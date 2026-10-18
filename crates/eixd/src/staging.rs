//! Per-session staged writes (the shadow workspace).
//!
//! A `write_file` call under the default `ask` policy does not touch the real
//! workspace. It stores the proposed bytes here and returns immediately; the
//! user later applies or rejects the patch. Reads (`read_file`, `list_dir`) and
//! subsequent writes see the staged contents overlaid on the real workspace, so
//! the agent never loses track of what it has already proposed.
//!
//! Layout: `<data>/staging/<session>/<patch_id>.json`. The file exists only
//! while the patch is pending; applying, rejecting or conflicting removes it.
//! Patches therefore survive a daemon restart and can still be answered.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use eix_core::now_ms;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedPatch {
    version: u32,
    pub patch_id: String,
    pub session_id: String,
    pub call_id: String,
    pub tool: String,
    /// Absolute path inside the workspace.
    pub path: PathBuf,
    /// Workspace-relative path used in the UI.
    pub label: String,
    /// Contents the proposal was based on (staged or on disk).
    pub old: String,
    pub new: String,
    pub created_ms: i64,
}

impl StagedPatch {
    pub fn new(
        session_id: &str,
        call_id: &str,
        tool: &str,
        path: &Path,
        label: &str,
        old: String,
        new: String,
    ) -> Self {
        Self {
            version: VERSION,
            patch_id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            path: path.to_path_buf(),
            label: label.to_string(),
            old,
            new,
            created_ms: now_ms(),
        }
    }
}

/// In-memory index over the on-disk staging directory.
pub struct Staging {
    root: PathBuf,
    pending: Mutex<HashMap<String, Vec<StagedPatch>>>,
}

impl Staging {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    fn patch_path(&self, patch: &StagedPatch) -> PathBuf {
        self.session_dir(&patch.session_id)
            .join(format!("{}.json", patch.patch_id))
    }

    /// Rebuild the index from disk. Called once at daemon startup.
    pub fn load(&self) -> usize {
        let Ok(sessions) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        let mut loaded = Vec::new();
        for session in sessions.flatten() {
            if !session
                .file_type()
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(patches) = std::fs::read_dir(session.path()) else {
                continue;
            };
            for entry in patches.flatten() {
                if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(entry.path()) else {
                    continue;
                };
                match serde_json::from_slice::<StagedPatch>(&bytes) {
                    Ok(patch) if patch.version == VERSION => loaded.push(patch),
                    _ => continue,
                }
            }
        }
        loaded.sort_by_key(|patch: &StagedPatch| patch.created_ms);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for patch in &loaded {
            pending
                .entry(patch.session_id.clone())
                .or_default()
                .push(patch.clone());
        }
        loaded.len()
    }

    /// Persist a new proposal and make it visible to reads.
    pub fn add(&self, patch: StagedPatch) -> Result<()> {
        let dir = self.session_dir(&patch.session_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = self.patch_path(&patch);
        let bytes = serde_json::to_vec(&patch)?;
        std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(patch.session_id.clone())
            .or_default()
            .push(patch);
        Ok(())
    }

    /// Replace a patch's proposed content (an edit-before-approve). Returns the
    /// updated patch, or `None` when the id is unknown.
    pub fn update_content(
        &self,
        session_id: &str,
        patch_id: &str,
        new: String,
    ) -> Result<Option<StagedPatch>> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(patches) = pending.get_mut(session_id) else {
            return Ok(None);
        };
        let Some(patch) = patches.iter_mut().find(|patch| patch.patch_id == patch_id) else {
            return Ok(None);
        };
        patch.new = new;
        patch.created_ms = now_ms();
        let updated = patch.clone();
        let path = self.patch_path(&updated);
        let bytes = serde_json::to_vec(&updated)?;
        std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(Some(updated))
    }

    /// All pending patches for a session, oldest first.
    pub fn pending(&self, session_id: &str) -> Vec<StagedPatch> {
        let mut patches = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        patches.sort_by_key(|patch| patch.created_ms);
        patches
    }

    /// Every pending patch across sessions, oldest first.
    pub fn all(&self) -> Vec<StagedPatch> {
        let mut patches: Vec<StagedPatch> = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .flat_map(|patches| patches.iter().cloned())
            .collect();
        patches.sort_by_key(|patch| patch.created_ms);
        patches
    }

    pub fn count(&self, session_id: &str) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(Vec::len)
            .unwrap_or(0)
    }

    pub fn get(&self, session_id: &str, patch_id: &str) -> Option<StagedPatch> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)?
            .iter()
            .find(|patch| patch.patch_id == patch_id)
            .cloned()
    }

    pub fn oldest(&self, session_id: &str) -> Option<StagedPatch> {
        self.pending(session_id).into_iter().next()
    }

    /// The newest staged contents for an absolute path, used as the base of
    /// subsequent reads and writes.
    pub fn latest_content(&self, session_id: &str, path: &Path) -> Option<String> {
        self.pending(session_id)
            .into_iter()
            .filter(|patch| patch.path == path)
            .max_by_key(|patch| patch.created_ms)
            .map(|patch| patch.new)
    }

    /// Remove a patch from the index *and* delete its file. Returns the patch
    /// so the caller can apply it; on failure use [`Self::restore`].
    pub fn take(&self, session_id: &str, patch_id: &str) -> Option<StagedPatch> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let patches = pending.get_mut(session_id)?;
        let index = patches
            .iter()
            .position(|patch| patch.patch_id == patch_id)?;
        let patch = patches.remove(index);
        if patches.is_empty() {
            pending.remove(session_id);
        }
        drop(pending);
        let _ = std::fs::remove_file(self.patch_path(&patch));
        Some(patch)
    }

    /// Put a taken patch back (for example when the write failed).
    pub fn restore(&self, patch: StagedPatch) -> Result<()> {
        self.add(patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Temp {
        base: PathBuf,
        staging: Staging,
    }

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir().join(format!(
                "eix-staging-{label}-{}-{nanos}",
                std::process::id()
            ));
            let staging = Staging::new(base.join("staging"));
            Self { base, staging }
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn patch(session: &str, id: &str, path: &str, old: &str, new: &str) -> StagedPatch {
        StagedPatch::new(
            session,
            &format!("call_{id}"),
            "write_file",
            Path::new("/ws").join(path).as_path(),
            path,
            old.to_string(),
            new.to_string(),
        )
    }

    #[test]
    fn add_and_reload_from_disk() {
        let temp = Temp::new("reload");
        let first = patch("s1", "1", "a.txt", "one", "two");
        temp.staging.add(first.clone()).expect("add");

        let reloaded = Staging::new(temp.base.join("staging"));
        assert_eq!(reloaded.load(), 1);
        let pending = reloaded.pending("s1");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].patch_id, first.patch_id);
        assert_eq!(pending[0].old, "one");
        assert_eq!(pending[0].new, "two");
    }

    #[test]
    fn latest_content_follows_the_newest_patch() {
        let temp = Temp::new("latest");
        let path = Path::new("/ws/a.txt");
        let mut first = patch("s1", "1", "a.txt", "one", "two");
        first.created_ms = 10;
        let mut second = patch("s1", "2", "a.txt", "two", "three");
        second.created_ms = 20;
        temp.staging.add(first).unwrap();
        temp.staging.add(second).unwrap();
        assert_eq!(
            temp.staging.latest_content("s1", path).as_deref(),
            Some("three")
        );
        assert_eq!(
            temp.staging.latest_content("s1", Path::new("/ws/b.txt")),
            None
        );
    }

    #[test]
    fn take_removes_the_file_and_restore_brings_it_back() {
        let temp = Temp::new("take");
        let staged = patch("s1", "1", "a.txt", "one", "two");
        temp.staging.add(staged.clone()).unwrap();
        let file = temp
            .base
            .join("staging/s1")
            .join(format!("{}.json", staged.patch_id));
        assert!(file.is_file());

        let taken = temp.staging.take("s1", &staged.patch_id).expect("take");
        assert!(!file.exists());
        assert!(temp.staging.pending("s1").is_empty());
        assert!(temp.staging.take("s1", &staged.patch_id).is_none());

        temp.staging.restore(taken).unwrap();
        assert!(file.is_file());
        assert_eq!(temp.staging.count("s1"), 1);
    }

    #[test]
    fn oldest_returns_creation_order_after_reload() {
        let temp = Temp::new("oldest");
        let mut first = patch("s1", "1", "a.txt", "", "one");
        first.created_ms = 10;
        let mut second = patch("s1", "2", "b.txt", "", "two");
        second.created_ms = 20;
        // Insert out of order; load sorts by created_ms.
        temp.staging.add(second).unwrap();
        temp.staging.add(first).unwrap();
        let reloaded = Staging::new(temp.base.join("staging"));
        assert_eq!(reloaded.load(), 2);
        assert_eq!(reloaded.oldest("s1").unwrap().label, "a.txt");
    }
}
