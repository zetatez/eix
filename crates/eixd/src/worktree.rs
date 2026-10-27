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
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Hard ceiling for one git invocation. `fetch`, `pull`, `push` and `rebase`
/// talk to a remote or can wait on a stale lock; without this the integration
/// loop could stall forever. Local commands finish in milliseconds.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

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

/// Find the worktree that currently has `branch` checked out. Integration
/// must run in that worktree, not necessarily the main one, because git
/// refuses to update a branch from a different checkout.
pub async fn worktree_for_branch(dir: &Path, branch: &str) -> Option<PathBuf> {
    let output = git_stdout(dir, &["worktree", "list", "--porcelain"]).await?;
    let wanted = format!("refs/heads/{branch}");
    let mut current: Option<PathBuf> = None;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
        } else if let Some(name) = line.strip_prefix("branch ") {
            if name.trim() == wanted {
                return current.map(|path| path.canonicalize().unwrap_or(path));
            }
        } else if line.trim().is_empty() {
            current = None;
        }
    }
    None
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

/// Resolve a fallback base branch. Session creation passes the branch checked
/// out in the starting directory first; this `auto` path is for detached HEADs
/// and legacy sessions. It prefers the remote HEAD name, then `main` /
/// `master`, then the repository's current branch.
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
pub async fn sync_base(repo: &Path, base: &str) -> Result<SyncRun> {
    require_base(repo, base).await?;
    let mut commands = Vec::new();
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
        run_git_logged(repo, &["fetch", "--prune", "origin"], &mut commands).await?;
        // Prefer the configured upstream. Repositories that only have a remote
        // but no tracking branch still integrate against origin/<base>.
        let upstream = git_stdout(
            repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .await
        .unwrap_or_default();
        if !upstream.trim().is_empty() {
            run_git_logged(repo, &["pull", "--ff-only"], &mut commands).await?;
        } else if ref_exists(repo, &format!("refs/remotes/origin/{base}")).await {
            let target = format!("origin/{base}");
            run_git_logged(repo, &["merge", "--ff-only", &target], &mut commands).await?;
        }
    }
    Ok(SyncRun {
        head: head(repo).await?,
        commands,
    })
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
pub async fn merge_ff(base_worktree: &Path, branch: &str) -> Result<CommandLog> {
    run_git_log(base_worktree, &["merge", "--ff-only", branch]).await
}

/// Push the base branch after a local fast-forward. Never forces.
pub async fn push_base(base_worktree: &Path, base: &str) -> Result<CommandLog> {
    run_git_log(base_worktree, &["push", "origin", base]).await
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

/// Whether the in-progress rebase has tracked work that would be lost by
/// aborting: unresolved conflicts, staged resolutions, or unstaged edits.
/// Untracked files survive `git rebase --abort` and are intentionally ignored.
pub async fn rebase_has_work(worktree: &Path) -> bool {
    match git_stdout(worktree, &["status", "--porcelain", "--untracked-files=no"]).await {
        Some(status) => !status.trim().is_empty(),
        None => true,
    }
}

/// Clear a stale rebase so integration can continue. Always preserves a
/// recovery point: metadata, a patch and copies of changed files under the
/// worktree's git dir, plus a `refs/eix/recovery/<stamp>` ref.
pub async fn recover_rebase(worktree: &Path) -> Result<Option<RebaseRecovery>> {
    if !in_rebase(worktree).await {
        return Ok(None);
    }

    let head_name = read_rebase_meta(worktree, "head-name").await;
    let branch = match head_name
        .as_deref()
        .and_then(|name| name.strip_prefix("refs/heads/"))
    {
        Some(name) => Some(name.to_string()),
        None => branch(worktree).await,
    };
    let head = head(worktree).await.unwrap_or_default();
    let orig_head = match read_rebase_meta(worktree, "orig-head").await {
        Some(value) => Some(value),
        None => git_stdout(worktree, &["rev-parse", "--verify", "--quiet", "ORIG_HEAD"])
            .await
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    };
    let onto = read_rebase_meta(worktree, "onto").await;
    let conflicts = conflict_files(worktree).await;
    let staged = name_only(worktree, &["diff", "--cached", "--name-only"]).await;
    let unstaged = name_only(worktree, &["diff", "--name-only"]).await;
    let untracked = name_only(worktree, &["ls-files", "--others", "--exclude-standard"]).await;
    let had_work = !conflicts.is_empty() || !staged.is_empty() || !unstaged.is_empty();

    let stamp = recovery_stamp();
    // The common git dir survives worktree removal, so backups from linked
    // worktrees all live under the main repository's `.git/eix/recovery`.
    let backup_dir = git_common_dir(worktree)
        .await
        .map(|dir| dir.join("eix").join("recovery"))
        .unwrap_or_else(|| worktree.join(".git").join("eix").join("recovery"));
    std::fs::create_dir_all(&backup_dir)
        .with_context(|| format!("cannot create {}", backup_dir.display()))?;

    // Patches are convenient; file copies below are the fallback for binary
    // files and conflict markers that a diff cannot represent cleanly.
    let mut patch_text = String::new();
    for (label, args) in [
        (
            "staged",
            &["diff", "--cached", "--binary", "--no-color"][..],
        ),
        ("worktree", &["diff", "--binary", "--no-color"][..]),
    ] {
        if let Ok(output) = run_git_capture(worktree, args).await {
            if !output.stdout.trim().is_empty() {
                patch_text.push_str(&format!("# {label}\n"));
                patch_text.push_str(&output.stdout);
                if !patch_text.ends_with('\n') {
                    patch_text.push('\n');
                }
            }
        }
    }
    let backup_patch = if patch_text.trim().is_empty() {
        None
    } else {
        let path = backup_dir.join(format!("{stamp}.patch"));
        std::fs::write(&path, patch_text)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Some(path)
    };

    let files_dir = backup_dir.join(format!("{stamp}-files"));
    let mut copied = 0usize;
    // Untracked files survive an abort, so they are listed in the metadata
    // but not copied. Copying an untracked build directory would be costly.
    for name in conflicts.iter().chain(staged.iter()).chain(unstaged.iter()) {
        let Some(relative) = safe_relative_path(name) else {
            continue;
        };
        let source = worktree.join(&relative);
        if !source.is_file() {
            continue;
        }
        let target = files_dir.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::copy(&source, &target)
            .with_context(|| format!("cannot back up {}", source.display()))?;
        copied += 1;
    }

    let backup_ref = if head.is_empty() {
        None
    } else {
        let refname = format!("refs/eix/recovery/{stamp}");
        match run_git_capture(worktree, &["update-ref", &refname, &head]).await {
            Ok(output) if output.success => Some(refname),
            _ => None,
        }
    };
    let backup_files = (copied > 0).then_some(files_dir);

    let metadata_path = backup_dir.join(format!("{stamp}.txt"));
    let metadata = recovery_metadata(
        worktree,
        branch.as_deref(),
        &head,
        orig_head.as_deref(),
        onto.as_deref(),
        &conflicts,
        &staged,
        &unstaged,
        &untracked,
        backup_patch.as_deref(),
        backup_files.as_deref(),
        backup_ref.as_deref(),
        copied,
    );
    std::fs::write(&metadata_path, metadata)
        .with_context(|| format!("cannot write {}", metadata_path.display()))?;

    let abort = run_git_capture(worktree, &["rebase", "--abort"]).await?;
    if !abort.success {
        // `--abort` normally restores the branch. If the metadata is too
        // damaged for it, `--quit` at least removes the rebase state; restore
        // the branch explicitly afterwards.
        let quit = run_git_capture(worktree, &["rebase", "--quit"]).await?;
        if !quit.success {
            bail!(
                "cannot abort rebase in {}: {} (backup: {})",
                worktree.display(),
                abort.combined(),
                backup_dir.display()
            );
        }
        if let Some(branch) = branch.as_deref() {
            let _ = run_git(worktree, &["checkout", "-f", branch]).await;
        } else if let Some(orig) = orig_head.as_deref() {
            let _ = run_git(worktree, &["checkout", "-f", "--detach", orig]).await;
        }
    }
    if in_rebase(worktree).await {
        bail!(
            "rebase metadata is still present after recovery in {} (backup: {})",
            worktree.display(),
            backup_dir.display()
        );
    }

    Ok(Some(RebaseRecovery {
        worktree: worktree.to_path_buf(),
        branch,
        head,
        orig_head,
        onto,
        conflicts,
        staged,
        unstaged,
        untracked,
        backup_dir,
        backup_patch,
        backup_files,
        backup_ref,
        had_work,
    }))
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

/// A command and its combined output, kept for the integration transcript.
#[derive(Debug, Clone)]
pub struct CommandLog {
    pub command: String,
    pub output: String,
}

/// Result of syncing the base branch, including the commands that ran.
#[derive(Debug, Clone)]
pub struct SyncRun {
    pub head: String,
    pub commands: Vec<CommandLog>,
}

/// A stale rebase that was backed up and removed so integration can continue.
///
/// `backup_dir` always contains a metadata file; when there were tracked
/// changes it also contains a binary patch and copies of the changed files.
/// `backup_ref` points at the pre-abort HEAD, which may be an intermediate
/// rebase commit.
#[derive(Debug, Clone)]
pub struct RebaseRecovery {
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub orig_head: Option<String>,
    pub onto: Option<String>,
    pub conflicts: Vec<String>,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
    pub backup_dir: PathBuf,
    pub backup_patch: Option<PathBuf>,
    pub backup_files: Option<PathBuf>,
    pub backup_ref: Option<String>,
    pub had_work: bool,
}

async fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    run_git_log(dir, args).await?;
    Ok(())
}

async fn run_git_logged(dir: &Path, args: &[&str], logs: &mut Vec<CommandLog>) -> Result<()> {
    logs.push(run_git_log(dir, args).await?);
    Ok(())
}

async fn run_git_log(dir: &Path, args: &[&str]) -> Result<CommandLog> {
    let output = run_git_capture(dir, args).await?;
    let command = format!("git {}", args.join(" "));
    if !output.success {
        bail!("{command} failed: {}", output.combined());
    }
    Ok(CommandLog {
        command,
        output: output.combined(),
    })
}

async fn run_git_capture(dir: &Path, args: &[&str]) -> Result<CommandOutput> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        // A dropped future kills the child instead of leaving it running.
        .kill_on_drop(true);
    let output = match tokio::time::timeout(GIT_TIMEOUT, command.output()).await {
        Ok(output) => output.with_context(|| format!("cannot run git {}", args.join(" ")))?,
        Err(_) => bail!(
            "git {} timed out after {}s",
            args.join(" "),
            GIT_TIMEOUT.as_secs()
        ),
    };
    Ok(CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

async fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

async fn git_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let raw = git_stdout(dir, &["rev-parse", "--git-path", name]).await?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    })
}

async fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    let raw = git_stdout(dir, &["rev-parse", "--git-common-dir"]).await?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    })
}

async fn read_rebase_meta(worktree: &Path, name: &str) -> Option<String> {
    for kind in ["rebase-merge", "rebase-apply"] {
        let Some(path) = git_path(worktree, &format!("{kind}/{name}")).await else {
            continue;
        };
        if let Ok(text) = std::fs::read_to_string(path) {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

async fn name_only(dir: &Path, args: &[&str]) -> Vec<String> {
    git_stdout(dir, args)
        .await
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn safe_relative_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute()
        || !path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return None;
    }
    Some(path.to_path_buf())
}

fn recovery_stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{}", std::process::id())
}

#[allow(clippy::too_many_arguments)]
fn recovery_metadata(
    worktree: &Path,
    branch: Option<&str>,
    head: &str,
    orig_head: Option<&str>,
    onto: Option<&str>,
    conflicts: &[String],
    staged: &[String],
    unstaged: &[String],
    untracked: &[String],
    backup_patch: Option<&Path>,
    backup_files: Option<&Path>,
    backup_ref: Option<&str>,
    copied_files: usize,
) -> String {
    let list = |items: &[String]| {
        if items.is_empty() {
            "(none)".to_string()
        } else {
            items.join(", ")
        }
    };
    format!(
        "worktree: {}\nbranch: {}\nHEAD: {}\norig-head: {}\nonto: {}\nconflicts: {}\nstaged: {}\nunstaged: {}\nuntracked: {}\nbackup-patch: {}\nbackup-files: {}\nbackup-ref: {}\nbacked-up-files: {copied_files}\n",
        worktree.display(),
        branch.unwrap_or("(detached)"),
        head,
        orig_head.unwrap_or("(unknown)"),
        onto.unwrap_or("(unknown)"),
        list(conflicts),
        list(staged),
        list(unstaged),
        list(untracked),
        backup_patch
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        backup_files
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        backup_ref.unwrap_or("(none)"),
    )
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
    use super::{in_rebase, is_clean, rebase_has_work, recover_rebase, slug, worktree_for_branch};
    use std::path::Path;

    #[test]
    fn slugs_are_safe_branch_names() {
        assert_eq!(slug("auth~2"), "auth-2");
        assert_eq!(slug("Feature/Login"), "feature-login");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("中文"), "session");
    }

    #[tokio::test]
    async fn finds_the_worktree_that_has_a_branch_checked_out() -> anyhow::Result<()> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "eix-worktree-branch-{}-{stamp}",
            std::process::id()
        ));
        let linked = root.with_extension("feature");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&linked);
        std::fs::create_dir_all(&root)?;

        assert!(git(&root, &["init", "-q", "-b", "main"]).status.success());
        std::fs::write(root.join("file.txt"), "base\n")?;
        assert!(git(&root, &["add", "file.txt"]).status.success());
        assert!(git(&root, &["commit", "-q", "-m", "base"]).status.success());
        let add = git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().expect("linked path"),
            ],
        );
        assert!(
            add.status.success(),
            "git worktree add: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let main = worktree_for_branch(&root, "main")
            .await
            .expect("main worktree");
        assert_eq!(main, root.canonicalize()?);
        let feature = worktree_for_branch(&root, "feature")
            .await
            .expect("feature worktree");
        assert_eq!(feature, linked.canonicalize()?);
        assert!(worktree_for_branch(&root, "missing").await.is_none());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&linked);
        Ok(())
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(["-c", "user.email=eix@example.com", "-c", "user.name=eix"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git")
    }

    fn git_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
        let mut command = std::process::Command::new("git");
        command
            .args(["-c", "user.email=eix@example.com", "-c", "user.name=eix"])
            .args(args)
            .current_dir(dir);
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("run git")
    }

    #[tokio::test]
    async fn recovers_a_conflicted_rebase_with_a_backup() -> anyhow::Result<()> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("eix-recovery-{}-{stamp}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;

        assert!(git(&root, &["init", "-q", "-b", "main"]).status.success());
        std::fs::write(root.join("file.txt"), "base\n")?;
        assert!(git(&root, &["add", "file.txt"]).status.success());
        assert!(git(&root, &["commit", "-q", "-m", "base"]).status.success());

        assert!(git(&root, &["checkout", "-q", "-b", "feature"])
            .status
            .success());
        std::fs::write(root.join("file.txt"), "feature\n")?;
        assert!(git(&root, &["add", "file.txt"]).status.success());
        assert!(git(&root, &["commit", "-q", "-m", "feature"])
            .status
            .success());

        assert!(git(&root, &["checkout", "-q", "main"]).status.success());
        std::fs::write(root.join("file.txt"), "main\n")?;
        assert!(git(&root, &["add", "file.txt"]).status.success());
        assert!(git(&root, &["commit", "-q", "-m", "main"]).status.success());
        assert!(git(&root, &["checkout", "-q", "feature"]).status.success());

        let rebase = git(&root, &["rebase", "main"]);
        assert!(
            !rebase.status.success(),
            "rebase should conflict: {}",
            String::from_utf8_lossy(&rebase.stderr)
        );
        assert!(in_rebase(&root).await);
        assert!(rebase_has_work(&root).await);

        let recovery = recover_rebase(&root)
            .await?
            .expect("recovery should run for an in-progress rebase");
        assert!(recovery.had_work);
        assert!(recovery.conflicts.iter().any(|file| file == "file.txt"));
        assert!(recovery.backup_dir.is_dir());
        assert!(std::fs::read_dir(&recovery.backup_dir)?.next().is_some());
        let backup_ref = recovery.backup_ref.as_deref().expect("backup ref");
        assert!(git(&root, &["rev-parse", "--verify", backup_ref])
            .status
            .success());
        assert!(!in_rebase(&root).await);
        assert!(is_clean(&root).await?);
        assert_eq!(std::fs::read_to_string(root.join("file.txt"))?, "feature\n");

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[tokio::test]
    async fn recovers_an_edit_stopped_rebase_without_work() -> anyhow::Result<()> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("eix-recovery-edit-{}-{stamp}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;

        assert!(git(&root, &["init", "-q", "-b", "main"]).status.success());
        std::fs::write(root.join("file.txt"), "base\n")?;
        assert!(git(&root, &["add", "file.txt"]).status.success());
        assert!(git(&root, &["commit", "-q", "-m", "base"]).status.success());
        assert!(git(&root, &["checkout", "-q", "-b", "feature"])
            .status
            .success());
        std::fs::write(root.join("file.txt"), "feature\n")?;
        assert!(git(&root, &["add", "file.txt"]).status.success());
        assert!(git(&root, &["commit", "-q", "-m", "feature"])
            .status
            .success());

        let rebase = git_env(
            &root,
            &["rebase", "-i", "main"],
            &[
                ("GIT_SEQUENCE_EDITOR", "sed -i '1s/^pick /edit /'"),
                ("GIT_EDITOR", "true"),
            ],
        );
        assert!(
            in_rebase(&root).await,
            "rebase should stop at edit: {}",
            String::from_utf8_lossy(&rebase.stderr)
        );
        assert!(!rebase_has_work(&root).await);

        let recovery = recover_rebase(&root)
            .await?
            .expect("recovery should clear an edit-stopped rebase");
        assert!(!recovery.had_work);
        assert!(recovery.backup_patch.is_none());
        assert!(recovery.backup_files.is_none());
        assert!(recovery.backup_dir.is_dir());
        assert!(!in_rebase(&root).await);
        assert!(is_clean(&root).await?);
        assert_eq!(std::fs::read_to_string(root.join("file.txt"))?, "feature\n");

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
