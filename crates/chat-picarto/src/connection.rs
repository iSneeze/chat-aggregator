//! Connecting the way a logged-out visitor's browser does (see PICARTO.md,
//! "How the website gets its chat"): look the channel up in the public API,
//! get an anonymous token from the website's GraphQL API, open the chat
//! WebSocket with it.

use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::SetupError;
use chat_websocket::Socket;
use chat_websocket::tungstenite::client::IntoClientRequest;
use chat_websocket::tungstenite::http::{HeaderValue, header};
use serde_json::{Value, json};

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
        // Only the user can fix that (a typo, a renamed channel): a
        // `SetupError`, which the engine doesn't retry.
        return Err(SetupError(format!("there is no Picarto channel named {name:?}")).into());
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
pub(crate) async fn open(http: &reqwest::Client, channel: &str) -> anyhow::Result<Socket> {
    let token = chat_token(http, channel).await?;
    // The token goes into the URL itself, so the URL is a secret too: it's
    // never logged, and tungstenite's errors don't include it.
    let mut request = format!("{CHAT}{token}").into_client_request()?;
    request
        .headers_mut()
        .insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    chat_websocket::connect(request)
        .await
        .context("connecting to Picarto's chat server")
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
