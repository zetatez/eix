# eix plugin SDKs

Language bindings for [eix server plugins](../docs/plugins.md) (plugin API v2).
All SDKs are dependency-free relative to their language and speak JSON-RPC 2.0
over NDJSON on stdin/stdout.

| SDK | Entry point | Test |
| --- | --- | --- |
| Rust | `crates/eix-plugin-sdk` | `cargo test -p eix-plugin-sdk` |
| Python | `sdks/python/eix_plugin.py` | `python3 sdks/python/tests/test_conformance.py` |
| TypeScript / JavaScript | `sdks/typescript/index.js` + `index.d.ts` | `node sdks/typescript/test/conformance.mjs` |

The shared harness exercises a spawned plugin end to end:

```bash
python3 sdks/conformance/run.py -- python3 sdks/python/examples/echo.py
python3 sdks/conformance/run.py -- node sdks/typescript/examples/echo.mjs
```

It checks `initialize`, `tools/call`, `commands/run`, `hooks/run`,
`ui/render`, answers plugin-initiated `host.*` requests, and requires a clean
`shutdown` exit. Point it at any package command to validate a custom SDK or a
plugin written by hand.

## ABI / protocol notes

- Handshake: `initialize` with `protocol_version` and `api_version` both `2`;
  the result advertises tools, commands, events, hooks, UI surfaces and RPC
  methods.
- Request ids are matched per direction. The daemon allocates from `1`; the
  Python and TypeScript SDKs allocate host-request ids from `1 << 32` so a
  simultaneous request in each direction cannot be mistaken for its response.
- Notifications (events, `$/cancel`, `shutdown`) have no `id`.
- Tool handlers return a full result dict, a content-block list, or a string;
  the SDKs normalize the latter two.
- `shutdown` must terminate the process; the daemon treats a non-zero exit as a
  crash.
