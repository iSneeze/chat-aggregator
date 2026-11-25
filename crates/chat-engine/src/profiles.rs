//! Profiles: named setups (sources, overlay look, tips, chat reactions,
//! widget values), each in a file of its own (`profiles/<name>.toml` next
//! to `config.toml`), one of them active. The actor does the work (it owns
//! the sources and the settings, see `actor.rs`); this is the part of
//! `EngineHandle` that asks for it. What's active and what there is shows
//! in the status (`Status::profile`, `Status::profiles`).

use crate::EngineHandle;
use crate::actor::Command;

impl EngineHandle {
    /// Makes profile `name` the active one (case doesn't count): the
    /// current one is saved, its sources stop, and the other's settings and
    /// sources take their place. Overlays reload with an empty chat. The
    /// test window's source stays. A profile whose file can't be read is an
    /// error, and nothing changes.
    pub async fn switch_profile(&self, name: &str) -> anyhow::Result<()> {
        let name = name.to_string();
        self.call(|reply| Command::SwitchProfile(name, reply)).await
    }

    /// Makes a new profile, without switching to it: empty (default
    /// settings, no sources), or with `copy` a copy of the active one.
    /// Returns its name as saved (outer spaces trimmed). A name that can't
    /// be a file's, or that's taken (whatever its case), is an error.
    pub async fn create_profile(&self, name: &str, copy: bool) -> anyhow::Result<String> {
        let name = name.to_string();
        self.call(|reply| Command::CreateProfile { name, copy, reply })
            .await
    }

    /// Renames a profile, the active one too. Returns the new name as
    /// saved.
    pub async fn rename_profile(&self, from: &str, to: &str) -> anyhow::Result<String> {
        let (from, to) = (from.to_string(), to.to_string());
        self.call(|reply| Command::RenameProfile { from, to, reply })
            .await
    }

    /// Deletes a profile's file, and the tip services' tokens only its
    /// sources used. Not the active one: switch to another first.
    pub async fn delete_profile(&self, name: &str) -> anyhow::Result<()> {
        let name = name.to_string();
        let sources = self
            .call(|reply| Command::DeleteProfile(name, reply))
            .await?;
        for config in &sources {
            self.forget_secrets_of(config).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chat_core::secrets::SecretStore;

    use crate::config_file::{ProfileFile, save_all};
    use crate::{ConfigFile, Engine, EngineHandle, Newest, SourceConfig, SourceState, Status};

    /// A settings folder of its own (`name` keeps tests apart), empty.
    fn settings_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chat-engine-profiles-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An engine started from the settings in `dir`, as the app does:
    /// saving, with a secret store in the folder.
    async fn start(dir: &Path) -> Engine {
        let path = dir.join("config.toml");
        let mut config = ConfigFile::load_or_default(&path, None)
            .unwrap()
            .into_engine_config(dir);
        config.settings.server.port = 0;
        config.save_to = Some(path);
        config.secrets = Some(SecretStore::Dir(dir.join("secrets")));
        Engine::start(config).await.unwrap()
    }

    fn twitch(channel: &str) -> SourceConfig {
        SourceConfig::Twitch {
            channel: channel.into(),
        }
    }

    async fn wait_for(handle: &EngineHandle, condition: impl Fn(&Status) -> bool) -> Status {
        let wait = handle
            .status()
            .wait_for(|s| condition(s))
            .await
            .map(|s| s.clone());
        wait.unwrap()
    }

    /// The configured sources' settings, as the status lists them.
    fn sources(status: &Status) -> Vec<SourceConfig> {
        status
            .sources
            .iter()
            .filter_map(|source| source.config.clone())
            .collect()
    }

    #[tokio::test]
    async fn switching_swaps_settings_and_sources_and_starts_the_chat_over() {
        let dir = settings_dir("switch");
        // Two profiles on disk: the active one, and one with its own look
        // and source.
        let mut main = ProfileFile::default();
        main.sources.push(twitch("your_channel").into());
        let config = ConfigFile::default();
        save_all(&dir.join("config.toml"), &config, &main, false).unwrap();
        let mut indie = ProfileFile::default();
        indie.overlay.newest = Newest::Top;
        indie.sources.push(SourceConfig::Demo.into());
        indie
            .save(&ConfigFile::profile_path(&dir, "Indie"))
            .unwrap();

        let engine = start(&dir).await;
        let handle = engine.handle();
        let status = handle.status().borrow().clone();
        assert_eq!(status.profile, "Main");
        assert_eq!(status.profiles, ["Indie", "Main"]);
        assert_eq!(sources(&status), [twitch("your_channel")]);
        // The test window's source belongs to no profile: it stays.
        let (manual, _input) = handle.add_manual_source().await.unwrap();
        // Something to clear.
        let mut msg = chat_core::demo::sample_messages(0).remove(0);
        msg.id = "LCC.ExampleMessageId0001".into();
        engine.hub().publish(chat_core::ChatEvent::Message(msg));
        assert_eq!(engine.hub().subscribe().0.len(), 1);

        handle.switch_profile("indie").await.unwrap();
        let status = wait_for(&handle, |s| s.profile == "Indie").await;
        assert_eq!(status.profile_switches, 1);
        assert_eq!(sources(&status), [SourceConfig::Demo]);
        assert!(
            status.sources.iter().any(|source| source.id == manual),
            "the test window's source stays"
        );
        assert_eq!(
            handle.settings().borrow().profile.overlay.newest,
            Newest::Top
        );
        assert!(
            engine.hub().subscribe().0.is_empty(),
            "the chat starts over"
        );
        // Saved: `config.toml` names it, and the other's file is as it was.
        let loaded = ConfigFile::load(&dir.join("config.toml"), None).unwrap();
        assert_eq!(loaded.config.profile, "Indie");
        assert_eq!(
            ProfileFile::load(&ConfigFile::profiles_dir(&dir), "Main")
                .unwrap()
                .sources,
            main.sources
        );

        // A change goes into the active profile's file only.
        handle
            .edit_settings(|s| s.profile.overlay.newest = Newest::Bottom)
            .await
            .unwrap();
        handle.switch_profile("Main").await.unwrap();
        let status = wait_for(&handle, |s| s.profile == "Main").await;
        assert_eq!(sources(&status), [twitch("your_channel")]);
        let indie = ProfileFile::load(&ConfigFile::profiles_dir(&dir), "Indie").unwrap();
        assert_eq!(indie.overlay.newest, Newest::Bottom);

        assert!(
            handle.switch_profile("Collab").await.is_err(),
            "no such profile"
        );
        engine.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_broken_profile_changes_nothing() {
        let dir = settings_dir("broken");
        let engine = start(&dir).await;
        let handle = engine.handle();
        handle
            .add_stopped_source(twitch("your_channel"))
            .await
            .unwrap();
        handle.create_profile("Indie", false).await.unwrap();
        std::fs::write(
            ConfigFile::profile_path(&dir, "Indie"),
            "[overlay]\nnewst = \"top\"",
        )
        .unwrap();

        let err = handle.switch_profile("Indie").await.unwrap_err();
        assert!(format!("{err:#}").contains("newst"), "{err:#}");
        let status = handle.status().borrow().clone();
        assert_eq!(status.profile, "Main");
        assert_eq!(sources(&status), [twitch("your_channel")]);
        engine.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn profiles_are_made_empty_or_copied_renamed_and_deleted() {
        let dir = settings_dir("manage");
        let engine = start(&dir).await;
        let handle = engine.handle();
        handle
            .add_stopped_source(twitch("your_channel"))
            .await
            .unwrap();
        handle
            .edit_settings(|s| s.profile.overlay.newest = Newest::Top)
            .await
            .unwrap();
        let profiles_dir = ConfigFile::profiles_dir(&dir);

        assert_eq!(
            handle.create_profile(" Indie ", false).await.unwrap(),
            "Indie"
        );
        assert_eq!(
            ProfileFile::load(&profiles_dir, "Indie").unwrap(),
            ProfileFile::default(),
            "empty"
        );
        handle.create_profile("Collab", true).await.unwrap();
        let collab = ProfileFile::load(&profiles_dir, "Collab").unwrap();
        assert_eq!(collab.overlay.newest, Newest::Top, "a copy");
        assert_eq!(collab.sources.len(), 1);
        assert_eq!(handle.status().borrow().profile, "Main", "not switched to");
        for taken in ["indie", "MAIN"] {
            assert!(
                handle.create_profile(taken, false).await.is_err(),
                "{taken}"
            );
        }
        assert!(handle.create_profile("a/b", false).await.is_err());
        let status = wait_for(&handle, |s| s.profiles.len() == 3).await;
        assert_eq!(status.profiles, ["Collab", "Indie", "Main"]);

        // An inactive one: its file is renamed.
        handle.rename_profile("collab", "Duo").await.unwrap();
        assert!(ProfileFile::load(&profiles_dir, "Duo").is_ok());
        assert!(!ConfigFile::profile_path(&dir, "Collab").exists());
        assert!(
            handle.rename_profile("Duo", "indie").await.is_err(),
            "taken"
        );
        // The active one: saved under the new name, and `config.toml` says
        // so.
        handle.rename_profile("Main", "Corpo").await.unwrap();
        let status = wait_for(&handle, |s| s.profile == "Corpo").await;
        assert_eq!(status.profiles, ["Corpo", "Duo", "Indie"]);
        assert_eq!(status.profile_switches, 0, "the same profile, renamed");
        assert!(!ConfigFile::profile_path(&dir, "Main").exists());
        let loaded = ConfigFile::load(&dir.join("config.toml"), None).unwrap();
        assert_eq!(loaded.config.profile, "Corpo");
        assert_eq!(loaded.profile.sources.len(), 1);
        // Only the case: still one file (on any system).
        handle.rename_profile("Corpo", "CORPO").await.unwrap();
        let names: Vec<_> = std::fs::read_dir(&profiles_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.to_lowercase() == "corpo.toml")
            .collect();
        assert_eq!(names, ["CORPO.toml"]);
        assert_eq!(
            ConfigFile::load(&dir.join("config.toml"), None)
                .unwrap()
                .config
                .profile,
            "CORPO"
        );

        assert!(
            handle.delete_profile("CORPO").await.is_err(),
            "the active one"
        );
        handle.delete_profile("duo").await.unwrap();
        assert!(!ConfigFile::profile_path(&dir, "Duo").exists());
        let status = wait_for(&handle, |s| s.profiles.len() == 2).await;
        assert_eq!(status.profiles, ["CORPO", "Indie"]);
        engine.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_token_stays_while_any_profile_uses_it() {
        let dir = settings_dir("tokens");
        let engine = start(&dir).await;
        let handle = engine.handle();
        let store = SecretStore::Dir(dir.join("secrets"));
        let tips = SourceConfig::Streamlabs {
            label: "Corpo".into(),
        };
        let name = tips.token().unwrap().secret_name;
        handle.store_token(&tips, "t0ken").await.unwrap();
        let id = handle.add_stopped_source(tips).await.unwrap();
        // A copy shares the source, and with it the token.
        handle.create_profile("Copy", true).await.unwrap();

        handle.remove_source(id).await.unwrap();
        assert!(
            store.load(&name).await.unwrap().is_some(),
            "the copy still uses it"
        );
        handle.delete_profile("Copy").await.unwrap();
        assert_eq!(store.load(&name).await.unwrap(), None, "nobody uses it now");
        engine.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_old_config_file_is_converted_by_the_first_change() {
        let dir = settings_dir("convert");
        let path = dir.join("config.toml");
        let old = "[server]\nnewest = \"top\"\n\n[[sources]]\ntype = \"demo\"\nenabled = false\n";
        std::fs::write(&path, old).unwrap();

        let engine = start(&dir).await;
        let handle = engine.handle();
        let status = handle.status().borrow().clone();
        assert_eq!(status.profile, "Main");
        assert_eq!(status.profiles, ["Main"]);
        assert!(!status.converted_to_profiles);
        assert_eq!(sources(&status), [SourceConfig::Demo]);
        assert!(matches!(status.sources[0].state, SourceState::Stopped));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            old,
            "not at startup"
        );

        handle.create_profile("Indie", false).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            old,
            "a new profile leaves config.toml alone"
        );
        handle.switch_profile("Indie").await.unwrap();
        let status = wait_for(&handle, |s| s.profile == "Indie").await;
        assert!(status.converted_to_profiles);
        assert_eq!(
            std::fs::read_to_string(dir.join("config.old.toml")).unwrap(),
            old
        );
        let main = ProfileFile::load(&ConfigFile::profiles_dir(&dir), "Main").unwrap();
        assert_eq!(main.overlay.newest, Newest::Top, "nothing lost");
        assert!(!main.sources[0].enabled);
        let loaded = ConfigFile::load(&path, None).unwrap();
        assert!(!loaded.old_layout);
        assert_eq!(loaded.config.profile, "Indie");
        engine.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
