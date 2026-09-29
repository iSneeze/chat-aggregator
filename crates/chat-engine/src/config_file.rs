//! The settings file (`config.toml`), shared by headless mode and, later,
//! the app. The engine itself only ever *reads* it; saving is up to the
//! caller (the app's settings window, or your text editor).

use std::io::{ErrorKind, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

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
    /// Overlay theme: a built-in theme's name (`default`, `minimal`) or a
    /// folder name in `themes/` next to the config file. `None`: the
    /// default built-in theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    pub stagger_ms: u64,
    pub stagger_max_ms: u64,
    /// The JSON API for other programs. Off by default: any web page open
    /// in the browser could connect to it too.
    pub api: bool,
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
            newest: Newest::Bottom,
        }
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

    /// `settings_dir` is the folder the config file is in: themes are
    /// looked up next to it.
    pub fn into_engine_config(self, settings_dir: &Path) -> EngineConfig {
        let server = self.server;
        EngineConfig {
            // Switched-off sources aren't started; headless mode has no
            // switch to turn them on, the app adds them itself.
            sources: self
                .sources
                .into_iter()
                .filter(|entry| entry.enabled)
                .map(|entry| entry.config)
                .collect(),
            youtube: self.youtube,
            bind: (Ipv4Addr::LOCALHOST, server.port).into(),
            history: server.history,
            theme: server.theme.map_or_else(ThemeSource::default, |name| {
                ThemeSource::named(&Self::themes_dir(settings_dir), &name)
            }),
            themes_dir: Some(Self::themes_dir(settings_dir)),
            stagger: Stagger::new(
                Duration::from_millis(server.stagger_ms),
                Duration::from_millis(server.stagger_max_ms),
            ),
            api: server.api,
            newest: server.newest,
        }
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|e| e.downcast_ref::<std::io::Error>())
        .any(|e| e.kind() == ErrorKind::NotFound)
}

/// Only the current user may read it (Unix: mode 0600; on Windows the
/// user's profile folder already restricts access).
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    let result = (|| {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            if path.exists() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        options.open(path)?.write_all(contents.as_bytes())
    })();
    result.with_context(|| format!("writing {}", path.display()))
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

        let engine = file.into_engine_config(Path::new("/settings"));
        assert_eq!(
            engine.sources,
            [
                twitch("your_channel"),
                SourceConfig::YouTube { video_id: None }
            ],
            "switched-off sources don't start"
        );
        assert_eq!(engine.bind.port(), 8080);
        assert!(engine.api);
        assert_eq!(engine.newest, Newest::Top);
        assert_eq!(engine.history, 5);
        assert_eq!(
            engine.theme,
            ThemeSource::Folder("/settings/themes/cozy".into()),
            "a theme name is a folder in themes/ next to the config file"
        );
        let minimal = ConfigFile {
            server: ServerSettings {
                theme: Some("minimal".into()),
                ..ServerSettings::default()
            },
            ..ConfigFile::default()
        };
        assert_eq!(
            minimal.into_engine_config(Path::new("/settings")).theme,
            ThemeSource::Builtin(crate::Builtin::Minimal),
            "built-in themes by name"
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
