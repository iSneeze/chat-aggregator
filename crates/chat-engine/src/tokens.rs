//! The tip services' tokens in the secret store (StreamElements' Overlay
//! Token, Streamlabs' Socket API Token): stored when a source is added,
//! deleted when the last source using one is removed. Never in
//! `config.toml`, which gets pasted into bug reports. What differs between
//! the services (where a token is kept, what it's called) is in
//! `SourceConfig::token`; everything here works the same for both.

use chat_core::SetupError;
use tracing::warn;

use crate::{EngineHandle, SourceConfig, SourceId};

impl EngineHandle {
    /// Stores `token` for the source `config` (a tip service's), then adds
    /// that source and starts it. Adding a source of the same account again
    /// replaces its stored token: that's how a streamer fixes a refused one.
    pub async fn add_source_with_token(
        &self,
        config: SourceConfig,
        token: &str,
    ) -> anyhow::Result<SourceId> {
        self.store_token(&config, token).await?;
        self.add_source(config).await
    }

    /// Stores `token` for the source `config`, replacing a stored one,
    /// without adding the source (tests add it stopped, so it doesn't
    /// connect). Settings or a token that could never work are a
    /// `SetupError`, before anything is stored.
    pub async fn store_token(&self, config: &SourceConfig, token: &str) -> anyhow::Result<()> {
        config.validate().map_err(SetupError)?;
        let Some(source_token) = config.token() else {
            return Err(SetupError(format!("{} takes no token", config.label())).into());
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(SetupError(format!("the {} is empty", source_token.what)).into());
        }
        let secrets = self.secret_store()?;
        let name = source_token.secret_name;
        // On the engine's runtime: the store runs the keyring in tokio's
        // blocking threads, and the app calls this from a GPUI task.
        self.runtime
            .spawn(async move { secrets.save(&name, &token).await })
            .await?
    }

    /// After source `removed` (with settings `config`) was removed: deletes
    /// the token it used, unless another source still uses it. A failure is
    /// only logged: the source is gone either way, and adding it again
    /// replaces the token.
    pub(crate) async fn forget_secrets_of(&self, removed: SourceId, config: &SourceConfig) {
        let Some(name) = config.token().map(|token| token.secret_name) else {
            return;
        };
        // The status may not show the removal yet (it's published after the
        // actor's answer), hence the id check.
        let still_used = self.status.borrow().sources.iter().any(|source| {
            source.id != removed
                && (source.config.as_ref().and_then(SourceConfig::token))
                    .is_some_and(|token| token.secret_name == name)
        });
        if still_used {
            return;
        }
        let Ok(secrets) = self.secret_store() else {
            return;
        };
        let deleted = self
            .runtime
            .spawn(async move { secrets.delete(&name).await })
            .await;
        match deleted {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("couldn't delete a source's token: {e:#}"),
            Err(e) => warn!("couldn't delete a source's token: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chat_core::SetupError;
    use chat_core::secrets::SecretStore;

    use crate::{Engine, EngineConfig, SourceConfig};

    /// An engine that keeps its secrets in a folder of its own (`name`
    /// keeps tests apart), with that folder's store.
    async fn engine_with_secrets(name: &str) -> (Engine, SecretStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("chat-engine-{name}-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.secrets = Some(store.clone());
        (Engine::start(config).await.unwrap(), store, dir)
    }

    #[tokio::test]
    async fn a_token_stays_while_a_source_uses_it() {
        let (engine, store, dir) = engine_with_secrets("tokens").await;
        let handle = engine.handle();
        let stored = |name: &'static str| {
            let store = store.clone();
            async move { store.load(name).await.unwrap() }
        };
        // The names are spelled out: tokens stored under them must keep
        // working.
        const SE: &str = "streamelements:0123456789abcdef01234567";
        const CORPO: &str = "streamlabs:Corpo";
        const INDIE: &str = "streamlabs:Indie";

        // Pasted with spaces and in capitals: stored under the clean id.
        let se = SourceConfig::StreamElements {
            account: " 0123456789ABCDEF01234567 ".into(),
        };
        handle.store_token(&se, " t0ken\n").await.unwrap();
        assert_eq!(stored(SE).await.as_deref(), Some("t0ken"));
        // Two personas, two Streamlabs accounts: a token for each.
        let corpo = SourceConfig::Streamlabs {
            label: "Corpo".into(),
        };
        let indie = SourceConfig::Streamlabs {
            label: "Indie".into(),
        };
        handle.store_token(&corpo, "t0ken-corpo").await.unwrap();
        handle.store_token(&indie, "t0ken-indie").await.unwrap();

        // Stopped: a started source would try to reach the service. The
        // StreamElements account twice: one token for both.
        let mut ids = Vec::new();
        for config in [se.clone(), se, corpo, indie] {
            ids.push(handle.add_stopped_source(config).await.unwrap());
        }
        handle
            .status()
            .wait_for(|status| status.sources.len() == 4)
            .await
            .unwrap();

        handle.remove_source(ids[0]).await.unwrap();
        assert_eq!(stored(SE).await.as_deref(), Some("t0ken"), "still used");
        // Removing one persona leaves the other's token alone.
        handle.remove_source(ids[2]).await.unwrap();
        assert_eq!(stored(CORPO).await, None);
        assert_eq!(stored(INDIE).await.as_deref(), Some("t0ken-indie"));
        handle.remove_source(ids[1]).await.unwrap();
        assert_eq!(stored(SE).await, None, "the last one is gone");

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bad_input_stores_and_adds_nothing() {
        let (engine, _store, dir) = engine_with_secrets("tokens-bad").await;
        let handle = engine.handle();

        let streamlabs = SourceConfig::Streamlabs {
            label: "Main".into(),
        };
        for (config, token) in [
            // Not an Account ID.
            (
                SourceConfig::StreamElements {
                    account: "your_channel".into(),
                },
                "t0ken",
            ),
            (streamlabs, "  "),
            // A source without a token.
            (
                SourceConfig::Twitch {
                    channel: "your_channel".into(),
                },
                "t0ken",
            ),
        ] {
            let refused = handle
                .add_source_with_token(config.clone(), token)
                .await
                .unwrap_err();
            // Shown to the user, never retried.
            assert!(refused.is::<SetupError>(), "{config:?}: {refused:#}");
        }
        // Refused before anything was stored or added (and so before any
        // connection).
        let empty = std::fs::read_dir(&dir).map_or(true, |mut files| files.next().is_none());
        assert!(empty, "nothing stored");
        assert!(handle.status().borrow().sources.is_empty());

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
