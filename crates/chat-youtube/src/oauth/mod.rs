//! YouTube login via OAuth 2.0, using Google's flow for desktop apps
//! (authorization code + PKCE, redirected to a short-lived local server).
//!
//! Streamers bring their own Google Cloud project: YouTube quota is billed to
//! the project that owns the OAuth client, and one shared project can't cover
//! even a single all-day stream (see docs/youtube-setup.md). So the client id
//! and secret are settings ([`OAuthApp`]), not constants.
//!
//! - [`begin_login`] → open [`PendingLogin::url`] in a browser →
//!   [`PendingLogin::complete`]: one-time login; stores the refresh token.
//! - [`TokenProvider`]: hands out access tokens, refreshing them as needed.
//! - [`logout`]: forgets the stored token.

use std::time::Duration;

use anyhow::Context;
use oauth2::basic::BasicClient;
use oauth2::{AuthType, AuthUrl, ClientId, ClientSecret, EndpointNotSet, EndpointSet, TokenUrl};

mod login;
mod provider;
mod store;

pub use login::{PendingLogin, begin_login};
pub use provider::{LoginRequired, TokenProvider};
pub use store::TokenStore;

/// Read-only access to the account's YouTube data: enough to find the
/// streamer's own broadcasts (including unlisted/members-only) and read chat.
const SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";

/// Google's endpoints; tests point these at a local fake.
#[derive(Clone)]
struct Endpoints {
    auth: String,
    token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
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

    /// From `YOUTUBE_CLIENT_ID` and `YOUTUBE_CLIENT_SECRET` (until the config
    /// file exists).
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str| {
            std::env::var(name).with_context(|| {
                format!("{name} must be set (your OAuth client, see docs/youtube-setup.md)")
            })
        };
        Ok(Self::new(
            var("YOUTUBE_CLIENT_ID")?,
            var("YOUTUBE_CLIENT_SECRET")?,
        ))
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

/// Forgets the stored login. The next start needs a new [`begin_login`].
pub async fn logout(app: &OAuthApp) -> anyhow::Result<()> {
    app.store.delete(app).await
}

/// The YouTube channel the stored login belongs to, e.g. to show "connected
/// as …". Costs 1 quota unit. Fails with [`LoginRequired`] if there's no
/// valid login (none stored, or revoked/expired).
pub async fn logged_in_channel(app: &OAuthApp) -> anyhow::Result<String> {
    let tokens = TokenProvider::from_store(app.clone()).await?;
    crate::channel_title(&reqwest::Client::new(), &crate::Auth::OAuth(tokens)).await
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

/// A local stand-in for Google's token endpoint, shared by the tests of the
/// submodules.
#[cfg(test)]
mod fake_google {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::{Form, State};
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::{Endpoints, OAuthApp, TokenStore};

    pub(super) const GOOD_CODE: &str = "good-code";
    pub(super) const REVOKED: &str = "revoked-refresh-token";

    #[derive(Clone)]
    pub(super) struct FakeGoogle {
        pub(super) refreshes: Arc<AtomicUsize>,
        /// `expires_in` of refreshed access tokens, in seconds.
        pub(super) expires_in: u64,
    }

    /// Starts the fake and returns an app pointing at it, storing tokens in
    /// a fresh temp dir.
    pub(super) async fn start(expires_in: u64) -> (OAuthApp, FakeGoogle, std::path::PathBuf) {
        let fake = FakeGoogle {
            refreshes: Arc::new(AtomicUsize::new(0)),
            expires_in,
        };
        let router = Router::new()
            .route("/token", post(token))
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
        };
        (app, fake, dir)
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
                if get("code") != Some(GOOD_CODE) || get("code_verifier").is_none() {
                    return invalid_grant;
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "access_token": "access-0",
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
}
