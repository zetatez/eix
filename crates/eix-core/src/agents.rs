//! Agent definitions loaded from Markdown files.
//!
//! Built-in agents cover the common modes. User agents live next to custom
//! commands:
//!
//! ```text
//! ~/.config/eix/agents/reviewer.md
//! <project>/.eix/agents/reviewer.md
//! <project>/.opencode/agents/reviewer.md   (OpenCode compatibility)
//! ```
//!
//! Frontmatter keys: `description`, `mode` (`primary` / `subagent` / `all`),
//! `model`, and `permission` (`read-only` / `workspace-write` / `full`). The
//! Markdown body becomes additional system instructions for the agent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::CompatConfig;
use crate::session::PermissionLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Can be selected as the session's primary agent.
    Primary,
    /// Can only be used by `spawn_agent`.
    Subagent,
    /// Both.
    All,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Subagent => "subagent",
            Self::All => "all",
        }
    }

    pub fn can_primary(self) -> bool {
        matches!(self, Self::Primary | Self::All)
    }

    pub fn can_subagent(self) -> bool {
        matches!(self, Self::Subagent | Self::All)
    }

    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "primary" | "main" => Self::Primary,
            "subagent" | "sub" => Self::Subagent,
            _ => Self::All,
        }
    }
}

/// One agent definition. `prompt` is appended to the built-in system prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSpec {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub mode: AgentMode,
    pub model: Option<String>,
    pub permission: Option<PermissionLevel>,
    pub builtin: bool,
    #[serde(default)]
    pub source: Option<PathBuf>,
}

/// Agents every installation has, in menu order.
pub fn builtin_agents() -> Vec<AgentSpec> {
    vec![
        AgentSpec {
            name: "build".to_string(),
            description: "default coding agent".to_string(),
            prompt: String::new(),
            mode: AgentMode::All,
            model: None,
            permission: None,
            builtin: true,
            source: None,
        },
        AgentSpec {
            name: "plan".to_string(),
            description: "inspect and design without editing files".to_string(),
            prompt: "You are a planning agent. Inspect the workspace, reason about the \
                     design, and produce a concrete plan. Do not modify files or run \
                     commands that change state."
                .to_string(),
            mode: AgentMode::Primary,
            model: None,
            permission: Some(PermissionLevel::ReadOnly),
            builtin: true,
            source: None,
        },
        AgentSpec {
            name: "explore".to_string(),
            description: "read-only search and analysis for sub-agents".to_string(),
            prompt: "Search and read the workspace, then report findings concisely. \
                     Do not modify files or run commands that change state."
                .to_string(),
            mode: AgentMode::Subagent,
            model: None,
            permission: Some(PermissionLevel::ReadOnly),
            builtin: true,
            source: None,
        },
    ]
}

/// Every agent visible from `cwd`. Project agents override global agents with
/// the same name, and user agents override built-ins.
pub fn load_agents(cwd: &Path, compat: &CompatConfig) -> Vec<AgentSpec> {
    let mut by_name: BTreeMap<String, AgentSpec> = builtin_agents()
        .into_iter()
        .map(|agent| (agent.name.clone(), agent))
        .collect();

    for source in crate::compat::agent_sources(cwd, compat) {
        collect_from(&source, &source, &mut by_name);
    }
    by_name.into_values().collect()
}

/// Look up one agent by name, reloading from disk so edits apply immediately.
pub fn find_agent(cwd: &Path, name: &str, compat: &CompatConfig) -> Option<AgentSpec> {
    load_agents(cwd, compat)
        .into_iter()
        .find(|agent| agent.name == name)
}

fn collect_from(source: &Path, dir: &Path, out: &mut BTreeMap<String, AgentSpec>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_from(source, &path, out);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(source) else {
            continue;
        };
        let Some(name) = agent_name(relative) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (meta, body) = split_frontmatter(&text);
        let description = meta
            .get("description")
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("custom agent {name}"));
        out.insert(
            name.clone(),
            AgentSpec {
                name,
                description,
                prompt: body.trim().to_string(),
                mode: meta
                    .get("mode")
                    .map(|value| AgentMode::parse(value))
                    .unwrap_or(AgentMode::All),
                model: meta.get("model").cloned().filter(|value| !value.is_empty()),
                permission: meta
                    .get("permission")
                    .and_then(|value| value.trim().parse().ok()),
                builtin: false,
                source: Some(path),
            },
        );
    }
}

fn agent_name(relative: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let part = component.as_os_str().to_string_lossy();
        if part.is_empty() {
            continue;
        }
        parts.push(part.to_string());
    }
    let last = parts.last_mut()?;
    let stem = last.strip_suffix(".md")?.to_string();
    if stem.is_empty() {
        return None;
    }
    *last = stem;
    Some(parts.join("/"))
}

fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut meta = BTreeMap::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (meta, text);
    }
    let mut body_start = None;
    let mut offset = text.find('\n').map(|index| index + 1).unwrap_or(text.len());
    for line in lines {
        offset += line.len() + 1;
        if line.trim() == "---" {
            body_start = Some(offset.min(text.len()));
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !key.is_empty() {
                meta.insert(key, value);
            }
        }
    }
    match body_start {
        Some(start) => (meta, &text[start..]),
        None => (BTreeMap::new(), text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("eix-agents-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(dir.join(".eix/agents")).unwrap();
        dir
    }

    #[test]
    fn builtins_are_available() {
        let agents = builtin_agents();
        assert!(agents.iter().any(|agent| agent.name == "build"));
        let plan = agents.iter().find(|agent| agent.name == "plan").unwrap();
        assert_eq!(plan.permission, Some(PermissionLevel::ReadOnly));
        assert!(plan.mode.can_primary());
        assert!(!plan.mode.can_subagent());
    }

    #[test]
    fn loads_compatible_agent_definitions() {
        let cwd = temp_dir("compat");
        for (dir, name) in [
            (".claude/agents", "claude"),
            (".opencode/agents", "opencode"),
            (".gemini/agents", "gemini"),
        ] {
            std::fs::create_dir_all(cwd.join(dir)).unwrap();
            std::fs::write(
                cwd.join(dir).join(format!("{name}.md")),
                format!("---\ndescription: {name} agent\n---\n{name} prompt\n"),
            )
            .unwrap();
        }
        let agents = load_agents(&cwd, &CompatConfig::all());
        for name in ["claude", "opencode", "gemini"] {
            assert!(agents.iter().any(|agent| agent.name == name), "{name}");
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn loads_custom_agents_and_overrides_builtins() {
        let cwd = temp_dir("load");
        std::fs::write(
            cwd.join(".eix/agents/reviewer.md"),
            "---\ndescription: Review changes\nmode: subagent\nmodel: openai/gpt-5\npermission: read-only\n---\n\nFind bugs.\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".eix/agents/build.md"),
            "---\ndescription: custom build\n---\n\nBe terse.\n",
        )
        .unwrap();

        let agents = load_agents(&cwd, &CompatConfig::default());
        let reviewer = agents
            .iter()
            .find(|agent| agent.name == "reviewer")
            .expect("reviewer");
        assert_eq!(reviewer.description, "Review changes");
        assert_eq!(reviewer.mode, AgentMode::Subagent);
        assert_eq!(reviewer.permission, Some(PermissionLevel::ReadOnly));
        assert_eq!(reviewer.prompt, "Find bugs.");
        let build = agents.iter().find(|agent| agent.name == "build").unwrap();
        assert!(!build.builtin);
        assert_eq!(build.description, "custom build");
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
