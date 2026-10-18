//! Git worktrees for sessions.
//!
//! A session normally shares its `cwd` with every other session. For mutual
//! isolation `session.create {worktree: true}` and forked sessions can get their
//! own worktree under `<repo>/.worktrees/<name>` on branch `eix/<name>`. The
//! directory is ignored by workspace snapshots (see `snapshot::EXCLUDED`), so
//! rewinding one worktree never touches another.
//!
//! Removal is deliberately explicit: `worktree.remove` (and `eix worktree rm`)
//! asks git to remove the directory and only discards uncommitted files when
//! `force` is set.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Created {
    pub path: PathBuf,
    pub branch: String,
    pub repo: PathBuf,
}

/// The repository root containing `dir`, if it is inside a git work tree.
pub async fn repo_root(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Main repository for a (possibly linked) worktree. `--git-common-dir`
/// points at `<main>/.git`; its parent is the main work tree.
pub async fn main_repo(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let common = if Path::new(&raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        dir.join(raw)
    };
    let repo = common.parent()?.to_path_buf();
    Some(repo.canonicalize().unwrap_or(repo))
}

/// Create a worktree named after `name` in `dir`'s repository.
pub async fn create(dir: &Path, name: &str) -> Result<Created> {
    // Resolve the main repository: a session may itself already live in a
    // linked worktree, and nested `.worktrees` directories confuse git.
    let repo = match main_repo(dir).await.filter(|repo| repo.exists()) {
        Some(repo) => repo,
        None => repo_root(dir)
            .await
            .with_context(|| format!("{} is not inside a git work tree", dir.display()))?,
    };
    let slug = slug(name);
    let base = repo.join(".worktrees");
    std::fs::create_dir_all(&base).with_context(|| format!("cannot create {}", base.display()))?;

    // Never silently reuse a directory or branch that already exists.
    let mut candidate = base.join(&slug);
    let mut branch = format!("eix/{slug}");
    let mut suffix = 2;
    while candidate.exists() || branch_exists(&repo, &branch).await {
        candidate = base.join(format!("{slug}-{suffix}"));
        branch = format!("eix/{slug}-{suffix}");
        suffix += 1;
        if suffix > 1000 {
            bail!("cannot find a free worktree name for {slug:?}");
        }
    }

    let output = Command::new("git")
        .args(["worktree", "add", "-b"])
        .arg(&branch)
        .arg(&candidate)
        .current_dir(&repo)
        .stdin(Stdio::null())
        .output()
        .await
        .context("cannot run git worktree add")?;
    if !output.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    // Canonical paths keep `cwd` stable when the repo lives behind a symlink.
    let path = candidate.canonicalize().unwrap_or(candidate);
    Ok(Created { path, branch, repo })
}

/// Remove one worktree. Without `force` git refuses when the worktree has
/// uncommitted changes; the error is returned so the caller can ask again.
pub async fn remove(path: &Path, force: bool) -> Result<()> {
    let repo = main_repo(path)
        .await
        .unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf());
    let mut command = Command::new("git");
    command.arg("worktree").arg("remove");
    if force {
        command.arg("--force");
    }
    let output = command
        .arg(path)
        .current_dir(&repo)
        .stdin(Stdio::null())
        .output()
        .await
        .context("cannot run git worktree remove")?;
    if !output.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&repo)
        .stdin(Stdio::null())
        .output()
        .await;
    Ok(())
}

/// Current branch of a worktree, for the CLI listing.
pub async fn branch(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

async fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(repo)
        .stdin(Stdio::null())
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(['-', '.']).to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn slugs_are_safe_branch_names() {
        assert_eq!(slug("auth~2"), "auth-2");
        assert_eq!(slug("Feature/Login"), "feature-login");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("中文"), "session");
    }
}
