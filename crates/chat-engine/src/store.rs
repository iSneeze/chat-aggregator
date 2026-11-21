//! Profile storage, CRUD and persistence for the engine actor.
//!
//! Separated from source task supervision (`actor.rs`) so `Actor` focuses on
//! coordinating running sources and settings while this module owns the on-disk
//! layout (`config.toml` + `profiles/<name>.toml`), legacy conversion, and
//! cross-profile secret checks.
//!
//! Every filesystem call runs on Tokio's blocking thread pool
//! (`spawn_blocking`) so disk latency or antivirus scans never stall the
//! 2-thread async runtime. Because the actor awaits each method sequentially
//! inside `Actor::handle`, command-to-disk ordering stays strictly serialized.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use tracing::{info, warn};

use crate::actor::Saving;
use crate::config::SourceConfig;
use crate::config_file::{
    ConfigFile, ProfileFile, ProfileSettings, Settings, SourceEntry, find_profile_file,
    list_profiles, normalize_profile_name, same_name, save_all, sort_profiles,
};

/// Why profiles can't be managed without a settings folder.
const NO_FOLDER: &str = "there are no profiles without a settings folder";

pub(crate) struct ProfileStore {
    /// Where the settings and sources are saved (`config.toml`, and the
    /// active profile's file next to it); `None`: nowhere.
    save_to: Option<PathBuf>,
    /// The active profile: whose settings and sources are running.
    profile: String,
    /// Every profile, sorted (`Status::profiles`): those with a file at
    /// startup, the active one, and those made since.
    profiles: Vec<String>,
    /// `Status::profile_switches`.
    switches: u64,
    /// `config.toml` is still a file from before profiles: the next save
    /// converts it.
    old_layout: bool,
    /// A save did that (`Status::converted_to_profiles`).
    converted: bool,
    /// Why the last save failed, for the status; `None` once one works.
    save_error: Option<String>,
}

impl ProfileStore {
    pub(crate) fn new(saving: Saving) -> Self {
        let Saving {
            to: save_to,
            profile,
            old_layout,
        } = saving;
        let mut profiles = Vec::new();
        if let Some(dir) = save_to.as_deref().map(settings_dir) {
            profiles = list_profiles(&ConfigFile::profiles_dir(dir));
            // A new setup's has no file yet.
            if !profiles.iter().any(|name| same_name(name, &profile)) {
                profiles.push(profile.clone());
                sort_profiles(&mut profiles);
            }
        }
        Self {
            save_to,
            profile,
            profiles,
            switches: 0,
            old_layout,
            converted: false,
            save_error: None,
        }
    }

    pub(crate) fn profile(&self) -> &str {
        &self.profile
    }

    pub(crate) fn profiles(&self) -> &[String] {
        &self.profiles
    }

    pub(crate) fn switches(&self) -> u64 {
        self.switches
    }

    pub(crate) fn converted(&self) -> bool {
        self.converted
    }

    pub(crate) fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    /// Whether the engine is saving to disk and the last save succeeded.
    pub(crate) fn is_saved(&self) -> bool {
        self.save_to.is_some() && self.save_error.is_none()
    }

    /// The folder `config.toml` is in, if the engine saves.
    fn settings_dir(&self) -> anyhow::Result<PathBuf> {
        self.save_to
            .as_deref()
            .map(|p| settings_dir(p).to_path_buf())
            .context(NO_FOLDER)
    }

    /// The profile called `name` (case doesn't count), as it's spelled in
    /// the list.
    fn find_profile(&self, name: &str) -> anyhow::Result<String> {
        let name = name.trim();
        self.profiles
            .iter()
            .find(|profile| same_name(profile, name))
            .cloned()
            .ok_or_else(|| anyhow!("there's no profile named \"{name}\""))
    }

    /// Writes `config.toml` and the active profile's file on Tokio's blocking
    /// thread pool, if the engine has a place for them. Called after every
    /// change to settings or sources, never at startup (untouched files keep
    /// their comments). A failure shows in the status until a later save works.
    pub(crate) async fn save(&mut self, settings: Settings, sources: Vec<SourceEntry>) {
        let Some(path) = self.save_to.clone() else {
            return;
        };
        // Unpacked without `..`: a new part of the settings has to be placed
        // here, or this doesn't compile (and it would never be saved).
        let Settings {
            server,
            youtube,
            app,
            profile,
        } = settings;
        let config = ConfigFile {
            profile: self.profile.clone(),
            server,
            youtube,
            app,
        };
        let profile = ProfileFile::new(profile, sources);
        let old_layout = self.old_layout;
        let result =
            tokio::task::spawn_blocking(move || save_all(&path, &config, &profile, old_layout))
                .await
                .unwrap_or_else(|e| Err(anyhow!("saving settings failed: {e}")));
        self.save_error = match result {
            Ok(()) => {
                if self.old_layout {
                    info!("settings converted to profiles, the old file kept as config.old.toml");
                    self.old_layout = false;
                    self.converted = true;
                }
                None
            }
            Err(e) => {
                warn!("couldn't save the settings: {e:#}");
                Some(format!("{e:#}"))
            }
        };
    }

    /// Validates a profile switch, saves the active profile, and loads the
    /// target profile's settings and sources before the actor modifies any
    /// running state. Returns `Ok(None)` if `name` is already active.
    pub(crate) async fn prepare_switch(
        &mut self,
        name: &str,
        current_settings: Settings,
        current_sources: Vec<SourceEntry>,
    ) -> anyhow::Result<Option<(String, ProfileSettings, Vec<SourceEntry>)>> {
        let profiles_dir = ConfigFile::profiles_dir(&self.settings_dir()?);
        let name = self.find_profile(name)?;
        if name == self.profile {
            return Ok(None);
        }
        // The profile being left must be on disk: once another one is active,
        // its file is all there is of it. (It's saved after every change, but
        // not before the first: a new setup, or a file from before profiles,
        // which this converts.)
        self.save(current_settings, current_sources).await;
        if let Some(e) = &self.save_error {
            bail!("couldn't save the current profile, so it stays active: {e}");
        }
        // Read before anything changes: a broken file leaves everything as it is.
        let load_name = name.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            ProfileFile::load_or_default(&profiles_dir, &load_name)
        })
        .await
        .map_err(|e| anyhow!("loading profile failed: {e}"))??;
        let (profile, sources) = loaded.split();
        Ok(Some((name, profile, sources)))
    }

    /// Records that the actor has switched to `name`.
    pub(crate) fn commit_switch(&mut self, name: String) {
        self.profile = name;
        self.switches += 1;
    }

    /// Creates a new profile file (empty, or a copy of the active profile if
    /// `copy_from` is `Some`).
    pub(crate) async fn create(
        &mut self,
        name: &str,
        copy_from: Option<(ProfileSettings, Vec<SourceEntry>)>,
    ) -> anyhow::Result<String> {
        let settings_dir = self.settings_dir()?;
        let name = normalize_profile_name(name).map_err(anyhow::Error::msg)?;
        if self
            .profiles
            .iter()
            .any(|profile| same_name(profile, &name))
        {
            bail!("there's already a profile named \"{name}\"");
        }
        let copy = copy_from.is_some();
        let file = match copy_from {
            Some((profile, sources)) => ProfileFile::new(profile, sources),
            None => ProfileFile::default(),
        };
        let disk_name = name.clone();
        tokio::task::spawn_blocking(move || {
            // A file not in the list (made by hand since startup) counts too:
            // it isn't written over.
            if find_profile_file(&ConfigFile::profiles_dir(&settings_dir), &disk_name).is_some() {
                bail!("there's already a profile named \"{disk_name}\"");
            }
            let path = ConfigFile::profile_path(&settings_dir, &disk_name);
            file.save(&path)
        })
        .await
        .map_err(|e| anyhow!("creating profile failed: {e}"))??;
        info!(profile = %name, copy, "profile created");
        self.profiles.push(name.clone());
        sort_profiles(&mut self.profiles);
        Ok(name)
    }

    /// Renames `from` to `to`, handling both active and inactive profiles (as
    /// well as case-only renames on case-insensitive filesystems).
    pub(crate) async fn rename(
        &mut self,
        from: &str,
        to: &str,
        current_settings: Settings,
        current_sources: Vec<SourceEntry>,
    ) -> anyhow::Result<String> {
        let settings_dir = self.settings_dir()?;
        let profiles_dir = ConfigFile::profiles_dir(&settings_dir);
        let from = self.find_profile(from)?;
        let to = normalize_profile_name(to).map_err(anyhow::Error::msg)?;
        if to == from {
            return Ok(to);
        }
        let only_case = same_name(&from, &to);
        if !only_case && self.profiles.iter().any(|profile| same_name(profile, &to)) {
            bail!("there's already a profile named \"{to}\"");
        }
        let new_path = ConfigFile::profile_path(&settings_dir, &to);
        let (target_exists, old_path) = {
            let profiles_dir = profiles_dir.clone();
            let (from, to) = (from.clone(), to.clone());
            tokio::task::spawn_blocking(move || {
                let target_exists = !only_case && find_profile_file(&profiles_dir, &to).is_some();
                let old_path = find_profile_file(&profiles_dir, &from);
                (target_exists, old_path)
            })
            .await
            .map_err(|e| anyhow!("checking profile files failed: {e}"))?
        };
        if target_exists {
            bail!("{} already exists", new_path.display());
        }
        let active = from == self.profile;
        if active && !only_case {
            // Written under the new name first, then `config.toml` names it,
            // then the old file goes: stopped anywhere in between, the profile
            // is still there (twice, at worst).
            self.profile = to.clone();
            self.save(current_settings, current_sources).await;
            if let Some(e) = self.save_error.clone() {
                self.profile = from;
                bail!("couldn't save the profile under its new name: {e}");
            }
            if let Some(old_path) = old_path {
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = std::fs::remove_file(&old_path) {
                        warn!("couldn't remove {}: {e}", old_path.display());
                    }
                })
                .await;
            }
        } else {
            // Only the case changes, or the profile isn't running: renamed in
            // place. (Written anew and the old one deleted, a change of case
            // alone would delete it on Windows and macOS, where both names are
            // one file.)
            if let Some(old_path) = old_path {
                tokio::task::spawn_blocking(move || {
                    std::fs::rename(&old_path, &new_path).with_context(|| {
                        format!("renaming {} to {}", old_path.display(), new_path.display())
                    })
                })
                .await
                .map_err(|e| anyhow!("renaming profile failed: {e}"))??;
            }
            if active {
                self.profile = to.clone();
                self.save(current_settings, current_sources).await;
            }
        }
        info!(%from, %to, "profile renamed");
        self.profiles.retain(|profile| *profile != from);
        self.profiles.push(to.clone());
        sort_profiles(&mut self.profiles);
        Ok(to)
    }

    /// Deletes an inactive profile's file and returns its sources (so the
    /// caller can forget secrets no other profile uses).
    pub(crate) async fn delete(&mut self, name: &str) -> anyhow::Result<Vec<SourceConfig>> {
        let profiles_dir = ConfigFile::profiles_dir(&self.settings_dir()?);
        let name = self.find_profile(name)?;
        if name == self.profile {
            bail!("the active profile can't be deleted: switch to another one first");
        }
        let disk_name = name.clone();
        let sources = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<SourceConfig>> {
            // Its sources, for forgetting their secrets afterwards. A file that
            // can't be read is deleted all the same (you asked for it gone),
            // but whatever secrets it used stay behind.
            let sources = match ProfileFile::load(&profiles_dir, &disk_name) {
                Ok(file) => file.sources.into_iter().map(|entry| entry.config).collect(),
                Err(e) => {
                    warn!("deleting profile {disk_name}, which can't be read: {e:#}");
                    Vec::new()
                }
            };
            if let Some(path) = find_profile_file(&profiles_dir, &disk_name) {
                std::fs::remove_file(&path)
                    .with_context(|| format!("deleting {}", path.display()))?;
            }
            Ok(sources)
        })
        .await
        .map_err(|e| anyhow!("deleting profile failed: {e}"))??;
        info!(profile = %name, "profile deleted");
        self.profiles.retain(|profile| *profile != name);
        Ok(sources)
    }

    /// Whether any *inactive* profile's file uses the secret `name`. A file
    /// that can't be read might, so it counts as using it.
    pub(crate) async fn secret_in_use_in_other_profiles(&self, name: &str) -> bool {
        let Ok(settings_dir) = self.settings_dir() else {
            return false;
        };
        let profiles_dir = ConfigFile::profiles_dir(&settings_dir);
        let others: Vec<String> = self
            .profiles
            .iter()
            .filter(|profile| **profile != self.profile)
            .cloned()
            .collect();
        if others.is_empty() {
            return false;
        }
        let secret = name.to_string();
        tokio::task::spawn_blocking(move || {
            others
                .iter()
                .any(|profile| match ProfileFile::load(&profiles_dir, profile) {
                    Ok(file) => file.sources.iter().any(|entry| {
                        entry
                            .config
                            .token()
                            .is_some_and(|token| token.secret_name == secret)
                    }),
                    Err(_) => true,
                })
        })
        .await
        .unwrap_or(true)
    }
}

/// The folder `config.toml` at `path` is in.
fn settings_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}
