//! Finds the live chat to attach to: either a given video's active chat
//! (`videos.list`) or the creator's own current/next broadcast
//! (`liveBroadcasts.list`). This is the one place we still use REST:
//! 1 quota unit per call, never per gRPC reconnect.

use std::time::Duration;

use anyhow::Context;

use crate::oauth::TokenProvider;
use crate::quota::{QuotaExhausted, is_quota_body};

/// Longest a whole REST call (connect, send, read the answer) may take.
const REST_TIMEOUT: Duration = Duration::from_secs(30);
const REST_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The HTTP client for the REST calls in here. A plain `Client::new()` has
/// no time limit at all: a call that stalls (network gone mid-answer) would
/// hang the source for good while it looks for your broadcast. With a
/// limit, it fails like any network error and the usual retry takes over.
pub fn rest_client() -> reqwest::Client {
    client_with_timeouts(REST_TIMEOUT, REST_CONNECT_TIMEOUT)
}

fn client_with_timeouts(total: Duration, connect: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(total)
        .connect_timeout(connect)
        .build()
        .expect("static reqwest configuration is valid")
}

/// Turns a failed REST response into an error. Checking the status before
/// parsing matters: Google's error body says *why*. A used-up daily quota
/// becomes `QuotaExhausted`, so the source can wait for the reset.
fn http_error(api: &str, status: reqwest::StatusCode, body: String) -> anyhow::Error {
    if is_quota_body(&body) {
        QuotaExhausted(format!("{api}: HTTP {status}")).into()
    } else {
        anyhow::anyhow!("{api} failed: HTTP {status}: {body}")
    }
}

#[derive(serde::Deserialize)]
struct VideosResponse {
    // With `fields`, Google leaves out an empty list altogether (`{}`).
    #[serde(default)]
    items: Vec<VideoItem>,
}

#[derive(serde::Deserialize)]
struct VideoItem {
    snippet: Option<VideoSnippet>,
    #[serde(rename = "liveStreamingDetails")]
    live_streaming_details: Option<LiveStreamingDetails>,
}

#[derive(serde::Deserialize)]
struct VideoSnippet {
    #[serde(rename = "channelId")]
    channel_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct LiveStreamingDetails {
    #[serde(rename = "activeLiveChatId")]
    active_live_chat_id: Option<String>,
}

/// The live chat of a given video. Costs 1 quota unit.
pub async fn resolve_video(
    http: &reqwest::Client,
    auth: &Auth,
    video_id: &str,
) -> anyhow::Result<ResolvedStream> {
    // parse_with_params percent-encodes the values, so a malformed id
    // can't inject extra query parameters. `snippet` (for the channel id)
    // costs nothing extra: a call is 1 unit whatever parts it asks for.
    // `fields` trims the answer to what we read (title, description,
    // thumbnails and tags would be most of it).
    let url = reqwest::Url::parse_with_params(
        "https://www.googleapis.com/youtube/v3/videos",
        [
            ("part", "snippet,liveStreamingDetails"),
            ("id", video_id),
            (
                "fields",
                "items(snippet/channelId,liveStreamingDetails/activeLiveChatId)",
            ),
        ],
    )?;
    let response = auth
        .apply(http.get(url))
        .await?
        .send()
        .await
        .context("videos.list request failed")?;

    // Check the status BEFORE parsing: Google's error body says *why*.
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(http_error("videos.list", status, body));
    }

    let resp: VideosResponse = response
        .json()
        .await
        .context("videos.list returned invalid JSON")?;

    let video = resp.items.into_iter().next();
    let channel_id = video
        .as_ref()
        .and_then(|v| v.snippet.as_ref())
        .and_then(|s| s.channel_id.clone());
    let live_chat_id = video
        .and_then(|v| v.live_streaming_details)
        .and_then(|d| d.active_live_chat_id)
        .ok_or_else(|| {
            anyhow::anyhow!("video {video_id} has no active live chat (offline, or not visible to these credentials?)")
        })?;
    Ok(ResolvedStream {
        video_id: video_id.to_string(),
        channel_id,
        live_chat_id: Some(live_chat_id),
        is_live: true,
        scheduled_start_time: None,
    })
}

/// How to authenticate a YouTube Data API call.
/// Google accepts an API key OR an OAuth access token on read endpoints;
/// unlisted/members-only resources additionally require a token belonging
/// to an account allowed to see them.
#[derive(Clone)]
pub enum Auth {
    /// Public data only (the testing path: any public video by id).
    ApiKey(String),
    /// A logged-in account (the streamer path, see `crate::oauth`).
    OAuth(TokenProvider),
}

impl Auth {
    /// The header that authenticates a request, as (name, value). Async
    /// because an OAuth access token may have to be refreshed first.
    pub(crate) async fn header(&self) -> anyhow::Result<(&'static str, String)> {
        Ok(match self {
            Auth::ApiKey(key) => ("x-goog-api-key", key.clone()),
            // The scheme is required: without "Bearer " the gRPC endpoint
            // answers `PermissionDenied: unregistered callers`.
            Auth::OAuth(tokens) => (
                "authorization",
                format!("Bearer {}", tokens.access_token().await?),
            ),
        })
    }

    async fn apply(&self, rb: reqwest::RequestBuilder) -> anyhow::Result<reqwest::RequestBuilder> {
        let (name, value) = self.header().await?;
        Ok(rb.header(name, value))
    }
}

/// The logged-in account's channel name, e.g. to confirm a login worked.
/// Costs 1 quota unit.
pub async fn channel_title(http: &reqwest::Client, auth: &Auth) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct ChannelsResponse {
        #[serde(default)]
        items: Vec<Channel>,
    }
    #[derive(serde::Deserialize)]
    struct Channel {
        snippet: ChannelSnippet,
    }
    #[derive(serde::Deserialize)]
    struct ChannelSnippet {
        title: String,
    }

    let url = "https://www.googleapis.com/youtube/v3/channels\
               ?part=snippet&mine=true&fields=items/snippet/title";
    let response = auth
        .apply(http.get(url))
        .await?
        .send()
        .await
        .context("channels.list request failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(http_error("channels.list", status, body));
    }
    let resp: ChannelsResponse = response
        .json()
        .await
        .context("channels.list returned unexpected JSON")?;
    resp.items
        .into_iter()
        .next()
        .map(|c| c.snippet.title)
        .context("this Google account has no YouTube channel")
}

/// A resolved broadcast, with enough information for the caller to decide
/// what to do next.
///
/// - `is_live == true`: an active chat exists; attach to `live_chat_id`.
/// - `is_live == false`: scheduled but not live yet; wait until
///   `scheduled_start_time` (always set then), polling faster as it
///   approaches.
pub struct ResolvedStream {
    pub video_id: String,
    /// The channel the broadcast belongs to (its custom emoji are cached
    /// per channel). Google always sends it; `None` only if it ever doesn't.
    pub channel_id: Option<String>,
    pub live_chat_id: Option<String>,
    pub is_live: bool,
    pub scheduled_start_time: Option<chrono::DateTime<chrono::Utc>>,
}

fn parse_time(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    raw.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// How late a scheduled broadcast may be and still count as "about to
/// start" (a streamer running late). Anything later is *stale*: typically a
/// "free chat" waiting room scheduled months ago that never goes live, or a
/// forgotten draft. Stale broadcasts are not waited for: one of them would
/// otherwise look like "starting any minute" forever, and be checked every
/// 30 s around the clock (2,880 quota units a day).
pub(crate) const STALE_AFTER: chrono::Duration = chrono::Duration::hours(3);

#[derive(serde::Deserialize)]
struct BroadcastsResponse {
    // With `fields`, Google leaves out an empty list altogether (`{}`).
    #[serde(default)]
    items: Vec<BroadcastItem>,
}

#[derive(serde::Deserialize)]
struct BroadcastItem {
    id: String,
    snippet: Option<BroadcastSnippet>,
    status: Option<BroadcastStatus>,
}

#[derive(serde::Deserialize)]
struct BroadcastSnippet {
    #[serde(rename = "channelId")]
    channel_id: Option<String>,
    #[serde(rename = "liveChatId")]
    live_chat_id: Option<String>,
    #[serde(rename = "scheduledStartTime")]
    scheduled_start_time: Option<String>,
}

#[derive(serde::Deserialize)]
struct BroadcastStatus {
    #[serde(rename = "lifeCycleStatus")]
    life_cycle_status: Option<String>,
}

impl BroadcastItem {
    fn life_cycle(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.life_cycle_status.as_deref())
    }

    fn scheduled_start(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        parse_time(
            self.snippet
                .as_ref()
                .and_then(|s| s.scheduled_start_time.as_deref()),
        )
    }
}

/// Resolves the creator's current broadcast, or the next upcoming one.
///
/// Returns `Ok(None)` when nothing is live and nothing is coming up (no
/// scheduled broadcast, or only stale ones, see [`STALE_AFTER`]): the
/// caller should keep scanning rather than treat that as an error.
/// Requires OAuth (youtube.readonly scope): `mine=true` only works for
/// the token's own channel, and is the ONLY way to see unlisted and
/// members-only broadcasts. Costs 1 quota unit per call.
pub async fn resolve_own_broadcast(
    http: &reqwest::Client,
    auth: &Auth,
    log_all: bool,
) -> anyhow::Result<Option<ResolvedStream>> {
    if !matches!(auth, Auth::OAuth(_)) {
        anyhow::bail!("OwnBroadcast requires OAuth; API keys cannot use `mine=true`");
    }

    // Only ONE of `mine` / `broadcastStatus` / `id` may be used per request
    // (the API rejects combinations), so fetch everything we own and
    // classify lifeCycleStatus locally. 50 is the most one page holds, and
    // costs the same 1 unit as 10: fewer old broadcasts can push the live
    // one off the page. This call repeats every few minutes while nothing
    // is live, so `fields` keeps only what we read: each broadcast's title,
    // description and thumbnails would otherwise be most of the answer.
    // (A misspelt field name fails the whole call with HTTP 400.)
    let url = "https://www.googleapis.com/youtube/v3/liveBroadcasts\
               ?part=id,snippet,status&mine=true&broadcastType=all&maxResults=50\
               &fields=items(id,snippet(channelId,liveChatId,scheduledStartTime),status/lifeCycleStatus)";

    let response = auth
        .apply(http.get(url))
        .await?
        .send()
        .await
        .context("liveBroadcasts.list request failed")?;

    // Check the status BEFORE parsing: Google's error body says *why*.
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(http_error("liveBroadcasts.list", status, body));
    }
    let resp: BroadcastsResponse = response
        .json()
        .await
        .context("liveBroadcasts.list returned unexpected JSON")?;

    // Debug aid: on the first scan poll, show every broadcast we own so the
    // lifeCycleStatus classification is visible. Later polls stay quiet.
    if log_all {
        for item in &resp.items {
            tracing::debug!(
                id = %item.id,
                status = ?item.life_cycle(),
                scheduled = ?item.scheduled_start(),
                "own broadcast"
            );
        }
    }

    Ok(pick_broadcast(resp.items, chrono::Utc::now()))
}

/// What to follow among your broadcasts at `now`: the live one, otherwise
/// the next upcoming one (the earliest start that isn't stale).
fn pick_broadcast(
    items: Vec<BroadcastItem>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ResolvedStream> {
    // 1. Live now (or transitioning into live)? Attach immediately.
    if let Some(item) = items
        .iter()
        .find(|i| matches!(i.life_cycle(), Some("live" | "liveStarting")))
    {
        return Some(ResolvedStream {
            video_id: item.id.clone(),
            channel_id: item.snippet.as_ref().and_then(|s| s.channel_id.clone()),
            live_chat_id: item.snippet.as_ref().and_then(|s| s.live_chat_id.clone()),
            is_live: true,
            scheduled_start_time: None,
        });
    }

    // 2. Otherwise the earliest scheduled start that's still upcoming (or
    //    at most `STALE_AFTER` late). Broadcasts without a start time can't
    //    be waited for either: they're skipped like stale ones.
    items
        .into_iter()
        .filter(|i| {
            matches!(
                i.life_cycle(),
                Some("ready" | "testing" | "testStarting" | "created")
            )
        })
        .filter_map(|item| Some((item.scheduled_start()?, item)))
        .filter(|(start, _)| *start >= now - STALE_AFTER)
        .min_by_key(|(start, _)| *start)
        .map(|(start, item)| ResolvedStream {
            video_id: item.id,
            // Fields are evaluated in the order written: this borrows the
            // snippet before the next line moves it out of `item`.
            channel_id: item.snippet.as_ref().and_then(|s| s.channel_id.clone()),
            live_chat_id: item.snippet.and_then(|s| s.live_chat_id),
            is_live: false,
            scheduled_start_time: Some(start),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stalled_rest_call_gives_up() {
        // Accepts connections and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        let client = client_with_timeouts(Duration::from_millis(300), Duration::from_secs(1));
        let call = client.get(format!("http://{addr}/")).send();
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("the call hung despite its time limit");
        assert!(result.unwrap_err().is_timeout());
    }

    /// A broadcast as `liveBroadcasts.list` returns it.
    fn broadcast(id: &str, status: &str, start: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "snippet": {
                "channelId": "UC_own",
                "liveChatId": format!("chat-{id}"),
                "scheduledStartTime": start,
            },
            "status": { "lifeCycleStatus": status },
        })
    }

    fn pick(items: Vec<serde_json::Value>, now: &str) -> Option<ResolvedStream> {
        let response: BroadcastsResponse =
            serde_json::from_value(serde_json::json!({ "items": items })).unwrap();
        pick_broadcast(response.items, now.parse().unwrap())
    }

    const NOW: &str = "2026-10-07T18:00:00Z";

    #[test]
    fn an_answer_without_items_is_an_empty_list() {
        // What `fields` turns "no broadcasts" / "no such video" into.
        let broadcasts: BroadcastsResponse = serde_json::from_str("{}").unwrap();
        assert!(pick_broadcast(broadcasts.items, NOW.parse().unwrap()).is_none());
        let videos: VideosResponse = serde_json::from_str("{}").unwrap();
        assert!(videos.items.is_empty());
    }

    #[test]
    fn a_live_broadcast_wins() {
        let picked = pick(
            vec![
                broadcast("soon", "ready", Some("2026-10-07T18:05:00Z")),
                broadcast("on-air", "live", Some("2026-10-07T17:00:00Z")),
            ],
            NOW,
        )
        .unwrap();
        assert_eq!(picked.video_id, "on-air");
        assert_eq!(picked.channel_id.as_deref(), Some("UC_own"));
        assert!(picked.is_live);
        assert_eq!(picked.live_chat_id.as_deref(), Some("chat-on-air"));
    }

    #[test]
    fn a_free_chat_room_does_not_hide_the_next_stream() {
        let picked = pick(
            vec![
                // Scheduled months ago, never goes live.
                broadcast("free-chat", "ready", Some("2026-03-01T12:00:00Z")),
                broadcast("tomorrow", "ready", Some("2026-10-08T20:00:00Z")),
                broadcast("next-week", "created", Some("2026-10-14T20:00:00Z")),
            ],
            NOW,
        )
        .unwrap();
        assert_eq!(picked.video_id, "tomorrow");
        assert!(!picked.is_live);
    }

    #[test]
    fn a_stream_running_late_is_still_waited_for() {
        let late = pick(
            vec![broadcast("late", "ready", Some("2026-10-07T15:30:00Z"))],
            NOW,
        );
        assert_eq!(late.unwrap().video_id, "late", "2.5 h late: still upcoming");
        let stale = pick(
            vec![broadcast("stale", "ready", Some("2026-10-07T14:30:00Z"))],
            NOW,
        );
        assert!(stale.is_none(), "3.5 h late: stale");
    }

    #[test]
    fn only_stale_or_unscheduled_broadcasts_mean_nothing_is_coming() {
        let picked = pick(
            vec![
                broadcast("free-chat", "ready", Some("2026-03-01T12:00:00Z")),
                broadcast("draft", "created", None),
                broadcast("old", "complete", Some("2026-10-06T20:00:00Z")),
            ],
            NOW,
        );
        assert!(picked.is_none());
    }
}
