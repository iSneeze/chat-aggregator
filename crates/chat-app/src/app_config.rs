//! The app's view of `config.toml`: keeps it and the running engine in step
//! (which config entry belongs to which `SourceId`), and saves after every
//! change the window makes (sources, YouTube settings, overlay theme).
//!
//! Plain Rust without GPUI, so it's testable on its own. The window only
//! calls these methods after the engine has confirmed a change.

use std::path::{Path, PathBuf};

use chat_engine::{ConfigFile, EngineHandle, SourceConfig, SourceId, YouTubeSettings};

pub struct AppConfig {
    file: ConfigFile,
    path: PathBuf,
    /// In config order, which is also the order shown in the window.
    entries: Vec<(SourceId, SourceConfig)>,
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
    /// the id the engine hands out for each.
    pub async fn start_all(&mut self, engine: &EngineHandle) -> anyhow::Result<()> {
        for config in self.file.sources.clone() {
            let id = engine.add_source(config.clone()).await?;
            self.entries.push((id, config));
        }
        Ok(())
    }

    /// The engine added a source: remember it and save the config.
    pub fn added(&mut self, id: SourceId, config: SourceConfig) -> anyhow::Result<()> {
        self.entries.push((id, config));
        self.save()
    }

    /// The engine removed a source: forget it and save the config.
    pub fn removed(&mut self, id: SourceId) -> anyhow::Result<()> {
        self.entries.retain(|(entry, _)| *entry != id);
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
        self.file.sources = self.entries.iter().map(|(_, c)| c.clone()).collect();
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
            sources: vec![SourceConfig::Demo],
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
            [SourceConfig::Demo, twitch.clone()]
        );

        let demo_id = list.entries[0].0;
        list.removed(demo_id).unwrap();
        assert_eq!(ConfigFile::load(&path).unwrap().sources, [twitch]);

        engine.shutdown().await;
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
