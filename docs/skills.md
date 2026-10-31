# Skills, commands, and agents

All three extension kinds are Markdown files read from disk at startup or on use, and none of them
requires a rebuild. What differs is how they reach the model.

| | Skills | Commands | Agents |
| --- | --- | --- | --- |
| Location | `skills/<name>/SKILL.md` | `cmds/**.md` | `agents/*.md` |
| Trigger | The model calls `use_skill` on demand | The user types `/name` | The user picks one, or `spawn_agent` |
| Enters context | One catalog line first, full text after the call | The expanded prompt | Appended to the system prompt |
| Key fields | `description`, `allowed_tools` | `description`, `agent`, `model` | `role`, `model`, `description` |

## 1. Skills

### Locations and precedence

Global: `~/.config/eix/skills/<name>/SKILL.md` (respects `EIX_CONFIG_DIR`). Project:
`.eix/skills/`. On a name clash the project copy overrides the global one, and user files override
built-ins.

Built-in skills such as `frontend` ship with the binary. You can use them as-is or shadow one with
a file of the same name.

### File format

Markdown with frontmatter:

```markdown
---
name: release-notes
description: Turn a git log into release notes. Use for "release notes" or "changelog".
allowed_tools: ["exec_command", "read_file"]
---

# Release notes

Read the range with `git log`, group changes by type, then write the notes.
```

`description` is what tells the model when the skill applies, so write "what it does + when to use
it". The body is the full instruction set the model reads after calling `use_skill`.

`allowed_tools` is an optional restriction: the tools the model may still use after loading the
skill. Leaving it empty means no restriction. `*` and `?` wildcards are supported.

### How the catalog enters context

eix never puts every skill body into the request. It emits one `<available_skills>` catalog with a
name and description per skill; bodies stay on disk until `use_skill` reads them.

The catalog itself costs budget: it is capped at 2% of the context window and at most 8 KiB. Past
the cap it degrades in order: truncate descriptions, then halve them, then drop descriptions, then
drop trailing entries. In other words, with many skills installed the later ones may not appear at
all, so tighter descriptions beat more entries.

`use_skill` accepts `arguments` for skills whose bodies use `$ARGUMENTS` / `$1`..`$9`.

## 2. Commands

### Locations

| Location | Purpose |
| --- | --- |
| `~/.config/eix/cmds/` | Global commands |
| `<project>/.eix/cmds/` | Project commands; subdirectories group them |
| `~/.config/eix/commands/` | Legacy directory name, still read |
| `<project>/.opencode/commands/` | OpenCode-compatible directory |

Subdirectories become part of the command name: `cmds/team/review.md` is `/team/review`, with `/`
as the separator. The nearer project source wins over the global one and over ancestor
directories.

### Body and arguments

The body is the prompt template. Frontmatter is optional:

```markdown
---
description: Review the staged diff
agent: reviewer
model: claude-sonnet-4
---

Review the staged changes. Focus on $ARGUMENTS.
Diff stat: $1
```

| Placeholder | Expands to |
| --- | --- |
| `$ARGUMENTS` | The whole argument string |
| `$1`, `$2`, … | Positional; the highest placeholder absorbs the rest |

`agent` selects the agent that handles the command and `model` overrides the model for this run.
Both are optional.

## 3. Agents

### Definition

In `~/.config/eix/agents/*.md` or the project's `.eix/agents/*.md`:

```markdown
---
name: reviewer
description: Review changes for correctness and scope
role: primary
model: claude-sonnet-4
---

You review diffs. Report findings by severity with file:line evidence.
```

`prompt` (that is, the body) is appended after the built-in system prompt; it never replaces it.

`role` decides where the agent can be used:

| Value | Meaning |
| --- | --- |
| `primary` / `main` | Usable only as the main agent |
| `subagent` / `sub` | Reachable only through `spawn_agent` |
| Anything else or omitted | Either |

### Built-in agents

`explore` is the built-in read-only sub-agent: search and analysis only, no edits, no mutating
commands, no questions, and evidence returned as `file:line`. When a task needs "investigate first,
then change", running it through `explore` is more predictable than letting the main agent
investigate and edit at the same time.

The built-in read/write workflows are session modes rather than agents, so they never show up in
the agent list.

### Override order

Project overrides global, and user overrides built-in. Overrides are by name with no merging: a
same-named file replaces the earlier one wholesale.

## 4. Compatibility with other tools

Beyond the directories above, eix can read resources from other harnesses, gated per source in
config: `claude_code`, `opencode`, `codex_cli`, `gemini_cli`, `pi`.

When enabled, the matching locations are scanned too, for example Claude Code's `~/.claude` and the
project's `.claude`, OpenCode's `~/.config/opencode` and `.opencode`, Codex's `$CODEX_HOME` and
`.codex`, Gemini's `~/.gemini` and `.gemini`, and Pi's `.pi` / `.agents`. The mapping differs per
source: Codex's `prompts/`, for instance, is read as commands.

The compatibility layer is read-only; it never modifies those directories.

## 5. Troubleshooting

**A slash command does not appear.** Confirm the file is under `cmds/`, ends in `.md`, and is
spelled correctly. Subdirectories become part of the name (`/team/review`).

**A skill never triggers.** Check whether `description` states when to use it. When the catalog is
under budget pressure, long descriptions are truncated and may disappear entirely.

**Edits to a file have no effect.** Skills and agents are re-read on lookup and commands are loaded
when a session starts, so reopening the session after a change is the reliable path.

**An agent cannot be selected.** Check `role`: anything marked `subagent` is reachable only through
`spawn_agent` and never appears in the main agent picker.
