//! Streamlabs' Socket API frames as chat-core events (see STREAMLABS.md,
//! "Protocol" and "Events seen").
//!
//! Two layers in every text frame: Engine.IO 3 (the first character: `0`
//! open, `3` the answer to our ping, `4` a socket.io packet) and socket.io
//! 2 inside (`40` connected, `42[…]` an event, `44…` an error). Read as
//! loose JSON (`serde_json::Value`) rather than structs, like the
//! StreamElements frames: Streamlabs' fields differ from its own docs, and
//! a missing or oddly typed field should cost that field, not the donation.

use std::time::Duration;

use chat_core::tiers::{Amount, TipSettings, display_amount};
use chat_core::{Author, ChatMessage, ChatPlatform, MessageKind};
use serde_json::Value;

/// Engine.IO 3's own defaults, until the open packet says otherwise
/// (Streamlabs sends 10 s and 20 s).
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(25);
pub(crate) const PING_TIMEOUT: Duration = Duration::from_secs(60);

/// What one text frame means for the source.
#[derive(Debug)]
pub(crate) enum Frame {
    /// `0{…}`: the session is open; ping every `interval`, and give up on
    /// the connection after `interval + timeout` without a sound.
    Open {
        interval: Duration,
        timeout: Duration,
    },
    /// `40`: the token was accepted.
    Connected,
    /// `44"…"`: the token was refused (seen: "Authentication error"). Only
    /// the user can fix it.
    Refused(String),
    /// Donations, as messages: `message` is a list in every event.
    Donations(Vec<ChatMessage>),
    /// Anything else: pongs, settings, statistics, previews, the
    /// platforms' relayed events.
    Ignored,
}

/// `tips`: the streamer's tip settings, for a donation's tier.
pub(crate) fn decode(text: &str, tips: &TipSettings) -> Frame {
    let mut chars = text.chars();
    match chars.next() {
        Some('0') => {
            let open: Value = serde_json::from_str(chars.as_str()).unwrap_or_default();
            let ms = |key: &str, default: Duration| {
                open[key].as_u64().map_or(default, Duration::from_millis)
            };
            Frame::Open {
                interval: ms("pingInterval", PING_INTERVAL),
                timeout: ms("pingTimeout", PING_TIMEOUT),
            }
        }
        Some('4') => socket_io(chars.as_str(), tips),
        _ => Frame::Ignored,
    }
}

/// A socket.io packet: what follows Engine.IO's `4`.
fn socket_io(packet: &str, tips: &TipSettings) -> Frame {
    let mut chars = packet.chars();
    match chars.next() {
        Some('0') => Frame::Connected,
        Some('4') => {
            let reason = chars.as_str();
            Frame::Refused(
                serde_json::from_str::<String>(reason).unwrap_or_else(|_| reason.to_owned()),
            )
        }
        Some('2') => {
            let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(chars.as_str()) else {
                return Frame::Ignored;
            };
            match (parts.first().and_then(Value::as_str), parts.get(1)) {
                (Some("event"), Some(event)) => donations(event, tips),
                // `donationPageSettings` and anything else that isn't an
                // "event".
                _ => Frame::Ignored,
            }
        }
        _ => Frame::Ignored,
    }
}

fn donations(event: &Value, tips: &TipSettings) -> Frame {
    // The tip page's own donations. The docs give them `for: "streamlabs"`,
    // their sample has no `for` at all: both are taken. Every other `for`
    // (`twitch_account`, `youtube_account`) is a platform's event relayed,
    // which the platform sources show already.
    let ours = event["for"].is_null() || event["for"] == "streamlabs";
    if event["type"] != "donation" || !ours {
        return Frame::Ignored;
    }
    let Some(entries) = event["message"].as_array() else {
        return Frame::Ignored;
    };
    let messages: Vec<_> = entries
        .iter()
        .filter_map(|entry| donation(entry, tips))
        .collect();
    if messages.is_empty() {
        Frame::Ignored
    } else {
        Frame::Donations(messages)
    }
}

/// One donation. `None` for the alert box editor's previews (fake alerts
/// it plays every ~18 s while it's open: never real) and for one without a
/// usable amount or id. Tests from the dashboard's buttons (`isTest`) are
/// kept on purpose (decided 2026-11-07): that's how a streamer checks the
/// overlay and the reactions before going live.
fn donation(entry: &Value, tips: &TipSettings) -> Option<ChatMessage> {
    if entry["isPreview"] == true {
        return None;
    }
    let id = text(&entry["_id"])?;
    // A number in the capture, a string ("13.37") in Streamlabs' docs.
    let amount = entry["amount"]
        .as_f64()
        .or_else(|| entry["amount"].as_str()?.trim().parse().ok())
        .and_then(Amount::of)?;
    let currency = text(&entry["currency"]).map(|code| code.to_ascii_uppercase());
    // `from` is the donor. `name` is too in the docs' sample, but in test
    // donations it's the streamer's own name, so it's only the fallback.
    let name = text(&entry["from"])
        .or_else(|| text(&entry["name"]))
        .unwrap_or_else(|| "Anonymous".to_owned());
    // No usable account id: `from_user_id` is 1 for every test donor. The
    // name typed on the tip page is all there is, as with StreamElements.
    let author_id = name.to_lowercase();

    Some(ChatMessage {
        id,
        platform: ChatPlatform::Streamlabs,
        author: Author {
            id: author_id,
            name,
            color: None,
            badges: Vec::new(),
            avatar_url: None,
        },
        text: text(&entry["message"]).unwrap_or_default(),
        emotes: Vec::new(),
        // Donation events carry no time of their own.
        timestamp: jiff::Timestamp::now(),
        kind: MessageKind::Donation {
            // Our own formatting rather than Streamlabs' `formattedAmount`:
            // StreamElements tips look the same then.
            amount: display_amount(amount, currency.as_deref()),
            tier: Some(tier(amount, currency.as_deref(), tips)),
            value: Some(amount.as_f64()),
            currency,
        },
    })
}

/// The tier from the streamer's tip tiers, which are in their tip currency.
/// A donation in another currency is still a donation, but how much it's
/// worth would take a conversion: tier 1 (decided 2026-11-07). While no tip
/// currency is set, the tiers are taken as they are.
fn tier(amount: Amount, currency: Option<&str>, tips: &TipSettings) -> u32 {
    match (tips.currency(), currency) {
        (Some(ours), Some(theirs)) if ours != theirs => 1,
        _ => tips.tier(amount),
    }
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
    use serde_json::json;

    /// A donation event frame as Streamlabs sends it (shape from a capture,
    /// values made up; the editor's settings left out of previews).
    fn event(kind: &str, service: Option<&str>, entry: Value) -> String {
        let mut event = json!({
            "type": kind,
            "message": [entry],
            "event_id": "evt_00000000000000000000000000000001",
        });
        if let Some(service) = service {
            event["for"] = json!(service);
        }
        format!("42{}", json!(["event", event]))
    }

    fn entry(amount: Value) -> Value {
        json!({
            "name": "your_channel",
            "message": "Hello!",
            "from": "Viewer42",
            "to": { "name": "your_channel" },
            "from_user_id": 1,
            "amount": amount,
            "formattedAmount": "€9.00",
            "currency": "EUR",
            "recurring": false,
            "isTest": false,
            "isPreview": false,
            "unsavedSettings": [],
            "freeze": false,
            "_id": "00000000000000000000000000000001",
            "priority": 10,
        })
    }

    fn decoded(frame: &str, tips: &TipSettings) -> Vec<ChatMessage> {
        match decode(frame, tips) {
            Frame::Donations(messages) => messages,
            other => panic!("expected donations, got {other:?}"),
        }
    }

    fn donation_of(entry: Value, tips: &TipSettings) -> ChatMessage {
        let mut messages = decoded(&event("donation", Some("streamlabs"), entry), tips);
        assert_eq!(messages.len(), 1);
        messages.remove(0)
    }

    #[test]
    fn a_donation_becomes_a_donation_message() {
        let message = donation_of(entry(json!(9)), &TipSettings::default());
        assert_eq!(message.id, "00000000000000000000000000000001");
        assert_eq!(message.platform, ChatPlatform::Streamlabs);
        assert_eq!(
            message.author.name, "Viewer42",
            "the donor, not the streamer"
        );
        assert_eq!(message.author.id, "viewer42");
        assert_eq!(message.text, "Hello!");
        let MessageKind::Donation {
            amount,
            tier,
            value,
            currency,
        } = message.kind
        else {
            panic!("not a donation: {:?}", message.kind);
        };
        assert_eq!(amount, "€9.00");
        assert_eq!(tier, Some(3), "YouTube's steps: 5 ≤ 9 < 10");
        assert_eq!(value, Some(9.0));
        assert_eq!(currency.as_deref(), Some("EUR"));
    }

    #[test]
    fn test_donations_count_and_previews_never_do() {
        let tips = TipSettings::default();
        let mut test = entry(json!(12));
        test["isTest"] = json!(true);
        assert_eq!(donation_of(test, &tips).text, "Hello!");

        let mut preview = entry(json!(78));
        preview["isPreview"] = json!(true);
        preview["previewAction"] = json!("initial");
        let frame = decode(&event("donation", Some("streamlabs"), preview), &tips);
        assert!(matches!(frame, Frame::Ignored), "{frame:?}");
    }

    #[test]
    fn amounts_come_as_numbers_or_text() {
        let tips = TipSettings::default();
        let message = donation_of(entry(json!("13.37")), &tips);
        assert!(matches!(
            message.kind,
            MessageKind::Donation {
                value: Some(13.37),
                tier: Some(4),
                ..
            }
        ));
        for amount in [json!(null), json!("lots"), json!(-5)] {
            let frame = decode(
                &event("donation", Some("streamlabs"), entry(amount.clone())),
                &tips,
            );
            assert!(matches!(frame, Frame::Ignored), "{amount}: {frame:?}");
        }
    }

    #[test]
    fn tiers_follow_the_tip_settings_in_their_currency() {
        let tips = TipSettings {
            currency: "EUR".into(),
            tiers: [3, 6, 9, 69, 100, 420].map(Amount::whole),
        };
        let tier_of = |currency: &str| {
            let mut donation = entry(json!(69));
            donation["currency"] = json!(currency);
            match donation_of(donation, &tips).kind {
                MessageKind::Donation { tier, .. } => tier,
                other => panic!("not a donation: {other:?}"),
            }
        };
        assert_eq!(tier_of("EUR"), Some(5));
        // Another currency: a donation of unknown worth, the lowest tier.
        assert_eq!(tier_of("USD"), Some(1));
        // No tip currency set: the tiers as they are.
        let unset = TipSettings {
            currency: String::new(),
            ..tips.clone()
        };
        assert!(matches!(
            donation_of(entry(json!(69)), &unset).kind,
            MessageKind::Donation { tier: Some(5), .. }
        ));
    }

    #[test]
    fn missing_names_and_messages() {
        let mut donation = entry(json!(1));
        donation["from"] = json!(null);
        donation["message"] = json!("  ");
        let message = donation_of(donation, &TipSettings::default());
        // Without `from`, `name` is the best there is.
        assert_eq!(message.author.name, "your_channel");
        assert_eq!(message.text, "");

        let mut nameless = entry(json!(1));
        nameless["from"] = json!(null);
        nameless["name"] = json!(null);
        assert_eq!(
            donation_of(nameless, &TipSettings::default()).author.name,
            "Anonymous"
        );
    }

    #[test]
    fn only_the_tip_pages_donations_are_kept() {
        let tips = TipSettings::default();
        // The docs' sample: no `for`.
        assert_eq!(
            decoded(&event("donation", None, entry(json!(5))), &tips).len(),
            1
        );
        for (kind, service) in [
            ("donation", Some("twitch_account")),
            ("superchat", Some("youtube_account")),
            ("follow", Some("youtube_account")),
            ("bits", Some("twitch_account")),
            ("alertPlaying", None),
            ("streamlabels", None),
            ("alertBoxSettingsUpdate", None),
        ] {
            let frame = decode(&event(kind, service, entry(json!(5))), &tips);
            assert!(matches!(frame, Frame::Ignored), "{kind}: {frame:?}");
        }
        let settings = r#"42["donationPageSettings",{"amount_minimum":"1"}]"#;
        assert!(matches!(decode(settings, &tips), Frame::Ignored));
    }

    #[test]
    fn every_donation_in_an_event_counts() {
        let mut second = entry(json!(2));
        second["_id"] = json!("00000000000000000000000000000002");
        let frame = format!(
            "42{}",
            json!(["event", { "type": "donation", "for": "streamlabs",
                              "message": [entry(json!(1)), second] }])
        );
        let ids: Vec<_> = decoded(&frame, &TipSettings::default())
            .into_iter()
            .map(|message| message.id)
            .collect();
        assert_eq!(
            ids,
            [
                "00000000000000000000000000000001",
                "00000000000000000000000000000002"
            ]
        );
    }

    #[test]
    fn connection_frames() {
        let tips = TipSettings::default();
        let open = r#"0{"sid":"x","upgrades":[],"pingInterval":10000,"pingTimeout":20000,"maxPayload":1000000}"#;
        assert!(matches!(
            decode(open, &tips),
            Frame::Open { interval, timeout }
                if interval == Duration::from_secs(10) && timeout == Duration::from_secs(20)
        ));
        // An open packet without timing: Engine.IO 3's defaults.
        assert!(matches!(
            decode("0{}", &tips),
            Frame::Open {
                interval: PING_INTERVAL,
                timeout: PING_TIMEOUT
            }
        ));
        assert!(matches!(decode("40", &tips), Frame::Connected));
        assert!(matches!(
            decode(r#"44"Authentication error""#, &tips),
            Frame::Refused(ref why) if why == "Authentication error"
        ));
        for other in ["3", "", "6", "41", "42not json", "not a packet"] {
            assert!(matches!(decode(other, &tips), Frame::Ignored), "{other:?}");
        }
    }
}
