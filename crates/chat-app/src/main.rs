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
use std::path::PathBuf;
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
mod youtube_panel;

use app_config::AppConfig;
use app_view::AppView;

fn main() -> anyhow::Result<()> {
    let config_path = ConfigFile::default_path()?;
    logging::init(config_path.parent());
    // An error returned from `main` is only printed to the console; log it
    // too, so a failed start (e.g. the port is taken) is in the log file.
    run(config_path).inspect_err(|e| tracing::error!("{e:#}"))
}

fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let file = ConfigFile::load_or_default(&config_path)?;

    // Two workers are plenty for chat; tokio's default is one per CPU core.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("chat-engine")
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;

    // The engine starts without sources: `AppConfig` adds them one by one
    // to learn the id the engine gives each.
    let settings_dir = config_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut engine_config = file.clone().into_engine_config(&settings_dir);
    engine_config.sources.clear();
    let engine = runtime.block_on(Engine::start(engine_config))?;
    let handle = engine.handle();
    let mut config = AppConfig::new(file, config_path);
    runtime.block_on(config.start_all(&handle))?;

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
                    appearance::apply(config.app().appearance, Some(window), cx);
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
    Ok(())
}
