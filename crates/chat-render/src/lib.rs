//! Turns chat messages into styled HTML for the overlay.
//!
//! A [`Theme`] is a message template (minijinja, `message.html`) plus a
//! stylesheet (`overlay.css`). The built-in defaults are compiled into the
//! binary; [`Theme::load`] lets a folder override either file.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::Context;
use chat_core::ChatMessage;
use minijinja::Environment;

pub mod themes;
mod view;

pub const DEFAULT_MESSAGE_TEMPLATE: &str = include_str!("../templates/message.html");
pub const DEFAULT_CSS: &str = include_str!("../templates/overlay.css");

pub const MESSAGE_FILE: &str = "message.html";
pub const CSS_FILE: &str = "overlay.css";

pub struct Theme {
    env: Environment<'static>,
    css: String,
}

impl Theme {
    /// The built-in template and CSS.
    pub fn builtin() -> Self {
        // Can only fail if the built-in template has a syntax error, which
        // the tests below would catch: a programmer error, not a runtime one.
        Self::from_sources(DEFAULT_MESSAGE_TEMPLATE.into(), DEFAULT_CSS.into())
            .expect("built-in message template is valid")
    }

    /// Loads `message.html` and `overlay.css` from `dir`, falling back to
    /// the built-in version for each file that doesn't exist. Syntax errors
    /// in the template are reported here, not at render time.
    pub fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref();
        let message = read_or_default(&dir.join(MESSAGE_FILE), DEFAULT_MESSAGE_TEMPLATE)?;
        let css = read_or_default(&dir.join(CSS_FILE), DEFAULT_CSS)?;
        Self::from_sources(message, css)
            .with_context(|| format!("invalid {MESSAGE_FILE} in {}", dir.display()))
    }

    fn from_sources(message: String, css: String) -> anyhow::Result<Self> {
        let mut env = Environment::new();
        // Drop the newline after a block tag and the indentation before it,
        // so `{% if %}` lines don't leave blank lines in the output.
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        // The `.html` name also switches on HTML auto-escaping.
        env.add_template_owned(MESSAGE_FILE, message)?;
        Ok(Self { env, css })
    }

    /// Renders one message to an HTML fragment (`<article class="msg">…`).
    pub fn render(&self, msg: &ChatMessage) -> anyhow::Result<String> {
        let template = self.env.get_template(MESSAGE_FILE)?;
        Ok(template.render(view::MessageView::new(msg))?)
    }

    pub fn css(&self) -> &str {
        &self.css
    }
}

/// Just the stylesheet from `dir` (or the built-in one). Separate from
/// [`Theme::load`] so a broken template doesn't also discard working CSS.
pub fn read_css(dir: impl AsRef<Path>) -> anyhow::Result<String> {
    read_or_default(&dir.as_ref().join(CSS_FILE), DEFAULT_CSS)
}

impl Default for Theme {
    fn default() -> Self {
        Self::builtin()
    }
}

fn read_or_default(path: &Path, default: &str) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(default.to_string()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::demo::sample_messages;
    use chat_core::{Author, ChatPlatform, EmoteRef, MessageKind};

    fn message(platform: ChatPlatform, text: &str, kind: MessageKind) -> ChatMessage {
        ChatMessage {
            id: "m1".into(),
            platform,
            author: Author {
                id: "u1".into(),
                name: "Ann".into(),
                color: Some("#7f5af0".into()),
                badges: vec!["moderator".into()],
                avatar_url: None,
            },
            text: text.into(),
            emotes: vec![],
            timestamp: chrono::Utc::now(),
            kind,
        }
    }

    fn render(msg: &ChatMessage) -> String {
        Theme::builtin().render(msg).unwrap()
    }

    #[test]
    fn every_sample_renders() {
        let theme = Theme::builtin();
        for msg in sample_messages(0) {
            let html = theme.render(&msg).unwrap();
            assert!(html.starts_with("<article class=\"msg msg--"), "{html}");
            assert!(html.contains(&format!("data-id=\"{}\"", msg.id)), "{html}");
        }
    }

    #[test]
    fn chat_text_is_escaped() {
        let html = render(&message(
            ChatPlatform::Twitch,
            "<script>alert('x')</script>",
            MessageKind::Text,
        ));
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    /// Classic attempts to turn chat into markup or script. They must all
    /// end up as text: in OBS, script in an overlay runs without Chromium's
    /// sandbox (see `OVERLAY_CSP` in chat-server for the second line of
    /// defence).
    const HOSTILE: &[&str] = &[
        r#""><img src=x onerror=alert(1)>"#,
        "'><svg onload=alert(1)>",
        "<script>alert(1)</script>",
        "</p></article><script>alert(1)</script>",
        "javascript:alert(1)",
        "{{ 7*7 }}{% if true %}",
    ];

    /// A message with `value` in every field a chatter or platform controls.
    fn filled_with(value: &str, kind: MessageKind) -> ChatMessage {
        ChatMessage {
            id: value.into(),
            platform: ChatPlatform::Twitch,
            author: Author {
                id: value.into(),
                name: value.into(),
                color: Some(value.into()),
                badges: vec![value.into()],
                avatar_url: Some(value.into()),
            },
            text: value.into(),
            // The whole text is also an emote code, so it lands in the
            // emote's `alt`/`title` attributes too.
            emotes: vec![EmoteRef {
                code: value.into(),
                url: value.into(),
            }],
            timestamp: chrono::Utc::now(),
            kind,
        }
    }

    fn kinds(value: &str) -> Vec<MessageKind> {
        vec![
            MessageKind::Text,
            MessageKind::Donation {
                amount: value.into(),
            },
            MessageKind::Special {
                image_url: Some(value.into()),
                amount: Some(value.into()),
                info: Some(value.into()),
            },
            MessageKind::MembershipJoin { info: value.into() },
            MessageKind::MembershipGift { count: 3 },
            MessageKind::SystemNotice { info: value.into() },
        ]
    }

    /// The characters that make up markup: tags and attribute quotes.
    /// Escaped text (`&lt;`, `&quot;`, `&#x27;`) contains none of them.
    fn markup(html: &str) -> String {
        html.chars().filter(|c| "<>\"'".contains(*c)).collect()
    }

    #[test]
    fn hostile_chat_never_becomes_markup() {
        for hostile in HOSTILE {
            for (safe_kind, hostile_kind) in kinds("x").into_iter().zip(kinds(hostile)) {
                let expected = render(&filled_with("x", safe_kind));
                let html = render(&filled_with(hostile, hostile_kind));
                assert_eq!(
                    markup(&html),
                    markup(&expected),
                    "{hostile:?} changed the markup:\n{html}"
                );
                // Chat is data, never template code: it shows up as typed.
                if hostile.starts_with("{{") {
                    assert!(html.contains("{{ 7*7 }}"), "{html}");
                }
            }
        }
    }

    #[test]
    fn root_carries_classes_and_data_attributes() {
        let html = render(&message(ChatPlatform::YouTube, "hi", MessageKind::Text));
        assert!(
            html.contains(r#"class="msg msg--text msg--youtube""#),
            "{html}"
        );
        assert!(html.contains(r#"data-platform="youtube""#), "{html}");
        assert!(html.contains(r#"data-author="u1""#), "{html}");
        assert!(
            html.contains(r#"style="--author-color: #7f5af0""#),
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="badge badge--moderator">"#),
            "{html}"
        );
    }

    #[test]
    fn emotes_become_images() {
        let mut msg = message(ChatPlatform::Twitch, "hi Kappa", MessageKind::Text);
        msg.emotes = vec![EmoteRef {
            code: "Kappa".into(),
            // minijinja also escapes "/" (as &#x2f;) inside URLs; browsers
            // decode that, but it would make this assertion unreadable.
            url: "kappa.png".into(),
        }];
        let html = render(&msg);
        assert!(
            html.contains(r#"<p class="msg__body">hi <img class="emote" src="kappa.png" alt="Kappa" title="Kappa"></p>"#),
            "{html}"
        );
    }

    #[test]
    fn donation_shows_amount_and_is_paid() {
        let html = render(&message(
            ChatPlatform::YouTube,
            "gg",
            MessageKind::Donation {
                amount: "€5.00".into(),
            },
        ));
        assert!(html.contains("msg--donation"), "{html}");
        assert!(html.contains("msg--paid"), "{html}");
        assert!(
            html.contains(r#"<data class="msg__amount">€5.00</data>"#),
            "{html}"
        );
    }

    #[test]
    fn sticker_without_image_shows_amount_and_description() {
        let html = render(&message(
            ChatPlatform::YouTube,
            "",
            MessageKind::Special {
                image_url: None,
                amount: Some("€2.00".into()),
                info: Some("dancing cat".into()),
            },
        ));
        assert!(html.contains("msg--special"), "{html}");
        assert!(html.contains("msg--paid"), "{html}");
        assert!(
            html.contains(r#"<p class="msg__event"><data class="msg__amount">€2.00</data> <span class="msg__info">dancing cat</span></p>"#),
            "{html}"
        );
        assert!(!html.contains("msg__sticker"), "{html}");
    }

    #[test]
    fn gift_wording_follows_platform_and_count() {
        let gift = |platform, count| {
            render(&message(
                platform,
                "",
                MessageKind::MembershipGift { count },
            ))
        };
        assert!(gift(ChatPlatform::Twitch, 5).contains(r#"5</data> subs</p>"#));
        assert!(gift(ChatPlatform::YouTube, 1).contains(r#"1</data> membership</p>"#));
    }

    #[test]
    fn empty_text_has_no_body() {
        let html = render(&message(
            ChatPlatform::Twitch,
            "",
            MessageKind::SystemNotice {
                info: "Kim is raiding".into(),
            },
        ));
        assert!(!html.contains("msg__body"), "{html}");
        assert!(
            html.contains(r#"<p class="msg__event">Kim is raiding</p>"#),
            "{html}"
        );
    }

    #[test]
    fn invalid_color_is_dropped() {
        let mut msg = message(ChatPlatform::Twitch, "hi", MessageKind::Text);
        msg.author.color = Some("red; background: url(x)".into());
        assert!(!render(&msg).contains("--author-color"));
    }

    #[test]
    fn load_overrides_only_existing_files() {
        let dir = std::env::temp_dir().join(format!("chat-render-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MESSAGE_FILE), "<p>{{ author.name }}</p>").unwrap();

        let theme = Theme::load(&dir).unwrap();
        let msg = message(ChatPlatform::Twitch, "hi", MessageKind::Text);
        assert_eq!(theme.render(&msg).unwrap(), "<p>Ann</p>");
        assert_eq!(theme.css(), DEFAULT_CSS); // no overlay.css in dir

        std::fs::write(dir.join(MESSAGE_FILE), "{% if %}").unwrap();
        assert!(Theme::load(&dir).is_err(), "syntax errors surface at load");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
