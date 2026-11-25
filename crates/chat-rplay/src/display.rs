//! How RPlay's website shows chat, ported: name colours, anonymous names,
//! the streamer's banned words (see RPLAY.md, "Display details"). RPlay
//! sends none of this in the messages; every viewer's browser works it out
//! the same way, so the overlay has to as well.

/// The name colour RPlay gives a user, from their id (`stringToBrightColor`
/// on the website): always the same for the same chatter, and always light
/// (every channel is at least 0x7f).
///
/// The website's loop looks like it needs care: `h << 5` cuts `h` to 32
/// bits, while `- h` and `+ char` work on the full number, which grows past
/// 32 bits. But only the low 32 bits are used in the end, and those come
/// out the same when every step wraps at 32 bits (as long as the full
/// number stays exact, which it does far beyond a 24-character id). So
/// it's Java's string hash, as for the anonymous names; the test compares
/// with values from RPlay's own function.
///
/// The website also swaps two colours of its own design (`#00b2ff`,
/// `#00d85f`) for another, but with every channel at 0x7f or more neither
/// can come out, so that's left out here.
pub(crate) fn name_color(user_id: &str) -> String {
    let h = java_hash(user_id);
    let channel = |shift: u32| ((h >> shift) & 0xff) as u8;
    // `Math.min(c + 127, 255)`.
    let bright = |c: u8| c.saturating_add(127);
    format!(
        "#{:02x}{:02x}{:02x}",
        bright(channel(16)),
        bright(channel(8)),
        bright(channel(0))
    )
}

/// Java's string hash (`h = 31 * h + char`, wrapping at 32 bits), over
/// UTF-16 units: JavaScript's `charCodeAt` counts in those.
fn java_hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |h, unit| {
        h.wrapping_shl(5)
            .wrapping_sub(h)
            .wrapping_add(i32::from(unit))
    })
}

/// What RPlay shows instead of a hidden name in a channel with anonymous
/// chat (`nickToAnonNick`): "AnonAru-" and five digits from a hash of the
/// real nickname, so the same chatter keeps the same number. The website
/// translates "AnonAru"; English is what an overlay gets.
///
/// One difference: RPlay hashes the nickname in the viewer's language
/// (`multiLangNick`), so a chatter with a translated name can get another
/// number on RPlay's page. Only the plain nickname is in the message.
pub(crate) fn anonymous_name(nickname: &str) -> String {
    // In i64: the absolute value of i32::MIN doesn't fit an i32.
    let h = i64::from(java_hash(nickname));
    format!("AnonAru-{:05}", h.abs() % 100_000)
}

/// The text with the streamer's banned words starred out, as RPlay's page
/// does (`getBannedWordsFilterText`): each word is found ignoring case and
/// with any whitespace between its letters ("b a d" counts as "bad"), and
/// replaced by one `*` per letter. Words are applied in turn, each to the
/// result of the ones before.
pub(crate) fn censor(text: &str, banned_words: &[String]) -> String {
    let mut text = text.to_owned();
    for word in banned_words {
        let letters: Vec<char> = word.chars().collect();
        if letters.is_empty() {
            continue;
        }
        // JavaScript's `"*".repeat(word.length)`: UTF-16 units again, which
        // is one per letter for everything but emoji and rare characters.
        let stars = "*".repeat(word.encode_utf16().count());
        text = replace_spaced(&text, &letters, &stars);
    }
    text
}

/// Replaces every match of `letters` (whitespace allowed between them,
/// case ignored) with `stars`, scanning left to right like a global regex
/// replace.
fn replace_spaced(text: &str, letters: &[char], stars: &str) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    // Copied to `out` up to this byte.
    let mut done = 0;
    let mut i = 0;
    while i < chars.len() {
        match match_at(&chars, i, letters) {
            Some(end) => {
                out.push_str(&text[done..chars[i].0]);
                out.push_str(stars);
                done = chars.get(end).map_or(text.len(), |&(byte, _)| byte);
                i = end;
            }
            None => i += 1,
        }
    }
    out.push_str(&text[done..]);
    out
}

/// If `letters` match at char index `start`, the char index right after
/// the match.
fn match_at(chars: &[(usize, char)], start: usize, letters: &[char]) -> Option<usize> {
    let mut i = start;
    for (n, &letter) in letters.iter().enumerate() {
        // Whitespace only *between* letters, as in `a\s*b\s*c`.
        if n > 0 {
            while chars.get(i).is_some_and(|&(_, c)| c.is_whitespace()) {
                i += 1;
            }
        }
        let &(_, c) = chars.get(i)?;
        if !same_letter(c, letter) {
            return None;
        }
        i += 1;
    }
    Some(i)
}

fn same_letter(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_colors_match_rplays_own_function() {
        // Computed with the website's `stringToBrightColor` itself (run in
        // Node, 2026-11-14), for made-up ids. In the website's loop, every
        // one of these grows past 32 bits.
        for (id, color) in [
            ("0123456789abcdef01234567", "#ffffff"),
            ("5f0000000000000000000001", "#ffe591"),
            ("ffffffffffffffffffffffff", "#ffb87f"),
            ("aaaaaaaaaaaaaaaaaaaaaaaa", "#ffffff"),
            ("000000000000000000000000", "#ccc77f"),
            ("64b7e2c9a1f3d5e7b9c1a3e5", "#bfffff"),
        ] {
            assert_eq!(name_color(id), color, "{id}");
        }
    }

    #[test]
    fn anonymous_names_match_rplays_own_function() {
        // Also computed with the website's code (`nickToAnonNick`).
        for (nickname, number) in [
            ("Viewer42", "02800"),
            ("視聴者", "00519"),
            ("", "00000"),
            ("a", "00097"),
            ("Some Long Nickname With Spaces 123", "56513"),
            ("😀x", "59989"),
        ] {
            assert_eq!(
                anonymous_name(nickname),
                format!("AnonAru-{number}"),
                "{nickname:?}"
            );
        }
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn banned_words_are_starred_out() {
        let banned = words(&["bad", "worse"]);
        assert_eq!(censor("a bad day", &banned), "a *** day");
        // Case and spaces between the letters don't help...
        assert_eq!(censor("BaD and b a  d", &banned), "*** and ***");
        // ...and like RPlay's, the match doesn't care about word borders.
        assert_eq!(censor("badge", &banned), "***ge");
        assert_eq!(censor("worse, bad", &banned), "*****, ***");
        assert_eq!(censor("nothing here", &banned), "nothing here");

        // Other scripts, empty words, regex metacharacters, and leading whitespace.
        let unicode = words(&["", "나쁜", "バカ"]);
        assert_eq!(censor("나쁜 말, バ カ!", &unicode), "** 말, **!");
        assert_eq!(censor("1+1 (a.b)", &words(&["1+1", "a.b"])), "*** (***)");
        assert_eq!(censor("  bad", &words(&["bad"])), "  ***");
    }
}
