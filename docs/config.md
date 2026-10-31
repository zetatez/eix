# Configuration

eix reads TOML. One file is enough to get started; everything else has a default. This page covers
how layers merge, what a project may change, and which keys belong to which module. Each module
also has a deep-dive page, linked from its section.

The annotated reference is [`config.tmpl.toml`](../config.tmpl.toml). Copy it to
`~/.config/eix/config.toml` and edit; every key in it is commented with its default.

## 1. Where the config is read from

Precedence, highest first:

| Layer | Path | Notes |
| --- | --- | --- |
| Explicit | `--config PATH` or `EIX_CONFIG` | Highest priority, and the project layer is **not merged**. Use it for reproducible setups |
| Global | `~/.config/eix/config.toml` (or `$EIX_CONFIG_DIR/config.toml`) | Your defaults; the only layer allowed to hold executable policy |
| Project | `.eix/config.toml` in each ancestor directory | Nearest directory wins; tighten-only |

Tables merge and arrays replace. Scalar values from a nearer layer win outright.

`{env:NAME}` and `{file:path}` placeholders are expanded in the global and explicit layers only.
A project file cannot read host files or the environment through them.

Root keys that are not tables (`formatter_defaults`) must appear **above** the first `[table]`,
otherwise TOML parses them into that table.

## 2. What a project layer may not do

A cloned repository is untrusted input, so the project layer is sanitized before merging. Each
rejected key produces a warning, visible through `eix doctor`.

Dropped entirely:

| Key | Why |
| --- | --- |
| `provider`, `providers`, `provider_runtime` | Chooses where prompts and keys go |
| `models`, `model_roles` | Same |
| `server` | Opens network listeners |
| `mcp`, `lsp`, `formatter`, `formatter_defaults` | Spawn processes |
| `notifications` | Runs an arbitrary command |
| `plugins` | Executes code |
| `integration`, `worktree` | Commits, rebases, merges, and pushes |
| `context.instructions_files` | Would let a repo point at arbitrary host files |
| `math.preamble` | Raw LaTeX injected into TeX input |
| `tools.sandbox` | Picks the isolation mode |
| `tools.sandbox_network` | Lets sandboxed commands reach the network |
| `tools.max_output_bytes`, `tools.command_timeout_secs` | Raises resource limits |
| `external_directory`, `sandbox_read_paths`, `sandbox_writable_roots`, `network_allow` | Widens filesystem or network reach |

Three keys are conditional rather than dropped outright, because tightening is always allowed:

| Key | Dropped when | Warning |
| --- | --- | --- |
| `tools.snapshots` | set to `false` | `project config cannot disable snapshots; ignored` |
| `tools.auto_apply_workspace_edits` | set to `true` | `project config cannot enable tools.auto_apply_workspace_edits; ignored` |
| `compations.<key>` | set to `true` | `project config cannot enable compations.<key>; forced false` |

A project may therefore turn snapshots on, turn auto-apply off, and turn a compatibility source
off, but never the reverse.

Permission rules from a project layer can only tighten. A permissive rule is downgraded to `ask`,
and rejected entries are listed in `security_warnings`, which `eix doctor` prints.

## 3. Providers

Prompt traffic goes to one provider: exactly one table sets `default = true`. The table key is the
prefix used in model ids, so `[providers.openai]` plus model `gpt-4o` is `openai/gpt-4o`.

```toml
[providers.openai]
default = true
name = "OpenAI"
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
```

| Key | Meaning |
| --- | --- |
| `default` | Marks the single default provider |
| `name` | Display name |
| `base_url` | Endpoint root |
| `model` | Default model for this provider |
| `kind` | `openai` (default), `anthropic`, `gemini`, `deepseek` |
| `api_key` | Literal key; keep it out of a shared config |
| `api_key_env` | Name of the environment variable holding the key |

Prefer `eix auth login <name>` over writing keys: the credential then lives in the data directory,
not the config. `api_key_env` is for CI and containers.

Native protocols (`anthropic`, `gemini`, `deepseek`) speak each vendor's own API and need
`max_output_tokens` in `[provider_runtime]`. The OpenAI-compatible `kind` covers OpenAI, OpenRouter,
Groq, xAI, Ollama, and anything else exposing `/v1`.

### OAuth providers

`[providers.<name>.oauth]` enables the browser flow used by `eix auth login <name>`:

| Key | Meaning |
| --- | --- |
| `authorization_endpoint` / `token_endpoint` | OAuth endpoints |
| `client_id` / `client_secret` | Client credentials; the secret is optional for public clients |
| `scopes` | Requested scopes |
| `redirect_uri` | Loopback callback; optional |
| `extra_authorize_params` / `extra_token_params` | Extra query or form fields |

## 4. Runtime knobs

`[provider_runtime]` applies to every provider; it is not per-provider state.

| Key | Default | Meaning |
| --- | --- | --- |
| `timeout_secs` | `300` | Per-request wall clock |
| `retry_attempts` | `8` | Attempts for transient failures |
| `retry_base_ms` | `250` | Base backoff |
| `retry_max_secs` | `60` | Backoff ceiling |
| `retry_window_secs` | `600` | Keep auto-recovering for this long before surfacing the error |
| `max_output_tokens` | `8192` | Required by Anthropic and Gemini |
| `context_tokens` | unset | Default window when the model does not declare one |
| `system_prompt` | unset | Replaces the built-in system prompt |
| `prompt_profile` | `default` | `default` or `concise` |
| `persistent_mode` | unset | Schedules and heartbeats only |

## 5. Models and roles

`[[models]]` adds entries to the `/model` picker. The default provider's model is always present
and listed first.

| Key | Meaning |
| --- | --- |
| `id` | Selection id, `provider/model` |
| `provider` | Reuses that provider's endpoint and credential |
| `kind` | Overrides the provider's protocol for this entry |
| `model` / `name` | Wire name and display name |
| `variants` | Reasoning efforts offered in the picker |
| `input_cost` / `output_cost` | USD per 1M tokens, for cost reporting |
| `context_tokens` | Window size for budgeting |

`[model_roles]` pins a model per task:

| Key | Used by |
| --- | --- |
| `execute` | Normal turns and exploration sub-agents |
| `plan` | Plan mode; falls back to the session's `/model` pin, then to `execute` |

## 6. Tools, permissions, and the sandbox

`[tools]` holds both the execution knobs and the rule table. Full semantics (deny-first precedence,
specificity scoring, argv-prefix matching) are in
[Approvals, sandboxing, and permission rules](./permissions.md).

```toml
[tools]
auto_apply_workspace_edits = true
command_timeout_secs = 60
max_parallel_subagents = 32
sandbox = "auto"
sandbox_network = false
snapshots = true

[[tools.rules]]
id = "deny-rm"
tool = "exec_command"
command_regex = "^rm "
action = "deny"
```

| Key | Default | Meaning |
| --- | --- | --- |
| `auto_apply_workspace_edits` | `true` | Apply low and medium-risk writes without a per-edit prompt |
| `max_output_bytes` | see template | Cap on a single tool result |
| `command_timeout_secs` | `60` | Wall clock for one `exec_command` |
| `max_parallel_subagents` | `32` | One of 2, 4, 8, 16, 32, 64, 128, 256 |
| `sandbox` | `auto` | `auto` (bwrap when present), `required`, `off` |
| `sandbox_network` | `false` | Let sandboxed commands reach the network |
| `snapshots` | `true` | Rollback snapshots before mutating tools |
| `sandbox_read_paths` | empty | Read-only mounts, `~` supported; add toolchain caches here |
| `sandbox_writable_roots` | empty | Extra writable paths |
| `external_directory` | empty | Paths outside the workspace that tools may touch |
| `network_allow` | empty | Allowed hosts for web tools; empty means unrestricted |

Each `[[tools.rules]]` entry takes `id`, `tool`, and optionally `path`, `command`,
`command_regex`, `signature`, and `action` (`allow`, `ask`, `deny`, `sandbox`; default `ask`).

## 7. Context and compaction

`[context]` sizes the request package: budget, compaction thresholds, tool-output and image
estimates, and the instruction files. Keys and the trimming order are documented in
[Context and context management](./context.md).

The one key that matters for layering: `instructions_files` is global-only, because it names files
outside the repository.

## 8. Worktrees and integration

`[worktree]` decides whether new sessions get an isolated checkout; `[integration]` runs the
merge queue. Both are executable policy and therefore global-only.

```toml
[worktree]
auto = false
base = "auto"

[integration]
enabled = true
auto_commit = true
test = "auto"
```

Key meanings, the queue steps, and per-repository overrides under
`[integration.repos."<abs path>"]` are in [Worktrees](./worktree.md).

## 9. Language servers and formatters

`[lsp]` configures diagnostics. The 14 built-in servers need no configuration at all; a user entry
with the same name replaces the built-in one wholesale. Field-by-field detail, including the trap
of forgetting `extensions`, is in [LSP and language servers](./lsp.md).

Formatting runs after a write, at most one formatter per file:

```toml
formatter_defaults = true

[[formatter]]
command = "rustfmt --edition 2021"
extensions = ["rs"]

[[formatter]]
extensions = ["md"]
```

| Item | Meaning |
| --- | --- |
| `formatter_defaults` | Root key. When `true`, the built-in table fills in unmatched extensions |
| `command` | Shell command; the file path is appended |
| `extensions` | Extensions this entry owns, without the dot |
| empty `command` | Disables that extension, e.g. to stop formatting Markdown |

## 10. MCP servers

`[mcp.servers.<name>]` adds tools, resources, and prompts from local or remote MCP servers. stdio
servers spawn lazily. Fields, transports, tool-name filters, and OAuth are covered in
[MCP](./mcp.md).

## 11. Plugins

`[plugins.<name>]` registers an out-of-process JSON-RPC plugin or an in-process dylib. Only the
global config may add plugins, and every capability is requested by the manifest and granted here.
See [Plugins](./plugins.md) and [dylib plugins](./plugin-dylib.md).

## 12. Voice

`[voice]` is enabled by default and only records when asked. Engine selection, capture thresholds,
model placement, and the cloud credential path are in [Voice setup](./voice-setup.md); the RPC and
event contract is in [Voice input](./voice.md).

## 13. Interface: theme and keys

`[theme]` picks a preset and may override individual tokens:

```toml
[theme]
preset = "dark-github"
```

Tokens: `normal`, `comment`, `string`, `function`, `error`, `diff_add`, `diff_delete`, `panel`.
Anything you leave out keeps the preset's colour. The authoritative list of presets is the comment
block above `[theme]` in [`config.tmpl.toml`](../config.tmpl.toml); each one is named `<light|dark>-<scheme>`.

`[keys]` overrides bindings. A comma-separated value binds several keys, and leader chords are
written as `leader X`:

```toml
[keys]
leader = "Space"
voice = "F2"
new_session = "leader n"
```

Changing `leader` only changes the prefix; existing `leader X` bindings follow it. The accepted key
names are the ones shown under `[keys]` in [`config.tmpl.toml`](../config.tmpl.toml), which lists
every action the default keymap binds, grouped as global keys, session tabs, and `leader` chords.
A misspelled name is skipped without an error and the action keeps its default binding, so verify a
rebind by pressing the new key.

## 14. Notifications

`[notifications]` fires when a session needs attention:

```toml
[notifications]
bell = false
command = 'notify-send eix "$EIX_SESSION_NAME: $EIX_STATUS"'
cooldown_secs = 10
```

`command` runs with `EIX_SESSION_NAME` and `EIX_STATUS` in the environment and is rate-limited by
`cooldown_secs`. Because it executes a command, the project layer cannot set this table.

## 15. Server and remote access

`[server]` opens the TCP and HTTP listeners. Both are off when unset, and `token` becomes required
as soon as either address is set.

```toml
[server]
tcp = "127.0.0.1:7420"
http = "127.0.0.1:7421"
token = "$EIX_TCP_TOKEN"
```

Endpoints, pairing, and the SDK are documented in [HTTP API and SDK](./http-api.md). Keep the
listeners on loopback and reach them through an SSH tunnel.

## 16. Images and math

`[images]` bounds what a prompt may carry and how long blobs live:

| Key | Meaning |
| --- | --- |
| `max_prompt_images` | Images per prompt (default `4`) |
| `max_image_bytes` | Size cap per image |
| `retention_days` | Delete unreferenced blobs after this many days; `0` disables |
| `max_store_bytes` | Cap for the blob directory; `0` means no cap |

Referenced blobs are never removed by retention.

`[math]` is TUI-side rendering of display math; the daemon ignores it:

| Key | Meaning |
| --- | --- |
| `images` | Render through the terminal graphics protocol |
| `dpi` | Rasterisation DPI passed to `dvipng` or poppler |
| `max_source_bytes` | Reject longer formulas before invoking TeX |
| `min_scale` / `max_rows` | Fall back to ASCII instead of shrinking further |
| `preamble` | Extra LaTeX preamble; global-only |

## 17. Compatibility with other harnesses

`[compations]` opts in to reading another agent tool's global and project resources. All switches
default to `false`, and native `.eix` sources always load.

| Key | Reads |
| --- | --- |
| `claude_code` | `~/.claude` and `.claude` |
| `opencode` | `~/.config/opencode` and `.opencode` |
| `codex_cli` | `$CODEX_HOME` and `.codex` |
| `gemini_cli` | `~/.gemini` and `.gemini` |
| `pi` | `$PI_CODING_AGENT_DIR` and `.pi` |

The mapping per source is described in [Skills, commands, and agents](./skills.md). The layer is
read-only and never writes to those directories.

## 18. Checking a config

```console
$ eix doctor
```

`eix doctor` loads the merged config and prints the effective value of each module, which is how you
confirm an edit actually took effect:

```text
provider <name> · model <id>
api      <base_url>
key      present | absent
sandbox  auto (bwrap available) · snapshot on · rules 4 · network blocked
compations off (native .eix only)
context  <budget> tokens · AGENTS.md found · workspace summary on
daemon   eixd <version> pid <pid> protocol <n>
```

The last field of `key` is the useful one: `absent` usually means `api_key_env` names a variable
that is not exported in the environment the daemon inherited.

Every warning collected while loading is printed below those lines, prefixed with `warning`:
syntax errors, keys stripped from a project layer, permission rules downgraded to `ask`, and
`cannot load config` if a layer failed to parse.

The command reads the config from disk each time, so rerun it after editing instead of restarting
the daemon.

To try a config without touching your files:

```console
$ eix --config ./scratch.toml doctor
```

An explicit config replaces the whole merge chain: the global file and every project layer are
ignored, which makes it the right tool for reproducing a report.
