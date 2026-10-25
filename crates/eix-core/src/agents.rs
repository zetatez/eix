//! Agent definitions loaded from Markdown files.
//!
//! Agents are custom prompt/model bundles. The session's execution mode is
//! separate (`SessionMode`), so an agent never changes what a session may do.
//!
//! ```text
//! ~/.config/eix/agents/reviewer.md
//! <project>/.eix/agents/reviewer.md
//! <project>/.opencode/agents/reviewer.md   (OpenCode compatibility)
//! ```
//!
//! Frontmatter keys: `description`, `role` (`primary` / `subagent` / `both`),
//! and `model`. The Markdown body becomes additional system instructions for
//! the agent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::CompatConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Can be selected as the session's primary agent and used by
    /// `spawn_agent`.
    #[default]
    Both,
    /// Can only be selected as the session's primary agent.
    Primary,
    /// Can only be used by `spawn_agent`.
    Subagent,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Primary => "primary",
            Self::Subagent => "subagent",
        }
    }

    pub fn can_primary(self) -> bool {
        matches!(self, Self::Primary | Self::Both)
    }

    pub fn can_subagent(self) -> bool {
        matches!(self, Self::Subagent | Self::Both)
    }

    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "primary" | "main" => Self::Primary,
            "subagent" | "sub" => Self::Subagent,
            _ => Self::Both,
        }
    }
}

/// One agent definition. `prompt` is appended to the built-in system prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSpec {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub role: AgentRole,
    pub model: Option<String>,
    pub builtin: bool,
    #[serde(default)]
    pub source: Option<PathBuf>,
}

/// Agents every installation has, in menu order. Custom primary agents live in
/// Markdown files; the built-in writable/read-only workflows are session modes,
/// not agents.
pub fn builtin_agents() -> Vec<AgentSpec> {
    vec![AgentSpec {
        name: "explore".to_string(),
        description: "read-only search and analysis for sub-agents".to_string(),
        prompt:
            "Investigate one focused question in the workspace. Return findings with file:line \
                 evidence, the minimum context needed, and any uncertainty. Do not edit files, run \
                 mutating commands, ask questions, or narrate your process."
                .to_string(),
        role: AgentRole::Subagent,
        model: None,
        builtin: true,
        source: None,
    }]
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
                role: meta
                    .get("role")
                    .map(|value| AgentRole::parse(value))
                    .unwrap_or(AgentRole::Both),
                model: meta.get("model").cloned().filter(|value| !value.is_empty()),
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
    fn builtins_are_subagent_only() {
        let agents = builtin_agents();
        assert!(agents.iter().all(|agent| agent.name != "build"));
        assert!(agents.iter().all(|agent| agent.name != "plan"));
        let explore = agents.iter().find(|agent| agent.name == "explore").unwrap();
        assert_eq!(explore.role, AgentRole::Subagent);
        assert!(!explore.role.can_primary());
        assert!(explore.role.can_subagent());
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
            "---\ndescription: Review changes\nrole: subagent\nmodel: openai/gpt-5\n---\n\nFind bugs.\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".eix/agents/explore.md"),
            "---\ndescription: custom explore\nrole: primary\n---\n\nBe terse.\n",
        )
        .unwrap();

        let agents = load_agents(&cwd, &CompatConfig::default());
        let reviewer = agents
            .iter()
            .find(|agent| agent.name == "reviewer")
            .expect("reviewer");
        assert_eq!(reviewer.description, "Review changes");
        assert_eq!(reviewer.role, AgentRole::Subagent);
        assert_eq!(reviewer.prompt, "Find bugs.");
        let explore = agents.iter().find(|agent| agent.name == "explore").unwrap();
        assert!(!explore.builtin);
        assert_eq!(explore.description, "custom explore");
        assert_eq!(explore.role, AgentRole::Primary);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn legacy_mode_and_permission_frontmatter_are_ignored() {
        let cwd = temp_dir("legacy");
        std::fs::write(
            cwd.join(".eix/agents/reviewer.md"),
            "---\ndescription: Review changes\nmode: subagent\npermission: read-only\n---\n\nFind bugs.\n",
        )
        .unwrap();

        let agents = load_agents(&cwd, &CompatConfig::default());
        let reviewer = agents
            .iter()
            .find(|agent| agent.name == "reviewer")
            .expect("reviewer");
        // `role` is absent, so the new default applies; legacy keys are ignored.
        assert_eq!(reviewer.role, AgentRole::Both);
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
