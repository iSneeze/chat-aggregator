//! Ties the pieces together and owns their lifetimes:
//!
//! ```text
//!                     ┌──────── actor (control plane) ────────┐
//!  EngineHandle ─────→│ commands: add/remove/start/stop/update │───→ watch<Status> ──→ UI
//!                     │ restarts failed sources with backoff   │
//!                     └───────────────┬────────────────────────┘
//!                                     │ starts / stops
//!  source ─→ forwarder (counts) ─→ Hub (broadcast + history) ─→ server (overlay SSE, JSON API)
//! ```
//!
//! The data plane (chat flowing into the hub) and the control plane (what
//! runs, and how it's doing) are separate: chat never passes through the
//! actor, so a busy chat can't slow down commands, and vice versa.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use chat_core::Hub;
use chat_server::{ApiOrigins, Avatars, Connections, ServerState};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

mod actor;
mod config;
mod config_file;
mod factory;
mod settings;
mod status;
mod youtube;

pub use chat_server::{ANSWER_TIMEOUT, Builtin, Newest, Stagger, ThemeSource};
pub use config::{SetupError, SourceConfig, YouTubeSettings};
pub use config_file::{AppSettings, Appearance, ConfigFile, ServerSettings, Settings, SourceEntry};
pub use status::{Health, SourceId, SourceState, SourceStatus, Status};
pub use youtube::{YouTubeAccount, YouTubeLogin};
// For checking a custom emoji export before using it (the app's import).
pub use chat_youtube::EmojiMap;

pub use actor::ManualInput;
use actor::{Actor, Command, Reply};
use factory::{Production, SourceFactory};
use settings::Controls;

pub const DEFAULT_PORT: u16 = 7878;
pub const DEFAULT_HISTORY: usize = 20;

#[derive(Default)]
pub struct EngineConfig {
    /// The settings to start with, usually from `config.toml`
    /// (`ConfigFile::into_engine_config`). The engine owns them from then
    /// on: see `EngineHandle::edit_settings`.
    pub settings: Settings,
    /// Added in this order: started right away, or listed as switched off
    /// (`enabled = false`).
    pub sources: Vec<SourceEntry>,
    /// The folder of named themes (`themes/` next to `config.toml`): where
    /// the settings' theme name and overlay URLs' `?theme=<name>` are looked
    /// up. `None`: only built-in themes in overlay URLs.
    pub themes_dir: Option<PathBuf>,
    /// Whether other sites wait for the streamer's answer (the app shows
    /// the question, see `Status::api_requests`) or are refused right away
    /// (headless mode: nobody to ask).
    pub ask_api_origins: bool,
    /// Where the engine saves the settings and sources after each change
    /// (`config.toml`). `None`: never written (headless mode, which leaves
    /// your file and its comments alone, and tests).
    pub save_to: Option<PathBuf>,
}

/// Controls a running engine. Cheap to clone: the UI, headless mode and
/// tests each hold one. Most methods send a message to the actor and wait
/// for its answer.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<Status>,
    settings: watch::Receiver<Settings>,
    /// Shared with the actor; used here only to reload the overlays, which
    /// changes nothing the actor owns.
    controls: Arc<Controls>,
    /// The engine's tokio runtime, for work that needs one (the YouTube
    /// login's local server, the keyring): callers may be on any executor,
    /// like the app's GPUI tasks.
    runtime: tokio::runtime::Handle,
}

impl EngineHandle {
    /// Adds a source and starts it.
    pub async fn add_source(&self, config: SourceConfig) -> anyhow::Result<SourceId> {
        self.call(|reply| Command::Add(config, true, reply)).await
    }

    /// Adds a source switched off, e.g. one the user turned off last time:
    /// it's listed as stopped and doesn't connect until started.
    pub async fn add_stopped_source(&self, config: SourceConfig) -> anyhow::Result<SourceId> {
        self.call(|reply| Command::Add(config, false, reply)).await
    }

    /// Adds a source whose messages come from the returned `ManualInput`
    /// (the app's test window), and starts it. It ends when the input is
    /// dropped; remove it then.
    pub async fn add_manual_source(&self) -> anyhow::Result<(SourceId, ManualInput)> {
        self.call(Command::AddManual).await
    }

    /// Stops and forgets a source.
    pub async fn remove_source(&self, id: SourceId) -> anyhow::Result<()> {
        self.call(|reply| Command::Remove(id, reply)).await
    }

    /// Starts a stopped (or failed, or finished) source.
    pub async fn start_source(&self, id: SourceId) -> anyhow::Result<()> {
        self.call(|reply| Command::Start(id, reply)).await
    }

    /// Stops a source; it stays in the list as "stopped".
    pub async fn stop_source(&self, id: SourceId) -> anyhow::Result<()> {
        self.call(|reply| Command::Stop(id, reply)).await
    }

    /// Changes a source's settings and restarts it (unless it's stopped).
    /// The test window's source has none: that's an error.
    pub async fn update_source(&self, id: SourceId, config: SourceConfig) -> anyhow::Result<()> {
        self.call(|reply| Command::Update(id, config, reply)).await
    }

    /// The current settings. A `watch` receiver always holds the latest
    /// value; `changed().await` waits for the next change.
    pub fn settings(&self) -> watch::Receiver<Settings> {
        self.settings.clone()
    }

    /// Changes settings: `change` edits the current ones, and the engine
    /// applies whatever changed (overlays reload for a new theme or chat
    /// direction, YouTube sources restart for new YouTube settings, …) and
    /// saves them. When the returned future finishes, `settings()` has the
    /// result.
    ///
    /// A closure rather than the new settings: it runs on the newest
    /// settings inside the engine, so a window can't undo another one's
    /// change by sending back an older copy. Change only what you mean to.
    ///
    /// Not an `async fn`: the change is sent right away, when this is
    /// called, not when the future is first awaited. So a change made just
    /// before the app quits (the settings window's last edit) still reaches
    /// the engine, which handles it before shutting down. (`use<C>`: the
    /// future doesn't borrow `self`, so it can outlive this handle.)
    pub fn edit_settings<C>(&self, change: C) -> impl Future<Output = anyhow::Result<()>> + use<C>
    where
        C: FnOnce(&mut Settings) + Send + 'static,
    {
        self.call(|reply| Command::EditSettings(Box::new(change), reply))
    }

    /// Makes connected overlays reload, e.g. after editing the theme files.
    pub fn reload_overlays(&self) {
        self.controls.reload_overlays();
    }

    /// The streamer's answer to a site waiting in `Status::api_requests`.
    /// "Allow" lets it in now and adds it to the settings' allowed sites;
    /// "deny" refuses it until the engine restarts.
    pub async fn answer_api_request(&self, origin: &str, allow: bool) -> anyhow::Result<()> {
        let origin = origin.to_string();
        self.call(|reply| Command::AnswerApiRequest {
            origin,
            allow,
            reply,
        })
        .await
    }

    /// The live status. A `watch` receiver always holds the latest value;
    /// `changed().await` waits for the next update.
    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// Sends a command with a fresh reply channel, and returns a future for
    /// the answer. `make` builds the command around the reply sender. The
    /// command goes out right away, not when the future is first awaited
    /// (see `edit_settings`), unless 32 commands are already waiting: then
    /// it's sent once the future gets its turn.
    fn call<T, M>(&self, make: M) -> impl Future<Output = anyhow::Result<T>> + use<T, M>
    where
        M: FnOnce(Reply<T>) -> Command,
    {
        let shut_down = || anyhow!("the engine has shut down");
        let (reply, answer) = oneshot::channel();
        let later = match self.commands.try_send(make(reply)) {
            Ok(()) => Ok(None),
            Err(mpsc::error::TrySendError::Full(command)) => {
                Ok(Some((self.commands.clone(), command)))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(shut_down()),
        };
        async move {
            if let Some((commands, command)) = later? {
                commands.send(command).await.map_err(|_| shut_down())?;
            }
            answer.await.map_err(|_| shut_down())?
        }
    }
}

pub struct Engine {
    handle: EngineHandle,
    hub: Arc<Hub>,
    addr: SocketAddr,
    shutdown: CancellationToken,
    // A JoinSet aborts all its tasks when dropped, so even an Engine that is
    // dropped without `shutdown()` doesn't leave tasks running.
    tasks: JoinSet<()>,
}

impl Engine {
    pub async fn start(config: EngineConfig) -> anyhow::Result<Self> {
        let factory = Production::new(config.settings.youtube.clone());
        Self::start_with(config, factory).await
    }

    /// `start` with any source factory; tests use scripted sources.
    async fn start_with<F: SourceFactory>(
        config: EngineConfig,
        factory: F,
    ) -> anyhow::Result<Self> {
        let server = &config.settings.server;
        // Localhost only: nothing else on the network can reach it. Bound
        // first: if the port is taken, fail before anything else runs.
        let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port));
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("can't listen on {bind}"))?;
        let addr = listener.local_addr()?;

        let hub = Arc::new(Hub::new(server.history));
        let shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();

        // The server reads these; the actor changes them (`Controls`).
        let (theme_tx, theme_rx) =
            watch::channel(server.theme_source(config.themes_dir.as_deref()));
        let (stagger_tx, stagger_rx) = watch::channel(server.stagger());
        let (api_tx, api_rx) = watch::channel(server.api);
        let (newest_tx, newest_rx) = watch::channel(server.newest);
        let origins = Arc::new(ApiOrigins::new(
            server.api_origins.clone(),
            config.ask_api_origins,
        ));
        let connections = Arc::new(Connections::default());
        let state = ServerState {
            hub: hub.clone(),
            theme: theme_rx,
            shutdown: shutdown.clone(),
            stagger: stagger_rx,
            api: api_rx,
            newest: newest_rx,
            themes_dir: config.themes_dir.clone(),
            connections: connections.clone(),
            origins: origins.clone(),
            avatars: Arc::new(Avatars::new()),
        };
        let controls = Arc::new(Controls {
            hub: hub.clone(),
            theme: theme_tx,
            stagger: stagger_tx,
            api: api_tx,
            newest: newest_tx,
            origins,
            themes_dir: config.themes_dir,
        });

        let (status_tx, status_rx) = watch::channel(Status {
            overlay_url: format!("http://{addr}/"),
            overlays_connected: 0,
            api_clients: 0,
            api_requests: Vec::new(),
            save_error: None,
            sources: Vec::new(),
        });
        let (settings_tx, settings_rx) = watch::channel(config.settings);
        let (commands_tx, commands_rx) = mpsc::channel(32);
        let mut actor = Actor::new(
            factory,
            settings_tx,
            controls.clone(),
            status_tx,
            connections,
            config.save_to,
        );
        actor.add_from_config(config.sources);
        tasks.spawn(actor.run(commands_rx, shutdown.clone()));
        tasks.spawn(async move {
            if let Err(e) = chat_server::serve(listener, state).await {
                error!("overlay server failed: {e}");
            }
        });

        let handle = EngineHandle {
            commands: commands_tx,
            status: status_rx,
            settings: settings_rx,
            controls,
            // `start` runs on the engine's runtime: this is it.
            runtime: tokio::runtime::Handle::current(),
        };

        info!("overlay running at http://{addr}/");
        Ok(Self {
            handle,
            hub,
            addr,
            shutdown,
            tasks,
        })
    }

    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// The address actually bound (useful with port 0).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// For in-process consumers, e.g. a preview in the UI later.
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    /// Stops sources and the server, and waits until everything has ended.
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::{Activity, ChatEvent, ChatSource, Reporter};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::time::{Instant, timeout};

    const WAIT: Duration = Duration::from_secs(5);

    /// Default settings on a free port: the OS picks one, so tests can run
    /// in parallel.
    fn local() -> Settings {
        let mut settings = Settings::default();
        settings.server.port = 0;
        settings
    }

    // ---- with the real sources ----

    async fn start_demo() -> Engine {
        Engine::start(EngineConfig {
            sources: vec![SourceConfig::Demo.into()],
            settings: local(),
            ..EngineConfig::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn events_flow_from_source_to_hub() {
        let engine = start_demo().await;
        let (_, mut rx) = engine.hub().subscribe();
        // The demo sends its first message after 1.5 s.
        let event = timeout(WAIT, rx.recv()).await.unwrap().unwrap();
        assert!(matches!(event, ChatEvent::Message(_)));
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn overlay_is_served_and_shutdown_finishes_while_connected() {
        let engine = start_demo().await;
        let base = format!("http://{}", engine.addr());

        let page = reqwest::get(&base).await.unwrap().text().await.unwrap();
        assert!(page.contains("EventSource"));

        // An open overlay connection must not block shutdown.
        let _overlay = reqwest::get(format!("{base}/events")).await.unwrap();
        timeout(WAIT, engine.shutdown())
            .await
            .expect("shutdown hung");
    }

    #[tokio::test]
    async fn status_counts_connected_overlays() {
        let engine = start_demo().await;
        let _overlay = reqwest::get(format!("http://{}/events", engine.addr()))
            .await
            .unwrap();
        let status = wait_for(&engine.handle(), |s| s.overlays_connected == 1).await;
        assert_eq!(status.api_clients, 0);
        engine.shutdown().await;
    }

    /// A web site connecting to the API waits; the status shows it, and the
    /// answer lets its connection through.
    #[tokio::test]
    async fn sites_waiting_for_an_answer_show_in_the_status() {
        const GAME: &str = "https://game.example";
        let mut settings = local();
        settings.server.api = true;
        let engine = Engine::start(EngineConfig {
            settings,
            ask_api_origins: true,
            ..EngineConfig::default()
        })
        .await
        .unwrap();
        // The WebSocket opening request, as a page on that site sends it.
        let url = format!("http://{}/api/v1/ws", engine.addr());
        let connecting = tokio::spawn(async move {
            reqwest::Client::new()
                .get(url)
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .header("origin", GAME)
                .send()
                .await
                .unwrap()
                .status()
        });

        let handle = engine.handle();
        wait_for(&handle, |s| s.api_requests == [GAME]).await;
        handle.answer_api_request(GAME, true).await.unwrap();
        let status = timeout(WAIT, connecting).await.unwrap().unwrap();
        assert_eq!(status, reqwest::StatusCode::SWITCHING_PROTOCOLS);
        wait_for(&handle, |s| s.api_requests.is_empty()).await;
        // Allowed for good: it's in the settings now (once, however often
        // it's allowed).
        handle.answer_api_request(GAME, true).await.unwrap();
        assert_eq!(handle.settings().borrow().server.api_origins, [GAME]);
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn taken_port_is_an_error() {
        let engine = start_demo().await;
        let mut settings = Settings::default();
        settings.server.port = engine.addr().port();
        let second = Engine::start(EngineConfig {
            settings,
            ..EngineConfig::default()
        })
        .await;
        assert!(second.is_err());
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn youtube_without_settings_needs_attention() {
        let engine = Engine::start(EngineConfig {
            sources: vec![SourceConfig::YouTube { video_id: None }.into()],
            settings: local(),
            ..EngineConfig::default()
        })
        .await
        .unwrap();
        let status = wait_for(&engine.handle(), |s| {
            matches!(s.sources[0].state, SourceState::NeedsAttention { .. })
        })
        .await;
        assert_eq!(status.sources[0].health(), Health::Error);
        assert!(
            status.sources[0].summary().contains("client id"),
            "{}",
            status.sources[0].summary()
        );
        engine.shutdown().await;
    }

    // ---- with scripted sources ----

    /// Builds sources from a script in the Twitch channel name, e.g.
    /// `"fail-then-run:2"`, and counts builds per script.
    #[derive(Default)]
    struct Scripted {
        builds: Mutex<HashMap<String, usize>>,
    }

    enum Behaviour {
        /// Report `Receiving`, send this many messages, then run forever.
        Run(usize),
        /// Run this long, then fail.
        FailAfter(Duration),
        /// End normally at once.
        Finish,
        Panic,
    }

    impl ChatSource for Behaviour {
        async fn run(
            self,
            tx: tokio::sync::mpsc::Sender<ChatEvent>,
            activity: Reporter,
        ) -> anyhow::Result<()> {
            match self {
                Behaviour::Run(n) => {
                    activity.set(Activity::Receiving);
                    for msg in chat_core::demo::sample_messages(0).into_iter().take(n) {
                        tx.send(ChatEvent::Message(msg)).await?;
                    }
                    std::future::pending().await
                }
                Behaviour::FailAfter(after) => {
                    activity.set(Activity::Receiving);
                    tokio::time::sleep(after).await;
                    anyhow::bail!("connection lost")
                }
                Behaviour::Finish => Ok(()),
                Behaviour::Panic => panic!("oops"),
            }
        }
    }

    impl Scripted {
        fn builds(&self, script: &str) -> usize {
            self.builds
                .lock()
                .unwrap()
                .get(script)
                .copied()
                .unwrap_or(0)
        }
    }

    impl SourceFactory for Arc<Scripted> {
        type Source = Behaviour;

        async fn build(&self, config: &SourceConfig) -> anyhow::Result<Behaviour> {
            // YouTube configs stand for a source that just runs; counted
            // under "youtube".
            if let SourceConfig::YouTube { .. } = config {
                *self
                    .builds
                    .lock()
                    .unwrap()
                    .entry("youtube".into())
                    .or_default() += 1;
                return Ok(Behaviour::Run(0));
            }
            let SourceConfig::Twitch { channel: script } = config else {
                anyhow::bail!("scripted sources are Twitch or YouTube configs");
            };
            let build = {
                let mut builds = self.builds.lock().unwrap();
                let count = builds.entry(script.clone()).or_default();
                *count += 1;
                *count
            };
            let (name, arg) = script.split_once(':').unwrap_or((script, "0"));
            let arg: u64 = arg.parse().unwrap();
            match name {
                "fail-then-run" if build <= arg as usize => anyhow::bail!("not live yet"),
                "fail-then-run" | "emit" => Ok(Behaviour::Run(arg as usize)),
                "fail-after" => Ok(Behaviour::FailAfter(Duration::from_secs(arg))),
                "setup" => Err(SetupError("no channel set".into()).into()),
                "login" => Err(anyhow::Error::new(chat_youtube::oauth::LoginRequired::new(
                    "not logged in yet",
                ))
                .context("building the YouTube source")),
                "finish" => Ok(Behaviour::Finish),
                "panic" => Ok(Behaviour::Panic),
                _ => panic!("unknown script {script}"),
            }
        }
    }

    fn script(s: &str) -> SourceConfig {
        SourceConfig::Twitch { channel: s.into() }
    }

    async fn start_scripted(sources: &[&str]) -> (Engine, Arc<Scripted>) {
        let factory = Arc::new(Scripted::default());
        let engine = Engine::start_with(
            EngineConfig {
                sources: sources.iter().map(|s| script(s).into()).collect(),
                settings: local(),
                ..EngineConfig::default()
            },
            factory.clone(),
        )
        .await
        .unwrap();
        (engine, factory)
    }

    /// Waits until the status matches `condition`. With the paused clock,
    /// waiting for "the 3rd attempt" takes no real time.
    async fn wait_for(handle: &EngineHandle, condition: impl Fn(&Status) -> bool) -> Status {
        let mut status = handle.status();
        // Returning this directly only compiles since edition 2024: the
        // temporary borrow from `wait_for` is now dropped before `status`.
        status.wait_for(|s| condition(s)).await.unwrap().clone()
    }

    fn state(status: &Status) -> &SourceState {
        &status.sources[0].state
    }

    #[tokio::test(start_paused = true)]
    async fn failed_source_is_retried_with_growing_waits() {
        let started = Instant::now();
        let (engine, factory) = start_scripted(&["fail-then-run:2"]).await;
        let handle = engine.handle();

        // Build 1 fails → retry after 2 s → build 2 fails → retry after 4 s.
        let s = wait_for(&handle, |s| {
            matches!(state(s), SourceState::Retrying { attempt: 2, .. })
        })
        .await;
        assert_eq!(s.sources[0].health(), Health::Warning);
        assert!(s.sources[0].summary().contains("not live yet"));

        // Build 3 succeeds, 2 s + 4 s after the start.
        let s = wait_for(&handle, |s| {
            s.sources[0].activity == Some(Activity::Receiving)
        })
        .await;
        assert_eq!(*state(&s), SourceState::Running);
        assert_eq!(s.sources[0].health(), Health::Ok);
        assert_eq!(factory.builds("fail-then-run:2"), 3);
        assert_eq!(started.elapsed().as_secs(), 6);
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_start_over_after_a_stable_run() {
        let (engine, factory) = start_scripted(&["fail-after:120"]).await;
        let handle = engine.handle();
        // Ran 120 s (longer than a minute) before failing each time: always
        // the first attempt again, i.e. a 2 s wait, never more.
        wait_for(&handle, |_| factory.builds("fail-after:120") >= 3).await;
        let s = wait_for(&handle, |s| {
            matches!(state(s), SourceState::Retrying { .. })
        })
        .await;
        assert!(
            matches!(state(&s), SourceState::Retrying { attempt: 1, .. }),
            "{s:?}"
        );
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn needs_attention_is_not_retried() {
        let (engine, factory) = start_scripted(&["setup", "login"]).await;
        let handle = engine.handle();
        let s = wait_for(&handle, |s| {
            s.sources
                .iter()
                .all(|src| matches!(src.state, SourceState::NeedsAttention { .. }))
        })
        .await;
        assert_eq!(s.overall(), Health::Error);
        assert!(
            s.sources[1].summary().contains("login required"),
            "{}",
            s.sources[1].summary()
        );

        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(factory.builds("setup"), 1, "not rebuilt");
        assert_eq!(factory.builds("login"), 1, "not rebuilt");
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn normal_end_is_finished_not_retried() {
        let (engine, factory) = start_scripted(&["finish"]).await;
        let s = wait_for(&engine.handle(), |s| *state(s) == SourceState::Finished).await;
        assert_eq!(s.sources[0].health(), Health::Off);
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(factory.builds("finish"), 1);
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_source_is_retried() {
        let (engine, _) = start_scripted(&["panic"]).await;
        let s = wait_for(&engine.handle(), |s| {
            matches!(state(s), SourceState::Retrying { .. })
        })
        .await;
        assert!(
            s.sources[0].summary().contains("crashed"),
            "{}",
            s.sources[0].summary()
        );
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_cancels_a_pending_retry() {
        let (engine, factory) = start_scripted(&["fail-then-run:99"]).await;
        let handle = engine.handle();
        let s = wait_for(&handle, |s| {
            matches!(state(s), SourceState::Retrying { .. })
        })
        .await;
        handle.stop_source(s.sources[0].id).await.unwrap();

        tokio::time::sleep(Duration::from_secs(600)).await;
        let s = handle.status().borrow().clone();
        assert_eq!(*state(&s), SourceState::Stopped);
        assert_eq!(s.sources[0].health(), Health::Off);
        assert_eq!(
            factory.builds("fail-then-run:99"),
            1,
            "the retry timer was ignored"
        );

        handle.start_source(s.sources[0].id).await.unwrap();
        wait_for(&handle, |_| factory.builds("fail-then-run:99") == 2).await;
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_added_stopped_doesnt_run_until_started() {
        let (engine, factory) = start_scripted(&[]).await;
        let handle = engine.handle();
        let id = handle.add_stopped_source(script("emit:1")).await.unwrap();
        let s = wait_for(&handle, |s| s.source(id).is_some()).await;
        assert_eq!(s.source(id).unwrap().state, SourceState::Stopped);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(factory.builds("emit:1"), 0, "never connected");

        handle.start_source(id).await.unwrap();
        wait_for(&handle, |s| s.source(id).unwrap().messages == 1).await;
        engine.shutdown().await;
    }

    /// A change reaches the running parts, and `settings()` has it as soon
    /// as `edit_settings` returns.
    #[tokio::test]
    async fn settings_changes_reach_the_running_parts() {
        let (engine, _) = start_scripted(&[]).await;
        let handle = engine.handle();
        for i in 0..3 {
            let mut msg = chat_core::demo::sample_messages(0).remove(0);
            msg.id = format!("m{i}");
            engine.hub().publish(ChatEvent::Message(msg));
        }
        handle
            .edit_settings(|s| {
                s.server.history = 1;
                s.server.newest = Newest::Top;
                s.server.theme = Some("minimal".into());
            })
            .await
            .unwrap();
        assert_eq!(handle.settings().borrow().server.history, 1);
        let (history, _) = engine.hub().subscribe();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "m2", "the newest one stays");

        // The direction lands in the overlay page, the theme in its CSS.
        let get = |path: &str| {
            let url = format!("http://{}{path}", engine.addr());
            async move { reqwest::get(url).await.unwrap().text().await.unwrap() }
        };
        assert!(get("/").await.contains(r#"data-newest="top""#));
        assert_eq!(get("/theme/overlay.css").await, Builtin::Minimal.css());
        engine.shutdown().await;
    }

    /// Two windows changing different settings at the same moment: both
    /// changes stick, since each runs on the settings the other one left.
    #[tokio::test(start_paused = true)]
    async fn changes_from_different_places_both_stick() {
        let (engine, _) = start_scripted(&[]).await;
        let (a, b) = (engine.handle(), engine.handle());
        let (first, second) = tokio::join!(
            a.edit_settings(|s| s.server.history = 5),
            b.edit_settings(|s| s.server.newest = Newest::Top),
        );
        first.unwrap();
        second.unwrap();
        let settings = a.settings().borrow().clone();
        assert_eq!(settings.server.history, 5);
        assert_eq!(settings.server.newest, Newest::Top);
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn messages_are_counted() {
        let (engine, _) = start_scripted(&["emit:3"]).await;
        let s = wait_for(&engine.handle(), |s| s.sources[0].messages == 3).await;
        assert!(s.sources[0].last_message.is_some());
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn update_restarts_with_the_new_config() {
        let (engine, factory) = start_scripted(&["setup"]).await;
        let handle = engine.handle();
        let s = wait_for(&handle, |s| {
            matches!(state(s), SourceState::NeedsAttention { .. })
        })
        .await;
        let id = s.sources[0].id;

        // Fixing the settings brings it back, without waiting for a retry.
        handle.update_source(id, script("emit:1")).await.unwrap();
        let s = wait_for(&handle, |s| s.sources[0].messages == 1).await;
        assert_eq!(s.sources[0].config, Some(script("emit:1")));
        assert_eq!(factory.builds("emit:1"), 1);
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn new_youtube_settings_or_login_restart_running_youtube_sources() {
        let factory = Arc::new(Scripted::default());
        let engine = Engine::start_with(
            EngineConfig {
                sources: vec![
                    SourceConfig::YouTube { video_id: None }.into(),
                    SourceConfig::YouTube {
                        video_id: Some("off".into()),
                    }
                    .into(),
                    script("emit:0").into(),
                ],
                settings: local(),
                ..EngineConfig::default()
            },
            factory.clone(),
        )
        .await
        .unwrap();
        let handle = engine.handle();
        let s = wait_for(&handle, |_| factory.builds("youtube") == 2).await;
        handle.stop_source(s.sources[1].id).await.unwrap();

        handle
            .edit_settings(|s| s.youtube.api_key = Some("key".into()))
            .await
            .unwrap();
        wait_for(&handle, |_| factory.builds("youtube") == 3).await;
        // Settings that don't concern YouTube leave it alone.
        handle
            .edit_settings(|s| s.server.history = 5)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            factory.builds("youtube"),
            3,
            "the stopped one stays stopped, the others restart once"
        );
        assert_eq!(factory.builds("emit:0"), 1, "other sources aren't touched");

        // A login made (or removed) restarts them too.
        handle.youtube_login_changed().await.unwrap();
        wait_for(&handle, |_| factory.builds("youtube") == 4).await;
        engine.shutdown().await;
    }

    /// Importing a new emoji export over the old file leaves the setting as
    /// it was, but the sources must load the new emoji: they restart anyway
    /// (once per import).
    #[tokio::test(start_paused = true)]
    async fn a_reimported_emoji_file_restarts_youtube_sources() {
        let factory = Arc::new(Scripted::default());
        let engine = Engine::start_with(
            EngineConfig {
                sources: vec![SourceConfig::YouTube { video_id: None }.into()],
                settings: local(),
                ..EngineConfig::default()
            },
            factory.clone(),
        )
        .await
        .unwrap();
        let handle = engine.handle();
        wait_for(&handle, |_| factory.builds("youtube") == 1).await;

        let file = PathBuf::from("youtube-emojis.json");
        handle.use_youtube_emojis(file.clone()).await.unwrap();
        wait_for(&handle, |_| factory.builds("youtube") == 2).await;
        assert_eq!(
            handle.settings().borrow().youtube.emojis,
            Some(file.clone())
        );

        // The same file again, now with a new export in it.
        handle.use_youtube_emojis(file).await.unwrap();
        wait_for(&handle, |_| factory.builds("youtube") == 3).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(factory.builds("youtube"), 3, "one restart per import");
        engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn manual_source_forwards_until_its_input_is_dropped() {
        let (engine, _) = start_scripted(&[]).await;
        let handle = engine.handle();
        let (_, mut hub) = engine.hub().subscribe();

        let (id, input) = handle.add_manual_source().await.unwrap();
        let s = wait_for(&handle, |s| {
            s.source(id).is_some_and(|s| s.health() == Health::Ok)
        })
        .await;
        assert_eq!(s.source(id).unwrap().label(), "Test messages");
        assert_eq!(s.source(id).unwrap().config, None, "it has no settings");
        assert!(
            handle.update_source(id, SourceConfig::Demo).await.is_err(),
            "so there are none to change"
        );

        let msg = chat_core::demo::sample_messages(0).remove(0);
        input.send(ChatEvent::Message(msg.clone())).await.unwrap();
        match hub.recv().await.unwrap() {
            ChatEvent::Message(m) => assert_eq!(m.id, msg.id),
            other => panic!("expected the message, got {other:?}"),
        }
        wait_for(&handle, |s| s.source(id).unwrap().messages == 1).await;

        // Stopping and starting again re-attaches to the same input.
        handle.stop_source(id).await.unwrap();
        handle.start_source(id).await.unwrap();
        wait_for(&handle, |s| {
            s.source(id).unwrap().state == SourceState::Running
        })
        .await;
        input.send(ChatEvent::Message(msg)).await.unwrap();
        wait_for(&handle, |s| s.source(id).unwrap().messages == 2).await;

        drop(input); // the window closed
        wait_for(&handle, |s| {
            s.source(id).unwrap().state == SourceState::Finished
        })
        .await;
        engine.shutdown().await;
    }

    // ---- saving config.toml ----

    /// Where a test's `config.toml` goes, in a fresh folder.
    fn temp_config(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-engine-save-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.toml")
    }

    /// Changes to the settings and the sources are saved: the sources in
    /// the order they were added, each with its on/off switch. The test
    /// window's source has no settings and isn't saved. Starting saves
    /// nothing (a file nobody changed keeps its comments).
    #[tokio::test(start_paused = true)]
    async fn changes_are_saved_with_the_sources() {
        let path = temp_config("changes");
        let switched_off = SourceEntry {
            config: script("fail-then-run:99"),
            enabled: false,
        };
        let engine = Engine::start_with(
            EngineConfig {
                settings: local(),
                sources: vec![script("emit:0").into(), switched_off.clone()],
                save_to: Some(path.clone()),
                ..EngineConfig::default()
            },
            Arc::new(Scripted::default()),
        )
        .await
        .unwrap();
        let handle = engine.handle();
        let s = wait_for(&handle, |s| s.sources.len() == 2).await;
        assert!(!path.exists(), "nothing changed, nothing written");

        let saved = || ConfigFile::load(&path).unwrap();
        handle
            .edit_settings(|s| s.server.newest = Newest::Top)
            .await
            .unwrap();
        assert_eq!(saved().server.newest, Newest::Top);
        assert_eq!(saved().sources, [script("emit:0").into(), switched_off]);

        // Switched off and on: remembered.
        let (first, second) = (s.sources[0].id, s.sources[1].id);
        handle.stop_source(first).await.unwrap();
        handle.start_source(second).await.unwrap();
        let switches: Vec<bool> = saved().sources.iter().map(|e| e.enabled).collect();
        assert_eq!(switches, [false, true], "on, even while it's failing");

        // Added at the end, removed, changed; the test window's never.
        let added = handle.add_source(script("emit:1")).await.unwrap();
        handle.remove_source(first).await.unwrap();
        handle.update_source(added, script("emit:2")).await.unwrap();
        let (_manual, _input) = handle.add_manual_source().await.unwrap();
        let configs: Vec<SourceConfig> = saved().sources.into_iter().map(|e| e.config).collect();
        assert_eq!(configs, [script("fail-then-run:99"), script("emit:2")]);

        engine.shutdown().await;
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Saving can fail (here: a folder is where the file should go). The
    /// status says so, until a save works again.
    #[tokio::test]
    async fn a_failed_save_shows_in_the_status() {
        let path = temp_config("failed");
        std::fs::create_dir_all(&path).unwrap();
        let engine = Engine::start(EngineConfig {
            settings: local(),
            save_to: Some(path.clone()),
            ..EngineConfig::default()
        })
        .await
        .unwrap();
        let handle = engine.handle();

        handle
            .edit_settings(|s| s.server.history = 5)
            .await
            .unwrap();
        let status = wait_for(&handle, |s| s.save_error.is_some()).await;
        let error = status.save_error.unwrap();
        assert!(error.contains("config.toml"), "names the file: {error}");
        assert_eq!(
            handle.settings().borrow().server.history,
            5,
            "applied anyway"
        );

        std::fs::remove_dir(&path).unwrap();
        handle
            .edit_settings(|s| s.server.history = 6)
            .await
            .unwrap();
        wait_for(&handle, |s| s.save_error.is_none()).await;
        assert_eq!(ConfigFile::load(&path).unwrap().server.history, 6);

        engine.shutdown().await;
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A change sent right before the engine shuts down (the settings
    /// window's last edit, while the app quits) is still applied and saved,
    /// though nobody waited for the engine's answer.
    #[tokio::test]
    async fn a_change_sent_just_before_shutdown_is_saved() {
        let path = temp_config("shutdown");
        let engine = Engine::start(EngineConfig {
            settings: local(),
            save_to: Some(path.clone()),
            ..EngineConfig::default()
        })
        .await
        .unwrap();
        let _never_awaited = engine.handle().edit_settings(|s| s.server.history = 7);
        engine.shutdown().await;
        assert_eq!(ConfigFile::load(&path).unwrap().server.history, 7);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn add_and_remove_at_runtime() {
        let (engine, _) = start_scripted(&[]).await;
        let handle = engine.handle();
        let id = handle.add_source(script("emit:0")).await.unwrap();
        wait_for(&handle, |s| s.source(id).is_some()).await;

        handle.remove_source(id).await.unwrap();
        wait_for(&handle, |s| s.sources.is_empty()).await;
        assert!(handle.remove_source(id).await.is_err(), "unknown id");
        engine.shutdown().await;
    }
}
