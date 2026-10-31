# MCP（Model Context Protocol）

MCP 让 eix 把外部进程或远端服务提供的工具、资源、提示词接进模型。每个 server 是一个独立条目，
启动时连接、列出能力，之后按需调用。

相关配置在 `[mcp.servers.<name>]`（见 `config.tmpl.toml`），实现位于 `crates/eixd/src/mcp.rs`。

## 1. 两种传输

### 本地 stdio

设置 `command` 和 `args`，eix 直接 `spawn` 该程序（**不经过 shell**），通过 stdin/stdout 交换
按行分隔的 JSON-RPC。子进程随 client 一起销毁。

```toml
[mcp.servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
```

因为不经过 shell，管道、重定向、`&&` 都不会生效；需要什么就写进 `args`。

### 远端 Streamable HTTP

设置 `url`（与 `command` 互斥）。eix 使用 MCP 的 Streamable HTTP 传输：请求可能返回 JSON，
也可能返回 SSE 流；session id 和协议版本在响应头里带回，后续请求继续携带。

```toml
[mcp.servers.docs]
url = "https://mcp.example.com/mcp"
timeout_secs = 30
```

## 2. 字段速查

| 字段 | 默认 | 说明 |
|---|---|---|
| `command` / `args` | 空 | 本地 server 的可执行文件与参数，不经 shell |
| `env` | 空 | 追加给子进程的环境变量 |
| `url` | 无 | 远端 endpoint；设置后忽略 `command` |
| `headers` | 空 | 仅远端；值支持 `{env:NAME}` / `{file:path}` 占位符 |
| `oauth` | 无 | 远端 OAuth 客户端设置，见第 6 节 |
| `enabled` | `true` | 设为 `false` 可保留条目但不连接 |
| `timeout_secs` | `30` | `tools/list` 与 `tools/call` 的单次超时 |
| `allowed_tools` | 空 | 只暴露匹配的工具名；空表示全部 |
| `denied_tools` | 空 | 隐藏匹配的工具名，**优先于** `allowed_tools` |

工具名过滤支持 `*` 和 `?` 通配。这里的匹配对象是 server 自己声明的工具名，不是暴露名；
`*` 可以跨越 `/` 等字符，因为工具名不是路径。

```toml
[mcp.servers.docs.headers]
Authorization = "Bearer {env:MCP_TOKEN}"
```

`allowed_tools` 与 `denied_tools` 是数组，写在同一个 server 表里，不要开子表：

```toml
[mcp.servers.docs]
url = "https://mcp.example.com/mcp"
allowed_tools = ["search_*", "read_*"]
denied_tools = ["write_*"]
```

## 3. 工具如何暴露给模型

模型看到的工具名是 `mcp__<server>__<tool>`，例如 `mcp__files__read`。server 名和工具名里
除字母、数字、`_`、`-` 之外的字符都会替换成 `_`。

因此：

- 改 `[mcp.servers.<name>]` 的键名会改变模型看到的工具名。
- `[[tools.rules]]` 里匹配 MCP 工具时，用暴露名，例如 `tool = "mcp__docs__search"`，
  或用 `mcp__*` 通配。
- 工具输出会被压平成文本：多个 `content` 片段按顺序拼接，`isError` 为真时同样返回文本。
  遇到无法识别的结构则回退成原始 JSON。

server 声明的工具在被过滤后才会出现在请求里，所以 `allowed_tools`/`denied_tools` 是真正的
“不可见”，而不是“可见但拒调用”。

## 4. 资源与提示词

除工具外，MCP 还提供资源和提示词。它们不直接变成模型工具，而是通过几个固定工具读取：

| 工具 | 作用 |
|---|---|
| `mcp_list_resources` | 列出资源（可按 server 过滤） |
| `mcp_list_resource_templates` | 列出 RFC 6570 资源模板 |
| `mcp_read_resource` | 读取某个 URI |
| `mcp_list_prompts` | 列出提示词模板 |
| `mcp_get_prompt` | 取回提示词（含参数） |

资源模板需要调用方自己展开 `{var}` 后再用 `mcp_read_resource` 读取。

## 5. 观测

`mcp.list` RPC 返回每个 server 的状态：名称、`enabled`、工具数、资源数、提示词数，以及连接
失败时的错误文本。TUI 和 CLI 都通过它展示健康状态。

连接失败不会拖垮会话：该 server 被跳过，错误留在状态里，其余 server 照常工作。

## 6. OAuth

远端 server 需要 OAuth 时，先声明 `[mcp.servers.<name>.oauth]`，再运行
`eix mcp auth <name>` 走浏览器流程；`eix mcp logout` 清除凭据。token 存在数据目录的
`oauth/mcp` 下。

| 字段 | 行为 |
|---|---|
| `client_id` | 留空则动态注册客户端 |
| `client_secret` | 机密客户端才需要；公共客户端省略 |
| `scopes` | 留空时用服务端 `WWW-Authenticate` 里的 scope；再没有就不发 scope 参数 |

有存储的 token 时，请求自动带上 Bearer 头，无需在 `headers` 里重复写。

```toml
[mcp.servers.docs.oauth]
scopes = ["read"]
# client_id = "..."        # 可选
# client_secret = "..."    # 可选
```

## 7. 常见问题

**工具没出现。** 先看 `mcp.list` 的状态：`error` 非空说明连接失败（命令不存在、URL 不可达、
鉴权被拒）；工具数为 0 说明连上了但 server 没声明工具，或被 `allowed_tools`/`denied_tools`
过滤光了。过滤是 deny 优先，两个列表同时命中时工具被隐藏。

**改了配置没生效。** server 在配置加载或刷新时重建；重开会话或重启 daemon 会重新连接。
旧进程持有的子进程在 client 销毁时才被杀死。

**远端 401。** 先确认 `eix mcp auth <name>` 成功，再检查 `headers` 里是否手写了一个过期
的 `Authorization`，手写头会覆盖或冲突。

**工具名对不上。** 模型看到的是 `mcp__<server>__<tool>`，不是 server 原名的工具名；把配置键
里的点、空格换成 `_` 后再比对。
