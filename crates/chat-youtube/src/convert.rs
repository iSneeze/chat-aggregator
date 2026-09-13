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

use crate::pb;
use chat_core::{Author, ChatMessage, ChatPlatform, MessageKind};

pub fn convert(item: pb::LiveChatMessage, dedupe: &mut BoundedIdSet) -> Option<ChatMessage> {
    // Gift events reuse ids to update combo counts; dedupe keeps first only.
    if let Some(id) = &item.id {
        if !dedupe.insert(id.clone()) {
            return None;
        }
    }

    let snippet = item.snippet?;
    let author = item.author_details.unwrap_or_default();

    let kind = match (snippet.r#type, snippet.displayed_content) {
        (_, Some(pb::live_chat_message_snippet::DisplayedContent::SuperChatDetails(sc))) => {
            MessageKind::Donation {
                amount: sc.amount_display_string.unwrap_or_default(),
            }
        }
        (_, Some(pb::live_chat_message_snippet::DisplayedContent::SuperStickerDetails(_))) => {
            MessageKind::Special { emote_url: None } // TODO: API gives no sticker URL
        }
        (_, Some(pb::live_chat_message_snippet::DisplayedContent::GiftDetails(gift))) => {
            MessageKind::Special {
                emote_url: gift.gift_url,
            }
        }
        (_, Some(pb::live_chat_message_snippet::DisplayedContent::NewSponsorDetails(d))) => {
            MessageKind::MembershipJoin {
                info: d.member_level_name.unwrap_or_else(|| "member".into()),
            }
        }
        (
            _,
            Some(pb::live_chat_message_snippet::DisplayedContent::MemberMilestoneChatDetails(d)),
        ) => MessageKind::MembershipJoin {
            info: format!(
                "{} month{} member{}",
                d.member_month(),
                if d.member_month() == 1 { "" } else { "s" },
                d.user_comment
                    .filter(|c| !c.is_empty())
                    .map(|c| format!(": {c}"))
                    .unwrap_or_default()
            ),
        },
        (_, Some(pb::live_chat_message_snippet::DisplayedContent::MembershipGiftingDetails(d))) => {
            MessageKind::MembershipGift {
                amount: d.gift_memberships_count().max(0) as usize,
            }
        }
        // Per-recipient echo of a gifting event we already counted; skip
        // to avoid double-counting gifts.
        (
            _,
            Some(pb::live_chat_message_snippet::DisplayedContent::GiftMembershipReceivedDetails(_)),
        ) => {
            return None;
        }
        // Tombstones are deletion markers; nothing to display.
        _ => MessageKind::Text,
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

    Some(ChatMessage {
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
            badges: badges,
            avatar_url: author.profile_image_url,
        },
        text: snippet.display_message.unwrap_or_default(),
        emotes: vec![], // YouTube emotes (:like:) have no id/url in this API
        timestamp: snippet
            .published_at
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now),
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb::LiveChatMessageAuthorDetails;

    #[test]
    fn converts_text_message() {
        let item = pb::LiveChatMessage {
            id: Some("m1".into()),
            snippet: Some(pb::LiveChatMessageSnippet {
                r#type: Some(
                    pb::live_chat_message_snippet::type_wrapper::Type::TextMessageEvent as i32,
                ),
                author_channel_id: Some("UC1".into()),
                published_at: Some("2026-09-06T10:00:00Z".into()),
                has_display_content: Some(true),
                display_message: Some("hello".into()),
                displayed_content: Some(
                    pb::live_chat_message_snippet::DisplayedContent::TextMessageDetails(
                        pb::LiveChatTextMessageDetails {
                            message_text: Some("hello".into()),
                        },
                    ),
                ),
                ..Default::default()
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                channel_id: Some("UC1".into()),
                display_name: Some("Ann".into()),
                is_chat_moderator: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut dedupe = BoundedIdSet::new(16);
        let msg = convert(item, &mut dedupe).unwrap();
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
                r#type: Some(
                    pb::live_chat_message_snippet::type_wrapper::Type::SuperChatEvent as i32,
                ),
                display_message: Some("€2.00 WOO".into()),
                displayed_content: Some(
                    pb::live_chat_message_snippet::DisplayedContent::SuperChatDetails(
                        pb::LiveChatSuperChatDetails {
                            amount_display_string: Some("€2.00".into()),
                            user_comment: Some("WOO".into()),
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                display_name: Some("Bob".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut dedupe = BoundedIdSet::new(16);
        let msg = convert(item, &mut dedupe).unwrap();
        assert!(matches!(msg.kind, MessageKind::Donation { .. }));
        assert_eq!(msg.text, "€2.00 WOO");
    }
}
