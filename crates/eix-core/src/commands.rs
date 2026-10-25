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

use crate::compat::CommandFormat;
use crate::config::CompatConfig;

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
pub fn load_commands(cwd: &Path, compat: &CompatConfig) -> Vec<CustomCommand> {
    load_commands_with_roots(
        cwd,
        &crate::config::config_home(),
        &crate::config::config_dir(),
        compat,
    )
}

fn load_commands_with_roots(
    cwd: &Path,
    config_home: &Path,
    config_dir: &Path,
    compat: &CompatConfig,
) -> Vec<CustomCommand> {
    let mut commands: BTreeMap<String, CustomCommand> = BTreeMap::new();
    for (source, format) in crate::compat::command_sources(cwd, compat, config_home, config_dir) {
        collect_from(&source, &source, &mut commands, format);
    }
    commands.into_values().collect()
}

fn collect_from(
    source: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, CustomCommand>,
    format: CommandFormat,
) {
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
            collect_from(source, &path, out, format);
            continue;
        }
        let Some(relative) = path.strip_prefix(source).ok() else {
            continue;
        };
        match format {
            CommandFormat::Markdown => {
                if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                    continue;
                }
                let Some(name) = command_name(relative, "md") else {
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
            CommandFormat::GeminiToml => {
                if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                    continue;
                }
                let Some(name) = command_name(relative, "toml") else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(value) = toml::from_str::<toml::Value>(&text) else {
                    continue;
                };
                let Some(table) = value.as_table() else {
                    continue;
                };
                let Some(prompt) = table.get("prompt").and_then(toml::Value::as_str) else {
                    continue;
                };
                let template = prompt.replace("{{args}}", "$ARGUMENTS").trim().to_string();
                if template.is_empty() {
                    continue;
                }
                let description = table
                    .get("description")
                    .and_then(toml::Value::as_str)
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| first_line(&template));
                out.insert(
                    name.clone(),
                    CustomCommand {
                        name,
                        description,
                        template,
                        agent: None,
                        model: None,
                        source: path,
                    },
                );
            }
        }
    }
}

fn command_name(relative: &Path, extension: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let part = component.as_os_str().to_string_lossy();
        if part.is_empty() {
            continue;
        }
        parts.push(part.to_string());
    }
    let last = parts.last_mut()?;
    let suffix = format!(".{extension}");
    let stem = last.strip_suffix(&suffix)?.to_string();
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
            "---\ndescription: Review code\nagent: reviewer\nmodel: openai/gpt-5\n---\n\nReview $ARGUMENTS for bugs.\n",
        )
        .unwrap();
        std::fs::write(cwd.join(".eix/cmds/team/check.md"), "Check $1 and $2.\n").unwrap();

        let commands = load_commands(&cwd, &CompatConfig::default());
        let review = commands
            .iter()
            .find(|command| command.name == "review")
            .expect("review");
        assert_eq!(review.description, "Review code");
        assert_eq!(review.agent.as_deref(), Some("reviewer"));
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

        let commands =
            load_commands_with_roots(&cwd, &config_home, &config_dir, &CompatConfig::default());
        let global = commands
            .iter()
            .find(|command| command.name == "global")
            .expect("global");
        assert_eq!(global.template, "cmds template");
        assert!(commands.iter().any(|command| command.name == "review"));
        let _ = std::fs::remove_dir_all(&config_home);
    }

    #[test]
    fn loads_compatible_agent_commands() {
        let cwd = temp_dir("compat");
        std::fs::create_dir_all(cwd.join(".claude/commands")).unwrap();
        std::fs::write(
            cwd.join(".claude/commands/claude.md"),
            "Claude $ARGUMENTS\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join(".opencode/commands")).unwrap();
        std::fs::write(cwd.join(".opencode/commands/opencode.md"), "OpenCode $1\n").unwrap();
        std::fs::create_dir_all(cwd.join(".codex/prompts")).unwrap();
        std::fs::write(cwd.join(".codex/prompts/codex.md"), "Codex $ARGUMENTS\n").unwrap();
        std::fs::create_dir_all(cwd.join(".gemini/commands")).unwrap();
        std::fs::write(
            cwd.join(".gemini/commands/gemini.toml"),
            "description = \"Gemini command\"\nprompt = \"Gemini {{args}}\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join(".pi/prompts")).unwrap();
        std::fs::write(cwd.join(".pi/prompts/pi.md"), "Pi $1\n").unwrap();

        let commands = load_commands(&cwd, &CompatConfig::all());
        for name in ["claude", "opencode", "codex", "gemini", "pi"] {
            assert!(
                commands.iter().any(|command| command.name == name),
                "{name}"
            );
        }
        let gemini = commands
            .iter()
            .find(|command| command.name == "gemini")
            .unwrap();
        assert_eq!(gemini.description, "Gemini command");
        assert_eq!(gemini.template, "Gemini $ARGUMENTS");
        let _ = std::fs::remove_dir_all(&cwd);
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
