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

const REVIEW_SKILL: &str = r#"# Review

Use when asked to review a change, commit, branch, or pull request.

## Rules

- Review only the change under review. Do not flag pre-existing problems unless the change makes them worse.
- A finding must affect correctness, security, performance, or maintainability; be discrete and actionable; and be something the author would likely fix. If it depends on an unstated assumption or only speculates that another part might break, drop it.
- Findings first, ordered by severity and almost always with `path:line`:
  - P0: data loss, security hole, broken build, or a regression that ships immediately.
  - P1: a likely bug, crash, or behavior regression in a normal path.
  - P2: edge case, missing test, unclear contract, or fragile assumption.
  - P3: naming, style, wording, or cleanup.
- Each finding states what breaks and why it matters, plus the inputs or conditions needed to trigger it. If you did not verify it, say so instead of guessing.
- Deduplicate by changed location and defect: one finding per issue. Do not split variants of the same bug.
- If a repository instruction file (AGENTS.md or equivalent) supplies the rule, cite that file and the smallest supporting line range in the finding.
- Ignore trivial style unless it obscures meaning or violates a documented standard. Do not claim higher severity than the evidence supports.
- No praise, no restating the diff, no generic advice. Suggest a fix only when it is concrete.
- Keep the summary last and short. If there are no findings, say "No findings" and list residual risks or tests you did not run.

## Structured output

When the caller asks for machine-readable or CI output, return only one JSON object, with no markdown fence or extra prose:

{
  "findings": [
    {
      "title": "<= 80 chars, imperative",
      "body": "one Markdown paragraph: why this is a problem, with file/line references",
      "confidence_score": 0.0,
      "priority": 0,
      "code_location": {
        "absolute_file_path": "/abs/path",
        "line_range": {"start": 1, "end": 1}
      }
    }
  ],
  "overall_correctness": "patch is correct" | "patch is incorrect",
  "overall_explanation": "1-3 sentences justifying the verdict",
  "overall_confidence_score": 0.0
}

- `priority` is 0-3 for P0-P3; omit it only when severity is genuinely undetermined.
- Prefix each `title` with `[P0]`-`[P3]` to match `priority`.
- `line_range` must overlap the change and stay as short as possible (never more than 10 lines).
- `overall_correctness` is "patch is correct" only when existing code and tests still hold and no blocking bug remains; ignore nits for this verdict.
- Do not generate a PR fix or apply edits.
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
        (
            "review",
            "Review a change: P0-P3 findings first, only this change, evidence with path:line.",
            REVIEW_SKILL,
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

/// `<available_skills>` catalog. Empty when no skills are installed.
///
/// The catalog is a stable prefix but it is not free: a machine with many
/// user skills could crowd out the conversation. Keep it inside `max_bytes`
/// (the daemon passes 2% of the context window, capped at 8 KiB) by shortening
/// descriptions first, then omitting descriptions, then omitting trailing
/// skills. The full instructions stay behind `use_skill`.
pub fn available_prompt(skills: &[Skill], max_bytes: usize) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let max_bytes = max_bytes.max(256);
    let mut header = String::from(
        "<available_skills>\n\
         Skills are instructions in `SKILL.md` files. When a task matches a skill, call `use_skill` \
         with its name before doing the work and follow the full file, not a summary. Do not delegate \
         skill reading to sub-agents. Reuse a skill's scripts and assets instead of rewriting them. \
         Pass `arguments` when the skill uses `$ARGUMENTS` or `$1`..`$9`. Catalog paths are host \
         paths; read the file directly.\n",
    );
    let footer = "</available_skills>\n";
    let fixed = header.len() + footer.len();

    // Phase 1: cap each description, then halve it until the catalog fits.
    let mut description_cap = 240;
    loop {
        let entries: Vec<String> = skills
            .iter()
            .map(|skill| catalog_line(skill, Some(description_cap)))
            .collect();
        if fixed + entries.iter().map(|entry| entry.len() + 1).sum::<usize>() <= max_bytes {
            header.push_str(&entries.join("\n"));
            header.push('\n');
            header.push_str(footer);
            return header;
        }
        if description_cap <= 40 {
            break;
        }
        description_cap /= 2;
    }

    // Phase 2: names and paths only.
    let entries: Vec<String> = skills
        .iter()
        .map(|skill| catalog_line(skill, None))
        .collect();
    if fixed + entries.iter().map(|entry| entry.len() + 1).sum::<usize>() <= max_bytes {
        header.push_str(&entries.join("\n"));
        header.push('\n');
        header.push_str(footer);
        return header;
    }

    // Phase 3: keep as many leading skills as fit and name the rest.
    let mut kept = Vec::new();
    let mut used = fixed;
    for (index, entry) in entries.iter().enumerate() {
        let omitted = skills.len() - index - 1;
        let marker = omitted_marker(omitted);
        if used + entry.len() + 1 + marker.len() > max_bytes {
            break;
        }
        used += entry.len() + 1;
        kept.push(entry.clone());
    }
    let omitted = skills.len() - kept.len();
    header.push_str(&kept.join("\n"));
    if omitted > 0 {
        if !kept.is_empty() {
            header.push('\n');
        }
        header.push_str(&omitted_marker(omitted));
    }
    header.push('\n');
    header.push_str(footer);
    header
}

fn catalog_line(skill: &Skill, description_cap: Option<usize>) -> String {
    let path = skill.source.display();
    match description_cap {
        Some(cap) if !skill.description.trim().is_empty() => {
            let description = cap_chars(skill.description.trim(), cap);
            format!("- {}: {} (path: {})", skill.name, description, path)
        }
        _ => format!("- {} (path: {})", skill.name, path),
    }
}

fn omitted_marker(omitted: usize) -> String {
    format!("[{omitted} skills omitted]")
}

fn cap_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
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
        "todowrite" | "todo" => "update_plan".to_string(),
        "askuserquestion" | "ask_user" => "request_user_input".to_string(),
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
        let prompt = available_prompt(&skills, 8 * 1024);
        assert!(prompt.contains("- review:"));
        assert!(prompt.contains("use_skill"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn builtin_skills_are_listed_and_project_skills_override_them() {
        let builtins = builtin_skills();
        for name in ["frontend", "engineering", "review"] {
            let skill = builtins
                .iter()
                .find(|skill| skill.name == name)
                .unwrap_or_else(|| panic!("missing builtin {name}"));
            assert!(!skill.body.is_empty());
            assert!(skill.source.starts_with("<builtin>"));
        }
        let review = builtins
            .iter()
            .find(|skill| skill.name == "review")
            .expect("review");
        for expected in [
            "P0",
            "P3",
            "Deduplicate",
            "confidence_score",
            "overall_correctness",
            "absolute_file_path",
        ] {
            assert!(
                review.body.contains(expected),
                "review skill is missing {expected:?}"
            );
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
        let prompt = available_prompt(&builtins, 8 * 1024);
        assert!(prompt.contains("- frontend:"));
        assert!(prompt.contains("- engineering:"));

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

    #[test]
    fn skill_catalog_respects_its_budget_and_keeps_tool_grants_out() {
        let skills: Vec<Skill> = (0..12)
            .map(|index| Skill {
                name: format!("skill-{index:02}"),
                description: "a very long description that should be shortened before the \
                              catalog is allowed to grow past its byte budget "
                    .repeat(4),
                body: String::new(),
                allowed_tools: vec!["exec_command".to_string()],
                source: PathBuf::from(format!("/tmp/skills/skill-{index:02}/SKILL.md")),
            })
            .collect();

        let full = available_prompt(&skills, 64 * 1024);
        assert!(full.contains("- skill-00:"));
        assert!(
            !full.contains("<allowed_tools>"),
            "grants belong to use_skill"
        );

        let compact = available_prompt(&skills, 700);
        assert!(
            compact.len() <= 700,
            "catalog is {} bytes: {compact}",
            compact.len()
        );
        assert!(compact.contains("[") && compact.contains("skills omitted]"));
        assert!(compact.ends_with("</available_skills>\n"));
    }
}
