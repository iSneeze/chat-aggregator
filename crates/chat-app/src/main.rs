// Release builds on Windows open without a console window: the log file
// in the settings folder records what the console used to show. Dev
// builds (`cargo run`) keep the console to watch the log live.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! chat-aggregator's desktop app: a window to control the engine.
//!
//! Two runtimes side by side:
//! - GPUI owns the main thread (window, input, drawing) with its own
//!   executor.
//! - tokio runs the engine (network, overlay server) on two worker threads.
//!
//! They only talk through `tokio::sync` channels (`EngineHandle` commands,
//! the `watch` status), which can be awaited from either side: they don't
//! need the tokio runtime, only their wakers.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::Context as _;
use chat_engine::{ConfigFile, Engine};
use gpui_kit::component::Root;
use gpui_kit::*;

mod app_config;
mod app_view;
mod appearance;
mod emoji_import;
mod logging;
mod manual_window;
mod settings_window;
mod startup_error;
mod youtube_panel;

use app_config::AppConfig;
use app_view::AppView;
use startup_error::{Stage, StartupError};

fn main() -> anyhow::Result<()> {
    let config_path = ConfigFile::default_path()?;
    logging::init(config_path.parent());
    let settings_dir = config_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    match start(config_path, &settings_dir) {
        Ok(started) => {
            run(started);
            Ok(())
        }
        Err(failed) => {
            // Logged, so a failed start is in the log file too...
            tracing::error!("{:#}", failed.error);
            // ...and shown: on Windows there's no console to print it to.
            startup_error::show(&failed, &settings_dir);
            Err(failed.error)
        }
    }
}

/// Everything that's ready once starting up worked.
struct Started {
    runtime: tokio::runtime::Runtime,
    engine: Engine,
    config: AppConfig,
}

/// Reads the settings and starts the engine. Each step's errors are marked
/// with the step (`Stage`), so the error window can give fitting advice.
fn start(config_path: PathBuf, settings_dir: &Path) -> Result<Started, StartupError> {
    let file =
        ConfigFile::load_or_default(&config_path).map_err(StartupError::at(Stage::Config))?;

    // Two workers are plenty for chat; tokio's default is one per CPU core.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("chat-engine")
        .enable_all()
        .build()
        .context("starting the tokio runtime")
        .map_err(StartupError::at(Stage::Other))?;

    // The engine starts without sources: `AppConfig` adds them one by one
    // to learn the id the engine gives each.
    let sources = file.sources.clone();
    let mut engine_config = file.into_engine_config(settings_dir);
    engine_config.sources.clear();
    // Unlike headless mode, the app can ask the streamer about web sites
    // that want to use the JSON API (`AppView` shows the question).
    engine_config.ask_api_origins = true;
    let engine = runtime
        .block_on(Engine::start(engine_config))
        .map_err(StartupError::at(Stage::Server))?;
    let mut config = AppConfig::new(config_path, engine.handle().settings());
    runtime
        .block_on(config.start_all(&engine.handle(), sources))
        .map_err(StartupError::at(Stage::Other))?;
    Ok(Started {
        runtime,
        engine,
        config,
    })
}

/// Opens the main window and runs until the app quits.
fn run(started: Started) {
    let Started {
        runtime,
        engine,
        config,
    } = started;
    let handle = engine.handle();
    let tokio = runtime.handle().clone();
    let tokio_for_quit = tokio.clone();
    // `Option`, because the quit callback may in principle run more than
    // once (it's an `FnMut`), but the engine can only be shut down once.
    let mut engine = Some(engine);

    application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);

        // Closing the main window quits the app, even if the test messages
        // window is still open... The main window's id is only known once
        // it's open; `Rc<Cell>` shares that slot between this callback and
        // the task opening the window (both on the main thread).
        let main_window = Rc::new(Cell::new(None));
        let closed_main = main_window.clone();
        cx.on_window_closed(move |cx, closed| {
            if closed_main.get() == Some(closed) || cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        // ...and quitting shuts the engine down cleanly first. `shutdown()`
        // runs on the tokio runtime; GPUI just awaits its result.
        cx.on_app_quit(move |_| {
            let shutdown = engine
                .take()
                .map(|engine| tokio_for_quit.spawn(engine.shutdown()));
            async move {
                if let Some(shutdown) = shutdown {
                    let _ = shutdown.await;
                }
            }
        })
        .detach();

        // Tiling window managers (like niri) size the window themselves;
        // everywhere else it opens at this size, centred.
        let bounds = Bounds::centered(None, size(px(560.), px(640.)), cx);
        cx.spawn(async move |cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("chat-aggregator".into()),
                    ..Default::default()
                }),
                // Wayland app id: lets window managers (e.g. niri) match
                // this window in their rules.
                app_id: Some("chat-aggregator".into()),
                window_min_size: Some(size(px(420.), px(360.))),
                ..Default::default()
            };
            let window = cx
                .open_window(options, |window, cx| {
                    // The look chosen in the settings (by default: follow
                    // the system's light/dark mode, now and later).
                    appearance::apply(config.settings().app.appearance, Some(window), cx);
                    // Shared with the settings window from here on.
                    let config = cx.new(|_| config);
                    let view = cx.new(|cx| AppView::new(handle, config, tokio, window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open the window");
            main_window.set(Some(window.window_id()));
        })
        .detach();
    });
    // Only now: the engine's tasks run on this runtime until the app quits.
    drop(runtime);
}

// Explicit imports, see `startup_error::tests`.
#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::path::PathBuf;

    use super::{Stage, start};

    fn settings_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chat-app-start-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_taken_port_fails_at_the_server_step() {
        // Something else already listens on the port.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let dir = settings_dir("port");
        let config = dir.join("config.toml");
        std::fs::write(&config, format!("[server]\nport = {port}\n")).unwrap();

        let failed = start(config, &dir).err().expect("the port is taken");
        assert_eq!(failed.stage, Stage::Server);
        let kind = failed
            .error
            .chain()
            .find_map(|e| e.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind);
        assert_eq!(kind, Some(ErrorKind::AddrInUse));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_broken_config_fails_at_the_config_step_and_is_left_alone() {
        let dir = settings_dir("config");
        let config = dir.join("config.toml");
        std::fs::write(&config, "[server]\nprot = 7878\n").unwrap();

        let failed = start(config.clone(), &dir)
            .err()
            .expect("the config is broken");
        assert_eq!(failed.stage, Stage::Config);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "[server]\nprot = 7878\n",
            "nothing in the file was changed"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
