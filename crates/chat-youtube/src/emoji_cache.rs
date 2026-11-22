//! Custom emoji and custom member badges learned from a channel's web chat,
//! kept between runs in `<folder>/`:
//! - `<channel id>.json` in the emoji export format (codes are only unique
//!   within a channel)
//! - `<channel id>.badges.json` mapping member channel IDs to their latest
//!   custom loyalty badge image URL
//!
//! [`BroadcastEmojis`] is what a broadcast's chat is matched against while
//! it's read: the import, the caches, and what the web chat reader learns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::emoji::EmojiMap;
use crate::web_chat::Reader;

/// Learned emoji and badges are saved at most this often (and once more at
/// the end): a busy chat brings many in its first minutes.
const SAVE_EVERY: Duration = Duration::from_secs(30);

/// Cap on cached member badge entries per channel so a huge channel's cache
/// file stays bounded.
const MAX_CACHED_MEMBER_BADGES: usize = 5000;

pub(crate) struct EmojiCache {
    path: PathBuf,
    emojis: EmojiMap,
}

impl EmojiCache {
    /// The cache of `channel_id` in `folder`. A missing file is an empty
    /// cache, and so is a broken one (with a warning): it's only a cache,
    /// and the next save replaces it. `None` if `channel_id` doesn't look
    /// like a channel id.
    pub(crate) fn open(folder: &Path, channel_id: &str) -> Option<Self> {
        // The id becomes a file name. It comes from Google, but if it ever
        // held something like `../x`, joining it would point outside the
        // folder: only plain ids get through.
        if !is_channel_id(channel_id) {
            warn!(channel_id, "not a channel id; not caching its emoji");
            return None;
        }
        let path = folder.join(format!("{channel_id}.json"));
        let emojis = if path.exists() {
            EmojiMap::load(&path).unwrap_or_else(|e| {
                warn!("ignoring the emoji cache: {e:#}");
                EmojiMap::default()
            })
        } else {
            EmojiMap::default()
        };
        debug!(path = %path.display(), count = emojis.len(), "emoji cache opened");
        Some(Self { path, emojis })
    }

    pub(crate) fn emojis(&self) -> &EmojiMap {
        &self.emojis
    }

    /// Adds or updates one emoji in memory; `save` writes it. `true` if
    /// that changed anything (and a save is due).
    pub(crate) fn learn(&mut self, code: String, url: String) -> bool {
        self.emojis.insert(code, url)
    }

    /// Writes the cache to its file, atomically (see `write_private`): a
    /// crash mid-save leaves the previous file, never a broken one.
    pub(crate) fn save(&self) -> anyhow::Result<()> {
        chat_core::fs::write_private(&self.path, &self.emojis.to_json())
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct BadgeCacheFile {
    version: u32,
    entries: Vec<BadgeCacheEntry>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct BadgeCacheEntry {
    author_id: String,
    url: String,
}

/// Per-channel cache of `author_channel_id → custom member badge URL` stored
/// at `<folder>/<channel_id>.badges.json`.
pub(crate) struct MemberBadgeCache {
    path: PathBuf,
    badges: HashMap<String, String>,
}

impl MemberBadgeCache {
    pub(crate) fn open(folder: &Path, channel_id: &str) -> Option<Self> {
        if !is_channel_id(channel_id) {
            return None;
        }
        let path = folder.join(format!("{channel_id}.badges.json"));
        let badges = if path.exists() {
            Self::load_file(&path).unwrap_or_else(|e| {
                warn!("ignoring the member badge cache: {e:#}");
                HashMap::new()
            })
        } else {
            HashMap::new()
        };
        debug!(path = %path.display(), count = badges.len(), "member badge cache opened");
        Some(Self { path, badges })
    }

    fn load_file(path: &Path) -> anyhow::Result<HashMap<String, String>> {
        let json = std::fs::read_to_string(path)?;
        let file: BadgeCacheFile = serde_json::from_str(&json)?;
        anyhow::ensure!(
            file.version == 1,
            "unsupported member badge cache version {}",
            file.version
        );
        let mut map = HashMap::with_capacity(file.entries.len().min(MAX_CACHED_MEMBER_BADGES));
        for entry in file.entries {
            if map.len() >= MAX_CACHED_MEMBER_BADGES {
                break;
            }
            if !entry.author_id.is_empty() && entry.url.starts_with("https://") {
                map.insert(entry.author_id, entry.url);
            }
        }
        Ok(map)
    }

    pub(crate) fn badges(&self) -> &HashMap<String, String> {
        &self.badges
    }

    pub(crate) fn learn(&mut self, author_id: String, url: String) -> bool {
        if self.badges.get(&author_id) == Some(&url) {
            return false;
        }
        if !self.badges.contains_key(&author_id) && self.badges.len() >= MAX_CACHED_MEMBER_BADGES {
            return false;
        }
        self.badges.insert(author_id, url);
        true
    }

    pub(crate) fn save(&self) -> anyhow::Result<()> {
        let mut entries: Vec<_> = self
            .badges
            .iter()
            .map(|(author_id, url)| BadgeCacheEntry {
                author_id: author_id.clone(),
                url: url.clone(),
            })
            .collect();
        entries.sort_by(|a, b| a.author_id.cmp(&b.author_id));
        let json = serde_json::to_string_pretty(&BadgeCacheFile {
            version: 1,
            entries,
        })
        .expect("strings always serialize");
        chat_core::fs::write_private(&self.path, &json)
    }
}

/// The emoji and custom member badges of the broadcast being read. Learned
/// emoji go into the map (they win over the import: newer) and into the
/// channel's caches.
pub(crate) struct BroadcastEmojis {
    map: EmojiMap,
    member_badges: HashMap<String, String>,
    cache: Option<EmojiCache>,
    badge_cache: Option<MemberBadgeCache>,
    reader: Option<Reader>,
    /// The emoji cache has entries its file doesn't have yet.
    unsaved_emojis: bool,
    /// The member badge cache has entries its file doesn't have yet.
    unsaved_badges: bool,
    last_save: Instant,
}

impl BroadcastEmojis {
    pub(crate) fn new(
        map: EmojiMap,
        cache: Option<EmojiCache>,
        badge_cache: Option<MemberBadgeCache>,
        reader: Option<Reader>,
    ) -> Self {
        let member_badges = badge_cache
            .as_ref()
            .map(|c| c.badges().clone())
            .unwrap_or_default();
        Self {
            map,
            member_badges,
            cache,
            badge_cache,
            reader,
            unsaved_emojis: false,
            unsaved_badges: false,
            last_save: Instant::now(),
        }
    }

    /// Just `map`: nothing learned, nothing saved.
    #[cfg(test)]
    pub(crate) fn fixed(map: EmojiMap) -> Self {
        Self::new(map, None, None, None)
    }

    pub(crate) fn map(&self) -> &EmojiMap {
        &self.map
    }

    pub(crate) fn member_badges(&self) -> &HashMap<String, String> {
        &self.member_badges
    }

    /// Takes in what the reader learned since the last call, without
    /// waiting for it, and saves the caches if that's due. Called before each
    /// batch of messages is converted: that's when emoji and badges are needed.
    pub(crate) fn take_learned(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        while let Some(batch) = reader.try_recv() {
            for (code, url) in batch.emojis {
                if let Some(cache) = &mut self.cache {
                    self.unsaved_emojis |= cache.learn(code.clone(), url.clone());
                }
                self.map.insert(code, url);
            }
            for (author_id, url) in batch.member_badges {
                if let Some(badge_cache) = &mut self.badge_cache {
                    self.unsaved_badges |= badge_cache.learn(author_id.clone(), url.clone());
                }
                if self.member_badges.contains_key(&author_id)
                    || self.member_badges.len() < MAX_CACHED_MEMBER_BADGES
                {
                    self.member_badges.insert(author_id, url);
                }
            }
        }
        if (self.unsaved_emojis || self.unsaved_badges) && self.last_save.elapsed() >= SAVE_EVERY {
            self.save();
        }
    }

    /// Small files, written at most every `SAVE_EVERY`: blocking the thread
    /// for that moment is fine (`spawn_blocking` isn't worth it).
    fn save(&mut self) {
        if self.unsaved_emojis
            && let Some(cache) = &self.cache
            && let Err(e) = cache.save()
        {
            warn!("saving learned emoji: {e:#}");
        }
        if self.unsaved_badges
            && let Some(badge_cache) = &self.badge_cache
            && let Err(e) = badge_cache.save()
        {
            warn!("saving learned member badges: {e:#}");
        }
        self.unsaved_emojis = false;
        self.unsaved_badges = false;
        self.last_save = Instant::now();
    }
}

impl Drop for BroadcastEmojis {
    /// The last save. `Drop` rather than a call at the end of the run loop:
    /// it also runs when the source is switched off, which cancels the
    /// source's task mid-`await` (its variables are dropped, no code after
    /// that point runs).
    fn drop(&mut self) {
        if self.unsaved_emojis || self.unsaved_badges {
            self.save();
        }
    }
}

/// Channel ids are `UC` plus 22 letters, digits, `-` or `_`. Checked
/// loosely (any length up to 64) so a format change on Google's side
/// doesn't switch caching off, while `/`, `\` and `.` never pass.
fn is_channel_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chat-youtube-emoji-cache-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn learned_emoji_survive_a_restart() {
        let dir = temp_dir("restart");
        let mut cache = EmojiCache::open(&dir, "UC_channel-1").unwrap();
        assert!(cache.emojis().is_empty(), "no file yet: empty");
        assert!(cache.learn(":_hype:".into(), "https://yt3.ggpht.com/hype".into()));
        cache.save().unwrap(); // creates the folder too

        let again = EmojiCache::open(&dir, "UC_channel-1").unwrap();
        assert_eq!(again.emojis().len(), 1);
        assert!(
            EmojiCache::open(&dir, "UC_other")
                .unwrap()
                .emojis()
                .is_empty(),
            "another channel has its own file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_cache_starts_empty() {
        let dir = temp_dir("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("UC1.json"), "{ not json").unwrap();
        assert!(EmojiCache::open(&dir, "UC1").unwrap().emojis().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn learned_emoji_and_member_badges_are_used_at_once_and_saved_at_the_end() {
        let dir = temp_dir("learned");
        let mut import = EmojiMap::default();
        import.insert(":_hype:".into(), "https://yt3.ggpht.com/hype".into());
        let (tx, reader) = Reader::fake();
        let cache = EmojiCache::open(&dir, "UC1");
        let badge_cache = MemberBadgeCache::open(&dir, "UC1");
        let mut emojis = BroadcastEmojis::new(import, cache, badge_cache, Some(reader));

        tx.send(crate::web_chat::LearnedBatch {
            emojis: vec![
                (":_hype:".into(), "https://yt3.ggpht.com/hype-v2".into()),
                (":_wave:".into(), "https://yt3.ggpht.com/wave".into()),
            ],
            member_badges: vec![("UC_m1".into(), "https://yt3.ggpht.com/badge32".into())],
        })
        .await
        .unwrap();
        emojis.take_learned();
        let urls: Vec<_> = emojis
            .map()
            .find(":_hype: :_wave:")
            .emotes
            .into_iter()
            .map(|e| e.url)
            .collect();
        assert_eq!(
            urls,
            [
                "https://yt3.ggpht.com/hype-v2",
                "https://yt3.ggpht.com/wave"
            ],
            "learned ones win over the import"
        );
        assert_eq!(
            emojis.member_badges().get("UC_m1").map(String::as_str),
            Some("https://yt3.ggpht.com/badge32")
        );
        assert!(!dir.join("UC1.json").exists(), "not due yet");
        assert!(!dir.join("UC1.badges.json").exists(), "not due yet");

        drop(emojis);
        let saved = EmojiCache::open(&dir, "UC1").unwrap();
        assert_eq!(saved.emojis().len(), 2, "saved when the broadcast ends");
        let saved_badges = MemberBadgeCache::open(&dir, "UC1").unwrap();
        assert_eq!(
            saved_badges.badges().get("UC_m1").map(String::as_str),
            Some("https://yt3.ggpht.com/badge32"),
            "member badges saved when the broadcast ends"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_plain_ids_name_a_file() {
        let dir = temp_dir("ids");
        for bad in ["", "../UC1", "UC1/..", "..", "a\\b", "UC 1", "UC1.json"] {
            assert!(EmojiCache::open(&dir, bad).is_none(), "{bad:?}");
        }
        assert!(EmojiCache::open(&dir, "UCabcdefghijklmnopqrstu-_").is_some());
    }
}
