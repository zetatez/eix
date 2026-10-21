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
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

/// Hard cap on provider <-> tool round trips for one user turn. Long agent
/// tasks routinely need more than a dozen rounds; this still bounds runaway
/// loops while leaving enough headroom.
pub const MAX_TOOL_ROUNDS: usize = 64;

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
            "Read a UTF-8 text file inside the session workspace. Returns numbered lines. Use offset/limit to page through large files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace."},
                    "offset": {"type": "integer", "description": "First line to return (1-based, default 1)."},
                    "limit": {"type": "integer", "description": "Maximum number of lines to return (default 2000)."}
                },
                "required": ["path"]
            }),
        ),
        ToolDefinition::function(
            "use_skill",
            "Load the full instructions of an installed skill by name. Use this before work that matches a skill description in the system prompt.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Skill name exactly as listed in <available_skills>."},
                    "arguments": {"type": "string", "description": "Optional arguments for $ARGUMENTS and $1..$9 in the skill body."}
                },
                "required": ["name"]
            }),
        ),
        ToolDefinition::function(
            "todo",
            "Replace the session's task list. Use short, verifiable items and update statuses as work progresses.",
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
            "webfetch",
            "Fetch an http(s) URL and return readable text (HTML is stripped). Truncated to roughly 100 KB.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http:// or https:// URL."}
                },
                "required": ["url"]
            }),
        ),
        ToolDefinition::function(
            "websearch",
            "Search the web and return result titles and URLs. Uses DuckDuckGo's HTML endpoint; no API key.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "max_results": {"type": "integer", "description": "Default 5, maximum 10."}
                },
                "required": ["query"]
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
            "grep",
            "Search file contents with a regular expression (or literal text) inside the session workspace. Returns path:line: preview.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression, or literal text when literal is true."},
                    "path": {"type": "string", "description": "Directory to search (default .)."},
                    "include": {"type": "string", "description": "Only search paths matching this glob, e.g. *.rs or src/**."},
                    "literal": {"type": "boolean", "description": "Treat pattern as literal text (default false)."},
                    "caseSensitive": {"type": "boolean", "description": "Case-sensitive search (default true)."},
                    "limit": {"type": "integer", "description": "Maximum matching lines (default 100)."}
                },
                "required": ["pattern"]
            }),
        ),
        ToolDefinition::function(
            "glob",
            "Find file paths matching a glob pattern inside the session workspace, e.g. **/*.rs.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern relative to path, e.g. **/*.ts."},
                    "path": {"type": "string", "description": "Directory to search (default .)."},
                    "hidden": {"type": "boolean", "description": "Include hidden files and directories (default false)."},
                    "limit": {"type": "integer", "description": "Maximum paths to return (default 100)."}
                },
                "required": ["pattern"]
            }),
        ),
        ToolDefinition::function(
            "lsp_diagnostics",
            "Return language-server diagnostics for one file. Read-only; returns no output when no language server is configured for the extension.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File to check."}
                },
                "required": ["path"]
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
            "edit",
            "Replace text in an existing file inside the session workspace. oldString must match exactly once unless replaceAll is true. Requires approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "oldString": {"type": "string", "description": "Exact text to replace."},
                    "newString": {"type": "string", "description": "Replacement text (must differ from oldString)."},
                    "replaceAll": {"type": "boolean", "description": "Replace every occurrence (default false)."}
                },
                "required": ["path", "oldString", "newString"]
            }),
        ),
        ToolDefinition::function(
            "patch",
            "Apply a multi-file patch inside the session workspace. Requires approval. Use the *** Begin Patch / *** Add File / *** Update File / *** Delete File / *** End Patch format.",
            json!({
                "type": "object",
                "properties": {
                    "patchText": {"type": "string", "description": "Patch text in the apply_patch format."}
                },
                "required": ["patchText"]
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
                    "name": {"type": "string", "description": "Optional short label, e.g. `docs`."},
                    "agent": {"type": "string", "description": "Optional sub-agent name from /agents, e.g. `explore` or a project Markdown agent."}
                },
                "required": ["prompt"]
            }),
        ),
        ToolDefinition::function(
            "ask_user",
            "Ask the user a multiple-choice question when a decision is genuinely theirs: ambiguous requirements, risky or destructive choices, or a preference you cannot infer. Provide 2-6 options ordered best first; the first is shown as the recommendation and Enter selects it. Blocks until the user answers.",
            json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "A short, specific question."},
                    "options": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 6,
                        "description": "Best option first; the first is the recommendation.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string"},
                                "description": {"type": "string", "description": "Optional one-line trade-off or detail."}
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
            "List resources exposed by configured MCP servers. Read-only. Omit `server` to list every server.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "Configured MCP server name."}
                }
            }),
        ),
        ToolDefinition::function(
            "mcp_read_resource",
            "Read one MCP resource by URI. Read-only; binary resources are summarized rather than expanded.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "Configured MCP server name."},
                    "uri": {"type": "string", "description": "Resource URI from mcp_list_resources."}
                },
                "required": ["server", "uri"]
            }),
        ),
        ToolDefinition::function(
            "mcp_list_prompts",
            "List prompt templates exposed by configured MCP servers. Read-only. Omit `server` to list every server.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "Configured MCP server name."}
                }
            }),
        ),
        ToolDefinition::function(
            "mcp_get_prompt",
            "Expand one MCP prompt template. Read-only. `arguments` is an object matching the prompt's declared arguments.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "Configured MCP server name."},
                    "name": {"type": "string", "description": "Prompt name from mcp_list_prompts."},
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
        .filter(|tool| {
            matches!(
                tool.function.name.as_str(),
                "read_file"
                    | "list_dir"
                    | "grep"
                    | "glob"
                    | "lsp_diagnostics"
                    | "use_skill"
                    | "todo"
                    | "webfetch"
                    | "websearch"
                    | "process_output"
                    | "mcp_list_resources"
                    | "mcp_read_resource"
                    | "mcp_list_prompts"
                    | "mcp_get_prompt"
            )
        })
        .collect()
}

pub fn is_known(tool: &str) -> bool {
    matches!(
        tool,
        "read_file"
            | "list_dir"
            | "grep"
            | "glob"
            | "lsp_diagnostics"
            | "use_skill"
            | "todo"
            | "webfetch"
            | "websearch"
            | "write_file"
            | "edit"
            | "patch"
            | "run_command"
            | "start_command"
            | "process_output"
            | "spawn_agent"
            | "ask_user"
            | "mcp_list_resources"
            | "mcp_read_resource"
            | "mcp_list_prompts"
            | "mcp_get_prompt"
    )
}

/// Mutating tools need an approval decision under the default `ask` policy.
pub fn requires_approval(tool: &str) -> bool {
    matches!(
        tool,
        "write_file" | "edit" | "patch" | "run_command" | "start_command"
    )
}

/// One-line description shown in the approval card / notification.
pub fn summarize(tool: &str, args: &Value) -> String {
    match tool {
        "read_file" => format!("read_file {}", string_arg(args, "path")),
        "list_dir" => {
            let path = optional_string(args, "path").unwrap_or(".");
            format!("list_dir {path}")
        }
        "grep" => {
            let pattern = string_arg(args, "pattern");
            let path = optional_string(args, "path").unwrap_or(".");
            format!("grep {:?} in {path}", truncate(&pattern, 80))
        }
        "glob" => {
            let pattern = string_arg(args, "pattern");
            let path = optional_string(args, "path").unwrap_or(".");
            format!("glob {:?} in {path}", truncate(&pattern, 80))
        }
        "lsp_diagnostics" => format!("lsp_diagnostics {}", string_arg(args, "path")),
        "edit" => {
            let path = string_arg(args, "path");
            let old = string_arg(args, "oldString");
            format!("edit {path} ({})", truncate(&old, 60))
        }
        "patch" => {
            let text = string_arg(args, "patchText");
            let files = text
                .lines()
                .filter(|line| {
                    line.starts_with("*** Add File:")
                        || line.starts_with("*** Update File:")
                        || line.starts_with("*** Delete File:")
                })
                .count();
            format!("patch ({files} file{})", if files == 1 { "" } else { "s" })
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
        "ask_user" => {
            let question = string_arg(args, "question");
            format!("ask_user: {}", truncate(&question, 100))
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
        "grep" => grep(args, cwd, config).await,
        "glob" => glob_files(args, cwd, config),
        "use_skill" => use_skill(args, cwd),
        "webfetch" => webfetch(args).await,
        "websearch" => websearch(args).await,
        "todo" => Err(anyhow::anyhow!("todo is handled by the daemon")),
        "write_file" => write_file(args, cwd, config).await,
        "edit" => edit(args, cwd, config).await,
        "patch" => patch(args, cwd, config).await,
        "run_command" => run_command(args, cwd, config).await,
        "start_command" | "process_output" => {
            Err(anyhow::anyhow!("{tool} is handled by the daemon"))
        }
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

/// Run the configured formatter on a file that was just written. Returns an
/// optional note for the tool output; failures never undo the write.
pub async fn format_file(config: &Config, cwd: &Path, path: &Path) -> Option<String> {
    let formatter = &config.formatter;
    if !formatter.applies_to(path) {
        return None;
    }
    let command = formatter.command.as_deref()?.trim();
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
    match tokio::time::timeout(
        Duration::from_secs(formatter.timeout_secs),
        process.output(),
    )
    .await
    {
        Err(_) => Some(format!(
            "\n[formatter timed out after {}s]",
            formatter.timeout_secs
        )),
        Ok(Err(err)) => Some(format!("\n[formatter failed: {err}]")),
        Ok(Ok(output)) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            Some(if stderr.is_empty() {
                format!("\n[formatter exited with {}]", output.status)
            } else {
                format!(
                    "\n[formatter failed: {}]",
                    cap_output(stderr.to_string(), 2000)
                )
            })
        }
        Ok(Ok(_)) => None,
    }
}

/// Minimal HTML -> text conversion for `webfetch` / `websearch`.
fn html_to_text(html: &str) -> String {
    let scripts =
        Regex::new(r"(?is)<(script|style)[^>]*>.*?</(script|style)>").expect("static regex");
    let without_scripts = scripts.replace_all(html, " ");
    let tags = Regex::new(r"(?s)<[^>]+>").expect("static regex");
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

async fn web_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("eix/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot build HTTP client")
}

async fn webfetch(args: &Value) -> Result<String> {
    let url = required_string(args, "url")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("url must start with http:// or https://");
    }
    let response = web_client()
        .await?
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
    let response = web_client()
        .await?
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
    let link =
        Regex::new(r#"(?is)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("static regex");
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

fn use_skill(args: &Value, cwd: &Path) -> Result<String> {
    let name = required_string(args, "name")?;
    let arguments = args
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match eix_core::find_skill(cwd, name) {
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
            let names: Vec<String> = eix_core::load_skills(cwd)
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

async fn read_file(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let raw = required_string(args, "path")?;
    let path = resolve_path_in(cwd, raw, config)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read {}", display(&path, cwd)))?;
    let text = String::from_utf8_lossy(&bytes);
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(2000)
        .max(1) as usize;
    Ok(format_read(
        &text,
        offset,
        limit,
        config.tools.max_output_bytes,
    ))
}

/// Render a numbered, paginated slice of a text file. `offset` is 1-based.
pub fn format_read(text: &str, offset: usize, limit: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return "(empty file)".to_string();
    }
    if offset > lines.len() {
        return format!(
            "(offset {offset} is past the end of the file, {} lines)",
            lines.len()
        );
    }
    let start = offset - 1;
    let end = (start + limit).min(lines.len());
    let mut out = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{}: {line}\n", start + index + 1));
        if out.len() >= max_bytes {
            out.push_str("… [truncated]\n");
            break;
        }
    }
    if end < lines.len() {
        out.push_str(&format!(
            "… [{} more lines; use offset={} to continue]\n",
            lines.len() - end,
            end + 1
        ));
    }
    cap_output(out, max_bytes)
}

/// Recursively collect files below `root`, skipping VCS/build directories.
fn walk_files(root: &Path, hidden: bool, max_files: usize) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        ".worktrees",
        ".hg",
        ".svn",
    ];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && !hidden {
                continue;
            }
            if SKIP.contains(&name.as_ref()) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                out.push(path);
                if out.len() >= max_files {
                    return out;
                }
            }
        }
        if out.len() >= max_files {
            break;
        }
    }
    out
}

fn relative_label(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

async fn grep(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let pattern = required_string(args, "pattern")?;
    if pattern.is_empty() {
        bail!("pattern must not be empty");
    }
    let raw = optional_string(args, "path").unwrap_or(".");
    let root = resolve_path_in(cwd, raw, config)?;
    if !root.is_dir() {
        bail!("{} is not a directory", display(&root, cwd));
    }
    let literal = args
        .get("literal")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let case_sensitive = args
        .get("caseSensitive")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .max(1) as usize;
    let include = optional_string(args, "include").map(str::to_string);
    let expression = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let regex = RegexBuilder::new(&expression)
        .case_insensitive(!case_sensitive)
        .build()
        .with_context(|| format!("invalid pattern {pattern:?}"))?;

    let root = root.canonicalize().unwrap_or(root);
    let files = walk_files(&root, false, 20_000);
    let mut matches = Vec::new();
    for path in files {
        let label = relative_label(&path, cwd);
        if let Some(include) = &include {
            let pattern = glob::Pattern::new(include)
                .with_context(|| format!("invalid include glob {include:?}"))?;
            if !pattern.matches(&label)
                && !path
                    .file_name()
                    .map(|name| pattern.matches(&name.to_string_lossy()))
                    .unwrap_or(false)
            {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.iter().take(8_000).any(|byte| *byte == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(format!("{label}:{}: {}", index + 1, truncate(line, 300)));
                if matches.len() >= limit {
                    break;
                }
            }
        }
        if matches.len() >= limit {
            break;
        }
    }
    if matches.is_empty() {
        return Ok("(no matches)".to_string());
    }
    Ok(cap_output(
        matches.join("\n"),
        config.tools.max_output_bytes,
    ))
}

fn glob_files(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let pattern = required_string(args, "pattern")?;
    if pattern.is_empty() {
        bail!("pattern must not be empty");
    }
    let raw = optional_string(args, "path").unwrap_or(".");
    let root = resolve_path_in(cwd, raw, config)?;
    if !root.is_dir() {
        bail!("{} is not a directory", display(&root, cwd));
    }
    let hidden = args.get("hidden").and_then(Value::as_bool).unwrap_or(false);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .max(1) as usize;
    let root = root.canonicalize().unwrap_or(root);
    let joined = root.join(pattern);
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: !hidden,
    };
    let mut out = Vec::new();
    let entries = glob::glob_with(&joined.to_string_lossy(), options)
        .with_context(|| format!("invalid glob {pattern:?}"))?;
    for entry in entries.flatten() {
        let Ok(real) = entry.canonicalize() else {
            continue;
        };
        if !real.starts_with(&root) {
            continue;
        }
        if !hidden
            && real
                .components()
                .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }
        out.push(relative_label(&real, cwd));
        if out.len() >= limit {
            break;
        }
    }
    out.sort();
    if out.is_empty() {
        return Ok("(no matches)".to_string());
    }
    Ok(cap_output(out.join("\n"), config.tools.max_output_bytes))
}

async fn edit(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let raw = required_string(args, "path")?;
    let old_string = required_string(args, "oldString")?;
    let new_string = required_string(args, "newString")?;
    let replace_all = args
        .get("replaceAll")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = resolve_path_in(cwd, raw, config)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read {}", display(&path, cwd)))?;
    let old = String::from_utf8_lossy(&bytes);
    let new = apply_edit(&old, old_string, new_string, replace_all)?;
    tokio::fs::write(&path, new.as_bytes())
        .await
        .with_context(|| format!("cannot write {}", display(&path, cwd)))?;
    let mut message = format!("edited {} ({} bytes)", display(&path, cwd), new.len());
    if let Some(note) = format_file(config, cwd, &path).await {
        message.push_str(&note);
    }
    Ok(message)
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

/// Preview a multi-file patch as one unified diff.
pub async fn patch_preview(patch_text: &str, cwd: &Path, config: &Config) -> Result<String> {
    let ops = patch_ops(patch_text, cwd, config).await?;
    let mut out = String::new();
    for op in ops {
        if op.delete {
            out.push_str(&format!("--- a/{}\n+++ /dev/null\n", op.label));
            for line in op.old.lines() {
                out.push_str(&format!("-{line}\n"));
            }
        } else {
            out.push_str(&unified_diff(&op.label, &op.old, &op.new));
        }
    }
    Ok(cap_output(out, config.tools.max_output_bytes))
}

async fn patch(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let patch_text = required_string(args, "patchText")?;
    let ops = patch_ops(patch_text, cwd, config).await?;
    // Validate every target before writing anything.
    for op in &ops {
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
    let mut formatted: Vec<PathBuf> = Vec::new();
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
            formatted.push(op.path);
        }
    }
    let mut message = format!("patched {written} file(s), deleted {deleted} file(s)");
    for path in &formatted {
        if let Some(note) = format_file(config, cwd, path).await {
            message.push_str(&note);
        }
    }
    Ok(message)
}

async fn list_dir(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let raw = optional_string(args, "path").unwrap_or(".");
    let path = resolve_path_in(cwd, raw, config)?;
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

async fn write_file(args: &Value, cwd: &Path, config: &Config) -> Result<String> {
    let raw = required_string(args, "path")?;
    let content = required_string(args, "content")?;
    let path = resolve_path_in(cwd, raw, config)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create {}", display(parent, cwd)))?;
    }
    tokio::fs::write(&path, content.as_bytes())
        .await
        .with_context(|| format!("cannot write {}", display(&path, cwd)))?;
    let mut message = format!("wrote {} bytes to {}", content.len(), display(&path, cwd));
    if let Some(note) = format_file(config, cwd, &path).await {
        message.push_str(&note);
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

/// Resolve a shell command into a concrete (possibly sandboxed) invocation.
pub fn command_plan(
    command: &str,
    cwd_arg: Option<&str>,
    session_cwd: &Path,
    config: &Config,
) -> Result<CommandPlan> {
    let command_cwd = match cwd_arg {
        Some(raw) => resolve_path_in(session_cwd, raw, config)?,
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
    fn html_and_url_helpers_are_conservative() {
        assert_eq!(
            html_to_text("<h1>Hi &amp; bye</h1><script>x()</script> text"),
            "Hi & bye text"
        );
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(percent_decode("a%20b%26c"), "a b&c");
    }

    #[tokio::test]
    async fn webfetch_reads_html_from_a_local_server() {
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
            "webfetch",
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
        let mut config = Config::default();
        config.formatter.command = Some("sed -i 's/hello/HELLO/'".into());
        config.formatter.extensions = vec!["txt".into()];
        let outcome = execute(
            "write_file",
            &json!({"path": "note.txt", "content": "hello"}),
            &cwd,
            &config,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).unwrap(),
            "HELLO"
        );

        let outcome = execute(
            "write_file",
            &json!({"path": "note.md", "content": "hello"}),
            &cwd,
            &config,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.md")).unwrap(),
            "hello"
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
    async fn read_list_and_write() {
        let cwd = temp_cwd("rw");
        std::fs::write(cwd.join("a.txt"), "hello").unwrap();
        let config = Config::default();

        let read = execute("read_file", &json!({"path": "a.txt"}), &cwd, &config).await;
        assert!(read.ok);
        assert_eq!(read.output, "1: hello\n");

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
        assert!(requires_approval("edit"));
        assert!(requires_approval("patch"));
        assert!(!requires_approval("read_file"));
        assert!(!requires_approval("grep"));
        assert!(!requires_approval("glob"));
        assert!(is_known("ask_user"));
        assert!(is_known("grep"));
        assert!(!requires_approval("ask_user"));
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

    #[tokio::test]
    async fn read_file_pages_with_line_numbers() {
        let cwd = temp_cwd("read-page");
        std::fs::write(cwd.join("a.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let config = Config::default();

        let page = execute(
            "read_file",
            &json!({"path": "a.txt", "offset": 2, "limit": 2}),
            &cwd,
            &config,
        )
        .await;
        assert!(page.ok, "{}", page.output);
        assert!(page.output.contains("2: b"), "{}", page.output);
        assert!(page.output.contains("3: c"), "{}", page.output);
        assert!(!page.output.contains("1: a"), "{}", page.output);
        assert!(page.output.contains("2 more lines"), "{}", page.output);

        let past = execute(
            "read_file",
            &json!({"path": "a.txt", "offset": 99}),
            &cwd,
            &config,
        )
        .await;
        assert!(past.ok);
        assert!(past.output.contains("past the end"), "{}", past.output);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn grep_searches_contents_with_include_and_literal() {
        let cwd = temp_cwd("grep");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src/a.rs"), "fn main() {}\n// TODO: later\n").unwrap();
        std::fs::write(cwd.join("src/b.txt"), "TODO not rust\n").unwrap();
        let config = Config::default();

        let found = execute(
            "grep",
            &json!({"pattern": "TODO", "include": "*.rs"}),
            &cwd,
            &config,
        )
        .await;
        assert!(found.ok, "{}", found.output);
        assert!(found.output.contains("src/a.rs:2"), "{}", found.output);
        assert!(!found.output.contains("b.txt"), "{}", found.output);

        let literal = execute(
            "grep",
            &json!({"pattern": "fn main()", "literal": true, "caseSensitive": false}),
            &cwd,
            &config,
        )
        .await;
        assert!(literal.ok, "{}", literal.output);
        assert!(literal.output.contains("src/a.rs:1"), "{}", literal.output);

        let none = execute("grep", &json!({"pattern": "zzz"}), &cwd, &config).await;
        assert_eq!(none.output, "(no matches)");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn glob_finds_matching_paths_inside_the_workspace() {
        let cwd = temp_cwd("glob");
        std::fs::create_dir_all(cwd.join("src/nested")).unwrap();
        std::fs::write(cwd.join("src/a.rs"), "").unwrap();
        std::fs::write(cwd.join("src/nested/b.rs"), "").unwrap();
        std::fs::write(cwd.join("src/c.txt"), "").unwrap();
        let config = Config::default();

        let out = execute("glob", &json!({"pattern": "**/*.rs"}), &cwd, &config).await;
        assert!(out.ok, "{}", out.output);
        assert!(out.output.contains("src/a.rs"), "{}", out.output);
        assert!(out.output.contains("src/nested/b.rs"), "{}", out.output);
        assert!(!out.output.contains("c.txt"), "{}", out.output);

        let escaped = execute("glob", &json!({"pattern": "../*.rs"}), &cwd, &config).await;
        assert!(!escaped.output.contains(".."), "{}", escaped.output);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn edit_replaces_text_and_rejects_ambiguous_matches() {
        let cwd = temp_cwd("edit");
        std::fs::write(cwd.join("a.txt"), "one two one\n").unwrap();
        let config = Config::default();

        let ambiguous = execute(
            "edit",
            &json!({"path": "a.txt", "oldString": "one", "newString": "ONE"}),
            &cwd,
            &config,
        )
        .await;
        assert!(!ambiguous.ok);
        assert!(
            ambiguous.output.contains("matched 2 times"),
            "{}",
            ambiguous.output
        );

        let all = execute(
            "edit",
            &json!({"path": "a.txt", "oldString": "one", "newString": "ONE", "replaceAll": true}),
            &cwd,
            &config,
        )
        .await;
        assert!(all.ok, "{}", all.output);
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "ONE two ONE\n"
        );

        let plan = write_plan(
            "edit",
            &json!({"path": "a.txt", "oldString": "two", "newString": "TWO"}),
            &cwd,
            &config,
        )
        .await
        .expect("edit plan");
        assert!(plan.diff().contains("-ONE two ONE"), "{}", plan.diff());
        assert!(plan.diff().contains("+ONE TWO ONE"), "{}", plan.diff());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn patch_adds_and_updates_multiple_files() {
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

        let applied = execute("patch", &json!({"patchText": text}), &cwd, &config).await;
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
            "patch",
            &json!({"patchText": "*** Begin Patch\n*** Update File: a.txt\n@@\n-nope\n+x\n*** End Patch\n"}),
            &cwd,
            &config,
        )
        .await;
        assert!(!missing.ok);
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
