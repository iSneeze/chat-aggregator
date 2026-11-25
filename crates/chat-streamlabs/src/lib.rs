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

use anyhow::bail;
use chat_core::tiers::TipSettings;
use chat_core::{Activity, ChatEvent, ChatSource, Reporter, SetupError};
use chat_websocket::tungstenite::{self, Message};
use chat_websocket::{Ended, Event, Keepalive, Session};
use tokio::sync::{mpsc, watch};
use tracing::{Instrument, debug, info, info_span};

use frames::Frame;

const SOCKETS: &str = "wss://sockets.streamlabs.com/socket.io/";

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
    chat_core::normalize_source_label(input, "Streamlabs account")
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

impl ChatSource for StreamlabsSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        // The label in every log line from here on (as
        // `streamlabs{label=…}`): with two accounts, the log says whose.
        let span = info_span!("streamlabs", label = %self.label);
        chat_websocket::keep_connected("Streamlabs", &activity, || self.read(&tx, &activity))
            .instrument(span)
            .await
    }
}

impl StreamlabsSource {
    /// One connection: from connecting until it ends.
    async fn read(
        &self,
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
        // The errors end up in the log and the app: only the status of a
        // refused handshake, and any other error without the token, in
        // case it quotes the address.
        let socket = match chat_websocket::connect(url).await {
            Ok(socket) => socket,
            Err(tungstenite::Error::Http(response)) => {
                bail!(
                    "Streamlabs refused the connection: HTTP {}",
                    response.status()
                )
            }
            Err(e) => bail!(
                "connecting to Streamlabs: {}",
                self.without_token(&e.to_string())
            ),
        };
        debug!("connected to Streamlabs");

        // Engine.IO 3: the client pings (`2`), the server answers `3`.
        // Without our pings, Streamlabs drops the connection.
        let mut session = Session::new(
            "Streamlabs",
            socket,
            Keepalive::with_ping(
                frames::PING_INTERVAL,
                Message::text("2"),
                frames::PING_INTERVAL + frames::PING_TIMEOUT,
            ),
        );
        loop {
            match session.next().await? {
                Event::Text(text) => match frames::decode(&text) {
                    Frame::Open { interval, timeout } => {
                        session.set_timing(interval, interval + timeout);
                    }
                    Frame::Connected => {
                        info!("reading Streamlabs donations");
                        activity.set(Activity::Receiving);
                    }
                    Frame::Refused(why) => return Err(token_refused(&why).into()),
                    Frame::Donations(donations) => {
                        for tip in donations {
                            // The tip settings as they are now. In a statement
                            // of its own: `borrow`'s read lock ends with it,
                            // and a lock held across the `.await` below would
                            // make the task unsendable between threads.
                            let message = tip.into_message(&self.tips.borrow());
                            if tx.send(ChatEvent::Message(message)).await.is_err() {
                                return Ok(Ended::ReceiverGone);
                            }
                        }
                    }
                    Frame::Ignored => {}
                },
                Event::Closed(close) => {
                    debug!(?close, "Streamlabs said goodbye");
                    return Ok(Ended::Closed);
                }
                Event::Renew => unreachable!("Streamlabs sessions do not schedule renewal"),
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

#[cfg(test)]
mod tests {
    use super::*;

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
