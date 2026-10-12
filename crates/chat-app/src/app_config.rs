//! The windows' view of the settings. The engine owns them and saves
//! `config.toml` (`EngineHandle::settings`, `edit_settings`); `AppConfig`
//! only reads the current ones from there, so no window can work on a
//! stale copy, and knows where the app's files live.
//!
//! One `AppConfig` is shared by all windows as a GPUI `Entity`. Windows
//! that `cx.observe` it redraw whenever the settings change, whichever
//! window (or the engine itself, e.g. "Allow" adding a site) changed them.

use std::path::PathBuf;

use chat_engine::{ConfigFile, EngineHandle, Settings};
use gpui_kit::{AnyWindowHandle, App, AppContext as _, Entity};
use tokio::sync::watch;

use crate::app_view::report;

pub struct AppConfig {
    /// The engine's settings: a `watch` always holds the latest ones.
    settings: watch::Receiver<Settings>,
    /// The folder `config.toml` is in; the app's other files go next to it.
    settings_dir: PathBuf,
}

impl AppConfig {
    /// The shared entity, plus a task that notifies its observers whenever
    /// the engine's settings change. It ends with the engine (or once
    /// nothing uses the entity any more).
    pub fn new(engine: &EngineHandle, settings_dir: PathBuf, cx: &mut App) -> Entity<Self> {
        let config = cx.new(|_| Self {
            settings: engine.settings(),
            settings_dir,
        });
        let mut changes = engine.settings();
        let weak = config.downgrade();
        cx.spawn(async move |cx| {
            while changes.changed().await.is_ok() {
                if weak.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
        .detach();
        config
    }

    /// The current settings: a copy, taken out of the engine's `watch`
    /// right away (a borrow of it, kept around, would lock the engine out of
    /// changing them).
    pub fn settings(&self) -> Settings {
        self.settings.borrow().clone()
    }

    /// The folder with the overlay themes, next to the config file.
    pub fn themes_dir(&self) -> PathBuf {
        ConfigFile::themes_dir(&self.settings_dir)
    }

    /// The folder the config file lives in; other app files go next to it.
    pub fn settings_dir(&self) -> PathBuf {
        self.settings_dir.clone()
    }
}

/// Changes the engine's settings: `change` edits them (see
/// `EngineHandle::edit_settings`), and the engine applies and saves them.
/// The change is sent right away; if the engine can't take it (it has shut
/// down), that's shown in `window`, or only logged if it has closed by then
/// (the settings window applies its last edit while closing). A failed
/// save shows in the main window (`Status::save_error`).
pub fn edit_settings(
    engine: &EngineHandle,
    window: AnyWindowHandle,
    change: impl FnOnce(&mut Settings) + Send + 'static,
    cx: &mut App,
) {
    let applied = engine.edit_settings(change);
    cx.spawn(async move |cx| {
        if let Err(e) = applied.await {
            let message = format!("Couldn't change the settings: {e:#}");
            tracing::warn!("{message}");
            let _ = window.update(cx, |_, window, cx| report(window, cx, message));
        }
    })
    .detach();
}
