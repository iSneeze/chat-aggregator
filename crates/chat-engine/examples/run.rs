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
//!   --youtube <video_id>  needs YOUTUBE_API_KEY
//!   --youtube-own         your own broadcast; needs YOUTUBE_ACCESS_TOKEN
//!   --theme <dir>         custom message.html and/or overlay.css
//!   --port <n>            default 7878
//!   --history <n>         messages replayed to a new overlay, default 20
//!
//! YOUTUBE_EMOJIS=<export.json> adds custom emoji to all YouTube sources.
//! RUST_LOG=debug shows more detail (e.g. YouTube's routine reconnects).

use std::net::Ipv4Addr;

use anyhow::{Context, bail};
use chat_core::demo::DemoSource;
use chat_engine::{Engine, EngineConfig, SourceSpec};
use chat_twitch::TwitchSource;
use chat_youtube::{Auth, EmojiMap, YouTubeSource, YouTubeTarget};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = parse_args()?;
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

fn parse_args() -> anyhow::Result<EngineConfig> {
    let mut config = EngineConfig::default();
    let mut port = chat_engine::DEFAULT_PORT;
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
            "--youtube-own" => config.sources.push(SourceSpec::YouTube(YouTubeSource {
                target: YouTubeTarget::OwnBroadcast,
                auth: Auth::Bearer(env("YOUTUBE_ACCESS_TOKEN")?),
                emojis: youtube_emojis(&mut emojis)?,
            })),
            "--theme" => config.theme_dir = Some(next_value(&mut args, "--theme")?.into()),
            "--port" => port = next_value(&mut args, "--port")?.parse().context("--port")?,
            "--history" => {
                config.history = next_value(&mut args, "--history")?
                    .parse()
                    .context("--history")?
            }
            other => bail!("unknown flag {other:?} (see the top of examples/run.rs)"),
        }
    }
    config.bind = (Ipv4Addr::LOCALHOST, port).into();
    Ok(config)
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
