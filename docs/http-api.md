# HTTP API 与 SDK

daemon 除了 Unix socket，还能监听 TCP 与 HTTP。前端（TUI、CLI、Neovim、第三方脚本）都走同一套
JSON-RPC 方法，协议定义在 `crates/eix-core/src/protocol.rs`。

## 1. 打开监听

```toml
[server]
tcp = "127.0.0.1:7420"   # 行分隔 JSON-RPC
http = "127.0.0.1:7421"   # JSON-RPC + SSE
token = "<shared-secret>"
```

`tcp` 或 `http` 任一非空时，`token` 是必填项。空字符串表示关闭对应监听。

客户端连接：

```bash
eix --tcp 127.0.0.1:7421 --token "$EIX_TCP_TOKEN"
```

等价的环变量是 `EIX_TCP` 与 `EIX_TCP_TOKEN`。

## 2. 端点

| 方法 | 路径 | 鉴权 | 用途 |
|---|---|---|---|
| `GET` | `/health` | 否 | 返回 `{{"ok":true,"version":"…"}}`，用于探活 |
| `GET` | `/openapi.json` | 否 | OpenAPI 文档，可直接喂给代码生成器 |
| `POST` | `/pair` | 否（用一次性配对码换 token） | 配对 |
| `GET` | `/stats` | 是 | 汇总统计 |
| `GET` | `/events` | 是 | SSE 事件流 |
| `POST` | `/rpc` | 是 | 所有 JSON-RPC 调用 |
| `POST` | `/asr/<utterance>/pcm` | 是 | 语音分片上传，见 `docs/voice.md` |

鉴权接受两种头：`Authorization: Bearer <token>` 或 `x-eix-token: <token>`。

`/pair` 是唯一故意不鉴权的写端点：一次性配对码本身就是凭据，所以客户端不必先把 token
发过来。它的请求体在鉴权检查之前就被读取。

## 3. 一次 RPC

```bash
curl -s http://127.0.0.1:7421/rpc \
  -H "Authorization: Bearer $EIX_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.list","params":{}}'
```

响应遵循 JSON-RPC 2.0：成功带 `result`，失败带 `error` 和 `code`。方法名与参数形态以
`crates/eix-core/src/protocol.rs` 为准，`/openapi.json` 里也有同样的列表。

## 4. 订阅事件

`GET /events` 返回 `text/event-stream`。可用 `session_id` 查询参数重复多次来过滤；不带参数
时收到全部事件。

```bash
curl -N http://127.0.0.1:7421/events?session_id=s-123 \
  -H "Authorization: Bearer $EIX_TOKEN"
```

过滤语义：命中列表的会话事件 + 全局通知都会到达。事件之间以空行分隔，一帧可能被拆到多个
网络分片里，所以按帧边界解析，不要按分片解析。

## 5. Rust SDK

`crates/eix-sdk` 是对上述 HTTP 的薄封装，`Client::new(base, token)` 之后：

| 方法 | 说明 |
|---|---|
| `health()` | 探活 |
| `stats()` | 统计 |
| `sessions(include_archived)` | 会话列表 |
| `create_session(&params)` | 建会话 |
| `submit(&params)` | 提交一轮用户输入 |
| `cancel(session_id)` | 取消当前轮 |
| `approve(&params)` | 回应审批请求 |
| `request(method, params)` | 任意方法，返回 `Value` |
| `request_typed::<P, T>(…)` | 带类型的上面那个 |
| `events(&[session_id])` | 返回 `EventStream` |

`Client` 可自由 clone，内部共享请求计数器与连接池。`EventStream` 是异步流，逐帧产出解析后的
事件；它保留原始字节直到帧边界完整，避免把跨分片的 UTF-8 字符截断。

错误类型区分三种情况：`Unauthorized`（401）、`Status { status, body }`（其他非 2xx）与
`Rpc { code, message }`（协议层错误）。帧不完整会得到 `IncompleteEvent`，通常意味着连接被
中途关闭。

TypeScript 客户端的用法见 `clients/ts/README.md`。

## 6. 安全

- `token` 是唯一凭据，等同于 daemon 的完全控制权：能批准写操作、能读全部会话。不要把它放进
  前端代码或共享脚本。
- 默认只监听 `127.0.0.1`。要跨机器访问时放在 SSH 隧道或反向代理后面，不要直接暴露公网。
- `/pair` 换发 token 后，旧 token 不会自动失效；需要轮换时改配置并重启 daemon。
