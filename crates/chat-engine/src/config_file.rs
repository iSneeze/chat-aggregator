//! The settings files: `config.toml` with what all profiles share, and a
//! file per profile in `profiles/` next to it. Shared by headless mode and
//! the app.
//!
//! A profile is a setup of its own (a VTuber's corporate and indie
//! persona): its sources, overlay look, tips, chat reactions and widget
//! values ([`ProfileFile`]). `config.toml` ([`ConfigFile`]) holds the rest
//! (server, YouTube's Google project and channels, the app's look) and
//! which profile is active.
//!
//! The engine starts with what's in them and from then on owns the settings
//! ([`Settings`]) and the sources: it applies changes
//! (`EngineHandle::edit_settings`, the source commands), publishes the
//! current state and saves both files after each change (`save_all`).
//! Never at startup, and never in headless mode: a file nobody changed
//! keeps its comments.
//!
//! Versions before profiles kept everything in `config.toml`. Such a file
//! still loads, as the active profile ([`Loaded::old_layout`]); the first
//! save converts it, keeping the old file as `config.old.toml`.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::fs::write_private;
use chat_core::secrets::SecretStore;
use chat_core::tiers::TipSettings;
use chat_reactions::ReactionSettings;

use crate::config::{SourceConfig, YouTubeSettings};
use crate::widgets::Values;
use crate::{DEFAULT_HISTORY, DEFAULT_PORT, EngineConfig, Hide, Newest, Stagger, ThemeSource};

/// A commented `config.toml`, written by `run --init-config`.
pub const TEMPLATE: &str = include_str!("../config.example.toml");
/// A commented profile file, written by `run --init-config` with it.
pub const PROFILE_TEMPLATE: &str = include_str!("../profile.example.toml");

/// The profile there is before any other: what an older `config.toml`
/// becomes, and what a new setup starts with.
pub const DEFAULT_PROFILE: &str = "Main";

/// `config.toml`: what all profiles share, and which one is active.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
// `default`: a missing section or setting takes its default value.
// `deny_unknown_fields`: a typo like `prot = 7878` is an error pointing at
// the line, instead of being silently ignored.
#[serde(default, deny_unknown_fields)]
pub struct ConfigFile {
    /// The active profile: `profiles/<profile>.toml`. First: a plain value
    /// after a table would belong to that table.
    pub profile: String,
    pub server: ServerSettings,
    pub youtube: YouTubeSettings,
    pub app: AppSettings,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            profile: DEFAULT_PROFILE.into(),
            server: ServerSettings::default(),
            youtube: YouTubeSettings::default(),
            app: AppSettings::default(),
        }
    }
}

/// A profile's file (`profiles/<name>.toml`): its settings and sources.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileFile {
    pub overlay: OverlaySettings,
    /// Only written when changed from the defaults.
    #[serde(skip_serializing_if = "TipSettings::is_default")]
    pub tips: TipSettings,
    /// Before the sources: a `[reactions]` table written after the
    /// `[[sources]]` blocks would read as part of the last one.
    pub reactions: ReactionSettings,
    /// `[widgets.<name>]`: the values picked for each widget's settings.
    /// Only written when there are some.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub widgets: BTreeMap<String, Values>,
    pub sources: Vec<SourceEntry>,
}

/// All the settings but the sources (the engine runs those, see
/// `EngineHandle::add_source`): `config.toml`'s, and the active profile's
/// in `profile`. The engine owns them while it runs: read them with
/// `EngineHandle::settings`, change them with `EngineHandle::edit_settings`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub server: ServerSettings,
    pub youtube: YouTubeSettings,
    pub app: AppSettings,
    /// The active profile's settings: they change with the profile.
    pub profile: ProfileSettings,
}

/// A profile's settings (its sources aside).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfileSettings {
    pub overlay: OverlaySettings,
    pub tips: TipSettings,
    pub reactions: ReactionSettings,
    /// The values picked in the app for each widget's settings, by widget
    /// name, only those that differ from the widget's defaults. Nothing
    /// running reads them: the app's widget form and its "Copy URL" do (the
    /// values travel in the widget's URL, see `widgets::Widget::url`).
    pub widgets: BTreeMap<String, Values>,
}

impl ProfileFile {
    /// Taken apart into the settings and the sources.
    pub fn split(self) -> (ProfileSettings, Vec<SourceEntry>) {
        let ProfileFile {
            overlay,
            tips,
            reactions,
            widgets,
            sources,
        } = self;
        let settings = ProfileSettings {
            overlay,
            tips,
            reactions,
            widgets,
        };
        (settings, sources)
    }

    /// Put together from the settings and the sources.
    pub fn new(settings: ProfileSettings, sources: Vec<SourceEntry>) -> Self {
        // Unpacked without `..`: a setting added later has to be placed
        // here, or this doesn't compile (and it would be lost on saving).
        let ProfileSettings {
            overlay,
            tips,
            reactions,
            widgets,
        } = settings;
        Self {
            overlay,
            tips,
            reactions,
            widgets,
            sources,
        }
    }
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
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            history: DEFAULT_HISTORY,
            stagger_ms: 250,
            stagger_max_ms: 2000,
            api: false,
            api_origins: Vec::new(),
        }
    }
}

impl ServerSettings {
    /// The overlay's burst pacing.
    pub fn stagger(&self) -> Stagger {
        Stagger::new(
            Duration::from_millis(self.stagger_ms),
            Duration::from_millis(self.stagger_max_ms),
        )
    }
}

/// How a profile's overlay looks (`[overlay]` in its file).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverlaySettings {
    /// Overlay theme: a built-in theme's name (`default`, `minimal`), a
    /// folder name in `themes/` next to the config file, or the full path of
    /// a theme folder anywhere. `None`: the default built-in theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Which end of the overlay new messages appear at, unless the overlay
    /// URL says otherwise (`?newest=top`).
    pub newest: Newest,
    /// Platforms and kinds of message the overlay leaves out, e.g.
    /// `["streamelements", "notice"]`, unless the overlay URL says otherwise
    /// (`?hide=…`). The JSON API and chat reactions still get them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<Hide>,
}

impl OverlaySettings {
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
}

/// `config.toml` as versions before profiles wrote it: everything in one
/// file, the overlay's settings in `[server]`. Only read, never written.
#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct OldConfigFile {
    server: OldServerSettings,
    youtube: YouTubeSettings,
    app: AppSettings,
    tips: TipSettings,
    reactions: ReactionSettings,
    widgets: BTreeMap<String, Values>,
    sources: Vec<SourceEntry>,
}

/// The old `[server]`: today's, with the overlay's settings in it. Spelled
/// out rather than `flatten`ed: serde can't refuse unknown keys next to a
/// flattened struct, and a typo must stay an error.
#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OldServerSettings {
    port: u16,
    history: usize,
    theme: Option<String>,
    stagger_ms: u64,
    stagger_max_ms: u64,
    api: bool,
    api_origins: Vec<String>,
    newest: Newest,
    hide: Vec<Hide>,
}

impl Default for OldServerSettings {
    fn default() -> Self {
        let server = ServerSettings::default();
        let overlay = OverlaySettings::default();
        Self {
            port: server.port,
            history: server.history,
            theme: overlay.theme,
            stagger_ms: server.stagger_ms,
            stagger_max_ms: server.stagger_max_ms,
            api: server.api,
            api_origins: server.api_origins,
            newest: overlay.newest,
            hide: overlay.hide,
        }
    }
}

impl OldConfigFile {
    /// The same settings, in today's two files.
    fn split(self) -> (ConfigFile, ProfileFile) {
        let OldServerSettings {
            port,
            history,
            theme,
            stagger_ms,
            stagger_max_ms,
            api,
            api_origins,
            newest,
            hide,
        } = self.server;
        let config = ConfigFile {
            profile: DEFAULT_PROFILE.into(),
            server: ServerSettings {
                port,
                history,
                stagger_ms,
                stagger_max_ms,
                api,
                api_origins,
            },
            youtube: self.youtube,
            app: self.app,
        };
        let profile = ProfileFile {
            overlay: OverlaySettings {
                theme,
                newest,
                hide,
            },
            tips: self.tips,
            reactions: self.reactions,
            widgets: self.widgets,
            sources: self.sources,
        };
        (config, profile)
    }
}

/// What an older `config.toml` has that today's keeps in a profile.
const OLD_TOP_LEVEL: [&str; 4] = ["tips", "reactions", "widgets", "sources"];
const OLD_IN_SERVER: [&str; 3] = ["theme", "newest", "hide"];

/// What the settings folder holds, as read at startup: `config.toml` and
/// the active profile.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Loaded {
    pub config: ConfigFile,
    pub profile: ProfileFile,
    /// `config.toml` is still in the layout of versions before profiles
    /// (the profile came from it): the first save converts it.
    pub old_layout: bool,
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

    /// Reads `config.toml` at `path` and the active profile's file next to
    /// it: the one `config.toml` names, or `profile` (headless `--profile`).
    /// A profile without a file (a new setup) has the default settings and
    /// no sources.
    pub fn load(path: &Path, profile: Option<&str>) -> anyhow::Result<Loaded> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::load_text(&text, path, profile)
    }

    /// Like [`load`](Self::load), but a missing `config.toml` just means
    /// defaults (and the profile's file is still looked for).
    pub fn load_or_default(path: &Path, profile: Option<&str>) -> anyhow::Result<Loaded> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::load_text(&text, path, profile),
            Err(e) if e.kind() == ErrorKind::NotFound => Self::load_text("", path, profile),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn load_text(text: &str, path: &Path, profile: Option<&str>) -> anyhow::Result<Loaded> {
        let (mut config, old) =
            Self::parse(text).with_context(|| format!("in {}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new(""));
        if let Some(name) = profile {
            config.profile = normalize_profile_name(name)
                .map_err(anyhow::Error::msg)
                .context("the profile asked for")?;
        }
        let (profile, old_layout) = match old {
            // The old file holds the active profile, unless another was
            // asked for (the old file is the only one older versions knew).
            Some(old) if same_name(&config.profile, DEFAULT_PROFILE) => (old, true),
            Some(_) => bail!(
                "{} is from a version before profiles, so it holds only its own profile: open \
                 the app once to convert it",
                path.display()
            ),
            None => {
                let profiles_dir = Self::profiles_dir(dir);
                // The file's own spelling wins ("main.toml" for `profile =
                // "Main"`): saved under it, the profile stays one file.
                if let Some(path) = find_profile_file(&profiles_dir, &config.profile)
                    && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                {
                    config.profile = stem.to_string();
                }
                let profile = ProfileFile::load_or_default(&profiles_dir, &config.profile)?;
                (profile, false)
            }
        };
        Ok(Loaded {
            config,
            profile,
            old_layout,
        })
    }

    /// Reads a `config.toml`. With the layout of versions before profiles,
    /// the profile it holds comes along.
    pub fn parse(text: &str) -> anyhow::Result<(Self, Option<ProfileFile>)> {
        // A first, loose look (any TOML) says which layout this is; then
        // the strict one, straight from the text, whose errors name the
        // line.
        let table: toml::Table = toml::from_str(text)?;
        let server = table.get("server").and_then(toml::Value::as_table);
        let old_keys: Vec<&str> = OLD_TOP_LEVEL
            .iter()
            .filter(|key| table.contains_key(**key))
            .chain(
                OLD_IN_SERVER
                    .iter()
                    .filter(|key| server.is_some_and(|s| s.contains_key(**key))),
            )
            .copied()
            .collect();
        if old_keys.is_empty() {
            let mut file: Self = toml::from_str(text)?;
            // It's a file name: nothing like "../elsewhere".
            file.profile = normalize_profile_name(&file.profile)
                .map_err(anyhow::Error::msg)
                .context("profile")?;
            return Ok((file, None));
        }
        // `profile` is only in files written since profiles exist, so this
        // is a hand edit: say where it goes now, rather than ignoring it.
        if let Some(profile) = table.get("profile").and_then(toml::Value::as_str) {
            bail!(
                "{} belong in the profile's own file now (profiles/{profile}.toml next to \
                 config.toml), not in config.toml",
                old_keys.join(", ")
            );
        }
        let old: OldConfigFile = toml::from_str(text)?;
        let (config, profile) = old.split();
        profile.check()?;
        Ok((config, Some(profile)))
    }

    /// Writes the file, readable only by you (it contains the client
    /// secret). Comments in an existing file are not kept.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_private(path, &toml::to_string_pretty(self)?)
    }

    /// Writes the commented templates (`config.toml` at `path` and its
    /// profile), unless either exists already.
    pub fn write_template(path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().unwrap_or(Path::new(""));
        let profile = Self::profile_path(dir, DEFAULT_PROFILE);
        for file in [path, &profile] {
            if file.exists() {
                bail!("{} already exists; not overwriting it", file.display());
            }
        }
        write_private(&profile, PROFILE_TEMPLATE)?;
        write_private(path, TEMPLATE)
    }

    /// The folder with the overlay themes, next to the config file in
    /// `settings_dir`.
    pub fn themes_dir(settings_dir: &Path) -> PathBuf {
        settings_dir.join("themes")
    }

    /// The folder with the imported widgets, next to the config file in
    /// `settings_dir`.
    pub fn widgets_dir(settings_dir: &Path) -> PathBuf {
        settings_dir.join("widgets")
    }

    /// The folder with the profiles' files, next to the config file in
    /// `settings_dir`.
    pub fn profiles_dir(settings_dir: &Path) -> PathBuf {
        settings_dir.join("profiles")
    }

    /// Where profile `name`'s file goes, in the settings folder
    /// `settings_dir`.
    pub fn profile_path(settings_dir: &Path, name: &str) -> PathBuf {
        Self::profiles_dir(settings_dir).join(format!("{name}.toml"))
    }
}

impl Loaded {
    /// `settings_dir` is the folder the config file is in: themes and the
    /// emoji caches are kept next to it.
    pub fn into_engine_config(self, settings_dir: &Path) -> EngineConfig {
        let Loaded {
            config,
            profile,
            old_layout,
        } = self;
        let (profile_settings, sources) = profile.split();
        EngineConfig {
            settings: Settings {
                server: config.server,
                youtube: config.youtube,
                app: config.app,
                profile: profile_settings,
            },
            sources,
            profile: config.profile,
            old_layout,
            themes_dir: Some(ConfigFile::themes_dir(settings_dir)),
            widgets_dir: Some(ConfigFile::widgets_dir(settings_dir)),
            // Unlike `config.toml`, written in headless mode too: it's
            // data the sources collect, not a file you edit.
            emoji_cache_dir: Some(settings_dir.join("youtube-emojis")),
            // Headless mode has nobody to ask, and leaves the file alone;
            // the app switches both on.
            ask_api_origins: false,
            save_to: None,
            secrets: Some(SecretStore::System),
        }
    }
}

impl ProfileFile {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        // toml's error message already names the line and shows it.
        let file: Self = toml::from_str(text)?;
        file.check()?;
        Ok(file)
    }

    /// What serde can't see: a rule that could never fire (an empty
    /// command, two rules with one name), tiers out of order. An error too,
    /// like a typo.
    fn check(&self) -> anyhow::Result<()> {
        self.tips.check().map_err(anyhow::Error::msg)?;
        self.reactions.check().map_err(anyhow::Error::msg)?;
        Ok(())
    }

    /// Profile `name`'s file in `profiles_dir`. Found whatever the case of
    /// its name (case-insensitive systems don't tell "main" and "Main"
    /// apart, so neither does this).
    pub fn load(profiles_dir: &Path, name: &str) -> anyhow::Result<Self> {
        let path = find_profile_file(profiles_dir, name)
            .with_context(|| format!("there's no profile named \"{name}\""))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Like [`load`](Self::load), but without a file the profile is new:
    /// default settings, no sources.
    pub fn load_or_default(profiles_dir: &Path, name: &str) -> anyhow::Result<Self> {
        if find_profile_file(profiles_dir, name).is_none() {
            return Ok(Self::default());
        }
        Self::load(profiles_dir, name)
    }

    /// Writes the file, readable only by you (like `config.toml`).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_private(path, &toml::to_string_pretty(self)?)
    }
}

/// Writes the settings: the profile's file first, then `config.toml`, which
/// names it, so `config.toml` never points at a profile that isn't written
/// yet. Each file is written whole or not at all (`write_private`).
///
/// `old_layout`: `config.toml` is still a file from before profiles. It's
/// copied to `config.old.toml` first (once: an existing copy stays), for
/// going back to an older version. Should this stop halfway, `config.toml`
/// is still the old file, which loads as before and is converted at the
/// next save.
pub fn save_all(
    config_path: &Path,
    config: &ConfigFile,
    profile: &ProfileFile,
    old_layout: bool,
) -> anyhow::Result<()> {
    let dir = config_path.parent().unwrap_or(Path::new(""));
    profile.save(&ConfigFile::profile_path(dir, &config.profile))?;
    if old_layout {
        let backup = dir.join("config.old.toml");
        if !backup.exists() {
            let old = std::fs::read_to_string(config_path)
                .with_context(|| format!("reading {}", config_path.display()))?;
            write_private(&backup, &old)?;
        }
    }
    config.save(config_path)
}

/// Checks a profile's name and tidies it (outer spaces go). It's a file
/// name too, so: 1 to 40 letters, digits, spaces, `-` and `_`, and none of
/// the names Windows keeps for itself (`CON`, `NUL`, `COM1`, …).
pub fn normalize_profile_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the profile needs a name".into());
    }
    if name.chars().count() > 40 {
        return Err("a profile's name can be 40 characters at most".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_alphanumeric() || matches!(c, ' ' | '-' | '_')))
    {
        return Err(format!(
            "a profile's name can only have letters, digits, spaces, - and _ (not {c:?})"
        ));
    }
    if chat_core::fs::is_windows_reserved_name(name) {
        return Err(format!(
            "\"{name}\" can't be a profile's name (Windows keeps it for itself)"
        ));
    }
    Ok(name.to_string())
}

/// Whether two profile names are the same profile: case doesn't count, as
/// on the file systems of Windows and macOS.
pub fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The profiles with a file in `profiles_dir`, sorted by name. Files whose
/// names couldn't be profiles' are left out.
pub fn list_profiles(profiles_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(profiles_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? != "toml" || !entry.file_type().ok()?.is_file() {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            normalize_profile_name(stem)
                .ok()
                .filter(|name| name == stem)
        })
        .collect();
    sort_profiles(&mut names);
    names
}

/// Sorts profile names as people read them: case doesn't count.
pub fn sort_profiles(names: &mut [String]) {
    names.sort_by_key(|name| name.to_lowercase());
}

/// The file of profile `name` in `profiles_dir`, whatever its case.
pub(crate) fn find_profile_file(profiles_dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = profiles_dir.join(format!("{name}.toml"));
    if exact.is_file() {
        return Some(exact);
    }
    // A case-sensitive system (Linux), with the file's name in another
    // case: e.g. renamed from "main" to "Main", and stopped before
    // `config.toml` said so.
    std::fs::read_dir(profiles_dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .find(|path| {
            path.is_file()
                && path.extension().is_some_and(|ext| ext == "toml")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| same_name(stem, name))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::tiers::Amount;

    /// A settings folder of its own (`name` keeps tests apart), empty.
    fn settings_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn twitch(channel: &str) -> SourceConfig {
        SourceConfig::Twitch {
            channel: channel.into(),
        }
    }

    /// Everything an older `config.toml` could hold, in its layout.
    const OLD_FULL: &str = r#"
        [server]
        port = 8080
        history = 5
        theme = "cozy"
        stagger_ms = 100
        stagger_max_ms = 1000
        api = true
        api_origins = ["https://game.example"]
        newest = "top"
        hide = ["streamelements", "notice"]

        [youtube]
        client_id = "id"
        client_secret = "secret"

        [[youtube.accounts]]
        channel_id = "UC_your_channel"
        name = "your_channel"

        [[sources]]
        type = "twitch"
        channel = "your_channel"

        [app]
        appearance = "high-contrast"

        [tips]
        currency = "EUR"

        [reactions]
        warudo = { url = "ws://192.168.1.20:19190" }
        obs = { url = "ws://127.0.0.1:4456" }

        [[reactions.rules]]
        name = "bonk"
        trigger = { type = "command", command = "!bonk" }
        action = { target = "warudo", action = "throw_item" }

        [widgets.live-emoji]
        size = 72

        [[sources]]
        type = "twitch"
        channel = "other_channel"
        enabled = false

        [[sources]]
        type = "youtube"
        account = "UC_your_channel"
        "#;

    #[test]
    fn full_files_parse() {
        let (file, old) = ConfigFile::parse(
            r#"
            profile = "Indie"

            [server]
            port = 8080
            history = 5
            stagger_ms = 100
            stagger_max_ms = 1000
            api = true
            api_origins = ["https://game.example"]

            [youtube]
            client_id = "id"
            client_secret = "secret"

            [app]
            appearance = "high-contrast"
            "#,
        )
        .unwrap();
        assert_eq!(old, None, "today's layout");
        assert_eq!(file.profile, "Indie");
        assert_eq!(file.server.port, 8080);
        assert_eq!(file.youtube.client_id.as_deref(), Some("id"));
        assert_eq!(file.app.appearance, Appearance::HighContrast);

        let profile = ProfileFile::parse(
            r#"
            [overlay]
            theme = "cozy"
            newest = "top"
            hide = ["streamelements", "notice"]

            [reactions]
            warudo = { url = "ws://192.168.1.20:19190" }

            [[reactions.rules]]
            name = "bonk"
            trigger = { type = "command", command = "!bonk" }
            action = { target = "warudo", action = "throw_item" }

            [[sources]]
            type = "twitch"
            channel = "your_channel"

            [[sources]]
            type = "twitch"
            channel = "other_channel"
            enabled = false

            [[sources]]
            type = "youtube"
            "#,
        )
        .unwrap();
        assert_eq!(profile.overlay.theme.as_deref(), Some("cozy"));
        assert_eq!(profile.overlay.newest, Newest::Top);
        assert_eq!(profile.overlay.hide, [Hide::StreamElements, Hide::Notice]);
        assert_eq!(profile.reactions.rules[0].name, "bonk");
        assert_eq!(
            profile.sources,
            [
                twitch("your_channel").into(),
                SourceEntry {
                    config: twitch("other_channel"),
                    enabled: false,
                },
                SourceConfig::YouTube {
                    video_id: None,
                    account: None,
                }
                .into(),
            ]
        );

        let sources = profile.sources.clone();
        let loaded = Loaded {
            config: file,
            profile,
            old_layout: false,
        };
        let engine = loaded.into_engine_config(Path::new("/settings"));
        assert_eq!(engine.sources, sources, "switched-off ones too: listed");
        assert_eq!(engine.profile, "Indie");
        assert!(!engine.old_layout);
        assert!(!engine.ask_api_origins, "headless mode has nobody to ask");
        assert_eq!(engine.save_to, None, "headless mode leaves the file alone");
        assert_eq!(
            engine.secrets,
            Some(SecretStore::System),
            "the OBS password is looked for in the keyring"
        );
        let server = &engine.settings.server;
        assert_eq!(server.port, 8080);
        assert!(server.api);
        assert_eq!(server.api_origins, ["https://game.example"]);
        assert_eq!(server.history, 5);
        assert_eq!(engine.settings.app.appearance, Appearance::HighContrast);
        let profile = &engine.settings.profile;
        assert_eq!(profile.overlay.newest, Newest::Top);
        assert_eq!(profile.reactions.warudo.url, "ws://192.168.1.20:19190");
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
    fn an_old_config_file_becomes_the_main_profile_with_nothing_lost() {
        let (config, profile) = ConfigFile::parse(OLD_FULL).unwrap();
        let profile = profile.expect("the old layout holds a profile");
        assert_eq!(config.profile, DEFAULT_PROFILE);
        assert_eq!(
            config.server,
            ServerSettings {
                port: 8080,
                history: 5,
                stagger_ms: 100,
                stagger_max_ms: 1000,
                api: true,
                api_origins: vec!["https://game.example".into()],
            }
        );
        assert_eq!(config.youtube.client_secret.as_deref(), Some("secret"));
        assert_eq!(config.youtube.accounts[0].channel_id, "UC_your_channel");
        assert_eq!(config.app.appearance, Appearance::HighContrast);
        assert_eq!(
            profile.overlay,
            OverlaySettings {
                theme: Some("cozy".into()),
                newest: Newest::Top,
                hide: vec![Hide::StreamElements, Hide::Notice],
            }
        );
        assert_eq!(profile.tips.currency(), Some("EUR"));
        assert_eq!(profile.reactions.obs.url, "ws://127.0.0.1:4456");
        assert_eq!(profile.reactions.rules[0].name, "bonk");
        assert_eq!(
            profile.widgets["live-emoji"]["size"],
            crate::widgets::Value::Number(72.0)
        );
        assert_eq!(profile.sources.len(), 3);
        assert_eq!(
            profile.sources[2].config,
            SourceConfig::YouTube {
                video_id: None,
                account: Some("UC_your_channel".into()),
            }
        );

        // Written in today's layout and read back: the same settings. (So
        // nothing an old file holds can get lost in the move: a field
        // forgotten in the split would differ here.)
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(ConfigFile::parse(&text).unwrap(), (config, None));
        let text = toml::to_string_pretty(&profile).unwrap();
        assert_eq!(ProfileFile::parse(&text).unwrap(), profile);
        assert!(text.contains("[overlay]"), "{text}");

        // Only overlay settings in [server] are enough to tell.
        let (config, profile) = ConfigFile::parse("[server]\nnewest = \"top\"").unwrap();
        assert_eq!(profile.unwrap().overlay.newest, Newest::Top);
        assert_eq!(config.server, ServerSettings::default());
    }

    #[test]
    fn profile_settings_in_config_toml_are_an_error_once_it_names_a_profile() {
        // A hand edit mixing both layouts: say where the setting goes now,
        // rather than quietly ignoring the profile's file.
        let err = ConfigFile::parse("profile = \"Indie\"\n[server]\ntheme = \"cozy\"").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("theme") && message.contains("profiles/Indie.toml"),
            "{message}"
        );
        let err =
            ConfigFile::parse("profile = \"Main\"\n[[sources]]\ntype = \"demo\"").unwrap_err();
        assert!(format!("{err:#}").contains("sources"), "{err:#}");
    }

    #[test]
    fn theme_settings_name_or_point_at_a_theme() {
        let themes_dir = Path::new("/settings/themes");
        let theme = |name: &str| {
            OverlaySettings {
                theme: Some(name.into()),
                ..OverlaySettings::default()
            }
            .theme_source(Some(themes_dir))
        };
        assert_eq!(
            OverlaySettings::default().theme_source(Some(themes_dir)),
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
        let (file, old) = ConfigFile::parse("").unwrap();
        assert_eq!((file.clone(), old), (ConfigFile::default(), None));
        assert_eq!(file.profile, DEFAULT_PROFILE);
        assert_eq!(file.server.port, DEFAULT_PORT);
        assert!(!file.server.api, "the JSON API is off unless asked for");

        let (file, _) = ConfigFile::parse("[server]\nport = 9000").unwrap();
        assert_eq!(file.server.port, 9000);
        assert_eq!(
            file.server.history, DEFAULT_HISTORY,
            "rest of the section stays default"
        );
        assert_eq!(ProfileFile::parse("").unwrap(), ProfileFile::default());
    }

    #[test]
    fn typos_are_errors_that_name_the_line() {
        let err = ConfigFile::parse("[server]\nprot = 9000").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("prot") && message.contains("line 2"),
            "{message}"
        );
        // In an old file too: telling the layouts apart doesn't loosen it.
        let err = ConfigFile::parse("[server]\ntheme = \"cozy\"\nprot = 9000").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("prot") && message.contains("line 3"),
            "{message}"
        );
        let err = ConfigFile::parse("[[sources]]\ntype = \"twich\"").unwrap_err();
        assert!(format!("{err:#}").contains("twich"), "{err:#}");
        let err = ConfigFile::parse("profle = \"Main\"").unwrap_err();
        assert!(format!("{err:#}").contains("profle"), "{err:#}");
        let err = ConfigFile::parse("profile = \"../Main\"").unwrap_err();
        assert!(format!("{err:#}").contains("profile"), "{err:#}");

        let err = ProfileFile::parse("[[sources]]\ntype = \"twich\"").unwrap_err();
        assert!(format!("{err:#}").contains("twich"), "{err:#}");
        // Unlike in an overlay URL, an unknown name to hide is an error here:
        // the app can say so.
        let err = ProfileFile::parse("[overlay]\nhide = [\"stremelements\"]").unwrap_err();
        assert!(format!("{err:#}").contains("stremelements"), "{err:#}");

        // Inside a source and in [youtube] too: a typo must not silently
        // change what gets watched.
        let err =
            ProfileFile::parse("[[sources]]\ntype = \"youtube\"\nvidoe_id = \"x\"").unwrap_err();
        assert!(format!("{err:#}").contains("vidoe_id"), "{err:#}");
        let err = ConfigFile::parse("[youtube]\nclient_secert = \"x\"").unwrap_err();
        assert!(format!("{err:#}").contains("client_secert"), "{err:#}");
        // (Not caught next to `type = "demo"`: serde doesn't check extra
        // keys for variants without fields. Harmless for the demo.)
        let err =
            ProfileFile::parse("[[sources]]\ntype = \"twitch\"\nchannel = \"x\"\nenabeld = false")
                .unwrap_err();
        assert!(format!("{err:#}").contains("enabeld"), "{err:#}");
        let err = ConfigFile::parse("[app]\nappearance = \"blue\"").unwrap_err();
        assert!(format!("{err:#}").contains("blue"), "{err:#}");
        let err = ProfileFile::parse("[reactions]\npasued = true").unwrap_err();
        assert!(format!("{err:#}").contains("pasued"), "{err:#}");
        // Today's server settings no longer take the overlay's.
        let err = ProfileFile::parse("[server]\nport = 1").unwrap_err();
        assert!(format!("{err:#}").contains("server"), "{err:#}");
    }

    #[test]
    fn reaction_rules_that_cant_work_are_errors() {
        let rule = |name: &str, command: &str| {
            format!(
                "[[reactions.rules]]\nname = \"{name}\"\n\
                 trigger = {{ type = \"command\", command = \"{command}\" }}\n\
                 action = {{ target = \"warudo\", action = \"x\" }}\n"
            )
        };
        assert!(ProfileFile::parse(&rule("a", "!a")).is_ok());
        let err = ProfileFile::parse(&(rule("a", "!a") + &rule("a", "!b"))).unwrap_err();
        assert!(format!("{err:#}").contains("unique"), "{err:#}");
        assert!(ProfileFile::parse(&rule("a", "")).is_err());
        // In an old config.toml just the same.
        assert!(ConfigFile::parse(&(rule("a", "!a") + &rule("a", "!b"))).is_err());
    }

    #[test]
    fn the_templates_are_valid() {
        let (file, old) = ConfigFile::parse(TEMPLATE).unwrap();
        assert_eq!(old, None, "the template has today's layout");
        assert_eq!(file, ConfigFile::default());
        let profile = ProfileFile::parse(PROFILE_TEMPLATE).unwrap();
        assert_eq!(profile.sources, [SourceConfig::Demo.into()]);
        assert_eq!(profile.overlay, OverlaySettings::default());
        assert_eq!(profile.tips, TipSettings::default());
        assert_eq!(profile.reactions, ReactionSettings::default());
        assert!(profile.widgets.is_empty());
    }

    #[test]
    fn widget_values_keep_their_types_and_are_only_written_when_there_are_some() {
        use crate::widgets::Value;

        // Hand-written: a whole number is a number too.
        let file = ProfileFile::parse(
            "[widgets.live-emoji]\nmode = \"calm\"\nsize = 72\nlife = 2.5\nglow = false\n",
        )
        .unwrap();
        let values = &file.widgets["live-emoji"];
        assert_eq!(values["mode"], Value::Text("calm".into()));
        assert_eq!(values["size"], Value::Number(72.0));
        assert_eq!(values["life"], Value::Number(2.5));
        assert_eq!(values["glow"], Value::Bool(false));

        // Written and read back as they were.
        let text = toml::to_string_pretty(&file).unwrap();
        assert_eq!(ProfileFile::parse(&text).unwrap(), file);
        // No widgets: no `[widgets]` table at all.
        let text = toml::to_string_pretty(&ProfileFile::default()).unwrap();
        assert!(!text.contains("widgets"), "{text}");
        // Something that's none of the types is an error, like a typo.
        assert!(ProfileFile::parse("[widgets.live-emoji]\nsize = [1, 2]\n").is_err());
    }

    #[test]
    fn tip_settings_parse_and_are_checked() {
        let file =
            ProfileFile::parse("[tips]\ncurrency = \"EUR\"\ntiers = [3, 6, 9, 69, 100, 420.5]")
                .unwrap();
        assert_eq!(file.tips.currency(), Some("EUR"));
        assert_eq!(file.tips.tier(Amount::whole(69)), 5);
        assert_eq!(file.tips.tiers[5], Amount::of(420.5).unwrap());
        for bad in [
            "[tips]\ncurrency = \"euro\"",
            "[tips]\ntiers = [2, 5, 10, 20, 50]",
            "[tips]\ntiers = [2, 5, 10, 9, 50, 100]",
            "[tips]\ntiers = [2, 5, 10, 20, 50, -1]",
        ] {
            assert!(ProfileFile::parse(bad).is_err(), "{bad}");
            assert!(ConfigFile::parse(bad).is_err(), "old layout: {bad}");
        }
    }

    #[test]
    fn default_tip_settings_stay_out_of_the_file() {
        let mut file = ProfileFile::default();
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(!text.contains("[tips]"), "{text}");
        file.tips.currency = "EUR".into();
        let text = toml::to_string_pretty(&file).unwrap();
        // Written as typed: whole amounts as whole numbers (`2`, not
        // `2.0`). Read back as TOML values, so the layout doesn't matter.
        let written: toml::Table = text.parse().unwrap();
        assert_eq!(written["tips"]["tiers"][0].as_integer(), Some(2), "{text}");
        assert_eq!(ProfileFile::parse(&text).unwrap(), file);
    }

    #[test]
    fn emoji_learning_is_on_unless_switched_off() {
        let (without, _) = ConfigFile::parse("[youtube]\nclient_id = \"id\"").unwrap();
        assert!(without.youtube.learn_emojis, "older files: on");
        let (off, _) = ConfigFile::parse("[youtube]\nlearn_emojis = false").unwrap();
        assert!(!off.youtube.learn_emojis);
    }

    #[test]
    fn save_load_roundtrip_and_private_files() {
        let dir = settings_dir("roundtrip");
        let path = dir.join("sub").join("config.toml");
        let mut config = ConfigFile {
            profile: "Indie".into(),
            ..ConfigFile::default()
        };
        config.youtube.client_secret = Some("secret".into());
        config.app.appearance = Appearance::Dark;
        let mut profile = ProfileFile::default();
        profile.overlay.newest = Newest::Top;
        profile.reactions = toml::from_str(
            r#"
            [[rules]]
            name = "bonk"
            trigger = { type = "command", command = "!bonk" }
            guards = { min_role = "moderator", cooldown_secs = 10 }
            action = { target = "warudo", action = "throw_item" }
            "#,
        )
        .unwrap();
        profile.sources.push(twitch("your_channel").into());
        profile.sources.push(SourceEntry {
            config: SourceConfig::Demo,
            enabled: false,
        });

        save_all(&path, &config, &profile, false).unwrap();
        let profile_path = dir.join("sub/profiles/Indie.toml");
        let text = std::fs::read_to_string(&profile_path).unwrap();
        assert_eq!(
            text.matches("enabled").count(),
            1,
            "only written for the switched-off source:\n{text}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"appearance = "dark""#), "{text}");
        assert!(text.starts_with(r#"profile = "Indie""#), "{text}");
        assert_eq!(
            ConfigFile::load(&path, None).unwrap(),
            Loaded {
                config,
                profile,
                old_layout: false,
            }
        );
        assert!(!dir.join("sub/config.old.toml").exists(), "nothing to keep");
        #[cfg(unix)]
        for file in [&path, &profile_path] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "contains the client secret");
        }
        assert!(
            ConfigFile::write_template(&path).is_err(),
            "never overwrites"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_templates_are_written_together() {
        let dir = settings_dir("template");
        let path = dir.join("config.toml");
        ConfigFile::write_template(&path).unwrap();
        let loaded = ConfigFile::load(&path, None).unwrap();
        assert_eq!(loaded.config.profile, DEFAULT_PROFILE);
        assert_eq!(loaded.profile.sources, [SourceConfig::Demo.into()]);
        assert!(!loaded.old_layout);
        // Not over a profile that's there already either.
        std::fs::remove_file(&path).unwrap();
        assert!(ConfigFile::write_template(&path).is_err());
        assert!(!path.exists(), "neither file written");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_old_file_is_converted_at_the_first_save_and_kept_as_config_old_toml() {
        let dir = settings_dir("convert");
        let path = dir.join("config.toml");
        std::fs::write(&path, OLD_FULL).unwrap();

        // Read as it is: nothing written yet (headless mode stops here).
        let loaded = ConfigFile::load(&path, None).unwrap();
        assert!(loaded.old_layout);
        assert_eq!(loaded.config.profile, DEFAULT_PROFILE);
        assert_eq!(loaded.profile.sources.len(), 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD_FULL);
        assert!(!dir.join("profiles").exists());

        save_all(&path, &loaded.config, &loaded.profile, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("config.old.toml")).unwrap(),
            OLD_FULL,
            "the old file, as it was"
        );
        let again = ConfigFile::load(&path, None).unwrap();
        assert!(!again.old_layout, "converted");
        assert_eq!(again.config, loaded.config);
        assert_eq!(again.profile, loaded.profile, "nothing lost");
        assert!(dir.join("profiles/Main.toml").is_file());

        // A copy made earlier stays: it's the one from before profiles.
        std::fs::write(&path, "[server]\ntheme = \"cozy\"").unwrap();
        let loaded = ConfigFile::load(&path, None).unwrap();
        save_all(&path, &loaded.config, &loaded.profile, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("config.old.toml")).unwrap(),
            OLD_FULL
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_profile_can_be_asked_for() {
        let dir = settings_dir("ask");
        let path = dir.join("config.toml");
        let indie = ProfileFile {
            sources: vec![twitch("your_channel").into()],
            ..ProfileFile::default()
        };
        save_all(
            &path,
            &ConfigFile::default(),
            &ProfileFile::default(),
            false,
        )
        .unwrap();
        indie
            .save(&ConfigFile::profile_path(&dir, "Indie"))
            .unwrap();

        let loaded = ConfigFile::load(&path, Some(" Indie ")).unwrap();
        assert_eq!(loaded.config.profile, "Indie");
        assert_eq!(loaded.profile, indie);
        // Found whatever the case, and then called as its file is: saved
        // under that name, it stays one file.
        let loaded = ConfigFile::load(&path, Some("indie")).unwrap();
        assert_eq!(loaded.config.profile, "Indie");
        assert_eq!(loaded.profile, indie);
        // A new one: no file yet, so nothing in it.
        let loaded = ConfigFile::load(&path, Some("Collab")).unwrap();
        assert_eq!(loaded.config.profile, "Collab");
        assert_eq!(loaded.profile, ProfileFile::default());
        assert!(ConfigFile::load(&path, Some("../Main")).is_err());

        // An old file only knows its own profile.
        std::fs::write(&path, OLD_FULL).unwrap();
        assert!(ConfigFile::load(&path, Some("main")).unwrap().old_layout);
        let err = ConfigFile::load(&path, Some("Indie")).unwrap_err();
        assert!(format!("{err:#}").contains("before profiles"), "{err:#}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_means_defaults_but_only_for_load_or_default() {
        let path = Path::new("/nonexistent/chat-aggregator/config.toml");
        assert_eq!(
            ConfigFile::load_or_default(path, None).unwrap(),
            Loaded::default()
        );
        assert!(ConfigFile::load(path, None).is_err());
    }

    #[test]
    fn a_broken_profile_file_is_an_error_naming_it() {
        let dir = settings_dir("broken");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(ConfigFile::profiles_dir(&dir)).unwrap();
        std::fs::write(
            ConfigFile::profile_path(&dir, "Main"),
            "[overlay]\nnewst = 1",
        )
        .unwrap();
        let err = ConfigFile::load_or_default(&path, None).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("Main.toml") && message.contains("newst"),
            "{message}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn profile_names_are_file_names_too() {
        assert_eq!(normalize_profile_name("  Indie  ").unwrap(), "Indie");
        assert_eq!(normalize_profile_name("Main_2 – no").ok(), None);
        assert_eq!(
            normalize_profile_name("VTuber-Collab 2").unwrap(),
            "VTuber-Collab 2"
        );
        assert_eq!(normalize_profile_name("Säsong 2").unwrap(), "Säsong 2");
        for bad in [
            "", "   ", "../x", "a/b", "a\\b", "x.toml", "con", "NUL", "com1", "Lpt9", "a:b",
        ] {
            assert!(normalize_profile_name(bad).is_err(), "{bad:?}");
        }
        assert!(normalize_profile_name("COM10").is_ok(), "only COM1 to COM9");
        assert!(normalize_profile_name(&"x".repeat(40)).is_ok());
        assert!(normalize_profile_name(&"x".repeat(41)).is_err());
        assert!(same_name("main", "Main"));
        assert!(!same_name("Main", "Main 2"));
    }

    #[test]
    fn profiles_are_listed_by_name() {
        let dir = settings_dir("list");
        let profiles = ConfigFile::profiles_dir(&dir);
        assert_eq!(list_profiles(&profiles), Vec::<String>::new(), "no folder");
        std::fs::create_dir_all(&profiles).unwrap();
        for name in [
            "main.toml",
            "Indie.toml",
            "collab.toml",
            "notes.txt",
            "a.b.toml",
        ] {
            std::fs::write(profiles.join(name), "").unwrap();
        }
        std::fs::create_dir(profiles.join("folder.toml")).unwrap();
        assert_eq!(list_profiles(&profiles), ["collab", "Indie", "main"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
