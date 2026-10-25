//! Content-addressed storage for prompt images.
//!
//! Images used to be embedded as base64 inside `user_message` events, which
//! made the SQLite log grow quickly. They now live under
//! `<data>/blobs/<first-two-hex>/<sha256>` and events carry only the hash.
//! The store also implements retention: only blobs that no event references
//! may be deleted, so replaying history never loses an image.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use eix_core::ImagesConfig;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Default)]
pub struct BlobGc {
    pub removed: usize,
    pub removed_bytes: u64,
    pub kept: usize,
    pub kept_bytes: u64,
}

#[derive(Debug)]
pub struct BlobStore {
    root: PathBuf,
    config: arc_swap::ArcSwap<ImagesConfig>,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>, config: ImagesConfig) -> Self {
        Self {
            root: root.into(),
            config: arc_swap::ArcSwap::from_pointee(config),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> arc_swap::Guard<Arc<ImagesConfig>> {
        self.config.load()
    }

    /// Apply image limits from a hot-reloaded config.
    pub fn set_config(&self, config: ImagesConfig) {
        self.config.store(Arc::new(config));
    }

    /// Store bytes under their SHA-256. Storing the same image twice is a
    /// no-op, which keeps screenshots and reused references cheap.
    pub async fn put(&self, bytes: &[u8]) -> Result<String> {
        let id = hex_digest(bytes);
        let path = self.path_for(&id)?;
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(id);
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        // A per-process counter avoids two concurrent writers of the same
        // content racing on one temporary path.
        static TEMP: AtomicU64 = AtomicU64::new(0);
        let temp = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = tokio::fs::File::create(&temp)
            .await
            .with_context(|| format!("cannot create {}", temp.display()))?;
        file.write_all(bytes)
            .await
            .with_context(|| format!("cannot write {}", temp.display()))?;
        file.flush().await?;
        drop(file);
        set_private(&temp)?;
        // A concurrent writer may have won the race; replacing is harmless
        // because content-addressed files are identical.
        tokio::fs::rename(&temp, &path)
            .await
            .with_context(|| format!("cannot install blob {}", path.display()))?;
        Ok(id)
    }

    pub async fn get(&self, id: &str) -> Result<Vec<u8>> {
        let path = self.path_for(id)?;
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("cannot read image blob {id}"))
    }

    pub async fn load_many(&self, ids: &HashSet<String>) -> HashMap<String, Vec<u8>> {
        let mut loaded = HashMap::new();
        for id in ids {
            if let Ok(bytes) = self.get(id).await {
                loaded.insert(id.clone(), bytes);
            }
        }
        loaded
    }

    /// Remove unreferenced blobs. `retention_days` protects recently orphaned
    /// files (for example after an undo); `max_store_bytes` applies pressure
    /// even when retention is disabled.
    pub async fn gc(&self, used: &HashSet<String>, now: SystemTime) -> Result<BlobGc> {
        let mut files = Vec::new();
        let mut total = 0u64;
        collect_files(&self.root, &mut files).await?;
        for file in &files {
            total = total.saturating_add(file.bytes);
        }

        let config = self.config.load();
        let retention = (config.retention_days > 0)
            .then(|| Duration::from_secs(config.retention_days * 86_400));
        let over_cap = config.max_store_bytes > 0 && total > config.max_store_bytes;

        // Oldest first, so a size cap evicts the least recently written orphans.
        files.sort_by_key(|file| file.modified);
        let mut removed = 0usize;
        let mut removed_bytes = 0u64;
        let mut remaining = total;
        for file in files {
            if used.contains(&file.id) {
                continue;
            }
            let expired = retention.is_some_and(|retention| {
                now.duration_since(file.modified)
                    .map(|age| age > retention)
                    .unwrap_or(false)
            });
            let pressure =
                over_cap && config.max_store_bytes > 0 && remaining > config.max_store_bytes;
            if !expired && !pressure {
                continue;
            }
            if tokio::fs::remove_file(&file.path).await.is_ok() {
                removed += 1;
                removed_bytes = removed_bytes.saturating_add(file.bytes);
                remaining = remaining.saturating_sub(file.bytes);
            }
        }
        Ok(BlobGc {
            removed,
            removed_bytes,
            kept: files_len(&self.root).await?.0.saturating_sub(removed),
            kept_bytes: total.saturating_sub(removed_bytes),
        })
    }

    pub async fn stats(&self) -> Result<(usize, u64)> {
        let mut files = Vec::new();
        collect_files(&self.root, &mut files).await?;
        Ok((
            files.len(),
            files.iter().map(|file| file.bytes).sum::<u64>(),
        ))
    }

    fn path_for(&self, id: &str) -> Result<PathBuf> {
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid image blob id {id:?}");
        }
        let id = id.to_ascii_lowercase();
        Ok(self.root.join(&id[..2]).join(id))
    }
}

struct BlobFile {
    id: String,
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

async fn collect_files(dir: &Path, out: &mut Vec<BlobFile>) -> Result<()> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Ok(());
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Ok(kind) = entry.file_type().await else {
            continue;
        };
        if kind.is_dir() {
            Box::pin(collect_files(&path, out)).await?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue; // temp files and unrelated data
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        out.push(BlobFile {
            id: id.to_ascii_lowercase(),
            path,
            bytes: meta.len(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(())
}

async fn files_len(dir: &Path) -> Result<(usize, u64)> {
    let mut files = Vec::new();
    collect_files(dir, &mut files).await?;
    Ok((files.len(), files.iter().map(|file| file.bytes).sum()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn set_private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("eix-blobs-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn stores_deduplicates_and_loads() {
        let dir = temp_dir("put");
        let store = BlobStore::new(&dir, ImagesConfig::default());
        let a = store.put(b"hello").await.unwrap();
        let b = store.put(b"hello").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(store.get(&a).await.unwrap(), b"hello");
        assert_eq!(store.stats().await.unwrap(), (1, 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gc_keeps_referenced_and_recent_blobs() {
        let dir = temp_dir("gc");
        let store = BlobStore::new(
            &dir,
            ImagesConfig {
                retention_days: 0,
                max_store_bytes: 1,
                ..ImagesConfig::default()
            },
        );
        let used = store.put(b"used").await.unwrap();
        let orphan = store.put(b"orphan").await.unwrap();
        let mut keep = HashSet::new();
        keep.insert(used.clone());
        let report = store.gc(&keep, SystemTime::now()).await.unwrap();
        assert_eq!(report.removed, 1);
        assert!(store.get(&used).await.is_ok());
        assert!(store.get(&orphan).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
