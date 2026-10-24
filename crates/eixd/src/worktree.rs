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
    pub base_branch: Option<String>,
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
    create_from_base(dir, name, None).await
}

/// Create a worktree from an explicit local base branch. `None` keeps the
/// existing behavior (current HEAD of the main worktree).
pub async fn create_from_base(dir: &Path, name: &str, base: Option<&str>) -> Result<Created> {
    // Resolve the main repository: a session may itself already live in a
    // linked worktree, and nested `.worktrees` directories confuse git.
    let repo = match main_repo(dir).await.filter(|repo| repo.exists()) {
        Some(repo) => repo,
        None => repo_root(dir)
            .await
            .with_context(|| format!("{} is not inside a git work tree", dir.display()))?,
    };
    let slug = slug(name);
    ensure_worktrees_ignored(&repo).await?;
    let worktrees = repo.join(".worktrees");
    std::fs::create_dir_all(&worktrees)
        .with_context(|| format!("cannot create {}", worktrees.display()))?;

    // Never silently reuse a directory or branch that already exists.
    let mut candidate = worktrees.join(&slug);
    let mut branch = format!("eix/{slug}");
    let mut suffix = 2;
    while candidate.exists() || branch_exists(&repo, &branch).await {
        candidate = worktrees.join(format!("{slug}-{suffix}"));
        branch = format!("eix/{slug}-{suffix}");
        suffix += 1;
        if suffix > 1000 {
            bail!("cannot find a free worktree name for {slug:?}");
        }
    }

    let mut command = Command::new("git");
    command
        .args(["worktree", "add", "-b"])
        .arg(&branch)
        .arg(&candidate);
    if let Some(base) = base {
        command.arg(base);
    }
    let output = command
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
    Ok(Created {
        path,
        branch,
        repo,
        base_branch: base.map(str::to_string),
    })
}

/// Resolve the base branch. `auto` prefers the remote HEAD name, then the
/// conventional `main` / `master`, then the repository's current branch.
pub async fn resolve_base(repo: &Path, configured: &str) -> Result<String> {
    let configured = configured.trim();
    if !configured.is_empty() && configured != "auto" {
        return Ok(configured.to_string());
    }
    if let Some(name) = git_stdout(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await
    {
        if let Some(name) = name.trim().strip_prefix("origin/") {
            if branch_exists(repo, name).await {
                return Ok(name.to_string());
            }
        }
    }
    for name in ["main", "master"] {
        if branch_exists(repo, name).await {
            return Ok(name.to_string());
        }
    }
    if let Some(name) = git_stdout(repo, &["symbolic-ref", "--short", "HEAD"]).await {
        let name = name.trim();
        if !name.is_empty() && name != "HEAD" {
            return Ok(name.to_string());
        }
    }
    bail!("cannot determine base branch; set [worktree] base explicitly")
}

/// Verify that `repo` is a clean worktree checked out on `base`, then return
/// its HEAD. The queue uses this even when remote pulls are disabled.
pub async fn require_base(repo: &Path, base: &str) -> Result<String> {
    let current = git_stdout(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .unwrap_or_default();
    if current.trim() != base {
        bail!(
            "base worktree {} is on {:?}, expected {base:?}",
            repo.display(),
            current.trim()
        );
    }
    if !is_clean(repo).await? {
        bail!("base worktree {} has uncommitted changes", repo.display());
    }
    head(repo).await
}

/// Pull the base branch with `--ff-only` before an integration attempt.
/// The base worktree must be clean and checked out on that branch.
pub async fn sync_base(repo: &Path, base: &str) -> Result<String> {
    require_base(repo, base).await?;
    if Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
    {
        run_git(repo, &["fetch", "--prune", "origin"]).await?;
        // Prefer the configured upstream. Repositories that only have a remote
        // but no tracking branch still integrate against origin/<base>.
        let upstream = git_stdout(
            repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .await
        .unwrap_or_default();
        if !upstream.trim().is_empty() {
            run_git(repo, &["pull", "--ff-only"]).await?;
        } else if ref_exists(repo, &format!("refs/remotes/origin/{base}")).await {
            run_git(repo, &["merge", "--ff-only", &format!("origin/{base}")]).await?;
        }
    }
    head(repo).await
}

/// Rebase a worktree branch onto `base`. Returns git's combined output even
/// on conflict so the repair prompt can include the useful part.
pub async fn rebase(worktree: &Path, base: &str) -> Result<CommandOutput> {
    run_git_capture(worktree, &["rebase", base]).await
}

/// Abort an in-progress rebase, keeping the pre-rebase branch contents.
pub async fn abort_rebase(worktree: &Path) -> Result<()> {
    run_git(worktree, &["rebase", "--abort"]).await
}

/// Continue an in-progress rebase after the agent staged resolutions.
pub async fn continue_rebase(worktree: &Path) -> Result<CommandOutput> {
    run_git_capture(worktree, &["rebase", "--continue"]).await
}

/// Fast-forward the base branch to `branch`.
pub async fn merge_ff(base_worktree: &Path, branch: &str) -> Result<()> {
    run_git(base_worktree, &["merge", "--ff-only", branch]).await
}

/// Push the base branch after a local fast-forward. Never forces.
pub async fn push_base(base_worktree: &Path, base: &str) -> Result<()> {
    run_git(base_worktree, &["push", "origin", base]).await
}

pub async fn is_clean(path: &Path) -> Result<bool> {
    let status = git_stdout(path, &["status", "--porcelain"])
        .await
        .with_context(|| format!("cannot read git status in {}", path.display()))?;
    Ok(status.trim().is_empty())
}

pub async fn head(path: &Path) -> Result<String> {
    git_stdout(path, &["rev-parse", "HEAD"])
        .await
        .map(|value| value.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("cannot resolve HEAD in {}", path.display()))
}

pub async fn in_rebase(path: &Path) -> bool {
    for kind in ["rebase-merge", "rebase-apply"] {
        if let Some(raw) = git_stdout(path, &["rev-parse", "--git-path", kind]).await {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let dir = Path::new(raw);
            let path = if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                path.join(dir)
            };
            if path.exists() {
                return true;
            }
        }
    }
    false
}

pub async fn conflict_files(path: &Path) -> Vec<String> {
    let Some(out) = git_stdout(path, &["diff", "--name-only", "--diff-filter=U"]).await else {
        return Vec::new();
    };
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `git add -A` and commit. Hooks run normally; a failing hook aborts the
/// queue rather than being bypassed with `--no-verify`.
pub async fn commit_all(worktree: &Path, message: &str) -> Result<()> {
    run_git(worktree, &["add", "-A"]).await?;
    run_git(worktree, &["commit", "-m", message]).await
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn combined(&self) -> String {
        let mut out = String::new();
        if !self.stdout.trim().is_empty() {
            out.push_str(self.stdout.trim());
        }
        if !self.stderr.trim().is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(self.stderr.trim());
        }
        out
    }
}

async fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = run_git_capture(dir, args).await?;
    if !output.success {
        bail!("git {} failed: {}", args.join(" "), output.combined());
    }
    Ok(())
}

async fn run_git_capture(dir: &Path, args: &[&str]) -> Result<CommandOutput> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("cannot run git {}", args.join(" ")))?;
    Ok(CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

async fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
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

/// Keep `.worktrees/` out of the main worktree's status. The integration queue
/// refuses to merge while base is dirty, and a repo that has not committed a
/// `.gitignore` entry would otherwise show every eix worktree as untracked.
async fn ensure_worktrees_ignored(repo: &Path) -> Result<()> {
    let Some(raw) = git_stdout(repo, &["rev-parse", "--git-path", "info/exclude"]).await else {
        return Ok(());
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .any(|line| matches!(line.trim(), ".worktrees/" | "/.worktrees/"))
    {
        return Ok(());
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.write_all(b"# eix session worktrees\n/.worktrees/\n")?;
    Ok(())
}

async fn ref_exists(repo: &Path, reference: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .current_dir(repo)
        .stdin(Stdio::null())
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
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
