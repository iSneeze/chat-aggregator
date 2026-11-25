//! RPlay's HTTP API, as far as the source needs it (see RPLAY.md, "How the
//! website gets its chat" and "The toolkey"): logging in with the
//! streamer's toolkey the way RPlay's own OBS widgets do, the channel's
//! state and a chat token as the streamer, the emote lists, nickname
//! lookups. And opening the chat WebSocket (Amazon IVS).

use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::SetupError;
use chat_websocket::Socket;
use chat_websocket::tungstenite::{self, client::IntoClientRequest};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::emotes::{self, Emotes};

const API: &str = "https://api.rplay.live";
const USER_AGENT: &str = concat!("chat-aggregator/", env!("CARGO_PKG_VERSION"));
/// A nickname lookup holds up the donation it's for: not for long.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn http_client() -> anyhow::Result<reqwest::Client> {
    // RPlay gzips its answers even unasked; the workspace's reqwest has
    // the `gzip` feature, which unpacks them.
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()?)
}

/// RPlay answered with an error status. Its own type, so the source can
/// tell a refused chat token (403) from other problems.
#[derive(Debug)]
pub(crate) struct HttpError {
    pub status: StatusCode,
    /// What was asked, for the message ("GET /live/chattoken").
    what: &'static str,
    /// RPlay's reason: the `code` of its JSON errors, or the start of the
    /// answer.
    reason: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: HTTP {}", self.what, self.status)?;
        if !self.reason.is_empty() {
            write!(f, " ({})", self.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpError {}

/// A live-tool session: what RPlay's OBS widgets get for the toolkey. It
/// acts for the streamer, on their own channel only.
pub(crate) struct Session {
    /// Sent as the `Authorization` header (no "Bearer"). A secret: never
    /// logged.
    token: String,
    /// The creator id of the channel the toolkey belongs to.
    pub channel: String,
    /// When the session's token runs out, if RPlay said.
    pub expires_at: Option<jiff::Timestamp>,
}

impl Session {
    /// Logs in with the toolkey (`POST /live/tool/session`). A key RPlay
    /// refuses (reset in the studio, mistyped) is a `SetupError`: only the
    /// streamer can fix that.
    pub(crate) async fn start(http: &reqwest::Client, toolkey: &str) -> anyhow::Result<Self> {
        let response = http
            .post(format!("{API}/live/tool/session"))
            .header(AUTHORIZATION, secret_header(toolkey)?)
            .json(&json!({}))
            .send()
            .await
            .context("logging in to RPlay")?;
        let answer = match read_json(response, "POST /live/tool/session").await {
            Ok(answer) => answer,
            Err(e) => {
                // A 4xx is about the key (RPlay answers 401
                // LIVE_TOOL_UNAUTHORIZED to a wrong one); a 5xx is RPlay
                // being down, worth retrying.
                if let Some(http) = chat_core::find_error::<HttpError>(&e)
                    && http.status.is_client_error()
                {
                    warn!("RPlay refused the widget link: {http}");
                    return Err(SetupError(
                        "RPlay doesn't accept this widget link (anymore). Was it reset in the RPlay \
                         studio? Copy the Stream Chat link again (studio → Dashboard Widget → \
                         Generate Widget URLs → Show), then remove this source and add it again \
                         with the new link"
                            .into(),
                    )
                    .into());
                }
                return Err(e);
            }
        };
        let token = answer["token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .context("RPlay's login answer has no token")?;
        let channel = answer["actor"]["oid"]
            .as_str()
            .filter(|oid| is_object_id(oid))
            .context("RPlay's login answer doesn't say whose channel it is")?;
        let expires_at = answer["expiresAt"].as_str().and_then(|t| t.parse().ok());
        info!(channel, expires = ?expires_at, "logged in to RPlay with the widget link");
        Ok(Self {
            token: token.to_owned(),
            channel: channel.to_ascii_lowercase(),
            expires_at,
        })
    }

    /// The channel's state (`GET /live/play`), as the streamer.
    pub(crate) async fn play(&self, http: &reqwest::Client) -> anyhow::Result<Play> {
        let answer = self.get(http, "live/play", "GET /live/play").await?;
        // Read into `Play`, which names only what we use: serde skips every
        // other field, so the answer's secrets (as the streamer, it includes
        // the stream key) are never kept, and can't end up in a log.
        serde_json::from_value(answer).context("reading RPlay's channel state")
    }

    /// A token for the chat WebSocket (`GET /live/chattoken`), as the
    /// streamer. A refusal is an [`HttpError`].
    pub(crate) async fn chat_token(&self, http: &reqwest::Client) -> anyhow::Result<ChatToken> {
        let answer = self
            .get(http, "live/chattoken", "GET /live/chattoken")
            .await?;
        let token: ChatToken =
            serde_json::from_value(answer).context("reading RPlay's chat token")?;
        if token.token.is_empty() {
            bail!("RPlay's chat token answer has no token");
        }
        Ok(token)
    }

    /// A GET with the live-tool login, as RPlay's site sends it
    /// (`applyLiveToolAuth`): the token as `Authorization`, and who asks
    /// as query parameters.
    async fn get(
        &self,
        http: &reqwest::Client,
        endpoint: &str,
        what: &'static str,
    ) -> anyhow::Result<Value> {
        let channel = self.channel.as_str();
        let url = Url::parse_with_params(
            &format!("{API}/{endpoint}"),
            [
                ("creatorOid", channel),
                ("requestorOid", channel),
                ("loginType", "live-tool"),
            ],
        )?;
        let response = http
            .get(url)
            .header(AUTHORIZATION, secret_header(&self.token)?)
            .send()
            .await
            .with_context(|| what.to_owned())?;
        read_json(response, what).await
    }
}

/// What the source uses of `/live/play`. Every field is optional: a field
/// RPlay leaves out or sends as `null` mustn't fail the connection.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Play {
    /// The chat server, `wss://edge.ivschat.<region>.amazonaws.com`.
    pub chat_url: Option<String>,
    /// `"offline"`, or how the channel streams (`"twitch"` for a stream
    /// embedded from Twitch, ...).
    pub stream_state: Option<String>,
    pub banned_words: Option<Vec<String>>,
    pub managers: Option<Vec<Manager>>,
    pub use_anonymous_chat: Option<bool>,
    // Only logged, to learn how restricted streams behave (RPLAY.md,
    // "Restricted streams").
    pub accessible: Option<bool>,
    pub allow_anonymous: Option<bool>,
    pub use_subscriber: Option<bool>,
    pub use_ticket: Option<bool>,
    pub use_secret_key: Option<bool>,
}

/// A manager as `/live/play` lists them. Only the id is read: the entries
/// also hold email addresses.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Manager {
    pub user_oid: Option<String>,
}

impl Play {
    pub(crate) fn is_live(&self) -> bool {
        self.stream_state
            .as_deref()
            .is_some_and(|state| !state.is_empty() && state != "offline")
    }

    pub(crate) fn log(&self) {
        info!(
            stream_state = self.stream_state.as_deref().unwrap_or_default(),
            accessible = ?self.accessible,
            allow_anonymous = ?self.allow_anonymous,
            use_subscriber = ?self.use_subscriber,
            use_ticket = ?self.use_ticket,
            use_secret_key = ?self.use_secret_key,
            anonymous_chat = ?self.use_anonymous_chat,
            banned_words = self.banned_words.as_ref().map_or(0, Vec::len),
            managers = self.managers.as_ref().map_or(0, Vec::len),
            "RPlay channel state"
        );
    }
}

/// `/live/chattoken`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ChatToken {
    /// A secret: never logged.
    pub token: String,
    /// When IVS ends the session (60 minutes after the token was made):
    /// a string or a number, by the look of it, so read loosely.
    pub session_expiration_time: Option<Value>,
    pub can_send_message: Option<bool>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Written by hand to leave the token out.
        f.debug_struct("Session")
            .field("channel", &self.channel)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl ChatToken {
    /// When IVS will end the session, if the answer says.
    pub(crate) fn session_ends(&self) -> Option<jiff::Timestamp> {
        match self.session_expiration_time.as_ref()? {
            Value::String(time) => time.parse().ok(),
            Value::Number(ms) => jiff::Timestamp::from_millisecond(ms.as_i64()?).ok(),
            _ => None,
        }
    }
}

/// The channel's emotes: RPlay's free set and the channel's own. Either
/// list failing just leaves its emotes as text.
pub(crate) async fn emotes(http: &reqwest::Client, channel: &str) -> Emotes {
    let mut lists = Vec::new();
    for (url, what, list) in [
        (
            format!("{API}/emoticon/group/free"),
            "GET /emoticon/group/free",
            "free",
        ),
        (
            format!("{API}/emoticon/group/creator?creatorOid={channel}"),
            "GET /emoticon/group/creator",
            "channel",
        ),
    ] {
        let answer = match http.get(&url).send().await {
            Ok(response) => read_json(response, what).await,
            Err(e) => Err(e.into()),
        };
        match answer {
            Ok(answer) => {
                // Whether the channel's own emotes arrived, and how many are
                // for a tier (which the source can't check per sender yet).
                let (all, for_a_tier) = emotes::count(&answer);
                info!(list, emotes = all, for_a_tier, "RPlay emotes");
                lists.push(answer);
            }
            Err(e) => warn!("RPlay emotes stay text: {e:#}"),
        }
    }
    Emotes::from_lists(&lists)
}

/// A user's nickname (`GET /live/user`, no login needed): donations don't
/// carry the sender's name. `None` if RPlay doesn't answer in time.
pub(crate) async fn nickname(http: &reqwest::Client, user_id: &str) -> Option<String> {
    let url = Url::parse_with_params(&format!("{API}/live/user"), [("userOid", user_id)]).ok()?;
    let lookup = async {
        let response = http.get(url).send().await?;
        read_json(response, "GET /live/user").await
    };
    match tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await {
        Ok(Ok(user)) => user["nickname"].as_str().map(str::to_owned),
        Ok(Err(e)) => {
            warn!("looking up an RPlay donor's name: {e:#}");
            None
        }
        Err(_) => {
            warn!("looking up an RPlay donor's name took too long");
            None
        }
    }
}

/// Opens the chat WebSocket with a chat token.
pub(crate) async fn open(chat_url: &str, token: &str) -> anyhow::Result<Socket> {
    // The token goes to whatever address RPlay names: at least never
    // unencrypted.
    if !chat_url.starts_with("wss://") {
        bail!("RPlay names an odd chat server: {chat_url:?}");
    }
    let mut request = chat_url.into_client_request()?;
    // Amazon IVS takes the token as the WebSocket *subprotocol*, because
    // browsers can't add other headers to a WebSocket. The server echoes it
    // back, which tungstenite insists on.
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", secret_header(token)?);
    match chat_websocket::connect(request).await {
        Ok(socket) => Ok(socket),
        // Only the status: the response would echo the token.
        Err(tungstenite::Error::Http(response)) => bail!(
            "RPlay's chat server refused the connection: HTTP {}",
            response.status()
        ),
        Err(e) => Err(e).context("connecting to RPlay's chat server"),
    }
}

/// The answer as JSON, or an [`HttpError`] with RPlay's reason.
async fn read_json(response: reqwest::Response, what: &'static str) -> anyhow::Result<Value> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(HttpError {
            status,
            what,
            reason: reason(&body),
        }
        .into());
    }
    response
        .json()
        .await
        .with_context(|| format!("{what}: reading the answer"))
}

/// RPlay's reason for an error: the `code` of its JSON errors
/// (`LIVE_TOOL_UNAUTHORIZED`), else the start of the answer, on one line
/// (error pages are multi-line HTML).
fn reason(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<Value>(body)
        && let Some(code) = json["code"].as_str()
    {
        return code.to_owned();
    }
    let line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars().take(120).collect()
}

/// A header value for a key or token, marked sensitive: it stays out of
/// debug output (reqwest's and tungstenite's logging included).
fn secret_header(value: &str) -> anyhow::Result<HeaderValue> {
    let mut header = HeaderValue::from_str(value)
        .context("the key contains characters an HTTP header can't hold")?;
    header.set_sensitive(true);
    Ok(header)
}

/// RPlay's ids are MongoDB object ids: 24 hex digits.
pub(crate) fn is_object_id(text: &str) -> bool {
    text.len() == 24 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_state_keeps_only_what_is_used() {
        // `/live/play` in the shape seen (values made up), as the streamer
        // gets it: with a stream key, which must not survive the reading.
        let answer = json!({
            "chatUrl": "wss://edge.ivschat.ap-northeast-1.amazonaws.com",
            "streamState": "live",
            "bannedWords": ["bad"],
            "managers": [{ "userOid": "bbbbbbbbbbbbbbbbbbbbbbbb", "nickname": "Mod", "email": "mod@example.com" }],
            "useAnonymousChat": null,
            "accessible": true,
            "rplayLiveStreamKey": "sk_secret",
            "twitchToken": "",
        });
        let play: Play = serde_json::from_value(answer).unwrap();
        assert!(play.is_live());
        assert_eq!(play.banned_words.as_deref(), Some(&["bad".to_owned()][..]));
        assert_eq!(play.use_anonymous_chat, None);
        let printed = format!("{play:?}");
        assert!(!printed.contains("sk_secret") && !printed.contains("example.com"));
    }

    #[test]
    fn session_end_as_text_or_number() {
        let text: ChatToken = serde_json::from_value(json!({
            "token": "t", "sessionExpirationTime": "2026-10-12T11:00:00.000Z"
        }))
        .unwrap();
        assert_eq!(
            text.session_ends().unwrap().to_string(),
            "2026-10-12T11:00:00Z"
        );
        let number: ChatToken = serde_json::from_value(json!({
            "token": "t", "sessionExpirationTime": 1_791_802_800_000_i64
        }))
        .unwrap();
        assert_eq!(
            number.session_ends().unwrap().as_millisecond(),
            1_791_802_800_000
        );
        let none: ChatToken = serde_json::from_value(json!({ "token": "t" })).unwrap();
        assert_eq!(none.session_ends(), None);
    }

    #[test]
    fn rplays_reasons() {
        assert_eq!(
            reason(r#"{"code":"LIVE_TOOL_UNAUTHORIZED"}"#),
            "LIVE_TOOL_UNAUTHORIZED"
        );
        assert_eq!(reason("Forbidden"), "Forbidden");
        assert_eq!(
            reason("<html>\n  <body>oops</body>\n</html>"),
            "<html> <body>oops</body> </html>"
        );
        assert_eq!(reason(&"x".repeat(500)).len(), 120);
    }
}
