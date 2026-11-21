//! Turning a `SourceConfig` into a runnable source.
//!
//! The actor is generic over a [`SourceFactory`], so tests can hand it
//! scripted sources ("fails twice, then runs") while production builds the
//! real Twitch/YouTube/Picarto/RPlay/StreamElements/Streamlabs/demo sources. This is static dispatch
//! (generics), not a trait object: the compiler generates one actor per factory type.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use chat_core::demo::DemoSource;
use chat_core::secrets::SecretStore;
use chat_core::tiers::TipSettings;
use chat_core::{ChatEvent, ChatSource, Reporter, SetupError};
use chat_picarto::PicartoSource;
use chat_rplay::RplaySource;
use chat_streamelements::StreamElementsSource;
use chat_streamlabs::StreamlabsSource;
use chat_twitch::TwitchSource;
use chat_youtube::oauth::{Account, LoginRequired, TokenProvider};
use chat_youtube::{Auth, EmojiMap, YouTubeSource, YouTubeTarget};
use tokio::sync::{mpsc, watch};

use crate::config::{SourceConfig, YouTubeSettings};

pub(crate) trait SourceFactory: Send + Sync + 'static {
    type Source: ChatSource + Send + 'static;

    /// Builds a source. Errors that only the user can fix should be (or
    /// wrap) a `SetupError` or `LoginRequired`, see [`needs_attention`].
    fn build(
        &self,
        config: &SourceConfig,
    ) -> impl Future<Output = anyhow::Result<Self::Source>> + Send;

    /// A source needed a login that isn't valid (anymore). Forget the cached
    /// ones, so the next build picks up a fresh login.
    fn forget_login(&self) {}

    /// New YouTube settings (client, API key, emojis) for future builds.
    fn set_youtube(&self, _settings: YouTubeSettings) {}
}

/// Errors not worth retrying automatically: the user has to act. Every
/// source says so with chat-core's `SetupError`; YouTube's `LoginRequired`
/// is its own type because the app answers it differently (with the login
/// button).
pub(crate) fn needs_attention(err: &anyhow::Error) -> bool {
    // `chain()` walks the error and everything it wraps, so this also finds
    // a `LoginRequired` that got extra context attached on the way up.
    err.chain()
        .any(|e| e.is::<SetupError>() || e.is::<LoginRequired>())
}

pub(crate) fn is_login_required(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<LoginRequired>())
}

/// A built, runnable source of any kind.
pub(crate) enum SourceSpec {
    Twitch(TwitchSource),
    YouTube(YouTubeSource),
    Picarto(PicartoSource),
    Rplay(RplaySource),
    StreamElements(StreamElementsSource),
    Streamlabs(StreamlabsSource),
    Demo(DemoSource),
}

impl ChatSource for SourceSpec {
    /// Runs whichever source this is. No `dyn` needed: the `match` picks the
    /// concrete source, and this async fn's future simply has room for any
    /// of them.
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        match self {
            SourceSpec::Twitch(s) => s.run(tx, activity).await,
            SourceSpec::YouTube(s) => s.run(tx, activity).await,
            SourceSpec::Picarto(s) => s.run(tx, activity).await,
            SourceSpec::Rplay(s) => s.run(tx, activity).await,
            SourceSpec::StreamElements(s) => s.run(tx, activity).await,
            SourceSpec::Streamlabs(s) => s.run(tx, activity).await,
            SourceSpec::Demo(s) => s.run(tx, activity).await,
        }
    }
}

struct LoginCache {
    /// Generation at which these providers were loaded. When `login_epoch`
    /// advances (`forget_login`), stale entries are discarded even if a
    /// `login()` call was mid-flight while `forget_login` ran.
    epoch: u64,
    providers: HashMap<Account, TokenProvider>,
}

pub(crate) struct Production {
    // Changed at runtime by `set_youtube`, read by every build. A std Mutex:
    // only ever held for a quick clone, never across an `.await`.
    youtube: std::sync::Mutex<YouTubeSettings>,
    /// Bumped by `forget_login` (lock-free), so an invalidation is never lost
    /// even if a build is loading from the keyring at the same moment.
    login_epoch: AtomicU64,
    /// One login per account, shared by all YouTube sources that use it:
    /// one cached access token, refreshed once, instead of one per source.
    logins: tokio::sync::Mutex<LoginCache>,
    /// See `EngineConfig::emoji_cache_dir`.
    emoji_cache: Option<PathBuf>,
    /// See `EngineConfig::secrets`: the YouTube logins, the tip services'
    /// tokens.
    secrets: Option<SecretStore>,
    /// The tip settings, watched: every tip source gets a copy of this
    /// receiver, so a change reaches the running ones without a reconnect.
    /// The same channel the reactor's amount rules read (`Engine::start`).
    tips: watch::Receiver<TipSettings>,
    /// RPlay test builds: the settings folder, where RPlay sources write
    /// their raw chat capture. `None` without one (headless mode).
    #[cfg(feature = "rplay-capture")]
    rplay_capture: Option<PathBuf>,
}

impl Production {
    pub(crate) fn new(
        youtube: YouTubeSettings,
        emoji_cache: Option<PathBuf>,
        secrets: Option<SecretStore>,
        tips: watch::Receiver<TipSettings>,
    ) -> Self {
        Self {
            youtube: std::sync::Mutex::new(youtube),
            login_epoch: AtomicU64::new(0),
            logins: tokio::sync::Mutex::new(LoginCache {
                epoch: 0,
                providers: HashMap::new(),
            }),
            emoji_cache,
            secrets,
            tips,
            #[cfg(feature = "rplay-capture")]
            rplay_capture: None,
        }
    }

    /// RPlay test builds: RPlay sources capture their raw chat into `dir`.
    #[cfg(feature = "rplay-capture")]
    pub(crate) fn capturing_rplay(mut self, dir: Option<PathBuf>) -> Self {
        self.rplay_capture = dir;
        self
    }

    fn youtube(&self) -> YouTubeSettings {
        self.youtube
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The token of a source that has one (a tip service's, RPlay's
    /// toolkey), from the secret store. Read at every (re)start: a token
    /// stored again since (the source added anew) is picked up by "Start".
    async fn stored_token(&self, config: &SourceConfig) -> anyhow::Result<String> {
        let Some(token) = config.token() else {
            // Settings that don't pass `validate`: the source's own check
            // says what's wrong (or the caller asked for a token by mistake).
            return Err(SetupError(format!("{} takes no token", config.label())).into());
        };
        let stored = match &self.secrets {
            Some(secrets) => secrets
                .load(&token.secret_name)
                .await
                .with_context(|| format!("reading the {}", token.what))?,
            None => None,
        };
        stored.ok_or_else(|| {
            SetupError(format!(
                "no {} is stored for this source: remove it and add it again with the {} \
                 (headless: run {})",
                token.what, token.what, token.headless_flag
            ))
            .into()
        })
    }

    /// The login of a YouTube source with `account`: that channel's, or
    /// without one, the first connected. Before any is listed, the single
    /// login of older versions (until the app has moved it, see
    /// `chat_engine::youtube`; headless mode never does).
    async fn login(&self, account: Option<&str>) -> anyhow::Result<TokenProvider> {
        let epoch = self.login_epoch.load(Ordering::Acquire);
        let settings = self.youtube();
        let account = match settings.account_for(account) {
            Some(listed) => Account::Channel(listed.channel_id.clone()),
            // Not listed: its login may still be stored (a list edited by
            // hand); if not, that's what the error says.
            None => account.map_or(Account::Legacy, |id| Account::Channel(id.into())),
        };
        {
            let mut cached = self.logins.lock().await;
            if cached.epoch != epoch {
                cached.providers.clear();
                cached.epoch = epoch;
            }
            if let Some(provider) = cached.providers.get(&account) {
                return Ok(provider.clone());
            }
        }
        // The missing client first: that's what the streamer can fix.
        let app = settings.oauth_app()?;
        let Some(secrets) = &self.secrets else {
            return Err(
                SetupError("this engine keeps no secrets, so no YouTube login".into()).into(),
            );
        };
        let app = app.with_store(secrets.clone());
        let provider = TokenProvider::from_store(app, account.clone()).await?;
        let mut cached = self.logins.lock().await;
        // Only cache if `forget_login` didn't advance the epoch while we were
        // loading from the keyring.
        let current = self.login_epoch.load(Ordering::Acquire);
        if current == epoch {
            if cached.epoch != epoch {
                cached.providers.clear();
                cached.epoch = epoch;
            }
            cached.providers.insert(account, provider.clone());
        }
        Ok(provider)
    }
}

impl SourceFactory for Production {
    type Source = SourceSpec;

    async fn build(&self, config: &SourceConfig) -> anyhow::Result<SourceSpec> {
        Ok(match config {
            SourceConfig::Twitch { channel } => SourceSpec::Twitch(TwitchSource {
                channel: chat_twitch::normalize_channel(channel).map_err(SetupError)?,
            }),
            SourceConfig::Picarto { channel } => SourceSpec::Picarto(PicartoSource {
                channel: chat_picarto::normalize_channel(channel).map_err(SetupError)?,
            }),
            SourceConfig::Rplay { label } => {
                let source = RplaySource::new(
                    chat_rplay::normalize_label(label).map_err(SetupError)?,
                    // Stored as the bare key; read through the same check,
                    // so a key stored by hand (headless) may be a link too.
                    chat_rplay::toolkey_from(&self.stored_token(config).await?)
                        .map_err(SetupError)?,
                );
                #[cfg(feature = "rplay-capture")]
                let source = match &self.rplay_capture {
                    Some(dir) => source.capture_into(dir.clone()),
                    None => source,
                };
                SourceSpec::Rplay(source)
            }
            SourceConfig::StreamElements { account } => {
                SourceSpec::StreamElements(StreamElementsSource {
                    account: chat_streamelements::normalize_account(account).map_err(SetupError)?,
                    token: self.stored_token(config).await?,
                    tips: self.tips.clone(),
                })
            }
            SourceConfig::Streamlabs { label } => SourceSpec::Streamlabs(StreamlabsSource {
                label: chat_streamlabs::normalize_label(label).map_err(SetupError)?,
                token: self.stored_token(config).await?,
                tips: self.tips.clone(),
            }),
            SourceConfig::Demo => SourceSpec::Demo(DemoSource::default()),
            SourceConfig::YouTube { video_id, account } => {
                let settings = self.youtube();
                let emojis = match &settings.emojis {
                    Some(path) => EmojiMap::load(path)
                        .map_err(|e| SetupError(format!("YouTube emoji file: {e:#}")))?,
                    None => EmojiMap::default(),
                };
                let target = match video_id {
                    None => YouTubeTarget::OwnBroadcast {
                        login: self.login(account.as_deref()).await?,
                    },
                    Some(id) => {
                        // A public video works with just an API key; without
                        // one, the login works too.
                        let auth = match &settings.api_key {
                            Some(key) => Auth::ApiKey(key.clone()),
                            None => Auth::OAuth(self.login(account.as_deref()).await?),
                        };
                        YouTubeTarget::Video {
                            id: id.clone(),
                            auth,
                        }
                    }
                };
                SourceSpec::YouTube(YouTubeSource {
                    target,
                    emojis,
                    emoji_cache: self.emoji_cache.clone(),
                    learn_emojis: settings.learn_emojis,
                })
            }
        })
    }

    fn forget_login(&self) {
        let next = self.login_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        // Best-effort eager clear so dropped `TokenProvider`s free memory right
        // away; even if `try_lock` misses, the bumped epoch guarantees the next
        // `login()` clears stale entries and won't cache an in-flight load.
        if let Ok(mut cached) = self.logins.try_lock() {
            cached.providers.clear();
            cached.epoch = next;
        }
    }

    fn set_youtube(&self, settings: YouTubeSettings) {
        *self
            .youtube
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_errors_wait_for_the_user() {
        // Found behind added context too, as the actor sees errors.
        let missing = anyhow::Error::new(SetupError(
            "there is no Picarto channel named \"your_channel\"".into(),
        ))
        .context("Picarto chat");
        assert!(needs_attention(&missing));
        assert!(!needs_attention(&anyhow::anyhow!("connection reset")));
    }

    /// A YouTube source uses its channel's login from the engine's secret
    /// store; without a channel, the first one listed, and before any is,
    /// the single login of older versions. Loading one asks nothing of
    /// Google.
    #[tokio::test]
    async fn a_youtube_source_uses_its_channels_login() {
        let dir = std::env::temp_dir().join(format!(
            "chat-engine-factory-youtube-{}",
            std::process::id()
        ));
        let store = SecretStore::Dir(dir.clone());
        let mut youtube = YouTubeSettings {
            client_id: Some("test-client.apps.googleusercontent.com".into()),
            client_secret: Some("test-secret".into()),
            ..Default::default()
        };
        let factory = |youtube: &YouTubeSettings| {
            Production::new(
                youtube.clone(),
                None,
                Some(store.clone()),
                watch::channel(TipSettings::default()).1,
            )
        };
        let own = |account: Option<&str>| SourceConfig::YouTube {
            video_id: None,
            account: account.map(Into::into),
        };
        let builds =
            async |factory: &Production, config: SourceConfig| match factory.build(&config).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    assert!(is_login_required(&e), "{e:#}");
                    Err(())
                }
            };
        let name = |id: &str| format!("youtube:test-client.apps.googleusercontent.com{id}");

        // Before any channel is listed: the older single login.
        let none_listed = factory(&youtube);
        assert!(builds(&none_listed, own(None)).await.is_err());
        store.save(&name(""), "refresh-old").await.unwrap();
        assert!(builds(&none_listed, own(None)).await.is_ok());

        for (id, channel) in [("UC_main", "your_channel"), ("UC_second", "your_second")] {
            youtube.connect(crate::config::YouTubeChannel {
                channel_id: id.into(),
                name: channel.into(),
            });
        }
        store.save(&name(":UC_second"), "refresh-2").await.unwrap();
        let listed = factory(&youtube);
        assert!(builds(&listed, own(Some("UC_second"))).await.is_ok());
        assert!(
            builds(&listed, own(Some("UC_main"))).await.is_err(),
            "another channel's login doesn't stand in"
        );
        assert!(
            builds(&listed, own(None)).await.is_err(),
            "the first listed, not the older login"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_source_with_a_secret_needs_it_stored() {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-factory-tokens-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let factory = Production::new(
            YouTubeSettings::default(),
            None,
            Some(store.clone()),
            watch::channel(TipSettings::default()).1,
        );
        let streamelements = SourceConfig::StreamElements {
            account: "0123456789abcdef01234567".into(),
        };
        let corpo = SourceConfig::Streamlabs {
            label: "Corpo".into(),
        };
        let rplay = SourceConfig::Rplay {
            label: "Main".into(),
        };

        for config in [&streamelements, &corpo, &rplay] {
            let missing = factory.build(config).await.err().expect("no token yet");
            assert!(needs_attention(&missing), "{missing:#}");
        }
        // Another account's token doesn't count.
        store.save("streamlabs:Indie", "other").await.unwrap();
        assert!(factory.build(&corpo).await.is_err());

        store
            .save("streamelements:0123456789abcdef01234567", "t0ken-se")
            .await
            .unwrap();
        store.save("streamlabs:Corpo", "t0ken-corpo").await.unwrap();
        let Ok(SourceSpec::StreamElements(source)) = factory.build(&streamelements).await else {
            panic!("expected a StreamElements source");
        };
        assert_eq!(source.token, "t0ken-se");
        let Ok(SourceSpec::Streamlabs(source)) = factory.build(&corpo).await else {
            panic!("expected a Streamlabs source");
        };
        assert_eq!(
            (source.label.as_str(), source.token.as_str()),
            ("Corpo", "t0ken-corpo")
        );
        // RPlay's toolkey, stored by hand as a whole link: the key is read
        // out of it.
        store
            .save("rplay:Main", "https://rplay.live/x?toolkey=Key-123")
            .await
            .unwrap();
        let Ok(SourceSpec::Rplay(source)) = factory.build(&rplay).await else {
            panic!("expected an RPlay source");
        };
        assert_eq!(
            (source.label.as_str(), source.toolkey.as_str()),
            ("Main", "Key-123")
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
