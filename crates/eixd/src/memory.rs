//! Small, workspace-scoped notes that survive sessions.
//!
//! Memory is deliberately a plain Markdown file under the eix data directory
//! (never inside the user's repository). It is injected into the context as a
//! fixed `memory` sector entry, so compaction cannot silently discard it. The
//! model writes it through the `memory` tool; a user can inspect it at
//! `<data>/memory/<workspace-key>.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::fs;

/// Keep memory small enough to leave room for the actual conversation. The
/// model can rewrite or clear it, so this is a hard safety cap rather than a
/// policy limit.
pub const MAX_MEMORY_CHARS: usize = 6_000;

#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
    max_chars: usize,
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_chars: MAX_MEMORY_CHARS,
        }
    }

    /// Stable per-workspace file. Canonicalisation keeps `repo` and
    /// `repo/.`/symlinked spellings on the same memory.
    pub fn path_for(&self, cwd: &Path) -> PathBuf {
        let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        let key: String = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.dir.join(format!("{key}.md"))
    }

    pub async fn read(&self, cwd: &Path) -> Option<String> {
        let path = self.path_for(cwd);
        let text = fs::read_to_string(&path).await.ok()?;
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(cap_chars(text, self.max_chars))
        }
    }

    pub async fn write(&self, cwd: &Path, text: &str) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("memory text is empty");
        }
        if text.chars().count() > self.max_chars {
            anyhow::bail!(
                "memory is too large ({} chars; max {}); shorten it with action=replace or clear it first",
                text.chars().count(),
                self.max_chars
            );
        }
        fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("cannot create memory dir {}", self.dir.display()))?;
        let path = self.path_for(cwd);
        fs::write(&path, text)
            .await
            .with_context(|| format!("cannot write memory {}", path.display()))?;
        set_private_permissions(&path);
        Ok(text.to_string())
    }

    pub async fn append(&self, cwd: &Path, note: &str) -> Result<String> {
        let note = note.trim();
        if note.is_empty() {
            anyhow::bail!("memory text is empty");
        }
        let current = self.read(cwd).await.unwrap_or_default();
        let combined = if current.is_empty() {
            note.to_string()
        } else {
            format!("{current}\n\n{note}")
        };
        if combined.chars().count() > self.max_chars {
            anyhow::bail!(
                "memory is full ({} chars; max {}); use action=replace to rewrite it",
                combined.chars().count(),
                self.max_chars
            );
        }
        self.write(cwd, &combined).await
    }

    pub async fn clear(&self, cwd: &Path) -> Result<()> {
        let path = self.path_for(cwd);
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot clear memory {}", path.display())),
        }
    }
}

fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("eix-memory-{label}-{nanos}"))
    }

    #[tokio::test]
    async fn memory_is_scoped_by_workspace_and_survives_reload() {
        let root = temp_dir("scope");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let store = MemoryStore::new(root.join("state"));

        store.append(&a, "prefers small commits").await.unwrap();
        assert_eq!(
            store.read(&a).await.as_deref(),
            Some("prefers small commits")
        );
        assert!(store.read(&b).await.is_none());

        let again = MemoryStore::new(root.join("state"));
        assert_eq!(
            again.read(&a).await.as_deref(),
            Some("prefers small commits")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn oversized_memory_is_rejected_instead_of_silently_truncated() {
        let root = temp_dir("limit");
        std::fs::create_dir_all(&root).unwrap();
        let store = MemoryStore::new(root.join("state"));
        let too_long = "x".repeat(MAX_MEMORY_CHARS + 1);
        assert!(store.write(&root, &too_long).await.is_err());
        assert!(store.append(&root, &too_long).await.is_err());
        assert!(store.read(&root).await.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn append_and_clear_are_mutating_operations() {
        let root = temp_dir("mutate");
        std::fs::create_dir_all(&root).unwrap();
        let store = MemoryStore::new(root.join("state"));

        store.append(&root, "first").await.unwrap();
        let text = store.append(&root, "second").await.unwrap();
        assert_eq!(text, "first\n\nsecond");
        store.clear(&root).await.unwrap();
        assert!(store.read(&root).await.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
