use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// Prints everything Streamlabs' Socket API sends for an account as raw
/// frames, before anything is decoded or filtered. For collecting real
/// donations and events to build the Streamlabs source from (see
/// STREAMLABS.md, the maintainer's research notes, kept outside the public
/// repository).
///
/// It needs the account's Socket API Token (Streamlabs dashboard: Settings
/// → API Settings → API Tokens → "Your Socket API Token"). That's a secret,
/// so it comes from the environment, not the command line (which ends up in
/// the shell history):
///
///   read -rs SL_TOKEN && export SL_TOKEN   # paste the token, then Enter
///   cargo run -p chat-streamlabs --example raw_streamlabs | tee /tmp/sl-raw.log
///   unset SL_TOKEN
///
/// Streamlabs speaks socket.io 2, which is Engine.IO 3 underneath, done by
/// hand here: the server opens with `0{…}` (its ping timing), socket.io
/// adds `40` (connected), and events come as `42["event",{…}]`. Unlike
/// Engine.IO 4, the *client* pings (`2`) and the server answers (`3`):
/// without our pings, Streamlabs drops the connection. Pings and pongs
/// aren't printed; every other frame is, with a label up front for
/// grepping (`[event donation for streamlabs]`).
///
/// A wrong token gets `44"Authentication error"` right after the open
/// packet (seen 2026-11-07), and the connection then just stays open: the
/// example stops there. Test donations from the dashboard are marked
/// `isTest`, the alert box editor's fake alerts `isPreview`: the labels
/// say "(test)" and "(preview)". Still to find out: what a real donation
/// looks like (STREAMLABS.md).
///
/// Dropped connections are retried, waiting longer after each failure in a
/// row. Stop it with Ctrl+C. The log contains real names, amounts and
/// messages: keep it out of the repo. The token is never printed.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = std::env::var("SL_TOKEN")
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .context(
            "no token: run `read -rs SL_TOKEN && export SL_TOKEN`, paste the Socket API Token \
             from the Streamlabs dashboard (Settings → API Settings → API Tokens), then Enter",
        )?;
    // From here on, `line` blanks it out of everything it prints.
    let _ = SECRET.set(token.clone());
    line("token", &format!("{} characters", token.len()));
    let tls = tls_config()?;

    // An error before the first connection most likely means no network
    // (or a refused token): stop and say so. Later errors are retried.
    let mut connected_once = false;
    let mut delay = RETRY_FIRST;
    loop {
        match connect(&tls, &token).await {
            Ok(socket) => {
                connected_once = true;
                delay = RETRY_FIRST;
                match print_frames(socket).await {
                    Ok(()) => {}
                    // Retrying won't help: the token is wrong.
                    Err(error) if error.is::<Refused>() => return Err(error),
                    Err(error) => line("error", &format!("{error:#}")),
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

const SOCKETS: &str = "wss://sockets.streamlabs.com/socket.io/";
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// Until the server's open packet says otherwise (Engine.IO 3's defaults).
const PING_INTERVAL: Duration = Duration::from_secs(25);
const PING_TIMEOUT: Duration = Duration::from_secs(60);

/// The token, for blanking it out of printed text: a WebSocket error may
/// well quote the URL it failed on, and the token is in that URL.
static SECRET: OnceLock<String> = OnceLock::new();

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Streamlabs answered with a socket.io error (a wrong token): its reason.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Streamlabs refused the connection ({}): copy the Socket API Token again",
            self.0
        )
    }
}

impl std::error::Error for Refused {}

async fn connect(tls: &Arc<rustls::ClientConfig>, token: &str) -> anyhow::Result<Socket> {
    // The token goes into the address (as in Streamlabs' own example);
    // `EIO=3` asks for Engine.IO 3, `transport=websocket` skips the long
    // polling socket.io starts with otherwise.
    let url = format!(
        "{SOCKETS}?token={}&EIO=3&transport=websocket",
        percent_encoded(token)
    );
    let connector = Connector::Rustls(Arc::clone(tls));
    match tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector)).await
    {
        Ok((socket, _response)) => {
            line("connected", SOCKETS);
            Ok(socket)
        }
        // A refused handshake: the status and Streamlabs' reason are the
        // interesting part (likely how a wrong token shows).
        Err(tungstenite::Error::Http(response)) => {
            let status = response.status();
            let body = response
                .body()
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            let body: String = body.chars().take(300).collect();
            anyhow::bail!("handshake refused: HTTP {status}: {body}")
        }
        Err(error) => Err(error).context("connecting to Streamlabs"),
    }
}

/// Prints frames until the connection ends, pinging as Engine.IO 3 wants.
async fn print_frames(mut socket: Socket) -> anyhow::Result<()> {
    let mut ping_interval = PING_INTERVAL;
    let mut ping_timeout = PING_TIMEOUT;
    let mut next_ping = Instant::now() + ping_interval;
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            frame = socket.next() => {
                let Some(frame) = frame else {
                    line("closed", "connection dropped");
                    return Ok(());
                };
                heard = Instant::now();
                match frame? {
                    Message::Text(text) => {
                        let packet = Packet::of(&text);
                        if let Packet::Open { interval, timeout } = &packet {
                            ping_interval = *interval;
                            ping_timeout = *timeout;
                            next_ping = Instant::now() + ping_interval;
                        }
                        if !matches!(packet, Packet::Pong) {
                            line(&packet.label(), &text);
                        }
                        if let Some(reason) = packet.error() {
                            return Err(Refused(reason).into());
                        }
                    }
                    Message::Binary(bytes) => line("binary", &format!("{} bytes", bytes.len())),
                    Message::Close(Some(close)) => {
                        let code = u16::from(close.code);
                        line("closed", &format!("code {code}, reason {:?}", close.reason.as_str()));
                        return Ok(());
                    }
                    Message::Close(None) => {
                        line("closed", "without a close frame");
                        return Ok(());
                    }
                    // WebSocket-level keep-alive, answered by tungstenite.
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            () = tokio::time::sleep_until(next_ping) => {
                // Engine.IO 3: the client pings, the server answers `3`.
                socket.send(Message::text("2")).await.context("pinging Streamlabs")?;
                next_ping = Instant::now() + ping_interval;
            }
            () = tokio::time::sleep_until(heard + ping_interval + ping_timeout) => {
                anyhow::bail!(
                    "no answer from Streamlabs for {} s",
                    (ping_interval + ping_timeout).as_secs()
                );
            }
        }
    }
}

/// One Engine.IO 3 text frame, as far as the example cares.
enum Packet {
    /// `0{…}`: the session, with the server's ping timing.
    Open {
        interval: Duration,
        timeout: Duration,
    },
    /// `3`: the answer to our ping.
    Pong,
    /// `4…`: a socket.io packet; the rest of the frame after the `4`.
    SocketIo(String),
    /// Anything else (`1` close, `6` noop, …): its type digit.
    Other(char),
}

impl Packet {
    fn of(text: &str) -> Self {
        let mut chars = text.chars();
        match chars.next() {
            Some('0') => {
                let open: Value = serde_json::from_str(chars.as_str()).unwrap_or_default();
                let ms = |key: &str, default: Duration| {
                    open[key].as_u64().map_or(default, Duration::from_millis)
                };
                Packet::Open {
                    interval: ms("pingInterval", PING_INTERVAL),
                    timeout: ms("pingTimeout", PING_TIMEOUT),
                }
            }
            Some('3') => Packet::Pong,
            Some('4') => Packet::SocketIo(chars.as_str().to_owned()),
            Some(other) => Packet::Other(other),
            None => Packet::Other(' '),
        }
    }

    /// A socket.io error packet's reason (`44"Authentication error"`).
    fn error(&self) -> Option<String> {
        let Packet::SocketIo(rest) = self else {
            return None;
        };
        let reason = rest.strip_prefix('4')?;
        Some(serde_json::from_str::<String>(reason).unwrap_or_else(|_| reason.to_owned()))
    }

    /// "open", "connect", "event donation for streamlabs", "error", …
    fn label(&self) -> String {
        match self {
            Packet::Open { .. } => "open".into(),
            Packet::Pong => "pong".into(),
            Packet::Other(kind) => format!("engine.io {kind}"),
            Packet::SocketIo(rest) => {
                let mut chars = rest.chars();
                match chars.next() {
                    Some('0') => "connect".into(),
                    Some('1') => "disconnect".into(),
                    Some('2') => event_label(chars.as_str()),
                    Some('4') => "error".into(),
                    Some(other) => format!("socket.io {other}"),
                    None => "socket.io".into(),
                }
            }
        }
    }
}

/// `["event",{"type":"donation","for":"streamlabs",…}]` as "event donation
/// for streamlabs", plus " (test)" for the dashboard's test buttons and
/// " (preview)" for the alert box editor's fake alerts (it plays one every
/// ~18 s while it's open). A real donation has neither flag set, so its
/// label stays plain.
fn event_label(payload: &str) -> String {
    let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(payload) else {
        return "event (not json)".into();
    };
    let name = parts.first().and_then(Value::as_str).unwrap_or("?");
    let data = parts.get(1).cloned().unwrap_or_default();
    let mut label = name.to_owned();
    if let Some(kind) = data["type"].as_str() {
        label = format!("{label} {kind}");
    }
    if let Some(service) = data["for"].as_str() {
        label = format!("{label} for {service}");
    }
    // Seen in the first entry of `message` (2026-11-07); `== true` is
    // false for a missing field or anything that isn't the JSON `true`.
    let flag = |key: &str| data["message"][0][key] == true;
    if flag("isTest") {
        label.push_str(" (test)");
    }
    if flag("isPreview") {
        label.push_str(" (preview)");
    }
    label
}

/// The token for a URL's query: everything but letters, digits and
/// `-._~` as `%XX`. (Socket tokens look URL-safe already; this makes sure.)
fn percent_encoded(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn line(label: &str, text: &str) {
    let text = match SECRET.get() {
        Some(secret) => text
            .replace(secret.as_str(), "<token>")
            .replace(&percent_encoded(secret), "<token>"),
        None => text.to_owned(),
    };
    println!(
        "{} [{label}] {text}",
        jiff::Zoned::now().strftime("%H:%M:%S%.3f")
    );
}

/// TLS for the WebSocket, with the crypto backend chosen explicitly (see
/// chat-streamelements' `tls_config`).
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
