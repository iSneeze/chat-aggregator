//! Streamlabs source: tips sent through Streamlabs' tip page, shown as
//! donations next to the chat. What's known about Streamlabs' Socket API is
//! in STREAMLABS.md, the maintainer's research notes (kept outside the
//! public repository); `examples/raw_streamlabs.rs` captures its raw
//! frames.
//!
//! Connects with the account's Socket API Token. The socket also brings the
//! account's settings, statistics and the platforms' relayed events
//! (Twitch, YouTube), which the platform sources show already: only the tip
//! page's donations are kept. Test donations from the dashboard count like
//! real ones, the alert box editor's previews never do (decided
//! 2026-11-07).
//!
//! The Socket API is socket.io 2 (Engine.IO 3), which the Rust socket.io
//! crates don't speak (they're on version 4). The little it takes is done
//! by hand on a plain WebSocket: read the open packet's timing, ping, and
//! read events.

mod frames;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::tiers::TipSettings;
use chat_core::{Activity, ChatEvent, ChatSource, Reporter, SetupError};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{debug, info, warn};

use frames::Frame;

const SOCKETS: &str = "wss://sockets.streamlabs.com/socket.io/";
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// A connection that lasted this long worked: the wait starts over.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// The longest label: it's shown in the app's source list.
const LABEL_MAX: usize = 40;

pub struct StreamlabsSource {
    /// The streamer's name for this account (passed [`normalize_label`]):
    /// a streamer with two personas has two accounts, and the token doesn't
    /// say whose it is.
    pub label: String,
    /// The account's Socket API Token (the engine keeps it in the secret
    /// store under the label, never in `config.toml`).
    pub token: String,
    /// The streamer's tip settings (currency, tiers), watched: a change in
    /// the app applies to the next donation, without reconnecting.
    pub tips: watch::Receiver<TipSettings>,
}

/// The label as typed, checked: it names the account in the app and keys
/// its token, so an empty one (or one with line breaks) won't do.
pub fn normalize_label(input: &str) -> Result<String, String> {
    let label = input.trim();
    if label.is_empty() {
        return Err("give this Streamlabs account a name, like \"Main\"".into());
    }
    if label.chars().count() > LABEL_MAX {
        return Err(format!("the name is longer than {LABEL_MAX} characters"));
    }
    if label.chars().any(char::is_control) {
        return Err("the name can't contain line breaks or tabs".into());
    }
    Ok(label.to_owned())
}

/// Streamlabs refused the token (its reason in `why`): only the user can
/// fix that (copy the token again), so it's a `SetupError`, and the source
/// isn't restarted.
fn token_refused(why: &str) -> SetupError {
    SetupError(format!(
        "Streamlabs refused the Socket API Token ({why}): copy it again from the Streamlabs \
         dashboard (Settings → API Settings → API Tokens), then remove this source and add it \
         again"
    ))
}

/// Why a connection ended without an error.
enum Ended {
    /// Streamlabs closed it; connect again after a pause.
    Closed,
    /// Nobody listens to our events anymore: stop.
    ReceiverGone,
}

impl ChatSource for StreamlabsSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        let tls = tls_config()?;
        // Reconnects by itself, like the StreamElements source: a network
        // hiccup isn't worth a restart by the engine.
        let mut delay = RETRY_FIRST;
        loop {
            let started = Instant::now();
            let problem = match self.read(&tls, &tx, &activity).await {
                Ok(Ended::ReceiverGone) => return Ok(()),
                Ok(Ended::Closed) => {
                    info!("Streamlabs closed the connection");
                    "connection closed by Streamlabs".to_owned()
                }
                // A wrong token stays wrong: the engine shows it to the user.
                Err(e) if e.is::<SetupError>() => return Err(e),
                Err(e) => {
                    let problem = self.without_token(&format!("{e:#}"));
                    warn!("Streamlabs ({}): {problem}", self.label);
                    format!("connection problem: {problem}")
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

impl StreamlabsSource {
    /// One connection: from connecting until it ends.
    async fn read(
        &self,
        tls: &Arc<rustls::ClientConfig>,
        tx: &mpsc::Sender<ChatEvent>,
        activity: &Reporter,
    ) -> anyhow::Result<Ended> {
        // The token goes into the address (as in Streamlabs' own example);
        // `EIO=3` asks for Engine.IO 3, `transport=websocket` skips the long
        // polling socket.io starts with otherwise.
        let url = format!(
            "{SOCKETS}?token={}&EIO=3&transport=websocket",
            percent_encoded(&self.token)
        );
        let connector = Connector::Rustls(Arc::clone(tls));
        let mut socket = match tokio_tungstenite::connect_async_tls_with_config(
            url,
            None,
            false,
            Some(connector),
        )
        .await
        {
            Ok((socket, _response)) => socket,
            // Only the status: the address (with the token in it) stays
            // out of the error, which ends up in the log and the app.
            Err(tungstenite::Error::Http(response)) => {
                bail!(
                    "Streamlabs refused the connection: HTTP {}",
                    response.status()
                )
            }
            Err(e) => return Err(e).context("connecting to Streamlabs"),
        };
        debug!("connected to Streamlabs ({})", self.label);

        let mut ping_interval = frames::PING_INTERVAL;
        let mut ping_timeout = frames::PING_TIMEOUT;
        let mut next_ping = Instant::now() + ping_interval;
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                frame = socket.next() => {
                    let Some(frame) = frame else {
                        return Ok(Ended::Closed);
                    };
                    heard = Instant::now();
                    match frame.context("reading from Streamlabs")? {
                        Message::Text(text) => {
                            // Decoded before the `match`, not in its head: the
                            // settings' read lock from `borrow` would live
                            // through the whole `match`, across the `.await`
                            // below, and a lock held across an `.await` makes
                            // the task unsendable between threads.
                            let frame = frames::decode(&text, &self.tips.borrow());
                            match frame {
                                Frame::Open { interval, timeout } => {
                                    ping_interval = interval;
                                    ping_timeout = timeout;
                                    next_ping = Instant::now() + ping_interval;
                                }
                                Frame::Connected => {
                                    info!("reading Streamlabs donations ({})", self.label);
                                    activity.set(Activity::Receiving);
                                }
                                Frame::Refused(why) => return Err(token_refused(&why).into()),
                                Frame::Donations(messages) => {
                                    for message in messages {
                                        if tx.send(ChatEvent::Message(message)).await.is_err() {
                                            return Ok(Ended::ReceiverGone);
                                        }
                                    }
                                }
                                Frame::Ignored => {}
                            }
                        }
                        Message::Close(close) => {
                            debug!(?close, "Streamlabs said goodbye");
                            return Ok(Ended::Closed);
                        }
                        // WebSocket-level keep-alive, answered by tungstenite.
                        _ => {}
                    }
                }
                () = tokio::time::sleep_until(next_ping) => {
                    // Engine.IO 3: the client pings, the server answers `3`.
                    // Without our pings, Streamlabs drops the connection.
                    socket.send(Message::text("2")).await.context("pinging Streamlabs")?;
                    next_ping = Instant::now() + ping_interval;
                }
                () = tokio::time::sleep_until(heard + ping_interval + ping_timeout) => {
                    bail!(
                        "no answer from Streamlabs for {} s",
                        (ping_interval + ping_timeout).as_secs()
                    );
                }
            }
        }
    }

    /// `text` with the token blanked out, in case an error quotes the
    /// address it failed on (the token is in it).
    fn without_token(&self, text: &str) -> String {
        if self.token.is_empty() {
            return text.to_owned();
        }
        text.replace(&percent_encoded(&self.token), "…")
            .replace(&self.token, "…")
    }
}

/// The token for a URL's query: everything but letters, digits and `-._~`
/// as `%XX`. (Socket tokens look URL-safe already; this makes sure.)
fn percent_encoded(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// TLS for the WebSocket, with the crypto backend chosen explicitly (see
/// chat-streamelements' `tls_config` for why). The safe defaults include
/// TLS 1.2, which sockets.streamlabs.com needs, thanks to rustls' `tls12`
/// feature (Cargo.toml).
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
    fn labels_are_trimmed_and_checked() {
        assert_eq!(normalize_label("  Corpo \n").unwrap(), "Corpo");
        assert_eq!(normalize_label("インディー").unwrap(), "インディー");
        for input in ["", "   ", "two\nlines", &"x".repeat(LABEL_MAX + 1)] {
            assert!(normalize_label(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn the_token_never_shows_in_errors() {
        let (tx, _rx) = watch::channel(TipSettings::default());
        let source = StreamlabsSource {
            label: "Main".into(),
            token: "abc.def+ghi".into(),
            tips: tx.subscribe(),
        };
        let error = format!("failed: {SOCKETS}?token=abc.def%2Bghi&EIO=3 (abc.def+ghi)");
        let shown = source.without_token(&error);
        assert!(!shown.contains("abc.def"), "{shown}");
        assert!(shown.contains("token=…&EIO=3"), "{shown}");
    }
}
