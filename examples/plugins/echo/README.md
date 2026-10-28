# echo plugin

A tiny eix server plugin written with the Python standard library. It shows the
complete plugin API v2 lifecycle: JSON-RPC 2.0 `initialize`, model tools with
content blocks, runtime and declarative prompt commands, event subscriptions
and host-managed storage.

Run it from the repository root with this global config fragment:

```toml
[plugins.echo]
enabled = true
manifest = "examples/plugins/echo/eix-plugin.toml"
timeout_secs = 30
```

`manifest` is resolved from the config cwd when it is relative. The manifest's
`capabilities` are the request; config `capabilities` can only narrow that set.
You can also set `command` / `args` directly and omit the manifest; in that
case the config grant defaults to `[tools, commands]` unless `capabilities` is
set.

After `/reload` (or a daemon restart), `/` lists `plugin/echo/review` (runtime)
and `plugin/echo/triage` (declarative, from `eix-plugin.toml`). The model sees
`plugin__echo__word_count`, `plugin__echo__remember` and
`plugin__echo__recall`. Plugin tool calls default to approval and are hidden
from plan sessions and sub-agents. `session.*` notifications are printed to
stderr. The daemon's best-effort `shutdown` notification makes the example exit
cleanly when eixd stops.

The full protocol, capability list and host API are documented in
[`docs/plugins.md`](../../../docs/plugins.md).
