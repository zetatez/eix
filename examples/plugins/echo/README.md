# echo plugin

A tiny eix server plugin written with the Python standard library. It shows the
complete lifecycle: `initialize`, model tools, a prompt command, event
subscriptions and host-managed storage.

Run it from the repository root with this global config fragment:

```yaml
plugins:
  echo:
    enabled: true
    manifest: examples/plugins/echo/eix-plugin.toml
    capabilities: [tools, commands, events, storage]
    timeout_secs: 30
```

`manifest` is resolved from the process cwd when it is relative. You can also
set `command` / `args` directly and omit the manifest; in that case the config
grant defaults to `[tools, commands]` unless `capabilities` is set.

After `/reload` (or a daemon restart), `/` lists `plugin/echo/review`. The model
sees `plugin__echo__word_count`, `plugin__echo__remember` and
`plugin__echo__recall`. Plugin tool calls default to approval and are hidden
from plan sessions and sub-agents. `session.*` notifications are printed to
stderr. The daemon's best-effort `shutdown` notification makes the example exit
cleanly when eixd stops.

See the "插件" section in the repository README for the protocol and security
model.
