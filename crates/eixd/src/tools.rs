//! Built-in tools and the workspace sandbox.
//!
//! Tools receive JSON arguments from the model, never a pre-rendered command.
//! Reading, searching and listing go through `exec_command`; edits go through
//! `apply_patch`. Both mutate the workspace only after the approval gate in
//! `server` allows the concrete call.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use eix_core::{sandbox, Config, HunkInfo, SandboxMode, ToolDefinition};
use regex::Regex;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
    pub duration_ms: u64,
}

/// Single source of truth for tool policy. Definitions stay separate because
/// they are large JSON schemas, but every list/approval decision derives from
/// this table so a new tool cannot silently drift between them.
#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    name: &'static str,
    /// May be used by a read-only sub-agent.
    subagent_allowed: bool,
    /// Safe to run concurrently with other read-only tools in one batch.
    parallel_safe: bool,
}

const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "apply_patch",
        subagent_allowed: false,
        parallel_safe: false,
    },
    ToolSpec {
        name: "exec_command",
        // A child may run proved read-only commands; the permission engine
        // still refuses anything that would mutate the workspace.
        subagent_allowed: true,
        parallel_safe: false,
    },
    ToolSpec {
        name: "write_stdin",
        subagent_allowed: false,
        parallel_safe: false,
    },
    ToolSpec {
        name: "view_image",
        subagent_allowed: true,
        parallel_safe: true,
    },
    ToolSpec {
        name: "use_skill",
        subagent_allowed: true,
        parallel_safe: false,
    },
    ToolSpec {
        name: "todo",
        subagent_allowed: true,
        parallel_safe: false,
    },
    ToolSpec {
        name: "memory",
        subagent_allowed: false,
        parallel_safe: false,
    },
    ToolSpec {
        name: "ready",
        subagent_allowed: false,
        parallel_safe: false,
    },
    ToolSpec {
        name: "web_fetch",
        subagent_allowed: true,
        parallel_safe: true,
    },
    ToolSpec {
        name: "web_search",
        subagent_allowed: true,
        parallel_safe: true,
    },
    ToolSpec {
        name: "lsp_diagnostics",
        subagent_allowed: true,
        parallel_safe: false,
    },
    ToolSpec {
        name: "spawn_agent",
        subagent_allowed: false,
        parallel_safe: false,
    },
    ToolSpec {
        name: "ask_user",
        subagent_allowed: false,
        parallel_safe: false,
    },
    ToolSpec {
        name: "mcp_list_resources",
        subagent_allowed: true,
        parallel_safe: false,
    },
    ToolSpec {
        name: "mcp_read_resource",
        subagent_allowed: true,
        parallel_safe: false,
    },
    ToolSpec {
        name: "mcp_list_prompts",
        subagent_allowed: true,
        parallel_safe: false,
    },
    ToolSpec {
        name: "mcp_get_prompt",
        subagent_allowed: true,
        parallel_safe: false,
    },
];

fn spec(tool: &str) -> Option<&'static ToolSpec> {
    TOOL_SPECS.iter().find(|spec| spec.name == tool)
}

/// Tool definitions sent to the provider. Keep descriptions short: they are
/// paid for on every request. The schemas are built once and cloned, so a turn
/// with several tool rounds does not re-run the JSON macros each time.
pub fn definitions() -> Vec<ToolDefinition> {
    static DEFINITIONS: OnceLock<Vec<ToolDefinition>> = OnceLock::new();
    DEFINITIONS.get_or_init(build_definitions).clone()
}

fn build_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::function(
            "apply_patch",
            "Edit files with a patch. Use *** Begin Patch, Add/Update/Delete File hunks, then *** End Patch. Requires approval.",
            json!({
                "type": "object",
                "properties": {
                    "patchText": {"type": "string", "description": "*** Begin Patch ... *** End Patch."}
                },
                "required": ["patchText"]
            }),
        ),
        ToolDefinition::function(
            "exec_command",
            "Run a shell command in the workspace. Returns output, or a session id when it is still running. Use cat, sed, awk, nl, rg, fd and ls for files.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "Shell command to execute."},
                    "workdir": {"type": "string", "description": "Workspace-relative directory (default: session cwd)."},
                    "yield_time_ms": {"type": "integer", "description": "Wait for output before returning a session id (default 10000, max 30000)."},
                    "max_output_tokens": {"type": "integer", "description": "Approximate output cap (default: config limit)."}
                },
                "required": ["cmd"]
            }),
        ),
        ToolDefinition::function(
            "write_stdin",
            "Write stdin to a running exec_command session, or poll its new output. Use an empty chars to poll.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Session id returned by exec_command."},
                    "chars": {"type": "string", "description": "Bytes to write; empty to only poll."},
                    "yield_time_ms": {"type": "integer", "description": "Wait for output (default 1000, max 30000)."}
                },
                "required": ["session_id"]
            }),
        ),
        ToolDefinition::function(
            "view_image",
            "Load a workspace image so you can inspect it visually.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative image path."},
                    "detail": {"type": "string", "enum": ["high", "original"], "description": "Image detail (default high)."}
                },
                "required": ["path"]
            }),
        ),
        ToolDefinition::function(
            "use_skill",
            "Load an installed skill's full instructions before matching work.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Skill name from <available_skills>."},
                    "arguments": {"type": "string", "description": "Optional $ARGUMENTS / $1..$9 values."}
                },
                "required": ["name"]
            }),
        ),
        ToolDefinition::function(
            "todo",
            "Replace this session's task list. Use only for genuinely multi-step work.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        ),
        ToolDefinition::function(
            "ready",
            "Queue this worktree for integration after commits and tests pass.",
            json!({
                "type": "object",
                "properties": {}
            }),
        ),
        ToolDefinition::function(
            "memory",
            "Persist durable workspace facts across sessions. Never store secrets or transient details.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "replace", "clear"]},
                    "text": {"type": "string", "description": "Markdown note for add/replace."}
                },
                "required": ["action"]
            }),
        ),
        ToolDefinition::function(
            "web_fetch",
            "Fetch an http(s) URL as readable text (about 100 KB max).",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http(s) URL."}
                },
                "required": ["url"]
            }),
        ),
        ToolDefinition::function(
            "web_search",
            "Search the web and return result titles and URLs.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "max_results": {"type": "integer", "description": "Max results (default 5, max 10)."}
                },
                "required": ["query"]
            }),
        ),
        ToolDefinition::function(
            "lsp_diagnostics",
            "Return LSP diagnostics for one file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File to check."}
                },
                "required": ["path"]
            }),
        ),
        ToolDefinition::function(
            "spawn_agent",
            "Run a read-only sub-agent on one focused question; returns its answer.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Focused task."},
                    "name": {"type": "string", "description": "Short label."},
                    "agent": {"type": "string", "description": "Sub-agent name from /agents."}
                },
                "required": ["prompt"]
            }),
        ),
        ToolDefinition::function(
            "ask_user",
            "Ask the user to choose when a decision is genuinely theirs. Best option first; blocks until answered.",
            json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "Short, specific question."},
                    "options": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 6,
                        "description": "Options, best first.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string"},
                                "description": {"type": "string", "description": "One-line trade-off."}
                            },
                            "required": ["label"]
                        }
                    }
                },
                "required": ["question", "options"]
            }),
        ),
        ToolDefinition::function(
            "mcp_list_resources",
            "List resources exposed by configured MCP servers.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server (omit for all)."}
                }
            }),
        ),
        ToolDefinition::function(
            "mcp_read_resource",
            "Read one MCP resource by URI.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server."},
                    "uri": {"type": "string", "description": "Resource URI."}
                },
                "required": ["server", "uri"]
            }),
        ),
        ToolDefinition::function(
            "mcp_list_prompts",
            "List prompt templates exposed by configured MCP servers.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server (omit for all)."}
                }
            }),
        ),
        ToolDefinition::function(
            "mcp_get_prompt",
            "Expand one MCP prompt template.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server."},
                    "name": {"type": "string", "description": "Prompt name."},
                    "arguments": {"type": "object", "description": "Prompt arguments."}
                },
                "required": ["server", "name"]
            }),
        ),
    ]
}

/// Tools a sub-agent may use. No mutating tools and no nested agents, so a
/// child can never widen the parent's permissions or change the workspace.
pub fn readonly_definitions() -> Vec<ToolDefinition> {
    definitions()
        .into_iter()
        .filter(|tool| spec(&tool.function.name).is_some_and(|spec| spec.subagent_allowed))
        .collect()
}

pub fn is_known(tool: &str) -> bool {
    spec(tool).is_some()
}

/// Read-only tools that can safely run concurrently in one assistant batch.
/// They do not mutate the workspace or daemon state and do not block on the
/// user, so a model that requests several independent reads does not pay their
/// latency sequentially.
pub fn parallel_safe(tool: &str) -> bool {
    spec(tool).is_some_and(|spec| spec.parallel_safe)
}

/// Tools that always mutate and are therefore hidden from read-only sessions.
/// `exec_command` is deliberately not here: read-only shell commands such as
/// `git status` are useful, and the permission engine rejects mutating ones.
pub fn hard_mutating(tool: &str) -> bool {
    matches!(tool, "apply_patch" | "write_stdin")
}

/// One-line description shown in the approval card / notification.
pub fn summarize(tool: &str, args: &Value) -> String {
    match tool {
        "lsp_diagnostics" => format!("lsp_diagnostics {}", string_arg(args, "path")),
        "view_image" => format!("view_image {}", string_arg(args, "path")),
        "apply_patch" => {
            let text = patch_text(args).unwrap_or_default();
            let files = text
                .lines()
                .filter(|line| {
                    line.starts_with("*** Add File:")
                        || line.starts_with("*** Update File:")
                        || line.starts_with("*** Delete File:")
                })
                .count();
            format!(
                "apply_patch ({files} file{})",
                if files == 1 { "" } else { "s" }
            )
        }
        "exec_command" => {
            let command = args
                .get("cmd")
                .or_else(|| args.get("command"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("exec_command: {}", truncate(command, 120))
        }
        "write_stdin" => {
            let id = args
                .get("session_id")
                .and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| value.as_u64().map(|id| id.to_string()))
                })
                .unwrap_or_default();
            format!("write_stdin {}", truncate(&id, 12))
        }
        "spawn_agent" => {
            let name = optional_string(args, "name").unwrap_or("agent");
            format!("spawn_agent {name}")
        }
        "ready" => "ready for integration".to_string(),
        "ask_user" => {
            let question = string_arg(args, "question");
            format!("ask_user: {}", truncate(&question, 100))
        }
        other if other.starts_with(eix_core::PLUGIN_TOOL_PREFIX) => {
            // Plugin arguments are opaque to the host, so show a bounded JSON
            // preview in the approval card instead of only the tool name.
            let args = serde_json::to_string(args).unwrap_or_default();
            format!("{other} {}", truncate(&args, 200))
        }
        other => other.to_string(),
    }
}

/// Parse and validate an `ask_user` call. Options are ordered best-first.
pub fn ask_user_args(args: &Value) -> Result<(String, Vec<eix_core::InputOption>)> {
    use eix_core::InputOption;

    let question = args
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .context("ask_user needs a non-empty question")?;
    let raw = args
        .get("options")
        .and_then(Value::as_array)
        .context("ask_user needs an options array")?;
    if !(2..=6).contains(&raw.len()) {
        bail!("ask_user needs 2-6 options");
    }
    let mut options = Vec::with_capacity(raw.len());
    for value in raw {
        let (label, description) = match value {
            Value::String(label) => (label.trim().to_string(), None),
            Value::Object(map) => {
                let label = map
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let description = map
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_string);
                (label, description)
            }
            _ => bail!("ask_user options must be strings or {{label, description}} objects"),
        };
        if label.is_empty() {
            bail!("ask_user options need a non-empty label");
        }
        options.push(InputOption { label, description });
    }
    Ok((question.to_string(), options))
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
    /// True when `new` already came back from the configured formatter.
    /// Applying the plan then skips a second formatter pass so a
    /// non-idempotent command cannot mutate the approved content again.
    formatted: bool,
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

    /// False when applying the plan would rewrite identical bytes.
    pub fn changes(&self) -> bool {
        self.old != self.new
    }

    /// Whether `new_content` has already been formatted for this preview.
    pub fn formatted(&self) -> bool {
        self.formatted
    }

    /// Mark an existing plan as preformatted, e.g. when it was rebuilt from a
    /// staged patch whose bytes were formatted before review.
    pub fn mark_formatted(mut self) -> Self {
        self.formatted = true;
        self
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
pub fn write_target(args: &Value, cwd: &Path, config: &Config) -> Option<PathBuf> {
    let raw = args.get("path").and_then(Value::as_str)?;
    resolve_path_in(cwd, raw, config).ok()
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
    match tool {
        "write_file" => write_plan_inner(args, cwd, config, base).await,
        "edit" => edit_plan_inner(args, cwd, config, base).await,
        _ => bail!("{tool} has no write plan"),
    }
}

/// Apply an exact-text replacement the same way the `edit` tool does.
fn apply_edit(old: &str, old_string: &str, new_string: &str, replace_all: bool) -> Result<String> {
    if old_string.is_empty() {
        bail!("oldString must not be empty");
    }
    if old_string == new_string {
        bail!("oldString and newString must differ");
    }
    let matches = old.matches(old_string).count();
    if matches == 0 {
        bail!("oldString was not found in the file");
    }
    if matches > 1 && !replace_all {
        bail!("oldString matched {matches} times; add more context or set replaceAll to true");
    }
    let updated = if replace_all {
        old.replace(old_string, new_string)
    } else {
        old.replacen(old_string, new_string, 1)
    };
    Ok(updated)
}

async fn edit_plan_inner(
    args: &Value,
    cwd: &Path,
    config: &Config,
    base: Option<&str>,
) -> Result<WritePlan> {
    let raw = required_string(args, "path")?;
    let old_string = required_string(args, "oldString")?;
    let new_string = required_string(args, "newString")?;
    let replace_all = args
        .get("replaceAll")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = resolve_path_in(cwd, raw, config)?;
    let old = match base {
        Some(base) => base.to_string(),
        None => match tokio::fs::read(&path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("{} does not exist", display(&path, cwd))
            }
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", display(&path, cwd)))
            }
        },
    };
    let new = apply_edit(&old, old_string, new_string, replace_all)?;
    let label = display(&path, cwd);
    Ok(build_plan(path, label, old, new, config))
}

async fn write_plan_inner(
    args: &Value,
    cwd: &Path,
    config: &Config,
    base: Option<&str>,
) -> Result<WritePlan> {
    let raw = required_string(args, "path")?;
    let new = required_string(args, "content")?;
    let path = resolve_path_in(cwd, raw, config)?;
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
        formatted: false,
    }
}

/// Rebuild a direct-write plan from the file as it exists now. Post-write
/// formatters may have changed the bytes, and the review diff should show what
/// actually landed on disk while still diffing against the pre-write base.
pub async fn refresh_plan(plan: &WritePlan, config: &Config) -> WritePlan {
    let new = match tokio::fs::read(&plan.path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => plan.new.clone(),
    };
    build_plan(
        plan.path.clone(),
        plan.label.clone(),
        plan.old.clone(),
        new,
        config,
    )
}

/// Run the configured formatter over proposed bytes before the user sees the
/// approval diff. The formatted result becomes the staged content too, so the
/// preview and the bytes eventually written agree, and `apply_plan` can skip a
/// second formatter pass.
pub async fn format_plan(plan: WritePlan, cwd: &Path, config: &Config) -> WritePlan {
    let Some(formatted) = format_content(config, cwd, plan.path(), plan.new_content()).await else {
        return plan;
    };
    if formatted == plan.new_content() {
        return plan.mark_formatted();
    }
    build_plan(
        plan.path.clone(),
        plan.label.clone(),
        plan.old.clone(),
        formatted,
        config,
    )
    .mark_formatted()
}

/// Rebuild a multi-file patch diff after the writes and formatters finished.
pub async fn refresh_patch_diff(ops: &[PatchOp], config: &Config) -> String {
    let mut diff = String::new();
    for op in ops {
        let new = if op.delete {
            String::new()
        } else {
            match tokio::fs::read(&op.path).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => op.new.clone(),
            }
        };
        let plan = build_plan(
            op.path.clone(),
            op.label.clone(),
            op.old.clone(),
            new,
            config,
        );
        if !diff.is_empty() && !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str(plan.diff());
    }
    diff
}

/// Write the approved whole change or the selected hunks, then run the same
/// post-write formatter as direct tool execution.
pub async fn apply_plan(
    plan: &WritePlan,
    selected: Option<&[usize]>,
    cwd: &Path,
    config: &Config,
) -> ToolOutcome {
    let started = Instant::now();
    match plan.write(selected).await {
        Ok(mut output) => {
            // A reviewed plan already carries formatted bytes. Running the
            // formatter again would let a non-idempotent command change the
            // exact content the user approved.
            if !plan.formatted() {
                if let Some(note) = format_file(config, cwd, plan.path()).await {
                    output.push_str(&note);
                }
            }
            ToolOutcome {
                ok: true,
                output,
                duration_ms: elapsed_ms(started),
            }
        }
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
    execute_with_cancel(tool, args, cwd, config, None).await
}

/// Like [`execute`], but long-running commands observe the turn's cancellation
/// flag and are killed instead of blocking until `command_timeout_secs`.
pub async fn execute_with_cancel(
    tool: &str,
    args: &Value,
    cwd: &Path,
    config: &Config,
    cancel: Option<&AtomicBool>,
) -> ToolOutcome {
    execute_in(tool, args, cwd, config, cancel, None).await
}

/// Execute a tool with an explicit sandbox workspace root. Worktree sessions
/// pass the worktree root so a cwd inside `go/` can still see `.git`.
pub async fn execute_in(
    tool: &str,
    args: &Value,
    cwd: &Path,
    config: &Config,
    cancel: Option<&AtomicBool>,
    sandbox_root: Option<&Path>,
) -> ToolOutcome {
    let started = Instant::now();
    let result = match tool {
        "use_skill" => use_skill(args, cwd, config),
        "web_fetch" => webfetch(args).await,
        "web_search" => websearch(args).await,
        "todo" => Err(anyhow::anyhow!("todo is handled by the daemon")),
        "apply_patch" => patch(args, cwd, config).await,
        // The daemon routes model `exec_command` calls through the process
        // registry. This synchronous path serves `shell.run` and other host
        // callers that want a bounded command.
        "exec_command" => run_command(args, cwd, config, cancel, sandbox_root).await,
        "write_stdin" | "view_image" => Err(anyhow::anyhow!("{tool} is handled by the daemon")),
        "mcp_list_resources" | "mcp_read_resource" | "mcp_list_prompts" | "mcp_get_prompt" => {
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

const DEFAULT_FORMATTER_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy)]
struct BuiltinFormatter {
    command: &'static str,
    extensions: &'static [&'static str],
}

/// Built-in formatters, tried in order after explicit config entries. A
/// missing executable is skipped silently, so this table can name popular
/// formatters without turning every write into a "command not found" note.
///
/// Commands must append the file path (`$1`) and format in place. TypeScript
/// is handled by Prettier/Biome/Deno rather than a separate `tsc` pass.
const BUILTIN_FORMATTERS: &[BuiltinFormatter] = &[
    BuiltinFormatter {
        command: "rustfmt --edition 2021",
        extensions: &["rs"],
    },
    BuiltinFormatter {
        command: "gofmt -w",
        extensions: &["go"],
    },
    BuiltinFormatter {
        command: "ruff format",
        extensions: &["py", "pyi"],
    },
    BuiltinFormatter {
        command: "black --quiet",
        extensions: &["py", "pyi"],
    },
    BuiltinFormatter {
        command: "prettier --write",
        extensions: &[
            "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "json", "jsonc", "json5", "yaml",
            "yml", "md", "mdx", "html", "htm", "css", "scss", "less", "graphql", "gql", "vue",
        ],
    },
    BuiltinFormatter {
        command: "biome format --write",
        extensions: &[
            "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "json", "jsonc",
        ],
    },
    BuiltinFormatter {
        command: "deno fmt",
        extensions: &[
            "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "json", "jsonc", "md", "yaml",
            "yml", "html", "css",
        ],
    },
    BuiltinFormatter {
        command: "yamlfix",
        extensions: &["yaml", "yml"],
    },
    BuiltinFormatter {
        command: "taplo fmt",
        extensions: &["toml"],
    },
    BuiltinFormatter {
        command: "shfmt -w",
        extensions: &["sh", "bash", "zsh", "ksh"],
    },
    BuiltinFormatter {
        command: "stylua",
        extensions: &["lua"],
    },
    BuiltinFormatter {
        command: "google-java-format -i",
        extensions: &["java"],
    },
    BuiltinFormatter {
        command: "buf format -w",
        extensions: &["proto"],
    },
    BuiltinFormatter {
        command: "clang-format -i",
        extensions: &[
            "c", "h", "cc", "cpp", "cxx", "hpp", "hxx", "hh", "mm", "java", "cs", "proto", "cu",
            "cuh",
        ],
    },
    BuiltinFormatter {
        command: "sqlfluff format --dialect ansi",
        extensions: &["sql"],
    },
    BuiltinFormatter {
        command: "pg_format -i",
        extensions: &["sql"],
    },
    BuiltinFormatter {
        command: "hclfmt -w",
        extensions: &["tf", "tfvars", "hcl"],
    },
    BuiltinFormatter {
        command: "alejandra",
        extensions: &["nix"],
    },
    BuiltinFormatter {
        command: "nixfmt",
        extensions: &["nix"],
    },
    BuiltinFormatter {
        command: "nixpkgs-fmt",
        extensions: &["nix"],
    },
    BuiltinFormatter {
        command: "ktlint -F",
        extensions: &["kt", "kts"],
    },
    BuiltinFormatter {
        command: "ktfmt",
        extensions: &["kt", "kts"],
    },
    BuiltinFormatter {
        command: "swiftformat",
        extensions: &["swift"],
    },
    BuiltinFormatter {
        command: "swift-format format --in-place",
        extensions: &["swift"],
    },
    BuiltinFormatter {
        command: "dart format",
        extensions: &["dart"],
    },
    BuiltinFormatter {
        command: "mix format",
        extensions: &["ex", "exs"],
    },
    BuiltinFormatter {
        command: "ormolu --mode inplace",
        extensions: &["hs", "lhs"],
    },
    BuiltinFormatter {
        command: "fourmolu -i",
        extensions: &["hs", "lhs"],
    },
    BuiltinFormatter {
        command: "stylish-haskell -i",
        extensions: &["hs", "lhs"],
    },
    BuiltinFormatter {
        command: "ocamlformat -i",
        extensions: &["ml", "mli"],
    },
    BuiltinFormatter {
        command: "scalafmt -i",
        extensions: &["scala", "sc"],
    },
    BuiltinFormatter {
        command: "rubocop -a",
        extensions: &["rb"],
    },
    BuiltinFormatter {
        command: "standardrb --fix",
        extensions: &["rb"],
    },
    BuiltinFormatter {
        command: "phpcbf",
        extensions: &["php"],
    },
    BuiltinFormatter {
        command: "php-cs-fixer fix",
        extensions: &["php"],
    },
    BuiltinFormatter {
        command: "Rscript -e 'styler::style_file(commandArgs(TRUE)[1])'",
        extensions: &["r"],
    },
    BuiltinFormatter {
        command: "zig fmt",
        extensions: &["zig"],
    },
    BuiltinFormatter {
        command: "nimpretty",
        extensions: &["nim"],
    },
    BuiltinFormatter {
        command: "v fmt -w",
        extensions: &["v"],
    },
    BuiltinFormatter {
        command: "crystal tool format",
        extensions: &["cr"],
    },
    BuiltinFormatter {
        command: "fantomas",
        extensions: &["fs", "fsx", "fsi"],
    },
    BuiltinFormatter {
        command: "gleam format",
        extensions: &["gleam"],
    },
    BuiltinFormatter {
        command: "elm-format --yes",
        extensions: &["elm"],
    },
    BuiltinFormatter {
        command: "cljfmt fix",
        extensions: &["clj", "cljs", "cljc", "edn"],
    },
    BuiltinFormatter {
        command: "forge fmt",
        extensions: &["sol"],
    },
    BuiltinFormatter {
        command: "cairo-format -i",
        extensions: &["cairo"],
    },
    BuiltinFormatter {
        command: "gdformat",
        extensions: &["gd"],
    },
    BuiltinFormatter {
        command: "jsonnetfmt -i",
        extensions: &["jsonnet", "libsonnet"],
    },
    BuiltinFormatter {
        command: "cmake-format -i",
        extensions: &["cmake"],
    },
];

fn builtin_formatters_for(path: &Path) -> impl Iterator<Item = &'static BuiltinFormatter> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    BUILTIN_FORMATTERS.iter().filter(move |formatter| {
        extension.as_deref().is_some_and(|extension| {
            formatter
                .extensions
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(extension))
        })
    })
}

fn builtin_formatter_for(path: &Path) -> Option<&'static BuiltinFormatter> {
    builtin_formatters_for(path).find(|formatter| command_available(formatter.command))
}

/// Check the executable without spawning a shell. Built-in commands are
/// skipped when their program is not on `PATH`.
fn command_available(command: &str) -> bool {
    let Some(program) = command.split_whitespace().next() else {
        return false;
    };
    let program = program.trim_matches(|ch| ch == '\'' || ch == '"');
    if program.is_empty() {
        return false;
    }
    if program.contains('/') || program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        dir.join(program).is_file()
            || (cfg!(windows) && dir.join(format!("{program}.exe")).is_file())
    })
}

/// Resolve the formatter command and timeout for one path.
fn formatter_for(path: &Path, config: &Config) -> Option<(String, u64)> {
    match config.formatter.entry_for(path) {
        Some(formatter) => {
            let command = formatter.command.as_deref()?.trim();
            if command.is_empty() {
                return None;
            }
            // Config entries normally win. If their executable is missing,
            // fall through to the built-in candidates instead of failing every
            // write; this is what lets a stale `ruff` entry use `black`, etc.
            if config.formatter_defaults && !command_available(command) {
                builtin_formatter_for(path)
                    .map(|builtin| (builtin.command.to_string(), DEFAULT_FORMATTER_TIMEOUT_SECS))
            } else {
                Some((command.to_string(), formatter.timeout_secs))
            }
        }
        None if config.formatter_defaults => {
            let formatter = builtin_formatter_for(path)?;
            Some((
                formatter.command.to_string(),
                DEFAULT_FORMATTER_TIMEOUT_SECS,
            ))
        }
        None => None,
    }
}

async fn run_formatter(
    command: &str,
    timeout_secs: u64,
    cwd: &Path,
    path: &Path,
) -> std::result::Result<(), String> {
    let mut process = tokio::process::Command::new("sh");
    process
        .arg("-c")
        .arg(format!("{command} \"$1\""))
        .arg("sh")
        .arg(path)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match tokio::time::timeout(Duration::from_secs(timeout_secs), process.output()).await {
        Err(_) => Err(format!("timed out after {timeout_secs}s")),
        Ok(Err(err)) => Err(format!("failed: {err}")),
        Ok(Ok(output)) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            Err(if stderr.is_empty() {
                format!("exited with {}", output.status)
            } else {
                format!("failed: {}", cap_output(stderr.to_string(), 2000))
            })
        }
        Ok(Ok(_)) => Ok(()),
    }
}

/// Run the configured formatter, or the first available built-in formatter
/// for the file's extension, on a file that was just written. Returns an
/// optional note for the tool output; failures never undo the write.
pub async fn format_file(config: &Config, cwd: &Path, path: &Path) -> Option<String> {
    let (command, timeout_secs) = formatter_for(path, config)?;
    run_formatter(&command, timeout_secs, cwd, path)
        .await
        .err()
        .map(|err| format!("\n[formatter {err}]"))
}

/// Format proposed bytes without touching the real target. The temporary file
/// lives beside the target (or in `cwd` when the parent does not exist yet) so
/// project-local formatter configuration still applies. Returns the formatted
/// text on success, including when the formatter made no changes.
pub async fn format_content(
    config: &Config,
    cwd: &Path,
    path: &Path,
    content: &str,
) -> Option<String> {
    if content.len() > 2_000_000 {
        return None;
    }
    let (command, timeout_secs) = formatter_for(path, config)?;
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    let directory = path
        .parent()
        .filter(|parent| parent.is_dir())
        .unwrap_or(cwd);
    let name = if extension.is_empty() {
        format!(".eix-preview-{}", uuid::Uuid::new_v4())
    } else {
        format!(".eix-preview-{}.{}", uuid::Uuid::new_v4(), extension)
    };
    let temp = directory.join(name);
    if tokio::fs::write(&temp, content.as_bytes()).await.is_err() {
        return None;
    }
    let result = run_formatter(&command, timeout_secs, cwd, &temp).await;
    let formatted = match result {
        Ok(()) => tokio::fs::read(&temp)
            .await
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok()),
        Err(_) => None,
    };
    let _ = tokio::fs::remove_file(&temp).await;
    formatted
}

/// Minimal HTML -> text conversion for `webfetch` / `websearch`.
fn html_to_text(html: &str) -> String {
    static SCRIPTS: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    let scripts = SCRIPTS.get_or_init(|| {
        Regex::new(r"(?is)<(script|style)[^>]*>.*?</(script|style)>").expect("static regex")
    });
    let without_scripts = scripts.replace_all(html, " ");
    let tags = TAGS.get_or_init(|| Regex::new(r"(?s)<[^>]+>").expect("static regex"));
    let text = tags.replace_all(&without_scripts, " ");
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &input[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn web_client() -> Result<reqwest::Client> {
    // The client pools connections and owns a TLS backend; rebuilding it for
    // every fetch paid setup cost and prevented keep-alive between calls.
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("eix/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|err| format!("cannot build HTTP client: {err}"))
        })
        .clone()
        .map_err(|err| anyhow!(err))
}

async fn webfetch(args: &Value) -> Result<String> {
    let url = required_string(args, "url")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("url must start with http:// or https://");
    }
    let response = web_client()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot fetch {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{url} returned HTTP {status}");
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let body = response
        .text()
        .await
        .with_context(|| format!("cannot read {url}"))?;
    let text = if content_type.contains("html") {
        html_to_text(&body)
    } else {
        body
    };
    Ok(cap_output(text, 100_000))
}

async fn websearch(args: &Value) -> Result<String> {
    let query = required_string(args, "query")?;
    let max = args
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 10) as usize;
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        percent_encode(query)
    );
    let response = web_client()?
        .get(&url)
        .send()
        .await
        .context("websearch request failed")?;
    if !response.status().is_success() {
        bail!("websearch returned HTTP {}", response.status());
    }
    let body = response
        .text()
        .await
        .context("cannot read websearch results")?;
    static LINK: OnceLock<Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| {
        Regex::new(r#"(?is)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("static regex")
    });
    let mut out = String::new();
    let mut count = 0;
    for capture in link.captures_iter(&body) {
        let mut href = capture[1].to_string();
        if let Some(index) = href.find("uddg=") {
            let encoded = href[index + 5..].split('&').next().unwrap_or_default();
            href = percent_decode(encoded);
        }
        if href.starts_with("//") {
            href.insert_str(0, "https:");
        }
        let title = html_to_text(&capture[2]);
        if title.is_empty() || href.is_empty() {
            continue;
        }
        count += 1;
        out.push_str(&format!("{count}. {title}\n   {href}\n"));
        if count >= max {
            break;
        }
    }
    if out.is_empty() {
        bail!("websearch returned no results for {query:?}");
    }
    Ok(cap_output(out, 20_000))
}

fn use_skill(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let name = required_string(args, "name")?;
    let arguments = args
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match eix_core::find_skill(cwd, name, &config.compations) {
        Some(skill) => {
            let body = eix_core::expand_command(&skill.body, arguments);
            let restriction = if skill.allowed_tools.is_empty() {
                String::new()
            } else {
                format!(
                    "\nAllowed tools for the rest of this turn: {}\n",
                    skill.allowed_tools.join(", ")
                )
            };
            Ok(format!("# skill: {}\n{restriction}\n{body}", skill.name))
        }
        None => {
            let names: Vec<String> = eix_core::load_skills(cwd, &config.compations)
                .into_iter()
                .map(|skill| skill.name)
                .collect();
            if names.is_empty() {
                bail!("unknown skill {name:?}; no skills are installed")
            }
            bail!("unknown skill {name:?}; available: {}", names.join(", "))
        }
    }
}

/// Render a numbered, paginated slice of a text file. `offset` is 1-based.
pub fn format_read(text: &str, offset: usize, limit: usize, max_bytes: usize) -> String {
    // Count without materialising a `Vec<&str>` for the whole file: a large
    // log would otherwise allocate one pointer per line just to slice it.
    let total = text.lines().count();
    if total == 0 {
        return "(empty file)".to_string();
    }
    if offset > total {
        return format!("(offset {offset} is past the end of the file, {total} lines)");
    }
    let start = offset - 1;
    let end = (start + limit).min(total);
    let mut out = String::new();
    for (index, line) in text.lines().skip(start).take(end - start).enumerate() {
        out.push_str(&format!("{}: {line}\n", start + index + 1));
        if out.len() >= max_bytes {
            out.push_str("… [truncated]\n");
            break;
        }
    }
    if end < total {
        out.push_str(&format!(
            "… [{} more lines; use offset={} to continue]\n",
            total - end,
            end + 1
        ));
    }
    cap_output(out, max_bytes)
}

/// One file operation parsed from a `patch` call.
#[derive(Debug, Clone)]
pub struct PatchOp {
    pub path: PathBuf,
    pub label: String,
    pub old: String,
    pub new: String,
    pub delete: bool,
}

/// Parse and validate an apply_patch document against the current workspace.
/// The returned ops carry the exact old/new contents so the caller can show a
/// diff or apply them all.
pub async fn patch_ops(patch_text: &str, cwd: &Path, config: &Config) -> Result<Vec<PatchOp>> {
    let lines: Vec<&str> = patch_text.lines().collect();
    let mut index = 0;
    let mut ops = Vec::new();
    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() || line.starts_with("*** Begin Patch") {
            index += 1;
            continue;
        }
        if line.starts_with("*** End Patch") {
            break;
        }
        if let Some(raw) = line.strip_prefix("*** Add File: ") {
            let path = resolve_path_in(cwd, raw.trim(), config)?;
            index += 1;
            let mut body = String::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let line = lines[index];
                if let Some(content) = line.strip_prefix('+') {
                    body.push_str(content);
                    body.push('\n');
                }
                index += 1;
            }
            let old = match tokio::fs::read(&path).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(err) => {
                    return Err(err).with_context(|| format!("cannot read {}", display(&path, cwd)))
                }
            };
            if !old.is_empty() {
                bail!("Add File target {} already exists", display(&path, cwd));
            }
            ops.push(PatchOp {
                label: display(&path, cwd),
                path,
                old,
                new: body,
                delete: false,
            });
            continue;
        }
        if let Some(raw) = line.strip_prefix("*** Delete File: ") {
            let path = resolve_path_in(cwd, raw.trim(), config)?;
            let old = tokio::fs::read(&path)
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .with_context(|| format!("cannot read {}", display(&path, cwd)))?;
            ops.push(PatchOp {
                label: display(&path, cwd),
                path,
                old,
                new: String::new(),
                delete: true,
            });
            index += 1;
            continue;
        }
        if let Some(raw) = line.strip_prefix("*** Update File: ") {
            let path = resolve_path_in(cwd, raw.trim(), config)?;
            let old = tokio::fs::read(&path)
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .with_context(|| format!("cannot read {}", display(&path, cwd)))?;
            index += 1;
            let mut current = old.clone();
            let mut applied_any = false;
            while index < lines.len() && !lines[index].starts_with("*** ") {
                if lines[index].starts_with("@@") {
                    index += 1;
                    continue;
                }
                let mut old_block = Vec::new();
                let mut new_block = Vec::new();
                while index < lines.len() {
                    let hline = lines[index];
                    if hline.starts_with("*** ") || hline.starts_with("@@") {
                        break;
                    }
                    match hline.chars().next() {
                        Some(' ') => {
                            let rest = &hline[1..];
                            old_block.push(rest.to_string());
                            new_block.push(rest.to_string());
                        }
                        Some('-') => old_block.push(hline[1..].to_string()),
                        Some('+') => new_block.push(hline[1..].to_string()),
                        None => {}
                        _ => {
                            // Tolerate unprefixed context lines.
                            old_block.push(hline.to_string());
                            new_block.push(hline.to_string());
                        }
                    }
                    index += 1;
                }
                if old_block.is_empty() && new_block.is_empty() {
                    continue;
                }
                current = apply_patch_block(&current, &old_block, &new_block)
                    .with_context(|| format!("cannot apply hunk to {}", display(&path, cwd)))?;
                applied_any = true;
            }
            if !applied_any {
                bail!("Update File {} has no hunks", display(&path, cwd));
            }
            ops.push(PatchOp {
                label: display(&path, cwd),
                path,
                old,
                new: current,
                delete: false,
            });
            continue;
        }
        index += 1;
    }
    if ops.is_empty() {
        bail!("patchText contains no Add/Update/Delete File sections");
    }
    Ok(ops)
}

/// Replace the first occurrence of `old_block` with `new_block`.
fn apply_patch_block(content: &str, old_block: &[String], new_block: &[String]) -> Result<String> {
    if old_block.is_empty() {
        bail!("hunk has no context or removed lines");
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut found = None;
    'search: for start in 0..=lines.len().saturating_sub(old_block.len()) {
        for (offset, expected) in old_block.iter().enumerate() {
            if lines[start + offset].trim_end_matches('\r') != expected.trim_end_matches('\r') {
                continue 'search;
            }
        }
        found = Some(start);
        break;
    }
    let Some(start) = found else {
        bail!("hunk context was not found");
    };
    let mut result = Vec::with_capacity(lines.len() + new_block.len());
    result.extend_from_slice(&lines[..start]);
    result.extend(new_block.iter().map(String::as_str));
    result.extend_from_slice(&lines[start + old_block.len()..]);
    let mut out = result.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Format every replacement in a patch before it is shown for approval. The
/// returned operations are also what gets applied, so the preview, the staged
/// bytes, and the final file all describe the same content.
pub async fn format_patch_ops(mut ops: Vec<PatchOp>, cwd: &Path, config: &Config) -> Vec<PatchOp> {
    for op in &mut ops {
        if op.delete {
            continue;
        }
        if let Some(formatted) = format_content(config, cwd, &op.path, &op.new).await {
            op.new = formatted;
        }
    }
    ops
}

/// Render parsed patch operations as one unified diff.
pub fn patch_diff(ops: &[PatchOp], config: &Config) -> String {
    let mut out = String::new();
    for op in ops {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if op.delete {
            out.push_str(&format!("--- a/{}\n+++ /dev/null\n", op.label));
            for line in op.old.lines() {
                out.push_str(&format!("-{line}\n"));
            }
        } else {
            out.push_str(&unified_diff(&op.label, &op.old, &op.new));
        }
    }
    cap_output(out, config.tools.max_output_bytes)
}

/// Preview a multi-file patch as one unified diff.
pub async fn patch_preview(patch_text: &str, cwd: &Path, config: &Config) -> Result<String> {
    let ops = patch_ops(patch_text, cwd, config).await?;
    Ok(patch_diff(&ops, config))
}

/// Write the exact operations previously formatted for review. Unlike the raw
/// `patch` tool this does not run formatters again.
pub async fn apply_patch_ops(ops: &[PatchOp]) -> ToolOutcome {
    let started = Instant::now();
    match write_patch_ops(ops).await {
        Ok((written, deleted)) => ToolOutcome {
            ok: true,
            output: format!("patched {written} file(s), deleted {deleted} file(s)"),
            duration_ms: elapsed_ms(started),
        },
        Err(err) => ToolOutcome {
            ok: false,
            output: format!("{err:#}"),
            duration_ms: elapsed_ms(started),
        },
    }
}

async fn write_patch_ops(ops: &[PatchOp]) -> Result<(usize, usize)> {
    // Validate every target before writing anything.
    for op in ops {
        if op.delete {
            continue;
        }
        if let Ok(current) = tokio::fs::read(&op.path).await {
            let current = String::from_utf8_lossy(&current);
            if current != op.old {
                bail!("{} changed while the patch was being prepared", op.label);
            }
        }
    }
    let mut written = 0;
    let mut deleted = 0;
    for op in ops {
        if op.delete {
            tokio::fs::remove_file(&op.path)
                .await
                .with_context(|| format!("cannot delete {}", op.label))?;
            deleted += 1;
        } else {
            if let Some(parent) = op.path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            tokio::fs::write(&op.path, op.new.as_bytes())
                .await
                .with_context(|| format!("cannot write {}", op.label))?;
            written += 1;
        }
    }
    Ok((written, deleted))
}

async fn patch(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let patch_text = patch_text(args).context("apply_patch needs patchText")?;
    let ops = patch_ops(&patch_text, cwd, config).await?;
    let (written, deleted) = write_patch_ops(&ops).await?;
    let mut message = format!("patched {written} file(s), deleted {deleted} file(s)");
    for op in &ops {
        if op.delete {
            continue;
        }
        if let Some(note) = format_file(config, cwd, &op.path).await {
            message.push_str(&note);
        }
    }
    Ok(message)
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

fn expand_sandbox_path(value: &str, workspace: &Path) -> PathBuf {
    let trimmed = value.trim();
    if trimmed == "~" {
        return sandbox::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = sandbox::home_dir() {
            return home.join(rest);
        }
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

/// The real git metadata directory for a linked worktree. Normal repositories
/// have `.git/`, which is already covered by the workspace bind.
fn worktree_git_dir(workspace: &Path) -> Option<PathBuf> {
    let pointer = std::fs::read_to_string(workspace.join(".git")).ok()?;
    let raw = pointer.trim().strip_prefix("gitdir:")?.trim();
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    path.canonicalize().ok().filter(|path| path.is_dir())
}

/// Resolve a shell command into a concrete (possibly sandboxed) invocation.
pub fn command_plan(
    command: &str,
    cwd_arg: Option<&str>,
    session_cwd: &Path,
    sandbox_root: Option<&Path>,
    config: &Config,
) -> Result<CommandPlan> {
    let command_cwd = match cwd_arg {
        Some(raw) => resolve_path_in(session_cwd, raw, config)?,
        None => session_cwd.to_path_buf(),
    };
    let command_cwd = command_cwd.canonicalize().unwrap_or(command_cwd);
    let sandboxed = match config.tools.sandbox {
        SandboxMode::Off => false,
        SandboxMode::Required => {
            if !sandbox::bwrap_available() {
                bail!(
                    "tools.sandbox = \"required\" but {}",
                    sandbox::unavailable_reason()
                );
            }
            true
        }
        SandboxMode::Auto => sandbox::bwrap_available(),
    };

    let mut args: Vec<std::ffi::OsString> = Vec::new();
    if sandboxed {
        // A worktree session may keep its cwd in a subdirectory. Mount the
        // worktree root, not just that subdirectory, so `.git` and git commit
        // operations remain usable inside the sandbox.
        let workspace = sandbox_root
            .unwrap_or(session_cwd)
            .canonicalize()
            .unwrap_or_else(|_| sandbox_root.unwrap_or(session_cwd).to_path_buf());
        let read_paths: Vec<PathBuf> = config
            .tools
            .sandbox_read_paths
            .iter()
            .map(|path| expand_sandbox_path(path, &workspace))
            .collect();
        let mut writable_roots: Vec<PathBuf> = config
            .tools
            .sandbox_writable_roots
            .iter()
            .map(|path| expand_sandbox_path(path, &workspace))
            .filter(|path| path != &workspace)
            .collect();
        // Linked worktrees keep `.git` outside the worktree as a `gitdir:`
        // pointer. Bind that metadata directory too; otherwise every git
        // command inside the sandbox fails with "not a valid repository".
        if let Some(git_dir) = worktree_git_dir(&workspace) {
            if git_dir != workspace && !writable_roots.contains(&git_dir) {
                writable_roots.push(git_dir);
            }
        }
        args.extend(sandbox::bwrap_args(
            &workspace,
            &command_cwd,
            config.tools.sandbox_network,
            &read_paths,
            &writable_roots,
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

async fn run_command(
    args: &Value,
    cwd: &Path,
    config: &Config,
    cancel: Option<&AtomicBool>,
    sandbox_root: Option<&Path>,
) -> Result<String> {
    let command = args
        .get("cmd")
        .or_else(|| args.get("command"))
        .and_then(Value::as_str)
        .context("cmd is required")?;
    let plan = command_plan(
        command,
        optional_string(args, "workdir").or_else(|| optional_string(args, "cwd")),
        cwd,
        sandbox_root,
        config,
    )?;
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
    let output = tokio::select! {
        result = tokio::time::timeout(timeout, child.wait_with_output()) => {
            match result {
                Ok(result) => result.context("command failed")?,
                Err(_) => {
                    return Ok(format!(
                        "command timed out after {}s",
                        config.tools.command_timeout_secs
                    ));
                }
            }
        }
        // `child` has `kill_on_drop`, so dropping the wait future on cancel
        // terminates the process instead of leaving it running until timeout.
        _ = wait_for_cancel(cancel) => bail!("command cancelled"),
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

async fn wait_for_cancel(cancel: Option<&AtomicBool>) {
    let Some(cancel) = cancel else {
        std::future::pending::<()>().await;
        return;
    };
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Resolve `raw` inside `cwd`, rejecting traversal and symlink escapes.
pub fn resolve_path(cwd: &Path, raw: &str) -> Result<PathBuf> {
    resolve_path_inner(cwd, raw, None)
}

/// Like [`resolve_path`], but `tools.external_directory` patterns may allow
/// paths outside the workspace.
pub fn resolve_path_in(cwd: &Path, raw: &str, config: &Config) -> Result<PathBuf> {
    resolve_path_inner(cwd, raw, Some(config))
}

fn resolve_path_inner(cwd: &Path, raw: &str, config: Option<&Config>) -> Result<PathBuf> {
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
    let external =
        |path: &Path| config.is_some_and(|config| config.tools.external_path_allowed(path));
    let external_allowed = external(&normalized);
    if !normalized.starts_with(&root) && !external_allowed {
        if let Ok(real) = normalized.canonicalize() {
            if real.starts_with(&root) || external(&real) {
                return Ok(real);
            }
        }
        bail!("path escapes the session workspace: {raw}");
    }
    if let Ok(real) = normalized.canonicalize() {
        if !real.starts_with(&root) && !external(&real) {
            bail!("path escapes the session workspace: {raw}");
        }
        return Ok(real);
    }
    if external_allowed {
        return Ok(normalized);
    }
    // New path: the nearest existing ancestor must still be inside the root.
    let mut ancestor = normalized.parent();
    while let Some(dir) = ancestor {
        if let Ok(real) = dir.canonicalize() {
            if !real.starts_with(&root) && !external(&real) {
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

/// `apply_patch` is a JSON function in eix, but models often reuse Codex's
/// freeform field names. Accept the common aliases rather than failing a call
/// whose patch is otherwise valid.
fn patch_text(args: &Value) -> Option<String> {
    ["patchText", "patch", "input", "text"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(str::to_string)
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
    use eix_core::FormatterConfig;
    use std::sync::Arc;
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
    fn builtin_tool_schemas_are_compact() {
        let tokens = crate::context::estimate_tool_schema_tokens(&definitions());
        // Every request pays for these schemas. Keep room for a few additions,
        // but fail before descriptions drift back into paragraphs.
        assert!(tokens < 2_400, "built-in tool schemas are {tokens} tokens");
    }

    #[test]
    fn html_and_url_helpers_are_conservative() {
        assert_eq!(
            html_to_text("<h1>Hi &amp; bye</h1><script>x()</script> text"),
            "Hi & bye text"
        );
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(percent_decode("a%20b%26c"), "a b&c");
    }

    #[tokio::test]
    async fn web_fetch_reads_html_from_a_local_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let body = "<html><body><h1>Hello</h1><p>world</p></body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let output = execute(
            "web_fetch",
            &json!({"url": format!("http://{addr}/")}),
            Path::new("."),
            &Config::default(),
        )
        .await;
        assert!(output.ok, "{}", output.output);
        assert!(output.output.contains("Hello world"));
        server.abort();
    }

    #[tokio::test]
    async fn formatter_runs_after_a_direct_write_and_respects_extensions() {
        let cwd = temp_cwd("formatter");
        let config = Config {
            formatter: FormatterConfig {
                command: Some("sed -i 's/hello/HELLO/'".into()),
                extensions: vec!["txt".into()],
                timeout_secs: 30,
            }
            .into(),
            formatter_defaults: false,
            ..Config::default()
        };
        let outcome = execute(
            "apply_patch",
            &json!({"patchText": "*** Begin Patch\n*** Add File: note.txt\n+hello\n*** End Patch\n"}),
            &cwd,
            &config,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).unwrap(),
            "HELLO\n"
        );

        let outcome = execute(
            "apply_patch",
            &json!({"patchText": "*** Begin Patch\n*** Add File: note.md\n+hello\n*** End Patch\n"}),
            &cwd,
            &config,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.md")).unwrap(),
            "hello\n"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn builtins_cover_common_languages() {
        for extension in [
            "rs", "go", "py", "js", "ts", "tsx", "json", "yaml", "toml", "md", "html", "css", "c",
            "cpp", "h", "sh", "lua", "sql", "tf", "nix", "kt", "swift", "dart", "ex", "hs", "ml",
            "scala", "rb", "php", "r", "zig", "nim", "v", "cr", "fs", "gleam", "elm", "clj", "sol",
            "cairo", "gd", "jsonnet", "cmake", "graphql", "proto",
        ] {
            let path = PathBuf::from(format!("file.{extension}"));
            assert!(
                builtin_formatters_for(&path).next().is_some(),
                "no built-in formatter for .{extension}"
            );
        }
    }

    #[test]
    fn explicit_entries_can_disable_builtins() {
        let config = Config {
            formatter: FormatterConfig {
                command: None,
                extensions: vec!["rs".into()],
                timeout_secs: 30,
            }
            .into(),
            ..Config::default()
        };
        let path = PathBuf::from("src/lib.rs");
        assert!(config.formatter.entry_for(&path).is_some());
        assert!(config.formatter.matching(&path).is_none());
        assert!(config.formatter_defaults);
    }

    #[tokio::test]
    async fn refreshed_plan_shows_formatted_bytes() {
        let cwd = temp_cwd("refresh-formatter");
        let config = Config {
            formatter: FormatterConfig {
                command: Some("sed -i 's/hello/HELLO/'".into()),
                extensions: vec!["txt".into()],
                timeout_secs: 30,
            }
            .into(),
            ..Config::default()
        };
        let args = json!({"path": "note.txt", "content": "hello"});
        let plan = write_plan("write_file", &args, &cwd, &config)
            .await
            .expect("pre-write plan");
        assert!(apply_plan(&plan, None, &cwd, &config).await.ok);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).unwrap(),
            "HELLO"
        );

        let refreshed = refresh_plan(&plan, &config).await;
        assert!(refreshed.diff().contains("+HELLO"), "{}", refreshed.diff());
        assert!(!refreshed.diff().contains("+hello"), "{}", refreshed.diff());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn format_plan_previews_formatted_content_without_formatting_twice() {
        let cwd = temp_cwd("format-plan-preview");
        let config = Config {
            formatter: FormatterConfig {
                // Deliberately non-idempotent: a second pass would append a
                // second X and change the bytes the user approved.
                command: Some("sed -i 's/$/X/'".into()),
                extensions: vec!["txt".into()],
                timeout_secs: 30,
            }
            .into(),
            formatter_defaults: false,
            ..Config::default()
        };
        let args = json!({"path": "note.txt", "content": "hello"});
        let plan = write_plan("write_file", &args, &cwd, &config)
            .await
            .expect("pre-write plan");
        let plan = format_plan(plan, &cwd, &config).await;
        assert!(plan.formatted());
        assert!(plan.diff().contains("+helloX"), "{}", plan.diff());

        let outcome = apply_plan(&plan, None, &cwd, &config).await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).unwrap(),
            "helloX"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn formatted_patch_ops_apply_the_preview_bytes_once() {
        let cwd = temp_cwd("format-patch-preview");
        std::fs::write(cwd.join("note.txt"), "hello\n").unwrap();
        let config = Config {
            formatter: FormatterConfig {
                command: Some("sed -i 's/$/X/'".into()),
                extensions: vec!["txt".into()],
                timeout_secs: 30,
            }
            .into(),
            formatter_defaults: false,
            ..Config::default()
        };
        let ops = vec![PatchOp {
            path: cwd.join("note.txt"),
            label: "note.txt".into(),
            old: "hello\n".into(),
            new: "hello\n".into(),
            delete: false,
        }];
        let ops = format_patch_ops(ops, &cwd, &config).await;
        let diff = patch_diff(&ops, &config);
        assert!(diff.contains("+helloX"), "{diff}");
        assert!(!diff.contains("+helloXX"), "{diff}");

        let outcome = apply_patch_ops(&ops).await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).unwrap(),
            "helloX\n"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn formatter_uses_the_first_matching_entry() {
        let cwd = temp_cwd("formatter-order");
        let config = Config {
            formatter: vec![
                FormatterConfig {
                    command: Some("sed -i 's/hello/FIRST/'".into()),
                    extensions: vec!["txt".into()],
                    timeout_secs: 30,
                },
                FormatterConfig {
                    command: Some("sed -i 's/hello/SECOND/'".into()),
                    extensions: vec!["txt".into()],
                    timeout_secs: 30,
                },
                FormatterConfig {
                    command: Some("sed -i 's/world/WORLD/'".into()),
                    extensions: vec!["md".into()],
                    timeout_secs: 30,
                },
            ]
            .into(),
            ..Config::default()
        };

        let outcome = execute(
            "apply_patch",
            &json!({"patchText": "*** Begin Patch\n*** Add File: note.txt\n+hello\n*** End Patch\n"}),
            &cwd,
            &config,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).unwrap(),
            "FIRST\n"
        );

        let outcome = execute(
            "apply_patch",
            &json!({"patchText": "*** Begin Patch\n*** Add File: note.md\n+world\n*** End Patch\n"}),
            &cwd,
            &config,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.md")).unwrap(),
            "WORLD\n"
        );

        let _ = std::fs::remove_dir_all(&cwd);
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

    #[test]
    fn external_directory_opt_in_allows_escapes() {
        let cwd = temp_cwd("external-cwd");
        let outside = temp_cwd("external-target");
        std::fs::write(outside.join("note.txt"), "hello").unwrap();
        let target = outside.join("note.txt");
        let raw = target.to_string_lossy().into_owned();

        assert!(resolve_path(&cwd, &raw).is_err());

        let mut config = Config::default();
        config.tools.external_directory = vec![outside.to_string_lossy().into_owned()];
        assert_eq!(
            resolve_path_in(&cwd, &raw, &config).unwrap(),
            target.canonicalize().unwrap()
        );

        // A sibling outside the allow-list is still rejected.
        let sibling = outside.with_file_name(format!(
            "{}-other",
            outside.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(resolve_path_in(&cwd, sibling.join("x").to_str().unwrap(), &config).is_err());

        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    #[tokio::test]
    async fn apply_patch_writes_and_rejects_escapes() {
        let cwd = temp_cwd("rw");
        let config = Config::default();

        let wrote = execute(
            "apply_patch",
            &json!({"patchText": "*** Begin Patch\n*** Add File: sub/b.txt\n+hi\n*** End Patch\n"}),
            &cwd,
            &config,
        )
        .await;
        assert!(wrote.ok, "{}", wrote.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("sub/b.txt")).unwrap(),
            "hi\n"
        );

        let escaped = execute(
            "apply_patch",
            &json!({"patchText": "*** Begin Patch\n*** Add File: ../nope.txt\n+x\n*** End Patch\n"}),
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
            write_plan("exec_command", &json!({"command": "true"}), &cwd, &config)
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
            write_plan("exec_command", &json!({"command": "true"}), &cwd, &config)
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
            "exec_command",
            &json!({"command": "printf hi"}),
            &cwd,
            &config,
        )
        .await;
        assert!(ok.ok);
        assert_eq!(ok.output, "hi");

        let failed = execute(
            "exec_command",
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
            "exec_command",
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
            "exec_command",
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
            "exec_command",
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
            "exec_command",
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
    fn linked_worktree_git_metadata_is_mounted() {
        if !sandbox::bwrap_available() {
            return;
        }
        let base = temp_cwd("worktree-git");
        let root = base.join("worktree");
        let sub = root.join("go");
        let git_dir = base.join("repo/.git/worktrees/example");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();

        assert_eq!(
            worktree_git_dir(&root),
            Some(git_dir.canonicalize().unwrap())
        );
        let plan = command_plan(
            "git status --short",
            None,
            &sub,
            Some(&root),
            &Config::default(),
        )
        .unwrap();
        let args: Vec<String> = plan
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--bind", root.to_str().unwrap()]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--bind", git_dir.to_str().unwrap()]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--chdir", sub.to_str().unwrap()]),
            "{args:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn sandbox_git_works_in_a_linked_worktree_subdirectory() {
        if !sandbox::bwrap_available()
            || std::process::Command::new("git")
                .arg("--version")
                .output()
                .is_err()
        {
            return;
        }
        let base = temp_cwd("sandbox-git-worktree");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("README"), "x").unwrap();
        git(&["add", "README"]);
        git(&[
            "-c",
            "user.name=eix",
            "-c",
            "user.email=eix@example.invalid",
            "commit",
            "-qm",
            "init",
        ]);
        let worktree = base.join("worktree");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "sandbox-test",
            worktree.to_str().unwrap(),
        ]);
        let sub = worktree.join("go");
        std::fs::create_dir_all(&sub).unwrap();

        let out = execute_in(
            "exec_command",
            &json!({"command": "git status --short"}),
            &sub,
            &Config::default(),
            None,
            Some(&worktree),
        )
        .await;
        assert!(out.ok, "{}", out.output);
        assert!(
            !out.output.contains("not a valid repository"),
            "{}",
            out.output
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tool_specs_cover_every_definition_and_keep_policy_consistent() {
        let definitions = definitions();
        for tool in &definitions {
            let name = tool.function.name.as_str();
            assert!(is_known(name), "{name} has no ToolSpec");
        }
        for spec in TOOL_SPECS {
            assert!(
                definitions
                    .iter()
                    .any(|tool| tool.function.name == spec.name),
                "{} has a ToolSpec but no definition",
                spec.name
            );
            assert!(
                !(spec.subagent_allowed && hard_mutating(spec.name)),
                "{} would let a read-only sub-agent mutate the workspace",
                spec.name
            );
            assert!(
                !(spec.parallel_safe && hard_mutating(spec.name)),
                "{} cannot be both mutating and parallel-safe",
                spec.name
            );
        }
        // The derived lists agree with the table.
        for tool in readonly_definitions() {
            let name = tool.function.name.as_str();
            assert!(spec(name).is_some_and(|spec| spec.subagent_allowed));
        }
        for spec in TOOL_SPECS {
            assert_eq!(parallel_safe(spec.name), spec.parallel_safe);
        }
    }

    #[tokio::test]
    async fn exec_command_cancellation_kills_the_process() {
        let cwd = temp_cwd("cancel-command");
        let cancel = Arc::new(AtomicBool::new(false));
        let task = {
            let cwd = cwd.clone();
            let cancel = Arc::clone(&cancel);
            tokio::spawn(async move {
                let config = Config {
                    tools: eix_core::ToolsConfig {
                        command_timeout_secs: 30,
                        ..eix_core::ToolsConfig::default()
                    },
                    ..Config::default()
                };
                execute_with_cancel(
                    "exec_command",
                    &json!({"command": "sleep 30"}),
                    &cwd,
                    &config,
                    Some(cancel.as_ref()),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.store(true, Ordering::SeqCst);
        let outcome = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancelled command must not wait for the timeout")
            .expect("task panicked");
        assert!(!outcome.ok, "{}", outcome.output);
        assert!(outcome.output.contains("cancelled"), "{}", outcome.output);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn summaries_name_the_target() {
        assert_eq!(
            summarize("view_image", &json!({"path": "docs/a.png"})),
            "view_image docs/a.png"
        );
        assert!(summarize("exec_command", &json!({"cmd": "rg TODO"})).starts_with("exec_command:"));
        assert_eq!(
            summarize(
                "apply_patch",
                &json!({"patchText": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch"})
            ),
            "apply_patch (1 file)"
        );
        assert!(
            summarize("plugin__acme__deploy", &json!({"env": "prod"})).contains("\"env\":\"prod\"")
        );
        assert!(is_known("ask_user"));
        assert!(is_known("apply_patch"));
        assert!(hard_mutating("apply_patch"));
        assert!(hard_mutating("write_stdin"));
        assert!(!hard_mutating("exec_command"));
        assert!(!hard_mutating("view_image"));
    }

    #[test]
    fn ask_user_parses_options_and_rejects_bad_input() {
        let (question, options) = ask_user_args(&json!({
            "question": "Which store?",
            "options": [
                {"label": "SQLite", "description": "embedded"},
                "Postgres"
            ]
        }))
        .expect("valid args");
        assert_eq!(question, "Which store?");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label, "SQLite");
        assert_eq!(options[0].description.as_deref(), Some("embedded"));
        assert_eq!(options[1].label, "Postgres");
        assert_eq!(options[1].description, None);

        // Too few, too many, empty labels and a missing question all fail.
        assert!(ask_user_args(&json!({"question": "q", "options": ["only"]})).is_err());
        assert!(ask_user_args(&json!({"question": "q", "options": ["a", ""]})).is_err());
        assert!(ask_user_args(&json!({"question": "  ", "options": ["a", "b"]})).is_err());
        let seven: Vec<String> = (0..7).map(|n| format!("o{n}")).collect();
        assert!(ask_user_args(&json!({"question": "q", "options": seven})).is_err());
    }

    #[test]
    fn format_read_pages_with_line_numbers() {
        let text = "a\nb\nc\nd\ne\n";
        let page = format_read(text, 2, 2, 4096);
        assert!(page.contains("2: b"), "{page}");
        assert!(page.contains("3: c"), "{page}");
        assert!(!page.contains("1: a"), "{page}");
        assert!(page.contains("2 more lines"), "{page}");

        let past = format_read(text, 99, 2, 4096);
        assert!(past.contains("past the end"), "{past}");
    }

    #[tokio::test]
    async fn apply_patch_adds_and_updates_multiple_files() {
        let cwd = temp_cwd("patch");
        std::fs::write(cwd.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let config = Config::default();
        let text = "\
*** Begin Patch
*** Update File: a.txt
@@
 one
-two
+TWO
 three
*** Add File: sub/b.txt
+hello
*** End Patch
";
        let preview = patch_preview(text, &cwd, &config).await.expect("preview");
        assert!(preview.contains("-two"), "{preview}");
        assert!(preview.contains("+TWO"), "{preview}");
        assert!(preview.contains("+hello"), "{preview}");

        let applied = execute("apply_patch", &json!({"patchText": text}), &cwd, &config).await;
        assert!(applied.ok, "{}", applied.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("sub/b.txt")).unwrap(),
            "hello\n"
        );

        let missing = execute(
            "apply_patch",
            &json!({"patchText": "*** Begin Patch\n*** Update File: a.txt\n@@\n-nope\n+x\n*** End Patch\n"}),
            &cwd,
            &config,
        )
        .await;
        assert!(!missing.ok);
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
