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
budget_tokens = 0                 # 0 = use the model window
auto_compact = true
auto_compact_threshold = 0.8
auto_compact_keep_tokens = 20000
auto_compact_keep_turns = 2
tool_output_tokens = 6000
image_tokens = 1500
instructions_files = ["AGENTS.md"]
workspace_summary = true
```

`budget_tokens = 0` is the factory default (auto). The other numbers are **illustrative, not
factory defaults**. Before changing anything, read the `[context]` comment block in
`config.tmpl.toml`.

| Field | Effect |
| --- | --- |
| `budget_tokens` | Soft ceiling for one request; `0` (default) uses the model window minus the completion reserve, falling back to 32k when the window is unknown |
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
| `/compact [notes]` | Summarize the conversation once, now; `notes` steers what the summary keeps |
| `/effort` | Change the reasoning-effort level, which shifts output length per turn |

Automatic compaction is checked before each turn and after every tool batch. It folds older history
into a summary, keeps the most recent `auto_compact_keep_tokens` verbatim, and preserves at least
`auto_compact_keep_turns` user turns.

Both automatic and manual compaction are **asynchronous**. The turn keeps running while the summary
model works; the `ContextCompacted` event is appended when it finishes, and the next ledger rebuild
(the next tool round or the next turn) loads it. `/compact` returns as soon as the request is
accepted, even while a turn is running. Repeated requests while a summary is in flight are merged
into one pending job.

Compaction is lossy: detail behind the summary exists only in the event log, not in the model's
context. Restate important facts in the conversation, or put them in `AGENTS.md`.

By default `budget_tokens` is `0`, which means "use the model window": the request budget grows
with a 128k or 1M model instead of compacting at a fixed 32k ceiling. The effective budget is the
window minus a reserve for the completion and tokenizer differences. The window comes from the
model entry, `[provider_runtime].context_tokens`, or a provider catalog entry that has already been
fetched. When it is unknown, eix keeps a conservative 32k fallback so an 8k local model is not sent
a 1M prompt. Set an explicit `budget_tokens` to cap spending or to compact earlier; a value above
the window is clamped automatically.

The trigger compares usage with the effective budget, not with the model's full window. A small
`budget_tokens` therefore compacts earlier and more often than a harness that waits for the model
window to fill; raise it (within the model's window) if you prefer fewer, larger compactions.
Summaries reuse the session model and the conversation prefix, which keeps provider prompt-cache
hits high when the provider supports them.

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

**Compaction fires after a few messages.** Check whether the model declares `context_tokens`
(otherwise eix cannot grow the auto budget past its 32k fallback), whether an explicit
`budget_tokens` is set below the real window, and whether `auto_compact_threshold` is too small.
Opening `/model` once fetches the provider catalog, which can supply the window for listed models.

**Some skills are missing from the catalog.** The catalog is capped at 2% of the context window
and at most 8 KiB; entries at the end are dropped in order once the cap is hit.
