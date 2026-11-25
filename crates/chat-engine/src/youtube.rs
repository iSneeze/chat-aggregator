//! The YouTube logins, through the engine: which channels are connected,
//! connecting one through the browser, logging one out.
//!
//! The engine has what that needs: the client (`[youtube]` in its
//! settings), the secret store and a tokio runtime (the login runs a small
//! local server for Google's answer, and the keyring calls run on tokio's
//! blocking threads). So a window only says what it wants, from any
//! executor, and the engine restarts its YouTube sources itself after a
//! login or logout.
//!
//! A login belongs to one channel, so a streamer with several channels
//! connects each; the settings list them (`YouTubeSettings::accounts`,
//! since the keyring can't list what's in it), and each YouTube source
//! says which one it uses (`SourceConfig::YouTube::account`).

use std::path::PathBuf;

use anyhow::anyhow;
use chat_youtube::oauth::{self, Account, OAuthApp};
use tokio::sync::oneshot;
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, info, warn};

use crate::EngineHandle;
use crate::actor::Command;
use crate::config::{YouTubeChannel, YouTubeSettings};
use crate::factory::is_login_required;
use chat_core::SetupError;

/// The connected YouTube channels, from [`EngineHandle::youtube_accounts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YouTubeAccounts {
    /// No client id and secret in the settings yet.
    NotSetUp,
    /// The client is set up; these channels are connected, in the order
    /// they were (none yet: empty).
    SetUp(Vec<YouTubeAccount>),
}

/// A connected YouTube channel, and whether its login works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YouTubeAccount {
    /// `None`: the login of a version before several channels could be
    /// connected, whose channel couldn't be looked up yet (offline, quota
    /// used up). It works all the same; the next check moves it to its
    /// channel.
    pub channel: Option<YouTubeChannel>,
    /// `false`: Google rejected its login (revoked, or expired) or it's
    /// gone from the keyring: connect it again. `true` also when it
    /// couldn't be checked (offline): the sources will tell.
    pub logged_in: bool,
}

/// A login in progress: open [`url`](Self::url) in a browser, then await
/// [`finish`](Self::finish). Dropping it cancels the login: the engine's
/// login task stops, and with it the local server waiting for Google's
/// answer.
pub struct YouTubeLogin {
    url: String,
    /// Abort-on-drop: dropping this handle stops the task. That's all
    /// "cancel" takes.
    task: AbortOnDropHandle<anyhow::Result<YouTubeChannel>>,
}

impl YouTubeLogin {
    /// Google's consent page.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits until the login is done in the browser (at most 5 minutes),
    /// and says which channel was connected. By then it's in the settings,
    /// and YouTube sources that are on have restarted.
    pub async fn finish(self) -> anyhow::Result<YouTubeChannel> {
        self.task
            .await
            .map_err(|e| anyhow!("the login stopped: {e}"))?
    }
}

impl EngineHandle {
    /// The connected channels, each with whether its login works (asks
    /// Google for an access token: no quota).
    ///
    /// Also moves the login of older versions, which had a single one, to
    /// its channel (1 quota unit, once): the channel goes into the
    /// settings, and only once they're saved is the old entry deleted.
    /// Until then it keeps working, so nobody has to log in again.
    pub async fn youtube_accounts(&self) -> anyhow::Result<YouTubeAccounts> {
        let app = match self.youtube_client().await {
            Ok(app) => app,
            Err(e) if e.is::<SetupError>() => return Ok(YouTubeAccounts::NotSetUp),
            Err(e) => return Err(e),
        };
        let engine = self.clone();
        self.runtime
            .spawn(async move { engine.check_accounts(app).await })
            .await?
    }

    /// Starts a login: Google's consent page in the browser, answered to a
    /// short-lived server on this computer (see `chat_youtube::oauth`).
    /// Google asks which account and channel: a new one is added to the
    /// connected ones, one connected already gets the new login.
    pub async fn youtube_login(&self) -> anyhow::Result<YouTubeLogin> {
        let app = self.youtube_client().await?;
        let (url_tx, url_rx) = oneshot::channel();
        let engine = self.clone();
        let task = AbortOnDropHandle::new(self.runtime.spawn(async move {
            let pending = oauth::begin_login(&app).await?;
            let _ = url_tx.send(pending.url().to_string());
            let channel = pending.complete().await?;
            let channel = YouTubeChannel {
                channel_id: channel.id,
                name: channel.title,
            };
            // YouTube sources that are on pick up the new login right away.
            let connected = channel.clone();
            engine
                .youtube_logins_changed(move |youtube| youtube.connect(connected))
                .await?;
            Ok(channel)
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

    /// Forgets `account`'s login and takes it off the connected channels.
    /// YouTube sources that are on restart, and those that used it say
    /// they need a login.
    pub async fn youtube_logout(&self, account: &YouTubeAccount) -> anyhow::Result<()> {
        let app = self.youtube_client().await?;
        let id = account.channel.as_ref().map(|c| c.channel_id.clone());
        let stored = id.clone().map_or(Account::Legacy, Account::Channel);
        self.runtime
            .spawn(async move { oauth::logout(&app, &stored).await })
            .await??;
        self.youtube_logins_changed(move |youtube| {
            if let Some(id) = id {
                youtube.accounts.retain(|a| a.channel_id != id);
            }
        })
        .await
        .map(drop)
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

    /// The stored logins changed (one made, moved or removed): `change`
    /// updates the connected channels to match, and YouTube sources that
    /// are on restart, even if the list stays the same (a channel logged
    /// in again). Answers whether the list is saved: headless mode and
    /// tests never save.
    pub(crate) async fn youtube_logins_changed(
        &self,
        change: impl FnOnce(&mut YouTubeSettings) + Send + 'static,
    ) -> anyhow::Result<bool> {
        self.call(|reply| {
            Command::YouTubeLoginsChanged(
                Box::new(move |settings| change(&mut settings.youtube)),
                reply,
            )
        })
        .await
    }

    /// The OAuth client in the current settings (`SetupError` if there's
    /// none), with the engine's secret store. Asked through the actor, in
    /// order with the other commands: a client saved a moment before
    /// (`edit_settings`) is the one used.
    async fn youtube_client(&self) -> anyhow::Result<OAuthApp> {
        let app = self.call(Command::YouTubeClient).await?;
        Ok(app.with_store(self.secret_store()?))
    }

    /// See [`youtube_accounts`](Self::youtube_accounts); runs on the
    /// engine's runtime.
    async fn check_accounts(&self, app: OAuthApp) -> anyhow::Result<YouTubeAccounts> {
        let legacy_left = self.move_legacy_login(&app).await;
        let listed = self.settings.borrow().youtube.accounts.clone();
        let mut accounts = Vec::with_capacity(listed.len() + 1);
        for channel in listed {
            let account = Account::Channel(channel.channel_id.clone());
            let logged_in = match oauth::check_login(&app, account).await {
                Ok(()) => true,
                Err(e) if is_login_required(&e) => false,
                Err(e) => {
                    debug!("couldn't check a YouTube login: {e:#}");
                    true
                }
            };
            accounts.push(YouTubeAccount {
                channel: Some(channel),
                logged_in,
            });
        }
        if legacy_left {
            accounts.push(YouTubeAccount {
                channel: None,
                logged_in: true,
            });
        }
        Ok(YouTubeAccounts::SetUp(accounts))
    }

    /// Moves older versions' single login to its channel, if there is one.
    /// The order keeps it safe: a copy under the channel's name first
    /// (`adopt_legacy_login`), then the channel into the settings, and only
    /// once those are saved, the old entry goes. Whatever fails on the way,
    /// the old entry is still there, and the next check tries again.
    /// Answers whether it's still there, not moved yet.
    async fn move_legacy_login(&self, app: &OAuthApp) -> bool {
        match oauth::legacy_login_stored(app).await {
            Ok(true) => {}
            Ok(false) => return false,
            // A keyring that can't be read: the connected channels' checks
            // run into it too, and say so there.
            Err(e) => {
                debug!("couldn't look for an older YouTube login: {e:#}");
                return false;
            }
        }
        let channel = match oauth::adopt_legacy_login(app).await {
            Ok(Some(channel)) => channel,
            Ok(None) => return false, // deleted meanwhile
            Err(e) if is_login_required(&e) => {
                // Rejected by Google (revoked, expired): it can't work
                // again, and keeping it would only mean asking every time.
                info!("the older YouTube login was rejected by Google; removing it");
                if let Err(e) = oauth::logout(app, &Account::Legacy).await {
                    warn!("couldn't remove the older YouTube login: {e:#}");
                }
                return false;
            }
            Err(e) => {
                debug!("couldn't move the older YouTube login yet: {e:#}");
                return true;
            }
        };
        let connected = YouTubeChannel {
            channel_id: channel.id,
            name: channel.title,
        };
        match self
            .youtube_logins_changed(move |youtube| youtube.connect(connected))
            .await
        {
            Ok(true) => {
                info!("moved the YouTube login to its channel");
                // A failure here leaves a copy behind, nothing worse: the
                // next check moves it again (same channel) and retries.
                if let Err(e) = oauth::logout(app, &Account::Legacy).await {
                    warn!("couldn't remove the older YouTube login after moving it: {e:#}");
                }
            }
            // Listed for now, not saved: the old entry stays for next time.
            Ok(false) => debug!("YouTube login copied to its channel; the settings aren't saved"),
            Err(e) => debug!("couldn't list the moved YouTube login: {e:#}"),
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::Duration;

    use chat_core::secrets::SecretStore;
    use chat_youtube::oauth::fake_google::{self, CHANNEL_ID, CHANNEL_TITLE, REVOKED};

    use crate::{Engine, EngineConfig};

    /// A fresh, empty folder for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-youtube-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// An engine keeping its secrets in `dir`, saving its settings to
    /// `config.toml` in it if `saves`.
    async fn start(dir: &Path, saves: bool) -> Engine {
        let mut config = EngineConfig::default();
        config.settings.server.port = 0; // the OS picks a free one
        config.secrets = Some(SecretStore::Dir(dir.join("secrets")));
        config.save_to = saves.then(|| dir.join("config.toml"));
        Engine::start(config).await.unwrap()
    }

    fn your_channel() -> YouTubeChannel {
        YouTubeChannel {
            channel_id: CHANNEL_ID.into(),
            name: CHANNEL_TITLE.into(),
        }
    }

    /// A login uses the client in the settings, even one saved a moment
    /// before. Dropping it (cancel) stops the local server that waits for
    /// Google's answer.
    #[tokio::test]
    async fn a_login_uses_the_current_client_and_stops_when_dropped() {
        let dir = temp_dir("login");
        let engine = start(&dir, false).await;
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

    /// The single login of older versions moves to its channel: the
    /// channel into the settings, and once they're saved, the old entry
    /// goes. Nobody logs in again.
    #[tokio::test]
    async fn the_old_login_moves_to_its_channel_once_the_list_is_saved() {
        let (app, _, google_dir) = fake_google::start(3600).await;
        fake_google::save_login(&app, &Account::Legacy, "refresh-old").await;
        let dir = temp_dir("move-saved");
        let engine = start(&dir, true).await;
        let handle = engine.handle();

        let accounts = handle.check_accounts(app.clone()).await.unwrap();
        assert_eq!(
            accounts,
            YouTubeAccounts::SetUp(vec![YouTubeAccount {
                channel: Some(your_channel()),
                logged_in: true,
            }])
        );
        assert_eq!(
            handle.settings().borrow().youtube.accounts,
            [your_channel()]
        );
        let saved = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(saved.contains(CHANNEL_ID), "{saved}");
        let channel = Account::Channel(CHANNEL_ID.into());
        assert_eq!(
            fake_google::stored_login(&app, &channel).await.as_deref(),
            Some("refresh-old")
        );
        assert_eq!(
            fake_google::stored_login(&app, &Account::Legacy).await,
            None
        );

        // The next check finds nothing left to move.
        let again = handle.check_accounts(app).await.unwrap();
        assert_eq!(again, accounts);
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(google_dir);
    }

    /// Without a saved list (headless mode never saves; a failed save), the
    /// old entry stays: next time it's the way to the login again.
    #[tokio::test]
    async fn the_old_login_stays_while_the_list_isnt_saved() {
        let (app, _, google_dir) = fake_google::start(3600).await;
        fake_google::save_login(&app, &Account::Legacy, "refresh-old").await;
        let dir = temp_dir("move-unsaved");
        let engine = start(&dir, false).await;
        let handle = engine.handle();

        let accounts = handle.check_accounts(app.clone()).await.unwrap();
        assert_eq!(
            accounts,
            YouTubeAccounts::SetUp(vec![YouTubeAccount {
                channel: Some(your_channel()),
                logged_in: true,
            }]),
            "listed for now"
        );
        assert_eq!(
            fake_google::stored_login(&app, &Account::Legacy)
                .await
                .as_deref(),
            Some("refresh-old")
        );
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(google_dir);
    }

    /// An old login Google rejects can never work again: it's removed, not
    /// listed.
    #[tokio::test]
    async fn a_rejected_old_login_is_removed() {
        let (app, _, google_dir) = fake_google::start(3600).await;
        fake_google::save_login(&app, &Account::Legacy, REVOKED).await;
        let dir = temp_dir("move-rejected");
        let engine = start(&dir, true).await;
        let handle = engine.handle();

        let accounts = handle.check_accounts(app.clone()).await.unwrap();
        assert_eq!(accounts, YouTubeAccounts::SetUp(Vec::new()));
        assert_eq!(
            fake_google::stored_login(&app, &Account::Legacy).await,
            None
        );
        assert!(handle.settings().borrow().youtube.accounts.is_empty());
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(google_dir);
    }

    /// Each connected channel is checked on its own; one whose login is
    /// gone or rejected says so, the others stay connected.
    #[tokio::test]
    async fn each_channel_is_checked_on_its_own() {
        let (app, _, google_dir) = fake_google::start(3600).await;
        let dir = temp_dir("check");
        let engine = start(&dir, false).await;
        let handle = engine.handle();
        let channel = |id: &str| YouTubeChannel {
            channel_id: id.into(),
            name: format!("name of {id}"),
        };
        handle
            .edit_settings(move |s| {
                for id in ["UC_main", "UC_second", "UC_third"] {
                    s.youtube.connect(channel(id));
                }
            })
            .await
            .unwrap();
        fake_google::save_login(&app, &Account::Channel("UC_main".into()), "refresh-1").await;
        fake_google::save_login(&app, &Account::Channel("UC_third".into()), REVOKED).await;

        let YouTubeAccounts::SetUp(accounts) = handle.check_accounts(app).await.unwrap() else {
            panic!("set up");
        };
        let found: Vec<_> = accounts
            .iter()
            .map(|a| (a.channel.as_ref().unwrap().channel_id.as_str(), a.logged_in))
            .collect();
        assert_eq!(
            found,
            [("UC_main", true), ("UC_second", false), ("UC_third", false)]
        );
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(google_dir);
    }

    /// Logging out forgets that channel's login and takes it off the list;
    /// the other channel's login stays.
    #[tokio::test]
    async fn logging_out_removes_one_channel() {
        let dir = temp_dir("logout");
        let engine = start(&dir, false).await;
        let handle = engine.handle();
        let second = YouTubeChannel {
            channel_id: "UC_second".into(),
            name: "your_second_channel".into(),
        };
        let both = second.clone();
        handle
            .edit_settings(move |s| {
                s.youtube.client_id = Some("test-client.apps.googleusercontent.com".into());
                s.youtube.client_secret = Some("test-secret".into());
                s.youtube.connect(your_channel());
                s.youtube.connect(both);
            })
            .await
            .unwrap();
        let store = SecretStore::Dir(dir.join("secrets"));
        let name = |id: &str| format!("youtube:test-client.apps.googleusercontent.com:{id}");
        store.save(&name(CHANNEL_ID), "refresh-1").await.unwrap();
        store.save(&name("UC_second"), "refresh-2").await.unwrap();

        let account = YouTubeAccount {
            channel: Some(second),
            logged_in: true,
        };
        handle.youtube_logout(&account).await.unwrap();
        assert_eq!(
            handle.settings().borrow().youtube.accounts,
            [your_channel()]
        );
        assert_eq!(store.load(&name("UC_second")).await.unwrap(), None);
        assert_eq!(
            store.load(&name(CHANNEL_ID)).await.unwrap().as_deref(),
            Some("refresh-1")
        );
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
