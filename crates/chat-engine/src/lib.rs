//! Ties the pieces together and owns their lifetimes:
//!
//! ```text
//! sources ── mpsc ──→ hub task ──→ Hub (broadcast + history) ──→ server (SSE → overlays)
//! ```
//!
//! This is the data plane only. The control plane (commands from the UI,
//! status back to it) builds on top of this later.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use chat_core::demo::DemoSource;
use chat_core::{ChatEvent, ChatSource, Hub};
use chat_server::ServerState;
use chat_twitch::TwitchSource;
use chat_youtube::{YouTubeSource, YouTubeTarget};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::{error, info};

pub const DEFAULT_PORT: u16 = 7878;
pub const DEFAULT_HISTORY: usize = 20;

/// A source to run, carrying its own settings.
pub enum SourceSpec {
    Twitch(TwitchSource),
    YouTube(YouTubeSource),
    Demo(DemoSource),
}

impl SourceSpec {
    /// Short label for logs.
    fn name(&self) -> String {
        match self {
            SourceSpec::Twitch(s) => format!("twitch:{}", s.channel),
            SourceSpec::YouTube(s) => match &s.target {
                YouTubeTarget::Video(id) => format!("youtube:{id}"),
                YouTubeTarget::OwnBroadcast => "youtube:own".into(),
            },
            SourceSpec::Demo(_) => "demo".into(),
        }
    }

    /// Runs whichever source this is. No `dyn` needed: the `match` picks
    /// the concrete source, and this async fn's future simply has room for
    /// any of them. Every source ends up as the same future type, which is
    /// all `tokio::spawn` cares about.
    async fn run(self, tx: mpsc::Sender<ChatEvent>) -> anyhow::Result<()> {
        match self {
            SourceSpec::Twitch(s) => s.run(tx).await,
            SourceSpec::YouTube(s) => s.run(tx).await,
            SourceSpec::Demo(s) => s.run(tx).await,
        }
    }
}

pub struct EngineConfig {
    pub sources: Vec<SourceSpec>,
    /// Where the overlay is served. Localhost only by default: nothing else
    /// on the network can reach it.
    pub bind: SocketAddr,
    /// Messages replayed to a newly connected overlay (0 = none).
    pub history: usize,
    /// Folder with a custom `message.html` / `overlay.css`.
    pub theme_dir: Option<PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            bind: (Ipv4Addr::LOCALHOST, DEFAULT_PORT).into(),
            history: DEFAULT_HISTORY,
            theme_dir: None,
        }
    }
}

pub struct Engine {
    hub: Arc<Hub>,
    addr: SocketAddr,
    shutdown: CancellationToken,
    // A JoinSet aborts all its tasks when dropped, so even an Engine that is
    // dropped without `shutdown()` doesn't leave tasks running.
    tasks: JoinSet<()>,
}

impl Engine {
    pub async fn start(config: EngineConfig) -> anyhow::Result<Self> {
        // Bind first: if the port is taken, fail before anything else runs.
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("can't listen on {}", config.bind))?;
        let addr = listener.local_addr()?;

        let hub = Arc::new(Hub::new(config.history));
        let shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();

        // Sources share one mpsc channel into the hub task. mpsc gives
        // backpressure (a source waits if the hub is busy), the hub fans
        // out to any number of consumers.
        let (tx, mut rx) = mpsc::channel(256);
        for spec in config.sources {
            tasks.spawn(supervise(spec, tx.clone(), shutdown.clone()));
        }
        // Only the sources hold senders now. Once they have all ended,
        // `recv()` returns `None` and the hub task finishes by itself.
        drop(tx);

        let hub_in = hub.clone();
        tasks.spawn(async move {
            while let Some(event) = rx.recv().await {
                hub_in.publish(event);
            }
        });

        let state = ServerState {
            hub: hub.clone(),
            theme_dir: config.theme_dir,
            shutdown: shutdown.clone(),
        };
        tasks.spawn(async move {
            if let Err(e) = chat_server::serve(listener, state).await {
                error!("overlay server failed: {e}");
            }
        });

        info!("overlay running at http://{addr}/");
        Ok(Self {
            hub,
            addr,
            shutdown,
            tasks,
        })
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

/// Runs one source in its own task and reports how it ended.
///
/// The source gets its own task (rather than running inline here) so a
/// panic inside it arrives as a `JoinError` we can log, instead of silently
/// taking this supervisor down with it.
async fn supervise(spec: SourceSpec, tx: mpsc::Sender<ChatEvent>, shutdown: CancellationToken) {
    let name = spec.name();
    info!(source = %name, "starting");
    // Abort-on-drop: if this supervisor is itself aborted (Engine dropped),
    // the source goes with it instead of running on detached.
    let mut task = AbortOnDropHandle::new(tokio::spawn(spec.run(tx)));

    tokio::select! {
        result = &mut task => match result {
            Ok(Ok(())) => info!(source = %name, "source finished"),
            Ok(Err(e)) => error!(source = %name, "source failed: {e:#}"),
            Err(e) => error!(source = %name, "source panicked: {e}"),
        },
        () = shutdown.cancelled() => {
            // Cancelling async work in Rust = dropping its future. Aborting
            // the task drops the source's future at its next `.await`.
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(5);

    async fn start_demo() -> Engine {
        Engine::start(EngineConfig {
            sources: vec![SourceSpec::Demo(DemoSource {
                interval: Duration::from_millis(5),
            })],
            bind: (Ipv4Addr::LOCALHOST, 0).into(),
            ..EngineConfig::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn events_flow_from_source_to_hub() {
        let engine = start_demo().await;
        let (_, mut rx) = engine.hub().subscribe();
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
}
