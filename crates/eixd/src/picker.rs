//! Picker providers served by the daemon.
//!
//! The daemon is the only component that knows a session's workspace, so it
//! answers `picker.query` and the lazy `picker.preview` reads. Frontends still
//! own fuzzy filtering and the final action.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context};
use eix_core::{
    Config, Event, EventKind, PickerItem, PickerPreviewResult, PickerResult, KIND_BRANCHES,
    KIND_FILES, KIND_GIT, KIND_GREP, KIND_IMAGES, KIND_SESSION_PREVIEW, KIND_WORKTREES,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Hard ceiling so a huge repository cannot flood the socket.
const MAX_ROWS: usize = 5_000;
/// Default when the caller does not ask for a specific count.
const DEFAULT_ROWS: u32 = 500;
/// Read at most this much of a file for the picker preview.
const PREVIEW_BYTES: usize = 64 * 1024;
/// Keep the right pane from becoming a full-file viewer.
const PREVIEW_LINES: usize = 400;
/// Avoid giant single-line files (minified JavaScript, data files, ...).
const PREVIEW_LINE_CHARS: usize = 400;
/// Skip files larger than this during content search; the picker is for
/// navigation, not for scanning multi-gigabyte logs.
const GREP_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Keep one matching line from flooding the row/detail payload.
const GREP_LINE_CHARS: usize = 400;
/// Fetch enough recent events to fill a typical preview without replaying a
/// long session from the beginning.
const SESSION_PREVIEW_EVENTS: usize = 120;
/// Cap one message so a single long answer cannot crowd out the rest.
const SESSION_PREVIEW_TEXT_CHARS: usize = 800;
const SESSION_PREVIEW_LINES: usize = 400;

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

/// Image extensions accepted by `/image` and `prompt.submit`.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// List image files under `cwd`. Images are a much smaller subset than every
/// workspace file, so the default window is wider before the frontend filters
/// locally. Row values stay workspace-relative so `/image` can attach them and
/// insert `@path` into the prompt.
pub async fn images(cwd: &Path, query: Option<&str>, limit: Option<u32>) -> PickerResult {
    let root = cwd.to_path_buf();
    let needle = lower(query);
    let limit = limit.map_or(MAX_ROWS, |value| clamp_rows(Some(value)));
    let items = tokio::task::spawn_blocking(move || walk_images(&root, needle.as_deref(), limit))
        .await
        .unwrap_or_default();
    PickerResult {
        kind: KIND_IMAGES.to_string(),
        items,
    }
}

fn walk_images(root: &Path, needle: Option<&str>, limit: usize) -> Vec<PickerItem> {
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
            if !file_type.is_file() || !is_image_file(&path) {
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
            let detail = entry.metadata().ok().map(|meta| format_size(meta.len()));
            let mut item = PickerItem::new(value);
            if let Some(detail) = detail {
                item = item.with_detail(detail);
            }
            items.push(item.with_preview(path.display().to_string()));
        }
    }
    items.sort_by(|left, right| left.value.cmp(&right.value));
    items
}

fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
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

/// Search workspace file contents for a case-insensitive literal query.
/// Empty queries intentionally return no rows: scanning an entire workspace
/// before the user has typed anything would make opening the picker expensive.
pub async fn grep(cwd: &Path, query: Option<&str>, limit: Option<u32>) -> PickerResult {
    let root = cwd.to_path_buf();
    let needle = query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase);
    let limit = clamp_rows(limit);
    let items = match needle {
        Some(needle) => tokio::task::spawn_blocking(move || search_contents(&root, &needle, limit))
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    PickerResult {
        kind: KIND_GREP.to_string(),
        items,
    }
}

fn search_contents(root: &Path, needle: &str, limit: usize) -> Vec<PickerItem> {
    let mut items = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if items.len() >= limit {
            break;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Stable order makes repeated searches and tests deterministic.
        let mut entries: Vec<_> = read.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if items.len() >= limit {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
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
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > GREP_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            // A NUL byte is a cheap binary-file signal. Lossy UTF-8 below keeps
            // the picker usable for files that are mostly text.
            if bytes.iter().take(8192).any(|byte| *byte == 0) {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            let text = String::from_utf8_lossy(&bytes);
            for (index, line) in text.lines().enumerate() {
                if !line.to_lowercase().contains(needle) {
                    continue;
                }
                let line_no = index + 1;
                let detail = truncate_chars(line.trim(), GREP_LINE_CHARS);
                let detail = if detail.is_empty() {
                    "(blank line)".to_string()
                } else {
                    detail
                };
                let value = format!("{relative}:{line_no}");
                let preview = format!("{relative}:{line_no}\n\n{detail}");
                items.push(
                    PickerItem::new(value.clone())
                        .with_label(value)
                        .with_detail(detail)
                        .with_preview(preview),
                );
                if items.len() >= limit {
                    break;
                }
            }
        }
    }
    items
}

/// Read a workspace file for the picker's right pane. The path is resolved
/// through the same sandbox as `exec_command`; the payload is capped so selecting
/// a huge file cannot stall the UI.
pub async fn file_preview(
    cwd: &Path,
    value: &str,
    config: &Config,
    limit: Option<usize>,
) -> anyhow::Result<PickerPreviewResult> {
    let path = crate::tools::resolve_path_in(cwd, value, config)
        .with_context(|| format!("cannot preview {value}"))?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("cannot stat {}", path.display()))?;
    if !metadata.is_file() {
        bail!("not a file: {value}");
    }
    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((PREVIEW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    let truncated = bytes.len() > PREVIEW_BYTES;
    bytes.truncate(PREVIEW_BYTES);
    let text = if bytes.iter().take(8192).any(|byte| *byte == 0) {
        format!("(binary file · {} bytes)", metadata.len())
    } else {
        render_file_preview(&String::from_utf8_lossy(&bytes), limit, truncated)
    };
    Ok(PickerPreviewResult {
        kind: KIND_FILES.to_string(),
        value: value.to_string(),
        text,
    })
}

fn render_file_preview(text: &str, limit: Option<usize>, truncated: bool) -> String {
    let max_lines = limit.unwrap_or(PREVIEW_LINES).clamp(1, 2_000);
    let mut out = String::new();
    let mut shown = 0usize;
    for (index, line) in text.lines().enumerate() {
        if index >= max_lines {
            break;
        }
        out.push_str(&format!(
            "{:>4} │ {}\n",
            index + 1,
            truncate_chars(line, PREVIEW_LINE_CHARS)
        ));
        shown += 1;
    }
    if shown == 0 {
        out.push_str("(empty file)\n");
    } else if truncated || text.lines().count() > shown {
        out.push_str("… [preview truncated]\n");
    }
    out.trim_end().to_string()
}

/// Render the most recent conversation events for a session picker preview.
/// The TUI shows the tail of this text, so the newest turn stays visible.
pub fn session_preview(value: &str, events: &[Event], limit: Option<usize>) -> PickerPreviewResult {
    let max_events = limit.unwrap_or(SESSION_PREVIEW_EVENTS).clamp(1, 500);
    let mut picked: Vec<&Event> = events
        .iter()
        .filter(|event| conversation_text(event).is_some())
        .collect();
    if picked.len() > max_events {
        picked.drain(..picked.len() - max_events);
    }

    let mut lines: Vec<String> = Vec::new();
    for event in picked {
        let Some((role, text)) = conversation_text(event) else {
            continue;
        };
        let text = truncate_chars(text.trim(), SESSION_PREVIEW_TEXT_CHARS);
        if text.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("{role}:"));
        let rendered = text.lines().take(16).count();
        for line in text.lines().take(16) {
            lines.push(format!("  {line}"));
        }
        if rendered < text.lines().count() {
            lines.push("  …".to_string());
        }
    }
    // Keep the newest part when the conversation is longer than the preview.
    // The TUI also tails this text, so trimming the oldest lines here preserves
    // the most relevant turn instead of the beginning of the fetched window.
    if lines.len() > SESSION_PREVIEW_LINES {
        lines.drain(..lines.len() - SESSION_PREVIEW_LINES);
        while lines.first().is_some_and(|line| line.is_empty()) {
            lines.remove(0);
        }
    }

    PickerPreviewResult {
        kind: KIND_SESSION_PREVIEW.to_string(),
        value: value.to_string(),
        text: if lines.is_empty() {
            "(no conversation yet)".to_string()
        } else {
            lines.join("\n")
        },
    }
}

fn conversation_text(event: &Event) -> Option<(&'static str, String)> {
    match event.kind {
        EventKind::UserMessage | EventKind::AssistantMessage => {
            let role = if event.kind == EventKind::UserMessage {
                "you"
            } else {
                "assistant"
            };
            Some((
                role,
                event.payload.get("text")?.as_str()?.trim().to_string(),
            ))
        }
        EventKind::ToolCall => {
            let tool = event
                .payload
                .get("tool")
                .or_else(|| event.payload.get("name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let text = match event
                .payload
                .get("arguments")
                .filter(|arguments| !arguments.is_null())
            {
                Some(arguments) => crate::tools::summarize(tool, arguments),
                None => event
                    .payload
                    .get("summary")
                    .and_then(serde_json::Value::as_str)
                    .map(|summary| format!("{tool} {summary}"))
                    .unwrap_or_else(|| tool.to_string()),
            };
            Some(("tool", text))
        }
        EventKind::Error => Some((
            "error",
            event
                .payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("error")
                .to_string(),
        )),
        EventKind::ContextCompacted => Some(("context", "compacted".to_string())),
        _ => None,
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Git worktrees reachable from `cwd`'s repository.
pub async fn worktrees(cwd: &Path, query: Option<&str>, limit: Option<u32>) -> PickerResult {
    let raw = run_git(cwd, &["worktree", "list", "--porcelain"]).await;
    let mut items = parse_worktrees(&raw);
    let needle = lower(query);
    items.retain(|item| matches(item, needle.as_deref()));
    items.truncate(clamp_rows(limit));
    PickerResult {
        kind: KIND_WORKTREES.to_string(),
        items,
    }
}

/// Worktrees first, then local branches, with the row detail making the type
/// explicit so a same-named branch and worktree stay distinguishable.
pub async fn git(cwd: &Path, query: Option<&str>, limit: Option<u32>) -> PickerResult {
    let (tree_raw, branch_raw) = tokio::join!(
        run_git(cwd, &["worktree", "list", "--porcelain"]),
        run_git(
            cwd,
            &[
                "branch",
                "--format=%(HEAD)\t%(refname:short)\t%(objectname:short)\t%(subject)",
            ],
        )
    );
    let mut items = combined_git_items(&tree_raw, &branch_raw);
    let needle = lower(query);
    items.retain(|item| matches(item, needle.as_deref()));
    items.truncate(clamp_rows(limit));
    PickerResult {
        kind: KIND_GIT.to_string(),
        items,
    }
}

fn combined_git_items(tree_raw: &str, branch_raw: &str) -> Vec<PickerItem> {
    let mut items = parse_worktrees(tree_raw);
    // `git worktree list` always includes the main checkout. It is redundant
    // in the combined picker until there is at least one linked worktree.
    if items.len() <= 1 {
        items.clear();
    }
    for item in &mut items {
        prefix_detail(item, "worktree");
    }
    for mut item in parse_branches(branch_raw) {
        prefix_detail(&mut item, "branch");
        items.push(item);
    }
    items
}

fn prefix_detail(item: &mut PickerItem, kind: &str) {
    let detail = item.detail.take().unwrap_or_default();
    item.detail = Some(if detail.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} · {detail}")
    });
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
    let raw = run_git(
        cwd,
        &[
            "branch",
            "--format=%(HEAD)\t%(refname:short)\t%(objectname:short)\t%(subject)",
        ],
    )
    .await;
    let mut items = parse_branches(&raw);
    let needle = lower(query);
    items.retain(|item| matches(item, needle.as_deref()));
    items.truncate(clamp_rows(limit));
    PickerResult {
        kind: KIND_BRANCHES.to_string(),
        items,
    }
}

fn parse_branches(raw: &str) -> Vec<PickerItem> {
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
    items
}

/// Run `git` and return stdout, or an empty string when it fails (not a repo,
/// git missing, ...). The picker then simply shows no rows.
async fn run_git(cwd: &Path, args: &[&str]) -> String {
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
    async fn lists_images_and_skips_other_files() {
        let dir = temp_dir("images");
        std::fs::create_dir_all(dir.join("shots")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("shots/one.PNG"), "png").unwrap();
        std::fs::write(dir.join("shots/two.jpeg"), "jpg").unwrap();
        std::fs::write(dir.join("notes.txt"), "text").unwrap();
        std::fs::write(dir.join("target/derived.png"), "png").unwrap();

        let result = images(&dir, None, None).await;
        assert_eq!(result.kind, KIND_IMAGES);
        let values: Vec<&str> = result.items.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(values, vec!["shots/one.PNG", "shots/two.jpeg"]);
        assert!(result.items[0].detail.as_deref().unwrap().ends_with(" B"));

        let filtered = images(&dir, Some("JPEG"), None).await;
        assert_eq!(filtered.items.len(), 1);
        assert_eq!(filtered.items[0].value, "shots/two.jpeg");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn greps_contents_and_returns_path_line_rows() {
        let dir = temp_dir("grep");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "fn main() {}\nlet needle = 1;\nNEEDLE again\n",
        )
        .unwrap();
        std::fs::write(dir.join(".git/config"), "needle").unwrap();
        std::fs::write(dir.join("target/debug"), "needle").unwrap();
        std::fs::write(
            dir.join("binary.bin"),
            [0u8, b'n', b'e', b'e', b'd', b'l', b'e'],
        )
        .unwrap();

        let result = grep(&dir, Some("needle"), None).await;
        assert_eq!(result.kind, KIND_GREP);
        let values: Vec<&str> = result
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect();
        assert_eq!(values, vec!["src/main.rs:2", "src/main.rs:3"]);
        assert!(result.items[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("needle"));
        assert!(result.items[0]
            .preview
            .as_deref()
            .unwrap()
            .contains("src/main.rs:2"));

        let limited = grep(&dir, Some("needle"), Some(1)).await;
        assert_eq!(limited.items.len(), 1);
        let empty = grep(&dir, Some("   "), None).await;
        assert!(empty.items.is_empty());
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

    #[tokio::test]
    async fn file_preview_reads_the_selected_workspace_file() {
        let dir = temp_dir("file-preview");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\nlet x = 1;\n").unwrap();

        let result = file_preview(&dir, "src/main.rs", &Config::default(), Some(1))
            .await
            .unwrap();
        assert_eq!(result.kind, KIND_FILES);
        assert_eq!(result.value, "src/main.rs");
        assert!(result.text.contains("1 │ fn main() {}"), "{}", result.text);
        assert!(result.text.contains("preview truncated"), "{}", result.text);
        assert!(!result.text.contains("let x = 1"));

        let binary = dir.join("image.bin");
        std::fs::write(&binary, [0u8, 1, 2, 3]).unwrap();
        let result = file_preview(&dir, "image.bin", &Config::default(), None)
            .await
            .unwrap();
        assert!(result.text.contains("binary file"), "{}", result.text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_preview_renders_recent_conversation() {
        let events = vec![
            Event::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({ "text": "hello" }),
            ),
            Event::new(
                "s1",
                2,
                EventKind::AssistantMessage,
                serde_json::json!({ "text": "hi there" }),
            ),
            Event::new(
                "s1",
                3,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "exec_command",
                    "arguments": { "cmd": "cat src/a.rs" }
                }),
            ),
        ];
        let preview = session_preview("s1", &events, None);
        assert_eq!(preview.kind, KIND_SESSION_PREVIEW);
        assert_eq!(preview.value, "s1");
        assert!(preview.text.contains("you:\n  hello"), "{}", preview.text);
        assert!(
            preview.text.contains("assistant:\n  hi there"),
            "{}",
            preview.text
        );
        assert!(
            preview.text.contains("tool:\n  exec_command: cat src/a.rs"),
            "{}",
            preview.text
        );

        let recent = session_preview("s1", &events, Some(1));
        assert!(!recent.text.contains("you:\n  hello"), "{}", recent.text);
        assert!(
            recent.text.contains("tool:\n  exec_command: cat src/a.rs"),
            "{}",
            recent.text
        );
    }

    #[test]
    fn session_preview_keeps_the_newest_lines_when_it_is_long() {
        let mut events = Vec::new();
        for index in 0..60 {
            let text = format!("message {index}\n{}", "body\n".repeat(8));
            events.push(Event::new(
                "s1",
                index + 1,
                EventKind::UserMessage,
                serde_json::json!({ "text": text }),
            ));
        }
        let preview = session_preview("s1", &events, None);
        assert!(preview.text.contains("message 59"), "{}", preview.text);
        assert!(!preview.text.contains("message 0\n"), "{}", preview.text);
        assert!(preview.text.lines().count() <= SESSION_PREVIEW_LINES);
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

    #[test]
    fn combined_git_omits_the_main_worktree_without_linked_ones() {
        let tree = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n";
        let branches = "*\tmain\tabc123\tinit\n";
        let items = combined_git_items(tree, branches);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "main");
        assert_eq!(items[0].detail.as_deref(), Some("branch · init"));
    }

    #[test]
    fn combined_git_keeps_the_main_worktree_when_a_linked_one_exists() {
        let tree = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\n\
                    worktree /repo/.worktrees/feat\nHEAD def456\nbranch refs/heads/eix/feat\n";
        let branches = "*\tmain\tabc123\tinit\n";
        let items = combined_git_items(tree, branches);
        assert!(items.iter().any(|item| {
            item.value == "/repo" && item.detail.as_deref() == Some("worktree · main")
        }));
        assert!(items.iter().any(|item| {
            item.value == "/repo/.worktrees/feat"
                && item.detail.as_deref() == Some("worktree · eix/feat")
        }));
    }

    #[tokio::test]
    async fn git_providers_return_empty_without_a_repo() {
        let dir = temp_dir("no-repo");
        assert!(worktrees(&dir, None, None).await.items.is_empty());
        assert!(branches(&dir, None, None).await.items.is_empty());
        assert!(git(&dir, None, None).await.items.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
