//! The YouTube login, through the engine: "connected as …", logging in
//! through the browser, logging out.
//!
//! The engine has what that needs: the client (`[youtube]` in its
//! settings) and a tokio runtime (the login runs a small local server for
//! Google's answer, and the keyring calls run on tokio's blocking threads).
//! So a window only says what it wants, from any executor, and the engine
//! restarts its YouTube sources itself after a login or logout.

use std::path::PathBuf;

use anyhow::anyhow;
use chat_youtube::oauth::{self, OAuthApp};
use tokio::sync::oneshot;
use tokio_util::task::AbortOnDropHandle;

use crate::EngineHandle;
use crate::actor::Command;
use crate::config::SetupError;
use crate::factory::is_login_required;

/// Who's logged in to YouTube.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YouTubeAccount {
    /// No client id and secret in the settings yet.
    NotSetUp,
    /// Nobody logged in, or Google rejected the stored login.
    LoggedOut,
    /// Logged in. `channel`: the account's channel name, `None` if it
    /// couldn't be fetched (offline, quota used up).
    LoggedIn { channel: Option<String> },
}

/// A login in progress: open [`url`](Self::url) in a browser, then await
/// [`finish`](Self::finish). Dropping it cancels the login: the engine's
/// login task stops, and with it the local server waiting for Google's
/// answer.
pub struct YouTubeLogin {
    url: String,
    /// Abort-on-drop: dropping this handle stops the task. That's all
    /// "cancel" takes.
    task: AbortOnDropHandle<anyhow::Result<YouTubeAccount>>,
}

impl YouTubeLogin {
    /// Google's consent page.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits until the login is done in the browser (at most 5 minutes).
    /// By then, YouTube sources that are on have restarted with it.
    pub async fn finish(self) -> anyhow::Result<YouTubeAccount> {
        self.task
            .await
            .map_err(|e| anyhow!("the login stopped: {e}"))?
    }
}

impl EngineHandle {
    /// Whose login is stored (asks YouTube for the channel name: 1 quota
    /// unit).
    pub async fn youtube_account(&self) -> anyhow::Result<YouTubeAccount> {
        let app = match self.youtube_client().await {
            Ok(app) => app,
            Err(e) if e.is::<SetupError>() => return Ok(YouTubeAccount::NotSetUp),
            Err(e) => return Err(e),
        };
        Ok(self
            .runtime
            .spawn(async move { account(&app).await })
            .await?)
    }

    /// Starts a login: Google's consent page in the browser, answered to a
    /// short-lived server on this computer (see `chat_youtube::oauth`).
    pub async fn youtube_login(&self) -> anyhow::Result<YouTubeLogin> {
        let app = self.youtube_client().await?;
        let (url_tx, url_rx) = oneshot::channel();
        let engine = self.clone();
        let task = AbortOnDropHandle::new(self.runtime.spawn(async move {
            let pending = oauth::begin_login(&app).await?;
            let _ = url_tx.send(pending.url().to_string());
            pending.complete().await?;
            // YouTube sources that are on pick up the new login right away.
            engine.youtube_login_changed().await?;
            Ok(account(&app).await)
        }));
        match url_rx.await {
            Ok(url) => Ok(YouTubeLogin { url, task }),
            // No URL: the login couldn't even start (e.g. no free port for
            // its server). The task says why.
            Err(_) => Err(match task.await {
                Ok(Err(e)) => e,
                Ok(Ok(_)) => anyhow!("the login ended before it started"),
                Err(e) => e.into(),
            }),
        }
    }

    /// Forgets the stored login. YouTube sources that are on restart and
    /// say they need a login.
    pub async fn youtube_logout(&self) -> anyhow::Result<()> {
        let app = self.youtube_client().await?;
        self.runtime
            .spawn(async move { oauth::logout(&app).await })
            .await??;
        self.youtube_login_changed().await
    }

    /// Uses the custom emoji export at `path` (it goes into the settings),
    /// and restarts the YouTube sources that are on, so they load it.
    /// Unlike setting `emojis` with `edit_settings`, this restarts them even
    /// when `path` is the file already set: a new export imported over the
    /// old one is the same setting, but new emoji.
    pub async fn use_youtube_emojis(&self, path: PathBuf) -> anyhow::Result<()> {
        self.call(|reply| Command::UseYouTubeEmojis(path, reply))
            .await
    }

    /// The stored login changed: YouTube sources that are on restart with
    /// it.
    pub(crate) async fn youtube_login_changed(&self) -> anyhow::Result<()> {
        self.call(Command::YouTubeLoginChanged).await
    }

    /// The OAuth client in the current settings (`SetupError` if there's
    /// none). Asked through the actor, in order with the other commands: a
    /// client saved a moment before (`edit_settings`) is the one used.
    async fn youtube_client(&self) -> anyhow::Result<OAuthApp> {
        self.call(Command::YouTubeClient).await
    }
}

/// Who `app`'s stored login belongs to.
async fn account(app: &OAuthApp) -> YouTubeAccount {
    match oauth::logged_in_channel(app).await {
        Ok(channel) => YouTubeAccount::LoggedIn {
            channel: Some(channel),
        },
        Err(e) if is_login_required(&e) => YouTubeAccount::LoggedOut,
        // A login is stored, but the name couldn't be fetched (offline,
        // quota): the sources will tell the rest.
        Err(e) => {
            tracing::debug!("couldn't fetch the YouTube channel name: {e:#}");
            YouTubeAccount::LoggedIn { channel: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use crate::{Engine, EngineConfig};

    async fn start() -> Engine {
        let mut config = EngineConfig::default();
        config.settings.server.port = 0; // the OS picks a free one
        Engine::start(config).await.unwrap()
    }

    /// Without a client, nothing is asked of Google or the keyring.
    #[tokio::test]
    async fn without_a_client_youtube_is_not_set_up() {
        let engine = start().await;
        let handle = engine.handle();
        assert_eq!(
            handle.youtube_account().await.unwrap(),
            YouTubeAccount::NotSetUp
        );
        let Err(e) = handle.youtube_login().await else {
            panic!("a login without a client");
        };
        assert!(e.to_string().contains("client id"), "{e}");
        assert!(handle.youtube_logout().await.is_err());
        engine.shutdown().await;
    }

    /// A login uses the client in the settings, even one saved a moment
    /// before. Dropping it (cancel) stops the local server that waits for
    /// Google's answer.
    #[tokio::test]
    async fn a_login_uses_the_current_client_and_stops_when_dropped() {
        let engine = start().await;
        let handle = engine.handle();
        // Not awaited: the login is asked for right after, and must see it.
        let _saved = handle.edit_settings(|s| {
            s.youtube.client_id = Some("new-client.apps.googleusercontent.com".into());
            s.youtube.client_secret = Some("test-secret".into());
        });
        let login = handle.youtube_login().await.unwrap();
        let url = reqwest::Url::parse(login.url()).unwrap();
        let query: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["client_id"], "new-client.apps.googleusercontent.com");

        // Google sends the browser back to a server on this computer...
        let redirect = reqwest::Url::parse(&query["redirect_uri"]).unwrap();
        let port = redirect.port().unwrap();
        let connect = || tokio::net::TcpStream::connect(("127.0.0.1", port));
        assert!(connect().await.is_ok(), "the login server waits");
        // ...which stops once the login is dropped.
        drop(login);
        tokio::time::timeout(Duration::from_secs(5), async {
            while connect().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the login server still runs after cancelling");
        engine.shutdown().await;
    }
}
