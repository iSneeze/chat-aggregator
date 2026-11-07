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
use chat_core::{ChatEvent, ChatSource, Reporter};
use chat_picarto::{ChannelNotFound, PicartoSource};
use chat_streamelements::{StreamElementsSource, TokenRefused};
// Renamed: both tip services have a `TokenRefused`.
use chat_streamlabs::{StreamlabsSource, TokenRefused as StreamlabsTokenRefused};
use chat_twitch::TwitchSource;
use chat_youtube::oauth::{LoginRequired, TokenProvider};
use chat_youtube::{Auth, EmojiMap, YouTubeSource, YouTubeTarget};
use tokio::sync::{mpsc, watch};

use crate::config::{SetupError, SourceConfig, YouTubeSettings};

pub(crate) trait SourceFactory: Send + Sync + 'static {
    type Source: ChatSource + Send + 'static;

    /// Builds a source. Errors that only the user can fix should be (or
    /// wrap) a `SetupError`, `LoginRequired`, `ChannelNotFound` or a tip
    /// service's `TokenRefused`, see [`needs_attention`].
    fn build(
        &self,
        config: &SourceConfig,
    ) -> impl Future<Output = anyhow::Result<Self::Source>> + Send;

    /// A source needed a login that isn't valid (anymore). Forget any cached
    /// one, so the next build picks up a fresh login.
    fn forget_login(&self) {}

    /// New YouTube settings (client, API key, emojis) for future builds.
    fn set_youtube(&self, _settings: YouTubeSettings) {}

    /// New tip settings (currency, tiers): for future builds, and for the
    /// running tip sources, which apply them to their next tip.
    fn set_tips(&self, _tips: TipSettings) {}
}

/// Errors not worth retrying automatically: the user has to act.
pub(crate) fn needs_attention(err: &anyhow::Error) -> bool {
    // `chain()` walks the error and everything it wraps, so this also finds
    // a `LoginRequired` that got extra context attached on the way up.
    err.chain().any(|e| {
        e.is::<SetupError>()
            || e.is::<LoginRequired>()
            || e.is::<ChannelNotFound>()
            || e.is::<TokenRefused>()
            || e.is::<StreamlabsTokenRefused>()
    })
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
    /// Every tip source watches this (`subscribe`), so a change reaches
    /// the running ones without a reconnect.
    tips: watch::Sender<TipSettings>,
}

impl Production {
    pub(crate) fn new(
        youtube: YouTubeSettings,
        emoji_cache: Option<PathBuf>,
        secrets: Option<SecretStore>,
        tips: TipSettings,
    ) -> Self {
        Self {
            youtube: std::sync::Mutex::new(youtube),
            login: tokio::sync::Mutex::new(None),
            emoji_cache,
            secrets,
            tips: watch::Sender::new(tips),
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
            SourceConfig::Picarto { channel } => SourceSpec::Picarto(PicartoSource {
                channel: chat_picarto::normalize_channel(channel).map_err(SetupError)?,
            }),
            SourceConfig::StreamElements { account } => {
                let account =
                    chat_streamelements::normalize_account(account).map_err(SetupError)?;
                // Read at every (re)start: a token stored again since
                // (the source added anew) is picked up by "Start".
                let token = match &self.secrets {
                    Some(secrets) => secrets
                        .load(&chat_streamelements::token_secret_name(&account))
                        .await
                        .context("reading the StreamElements token")?,
                    None => None,
                };
                let token = token.ok_or_else(|| {
                    SetupError(
                        "no StreamElements Overlay Token is stored for this Account ID: remove \
                         this source and add it again with the token (headless: run \
                         --streamelements-token)"
                            .into(),
                    )
                })?;
                SourceSpec::StreamElements(StreamElementsSource {
                    account,
                    token,
                    tips: self.tips.subscribe(),
                })
            }
            SourceConfig::Streamlabs { label } => {
                let label = chat_streamlabs::normalize_label(label).map_err(SetupError)?;
                // Read at every (re)start, like the StreamElements token.
                let token = match &self.secrets {
                    Some(secrets) => secrets
                        .load(&chat_streamlabs::token_secret_name(&label))
                        .await
                        .context("reading the Streamlabs token")?,
                    None => None,
                };
                let token = token.ok_or_else(|| {
                    SetupError(
                        "no Streamlabs Socket API Token is stored under this name: remove this \
                         source and add it again with the token (headless: run \
                         --streamlabs-token)"
                            .into(),
                    )
                })?;
                SourceSpec::Streamlabs(StreamlabsSource {
                    label,
                    token,
                    tips: self.tips.subscribe(),
                })
            }
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

    fn set_tips(&self, tips: TipSettings) {
        self.tips.send_replace(tips);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_picarto_channel_waits_for_the_user() {
        // Found behind added context too, as the actor sees errors.
        let missing =
            anyhow::Error::new(ChannelNotFound("your_channel".into())).context("Picarto chat");
        assert!(needs_attention(&missing));
        assert!(!needs_attention(&anyhow::anyhow!("connection reset")));
    }

    #[test]
    fn a_refused_streamelements_token_waits_for_the_user() {
        let refused =
            anyhow::Error::new(TokenRefused("permission_denied".into())).context("StreamElements");
        assert!(needs_attention(&refused));
    }

    #[test]
    fn a_refused_streamlabs_token_waits_for_the_user() {
        let refused = anyhow::Error::new(StreamlabsTokenRefused("Authentication error".into()))
            .context("Streamlabs");
        assert!(needs_attention(&refused));
    }

    #[tokio::test]
    async fn a_streamlabs_source_needs_its_stored_token() {
        let dir = std::env::temp_dir().join(format!("chat-engine-sl-token-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let factory = Production::new(
            YouTubeSettings::default(),
            None,
            Some(store.clone()),
            TipSettings::default(),
        );
        let config = SourceConfig::Streamlabs {
            label: "Corpo".into(),
        };

        let missing = factory.build(&config).await.err().expect("no token yet");
        assert!(needs_attention(&missing), "{missing:#}");

        // Each label has its own token.
        store
            .save(&chat_streamlabs::token_secret_name("Indie"), "other")
            .await
            .unwrap();
        assert!(factory.build(&config).await.is_err());
        store
            .save(&chat_streamlabs::token_secret_name("Corpo"), "t0ken")
            .await
            .unwrap();
        let Ok(SourceSpec::Streamlabs(source)) = factory.build(&config).await else {
            panic!("expected a Streamlabs source");
        };
        assert_eq!(
            (source.label.as_str(), source.token.as_str()),
            ("Corpo", "t0ken")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_streamelements_source_needs_its_stored_token() {
        let dir = std::env::temp_dir().join(format!("chat-engine-se-token-{}", std::process::id()));
        let store = SecretStore::Dir(dir.clone());
        let factory = Production::new(
            YouTubeSettings::default(),
            None,
            Some(store.clone()),
            TipSettings::default(),
        );
        let config = SourceConfig::StreamElements {
            account: "0123456789abcdef01234567".into(),
        };

        let missing = factory.build(&config).await.err().expect("no token yet");
        assert!(needs_attention(&missing), "{missing:#}");

        let name = chat_streamelements::token_secret_name("0123456789abcdef01234567");
        store.save(&name, "t0ken").await.unwrap();
        let Ok(SourceSpec::StreamElements(source)) = factory.build(&config).await else {
            panic!("expected a StreamElements source");
        };
        assert_eq!(source.token, "t0ken");

        let _ = std::fs::remove_dir_all(dir);
    }
}
