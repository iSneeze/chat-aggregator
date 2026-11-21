//! RPlay chat source: reads the streamer's own chat on RPlay (rplay.live).
//! What's known about RPlay's chat is in RPLAY.md, the maintainer's
//! research notes (kept outside the public repository);
//! `examples/raw_rplay.rs` captures raw chat traffic.
//!
//! RPlay has no public chat API. Its chat runs on Amazon IVS Chat, which
//! the source joins the way RPlay's own OBS widgets do: with the
//! streamer's **toolkey**, the key in every widget link from their RPlay
//! studio, traded for a session that acts for the streamer. Never
//! anonymously: a channel that asks viewers to log in (even just to follow
//! for free) gives anonymous visitors no chat, and the toolkey gets past
//! every access rule. The key can do much more than read chat (moderation,
//! stream settings), so it's kept in the secret store and never logged;
//! the source only reads.
//!
//! Shows chat messages with their emotes, donations and missions (in
//! coins), deletions, bans and incoming raids, with RPlay's name colours,
//! anonymous names, profile pictures (for chatters who have one) and the
//! streamer's banned words. Not yet: subscriber badges and which emotes a
//! sender's tier allows (messages carry no tier, seen 2026-11-15: both need
//! a lookup per chatter).

mod api;
mod avatars;
#[cfg(feature = "capture")]
mod capture;
mod display;
mod emotes;
mod frames;

#[cfg(feature = "capture")]
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::{Activity, ChatEvent, ChatSource, Reporter};
use chat_websocket::Ended;
use chat_websocket::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};
use reqwest::{StatusCode, Url};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{Instrument, debug, info, info_span, warn};

use api::{ChatToken, HttpError, Play, Session};
use avatars::Avatars;
use frames::{Frame, Room};

/// How often to ping the chat server. IVS answers every ping, so anything
/// heard proves the connection alive.
const PING_EVERY: Duration = Duration::from_secs(30);
/// This much silence means the connection is dead without either side
/// having noticed (a sleeping laptop, a dropped route).
const SILENT_FOR: Duration = Duration::from_secs(75);
/// IVS ends a chat session after 60 minutes. The source ends it a little
/// earlier itself and connects again right away, rather than wait to be
/// cut off and then for the reconnect pause.
const RENEW_BEFORE_END: Duration = Duration::from_secs(30);
/// When RPlay doesn't say when the session ends: renew after this long.
const RENEW_AFTER: Duration = Duration::from_secs(55 * 60);
/// How often to ask again while RPlay gives no chat token because the
/// channel is offline.
const OFFLINE_RETRY: Duration = Duration::from_secs(60);
/// The longest name for a source: it's shown in the app's source list.
const LABEL_MAX: usize = 40;
/// Where to tell the streamer to find a widget link.
const WHERE_THE_LINK_IS: &str = "in your RPlay studio, open Dashboard Widget, click Generate \
     Widget URLs, then Show next to Stream Chat, and copy that link";

pub struct RplaySource {
    /// The streamer's name for this channel (passed [`normalize_label`]):
    /// a streamer with two personas has two channels.
    pub label: String,
    /// The channel's toolkey (passed [`toolkey_from`]; the engine keeps it
    /// in the secret store under the label, never in `config.toml`).
    pub toolkey: String,
    /// Test builds: the folder for the raw chat capture
    /// ([`RplaySource::capture_into`]).
    #[cfg(feature = "capture")]
    capture_dir: Option<PathBuf>,
}

impl RplaySource {
    // A constructor rather than a struct literal: in test builds the
    // struct has one more (private) field, and code that builds it doesn't
    // need to know.
    pub fn new(label: String, toolkey: String) -> Self {
        Self {
            label,
            toolkey,
            #[cfg(feature = "capture")]
            capture_dir: None,
        }
    }

    /// Test builds: every frame from the chat server also goes into
    /// `rplay-capture.jsonl` in `dir`, chat included (see `capture`).
    #[cfg(feature = "capture")]
    pub fn capture_into(mut self, dir: PathBuf) -> Self {
        self.capture_dir = Some(dir);
        self
    }
}

/// The label as typed, checked: it names the channel in the app and keys
/// its toolkey, so an empty one (or one with line breaks) won't do.
pub fn normalize_label(input: &str) -> Result<String, String> {
    let label = input.trim();
    if label.is_empty() {
        return Err("give this RPlay channel a name, like \"Main\"".into());
    }
    if label.chars().count() > LABEL_MAX {
        return Err(format!("the name is longer than {LABEL_MAX} characters"));
    }
    if label.chars().any(char::is_control) {
        return Err("the name can't contain line breaks or tabs".into());
    }
    Ok(label.to_owned())
}

/// The toolkey from what the streamer pasted: a widget link from their
/// RPlay studio (every one has the key as `?toolkey=…`: the Stream Chat
/// dock's, the chat box's, ...), or the key itself. A link without a key
/// (the channel's page, say) is refused with where to find one.
pub fn toolkey_from(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(format!("paste a widget link: {WHERE_THE_LINK_IS}"));
    }
    let lowercase = input.to_ascii_lowercase();
    let is_link = input.contains("://")
        || lowercase.starts_with("rplay.live")
        || lowercase.starts_with("www.rplay.live");
    if !is_link {
        return if is_key(input) {
            Ok(input.to_owned())
        } else {
            Err(format!(
                "that's neither a widget link nor a key: {WHERE_THE_LINK_IS}"
            ))
        };
    }
    let link = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let url = Url::parse(&link).map_err(|_| format!("that isn't a link: {WHERE_THE_LINK_IS}"))?;
    let rplay = url
        .host_str()
        .is_some_and(|host| host == "rplay.live" || host.ends_with(".rplay.live"));
    if !rplay {
        return Err(format!("that's not an RPlay link: {WHERE_THE_LINK_IS}"));
    }
    url.query_pairs()
        .find(|(name, _)| name == "toolkey")
        .map(|(_, key)| key.into_owned())
        .filter(|key| is_key(key))
        .ok_or_else(|| format!("this link has no widget key in it: {WHERE_THE_LINK_IS}"))
}

/// What a key can look like, as far as we can tell: something an HTTP
/// header can carry, without spaces.
fn is_key(text: &str) -> bool {
    (1..=1024).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_graphic())
}

impl ChatSource for RplaySource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        let http = api::http_client()?;
        // The label in every log line from here on (as `rplay{label=…}`):
        // with two channels, the log says whose.
        let span = info_span!("rplay", label = %self.label);
        chat_websocket::keep_connected("RPlay", &activity, || self.read(&http, &tx, &activity))
            .instrument(span)
            .await
    }
}

impl RplaySource {
    /// One connection: from the login until it ends. Each one starts from
    /// scratch (a new live-tool session, the channel's state, its emotes),
    /// which also picks up whatever changed in between.
    async fn read(
        &self,
        http: &reqwest::Client,
        tx: &mpsc::Sender<ChatEvent>,
        activity: &Reporter,
    ) -> anyhow::Result<Ended> {
        let session = Session::start(http, &self.toolkey).await?;
        let Some((play, token)) = wait_for_chat(http, &session, activity).await? else {
            return Ok(Ended::Renewing);
        };
        let chat_url = play
            .chat_url
            .as_deref()
            .filter(|url| !url.is_empty())
            .context("RPlay names no chat server for the channel")?;
        let emotes = api::emotes(http, &session.channel).await;
        let mut socket = api::open(chat_url, &token.token).await?;
        info!(
            session_ends = ?token.session_ends(),
            can_send = ?token.can_send_message,
            "connected to RPlay chat"
        );
        activity.set(live_state(play.is_live()));
        #[cfg(feature = "capture")]
        let mut capture = self
            .capture_dir
            .as_deref()
            .and_then(|dir| capture::Capture::open(dir, &self.label));

        let managers = play
            .managers
            .unwrap_or_default()
            .into_iter()
            .filter_map(|manager| manager.user_oid);
        let mut room = Room::new(
            session.channel.clone(),
            managers,
            play.banned_words.unwrap_or_default(),
            play.use_anonymous_chat.unwrap_or(false),
            emotes,
        );
        let mut avatars = Avatars::default();
        let renew_at = renewal_time(&token);
        let mut pings = tokio::time::interval(PING_EVERY);
        // An interval's first tick is immediate; the first ping is due later.
        pings.tick().await;
        let mut heard = Instant::now();
        loop {
            // Waits for whichever comes first. The losing branches' futures
            // are dropped before the winner's code runs, so the ping branch
            // may use `socket` although `socket.next()` borrowed it.
            tokio::select! {
                frame = socket.next() => {
                    let Some(frame) = frame else {
                        return Ok(Ended::Closed);
                    };
                    heard = Instant::now();
                    let frame = frame.context("reading from RPlay's chat")?;
                    #[cfg(feature = "capture")]
                    if let Some(capture) = &mut capture {
                        match &frame {
                            Message::Text(text) => capture.frame(text),
                            Message::Close(close) => capture.note(&format!("closed: {close:?}")),
                            _ => {}
                        }
                    }
                    let text = match frame {
                        Message::Text(text) => text,
                        Message::Close(close) => {
                            // How IVS ends a session (expired, kicked, ...):
                            // for the log, to learn from.
                            match close {
                                Some(close) => info!(
                                    code = u16::from(close.code),
                                    reason = close.reason.as_str(),
                                    "RPlay's chat server closed the connection"
                                ),
                                None => info!("RPlay's chat server closed the connection"),
                            }
                            return Ok(Ended::Closed);
                        }
                        // Pongs (the answers to our pings) and pings, which
                        // tungstenite answers by itself.
                        _ => continue,
                    };
                    let mut event = match room.decode(&text) {
                        Frame::Event(event) => event,
                        Frame::Donation(pending) => {
                            let name = api::nickname(http, &pending.user_id).await;
                            ChatEvent::Message(room.finish(pending, name))
                        }
                        Frame::Live(live) => {
                            info!(live, "RPlay stream state changed");
                            activity.set(live_state(live));
                            continue;
                        }
                        Frame::ReloadEmotes => {
                            room.emotes = api::emotes(http, &session.channel).await;
                            continue;
                        }
                        Frame::UserChanged(user_id) => {
                            avatars.forget(&user_id);
                            continue;
                        }
                        Frame::Error(error) => {
                            warn!("RPlay's chat server reported an error: {error}");
                            continue;
                        }
                        Frame::Ignored => continue,
                    };
                    // Only photos that exist: a new sender's message waits
                    // for the check, so chat stays in order.
                    if let ChatEvent::Message(message) = &mut event {
                        avatars.verify(http, message).await;
                    }
                    if tx.send(event).await.is_err() {
                        return Ok(Ended::ReceiverGone);
                    }
                }
                _ = pings.tick() => {
                    socket
                        .send(Message::Ping(Default::default()))
                        .await
                        .context("sending a ping to RPlay's chat")?;
                }
                // A deadline, not a timeout around `next()`: the ping branch
                // wins every 30 s, which would restart a timeout each time.
                () = tokio::time::sleep_until(heard + SILENT_FOR) => {
                    bail!("no answer from RPlay's chat for {} s", SILENT_FOR.as_secs());
                }
                () = tokio::time::sleep_until(renew_at) => {
                    debug!("RPlay's chat session is about to end");
                    // A polite goodbye; the connection is dropped either way.
                    let _ = socket.close(None).await;
                    return Ok(Ended::Renewing);
                }
            }
        }
    }
}

/// The channel's state and a chat token. While RPlay gives no token
/// because the channel is offline, waits for the stream to start, asking
/// again every minute; the status says so. `None` when the live-tool
/// session runs out while waiting: then a new one is needed.
///
/// A fallback: the streamer's own token worked while offline in the first
/// live test (2026-11-15), so this didn't wait. An anonymous token for an
/// offline channel was refused (403) once, on a channel that also required
/// a login; should RPlay ever refuse the streamer the same way, the source
/// waits instead of failing.
async fn wait_for_chat(
    http: &reqwest::Client,
    session: &Session,
    activity: &Reporter,
) -> anyhow::Result<Option<(Play, ChatToken)>> {
    let mut waiting = false;
    loop {
        let play = session.play(http).await?;
        play.log();
        let refused = match session.chat_token(http).await {
            Ok(token) => return Ok(Some((play, token))),
            Err(e) => match chat_core::find_error::<HttpError>(&e) {
                Some(refusal) if refusal.status == StatusCode::FORBIDDEN && !play.is_live() => {
                    refusal.to_string()
                }
                _ => return Err(e),
            },
        };
        if !waiting {
            info!(
                "RPlay gives no chat token while the channel is offline ({refused}); waiting for the stream to start"
            );
            waiting = true;
        }
        activity.set(Activity::Idle(
            "the channel is offline: waiting for the stream to start".into(),
        ));
        tokio::time::sleep(OFFLINE_RETRY).await;
        let session_ending = session.expires_at.is_some_and(|ends| {
            ends.duration_since(jiff::Timestamp::now()) < jiff::SignedDuration::from_mins(5)
        });
        if session_ending {
            return Ok(None);
        }
    }
}

/// When to end the chat session ourselves: just before IVS would.
fn renewal_time(token: &ChatToken) -> Instant {
    let left = token
        .session_ends()
        .and_then(|ends| {
            let left = ends.duration_since(jiff::Timestamp::now());
            Duration::try_from(left).ok()
        })
        .map_or(RENEW_AFTER, |left| left.saturating_sub(RENEW_BEFORE_END));
    // Never right away: a session that ends at once (a wrong clock, an
    // odd answer) would otherwise make the source reconnect in a loop.
    Instant::now() + left.max(Duration::from_secs(60))
}

/// The channel's chat is read whether it's live or not; offline just means
/// it's quieter, so it's not a problem to show.
fn live_state(live: bool) -> Activity {
    if live {
        Activity::Receiving
    } else {
        Activity::Idle("the channel is offline (its chat is still read)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(normalize_label("  Main ").as_deref(), Ok("Main"));
        for bad in ["", "   ", "two\nlines", &"x".repeat(41)] {
            assert!(normalize_label(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_key_comes_from_any_widget_link() {
        for link in [
            "https://rplay.live/live/chat/basic/0123456789abcdef01234567?toolkey=Key-123_abc&lang=en",
            "https://rplay.live/live/chat/box/0123456789abcdef01234567?fs=17&b=%23000&c=%23fff&toolkey=Key-123_abc&lang=en",
            "https://rplay.live/live/donation/goal/0123456789abcdef01234567?toolkey=Key-123_abc",
            "rplay.live/live/chat/basic/0123456789abcdef01234567?toolkey=Key-123_abc",
            " https://www.rplay.live/live/studio/x?m=dark&toolkey=Key-123_abc ",
        ] {
            assert_eq!(toolkey_from(link).as_deref(), Ok("Key-123_abc"), "{link}");
        }
        // The key itself works too, and percent-encoded parts are decoded.
        assert_eq!(toolkey_from("Key-123_abc").as_deref(), Ok("Key-123_abc"));
        assert_eq!(
            toolkey_from("https://rplay.live/x?toolkey=a%2Bb%3D").as_deref(),
            Ok("a+b=")
        );
    }

    #[test]
    fn links_without_a_key_are_refused() {
        for input in [
            "",
            "https://rplay.live/live/0123456789abcdef01234567",
            "https://rplay.live/c/your_channel/live",
            "https://rplay.live/x?toolkey=",
            "https://example.com/x?toolkey=Key-123_abc",
            "has a space",
        ] {
            let problem = toolkey_from(input).unwrap_err();
            assert!(problem.contains("Dashboard Widget"), "{input:?}: {problem}");
        }
    }

    #[test]
    fn renewal_comes_before_the_session_ends() {
        let in_an_hour = jiff::Timestamp::now() + jiff::SignedDuration::from_mins(60);
        let token = ChatToken {
            session_expiration_time: Some(in_an_hour.to_string().into()),
            ..ChatToken::default()
        };
        let left = renewal_time(&token) - Instant::now();
        assert!(left < Duration::from_secs(3600 - 29), "{left:?}");
        assert!(left > Duration::from_secs(3600 - 35), "{left:?}");
        // Unknown: after 55 minutes. Already over: not at once.
        let left = renewal_time(&ChatToken::default()) - Instant::now();
        assert!(left > Duration::from_secs(54 * 60), "{left:?}");
        let past = ChatToken {
            session_expiration_time: Some("2020-01-01T00:00:00Z".into()),
            ..ChatToken::default()
        };
        assert!(renewal_time(&past) - Instant::now() > Duration::from_secs(59));
    }
}
