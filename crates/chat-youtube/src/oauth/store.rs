//! Where the refresh token lives between runs.
//!
//! The refresh token is the long-lived secret (it can mint access tokens
//! until revoked), so it goes into the OS keyring: Keychain on macOS,
//! Credential Manager on Windows, Secret Service (gnome-keyring, KWallet,
//! KeePassXC, ...) on Linux. Without a keyring, a file in the app's config
//! folder that only the user can read.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::OAuthApp;

const KEYRING_SERVICE: &str = "chat-aggregator";

#[derive(Clone, Debug)]
pub enum TokenStore {
    /// The OS keyring, falling back to a file in the app's config folder if
    /// the system has no keyring service.
    System,
    /// A file in this folder (tests, portable setups).
    Dir(PathBuf),
}

impl TokenStore {
    // The keyring libraries block (Secret Service is a synchronous D-Bus
    // call), and so does file I/O. Blocking inside an async task would stall
    // one of tokio's worker threads, so each operation runs on tokio's
    // separate thread pool for blocking work via `spawn_blocking`.

    pub(crate) async fn load(&self, app: &OAuthApp) -> anyhow::Result<Option<String>> {
        let (store, account) = (self.clone(), account(app));
        tokio::task::spawn_blocking(move || store.load_blocking(&account)).await?
    }

    pub(crate) async fn save(&self, app: &OAuthApp, token: &str) -> anyhow::Result<()> {
        let (store, account, token) = (self.clone(), account(app), token.to_string());
        tokio::task::spawn_blocking(move || store.save_blocking(&account, &token)).await?
    }

    pub(crate) async fn delete(&self, app: &OAuthApp) -> anyhow::Result<()> {
        let (store, account) = (self.clone(), account(app));
        tokio::task::spawn_blocking(move || store.delete_blocking(&account)).await?
    }

    fn load_blocking(&self, account: &str) -> anyhow::Result<Option<String>> {
        match self {
            TokenStore::Dir(dir) => read_file(&file_path(dir, account)),
            TokenStore::System => match keyring_entry(account).and_then(|e| e.get_password()) {
                Ok(token) => Ok(Some(token)),
                // Not in the keyring: maybe saved to the fallback file
                // earlier, while no keyring was running.
                Err(keyring::Error::NoEntry) => read_file(&fallback_path(account)?),
                Err(e) if no_keyring(&e) => read_file(&fallback_path(account)?),
                Err(e) => Err(keyring_error(e)),
            },
        }
    }

    fn save_blocking(&self, account: &str, token: &str) -> anyhow::Result<()> {
        match self {
            TokenStore::Dir(dir) => write_private(&file_path(dir, account), token),
            TokenStore::System => {
                match keyring_entry(account).and_then(|e| e.set_password(token)) {
                    Ok(()) => {
                        // A token from a keyring-less time is now outdated.
                        remove_file(&fallback_path(account)?)?;
                        Ok(())
                    }
                    Err(e) if no_keyring(&e) => {
                        let path = fallback_path(account)?;
                        tracing::warn!(
                            "no system keyring available ({e}); storing the YouTube login in {} \
                         (readable only by you)",
                            path.display()
                        );
                        write_private(&path, token)
                    }
                    Err(e) => Err(keyring_error(e)),
                }
            }
        }
    }

    fn delete_blocking(&self, account: &str) -> anyhow::Result<()> {
        match self {
            TokenStore::Dir(dir) => remove_file(&file_path(dir, account)),
            TokenStore::System => {
                match keyring_entry(account).and_then(|e| e.delete_credential()) {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(e) if no_keyring(&e) => {}
                    Err(e) => return Err(keyring_error(e)),
                }
                remove_file(&fallback_path(account)?)
            }
        }
    }
}

/// One entry per OAuth client: switching to another Google project must not
/// reuse a token that belongs to a different one.
fn account(app: &OAuthApp) -> String {
    format!("youtube:{}", app.client_id())
}

fn keyring_entry(account: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, account)
}

/// The system has no usable keyring at all: fall back to the file. A keyring
/// that exists but is locked (`NoStorageAccess`) is deliberately *not* in
/// this list: the user chose to lock it, and quietly writing the secret to a
/// file instead would go around that choice.
fn no_keyring(e: &keyring::Error) -> bool {
    matches!(
        e,
        keyring::Error::NoDefaultStore | keyring::Error::PlatformFailure(_)
    )
}

fn keyring_error(e: keyring::Error) -> anyhow::Error {
    anyhow::Error::new(e).context("accessing the system keyring failed (is it locked?)")
}

fn fallback_path(account: &str) -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "chat-aggregator")
        .context("can't determine the config folder (no home directory?)")?;
    Ok(file_path(dirs.config_dir(), account))
}

fn file_path(dir: &Path, account: &str) -> PathBuf {
    // Client ids look like "1234-abc.apps.googleusercontent.com", but keep
    // the file name safe whatever comes in.
    let name: String = account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{name}.token"))
}

fn read_file(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(token) => Ok(Some(token.trim().to_string())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes a file only the current user can read (on Unix: mode 0600; on
/// Windows the user's profile folder already restricts access).
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    let result = (|| {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            // `mode` only applies when the file is created; tighten an
            // existing file too.
            if path.exists() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        options.open(path)?.write_all(contents.as_bytes())
    })();
    result.with_context(|| format!("writing {}", path.display()))
}

fn remove_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_dir(name: &str) -> (OAuthApp, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "chat-youtube-store-test-{}-{name}",
            std::process::id()
        ));
        let app = OAuthApp::new("123-abc.apps.googleusercontent.com", "secret")
            .with_store(TokenStore::Dir(dir.clone()));
        (app, dir)
    }

    #[tokio::test]
    async fn save_load_delete_roundtrip() {
        let (app, dir) = app_with_dir("roundtrip");
        assert_eq!(app.store.load(&app).await.unwrap(), None);

        app.store.save(&app, "refresh-1").await.unwrap();
        assert_eq!(
            app.store.load(&app).await.unwrap().as_deref(),
            Some("refresh-1")
        );

        app.store.delete(&app).await.unwrap();
        assert_eq!(app.store.load(&app).await.unwrap(), None);
        app.store.delete(&app).await.unwrap(); // deleting twice is fine
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn token_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (app, dir) = app_with_dir("private");
        app.store.save(&app, "refresh-1").await.unwrap();

        let path = file_path(&dir, &account(&app));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "token file must be readable only by the user"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_names_are_sanitized() {
        let path = file_path(Path::new("/x"), "youtube:../evil/id");
        assert_eq!(path, Path::new("/x/youtube_.._evil_id.token"));
    }
}
