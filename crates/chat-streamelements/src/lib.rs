//! StreamElements source: tips sent through StreamElements' tip page, shown
//! as donations next to the chat. What's known about StreamElements' event
//! gateway (Astro) is in STREAMELEMENTS.md, the maintainer's research notes
//! (kept outside the public repository); `examples/raw_streamelements.rs`
//! captures its raw events.
//!
//! Reads the channel's activity feed (`channel.activities`) with the
//! streamer's Overlay Token: that token may not read the `channel.tips`
//! topic, but tips arrive in the activity feed too. Only tips are kept: the
//! feed also relays the platforms' own events (Super Chats, bits, subs),
//! which the platform sources already show. Tips emulated from the
//! StreamElements dashboard come through like real ones (decided
//! 2026-11-07: streamers emulate tips to rehearse their alerts and
//! reactions).

mod frames;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::tiers::TipSettings;
use chat_core::{Activity, ChatEvent, ChatSource, Reporter, SetupError};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use frames::Frame;

const ASTRO: &str = "wss://astro.streamelements.com";
/// Astro pings every 30 s, so this much silence means the connection is
/// dead without either side having noticed (a sleeping laptop, a dropped
/// route).
const SILENT_FOR: Duration = Duration::from_secs(75);
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// A connection that lasted this long worked: the wait starts over.
const STABLE_AFTER: Duration = Duration::from_secs(60);

pub struct StreamElementsSource {
    /// An id that passed [`normalize_account`].
    pub account: String,
    /// The channel's Overlay Token (the engine keeps it in the secret
    /// store, never in `config.toml`).
    pub token: String,
    /// The streamer's tip settings (currency, tiers), watched: a change in
    /// the app applies to the next tip, without reconnecting.
    pub tips: watch::Receiver<TipSettings>,
}

/// The account id from what a user typed or pasted ("Account ID" on the
/// StreamElements dashboard): 24 hex digits. A malformed id can never work,
/// so it's caught here instead of being retried like a network problem.
pub fn normalize_account(input: &str) -> Result<String, String> {
    let id = input.trim();
    if id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(id.to_ascii_lowercase())
    } else {
        Err(format!(
            "expected the 24-character Account ID from the StreamElements dashboard, got {id:?}"
        ))
    }
}

/// StreamElements refused the token for this account (Astro's reason in
/// `why`): only the user can fix that (copy the token and id again), so
/// it's a `SetupError`, and the source isn't restarted.
fn token_refused(why: &str) -> SetupError {
    SetupError(format!(
        "StreamElements refused the Overlay Token for this Account ID ({why}): copy both again \
         from the StreamElements dashboard, then remove this source and add it again"
    ))
}

/// Why a connection ended without an error.
enum Ended {
    /// StreamElements closed it; connect again after a pause.
    Closed,
    /// StreamElements announced a restart of its server: connect again
    /// right away (to another one).
    Restarting,
    /// Nobody listens to our events anymore: stop.
    ReceiverGone,
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl ChatSource for StreamElementsSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        let tls = tls_config()?;
        // Reconnects by itself, like the Picarto source: a network hiccup
        // isn't worth a restart by the engine.
        let mut delay = RETRY_FIRST;
        loop {
            let started = Instant::now();
            let problem = match self.read(&tls, &tx, &activity).await {
                Ok(Ended::ReceiverGone) => return Ok(()),
                // Astro's reconnect token would resume the old session, but
                // a fresh one is simpler and only a moment away. A tip in
                // that moment is missed, as with any reconnect.
                Ok(Ended::Restarting) => {
                    info!("StreamElements is restarting its server; reconnecting");
                    activity.set(Activity::Connecting);
                    continue;
                }
                Ok(Ended::Closed) => {
                    info!("StreamElements closed the connection");
                    "connection closed by StreamElements".to_owned()
                }
                // A wrong token stays wrong: the engine shows it to the user.
                Err(e) if e.is::<SetupError>() => return Err(e),
                Err(e) => {
                    warn!("StreamElements: {e:#}");
                    format!("connection problem: {e:#}")
                }
            };
            // Waits longer after each quick failure in a row, so a server
            // that keeps refusing isn't hammered.
            if started.elapsed() >= STABLE_AFTER {
                delay = RETRY_FIRST;
            }
            activity.set(Activity::Degraded(format!(
                "{problem}; reconnecting in {} s",
                delay.as_secs()
            )));
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RETRY_MAX);
            activity.set(Activity::Connecting);
        }
    }
}

impl StreamElementsSource {
    /// One connection: from connecting until it ends.
    async fn read(
        &self,
        tls: &Arc<rustls::ClientConfig>,
        tx: &mpsc::Sender<ChatEvent>,
        activity: &Reporter,
    ) -> anyhow::Result<Ended> {
        let connector = Connector::Rustls(Arc::clone(tls));
        let (mut socket, _response) =
            tokio_tungstenite::connect_async_tls_with_config(ASTRO, None, false, Some(connector))
                .await
                .context("connecting to StreamElements")?;
        debug!("connected to StreamElements");

        let mut heard = Instant::now();
        loop {
            tokio::select! {
                frame = socket.next() => {
                    let Some(frame) = frame else {
                        return Ok(Ended::Closed);
                    };
                    heard = Instant::now();
                    match frame.context("reading from StreamElements")? {
                        Message::Text(text) => match frames::decode(&text) {
                            // Astro's own examples wait for the welcome
                            // before subscribing.
                            Frame::Welcome => self.subscribe(&mut socket).await?,
                            Frame::Subscribed => {
                                info!("reading StreamElements tips");
                                activity.set(Activity::Receiving);
                            }
                            Frame::Refused(why) => return Err(token_refused(&why).into()),
                            Frame::Failed(why) => {
                                bail!("StreamElements refused the subscription: {why}")
                            }
                            Frame::Tip(tip) => {
                                // The tip settings as they are now. In a
                                // statement of its own: `borrow`'s read lock
                                // ends with it, and a lock held across the
                                // `.await` below would make the task
                                // unsendable between threads.
                                let message = tip.into_message(&self.tips.borrow());
                                if tx.send(ChatEvent::Message(message)).await.is_err() {
                                    return Ok(Ended::ReceiverGone);
                                }
                            }
                            Frame::Reconnect => return Ok(Ended::Restarting),
                            Frame::Ignored => {}
                        },
                        Message::Close(close) => {
                            debug!(?close, "StreamElements said goodbye");
                            return Ok(Ended::Closed);
                        }
                        // WebSocket-level keep-alive: tungstenite answers
                        // Astro's pings by itself.
                        _ => {}
                    }
                }
                () = tokio::time::sleep_until(heard + SILENT_FOR) => {
                    bail!("no sign of life from StreamElements for {} s", SILENT_FOR.as_secs());
                }
            }
        }
    }

    async fn subscribe(&self, socket: &mut Socket) -> anyhow::Result<()> {
        let command = frames::subscribe(&self.account, &self.token);
        socket
            .send(Message::text(command))
            .await
            .context("subscribing to StreamElements' activity feed")
    }
}

/// TLS for the WebSocket, with the crypto backend chosen explicitly.
/// Otherwise tungstenite asks rustls for the process-wide default, which
/// panics when rustls was built with both of its backends (`ring` from
/// chat-youtube's gRPC, `aws-lc-rs` from reqwest), as in the app. We take
/// aws-lc-rs, which reqwest uses anyway.
fn tls_config() -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ids_are_24_hex_digits() {
        assert_eq!(
            normalize_account("  0123456789ABCDEF01234567\n").unwrap(),
            "0123456789abcdef01234567"
        );
        for input in [
            "",
            "0123456789abcdef0123456",
            "0123456789abcdef0123456g",
            "your_channel",
        ] {
            assert!(normalize_account(input).is_err(), "{input:?}");
        }
    }
}
