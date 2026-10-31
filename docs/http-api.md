# HTTP API and SDK

Besides its Unix socket, the daemon can listen on TCP and HTTP. Frontends (TUI, CLI, Neovim,
third-party scripts) all speak the same JSON-RPC methods, defined in
`crates/eix-core/src/protocol.rs`.

## 1. Enabling the listeners

```toml
[server]
tcp = "127.0.0.1:7420"   # newline-delimited JSON-RPC
http = "127.0.0.1:7421"   # JSON-RPC + SSE
token = "<shared-secret>"
```

Whenever `tcp` or `http` is non-empty, `token` is required. An empty string disables the
corresponding listener.

Connecting a client:

```bash
eix --tcp 127.0.0.1:7421 --token "$EIX_TCP_TOKEN"
```

The equivalent environment variables are `EIX_TCP` and `EIX_TCP_TOKEN`.

## 2. Endpoints

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| `GET` | `/health` | no | Returns `{{"ok":true,"version":"…"}}`; use it for liveness checks |
| `GET` | `/openapi.json` | no | OpenAPI document, ready for code generators |
| `POST` | `/pair` | no (exchanges a one-time code for a token) | Pairing |
| `GET` | `/stats` | yes | Aggregate statistics |
| `GET` | `/events` | yes | SSE event stream |
| `POST` | `/rpc` | yes | Every JSON-RPC call |
| `POST` | `/asr/<utterance>/pcm` | yes | Speech chunk upload, see [Voice input](./voice.md) |

Two headers authenticate a request: `Authorization: Bearer <token>` or `X-Eix-Token`.

`/pair` is the only write endpoint that is deliberately unauthenticated: the one-time pairing code
is itself the credential, so a client does not need the token first. Its request body is read
before the auth check.

## 3. One RPC call

```bash
curl -s http://127.0.0.1:7421/rpc \
  -H "Authorization: Bearer $EIX_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.list","params":{}}'
```

Responses follow JSON-RPC 2.0: success carries `result`, failure carries `error` with a `code`.
Method names and parameter shapes are authoritative in `crates/eix-core/src/protocol.rs`, and
`/openapi.json` lists the same set.

## 4. Subscribing to events

`GET /events` returns `text/event-stream`. Repeat the `session_id` query parameter to filter;
without it you receive every event.

```bash
curl -N http://127.0.0.1:7421/events?session_id=s-123 \
  -H "Authorization: Bearer $EIX_TOKEN"
```

Filter semantics: events for sessions in the list arrive, and so do global notifications. Frames
are separated by a blank line and a single frame may be split across TCP segments, so parse on
frame boundaries, never on chunk boundaries.

## 5. Rust SDK

`crates/eix-sdk` is a thin wrapper over this HTTP surface. After `Client::new(base, token)`:

| Method | Purpose |
| --- | --- |
| `health()` | Liveness |
| `stats()` | Statistics |
| `sessions(include_archived)` | Session list |
| `create_session(&params)` | Create a session |
| `submit(&params)` | Submit one user turn |
| `cancel(session_id)` | Cancel the current turn |
| `approve(&params)` | Answer an approval request |
| `request(method, params)` | Any method, returning `Value` |
| `request_typed::<P, T>(…)` | The same call, typed |
| `events(&[session_id])` | Returns an `EventStream` |

`Client` is cheap to clone and shares a request counter and connection pool internally.
`EventStream` is an async stream that yields parsed events frame by frame; it buffers raw bytes
until a frame boundary is complete, so a UTF-8 character split across segments is never truncated.

Errors fall into three cases: `Unauthorized` (401), `Status { status, body }` (any other non-2xx),
and `Rpc { code, message }` (protocol-level failure). An incomplete frame yields
`IncompleteEvent`, which usually means the connection closed mid-frame.

For the TypeScript client, see `clients/ts/README.md`.

## 6. Security

- `token` is the only credential and grants full control of the daemon: it can approve writes and
  read every session. Do not ship it in frontend code or shared scripts.
- Listeners bind to `127.0.0.1` by default. For cross-machine access put the daemon behind an SSH
  tunnel or a reverse proxy; never expose it directly to the internet.
- After `/pair` issues a token, older tokens stay valid. Rotate by changing the config and
  restarting the daemon.
