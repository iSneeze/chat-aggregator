//! Where secrets live between runs: the YouTube login (a refresh token),
//! the OBS WebSocket password.
//!
//! They go into the OS keyring: Keychain on macOS, Credential Manager on
//! Windows, Secret Service (gnome-keyring, KWallet, KeePassXC, ...) on
//! Linux. Without a keyring, a file in the app's config folder that only
//! the user can read. Never `config.toml`: that file gets pasted into bug
//! reports.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::fs::write_private;

const KEYRING_SERVICE: &str = "chat-aggregator";

/// Where secrets are kept. Each secret has an *account* name within it,
/// e.g. `youtube:<client id>` or `obs-websocket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretStore {
    /// The OS keyring, falling back to a file in the app's config folder if
    /// the system has no keyring service.
    System,
    /// A file per secret in this folder (tests, portable setups).
    Dir(PathBuf),
}

impl SecretStore {
    // The keyring libraries block (Secret Service is a synchronous D-Bus
    // call), and so does file I/O. Blocking inside an async task would stall
    // one of tokio's worker threads, so each operation runs on tokio's
    // separate thread pool for blocking work via `spawn_blocking`.

    pub async fn load(&self, account: &str) -> anyhow::Result<Option<String>> {
        let (store, account) = (self.clone(), account.to_string());
        tokio::task::spawn_blocking(move || store.load_blocking(&account)).await?
    }

    pub async fn save(&self, account: &str, secret: &str) -> anyhow::Result<()> {
        let (store, account, secret) = (self.clone(), account.to_string(), secret.to_string());
        tokio::task::spawn_blocking(move || store.save_blocking(&account, &secret)).await?
    }

    pub async fn delete(&self, account: &str) -> anyhow::Result<()> {
        let (store, account) = (self.clone(), account.to_string());
        tokio::task::spawn_blocking(move || store.delete_blocking(&account)).await?
    }

    fn load_blocking(&self, account: &str) -> anyhow::Result<Option<String>> {
        match self {
            SecretStore::Dir(dir) => read_file(&file_path(dir, account)),
            SecretStore::System => match keyring_entry(account).and_then(|e| e.get_password()) {
                Ok(secret) => Ok(Some(secret)),
                // Not in the keyring: maybe saved to the fallback file
                // earlier, while no keyring was running.
                Err(keyring::Error::NoEntry) => read_file(&fallback_path(account)?),
                Err(e) if no_keyring(&e) => read_file(&fallback_path(account)?),
                Err(e) => Err(keyring_error(e)),
            },
        }
    }

    fn save_blocking(&self, account: &str, secret: &str) -> anyhow::Result<()> {
        match self {
            SecretStore::Dir(dir) => write_private(&file_path(dir, account), secret),
            SecretStore::System => {
                match keyring_entry(account).and_then(|e| e.set_password(secret)) {
                    Ok(()) => {
                        // A secret from a keyring-less time is now outdated.
                        remove_file(&fallback_path(account)?)?;
                        Ok(())
                    }
                    Err(e) if no_keyring(&e) => {
                        let path = fallback_path(account)?;
                        tracing::warn!(
                            "no system keyring available ({e}); storing {account} in {} \
                             (readable only by you)",
                            path.display()
                        );
                        write_private(&path, secret)
                    }
                    Err(e) => Err(keyring_error(e)),
                }
            }
        }
    }

    fn delete_blocking(&self, account: &str) -> anyhow::Result<()> {
        match self {
            SecretStore::Dir(dir) => remove_file(&file_path(dir, account)),
            SecretStore::System => {
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
    // Account names contain e.g. a client id like
    // "1234-abc.apps.googleusercontent.com", but keep the file name safe
    // whatever comes in. (`.token` for every secret: the YouTube login's
    // files had that name before this module was shared, and must still be
    // found.)
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
        Ok(secret) => Ok(Some(secret.trim().to_string())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
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

    fn temp_store(name: &str) -> (SecretStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "chat-core-secrets-test-{}-{name}",
            std::process::id()
        ));
        (SecretStore::Dir(dir.clone()), dir)
    }

    #[tokio::test]
    async fn save_load_delete_roundtrip() {
        let (store, dir) = temp_store("roundtrip");
        assert_eq!(store.load("a").await.unwrap(), None);

        store.save("a", "secret-1").await.unwrap();
        store.save("b", "other").await.unwrap();
        assert_eq!(store.load("a").await.unwrap().as_deref(), Some("secret-1"));
        assert_eq!(store.load("b").await.unwrap().as_deref(), Some("other"));

        store.delete("a").await.unwrap();
        assert_eq!(store.load("a").await.unwrap(), None);
        store.delete("a").await.unwrap(); // deleting twice is fine
        assert_eq!(store.load("b").await.unwrap().as_deref(), Some("other"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_names_are_sanitized() {
        let path = file_path(Path::new("/x"), "youtube:../evil/id");
        assert_eq!(path, Path::new("/x/youtube_.._evil_id.token"));
    }
}
