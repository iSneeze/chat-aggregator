use chat_core::{ChatEvent, ChatSource, MessageKind, Reporter};
use chat_twitch::TwitchSource;

/// example to test twitch message collection on live channels - live integration test
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let channel = std::env::args()
        .nth(1)
        .expect("usage: cargo run -p chat-twitch --example listen_twitch -- <channel>");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(256);

    tokio::spawn(TwitchSource { channel }.run(tx, Reporter::detached()));

    while let Some(event) = rx.recv().await {
        let msg = match event {
            ChatEvent::Message(msg) => msg,
            ChatEvent::Delete { message_id, .. } => {
                println!("[DELETE] message {message_id}");
                continue;
            }
            ChatEvent::ClearUser { user_id, .. } => {
                println!("[CLEAR USER] {user_id}");
                continue;
            }
            ChatEvent::ClearAll { .. } => {
                println!("[CLEAR ALL]");
                continue;
            }
        };
        let kind = match &msg.kind {
            MessageKind::Text => "text".to_string(),
            MessageKind::EmoteOnly => "emote-only".to_string(),
            MessageKind::Donation { amount, .. } => format!("DONATION {amount}"),
            MessageKind::Special { .. } => "special".to_string(),
            MessageKind::MembershipJoin { .. } => "SUB".to_string(),
            MessageKind::MembershipGift { count } => format!("GIFT x{count}"),
            MessageKind::SystemNotice { info } => format!("notice: {info}"),
        };
        println!(
            "[{}] {} ({}): {} <{}> emotes: {}",
            kind,
            msg.author.name,
            msg.author.id,
            msg.text,
            msg.timestamp,
            msg.emotes
                .iter()
                .map(|e| e.code.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}
