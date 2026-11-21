//! Saving files that hold secrets (`config.toml` with the client secret,
//! the YouTube token file) safely.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Writes `contents` to `path` so that only the current user can read it
/// (Unix: mode 0600; on Windows the user's profile folder already restricts
/// access), and **atomically**: whatever happens (crash, power cut, full
/// disk), `path` afterwards holds either the old contents or the new ones,
/// never a half-written file.
///
/// How: write everything to `<name>.tmp` next to it, make sure it's on disk
/// (`sync_all`), then `rename` it over the old file. A rename within one
/// folder swaps which file the name points to in one step. Writing into the
/// file itself would first empty it (`truncate`) and could stop halfway.
///
/// If `path` is a symlink (e.g. a dotfiles setup), the file it points to is
/// replaced, and the link stays.
pub fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    write_atomically(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    // Renaming over a symlink would replace the link itself with a plain
    // file; write next to its target instead. (`canonicalize` fails if the
    // file doesn't exist yet: then there's no link either.)
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)?;
    let mut temp_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_os_string();
    temp_name.push(".tmp");
    let temp = dir.join(temp_name);

    let result = (|| {
        let mut file = open_private(&temp)?;
        file.write_all(contents.as_bytes())?;
        // On disk before the rename: otherwise a power cut right after it
        // could leave the new name pointing at a file that's still empty.
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp); // don't leave a half-written copy
        return result;
    }
    // Unix: the rename itself lives in the folder; flush that too, so it
    // survives a power cut. (Not possible, and not needed, on Windows.)
    #[cfg(unix)]
    if let Ok(dir) = File::open(&dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Whether `name` is one of the reserved device names Windows keeps for itself
/// (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, case-insensitive).
/// Creating a file or folder with one of these fails or behaves strangely on
/// Windows, so profile, theme and widget names refuse them on every OS.
pub fn is_windows_reserved_name(name: &str) -> bool {
    let upper = name.trim().to_ascii_uppercase();
    ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || ["COM", "LPT"].iter().any(|prefix| {
            upper
                .strip_prefix(prefix)
                .is_some_and(|n| n.len() == 1 && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// Resolves `path` (which is expected to live inside `dir`) and verifies it
/// does not escape `dir` through symlinks.
///
/// - `Ok(Some(canonical))` if `path` exists and resolves inside `dir`.
/// - `Ok(None)` if `path` (or `dir`) does not exist.
/// - `Err(PermissionDenied)` if `path` resolves outside `dir`.
pub fn existing_inside_dir(dir: &Path, path: &Path) -> io::Result<Option<PathBuf>> {
    let canon_dir = match std::fs::canonicalize(dir) {
        Ok(canon) => canon,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let canon_path = match std::fs::canonicalize(path) {
        Ok(canon) => canon,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if canon_path.starts_with(&canon_dir) {
        Ok(Some(canon_path))
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} points outside {}", path.display(), dir.display()),
        ))
    }
}

/// Creates (or empties) `path`, readable only by the current user.
fn open_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        // `mode` only applies when the file is created: a leftover `.tmp`
        // from a crash keeps its old permissions unless tightened.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chat-core-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn replaces_the_file_and_leaves_nothing_behind() {
        let dir = temp_dir("replace");
        let path = dir.join("config.toml");
        write_private(&path, "first").unwrap();
        write_private(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"], "no .tmp left over");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("private");
        let path = dir.join("config.toml");
        // Even when the file existed before with looser permissions, and a
        // leftover temp file too.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "old").unwrap();
        std::fs::write(dir.join("config.toml.tmp"), "crashed").unwrap();
        for file in [&path, &dir.join("config.toml.tmp")] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_private(&path, "secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_stays_a_symlink() {
        let dir = temp_dir("symlink");
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let target = dir.join("dotfiles").join("config.toml");
        std::fs::write(&target, "old").unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_private(&link, "new").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn windows_reserved_names_are_caught() {
        for bad in ["CON", "nul", "PRN", "aux", "COM1", "lpt9", " NUL "] {
            assert!(is_windows_reserved_name(bad), "{bad}");
        }
        for ok in ["cozy", "COM10", "console", "COM", "LPTa"] {
            assert!(!is_windows_reserved_name(ok), "{ok}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_inside_dir_rejects_escaping_symlinks() {
        let root = temp_dir("inside-dir");
        let theme = root.join("theme");
        std::fs::create_dir_all(&theme).unwrap();
        let secret = root.join("config.toml");
        std::fs::write(&secret, "secret").unwrap();
        let inside = theme.join("ok.css");
        std::fs::write(&inside, "body{}").unwrap();
        let escape = theme.join("escape.css");
        std::os::unix::fs::symlink(&secret, &escape).unwrap();

        assert!(existing_inside_dir(&theme, &inside).unwrap().is_some());
        assert!(
            existing_inside_dir(&theme, &theme.join("missing.css"))
                .unwrap()
                .is_none()
        );
        let err = existing_inside_dir(&theme, &escape).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        std::fs::remove_dir_all(root).unwrap();
    }
}
