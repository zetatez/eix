//! Built-in tools and the workspace sandbox.
//!
//! Tools receive JSON arguments from the model, never a pre-rendered command.
//! `read_file` / `list_dir` are read-only; `write_file` / `run_command` mutate
//! the workspace and therefore pass through the approval gate in `server`.
//!
//! Phase 2a executes approved writes directly. The staged shadow workspace
//! described in DESIGN.md is a later step; until then the approval card is the
//! safety boundary.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::{Config, ToolDefinition};
use serde_json::{json, Value};
use similar::TextDiff;

/// Hard cap on provider <-> tool round trips for one user turn.
pub const MAX_TOOL_ROUNDS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
    pub duration_ms: u64,
}

/// Tool definitions sent to the provider. Keep descriptions short: they are
/// paid for on every request.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            "read_file",
            "Read a UTF-8 text file inside the session workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace."}
                },
                "required": ["path"]
            }),
        ),
        ToolDefinition::function(
            "list_dir",
            "List a directory inside the session workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the workspace (default .)."}
                }
            }),
        ),
        ToolDefinition::function(
            "write_file",
            "Create or overwrite a text file inside the session workspace. Requires approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        ),
        ToolDefinition::function(
            "run_command",
            "Run a shell command in the session workspace and capture its output. Requires approval.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "description": "Optional directory relative to the workspace."}
                },
                "required": ["command"]
            }),
        ),
    ]
}

pub fn is_known(tool: &str) -> bool {
    matches!(
        tool,
        "read_file" | "list_dir" | "write_file" | "run_command"
    )
}

/// Mutating tools need an approval decision under the default `ask` policy.
pub fn requires_approval(tool: &str) -> bool {
    matches!(tool, "write_file" | "run_command")
}

/// One-line description shown in the approval card / notification.
pub fn summarize(tool: &str, args: &Value) -> String {
    match tool {
        "read_file" => format!("read_file {}", string_arg(args, "path")),
        "list_dir" => {
            let path = optional_string(args, "path").unwrap_or(".");
            format!("list_dir {path}")
        }
        "write_file" => {
            let bytes = args
                .get("content")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0);
            format!("write_file {} (+{bytes} bytes)", string_arg(args, "path"))
        }
        "run_command" => {
            let command = string_arg(args, "command");
            format!("run_command: {}", truncate(&command, 120))
        }
        other => other.to_string(),
    }
}

/// A unified diff the user can review before approving. `None` means the tool
/// has no reviewable diff (for example shell commands). This runs before
/// execution, so the shown diff is advisory: the write still happens after the
/// decision and conflict detection is not implemented yet.
pub async fn preview(tool: &str, args: &Value, cwd: &Path, config: &Config) -> Option<String> {
    match tool {
        "write_file" => match write_file_diff(args, cwd).await {
            Ok(diff) => Some(cap_output(diff, config.tools.max_output_bytes)),
            Err(err) => Some(format!("(cannot preview diff: {err:#})")),
        },
        _ => None,
    }
}

/// Unified diff of a `write_file` call against the current file contents. A
/// missing file is treated as an empty original (a pure addition).
async fn write_file_diff(args: &Value, cwd: &Path) -> Result<String> {
    let raw = required_string(args, "path")?;
    let new = required_string(args, "content")?;
    let path = resolve_path(cwd, raw)?;
    let old = match tokio::fs::read(&path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read {}", display(&path, cwd)))
        }
    };
    let label = display(&path, cwd);
    if old.len() > 2_000_000 || new.len() > 2_000_000 {
        return Ok(format!("{label}: diff omitted (file larger than 2 MB)\n"));
    }
    Ok(unified_diff(&label, &old, new))
}

/// Fingerprint of the file an approval diff was generated against. Re-checked
/// just before writing so a change made while the user reviewed is not
/// silently overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteGuard {
    path: PathBuf,
    label: String,
    hash: u64,
}

/// Capture the current contents of a `write_file` target for conflict
/// detection. `None` for tools without a file target (shell commands).
pub async fn write_guard(tool: &str, args: &Value, cwd: &Path) -> Option<WriteGuard> {
    if tool != "write_file" {
        return None;
    }
    let raw = required_string(args, "path").ok()?;
    let path = resolve_path(cwd, raw).ok()?;
    let bytes = tokio::fs::read(&path).await.unwrap_or_default();
    Some(WriteGuard {
        label: display(&path, cwd),
        path,
        hash: hash_bytes(&bytes),
    })
}

/// Fail when the guarded file changed after the diff was shown.
pub async fn verify_guard(guard: &WriteGuard) -> Result<()> {
    let bytes = tokio::fs::read(&guard.path).await.unwrap_or_default();
    if hash_bytes(&bytes) != guard.hash {
        bail!(
            "{} changed after the diff was shown; review and approve again",
            guard.label
        );
    }
    Ok(())
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let mut out = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub async fn execute(tool: &str, args: &Value, cwd: &Path, config: &Config) -> ToolOutcome {
    let started = Instant::now();
    let result = match tool {
        "read_file" => read_file(args, cwd, config).await,
        "list_dir" => list_dir(args, cwd, config).await,
        "write_file" => write_file(args, cwd, config).await,
        "run_command" => run_command(args, cwd, config).await,
        other => Err(anyhow::anyhow!("unknown tool {other:?}")),
    };
    match result {
        Ok(output) => ToolOutcome {
            ok: true,
            output,
            duration_ms: elapsed_ms(started),
        },
        Err(err) => ToolOutcome {
            ok: false,
            output: format!("{err:#}"),
            duration_ms: elapsed_ms(started),
        },
    }
}

async fn read_file(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let raw = required_string(args, "path")?;
    let path = resolve_path(cwd, raw)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read {}", display(&path, cwd)))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(cap_output(text.into_owned(), config.tools.max_output_bytes))
}

async fn list_dir(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let raw = optional_string(args, "path").unwrap_or(".");
    let path = resolve_path(cwd, raw)?;
    let mut entries = Vec::new();
    let mut dir = tokio::fs::read_dir(&path)
        .await
        .with_context(|| format!("cannot list {}", display(&path, cwd)))?;
    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let suffix = match entry.file_type().await {
            Ok(kind) if kind.is_dir() => "/",
            _ => "",
        };
        entries.push(format!("{name}{suffix}"));
        if entries.len() >= 500 {
            entries.push("… (more entries omitted)".to_string());
            break;
        }
    }
    entries.sort();
    if entries.is_empty() {
        return Ok("(empty directory)".to_string());
    }
    Ok(cap_output(
        entries.join("\n"),
        config.tools.max_output_bytes,
    ))
}

async fn write_file(args: &Value, cwd: &Path, _config: &Config) -> Result<String> {
    let raw = required_string(args, "path")?;
    let content = required_string(args, "content")?;
    let path = resolve_path(cwd, raw)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create {}", display(parent, cwd)))?;
    }
    tokio::fs::write(&path, content.as_bytes())
        .await
        .with_context(|| format!("cannot write {}", display(&path, cwd)))?;
    Ok(format!(
        "wrote {} bytes to {}",
        content.len(),
        display(&path, cwd)
    ))
}

async fn run_command(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let command = required_string(args, "command")?;
    let command_cwd = match optional_string(args, "cwd") {
        Some(raw) => resolve_path(cwd, raw)?,
        None => cwd.to_path_buf(),
    };
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&command_cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot run command in {}", command_cwd.display()))?;

    let timeout = Duration::from_secs(config.tools.command_timeout_secs);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.context("command failed")?,
        Err(_) => {
            return Ok(format!(
                "command timed out after {}s",
                config.tools.command_timeout_secs
            ));
        }
    };

    let mut text = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&stderr);
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        text.push_str(&format!("\nexit status: {code}"));
    }
    Ok(cap_output(text, config.tools.max_output_bytes))
}

/// Resolve `raw` inside `cwd`, rejecting traversal and symlink escapes.
pub fn resolve_path(cwd: &Path, raw: &str) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("path must not be empty");
    }
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    let root = cwd.canonicalize().unwrap_or_else(|_| normalize(cwd));
    let normalized = normalize(&joined);
    if !normalized.starts_with(&root) {
        if let Ok(real) = normalized.canonicalize() {
            if real.starts_with(&root) {
                return Ok(real);
            }
        }
        bail!("path escapes the session workspace: {raw}");
    }
    if let Ok(real) = normalized.canonicalize() {
        if !real.starts_with(&root) {
            bail!("path escapes the session workspace: {raw}");
        }
        return Ok(real);
    }
    // New path: the nearest existing ancestor must still be inside the root.
    let mut ancestor = normalized.parent();
    while let Some(dir) = ancestor {
        if let Ok(real) = dir.canonicalize() {
            if !real.starts_with(&root) {
                bail!("path escapes the session workspace: {raw}");
            }
            break;
        }
        ancestor = dir.parent();
    }
    Ok(normalized)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {key:?}"))
}

fn string_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn optional_string<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn cap_output(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated at {max} bytes]", &text[..end])
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_cwd(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("eix-tools-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn paths_stay_inside_the_workspace() {
        let cwd = temp_cwd("paths");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        assert_eq!(
            resolve_path(&cwd, "src/main.rs").unwrap(),
            cwd.canonicalize().unwrap().join("src/main.rs")
        );
        assert!(resolve_path(&cwd, "../escape.rs").is_err());
        assert!(resolve_path(&cwd, "/etc/passwd").is_err());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn read_list_and_write() {
        let cwd = temp_cwd("rw");
        std::fs::write(cwd.join("a.txt"), "hello").unwrap();
        let config = Config::default();

        let read = execute("read_file", &json!({"path": "a.txt"}), &cwd, &config).await;
        assert!(read.ok);
        assert_eq!(read.output, "hello");

        let listed = execute("list_dir", &json!({}), &cwd, &config).await;
        assert!(listed.ok);
        assert!(listed.output.contains("a.txt"));

        let wrote = execute(
            "write_file",
            &json!({"path": "sub/b.txt", "content": "hi"}),
            &cwd,
            &config,
        )
        .await;
        assert!(wrote.ok, "{}", wrote.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("sub/b.txt")).unwrap(),
            "hi"
        );

        let escaped = execute(
            "write_file",
            &json!({"path": "../nope.txt", "content": "x"}),
            &cwd,
            &config,
        )
        .await;
        assert!(!escaped.ok);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn preview_reports_a_unified_diff() {
        let cwd = temp_cwd("diff");
        let config = Config::default();
        std::fs::write(cwd.join("a.txt"), "one\ntwo\nthree\n").unwrap();

        let diff = preview(
            "write_file",
            &json!({"path": "a.txt", "content": "one\nTWO\nthree\n"}),
            &cwd,
            &config,
        )
        .await
        .expect("diff preview");
        assert!(diff.contains("--- a/a.txt"), "{diff}");
        assert!(diff.contains("+++ b/a.txt"), "{diff}");
        assert!(diff.contains("-two"), "{diff}");
        assert!(diff.contains("+TWO"), "{diff}");

        let added = preview(
            "write_file",
            &json!({"path": "new.txt", "content": "hello\n"}),
            &cwd,
            &config,
        )
        .await
        .expect("diff preview");
        assert!(added.contains("+hello"), "{added}");

        assert!(
            preview("run_command", &json!({"command": "true"}), &cwd, &config)
                .await
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn write_guard_detects_a_changed_file() {
        let cwd = temp_cwd("guard");
        std::fs::write(cwd.join("a.txt"), "one").unwrap();
        let guard = write_guard(
            "write_file",
            &json!({"path": "a.txt", "content": "two"}),
            &cwd,
        )
        .await
        .expect("guard");
        assert!(verify_guard(&guard).await.is_ok());

        std::fs::write(cwd.join("a.txt"), "external").unwrap();
        assert!(verify_guard(&guard).await.is_err());

        assert!(
            write_guard("run_command", &json!({"command": "true"}), &cwd)
                .await
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn command_captures_output_and_exit_status() {
        let cwd = temp_cwd("cmd");
        let config = Config::default();
        let ok = execute(
            "run_command",
            &json!({"command": "printf hi"}),
            &cwd,
            &config,
        )
        .await;
        assert!(ok.ok);
        assert_eq!(ok.output, "hi");

        let failed = execute(
            "run_command",
            &json!({"command": "echo oops >&2; exit 3"}),
            &cwd,
            &config,
        )
        .await;
        assert!(failed.ok);
        assert!(failed.output.contains("[stderr]"));
        assert!(failed.output.contains("exit status: 3"));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn summaries_name_the_target() {
        assert_eq!(
            summarize("read_file", &json!({"path": "src/a.rs"})),
            "read_file src/a.rs"
        );
        assert!(summarize("run_command", &json!({"command": "ls"})).starts_with("run_command:"));
        assert!(requires_approval("write_file"));
        assert!(!requires_approval("read_file"));
    }
}
