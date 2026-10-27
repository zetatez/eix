# eix TypeScript client

Dependency-free, typed client for the eix daemon's HTTP API. Works in Node 20+,
Bun, Deno and browsers (`fetch`, `ReadableStream`, `AbortSignal`).

The daemon must expose HTTP:

```yaml
server:
  http: 127.0.0.1:7778
  token: "{env:EIX_TOKEN}"
```

```sh
npm run build        # emit dist/
npm run typecheck    # tsc --noEmit
```

The package is not published yet; depend on it with a local path or copy
`src/index.ts` into your project.

## Pairing

A browser or another machine should not copy the daemon's static token. Mint a
one-time code locally and redeem it over HTTP:

```sh
eix pair --name laptop
# pairing code: 1a2b-3c4d-5e6f-7a8b
# expires: 2026-10-28 12:34:56 (5 minutes, single use)
```

```ts
import { EixClient } from "eix-client";

const anonymous = new EixClient({ baseUrl: "http://127.0.0.1:7778" });
const { token, client } = await anonymous.pair("1a2b-3c4d-5e6f-7a8b", "laptop");

const eix = anonymous.withToken(token);
await eix.stats();
```

Codes are case- and dash-insensitive, valid for five minutes, and single use.
The daemon stores only the token's SHA-256 hash and remembers the label.
Manage clients with `eix pair list` and `eix pair revoke <id>`; revocation takes
effect immediately. Failed redemptions are rate limited (20 per minute).

## Sessions and turns

```ts
const session = await eix.createSession({
  name: "web",
  cwd: "/home/me/project",
  worktree: true,
});

// One call that streams deltas until the turn completes.
for await (const note of eix.promptStream(session.id, "fix the failing tests")) {
  if (note.method === "message.delta") process.stdout.write(note.params.delta);
  if (note.method === "reasoning.delta") process.stderr.write(note.params.delta);
}
```

`promptStream` opens `/events` before submitting the prompt, so no delta is
lost. For approvals, questions or multiple concurrent turns, drive one event
stream yourself:

```ts
for await (const note of eix.events({ sessionId: [session.id] })) {
  switch (note.method) {
    case "tool.approval_required": {
      const approval = note.params;
      if (approval.allow_always && approval.risk === "low") {
        await eix.always(session.id, approval.approval_id, "session");
      } else {
        await eix.deny(session.id, approval.approval_id, "not this time");
      }
      break;
    }
    case "input.required":
      await eix.answer(session.id, note.params.input_id, 0);
      break;
    case "turn.completed":
      console.log("done", note.params.cancelled ? "cancelled" : "ok");
      break;
  }
}
```

`events()` is an async generator: the HTTP connection opens on the first
`next()`, so start consuming before you emit the event you are waiting for.

Approving a staged write can apply selected hunks:
`eix.approve(session.id, approvalId, { hunks: [0, 2] })`.

Remote clients (anything authenticated with a token, including paired clients)
are contained by the daemon's workspace: `createSession` with a `cwd` outside
that directory fails with `forbidden`. Omit `cwd` to use the daemon's default.

## API surface

| Method | Route / RPC |
| --- | --- |
| `health()` / `stats()` / `openapi()` | `GET /health`, `/stats`, `/openapi.json` |
| `pair(code, name?)` | `POST /pair` |
| `events(options)` | `GET /events` (SSE, optional `session_id` filter) |
| `rpc(method, params)` | `POST /rpc` |
| `sessions()` / `createSession()` / `attach()` / `rename()` | `session.*` |
| `prompt()` / `promptStream()` / `cancel()` / `setMode()` | `prompt.*`, `session.mode` |
| `approvals()` / `approve()` / `always()` / `deny()` | `approval.*` |
| `answer()` | `input.respond` |

`rpc()` is the escape hatch for the rest of the daemon protocol; see
[`docs/nvim.md`](../../docs/nvim.md) for the request/notification shapes.

## Errors

Non-2xx responses and JSON-RPC errors throw `EixError` with `status` and `code`
(e.g. `unauthorized`, `forbidden`, `rate_limited`, `not_found`). A paired token
that is revoked starts failing with `401 unauthorized`.
