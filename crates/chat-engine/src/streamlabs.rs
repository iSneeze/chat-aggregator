//! Streamlabs sources and their tokens. Each source has a label the
//! streamer picks ("Main", "Corpo"), because the Socket API Token doesn't
//! say whose account it is; the token lives in the secret store under that
//! label (see `tokens.rs`).

use chat_streamlabs::token_secret_name;

use crate::{EngineHandle, SetupError, SourceConfig, SourceId};

impl EngineHandle {
    /// Stores the Socket API Token for the Streamlabs account named
    /// `label`, then adds a source for it and starts it. Adding the same
    /// label again replaces its stored token: that's how a streamer fixes a
    /// refused one.
    pub async fn add_streamlabs_source(
        &self,
        label: &str,
        token: &str,
    ) -> anyhow::Result<SourceId> {
        let label = self.store_streamlabs_token(label, token).await?;
        self.add_source(SourceConfig::Streamlabs { label }).await
    }

    /// Stores the Socket API Token for `label`, replacing a stored one.
    /// Returns the label as the source settings want it (trimmed).
    pub async fn store_streamlabs_token(&self, label: &str, token: &str) -> anyhow::Result<String> {
        let label = chat_streamlabs::normalize_label(label).map_err(SetupError)?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(SetupError("the Socket API Token is empty".into()).into());
        }
        self.store_secret(token_secret_name(&label), token).await?;
        Ok(label)
    }
}

#[cfg(test)]
mod tests {
    use chat_core::secrets::SecretStore;

    use crate::{Engine, EngineConfig, SourceConfig};

    #[tokio::test]
    async fn each_label_keeps_its_own_token() {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-sl-source-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.secrets = Some(store.clone());
        let engine = Engine::start(config).await.unwrap();
        let handle = engine.handle();
        let token_of = |label: &str| {
            let (store, name) = (store.clone(), chat_streamlabs::token_secret_name(label));
            async move { store.load(&name).await.unwrap() }
        };

        // Two personas, two accounts. Pasted with spaces: stored trimmed.
        let corpo = handle
            .store_streamlabs_token(" Corpo ", " t0ken-corpo\n")
            .await
            .unwrap();
        assert_eq!(corpo, "Corpo");
        let indie = handle
            .store_streamlabs_token("Indie", "t0ken-indie")
            .await
            .unwrap();
        assert_eq!(token_of("Corpo").await.as_deref(), Some("t0ken-corpo"));
        assert_eq!(token_of("Indie").await.as_deref(), Some("t0ken-indie"));

        // Stopped: a started source would try to reach Streamlabs.
        let first = handle
            .add_stopped_source(SourceConfig::Streamlabs { label: corpo })
            .await
            .unwrap();
        let second = handle
            .add_stopped_source(SourceConfig::Streamlabs { label: indie })
            .await
            .unwrap();
        handle
            .status()
            .wait_for(|status| status.sources.len() == 2)
            .await
            .unwrap();

        // Removing one persona leaves the other's token alone.
        handle.remove_source(first).await.unwrap();
        assert_eq!(token_of("Corpo").await, None);
        assert_eq!(token_of("Indie").await.as_deref(), Some("t0ken-indie"));
        handle.remove_source(second).await.unwrap();
        assert_eq!(token_of("Indie").await, None);

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bad_input_stores_nothing() {
        let dir = std::env::temp_dir().join(format!("chat-engine-sl-bad-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.secrets = Some(store.clone());
        let engine = Engine::start(config).await.unwrap();
        let handle = engine.handle();

        // Refused before anything is stored or added (and so before any
        // connection).
        assert!(handle.add_streamlabs_source("  ", "t0ken").await.is_err());
        assert!(handle.add_streamlabs_source("Main", "  ").await.is_err());
        let name = chat_streamlabs::token_secret_name("Main");
        assert_eq!(store.load(&name).await.unwrap(), None);
        assert!(handle.status().borrow().sources.is_empty());

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
