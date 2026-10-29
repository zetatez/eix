# eix 插件体系设计（v2）

- 状态：v2 内核、hooks、文本 UI、插件间 RPC、细粒度 allowlist、打包/验签/安装均已实现；自定义 UI 视图树与 lockfile 仍是后续里程碑
- 原生 dylib 扩展：见 [`plugin-dylib.md`](plugin-dylib.md)（P1 已落地：`DylibBackend` 加载器 + `describe` / `tool.call` / `command.run`）
- 适用范围：`eixd`（host）、`eix-core`（协议与 manifest/打包）、`eix-tui`（UI 贡献的渲染端与 CLI）
- 关联实现：`crates/eix-core/src/plugin.rs`、`crates/eix-core/src/plugin_package.rs`、`crates/eixd/src/plugin/`、`crates/eixd/src/server.rs`、`crates/eix-tui/src/tui/app/`、`crates/eix-tui/src/main.rs`

本文描述 eix 插件体系从 v1（外挂工具 + 提示词命令）演进到 v2（贡献点 + hooks + UI + 细粒度授权）的架构、协议、安全边界和落地状态。v1 协议与 manifest 已废弃，不保留兼容层。

## 0. 当前实现状态

已落地：

| 能力       | 实现                                                                                                   |
| ---        | ---                                                                                                    |
| Manifest   | `schema = 2`、`api_version = 2`、`[components.server]` / `[components.ui]` / `[components.dylib]`、`abi`、`[[contributes.commands]]`、`[package]` |
| 原生 dylib | `ExtensionBackend` + `DylibBackend`；`describe` / `tool.call` / `command.run` / `hook.run` / `rpc.call` 已接入，与进程插件共用注册表 |
| 协议       | JSON-RPC 2.0 over NDJSON；`initialize`、`tools/call`、`commands/run`、`hooks/run`、`rpc.call`、host 反向请求、通知 |
| 多路复用   | writer task + reader task + 每请求 oneshot；超时发送 `$/cancel`                                        |
| 注册表     | tools / commands / events / hooks / ui / rpc；启动时做命名空间与能力校验                               |
| 生命周期   | `disabled` / `starting` / `ready` / `degraded` / `failed` / `stopped`；连续 3 次后端失败进入 `degraded`，成功一次恢复 |
| Host API   | `host.log/info/ping`、`storage.*`、`fs.*`、`net.request`、`secrets.get`、`session.*`、`ui.*`、`notify`、`rpc.call`、`blob.*`、`job.*` |
| Hooks      | `tool.before` / `tool.after` / `prompt.build` / `turn.stop` / `turn.end`，按 priority + 插件名排序                  |
| Action 命令 | manifest 或运行时 `commands` 的 `action`；host 调用 `commands/run`，可返回 prompt 或文本输出          |
| 声明式插件 | 仅含 `[[contributes.commands]]` 的包不启动进程                                                         |
| 工具结果   | content blocks（text/image/resource/diff/json）+ `structured` + `is_error`                             |
| Settings   | manifest `settings = "schema.json"` 必须是包内、`type: object`、≤256 KiB；`plugin.settings.set` 校验后写回配置并通知插件 |
| UI 表面    | `plugin.ui.list` 返回已声明的 `panel` / `status` / `picker`；TUI `/plugins [name]` 展示                       |
| 分发       | `eix plugin pack/verify/install/trust/untrust`；detached Ed25519 签名 + SHA-256 checksums              |
| 示例       | `examples/plugins/echo/` 已迁移到 v2，并演示 action 命令                                               |

尚未实现：

- dylib 插件的 host API 覆盖同步安全的 `log/info/ping/notify/storage.*/secrets.get/fs.*/session.*/blob.*`；`net` / `ui` / `rpc` / `jobs` 仍返回 `EIX_ERR_DENIED`，详见 [`plugin-dylib.md`](plugin-dylib.md)。
- 插件提交自定义视图树（`pane` / `picker` 等）；`picker` 表面可以被声明和列出，实际渲染仍复用文本面板。
- TUI 内直接编辑 settings；`plugin.settings.set` 已可用，但 TUI 仍只读展示。
- lockfile、registry、lazy start、ring buffer 审计表和 OS sandbox。

---

## 1. 背景与现状

### 1.1 旧体系（已废弃）

v1 使用 `schema = 1`、`PLUGIN_API_VERSION = 1`、顶层 `[server]`，工具结果只有 `{ok, output: String}`。这些字段与旧 JSON 形状已从代码中删除；当前仓库只接受 schema 2 / API v2，旧 manifest 会在加载时被拒绝。

### 1.2 v2 要解决的问题

已在本次内核中解决：

1. 工具结果表达力弱：现在使用 content blocks、`structured` 和 `is_error`。
2. 单请求串行：改为 writer/reader task + 每请求 oneshot，请求按 `id` 多路复用。
3. 事件不可靠且不可配置：现在按 `initialize` 的事件模式过滤订阅，队列满仍是 best-effort。

已在后续里程碑解决：

4. hooks：`tool.before` / `tool.after` / `prompt.build` / `turn.stop` / `turn.end` 已接入 turn 与工具管线。
5. UI 贡献：文本 `panel` / `status` 表面与 `host.notify` 已接入 TUI；自定义视图树仍待实现。
6. 包管理：提供 `pack` / `verify` / `install`、SHA-256 checksums、Ed25519 签名和本地 trust store。
7. settings schema：manifest 可声明 JSON Schema，`/plugins <name>` 展示 schema 与当前值。
8. 能力粒度：fs 限定 workspace，网络与 RPC 使用 allowlist，secrets 按环境变量名授权。

仍需后续里程碑：

9. UI 视图树、picker provider 与 settings 的可持久化编辑。
10. lockfile、registry、lazy start、退避重启、ring buffer 审计表和指标。
11. 可选 OS sandbox。
12. 插件 SDK（`eix-sdk` 目前是 HTTP client，不是 plugin SDK）。
13. 项目级插件仍被完全禁止；多 workspace 语义不完整。

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
│  DylibHost  (in-process cdylib, ABI v1 dispatch)             │
└────────────────────────┬─────────────────────────────────────┘
                         │ stdio / JSON-RPC 2.0
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
      server         ui component    declarative
      component      (view model)     (no process)
```

### 3.1 四种插件形态

| 形态 | 是否需要进程 | 适合场景 | 能力 |
| --- | --- | --- | --- |
| 服务（Service） | 是 | 工具、hooks、事件、RPC、网络、文件、secret、后台任务 | host API |
| 原生（Dylib） | 否（加载进 `eixd`） | 性能敏感的工具/命令、first-party 深度扩展 | ABI v1 + 同步 host API |
| UI（UI） | 是 | 文本 panel / status、通知；自定义视图树待实现 | 声明式文本表面 |
| 声明式（Declarative） | 否 | 命令、skill、agent、主题、键位、MCP server、权限规则 | 静态贡献 |

一个包可以同时包含声明式、server / dylib 和 ui 三类贡献，共用同一个 manifest 和授权。`components.server` 与 `components.dylib` 互斥；本版不会为 `[components.ui]` 单独启动进程，UI 文本表面来自 server 组件的 `initialize.ui` 与 `host.ui.*`。

dylib 是**完全可信代码**：它在 daemon 地址空间内运行，崩溃会带崩 daemon，能力系统只约束 `host.*` 调用，不能阻止插件直接使用 `libc`。设计细节见 [`plugin-dylib.md`](plugin-dylib.md)。

### 3.2 统一注册表

`ExtensionRegistry` 在插件启动时把 manifest 和握手结果解析成 host 侧的强类型结构：

- 热路径只做查表，不在每次调用时解析 JSON 或检查 manifest。
- 贡献冲突在启动时发现：命名空间冲突、schema 非法、能力未授权。
- 插件退出时按 `plugin` 字段整体移除其贡献，不影响其他插件。

当前 `PluginManager` 已维护 `tools` / `commands` / `subscriptions` / `statuses` 表；进程插件与 dylib 插件都注册进同一张表。实际调用经 `ExtensionBackend` trait 分发：`ProcessBackend`（JSON-RPC 子进程）与 `DylibBackend`（ABI v1 进程内）只是同一 trait 的两种实现，TUI 和 turn 管线不需要感知 backend 类型。

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
  "tools", "commands", "events", "hooks", "ui", "rpc", "jobs",
  "workspace_read", "network", "secrets", "storage",
]

# 可选：设置表单 schema；必须是包内相对路径、JSON object、≤256 KiB。
settings = "settings.schema.json"

[components.server]
command = "python3"
entry = "server.py"

# 原生 dylib 与 components.server 互斥；二选一。
# abi = 1
# [components.dylib]
# path = "libacme.so"

# UI 组件当前只作为打包声明；文本表面由 server 组件的 initialize.ui 提供。
[components.ui]
command = "python3"
entry = "ui.py"

# 可选：分发元数据。`eix plugin pack` 生成 checksums，签名文件覆盖它。
[package]
publisher = "acme"
signature = "eix-plugin.sig"
checksums = "eix-plugin.sums"

[[contributes.commands]]
name = "triage"
description = "Triage the current issue"
prompt = "Read the issue and propose a plan. $ARGUMENTS"

# action 命令需要 server 组件；前端调用 plugin.command.run。
[[contributes.commands]]
name = "sync"
description = "Sync the issue cache"
action = "sync"
```

去掉 `hooks` / `rpc` / `jobs` 等能力后仍是合法 manifest；当前支持的能力集合见第 6 节。

本版解析 `contributes.commands`；`skills`、`mcp_servers`、`themes`、`keymaps` 等贡献点仍按第 5 节规划。

### 4.3 兼容策略

- 只接受 `schema = 2`；缺少 `schema` 或 `api_version` 的 v1 manifest 拒绝加载。
- 只识别 `[components.server]` / `[components.ui]` / `[components.dylib]`；顶层 `[server]` 不再识别。
- `components.dylib` 与 `components.server` 互斥；dylib 包使用 `abi` 声明 ABI 版本，缺省按 1 处理。
- `[components.ui]` 目前只作为打包声明；没有 server 组件时不会启动 UI 进程。
- 未知字段由 serde 忽略，未知贡献点暂不解析。
- 仅声明式（只有 `contributes.commands`）的包允许没有 server / ui / dylib 组件；action 命令除外，它必须有 server 或 dylib。

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

本版实现 `tools`、`commands`、`events`、`hooks`、文本 `ui` 和 `rpc`。`themes`、`keymaps`、`agents`、`skills`、`mcp_servers`、`providers`、`jobs` 的贡献声明仍保留在设计中；`jobs` 目前通过 host API 注册，不从 manifest 静态声明。

### 5.1 命名与冲突规则

- 工具：`plugin__<plugin>__<tool>`；`<tool>` 不允许包含 `__`。
- 命令：`plugin/<plugin>/<command>`；不能覆盖内置命令和用户 Markdown 命令。
- 键位：`plugin.<plugin>.<action>`；用户配置优先于插件。
- 主题：只能引用或新增 token，不能覆盖内置主题名。

### 5.2 Action 命令

命令可以只展开 prompt，也可以声明一个 action，由 plugin server 在运行时处理：

```toml
[[contributes.commands]]
name = "sync"
description = "Sync the issue cache"
args = "[--full]"
action = "sync"
```

运行时命令也可以用 `action` 字段：

```json
{
  "name": "sync",
  "description": "Sync the issue cache",
  "args": "[--full]",
  "action": "sync"
}
```

- `prompt` 和 `action` 必须恰好设置一个；action 需要 server 或 dylib 组件。
- TUI 的 `/` 菜单和命令面板可以触发；host 调用插件的 `commands/run`。
- `commands/run` 参数：`{command, arguments, session_id, workspace, cwd}`。
- 返回值：`{ok, prompt?, output?}`。`prompt` 会作为用户消息提交，`output` 展示在文本面板。
- 不开放任意 shell；需要执行命令的插件必须通过已授权 host API 或工具审批路径。

---

## 6. 授权与作用域

### 6.1 配置示例

```toml
[plugins.acme]
enabled = true
manifest = "plugins/acme/eix-plugin.toml"
timeout_secs = 30
capabilities = ["tools", "commands", "hooks", "network", "secrets", "storage"]
allowed_tools = ["search_*"]
denied_tools = ["delete_*"]

# 主机名或单个前导 `*.` 通配；留空拒绝所有请求。
network_allow = ["api.acme.com", "*.acme-cdn.com"]
# 只列出可读取的环境变量名；留空拒绝所有 secret。
secrets = ["ACME_TOKEN"]
# 允许调用的其他插件；留空拒绝所有 RPC，`"*"` 允许全部。
rpc_allow = ["issue-cache"]

[plugins.acme.settings]
project_key = "ACME"
```

原生 dylib 插件改用库入口，其余授权字段相同：

```toml
[plugins.acme]
enabled = true
library = "plugins/acme/libacme.so"
timeout_secs = 30
capabilities = ["tools", "commands"]
allowed_tools = ["search_*"]
```

`library` 不能与 `command` 同时出现；manifest 里的 `[components.dylib]` 等价于配置了 `library`。

### 6.2 规则

- manifest 的 `capabilities` 是申请，config 的 `capabilities` 是授权；配置只能收窄，不能扩大 manifest 的申请。
- 能力是开关，allowlist 是作用域：`network` 没有 `network_allow` 时拒绝一切，`secrets` 没有 `secrets` 列表时拒绝一切，`rpc` 没有 `rpc_allow` 时拒绝一切。
- 网络只允许 `https`；域名精确匹配或单个 `*.` 后缀匹配；不跟随重定向，URL 中不允许凭据。
- `secrets` 只从插件 `env` 或进程环境读取，值不写日志、不入模型上下文。
- fs 只能访问 workspace，拒绝 `..`、绝对逃逸和 symlink；单文件读写上限 1 MiB。
- RPC 目标必须在 `rpc_allow`，目标插件必须导出该方法，调用链深度上限 8，超时 60s。
- `tools.read_only` 只有在显式信任时才生效；否则插件工具一律按可写工具走审批。
- 项目层默认不能授权插件；插件只来自可信全局配置。
- 多个 workspace 共用插件进程时，host 方法按调用时的 workspace 校验。

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
  "features": [
    "content_blocks","cancel","hooks","ui","rpc","jobs",
    "fs","net","secrets","session_read","blobs","command_actions"
  ],
  "capabilities": ["tools","commands","hooks","events","storage"],
  "workspace": "/repo",
  "config": {"data_dir":"...","settings":{"project_key":"ACME"}}
}}
```

```json
{"jsonrpc":"2.0","id":1,"result":{
  "name":"acme","version":"2.1.0","api_version":2,
  "features":["content_blocks","cancel","hooks"],
  "tools":[],
  "commands":[{"name":"sync","action":"sync"}],
  "events":["session.*"],
  "hooks":[{"point":"tool.before","priority":10,"on_error":"deny"}],
  "ui":[{"surface":"panel","id":"issues","title":"Issues"}],
  "rpc":[{"name":"issues.search","description":"Search cached issues"}]
}}
```

- `features` 协商：host 只调用双方都支持的扩展方法。
- `api_version` 当前是整数，必须与 host 完全一致；不匹配拒绝启动。
- 非空 `hooks` 需要 `hooks` 能力；非空 `ui` 需要 `ui` 能力；非空 `rpc` 需要 `rpc` 能力，否则启动失败。
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
| `host.log` | — | 写 stderr，支持请求和通知两种形式 |
| `host.info` | — | 返回插件名、API 版本、能力、workspace 与 allowlist |
| `host.ping` | — | 返回 `{"pong":true}` |
| `host.storage.get/set/delete/list` | `storage` | 复用现有 KV，按 global/workspace 分区；key ≤512 B，value ≤1 MB |
| `host.fs.read` / `list` | `workspace_read` | 只能访问 workspace，拒绝 `..`、逃逸与 symlink；单文件 ≤1 MiB |
| `host.fs.write` | `workspace_write` | 同上；父目录必须已存在 |
| `host.net.request` | `network` | 仅 `https`，域名 allowlist，禁止重定向与 URL 凭据；请求 ≤1 MiB，响应最多 4 MiB |
| `host.secrets.get` | `secrets` | 只读取 `plugins.<name>.secrets` 列出的环境变量 |
| `host.session.get` / `list` | `sessions_read` | 只读 session 元数据 |
| `host.notify` | `ui` | 发送 `plugin.notify`，TUI 显示 toast |
| `host.ui.open` / `update` / `close` | `ui` | 文本 `panel` / `status` 表面，正文 ≤256 KiB |
| `host.rpc.call` | `rpc` | 调用 allowlist 内其他插件导出的方法 |
| `host.blob.put` / `get` | `storage` | 内容寻址（SHA-256），存在插件数据目录，单个 ≤8 MiB |
| `host.job.register/trigger/unregister` | `jobs` | 最多 32 个 job，间隔 1..=86400s，通过 `job.run` 通知插件 |

错误码统一：`-32601 method_not_found`、`-32602 invalid_params`、`-32001 unauthorized`。超时与取消由 client 层处理，使用 `-32003 timeout` / `-32004 cancelled` 的预留在协议中。

### 7.6 插件间 RPC

```text
plugin A -> host.rpc.call {plugin:"acme", method:"issues.search", arguments:{...}}
host     -> plugin B: rpc.call {method, arguments, caller, depth}
plugin B -> host: result
host     -> plugin A: result
```

- 调用方必须获得 `rpc` 能力，且目标在 `plugins.<caller>.rpc_allow` 中。
- 目标插件必须在 `initialize.rpc` 中导出该方法；方法名使用插件内命名空间。
- 调用链深度上限 8；每次调用超时 60s。
- 目标插件若要继续调用其他插件，应把收到的 `depth` 原样传回 `host.rpc.call`（Rust SDK 用 `Ctx::rpc_call_with_depth`）。
- 进程与 dylib 后端都实现了 `rpc.call`；dylib 侧由 `Plugin::rpc` 处理 `RpcCall { method, arguments, caller, depth }`。
- 每次调用记录 caller / callee 与失败原因到 daemon 日志（不记录参数内容）。

---

## 8. Hooks

事件是只读通知，hook 是可参与决策的请求，两者不混用。

### 8.1 Hook 点

| Hook | 时机 | 能做什么 | 失败策略 |
| --- | --- | --- | --- |
| `tool.before` | 正常权限判定之后、审批与执行之前 | 返回 `deny` 否决工具；不能批准 | `on_error` = `allow` / `deny` / `ignore` |
| `tool.after` | 工具执行完成、记录结果之前 | 用 `output` 替换工具输出 | 记录错误并继续 |
| `prompt.build` | 用户 prompt 提交、写入事件之前 | 用 `prompt` 替换用户消息 | 记录错误并继续 |
| `turn.stop` | 模型给出最终回答、turn 完成之前 | 返回 `block` 阻止完成，`reason` 会作为提醒进入下一轮 | `on_error` = `allow` / `deny` / `ignore`；`deny` 视为阻塞 |
| `turn.end` | turn 成功或失败之后 | 只读通知；作为 `hooks/run` 请求发送 | 记录错误并继续 |

hook 在 `initialize.hooks` 中声明；声明非空必须获得 `hooks` 能力：

```json
{"point":"tool.before","priority":10,"timeout_ms":500,"on_error":"deny"}
```

`timeout_ms` 为 0 时使用插件的请求超时，最大 300000ms。

### 8.2 返回值

```json
{"action":"deny","reason":"missing issue id"}
{"deny":true,"reason":"generated file"}
{"output":"redacted tool output"}
{"prompt":"rewritten user prompt"}
{"block":true,"reason":"tests are still failing"}
```

规则：

- `tool.before` 的 `deny` / `action: "deny"` / `decision: "deny"` / `allow: false` 都表示否决；`reason` 会进入工具结果。
- `tool.after` 的 `output` 替换当前输出，并受插件输出上限约束。
- `prompt.build` 的 `prompt` 为空或超限时忽略；其余 hook 看到的是上一条 hook 的结果。
- `turn.stop` 的 `block` / `action: "block"` / `decision: "block"` 会阻止 turn 结束；`reason` 作为系统提醒注入下一轮。同一 turn 最多阻塞 2 次，避免插件把 turn 永久挂起。子 agent 的 turn 不运行该 hook。
- `turn.end` 的返回值被忽略，hooks 不能改变 turn 结果。

### 8.3 顺序、超时与并发

- 顺序：按 `(priority, plugin_name)` 升序串行执行。
- 超时：每个 hook 可单独设置；超时后按 `on_error` 处理，`tool.before` 的 `deny` 策略会否决工具。
- 预算：没有跨 hook 的总预算；每个插件受自身请求超时与 hook 超时约束。
- `turn.end` 在后台任务中发送，不阻塞 turn 完成。
- `turn.stop` 会阻塞 turn 完成，因此超时会计入用户等待；保持 hook 快速。
- hook 内触发的工具调用不会再触发同类 hook（调用来自插件进程，不进入 turn 工具管线）。

### 8.4 审批权

审批权只属于 host 和用户。hook 最多能否决或改写输出，不能返回 `allow` 绕过审批。`tool.before` 在正常权限判定之后运行，因此不会把 `ask` 降级为自动允许。

---

## 9. UI 贡献

### 9.1 已实现的文本表面

- 插件不输出 ANSI、不直接操作终端。
- `initialize.ui` 可声明 `panel` / `status` / `picker`；`plugin.ui.list` 把它们暴露给前端。
- `host.ui.open` / `update` 发送 `{plugin, id, surface, title, body}`，TUI 打开或更新文本面板。
- `host.ui.close` 关闭匹配插件的前缀面板。
- `host.notify` 发送 `{plugin, level, message}`，TUI 显示 toast。
- 正文是纯文本或字符串数组，单条 ≤256 KiB。

### 9.2 Surface

| Surface | 现状 | 说明 |
| --- | --- | --- |
| `panel` | 已实现 | 全屏文本面板，Esc 关闭 |
| `status` | 已实现 | 目前作为文本面板打开，不常驻 statusline |
| `picker` | 声明/列出 | `plugin.ui.list` 可见，`/plugins <name>` 会标注；实际交互仍待 `ui.render` / `ui.action` |
| `notification` | 已实现 | `host.notify` toast |

`plugin.ui.list` 返回 `{surfaces: [{plugin, surface, id, title}]}`。TUI 在 `/plugins` 和 `/plugins <name>` 中展示声明，并据此解释 `plugin.ui.open` 事件；真正由插件驱动选择器还需要下一步的自定义视图树。

### 9.3 自定义视图树（后续）

```json
{
  "kind": "list",
  "id": "issues",
  "title": "Issues",
  "items": [{"id": "42", "label": "#42 Fix parser"}],
  "actions": [{"id": "open", "label": "Open", "key": "Enter"}]
}
```

规划中的节点：`text`、`markdown`、`list`、`table`、`tree`、`tabs`、`form`、`diff`、`log`、`progress`、`sparkline`、`actions`、`spacer`。限制：最大 2000 节点、深度 16、单文本 64 KiB；未知 `kind` 渲染为错误占位。

### 9.4 设置表单

插件在 manifest 中声明 `settings = "settings.schema.json"`：

```json
{
  "type": "object",
  "properties": {
    "project_key": {"type": "string", "title": "Project key"},
    "auto_sync": {"type": "boolean", "default": true}
  },
  "required": ["project_key"]
}
```

- schema 必须位于插件目录内、`type: object`、≤256 KiB。
- `/plugins <name>` 调用 `plugin.settings`，展示 schema、当前值、类型、必填项与 enum 选项。
- `plugin.settings.set {name, values}` 做浅合并（`null` 删除键），按 schema 校验后写回全局/显式配置文件；注释与无关键由 `toml_edit` 保留。
- 写入成功后运行中的插件收到 `config.changed`（dylib 走 `Plugin::config_changed`），daemon 自身写文件不会触发全量 `/reload`。
- 当前不提供 TUI 内编辑；修改仍可通过 `plugin.settings.set` 或直接编辑 `[plugins.<name>.settings]`。
- 设置值随 `initialize` 传给插件；`config.changed` 让运行中的插件立即看到新值。

---

## 10. 安全模型

### 10.1 信任分层

| 层 | 默认 | 说明 |
| --- | --- | --- |
| 内置工具 | 可信 | 现有风险分类 |
| dylib 插件 | 完全可信 | 在 `eixd` 地址空间内运行；仅加载签名/受信任的库 |
| 声明式插件 | 可信但受路径约束 | 只能贡献静态内容 |
| 服务插件 | 不可信 | 工具默认审批；hook 不能批准 |
| UI 插件 | 不可信 | host 渲染，节点受限 |
| 项目级插件 | 默认禁止 | 需显式 trust 或签名 |

### 10.2 能力边界

- `workspace_read` / `workspace_write`：workspace root，拒绝 `..`、绝对逃逸和 symlink；单文件 ≤1 MiB。
- `network`：只允许 `https`，域名 allowlist，host 代理请求；不跟随重定向，URL 不含凭据。
- `secrets`：按环境变量名授权，值不写日志、不进入模型上下文。
- `tools.read_only`：只有显式信任才生效。
- `hooks`：按 hook 点授权；hook 不能批准工具。
- `rpc`：调用方与目标双向受 allowlist/导出方法约束。
- `storage` / `blob`：按插件名隔离；blob 存在插件自己的数据目录。
- `jobs`：最多 32 个，间隔 1..=86400s。

能力限制的是 host API，不是操作系统。插件进程仍以 daemon 用户身份运行，可以直接读写该用户可访问的文件和网络。需要强隔离时必须启用 sandbox，或使用单独用户 / 容器运行 daemon。

dylib 插件尤其如此：它与 daemon 同进程，可以直接 `libc::open` / 发网络 / 读环境变量，绕过所有 `host.*` 检查。因此 dylib 必须视为完全可信代码，安全边界只能放在加载前（签名、安装路径、ABI/平台校验）。

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

- `eix plugin pack <dir>` 生成 `eix-plugin.sums`：每行 `<sha256>\t<relative/path>`，覆盖除 checksums / 签名以外的全部文件。
- `eix plugin verify <dir> [--allow-unsigned]` 校验 checksums，并在存在 `eix-plugin.sig` 时用 trust store 验证 detached Ed25519 签名。
- 签名覆盖 `eix-plugin.sums` 的原始字节；默认签名文件为 `eix-plugin.sig`，可用 `[package] signature` 覆盖。
- `eix plugin trust <publisher> <base64-public-key>` / `untrust` 管理全局 `plugin-trust.toml`。
- `eix plugin install <dir> [--force] [--allow-unsigned]` 先验签再复制到 `~/.config/eix/plugins/<name>`；不自动修改配置，只打印 `[plugins.<name>]` 片段。
- 安装拒绝 symlink、路径逃逸和未列入 checksums 的额外文件；未签名安装必须显式 `--allow-unsigned`。
- 尚无 registry、lockfile 与自动升级；升级需要重新 pack / 签名 / install。

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

- `degraded`：连续 3 次后端调用失败（工具 / 命令 / hook），插件仍可用，成功一次自动回到 `ready`；`plugin.list` 暴露 `failures` 计数与最后一次错误。
- `failed`：启动失败或崩溃，保留错误并允许 restart。
- `stopped`：用户禁用或 daemon 退出。

### 11.2 启动与重启

- 已实现：启动失败标记 `failed`、`plugin.restart`、崩溃检测（`exit_reason`）。
- 规划中：`lazy = true` 惰性启动、事件订阅者 eager 除外、1s/2s/4s 退避与 circuit breaker、`/reload` 配置 diff 只重启变化的插件。

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
    pub package: Option<PluginPackage>,
}

pub struct PluginPackage {
    pub publisher: Option<String>,
    pub signature: Option<PathBuf>,  // 默认 eix-plugin.sig
    pub checksums: Option<PathBuf>,  // 默认 eix-plugin.sums
}

pub struct PluginContributions {
    pub commands: Vec<ManifestCommand>,
}

pub struct ManifestCommand {
    pub name: String,
    pub description: String,
    pub args: String,
    pub prompt: String,              // prompt 与 action 二选一
    pub action: Option<String>,
}

pub struct PluginConfig {
    pub command: String,
    pub args: Vec<String>,
    pub library: Option<PathBuf>,    // 原生 dylib；与 command 互斥
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub timeout_secs: u64,
    pub cwd: Option<PathBuf>,
    pub manifest: Option<PathBuf>,
    pub capabilities: Vec<PluginCapability>,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub settings: BTreeMap<String, serde_json::Value>,
    pub network_allow: Vec<String>,
    pub secrets: Vec<String>,
    pub rpc_allow: Vec<String>,
}
```

握手结果中的 `hooks` / `ui` / `rpc` 使用强类型 `PluginHook` / `PluginUi` / `PluginRpc`；manifest 只声明能力，具体 hook 点、UI 表面和 RPC 方法在 `initialize` 返回。

### 12.2 配置

当前 `[plugins.<name>]` 支持：

- `command` / `args` / `env` / `cwd`：进程入口；与 `manifest` 二选一或组合。
- `library`：原生 dylib 路径；相对路径按 config cwd 解析，与 `command` / `components.server` 互斥。使用 dylib 时必须同时授予它实际调用的能力（如 `tools` / `commands`）。
- `enabled` / `timeout_secs`：启停与请求超时（秒）。
- `manifest`：`eix-plugin.toml`，相对路径按 config cwd 解析；manifest 的 `name` 必须与配置 key 一致。manifest 含 `components.dylib` 时等价于配置了 `library`。
- `capabilities`：只能收窄 manifest 的申请；留空表示采用 manifest 的申请或 `[tools, commands]`。
- `allowed_tools` / `denied_tools`：`*` / `?` 通配符，deny 优先。
- `settings`：任意 JSON 值，随 `initialize` 传给插件。
- `network_allow`：精确域名或单个前导 `*.` 通配；空列表拒绝所有网络请求。
- `secrets`：可读取的环境变量名；空列表拒绝所有 secret。
- `rpc_allow`：可调用的其他插件名；`"*"` 允许全部，空列表拒绝所有 RPC。

项目级配置仍然不能添加或修改插件。

### 12.3 数据库

- `plugin_kv`：已用于 storage，按插件名与 global/workspace scope 隔离。
- `plugin_audit`、`plugin_installs`：规划中，尚未实现。
- secrets 只从配置选择的环境变量读取，不落 SQLite。

---

## 13. 可观测性与开发体验

### 13.1 运维

- 每插件 ring buffer（最近 500 行）、`plugin.logs {name, since}`：规划中。
- `plugin.list` 已返回 state / tools / commands / hooks / ui / rpc / events / capabilities / failures / error。
- `plugin.settings {name}` 返回 schema 与当前值；`plugin.settings.set {name, values}` 校验并写回配置。
- `plugin.ui.list` 返回已声明的 `{plugin, surface, id, title}` 表面，供 TUI 预知面板与 picker。
- `plugin.doctor`：规划中。
- TUI `/plugins [name]`：列表、贡献计数、错误；带 name 时展示 settings schema。
- `/reload` 仍会重启全部插件；按 diff 重启尚未实现。
- statusline 在插件 failed/degraded 时显示标记：规划中。

### 13.2 开发

- `eix plugin new` / `run` / `test`：规划中。
- `eix plugin pack/verify/install/trust/untrust`：已实现。
- SDK：`eix-plugin` Rust crate、Python package、TypeScript package：规划中；`eix-sdk` 是 daemon HTTP client。
- 文档：本文件与 `crates/eix-core/src/plugin.rs` 的类型定义。
- 示例：`examples/plugins/echo/` 覆盖 tools、commands、action、events、storage。

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

### Phase 2：Hooks + 声明式插件 + Action 命令（核心已完成）

- hook 点、顺序、超时、失败策略已落地；审计表尚未实现。
- `commands` 支持 action。
- 声明式 themes / keymaps / agents / skills / mcp_servers 仍未实现。
- settings schema 已解析，`plugin.settings.set` 校验后写回配置并通过 `config.changed` 热更新；TUI 仍只读展示。
- 连续失败 3 次进入 `degraded`，成功一次恢复。

### Phase 3：UI 贡献（文本表面已完成）

- `panel` / `status` / `notification` 已落地；`picker` 可声明并通过 `plugin.ui.list` 列出，自定义 `UiNode`、`ui.render` / `ui.action` 待实现。
- `/plugins [name]` 管理器已落地，并展示插件声明的 UI 表面。
- 验收：插件能打开/更新/关闭文本面板，JSON 不被当作 ANSI 执行。

### Phase 4：安全与分发（核心已完成）

- fs / network / secrets / RPC 的 allowlist 已落地。
- 可选 strict sandbox 未实现。
- `pack` / `verify` / `install` / Ed25519 签名 / trust store 已落地；lockfile 与 registry 未实现。

### Phase 5：高级扩展（部分完成）

- 插件间 RPC 与 jobs 已落地；provider adapter、WASM 可选层未实现。

---

## 16. 关键决策

| 决策                | 结论     | 理由                                     |
| ---                 | ---      | ---                                      |
| 进程内 dylib        | 可信 opt-in | 用 C 边界 + 5 个符号 + JSON dispatch 保持演进；只加载受信任库，默认仍用进程后端 |
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
| `crates/eix-core/src/plugin.rs`      | manifest v2、配置、JSON-RPC、content blocks、hooks/ui/rpc、action/settings 类型 |
| `crates/eix-core/src/plugin_package.rs` | `pack` / `verify` / `install`、checksums、Ed25519、trust store             |
| `crates/eixd/src/plugin/mod.rs`      | `PluginManager`、注册表、生命周期、hooks 管线、RPC 路由、声明式/action 命令、backend 选择、`degraded` 计数、settings 写入 |
| `crates/eixd/src/plugin/backend.rs`  | `ExtensionBackend` trait 与 `BackendFuture`                                |
| `crates/eixd/src/plugin/process.rs`  | `ProcessBackend`：JSON-RPC 子进程适配器                                    |
| `crates/eixd/src/plugin/dylib.rs`    | `DylibBackend`、ABI 校验、方法翻译、超时、dylib host API                    |
| `crates/eixd/src/plugin/client.rs`   | 进程 spawn、writer/reader、请求多路复用、超时取消、事件投递、features       |
| `crates/eixd/src/plugin/host_api.rs` | `HostContext`、共享 `WorkspaceFs`/blob、fs/net/secrets/session/ui/rpc/job 与能力校验 |
| `crates/eixd/src/server.rs`          | `plugin.command.run` / `plugin.settings[.set]` / `plugin.ui.list`、watcher 自写抑制、turn/tool/prompt hook 接入 |
| `crates/eixd/src/classifier.rs`      | `RiskCategory::Plugin` 保留；插件工具默认审批                              |
| `crates/eix-tui/src/main.rs`         | `eix plugin pack/verify/install/trust/untrust` CLI                         |
| `crates/eix-tui/src/tui/app/`        | `/plugins [name]`、action 命令分发、`plugin.ui.*` 文本面板与 toast          |
| `crates/eix-core/src/config.rs`      | `plugins` 解析与 allowlist 校验；项目级 `plugins` 仍被删除                  |
| `examples/plugins/echo/`             | v2 示例：JSON-RPC 2.0、content blocks、storage、events、action              |
| `examples/plugins/hello-dylib/`      | dylib 示例：ABI v1、`echo` 工具、`sync` action 命令                          |

hooks / UI / RPC 的接入点见第 7、8、9 节；未实现的只有自定义视图树、settings 编辑与 lockfile。

## 19. 后续最小步骤

1. 审计与可观测性：`plugin_audit`、ring buffer、`plugin.logs`。
2. 更多声明式贡献：skills / agents / themes / keymaps / MCP。
3. Python / TypeScript SDK 与协议一致性测试。
4. 自定义 UI 视图树、picker provider、TUI 内 settings 编辑（`plugin.settings.set` 已可用）。
5. lockfile、registry、按 diff 重启与可选 OS sandbox。
