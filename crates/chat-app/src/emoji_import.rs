//! Taking in a YouTube emoji export (scripts/yt-emoji-export.user.js): check
//! it, then copy it into the settings folder, so the original download can
//! be moved or deleted without breaking anything.
//!
//! Plain file work without GPUI, so it's testable on its own; the window
//! runs it in the background.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chat_youtube::EmojiMap;

/// The copy's file name inside the settings folder.
const FILE_NAME: &str = "youtube-emojis.json";

/// Checks `source` and copies it into `settings_dir`. Returns where the copy
/// is and how many emoji it has. Nothing is copied if the file is invalid.
pub fn import(source: &Path, settings_dir: &Path) -> anyhow::Result<(PathBuf, usize)> {
    // Load first: a broken or unrelated file must not replace a good one.
    let count = count(source)?;
    let target = settings_dir.join(FILE_NAME);
    if same_file(source, &target) {
        return Ok((target, count));
    }
    std::fs::create_dir_all(settings_dir)
        .with_context(|| format!("creating {}", settings_dir.display()))?;
    // Copy to a temporary name, then rename: a rename is atomic, so the
    // target is never a half-written file, even if copying fails midway.
    let partial = settings_dir.join(format!("{FILE_NAME}.partial"));
    std::fs::copy(source, &partial).with_context(|| format!("copying {}", source.display()))?;
    std::fs::rename(&partial, &target).with_context(|| format!("saving {}", target.display()))?;
    Ok((target, count))
}

/// Number of emoji in an export, or why it can't be used.
pub fn count(path: &Path) -> anyhow::Result<usize> {
    let map = EmojiMap::load(path)?;
    if map.is_empty() {
        bail!("{} contains no emoji", path.display());
    }
    Ok(map.len())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = r#"{ "version": 1, "entries": [
        { "code": ":_hype:", "url": "https://yt3.ggpht.com/hype" },
        { "code": ":_wave:", "url": "https://yt3.ggpht.com/wave" }
    ] }"#;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-app-emoji-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn valid_export_is_copied_into_the_settings_folder() {
        let dir = temp_dir("valid");
        let download = dir.join("youtube-emojis-UC123.json");
        std::fs::write(&download, EXPORT).unwrap();
        let settings = dir.join("settings");

        let (copy, count) = import(&download, &settings).unwrap();
        assert_eq!(count, 2);
        assert_eq!(copy, settings.join(FILE_NAME));
        std::fs::remove_file(&download).unwrap();
        assert_eq!(
            super::count(&copy).unwrap(),
            2,
            "the copy doesn't need the original"
        );
        assert!(!settings.join(format!("{FILE_NAME}.partial")).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_file_changes_nothing() {
        let dir = temp_dir("invalid");
        let settings = dir.join("settings");
        let good = dir.join("good.json");
        std::fs::write(&good, EXPORT).unwrap();
        import(&good, &settings).unwrap();

        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert!(import(&bad, &settings).is_err());
        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{ "version": 1, "entries": [] }"#).unwrap();
        assert!(import(&empty, &settings).is_err());

        assert_eq!(
            count(&settings.join(FILE_NAME)).unwrap(),
            2,
            "the good copy survives"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn importing_the_copy_itself_is_fine() {
        let dir = temp_dir("same");
        let download = dir.join("x.json");
        std::fs::write(&download, EXPORT).unwrap();
        let (copy, _) = import(&download, &dir).unwrap();
        assert_eq!(import(&copy, &dir).unwrap().1, 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
