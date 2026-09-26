//! Keeps a valid access token around: refreshed from the stored refresh
//! token whenever the current one is missing or about to expire.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use oauth2::basic::BasicErrorResponseType;
use oauth2::{RefreshToken, RequestTokenError, TokenResponse};

use super::{GoogleClient, OAuthApp, http_client, send};

/// Refresh this long before the token expires, so a token handed out is
/// never about to die during a request or a reconnect.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// The user has to log in (again): no login stored yet, or Google rejected
/// the stored one (revoked by the user, or expired, e.g. after 7 days while
/// the Google project's consent screen is still in "Testing").
///
/// Returned inside `anyhow::Error`; check with
/// `err.downcast_ref::<LoginRequired>()`.
#[derive(Debug)]
pub struct LoginRequired(String);

impl LoginRequired {
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

impl fmt::Display for LoginRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "YouTube login required: {}", self.0)
    }
}

impl std::error::Error for LoginRequired {}

/// Hands out access tokens. Cheap to clone; all clones share one cached
/// token, so the YouTube source can ask on every reconnect (every ~12 s)
/// without causing a refresh each time.
#[derive(Clone)]
pub struct TokenProvider {
    inner: Arc<Inner>,
}

struct Inner {
    app: OAuthApp,
    client: GoogleClient,
    http: reqwest::Client,
    // A tokio Mutex, not std: it's held *across* the `.await` of a refresh
    // request, which a std Mutex must never be (it would block the thread).
    // Holding it there is the point: if two callers need a token at the same
    // moment, the second waits for the first one's refresh instead of
    // starting its own.
    state: tokio::sync::Mutex<State>,
}

struct State {
    refresh_token: String,
    access: Option<(String, Instant)>,
}

impl TokenProvider {
    /// Uses the login stored by a previous [`super::begin_login`].
    pub async fn from_store(app: OAuthApp) -> anyhow::Result<Self> {
        let refresh_token = app
            .store
            .load(&app)
            .await?
            .ok_or_else(|| LoginRequired("not logged in yet".into()))?;
        Self::new(app, refresh_token)
    }

    fn new(app: OAuthApp, refresh_token: String) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                client: app.client()?,
                app,
                http: http_client(),
                state: tokio::sync::Mutex::new(State {
                    refresh_token,
                    access: None,
                }),
            }),
        })
    }

    /// A currently valid access token, refreshed if necessary.
    pub async fn access_token(&self) -> anyhow::Result<String> {
        let mut state = self.inner.state.lock().await;
        if let Some((token, expires)) = &state.access
            && Instant::now() + REFRESH_MARGIN < *expires
        {
            return Ok(token.clone());
        }

        let http = &self.inner.http;
        let response = self
            .inner
            .client
            .exchange_refresh_token(&RefreshToken::new(state.refresh_token.clone()))
            .request_async(&|request| send(http.clone(), request))
            .await;
        let response = match response {
            Ok(response) => response,
            Err(RequestTokenError::ServerResponse(e))
                if *e.error() == BasicErrorResponseType::InvalidGrant =>
            {
                return Err(LoginRequired("Google rejected the stored login".into()).into());
            }
            Err(e) => return Err(e).context("refreshing the YouTube access token failed"),
        };

        // Google usually keeps the refresh token; if it ever sends a new
        // one, the old one stops working, so store the replacement.
        if let Some(new) = response.refresh_token()
            && *new.secret() != state.refresh_token
        {
            state.refresh_token = new.secret().clone();
            let app = &self.inner.app;
            app.store.save(app, &state.refresh_token).await?;
        }

        let token = response.access_token().secret().clone();
        let lifetime = response.expires_in().unwrap_or(Duration::from_secs(3600));
        state.access = Some((token.clone(), Instant::now() + lifetime));
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::fake_google::{self, REVOKED};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn not_logged_in_is_login_required() {
        let (app, _, _) = fake_google::start(3600).await;
        let err = TokenProvider::from_store(app).await.err().unwrap();
        assert!(err.downcast_ref::<LoginRequired>().is_some(), "{err:#}");
    }

    #[tokio::test]
    async fn token_is_cached_until_close_to_expiry() {
        let (app, fake, _) = fake_google::start(3600).await;
        let provider = TokenProvider::new(app, "refresh-1".into()).unwrap();

        assert_eq!(provider.access_token().await.unwrap(), "access-1");
        assert_eq!(provider.access_token().await.unwrap(), "access-1");
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn token_within_the_margin_is_refreshed() {
        // Lives shorter than the refresh margin: stale as soon as it arrives.
        let (app, fake, _) = fake_google::start(60).await;
        let provider = TokenProvider::new(app, "refresh-1".into()).unwrap();

        assert_eq!(provider.access_token().await.unwrap(), "access-1");
        assert_eq!(provider.access_token().await.unwrap(), "access-2");
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let (app, fake, _) = fake_google::start(3600).await;
        let provider = TokenProvider::new(app, "refresh-1".into()).unwrap();

        // A second handle, like a second source sharing the login. It needs
        // a name: the future returned by `access_token()` borrows it until
        // `join!` finishes, so a temporary `provider.clone()` would be
        // dropped too early (E0716).
        let other = provider.clone();
        let (a, b) = tokio::join!(provider.access_token(), other.access_token());
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn revoked_login_is_login_required() {
        let (app, _, _) = fake_google::start(3600).await;
        let provider = TokenProvider::new(app, REVOKED.into()).unwrap();
        let err = provider.access_token().await.unwrap_err();
        assert!(err.downcast_ref::<LoginRequired>().is_some(), "{err:#}");
    }
}
