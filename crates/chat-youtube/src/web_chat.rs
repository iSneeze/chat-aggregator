//! Learns custom emoji (`:code:` → image) from YouTube's own web chat: the
//! API sends only the `:code:`.
//!
//! The chat page (`/live_chat?v=<video id>`) and the requests it makes for
//! new messages (`youtubei/v1/live_chat/get_live_chat`) carry every
//! message's emoji with its codes and image. This reads them the way the
//! page does, without a login, and keeps only the emoji: the messages
//! themselves still come from the API. (Measured 2026-10-20, see ROADMAP.)
//!
//! Unofficial: YouTube can change any of it at any time. So everything here
//! is best effort: problems are logged and retried slowly, and never touch
//! the chat itself.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, info, warn};

/// One learned emoji: its `:code:` and image URL.
pub type Learned = (String, String);

const YOUTUBE: &str = "https://www.youtube.com";

/// YouTube answers anything that doesn't look like a browser with a stub
/// page without the chat (a short `Mozilla/5.0` isn't enough).
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";

/// Between polls, when YouTube doesn't say (it suggests 10 s).
const POLL_DEFAULT: Duration = Duration::from_secs(10);
/// Bounds for YouTube's suggestion: never hammer it, never fall far behind.
const POLL_MIN: Duration = Duration::from_secs(1);
const POLL_MAX: Duration = Duration::from_secs(30);

/// After a failure, wait this long, doubling up to `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(10 * 60);

/// The page has no chat for us: switched off, members-only (we're not
/// logged in), or over. Look again after a while; the API side decides
/// whether the source keeps running.
const NO_CHAT_RETRY: Duration = Duration::from_secs(5 * 60);

/// A running reader. Dropping it stops the reader.
pub(crate) struct Reader {
    learned: mpsc::Receiver<Vec<Learned>>,
    // `AbortOnDropHandle`: the task is cancelled when this is dropped, so a
    // finished broadcast (or a stopped source) leaves nothing running.
    _task: AbortOnDropHandle<()>,
}

impl Reader {
    /// Starts reading the web chat of `video_id`.
    pub(crate) fn start(http: reqwest::Client, video_id: String) -> Self {
        let (tx, learned) = mpsc::channel(16);
        let task = tokio::spawn(learn(http, YOUTUBE.to_string(), video_id, tx));
        Self {
            learned,
            _task: AbortOnDropHandle::new(task),
        }
    }

    /// A reader fed by the test through the returned sender.
    #[cfg(test)]
    pub(crate) fn fake() -> (mpsc::Sender<Vec<Learned>>, Self) {
        let (tx, learned) = mpsc::channel(16);
        let task = AbortOnDropHandle::new(tokio::spawn(async {}));
        (
            tx,
            Self {
                learned,
                _task: task,
            },
        )
    }

    /// Emoji learned since the last call, without waiting.
    pub(crate) fn try_recv(&mut self) -> Option<Vec<Learned>> {
        self.learned.try_recv().ok()
    }
}

/// Follows the web chat until `learned` closes, sending emoji not sent
/// before. `base`: YouTube's address (a local server in tests).
pub async fn learn(
    http: reqwest::Client,
    base: String,
    video_id: String,
    learned: mpsc::Sender<Vec<Learned>>,
) {
    // Code → URL of everything sent so far, across page reloads: each
    // emoji is sent once (again only if its picture changes).
    let mut sent = HashMap::new();
    let mut retry = RETRY_FIRST;
    let mut said_no_chat = false;
    loop {
        let wait = match follow(&http, &base, &video_id, &mut sent, &learned).await {
            Ok(Stop::ReceiverGone) => return,
            Ok(Stop::Reload) => {
                debug!("web chat asks for a reload");
                retry = RETRY_FIRST;
                POLL_DEFAULT
            }
            Ok(Stop::NoChat) => {
                if !said_no_chat {
                    info!(
                        video_id,
                        "no web chat to learn emoji from (members-only or chat off?)"
                    );
                    said_no_chat = true;
                }
                NO_CHAT_RETRY
            }
            Err(e) => {
                warn!(?retry, "learning emoji from the web chat: {e:#}");
                let wait = retry;
                retry = (retry * 2).min(RETRY_MAX);
                wait
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// Why `follow` stopped (when it didn't fail).
#[derive(Debug, PartialEq)]
enum Stop {
    /// Nobody listens anymore: the broadcast or the source is over.
    ReceiverGone,
    /// YouTube wants the page loaded again (the position went stale).
    Reload,
    /// No chat to follow (see `NO_CHAT_RETRY`).
    NoChat,
}

/// Loads the chat page, then polls for new messages until something
/// interrupts that.
async fn follow(
    http: &reqwest::Client,
    base: &str,
    video_id: &str,
    sent: &mut HashMap<String, String>,
    learned: &mpsc::Sender<Vec<Learned>>,
) -> anyhow::Result<Stop> {
    // `parse_with_params` percent-encodes the id, so it can't add
    // parameters of its own.
    let url = reqwest::Url::parse_with_params(&format!("{base}/live_chat"), [("v", video_id)])?;
    let html = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .context("loading the chat page")?
        .error_for_status()?
        .text()
        .await?;
    let (client_version, data) = parse_page(&html)?;
    let Some(chat) = data.pointer("/contents/liveChatRenderer") else {
        return Ok(Stop::NoChat);
    };
    if !send_new(chat, sent, learned).await {
        return Ok(Stop::ReceiverGone);
    }

    let mut next = next_poll(chat);
    loop {
        let (continuation, wait) = match next {
            Next::Poll { continuation, wait } => (continuation, wait),
            Next::Reload => return Ok(Stop::Reload),
            Next::Ended => return Ok(Stop::NoChat),
        };
        tokio::time::sleep(wait).await;
        // What the page itself sends: who's asking, and where it left off.
        let body = serde_json::json!({
            "context": { "client": { "clientName": "WEB", "clientVersion": client_version } },
            "continuation": continuation,
        });
        let answer: Value = http
            .post(format!(
                "{base}/youtubei/v1/live_chat/get_live_chat?prettyPrint=false"
            ))
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .json(&body)
            .send()
            .await
            .context("polling the web chat")?
            .error_for_status()?
            .json()
            .await
            .context("the web chat answered with something other than JSON")?;
        let Some(chat) = answer.pointer("/continuationContents/liveChatContinuation") else {
            return Ok(Stop::NoChat);
        };
        if !send_new(chat, sent, learned).await {
            return Ok(Stop::ReceiverGone);
        }
        next = next_poll(chat);
    }
}

/// Sends the emoji in `value` that weren't sent before. `false` if nobody
/// listens anymore.
async fn send_new(
    value: &Value,
    sent: &mut HashMap<String, String>,
    learned: &mpsc::Sender<Vec<Learned>>,
) -> bool {
    let mut found = Vec::new();
    emojis_in(value, &mut found);
    // `insert` hands back the URL it replaced: new unless that was this
    // same URL. (Also drops repeats within `found`.)
    found.retain(|(code, url)| sent.insert(code.clone(), url.clone()).as_ref() != Some(url));
    if found.is_empty() {
        return !learned.is_closed();
    }
    debug!(count = found.len(), "learned emoji from the web chat");
    learned.send(found).await.is_ok()
}

/// The client version and the embedded data (`ytInitialData`) of the chat
/// page. The page script sets them as JavaScript: the data is a JSON
/// object right after its name, followed by more script.
fn parse_page(html: &str) -> anyhow::Result<(String, Value)> {
    const VERSION_KEY: &str = r#""INNERTUBE_CONTEXT_CLIENT_VERSION":""#;
    let version = html
        .split_once(VERSION_KEY)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(version, _)| version.to_string())
        .context("no client version in the chat page")?;

    let after_name = html
        .split_once("ytInitialData")
        .context("no chat data in the chat page")?
        .1;
    let start = after_name
        .find('{')
        .context("no chat data in the chat page")?;
    // A stream deserializer reads one JSON value and stops there, ignoring
    // the script that follows (`from_str` would call that an error).
    let data = serde_json::Deserializer::from_str(&after_name[start..])
        .into_iter::<Value>()
        .next()
        .context("no chat data in the chat page")?
        .context("the chat page's data isn't valid JSON")?;
    Ok((version, data))
}

/// What to do after a page or poll answer.
#[derive(Debug, PartialEq)]
enum Next {
    /// Ask again with this `continuation` (YouTube's "where you left off")
    /// after `wait`.
    Poll {
        continuation: String,
        wait: Duration,
    },
    /// The position is stale: load the page again.
    Reload,
    /// Nothing to continue with: the chat is over.
    Ended,
}

/// Reads `continuations` of a chat (`liveChatRenderer` from the page, or
/// `liveChatContinuation` from a poll). Its one entry is named after the
/// kind (`invalidationContinuationData`, `timedContinuationData`, …); all
/// but the reload kind are polled the same way.
fn next_poll(chat: &Value) -> Next {
    let Some(Value::Object(entry)) = chat.pointer("/continuations/0") else {
        return Next::Ended;
    };
    if entry.contains_key("reloadContinuationData") {
        return Next::Reload;
    }
    let Some(data) = entry.values().next() else {
        return Next::Ended;
    };
    let Some(continuation) = data.get("continuation").and_then(Value::as_str) else {
        return Next::Ended;
    };
    let wait = data
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .map_or(POLL_DEFAULT, Duration::from_millis)
        .clamp(POLL_MIN, POLL_MAX);
    Next::Poll {
        continuation: continuation.to_string(),
        wait,
    }
}

/// Every custom emoji anywhere in `value`. Searching the whole answer
/// rather than following a path: emoji appear in messages, Super Chats,
/// membership notes and the ticker, and paths change more often than the
/// emoji objects do.
fn emojis_in(value: &Value, out: &mut Vec<Learned>) {
    match value {
        Value::Object(map) => {
            if let Some(emoji) = map.get("emoji") {
                custom_emoji(emoji, out);
            }
            for inner in map.values() {
                emojis_in(inner, out);
            }
        }
        Value::Array(items) => {
            for inner in items {
                emojis_in(inner, out);
            }
        }
        _ => {}
    }
}

/// One emoji object: `{ "isCustomEmoji": true, "shortcuts": [":_hype:", …],
/// "image": { "thumbnails": [{ "url", "width" }, …] } }`. Unicode emoji
/// (no `isCustomEmoji`) need no mapping: they arrive as characters.
fn custom_emoji(emoji: &Value, out: &mut Vec<Learned>) {
    if emoji.get("isCustomEmoji").and_then(Value::as_bool) != Some(true) {
        return;
    }
    // The largest picture: overlays often show emoji bigger than chat does.
    let url = emoji
        .pointer("/image/thumbnails")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .max_by_key(|t| t.get("width").and_then(Value::as_u64).unwrap_or(0))
        .and_then(|t| t.get("url"))
        .and_then(Value::as_str);
    // Only https: the overlay's CSP allows no other images, and it keeps
    // `javascript:` and the like out for good.
    let Some(url) = url.filter(|u| u.starts_with("https://")) else {
        return;
    };
    // A channel emoji has several codes (`:_hypeWave:`, `:hypeWave:`, …)
    // for one picture; any of them can show up in a message.
    let codes = emoji
        .get("shortcuts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|code| is_code(code));
    for code in codes {
        out.push((code.to_string(), url.to_string()));
    }
}

/// `:something:` with no colons or spaces inside: what `EmojiMap` matches.
fn is_code(code: &str) -> bool {
    code.len() > 2
        && code.starts_with(':')
        && code.ends_with(':')
        && !code[1..code.len() - 1].contains(|c: char| c == ':' || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A custom emoji as YouTube sends it.
    fn emoji(codes: &[&str], image: &str) -> Value {
        json!({
            "emojiId": format!("UC_channel/{image}"),
            "shortcuts": codes,
            "searchTerms": [],
            "image": { "thumbnails": [
                { "url": format!("https://yt3.ggpht.com/{image}=w24-h24"), "width": 24, "height": 24 },
                { "url": format!("https://yt3.ggpht.com/{image}=w48-h48"), "width": 48, "height": 48 }
            ] },
            "isCustomEmoji": true
        })
    }

    /// A text message whose runs hold `emoji`.
    fn message(emoji: Value) -> Value {
        json!({ "addChatItemAction": { "item": { "liveChatTextMessageRenderer": {
            "message": { "runs": [ { "text": "hi " }, { "emoji": emoji } ] },
            "authorName": { "simpleText": "Viewer42" }
        } } } })
    }

    fn continuation(kind: &str, token: &str) -> Value {
        json!([{ kind: { "continuation": token, "timeoutMs": 1000 } }])
    }

    fn page(chat: Value) -> String {
        format!(
            r#"<html><script>var ytcfg = {{"INNERTUBE_CONTEXT_CLIENT_VERSION":"2.20261019.01.00"}};</script>
            <script>window["ytInitialData"] = {};</script><script>if (window.x) {{ y(); }}</script></html>"#,
            json!({ "contents": { "liveChatRenderer": chat } })
        )
    }

    #[test]
    fn custom_emoji_are_found_with_every_code_and_the_largest_image() {
        let answer = json!({ "actions": [
            message(emoji(&[":_hypeWave:", ":hypeWave:"], "hype")),
            // A unicode emoji: arrives as a character, nothing to learn.
            message(json!({ "emojiId": "😀", "shortcuts": [":grinning:"],
                "image": { "thumbnails": [{ "url": "https://www.youtube.com/s/x.svg" }] } })),
        ] });
        let mut found = Vec::new();
        emojis_in(&answer, &mut found);
        assert_eq!(
            found,
            [
                (
                    ":_hypeWave:".to_string(),
                    "https://yt3.ggpht.com/hype=w48-h48".to_string()
                ),
                (
                    ":hypeWave:".to_string(),
                    "https://yt3.ggpht.com/hype=w48-h48".to_string()
                ),
            ]
        );
    }

    #[test]
    fn odd_codes_and_non_https_images_are_skipped() {
        let mut odd = emoji(&[":ok:", "plain", "::", ":two words:", ":a:b:"], "x");
        let mut found = Vec::new();
        custom_emoji(&odd, &mut found);
        assert_eq!(found.len(), 1, "only :ok: is a code: {found:?}");

        odd["image"]["thumbnails"] = json!([{ "url": "javascript:alert(1)", "width": 48 }]);
        found.clear();
        custom_emoji(&odd, &mut found);
        assert!(found.is_empty());
    }

    #[test]
    fn the_page_gives_version_data_and_where_to_continue() {
        let html = page(json!({
            "actions": [message(emoji(&[":_hypeWave:"], "hype"))],
            "continuations": continuation("invalidationContinuationData", "next-1"),
        }));
        let (version, data) = parse_page(&html).unwrap();
        assert_eq!(version, "2.20261019.01.00");
        let chat = data.pointer("/contents/liveChatRenderer").unwrap();
        assert_eq!(
            next_poll(chat),
            Next::Poll {
                continuation: "next-1".into(),
                wait: Duration::from_secs(1)
            }
        );
    }

    #[test]
    fn a_page_without_the_data_is_an_error() {
        assert!(parse_page("<html>Something went wrong</html>").is_err());
    }

    #[test]
    fn continuations_poll_reload_or_end() {
        let chat = |c: Value| json!({ "continuations": c });
        assert_eq!(
            next_poll(&chat(continuation("reloadContinuationData", "r"))),
            Next::Reload
        );
        assert_eq!(next_poll(&json!({})), Next::Ended);
        // No hint: the default; a silly one: within bounds.
        let timed = |ms: Option<u64>| {
            let mut c = json!([{ "timedContinuationData": { "continuation": "t" } }]);
            if let Some(ms) = ms {
                c[0]["timedContinuationData"]["timeoutMs"] = json!(ms);
            }
            match next_poll(&chat(c)) {
                Next::Poll { wait, .. } => wait,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(timed(None), POLL_DEFAULT);
        assert_eq!(timed(Some(0)), POLL_MIN);
        assert_eq!(timed(Some(3_600_000)), POLL_MAX);
    }

    #[tokio::test]
    async fn each_emoji_is_sent_once() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut sent = HashMap::new();
        let answer = json!([
            message(emoji(&[":_hypeWave:"], "hype")),
            message(emoji(&[":_hypeWave:"], "hype")),
        ]);
        assert!(send_new(&answer, &mut sent, &tx).await);
        assert_eq!(rx.try_recv().unwrap().len(), 1, "repeats in one answer");
        assert!(send_new(&answer, &mut sent, &tx).await);
        assert!(rx.try_recv().is_err(), "nothing new: nothing sent");
        let changed = json!([message(emoji(&[":_hypeWave:"], "hype-v2"))]);
        assert!(send_new(&changed, &mut sent, &tx).await);
        assert_eq!(rx.try_recv().unwrap().len(), 1, "a new picture is sent");
        drop(rx);
        assert!(!send_new(&answer, &mut sent, &tx).await, "nobody listens");
    }

    /// The whole round against a stand-in for YouTube: page, one poll that
    /// brings a new emoji, then a stale position.
    #[tokio::test]
    async fn follows_the_page_and_its_polls() {
        use axum::routing::{get, post};
        let html = page(json!({
            "actions": [message(emoji(&[":_hypeWave:"], "hype"))],
            "continuations": continuation("invalidationContinuationData", "next-1"),
        }));
        let polls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = polls.clone();
        let router = axum::Router::new()
            .route(
                "/live_chat",
                get(
                    |axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| async move {
                        assert_eq!(q["v"], "video-1");
                        axum::response::Html(html)
                    },
                ),
            )
            .route(
                "/youtubei/v1/live_chat/get_live_chat",
                post(move |axum::Json(body): axum::Json<Value>| async move {
                    seen.lock().unwrap().push(body);
                    axum::Json(json!({ "continuationContents": { "liveChatContinuation": {
                        "actions": [message(emoji(&[":_wave:"], "wave"))],
                        "continuations": continuation("reloadContinuationData", "r"),
                    } } }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });

        let (tx, mut rx) = mpsc::channel(4);
        let stop = follow(
            &reqwest::Client::new(),
            &base,
            "video-1",
            &mut HashMap::new(),
            &tx,
        )
        .await
        .unwrap();

        assert_eq!(stop, Stop::Reload);
        assert_eq!(
            rx.recv().await.unwrap()[0].0,
            ":_hypeWave:",
            "from the page"
        );
        assert_eq!(rx.recv().await.unwrap()[0].0, ":_wave:", "from the poll");
        let polls = polls.lock().unwrap();
        assert_eq!(polls.len(), 1);
        assert_eq!(polls[0]["continuation"], "next-1");
        assert_eq!(
            polls[0]["context"]["client"]["clientVersion"],
            "2.20261019.01.00"
        );
    }
}
