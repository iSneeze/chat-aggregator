//! HTTP side of the aggregator: the OBS overlay page, its stylesheet, and
//! the Server-Sent Events stream that feeds it.
//!
//! | route          | serves                                                   |
//! |----------------|----------------------------------------------------------|
//! | `/`            | the overlay page (add it to OBS as a Browser Source)     |
//! | `/theme/overlay.css` | the theme's CSS, re-read on every request          |
//! | `/theme/…`     | other files of the theme folder (images, fonts)          |
//! | `/themes/<name>/…` | the same for a named theme (`/?theme=<name>`)        |
//! | `/events`      | SSE: replay history, then live chat as rendered HTML     |
//! | `/avatar/<key>` | a chatter's profile picture, downloaded once (`avatars`) |
//! | `/api/v1/ws`   | WebSocket: live events as JSON (see `api`, docs/api.md)  |
//! | `/widgets/<name>` | a widget: a page that reads the JSON API (`widgets`) |

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use chat_core::{ChatEvent, Hub};
use chat_render::Theme;
use chat_render::options::{ResolvedStyle, ThemeManifest, ThemeValues, lookup_theme_values};
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, stream};
use hide::OverlayFilter;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

mod api;
mod approvals;
mod avatars;
mod connections;
mod hide;
mod local;
mod preview;
mod stagger;
pub mod widgets;

// Part of this crate's API (`ServerState::theme`), so users of the server
// don't need chat-render as a dependency of their own just to name them.
pub use approvals::{ANSWER_TIMEOUT, ApiOrigins};
pub use avatars::Avatars;
pub use chat_render::options as theme_options;
pub use chat_render::{Builtin, ThemeSource};
pub use connections::Connections;
pub use hide::Hide;
pub use stagger::Stagger;

/// Which end of the overlay the newest message appears at. The app's
/// setting is the default; an overlay URL with `?newest=top` (or `bottom`)
/// overrides it, so each OBS scene can have its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Newest {
    /// Chat grows upward, old messages leave at the top (the classic look).
    #[default]
    Bottom,
    /// Chat grows downward, old messages leave at the bottom.
    Top,
}

impl Newest {
    pub fn as_str(self) -> &'static str {
        match self {
            Newest::Bottom => "bottom",
            Newest::Top => "top",
        }
    }
}

const OVERLAY_HTML: &str = include_str!("overlay.html");

/// The overlay page's Content-Security-Policy: rules sent along with the
/// page that tell the browser (OBS's browser source) what it may load and
/// run. The escaping in `chat-render` already keeps chat from becoming
/// markup; this is the second line of defence, for the day that breaks, or
/// for a theme from someone else that contains script. It matters more in
/// OBS than in a normal browser: OBS runs its browser without Chromium's
/// sandbox, so script in an overlay can be one browser bug away from the
/// streamer's computer (CVE-2024-7971 was exploited exactly like that).
///
/// - `script-src` allows only our own overlay script, by the SHA-256 hash of
///   its exact text. Everything else is blocked: `<script>` from anywhere
///   else, and inline event handlers like `<img onerror=…>`.
/// - Styles, images and fonts may come from the web (platform emotes and
///   avatars, web fonts in themes); `'unsafe-inline'` styles are needed for
///   the author colour's `style` attribute. CSS can't run script.
/// - `connect-src 'self'`: the live chat stream from this server only.
/// - `default-src 'none'` blocks everything not listed: frames, plugins,
///   workers.
///
/// A `LazyLock` computes it once, on first use; the hash is taken from the
/// same text that is served, so editing the script can't make them differ.
static OVERLAY_CSP: LazyLock<String> = LazyLock::new(|| {
    let script = OVERLAY_HTML
        .split_once("<script>")
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(script, _)| script)
        .expect("overlay.html has its script inline");
    let hash = script_hash(script);
    format!(
        "default-src 'none'; script-src 'sha256-{hash}'; \
         style-src 'self' 'unsafe-inline' https:; img-src 'self' https: data:; \
         font-src 'self' https: data:; media-src 'self' https: data:; \
         connect-src 'self'; base-uri 'none'; form-action 'none'"
    )
});

/// What a CSP's `'sha256-…'` needs for an inline script: the SHA-256 of its
/// exact text, in base64. Shared by the overlay and the widgets.
fn script_hash(script: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(<sha2::Sha256 as sha2::Digest>::digest(script.as_bytes()))
}

/// Stops the browser from guessing a file's type: a theme file is only ever
/// used as what we declare (e.g. never run as script).
const NO_SNIFF: (header::HeaderName, &str) = (header::X_CONTENT_TYPE_OPTIONS, "nosniff");

/// Live overlay settings read by the server on page load, SSE connect, or per
/// message. Grouped into one `watch` channel so adding future overlay options
/// (such as built-in theme accents) doesn't multiply channels across
/// `ServerState` and `chat-engine`. Reloading connected overlays is a separate
/// explicit counter ([`ServerState::reload`]), so changing `stagger` or `hide`
/// never reloads overlays.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OverlayConfig {
    /// The app's theme: built-in, or a folder with a custom `message.html`
    /// / `overlay.css`.
    pub theme: ThemeSource,
    /// Per-theme customization overrides keyed by [`ThemeSource::key`].
    /// Applied to initial CSS/HTML responses and pushed live over SSE
    /// (`event: style`) without reloading overlays.
    pub theme_options: BTreeMap<String, ThemeValues>,
    /// Spacing of message bursts in the overlay (not in the API). Read when
    /// an overlay connects.
    pub stagger: Stagger,
    /// The default chat direction, written into the overlay page when it
    /// loads.
    pub newest: Newest,
    /// What overlays leave out, unless their URL says otherwise (`?hide=`).
    /// Read for every message: a change applies to the next one.
    pub hide: Vec<Hide>,
}

/// Shared by all request handlers. Cloned per request, which is cheap:
/// every field is an `Arc` or a small handle around one.
#[derive(Clone)]
pub struct ServerState {
    pub hub: Arc<Hub>,
    /// Live overlay settings (theme, pacing, direction, hidden items).
    pub overlay: watch::Receiver<OverlayConfig>,
    /// Incremented whenever connected overlays should reload (new theme, new
    /// chat direction, profile switch, or manual "Reload overlays"). Separate
    /// from `overlay` so non-reloading edits (`stagger`, `hide`) never reload
    /// overlays and reloads are explicit.
    pub reload: watch::Receiver<u64>,
    /// Cancelled when the app shuts down.
    pub shutdown: CancellationToken,
    /// Whether the JSON API accepts clients. Switching it off also closes
    /// the connected ones.
    pub api: watch::Receiver<bool>,
    /// The folder of named themes (`themes/` next to `config.toml`), for
    /// overlays that pick their own with `?theme=<name>`. Fixed for the
    /// server's lifetime; `None`: every overlay uses the app's theme.
    pub themes_dir: Option<PathBuf>,
    /// The folder of imported widgets (`widgets/` next to `config.toml`),
    /// served at `/widgets/<name>` after the built-in ones. Fixed for the
    /// server's lifetime; `None`: only the built-in widgets.
    pub widgets_dir: Option<PathBuf>,
    pub connections: Arc<Connections>,
    /// Which web sites may use the JSON API, and the questions about new
    /// ones waiting for the streamer's answer.
    pub origins: Arc<ApiOrigins>,
    /// Profile pictures for the overlay, downloaded once for all overlays.
    pub avatars: Arc<Avatars>,
}

/// The query of an overlay URL (`/?theme=cozy`), passed on to `/events` by
/// the page. Other parameters (`newest`) are the page script's business.
#[derive(Default, serde::Deserialize)]
struct OverlayQuery {
    theme: Option<String>,
    /// What this overlay leaves out, instead of the app's setting: names
    /// separated by commas (`?hide=streamelements,notice`), see [`Hide`].
    hide: Option<String>,
}

/// Which theme an overlay uses.
#[derive(Debug, PartialEq)]
enum ThemeChoice {
    /// The app's current theme: no `?theme=`, or one that isn't there.
    App,
    /// Pinned by the overlay URL, whatever the app's theme: a built-in
    /// theme by its name (`default`, `minimal`), or a folder in the themes
    /// folder.
    Pinned { name: String, source: ThemeSource },
}

impl ServerState {
    fn current_theme(&self) -> ThemeSource {
        self.overlay.borrow().theme.clone()
    }

    /// The theme for `?theme=<requested>`. Only themes in the themes folder
    /// can be picked, by a valid theme name (which rules out `..` and
    /// slashes); anything else is logged and gets the app's theme, so a
    /// typo in a scene's URL still shows chat.
    async fn choose_theme(&self, requested: Option<&str>) -> ThemeChoice {
        let Some(name) = requested else {
            return ThemeChoice::App;
        };
        // The built-in themes' names can't be folder names
        // (`validate_name` refuses them), so they're free to mean those.
        if let Some(builtin) = Builtin::from_name(name) {
            return ThemeChoice::Pinned {
                name: builtin.name().into(),
                source: ThemeSource::Builtin(builtin),
            };
        }
        let valid = chat_render::themes::validate_name(name).is_ok() && name == name.trim();
        let candidate = self
            .themes_dir
            .as_ref()
            .filter(|_| valid)
            .map(|root| root.join(name));
        let dir = match candidate {
            Some(dir) => {
                let exists = tokio::task::spawn_blocking({
                    let dir = dir.clone();
                    move || dir.is_dir()
                })
                .await
                .unwrap_or(false);
                exists.then_some(dir)
            }
            None => None,
        };
        match dir {
            Some(dir) => ThemeChoice::Pinned {
                name: name.to_string(),
                source: ThemeSource::Folder(dir),
            },
            None => {
                // `debug!` rather than `warn!`: any web page could send
                // `/?theme=unknown` requests, which shouldn't fill the log file.
                debug!(
                    theme = name,
                    "an overlay asked for a theme that isn't in the themes folder, using the app's theme"
                );
                ThemeChoice::App
            }
        }
    }

    fn source_for(&self, choice: &ThemeChoice) -> ThemeSource {
        match choice {
            ThemeChoice::App => self.current_theme(),
            ThemeChoice::Pinned { source, .. } => source.clone(),
        }
    }

    /// Loads the theme's `theme.toml` manifest. Built-ins use the cached
    /// manifest in memory; custom folders load off the async runtime. A broken
    /// custom manifest logs and yields an empty manifest (no overrides).
    async fn load_manifest(&self, source: ThemeSource) -> ThemeManifest {
        let loaded = match source {
            ThemeSource::Builtin(builtin) => Ok(builtin.manifest().clone()),
            ThemeSource::Folder(_) => tokio::task::spawn_blocking(move || source.manifest())
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("loading theme manifest failed: {e}"))),
        };
        loaded.unwrap_or_else(|e| {
            error!("invalid theme manifest, ignoring options: {e:#}");
            ThemeManifest::default()
        })
    }

    /// Loaded per overlay connection, so "edit the template, refresh the
    /// browser source" picks up changes without a restart. Built-in themes
    /// return a shared cached copy; custom folders load on Tokio's blocking
    /// pool so disk I/O never stalls the 2-thread async runtime. A broken
    /// custom template logs and falls back to the built-in one.
    async fn load_theme(&self, source: ThemeSource) -> Arc<Theme> {
        let loaded = match source {
            ThemeSource::Builtin(builtin) => Ok(Theme::shared_builtin(builtin)),
            ThemeSource::Folder(_) => tokio::task::spawn_blocking(move || source.load_shared())
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("loading theme failed: {e}"))),
        };
        loaded.unwrap_or_else(|e| {
            error!("invalid custom theme, using the built-in template: {e:#}");
            Theme::shared_builtin(Builtin::Default)
        })
    }
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(overlay_page))
        .route("/preview", get(preview::page))
        .route("/preview/frame", get(preview::frame))
        .route("/preview/events", get(preview::events))
        .route("/theme/{*path}", get(theme_file))
        .route("/themes/{name}/{*path}", get(named_theme_file))
        .route("/events", get(events))
        .route("/avatar/{key}", get(avatars::avatar))
        .route("/api/v1/ws", get(api::ws))
        .route("/widgets/{name}", get(widgets::page))
        .with_state(state)
        // Runs before every route: requests for another host never reach
        // them (see `local`).
        .layer(axum::middleware::from_fn(local::only_local_hosts))
}

/// Serves until `state.shutdown` is cancelled.
pub async fn serve(listener: TcpListener, state: ServerState) -> std::io::Result<()> {
    let shutdown = state.shutdown.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
}

// OBS caches aggressively; `no-cache` makes it revalidate on refresh, so
// edited CSS actually shows up.
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-cache");

async fn overlay_page(
    State(state): State<ServerState>,
    Query(query): Query<OverlayQuery>,
) -> impl IntoResponse {
    let choice = state.choose_theme(query.theme.as_deref()).await;
    let source = state.source_for(&choice);
    let manifest = state.load_manifest(source.clone()).await;
    let (newest, resolved) = {
        let overlay = state.overlay.borrow();
        let values = lookup_theme_values(&overlay.theme_options, &source.key());
        (overlay.newest.as_str(), manifest.resolve(values))
    };
    // The default direction and active style classes go into attributes of the
    // chat element, outside the script: the script's hash in the CSP stays the
    // same.
    let mut page = OVERLAY_HTML.replacen(
        r#"data-newest="bottom""#,
        &format!(r#"data-newest="{newest}""#),
        1,
    );
    if !resolved.classes.is_empty() {
        let joined = resolved.classes.join(" ");
        page = page.replacen(
            r#"<main class="chat""#,
            &format!(r#"<main class="chat {joined}""#),
            1,
        );
        page = page.replacen(
            r#"data-style-classes="""#,
            &format!(r#"data-style-classes="{joined}""#),
            1,
        );
    }
    // A pinned theme's stylesheet, written into the page rather than
    // swapped by the script: the script reads `--max-messages` from the
    // stylesheet, which must be loaded by then.
    if let ThemeChoice::Pinned { name, .. } = choice {
        page = page.replacen(
            r#"href="theme/overlay.css""#,
            &format!(r#"href="themes/{}/overlay.css""#, url_segment(&name)),
            1,
        );
    }
    (
        [
            NO_CACHE,
            NO_SNIFF,
            (header::CONTENT_SECURITY_POLICY, OVERLAY_CSP.as_str()),
        ],
        Html(page),
    )
}

/// A theme name as one URL path segment: letters and digits stay, anything
/// else (spaces, `ä`) is percent-encoded. The result only has characters
/// that are safe in an HTML attribute too.
fn url_segment(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The app's theme's files. The CSS lives under `/theme/` too, so a
/// relative URL in it (`url(bg.png)`) resolves to `/theme/bg.png`: a file
/// in the same theme folder.
async fn theme_file(State(state): State<ServerState>, UrlPath(path): UrlPath<String>) -> Response {
    serve_theme_file(&state, state.current_theme(), &path).await
}

/// A pinned theme's files (`/?theme=<name>`), the same way: its CSS's
/// `url(bg.png)` resolves to `/themes/<name>/bg.png`. Unknown names are
/// not found, instead of falling back like the page: a stylesheet from
/// another theme would only mix two looks.
async fn named_theme_file(
    State(state): State<ServerState>,
    UrlPath((name, path)): UrlPath<(String, String)>,
) -> Response {
    match state.choose_theme(Some(&name)).await {
        ThemeChoice::Pinned { source, .. } => serve_theme_file(&state, source, &path).await,
        ThemeChoice::App => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `path` from the theme `source`: its CSS, falling back to the built-in
/// one, or one of its folder's files.
async fn serve_theme_file(state: &ServerState, source: ThemeSource, path: &str) -> Response {
    if path == chat_render::CSS_FILE {
        return overlay_css(state, source).await.into_response();
    }
    let Some(dir) = source.folder() else {
        return StatusCode::NOT_FOUND.into_response(); // built-in themes have no files
    };
    let Some(file) = inside(dir, path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Canonicalize both `dir` and `file` so symlinks inside a theme folder
    // can't escape to read files outside it (e.g. `../../config.toml`).
    let (Ok(canon_dir), Ok(canon_file)) = (
        tokio::fs::canonicalize(dir).await,
        tokio::fs::canonicalize(&file).await,
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !canon_file.starts_with(&canon_dir) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(&canon_file).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type(&file)),
                NO_CACHE,
                NO_SNIFF,
                // Theme files are images, fonts, CSS: nothing that needs
                // script. This stops script in e.g. an SVG, should one ever
                // be opened as a page instead of shown as an image.
                (header::CONTENT_SECURITY_POLICY, "script-src 'none'"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn overlay_css(state: &ServerState, source: ThemeSource) -> impl IntoResponse {
    let key = source.key();
    let (mut css, manifest) = match source {
        ThemeSource::Builtin(builtin) => (builtin.css().to_string(), builtin.manifest().clone()),
        ThemeSource::Folder(_) => {
            let (css_res, manifest_res) =
                tokio::task::spawn_blocking(move || (source.css(), source.manifest()))
                    .await
                    .unwrap_or_else(|e| {
                        (
                            Err(anyhow::anyhow!("reading CSS failed: {e}")),
                            Err(anyhow::anyhow!("reading theme manifest failed: {e}")),
                        )
                    });
            let css = css_res.unwrap_or_else(|e| {
                error!("can't read custom CSS, using the built-in one: {e:#}");
                chat_render::DEFAULT_CSS.to_string()
            });
            let manifest = manifest_res.unwrap_or_else(|e| {
                error!("invalid theme manifest, ignoring options: {e:#}");
                ThemeManifest::default()
            });
            (css, manifest)
        }
    };
    let overrides: ResolvedStyle = {
        let overlay = state.overlay.borrow();
        let values = lookup_theme_values(&overlay.theme_options, &key);
        manifest.resolve_overrides(values)
    };
    if !overrides.vars.is_empty() {
        css.push_str(&overrides.to_css());
    }
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            NO_CACHE,
            NO_SNIFF,
        ],
        css,
    )
}

/// `dir/relative`, but only if that stays inside `dir`: a request like
/// `/theme/..%2F..%2Fconfig.toml` must not read files outside the theme
/// (the config file holds the client secret). Only plain path segments are
/// allowed: no `..`, no absolute paths, no Windows drive prefixes.
fn inside(dir: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        .then(|| dir.join(relative))
}

fn content_type(file: &Path) -> &'static str {
    let extension = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

async fn events(State(state): State<ServerState>, Query(query): Query<OverlayQuery>) -> Response {
    // Counted as connected for as long as this response stream exists.
    let Some(connected) = state.connections.try_overlay() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many overlays connected.",
        )
            .into_response();
    };
    let choice = state.choose_theme(query.theme.as_deref()).await;
    let source = state.source_for(&choice);
    let style_key = source.key();
    let theme = state.load_theme(source.clone()).await;
    let manifest = state.load_manifest(source).await;
    let filter = OverlayFilter::new(&state.overlay, query.hide.as_deref());
    let (history, rx) = state.hub.subscribe();

    // Hidden messages are left out before anything else: they take no
    // pacing slot, and the replay shows the visible ones it has.
    let history: Vec<ChatEvent> = history
        .into_iter()
        .filter(|msg| filter.shows_message(msg))
        .map(ChatEvent::Message)
        .collect();
    let replay = stream::iter(history);
    let live = BroadcastStream::new(rx).filter_map(move |item| {
        let event = match item {
            Ok(event) => Some(event).filter(|event| filter.shows(event)),
            // This overlay fell more than the broadcast buffer behind: the
            // missed events are gone, carry on with the current ones.
            Err(BroadcastStreamRecvError::Lagged(missed)) => {
                warn!(missed, "overlay fell behind, skipped events");
                None
            }
        };
        std::future::ready(event)
    });
    // Live events go through the pacer (its own task per overlay); the
    // replay above doesn't: after a reload the history should appear at once.
    // The two branches produce different stream types, so both are boxed
    // into one type ("type erasure") to fit in the same variable.
    // `Stagger` is `Copy`: copying it out releases the watch's read lock right
    // away instead of holding it across the overlay's lifetime.
    let pacing = state.overlay.borrow().stagger;
    let live: BoxStream<'static, ChatEvent> = if pacing.is_off() {
        live.boxed()
    } else {
        let (tx, paced) = tokio::sync::mpsc::channel(256);
        tokio::spawn(stagger::run(live, tx, pacing));
        tokio_stream::wrappers::ReceiverStream::new(paced).boxed()
    };

    // Tells the overlay to reload whenever `state.reload` ticks (theme or
    // direction change, profile switch, or manual "Reload overlays"). Overlays
    // with a pinned theme reload too and come back with their own theme (it's
    // in their URL).
    //
    // A `watch` receiver remembers which version it last saw, and a clone
    // copies that memory. Mark the current generation as seen first, so a
    // freshly connected overlay only reloads on *future* reload ticks.
    let mut reload_rx = state.reload.clone();
    reload_rx.borrow_and_update();
    let reload = WatchStream::from_changes(reload_rx)
        .map(|_| Event::default().event("reload").data("theme changed"));

    // Push live theme option changes (`event: style`) whenever this overlay's
    // resolved style changes, without reloading the page.
    let mut style_rx = state.overlay.clone();
    let initial_style = {
        let overlay = style_rx.borrow_and_update();
        let values = lookup_theme_values(&overlay.theme_options, &style_key);
        manifest.resolve(values)
    };
    let style_events = WatchStream::from_changes(style_rx).filter_map({
        let mut last = initial_style;
        move |overlay| {
            let values = lookup_theme_values(&overlay.theme_options, &style_key);
            let next = manifest.resolve(values);
            let event = if next != last {
                last = next.clone();
                serde_json::to_string(&next)
                    .ok()
                    .map(|json| Event::default().event("style").data(json))
            } else {
                None
            };
            std::future::ready(event)
        }
    });

    let avatars = state.avatars.clone();
    let events = replay
        .chain(live)
        .filter_map(move |event| std::future::ready(to_sse(&theme, &avatars, &event)));
    // `select` merges the streams: whichever has something next goes first.
    let stream = stream::select(stream::select(events, reload), style_events)
        .map(Ok::<_, Infallible>)
        // An SSE response never ends by itself, and graceful shutdown waits
        // for open responses to finish: without this, shutdown would hang
        // for as long as an overlay is connected.
        .take_until(state.shutdown.cancelled_owned())
        // The closure owns the guard, so the guard lives exactly as long as
        // the stream: when the overlay disconnects, the stream is dropped
        // and the count goes down.
        .map(move |event| {
            let _connected = &connected;
            event
        });

    // Comment lines every 15s keep idle connections (and OBS) from timing out.
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// One named SSE event per chat event; the overlay script listens by name.
/// Messages are sent as rendered HTML (with local avatar URLs), moderation
/// events as the same JSON the API uses.
fn to_sse(theme: &Theme, avatars: &Avatars, event: &ChatEvent) -> Option<Event> {
    let (name, data) = match event {
        ChatEvent::Message(msg) => match theme.render(&avatars.for_overlay(msg)) {
            Ok(html) => ("chat", html),
            Err(e) => {
                error!(id = %msg.id, "failed to render message: {e:#}");
                return None;
            }
        },
        ChatEvent::Delete { .. } => ("delete", api::to_json(event)),
        ChatEvent::ClearUser { .. } => ("clear_user", api::to_json(event)),
        ChatEvent::ClearAll { .. } => ("clear_all", api::to_json(event)),
    };
    Some(Event::default().event(name).data(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::{Author, ChatMessage, ChatPlatform, MessageKind};
    use std::net::SocketAddr;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    pub(crate) const WAIT: Duration = Duration::from_secs(5);

    pub(crate) struct TestServer {
        pub(crate) addr: SocketAddr,
        pub(crate) hub: Arc<Hub>,
        pub(crate) shutdown: CancellationToken,
        pub(crate) task: JoinHandle<std::io::Result<()>>,
        pub(crate) overlay: watch::Sender<OverlayConfig>,
        pub(crate) reload: watch::Sender<u64>,
        pub(crate) api: watch::Sender<bool>,
        pub(crate) connections: Arc<Connections>,
        pub(crate) origins: Arc<ApiOrigins>,
        pub(crate) avatars: Arc<Avatars>,
    }

    impl TestServer {
        fn set_theme(&self, theme: ThemeSource) {
            self.overlay.send_modify(|o| o.theme = theme);
            self.reload.send_modify(|n| *n += 1);
        }
    }

    pub(crate) async fn start(theme_dir: Option<PathBuf>) -> TestServer {
        start_with_themes(theme_dir, None).await
    }

    pub(crate) async fn start_with_themes(
        theme_dir: Option<PathBuf>,
        themes_dir: Option<PathBuf>,
    ) -> TestServer {
        // Nobody to ask, like headless mode: a test about asking says so.
        start_with(theme_dir, themes_dir, ApiOrigins::new([], false)).await
    }

    pub(crate) async fn start_with(
        theme_dir: Option<PathBuf>,
        themes_dir: Option<PathBuf>,
        origins: ApiOrigins,
    ) -> TestServer {
        start_full(theme_dir, themes_dir, None, origins).await
    }

    /// With a widgets folder (`widgets_dir`) as well.
    pub(crate) async fn start_with_widgets(widgets_dir: PathBuf) -> TestServer {
        start_full(None, None, Some(widgets_dir), ApiOrigins::new([], false)).await
    }

    async fn start_full(
        theme_dir: Option<PathBuf>,
        themes_dir: Option<PathBuf>,
        widgets_dir: Option<PathBuf>,
        origins: ApiOrigins,
    ) -> TestServer {
        let origins = Arc::new(origins);
        // Port 0: the OS picks a free port, so tests can run in parallel.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hub = Arc::new(Hub::new(20));
        let shutdown = CancellationToken::new();
        let source = theme_dir.map_or_else(ThemeSource::default, ThemeSource::Folder);
        let (overlay_tx, overlay_rx) = watch::channel(OverlayConfig {
            theme: source,
            ..OverlayConfig::default()
        });
        let (reload_tx, reload_rx) = watch::channel(0u64);
        let connections = Arc::new(Connections::default());
        // On in tests: most of them are about the API.
        let (api_tx, api_rx) = watch::channel(true);
        let avatars = Arc::new(Avatars::for_tests());
        let state = ServerState {
            hub: hub.clone(),
            overlay: overlay_rx,
            reload: reload_rx,
            shutdown: shutdown.clone(),
            api: api_rx,
            themes_dir,
            widgets_dir,
            connections: connections.clone(),
            origins: origins.clone(),
            avatars: avatars.clone(),
        };
        let task = tokio::spawn(serve(listener, state));
        TestServer {
            addr,
            hub,
            shutdown,
            task,
            overlay: overlay_tx,
            reload: reload_tx,
            api: api_tx,
            connections,
            origins,
            avatars,
        }
    }

    pub(crate) fn message(id: &str) -> ChatEvent {
        ChatEvent::Message(ChatMessage {
            id: id.into(),
            platform: ChatPlatform::Twitch,
            author: Author {
                id: "u1".into(),
                name: "Ann".into(),
                color: None,
                badges: vec![],
                badge_urls: std::collections::BTreeMap::new(),
                avatar_url: None,
            },
            text: format!("text of {id}"),
            emotes: vec![],
            timestamp: jiff::Timestamp::now(),
            kind: MessageKind::Text,
        })
    }

    /// Reads from an open SSE response until `needle` shows up.
    async fn read_until(resp: &mut reqwest::Response, seen: &mut String, needle: &str) {
        timeout(WAIT, async {
            while !seen.contains(needle) {
                let chunk = resp.chunk().await.unwrap().expect("stream ended early");
                seen.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; got:\n{seen}"));
    }

    #[tokio::test]
    async fn serves_page_and_css() {
        let server = start(None).await;
        let base = format!("http://{}", server.addr);

        let page = reqwest::get(&base).await.unwrap().text().await.unwrap();
        assert!(page.contains(r#"<main class="chat""#));
        assert!(page.contains(r#"new EventSource("events" + location.search)"#));
        assert!(page.contains(r#"href="theme/overlay.css""#));

        let resp = reqwest::get(format!("{base}/theme/overlay.css"))
            .await
            .unwrap();
        assert!(
            resp.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/css")
        );
        assert!(resp.text().await.unwrap().contains("--max-messages"));
    }

    #[tokio::test]
    async fn sse_replays_history_then_streams_live_events() {
        let server = start(None).await;
        server.hub.publish(message("old"));

        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"data-id="old""#).await;
        assert!(seen.contains("event: chat"), "{seen}");

        // Subscribed now (history arrived), so live events reach us too.
        server.hub.publish(ChatEvent::Delete {
            platform: ChatPlatform::Twitch,
            message_id: "old".into(),
        });
        server.hub.publish(message("new"));
        read_until(&mut resp, &mut seen, r#"data-id="new""#).await;
        assert!(seen.contains("event: delete"), "{seen}");
        assert!(seen.contains(r#""message_id":"old""#), "{seen}");
    }

    /// A StreamElements tip: hidden by `streamelements` and by `donation`.
    fn tip(id: &str) -> ChatEvent {
        let ChatEvent::Message(mut msg) = message(id) else {
            unreachable!()
        };
        msg.platform = ChatPlatform::StreamElements;
        msg.kind = MessageKind::Donation {
            amount: "10".into(),
            tier: Some(4),
            value: None,
            currency: None,
        };
        ChatEvent::Message(msg)
    }

    #[tokio::test]
    async fn hidden_messages_never_reach_the_overlay() {
        let server = start(None).await;
        server
            .overlay
            .send_modify(|o| o.hide = vec![Hide::StreamElements]);
        // In the history...
        server.hub.publish(tip("old-tip"));
        server.hub.publish(message("old-chat"));

        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"data-id="old-chat""#).await;
        // ...and live. Messages arrive in order, so once "chat" is here, the
        // tip before it would have been too.
        server.hub.publish(tip("tip"));
        server.hub.publish(message("chat"));
        read_until(&mut resp, &mut seen, r#"data-id="chat""#).await;
        assert!(!seen.contains(r#"data-id="old-tip""#), "{seen}");
        assert!(!seen.contains(r#"data-id="tip""#), "{seen}");

        // The app shows tips again: the next one gets through, no reload.
        server.overlay.send_modify(|o| o.hide.clear());
        server.hub.publish(tip("new-tip"));
        read_until(&mut resp, &mut seen, r#"data-id="new-tip""#).await;
    }

    #[tokio::test]
    async fn an_overlay_url_hides_its_own_choice() {
        let server = start(None).await;
        // The app hides nothing; this overlay hides donations.
        let mut resp = reqwest::get(format!("http://{}/events?hide=donation", server.addr))
            .await
            .unwrap();
        eventually(|| server.connections.overlays() == 1).await;
        server.hub.publish(tip("tip"));
        server.hub.publish(message("chat"));
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"data-id="chat""#).await;
        assert!(!seen.contains(r#"data-id="tip""#), "{seen}");

        // The app hides tips; an empty `?hide=` still shows everything.
        server
            .overlay
            .send_modify(|o| o.hide = vec![Hide::StreamElements]);
        let mut resp = reqwest::get(format!("http://{}/events?hide=", server.addr))
            .await
            .unwrap();
        let mut seen = String::new();
        // Both in the history now.
        read_until(&mut resp, &mut seen, r#"data-id="chat""#).await;
        assert!(seen.contains(r#"data-id="tip""#), "{seen}");
    }

    #[tokio::test]
    async fn overlays_connecting_after_a_stagger_change_use_the_new_values() {
        let server = start(None).await;
        // Very slow pacing: a burst of three would take 4 s...
        server.overlay.send_modify(|o| {
            o.stagger = Stagger::new(Duration::from_secs(2), Duration::from_secs(5));
        });
        // ...but it's switched off before this overlay connects.
        server.overlay.send_modify(|o| o.stagger = Stagger::off());
        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();
        eventually(|| server.connections.overlays() == 1).await;

        for id in ["b1", "b2", "b3"] {
            server.hub.publish(message(id));
        }
        let mut seen = String::new();
        timeout(
            Duration::from_secs(1),
            read_until(&mut resp, &mut seen, r#"data-id="b3""#),
        )
        .await
        .expect("unpaced: the whole burst arrives at once");
    }

    #[tokio::test]
    async fn overlay_page_only_allows_its_own_script() {
        let server = start(None).await;
        let resp = reqwest::get(format!("http://{}/", server.addr))
            .await
            .unwrap();
        let csp = resp.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
        let page = resp.text().await.unwrap();

        // One inline script, allowed by the hash of exactly what's served.
        assert_eq!(page.matches("<script").count(), 1, "one script only");
        let script = page
            .split_once("<script>")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .unwrap()
            .0;
        let hash = base64::engine::general_purpose::STANDARD
            .encode(<sha2::Sha256 as sha2::Digest>::digest(script.as_bytes()));
        let script_src = csp
            .split(';')
            .map(str::trim)
            .find(|rule| rule.starts_with("script-src"))
            .expect("a script-src rule");
        assert_eq!(script_src, format!("script-src 'sha256-{hash}'"));
        // Nothing falls back to "allow everything".
        assert!(csp.starts_with("default-src 'none';"), "{csp}");
        assert!(csp.contains("connect-src 'self'"), "{csp}");
    }

    #[tokio::test]
    async fn overlay_page_carries_the_default_direction() {
        let server = start(None).await;
        let page = |server: &TestServer| {
            let url = format!("http://{}/", server.addr);
            async move { reqwest::get(url).await.unwrap().text().await.unwrap() }
        };
        assert!(page(&server).await.contains(r#"data-newest="bottom""#));
        server.overlay.send_modify(|o| o.newest = Newest::Top);
        let top = page(&server).await;
        assert!(top.contains(r#"data-newest="top""#), "{top}");
        assert!(!top.contains(r#"data-newest="bottom""#));
    }

    #[tokio::test]
    async fn broken_custom_template_falls_back_to_builtin() {
        let dir = std::env::temp_dir().join(format!("chat-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("message.html"), "{% if %}").unwrap();

        let server = start(Some(dir.clone())).await;
        server.hub.publish(message("m1"));
        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"<article class="msg"#).await;

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Polls until `condition` holds (the server updates counters in its own
    /// tasks, so the test can't know the exact moment).
    pub(crate) async fn eventually(condition: impl Fn() -> bool) {
        timeout(WAIT, async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("condition not reached in time");
    }

    #[tokio::test]
    async fn overlays_are_counted_while_connected() {
        let server = start(None).await;
        server.hub.publish(message("m1"));
        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"data-id="m1""#).await;
        assert_eq!(server.connections.overlays(), 1);

        drop(resp); // the overlay goes away
        // The server notices on its next write to the closed connection.
        server.hub.publish(message("m2"));
        eventually(|| server.connections.overlays() == 0).await;
    }

    #[tokio::test]
    async fn theme_change_reloads_overlays_and_css() {
        let dir = std::env::temp_dir().join(format!("chat-server-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("overlay.css"), "/* my theme */").unwrap();

        let server = start(None).await;
        let base = format!("http://{}", server.addr);
        server.hub.publish(message("m1"));
        let mut resp = reqwest::get(format!("{base}/events")).await.unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"data-id="m1""#).await;
        assert!(
            !seen.contains("event: reload"),
            "no reload without a change"
        );

        server.set_theme(ThemeSource::Folder(dir.clone()));
        read_until(&mut resp, &mut seen, "event: reload").await;
        let css = reqwest::get(format!("{base}/theme/overlay.css"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(css, "/* my theme */");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Regression test: after a theme change, an overlay that connects
    /// later must not be told to reload (it already loads the new theme).
    /// Getting this wrong made overlays reload in an endless loop.
    #[tokio::test]
    async fn overlay_connecting_after_a_theme_change_does_not_reload() {
        let server = start(None).await;
        server.set_theme(ThemeSource::Folder(std::env::temp_dir()));

        server.hub.publish(message("m1"));
        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"data-id="m1""#).await;
        // Give a wrongly queued reload the chance to arrive, then check.
        server.hub.publish(message("m2"));
        read_until(&mut resp, &mut seen, r#"data-id="m2""#).await;
        assert!(!seen.contains("event: reload"), "reload loop: {seen}");
    }

    #[tokio::test]
    async fn theme_files_are_served_but_nothing_outside_the_theme() {
        let root = std::env::temp_dir().join(format!("chat-server-assets-{}", std::process::id()));
        let theme = root.join("themes").join("cozy");
        std::fs::create_dir_all(theme.join("fonts")).unwrap();
        std::fs::write(theme.join("bg.png"), [0x89, b'P', b'N', b'G']).unwrap();
        std::fs::write(theme.join("fonts").join("f.woff2"), "font").unwrap();
        std::fs::write(root.join("config.toml"), "client_secret = \"x\"").unwrap();

        let server = start(Some(theme.clone())).await;
        let base = format!("http://{}/theme", server.addr);

        let png = reqwest::get(format!("{base}/bg.png")).await.unwrap();
        assert_eq!(png.headers()["content-type"], "image/png");
        assert_eq!(
            png.bytes().await.unwrap().as_ref(),
            [0x89, b'P', b'N', b'G']
        );
        let font = reqwest::get(format!("{base}/fonts/f.woff2")).await.unwrap();
        assert_eq!(font.headers()["content-type"], "font/woff2");
        assert_eq!(font.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            font.headers()["content-security-policy"],
            "script-src 'none'"
        );

        // `%2F` is an encoded "/": the router decodes it into the path, so
        // this asks for ../../config.toml. It must not be served.
        for sneaky in ["..%2F..%2Fconfig.toml", "%2E%2E/%2E%2E/config.toml"] {
            let resp = reqwest::get(format!("{base}/{sneaky}")).await.unwrap();
            assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND, "{sneaky}");
        }
        let missing = reqwest::get(format!("{base}/nope.png")).await.unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

        std::fs::remove_dir_all(root).unwrap();
    }

    /// A themes folder with two themes, "studio" and "my theme" (a space, as
    /// theme names may have), each with its own template and CSS.
    fn themes_folder(test: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("chat-server-{test}-{}", std::process::id()));
        for name in ["studio", "my theme"] {
            let dir = root.join("themes").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("message.html"),
                format!(r#"<article class="msg" data-id="{{{{ id }}}}">{name}</article>"#),
            )
            .unwrap();
            std::fs::write(dir.join("overlay.css"), format!("/* {name} */")).unwrap();
            std::fs::write(dir.join("bg.png"), name).unwrap();
        }
        std::fs::write(root.join("config.toml"), "client_secret = \"x\"").unwrap();
        root
    }

    async fn get_text(url: String) -> (reqwest::StatusCode, String) {
        let resp = reqwest::get(url).await.unwrap();
        (resp.status(), resp.text().await.unwrap())
    }

    #[tokio::test]
    async fn overlay_url_pins_a_theme() {
        let root = themes_folder("pinned");
        // The app's theme is the built-in one; the URL picks "studio".
        let server = start_with_themes(None, Some(root.join("themes"))).await;
        let base = format!("http://{}", server.addr);

        let (_, page) = get_text(format!("{base}/?theme=studio&newest=top")).await;
        assert!(page.contains(r#"href="themes/studio/overlay.css""#), "{page}");
        assert!(!page.contains(r#"href="theme/overlay.css""#), "{page}");
        let (_, css) = get_text(format!("{base}/themes/studio/overlay.css")).await;
        assert_eq!(css, "/* studio */");
        // Relative URLs in its CSS resolve to its own folder.
        let (_, png) = get_text(format!("{base}/themes/studio/bg.png")).await;
        assert_eq!(png, "studio");

        // The event stream gets the page's query and renders with the
        // pinned template.
        server.hub.publish(message("m1"));
        let mut resp = reqwest::get(format!("{base}/events?theme=studio&newest=top"))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"data-id="m1">studio</article>"#).await;

        // A name that needs encoding in a URL.
        let (_, page) = get_text(format!("{base}/?theme=my%20theme")).await;
        assert!(
            page.contains(r#"href="themes/my%20theme/overlay.css""#),
            "{page}"
        );
        let (_, css) = get_text(format!("{base}/themes/my%20theme/overlay.css")).await;
        assert_eq!(css, "/* my theme */");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn unknown_or_hostile_theme_names_get_the_apps_theme() {
        let root = themes_folder("unknown");
        let server = start_with_themes(None, Some(root.join("themes"))).await;
        let base = format!("http://{}", server.addr);

        // `%2E%2E` is "..", `%2F` a "/": decoded before the name is checked.
        for name in ["nope", "%2E%2E", "studio%2F..%2F..", "%22%3E%3Cscript%3E"] {
            let (_, page) = get_text(format!("{base}/?theme={name}")).await;
            assert!(
                page.contains(r#"href="theme/overlay.css""#),
                "{name}: {page}"
            );
            assert!(!page.contains("themes/"), "{name}: {page}");
            let (status, _) = get_text(format!("{base}/themes/{name}/overlay.css")).await;
            assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{name}");
        }
        // Nothing outside a pinned theme's folder either.
        let (status, _) = get_text(format!("{base}/themes/studio/..%2F..%2Fconfig.toml")).await;
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

        // The stream falls back to the app's (here: built-in) template.
        server.hub.publish(message("m1"));
        let mut resp = reqwest::get(format!("{base}/events?theme=nope"))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"<article class="msg msg--text"#).await;

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn builtin_theme_names_pin_the_builtin_themes() {
        let root = themes_folder("default");
        let themes = root.join("themes");
        // The app uses "studio"; `?theme=default` still gets the built-in one.
        let server = start_with_themes(Some(themes.join("studio")), Some(themes)).await;
        let base = format!("http://{}", server.addr);

        let (_, page) = get_text(format!("{base}/?theme=default")).await;
        assert!(
            page.contains(r#"href="themes/default/overlay.css""#),
            "{page}"
        );
        let (_, css) = get_text(format!("{base}/themes/default/overlay.css")).await;
        assert_eq!(css, chat_render::DEFAULT_CSS);

        server.hub.publish(message("m1"));
        let mut resp = reqwest::get(format!("{base}/events?theme=default"))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"<article class="msg msg--text"#).await;
        assert!(!seen.contains("studio</article>"), "{seen}");

        // The other built-in themes work the same way, in any capitalisation.
        let (_, page) = get_text(format!("{base}/?theme=Minimal")).await;
        assert!(
            page.contains(r#"href="themes/minimal/overlay.css""#),
            "{page}"
        );
        let (_, css) = get_text(format!("{base}/themes/minimal/overlay.css")).await;
        assert_eq!(css, Builtin::Minimal.css());
        let mut resp = reqwest::get(format!("{base}/events?theme=minimal"))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, r#"<div class="msg msg--text"#).await;

        let (_, css) = get_text(format!("{base}/themes/Cozy/overlay.css")).await;
        assert_eq!(css, Builtin::Cozy.css());

        std::fs::remove_dir_all(root).unwrap();
    }

    /// DNS rebinding: a request that reaches us but was meant for another
    /// site (its `Host` says so) gets nothing, on any route.
    #[tokio::test]
    async fn requests_for_other_hosts_are_refused() {
        let server = start(None).await;
        let client = reqwest::Client::new();
        let port = server.addr.port();
        let get = |path: &str, host: String| {
            client
                .get(format!("http://{}{path}", server.addr))
                .header("host", host)
                .send()
        };
        for path in ["/", "/theme/overlay.css", "/events", "/api/v1/ws"] {
            let resp = get(path, format!("evil.example:{port}")).await.unwrap();
            assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN, "{path}");
        }
        for host in ["localhost", "127.0.0.1", "[::1]"] {
            let resp = get("/", format!("{host}:{port}")).await.unwrap();
            assert_eq!(resp.status(), reqwest::StatusCode::OK, "{host}");
        }
    }

    #[tokio::test]
    async fn shutdown_completes_with_an_overlay_connected() {
        let server = start(None).await;
        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();

        server.shutdown.cancel();
        timeout(WAIT, server.task)
            .await
            .expect("graceful shutdown hung on the open SSE stream")
            .unwrap()
            .unwrap();
        // ...and the overlay's stream was ended, not left dangling.
        let end = timeout(WAIT, async {
            while resp.chunk().await.ok().flatten().is_some() {}
        });
        end.await.expect("SSE stream still open after shutdown");
    }

    #[tokio::test]
    async fn theme_options_apply_to_css_page_and_live_sse() {
        use chat_render::options::{ThemeValue, ThemeValues};

        let server = start(None).await;
        let base = format!("http://{}", server.addr);

        let mut opts = ThemeValues::new();
        opts.insert("text_color".into(), ThemeValue::Text("#ffaa00".into()));
        opts.insert("show_timestamps".into(), ThemeValue::Bool(true));
        server.overlay.send_modify(|o| {
            o.theme_options.insert("default".into(), opts);
        });

        let (_, page) = get_text(format!("{base}/")).await;
        assert!(page.contains(r#"class="chat chat--show-time""#), "{page}");
        assert!(
            page.contains(r#"data-style-classes="chat--show-time""#),
            "{page}"
        );
        let (_, css) = get_text(format!("{base}/theme/overlay.css")).await;
        assert!(css.contains("--chat-text: #ffaa00;"), "{css}");

        // Connected overlay receives live style updates without reloading.
        let mut resp = reqwest::get(format!("{base}/events")).await.unwrap();
        eventually(|| server.connections.overlays() == 1).await;

        server.overlay.send_modify(|o| {
            o.theme_options.clear();
        });
        let mut seen = String::new();
        read_until(&mut resp, &mut seen, "event: style").await;
        assert!(seen.contains(r#""--chat-text":"#), "{seen}");
        assert!(seen.contains(r#""classes":[]"#), "{seen}");
        assert!(!seen.contains("event: reload"), "must not reload: {seen}");

        // After reset, CSS returns to exact built-in stylesheet.
        let (_, css) = get_text(format!("{base}/theme/overlay.css")).await;
        assert_eq!(css, chat_render::DEFAULT_CSS);
    }

    #[tokio::test]
    async fn pinned_theme_gets_its_own_theme_options_live() {
        use chat_render::options::{ThemeValue, ThemeValues};

        let server = start(None).await;
        let base = format!("http://{}", server.addr);

        let mut resp = reqwest::get(format!("{base}/events?theme=minimal"))
            .await
            .unwrap();
        eventually(|| server.connections.overlays() == 1).await;

        let mut opts = ThemeValues::new();
        opts.insert("show_platform".into(), ThemeValue::Bool(false));
        opts.insert("font_size".into(), ThemeValue::Number(28.0));
        server.overlay.send_modify(|o| {
            o.theme_options.insert("minimal".into(), opts);
        });

        let mut seen = String::new();
        read_until(&mut resp, &mut seen, "event: style").await;
        assert!(seen.contains(r#""--chat-font-size":"28px""#), "{seen}");
        assert!(seen.contains("chat--hide-platform"), "{seen}");
    }
}
