//! Durable checkpoint: per-file byte offsets plus a bounded recent-id guard.
//!
//! The offset map is what makes the daemon idempotent across restarts — we never
//! re-read committed bytes. The `recent_ids` ring is a belt-and-suspenders guard
//! against re-emitting a `message.id` after an unclean shutdown (where the offset
//! may not have been flushed). It is intentionally bounded.

use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::StateError;

const STATE_VERSION: u32 = 1;
const RECENT_IDS_CAP: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scanned: Option<String>,
}

/// On-disk checkpoint document.
#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub files: HashMap<String, FileState>,
    #[serde(default)]
    pub recent_ids: Vec<String>,

    // Derived index over `recent_ids`, not serialized.
    #[serde(skip)]
    recent_set: HashSet<String>,
    #[serde(skip)]
    recent_queue: VecDeque<String>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    dirty: bool,
}

impl State {
    /// Load state from `<state_dir>/state.json`, or start empty if absent.
    pub fn load(state_dir: &Path) -> Result<Self, StateError> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("state.json");

        let mut state = match std::fs::read(&path) {
            Ok(bytes) => {
                let mut s: State = serde_json::from_slice(&bytes)?;
                s.path = path;
                s
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                version: STATE_VERSION,
                files: HashMap::new(),
                recent_ids: Vec::new(),
                recent_set: HashSet::new(),
                recent_queue: VecDeque::new(),
                path,
                dirty: false,
            },
            Err(e) => return Err(e.into()),
        };

        // Rebuild the in-memory index from the persisted ring.
        for id in &state.recent_ids {
            state.recent_set.insert(id.clone());
            state.recent_queue.push_back(id.clone());
        }
        Ok(state)
    }

    pub fn offset_for(&self, path: &Path) -> u64 {
        self.files
            .get(&path.to_string_lossy().to_string())
            .map(|f| f.offset)
            .unwrap_or(0)
    }

    pub fn set_offset(&mut self, path: &Path, offset: u64, when: Option<String>) {
        self.files.insert(
            path.to_string_lossy().to_string(),
            FileState {
                offset,
                last_scanned: when,
            },
        );
        self.dirty = true;
    }

    /// Returns true if `id` was already recorded as emitted recently.
    pub fn seen_recent(&self, id: &str) -> bool {
        self.recent_set.contains(id)
    }

    /// Record `id` as emitted, evicting the oldest entry past the cap.
    pub fn mark_recent(&mut self, id: String) {
        if self.recent_set.insert(id.clone()) {
            self.recent_queue.push_back(id);
            while self.recent_queue.len() > RECENT_IDS_CAP {
                if let Some(old) = self.recent_queue.pop_front() {
                    self.recent_set.remove(&old);
                }
            }
            self.dirty = true;
        }
    }

    /// Atomically persist if there are unsaved changes (temp file + rename).
    pub fn save_if_dirty(&mut self) -> Result<(), StateError> {
        if !self.dirty {
            return Ok(());
        }
        self.save()
    }

    /// Atomically persist unconditionally.
    pub fn save(&mut self) -> Result<(), StateError> {
        // Sync the serializable ring from the queue (preserves eviction order).
        self.recent_ids = self.recent_queue.iter().cloned().collect();
        self.version = STATE_VERSION;

        let dir = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        std::fs::create_dir_all(&dir)?;

        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        let bytes = serde_json::to_vec_pretty(self)?;
        tmp.write_all(&bytes)?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(&self.path).map_err(|e| StateError::Io(e.error))?;

        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_offsets_and_recent_ids() {
        let dir = tempfile::tempdir().unwrap();
        let p = Path::new("/some/transcript.jsonl");

        {
            let mut s = State::load(dir.path()).unwrap();
            s.set_offset(p, 1234, Some("2026-06-16T00:00:00Z".into()));
            s.mark_recent("msg_a".into());
            s.mark_recent("msg_b".into());
            s.save().unwrap();
        }

        let s2 = State::load(dir.path()).unwrap();
        assert_eq!(s2.offset_for(p), 1234);
        assert!(s2.seen_recent("msg_a"));
        assert!(s2.seen_recent("msg_b"));
        assert!(!s2.seen_recent("msg_c"));
    }

    #[test]
    fn recent_ids_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = State::load(dir.path()).unwrap();
        for i in 0..(RECENT_IDS_CAP + 50) {
            s.mark_recent(format!("id_{i}"));
        }
        assert!(s.recent_queue.len() <= RECENT_IDS_CAP);
        // Oldest evicted, newest retained.
        assert!(!s.seen_recent("id_0"));
        assert!(s.seen_recent(&format!("id_{}", RECENT_IDS_CAP + 49)));
    }
}
