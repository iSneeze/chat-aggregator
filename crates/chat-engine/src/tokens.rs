//! The tip services' tokens in the secret store (StreamElements' Overlay
//! Token, Streamlabs' Socket API Token): stored when a source is added,
//! deleted when the last source using one is removed. Never in
//! `config.toml`, which gets pasted into bug reports.

use tracing::warn;

use crate::{EngineHandle, SourceConfig, SourceId};

impl EngineHandle {
    /// Stores `secret` under `name`, replacing a stored one.
    pub(crate) async fn store_secret(&self, name: String, secret: String) -> anyhow::Result<()> {
        let secrets = self.secret_store()?;
        // On the engine's runtime: the store runs the keyring in tokio's
        // blocking threads, and the app calls this from a GPUI task.
        self.runtime
            .spawn(async move { secrets.save(&name, &secret).await })
            .await?
    }

    /// After source `removed` (with settings `config`) was removed: deletes
    /// the secret it used, unless another source still uses it. A failure is
    /// only logged: the source is gone either way, and adding it again
    /// replaces the secret.
    pub(crate) async fn forget_secrets_of(&self, removed: SourceId, config: &SourceConfig) {
        let Some(name) = config.secret_name() else {
            return;
        };
        // The status may not show the removal yet (it's published after the
        // actor's answer), hence the id check.
        let still_used = self.status.borrow().sources.iter().any(|source| {
            source.id != removed
                && source.config.as_ref().and_then(SourceConfig::secret_name) == Some(name.clone())
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
