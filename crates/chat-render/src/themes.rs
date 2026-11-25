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

use crate::{Builtin, CSS_FILE, MANIFEST_FILE, MESSAGE_FILE};

/// The valid theme folders in `themes_dir`, sorted by name (case-insensitive).
/// Only folders whose names pass [`validate_name`] are included, so the app's
/// theme picker and overlay `?theme=<name>` URLs always agree. A missing folder
/// simply means no themes yet.
pub fn list(themes_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(themes_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_name(name).is_ok() && name == name.trim())
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
    if name.chars().count() > 40 {
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
    if chat_core::fs::is_windows_reserved_name(name) {
        return Err(format!(
            "\"{name}\" can't be a theme name (Windows keeps it for itself)"
        ));
    }
    Ok(())
}

/// Creates `themes_dir/<name>/` with copies of the default built-in theme's
/// template, CSS, and manifest, to edit from there. Never touches an existing
/// theme.
pub fn create_from_default(themes_dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    create_from_builtin(themes_dir, name, Builtin::Default)
}

/// Creates `themes_dir/<name>/` as an editable copy of any built-in theme
/// (`message.html`, `overlay.css`, and `theme.toml`). Never touches an
/// existing theme.
pub fn create_from_builtin(
    themes_dir: &Path,
    name: &str,
    base: Builtin,
) -> anyhow::Result<PathBuf> {
    if let Err(problem) = validate_name(name) {
        bail!("{problem}");
    }
    let dir = themes_dir.join(name.trim());
    if dir.exists() {
        bail!("a theme called \"{}\" already exists", name.trim());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (file, contents) in [
        (MESSAGE_FILE, base.message_template()),
        (CSS_FILE, base.css()),
        (MANIFEST_FILE, base.manifest_toml()),
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
    fn lists_valid_theme_folders_sorted() {
        let dir = temp("list");
        assert!(list(&dir).is_empty(), "no folder yet is fine");
        for name in [
            "dark", "Studio", ".hidden", "my.theme", "default", "cozy", "CON",
        ] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "not a theme").unwrap();
        assert_eq!(list(&dir), ["dark", "Studio"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_theme_is_a_copy_of_the_defaults() {
        let dir = temp("create");
        let theme = create_from_default(&dir, " studio ").unwrap();
        assert_eq!(theme, dir.join("studio"));
        assert_eq!(
            std::fs::read_to_string(theme.join(CSS_FILE)).unwrap(),
            Builtin::Default.css()
        );
        assert_eq!(
            std::fs::read_to_string(theme.join(MESSAGE_FILE)).unwrap(),
            Builtin::Default.message_template()
        );
        assert_eq!(
            std::fs::read_to_string(theme.join(MANIFEST_FILE)).unwrap(),
            Builtin::Default.manifest_toml()
        );
        // The copy works as a theme and its manifest parses.
        crate::Theme::load(&theme).unwrap();
        assert!(
            !crate::ThemeSource::Folder(theme.clone())
                .manifest()
                .unwrap()
                .options
                .is_empty()
        );

        std::fs::write(theme.join(CSS_FILE), "/* edited */").unwrap();
        assert!(create_from_default(&dir, "studio").is_err());
        assert_eq!(
            std::fs::read_to_string(theme.join(CSS_FILE)).unwrap(),
            "/* edited */",
            "an existing theme is never overwritten"
        );

        // Copying another built-in theme (Minimal) copies its own files.
        let min = create_from_builtin(&dir, "my minimal", Builtin::Minimal).unwrap();
        assert_eq!(
            std::fs::read_to_string(min.join(CSS_FILE)).unwrap(),
            Builtin::Minimal.css()
        );
        assert_eq!(
            std::fs::read_to_string(min.join(MESSAGE_FILE)).unwrap(),
            Builtin::Minimal.message_template()
        );
        assert_eq!(
            std::fs::read_to_string(min.join(MANIFEST_FILE)).unwrap(),
            Builtin::Minimal.manifest_toml()
        );
        crate::Theme::load(&min).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
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
        assert!(
            validate_name("Bubbles").is_err(),
            "built-in names are taken"
        );
        assert!(validate_name("Cozy").is_err(), "built-in names are taken");
        assert!(validate_name("Retro").is_err(), "built-in names are taken");
        assert!(validate_name("Neon").is_err(), "built-in names are taken");
        assert!(validate_name("Ornate").is_err(), "built-in names are taken");
        assert!(validate_name("Aero").is_err(), "built-in names are taken");
        assert!(validate_name("Comic").is_err(), "built-in names are taken");
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(&"x".repeat(41)).is_err());
        assert!(
            validate_name(&"ä".repeat(40)).is_ok(),
            "limit is in characters, not bytes"
        );
        assert!(validate_name(&"ä".repeat(41)).is_err());
        assert!(validate_name("CON").is_err());
        assert!(validate_name("nul").is_err());
        assert!(validate_name("COM1").is_err());
    }
}
