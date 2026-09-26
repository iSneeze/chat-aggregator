//! Runs the engine headless: sources in, overlay out, status in the terminal.
//!
//!   cargo run -p chat-engine --example run                  # everything from your config file
//!   cargo run -p chat-engine --example run -- --init-config # create a commented config file
//!   cargo run -p chat-engine --example run -- --demo        # quick test, ignores the file's sources
//!
//! Then add http://127.0.0.1:7878/ as a Browser Source in OBS.
//!
//! Settings ([server], [youtube]) come from the config file: `--config <file>`,
//! or the default one (~/.config/chat-aggregator/config.toml on Linux) if it
//! exists. Its [[sources]] are used unless you give source flags, which then
//! replace them. Other flags override the file.
//!
//!   --config <file>       use this config file
//!   --init-config [file]  write a commented config file (default location) and exit
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
//! These environment variables override the file's [youtube] settings:
//!   YOUTUBE_CLIENT_ID, YOUTUBE_CLIENT_SECRET  your OAuth client (docs/youtube-setup.md)
//!   YOUTUBE_API_KEY                           optional, for --youtube <video_id>
//!   YOUTUBE_EMOJIS=<export.json>              optional custom emoji
//!
//! RUST_LOG=debug shows more detail (e.g. YouTube's routine reconnects).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use chat_engine::{
    ConfigFile, Engine, EngineConfig, Health, SourceConfig, Status, YouTubeSettings,
};
use chat_youtube::Auth;
use chat_youtube::oauth::{self, OAuthApp, TokenProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--init-config") {
        let path = match args.get(1) {
            Some(path) => PathBuf::from(path),
            None => ConfigFile::default_path()?,
        };
        ConfigFile::write_template(&path)?;
        println!(
            "wrote {}; edit it, then start without arguments",
            path.display()
        );
        return Ok(());
    }

    let config = build_config(&args)?;
    match args.first().map(String::as_str) {
        Some("--youtube-login") => return youtube_login(&config.youtube.oauth_app()?).await,
        Some("--youtube-logout") => {
            oauth::logout(&config.youtube.oauth_app()?).await?;
            println!("logged out of YouTube");
            return Ok(());
        }
        _ => {}
    }
    if config.sources.is_empty() {
        bail!(
            "no sources: add [[sources]] to your config file (create one with --init-config), \
             or try --demo (see the top of examples/run.rs)"
        );
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
    let mut shown_clients = (0, 0);
    loop {
        // The borrow holds a lock (it's not `Send`) and must be gone before
        // the `.await` below. An explicit `drop()` isn't enough: the compiler
        // decides what lives across an await by *scope*, so the borrow gets
        // its own block.
        {
            // `borrow_and_update` marks the value as seen, so `changed()`
            // below waits for the next one.
            let current = status.borrow_and_update();
            let clients = (current.overlays_connected, current.api_clients);
            if clients != shown_clients {
                println!(
                    "📺 overlays connected: {}, API clients: {}",
                    clients.0, clients.1
                );
                shown_clients = clients;
            }
            for source in &current.sources {
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
        }
        if status.changed().await.is_err() {
            return; // engine shut down
        }
    }
}

/// The config file, then flags on top, then environment variables.
fn build_config(args: &[String]) -> anyhow::Result<EngineConfig> {
    let mut file = match flag_value(args, "--config")? {
        Some(path) => ConfigFile::load(&PathBuf::from(path))?,
        None => ConfigFile::load_or_default(&ConfigFile::default_path()?)?,
    };

    let mut flag_sources = Vec::new();
    let mut args = args.iter().cloned();
    while let Some(flag) = args.next() {
        let server = &mut file.server;
        match flag.as_str() {
            "--config" => {
                next_value(&mut args, "--config")?; // already loaded above
            }
            "--youtube-login" | "--youtube-logout" => {}
            "--demo" => flag_sources.push(SourceConfig::Demo),
            "--twitch" => flag_sources.push(SourceConfig::Twitch {
                channel: next_value(&mut args, "--twitch")?,
            }),
            "--youtube" => flag_sources.push(SourceConfig::YouTube {
                video_id: Some(next_value(&mut args, "--youtube")?),
            }),
            "--youtube-own" => flag_sources.push(SourceConfig::YouTube { video_id: None }),
            "--theme" => server.theme_dir = Some(next_value(&mut args, "--theme")?.into()),
            "--port" => server.port = parse(&mut args, "--port")?,
            "--history" => server.history = parse(&mut args, "--history")?,
            "--stagger" => server.stagger_ms = parse(&mut args, "--stagger")?,
            "--stagger-max" => server.stagger_max_ms = parse(&mut args, "--stagger-max")?,
            other => bail!("unknown flag {other:?} (see the top of examples/run.rs)"),
        }
    }
    // Source flags replace the file's sources, so a quick `--demo` doesn't
    // also start everything from your config.
    if !flag_sources.is_empty() {
        file.sources = flag_sources;
    }
    apply_env(&mut file.youtube);
    Ok(file.into_engine_config())
}

fn apply_env(youtube: &mut YouTubeSettings) {
    let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    if let Some(v) = var("YOUTUBE_CLIENT_ID") {
        youtube.client_id = Some(v);
    }
    if let Some(v) = var("YOUTUBE_CLIENT_SECRET") {
        youtube.client_secret = Some(v);
    }
    if let Some(v) = var("YOUTUBE_API_KEY") {
        youtube.api_key = Some(v);
    }
    if let Some(v) = var("YOUTUBE_EMOJIS") {
        youtube.emojis = Some(v.into());
    }
}

/// The value after `flag` anywhere in `args`, if the flag is there.
fn flag_value<'a>(args: &'a [String], flag: &str) -> anyhow::Result<Option<&'a str>> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => args
            .get(i + 1)
            .map(|v| Some(v.as_str()))
            .with_context(|| format!("{flag} needs a value")),
    }
}

fn parse<T>(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    next_value(args, flag)?
        .parse()
        .with_context(|| format!("invalid value for {flag}"))
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
