//! YouTube login via OAuth 2.0, using Google's flow for desktop apps
//! (authorization code + PKCE, redirected to a short-lived local server).
//!
//! Streamers bring their own Google Cloud project: YouTube quota is billed to
//! the project that owns the OAuth client, and one shared project can't cover
//! even a single all-day stream (see docs/youtube-setup.md). So the client id
//! and secret are settings ([`OAuthApp`]), not constants.
//!
//! A login belongs to one YouTube channel, the one picked on Google's
//! consent page, so a streamer with several channels logs in once per
//! channel; each login is stored under its channel ([`Account`]).
//!
//! - [`begin_login`] → open [`PendingLogin::url`] in a browser →
//!   [`PendingLogin::complete`]: one-time login; stores the refresh token
//!   under the channel it's for.
//! - [`TokenProvider`]: hands out access tokens, refreshing them as needed.
//! - [`check_login`]: whether a stored login still works.
//! - [`logout`]: forgets a stored token.
//! - [`adopt_legacy_login`]: copies the single login of older versions to
//!   its channel's name.

use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{AuthType, AuthUrl, ClientId, ClientSecret, EndpointNotSet, EndpointSet, TokenUrl};

mod login;
mod provider;
mod store;

pub use login::{PendingLogin, begin_login};
pub use provider::{LoginRequired, TokenProvider};
pub use store::{Account, TokenStore};

/// Read-only access to the account's YouTube data: enough to find the
/// streamer's own broadcasts (including unlisted/members-only) and read chat.
const SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";

/// Google's endpoints; tests point these at a local fake.
#[derive(Clone)]
struct Endpoints {
    auth: String,
    token: String,
    /// `channels.list`: which channel a login belongs to.
    channels: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            channels: crate::resolver::CHANNELS_URL.into(),
        }
    }
}

/// The streamer's own OAuth client ("Desktop app" type) from their Google
/// Cloud project.
///
/// Google explicitly doesn't treat a desktop app's client secret as
/// confidential (any installed program could be taken apart to find it);
/// PKCE is what protects the login. It's still nothing to post publicly.
#[derive(Clone)]
pub struct OAuthApp {
    client_id: String,
    client_secret: String,
    store: TokenStore,
    endpoints: Endpoints,
}

impl OAuthApp {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            store: TokenStore::System,
            endpoints: Endpoints::default(),
        }
    }

    /// Where the refresh token is kept (default: [`TokenStore::System`]).
    pub fn with_store(mut self, store: TokenStore) -> Self {
        self.store = store;
        self
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The `oauth2` client for our endpoints. The type parameters record
    /// which endpoints are set ("typestate"): calling `exchange_code` on a
    /// client without a token URL simply wouldn't compile.
    fn client(&self) -> anyhow::Result<GoogleClient> {
        Ok(BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_client_secret(ClientSecret::new(self.client_secret.clone()))
            .set_auth_uri(AuthUrl::new(self.endpoints.auth.clone())?)
            .set_token_uri(TokenUrl::new(self.endpoints.token.clone())?)
            // Credentials as form fields, as in Google's documentation
            // (the crate's default is an HTTP Basic auth header).
            .set_auth_type(AuthType::RequestBody))
    }
}

/// Auth URL and token URL set; no device, introspection or revocation URL.
type GoogleClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// Forgets `account`'s stored login: using it needs a new [`begin_login`].
/// (Only here: Google keeps the permission until it's revoked in the Google
/// account's settings.)
pub async fn logout(app: &OAuthApp, account: &Account) -> anyhow::Result<()> {
    app.store.delete(&store::secret_name(app, account)).await
}

/// Whether `account`'s stored login still works: Google hands out an access
/// token for it. No quota (only YouTube's own API counts). Fails with
/// [`LoginRequired`] if there's none or Google rejected it; with anything
/// else (offline) it's unknown.
pub async fn check_login(app: &OAuthApp, account: Account) -> anyhow::Result<()> {
    let login = TokenProvider::from_store(app.clone(), account).await?;
    login.access_token().await.map(drop)
}

/// Whether older versions' single login ([`Account::Legacy`]) is stored.
pub async fn legacy_login_stored(app: &OAuthApp) -> anyhow::Result<bool> {
    let legacy = store::secret_name(app, &Account::Legacy);
    Ok(app.store.load(&legacy).await?.is_some())
}

/// Moves along the login older versions stored (one per client, without
/// its channel, [`Account::Legacy`]): asks YouTube whose it is (1 quota
/// unit) and stores a copy under that channel's name. `None` if there's no
/// such login.
///
/// The old entry stays: the caller deletes it ([`logout`]) once it has
/// noted the channel somewhere it will find it again. Until then the old
/// entry is the only way to the login.
pub async fn adopt_legacy_login(app: &OAuthApp) -> anyhow::Result<Option<crate::OwnChannel>> {
    let legacy = store::secret_name(app, &Account::Legacy);
    let Some(refresh_token) = app.store.load(&legacy).await? else {
        return Ok(None);
    };
    let login = TokenProvider::new(app.clone(), Account::Legacy, refresh_token, None)?;
    let header = crate::resolver::bearer(&login).await?;
    let channel =
        crate::resolver::own_channel_at(&crate::rest_client(), header, &app.endpoints.channels)
            .await?;
    // Read back rather than the one loaded above: should Google have
    // replaced it during the refresh, only the new one still works.
    let refresh_token = login.refresh_token().await;
    let account = Account::Channel(channel.id.clone());
    app.store
        .save(&store::secret_name(app, &account), &refresh_token)
        .await?;
    Ok(Some(channel))
}

/// HTTP client for talking to Google's token endpoint.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        // The oauth2 docs' advice: following redirects on token requests
        // would let a malicious redirect send our secrets elsewhere (SSRF).
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("static reqwest configuration is valid")
}

/// Bridges `oauth2`'s HTTP types to our reqwest version. `oauth2` accepts
/// any `Fn(HttpRequest) -> Future<Output = Result<HttpResponse, _>>` as its
/// HTTP client, so callers pass `&|request| send(http.clone(), request)`.
async fn send(
    http: reqwest::Client,
    request: oauth2::HttpRequest,
) -> Result<oauth2::HttpResponse, reqwest::Error> {
    let response = http.execute(reqwest::Request::try_from(request)?).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let mut converted = oauth2::HttpResponse::new(response.bytes().await?.to_vec());
    *converted.status_mut() = status;
    *converted.headers_mut() = headers;
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::fake_google::{self, CHANNEL_ID, CHANNEL_TITLE, REVOKED};
    use super::*;

    fn is_login_required(e: &anyhow::Error) -> bool {
        e.chain().any(|e| e.is::<LoginRequired>())
    }

    async fn stored(app: &OAuthApp, account: &Account) -> Option<String> {
        app.store
            .load(&store::secret_name(app, account))
            .await
            .unwrap()
    }

    /// Older versions' single login is copied to its channel's name; the
    /// old entry stays until the caller has noted the channel.
    #[tokio::test]
    async fn the_old_login_is_copied_to_its_channel() {
        let (app, _, dir) = fake_google::start(3600).await;
        assert_eq!(adopt_legacy_login(&app).await.unwrap(), None, "none stored");
        assert!(!legacy_login_stored(&app).await.unwrap());

        let legacy = store::secret_name(&app, &Account::Legacy);
        app.store.save(&legacy, "refresh-old").await.unwrap();
        assert!(legacy_login_stored(&app).await.unwrap());
        let channel = adopt_legacy_login(&app).await.unwrap().unwrap();
        assert_eq!(
            (channel.id.as_str(), channel.title.as_str()),
            (CHANNEL_ID, CHANNEL_TITLE)
        );
        let account = Account::Channel(CHANNEL_ID.into());
        assert_eq!(stored(&app, &account).await.as_deref(), Some("refresh-old"));
        assert_eq!(
            stored(&app, &Account::Legacy).await.as_deref(),
            Some("refresh-old"),
            "deleting it is the caller's business"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_rejected_old_login_is_login_required_and_copied_nowhere() {
        let (app, _, dir) = fake_google::start(3600).await;
        let legacy = store::secret_name(&app, &Account::Legacy);
        app.store.save(&legacy, REVOKED).await.unwrap();
        let err = adopt_legacy_login(&app).await.unwrap_err();
        assert!(is_login_required(&err), "{err:#}");
        assert_eq!(
            stored(&app, &Account::Channel(CHANNEL_ID.into())).await,
            None
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn check_and_logout_are_per_account() {
        let (app, _, dir) = fake_google::start(3600).await;
        let main = Account::Channel("UC_main".into());
        let second = Account::Channel("UC_second".into());
        let err = check_login(&app, main.clone()).await.unwrap_err();
        assert!(is_login_required(&err), "not connected: {err:#}");

        for account in [&main, &second] {
            let name = store::secret_name(&app, account);
            app.store.save(&name, "refresh-1").await.unwrap();
        }
        check_login(&app, main.clone()).await.unwrap();
        logout(&app, &main).await.unwrap();
        let err = check_login(&app, main).await.unwrap_err();
        assert!(is_login_required(&err), "logged out: {err:#}");
        check_login(&app, second.clone())
            .await
            .expect("the other channel stays connected");

        let name = store::secret_name(&app, &second);
        app.store.save(&name, REVOKED).await.unwrap();
        let err = check_login(&app, second).await.unwrap_err();
        assert!(is_login_required(&err), "rejected by Google: {err:#}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// A local stand-in for Google's token and channel endpoints, shared by the
/// tests of the submodules and, through the `test-support` feature, other
/// crates' tests.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod fake_google {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::{Form, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::{Account, Endpoints, OAuthApp, TokenStore, store};

    pub const GOOD_CODE: &str = "good-code";
    /// A login with a Google account that has no YouTube channel.
    pub const NO_CHANNEL_CODE: &str = "no-channel-code";
    pub const REVOKED: &str = "revoked-refresh-token";
    /// The channel every other login belongs to.
    pub const CHANNEL_ID: &str = "UC_your_channel";
    pub const CHANNEL_TITLE: &str = "your_channel";
    /// The access token of the `NO_CHANNEL_CODE` login.
    const NO_CHANNEL_ACCESS: &str = "access-no-channel";

    #[derive(Clone)]
    pub struct FakeGoogle {
        pub refreshes: Arc<AtomicUsize>,
        /// `expires_in` of refreshed access tokens, in seconds.
        pub expires_in: u64,
    }

    /// Starts the fake and returns an app pointing at it, storing tokens in
    /// a fresh temp dir.
    pub async fn start(expires_in: u64) -> (OAuthApp, FakeGoogle, std::path::PathBuf) {
        let fake = FakeGoogle {
            refreshes: Arc::new(AtomicUsize::new(0)),
            expires_in,
        };
        let router = Router::new()
            .route("/token", post(token))
            .route("/channels", get(channels))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        static DIRS: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "chat-youtube-oauth-test-{}-{}",
            std::process::id(),
            DIRS.fetch_add(1, Ordering::Relaxed)
        ));
        let mut app = OAuthApp::new("test-client.apps.googleusercontent.com", "test-secret")
            .with_store(TokenStore::Dir(dir.clone()));
        app.endpoints = Endpoints {
            auth: format!("{base}/auth"),
            token: format!("{base}/token"),
            channels: format!("{base}/channels"),
        };
        (app, fake, dir)
    }

    /// Stores a login for `account` where `app` looks for it, as a login
    /// (or an older version) would have.
    pub async fn save_login(app: &OAuthApp, account: &Account, refresh_token: &str) {
        let name = store::secret_name(app, account);
        app.store.save(&name, refresh_token).await.unwrap();
    }

    /// The login stored for `account`, if any.
    pub async fn stored_login(app: &OAuthApp, account: &Account) -> Option<String> {
        let name = store::secret_name(app, account);
        app.store.load(&name).await.unwrap()
    }

    async fn token(
        State(fake): State<FakeGoogle>,
        Form(form): Form<std::collections::HashMap<String, String>>,
    ) -> (StatusCode, Json<Value>) {
        let get = |key: &str| form.get(key).map(String::as_str);
        assert_eq!(
            get("client_id"),
            Some("test-client.apps.googleusercontent.com")
        );
        assert_eq!(get("client_secret"), Some("test-secret"));
        let invalid_grant = (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant", "error_description": "Bad Request" })),
        );
        match get("grant_type") {
            Some("authorization_code") => {
                // PKCE: the verifier must come along with the code.
                let access = match get("code") {
                    Some(GOOD_CODE) => "access-0",
                    Some(NO_CHANNEL_CODE) => NO_CHANNEL_ACCESS,
                    _ => return invalid_grant,
                };
                if get("code_verifier").is_none() {
                    return invalid_grant;
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "access_token": access,
                        "expires_in": 3599,
                        "refresh_token": "refresh-1",
                        "scope": super::SCOPE,
                        "token_type": "Bearer"
                    })),
                )
            }
            Some("refresh_token") => {
                if get("refresh_token") == Some(REVOKED) {
                    return invalid_grant;
                }
                let n = fake.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                (
                    StatusCode::OK,
                    Json(json!({
                        "access_token": format!("access-{n}"),
                        "expires_in": fake.expires_in,
                        "scope": super::SCOPE,
                        "token_type": "Bearer"
                    })),
                )
            }
            _ => invalid_grant,
        }
    }

    /// `channels.list?mine=true`: the channel the access token belongs to.
    async fn channels(headers: HeaderMap) -> (StatusCode, Json<Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        match auth.strip_prefix("Bearer ") {
            None => (StatusCode::UNAUTHORIZED, Json(json!({}))),
            // With `fields`, an account without a channel gets `{}`.
            Some(NO_CHANNEL_ACCESS) => (StatusCode::OK, Json(json!({}))),
            Some(_) => (
                StatusCode::OK,
                Json(json!({
                    "items": [{ "id": CHANNEL_ID, "snippet": { "title": CHANNEL_TITLE } }]
                })),
            ),
        }
    }
}
