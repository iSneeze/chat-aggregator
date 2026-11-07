//! StreamElements sources and their tokens. The token lives in the secret
//! store rather than `config.toml` (which gets pasted into bug reports),
//! under the account id, so adding a source is two steps: store the token,
//! then add the source.

use chat_streamelements::token_secret_name;

use crate::{EngineHandle, SetupError, SourceConfig, SourceId};

impl EngineHandle {
    /// Stores the Overlay Token for the StreamElements account `account`
    /// ("Account ID" on the dashboard), then adds a source for it and starts
    /// it. Adding the same account again replaces its stored token: that's
    /// how a streamer fixes a refused one.
    pub async fn add_streamelements_source(
        &self,
        account: &str,
        token: &str,
    ) -> anyhow::Result<SourceId> {
        let account = self.store_streamelements_token(account, token).await?;
        self.add_source(SourceConfig::StreamElements { account })
            .await
    }

    /// Stores the Overlay Token for `account`, replacing a stored one.
    /// Returns the account id as the source settings want it (trimmed,
    /// lowercase).
    pub async fn store_streamelements_token(
        &self,
        account: &str,
        token: &str,
    ) -> anyhow::Result<String> {
        let account = chat_streamelements::normalize_account(account).map_err(SetupError)?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(SetupError("the Overlay Token is empty".into()).into());
        }
        self.store_secret(token_secret_name(&account), token)
            .await?;
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use chat_core::secrets::SecretStore;

    use crate::{Engine, EngineConfig, SourceConfig};

    const ACCOUNT: &str = "0123456789abcdef01234567";

    #[tokio::test]
    async fn the_token_is_stored_and_removed_with_the_source() {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-se-source-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.secrets = Some(store.clone());
        let engine = Engine::start(config).await.unwrap();
        let handle = engine.handle();
        let name = chat_streamelements::token_secret_name(ACCOUNT);

        // Pasted with spaces and in capitals: stored under the clean id.
        let account = handle
            .store_streamelements_token(" 0123456789ABCDEF01234567 ", " t0ken\n")
            .await
            .unwrap();
        assert_eq!(account, ACCOUNT);
        assert_eq!(store.load(&name).await.unwrap().as_deref(), Some("t0ken"));

        // Stopped: a started source would try to reach StreamElements. Two
        // for the same account: the token stays until the last one goes.
        let config = SourceConfig::StreamElements { account };
        let first = handle.add_stopped_source(config.clone()).await.unwrap();
        let second = handle.add_stopped_source(config).await.unwrap();
        handle
            .status()
            .wait_for(|status| status.sources.len() == 2)
            .await
            .unwrap();

        handle.remove_source(first).await.unwrap();
        assert_eq!(store.load(&name).await.unwrap().as_deref(), Some("t0ken"));
        handle.remove_source(second).await.unwrap();
        assert_eq!(store.load(&name).await.unwrap(), None);

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bad_input_stores_nothing() {
        let dir = std::env::temp_dir().join(format!("chat-engine-se-bad-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.secrets = Some(store.clone());
        let engine = Engine::start(config).await.unwrap();
        let handle = engine.handle();

        // Refused before anything is stored or added (and so before any
        // connection).
        assert!(
            handle
                .add_streamelements_source("your_channel", "t0ken")
                .await
                .is_err()
        );
        assert!(
            handle
                .add_streamelements_source(ACCOUNT, "  ")
                .await
                .is_err()
        );
        let name = chat_streamelements::token_secret_name(ACCOUNT);
        assert_eq!(store.load(&name).await.unwrap(), None);
        assert!(handle.status().borrow().sources.is_empty());

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
