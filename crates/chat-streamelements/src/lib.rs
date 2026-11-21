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

use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::tiers::TipSettings;
use chat_core::{Activity, ChatEvent, ChatSource, Reporter, SetupError};
use chat_websocket::tungstenite::Message;
use chat_websocket::{Ended, Event, Keepalive, Session};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info};

use frames::Frame;

const ASTRO: &str = "wss://astro.streamelements.com";
/// Astro pings every 30 s, so this much silence means the connection is
/// dead without either side having noticed (a sleeping laptop, a dropped
/// route).
const SILENT_FOR: Duration = Duration::from_secs(75);

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

impl ChatSource for StreamElementsSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        chat_websocket::keep_connected("StreamElements", &activity, || self.read(&tx, &activity))
            .await
    }
}

impl StreamElementsSource {
    /// One connection: from connecting until it ends.
    async fn read(
        &self,
        tx: &mpsc::Sender<ChatEvent>,
        activity: &Reporter,
    ) -> anyhow::Result<Ended> {
        let socket = chat_websocket::connect(ASTRO)
            .await
            .context("connecting to StreamElements")?;
        debug!("connected to StreamElements");

        let mut session = Session::new(
            "StreamElements",
            socket,
            Keepalive::server_pings(SILENT_FOR),
        );
        loop {
            match session.next().await? {
                Event::Text(text) => match frames::decode(&text) {
                    // Astro's own examples wait for the welcome before subscribing.
                    Frame::Welcome => self.subscribe(&mut session).await?,
                    Frame::Subscribed => {
                        info!("reading StreamElements tips");
                        activity.set(Activity::Receiving);
                    }
                    Frame::Refused(why) => return Err(token_refused(&why).into()),
                    Frame::Failed(why) => {
                        bail!("StreamElements refused the subscription: {why}")
                    }
                    Frame::Tip(tip) => {
                        // The tip settings as they are now. In a statement of
                        // its own: `borrow`'s read lock ends with it, and a
                        // lock held across the `.await` below would make the
                        // task unsendable between threads.
                        let message = tip.into_message(&self.tips.borrow());
                        if tx.send(ChatEvent::Message(message)).await.is_err() {
                            return Ok(Ended::ReceiverGone);
                        }
                    }
                    // Astro's reconnect token would resume the old session,
                    // but a fresh one is simpler and only a moment away. A
                    // tip in that moment is missed, as with any reconnect.
                    Frame::Reconnect => return Ok(Ended::Restarting),
                    Frame::Ignored => {}
                },
                Event::Closed(close) => {
                    debug!(?close, "StreamElements said goodbye");
                    return Ok(Ended::Closed);
                }
                Event::Renew => unreachable!("StreamElements sessions do not schedule renewal"),
            }
        }
    }

    async fn subscribe(&self, session: &mut Session) -> anyhow::Result<()> {
        let command = frames::subscribe(&self.account, &self.token);
        session
            .send(Message::text(command))
            .await
            .context("subscribing to StreamElements' activity feed")
    }
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
