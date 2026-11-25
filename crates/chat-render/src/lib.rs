//! Turns chat messages into styled HTML for the overlay.
//!
//! A theme is a message template (minijinja, `message.html`) plus a
//! stylesheet (`overlay.css`). The built-in themes ([`Builtin`]) are
//! compiled into the binary; a folder can override either file of the
//! default one. [`ThemeSource`] says which theme to use.
//!
//! The two files are loaded separately, so a broken one never takes the
//! other down: [`ThemeSource::load`] gives the template, ready to render (a
//! [`Theme`]), and [`ThemeSource::css`] the stylesheet, which is served as
//! it is.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::Context;
use chat_core::ChatMessage;
use minijinja::Environment;

/// Maximum template execution steps per message render. Generous enough for
/// any real chat message template while stopping hostile or accidental infinite
/// loops (e.g. `{% for i in range(10**9) %}`) from stalling a worker thread.
const TEMPLATE_FUEL: u64 = 100_000;

static BUILTIN_DEFAULT: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Default)));
static BUILTIN_MINIMAL: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Minimal)));
static BUILTIN_BUBBLES: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Bubbles)));
static BUILTIN_COZY: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Cozy)));
static BUILTIN_RETRO: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Retro)));
static BUILTIN_NEON: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Neon)));
static BUILTIN_ORNATE: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Ornate)));
static BUILTIN_AERO: LazyLock<Arc<Theme>> =
    LazyLock::new(|| Arc::new(Theme::from_builtin(Builtin::Aero)));
static BUILTIN_DEFAULT_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(DEFAULT_MANIFEST).expect("built-in Default theme.toml is valid")
});
static BUILTIN_MINIMAL_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(MINIMAL_MANIFEST).expect("built-in Minimal theme.toml is valid")
});
static BUILTIN_BUBBLES_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(BUBBLES_MANIFEST).expect("built-in Bubbles theme.toml is valid")
});
static BUILTIN_COZY_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(COZY_MANIFEST).expect("built-in Cozy theme.toml is valid")
});
static BUILTIN_RETRO_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(RETRO_MANIFEST).expect("built-in Retro theme.toml is valid")
});
static BUILTIN_NEON_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(NEON_MANIFEST).expect("built-in Neon theme.toml is valid")
});
static BUILTIN_ORNATE_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(ORNATE_MANIFEST).expect("built-in Ornate theme.toml is valid")
});
static BUILTIN_AERO_MANIFEST: LazyLock<options::ThemeManifest> = LazyLock::new(|| {
    options::ThemeManifest::parse(AERO_MANIFEST).expect("built-in Aero theme.toml is valid")
});

pub mod options;
mod seed;
pub mod themes;
mod view;

pub const DEFAULT_MESSAGE_TEMPLATE: &str = include_str!("../templates/message.html");
pub const DEFAULT_CSS: &str = include_str!("../templates/overlay.css");
pub const DEFAULT_MANIFEST: &str = include_str!("../templates/theme.toml");
const MINIMAL_MESSAGE_TEMPLATE: &str = include_str!("../templates/minimal/message.html");
const MINIMAL_CSS: &str = include_str!("../templates/minimal/overlay.css");
const MINIMAL_MANIFEST: &str = include_str!("../templates/minimal/theme.toml");
const BUBBLES_MESSAGE_TEMPLATE: &str = include_str!("../templates/bubbles/message.html");
const BUBBLES_CSS: &str = include_str!("../templates/bubbles/overlay.css");
const BUBBLES_MANIFEST: &str = include_str!("../templates/bubbles/theme.toml");
const COZY_MESSAGE_TEMPLATE: &str = include_str!("../templates/cozy/message.html");
const COZY_CSS: &str = include_str!("../templates/cozy/overlay.css");
const COZY_MANIFEST: &str = include_str!("../templates/cozy/theme.toml");
const RETRO_MESSAGE_TEMPLATE: &str = include_str!("../templates/retro/message.html");
const RETRO_CSS: &str = include_str!("../templates/retro/overlay.css");
const RETRO_MANIFEST: &str = include_str!("../templates/retro/theme.toml");
const NEON_MESSAGE_TEMPLATE: &str = include_str!("../templates/neon/message.html");
const NEON_CSS: &str = include_str!("../templates/neon/overlay.css");
const NEON_MANIFEST: &str = include_str!("../templates/neon/theme.toml");
const ORNATE_MESSAGE_TEMPLATE: &str = include_str!("../templates/ornate/message.html");
const ORNATE_CSS: &str = include_str!("../templates/ornate/overlay.css");
const ORNATE_MANIFEST: &str = include_str!("../templates/ornate/theme.toml");
const AERO_MESSAGE_TEMPLATE: &str = include_str!("../templates/aero/message.html");
const AERO_CSS: &str = include_str!("../templates/aero/overlay.css");
const AERO_MANIFEST: &str = include_str!("../templates/aero/theme.toml");

pub const MESSAGE_FILE: &str = "message.html";
pub const CSS_FILE: &str = "overlay.css";
pub use options::MANIFEST_FILE;

/// The themes compiled into the binary. A theme folder falls back to
/// `Default`'s files for whatever it doesn't have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// Cards with badges, the event line and the text.
    Default,
    /// One line per message: name and text.
    Minimal,
    /// Rounded speech bubbles with a floating nameplate pill.
    Bubbles,
    /// Warm pastel stationery cards with stitched borders and chatter charms.
    Cozy,
    /// Classic 16-bit JRPG dialogue boxes with nameplate tabs and pixel bezels.
    Retro,
    /// Cybernetic HUD cards with chamfered corners and dual-color neon accents.
    Neon,
    /// Gilded baroque & dark-fantasy cards with filigree corners and arcane crests.
    Ornate,
    /// Frutiger Aero glossy glass panels with specular sheen and seeded bokeh orbs.
    Aero,
}

impl Builtin {
    pub const ALL: [Builtin; 8] = [
        Builtin::Default,
        Builtin::Minimal,
        Builtin::Bubbles,
        Builtin::Cozy,
        Builtin::Retro,
        Builtin::Neon,
        Builtin::Ornate,
        Builtin::Aero,
    ];

    /// Its name in `config.toml` and in overlay URLs (`?theme=minimal`).
    /// No theme folder can have it (`themes::validate_name` refuses it).
    pub fn name(self) -> &'static str {
        match self {
            Builtin::Default => "default",
            Builtin::Minimal => "minimal",
            Builtin::Bubbles => "bubbles",
            Builtin::Cozy => "cozy",
            Builtin::Retro => "retro",
            Builtin::Neon => "neon",
            Builtin::Ornate => "ornate",
            Builtin::Aero => "aero",
        }
    }

    /// Its name in the app.
    pub fn label(self) -> &'static str {
        match self {
            Builtin::Default => "Default",
            Builtin::Minimal => "Minimal",
            Builtin::Bubbles => "Bubbles",
            Builtin::Cozy => "Cozy",
            Builtin::Retro => "Retro",
            Builtin::Neon => "Neon",
            Builtin::Ornate => "Ornate",
            Builtin::Aero => "Aero",
        }
    }

    /// Any capitalisation, like theme names in the app's picker.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|b| b.name().eq_ignore_ascii_case(name.trim()))
    }

    pub fn message_template(self) -> &'static str {
        match self {
            Builtin::Default => DEFAULT_MESSAGE_TEMPLATE,
            Builtin::Minimal => MINIMAL_MESSAGE_TEMPLATE,
            Builtin::Bubbles => BUBBLES_MESSAGE_TEMPLATE,
            Builtin::Cozy => COZY_MESSAGE_TEMPLATE,
            Builtin::Retro => RETRO_MESSAGE_TEMPLATE,
            Builtin::Neon => NEON_MESSAGE_TEMPLATE,
            Builtin::Ornate => ORNATE_MESSAGE_TEMPLATE,
            Builtin::Aero => AERO_MESSAGE_TEMPLATE,
        }
    }

    pub fn css(self) -> &'static str {
        match self {
            Builtin::Default => DEFAULT_CSS,
            Builtin::Minimal => MINIMAL_CSS,
            Builtin::Bubbles => BUBBLES_CSS,
            Builtin::Cozy => COZY_CSS,
            Builtin::Retro => RETRO_CSS,
            Builtin::Neon => NEON_CSS,
            Builtin::Ornate => ORNATE_CSS,
            Builtin::Aero => AERO_CSS,
        }
    }

    /// The raw `theme.toml` manifest compiled in for this built-in theme.
    pub fn manifest_toml(self) -> &'static str {
        match self {
            Builtin::Default => DEFAULT_MANIFEST,
            Builtin::Minimal => MINIMAL_MANIFEST,
            Builtin::Bubbles => BUBBLES_MANIFEST,
            Builtin::Cozy => COZY_MANIFEST,
            Builtin::Retro => RETRO_MANIFEST,
            Builtin::Neon => NEON_MANIFEST,
            Builtin::Ornate => ORNATE_MANIFEST,
            Builtin::Aero => AERO_MANIFEST,
        }
    }

    /// Cached parsed customization manifest for this built-in theme.
    pub fn manifest(self) -> &'static options::ThemeManifest {
        match self {
            Builtin::Default => &BUILTIN_DEFAULT_MANIFEST,
            Builtin::Minimal => &BUILTIN_MINIMAL_MANIFEST,
            Builtin::Bubbles => &BUILTIN_BUBBLES_MANIFEST,
            Builtin::Cozy => &BUILTIN_COZY_MANIFEST,
            Builtin::Retro => &BUILTIN_RETRO_MANIFEST,
            Builtin::Neon => &BUILTIN_NEON_MANIFEST,
            Builtin::Ornate => &BUILTIN_ORNATE_MANIFEST,
            Builtin::Aero => &BUILTIN_AERO_MANIFEST,
        }
    }
}

/// Where the overlay's theme comes from: compiled in, or a folder (usually
/// one in the themes folder, see [`themes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeSource {
    Builtin(Builtin),
    Folder(PathBuf),
}

impl Default for ThemeSource {
    fn default() -> Self {
        Self::Builtin(Builtin::Default)
    }
}

impl ThemeSource {
    /// A theme as `config.toml` names it: a built-in theme's name,
    /// otherwise a folder in `themes_dir`, or the full path of a theme
    /// folder anywhere. Trusted input only: the name isn't checked here
    /// (names from elsewhere, like overlay URLs, go through
    /// [`themes::validate_name`] first).
    pub fn named(themes_dir: &Path, name: &str) -> Self {
        match Builtin::from_name(name) {
            Some(builtin) => Self::Builtin(builtin),
            // `join` with a full path gives that path, so both work.
            None => Self::Folder(themes_dir.join(name)),
        }
    }

    /// The theme's folder, for its images and fonts; built-in themes have
    /// none.
    pub fn folder(&self) -> Option<&Path> {
        match self {
            Self::Builtin(_) => None,
            Self::Folder(dir) => Some(dir),
        }
    }

    /// Canonical key for storing this theme's customization values in
    /// `[overlay.theme_options.<key>]`.
    pub fn key(&self) -> String {
        match self {
            Self::Builtin(builtin) => builtin.name().to_string(),
            Self::Folder(dir) => dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| dir.display().to_string()),
        }
    }

    /// Loads the theme's message template. A folder is read now (see
    /// [`Theme::load`]).
    pub fn load(&self) -> anyhow::Result<Theme> {
        match self {
            Self::Builtin(builtin) => Ok(Theme::from_builtin(*builtin)),
            Self::Folder(dir) => Theme::load(dir),
        }
    }

    /// Like [`load`](Self::load), but built-in themes return a cached shared
    /// [`Arc<Theme>`] instead of re-parsing the template on every connection.
    pub fn load_shared(&self) -> anyhow::Result<Arc<Theme>> {
        match self {
            Self::Builtin(builtin) => Ok(Theme::shared_builtin(*builtin)),
            Self::Folder(dir) => Theme::load(dir).map(Arc::new),
        }
    }

    /// The theme's stylesheet. A folder without one gets the default
    /// built-in stylesheet.
    pub fn css(&self) -> anyhow::Result<String> {
        match self {
            Self::Builtin(builtin) => Ok(builtin.css().to_string()),
            Self::Folder(dir) => read_inside_or_default(dir, CSS_FILE, DEFAULT_CSS),
        }
    }

    /// Loads the theme's customization manifest (`theme.toml`). A custom
    /// folder without `theme.toml` returns an empty manifest (no options),
    /// rather than inheriting the built-in one, because custom CSS may not
    /// define the default theme's variables.
    pub fn manifest(&self) -> anyhow::Result<options::ThemeManifest> {
        match self {
            Self::Builtin(builtin) => Ok(builtin.manifest().clone()),
            Self::Folder(dir) => {
                let path = dir.join(MANIFEST_FILE);
                let Some(canon) = chat_core::fs::existing_inside_dir(dir, &path)
                    .with_context(|| format!("checking {}", path.display()))?
                else {
                    return Ok(options::ThemeManifest::default());
                };
                let text = std::fs::read_to_string(&canon)
                    .with_context(|| format!("reading {}", path.display()))?;
                options::ThemeManifest::parse(&text)
                    .with_context(|| format!("in {}", path.display()))
            }
        }
    }
}

/// A theme's message template, ready to render messages with. (Its
/// stylesheet isn't part of it: see the module docs.)
pub struct Theme {
    env: Environment<'static>,
}

impl Theme {
    /// The default built-in template.
    pub fn builtin() -> Self {
        Self::from_builtin(Builtin::Default)
    }

    pub fn from_builtin(builtin: Builtin) -> Self {
        // Can only fail if a built-in template has a syntax error, which
        // the tests below would catch: a programmer error, not a runtime one.
        Self::from_template(builtin.message_template().into())
            .expect("built-in message template is valid")
    }

    /// Cached built-in theme, compiled once per process.
    pub fn shared_builtin(builtin: Builtin) -> Arc<Self> {
        match builtin {
            Builtin::Default => Arc::clone(&BUILTIN_DEFAULT),
            Builtin::Minimal => Arc::clone(&BUILTIN_MINIMAL),
            Builtin::Bubbles => Arc::clone(&BUILTIN_BUBBLES),
            Builtin::Cozy => Arc::clone(&BUILTIN_COZY),
            Builtin::Retro => Arc::clone(&BUILTIN_RETRO),
            Builtin::Neon => Arc::clone(&BUILTIN_NEON),
            Builtin::Ornate => Arc::clone(&BUILTIN_ORNATE),
            Builtin::Aero => Arc::clone(&BUILTIN_AERO),
        }
    }

    /// Loads `message.html` from `dir`, or the built-in one if the folder
    /// has none. Syntax errors in the template are reported here, not at
    /// render time.
    pub fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref();
        let message = read_inside_or_default(dir, MESSAGE_FILE, DEFAULT_MESSAGE_TEMPLATE)?;
        Self::from_template(message)
            .with_context(|| format!("invalid {MESSAGE_FILE} in {}", dir.display()))
    }

    fn from_template(message: String) -> anyhow::Result<Self> {
        let mut env = Environment::new();
        // Drop the newline after a block tag and the indentation before it,
        // so `{% if %}` lines don't leave blank lines in the output.
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        // Cap execution steps so a hostile or buggy custom template can't
        // hang the server's worker threads.
        env.set_fuel(Some(TEMPLATE_FUEL));
        // `value | seed(n, salt)`: a stable number per chatter (seed.rs).
        env.add_filter("seed", seed::filter);
        // The `.html` name also switches on HTML auto-escaping.
        env.add_template_owned(MESSAGE_FILE, message)?;
        Ok(Self { env })
    }

    /// Renders one message to an HTML fragment (`<article class="msg">…`).
    pub fn render(&self, msg: &ChatMessage) -> anyhow::Result<String> {
        let template = self.env.get_template(MESSAGE_FILE)?;
        Ok(template.render(view::MessageView::new(msg))?)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Reads `dir/file_name` if it exists and stays inside `dir` (rejecting
/// symlinks pointing outside the theme folder), otherwise returns `default`.
fn read_inside_or_default(dir: &Path, file_name: &str, default: &str) -> anyhow::Result<String> {
    let path = dir.join(file_name);
    let Some(canon) = chat_core::fs::existing_inside_dir(dir, &path)
        .with_context(|| format!("checking {}", path.display()))?
    else {
        return Ok(default.to_string());
    };
    std::fs::read_to_string(&canon).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::demo::sample_messages;
    use chat_core::{Author, ChatPlatform, EmoteRef, MessageKind};

    fn message(platform: ChatPlatform, text: &str, kind: MessageKind) -> ChatMessage {
        ChatMessage {
            id: "m1".into(),
            platform,
            author: Author {
                id: "u1".into(),
                name: "Ann".into(),
                color: Some("#7f5af0".into()),
                badges: vec!["moderator".into()],
                badge_urls: std::collections::BTreeMap::new(),
                avatar_url: None,
            },
            text: text.into(),
            emotes: vec![],
            timestamp: jiff::Timestamp::now(),
            kind,
        }
    }

    fn render(msg: &ChatMessage) -> String {
        Theme::builtin().render(msg).unwrap()
    }

    /// Classic attempts to turn chat into markup or script. They must all
    /// end up as text: in OBS, script in an overlay runs without Chromium's
    /// sandbox (see `OVERLAY_CSP` in chat-server for the second line of
    /// defence).
    const HOSTILE: &[&str] = &[
        r#""><img src=x onerror=alert(1)>"#,
        "'><svg onload=alert(1)>",
        "<script>alert(1)</script>",
        "</p></article><script>alert(1)</script>",
        "javascript:alert(1)",
        "{{ 7*7 }}{% if true %}",
    ];

    /// A message with `value` in every field a chatter or platform controls.
    fn filled_with(value: &str, kind: MessageKind) -> ChatMessage {
        ChatMessage {
            id: value.into(),
            platform: ChatPlatform::Twitch,
            author: Author {
                id: value.into(),
                name: value.into(),
                color: Some(value.into()),
                badges: vec![value.into()],
                badge_urls: std::collections::BTreeMap::from([(
                    value.into(),
                    format!("https://static-cdn.jtvnw.net/badges/v1/{value}/2"),
                )]),
                avatar_url: Some(value.into()),
            },
            text: value.into(),
            // The whole text is also an emote code, so it lands in the
            // emote's `alt`/`title` attributes too.
            emotes: vec![EmoteRef {
                code: value.into(),
                url: value.into(),
            }],
            timestamp: jiff::Timestamp::now(),
            kind,
        }
    }

    fn kinds(value: &str) -> Vec<MessageKind> {
        vec![
            MessageKind::Text,
            MessageKind::Donation {
                amount: value.into(),
                tier: Some(4),
                value: None,
                currency: None,
            },
            MessageKind::Special {
                image_url: Some(value.into()),
                amount: Some(value.into()),
                info: Some(value.into()),
                tier: Some(4),
                value: None,
                currency: None,
            },
            MessageKind::MembershipJoin {
                info: value.into(),
                months: Some(12),
            },
            MessageKind::MembershipGift { count: 3 },
            MessageKind::SystemNotice { info: value.into() },
        ]
    }

    /// The characters that make up markup: tags and attribute quotes.
    /// Escaped text (`&lt;`, `&quot;`, `&#x27;`) contains none of them.
    fn markup(html: &str) -> String {
        html.chars().filter(|c| "<>\"'".contains(*c)).collect()
    }

    #[test]
    fn hostile_chat_never_becomes_markup() {
        for builtin in Builtin::ALL {
            let theme = Theme::from_builtin(builtin);
            let render = |msg: &ChatMessage| theme.render(msg).unwrap();
            hostile_chat_stays_text(builtin, render);
        }
    }

    fn hostile_chat_stays_text(builtin: Builtin, render: impl Fn(&ChatMessage) -> String) {
        for hostile in HOSTILE {
            for (safe_kind, hostile_kind) in kinds("x").into_iter().zip(kinds(hostile)) {
                let expected = render(&filled_with("x", safe_kind));
                let html = render(&filled_with(hostile, hostile_kind));
                assert_eq!(
                    markup(&html),
                    markup(&expected),
                    "{builtin:?}: {hostile:?} changed the markup:\n{html}"
                );
                // Chat is data, never template code: it shows up as typed.
                if hostile.starts_with("{{") {
                    assert!(html.contains("{{ 7*7 }}"), "{html}");
                }
            }
        }
    }

    #[test]
    fn every_builtin_theme_renders_every_sample() {
        for builtin in Builtin::ALL {
            let theme = Theme::from_builtin(builtin);
            assert_eq!(ThemeSource::Builtin(builtin).css().unwrap(), builtin.css());
            for msg in sample_messages(0) {
                let html = theme.render(&msg).unwrap();
                assert!(html.contains(r#" class="msg msg--"#), "{builtin:?}: {html}");
                assert!(
                    html.contains(&format!(r#"data-id="{}""#, msg.id)),
                    "{builtin:?}: {html}"
                );
            }
        }
    }

    /// Minimal's messages are one line of markup with no whitespace between
    /// tags: a stray space would show up as a gap on screen.
    #[test]
    fn minimal_theme_is_one_line_per_message() {
        let theme = Theme::from_builtin(Builtin::Minimal);
        for msg in sample_messages(0) {
            let html = theme.render(&msg).unwrap();
            assert!(!html.contains('\n'), "{html}");
            // (Spaces inside the body are the chatter's own.)
            let markup = html.split(r#"<span class="msg__body">"#).next().unwrap();
            assert!(!markup.contains("> <"), "{html}");
        }
    }

    #[test]
    fn builtin_names_and_sources() {
        assert_eq!(Builtin::from_name(" Minimal "), Some(Builtin::Minimal));
        assert_eq!(Builtin::from_name("BUBBLES"), Some(Builtin::Bubbles));
        assert_eq!(Builtin::from_name("Cozy"), Some(Builtin::Cozy));
        assert_eq!(Builtin::from_name("RETRO"), Some(Builtin::Retro));
        assert_eq!(Builtin::from_name("Neon"), Some(Builtin::Neon));
        assert_eq!(Builtin::from_name("ORNATE"), Some(Builtin::Ornate));
        assert_eq!(Builtin::from_name("Aero"), Some(Builtin::Aero));
        assert_eq!(Builtin::from_name("studio"), None);
        let themes = Path::new("/themes");
        assert_eq!(
            ThemeSource::named(themes, "minimal"),
            ThemeSource::Builtin(Builtin::Minimal)
        );
        assert_eq!(
            ThemeSource::named(themes, "bubbles"),
            ThemeSource::Builtin(Builtin::Bubbles)
        );
        assert_eq!(
            ThemeSource::named(themes, "cozy"),
            ThemeSource::Builtin(Builtin::Cozy)
        );
        assert_eq!(
            ThemeSource::named(themes, "retro"),
            ThemeSource::Builtin(Builtin::Retro)
        );
        assert_eq!(
            ThemeSource::named(themes, "neon"),
            ThemeSource::Builtin(Builtin::Neon)
        );
        assert_eq!(
            ThemeSource::named(themes, "ornate"),
            ThemeSource::Builtin(Builtin::Ornate)
        );
        assert_eq!(
            ThemeSource::named(themes, "aero"),
            ThemeSource::Builtin(Builtin::Aero)
        );
        assert_eq!(
            ThemeSource::named(themes, "studio"),
            ThemeSource::Folder(themes.join("studio"))
        );
        let anywhere = std::env::temp_dir().join("studio");
        assert_eq!(
            ThemeSource::named(themes, anywhere.to_str().unwrap()),
            ThemeSource::Folder(anywhere),
            "a full path is that folder, wherever it is"
        );
        assert_eq!(ThemeSource::default().css().unwrap(), DEFAULT_CSS);
    }

    #[test]
    fn emotes_become_images() {
        let mut msg = message(ChatPlatform::Twitch, "hi Kappa", MessageKind::Text);
        msg.emotes = vec![EmoteRef {
            code: "Kappa".into(),
            // minijinja also escapes "/" (as &#x2f;) inside URLs; browsers
            // decode that, but it would make this assertion unreadable.
            url: "kappa.png".into(),
        }];
        let html = render(&msg);
        assert!(
            html.contains(r#"<p class="msg__body">hi <img class="emote" src="kappa.png" alt="Kappa" title="Kappa"></p>"#),
            "{html}"
        );
    }

    #[test]
    fn gift_wording_follows_platform_and_count() {
        let gift = |platform, count| {
            render(&message(
                platform,
                "",
                MessageKind::MembershipGift { count },
            ))
        };
        assert!(gift(ChatPlatform::Twitch, 5).contains(r#"5</data> subs</p>"#));
        assert!(gift(ChatPlatform::YouTube, 1).contains(r#"1</data> membership</p>"#));
    }

    #[test]
    fn load_overrides_only_existing_files() {
        let dir = std::env::temp_dir().join(format!("chat-render-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MESSAGE_FILE), "<p>{{ author.name }}</p>").unwrap();

        let theme = Theme::load(&dir).unwrap();
        let msg = message(ChatPlatform::Twitch, "hi", MessageKind::Text);
        assert_eq!(theme.render(&msg).unwrap(), "<p>Ann</p>");
        // No overlay.css in dir: the built-in one.
        assert_eq!(ThemeSource::Folder(dir.clone()).css().unwrap(), DEFAULT_CSS);

        std::fs::write(dir.join(MESSAGE_FILE), "{% if %}").unwrap();
        assert!(Theme::load(&dir).is_err(), "syntax errors surface at load");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The template and the stylesheet are loaded separately, so a broken
    /// one never takes the other down. Here: an `overlay.css` that can't be
    /// read as text (saved in an old encoding, e.g. Latin-1 with a `©`).
    #[test]
    fn unreadable_css_does_not_cost_the_template() {
        let dir = std::env::temp_dir().join(format!("chat-render-css-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MESSAGE_FILE), "<p>{{ author.name }}</p>").unwrap();
        std::fs::write(dir.join(CSS_FILE), b"/* \xA9 2026 */").unwrap();

        let theme = Theme::load(&dir).expect("the template is fine");
        let msg = message(ChatPlatform::Twitch, "hi", MessageKind::Text);
        assert_eq!(theme.render(&msg).unwrap(), "<p>Ann</p>");
        assert!(ThemeSource::Folder(dir.clone()).css().is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn runaway_template_hits_fuel_limit() {
        let theme =
            Theme::from_template("{% for i in range(1000000) %}x{% endfor %}".into()).unwrap();
        let msg = message(ChatPlatform::Twitch, "hi", MessageKind::Text);
        assert!(theme.render(&msg).is_err(), "fuel cap stops infinite loops");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outside_theme_folder_is_refused() {
        let root = std::env::temp_dir().join(format!("chat-render-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("cozy");
        std::fs::create_dir_all(&dir).unwrap();
        let outside = root.join("config.toml");
        std::fs::write(&outside, "secret = 1").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(MESSAGE_FILE)).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(CSS_FILE)).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(MANIFEST_FILE)).unwrap();

        assert!(Theme::load(&dir).is_err());
        assert!(ThemeSource::Folder(dir.clone()).css().is_err());
        assert!(ThemeSource::Folder(dir).manifest().is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn badge_icons_render_when_https_and_fall_back_otherwise() {
        let mut msg = message(ChatPlatform::Twitch, "hi", MessageKind::Text);
        msg.author.badges = vec![
            "moderator".into(),
            "member".into(),
            "proxied".into(),
            "unsafe".into(),
            "traversal".into(),
        ];
        msg.author.badge_urls.insert(
            "moderator".into(),
            "https://static-cdn.jtvnw.net/badges/v1/3267646d-33f0-4b17-b3df-f923a41db1d0/2".into(),
        );
        // Server's own local hex-keyed avatar/badge proxy path is accepted.
        msg.author
            .badge_urls
            .insert("proxied".into(), "avatar/0123456789abcdef".into());
        // Non-HTTPS or non-hex proxy URLs must be dropped and rendered as text pills.
        msg.author
            .badge_urls
            .insert("unsafe".into(), "javascript:alert(1)".into());
        msg.author
            .badge_urls
            .insert("traversal".into(), "avatar/../secret".into());

        for builtin in [
            Builtin::Default,
            Builtin::Bubbles,
            Builtin::Cozy,
            Builtin::Retro,
            Builtin::Neon,
            Builtin::Ornate,
            Builtin::Aero,
        ] {
            let html = Theme::from_builtin(builtin).render(&msg).unwrap();
            assert!(
                html.contains(
                    r#"<img class="badge badge--icon badge--moderator" src="https:&#x2f;&#x2f;static-cdn.jtvnw.net&#x2f;badges&#x2f;v1&#x2f;3267646d-33f0-4b17-b3df-f923a41db1d0&#x2f;2" alt="moderator" title="moderator">"#
                ),
                "{builtin:?}: expected badge icon img:\n{html}"
            );
            assert!(
                html.contains(
                    r#"<img class="badge badge--icon badge--proxied" src="avatar&#x2f;0123456789abcdef" alt="proxied" title="proxied">"#
                ),
                "{builtin:?}: expected proxied badge icon img:\n{html}"
            );
            assert!(
                html.contains(r#"<span class="badge badge--member">member</span>"#),
                "{builtin:?}: expected text pill fallback for member:\n{html}"
            );
            assert!(
                html.contains(r#"<span class="badge badge--unsafe">unsafe</span>"#)
                    && !html.contains("javascript:alert(1)"),
                "{builtin:?}: unsafe badge URL must fall back to text pill:\n{html}"
            );
            assert!(
                html.contains(r#"<span class="badge badge--traversal">traversal</span>"#)
                    && !html.contains(".."),
                "{builtin:?}: path-traversal badge URL must fall back to text pill:\n{html}"
            );
        }
    }

    #[test]
    fn every_builtin_theme_exposes_role_classes_and_role_color_options() {
        for builtin in Builtin::ALL {
            let manifest = builtin.manifest();
            for opt_name in ["member_color", "moderator_color", "owner_color"] {
                assert!(
                    manifest.options.iter().any(|o| o.name == opt_name),
                    "{builtin:?} missing {opt_name} option"
                );
            }
            let theme = Theme::from_builtin(builtin);
            for (badges, expected_class) in [
                (vec![], "msg--role-viewer"),
                (vec!["subscriber".into()], "msg--role-member"),
                (vec!["vip".into()], "msg--role-member"),
                (vec!["member".into()], "msg--role-member"),
                (vec!["moderator".into()], "msg--role-moderator"),
                (vec!["broadcaster".into()], "msg--role-owner"),
            ] {
                let mut msg = message(ChatPlatform::Twitch, "hi", MessageKind::Text);
                msg.author.badges = badges;
                let html = theme.render(&msg).unwrap();
                assert!(
                    html.contains(expected_class),
                    "{builtin:?}: expected {expected_class} in:\n{html}"
                );
            }
        }
    }
}
