use std::collections::{HashSet, VecDeque};
use std::future::Future;
use tokio::sync::mpsc;

pub mod activity;
pub mod demo;
pub mod fs;
pub mod hub;
pub mod secrets;
pub mod tiers;

pub use activity::{Activity, Reporter};
pub use hub::Hub;

pub trait ChatSource {
    /// Connects to the platform and pushes normalized events until
    /// the stream ends or an unrecoverable error occurs.
    ///
    /// Takes `self` by value: a source is spawned once and consumed by its
    /// task. `Ok(())` means the stream ended normally (e.g. the broadcast is
    /// over), `Err` means it failed.
    ///
    /// `activity` is for status displays: report what's happening (see
    /// [`Activity`]), especially anything that stops chat from arriving.
    fn run(
        self,
        tx: mpsc::Sender<ChatEvent>,
        activity: Reporter,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// A problem only the user can fix: a missing setting, an unreadable file,
/// a channel that doesn't exist, a refused token. A source failing with it
/// isn't restarted automatically, and a source's own reconnect loop gives
/// up on it too.
///
/// One type for every source, here in chat-core: the engine recognises it
/// by its type (`anyhow::Error::is`, a check at run time), so it has to be
/// the same type on both sides, in a crate both depend on.
#[derive(Debug)]
pub struct SetupError(pub String);

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupError {}

/// Whether `err` or any error in its `std::error::Error::source()` chain is of
/// type `T`. Unlike `err.is::<T>()` (which only checks the root error inside
/// `anyhow::Error`, not nested `source()` errors), this walks the full chain.
pub fn has_error<T: std::error::Error + Send + Sync + 'static>(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.is::<T>())
}

/// The first error of type `T` anywhere in `err`'s cause chain. Unlike
/// `err.downcast_ref::<T>()`, this walks nested `std::error::Error::source()`
/// causes too.
pub fn find_error<T: std::error::Error + Send + Sync + 'static>(err: &anyhow::Error) -> Option<&T> {
    err.chain().find_map(|cause| cause.downcast_ref::<T>())
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
    pub timestamp: jiff::Timestamp,
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
// as cheap as passing a reference. `PartialEq`/`Eq` let us compare platforms,
// `Hash` makes them usable as map keys (per-chatter cooldowns).
// `Deserialize`: settings name platforms too (a reaction's platform guard),
// with the same lowercase names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ChatPlatform {
    Twitch,
    YouTube,
    Rplay,
    Picarto,
    // Tips through StreamElements' tip page: not a chat, every message is a
    // donation. A plain comment, not a doc comment: with one, the JSON
    // Schema would describe this value apart from the others (`oneOf`)
    // instead of keeping one flat list of platforms.
    StreamElements,
    // Tips through Streamlabs' tip page: donations only, like
    // StreamElements.
    Streamlabs,
}

impl ChatPlatform {
    /// Every platform in canonical UI display order.
    pub const ALL: [ChatPlatform; 6] = [
        ChatPlatform::Twitch,
        ChatPlatform::YouTube,
        ChatPlatform::Picarto,
        ChatPlatform::Rplay,
        ChatPlatform::StreamElements,
        ChatPlatform::Streamlabs,
    ];

    /// Lowercase name, as used in HTML classes and event payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            ChatPlatform::Twitch => "twitch",
            ChatPlatform::YouTube => "youtube",
            ChatPlatform::Rplay => "rplay",
            ChatPlatform::Picarto => "picarto",
            ChatPlatform::StreamElements => "streamelements",
            ChatPlatform::Streamlabs => "streamlabs",
        }
    }

    /// Human-readable display name for UI labels and status lines.
    pub fn label(self) -> &'static str {
        match self {
            ChatPlatform::Twitch => "Twitch",
            ChatPlatform::YouTube => "YouTube",
            ChatPlatform::Rplay => "RPlay",
            ChatPlatform::Picarto => "Picarto",
            ChatPlatform::StreamElements => "StreamElements",
            ChatPlatform::Streamlabs => "Streamlabs",
        }
    }
}

/// A chatter's standing, across platforms. Ordered from least to most, so a
/// check like "at least moderator" is simply `role >= min_role`
/// (`PartialOrd`/`Ord` derived on an enum compare by declaration order).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Everyone,
    /// Twitch subscriber (or founder), YouTube member, Picarto subscriber.
    #[serde(alias = "member")]
    Subscriber,
    /// Twitch VIP.
    Vip,
    Moderator,
    /// The channel's owner.
    Broadcaster,
}

impl Role {
    /// The highest role among the author's badges. Platforms name them
    /// differently (Twitch `broadcaster`/`lead_moderator`, YouTube
    /// `owner`/`member`), so badges are mapped here in one canonical place.
    pub fn of(msg: &ChatMessage) -> Role {
        msg.author
            .badges
            .iter()
            .map(|badge| match badge.as_str() {
                "broadcaster" | "owner" => Role::Broadcaster,
                "moderator" | "lead_moderator" => Role::Moderator,
                "vip" => Role::Vip,
                "subscriber" | "founder" | "member" => Role::Subscriber,
                _ => Role::Everyone,
            })
            .max()
            .unwrap_or(Role::Everyone)
    }

    /// Lowercase name, matching serde's `snake_case` representation and
    /// overlay templates' `author.role` variable.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Everyone => "everyone",
            Role::Subscriber => "subscriber",
            Role::Vip => "vip",
            Role::Moderator => "moderator",
            Role::Broadcaster => "broadcaster",
        }
    }
}

/// Keeps recently seen message/event IDs in FIFO insertion order, bounded in
/// size so a 24/7 source never grows memory without bound.
pub struct BoundedIdSet {
    seen: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl BoundedIdSet {
    pub fn new(cap: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    /// Inserts `id` and returns `true` if it had not been seen recently.
    /// Avoids allocating a `String` when `id` is already present.
    pub fn insert(&mut self, id: impl AsRef<str> + Into<String>) -> bool {
        if self.seen.contains(id.as_ref()) {
            return false;
        }
        if self.cap == 0 {
            return true;
        }
        if self.order.len() == self.cap
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        let owned = id.into();
        self.seen.insert(owned.clone());
        self.order.push_back(owned);
        true
    }
}

/// Maximum length (in Unicode scalar values) of a user-assigned source label
/// (used by sources whose credentials don't carry a channel name, e.g. RPlay
/// and Streamlabs).
pub const MAX_SOURCE_LABEL_LEN: usize = 40;

/// Trims and validates a user-assigned source label. `kind` describes the
/// source in the empty-input hint (e.g. `"RPlay channel"`,
/// `"Streamlabs account"`).
pub fn normalize_source_label(input: &str, kind: &str) -> Result<String, String> {
    let label = input.trim();
    if label.is_empty() {
        return Err(format!("give this {kind} a name, like \"Main\""));
    }
    if label.chars().count() > MAX_SOURCE_LABEL_LEN {
        return Err(format!(
            "the name is longer than {MAX_SOURCE_LABEL_LEN} characters"
        ));
    }
    if label.chars().any(char::is_control) {
        return Err("the name can't contain line breaks or tabs".into());
    }
    Ok(label.to_owned())
}

#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Author {
    /// Platform-specific user id (Twitch user id, YouTube channel id,
    /// Picarto user id, RPlay user id).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Name color, as `#rrggbb`: chosen by the user (Twitch and Picarto),
    /// or the one RPlay gives them.
    pub color: Option<String>,
    /// Badge names, e.g. `moderator`, `subscriber`, `member`, `vip`.
    pub badges: Vec<String>,
    /// Profile picture URL (YouTube, Picarto, and RPlay for chatters who
    /// have one).
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
        /// See [`MessageKind::Special`]'s `tier`.
        tier: Option<u32>,
        /// See [`MessageKind::Special`]'s `value`.
        value: Option<f64>,
        /// See [`MessageKind::Special`]'s `currency`.
        currency: Option<String>,
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
        /// How big the amount is, from 1 (the smallest) to 7, on one scale
        /// for every platform (YouTube's Super Chat tiers, see
        /// `chat_core::tiers`); `null` if the platform has none:
        /// - YouTube Super Chats and Stickers: YouTube's tier (its colour in
        ///   YouTube's chat: 1 blue, 2 cyan, 3 teal, 4 yellow, 5 orange,
        ///   6 magenta, 7 red), whatever the currency. YouTube's top band
        ///   may report more than 7.
        /// - Twitch bits: YouTube's dollar steps × 100: 1 (below 200 bits),
        ///   2 (200+), 3 (500+), 4 (1,000+), 5 (2,000+), 6 (5,000+),
        ///   7 (10,000+).
        /// - RPlay coins: the bits' steps ÷ 5 (a coin buys about what 5 bits
        ///   do): 1 (below 40 coins), 2 (40+), 3 (100+), 4 (200+),
        ///   5 (400+), 6 (1,000+), 7 (2,000+).
        /// - StreamElements and Streamlabs tips: from the streamer's own
        ///   amounts (by default YouTube's steps, in the tip page's
        ///   currency). A Streamlabs tip in another currency: 1 (its worth
        ///   is unknown without a conversion).
        tier: Option<u32>,
        /// The amount as a number, in the platform's unit: bits for Twitch,
        /// coins for RPlay, jewels for YouTube gifts, otherwise money in
        /// `currency` (`5.0` for "€5.00"); `null` if unknown.
        value: Option<f64>,
        /// The ISO code of `value`'s currency ("EUR"): YouTube's is the
        /// viewer's, a StreamElements tip's the streamer's tip page's (if
        /// they set it), a Streamlabs tip's the one Streamlabs reports;
        /// `null` for bits, coins, jewels or an unknown currency.
        currency: Option<String>,
    },
    /// New subscriber/member, resub, membership milestone.
    #[serde(rename = "membership")]
    MembershipJoin {
        /// The platform's description, e.g. "subscribed for 12 months".
        info: String,
        /// How many months they've been a member, for a resub or milestone;
        /// `null` for a new subscriber/member.
        months: Option<u32>,
    },
    /// Gifted subs (Twitch) or memberships (YouTube).
    #[serde(rename = "gift")]
    MembershipGift {
        /// Number of gifted subs/memberships.
        count: usize,
    },
    /// Raids, announcements, watch streaks, etc.: platform events without a
    /// kind of their own.
    #[serde(rename = "notice")]
    SystemNotice {
        /// The platform's description.
        info: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The smallest possible source, to try the `ChatSource` contract on.
    /// Test-only: the real sources live in their own crates (and the demo
    /// one in `demo`).
    struct MockSource {
        count: usize,
        delay: Duration,
    }

    impl ChatSource for MockSource {
        async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
            activity.set(Activity::Receiving);
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
                    timestamp: jiff::Timestamp::now(),
                    kind: MessageKind::Text,
                };
                // If this errors, the receiver was dropped: nobody is
                // listening anymore, so we shut down gracefully.
                tx.send(ChatEvent::Message(msg)).await?;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn mock_source_sends_messages_then_ends() {
        let (tx, mut rx) = mpsc::channel(16);

        // Spawn the source as a task, exactly like the server will.
        tokio::spawn(
            MockSource {
                count: 5,
                delay: Duration::from_millis(1),
            }
            .run(tx, Reporter::detached()),
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
        .run(tx, Reporter::detached())
        .await;

        assert!(result.is_err());
    }

    #[test]
    fn error_chain_helpers_find_wrapped_errors() {
        #[derive(Debug)]
        struct Wrapper(SetupError);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("outer wrapper")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let wrapped = anyhow::Error::new(Wrapper(SetupError("bad token".into())));
        assert!(!wrapped.is::<SetupError>());
        assert!(has_error::<SetupError>(&wrapped));
        assert_eq!(find_error::<SetupError>(&wrapped).unwrap().0, "bad token");
    }

    #[test]
    fn bounded_id_set_evicts_oldest() {
        let mut set = BoundedIdSet::new(2);
        assert!(set.insert("a"));
        assert!(!set.insert("a"));
        assert!(set.insert("b"));
        assert!(set.insert("c")); // evicts "a"
        assert!(!set.insert("b"));
        assert!(set.insert("a")); // evicted, so new again
    }

    #[test]
    fn role_of_picks_highest_badge_across_platforms() {
        let mut msg = demo::sample_messages(0).remove(0);
        msg.author.badges = vec![];
        assert_eq!(Role::of(&msg), Role::Everyone);
        msg.author.badges = vec!["member".into()];
        assert_eq!(Role::of(&msg), Role::Subscriber);
        msg.author.badges = vec!["subscriber".into(), "vip".into()];
        assert_eq!(Role::of(&msg), Role::Vip);
        msg.author.badges = vec!["lead_moderator".into()];
        assert_eq!(Role::of(&msg), Role::Moderator);
        msg.author.badges = vec!["moderator".into(), "owner".into()];
        assert_eq!(Role::of(&msg), Role::Broadcaster);
    }

    #[test]
    fn source_label_validation() {
        assert_eq!(
            normalize_source_label("  Main \n", "RPlay channel").unwrap(),
            "Main"
        );
        assert!(normalize_source_label("   ", "RPlay channel").is_err());
        assert!(normalize_source_label("a\nb", "RPlay channel").is_err());
        assert!(
            normalize_source_label(&"x".repeat(MAX_SOURCE_LABEL_LEN + 1), "RPlay channel").is_err()
        );
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
                // RFC 3339 in UTC, with only as many fractional digits as
                // needed (jiff's format): `.5`, not `.500`.
                "timestamp": "2026-09-25T18:40:44.5Z",
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
                amount: "€5.00".into(),
                tier: Some(3),
                value: Some(5.0),
                currency: Some("EUR".into()),
            }),
            json!({ "type": "donation", "amount": "€5.00", "tier": 3, "value": 5.0, "currency": "EUR" })
        );
        assert_eq!(
            kind(MessageKind::Special {
                image_url: None,
                amount: Some("€2.00".into()),
                info: Some("cat".into()),
                tier: None,
                value: None,
                currency: None,
            }),
            json!({ "type": "special", "image_url": null, "amount": "€2.00", "info": "cat", "tier": null, "value": null, "currency": null })
        );
        assert_eq!(
            kind(MessageKind::MembershipJoin {
                info: "12 months".into(),
                months: Some(12),
            }),
            json!({ "type": "membership", "info": "12 months", "months": 12 })
        );
        assert_eq!(
            kind(MessageKind::MembershipJoin {
                info: "Welcome!".into(),
                months: None,
            }),
            json!({ "type": "membership", "info": "Welcome!", "months": null })
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
        assert_eq!(
            to_json(&ChatEvent::ClearAll {
                platform: ChatPlatform::Picarto,
            }),
            json!({ "type": "clear_all", "platform": "picarto" })
        );
        assert_eq!(
            to_json(&ChatEvent::ClearAll {
                platform: ChatPlatform::StreamElements,
            }),
            json!({ "type": "clear_all", "platform": "streamelements" })
        );
    }
}
