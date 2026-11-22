//! What an overlay leaves out: whole platforms (StreamElements or Streamlabs
//! tips, for a streamer whose tips already show in the service's own alert
//! box) or
//! kinds of message (no Super Chats in the chat box).
//!
//! Only the overlay: the JSON API and the chat reactions see everything, so
//! a tip hidden from the chat box still fires its reaction. Filtered here in
//! the server rather than in the theme's CSS: a hidden message never reaches
//! the overlay at all, so it takes no pacing slot and doesn't push visible
//! messages out of the replayed history.

use std::str::FromStr;

use chat_core::{ChatEvent, ChatMessage, ChatPlatform, MessageKind};
use tokio::sync::watch;

use crate::OverlayConfig;

/// Something an overlay can leave out: a platform or a kind of message.
/// Named as in the JSON API (a message's `platform` and `kind.type`), in
/// `config.toml` (`hide = ["streamelements"]`) and in an overlay URL
/// (`?hide=streamelements,notice`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Hide {
    Twitch,
    #[serde(rename = "youtube")]
    YouTube,
    Rplay,
    Picarto,
    #[serde(rename = "streamelements")]
    StreamElements,
    Streamlabs,
    Text,
    EmoteOnly,
    Donation,
    Special,
    Membership,
    Gift,
    Notice,
}

impl Hide {
    pub const ALL: [Hide; 13] = [
        Hide::Twitch,
        Hide::YouTube,
        Hide::Rplay,
        Hide::Picarto,
        Hide::StreamElements,
        Hide::Streamlabs,
        Hide::Text,
        Hide::EmoteOnly,
        Hide::Donation,
        Hide::Special,
        Hide::Membership,
        Hide::Gift,
        Hide::Notice,
    ];

    /// The name, as in `config.toml`, URLs and the JSON API.
    pub fn as_str(self) -> &'static str {
        match self {
            Hide::Twitch => "twitch",
            Hide::YouTube => "youtube",
            Hide::Rplay => "rplay",
            Hide::Picarto => "picarto",
            Hide::StreamElements => "streamelements",
            Hide::Streamlabs => "streamlabs",
            Hide::Text => "text",
            Hide::EmoteOnly => "emote_only",
            Hide::Donation => "donation",
            Hide::Special => "special",
            Hide::Membership => "membership",
            Hide::Gift => "gift",
            Hide::Notice => "notice",
        }
    }

    // Both `match`es list every platform and kind without a `_` arm: a new
    // one added to chat-core won't compile until it's here too.

    fn of_platform(platform: ChatPlatform) -> Hide {
        match platform {
            ChatPlatform::Twitch => Hide::Twitch,
            ChatPlatform::YouTube => Hide::YouTube,
            ChatPlatform::Rplay => Hide::Rplay,
            ChatPlatform::Picarto => Hide::Picarto,
            ChatPlatform::StreamElements => Hide::StreamElements,
            ChatPlatform::Streamlabs => Hide::Streamlabs,
        }
    }

    fn of_kind(kind: &MessageKind) -> Hide {
        match kind {
            MessageKind::Text => Hide::Text,
            MessageKind::EmoteOnly => Hide::EmoteOnly,
            MessageKind::Donation { .. } => Hide::Donation,
            MessageKind::Special { .. } => Hide::Special,
            MessageKind::MembershipJoin { .. } => Hide::Membership,
            MessageKind::MembershipGift { .. } => Hide::Gift,
            MessageKind::SystemNotice { .. } => Hide::Notice,
        }
    }

    /// The names in an overlay URL's `?hide=`, separated by commas. Unknown
    /// names are skipped: a typo in a browser source's URL shouldn't leave
    /// the stream without chat.
    pub fn parse_list(text: &str) -> Vec<Hide> {
        text.split(',')
            .filter_map(|name| name.trim().parse().ok())
            .collect()
    }
}

impl FromStr for Hide {
    type Err = ();

    fn from_str(name: &str) -> Result<Self, ()> {
        Hide::ALL
            .into_iter()
            .find(|hide| hide.as_str() == name)
            .ok_or(())
    }
}

/// Whether a message gets through: neither its platform nor its kind is
/// hidden.
fn shown(hidden: &[Hide], msg: &ChatMessage) -> bool {
    !hidden.contains(&Hide::of_platform(msg.platform))
        && !hidden.contains(&Hide::of_kind(&msg.kind))
}

/// One overlay's filter.
pub(crate) enum OverlayFilter {
    /// The app's setting, read for every message: a change applies to the
    /// next one, without a reload.
    App(watch::Receiver<OverlayConfig>),
    /// The overlay URL's `?hide=`, which wins over the app's setting (also
    /// when empty: `?hide=` shows everything in that browser source).
    Url(Vec<Hide>),
}

impl OverlayFilter {
    pub(crate) fn new(app: &watch::Receiver<OverlayConfig>, url: Option<&str>) -> Self {
        match url {
            Some(list) => OverlayFilter::Url(Hide::parse_list(list)),
            None => OverlayFilter::App(app.clone()),
        }
    }

    pub(crate) fn shows_message(&self, msg: &ChatMessage) -> bool {
        match self {
            // `borrow` holds a read lock: only for this one check.
            OverlayFilter::App(overlay) => shown(&overlay.borrow().hide, msg),
            OverlayFilter::Url(hidden) => shown(hidden, msg),
        }
    }

    /// Moderation always gets through: removing a message the overlay never
    /// got does nothing, and one it did get must still go.
    pub(crate) fn shows(&self, event: &ChatEvent) -> bool {
        match event {
            ChatEvent::Message(msg) => self.shows_message(msg),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::Author;

    fn message(platform: ChatPlatform, kind: MessageKind) -> ChatMessage {
        ChatMessage {
            id: "m1".into(),
            platform,
            author: Author {
                id: "u1".into(),
                name: "Viewer42".into(),
                color: None,
                badges: vec![],
                badge_urls: std::collections::BTreeMap::new(),
                avatar_url: None,
            },
            text: "hi".into(),
            emotes: vec![],
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            kind,
        }
    }

    fn tip() -> ChatMessage {
        message(
            ChatPlatform::StreamElements,
            MessageKind::Donation {
                amount: "10".into(),
                tier: Some(4),
                value: None,
                currency: None,
            },
        )
    }

    #[test]
    fn a_platform_or_a_kind_hides_a_message() {
        let chat = message(ChatPlatform::Twitch, MessageKind::Text);
        assert!(shown(&[], &tip()));
        assert!(!shown(&[Hide::StreamElements], &tip()));
        assert!(shown(&[Hide::StreamElements], &chat));
        // One tip service hidden, the other still shown.
        let streamlabs = ChatMessage {
            platform: ChatPlatform::Streamlabs,
            ..tip()
        };
        assert!(shown(&[Hide::StreamElements], &streamlabs));
        assert!(!shown(&[Hide::Streamlabs], &streamlabs));
        // By kind: every donation, whatever the platform.
        assert!(!shown(&[Hide::Donation], &tip()));
        assert!(shown(&[Hide::Donation], &chat));
        assert!(!shown(&[Hide::Text], &chat));
    }

    #[test]
    fn names_match_the_json_api() {
        // Platforms: chat-core's names.
        for platform in [
            ChatPlatform::Twitch,
            ChatPlatform::YouTube,
            ChatPlatform::Rplay,
            ChatPlatform::Picarto,
            ChatPlatform::StreamElements,
            ChatPlatform::Streamlabs,
        ] {
            assert_eq!(Hide::of_platform(platform).as_str(), platform.as_str());
        }
        // Kinds: the `type` in a message's JSON.
        let kinds = [
            MessageKind::Text,
            MessageKind::EmoteOnly,
            MessageKind::Donation {
                amount: "1".into(),
                tier: None,
                value: None,
                currency: None,
            },
            MessageKind::Special {
                image_url: None,
                amount: None,
                info: None,
                tier: None,
                value: None,
                currency: None,
            },
            MessageKind::MembershipJoin {
                info: "new".into(),
                months: None,
            },
            MessageKind::MembershipGift { count: 1 },
            MessageKind::SystemNotice {
                info: "raid".into(),
            },
        ];
        for kind in kinds {
            let json = serde_json::to_value(&kind).unwrap();
            assert_eq!(Hide::of_kind(&kind).as_str(), json["type"]);
        }
        // And serde (config.toml) uses the same names as `as_str` (URLs).
        for hide in Hide::ALL {
            assert_eq!(serde_json::to_value(hide).unwrap(), hide.as_str());
        }
    }

    #[test]
    fn url_lists_skip_what_they_dont_know() {
        assert_eq!(
            Hide::parse_list("streamelements, notice,,typo,donation"),
            [Hide::StreamElements, Hide::Notice, Hide::Donation]
        );
        assert!(Hide::parse_list("").is_empty());
    }

    #[test]
    fn the_url_wins_over_the_app() {
        let (app_tx, app) = watch::channel(OverlayConfig {
            hide: vec![Hide::StreamElements],
            ..OverlayConfig::default()
        });
        let from_app = OverlayFilter::new(&app, None);
        assert!(!from_app.shows_message(&tip()));
        // An empty `?hide=` shows everything in that overlay.
        assert!(OverlayFilter::new(&app, Some("")).shows_message(&tip()));
        // The app's setting changes: the next message follows it.
        app_tx.send_modify(|overlay| overlay.hide.clear());
        assert!(from_app.shows_message(&tip()));
    }

    #[test]
    fn moderation_always_gets_through() {
        let (_tx, app) = watch::channel(OverlayConfig {
            hide: Hide::ALL.to_vec(),
            ..OverlayConfig::default()
        });
        let filter = OverlayFilter::new(&app, None);
        let delete = ChatEvent::Delete {
            platform: ChatPlatform::StreamElements,
            message_id: "m1".into(),
        };
        assert!(filter.shows(&delete));
        assert!(!filter.shows(&ChatEvent::Message(tip())));
    }
}
