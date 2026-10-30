# eix

> 为 vim / dwm / Arch 工作流打造的 coding agent。
> 持久多 session、Vim 原生输入、多 pane TUI，以及 CLI / HTTP / 可替换前端。

`eix` 由常驻 daemon `eixd` 和前端 `eix` 组成。session、事件日志、工具执行、审批和上下文都在
daemon 中维护；TUI、`eix chat`、`eix send` 和 HTTP SDK 只是可替换的前端，Neovim 通过协议接成
外部插件。关掉终端不会中断正在运行的 turn，下次启动或 attach 即可继续。

[设计文档](./DESIGN.md) · [完整配置模板](./config.tmpl.toml) · [插件示例](./examples/plugins/echo) ·
[插件体系设计](./docs/plugins.md) · [Neovim 集成](./docs/nvim.md) · [协议契约](./crates/eix-core/src/protocol.rs)

## 为什么是 eix

- **session 是一等公民**：常驻、可 attach/detach、可并行、可模糊切换；关终端不影响运行。
- **Vim 原生输入**：输入框是内置的模态编辑器，不启动 Neovim 子进程，按键零往返。
- **默认安全**：Plan / Build / Auto / Full 四种执行模式，配合风险分析、bubblewrap 沙箱、
  补丁审批、执行前快照和 secret 脱敏。
- **上下文自动管理**：`AGENTS.md`、workspace summary、工具输出预算、自动压缩和跨 session 记忆
  不需要手动维护。
- **可组合**：OpenAI 兼容多 provider、Skills、Markdown 命令与 agent、MCP、外部插件。
- **不止 TUI**：同一 daemon 支持 CLI、HTTP JSON-RPC / SSE、Rust SDK、TCP 远程 attach，
  以及给 Neovim 插件用的 JSON-RPC 接口。

## 功能一览

| 领域 | 能力 |
| --- | --- |
| Session | 事件日志 + SQLite 持久化、自动命名、attach/detach、fork / branch / rewind、rewind/redo（对话 + 工作区文件）、export/import |
| TUI | 多 pane 自动平铺、三种布局、统一 fuzzy picker、Markdown 转录、side-by-side diff、审批收件箱 |
| 输入 | 内置 Vim 风格编辑器（normal / insert / visual）、leader 键、可配置快捷键、命令面板 |
| 执行模式 | `plan` 只读、`build` 工作区写入、`auto` 意图分类、`full` 放行 |
| 安全 | 风险分析、显式规则、bubblewrap 沙箱、补丁审批 + 执行前快照、secret 占位符 |
| 工具 | `apply_patch` 编辑、`exec_command` / `write_stdin` shell、`update_plan` 计划、`tool_search` 按需发现 MCP/插件工具、web、LSP、图片、子 agent |
| 上下文 | sector 预算、AGENTS.md、workspace summary、自动 compact、memory / plan |
| 扩展 | Skills、Markdown 命令、Markdown agent、MCP stdio / HTTP、out-of-process 插件 |
| Provider | 多 endpoint（OpenAI 兼容 / Anthropic / Gemini / DeepSeek Messages 原生协议）、API key + OAuth 登录、自动拉取模型列表、重试/退避、运行时模型切换与 reasoning 强度、thinking 回放与 cache/reasoning token 计量 |
| 前端 | TUI、chat/send CLI、HTTP + SSE + `eix-sdk`、TCP，以及外部编辑器插件接口 |

## 快速开始

### 环境

- Rust stable（最低 1.89）与 Cargo
- Linux 或 macOS；Windows 暂不支持
- 可选：`bwrap`（bubblewrap）用于命令沙箱

### 构建

```sh
git clone https://github.com/zetatez/eix.git
cd eix
cargo build --release
./target/release/eix doctor
```

`cargo build` 会同时构建 `eix`（TUI + CLI）和 `eixd`（daemon）。daemon 由 `eix` 在第一次连接时
自动拉起，使用独立进程组，关闭终端不会杀掉它。下文命令假设 `eix` 已在 `PATH` 中；直接从源码
运行时请使用 `./target/release/eix`。

### 配置 provider

最小配置（和下面的本地配置二选一；`providers` 中恰好一个 `default = true`）：

```toml
# ~/.config/eix/config.toml
[providers.openai]
default = true
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
```

```sh
export OPENAI_API_KEY=...
./target/release/eix
```

本地模型或兼容端点：

```toml
[providers.local]
default = true
base_url = "http://localhost:11434/v1"
model = "qwen2.5-coder:7b"
```

`kind` 选择协议，默认 `openai`（OpenAI 及所有兼容端点）。Anthropic 与 Gemini 使用各自的原生流式
接口，`base_url` 只写到服务根或版本前缀：

```toml
[providers.claude]
kind = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-sonnet-4"
api_key_env = "ANTHROPIC_API_KEY"

[providers.google]
kind = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
model = "gemini-2.5-pro"

# DeepSeek native Messages protocol: signed thinking replay, effort control
# (off/low/high/max), in-history prompt updates, and cache-token accounting.
[providers.deepseek-native]
kind = "deepseek"
base_url = "https://api.deepseek.com/anthropic"
model = "deepseek-v4-flash"
api_key_env = "DEEPSEEK_API_KEY"
```

单个 `[[models]]` 可以用 `kind` 覆盖所属 provider 的协议。模型目录（`/model`、`eix model --all`）
会按协议读取 `/models`，过滤 Gemini 不支持 `generateContent` 的模型。

可以按任务角色选择模型，例如规划用更强的模型、探索和执行共用一个便宜的模型：

```toml
[model_roles]
plan = "anthropic/claude-sonnet-4"     # Plan 模式的只读规划
execute = "anthropic/claude-haiku-4"   # 普通读写 turn、探索子代理与后台辅助调用
```

值使用与 `/model` 相同的选择 id（可带 `#variant`）。`/model` 固定 session 的
`execute` 模型，`/model-plan` 固定 Plan 模式模型；两者都只影响当前 session。
Plan 模式的优先级是 `/model-plan` → `model_roles.plan` → `/model` →
`model_roles.execute` → daemon 默认模型，因此未配置昂贵的 Plan 模型时会自动回退，
不会报错。子代理和后台辅助调用始终使用 `/model` / `model_roles.execute`，不会继承
Plan 模型；子代理自己的 `model:` 仍然优先。

密钥优先级：配置内联 `api_key` → `api_key_env` → `<data_dir>/auth/<provider>.json`。最后一项由
`eix auth login` 写入（0600 权限），daemon 每次请求时读取，因此登录/注销无需重启或 `reload`：

```sh
eix auth login openai            # 交互输入 API key
echo "$KEY" | eix auth login openai --stdin
eix auth login claude --api-key sk-...
eix auth list                    # 显示每个 provider 的凭据来源
eix auth logout openai
```

需要浏览器登录的 provider 可配置 `[providers.<name>.oauth]`（authorization / token endpoint、
`client_id`、可选 `client_secret`、`scopes`、`redirect_uri`、额外参数），`eix auth login <name>`
会走 authorization-code + PKCE + loopback 回调；token 过期前自动 refresh。

临时试用也可以只用环境变量：`EIX_BASE_URL`、`EIX_MODEL`、`EIX_API_KEY`。所有配置项、默认值和
注释见 [config.tmpl.toml](./config.tmpl.toml)。

### 第一次运行

```sh
eix                       # 默认打开 TUI；没有 session 时自动创建
eix chat                  # 纯命令行交互
eix send '解释 session 模型'
echo 'review the diff' | eix send
eix send --image screenshot.png '这个报错怎么修'
eix ls
eix log
```

当前目录没有 session 时，`eix` 会 attach 当前目录最近更新的 user session，没有则新建。stdout 被
管道时，不带子命令的 `eix` 会退回 `eix chat`。socket 默认在 `$XDG_RUNTIME_DIR/eix.sock`，没有
runtime dir 时回退到平台 cache 目录（Linux 通常 `~/.cache/eix/eix.sock`，macOS
`~/Library/Caches/eix/eix.sock`）。数据在平台 data-local 目录下的 `eix/`（Linux 通常
`~/.local/share/eix/`，macOS `~/Library/Application Support/eix/`）。可用 `EIX_SOCKET`、
`EIX_DATA_DIR`、`EIX_CONFIG` 或对应命令行参数覆盖。

## 核心概念

### Session

一个 session 是一条 append-only 事件日志，包含对话、工具调用、审批、补丁、压缩和状态变化。
TUI 可以把多个 session 同时开在不同 pane；同一 session 只会在一个 pane 中出现。`fork` 复制历史，
`/fork` 不填名字时会打开 picker，选中某条用户消息后只复制它之前的历史（也可显式选 “entire conversation”）；
`rewind` 还会按执行前快照回滚工作区（尽力而为，并明确报告无法恢复的改动）。

### 执行模式

| 模式 | 能力 |
| --- | --- |
| `plan` | 硬只读；保留只读 shell 检查，隐藏写入和后台命令 |
| `build` | 默认；workspace-write，按风险/规则决定直接执行、沙箱或询问 |
| `auto` | workspace-write；未命中规则的默认询问交给意图分类器，低置信度仍询问 |
| `full` | 解除 workspace 路径限制并放行默认询问；硬拒绝、secret 保护和显式 `deny` / `ask` 仍生效 |

在 TUI 用 `Tab` / `Shift-Tab` 循环，或 `/perm` / `Space m`；CLI 用 `eix mode`。

### 审批与工作区

`build` 模式下，工作区内低/中风险的 `apply_patch` 默认直接应用（保留快照）；需要审批的补丁
会显示完整 diff，审批卡片支持 side-by-side 审阅、hunk 级选择和带理由拒绝。`exec_command`
无法预演，执行前会抓工作区快照。所有路径默认经过 workspace 沙箱检查（`full` 例外），
所有工具决策写入审计事件。

### Worktree 与集成

默认所有 session 共享当前工作区。需要隔离时用 `eix new --worktree`，或在 TUI 用
`/new-worktree` / `Space W` 把当前 session 移入 `.worktrees/<name>`。agent 完成工作后可
`ready`，daemon 串行执行 rebase → 测试 → fast-forward merge；失败会注入原 session 进入修复循环。

### 上下文与扩展

daemon 每次请求都会从事件日志重建上下文，按 sector 分配预算，自动裁剪和压缩；`AGENTS.md`、
workspace summary、memory 和计划会自动注入。Skills、命令、agent 按需加载；MCP 与插件工具由
`tool_search` 按需发现，不会一次性塞满上下文。

## 使用

### TUI

```sh
eix [--session ID]
eix tui [--session ID]
```

常用按键：

| 按键 | 功能 |
| --- | --- |
| `F1` | 可搜索帮助 |
| `Ctrl-T` | 切换 reasoning 强度 |
| `Ctrl-/` | 模型 picker |
| `Space o` / `Space h` | session picker（已打开则聚焦，否则新 pane）/ 输入 timeline |
| `Space f` / `Space s` / `Space g` | 文件 / 内容搜索 / git picker |
| `Space p` | subagents 实时 picker（无子 agent 时打开并发上限） |
| `Space n` / `Space W` | 新建 session / 移入新 worktree |
| `Space a` / `Space b` | 审批收件箱 / 后台进程 |
| `Tab` / `Shift-Tab` | 正向 / 反向循环执行模式 |
| `Enter` | 发送 prompt；turn 运行中自动排队 |
| `Esc Esc` | 中断当前 turn，或回到转录区 |

`/model` 为当前 session 固定普通 turn 与探索子代理共用的模型；`/model-plan` 为当前
session 固定 Plan 模式模型，`/model-plan default` 清除并回退到 `model_roles.plan` 或
`/model`。`/model global <id>`（或 `eix model use <id>`）切换 daemon 的运行时默认模型，
只影响没有固定模型的 session，不写回 config，daemon 重启后恢复。
`/model global default` 清除覆盖。

`/rewind` 打开 checkpoint picker，选中后再选择回滚范围（对话、文件或两者）；也可以直接
`/rewind <n> [both|chat|files]`，`n` 从最新 user turn 倒数。`/undo` 是 `/rewind` 的兼容别名，
`/redo` 恢复最近一次 rewind。`/context` 在 overlay 中显示 token 预算与各 sector 占用
（`/context all` 附带逐条 entry）；`/effort` 打开 low/medium/high picker（`/effort high` 仍可直接设置）；
`/branch` 分叉当前对话并把当前 pane 切到新分支（原 session 仍保留在 session picker）。
`/diff` 打开 changed-files picker（右侧预览每个文件的 diff）；`/doctor` 在可滚动 overlay 中按 paths / runtime / model / tools / context 分区展示 daemon 诊断（与 `eix doctor` 共用渲染）；`/goal` 设置跨轮注入的
持久目标，`/loop` / `/schedule` 在 session 空闲时按间隔或指定时间再次发送 prompt；
`/code-review`、`/security-review`、`/simplify`、`/verify` 是内置的审查与验证 prompt 模板。

完整键位、prompt 命令、审批卡片、picker 和鼠标操作见
[DESIGN.md 的实现参考](./DESIGN.md#19-实现参考)。

### CLI

| 命令 | 用途 |
| --- | --- |
| `eix tui` / `eix chat` | 全屏 TUI / 纯命令行交互 |
| `eix send [--image PATH] TEXT` | 发送单条 prompt 并流式输出 |
| `eix ls` / `eix log` | 列出 session / 打印持久化转录 |
| `eix new [--worktree] [--mode MODE]` | 新建 session |
| `eix mode MODE` | 切换执行模式 |
| `eix fork` / `eix rewind` | 复制历史 / 回滚到某个切点 |
| `eix ready` / `eix integrate` | 提交集成 / 查看或继续集成队列 |
| `eix worktree list\|remove` | 管理 session worktree |
| `eix approve` / `eix deny` / `eix answer` | 无头处理审批与提问 |
| `eix ps` / `eix tail` / `eix kill` | 后台进程 |
| `eix status` / `eix approvals` / `eix pick` | 脚本和 dwmblocks / rofi / fzf 集成 |
| `eix auth login\|list\|logout` | 保存 provider API key 或 OAuth 登录、查看凭据来源、注销 |
| `eix model [show] [--all]` / `eix model use <id>` / `eix model default` | 查看模型目录与 provider 认证状态 / 切换 daemon 运行时默认模型 / 恢复配置默认值 |
| `eix mcp auth <server>` / `eix mcp logout <server>` | 远程 MCP server 的 OAuth 授权 / 注销 |
| `eix pair` / `eix pair list\|revoke` | 为新客户端生成一次性配对码 / 管理已配对客户端 |
| `eix acp` | 以 Agent Client Protocol 适配器运行（stdio，供编辑器启动） |
| `eix doctor` | 诊断配置、路径、沙箱和 daemon |

完整命令和参数见 `eix --help`。

### HTTP API、SDK 与远程 attach

只监听 Unix socket 时，连接靠文件系统权限保护。需要脚本、Web 前端或跨机器访问时，可以打开
HTTP 和/或 TCP：

```toml
[server]
http = "127.0.0.1:7778"
tcp = "127.0.0.1:7777"
token = "{env:EIX_TOKEN}"
```

```sh
export EIX_TOKEN=...
curl -s localhost:7778/health
curl -s localhost:7778/openapi.json | jq '.paths | keys'
curl -s localhost:7778/rpc \
  -H "Authorization: Bearer $EIX_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"id":1,"method":"session.list","params":{}}'

eix --tcp 127.0.0.1:7777 --token "$EIX_TOKEN" tui
```

HTTP 提供 `/health`、`/openapi.json`（OpenAPI 3.1）、`/stats`、`POST /rpc` 和 SSE `/events`。
新客户端不必复制静态 token：在本机运行 `eix pair --name laptop` 生成 5 分钟有效的一次性配对码，
客户端把它 `POST /pair`（无需认证）兑换成持久 token；daemon 只保存 token 的 SHA-256，`eix pair list`
/ `revoke` 管理，撤销立即生效。失败的兑换按 60 秒窗口限流。

```sh
code=$(eix pair --name laptop | awk '/pairing code/{print $3}')
curl -s localhost:7778/pair -H 'content-type: application/json' \
  -d "{\"code\":\"$code\"}"   # => {"token":"...","client":{...}}
```

Rust 项目可以直接依赖 [`eix-sdk`](./crates/eix-sdk)，TypeScript 项目用无依赖的
[`clients/ts`](./clients/ts)（fetch + SSE，含 session / prompt / approval / pairing）。
远程端点不会自动拉起本地 daemon；详细认证、事件过滤和 API 说明见
[DESIGN.md](./DESIGN.md)。

### Neovim 插件

eix 不内置 Vim/Neovim 前端。插件通过 daemon 的 JSON-RPC（Unix socket 或 TCP）接入，自己负责
buffer、窗口、键位和 picker；接口、消息格式、常用方法与最小 Lua 传输层见
[docs/nvim.md](./docs/nvim.md)。HTTP/SSE 也可以作为插件的传输方式。

### 编辑器集成（ACP）

`eix acp` 在 stdio 上实现 [Agent Client Protocol](https://agentclientprotocol.com/) v1，供 Zed 等
编辑器直接启动。适配器只做翻译：session、事件日志、worktree、MCP 和审批仍然由 daemon 持有。

示例（Zed `settings.json`）：

```json
{
  "agent_servers": {
    "eix": {
      "command": "eix",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

已支持：文本 prompt（含内嵌文本/资源链接）、流式回复与思考、工具调用状态、审批
（`session/request_permission`）、`session/load` 历史回放、`session/close`、模式切换
（`plan` / `build` / `auto` / `full`）与取消。图片、音频、终端代理和编辑器传入的
`mcpServers` 暂不支持；MCP 请在 eix 配置里声明。

## 配置

eix 自己的全局文件统一在 `~/.config/eix/`（macOS 也是）：`config.toml`、`AGENTS.md`、
`skills/`、`cmds/`、`agents/`。`EIX_CONFIG_DIR` 可覆盖这个目录。

配置分层（daemon 从启动目录向上查找）：

1. `~/.config/eix/config.toml`（或 `EIX_CONFIG` / `--config`；显式路径不再合并项目层）
2. 祖先目录的 `.eix/config.toml`，从最外层到最近层依次覆盖

表递归合并，数组整体替换。全局层和显式配置支持 `{env:NAME}` 与 `{file:path}`；项目层不展开
这两个占位符。daemon 每 2 秒检查 mtime，变更后热重载；解析失败保留旧配置并通过 `config.error`
通知，TUI 里也可以 `/reload`。

项目层只能收紧策略：会扩大权限或影响信任的字段会被丢弃；`allow` / `sandbox` 规则降级为 `ask`；
项目 `cli.json` 不能把默认模式设为 `full`。项目配置永远不能添加插件、开启其他 agent 的兼容
资源，也不能扩大沙箱或网络权限。被拒绝的项会写入 `security_warnings`，可用 `eix doctor` 查看。

常用配置段：

| 段 | 作用 |
| --- | --- |
| `providers` / `provider_runtime` | provider（`kind` = openai/anthropic/gemini、可选 `oauth`）、默认模型、超时、重试、`max_output_tokens`、系统提示、`prompt_profile`、`persistent_mode` |
| `models` / `model_roles` | 固定模型目录，以及 `plan` / `execute` 任务角色模型（`execute` 同时用于探索子代理） |
| `tools` | 自动应用策略、显式规则、沙箱、网络、快照、外部目录 |
| `context` | token 预算、指令文件、workspace summary、自动压缩 |
| `worktree` / `integration` | worktree 基线、自动 ready、自动提交、测试命令 |
| `compations` | 兼容 Claude Code / OpenCode / Codex CLI / Gemini CLI / Pi 的资源 |
| `mcp` / `plugins` | MCP server 与 out-of-process 插件 |
| `images` / `formatter` / `lsp` | 图片保留、写入后格式化、语言服务器 |
| `theme` / `keys` / `notifications` | TUI 主题、快捷键、外部通知 |
| `server` | HTTP / TCP 与 token |

环境变量只覆盖启动、连接、模型和 CLI 初始值：`EIX_BASE_URL`、`EIX_MODEL`、`EIX_API_KEY`、
`EIX_SOCKET`、`EIX_DATA_DIR`、`EIX_CONFIG`、`EIX_CONFIG_DIR`、`EIX_CLI_CONFIG`、
`EIX_TCP`、`EIX_TCP_TOKEN`、`EIX_HTTP`。其余配置写入 `config.toml`。

## 扩展

| 扩展 | 位置 / 机制 |
| --- | --- |
| Skills | `~/.config/eix/skills/<name>/SKILL.md`、项目 `.eix/skills/`；按需加载正文 |
| 命令 | `~/.config/eix/cmds/*.md`、项目 `.eix/cmds/*.md`；`$ARGUMENTS` / `$1..$9` |
| Agent | `~/.config/eix/agents/*.md`、项目 `.eix/agents/*.md`；`primary` / `subagent` |
| MCP | `mcp.servers.<name>`；stdio / Streamable HTTP、懒启动、`mcp__server__tool`、resources / templates / prompts；远程可用 `eix mcp auth <server>` 走 OAuth |
| 插件 | `plugins.<name>`；`eix-plugin.toml`（schema 2）+ JSON-RPC 2.0 over stdio，独立子进程；声明式命令可无进程 |

兼容资源默认全部关闭，可在 `compations:` 中逐项打开。项目配置只能关闭、不能开启。示例见
[examples/plugins/echo](./examples/plugins/echo)，完整说明见 [DESIGN.md](./DESIGN.md)。

## 架构与项目结构

```
┌──────────────┐   JSON-RPC / SSE   ┌────────────────────────────┐
│ eix (TUI/CLI)│ ─────────────────> │ eixd                       │
│ 外部前端     │ <───────────────── │ sessions / event log       │
│ HTTP / SDK   │                    │ agent runtime / tools      │
└──────────────┘                    │ approvals / context / git  │
                                    └────────────────────────────┘
```

```
crates/
├── eix-core   共享类型：session、事件、协议、配置、风险分析
├── eixd       daemon：session registry、SQLite、agent runtime、工具、MCP、插件
├── eix-sdk    HTTP JSON-RPC / SSE 类型化 Rust client
└── eix-tui    `eix` 前端：TUI + chat / send / log / picker CLI
```

协议以 `crates/eix-core/src/protocol.rs` 为唯一契约来源；设计决策、状态机和完整实现参考见
[DESIGN.md](./DESIGN.md)。

## 构建与打包

需要 Rust stable、`cargo`、`make`。Arch 上还需要 `base-devel`；打 Debian/Ubuntu 包需要
`dpkg-deb`；macOS 包必须在 macOS 上构建。

```sh
make release                  # 构建当前 OS/arch 并生成原生安装包
make dist                     # 构建当前 OS 支持的所有格式
make package-arch             # Arch Linux
make package-deb              # Debian/Ubuntu
make package-tar              # 通用 tar.gz
make package-darwin           # macOS tar.gz + Homebrew formula
```

产物都在 `dist/`。`make release` / `make package` 只针对当前主机和架构；跨平台 release 由
[`.github/workflows/release.yml`](./.github/workflows/release.yml) 的 runner matrix 生成
Linux x86_64/aarch64、macOS Intel/Apple Silicon 和 Arch x86_64。Debian 包建议在目标发行版或
CI 的 Ubuntu runner 上构建；CI 使用 Ubuntu 22.04 作为 glibc baseline，macOS 使用
`MACOSX_DEPLOYMENT_TARGET=11.0`。

## 开发

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI 在 Linux 和 macOS 上跑同一套测试，并用 Rust 1.89 做 MSRV 检查；没有 `git` 的环境会自动
跳过相关测试。协议变更请同时更新 `crates/eix-core/src/protocol.rs` 和
[DESIGN.md](./DESIGN.md)。

## 实现状态

核心闭环已经可用：daemon、SQLite 事件日志、流式 OpenAI 兼容 provider、多 pane TUI、内置 Vim
编辑器、工具与审批、沙箱/快照/补丁写入、自动上下文、fork/rewind、worktree、集成队列、子 agent、
MCP、插件、Skills、HTTP/SDK、TCP 远程，以及外部 Neovim 插件所需的协议接口。完整实现清单见
[DESIGN.md 的实现状态](./DESIGN.md#20-实现状态)。

## License

MIT
