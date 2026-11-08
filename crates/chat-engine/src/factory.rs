//! Turning a `SourceConfig` into a runnable source.
//!
//! The actor is generic over a [`SourceFactory`], so tests can hand it
//! scripted sources ("fails twice, then runs") while production builds the
//! real Twitch/YouTube/Picarto/StreamElements/Streamlabs/demo sources. This is static dispatch
//! (generics), not a trait object: the compiler generates one actor per factory type.

use std::future::Future;
use std::path::PathBuf;

use anyhow::Context;
use chat_core::demo::DemoSource;
use chat_core::secrets::SecretStore;
use chat_core::tiers::TipSettings;
use chat_core::{ChatEvent, ChatSource, Reporter, SetupError};
use chat_picarto::PicartoSource;
use chat_streamelements::StreamElementsSource;
use chat_streamlabs::StreamlabsSource;
use chat_twitch::TwitchSource;
use chat_youtube::oauth::{LoginRequired, TokenProvider};
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

    /// A source needed a login that isn't valid (anymore). Forget any cached
    /// one, so the next build picks up a fresh login.
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
            SourceSpec::StreamElements(s) => s.run(tx, activity).await,
            SourceSpec::Streamlabs(s) => s.run(tx, activity).await,
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
    /// See `EngineConfig::emoji_cache_dir`.
    emoji_cache: Option<PathBuf>,
    /// See `EngineConfig::secrets`: the tip services' tokens.
    secrets: Option<SecretStore>,
    /// The tip settings, watched: every tip source gets a copy of this
    /// receiver, so a change reaches the running ones without a reconnect.
    /// The same channel the reactor's amount rules read (`Engine::start`).
    tips: watch::Receiver<TipSettings>,
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
            login: tokio::sync::Mutex::new(None),
            emoji_cache,
            secrets,
            tips,
        }
    }

    fn youtube(&self) -> YouTubeSettings {
        self.youtube
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The token of a tip service's source, from the secret store. Read at
    /// every (re)start: a token stored again since (the source added anew)
    /// is picked up by "Start".
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
                "no {} is stored for this source: remove it and add it again with the token \
                 (headless: run {})",
                token.what, token.headless_flag
            ))
            .into()
        })
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
            SourceConfig::Picarto { channel } => SourceSpec::Picarto(PicartoSource {
                channel: chat_picarto::normalize_channel(channel).map_err(SetupError)?,
            }),
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
                    emoji_cache: self.emoji_cache.clone(),
                    learn_emojis: settings.learn_emojis,
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

    #[tokio::test]
    async fn a_tip_source_needs_its_stored_token() {
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

        for config in [&streamelements, &corpo] {
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

        let _ = std::fs::remove_dir_all(dir);
    }
}
