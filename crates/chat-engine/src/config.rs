//! What the engine runs, as plain data: serializable, so the same types can
//! later be the config file and what the UI edits.

use std::fmt;
use std::path::PathBuf;

use chat_core::SetupError;
use chat_youtube::oauth::OAuthApp;

/// One source, described by its settings. The engine builds the actual
/// source from this on every (re)start: `ChatSource::run` consumes a source,
/// so a restart needs a fresh one.
///
/// Every source that can be in `config.toml`, and only those: the app's test
/// window has a source too, but no settings (the window feeds it, see
/// `EngineHandle::add_manual_source`), so it has no variant here and can't
/// end up in the file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
// `deny_unknown_fields`: a typo like `vidoe_id` must be an error; ignoring
// it would silently turn "this video" into "your own broadcasts".
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceConfig {
    Twitch {
        channel: String,
    },
    /// Without `video_id`: your own current/next broadcast (needs the
    /// YouTube login; the streamer path). With: that specific video.
    #[serde(rename = "youtube")]
    YouTube {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        video_id: Option<String>,
        /// Whose login, by channel id (one of `[[youtube.accounts]]`), for a
        /// streamer with several channels. Without: the first account
        /// connected (or the login of versions before several accounts).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// Any channel's chat, read anonymously like a logged-out visitor.
    Picarto {
        channel: String,
    },
    /// Tips through StreamElements. `account` is the channel's "Account ID"
    /// from the StreamElements dashboard; its Overlay Token is kept in the
    /// secret store, not here (see [`SourceConfig::token`]).
    #[serde(rename = "streamelements")]
    StreamElements {
        account: String,
    },
    /// Tips through Streamlabs. `label` is the streamer's own name for the
    /// account ("Main", "Corpo"): a streamer with several personas adds one
    /// source per account and switches each on or off. Its Socket API Token
    /// is kept in the secret store under the label, not here (see
    /// [`SourceConfig::token`]).
    Streamlabs {
        label: String,
    },
    /// Sample messages of every kind, for styling and testing.
    Demo,
}

impl SourceConfig {
    /// Checks what can be checked without connecting: lets a settings form
    /// point out a typo right away instead of adding a source that can
    /// never work.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            SourceConfig::Twitch { channel } => chat_twitch::normalize_channel(channel).map(drop),
            SourceConfig::YouTube {
                video_id: Some(id), ..
            } if id.trim().is_empty() => Err("the video id is empty".into()),
            SourceConfig::YouTube {
                account: Some(id), ..
            } if id.trim().is_empty() => Err("the YouTube account is empty".into()),
            SourceConfig::Picarto { channel } => chat_picarto::normalize_channel(channel).map(drop),
            SourceConfig::StreamElements { account } => {
                chat_streamelements::normalize_account(account).map(drop)
            }
            SourceConfig::Streamlabs { label } => chat_streamlabs::normalize_label(label).map(drop),
            SourceConfig::YouTube { .. } | SourceConfig::Demo => Ok(()),
        }
    }

    /// Short name for logs. Status displays use [`label_with`]
    /// (Self::label_with), which can name a YouTube account.
    pub fn label(&self) -> String {
        self.label_with(&YouTubeSettings::default())
    }

    /// Short name for status displays: a YouTube source of your own
    /// broadcasts is named after its channel, as listed in `youtube`'s
    /// accounts.
    pub fn label_with(&self, youtube: &YouTubeSettings) -> String {
        match self {
            SourceConfig::Twitch { channel } => format!("Twitch: {channel}"),
            SourceConfig::YouTube {
                video_id: None,
                account,
            } => match (account, youtube.account_for(account.as_deref())) {
                (_, Some(listed)) => format!("YouTube: {}", listed.name),
                (Some(id), None) => format!("YouTube: channel {id}"),
                (None, None) => "YouTube: your channel".into(),
            },
            SourceConfig::YouTube {
                video_id: Some(id), ..
            } => format!("YouTube: video {id}"),
            SourceConfig::Picarto { channel } => format!("Picarto: {channel}"),
            // The account id means nothing to the streamer: there's usually
            // just the one.
            SourceConfig::StreamElements { .. } => "StreamElements tips".into(),
            SourceConfig::Streamlabs { label } => format!("Streamlabs: {label}"),
            SourceConfig::Demo => "Demo".into(),
        }
    }

    /// The token this source reads from the secret store, if it has one;
    /// `None` too for settings that could never have stored one (they don't
    /// pass `validate`). Everything that differs between the tip services'
    /// tokens is here, so storing, loading and deleting them is the same
    /// code for both (`tokens.rs`).
    pub fn token(&self) -> Option<SourceToken> {
        // The names are the ones tokens were always stored under: changing
        // them would lose the tokens streamers have stored. `.ok()?`: a
        // malformed id or label ends the function with `None`.
        match self {
            SourceConfig::StreamElements { account } => Some(SourceToken {
                secret_name: format!(
                    "streamelements:{}",
                    chat_streamelements::normalize_account(account).ok()?
                ),
                what: "StreamElements Overlay Token",
                headless_flag: "--streamelements-token",
            }),
            SourceConfig::Streamlabs { label } => Some(SourceToken {
                secret_name: format!(
                    "streamlabs:{}",
                    chat_streamlabs::normalize_label(label).ok()?
                ),
                what: "Streamlabs Socket API Token",
                headless_flag: "--streamlabs-token",
            }),
            SourceConfig::Twitch { .. }
            | SourceConfig::YouTube { .. }
            | SourceConfig::Picarto { .. }
            | SourceConfig::Demo => None,
        }
    }
}

/// A source's token in the secret store (see [`SourceConfig::token`]):
/// never in `config.toml`, which gets pasted into bug reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceToken {
    /// Its name in the secret store: the service and the account id (or
    /// the streamer's label), so each account has its own token.
    pub secret_name: String,
    /// What the token is called on the service's dashboard, for messages.
    pub what: &'static str,
    /// The headless `run` example's flag that stores it.
    pub headless_flag: &'static str,
}

/// A connected YouTube channel (`[[youtube.accounts]]`): its login is in
/// the secret store under its id. Listed here because the store can't list
/// what's in it; neither value is a secret.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YouTubeChannel {
    /// Google's id for it (`UC…`): stable, unlike the name.
    pub channel_id: String,
    /// Its name when it was connected, to show in the app.
    pub name: String,
}

/// YouTube settings shared by all YouTube sources: the streamer's own
/// Google project (see docs/youtube-setup.md).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
// `default`: a missing setting takes its value from `Default` below.
#[serde(default, deny_unknown_fields)]
pub struct YouTubeSettings {
    /// OAuth client of type "Desktop app".
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Optional, only used by `video_id` sources (any public video): with
    /// a key they don't need the login. Your own broadcasts always use it.
    pub api_key: Option<String>,
    /// Custom emoji export (scripts/yt-emoji-export.user.js).
    pub emojis: Option<PathBuf>,
    /// Learn custom emoji from YouTube's web chat while reading a chat
    /// (docs/youtube-emoji.md). On by default: it's what makes channel
    /// emoji show up without the export.
    pub learn_emojis: bool,
    /// The connected channels, in the order they were connected. The app
    /// adds one at each new login and removes it at "Log out".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<YouTubeChannel>,
}

// Written by hand for `learn_emojis`: a derived `Default` would make it
// `false`, since that's `bool`'s default.
impl Default for YouTubeSettings {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            api_key: None,
            emojis: None,
            learn_emojis: true,
            accounts: Vec::new(),
        }
    }
}

impl YouTubeSettings {
    /// The account a YouTube source with `account` uses, if it's listed:
    /// that channel, or without one, the first connected.
    pub fn account_for(&self, account: Option<&str>) -> Option<&YouTubeChannel> {
        match account {
            Some(id) => self.accounts.iter().find(|a| a.channel_id == id),
            None => self.accounts.first(),
        }
    }

    /// Adds a newly connected channel, or updates its name if it's
    /// connected already (logged in again, or renamed since).
    pub fn connect(&mut self, channel: YouTubeChannel) {
        match self
            .accounts
            .iter_mut()
            .find(|a| a.channel_id == channel.channel_id)
        {
            Some(listed) => listed.name = channel.name,
            None => self.accounts.push(channel),
        }
    }

    pub fn oauth_app(&self) -> Result<OAuthApp, SetupError> {
        match (&self.client_id, &self.client_secret) {
            (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => {
                Ok(OAuthApp::new(id, secret))
            }
            _ => Err(SetupError(
                "YouTube isn't set up yet: your Google project's client id and secret are missing"
                    .into(),
            )),
        }
    }
}

// Written by hand instead of derived: a derived `Debug` would print the
// client secret and API key into any log line that shows the settings.
impl fmt::Debug for YouTubeSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hidden = |set: &Option<String>| if set.is_some() { "<set>" } else { "<unset>" };
        f.debug_struct("YouTubeSettings")
            .field("client_id", &self.client_id)
            .field("client_secret", &hidden(&self.client_secret))
            .field("api_key", &hidden(&self.api_key))
            .field("emojis", &self.emojis)
            .field("learn_emojis", &self.learn_emojis)
            .field("accounts", &self.accounts)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_config_toml_shape() {
        // The shape the config file will have: `type` picks the variant.
        #[derive(serde::Deserialize)]
        struct File {
            sources: Vec<SourceConfig>,
        }
        let file: File = serde_json::from_value(serde_json::json!({
            "sources": [
                { "type": "twitch", "channel": "your_channel" },
                { "type": "youtube" },
                { "type": "youtube", "video_id": "abc123" },
                { "type": "youtube", "account": "UC_second" },
                { "type": "picarto", "channel": "your_channel" },
                { "type": "streamelements", "account": "0123456789abcdef01234567" },
                { "type": "streamlabs", "label": "Corpo" },
                { "type": "demo" }
            ]
        }))
        .unwrap();
        assert_eq!(
            file.sources,
            [
                SourceConfig::Twitch {
                    channel: "your_channel".into()
                },
                SourceConfig::YouTube {
                    video_id: None,
                    account: None
                },
                SourceConfig::YouTube {
                    video_id: Some("abc123".into()),
                    account: None
                },
                SourceConfig::YouTube {
                    video_id: None,
                    account: Some("UC_second".into())
                },
                SourceConfig::Picarto {
                    channel: "your_channel".into()
                },
                SourceConfig::StreamElements {
                    account: "0123456789abcdef01234567".into()
                },
                SourceConfig::Streamlabs {
                    label: "Corpo".into()
                },
                SourceConfig::Demo,
            ]
        );
    }

    #[test]
    fn validation_catches_typos() {
        // Each source's settings go through its own check: one that passes
        // and one that doesn't per source (the checks themselves are tested
        // in the platform crates).
        let valid = [
            SourceConfig::Twitch {
                channel: "your_channel".into(),
            },
            SourceConfig::YouTube {
                video_id: None,
                account: None,
            },
            SourceConfig::Picarto {
                channel: "picarto.tv/your_channel".into(),
            },
            SourceConfig::StreamElements {
                account: "0123456789abcdef01234567".into(),
            },
            SourceConfig::Streamlabs {
                label: "Corpo".into(),
            },
        ];
        let invalid = [
            SourceConfig::Twitch {
                channel: "not valid!".into(),
            },
            SourceConfig::YouTube {
                video_id: Some(" ".into()),
                account: None,
            },
            SourceConfig::YouTube {
                video_id: None,
                account: Some(" ".into()),
            },
            SourceConfig::Picarto {
                channel: "not valid!".into(),
            },
            SourceConfig::StreamElements {
                account: "your_channel".into(),
            },
            SourceConfig::Streamlabs { label: " ".into() },
        ];
        for config in valid {
            assert!(config.validate().is_ok(), "refused: {config:?}");
        }
        for config in invalid {
            assert!(config.validate().is_err(), "accepted: {config:?}");
        }
    }

    #[test]
    fn a_youtube_source_is_named_after_its_account() {
        let own = |account: Option<&str>| SourceConfig::YouTube {
            video_id: None,
            account: account.map(Into::into),
        };
        let mut youtube = YouTubeSettings::default();
        assert_eq!(own(None).label_with(&youtube), "YouTube: your channel");
        assert_eq!(
            own(Some("UC_second")).label_with(&youtube),
            "YouTube: channel UC_second",
            "not listed (anymore): its id at least"
        );
        youtube.connect(YouTubeChannel {
            channel_id: "UC_main".into(),
            name: "your_channel".into(),
        });
        youtube.connect(YouTubeChannel {
            channel_id: "UC_second".into(),
            name: "your_second_channel".into(),
        });
        assert_eq!(
            own(Some("UC_second")).label_with(&youtube),
            "YouTube: your_second_channel"
        );
        assert_eq!(
            own(None).label_with(&youtube),
            "YouTube: your_channel",
            "without an account: the first one, which it uses"
        );
        let video = SourceConfig::YouTube {
            video_id: Some("abc123".into()),
            account: Some("UC_main".into()),
        };
        assert_eq!(video.label_with(&youtube), "YouTube: video abc123");
        assert_eq!(own(Some("UC_second")).label(), "YouTube: channel UC_second");
    }

    #[test]
    fn connecting_a_channel_again_keeps_one_entry() {
        let mut youtube = YouTubeSettings::default();
        let channel = |id: &str, name: &str| YouTubeChannel {
            channel_id: id.into(),
            name: name.into(),
        };
        youtube.connect(channel("UC_main", "your_channel"));
        youtube.connect(channel("UC_second", "your_second_channel"));
        // Logged in again after renaming it on YouTube: same place, new name.
        youtube.connect(channel("UC_main", "your_renamed_channel"));
        assert_eq!(
            youtube.accounts,
            [
                channel("UC_main", "your_renamed_channel"),
                channel("UC_second", "your_second_channel")
            ]
        );
    }

    #[test]
    fn debug_hides_secrets() {
        let settings = YouTubeSettings {
            client_id: Some("id".into()),
            client_secret: Some("GOCSPX-secret".into()),
            api_key: Some("AIza-key".into()),
            ..Default::default()
        };
        let printed = format!("{settings:?}");
        assert!(
            !printed.contains("GOCSPX-secret") && !printed.contains("AIza-key"),
            "{printed}"
        );
    }

    #[test]
    fn oauth_app_needs_both_values() {
        assert!(YouTubeSettings::default().oauth_app().is_err());
        let settings = YouTubeSettings {
            client_id: Some("id".into()),
            client_secret: Some("secret".into()),
            ..Default::default()
        };
        assert!(settings.oauth_app().is_ok());
    }
}
