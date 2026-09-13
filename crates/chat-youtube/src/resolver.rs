//! Resolves a video ID to its active live chat ID.
//! This is the one place we still use REST: 1 quota unit per call,
//! done once per source lifetime, not per reconnect.

use anyhow::Context;
use tonic::IntoRequest;

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

pub async fn resolve_live_chat_id(auth: &Auth, video_id: &str) -> anyhow::Result<String> {
    let url = format!(
        "https://www.googleapis.com/youtube/v3/videos?part=liveStreamingDetails&id={video_id}"
    );
    let resp: VideosResponse = auth
        .apply(reqwest::Client::new().get(url))
        .send()
        .await
        .context("videos.list request failed")?
        .error_for_status()
        .context("videos.list returned an error")?
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

/// A broadcast resolved to its video id, with the live chat id when the
/// API already provides one (saves the follow-up videos.list call).
pub struct ResolvedStream {
    pub video_id: String,
    pub live_chat_id: Option<String>,
}

/// Resolves the creator's current (or next scheduled) broadcast.
/// Requires OAuth (youtube.readonly scope): `mine=true` only works for
/// the token's own channel, and is the ONLY way to see unlisted and
/// members-only broadcasts. Costs 1 quota unit per call.
pub async fn resolve_own_broadcast(auth: &Auth) -> anyhow::Result<ResolvedStream> {
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
               ?part=id,snippet,status&mine=true&broadcastType=all&maxResults=50";

    let response = auth
        .apply(reqwest::Client::new().get(url))
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

    let total = resp.items.len();

    // 1. Live now (or transitioning into live)?
    if let Some(item) = resp
        .items
        .iter()
        .find(|i| matches!(life_cycle(&i.status), Some("live") | Some("liveStarting")))
    {
        return Ok(ResolvedStream {
            video_id: item.id.clone(),
            live_chat_id: item.snippet.as_ref().and_then(|s| s.live_chat_id.clone()),
        });
    }

    // 2. Otherwise the soonest scheduled broadcast.
    //    Note: `None` sorts before `Some` in min_by_key — a broadcast with
    //    no scheduledStartTime would win; acceptable for now.
    resp.items
        .into_iter()
        .filter(|i| {
            matches!(
                life_cycle(&i.status),
                Some("ready") | Some("testing") | Some("testStarting") | Some("created")
            )
        })
        .min_by_key(|i| i.snippet.as_ref().and_then(|s| s.scheduled_start_time.clone()))
        .map(|item| ResolvedStream {
            video_id: item.id,
            live_chat_id: item.snippet.and_then(|s| s.live_chat_id),
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no live or scheduled broadcasts found (liveBroadcasts.list returned {total} broadcasts, \
                 none in a live/ready/testing state)"
            )
        })
}

