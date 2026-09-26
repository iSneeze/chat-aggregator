//! The JSON WebSocket API: `GET /api/v1/ws`.
//!
//! Every `ChatEvent` is sent as one text frame of JSON, in the format fixed
//! by the serde attributes in `chat-core` (see docs/api.md and
//! docs/asyncapi.yaml). The connection is one-way for now: messages from the
//! client are read (to notice disconnects and answer pings) but ignored.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Query, State};
use axum::response::Response;
use chat_core::ChatEvent;
use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use crate::ServerState;

/// How often the server pings an idle client, so dead connections (client
/// crashed, network gone) are noticed and cleaned up.
const PING_EVERY: Duration = Duration::from_secs(30);

#[derive(serde::Deserialize)]
pub(crate) struct WsParams {
    /// `?history=true`: replay the recent messages first. Off by default so
    /// a game reacting to chat commands doesn't re-run old ones after a
    /// reconnect.
    #[serde(default)]
    history: bool,
}

/// The HTTP handler: agrees to switch the connection to the WebSocket
/// protocol, then hands the socket to `client`.
pub(crate) async fn ws(
    upgrade: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(state): State<ServerState>,
) -> Response {
    upgrade.on_upgrade(move |socket| client(socket, state, params.history))
}

/// Serves one connected client until it leaves or the server shuts down.
async fn client(mut socket: WebSocket, state: ServerState, history: bool) {
    // Counted as connected until this function returns, however it returns.
    let _connected = state.connections.api_client();
    let (snapshot, mut rx) = state.hub.subscribe();
    if history {
        for msg in snapshot {
            if send(&mut socket, to_json(&ChatEvent::Message(msg)))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    // First ping one interval from now, not immediately.
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);

    loop {
        // Wait for whichever happens first; the other branches are simply
        // dropped and re-created on the next loop iteration.
        tokio::select! {
            event = rx.recv() => {
                let json = match event {
                    Ok(event) => to_json(&event),
                    // More than the hub's buffer behind: those events are
                    // gone. Tell the client, then continue with current ones.
                    Err(RecvError::Lagged(missed)) => lagged_json(missed),
                    Err(RecvError::Closed) => break,
                };
                if send(&mut socket, json).await.is_err() {
                    break; // client went away
                }
            }
            incoming = socket.recv() => match incoming {
                // Client closed the connection, or it broke.
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                // Pings are answered automatically by the WebSocket library;
                // anything the client sends is ignored for now.
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
            // Say goodbye properly instead of just dropping the connection,
            // and don't hold up the server's shutdown.
            () = state.shutdown.cancelled() => {
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: close_code::AWAY,
                        reason: "server shutting down".into(),
                    })))
                    .await;
                break;
            }
        }
    }
    debug!("API client disconnected");
}

async fn send(socket: &mut WebSocket, json: String) -> Result<(), axum::Error> {
    socket.send(Message::Text(json.into())).await
}

/// One event as JSON. Shared with the overlay's SSE stream, so there's a
/// single JSON format for moderation events.
pub(crate) fn to_json(event: &ChatEvent) -> String {
    // serde_json only fails for things our types can't contain (e.g. maps
    // with non-string keys), so a failure here would be a programming error.
    serde_json::to_string(event).expect("ChatEvent always serializes to JSON")
}

/// Sent instead of events the client was too slow to receive.
fn lagged_json(missed: u64) -> String {
    serde_json::json!({ "type": "lagged", "missed": missed }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{WAIT, message, start};
    use futures_util::StreamExt;
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect(addr: std::net::SocketAddr, query: &str) -> Client {
        let (client, _) = connect_async(format!("ws://{addr}/api/v1/ws{query}"))
            .await
            .unwrap();
        client
    }

    /// Next text frame, parsed as JSON.
    async fn next_json(client: &mut Client) -> serde_json::Value {
        loop {
            let frame = timeout(WAIT, client.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("connection closed")
                .unwrap();
            if let WsMessage::Text(text) = frame {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    /// The server subscribes to the hub in its own task after the upgrade,
    /// so a test can't know exactly when it's subscribed. Publish until the
    /// first event arrives, then everything after that is live.
    async fn sync(server: &crate::tests::TestServer, client: &mut Client) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            server.hub.publish(message("sync"));
            if let Ok(Some(Ok(WsMessage::Text(_)))) =
                timeout(Duration::from_millis(50), client.next()).await
            {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "never subscribed");
        }
    }

    #[tokio::test]
    async fn live_events_arrive_as_json() {
        let server = start(None).await;
        let mut client = connect(server.addr, "").await;
        sync(&server, &mut client).await;
        // Drain any extra sync messages that raced in.
        server.hub.publish(message("live"));
        let mut json = next_json(&mut client).await;
        while json["id"] == "sync" {
            json = next_json(&mut client).await;
        }
        assert_eq!(json["type"], "message");
        assert_eq!(json["id"], "live");
        assert_eq!(json["kind"]["type"], "text");
    }

    #[tokio::test]
    async fn no_history_by_default_but_on_request() {
        let server = start(None).await;
        server.hub.publish(message("old"));

        let mut with = connect(server.addr, "?history=true").await;
        assert_eq!(next_json(&mut with).await["id"], "old");

        let mut without = connect(server.addr, "").await;
        sync(&server, &mut without).await;
        // What arrived first was a live "sync" message, not the old one;
        // make sure "old" doesn't show up later either.
        server.hub.publish(message("after"));
        loop {
            let json = next_json(&mut without).await;
            assert_ne!(json["id"], "old", "history sent without ?history=true");
            if json["id"] == "after" {
                break;
            }
        }
    }

    #[tokio::test]
    async fn api_clients_are_counted_while_connected() {
        let server = start(None).await;
        let mut client = connect(server.addr, "").await;
        sync(&server, &mut client).await;
        assert_eq!(server.connections.api_clients(), 1);

        client.close(None).await.unwrap();
        crate::tests::eventually(|| server.connections.api_clients() == 0).await;
    }

    #[test]
    fn lagged_notice_format() {
        let json: serde_json::Value = serde_json::from_str(&lagged_json(7)).unwrap();
        assert_eq!(json, serde_json::json!({ "type": "lagged", "missed": 7 }));
    }

    #[tokio::test]
    async fn shutdown_closes_clients_and_completes() {
        let server = start(None).await;
        let mut client = connect(server.addr, "").await;
        sync(&server, &mut client).await;

        server.shutdown.cancel();
        timeout(WAIT, server.task)
            .await
            .expect("shutdown hung with an API client connected")
            .unwrap()
            .unwrap();

        // The client got a proper Close frame (possibly after queued events).
        let closed = timeout(WAIT, async {
            while let Some(Ok(frame)) = client.next().await {
                if let WsMessage::Close(Some(close)) = frame {
                    return close.code;
                }
            }
            panic!("connection ended without a Close frame");
        })
        .await
        .unwrap();
        assert_eq!(u16::from(closed), close_code::AWAY);
    }
}
