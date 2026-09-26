//! What the engine runs, as plain data: serializable, so the same types can
//! later be the config file and what the UI edits.

use std::fmt;
use std::path::PathBuf;

use chat_youtube::oauth::OAuthApp;

/// One source, described by its settings. The engine builds the actual
/// source from this on every (re)start: `ChatSource::run` consumes a source,
/// so a restart needs a fresh one.
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
    },
    /// Sample messages of every kind, for styling and testing.
    Demo,
    /// Messages typed into the app's test window. Only ever created by
    /// `EngineHandle::add_manual_source` (its messages come from that
    /// window), so it's never read from or written to the config file.
    #[serde(skip)]
    Manual,
}

impl SourceConfig {
    /// Checks what can be checked without connecting: lets a settings form
    /// point out a typo right away instead of adding a source that can
    /// never work.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            SourceConfig::Twitch { channel } => chat_twitch::normalize_channel(channel).map(drop),
            SourceConfig::YouTube { video_id: Some(id) } if id.trim().is_empty() => {
                Err("the video id is empty".into())
            }
            SourceConfig::YouTube { .. } | SourceConfig::Demo | SourceConfig::Manual => Ok(()),
        }
    }

    /// Short name for status displays and logs.
    pub fn label(&self) -> String {
        match self {
            SourceConfig::Twitch { channel } => format!("Twitch: {channel}"),
            SourceConfig::YouTube { video_id: None } => "YouTube: your channel".into(),
            SourceConfig::YouTube { video_id: Some(id) } => format!("YouTube: video {id}"),
            SourceConfig::Demo => "Demo".into(),
            SourceConfig::Manual => "Test messages".into(),
        }
    }
}

/// YouTube settings shared by all YouTube sources: the streamer's own
/// Google project (see docs/youtube-setup.md).
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YouTubeSettings {
    /// OAuth client of type "Desktop app".
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Optional, only used by `video_id` sources (any public video): with
    /// a key they don't need the login. Your own broadcasts always use it.
    pub api_key: Option<String>,
    /// Custom emoji export (scripts/yt-emoji-export.user.js).
    pub emojis: Option<PathBuf>,
}

impl YouTubeSettings {
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
            .finish()
    }
}

/// A problem only the user can fix (a missing setting, an unreadable file).
/// Sources failing with it are not restarted automatically.
#[derive(Debug)]
pub struct SetupError(pub String);

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupError {}

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
                SourceConfig::YouTube { video_id: None },
                SourceConfig::YouTube {
                    video_id: Some("abc123".into())
                },
                SourceConfig::Demo,
            ]
        );
    }

    #[test]
    fn validation_catches_typos() {
        assert!(
            SourceConfig::Twitch {
                channel: "your_channel".into()
            }
            .validate()
            .is_ok()
        );
        assert!(
            SourceConfig::Twitch {
                channel: "not valid!".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            SourceConfig::YouTube {
                video_id: Some(" ".into())
            }
            .validate()
            .is_err()
        );
        assert!(SourceConfig::YouTube { video_id: None }.validate().is_ok());
    }

    #[test]
    fn debug_hides_secrets() {
        let settings = YouTubeSettings {
            client_id: Some("id".into()),
            client_secret: Some("GOCSPX-secret".into()),
            api_key: Some("AIza-key".into()),
            emojis: None,
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
