use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, TimeDelta, Utc};
use futures_util::StreamExt;
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// Prints everything RPlay's chat sends for a channel as raw JSON frames,
/// before anything is decoded or filtered. For collecting real messages and
/// events to build the RPlay source from (see RPLAY.md, the maintainer's
/// research notes, kept outside the public repository):
///
///   cargo run -p chat-rplay --example raw_rplay -- <channel> | tee /tmp/rplay-raw.log
///
/// `<channel>` is the creator id or a link to the channel: the stream page
/// (`rplay.live/live/<id>`), a custom link (`rplay.live/c/<name>/live`) or
/// any other page with the id in it.
///
/// Connects anonymously with a read-only token, like a logged-out visitor's
/// browser. RPlay ends the session after 60 minutes (the token's lifetime);
/// this reconnects with a fresh token, so it can run through a whole stream.
/// Stop it with Ctrl+C.
///
/// With a toolkey from the streamer's RPlay studio (for restricted streams:
/// tickets, subscribers only, secret key), it connects as the streamer's own
/// OBS widgets do instead, after logging what an anonymous visitor gets. The
/// key is a secret, so it comes from the environment, not the command line
/// (which ends up in the shell history):
///
///   read -rs RPLAY_TOOLKEY && export RPLAY_TOOLKEY   # paste the key or a widget link, then Enter
///   cargo run -p chat-rplay --example raw_rplay | tee /tmp/rplay-raw.log
///   unset RPLAY_TOOLKEY
///
/// Without `<channel>` it uses the toolkey's own channel.
///
/// Each chatter is also looked up once, as RPlay's page does, for what the
/// messages lack (subscription tier, admin flag): a `[user]` line, and a
/// `[user updated]` line when RPlay announces a change to them.
///
/// The log contains real chatter ids, names and messages: keep it out of the
/// repo. Tokens and keys are never printed.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("chat-aggregator/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;
    let tls = tls_config()?;

    let mut tool = match std::env::var("RPLAY_TOOLKEY") {
        Ok(value) if !value.trim().is_empty() => {
            Some(ToolSession::start(&http, &toolkey(&value)).await?)
        }
        _ => None,
    };
    let creator = match (std::env::args().nth(1), &tool) {
        (Some(input), _) => resolve_creator(&http, &input).await?,
        (None, Some(tool)) => tool.channel.clone(),
        (None, None) => bail!(
            "usage: cargo run -p chat-rplay --example raw_rplay -- <creator id or rplay.live link>"
        ),
    };
    if let Some(tool) = &tool {
        // RPlay's site doesn't use a toolkey for other channels either.
        if tool.channel != creator {
            bail!(
                "the toolkey belongs to channel {}, not {creator}",
                tool.channel
            );
        }
        log_anonymous_access(&http, &creator).await;
    }

    // An error before the first connection most likely means a wrong id or
    // no network: stop and say so. Later errors (network hiccups, RPlay
    // restarting) are retried, waiting longer after each failure in a row.
    let mut connected_once = false;
    let mut delay = RETRY_FIRST;
    // Outlives the connections: a chatter is looked up once per run.
    let mut lookups = Lookups {
        http: http.clone(),
        creator: creator.clone(),
        seen: HashSet::new(),
    };
    loop {
        match connect(&http, &tls, &creator, tool.as_mut()).await {
            Ok(socket) => {
                connected_once = true;
                delay = RETRY_FIRST;
                if let Err(error) = print_frames(socket, &mut lookups).await {
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

const API: &str = "https://api.rplay.live";
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);

/// The `/live/play` fields that show what state the channel is in. Only
/// these get printed: the answer also has fields like the stream key (empty
/// for anonymous visitors, but the real one with a toolkey).
const PLAY_FIELDS: &[&str] = &[
    "streamState",
    "streamStartTime",
    "chatUrl",
    "accessible",
    "allowAnonymous",
    "useAnonymousChat",
    "subscriberOnlyChat",
    "useSubscriber",
    "useTicket",
    "useSecretKey",
    "isAdultContent",
    "chatCacheVersion",
];
/// `/live/play` lists printed as their length only (`managers` holds email
/// addresses).
const PLAY_LISTS: &[&str] = &["bannedWords", "managers", "subscriptionTiers", "tierHashes"];
/// The parts of the channel's subscription tiers (`/live/play`) needed to
/// read the users' `subscribingTo`: a tier hash there is one of these.
const TIER_FIELDS: &[&str] = &["tierHash", "name", "grade", "customIcon"];
/// The `/live/user` fields printed as they are. Its `subscribingTo` covers
/// every channel the user follows, so only this channel's entry and the
/// number of channels get printed.
const USER_FIELDS: &[&str] = &["nickname", "multiLangNick", "admin", "isLivestreamer"];
/// Everything `/live/chattoken` returns except the token itself.
const TOKEN_FIELDS: &[&str] = &[
    "chatUserId",
    "canSendMessage",
    "isMuted",
    "mutedUntil",
    "sessionExpirationTime",
    "tokenExpirationTime",
];

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Does what RPlay's web page does: look up the channel's chat server, get a
/// token, open the WebSocket. Fetches both again on every reconnect, so the
/// log also shows how the channel changes (live, offline, ...).
async fn connect(
    http: &reqwest::Client,
    tls: &Arc<rustls::ClientConfig>,
    creator: &str,
    tool: Option<&mut ToolSession>,
) -> anyhow::Result<Socket> {
    // Renewing needs `&mut`; the requests below only read the session.
    let tool = match tool {
        Some(tool) => {
            tool.renew_if_needed(http).await?;
            Some(&*tool)
        }
        None => None,
    };

    let play = get_json(http, "live/play", creator, tool).await?;
    line("play", &play_summary(&play));
    line("tiers", &tiers_summary(&play));
    let chat_url = play["chatUrl"]
        .as_str()
        .filter(|url| !url.is_empty())
        .context("/live/play has no chatUrl")?;

    let answer = get_json(http, "live/chattoken", creator, tool).await?;
    line("token", &token_summary(&answer));
    let token = answer["token"]
        .as_str()
        .context("/live/chattoken has no token")?;

    let mut request = chat_url.into_client_request()?;
    // Amazon IVS takes the token as the WebSocket *subprotocol*, because
    // browsers can't add other headers to a WebSocket. The server echoes it
    // back, which tungstenite insists on.
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", secret_header(token)?);
    let connector = Connector::Rustls(Arc::clone(tls));
    let (socket, _response) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .context("connecting to the chat server")?;
    line("connected", chat_url);
    Ok(socket)
}

/// With a toolkey: what a logged-out visitor gets, for comparison. On a
/// restricted stream this shows whether, and how, RPlay refuses them.
async fn log_anonymous_access(http: &reqwest::Client, creator: &str) {
    match get_json(http, "live/play", creator, None).await {
        Ok(play) => line("anonymous play", &play_summary(&play)),
        Err(error) => line("anonymous play", &format!("error: {error:#}")),
    }
    match get_json(http, "live/chattoken", creator, None).await {
        Ok(answer) => line("anonymous token", &token_summary(&answer)),
        Err(error) => line("anonymous token", &format!("error: {error:#}")),
    }
}

/// Prints frames until the server closes the connection.
async fn print_frames(mut socket: Socket, lookups: &mut Lookups) -> anyhow::Result<()> {
    while let Some(frame) = socket.next().await {
        match frame? {
            Message::Text(text) => {
                let json = serde_json::from_str::<Value>(&text).ok();
                line(&label(json.as_ref()), &text);
                if let Some(json) = &json {
                    lookups.on_frame(json);
                }
            }
            Message::Binary(bytes) => line("binary", &format!("{} bytes", bytes.len())),
            // The close code and reason tell how IVS ends a session (token
            // expired, kicked, ...).
            Message::Close(Some(close)) => {
                let code = u16::from(close.code);
                line(
                    "closed",
                    &format!("code {code}, reason {:?}", close.reason.as_str()),
                );
                return Ok(());
            }
            Message::Close(None) => {
                line("closed", "without a close frame");
                return Ok(());
            }
            // Keep-alive: tungstenite answers pings by itself.
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    line("closed", "connection dropped");
    Ok(())
}

/// Looks chatters up the way RPlay's page does (`/live/user`, plus how long
/// they've been subscribed), each once. Runs beside the frame printing, so a
/// slow answer doesn't hold up the frames or their timestamps.
struct Lookups {
    http: reqwest::Client,
    creator: String,
    /// Everyone looked up so far.
    seen: HashSet<String>,
}

impl Lookups {
    fn on_frame(&mut self, frame: &Value) {
        match (frame["Type"].as_str(), frame["EventName"].as_str()) {
            // `insert` is false for someone already in the set.
            (Some("MESSAGE"), _) => {
                if let Some(user) = frame["Sender"]["UserId"].as_str()
                    && self.seen.insert(user.to_owned())
                {
                    self.spawn(user, "user");
                }
            }
            // Mostly sent for viewers who never chat: only chatters' changes
            // are interesting, and only seeing the new data shows what the
            // event was about.
            (Some("EVENT"), Some("updatedUserData")) => {
                if let Some(user) = frame["Attributes"]["userOid"].as_str()
                    && self.seen.contains(user)
                {
                    self.spawn(user, "user updated");
                }
            }
            _ => {}
        }
    }

    fn spawn(&self, user: &str, label: &'static str) {
        // The task may outlive this call (and this connection), so it gets
        // its own copies instead of borrowing. Cloning a reqwest client is
        // cheap: it's a shared handle to one connection pool.
        let http = self.http.clone();
        let creator = self.creator.clone();
        let user = user.to_owned();
        tokio::spawn(async move {
            match user_summary(&http, &creator, &user).await {
                Ok(summary) => line(label, &format!("{user} {summary}")),
                Err(error) => line(label, &format!("{user} error: {error:#}")),
            }
        });
    }
}

/// What RPlay's page knows about a chatter besides the message: names, the
/// admin flag, this channel's tiers they hold and how long they've
/// subscribed.
async fn user_summary(http: &reqwest::Client, creator: &str, user: &str) -> anyhow::Result<String> {
    let url = Url::parse_with_params(&format!("{API}/live/user"), [("userOid", user)])?;
    let info = read_json(http.get(url).send().await?)
        .await
        .context("GET /live/user")?;
    // Asked for this one channel only, so the whole answer can be printed.
    let url = Url::parse_with_params(
        &format!("{API}/account/subscription-durations"),
        [("users", user), ("creators", creator)],
    )?;
    let durations = read_json(http.get(url).send().await?)
        .await
        .context("GET /account/subscription-durations")?;

    let mut summary = pick(&info, USER_FIELDS);
    let subscriptions = &info["subscribingTo"];
    summary.insert(
        "subscribingTo.here".to_owned(),
        subscriptions[creator].clone(),
    );
    if let Some(channels) = subscriptions.as_object() {
        summary.insert("subscribingTo.len".to_owned(), channels.len().into());
    }
    summary.insert("subscriptionDurations".to_owned(), durations);
    Ok(Value::Object(summary).to_string())
}

/// RPlay's "live tool" login, as its OBS widgets use it: the streamer's
/// toolkey (from their studio) traded for a short-lived token that acts for
/// their own channel only. See RPLAY.md, "Restricted streams".
struct ToolSession {
    token: String,
    refresh_token: String,
    /// The creator id of the channel the toolkey belongs to.
    channel: String,
    expires_at: DateTime<Utc>,
}

impl ToolSession {
    async fn start(http: &reqwest::Client, toolkey: &str) -> anyhow::Result<Self> {
        let answer = post_json(http, "live/tool/session", toolkey, json!({}))
            .await
            .context("logging in with the toolkey")?;
        let session = Self {
            token: string(&answer, "token")?,
            refresh_token: string(&answer, "toolRefreshToken")?,
            channel: string(&answer["actor"], "oid")?,
            expires_at: string(&answer, "expiresAt")?.parse()?,
        };
        session.log("tool session");
        Ok(session)
    }

    /// Renews the token when it has less than 30 minutes left, as RPlay's
    /// site does.
    async fn renew_if_needed(&mut self, http: &reqwest::Client) -> anyhow::Result<()> {
        if self.expires_at - Utc::now() > TimeDelta::minutes(30) {
            return Ok(());
        }
        let body = json!({ "requestorOid": self.channel, "loginType": "live-tool" });
        let answer = post_json(http, "live/tool/refresh", &self.refresh_token, body)
            .await
            .context("renewing the toolkey session")?;
        self.token = string(&answer, "token")?;
        self.expires_at = string(&answer, "expiresAt")?.parse()?;
        // RPlay's site merges the answer into the old session, so a missing
        // refresh token means "keep the old one".
        if let Some(refresh_token) = answer["toolRefreshToken"].as_str() {
            self.refresh_token = refresh_token.to_owned();
        }
        self.log("tool renewed");
        Ok(())
    }

    fn log(&self, label: &str) {
        line(
            label,
            &format!("channel {}, expires {}", self.channel, self.expires_at),
        );
    }
}

/// The toolkey from `RPLAY_TOOLKEY`: the key itself, or a widget link that
/// contains it as `?toolkey=…` (that's how RPlay's studio hands it out).
fn toolkey(value: &str) -> String {
    let value = value.trim();
    Url::parse(value)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == "toolkey")
                .map(|(_, key)| key.into_owned())
        })
        .unwrap_or_else(|| value.to_owned())
}

/// The creator id from a bare id or a link to the channel: any rplay.live
/// page with the id in its path (`/live/<id>`, widget links, ...) or a custom
/// link (`/c/<name>/live`).
async fn resolve_creator(http: &reqwest::Client, input: &str) -> anyhow::Result<String> {
    let input = input.trim();
    if is_object_id(input) {
        return Ok(input.to_ascii_lowercase());
    }
    let link = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let segments: Vec<String> = Url::parse(&link)
        .ok()
        .and_then(|url| Some(url.path_segments()?.map(str::to_owned).collect()))
        .unwrap_or_default();
    if let Some(id) = segments.iter().find(|segment| is_object_id(segment)) {
        return Ok(id.to_ascii_lowercase());
    }
    let custom_name = segments
        .iter()
        .position(|segment| segment == "c")
        .and_then(|index| segments.get(index + 1))
        .filter(|name| !name.is_empty());
    let Some(name) = custom_name else {
        bail!("expected a 24-character creator id or an rplay.live link, got {input:?}");
    };

    // What RPlay's pop-out chat page does with a custom link.
    let url = Url::parse_with_params(
        &format!("{API}/account/getuser"),
        [("customUrl", name.as_str()), ("filter[]", "_id")],
    )?;
    let answer = read_json(http.get(url).send().await?)
        .await
        .with_context(|| format!("looking up the custom link {name:?}"))?;
    let id = answer["_id"]
        .as_str()
        .filter(|id| is_object_id(id))
        .context("the custom link lookup returned no creator id")?;
    line("custom link", &format!("{name} is channel {id}"));
    Ok(id.to_ascii_lowercase())
}

/// RPlay's ids are MongoDB object ids: 24 hex digits.
fn is_object_id(text: &str) -> bool {
    text.len() == 24 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// GET `https://api.rplay.live/<endpoint>?creatorOid=<creator>`, anonymous
/// or with the toolkey session.
async fn get_json(
    http: &reqwest::Client,
    endpoint: &str,
    creator: &str,
    tool: Option<&ToolSession>,
) -> anyhow::Result<Value> {
    let mut params = vec![("creatorOid", creator)];
    if let Some(tool) = tool {
        // What RPlay's site adds to live-tool requests (`applyLiveToolAuth`).
        params.extend([
            ("requestorOid", tool.channel.as_str()),
            ("loginType", "live-tool"),
        ]);
    }
    let mut request = http.get(Url::parse_with_params(
        &format!("{API}/{endpoint}"),
        params,
    )?);
    if let Some(tool) = tool {
        request = request.header(AUTHORIZATION, secret_header(&tool.token)?);
    }
    read_json(request.send().await?)
        .await
        .with_context(|| format!("GET /{endpoint}"))
}

/// POST with a key or token as the whole `Authorization` header (no
/// "Bearer"), as the live-tool login wants it.
async fn post_json(
    http: &reqwest::Client,
    endpoint: &str,
    authorization: &str,
    body: Value,
) -> anyhow::Result<Value> {
    let response = http
        .post(format!("{API}/{endpoint}"))
        .header(AUTHORIZATION, secret_header(authorization)?)
        .json(&body)
        .send()
        .await?;
    read_json(response)
        .await
        .with_context(|| format!("POST /{endpoint}"))
}

/// The answer as JSON. On an error status, the error includes the start of
/// the answer: RPlay's reason for refusing is what we're after.
async fn read_json(response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        // One line for the log (error pages are multi-line HTML).
        let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let body: String = body.chars().take(300).collect();
        bail!("HTTP {status}: {body}");
    }
    // RPlay gzips even when not asked to; reqwest's `gzip` feature unpacks it.
    Ok(response.json().await?)
}

/// A header value for a key or token. Marked sensitive, so it stays out of
/// debug output (reqwest's and tungstenite's logging included).
fn secret_header(value: &str) -> anyhow::Result<HeaderValue> {
    let mut header =
        HeaderValue::from_str(value).context("the key contains characters a header can't hold")?;
    header.set_sensitive(true);
    Ok(header)
}

fn play_summary(play: &Value) -> String {
    let mut summary = pick(play, PLAY_FIELDS);
    for &list in PLAY_LISTS {
        if let Some(items) = play[list].as_array() {
            summary.insert(format!("{list}.len"), items.len().into());
        }
    }
    Value::Object(summary).to_string()
}

/// The channel's tiers, lowest first (RPlay's order: a user with several
/// gets the last one they hold).
fn tiers_summary(play: &Value) -> String {
    let tiers = play["subscriptionTiers"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let tiers: Vec<Value> = tiers
        .iter()
        .map(|tier| Value::Object(pick(tier, TIER_FIELDS)))
        .collect();
    Value::Array(tiers).to_string()
}

fn token_summary(answer: &Value) -> String {
    Value::Object(pick(answer, TOKEN_FIELDS)).to_string()
}

/// The frame's type, plus the event name for events: "MESSAGE",
/// "EVENT donation", "EVENT aws:DELETE_MESSAGE". Up front for grepping.
fn label(frame: Option<&Value>) -> String {
    let Some(json) = frame else {
        return "not json".to_owned();
    };
    let kind = json["Type"].as_str().unwrap_or("?");
    match json["EventName"].as_str() {
        Some(event) => format!("{kind} {event}"),
        None => kind.to_owned(),
    }
}

/// A copy of only the given fields (those that exist).
fn pick(json: &Value, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|&field| Some((field.to_owned(), json.get(field)?.clone())))
        .collect()
}

/// A string field of an API answer, or an error naming it.
fn string(json: &Value, field: &str) -> anyhow::Result<String> {
    json[field]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("the answer has no `{field}`"))
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
