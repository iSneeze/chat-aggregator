//! The app's view of `config.toml`: keeps it and the running engine in step
//! (which config entry belongs to which `SourceId`), and saves after every
//! change a window makes (sources and their on/off switch, YouTube
//! settings, overlay theme, the settings window).
//!
//! Plain Rust without GPUI, so it's testable on its own. The windows only
//! call these methods after the engine has confirmed a change. They share
//! one `AppConfig` as a GPUI `Entity`, so none of them works on a stale copy.

use std::path::{Path, PathBuf};

use chat_engine::{
    AppSettings, ConfigFile, EngineHandle, ServerSettings, SourceConfig, SourceEntry, SourceId,
    YouTubeSettings,
};

pub struct AppConfig {
    file: ConfigFile,
    path: PathBuf,
    /// In config order, which is also the order shown in the window.
    entries: Vec<(SourceId, SourceEntry)>,
}

impl AppConfig {
    pub fn new(file: ConfigFile, path: PathBuf) -> Self {
        Self {
            file,
            path,
            entries: Vec::new(),
        }
    }

    /// Adds every source from the config file to the engine, remembering
    /// the id the engine hands out for each. Sources switched off last time
    /// are listed but not started.
    pub async fn start_all(&mut self, engine: &EngineHandle) -> anyhow::Result<()> {
        for entry in self.file.sources.clone() {
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

    pub fn server(&self) -> &ServerSettings {
        &self.file.server
    }

    /// Port, history and staggering from the settings window. The theme is
    /// part of `ServerSettings` too, but has its own setter: it's changed in
    /// the main window, so this one keeps whatever is set.
    pub fn set_server(&mut self, mut settings: ServerSettings) -> anyhow::Result<()> {
        settings.theme = self.file.server.theme.take();
        self.file.server = settings;
        self.save()
    }

    pub fn app(&self) -> &AppSettings {
        &self.file.app
    }

    pub fn set_app(&mut self, settings: AppSettings) -> anyhow::Result<()> {
        self.file.app = settings;
        self.save()
    }

    pub fn youtube(&self) -> &YouTubeSettings {
        &self.file.youtube
    }

    /// New YouTube settings (entered in the window): remember and save.
    pub fn set_youtube(&mut self, settings: YouTubeSettings) -> anyhow::Result<()> {
        self.file.youtube = settings;
        self.save()
    }

    /// The folder the config file lives in; other app files go next to it.
    pub fn themes_dir(&self) -> PathBuf {
        ConfigFile::themes_dir(&self.settings_dir())
    }

    /// The overlay theme's name; `None` = the built-in theme.
    pub fn theme(&self) -> Option<&str> {
        self.file.server.theme.as_deref()
    }

    pub fn set_theme(&mut self, theme: Option<String>) -> anyhow::Result<()> {
        self.file.server.theme = theme;
        self.save()
    }

    pub fn settings_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    pub fn config_path(&self) -> &Path {
        &self.path
    }

    fn save(&mut self) -> anyhow::Result<()> {
        self.file.sources = self.entries.iter().map(|(_, e)| e.clone()).collect();
        self.file.save(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_engine::{Engine, EngineConfig};
    use std::net::Ipv4Addr;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("chat-app-test-{}-{name}", std::process::id()))
            .join("config.toml")
    }

    #[tokio::test]
    async fn changes_are_saved_in_config_order() {
        let engine = Engine::start(EngineConfig {
            bind: (Ipv4Addr::LOCALHOST, 0).into(),
            ..EngineConfig::default()
        })
        .await
        .unwrap();
        let handle = engine.handle();

        let path = temp_path("order");
        let file = ConfigFile {
            sources: vec![SourceConfig::Demo.into()],
            ..ConfigFile::default()
        };
        let mut list = AppConfig::new(file, path.clone());
        list.start_all(&handle).await.unwrap();
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
        let start = || async {
            Engine::start(EngineConfig {
                bind: (Ipv4Addr::LOCALHOST, 0).into(),
                ..EngineConfig::default()
            })
            .await
            .unwrap()
        };
        let file = ConfigFile {
            sources: vec![SourceConfig::Demo.into()],
            ..ConfigFile::default()
        };

        // First run: switch the demo off.
        let engine = start().await;
        let mut config = AppConfig::new(file, path.clone());
        config.start_all(&engine.handle()).await.unwrap();
        let id = config.entries[0].0;
        engine.handle().stop_source(id).await.unwrap();
        config.set_enabled(id, false).unwrap();
        engine.shutdown().await;

        // Next run: it's listed, but stopped.
        let engine = start().await;
        let mut config = AppConfig::new(ConfigFile::load(&path).unwrap(), path.clone());
        config.start_all(&engine.handle()).await.unwrap();
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

    #[test]
    fn server_settings_keep_the_theme() {
        let path = temp_path("server");
        let mut config = AppConfig::new(ConfigFile::default(), path.clone());
        config.set_theme(Some("cozy".into())).unwrap();
        config
            .set_server(ServerSettings {
                history: 5,
                theme: None, // the settings window doesn't manage the theme
                ..ServerSettings::default()
            })
            .unwrap();
        let saved = ConfigFile::load(&path).unwrap();
        assert_eq!(saved.server.history, 5);
        assert_eq!(saved.server.theme.as_deref(), Some("cozy"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
