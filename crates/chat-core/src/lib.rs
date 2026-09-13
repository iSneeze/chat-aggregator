use std::future::Future;
use tokio::sync::mpsc;

pub trait ChatSource {
    /// Connects to the platform and pushes normalized messages until
    /// the stream ends or an unrecoverable error occurs.
    fn run(
        self: Box<Self>,
        tx: mpsc::Sender<ChatMessage>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub platform: ChatPlatform,
    pub author: Author,
    pub text: String,
    pub emotes: Vec<EmoteRef>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub kind: MessageKind,
}

/// Storing emotes with the message ensures that the ids match, cause they can change per channel, or over time.
/// Trading correctness over performance
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmoteRef {
    pub code: String, // text as it appears in the message, e.g. "Kappa"
    pub url: String,  // platform-resolved image URL
}

#[derive(Debug, Clone, serde::Serialize)]
pub enum ChatPlatform {
    Twitch,
    YouTube,
    Rplay,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Author {
    pub id: String, // platform-specific ID
    pub name: String,
    pub color: Option<String>, // if applicable, maybe I randomize one for youtube
    pub badges: Vec<String>,   // mod, sub, etc.
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub enum MessageKind {
    Text,
    EmoteOnly { emotes: Vec<String> },
    Donation { amount: String },
    Special { emote_url: Option<String> }, // Youtube Stickers, Twitch Giant Emote
    MembershipJoin { info: String },
    MembershipGift { amount: usize },
    SystemNotice, // Raids, etc.
}

pub struct MockSource {
    pub count: usize,
    pub delay: std::time::Duration,
}

impl ChatSource for MockSource {
    async fn run(self: Box<Self>, tx: mpsc::Sender<ChatMessage>) -> anyhow::Result<()> {
        for i in 0..self.count {
            tokio::time::sleep(self.delay).await;

            let msg = ChatMessage {
                platform: ChatPlatform::Twitch,
                author: Author {
                    id: format!("mock-user-{i}"),
                    name: format!("MockUser{i}"),
                    color: Some("#7f5af0".to_string()),
                    badges: vec![],
                    avatar_url: None,
                },
                text: format!("mock message number {i}"),
                emotes: vec![],
                timestamp: chrono::Utc::now(),
                kind: MessageKind::Text,
            };
            println!("{:?}", &msg);
            // If this errors, the receiver was dropped: nobody is
            // listening anymore, so we shut down gracefully.
            tx.send(msg).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn mock_source_sends_messages_then_ends() {
        let (tx, mut rx) = mpsc::channel(16);

        // Spawn the source as a task, exactly like the server will.
        tokio::spawn(
            Box::new(MockSource {
                count: 5,
                delay: Duration::from_millis(1),
            })
            .run(tx),
        );

        let mut received = Vec::new();
        while let Some(msg) = rx.recv().await {
            received.push(msg);
        }

        assert_eq!(received.len(), 5);
        assert_eq!(received[0].text, "mock message number 0");
        assert_eq!(received[0].author.name, "MockUser0");
        assert!(matches!(received[2].kind, MessageKind::Text));
    }

    #[tokio::test]
    async fn source_shuts_down_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel(16);

        // Drop the receiver immediately: sends must fail, not hang.
        drop(rx);

        let result = Box::new(MockSource {
            count: 3,
            delay: Duration::from_millis(1),
        })
        .run(tx)
        .await;

        assert!(result.is_err());
    }
}
