//! Picker providers served by the daemon.
//!
//! The daemon is the only component that knows a session's workspace, so it
//! answers `picker.query`. Frontends still own fuzzy filtering, previews and
//! the final action; this module only produces candidate rows.

use std::path::Path;
use std::process::Stdio;

use eix_core::{PickerItem, PickerResult, KIND_BRANCHES, KIND_FILES, KIND_WORKTREES};
use tokio::process::Command;

/// Hard ceiling so a huge repository cannot flood the socket.
const MAX_ROWS: usize = 5_000;
/// Default when the caller does not ask for a specific count.
const DEFAULT_ROWS: u32 = 500;

fn clamp_rows(limit: Option<u32>) -> usize {
    limit.unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS as u32) as usize
}

fn matches(item: &PickerItem, needle: Option<&str>) -> bool {
    match needle {
        None => true,
        Some(needle) => {
            item.label.to_lowercase().contains(needle)
                || item.value.to_lowercase().contains(needle)
                || item
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.to_lowercase().contains(needle))
        }
    }
}

fn lower(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase)
}

/// List files under `cwd`, skipping the same derived directories as snapshots.
pub async fn files(cwd: &Path, query: Option<&str>, limit: Option<u32>) -> PickerResult {
    let root = cwd.to_path_buf();
    let needle = lower(query);
    let limit = clamp_rows(limit);
    let items = tokio::task::spawn_blocking(move || walk_files(&root, needle.as_deref(), limit))
        .await
        .unwrap_or_default();
    PickerResult {
        kind: KIND_FILES.to_string(),
        items,
    }
}

fn walk_files(root: &Path, needle: Option<&str>, limit: usize) -> Vec<PickerItem> {
    let mut items = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if items.len() >= limit {
            break;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            if items.len() >= limit {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // Symlinks can point outside the workspace or form loops; the
            // picker only offers real files inside it.
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if crate::snapshot::is_excluded_dir(&entry.file_name()) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let value = relative.to_string_lossy().replace('\\', "/");
            if let Some(needle) = needle {
                if !value.to_lowercase().contains(needle) {
                    continue;
                }
            }
            items.push(PickerItem::new(value).with_preview(path.display().to_string()));
        }
    }
    items.sort_by(|left, right| left.value.cmp(&right.value));
    items
}

/// Git worktrees reachable from `cwd`'s repository.
pub async fn worktrees(cwd: &Path, query: Option<&str>, limit: Option<u32>) -> PickerResult {
    let raw = git(cwd, &["worktree", "list", "--porcelain"]).await;
    let mut items = parse_worktrees(&raw);
    let needle = lower(query);
    items.retain(|item| matches(item, needle.as_deref()));
    items.truncate(clamp_rows(limit));
    PickerResult {
        kind: KIND_WORKTREES.to_string(),
        items,
    }
}

#[derive(Default)]
struct WorktreeBlock {
    path: Option<String>,
    branch: Option<String>,
    head: Option<String>,
    detached: bool,
}

impl WorktreeBlock {
    fn finish(&mut self, items: &mut Vec<PickerItem>) {
        if let Some(path) = self.path.take() {
            let detail = if self.detached {
                "detached".to_string()
            } else {
                self.branch.take().unwrap_or_else(|| "bare".to_string())
            };
            let mut preview = format!("path    {path}");
            if !detail.is_empty() {
                preview.push_str(&format!("\nbranch  {detail}"));
            }
            if let Some(head) = self.head.take() {
                preview.push_str(&format!("\nhead    {head}"));
            }
            let label = Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            items.push(
                PickerItem::new(path)
                    .with_label(label)
                    .with_detail(detail)
                    .with_preview(preview),
            );
        }
        self.branch = None;
        self.head = None;
        self.detached = false;
    }
}

fn parse_worktrees(raw: &str) -> Vec<PickerItem> {
    let mut items = Vec::new();
    let mut block = WorktreeBlock::default();
    for line in raw.lines() {
        if line.is_empty() {
            block.finish(&mut items);
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            block.path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            block.head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            block.branch = Some(rest.trim_start_matches("refs/heads/").to_string());
        } else if line == "detached" {
            block.detached = true;
        }
    }
    block.finish(&mut items);
    items
}

/// Local branches of `cwd`'s repository.
pub async fn branches(cwd: &Path, query: Option<&str>, limit: Option<u32>) -> PickerResult {
    let raw = git(
        cwd,
        &[
            "branch",
            "--format=%(HEAD)\t%(refname:short)\t%(objectname:short)\t%(subject)",
        ],
    )
    .await;
    let mut items = Vec::new();
    for line in raw.lines() {
        let mut parts = line.splitn(4, '\t');
        let head = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let object = parts.next().unwrap_or("").trim();
        let subject = parts.next().unwrap_or("").trim();
        let current = head == "*";
        let mut preview = format!("branch  {name}\nhead    {object}");
        if !subject.is_empty() {
            preview.push_str(&format!("\nsubject {subject}"));
        }
        if current {
            preview.push_str("\n\n(current branch)");
        }
        let label = if current {
            format!("* {name}")
        } else {
            name.to_string()
        };
        items.push(
            PickerItem::new(name)
                .with_label(label)
                .with_detail(subject.to_string())
                .with_preview(preview),
        );
    }
    let needle = lower(query);
    items.retain(|item| matches(item, needle.as_deref()));
    items.truncate(clamp_rows(limit));
    PickerResult {
        kind: KIND_BRANCHES.to_string(),
        items,
    }
}

/// Run `git` and return stdout, or an empty string when it fails (not a repo,
/// git missing, ...). The picker then simply shows no rows.
async fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eix-picker-{tag}-{}-{}",
            std::process::id(),
            eix_core::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn lists_files_and_skips_derived_dirs() {
        let dir = temp_dir("files");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join(".git/config"), "secret").unwrap();
        std::fs::write(dir.join("target/debug"), "binary").unwrap();

        let result = files(&dir, None, None).await;
        let values: Vec<&str> = result.items.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(values, vec!["src/main.rs"]);
        assert_eq!(result.kind, KIND_FILES);
        assert!(result.items[0].preview.is_some());

        let filtered = files(&dir, Some("MAIN"), None).await;
        assert_eq!(filtered.items.len(), 1);
        let missing = files(&dir, Some("nope"), None).await;
        assert!(missing.items.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn limit_is_clamped_and_applied() {
        let dir = temp_dir("limit");
        for index in 0..10 {
            std::fs::write(dir.join(format!("file-{index}.txt")), "").unwrap();
        }
        let result = files(&dir, None, Some(3)).await;
        assert_eq!(result.items.len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_worktree_porcelain() {
        let raw = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\n\
                   worktree /repo/.worktrees/feat\nHEAD def456\nbranch refs/heads/eix/feat\n\n\
                   worktree /repo/detached\nHEAD 999aaa\ndetached\n";
        let items = parse_worktrees(raw);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].value, "/repo");
        assert_eq!(items[0].detail.as_deref(), Some("main"));
        assert_eq!(items[1].detail.as_deref(), Some("eix/feat"));
        assert_eq!(items[2].detail.as_deref(), Some("detached"));
    }

    #[tokio::test]
    async fn git_providers_return_empty_without_a_repo() {
        let dir = temp_dir("no-repo");
        assert!(worktrees(&dir, None, None).await.items.is_empty());
        assert!(branches(&dir, None, None).await.items.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
