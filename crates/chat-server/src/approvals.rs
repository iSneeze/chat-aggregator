//! Which web sites may use the JSON API, and asking the streamer about new
//! ones.
//!
//! Programs, local pages and OBS browser sources always get in (see
//! [`crate::local`]). A site from the web (`https://game.example`) gets in
//! if the streamer allowed it. If they haven't decided yet, its connection
//! waits here while the app asks them. The game doesn't have to reconnect:
//! once they click "Allow", its connection simply goes through.
//!
//! - **Allowed** sites come from the settings (`[server] api_origins`) and
//!   from "Allow" in the app; the app saves the latter.
//! - **Denied** sites aren't asked about again until the app restarts, so
//!   a pushy page can't keep the question popping up.
//! - **Waiting**: one question per site, however many connections it
//!   opens, and at most [`MAX_WAITING`] sites at once, so a page cycling
//!   through subdomains can't flood the screen.
//!
//! Headless mode has nobody to ask (`ask: false`): there, only the settings
//! count.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{debug, info};

use crate::local;

/// How long a connection waits for the streamer's answer. Unanswered, it's
/// refused, but not remembered: the next attempt asks again.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// Sites waiting for an answer at the same time; more are refused at once.
const MAX_WAITING: usize = 3;

/// Denied sites remembered at most (in headless mode, every refused site
/// counts): enough for any real use, and a page inventing new subdomains
/// can't grow the list without end.
const MAX_DENIED: usize = 100;

/// Why a client was turned away; the text goes into the `403` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Not a site we could ask about (`null`, or not a web address).
    Unknown,
    /// Nobody to ask: headless mode, or too many questions already open.
    NotAllowed,
    /// The streamer said no.
    Denied,
    /// No answer in time (or the question was withdrawn).
    NoAnswer,
}

impl Refusal {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Refusal::Unknown => {
                "The JSON API can't tell which page is asking (its origin is \"null\" or \
                 unusual), so it can't let it in. Programs, pages served from localhost and \
                 OBS browser sources can always connect."
            }
            Refusal::NotAllowed => {
                "This site isn't allowed to use the JSON API. Allow it in chat-aggregator \
                 when asked, or add it to `api_origins` in config.toml."
            }
            Refusal::Denied => {
                "The streamer didn't allow this site to use the JSON API (until \
                 chat-aggregator restarts)."
            }
            Refusal::NoAnswer => {
                "Nobody answered chat-aggregator's question whether this site may use the \
                 JSON API. Connect again to ask again."
            }
        }
    }
}

pub struct ApiOrigins {
    state: Mutex<State>,
    /// The sites waiting for an answer, oldest first: what the app asks.
    requests: watch::Sender<Vec<String>>,
    /// Bumped whenever the allowed list changes, so connected clients can
    /// check whether they're still welcome.
    revision: watch::Sender<u64>,
    /// Whether there's anyone to ask (the app), or only the list counts.
    ask: bool,
}

#[derive(Default)]
struct State {
    allowed: HashSet<String>,
    denied: HashSet<String>,
    waiting: Vec<Waiting>,
}

/// One open question.
struct Waiting {
    origin: String,
    /// `None` until answered; every connection from this site waits on a
    /// receiver of it. A `watch` (not a `oneshot`) because there can be
    /// several waiting, and all of them need the same answer.
    answer: watch::Sender<Option<bool>>,
    /// Shared by all its connections: a later one doesn't extend it.
    deadline: Instant,
}

impl ApiOrigins {
    /// `allowed`: the sites from the settings. `ask`: whether the app asks
    /// about unknown ones (false in headless mode).
    pub fn new(allowed: impl IntoIterator<Item = String>, ask: bool) -> Self {
        Self {
            state: Mutex::new(State {
                allowed: allowed.into_iter().collect(),
                ..State::default()
            }),
            requests: watch::channel(Vec::new()).0,
            revision: watch::channel(0).0,
            ask,
        }
    }

    /// The sites waiting for an answer. Changes when one arrives, is
    /// answered or gives up waiting.
    pub fn requests(&self) -> watch::Receiver<Vec<String>> {
        self.requests.subscribe()
    }

    /// The streamer's answer about `origin`. Every connection waiting for it
    /// gets it right away; "deny" lasts until restart.
    pub fn answer(&self, origin: &str, allow: bool) {
        let mut state = self.lock();
        if allow {
            state.allowed.insert(origin.to_string());
            state.denied.remove(origin);
        } else if state.denied.len() < MAX_DENIED {
            state.denied.insert(origin.to_string());
        }
        if let Some(ix) = state.waiting.iter().position(|w| w.origin == origin) {
            let waiting = state.waiting.remove(ix);
            waiting.answer.send_replace(Some(allow));
        }
        self.publish(&state);
        drop(state);
        if allow {
            self.revision.send_modify(|r| *r += 1);
        }
    }

    /// Replaces the allowed sites (the settings changed, e.g. one was
    /// removed). Connected clients of a site no longer on the list are
    /// disconnected.
    pub fn set_allowed(&self, allowed: impl IntoIterator<Item = String>) {
        self.lock().allowed = allowed.into_iter().collect();
        self.revision.send_modify(|r| *r += 1);
    }

    /// Whether a client from `origin` may connect, waiting for the
    /// streamer's answer if they haven't decided about it yet.
    pub(crate) async fn check(&self, origin: Option<&str>) -> Result<(), Refusal> {
        if local::allowed_origin(origin) {
            return Ok(());
        }
        let Some(origin) = origin.filter(|o| is_web_origin(o)) else {
            return Err(Refusal::Unknown);
        };
        // The lock is taken and released inside this block: it's a std
        // Mutex, which must never be held across the `.await` below.
        let (answer, deadline) = {
            let mut state = self.lock();
            if state.allowed.contains(origin) {
                return Ok(());
            }
            if state.denied.contains(origin) {
                return Err(Refusal::Denied);
            }
            if !self.ask {
                // Logged once per site (the denied list remembers it).
                if state.denied.len() < MAX_DENIED && state.denied.insert(origin.to_string()) {
                    info!(
                        origin,
                        "refused a JSON API client from this site; to allow it, add it to \
                         api_origins in config.toml"
                    );
                }
                return Err(Refusal::NotAllowed);
            }
            match state.waiting.iter().find(|w| w.origin == origin) {
                Some(waiting) => (waiting.answer.subscribe(), waiting.deadline),
                None if state.waiting.len() >= MAX_WAITING => {
                    debug!(origin, "too many sites waiting for an answer, refused");
                    return Err(Refusal::NotAllowed);
                }
                None => {
                    let (sender, receiver) = watch::channel(None);
                    let deadline = Instant::now() + ANSWER_TIMEOUT;
                    state.waiting.push(Waiting {
                        origin: origin.to_string(),
                        answer: sender,
                        deadline,
                    });
                    self.publish(&state);
                    (receiver, deadline)
                }
            }
        };
        // However this ends (answered, timed out, or the connection gone
        // and this future dropped mid-wait), the guard's `drop` withdraws
        // the question once no connection waits for it any more. It owns
        // the receiver, so it can let go of it first.
        let mut guard = WaitGuard {
            origins: self,
            origin,
            answer: Some(answer),
        };
        let answer = guard.answer.as_mut().expect("set just above");
        let answered = tokio::time::timeout_at(deadline, answer.wait_for(Option::is_some))
            .await
            // Copy the answer out of the borrow (`*`): the borrow holds a
            // lock, and would make this future non-`Send` if kept.
            .map(|result| result.map(|answer| *answer));
        match answered {
            Ok(Ok(Some(true))) => Ok(()),
            Ok(Ok(_)) => Err(Refusal::Denied),
            // Timed out, or the question was withdrawn (sender dropped).
            Err(_) | Ok(Err(_)) => Err(Refusal::NoAnswer),
        }
    }

    /// Finishes once a client from `origin` isn't allowed any more (its
    /// site was removed from the list); never, for local clients.
    pub(crate) async fn revoked(&self, origin: Option<&str>) {
        let mut changes = self.revision.subscribe();
        loop {
            let allowed = local::allowed_origin(origin)
                || origin.is_some_and(|o| self.lock().allowed.contains(o));
            if !allowed {
                return;
            }
            if changes.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    fn publish(&self, state: &State) {
        let waiting: Vec<String> = state.waiting.iter().map(|w| w.origin.clone()).collect();
        self.requests.send_if_modified(|current| {
            let changed = *current != waiting;
            *current = waiting;
            changed
        });
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // See `Hub::lock_history`: the state stays valid even if a thread
        // panicked while holding the lock.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Withdraws a question when its last waiting connection is gone. RAII, like
/// `connections::Guard`: `drop` runs on every way out of `check`, including
/// the future being dropped while it waits (the client gave up).
struct WaitGuard<'a> {
    origins: &'a ApiOrigins,
    origin: &'a str,
    /// This connection's receiver for the answer. An `Option` so `drop`
    /// can drop it before counting who's still waiting: fields are only
    /// dropped automatically *after* `drop` has run.
    answer: Option<watch::Receiver<Option<bool>>>,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.answer = None;
        let mut state = self.origins.lock();
        let unwanted = state
            .waiting
            .iter()
            .position(|w| w.origin == self.origin && w.answer.receiver_count() == 0);
        if let Some(ix) = unwanted {
            state.waiting.remove(ix);
            self.origins.publish(&state);
        }
    }
}

/// A web site's origin, plain enough to show the streamer: `http(s)://`
/// and a host (with an optional port), nothing else. Browsers send
/// international domain names as `xn--…`, so only ASCII is expected, which
/// also rules out look-alike letters in the question.
fn is_web_origin(origin: &str) -> bool {
    let Some(host) = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    else {
        return false;
    };
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const GAME: &str = "https://game.example";

    /// Waits until the open questions satisfy `condition`; fails the test
    /// instead of hanging if they never do.
    async fn requests_become(origins: &ApiOrigins, condition: impl FnMut(&Vec<String>) -> bool) {
        let mut requests = origins.requests();
        tokio::time::timeout(Duration::from_secs(5), requests.wait_for(condition))
            .await
            .expect("the questions never got there")
            .unwrap();
    }

    /// Waits until `origin` is asked about (a spawned `check` runs on its own).
    async fn asked(origins: &ApiOrigins, origin: &str) {
        requests_become(origins, |r| r.iter().any(|o| o == origin)).await;
    }

    fn spawn_check(
        origins: &Arc<ApiOrigins>,
        origin: &'static str,
    ) -> tokio::task::JoinHandle<Result<(), Refusal>> {
        let origins = origins.clone();
        tokio::spawn(async move { origins.check(Some(origin)).await })
    }

    #[tokio::test]
    async fn local_clients_and_listed_sites_get_in_without_asking() {
        let origins = ApiOrigins::new([GAME.to_string()], true);
        for origin in [
            None,
            Some("http://localhost:8080"),
            Some("http://absolute"),
            Some(GAME),
        ] {
            assert_eq!(origins.check(origin).await, Ok(()), "{origin:?}");
        }
        assert!(origins.requests().borrow().is_empty());
    }

    #[tokio::test]
    async fn null_and_odd_origins_are_refused_without_asking() {
        let origins = ApiOrigins::new([], true);
        for origin in [
            "null",
            "file://",
            "chrome-extension://abc",
            "https://",
            "https://a b",
            "https://gаme.example",
        ] {
            assert_eq!(
                origins.check(Some(origin)).await,
                Err(Refusal::Unknown),
                "{origin}"
            );
        }
        assert!(origins.requests().borrow().is_empty());
    }

    #[tokio::test]
    async fn without_anyone_to_ask_only_the_list_counts() {
        let origins = ApiOrigins::new([], false);
        assert_eq!(origins.check(Some(GAME)).await, Err(Refusal::NotAllowed));
        assert!(origins.requests().borrow().is_empty());
    }

    #[tokio::test]
    async fn allow_lets_every_waiting_connection_in_and_is_remembered() {
        let origins = Arc::new(ApiOrigins::new([], true));
        let (first, second) = (spawn_check(&origins, GAME), spawn_check(&origins, GAME));
        asked(&origins, GAME).await;
        tokio::task::yield_now().await;
        assert_eq!(
            *origins.requests().borrow(),
            [GAME],
            "one question for both"
        );

        origins.answer(GAME, true);
        assert_eq!(first.await.unwrap(), Ok(()));
        assert_eq!(second.await.unwrap(), Ok(()));
        assert!(origins.requests().borrow().is_empty());
        assert_eq!(origins.check(Some(GAME)).await, Ok(()), "not asked again");
    }

    #[tokio::test]
    async fn deny_is_remembered_until_restart() {
        let origins = Arc::new(ApiOrigins::new([], true));
        let waiting = spawn_check(&origins, GAME);
        asked(&origins, GAME).await;
        origins.answer(GAME, false);
        assert_eq!(waiting.await.unwrap(), Err(Refusal::Denied));
        assert_eq!(origins.check(Some(GAME)).await, Err(Refusal::Denied));
        assert!(origins.requests().borrow().is_empty(), "not asked again");
    }

    // `start_paused`: tokio's clock jumps ahead instead of really waiting
    // two minutes (see the stagger tests).
    #[tokio::test(start_paused = true)]
    async fn unanswered_questions_expire_and_are_asked_again() {
        let origins = Arc::new(ApiOrigins::new([], true));
        let waiting = spawn_check(&origins, GAME);
        asked(&origins, GAME).await;
        assert_eq!(waiting.await.unwrap(), Err(Refusal::NoAnswer));
        assert!(
            origins.requests().borrow().is_empty(),
            "the question is withdrawn"
        );

        let again = spawn_check(&origins, GAME);
        asked(&origins, GAME).await;
        origins.answer(GAME, true);
        assert_eq!(again.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_question_is_withdrawn_when_its_connection_gives_up() {
        let origins = Arc::new(ApiOrigins::new([], true));
        let waiting = spawn_check(&origins, GAME);
        asked(&origins, GAME).await;
        waiting.abort(); // like a browser tab closed while waiting
        requests_become(&origins, Vec::is_empty).await;
    }

    #[tokio::test]
    async fn only_a_few_sites_are_asked_about_at_once() {
        let origins = Arc::new(ApiOrigins::new([], true));
        let sites = [
            "https://a.example",
            "https://b.example",
            "https://c.example",
        ];
        let _waiting: Vec<_> = sites.iter().map(|s| spawn_check(&origins, s)).collect();
        for site in sites {
            asked(&origins, site).await;
        }
        assert_eq!(
            origins.check(Some("https://d.example")).await,
            Err(Refusal::NotAllowed)
        );
        assert_eq!(origins.requests().borrow().len(), MAX_WAITING);
    }

    #[tokio::test]
    async fn removing_a_site_ends_its_connections() {
        let origins = ApiOrigins::new([GAME.to_string()], true);
        let revoked = origins.revoked(Some(GAME));
        let local = origins.revoked(None);
        tokio::pin!(revoked, local);
        // Still allowed: neither finishes.
        tokio::select! {
            biased;
            () = &mut revoked => panic!("revoked while still allowed"),
            () = &mut local => panic!("a local client is never revoked"),
            () = tokio::task::yield_now() => {}
        }
        origins.set_allowed([]);
        tokio::time::timeout(Duration::from_secs(5), revoked)
            .await
            .expect("still connected after being removed");
    }
}
