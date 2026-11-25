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

use std::time::Duration;

use chat_core::{Activity, ChatEvent, ChatSource, Reporter};
use chat_websocket::tungstenite::Message;
use chat_websocket::{Ended, Event, Keepalive, Session};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span, warn};

pub use emotes::{Body, ChannelEmote, SiteEmotes};

use frames::{Context as FrameContext, Frame};

/// What the website sends to keep the chat connection alive, and how often.
const PING: &str = r#"{"type":"ping","message":"__ping__"}"#;
const PING_EVERY: Duration = Duration::from_secs(30);
/// Every ping gets an answer, so this much silence means the connection is
/// dead without either side having noticed (a sleeping laptop, a dropped
/// route).
const SILENT_FOR: Duration = Duration::from_secs(75);

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

impl ChatSource for PicartoSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        let http = connection::http_client()?;
        // Once per source, not per connection: the script is 1.4 MB.
        // Without it only channel emotes become images; chat still works.
        let emotes = match SiteEmotes::fetch(&http).await {
            Ok(emotes) => emotes,
            Err(e) => {
                warn!("Picarto's emotes and emoji names stay text: {e:#}");
                SiteEmotes::default()
            }
        };

        // The channel in every log line from here on (as
        // `picarto{channel=…}`): with several channels, the log says whose.
        let span = info_span!("picarto", channel = %self.channel);
        // Reconnects by itself instead of returning an error for the engine
        // to restart: a restart builds a new source, which would download
        // the emote tables again.
        chat_websocket::keep_connected("Picarto", &activity, || {
            self.read(&http, &emotes, &tx, &activity)
        })
        .instrument(span)
        .await
    }
}

impl PicartoSource {
    /// One connection: from the channel lookup until it ends.
    async fn read(
        &self,
        http: &reqwest::Client,
        emotes: &SiteEmotes,
        tx: &mpsc::Sender<ChatEvent>,
        activity: &Reporter,
    ) -> anyhow::Result<Ended> {
        let channel = connection::lookup(http, &self.channel).await?;
        let socket = connection::open(http, &channel.name).await?;
        info!("connected to Picarto chat");
        activity.set(live_state(channel.online));

        let cx = FrameContext {
            owner_id: &channel.id,
            emotes,
        };
        let mut session = Session::new(
            "Picarto's chat",
            socket,
            Keepalive::with_ping(PING_EVERY, Message::text(PING), SILENT_FOR),
        );
        loop {
            match session.next().await? {
                Event::Text(text) => match frames::decode(&text, &cx) {
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
                Event::Closed(close) => {
                    debug!(?close, "Picarto's chat server said goodbye");
                    return Ok(Ended::Closed);
                }
                Event::Renew => unreachable!("Picarto sessions do not schedule renewal"),
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
    fn channel_names_and_links_validate() {
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
        for input in ["", "has space", "../etc", "https://picarto.tv/", "name?x=1"] {
            assert!(normalize_channel(input).is_err(), "{input}");
        }
    }
}
