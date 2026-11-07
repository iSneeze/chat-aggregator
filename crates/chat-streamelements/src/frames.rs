//! Astro's frames as chat-core events (see STREAMELEMENTS.md, "Protocol"
//! and "A tip in `channel.activities`").
//!
//! Read as loose JSON (`serde_json::Value`) rather than structs, like the
//! Picarto frames: the event fields aren't documented, and a missing or
//! oddly typed field should cost that field, not the whole tip.

use chat_core::tiers::{Amount, TipSettings};
use chat_core::{Author, ChatMessage, ChatPlatform, MessageKind};
use serde_json::{Value, json};

/// The activity feed: the only topic the Overlay Token may read that has
/// tips in it.
const TOPIC: &str = "channel.activities";

/// What one text frame means for the source.
#[derive(Debug)]
pub(crate) enum Frame {
    /// The first frame of a connection: time to subscribe.
    Welcome,
    /// The subscription was accepted.
    Subscribed,
    /// The token was refused (wrong token, or not this account's): only the
    /// user can fix it. Astro's reason.
    Refused(String),
    /// The subscription failed for another reason (rate limit, ...): worth
    /// another try later.
    Failed(String),
    /// A tip, as a donation message. Boxed: a message is far bigger than the
    /// other variants.
    Tip(Box<ChatMessage>),
    /// Astro is about to shut this server down: connect again.
    Reconnect,
    /// Anything else: other activities, unknown frames.
    Ignored,
}

/// The subscribe command for the activity feed of `account`.
pub(crate) fn subscribe(account: &str, token: &str) -> String {
    json!({
        "type": "subscribe",
        "nonce": TOPIC,
        "data": {
            "topic": TOPIC,
            "room": account,
            "token": token,
            // The Overlay Token; Astro calls it an API key.
            "token_type": "apikey",
        },
    })
    .to_string()
}

/// `tips`: the streamer's tip settings, for a tip's tier and currency.
pub(crate) fn decode(text: &str, tips: &TipSettings) -> Frame {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return Frame::Ignored;
    };
    match frame["type"].as_str() {
        Some("welcome") => Frame::Welcome,
        Some("reconnect") => Frame::Reconnect,
        Some("response") => {
            let reason = frame["data"]["message"]
                .as_str()
                .unwrap_or("no reason given")
                .to_owned();
            match frame["error"].as_str().filter(|error| !error.is_empty()) {
                None => Frame::Subscribed,
                // Seen for a made-up token ("unauthenticated") and for a
                // real one on a topic it may not read ("permission_denied").
                Some("err_unauthorized") => Frame::Refused(reason),
                Some(error) => Frame::Failed(format!("{error}: {reason}")),
            }
        }
        Some("message") if frame["topic"] == TOPIC => match tip(&frame["data"], tips) {
            Some(message) => Frame::Tip(Box::new(message)),
            None => Frame::Ignored,
        },
        _ => Frame::Ignored,
    }
}

/// A tip from the activity feed. `None` for every other activity: those are
/// the platforms' own events (Super Chats, bits, subs, follows), which the
/// platform sources show already, or StreamElements extras (merch) not
/// handled yet.
fn tip(activity: &Value, tips: &TipSettings) -> Option<ChatMessage> {
    if activity["type"] != "tip" {
        return None;
    }
    let data = &activity["data"];
    let id = text(&activity["_id"]).or_else(|| text(&activity["activityId"]))?;
    // A number in every tip seen so far; Streamlabs is known to send
    // amounts as text now and then, so that's accepted too.
    let amount = data["amount"]
        .as_f64()
        .or_else(|| data["amount"].as_str()?.trim().parse().ok())
        .and_then(Amount::of)?;
    let username = text(&data["username"]);
    let name = text(&data["displayName"])
        .or_else(|| username.clone())
        .unwrap_or_else(|| "Anonymous".to_owned());
    // `providerId` is no use as the author's id: in emulated tips it's the
    // same for every tipper (STREAMELEMENTS.md). The name typed on the tip
    // page is all there is.
    let author_id = username.unwrap_or_else(|| name.to_lowercase());
    let timestamp = text(&activity["createdAt"])
        .and_then(|time| time.parse().ok())
        .unwrap_or_else(jiff::Timestamp::now);

    Some(ChatMessage {
        id,
        platform: ChatPlatform::StreamElements,
        author: Author {
            id: author_id,
            name,
            color: None,
            badges: Vec::new(),
            avatar_url: text(&data["avatar"]).filter(|url| url.starts_with("https://")),
        },
        text: text(&data["message"]).unwrap_or_default(),
        emotes: Vec::new(),
        timestamp,
        // StreamElements has no tiers of its own, and doesn't say the
        // currency (STREAMELEMENTS.md): tips are always in the one currency
        // of the streamer's tip page, so the streamer's settings say both.
        kind: MessageKind::Donation {
            amount: tips.display(amount),
            tier: Some(tips.tier(amount)),
            value: Some(amount.as_f64()),
            currency: tips.currency().map(str::to_owned),
        },
    })
}

/// A non-empty string field.
fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An activity frame as Astro sends it (shape from a capture, values
    /// made up).
    fn activity(kind: &str, data: Value) -> String {
        json!({
            "id": "0f8fad5b-d9cb-469f-a165-70867728950e",
            "ts": "2026-11-07T15:03:27.872Z",
            "type": "message",
            "topic": "channel.activities",
            "room": "0123456789abcdef01234567",
            "data": {
                "_id": "aaaaaaaaaaaaaaaaaaaaaaaa",
                "activityId": "aaaaaaaaaaaaaaaaaaaaaaaa",
                "channel": "0123456789abcdef01234567",
                "provider": "youtube",
                "type": kind,
                "createdAt": "2026-11-07T15:03:27.532Z",
                "updatedAt": "2026-11-07T15:03:27.600Z",
                "isMock": true,
                "sessionEventsCount": 1,
                "data": data,
            },
        })
        .to_string()
    }

    fn decoded_tip(data: Value) -> ChatMessage {
        tip_with(data, &TipSettings::default())
    }

    fn tip_with(data: Value, tips: &TipSettings) -> ChatMessage {
        match decode(&activity("tip", data), tips) {
            Frame::Tip(message) => *message,
            other => panic!("expected a tip, got {other:?}"),
        }
    }

    #[test]
    fn a_tip_becomes_a_donation() {
        let message = decoded_tip(json!({
            "username": "viewer42",
            "displayName": "Viewer42",
            "providerId": "UC0000000000000000000000",
            "amount": 10,
            "message": "Hello!",
            "gifted": false,
            "avatar": "https://cdn.streamelements.com/static/default-avatar.png",
        }));
        assert_eq!(message.id, "aaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(message.platform, ChatPlatform::StreamElements);
        assert_eq!(message.author.id, "viewer42");
        assert_eq!(message.author.name, "Viewer42");
        assert_eq!(
            message.author.avatar_url.as_deref(),
            Some("https://cdn.streamelements.com/static/default-avatar.png")
        );
        assert_eq!(message.text, "Hello!");
        assert_eq!(
            message.timestamp,
            "2026-11-07T15:03:27.532Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
        );
        let MessageKind::Donation {
            amount,
            tier,
            value,
            currency,
        } = message.kind
        else {
            panic!("not a donation: {:?}", message.kind);
        };
        assert_eq!(amount, "10");
        assert_eq!(tier, Some(4));
        assert_eq!(value, Some(10.0));
        assert_eq!(currency, None, "no currency set");
    }

    #[test]
    fn the_streamers_settings_give_tier_and_currency() {
        let tips = TipSettings {
            currency: "EUR".into(),
            tiers: [3, 6, 9, 69, 100, 420].map(Amount::whole),
        };
        let message = tip_with(json!({ "amount": 69 }), &tips);
        assert!(matches!(
            message.kind,
            MessageKind::Donation { ref amount, tier: Some(5), ref currency, .. }
                if amount == "€69.00" && currency.as_deref() == Some("EUR")
        ));
    }

    #[test]
    fn a_tip_without_message_or_names_still_counts() {
        let message = decoded_tip(json!({ "amount": "2.5" }));
        assert_eq!(message.author.name, "Anonymous");
        assert_eq!(message.text, "");
        assert!(message.author.avatar_url.is_none());
        assert!(matches!(
            message.kind,
            MessageKind::Donation { ref amount, tier: Some(2), value: Some(2.5), .. } if amount == "2.50"
        ));
    }

    #[test]
    fn a_tip_without_an_amount_is_dropped() {
        for amount in [json!(null), json!("lots"), json!(-5)] {
            let frame = decode(
                &activity("tip", json!({ "amount": amount })),
                &TipSettings::default(),
            );
            assert!(matches!(frame, Frame::Ignored), "{amount}: {frame:?}");
        }
    }

    #[test]
    fn only_tips_are_kept() {
        // The platforms' own events, relayed: the platform sources show
        // them already. Merch isn't handled yet.
        for kind in [
            "superchat",
            "sponsor",
            "subscriber",
            "follower",
            "cheer",
            "merch",
        ] {
            let frame = decode(
                &activity(kind, json!({ "username": "viewer42", "amount": 10 })),
                &TipSettings::default(),
            );
            assert!(matches!(frame, Frame::Ignored), "{kind}: {frame:?}");
        }
    }

    #[test]
    fn avatars_must_be_https() {
        let message = decoded_tip(json!({ "amount": 1, "avatar": "javascript:alert(1)" }));
        assert!(message.author.avatar_url.is_none());
    }

    #[test]
    fn subscription_answers() {
        let ok = r#"{"id":"1","ts":"2026-11-07T15:01:52Z","type":"response","nonce":"channel.activities","data":{"message":"successfully subscribed to topic","room":"0123456789abcdef01234567","topic":"channel.activities"}}"#;
        assert!(matches!(
            decode(ok, &TipSettings::default()),
            Frame::Subscribed
        ));

        let refused = r#"{"id":"2","ts":"2026-11-07T15:01:52Z","type":"response","nonce":"channel.activities","error":"err_unauthorized","data":{"message":"permission_denied: user is not authorized for resource"}}"#;
        assert!(
            matches!(decode(refused, &TipSettings::default()), Frame::Refused(ref why) if why.starts_with("permission_denied"))
        );

        let limited = r#"{"id":"3","ts":"2026-11-07T15:01:52Z","type":"response","error":"rate_limit_exceeded","data":{"message":"slow down"}}"#;
        assert!(
            matches!(decode(limited, &TipSettings::default()), Frame::Failed(ref why) if why.starts_with("rate_limit_exceeded"))
        );
    }

    #[test]
    fn connection_frames() {
        let welcome = r#"{"id":"1","ts":"2026-11-07T15:01:52Z","type":"welcome","data":{"client_id":"x","message":"hi"}}"#;
        assert!(matches!(
            decode(welcome, &TipSettings::default()),
            Frame::Welcome
        ));
        let reconnect = r#"{"type":"reconnect","data":{"reconnect_token":"x"}}"#;
        assert!(matches!(
            decode(reconnect, &TipSettings::default()),
            Frame::Reconnect
        ));
        assert!(matches!(
            decode("not json", &TipSettings::default()),
            Frame::Ignored
        ));
        // Another topic (only one is subscribed, but still).
        let other = activity("tip", json!({ "amount": 1 }))
            .replace("channel.activities", "channel.stream.status");
        assert!(matches!(
            decode(&other, &TipSettings::default()),
            Frame::Ignored
        ));
    }

    #[test]
    fn the_subscription_uses_the_overlay_token() {
        let command: Value =
            serde_json::from_str(&subscribe("0123456789abcdef01234567", "t0ken")).unwrap();
        assert_eq!(command["type"], "subscribe");
        assert_eq!(command["data"]["topic"], "channel.activities");
        assert_eq!(command["data"]["room"], "0123456789abcdef01234567");
        assert_eq!(command["data"]["token"], "t0ken");
        assert_eq!(command["data"]["token_type"], "apikey");
    }
}
