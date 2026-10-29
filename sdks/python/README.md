# eix Python plugin SDK

Dependency-free Python binding for eix server plugins (API v2). It uses only the
standard library and runs on Python 3.9+.

```python
from eix_plugin import Plugin, text

plugin = Plugin("hello", version="0.1.0")


@plugin.tool(
    "echo",
    description="Echo a string.",
    input_schema={"type": "object", "properties": {"text": {"type": "string"}}},
)
def echo(arguments, ctx):
    return text(arguments.get("text", ""))


if __name__ == "__main__":
    plugin.run()
```

## Registration

| Method | Purpose |
| --- | --- |
| `@plugin.tool(name, description=…, input_schema=…, read_only=…)` | Model tool; handler `(arguments, ctx)` |
| `@plugin.command(name, action=…, prompt=…)` | Action command (`commands/run`) or prompt-only command |
| `@plugin.hook(point, priority=…, timeout_ms=…, on_error=…)` | Hook handler `(params, ctx)` |
| `plugin.event(pattern)` / `@plugin.on_event(pattern)` | Subscribe to daemon notifications |
| `plugin.ui(id, surface=…, render=…, action=…, pick=…)` | Declare a UI surface and its handlers |
| `@plugin.rpc(name)` | Expose a plugin RPC method |

Handlers may return a string, a content-block list, or a full result dict.
`text`, `image`, `resource`, `diff`, `json_block` and `tool_result` build the
wire shapes.

## Context

`ctx.host(method, params)` calls a host API and waits for the response.
Convenience wrappers: `ctx.log`, `ctx.notify`, `ctx.storage_get`,
`ctx.storage_set`. While a handler waits on the host, incoming requests are
deferred and processed afterwards, so the loop stays responsive without
threads.

## Test

```bash
python3 sdks/python/tests/test_conformance.py
python3 sdks/conformance/run.py -- python3 sdks/python/examples/echo.py
```

See [`sdks/README.md`](../README.md) for the shared conformance harness.
