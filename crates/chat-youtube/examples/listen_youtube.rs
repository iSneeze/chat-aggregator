use anyhow::Context;
use chat_core::{ChatMessage, ChatSource, MessageKind};
use chat_youtube::{Auth, YouTubeSource, YouTubeTarget};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // usage: listen_youtube <video_id>            (API key)
    //        listen_youtube --member <video_id>   (OAuth, e.g. members-only stream)
    //        listen_youtube --own                 (OAuth, your own broadcast)
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

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatMessage>(256);
    let mut source_task = tokio::spawn(Box::new(YouTubeSource { target, auth }).run(tx));

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => {
                    let kind = match &msg.kind {
                        MessageKind::Text => "text".into(),
                        MessageKind::EmoteOnly { emotes } => format!("emote-only {emotes:?}"),
                        MessageKind::Donation { amount } => format!("DONATION {amount}"),
                        MessageKind::Special { .. } => "special".into(),
                        MessageKind::MembershipJoin { .. } => "MEMBER".into(),
                        MessageKind::MembershipGift { amount } => format!("GIFT x{amount}"),
                        MessageKind::SystemNotice => "notice".into(),
                    };
                    println!("[{}] {}: {} <{}>", kind, msg.author.name, msg.text, msg.timestamp);
                }
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

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}
