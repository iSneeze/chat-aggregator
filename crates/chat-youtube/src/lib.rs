//! YouTube live chat source.
//!
//! Two resolution paths (see `resolver`):
//! - `YouTubeTarget::Video`: public videos, API-key auth (testing path)
//! - `YouTubeTarget::OwnBroadcast`: the authenticated creator's own
//!   broadcast, including unlisted/members-only (OAuth, the product path)
//!
//! `OwnBroadcast` runs in *scan mode*: it resolves the current broadcast,
//! attaches a gRPC `streamList` connection, and when the broadcast ends it
//! goes back to scanning for the next one — polling cheaply (REST, 1 unit)
//! while nothing is live instead of idling on an expensive gRPC stream.
//!
//! The gRPC stream is known to drop (EOF after ~10s; a Google bug with an
//! open issue-tracker ticket, not intended behaviour), so a reconnect loop
//! resumes via `page_token` with backoff. Nothing here depends on the
//! stream length: it works the same once Google fixes it.
//!
//! OAuth login and token refresh live in [`oauth`].
//!
//! `Video` is one-shot: it resolves once and completes when the stream ends.

pub use crate::emoji::EmojiMap;
pub use crate::resolver::{Auth, channel_title};
use std::time::Duration;

use anyhow::Context;
use chat_core::{Activity, ChatEvent, ChatSource, Reporter};
use tokio::sync::mpsc;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};
use tracing::{debug, info, warn};

mod convert;
mod emoji;
pub mod oauth;
mod pb;
mod quota;
mod resolver;

use convert::BoundedIdSet;
use pb::LiveChatMessageListRequest;
use pb::v3_data_live_chat_message_service_client::V3DataLiveChatMessageServiceClient;
use quota::Exhaustion;
pub use quota::QuotaExhausted;
use resolver::ResolvedStream;

const ENDPOINT: &str = "https://youtube.googleapis.com:443";

/// Scan-mode cadence: how often to re-resolve while waiting for a broadcast
/// to go live. Short once the scheduled start is within `NEAR_WINDOW`, long
/// otherwise (cheap REST calls, unlike an idle gRPC stream).
const POLL_NEAR: Duration = Duration::from_secs(30);
const POLL_FAR: Duration = Duration::from_secs(5 * 60);
const NEAR_WINDOW: chrono::Duration = chrono::Duration::minutes(5);

/// After a broadcast ends, pause before re-resolving so that
/// `liveBroadcasts.list` has moved it to `complete`. Resolving while it
/// still reads as `live` would re-attach to a closed chat, which the
/// server rejects with a fatal `FailedPrecondition`.
const SETTLE_AFTER_END: Duration = Duration::from_secs(15);

/// Minimum wait after a short-term rate limit ("too many requests").
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);

pub enum YouTubeTarget {
    /// Watch a specific video (public path, API key auth).
    Video(String),
    /// Watch the authenticated creator's own current/next broadcast
    /// (OAuth path — sees unlisted and members-only streams).
    OwnBroadcast,
}

/// Why a streamList connection ended.
enum StreamEnd {
    /// Server marked the broadcast offline (`offline_at` set): truly over.
    Offline,
    /// Connection ended without an offline marker (known ~10s EOF bug,
    /// network blips): resume with page_token.
    Eof,
    /// Downstream receiver is gone: the server is shutting down.
    ReceiverGone,
}

pub struct YouTubeSource {
    pub target: YouTubeTarget,
    pub auth: Auth,
    /// Custom emoji from the browser export; `EmojiMap::default()` for none.
    pub emojis: EmojiMap,
}

impl ChatSource for YouTubeSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        // Before the resolution loop, so we don't move self.target twice.
        let is_own_broadcast = matches!(self.target, YouTubeTarget::OwnBroadcast);

        // One HTTP/2 channel for the source's whole lifetime; each
        // broadcast is streamed over it and reconnects reuse it.
        let channel = Endpoint::from_shared(ENDPOINT.to_string())?
            .tls_config(ClientTlsConfig::new().with_webpki_roots())?
            .connect()
            .await
            .context("gRPC connect to YouTube failed")?;
        let mut client = V3DataLiveChatMessageServiceClient::new(channel);
        // Same idea for REST: one client = one connection pool, reused by
        // every resolve instead of a fresh TLS handshake per call.
        let http = reqwest::Client::new();
        let mut first_scan = true;

        // `outer`: resolve a broadcast, attach, and — for OwnBroadcast —
        // come back here to scan for the next one once it ends.
        'outer: loop {
            // --- Resolve phase (REST, 1 quota unit) ---
            let stream = match self.resolve(&http, &mut first_scan).await {
                Ok(Some(stream)) => stream,
                Ok(None) => {
                    activity.set(Activity::Idle(
                        "no live or scheduled broadcast on your channel; checking every 5 min"
                            .into(),
                    ));
                    info!(next_check = ?POLL_FAR, "nothing live or scheduled");
                    tokio::time::sleep(POLL_FAR).await;
                    continue 'outer;
                }
                // Every call fails until the daily reset: wait for it
                // instead of ending the source.
                Err(e) if e.is::<QuotaExhausted>() => {
                    quota::wait_for_reset(&format!("{e:#}"), &activity).await;
                    continue 'outer;
                }
                Err(e) => return Err(e),
            };

            // A scheduled broadcast has no chat yet: idle cheaply by
            // polling, faster as the start time approaches.
            if !stream.is_live {
                let wait = scan_interval(stream.scheduled_start_time);
                let when = stream
                    .scheduled_start_time
                    .map(|t| {
                        format!(
                            " (scheduled for {})",
                            t.with_timezone(&chrono::Local).format("%H:%M")
                        )
                    })
                    .unwrap_or_default();
                activity.set(Activity::Idle(format!(
                    "waiting for your broadcast to start{when}"
                )));
                info!(
                    video_id = %stream.video_id,
                    next_check = ?wait,
                    "waiting for broadcast to start"
                );
                tokio::time::sleep(wait).await;
                continue 'outer;
            }

            let live_chat_id = stream
                .live_chat_id
                .context("live broadcast is missing a live chat id")?;

            // --- Attach phase: per-broadcast reconnect state ---
            // Reset per outer iteration: a page_token or dedupe set from
            // a finished broadcast must not leak into the next one.
            let mut page_token: Option<String> = None;
            let mut dedupe = BoundedIdSet::new(2048);
            let mut backoff = Duration::from_secs(2);

            'inner: loop {
                let mut request = Request::new(LiveChatMessageListRequest {
                    live_chat_id: Some(live_chat_id.clone()),
                    hl: None,
                    profile_image_size: None,
                    max_results: Some(20),
                    page_token: page_token.clone(),
                    part: vec!["id".into(), "snippet".into(), "authorDetails".into()],
                });

                // Asked on every reconnect: with OAuth this hands out the
                // cached token, refreshing it shortly before it expires.
                let (name, value) = self.auth.header().await?;
                let value: MetadataValue<_> = value
                    .parse()
                    .context("credentials contain characters invalid in a header")?;
                request.metadata_mut().insert(name, value);

                match consume_stream(
                    &mut client,
                    request,
                    &mut page_token,
                    &mut dedupe,
                    &self.emojis,
                    &tx,
                    &activity,
                )
                .await
                {
                    Ok((n, StreamEnd::Eof)) => {
                        if n > 0 {
                            backoff = Duration::from_secs(2); // stream was healthy
                        }
                        // The ~10s server EOF: routine, so debug level only.
                        debug!(
                            page_token_set = page_token.is_some(),
                            ?backoff,
                            "stream ended, reconnecting"
                        );
                    }
                    Ok((_, StreamEnd::Offline)) => {
                        if is_own_broadcast {
                            activity.set(Activity::Idle(
                                "broadcast ended; waiting for your next one".into(),
                            ));
                            info!("broadcast ended; back to scan mode");
                            tokio::time::sleep(SETTLE_AFTER_END).await;
                            first_scan = true; // re-list all broadcasts next scan
                            break 'inner;
                        }
                        // One-shot Video target: normal completion.
                        info!("stream offline, done");
                        return Ok(());
                    }
                    Ok((_, StreamEnd::ReceiverGone)) => {
                        return Ok(()); // server shutting down
                    }
                    Err(status) => match quota::classify(&status) {
                        Some(Exhaustion::Daily) => {
                            quota::wait_for_reset(status.message(), &activity).await;
                            // Hours may have passed: find the broadcast anew.
                            first_scan = true;
                            continue 'outer;
                        }
                        Some(Exhaustion::RateLimit) => {
                            backoff = backoff.max(RATE_LIMIT_BACKOFF);
                            activity.set(Activity::Degraded(format!(
                                "rate limited by YouTube, retrying in {} s",
                                backoff.as_secs()
                            )));
                            warn!(%status, ?backoff, "rate limited by YouTube, reconnecting");
                        }
                        None if is_fatal(&status) => {
                            return Err(status).context("YouTube gRPC stream failed");
                        }
                        None => {
                            activity.set(Activity::Degraded(format!(
                                "connection problem, reconnecting: {}",
                                status.message()
                            )));
                            warn!(%status, ?backoff, "stream error, reconnecting");
                        }
                    },
                }

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            // Fell out of 'inner: for OwnBroadcast the broadcast is over,
            // so 'outer re-resolves (the next one, or nothing).
        }
    }
}

impl YouTubeSource {
    /// Finds the chat to attach to. `Ok(None)`: nothing live or scheduled
    /// (only for `OwnBroadcast`).
    async fn resolve(
        &self,
        http: &reqwest::Client,
        first_scan: &mut bool,
    ) -> anyhow::Result<Option<ResolvedStream>> {
        match &self.target {
            YouTubeTarget::Video(video_id) => {
                let live_chat_id =
                    resolver::resolve_live_chat_id(http, &self.auth, video_id).await?;
                Ok(Some(ResolvedStream {
                    video_id: video_id.clone(),
                    live_chat_id: Some(live_chat_id),
                    is_live: true,
                    scheduled_start_time: None,
                }))
            }
            YouTubeTarget::OwnBroadcast => {
                let resolved =
                    resolver::resolve_own_broadcast(http, &self.auth, *first_scan).await?;
                *first_scan = false;
                Ok(resolved)
            }
        }
    }
}

/// How long to wait before re-checking a broadcast that is not live yet:
/// short when its start time is near (or already past but the chat has not
/// come up), long otherwise.
fn scan_interval(start: Option<chrono::DateTime<chrono::Utc>>) -> Duration {
    match start {
        Some(t) if t - chrono::Utc::now() <= NEAR_WINDOW => POLL_NEAR,
        _ => POLL_FAR,
    }
}

/// Runs one streamList stream until it ends or errors. Returns the number
/// of events forwarded so the caller can tell healthy streams from
/// immediately-dead ones. `page_token` is updated as responses arrive.
// `Status` is large (176 bytes), which clippy flags; boxing it isn't
// worth it on this cold path.
#[allow(clippy::result_large_err)]
async fn consume_stream(
    client: &mut V3DataLiveChatMessageServiceClient<Channel>,
    request: Request<LiveChatMessageListRequest>,
    page_token: &mut Option<String>,
    dedupe: &mut BoundedIdSet,
    emojis: &EmojiMap,
    tx: &mpsc::Sender<ChatEvent>,
    activity: &Reporter,
) -> Result<(usize, StreamEnd), Status> {
    let mut stream = client.stream_list(request).await?.into_inner();
    // Attached to the chat. (The routine ~10 s disconnects don't change
    // this: the reconnect follows right away.)
    activity.set(Activity::Receiving);
    let mut count = 0;

    while let Some(response) = stream.message().await? {
        // The broadcast may be over, but this response can still carry
        // the last messages: record the flag, forward items, then stop.
        let offline = response.offline_at.is_some();

        if let Some(token) = response.next_page_token {
            *page_token = Some(token);
        }
        for item in response.items {
            let Some(event) = convert::convert(item, dedupe, emojis) else {
                continue;
            };
            count += 1;
            if tx.send(event).await.is_err() {
                return Ok((count, StreamEnd::ReceiverGone));
            }
        }

        if offline {
            return Ok((count, StreamEnd::Offline));
        }
    }
    Ok((count, StreamEnd::Eof))
}

/// Permanent failures: retrying cannot help. (`ResourceExhausted` is
/// handled before this, see `quota::classify`.) Transient failures (EOF bug,
/// network blips) fall through to the reconnect loop.
fn is_fatal(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::InvalidArgument
            | Code::NotFound
            | Code::PermissionDenied
            | Code::Unauthenticated
            | Code::Unimplemented
            | Code::FailedPrecondition // live chat closed or invalid id!
    )
}
