use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chat_websocket::Socket;
use chat_websocket::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};

/// Prints everything StreamElements' event gateway (Astro) sends for a
/// channel as raw JSON frames, before anything is decoded or filtered. For
/// collecting real tips and events to build the StreamElements source from
/// (see STREAMELEMENTS.md, the maintainer's research notes, kept outside the
/// public repository).
///
/// Astro needs one of the tokens from the StreamElements dashboard (avatar,
/// top right → the channel name): the JWT or the Overlay Token. Both are
/// secrets, so the token comes from the environment, not the command line
/// (which ends up in the shell history):
///
///   read -rs SE_TOKEN && export SE_TOKEN   # paste the token, then Enter
///   cargo run -p chat-streamelements --example raw_streamelements | tee /tmp/se-raw.log
///   unset SE_TOKEN
///
/// The token's kind is guessed from its shape (a JWT is three base64 parts
/// joined by dots, starting with `eyJ`); `SE_TOKEN_TYPE=jwt`, `apikey` (the
/// Overlay Token) or `oauth2` overrides the guess.
///
/// Optional arguments, in any order:
///
///   cargo run -p chat-streamelements --example raw_streamelements -- [account id] [topic ...]
///
/// - The account id (the channel's id, shown as "Account ID" on the same
///   dashboard page) is the `room` the topics are subscribed in. Without
///   it, it's read from the JWT, or looked up with it. The Overlay Token
///   needs it given.
/// - Topics start with `channel.`; without any, it subscribes to
///   [`DEFAULT_TOPICS`]. Each subscription's answer is printed, so a run
///   with the Overlay Token shows which topics that token may read.
///
/// Test tips from the dashboard (the activity feed's "emulate" and the tip
/// page) show up here like real ones, if Astro sends them at all: that's one
/// of the things to find out.
///
/// Before connecting, it asks StreamElements' REST API for the newest past
/// tip (`/kappa/v2/tips/<account id>?limit=1`), trying the token as
/// `Authorization: Bearer …` and `apikey …`: does the Overlay Token work
/// there, and does a tip carry its `currency`? Only the HTTP status, the
/// field names and a few harmless values (`currency`, `amount`, `provider`,
/// `status`, …) are printed, never the tipper's name, email or message.
///
/// When Astro announces a shutdown it hands out a reconnect token; this then
/// resumes the session, which keeps its subscriptions. Other dropped
/// connections are retried with fresh subscriptions. Stop it with Ctrl+C.
///
/// The log contains real names, amounts and messages: keep it out of the
/// repo. Tokens (and the reconnect token) are never printed.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = Token::from_env()?;
    let (room, topics) = arguments()?;
    line("token", &token.summary());
    let room = match room {
        Some(room) => room,
        None => room_from_token(&token).await?,
    };
    probe_tips(&token, &room).await;

    // An error before the first connection most likely means no network:
    // stop and say so. Later errors are retried, waiting longer after each
    // failure in a row.
    let mut connected_once = false;
    let mut delay = RETRY_FIRST;
    let mut resume: Option<String> = None;
    loop {
        // `take` empties `resume`: a reconnect token is used once.
        let resuming = resume.take();
        match connect(resuming.as_deref()).await {
            Ok(socket) => {
                connected_once = true;
                delay = RETRY_FIRST;
                let session = Session {
                    token: &token,
                    room: &room,
                    topics: &topics,
                    // A resumed session keeps its subscriptions (Astro's
                    // docs); subscribing again would only print errors.
                    subscribe: resuming.is_none(),
                };
                if let Err(error) = session.print_frames(socket, &mut resume).await {
                    line("error", &format!("{error:#}"));
                }
            }
            Err(error) if !connected_once => return Err(error),
            Err(error) => {
                line("error", &format!("{error:#}"));
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
        if resume.is_some() {
            line("reconnecting", "in 1 s, resuming the session");
            tokio::time::sleep(Duration::from_secs(1)).await;
        } else {
            line("reconnecting", &format!("in {} s", delay.as_secs()));
            tokio::time::sleep(delay).await;
        }
    }
}

const ASTRO: &str = "wss://astro.streamelements.com";
const API: &str = "https://api.streamelements.com/kappa/v2";
/// Tips and their moderation (refunds, hidden tips?) are what the source
/// needs. Activities show how StreamElements relays the platforms' own
/// events (bits, subs, Super Chats), which the source must leave out (we
/// read those from the platforms already), and whether tips show up there
/// too. The stream status is small and may help tell test tips from real
/// ones.
const DEFAULT_TOPICS: &[&str] = &[
    "channel.tips",
    "channel.tips.moderation",
    "channel.activities",
    "channel.stream.status",
];
/// Astro allows one subscribe command per 100 ms.
const SUBSCRIBE_GAP: Duration = Duration::from_millis(150);
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// JWT claims that are safe to print. Anything else stays hidden: a
/// StreamElements JWT may carry further secrets in its claims.
const SAFE_CLAIMS: &[&str] = &["channel", "role", "provider", "iss", "aud", "iat", "exp"];

/// The dashboard token and the name Astro's `token_type` field uses for it.
struct Token {
    secret: String,
    kind: String,
}

impl Token {
    fn from_env() -> anyhow::Result<Self> {
        let secret = std::env::var("SE_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .context(
                "no token: run `read -rs SE_TOKEN && export SE_TOKEN`, paste the JWT or the \
                 Overlay Token from the StreamElements dashboard, then Enter",
            )?;
        let kind = match std::env::var("SE_TOKEN_TYPE") {
            Ok(kind) if !kind.trim().is_empty() => kind.trim().to_owned(),
            _ if is_jwt(&secret) => "jwt".to_owned(),
            _ => "apikey".to_owned(),
        };
        Ok(Self { secret, kind })
    }

    /// The token's kind, and for a JWT the claims that are safe to print
    /// (names of the others, to learn what's in there).
    fn summary(&self) -> String {
        let Some(claims) = self.claims() else {
            return format!("{}, {} characters", self.kind, self.secret.len());
        };
        let mut safe = serde_json::Map::new();
        let mut hidden = Vec::new();
        for (name, value) in &claims {
            if SAFE_CLAIMS.contains(&name.as_str()) {
                safe.insert(name.clone(), value.clone());
            } else {
                hidden.push(name.as_str());
            }
        }
        let expiry = match claims.get("exp").and_then(Value::as_i64) {
            Some(exp) => match jiff::Timestamp::from_second(exp) {
                Ok(time) => format!("expires {time}"),
                Err(_) => format!("exp {exp} out of range"),
            },
            None => "no expiry".to_owned(),
        };
        format!(
            "{}, claims {} ({expiry}), hidden claims {hidden:?}",
            self.kind,
            Value::Object(safe)
        )
    }

    /// The JWT's claims (its middle part), read but not checked. `None` for
    /// anything that isn't a JWT.
    fn claims(&self) -> Option<serde_json::Map<String, Value>> {
        let part = self.secret.split('.').nth(1)?;
        let bytes = URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()?;
        match serde_json::from_slice(&bytes).ok()? {
            Value::Object(claims) => Some(claims),
            _ => None,
        }
    }
}

/// A JWT: header, claims and signature, base64-encoded and joined by dots.
/// The header is JSON, so it always starts with `eyJ` (`{"` in base64).
fn is_jwt(token: &str) -> bool {
    token.starts_with("eyJ") && token.split('.').count() == 3
}

/// The account id and the topics from the command line.
fn arguments() -> anyhow::Result<(Option<String>, Vec<String>)> {
    let mut room = None;
    let mut topics = Vec::new();
    for argument in std::env::args().skip(1) {
        if argument.starts_with("channel.") {
            topics.push(argument);
        } else if room.is_none() {
            room = Some(argument);
        } else {
            bail!(
                "usage: cargo run -p chat-streamelements --example raw_streamelements -- \
                 [account id] [channel.topic ...]"
            );
        }
    }
    if topics.is_empty() {
        topics = DEFAULT_TOPICS
            .iter()
            .map(|&topic| topic.to_owned())
            .collect();
    }
    Ok((room, topics))
}

/// The channel id from the JWT's `channel` claim or, failing that, from
/// StreamElements' API with the JWT as login.
async fn room_from_token(token: &Token) -> anyhow::Result<String> {
    if let Some(channel) = token
        .claims()
        .and_then(|claims| claims.get("channel")?.as_str().map(str::to_owned))
    {
        line("room", &format!("{channel} (from the JWT)"));
        return Ok(channel);
    }
    if token.kind != "jwt" {
        bail!(
            "the account id is needed with this token: pass it as the first argument (\"Account \
             ID\" on the dashboard page with the tokens)"
        );
    }
    let response = http_client()?
        .get(format!("{API}/channels/me"))
        .bearer_auth(&token.secret)
        .send()
        .await
        .context("looking up the channel")?;
    let status = response.status();
    if !status.is_success() {
        bail!("looking up the channel: HTTP {status}; pass the account id as the first argument");
    }
    let answer: Value = response.json().await?;
    let channel = answer["_id"]
        .as_str()
        .context("the channel lookup returned no `_id`")?
        .to_owned();
    line("room", &format!("{channel} (from /channels/me)"));
    Ok(channel)
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("chat-aggregator/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?)
}

/// Fields of a past tip whose values are printed: nothing about the person.
/// Everything else shows only as a field name.
const TIP_VALUES: &[&str] = &[
    "donation.currency",
    "donation.amount",
    "provider",
    "status",
    "approved",
    "deleted",
    "createdAt",
];

/// Asks the REST API for the newest past tip, with each way of sending the
/// token until one is accepted, and prints what came back (see the doc
/// comment of `main`). Only informational: a failure doesn't stop the
/// capture.
async fn probe_tips(token: &Token, room: &str) {
    let http = match http_client() {
        Ok(http) => http,
        Err(error) => return line("tips", &format!("error: {error:#}")),
    };
    let url = format!("{API}/tips/{room}?limit=1&offset=0&sort=-createdAt");
    for scheme in ["Bearer", "apikey"] {
        let header =
            match reqwest::header::HeaderValue::from_str(&format!("{scheme} {}", token.secret)) {
                Ok(mut header) => {
                    // Kept out of debug output (reqwest's logging included).
                    header.set_sensitive(true);
                    header
                }
                Err(_) => return line("tips", "the token can't go into a header"),
            };
        let response = http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, header)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                line("tips", &format!("{scheme}: error: {error:#}"));
                continue;
            }
        };
        let status = response.status();
        if !status.is_success() {
            // StreamElements' reason, shortened; it names no person.
            let body = response.text().await.unwrap_or_default();
            let body: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let body: String = body.chars().take(200).collect();
            line("tips", &format!("{scheme}: HTTP {status}: {body}"));
            continue;
        }
        match response.json::<Value>().await {
            Ok(answer) => line(
                "tips",
                &format!("{scheme}: HTTP {status}: {}", tips_summary(&answer)),
            ),
            Err(error) => line(
                "tips",
                &format!("{scheme}: HTTP {status}, not JSON: {error}"),
            ),
        }
        return;
    }
}

/// The answer's shape (field names, the total) and [`TIP_VALUES`] of the
/// newest tip.
fn tips_summary(answer: &Value) -> String {
    let mut summary = serde_json::Map::new();
    if let Some(total) = answer.get("total") {
        summary.insert("total".to_owned(), total.clone());
    }
    let Some(tip) = answer["docs"].as_array().and_then(|docs| docs.first()) else {
        summary.insert("docs".to_owned(), "none".into());
        return Value::Object(summary).to_string();
    };
    let mut names = Vec::new();
    field_names(tip, "", &mut names);
    summary.insert("fields".to_owned(), names.into());
    for path in TIP_VALUES {
        let value = path.split('.').fold(tip, |value, key| &value[key]);
        summary.insert((*path).to_owned(), value.clone());
    }
    Value::Object(summary).to_string()
}

/// Every field's dotted path, e.g. `donation.user.email`: the names only.
fn field_names(value: &Value, prefix: &str, names: &mut Vec<String>) {
    if let Value::Object(fields) = value {
        for (key, inner) in fields {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            field_names(inner, &path, names);
            names.push(path);
        }
    }
}

/// Opens the WebSocket: a new session, or the old one again with the
/// reconnect token Astro handed out before shutting down.
async fn connect(resume: Option<&str>) -> anyhow::Result<Socket> {
    let url = match resume {
        Some(reconnect_token) => format!("{ASTRO}/?reconnect_token={reconnect_token}"),
        None => ASTRO.to_owned(),
    };
    let socket = chat_websocket::connect(url)
        .await
        .context("connecting to Astro")?;
    let how = if resume.is_some() {
        "resumed session"
    } else {
        "new session"
    };
    line("connected", &format!("{ASTRO} ({how})"));
    Ok(socket)
}

/// What one connection needs to know.
struct Session<'a> {
    token: &'a Token,
    room: &'a str,
    topics: &'a [String],
    /// Subscribe once Astro says welcome (not for a resumed session).
    subscribe: bool,
}

impl Session<'_> {
    /// Prints frames until the server closes the connection. A reconnect
    /// token Astro hands out ends up in `resume`.
    async fn print_frames(
        &self,
        mut socket: Socket,
        resume: &mut Option<String>,
    ) -> anyhow::Result<()> {
        while let Some(frame) = socket.next().await {
            match frame? {
                Message::Text(text) => {
                    let json = serde_json::from_str::<Value>(&text).ok();
                    let reconnect_token = json
                        .as_ref()
                        .and_then(|json| json["data"]["reconnect_token"].as_str());
                    line(&label(json.as_ref()), &self.redact(&text, reconnect_token));
                    if let Some(reconnect_token) = reconnect_token {
                        *resume = Some(reconnect_token.to_owned());
                    }
                    // Subscribing before the welcome might work too, but
                    // Astro's examples wait for it.
                    let welcome = json.as_ref().is_some_and(|json| json["type"] == "welcome");
                    if welcome && self.subscribe {
                        self.subscribe_all(&mut socket).await?;
                    }
                }
                Message::Binary(bytes) => line("binary", &format!("{} bytes", bytes.len())),
                // The close code and reason tell how Astro ends a session
                // (shutdown, bad token, rate limit, ...).
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
                // Keep-alive: Astro pings every 30 s, tungstenite answers by
                // itself (as long as we keep reading).
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        line("closed", "connection dropped");
        Ok(())
    }

    /// Subscribes to every topic, each with its name as the `nonce`, which
    /// Astro echoes in its answer: that tells the answers apart.
    async fn subscribe_all(&self, socket: &mut Socket) -> anyhow::Result<()> {
        for topic in self.topics {
            let command = json!({
                "type": "subscribe",
                "nonce": topic,
                "data": {
                    "topic": topic,
                    "room": self.room,
                    "token": self.token.secret,
                    "token_type": self.token.kind,
                },
            });
            socket.send(Message::text(command.to_string())).await?;
            line(
                "subscribe",
                &format!("{topic} in room {} ({})", self.room, self.token.kind),
            );
            tokio::time::sleep(SUBSCRIBE_GAP).await;
        }
        Ok(())
    }

    /// The frame as received, minus secrets: our token (should Astro ever
    /// echo it) and the reconnect token (it takes over the session).
    fn redact(&self, text: &str, reconnect_token: Option<&str>) -> String {
        let text = text.replace(&self.token.secret, "<token>");
        match reconnect_token {
            Some(reconnect_token) if !reconnect_token.is_empty() => {
                text.replace(reconnect_token, "<reconnect token>")
            }
            _ => text,
        }
    }
}

/// The frame's type, plus the topic and any error: "welcome",
/// "response channel.tips", "message channel.tips". Up front for grepping.
fn label(frame: Option<&Value>) -> String {
    let Some(json) = frame else {
        return "not json".to_owned();
    };
    let mut label = json["type"].as_str().unwrap_or("?").to_owned();
    // Answers to our subscriptions carry the topic as their nonce.
    if let Some(topic) = json["topic"].as_str().or(json["nonce"].as_str()) {
        label = format!("{label} {topic}");
    }
    if let Some(error) = json["error"].as_str().filter(|error| !error.is_empty()) {
        label = format!("{label} error {error}");
    }
    label
}

fn line(label: &str, text: &str) {
    println!(
        "{} [{label}] {text}",
        jiff::Zoned::now().strftime("%H:%M:%S%.3f")
    );
}
