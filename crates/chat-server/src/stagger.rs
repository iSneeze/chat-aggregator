//! Spreads out chat messages that arrive in a burst, so the overlay doesn't
//! show many at once.
//!
//! Bursts happen after each YouTube reconnect (messages from the gap arrive
//! together), when one response carries several messages, and in hype
//! moments on Twitch. Staggering applies to the overlay only: the JSON API
//! and the history replay are never delayed.
//!
//! Rules:
//! - A message arriving while nothing is queued shows immediately.
//! - Queued messages are released `gap` apart, closer together in a big
//!   burst, so none waits longer than `max_delay` after it arrived.
//! - Moderation events are never delayed, and they remove matching messages
//!   that are still waiting, so a deleted message never shows up.

use std::collections::VecDeque;
use std::time::Duration;

use chat_core::{ChatEvent, ChatMessage};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How messages in a burst are spaced out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stagger {
    gap: Duration,
    max_delay: Duration,
}

impl Stagger {
    /// Upper limit for `max_delay`: live chat must stay live.
    pub const MAX_DELAY_LIMIT: Duration = Duration::from_secs(5);

    /// `gap` between messages of a burst, and the most a message may be
    /// delayed (capped at [`Self::MAX_DELAY_LIMIT`]). A zero value turns
    /// staggering off.
    pub fn new(gap: Duration, max_delay: Duration) -> Self {
        Self {
            gap,
            max_delay: max_delay.min(Self::MAX_DELAY_LIMIT),
        }
    }

    /// No staggering: messages show as soon as they arrive.
    pub fn off() -> Self {
        Self::new(Duration::ZERO, Duration::ZERO)
    }

    pub fn is_off(&self) -> bool {
        self.gap.is_zero() || self.max_delay.is_zero()
    }
}

impl Default for Stagger {
    fn default() -> Self {
        Self::new(Duration::from_millis(250), Duration::from_secs(2))
    }
}

/// Paces `events` into `out` until either side ends. Runs as its own task
/// per overlay connection.
pub(crate) async fn run(
    events: impl Stream<Item = ChatEvent>,
    out: mpsc::Sender<ChatEvent>,
    stagger: Stagger,
) {
    let mut events = std::pin::pin!(events);
    let mut queue: VecDeque<(ChatMessage, Instant)> = VecDeque::new();
    let mut last_sent: Option<Instant> = None;

    loop {
        let release_at = next_release(&queue, last_sent, stagger);
        tokio::select! {
            // `biased`: check the branches in this order instead of picking
            // a random ready one (tokio's default, for fairness). A message
            // that is due goes out before more input is read, so the output
            // order is the same on every run.
            biased;
            // Only armed while something is queued (`release_at` is Some).
            () = sleep_until(release_at), if release_at.is_some() => {
                let (msg, _) = queue.pop_front().expect("armed only with a queued message");
                if out.send(ChatEvent::Message(msg)).await.is_err() {
                    return;
                }
                last_sent = Some(Instant::now());
            }
            event = events.next() => match event {
                Some(ChatEvent::Message(msg)) => queue.push_back((msg, Instant::now())),
                Some(moderation) => {
                    queue.retain(|(msg, _)| !removes(&moderation, msg));
                    if out.send(moderation).await.is_err() {
                        return;
                    }
                }
                None => return, // hub closed or server shutting down
            },
            // The overlay disconnected: stop now rather than at the next event.
            () = out.closed() => return,
        }
    }
}

/// When the first queued message may be shown, or `None` if nothing waits.
fn next_release(
    queue: &VecDeque<(ChatMessage, Instant)>,
    last_sent: Option<Instant>,
    stagger: Stagger,
) -> Option<Instant> {
    let (_, front_arrived) = queue.front()?;
    let Some(last_sent) = last_sent else {
        return Some(*front_arrived); // nothing shown yet: right away
    };
    let now = Instant::now();
    // Normal spacing after the previous message.
    let spaced = last_sent + stagger.gap;
    // In a big burst, spread the time left until the newest message's
    // deadline evenly over everything still queued.
    let (_, back_arrived) = queue.back().expect("front exists");
    let back_deadline = *back_arrived + stagger.max_delay;
    let even = now + back_deadline.saturating_duration_since(now) / queue.len() as u32;
    // And never later than the first message's own deadline.
    let front_deadline = *front_arrived + stagger.max_delay;
    Some(spaced.min(even).min(front_deadline))
}

/// Sleeps until `at`; `None` never fires (that select branch is disabled).
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Whether a moderation event removes `msg`.
fn removes(event: &ChatEvent, msg: &ChatMessage) -> bool {
    match event {
        ChatEvent::Message(_) => false,
        ChatEvent::Delete {
            platform,
            message_id,
        } => msg.platform == *platform && msg.id == *message_id,
        ChatEvent::ClearUser { platform, user_id } => {
            msg.platform == *platform && msg.author.id == *user_id
        }
        ChatEvent::ClearAll { platform } => msg.platform == *platform,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::{Author, ChatPlatform, MessageKind};

    fn message(id: &str) -> ChatEvent {
        ChatEvent::Message(ChatMessage {
            id: id.into(),
            platform: ChatPlatform::YouTube,
            author: Author {
                id: format!("author-{id}"),
                name: "Ann".into(),
                color: None,
                badges: vec![],
                avatar_url: None,
            },
            text: String::new(),
            emotes: vec![],
            timestamp: chrono::Utc::now(),
            kind: MessageKind::Text,
        })
    }

    /// Feeds events through the pacer and records when each one comes out,
    /// in milliseconds after the start. With tokio's paused clock the times
    /// are exact: the runtime jumps straight to the next timer instead of
    /// actually waiting.
    struct Harness {
        input: mpsc::Sender<ChatEvent>,
        output: mpsc::Receiver<ChatEvent>,
        start: Instant,
    }

    impl Harness {
        fn new(stagger: Stagger) -> Self {
            let (input, input_rx) = mpsc::channel(256);
            let (output_tx, output) = mpsc::channel(256);
            let events = tokio_stream::wrappers::ReceiverStream::new(input_rx);
            tokio::spawn(run(events, output_tx, stagger));
            Self {
                input,
                output,
                start: Instant::now(),
            }
        }

        async fn send(&self, event: ChatEvent) {
            self.input.send(event).await.unwrap();
        }

        /// Next event out, as (id or kind, ms since start).
        async fn next(&mut self) -> (String, u128) {
            let event = self.output.recv().await.unwrap();
            let label = match event {
                ChatEvent::Message(m) => m.id,
                other => format!("{other:?}")
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_string(),
            };
            (label, self.start.elapsed().as_millis())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn single_message_is_not_delayed() {
        let mut h = Harness::new(Stagger::default());
        h.send(message("a")).await;
        assert_eq!(h.next().await, ("a".into(), 0));
    }

    #[tokio::test(start_paused = true)]
    async fn small_burst_is_spaced_by_the_gap() {
        let mut h = Harness::new(Stagger::default());
        for id in ["a", "b", "c"] {
            h.send(message(id)).await;
        }
        assert_eq!(h.next().await, ("a".into(), 0));
        assert_eq!(h.next().await, ("b".into(), 250));
        assert_eq!(h.next().await, ("c".into(), 500));
    }

    #[tokio::test(start_paused = true)]
    async fn big_burst_fits_within_max_delay() {
        let mut h = Harness::new(Stagger::default());
        for i in 0..40 {
            h.send(message(&i.to_string())).await;
        }
        let mut last = 0;
        for i in 0..40 {
            let (id, at) = h.next().await;
            assert_eq!(id, i.to_string(), "order is kept");
            assert!(at >= last, "never out of order in time");
            last = at;
        }
        assert!(last <= 2000, "last message after {last} ms, cap is 2000");
        assert!(
            last > 1000,
            "but still spread out, not dumped at once ({last} ms)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn messages_after_a_pause_show_immediately() {
        let mut h = Harness::new(Stagger::default());
        h.send(message("a")).await;
        assert_eq!(h.next().await, ("a".into(), 0));
        tokio::time::sleep(Duration::from_secs(1)).await;
        h.send(message("b")).await;
        assert_eq!(h.next().await, ("b".into(), 1000));
    }

    #[tokio::test(start_paused = true)]
    async fn delete_is_immediate_and_drops_the_queued_message() {
        let mut h = Harness::new(Stagger::default());
        for id in ["a", "b", "c"] {
            h.send(message(id)).await;
        }
        h.send(ChatEvent::Delete {
            platform: ChatPlatform::YouTube,
            message_id: "b".into(),
        })
        .await;
        assert_eq!(h.next().await, ("a".into(), 0));
        assert_eq!(
            h.next().await,
            ("Delete".into(), 0),
            "moderation isn't delayed"
        );
        assert_eq!(h.next().await, ("c".into(), 250), "b never shows");
    }

    #[tokio::test(start_paused = true)]
    async fn clear_user_drops_their_queued_messages() {
        let mut h = Harness::new(Stagger::default());
        for id in ["a", "b"] {
            h.send(message(id)).await;
        }
        h.send(ChatEvent::ClearUser {
            platform: ChatPlatform::YouTube,
            user_id: "author-b".into(),
        })
        .await;
        h.send(message("c")).await;
        assert_eq!(h.next().await, ("a".into(), 0));
        assert_eq!(h.next().await, ("ClearUser".into(), 0));
        assert_eq!(h.next().await, ("c".into(), 250));
    }

    #[test]
    fn max_delay_is_capped_and_zero_means_off() {
        let s = Stagger::new(Duration::from_millis(250), Duration::from_secs(60));
        assert_eq!(s.max_delay, Stagger::MAX_DELAY_LIMIT);
        assert!(Stagger::off().is_off());
        assert!(!Stagger::default().is_off());
    }
}
