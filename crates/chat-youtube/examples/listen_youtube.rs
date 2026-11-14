use anyhow::Context;
use chat_core::{ChatEvent, ChatMessage, ChatSource, MessageKind, Reporter};
use chat_youtube::oauth::{self, Account, OAuthApp, TokenProvider};
use chat_youtube::{Auth, EmojiMap, YouTubeSource, YouTubeTarget};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Without a subscriber, the source's tracing logs would go nowhere.
    // RUST_LOG=chat_youtube=debug also shows the routine reconnects.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // usage: listen_youtube <video_id>                      (API key)
    //        listen_youtube --login                         (OAuth: log in once in the browser;
    //                                                        prints the channel id)
    //        listen_youtube --own <channel_id>              (OAuth, your own broadcast)
    //        listen_youtube --member <channel_id> <video_id> (OAuth, e.g. members-only stream)
    //        listen_youtube --logout <channel_id>
    // OAuth needs YOUTUBE_CLIENT_ID and YOUTUBE_CLIENT_SECRET (docs/youtube-setup.md).
    // optional: YOUTUBE_EMOJIS=<export.json> from scripts/yt-emoji-export.user.js
    let args: Vec<String> = std::env::args().collect();

    let arg = |i: usize| args.get(i).map(String::as_str);
    let target = match (arg(1), arg(2), arg(3)) {
        (Some("--login"), None, None) => return login(&app_from_env()?).await,
        (Some("--logout"), Some(channel), None) => {
            oauth::logout(&app_from_env()?, &Account::Channel(channel.into())).await?;
            println!("logged out");
            return Ok(());
        }
        (Some("--own"), Some(channel), None) => YouTubeTarget::OwnBroadcast {
            login: stored_login(channel).await?,
        },
        (Some("--member"), Some(channel), Some(video_id)) => YouTubeTarget::Video {
            id: video_id.to_string(),
            auth: Auth::OAuth(stored_login(channel).await?),
        },
        (Some(video_id), None, None) if !video_id.starts_with("--") => YouTubeTarget::Video {
            id: video_id.to_string(),
            auth: Auth::ApiKey(env("YOUTUBE_API_KEY")),
        },
        _ => anyhow::bail!(
            "usage: listen_youtube <video_id> | --login | --own <channel_id> \
             | --member <channel_id> <video_id> | --logout <channel_id>"
        ),
    };

    let emojis = match std::env::var("YOUTUBE_EMOJIS") {
        Ok(path) => {
            let map = EmojiMap::load(&path)?;
            println!("loaded {} custom emoji from {path}", map.len());
            map
        }
        Err(_) => EmojiMap::default(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(256);
    let mut source_task = tokio::spawn(
        YouTubeSource {
            target,
            emojis,
            // A quick look at a chat: learned emoji aren't kept.
            emoji_cache: None,
            learn_emojis: true,
        }
        .run(tx, Reporter::detached()),
    );

    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => print_event(event),
                None => {
                    // tx dropped => source finished; its result is the real story.
                    let res = (&mut source_task).await;
                    res.context("source task panicked")??;
                    println!("source ended cleanly");
                    return Ok(());
                },
            },
            res = &mut source_task => {
                // Source task ended: either an error (show it!) or clean shutdown.
                res.context("source task panicked")??;
                println!("source ended cleanly");
                return Ok(());
            }
        }
    }
}

fn print_event(event: ChatEvent) {
    match event {
        ChatEvent::Message(msg) => print_message(&msg),
        ChatEvent::Delete { message_id, .. } => println!("[DELETE] message {message_id}"),
        ChatEvent::ClearUser { user_id, .. } => println!("[CLEAR USER] {user_id}"),
        ChatEvent::ClearAll { .. } => println!("[CLEAR ALL]"),
    }
}

fn print_message(msg: &ChatMessage) {
    let kind = match &msg.kind {
        MessageKind::Text => "text".into(),
        MessageKind::EmoteOnly => "emote-only".into(),
        MessageKind::Donation { amount, .. } => format!("DONATION {amount}"),
        MessageKind::Special { .. } => "special".into(),
        MessageKind::MembershipJoin { .. } => "MEMBER".into(),
        MessageKind::MembershipGift { count } => format!("GIFT x{count}"),
        MessageKind::SystemNotice { info } => format!("notice: {info}"),
    };
    let badges = if msg.author.badges.is_empty() {
        String::new()
    } else {
        format!(" {{{}}}", msg.author.badges.join(","))
    };
    let emotes = if msg.emotes.is_empty() {
        String::new()
    } else {
        let codes: Vec<_> = msg.emotes.iter().map(|e| e.code.as_str()).collect();
        format!(" emotes: {}", codes.join(", "))
    };
    println!(
        "[{}] {}{}: {} <{}> id={}{}",
        kind, msg.author.name, badges, msg.text, msg.timestamp, msg.id, emotes
    );
}

/// Logs in once per channel: the browser shows Google's consent page, the
/// refresh token ends up in the system keyring under the channel picked.
async fn login(app: &OAuthApp) -> anyhow::Result<()> {
    let pending = oauth::begin_login(app).await?;
    println!(
        "Opening Google's login page. If no browser opens, visit:\n\n  {}\n",
        pending.url()
    );
    let _ = webbrowser::open(pending.url());
    let channel = pending.complete().await?;
    println!(
        "logged in as \"{}\"; use it with --own {}",
        channel.title, channel.id
    );
    Ok(())
}

/// The login `--login` stored for `channel`.
async fn stored_login(channel: &str) -> anyhow::Result<TokenProvider> {
    TokenProvider::from_store(app_from_env()?, Account::Channel(channel.into()))
        .await
        .context("run `listen_youtube --login` first (with that channel)")
}

/// Your OAuth client from `YOUTUBE_CLIENT_ID` and `YOUTUBE_CLIENT_SECRET`.
/// (The app and headless mode read it from `config.toml` instead.)
fn app_from_env() -> anyhow::Result<OAuthApp> {
    let var = |name: &str| {
        std::env::var(name).with_context(|| {
            format!("{name} must be set (your OAuth client, see docs/youtube-setup.md)")
        })
    };
    Ok(OAuthApp::new(
        var("YOUTUBE_CLIENT_ID")?,
        var("YOUTUBE_CLIENT_SECRET")?,
    ))
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}
