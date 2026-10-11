use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::{AsRawIRC, ServerMessage};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

/// Prints everything Twitch sends for a channel as raw IRC lines, before
/// `TwitchSource` filters anything. For finding events the source drops
/// (e.g. unknown USERNOTICE types like watch streaks):
///
///   cargo run -p chat-twitch --example raw_twitch -- <channel> | tee /tmp/twitch-raw.log
///
/// The log contains real chatter names and ids: keep it out of the repo.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let channel = std::env::args()
        .nth(1)
        .expect("usage: cargo run -p chat-twitch --example raw_twitch -- <channel>");
    let channel = chat_twitch::normalize_channel(&channel).map_err(anyhow::Error::msg)?;

    // Same anonymous connection as `TwitchSource`, so this sees exactly
    // what the source receives.
    let config = ClientConfig::new_simple(StaticLoginCredentials::anonymous());
    let (mut incoming_messages, client) =
        TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);
    client.join(channel)?;

    while let Some(message) = incoming_messages.recv().await {
        // Keep-alive traffic, nothing to see.
        if matches!(message, ServerMessage::Ping(_) | ServerMessage::Pong(_)) {
            continue;
        }
        // `source()` borrows the parsed line: printing needs no copy.
        let raw = message.source();
        // The event type of USERNOTICE/NOTICE lines, up front for grepping.
        let label = match raw.tags.0.get("msg-id") {
            Some(msg_id) => format!("{} msg-id={msg_id}", raw.command),
            None => raw.command.clone(),
        };
        println!(
            "{} [{label}] {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            raw.as_raw_irc()
        );
    }
    Ok(())
}
