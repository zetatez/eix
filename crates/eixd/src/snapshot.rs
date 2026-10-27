//! Pre-execution snapshots.
//!
//! Before a mutating tool runs, copy what it can change into
//! `<data>/snapshots/<session>/<snapshot>/` so a later rewind has something to
//! restore. `apply_patch` snapshots only the files it names; `exec_command`
//! snapshots the whole workspace, because a shell command can touch anything.
//!
//! Snapshots are a safety net, not a guarantee. Files over the per-file cap,
//! symlinks, and everything under the excluded directories are counted as
//! `skipped` instead of silently ignored, and a failed capture is recorded as a
//! `snapshot` event with `ok = false`. The daemon never blocks a mutation on a
//! snapshot; it makes the loss visible instead.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use eix_core::now_ms;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Keep at most this many snapshots per session; older ones are pruned.
pub const KEEP_PER_SESSION: usize = 20;
/// Files larger than this are not copied, so they are not restorable.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Stop copying once a tree snapshot reaches this size.
pub const MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024;
/// Stop after this many copied files in a tree snapshot.
pub const MAX_FILES: usize = 20_000;
/// Stop listing after this many files; the snapshot is marked incomplete and
/// restore will not delete files it did not see.
pub const MAX_ENTRIES: usize = 50_000;

/// Derived, huge, or metadata directories that are never snapshotted. Kept in
/// the manifest so restore never deletes files inside them either.
const EXCLUDED: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".cache",
    "dist",
    "build",
    ".next",
    ".tox",
    ".worktrees",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// One file (the target of a `write_file`).
    File,
    /// A recursive copy of the workspace (before a `run_command`).
    Tree,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Tree => "tree",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub scope: Scope,
    pub files: usize,
    pub bytes: u64,
    pub skipped: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: usize,
    pub removed: usize,
    /// Pre-existing files whose bytes were not captured (too large / over
    /// budget), so they could not be put back.
    pub missing: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    id: String,
    session_id: String,
    tool: String,
    call_id: String,
    scope: Scope,
    created_ms: i64,
    cwd: PathBuf,
    #[serde(default)]
    excluded: Vec<String>,
    /// False when the walk stopped early, so restore must not delete files it
    /// did not see.
    #[serde(default = "default_true")]
    complete: bool,
    files: usize,
    bytes: u64,
    skipped: usize,
    entries: Vec<Entry>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    /// False for a write target that did not exist yet: restore deletes it.
    existed: bool,
}

/// Snapshot storage rooted at `<data>/snapshots`.
#[derive(Debug, Clone)]
pub struct Snapshotter {
    root: PathBuf,
}

impl Snapshotter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding one snapshot, for tests and (later) rewind.
    pub fn dir(&self, session_id: &str, snapshot_id: &str) -> PathBuf {
        self.root.join(session_id).join(snapshot_id)
    }

    /// Copy the single file a `write_file` call is about to overwrite.
    pub async fn capture_file(
        &self,
        session_id: &str,
        tool: &str,
        call_id: &str,
        cwd: &Path,
        path: &Path,
    ) -> Result<Snapshot> {
        let root = self.root.clone();
        let session_id = session_id.to_string();
        let tool = tool.to_string();
        let call_id = call_id.to_string();
        let cwd = cwd.to_path_buf();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            capture_file_blocking(&root, &session_id, &tool, &call_id, &cwd, &path)
        })
        .await
        .context("snapshot task panicked")?
    }

    /// Copy the files an `apply_patch` call is about to change.
    pub async fn capture_files(
        &self,
        session_id: &str,
        tool: &str,
        call_id: &str,
        cwd: &Path,
        paths: &[PathBuf],
    ) -> Result<Snapshot> {
        let root = self.root.clone();
        let session_id = session_id.to_string();
        let tool = tool.to_string();
        let call_id = call_id.to_string();
        let cwd = cwd.to_path_buf();
        let paths = paths.to_vec();
        tokio::task::spawn_blocking(move || {
            capture_files_blocking(&root, &session_id, &tool, &call_id, &cwd, &paths)
        })
        .await
        .context("snapshot task panicked")?
    }

    /// Copy the whole workspace before a `run_command`.
    pub async fn capture_tree(
        &self,
        session_id: &str,
        tool: &str,
        call_id: &str,
        cwd: &Path,
    ) -> Result<Snapshot> {
        let root = self.root.clone();
        let session_id = session_id.to_string();
        let tool = tool.to_string();
        let call_id = call_id.to_string();
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || {
            capture_tree_blocking(&root, &session_id, &tool, &call_id, &cwd)
        })
        .await
        .context("snapshot task panicked")?
    }

    /// Put a snapshot back over `cwd`. For a complete tree snapshot, files that
    /// were not present when the snapshot was taken are removed.
    pub async fn restore(
        &self,
        session_id: &str,
        snapshot_id: &str,
        cwd: &Path,
    ) -> Result<RestoreReport> {
        let dir = self.dir(session_id, snapshot_id);
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || restore_blocking(&dir, &cwd))
            .await
            .context("restore task panicked")?
    }

    /// Drop all but the newest `keep` snapshots for a session.
    pub fn prune(&self, session_id: &str, keep: usize) -> usize {
        prune_dir(&self.root, session_id, keep)
    }
}

/// Copy `src` to `dst`, preferring a copy-on-write reflink. A `run_command`
/// snapshots the whole workspace, and a regular copy of every file is the
/// slowest part of the command. btrfs/XFS reflinks make it metadata-only; other
/// filesystems fall back to `std::fs::copy`.
fn copy_file(src: &Path, dst: &Path) -> std::io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        // FICLONE = _IOW(0x94, 9, int) on Linux.
        const FICLONE: libc::c_ulong = 0x4004_9409;
        if let Ok(input) = std::fs::File::open(src) {
            if let Ok(output) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dst)
            {
                let rc = unsafe { libc::ioctl(output.as_raw_fd(), FICLONE, input.as_raw_fd()) };
                if rc == 0 {
                    return std::fs::metadata(dst).map(|meta| meta.len());
                }
                // The reflink was rejected; drop the empty file so the
                // fallback starts from a clean destination.
                drop(output);
                let _ = std::fs::remove_file(dst);
            }
        }
    }
    std::fs::copy(src, dst)
}

fn capture_file_blocking(
    root: &Path,
    session_id: &str,
    tool: &str,
    call_id: &str,
    cwd: &Path,
    path: &Path,
) -> Result<Snapshot> {
    let path = path.to_path_buf();
    capture_files_blocking(
        root,
        session_id,
        tool,
        call_id,
        cwd,
        std::slice::from_ref(&path),
    )
}

fn capture_files_blocking(
    root: &Path,
    session_id: &str,
    tool: &str,
    call_id: &str,
    cwd: &Path,
    paths: &[PathBuf],
) -> Result<Snapshot> {
    let cwd = cwd.to_path_buf();
    let canon = canonical(&cwd);
    let id = Uuid::new_v4().to_string();
    let dir = root.join(session_id).join(&id);
    let files_dir = dir.join("files");
    std::fs::create_dir_all(&files_dir)
        .with_context(|| format!("cannot create {}", files_dir.display()))?;

    let mut entries = Vec::new();
    let mut files = 0;
    let mut bytes = 0;
    let mut skipped = 0;
    for path in paths {
        // `resolve_path` returns a canonicalized path for existing files but a
        // lexically normalized one for new files, so match against both the
        // given cwd and its canonical form (they differ when the session lives
        // behind a symlink).
        let rel = match path.strip_prefix(&cwd) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => path
                .strip_prefix(&canon)
                .with_context(|| format!("{} is outside {}", path.display(), canon.display()))?
                .to_path_buf(),
        };
        let metadata = std::fs::metadata(path).ok();
        let existed = metadata.is_some();
        entries.push(Entry {
            path: rel.clone(),
            existed,
        });
        match metadata {
            Some(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => {
                let target = files_dir.join(&rel);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                match copy_file(path, &target) {
                    Ok(_) => {
                        files += 1;
                        bytes += meta.len();
                    }
                    Err(_) => skipped += 1,
                }
            }
            Some(_) => skipped += 1,
            None => {}
        }
    }

    let manifest = Manifest {
        version: 1,
        id: id.clone(),
        session_id: session_id.to_string(),
        tool: tool.to_string(),
        call_id: call_id.to_string(),
        scope: Scope::File,
        created_ms: now_ms(),
        cwd: canonical(&cwd),
        excluded: Vec::new(),
        complete: true,
        files,
        bytes,
        skipped,
        entries,
    };
    write_manifest(&dir, &manifest)?;
    prune_dir(root, session_id, KEEP_PER_SESSION);
    Ok(Snapshot {
        id,
        scope: Scope::File,
        files,
        bytes,
        skipped,
    })
}

fn capture_tree_blocking(
    root: &Path,
    session_id: &str,
    tool: &str,
    call_id: &str,
    cwd: &Path,
) -> Result<Snapshot> {
    let cwd = canonical(cwd);
    let id = Uuid::new_v4().to_string();
    let dir = root.join(session_id).join(&id);
    let files_dir = dir.join("files");
    std::fs::create_dir_all(&files_dir)
        .with_context(|| format!("cannot create {}", files_dir.display()))?;

    let mut entries = Vec::new();
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut skipped = 0usize;
    let mut complete = true;

    let mut stack = vec![cwd.clone()];
    while let Some(current) = stack.pop() {
        let read = match std::fs::read_dir(&current) {
            Ok(read) => read,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let name = entry.file_name();
            if is_excluded_dir(&name) {
                continue;
            }
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if file_type.is_symlink() {
                skipped += 1;
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                skipped += 1;
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                complete = false;
                break;
            }
            let Ok(rel) = path.strip_prefix(&cwd).map(Path::to_path_buf) else {
                continue;
            };
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            entries.push(Entry {
                path: rel.clone(),
                existed: true,
            });
            if size > MAX_FILE_BYTES
                || files >= MAX_FILES
                || bytes.saturating_add(size) > MAX_TOTAL_BYTES
            {
                skipped += 1;
                continue;
            }
            let target = files_dir.join(&rel);
            let Some(parent) = target.parent() else {
                skipped += 1;
                continue;
            };
            if std::fs::create_dir_all(parent).is_err() {
                skipped += 1;
                continue;
            }
            match copy_file(&path, &target) {
                Ok(_) => {
                    files += 1;
                    bytes += size;
                }
                Err(_) => skipped += 1,
            }
        }
        if !complete {
            break;
        }
    }

    let manifest = Manifest {
        version: 1,
        id: id.clone(),
        session_id: session_id.to_string(),
        tool: tool.to_string(),
        call_id: call_id.to_string(),
        scope: Scope::Tree,
        created_ms: now_ms(),
        cwd,
        excluded: EXCLUDED.iter().map(|name| (*name).to_string()).collect(),
        complete,
        files,
        bytes,
        skipped,
        entries,
    };
    write_manifest(&dir, &manifest)?;
    prune_dir(root, session_id, KEEP_PER_SESSION);
    Ok(Snapshot {
        id,
        scope: Scope::Tree,
        files,
        bytes,
        skipped,
    })
}

fn restore_blocking(dir: &Path, cwd: &Path) -> Result<RestoreReport> {
    let manifest_path = dir.join("manifest.json");
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("cannot parse snapshot manifest")?;
    if manifest.version != 1 {
        bail!("unsupported snapshot version {}", manifest.version);
    }
    let cwd = canonical(cwd);
    let files_dir = dir.join("files");
    let mut report = RestoreReport::default();

    // Remove files that appeared after a complete tree snapshot.
    if manifest.scope == Scope::Tree && manifest.complete {
        let known: HashSet<&Path> = manifest
            .entries
            .iter()
            .map(|entry| entry.path.as_path())
            .collect();
        let mut stack = vec![cwd.clone()];
        while let Some(current) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in read.flatten() {
                if is_excluded_dir(&entry.file_name()) {
                    continue;
                }
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(&cwd).map(Path::to_path_buf) else {
                    continue;
                };
                if !known.contains(rel.as_path()) && std::fs::remove_file(&path).is_ok() {
                    report.removed += 1;
                }
            }
        }
    }

    for entry in &manifest.entries {
        let Some(rel) = safe_relative(&entry.path) else {
            continue;
        };
        let target = cwd.join(rel);
        let saved = files_dir.join(rel);
        if saved.is_file() {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(&saved, &target) {
                Ok(_) => report.restored += 1,
                Err(_) => report.missing += 1,
            }
        } else if entry.existed {
            report.missing += 1;
        } else if std::fs::remove_file(&target).is_ok() {
            report.removed += 1;
        }
    }
    Ok(report)
}

/// Snapshot paths are written by us, but never trust a manifest with `..` or an
/// absolute path.
fn safe_relative(path: &Path) -> Option<&Path> {
    if path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
    {
        return None;
    }
    Some(path)
}

fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let bytes = serde_json::to_vec(manifest)?;
    let path = dir.join("manifest.json");
    std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))
}

fn prune_dir(root: &Path, session_id: &str, keep: usize) -> usize {
    let dir = root.join(session_id);
    let Ok(read) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = read
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let meta = entry.metadata().ok()?;
            meta.is_dir()
                .then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), path))
        })
        .collect();
    dirs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut removed = 0;
    for (_, path) in dirs.into_iter().skip(keep) {
        if std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// True for derived or metadata directories that are never listed or copied
/// (`.git`, `target`, `node_modules`, ...). Shared with the file picker.
pub fn is_excluded_dir(name: &std::ffi::OsStr) -> bool {
    EXCLUDED
        .iter()
        .any(|excluded| name == std::ffi::OsStr::new(excluded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Temp {
        base: PathBuf,
        cwd: PathBuf,
        snapshots: Snapshotter,
    }

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir().join(format!(
                "eix-snapshot-{label}-{}-{nanos}",
                std::process::id()
            ));
            let cwd = base.join("ws");
            std::fs::create_dir_all(&cwd).unwrap();
            let snapshots = Snapshotter::new(base.join("snapshots"));
            Self {
                base,
                cwd,
                snapshots,
            }
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[tokio::test]
    async fn file_snapshot_restores_and_deletes_new_files() {
        let temp = Temp::new("file");
        let existing = temp.cwd.join("a.txt");
        std::fs::write(&existing, "one").unwrap();

        let first = temp
            .snapshots
            .capture_file("s1", "write_file", "call_1", &temp.cwd, &existing)
            .await
            .expect("capture existing");
        assert_eq!(first.scope, Scope::File);
        assert_eq!(first.files, 1);
        assert_eq!(first.skipped, 0);

        let created = temp.cwd.join("new.txt");
        let second = temp
            .snapshots
            .capture_file("s1", "write_file", "call_2", &temp.cwd, &created)
            .await
            .expect("capture missing");
        assert_eq!(second.files, 0);
        assert_eq!(second.skipped, 0);

        std::fs::write(&existing, "two").unwrap();
        std::fs::write(&created, "created").unwrap();

        let report = temp
            .snapshots
            .restore("s1", &second.id, &temp.cwd)
            .await
            .expect("restore new");
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(!created.exists());

        let report = temp
            .snapshots
            .restore("s1", &first.id, &temp.cwd)
            .await
            .expect("restore old");
        assert_eq!(report.restored, 1, "{report:?}");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "one");
    }

    #[tokio::test]
    async fn tree_snapshot_restores_files_and_removes_additions() {
        let temp = Temp::new("tree");
        std::fs::create_dir_all(temp.cwd.join("dir")).unwrap();
        std::fs::create_dir_all(temp.cwd.join("target")).unwrap();
        std::fs::create_dir_all(temp.cwd.join(".git")).unwrap();
        std::fs::write(temp.cwd.join("a.txt"), "one").unwrap();
        std::fs::write(temp.cwd.join("dir/b.txt"), "two").unwrap();
        std::fs::write(temp.cwd.join("target/junk"), "junk").unwrap();
        std::fs::write(temp.cwd.join(".git/config"), "git").unwrap();

        let snapshot = temp
            .snapshots
            .capture_tree("s1", "run_command", "call_1", &temp.cwd)
            .await
            .expect("capture");
        assert_eq!(snapshot.scope, Scope::Tree);
        assert_eq!(snapshot.files, 2, "excluded dirs must not be copied");
        let dir = temp.snapshots.dir("s1", &snapshot.id);
        assert!(dir.join("files/a.txt").is_file());
        assert!(!dir.join("files/target").exists());
        assert!(!dir.join("files/.git").exists());

        std::fs::write(temp.cwd.join("a.txt"), "changed").unwrap();
        std::fs::remove_file(temp.cwd.join("dir/b.txt")).unwrap();
        std::fs::write(temp.cwd.join("extra.txt"), "extra").unwrap();

        let report = temp
            .snapshots
            .restore("s1", &snapshot.id, &temp.cwd)
            .await
            .expect("restore");
        assert_eq!(report.restored, 2, "{report:?}");
        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(
            std::fs::read_to_string(temp.cwd.join("a.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(temp.cwd.join("dir/b.txt")).unwrap(),
            "two"
        );
        assert!(!temp.cwd.join("extra.txt").exists());
        // Excluded files were never touched.
        assert!(temp.cwd.join("target/junk").is_file());
        assert!(temp.cwd.join(".git/config").is_file());
    }

    #[tokio::test]
    async fn pruning_keeps_the_newest_snapshots() {
        let temp = Temp::new("prune");
        let file = temp.cwd.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let mut ids = Vec::new();
        for index in 0..3 {
            let snapshot = temp
                .snapshots
                .capture_file(
                    "s1",
                    "write_file",
                    &format!("call_{index}"),
                    &temp.cwd,
                    &file,
                )
                .await
                .expect("capture");
            ids.push(snapshot.id);
            // Distinct mtimes so the newest is unambiguous.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let removed = temp.snapshots.prune("s1", 1);
        assert_eq!(removed, 2, "kept {ids:?}");
        let dir = temp.snapshots.root().join("s1");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
    }

    #[tokio::test]
    async fn restore_rejects_paths_that_escape_the_workspace() {
        let temp = Temp::new("escape");
        let file = temp.cwd.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let snapshot = temp
            .snapshots
            .capture_file("s1", "write_file", "call_1", &temp.cwd, &file)
            .await
            .expect("capture");
        // Rewrite the manifest with a traversal entry and make sure restore
        // ignores it instead of writing outside the workspace.
        let dir = temp.snapshots.dir("s1", &snapshot.id);
        let mut manifest: Manifest =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        manifest.entries[0].path = PathBuf::from("../escape.txt");
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let report = temp
            .snapshots
            .restore("s1", &snapshot.id, &temp.cwd)
            .await
            .expect("restore");
        assert_eq!(report.restored, 0);
        assert!(!temp.base.join("escape.txt").exists());
    }

    #[tokio::test]
    async fn file_snapshot_handles_a_symlinked_cwd() {
        let temp = Temp::new("symlink");
        let real = temp.cwd.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.txt"), "one").unwrap();
        let link = temp.base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The cwd goes through the symlink, while `resolve_path` hands back the
        // canonicalized path for an existing file.
        let path = real.join("a.txt").canonicalize().unwrap();
        let snapshot = temp
            .snapshots
            .capture_file("s1", "write_file", "call_1", &link, &path)
            .await
            .expect("capture through a symlinked cwd");
        assert_eq!(snapshot.files, 1);
        let saved = temp.snapshots.dir("s1", &snapshot.id).join("files/a.txt");
        assert_eq!(std::fs::read_to_string(saved).unwrap(), "one");
    }
}
