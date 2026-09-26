//! Runs the engine headless: sources in, overlay out.
//!
//!   cargo run -p chat-engine --example run -- --demo
//!   cargo run -p chat-engine --example run -- --twitch somechannel --youtube VIDEO_ID
//!
//! Then add http://127.0.0.1:7878/ as a Browser Source in OBS.
//!
//! Flags (combine freely; --twitch and --youtube may repeat):
//!   --demo                sample messages of every kind, including deletions
//!   --twitch <channel>
//!   --youtube <video_id>  any public video; needs YOUTUBE_API_KEY (testing path)
//!   --youtube-own         your own current/next broadcast (the streamer path);
//!                         needs a login, see below
//!   --theme <dir>         custom message.html and/or overlay.css
//!   --port <n>            default 7878
//!   --history <n>         messages replayed to a new overlay, default 20
//!   --stagger <ms>        spacing of message bursts in the overlay, default 250 (0 = off)
//!   --stagger-max <ms>    most a message may be delayed by it, default 2000 (max 5000)
//!
//! YouTube login (once; the token is kept in the system keyring):
//!   cargo run -p chat-engine --example run -- --youtube-login
//!   cargo run -p chat-engine --example run -- --youtube-logout
//! Both, and --youtube-own, need YOUTUBE_CLIENT_ID and YOUTUBE_CLIENT_SECRET
//! from your own Google Cloud project (docs/youtube-setup.md).
//!
//! YOUTUBE_EMOJIS=<export.json> adds custom emoji to all YouTube sources.
//! RUST_LOG=debug shows more detail (e.g. YouTube's routine reconnects).

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, bail};
use chat_core::demo::DemoSource;
use chat_engine::{Engine, EngineConfig, SourceSpec, Stagger};
use chat_twitch::TwitchSource;
use chat_youtube::oauth::{self, OAuthApp, TokenProvider};
use chat_youtube::{Auth, EmojiMap, YouTubeSource, YouTubeTarget};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("--youtube-login") => return youtube_login(&OAuthApp::from_env()?).await,
        Some("--youtube-logout") => {
            oauth::logout(&OAuthApp::from_env()?).await?;
            println!("logged out of YouTube");
            return Ok(());
        }
        _ => {}
    }

    let config = parse_args().await?;
    if config.sources.is_empty() {
        bail!("no sources given; try --demo (see the top of examples/run.rs)");
    }

    let engine = Engine::start(config).await?;
    println!(
        "\n  overlay: http://{}/   (Ctrl+C to stop)\n",
        engine.addr()
    );

    tokio::signal::ctrl_c().await?;
    println!("shutting down…");
    engine.shutdown().await;
    Ok(())
}

async fn parse_args() -> anyhow::Result<EngineConfig> {
    let mut config = EngineConfig::default();
    let mut port = chat_engine::DEFAULT_PORT;
    let (mut stagger_ms, mut stagger_max_ms) = (250, 2000);
    let mut emojis: Option<EmojiMap> = None;

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--demo" => config.sources.push(SourceSpec::Demo(DemoSource::default())),
            "--twitch" => {
                let channel = next_value(&mut args, "--twitch")?;
                config
                    .sources
                    .push(SourceSpec::Twitch(TwitchSource { channel }));
            }
            "--youtube" => {
                let video_id = next_value(&mut args, "--youtube")?;
                config.sources.push(SourceSpec::YouTube(YouTubeSource {
                    target: YouTubeTarget::Video(video_id),
                    auth: Auth::ApiKey(env("YOUTUBE_API_KEY")?),
                    emojis: youtube_emojis(&mut emojis)?,
                }));
            }
            "--youtube-own" => {
                let tokens = TokenProvider::from_store(OAuthApp::from_env()?)
                    .await
                    .context("log in first: run with --youtube-login")?;
                config.sources.push(SourceSpec::YouTube(YouTubeSource {
                    target: YouTubeTarget::OwnBroadcast,
                    auth: Auth::OAuth(tokens),
                    emojis: youtube_emojis(&mut emojis)?,
                }))
            }
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

/// Loads YOUTUBE_EMOJIS once and hands out copies for each YouTube source.
fn youtube_emojis(cache: &mut Option<EmojiMap>) -> anyhow::Result<EmojiMap> {
    if cache.is_none() {
        let map = match std::env::var("YOUTUBE_EMOJIS") {
            Ok(path) => {
                let map = EmojiMap::load(&path)?;
                println!("loaded {} custom emoji from {path}", map.len());
                map
            }
            Err(_) => EmojiMap::default(),
        };
        *cache = Some(map);
    }
    Ok(cache.clone().unwrap_or_default())
}

/// The argument following a flag, e.g. the channel after --twitch.
fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next().with_context(|| format!("{flag} needs a value"))
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}
