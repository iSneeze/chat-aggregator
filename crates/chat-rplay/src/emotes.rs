//! RPlay's emotes (see RPLAY.md, "Emotes"): written as `:key:` in the
//! text, nothing else marks them. The images come from two lists, RPlay's
//! free set and the channel's own, both fetched without a login.

use std::collections::HashMap;

use chat_core::EmoteRef;
use serde_json::Value;

/// Where emote images live: `<IMAGES><s3Key>`, no login needed.
pub(crate) const IMAGES: &str = "https://pb3.rplay.live/";

/// The emotes known for one channel. Empty by default: then every `:key:`
/// stays text.
#[derive(Debug, Default)]
pub(crate) struct Emotes {
    /// By key, spelled as RPlay lists it (without the colons).
    by_key: HashMap<String, Emote>,
}

#[derive(Debug)]
struct Emote {
    url: String,
    /// Who may use it: 0 for everyone (the free set), else a tier level.
    use_level: i64,
}

/// The emotes of a message text.
#[derive(Debug)]
pub(crate) struct Body {
    /// Each spelling found, once, in order of appearance. The text keeps
    /// them as typed, so every code is exactly what's in the text.
    pub emotes: Vec<EmoteRef>,
    /// Nothing but emotes and whitespace.
    pub emote_only: bool,
}

impl Emotes {
    /// Reads RPlay's emote lists (the answers of `/emoticon/group/free`
    /// and `/emoticon/group/creator`, as they are). An entry that lacks
    /// something is skipped, not fatal: one odd emote shouldn't cost all
    /// the others.
    pub(crate) fn from_lists(lists: &[Value]) -> Self {
        let mut by_key: HashMap<String, Emote> = HashMap::new();
        let groups = lists
            .iter()
            .filter_map(|list| list.get("emoticonGroups")?.as_array())
            .flatten();
        for group in groups {
            let Some(emotes) = group.get("emoticons").and_then(Value::as_array) else {
                continue;
            };
            for emote in emotes {
                let Some((key, emote)) = read_emote(emote) else {
                    continue;
                };
                // Two groups with the same key: RPlay keeps the one with the
                // highest level the sender may use. Without knowing the
                // sender's tier (that needs a lookup per chatter, not done
                // yet), the highest level wins; on a tie, the later one, as
                // on RPlay's page.
                match by_key.get(&key) {
                    Some(known) if known.use_level > emote.use_level => {}
                    _ => {
                        by_key.insert(key, emote);
                    }
                }
            }
        }
        Self { by_key }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Finds the emotes in a message text the way RPlay's page does
    /// (`textToEmoticonHtml`): the text is split at every `:`, and each
    /// piece that is exactly a known key counts (so `a:key:b` too: no word
    /// borders). Then every `:key:` of those keys becomes an image,
    /// ignoring upper/lower case: once `:hypeWave:` is in a message,
    /// `:HYPEWAVE:` in it is an image too.
    ///
    /// Every emote is treated as usable by everyone: RPlay shows an emote
    /// of a tier the sender doesn't have as text, but which tiers a sender
    /// has needs a lookup that isn't done yet.
    pub(crate) fn find(&self, text: &str) -> Body {
        // In order of appearance, as RPlay's page goes through them: two
        // keys that differ only in case would both match the same `:key:`,
        // and then the first one found wins (a `HashSet` would pick one at
        // random).
        let mut found: Vec<&str> = Vec::new();
        for piece in text.split(':') {
            if self.by_key.contains_key(piece) && !found.contains(&piece) {
                found.push(piece);
            }
        }
        if found.is_empty() {
            return Body {
                emotes: Vec::new(),
                emote_only: false,
            };
        }

        let mut emotes: Vec<EmoteRef> = Vec::new();
        let mut gaps_blank = true;
        // Looked at up to this byte.
        let mut done = 0;
        let mut i = 0;
        // Left to right, each match taking its colons with it, the way the
        // overlay splits the text later: `:a:b:` is the emote `:a:`, then
        // the text `b:`. ':' is ASCII, so `i` stays on a char boundary.
        while let Some(offset) = text[i..].find(':') {
            let start = i + offset;
            let matched = found.iter().find_map(|key| {
                let len = caseless_code_at(&text[start..], key)?;
                Some((len, &self.by_key[*key]))
            });
            let Some((len, emote)) = matched else {
                i = start + 1;
                continue;
            };
            gaps_blank &= text[done..start].trim().is_empty();
            let code = &text[start..start + len];
            if !emotes.iter().any(|known| known.code == code) {
                emotes.push(EmoteRef {
                    code: code.to_owned(),
                    url: emote.url.clone(),
                });
            }
            done = start + len;
            i = done;
        }
        gaps_blank &= text[done..].trim().is_empty();
        Body {
            emote_only: gaps_blank,
            emotes,
        }
    }
}

/// How many emotes one of RPlay's lists has, and how many of them are for a
/// tier (`useLevel` above 0): for the log.
pub(crate) fn count(list: &Value) -> (usize, usize) {
    let emotes: Vec<&Value> = list
        .get("emoticonGroups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("emoticons")?.as_array())
        .flatten()
        .collect();
    let for_a_tier = emotes
        .iter()
        .filter(|emote| emote.get("useLevel").and_then(Value::as_f64).unwrap_or(0.0) > 0.0)
        .count();
    (emotes.len(), for_a_tier)
}

/// One emote of a list: its key and what it stands for.
fn read_emote(emote: &Value) -> Option<(String, Emote)> {
    let key = emote.get("key")?.as_str()?;
    let s3_key = emote.get("s3Key")?.as_str()?;
    // A key with a colon could never be matched (the text is split at
    // them); a path with characters outside these would need escaping in
    // a URL, and none was seen: skipped rather than guessed at.
    let path_ok = !s3_key.is_empty()
        && s3_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b));
    if key.is_empty() || key.contains(':') || !path_ok {
        return None;
    }
    let use_level = emote
        .get("useLevel")
        .and_then(Value::as_f64)
        .map_or(0, |level| level as i64);
    Some((
        key.to_owned(),
        Emote {
            url: format!("{IMAGES}{s3_key}"),
            use_level,
        },
    ))
}

/// If `text` starts with `:key:` (case ignored), how many bytes that is in
/// `text`.
fn caseless_code_at(text: &str, key: &str) -> Option<usize> {
    let rest = text.strip_prefix(':')?;
    let mut chars = rest.char_indices();
    for wanted in key.chars() {
        let (_, c) = chars.next()?;
        if c != wanted && !c.to_lowercase().eq(wanted.to_lowercase()) {
            return None;
        }
    }
    let (end, c) = chars.next()?;
    (c == ':').then_some(1 + end + 1)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Lists in the shape of RPlay's answers (made-up keys and paths).
    fn emotes() -> Emotes {
        let free = json!({
            "type": "free",
            "emoticonGroups": [{
                "tierHash": "free",
                "useLevel": 0,
                "emoticons": [
                    { "key": "_hypeWave", "s3Key": "emoticon/free/hypeWave.gif", "type": "image/gif", "useLevel": 0 },
                    { "key": "_cheer", "s3Key": "emoticon/free/cheer.png", "type": "image/png", "useLevel": 0 },
                ],
            }],
        });
        let creator = json!({
            "type": "creator",
            "creatorOid": "0123456789abcdef01234567",
            "emoticonGroups": [{
                "tierHash": "aabbcc",
                "useLevel": 2,
                "emoticons": [
                    { "key": "yourHeart", "s3Key": "emoticon/0123456789abcdef01234567/heart.png", "useLevel": 2 },
                    // The same key as a free one, at a higher level.
                    { "key": "_cheer", "s3Key": "emoticon/0123456789abcdef01234567/cheer.png", "useLevel": 2 },
                    // Odd entries: skipped.
                    { "key": "noImage" },
                    { "key": "a:b", "s3Key": "emoticon/x.png" },
                    { "key": "spaces", "s3Key": "emoticon/has space.png" },
                    "not an object",
                ],
            }],
        });
        Emotes::from_lists(&[free, creator, json!({ "emoticonGroups": null })])
    }

    fn codes(body: &Body) -> Vec<&str> {
        body.emotes.iter().map(|e| e.code.as_str()).collect()
    }

    #[test]
    fn lists_are_read_with_higher_level_winning() {
        let emotes = emotes();
        assert_eq!(emotes.len(), 3);
        // The log's counts, per list as fetched: odd entries included.
        let creator = json!({ "emoticonGroups": [{ "emoticons": [
            { "key": "a", "useLevel": 2 }, { "key": "b", "useLevel": 0 }, { "key": "c" }, "odd",
        ] }] });
        assert_eq!(count(&creator), (4, 1));
        assert_eq!(count(&json!({ "emoticonGroups": null })), (0, 0));
        let body = emotes.find(":_hypeWave: :_cheer:");
        assert_eq!(codes(&body), [":_hypeWave:", ":_cheer:"]);
        assert_eq!(
            body.emotes[0].url,
            "https://pb3.rplay.live/emoticon/free/hypeWave.gif"
        );
        assert_eq!(
            body.emotes[1].url,
            "https://pb3.rplay.live/emoticon/0123456789abcdef01234567/cheer.png"
        );
        assert!(body.emote_only);
    }

    #[test]
    fn emotes_in_text() {
        let emotes = emotes();
        let body = emotes.find("hi :_hypeWave: and :yourHeart: :_hypeWave:");
        assert_eq!(codes(&body), [":_hypeWave:", ":yourHeart:"]);
        assert!(!body.emote_only);
        // Two emotes and spaces only.
        assert!(emotes.find(" :_hypeWave: :yourHeart: ").emote_only);
        // No word borders, as on RPlay's page.
        assert_eq!(codes(&emotes.find("a:_hypeWave:b")), [":_hypeWave:"]);
        // A match consumes its colons (`:_cheer:` uses the colon `:_hypeWave:` would need).
        let touching = emotes.find(":_cheer:_hypeWave:");
        assert_eq!(codes(&touching), [":_cheer:"]);
        assert!(!touching.emote_only, "`_hypeWave:` is left as text");
    }

    #[test]
    fn case_only_matters_for_the_first_spelling() {
        let emotes = emotes();
        // Alone, a differently spelled key is no emote...
        assert!(emotes.find(":_HYPEWAVE:").emotes.is_empty());
        // ...but once the real spelling is in the message, it is one too.
        let body = emotes.find(":_hypeWave: :_HYPEWAVE:");
        assert_eq!(codes(&body), [":_hypeWave:", ":_HYPEWAVE:"]);
        assert!(body.emote_only);
    }
}
