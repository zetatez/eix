//! User-defined prompt commands loaded from Markdown files.
//!
//! A command is a `.md` file below a `cmds/` directory:
//!
//! ```text
//! ~/.config/eix/cmds/review.md
//! <project>/.eix/cmds/team/review.md
//! <project>/.opencode/commands/review.md   (OpenCode compatibility)
//! ```
//!
//! The legacy `commands/` directory names remain readable.
//!
//! The file body is the prompt template. Optional frontmatter sets
//! `description`, `agent`, and `model`. `$ARGUMENTS` expands to the whole
//! argument string; `$1`, `$2`, ... expand positionally, and the highest
//! placeholder consumes the rest of the argument string.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A command discovered on disk. `name` uses `/` separators for nested files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub template: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub source: PathBuf,
}

/// Load every command visible from `cwd`. Nearer project sources override
/// global and ancestor sources with the same name.
pub fn load_commands(cwd: &Path) -> Vec<CustomCommand> {
    load_commands_with_roots(
        cwd,
        &crate::config::config_home(),
        &crate::config::config_dir(),
    )
}

fn load_commands_with_roots(
    cwd: &Path,
    config_home: &Path,
    config_dir: &Path,
) -> Vec<CustomCommand> {
    let mut sources: Vec<PathBuf> = Vec::new();
    sources.push(config_home.join("opencode/commands"));
    // Legacy global directory, then the current `cmds/` directory so a file in
    // either place is picked up and `cmds/` wins when both define a name.
    sources.push(config_dir.join("commands"));
    sources.push(config_dir.join("cmds"));

    let mut ancestors: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
    ancestors.reverse();
    for ancestor in ancestors {
        sources.push(ancestor.join(".eix/commands"));
        sources.push(ancestor.join(".opencode/commands"));
        sources.push(ancestor.join(".eix/cmds"));
    }

    let mut by_name: BTreeMap<String, CustomCommand> = BTreeMap::new();
    for source in sources {
        collect_from(&source, &source, &mut by_name);
    }
    by_name.into_values().collect()
}

fn collect_from(source: &Path, dir: &Path, out: &mut BTreeMap<String, CustomCommand>) {
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
        let Some(name) = command_name(relative) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (meta, body) = split_frontmatter(&text);
        let template = body.trim().to_string();
        if template.is_empty() {
            continue;
        }
        let description = meta
            .get("description")
            .cloned()
            .filter(|description| !description.is_empty())
            .unwrap_or_else(|| first_line(&template));
        out.insert(
            name.clone(),
            CustomCommand {
                name,
                description,
                template,
                agent: meta.get("agent").cloned().filter(|value| !value.is_empty()),
                model: meta.get("model").cloned().filter(|value| !value.is_empty()),
                source: path,
            },
        );
    }
}

fn command_name(relative: &Path) -> Option<String> {
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

/// Split a leading `---` frontmatter block from the Markdown body. Only flat
/// `key: value` pairs are supported, which covers the documented fields.
pub(crate) fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut meta = BTreeMap::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (meta, text);
    }
    let mut body_start = None;
    let mut offset = text.find('\n').map(|index| index + 1).unwrap_or(text.len());
    for line in lines {
        let line_start = offset;
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
        let _ = line_start;
    }
    match body_start {
        Some(start) => (meta, &text[start..]),
        None => (BTreeMap::new(), text),
    }
}

pub(crate) fn first_line(template: &str) -> String {
    template
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim().to_string())
        .unwrap_or_default()
        .chars()
        .take(72)
        .collect()
}

/// Expand `$ARGUMENTS` and `$1`..`$9` in a command template.
pub fn expand(template: &str, args: &str) -> String {
    let tokens = tokenize(args);
    let highest = (1..=9)
        .rev()
        .find(|index| template.contains(&format!("${index}")))
        .unwrap_or(0);

    let mut out = template.replace("$ARGUMENTS", args);
    let used_arguments = template.contains("$ARGUMENTS");
    if highest > 0 {
        for index in 1..=highest {
            let value = if index == highest {
                tokens.get(index - 1..).unwrap_or(&[]).join(" ")
            } else {
                tokens.get(index - 1).cloned().unwrap_or_default()
            };
            out = out.replace(&format!("${index}"), &value);
        }
    } else if !used_arguments && !args.trim().is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(args);
    }
    out
}

/// Split an argument string on whitespace, honouring single and double quotes.
pub fn tokenize(args: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in args.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
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
        let dir = std::env::temp_dir().join(format!(
            "eix-commands-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join(".eix/cmds/team")).unwrap();
        dir
    }

    #[test]
    fn loads_nested_markdown_with_frontmatter() {
        let cwd = temp_dir("load");
        std::fs::write(
            cwd.join(".eix/cmds/review.md"),
            "---\ndescription: Review code\nagent: plan\nmodel: openai/gpt-5\n---\n\nReview $ARGUMENTS for bugs.\n",
        )
        .unwrap();
        std::fs::write(cwd.join(".eix/cmds/team/check.md"), "Check $1 and $2.\n").unwrap();

        let commands = load_commands(&cwd);
        let review = commands
            .iter()
            .find(|command| command.name == "review")
            .expect("review");
        assert_eq!(review.description, "Review code");
        assert_eq!(review.agent.as_deref(), Some("plan"));
        assert_eq!(review.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(review.template, "Review $ARGUMENTS for bugs.");
        assert!(commands.iter().any(|command| command.name == "team/check"));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn loads_global_cmds_and_prefers_them_over_legacy_commands() {
        let config_home = temp_dir("global-home");
        let config_dir = config_home.join("eix");
        std::fs::create_dir_all(config_dir.join("cmds")).unwrap();
        std::fs::create_dir_all(config_dir.join("commands")).unwrap();
        std::fs::write(config_dir.join("commands/global.md"), "legacy template\n").unwrap();
        std::fs::write(config_dir.join("cmds/global.md"), "cmds template\n").unwrap();
        std::fs::write(config_dir.join("cmds/review.md"), "Review $ARGUMENTS.\n").unwrap();
        let cwd = config_home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let commands = load_commands_with_roots(&cwd, &config_home, &config_dir);
        let global = commands
            .iter()
            .find(|command| command.name == "global")
            .expect("global");
        assert_eq!(global.template, "cmds template");
        assert!(commands.iter().any(|command| command.name == "review"));
        let _ = std::fs::remove_dir_all(&config_home);
    }

    #[test]
    fn expands_arguments_and_positionals() {
        assert_eq!(expand("Review $ARGUMENTS.", "src/a.rs"), "Review src/a.rs.");
        assert_eq!(
            expand("Check $1. Focus on $2.", "src/a.rs \"error handling\""),
            "Check src/a.rs. Focus on error handling."
        );
        assert_eq!(
            expand("Compare $1 with $2.", "api stable branch"),
            "Compare api with stable branch."
        );
        assert_eq!(
            expand("Explain this code.", "src/a.rs"),
            "Explain this code.\n\nsrc/a.rs"
        );
        assert!(expand("$ARGUMENTS", "").is_empty());
    }

    #[test]
    fn tokenize_keeps_quoted_groups() {
        assert_eq!(
            tokenize("a \"b c\" 'd e'"),
            vec!["a".to_string(), "b c".to_string(), "d e".to_string()]
        );
    }
}
