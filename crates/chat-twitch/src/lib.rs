use anyhow::Context;
use chat_core::EmoteRef;
use chat_core::{Author, ChatMessage, ChatPlatform, ChatSource, MessageKind};
use std::collections::HashSet;
use std::future::Future;
use tokio::sync::mpsc;
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::Emote;
use twitch_irc::message::{
    PrivmsgMessage, RGBColor, ServerMessage, UserNoticeEvent, UserNoticeMessage,
};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

pub struct TwitchSource {
    pub channel: String,
}

impl ChatSource for TwitchSource {
    fn run(
        self: Box<Self>,
        tx: mpsc::Sender<ChatMessage>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        async move {
            let config = ClientConfig::new_simple(StaticLoginCredentials::anonymous());
            let (mut incoming_messages, client) =
                TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

            client
                .join(self.channel.to_lowercase())
                .context("failed to join Twitch channel")?;

            while let Some(message) = incoming_messages.recv().await {
                let Some(msg) = convert(message) else {
                    continue;
                };
                // Receiver dropped -> nobody is listening anymore; shut down.
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
            Ok(())
        }
    }
}

fn rgb_to_hex(color: RGBColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

fn convert(message: ServerMessage) -> Option<ChatMessage> {
    match message {
        ServerMessage::Privmsg(p) => Some(convert_privmsg(p)),
        ServerMessage::UserNotice(un) => convert_user_notice(un),
        _ => None,
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
        None => match emote_only_codes(&p.message_text, &p.emotes) {
            Some(codes) => MessageKind::EmoteOnly { emotes: codes },
            None => MessageKind::Text,
        },
    };

    let emotes = unique_emotes(&p.emotes);

    ChatMessage {
        platform: ChatPlatform::Twitch,
        author: Author {
            id: p.sender.id,
            name: p.sender.name,
            color: p.name_color.map(rgb_to_hex),
            badges: p.badges.into_iter().map(|b| b.name).collect(),
            avatar_url: None,
        },
        text: p.message_text,
        emotes: emotes,
        timestamp: p.server_timestamp,
        kind,
    }
}

fn convert_user_notice(un: UserNoticeMessage) -> Option<ChatMessage> {
    let kind = match un.event {
        UserNoticeEvent::Raid { .. } | UserNoticeEvent::Announcement { .. } => {
            MessageKind::SystemNotice
        }
        UserNoticeEvent::SubOrResub { .. } => MessageKind::MembershipJoin {
            info: un.system_message.clone(),
        },
        UserNoticeEvent::SubGift { .. } => MessageKind::MembershipGift { amount: 1 },
        UserNoticeEvent::SubMysteryGift {
            mass_gift_count, ..
        }
        | UserNoticeEvent::AnonSubMysteryGift {
            mass_gift_count, ..
        } => MessageKind::MembershipGift {
            amount: mass_gift_count as usize,
        },
        _ => return None,
    };

    Some(ChatMessage {
        platform: ChatPlatform::Twitch,
        author: Author {
            id: un.sender.id,
            name: un.sender.name,
            color: un.name_color.map(rgb_to_hex),
            badges: un.badges.iter().map(|b| b.name.clone()).collect(),
            avatar_url: None,
        },
        text: un
            .message_text
            .clone()
            .unwrap_or_else(|| un.system_message.clone()),
        emotes: vec![],
        timestamp: un.server_timestamp,
        kind,
    })
}

/// If the message consists only of emotes (plus whitespace), return their codes.
fn emote_only_codes(text: &str, emotes: &[twitch_irc::message::Emote]) -> Option<Vec<String>> {
    if emotes.is_empty() {
        return None;
    }
    let mut covered = vec![false; text.chars().count()];
    for emote in emotes {
        for i in emote.char_range.clone() {
            covered[i] = true;
        }
    }
    // Any visible character outside an emote span means it's not emote-only.
    if text
        .chars()
        .zip(&covered)
        .any(|(c, is_emote)| !is_emote && !c.is_whitespace())
    {
        return None;
    }
    Some(emotes.iter().map(|e| e.code.clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use twitch_irc::message::IRCMessage;

    fn parse(raw: &str) -> ServerMessage {
        ServerMessage::try_from(IRCMessage::parse(raw).unwrap()).unwrap()
    }

    #[test]
    fn privmsg_converts() {
        let msg = parse(
            "@badge-info=;badges=moderator/1;color=#7F5AF0;display-name=Alice;emotes=;id=abc;mod=1;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=42;user-type=mod :alice!alice@alice.tmi.twitch.tv PRIVMSG #somechannel :hello world",
        );
        let out = convert(msg).unwrap();
        assert_eq!(out.author.name, "Alice");
        assert_eq!(out.author.color.as_deref(), Some("#7f5af0"));
        assert_eq!(out.author.badges, vec!["moderator".to_string()]);
        assert_eq!(out.text, "hello world");
        assert!(matches!(out.kind, MessageKind::Text));
    }

    #[test]
    fn bits_message_is_donation() {
        let msg = parse(
            "@badge-info=;badges=;bits=100;color=;display-name=Bob;emotes=;id=def;mod=0;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=43;user-type= :bob!bob@bob.tmi.twitch.tv PRIVMSG #somechannel :Cheer100 hey!",
        );
        let out = convert(msg).unwrap();
        assert!(matches!(out.kind, MessageKind::Donation { .. }));
    }

    #[test]
    fn emote_only_message_detected() {
        let msg = parse(
            "@badge-info=;badges=;color=;display-name=Cara;emotes=25:0-4;id=ghi;mod=0;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=44;user-type= :cara!cara@cara.tmi.twitch.tv PRIVMSG #somechannel :Kappa",
        );
        let out = convert(msg).unwrap();
        match out.kind {
            MessageKind::EmoteOnly { emotes } => assert_eq!(emotes, vec!["Kappa".to_string()]),
            other => panic!("expected EmoteOnly, got {other:?}"),
        }
    }
}
