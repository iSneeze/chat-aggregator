//! The one-time login: Google's consent page in the browser, redirected back
//! to a short-lived server on `127.0.0.1` that picks up the result.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use oauth2::{
    AuthorizationCode, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::{GoogleClient, OAuthApp, SCOPE, http_client, send};

/// How long to wait for the user to finish in the browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// What the browser brings back: an authorization code, or Google's error
/// (e.g. `access_denied` when the user clicks "Cancel").
type CallbackResult = Result<String, String>;

/// A login waiting for the user. Open [`url`](Self::url) in a browser, then
/// await [`complete`](Self::complete). Dropping it cancels the login and
/// stops the local server.
pub struct PendingLogin {
    url: String,
    app: OAuthApp,
    client: GoogleClient,
    verifier: PkceCodeVerifier,
    result: oneshot::Receiver<CallbackResult>,
    // Dropping this sender (with the struct) also stops the server.
    stop_server: oneshot::Sender<()>,
}

/// Starts a login: binds the local callback server and builds Google's
/// consent-page URL.
pub async fn begin_login(app: &OAuthApp) -> anyhow::Result<PendingLogin> {
    // Port 0: any free port. Google accepts any loopback port for desktop
    // clients, so there's nothing to configure and no clash with the overlay.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("can't start the local login server")?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let client = app.client()?.set_redirect_uri(RedirectUrl::new(redirect)?);

    // PKCE: we keep a random secret (the verifier) and send only its hash
    // (the challenge). Whoever redeems the code must present the verifier,
    // so an intercepted code is useless to anyone else.
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(SCOPE.into()))
        .set_pkce_challenge(challenge)
        // We need a refresh token, to keep working after the first hour.
        .add_extra_param("access_type", "offline")
        // Google only sends a refresh token when the consent screen is
        // shown; force it, so logging in again also gets one.
        .add_extra_param("prompt", "consent")
        .url();

    let (result_tx, result_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let state = CallbackState {
        expected_state: csrf.secret().clone(),
        result: Arc::new(Mutex::new(Some(result_tx))),
    };
    let router = Router::new()
        .route("/callback", get(callback))
        .with_state(state);
    tokio::spawn(async move {
        // Graceful: a response that is still being sent (the "you can close
        // this tab" page) is finished before the server stops. `stop_rx`
        // also resolves when the sender is dropped, i.e. the login is dropped.
        let shutdown = async {
            let _ = stop_rx.await;
        };
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::warn!("login server failed: {e}");
        }
    });

    Ok(PendingLogin {
        url: url.to_string(),
        app: app.clone(),
        client,
        verifier,
        result: result_rx,
        stop_server: stop_tx,
    })
}

impl PendingLogin {
    /// Google's consent page. Open it in the user's browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for the user to finish (up to 5 minutes), exchanges the code for
    /// tokens and stores the refresh token.
    pub async fn complete(self) -> anyhow::Result<()> {
        // `self` is taken apart field by field below ("partial moves"); fine
        // because PendingLogin has no `Drop` impl of its own.
        let outcome = tokio::time::timeout(LOGIN_TIMEOUT, self.result).await;
        let _ = self.stop_server.send(());

        let code = match outcome {
            Err(_) => bail!("login timed out: nothing came back from the browser within 5 minutes"),
            Ok(Err(_)) => bail!("the local login server stopped unexpectedly"),
            Ok(Ok(Err(error))) => bail!("Google refused the login: {error}"),
            Ok(Ok(Ok(code))) => code,
        };

        let http = http_client();
        let tokens = self
            .client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(self.verifier)
            .request_async(&|request| send(http.clone(), request))
            .await
            .context("exchanging the login code with Google failed")?;
        let refresh = tokens.refresh_token().context(
            "Google sent no refresh token; try logging in again (the consent screen must be shown)",
        )?;
        self.app.store.save(&self.app, refresh.secret()).await
    }
}

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    /// Taken by the first valid callback; later ones find `None`.
    result: Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
}

#[derive(serde::Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(s): State<CallbackState>,
    Query(params): Query<CallbackParams>,
) -> (StatusCode, Html<String>) {
    // `state` is the random value we put into the consent URL. A request
    // without it didn't come from this login (an old tab, or another page
    // poking at localhost): reject it and keep waiting for the real one.
    if params.state.as_deref() != Some(s.expected_state.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            page(
                "This link doesn't belong to the current login. Start the login again from the app.",
            ),
        );
    }
    let outcome = match (params.code, params.error) {
        (Some(code), _) => Ok(code),
        (None, Some(error)) => Err(error),
        (None, None) => Err("no authorization code in the response".to_string()),
    };
    let message = match &outcome {
        Ok(_) => "Connected to YouTube. You can close this tab and go back to the app.",
        Err(_) => "The login was cancelled or refused. You can close this tab.",
    };
    if let Some(tx) = s.result.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = tx.send(outcome);
    }
    (StatusCode::OK, page(message))
}

fn page(message: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>chat-aggregator</title>\
         <body style=\"font:18px system-ui,sans-serif;display:grid;place-items:center;\
         height:90vh;margin:0\"><p>{message}</p></body>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::fake_google::{self, GOOD_CODE};
    use oauth2::url::Url;

    fn query(url: &str) -> std::collections::HashMap<String, String> {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    /// Plays the browser: follows Google's redirect back to our server.
    async fn redirect_back(login: &PendingLogin, params: &str) -> reqwest::StatusCode {
        let redirect_uri = query(login.url())["redirect_uri"].clone();
        reqwest::get(format!("{redirect_uri}?{params}"))
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn consent_url_asks_for_what_we_need() {
        let (app, _, _) = fake_google::start(3600).await;
        let login = begin_login(&app).await.unwrap();
        let q = query(login.url());

        assert_eq!(q["client_id"], app.client_id());
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert!(!q["state"].is_empty());
    }

    #[tokio::test]
    async fn full_login_stores_the_refresh_token() {
        let (app, _, dir) = fake_google::start(3600).await;
        let login = begin_login(&app).await.unwrap();
        let state = query(login.url())["state"].clone();

        // A request with a wrong state is rejected and doesn't end the login.
        assert_eq!(
            redirect_back(&login, &format!("code={GOOD_CODE}&state=forged")).await,
            reqwest::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            redirect_back(&login, &format!("code={GOOD_CODE}&state={state}")).await,
            reqwest::StatusCode::OK
        );
        login.complete().await.unwrap();

        assert_eq!(
            app.store.load(&app).await.unwrap().as_deref(),
            Some("refresh-1")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cancelled_consent_is_an_error() {
        let (app, _, _) = fake_google::start(3600).await;
        let login = begin_login(&app).await.unwrap();
        let state = query(login.url())["state"].clone();

        redirect_back(&login, &format!("error=access_denied&state={state}")).await;
        let err = login.complete().await.unwrap_err();
        assert!(err.to_string().contains("access_denied"), "{err}");
    }
}
