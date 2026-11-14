//! The windows' view of the settings. The engine owns them and saves
//! `config.toml` (`EngineHandle::settings`, `edit_settings`); `AppConfig`
//! only reads the current ones from there, so no window can work on a
//! stale copy, and knows where the app's files live.
//!
//! One `AppConfig` is shared by all windows as a GPUI `Entity`. Windows
//! that `cx.observe` it redraw whenever the settings change, whichever
//! window (or the engine itself, e.g. "Allow" adding a site) changed them,
//! and when another profile becomes the active one ([`AppConfig::switches`])
//! or the active one is renamed.

use std::path::PathBuf;

use chat_engine::{ConfigFile, EngineHandle, Settings, Status};
use gpui_kit::{AnyWindowHandle, App, AppContext as _, Entity};
use tokio::sync::watch;

use crate::app_view::report;

pub struct AppConfig {
    /// The engine's settings: a `watch` always holds the latest ones.
    settings: watch::Receiver<Settings>,
    /// The engine's status, for the active profile.
    status: watch::Receiver<Status>,
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
            status: engine.status(),
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
        // Another profile active, or the active one renamed: said once, not
        // with every status (that changes with each message's count). The
        // engine sends the new profile's settings before the status that
        // counts the switch, so an observer that sees the new count finds
        // its settings too.
        let mut status = engine.status();
        let weak = config.downgrade();
        let profile = |status: &Status| (status.profile_switches, status.profile.clone());
        cx.spawn(async move |cx| {
            let mut shown = profile(&status.borrow_and_update());
            while status.changed().await.is_ok() {
                let current = profile(&status.borrow_and_update());
                if current == shown {
                    continue;
                }
                shown = current;
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

    /// The active profile's name: whose settings `settings` has.
    pub fn profile(&self) -> String {
        self.status.borrow().profile.clone()
    }

    /// Counts the switches to another profile (`Status::profile_switches`).
    /// A window holding a profile's settings (a draft, a form) notes this
    /// when it loads them, and loads them again when it has moved on. Not
    /// the name: renaming the active profile keeps its settings, and
    /// whatever is being typed into them.
    pub fn switches(&self) -> u64 {
        self.status.borrow().profile_switches
    }

    /// The folder with the overlay themes, next to the config file.
    pub fn themes_dir(&self) -> PathBuf {
        ConfigFile::themes_dir(&self.settings_dir)
    }

    /// The folder with the imported widgets, next to the config file.
    pub fn widgets_dir(&self) -> PathBuf {
        ConfigFile::widgets_dir(&self.settings_dir)
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
