//! Logging: to the console and to a file in the settings folder.
//!
//! The file is what testers and users can send along with a bug report,
//! without having to copy text out of a console (and, since the Windows
//! release build opens without a console, it's the only record there is).
//! It holds what the program did (sources started, retries, errors), never
//! chat messages or secrets; docs/privacy.md says so.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// The current run's log, next to `config.toml`.
pub const FILE_NAME: &str = "chat-aggregator.log";
/// The run before, so restarting after a crash doesn't wipe out its log.
const OLD_FILE_NAME: &str = "chat-aggregator.old.log";

/// Sets up logging for the whole process: to the console, and into
/// `settings_dir` if there is one. If the file can't be created, logging
/// still goes to the console (and says why the file is missing).
pub fn init(settings_dir: Option<&Path>) {
    let (file, file_error) = match settings_dir.map(open_log_file) {
        Some(Ok(file)) => (Some(file), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    // `Arc<File>` is a writer the layer can share between threads: `&File`
    // implements `Write`, so no lock is needed around it. An `Option` of a
    // layer is a layer too; `None` does nothing.
    let file_layer = file.map(|file| fmt::layer().with_ansi(false).with_writer(Arc::new(file)));
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .with(file_layer)
        .init();
    // Only now is there anywhere to report it.
    if let Some(e) = file_error {
        tracing::warn!("no log file: {e:#}");
    }
    log_panics();
}

/// Creates a fresh log file in `dir`, keeping the previous one as
/// [`OLD_FILE_NAME`].
fn open_log_file(dir: &Path) -> anyhow::Result<File> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(FILE_NAME);
    // `rename` replaces an existing old log, on Windows too.
    match std::fs::rename(&path, dir.join(OLD_FILE_NAME)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("keeping {}", path.display())),
    }
    File::create(&path).with_context(|| format!("creating {}", path.display()))
}

/// Writes panics into the log too. Rust's default panic handler prints them
/// only to the console, so a crash would leave no trace in the file.
fn log_panics() {
    // `take_hook` returns the current (default) handler; calling it after
    // logging keeps its console output, e.g. the backtrace hint.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        tracing::error!(thread, "{info}");
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-app-logging-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write_run(dir: &Path, text: &str) {
        use std::io::Write as _;
        let mut file = open_log_file(dir).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn creates_the_settings_folder_on_first_start() {
        let dir = temp_dir("first");
        write_run(&dir, "run 1");
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(),
            "run 1"
        );
        assert!(!dir.join(OLD_FILE_NAME).exists());
    }

    #[test]
    fn the_previous_run_is_kept_after_a_restart() {
        let dir = temp_dir("restart");
        write_run(&dir, "run 1");
        write_run(&dir, "run 2");
        write_run(&dir, "run 3");
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(),
            "run 3"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(OLD_FILE_NAME)).unwrap(),
            "run 2"
        );
    }
}
