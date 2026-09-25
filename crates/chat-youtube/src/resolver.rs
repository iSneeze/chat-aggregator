//! Finds the live chat to attach to: either a given video's active chat
//! (`videos.list`) or the creator's own current/next broadcast
//! (`liveBroadcasts.list`). This is the one place we still use REST:
//! 1 quota unit per call, never per gRPC reconnect.

use anyhow::Context;

#[derive(serde::Deserialize)]
struct VideosResponse {
    items: Vec<VideoItem>,
}

#[derive(serde::Deserialize)]
struct VideoItem {
    #[serde(rename = "liveStreamingDetails")]
    live_streaming_details: Option<LiveStreamingDetails>,
}

#[derive(serde::Deserialize)]
struct LiveStreamingDetails {
    #[serde(rename = "activeLiveChatId")]
    active_live_chat_id: Option<String>,
}

pub async fn resolve_live_chat_id(
    http: &reqwest::Client,
    auth: &Auth,
    video_id: &str,
) -> anyhow::Result<String> {
    // parse_with_params percent-encodes the values, so a malformed id
    // can't inject extra query parameters.
    let url = reqwest::Url::parse_with_params(
        "https://www.googleapis.com/youtube/v3/videos",
        [("part", "liveStreamingDetails"), ("id", video_id)],
    )?;
    let response = auth
        .apply(http.get(url))
        .send()
        .await
        .context("videos.list request failed")?;

    // Check the status BEFORE parsing: Google's error body says *why*.
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("videos.list failed: HTTP {status}: {body}");
    }

    let resp: VideosResponse = response
        .json()
        .await
        .context("videos.list returned invalid JSON")?;

    resp.items
        .into_iter()
        .next()
        .and_then(|v| v.live_streaming_details)
        .and_then(|d| d.active_live_chat_id)
        .ok_or_else(|| {
            anyhow::anyhow!("video {video_id} has no active live chat (offline, or not visible to these credentials?)")
        })
}

/// How to authenticate a YouTube Data API call.
/// Google accepts an API key OR an OAuth access token on read endpoints;
/// unlisted/members-only resources additionally require a token belonging
/// to an account allowed to see them.
#[derive(Clone)]
pub enum Auth {
    ApiKey(String),
    Bearer(String),
}

impl Auth {
    fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Auth::ApiKey(key) => rb.header("x-goog-api-key", key.as_str()),
            Auth::Bearer(token) => rb.bearer_auth(token),
        }
    }
}

/// A resolved broadcast, with enough information for the caller to decide
/// what to do next.
///
/// - `is_live == true`: an active chat exists; attach to `live_chat_id`.
/// - `is_live == false`: scheduled but not live yet; wait until
///   `scheduled_start_time`, polling faster as it approaches.
pub struct ResolvedStream {
    pub video_id: String,
    pub live_chat_id: Option<String>,
    pub is_live: bool,
    pub scheduled_start_time: Option<chrono::DateTime<chrono::Utc>>,
}

fn parse_time(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    raw.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// Resolves the creator's current broadcast, or the soonest scheduled one.
///
/// Returns `Ok(None)` when nothing is live and nothing is scheduled: the
/// caller should keep scanning rather than treat that as an error.
/// Requires OAuth (youtube.readonly scope): `mine=true` only works for
/// the token's own channel, and is the ONLY way to see unlisted and
/// members-only broadcasts. Costs 1 quota unit per call.
pub async fn resolve_own_broadcast(
    http: &reqwest::Client,
    auth: &Auth,
    log_all: bool,
) -> anyhow::Result<Option<ResolvedStream>> {
    if !matches!(auth, Auth::Bearer(_)) {
        anyhow::bail!("OwnBroadcast requires OAuth; API keys cannot use `mine=true`");
    }

    #[derive(serde::Deserialize)]
    struct BroadcastsResponse {
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

    fn life_cycle(status: &Option<BroadcastStatus>) -> Option<&str> {
        status.as_ref().and_then(|s| s.life_cycle_status.as_deref())
    }

    // Only ONE of `mine` / `broadcastStatus` / `id` may be used per request
    // (the API rejects combinations), so fetch everything we own and
    // classify lifeCycleStatus locally.
    let url = "https://www.googleapis.com/youtube/v3/liveBroadcasts\
               ?part=id,snippet,status&mine=true&broadcastType=all&maxResults=10";

    let response = auth
        .apply(http.get(url))
        .send()
        .await
        .context("liveBroadcasts.list request failed")?;

    // Check the status BEFORE parsing: Google's error body says *why*.
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("liveBroadcasts.list failed: HTTP {status}: {body}");
    }
    let resp: BroadcastsResponse = response
        .json()
        .await
        .context("liveBroadcasts.list returned unexpected JSON")?;

    // Debug aid: on the first scan poll, show every broadcast we own so the
    // lifeCycleStatus classification is visible. Later polls stay quiet.
    if log_all {
        for item in &resp.items {
            eprintln!(
                "[youtube] own broadcast {} status={:?} scheduled={:?}",
                item.id,
                life_cycle(&item.status),
                item.snippet
                    .as_ref()
                    .and_then(|s| s.scheduled_start_time.as_deref()),
            );
        }
    }

    // 1. Live now (or transitioning into live)? Attach immediately.
    if let Some(item) = resp
        .items
        .iter()
        .find(|i| matches!(life_cycle(&i.status), Some("live") | Some("liveStarting")))
    {
        return Ok(Some(ResolvedStream {
            video_id: item.id.clone(),
            live_chat_id: item.snippet.as_ref().and_then(|s| s.live_chat_id.clone()),
            is_live: true,
            scheduled_start_time: None,
        }));
    }

    // 2. Otherwise the soonest scheduled broadcast, if any.
    //    Times are parsed before comparing, and the key `(is_none, time)`
    //    sorts broadcasts without a start time last: tuples compare field
    //    by field and `false < true`.
    Ok(resp
        .items
        .into_iter()
        .filter(|i| {
            matches!(
                life_cycle(&i.status),
                Some("ready") | Some("testing") | Some("testStarting") | Some("created")
            )
        })
        .map(|item| {
            let start = parse_time(
                item.snippet
                    .as_ref()
                    .and_then(|s| s.scheduled_start_time.as_deref()),
            );
            (start, item)
        })
        .min_by_key(|(start, _)| (start.is_none(), *start))
        .map(|(scheduled_start_time, item)| ResolvedStream {
            video_id: item.id,
            live_chat_id: item.snippet.and_then(|s| s.live_chat_id),
            is_live: false,
            scheduled_start_time,
        }))
}
