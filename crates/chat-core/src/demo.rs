//! Sample messages covering every `MessageKind` and platform, plus short/long
//! names and messages for previews, layout checks, and demo mode: lets you
//! style an overlay without waiting for someone to actually donate.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::{
    Activity, Author, ChatEvent, ChatMessage, ChatPlatform, ChatSource, EmoteRef, MessageKind,
    Reporter,
};

const KAPPA: &str = "https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/2.0";
const HEY_GUYS: &str = "https://static-cdn.jtvnw.net/emoticons/v2/30259/default/dark/2.0";
// Stand-in image for a YouTube custom emoji (real ones come from the
// channel's emoji export).
const YT_EMOJI: &str = "https://static-cdn.jtvnw.net/emoticons/v2/354/default/dark/2.0";
// Tiny inline SVGs so the avatar slot has varied colours offline.
const AVATAR_CORAL: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'%3E%3Crect width='1' height='1' fill='%23e05d44'/%3E%3C/svg%3E";
const AVATAR_TEAL: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'%3E%3Crect width='1' height='1' fill='%232cb67d'/%3E%3C/svg%3E";
const AVATAR_INDIGO: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'%3E%3Crect width='1' height='1' fill='%235c7cfa'/%3E%3C/svg%3E";
const AVATAR_GOLD: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'%3E%3Crect width='1' height='1' fill='%23e6a800'/%3E%3C/svg%3E";

fn emote(code: &str, url: &str) -> EmoteRef {
    EmoteRef {
        code: code.into(),
        url: url.into(),
    }
}

fn id_slug(name: &str) -> String {
    name.to_lowercase().replace(' ', "-")
}

fn twitch_badge_url(name: &str) -> Option<&'static str> {
    match name {
        "broadcaster" => {
            Some("https://static-cdn.jtvnw.net/badges/v1/5527c58c-fb7d-422d-b71b-f309dcb85cc1/2")
        }
        "moderator" => {
            Some("https://static-cdn.jtvnw.net/badges/v1/3267646d-33f0-4b17-b3df-f923a41db1d0/2")
        }
        "vip" => {
            Some("https://static-cdn.jtvnw.net/badges/v1/b817aba4-fad8-49e2-b88a-7cc744dfa6ec/2")
        }
        "subscriber" => {
            Some("https://static-cdn.jtvnw.net/badges/v1/5d9f2208-5dd8-11e7-8513-2ff4adfae661/2")
        }
        "sub-gifter" => {
            Some("https://static-cdn.jtvnw.net/badges/v1/a5ef6c17-2e5b-4d8f-9b80-2779fd722414/2")
        }
        _ => None,
    }
}

fn twitch(name: &str, color: &str, badges: &[&str]) -> Author {
    let badge_urls = badges
        .iter()
        .filter_map(|&b| twitch_badge_url(b).map(|u| (b.to_string(), u.to_string())))
        .collect();
    Author {
        id: format!("tw-{}", id_slug(name)),
        name: name.into(),
        color: Some(color.into()),
        badges: badges.iter().map(|b| b.to_string()).collect(),
        badge_urls,
        avatar_url: None,
    }
}

fn youtube(name: &str, badges: &[&str], avatar: &str) -> Author {
    Author {
        id: format!("UC-{}", id_slug(name)),
        name: name.into(),
        color: None,
        badges: badges.iter().map(|b| b.to_string()).collect(),
        badge_urls: BTreeMap::new(),
        avatar_url: Some(avatar.into()),
    }
}

fn picarto(name: &str, color: &str, badges: &[&str], avatar: &str) -> Author {
    Author {
        id: format!("pc-{}", id_slug(name)),
        name: name.into(),
        color: Some(color.into()),
        badges: badges.iter().map(|b| b.to_string()).collect(),
        badge_urls: BTreeMap::new(),
        avatar_url: Some(avatar.into()),
    }
}

fn rplay(name: &str, badges: &[&str], avatar: &str) -> Author {
    Author {
        id: format!("rp-{}", id_slug(name)),
        name: name.into(),
        color: None,
        badges: badges.iter().map(|b| b.to_string()).collect(),
        badge_urls: BTreeMap::new(),
        avatar_url: Some(avatar.into()),
    }
}

fn tipper(name: &str) -> Author {
    Author {
        id: format!("tip-{}", id_slug(name)),
        name: name.into(),
        color: None,
        badges: vec![],
        badge_urls: BTreeMap::new(),
        avatar_url: None,
    }
}

/// What every sample message's id starts with.
const ID_PREFIX: &str = "demo-";

/// Whether `msg` is one of the [`sample_messages`]. Chat reactions ignore
/// them: styling a theme with the demo running mustn't keep throwing things
/// at the streamer's model.
pub fn is_sample(msg: &ChatMessage) -> bool {
    msg.id.starts_with(ID_PREFIX)
}

/// Sample messages covering every platform, `MessageKind`, and layout edge case
/// (short and long names, short and long messages, unbroken text, badges,
/// avatars, emotes). Ids are unique within the returned list; `round` is mixed
/// into them so repeated calls (demo mode cycling) don't produce duplicate ids.
pub fn sample_messages(round: usize) -> Vec<ChatMessage> {
    let now = jiff::Timestamp::now();
    let msg = |n: usize, platform, author, text: &str, emotes, kind| ChatMessage {
        id: format!("{ID_PREFIX}{round}-{n}"),
        platform,
        author,
        text: text.into(),
        emotes,
        timestamp: now,
        kind,
    };
    use ChatPlatform::{Picarto, Rplay, StreamElements, Streamlabs, Twitch, YouTube};

    vec![
        msg(
            0,
            Twitch,
            twitch("Alice", "#7f5af0", &["moderator"]),
            "hello chat HeyGuys how is everyone doing?",
            vec![emote("HeyGuys", HEY_GUYS)],
            MessageKind::Text,
        ),
        msg(
            1,
            Picarto,
            picarto("Bo", "#e07a5f", &[], AVATAR_TEAL),
            "yo",
            vec![],
            MessageKind::Text,
        ),
        msg(
            2,
            Twitch,
            twitch(
                "Sir_Wobblesworth_Of_The_Third_Tab",
                "#c084fc",
                &["subscriber", "vip"],
            ),
            "real",
            vec![],
            MessageKind::Text,
        ),
        msg(
            3,
            Twitch,
            twitch("Cara", "#2cb67d", &[]),
            "Kappa Kappa Kappa",
            vec![emote("Kappa", KAPPA)],
            MessageKind::EmoteOnly,
        ),
        msg(
            4,
            YouTube,
            youtube("Finn", &[], AVATAR_GOLD),
            "",
            vec![],
            MessageKind::Special {
                image_url: None,
                amount: Some("€2.00".into()),
                info: Some("Super Sticker: cat doing a little dance".into()),
                tier: Some(2),
                value: Some(2.0),
                currency: Some("EUR".into()),
            },
        ),
        msg(
            5,
            Rplay,
            rplay("もち猫_Mochi", &[], AVATAR_INDIGO),
            "did the boss just walk off the cliff by itself?? 😭",
            vec![],
            MessageKind::Text,
        ),
        msg(
            6,
            Twitch,
            twitch("Eve", "#ff8906", &["subscriber"]),
            "Cheer500 take my bits and never mention the elevator incident again",
            vec![],
            MessageKind::Donation {
                amount: "500 bits".into(),
                tier: Some(3),
                value: Some(500.0),
                currency: None,
            },
        ),
        msg(
            7,
            Twitch,
            twitch("MaximilianVonSpreadsheet", "#38bdf8", &["subscriber"]),
            "WAITWAITWAITWAITWAITWAITWAITWAITWAITNOOOOOOOO",
            vec![],
            MessageKind::Text,
        ),
        msg(
            8,
            YouTube,
            youtube("Hana", &["member"], AVATAR_TEAL),
            "",
            vec![],
            MessageKind::MembershipJoin {
                info: "Welcome to the channel membership!".into(),
                months: None,
            },
        ),
        msg(
            9,
            Twitch,
            twitch("Gus", "#e53170", &["subscriber"]),
            "12 months already, still haven't learned what any of the channel points do",
            vec![],
            MessageKind::MembershipJoin {
                info: "Gus subscribed at Tier 1. They've subscribed for 12 months!".into(),
                months: Some(12),
            },
        ),
        msg(
            10,
            StreamElements,
            tipper("DefinitelyNotThreeCatsInATrenchcoat"),
            "Hydration check! Also if you beat this stage first try I will eat an entire plain tortilla on stream (I won't, I don't stream, I just want drama).",
            vec![],
            MessageKind::Donation {
                amount: "€50.00".into(),
                tier: Some(6),
                value: Some(50.0),
                currency: Some("EUR".into()),
            },
        ),
        msg(
            11,
            Twitch,
            twitch("Ivan", "#3da9fc", &["sub-gifter"]),
            "",
            vec![],
            MessageKind::MembershipGift { count: 5 },
        ),
        msg(
            12,
            YouTube,
            youtube("Jo", &["member"], AVATAR_CORAL),
            "",
            vec![],
            MessageKind::MembershipGift { count: 1 },
        ),
        msg(
            13,
            YouTube,
            youtube(
                "Professor_Overthinks_Every_Jump",
                &["moderator"],
                AVATAR_INDIGO,
            ),
            "Friendly reminder that if you press the dodge button BEFORE the giant laser hits you, it hurts significantly less. Source: watching you get vaporized six times in a row. HeyGuys",
            vec![emote("HeyGuys", HEY_GUYS)],
            MessageKind::Text,
        ),
        msg(
            14,
            Twitch,
            twitch("x", "#a78bfa", &[]),
            "Kappa",
            vec![emote("Kappa", KAPPA)],
            MessageKind::EmoteOnly,
        ),
        msg(
            15,
            Streamlabs,
            tipper("BigTipEnergy"),
            "Found two bucks in my couch cushions, go buy a really tiny hat for the mascot",
            vec![],
            MessageKind::Donation {
                amount: "$2.00".into(),
                tier: Some(2),
                value: Some(2.0),
                currency: Some("USD".into()),
            },
        ),
        msg(
            16,
            Twitch,
            twitch("soggy_pigeon_jpg", "#ff9f1c", &["broadcaster"]),
            "Okay I promise this is the last side quest before the boss fight. Yes I know I said that forty minutes ago, and yes I did just spend twelve of them reorganizing my inventory by vibe instead of damage stats.",
            vec![],
            MessageKind::Text,
        ),
        msg(
            17,
            YouTube,
            youtube("Q", &[], AVATAR_GOLD),
            "hi",
            vec![],
            MessageKind::Text,
        ),
        msg(
            18,
            YouTube,
            youtube("100_Suspiciously_Tall_Raccoons", &["member"], AVATAR_CORAL),
            "first time catching the stream live :_demoHype:",
            vec![emote(":_demoHype:", YT_EMOJI)],
            MessageKind::Text,
        ),
        msg(
            19,
            YouTube,
            youtube(
                "Lord Commander of Unread Tabs",
                &["member", "verified"],
                AVATAR_TEAL,
            ),
            "Please buy yourself a snack that isn't cold toast, you've been staring at that puzzle for twenty minutes.",
            vec![],
            MessageKind::Donation {
                amount: "€5.00".into(),
                tier: Some(3),
                value: Some(5.0),
                currency: Some("EUR".into()),
            },
        ),
        msg(
            20,
            Twitch,
            twitch("Kim", "#ef4565", &["broadcaster"]),
            "",
            vec![],
            MessageKind::SystemNotice {
                info: "Kim is raiding with a party of 42.".into(),
            },
        ),
        msg(
            21,
            Twitch,
            twitch("Lou", "#94a1b2", &["vip"]),
            "a <b>message</b> with <script>alert('html')</script> in it stays text",
            vec![],
            MessageKind::Text,
        ),
    ]
}

/// Curated subset of [`sample_messages`] for the static Overlay Preview:
/// covers chat, emotes, bits, Super Chat, resub, gifted subs, raid notice,
/// short names/messages, long unspaced/spaced names with badges, and a long
/// multi-sentence message.
pub fn showcase_messages(round: usize) -> Vec<ChatMessage> {
    sample_messages(round)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| matches!(i, 0 | 3 | 6 | 9 | 11 | 16 | 17 | 18 | 19 | 20))
        .map(|(_, msg)| msg)
        .collect()
}

/// Plays `sample_messages` in a loop, one every `interval`, and exercises
/// moderation along the way: halfway through each round the round's first
/// message is deleted, and at the end the user who posted HTML is cleared.
pub struct DemoSource {
    pub interval: Duration,
}

impl Default for DemoSource {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(1500),
        }
    }
}

impl ChatSource for DemoSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Receiving);
        for round in 0.. {
            let messages = sample_messages(round);
            let count = messages.len();
            let first_id = messages[0].id.clone();

            for (i, msg) in messages.into_iter().enumerate() {
                tokio::time::sleep(self.interval).await;
                let last_author = (i == count - 1).then(|| msg.author.id.clone());

                // Receiver gone = nobody listening anymore: stop quietly.
                if tx.send(ChatEvent::Message(msg)).await.is_err() {
                    return Ok(());
                }
                let moderation = if i == count / 2 {
                    Some(ChatEvent::Delete {
                        platform: ChatPlatform::Twitch,
                        message_id: first_id.clone(),
                    })
                } else {
                    last_author.map(|user_id| ChatEvent::ClearUser {
                        platform: ChatPlatform::Twitch,
                        user_id,
                    })
                };
                if let Some(event) = moderation {
                    tokio::time::sleep(self.interval).await;
                    if tx.send(event).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_ids_are_unique_across_rounds() {
        let mut ids = std::collections::HashSet::new();
        for round in 0..2 {
            for msg in sample_messages(round) {
                assert!(ids.insert(msg.id.clone()), "duplicate id {}", msg.id);
            }
        }
    }
}
