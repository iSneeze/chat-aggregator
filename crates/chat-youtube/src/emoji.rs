//! Custom YouTube emoji, loaded from the JSON export of
//! `scripts/yt-emoji-export.js`.
//!
//! The Data API only gives us the message text, where custom emoji appear
//! as `:code:` tokens with no image URL. The export maps those codes to
//! image URLs; unicode emoji need no mapping (they arrive as characters).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use anyhow::Context;
use chat_core::EmoteRef;

/// `:code:` → image URL. Empty by default: messages then simply carry no
/// emotes and render their tokens as plain text.
#[derive(Debug, Default, Clone)]
pub struct EmojiMap {
    by_code: HashMap<String, String>,
}

/// The fields of the export file we use; serde ignores the rest.
#[derive(serde::Deserialize)]
struct ExportFile {
    version: u32,
    entries: Vec<ExportEntry>,
}

#[derive(serde::Deserialize)]
struct ExportEntry {
    code: String,
    url: String,
}

/// Result of scanning one message's text.
pub(crate) struct EmojiMatch {
    /// Unique emotes in first-occurrence order (same contract as Twitch).
    pub emotes: Vec<EmoteRef>,
    /// Text consists only of known emoji tokens and whitespace.
    pub emote_only: bool,
}

impl EmojiMap {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("reading emoji export {}", path.display()))?;
        Self::from_json(&json).with_context(|| format!("parsing emoji export {}", path.display()))
    }

    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let file: ExportFile = serde_json::from_str(json)?;
        anyhow::ensure!(
            file.version == 1,
            "unsupported emoji export version {}",
            file.version
        );
        let by_code = file.entries.into_iter().map(|e| (e.code, e.url)).collect();
        Ok(Self { by_code })
    }

    pub fn len(&self) -> usize {
        self.by_code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_code.is_empty()
    }

    pub(crate) fn find(&self, text: &str) -> EmojiMatch {
        let tokens = self.tokens(text);

        // Emote-only: every gap between tokens is whitespace.
        let mut last = 0;
        let mut gaps_blank = true;
        for (range, _, _) in &tokens {
            gaps_blank &= text[last..range.start].trim().is_empty();
            last = range.end;
        }
        gaps_blank &= text[last..].trim().is_empty();

        let mut seen = HashSet::new();
        let emotes = tokens
            .iter()
            .filter(|(_, code, _)| seen.insert(*code))
            .map(|(_, code, url)| EmoteRef {
                code: code.to_string(),
                url: url.to_string(),
            })
            .collect();

        EmojiMatch {
            emotes,
            emote_only: !tokens.is_empty() && gaps_blank,
        }
    }

    /// All known `:code:` tokens in `text`, left to right, as
    /// (byte range, code, url).
    ///
    /// Walks the colons pairwise: if the text between an opening colon and
    /// the next one is a known code, that's a token; otherwise the second
    /// colon becomes the new opener. So `10:30 :yt:` finds `:yt:` even
    /// though `:30 :` came first.
    fn tokens<'m>(&'m self, text: &str) -> Vec<(Range<usize>, &'m str, &'m str)> {
        let mut found = Vec::new();
        let mut open: Option<usize> = None;
        // ':' is ASCII, so these byte indices are always char boundaries
        // and slicing `text` with them cannot panic.
        for (i, _) in text.match_indices(':') {
            if let Some(start) = open
                && let Some((code, url)) = self.by_code.get_key_value(&text[start..=i])
            {
                found.push((start..i + 1, code.as_str(), url.as_str()));
                open = None;
            } else {
                open = Some(i);
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> EmojiMap {
        EmojiMap::from_json(
            r#"{
                "version": 1,
                "source": "youtube-emoji-picker",
                "entries": [
                    { "code": ":yt:", "url": "https://yt3.ggpht.com/yt", "category": "YouTube" },
                    { "code": ":_hype:", "url": "https://yt3.ggpht.com/hype", "channel_id": "UC1" }
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn loads_export_format() {
        assert_eq!(map().len(), 2);
    }

    #[test]
    fn rejects_unknown_version() {
        assert!(EmojiMap::from_json(r#"{ "version": 2, "entries": [] }"#).is_err());
    }

    #[test]
    fn finds_tokens_unique_in_order() {
        let m = map().find(":_hype: hello :yt: :_hype:");
        let codes: Vec<_> = m.emotes.iter().map(|e| e.code.as_str()).collect();
        assert_eq!(codes, [":_hype:", ":yt:"]);
        assert!(!m.emote_only);
    }

    #[test]
    fn stray_colons_do_not_hide_tokens() {
        let m = map().find("at 10:30 :yt: see you");
        assert_eq!(m.emotes.len(), 1);
        assert_eq!(m.emotes[0].code, ":yt:");
    }

    #[test]
    fn unknown_tokens_are_ignored() {
        let m = map().find(":nope: :yt:");
        assert_eq!(m.emotes.len(), 1);
        assert!(!m.emote_only); // ":nope:" is visible text
    }

    #[test]
    fn emote_only_detected() {
        assert!(map().find(" :yt::_hype:  :yt: ").emote_only);
        assert!(!map().find("").emote_only);
        assert!(!map().find("plain text").emote_only);
    }

    #[test]
    fn empty_map_finds_nothing() {
        let m = EmojiMap::default().find(":yt:");
        assert!(m.emotes.is_empty());
        assert!(!m.emote_only);
    }
}
