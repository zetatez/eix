# eix dylib plugin system design (ABI v1)

- Status: **P1 has landed; P2 is partially landed.** The `eix-plugin-abi` and `eix-plugin-sdk`
  skeletons, the `ExtensionBackend` abstraction in `eixd`, the `ProcessBackend` adapter, and the
 `DylibBackend` loader are implemented. `describe`, `tool.call`, and `command.run` can be called on
  real plugin paths, and `hook.run`, `rpc.call`, `event.notify`, and `config.changed` are wired up as well.
- Goal: native performance, very deep extension points, and new capabilities without changing the
  ABI.
- Related code: `crates/eix-plugin-abi/`, `crates/eix-plugin-sdk/`,
 `crates/eixd/src/plugin/{backend,process,dylib}.rs`, `examples/plugins/hello-dylib/`.
- Existing implementation: `crates/eixd/src/plugin/` supports both out-of-process JSON-RPC
 (`ProcessBackend`) and in-process dylib (`DylibBackend`), sharing one registry, one capability check,
  and one hook router.

This document describes how to build a deeper, faster, more native extension system with **dylibs
(dynamic libraries)** than the existing out-of-process JSON-RPC plugins, while keeping it
evolvable. It currently covers `describe`, `tool.call`, `command.run`, `hook.run`, `rpc.call`,
`event.notify`, and `config.changed`, plus the synchronous-safe host API (log, storage, secrets, fs,
session read, blob). `net`, `ui`, `job`, and `host.rpc.call` need an async bridge and remain a later phase.
This document does not replace the landed v2 protocol in [`plugins.md`](plugins.md).

### 1.0 Using P1

```toml
# ~/.config/eix/config.toml
[plugins.hello-dylib]
enabled = true
# Absolute path; a relative path is resolved against the config cwd.
library = "/opt/eix/plugins/hello-dylib/libeix_plugin_hello_dylib.so"
capabilities = ["tools", "commands"]
timeout_secs = 30
```

Or declare it in a manifest (relative paths resolve against the manifest directory):

```toml
# eix-plugin.toml
schema = 2
name = "hello-dylib"
version = "0.1.0"
abi = 1
capabilities = ["tools", "commands"]

[components.dylib]
path = "libeix_plugin_hello_dylib.so"
```

```toml
[plugins.hello-dylib]
enabled = true
manifest = "/opt/eix/plugins/hello-dylib/eix-plugin.toml"
```

Rules:

- `library` is exclusive with `command` and `components.server`; declaring both fails at startup.
- `library` may be absolute; a relative path resolves against the config cwd. A manifest's
 `components.dylib.path` resolves against the manifest directory.
- Both the manifest's `abi` and the exported `eix_plugin_abi_version` are checked; a mismatch
  refuses to load.
- The `command` field in `plugin.list` shows the library path; the actual backend is chosen by
  whether `library` is present.

---

## 1. Goals and non-goals

### 1.1 Goals

- Plugins share the contribution registry, hook pipeline, UI model, and permission system with
  built-in features.
- Plugins can contribute tools, commands, hooks, prompt/context, providers, agents, skills, themes,
  keymaps, MCP, UI, RPC, and background jobs.
- Adding a contribution point or a host method **never changes the ABI and never forces existing
  plugins to recompile**.
- Call overhead approaches a function call, not a pipe plus JSON.
- Authors write plugins the way they write built-in Rust features: implement one trait, add one
  macro.
- Supply-chain constraints such as signatures, install paths, and capability allowlists remain.

### 1.2 Non-goals

- A dylib is not a security sandbox. An in-process dylib holds every privilege of the daemon.
- Running arbitrary third-party code safely is not a goal. Use hosted mode or WASM when you need
  isolation.
- Binary compatibility across Rust versions is not guaranteed. The stable part of the ABI is the C
  boundary, not Rust type layout.
- No hard timeout or forced termination for in-process dylibs; that requires a helper process.
- The existing JSON-RPC backend stays. Both coexist and each plugin picks one.

---

## 2. Hard constraints

The design has to work around these facts:

1. **Rust has no stable ABI.** Across a dylib boundary you may pass only `#[repr(C)]` POD, pointers,
   lengths, integers, and opaque handles; never `String`, `Vec`, `Arc`, trait objects, or `Result`.
2. **The dylib shares the host process.** A plugin panic, segfault, or `process::exit` hits the
   daemon directly.
3. **A dylib cannot really be unloaded.** Rust TLS, statics, and background threads make `dlclose`
   unreliable, so hot reload can only load a new copy, switch the registry, and leave the old
   copy resident.
4. **A synchronous ABI cannot hard-kill.** If a plugin hangs, an in-process design can only time
   out and isolate it; it cannot `kill` it like a child process.
5. **The capability system only constrains the host API.** A plugin can call `libc` directly and
   bypass `host.fs` and `host.net`.

So a dylib plugin must be treated as **fully trusted code**, and the security boundary belongs
before loading: signature and install-path checks.

---

## 3. Architecture

```text
eixd
   turn / tool / prompt / approval / UI pipeline
    ContributionRegistry (strongly typed, indexed by kind)
     ExtensionBackend trait
      BuiltinBackend   DylibBackend   ProcessBackend   WasmBackend
       (compiled in)   (dlopen)       (JSON-RPC)       (optional)
     HostApiV1 + plugin dispatch
     (5 C symbols, never growing)
```

The key point: **a dylib is just one backend**. The registry, permissions, UI, and hook pipeline
treat built-ins and plugins alike, and that is what makes it "native".

---

## 4. ABI v1

### 4.1 Design principles

- **The ABI never grows.** Every capability goes through a string method plus JSON parameters.
- Only five plugin symbols are exported: `abi_version`, `create`, `dispatch`, `free`, `destroy`.
- The host exposes one `HostApiV1` table with `dispatch` and `free`.
- A new contribution point is a new JSON section; a new method is a new string. Old plugins keep
  working.
- Only bytes, lengths, pointers, and integers cross the boundary.

### 4.2 Base types

```rust
pub const ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EixStr {
  pub ptr: *const u8,
  pub len: usize,
}

#[repr(C)]
pub struct EixBuf {
  pub ptr: *mut u8,
  pub len: usize,
  pub cap: usize,
}
```

- `EixStr`: borrowed UTF-8 bytes, valid until the call returns.
- `EixBuf`: an owned byte buffer, not `Copy`, freed exactly once.
- Large binaries are not inlined; they go through `host.blob.*` or base64.

### 4.3 The host API table

```rust
pub type HostDispatchFn = unsafe extern "C" fn(
  ctx: *mut c_void,
  method: EixStr,
  params: EixStr,
  out: *mut EixBuf,
) -> i32;

pub type HostFreeFn = unsafe extern "C" fn(ctx: *mut c_void, buf: EixBuf);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApiV1 {
  pub abi_version: u32,
  /// sizeof(HostApiV1); allows fields to be appended later
  pub size: usize,
  pub ctx: *mut c_void,
  pub dispatch: Option<HostDispatchFn>,
  pub free: Option<HostFreeFn>,
}
```

- A plugin checks `abi_version == ABI_VERSION` and `size >= size_of::<HostApiV1>()`.
- When the host appends fields later, old plugins read only the prefix they know.
- `dispatch` and `free` use `Option` for nullable function pointers; the FFI layout matches a raw
  pointer.

### 4.4 Exported plugin symbols

```rust
pub type PluginAbiVersionFn = unsafe extern "C" fn() -> u32;

pub type PluginCreateFn = unsafe extern "C" fn(
  host: *const HostApiV1,
  config: EixStr,
  out: *mut *mut c_void,
) -> i32;

pub type PluginDispatchFn = unsafe extern "C" fn(
  instance: *mut c_void,
  method: EixStr,
  params: EixStr,
  out: *mut EixBuf,
) -> i32;

pub type PluginFreeFn = unsafe extern "C" fn(instance: *mut c_void, buf: EixBuf);

pub type PluginDestroyFn = unsafe extern "C" fn(instance: *mut c_void);
```

The exported symbol names:

```text
eix_plugin_abi_version
eix_plugin_create
eix_plugin_dispatch
eix_plugin_free
eix_plugin_destroy
```

### 4.5 Status codes

```rust
pub const EIX_OK: i32 = 0;
pub const EIX_ERR_UNKNOWN_METHOD: i32 = -1;
pub const EIX_ERR_INVALID: i32 = -2;
pub const EIX_ERR_DENIED: i32 = -3;
pub const EIX_ERR_PANIC: i32 = -4;
pub const EIX_ERR_VERSION: i32 = -5;
pub const EIX_ERR_REENTRANT: i32 = -6;
pub const EIX_ERR_TIMEOUT: i32 = -7;
pub const EIX_ERR_INTERNAL: i32 = -8;
```

Error detail goes into the `out` JSON:

```json
{"code":"invalid_params","message":"missing field `path`"}
```

Even when the status code is non-zero, the host must read `out` (when present) and call
`eix_plugin_free`, otherwise the error detail leaks.

### 4.6 Memory ownership

| Direction | Allocated by | Freed by |
| --- | --- | --- |
| host → plugin input `EixStr` | host | host, after the call returns |
| plugin → host return `EixBuf` | plugin | host calls `eix_plugin_free` |
| plugin → host input `EixStr` | plugin | plugin, after the call returns |
| host → plugin return `EixBuf` | host | plugin calls `HostApiV1::free` |

Rules:

- Never pass `String`, `Vec`, `Arc`, or a trait object across the boundary.
- A plugin builds a returned `EixBuf` with `Vec::into_raw_parts`; `eix_plugin_free` restores it with
 `Vec::from_raw_parts`.
- Input buffers are owned by the caller; the callee must not retain the pointer.
- Never return a buffer pointing at static memory.

### 4.7 Panics, threads, and reentrancy

- Every `extern "C"` entry wraps its body in `catch_unwind(AssertUnwindSafe(…))`.
- Both plugin and host use `panic = "unwind"`; `panic = "abort"` cannot be caught.
- Never let an unwind cross the FFI boundary.
- Plugins must not call `std::process::exit`.
- The host must guarantee that `dispatch` on one instance is never concurrent. ABI v1 supports only
 `"threading": "serialized"`.
- If real concurrency is ever needed, change the trait to `&self` plus interior mutability, or have
  the host lock per instance; `&mut self` cannot be called from several threads.
- Never call back into the same instance during dispatch; when recursion is needed, return
 `EIX_ERR_REENTRANT` and let the plugin move the work to an async task.
- Dylib calls are synchronous; the host must run them on a blocking worker, never on the async
  executor.

---

## 5. Method table

### 5.1 host → plugin

| Method | Parameters | Returns |
| --- | --- | --- |
| `describe` | `{}` | `Descriptor` |
| `tool.call` | `{name, arguments, session_id, workspace, cwd}` | Tool result |
| `command.run` | `{command, arguments, session_id, workspace, cwd}` | `{ok, prompt?, output?}` |
| `hook.run` | `{point, session_id, turn_id, …}` | Hook result |
| `ui.render` | `{view_id, revision, surface}` | `UiNode` |
| `ui.action` | `{view_id, action_id, payload}` | Arbitrary JSON |
| `event.notify` | `{method, …}` | `{}` |
| `config.changed` | New settings | `{}` |
| `provider.list` | `{}` | Model list |
| `provider.stream` | `{model, messages, …}` | `{handle}` |
| `provider.next` | `{handle}` | `{chunk?, done}` |
| `rpc.call` | `{method, arguments, caller, depth}` | Arbitrary JSON |
| `job.run` | `{job_id}` | `{}` |
| `lifecycle.start` | `{}` | `{}` |
| `lifecycle.stop` | `{}` | `{}` |
| `health` | `{}` | `{ok}` |

Unrecognised methods fall through to the default `Plugin::dispatch`, which returns
`unknown_method`; a plugin may override it to support newer host methods.

### 5.2 plugin → host

| Method | P1 | Notes |
| --- | --- | --- |
| `host.log` | Supported | Writes to the daemon log |
| `host.info` | Supported | Returns plugin name, backend type, capabilities, and paths |
| `host.ping` | Supported | Health check |
| `host.notify` | Supported | Toast / `plugin.notify` event |
| `host.storage.get/set/delete/list` | Supported | KV with `global` and `workspace` scopes |
| `host.secrets.get` | Supported | Environment-variable allowlist; only `plugins.<name>.secrets` |
| `host.fs.read/list/write` | Supported | Shares `WorkspaceFs` with the process backend: canonicalise, reject `..` and symlinks, single file <=1 MiB |
| `host.net.request` | Not yet | Requires an async bridge |
| `host.session.get/list` | Supported | Read-only; needs the `sessions_read` capability |
| `host.ui.open/update/close` | Not yet | Requires a UI event bridge |
| `host.rpc.call` | Not yet | Requires an async bridge |
| `host.blob.put/get` | Supported | Shares the content-addressed store with the process backend, <=8 MiB each |
| `host.job.register/trigger/unregister` | Not yet | Requires an async bridge |
| `host.register/unregister` | Design goal | Dynamic contributions |
| `host.config.get` | Design goal | Current settings |

The synchronous-safe methods (log, storage, secrets, fs, session read, blob) are implemented
directly. net, ui, rpc, and job need an await or a return to the daemon event loop, so before P2
they explicitly return `EIX_ERR_DENIED` and suggest the hosted or process mode.

`config.changed` is the only non-event notification: `DylibBackend::notify` translates it into a
`config.changed` dispatch, and the SDK updates its cache and calls `Plugin::config_changed`. Every
other notification is wrapped as `event.notify {method, params}` and delivered to `Plugin::event`.

Adding a host method only means adding a string branch on the host side; the ABI does not change.

---

## 6. Descriptor and contribution registration

After `create`, the host calls:

```text
dispatch("describe", "{}") -> Descriptor
```

```json
{
 "name":"acme",
 "version":"2.1.0",
 "abi": 1,
 "capabilities":["tools","hooks","ui","network","storage"],
 "threading":"serialized",
 "contributions":{
  "tools":[
   {"name":"search","description":"…","input_schema":{…},"read_only": true}
  ],
  "commands":[
   {"name":"sync","action":"sync","args":"[--full]"}
  ],
  "hooks":[
   {"point":"tool.before","priority":10,"on_error":"deny"}
  ],
  "events":["session.*","turn.*"],
  "ui":[
   {"surface":"panel","id":"issues","title":"Issues"}
  ],
  "providers":[
   {"id":"acme","models":["acme-large"],"streaming": true}
  ],
  "agents":[],
  "skills":[],
  "themes":[],
  "keymaps":[],
  "mcp_servers":[],
  "rpc":[{"name":"issues.search","description":"Search cached issues"}]
 },
 "settings_schema":{"type":"object","properties":{}}
}
```

The host unpacks this into the `ContributionRegistry` and then does three things:

1. Namespace validation: `plugin__<name>__<tool>`, `plugin/<name>/<command>`, `plugin.<name>.*`.
2. Capability validation: declaring hooks without the `hooks` capability fails startup.
3. Conflict detection: a same-named contribution is refused, and deny wins.

Registration can also happen at runtime:

```text
host.dispatch("register", {"kind":"tool", …})
host.dispatch("unregister", {"kind":"tool","name":"search"})
```

For example, tools can be exposed per workspace without a restart.

---

## 7. Deep extension points

| Pipeline | How it extends |
| --- | --- |
| Model tools | `tools` plus `tool.call`, with streaming output, `read_only`, and an approval policy |
| Commands | `commands` plus `command.run`; prompt or action, never both |
| Prompt | `prompt.build` hook; may append context blocks and rewrite the system prompt |
| Model request | `model.request` hook; may change temperature, the tool allowlist, and injected messages |
| Model response | `model.response` hook; may redact and annotate |
| Provider | `providers` plus `provider.*`; a plugin can contribute an OpenAI-compatible provider |
| Agent | `agents`; declares prompt, tool set, and model preference |
| Skill | `skills`; provides `SKILL.md` |
| Hook | `tool.before/after`, `approval.before`, `file.before_write/after_write`, `session.*`, `compact.*`, `turn.*` |
| UI | `ui.render` returns a node tree that the host renders; `ui.action` carries interactions back |
| Keymap | `keymaps`, namespaced `plugin.<name>.*` |
| Theme | `themes`; may reference or add tokens, never override a built-in theme |
| Settings | `settings_schema` plus `config.changed` |
| Event | `events`, with filtering and a backpressure policy |
| Storage | `host.storage.*` / `host.blob.*` |
| Background jobs | `host.job.*` |
| Inter-plugin calls | `host.rpc.call` |
| MCP | Declare `mcp_servers`; reuses the existing MCP client |

### UI node tree

```json
{
 "kind":"list",
 "id":"issues",
 "title":"Issues",
 "items":[{"id":"42","label":"#42 Fix parser","detail":"open"}],
 "actions":[
  {"id":"open","label":"Open","key":"Enter"},
  {"id":"refresh","label":"Refresh","key":"r"}
 ]
}
```

Supported nodes: `text`, `markdown`, `list`, `table`, `tree`, `tabs`, `form`, `diff`, `log`, `progress`,
`sparkline`, `actions`, `spacer`.

Limits: 2000 nodes, depth 16, 64 KiB per text node; an unknown `kind` renders as an error
placeholder instead of crashing.

### What a new contribution point touches

Only three places, and **none of them is the ABI**:

1. A new section in the Descriptor JSON.
2. A new index and pipeline entry point in the host `ContributionRegistry`.
3. A new chainable helper in the SDK, such as `.provider(…)`.

---

## 8. Lifecycle

```text
dlopen
  → eix_plugin_abi_version
  → eix_plugin_create(host, config)
  → dispatch("describe")
  → register contributions
  → dispatch("lifecycle.start")
  → run
  → dispatch("lifecycle.stop")
  → eix_plugin_destroy
  → no dlclose
```

### 8.1 Hot reload

```text
copy the new .so to a unique path (libacme-<hash>.so)
  → dlopen the new copy
  → create + describe
  → atomically switch the registry
  → destroy the old instance
  → keep the old Library resident (no dlclose)
```

Never overwrite the original file and `dlopen` the same path; the dynamic linker may return the
old handle. Use versioned file names.

In P1, `plugin.restart` runs `DylibBackend::load` again and destroys the old instance, but the
library handle stays resident by design. To make new code take effect, copy the new build to a
versioned path and point the config at it; overwriting the original path and restarting only
guarantees the old mapping is still referenced, not that new code loads.

### 8.2 Timeouts

- The host calls `dispatch` on a blocking worker, wrapped in `tokio::time::timeout`.
- Calls on one instance are serialised by a `std::sync::Mutex`, and `spawn_blocking` keeps the
  async executor free.
- After a timeout the host calls `mark_closed`, `plugin.list` reports failed, and later requests are
  refused.
- **A thread cannot be killed.** If a plugin may hang forever, use hosted mode. P1's `shutdown`
  uses `try_lock` and never waits for a stuck call; the instance is destroyed when the last
 `Arc` is released.

### 8.3 Crashes

An in-process dylib crash is a daemon crash. Mitigations:

- Load only signed and trusted plugins.
- Put critical plugins in a helper process.
- After a crash, the supervisor restarts the daemon and marks the plugin failed.

### 8.4 Dual mode

P1 implements `inproc` only (inside the daemon process). Hosted mode is a later goal:

```toml
[plugins.acme]
# P1: declare the library directly
library = "/path/to/libacme.so"
# P4 goal: run the dylib in a dedicated child process
# mode = "hosted"
```

`hosted` is still a dylib, just loaded into a dedicated child process and talking over shared
memory or a Unix socket. It keeps native code and low overhead while restoring crash isolation and
hard timeouts.

**Recommendation: third-party plugins default to `hosted`; first-party plugins use `inproc`.**

---

## 9. Security model

This must be stated plainly:

- **An in-process dylib has no sandbox.** It can call `libc::open`, read the environment, and open
  sockets directly, bypassing every `host.*` capability check.
- A dylib plugin must therefore be treated as **fully trusted code**.
- The security boundary sits **before loading**:
  - load only from `~/.config/eix/plugins`;
  - verify `eix-plugin.sums` plus the Ed25519 signature;
  - require the manifest to declare `abi`, `target`, and `artifact`;
  - refuse path escapes, symlinks, and target mismatches.
- For real isolation, use hosted mode plus Landlock, seccomp, or a separate user.
- Capability allowlists remain, for hosted mode, defense in depth, and auditing.

Do not claim an in-process dylib is safe. It is not.

---

## 10. Using the SDK

### 10.1 Plugin side

```rust
use eix_plugin_sdk::prelude::*;

#[derive(Default)]
struct Acme;

impl Plugin for Acme {
  fn describe(&self) -> Descriptor {
    Descriptor::new("acme")
       .version(env!("CARGO_PKG_VERSION"))
       .capabilities(["tools", "hooks", "ui", "storage"])
       .tool(Tool::new("search", json!({
          "type": "object",
          "properties": {"query": {"type": "string"}},
          "required": ["query"],
       })).description("Search cached issues").read_only())
       .command(Command::action("sync", "sync").args("[--full]"))
       .hook(Hook::new("tool.before").priority(10).on_error("deny"))
       .ui_panel("issues", "Issues")
       .rpc("issues.search")
  }

  fn call_tool(&mut self, ctx: &mut Ctx<'_>, name: &str, args: Value) -> Result<Value> {
    match name {
       "search" => {
          let query = args.get("query").and_then(Value::as_str).unwrap_or_default();
          ctx.log("info", &format!("search: {query}"))?;
          Ok(json!({
          "content": [{"type": "text", "text": "found 0 issues"}],
          }))
        }
        other => Err(Error::UnknownMethod(other.to_string())),
    }
  }
}

eix_plugin!(Acme);
```

`eix_plugin!` handles:

- generating the five C symbols;
- wrapping panics;
- JSON encode/decode;
- freeing memory;
- routing `describe`, `tool.call`, `command.run`, `hook.run`, `ui.render`, `ui.action`,
 `event.notify`, `config.changed`, and `lifecycle.*` to trait methods;
- forwarding unknown methods to `Plugin::dispatch`.

### 10.2 Host-side loader (sketch)

```rust
pub struct DylibBackend {
  _lib: libloading::Library,
  instance: *mut c_void,
  dispatch: abi::PluginDispatchFn,
  free: abi::PluginFreeFn,
  destroy: abi::PluginDestroyFn,
}

impl DylibBackend {
  pub fn load(path: &Path, host: &abi::HostApiV1) -> anyhow::Result<Self> {
    let lib = unsafe { libloading::Library::new(path)? };
    let version: libloading::Symbol<abi::PluginAbiVersionFn> =
        unsafe { lib.get(b"eix_plugin_abi_version\0")? };
    if unsafe { version() } != abi::ABI_VERSION {
        anyhow::bail!("plugin ABI mismatch");
    }
    let create: libloading::Symbol<abi::PluginCreateFn> =
        unsafe { lib.get(b"eix_plugin_create\0")? };
    let mut instance = std::ptr::null_mut();
    let code = unsafe { create(host, abi::EixStr::EMPTY, &mut instance) };
    anyhow::ensure!(code == abi::EIX_OK, "create failed: {}", abi::status_name(code));
    // resolve and store dispatch, free, and destroy here.
    // 
  }
}
```

In the real implementation, watch the lifetime of `libloading::Symbol`: the symbol borrows the
`Library`, so dereference it and store a bare `fn` pointer before the symbol goes out of scope.

---

## 11. Building and distribution

### 11.1 Building

```bash
cargo build -p eix-plugin-hello-dylib --release
```

Output:

```text
target/release/libeix_plugin_hello_dylib.so   # Linux
target/release/libeix_plugin_hello_dylib.dylib # macOS
target/release/eix_plugin_hello_dylib.dll      # Windows
```

### 11.2 Manifest extension

P1 supports `abi` and `components.dylib`:

```toml
schema = 2
name = "acme"
version = "2.1.0"
abi = 1
capabilities = ["tools", "commands"]

[components.dylib]
path = "libacme.so"

[package]
publisher = "acme"
signature = "eix-plugin.sig"
checksums = "eix-plugin.sums"
```

`components.dylib.path` resolves against the manifest directory and must be a non-empty relative
path (absolute paths are only allowed in the config's `library`). `components.server` and
`components.dylib` are mutually exclusive.

`target` and `artifact` are resolved: `artifact` wins over `path`, and `target` accepts aliases
such as `linux-x86_64`, `x86_64-unknown-linux-gnu`, and `amd64`. It is compared with the current
platform at load time, and a mismatch is refused outright.

### 11.3 Installing

```bash
eix plugin pack dist/
# Sign eix-plugin.sums with an external Ed25519 tool and write the base64 signature to eix-plugin.sig
eix plugin verify dist/
eix plugin install dist/
```

For local development, `eix plugin install dist/ --allow-unsigned` backfills `[package]` for a
directory without one and repacks it. Installation checks:

- `abi` is within the range the host supports;
- `min_host_version` is not above the current host;
- `target` matches the current platform;
- checksums and the Ed25519 signature (mandatory when `require_signature = true`);
- `artifact` is inside the package, with no path escape and no symlink.

After installation the version and checksums are written to `plugins/plugins.lock`;
`eix plugin verify-lock` (or the library function `verify_lock`) reports drift.

---

## 12. Relation to the existing system

The existing `PluginManager` is not thrown away; it becomes a backend. P1 uses an explicit
`Pin<Box<dyn Future + Send + 'a>>` rather than `async-trait`:

```rust
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ExtensionBackend: Send + Sync {
  fn name(&self) -> &str;
  fn initialize<'a>(&'a self, spec: &'a PluginSpec)
    -> BackendFuture<'a, Result<PluginInitializeResult>>;
  fn request<'a>(&'a self, method: &'a str, params: Value)
    -> BackendFuture<'a, Result<Value>>;
  fn notify<'a>(&'a self, method: &'a str, params: Value)
    -> BackendFuture<'a, Result<()>>;
  fn is_closed(&self) -> bool;
  fn exit_reason<'a>(&'a self) -> BackendFuture<'a, Option<String>>;
  fn shutdown<'a>(&'a self) -> BackendFuture<'a, ()>;
}
```

Backend methods keep the v2 protocol names (`tools/call`, `commands/run`, `hooks/run`, `rpc.call`);
`DylibBackend` translates them to `tool.call`, `command.run`, and `hook.run`, and renames the
`tools/call` parameter `tool` to `name`.

- `ProcessBackend`: the existing JSON-RPC child process, adapted directly.
- `DylibBackend`: new; P1 wires it into `PluginManager` for startup, tools, commands, hooks,
  events, RPC, and shutdown.
- `BuiltinBackend`: built-in features may later use the same registry.
- `ContributionRegistry`: reuses the `tools/commands/subscriptions` registry in `PluginManager`
  rather than adding another abstraction.
- The TUI never needs to know the backend type.

So the dylib is an incremental capability, not a rewrite.

### Migration order

1. Done: extract `ExtensionBackend` and adapt the existing `PluginManager` as `ProcessBackend`.
2. Done: wire in `DylibBackend`, starting with `describe`, `tool.call`, and `command.run`.
3. Add hooks, UI, RPC, and providers (hook/event/RPC routing is reused; host-side async methods are
  still missing).
4. Add hosted mode and signature verification.
5. Consider WASM only after that.

---

## 13. Pitfall checklist

1. **Rust ABI is unstable.** After a host toolchain upgrade an old dylib may be layout-incompatible.
   Require the `abi` version and recompile.
2. **Allocators never cross the boundary.** Whoever allocates frees, or everything goes through the
  `free` function.
3. **Panics must be caught.** Every entry point wraps `catch_unwind`.
4. **Never `dlclose`.** Rust TLS, threads, and statics make unloading unsafe.
5. **Avoid symbol clashes.** Use `RTLD_LOCAL`, export only five symbols, and hide the rest.
6. **Never `println!` to stdout.** Use `host.log`.
7. **A plugin must not call `process::exit`.** It would kill the daemon.
8. **Async runtime.** Dylib calls are synchronous; the host uses `spawn_blocking`. A plugin may
   build its own tokio runtime, but `destroy` must shut it down and leak no threads.
9. **Reentrancy deadlock.** Never call back into the same instance during dispatch.
10. **Timeouts cannot kill a thread.** Use hosted mode when you need a hard timeout.
11. **Global state.** Each dylib has its own `std` copy, TLS, and global allocator; do not assume
   it shares them with the host.
12. **Static initialisation.** Never panic or start threads from a dylib's static constructors.

---

## 14. Roadmap

| Phase | Content | Acceptance |
| --- | --- | --- |
| P0 done | `eix-plugin-abi` and `eix-plugin-sdk` skeletons; the example dylib compiles | `cargo check --workspace --all-targets` |
| P1 done | `DylibBackend` loader, `describe`, `tool.call`, `command.run` | A plugin tool can be called in a real turn |
| P2 in progress | hooks, events, `rpc.call`, `config.changed`, and the synchronous host API (fs/session/blob) have landed; UI panels and async host methods remain | Feature parity with the JSON-RPC backend |
| P3 in progress | Process plugins already support the UI node tree, pickers, and `ui.render/action/pick`; the dylib UI bridge remains | Usable for highly customised surfaces |
| P4 done | `pack/verify/install/update`, signatures, `plugins.lock`, `require_signature`, `target`/`artifact`, the opt-in Linux sandbox; the registry remains | Third-party plugins can be distributed safely |
| P5 | Optional WASM backend | A sandbox for untrusted plugins |

---

## 15. Code mapping

| Location | Status |
| --- | --- |
| `crates/eix-plugin-abi/src/lib.rs` | `EixStr` / `EixBuf` / `HostApiV1`, the five plugin function types, and the status codes |
| `crates/eix-plugin-sdk/src/lib.rs` | `Plugin` trait, `Descriptor` builder, `Ctx`, `RpcCall`, the `eix_plugin!` macro, and the FFI wrapper; routes `describe`, `tool.call`, `command.run`, `hook.run`, `rpc.call`, `event.notify`, `config.changed`, and `lifecycle.*` |
| `crates/eixd/src/plugin/backend.rs` | `ExtensionBackend` trait and `BackendFuture` |
| `crates/eixd/src/plugin/process.rs` | `ProcessBackend`: the adapter for the existing JSON-RPC child process |
| `crates/eixd/src/plugin/dylib.rs` | `DylibBackend`: loading, ABI checks, method translation, timeouts, dylib host API |
| `crates/eixd/src/plugin/host_api.rs` | The shared `WorkspaceFs` and blob implementation, reused by the process and dylib backends |
| `crates/eixd/src/plugin/mod.rs` | `PluginManager` selects the backend from `spec.library`; the registry and hook/tool/command/RPC/event routing are shared; failure counts and `degraded` |
| `crates/eix-core/src/plugin.rs` | `components.dylib`, `abi`, `library`, `PLUGIN_DYLIB_ABI_VERSION`, settings schema validation, and UI surface types |
| `examples/plugins/hello-dylib/` | The minimal cdylib example (an `echo` tool plus a `sync` command) and its `eix-plugin.toml` |
| `docs/plugins.md` | The landed v2 out-of-process plugin system |

P1 is wired: `PluginManager::start_backend` picks `DylibBackend` when `spec.library` is set,
`describe` registers tools and commands, `tools/call`, `commands/run`, `hooks/run`, and `rpc.call` are
dispatched through the trait, and `shutdown` destroys the instance without `dlclose`. P2 added
the synchronous host API, `config.changed` hot updates, and `degraded` counting.
