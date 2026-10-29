# hello-dylib

Minimal dylib plugin for ABI v1. It is a workspace member, so
`cargo check --workspace --all-targets` compiles the boundary code.

```bash
cargo build -p eix-plugin-hello-dylib --release
```

This produces `target/release/libeix_plugin_hello_dylib.so` on Linux
(`.dylib` on macOS, `.dll` on Windows). Build the debug variant when running
the ignored host test:

```bash
cargo build -p eix-plugin-hello-dylib
cargo test -p eixd --lib -- --ignored dylib
```

## Load it

The daemon can point at the library directly:

```toml
[plugins.hello-dylib]
enabled = true
library = "/absolute/path/to/libeix_plugin_hello_dylib.so"
capabilities = ["tools", "commands"]
timeout_secs = 30
```

Or install the manifest next to the library and reference it:

```toml
[plugins.hello-dylib]
enabled = true
manifest = "/absolute/path/to/eix-plugin.toml"
```

`eix-plugin.toml` in this directory uses
`[components.dylib] path = "libeix_plugin_hello_dylib.so"`, resolved relative
to the manifest. Copy the `.so` next to the manifest when installing.

`library` and `command` / `[components.server]` are mutually exclusive. The
manifest `abi = 1` and the exported `eix_plugin_abi_version` are both checked.

## What it demonstrates

- `describe` registers one tool (`echo`) and one action command (`sync`).
- `tool.call` / `command.run` round-trip through `eix_plugin_dispatch`.
- `host.log` goes through the host callback; storage and `host.secrets.get`
  are available when the matching capabilities are granted.

The crate implements the five exported symbols through `eix_plugin!`:

```text
eix_plugin_abi_version
eix_plugin_create
eix_plugin_dispatch
eix_plugin_free
eix_plugin_destroy
```

Everything else is carried over the bidirectional `dispatch` method table, so
new contributions and host methods do not change the ABI. P1 only wires
`describe` / `tool.call` / `command.run`; see
[`docs/plugin-dylib.md`](../../../docs/plugin-dylib.md) for the full design,
trust model, and remaining milestones.
