//! Renders one message of every kind into a standalone HTML page, for
//! styling without a live chat or server:
//!
//!   cargo run -p chat-render --example preview > preview.html
//!   cargo run -p chat-render --example preview -- my-theme/ > preview.html
//!
//! The optional folder may contain `message.html` and/or `overlay.css`.

use chat_render::Theme;

fn main() -> anyhow::Result<()> {
    let theme = match std::env::args().nth(1) {
        Some(dir) => Theme::load(dir)?,
        None => Theme::builtin(),
    };

    let mut messages = String::new();
    for msg in chat_core::demo::sample_messages(0) {
        messages.push_str(&theme.render(&msg)?);
    }

    println!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>chat overlay preview</title>
<style>
{css}
</style>
<style>
/* preview only: a checkerboard stands in for the game behind the overlay */
html {{ background: repeating-conic-gradient(#3b3f4a 0 25%, #454a57 0 50%) 0 0 / 40px 40px; }}
.chat {{ width: 420px; height: auto; }}
</style>
</head>
<body>
<main class="chat" role="log">
{messages}</main>
</body>
</html>"#,
        css = theme.css()
    );
    Ok(())
}
