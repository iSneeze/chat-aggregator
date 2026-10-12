//! The app's side of `config.toml`: which config entry belongs to which
//! running source (`SourceId`), and saving the file after every change a
//! window makes (sources and their on/off switch, and the settings).
//!
//! The settings themselves belong to the engine (`EngineHandle::settings`).
//! `AppConfig` reads them from there instead of keeping a copy, so no window
//! can work on a stale one, and windows change them with [`edit_settings`].
//! One `AppConfig` is shared by all windows as a GPUI `Entity`.

use std::path::{Path, PathBuf};

use chat_engine::{ConfigFile, EngineHandle, Settings, SourceConfig, SourceEntry, SourceId};
use gpui_kit::{AnyWindowHandle, App, Entity};
use tokio::sync::watch;

use crate::app_view::report;

pub struct AppConfig {
    path: PathBuf,
    /// The engine's settings: a `watch` always holds the latest ones.
    settings: watch::Receiver<Settings>,
    /// In config order, which is also the order shown in the window.
    entries: Vec<(SourceId, SourceEntry)>,
}

impl AppConfig {
    /// `settings`: the engine's (`EngineHandle::settings`).
    pub fn new(path: PathBuf, settings: watch::Receiver<Settings>) -> Self {
        Self {
            path,
            settings,
            entries: Vec::new(),
        }
    }

    /// Adds the sources from the config file to the engine, remembering the
    /// id the engine hands out for each. Sources switched off last time are
    /// listed but not started.
    pub async fn start_all(
        &mut self,
        engine: &EngineHandle,
        sources: Vec<SourceEntry>,
    ) -> anyhow::Result<()> {
        for entry in sources {
            let config = entry.config.clone();
            let id = if entry.enabled {
                engine.add_source(config).await?
            } else {
                engine.add_stopped_source(config).await?
            };
            self.entries.push((id, entry));
        }
        Ok(())
    }

    /// The engine added a source (switched on): remember it and save.
    pub fn added(&mut self, id: SourceId, config: SourceConfig) -> anyhow::Result<()> {
        self.entries.push((id, config.into()));
        self.save()
    }

    /// The engine removed a source: forget it and save the config.
    pub fn removed(&mut self, id: SourceId) -> anyhow::Result<()> {
        self.entries.retain(|(entry, _)| *entry != id);
        self.save()
    }

    /// The user switched a source on or off: remembered for the next start.
    /// Sources that aren't in the config (the test window's) are ignored.
    pub fn set_enabled(&mut self, id: SourceId, enabled: bool) -> anyhow::Result<()> {
        match self.entries.iter_mut().find(|(entry, _)| *entry == id) {
            Some((_, entry)) if entry.enabled != enabled => {
                entry.enabled = enabled;
                self.save()
            }
            _ => Ok(()),
        }
    }

    /// The current settings: a copy, taken out of the engine's `watch`
    /// right away (a borrow of it, kept around, would lock the engine out of
    /// changing them).
    pub fn settings(&self) -> Settings {
        self.settings.borrow().clone()
    }

    /// The folder with the overlay themes, next to the config file.
    pub fn themes_dir(&self) -> PathBuf {
        ConfigFile::themes_dir(&self.settings_dir())
    }

    /// The folder the config file lives in; other app files go next to it.
    pub fn settings_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    pub fn config_path(&self) -> &Path {
        &self.path
    }

    /// Writes `config.toml`: the engine's current settings, and the sources.
    pub fn save(&self) -> anyhow::Result<()> {
        let Settings {
            server,
            youtube,
            app,
        } = self.settings();
        let file = ConfigFile {
            server,
            youtube,
            app,
            sources: self.entries.iter().map(|(_, e)| e.clone()).collect(),
        };
        file.save(&self.path)
    }
}

/// Changes the engine's settings (`change` edits them, see
/// `EngineHandle::edit_settings`), then saves `config.toml`. Runs in the
/// background: problems are shown in `window`, or only logged if it has
/// closed by then (the settings window applies its last edit while
/// closing). Windows showing settings redraw when it's done, if they
/// `cx.observe` the config.
pub fn edit_settings(
    config: &Entity<AppConfig>,
    engine: &EngineHandle,
    window: AnyWindowHandle,
    change: impl FnOnce(&mut Settings) + Send + 'static,
    cx: &mut App,
) {
    let engine = engine.clone();
    save_after(
        config,
        window,
        async move { engine.edit_settings(change).await },
        cx,
    );
}

/// [`edit_settings`] for any engine call that changes the settings: waits
/// for `change`, then saves.
pub fn save_after(
    config: &Entity<AppConfig>,
    window: AnyWindowHandle,
    change: impl Future<Output = anyhow::Result<()>> + 'static,
    cx: &mut App,
) {
    // A strong handle: the window that asked may close meanwhile (it's
    // often the config's last owner then), and the save must still happen.
    // It's only held until the engine answers, which is quick even while
    // the app quits (a stopped engine answers with an error at once).
    let config = config.clone();
    cx.spawn(async move |cx| {
        let problem = match change.await {
            Ok(()) => config
                .update(cx, |config, cx| {
                    cx.notify();
                    config.save()
                })
                .err()
                .map(|e| format!("Couldn't save the settings: {e:#}")),
            Err(e) => Some(format!("Couldn't change the settings: {e:#}")),
        };
        if let Some(message) = problem {
            tracing::warn!("{message}");
            let _ = window.update(cx, |_, window, cx| report(window, cx, message));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_engine::{Engine, EngineConfig, Newest};

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("chat-app-test-{}-{name}", std::process::id()))
            .join("config.toml")
    }

    /// An engine on a free port (the OS picks one).
    async fn start_engine() -> Engine {
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        Engine::start(config).await.unwrap()
    }

    #[tokio::test]
    async fn changes_are_saved_in_config_order() {
        let engine = start_engine().await;
        let handle = engine.handle();

        let path = temp_path("order");
        let mut list = AppConfig::new(path.clone(), handle.settings());
        list.start_all(&handle, vec![SourceConfig::Demo.into()])
            .await
            .unwrap();
        assert_eq!(list.entries.len(), 1);

        let twitch = SourceConfig::Twitch {
            channel: "your_channel".into(),
        };
        let id = handle.add_source(twitch.clone()).await.unwrap();
        list.added(id, twitch.clone()).unwrap();
        assert_eq!(
            ConfigFile::load(&path).unwrap().sources,
            [SourceConfig::Demo.into(), twitch.clone().into()]
        );

        let demo_id = list.entries[0].0;
        list.removed(demo_id).unwrap();
        assert_eq!(ConfigFile::load(&path).unwrap().sources, [twitch.into()]);

        engine.shutdown().await;
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn switched_off_sources_stay_off_after_a_restart() {
        let path = temp_path("enabled");
        let sources = vec![SourceConfig::Demo.into()];

        // First run: switch the demo off.
        let engine = start_engine().await;
        let mut config = AppConfig::new(path.clone(), engine.handle().settings());
        config.start_all(&engine.handle(), sources).await.unwrap();
        let id = config.entries[0].0;
        engine.handle().stop_source(id).await.unwrap();
        config.set_enabled(id, false).unwrap();
        engine.shutdown().await;

        // Next run: it's listed, but stopped.
        let engine = start_engine().await;
        let mut config = AppConfig::new(path.clone(), engine.handle().settings());
        let saved = ConfigFile::load(&path).unwrap().sources;
        config.start_all(&engine.handle(), saved).await.unwrap();
        let status = engine.handle().status();
        let status = status.borrow();
        let demo = status.source(config.entries[0].0).expect("still listed");
        assert_eq!(demo.state, chat_engine::SourceState::Stopped);
        drop(status);

        // Sources that aren't in the config (the test window's) change
        // nothing.
        let (manual, _input) = engine.handle().add_manual_source().await.unwrap();
        config.set_enabled(manual, false).unwrap();
        config.set_enabled(manual, true).unwrap();
        assert!(!ConfigFile::load(&path).unwrap().sources[0].enabled);

        engine.shutdown().await;
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// The file gets the engine's settings as they are now: no copy in the
    /// app that could be behind.
    #[tokio::test]
    async fn saving_writes_the_engines_current_settings() {
        let engine = start_engine().await;
        let handle = engine.handle();
        let path = temp_path("settings");
        let config = AppConfig::new(path.clone(), handle.settings());

        handle
            .edit_settings(|s| {
                s.server.newest = Newest::Top;
                s.server.theme = Some("cozy".into());
            })
            .await
            .unwrap();
        assert_eq!(config.settings().server.newest, Newest::Top);
        config.save().unwrap();
        let saved = ConfigFile::load(&path).unwrap();
        assert_eq!(saved.server.newest, Newest::Top);
        assert_eq!(saved.server.theme.as_deref(), Some("cozy"));

        engine.shutdown().await;
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
