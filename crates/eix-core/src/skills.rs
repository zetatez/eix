//! Markdown skills: reusable instructions the model can load on demand.
//!
//! Layout (nearer project sources override global ones with the same name):
//!
//! ```text
//! ~/.config/eix/skills/review/SKILL.md
//! ~/.claude/skills/review/SKILL.md
//! <project>/.eix/skills/review/SKILL.md
//! <project>/.opencode/skills/review.md
//! <project>/.claude/skills/review/SKILL.md
//! ```
//!
//! Frontmatter supports `name`, `description` and `allowed-tools`; the body is
//! returned by the `use_skill` tool after `$ARGUMENTS` / `$1`..`$9` expansion.
//! Skills stay out of the prompt until the model asks for one, so a large skill
//! library does not consume the context budget.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::commands::{first_line, split_frontmatter};
use crate::config::CompatConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Tools the model may still use after loading this skill. Empty means no
    /// restriction. Names support `*` and `?` wildcards.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    pub source: PathBuf,
}

/// Skills visible from `cwd`, sorted by name.
pub fn load_skills(cwd: &Path, compat: &CompatConfig) -> Vec<Skill> {
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    for source in crate::compat::skill_sources(cwd, compat) {
        collect_from(&source, &mut by_name);
    }
    by_name.into_values().collect()
}

/// Find one skill by its exact name.
pub fn find_skill(cwd: &Path, name: &str, compat: &CompatConfig) -> Option<Skill> {
    load_skills(cwd, compat)
        .into_iter()
        .find(|skill| skill.name == name)
}

/// `<available_skills>` block appended to the system prompt. Empty when no
/// skills are installed.
pub fn available_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n<available_skills>\nCall the `use_skill` tool with a skill name to load its full instructions before doing work that matches it. Pass `arguments` when the skill uses `$ARGUMENTS` or `$1`..`$9`.\n",
    );
    for skill in skills {
        out.push_str(&format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n",
            skill.name, skill.description
        ));
        if !skill.allowed_tools.is_empty() {
            out.push_str(&format!(
                "    <allowed_tools>{}</allowed_tools>\n",
                skill.allowed_tools.join(", ")
            ));
        }
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>\n");
    out
}

/// Claude-style skills name tools as `Read`, `Grep`, `Bash`, ... Map the common
/// ones to eix's tool names; unknown names are lowercased (eix tools are all
/// lowercase) so `allowed-tools` remains predictable.
fn normalize_tool_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "read" => "read_file".to_string(),
        "write" => "write_file".to_string(),
        "multiedit" => "edit".to_string(),
        "bash" | "shell" => "run_command".to_string(),
        "ls" => "list_dir".to_string(),
        "task" => "spawn_agent".to_string(),
        "todowrite" => "todo".to_string(),
        "askuserquestion" => "ask_user".to_string(),
        "bashoutput" => "process_output".to_string(),
        // eix tool names (and exposed MCP names) are lowercase.
        _ => lower,
    }
}

fn collect_from(dir: &Path, out: &mut BTreeMap<String, Skill>) {
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
            let candidate = path.join("SKILL.md");
            if candidate.is_file() {
                insert_skill(&candidate, out);
            } else {
                // Pi and Codex support nested skill directories (for example
                // `skills/group/name/SKILL.md`) and recursively scan them.
                collect_from(&path, out);
            }
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            insert_skill(&path, out);
        }
    }
}

fn insert_skill(skill_path: &Path, out: &mut BTreeMap<String, Skill>) {
    let Ok(text) = std::fs::read_to_string(skill_path) else {
        return;
    };
    let (meta, body) = split_frontmatter(&text);
    let body = body.trim().to_string();
    if body.is_empty() {
        return;
    }
    let fallback = skill_path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            skill_path
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let name = meta
        .get("name")
        .cloned()
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback);
    if name.is_empty() {
        return;
    }
    let description = meta
        .get("description")
        .cloned()
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| first_line(&body));
    let allowed_tools = meta
        .get("allowed-tools")
        .or_else(|| meta.get("allowed_tools"))
        .map(|list| {
            list.trim_matches(|ch| ch == '[' || ch == ']')
                .split([',', ' ', '\t'])
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .map(normalize_tool_name)
                .collect()
        })
        .unwrap_or_default();
    out.insert(
        name.clone(),
        Skill {
            name,
            description,
            body,
            allowed_tools,
            source: skill_path.to_path_buf(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("eix-skills-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_skills_and_nearer_layers_override() {
        let root = temp_dir("load");
        let project = root.join("repo");
        std::fs::create_dir_all(root.join(".eix/skills/review")).unwrap();
        std::fs::create_dir_all(project.join(".eix/skills/deploy")).unwrap();
        std::fs::write(
            root.join(".eix/skills/review/SKILL.md"),
            "---\ndescription: outer review\n---\nOuter body\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".eix/skills/review.md"),
            "---\nname: review\ndescription: inner review\n---\nInner body\n",
        )
        .unwrap();
        std::fs::write(project.join(".eix/skills/deploy/SKILL.md"), "Ship it.\n").unwrap();
        std::fs::create_dir_all(project.join(".claude/skills/audit")).unwrap();
        std::fs::write(
            project.join(".claude/skills/audit/SKILL.md"),
            "---\nallowed-tools: [Read, Grep]\n---\nAudit it.\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.join(".pi/skills/tools/lint")).unwrap();
        std::fs::write(
            project.join(".pi/skills/tools/lint/SKILL.md"),
            "---\ndescription: nested Pi skill\n---\nLint carefully.\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".claude/skills/review.md"),
            "---\nname: review\ndescription: claude review\n---\nClaude body\n",
        )
        .unwrap();

        // Claude compatibility is opt-in.
        assert!(!load_skills(&project, &CompatConfig::default())
            .iter()
            .any(|skill| skill.name == "audit"));

        let compat = CompatConfig {
            claude_code: true,
            pi: true,
            ..Default::default()
        };
        let skills = load_skills(&project, &compat);
        // Global sources may contribute real skills on the test machine; only
        // the project entries are asserted.
        assert!(skills.len() >= 3);
        let review = find_skill(&project, "review", &compat).expect("review");
        assert_eq!(review.description, "inner review");
        assert_eq!(review.body, "Inner body");
        assert!(review.source.ends_with(".eix/skills/review.md"));
        let deploy = find_skill(&project, "deploy", &compat).expect("deploy");
        assert_eq!(deploy.description, "Ship it.");
        let audit = find_skill(&project, "audit", &compat).expect("audit");
        assert_eq!(audit.allowed_tools, vec!["read_file", "grep"]);
        let lint = find_skill(&project, "lint", &compat).expect("nested Pi skill");
        assert_eq!(lint.description, "nested Pi skill");
        let prompt = available_prompt(&skills);
        assert!(prompt.contains("<name>review</name>"));
        assert!(prompt.contains("use_skill"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
