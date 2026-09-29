//! Named overlay themes: one folder per theme inside a themes folder.
//!
//! ```text
//! themes/
//! ├── cozy/
//! │   ├── overlay.css     the look
//! │   ├── message.html    optional: the structure of a message
//! │   └── …               optional: images, fonts used by the CSS
//! └── dark/
//!     └── overlay.css
//! ```
//!
//! Every file is optional: whatever a theme doesn't have comes from the
//! built-in theme, which itself is never copied into this folder (so it
//! keeps improving with updates). "New theme" starts from a copy of it.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::{Builtin, CSS_FILE, DEFAULT_CSS, DEFAULT_MESSAGE_TEMPLATE, MESSAGE_FILE};

/// The theme folders in `themes_dir`, sorted by name (case-insensitive).
/// A missing folder simply means no themes yet.
pub fn list(themes_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(themes_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names
}

/// Checks a name for a new theme. It becomes a folder name, so it's kept to
/// characters that are safe on every operating system.
pub fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("give the theme a name".into());
    }
    if name.len() > 40 {
        return Err("that name is too long (40 characters at most)".into());
    }
    // The built-in themes' names: config.toml and overlay URLs use them.
    if let Some(builtin) = Builtin::from_name(name) {
        return Err(format!("\"{}\" is a built-in theme", builtin.label()));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
    {
        return Err("use letters, digits, spaces, - and _ only".into());
    }
    Ok(())
}

/// Creates `themes_dir/<name>/` with copies of the built-in template and
/// CSS, to edit from there. Never touches an existing theme.
pub fn create_from_default(themes_dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if let Err(problem) = validate_name(name) {
        bail!("{problem}");
    }
    let dir = themes_dir.join(name.trim());
    if dir.exists() {
        bail!("a theme called \"{}\" already exists", name.trim());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (file, contents) in [
        (MESSAGE_FILE, DEFAULT_MESSAGE_TEMPLATE),
        (CSS_FILE, DEFAULT_CSS),
    ] {
        let path = dir.join(file);
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-render-themes-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lists_theme_folders_sorted() {
        let dir = temp("list");
        assert!(list(&dir).is_empty(), "no folder yet is fine");
        for name in ["dark", "Cozy", ".hidden"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "not a theme").unwrap();
        assert_eq!(list(&dir), ["Cozy", "dark"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_theme_is_a_copy_of_the_defaults() {
        let dir = temp("create");
        let theme = create_from_default(&dir, " cozy ").unwrap();
        assert_eq!(theme, dir.join("cozy"));
        assert_eq!(
            std::fs::read_to_string(theme.join(CSS_FILE)).unwrap(),
            DEFAULT_CSS
        );
        assert_eq!(
            std::fs::read_to_string(theme.join(MESSAGE_FILE)).unwrap(),
            DEFAULT_MESSAGE_TEMPLATE
        );
        // The copy works as a theme.
        crate::Theme::load(&theme).unwrap();

        std::fs::write(theme.join(CSS_FILE), "/* edited */").unwrap();
        assert!(create_from_default(&dir, "cozy").is_err());
        assert_eq!(
            std::fs::read_to_string(theme.join(CSS_FILE)).unwrap(),
            "/* edited */",
            "an existing theme is never overwritten"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_are_checked() {
        assert!(validate_name("Cozy Night 2").is_ok());
        assert!(validate_name("  ").is_err());
        assert!(validate_name("default").is_err());
        assert!(
            validate_name(" Minimal ").is_err(),
            "built-in names are taken"
        );
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(&"x".repeat(41)).is_err());
    }
}
