# Context and context management

Every request has to fit the system prompt, the instruction files, the tool definitions, the skill
catalog, the conversation history, and tool output. `[context]` decides how that package is
trimmed and when compaction fires.

## 1. What a request is made of

Ordered from most to least stable:

1. **System prompt**: the built-in text plus the agent's `prompt`.
2. **Instruction files**: `AGENTS.md` and friends, see section 3.
3. **Tool definitions**: built-in tools, MCP tools, plugin tools.
4. **Skill catalog**: `<available_skills>`, one description line each; bodies are read on demand.
5. **Workspace summary**: cwd and top-level entries (`workspace_summary`); git status lives in the
   separate environment section.
6. **Conversation history**: user messages, assistant messages, tool calls and results.

The first few items form a stable prefix; only the last one keeps growing. Compaction touches the
history and nothing else.

## 2. Settings

```toml
[context]
budget_tokens = 120000
auto_compact = true
auto_compact_threshold = 0.8
auto_compact_keep_tokens = 20000
auto_compact_keep_turns = 2
tool_output_tokens = 6000
image_tokens = 1500
instructions_files = ["AGENTS.md"]
workspace_summary = true
```

Those numbers are **illustrative, not factory defaults**. Before changing anything, read the
`[context]` comment block in `config.tmpl.toml`; it carries defaults matched to the model window
you actually use.

| Field | Effect |
| --- | --- |
| `budget_tokens` | Soft ceiling for one request; past it, old tool output and history are trimmed first |
| `auto_compact` | Whether to summarise old history automatically before the window overflows |
| `auto_compact_threshold` | Trigger ratio in (0, 1]; 0.8 means compact at 80% of the budget |
| `auto_compact_keep_tokens` | Budget of recent conversation kept **verbatim** outside the summary; this is the primary retention control |
| `auto_compact_keep_turns` | Minimum number of recent user turns kept even if they exceed that budget |
| `tool_output_tokens` | Cap on a single tool result entering the request; the full output stays in the event log |
| `image_tokens` | Conservative per-image estimate, since vision pricing varies by model; overridable |

`auto_compact_keep_tokens` and `auto_compact_keep_turns` relate as "budget first, turns as a
floor": the former sets how much raw text survives the summary, the latter guarantees the newest
turns never exist only as a summary.

## 3. Instruction files

`instructions_files` is a list of workspace-relative paths; missing files are skipped. In
addition, `AGENTS.md` is read once from the eix config directory (`~/.config/eix/AGENTS.md`).

Precedence matches the usual agent convention: the closer a file is to the current directory, the
later it is appended, and on conflict the later file wins. Put general preferences in the global
file and repository conventions in the project file.

This content lands in the "instructions" sector and is preserved ahead of conversation history.

## 4. Manual control

| Command | Effect |
| --- | --- |
| `/context` | Show current usage: per-sector estimates, budget, headroom |
| `/compact` | Compact once now, without waiting for the threshold |
| `/effort` | Change the reasoning-effort level, which shifts output length per turn |

Automatic compaction is checked before each turn. It folds older history into a summary, keeps the
most recent `auto_compact_keep_tokens` verbatim, and preserves at least
`auto_compact_keep_turns` user turns.

Compaction is lossy: detail behind the summary exists only in the event log, not in the model's
context. Restate important facts in the conversation, or put them in `AGENTS.md`.

## 5. What gets sacrificed first

Roughly in this order:

1. The part of any tool output beyond `tool_output_tokens`, oldest first.
2. Older history folded into the summary.
3. The skill catalog: descriptions are truncated, then dropped, then trailing entries are lost.

Tool definitions and the system prompt are never trimmed, so **installing many MCP servers eats
conversation space** instead of being automatically shrunk. When the tool set grows, narrow it with
`allowed_tools`/`denied_tools` or disable servers you are not using.

## 6. Troubleshooting

**Every turn is slow and expensive.** Use `/context` to see whether tool definitions or
instruction files consume most of the budget; an oversized tool-output cap also inflates history.

**The model "forgot" earlier decisions after compaction.** A summary keeps only the essentials.
Put hard constraints in `AGENTS.md`, which compaction leaves untouched.

**Compaction fires after a few messages.** Check whether `budget_tokens` is set below the model's
real window, and whether `auto_compact_threshold` is too small.

**Some skills are missing from the catalog.** The catalog is capped at 2% of the context window
and at most 8 KiB; entries at the end are dropped in order once the cap is hit.
