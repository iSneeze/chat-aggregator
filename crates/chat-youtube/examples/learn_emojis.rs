//! Runs only the web chat reader (`chat_youtube::web_chat`) on a live
//! video and prints every custom emoji it learns. No API key or login.
//!
//! usage: learn_emojis <video id>
//! RUST_LOG=chat_youtube=debug also shows each page load and poll.
//!
//! Prints real emoji codes of the channel: keep the output out of the repo.

use chat_youtube::{rest_client, web_chat};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let Some(video_id) = std::env::args().nth(1) else {
        anyhow::bail!("usage: learn_emojis <video id>");
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(web_chat::learn(
        rest_client(),
        "https://www.youtube.com".into(),
        video_id,
        tx,
    ));
    while let Some(batch) = rx.recv().await {
        for (code, url) in batch {
            println!("{code}  {url}");
        }
    }
    Ok(())
}
