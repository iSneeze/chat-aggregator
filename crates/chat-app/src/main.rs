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

use anyhow::Context as _;
use chat_engine::{ConfigFile, Engine};
use gpui_kit::component::{Root, Theme};
use gpui_kit::*;

mod app_view;
mod emoji_import;
mod sources;
mod youtube_panel;

use app_view::AppView;
use sources::SourceList;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = ConfigFile::default_path()?;
    let file = ConfigFile::load_or_default(&config_path)?;

    // Two workers are plenty for chat; tokio's default is one per CPU core.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("chat-engine")
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;

    // The engine starts without sources: `SourceList` adds them one by one
    // to learn the id the engine gives each.
    let mut engine_config = file.clone().into_engine_config();
    engine_config.sources.clear();
    let engine = runtime.block_on(Engine::start(engine_config))?;
    let handle = engine.handle();
    let mut sources = SourceList::new(file, config_path);
    runtime.block_on(sources.start_all(&handle))?;

    let tokio = runtime.handle().clone();
    let tokio_for_quit = tokio.clone();
    // `Option`, because the quit callback may in principle run more than
    // once (it's an `FnMut`), but the engine can only be shut down once.
    let mut engine = Some(engine);

    application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);

        // Closing the (last) window quits the app...
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
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
            cx.open_window(options, |window, cx| {
                // Follow the system's light/dark setting, now and later.
                Theme::sync_system_appearance(Some(window), cx);
                let view = cx.new(|cx| AppView::new(handle, sources, tokio, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open the window");
        })
        .detach();
    });
    Ok(())
}
