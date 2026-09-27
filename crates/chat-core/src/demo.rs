//! Sample messages covering every `MessageKind` on both platforms, for
//! previews and demo mode: lets you style an overlay without waiting for
//! someone to actually donate.

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
// Tiny inline SVG so the avatar slot has something to show offline.
const AVATAR: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1 1'%3E%3Crect width='1' height='1' fill='%23e05d44'/%3E%3C/svg%3E";

fn emote(code: &str, url: &str) -> EmoteRef {
    EmoteRef {
        code: code.into(),
        url: url.into(),
    }
}

fn twitch(name: &str, color: &str, badges: &[&str]) -> Author {
    Author {
        id: format!("tw-{}", name.to_lowercase()),
        name: name.into(),
        color: Some(color.into()),
        badges: badges.iter().map(|b| b.to_string()).collect(),
        avatar_url: None,
    }
}

fn youtube(name: &str, badges: &[&str]) -> Author {
    Author {
        id: format!("UC-{}", name.to_lowercase()),
        name: name.into(),
        color: None,
        badges: badges.iter().map(|b| b.to_string()).collect(),
        avatar_url: Some(AVATAR.into()),
    }
}

/// One message of every kind, alternating platforms. Ids are unique within
/// the returned list; `round` is mixed into them so repeated calls (demo
/// mode cycling) don't produce duplicate ids.
pub fn sample_messages(round: usize) -> Vec<ChatMessage> {
    let now = chrono::Utc::now();
    let msg = |n: usize, platform, author, text: &str, emotes, kind| ChatMessage {
        id: format!("demo-{round}-{n}"),
        platform,
        author,
        text: text.into(),
        emotes,
        timestamp: now,
        kind,
    };
    use ChatPlatform::{Twitch, YouTube};

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
            YouTube,
            youtube("Bob", &["member"]),
            "first time catching the stream live :_demoHype:",
            vec![emote(":_demoHype:", YT_EMOJI)],
            MessageKind::Text,
        ),
        msg(
            2,
            Twitch,
            twitch("Cara", "#2cb67d", &[]),
            "Kappa Kappa Kappa",
            vec![emote("Kappa", KAPPA)],
            MessageKind::EmoteOnly,
        ),
        msg(
            3,
            YouTube,
            youtube("Dan", &["member", "verified"]),
            "keep up the great work!",
            vec![],
            MessageKind::Donation {
                amount: "€5.00".into(),
            },
        ),
        msg(
            4,
            Twitch,
            twitch("Eve", "#ff8906", &["subscriber"]),
            "Cheer100 take my bits",
            vec![],
            MessageKind::Donation {
                amount: "100 bits".into(),
            },
        ),
        msg(
            5,
            YouTube,
            youtube("Finn", &[]),
            "",
            vec![],
            MessageKind::Special {
                image_url: None,
                amount: Some("€2.00".into()),
                info: Some("Super Sticker: cat doing a little dance".into()),
            },
        ),
        msg(
            6,
            Twitch,
            twitch("Gus", "#e53170", &["subscriber"]),
            "12 months already, time flies",
            vec![],
            MessageKind::MembershipJoin {
                info: "Gus subscribed at Tier 1. They've subscribed for 12 months!".into(),
                months: Some(12),
            },
        ),
        msg(
            7,
            YouTube,
            youtube("Hana", &["member"]),
            "",
            vec![],
            MessageKind::MembershipJoin {
                info: "Welcome to the channel membership!".into(),
                months: None,
            },
        ),
        msg(
            8,
            Twitch,
            twitch("Ivan", "#3da9fc", &["sub-gifter"]),
            "",
            vec![],
            MessageKind::MembershipGift { count: 5 },
        ),
        msg(
            9,
            YouTube,
            youtube("Jo", &["member"]),
            "",
            vec![],
            MessageKind::MembershipGift { count: 1 },
        ),
        msg(
            10,
            Twitch,
            twitch("Kim", "#ef4565", &["broadcaster"]),
            "",
            vec![],
            MessageKind::SystemNotice {
                info: "Kim is raiding with a party of 42.".into(),
            },
        ),
        msg(
            11,
            Twitch,
            twitch("Lou", "#94a1b2", &["vip"]),
            "a <b>message</b> with <script>alert('html')</script> in it stays text",
            vec![],
            MessageKind::Text,
        ),
    ]
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

    #[test]
    fn demo_moderation_targets_twitch_messages() {
        // DemoSource deletes/clears on Twitch; the targets must be Twitch too.
        let messages = sample_messages(0);
        assert_eq!(messages[0].platform, ChatPlatform::Twitch);
        assert_eq!(messages.last().unwrap().platform, ChatPlatform::Twitch);
    }

    #[tokio::test]
    async fn demo_source_deletes_its_first_message() {
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(
            DemoSource {
                interval: Duration::from_millis(1),
            }
            .run(tx, Reporter::detached()),
        );

        let first_id = sample_messages(0)[0].id.clone();
        loop {
            match rx.recv().await.expect("demo keeps running") {
                ChatEvent::Delete { message_id, .. } => {
                    assert_eq!(message_id, first_id);
                    break;
                }
                ChatEvent::Message(_) => continue,
                other => panic!("expected a delete first, got {other:?}"),
            }
        }
    }
}
