use anyhow::Context;
use chat_core::{Author, ChatEvent, ChatMessage, ChatPlatform, ChatSource, EmoteRef, MessageKind};
use std::collections::HashSet;
use tokio::sync::mpsc;
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::{
    ClearChatAction, ClearChatMessage, Emote, PrivmsgMessage, RGBColor, ServerMessage,
    UserNoticeEvent, UserNoticeMessage,
};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

pub struct TwitchSource {
    pub channel: String,
}

impl ChatSource for TwitchSource {
    // `async fn` satisfies the trait's `impl Future + Send` as long as the
    // compiler can prove the future is Send; it checks this for us.
    async fn run(self: Box<Self>, tx: mpsc::Sender<ChatEvent>) -> anyhow::Result<()> {
        let config = ClientConfig::new_simple(StaticLoginCredentials::anonymous());
        let (mut incoming_messages, client) =
            TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

        client
            .join(self.channel.to_lowercase())
            .context("failed to join Twitch channel")?;

        while let Some(message) = incoming_messages.recv().await {
            let Some(event) = convert(message) else {
                continue;
            };
            // Receiver dropped -> nobody is listening anymore; shut down.
            if tx.send(event).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

fn rgb_to_hex(color: RGBColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

fn convert(message: ServerMessage) -> Option<ChatEvent> {
    match message {
        ServerMessage::Privmsg(p) => Some(ChatEvent::Message(convert_privmsg(p))),
        ServerMessage::UserNotice(un) => convert_user_notice(un).map(ChatEvent::Message),
        ServerMessage::ClearMsg(m) => Some(ChatEvent::Delete {
            platform: ChatPlatform::Twitch,
            message_id: m.message_id,
        }),
        ServerMessage::ClearChat(c) => Some(convert_clear_chat(c)),
        _ => None,
    }
}

fn convert_clear_chat(c: ClearChatMessage) -> ChatEvent {
    match c.action {
        ClearChatAction::ChatCleared => ChatEvent::ClearAll {
            platform: ChatPlatform::Twitch,
        },
        // For display purposes a timeout is a ban: the user's messages go.
        ClearChatAction::UserBanned { user_id, .. }
        | ClearChatAction::UserTimedOut { user_id, .. } => ChatEvent::ClearUser {
            platform: ChatPlatform::Twitch,
            user_id,
        },
    }
}

fn twitch_emote_url(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0")
}

/// Unique emotes in first-occurrence order. The renderer replaces codes in
/// the text itself, so occurrence counts don't need to be stored here.
fn unique_emotes(emotes: &[Emote]) -> Vec<EmoteRef> {
    let mut seen = HashSet::new();
    emotes
        .iter()
        .filter(|e| seen.insert(e.code.as_str()))
        .map(|e| EmoteRef {
            code: e.code.clone(),
            url: twitch_emote_url(&e.id),
        })
        .collect()
}

fn convert_privmsg(p: PrivmsgMessage) -> ChatMessage {
    // Bits arrive as a normal chat message carrying a "bits" tag.
    let kind = match p.bits {
        Some(bits) => MessageKind::Donation {
            amount: format!("{bits} bits"),
        },
        None if is_emote_only(&p.message_text, &p.emotes) => MessageKind::EmoteOnly,
        None => MessageKind::Text,
    };

    ChatMessage {
        id: p.message_id,
        platform: ChatPlatform::Twitch,
        author: Author {
            id: p.sender.id,
            name: p.sender.name,
            color: p.name_color.map(rgb_to_hex),
            badges: p.badges.into_iter().map(|b| b.name).collect(),
            avatar_url: None,
        },
        emotes: unique_emotes(&p.emotes),
        text: p.message_text,
        timestamp: p.server_timestamp,
        kind,
    }
}

/// Individual `subgift` notices that belong to a community gift carry this
/// tag; a direct gift to one person does not.
const COMMUNITY_GIFT_TAG: &str = "msg-param-community-gift-id";

fn convert_user_notice(un: UserNoticeMessage) -> Option<ChatMessage> {
    let kind = match &un.event {
        UserNoticeEvent::Raid { .. } | UserNoticeEvent::Announcement { .. } => {
            MessageKind::SystemNotice
        }
        UserNoticeEvent::SubOrResub { .. } => MessageKind::MembershipJoin {
            info: un.system_message.clone(),
        },
        // A community gift of N subs arrives as one `submysterygift` (counted
        // below) followed by N per-recipient `subgift`s: skip those, or every
        // gift would be counted twice.
        UserNoticeEvent::SubGift { .. } if un.source.tags.0.contains_key(COMMUNITY_GIFT_TAG) => {
            return None;
        }
        UserNoticeEvent::SubGift { .. } => MessageKind::MembershipGift { amount: 1 },
        UserNoticeEvent::SubMysteryGift {
            mass_gift_count, ..
        }
        | UserNoticeEvent::AnonSubMysteryGift {
            mass_gift_count, ..
        } => MessageKind::MembershipGift {
            amount: *mass_gift_count as usize,
        },
        _ => return None,
    };

    Some(ChatMessage {
        id: un.message_id,
        platform: ChatPlatform::Twitch,
        author: Author {
            id: un.sender.id,
            name: un.sender.name,
            color: un.name_color.map(rgb_to_hex),
            badges: un.badges.into_iter().map(|b| b.name).collect(),
            avatar_url: None,
        },
        // Resub messages can contain emotes, just like normal chat.
        emotes: unique_emotes(&un.emotes),
        text: un.message_text.unwrap_or(un.system_message),
        timestamp: un.server_timestamp,
        kind,
    })
}

/// True if the message consists only of emotes (plus whitespace).
fn is_emote_only(text: &str, emotes: &[Emote]) -> bool {
    if emotes.is_empty() {
        return false;
    }
    let mut covered = vec![false; text.chars().count()];
    for emote in emotes {
        // Twitch sometimes sends ranges past the end of the text, and
        // twitch-irc passes them through (twitchdev/issues#104). Clamp
        // instead of indexing, which would panic and kill the source.
        let range = emote.char_range.start..emote.char_range.end.min(covered.len());
        if let Some(span) = covered.get_mut(range) {
            span.fill(true);
        }
    }
    // Any visible character outside an emote span means it's not emote-only.
    text.chars()
        .zip(&covered)
        .all(|(c, &is_emote)| is_emote || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;
    use twitch_irc::message::IRCMessage;

    fn parse(raw: &str) -> ServerMessage {
        ServerMessage::try_from(IRCMessage::parse(raw).unwrap()).unwrap()
    }

    /// Converts and unwraps a `ChatEvent::Message`.
    fn convert_msg(raw: &str) -> ChatMessage {
        match convert(parse(raw)) {
            Some(ChatEvent::Message(m)) => m,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn privmsg_converts() {
        let out = convert_msg(
            "@badge-info=;badges=moderator/1;color=#7F5AF0;display-name=Alice;emotes=;id=abc;mod=1;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=42;user-type=mod :alice!alice@alice.tmi.twitch.tv PRIVMSG #somechannel :hello world",
        );
        assert_eq!(out.id, "abc");
        assert_eq!(out.author.name, "Alice");
        assert_eq!(out.author.color.as_deref(), Some("#7f5af0"));
        assert_eq!(out.author.badges, vec!["moderator".to_string()]);
        assert_eq!(out.text, "hello world");
        assert!(matches!(out.kind, MessageKind::Text));
    }

    #[test]
    fn bits_message_is_donation() {
        let out = convert_msg(
            "@badge-info=;badges=;bits=100;color=;display-name=Bob;emotes=;id=def;mod=0;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=43;user-type= :bob!bob@bob.tmi.twitch.tv PRIVMSG #somechannel :Cheer100 hey!",
        );
        assert!(matches!(out.kind, MessageKind::Donation { .. }));
    }

    #[test]
    fn emote_only_message_detected() {
        let out = convert_msg(
            "@badge-info=;badges=;color=;display-name=Cara;emotes=25:0-4,6-10;id=ghi;mod=0;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=44;user-type= :cara!cara@cara.tmi.twitch.tv PRIVMSG #somechannel :Kappa Kappa",
        );
        assert!(matches!(out.kind, MessageKind::EmoteOnly));
        // Two occurrences, one unique emote.
        assert_eq!(out.emotes.len(), 1);
        assert_eq!(out.emotes[0].code, "Kappa");
    }

    #[test]
    fn out_of_bounds_emote_range_does_not_panic() {
        // Range 0-9 on a 5-char message: the Twitch bug twitch-irc passes through.
        let out = convert_msg(
            "@badge-info=;badges=;color=;display-name=Dan;emotes=25:0-9;id=jkl;mod=0;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=45;user-type= :dan!dan@dan.tmi.twitch.tv PRIVMSG #somechannel :Kappa",
        );
        assert!(matches!(out.kind, MessageKind::EmoteOnly));
    }

    #[test]
    fn clearmsg_is_delete() {
        let event = convert(parse(
            "@login=alazymeme;room-id=;target-msg-id=3c92014f-340a-4dc3-a9c9-e5cf182f4a84;tmi-sent-ts=1594561955611 :tmi.twitch.tv CLEARMSG #pajlada :bye",
        ));
        match event {
            Some(ChatEvent::Delete { message_id, .. }) => {
                assert_eq!(message_id, "3c92014f-340a-4dc3-a9c9-e5cf182f4a84")
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn timeout_and_ban_clear_user() {
        for raw in [
            // timeout (has ban-duration)
            "@ban-duration=1;room-id=11148817;target-user-id=148973258;tmi-sent-ts=1594553828245 :tmi.twitch.tv CLEARCHAT #pajlada :fabzeef",
            // permanent ban
            "@room-id=11148817;target-user-id=148973258;tmi-sent-ts=1594561360331 :tmi.twitch.tv CLEARCHAT #pajlada :fabzeef",
        ] {
            match convert(parse(raw)) {
                Some(ChatEvent::ClearUser { user_id, .. }) => assert_eq!(user_id, "148973258"),
                other => panic!("expected ClearUser, got {other:?}"),
            }
        }
    }

    #[test]
    fn clearchat_without_user_clears_all() {
        let event = convert(parse(
            "@room-id=40286300;tmi-sent-ts=1594561392337 :tmi.twitch.tv CLEARCHAT #randers",
        ));
        assert!(matches!(event, Some(ChatEvent::ClearAll { .. })));
    }

    const SUBGIFT: &str = "@badge-info=;badges=;color=;display-name=Gifter;emotes=;flags=;id=g1;login=gifter;mod=0;msg-id=subgift;msg-param-gift-months=1;msg-param-months=2;msg-param-recipient-display-name=Lucky;msg-param-recipient-id=99;msg-param-recipient-user-name=lucky;msg-param-sub-plan-name=Sub;msg-param-sub-plan=1000;room-id=123;subscriber=0;system-msg=Gifter\\sgifted\\sa\\ssub;tmi-sent-ts=1700000000000;user-id=46;user-type= :tmi.twitch.tv USERNOTICE #somechannel";

    #[test]
    fn direct_sub_gift_counts_one() {
        let out = convert_msg(SUBGIFT);
        assert!(matches!(
            out.kind,
            MessageKind::MembershipGift { amount: 1 }
        ));
    }

    #[test]
    fn sub_gift_from_community_gift_is_skipped() {
        let raw = SUBGIFT.replacen(
            "msg-id=subgift;",
            "msg-id=subgift;msg-param-community-gift-id=777;",
            1,
        );
        assert!(convert(parse(&raw)).is_none());
    }

    #[test]
    fn mystery_gift_counts_the_wave() {
        let out = convert_msg(
            "@badge-info=;badges=;color=;display-name=Gifter;emotes=;flags=;id=g0;login=gifter;mod=0;msg-id=submysterygift;msg-param-mass-gift-count=5;msg-param-origin-id=abc;msg-param-sender-count=5;msg-param-sub-plan=1000;room-id=123;subscriber=0;system-msg=Gifter\\sis\\sgifting\\s5\\ssubs;tmi-sent-ts=1700000000000;user-id=46;user-type= :tmi.twitch.tv USERNOTICE #somechannel",
        );
        assert!(matches!(
            out.kind,
            MessageKind::MembershipGift { amount: 5 }
        ));
    }
}
