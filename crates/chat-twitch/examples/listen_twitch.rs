use chat_core::{ChatSource, ChatMessage};
use chat_twitch::TwitchSource;


/// example to test twitch message collection on live channels - live integration test
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let channel = std::env::args()
        .nth(1)
        .expect("usage: cargo run -p chat-twitch --example listen_twitch -- <channel>");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatMessage>(256);

    tokio::spawn(Box::new(TwitchSource { channel }).run(tx));
    
    while let Some(msg) = rx.recv().await {
        let kind = match &msg.kind {
            chat_core::MessageKind::Text => "text".to_string(),
            chat_core::MessageKind::EmoteOnly { emotes } => format!("emote-only {emotes:?}"),
            chat_core::MessageKind::Donation { amount } => format!("DONATION {amount}"),
            chat_core::MessageKind::Special { .. } => "special".to_string(),
            chat_core::MessageKind::MembershipJoin { .. } => "SUB".to_string(),
            chat_core::MessageKind::MembershipGift { amount } => format!("GIFT x{amount}"),
            chat_core::MessageKind::SystemNotice => "notice".to_string(),
        };
        println!("[{}] {} ({}): {} <{}> emotes: {}", kind, msg.author.name, msg.author.id, msg.text, msg.timestamp,
            msg.emotes.iter().map(|e| e.code.as_str()).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}
