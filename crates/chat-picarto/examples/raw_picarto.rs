use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chat_picarto::{ChannelEmote, SiteEmotes};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// Prints everything Picarto's chat sends for a channel as raw JSON frames,
/// before anything is decoded or filtered. For collecting real messages and
/// events to build the Picarto source from (see PICARTO.md, the maintainer's
/// research notes, kept outside the public repository):
///
///   cargo run -p chat-picarto --example raw_picarto -- <channel> | tee /tmp/picarto-raw.log
///
/// `<channel>` is the channel name or a link to the channel: the stream page
/// (`picarto.tv/<name>`) or the pop-out chat
/// (`picarto.tv/chatpopout/<name>/public`).
///
/// Connects anonymously, like a logged-out visitor's browser: a token from
/// Picarto's GraphQL API, then the chat WebSocket. Pings every 30 seconds as
/// the site does, and reconnects with a fresh token when the connection
/// ends, so it can run through a whole stream. Stop it with Ctrl+C.
///
/// Also loads the emote tables from Picarto's website (`[emotes]`) and
/// prints each chat message the way the source will show it (`[shown]`),
/// right after its raw `[c]` frame.
///
/// The log contains real chatter ids, names and messages: keep it out of the
/// repo. The token is never printed (only its claims).
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Some(input) = std::env::args().nth(1) else {
        bail!(
            "usage: cargo run -p chat-picarto --example raw_picarto -- <channel name or picarto.tv link>"
        );
    };
    let channel = chat_picarto::normalize_channel(&input).map_err(anyhow::Error::msg)?;
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()?;
    let tls = tls_config()?;
    // Without them only channel emotes resolve; the capture goes on.
    let emotes = match SiteEmotes::fetch(&http).await {
        Ok(emotes) => {
            line(
                "emotes",
                &format!(
                    "{} Picarto emotes, {} emoji names",
                    emotes.ptv_len(),
                    emotes.emoji_len()
                ),
            );
            emotes
        }
        Err(error) => {
            line("error", &format!("loading the emote tables: {error:#}"));
            SiteEmotes::default()
        }
    };

    // An error before the first connection most likely means a wrong name
    // or no network: stop and say so. Later errors (network hiccups, Picarto
    // restarting) are retried, waiting longer after each failure in a row.
    let mut connected_once = false;
    let mut delay = RETRY_FIRST;
    loop {
        match connect(&http, &tls, &channel).await {
            Ok(socket) => {
                connected_once = true;
                delay = RETRY_FIRST;
                if let Err(error) = print_frames(socket, &emotes).await {
                    line("error", &format!("{error:#}"));
                }
            }
            Err(error) if !connected_once => return Err(error),
            Err(error) => {
                line("error", &format!("{error:#}"));
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
        line("reconnecting", &format!("in {} s", delay.as_secs()));
        tokio::time::sleep(delay).await;
    }
}

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
/// What the website sends to keep the chat connection alive.
const PING: &str = r#"{"type":"ping","message":"__ping__"}"#;
const PING_EVERY: Duration = Duration::from_secs(30);
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);

/// The public channel info fields that show what state the channel is in.
/// The rest (description panels, thumbnails, tags, ...) is left out.
const CHANNEL_FIELDS: &[&str] = &[
    "user_id",
    "name",
    "online",
    "viewers",
    "followers",
    "subscribers",
    "account_type",
    "adult",
    "private",
    "chat_settings",
    "multistream",
    "last_live",
];

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Does what Picarto's web page does: get a token, open the WebSocket.
/// Looks the channel up in the public API first, so the log shows how the
/// channel changes (live, offline, ...) and a wrong name gets a clear
/// error: the token request just returns nothing for it.
async fn connect(
    http: &reqwest::Client,
    tls: &Arc<rustls::ClientConfig>,
    channel: &str,
) -> anyhow::Result<Socket> {
    let info = get_json(http, &format!("{PUBLIC_API}/channel/name/{channel}"))
        .await
        .context("looking up the channel")?;
    line(
        "channel",
        &Value::Object(pick(&info, CHANNEL_FIELDS)).to_string(),
    );

    let token = chat_token(http, channel).await?;
    line("token", &token_claims(&token));

    // The token goes into the URL itself, so the URL is a secret too: it's
    // never printed, and tungstenite's errors don't include it.
    let mut request = format!("{CHAT}{token}").into_client_request()?;
    request
        .headers_mut()
        .insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    let connector = Connector::Rustls(Arc::clone(tls));
    let (socket, _response) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .context("connecting to the chat server")?;
    line("connected", &format!("{CHAT}…"));
    Ok(socket)
}

/// The website's GraphQL request for an anonymous chat token.
async fn chat_token(http: &reqwest::Client, channel: &str) -> anyhow::Result<String> {
    let body = json!({ "query": TOKEN_QUERY, "variables": { "channelName": channel } });
    let response = http.post(GRAPHQL).json(&body).send().await?;
    let answer = read_json(response)
        .await
        .context("requesting a chat token")?;
    // GraphQL reports problems in the answer, with status 200.
    if let Some(errors) = answer.get("errors") {
        bail!("the token request failed: {errors}");
    }
    answer["data"]["generateJwtToken"]["key"]
        .as_str()
        .map(str::to_owned)
        .context("Picarto returned no chat token for this channel")
}

/// Prints frames until the server closes the connection, pinging in
/// between.
async fn print_frames(mut socket: Socket, emotes: &SiteEmotes) -> anyhow::Result<()> {
    let mut pings = tokio::time::interval(PING_EVERY);
    // An interval's first tick is immediate; the first ping is due later.
    pings.tick().await;
    loop {
        // Waits for whichever comes first. The losing branch's future is
        // dropped before the winner's code runs, so both branches may use
        // `socket` mutably.
        tokio::select! {
            frame = socket.next() => {
                let Some(frame) = frame else {
                    line("closed", "connection dropped");
                    return Ok(());
                };
                if print_frame(frame?, emotes) {
                    return Ok(());
                }
            }
            _ = pings.tick() => {
                socket.send(Message::text(PING)).await.context("sending a ping")?;
            }
        }
    }
}

/// Prints one frame; true when it was the server's goodbye.
fn print_frame(frame: Message, emotes: &SiteEmotes) -> bool {
    match frame {
        Message::Text(text) => {
            let json = serde_json::from_str::<Value>(&text).ok();
            // The answers to our own pings: one every 30 s, nothing to learn.
            if json.as_ref().is_some_and(|json| json["code"] == "PONG") {
                return false;
            }
            line(&label(json.as_ref()), &text);
            if let Some(json) = &json {
                print_shown(json, emotes);
            }
        }
        Message::Binary(bytes) => line("binary", &format!("{} bytes", bytes.len())),
        // The close code and reason tell how Picarto ends a session (stream
        // over, bad token, kicked, ...).
        Message::Close(Some(close)) => {
            let code = u16::from(close.code);
            line(
                "closed",
                &format!("code {code}, reason {:?}", close.reason.as_str()),
            );
            return true;
        }
        Message::Close(None) => {
            line("closed", "without a close frame");
            return true;
        }
        // WebSocket-level keep-alive: tungstenite answers pings by itself.
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
    }
    false
}

/// Chat messages (`c`) with their emotes resolved: the text as the source
/// will show it, then the codes that became images.
fn print_shown(frame: &Value, emotes: &SiteEmotes) {
    if frame["t"] != "c" {
        return;
    }
    for message in frame["m"].as_array().into_iter().flatten() {
        let text = message["m"].as_str().unwrap_or_default();
        let channel: Vec<Option<ChannelEmote>> =
            serde_json::from_value(message["e"].clone()).unwrap_or_default();
        let body = emotes.convert(text, &channel);
        let codes: Vec<&str> = body.emotes.iter().map(|e| e.code.as_str()).collect();
        let only = if body.emote_only { " (emote only)" } else { "" };
        line(
            "shown",
            &format!("{} | images: {}{only}", body.text, codes.join(" ")),
        );
    }
}

/// The frame's type: `t`, or `type` for some frames (`stream`). For a role
/// change (`ua`) also the change inside it: "ua ub" is a ban. Up front for
/// grepping.
fn label(frame: Option<&Value>) -> String {
    let Some(json) = frame else {
        return "not json".to_owned();
    };
    let Some(kind) = json["t"].as_str().or_else(|| json["type"].as_str()) else {
        return match json["code"].as_str() {
            Some(code) => format!("code {code}"),
            None => "?".to_owned(),
        };
    };
    match json["m"][0]["t"].as_str() {
        Some(inner) if inner != kind => format!("{kind} {inner}"),
        _ => kind.to_owned(),
    }
}

/// The claims of the chat token (a JWT: header, claims and signature,
/// base64-encoded and joined by dots). Not checked, only read: they show
/// who the token is for and whether it expires.
fn token_claims(token: &str) -> String {
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    match claims {
        Some(claims) => {
            let expiry = if claims.get("exp").is_some() {
                "has an expiry (exp)"
            } else {
                "no expiry"
            };
            format!("{claims} ({expiry})")
        }
        None => "not a readable JWT".to_owned(),
    }
}

async fn get_json(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    read_json(http.get(url).send().await?).await
}

/// The answer as JSON. On an error status, the error includes the start of
/// the answer: Picarto's reason for refusing is what we're after.
async fn read_json(response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        // One line for the log (error pages are multi-line HTML).
        let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let body: String = body.chars().take(300).collect();
        bail!("HTTP {status}: {body}");
    }
    Ok(response.json().await?)
}

/// A copy of only the given fields (those that exist).
fn pick(json: &Value, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|&field| Some((field.to_owned(), json.get(field)?.clone())))
        .collect()
}

fn line(label: &str, text: &str) {
    println!(
        "{} [{label}] {text}",
        chrono::Local::now().format("%H:%M:%S%.3f")
    );
}

/// TLS for the WebSocket, with the crypto backend chosen explicitly.
/// Otherwise tungstenite asks rustls for the process-wide default, which
/// panics when rustls was built with both of its backends: `ring` (from
/// chat-youtube's gRPC) and `aws-lc-rs` (from reqwest). That happens in any
/// build that contains both crates (`--workspace` now, the app later). We
/// take aws-lc-rs, which reqwest uses anyway.
fn tls_config() -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}
