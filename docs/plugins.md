# eix 插件体系设计（v2）

- 状态：v2 内核已实现（tools / commands / events / storage / 生命周期）；hooks、UI、RPC、安装分发仍是后续里程碑
- 适用范围：`eixd`（host）、`eix-core`（协议与 manifest）、`eix-tui`（UI 贡献的渲染端）
- 关联实现：`crates/eix-core/src/plugin.rs`、`crates/eixd/src/plugin/`、`crates/eixd/src/server.rs`、`crates/eix-tui/src/tui/app/`

本文描述 eix 插件体系从 v1（外挂工具 + 提示词命令）演进到 v2（贡献点 + hooks + UI + 细粒度授权）的目标架构、协议、安全边界和落地路线。v1 协议与 manifest 已废弃，不保留兼容层。

## 0. 当前实现状态

已落地：

| 能力       | 实现                                                                                                   |
| ---        | ---                                                                                                    |
| Manifest   | `schema = 2`、`api_version = 2`、`[components.server]` / `[components.ui]`、`[[contributes.commands]]` |
| 协议       | JSON-RPC 2.0 over NDJSON；`initialize`、`tools/call`、host 反向请求、通知                              |
| 多路复用   | writer task + reader task + 每请求 oneshot；超时发送 `$/cancel`                                        |
| 注册表     | tools / commands / events；启动时做命名空间与能力校验                                                  |
| 生命周期   | `disabled` / `starting` / `ready` / `degraded` / `failed` / `stopped`                                  |
| Host API   | `host.log`、`host.info`、`host.ping`、`host.storage.*`                                                 |
| 声明式插件 | 仅含 `[[contributes.commands]]` 的包不启动进程                                                         |
| 工具结果   | content blocks（text/image/resource/diff/json）+ `structured` + `is_error`                             |
| 示例       | `examples/plugins/echo/` 已迁移到 v2                                                                   |

明确的占位（握手时声明非空会启动失败）：

- `hooks`、`ui`、`rpc` 贡献。
- `host.fs.*`、`host.net.*`、`host.secrets.*`、`host.session.*`、`host.ui.*`、`host.rpc.call`、`host.blob.*`、`host.job.*` 等 host 方法返回 `unsupported`。

---

## 1. 背景与现状

### 1.1 旧体系（已废弃）

v1 使用 `schema = 1`、`PLUGIN_API_VERSION = 1`、顶层 `[server]`，工具结果只有 `{ok, output: String}`。这些字段与旧 JSON 形状已从代码中删除；当前仓库只接受 schema 2 / API v2，旧 manifest 会在加载时被拒绝。

### 1.2 v2 要解决的问题

已在本次内核中解决：

1. 工具结果表达力弱：现在使用 content blocks、`structured` 和 `is_error`。
2. 单请求串行：改为 writer/reader task + 每请求 oneshot，请求按 `id` 多路复用。
3. 事件不可靠且不可配置：现在按 `initialize` 的事件模式过滤订阅，队列满仍是 best-effort。

仍需后续里程碑：

4. hooks：无法在 turn、prompt、tool、approval、文件写入等管线插入逻辑。
5. UI 贡献：manifest 可以声明 `components.ui`，但当前会显式拒绝启动。
6. 包管理：无安装、版本约束、lockfile、校验和、签名、平台约束。
7. settings schema：可以传 `settings` 值，但尚不能在 TUI 中生成表单。
8. 能力粒度粗：路径、域名、secret 仍是布尔开关，无法表达作用域。
9. 只有全局插件：项目级插件仍被完全禁止；多 workspace 语义不完整。
10. 可观测性弱：日志只走 `eprintln` / `host.log`，没有 ring buffer、审计表和指标。
11. `/reload` 全量重启：改一个插件仍会重启全部；没有 lazy start、退避重启和健康状态。
12. 没有插件 SDK：`eix-sdk` 是 HTTP client，不是 plugin SDK。

---

## 2. 目标与非目标

### 2.1 目标

- 插件可以扩展 eix 的模型工具、命令、prompt/context、审批、文件写入、UI、主题、键位、agent、skill、MCP、provider 和后台任务。
- 声明式插件零代码可用；需要逻辑时再使用子进程。
- 所有扩展点共享同一套 manifest、授权模型和注册表。
- 插件崩溃、卡死、超时不会阻塞 turn 或拖垮 daemon。
- 权限默认拒绝，授权可细到路径、域名、secret 名和 hook 点。
- 插件作者有稳定的协议、SDK 和一致性测试。

### 2.2 非目标

- 不做进程内 `dylib` 插件。ABI 不稳定，崩溃会拖垮 daemon，且需要 `unsafe`。
- 不让插件直接输出 ANSI 或操作终端。
- 不允许插件绕过审批、直接批准工具调用或静默改写用户消息。
- 不假设子进程等于 OS 沙箱。能力模型限制的是 host API，不是操作系统权限。

### 2.3 设计原则

1. Kernel stable，contributions flexible：内核只认贡献点和能力，不认具体插件。
2. Deny by default：未授权的贡献启动失败，而不是静默降级。
3. 不可静默提权：hook 可以否决、标注、请求重新审批，但不能替 host 批准。
4. Host-rendered UI：插件提交视图模型，host 负责布局、主题、键位和安全。
5. 声明优先：命令、技能、agent、主题、MCP 等常见扩展不需要写代码。
6. 失败隔离 + 确定性：hook 有明确顺序、超时和失败策略。
7. 零成本缺席：没有插件订阅时，热路径不 clone、不加锁、不 spawn。
8. 前向兼容：未知 host 方法明确 `unsupported`；未知贡献点不静默执行。

---

## 3. 总体架构

```text
┌──────────────────────────────────────────────────────────────┐
│ eixd                                                         │
│                                                              │
│  turn / tool / prompt / approval pipeline                    │
│            │                                                 │
│            ▼                                                 │
│  ExtensionRegistry                                           │
│    tools · commands · hooks · events · ui · themes · ...     │
│            │                                                 │
│            ├── builtin contributions                         │
│            ├── declarative package contributions             │
│            └── service / ui plugin contributions             │
│                        │                                     │
│  CapabilityGateway ────┘                                     │
│    fs · network · secrets · storage · rpc · ui               │
│                        │                                     │
│  PluginHost (process per plugin, JSON-RPC, multiplexed)      │
└────────────────────────┬─────────────────────────────────────┘
                         │ stdio / JSON-RPC 2.0
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
      server         ui component    declarative
      component      (view model)     (no process)
```

### 3.1 三种插件形态

| 形态 | 是否需要进程 | 适合场景 | 能力 |
| --- | --- | --- | --- |
| 声明式（Declarative） | 否 | 命令、skill、agent、主题、键位、MCP server、权限规则 | 静态贡献 |
| 服务（Service） | 是 | 工具、hooks、事件、RPC、网络、文件、secret、后台任务 | host API |
| UI（UI） | 是 | 自定义 pane、overlay、status、picker、设置表单 | 声明式视图树 |

一个包可以同时包含声明式、server 和 ui 三类贡献，共用同一个 manifest 和授权；本版 UI 组件尚未实现，声明后启动会失败。

### 3.2 统一注册表

`ExtensionRegistry` 在插件启动时把 manifest 和握手结果解析成 host 侧的强类型结构：

- 热路径只做查表，不在每次调用时解析 JSON 或检查 manifest。
- 贡献冲突在启动时发现：命名空间冲突、schema 非法、能力未授权。
- 插件退出时按 `plugin` 字段整体移除其贡献，不影响其他插件。

当前 `PluginManager` 已维护 `tools` / `commands` / `subscriptions` / `statuses` 表；后续可进一步统一为按 kind 索引的 `ContributionRegistry`。

---

## 4. 包与 Manifest

### 4.1 目录布局

```text
acme/
  eix-plugin.toml
  server.py
  ui.py
  settings.schema.json
  skills/review/SKILL.md
  agents/triage.toml
```

### 4.2 Manifest v2

```toml
schema = 2
name = "acme"
version = "2.1.0"
description = "Issue tracking and review helpers"
api_version = 2              # 整数；必须等于 host 的 PLUGIN_API_VERSION
min_host_version = "0.9.0"   # 记录但不强制

capabilities = [
  "tools", "commands", "hooks", "ui",
  "workspace_read", "network", "secrets", "storage",
]

# 可选：设置表单 schema（当前只记录路径，TUI 尚未渲染）。
settings = "settings.schema.json"

[components.server]
command = "python3"
entry = "server.py"

# UI 组件是后续里程碑；当前版本声明它会让插件启动失败。
[components.ui]
command = "python3"
entry = "ui.py"

[[contributes.commands]]
name = "triage"
description = "Triage the current issue"
prompt = "Read the issue and propose a plan. $ARGUMENTS"
```

去掉 `hooks` / `ui` 能力与 `components.ui` 后，就是当前版本可运行的 manifest 子集。

本版只解析 `contributes.commands`；`skills`、`mcp_servers`、`themes`、`keymaps` 等贡献点仍按第 5 节规划。

### 4.3 兼容策略

- 只接受 `schema = 2`；缺少 `schema` 或 `api_version` 的 v1 manifest 拒绝加载。
- 只识别 `[components.server]` / `[components.ui]`；顶层 `[server]` 不再识别。
- 本版 `components.ui` 会在启动时显式拒绝（UI 里程碑未实现）。
- 未知字段由 serde 忽略，未知贡献点暂不解析。
- 仅声明式（只有 `contributes.commands`）的包允许没有 server / ui 组件。

### 4.4 从 v1 迁移

| v1 | v2 |
| --- | --- |
| 顶层 `[server]` | `[components.server]` |
| 省略 `schema` / `api_version = 1` | `schema = 2` / `api_version = 2` |
| `{id, result}` / `{id, error: {code: "..."} }` | 增加 `"jsonrpc": "2.0"`；错误码为整数 |
| `tools/call` 返回 `{ok, output}` | 返回 `{ok, content: [{"type":"text","text":...}], is_error?}` |
| 只能握手返回命令 | 也可在 `[[contributes.commands]]` 静态声明 |
| 单请求串行 | 响应按 `id` 关联，可并发 |

---

## 5. 贡献点

| 贡献点 | 形态 | 能力 | 说明 |
| --- | --- | --- | --- |
| `tools` | 服务 | `tools` | 模型工具，暴露为 `plugin__<plugin>__<tool>` |
| `commands` | 声明/服务 | `commands` | prompt 模板和 action 命令 |
| `hooks` | 服务 | `hooks` | 管线拦截，见第 7 节 |
| `events` | 服务 | `events` | 只读通知订阅 |
| `ui.views` | 服务 | `ui` | 自定义 pane / overlay |
| `ui.pickers` | 服务/声明 | `ui` | 统一 Picker 的 provider |
| `ui.status` | 服务 | `ui` | statusline 片段 |
| `ui.settings` | 声明 | — | 由 JSON Schema 生成表单 |
| `themes` | 声明 | — | 颜色 token，不可覆盖内置名 |
| `keymaps` | 声明 | — | 命名空间动作 `plugin.<name>.*` |
| `agents` | 声明 | `agents` | prompt + 工具集 + 模型偏好 |
| `skills` | 声明 | `skills` | `SKILL.md` |
| `mcp_servers` | 声明 | — | 复用现有 MCP |
| `providers` | 声明/服务 | `network`/`secrets` | 先声明式 OpenAI-compatible，后 adapter |
| `jobs` | 服务 | `jobs` | 定时/后台任务 |
| `rpc` | 服务 | `rpc` | 暴露给其他插件的 RPC 方法 |

本版只实现 `tools`、`commands`、`events`。其余贡献点保留在设计中，尚未解析或执行；hook / UI / RPC 在握手中非空会直接拒绝启动。

### 5.1 命名与冲突规则

- 工具：`plugin__<plugin>__<tool>`；`<tool>` 不允许包含 `__`。
- 命令：`plugin/<plugin>/<command>`；不能覆盖内置命令和用户 Markdown 命令。
- 键位：`plugin.<plugin>.<action>`；用户配置优先于插件。
- 主题：只能引用或新增 token，不能覆盖内置主题名。

### 5.2 Action 命令（未实现）

当前插件命令只能展开 prompt。规划中的 `action` 类型：

```toml
[[contributes.commands]]
name = "sync"
description = "Sync the issue cache"
[contributes.commands.action]
method = "sync"
args_schema = { type = "object", properties = { full = { type = "boolean" } } }
needs = ["network", "storage"]
```

- TUI 的 `/` 菜单和命令面板可以触发。
- host 调用插件的 `commands/run`，由插件通过已授权 host API 执行。
- 不开放任意 shell；需要执行命令的插件必须申请 `exec` 能力并走审批。

---

## 6. 授权与作用域

### 6.1 配置示例

```toml
[plugins.acme]
source = { registry = "acme", version = "^2.1" }
enabled = true
lazy = true

[plugins.acme.grants.tools]
read_only = true               # 是否信任 read_only 标注

[plugins.acme.grants.fs]
read = ["${workspace}/**"]
write = ["${workspace}/docs/**"]

[plugins.acme.grants]
network = ["api.acme.com", "*.acme-cdn.com"]
secrets = ["ACME_TOKEN"]
hooks = ["tool.before", "turn.end"]
ui = true
rpc = { call = ["acme/*"] }

[plugins.acme.settings]
project_key = "ACME"
```

### 6.2 规则

- manifest 的 `capabilities` 是申请，config 的 `grants` 是授权；授权只能收窄。
- 路径、域名、secret 名支持 glob；deny 优先。
- 每次 host API 调用都带 `workspace` 和 `session_id`，host 校验调用者是否有权访问该作用域。
- `tools.read_only` 只有在 `grants.tools.read_only = true` 时才被信任；否则一律按可写工具处理。
- 项目层默认不能授权插件；需要显式 `eix plugin trust` 或签名白名单。
- 多个 workspace 共用插件进程时，授权按 workspace 分别生效。

---

## 7. 协议 v2

### 7.1 传输

保留 NDJSON over stdio，并明确约束：

- stdout 只允许协议帧；日志必须走 stderr 或 `host.log`。
- 单帧上限 8 MiB；超限断连并标记 failed。
- 大二进制不内联，走 `host.blob.put/get` 返回 handle。
- 后续可协商 `Content-Length` framing，不作为第一版要求。

### 7.2 握手

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol_version": 2,
  "api_version": 2,
  "plugin": {"name":"acme","version":"2.1.0"},
  "host": {"name":"eixd","version":"0.9.0"},
  "features": ["content_blocks","cancel"],
  "capabilities": ["tools","commands","events","storage"],
  "workspace": "/repo",
  "config": {"data_dir":"...","settings":{"project_key":"ACME"}}
}}
```

```json
{"jsonrpc":"2.0","id":1,"result":{
  "name":"acme","version":"2.1.0","api_version":2,
  "features":["content_blocks","cancel"],
  "tools":[],
  "commands":[],
  "events":["session.*"],
  "hooks":[],
  "ui":[],
  "rpc":[]
}}
```

- `features` 协商：host 只调用双方都支持的扩展方法。
- `api_version` 当前是整数，必须与 host 完全一致；不匹配拒绝启动。
- `hooks` / `ui` / `rpc` 本版必须为空，非空会让该插件启动失败。
- 事件列表使用 `*` / `?` 通配符，例如 `session.*`。

### 7.3 多路复用

```text
writer task  <-- mpsc<Frame>
reader task  --> BTreeMap<id, oneshot::Sender<Response>>
dispatcher   --> 每个 in-flight 请求一个 task，max_inflight 默认 8
```

效果：

- 工具调用和 host 回调可以并发。
- 支持 `$/cancel`：host 超时后发取消通知。
- `$/progress` 仍预留在协议中，host 当前不消费。
- 超时按请求粒度，而不是整个插件。

### 7.4 工具结果

```json
{"ok": true,
 "content": [
   {"type":"text","text":"found 3 issues"},
   {"type":"image","mime":"image/png","blob":"blob_123"},
   {"type":"diff","path":"src/lib.rs","patch":"..."},
   {"type":"resource","uri":"acme://issue/42","title":"Issue 42"}
 ],
 "structured": {"issues":[]},
 "is_error": false}
```

配套限制：

- 工具输出按 `tools.max_output_bytes` 截断，插件侧上限 1 MiB；单项大小与图片 MIME 白名单仍是后续工作。
- `structured` 当前只在 `content` 为空时作为文本回退。
- 当前 TUI 只渲染文本：image / resource / diff / json 会先变成短占位文本。
- v1 的 `output: String` 已移除，不再自动包装。

### 7.5 Host API

统一 `host.*` 命名空间，每个方法先检查能力：

| 方法 | 能力 | 说明 |
| --- | --- | --- |
| `host.log` | — | 已实现；写 stderr，支持请求和通知两种形式 |
| `host.info` | — | 已实现；返回插件名、API 版本、能力、workspace |
| `host.ping` | — | 已实现 |
| `host.storage.*` | `storage` | 已实现；复用现有 KV，按 global/workspace 分区 |
| `host.fs.read` / `write` / `list` | `workspace_read` / `workspace_write` | 已预留，返回 `unsupported` |
| `host.net.request` | `network` | 已预留，返回 `unsupported` |
| `host.secrets.get` | `secrets` | 已预留，返回 `unsupported` |
| `host.session.get` / `list` | `sessions_read` | 已预留，返回 `unsupported` |
| `host.notify` | `ui` | 已预留，返回 `unsupported` |
| `host.ui.open` / `update` / `close` | `ui` | 已预留，返回 `unsupported` |
| `host.rpc.call` | `rpc` | 已预留，返回 `unsupported` |
| `host.blob.put` / `get` | `storage` | 已预留，返回 `unsupported` |
| `host.job.*` | `jobs` | 已预留，返回 `unsupported` |

错误码统一：`-32602 invalid_params`、`-32001 unauthorized`、`-32002 unsupported`、`-32003 timeout`、`-32004 cancelled`、`-32005 plugin_failed`。

### 7.6 插件间 RPC

```text
plugin A -> host.rpc.call {plugin:"acme", method:"issues.search", params:{...}}
host     -> plugin B: rpc/call
plugin B -> host: result
host     -> plugin A: result
```

- 只有声明 `rpc` 且被授权 `rpc.call` 的插件可调用。
- 方法名带插件命名空间，避免撞名。
- 调用链深度上限 4，防止递归。
- 每次调用记录审计，包含 caller / callee。

---

## 8. Hooks

事件是只读通知，hook 是可参与决策的请求，两者不混用。

### 8.1 Hook 点

| Hook | 时机 | 能做什么 | 默认失败策略 |
| --- | --- | --- | --- |
| `daemon.start` / `stop` | daemon 生命周期 | 初始化 / 清理 | skip |
| `config.changed` | 配置热重载 | 重新读取 settings | skip |
| `session.create` / `archive` | session 生命周期 | 初始化 / 清理 | skip |
| `turn.start` | turn 开始 | 注入上下文、记录 | skip |
| `prompt.build` | 组装 system prompt / context | 追加 context block、标注 | skip |
| `model.request` | 发模型请求前 | 调整 temperature、追加工具白名单 | skip |
| `model.response` | 模型响应后 | 标注、脱敏 | skip |
| `tool.before` | 工具执行前 | deny、标注、条件改写参数 | skip |
| `tool.after` | 工具执行后 | 改写输出、脱敏、附注 | skip |
| `approval.before` | 审批展示前 | 只能 deny 或加说明 | skip |
| `file.before_write` / `after_write` | 文件写入 | 校验、格式化、审计 | abort |
| `message.delta` | 流式增量 | 观察、聚合 | skip |
| `compact.before` / `after` | 上下文压缩 | 观察、保留关键片段 | skip |
| `turn.end` | turn 结束 | 清理、统计 | skip |

### 8.2 返回值

```json
{"decision":"continue"}
{"decision":"deny","reason":"missing issue id"}
{"decision":"continue","annotations":[{"level":"warning","text":"generated file"}]}
{"decision":"continue","patch":{"arguments":{"path":"src/lib.rs"}}}
```

规则：

- `deny` 只能阻止，不能批准。
- `patch` 只允许白名单字段；改写参数后 host 重新做风险分类和审批。
- annotation 进入 transcript / approval UI，可折叠。
- 所有修改进审计日志。

### 8.3 顺序、超时与并发

- 顺序：内置强制逻辑永远第一；插件 hook 按 `(priority, plugin_name)` 全序。
- 超时：默认 300ms，最大 2s。
- 并发：第一版串行以保证确定性；后续对纯 observer hook 并行，再按顺序合并。
- 预算：每个 turn 的 hook 总预算约 1s，超预算后跳过剩余 hook 并记录 warning。
- 递归：hook 内触发的工具调用不再触发同类 hook（深度 1）。

### 8.4 审批权

审批权只属于 host 和用户。hook 最多能否决或要求更多信息，不能返回 `allow`。这样即使插件被攻破，也无法绕过 eix 的审批模型。

---

## 9. UI 贡献

### 9.1 原则

- 插件不输出 ANSI、不直接操作终端。
- 插件提交视图树，host 用现有 ratatui 渲染。
- 视图树有 schema 版本和硬上限。
- 交互通过 `ui/action` 回到插件，插件再通过有权限的 host API 执行操作。

### 9.2 视图节点

```json
{
  "kind": "list",
  "id": "issues",
  "title": "Issues",
  "items": [
    {"id": "42", "label": "#42 Fix parser", "detail": "open · p1"},
    {"id": "43", "label": "#43 Add docs", "detail": "closed"}
  ],
  "actions": [
    {"id": "open", "label": "Open", "key": "Enter"},
    {"id": "refresh", "label": "Refresh", "key": "r"}
  ]
}
```

支持的节点：`text`、`markdown`、`list`、`table`、`tree`、`tabs`、`form`、`diff`、`log`、`progress`、`sparkline`、`actions`、`spacer`。

限制：最大 2000 节点、深度 16、单文本 64 KiB；未知 `kind` 渲染为错误占位，不崩溃。

### 9.3 Surface

| Surface | 用途 | 生命周期 |
| --- | --- | --- |
| `pane` | 全屏 / 分屏视图 | 用户或命令控制 |
| `overlay` | 模态对话框 | 短生命周期 |
| `status` | statusline 片段 | 常驻，低频更新 |
| `picker` | 统一 Picker 的 provider | 查询驱动 |
| `settings` | 插件设置表单 | 由 JSON Schema 生成 |
| `notification` | toast | 一次性 |

### 9.4 更新与事件流

```text
host -> plugin: ui/open {view_id, surface, size}
plugin -> host: ui/update {view_id, revision, tree}
host -> plugin: ui/action {view_id, action_id, payload}
host -> plugin: ui/visibility {view_id, visible}
host -> plugin: ui/close {view_id}
```

- `revision` 单调递增，host 丢弃过期更新。
- 更新合并窗口 16–50ms，避免高频重绘。
- 不可见视图不推送更新。
- host 先处理全局 / overlay 键，再把剩余动作发给插件。
- 主题使用 host token，不写死颜色。

### 9.5 设置表单

插件提供 JSON Schema 子集：

```json
{
  "type": "object",
  "properties": {
    "project_key": {"type": "string", "title": "Project key"},
    "auto_sync": {"type": "boolean", "default": true}
  }
}
```

host 在 `/plugins` 中生成表单，保存后通过 `config.changed` 通知插件。

---

## 10. 安全模型

### 10.1 信任分层

| 层 | 默认 | 说明 |
| --- | --- | --- |
| 内置工具 | 可信 | 现有风险分类 |
| 声明式插件 | 可信但受路径约束 | 只能贡献静态内容 |
| 服务插件 | 不可信 | 工具默认审批；hook 不能批准 |
| UI 插件 | 不可信 | host 渲染，节点受限 |
| 项目级插件 | 默认禁止 | 需显式 trust 或签名 |

### 10.2 能力边界

- `workspace_read` / `workspace_write`：glob + workspace root，拒绝 `..` 和 symlink 逃逸。
- `network`：域名/端口白名单，host 代理。
- `secrets`：按名授权，按需获取，日志脱敏，不进入模型上下文。
- `tools.read_only`：只有显式信任才生效。
- `hooks`：按 hook 点授权。

能力限制的是 host API，不是操作系统。插件进程仍以 daemon 用户身份运行，可以直接读写该用户可访问的文件和网络。需要强隔离时必须启用 sandbox，或使用单独用户 / 容器运行 daemon。

### 10.3 审计

新增 `plugin_audit`：

```text
id, ts, plugin, method, capability, session_id, workspace,
ok, duration_ms, detail_hash, error
```

记录所有 host API 调用、hook 决策、工具调用和审批交互。参数只存 hash 或摘要，避免 secret 落库。TUI `/plugins` 可查看最近审计与错误。

### 10.4 可选 OS sandbox

```toml
[plugins.acme]
sandbox = "strict"   # inherit | strict
```

- Linux：Landlock（只读系统目录 + 授权 workspace + 插件数据目录 + 授权网络）+ seccomp + `no_new_privs` + rlimit。
- macOS：seatbelt profile。
- 其他平台：明确说明只有进程隔离，不假装有沙箱。

### 10.5 供应链

- `eix-plugins.lock` 记录 name / version / source / digest。
- 安装时校验 checksum；registry 包支持签名。
- `eix plugin verify` 检查 manifest、digest、API 版本和入口可执行。
- 首次安装 TOFU；升级 digest 变化需要确认。

---

## 11. 生命周期与运行时

### 11.1 状态机

```text
disabled -> installed -> starting -> ready
                         |  ^         |
                         v  |         v
                       failed      degraded
                         |
                      stopped
```

- `degraded`：部分贡献可用，某个 hook/tool 连续失败。
- `failed`：启动失败或崩溃，保留错误并允许 restart。
- `stopped`：用户禁用或 daemon 退出。

### 11.2 启动与重启

- `lazy = true`：首次使用其贡献时启动。
- 事件订阅者和关键 hook 默认 eager。
- 独立退避：1s、2s、4s… 上限 60s；连续失败进入 circuit breaker。
- `/reload` 做配置 diff，只重启变化的插件。

### 11.3 并发与背压

- 每插件 `max_inflight` 默认 8，超出排队或拒绝。
- 事件投递按订阅者策略：`drop`（观察型）/ `coalesce`（UI 状态）/ `queue`（有限队列）。
- hook 永远走请求-响应，不走事件队列。
- 大输出截断，保留头尾和长度提示。

### 11.4 多 workspace

- 默认一个插件进程服务多个 workspace，所有 host 调用带 `workspace` 并校验。
- 插件应按 workspace 分区状态，或声明 `per_workspace = true` 启动独立进程。
- hook / event payload 必须包含 `workspace`，避免跨项目泄漏。

---

## 12. 数据模型与配置

### 12.1 Rust 类型

```rust
pub struct PluginManifest {
    pub schema: u32,                 // 只接受 2
    pub name: String,
    pub version: String,
    pub description: String,
    pub api_version: u32,            // 必须等于 PLUGIN_API_VERSION
    pub min_host_version: Option<String>,
    pub capabilities: Vec<PluginCapability>,
    pub components: PluginComponents,
    pub contributes: PluginContributions,
    pub settings: Option<PathBuf>,
}

pub struct PluginComponents {
    pub server: Option<PluginComponent>,
    pub ui: Option<PluginComponent>,
}

pub struct PluginComponent {
    pub command: String,
    pub args: Vec<String>,
    pub entry: Option<PathBuf>,
}

pub struct PluginContributions {
    pub commands: Vec<ManifestCommand>,
}

pub struct PluginConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub timeout_secs: u64,
    pub cwd: Option<PathBuf>,
    pub manifest: Option<PathBuf>,
    pub capabilities: Vec<PluginCapability>,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub settings: BTreeMap<String, serde_json::Value>,
}
```

hook / UI / RPC 的强类型结构暂未加入 manifest；它们只在握手的 `PluginInitializeResult` 中保留占位字段，非空即启动失败。

### 12.2 配置

当前 `[plugins.<name>]` 支持：

- `command` / `args` / `env` / `cwd`：进程入口；与 `manifest` 二选一或组合。
- `enabled` / `timeout_secs`：启停与请求超时（秒）。
- `manifest`：`eix-plugin.toml`，相对路径按 config cwd 解析；manifest 的 `name` 必须与配置 key 一致。
- `capabilities`：只能收窄 manifest 的申请；留空表示采用 manifest 的申请或 `[tools, commands]`。
- `allowed_tools` / `denied_tools`：`*` / `?` 通配符，deny 优先。
- `settings`：任意 JSON 值，随 `initialize` 传给插件。

后续再增加 `source`、`version`、`lazy`、细粒度 grants、`sandbox`、`per_workspace`。

### 12.3 数据库

- 复用 `plugin_kv`。
- 新增 `plugin_audit`。
- 新增 `plugin_installs`（版本、来源、digest、信任状态）。
- secrets 走 OS keyring 或加密存储，不直接落 SQLite 明文。

---

## 13. 可观测性与开发体验

### 13.1 运维

- 每插件 ring buffer（最近 500 行），`plugin.logs {name, since}`。
- `plugin.list` 增加：pid、uptime、restarts、inflight、p50/p95 延迟、last_error、贡献数量。
- `plugin.doctor`：检查 manifest、入口可执行、API 版本、schema、授权冲突。
- TUI `/plugins`：列表、状态、日志、设置、enable/disable/restart。
- statusline 在插件 failed/degraded 时显示标记。

### 13.2 开发

- `eix plugin new`：生成 manifest + Python/TS/Rust 模板。
- `eix plugin run <dir>`：前台运行 + 协议 dump + 热重载。
- `eix plugin test`：协议一致性测试（握手、工具 schema、hook 返回、超时、取消）。
- SDK：`eix-plugin` Rust crate、Python package、TypeScript package。
- 文档：协议参考由 schema / Rust 类型生成，避免手写漂移。
- 示例：声明式、tool、hook、UI、RPC 各一个。

---

## 14. 兼容与版本策略

1. 当前 `PLUGIN_API_VERSION = 2` 是整数；握手双方必须完全一致。
2. 每个贡献点未来可带独立 schema 版本，UI 树单独 `ui_schema`。
3. v1 manifest / 旧 JSON 协议已删除，不提供兼容层；迁移方式是按第 4、7 节改写 manifest 与响应。
4. 未知贡献点与未知 host 方法：明确失败或 `unsupported`，不静默降级。
5. 未来协议升级流程：文档标记 → warning 期 → 提升 `PLUGIN_API_VERSION`；不同 major 不混跑。

---

## 15. 落地路线

### Phase 0：内核重构（已完成）

- 把 `PluginManager` 拆成 `mod` / `client` / `host_api`。
- 统一 tools / commands / events 注册表与启动时校验。
- 加入状态枚举、声明式命令、工具名过滤、事件订阅。
- 验收：`cargo test -p eixd plugin` 与插件 e2e 覆盖新协议。

### Phase 1：协议 v2 + SDK（协议已完成，SDK 未开始）

- JSON-RPC 2.0 帧、多路复用、取消、content blocks 已落地。
- Python/TS SDK + conformance test 未开始。
- 验收：一个慢工具不阻塞同插件的第二个调用；取消能生效。

### Phase 2：Hooks + 声明式插件 + Action 命令

- hook 点、顺序、超时、失败策略、审计。
- `commands` 支持 action。
- 声明式 themes / keymaps / agents / skills / mcp_servers。
- settings schema + 热更新。
- 验收：插件能 veto 工具、追加 context、拦截文件写入；声明式包无需代码即可安装。

### Phase 3：UI 贡献

- `UiNode` 渲染器、`pane` / `overlay` / `status` / `picker` / `settings`。
- `/plugins` 管理器。
- 验收：插件能打开实时更新的 pane，键位不冲突，主题跟随 host。

### Phase 4：安全与分发

- 细粒度 fs / network / secrets 授权。
- 可选 strict sandbox。
- install / lockfile / digest / 签名 / trust。
- 验收：只读插件无法写 workspace；网络只能访问白名单；升级 digest 变化需确认。

### Phase 5：高级扩展

- 插件间 RPC、jobs、provider adapter、WASM 可选层。

---

## 16. 关键决策

| 决策                | 结论     | 理由                                     |
| ---                 | ---      | ---                                      |
| 进程内 dylib        | 不做     | ABI 不稳定、崩溃拖垮 daemon、需要 unsafe |
| 进程外 JSON         | 默认     | 语言无关、调试简单、崩溃隔离             |
| WASM                | 后续可选 | 真正沙箱，但 UI/网络/文件能力要重做一遍  |
| 原始 ANSI UI        | 不做     | 破坏主题、键位、布局和可访问性           |
| 插件直接改消息历史  | 限制     | 只允许追加 context / 标注，避免绕过审批  |
| hook 批准工具       | 禁止     | 安全边界必须留在 host                    |
| 每 workspace 一进程 | 默认不做 | 进程数膨胀；用 per-call scope + 可选开关 |
| 项目级插件          | 默认禁止 | 克隆仓库不能自动执行代码                 |
| 插件工具自动批准    | 禁止     | 维持 `RiskCategory::Plugin` 策略         |

---

## 17. 开放问题

1. 插件生态主语言：Python、TypeScript/Node、Rust 的优先级。
2. UI 插件范围：先做 picker/status/settings，还是直接做自定义 pane。
3. 项目级插件是否允许，以及 trust / 签名的具体形式。
4. 是否要 WASM 层；这会显著增加协议和运行时复杂度。
5. 是否允许插件贡献 model provider（涉及 network、secrets、streaming）。
6. 多 workspace daemon 下默认共享进程还是按 workspace 隔离。

---

## 18. 与现有代码的映射

| 位置                                 | 现状                                                                     |
| ---                                  | ---                                                                      |
| `crates/eix-core/src/plugin.rs`      | manifest v2、配置、JSON-RPC 类型、content blocks、状态枚举               |
| `crates/eixd/src/plugin/mod.rs`      | `PluginManager`、注册表、生命周期、声明式命令、校验                      |
| `crates/eixd/src/plugin/client.rs`   | 进程 spawn、writer/reader、请求多路复用、超时取消、事件投递              |
| `crates/eixd/src/plugin/host_api.rs` | `HostContext`、storage、预留方法处理                                     |
| `crates/eixd/src/server.rs`          | `plugin.list` / `plugin.commands` / `plugin.restart`、工具定义与调用接入 |
| `crates/eixd/src/classifier.rs`      | `RiskCategory::Plugin` 保留；插件工具默认审批                            |
| `crates/eix-tui/src/tui/app/mod.rs`  | `merge_commands` 合并宿主下发的插件命令                                  |
| `crates/eix-core/src/config.rs`      | `plugins` 解析与校验；项目级 `plugins` 仍被删除                          |
| `examples/plugins/echo/`             | v2 示例：JSON-RPC 2.0、content blocks、storage、events                   |

hooks / UI / RPC 的接入点仍按第 7、8 节规划，当前在启动时显式拒绝。

## 19. 后续最小步骤

1. Hooks：`tool.before` / `tool.after` / `prompt.build` / `turn.end` + 审计。
2. 更多声明式贡献：skills / agents / themes / keymaps / MCP。
3. Python / TypeScript SDK 与协议一致性测试。
4. UI 贡献与 `/plugins` 管理器。
5. 安装、lockfile、签名与信任。
