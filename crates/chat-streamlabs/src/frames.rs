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

use chat_core::tiers::{Amount, Tip};
use chat_core::{Author, ChatPlatform};
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
    /// Donations, as Streamlabs sent them (the source applies the
    /// streamer's tip settings): `message` is a list in every event.
    Donations(Vec<Tip>),
    /// Anything else: pongs, settings, statistics, previews, the
    /// platforms' relayed events.
    Ignored,
}

pub(crate) fn decode(text: &str) -> Frame {
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
        Some('4') => socket_io(chars.as_str()),
        _ => Frame::Ignored,
    }
}

/// A socket.io packet: what follows Engine.IO's `4`.
fn socket_io(packet: &str) -> Frame {
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
                (Some("event"), Some(event)) => donations(event),
                // `donationPageSettings` and anything else that isn't an
                // "event".
                _ => Frame::Ignored,
            }
        }
        _ => Frame::Ignored,
    }
}

fn donations(event: &Value) -> Frame {
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
    let donations: Vec<_> = entries.iter().filter_map(donation).collect();
    if donations.is_empty() {
        Frame::Ignored
    } else {
        Frame::Donations(donations)
    }
}

/// One donation. `None` for the alert box editor's previews (fake alerts
/// it plays every ~18 s while it's open: never real) and for one without a
/// usable amount or id. Tests from the dashboard's buttons (`isTest`) are
/// kept on purpose (decided 2026-11-07): that's how a streamer checks the
/// overlay and the reactions before going live.
fn donation(entry: &Value) -> Option<Tip> {
    if entry["isPreview"] == true {
        return None;
    }
    let id = text(&entry["_id"])?;
    // A number in the capture, a string ("13.37") in Streamlabs' docs.
    // Shown with our own formatting rather than Streamlabs'
    // `formattedAmount`, so its tips look like StreamElements'.
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

    Some(Tip {
        id,
        platform: ChatPlatform::Streamlabs,
        author: Author {
            id: author_id,
            name,
            color: None,
            badges: Vec::new(),
            badge_urls: std::collections::BTreeMap::new(),
            avatar_url: None,
        },
        text: text(&entry["message"]).unwrap_or_default(),
        // Donation events carry no time of their own.
        timestamp: jiff::Timestamp::now(),
        amount,
        // Unlike StreamElements, Streamlabs says each tip's currency: one
        // in another than the tip currency gets tier 1
        // (`TipSettings::donation`).
        currency,
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

    fn decoded(frame: &str) -> Vec<Tip> {
        match decode(frame) {
            Frame::Donations(donations) => donations,
            other => panic!("expected donations, got {other:?}"),
        }
    }

    fn donation_of(entry: Value) -> Tip {
        let mut donations = decoded(&event("donation", Some("streamlabs"), entry));
        assert_eq!(donations.len(), 1);
        donations.remove(0)
    }

    #[test]
    fn a_donation_is_read() {
        let mut raw = entry(json!(9));
        raw["currency"] = json!("eur");
        let tip = donation_of(raw);
        assert_eq!(tip.id, "00000000000000000000000000000001");
        assert_eq!(tip.platform, ChatPlatform::Streamlabs);
        assert_eq!(tip.author.name, "Viewer42", "the donor, not the streamer");
        assert_eq!(tip.author.id, "viewer42");
        assert_eq!(tip.text, "Hello!");
        assert_eq!(tip.amount, Amount::whole(9));
        assert_eq!(tip.currency.as_deref(), Some("EUR"));
    }

    #[test]
    fn test_donations_count_and_previews_never_do() {
        let mut test = entry(json!(12));
        test["isTest"] = json!(true);
        assert_eq!(donation_of(test).text, "Hello!");

        let mut preview = entry(json!(78));
        preview["isPreview"] = json!(true);
        preview["previewAction"] = json!("initial");
        let frame = decode(&event("donation", Some("streamlabs"), preview));
        assert!(matches!(frame, Frame::Ignored), "{frame:?}");

        // The docs' sample without `for` is kept; relayed platform donations are ignored.
        assert_eq!(decoded(&event("donation", None, entry(json!(5)))).len(), 1);
        let relayed = decode(&event("donation", Some("twitch_account"), entry(json!(5))));
        assert!(matches!(relayed, Frame::Ignored));
    }

    #[test]
    fn missing_names_and_messages() {
        let mut donation = entry(json!(1));
        donation["from"] = json!(null);
        donation["message"] = json!("  ");
        let tip = donation_of(donation);
        // Without `from`, `name` is the best there is.
        assert_eq!(tip.author.name, "your_channel");
        assert_eq!(tip.text, "");

        let mut nameless = entry(json!(1));
        nameless["from"] = json!(null);
        nameless["name"] = json!(null);
        assert_eq!(donation_of(nameless).author.name, "Anonymous");
    }

    #[test]
    fn every_donation_in_an_event_counts() {
        let mut second = entry(json!("13.37"));
        second["_id"] = json!("00000000000000000000000000000002");
        let frame = format!(
            "42{}",
            json!(["event", { "type": "donation", "for": "streamlabs",
                              "message": [entry(json!(1)), second] }])
        );
        let tips = decoded(&frame);
        let ids: Vec<_> = tips.iter().map(|tip| tip.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "00000000000000000000000000000001",
                "00000000000000000000000000000002"
            ]
        );
        assert_eq!(tips[1].amount, Amount::of(13.37).unwrap());
    }

    #[test]
    fn connection_frames() {
        let open = r#"0{"sid":"x","upgrades":[],"pingInterval":10000,"pingTimeout":20000,"maxPayload":1000000}"#;
        assert!(matches!(
            decode(open),
            Frame::Open { interval, timeout }
                if interval == Duration::from_secs(10) && timeout == Duration::from_secs(20)
        ));
        // An open packet without timing: Engine.IO 3's defaults.
        assert!(matches!(
            decode("0{}"),
            Frame::Open {
                interval: PING_INTERVAL,
                timeout: PING_TIMEOUT
            }
        ));
        assert!(matches!(decode("40"), Frame::Connected));
        assert!(matches!(
            decode(r#"44"Authentication error""#),
            Frame::Refused(ref why) if why == "Authentication error"
        ));
        assert!(matches!(decode("not a packet"), Frame::Ignored));
    }
}
