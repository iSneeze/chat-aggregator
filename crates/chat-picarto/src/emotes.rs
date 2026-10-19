//! Emotes in Picarto chat messages.
//!
//! A message's text holds every emote as a `:code:` token. Only channel
//! emotes come with an image: the message's `e` list. The website knows
//! the rest from tables in its own JavaScript bundle, which [`SiteEmotes`]
//! reads at runtime (see PICARTO.md, "Emotes and badges"):
//!
//! - Picarto's own emotes, `:ptv-<name>:`: 34 images, each stored in the
//!   bundle as a `data:` URI. Picarto hosts them nowhere else.
//! - Emoji names (`:rofl:`, `:e-mail:`): about 1,800 names of unicode
//!   emoji, which we turn into the emoji characters themselves.
//!
//! Reading them from the site rather than shipping copies keeps Picarto's
//! artwork out of our repo and picks up new emotes by itself.

use std::collections::{HashMap, HashSet};

use anyhow::Context;
use chat_core::EmoteRef;

const SITE: &str = "https://picarto.tv";
/// Where relative image paths (channel emotes, avatars) live.
const IMAGES: &str = "https://images.picarto.tv/";
/// The bundle had 1,820 emoji names (2026-10-16). Far fewer means the
/// format changed and we'd be reading something else: better none.
const MIN_EMOJI: usize = 1000;
/// The only kinds of `data:` URI we pass on as an emote image.
const IMAGE_DATA: &[&str] = &[
    "data:image/png;base64,",
    "data:image/gif;base64,",
    "data:image/jpeg;base64,",
    "data:image/webp;base64,",
];

/// The emote tables of Picarto's website. Empty by default: then only
/// channel emotes are found, and other codes stay plain text.
#[derive(Debug, Default, Clone)]
pub struct SiteEmotes {
    /// Lowercased name after `ptv-` → `data:` URI.
    ptv: HashMap<String, String>,
    /// Lowercased emoji name → the emoji's characters.
    emoji: HashMap<String, String>,
}

/// One entry of a message's `e` list.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChannelEmote {
    /// The code without colons, e.g. `cyour_channel-hypeWave`.
    pub a: String,
    /// Image path, relative to `images.picarto.tv` (or a full URL).
    pub u: String,
    /// Set when the streamer replaced the image: the site appends it as
    /// `?version=` so browsers don't show the old one from their cache.
    #[serde(default)]
    pub d: Option<serde_json::Value>,
}

/// A message text with its emotes resolved.
#[derive(Debug)]
pub struct Body {
    /// The text, emoji names replaced by the emoji. Emote codes stay as
    /// they were typed: each one is an [`EmoteRef::code`].
    pub text: String,
    /// Emotes with an image, each listed once, in order of appearance.
    pub emotes: Vec<EmoteRef>,
    /// Nothing but image emotes and whitespace. Emoji count as text, like
    /// unicode emoji from the other platforms.
    pub emote_only: bool,
}

impl SiteEmotes {
    /// Downloads the website's main bundle and reads the tables from it.
    /// About 1.4 MB compressed: do it once per source, not per reconnect.
    pub async fn fetch(http: &reqwest::Client) -> anyhow::Result<Self> {
        let page = get_text(http, &format!("{SITE}/"))
            .await
            .context("loading picarto.tv")?;
        let path = bundle_path(&page).context("no main script found on picarto.tv")?;
        let js = get_text(http, &format!("{SITE}{path}"))
            .await
            .context("loading picarto.tv's main script")?;
        Ok(Self::from_bundle(&js))
    }

    /// Reads both tables from the bundle's source. A table that can't be
    /// read stays empty (with a warning): its codes then stay text.
    pub fn from_bundle(js: &str) -> Self {
        let ptv = parse_ptv(js);
        if ptv.is_empty() {
            tracing::warn!("no Picarto emotes found in picarto.tv's script: its format changed");
        }
        let mut emoji = parse_emoji(js);
        if emoji.len() < MIN_EMOJI {
            tracing::warn!(
                found = emoji.len(),
                "too few emoji names in picarto.tv's script: its format changed"
            );
            emoji.clear();
        }
        tracing::debug!(ptv = ptv.len(), emoji = emoji.len(), "Picarto emote tables");
        Self { ptv, emoji }
    }

    /// How many `ptv-` emotes are known.
    pub fn ptv_len(&self) -> usize {
        self.ptv.len()
    }

    /// How many emoji names are known.
    pub fn emoji_len(&self) -> usize {
        self.emoji.len()
    }

    /// Resolves the `:code:` tokens of a message text. `channel` is the
    /// message's `e` list; Picarto puts `null` in it for codes it couldn't
    /// resolve, hence the `Option`s.
    ///
    /// Each code is looked up in turn, first match wins:
    ///
    /// 1. `ptv-` + one of Picarto's own emotes,
    /// 2. the message's channel emotes,
    /// 3. the emoji names,
    /// 4. otherwise it stays text.
    ///
    /// The website instead picks *one* table by the code's shape (with
    /// `ptv-`: Picarto's; with another `-`: the channel's; without: emoji),
    /// so it shows the three emoji names with a `-` (`:e-mail:`) as text.
    /// The order above gives the same result for every code the site
    /// resolves, and also finds those three. Like the site, matching
    /// ignores upper/lower case.
    pub fn convert(&self, text: &str, channel: &[Option<ChannelEmote>]) -> Body {
        let mut out = String::with_capacity(text.len());
        let mut emotes = Vec::new();
        let mut seen = HashSet::new();
        let mut any_emoji = false;
        let mut gaps_blank = true;
        // Copied to `out` up to here.
        let mut done = 0;
        let mut open: Option<usize> = None;
        // Walks the colons pairwise, as chat-youtube's emoji map does: if
        // the text between an opening colon and the next one is a known
        // code, that's a token; otherwise the second colon becomes the new
        // opener. So `10:30 :rofl:` still finds `:rofl:`. ':' is ASCII, so
        // these byte indices are char boundaries and slicing can't panic.
        for (i, _) in text.match_indices(':') {
            let Some(start) = open else {
                open = Some(i);
                continue;
            };
            let token = &text[start..=i];
            let Some(found) = self.lookup(&token[1..token.len() - 1], channel) else {
                open = Some(i);
                continue;
            };
            let gap = &text[done..start];
            gaps_blank &= gap.trim().is_empty();
            out.push_str(gap);
            match found {
                Found::Image(url) => {
                    out.push_str(token);
                    if seen.insert(token) {
                        emotes.push(EmoteRef {
                            code: token.to_owned(),
                            url,
                        });
                    }
                }
                Found::Emoji(chars) => {
                    out.push_str(chars);
                    any_emoji = true;
                }
            }
            done = i + 1;
            open = None;
        }
        let rest = &text[done..];
        gaps_blank &= rest.trim().is_empty();
        out.push_str(rest);

        Body {
            text: out,
            emote_only: !emotes.is_empty() && !any_emoji && gaps_blank,
            emotes,
        }
    }

    /// What a code (without colons) stands for, if anything.
    fn lookup(&self, code: &str, channel: &[Option<ChannelEmote>]) -> Option<Found<'_>> {
        if code.is_empty() {
            return None;
        }
        let code = code.to_lowercase();
        // Before the channel's: an `a` without a dash would match
        // `ptv-<a>` there too, and the site checks these first.
        if let Some(url) = code
            .strip_prefix("ptv-")
            .and_then(|name| self.ptv.get(name))
        {
            return Some(Found::Image(url.clone()));
        }
        if let Some(emote) = channel_emote(&code, channel) {
            return Some(Found::Image(channel_url(emote)));
        }
        self.emoji.get(&code).map(|chars| Found::Emoji(chars))
    }
}

enum Found<'a> {
    Image(String),
    Emoji(&'a str),
}

/// The site's rule: an `a` with a `-` is the whole code; an `a` without
/// one is the part after the code's first `-` (both lowercased).
fn channel_emote<'e>(code: &str, channel: &'e [Option<ChannelEmote>]) -> Option<&'e ChannelEmote> {
    let after_dash = code.split_once('-').map(|(_, rest)| rest);
    channel.iter().flatten().find(|emote| {
        let a = emote.a.to_lowercase();
        if a.contains('-') {
            a == code
        } else {
            after_dash == Some(a.as_str())
        }
    })
}

fn channel_url(emote: &ChannelEmote) -> String {
    let url = image_url(&emote.u);
    // Only plain versions (numbers, simple strings): it goes into a URL.
    let version = match &emote.d {
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::String(s))
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') =>
        {
            Some(s.clone())
        }
        _ => None,
    };
    match version {
        Some(version) => format!("{url}?version={version}"),
        None => url,
    }
}

/// A Picarto image path as a full URL. Paths are usually relative to
/// `images.picarto.tv`, sometimes already full URLs.
fn image_url(path: &str) -> String {
    if path.starts_with("https://") || path.starts_with("http://") {
        path.to_owned()
    } else {
        format!("{IMAGES}{}", path.trim_start_matches('/'))
    }
}

async fn get_text(http: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    Ok(http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// The main bundle's path in the site's HTML: `/static/js/main.<hash>.js`.
/// The hash changes with every deploy of the site.
fn bundle_path(html: &str) -> Option<&str> {
    let start = html.find("/static/js/main.")?;
    let rest = &html[start..];
    let path = &rest[..rest.find('"')?];
    let plain = path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b));
    (plain && path.ends_with(".js")).then_some(path)
}

/// The `ptv-` emotes. The bundle lists them as
/// `{name:"shipit",filename:"shipit.png",emoticon:t(21057)}`, where
/// `t(21057)` loads webpack module 21057, which is just
/// `21057:function(e){"use strict";e.exports="data:image/png;base64,…"}`.
/// The minifier renames `t` and `e` and webpack renumbers modules on every
/// deploy, so only the shape is relied on.
fn parse_ptv(js: &str) -> HashMap<String, String> {
    let mut found = HashMap::new();
    for (start, _) in js.match_indices("name:\"") {
        let entry = &js[start + "name:\"".len()..];
        let parsed = (|| {
            let (name, rest) = entry.split_once('"')?;
            let rest = rest.strip_prefix(",filename:\"")?;
            let (_filename, rest) = rest.split_once('"')?;
            let rest = rest.strip_prefix(",emoticon:")?;
            let rest = rest
                .trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$');
            let rest = rest.strip_prefix('(')?;
            let (id, _) = rest.split_once(')')?;
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((name, module_data_uri(js, id)?))
        })();
        if let Some((name, url)) = parsed {
            found.insert(name.to_lowercase(), url.to_owned());
        }
    }
    found
}

/// The image `data:` URI that webpack module `id` exports, if it's one.
fn module_data_uri<'j>(js: &'j str, id: &str) -> Option<&'j str> {
    let key = format!("{id}:function(");
    js.match_indices(&key).find_map(|(start, _)| {
        // A module key follows `{` or `,`; anything else is a different
        // number that happens to end the same way (`121057:function(`).
        if !matches!(js[..start].bytes().last(), Some(b'{' | b',')) {
            return None;
        }
        // `(e){"use strict";e.exports="`: short, but leave some room.
        let head = &js[start + key.len()..];
        let head = &head[..head.len().min(80)];
        let at = head.find(".exports=\"")? + ".exports=\"".len();
        let value = &js[start + key.len() + at..];
        let uri = &value[..value.find('"')?];
        let data = IMAGE_DATA
            .iter()
            .find_map(|prefix| uri.strip_prefix(prefix))?;
        let base64 = !data.is_empty()
            && data
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
        base64.then_some(uri)
    })
}

/// The emoji names. The bundle has one object with an entry per name:
/// `rofl:{unicode:"1f923",unicode_alt:"",code_decimal:…}`, names that
/// aren't plain identifiers quoted (`"e-mail":{…}`). Both fields are code
/// points in hex, joined by `-`. `unicode_alt`, when set, is the complete
/// sequence: with the joiners (`200d`) that make a family one emoji
/// instead of four people, and `fe0f`, which asks for the colourful look.
fn parse_emoji(js: &str) -> HashMap<String, String> {
    const MARK: &str = ":{unicode:\"";
    let mut found = HashMap::new();
    for (at, _) in js.match_indices(MARK) {
        let Some(name) = object_key(&js[..at]) else {
            continue;
        };
        let parsed = (|| {
            let (unicode, rest) = js[at + MARK.len()..].split_once('"')?;
            let rest = rest.strip_prefix(",unicode_alt:\"")?;
            let (alt, _) = rest.split_once('"')?;
            emoji_chars(if alt.is_empty() { unicode } else { alt })
        })();
        if let Some(chars) = parsed {
            found.insert(name.to_lowercase(), chars);
        }
    }
    found
}

/// The object key that `before` ends with: `"e-mail"` or `rofl`, preceded
/// by `{` or `,`.
fn object_key(before: &str) -> Option<&str> {
    let (key, rest) = if let Some(quoted) = before.strip_suffix('"') {
        let open = quoted.rfind('"')?;
        (&quoted[open + 1..], &quoted[..open])
    } else {
        let start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
            .map_or(0, |i| i + 1);
        (&before[start..], &before[..start])
    };
    let after_brace_or_comma = matches!(rest.bytes().last(), Some(b'{' | b','));
    (!key.is_empty() && after_brace_or_comma).then_some(key)
}

/// `1f468-200d-1f469` → the characters.
fn emoji_chars(hex: &str) -> Option<String> {
    hex.split('-')
        .map(|part| char::from_u32(u32::from_str_radix(part, 16).ok()?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "data:image/png;base64,iVBORw0KGgo=";
    const GIF: &str = "data:image/gif;base64,R0lGODlh";

    /// A few pieces in the shape of the real bundle (minified webpack).
    fn bundle() -> String {
        [
            r#"var x={"use strict";t.d(n,{cn:function(){return a}});var a=[{name:"shipit",filename:"shipit.png",emoticon:t(21057)},"#,
            r#"{name:"uneasy",filename:"uneasy_anim.gif",emoticon:t(85504)},"#,
            r#"{name:"SteelwormHades",filename:"steelwormhades.png",emoticon:t(15512)},"#,
            // Points at a module that isn't an image: skipped.
            r#"{name:"broken",filename:"broken.png",emoticon:t(777)},"#,
            // Points at a module that doesn't exist: skipped.
            r#"{name:"missing",filename:"missing.png",emoticon:t(4242)}]},"#,
            &format!(r#"21057:function(e){{"use strict";e.exports="{PNG}"}},"#),
            &format!(r#"85504:function(e){{"use strict";e.exports="{GIF}"}},"#),
            // Another module whose number ends in 15512: must not be taken.
            r#"915512:function(e){"use strict";e.exports="data:image/png;base64,WRONG"},"#,
            &format!(r#"15512:function(e){{"use strict";e.exports="{PNG}"}},"#),
            r#"777:function(e){"use strict";e.exports="javascript:alert(1)"},"#,
            r#"99343:function(e,n){"use strict";n.Z={grinning:{unicode:"1f600",unicode_alt:"",code_decimal:"&#128512;",name:"grinning face",shortname:":grinning:"},"#,
            r#"rofl:{unicode:"1f923",unicode_alt:"",code_decimal:"&#129315;",name:"rolling on the floor laughing",shortname:":rofl:"},"#,
            r#""e-mail":{unicode:"1f4e7",unicode_alt:"",code_decimal:"&#128231;",name:"e-mail symbol",shortname:":e-mail:"},"#,
            r#"family_mmgb:{unicode:"1f468-1f468-1f467-1f466",unicode_alt:"1f468-200d-1f468-200d-1f467-200d-1f466",code_decimal:"",name:"family",shortname:":family_mmgb:"},"#,
            r#"bad:{unicode:"zzzz",unicode_alt:"",code_decimal:"",name:"bad",shortname:":bad:"}}}"#,
        ]
        .concat()
    }

    fn site() -> SiteEmotes {
        SiteEmotes {
            ptv: parse_ptv(&bundle()),
            emoji: parse_emoji(&bundle()),
        }
    }

    fn channel_emote(a: &str, u: &str) -> Option<ChannelEmote> {
        Some(ChannelEmote {
            a: a.to_owned(),
            u: u.to_owned(),
            d: None,
        })
    }

    fn codes(body: &Body) -> Vec<&str> {
        body.emotes.iter().map(|e| e.code.as_str()).collect()
    }

    #[test]
    fn finds_bundle_in_page() {
        let html = r#"<script defer="defer" src="/static/js/main.d543fe27.js"></script>"#;
        assert_eq!(bundle_path(html), Some("/static/js/main.d543fe27.js"));
        assert_eq!(bundle_path("<html></html>"), None);
        assert_eq!(bundle_path(r#"src="/static/js/main.x.js?<b>""#), None);
    }

    #[test]
    fn reads_ptv_emotes_from_their_modules() {
        let ptv = parse_ptv(&bundle());
        let mut names: Vec<_> = ptv.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["shipit", "steelwormhades", "uneasy"]);
        assert_eq!(ptv["uneasy"], GIF);
        // The module with exactly this number, not one ending in it.
        assert_eq!(ptv["steelwormhades"], PNG);
    }

    #[test]
    fn reads_emoji_names() {
        let emoji = parse_emoji(&bundle());
        assert_eq!(emoji["rofl"], "🤣");
        assert_eq!(emoji["e-mail"], "📧");
        assert!(!emoji.contains_key("bad"));
        assert_eq!(emoji.len(), 4);
    }

    #[test]
    fn joined_emoji_keep_their_joiners() {
        let emoji = parse_emoji(&bundle());
        assert_eq!(emoji["family_mmgb"], "👨\u{200d}👨\u{200d}👧\u{200d}👦");
    }

    #[test]
    fn too_few_emoji_means_none() {
        // The test bundle's 4 names are below MIN_EMOJI: a changed format.
        let site = SiteEmotes::from_bundle(&bundle());
        assert_eq!(site.emoji_len(), 0);
        assert_eq!(site.ptv_len(), 3);
    }

    #[test]
    fn ptv_emotes_become_images() {
        let body = site().convert(":PTV-ShipIt: :ptv-victory::ptv-uneasy:", &[]);
        // Codes keep their spelling; unknown ones stay text.
        assert_eq!(body.text, ":PTV-ShipIt: :ptv-victory::ptv-uneasy:");
        assert_eq!(codes(&body), [":PTV-ShipIt:", ":ptv-uneasy:"]);
        assert_eq!(body.emotes[0].url, PNG);
        assert_eq!(body.emotes[1].url, GIF);
    }

    #[test]
    fn emoji_names_become_emoji() {
        let body = site().convert("hi :rofl::Grinning: :nope: :e-mail:", &[None]);
        assert_eq!(body.text, "hi 🤣😀 :nope: 📧");
        assert!(body.emotes.is_empty());
        assert!(!body.emote_only);
    }

    #[test]
    fn channel_emotes_come_from_the_message() {
        let channel = [
            None,
            channel_emote(
                "cyour_channel-hypeWave",
                "ptvimages/1/12/1234567/emoticons/hypeWave.gif",
            ),
            channel_emote("wave", "https://images.picarto.tv/wave.png"),
        ];
        let body = site().convert(":cyour_channel-hypeWave: :cother-WAVE:", &channel);
        assert_eq!(codes(&body), [":cyour_channel-hypeWave:", ":cother-WAVE:"]);
        assert_eq!(
            body.emotes[0].url,
            "https://images.picarto.tv/ptvimages/1/12/1234567/emoticons/hypeWave.gif"
        );
        // An `a` without a dash matches the part after the code's first one.
        assert_eq!(body.emotes[1].url, "https://images.picarto.tv/wave.png");
        assert!(body.emote_only);
    }

    #[test]
    fn ptv_emotes_win_over_channel_emotes() {
        let channel = [channel_emote("shipit", "theirs.png")];
        let body = site().convert(":ptv-shipit:", &channel);
        assert_eq!(body.emotes[0].url, PNG);
    }

    #[test]
    fn channel_emote_version_busts_the_cache() {
        let mut emote = channel_emote("cchan-wave", "wave.png").unwrap();
        emote.d = Some(serde_json::json!(1700000000));
        assert_eq!(
            channel_url(&emote),
            "https://images.picarto.tv/wave.png?version=1700000000"
        );
        emote.d = Some(serde_json::json!("a b"));
        assert_eq!(channel_url(&emote), "https://images.picarto.tv/wave.png");
    }

    #[test]
    fn e_list_parses_with_nulls() {
        let e: Vec<Option<ChannelEmote>> =
            serde_json::from_str(r#"[null,{"a":"cchan-wave","u":"wave.png"}]"#).unwrap();
        assert!(e[0].is_none());
        assert_eq!(e[1].as_ref().unwrap().a, "cchan-wave");
    }

    #[test]
    fn stray_colons_do_not_hide_codes() {
        let body = site().convert("at 10:30 :ptv-shipit: ok:", &[]);
        assert_eq!(body.text, "at 10:30 :ptv-shipit: ok:");
        assert_eq!(codes(&body), [":ptv-shipit:"]);
    }

    #[test]
    fn emotes_listed_once() {
        let body = site().convert(":ptv-shipit: :ptv-shipit:", &[]);
        assert_eq!(codes(&body), [":ptv-shipit:"]);
        assert!(body.emote_only);
    }

    #[test]
    fn emote_only_needs_images_and_nothing_else() {
        let site = site();
        assert!(site.convert(" :ptv-shipit:  :ptv-uneasy: ", &[]).emote_only);
        assert!(!site.convert(":ptv-shipit: :rofl:", &[]).emote_only);
        assert!(!site.convert(":ptv-shipit: hi", &[]).emote_only);
        assert!(!site.convert("", &[]).emote_only);
        assert!(!site.convert(":rofl:", &[]).emote_only);
    }

    #[test]
    fn empty_tables_leave_text_alone() {
        let body = SiteEmotes::default().convert(":ptv-shipit: :rofl:", &[]);
        assert_eq!(body.text, ":ptv-shipit: :rofl:");
        assert!(body.emotes.is_empty());
    }
}
