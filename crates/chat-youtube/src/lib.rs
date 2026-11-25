//! YouTube live chat source.
//!
//! Two resolution paths (see `resolver`):
//! - `YouTubeTarget::Video`: a given video, with an API key (public
//!   videos, the testing path) or a login
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
pub use crate::resolver::{Auth, OwnChannel, own_channel, rest_client};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use chat_core::{Activity, BoundedIdSet, ChatEvent, ChatSource, Reporter};
use tokio::sync::mpsc;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};
use tracing::{debug, info, warn};

mod convert;
mod emoji;
mod emoji_cache;
pub mod oauth;
mod pb;
mod quota;
mod resolver;
pub mod web_chat;

use emoji_cache::{BroadcastEmojis, EmojiCache, MemberBadgeCache};
use oauth::TokenProvider;
use pb::LiveChatMessageListRequest;
use pb::v3_data_live_chat_message_service_client::V3DataLiveChatMessageServiceClient;
use quota::Exhaustion;
pub use quota::QuotaExhausted;
use resolver::{Broadcast, ResolvedStream};

const ENDPOINT: &str = "https://youtube.googleapis.com:443";

/// Scan-mode cadence: how often to re-resolve while waiting for a broadcast
/// to go live. Short once the scheduled start is within `NEAR_WINDOW` (or
/// past, but not stale), long while it's further away (cheap REST calls,
/// 1 unit each, unlike an idle gRPC stream).
const POLL_NEAR: Duration = Duration::from_secs(30);
const POLL_FAR: Duration = Duration::from_secs(5 * 60);
const NEAR_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_mins(5);

/// How often to check while nothing is live or coming up: that's also the
/// longest chat takes to appear after going live without scheduling (just
/// "Start streaming" in OBS). 2 min = 720 quota units a day (user's choice,
/// 2026-10-07: 5 min was too slow to start, 1 min too costly).
const POLL_IDLE: Duration = Duration::from_secs(2 * 60);

/// Waits between reconnects grow from 2 s up to this while connections end
/// without delivering anything (a quiet chat, or a problem), and drop back to
/// 2 s once messages flow. Slowing down saves quota (5 units per reconnect)
/// in quiet stretches; the cap bounds how late the first message after a
/// quiet stretch can be (user's choice, 2026-10-07: 30 s felt too long).
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(20);
const FIRST_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// After a broadcast ends, pause before re-resolving so that
/// `liveBroadcasts.list` has moved it to `complete`. Resolving while it
/// still reads as `live` would re-attach to the closed chat, which the
/// server rejects with `FailedPrecondition` (handled as another "ended").
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

/// What to read, and what authenticates reading it. The two belong
/// together: your own broadcasts can only be found with your login, while
/// a given video also works with an API key. So each variant carries what
/// it accepts, and "your broadcasts, with an API key" can't be built.
pub enum YouTubeTarget {
    /// Watch a specific video: a public one with an API key, or one only
    /// some account may see (members-only) with that account's login.
    Video { id: String, auth: Auth },
    /// Watch the logged-in creator's own current/next broadcast (sees
    /// unlisted and members-only streams).
    OwnBroadcast { login: TokenProvider },
}

impl YouTubeTarget {
    /// The header that authenticates a call for this target, as (name,
    /// value). Asked for on every call: a login's access token is renewed
    /// shortly before it expires.
    async fn header(&self) -> anyhow::Result<(&'static str, String)> {
        match self {
            YouTubeTarget::Video { auth, .. } => auth.header().await,
            YouTubeTarget::OwnBroadcast { login } => resolver::bearer(login).await,
        }
    }
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
    /// Custom emoji from the browser export; `EmojiMap::default()` for none.
    pub emojis: EmojiMap,
    /// The folder of the per-channel emoji caches (see `emoji_cache`);
    /// `None`: no cache.
    pub emoji_cache: Option<PathBuf>,
    /// Learn custom emoji from YouTube's web chat while reading a
    /// broadcast (see `web_chat`).
    pub learn_emojis: bool,
}

impl ChatSource for YouTubeSource {
    async fn run(self, tx: mpsc::Sender<ChatEvent>, activity: Reporter) -> anyhow::Result<()> {
        activity.set(Activity::Connecting);
        // Before the resolution loop, so we don't move self.target twice.
        let is_own_broadcast = matches!(self.target, YouTubeTarget::OwnBroadcast { .. });

        // One HTTP/2 channel, reused by every broadcast and reconnect; only
        // replaced when it stalls (see `StreamEnd::Stalled`).
        let mut client = connect().await?;
        // Same idea for REST: one client = one connection pool, reused by
        // every resolve instead of a fresh TLS handshake per call.
        let http = rest_client();
        let mut first_scan = true;

        // `outer`: resolve a broadcast, attach, and — for OwnBroadcast —
        // come back here to scan for the next one once it ends.
        'outer: loop {
            // --- Resolve phase (REST, 1 quota unit) ---
            let stream = match self.resolve(&http, &mut first_scan).await {
                Ok(Some(stream)) => stream,
                Ok(None) => {
                    activity.set(Activity::Idle(format!(
                        "no live or upcoming broadcast on your channel; checking every {} min",
                        POLL_IDLE.as_secs() / 60
                    )));
                    info!(next_check = ?POLL_IDLE, "nothing live or upcoming");
                    tokio::time::sleep(POLL_IDLE).await;
                    continue 'outer;
                }
                // Every call fails until the daily reset: wait for it
                // instead of ending the source.
                Err(e) if chat_core::has_error::<QuotaExhausted>(&e) => {
                    quota::wait_for_reset(&format!("{e:#}"), &activity).await;
                    continue 'outer;
                }
                Err(e) => return Err(e),
            };

            // Only a live broadcast with its chat on goes past this
            // `match`; the other two states wait and resolve again.
            // (`stream.state` moves out here; the other fields of `stream`
            // stay usable, since Rust tracks moves per field.)
            let live_chat_id = match stream.state {
                // A scheduled broadcast has no chat yet: idle cheaply by
                // polling, faster as the start time approaches.
                Broadcast::Upcoming { start } => {
                    let wait = scan_interval(start);
                    let now = jiff::Zoned::now();
                    let when = start_label(&start.to_zoned(now.time_zone().clone()), now.date());
                    activity.set(Activity::Idle(format!(
                        "waiting for your broadcast to start (scheduled for {when})"
                    )));
                    info!(
                        video_id = %stream.video_id,
                        next_check = ?wait,
                        "waiting for broadcast to start"
                    );
                    tokio::time::sleep(wait).await;
                    continue 'outer;
                }
                // Live, but with chat switched off: nothing to read. Not an
                // error: check again later, the chat may be switched on.
                Broadcast::Live { chat: None } => {
                    activity.set(Activity::Idle(format!(
                        "your broadcast is live, but its chat is off; checking again in {} min",
                        POLL_IDLE.as_secs() / 60
                    )));
                    info!(video_id = %stream.video_id, "live broadcast without a chat");
                    tokio::time::sleep(POLL_IDLE).await;
                    continue 'outer;
                }
                Broadcast::Live { chat: Some(id) } => id,
            };

            // Dropped when this broadcast's chat is left, for whatever
            // reason: that stops its web chat reader and saves its cache.
            let reader = self
                .learn_emojis
                .then(|| web_chat::Reader::start(http.clone(), stream.video_id.clone()));
            let mut emojis = self.broadcast_emojis(stream.channel_id.as_deref(), reader);

            // --- Attach phase: per-broadcast reconnect state ---
            // Reset per outer iteration: a page_token or dedupe set from
            // a finished broadcast must not leak into the next one.
            let mut page_token: Option<String> = None;
            let mut dedupe = BoundedIdSet::new(2048);
            let mut backoff = FIRST_RECONNECT_BACKOFF;

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
                let (name, value) = self.target.header().await?;
                let value: MetadataValue<_> = value
                    .parse()
                    .context("credentials contain characters invalid in a header")?;
                request.metadata_mut().insert(name, value);

                match consume_stream(
                    &mut client,
                    request,
                    &mut page_token,
                    &mut dedupe,
                    &mut emojis,
                    &tx,
                    &activity,
                    STALL_TIMEOUT,
                )
                .await
                {
                    Ok((n, StreamEnd::Eof)) => {
                        if n > 0 {
                            backoff = FIRST_RECONNECT_BACKOFF; // messages flow
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
                            broadcast_ended(&activity).await;
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
                    Err(status) => match on_error(&status, is_own_broadcast) {
                        OnError::WaitForQuota => {
                            quota::wait_for_reset(status.message(), &activity).await;
                            // Hours may have passed: find the broadcast anew.
                            first_scan = true;
                            continue 'outer;
                        }
                        OnError::RateLimited => {
                            backoff = backoff.max(RATE_LIMIT_BACKOFF);
                            activity.set(Activity::Degraded(format!(
                                "rate limited by YouTube, retrying in {} s",
                                backoff.as_secs()
                            )));
                            warn!(%status, ?backoff, "rate limited by YouTube, reconnecting");
                        }
                        OnError::BroadcastEnded => {
                            debug!(%status, "chat closed");
                            broadcast_ended(&activity).await;
                            first_scan = true;
                            break 'inner;
                        }
                        OnError::Fatal => {
                            return Err(status).context("YouTube gRPC stream failed");
                        }
                        OnError::Retry => {
                            activity.set(Activity::Degraded(format!(
                                "connection problem, reconnecting: {}",
                                status.message()
                            )));
                            warn!(%status, ?backoff, "stream error, reconnecting");
                        }
                    },
                }

                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff);
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
            YouTubeTarget::Video { id, auth } => {
                Ok(Some(resolver::resolve_video(http, auth, id).await?))
            }
            YouTubeTarget::OwnBroadcast { login } => {
                let resolved = resolver::resolve_own_broadcast(http, login, *first_scan).await?;
                *first_scan = false;
                Ok(resolved)
            }
        }
    }

    /// The emoji for a broadcast on `channel_id`: the imported ones plus
    /// what that channel's cache has learned, and later what `reader`
    /// learns. The cache wins where both know a code: it's newer than the
    /// export.
    fn broadcast_emojis(
        &self,
        channel_id: Option<&str>,
        reader: Option<web_chat::Reader>,
    ) -> BroadcastEmojis {
        let cache = self
            .emoji_cache
            .as_deref()
            .zip(channel_id)
            .and_then(|(folder, channel)| EmojiCache::open(folder, channel));
        let badge_cache = self
            .emoji_cache
            .as_deref()
            .zip(channel_id)
            .and_then(|(folder, channel)| MemberBadgeCache::open(folder, channel));
        let map = match &cache {
            Some(cache) => self.emojis.merged(cache.emojis()),
            None => self.emojis.clone(),
        };
        BroadcastEmojis::new(map, cache, badge_cache, reader)
    }
}

/// How long to wait before re-checking a broadcast that is not live yet:
/// short when its start time is near (or already past but the chat has not
/// come up), long otherwise. (Stale ones never get here, see
/// `resolver::STALE_AFTER`.)
fn scan_interval(start: jiff::Timestamp) -> Duration {
    if start.duration_since(jiff::Timestamp::now()) <= NEAR_WINDOW {
        POLL_NEAR
    } else {
        POLL_FAR
    }
}

/// A scheduled start for the status line: just the time if it's `today`,
/// otherwise with the day ("Thu 8 Oct 20:00"), so a stream tomorrow isn't
/// mistaken for one today. `start` carries its time zone (the app passes
/// local time, tests a fixed offset); `today` is a date in that same zone.
fn start_label(start: &jiff::Zoned, today: jiff::civil::Date) -> String {
    if start.date() == today {
        start.strftime("%H:%M").to_string()
    } else {
        start.strftime("%a %-d %b %H:%M").to_string()
    }
}

/// The next wait between reconnects: doubled, up to `MAX_RECONNECT_BACKOFF`.
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_RECONNECT_BACKOFF)
}

/// The broadcast is over: say so, give YouTube a moment to list it as
/// complete (see `SETTLE_AFTER_END`), then the caller scans for the next one.
async fn broadcast_ended(activity: &Reporter) {
    activity.set(Activity::Idle(
        "broadcast ended; waiting for your next one".into(),
    ));
    info!("broadcast ended; back to scan mode");
    tokio::time::sleep(SETTLE_AFTER_END).await;
}

/// What a failed streamList call means for the source.
#[derive(Debug, PartialEq)]
enum OnError {
    /// The daily quota is used up: wait for the reset.
    WaitForQuota,
    /// Too many requests: back off a while, then reconnect.
    RateLimited,
    /// The chat is closed: your broadcast is over, scan for the next one.
    BroadcastEnded,
    /// Retrying can't help: end the source with the error.
    Fatal,
    /// Probably passing (network blip): reconnect after the usual wait.
    Retry,
}

fn on_error(status: &Status, is_own_broadcast: bool) -> OnError {
    match quota::classify(status) {
        Some(Exhaustion::Daily) => return OnError::WaitForQuota,
        Some(Exhaustion::RateLimit) => return OnError::RateLimited,
        None => {}
    }
    // "Live chat ended" arrives as `FailedPrecondition`. When following
    // your own broadcasts that's the normal end of a stream (often instead
    // of the `offline_at` marker, because of the ~10 s EOF bug): back to
    // waiting for the next one, not an error. A single watched video is
    // simply over then, like any other permanent failure.
    if is_own_broadcast && status.code() == Code::FailedPrecondition {
        return OnError::BroadcastEnded;
    }
    if is_fatal(status) {
        OnError::Fatal
    } else {
        OnError::Retry
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
    emojis: &mut BroadcastEmojis,
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
        emojis.take_learned();
        for item in response.items {
            let Some(event) = convert::convert(item, dedupe, emojis.map(), emojis.member_badges())
            else {
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
            | Code::FailedPrecondition // live chat closed (see `on_error`)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn a_closed_chat_ends_your_broadcast_but_not_the_source() {
        let closed = Status::failed_precondition("The live chat is no longer live.");
        assert_eq!(on_error(&closed, true), OnError::BroadcastEnded);
        assert_eq!(
            on_error(&closed, false),
            OnError::Fatal,
            "a watched video is over"
        );

        let daily = Status::resource_exhausted("you have exceeded your quota");
        assert_eq!(on_error(&daily, true), OnError::WaitForQuota);
        let per_minute = Status::resource_exhausted("Queries per minute");
        assert_eq!(on_error(&per_minute, true), OnError::RateLimited);
        assert_eq!(on_error(&Status::not_found("x"), true), OnError::Fatal);
        assert_eq!(on_error(&Status::unavailable("blip"), true), OnError::Retry);
    }

    #[test]
    fn a_broadcast_gets_the_import_plus_its_channels_cache() {
        let folder =
            std::env::temp_dir().join(format!("chat-youtube-emojis-for-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("UC1.json"),
            r#"{ "version": 1, "entries": [
                { "code": ":_hype:", "url": "https://yt3.ggpht.com/hype-v2" },
                { "code": ":_wave:", "url": "https://yt3.ggpht.com/wave" }
            ] }"#,
        )
        .unwrap();
        let source = YouTubeSource {
            // Which target doesn't matter here: no call is made.
            target: YouTubeTarget::Video {
                id: "abc123".into(),
                auth: Auth::ApiKey(String::new()),
            },
            emojis: EmojiMap::from_json(
                r#"{ "version": 1, "entries": [
                    { "code": ":_hype:", "url": "https://yt3.ggpht.com/hype" },
                    { "code": ":yt:", "url": "https://yt3.ggpht.com/yt" }
                ] }"#,
            )
            .unwrap(),
            emoji_cache: Some(folder.clone()),
            learn_emojis: false,
        };
        let urls = |map: &EmojiMap| -> Vec<String> {
            map.find(":_hype: :_wave: :yt:")
                .emotes
                .into_iter()
                .map(|e| e.url)
                .collect()
        };

        let emojis = source.broadcast_emojis(Some("UC1"), None);
        assert_eq!(
            urls(emojis.map()),
            [
                "https://yt3.ggpht.com/hype-v2", // the cache is newer
                "https://yt3.ggpht.com/wave",
                "https://yt3.ggpht.com/yt",
            ]
        );
        // Another channel, or none known: just the import.
        assert_eq!(
            urls(source.broadcast_emojis(Some("UC2"), None).map()).len(),
            2
        );
        assert_eq!(urls(source.broadcast_emojis(None, None).map()).len(), 2);
        std::fs::remove_dir_all(&folder).unwrap();
    }

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
        let (mut emojis, activity) = (
            BroadcastEmojis::fixed(EmojiMap::default()),
            Reporter::detached(),
        );
        let consume = consume_stream(
            &mut client,
            Request::new(LiveChatMessageListRequest::default()),
            &mut page_token,
            &mut dedupe,
            &mut emojis,
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
