//! Discovery of skills, commands, and agents.
//!
//! Native `.eix` sources are always loaded. Sources written for Claude Code,
//! OpenCode, Codex CLI, Gemini CLI, and Pi are gated by [`CompatConfig`], so a
//! global installation of another agent cannot leak resources into eix unless
//! the user opts in. Precedence is lowest to highest; later entries win.

use std::path::{Path, PathBuf};

use crate::config::{config_dir, config_home, CompatConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandFormat {
    Markdown,
    GeminiToml,
}

pub(crate) fn skill_sources(cwd: &Path, compat: &CompatConfig) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        if compat.claude_code {
            push(&mut out, home.join(".claude/skills"));
        }
        if compat.opencode {
            push(&mut out, config_home().join("opencode/skills"));
        }
        if compat.codex_cli {
            push(&mut out, codex_home(&home).join("skills"));
        }
        if compat.gemini_cli || compat.pi {
            push(&mut out, home.join(".agents/skills"));
        }
        if compat.gemini_cli {
            push(&mut out, home.join(".gemini/skills"));
        }
        if compat.pi {
            push(&mut out, pi_agent_dir(&home).join("skills"));
        }
    }
    push(&mut out, crate::config::skills_dir());
    for ancestor in ancestors(cwd) {
        if compat.claude_code {
            push(&mut out, ancestor.join(".claude/skills"));
        }
        if compat.opencode {
            push(&mut out, ancestor.join(".opencode/skills"));
        }
        if compat.codex_cli {
            push(&mut out, ancestor.join(".codex/skills"));
        }
        if compat.gemini_cli || compat.pi {
            push(&mut out, ancestor.join(".agents/skills"));
        }
        if compat.gemini_cli {
            push(&mut out, ancestor.join(".gemini/skills"));
        }
        if compat.pi {
            push(&mut out, ancestor.join(".pi/skills"));
        }
        push(&mut out, ancestor.join(".eix/skills"));
    }
    out
}

pub(crate) fn command_sources(
    cwd: &Path,
    compat: &CompatConfig,
    config_home: &Path,
    config_dir: &Path,
) -> Vec<(PathBuf, CommandFormat)> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        if compat.claude_code {
            push_command(
                &mut out,
                home.join(".claude/commands"),
                CommandFormat::Markdown,
            );
        }
        if compat.opencode {
            push_command(
                &mut out,
                config_home.join("opencode/commands"),
                CommandFormat::Markdown,
            );
        }
        if compat.codex_cli {
            push_command(
                &mut out,
                codex_home(&home).join("prompts"),
                CommandFormat::Markdown,
            );
        }
        if compat.gemini_cli {
            push_command(
                &mut out,
                home.join(".gemini/commands"),
                CommandFormat::GeminiToml,
            );
        }
        if compat.pi {
            push_command(
                &mut out,
                pi_agent_dir(&home).join("prompts"),
                CommandFormat::Markdown,
            );
        }
    }
    push_command(
        &mut out,
        config_dir.join("commands"),
        CommandFormat::Markdown,
    );
    push_command(&mut out, config_dir.join("cmds"), CommandFormat::Markdown);
    for ancestor in ancestors(cwd) {
        if compat.claude_code {
            push_command(
                &mut out,
                ancestor.join(".claude/commands"),
                CommandFormat::Markdown,
            );
        }
        if compat.opencode {
            push_command(
                &mut out,
                ancestor.join(".opencode/commands"),
                CommandFormat::Markdown,
            );
        }
        if compat.codex_cli {
            push_command(
                &mut out,
                ancestor.join(".codex/prompts"),
                CommandFormat::Markdown,
            );
        }
        if compat.gemini_cli {
            push_command(
                &mut out,
                ancestor.join(".gemini/commands"),
                CommandFormat::GeminiToml,
            );
        }
        if compat.pi {
            push_command(
                &mut out,
                ancestor.join(".pi/prompts"),
                CommandFormat::Markdown,
            );
        }
        push_command(
            &mut out,
            ancestor.join(".eix/commands"),
            CommandFormat::Markdown,
        );
        push_command(
            &mut out,
            ancestor.join(".eix/cmds"),
            CommandFormat::Markdown,
        );
    }
    out
}

pub(crate) fn agent_sources(cwd: &Path, compat: &CompatConfig) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        if compat.claude_code {
            push(&mut out, home.join(".claude/agents"));
        }
        if compat.opencode {
            push(&mut out, config_home().join("opencode/agents"));
        }
        if compat.gemini_cli {
            push(&mut out, home.join(".gemini/agents"));
        }
    }
    push(&mut out, config_dir().join("agents"));
    for ancestor in ancestors(cwd) {
        if compat.claude_code {
            push(&mut out, ancestor.join(".claude/agents"));
        }
        if compat.opencode {
            push(&mut out, ancestor.join(".opencode/agents"));
        }
        if compat.gemini_cli {
            push(&mut out, ancestor.join(".gemini/agents"));
        }
        push(&mut out, ancestor.join(".eix/agents"));
    }
    out
}

/// Global instruction files (`CLAUDE.md` / `GEMINI.md` / `AGENTS.md`) for the
/// enabled harnesses. The caller deduplicates them against its own files.
pub fn global_instruction_sources(compat: &CompatConfig) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        if compat.claude_code {
            out.push((
                home.join(".claude/CLAUDE.md"),
                "global .claude/CLAUDE.md".to_string(),
            ));
        }
        if compat.opencode {
            out.push((
                config_home().join("opencode/AGENTS.md"),
                "global opencode/AGENTS.md".to_string(),
            ));
        }
        if compat.codex_cli {
            out.push((
                codex_home(&home).join("AGENTS.md"),
                "global codex/AGENTS.md".to_string(),
            ));
        }
        if compat.gemini_cli {
            out.push((
                home.join(".gemini/GEMINI.md"),
                "global .gemini/GEMINI.md".to_string(),
            ));
        }
        if compat.pi {
            let dir = pi_agent_dir(&home);
            out.push((dir.join("AGENTS.md"), "global pi/AGENTS.md".to_string()));
            out.push((dir.join("CLAUDE.md"), "global pi/CLAUDE.md".to_string()));
        }
    }
    out
}

/// Project-level context filenames that are only read when the matching
/// harness is enabled. `AGENTS.md` is already part of eix's native instruction
/// list, so it is intentionally not repeated here.
pub fn project_instruction_names(compat: &CompatConfig) -> Vec<&'static str> {
    let mut names = Vec::new();
    if compat.claude_code || compat.pi {
        names.push("CLAUDE.md");
    }
    if compat.gemini_cli {
        names.push("GEMINI.md");
    }
    names
}

fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

fn codex_home(home: &Path) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
}

fn pi_agent_dir(home: &Path) -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/agent"))
}

fn ancestors(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<_> = cwd.ancestors().map(Path::to_path_buf).collect();
    dirs.reverse();
    dirs
}

fn push(out: &mut Vec<PathBuf>, path: PathBuf) {
    if !out.contains(&path) {
        out.push(path);
    }
}

fn push_command(out: &mut Vec<(PathBuf, CommandFormat)>, path: PathBuf, format: CommandFormat) {
    if !out.iter().any(|(existing, _)| existing == &path) {
        out.push((path, format));
    }
}
