//! Picarto chat source: reads any channel's chat anonymously, through the
//! same WebSocket the website uses for logged-out visitors. What's known
//! about it is in PICARTO.md, the maintainer's research notes (kept
//! outside the public repository); `examples/raw_picarto.rs` captures raw
//! chat traffic.
//!
//! Shows chat messages with their emotes, and deletions and cleared chats.
//! Tips, subscriptions and raids aren't handled yet: none has been seen in
//! a capture so far.

mod connection;
mod emotes;
mod frames;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::{Activity, ChatEvent, ChatSource, Reporter};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

pub use connection::ChannelNotFound;
pub use emotes::{Body, ChannelEmote, SiteEmotes};

use frames::{Context as FrameContext, Frame};

/// What the website sends to keep the chat connection alive, and how often.
const PING: &str = r#"{"type":"ping","message":"__ping__"}"#;
const PING_EVERY: Duration = Duration::from_secs(30);
/// Every ping gets an answer, so this much silence means the connection is
/// dead without either side having noticed (a sleeping laptop, a dropped
/// route).
const SILENT_FOR: Duration = Duration::from_secs(75);
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// A connection that lasted this long worked: the wait starts over.
const STABLE_AFTER: Duration = Duration::from_secs(60);

pub struct PicartoSource {
    /// A name that passed [`normalize_channel`].
    pub channel: String,
}

/// The channel name from what a user typed or pasted: the name itself, or a
/// link as copied from the address bar, either the stream page
/// (`picarto.tv/<name>`) or the pop-out chat
/// (`picarto.tv/chatpopout/<name>/public`). An invalid name can never
/// work, so it's caught here instead of being retried like a network
/// problem.
pub fn normalize_channel(input: &str) -> Result<String, String> {
    let input = input.trim();
    let name = if input.contains("picarto.tv") {
        let link = if input.contains("://") {
            input.to_owned()
        } else {
            format!("https://{input}")
        };
        let url = reqwest::Url::parse(&link).map_err(|_| format!("not a link: {input:?}"))?;
        let segments: Vec<&str> = url
            .path_segments()
            .map(|segments| segments.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        match segments.as_slice() {
            ["chatpopout", name, ..] | [name, ..] => name.to_string(),
            [] => return Err(format!("the link has no channel name in it: {input:?}")),
        }
    } else {
        input.to_owned()
    };
    // Also keeps the name safe to put into a URL path as it is.
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(name)
    } else {
        Err(format!(
            "expected a Picarto channel name or a picarto.tv link, got {input:?}"
        ))
    }
}

/// Why a connection ended without an error.
enum Ended {
    /// Picarto closed it; connect again.
    Closed,
    /// Nobody listens to our events anymore: stop.
    ReceiverGone,
}

impl ChatSource for PicartoSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        let http = connection::http_client()?;
        let tls = connection::tls_config()?;
        // Once per source, not per connection: the script is 1.4 MB.
        // Without it only channel emotes become images; chat still works.
        let emotes = match SiteEmotes::fetch(&http).await {
            Ok(emotes) => emotes,
            Err(e) => {
                warn!("Picarto's emotes and emoji names stay text: {e:#}");
                SiteEmotes::default()
            }
        };

        // Reconnects by itself instead of returning an error for the engine
        // to restart: a restart builds a new source, which would download
        // the emote tables again.
        let mut delay = RETRY_FIRST;
        loop {
            let started = Instant::now();
            let problem = match self.read(&http, &tls, &emotes, &tx, &activity).await {
                Ok(Ended::ReceiverGone) => return Ok(()),
                Ok(Ended::Closed) => {
                    info!(channel = %self.channel, "Picarto closed the chat connection");
                    "connection closed by Picarto".to_owned()
                }
                // A wrong name stays wrong: the engine shows it to the user.
                Err(e) if e.is::<ChannelNotFound>() => return Err(e),
                Err(e) => {
                    warn!(channel = %self.channel, "Picarto chat: {e:#}");
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

impl PicartoSource {
    /// One connection: from the channel lookup until it ends.
    async fn read(
        &self,
        http: &reqwest::Client,
        tls: &Arc<rustls::ClientConfig>,
        emotes: &SiteEmotes,
        tx: &mpsc::Sender<ChatEvent>,
        activity: &Reporter,
    ) -> anyhow::Result<Ended> {
        let channel = connection::lookup(http, &self.channel).await?;
        let mut socket = connection::open(http, tls, &channel.name).await?;
        info!(channel = %channel.name, "connected to Picarto chat");
        activity.set(live_state(channel.online));

        let cx = FrameContext {
            owner_id: &channel.id,
            emotes,
        };
        let mut pings = tokio::time::interval(PING_EVERY);
        // An interval's first tick is immediate; the first ping is due later.
        pings.tick().await;
        let mut heard = Instant::now();
        loop {
            // Waits for whichever comes first. The losing branches' futures
            // are dropped before the winner's code runs, so the ping branch
            // may use `socket` although `socket.next()` borrowed it.
            tokio::select! {
                frame = socket.next() => {
                    let Some(frame) = frame else {
                        return Ok(Ended::Closed);
                    };
                    heard = Instant::now();
                    match frame.context("reading from Picarto's chat")? {
                        Message::Text(text) => match frames::decode(&text, &cx) {
                            Frame::Events(events) => {
                                for event in events {
                                    if tx.send(event).await.is_err() {
                                        return Ok(Ended::ReceiverGone);
                                    }
                                }
                            }
                            Frame::Stream { online } => activity.set(live_state(online)),
                            Frame::Ignored => {}
                        },
                        Message::Close(close) => {
                            debug!(?close, "Picarto's chat server said goodbye");
                            return Ok(Ended::Closed);
                        }
                        // WebSocket-level keep-alive: tungstenite answers
                        // pings by itself.
                        _ => {}
                    }
                }
                _ = pings.tick() => {
                    socket
                        .send(Message::text(PING))
                        .await
                        .context("sending a ping to Picarto's chat")?;
                }
                // A deadline, not a timeout around `next()`: the ping branch
                // wins every 30 s, which would restart a timeout each time.
                () = tokio::time::sleep_until(heard + SILENT_FOR) => {
                    bail!("no answer from Picarto's chat for {} s", SILENT_FOR.as_secs());
                }
            }
        }
    }
}

/// The channel's chat can be read whether it's live or not; offline just
/// means it's quieter, so it's not a problem to show.
fn live_state(online: bool) -> Activity {
    if online {
        Activity::Receiving
    } else {
        Activity::Idle("the channel is offline (its chat is still read)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names_and_links() {
        for input in [
            "your_channel",
            " your_channel ",
            "picarto.tv/your_channel",
            "https://picarto.tv/your_channel",
            "https://www.picarto.tv/your_channel?tab=chat",
            "https://picarto.tv/chatpopout/your_channel/public",
        ] {
            assert_eq!(
                normalize_channel(input).as_deref(),
                Ok("your_channel"),
                "{input}"
            );
        }
        assert_eq!(
            normalize_channel("Your-Channel").as_deref(),
            Ok("Your-Channel")
        );
    }

    #[test]
    fn invalid_names_are_rejected() {
        for input in ["", "has space", "../etc", "https://picarto.tv/", "name?x=1"] {
            assert!(normalize_channel(input).is_err(), "{input}");
        }
    }
}
