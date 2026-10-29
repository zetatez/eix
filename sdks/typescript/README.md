# eix TypeScript / JavaScript plugin SDK

Dependency-free ESM binding for eix server plugins (API v2). `index.d.ts`
provides the TypeScript types; `index.js` is the runtime, so there is no build
step for the SDK itself.

```js
import { Plugin, text } from "./index.js";

const plugin = new Plugin({ name: "hello", version: "0.1.0" });

plugin.tool(
  "echo",
  {
    description: "Echo a string.",
    inputSchema: { type: "object", properties: { text: { type: "string" } } },
  },
  (arguments) => text(arguments.text ?? ""),
);

await plugin.run();
```

## Registration

| Method | Purpose |
| --- | --- |
| `plugin.tool(name, options, handler)` | Model tool; handler `(arguments, ctx)`, sync or async |
| `plugin.command(name, options, handler)` | Action command (`action`) or prompt-only (`prompt`) |
| `plugin.hook(point, handler, options)` | Hook handler `(params, ctx)` |
| `plugin.event(pattern)` / `plugin.onEvent(pattern, handler)` | Subscribe to daemon notifications |
| `plugin.ui({ id, title, surface, render, action, pick })` | Declare a UI surface and handlers |
| `plugin.rpc(name, options, handler)` | Expose a plugin RPC method |

Handlers may return a string, a content-block list, or a full result dict.
`text`, `image`, `resource`, `diff`, `jsonBlock` and `toolResult` build the wire
shapes.

## Context

`ctx.host(method, params)` returns a promise for the host result; incoming
requests continue to be served while it is pending. Convenience wrappers:
`ctx.log`, `ctx.notify`, `ctx.storageGet`, `ctx.storageSet`.

## Type checking

Point your `tsconfig.json` at the bundled declarations:

```json
{
  "compilerOptions": {
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "target": "ES2022",
    "strict": true
  }
}
```

Import from the package root (`"eix-plugin"`) or the local `index.js`.

## Test

```bash
node sdks/typescript/test/conformance.mjs
python3 sdks/conformance/run.py -- node sdks/typescript/examples/echo.mjs
```

See [`sdks/README.md`](../README.md) for the shared conformance harness.
