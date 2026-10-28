# Neovim 插件集成

eix 不内置 Vim/Neovim 前端，也不接管 buffer、窗口、键位或 picker。Neovim 插件是独立的
daemon client：它读完本文的协议后，自己决定怎么渲染和交互。

如果你只想快速验证接口，先启动 daemon：

```sh
eix status --json
```

输出里的 `daemon.endpoint` 就是当前 socket 路径（TCP 时是 `tcp://...`）。插件也可以读取
`EIX_SOCKET`，或让用户显式传入 `--socket`。使用 TCP 时对应 `EIX_TCP` / `EIX_TCP_TOKEN`。

## 传输

推荐直接连 Unix socket，使用换行分隔的 JSON-RPC：

- 默认 socket 由 `eix_core::config::socket_path()` 决定：
  `$XDG_RUNTIME_DIR/eix.sock`；macOS 回退到 `~/Library/Caches/eix/eix.sock`。
- 每行一个 JSON 对象，UTF-8 编码；不要发送多行 JSON。
- Unix socket 视为已认证（靠文件系统权限保护）。

需要跨机器时使用 TCP：

```toml
[server]
tcp = "127.0.0.1:7777"
token = "..."
```

TCP 连接的第一个请求必须是：

```json
{"id":1,"method":"auth","params":{"token":"..."}}
```

认证成功后才返回其它响应和事件；失败返回 `unauthorized` 并断开。token 可以是配置里的
`server.token`，也可以是用 `eix pair` 生成的一次性码兑换来的客户端 token（daemon 只保存其
SHA-256，`eix pair list` / `revoke` 管理）。HTTP/SSE 是另一条可选的
集成路径，见 [DESIGN.md §19](../DESIGN.md#196-neovim-插件集成)。

## 消息格式

请求由客户端发出：

```json
{"id":1,"method":"session.list","params":{}}
```

响应总是带回同一个 `id`：

```json
{"id":1,"result":{"sessions":[]}}
{"id":1,"error":{"code":"bad_request","message":"..."}}
```

通知由 daemon 主动推送，没有 `id`：

```json
{"method":"session.event","params":{"session_id":"...","seq":12,"ts":1730000000000,"kind":"assistant_message","payload":{"text":"...","model":"..."}}}
```

`id` 只需在单条连接内单调递增。收到不认识的 notification 时忽略即可；收到 `stream.lagged`
说明事件队列溢出，插件应通过 `session.events` 拉取缺失的 `seq` 再继续。

## 最小调用顺序

```text
connect
  -> hello
  -> session.list / session.create
  -> session.subscribe {session_ids: [id]}
  -> prompt.submit {session_id, text}
  -> 读 session.event / message.delta / tool.* / turn.completed
  -> prompt.cancel / approval.respond / input.respond（按需）
```

`session.subscribe` 把广播收窄到指定 session；省略 `session_ids`（或传 `{}`）表示恢复全部，
传空数组会静默广播。多 pane 插件应为每个连接分别订阅，不要把多个 session 的事件混在一条连接里。

## 常用请求

| method | params | 说明 |
| --- | --- | --- |
| `hello` | `null` | 协议版本、daemon 版本、模型等 |
| `session.list` | `{include_archived?}` | 列出 session |
| `session.create` | `{name?, cwd?, model?, worktree?, mode?, agent?}` | 新建 session |
| `session.get` | `{session_id}` | 单个 session 详情 |
| `session.subscribe` | `{session_ids?: string[] \| null}` | 收窄/恢复广播 |
| `session.events` | `{session_id, after_seq?, before_seq?, limit?}` | 分页读取持久化事件，用于重连/补洞 |
| `prompt.submit` | `{session_id, text, images?}` | 提交一轮 prompt |
| `prompt.cancel` | `{session_id}` | 取消当前 turn |
| `approval.respond` | `{session_id, approval_id, decision, reason?, hunks?, scope?}` | 审批；`decision` 为 `approve` / `deny` / `always` |
| `input.respond` | `{session_id, input_id, option_index}` | 回答模型的提问 |
| `session.mode` | `{session_id, mode, temporary?}` | 切换 plan/build/auto/full |
| `draft.get` / `draft.set` | `{session_id}` / `{session_id, text}` | 读写未发送输入 |
| `process.list` / `process.output` / `process.kill` | `{session_id?}` / `{process_id, ...}` / `{process_id}` | 后台进程 |
| `picker.query` / `picker.preview` | `{kind, session_id?, query?, limit?}` / `{kind, session_id?, value, limit?}` | 文件、git、session、context 选择器数据 |

完整参数以 [`crates/eix-core/src/protocol.rs`](../crates/eix-core/src/protocol.rs) 为准。

## 常用通知

| method | 说明 |
| --- | --- |
| `turn.started` | 一轮开始，带 `session_id` / `turn_id` / `model` |
| `message.delta` | 流式正文，带 `delta` |
| `reasoning.delta` | 流式 reasoning，显示用，不要写回上下文 |
| `message.reset` | 正文需要清空重放 |
| `tool.started` / `tool.completed` | 工具调用生命周期 |
| `tool.approval_required` | 需要审批；用 `approval.respond` 回复 |
| `input.required` | 模型提问；用 `input.respond` 回复 |
| `turn.completed` | 一轮结束，带 `cancelled` / `error` / `usage` / `cost` |
| `session.event` | 持久化事件（transcript 的权威来源），`kind` 为 snake_case |
| `session.updated` | session 元数据或状态变化 |
| `process.output` / `process.exited` | 后台进程输出与退出 |
| `integration.updated` | worktree 集成队列变化 |
| `stream.lagged` | 广播丢弃了事件，需要补拉 |
| `config.error` | 配置热重载失败 |

`session.event.params.kind` 常见值：`user_message`、`assistant_message`、`tool_call`、
`tool_result`、`patch_proposed`、`patch_applied`、`patch_rejected`、`approval_required`、
`input_required`、`input_answered`、`process_started`、`process_exited`、`status_changed`、
`context_compacted`、`mode`、`error`。事件 payload 的类型定义在同一 `event.rs` 中。

插件渲染建议：先用 `session.events` 建完整 transcript，再用 `session.event` 追加，最后用
`message.delta` 做流式刷新。只依赖 `message.delta` 会在断线重连后丢历史。

## 最小 Lua 传输层

下面只是连接和收发，不包含 UI。把它放进插件的 `lua/eix/rpc.lua`，再在上层实现 buffer、
window、keymap 和 picker。

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

使用：

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

TCP 版本把 `uv.new_pipe` 换成 `uv.new_tcp`，并在连接成功后先发 `auth`：

```lua
rpc:request("auth", { token = token }, function(response)
  assert(response.result and response.result.authenticated)
end)
```

## 注意事项

- 同一 session 同时只应有一个 turn；重复 `prompt.submit` 会返回 busy 错误，插件应排队或提示。
- 每个 Neovim 进程是独立 client；多个窗口共享同一 daemon，不要假设自己是唯一写入者。
- 不要在 libuv 回调里直接调用 `vim.api.nvim_*`；用 `vim.schedule` 切回主循环（上面的示例已处理）。
- 插件关闭时关闭 pipe；重连后重新 `hello` + `session.subscribe` + `session.events` 补历史。
- 协议的权威契约是 `crates/eix-core/src/protocol.rs` 和 `PROTOCOL_VERSION`，不要依赖渲染文本。
