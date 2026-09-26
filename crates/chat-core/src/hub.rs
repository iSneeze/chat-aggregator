//! Fan-out of chat events to any number of consumers (overlays, the JSON
//! API, the UI), plus a short replay history so a consumer that connects
//! late (e.g. OBS reloading the overlay) doesn't start from a blank chat.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::broadcast;

use crate::{ChatEvent, ChatMessage};

/// How many live events a slow subscriber may fall behind before it starts
/// missing some (it then gets `RecvError::Lagged` instead of stalling
/// everyone else).
const LIVE_BUFFER: usize = 256;

pub struct Hub {
    tx: broadcast::Sender<ChatEvent>,
    // A std (not tokio) Mutex: it's only held for a few non-async
    // operations, never across an `.await`, which is exactly the case the
    // tokio docs recommend the std one for.
    history: Mutex<History>,
}

impl Hub {
    /// `history`: how many recent messages new subscribers get replayed
    /// (0 disables replay).
    pub fn new(history: usize) -> Self {
        let (tx, _) = broadcast::channel(LIVE_BUFFER);
        Self {
            tx,
            history: Mutex::new(History {
                messages: VecDeque::with_capacity(history),
                cap: history,
            }),
        }
    }

    pub fn publish(&self, event: ChatEvent) {
        let mut history = self.lock_history();
        history.apply(&event);
        // Sent while still holding the lock, see `subscribe`. An error only
        // means nobody is subscribed right now, which is fine.
        let _ = self.tx.send(event);
    }

    /// Returns the replay history (oldest first) and a receiver for
    /// everything published afterwards.
    ///
    /// Both happen under the same lock `publish` holds while it records and
    /// sends. So every event is either already in the snapshot or will
    /// arrive on the receiver: never both (duplicate) and never neither (gap).
    pub fn subscribe(&self) -> (Vec<ChatMessage>, broadcast::Receiver<ChatEvent>) {
        let history = self.lock_history();
        let rx = self.tx.subscribe();
        (history.messages.iter().cloned().collect(), rx)
    }

    fn lock_history(&self) -> MutexGuard<'_, History> {
        // A Mutex is "poisoned" if a thread panicked while holding it. The
        // history is a plain list that is always valid between operations,
        // so keep using it rather than spreading the panic to every caller.
        self.history.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct History {
    messages: VecDeque<ChatMessage>,
    cap: usize,
}

impl History {
    /// Mirrors what an overlay does with each event, so a replay shows the
    /// same state a long-connected overlay would: deleted messages stay gone.
    fn apply(&mut self, event: &ChatEvent) {
        match event {
            ChatEvent::Message(msg) => {
                if self.cap == 0 {
                    return;
                }
                if self.messages.len() == self.cap {
                    self.messages.pop_front();
                }
                self.messages.push_back(msg.clone());
            }
            ChatEvent::Delete {
                platform,
                message_id,
            } => self
                .messages
                .retain(|m| !(m.platform == *platform && m.id == *message_id)),
            ChatEvent::ClearUser { platform, user_id } => self
                .messages
                .retain(|m| !(m.platform == *platform && m.author.id == *user_id)),
            ChatEvent::ClearAll { platform } => self.messages.retain(|m| m.platform != *platform),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Author, ChatPlatform, MessageKind};

    fn msg(platform: ChatPlatform, id: &str, author: &str) -> ChatEvent {
        ChatEvent::Message(ChatMessage {
            id: id.into(),
            platform,
            author: Author {
                id: author.into(),
                name: author.into(),
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

    fn history_ids(hub: &Hub) -> Vec<String> {
        hub.subscribe().0.into_iter().map(|m| m.id).collect()
    }

    #[test]
    fn history_keeps_only_the_newest() {
        let hub = Hub::new(2);
        for id in ["a", "b", "c"] {
            hub.publish(msg(ChatPlatform::Twitch, id, "u"));
        }
        assert_eq!(history_ids(&hub), ["b", "c"]);
    }

    #[test]
    fn zero_disables_history() {
        let hub = Hub::new(0);
        hub.publish(msg(ChatPlatform::Twitch, "a", "u"));
        assert!(history_ids(&hub).is_empty());
    }

    #[test]
    fn moderation_applies_to_history_per_platform() {
        let hub = Hub::new(10);
        hub.publish(msg(ChatPlatform::Twitch, "1", "troll"));
        hub.publish(msg(ChatPlatform::YouTube, "2", "troll")); // same id, other platform
        hub.publish(msg(ChatPlatform::Twitch, "3", "nice"));
        hub.publish(msg(ChatPlatform::Twitch, "4", "nice"));

        hub.publish(ChatEvent::ClearUser {
            platform: ChatPlatform::Twitch,
            user_id: "troll".into(),
        });
        assert_eq!(history_ids(&hub), ["2", "3", "4"]);

        hub.publish(ChatEvent::Delete {
            platform: ChatPlatform::Twitch,
            message_id: "3".into(),
        });
        assert_eq!(history_ids(&hub), ["2", "4"]);

        hub.publish(ChatEvent::ClearAll {
            platform: ChatPlatform::Twitch,
        });
        assert_eq!(history_ids(&hub), ["2"]);
    }

    #[test]
    fn subscriber_gets_history_then_only_new_events() {
        let hub = Hub::new(10);
        hub.publish(msg(ChatPlatform::Twitch, "old", "u"));

        let (history, mut rx) = hub.subscribe();
        hub.publish(msg(ChatPlatform::Twitch, "new", "u"));

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "old");
        match rx.try_recv() {
            Ok(ChatEvent::Message(m)) => assert_eq!(m.id, "new"),
            other => panic!("expected the new message, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "nothing else, in particular not 'old'"
        );
    }

    #[test]
    fn publish_without_subscribers_is_fine() {
        let hub = Hub::new(1);
        hub.publish(msg(ChatPlatform::Twitch, "a", "u")); // must not panic
    }
}
