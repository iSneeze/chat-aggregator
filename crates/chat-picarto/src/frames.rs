//! Picarto's chat frames as chat-core events (see PICARTO.md, "Frame
//! format"). Pure functions: the socket lives in `lib.rs`, so everything
//! here is tested with frames in the shape of captured ones.
//!
//! Frames are read as loose JSON (`serde_json::Value`) rather than into
//! structs: Picarto's format is undocumented, keys are one or two letters,
//! and the same key is sometimes a string, sometimes a number. Picking out
//! what we need, and skipping a message that lacks it, keeps one odd frame
//! from failing the whole connection.

use chat_core::{Author, ChatEvent, ChatMessage, ChatPlatform, MessageKind};
use serde::Deserialize;
use serde_json::Value;

use crate::emotes::{ChannelEmote, SiteEmotes, image_url};

const PLATFORM: ChatPlatform = ChatPlatform::Picarto;

/// What one frame means for the source.
#[derive(Debug)]
pub(crate) enum Frame {
    /// Chat or moderation, for the hub.
    Events(Vec<ChatEvent>),
    /// The channel's state (`stream`): whether the configured channel is
    /// live.
    Stream { online: bool },
    /// Everything else: joins and leaves, the answers to our pings, and the
    /// frame types the source doesn't handle yet (tips, subscriptions,
    /// raids, polls, ...; none of them seen in a capture so far).
    Ignored,
}

/// What converting needs besides the frame itself.
pub(crate) struct Context<'a> {
    /// The configured channel's id: its streamer gets the `broadcaster`
    /// badge. Not the `s` flag: in a multistream the partners have it too.
    pub owner_id: &'a str,
    pub emotes: &'a SiteEmotes,
}

pub(crate) fn decode(text: &str, cx: &Context) -> Frame {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return Frame::Ignored;
    };
    // Most frames name their type `t` and their payload `m`; a few
    // (`stream`) spell them out, and the site accepts both everywhere.
    let kind = frame
        .get("t")
        .or_else(|| frame.get("type"))
        .and_then(Value::as_str);
    let payload = frame
        .get("m")
        .or_else(|| frame.get("messages"))
        .unwrap_or(&Value::Null);

    // In a multistream, chat and moderation of every channel in it arrive
    // here, and all of it is kept: it's one shared chat on Picarto too.
    match kind {
        Some("c") => Frame::Events(
            items(payload)
                .filter_map(|m| message(m, cx))
                .map(ChatEvent::Message)
                .collect(),
        ),
        // Seen only in the site's code so far: `{id, m}`, with `m` the text
        // the site shows instead. We remove the message, as for the other
        // platforms.
        Some("rm") => Frame::Events(
            items(payload)
                .filter_map(|m| {
                    Some(ChatEvent::Delete {
                        platform: PLATFORM,
                        message_id: id_field(m, "id")?,
                    })
                })
                .collect(),
        ),
        // All of a user's messages: `{u}`, the user id messages carry.
        Some("cm") => Frame::Events(
            items(payload)
                .filter_map(|m| {
                    Some(ChatEvent::ClearUser {
                        platform: PLATFORM,
                        user_id: id_field(m, "u")?,
                    })
                })
                .collect(),
        ),
        Some("cr") => Frame::Events(vec![ChatEvent::ClearAll { platform: PLATFORM }]),
        // The top level is the channel we connected to; a multistream's
        // other channels are in its `streams`.
        Some("stream") => match payload.get("online").and_then(Value::as_bool) {
            Some(online) => Frame::Stream { online },
            None => Frame::Ignored,
        },
        _ => Frame::Ignored,
    }
}

/// A payload is an array of items or a single one.
fn items(payload: &Value) -> impl Iterator<Item = &Value> {
    match payload {
        Value::Array(items) => items.as_slice(),
        Value::Object(_) => std::slice::from_ref(payload),
        _ => &[],
    }
    .iter()
}

/// An id, which frames carry as a string or a number. `None` when missing
/// or empty.
fn id_field(item: &Value, key: &str) -> Option<String> {
    match item.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn str_field<'v>(item: &'v Value, key: &str) -> Option<&'v str> {
    item.get(key).and_then(Value::as_str)
}

fn message(m: &Value, cx: &Context) -> Option<ChatMessage> {
    // The items of a `c` frame are chat messages, `t: "c"` as well. Should
    // another kind ever turn up among them, it's not plain chat.
    if str_field(m, "t").is_some_and(|t| t != "c") {
        return None;
    }
    let id = id_field(m, "id")?;
    let user_id = id_field(m, "u")?;

    // `e` lists the message's channel emotes, with `null` for codes the
    // server couldn't resolve. `&Value` is a serde deserializer itself, so
    // this reads it without copying the JSON. If it doesn't parse, those
    // codes stay text.
    let channel_emotes = m
        .get("e")
        .and_then(|e| Vec::<Option<ChannelEmote>>::deserialize(e).ok())
        .unwrap_or_default();
    let body = cx
        .emotes
        .convert(str_field(m, "m").unwrap_or_default(), &channel_emotes);

    let mut badges = Vec::new();
    if user_id == cx.owner_id {
        badges.push("broadcaster".to_owned());
    }
    // A subscriber of the channel the message was written in (`c`).
    if m.get("b").and_then(Value::as_bool) == Some(true) {
        badges.push("subscriber".to_owned());
    }

    Some(ChatMessage {
        id,
        platform: PLATFORM,
        author: Author {
            id: user_id,
            name: str_field(m, "n").unwrap_or_default().to_owned(),
            color: str_field(m, "k").and_then(color),
            badges,
            avatar_url: str_field(m, "i")
                .filter(|path| !path.is_empty())
                .map(image_url),
        },
        text: body.text,
        emotes: body.emotes,
        timestamp: m
            .get("d")
            .and_then(Value::as_i64)
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or_else(chrono::Utc::now),
        kind: if body.emote_only {
            MessageKind::EmoteOnly
        } else {
            MessageKind::Text
        },
    })
}

/// The name colour, 6 hex digits without `#`, as `#rrggbb`. Anything else
/// is dropped: it ends up in the overlay's CSS.
fn color(hex: &str) -> Option<String> {
    (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("#{}", hex.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chat frame in the captured shape (values replaced).
    const CHAT: &str = r#"{"t":"c","m":[{"t":"c","c":"1234567","u":"7654321","n":"Viewer42","rn":"your_channel","i":"usericons/default.png","y":"F","m":"hello everyone","id":"00000000-0000-0000-0000-000000000000","d":1791800000000,"cb":true,"k":"AABBCC","rc":"ddeeff"}]}"#;

    /// One with a channel emote, a subscriber and an uploaded avatar.
    const CHAT_EMOTE: &str = r#"{"t":"c","m":[{"t":"c","c":"1234567","u":"7654321","n":"Viewer42","rn":"your_channel","i":"ptvimages/7/76/7654321/avatars/a.jpg","y":"P","m":":cyour_channel-hypeWave:","id":"00000000-0000-0000-0000-000000000001","d":1791800000000,"b":true,"cb":{"c":1000,"i":"badges/1000.png"},"k":"aabbcc","e":[{"a":"cyour_channel-hypeWave","u":"ptvimages/1/12/1234567/emoticons/hypeWave.gif"}],"rc":"ddeeff"}]}"#;

    fn decode_with(text: &str, owner_id: &str) -> Frame {
        let emotes = SiteEmotes::default();
        decode(
            text,
            &Context {
                owner_id,
                emotes: &emotes,
            },
        )
    }

    fn events(text: &str) -> Vec<ChatEvent> {
        match decode_with(text, "1234567") {
            Frame::Events(events) => events,
            other => panic!("expected events, got {other:?}"),
        }
    }

    fn one_message(text: &str, owner_id: &str) -> ChatMessage {
        match decode_with(text, owner_id) {
            Frame::Events(mut events) if events.len() == 1 => match events.remove(0) {
                ChatEvent::Message(m) => m,
                other => panic!("expected a message, got {other:?}"),
            },
            other => panic!("expected one event, got {other:?}"),
        }
    }

    #[test]
    fn chat_message_converts() {
        let m = one_message(CHAT, "1234567");
        assert_eq!(m.id, "00000000-0000-0000-0000-000000000000");
        assert_eq!(m.platform, ChatPlatform::Picarto);
        assert_eq!(m.author.id, "7654321");
        assert_eq!(m.author.name, "Viewer42");
        assert_eq!(m.author.color.as_deref(), Some("#aabbcc"));
        assert_eq!(
            m.author.avatar_url.as_deref(),
            Some("https://images.picarto.tv/usericons/default.png")
        );
        assert!(m.author.badges.is_empty());
        assert_eq!(m.text, "hello everyone");
        assert!(m.emotes.is_empty());
        assert_eq!(m.timestamp.timestamp_millis(), 1791800000000);
        assert!(matches!(m.kind, MessageKind::Text));
    }

    #[test]
    fn channel_emotes_and_subscribers() {
        let m = one_message(CHAT_EMOTE, "1234567");
        assert_eq!(m.author.badges, ["subscriber"]);
        assert_eq!(m.emotes.len(), 1);
        assert_eq!(m.emotes[0].code, ":cyour_channel-hypeWave:");
        assert_eq!(
            m.emotes[0].url,
            "https://images.picarto.tv/ptvimages/1/12/1234567/emoticons/hypeWave.gif"
        );
        assert!(matches!(m.kind, MessageKind::EmoteOnly));
    }

    #[test]
    fn the_configured_channels_streamer_is_the_broadcaster() {
        // The streamer chatting in their own channel: `u` is the channel id.
        let own = CHAT.replace(r#""u":"7654321""#, r#""u":"1234567","s":true,"o":true"#);
        assert_eq!(one_message(&own, "1234567").author.badges, ["broadcaster"]);
        // A multistream partner has `s` too, but isn't who the overlay is
        // for.
        let partner = CHAT.replace(
            r#""c":"1234567","u":"7654321""#,
            r#""c":"2222222","u":"2222222","s":true,"o":true"#,
        );
        assert!(one_message(&partner, "1234567").author.badges.is_empty());
    }

    #[test]
    fn multistream_partners_chat_is_kept() {
        // Written in another channel of the multistream (`c`, `rn`): the
        // chat is shared, so it's shown like any other message.
        let other = CHAT.replace(r#""c":"1234567""#, r#""c":"2222222""#);
        assert_eq!(one_message(&other, "1234567").text, "hello everyone");
    }

    #[test]
    fn ids_may_be_numbers() {
        let m = one_message(&CHAT.replace(r#""u":"7654321""#, r#""u":7654321"#), "x");
        assert_eq!(m.author.id, "7654321");
    }

    #[test]
    fn a_message_without_id_or_sender_is_skipped() {
        let no_id = CHAT.replace(r#""id":"00000000-0000-0000-0000-000000000000","#, "");
        assert!(events(&no_id).is_empty());
        let no_user = CHAT.replace(r#""u":"7654321","#, "");
        assert!(events(&no_user).is_empty());
    }

    #[test]
    fn odd_fields_are_dropped_not_fatal() {
        let odd = CHAT
            .replace(r#""k":"AABBCC""#, r#""k":"red;}""#)
            .replace(r#""d":1791800000000"#, r#""d":"soon""#)
            .replace(r#""i":"usericons/default.png""#, r#""i":"""#)
            .replace(r#""m":"hello everyone""#, r#""m":"hi","e":"nonsense""#);
        let m = one_message(&odd, "x");
        assert_eq!(m.author.color, None);
        assert_eq!(m.author.avatar_url, None);
        assert_eq!(m.text, "hi");
    }

    #[test]
    fn full_avatar_urls_stay_as_they_are() {
        let full = CHAT.replace(
            r#""i":"usericons/default.png""#,
            r#""i":"https://images.picarto.tv/usericons/default.png""#,
        );
        assert_eq!(
            one_message(&full, "x").author.avatar_url.as_deref(),
            Some("https://images.picarto.tv/usericons/default.png")
        );
    }

    #[test]
    fn deleted_message() {
        // `m` as an object and as an array: the site takes both.
        for frame in [
            r#"{"t":"rm","m":{"id":"00000000-0000-0000-0000-000000000001","m":"message deleted"}}"#,
            r#"{"t":"rm","m":[{"id":"00000000-0000-0000-0000-000000000001"}]}"#,
        ] {
            match events(frame).as_slice() {
                [
                    ChatEvent::Delete {
                        platform: ChatPlatform::Picarto,
                        message_id,
                    },
                ] => assert_eq!(message_id, "00000000-0000-0000-0000-000000000001"),
                other => panic!("expected a delete, got {other:?}"),
            }
        }
    }

    #[test]
    fn cleared_user_and_chat() {
        match events(r#"{"t":"cm","m":{"u":7654321}}"#).as_slice() {
            [ChatEvent::ClearUser { user_id, .. }] => assert_eq!(user_id, "7654321"),
            other => panic!("expected a cleared user, got {other:?}"),
        }
        assert!(matches!(
            events(r#"{"t":"cr"}"#).as_slice(),
            [ChatEvent::ClearAll {
                platform: ChatPlatform::Picarto
            }]
        ));
    }

    #[test]
    fn stream_frame_tells_whether_the_channel_is_live() {
        let frame = r#"{"type":"stream","messages":{"id":1234567,"name":"your_channel","online":false,"multistream":false,"viewers":0,"streams":[]}}"#;
        assert!(matches!(
            decode_with(frame, "1234567"),
            Frame::Stream { online: false }
        ));
    }

    #[test]
    fn everything_else_is_ignored() {
        for frame in [
            r#"{"success":true,"code":"PONG"}"#,
            r#"{"t":"un","m":{"c":"1234567","u":"7654321","n":"Viewer42"}}"#,
            r#"{"t":"ur","m":{"u":"0","n":""}}"#,
            r#"{"t":"ct","m":[{"t":"ct","u":"7654321","n":"Viewer42","x":100}]}"#,
            "not json",
        ] {
            assert!(
                matches!(decode_with(frame, "1234567"), Frame::Ignored),
                "{frame}"
            );
        }
    }
}
