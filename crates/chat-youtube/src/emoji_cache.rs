//! Custom emoji learned from a channel's chat, kept between runs: one file
//! per channel (`<folder>/<channel id>.json`), in the export's format.
//!
//! A file per channel, not one for all: codes are only unique within a
//! channel (two channels can each have a `:_hype:` with its own picture).
//!
//! [`BroadcastEmojis`] is what a broadcast's chat is matched against while
//! it's read: the import, the cache, and what the web chat reader learns.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::emoji::EmojiMap;
use crate::web_chat::Reader;

/// Learned emoji are saved at most this often (and once more at the end):
/// a busy chat brings many in its first minutes.
const SAVE_EVERY: Duration = Duration::from_secs(30);

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

/// The emoji of the broadcast being read. Learned emoji go into the map
/// (they win over the import: newer) and into the channel's cache.
pub(crate) struct BroadcastEmojis {
    map: EmojiMap,
    cache: Option<EmojiCache>,
    reader: Option<Reader>,
    /// The cache has emoji its file doesn't have yet.
    unsaved: bool,
    last_save: Instant,
}

impl BroadcastEmojis {
    pub(crate) fn new(map: EmojiMap, cache: Option<EmojiCache>, reader: Option<Reader>) -> Self {
        Self {
            map,
            cache,
            reader,
            unsaved: false,
            last_save: Instant::now(),
        }
    }

    /// Just `map`: nothing learned, nothing saved.
    #[cfg(test)]
    pub(crate) fn fixed(map: EmojiMap) -> Self {
        Self::new(map, None, None)
    }

    pub(crate) fn map(&self) -> &EmojiMap {
        &self.map
    }

    /// Takes in what the reader learned since the last call, without
    /// waiting for it, and saves the cache if that's due. Called before each
    /// batch of messages is converted: that's when the emoji are needed.
    pub(crate) fn take_learned(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        while let Some(batch) = reader.try_recv() {
            for (code, url) in batch {
                if let Some(cache) = &mut self.cache {
                    self.unsaved |= cache.learn(code.clone(), url.clone());
                }
                self.map.insert(code, url);
            }
        }
        if self.unsaved && self.last_save.elapsed() >= SAVE_EVERY {
            self.save();
        }
    }

    /// A small file, written at most every `SAVE_EVERY`: blocking the
    /// thread for that moment is fine (`spawn_blocking` isn't worth it).
    fn save(&mut self) {
        if let Some(cache) = &self.cache
            && let Err(e) = cache.save()
        {
            warn!("saving learned emoji: {e:#}");
        }
        self.unsaved = false;
        self.last_save = Instant::now();
    }
}

impl Drop for BroadcastEmojis {
    /// The last save. `Drop` rather than a call at the end of the run loop:
    /// it also runs when the source is switched off, which cancels the
    /// source's task mid-`await` (its variables are dropped, no code after
    /// that point runs).
    fn drop(&mut self) {
        if self.unsaved {
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
    async fn learned_emoji_are_used_at_once_and_saved_at_the_end() {
        let dir = temp_dir("learned");
        let mut import = EmojiMap::default();
        import.insert(":_hype:".into(), "https://yt3.ggpht.com/hype".into());
        let (tx, reader) = Reader::fake();
        let cache = EmojiCache::open(&dir, "UC1");
        let mut emojis = BroadcastEmojis::new(import, cache, Some(reader));

        tx.send(vec![
            (":_hype:".into(), "https://yt3.ggpht.com/hype-v2".into()),
            (":_wave:".into(), "https://yt3.ggpht.com/wave".into()),
        ])
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
        assert!(!dir.join("UC1.json").exists(), "not due yet");

        drop(emojis);
        let saved = EmojiCache::open(&dir, "UC1").unwrap();
        assert_eq!(saved.emojis().len(), 2, "saved when the broadcast ends");
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
