# eix usage overview

eix is a **session-resident** coding agent: a long-lived daemon (`eixd`) owns every session, event
log, tool execution, and approval, and each frontend (TUI, CLI, HTTP/SDK, Neovim, ACP) is merely a
client of it. Closing the terminal does not interrupt a running turn, and another machine can
attach to the same session.

```sh
eix  # open the TUI (the default subcommand)
eix doctor  # environment check: config, paths, daemon state
```

This page is the entry-level overview; each topic has its own document:
[Worktrees](./worktree.md) · [Voice](./voice.md) · [Voice setup](./voice-setup.md) ·
[Plugins](./plugins.md) · [dylib plugins](./plugin-dylib.md) · [Neovim](./nvim.md) ·
[LSP](./lsp.md) · [Design](../DESIGN.md)

## 1. Installation and first run

**Requirements**: Rust stable 1.89+, Cargo, Linux or macOS (Windows is unsupported). `bwrap` is
optional and used for the command sandbox.

```sh
git clone https://github.com/zetatez/eix.git
cd eix
cargo build --release
./target/release/eix doctor
```

The build produces three binaries:

| Binary | Purpose |
| --- | --- |
| `eix` | TUI plus CLI, the only entry point you need day to day |
| `eixd` | The daemon, started automatically on first connect in its own process group |
| `eix-asr` | Optional speech helper; without it there is no local transcription |

## 2. Configuring a provider

The config file lives at `~/.config/eix/config.toml` by default. Exactly one provider sets
`default = true`:

```toml
[providers.openai]
default = true
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
```

```sh
export OPENAI_API_KEY=$OPENAI_API_KEY
eix
```

Common variations:

- **Local models**: `base_url = "http://localhost:11434/v1"` with a local `model`.
- **Native protocols**: `kind = "anthropic"` / `"gemini"` / `"deepseek"`; the default is `openai`.
- **OAuth**: `eix auth login anthropic`; the token is stored daemon-side, never in the config.
- **Separate planning and execution models**: set `plan` and `execute` under `[model_roles]`.

To manage credentials without hand-editing config:

```sh
eix auth login openai --api-key $OPENAI_API_KEY   # or --stdin / --no-browser for OAuth
eix auth list                         # where each provider's credential comes from
eix model show                           # daemon default model and auth status
eix model use openai/gpt-4o              # switch the default at runtime
eix model default                      # revert to the configured default
```

Every setting is documented in [`config.tmpl.toml`](../config.tmpl.toml).

## 3. Core concepts

**Session**: one conversation plus one event log, with its own `cwd`, model, execution mode, and
permission policy. Many can run in parallel without interfering.

**Event log**: an append-only SQLite log. Each request rebuilds context from the log, which is why
fork, rewind, export, and multi-client sync are all just log reads.

**Pane**: one viewport in the TUI; a pane attaches to one session. The same session can be attached
by several panes (or several clients) at once.

**Execution mode**: `plan` / `build` / `auto` / `full`, deciding what tools may do and when approval is
needed. `build` is the default. `full` lifts workspace path restrictions and allows "always allow"
questions, but **hard denials and explicit rules still apply**. Switch with `Tab` / `Shift-Tab` in
the TUI, or `eix mode build`.

**Context**: `AGENTS.md`, the workspace summary, the per-sector token budget, automatic compaction, and
cross-session memory are all assembled daemon-side. `/context` shows usage and `/effort` changes
the reasoning level.

## 4. TUI

```sh
eix [--session ID]
```

### Keys

| Key | Action |
| --- | --- |
| `F1` | Searchable help |
| `F2` (hold) | Hold to talk; release to stop and transcribe |
| `Ctrl-p` | Command palette |
| `Ctrl-t` | Cycle reasoning level |
| `Ctrl-/` / `Ctrl-_` / `Ctrl-7` | Model picker |
| `Space o` / `Space h` | Session picker / input timeline |
| `Space f` / `Space s` / `Space g` | File picker / content search / git picker |
| `Space n` / `Space W` | New session / move into a new worktree |
| `Space a` / `Space b` | Approval inbox / background processes |
| `Space v` / `Space t` / `Space r` | Horizontal / tabbed / Fibonacci layout |
| `Space l` / `Space x` | Cycle layout / cancel the current turn |
| `Tab` / `Shift-Tab` | Switch execution mode |
| `Ctrl-1…9` | Focus the Nth session (requires the kitty keyboard protocol) |
| `Enter` / `Esc Esc` | Send or queue the prompt / interrupt and return to the transcript |
| `q` / `Z` / `Ctrl-c` / `Ctrl-d` | Close the pane / quit from normal mode / quit from any mode |

The prompt is a native Vim modal editor with motions, operators, counts, undo, and leader chords,
and it needs no subprocess. `[keys]` rebinds global, normal, and leader bindings (navigation inside
overlays is fixed). A binding may list several comma-separated keys, and a leader chord is written
as `new_session = "leader n"`.

### Slash commands

| Command | Action |
| --- | --- |
| `/help`  `/cancel`  `/exit` | Help / cancel the turn / quit |
| `/new`  `/log` | New session / reprint the transcript |
| `/model`  `/model-plan` | Pin this session's execution / planning model |
| `/rewind`  `/undo`  `/redo` | Checkpoint rollback for files and conversation |
| `/context`  `/effort` | Context usage / reasoning level |
| `/branch`  `/diff`  `/goal`  `/loop`  `/schedule` | Branch, show the diff, long-running goals, loops, schedules |
| `/ready`  `/integrate` | Queue for integration / show integration status |
| `/new-worktree` | Move the current session into a new worktree |
| `/doctor`  `/voice` | Self-check / voice input |
| `/code-review`  `/security-review`  `/simplify`  `/verify` | Built-in prompt templates |

The complete list is in the `F1` help panel or in [DESIGN.md](../DESIGN.md#19-实现参考).

### Approvals and diffs

High-risk patches open a **per-hunk** diff, and both approve and deny accept a reason. `Space a`
opens the cross-session approval inbox. Without the TUI, use the CLI:

```sh
eix approvals            # list every pending approval
eix approve --hunks 0,2   # approve only hunks 0 and 2
eix deny --reason "try another approach"
eix answer 1         # answer a question from the agent
```

## 5. CLI

The CLI and the TUI operate on the same sessions, which suits scripts, status bars, and remote
shells.

### Sessions

```sh
eix ls [--archived]      # list
eix new [--name N] [--cwd P] [--model M] [--worktree|--shared] [--mode M]
eix chat [--session ID]     # plain-text interaction (no TUI)
eix send [--session ID] "..."  # send once and stream the reply
eix log [--session ID]    # print the persisted transcript
eix rename [--session ID] NAME
eix cancel [--session ID]
eix delete <session>
eix archive <session> [--undo]   # hide but keep the events
```

### History and branching

```sh
eix fork [--at SEQ] [--name N] [--shared] # copy history into a new session
eix rewind [--at SEQ] [--name N]   # fork and roll the workspace back before SEQ
eix undo / eix redo                     # hide / restore the most recent turn
eix timeline [--limit N] [--json]   # turn list for navigation
eix export [--output FILE]     # export JSON
eix import <file> [--name N] [--cwd P] # import as a new session
```

`fork` shares files by default; only `rewind` touches the workspace. Both preserve the complete
event history.

### Background processes

```sh
eix ps [--session ID] # list background processes
eix tail PROCESS     # follow the tail of the log
eix kill PROCESS     # stop it
```

### Worktrees and integration

```sh
eix worktree list
eix worktree rm <SESSION_ID> [--force]
eix ready [--session ID]
eix integrate [status | continue | retry]
```

Mechanics (directory conventions, rebase-conflict backups, base branch resolution) are in
[worktree.md](./worktree.md).

### Status and external tooling

```sh
eix status [--json]       # daemon and session summary; --json feeds status bars
eix pick [--query Q] [--limit N] # id<TAB>name<TAB>status<TAB>cwd, for rofi or fzf
eix doctor             # config, path, and daemon diagnostics
eix daemon             # start or attach and print status
```

## 6. Remote access and multiple clients

The daemon exposes both HTTP and TCP entry points, authenticated by pairing tokens.

```sh
eix pair --name laptop         # mint a one-time pairing code
eix --tcp 127.0.0.1:7777 --token <TOKEN> tui
```

`--tcp` / `--token` also work through the environment variables `EIX_TCP` / `EIX_TCP_TOKEN`.

HTTP endpoints are `/health`, `/openapi.json`, `/stats`, `POST /rpc`, and the SSE stream at `/events`. A new
client first exchanges its pairing code for a token with `POST /pair`. Rust clients use
[`eix-sdk`](../crates/eix-sdk), TypeScript clients use [`clients/ts`](../clients/ts).

Editor side:

- **Neovim**: the plugin is an independent daemon client and does not take over buffers or
  keymaps; see [nvim.md](./nvim.md).
- **ACP**: `eix acp` runs an Agent Client Protocol adapter over stdio.

```json
{ "agent_servers": { "eix": { "command": "eix", "args": ["acp"], "env": {} } } }
```

## 7. Voice input

`[voice]` is enabled by default, but audio is **only captured when you ask for it**:

- `F2` holds to talk and stops and transcribes on release (this needs key-release reporting, that
  is, the kitty keyboard protocol; terminals without it degrade to press-once-to-toggle, with a
  60-second ceiling as a backstop).
- `/voice` starts and stops manually, which suits long passages.

The engine is chosen automatically: `sherpa` when SenseVoice is installed, otherwise `whisper`.
Model placement, download verification, `args` semantics, and troubleshooting are covered in
[voice-setup.md](./voice-setup.md) and [voice.md](./voice.md).

## 8. Configuration

### File precedence

1. `--config PATH` or `EIX_CONFIG`: **highest priority, and the project layer is no longer merged**
  (reproducible config).
2. `~/.config/eix/config.toml`: global.
3. `.eix/config.toml` in each ancestor directory: the nearer to the current directory, the
   higher the priority.

Tables merge and arrays replace. The project layer is treated as untrusted: `{env:}` / `{file:}`
expansion does not apply, and executable-policy tables (`plugins`, `integration`, `worktree`, and
`lsp`) are stripped with a warning, since those belong in the global config only.

### Main sections

| Section | Contents |
| --- | --- |
| `[providers.*]` / `[provider_runtime]` | Model endpoints, credentials, timeouts, retries |
| `[models]` / `[model_roles]` | Model catalog; which model `plan` / `execute` use |
| `[tools]` | Sandbox, network allowlist, snapshots, parallel sub-agent count, allow/ask/deny rules |
| `[context]` | Token budget, compaction threshold, `AGENTS.md` and other instruction files |
| `[integration]` / `[worktree]` | Integration queue and worktree policy |
| `[keys]` / `[theme]` | Key bindings and colors |
| `[notifications]` | Bell or external command when attention is needed |
| `[voice]` / `[voice.*]` | Voice toggle, capture thresholds, engine, models |
| `[mcp.servers.*]` / `[plugins.*]` | MCP servers; out-of-process plugins |
| `[images]` / `[math]` / `[formatter]` | Image and formula rendering, formatting |
| `[lsp]` | Language servers and diagnostics, see [LSP and language servers](./lsp.md) |
| `[server]` | HTTP / TCP listeners and token |
| `[compations]` | Compatibility layer for other harnesses' global and project resources |

`~` is **not expanded** in paths; write an absolute or relative path instead.

Permission rules live under `[tools]`, with the semantics **deny first, then the most specific
ask/allow/sandbox**. Project-level rules may only tighten (a permissive rule is downgraded to
`ask`), and rejected entries show up in `security_warnings` (visible through `eix doctor`).

## 9. Extending

- **Skills**: directories holding `SKILL.md`, loaded by name, discoverable on demand through
 `tool_search`.
- **Commands**: store frequently used prompts as slash commands.
- **Agents**: custom prompts and tool sets for main agents and sub-agents.
- **MCP**: `[mcp.servers.*]`; remote servers support OAuth through `eix mcp auth` / `eix mcp logout`.
- **Plugins**: out-of-process JSON-RPC or in-process dylib. From scaffold to release:

```sh
eix plugin new my-plugin --python  # or --dylib
eix plugin run my-plugin   # run in the foreground and watch the handshake
eix plugin test my-plugin
eix plugin pack my-plugin  # generate eix-plugin.sums
eix plugin verify my-plugin
eix plugin install my-plugin
eix plugin verify-lock  # compare installed files against plugins.lock
```

Publisher signatures use `eix plugin trust <publisher> <base64-ed25519-key>`. Protocol details are
in [plugins.md](./plugins.md) and [plugin-dylib.md](./plugin-dylib.md).

## 10. Security model

| Layer | Mechanism |
| --- | --- |
| Mode | `plan` is read-only; `build` is the default; then `auto`; `full` is the loosest |
| Sandbox | `bwrap` restricts filesystem and network; `[tools] sandbox = "required"` enforces it |
| Risk assessment | Judged per call; low and medium-risk workspace patches apply directly |
| Approval | Per-hunk for high-risk patches; a snapshot precedes `exec_command` |
| Audit | Every tool decision is recorded |
| Rollback | `rewind` rolls back both conversation and files |

## 11. Development and troubleshooting

```sh
cargo build --release
cargo test --offline -p eix-core --lib
cargo test --offline -p eix-tui --bin eix  # note --bin eix, otherwise the tests do not compile
cargo test --offline -p eixd --lib
```

Sessions, tools, worktrees, MCP, and approvals all live in the daemon, so a TUI crash loses no
sessions; just reconnect. After an upgrade, a running daemon still holds the old binary, so exit
`eix` and reopen it (or restart with `eix daemon`) for the new build to take effect.

`eix doctor` is the first command to run: it checks config parsing, path reachability, and daemon
state together.
