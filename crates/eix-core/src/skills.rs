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

const FRONTEND_SKILL: &str = r#"# Frontend

Use when building or restyling a UI. Follow the existing design system and framework conventions first.

## Direction

- Start from the audience and domain: operational tools should feel quiet, dense, and scannable; games and editorial pages can be more expressive.
- Use purposeful typography; avoid default stacks (Inter, Roboto, Arial, system) unless the project already uses them.
- Set a clear visual direction with CSS variables; vary palettes and type across projects; avoid purple-on-white defaults, a default dark theme, and one-note palettes dominated by a single hue family.
- Build atmosphere with gradients, shapes, or subtle patterns instead of flat backgrounds; no floating orbs, bokeh blobs, or purely atmospheric media where the user needs to inspect the real thing.
- Use a few meaningful animations (page load, staggered reveal); avoid generic micro-motion everywhere.
- Avoid boilerplate hero sections, nested cards, and marketing layouts for tools. When a landing page needs a hero, carry it with a relevant real or generated image, not a gradient or SVG illustration, and leave a hint of the next section visible.
- Keep navigation predictable and common workflows ergonomic; make dense information easy to scan and compare.

## Controls and layout

- Use familiar controls: icons for tools, swatches for color, segmented controls for modes, toggles for binary settings, sliders or steppers for numbers, tabs for views, and text buttons only for clear commands.
- Prefer lucide or the project's icon library over hand-drawn SVG; add tooltips naming unfamiliar icons.
- Build the states a real user expects: empty, loading, error, disabled, and long-content cases.
- Reserve hero-scale type for heroes; keep headings small inside panels, cards, sidebars, and dashboards. Define fixed-format elements with aspect-ratio, grid tracks, or min/max sizing so hover states, labels, and icons cannot shift the layout.
- Ensure text fits its container at every viewport without overlapping other content; do not scale font size with viewport width; letter spacing stays 0.
- For 3D, use Three.js and make the primary scene full-bleed rather than a decorative card.

## Verify

- Run the app, not just the build. Start the dev server for anything that needs one, use another port if it is taken, and give the user the URL. For a plain HTML page, give the file path instead.
- Check desktop and mobile viewports. For visual or 3D work, verify with Playwright screenshots and canvas-pixel checks that the scene is nonblank, correctly framed, interactive, and that assets render without overlap.
- Check accessible contrast, focus states, keyboard paths, and reduced-motion support.
"#;

const ENGINEERING_SKILL: &str = r#"# Engineering judgment

Use for refactors, data-model changes, or work where structure, abstraction, or test scope matters.

- Prefer the repo's existing patterns, frameworks, and helper APIs over inventing a new abstraction.
- Use structured parsers and serializers for structured data instead of ad hoc string manipulation.
- Add an abstraction only when it removes real complexity, reduces meaningful duplication, or matches an established local pattern.
- Keep edits inside the modules and ownership boundaries implied by the request; leave unrelated refactors and metadata churn alone.
- Scale tests with risk and blast radius: focused for narrow changes, broader for shared behavior, cross-module contracts, and user-facing flows.
- Handle errors explicitly; don't swallow failures or turn them into silent fallbacks.
- Check call sites and invariants before changing a shared API; migrate all callers instead of leaving compatibility shims unless asked.
"#;

/// Skills every installation has. User and project skills with the same name
/// override them.
pub fn builtin_skills() -> Vec<Skill> {
    [
        (
            "frontend",
            "UI design guidance: typography, color, layout, motion, responsiveness, and browser verification.",
            FRONTEND_SKILL,
        ),
        (
            "engineering",
            "Deeper judgment for refactors, data models, abstractions, and test scope.",
            ENGINEERING_SKILL,
        ),
    ]
    .into_iter()
    .map(|(name, description, body)| Skill {
        name: name.to_string(),
        description: description.to_string(),
        body: body.trim().to_string(),
        allowed_tools: Vec::new(),
        source: PathBuf::from(format!("<builtin>/{name}/SKILL.md")),
    })
    .collect()
}

/// Skills visible from `cwd`, sorted by name.
pub fn load_skills(cwd: &Path, compat: &CompatConfig) -> Vec<Skill> {
    let mut by_name: BTreeMap<String, Skill> = builtin_skills()
        .into_iter()
        .map(|skill| (skill.name.clone(), skill))
        .collect();
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
        "read" | "ls" | "grep" | "glob" | "bash" | "shell" | "read_file" | "list_dir"
        | "run_command" => "exec_command".to_string(),
        "write" | "multiedit" | "edit" | "patch" | "write_file" => "apply_patch".to_string(),
        "webfetch" => "web_fetch".to_string(),
        "websearch" => "web_search".to_string(),
        "task" => "spawn_agent".to_string(),
        "todowrite" => "todo".to_string(),
        "askuserquestion" => "ask_user".to_string(),
        "bashoutput" => "write_stdin".to_string(),
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
                .fold(Vec::new(), |mut tools: Vec<String>, tool| {
                    if !tools.contains(&tool) {
                        tools.push(tool);
                    }
                    tools
                })
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
        assert_eq!(audit.allowed_tools, vec!["exec_command"]);
        let lint = find_skill(&project, "lint", &compat).expect("nested Pi skill");
        assert_eq!(lint.description, "nested Pi skill");
        let prompt = available_prompt(&skills);
        assert!(prompt.contains("<name>review</name>"));
        assert!(prompt.contains("use_skill"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn builtin_skills_are_listed_and_project_skills_override_them() {
        let builtins = builtin_skills();
        for name in ["frontend", "engineering"] {
            let skill = builtins
                .iter()
                .find(|skill| skill.name == name)
                .unwrap_or_else(|| panic!("missing builtin {name}"));
            assert!(!skill.body.is_empty());
            assert!(skill.source.starts_with("<builtin>"));
        }
        let frontend = builtins
            .iter()
            .find(|skill| skill.name == "frontend")
            .expect("frontend");
        for expected in [
            "dev server",
            "Playwright",
            "lucide",
            "Three.js",
            "aspect-ratio",
        ] {
            assert!(
                frontend.body.contains(expected),
                "frontend skill is missing {expected:?}"
            );
        }
        let prompt = available_prompt(&builtins);
        assert!(prompt.contains("<name>frontend</name>"));
        assert!(prompt.contains("<name>engineering</name>"));

        let root = temp_dir("builtin");
        std::fs::create_dir_all(root.join(".eix/skills/frontend")).unwrap();
        std::fs::write(
            root.join(".eix/skills/frontend/SKILL.md"),
            "---\nname: frontend\ndescription: local frontend\n---\nLocal body\n",
        )
        .unwrap();
        let frontend = find_skill(&root, "frontend", &CompatConfig::default()).expect("frontend");
        assert_eq!(frontend.description, "local frontend");
        assert_eq!(frontend.body, "Local body");
        let _ = std::fs::remove_dir_all(&root);
    }
}
