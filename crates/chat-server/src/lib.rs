//! HTTP side of the aggregator: the OBS overlay page, its stylesheet, and
//! the Server-Sent Events stream that feeds it.
//!
//! | route          | serves                                                   |
//! |----------------|----------------------------------------------------------|
//! | `/`            | the overlay page (add it to OBS as a Browser Source)     |
//! | `/overlay.css` | the theme's CSS, re-read on every request                |
//! | `/events`      | SSE: replay history, then live chat as rendered HTML     |
//! | `/api/v1/ws`   | WebSocket: live events as JSON (see `api`, docs/api.md)  |

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use chat_core::{ChatEvent, Hub};
use chat_render::Theme;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt, stream};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

mod api;
mod connections;
mod stagger;

pub use connections::Connections;
pub use stagger::Stagger;

const OVERLAY_HTML: &str = include_str!("overlay.html");

/// Shared by all request handlers. Cloned per request, which is cheap:
/// every field is an `Arc` or a small handle around one.
#[derive(Clone)]
pub struct ServerState {
    pub hub: Arc<Hub>,
    /// Folder with a custom `message.html` / `overlay.css`; `None` = built-in.
    /// A `watch` channel: the current value, plus a notification when it
    /// changes, which makes connected overlays reload.
    pub theme_dir: watch::Receiver<Option<PathBuf>>,
    /// Cancelled when the app shuts down.
    pub shutdown: CancellationToken,
    /// Spacing of message bursts in the overlay (not in the API).
    pub stagger: Stagger,
    pub connections: Arc<Connections>,
}

impl ServerState {
    fn current_theme_dir(&self) -> Option<PathBuf> {
        self.theme_dir.borrow().clone()
    }

    /// Loaded per overlay connection, so "edit the template, refresh the
    /// browser source" picks up changes without a restart. A broken custom
    /// template must not take the overlay down mid-stream: log it and fall
    /// back to the built-in one.
    fn load_theme(&self) -> Theme {
        let Some(dir) = self.current_theme_dir() else {
            return Theme::builtin();
        };
        // Blocking file I/O in an async handler is normally a no-go, but
        // this is two small local files, once per connection.
        Theme::load(&dir).unwrap_or_else(|e| {
            error!("invalid custom theme, using the built-in template: {e:#}");
            Theme::builtin()
        })
    }
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(overlay_page))
        .route("/overlay.css", get(overlay_css))
        .route("/events", get(events))
        .route("/api/v1/ws", get(api::ws))
        .with_state(state)
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

async fn overlay_page() -> impl IntoResponse {
    ([NO_CACHE], Html(OVERLAY_HTML))
}

async fn overlay_css(State(state): State<ServerState>) -> impl IntoResponse {
    let css = match state.current_theme_dir() {
        None => chat_render::DEFAULT_CSS.to_string(),
        Some(dir) => chat_render::read_css(dir).unwrap_or_else(|e| {
            error!("can't read custom CSS, using the built-in one: {e:#}");
            chat_render::DEFAULT_CSS.to_string()
        }),
    };
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8"), NO_CACHE],
        css,
    )
}

async fn events(
    State(state): State<ServerState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let theme = state.load_theme();
    let (history, rx) = state.hub.subscribe();

    let replay = stream::iter(history.into_iter().map(ChatEvent::Message));
    let live = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(event) => Some(event),
            // This overlay fell more than the broadcast buffer behind: the
            // missed events are gone, carry on with the current ones.
            Err(BroadcastStreamRecvError::Lagged(missed)) => {
                warn!(missed, "overlay fell behind, skipped events");
                None
            }
        }
    });
    // Live events go through the pacer (its own task per overlay); the
    // replay above doesn't: after a reload the history should appear at once.
    // The two branches produce different stream types, so both are boxed
    // into one type ("type erasure") to fit in the same variable.
    let live: BoxStream<'static, ChatEvent> = if state.stagger.is_off() {
        live.boxed()
    } else {
        let (tx, paced) = tokio::sync::mpsc::channel(256);
        tokio::spawn(stagger::run(live, tx, state.stagger));
        tokio_stream::wrappers::ReceiverStream::new(paced).boxed()
    };

    // Counted as connected for as long as this response stream exists.
    let connected = state.connections.overlay();
    // Tells the overlay to reload when the theme folder changes; it then
    // reconnects and gets the new template and CSS. `from_changes` skips the
    // current value: only actual changes count.
    let reload = WatchStream::from_changes(state.theme_dir.clone())
        .map(|_| Event::default().event("reload").data("theme changed"));

    let events = replay
        .chain(live)
        .filter_map(move |event| std::future::ready(to_sse(&theme, &event)));
    // `select` merges both streams: whichever has something next goes first.
    let stream = stream::select(events, reload)
        .map(Ok)
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
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// One named SSE event per chat event; the overlay script listens by name.
/// Messages are sent as rendered HTML, moderation events as the same JSON
/// the API uses.
fn to_sse(theme: &Theme, event: &ChatEvent) -> Option<Event> {
    let (name, data) = match event {
        ChatEvent::Message(msg) => match theme.render(msg) {
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
        pub(crate) theme_dir: watch::Sender<Option<PathBuf>>,
        pub(crate) connections: Arc<Connections>,
    }

    pub(crate) async fn start(theme_dir: Option<PathBuf>) -> TestServer {
        // Port 0: the OS picks a free port, so tests can run in parallel.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hub = Arc::new(Hub::new(20));
        let shutdown = CancellationToken::new();
        let (theme_tx, theme_rx) = watch::channel(theme_dir);
        let connections = Arc::new(Connections::default());
        let state = ServerState {
            hub: hub.clone(),
            theme_dir: theme_rx,
            shutdown: shutdown.clone(),
            stagger: Stagger::default(),
            connections: connections.clone(),
        };
        let task = tokio::spawn(serve(listener, state));
        TestServer {
            addr,
            hub,
            shutdown,
            task,
            theme_dir: theme_tx,
            connections,
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
                avatar_url: None,
            },
            text: format!("text of {id}"),
            emotes: vec![],
            timestamp: chrono::Utc::now(),
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
        assert!(page.contains(r#"new EventSource("events")"#));

        let resp = reqwest::get(format!("{base}/overlay.css")).await.unwrap();
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

        server.theme_dir.send_replace(Some(dir.clone()));
        read_until(&mut resp, &mut seen, "event: reload").await;
        let css = reqwest::get(format!("{base}/overlay.css"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(css, "/* my theme */");

        std::fs::remove_dir_all(&dir).unwrap();
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
}
