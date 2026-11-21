//! The OBS output: talks to obs-websocket, the remote control built into
//! OBS Studio 28 and newer (Tools → WebSocket Server Settings), version 5 of
//! its protocol. Only the few requests chat reactions need, so our own small
//! client rather than a library.
//!
//! ```text
//! connect → Hello (maybe a login challenge) → Identify → Identified
//!         → Request ⇄ RequestResponse, one at a time
//! ```
//!
//! **Timed effects** ("this filter on for 30 s") are this task's job: it
//! remembers when each ends (`Timed`), across reconnects, and switches it
//! back when the time is up, when the output stops (rules changed, the app
//! quits), or, if OBS was away at that moment, right after reconnecting.
//! Firing again while one runs makes it last longer; it doesn't stack.
//! Something that was already on before the reaction is left on.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chat_core::secrets::SecretStore;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::OBS_PASSWORD;
use crate::output::{CONNECT_TIMEOUT, Outcome, Waiting, pause, taking_jobs};
use crate::reactor::OutputState;

/// OBS answers a request within milliseconds; much longer means the
/// connection is dead without having said so.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// obs-websocket's close code for a wrong password.
const AUTHENTICATION_FAILED: u16 = 4009;
/// The protocol version we speak (obs-websocket 5.x).
const RPC_VERSION: u64 = 1;

/// What a reaction does in OBS. Built from the rule's `Action` by the
/// reactor; every name in here comes from the streamer's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// A source's filter on, for `duration` or for good.
    Filter {
        source: String,
        filter: String,
        duration: Option<Duration>,
    },
    /// A scene's source shown, for `duration` or for good.
    Show {
        scene: String,
        source: String,
        duration: Option<Duration>,
    },
    /// A media source played from the start.
    Restart { source: String },
}

pub(crate) type Job = crate::output::Job<Effect>;

/// Something a timed effect switched on, to switch off again.
// `Ord`: kept in a `BTreeMap`, so they're switched back in a fixed order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Switch {
    Filter { source: String, filter: String },
    Item { scene: String, source: String },
}

impl fmt::Display for Switch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Switch::Filter { source, filter } => write!(f, "filter \"{filter}\" on \"{source}\""),
            Switch::Item { scene, source } => write!(f, "\"{source}\" in scene \"{scene}\""),
        }
    }
}

/// Timed effects still on, with when they end.
#[derive(Default)]
struct Timed(BTreeMap<Switch, Instant>);

impl Timed {
    fn next_end(&self) -> Option<Instant> {
        self.0.values().min().copied()
    }

    /// The effects over at `now`.
    fn due(&self, now: Instant) -> Vec<Switch> {
        let over = self.0.iter().filter(|(_, end)| **end <= now);
        over.map(|(switch, _)| switch.clone()).collect()
    }
}

/// Runs until `jobs` closes (the reactor no longer needs OBS), then
/// switches back what's still on. `secrets`: where the password is, if OBS
/// asks for one (`None`: nowhere, e.g. tests). `wake` cuts a pause between
/// connection attempts short (a new password was stored).
pub(crate) async fn run(
    url: String,
    queue_secs: Arc<std::sync::atomic::AtomicU32>,
    secrets: Option<SecretStore>,
    mut jobs: mpsc::Receiver<Job>,
    state: watch::Sender<OutputState>,
    wake: Arc<Notify>,
) {
    let url = url.as_str();
    let mut waiting = Waiting::new("OBS", queue_secs);
    let mut timed = Timed::default();
    // Read from the store when OBS first asks for it, and again only after
    // OBS refused it: a keyring may ask the user to unlock it each time.
    let mut password = None;
    // A job whose request broke off with the connection: one more try on
    // a fresh connection (see the Warudo output).
    let mut retry: Option<Job> = None;
    let mut failures = 0;
    loop {
        let connect = Obs::connect(url, secrets.as_ref(), &mut password);
        let connect = tokio::time::timeout(CONNECT_TIMEOUT, connect);
        let Some(connected) = taking_jobs(connect, &mut jobs, &mut waiting).await else {
            gave_up(&timed);
            return;
        };
        let mut obs = match connected {
            Ok(Ok(obs)) => obs,
            failed => {
                let error = match failed {
                    Ok(Err(e)) => e,
                    _ => ConnectError::Unreachable("no answer".into()),
                };
                if matches!(error, ConnectError::WrongPassword) {
                    password = None; // maybe a new one is stored by the next try
                }
                // Once per outage, not every few seconds while OBS is closed.
                if failures == 0 {
                    info!("can't connect to OBS at {url}: {error}");
                }
                failures += 1;
                waiting.extend(retry.take());
                state.send_replace(OutputState::Retrying {
                    error: error.to_string(),
                });
                let pause = pause(failures, &wake);
                if taking_jobs(pause, &mut jobs, &mut waiting).await.is_none() {
                    gave_up(&timed);
                    return;
                }
                continue;
            }
        };
        info!("connected to OBS at {url}");
        failures = 0;
        state.send_replace(OutputState::Connected);
        match session(&mut obs, &mut jobs, &mut waiting, &mut timed, retry.take()).await {
            Ended::Unneeded => return,
            Ended::Lost { unsent } => {
                info!("lost the connection to OBS");
                retry = unsent;
                state.send_replace(OutputState::Connecting);
            }
        }
    }
}

/// Stopping without a connection: what's still on stays on.
fn gave_up(timed: &Timed) {
    for switch in timed.0.keys() {
        warn!("OBS isn't connected: couldn't switch {switch} back off");
    }
}

enum Ended {
    Unneeded,
    Lost { unsent: Option<Job> },
}

/// While connected: first what's over or waited for the connection, then
/// jobs as they come and effects as they end.
async fn session(
    obs: &mut Obs,
    jobs: &mut mpsc::Receiver<Job>,
    waiting: &mut Waiting<Effect>,
    timed: &mut Timed,
    retry: Option<Job>,
) -> Ended {
    // Effects that ran out while OBS was away.
    if end(obs, timed, Instant::now()).await.is_err() {
        return Ended::Lost { unsent: retry };
    }
    // Pop one job at a time: if OBS drops mid-backlog, remaining queued jobs
    // stay in `waiting` instead of being discarded.
    let mut retry = retry;
    while let Some(mut job) = retry.take().or_else(|| waiting.next_fresh(Instant::now())) {
        if apply(obs, timed, &mut job).await.is_err() {
            return Ended::Lost { unsent: Some(job) };
        }
    }
    loop {
        // (Copied out: the `select!` below mustn't borrow `timed`, which
        // the branches change.)
        let next_end = timed.next_end();
        let effect_over = async {
            match next_end {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            job = jobs.recv() => {
                let Some(mut job) = job else {
                    // Rules changed or the app quits: nothing stays on
                    // because of us. Far in the future: everything is over.
                    let all = Instant::now() + Duration::from_secs(365 * 24 * 3600);
                    if end(obs, timed, all).await.is_err() {
                        gave_up(timed);
                    }
                    let _ = obs.socket.close(None).await;
                    return Ended::Unneeded;
                };
                if apply(obs, timed, &mut job).await.is_err() {
                    return Ended::Lost { unsent: Some(job) };
                }
            }
            () = effect_over => {
                if end(obs, timed, Instant::now()).await.is_err() {
                    return Ended::Lost { unsent: None };
                }
            }
            // We asked for no events, so anything that arrives between
            // requests is only of interest if it ends the connection.
            frame = obs.socket.next() => match frame {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    return Ended::Lost { unsent: None };
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Carries out one job. `Err` only when the connection broke: an error OBS
/// answers with (a source that doesn't exist) is the streamer's to fix, so
/// it's reported (the rule's status shows it) and the job counts as done.
async fn apply(obs: &mut Obs, timed: &mut Timed, job: &mut Job) -> Result<(), Lost> {
    // What a refusal is about, in the streamer's own names.
    let about = match &job.action {
        Effect::Filter { source, filter, .. } => format!("filter \"{filter}\" on \"{source}\""),
        Effect::Show { scene, source, .. } => format!("\"{source}\" in scene \"{scene}\""),
        Effect::Restart { source } => format!("media source \"{source}\""),
    };
    let result = match &job.action {
        Effect::Filter {
            source,
            filter,
            duration,
        } => {
            let switch = Switch::Filter {
                source: source.clone(),
                filter: filter.clone(),
            };
            switch_on(obs, timed, switch, *duration).await
        }
        Effect::Show {
            scene,
            source,
            duration,
        } => {
            let switch = Switch::Item {
                scene: scene.clone(),
                source: source.clone(),
            };
            switch_on(obs, timed, switch, *duration).await
        }
        Effect::Restart { source } => obs.restart_media(source).await,
    };
    match result {
        Ok(()) => job.finish("OBS", Outcome::Sent),
        Err(ObsError::Refused(why)) => {
            job.finish("OBS", Outcome::Refused(format!("{about}: {why}")))
        }
        Err(ObsError::Lost(lost)) => return Err(lost),
    }
    Ok(())
}

async fn switch_on(
    obs: &mut Obs,
    timed: &mut Timed,
    switch: Switch,
    duration: Option<Duration>,
) -> Result<(), ObsError> {
    let now = Instant::now();
    let ours = timed.0.get(&switch).copied();
    // Only asked when it isn't on because of an earlier reaction: then
    // "was it on before?" was answered back then (no).
    let was_on = match ours {
        Some(_) => false,
        None => obs.is_on(&switch).await?,
    };
    // Switched on even if it's ours already: someone may have switched it
    // off by hand meanwhile.
    obs.set(&switch, true).await?;
    match (duration, ours) {
        // It was on before the reaction: it stays on afterwards too.
        _ if was_on => {}
        (Some(duration), Some(end)) => {
            timed.0.insert(switch, end.max(now + duration));
        }
        (Some(duration), None) => {
            timed.0.insert(switch, now + duration);
        }
        // On for good: no end any more.
        (None, _) => {
            timed.0.remove(&switch);
        }
    }
    Ok(())
}

/// Switches off what's over at `now`. Stops at a broken connection; what's
/// left is switched off after reconnecting.
async fn end(obs: &mut Obs, timed: &mut Timed, now: Instant) -> Result<(), Lost> {
    for switch in timed.due(now) {
        match obs.set(&switch, false).await {
            Ok(()) => debug!("switched {switch} back off"),
            Err(ObsError::Refused(why)) => warn!("couldn't switch {switch} back off: {why}"),
            Err(ObsError::Lost(lost)) => return Err(lost),
        }
        timed.0.remove(&switch);
    }
    Ok(())
}

// ---- the protocol ----

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// An identified connection to obs-websocket.
struct Obs {
    socket: Socket,
    /// Each request's id, to match its answer.
    next_id: u64,
}

/// The connection broke (closed, an error, no answer).
#[derive(Debug)]
struct Lost {
    reason: String,
    /// The close code, if OBS closed it with one.
    code: Option<u16>,
}

impl Lost {
    fn new(reason: impl fmt::Display) -> Self {
        Self {
            reason: reason.to_string(),
            code: None,
        }
    }
}

#[derive(Debug)]
enum ObsError {
    Lost(Lost),
    /// OBS answered, with a failure: e.g. no source of that name.
    Refused(String),
}

impl From<Lost> for ObsError {
    fn from(lost: Lost) -> Self {
        ObsError::Lost(lost)
    }
}

#[derive(Debug)]
enum ConnectError {
    Unreachable(String),
    /// OBS wants a password, and none is stored.
    NoPassword,
    WrongPassword,
    /// Something answered, but not like obs-websocket 5.
    NotObs(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Unreachable(e) => write!(f, "{e}"),
            ConnectError::NoPassword => {
                write!(f, "OBS asks for a password, and none is stored")
            }
            ConnectError::WrongPassword => write!(f, "OBS refused the stored password"),
            ConnectError::NotObs(e) => write!(f, "not an OBS WebSocket server ({e})"),
        }
    }
}

impl From<Lost> for ConnectError {
    fn from(lost: Lost) -> Self {
        match lost.code {
            Some(AUTHENTICATION_FAILED) => ConnectError::WrongPassword,
            _ => ConnectError::Unreachable(lost.reason),
        }
    }
}

impl Obs {
    async fn connect(
        url: &str,
        secrets: Option<&SecretStore>,
        password: &mut Option<String>,
    ) -> Result<Obs, ConnectError> {
        let (mut socket, _response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| ConnectError::Unreachable(e.to_string()))?;
        // Hello: says whether a password is needed, and if so, what to do
        // with it (`authentication`).
        let hello = next_message(&mut socket).await?;
        if hello["op"] != 0 {
            return Err(ConnectError::NotObs("no Hello".into()));
        }
        let mut identify = json!({ "rpcVersion": RPC_VERSION, "eventSubscriptions": 0 });
        if let Some(login) = hello["d"]["authentication"].as_object() {
            let text = |key: &str| login.get(key).and_then(Value::as_str).unwrap_or_default();
            if password.is_none() {
                *password = load_password(secrets).await;
            }
            let Some(password) = password.as_deref() else {
                return Err(ConnectError::NoPassword);
            };
            identify["authentication"] =
                authentication(password, text("salt"), text("challenge")).into();
        }
        let message = json!({ "op": 1, "d": identify }).to_string();
        socket
            .send(Message::text(message))
            .await
            .map_err(|e| ConnectError::Unreachable(e.to_string()))?;
        // Identified, or closed with "authentication failed".
        let identified = next_message(&mut socket).await?;
        if identified["op"] != 2 {
            return Err(ConnectError::NotObs("not identified".into()));
        }
        Ok(Obs { socket, next_id: 1 })
    }

    /// Sends a request and waits for its answer's data.
    async fn request(&mut self, kind: &str, data: Value) -> Result<Value, ObsError> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let request = json!({
            "op": 6,
            "d": { "requestType": kind, "requestId": id, "requestData": data },
        });
        self.socket
            .send(Message::text(request.to_string()))
            .await
            .map_err(Lost::new)?;
        let wait = async {
            loop {
                let mut message = next_message(&mut self.socket).await?;
                if message["op"] == 7 && message["d"]["requestId"] == id.as_str() {
                    return Ok::<_, Lost>(message["d"].take());
                }
            }
        };
        let mut answer = tokio::time::timeout(REQUEST_TIMEOUT, wait)
            .await
            .map_err(|_| Lost::new("OBS didn't answer"))??;
        let status = &answer["requestStatus"];
        if status["result"] == true {
            Ok(answer["responseData"].take())
        } else {
            let comment = status["comment"].as_str().unwrap_or("no reason given");
            debug!("OBS refused {kind}: {comment}");
            Err(ObsError::Refused(comment.to_string()))
        }
    }

    async fn is_on(&mut self, switch: &Switch) -> Result<bool, ObsError> {
        let answer = match switch {
            Switch::Filter { source, filter } => {
                let data = json!({ "sourceName": source, "filterName": filter });
                let answer = self.request("GetSourceFilter", data).await?;
                answer["filterEnabled"].as_bool()
            }
            Switch::Item { scene, source } => {
                let id = self.item_id(scene, source).await?;
                let data = json!({ "sceneName": scene, "sceneItemId": id });
                let answer = self.request("GetSceneItemEnabled", data).await?;
                answer["sceneItemEnabled"].as_bool()
            }
        };
        answer.ok_or_else(|| ObsError::Refused("OBS sent no state".into()))
    }

    async fn set(&mut self, switch: &Switch, on: bool) -> Result<(), ObsError> {
        match switch {
            Switch::Filter { source, filter } => {
                let data =
                    json!({ "sourceName": source, "filterName": filter, "filterEnabled": on });
                self.request("SetSourceFilterEnabled", data).await?;
            }
            Switch::Item { scene, source } => {
                // Looked up each time: ids change when OBS restarts, and
                // the lookup is cheap.
                let id = self.item_id(scene, source).await?;
                let data = json!({ "sceneName": scene, "sceneItemId": id, "sceneItemEnabled": on });
                self.request("SetSceneItemEnabled", data).await?;
            }
        }
        Ok(())
    }

    /// A source's id within a scene (OBS switches scene items by id).
    async fn item_id(&mut self, scene: &str, source: &str) -> Result<u64, ObsError> {
        let data = json!({ "sceneName": scene, "sourceName": source });
        let answer = self.request("GetSceneItemId", data).await?;
        answer["sceneItemId"]
            .as_u64()
            .ok_or_else(|| ObsError::Refused("OBS sent no id".into()))
    }

    async fn restart_media(&mut self, source: &str) -> Result<(), ObsError> {
        let data = json!({
            "inputName": source,
            "mediaAction": "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_RESTART",
        });
        self.request("TriggerMediaInputAction", data).await?;
        Ok(())
    }
}

/// The next JSON message. Pings are answered by tungstenite; anything that
/// isn't JSON text is skipped.
async fn next_message(socket: &mut Socket) -> Result<Value, Lost> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                Ok(value) => return Ok(value),
                Err(e) => debug!("skipped a message from OBS that isn't JSON: {e}"),
            },
            Some(Ok(Message::Close(frame))) => {
                return Err(match frame {
                    Some(frame) => Lost {
                        reason: format!(
                            "closed by OBS ({}: {})",
                            u16::from(frame.code),
                            frame.reason
                        ),
                        code: Some(frame.code.into()),
                    },
                    None => Lost::new("closed by OBS"),
                });
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(Lost::new(e)),
            None => return Err(Lost::new("connection closed")),
        }
    }
}

/// The login answer obs-websocket expects:
/// `base64(sha256(base64(sha256(password + salt)) + challenge))`.
fn authentication(password: &str, salt: &str, challenge: &str) -> String {
    let secret = BASE64.encode(Sha256::digest(format!("{password}{salt}")));
    BASE64.encode(Sha256::digest(format!("{secret}{challenge}")))
}

/// The stored password; `None` if there's none or it can't be read (said in
/// the log: the connection error only says none is stored).
async fn load_password(secrets: Option<&SecretStore>) -> Option<String> {
    match secrets?.load(OBS_PASSWORD).await {
        Ok(password) => password.filter(|p| !p.is_empty()),
        Err(e) => {
            warn!("couldn't read the OBS password: {e:#}");
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    #[test]
    fn authentication_matches_obs_websockets_algorithm() {
        // Computed independently (Python's hashlib and base64), from the
        // algorithm in obs-websocket's protocol docs.
        let auth = authentication(
            "supersecretpassword",
            "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=",
            "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=",
        );
        assert_eq!(auth, "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4=");
    }

    /// What the stand-in OBS has: filters and scene items with their
    /// on/off state.
    #[derive(Default)]
    pub(crate) struct Scene {
        filters: HashMap<(String, String), bool>,
        /// (scene, source) → (id, visible)
        items: HashMap<(String, String), (u64, bool)>,
    }

    impl Scene {
        pub(crate) fn filter(mut self, source: &str, filter: &str, on: bool) -> Self {
            self.filters.insert((source.into(), filter.into()), on);
            self
        }

        pub(crate) fn item(mut self, scene: &str, source: &str, on: bool) -> Self {
            let id = self.items.len() as u64 + 10;
            self.items.insert((scene.into(), source.into()), (id, on));
            self
        }
    }

    /// A stand-in obs-websocket. Every change it makes (and every media
    /// restart) is reported on the returned channel, e.g.
    /// `"filter Mic/Chipmunk on"`.
    pub(crate) struct FakeObs {
        pub(crate) changes: mpsc::UnboundedReceiver<String>,
        /// Closes every open connection (OBS restarting).
        hang_up: Arc<Notify>,
        /// How many connections it accepted.
        connections: Arc<AtomicUsize>,
    }

    const SALT: &str = "c2FsdA==";
    const CHALLENGE: &str = "Y2hhbGxlbmdl";

    pub(crate) fn fake_obs(listener: TcpListener, password: Option<&str>, scene: Scene) -> FakeObs {
        let (tx, changes) = mpsc::unbounded_channel();
        let scene = Arc::new(Mutex::new(scene));
        let hang_up = Arc::new(Notify::new());
        let password = password.map(str::to_string);
        let notify = hang_up.clone();
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let (tx, scene, password) = (tx.clone(), scene.clone(), password.clone());
                let hang_up = notify.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let mut hello = json!({ "obsWebSocketVersion": "5.5.0", "rpcVersion": 1 });
                    if password.is_some() {
                        hello["authentication"] = json!({ "challenge": CHALLENGE, "salt": SALT });
                    }
                    let send = |v: Value| Message::text(v.to_string());
                    ws.send(send(json!({ "op": 0, "d": hello }))).await.unwrap();
                    let identify = read(&mut ws).await.unwrap();
                    assert_eq!(identify["op"], 1);
                    assert_eq!(identify["d"]["eventSubscriptions"], 0, "no events wanted");
                    if let Some(password) = &password {
                        let expected = authentication(password, SALT, CHALLENGE);
                        if identify["d"]["authentication"] != expected.as_str() {
                            let close = CloseFrame {
                                code: CloseCode::from(AUTHENTICATION_FAILED),
                                reason: "Authentication failed.".into(),
                            };
                            let _ = ws.close(Some(close)).await;
                            return;
                        }
                    } else {
                        assert!(identify["d"].get("authentication").is_none());
                    }
                    let identified = json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } });
                    ws.send(send(identified)).await.unwrap();
                    loop {
                        let request = tokio::select! {
                            request = read(&mut ws) => match request {
                                Some(request) => request,
                                None => return,
                            },
                            () = hang_up.notified() => return,
                        };
                        let d = &request["d"];
                        let answer = answer(
                            &scene,
                            &tx,
                            d["requestType"].as_str().unwrap(),
                            &d["requestData"],
                        );
                        let (ok, data, comment) = match answer {
                            Ok(data) => (true, data, Value::Null),
                            Err(comment) => (false, Value::Null, comment.into()),
                        };
                        let response = json!({ "op": 7, "d": {
                            "requestType": d["requestType"],
                            "requestId": d["requestId"],
                            "requestStatus": { "result": ok, "code": if ok { 100 } else { 600 }, "comment": comment },
                            "responseData": data,
                        }});
                        if ws.send(send(response)).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        FakeObs {
            changes,
            hang_up,
            connections,
        }
    }

    type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    async fn read(ws: &mut Ws) -> Option<Value> {
        loop {
            match ws.next().await? {
                Ok(Message::Text(text)) => return serde_json::from_str(&text).ok(),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    fn answer(
        scene: &Mutex<Scene>,
        changes: &mpsc::UnboundedSender<String>,
        kind: &str,
        data: &Value,
    ) -> Result<Value, &'static str> {
        let mut scene = scene.lock().unwrap();
        let text = |key: &str| data[key].as_str().unwrap_or_default().to_string();
        let on_off = |on: bool| if on { "on" } else { "off" };
        match kind {
            "GetSourceFilter" | "SetSourceFilterEnabled" => {
                let key = (text("sourceName"), text("filterName"));
                let state = scene.filters.get_mut(&key).ok_or("No filter was found")?;
                if let Some(on) = data["filterEnabled"].as_bool() {
                    *state = on;
                    let _ = changes.send(format!("filter {}/{} {}", key.0, key.1, on_off(on)));
                }
                Ok(json!({ "filterEnabled": *state }))
            }
            "GetSceneItemId" => {
                let key = (text("sceneName"), text("sourceName"));
                let (id, _) = scene.items.get(&key).ok_or("No scene item was found")?;
                Ok(json!({ "sceneItemId": id }))
            }
            "GetSceneItemEnabled" | "SetSceneItemEnabled" => {
                let id = data["sceneItemId"].as_u64();
                let (key, (_, state)) = scene
                    .items
                    .iter_mut()
                    .find(|(_, (item, _))| Some(*item) == id)
                    .ok_or("No scene item was found")?;
                if let Some(on) = data["sceneItemEnabled"].as_bool() {
                    *state = on;
                    let _ = changes.send(format!("item {}/{} {}", key.0, key.1, on_off(on)));
                }
                Ok(json!({ "sceneItemEnabled": *state }))
            }
            "TriggerMediaInputAction" => {
                assert_eq!(
                    data["mediaAction"],
                    "OBS_WEBSOCKET_MEDIA_INPUT_ACTION_RESTART"
                );
                let _ = changes.send(format!("media {} restart", text("inputName")));
                Ok(Value::Null)
            }
            other => panic!("unexpected request {other}"),
        }
    }

    // ---- the output against the stand-in ----

    struct Output {
        jobs: Option<mpsc::Sender<Job>>,
        state: watch::Receiver<OutputState>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Output {
        async fn send(&self, effect: Effect) {
            let job = Job::new("test", effect, Instant::now());
            self.jobs.as_ref().unwrap().send(job).await.unwrap();
        }

        async fn connected(&mut self) {
            until(&mut self.state, |s| *s == OutputState::Connected).await;
        }
    }

    fn start(url: &str, secrets: Option<SecretStore>) -> Output {
        let (jobs, rx) = mpsc::channel(8);
        let (state_tx, state) = watch::channel(OutputState::Connecting);
        let task = tokio::spawn(run(
            url.into(),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            secrets,
            rx,
            state_tx,
            Arc::default(),
        ));
        Output {
            jobs: Some(jobs),
            state,
            task,
        }
    }

    async fn until(state: &mut watch::Receiver<OutputState>, f: impl Fn(&OutputState) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), state.wait_for(f))
            .await
            .expect("state not reached")
            .unwrap();
    }

    async fn next(obs: &mut FakeObs) -> String {
        tokio::time::timeout(Duration::from_secs(10), obs.changes.recv())
            .await
            .expect("no change in OBS")
            .unwrap()
    }

    /// Nothing changes in OBS for `ms` milliseconds.
    async fn quiet(obs: &mut FakeObs, ms: u64) {
        let wait = tokio::time::timeout(Duration::from_millis(ms), obs.changes.recv()).await;
        assert!(wait.is_err(), "unexpected change: {wait:?}");
    }

    pub(crate) async fn listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    fn chipmunk(ms: Option<u64>) -> Effect {
        Effect::Filter {
            source: "Mic".into(),
            filter: "Chipmunk".into(),
            duration: ms.map(Duration::from_millis),
        }
    }

    fn mic() -> Scene {
        Scene::default().filter("Mic", "Chipmunk", false)
    }

    fn store_with(name: &str, password: Option<&str>) -> SecretStore {
        let dir = std::env::temp_dir().join(format!(
            "chat-reactions-obs-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SecretStore::Dir(dir.clone());
        if let Some(password) = password {
            std::fs::create_dir_all(&dir).unwrap();
            // Same file name the store would use.
            std::fs::write(dir.join(format!("{OBS_PASSWORD}.token")), password).unwrap();
        }
        store
    }

    #[tokio::test]
    async fn logs_in_with_the_stored_password() {
        let (listener, url) = listener().await;
        let _obs = fake_obs(listener, Some("hunter2"), mic());
        let mut out = start(&url, Some(store_with("right", Some("hunter2"))));
        out.connected().await;
    }

    #[tokio::test]
    async fn a_wrong_or_missing_password_says_so() {
        let (listener, url) = listener().await;
        let _obs = fake_obs(listener, Some("hunter2"), mic());
        let mut wrong = start(&url, Some(store_with("wrong", Some("letmein"))));
        let refused = |s: &OutputState| matches!(s, OutputState::Retrying { error } if error.contains("refused the stored password"));
        until(&mut wrong.state, refused).await;

        let mut none = start(&url, Some(store_with("none", None)));
        let missing = |s: &OutputState| matches!(s, OutputState::Retrying { error } if error.contains("none is stored"));
        until(&mut none.state, missing).await;
        let mut no_store = start(&url, None);
        until(&mut no_store.state, missing).await;
    }

    #[tokio::test]
    async fn a_timed_filter_goes_back_off() {
        let (listener, url) = listener().await;
        let mut obs = fake_obs(listener, None, mic());
        let mut out = start(&url, None);
        out.connected().await;
        let fired = Instant::now();
        out.send(chipmunk(Some(300))).await;
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk on");
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk off");
        assert!(fired.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn firing_again_makes_it_last_longer_instead_of_stacking() {
        let (listener, url) = listener().await;
        let mut obs = fake_obs(listener, None, mic());
        let mut out = start(&url, None);
        out.connected().await;
        let fired = Instant::now();
        out.send(chipmunk(Some(400))).await;
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk on");
        tokio::time::sleep(Duration::from_millis(200)).await;
        out.send(chipmunk(Some(400))).await;
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk on", "on again");
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk off");
        assert!(
            fired.elapsed() >= Duration::from_millis(600),
            "the second one's time: {:?}",
            fired.elapsed()
        );
        quiet(&mut obs, 300).await;
    }

    #[tokio::test]
    async fn what_was_on_before_stays_on() {
        let (listener, url) = listener().await;
        let already_on = Scene::default().filter("Mic", "Chipmunk", true);
        let mut obs = fake_obs(listener, None, already_on);
        let mut out = start(&url, None);
        out.connected().await;
        out.send(chipmunk(Some(100))).await;
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk on");
        quiet(&mut obs, 400).await;
    }

    #[tokio::test]
    async fn shows_a_source_for_a_while_and_plays_media() {
        let (listener, url) = listener().await;
        let scene = Scene::default()
            .item("Main", "Webcam", true)
            .item("Main", "Confetti", false);
        let mut obs = fake_obs(listener, None, scene);
        let mut out = start(&url, None);
        out.connected().await;
        out.send(Effect::Show {
            scene: "Main".into(),
            source: "Confetti".into(),
            duration: Some(Duration::from_millis(200)),
        })
        .await;
        assert_eq!(next(&mut obs).await, "item Main/Confetti on");
        assert_eq!(next(&mut obs).await, "item Main/Confetti off");

        out.send(Effect::Restart {
            source: "Airhorn".into(),
        })
        .await;
        assert_eq!(next(&mut obs).await, "media Airhorn restart");
    }

    #[tokio::test]
    async fn wrong_names_are_logged_and_the_next_reaction_still_works() {
        let (listener, url) = listener().await;
        let mut obs = fake_obs(listener, None, mic());
        let mut out = start(&url, None);
        out.connected().await;
        let typo = Effect::Filter {
            source: "Mic".into(),
            filter: "Typo".into(),
            duration: None,
        };
        let mut job = Job::new("typo", typo, Instant::now());
        let (reply, answer) = tokio::sync::oneshot::channel();
        job.report.reply = Some(reply);
        out.jobs.as_ref().unwrap().send(job).await.unwrap();
        // OBS's own words, for the rule's status in the app.
        let Ok(Outcome::Refused(why)) = answer.await else {
            panic!("a wrong filter name isn't reported as refused");
        };
        assert!(why.contains("Typo"), "{why}");

        out.send(chipmunk(None)).await;
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk on");
        assert_eq!(*out.state.borrow(), OutputState::Connected);
    }

    #[tokio::test]
    async fn stopping_switches_back_what_is_still_on() {
        let (listener, url) = listener().await;
        let mut obs = fake_obs(listener, None, mic());
        let mut out = start(&url, None);
        out.connected().await;
        out.send(chipmunk(Some(60_000))).await;
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk on");
        out.jobs = None; // the reactor lets go: rules changed, or quitting
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk off");
        tokio::time::timeout(Duration::from_secs(5), out.task)
            .await
            .expect("the task ended")
            .unwrap();
    }

    #[tokio::test]
    async fn an_effect_survives_obs_reconnecting() {
        let (listener, url) = listener().await;
        let mut obs = fake_obs(listener, None, mic());
        let mut out = start(&url, None);
        out.connected().await;
        out.send(chipmunk(Some(500))).await;
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk on");
        obs.hang_up.notify_waiters();
        // Switched off on the new connection, at the effect's end.
        assert_eq!(next(&mut obs).await, "filter Mic/Chipmunk off");
        assert_eq!(obs.connections.load(Ordering::SeqCst), 2, "reconnected");
    }
}
