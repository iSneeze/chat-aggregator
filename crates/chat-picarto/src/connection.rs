//! Connecting the way a logged-out visitor's browser does (see PICARTO.md,
//! "How the website gets its chat"): look the channel up in the public API,
//! get an anonymous token from the website's GraphQL API, open the chat
//! WebSocket with it.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// Sent with every request, the WebSocket's included: Picarto's chat server
/// refuses a handshake without a `User-Agent` (403), and tungstenite sends
/// none by itself. Any value will do.
const USER_AGENT: &str = concat!("chat-aggregator/", env!("CARGO_PKG_VERSION"));
const PUBLIC_API: &str = "https://api.picarto.tv/api/v1";
const GRAPHQL: &str = "https://ptvintern.picarto.tv/ptvapi";
const CHAT: &str = "wss://chat.picarto.tv/chat/token=";
/// The query the website runs for a chat token; `channel_name` works
/// without a login.
const TOKEN_QUERY: &str = "query generateToken($channelName: String) { \
    generateJwtToken(channel_name: $channelName) { key } }";

pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The channel doesn't exist on Picarto: only the user can fix that (a
/// typo, a renamed channel), so the engine doesn't retry it.
#[derive(Debug)]
pub struct ChannelNotFound(pub String);

impl fmt::Display for ChannelNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "there is no Picarto channel named {:?}", self.0)
    }
}

impl std::error::Error for ChannelNotFound {}

/// What the public API says about the channel, as far as the source needs
/// it.
pub(crate) struct Channel {
    /// The channel's id, which is also its streamer's user id in chat.
    pub id: String,
    /// The name as Picarto spells it.
    pub name: String,
    pub online: bool,
}

pub(crate) fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()?)
}

/// Looks the channel up in the public API. The token request just returns
/// nothing for a wrong name; this answers 404, so a typo gets a clear
/// error.
pub(crate) async fn lookup(http: &reqwest::Client, name: &str) -> anyhow::Result<Channel> {
    // `name` passed `normalize_channel`: safe in a URL path as it is.
    let response = http
        .get(format!("{PUBLIC_API}/channel/name/{name}"))
        .send()
        .await
        .context("looking up the Picarto channel")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(ChannelNotFound(name.to_owned()).into());
    }
    let info: Value = response
        .error_for_status()
        .context("looking up the Picarto channel")?
        .json()
        .await
        .context("reading the Picarto channel's info")?;
    let id = match &info["user_id"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) if !s.is_empty() => s.clone(),
        _ => bail!("Picarto's channel info has no id"),
    };
    Ok(Channel {
        id,
        name: info["name"].as_str().unwrap_or(name).to_owned(),
        online: info["online"].as_bool().unwrap_or(false),
    })
}

/// Gets a fresh anonymous token and opens the chat WebSocket with it.
pub(crate) async fn open(
    http: &reqwest::Client,
    tls: &Arc<rustls::ClientConfig>,
    channel: &str,
) -> anyhow::Result<Socket> {
    let token = chat_token(http, channel).await?;
    // The token goes into the URL itself, so the URL is a secret too: it's
    // never logged, and tungstenite's errors don't include it.
    let mut request = format!("{CHAT}{token}").into_client_request()?;
    request
        .headers_mut()
        .insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    let connector = Connector::Rustls(Arc::clone(tls));
    let (socket, _response) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .context("connecting to Picarto's chat server")?;
    Ok(socket)
}

/// The website's GraphQL request for an anonymous chat token.
async fn chat_token(http: &reqwest::Client, channel: &str) -> anyhow::Result<String> {
    let body = json!({ "query": TOKEN_QUERY, "variables": { "channelName": channel } });
    let answer: Value = http
        .post(GRAPHQL)
        .json(&body)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("requesting a Picarto chat token")?
        .json()
        .await
        .context("reading the Picarto chat token")?;
    // GraphQL reports problems in the answer, with status 200.
    if let Some(errors) = answer.get("errors") {
        bail!("the Picarto chat token request failed: {errors}");
    }
    answer["data"]["generateJwtToken"]["key"]
        .as_str()
        .map(str::to_owned)
        .context("Picarto returned no chat token for this channel")
}

/// TLS for the WebSocket, with the crypto backend chosen explicitly.
/// Otherwise tungstenite asks rustls for the process-wide default, which
/// panics when rustls was built with both of its backends: `ring` (from
/// chat-youtube's gRPC) and `aws-lc-rs` (from reqwest), as in any build with
/// both crates. We take aws-lc-rs, which reqwest uses anyway.
pub(crate) fn tls_config() -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}
