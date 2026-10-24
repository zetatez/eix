//! Helpers for the worktree integration queue.
//!
//! The queue itself lives on `Daemon` because it needs sessions, turns and the
//! provider. This module keeps the pure pieces (test detection, output
//! truncation and English worktree slug cleanup) testable.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct TestRun {
    pub success: bool,
    pub timed_out: bool,
    pub output: String,
}

/// Pick a test command. `auto` recognizes the common project markers; an
/// explicit command always wins. An empty result means "no test configured"
/// and the queue must not merge.
pub fn test_command(repo: &Path, configured: &str) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() && configured != "auto" {
        return Some(configured.to_string());
    }
    if repo.join("Cargo.toml").exists() {
        return Some("cargo test --workspace".into());
    }
    if repo.join("package.json").exists() {
        return Some("npm test".into());
    }
    if repo.join("go.mod").exists() {
        return Some("go test ./...".into());
    }
    if repo.join("pyproject.toml").exists() || repo.join("pytest.ini").exists() {
        return Some("pytest".into());
    }
    if repo.join("Makefile").exists() {
        return Some("make test".into());
    }
    None
}

/// Run the test command in the worktree with a hard wall-clock limit.
pub async fn run_test(worktree: &Path, command: &str, timeout_secs: u64) -> Result<TestRun> {
    let child = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .current_dir(worktree)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot run test command in {}", worktree.display()))?;
    let timeout = Duration::from_secs(timeout_secs.max(1));
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(TestRun {
            success: output.status.success(),
            timed_out: false,
            output: combined(&output.stdout, &output.stderr),
        }),
        Ok(Err(err)) => Err(err).context("test command failed to run"),
        Err(_) => Ok(TestRun {
            success: false,
            timed_out: true,
            output: format!("test command timed out after {timeout_secs}s"),
        }),
    }
}

fn combined(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::new();
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);
    if !out.trim().is_empty() {
        text.push_str(out.trim());
    }
    if !err.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err.trim());
    }
    text
}

/// Keep the useful tail of a test/conflict log. This is what goes into
/// notifications, the persisted error, and the repair prompt.
pub fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    let mut out: String = text.chars().skip(skip).collect();
    out.insert_str(0, "… (truncated)\n");
    out
}

/// Normalize a model-generated worktree name to a safe English kebab slug.
/// Non-ASCII input is discarded; the caller supplies a deterministic fallback.
pub fn clean_worktree_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if (ch.is_ascii_whitespace() || matches!(ch, '-' | '_' | '/' | '.'))
            && !out.ends_with('-')
        {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    let mut out = out.chars().take(48).collect::<String>();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        String::new()
    } else {
        out
    }
}

/// Deterministic English fallback when the naming provider is unavailable.
/// ASCII words are used when present; otherwise a timestamp keeps the name
/// readable and unique without leaking non-ASCII text into a branch name.
pub fn fallback_worktree_name(text: &str) -> String {
    let mut words = Vec::new();
    for word in text.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        let word = word.trim().to_ascii_lowercase();
        if word.len() >= 2 && !word.chars().all(|ch| ch.is_ascii_digit()) {
            words.push(word);
        }
        if words.len() >= 4 {
            break;
        }
    }
    if !words.is_empty() {
        return clean_worktree_name(&words.join("-"));
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("task-{ts}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_names_are_ascii_kebab() {
        assert_eq!(clean_worktree_name("Fix Auth Bug!"), "fix-auth-bug");
        assert_eq!(clean_worktree_name("`refactor/cache`"), "refactor-cache");
        assert_eq!(clean_worktree_name("修复登录"), "");
    }

    #[test]
    fn fallback_extracts_ascii_words() {
        assert_eq!(
            fallback_worktree_name("fix the login bug"),
            "fix-the-login-bug"
        );
        assert!(fallback_worktree_name("修复登录").starts_with("task-"));
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("abcdef", 3), "… (truncated)\ndef");
    }
}
