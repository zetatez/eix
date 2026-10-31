# Worktrees

By default every eix session shares its `cwd` with all the others. To let two sessions change the
same repository without stepping on each other, give a session a **git worktree**: its own
directory, its own branch, with commits merged back through git.

```console
$ eix new --worktree fix-login   # create the worktree now; the session runs inside it
$ eix worktree list              # which sessions have a worktree, and does the directory exist
$ eix ready                      # queue for integration (or press /ready in the TUI)
```

## Directory and branch conventions

Worktrees are always created under the **main repository**:

```
<repo>/.worktrees/<slug>   directory
eix/<slug>                 branch
```

- `<slug>` comes from the session name (or `--name`, or an auto-generated short name) and is
  converted into a legal branch fragment.
- If the name or branch already exists, a `-2`, `-3` … suffix is appended; an existing directory or
  branch is never silently reused.
- eix appends `.worktrees/` to `.git/info/exclude` and **never touches your `.gitignore`**, so it
  does not show up in `git status` and cannot be committed.
- Paths are canonicalised absolute paths, so `cwd` stays stable when the repository sits behind a
  symlink.
- A session that already lives in a linked worktree is fine: creation first resolves the main
  repository (the parent of `git rev-parse --git-common-dir`), which prevents nested
 `.worktrees` directories.
- If the session started in a **subdirectory** of the repository, the worktree `cwd` lands in the
  same relative subdirectory (falling back to the worktree root when that directory does not
  exist), so you are never moved somewhere unexpected.

## Four ways to create one

### 1. Command line

```console
eix new --worktree              # repository of the current directory; worktree named after the session
eix new --worktree --name feat-x # explicit name
eix new --shared                # the explicit opposite: share the current directory, no worktree
```

`--worktree` and `--shared` are mutually exclusive and error out when combined. On success the CLI
prints an extra line, `worktree /path/to/.worktrees/feat-x`. Without `--name` the session name
defaults to the directory name of `cwd`, so the worktree is usually named after the repository
directory; short names such as `fix-login` need `--name` or TUI auto-naming.

### 2. TUI

In normal mode press `Space W` (bound to `new_worktree` by default) to move the **current pane's
session** into a new worktree. It runs three preconditions and only shows a message instead of
issuing a request when one fails:

- no session is attached yet;
- the session already has a worktree;
- the session is running a turn (`busy`), so you must wait for it to finish.

The move loses no history: only `cwd` and the `worktree` field change, while the event log and
context remain. The RPC equivalent of `Space W` is `session.worktree.create`.

### 3. Automatic isolation on fork

`session.fork`'s `worktree` defaults to `None`, which means **auto**:

```rust
None => self.config.load().worktree.auto && repo_root(source.cwd).is_some()
```

In other words, a fork gets its own worktree **only when `[worktree] auto = true` and the source
session is inside a git working tree**; otherwise it shares the source workspace. Remote clients
(HTTP/SDK) are stricter: `None` is treated as `false`, so they must pass `worktree: true`
explicitly.

```console
eix fork --at 12 --name spike   # --name works here; isolation depends on [worktree] auto
eix fork --shared               # explicitly share the source workspace (worktree: false)
```

Two details worth remembering:

- With a worktree the CLI notes `files are shared through git only`: the forked directory is a
  **fresh checkout** and uncommitted changes are not copied. To roll files back, use
 `eix rewind`.
- When sharing, the note reads `the workspace is shared; files were not rolled back`.
- A forked worktree branches from the **source session's base branch**, not from its temporary
 `eix/<name>` branch.

### 4. Global switch: one per new session

```toml
[worktree]
auto = true
base = "auto"
```

With this on, a new session creates its worktree **when the first prompt is submitted**, not at
creation time, which lets the name follow the task: the model returns a two-to-five-word English
slug, with a local fallback rule if that fails.

This automatic path is deliberately narrow:

- It applies only to sessions eix created itself. Manually imported or legacy rows have no
  integration record and are never moved out from under you.
- Sessions carrying `worktree_disabled` (that is, created with `--shared`) are skipped.
- Two prompts arriving for the same session at once create one worktree; the other receives
 `busy`.
- A remote session whose repository root is outside the workspace the daemon exposes stays where
  it is.
- Name generation costs a model call, so it happens only on the first prompt and never twice.

## Listing and switching

```console
$ eix worktree list
● fix-login   3f2a91c4   ok      /home/me/repo/.worktrees/fix-login
○ spike       9b1e0077   missing /home/me/repo/.worktrees/spike
```

The columns are the status marker, name, short session id, directory state (`ok` / `missing`), and
absolute path. `missing` means the directory was deleted by hand; the record stays until
`git worktree prune` runs. When no session has a worktree, a single notice line is printed.

In the TUI, `Space g` opens the git picker with the repository's worktrees and branches. Selecting
an entry **inserts the path into the prompt** (handy for pointing the agent at that directory); it
does not `cd` and does not switch sessions.

## Integration: merging the branch back

Commits made inside a worktree do not return to `main` on their own. The integration queue does
that:

```console
eix ready                  # mark the current directory's session worktree as ready
eix ready <SESSION_ID>     # explicit session (id or id prefix only, not a name)
eix integrate status       # queue state
eix integrate retry        # requeue
eix integrate continue     # same, used after conflicts are repaired
```

`retry` and `continue` are the same code path and both call `integration.continue` to requeue; the
difference is only semantic. Without an argument they use the worktree session that owns the
current directory, so running them from a subdirectory of the worktree still finds it.

The TUI equivalents are `/ready` and `/integrate`.

### What the queue does

1. If `[integration] auto_commit = true` (**on by default**), commit the dirty changes in the
  worktree first, so `ready` does not fail on the agent's own edits.
2. Resolve the base branch and verify the base worktree: it must be **exactly on** that branch and
  **clean**, otherwise the run fails immediately (`base worktree … is on …`,
 `base worktree … has uncommitted changes`).
3. `git fetch --prune origin`, then `pull --ff-only`; with no upstream but an existing
 `origin/<base>`, it uses `merge --ff-only origin/<base>` instead.
4. `git rebase <base>` inside the worktree.
5. Run the test command (`auto` by default, which detects the right command for Cargo, npm, Go, and
  Python repositories).
6. `git merge --ff-only eix/<name>` in the base worktree.
7. Skip pushing when `push = false`; it **never force pushes**.

The switches for steps 3, 5, and 7 all come from `[integration]`: `test`, `timeout_secs`,
`pull_base`, `push`, plus `retry_max_secs`, `notify_after`, `pause_after`,
`conflict_repair`, `enabled`, and `idle_secs`, all of which participate in deserialisation.
Omitted keys take built-in defaults: `test = "auto"`, `timeout_secs = 1800`, `pull_base = true`,
`push = false`, `retry_max_secs = 300`, `notify_after = 3`, `pause_after = 0`,
`conflict_repair = true`, `enabled = true`, and `idle_secs = 300`.

`[integration.repos."<abs path>"]` supports four per-repository overrides: `base`, `test`,
`timeout_secs`, and `pull_base`; any key you omit inherits the global value. The key may be an
absolute path or just a directory name (the second `repo_override` pass retries with
`file_name()`).

When `test = "auto"` cannot detect a usable command, `server.rs` enters the repair flow with
a note reading "Set `[integration] test` …". That note is actionable: write a `test` entry, or
override it per repository through `repos`.

### Choosing the base branch

With `[worktree] base = "auto"` (the default), the order is:

1. the branch `origin/HEAD` points at, if a local branch of that name exists;
2. `main`;
3. `master`;
4. the repository's current branch.

If none apply, it reports `cannot determine base branch; set [worktree] base explicitly`; set it to
a concrete branch name. You can also override it per repository with `base` in
`[integration.repos."/abs/path"]`.

### Your work survives a rebase conflict

When conflict repair fails, eix backs up first and then runs `git rebase --abort` to return the
branch to its pre-rebase state. Backups live in the **main repository's** `.git/eix/recovery/`
(linked worktrees write there too, because the common git dir outlives the worktree):

```
.git/eix/recovery/<stamp>.patch    index + working tree diff, binaries included
.git/eix/recovery/<stamp>-files/   one copy per conflicted or changed file
.git/eix/recovery/<stamp>.txt      metadata: branch, HEAD, onto, conflicted file list
refs/eix/recovery/<stamp>          a ref pointing at the original HEAD
```

Untracked files are neither patched nor copied (so a build directory is not duplicated wholesale);
they are only recorded in the metadata, since `--abort` does not delete them anyway. If `--abort`
fails, eix falls back to `--quit` and restores the branch explicitly.

## Removing

```console
eix worktree list            # get the session id first
eix worktree rm 3f2a91c4     # refused by git when there are uncommitted changes
eix worktree rm 3f2a91c4 --force   # discard uncommitted files
```

The positional argument goes through `resolve_session`, so it matches only a session id or id
prefix (multiple prefix hits report `ambiguous`); it **does not accept a session or worktree
name**.

The implementation runs `git worktree remove [--force] <path>` followed by `git worktree prune`.
Three things to know:

- **The `eix/<name>` branch is not deleted**; clean it up yourself with `git branch -d`.
- If the directory was already deleted by hand, `remove` fails; running `git worktree prune`
  directly is easier.
- On success the session still exists: `cwd` moves back to the **main repository root**, the
 `worktree` field is cleared, and integration state resets to `pending`. In other words, moving a
  session back to the shared directory is a supported path.
- While the session is running a turn, or its integration is active, `remove` is refused (`busy`);
  wait for the turn to end or run `eix integrate continue`.

## Interaction with other mechanisms

- **Snapshots and rewind do not overlap.** `.worktrees` is on the snapshot exclusion list
  (alongside `.git`, `target`, and `node_modules`), so rolling back a session in one worktree never
  touches another. Snapshots inside a single worktree still skip `.git`, `target`, and the rest.
- **Fork and rewind semantics are unchanged.** A fork with a worktree is a fresh checkout; use
 `eix rewind` to roll files back.
- **HTTP/SDK clients** use the same `session.create { worktree: true }` and
  `session.worktree.create` / `session.worktree.remove`; fields are defined in
  [`crates/eix-core/src/protocol.rs`](../crates/eix-core/src/protocol.rs).

## Configuration reference

```toml
[worktree]
auto = false            # true: create a worktree on each new session's first prompt
base = "auto"          # or a branch name such as "main"

[integration]
enabled = true          # false: disable the whole queue
auto_ready = false      # true: auto-ready after idle_secs of inactivity
idle_secs = 300
auto_commit = true      # commit dirty worktree changes before ready
test = "auto"             # or a fixed command such as "cargo nextest run"
timeout_secs = 1800     # wall-clock ceiling for one test run
pull_base = true        # fetch and pull --ff-only the base before rebasing
push = false            # push the remote after a local fast-forward
retry_max_secs = 300    # maximum backoff for transient failures
notify_after = 3        # notify after this many consecutive failures; 0 = never
pause_after = 0         # pause after this many consecutive failures, awaiting `eix integrate continue`; 0 = never
conflict_repair = true  # let the agent try to repair conflicts and test failures

#[integration.repos."/abs/path"]
# base = "main"
# test = "npm test"
# timeout_secs = 600
# pull_base = false
```

The values above are the built-in defaults and all of them are read from configuration; omitting
any key falls back to its default. Semantics are described in the previous section.

`[worktree]` and `[integration]` **belong in the global config only**. A project config (a config
file inside the repository) that sets either table has it dropped with a warning, because both are
executable policy.

## Common problems

**`not inside a git work tree`**
`--cwd` (or the daemon's cwd) is not inside a git repository. Run `git init` first, or change
directory.

**The directory was created by `--worktree` but `eix worktree list` does not show it**
The list shows only sessions that have a worktree. When the worktree was added but the session
failed to be created, that `git worktree add` is not rolled back; confirm with `git worktree list`
and remove it by hand.

**A session should return to the shared directory**
`eix worktree rm <SESSION_ID>` deletes the worktree directory and moves the session's `cwd` back to
the main repository root (not to the subdirectory it started in). To keep the directory, create a
new session without `--worktree` instead.

**Integration keeps failing**
Start with `eix integrate status`. After repairing conflict failures as the message suggests, run
`eix integrate continue`; when the base is dirty, commit or clean the base worktree by hand and
then run `eix integrate retry`.
