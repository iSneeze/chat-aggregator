//! RPlay's chat frames as chat-core events (see RPLAY.md, "IVS frame
//! format" and "RPlay's message content"). No network here: the socket
//! lives in `lib.rs`, so everything in this file is tested with frames in
//! the shape of captured ones.
//!
//! Frames are read as loose JSON (`serde_json::Value`) rather than into
//! structs: RPlay's part of them is undocumented, and picking out what we
//! need, skipping a message that lacks it, keeps one odd frame from failing
//! the whole connection.

use std::collections::{HashMap, HashSet};

use chat_core::tiers::coins_tier;
use chat_core::{Author, BoundedIdSet, ChatEvent, ChatMessage, ChatPlatform, MessageKind};
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::avatars::photo_url;
use crate::display::{anonymous_name, censor, name_color};
use crate::emotes::Emotes;

const PLATFORM: ChatPlatform = ChatPlatform::Rplay;
/// How many message ids are remembered to drop repeats (RPlay's own client
/// does the same).
const SEEN_MAX: usize = 500;
/// Why IVS disconnects a user, when it means "their messages go": a ban,
/// an automatic ban (banned words), a kick. RPlay's page hides that user's
/// earlier messages on exactly these.
const REMOVES_MESSAGES: &[&str] = &["chatBan", "chatAutoBan", "kickOut"];
/// How many kinds of frames get a "first of its kind" log line (see
/// [`Room::first_time`]): enough for every kind RPlay has, few enough that
/// a flood of odd frames can't fill the log.
const LOGGED_MAX: usize = 100;

/// What one frame means for the source.
#[derive(Debug)]
pub(crate) enum Frame {
    /// Chat or moderation, for the hub.
    Event(ChatEvent),
    /// A donation whose sender's name isn't known yet (donations don't
    /// carry it): look it up, then [`Room::finish`] it.
    Donation(Pending),
    /// The stream started (`true`) or ended.
    Live(bool),
    /// The channel's emotes changed: fetch them again.
    ReloadEmotes,
    /// A user changed something (`updatedUserData`), maybe their profile
    /// photo: check it again (see `avatars`).
    UserChanged(String),
    /// An error frame from the chat server: worth a line in the log.
    Error(String),
    /// Everything else: settings that only changed the room's state (banned
    /// words, managers, anonymous chat), repeats, and what we don't show.
    Ignored,
}

/// A donation waiting for its sender's name.
#[derive(Debug)]
pub(crate) struct Pending {
    pub user_id: String,
    /// The sender hid their name (shown anonymously if the channel has
    /// anonymous chat on).
    hidden: bool,
    /// Whether the channel had anonymous chat on when this donation arrived.
    anonymous_chat: bool,
    /// Fallback nickname from an earlier chat message, if known.
    fallback_name: Option<String>,
    /// Complete but for the author's name.
    message: ChatMessage,
}

impl Pending {
    /// Fills in the donation's sender name: `looked_up` from RPlay if that
    /// worked, else `fallback_name` from an earlier chat message.
    pub(crate) fn finish(self, looked_up: Option<String>) -> ChatMessage {
        let nickname = looked_up.or(self.fallback_name).unwrap_or_default();
        let mut message = self.message;
        message.author.name = if self.anonymous_chat && self.hidden {
            anonymous_name(&nickname)
        } else {
            nickname
        };
        message
    }
}

/// What converting needs to know about the channel's chat, kept up to date
/// by the frames themselves (RPlay announces changes as events). One per
/// connection: a new connection starts from `/live/play` again.
pub(crate) struct Room {
    /// The creator id of the channel: its streamer gets the `broadcaster`
    /// badge.
    channel: String,
    /// The channel's managers (moderators), by user id.
    managers: HashSet<String>,
    /// The streamer's banned words, starred out in chat and donations.
    banned_words: Vec<String>,
    /// Whether the channel has anonymous chat on: then senders who hide
    /// their name are shown as "AnonAru-NNNNN".
    anonymous_chat: bool,
    pub emotes: Emotes,
    /// Nicknames from the messages so far, by user id: a donation from
    /// someone who chatted needs no lookup.
    names: HashMap<String, String>,
    seen: BoundedIdSet,
    /// The kinds of frames logged already (see [`Room::first_time`]).
    logged: HashSet<String>,
}

impl Room {
    pub(crate) fn new(
        channel: String,
        managers: impl IntoIterator<Item = String>,
        banned_words: Vec<String>,
        anonymous_chat: bool,
        emotes: Emotes,
    ) -> Self {
        Self {
            channel,
            managers: managers.into_iter().collect(),
            banned_words,
            anonymous_chat,
            emotes,
            names: HashMap::new(),
            seen: BoundedIdSet::new(SEEN_MAX),
            logged: HashSet::new(),
        }
    }

    pub(crate) fn decode(&mut self, text: &str) -> Frame {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            if self.first_time("not JSON") {
                warn!("skipped an RPlay chat frame that isn't JSON");
            }
            return Frame::Ignored;
        };
        match str_field(&frame, "Type") {
            Some("MESSAGE") => self.message(&frame),
            Some("EVENT") => self.event(&frame),
            Some("ERROR") => Frame::Error(format!(
                "{} {}",
                frame.get("ErrorCode").unwrap_or(&Value::Null),
                str_field(&frame, "ErrorMessage").unwrap_or_default()
            )),
            other => {
                let kind = short(other.unwrap_or("(none)"));
                if self.first_time(&format!("type {kind}")) {
                    info!(
                        frame_type = kind,
                        fields = ?names(frame.as_object()),
                        "skipped the first RPlay chat frame of an unknown type"
                    );
                }
                Frame::Ignored
            }
        }
    }

    fn message(&mut self, frame: &Value) -> Frame {
        let Some(id) = str_field(frame, "Id").filter(|id| !id.is_empty()) else {
            return Frame::Ignored;
        };
        let sender = frame.get("Sender").unwrap_or(&Value::Null);
        let Some(user_id) = str_field(sender, "UserId").filter(|id| !id.is_empty()) else {
            return Frame::Ignored;
        };
        let sender_attributes = attributes(sender);
        // What a message carries, for learning RPlay's chat from a log (the
        // names only: the values are chat). In the first live test
        // (2026-11-15) the sender had only `isBan` and `nickname`, no
        // subscriber tier; RPlay's API isn't stable, so this stays to show
        // when that changes.
        if self.first_time("message") {
            info!(
                fields = ?names(frame.as_object()),
                attributes = ?names(Some(&attributes(frame))),
                sender_attributes = ?names(Some(&sender_attributes)),
                "first RPlay chat message"
            );
        }
        // RPlay hides messages of banned senders.
        if attribute(&sender_attributes, "isBan") == Some("true") || !self.seen.insert(id) {
            return Frame::Ignored;
        }
        let nickname = attribute(&sender_attributes, "nickname").unwrap_or_default();
        if !nickname.is_empty() {
            self.names.insert(user_id.to_owned(), nickname.to_owned());
        }

        let Some(text) = content(frame) else {
            self.note_unreadable(frame);
            return Frame::Ignored;
        };
        let text = censor(&text, &self.banned_words);
        let body = self.emotes.find(&text);
        let hidden = attribute(&attributes(frame), "isHiddenName") == Some("true");
        Frame::Event(ChatEvent::Message(ChatMessage {
            id: id.to_owned(),
            platform: PLATFORM,
            author: self.author(user_id, self.shown_name(nickname, hidden), hidden),
            text,
            emotes: body.emotes,
            timestamp: send_time(frame),
            kind: if body.emote_only {
                MessageKind::EmoteOnly
            } else {
                MessageKind::Text
            },
        }))
    }

    fn event(&mut self, frame: &Value) -> Frame {
        let Some(name) = str_field(frame, "EventName") else {
            return Frame::Ignored;
        };
        let id = str_field(frame, "Id").unwrap_or_default();
        // An event without attributes has `"Attributes": null`.
        let attrs = attributes(frame);
        if !id.is_empty() && !self.seen.insert(id) {
            return Frame::Ignored;
        }
        self.note_event(name, &attrs);
        let get = |key: &str| attribute(&attrs, key);

        match name {
            "liveOn" => Frame::Live(true),
            "liveOff" => Frame::Live(false),
            "donation" => {
                // Replays of old donations, and the end of a mission (which
                // was shown when it started), aren't new money; RPlay skips
                // them too. Test donations are shown, unlike on RPlay's
                // page: as with Streamlabs, they're how a streamer tries the
                // overlay out.
                let mission_result =
                    get("donationType").is_some_and(|kind| kind.starts_with("mission_"));
                if get("isReplay") == Some("true") || mission_result {
                    return Frame::Ignored;
                }
                self.donation(frame, &attrs)
            }
            // A mission (a paid challenge for the streamer): RPlay shows its
            // start and every boost like a donation.
            "mission" => match get("type") {
                Some("created" | "appended") => self.donation(frame, &attrs),
                _ => Frame::Ignored,
            },
            // How RPlay deletes (seen 2026-11-15: `messageId`, `deletedBy`,
            // `chatCacheVersion`).
            "messageDeleted" => delete(get("messageId")),
            // IVS's own deletion: RPlay didn't use it in the first live test,
            // kept in case its moderation ever goes through IVS directly.
            "aws:DELETE_MESSAGE" => delete(get("MessageID")),
            "aws:DISCONNECT_USER" => {
                // RPlay's server sets the reason (a word like `chatBan`, no
                // chat): which ones exist decides whose messages go.
                info!(
                    reason = short(get("Reason").unwrap_or_default()),
                    "RPlay disconnected a chatter"
                );
                match (get("UserId"), get("Reason")) {
                    (Some(user_id), Some(reason))
                        if !user_id.is_empty() && REMOVES_MESSAGES.contains(&reason) =>
                    {
                        Frame::Event(ChatEvent::ClearUser {
                            platform: PLATFORM,
                            user_id: user_id.to_owned(),
                        })
                    }
                    _ => Frame::Ignored,
                }
            }
            "RAID_INCOMING" => self.raid(frame, &attrs),
            "bannedWordsUpdate" => {
                // A comma-separated list, as RPlay's page reads it.
                self.banned_words = get("bannedWords")
                    .filter(|words| !words.is_empty())
                    .map(|words| words.split(',').map(str::to_owned).collect())
                    .unwrap_or_default();
                Frame::Ignored
            }
            "makeManager" => {
                if let Some(user_id) = get("userOid").filter(|id| !id.is_empty()) {
                    self.managers.insert(user_id.to_owned());
                }
                Frame::Ignored
            }
            "removeManager" => {
                for user_id in get("userOids").unwrap_or_default().split(',') {
                    self.managers.remove(user_id);
                }
                Frame::Ignored
            }
            "studioSetting" => {
                self.anonymous_chat = get("useAnonymousChat") == Some("true");
                Frame::Ignored
            }
            "updatedEmoticonData" => Frame::ReloadEmotes,
            "updatedUserData" => match get("userOid").filter(|id| !id.is_empty()) {
                Some(user_id) => Frame::UserChanged(user_id.to_owned()),
                None => Frame::Ignored,
            },
            _ => Frame::Ignored,
        }
    }

    /// A donation or mission as a message. Its amount is in RPlay's coins,
    /// worded as on RPlay's page ("500 coins"), without a currency (like
    /// bits); the tier is from `chat_core::tiers` (a coin counts about as
    /// much as 5 bits).
    fn donation(&self, frame: &Value, attrs: &Map<String, Value>) -> Frame {
        let get = |key: &str| attribute(attrs, key);
        let Some(user_id) = get("userId").filter(|id| !id.is_empty()) else {
            return Frame::Ignored;
        };
        let id = str_field(frame, "Id")
            .filter(|id| !id.is_empty())
            .or_else(|| get("oid"))
            .unwrap_or_default();
        let coins = get("coin")
            .and_then(|coin| coin.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let hidden = get("isHiddenName") == Some("true");
        let message = ChatMessage {
            id: id.to_owned(),
            platform: PLATFORM,
            author: self.author(user_id, String::new(), hidden),
            // No emotes: RPlay shows them in chat messages only.
            text: censor(get("content").unwrap_or_default(), &self.banned_words),
            emotes: Vec::new(),
            timestamp: send_time(frame),
            kind: MessageKind::Donation {
                amount: format!("{coins} coins"),
                tier: Some(coins_tier(coins)),
                value: Some(coins as f64),
                currency: None,
            },
        };
        let pending = Pending {
            user_id: user_id.to_owned(),
            hidden,
            anonymous_chat: self.anonymous_chat,
            fallback_name: self.names.get(user_id).cloned(),
            message,
        };
        // Someone who chatted already: no lookup needed.
        if pending.fallback_name.is_some() {
            Frame::Event(ChatEvent::Message(pending.finish(None)))
        } else {
            Frame::Donation(pending)
        }
    }

    /// Another streamer raiding this channel, worded as on RPlay's page.
    fn raid(&self, frame: &Value, attrs: &Map<String, Value>) -> Frame {
        let get = |key: &str| attribute(attrs, key);
        let Some(from) = get("fromOid").filter(|id| !id.is_empty()) else {
            return Frame::Ignored;
        };
        let nickname = get("fromNickname").unwrap_or_default();
        let viewers = get("viewerCount").unwrap_or("0");
        Frame::Event(ChatEvent::Message(ChatMessage {
            id: str_field(frame, "Id").unwrap_or_default().to_owned(),
            platform: PLATFORM,
            author: self.author(from, nickname.to_owned(), false),
            text: String::new(),
            emotes: Vec::new(),
            timestamp: send_time(frame),
            kind: MessageKind::SystemNotice {
                info: format!("{nickname} raided with {viewers} viewers!"),
            },
        }))
    }

    /// The sender as chat-core's author. `hidden`: they hid their profile
    /// (`isHiddenName`), which on RPlay hides their photo, so they get none.
    fn author(&self, user_id: &str, name: String, hidden: bool) -> Author {
        let mut badges = Vec::new();
        if user_id == self.channel {
            badges.push("broadcaster".to_owned());
        }
        if self.managers.contains(user_id) {
            badges.push("moderator".to_owned());
        }
        Author {
            id: user_id.to_owned(),
            name,
            color: Some(name_color(user_id)),
            badges,
            badge_urls: std::collections::BTreeMap::new(),
            // Where their photo would be: most users have none, so the
            // source checks before the message goes out
            // (`Avatars::verify`).
            avatar_url: if hidden { None } else { photo_url(user_id) },
        }
    }

    /// The name to show. The real nickname is in every message, but a
    /// sender who hid it in a channel with anonymous chat must not appear
    /// with it: RPlay hides it in each viewer's browser, so here it's on
    /// us.
    fn shown_name(&self, nickname: &str, hidden: bool) -> String {
        if self.anonymous_chat && hidden {
            anonymous_name(nickname)
        } else {
            nickname.to_owned()
        }
    }

    /// The first event of each kind goes into the log, with the names of
    /// its attributes (never their values: those can be chat or names).
    /// For learning RPlay's events from a tester's log; a donation's type
    /// and status are no chat, so they're logged too.
    fn note_event(&mut self, name: &str, attrs: &Map<String, Value>) {
        if name == "donation" || name == "mission" {
            info!(
                event = name,
                donation_type = attribute(attrs, "donationType").unwrap_or_default(),
                kind = attribute(attrs, "type").unwrap_or_default(),
                status = attribute(attrs, "status").unwrap_or_default(),
                test = attribute(attrs, "isTest") == Some("true"),
                "RPlay {name}"
            );
        }
        if self.first_time(&format!("event {name}")) {
            info!(
                event = short(name),
                attributes = ?names(Some(attrs)),
                "first RPlay event of this kind"
            );
        }
    }

    /// A message whose text can't be read is skipped; the log says what its
    /// content looked like (field names, never the text), once per shape:
    /// RPlay may send other things than text as chat.
    fn note_unreadable(&mut self, frame: &Value) {
        let raw = str_field(frame, "Content").unwrap_or_default();
        let shape = match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(json)) => format!("{:?}", names(Some(&json))),
            _ => "empty".to_owned(),
        };
        if self.first_time(&format!("content {shape}")) {
            warn!(
                content_fields = shape,
                "skipped an RPlay chat message whose text isn't readable"
            );
        }
    }

    /// Whether `kind` is new to the log: each kind of frame is logged the
    /// first time only, and only up to [`LOGGED_MAX`] kinds.
    fn first_time(&mut self, kind: &str) -> bool {
        self.logged.len() < LOGGED_MAX && self.logged.insert(kind.to_owned())
    }
}

fn delete(message_id: Option<&str>) -> Frame {
    match message_id.filter(|id| !id.is_empty()) {
        Some(id) => Frame::Event(ChatEvent::Delete {
            platform: PLATFORM,
            message_id: id.to_owned(),
        }),
        None => Frame::Ignored,
    }
}

fn str_field<'v>(item: &'v Value, key: &str) -> Option<&'v str> {
    item.get(key).and_then(Value::as_str)
}

/// A frame's (or sender's) `Attributes`: string → string, by IVS's rules.
/// `null` (an event without any) and anything odd count as none.
fn attributes(item: &Value) -> Map<String, Value> {
    match item.get("Attributes") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

fn attribute<'m>(attrs: &'m Map<String, Value>, key: &str) -> Option<&'m str> {
    attrs.get(key).and_then(Value::as_str)
}

/// The field names of a JSON object, for the log. A viewer's client chooses
/// a message's fields, so the names could be any text: at most 20 of them,
/// each cut short, so the log can't fill up with chat this way.
fn names(fields: Option<&Map<String, Value>>) -> Vec<&str> {
    fields
        .into_iter()
        .flat_map(|fields| fields.keys())
        .take(20)
        .map(|name| short(name))
        .collect()
}

/// RPlay's words (a frame type, an event name) for the log, cut to 40
/// characters in case one is odd.
fn short(text: &str) -> &str {
    text.char_indices()
        .nth(40)
        .map_or(text, |(end, _)| &text[..end])
}

/// The message text: `Content` holds JSON `{"m": "<text>"}` (every message
/// of the first live test did); if it isn't a JSON object, it's the text
/// itself (both as on RPlay's page). `None` for no text at all.
fn content(frame: &Value) -> Option<String> {
    let raw = str_field(frame, "Content").unwrap_or_default();
    let text = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(json)) => json.get("m").and_then(Value::as_str)?.to_owned(),
        // Text that happens to be JSON too ("123", "true") is still text.
        _ => raw.to_owned(),
    };
    (!text.is_empty()).then_some(text)
}

fn send_time(frame: &Value) -> jiff::Timestamp {
    str_field(frame, "SendTime")
        .and_then(|time| time.parse().ok())
        .unwrap_or_else(jiff::Timestamp::now)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CHANNEL: &str = "0123456789abcdef01234567";
    const VIEWER: &str = "aaaaaaaaaaaaaaaaaaaaaaaa";
    const MANAGER: &str = "bbbbbbbbbbbbbbbbbbbbbbbb";

    /// A chat frame in the captured shape (values made up).
    fn message_frame(text: &str) -> String {
        json!({
            "Type": "MESSAGE",
            "Id": "ExampleId001",
            "RequestId": "00000000-0000-0000-0000-000000000000",
            "Attributes": { "isHiddenName": "false" },
            "Content": json!({ "m": text }).to_string(),
            "SendTime": "2026-10-12T10:00:00.123456789Z",
            "Sender": {
                "UserId": VIEWER,
                "Attributes": { "isBan": "false", "nickname": "Viewer42" },
            },
        })
        .to_string()
    }

    fn event_frame(name: &str, attributes: Value) -> String {
        json!({
            "Type": "EVENT",
            "Id": format!("Event{name}"),
            "RequestId": "00000000-0000-0000-0000-000000000000",
            "EventName": name,
            "SendTime": "2026-10-12T10:00:01Z",
            "Attributes": attributes,
        })
        .to_string()
    }

    fn room() -> Room {
        let emotes = Emotes::from_lists(&[json!({
            "emoticonGroups": [{ "emoticons": [
                { "key": "_hypeWave", "s3Key": "emoticon/free/hypeWave.gif", "useLevel": 0 },
            ]}],
        })]);
        Room::new(
            CHANNEL.into(),
            [MANAGER.to_owned()],
            vec!["bad".into()],
            false,
            emotes,
        )
    }

    fn event(frame: Frame) -> ChatEvent {
        match frame {
            Frame::Event(event) => event,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    fn message(frame: Frame) -> ChatMessage {
        match event(frame) {
            ChatEvent::Message(m) => m,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn chat_message_converts() {
        let m = message(room().decode(&message_frame("hello everyone")));
        assert_eq!(m.id, "ExampleId001");
        assert_eq!(m.platform, ChatPlatform::Rplay);
        assert_eq!(m.author.id, VIEWER);
        assert_eq!(m.author.name, "Viewer42");
        assert_eq!(m.author.color.as_deref(), Some("#ffffff"));
        assert!(m.author.badges.is_empty());
        // Where the photo would be; the source checks it before sending.
        assert_eq!(
            m.author.avatar_url.as_deref(),
            Some("https://pb3.rplay.live/profilePhoto/aaaaaaaaaaaaaaaaaaaaaaaa-small")
        );
        assert_eq!(m.text, "hello everyone");
        assert!(m.emotes.is_empty());
        assert_eq!(m.timestamp.to_string(), "2026-10-12T10:00:00.123456789Z");
        assert!(matches!(m.kind, MessageKind::Text));
    }

    #[test]
    fn emotes_and_banned_words() {
        let m = message(room().decode(&message_frame(":_hypeWave:")));
        assert_eq!(m.emotes[0].code, ":_hypeWave:");
        assert!(matches!(m.kind, MessageKind::EmoteOnly));

        let m = message(room().decode(&message_frame("not B A D at all")));
        assert_eq!(m.text, "not *** at all");
    }

    #[test]
    fn content_that_isnt_json_is_the_text() {
        let frame =
            message_frame("x").replace(r#""Content":"{\"m\":\"x\"}""#, r#""Content":"plain""#);
        assert_eq!(message(room().decode(&frame)).text, "plain");
        // Text that is JSON too, but no object: still the text.
        for text in ["123", "true", "null", r#"\"quoted\""#] {
            let frame = message_frame("x").replace(
                r#""Content":"{\"m\":\"x\"}""#,
                &format!(r#""Content":"{text}""#),
            );
            let shown = message(room().decode(&frame)).text;
            assert_eq!(shown, text.replace('\\', ""), "{text}");
        }
        // JSON without a text: nothing to show.
        let empty = message_frame("x").replace(r#"{\"m\":\"x\"}"#, "{}");
        assert!(matches!(room().decode(&empty), Frame::Ignored));
    }

    #[test]
    fn badges_for_the_streamer_and_managers() {
        let mut room = room();
        let own = message_frame("hi").replace(VIEWER, CHANNEL);
        assert_eq!(message(room.decode(&own)).author.badges, ["broadcaster"]);
        let manager = message_frame("hi")
            .replace(VIEWER, MANAGER)
            .replace("ExampleId001", "ExampleId002");
        assert_eq!(message(room.decode(&manager)).author.badges, ["moderator"]);

        // Managers come and go by events.
        room.decode(&event_frame(
            "removeManager",
            json!({ "userOids": MANAGER }),
        ));
        let again = manager.replace("ExampleId002", "ExampleId003");
        assert!(message(room.decode(&again)).author.badges.is_empty());
        room.decode(&event_frame(
            "makeManager",
            json!({ "userOid": VIEWER, "nickname": "Viewer42", "email": "x", "date": "y" }),
        ));
        let viewer = message_frame("hi").replace("ExampleId001", "ExampleId004");
        assert_eq!(message(room.decode(&viewer)).author.badges, ["moderator"]);
    }

    #[test]
    fn banned_senders_and_repeats_are_dropped() {
        let mut room = room();
        let banned = message_frame("hi").replace(r#""isBan":"false""#, r#""isBan":"true""#);
        assert!(matches!(room.decode(&banned), Frame::Ignored));
        let frame = message_frame("hi").replace("ExampleId001", "ExampleId002");
        assert!(matches!(room.decode(&frame), Frame::Event(_)));
        assert!(matches!(room.decode(&frame), Frame::Ignored), "a repeat");
    }

    #[test]
    fn hidden_names_in_anonymous_chat() {
        let hidden =
            message_frame("hi").replace(r#""isHiddenName":"false""#, r#""isHiddenName":"true""#);
        // Without anonymous chat, hiding only hides the avatar.
        let mut room = room();
        let m = message(room.decode(&hidden));
        assert_eq!(m.author.name, "Viewer42");
        assert_eq!(m.author.avatar_url, None);
        // With it, the real name never shows.
        room.decode(&event_frame(
            "studioSetting",
            json!({ "useAnonymousChat": "true" }),
        ));
        let again = hidden.replace("ExampleId001", "ExampleId002");
        let m = message(room.decode(&again));
        assert_eq!(m.author.name, "AnonAru-02800");
        assert_eq!(m.author.avatar_url, None);
    }

    #[test]
    fn banned_words_change_by_event() {
        let mut room = room();
        room.decode(&event_frame(
            "bannedWordsUpdate",
            json!({ "bannedWords": "foo,bar" }),
        ));
        let m = message(room.decode(&message_frame("bad foo bar")));
        assert_eq!(m.text, "bad *** ***");
        // Another update needs another id: the same one would be a repeat.
        let cleared = event_frame("bannedWordsUpdate", json!({ "bannedWords": "" }))
            .replace("EventbannedWordsUpdate", "ExampleId099");
        room.decode(&cleared);
        let m = message(room.decode(&message_frame("foo").replace("ExampleId001", "ExampleId002")));
        assert_eq!(m.text, "foo");
    }

    fn donation_frame(attributes: Value) -> String {
        event_frame("donation", attributes)
    }

    #[test]
    fn donation_waits_for_its_senders_name() {
        let room = &mut room();
        let frame = donation_frame(json!({
            "userId": VIEWER, "coin": "500", "content": "a bad joke", "donationType": "text",
            "isHiddenName": "false", "isTest": "false", "isReplay": "false",
        }));
        let Frame::Donation(pending) = room.decode(&frame) else {
            panic!("expected a donation to finish");
        };
        assert_eq!(pending.user_id, VIEWER);
        let m = pending.finish(Some("Viewer42".into()));
        assert_eq!(m.id, "Eventdonation");
        assert_eq!(m.author.name, "Viewer42");
        assert_eq!(m.text, "a *** joke");
        match m.kind {
            MessageKind::Donation {
                amount,
                tier,
                value,
                currency,
            } => {
                assert_eq!(amount, "500 coins");
                // 500 coins: tier 5 (400+), like 2,500 bits.
                assert_eq!((tier, value, currency), (Some(5), Some(500.0), None));
            }
            other => panic!("expected a donation, got {other:?}"),
        }
    }

    #[test]
    fn a_chatters_donation_needs_no_lookup() {
        let room = &mut room();
        room.decode(&message_frame("hi"));
        let frame = donation_frame(json!({ "userId": VIEWER, "coin": "100", "content": "" }));
        assert_eq!(message(room.decode(&frame)).author.name, "Viewer42");
    }

    #[test]
    fn a_failed_lookup_falls_back_to_what_chat_said() {
        let room = &mut room();
        let frame =
            donation_frame(json!({ "userId": VIEWER, "coin": "1", "isHiddenName": "true" }));
        let Frame::Donation(pending) = room.decode(&frame) else {
            panic!("expected a donation to finish");
        };
        let m = pending.finish(None);
        assert_eq!(m.author.name, "");
        // A donor who hid their profile shows no photo either.
        assert_eq!(m.author.avatar_url, None);
    }

    #[test]
    fn replays_and_mission_results_arent_donations() {
        for attributes in [
            json!({ "userId": VIEWER, "coin": "5", "isReplay": "true" }),
            json!({ "userId": VIEWER, "coin": "5", "donationType": "mission_success" }),
            json!({ "coin": "5" }),
        ] {
            assert!(matches!(
                room().decode(&donation_frame(attributes)),
                Frame::Ignored
            ));
        }
        // Test donations are shown.
        let test = json!({ "userId": VIEWER, "coin": "5", "isTest": "true" });
        assert!(matches!(
            room().decode(&donation_frame(test)),
            Frame::Donation(_)
        ));
    }

    #[test]
    fn missions_starting_or_boosted_are_donations() {
        for kind in ["created", "appended"] {
            let frame = event_frame(
                "mission",
                json!({ "userId": VIEWER, "coin": "300", "type": kind }),
            );
            assert!(
                matches!(room().decode(&frame), Frame::Donation(_)),
                "{kind}"
            );
        }
        let done = event_frame(
            "mission",
            json!({ "userId": VIEWER, "coin": "300", "type": "success" }),
        );
        assert!(matches!(room().decode(&done), Frame::Ignored));
    }

    #[test]
    fn deletions_and_bans() {
        for frame in [
            event_frame(
                "messageDeleted",
                json!({ "messageId": "ExampleId001", "deletedBy": MANAGER }),
            ),
            event_frame(
                "aws:DELETE_MESSAGE",
                json!({ "MessageID": "ExampleId001", "Reason": "x" }),
            ),
        ] {
            match event(room().decode(&frame)) {
                ChatEvent::Delete {
                    platform,
                    message_id,
                } => {
                    assert_eq!(platform, ChatPlatform::Rplay);
                    assert_eq!(message_id, "ExampleId001");
                }
                other => panic!("expected a delete, got {other:?}"),
            }
        }
        for reason in ["chatBan", "chatAutoBan", "kickOut"] {
            let frame = event_frame(
                "aws:DISCONNECT_USER",
                json!({ "UserId": VIEWER, "Reason": reason }),
            );
            match event(room().decode(&frame)) {
                ChatEvent::ClearUser { user_id, .. } => assert_eq!(user_id, VIEWER),
                other => panic!("expected a cleared user, got {other:?}"),
            }
        }
        // An unban or a reconnect removes nothing.
        for reason in ["chatUnban", "reconnect"] {
            let frame = event_frame(
                "aws:DISCONNECT_USER",
                json!({ "UserId": VIEWER, "Reason": reason }),
            );
            assert!(matches!(room().decode(&frame), Frame::Ignored), "{reason}");
        }
    }

    #[test]
    fn incoming_raid_is_a_notice() {
        let frame = event_frame(
            "RAID_INCOMING",
            json!({ "raidId": "r1", "fromOid": MANAGER, "fromNickname": "OtherStreamer",
                    "fromMultiLangNick": "{}", "viewerCount": "42" }),
        );
        let m = message(room().decode(&frame));
        assert_eq!(m.author.name, "OtherStreamer");
        match m.kind {
            MessageKind::SystemNotice { info } => {
                assert_eq!(info, "OtherStreamer raided with 42 viewers!")
            }
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn stream_state_and_emote_changes() {
        // `liveOff` came with `"Attributes": null` in the capture.
        assert!(matches!(
            room().decode(&event_frame("liveOn", Value::Null)),
            Frame::Live(true)
        ));
        assert!(matches!(
            room().decode(&event_frame("liveOff", Value::Null)),
            Frame::Live(false)
        ));
        let changed = event_frame("updatedEmoticonData", json!({ "creatorOid": CHANNEL }));
        assert!(matches!(room().decode(&changed), Frame::ReloadEmotes));
    }

    #[test]
    fn errors_and_everything_else() {
        let error = r#"{"Type":"ERROR","Id":"x","RequestId":"y","ErrorCode":429,"ErrorMessage":"slow down"}"#;
        match room().decode(error) {
            Frame::Error(text) => assert_eq!(text, "429 slow down"),
            other => panic!("expected an error, got {other:?}"),
        }
        match room().decode(&event_frame(
            "updatedUserData",
            json!({ "userOid": VIEWER }),
        )) {
            Frame::UserChanged(user_id) => assert_eq!(user_id, VIEWER),
            other => panic!("expected a changed user, got {other:?}"),
        }
        for frame in [
            event_frame("updatedUserData", json!({})),
            event_frame("subscriberOnlyChanged", json!({ "enabled": "true" })),
            event_frame(
                "MODERATION_ACTION",
                json!({ "type": "ban", "targetUserOid": VIEWER }),
            ),
            r#"{"Type":"SOMETHING_NEW"}"#.to_owned(),
            "not json".to_owned(),
        ] {
            assert!(matches!(room().decode(&frame), Frame::Ignored), "{frame}");
        }
    }

    #[test]
    fn odd_messages_are_skipped_not_fatal() {
        let room = &mut room();
        let no_id = message_frame("hi").replace(r#""Id":"ExampleId001","#, "");
        assert!(matches!(room.decode(&no_id), Frame::Ignored));
        let no_sender = message_frame("hi").replace(VIEWER, "");
        assert!(matches!(room.decode(&no_sender), Frame::Ignored));
        // A missing nickname or time: shown anyway.
        let bare = json!({ "Type": "MESSAGE", "Id": "x", "Content": "{\"m\":\"hi\"}",
                           "Sender": { "UserId": VIEWER, "Attributes": null } });
        let m = message(room.decode(&bare.to_string()));
        assert_eq!((m.author.name.as_str(), m.text.as_str()), ("", "hi"));
    }

    #[test]
    fn the_log_gets_each_kind_once_and_names_cut_short() {
        let mut room = room();
        assert!(room.first_time("event liveOn"));
        assert!(!room.first_time("event liveOn"), "once per kind");
        // A flood of odd kinds stops at the limit.
        for n in 0..LOGGED_MAX * 2 {
            room.first_time(&format!("type odd{n}"));
        }
        assert_eq!(room.logged.len(), LOGGED_MAX);
        assert!(!room.first_time("event never seen"));

        // Field names a viewer's client could choose: few, and short.
        let long = "x".repeat(100);
        let mut fields = Map::new();
        for n in 0..30 {
            fields.insert(format!("{n:02}{long}"), Value::Null);
        }
        let logged = names(Some(&fields));
        assert_eq!(logged.len(), 20);
        assert!(logged.iter().all(|name| name.chars().count() == 40));
        assert_eq!(short("視聴者"), "視聴者");
        assert_eq!(names(None), Vec::<&str>::new());
    }
}
