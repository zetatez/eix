# eix plugin system design (v2)

- Status: the v2 kernel, hooks, UI view tree, inter-plugin RPC, fine-grained allowlists,
  lifecycle management, pack/verify/lockfile, and the Python and TypeScript SDKs are implemented. WASM and a
  hosted runtime remain future milestones.
- Native dylib extensions: see [`plugin-dylib.md`](plugin-dylib.md). P1 has landed: the `DylibBackend`
  loader plus `describe`, `tool.call`, and `command.run`.
- Scope: `eixd` (host), `eix-core` (protocol, manifest, packaging), and `eix-tui` (UI rendering
  and CLI).
- Related code: `crates/eix-core/src/plugin.rs`, `crates/eix-core/src/plugin_package.rs`,
 `crates/eixd/src/plugin/`, `crates/eixd/src/server.rs`, `crates/eix-tui/src/tui/app/`, and
 `crates/eix-tui/src/main.rs`.

This document describes how the eix plugin system moved from v1 (sidecar tools plus prompt
commands) to v2 (contribution points, hooks, UI, and fine-grained authorization): architecture,
protocol, security boundaries, and implementation status. The v1 protocol and manifest are gone,
with no compatibility layer.

## 0. Current implementation status

Implemented:

| Capability | Implementation |
| --- | --- |
| Manifest | `schema = 2`, `api_version = 2`, `[components.server]` / `[components.ui]` / `[components.dylib]`, `abi`, `[[contributes.commands]]`, `[package]` |
| Native dylib | `ExtensionBackend` plus `DylibBackend`; `describe`, `tool.call`, `command.run`, `hook.run`, and `rpc.call` are wired in and share the registry with process plugins |
| Protocol | JSON-RPC 2.0 over NDJSON; `initialize`, `tools/call`, `commands/run`, `hooks/run`, `rpc.call`, host reverse requests, notifications |
| Multiplexing | Writer task, reader task, one oneshot per request; `$/cancel` on timeout |
| Registry | tools, commands, events, hooks, ui, rpc; namespace and capability checks at startup |
| Lifecycle | `disabled` / `starting` / `ready` / `degraded` / `failed` / `stopped`; three consecutive backend failures enter `degraded` and one success restores `ready`; `lazy` startup, backoff restart, and config-diff reload |
| Host API | `host.log/info/ping`, `storage.*`, `fs.*`, `net.request`, `secrets.get`, `session.*`, `ui.*`, `notify`, `rpc.call`, `blob.*`, `job.*` |
| Hooks | `tool.before` / `tool.after` / `prompt.build` / `turn.stop` / `turn.end`, ordered by priority then plugin name |
| Action commands | `action` in a manifest or runtime `commands`; the host calls `commands/run`, which may return a prompt or text output |
| Declarative plugins | A package containing only `[[contributes.commands]]` never starts a process |
| Tool results | Content blocks (text/image/resource/diff/json) plus `structured` and `is_error` |
| Settings | Manifest `settings = "schema.json"` must be in-package, `type: object`, and at most 256 KiB; `plugin.settings.set` validates, writes back, and notifies the plugin |
| UI view tree | `ui/render`, `ui/action`, `ui/pick`; `UiNode` supports text/markdown/heading/divider/list/table/form/actions/picker/group; the TUI renders into a text overlay and prints a `/plugins act` hint |
| Observability | 500-line ring buffer per plugin, `plugin.logs {name, since, limit}`, `plugin.logs.clear`, `plugin.doctor`; TUI `/plugins logs|doctor` and a statusline health marker |
| Distribution | `eix plugin pack/verify/install/update/uninstall/trust/untrust`; detached Ed25519 plus SHA-256 checksums, `plugins.lock`, `require_signature`, `min_host_version`, `target`/`artifact` |
| SDKs | `eix-plugin-sdk` (Rust), `sdks/python`, `sdks/typescript`; `sdks/conformance/run.py` runs the protocol conformance suite |
| Example | `examples/plugins/echo/` has been migrated to v2 and demonstrates an action command |

Not yet implemented:

- Host API coverage for dylib plugins is limited to the synchronous-safe surface
 (`log/info/ping/notify/storage.*/secrets.get/fs.*/session.*/blob.*`); `net`, `ui`, `rpc`, and `jobs` still return
 `EIX_ERR_DENIED`. See [`plugin-dylib.md`](plugin-dylib.md).
- Editing settings inside the TUI; `plugin.settings.set` works, but the TUI only displays values.
- A registry, automatic upgrades, and the WASM or hosted runtime.

---

## 1. Background

### 1.1 The old system (removed)

v1 used `schema = 1`, `PLUGIN_API_VERSION = 1`, a top-level `[server]` table, and tool results of the form
`{ok, output: String}`. Those fields and the old JSON shapes have been deleted from the code; the
repository accepts schema 2 / API v2 only, and an old manifest is rejected at load time.

### 1.2 Problems v2 solves

Solved in this kernel:

1. Weak tool-result expressiveness: now content blocks, `structured`, and `is_error`.
2. Serial single-request handling: now writer/reader tasks plus a oneshot per request, multiplexed
  by `id`.
3. Unreliable, unconfigurable events: subscriptions are now filtered by the event patterns from
 `initialize`, though a full queue is still best effort.

Solved in later milestones:

4. Hooks: `tool.before` / `tool.after` / `prompt.build` / `turn.stop` / `turn.end` are wired into the turn
  and tool pipelines.
5. UI contributions: `panel` / `status` / `picker`, `host.notify`, the `UiNode` view tree, and
 `ui.render` / `ui.action` / `ui.pick` are wired into the TUI.
6. Packaging: `pack` / `verify` / `install` / `update` / `uninstall`, SHA-256 checksums, Ed25519 signatures, a local
  trust store, and `plugins.lock`.
7. Settings schema: a manifest can declare a JSON Schema, and `/plugins <name>` shows the schema
  and current values.
8. Capability granularity: filesystem access is limited to the workspace, network and RPC use
  allowlists, and secrets are authorized by environment variable name.
9. Lifecycle: `lazy`, backoff restart, config-diff reload, ring-buffer logs, and `plugin.doctor`.
10. Security: `require_signature`, `min_host_version`, platform `target`/`artifact`, and an opt-in Linux
   sandbox.
11. SDKs: Rust `eix-plugin-sdk`, the Python and TypeScript SDKs, and a shared conformance harness.

Still open:

12. A registry, automatic upgrades, and a persistent `plugin_audit` table.
13. A WASM or hosted runtime; in-TUI settings editing.
14. Project-level plugins are still forbidden outright; multi-workspace semantics are incomplete.

---

## 2. Goals and non-goals

### 2.1 Goals

- Plugins can extend model tools, commands, prompt and context handling, approvals, file writes,
  UI, themes, keybindings, agents, skills, MCP, providers, and background jobs.
- Declarative plugins work with zero code; reach for a child process only when logic is needed.
- Every extension point shares one manifest, one authorization model, and one registry.
- A plugin that crashes, hangs, or times out never blocks a turn or takes down the daemon.
- Permissions deny by default, and grants can be narrowed to a path, domain, secret name, or hook
  point.
- Plugin authors get a stable protocol, SDKs, and a conformance test.

### 2.2 Non-goals

- No in-process `dylib` plugins in the default path. The ABI is unstable, a crash takes the daemon
  down, and it requires `unsafe`. (dylib support exists but is opt-in and fully trusted; see
  [`plugin-dylib.md`](plugin-dylib.md).)
- Plugins never emit ANSI directly or drive the terminal.
- Plugins cannot bypass approvals, approve tool calls on the user's behalf, or silently rewrite user
  messages.
- A child process is not treated as an OS sandbox. The capability model constrains the host API,
  not operating-system permissions.

### 2.3 Design principles

1. Stable kernel, flexible contributions: the kernel knows contribution points and capabilities,
  never individual plugins.
2. Deny by default: an unauthorised contribution fails at startup instead of degrading silently.
3. No silent privilege escalation: a hook may veto, annotate, or request re-approval, but never
  approve on the host's behalf.
4. Host-rendered UI: plugins submit a view model; the host owns layout, theme, keys, and safety.
5. Declarative first: commands, skills, agents, themes, and MCP need no code.
6. Failure isolation and determinism: hooks have explicit ordering, timeouts, and failure policy.
7. Zero cost when absent: with no plugin subscribed, hot paths do not clone, lock, or spawn.
8. Forward compatible: an unknown host method fails as `unsupported`; an unknown contribution point
  is never executed silently.

---

## 3. Architecture

```text
eixd
   turn / tool / prompt / approval pipeline
   ExtensionRegistry
    tools · commands · hooks · events · ui · themes · rpc · jobs
     builtin contributions
     declarative package contributions
     service / ui plugin contributions
   CapabilityGateway
    fs · network · secrets · storage · rpc · ui
   PluginHost (one process per plugin, JSON-RPC, multiplexed)
   DylibHost (in-process cdylib, ABI v1 dispatch)
```

### 3.1 Four plugin forms

| Form | Needs a process | Good for | Capabilities |
| --- | --- | --- | --- |
| Service | yes | Tools, hooks, events, RPC, network, files, secrets, background work | Host API |
| Native (dylib) | no (loaded into `eixd`) | Performance-sensitive tools and commands, first-party deep extensions | ABI v1 plus the synchronous host API |
| UI | yes | Text panels, status, pickers, notifications, view trees | Declarative surfaces plus `UiNode` |
| Declarative | no | Commands, skills, agents, themes, keybindings, MCP servers, permission rules | Static contributions |

A single package may contain declarative, server/dylib, and UI contributions, all sharing one
manifest and one authorization. `components.server` and `components.dylib` are mutually exclusive.
This version never starts a separate process for `[components.ui]`; UI text surfaces come from the
server component's `initialize.ui` and `host.ui.*`.

A dylib is **fully trusted code**: it runs in the daemon's address space, a crash takes the daemon
with it, and the capability system only constrains `host.*` calls, not direct `libc` use. See
[`plugin-dylib.md`](plugin-dylib.md).

### 3.2 One registry

The registry is strongly typed and indexed by kind. Built-in features and plugins are treated
identically, which is what makes the dylib backend feel native: same registry, same permission
checks, same hook routing, same UI model.

---

## 4. Packages and manifests

### 4.1 Directory layout

A package is a directory containing `eix-plugin.toml`, an optional settings schema, and either a
server entry point or a dylib:

```text
plugins/acme/
   eix-plugin.toml
   settings.schema.json
   libacme.so
```

### 4.2 Manifest v2

```toml
schema = 2
name = "acme"
version = "2.1.0"
description = "Issue cache integration"
api_version = 2
capabilities = ["tools", "commands", "hooks", "storage"]

# Optional: a settings form schema; must be an in-package relative path, a JSON object, <=256 KiB.
# settings = "settings.schema.json"

[components.server]
command = "python3"
args = ["server.py"]

# A native dylib and components.server are mutually exclusive; pick one.
# abi = 1
# [components.dylib]
# path = "libacme.so"

# UI components are packaging declarations only for now; text surfaces come from the server
# component's initialize.ui.
# [components.ui]
# entry = "ui.json"

# Optional: distribution metadata. `eix plugin pack` generates checksums, and the signature covers it.
# [package]
# publisher = "acme"
# signature = "eix-plugin.sig"
# checksums = "eix-plugin.sums"

[[contributes.commands]]
name = "sync"
description = "Sync the issue cache"
args = "[--full]"
# action commands need a server component; the frontend calls plugin.command.run.
action = "sync"
```

### 4.3 Compatibility policy

- Only `schema = 2` is accepted; a v1 manifest without `schema` or `api_version` is refused.
- Only `[components.server]`, `[components.ui]`, and `[components.dylib]` are recognised; a top-level
  `[server]` is not.
- `components.dylib` and `components.server` are mutually exclusive; a dylib package declares `abi`
  and defaults to 1 when omitted.
- `[components.ui]` is a packaging declaration only; without a server component no UI process is
  started.
- Unknown fields are ignored by serde; unknown contribution points are not parsed.
- A purely declarative package (only `contributes.commands`) may omit server, UI, and dylib
  components. Action commands are the exception and require a server or dylib.

### 4.4 Migrating from v1

| v1 | v2 |
| --- | --- |
| Top-level `[server]` | `[components.server]` |
| No `schema` / `api_version = 1` | `schema = 2` / `api_version = 2` |
| `{id, result}` / `{id, error: {code: "..."}}` | Add `"jsonrpc": "2.0"`; error codes are integers |
| `tools/call` returning `{ok, output}` | Returns `{ok, content: [{"type":"text","text":"..."}], is_error?}` |
| Commands only from the handshake | Also statically declared in `[[contributes.commands]]` |
| Serial requests | Responses are correlated by `id` and may be concurrent |

---

## 5. Contribution points

| Point | Form | Capability | Notes |
| --- | --- | --- | --- |
| `tools` | Service | `tools` | Model tools, exposed as `plugin__<plugin>__<tool>` |
| `commands` | Declarative/service | `commands` | Prompt templates and action commands |
| `hooks` | Service | `hooks` | Pipeline interception, see section 8 |
| `events` | Service | `events` | Read-only notification subscription |
| `ui.views` | Service | `ui` | Custom panes and overlays |
| `ui.pickers` | Service/declarative | `ui` | Providers for the unified picker |
| `ui.status` | Service | `ui` | Statusline fragments |
| `ui.settings` | Declarative | — | Form generated from JSON Schema |
| `themes` | Declarative | — | Colour tokens; built-in names cannot be overridden |
| `keymaps` | Declarative | — | Namespaced actions `plugin.<name>.*` |
| `agents` | Declarative | `agents` | Prompt, tool set, model preference |
| `skills` | Declarative | `skills` | `SKILL.md` |
| `mcp_servers` | Declarative | — | Reuses the existing MCP client |
| `providers` | Declarative/service | `network`/`secrets` | Declarative OpenAI-compatible first, adapters later |
| `jobs` | Service | `jobs` | Scheduled and background work |
| `rpc` | Service | `rpc` | RPC methods exposed to other plugins |

This version implements `tools`, `commands`, `events`, `hooks`, text `ui`, and `rpc`. The declarations for
`themes`, `keymaps`, `agents`, `skills`, `mcp_servers`, `providers`, and `jobs` remain design-only; `jobs`
are registered through the host API rather than declared statically.

### 5.1 Naming and conflicts

- Tools: `plugin__<plugin>__<tool>`; a tool name may not contain `__`.
- Commands: `plugin/<plugin>/<command>`; built-in and user Markdown commands win.
- Keybindings: `plugin.<plugin>.<action>`; user configuration wins over plugins.
- Themes: may reference or add tokens but cannot override a built-in theme name.

### 5.2 Action commands

A command can expand a prompt, or declare an action that the plugin server handles at runtime:

```toml
[[contributes.commands]]
name = "sync"
description = "Sync the issue cache"
args = "[--full]"
action = "sync"
```

Runtime commands may carry `action` too:

```json
{
  "name": "sync",
  "description": "Sync the issue cache",
  "args": "[--full]",
  "action": "sync"
}
```

- Exactly one of `prompt` and `action` must be set; an action needs a server or dylib component.
- The TUI `/` menu and command palette can trigger them; the host calls the plugin's
 `commands/run`.
- `commands/run` parameters: `{command, arguments, session_id, workspace, cwd}`.
- Return value: `{ok, prompt?, output?}`. `prompt` is submitted as a user message; `output` is shown
  in a text panel.
- No arbitrary shell is exposed; a plugin that needs to run commands goes through an authorized host
  API or the tool approval path.

---

## 6. Authorization and scope

### 6.1 Configuration

```toml
[plugins.acme]
enabled = true
manifest = "plugins/acme/eix-plugin.toml"
timeout_secs = 30
capabilities = ["tools", "commands", "hooks", "network", "secrets", "storage"]
allowed_tools = ["search_*"]
denied_tools = ["delete_*"]

# Hostnames, or one leading `*.` wildcard; empty denies every request.
network_allow = ["api.acme.com", "*.acme-cdn.com"]
# Environment variable names that may be read; empty denies every secret.
secrets = ["ACME_TOKEN"]
# Other plugins this one may call; empty denies all RPC, "*" allows everything.
rpc_allow = ["issue-cache"]

[plugins.acme.settings]
project_key = "ACME"
```

A native dylib plugin uses a library entry point instead; the authorization fields are identical:

```toml
[plugins.acme]
enabled = true
library = "plugins/acme/libacme.so"
timeout_secs = 30
capabilities = ["tools", "commands"]
allowed_tools = ["search_*"]
```

`library` cannot appear together with `command`; a `[components.dylib]` table in the manifest is
equivalent to configuring `library`.

### 6.2 Rules

- The manifest's `capabilities` are a request; the config's `capabilities` are the grant. Config can
  only narrow the request, never widen it.
- A capability is a switch; an allowlist is a scope. With no `network_allow`, `network` denies
  everything; with no `secrets` list, secrets are denied; with no `rpc_allow`, RPC is denied.
- Network access is `https` only, matched by exact domain or a single `*.` suffix. Redirects are not
  followed and credentials in URLs are rejected.
- Secrets are read only from the plugin's `env` or the process environment; values never enter logs
  or the model context.
- Filesystem access is limited to the workspace, rejecting `..`, absolute escapes, and symlinks;
  a single file is capped at 1 MiB.
- An RPC target must be in `rpc_allow`, must export the method, and the call chain is capped at depth
  8 with a 60 s timeout.
- `tools.read_only` takes effect only with explicit trust; otherwise plugin tools follow the normal
  approval path as writable tools.
- The project layer cannot authorize plugins by default; plugins come from the trusted global config
  only.
- When several workspaces share one plugin process, host methods are validated against the workspace
  of the call.

---

## 7. Protocol v2

### 7.1 Transport

NDJSON over stdio, with explicit constraints:

- stdout carries protocol frames only; logs go to stderr or `host.log`.
- A single frame is capped at 8 MiB; exceeding it drops the connection and marks the plugin failed.
- Large binaries are not inlined; `host.blob.put/get` returns a handle.
- `Content-Length` framing may be negotiated later and is not a first-version requirement.

### 7.2 Handshake

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol_version": 2,
  "api_version": 2,
  "plugin": {"name":"acme","version":"2.1.0"},
  "host": {"name":"eixd","version":"0.9.0"},
  "features": [
    "content_blocks","cancel","hooks","ui","rpc","jobs",
    "fs","net","secrets","session_read","blobs","command_actions"
  ],
  "capabilities": ["tools","commands","hooks","events","storage"],
  "workspace": "/repo",
  "config": {"data_dir":"...","settings":{"project_key":"ACME"}}
}}
```

```json
{"jsonrpc":"2.0","id":1,"result":{
  "name":"acme","version":"2.1.0","api_version":2,
  "features":["content_blocks","cancel","hooks"],
  "tools":[],
  "commands":[{"name":"sync","action":"sync"}],
  "events":["session.*"],
  "hooks":[{"point":"tool.before","priority":10,"on_error":"deny"}],
  "ui":[{"surface":"panel","id":"issues","title":"Issues"}],
  "rpc":[{"name":"issues.search","description":"Search cached issues"}]
}}
```

- `features` is negotiated: the host calls only extension methods both sides support.
- `api_version` is an integer and must match the host exactly; a mismatch refuses startup.
- A non-empty `hooks` needs the `hooks` capability, a non-empty `ui` needs `ui`, and a non-empty
  `rpc` needs `rpc`; otherwise startup fails.
- Event patterns use `*` / `?` wildcards, for example `session.*`.

### 7.3 Multiplexing

```text
writer task <-- mpsc<Frame>
reader task --> BTreeMap<id, oneshot::Sender<Response>>
dispatcher  --> one task per in-flight request, max_inflight defaults to 8
```

Effects:

- Tool calls and host callbacks run concurrently.
- `$/cancel` is supported; the host sends it after a timeout.
- `$/progress` is reserved in the protocol but not consumed by the host yet.
- Timeouts apply per request, not per plugin.

### 7.4 Tool results

```json
{"ok": true,
 "content": [
   {"type":"text","text":"found 3 issues"},
   {"type":"image","mime":"image/png","blob":"blob_123"},
   {"type":"diff","path":"src/lib.rs","patch":"..."},
   {"type":"resource","uri":"acme://issue/42","title":"Issue 42"}
 ],
 "structured": {"issues":[]},
 "is_error": false}
```

Limits:

- Tool output is truncated per `tools.max_output_bytes`, with a 1 MiB ceiling on the plugin side;
  per-item caps and an image MIME allowlist remain future work.
- `structured` is currently used only as a text fallback when `content` is empty.
- The TUI renders text only: image, resource, diff, and json become short placeholder text.
- The v1 `output: String` field is gone and is no longer wrapped automatically.

### 7.5 Host API

A single `host.*` namespace, with a capability check before every method:

| Method | Capability | Notes |
| --- | --- | --- |
| `host.log` | — | Writes to stderr; supports both requests and notifications |
| `host.info` | — | Returns plugin name, API version, capabilities, workspace, and allowlists |
| `host.ping` | — | Returns `{"pong":true}` |
| `host.storage.get/set/delete/list` | `storage` | Reuses the KV store, partitioned by global/workspace; key <=512 B, value <=1 MB |
| `host.fs.read` / `list` | `workspace_read` | Workspace only; rejects `..`, escapes, and symlinks; single file <=1 MiB |
| `host.fs.write` | `workspace_write` | Same rules; the parent directory must exist |
| `host.net.request` | `network` | `https` only, domain allowlist, no redirects or URL credentials; request <=1 MiB, response <=4 MiB |
| `host.secrets.get` | `secrets` | Reads only the environment variables listed in `plugins.<name>.secrets` |
| `host.session.get` / `list` | `sessions_read` | Read-only session metadata |
| `host.notify` | `ui` | Emits `plugin.notify`; the TUI shows a toast |
| `host.ui.open` / `update` / `close` | `ui` | Panel and status events; may carry `body` text or a `node` view tree, body <=256 KiB |
| `host.rpc.call` | `rpc` | Calls a method exported by another plugin in the allowlist |
| `host.blob.put` / `get` | `storage` | Content-addressed (SHA-256) in the plugin data directory, <=8 MiB each |
| `host.job.register/trigger/unregister` | `jobs` | At most 32 jobs, interval 1..=86400 s, delivered as a `job.run` notification |

Error codes are shared: `-32601 method_not_found`, `-32602 invalid_params`, `-32001 unauthorized`. Timeouts and
cancellation are handled by the client layer, with `-32003 timeout` / `-32004 cancelled` reserved in the
protocol.

### 7.6 Inter-plugin RPC

```text
plugin A -> host.rpc.call {plugin:"acme", method:"issues.search", arguments:{...}}
host     -> plugin B: rpc.call {method, arguments, caller, depth}
plugin B -> host: result
host     -> plugin A: result
```

- The caller needs the `rpc` capability and the target must appear in
  `plugins.<caller>.rpc_allow`.
- The target must export the method in `initialize.rpc`; method names live in the plugin's own
  namespace.
- Call depth is capped at 8 and each call times out after 60 s.
- A plugin that calls further must pass the received `depth` through to `host.rpc.call` unchanged
  (the Rust SDK offers `Ctx::rpc_call_with_depth`).
- Both the process and dylib backends implement `rpc.call`; on the dylib side `Plugin::rpc` handles
 `RpcCall { method, arguments, caller, depth }`.
- Each call logs caller, callee, and the failure reason to the daemon log, without arguments.

---

## 8. Hooks

Events are read-only notifications; hooks are requests that participate in a decision. The two are
never mixed.

### 8.1 Hook points

| Hook | When | Can do | Failure policy |
| --- | --- | --- | --- |
| `tool.before` | After normal permission checks, before approval and execution | Return `deny` to veto the tool; cannot approve | `on_error` = `allow` / `deny` / `ignore` |
| `tool.after` | After the tool ran, before the result is recorded | Replace the tool output with `output` | Log the error and continue |
| `prompt.build` | After the user submits, before the event is written | Replace the user message with `prompt` | Log the error and continue |
| `turn.stop` | After the model's final answer, before the turn completes | Return `block` to prevent completion; `reason` becomes a reminder for the next round | `on_error` = `allow` / `deny` / `ignore`; `deny` counts as blocking |
| `turn.end` | After the turn succeeds or fails | Read-only notification, sent as a `hooks/run` request | Log the error and continue |

Hooks are declared in `initialize.hooks`, and a non-empty declaration requires the `hooks`
capability:

```json
{"point":"tool.before","priority":10,"timeout_ms":500,"on_error":"deny"}
```

`timeout_ms` of 0 means the plugin's request timeout; the maximum is 300000 ms.

### 8.2 Return values

```json
{"action":"deny","reason":"missing issue id"}
{"deny":true,"reason":"generated file"}
{"output":"redacted tool output"}
{"prompt":"rewritten user prompt"}
{"block":true,"reason":"tests are still failing"}
```

Rules:

- For `tool.before`, `deny` / `action:"deny"` / `decision:"deny"` / `allow: false` all mean veto;
  `reason` is surfaced in the tool result.
- For `tool.after`, `output` replaces the current output and is subject to the plugin output cap.
- For `prompt.build`, an empty or oversized `prompt` is ignored; every other hook sees the previous
  hook's result.
- For `turn.stop`, `block` / `action:"block"` / `decision:"block"` prevents the turn from ending, and
  `reason` is injected as a system reminder into the next round. A turn can be blocked at most
  twice, so a plugin cannot hang it forever. Sub-agent turns do not run this hook.
- For `turn.end`, the return value is ignored; hooks cannot change the outcome.

### 8.3 Ordering, timeouts, and concurrency

- Ordering: serial, ascending by `(priority, plugin_name)`.
- Timeouts: each hook sets its own; on timeout the `on_error` policy applies, and a `tool.before`
  hook with `deny` vetoes the tool.
- Budget: there is no budget shared across hooks; each plugin is bounded by its request and hook
  timeouts.
- `turn.end` is dispatched on a background task and never blocks completion.
- `turn.stop` does block completion, so its timeout counts against the user's wait; keep it fast.
- A hook that triggers a tool call must not recurse into the same hook.

### 8.4 Approval authority

A hook may veto, annotate, and request re-approval, but it can never approve a tool call. Approval
remains the host's decision, made through the normal approval path with the user as the authority.

---

## 9. UI contributions

### 9.1 Implemented surfaces

The UI event payload is `{in, level, message}`, shown by the TUI as a toast. `body` is plain text or
an array of strings; `node` is an optional structured view tree. When both are present the TUI
renders `node` and falls back to `body` when no node renderer applies. A whole frame is capped at
256 KiB.

### 9.2 Surfaces

| Surface | Status | Notes |
| --- | --- | --- |
| `panel` | Implemented | Full-screen text overlay, closed with Esc |
| `status` | Implemented | Opens as a panel rather than living in the statusline |
| `picker` | Implemented | `ui/render` returns a `picker` node or declares a picker surface; `ui/pick` serves the search line and the TUI opens the plugin picker |
| `notification` | Implemented | `host.notify` toast |

`plugin.ui.list` returns `{surfaces: [{plugin, surface, id, title}]}`. The TUI `/plugins [name]`
shows declarations and supports:

```text
/plugins open <plugin> <id> [surface]     # calls plugin.ui.render
/plugins action <plugin> <id> <action> [k=v]...
/plugins act <action> [k=v]...            # acts on the current panel
/plugins pick <plugin> <id> [query]       # opens the ui/pick picker
```

The TUI renders the view tree into a text overlay; buttons and form submissions print the matching
`/plugins act <action>` hint, so the same tree is drivable from the keyboard, scripts, and tests.

### 9.3 View tree

```json
{
 "kind": "group",
 "title": "Issues",
 "children": [
  {"kind": "heading", "text": "Open", "level": 2},
  {"kind": "list", "items": ["#42 Fix parser", "#43 Add tests"]},
  {"kind": "actions", "actions": [
   {"id": "refresh", "label": "Refresh", "kind": "primary"}
  ]},
  {"kind": "picker", "id": "issues", "title": "Issues", "items": [
   {"id": "42", "title": "#42 Fix parser", "subtitle": "bug"}
  ]}
 ]
}
```

Nodes: `text`, `markdown`, `heading`, `divider`, `list`, `table`, `form`, `actions`, `picker`, `group`.
Limits: 2000 nodes, depth 16, 64 KiB of text per node, 64 columns and 1000 rows per table, 1000
picker items; an unknown `kind` fails deserialisation. `UiNode::validate()` runs daemon-side, so an
invalid tree never reaches the TUI.

`ui/render` returns `{title, node?, body}`. `ui/action` returns
`{ok, node?, body?, prompt?, close?, error?}`, where `prompt` is submitted as a user message and
`close` closes the panel. `ui/pick {id, query}` returns
`{items:[{id,title,subtitle}]}`; the TUI's plugin picker debounces queries by 200 ms and, on Enter,
sends the selection back as `ui/action {action:"picked", values:{item}}`.

The dylib backend does not implement `ui/render`, `ui/action`, or `ui/pick`; calls return unknown
method.

### 9.4 Settings form

A plugin declares `settings = "settings.schema.json"` in its manifest:

```json
{
 "type": "object",
 "properties": {
  "project_key": {"type": "string", "title": "Project key"},
  "auto_sync": {"type": "boolean", "default": true}
 },
 "required": ["project_key"]
}
```

- The schema must live inside the plugin directory, be `type: object`, and be at most 256 KiB.
- `/plugins <name>` calls `plugin.settings` and shows the schema, current values, types, required
  fields, and enum options.
- `plugin.settings.set {name, values}` shallow-merges (`null` deletes a key), validates against the
  schema, and writes back to the global or explicit config file; `toml_edit` preserves comments and
  unrelated keys.
- After a successful write, running plugins receive `config.changed` (via
 `Plugin::config_changed` on the dylib path), and the daemon's own write does not trigger a full
  `/reload`.
- There is no in-TUI editing yet; change values through `plugin.settings.set` or by editing
 `[plugins.<name>.settings]` directly.
- Settings travel to the plugin in `initialize`; `config.changed` makes new values visible
  immediately.

---

## 10. Security model

### 10.1 Trust tiers

| Tier | Default | Notes |
| --- | --- | --- |
| Built-in tools | Trusted | Existing risk classification |
| dylib plugins | Fully trusted | Run in the `eixd` address space; only signed or explicitly trusted libraries are loaded |
| Declarative plugins | Trusted but path-constrained | Static content only |
| Service plugins | Untrusted | Tools need approval by default; hooks cannot approve |
| UI plugins | Untrusted | Host-rendered, bounded nodes |
| Project-level plugins | Forbidden | Require explicit trust or a signature |

### 10.2 Capability boundaries

- `workspace_read` / `workspace_write`: the workspace root, rejecting `..`, absolute escapes, and
  symlinks; a single file is capped at 1 MiB.
- `network`: `https` only, domain allowlist, host-proxied requests, no redirects, no credentials in
  URLs.
- `secrets`: authorized by environment variable name; values never enter logs or model context.
- `tools.read_only`: effective only under explicit trust.
- `hooks`: authorized per hook point; a hook can never approve a tool.
- `rpc`: both caller and target are constrained by the allowlist and exported methods.
- `storage` / `blob`: isolated per plugin name; blobs live in the plugin's own data directory.
- `jobs`: at most 32, interval 1..=86400 s.

Capabilities constrain the host API, not the operating system. The plugin process still runs as the
daemon user and can read and write anything that user can, and reach the network directly. Strong
isolation requires the sandbox or running the daemon as a separate user or container.

This is especially true for dylib plugins: sharing an address space with the daemon, they can call
`libc::open`, open sockets, and read the environment, bypassing every `host.*` check. They must be
treated as fully trusted, and the boundary belongs before loading: signature, install path, ABI,
and platform checks.

### 10.3 Audit

A `plugin_audit` table is planned:

```text
id, ts, plugin, method, capability, session_id, workspace,
ok, duration_ms, detail_hash, error
```

It records host API calls, hook decisions, tool calls, and approval interactions. Arguments are
stored only as hashes or summaries, so secrets never reach the database. Until it lands, the
per-plugin ring buffer and `plugin.doctor` provide the audit trail.

### 10.4 Optional OS sandbox

```toml
[plugins.acme]
sandbox = true   # false by default; opt-in on Linux
require_signature = true
```

- Linux: `PR_SET_NO_NEW_PRIVS`, `PR_SET_PDEATHSIG`, rlimits, and a Landlock path allowlist covering
  the workspace, the plugin data directory, and system runtime paths.
- macOS and other platforms: a no-op. Process isolation remains, but no sandbox is pretended.
- The sandbox constrains reachable files and system calls; host API capability checks remain the
  primary boundary.
- `require_signature = true` refuses unsigned packages or packages without `[package]`. The default
  is false to keep local development easy.

### 10.5 Supply chain

- `eix plugin pack <dir>` writes `eix-plugin.sums`: one `<sha256>\t<relative/path>` line per file,
  covering everything except the checksums and signature files.
- `eix plugin verify <dir> [--allow-unsigned]` checks the sums and, when `eix-plugin.sig` exists,
  verifies the detached Ed25519 signature against the trust store.
- The signature covers the raw bytes of `eix-plugin.sums`; the default signature file is
 `eix-plugin.sig`, overridable in `[package] signature`.
- `eix plugin trust <publisher> <base64-public-key>` and `untrust` manage the global
 `plugin-trust.toml`.
- `eix plugin install <dir> [--force] [--allow-unsigned]` verifies first, copies into
 `~/.config/eix/plugins/<name>`, then writes `[plugins.<name>]` into the config.
- `--allow-unsigned` backfills `[package]` for a local directory and repacks it, which helps during
  development; production distribution should still be signed.
- `target` (aliases such as `linux-x86_64`, `x86_64-unknown-linux-gnu`, `amd64`) together with `artifact` and `path`
  lets one manifest point at the right platform library; `artifact` wins.
- `min_host_version` in the manifest is enforced when the daemon loads it; the host version is
  exposed through `PLUGIN_HOST_VERSION`.
- Installation refuses symlinks, path escapes, and extra files absent from the checksums.
- `~/.config/eix/plugins/plugins.lock` records version, publisher, and checksums of installed
  plugins; `eix plugin verify-lock` (library function `verify_lock`) detects drift and exits
  non-zero on failure.
- There is no registry and no automatic upgrade yet; `eix plugin update <dir>` re-verifies and
  overwrites the install, then writes the config back.

---

## 11. Lifecycle and runtime

### 11.1 State machine

```text
disabled -> installed -> starting -> ready
                  failed
                  degraded
            stopped
```

- `degraded`: three consecutive backend failures (tool, command, or hook). The plugin stays usable,
  one success returns it to `ready`, and `plugin.list` exposes the `failures` count and the last
  error.
- `failed`: startup failed or the plugin crashed; the error is kept and a restart is allowed.
- `stopped`: the user disabled it, or the daemon exited.

### 11.2 Startup and restart

- `lazy = true`: startup and reload only register the spec and state, without spawning; the first
  `plugin.start` or TUI `/plugins start <name>` starts it.
- A failed start marks the plugin `failed` and keeps `error`; `plugin.restart` and
 `/plugins restart` retry explicitly.
- Crash detection uses `exit_reason`; a 1 s supervisor notices the exit and restarts with 1/2/4/8/16 s
  backoff, up to six attempts, after which it enters `gave_up` and needs an explicit restart.
- `/reload` diffs the serialised `PluginConfig` fingerprint: only new or changed plugins restart, and
  an unchanged failed plugin is not retried by the reload.
- `plugin.stop/start/enable/disable/uninstall/forget` cover manual lifecycle. Uninstall removes only
  a managed install directory and requires the canonical path to sit inside the plugin root.

### 11.3 Concurrency and backpressure

- `max_inflight` defaults to 8 per plugin; excess requests queue or are rejected.
- Event delivery follows a per-subscriber policy: `drop` (observational), `coalesce` (UI state), or
 `queue` (bounded queue).
- Hooks always use request/response and never the event queue.
- Large output is truncated with head, tail, and a length notice.

### 11.4 Multiple workspaces

- By default one plugin process serves several workspaces, and every host call carries and validates
  a `workspace`.
- A plugin should partition state by workspace, or declare `per_workspace = true` to get one process
  per workspace.
- Hook and event payloads must include `workspace`, so nothing leaks across projects.

---

## 12. Data model and configuration

### 12.1 Rust types

```rust
pub struct PluginManifest {
  pub schema: u32,                    // only 2 is accepted
  pub name: String,
  pub version: String,
  pub description: String,
  pub api_version: u32,               // must equal PLUGIN_API_VERSION
  pub min_host_version: Option<String>,
  pub capabilities: Vec<PluginCapability>,
  pub components: PluginComponents,
  pub contributes: PluginContributions,
  pub settings: Option<PathBuf>,
  pub package: Option<PluginPackage>,
}

pub struct PluginPackage {
  pub publisher: Option<String>,
  pub signature: Option<PathBuf>,     // defaults to eix-plugin.sig
  pub checksums: Option<PathBuf>,     // defaults to eix-plugin.sums
}

pub struct PluginContributions {
  pub commands: Vec<ManifestCommand>,
}

pub struct ManifestCommand {
  pub name: String,
  pub description: String,
  pub args: String,
  pub prompt: String,                 // prompt or action, never both
  pub action: Option<String>,
}

pub struct PluginConfig {
  pub command: String,
  pub args: Vec<String>,
  pub library: Option<PathBuf>,       // native dylib; exclusive with command
  pub require_signature: bool,        // refuse unsigned packages or packages without [package]
  pub sandbox: bool,                  // opt-in process hardening on Linux
  pub env: BTreeMap<String, String>,
  pub enabled: bool,
  pub lazy: bool,                     // defer to plugin.start
  pub timeout_secs: u64,
  pub cwd: Option<PathBuf>,
  pub manifest: Option<PathBuf>,
  pub capabilities: Vec<PluginCapability>,
  pub allowed_tools: Vec<String>,
  pub denied_tools: Vec<String>,
  pub settings: BTreeMap<String, serde_json::Value>,
  pub network_allow: Vec<String>,
  pub secrets: Vec<String>,
  pub rpc_allow: Vec<String>,
}
```

The handshake's `hooks`, `ui`, and `rpc` use the strongly typed `PluginHook`, `PluginUi`, and
`PluginRpc`. The manifest only declares capabilities; the actual hook points, UI surfaces, and RPC
methods come back from `initialize`.

### 12.2 Configuration

`[plugins.<name>]` supports:

- `command` / `args` / `env` / `cwd`: the process entry point; use it alone or together with `manifest`.
- `library`: the native dylib path, resolved relative to the config cwd and exclusive with
  `command` / `components.server`. A dylib must also be granted the capabilities it actually uses, such
  as `tools` and `commands`.
- `enabled` / `timeout_secs`: enablement and request timeout in seconds.
- `lazy`: defer startup until the first `plugin.start` or `/plugins start`.
- `require_signature`: refuse unsigned packages or packages without `[package]`; default false.
- `sandbox`: enable no_new_privs, PDEATHSIG, rlimits, and Landlock on Linux; a no-op elsewhere.
  Default false.
- `manifest`: `eix-plugin.toml`, resolved relative to the config cwd; its `name` must match the
  config key. A manifest with `components.dylib` is equivalent to configuring `library`.
- `capabilities`: can only narrow the manifest's request; empty means adopt the request or
 `[tools, commands]`.
- `allowed_tools` / `denied_tools`: `*` and `?` wildcards; deny wins.
- `settings`: arbitrary JSON values, passed to the plugin in `initialize`.
- `network_allow`: exact domains or a single leading `*.` wildcard; an empty list denies all network
  requests.
- `secrets`: readable environment variable names; an empty list denies every secret.
- `rpc_allow`: callable plugin names; `"*"` allows all, and an empty list denies all RPC.

Project-level configuration still cannot add or modify plugins.

### 12.3 Database

- `plugin_kv`: used for storage, isolated per plugin name and global/workspace scope.
- `plugin_audit` and `plugin_installs`: planned, not implemented; observability currently uses the
  per-plugin ring buffer and `plugin.doctor`.
- Install state lives in `plugins/plugins.lock` (TOML), not in SQLite.
- Secrets are read only from the configured environment variables and never stored in SQLite.

---

## 13. Observability and developer experience

### 13.1 Operations

- Each plugin keeps the last 500 lines in an in-memory ring buffer; `plugin.logs {name?, since?,
  limit?}` pages by monotonic `seq` and never touches the database. `plugin.logs.clear` empties
  it, and `plugin.doctor` summarises state, the last error, the log tail, and lockfile drift.
- Logs are redacted before they are written, and `error_count` plus `last` are tracked. Daemon
  lifecycle messages, stderr, and `host.log` all land in the same buffer.
- `plugin.list` returns state, tools, commands, hooks, ui, rpc, events, capabilities, failures, error,
  and lazy.
- `plugin.settings {name}` returns the schema and current values; `plugin.settings.set {name, values}`
  validates and writes the config back.
- `plugin.ui.list` returns the declared `{plugin, surface, id, title}` surfaces, so the TUI knows the
  panels and pickers in advance.
- TUI `/plugins [name]` lists contributions, errors, and the settings schema; `/plugins logs [name]`,
 `/plugins doctor [name]`, `/plugins restart|start|stop|enable|disable|uninstall <name>`, and
 `/plugins set <name> <key>=<json>...` cover day-to-day operations.
- The TUI also supports `/plugins open|action|act|pick` to drive view trees and the plugin picker.
- `/reload` restarts only plugins whose config fingerprint changed; the statusline marks failed and
  degraded plugins and silently refreshes health after startup and every plugin action.

### 13.2 Development

- `eix plugin new <name> [--python|--dylib]` scaffolds a Python plugin or a dylib manifest;
 `eix plugin run` runs it in the foreground, and `eix plugin test` performs a 10 s initialize
  handshake check.
- `eix plugin pack/verify/install/update/uninstall/verify-lock/trust/untrust` are implemented;
  install and update write `[plugins.<name>]` and `plugins.lock` directly.
- SDKs: `crates/eix-plugin-sdk` (Rust), `sdks/python/eix_plugin.py`, and `sdks/typescript/`
  (ESM plus `.d.ts`). Note that `eix-sdk` is the daemon HTTP client, not a plugin SDK.
- Conformance: `sdks/conformance/run.py` starts a plugin and covers initialize, tools/call,
  commands/run, hooks/run, ui/render, and shutdown. The Python and TypeScript fixtures both have
  entry points; see each SDK README.
- Documentation: this file plus the type definitions in `crates/eix-core/src/plugin.rs`.
- Examples: `examples/plugins/echo/` covers tools, commands, actions, events, and storage;
 `sdks/*/examples/` covers the SDKs.

---

## 14. Compatibility and versioning

1. `PLUGIN_API_VERSION = 2` is an integer, and both sides of the handshake must match exactly.
2. Each contribution point may later carry its own schema version, with a separate `ui_schema` for
  the view tree.
3. v1 manifests and the old JSON protocol are removed with no compatibility layer; migrate by
  rewriting the manifest and responses per sections 4 and 7.
4. Unknown contribution points and unknown host methods fail explicitly or report `unsupported`;
  there is no silent degradation.
5. Future upgrades: document the change, keep it in a warning period, then bump
 `PLUGIN_API_VERSION`. Different majors never run mixed.

---

## 15. Roadmap

### Phase 0: kernel refactor (done)

- Split `PluginManager` into `mod`, `client`, and `host_api`.
- Unify the tools, commands, and events registries with startup validation.
- Add state enums, declarative commands, tool-name filtering, and event subscriptions.
- Acceptance: `cargo test -p eixd plugin` and the plugin e2e suite cover the new protocol.

### Phase 1: protocol v2 and SDKs (done)

- JSON-RPC 2.0 frames, multiplexing, cancellation, and content blocks have landed.
- The Python and TypeScript SDKs plus the shared conformance harness have landed; Rust uses
 `eix-plugin-sdk`.
- Acceptance: a slow tool does not block a second call on the same plugin, and cancellation works.

### Phase 2: hooks, declarative plugins, action commands (core done)

- Hook points, ordering, timeouts, and failure policy have landed; the audit table has not.
- `commands` supports actions.
- Declarative themes, keymaps, agents, skills, and mcp_servers are not implemented.
- The settings schema is parsed, and `plugin.settings.set` validates, writes back, and hot-reloads
  through `config.changed`; the TUI still displays values read-only.
- Three consecutive failures enter `degraded`, and one success restores `ready`.

### Phase 3: UI contributions (done)

- `panel`, `status`, `notification`, and `picker` have landed; the `UiNode` view tree,
 `ui.render`, `ui.action`, and `ui.pick` are implemented.
- The TUI renders nodes into a text overlay and offers `/plugins open|action|act|pick`; the plugin
  picker debounces queries by 200 ms.
- The `/plugins [name]` manager has landed and shows the UI surfaces a plugin declares.
- Acceptance: a plugin can open, update, and close panels and pickers, and JSON is never executed as
  ANSI.

### Phase 4: security and distribution (done, except the registry)

- Filesystem, network, secrets, and RPC allowlists have landed.
- `require_signature`, `min_host_version`, platform `target`/`artifact`, the opt-in Linux sandbox, and
 `plugins.lock` have landed.
- `pack` / `verify` / `install` / `update` / `uninstall`, Ed25519 signatures, and the trust store have
  landed; the registry and automatic upgrades have not.

### Phase 5: advanced extensions (partial)

- Inter-plugin RPC and jobs have landed; provider adapters and the optional WASM layer have not.

---

## 16. Key decisions

| Decision | Outcome | Rationale |
| --- | --- | --- |
| In-process dylib | Trusted opt-in | A C boundary with five symbols and JSON dispatch keeps it evolvable; only trusted libraries load, and the process backend stays the default |
| Out-of-process JSON-RPC | Default | Language-agnostic, easy to debug, isolates crashes |
| Capability gateway | Every host call | The capability model is the enforcement point, independent of the backend |
| Host-rendered UI | Always | Plugins never drive the terminal, so rendering stays safe and themeable |
| Two-tier manifests | Declarative first | Most extensions need no code, and code is opt-in |
| Integer API version | Exact match | A mismatch fails loudly rather than half-working |

---

## 17. Open questions

1. When to require signatures by default, and how to make local development pleasant.
2. Whether to add a registry service, or stay with directory installs plus a lockfile.
3. Which contribution points deserve first-class declarative forms next (skills, agents, themes,
  keymaps, MCP).
4. How much of the async host API can be exposed to dylib plugins through a bridging worker.
5. Whether the audit trail belongs in SQLite, the event log, or both.
6. With a multi-workspace daemon, whether to share one process by default or isolate per workspace.

---

## 18. Mapping to existing code

| Location | Status |
| --- | --- |
| `crates/eix-core/src/plugin.rs` | Manifest v2, config, JSON-RPC, content blocks, hooks/ui/rpc, `UiNode`, action and settings types |
| `crates/eix-core/src/plugin_package.rs` | `pack` / `verify` / `install` / `update`, checksums, Ed25519, trust store, `plugins.lock` / `verify_lock` |
| `crates/eixd/src/plugin/mod.rs` | `PluginManager`, registry, lifecycle and supervisor, logs/doctor, hook pipeline, RPC routing, declarative and action commands, UI rendering, backend selection, `degraded` counters, settings writes |
| `crates/eixd/src/plugin/logs.rs` | 500-line ring buffer per plugin, seq paging, redaction, error_count and last |
| `crates/eixd/src/plugin/backend.rs` | `ExtensionBackend` trait and `BackendFuture` |
| `crates/eixd/src/plugin/process.rs` | `ProcessBackend`: the JSON-RPC child-process adapter |
| `crates/eixd/src/plugin/dylib.rs` | `DylibBackend`, ABI checks, method translation, timeouts, dylib host API |
| `crates/eixd/src/plugin/client.rs` | Process spawn, sandbox, writer/reader, request multiplexing, timeout cancellation, event delivery, features |
| `crates/eixd/src/plugin/host_api.rs` | `HostContext`, shared `WorkspaceFs`/blob, fs/net/secrets/session/ui/rpc/job, capability checks |
| `crates/eixd/src/server.rs` | `plugin.command.run`, `plugin.settings[.set]`, `plugin.ui.list|render|action|pick`, `plugin.logs[.clear]`, `plugin.doctor`, watcher self-write suppression, turn/tool/prompt hook wiring |
| `crates/eixd/src/classifier.rs` | `RiskCategory::Plugin` retained; plugin tools need approval by default |
| `crates/eix-tui/src/main.rs` | `eix plugin new/run/test/pack/verify/install/update/uninstall/trust/untrust` CLI |
| `crates/eix-tui/src/tui/app/` | `/plugins [name|logs|doctor|start|stop|restart|enable|disable|uninstall|set|open|action|act|pick]`, `plugin.ui.*`, and view-tree rendering in `plugin_ui.rs` |
| `crates/eix-core/src/config.rs` | `plugins` parsing and allowlist validation, `register_plugin` / `remove_plugin_config`; a project-level `plugins` table is still dropped |
| `crates/eix-plugin-sdk/` | The Rust plugin SDK |
| `sdks/python/`, `sdks/typescript/` | Python and TS/JS plugin SDKs, examples, and tests |
| `sdks/conformance/` | The shared protocol conformance harness |
| `examples/plugins/echo/` | A v2 example covering JSON-RPC 2.0, content blocks, storage, events, and an action |
| `examples/plugins/hello-dylib/` | A dylib example: ABI v1, an `echo` tool, and a `sync` action command |

The hook, UI, and RPC wiring points are covered in sections 7, 8, and 9. The only pieces still
missing are a registry, automatic upgrades, the WASM or hosted runtime, and in-TUI settings
editing.

## 19. Next minimal steps

1. A registry, plus automatic upgrades for signed distributions.
2. More declarative contributions: skills, agents, themes, keymaps, and MCP.
3. In-TUI settings editing and richer view nodes (tree, tabs, progress, diff).
4. A WASM or hosted plugin runtime for real isolation.
5. A persistent `plugin_audit` table, replacing the in-memory ring buffer and `plugin.doctor`.
