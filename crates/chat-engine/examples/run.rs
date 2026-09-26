//! Runs the engine headless: sources in, overlay out, status in the terminal.
//!
//!   cargo run -p chat-engine --example run -- --demo
//!   cargo run -p chat-engine --example run -- --twitch your_channel --youtube-own
//!
//! Then add http://127.0.0.1:7878/ as a Browser Source in OBS.
//!
//! Flags (combine freely; --twitch and --youtube may repeat):
//!   --demo                sample messages of every kind, including deletions
//!   --twitch <channel>
//!   --youtube-own         your own current/next broadcast (the streamer path);
//!                         needs a login, see below
//!   --youtube <video_id>  a specific public video (testing path); uses
//!                         YOUTUBE_API_KEY if set, otherwise the login
//!   --theme <dir>         custom message.html and/or overlay.css
//!   --port <n>            default 7878
//!   --history <n>         messages replayed to a new overlay, default 20
//!   --stagger <ms>        spacing of message bursts in the overlay, default 250 (0 = off)
//!   --stagger-max <ms>    most a message may be delayed by it, default 2000 (max 5000)
//!
//! YouTube login (once; the token is kept in the system keyring):
//!   cargo run -p chat-engine --example run -- --youtube-login
//!   cargo run -p chat-engine --example run -- --youtube-logout
//!
//! YouTube settings come from the environment (the config file follows):
//!   YOUTUBE_CLIENT_ID, YOUTUBE_CLIENT_SECRET  your OAuth client (docs/youtube-setup.md)
//!   YOUTUBE_API_KEY                           optional, for --youtube <video_id>
//!   YOUTUBE_EMOJIS=<export.json>              optional custom emoji
//!
//! RUST_LOG=debug shows more detail (e.g. YouTube's routine reconnects).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, bail};
use chat_engine::{Engine, EngineConfig, Health, SourceConfig, Stagger, Status, YouTubeSettings};
use chat_youtube::Auth;
use chat_youtube::oauth::{self, OAuthApp, TokenProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let youtube = youtube_settings_from_env();
    match std::env::args().nth(1).as_deref() {
        Some("--youtube-login") => return youtube_login(&youtube.oauth_app()?).await,
        Some("--youtube-logout") => {
            oauth::logout(&youtube.oauth_app()?).await?;
            println!("logged out of YouTube");
            return Ok(());
        }
        _ => {}
    }

    let mut config = parse_args()?;
    config.youtube = youtube;
    if config.sources.is_empty() {
        bail!("no sources given; try --demo (see the top of examples/run.rs)");
    }

    let engine = Engine::start(config).await?;
    println!(
        "\n  overlay: http://{}/   (Ctrl+C to stop)\n",
        engine.addr()
    );
    tokio::spawn(print_status_changes(engine.handle().status()));

    tokio::signal::ctrl_c().await?;
    println!("shutting down…");
    engine.shutdown().await;
    Ok(())
}

/// Prints a line with a status light whenever a source's state changes.
async fn print_status_changes(mut status: tokio::sync::watch::Receiver<Status>) {
    let mut shown = HashMap::new();
    loop {
        // `borrow_and_update` marks the value as seen, so `changed()` below
        // waits for the next one.
        for source in &status.borrow_and_update().sources {
            // Compare state and activity, not the summary text: a retry's
            // text contains a countdown that would print a line per second.
            let key = format!("{:?}{:?}", source.state, source.activity);
            if shown.get(&source.id) != Some(&key) {
                let light = match source.health() {
                    Health::Ok => "🟢",
                    Health::Warning => "🟡",
                    Health::Error => "🔴",
                    Health::Off => "⚪",
                };
                println!("{light} {}: {}", source.label(), source.summary());
                shown.insert(source.id, key);
            }
        }
        if status.changed().await.is_err() {
            return; // engine shut down
        }
    }
}

fn youtube_settings_from_env() -> YouTubeSettings {
    let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    YouTubeSettings {
        client_id: var("YOUTUBE_CLIENT_ID"),
        client_secret: var("YOUTUBE_CLIENT_SECRET"),
        api_key: var("YOUTUBE_API_KEY"),
        emojis: var("YOUTUBE_EMOJIS").map(Into::into),
    }
}

fn parse_args() -> anyhow::Result<EngineConfig> {
    let mut config = EngineConfig::default();
    let mut port = chat_engine::DEFAULT_PORT;
    let (mut stagger_ms, mut stagger_max_ms) = (250, 2000);

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--demo" => config.sources.push(SourceConfig::Demo),
            "--twitch" => config.sources.push(SourceConfig::Twitch {
                channel: next_value(&mut args, "--twitch")?,
            }),
            "--youtube" => config.sources.push(SourceConfig::YouTube {
                video_id: Some(next_value(&mut args, "--youtube")?),
            }),
            "--youtube-own" => config
                .sources
                .push(SourceConfig::YouTube { video_id: None }),
            "--theme" => config.theme_dir = Some(next_value(&mut args, "--theme")?.into()),
            "--port" => port = next_value(&mut args, "--port")?.parse().context("--port")?,
            "--history" => {
                config.history = next_value(&mut args, "--history")?
                    .parse()
                    .context("--history")?
            }
            "--stagger" => {
                stagger_ms = next_value(&mut args, "--stagger")?
                    .parse()
                    .context("--stagger")?
            }
            "--stagger-max" => {
                stagger_max_ms = next_value(&mut args, "--stagger-max")?
                    .parse()
                    .context("--stagger-max")?
            }
            other => bail!("unknown flag {other:?} (see the top of examples/run.rs)"),
        }
    }
    config.bind = (Ipv4Addr::LOCALHOST, port).into();
    config.stagger = Stagger::new(
        Duration::from_millis(stagger_ms),
        Duration::from_millis(stagger_max_ms),
    );
    Ok(config)
}

/// Logs in once: the browser shows Google's consent page, the refresh token
/// ends up in the system keyring.
async fn youtube_login(app: &OAuthApp) -> anyhow::Result<()> {
    let pending = oauth::begin_login(app).await?;
    println!(
        "Opening Google's login page. If no browser opens, visit:\n\n  {}\n",
        pending.url()
    );
    let _ = webbrowser::open(pending.url());
    pending.complete().await?;

    let auth = Auth::OAuth(TokenProvider::from_store(app.clone()).await?);
    let channel = chat_youtube::channel_title(&reqwest::Client::new(), &auth).await?;
    println!("logged in to YouTube as \"{channel}\"; start with --youtube-own");
    Ok(())
}

/// The argument following a flag, e.g. the channel after --twitch.
fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next().with_context(|| format!("{flag} needs a value"))
}
