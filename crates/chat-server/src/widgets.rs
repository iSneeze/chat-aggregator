//! Widgets: self-contained web pages that read chat from the JSON API and
//! show it their own way (emotes floating up, a ball chat pushes around),
//! served at `/widgets/<name>`. Served rather than opened as files, so that
//! their settings can go into the URL: OBS's "Local file" field takes no
//! query, and a page opened from a `file://` URL can't use the API at all
//! (its origin is `null`, see `local`).
//!
//! # The convention
//!
//! A widget is one HTML file that describes the settings it offers in a
//! JSON block of its own:
//!
//! ```html
//! <script type="application/json" id="widget">
//! {
//!   "title": "Live Emoji",
//!   "description": "Chat's emotes float up from the bottom.",
//!   "settings": [
//!     { "name": "size", "label": "Emote size", "type": "number",
//!       "default": 56, "min": 8, "max": 512, "step": 4, "unit": "px" },
//!     { "name": "glow", "label": "Stage light", "type": "bool", "default": true }
//!   ]
//! }
//! </script>
//! ```
//!
//! Each setting can be given in the page's URL query (`?size=72&glow=0`;
//! switches as `1` or `0`), and the page uses that over the default. The
//! app reads the block to offer a form and a URL with what the streamer
//! picked; the page reads its defaults from the same block, so the two
//! can't disagree. [`Kind`] lists the types of setting.
//!
//! Browsers never run a `<script>` of type `application/json` (it's a data
//! block), so the block takes no part in the page's security rules.
//!
//! # Security
//!
//! A widget handles chat, which is untrusted, in OBS, whose browser runs
//! without Chromium's sandbox (see `OVERLAY_CSP`). So it gets the same kind
//! of rules as the overlay: only the widget's own inline scripts may run,
//! allowed by the hash of their exact text. Script smuggled in through chat
//! stays blocked even if a widget had a bug that let it into the page. It's
//! also why a widget must be a single file: a `<script src=…>` would be
//! blocked, and [`Widget::from_page`] says so up front.
//!
//! # Built in and imported
//!
//! Like themes: the built-in widgets come with the app, the streamer's own
//! live in a folder next to `config.toml` (`widgets/<name>.html`), copied
//! there by the app's import ([`prepare_import`], then [`Import::save`]
//! once the streamer agreed to replace one of the same name). A widget's
//! name comes from its title and is both its file name and part of its URL,
//! so it's kept to what's safe in both ([`is_valid_name`]). The values a
//! streamer picks for its settings are kept in `config.toml`
//! ([`Values`]); [`Widget::url`] turns them into the widget's address.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{NO_CACHE, NO_SNIFF, ServerState, local, script_hash};

/// The longest widget name: like theme names, short enough for a file name
/// and a URL that's still readable.
const MAX_NAME: usize = 40;

/// The largest widget file read. A widget is one page, but may carry its
/// pictures inline; the limit is there so that picking a video by mistake
/// doesn't load it whole.
const MAX_FILE: u64 = 5 * 1024 * 1024;

/// What a widget says about itself: the JSON in its block.
///
/// Fields this version doesn't know are ignored (no
/// `deny_unknown_fields`), so a widget written for a newer version still
/// works here, minus what's new.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Manifest {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub settings: Vec<Setting>,
}

/// One setting: a URL query parameter with a default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Setting {
    /// Its name in the URL query.
    pub name: String,
    /// What the app's form calls it.
    pub label: String,
    /// A sentence or two for the form, under the label.
    #[serde(default)]
    pub help: Option<String>,
    /// The block's `"type"` and the fields that come with it. `flatten`:
    /// they sit in the same JSON object as `name` and `label`.
    #[serde(flatten)]
    pub kind: Kind,
}

/// The types of setting, picked by the block's `"type"` field
/// (`tag = "type"`: serde reads that field first, then expects the fields
/// of the variant it names). An enum rather than one struct with optional
/// fields: a number always comes with its limits, a choice with its
/// options, and code that handles settings has to handle every type (the
/// compiler checks each `match`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Kind {
    /// A number from `min` to `max`. `step` is how far the app's − and +
    /// move it (1 if not given), `unit` is shown after it.
    Number {
        default: f64,
        min: f64,
        max: f64,
        #[serde(default)]
        step: Option<f64>,
        #[serde(default)]
        unit: Option<String>,
    },
    /// On or off: `1` or `0` in the URL.
    Bool { default: bool },
    /// Any text.
    Text { default: String },
    /// One of `options`.
    Choice {
        default: String,
        options: Vec<String>,
    },
}

/// A value picked for a setting, the way `config.toml` keeps it
/// (`[widgets.<name>]`: `size = 72`, `glow = false`, `mode = "calm"`).
/// `untagged`: written as the plain TOML value, and read back by trying
/// each variant in turn, so a hand-written `72` is a number just like
/// `72.0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Number(f64),
    /// For text and choice settings.
    Text(String),
}

/// The values picked for one widget, by setting name.
pub type Values = BTreeMap<String, Value>;

impl Value {
    /// How the URL query writes it, before percent-encoding.
    fn to_query(&self) -> String {
        match self {
            Self::Bool(on) => if *on { "1" } else { "0" }.to_string(),
            // `72`, not `72.0`: f64's Display leaves out a zero fraction.
            Self::Number(number) => number.to_string(),
            Self::Text(text) => text.clone(),
        }
    }
}

/// Why a file isn't a widget, or can't be used as one. The app shows these
/// to the streamer who picked the file, so they say what's wrong in plain
/// words.
#[derive(Debug, PartialEq)]
pub enum WidgetError {
    /// No `<script type="application/json" id="widget">`.
    NoBlock,
    /// More than one of them.
    TwoBlocks,
    /// A `<script src=…>`: blocked by a widget's security rules.
    ExternalScript,
    /// The block isn't valid JSON, isn't in the format, or has a setting
    /// that doesn't make sense: which one, and why.
    Format(String),
    /// Bigger than [`MAX_FILE`].
    TooBig,
    /// Not UTF-8 text: a picture or a video, say.
    NotText,
    /// The file couldn't be read: the system's reason.
    Unreadable(String),
    /// A file in the widgets folder whose name can't be a widget's.
    BadFileName,
    /// Would be called like a built-in widget, which always wins.
    BuiltinName(String),
}

impl fmt::Display for WidgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBlock => f.write_str(
                "this isn't a widget: it has no widget block \
                 (<script type=\"application/json\" id=\"widget\">)",
            ),
            Self::TwoBlocks => f.write_str("the page has more than one widget block"),
            Self::ExternalScript => f.write_str(
                "the page loads a script from elsewhere (<script src=…>), which a widget \
                 can't: its scripts must be inside the file",
            ),
            Self::Format(problem) => write!(f, "the widget block isn't right: {problem}"),
            Self::TooBig => f.write_str("the file is bigger than 5 MB: a widget is one web page"),
            Self::NotText => f.write_str("this isn't a web page (the file isn't text)"),
            Self::Unreadable(reason) => write!(f, "can't read the file: {reason}"),
            Self::BadFileName => {
                f.write_str("its file name may only have letters, digits, - and _ (before .html)")
            }
            Self::BuiltinName(name) => write!(f, "\"{name}\" is the name of a built-in widget"),
        }
    }
}

impl std::error::Error for WidgetError {}

/// A widget page, checked and ready to serve.
#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    /// Its address: `/widgets/<name>`.
    pub name: String,
    pub manifest: Manifest,
    page: String,
    /// The page's security rules but `connect-src`, which depends on the
    /// address it's asked for (see [`Widget::csp_for`]).
    csp: String,
}

/// The widgets that come with the app, in the order the app lists them.
/// Checked once, on first use; the tests make sure the `expect` never
/// fires. (The flake's source set lists these files too.)
static BUILTIN: LazyLock<Vec<Widget>> = LazyLock::new(|| {
    [
        (
            "live-emoji",
            include_str!("../../../examples/live-emoji.html"),
        ),
        ("danmaku", include_str!("../../../examples/danmaku.html")),
        ("timer", include_str!("../../../examples/timer.html")),
    ]
    .into_iter()
    .map(|(name, page)| Widget::from_page(name, page).expect("built-in widgets are valid"))
    .collect()
});

impl Widget {
    /// The widgets that come with the app.
    pub fn builtin() -> &'static [Widget] {
        &BUILTIN
    }

    /// Reads and checks a widget page: its block is there once and makes
    /// sense, and it loads no script from elsewhere.
    pub fn from_page(
        name: impl Into<String>,
        page: impl Into<String>,
    ) -> Result<Self, WidgetError> {
        let page = page.into();
        let mut block = None;
        let mut hashes = Vec::new();
        for script in scripts(&page) {
            let kind = attr(script.attrs, "type").map(|t| t.trim().to_ascii_lowercase());
            if kind.as_deref() == Some("application/json") {
                // Other JSON blocks are the page's own business.
                if attr(script.attrs, "id") == Some("widget") {
                    if block.is_some() {
                        return Err(WidgetError::TwoBlocks);
                    }
                    block = Some(script.text);
                }
            } else if attr(script.attrs, "src").is_some() {
                return Err(WidgetError::ExternalScript);
            } else {
                // Not every other type runs (`text/template` doesn't), but
                // allowing one that doesn't is harmless: it's the widget's
                // own text. Missing one that does would break the widget.
                hashes.push(script_hash(script.text));
            }
        }
        let block = block.ok_or(WidgetError::NoBlock)?;
        let manifest: Manifest =
            serde_json::from_str(block).map_err(|e| WidgetError::Format(e.to_string()))?;
        manifest.check().map_err(WidgetError::Format)?;
        let csp = csp(&hashes);
        Ok(Self {
            name: name.into(),
            manifest,
            page,
            csp,
        })
    }

    /// The widget's address on the server at `server` (the overlay's URL,
    /// `http://127.0.0.1:7878/`), with the `values` picked for it: only
    /// those that fit and differ from the default, in the block's order, so
    /// the same choices always give the same URL.
    pub fn url(&self, server: &str, values: &Values) -> String {
        let query: Vec<String> = self
            .manifest
            .settings
            .iter()
            .filter_map(|setting| {
                let value = values.get(&setting.name)?;
                let wanted = setting.accepts(value) && *value != setting.default_value();
                // Setting names are URL-safe already (`Manifest::check`).
                wanted.then(|| format!("{}={}", setting.name, percent_encoded(&value.to_query())))
            })
            .collect();
        let server = server.trim_end_matches('/');
        let mut url = format!("{server}/widgets/{}", self.name);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query.join("&"));
        }
        url
    }

    /// The page's security rules for one request. `connect-src` names the
    /// address the page was asked for (the request's `Host`), which is
    /// where its API is. `'self'` alone should allow `ws://` to that
    /// address in today's browsers, but OBS's Chromium is often a few
    /// versions behind. A `Host` that isn't this computer is never
    /// repeated (`only_local_hosts` refuses those requests anyway).
    fn csp_for(&self, host: Option<&HeaderValue>) -> String {
        let host = host
            .and_then(|h| h.to_str().ok())
            .filter(|h| local::is_loopback_host(h));
        match host {
            Some(host) => format!("{}; connect-src 'self' ws://{host}", self.csp),
            None => format!("{}; connect-src 'self'", self.csp),
        }
    }
}

impl Setting {
    /// Its default, as a value.
    pub fn default_value(&self) -> Value {
        match &self.kind {
            Kind::Number { default, .. } => Value::Number(*default),
            Kind::Bool { default } => Value::Bool(*default),
            Kind::Text { default } | Kind::Choice { default, .. } => Value::Text(default.clone()),
        }
    }

    /// Whether `value` fits this setting: the right type, within its limits
    /// or one of its options. A saved value can stop fitting when a widget
    /// is updated; it's then ignored, as if it had never been picked.
    pub fn accepts(&self, value: &Value) -> bool {
        match (&self.kind, value) {
            (Kind::Number { min, max, .. }, Value::Number(number)) => (min..=max).contains(&number),
            (Kind::Bool { .. }, Value::Bool(_)) | (Kind::Text { .. }, Value::Text(_)) => true,
            (Kind::Choice { options, .. }, Value::Text(text)) => options.contains(text),
            _ => false,
        }
    }
}

impl Manifest {
    /// What serde can't check: names usable in a URL and unique, defaults
    /// within their own limits. Says which setting is wrong, and why.
    fn check(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("its title is empty".into());
        }
        let mut seen = HashSet::new();
        for setting in &self.settings {
            let name = &setting.name;
            let url_safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-');
            if name.is_empty() || !name.bytes().all(url_safe) {
                return Err(format!(
                    "setting {name:?}: a name may only have letters, digits, _ and -"
                ));
            }
            if !seen.insert(name.as_str()) {
                return Err(format!("setting {name:?} is there twice"));
            }
            if setting.label.trim().is_empty() {
                return Err(format!("setting {name:?} has no label"));
            }
            match &setting.kind {
                Kind::Number {
                    default,
                    min,
                    max,
                    step,
                    ..
                } => {
                    if min > max {
                        return Err(format!("setting {name:?}: min is above max"));
                    }
                    if !(min..=max).contains(&default) {
                        return Err(format!(
                            "setting {name:?}: the default {default} isn't from {min} to {max}"
                        ));
                    }
                    if step.is_some_and(|step| step <= 0.0) {
                        return Err(format!("setting {name:?}: step must be above 0"));
                    }
                }
                Kind::Choice { default, options } => {
                    if !options.contains(default) {
                        return Err(format!(
                            "setting {name:?}: the default {default:?} isn't one of its options"
                        ));
                    }
                }
                Kind::Bool { .. } | Kind::Text { .. } => {}
            }
        }
        Ok(())
    }
}

/// A widget's security rules, without `connect-src` (see
/// [`Widget::csp_for`]). Like the overlay's (see `OVERLAY_CSP` for what
/// each rule is for), but scripts are the widget's own, by hash.
fn csp(script_hashes: &[String]) -> String {
    let scripts = if script_hashes.is_empty() {
        "'none'".to_string()
    } else {
        let allowed: Vec<String> = script_hashes
            .iter()
            .map(|hash| format!("'sha256-{hash}'"))
            .collect();
        allowed.join(" ")
    };
    format!(
        "default-src 'none'; script-src {scripts}; \
         style-src 'self' 'unsafe-inline' https:; img-src 'self' https: data:; \
         font-src 'self' https: data:; media-src 'self' https: data:; \
         base-uri 'none'; form-action 'none'"
    )
}

/// `%XX` for every byte of `text` but the few that mean nothing in a URL,
/// so a value can't end its parameter (`&`) or turn into a space (`+`), and
/// `✨` arrives as `%E2%9C%A8`.
fn percent_encoded(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Whether `name` can be a widget's: it's both its file name
/// (`<name>.html`) and part of its URL, so only ASCII letters, digits, `-`
/// and `_`. Nothing in a URL can then lead out of the widgets folder.
pub fn is_valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// A widget name from a title (or a file name): lowercase ASCII letters and
/// digits, everything else in between as one `-` ("Live Emoji!" →
/// `live-emoji`). Empty if there's nothing of those to use (a title all in
/// Japanese, say).
fn name_from(text: &str) -> String {
    let mut name = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    // All ASCII by now, so cutting at a byte count can't split a character.
    name.truncate(MAX_NAME);
    name.trim_end_matches('-').to_string()
}

fn file_name(name: &str) -> String {
    format!("{name}.html")
}

fn is_builtin(name: &str) -> bool {
    Widget::builtin().iter().any(|widget| widget.name == name)
}

/// The text of a widget file, unless it can't be one: too big, not text.
fn read_file(path: &Path) -> Result<String, WidgetError> {
    let unreadable = |e: io::Error| WidgetError::Unreadable(e.to_string());
    if std::fs::metadata(path).map_err(unreadable)?.len() > MAX_FILE {
        return Err(WidgetError::TooBig);
    }
    let bytes = std::fs::read(path).map_err(unreadable)?;
    String::from_utf8(bytes).map_err(|_| WidgetError::NotText)
}

/// Reads and checks the file `path` of the widgets folder, as the widget
/// `name` (its file name without `.html`).
fn load(path: &Path, name: &str) -> Result<Widget, WidgetError> {
    if !is_valid_name(name) {
        return Err(WidgetError::BadFileName);
    }
    if is_builtin(name) {
        return Err(WidgetError::BuiltinName(name.into()));
    }
    Widget::from_page(name, read_file(path)?)
}

/// A file in the widgets folder, and whether it can be used.
#[derive(Debug)]
pub struct Imported {
    /// Its file name without `.html`.
    pub name: String,
    /// The widget, or why the file can't be one.
    pub widget: Result<Widget, WidgetError>,
}

/// The `.html` files in the widgets folder `dir`, sorted by name. A missing
/// folder just means nothing imported yet. Files that can't be widgets are
/// listed too, with the reason: one edited into a broken state then says
/// why, instead of vanishing from the list.
pub fn imported(dir: &Path) -> Vec<Imported> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<Imported> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
        })
        .filter_map(|path| {
            // A name that isn't even Unicode can't be a widget's: skipped.
            let name = path.file_stem()?.to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let widget = load(&path, &name);
            Some(Imported { name, widget })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// A widget file checked for importing, not written yet: the app asks
/// first when it would replace one ([`Import::replaces`]).
#[derive(Debug)]
pub struct Import {
    pub widget: Widget,
    /// A widget of that name is imported already (likely an older version
    /// of the same one): saving replaces it.
    pub replaces: bool,
}

/// Reads and checks the file `file` for importing into the widgets folder
/// `dir`. Its name comes from its title ("Live Emoji" → `live-emoji`), not
/// its file name, because downloaded widgets are often just `index.html`;
/// only a title without letters or digits to use falls back to the file
/// name. Importing a newer version of a widget (same title) therefore
/// replaces the old one.
pub fn prepare_import(dir: &Path, file: &Path) -> Result<Import, WidgetError> {
    let mut widget = Widget::from_page(String::new(), read_file(file)?)?;
    let from_file = file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(name_from)
        .unwrap_or_default();
    let name = [name_from(&widget.manifest.title), from_file]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "widget".into());
    if is_builtin(&name) {
        return Err(WidgetError::BuiltinName(name));
    }
    let replaces = dir.join(file_name(&name)).exists();
    widget.name = name;
    Ok(Import { widget, replaces })
}

impl Import {
    /// Writes the widget into `dir` (created if needed), replacing one of
    /// the same name. Written to a temporary file first and then renamed,
    /// so a failed write never leaves half a widget or loses the old one.
    pub fn save(&self, dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(file_name(&self.widget.name));
        let temp = path.with_extension("html.tmp");
        std::fs::write(&temp, &self.widget.page)?;
        std::fs::rename(&temp, &path)?;
        Ok(path)
    }
}

/// Deletes the imported widget `name` from the widgets folder `dir`.
pub fn remove(dir: &Path, name: &str) -> io::Result<()> {
    if !is_valid_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} isn't a widget name"),
        ));
    }
    std::fs::remove_file(dir.join(file_name(name)))
}

/// `/widgets/<name>`: a built-in widget, else one from the widgets folder,
/// read on every request. Like theme files: edit it, refresh the OBS
/// source, and the change shows (the allowed script hashes have to match
/// the file as it is now anyway). Only valid names are looked up, so
/// nothing in the URL leads out of the folder.
pub(crate) async fn page(
    State(state): State<ServerState>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let host = headers.get(header::HOST);
    if let Some(widget) = Widget::builtin().iter().find(|w| w.name == name) {
        return serve(widget, host);
    }
    let Some(dir) = state.widgets_dir.clone().filter(|_| is_valid_name(&name)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Small local files, but reading is still blocking: done on tokio's
    // threads for that, not the ones serving everything else. The closure
    // takes its own copy of the name (`move`).
    let file = file_name(&name);
    let loaded = tokio::task::spawn_blocking({
        let name = name.clone();
        move || {
            let path = dir.join(file_name(&name));
            path.is_file().then(|| load(&path, &name))
        }
    })
    .await;
    match loaded {
        Ok(Some(Ok(widget))) => serve(&widget, host),
        Ok(Some(Err(problem))) => {
            warn!(widget = %file, "can't show a widget: {problem}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [
                    NO_SNIFF,
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                ],
                format!("This widget can't be shown: {problem}"),
            )
                .into_response()
        }
        Ok(None) | Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn serve(widget: &Widget, host: Option<&HeaderValue>) -> Response {
    let csp = widget.csp_for(host);
    (
        [NO_CACHE, NO_SNIFF],
        [(header::CONTENT_SECURITY_POLICY, csp)],
        Html(widget.page.clone()),
    )
        .into_response()
}

/// One `<script>` element of a page: its start tag's attributes, as text,
/// and its content.
struct Script<'a> {
    attrs: &'a str,
    text: &'a str,
}

/// The page's `<script>` elements, in order. A simple scan rather than a
/// full HTML parser, which is enough for pages that follow the convention.
/// Like browsers, it skips HTML comments (a widget may well mention its
/// block in one) and ends a script at the first `</script`, in any case.
fn scripts(page: &str) -> Vec<Script<'_>> {
    // Lowercasing ASCII leaves every byte where it was, so a position found
    // in `lower` is the same position in `page`.
    let lower = page.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut at = 0;
    loop {
        let script = lower[at..].find("<script").map(|i| at + i);
        let comment = lower[at..].find("<!--").map(|i| at + i);
        let start = match (script, comment) {
            (Some(script), Some(comment)) if comment < script => {
                match lower[comment..].find("-->") {
                    Some(end) => at = comment + end + "-->".len(),
                    None => break, // the rest of the page is a comment
                }
                continue;
            }
            (Some(script), _) => script,
            (None, _) => break,
        };
        let attrs_start = start + "<script".len();
        // `<scripts>` would be another element.
        let rest = &lower[attrs_start..];
        if !rest.starts_with(|c: char| c == '>' || c == '/' || c.is_ascii_whitespace()) {
            at = attrs_start;
            continue;
        }
        // An unclosed tag or script: the page is broken from here on.
        let Some(attrs_end) = rest.find('>').map(|i| attrs_start + i) else {
            break;
        };
        let Some(end) = lower[attrs_end..].find("</script").map(|i| attrs_end + i) else {
            break;
        };
        found.push(Script {
            attrs: &page[attrs_start..attrs_end],
            text: &page[attrs_end + 1..end],
        });
        at = end;
    }
    found
}

/// The value of the attribute `name` in a start tag's attribute text
/// (`type="application/json" id=widget`), if it's there: quoted with `"`
/// or `'`, or unquoted, or `""` for a bare `async`. Attribute names are
/// matched in any case, like browsers do.
fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attrs;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        if rest.is_empty() {
            return None;
        }
        // Never empty here, unless `rest` starts with `=`, which the value
        // below then consumes: every round moves on.
        let name_end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let (this, after) = rest.split_at(name_end);
        let after = after.trim_start();
        let (value, next) = match after.strip_prefix('=') {
            Some(value) => {
                let value = value.trim_start();
                match value.chars().next() {
                    Some(quote @ ('"' | '\'')) => match value[1..].find(quote) {
                        Some(close) => (&value[1..=close], &value[close + 2..]),
                        None => (&value[1..], ""),
                    },
                    _ => {
                        let end = value
                            .find(|c: char| c.is_ascii_whitespace())
                            .unwrap_or(value.len());
                        value.split_at(end)
                    }
                }
            }
            None => ("", after),
        };
        if this.eq_ignore_ascii_case(name) {
            return Some(value);
        }
        rest = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{start, start_with_widgets};

    const BLOCK: &str = r#"{
        "title": "Test widget",
        "description": "For the tests.",
        "settings": [
            { "name": "size", "label": "Size", "type": "number", "default": 56,
              "min": 8, "max": 512, "step": 4, "unit": "px", "help": "How big." },
            { "name": "glow", "label": "Glow", "type": "bool", "default": true },
            { "name": "sticker", "label": "Sticker", "type": "text", "default": "✨" },
            { "name": "mode", "label": "Mood", "type": "choice", "default": "party",
              "options": ["party", "calm"] }
        ]
    }"#;

    /// A page with `block` as its widget block and `script` as its script.
    fn page(block: &str, script: &str) -> String {
        format!(
            "<!doctype html><html><head>\
             <script type=\"application/json\" id=\"widget\">{block}</script>\
             </head><body><script>{script}</script></body></html>"
        )
    }

    fn script_src(csp: &str) -> &str {
        csp.split(';')
            .map(str::trim)
            .find(|rule| rule.starts_with("script-src"))
            .expect("a script-src rule")
    }

    #[test]
    fn reads_the_settings_from_the_block() {
        let widget = Widget::from_page("test", page(BLOCK, "go()")).unwrap();
        assert_eq!(widget.manifest.title, "Test widget");
        assert_eq!(widget.manifest.description, "For the tests.");
        let settings = &widget.manifest.settings;
        assert_eq!(settings.len(), 4);
        assert_eq!(settings[0].name, "size");
        assert_eq!(settings[0].help.as_deref(), Some("How big."));
        assert_eq!(
            settings[0].kind,
            Kind::Number {
                default: 56.0,
                min: 8.0,
                max: 512.0,
                step: Some(4.0),
                unit: Some("px".into())
            }
        );
        assert_eq!(settings[1].kind, Kind::Bool { default: true });
        assert_eq!(settings[1].help, None);
        assert_eq!(
            settings[2].kind,
            Kind::Text {
                default: "✨".into()
            }
        );
        assert_eq!(
            settings[3].kind,
            Kind::Choice {
                default: "party".into(),
                options: vec!["party".into(), "calm".into()]
            }
        );
    }

    #[test]
    fn fields_from_newer_versions_are_ignored() {
        let block = r#"{ "title": "T", "icon": "🎉", "settings": [
            { "name": "a", "label": "A", "type": "bool", "default": false, "group": "x" } ] }"#;
        let widget = Widget::from_page("test", page(block, "")).unwrap();
        assert_eq!(
            widget.manifest.settings[0].kind,
            Kind::Bool { default: false }
        );
    }

    #[test]
    fn only_the_widgets_own_scripts_may_run() {
        let html = format!(
            "{}<script type=\"application/json\" id=\"data\">{{\"x\": 1}}</script>\
             <SCRIPT type=module>second()</SCRIPT>",
            page(BLOCK, "first()")
        );
        let widget = Widget::from_page("test", html).unwrap();
        // Both scripts, by the hash of their exact text; JSON blocks aren't
        // scripts.
        assert_eq!(
            script_src(&widget.csp),
            format!(
                "script-src 'sha256-{}' 'sha256-{}'",
                script_hash("first()"),
                script_hash("second()")
            )
        );
        assert!(
            widget.csp.starts_with("default-src 'none';"),
            "{}",
            widget.csp
        );

        // No script at all: nothing may run.
        let html = "<script type=\"application/json\" id=\"widget\">{\"title\": \"T\"}</script>";
        let widget = Widget::from_page("test", html).unwrap();
        assert_eq!(script_src(&widget.csp), "script-src 'none'");
    }

    #[test]
    fn connections_only_go_back_to_this_computer() {
        let widget = Widget::from_page("test", page(BLOCK, "")).unwrap();
        let host = HeaderValue::from_static("127.0.0.1:7878");
        assert!(
            widget
                .csp_for(Some(&host))
                .ends_with("; connect-src 'self' ws://127.0.0.1:7878")
        );
        let elsewhere = HeaderValue::from_static("evil.example");
        assert!(
            widget
                .csp_for(Some(&elsewhere))
                .ends_with("; connect-src 'self'")
        );
        assert!(widget.csp_for(None).ends_with("; connect-src 'self'"));
    }

    #[test]
    fn pages_that_arent_widgets_are_refused() {
        let cases = [
            (
                "<html><script>go()</script></html>".to_string(),
                WidgetError::NoBlock,
            ),
            (
                // A JSON block, but not the widget's.
                "<script type=\"application/json\" id=\"data\">{}</script>".to_string(),
                WidgetError::NoBlock,
            ),
            (
                format!("{}{}", page(BLOCK, ""), page(BLOCK, "")),
                WidgetError::TwoBlocks,
            ),
            (
                format!(
                    "{}<script src=\"https://cdn.example/lib.js\"></script>",
                    page(BLOCK, "")
                ),
                WidgetError::ExternalScript,
            ),
        ];
        for (html, expected) in cases {
            assert_eq!(
                Widget::from_page("test", html.as_str()).unwrap_err(),
                expected,
                "{html}"
            );
        }
    }

    #[test]
    fn blocks_that_make_no_sense_are_refused_with_the_reason() {
        let number = |fields: &str| {
            format!(
                r#"{{ "title": "T", "settings": [
                    {{ "name": "n", "label": "N", "type": "number", {fields} }} ] }}"#
            )
        };
        let cases = [
            ("not json".to_string(), "expected"),
            (r#"{ "settings": [] }"#.to_string(), "missing field `title`"),
            (r#"{ "title": " " }"#.to_string(), "title is empty"),
            (number(r#""default": 1, "min": 0"#), "missing field `max`"),
            (
                r#"{ "title": "T", "settings": [
                    { "name": "c", "label": "C", "type": "colour", "default": "red" } ] }"#
                    .to_string(),
                "unknown variant `colour`",
            ),
            (
                number(r#""default": 1, "min": 5, "max": 0"#),
                "min is above max",
            ),
            (
                number(r#""default": 9, "min": 0, "max": 5"#),
                "isn't from 0 to 5",
            ),
            (
                number(r#""default": 1, "min": 0, "max": 5, "step": 0"#),
                "step must be above 0",
            ),
            (
                r#"{ "title": "T", "settings": [
                    { "name": "c", "label": "C", "type": "choice", "default": "x",
                      "options": ["a", "b"] } ] }"#
                    .to_string(),
                "isn't one of its options",
            ),
            (
                r#"{ "title": "T", "settings": [
                    { "name": "a b", "label": "A", "type": "bool", "default": true } ] }"#
                    .to_string(),
                "letters, digits, _ and -",
            ),
            (
                r#"{ "title": "T", "settings": [
                    { "name": "a", "label": "A", "type": "bool", "default": true },
                    { "name": "a", "label": "B", "type": "bool", "default": true } ] }"#
                    .to_string(),
                "is there twice",
            ),
            (
                r#"{ "title": "T", "settings": [
                    { "name": "a", "label": "", "type": "bool", "default": true } ] }"#
                    .to_string(),
                "has no label",
            ),
        ];
        for (block, expected) in cases {
            match Widget::from_page("test", page(&block, "")) {
                Err(WidgetError::Format(problem)) => {
                    assert!(problem.contains(expected), "{problem:?} for {block}");
                }
                other => panic!("{block}: {other:?}"),
            }
        }
    }

    #[test]
    fn attributes_are_read_like_browsers_do() {
        let attrs = r#" type="application/json" ID='widget' data-x=1 async src = "a b.js""#;
        assert_eq!(attr(attrs, "type"), Some("application/json"));
        assert_eq!(attr(attrs, "id"), Some("widget"));
        assert_eq!(attr(attrs, "data-x"), Some("1"));
        assert_eq!(attr(attrs, "async"), Some(""));
        assert_eq!(attr(attrs, "src"), Some("a b.js"));
        assert_eq!(attr(attrs, "defer"), None);
        // Broken ones end rather than loop or panic.
        assert_eq!(attr(r#" type="never closed"#, "type"), Some("never closed"));
        assert_eq!(attr(" = =", "type"), None);
        assert_eq!(attr("", "type"), None);
    }

    #[test]
    fn scripts_are_found_like_browsers_do() {
        let html = "<!-- the block: <script type=\"application/json\" id=\"widget\"> -->\
                    <scripts>no</scripts><script async>a</SCRIPT ><script\n>b</script>\
                    <script>never closed";
        let found: Vec<_> = scripts(html).iter().map(|s| (s.attrs, s.text)).collect();
        assert_eq!(found, [(" async", "a"), ("\n", "b")]);
        // A comment that never ends hides the rest of the page.
        assert!(scripts("<!-- <script>a</script>").is_empty());
    }

    #[test]
    fn the_built_in_widgets_are_valid_and_read_their_own_block() {
        let builtin = Widget::builtin();
        assert!(!builtin.is_empty());
        for widget in builtin {
            assert!(!widget.manifest.settings.is_empty(), "{}", widget.name);
            // The name an import of it would get, so importing it (or
            // another widget with its title) runs into the built-in name.
            assert_eq!(widget.name, name_from(&widget.manifest.title));
            // The page takes its defaults from the block, so the app's form
            // and the page can't disagree.
            assert!(
                widget.page.contains(r#"document.getElementById("widget")"#),
                "{} doesn't read its widget block",
                widget.name
            );
        }
    }

    #[tokio::test]
    async fn the_built_in_widgets_are_served_with_their_security_rules() {
        let server = start(None).await;
        for widget in Widget::builtin() {
            let url = format!("http://{}/widgets/{}", server.addr, widget.name);
            let resp = reqwest::get(url).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{}", widget.name);
            assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
            let csp = resp.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .to_string();
            let page = resp.text().await.unwrap();
            assert_eq!(page, widget.page, "{}", widget.name);

            // Its one script, by the hash of exactly what's served.
            let script = page
                .split_once("<script>")
                .and_then(|(_, rest)| rest.split_once("</script>"))
                .unwrap()
                .0;
            assert_eq!(
                script_src(&csp),
                format!("script-src 'sha256-{}'", script_hash(script)),
                "{}",
                widget.name
            );
            assert!(csp.starts_with("default-src 'none';"), "{csp}");
            assert!(
                csp.ends_with(&format!("connect-src 'self' ws://{}", server.addr)),
                "{csp}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_widgets_are_not_found() {
        let server = start(None).await;
        for path in ["nope", "live-emoji.html", "..%2Fsrc%2Flib.rs"] {
            let resp = reqwest::get(format!("http://{}/widgets/{path}", server.addr))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    /// A fresh, empty folder for one test (`test` keeps tests apart when
    /// they run as threads of one process).
    fn scratch(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-server-widgets-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn values(pairs: &[(&str, Value)]) -> Values {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn the_url_carries_only_values_that_differ_and_fit() {
        let widget = Widget::from_page("test", page(BLOCK, "")).unwrap();
        let bare = "http://127.0.0.1:7878/widgets/test";
        let server = "http://127.0.0.1:7878/";
        assert_eq!(widget.url(server, &Values::new()), bare);

        let picked = values(&[
            ("mode", Value::Text("calm".into())),
            ("size", Value::Number(72.0)),
            ("glow", Value::Bool(false)),
            ("sticker", Value::Text("🎉 & more+".into())),
        ]);
        // In the block's order (size, glow, sticker, mode), whatever the
        // map's; `&` and `+` can't break out of their value.
        assert_eq!(
            widget.url("http://127.0.0.1:7979", &picked),
            "http://127.0.0.1:7979/widgets/test\
             ?size=72&glow=0&sticker=%F0%9F%8E%89%20%26%20more%2B&mode=calm"
        );
        assert_eq!(
            widget.url(server, &values(&[("size", Value::Number(8.5))])),
            format!("{bare}?size=8.5")
        );

        // Defaults, values that don't fit (any more), and settings the
        // widget doesn't have are left out.
        let stale = values(&[
            ("size", Value::Number(56.0)),
            ("glow", Value::Text("yes".into())),
            ("mode", Value::Text("disco".into())),
            ("gone", Value::Bool(true)),
        ]);
        assert_eq!(widget.url(server, &stale), bare);
        let too_big = values(&[("size", Value::Number(9999.0))]);
        assert_eq!(widget.url(server, &too_big), bare);
    }

    #[test]
    fn names_are_made_from_titles_and_kept_safe() {
        assert_eq!(name_from("Live Emoji"), "live-emoji");
        assert_eq!(name_from("  Chat — Ball!! 2 "), "chat-ball-2");
        assert_eq!(name_from("Ab-Cd_ef"), "ab-cd-ef");
        assert_eq!(name_from("ライブ"), "");
        assert_eq!(name_from(&"a".repeat(60)).len(), MAX_NAME);

        for name in ["live-emoji", "My_Widget2"] {
            assert!(is_valid_name(name), "{name}");
        }
        for name in ["", "a b", "../x", "a/b", "ä", "x.html"] {
            assert!(!is_valid_name(name), "{name}");
        }
        assert!(!is_valid_name(&"a".repeat(MAX_NAME + 1)));
    }

    #[test]
    fn importing_names_the_widget_by_its_title_and_says_when_it_replaces_one() {
        let dir = scratch("import");
        let downloads = scratch("import-downloads");
        let file = downloads.join("index.html");
        std::fs::write(&file, page(BLOCK, "go()")).unwrap();

        let import = prepare_import(&dir, &file).unwrap();
        assert_eq!(import.widget.name, "test-widget");
        assert!(!import.replaces);
        let saved = import.save(&dir).unwrap();
        assert_eq!(saved, dir.join("test-widget.html"));
        assert_eq!(
            std::fs::read_to_string(&saved).unwrap(),
            page(BLOCK, "go()")
        );

        // A newer version of the same widget would replace it.
        std::fs::write(&file, page(BLOCK, "go(2)")).unwrap();
        let again = prepare_import(&dir, &file).unwrap();
        assert!(again.replaces);
        again.save(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(&saved).unwrap(),
            page(BLOCK, "go(2)")
        );
        // Nothing else in the folder: no temporary file left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // A title without letters or digits to use: the file's name.
        let ball = downloads.join("My Ball.html");
        std::fs::write(&ball, page(r#"{ "title": "ライブ" }"#, "")).unwrap();
        assert_eq!(prepare_import(&dir, &ball).unwrap().widget.name, "my-ball");

        // A built-in widget's name.
        let copy = downloads.join("copy.html");
        std::fs::write(&copy, page(r#"{ "title": "Live Emoji" }"#, "")).unwrap();
        assert_eq!(
            prepare_import(&dir, &copy).unwrap_err(),
            WidgetError::BuiltinName("live-emoji".into())
        );
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(downloads).unwrap();
    }

    #[test]
    fn files_that_cant_be_widgets_are_refused_with_the_reason() {
        let dir = scratch("refused");
        let downloads = scratch("refused-downloads");
        let write = |name: &str, bytes: &[u8]| {
            let path = downloads.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let not_a_widget = write("page.html", b"<html><script>go()</script></html>");
        assert_eq!(
            prepare_import(&dir, &not_a_widget).unwrap_err(),
            WidgetError::NoBlock
        );
        let picture = write("picture.png", &[0xff, 0xd8, 0xff, 0xe0, 0x00]);
        assert_eq!(
            prepare_import(&dir, &picture).unwrap_err(),
            WidgetError::NotText
        );
        // Too big: a file of that length without writing it (sparse), so
        // the test stays fast. Refused before reading.
        let video = downloads.join("video.html");
        std::fs::File::create(&video)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert_eq!(
            prepare_import(&dir, &video).unwrap_err(),
            WidgetError::TooBig
        );
        let gone = downloads.join("gone.html");
        assert!(matches!(
            prepare_import(&dir, &gone),
            Err(WidgetError::Unreadable(_))
        ));
        // Nothing was written.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(downloads).unwrap();
    }

    #[test]
    fn the_folder_lists_its_widgets_and_why_others_cant_be_used() {
        let dir = scratch("list");
        let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        write("b-widget.html", &page(BLOCK, ""));
        write("a-broken.html", "<html>no block</html>");
        write("bad name.html", &page(BLOCK, ""));
        write("live-emoji.html", &page(BLOCK, ""));
        // Not widgets at all: skipped.
        write("notes.txt", "notes");
        write(".hidden.html", &page(BLOCK, ""));
        write("half.html.tmp", &page(BLOCK, ""));
        std::fs::create_dir(dir.join("folder.html")).unwrap();

        let listed = imported(&dir);
        let names: Vec<&str> = listed.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["a-broken", "b-widget", "bad name", "live-emoji"]);
        assert_eq!(
            listed[0].widget.as_ref().unwrap_err(),
            &WidgetError::NoBlock
        );
        assert_eq!(
            listed[1].widget.as_ref().unwrap().manifest.title,
            "Test widget"
        );
        assert_eq!(
            listed[2].widget.as_ref().unwrap_err(),
            &WidgetError::BadFileName
        );
        assert_eq!(
            listed[3].widget.as_ref().unwrap_err(),
            &WidgetError::BuiltinName("live-emoji".into())
        );

        assert!(imported(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removing_deletes_only_that_widget() {
        let dir = scratch("remove");
        std::fs::write(dir.join("one.html"), page(BLOCK, "")).unwrap();
        std::fs::write(dir.join("two.html"), page(BLOCK, "")).unwrap();
        remove(&dir, "one").unwrap();
        let names: Vec<String> = imported(&dir).into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["two"]);
        assert!(remove(&dir, "../two").is_err());
        assert!(remove(&dir, "one").is_err(), "already gone");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn imported_widgets_are_served_from_the_folder_as_they_are_now() {
        let root = scratch("served");
        let dir = root.join("widgets");
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("mine.html");
        std::fs::write(&file, page(BLOCK, "first()")).unwrap();
        // A widget page next to the folder, not in it: never served.
        std::fs::write(root.join("outside.html"), page(BLOCK, "")).unwrap();
        let server = start_with_widgets(dir.clone()).await;
        let get = |path: &str| reqwest::get(format!("http://{}/widgets/{path}", server.addr));
        let script_src_of = |resp: &reqwest::Response| {
            script_src(resp.headers()["content-security-policy"].to_str().unwrap()).to_string()
        };

        let resp = get("mine").await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            script_src_of(&resp),
            format!("script-src 'sha256-{}'", script_hash("first()"))
        );
        assert_eq!(resp.text().await.unwrap(), page(BLOCK, "first()"));

        // Edited: the next request serves it, with its new script allowed.
        std::fs::write(&file, page(BLOCK, "second()")).unwrap();
        let resp = get("mine").await.unwrap();
        assert_eq!(
            script_src_of(&resp),
            format!("script-src 'sha256-{}'", script_hash("second()"))
        );

        // Broken: the reason instead of half a widget.
        std::fs::write(&file, "<html>no block</html>").unwrap();
        let resp = get("mine").await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(resp.text().await.unwrap().contains("no widget block"));

        // A built-in widget wins over a file of its name.
        std::fs::write(dir.join("live-emoji.html"), page(BLOCK, "")).unwrap();
        let resp = get("live-emoji").await.unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            include_str!("../../../examples/live-emoji.html")
        );

        for path in ["missing", "..%2Foutside", "mine.html"] {
            assert_eq!(
                get(path).await.unwrap().status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
