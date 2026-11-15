//! Profile pictures (see RPLAY.md, "Pictures"). Every user's photo has a
//! fixed address, but most chatters have none, and then RPlay's image
//! server answers 403 (in the first live test, 12 of 26 chatters had
//! none). RPlay's page checks before it shows one, so the source does too:
//! a message only gets an avatar that exists, and the overlay never shows
//! a broken image. RPlay's user lookup doesn't say whether there's a photo
//! (seen 2026-11-15), so the check asks the image server itself.

use std::collections::HashMap;
use std::time::Duration;

use chat_core::ChatMessage;
use reqwest::StatusCode;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::api::is_object_id;
use crate::emotes::IMAGES;

/// How long a check may take. A new chatter's first message waits for it
/// (so chat stays in order); RPlay's image server usually answers within a
/// tenth of that.
const CHECK_TIMEOUT: Duration = Duration::from_secs(1);
/// Checks that fail in a row (no answer in time, an odd one) before
/// checking pauses: an image server that's down shouldn't hold up every
/// new chatter's first message by a second.
const FAILURES_BEFORE_PAUSE: u32 = 3;
const PAUSE: Duration = Duration::from_secs(10 * 60);
/// Chatters remembered per connection; past this the memory starts over,
/// which only costs a check per chatter again.
const KNOWN_MAX: usize = 5_000;

/// Where a user's profile photo would be, in the small size RPlay's chat
/// uses. `None` for an id that isn't one (it goes into a URL).
pub(crate) fn photo_url(user_id: &str) -> Option<String> {
    is_object_id(user_id).then(|| format!("{IMAGES}profilePhoto/{user_id}-small"))
}

/// Which chatters have a photo, as far as checked. One per connection:
/// each new connection (at least hourly) checks again, which also picks up
/// a photo that changed without an `updatedUserData`.
#[derive(Default)]
pub(crate) struct Avatars {
    /// By user id: whether their photo exists.
    known: HashMap<String, bool>,
    /// Checks that failed in a row.
    failures: u32,
    /// Checking paused (after failures) until then: no avatars meanwhile.
    paused_until: Option<Instant>,
}

impl Avatars {
    /// Keeps a message's avatar only if the photo exists, asking RPlay's
    /// image server the first time a sender comes up. A message without an
    /// avatar (a sender who hid their profile) is left as it is.
    pub(crate) async fn verify(&mut self, http: &reqwest::Client, message: &mut ChatMessage) {
        let Some(url) = message.author.avatar_url.as_deref() else {
            return;
        };
        if !self.exists(http, &message.author.id, url).await {
            message.author.avatar_url = None;
        }
    }

    /// The user changed something (`updatedUserData`), maybe their photo:
    /// check again at their next message.
    pub(crate) fn forget(&mut self, user_id: &str) {
        self.known.remove(user_id);
    }

    async fn exists(&mut self, http: &reqwest::Client, user_id: &str, url: &str) -> bool {
        if let Some(&known) = self.known.get(user_id) {
            return known;
        }
        if self
            .paused_until
            .is_some_and(|until| Instant::now() < until)
        {
            return false;
        }
        match check(http, url).await {
            Some(exists) => {
                if self.paused_until.take().is_some() {
                    info!("RPlay's image server answers again: profile pictures are back");
                }
                self.failures = 0;
                if self.known.len() >= KNOWN_MAX {
                    self.known.clear();
                }
                self.known.insert(user_id.to_owned(), exists);
                exists
            }
            // Not remembered: the next message of this chatter asks again
            // (unless checking is paused by then).
            None => {
                self.failures += 1;
                if self.failures >= FAILURES_BEFORE_PAUSE {
                    warn!(
                        "RPlay's image server didn't answer {FAILURES_BEFORE_PAUSE} times in a row: \
                         no profile pictures for {} minutes",
                        PAUSE.as_secs() / 60
                    );
                    self.failures = 0;
                    self.paused_until = Some(Instant::now() + PAUSE);
                }
                false
            }
        }
    }
}

/// Whether the photo at `url` exists: `Some(true)` if the image server has
/// it, `Some(false)` if it says there's none (403, S3's answer for a
/// missing file, or 404), `None` for anything else or no answer in time.
/// A HEAD request: only the answer, not the picture.
async fn check(http: &reqwest::Client, url: &str) -> Option<bool> {
    let response = tokio::time::timeout(CHECK_TIMEOUT, http.head(url).send())
        .await
        .ok()?
        .ok()?;
    match response.status() {
        status if status.is_success() => Some(true),
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chat_core::{Author, ChatPlatform, MessageKind};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const VIEWER: &str = "aaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn photo_urls_only_for_real_ids() {
        assert_eq!(
            photo_url(VIEWER).as_deref(),
            Some("https://pb3.rplay.live/profilePhoto/aaaaaaaaaaaaaaaaaaaaaaaa-small")
        );
        for odd in ["", "../emoticon", "aaaaaaaaaaaaaaaaaaaaaaa?", "Viewer42"] {
            assert_eq!(photo_url(odd), None, "{odd:?}");
        }
    }

    /// A stand-in for RPlay's image server: `/yes…` has a photo, `/no…`
    /// is 403, `/odd…` a 500, `/slow…` never answers. Counts requests.
    async fn image_server() -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0; 1024];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request.split_whitespace().nth(1).unwrap_or("");
                    let status = if path.starts_with("/yes") {
                        "200 OK"
                    } else if path.starts_with("/no") {
                        "403 Forbidden"
                    } else if path.starts_with("/odd") {
                        "500 Internal Server Error"
                    } else {
                        // `/slow`: holds the connection, never answers.
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        return;
                    };
                    let answer = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        (base, requests)
    }

    fn message(user_id: &str, avatar_url: Option<String>) -> ChatMessage {
        ChatMessage {
            id: "ExampleId001".into(),
            platform: ChatPlatform::Rplay,
            author: Author {
                id: user_id.into(),
                name: "Viewer42".into(),
                color: None,
                badges: Vec::new(),
                avatar_url,
            },
            text: "hi".into(),
            emotes: Vec::new(),
            timestamp: jiff::Timestamp::now(),
            kind: MessageKind::Text,
        }
    }

    async fn verified(
        avatars: &mut Avatars,
        http: &reqwest::Client,
        user_id: &str,
        url: &str,
    ) -> Option<String> {
        let mut message = message(user_id, Some(url.to_owned()));
        avatars.verify(http, &mut message).await;
        message.author.avatar_url
    }

    #[tokio::test]
    async fn only_photos_that_exist_are_kept_and_each_is_checked_once() {
        let (base, requests) = image_server().await;
        let http = reqwest::Client::new();
        let mut avatars = Avatars::default();

        let yes = format!("{base}/yes");
        let no = format!("{base}/no");
        assert_eq!(
            verified(&mut avatars, &http, "a", &yes).await,
            Some(yes.clone())
        );
        assert_eq!(verified(&mut avatars, &http, "b", &no).await, None);
        // Their next messages need no request.
        assert_eq!(
            verified(&mut avatars, &http, "a", &yes).await,
            Some(yes.clone())
        );
        assert_eq!(verified(&mut avatars, &http, "b", &no).await, None);
        assert_eq!(requests.load(Ordering::SeqCst), 2);

        // A sender who hid their profile has no avatar to check.
        let mut hidden = message("c", None);
        avatars.verify(&http, &mut hidden).await;
        assert_eq!(hidden.author.avatar_url, None);
        assert_eq!(requests.load(Ordering::SeqCst), 2);

        // `updatedUserData`: asked again.
        avatars.forget("a");
        verified(&mut avatars, &http, "a", &yes).await;
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn an_image_server_in_trouble_pauses_the_checks() {
        let (base, requests) = image_server().await;
        let http = reqwest::Client::new();
        let mut avatars = Avatars::default();

        // No answer in time, an odd answer: no avatar, and not remembered.
        let slow = format!("{base}/slow");
        let odd = format!("{base}/odd");
        let started = Instant::now();
        assert_eq!(verified(&mut avatars, &http, "a", &slow).await, None);
        assert!(
            started.elapsed() < CHECK_TIMEOUT * 2,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(verified(&mut avatars, &http, "a", &odd).await, None);
        assert!(avatars.known.is_empty());
        // The third failure in a row pauses checking...
        assert_eq!(verified(&mut avatars, &http, "b", &odd).await, None);
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        let yes = format!("{base}/yes");
        assert_eq!(verified(&mut avatars, &http, "c", &yes).await, None);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "no request while paused"
        );

        // ...until the pause is over.
        avatars.paused_until = Some(Instant::now());
        assert_eq!(verified(&mut avatars, &http, "c", &yes).await, Some(yes));
        assert_eq!(avatars.paused_until, None);
    }
}
