//! The template's view of a message: exactly the variables documented at
//! the top of `message.html`. Kept separate from `ChatMessage` so the core
//! model can change without breaking people's custom templates.
//!
//! Every field borrows from the message (`&'a str`) instead of cloning: the
//! view only lives for the duration of one render call.

use chat_core::{ChatMessage, EmoteRef, MessageKind};

#[derive(serde::Serialize)]
pub(crate) struct MessageView<'a> {
    id: &'a str,
    platform: &'static str,
    kind: &'static str,
    paid: bool,
    author: AuthorView<'a>,
    timestamp: String,
    time: String,
    body: Vec<Part<'a>>,
    amount: Option<&'a str>,
    count: Option<usize>,
    info: Option<&'a str>,
    sticker_url: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct AuthorView<'a> {
    id: &'a str,
    name: &'a str,
    color: Option<&'a str>,
    badges: &'a [String],
    avatar_url: Option<&'a str>,
}

/// One piece of the message body. Serialized with a `type` field, so the
/// template can check `part.type == "emote"`.
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum Part<'a> {
    Text { text: &'a str },
    Emote { code: &'a str, url: &'a str },
}

impl<'a> MessageView<'a> {
    pub(crate) fn new(msg: &'a ChatMessage) -> Self {
        let mut view = MessageView {
            id: &msg.id,
            platform: msg.platform.as_str(),
            kind: "text",
            paid: false,
            author: AuthorView {
                id: &msg.author.id,
                name: &msg.author.name,
                color: msg.author.color.as_deref().filter(|c| is_hex_color(c)),
                badges: &msg.author.badges,
                avatar_url: msg.author.avatar_url.as_deref(),
            },
            timestamp: msg.timestamp.to_rfc3339(),
            time: msg
                .timestamp
                .with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string(),
            body: split_body(&msg.text, &msg.emotes),
            amount: None,
            count: None,
            info: None,
            sticker_url: None,
        };

        match &msg.kind {
            MessageKind::Text => {}
            MessageKind::EmoteOnly => view.kind = "emote-only",
            MessageKind::Donation { amount } => {
                view.kind = "donation";
                view.paid = true;
                view.amount = Some(amount);
            }
            MessageKind::Special {
                image_url,
                amount,
                info,
            } => {
                view.kind = "special";
                view.paid = true;
                view.sticker_url = image_url.as_deref();
                view.amount = amount.as_deref();
                view.info = info.as_deref();
            }
            MessageKind::MembershipJoin { info } => {
                view.kind = "membership";
                view.info = Some(info);
            }
            MessageKind::MembershipGift { count } => {
                view.kind = "gift";
                view.count = Some(*count);
            }
            MessageKind::SystemNotice { info } => {
                view.kind = "notice";
                view.info = Some(info);
            }
        }
        view
    }
}

/// The color ends up inside a `style` attribute. Escaping already stops it
/// from breaking out of the attribute, but only a plain `#rgb`/`#rrggbb`
/// value is allowed, so it can't inject other CSS properties either.
fn is_hex_color(c: &str) -> bool {
    c.strip_prefix('#').is_some_and(|hex| {
        matches!(hex.len(), 3 | 6) && hex.chars().all(|ch| ch.is_ascii_hexdigit())
    })
}

/// Splits `text` into plain-text and emote parts. The template writes the
/// `<img>` for emote parts; text parts get HTML-escaped. Chat text therefore
/// never reaches the page as raw HTML.
pub(crate) fn split_body<'a>(text: &'a str, emotes: &'a [EmoteRef]) -> Vec<Part<'a>> {
    if emotes.is_empty() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![Part::Text { text }]
        };
    }

    let mut parts = Vec::new();
    let mut plain_start = 0; // start of the current run of plain text
    let mut i = 0;
    while i < text.len() {
        if let Some(emote) = emote_at(text, i, emotes) {
            if plain_start < i {
                parts.push(Part::Text {
                    text: &text[plain_start..i],
                });
            }
            parts.push(Part::Emote {
                code: &emote.code,
                url: &emote.url,
            });
            i += emote.code.len();
            plain_start = i;
        } else {
            // Step one *character*, not one byte: `i` must stay on a char
            // boundary or the slices above would panic on e.g. emoji.
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if plain_start < text.len() {
        parts.push(Part::Text {
            text: &text[plain_start..],
        });
    }
    parts
}

/// The longest emote whose code starts at byte `i` and may match there.
fn emote_at<'e>(text: &str, i: usize, emotes: &'e [EmoteRef]) -> Option<&'e EmoteRef> {
    let rest = &text[i..];
    emotes
        .iter()
        .filter(|e| !e.code.is_empty() && rest.starts_with(e.code.as_str()))
        .filter(|e| is_token_code(&e.code) || is_whole_word(text, i, i + e.code.len()))
        .max_by_key(|e| e.code.len())
}

/// YouTube-style `:code:` tokens are self-delimiting and may touch each
/// other (`:yt::yt:`), so they match anywhere.
fn is_token_code(code: &str) -> bool {
    code.len() > 2 && code.starts_with(':') && code.ends_with(':')
}

/// Word codes (Twitch's `Kappa`) only match as whole words: `Kappachino`
/// must stay text.
fn is_whole_word(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emote(code: &str) -> EmoteRef {
        EmoteRef {
            code: code.into(),
            url: format!("https://img/{code}"),
        }
    }

    fn text(t: &str) -> Part<'_> {
        Part::Text { text: t }
    }

    #[test]
    fn plain_text_is_one_part() {
        assert_eq!(split_body("hello", &[]), vec![text("hello")]);
        assert!(split_body("", &[]).is_empty());
    }

    #[test]
    fn word_emotes_match_whole_words_only() {
        let emotes = [emote("Kappa")];
        let parts = split_body("Kappa Kappachino xKappa Kappa", &emotes);
        assert_eq!(
            parts,
            vec![
                Part::Emote {
                    code: "Kappa",
                    url: "https://img/Kappa"
                },
                text(" Kappachino xKappa "),
                Part::Emote {
                    code: "Kappa",
                    url: "https://img/Kappa"
                },
            ]
        );
    }

    #[test]
    fn token_emotes_may_touch() {
        let emotes = [emote(":yt:"), emote(":_hype:")];
        let parts = split_body("a:yt::_hype:b", &emotes);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], text("a"));
        assert!(matches!(parts[1], Part::Emote { code: ":yt:", .. }));
        assert!(matches!(
            parts[2],
            Part::Emote {
                code: ":_hype:",
                ..
            }
        ));
        assert_eq!(parts[3], text("b"));
    }

    #[test]
    fn longest_code_wins() {
        let emotes = [emote("Kappa"), emote("KappaPride")];
        let parts = split_body("KappaPride", &emotes);
        assert!(matches!(
            parts[..],
            [Part::Emote {
                code: "KappaPride",
                ..
            }]
        ));
    }

    #[test]
    fn multibyte_text_does_not_panic() {
        let emotes = [emote("Kappa")];
        let parts = split_body("日本語 😂 Kappa", &emotes);
        assert_eq!(parts[0], text("日本語 😂 "));
        assert!(matches!(parts[1], Part::Emote { .. }));
    }

    #[test]
    fn only_plain_hex_colors_pass() {
        assert!(is_hex_color("#7f5af0"));
        assert!(is_hex_color("#FFF"));
        assert!(!is_hex_color("7f5af0"));
        assert!(!is_hex_color("#7f5af0; background: red"));
        assert!(!is_hex_color("red"));
    }
}
