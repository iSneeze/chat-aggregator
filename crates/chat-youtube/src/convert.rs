use crate::emoji::EmojiMap;
use crate::pb;
use chat_core::{Author, ChatEvent, ChatMessage, ChatPlatform, MessageKind};
use pb::live_chat_message_snippet::DisplayedContent;
use pb::live_chat_message_snippet::type_wrapper::Type;

/// Keeps first-occurrence order of seen message ids, bounded in size so a
/// 24/7 process doesn't grow the set forever.
pub struct BoundedIdSet {
    seen: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
    cap: usize,
}

impl BoundedIdSet {
    pub fn new(cap: usize) -> Self {
        Self {
            seen: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
            cap,
        }
    }

    /// Returns true if this id was new.
    pub fn insert(&mut self, id: String) -> bool {
        if !self.seen.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

pub fn convert(
    item: pb::LiveChatMessage,
    dedupe: &mut BoundedIdSet,
    emojis: &EmojiMap,
) -> Option<ChatEvent> {
    let snippet = item.snippet?;

    // Gate on the event type first: several types have no display content
    // and must not fall through to the text path as empty messages.
    // (Unknown future type values also read as `InvalidType` and are dropped.)
    match snippet.r#type() {
        // A tombstone replaces a deleted message and reuses its id. It must
        // bypass dedupe (that id was already seen); repeats are harmless
        // since deleting twice is the same as deleting once.
        Type::Tombstone => {
            return item.id.map(|message_id| ChatEvent::Delete {
                platform: ChatPlatform::YouTube,
                message_id,
            });
        }
        Type::ChatEndedEvent
        | Type::SponsorOnlyModeStartedEvent
        | Type::SponsorOnlyModeEndedEvent
        | Type::PollEvent
        | Type::FanFundingEvent // deprecated, superseded by Super Chat
        | Type::InvalidType => return None,
        _ => {}
    }

    // Gift events reuse ids to update combo counts; dedupe keeps first only.
    // Reconnects without a page token also replay recent history.
    if let Some(id) = &item.id
        && !dedupe.insert(id.clone())
    {
        return None;
    }

    let author = item.author_details.unwrap_or_default();

    // YouTube's own rendering of the event; for plain chat it is the text.
    let display = snippet.display_message.unwrap_or_default();

    // `text` is only what the user typed; event descriptions go in `kind`.
    let (kind, text) = match snippet.displayed_content {
        // Moderation, not a message: the author here is the moderator.
        Some(DisplayedContent::UserBannedDetails(d)) => {
            return d
                .banned_user_details
                .and_then(|u| u.channel_id)
                .map(|user_id| ChatEvent::ClearUser {
                    platform: ChatPlatform::YouTube,
                    user_id,
                });
        }
        Some(DisplayedContent::SuperChatDetails(sc)) => (
            MessageKind::Donation {
                amount: sc.amount_display_string.unwrap_or_default(),
            },
            sc.user_comment.unwrap_or_default(),
        ),
        Some(DisplayedContent::SuperStickerDetails(d)) => (
            MessageKind::Special {
                image_url: None, // the API gives no sticker image URL
                amount: d.amount_display_string,
                info: d.super_sticker_metadata.and_then(|m| m.alt_text),
            },
            String::new(),
        ),
        Some(DisplayedContent::GiftDetails(gift)) => (
            MessageKind::Special {
                image_url: gift.gift_url,
                amount: gift.jewels_amount.map(|j| format!("{j} jewels")),
                info: gift.gift_name.or(gift.alt_text),
            },
            String::new(),
        ),
        Some(DisplayedContent::NewSponsorDetails(d)) => (
            MessageKind::MembershipJoin {
                // Prefer YouTube's own wording, like Twitch's system message.
                info: if display.is_empty() {
                    d.member_level_name.unwrap_or_else(|| "New member".into())
                } else {
                    display
                },
            },
            String::new(),
        ),
        Some(DisplayedContent::MemberMilestoneChatDetails(d)) => (
            MessageKind::MembershipJoin {
                info: format!(
                    "{} month{} member",
                    d.member_month(),
                    if d.member_month() == 1 { "" } else { "s" },
                ),
            },
            d.user_comment.unwrap_or_default(),
        ),
        Some(DisplayedContent::MembershipGiftingDetails(d)) => (
            MessageKind::MembershipGift {
                count: d.gift_memberships_count().max(0) as usize,
            },
            String::new(),
        ),
        // Per-recipient echo of a gifting event we already counted; skip
        // to avoid double-counting gifts.
        Some(DisplayedContent::GiftMembershipReceivedDetails(_)) => return None,
        // Already filtered by type above; listed so this match stays
        // exhaustive without a `_` arm (a new proto variant = compile error).
        Some(DisplayedContent::PollDetails(_)) => return None,
        Some(DisplayedContent::TextMessageDetails(_)) | None => (MessageKind::Text, display),
    };

    let found = emojis.find(&text);
    let kind = match kind {
        MessageKind::Text if found.emote_only => MessageKind::EmoteOnly,
        other => other,
    };

    let mut badges: Vec<String> = Vec::new();
    if author.is_chat_owner.unwrap_or(false) {
        badges.push("owner".into());
    }
    if author.is_chat_moderator.unwrap_or(false) {
        badges.push("moderator".into());
    }
    if author.is_chat_sponsor.unwrap_or(false) {
        badges.push("member".into());
    }
    if author.is_verified.unwrap_or(false) {
        badges.push("verified".into());
    }

    Some(ChatEvent::Message(ChatMessage {
        id: item.id.unwrap_or_default(),
        platform: ChatPlatform::YouTube,
        author: Author {
            id: author
                .channel_id
                .or(snippet.author_channel_id)
                .unwrap_or_default(),
            name: author
                .display_name
                .map(|n| n.trim_start_matches('@').to_string())
                .unwrap_or_default(),
            color: None, // YouTube doesn't assign name colors
            badges,
            avatar_url: author.profile_image_url,
        },
        text,
        emotes: found.emotes,
        timestamp: snippet
            .published_at
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now),
        kind,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb::LiveChatMessageAuthorDetails;

    fn text_item(id: &str, text: &str) -> pb::LiveChatMessage {
        pb::LiveChatMessage {
            id: Some(id.into()),
            snippet: Some(pb::LiveChatMessageSnippet {
                r#type: Some(Type::TextMessageEvent as i32),
                author_channel_id: Some("UC1".into()),
                published_at: Some("2026-09-06T10:00:00Z".into()),
                has_display_content: Some(true),
                display_message: Some(text.into()),
                displayed_content: Some(DisplayedContent::TextMessageDetails(
                    pb::LiveChatTextMessageDetails {
                        message_text: Some(text.into()),
                    },
                )),
                ..Default::default()
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                channel_id: Some("UC1".into()),
                display_name: Some("Ann".into()),
                is_chat_moderator: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Runs `convert` and unwraps a `ChatEvent::Message`.
    fn convert_msg(item: pb::LiveChatMessage, emojis: &EmojiMap) -> ChatMessage {
        match convert(item, &mut BoundedIdSet::new(16), emojis) {
            Some(ChatEvent::Message(m)) => m,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn converts_text_message() {
        let msg = convert_msg(text_item("m1", "hello"), &EmojiMap::default());
        assert_eq!(msg.id, "m1");
        assert_eq!(msg.author.name, "Ann");
        assert_eq!(msg.author.badges, vec!["moderator".to_string()]);
        assert_eq!(msg.text, "hello");
        assert!(matches!(msg.kind, MessageKind::Text));
    }

    #[test]
    fn converts_super_chat_to_donation() {
        let item = pb::LiveChatMessage {
            id: Some("m2".into()),
            snippet: Some(pb::LiveChatMessageSnippet {
                r#type: Some(Type::SuperChatEvent as i32),
                display_message: Some("€2.00 WOO".into()),
                displayed_content: Some(DisplayedContent::SuperChatDetails(
                    pb::LiveChatSuperChatDetails {
                        amount_display_string: Some("€2.00".into()),
                        user_comment: Some("WOO".into()),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                display_name: Some("Bob".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let msg = convert_msg(item, &EmojiMap::default());
        assert!(matches!(msg.kind, MessageKind::Donation { .. }));
        // Only the user's own words; the amount lives in the kind.
        assert_eq!(msg.text, "WOO");
    }

    #[test]
    fn duplicate_id_is_dropped() {
        let mut dedupe = BoundedIdSet::new(16);
        let emojis = EmojiMap::default();
        assert!(convert(text_item("m1", "hi"), &mut dedupe, &emojis).is_some());
        assert!(convert(text_item("m1", "hi"), &mut dedupe, &emojis).is_none());
    }

    #[test]
    fn tombstone_deletes_already_seen_message() {
        let mut dedupe = BoundedIdSet::new(16);
        let emojis = EmojiMap::default();
        convert(text_item("m1", "oops"), &mut dedupe, &emojis);

        let tombstone = pb::LiveChatMessage {
            id: Some("m1".into()),
            snippet: Some(pb::LiveChatMessageSnippet {
                r#type: Some(Type::Tombstone as i32),
                ..Default::default()
            }),
            ..Default::default()
        };
        match convert(tombstone, &mut dedupe, &emojis) {
            Some(ChatEvent::Delete { message_id, .. }) => assert_eq!(message_id, "m1"),
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn ban_clears_banned_user_not_moderator() {
        let item = pb::LiveChatMessage {
            id: Some("b1".into()),
            snippet: Some(pb::LiveChatMessageSnippet {
                r#type: Some(Type::UserBannedEvent as i32),
                author_channel_id: Some("UC_MOD".into()),
                displayed_content: Some(DisplayedContent::UserBannedDetails(
                    pb::LiveChatUserBannedMessageDetails {
                        banned_user_details: Some(pb::ChannelProfileDetails {
                            channel_id: Some("UC_TROLL".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
            ..Default::default()
        };
        match convert(item, &mut BoundedIdSet::new(16), &EmojiMap::default()) {
            Some(ChatEvent::ClearUser { user_id, .. }) => assert_eq!(user_id, "UC_TROLL"),
            other => panic!("expected ClearUser, got {other:?}"),
        }
    }

    #[test]
    fn silent_events_are_skipped() {
        for ty in [Type::ChatEndedEvent, Type::SponsorOnlyModeStartedEvent] {
            let item = pb::LiveChatMessage {
                id: Some("s1".into()),
                snippet: Some(pb::LiveChatMessageSnippet {
                    r#type: Some(ty as i32),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let out = convert(item, &mut BoundedIdSet::new(16), &EmojiMap::default());
            assert!(out.is_none(), "{ty:?} should be skipped, got {out:?}");
        }
    }

    #[test]
    fn custom_emoji_are_resolved() {
        let emojis = EmojiMap::from_json(
            r#"{ "version": 1, "entries": [ { "code": ":yt:", "url": "https://yt3.ggpht.com/yt" } ] }"#,
        )
        .unwrap();

        let msg = convert_msg(text_item("m1", "hi :yt:"), &emojis);
        assert_eq!(msg.emotes.len(), 1);
        assert_eq!(msg.emotes[0].url, "https://yt3.ggpht.com/yt");
        assert!(matches!(msg.kind, MessageKind::Text));

        let msg = convert_msg(text_item("m2", ":yt: :yt:"), &emojis);
        assert!(matches!(msg.kind, MessageKind::EmoteOnly));
    }
}
