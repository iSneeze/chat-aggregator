use std::future::Future;
use tokio::sync::mpsc;

pub mod demo;
pub mod hub;

pub use hub::Hub;

pub trait ChatSource {
    /// Connects to the platform and pushes normalized events until
    /// the stream ends or an unrecoverable error occurs.
    ///
    /// Takes `self` by value: a source is spawned once and consumed by its
    /// task. `Ok(())` means the stream ended normally (e.g. the broadcast is
    /// over), `Err` means it failed.
    fn run(self, tx: mpsc::Sender<ChatEvent>) -> impl Future<Output = anyhow::Result<()>> + Send;
}

// JSON format: every type below is part of the public WebSocket API
// (docs/api.md). serde attributes decide the exact JSON; renaming a field or
// variant here is a breaking change for API consumers, and the contract test
// at the bottom of this file will say so. With the optional `schema` feature,
// the types also derive a JSON Schema (published in docs/schema/).

/// Everything a source can report. Moderation actions are separate
/// variants rather than message kinds: they have no author or text, and
/// consumers must remove already-displayed messages when they arrive.
// Clippy suggests boxing the large `Message` variant so the rare small
// variants don't pay its size. But messages are the common case: boxing
// would add a heap allocation per message to save memory on deletes.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
// "Internally tagged": the variant name becomes a `"type"` field inside the
// object, e.g. {"type":"delete", ...}, instead of serde's default
// {"Delete": {...}} wrapper, which is awkward to handle in most languages.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// A chat message or a platform event (donation, sub, raid, ...).
    Message(ChatMessage),
    /// A single message was deleted by a moderator.
    Delete {
        platform: ChatPlatform,
        /// The `id` of the deleted message.
        message_id: String,
    },
    /// All messages by one user should go (ban or timeout).
    ClearUser {
        platform: ChatPlatform,
        /// The `author.id` whose messages should be removed.
        user_id: String,
    },
    /// The whole chat of this platform was cleared.
    ClearAll { platform: ChatPlatform },
}

#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatMessage {
    /// Platform-assigned message id; delete events refer to it.
    pub id: String,
    pub platform: ChatPlatform,
    pub author: Author,
    /// What the user typed; may be empty (e.g. a raid or a resub without a
    /// message). Descriptions of the event itself live in `kind`.
    pub text: String,
    /// Emotes used in `text`, each listed once. Occurrences in `text` are
    /// written exactly as `code`.
    pub emotes: Vec<EmoteRef>,
    /// When the message was sent (RFC 3339, UTC).
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub kind: MessageKind,
}

/// Emotes are stored with each message because their images can differ per
/// channel or change over time: this keeps code and image in sync.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EmoteRef {
    /// The emote as it appears in the text, e.g. `Kappa` or `:_hype:`.
    pub code: String,
    /// Image URL.
    pub url: String,
}

// `Copy`: a fieldless enum is just a small tag, so passing it by value is
// as cheap as passing a reference. `PartialEq`/`Eq` let us compare platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ChatPlatform {
    Twitch,
    YouTube,
    Rplay,
}

impl ChatPlatform {
    /// Lowercase name, as used in HTML classes and event payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            ChatPlatform::Twitch => "twitch",
            ChatPlatform::YouTube => "youtube",
            ChatPlatform::Rplay => "rplay",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Author {
    /// Platform-specific user id (Twitch user id, YouTube channel id).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Name color chosen by the user, as `#rrggbb` (Twitch only).
    pub color: Option<String>,
    /// Badge names, e.g. `moderator`, `subscriber`, `member`, `vip`.
    pub badges: Vec<String>,
    /// Profile picture URL (YouTube only).
    pub avatar_url: Option<String>,
}

/// What kind of message this is. The names match the `kind` values of the
/// overlay template (with `_` instead of `-`).
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageKind {
    /// Normal chat.
    Text,
    /// Only emotes and whitespace; which ones is in `emotes`.
    EmoteOnly,
    /// Paid message: Twitch bits, YouTube Super Chat.
    Donation {
        /// As displayed by the platform: "€5.00", "100 bits".
        amount: String,
    },
    /// Paid visual items: YouTube Super Stickers and gifts, later maybe
    /// Twitch giant emotes.
    Special {
        /// Image, if the platform provides one (Super Stickers: no).
        image_url: Option<String>,
        /// "€2.00", "10 jewels"
        amount: Option<String>,
        /// Name or description of the item, e.g. the sticker's alt text.
        info: Option<String>,
    },
    /// New subscriber/member, resub, membership milestone.
    #[serde(rename = "membership")]
    MembershipJoin {
        /// The platform's description, e.g. "subscribed for 12 months".
        info: String,
    },
    /// Gifted subs (Twitch) or memberships (YouTube).
    #[serde(rename = "gift")]
    MembershipGift {
        /// Number of gifted subs/memberships.
        count: usize,
    },
    /// Raids, announcements, etc.
    #[serde(rename = "notice")]
    SystemNotice {
        /// The platform's description.
        info: String,
    },
}

pub struct MockSource {
    pub count: usize,
    pub delay: std::time::Duration,
}

impl ChatSource for MockSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>) -> anyhow::Result<()> {
        for i in 0..self.count {
            tokio::time::sleep(self.delay).await;

            let msg = ChatMessage {
                id: format!("mock-{i}"),
                platform: ChatPlatform::Twitch,
                author: Author {
                    id: format!("mock-user-{i}"),
                    name: format!("MockUser{i}"),
                    color: Some("#7f5af0".to_string()),
                    badges: vec![],
                    avatar_url: None,
                },
                text: format!("mock message number {i}"),
                emotes: vec![],
                timestamp: chrono::Utc::now(),
                kind: MessageKind::Text,
            };
            // If this errors, the receiver was dropped: nobody is
            // listening anymore, so we shut down gracefully.
            tx.send(ChatEvent::Message(msg)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn mock_source_sends_messages_then_ends() {
        let (tx, mut rx) = mpsc::channel(16);

        // Spawn the source as a task, exactly like the server will.
        tokio::spawn(
            MockSource {
                count: 5,
                delay: Duration::from_millis(1),
            }
            .run(tx),
        );

        let mut received = Vec::new();
        while let Some(event) = rx.recv().await {
            let ChatEvent::Message(msg) = event else {
                panic!("mock source only sends messages, got {event:?}");
            };
            received.push(msg);
        }

        assert_eq!(received.len(), 5);
        assert_eq!(received[0].text, "mock message number 0");
        assert_eq!(received[0].author.name, "MockUser0");
        assert!(matches!(received[2].kind, MessageKind::Text));
    }

    #[tokio::test]
    async fn source_shuts_down_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel(16);

        // Drop the receiver immediately: sends must fail, not hang.
        drop(rx);

        let result = MockSource {
            count: 3,
            delay: Duration::from_millis(1),
        }
        .run(tx)
        .await;

        assert!(result.is_err());
    }
}

/// The JSON contract of the public API (docs/api.md). If one of these fails
/// after a change, that change breaks API consumers: either revert it, or
/// update the docs and treat it as a new API version.
#[cfg(test)]
mod json_contract {
    use super::*;
    use serde_json::json;

    fn to_json(event: &ChatEvent) -> serde_json::Value {
        serde_json::to_value(event).unwrap()
    }

    fn message(kind: MessageKind) -> ChatEvent {
        ChatEvent::Message(ChatMessage {
            id: "m1".into(),
            platform: ChatPlatform::YouTube,
            author: Author {
                id: "UC1".into(),
                name: "Ann".into(),
                color: None,
                badges: vec!["member".into()],
                avatar_url: Some("https://img/ann".into()),
            },
            text: "hi :yt:".into(),
            emotes: vec![EmoteRef {
                code: ":yt:".into(),
                url: "https://img/yt".into(),
            }],
            timestamp: "2026-09-25T18:40:44.5Z".parse().unwrap(),
            kind,
        })
    }

    #[test]
    fn message_event() {
        assert_eq!(
            to_json(&message(MessageKind::Text)),
            json!({
                "type": "message",
                "id": "m1",
                "platform": "youtube",
                "author": {
                    "id": "UC1",
                    "name": "Ann",
                    "color": null,
                    "badges": ["member"],
                    "avatar_url": "https://img/ann"
                },
                "text": "hi :yt:",
                "emotes": [{ "code": ":yt:", "url": "https://img/yt" }],
                "timestamp": "2026-09-25T18:40:44.500Z",
                "kind": { "type": "text" }
            })
        );
    }

    #[test]
    fn message_kinds() {
        let kind = |k| to_json(&message(k))["kind"].clone();

        assert_eq!(kind(MessageKind::Text), json!({ "type": "text" }));
        assert_eq!(
            kind(MessageKind::EmoteOnly),
            json!({ "type": "emote_only" })
        );
        assert_eq!(
            kind(MessageKind::Donation {
                amount: "€5.00".into()
            }),
            json!({ "type": "donation", "amount": "€5.00" })
        );
        assert_eq!(
            kind(MessageKind::Special {
                image_url: None,
                amount: Some("€2.00".into()),
                info: Some("cat".into()),
            }),
            json!({ "type": "special", "image_url": null, "amount": "€2.00", "info": "cat" })
        );
        assert_eq!(
            kind(MessageKind::MembershipJoin {
                info: "12 months".into()
            }),
            json!({ "type": "membership", "info": "12 months" })
        );
        assert_eq!(
            kind(MessageKind::MembershipGift { count: 5 }),
            json!({ "type": "gift", "count": 5 })
        );
        assert_eq!(
            kind(MessageKind::SystemNotice {
                info: "raid".into()
            }),
            json!({ "type": "notice", "info": "raid" })
        );
    }

    #[test]
    fn moderation_events() {
        assert_eq!(
            to_json(&ChatEvent::Delete {
                platform: ChatPlatform::Twitch,
                message_id: "m1".into(),
            }),
            json!({ "type": "delete", "platform": "twitch", "message_id": "m1" })
        );
        assert_eq!(
            to_json(&ChatEvent::ClearUser {
                platform: ChatPlatform::YouTube,
                user_id: "UC1".into(),
            }),
            json!({ "type": "clear_user", "platform": "youtube", "user_id": "UC1" })
        );
        assert_eq!(
            to_json(&ChatEvent::ClearAll {
                platform: ChatPlatform::Rplay,
            }),
            json!({ "type": "clear_all", "platform": "rplay" })
        );
    }
}
