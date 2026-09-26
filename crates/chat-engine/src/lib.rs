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
use chat_server::ServerState;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

mod actor;
mod config;
mod factory;
mod status;

pub use chat_server::Stagger;
pub use config::{SetupError, SourceConfig, YouTubeSettings};
pub use status::{Health, SourceId, SourceState, SourceStatus, Status};

use actor::{Actor, Command, Reply};
use factory::{Production, SourceFactory};

pub const DEFAULT_PORT: u16 = 7878;
pub const DEFAULT_HISTORY: usize = 20;

pub struct EngineConfig {
    /// Started right away, in this order.
    pub sources: Vec<SourceConfig>,
    pub youtube: YouTubeSettings,
    /// Where the overlay is served. Localhost only by default: nothing else
    /// on the network can reach it.
    pub bind: SocketAddr,
    /// Messages replayed to a newly connected overlay (0 = none).
    pub history: usize,
    /// Folder with a custom `message.html` / `overlay.css`.
    pub theme_dir: Option<PathBuf>,
    /// Spacing of message bursts in the overlay.
    pub stagger: Stagger,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            youtube: YouTubeSettings::default(),
            bind: (Ipv4Addr::LOCALHOST, DEFAULT_PORT).into(),
            history: DEFAULT_HISTORY,
            theme_dir: None,
            stagger: Stagger::default(),
        }
    }
}

/// Controls a running engine. Cheap to clone: the UI, headless mode and
/// tests each hold one. All methods just send a message to the actor and
/// wait for its answer.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<Status>,
}

impl EngineHandle {
    /// Adds a source and starts it.
    pub async fn add_source(&self, config: SourceConfig) -> anyhow::Result<SourceId> {
        self.call(|reply| Command::Add(config, reply)).await
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
    pub async fn update_source(&self, id: SourceId, config: SourceConfig) -> anyhow::Result<()> {
        self.call(|reply| Command::Update(id, config, reply)).await
    }

    /// The live status. A `watch` receiver always holds the latest value;
    /// `changed().await` waits for the next update.
    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// Sends a command with a fresh reply channel and waits for the answer.
    /// `make` builds the command around the reply sender.
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> anyhow::Result<T> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| anyhow!("the engine has shut down"))?;
        answer
            .await
            .map_err(|_| anyhow!("the engine has shut down"))?
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
        let factory = Production::new(config.youtube.clone());
        Self::start_with(config, factory).await
    }

    /// `start` with any source factory; tests use scripted sources.
    async fn start_with<F: SourceFactory>(
        config: EngineConfig,
        factory: F,
    ) -> anyhow::Result<Self> {
        // Bind first: if the port is taken, fail before anything else runs.
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("can't listen on {}", config.bind))?;
        let addr = listener.local_addr()?;

        let hub = Arc::new(Hub::new(config.history));
        let shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();

        let (status_tx, status_rx) = watch::channel(Status {
            overlay_url: format!("http://{addr}/"),
            sources: Vec::new(),
        });
        let (commands_tx, commands_rx) = mpsc::channel(32);
        let actor = Actor::new(factory, hub.clone(), status_tx);
        tasks.spawn(actor.run(commands_rx, shutdown.clone()));

        let state = ServerState {
            hub: hub.clone(),
            theme_dir: config.theme_dir,
            shutdown: shutdown.clone(),
            stagger: config.stagger,
        };
        tasks.spawn(async move {
            if let Err(e) = chat_server::serve(listener, state).await {
                error!("overlay server failed: {e}");
            }
        });

        let handle = EngineHandle {
            commands: commands_tx,
            status: status_rx,
        };
        for source in config.sources {
            handle.add_source(source).await?;
        }

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

    fn local() -> SocketAddr {
        (Ipv4Addr::LOCALHOST, 0).into()
    }

    // ---- with the real sources ----

    async fn start_demo() -> Engine {
        Engine::start(EngineConfig {
            sources: vec![SourceConfig::Demo],
            bind: local(),
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
    async fn taken_port_is_an_error() {
        let engine = start_demo().await;
        let second = Engine::start(EngineConfig {
            bind: engine.addr(),
            ..EngineConfig::default()
        })
        .await;
        assert!(second.is_err());
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn youtube_without_settings_needs_attention() {
        let engine = Engine::start(EngineConfig {
            sources: vec![SourceConfig::YouTube { video_id: None }],
            bind: local(),
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
            let SourceConfig::Twitch { channel: script } = config else {
                anyhow::bail!("scripted sources are Twitch configs");
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
                sources: sources.iter().map(|s| script(s)).collect(),
                bind: local(),
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
        assert_eq!(s.sources[0].config, script("emit:1"));
        assert_eq!(factory.builds("emit:1"), 1);
        engine.shutdown().await;
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
