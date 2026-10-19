//! Profile pictures for the overlay, downloaded by this server instead of
//! by every browser.
//!
//! YouTube's image server rate-limits (`429 Too Many Requests`) per IP
//! address. With the overlay loading avatars straight from Google, every
//! browser on the streamer's connection (OBS, a test tab, YouTube's own
//! chat) asked for every avatar, and a refused one was asked for again with
//! each new message from that chatter: on a busy chat, avatars went missing.
//! Here each avatar is downloaded once and kept in memory; all overlays get
//! it from `/avatar/<key>`.
//!
//! The server never downloads a URL named in a request: `<key>` is only
//! looked up, and only URLs that arrived in chat events and point at
//! Google's image hosts are ever registered. Otherwise any page or program
//! that can reach the server could make it fetch whatever it likes.
//!
//! Only the overlay sees the local URLs: the JSON API keeps the platform's
//! own `avatar_url` (it's part of the public API).

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chat_core::ChatMessage;
use reqwest::Url;
use tokio::sync::{OnceCell, Semaphore};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::{NO_SNIFF, ServerState};

/// How many avatars are kept. At about 5 kB each, the worst case is around
/// 25 MB; the oldest is dropped first.
const CAPACITY: usize = 5000;

/// How long an avatar is kept: as long as Google's own `cache-control`
/// allows. Browsers get the same `max-age` from us.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Downloads running at once: evens out bursts (an overlay reload brings the
/// whole history at once).
const PARALLEL_DOWNLOADS: usize = 4;

/// Avatars are ~5 kB; anything this big isn't an avatar.
const MAX_BYTES: usize = 1024 * 1024;

/// How long a browser's request waits through rate-limit pauses before it
/// gets `503`. An avatar that shows up late is better than none.
const MAX_WAIT: Duration = Duration::from_secs(15);

/// The first pause after a `429` without `Retry-After`; doubled with each
/// further one, up to `MAX_PAUSE`.
const FIRST_PAUSE: Duration = Duration::from_secs(2);
const MAX_PAUSE: Duration = Duration::from_secs(30);

/// The only image types passed on. Not SVG: it can contain script.
const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/gif"];

pub struct Avatars {
    entries: Mutex<Entries>,
    client: reqwest::Client,
    downloads: Semaphore,
    /// Google's limit is per IP address, not per image: after a `429`, all
    /// downloads wait.
    cooldown: Mutex<Cooldown>,
    /// Which URLs may be downloaded: Google's image hosts. Tests add their
    /// local fake server.
    allowed: fn(&Url) -> bool,
}

struct Entries {
    by_key: HashMap<String, Arc<Entry>>,
    /// Keys from oldest to newest, for dropping the oldest. Entries never
    /// move, so this is also the order of `Entry::added`.
    order: VecDeque<String>,
    cap: usize,
}

struct Entry {
    url: Url,
    added: Instant,
    /// Filled by the first successful download. A failed one leaves it
    /// empty, so the next request tries again; requests arriving during a
    /// download wait for that one instead of starting their own.
    image: OnceCell<Image>,
}

/// Cheap to clone: `Bytes` is reference counted.
#[derive(Clone)]
struct Image {
    bytes: Bytes,
    content_type: &'static str,
}

struct Cooldown {
    until: Option<Instant>,
    next_pause: Duration,
}

#[derive(Debug)]
enum FetchError {
    /// Google's rate limit (`429`), with its `Retry-After` if it sent one.
    RateLimited(Option<Duration>),
    /// Anything else: network, another status, not an image, too big.
    Failed(String),
}

impl Default for Avatars {
    fn default() -> Self {
        Self::new()
    }
}

impl Avatars {
    pub fn new() -> Self {
        Self::build(google_image_host, CAPACITY)
    }

    /// Also allows `http://127.0.0.1`, where tests run their fake Google.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::build(
            |url| {
                google_image_host(url)
                    || (url.scheme() == "http" && url.host_str() == Some("127.0.0.1"))
            },
            CAPACITY,
        )
    }

    fn build(allowed: fn(&Url) -> bool, cap: usize) -> Self {
        let client = reqwest::Client::builder()
            // Google's answer must not send us anywhere else.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            // Fails only if the TLS backend can't start, which is also
            // when `reqwest::Client::new()` panics.
            .expect("HTTP client");
        Self {
            entries: Mutex::new(Entries {
                by_key: HashMap::new(),
                order: VecDeque::new(),
                cap,
            }),
            client,
            downloads: Semaphore::new(PARALLEL_DOWNLOADS),
            cooldown: Mutex::new(Cooldown {
                until: None,
                next_pause: FIRST_PAUSE,
            }),
            allowed,
        }
    }

    /// `msg` as the overlay should render it: a Google avatar replaced by
    /// its local URL. Messages without one aren't copied (`Cow::Borrowed`).
    pub(crate) fn for_overlay<'a>(&self, msg: &'a ChatMessage) -> Cow<'a, ChatMessage> {
        let Some(key) = msg
            .author
            .avatar_url
            .as_deref()
            .and_then(|url| self.register(url))
        else {
            return Cow::Borrowed(msg);
        };
        let mut msg = msg.clone();
        // Relative, like the page's other URLs (`events`, `theme/…`).
        msg.author.avatar_url = Some(format!("avatar/{key}"));
        Cow::Owned(msg)
    }

    /// Remembers `url` if it may be downloaded; returns its key.
    fn register(&self, url: &str) -> Option<String> {
        let parsed = Url::parse(url).ok().filter(|u| (self.allowed)(u))?;
        let key = key_for(url);
        let mut entries = self.lock_entries();
        entries.drop_expired(Instant::now());
        if !entries.by_key.contains_key(&key) {
            entries.insert(key.clone(), parsed);
        }
        Some(key)
    }

    fn entry(&self, key: &str) -> Option<Arc<Entry>> {
        let mut entries = self.lock_entries();
        entries.drop_expired(Instant::now());
        // A clone of the `Arc`, so the lock is released before anyone
        // waits for the download.
        entries.by_key.get(key).cloned()
    }

    async fn image(&self, entry: &Entry) -> Result<Image, FetchError> {
        entry
            .image
            .get_or_try_init(|| self.download_patiently(&entry.url))
            .await
            .cloned()
    }

    /// Downloads `url`, waiting through rate-limit pauses for up to
    /// `MAX_WAIT`.
    async fn download_patiently(&self, url: &Url) -> Result<Image, FetchError> {
        let give_up = Instant::now() + MAX_WAIT;
        loop {
            if let Some(until) = self.paused_until() {
                if until > give_up {
                    return Err(FetchError::RateLimited(None));
                }
                tokio::time::sleep_until(until).await;
            }
            // Never closed, so `acquire` can't fail.
            let _permit = self.downloads.acquire().await.expect("semaphore open");
            match self.download(url).await {
                Err(FetchError::RateLimited(retry_after)) => self.rate_limited(retry_after),
                result => {
                    if result.is_ok() {
                        self.lock_cooldown().next_pause = FIRST_PAUSE;
                    }
                    return result;
                }
            }
        }
    }

    async fn download(&self, url: &Url) -> Result<Image, FetchError> {
        let failed = |e: reqwest::Error| FetchError::Failed(e.to_string());
        let mut response = self.client.get(url.clone()).send().await.map_err(failed)?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(FetchError::RateLimited(retry_after(response.headers())));
        }
        if !status.is_success() {
            return Err(FetchError::Failed(format!("status {status}")));
        }
        let content_type = image_type(response.headers().get(header::CONTENT_TYPE))
            .ok_or_else(|| FetchError::Failed("not an image".into()))?;
        let too_big = || FetchError::Failed(format!("larger than {MAX_BYTES} bytes"));
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BYTES as u64)
        {
            return Err(too_big());
        }
        // Read piece by piece: a missing or wrong `content-length` must not
        // let a huge body into memory.
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(too_big());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Image {
            bytes: Bytes::from(bytes),
            content_type,
        })
    }

    fn paused_until(&self) -> Option<Instant> {
        self.lock_cooldown()
            .until
            .filter(|until| *until > Instant::now())
    }

    /// Pauses all downloads: for Google's `Retry-After`, or for a pause
    /// that doubles each time. Logged once per pause, not per avatar.
    fn rate_limited(&self, retry_after: Option<Duration>) {
        let mut cooldown = self.lock_cooldown();
        let now = Instant::now();
        let pause = retry_after.unwrap_or(cooldown.next_pause).min(MAX_PAUSE);
        cooldown.next_pause = (cooldown.next_pause * 2).min(MAX_PAUSE);
        let was_paused = cooldown.until.is_some_and(|until| until > now);
        cooldown.until = Some(cooldown.until.map_or(now + pause, |u| u.max(now + pause)));
        if !was_paused {
            warn!(
                ?pause,
                "YouTube's image server is rate limiting avatars, pausing downloads"
            );
        }
    }

    fn lock_entries(&self) -> MutexGuard<'_, Entries> {
        // See `Hub::lock_history`: both maps stay valid even if a thread
        // panicked while holding the lock.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_cooldown(&self) -> MutexGuard<'_, Cooldown> {
        self.cooldown.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Entries {
    fn insert(&mut self, key: String, url: Url) {
        let entry = Entry {
            url,
            added: Instant::now(),
            image: OnceCell::new(),
        };
        self.by_key.insert(key.clone(), Arc::new(entry));
        self.order.push_back(key);
        while self.order.len() > self.cap {
            self.drop_oldest();
        }
    }

    /// Drops avatars older than `MAX_AGE`. They're the oldest, so they're
    /// at the front.
    fn drop_expired(&mut self, now: Instant) {
        while let Some(oldest) = self.order.front() {
            let fresh = self
                .by_key
                .get(oldest)
                .is_some_and(|entry| now.duration_since(entry.added) < MAX_AGE);
            if fresh {
                break;
            }
            self.drop_oldest();
        }
    }

    fn drop_oldest(&mut self) {
        if let Some(key) = self.order.pop_front() {
            self.by_key.remove(&key);
        }
    }
}

/// `GET /avatar/<key>`: an avatar registered by `for_overlay`, downloaded
/// on first request.
pub(crate) async fn avatar(
    State(state): State<ServerState>,
    UrlPath(key): UrlPath<String>,
) -> Response {
    // Unknown keys are only "not found": nothing is downloaded for them.
    let Some(entry) = state.avatars.entry(&key) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state.avatars.image(&entry).await {
        Ok(image) => (
            [
                (header::CONTENT_TYPE, image.content_type),
                // The key comes from the URL, and a new avatar has a new
                // URL: browsers may keep this one as long as we do.
                (header::CACHE_CONTROL, "max-age=86400"),
                NO_SNIFF,
                (header::CONTENT_SECURITY_POLICY, "script-src 'none'"),
            ],
            image.bytes,
        )
            .into_response(),
        Err(FetchError::RateLimited(_)) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(FetchError::Failed(why)) => {
            // The key, not the URL: logs carry no chatter data.
            debug!(%key, "avatar download failed: {why}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// Where YouTube's profile pictures live, over https only.
fn google_image_host(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            host.ends_with(".ggpht.com") || host.ends_with(".googleusercontent.com")
        })
}

/// The same URL always gets the same key: across overlays, reconnects and
/// repeated messages.
fn key_for(url: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(url.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// `Retry-After` in seconds. (It may also be a date; Google sends seconds
/// if anything, and a missing value just means our own pause.)
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    seconds.trim().parse().ok().map(Duration::from_secs)
}

fn image_type(value: Option<&HeaderValue>) -> Option<&'static str> {
    // "image/jpeg; charset=…" → "image/jpeg"
    let value = value?.to_str().ok()?;
    let essence = value.split(';').next()?.trim().to_ascii_lowercase();
    IMAGE_TYPES.into_iter().find(|t| *t == essence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TestServer, WAIT, start};
    use axum::routing::get;
    use chat_core::{Author, ChatEvent, ChatPlatform, MessageKind};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    const GOOGLE_AVATAR: &str = "https://yt3.ggpht.com/ExampleAvatar0001=s88-c-k-no-rj";

    fn message_with_avatar(id: &str, avatar_url: &str) -> ChatMessage {
        ChatMessage {
            id: id.into(),
            platform: ChatPlatform::YouTube,
            author: Author {
                id: "UC1".into(),
                name: "Viewer42".into(),
                color: None,
                badges: vec![],
                avatar_url: Some(avatar_url.into()),
            },
            text: "hi".into(),
            emotes: vec![],
            timestamp: chrono::Utc::now(),
            kind: MessageKind::Text,
        }
    }

    /// A fake image server counting its requests; `respond` gets the number
    /// of the request (from 0).
    struct Upstream {
        url: String,
        hits: Arc<AtomicUsize>,
    }

    async fn upstream(respond: fn(usize) -> Response) -> Upstream {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = axum::Router::new().route(
            "/{*path}",
            get(move || async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                // Slow enough that simultaneous requests overlap.
                tokio::time::sleep(Duration::from_millis(100)).await;
                respond(n)
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Upstream {
            url: format!("http://{addr}/avatar.png"),
            hits,
        }
    }

    fn png(_: usize) -> Response {
        ([(header::CONTENT_TYPE, "image/png")], vec![1u8, 2, 3]).into_response()
    }

    /// Registers `url` like an overlay rendering a message would; returns
    /// the local URL.
    fn register(server: &TestServer, url: &str) -> String {
        let msg = message_with_avatar("m1", url);
        let local = server.avatars.for_overlay(&msg);
        format!(
            "http://{}/{}",
            server.addr,
            local.author.avatar_url.as_deref().unwrap()
        )
    }

    #[tokio::test]
    async fn overlay_gets_local_avatars_while_the_api_keeps_googles() {
        let server = start(None).await;
        server
            .hub
            .publish(ChatEvent::Message(message_with_avatar("m1", GOOGLE_AVATAR)));

        let mut resp = reqwest::get(format!("http://{}/events", server.addr))
            .await
            .unwrap();
        let mut seen = String::new();
        timeout(WAIT, async {
            while !seen.contains("msg__avatar") {
                let chunk = resp.chunk().await.unwrap().expect("stream ended early");
                seen.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no avatar in:\n{seen}"));

        let key = key_for(GOOGLE_AVATAR);
        // The template escapes the `/` (as `&#x2f;`); the browser reads
        // it back as `/`.
        assert!(seen.contains(r#"src="avatar"#), "{seen}");
        assert!(seen.contains(&format!(r#"{key}""#)), "{seen}");
        assert!(!seen.contains("ggpht"), "{seen}");

        // The API serves the hub's events: they still have Google's URL.
        let (history, _) = server.hub.subscribe();
        assert_eq!(history[0].author.avatar_url.as_deref(), Some(GOOGLE_AVATAR));
    }

    #[test]
    fn other_hosts_are_left_alone() {
        let avatars = Avatars::new();
        for url in [
            "https://example.com/avatar.png",
            "http://yt3.ggpht.com/no-https",
            "not a url",
        ] {
            let msg = message_with_avatar("m1", url);
            assert!(
                matches!(avatars.for_overlay(&msg), Cow::Borrowed(_)),
                "{url} must not be rewritten"
            );
        }
        assert!(avatars.lock_entries().by_key.is_empty());
    }

    #[test]
    fn only_googles_image_hosts_are_allowed() {
        let allowed = |url: &str| google_image_host(&Url::parse(url).unwrap());
        assert!(allowed("https://yt3.ggpht.com/a=s88"));
        assert!(allowed("https://yt4.ggpht.com/a=s88"));
        assert!(allowed("https://lh3.googleusercontent.com/a"));
        assert!(!allowed("http://yt3.ggpht.com/a"));
        assert!(!allowed("https://yt3.ggpht.com.evil.example/a"));
        assert!(!allowed("https://evil.example/yt3.ggpht.com"));
        assert!(!allowed("https://ggpht.com.evil.example/a"));
    }

    #[tokio::test]
    async fn unknown_key_is_not_found_and_nothing_is_downloaded() {
        let server = start(None).await;
        let upstream = upstream(png).await;
        // The key of an allowed URL, but no chat event ever brought it.
        let key = key_for(&upstream.url);
        let resp = reqwest::get(format!("http://{}/avatar/{key}", server.addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn simultaneous_requests_download_once() {
        let server = start(None).await;
        let upstream = upstream(png).await;
        let local = register(&server, &upstream.url);

        let (a, b) = tokio::join!(reqwest::get(&local), reqwest::get(&local));
        for resp in [a.unwrap(), b.unwrap()] {
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.headers()["content-type"], "image/png");
            assert_eq!(resp.headers()["cache-control"], "max-age=86400");
            assert_eq!(resp.bytes().await.unwrap().as_ref(), [1, 2, 3]);
        }
        // And later ones come from memory.
        reqwest::get(&local).await.unwrap();
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rate_limit_pauses_then_retries() {
        let server = start(None).await;
        let upstream = upstream(|n| {
            if n == 0 {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "1")],
                    "<html>slow down</html>",
                )
                    .into_response()
            } else {
                png(n)
            }
        })
        .await;
        let local = register(&server, &upstream.url);

        let resp = reqwest::get(&local).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn only_images_are_passed_on() {
        fn html(_: usize) -> Response {
            ([(header::CONTENT_TYPE, "text/html")], "<script>").into_response()
        }
        fn svg(_: usize) -> Response {
            ([(header::CONTENT_TYPE, "image/svg+xml")], "<svg/>").into_response()
        }
        fn huge(_: usize) -> Response {
            (
                [(header::CONTENT_TYPE, "image/png")],
                vec![0u8; MAX_BYTES + 1],
            )
                .into_response()
        }
        let server = start(None).await;
        for respond in [html as fn(usize) -> Response, svg, huge] {
            let upstream = upstream(respond).await;
            let local = register(&server, &upstream.url);
            let resp = reqwest::get(&local).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        }
    }

    #[test]
    fn oldest_avatars_are_dropped_when_full() {
        let avatars = Avatars::build(google_image_host, 2);
        let urls = [
            "https://yt3.ggpht.com/a",
            "https://yt3.ggpht.com/b",
            "https://yt3.ggpht.com/c",
        ];
        let keys: Vec<_> = urls
            .iter()
            .map(|url| avatars.register(url).unwrap())
            .collect();
        assert!(avatars.entry(&keys[0]).is_none(), "oldest dropped");
        assert!(avatars.entry(&keys[1]).is_some());
        assert!(avatars.entry(&keys[2]).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn avatars_expire_after_a_day() {
        let avatars = Avatars::new();
        let key = avatars.register(GOOGLE_AVATAR).unwrap();
        tokio::time::advance(MAX_AGE - Duration::from_secs(1)).await;
        assert!(avatars.entry(&key).is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(avatars.entry(&key).is_none());
    }
}
