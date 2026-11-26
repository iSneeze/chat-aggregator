//! Managed custom font files (`fonts/` next to `config.toml`) and helpers for
//! validating both imported font files and installed system font family names.
//!
//! ```text
//! fonts/
//! ├── ComicNeue-Bold.woff2
//! └── DeadlockDisplay.ttf
//! ```
//!
//! Imported fonts are copied into `fonts/` so overlays keep working if the
//! original download is moved, and so `chat-server` only ever serves files
//! confined inside that directory.

use std::path::Path;

use anyhow::{Context, bail};

/// Supported font file extensions (lowercase, without leading dot).
pub const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "woff2", "woff"];

/// Upper bound on a single imported font file (25 MiB: room for large CJK
/// fonts while rejecting accidentally picked media files).
pub const MAX_FONT_BYTES: u64 = 25 * 1024 * 1024;

/// Prefix for an installed OS font family saved in a `font` theme option.
pub const SYSTEM_FONT_PREFIX: &str = "system:";

/// Prefix for an imported font file (inside `fonts/`) saved in a `font` option.
pub const FILE_FONT_PREFIX: &str = "file:";

/// Parsed representation of a `type = "font"` theme option value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontSelection<'a> {
    /// One of the theme's curated font stacks (e.g. `"Editorial"`).
    Preset(&'a str),
    /// A locally installed OS font family name (e.g. `"Georgia"`).
    System(&'a str),
    /// A validated filename inside the app's `fonts/` folder.
    File(&'a str),
}

/// Splits a saved `font` option string into its curated preset, system family,
/// or imported file form.
pub fn parse_selection(raw: &str) -> FontSelection<'_> {
    if let Some(family) = raw.strip_prefix(SYSTEM_FONT_PREFIX) {
        FontSelection::System(family.trim())
    } else if let Some(file) = raw.strip_prefix(FILE_FONT_PREFIX) {
        FontSelection::File(file.trim())
    } else {
        FontSelection::Preset(raw)
    }
}

/// Formats an installed system font family as a saved option value.
pub fn format_system_value(family: &str) -> String {
    format!("{SYSTEM_FONT_PREFIX}{}", family.trim())
}

/// Formats an imported font filename as a saved option value.
pub fn format_file_value(filename: &str) -> String {
    format!("{FILE_FONT_PREFIX}{}", filename.trim())
}

/// Checks whether a system font family name is safe to embed inside a quoted
/// CSS `font-family` declaration (`"<name>", system-ui, sans-serif`).
pub fn is_valid_system_font(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 100 {
        return false;
    }
    // Reject control characters and CSS / HTML syntax delimiters.
    !trimmed.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '"' | '\'' | '\\' | ';' | '{' | '}' | '(' | ')' | ',' | '<' | '>' | '/' | '*'
            )
    })
}

/// Checks whether `name` is a safe single-segment filename with a supported font
/// extension (`.ttf`, `.otf`, `.woff2`, `.woff`).
pub fn is_valid_filename(name: &str) -> bool {
    if name.is_empty() || name != name.trim() || name.chars().count() > 120 {
        return false;
    }
    if name.starts_with('.') || name.contains("..") {
        return false;
    }
    if name.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '/' | '\\'
                    | ':'
                    | '"'
                    | '\''
                    | ';'
                    | '{'
                    | '}'
                    | '('
                    | ')'
                    | '*'
                    | '?'
                    | '<'
                    | '>'
                    | '|'
            )
    }) {
        return false;
    }
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() || stem.ends_with('.') || chat_core::fs::is_windows_reserved_name(stem) {
        return false;
    }
    let ext_lower = ext.to_ascii_lowercase();
    FONT_EXTENSIONS.contains(&ext_lower.as_str())
}

/// Checks the leading magic bytes of a font file so accidentally selected
/// non-font files fail with a clear message before being copied.
pub fn validate_font_bytes(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 4 {
        return Err("file is too small to be a font".into());
    }
    let magic = &bytes[..4];
    let ok = matches!(
        magic,
        b"\x00\x01\x00\x00" | b"true" | b"OTTO" | b"ttcf" | b"wOFF" | b"wOF2"
    );
    if ok {
        Ok(())
    } else {
        Err("file does not look like a TTF, OTF, WOFF, or WOFF2 font".into())
    }
}

/// Human-friendly label for an imported font filename (drops the extension).
pub fn display_name(filename: &str) -> String {
    match filename.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => filename.to_string(),
    }
}

/// Unique CSS `font-family` name for an imported font file, prefixed so it
/// never collides with an installed OS font of the same name.
pub fn css_family_name(filename: &str) -> String {
    format!("chat-font-{filename}")
}

/// Lists valid font filenames in `fonts_dir`, sorted case-insensitively.
/// Missing folders return an empty list; symlinks pointing outside `fonts_dir`
/// are ignored.
pub fn list(fonts_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(fonts_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !is_valid_filename(&name) {
                return None;
            }
            let path = entry.path();
            let canon = chat_core::fs::existing_inside_dir(fonts_dir, &path).ok()??;
            canon.is_file().then_some(name)
        })
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names
}

/// Copies a user-picked font file into `fonts_dir` after checking its
/// extension, size, and magic header. Returns the stored filename inside
/// `fonts_dir`.
///
/// If an identical file with the same name is already imported, it is reused;
/// if a different file already uses that name, a numeric suffix (`-2`, `-3`, …)
/// is added so nothing is overwritten.
pub fn import(fonts_dir: &Path, source: &Path) -> anyhow::Result<String> {
    let meta =
        std::fs::metadata(source).with_context(|| format!("reading {}", source.display()))?;
    if !meta.is_file() {
        bail!("{} is not a file", source.display());
    }
    if meta.len() > MAX_FONT_BYTES {
        bail!("font file is larger than 25 MB");
    }

    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| FONT_EXTENSIONS.contains(&e.as_str()))
        .ok_or_else(|| anyhow::anyhow!("pick a .ttf, .otf, .woff2, or .woff font file"))?;

    let bytes = std::fs::read(source).with_context(|| format!("reading {}", source.display()))?;
    if let Err(problem) = validate_font_bytes(&bytes) {
        bail!("{problem}");
    }

    let raw_stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("custom-font");
    let stem = sanitize_stem(raw_stem);

    std::fs::create_dir_all(fonts_dir)
        .with_context(|| format!("creating {}", fonts_dir.display()))?;

    for attempt in 1u32..=999 {
        let candidate = if attempt == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{attempt}.{ext}")
        };
        let dest = fonts_dir.join(&candidate);
        if dest.exists() {
            if let Ok(existing) = std::fs::read(&dest)
                && existing == bytes
            {
                return Ok(candidate);
            }
            continue;
        }
        std::fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
        return Ok(candidate);
    }

    bail!("too many imported fonts with the same name")
}

/// Cleans a source filename stem into a safe cross-platform stem that satisfies
/// [`is_valid_filename`] once `.ext` is appended.
fn sanitize_stem(raw: &str) -> String {
    let mut cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '/' | '\\'
                        | ':'
                        | '"'
                        | '\''
                        | ';'
                        | '{'
                        | '}'
                        | '('
                        | ')'
                        | '*'
                        | '?'
                        | '<'
                        | '>'
                        | '|'
                )
            {
                '-'
            } else {
                c
            }
        })
        .collect();
    while cleaned.contains("..") {
        cleaned = cleaned.replace("..", ".");
    }
    let trimmed = cleaned.trim_matches(['.', ' ']).to_string();
    let mut stem: String = trimmed.chars().take(100).collect();
    let stem_trimmed = stem.trim_matches(['.', ' ']);
    if stem_trimmed.is_empty() || chat_core::fs::is_windows_reserved_name(stem_trimmed) {
        stem = "custom-font".to_string();
    } else {
        stem = stem_trimmed.to_string();
    }
    stem
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-render-fonts-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn validates_system_font_names_and_filenames() {
        assert!(is_valid_system_font("Georgia"));
        assert!(is_valid_system_font("Noto Sans CJK JP"));
        assert!(is_valid_system_font("M+ 1p"));
        assert!(is_valid_system_font("メイリオ"));
        assert!(!is_valid_system_font(""));
        assert!(!is_valid_system_font("   "));
        assert!(!is_valid_system_font("Arial\"; } body { display: none"));
        assert!(!is_valid_system_font("Font, Fallback"));
        assert!(!is_valid_system_font("Bad\nFont"));

        assert!(is_valid_filename("Deadlock-Bold.woff2"));
        assert!(is_valid_filename("My Comic Font.TTF"));
        assert!(is_valid_filename("OpenType.otf"));
        assert!(is_valid_filename("Legacy.woff"));
        assert!(!is_valid_filename(""));
        assert!(!is_valid_filename(".hidden.ttf"));
        assert!(!is_valid_filename("../escape.ttf"));
        assert!(!is_valid_filename("sub/font.ttf"));
        assert!(!is_valid_filename("CON.ttf"));
        assert!(!is_valid_filename("not-a-font.png"));
        assert!(!is_valid_filename("bad;name.ttf"));
    }

    #[test]
    fn imports_lists_deduplicates_and_disambiguates_fonts() {
        let root = temp("import");
        let src_dir = root.join("downloads");
        let fonts_dir = root.join("fonts");
        std::fs::create_dir_all(&src_dir).unwrap();

        assert!(list(&fonts_dir).is_empty());

        // Valid WOFF2 header.
        let src_a = src_dir.join("HeroFont.woff2");
        std::fs::write(&src_a, b"wOF2dummy-font-data-a").unwrap();
        let saved = import(&fonts_dir, &src_a).unwrap();
        assert_eq!(saved, "HeroFont.woff2");
        assert_eq!(list(&fonts_dir), vec!["HeroFont.woff2"]);

        // Re-importing the exact same bytes reuses the existing file.
        assert_eq!(import(&fonts_dir, &src_a).unwrap(), "HeroFont.woff2");

        // Different bytes with the same filename get a `-2` suffix.
        std::fs::write(&src_a, b"wOF2dummy-font-data-b").unwrap();
        let saved2 = import(&fonts_dir, &src_a).unwrap();
        assert_eq!(saved2, "HeroFont-2.woff2");
        assert_eq!(list(&fonts_dir), vec!["HeroFont-2.woff2", "HeroFont.woff2"]);

        // Invalid extension or bad magic bytes are rejected.
        let fake_png = src_dir.join("cat.png");
        std::fs::write(&fake_png, b"\x89PNG\r\n\x1a\n").unwrap();
        assert!(import(&fonts_dir, &fake_png).is_err());

        let fake_ttf = src_dir.join("actually-text.ttf");
        std::fs::write(&fake_ttf, b"hello world").unwrap();
        assert!(import(&fonts_dir, &fake_ttf).is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn list_ignores_symlinks_escaping_fonts_dir() {
        let root = temp("symlink");
        let fonts_dir = root.join("fonts");
        std::fs::create_dir_all(&fonts_dir).unwrap();
        let outside = root.join("secret.ttf");
        std::fs::write(&outside, b"wOF2secret").unwrap();
        std::os::unix::fs::symlink(&outside, fonts_dir.join("escape.ttf")).unwrap();
        std::fs::write(fonts_dir.join("valid.woff2"), b"wOF2ok").unwrap();

        assert_eq!(list(&fonts_dir), vec!["valid.woff2"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
