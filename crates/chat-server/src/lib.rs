//! HTTP side of the aggregator: the OBS overlay page, its stylesheet, and
//! the Server-Sent Events stream that feeds it.
//!
//! | route          | serves                                                   |
//! |----------------|----------------------------------------------------------|
//! | `/`            | the overlay page (add it to OBS as a Browser Source)     |
//! | `/overlay.css` | the theme's CSS, re-read on every request                |
//! | `/events`      | SSE: replay history, then live chat as rendered HTML     |

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
use futures_util::{Stream, StreamExt, stream};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

const OVERLAY_HTML: &str = include_str!("overlay.html");

/// Shared by all request handlers. Cloned per request, which is cheap:
/// an `Arc`, a token (also an `Arc` inside) and a short path.
#[derive(Clone)]
pub struct ServerState {
    pub hub: Arc<Hub>,
    /// Folder with a custom `message.html` / `overlay.css`; `None` = built-in.
    pub theme_dir: Option<PathBuf>,
    /// Cancelled when the app shuts down.
    pub shutdown: CancellationToken,
}

impl ServerState {
    /// Loaded per overlay connection, so "edit the template, refresh the
    /// browser source" picks up changes without a restart. A broken custom
    /// template must not take the overlay down mid-stream: log it and fall
    /// back to the built-in one.
    fn load_theme(&self) -> Theme {
        let Some(dir) = &self.theme_dir else {
            return Theme::builtin();
        };
        // Blocking file I/O in an async handler is normally a no-go, but
        // this is two small local files, once per connection.
        Theme::load(dir).unwrap_or_else(|e| {
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
    let css = match &state.theme_dir {
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

    let stream = replay
        .chain(live)
        .filter_map(move |event| std::future::ready(to_sse(&theme, event)))
        .map(Ok)
        // An SSE response never ends by itself, and graceful shutdown waits
        // for open responses to finish: without this, shutdown would hang
        // for as long as an overlay is connected.
        .take_until(state.shutdown.cancelled_owned());

    // Comment lines every 15s keep idle connections (and OBS) from timing out.
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// One named SSE event per chat event; the overlay script listens by name.
fn to_sse(theme: &Theme, event: ChatEvent) -> Option<Event> {
    let (name, data) = match event {
        ChatEvent::Message(msg) => match theme.render(&msg) {
            Ok(html) => ("chat", html),
            Err(e) => {
                error!(id = %msg.id, "failed to render message: {e:#}");
                return None;
            }
        },
        ChatEvent::Delete {
            platform,
            message_id,
        } => (
            "delete",
            json!({ "platform": platform.as_str(), "message_id": message_id }).to_string(),
        ),
        ChatEvent::ClearUser { platform, user_id } => (
            "clear_user",
            json!({ "platform": platform.as_str(), "user_id": user_id }).to_string(),
        ),
        ChatEvent::ClearAll { platform } => (
            "clear_all",
            json!({ "platform": platform.as_str() }).to_string(),
        ),
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

    const WAIT: Duration = Duration::from_secs(5);

    struct TestServer {
        addr: SocketAddr,
        hub: Arc<Hub>,
        shutdown: CancellationToken,
        task: JoinHandle<std::io::Result<()>>,
    }

    async fn start(theme_dir: Option<PathBuf>) -> TestServer {
        // Port 0: the OS picks a free port, so tests can run in parallel.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hub = Arc::new(Hub::new(20));
        let shutdown = CancellationToken::new();
        let state = ServerState {
            hub: hub.clone(),
            theme_dir,
            shutdown: shutdown.clone(),
        };
        let task = tokio::spawn(serve(listener, state));
        TestServer {
            addr,
            hub,
            shutdown,
            task,
        }
    }

    fn message(id: &str) -> ChatEvent {
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
