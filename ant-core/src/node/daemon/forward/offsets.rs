//! Persisted tail positions, so a daemon restart resumes where it left off.
//!
//! Without this the forwarder would have to choose between re-reading whole files on every start
//! (duplicating everything) and starting at the end (losing everything written while the daemon was
//! down). The acceptance criterion for V2-1021 is explicitly neither, so positions are written to
//! disk and reloaded.
//!
//! Positions are keyed by absolute log file path. ant-node rotates daily by *filename*
//! (`ant-node.2026-08-19.log`), so a new day is a new key rather than a moved cursor, and the
//! retention limit eventually deletes old ones — hence [`OffsetStore::prune`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::Result;

/// Filename of the persisted offsets within [`config::data_dir`].
const OFFSETS_FILENAME: &str = "log_forward_offsets.json";

/// Bytes of a log file that have been read and emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileOffset {
    /// Byte position immediately after the last event handed to the sink.
    pub offset: u64,
}

/// Tail positions for every log file being followed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OffsetStore {
    #[serde(default)]
    offsets: HashMap<String, FileOffset>,

    #[serde(skip)]
    path: PathBuf,

    /// Set when an offset has changed since the last successful save, so an idle forwarder does not
    /// rewrite an identical file every poll.
    #[serde(skip)]
    dirty: bool,
}

impl OffsetStore {
    /// Path of the persisted offsets for this machine.
    pub fn default_path() -> Result<PathBuf> {
        Ok(config::data_dir()?.join(OFFSETS_FILENAME))
    }

    /// Load offsets from disk, starting empty when the file is absent.
    ///
    /// A corrupt file starts empty rather than failing: unlike the opt-in config, losing positions
    /// degrades to "resume from the current end of file", which is a recoverable inconvenience
    /// rather than a silent misrepresentation of what the user consented to.
    pub fn load(path: &Path) -> Self {
        let mut store = std::fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str::<Self>(&contents).ok())
            .unwrap_or_default();
        store.path = path.to_path_buf();
        store
    }

    /// Position for a file, or `None` if it has never been read.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<u64> {
        self.offsets.get(key).map(|entry| entry.offset)
    }

    /// Record a new position.
    pub fn set(&mut self, key: &str, offset: u64) {
        let entry = self.offsets.entry(key.to_string()).or_default();
        if entry.offset != offset {
            entry.offset = offset;
            self.dirty = true;
        }
    }

    /// Forget every file not in `live`, so retention-deleted dailies do not accumulate forever.
    pub fn prune(&mut self, live: &[String]) {
        let before = self.offsets.len();
        self.offsets.retain(|key, _| live.iter().any(|k| k == key));
        if self.offsets.len() != before {
            self.dirty = true;
        }
    }

    /// Paths of every file with a recorded position.
    ///
    /// Used to tell a node being *resumed* — one whose files already have positions — from one
    /// being adopted for the first time, which must join its log at the end rather than upload the
    /// retained history.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.offsets.keys().map(String::as_str)
    }

    /// Whether anything has changed since the last successful [`Self::save`].
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Write offsets to disk atomically, if anything changed.
    ///
    /// The temporary-file-then-rename dance matters here: a half-written offsets file that parsed
    /// as valid JSON with a truncated position would replay a chunk of log on the next start.
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        let tmp_path = self.path.with_extension("tmp");
        std::fs::write(&tmp_path, &contents)?;
        std::fs::rename(&tmp_path, &self.path)?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_at(dir: &Path) -> OffsetStore {
        OffsetStore::load(&dir.join(OFFSETS_FILENAME))
    }

    #[test]
    fn absent_file_loads_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_at(tmp.path());
        assert!(store.is_empty());
        assert_eq!(store.get("anything"), None);
    }

    #[test]
    fn positions_survive_a_save_and_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_at(tmp.path());
        store.set("/logs/ant-node.2026-08-19.log", 4096);
        store.save().unwrap();

        let reloaded = store_at(tmp.path());
        assert_eq!(reloaded.get("/logs/ant-node.2026-08-19.log"), Some(4096));
    }

    #[test]
    fn a_corrupt_offsets_file_starts_empty_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(OFFSETS_FILENAME), "{{{ truncated").unwrap();
        assert!(store_at(tmp.path()).is_empty());
    }

    #[test]
    fn saving_is_skipped_while_nothing_has_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_at(tmp.path());
        assert!(!store.is_dirty());

        store.set("a", 10);
        assert!(store.is_dirty());
        store.save().unwrap();
        assert!(!store.is_dirty());

        // Setting the same value again is not a change.
        store.set("a", 10);
        assert!(!store.is_dirty());

        store.set("a", 11);
        assert!(store.is_dirty());
    }

    #[test]
    fn pruning_forgets_files_that_retention_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_at(tmp.path());
        store.set("old.log", 1);
        store.set("current.log", 2);
        store.save().unwrap();

        store.prune(&["current.log".to_string()]);

        assert_eq!(store.get("old.log"), None);
        assert_eq!(store.get("current.log"), Some(2));
        assert!(store.is_dirty(), "pruning is a change worth persisting");
    }

    #[test]
    fn keys_lists_every_tracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_at(tmp.path());
        store.set("/logs/node-1/ant-node.2026-08-19.log", 1);
        store.set("/logs/node-2/ant-node.2026-08-19.log", 2);

        let mut keys: Vec<&str> = store.keys().collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "/logs/node-1/ant-node.2026-08-19.log",
                "/logs/node-2/ant-node.2026-08-19.log"
            ]
        );
        assert!(store.keys().any(|key| key.starts_with("/logs/node-1")));
    }

    #[test]
    fn pruning_nothing_is_not_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_at(tmp.path());
        store.set("current.log", 2);
        store.save().unwrap();

        store.prune(&["current.log".to_string()]);
        assert!(!store.is_dirty());
    }
}
