# Neovim integration

eix ships no Vim/Neovim frontend and does not own buffers, windows, keymaps, or pickers. A Neovim
plugin is an independent daemon client: it learns the protocol from this page and decides on
rendering and interaction for itself.

To sanity-check the interface quickly, start the daemon first:

```sh
eix status --json
```

`daemon.endpoint` in that output is the current socket path (`tcp://...` when using TCP). A plugin
can also read `EIX_SOCKET`, or take an explicit `--socket` from the user. With TCP, use `EIX_TCP`
and `EIX_TCP_TOKEN`.

## Transport

Connecting straight to the Unix socket with newline-delimited JSON-RPC is the recommended path:

- The default socket comes from `eix_core::config::socket_path()`: `$XDG_RUNTIME_DIR/eix.sock`,
  with a macOS fallback to `~/Library/Caches/eix/eix.sock`.
- One JSON object per line, UTF-8 encoded. Never send multi-line JSON.
- The Unix socket counts as authenticated; filesystem permissions are the boundary.

For cross-machine use, TCP:

```toml
[server]
tcp = "127.0.0.1:7777"
token = "..."
```

The first request on a TCP connection must be:

```json
{"id":1,"method":"auth","params":{"token":"..."}}
```

Only after successful authentication do other responses and events flow; failure returns
`unauthorized` and disconnects. The token can be the configured `server.token`, or a client token
minted by exchanging a one-time `eix pair` code (the daemon stores only its SHA-256 and manages
tokens with `eix pair list` / `revoke`). HTTP/SSE is an alternative integration path; see
[DESIGN.md §19](../DESIGN.md#196-neovim-插件集成).

## Message format

Requests are sent by the client:

```json
{"id":1,"method":"session.list","params":{}}
```

Responses always echo the same `id`:

```json
{"id":1,"result":{"sessions":[]}}
{"id":1,"error":{"code":"bad_request","message":"..."}}
```

Notifications are pushed by the daemon and carry no `id`:

```json
{"method":"session.event","params":{"session_id":"...","seq":12,"ts":1730000000000,"kind":"assistant_message","payload":{"text":"...","model":"..."}}}
```

`id` only has to increase monotonically within one connection. Ignore notifications you do not
recognise; `stream.lagged` means the event queue overflowed, and the plugin should fetch the
missing `seq` values through `session.events` before continuing.

## Minimal call sequence

```text
connect
  -> hello
  -> session.list / session.create
  -> session.subscribe {session_ids: [id]}
  -> prompt.submit {session_id, text}
  -> read session.event / message.delta / tool.* / turn.completed
  -> prompt.cancel / approval.respond / input.respond (as needed)
```

`session.subscribe` narrows the broadcast to the listed sessions. Omitting `session_ids` (or
passing `{}`) restores the full broadcast, while an empty array silently broadcasts everything.
A multi-pane plugin should subscribe per connection rather than mixing several sessions into one
connection.

## Common requests

| method | params | Purpose |
| --- | --- | --- |
| `hello` | `null` | Protocol version, daemon version, models |
| `session.list` | `{include_archived?}` | List sessions |
| `session.create` | `{name?, cwd?, model?, worktree?, mode?, agent?}` | Create a session |
| `session.get` | `{session_id}` | Details for one session |
| `session.subscribe` | `{session_ids?: string[] \| null}` | Narrow or restore the broadcast |
| `session.events` | `{session_id, after_seq?, before_seq?, limit?}` | Page through persisted events; used for reconnects and gap filling |
| `prompt.submit` | `{session_id, text, images?}` | Submit a prompt; if a turn is already running it is queued and the reply carries `queued: true` with an empty `turn_id` |
| `prompt.cancel` | `{session_id}` | Cancel the current turn and clear that session's queued prompts |
| `approval.respond` | `{session_id, approval_id, decision, reason?, hunks?, scope?}` | Answer an approval; `decision` is `approve` / `deny` / `always` |
| `input.respond` | `{session_id, input_id, option_index}` | Answer a question the model asked |
| `session.mode` | `{session_id, mode, temporary?}` | Switch between plan/build/auto/full |
| `draft.get` / `draft.set` | `{session_id}` / `{session_id, text}` | Read or write unsent input |
| `process.list` / `process.output` / `process.kill` | `{session_id?}` / `{process_id, $EIX_TCP_TOKEN}` / `{process_id}` | Background processes |
| `picker.query` / `picker.preview` | `{kind, session_id?, query?, limit?}` / `{kind, session_id?, value, limit?}` | Picker data for files, images, git, sessions, context |
| `mcp.list` / `mcp.set_enabled` | `null` / `{name, enabled}` | List MCP servers, or enable/disable one and persist it to config |

Full parameter definitions are authoritative in
[`crates/eix-core/src/protocol.rs`](../crates/eix-core/src/protocol.rs).

## Common notifications

| method | Purpose |
| --- | --- |
| `turn.started` | A turn began, with `session_id` / `turn_id` / `model` |
| `message.delta` | Streaming body text, carrying `delta` |
| `reasoning.delta` | Streaming reasoning, for display only; never write it back into context |
| `message.reset` | The body must be cleared and replayed |
| `tool.started` / `tool.completed` | Tool-call lifecycle |
| `tool.approval_required` | An approval is needed; answer with `approval.respond` |
| `input.required` | The model asked a question; answer with `input.respond` |
| `turn.completed` | The turn ended, with `cancelled` / `error` / `usage` / `cost` / `rounds` |
| `prompt.queued` | The prompt was queued, with `session_id` / `position` (1-based) |
| `prompt.failed` | A queued prompt could not start, with `session_id` / `error`; it is never dropped silently |
| `prompt.cancelled` | The queue was cleared by `prompt.cancel`, with `session_id` / `dropped` (count) |
| `session.event` | A persisted event, the authoritative transcript source; `kind` is snake_case |
| `session.updated` | Session metadata or state changed |
| `process.output` / `process.exited` | Background process output and exit |
| `integration.updated` | The worktree integration queue changed |
| `stream.lagged` | The broadcast dropped events; backfill before continuing |
| `config.error` | A config hot reload failed |

Common `session.event.params.kind` values: `user_message`, `assistant_message`, `tool_call`,
`tool_result`, `patch_proposed`, `patch_applied`, `patch_rejected`, `approval_required`,
`input_required`, `input_answered`, `process_started`, `process_exited`, `status_changed`,
`context_compacted`, `mode`, `error`. Payload types are defined in the same `event.rs`.

Rendering advice: build the full transcript from `session.events`, append with `session.event`,
then use `message.delta` for streaming refresh. Relying on `message.delta` alone loses history
after a reconnect.

## Minimal Lua transport

This is transport only, with no UI. Put it in the plugin's `lua/eix/rpc.lua` and build buffers,
windows, keymaps, and pickers on top.

```lua
local uv = vim.uv or vim.loop

local Client = {}
Client.__index = Client

function Client.connect(path, on_notification)
  local self = setmetatable({
    next_id = 0,
    pending = {},
    queue = {},
    buffer = "",
    connected = false,
  }, Client)
  self.pipe = uv.new_pipe(false)

  self.pipe:connect(path, function(err)
    if err then
      vim.schedule(function()
        vim.notify("eix: " .. tostring(err), vim.log.levels.ERROR)
      end)
      return
    end

    -- Requests made before the socket was ready are replayed in order.
    self.connected = true
    for _, line in ipairs(self.queue) do
      self.pipe:write(line)
    end
    self.queue = {}

    self.pipe:read_start(function(read_err, chunk)
      if read_err then
        vim.schedule(function()
          vim.notify("eix: " .. tostring(read_err), vim.log.levels.ERROR)
        end)
        return
      end
      if not chunk then return end

      self.buffer = self.buffer .. chunk
      while true do
        local newline = self.buffer:find("\n", 1, true)
        if not newline then break end

        local line = self.buffer:sub(1, newline - 1)
        self.buffer = self.buffer:sub(newline + 1)
        if line ~= "" then
          local message = vim.json.decode(line)
          local callback = message.id and self.pending[message.id]
          if callback then
            self.pending[message.id] = nil
            vim.schedule(function() callback(message) end)
          else
            vim.schedule(function() on_notification(message) end)
          end
        end
      end
    end)
  end)

  return self
end

function Client:request(method, params, callback)
  self.next_id = self.next_id + 1
  local id = self.next_id
  self.pending[id] = callback or function() end
  local line = vim.json.encode({
    id = id,
    method = method,
    params = params or vim.empty_dict(),
  }) .. "\n"
  if self.connected then
    self.pipe:write(line)
  else
    self.queue[#self.queue + 1] = line
  end
  return id
end

return Client
```

Using it:

```lua
local Rpc = require("eix.rpc")

local rpc = Rpc.connect(socket_path, function(message)
  if message.method == "session.event" then
    -- message.params.kind / payload
  elseif message.method == "message.delta" then
    -- message.params.delta
  end
end)

rpc:request("hello", nil, function(response)
  assert(not response.error, response.error and response.error.message)
end)
rpc:request("session.subscribe", { session_ids = { session_id } })
rpc:request("prompt.submit", { session_id = session_id, text = "hello" })
```

The TCP variant swaps `uv.new_pipe` for `uv.new_tcp` and sends `auth` first:

```lua
rpc:request("auth", { token = token }, function(response)
  assert(response.result and response.result.authenticated)
end)
```

## Things to watch

- A session should have only one turn at a time. Submitting a prompt while a turn runs is queued by
  the daemon, which replies with `queued: true` and an empty `turn_id`. The plugin does not
  need its own queue, but it should handle the `prompt.queued` / `prompt.failed` / `prompt.cancelled`
  notifications when updating the UI.
- Every Neovim process is a separate client. Several windows share one daemon, so never assume you
  are the only writer.
- Do not call `vim.api.nvim_*` directly from a libuv callback; switch back to the main loop with
 `vim.schedule` (the example above already does this).
- Close the pipe when the plugin shuts down. After reconnecting, call `hello` again, then
 `session.subscribe`, then backfill history through `session.events`.
- The authoritative contract is `crates/eix-core/src/protocol.rs` together with
 `PROTOCOL_VERSION`; never depend on rendered text.
