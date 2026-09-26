use anyhow::Context;
use chat_core::{ChatEvent, ChatMessage, ChatSource, MessageKind};
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

    // usage: listen_youtube <video_id>            (API key)
    //        listen_youtube --member <video_id>   (OAuth, e.g. members-only stream)
    //        listen_youtube --own                 (OAuth, your own broadcast)
    // optional: YOUTUBE_EMOJIS=<export.json> from scripts/yt-emoji-export.js
    let args: Vec<String> = std::env::args().collect();

    let (target, auth) = match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("--own"), _) => (
            YouTubeTarget::OwnBroadcast,
            Auth::Bearer(env("YOUTUBE_ACCESS_TOKEN")),
        ),
        (Some("--member"), Some(video_id)) => (
            YouTubeTarget::Video(video_id.to_string()),
            Auth::Bearer(env("YOUTUBE_ACCESS_TOKEN")),
        ),
        (Some(video_id), None) => (
            YouTubeTarget::Video(video_id.to_string()),
            Auth::ApiKey(env("YOUTUBE_API_KEY")),
        ),
        _ => anyhow::bail!("usage: listen_youtube <video_id> | --member <video_id> | --own"),
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
            auth,
            emojis,
        }
        .run(tx),
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
        MessageKind::Donation { amount } => format!("DONATION {amount}"),
        MessageKind::Special { .. } => "special".into(),
        MessageKind::MembershipJoin { .. } => "MEMBER".into(),
        MessageKind::MembershipGift { amount } => format!("GIFT x{amount}"),
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

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}
