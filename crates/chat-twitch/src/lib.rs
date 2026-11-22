use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use chat_core::{
    Activity, Author, ChatEvent, ChatMessage, ChatPlatform, ChatSource, EmoteRef, MessageKind,
    Reporter,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::{
    ClearChatAction, ClearChatMessage, Emote, IRCMessage, IRCPrefix, PrivmsgMessage, RGBColor,
    ServerMessage, UserNoticeEvent, UserNoticeMessage,
};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

mod badges;

use badges::BadgeMap;

pub struct TwitchSource {
    pub channel: String,
}

/// Turns what a user typed ("#Some_Channel ") into a valid Twitch login
/// ("some_channel"), or explains what's wrong with it. Checking this up
/// front matters: an invalid name can never work, so it shouldn't be
/// retried like a network problem.
pub fn normalize_channel(input: &str) -> Result<String, String> {
    let channel = input.trim().trim_start_matches('#').to_lowercase();
    twitch_irc::validate::validate_login(&channel)
        .map(|()| channel)
        .map_err(|e| format!("invalid Twitch channel name: {e}"))
}

impl ChatSource for TwitchSource {
    // `async fn` satisfies the trait's `impl Future + Send` as long as the
    // compiler can prove the future is Send; it checks this for us.
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        let channel = self.channel.to_lowercase();
        let badge_map = Arc::new(RwLock::new(BadgeMap::builtin()));
        let badge_task = tokio::spawn({
            let channel = channel.clone();
            let badge_map = Arc::clone(&badge_map);
            async move {
                match BadgeMap::fetch_for_channel(&channel).await {
                    Ok(fetched) => {
                        debug!(channel, count = fetched.len(), "loaded Twitch chat badges");
                        if let Ok(mut guard) = badge_map.write() {
                            *guard = fetched;
                        }
                    }
                    Err(e) => {
                        warn!(
                            channel,
                            "couldn't load Twitch channel badges, using built-in icons: {e:#}"
                        );
                    }
                }
            }
        });

        let config = ClientConfig::new_simple(StaticLoginCredentials::anonymous());
        let (mut incoming_messages, client) =
            TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);

        let join_result = client
            .join(channel)
            .context("failed to join Twitch channel");
        if let Err(e) = join_result {
            badge_task.abort();
            return Err(e);
        }

        while let Some(message) = incoming_messages.recv().await {
            match message {
                // Twitch confirms our (anonymous) join before any chat
                // arrives. A channel that doesn't exist never confirms, so
                // it stays "connecting", which is the right signal.
                ServerMessage::Join(_)
                | ServerMessage::Privmsg(_)
                | ServerMessage::UserNotice(_) => activity.set(Activity::Receiving),
                // Twitch asks clients to reconnect now and then; twitch-irc
                // does that by itself, and we'll see the join again.
                ServerMessage::Reconnect(_) => activity.set(Activity::Connecting),
                _ => {}
            }
            let event = {
                let guard = badge_map.read().unwrap_or_else(|e| e.into_inner());
                convert(message, &guard)
            };
            let Some(event) = event else {
                continue;
            };
            // Receiver dropped -> nobody is listening anymore; shut down.
            if tx.send(event).await.is_err() {
                break;
            }
        }
        badge_task.abort();
        Ok(())
    }
}

fn rgb_to_hex(color: RGBColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

fn convert(message: ServerMessage, badges: &BadgeMap) -> Option<ChatEvent> {
    match message {
        ServerMessage::Privmsg(p) => Some(ChatEvent::Message(convert_privmsg(p, badges))),
        ServerMessage::UserNotice(un) => convert_user_notice(un, badges).map(ChatEvent::Message),
        ServerMessage::ClearMsg(m) => Some(ChatEvent::Delete {
            platform: ChatPlatform::Twitch,
            message_id: m.message_id,
        }),
        ServerMessage::ClearChat(c) => Some(convert_clear_chat(c)),
        // When twitch-irc fails to strictly parse a PRIVMSG or USERNOTICE
        // (for instance a raid missing `msg-param-profileImageURL`, a shared
        // chat notice without `system-msg`, an empty `display-name=`, or a
        // Prediction `badge-info` containing a comma), it wraps the original
        // IRCMessage in `ServerMessage::Generic`. Recover it leniently so
        // raids, watch streaks, and chat aren't silently dropped.
        other => convert_raw_irc(&IRCMessage::from(other), badges),
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

fn convert_privmsg(p: PrivmsgMessage, badges: &BadgeMap) -> ChatMessage {
    // Bits arrive as a normal chat message carrying a "bits" tag.
    let kind = match p.bits {
        Some(bits) => MessageKind::Donation {
            amount: format!("{bits} bits"),
            // On the scale every platform shares (YouTube's dollar steps
            // × 100), not Twitch's own cheer colours: so a reaction's
            // "tier 4 or more" means about the same everywhere.
            tier: Some(chat_core::tiers::bits_tier(bits)),
            value: Some(bits as f64),
            currency: None,
        },
        None if is_emote_only(&p.message_text, &p.emotes) => MessageKind::EmoteOnly,
        None => MessageKind::Text,
    };

    let badge_urls = badges.resolve(&p.badges);

    ChatMessage {
        id: p.message_id,
        platform: ChatPlatform::Twitch,
        author: Author {
            id: p.sender.id,
            name: p.sender.name,
            color: p.name_color.map(rgb_to_hex),
            badges: p.badges.into_iter().map(|b| b.name).collect(),
            badge_urls,
            avatar_url: None,
        },
        emotes: unique_emotes(&p.emotes),
        text: p.message_text,
        timestamp: timestamp(p.server_timestamp.timestamp_millis()),
        kind,
    }
}

/// twitch-irc turns the `tmi-sent-ts` tag (milliseconds since 1970) into a
/// chrono time; chat-core uses jiff. Going through the milliseconds loses
/// nothing, since that's all the tag carries. Only a time beyond the year
/// 9999 could fail; "now" is a fine stand-in for such nonsense.
fn timestamp(millis: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_millisecond(millis).unwrap_or_else(|_| jiff::Timestamp::now())
}

/// Individual `subgift` notices that belong to a community gift carry this
/// tag; a direct gift to one person does not.
const COMMUNITY_GIFT_TAG: &str = "msg-param-community-gift-id";

fn tag<'a>(irc: &'a IRCMessage, key: &str) -> Option<&'a str> {
    irc.tags
        .0
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

/// Synthesizes a readable notice when Twitch sends an empty `system-msg`
/// (common on `/announce` and Shared Chat notices, and as a safety net for
/// raids and watch streaks).
fn fallback_notice_info(irc: &IRCMessage, event_id: &str, sender_name: &str) -> String {
    match event_id {
        "raid" => {
            let raider = tag(irc, "msg-param-displayName").unwrap_or(sender_name);
            match tag(irc, "msg-param-viewerCount").and_then(|v| v.parse::<u64>().ok()) {
                Some(count) => format!("{raider} is raiding with a party of {count}!"),
                None => format!("{raider} is raiding!"),
            }
        }
        "announcement" => {
            if sender_name.is_empty() {
                "Announcement".into()
            } else {
                format!("Announcement from {sender_name}")
            }
        }
        "viewermilestone" if tag(irc, "msg-param-category") == Some("watch-streak") => {
            match tag(irc, "msg-param-value").and_then(|v| v.parse::<u64>().ok()) {
                Some(count) => format!(
                    "{sender_name} watched {count} consecutive streams and sparked a watch streak!"
                ),
                None => format!("{sender_name} reached a watch streak milestone!"),
            }
        }
        "bitsbadgetier" => match tag(irc, "msg-param-threshold").and_then(|v| v.parse::<u64>().ok())
        {
            Some(threshold) => format!("{sender_name} earned the {threshold} bits badge!"),
            None => format!("{sender_name} earned a new bits badge!"),
        },
        _ => String::new(),
    }
}

fn convert_user_notice(un: UserNoticeMessage, badges: &BadgeMap) -> Option<ChatMessage> {
    // Shared Chat notices wrap the original notice under `source-msg-id`.
    let effective_event_id = if un.event_id == "sharedchatnotice" {
        tag(&un.source, "source-msg-id").unwrap_or(un.event_id.as_str())
    } else {
        un.event_id.as_str()
    };

    let text = un.message_text.unwrap_or_default();
    let kind = match un.event {
        UserNoticeEvent::Raid { viewer_count, .. } => {
            let info = if un.system_message.is_empty() {
                format!("{} is raiding with a party of {viewer_count}!", un.sender.name)
            } else {
                un.system_message
            };
            MessageKind::SystemNotice { info }
        }
        UserNoticeEvent::Announcement { .. } => {
            let info = if un.system_message.is_empty() {
                fallback_notice_info(&un.source, "announcement", &un.sender.name)
            } else {
                un.system_message
            };
            MessageKind::SystemNotice { info }
        }
        UserNoticeEvent::SubOrResub {
            is_resub,
            cumulative_months,
            ..
        } => MessageKind::MembershipJoin {
            info: un.system_message,
            // A first sub is a welcome, not a milestone: no month count.
            // (`try_from`: the tag is parsed as a u64; a count that doesn't
            // fit a u32 would be garbage, so it's dropped rather than cut.)
            months: if is_resub {
                u32::try_from(cumulative_months).ok()
            } else {
                None
            },
        },
        // A community gift of N subs arrives as one `submysterygift` (counted
        // below) followed by N per-recipient `subgift`s: skip those, or every
        // gift would be counted twice.
        UserNoticeEvent::SubGift { .. } if un.source.tags.0.contains_key(COMMUNITY_GIFT_TAG) => {
            return None;
        }
        UserNoticeEvent::SubGift { .. } => MessageKind::MembershipGift { count: 1 },
        UserNoticeEvent::SubMysteryGift {
            mass_gift_count, ..
        }
        | UserNoticeEvent::AnonSubMysteryGift {
            mass_gift_count, ..
        } => MessageKind::MembershipGift {
            count: mass_gift_count as usize,
        },
        // Its `system-msg` is only "bits badge tier notification".
        UserNoticeEvent::BitsBadgeTier { threshold } => MessageKind::SystemNotice {
            info: format!("{} earned the {threshold} bits badge!", un.sender.name),
        },
        // Everything else Twitch announces: watch streaks (`viewermilestone`),
        // Shared Chat notices (`sharedchatnotice`), new chatters, gift-sub
        // upgrades, and newer event types (`Unknown`).
        _ => match classify_raw_user_notice(
            &un.source,
            effective_event_id,
            &un.sender.name,
            un.system_message,
            &text,
        ) {
            Some(kind) => kind,
            None => return None,
        },
    };

    let badge_urls = badges.resolve(&un.badges);

    Some(ChatMessage {
        id: un.message_id,
        platform: ChatPlatform::Twitch,
        author: Author {
            id: un.sender.id,
            name: un.sender.name,
            color: un.name_color.map(rgb_to_hex),
            badges: un.badges.into_iter().map(|b| b.name).collect(),
            badge_urls,
            avatar_url: None,
        },
        // Resub & watch-streak messages can contain emotes, just like normal chat.
        emotes: unique_emotes(&un.emotes),
        // Only what the user typed; the system description is in `kind`.
        text,
        timestamp: timestamp(un.server_timestamp.timestamp_millis()),
        kind,
    })
}

/// Classifies a `USERNOTICE` from its raw IRC tags when `twitch-irc` either
/// produced `UserNoticeEvent::Unknown` (e.g. `viewermilestone`,
/// `sharedchatnotice`) or fell back to `ServerMessage::Generic`.
fn classify_raw_user_notice(
    irc: &IRCMessage,
    event_id: &str,
    sender_name: &str,
    system_message: String,
    text: &str,
) -> Option<MessageKind> {
    match event_id {
        "sub" | "resub" => {
            let months = (event_id == "resub")
                .then(|| {
                    tag(irc, "msg-param-cumulative-months")
                        .and_then(|v| v.parse::<u32>().ok())
                })
                .flatten();
            let info = if system_message.is_empty() {
                match months {
                    Some(m) => format!("{sender_name} subscribed for {m} months!"),
                    None => format!("{sender_name} subscribed!"),
                }
            } else {
                system_message
            };
            Some(MessageKind::MembershipJoin { info, months })
        }
        "subgift" | "anonsubgift" => {
            if irc.tags.0.contains_key(COMMUNITY_GIFT_TAG) {
                return None;
            }
            Some(MessageKind::MembershipGift { count: 1 })
        }
        "submysterygift" | "anonsubmysterygift" => {
            let count = tag(irc, "msg-param-mass-gift-count")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1);
            Some(MessageKind::MembershipGift { count })
        }
        "bitsbadgetier" => Some(MessageKind::SystemNotice {
            info: fallback_notice_info(irc, "bitsbadgetier", sender_name),
        }),
        _ => {
            let info = if system_message.is_empty() {
                fallback_notice_info(irc, event_id, sender_name)
            } else {
                system_message
            };
            if info.is_empty() && text.is_empty() {
                return None;
            }
            Some(MessageKind::SystemNotice { info })
        }
    }
}

/// Lenient fallback parser for `USERNOTICE` / `PRIVMSG` frames that
/// `twitch-irc` emitted as `ServerMessage::Generic` after a strict-tag parse
/// error.
fn convert_raw_irc(irc: &IRCMessage, badges: &BadgeMap) -> Option<ChatEvent> {
    match irc.command.as_str() {
        "USERNOTICE" => convert_raw_user_notice(irc, badges).map(ChatEvent::Message),
        "PRIVMSG" => convert_raw_privmsg(irc, badges).map(ChatEvent::Message),
        _ => None,
    }
}

fn convert_raw_user_notice(irc: &IRCMessage, badges: &BadgeMap) -> Option<ChatMessage> {
    let raw_event_id = tag(irc, "msg-id")?;
    let event_id = if raw_event_id == "sharedchatnotice" {
        tag(irc, "source-msg-id").unwrap_or(raw_event_id)
    } else {
        raw_event_id
    };

    let sender_name = raw_sender_name(irc);
    let text = irc.params.get(1).cloned().unwrap_or_default();
    let system_message = tag(irc, "system-msg").unwrap_or_default().to_owned();
    let kind = classify_raw_user_notice(irc, event_id, &sender_name, system_message, &text)?;

    let (badge_names, badge_urls) = parse_raw_badges(irc, badges);
    let emotes = parse_raw_emotes(irc, &text);

    Some(ChatMessage {
        id: raw_message_id(irc),
        platform: ChatPlatform::Twitch,
        author: Author {
            id: tag(irc, "user-id").unwrap_or(&sender_name).to_owned(),
            name: sender_name,
            color: parse_raw_color(irc),
            badges: badge_names,
            badge_urls,
            avatar_url: None,
        },
        emotes: unique_emotes(&emotes),
        text,
        timestamp: parse_raw_timestamp(irc),
        kind,
    })
}

fn convert_raw_privmsg(irc: &IRCMessage, badges: &BadgeMap) -> Option<ChatMessage> {
    let raw_text = irc.params.get(1)?;
    let text = raw_text
        .strip_prefix("\u{0001}ACTION ")
        .and_then(|s| s.strip_suffix('\u{0001}'))
        .unwrap_or(raw_text)
        .to_owned();

    let emotes = parse_raw_emotes(irc, &text);
    let bits = tag(irc, "bits").and_then(|v| v.parse::<u64>().ok());
    let kind = match bits {
        Some(bits) => MessageKind::Donation {
            amount: format!("{bits} bits"),
            tier: Some(chat_core::tiers::bits_tier(bits)),
            value: Some(bits as f64),
            currency: None,
        },
        None if is_emote_only(&text, &emotes) => MessageKind::EmoteOnly,
        None => MessageKind::Text,
    };

    let sender_name = raw_sender_name(irc);
    let (badge_names, badge_urls) = parse_raw_badges(irc, badges);

    Some(ChatMessage {
        id: raw_message_id(irc),
        platform: ChatPlatform::Twitch,
        author: Author {
            id: tag(irc, "user-id").unwrap_or(&sender_name).to_owned(),
            name: sender_name,
            color: parse_raw_color(irc),
            badges: badge_names,
            badge_urls,
            avatar_url: None,
        },
        emotes: unique_emotes(&emotes),
        text,
        timestamp: parse_raw_timestamp(irc),
        kind,
    })
}

fn raw_sender_name(irc: &IRCMessage) -> String {
    if let Some(name) = tag(irc, "display-name")
        .or_else(|| tag(irc, "msg-param-displayName"))
        .or_else(|| tag(irc, "login"))
        .or_else(|| tag(irc, "msg-param-login"))
    {
        return name.to_owned();
    }
    if let Some(IRCPrefix::Full { nick, .. }) = &irc.prefix {
        return nick.clone();
    }
    String::new()
}

fn raw_message_id(irc: &IRCMessage) -> String {
    if let Some(id) = tag(irc, "id").or_else(|| tag(irc, "source-id")) {
        return id.to_owned();
    }
    let ts = tag(irc, "tmi-sent-ts").unwrap_or("0");
    format!("twitch-{ts}")
}

fn parse_raw_color(irc: &IRCMessage) -> Option<String> {
    let raw = tag(irc, "color")?;
    let hex = raw.strip_prefix('#')?;
    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| raw.to_ascii_lowercase())
}

fn parse_raw_timestamp(irc: &IRCMessage) -> jiff::Timestamp {
    tag(irc, "tmi-sent-ts")
        .and_then(|v| v.parse::<i64>().ok())
        .map(timestamp)
        .unwrap_or_else(jiff::Timestamp::now)
}

fn parse_raw_badges(
    irc: &IRCMessage,
    badges: &BadgeMap,
) -> (Vec<String>, std::collections::BTreeMap<String, String>) {
    let Some(raw) = tag(irc, "badges") else {
        return (Vec::new(), std::collections::BTreeMap::new());
    };
    let pairs: Vec<(&str, &str)> = raw
        .split(',')
        .filter_map(|entry| entry.split_once('/'))
        .filter(|(name, version)| !name.is_empty() && !version.is_empty())
        .collect();
    let names = pairs.iter().map(|(name, _)| (*name).to_owned()).collect();
    let urls = badges.resolve_pairs(pairs);
    (names, urls)
}

fn parse_raw_emotes(irc: &IRCMessage, message_text: &str) -> Vec<Emote> {
    let Some(raw) = tag(irc, "emotes") else {
        return Vec::new();
    };
    let mut emotes = Vec::new();
    for entry in raw.split('/') {
        let Some((id, ranges)) = entry.split_once(':') else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        for span in ranges.split(',') {
            let Some((start, end)) = span.split_once('-') else {
                continue;
            };
            let (Ok(start), Ok(end_incl)) = (start.parse::<usize>(), end.parse::<usize>()) else {
                continue;
            };
            let Some(end) = end_incl.checked_add(1) else {
                continue;
            };
            if start >= end {
                continue;
            }
            let code: String = message_text
                .chars()
                .skip(start)
                .take(end - start)
                .collect();
            emotes.push(Emote {
                id: id.to_owned(),
                char_range: start..end,
                code,
            });
        }
    }
    emotes.sort_unstable_by_key(|e| e.char_range.start);
    emotes
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

    fn parse(raw: &str) -> ServerMessage {
        ServerMessage::try_from(IRCMessage::parse(raw).unwrap()).unwrap()
    }

    /// Converts and unwraps a `ChatEvent::Message`.
    fn convert_msg(raw: &str) -> ChatMessage {
        match convert(parse(raw), &BadgeMap::builtin()) {
            Some(ChatEvent::Message(m)) => m,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn channel_names_are_normalized_and_validated() {
        assert_eq!(
            normalize_channel(" #Your_Channel ").unwrap(),
            "your_channel"
        );
        assert!(normalize_channel("").is_err());
        assert!(normalize_channel("has space").is_err());
        assert!(normalize_channel("a_name_that_is_way_too_long_for_twitch").is_err());
    }

    #[test]
    fn privmsg_converts_and_resolves_badge_icons() {
        let out = convert_msg(
            "@badge-info=;badges=moderator/1;color=#7F5AF0;display-name=Alice;emotes=;id=abc;mod=1;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=42;user-type=mod :alice!alice@alice.tmi.twitch.tv PRIVMSG #somechannel :hello world",
        );
        assert_eq!(out.id, "abc");
        assert_eq!(out.author.name, "Alice");
        assert_eq!(out.author.color.as_deref(), Some("#7f5af0"));
        assert_eq!(out.author.badges, vec!["moderator".to_string()]);
        assert!(
            out.author
                .badge_urls
                .get("moderator")
                .is_some_and(|u| u.starts_with("https://static-cdn.jtvnw.net/badges/v1/"))
        );
        assert_eq!(out.text, "hello world");
        assert!(matches!(out.kind, MessageKind::Text));
    }

    #[test]
    fn bits_message_is_donation() {
        let out = convert_msg(
            "@badge-info=;badges=;bits=100;color=;display-name=Bob;emotes=;id=def;mod=0;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=43;user-type= :bob!bob@bob.tmi.twitch.tv PRIVMSG #somechannel :Cheer100 hey!",
        );
        // 100 bits: tier 1 (below 200), counted as a number for amount rules.
        assert!(matches!(
            out.kind,
            MessageKind::Donation {
                tier: Some(1),
                value: Some(100.0),
                currency: None,
                ..
            }
        ));
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
        let event = convert(
            parse(
                "@login=alazymeme;room-id=;target-msg-id=3c92014f-340a-4dc3-a9c9-e5cf182f4a84;tmi-sent-ts=1594561955611 :tmi.twitch.tv CLEARMSG #pajlada :bye",
            ),
            &BadgeMap::builtin(),
        );
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
            match convert(parse(raw), &BadgeMap::builtin()) {
                Some(ChatEvent::ClearUser { user_id, .. }) => assert_eq!(user_id, "148973258"),
                other => panic!("expected ClearUser, got {other:?}"),
            }
        }
    }

    #[test]
    fn clearchat_without_user_clears_all() {
        let event = convert(
            parse("@room-id=40286300;tmi-sent-ts=1594561392337 :tmi.twitch.tv CLEARCHAT #randers"),
            &BadgeMap::builtin(),
        );
        assert!(matches!(event, Some(ChatEvent::ClearAll { .. })));
    }

    const SUBGIFT: &str = "@badge-info=;badges=;color=;display-name=Gifter;emotes=;flags=;id=g1;login=gifter;mod=0;msg-id=subgift;msg-param-gift-months=1;msg-param-months=2;msg-param-recipient-display-name=Lucky;msg-param-recipient-id=99;msg-param-recipient-user-name=lucky;msg-param-sub-plan-name=Sub;msg-param-sub-plan=1000;room-id=123;subscriber=0;system-msg=Gifter\\sgifted\\sa\\ssub;tmi-sent-ts=1700000000000;user-id=46;user-type= :tmi.twitch.tv USERNOTICE #somechannel";

    #[test]
    fn direct_sub_gift_counts_one() {
        let out = convert_msg(SUBGIFT);
        assert!(matches!(out.kind, MessageKind::MembershipGift { count: 1 }));
    }

    #[test]
    fn sub_gift_from_community_gift_is_skipped() {
        let raw = SUBGIFT.replacen(
            "msg-id=subgift;",
            "msg-id=subgift;msg-param-community-gift-id=777;",
            1,
        );
        assert!(convert(parse(&raw), &BadgeMap::builtin()).is_none());
    }

    const RESUB: &str = "@badge-info=subscriber/12;badges=subscriber/12;color=;display-name=Viewer42;emotes=;flags=;id=r1;login=viewer42;mod=0;msg-id=resub;msg-param-cumulative-months=12;msg-param-should-share-streak=0;msg-param-sub-plan-name=Sub;msg-param-sub-plan=1000;room-id=123;subscriber=1;system-msg=Viewer42\\ssubscribed\\sat\\sTier\\s1.\\sThey've\\ssubscribed\\sfor\\s12\\smonths!;tmi-sent-ts=1700000000000;user-id=47;user-type= :viewer42!viewer42@viewer42.tmi.twitch.tv USERNOTICE #somechannel :time flies";

    #[test]
    fn resub_carries_the_months() {
        let out = convert_msg(RESUB);
        match out.kind {
            MessageKind::MembershipJoin { info, months } => {
                assert_eq!(
                    info,
                    "Viewer42 subscribed at Tier 1. They've subscribed for 12 months!"
                );
                assert_eq!(months, Some(12));
            }
            other => panic!("expected MembershipJoin, got {other:?}"),
        }
        assert_eq!(out.text, "time flies");
    }

    #[test]
    fn first_sub_has_no_months() {
        let raw = RESUB.replacen("msg-id=resub;", "msg-id=sub;", 1).replacen(
            "msg-param-cumulative-months=12;",
            "msg-param-cumulative-months=1;",
            1,
        );
        let out = convert_msg(&raw);
        assert!(matches!(
            out.kind,
            MessageKind::MembershipJoin { months: None, .. }
        ));
    }

    /// A watch streak, which twitch-irc doesn't know (`Unknown`): shape as
    /// seen live, names and ids replaced.
    const WATCH_STREAK: &str = "@badge-info=;badges=;color=;display-name=Viewer42;emotes=emotesv2_0000:10-20;flags=;id=w1;login=viewer42;mod=0;msg-id=viewermilestone;msg-param-category=watch-streak;msg-param-copoReward=450;msg-param-id=w0;msg-param-value=5;room-id=123;subscriber=0;system-msg=Viewer42\\swatched\\s5\\sconsecutive\\sstreams\\sand\\ssparked\\sa\\swatch\\sstreak!;tmi-sent-ts=1700000000000;user-id=47;user-type= :tmi.twitch.tv USERNOTICE #your_channel :5 yippee! :_hypeWave:";

    #[test]
    fn unknown_user_notice_becomes_a_notice() {
        let out = convert_msg(WATCH_STREAK);
        match out.kind {
            MessageKind::SystemNotice { info } => assert_eq!(
                info,
                "Viewer42 watched 5 consecutive streams and sparked a watch streak!"
            ),
            other => panic!("expected SystemNotice, got {other:?}"),
        }
        assert_eq!(out.author.name, "Viewer42");
        assert_eq!(out.text, "5 yippee! :_hypeWave:");
        assert_eq!(out.emotes.len(), 1);
        assert_eq!(out.emotes[0].code, ":_hypeWave:");
    }

    #[test]
    fn bits_badge_gets_a_readable_notice() {
        let raw = WATCH_STREAK
            .replacen(
                "msg-id=viewermilestone;",
                "msg-id=bitsbadgetier;msg-param-threshold=1000;",
                1,
            )
            .replacen(
                "Viewer42\\swatched\\s5\\sconsecutive\\sstreams\\sand\\ssparked\\sa\\swatch\\sstreak!",
                "bits\\sbadge\\stier\\snotification",
                1,
            );
        let out = convert_msg(&raw);
        assert!(matches!(
            out.kind,
            MessageKind::SystemNotice { info } if info == "Viewer42 earned the 1000 bits badge!"
        ));
    }

    #[test]
    fn mystery_gift_counts_the_wave() {
        let out = convert_msg(
            "@badge-info=;badges=;color=;display-name=Gifter;emotes=;flags=;id=g0;login=gifter;mod=0;msg-id=submysterygift;msg-param-mass-gift-count=5;msg-param-origin-id=abc;msg-param-sender-count=5;msg-param-sub-plan=1000;room-id=123;subscriber=0;system-msg=Gifter\\sis\\sgifting\\s5\\ssubs;tmi-sent-ts=1700000000000;user-id=46;user-type= :tmi.twitch.tv USERNOTICE #somechannel",
        );
        assert!(matches!(out.kind, MessageKind::MembershipGift { count: 5 }));
    }

    #[test]
    fn raid_missing_profile_image_is_recovered_from_raw_irc() {
        // twitch-irc strictly requires `msg-param-profileImageURL` on raids;
        // when Twitch omits it, twitch-irc emits `ServerMessage::Generic`.
        let raw = "@badge-info=;badges=partner/1;color=#FF69B4;display-name=Kim;emotes=;flags=;id=raid-1;login=kim;mod=0;msg-id=raid;msg-param-displayName=Kim;msg-param-login=kim;msg-param-viewerCount=42;room-id=123;subscriber=0;system-msg=42\\sraiders\\sfrom\\sKim\\shave\\sjoined!;tmi-sent-ts=1700000000000;user-id=99;user-type= :tmi.twitch.tv USERNOTICE #somechannel";
        let irc = IRCMessage::parse(raw).unwrap();
        assert!(ServerMessage::try_from(irc.clone()).is_err());

        let Some(ChatEvent::Message(out)) = convert_raw_irc(&irc, &BadgeMap::builtin()) else {
            panic!("expected recovered raid message");
        };
        assert_eq!(out.id, "raid-1");
        assert_eq!(out.author.name, "Kim");
        assert!(out.author.badge_urls.contains_key("partner"));
        assert!(matches!(
            out.kind,
            MessageKind::SystemNotice { info } if info == "42 raiders from Kim have joined!"
        ));
    }

    #[test]
    fn raid_and_watch_streak_synthesize_info_when_system_msg_is_missing() {
        let raid = IRCMessage::parse(
            "@badges=;color=;display-name=Kim;id=r2;login=kim;msg-id=raid;msg-param-displayName=Kim;msg-param-viewerCount=17;room-id=123;tmi-sent-ts=1700000000000;user-id=99 :tmi.twitch.tv USERNOTICE #somechannel",
        )
        .unwrap();
        let Some(ChatEvent::Message(out)) = convert_raw_irc(&raid, &BadgeMap::builtin()) else {
            panic!("expected raid notice");
        };
        assert!(matches!(
            out.kind,
            MessageKind::SystemNotice { info } if info == "Kim is raiding with a party of 17!"
        ));

        let streak = IRCMessage::parse(
            "@badges=subscriber/12;color=;display-name=Viewer42;emotes=25:0-4;id=w2;login=viewer42;msg-id=viewermilestone;msg-param-category=watch-streak;msg-param-value=7;room-id=123;tmi-sent-ts=1700000000000;user-id=47 :tmi.twitch.tv USERNOTICE #somechannel :Kappa nice!",
        )
        .unwrap();
        let Some(ChatEvent::Message(out)) = convert_raw_irc(&streak, &BadgeMap::builtin()) else {
            panic!("expected watch streak notice");
        };
        assert!(matches!(
            out.kind,
            MessageKind::SystemNotice { info }
                if info == "Viewer42 watched 7 consecutive streams and sparked a watch streak!"
        ));
        assert_eq!(out.text, "Kappa nice!");
        assert_eq!(out.emotes.len(), 1);
        assert_eq!(out.emotes[0].code, "Kappa");
    }

    #[test]
    fn announcement_with_empty_system_msg_gets_sender_banner() {
        let raw = "@badge-info=;badges=moderator/1;color=#7F5AF0;display-name=Alice;emotes=;flags=;id=ann-1;login=alice;mod=1;msg-id=announcement;msg-param-color=PURPLE;room-id=123;subscriber=0;system-msg=;tmi-sent-ts=1700000000000;user-id=42;user-type=mod :tmi.twitch.tv USERNOTICE #somechannel :Stream starts in 5 minutes!";
        let out = convert_msg(raw);
        assert!(matches!(
            out.kind,
            MessageKind::SystemNotice { info } if info == "Announcement from Alice"
        ));
        assert_eq!(out.text, "Stream starts in 5 minutes!");
    }

    #[test]
    fn shared_chat_raid_notice_recovers_underlying_event() {
        let raw = "@badge-info=;badges=;color=;display-name=Kim;emotes=;flags=;id=sc-1;login=kim;mod=0;msg-id=sharedchatnotice;msg-param-displayName=Kim;msg-param-viewerCount=25;room-id=123;source-msg-id=raid;subscriber=0;system-msg=;tmi-sent-ts=1700000000000;user-id=99;user-type= :tmi.twitch.tv USERNOTICE #somechannel";
        let out = convert_msg(raw);
        assert!(matches!(
            out.kind,
            MessageKind::SystemNotice { info } if info == "Kim is raiding with a party of 25!"
        ));
    }

    #[test]
    fn privmsg_with_comma_in_badge_info_is_recovered_from_raw_irc() {
        // Prediction titles containing a comma break twitch-irc's badge-info
        // parser, emitting ServerMessage::Generic.
        let raw = "@badge-info=predictions/Yes,\\sdefinitely!;badges=predictions/blue-1,vip/1;color=#00FF00;display-name=Eva;emotes=;id=p-comma;mod=0;room-id=123;subscriber=0;tmi-sent-ts=1700000000000;turbo=0;user-id=88;user-type= :eva!eva@eva.tmi.twitch.tv PRIVMSG #somechannel :let's go!";
        let irc = IRCMessage::parse(raw).unwrap();
        assert!(ServerMessage::try_from(irc.clone()).is_err());

        let Some(ChatEvent::Message(out)) = convert_raw_irc(&irc, &BadgeMap::builtin()) else {
            panic!("expected recovered privmsg");
        };
        assert_eq!(out.author.name, "Eva");
        assert_eq!(out.author.badges, vec!["predictions", "vip"]);
        assert!(out.author.badge_urls.contains_key("vip"));
        assert!(out.author.badge_urls.contains_key("predictions"));
        assert_eq!(out.text, "let's go!");
    }
}
