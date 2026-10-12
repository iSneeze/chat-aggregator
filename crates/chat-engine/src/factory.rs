//! Turning a `SourceConfig` into a runnable source.
//!
//! The actor is generic over a [`SourceFactory`], so tests can hand it
//! scripted sources ("fails twice, then runs") while production builds the
//! real Twitch/YouTube/demo sources. This is static dispatch (generics), not
//! a trait object: the compiler generates one actor per factory type.

use std::future::Future;

use chat_core::demo::DemoSource;
use chat_core::{ChatEvent, ChatSource, Reporter};
use chat_twitch::TwitchSource;
use chat_youtube::oauth::{LoginRequired, TokenProvider};
use chat_youtube::{Auth, EmojiMap, YouTubeSource, YouTubeTarget};
use tokio::sync::mpsc;

use crate::config::{SetupError, SourceConfig, YouTubeSettings};

pub(crate) trait SourceFactory: Send + Sync + 'static {
    type Source: ChatSource + Send + 'static;

    /// Builds a source. Errors that only the user can fix should be (or
    /// wrap) a `SetupError` or `LoginRequired`, see [`needs_attention`].
    fn build(
        &self,
        config: &SourceConfig,
    ) -> impl Future<Output = anyhow::Result<Self::Source>> + Send;

    /// A source needed a login that isn't valid (anymore). Forget any cached
    /// one, so the next build picks up a fresh login.
    fn forget_login(&self) {}

    /// New YouTube settings (client, API key, emojis) for future builds.
    fn set_youtube(&self, _settings: YouTubeSettings) {}
}

/// Errors not worth retrying automatically: the user has to act.
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
            SourceSpec::Demo(s) => s.run(tx, activity).await,
        }
    }
}

pub(crate) struct Production {
    // Changed at runtime by `set_youtube`, read by every build. A std Mutex:
    // only ever held for a quick clone, never across an `.await`.
    youtube: std::sync::Mutex<YouTubeSettings>,
    /// One login shared by all YouTube sources: one cached access token,
    /// refreshed once, instead of one per source.
    login: tokio::sync::Mutex<Option<TokenProvider>>,
}

impl Production {
    pub(crate) fn new(youtube: YouTubeSettings) -> Self {
        Self {
            youtube: std::sync::Mutex::new(youtube),
            login: tokio::sync::Mutex::new(None),
        }
    }

    fn youtube(&self) -> YouTubeSettings {
        self.youtube
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn login(&self) -> anyhow::Result<TokenProvider> {
        let mut cached = self.login.lock().await;
        if let Some(provider) = &*cached {
            return Ok(provider.clone());
        }
        let provider = TokenProvider::from_store(self.youtube().oauth_app()?).await?;
        *cached = Some(provider.clone());
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
            SourceConfig::Demo => SourceSpec::Demo(DemoSource::default()),
            SourceConfig::YouTube { video_id } => {
                let settings = self.youtube();
                let emojis = match &settings.emojis {
                    Some(path) => EmojiMap::load(path)
                        .map_err(|e| SetupError(format!("YouTube emoji file: {e:#}")))?,
                    None => EmojiMap::default(),
                };
                let (target, auth) = match video_id {
                    None => (
                        YouTubeTarget::OwnBroadcast,
                        Auth::OAuth(self.login().await?),
                    ),
                    Some(id) => {
                        // A public video works with just an API key; without
                        // one, the login works too.
                        let auth = match &settings.api_key {
                            Some(key) => Auth::ApiKey(key.clone()),
                            None => Auth::OAuth(self.login().await?),
                        };
                        (YouTubeTarget::Video(id.clone()), auth)
                    }
                };
                SourceSpec::YouTube(YouTubeSource {
                    target,
                    auth,
                    emojis,
                })
            }
        })
    }

    fn forget_login(&self) {
        // `try_lock`: this is sync; if a build holds the lock right now, it's
        // loading a fresh login anyway.
        if let Ok(mut cached) = self.login.try_lock() {
            *cached = None;
        }
    }

    fn set_youtube(&self, settings: YouTubeSettings) {
        *self
            .youtube
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
    }
}
