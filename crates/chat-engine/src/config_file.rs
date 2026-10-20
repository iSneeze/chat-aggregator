//! The settings file (`config.toml`), shared by headless mode and the app.
//!
//! The engine starts with what's in it and from then on owns the settings
//! ([`Settings`]) and the sources: it applies changes
//! (`EngineHandle::edit_settings`, the source commands), publishes the
//! current state and saves it here after each change
//! (`EngineConfig::save_to`). Never at startup, and never in headless mode:
//! a file nobody changed keeps its comments.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chat_core::fs::write_private;

use crate::config::{SourceConfig, YouTubeSettings};
use crate::{DEFAULT_HISTORY, DEFAULT_PORT, EngineConfig, Newest, Stagger, ThemeSource};

/// A commented starting point, written by `run --init-config`.
pub const TEMPLATE: &str = include_str!("../config.example.toml");

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
// `default`: a missing section or setting takes its default value.
// `deny_unknown_fields`: a typo like `prot = 7878` is an error pointing at
// the line, instead of being silently ignored.
#[serde(default, deny_unknown_fields)]
pub struct ConfigFile {
    pub server: ServerSettings,
    pub youtube: YouTubeSettings,
    pub app: AppSettings,
    pub sources: Vec<SourceEntry>,
}

/// Everything in `config.toml` except the sources (the engine runs those,
/// see `EngineHandle::add_source`). The engine owns them while it runs:
/// read them with `EngineHandle::settings`, change them with
/// `EngineHandle::edit_settings`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub server: ServerSettings,
    pub youtube: YouTubeSettings,
    pub app: AppSettings,
}

/// One `[[sources]]` block: what to read, and whether it's switched on.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SourceEntry {
    // `flatten`: the source's own keys (`type`, `channel`, …) sit directly
    // in the block next to `enabled`, instead of in a nested table. Unknown
    // keys still get caught: whatever `enabled` doesn't claim goes to
    // `SourceConfig`, which refuses keys it doesn't know.
    #[serde(flatten)]
    pub config: SourceConfig,
    /// Switched off in the app: listed, but not connected at start. Only
    /// written when `false`, so a hand-written file doesn't need it.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub enabled: bool,
}

fn on() -> bool {
    true
}

// serde passes the field by reference, hence `&bool`.
fn is_on(enabled: &bool) -> bool {
    *enabled
}

impl From<SourceConfig> for SourceEntry {
    /// A new source is switched on.
    fn from(config: SourceConfig) -> Self {
        Self {
            config,
            enabled: true,
        }
    }
}

/// The desktop app's own settings; headless mode ignores them.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppSettings {
    pub appearance: Appearance,
}

/// The app's look. `System` follows the operating system's light/dark
/// setting; the others stay put.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
    HighContrast,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    pub port: u16,
    pub history: usize,
    /// Overlay theme: a built-in theme's name (`default`, `minimal`), a
    /// folder name in `themes/` next to the config file, or the full path of
    /// a theme folder anywhere. `None`: the default built-in theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    pub stagger_ms: u64,
    pub stagger_max_ms: u64,
    /// The JSON API for other programs. Off by default: most streamers
    /// never need it.
    pub api: bool,
    /// Web sites allowed to use the JSON API, e.g. a browser game
    /// (`"https://game.example"`). Programs, pages served from this
    /// computer and OBS browser sources don't need to be listed. The app
    /// asks about other sites and adds them here on "Allow".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub api_origins: Vec<String>,
    /// Which end of the overlay new messages appear at, unless the overlay
    /// URL says otherwise (`?newest=top`).
    pub newest: Newest,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            history: DEFAULT_HISTORY,
            theme: None,
            stagger_ms: 250,
            stagger_max_ms: 2000,
            api: false,
            api_origins: Vec::new(),
            newest: Newest::Bottom,
        }
    }
}

impl ServerSettings {
    /// The theme `theme` stands for, with the theme folders in
    /// `themes_dir`. A folder that isn't there (yet) still counts: it loads
    /// as the built-in look, the app warns about it, and once it's created,
    /// the next overlay reload picks it up.
    pub fn theme_source(&self, themes_dir: Option<&Path>) -> ThemeSource {
        // Without a themes folder (tests), a name is a folder in the
        // current directory.
        let themes_dir = themes_dir.unwrap_or(Path::new(""));
        self.theme
            .as_deref()
            .map_or_else(ThemeSource::default, |name| {
                ThemeSource::named(themes_dir, name)
            })
    }

    /// The overlay's burst pacing.
    pub fn stagger(&self) -> Stagger {
        Stagger::new(
            Duration::from_millis(self.stagger_ms),
            Duration::from_millis(self.stagger_max_ms),
        )
    }
}

impl ConfigFile {
    /// `config.toml` in the per-OS config folder: `~/.config/chat-aggregator`
    /// on Linux, `~/Library/Application Support/chat-aggregator` on macOS,
    /// `%APPDATA%\chat-aggregator` on Windows.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "chat-aggregator")
            .context("can't determine the config folder (no home directory?)")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Like [`load`](Self::load), but a missing file just means defaults.
    pub fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        match Self::load(path) {
            Err(e) if is_not_found(&e) => Ok(Self::default()),
            other => other,
        }
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        // toml's error message already names the line and shows it.
        Ok(toml::from_str(text)?)
    }

    /// Writes the file, readable only by you (it contains the client
    /// secret). Comments in an existing file are not kept.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_private(path, &toml::to_string_pretty(self)?)
    }

    /// Writes the commented template, unless the file already exists.
    pub fn write_template(path: &Path) -> anyhow::Result<()> {
        if path.exists() {
            anyhow::bail!("{} already exists; not overwriting it", path.display());
        }
        write_private(path, TEMPLATE)
    }

    /// The folder with the overlay themes, next to the config file in
    /// `settings_dir`.
    pub fn themes_dir(settings_dir: &Path) -> PathBuf {
        settings_dir.join("themes")
    }

    /// `settings_dir` is the folder the config file is in: themes and the
    /// emoji caches are kept next to it.
    pub fn into_engine_config(self, settings_dir: &Path) -> EngineConfig {
        EngineConfig {
            settings: Settings {
                server: self.server,
                youtube: self.youtube,
                app: self.app,
            },
            sources: self.sources,
            themes_dir: Some(Self::themes_dir(settings_dir)),
            // Unlike `config.toml`, written in headless mode too: it's
            // data the sources collect, not a file you edit.
            emoji_cache_dir: Some(settings_dir.join("youtube-emojis")),
            // Headless mode has nobody to ask, and leaves the file alone;
            // the app switches both on.
            ask_api_origins: false,
            save_to: None,
        }
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|e| e.downcast_ref::<std::io::Error>())
        .any(|e| e.kind() == ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_file_parses() {
        let file = ConfigFile::parse(
            r#"
            [server]
            port = 8080
            history = 5
            theme = "cozy"
            stagger_ms = 100
            stagger_max_ms = 1000
            api = true
            api_origins = ["https://game.example"]
            newest = "top"

            [youtube]
            client_id = "id"
            client_secret = "secret"

            [[sources]]
            type = "twitch"
            channel = "your_channel"

            [app]
            appearance = "high-contrast"

            [[sources]]
            type = "twitch"
            channel = "other_channel"
            enabled = false

            [[sources]]
            type = "youtube"
            "#,
        )
        .unwrap();
        assert_eq!(file.server.port, 8080);
        assert_eq!(file.server.theme.as_deref(), Some("cozy"));
        assert_eq!(file.youtube.client_id.as_deref(), Some("id"));
        assert_eq!(file.app.appearance, Appearance::HighContrast);
        let twitch = |channel: &str| SourceConfig::Twitch {
            channel: channel.into(),
        };
        assert_eq!(
            file.sources,
            [
                twitch("your_channel").into(),
                SourceEntry {
                    config: twitch("other_channel"),
                    enabled: false,
                },
                SourceConfig::YouTube { video_id: None }.into(),
            ]
        );

        let sources = file.sources.clone();
        let engine = file.into_engine_config(Path::new("/settings"));
        assert_eq!(engine.sources, sources, "switched-off ones too: listed");
        assert!(!engine.ask_api_origins, "headless mode has nobody to ask");
        assert_eq!(engine.save_to, None, "headless mode leaves the file alone");
        let server = &engine.settings.server;
        assert_eq!(server.port, 8080);
        assert!(server.api);
        assert_eq!(server.api_origins, ["https://game.example"]);
        assert_eq!(server.newest, Newest::Top);
        assert_eq!(server.history, 5);
        assert_eq!(engine.settings.app.appearance, Appearance::HighContrast);
        assert_eq!(
            engine.themes_dir.as_deref(),
            Some(Path::new("/settings/themes"))
        );
        assert_eq!(
            engine.emoji_cache_dir.as_deref(),
            Some(Path::new("/settings/youtube-emojis"))
        );
    }

    #[test]
    fn theme_settings_name_or_point_at_a_theme() {
        let themes_dir = Path::new("/settings/themes");
        let theme = |name: &str| {
            ServerSettings {
                theme: Some(name.into()),
                ..ServerSettings::default()
            }
            .theme_source(Some(themes_dir))
        };
        assert_eq!(
            ServerSettings::default().theme_source(Some(themes_dir)),
            ThemeSource::default()
        );
        assert_eq!(
            theme("minimal"),
            ThemeSource::Builtin(crate::Builtin::Minimal),
            "built-in themes by name"
        );
        assert_eq!(
            theme("cozy"),
            ThemeSource::Folder(themes_dir.join("cozy")),
            "a theme name is a folder in themes/ next to the config file"
        );
        let anywhere = std::env::temp_dir().join("my-theme");
        assert_eq!(
            theme(anywhere.to_str().unwrap()),
            ThemeSource::Folder(anywhere),
            "or the full path of any theme folder"
        );
    }

    #[test]
    fn missing_parts_take_defaults() {
        let file = ConfigFile::parse("").unwrap();
        assert_eq!(file, ConfigFile::default());
        assert_eq!(file.server.port, DEFAULT_PORT);
        assert!(!file.server.api, "the JSON API is off unless asked for");

        let file = ConfigFile::parse("[server]\nport = 9000").unwrap();
        assert_eq!(file.server.port, 9000);
        assert_eq!(
            file.server.history, DEFAULT_HISTORY,
            "rest of the section stays default"
        );
    }

    #[test]
    fn typos_are_errors_that_name_the_line() {
        let err = ConfigFile::parse("[server]\nprot = 9000").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("prot") && message.contains("line 2"),
            "{message}"
        );

        let err = ConfigFile::parse("[[sources]]\ntype = \"twich\"").unwrap_err();
        assert!(format!("{err:#}").contains("twich"), "{err:#}");

        // Inside a source and in [youtube] too: a typo must not silently
        // change what gets watched.
        let err =
            ConfigFile::parse("[[sources]]\ntype = \"youtube\"\nvidoe_id = \"x\"").unwrap_err();
        assert!(format!("{err:#}").contains("vidoe_id"), "{err:#}");
        let err = ConfigFile::parse("[youtube]\nclient_secert = \"x\"").unwrap_err();
        assert!(format!("{err:#}").contains("client_secert"), "{err:#}");
        // (Not caught next to `type = "demo"`: serde doesn't check extra
        // keys for variants without fields. Harmless for the demo.)
        let err =
            ConfigFile::parse("[[sources]]\ntype = \"twitch\"\nchannel = \"x\"\nenabeld = false")
                .unwrap_err();
        assert!(format!("{err:#}").contains("enabeld"), "{err:#}");
        let err = ConfigFile::parse("[app]\nappearance = \"blue\"").unwrap_err();
        assert!(format!("{err:#}").contains("blue"), "{err:#}");
    }

    #[test]
    fn the_template_is_valid() {
        let file = ConfigFile::parse(TEMPLATE).unwrap();
        assert_eq!(file.sources, [SourceConfig::Demo.into()]);
        assert_eq!(file.app, AppSettings::default());
        assert_eq!(file.server, ServerSettings::default());
        assert_eq!(file.youtube, YouTubeSettings::default());
    }

    #[test]
    fn emoji_learning_is_on_unless_switched_off() {
        let without = ConfigFile::parse("[youtube]\nclient_id = \"id\"").unwrap();
        assert!(without.youtube.learn_emojis, "older files: on");
        let off = ConfigFile::parse("[youtube]\nlearn_emojis = false").unwrap();
        assert!(!off.youtube.learn_emojis);
    }

    #[test]
    fn save_load_roundtrip_and_private_file() {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-config-test-{}", std::process::id()));
        let path = dir.join("sub").join("config.toml");
        let mut file = ConfigFile::default();
        file.youtube.client_secret = Some("secret".into());
        file.app.appearance = Appearance::Dark;
        file.sources.push(
            SourceConfig::Twitch {
                channel: "your_channel".into(),
            }
            .into(),
        );
        file.sources.push(SourceEntry {
            config: SourceConfig::Demo,
            enabled: false,
        });

        file.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.matches("enabled").count(),
            1,
            "only written for the switched-off source:\n{text}"
        );
        assert!(text.contains(r#"appearance = "dark""#), "{text}");
        assert_eq!(ConfigFile::load(&path).unwrap(), file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "contains the client secret");
        }
        assert!(
            ConfigFile::write_template(&path).is_err(),
            "never overwrites"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_means_defaults_but_only_for_load_or_default() {
        let path = Path::new("/nonexistent/chat-aggregator/config.toml");
        assert_eq!(
            ConfigFile::load_or_default(path).unwrap(),
            ConfigFile::default()
        );
        assert!(ConfigFile::load(path).is_err());
    }
}
