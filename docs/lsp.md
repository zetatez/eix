# LSP and language servers

eix treats language servers as a **diagnostics source**, not as an IDE backend. After a file is
written it fetches compile and type errors once, and it exposes an `lsp_diagnostics` tool the model
can call on demand. There is no completion, hover, go-to-definition, rename, or code action.

Configuration lives under `[lsp.servers.<name>]` (see `config.tmpl.toml`); the implementation is in
`crates/eixd/src/lsp.rs`.

## 1. What it actually does

Each server is a stdio child process speaking over stdin/stdout. eix consumes exactly one
notification, `textDocument/publishDiagnostics`; every other LSP capability is out of scope.

Three entry points:

| Entry point | When | Where the result goes |
| --- | --- | --- |
| Post-write note | After a successful `apply_patch` or file write | Appended to tool output as an `LSP diagnostics:` section |
| `lsp_diagnostics {path}` | The model calls it | Returned as a read-only tool result, **bypassing approvals** |
| `lsp.workspace` RPC | A frontend or script asks | Aggregated diagnostics; nothing is added to the conversation |

The post-write note **never fails a write**: a broken server is a note, and the edit has already
landed.

## 2. How it works

- **Lazy startup.** A server is spawned the first time a file with one of its extensions needs
  diagnostics, then reused per `(server name, workspace root)` for the daemon's lifetime.
- **Direct exec, no shell.** `Command::new(command)` plus `args`, with no `sh -c`, so `command`
   cannot contain pipes, redirections, or `&&`.
- **cwd is the workspace root.** The child starts in the session's `cwd`, and that is also the
  `rootUri`. Finding the project root (`Cargo.toml`, `go.mod`, `tsconfig.json`) is the
  server's job.
- **Narrow `initialize` capabilities.** Only `textDocument.publishDiagnostics` is declared, and both
  `workspace.configuration` and `workspaceFolders` are `false`. When a server sends a
  `workspace/configuration` reverse request, eix answers `null`; every other reverse request
   gets `null` as well.
- **Timeouts.** A single-file collection window is `min(timeout_secs, 2s)` and stops after 500 ms of
  silence; a workspace scan is `min(timeout_secs, 5s)` with 800 ms of silence. `timeout_secs`
  itself is clamped to `1..=120`.
- **Installs what is missing.** When the executable is not on `PATH`, eix installs it into its own
  data directory in the background (see section 7), and the server works once that finishes.
- **Failure caching.** A server that is definitively unusable (no install recipe, install failed)
  goes into `missing` and is not respawned; other startup failures go into `failures`. Both mean
  "this server is unusable". With `auto_install = false`, a missing executable stays quiet.
- **Config reloads clear everything.** `lsp.clear()` drops every child and cache; the next request
  starts fresh.
- **16 MiB message ceiling.** Larger messages fail instead of being read into memory.

## 3. Settings at a glance

Under `[lsp.servers.<name>]`, where `<name>` is any identifier (built-in names are listed below):

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | empty | Executable name or absolute path, exec'd directly |
| `args` | string[] | `[]` | Arguments, passed one by one without a shell |
| `env` | table | `{}` | Additional environment variables |
| `extensions` | string[] | `[]` | Owned extensions, **without the dot**, case-insensitive |
| `language_id` | string? | none | languageId for `didOpen`; inferred from the extension when empty |
| `initialization_options` | JSON | none | Passed through as `initializationOptions` |
| `enabled` | bool | `true` | `false` disables the server |
| `timeout_secs` | int | `10` | Ceiling for `initialize` and one collection, clamped `1..=120` |

`extensions` accepts entries with or without a leading dot (`"rs"` and `".rs"` are equivalent),
because matching calls `trim_start_matches('.')`.

`[lsp]` itself has one switch:

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto_install` | bool | `true` | Install a missing built-in server automatically; `false` uses only servers already on `PATH` |

## 4. The 14 built-in servers

These entries exist without any `[lsp]` configuration:

| Name | command | args | extensions |
| --- | --- | --- | --- |
| `bash` | `bash-language-server` | `start` | `sh` `bash` |
| `cpp` | `clangd` | — | `c` `h` `cc` `cpp` `cxx` `hpp` |
| `css` | `vscode-css-language-server` | `--stdio` | `css` `scss` `less` |
| `go` | `gopls` | — | `go` |
| `html` | `vscode-html-language-server` | `--stdio` | `html` `htm` |
| `json` | `vscode-json-language-server` | `--stdio` | `json` `jsonc` |
| `lua` | `lua-language-server` | — | `lua` |
| `markdown` | `marksman` | `server` | `md` |
| `python` | `pyright-langserver` | `--stdio` | `py` |
| `rust` | `rust-analyzer` | — | `rs` |
| `sql` | `sqls` | — | `sql` |
| `toml` | `taplo` | `lsp` `stdio` | `toml` |
| `typescript` | `typescript-language-server` | `--stdio` | `js` `jsx` `mjs` `cjs` `ts` `tsx` `mts` `cts` |
| `yaml` | `yaml-language-server` | `--stdio` | `yaml` `yml` |

All of them default to `timeout_secs = 10` and `enabled = true`.

## 5. How an extension picks a server

1. Take the file extension and lowercase it; no extension means no server.
2. Walk `config.lsp.servers`, skipping entries with `enabled = false`, and take the first one whose
  `extensions` matches.
3. `lsp.servers` is a `BTreeMap`, so the walk is in **name order**.

Step 3 matters: when two enabled servers claim `rs`, the earlier name wins (`a-rust` beats `rust`),
regardless of the order they appear in the file. To change a language's behaviour, **override the
existing name** instead of inventing a new one.

`language_id` inference: `rs→rust`, `py→python`, `js/mjs/cjs→javascript`, `ts/mts/cts→typescript`,
`tsx→typescriptreact`, `jsx→javascriptreact`, `c→c`, `h/cc/cpp/cxx/hpp→cpp`, `java→java`,
`rb→ruby`, `json→json`, `toml→toml`, `md→markdown`, `sh/bash→shellscript`, and otherwise the
lowercased extension unchanged.

## 6. Overriding and adding servers

The built-in map is laid down first, then user entries are merged by name with `extend`, replacing a
same-named entry wholesale. To change rust-analyzer's arguments:

```toml
[lsp.servers.rust]
command = "rust-analyzer"
args = []
extensions = ["rs"]
timeout_secs = 30                 # first index of a large repo is slow

[lsp.servers.rust.initialization_options]
check.command = "clippy"
```

Mind the wholesale replacement: if you write `[lsp.servers.rust]` but forget `extensions`, the
entry owns no files (`#[serde(default)]` fills an empty array) and `rs` falls back to "no server".
Copy the extensions across when overriding.

Disabling a built-in server:

```toml
[lsp.servers.markdown]
enabled = false
```

Adding a language that has no built-in entry, such as Java:

```toml
[lsp.servers.java]
command = "jdtls"
extensions = ["java"]
timeout_secs = 60
```

`env` supplies PATH entries or server-specific variables:

```toml
[lsp.servers.python]
env = { PYRIGHT_PYTHON_FORCE_VERSION = "latest" }
```

**`[lsp]` belongs in the global config only.** A project config (`.eix/config.toml` inside a
repository) that sets `[lsp]` has the whole table dropped, with a warning reading
`project config cannot set [lsp]; use the global config`. The table can spawn arbitrary processes,
which makes it executable policy.

## 7. Installing servers per language

With the default `auto_install = true`, a missing built-in server is installed in the background
into eix's data directory (`eix/lsp` under `EIX_DATA_DIR` or the system data directory, with
executables in `lsp/bin`; on Windows, directly in `lsp`). It needs no sudo and writes nothing
outside that directory.

| Server | Install method |
| --- | --- |
| `bash` `css` `html` `json` `python` `typescript` `yaml` | `npm install --global --prefix <data>/lsp` |
| `cpp` | `pip install --prefix <data>/lsp clangd` |
| `go` `sql` | `go install` with `GOBIN=<data>/lsp/bin` |
| `rust` | `rustup component add rust-analyzer` |
| `markdown` | The matching platform binary from the `artempyanykh/marksman` GitHub release |
| `toml` | The matching platform gzip from the `tamasfe/taplo` GitHub release |

`lua` has no install recipe (there is no simple package source); when missing, it is simply
unavailable.

Installs are background jobs and **do not block** the tool call that triggered them. The first call
shows one `[lsp: installing lsp server "markdown" …]` note and then stays quiet; once the install
finishes, the next diagnostics request uses the server. A failed install is reported once; later
attempts are treated as missing and skipped silently.

Installation only triggers for a built-in server name whose `command` file name matches the built-in
binary. A user override with a different `command` (say `command = "my-rust-analyzer"`) is never
replaced. The managed `bin` directory is prepended to that server child's `PATH`; if the entry sets
its own `PATH` through `env`, that value wins.

To disable automatic installation and use only servers already on `PATH`:

```toml
[lsp]
auto_install = false
```

The manual installation instructions below still apply for offline machines, when automatic
installation is disabled, or when the system package manager is a better fit.

After installing, confirm the binary is on `PATH` with `command -v <name>`. If the daemon is
started by a service manager such as systemd, its `PATH` may be shorter than your shell's
(`~/.npm-global/bin` and `~/.cargo/bin` are common omissions). Either add to `PATH` through `env`
or use an absolute path in `command`.

### Rust: rust-analyzer

```sh
rustup component add rust-analyzer   # recent rustup; installs into ~/.cargo/bin
# Alternatively a distro package, or a GitHub release from rust-lang/rust-analyzer
```

The first index of a large repository exceeds the default 10 seconds; raise `timeout_secs` for this
entry to 30 or more.

### C and C++: clangd

```sh
sudo apt install clangd        # Debian/Ubuntu
sudo pacman -S clang           # Arch: clang ships clangd
```

clangd depends on a compilation database and reports very little without one:

```sh
cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON ..
# or: bear -- make
```

### Go: gopls

```sh
go install golang.org/x/tools/gopls@latest   # lands in $(go env GOPATH)/bin
```

### Python: pyright

```sh
npm install -g pyright          # provides pyright-langserver
# or the distro pyright package
```

pyright must locate an interpreter itself. Inside a virtualenv, place `pyrightconfig.json` in the
session's `cwd`, or set `pythonPath` through `initialization_options`.

### TypeScript and JavaScript: typescript-language-server

```sh
npm install -g typescript-language-server typescript
```

### HTML, CSS, JSON: vscode-langservers-extracted

```sh
npm install -g vscode-langservers-extracted
```

One package provides `vscode-html-language-server`, `vscode-css-language-server`, and
`vscode-json-language-server`, matching the three built-in entries.

### YAML: yaml-language-server

```sh
npm install -g yaml-language-server
```

### Bash: bash-language-server

```sh
npm install -g bash-language-server
```

### Lua: lua-language-server

```sh
sudo pacman -S lua-language-server                      # Arch
# Other distros: GitHub releases from lua-language-server/lua-language-server
```

### TOML: taplo

```sh
cargo install taplo-cli --locked
# or: npm install -g @taplo/cli
```

The built-in arguments are `lsp stdio`; keep them when writing an override.

### SQL: sqls

```sh
go install github.com/sqls-server/sqls@latest
```

### Markdown: marksman

```sh
# GitHub releases: artempyanykh/marksman
brew install marksman
```

The built-in argument is `server`. Link and anchor checking is marksman's main job; eix does not
duplicate it.

### Checking your installation

To see at a glance which built-in servers are present:

```sh
for c in rust-analyzer clangd gopls lua-language-server marksman \
         pyright-langserver sqls taplo typescript-language-server \
         yaml-language-server bash-language-server; do
    printf '%-28s %s\n' "$c" "$(command -v "$c" || echo MISSING)"
done
```

## 8. What diagnostics look like

Single-file results are one line each as `path:line:column: severity: message`, with lines and
columns starting at 1 (LSP is 0-based; eix adds one):

```
/home/me/proj/src/main.rs:12:9: error: mismatched types
```

Severity mapping: LSP `1→error`, `2→warning`, `3→info`, and anything else (including absent)→
`hint`. Diagnostics with an empty message are dropped.

Limits: the first 50 entries for a single file; 2000 after aggregating a workspace scan.

`lsp.workspace` parameters and results:

| Field | Direction | Meaning |
| --- | --- | --- |
| `session_id` | input | Required; selects which cwd to scan |
| `max_files` | input | Defaults to `200`, clamped `1..=2000` |
| `diagnostics` | output | Sorted by path, then line, then column |
| `files` | output | Number of files actually opened |
| `servers` | output | Names of the servers that ran |
| `skipped` | output | Files not opened because the cap was hit |

The scan skips `.git`, `.hg`, `.svn`, `.eix`, `.opencode`, `node_modules`, `target`, `dist`, `build`,
`.venv`, `venv`, and `__pycache__`. Directory listings are cached for 30 seconds, up to 8 entries,
and cleared on config reload.

## 9. Troubleshooting

**No `LSP diagnostics:` section after a write.**
First check whether any server claims the extension. "The server found nothing" and "no server at
all" look identical in the post-write note; call `lsp_diagnostics` to tell them apart, since it
reports `no language server configured for <path>` explicitly.

**`lsp_diagnostics` reports `lsp diagnostics failed: cannot start lsp server …`**
The executable is not on `PATH`, and no automatic install is available (`auto_install = false`, no
install recipe, or a custom `command`). Watch the suffix: the first attempt reads `cannot start …:
rust-analyzer is not installed`, and later attempts hit the `missing` cache and read
`lsp server "rust" (rust-analyzer) is not installed`. After installing, reload the config (or restart
the daemon), because failures are cached.

**`lsp … timed out`**
`initialize` or a read timed out. Raise `timeout_secs` (maximum 120) for a large repository. A
server that never responds usually has wrong arguments; for example, `marksman` without the
`server` argument exits immediately, which surfaces as a closed connection.

**`lsp server closed the connection`**
The child exited. Run `command` plus `args` by hand to read its stderr; eix wires the server's
stderr to `/dev/null`, so it never appears in tool output.

**`lsp message is too large`**
A single message exceeded 16 MiB, usually a server trying to return its whole index. Use a lighter
server or scan a smaller workspace.

**A single `[lsp: …]` note appears once and never repeats**
That is the deduplication by design, so the same note does not reappear for every file. The first
automatic install produces one `installing …` note, and a failed install is reported once. An
install already in progress, a missing server with `auto_install = false`, and later failures write
nothing.

**Diagnostics for an extension never change**
Servers are long-lived, and `didChange` is only sent when eix itself writes the file again. External
edits (`git checkout`, another editor) are never pushed to it; reloading the config drops the
client so the next request rebuilds it.

## 10. Relation to permissions and the sandbox

- `lsp_diagnostics` is read-only and allowed automatically, without an approval.
- It still obeys read-path rules: `path` passes through `resolve_path_in` and
  `tools.external_path_allowed` first. See [Approvals, sandboxing, and permission rules](./permissions.md).
- Language server children are **not** managed by the `exec_command` sandbox. They run with the
  daemon's privileges, and how much memory they use or which files they touch is up to the
  server. That is why `[lsp]` is global-config only.
