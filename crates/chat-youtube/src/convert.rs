use std::collections::{BTreeMap, HashMap};

use crate::emoji::EmojiMap;
use crate::pb;
use chat_core::{Author, BoundedIdSet, ChatEvent, ChatMessage, ChatPlatform, MessageKind};
use pb::live_chat_message_snippet::DisplayedContent;
use pb::live_chat_message_snippet::type_wrapper::Type;

pub fn convert(
    item: pb::LiveChatMessage,
    dedupe: &mut BoundedIdSet,
    emojis: &EmojiMap,
    member_badges: &HashMap<String, String>,
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
                // YouTube's own tier (the colour in its chat), already
                // worked out across currencies: no exchange rates needed.
                tier: sc.tier,
                value: money(sc.amount_micros),
                currency: sc.currency,
            },
            sc.user_comment.unwrap_or_default(),
        ),
        Some(DisplayedContent::SuperStickerDetails(d)) => (
            MessageKind::Special {
                image_url: None, // the API gives no sticker image URL
                amount: d.amount_display_string,
                info: d.super_sticker_metadata.and_then(|m| m.alt_text),
                tier: d.tier,
                value: money(d.amount_micros),
                currency: d.currency,
            },
            String::new(),
        ),
        Some(DisplayedContent::GiftDetails(gift)) => (
            MessageKind::Special {
                image_url: gift.gift_url,
                amount: gift.jewels_amount.map(|j| format!("{j} jewels")),
                info: gift.gift_name.or(gift.alt_text),
                tier: None, // jewel gifts have no tiers
                value: gift.jewels_amount.map(|j| j as f64),
                currency: None, // jewels, not money
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
                months: None,
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
                // The field itself, not `member_month()`: that would turn a
                // missing value into 0.
                months: d.member_month,
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

    let author_id = author
        .channel_id
        .or(snippet.author_channel_id)
        .unwrap_or_default();

    let mut badges: Vec<String> = Vec::new();
    let mut badge_urls: BTreeMap<String, String> = BTreeMap::new();
    if author.is_chat_owner.unwrap_or(false) {
        badges.push("owner".into());
    }
    if author.is_chat_moderator.unwrap_or(false) {
        badges.push("moderator".into());
    }
    if author.is_chat_sponsor.unwrap_or(false) {
        badges.push("member".into());
        if let Some(url) = member_badges.get(&author_id) {
            badge_urls.insert("member".into(), url.clone());
        }
    }
    if author.is_verified.unwrap_or(false) {
        badges.push("verified".into());
    }

    Some(ChatEvent::Message(ChatMessage {
        id: item.id.unwrap_or_default(),
        platform: ChatPlatform::YouTube,
        author: Author {
            id: author_id,
            name: author
                .display_name
                .map(|n| n.trim_start_matches('@').to_string())
                .unwrap_or_default(),
            color: None, // YouTube doesn't assign name colors
            badges,
            badge_urls,
            avatar_url: author.profile_image_url,
        },
        text,
        emotes: found.emotes,
        timestamp: snippet
            .published_at
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .unwrap_or_else(jiff::Timestamp::now),
        kind,
    }))
}

/// A Super Chat's or Sticker's amount from YouTube's millionths
/// (`amount_micros`: 2,000,000 for €2.00).
fn money(micros: Option<u64>) -> Option<f64> {
    micros.map(|m| m as f64 / 1_000_000.0)
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
        match convert(item, &mut BoundedIdSet::new(16), emojis, &HashMap::new()) {
            Some(ChatEvent::Message(m)) => m,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn converts_text_message_and_custom_emoji() {
        let emojis = EmojiMap::from_json(
            r#"{ "version": 1, "entries": [ { "code": ":yt:", "url": "https://yt3.ggpht.com/yt" } ] }"#,
        )
        .unwrap();

        let msg = convert_msg(text_item("m1", "hi :yt:"), &emojis);
        assert_eq!(msg.id, "m1");
        assert_eq!(msg.author.name, "Ann");
        assert_eq!(msg.author.badges, vec!["moderator".to_string()]);
        assert_eq!(msg.text, "hi :yt:");
        assert_eq!(msg.emotes.len(), 1);
        assert_eq!(msg.emotes[0].url, "https://yt3.ggpht.com/yt");
        assert!(matches!(msg.kind, MessageKind::Text));

        let emote_only = convert_msg(text_item("m2", ":yt: :yt:"), &emojis);
        assert!(matches!(emote_only.kind, MessageKind::EmoteOnly));
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
                        amount_micros: Some(2_000_000),
                        currency: Some("EUR".into()),
                        user_comment: Some("WOO".into()),
                        tier: Some(2),
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
        let MessageKind::Donation {
            tier,
            value,
            currency,
            ..
        } = &msg.kind
        else {
            panic!("not a donation: {:?}", msg.kind);
        };
        assert_eq!(*tier, Some(2));
        // The number, for amount rules: in the viewer's currency.
        assert_eq!(*value, Some(2.0));
        assert_eq!(currency.as_deref(), Some("EUR"));
        // Only the user's own words; the amount lives in the kind.
        assert_eq!(msg.text, "WOO");
    }

    fn membership_item(r#type: Type, details: DisplayedContent) -> pb::LiveChatMessage {
        pb::LiveChatMessage {
            id: Some("m3".into()),
            snippet: Some(pb::LiveChatMessageSnippet {
                r#type: Some(r#type as i32),
                displayed_content: Some(details),
                ..Default::default()
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                display_name: Some("@Viewer42".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn milestone_and_new_member_handle_months() {
        let item = membership_item(
            Type::MemberMilestoneChatEvent,
            DisplayedContent::MemberMilestoneChatDetails(pb::LiveChatMemberMilestoneChatDetails {
                member_month: Some(43),
                user_comment: Some("still here".into()),
                ..Default::default()
            }),
        );
        let msg = convert_msg(item, &EmojiMap::default());
        match msg.kind {
            MessageKind::MembershipJoin { months, .. } => assert_eq!(months, Some(43)),
            other => panic!("expected MembershipJoin, got {other:?}"),
        }
        assert_eq!(msg.text, "still here");

        let new_member = membership_item(
            Type::NewSponsorEvent,
            DisplayedContent::NewSponsorDetails(pb::LiveChatNewSponsorDetails::default()),
        );
        let new_msg = convert_msg(new_member, &EmojiMap::default());
        assert!(matches!(
            new_msg.kind,
            MessageKind::MembershipJoin { months: None, .. }
        ));
    }

    #[test]
    fn tombstone_deletes_already_seen_message() {
        let mut dedupe = BoundedIdSet::new(16);
        let emojis = EmojiMap::default();
        let badges = HashMap::new();
        convert(text_item("m1", "oops"), &mut dedupe, &emojis, &badges);

        let tombstone = pb::LiveChatMessage {
            id: Some("m1".into()),
            snippet: Some(pb::LiveChatMessageSnippet {
                r#type: Some(Type::Tombstone as i32),
                ..Default::default()
            }),
            ..Default::default()
        };
        match convert(tombstone, &mut dedupe, &emojis, &badges) {
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
        match convert(
            item,
            &mut BoundedIdSet::new(16),
            &EmojiMap::default(),
            &HashMap::new(),
        ) {
            Some(ChatEvent::ClearUser { user_id, .. }) => assert_eq!(user_id, "UC_TROLL"),
            other => panic!("expected ClearUser, got {other:?}"),
        }
    }

    #[test]
    fn learned_member_badge_attaches_only_to_current_members() {
        let badges = HashMap::from([(
            "UC1".to_string(),
            "https://yt3.ggpht.com/member-badge=s32".to_string(),
        )]);

        // Active member gets the learned custom badge URL.
        let mut member_item = text_item("m1", "hello");
        member_item.author_details.as_mut().unwrap().is_chat_sponsor = Some(true);
        let Some(ChatEvent::Message(msg)) = convert(
            member_item,
            &mut BoundedIdSet::new(16),
            &EmojiMap::default(),
            &badges,
        ) else {
            panic!("expected message");
        };
        assert!(msg.author.badges.contains(&"member".to_string()));
        assert_eq!(
            msg.author.badge_urls.get("member").map(String::as_str),
            Some("https://yt3.ggpht.com/member-badge=s32")
        );

        // Non-member (e.g. expired membership) does not get the cached badge.
        let Some(ChatEvent::Message(non_member)) = convert(
            text_item("m2", "hello"),
            &mut BoundedIdSet::new(16),
            &EmojiMap::default(),
            &badges,
        ) else {
            panic!("expected message");
        };
        assert!(!non_member.author.badges.contains(&"member".to_string()));
        assert!(non_member.author.badge_urls.is_empty());
    }
}
