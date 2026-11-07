//! What the app asks of the chat reactions besides their settings (which
//! go through `edit_settings` like all others): testing a rule, and the OBS
//! password, which lives in the secret store rather than `config.toml`.

use anyhow::anyhow;
use chat_reactions::{OBS_PASSWORD, ReactionRequest, Target};
use tokio::sync::oneshot;

use crate::EngineHandle;

impl EngineHandle {
    /// Carries out the saved rule `rule`'s action right now: no trigger, no
    /// guards, also when the rule is switched off or reactions are paused.
    /// Only while its program is connected; the error says why not, or what
    /// the program answered (OBS: a filter that doesn't exist), in words
    /// for the streamer.
    pub async fn test_reaction(&self, rule: &str) -> anyhow::Result<()> {
        let (reply, answer) = oneshot::channel();
        let request = ReactionRequest::Test {
            rule: rule.to_string(),
            reply,
        };
        self.reactions
            .send(request)
            .await
            .map_err(|_| anyhow!("the engine has stopped"))?;
        answer
            .await
            .map_err(|_| anyhow!("the engine has stopped"))?
            .map_err(|why| anyhow!(why))
    }

    /// Stores the OBS WebSocket password (`None`: removes it), and makes
    /// OBS's output, if it's waiting to try again, try right away.
    pub async fn set_obs_password(&self, password: Option<String>) -> anyhow::Result<()> {
        let secrets = self.secret_store()?;
        // On the engine's runtime: the store runs the keyring in tokio's
        // blocking threads, and the app calls this from a GPUI task.
        self.runtime
            .spawn(async move {
                match password {
                    Some(password) => secrets.save(OBS_PASSWORD, &password).await,
                    None => secrets.delete(OBS_PASSWORD).await,
                }
            })
            .await??;
        // Only a nudge: if it doesn't arrive (shutting down), nothing is lost.
        let _ = self.reactions.try_send(ReactionRequest::Retry(Target::Obs));
        Ok(())
    }

    /// Whether an OBS password is stored. (Reads it: a locked keyring may
    /// ask to be unlocked.)
    pub async fn obs_password_stored(&self) -> anyhow::Result<bool> {
        let secrets = self.secret_store()?;
        let stored = self
            .runtime
            .spawn(async move { secrets.load(OBS_PASSWORD).await })
            .await??;
        Ok(stored.is_some())
    }

    pub(crate) fn secret_store(&self) -> anyhow::Result<chat_core::secrets::SecretStore> {
        self.secrets
            .clone()
            .ok_or_else(|| anyhow!("this engine keeps no secrets"))
    }
}

#[cfg(test)]
mod tests {
    use chat_core::secrets::SecretStore;

    use crate::{Engine, EngineConfig};

    #[tokio::test]
    async fn the_obs_password_is_stored_and_removed() {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-obs-password-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.secrets = Some(store.clone());
        let engine = Engine::start(config).await.unwrap();
        let handle = engine.handle();

        assert!(!handle.obs_password_stored().await.unwrap());
        handle
            .set_obs_password(Some("hunter2".into()))
            .await
            .unwrap();
        assert!(handle.obs_password_stored().await.unwrap());
        // Where the OBS output reads it from.
        let stored = store.load(crate::OBS_PASSWORD).await.unwrap();
        assert_eq!(stored.as_deref(), Some("hunter2"));
        handle.set_obs_password(None).await.unwrap();
        assert!(!handle.obs_password_stored().await.unwrap());

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn without_a_store_there_is_no_password() {
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        let engine = Engine::start(config).await.unwrap();
        assert!(engine.handle().obs_password_stored().await.is_err());
        engine.shutdown().await;
    }
}
