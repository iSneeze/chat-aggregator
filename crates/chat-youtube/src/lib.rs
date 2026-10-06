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

/// How long a stream may stay completely silent (no response at all, not
/// even an empty one) before we assume the connection died without telling
/// us: Wi-Fi switch, router dropping the connection, laptop sleep. TCP can't
/// tell "quiet" from "gone" on its own, so without this the read would wait
/// forever while the status light stays green. Healthy streams end after
/// ~10 s (the EOF bug), far below this; should Google fix that bug, a quiet
/// chat just means a reconnect per minute of silence.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Opening the connection; without a limit a dead network can hold the
/// attempt for minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// Nothing at all arrived for `STALL_TIMEOUT`: the connection is
    /// probably dead. Resume with page_token over a fresh connection.
    Stalled,
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

        // One HTTP/2 channel, reused by every broadcast and reconnect; only
        // replaced when it stalls (see `StreamEnd::Stalled`).
        let mut client = connect().await?;
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
                    STALL_TIMEOUT,
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
                    Ok((_, StreamEnd::Stalled)) => {
                        activity.set(Activity::Degraded(
                            "no answer from YouTube, reconnecting".into(),
                        ));
                        warn!(after = ?STALL_TIMEOUT, "YouTube stream went silent, reconnecting");
                        // Retrying on the same channel could hang again:
                        // tonic doesn't know its connection is dead either.
                        // Dropping the old client closes it. If the new
                        // connection fails too, the next round tries again
                        // (the backoff below grows with each attempt).
                        match connect().await {
                            Ok(fresh) => client = fresh,
                            Err(e) => warn!("reconnecting to YouTube failed: {e:#}"),
                        }
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

/// Opens the HTTP/2 connection to YouTube's gRPC endpoint.
async fn connect() -> anyhow::Result<V3DataLiveChatMessageServiceClient<Channel>> {
    let channel = Endpoint::from_shared(ENDPOINT.to_string())?
        .tls_config(ClientTlsConfig::new().with_webpki_roots())?
        .connect_timeout(CONNECT_TIMEOUT)
        // Lets the operating system notice a dead connection by itself
        // too, as a backup to `STALL_TIMEOUT`. (No HTTP/2 pings: gRPC
        // servers may hang up on clients that ping more often than they
        // allow, and Google doesn't say how often that is.)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .connect()
        .await
        .context("gRPC connect to YouTube failed")?;
    Ok(V3DataLiveChatMessageServiceClient::new(channel))
}

/// Runs one streamList stream until it ends or errors. Returns the number
/// of events forwarded so the caller can tell healthy streams from
/// immediately-dead ones. `page_token` is updated as responses arrive.
/// `stall_after`: see `STALL_TIMEOUT` (a parameter so tests can be quick).
// `Status` is large (176 bytes), which clippy flags; boxing it isn't
// worth it on this cold path.
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
async fn consume_stream(
    client: &mut V3DataLiveChatMessageServiceClient<Channel>,
    request: Request<LiveChatMessageListRequest>,
    page_token: &mut Option<String>,
    dedupe: &mut BoundedIdSet,
    emojis: &EmojiMap,
    tx: &mpsc::Sender<ChatEvent>,
    activity: &Reporter,
    stall_after: Duration,
) -> Result<(usize, StreamEnd), Status> {
    use tokio::time::timeout;

    // Both waits can hang on a dead connection: for the stream to open,
    // and for each response. `timeout` gives `Err(Elapsed)` when time is
    // up, and `Ok(...)` with the inner result otherwise.
    let Ok(opened) = timeout(stall_after, client.stream_list(request)).await else {
        return Ok((0, StreamEnd::Stalled));
    };
    let mut stream = opened?.into_inner();
    // Attached to the chat. (The routine ~10 s disconnects don't change
    // this: the reconnect follows right away.)
    activity.set(Activity::Receiving);
    let mut count = 0;

    loop {
        let Ok(next) = timeout(stall_after, stream.message()).await else {
            return Ok((count, StreamEnd::Stalled));
        };
        let Some(response) = next? else {
            break;
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Runs `consume_stream` against `addr` with a short stall limit, and
    /// fails the test if it doesn't come back on its own.
    async fn consume_from(addr: std::net::SocketAddr) -> (usize, StreamEnd) {
        // `connect_lazy`: the connection is made on the first request, i.e.
        // inside `consume_stream`, like a reconnect on an existing channel.
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect_lazy();
        let mut client = V3DataLiveChatMessageServiceClient::new(channel);
        let (tx, _rx) = mpsc::channel(1);
        // Named, not written inline as `&mut None`: the future borrows
        // them until it's awaited below, so they must outlive this
        // statement (inline temporaries would be dropped at its `;`).
        let (mut page_token, mut dedupe) = (None, BoundedIdSet::new(16));
        let (emojis, activity) = (EmojiMap::default(), Reporter::detached());
        let consume = consume_stream(
            &mut client,
            Request::new(LiveChatMessageListRequest::default()),
            &mut page_token,
            &mut dedupe,
            &emojis,
            &tx,
            &activity,
            Duration::from_millis(300),
        );
        tokio::time::timeout(Duration::from_secs(5), consume)
            .await
            .expect("consume_stream hung on a silent connection")
            .expect("a stall is not an error")
    }

    #[tokio::test]
    async fn a_server_that_never_answers_is_a_stall() {
        // Accepts connections and keeps them open without ever sending a
        // byte: what a connection that died silently looks like from here.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        assert!(matches!(consume_from(addr).await, (0, StreamEnd::Stalled)));
    }

    #[tokio::test]
    async fn a_stream_that_goes_silent_is_a_stall() {
        // A gRPC server that opens the stream (status 200, gRPC headers),
        // then never sends a response: the stall happens mid-stream.
        use axum::body::{Body, Bytes};
        let silent = || async {
            let body = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
            axum::response::Response::builder()
                .header("content-type", "application/grpc")
                .body(Body::from_stream(body))
                .unwrap()
        };
        let router = axum::Router::new().fallback(silent);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        assert!(matches!(consume_from(addr).await, (0, StreamEnd::Stalled)));
    }
}
