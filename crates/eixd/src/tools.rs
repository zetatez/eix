//! Built-in tools and the workspace sandbox.
//!
//! Tools receive JSON arguments from the model, never a pre-rendered command.
//! `read_file` / `list_dir` are read-only; `write_file` / `run_command` mutate
//! the workspace and therefore pass through the approval gate in `server`.
//!
//! Phase 2a executes approved writes directly. The staged shadow workspace
//! described in DESIGN.md is a later step; until then the approval card is the
//! safety boundary.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::{sandbox, Config, HunkInfo, SandboxMode, ToolDefinition};
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

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
        ToolDefinition::function(
            "start_command",
            "Start a long-running command (dev server, watch task) in the background. Returns a process id immediately; output is captured in a log. Requires approval.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "description": "Optional directory relative to the workspace."}
                },
                "required": ["command"]
            }),
        ),
        ToolDefinition::function(
            "process_output",
            "Read the recent output of a background process started with start_command. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "process_id": {"type": "string", "description": "Process id. Defaults to the newest process in this session."},
                    "max_bytes": {"type": "integer", "description": "How many trailing bytes to return (default 8000)."}
                }
            }),
        ),
        ToolDefinition::function(
            "spawn_agent",
            "Start a read-only sub-agent on a focused question and return its final answer. Sub-agents share this workspace, cannot mutate files, and cannot spawn further agents. Use it to explore several independent questions at once.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The task for the sub-agent."},
                    "name": {"type": "string", "description": "Optional short label, e.g. `docs`."}
                },
                "required": ["prompt"]
            }),
        ),
    ]
}

/// Tools a sub-agent may use. No mutating tools and no nested agents, so a
/// child can never widen the parent's permissions or change the workspace.
pub fn readonly_definitions() -> Vec<ToolDefinition> {
    definitions()
        .into_iter()
        .filter(|tool| {
            matches!(
                tool.function.name.as_str(),
                "read_file" | "list_dir" | "process_output"
            )
        })
        .collect()
}

pub fn is_known(tool: &str) -> bool {
    matches!(
        tool,
        "read_file"
            | "list_dir"
            | "write_file"
            | "run_command"
            | "start_command"
            | "process_output"
            | "spawn_agent"
    )
}

/// Mutating tools need an approval decision under the default `ask` policy.
pub fn requires_approval(tool: &str) -> bool {
    matches!(tool, "write_file" | "run_command" | "start_command")
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
        "start_command" => {
            let command = string_arg(args, "command");
            format!("start_command: {}", truncate(&command, 120))
        }
        "process_output" => match optional_string(args, "process_id") {
            Some(id) => format!("process_output {}", truncate(id, 12)),
            None => "process_output latest".to_string(),
        },
        "spawn_agent" => {
            let name = optional_string(args, "name").unwrap_or("agent");
            format!("spawn_agent {name}")
        }
        other => other.to_string(),
    }
}

/// A proposed `write_file`, kept in memory between showing the diff and the
/// user's decision. It carries the original contents (for the stale-write
/// check and for unselected hunks) and the hunk boundaries.
#[derive(Debug, Clone)]
pub struct WritePlan {
    path: PathBuf,
    label: String,
    old: String,
    new: String,
    hunks: Vec<Hunk>,
    diff: String,
}

#[derive(Debug, Clone)]
struct Hunk {
    old: Range<usize>,
    new: Range<usize>,
    info: HunkInfo,
}

impl WritePlan {
    pub fn diff(&self) -> &str {
        &self.diff
    }

    /// Absolute path of the file this plan will write.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Workspace-relative label shown in the UI.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Contents the plan was based on (staged or on disk).
    pub fn old(&self) -> &str {
        &self.old
    }

    /// Contents the plan will write.
    pub fn new_content(&self) -> &str {
        &self.new
    }

    pub fn hunk_infos(&self) -> Vec<HunkInfo> {
        self.hunks.iter().map(|hunk| hunk.info.clone()).collect()
    }

    /// Refuse to write when the file changed after the diff was generated.
    pub async fn verify_unchanged(&self) -> Result<()> {
        let current = tokio::fs::read(&self.path).await.unwrap_or_default();
        if current != self.old.as_bytes() {
            bail!(
                "{} changed after the diff was shown; review and approve again",
                self.label
            );
        }
        Ok(())
    }

    /// The result of applying `selected` hunks to the original contents.
    /// `None` or an empty selection means the whole change.
    pub fn apply(&self, selected: Option<&[usize]>) -> String {
        let Some(selected) = selected.filter(|selected| !selected.is_empty()) else {
            return self.new.clone();
        };
        if self.hunks.is_empty() {
            return self.new.clone();
        }
        let old = split_lines(&self.old);
        let new = split_lines(&self.new);
        let mut out = String::with_capacity(self.new.len());
        let mut cursor = 0;
        for (index, hunk) in self.hunks.iter().enumerate() {
            for line in &old[cursor..hunk.old.start] {
                out.push_str(line);
            }
            let lines = if selected.contains(&index) {
                &new[hunk.new.clone()]
            } else {
                &old[hunk.old.clone()]
            };
            for line in lines {
                out.push_str(line);
            }
            cursor = hunk.old.end;
        }
        for line in &old[cursor..] {
            out.push_str(line);
        }
        out
    }

    async fn write(&self, selected: Option<&[usize]>) -> Result<String> {
        let content = self.apply(selected);
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("cannot create {}", display(parent, &self.path)))?;
        }
        tokio::fs::write(&self.path, content.as_bytes())
            .await
            .with_context(|| format!("cannot write {}", self.label))?;
        Ok(format!("wrote {} bytes to {}", content.len(), self.label))
    }
}

/// The file a `write_file` call targets, resolved inside `cwd`. Used for the
/// pre-execution snapshot when no reviewed plan exists (policy `auto`).
pub fn write_target(args: &Value, cwd: &Path) -> Option<PathBuf> {
    let raw = args.get("path").and_then(Value::as_str)?;
    resolve_path(cwd, raw).ok()
}

/// Build the review plan for a file-writing tool. `None` for tools without a
/// file target (for example shell commands) or when the proposal cannot be
/// prepared (the tool will report the real error after approval).
pub async fn write_plan(
    tool: &str,
    args: &Value,
    cwd: &Path,
    config: &Config,
) -> Option<WritePlan> {
    write_plan_with_base(tool, args, cwd, config, None)
        .await
        .ok()
}

/// Like [`write_plan`], but `base` overrides the contents read from disk. The
/// staging layer passes the newest staged contents so a second write to the
/// same file diffs against what the agent already proposed.
pub async fn write_plan_with_base(
    tool: &str,
    args: &Value,
    cwd: &Path,
    config: &Config,
    base: Option<&str>,
) -> Result<WritePlan> {
    if tool != "write_file" {
        bail!("{tool} has no write plan");
    }
    write_plan_inner(args, cwd, config, base).await
}

async fn write_plan_inner(
    args: &Value,
    cwd: &Path,
    config: &Config,
    base: Option<&str>,
) -> Result<WritePlan> {
    let raw = required_string(args, "path")?;
    let new = required_string(args, "content")?;
    let path = resolve_path(cwd, raw)?;
    let old = match base {
        Some(base) => base.to_string(),
        None => match tokio::fs::read(&path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", display(&path, cwd)))
            }
        },
    };
    let label = display(&path, cwd);
    Ok(build_plan(path, label, old, new.to_string(), config))
}

/// Assemble the diff and hunk preview from already-known contents.
pub fn build_plan(
    path: PathBuf,
    label: String,
    old: String,
    new: String,
    config: &Config,
) -> WritePlan {
    let mut hunks = Vec::new();
    let diff = if old.len() > 2_000_000 || new.len() > 2_000_000 {
        format!("{label}: diff omitted (file larger than 2 MB)\n")
    } else {
        let text_diff = TextDiff::from_lines(old.as_str(), &new);
        for (index, ops) in text_diff.grouped_ops(3).iter().enumerate() {
            let old_start = ops.first().map(|op| op.old_range().start).unwrap_or(0);
            let old_end = ops.last().map(|op| op.old_range().end).unwrap_or(old_start);
            let new_start = ops.first().map(|op| op.new_range().start).unwrap_or(0);
            let new_end = ops.last().map(|op| op.new_range().end).unwrap_or(new_start);
            let header = format!(
                "@@ -{},{} +{},{} @@",
                old_start + 1,
                old_end - old_start,
                new_start + 1,
                new_end - new_start
            );
            let mut lines = vec![header.clone()];
            for op in ops {
                for change in text_diff.iter_changes(op) {
                    let sign = match change.tag() {
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                        ChangeTag::Equal => ' ',
                    };
                    let value = change.value();
                    let value = value.strip_suffix('\n').unwrap_or(value);
                    let value = value.strip_suffix('\r').unwrap_or(value);
                    lines.push(format!("{sign}{value}"));
                }
            }
            hunks.push(Hunk {
                old: old_start..old_end,
                new: new_start..new_end,
                info: HunkInfo {
                    index,
                    header,
                    lines,
                },
            });
        }
        unified_diff(&label, &old, &new)
    };

    WritePlan {
        path,
        label,
        old,
        new,
        hunks,
        diff: cap_output(diff, config.tools.max_output_bytes),
    }
}

/// Write the approved whole change or the selected hunks.
pub async fn apply_plan(plan: &WritePlan, selected: Option<&[usize]>) -> ToolOutcome {
    let started = Instant::now();
    match plan.write(selected).await {
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

/// Split text the same way `similar` does for `from_lines`: keep the line
/// endings, and treat `\n`, `\r\n`, and a lone `\r` as separators.
fn split_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                index += 2;
                lines.push(&text[start..index]);
                start = index;
            }
            b'\r' | b'\n' => {
                index += 1;
                lines.push(&text[start..index]);
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
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
        "start_command" | "process_output" => {
            Err(anyhow::anyhow!("{tool} is handled by the daemon"))
        }
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

/// A resolved shell invocation: sandbox wrapper, command, and working
/// directory. Shared by `run_command` (synchronous) and `start_command`
/// (background) so both go through the same sandbox decision.
#[derive(Debug, Clone)]
pub struct CommandPlan {
    pub program: std::ffi::OsString,
    pub args: Vec<std::ffi::OsString>,
    pub cwd: PathBuf,
}

impl CommandPlan {
    pub fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args).current_dir(&self.cwd);
        cmd
    }
}

/// Resolve a shell command into a concrete (possibly sandboxed) invocation.
pub fn command_plan(
    command: &str,
    cwd_arg: Option<&str>,
    session_cwd: &Path,
    config: &Config,
) -> Result<CommandPlan> {
    let command_cwd = match cwd_arg {
        Some(raw) => resolve_path(session_cwd, raw)?,
        None => session_cwd.to_path_buf(),
    };
    let command_cwd = command_cwd.canonicalize().unwrap_or(command_cwd);
    let sandboxed = match config.tools.sandbox {
        SandboxMode::Off => false,
        SandboxMode::Bwrap => {
            if !sandbox::bwrap_available() {
                bail!("tools.sandbox = \"bwrap\" but bwrap is not installed");
            }
            true
        }
        SandboxMode::Auto => sandbox::bwrap_available(),
    };

    let mut args: Vec<std::ffi::OsString> = Vec::new();
    if sandboxed {
        let workspace = session_cwd
            .canonicalize()
            .unwrap_or_else(|_| session_cwd.to_path_buf());
        let home = if config.tools.sandbox_read_home {
            sandbox::home_dir()
        } else {
            None
        };
        args.extend(sandbox::bwrap_args(
            &workspace,
            &command_cwd,
            config.tools.sandbox_network,
            home.as_deref(),
        ));
        args.push("sh".into());
    }
    args.push("-c".into());
    args.push(command.into());
    Ok(CommandPlan {
        program: if sandboxed {
            "bwrap".into()
        } else {
            "sh".into()
        },
        args,
        cwd: command_cwd,
    })
}

async fn run_command(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let command = required_string(args, "command")?;
    let plan = command_plan(command, optional_string(args, "cwd"), cwd, config)?;
    let command_cwd = plan.cwd.clone();
    let child = plan
        .command()
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

pub(crate) fn cap_output(text: String, max: usize) -> String {
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
    async fn write_plan_reports_a_unified_diff_and_hunks() {
        let cwd = temp_cwd("diff");
        let config = Config::default();
        std::fs::write(cwd.join("a.txt"), "one\ntwo\nthree\n").unwrap();

        let plan = write_plan(
            "write_file",
            &json!({"path": "a.txt", "content": "one\nTWO\nthree\n"}),
            &cwd,
            &config,
        )
        .await
        .expect("write plan");
        let diff = plan.diff();
        assert!(diff.contains("--- a/a.txt"), "{diff}");
        assert!(diff.contains("+++ b/a.txt"), "{diff}");
        assert!(diff.contains("-two"), "{diff}");
        assert!(diff.contains("+TWO"), "{diff}");
        let hunks = plan.hunk_infos();
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert!(hunks[0].header.starts_with("@@ -1,3 +1,3 @@"), "{hunks:?}");

        let added = write_plan(
            "write_file",
            &json!({"path": "new.txt", "content": "hello\n"}),
            &cwd,
            &config,
        )
        .await
        .expect("write plan");
        assert!(added.diff().contains("+hello"), "{}", added.diff());

        assert!(
            write_plan("run_command", &json!({"command": "true"}), &cwd, &config)
                .await
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn write_plan_applies_only_selected_hunks() {
        let cwd = temp_cwd("hunks");
        let config = Config::default();
        let original = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\neighteen\nnineteen\ntwenty\n";
        std::fs::write(cwd.join("a.txt"), original).unwrap();
        // Two edits separated by enough context to stay distinct hunks.
        let edited = "one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\nEIGHTEEN\nnineteen\ntwenty\n";
        let plan = write_plan(
            "write_file",
            &json!({"path": "a.txt", "content": edited}),
            &cwd,
            &config,
        )
        .await
        .expect("write plan");
        assert_eq!(plan.hunk_infos().len(), 2, "{:?}", plan.hunk_infos());

        // Selecting only the first hunk keeps the second edit out.
        let partial = plan.apply(Some(&[0]));
        assert!(partial.contains("TWO"), "{partial}");
        assert!(partial.contains("\neighteen\n"), "{partial}");
        assert!(!partial.contains("EIGHTEEN"), "{partial}");
        assert!(partial.ends_with("twenty\n"), "{partial}");

        // No selection (and an empty selection) means the whole change.
        assert_eq!(plan.apply(None), edited);
        assert_eq!(plan.apply(Some(&[])), edited);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn write_plan_detects_a_changed_file() {
        let cwd = temp_cwd("guard");
        let config = Config::default();
        std::fs::write(cwd.join("a.txt"), "one").unwrap();
        let plan = write_plan(
            "write_file",
            &json!({"path": "a.txt", "content": "two"}),
            &cwd,
            &config,
        )
        .await
        .expect("write plan");
        assert!(plan.verify_unchanged().await.is_ok());

        std::fs::write(cwd.join("a.txt"), "external").unwrap();
        assert!(plan.verify_unchanged().await.is_err());

        assert!(
            write_plan("run_command", &json!({"command": "true"}), &cwd, &config)
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

    #[tokio::test]
    async fn sandbox_off_runs_directly() {
        let cwd = temp_cwd("sandbox-off");
        let mut config = Config::default();
        config.tools.sandbox = SandboxMode::Off;
        let out = execute(
            "run_command",
            &json!({"command": "printf hi"}),
            &cwd,
            &config,
        )
        .await;
        assert!(out.ok);
        assert_eq!(out.output, "hi");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn sandbox_keeps_writes_inside_the_workspace() {
        if !sandbox::bwrap_available() {
            return;
        }
        let cwd = temp_cwd("sandbox");
        let config = Config::default();
        let marker = std::env::temp_dir().join(format!("eix-sandbox-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let inside = execute(
            "run_command",
            &json!({"command": "touch inside.txt"}),
            &cwd,
            &config,
        )
        .await;
        assert!(inside.ok, "{}", inside.output);
        assert!(cwd.join("inside.txt").exists());

        // Inside the sandbox `/tmp` is a private tmpfs, so this write must not
        // reach the host even though the command appears to succeed.
        let outside = execute(
            "run_command",
            &json!({"command": format!("touch {}", marker.display())}),
            &cwd,
            &config,
        )
        .await;
        assert!(outside.ok, "{}", outside.output);
        assert!(!marker.exists(), "sandbox leaked {}", marker.display());

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn sandbox_runs_in_the_requested_subdirectory() {
        if !sandbox::bwrap_available() {
            return;
        }
        let cwd = temp_cwd("sandbox-cwd");
        std::fs::create_dir_all(cwd.join("sub")).unwrap();
        std::fs::write(cwd.join("sub/marker.txt"), "x").unwrap();
        let out = execute(
            "run_command",
            &json!({"command": "cat marker.txt", "cwd": "sub"}),
            &cwd,
            &Config::default(),
        )
        .await;
        assert!(out.ok, "{}", out.output);
        assert_eq!(out.output, "x");
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
