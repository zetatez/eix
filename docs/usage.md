# eix 使用总览

eix 是一个**会话常驻**的编码 agent：长期运行的 daemon（`eixd`）持有全部会话、事件日志、
工具执行与审批，前端（TUI、CLI、HTTP/SDK、Neovim、ACP）都只是它的客户端。关掉终端不会
中断正在跑的回合，换台机器也能接上同一个会话。

```sh
eix                      # 打开 TUI（默认子命令）
eix doctor               # 环境自检：配置、路径、daemon 状态
```

本文是入口级总览，专题细节在各自文档里：
[Worktree](./worktree.md) · [语音](./voice.md) · [语音配置](./voice-setup.md) ·
[插件](./plugins.md) · [dylib 插件](./plugin-dylib.md) · [Neovim](./nvim.md) ·
[设计文档](../DESIGN.md)

## 1. 安装与第一次运行

**依赖**：Rust stable 1.89+、Cargo、Linux 或 macOS（不支持 Windows）。可选 `bwrap` 用于
命令沙箱。

```sh
git clone https://github.com/zetatez/eix.git
cd eix
cargo build --release
./target/release/eix doctor
```

构建产物有三个：

| 二进制 | 作用 |
|---|---|
| `eix` | TUI + CLI，唯一的日常入口 |
| `eixd` | daemon，首次连接时自动拉起，独立进程组 |
| `eix-asr` | 可选的语音 helper（不装就没有本地转录） |

## 2. 配置一个 provider

配置文件默认在 `~/.config/eix/config.toml`。有且只有一个 provider 设 `default = true`：

```toml
[providers.openai]
default = true
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
```

```sh
export OPENAI_API_KEY=sk-...
eix
```

几种常见变体：

- **本地模型**：`base_url = "http://localhost:11434/v1"` + 本地 `model`。
- **原生协议**：`kind = "anthropic"` / `"gemini"` / `"deepseek"`，默认是 `openai`。
- **OAuth**：`eix auth login anthropic`，token 存在 daemon 侧，不必写进配置。
- **分离规划/执行模型**：`[model_roles]` 里分别设 `plan` 和 `execute`。

不想手写配置就用 CLI 管理凭据：

```sh
eix auth login openai --api-key sk-...   # 或 --stdin / --no-browser 走 OAuth
eix auth list                            # 看每个 provider 的凭据来自哪
eix model show                           # daemon 默认模型 + 授权状态
eix model use openai/gpt-4o              # 运行时切换默认模型
eix model default                        # 恢复配置里的默认
```

全部配置项见 [`config.tmpl.toml`](../config.tmpl.toml)。

## 3. 核心概念

**Session**：一次独立对话 + 一份事件日志，有自己的 `cwd`、模型、执行模式、权限策略。
可以并行跑很多个，互不干扰。

**Event log**：只追加的 SQLite 日志。上下文每次请求都从日志重建，所以 fork、rewind、
导出、多端同步都只是读日志。

**Pane**：TUI 里的一个视图窗口，一个 pane attach 一个 session。同一 session 可以被多个
pane（或多个客户端）同时 attach。

**Execution mode**：`plan` / `build` / `auto` / `full`，决定工具能做什么、什么时候要审批。
`build` 是默认值。`full` 解除工作区路径限制并允许「默认放行」类问询，但**硬性拒绝和显式
规则仍然生效**。切换方式：TUI 里 `Tab` / `Shift-Tab`，或 `eix mode build`。

**Context**：`AGENTS.md`、工作区摘要、按扇区分配的 token 预算、自动压缩、跨会话记忆，都在
daemon 侧组装。`/context` 看用量，`/effort` 调推理档位。

## 4. TUI

```sh
eix [--session ID]
```

### 键位

| 按键 | 作用 |
|---|---|
| `F1` | 可搜索帮助 |
| `F2`（按住） | 按住说话，松开停止并转录 |
| `Ctrl-p` | 命令面板 |
| `Ctrl-t` | 切换推理档位 |
| `Ctrl-/` / `Ctrl-_` / `Ctrl-7` | 模型选择器 |
| `Space o` / `Space h` | 会话选择器 / 输入时间线 |
| `Space f` / `Space s` / `Space g` | 文件 / 内容搜索 / git picker |
| `Space n` / `Space W` | 新建会话 / 搬进新 worktree |
| `Space a` / `Space b` | 审批收件箱 / 后台进程 |
| `Space v` / `Space t` / `Space r` | 横向 / 标签式 / 斐波那契布局 |
| `Space l` / `Space x` | 循环布局 / 取消当前回合 |
| `Tab` / `Shift-Tab` | 切换执行模式 |
| `Ctrl-1…9` | 聚焦第 N 个会话（需 kitty 键盘协议） |
| `Enter` / `Esc Esc` | 发送或排队 prompt / 打断并回到对话流 |
| `q` / `Z` / `Ctrl-c` / `Ctrl-d` | 关 pane / normal 模式退出 / 任意模式退出 |

输入框是原生 Vim 模态编辑器：motion、operator、计数、undo、leader 组合键都在，不需要
子进程。`[keys]` 里可改全局 / normal / leader 绑定（叠加层内部导航是固定的），值可以用
逗号分隔写多个，leader 组合写成 `new_session = "leader n"`。

### 斜杠命令

| 命令 | 作用 |
|---|---|
| `/help` `/cancel` `/exit` | 帮助 / 取消回合 / 退出 |
| `/new` `/log` | 新会话 / 重打当前转录 |
| `/model` `/model-plan` | 固定本会话的执行 / 规划模型 |
| `/rewind` `/undo` `/redo` | 文件与对话的检查点回滚 |
| `/context` `/effort` | 上下文用量 / 推理档位 |
| `/branch` `/diff` `/goal` `/loop` `/schedule` | 分支、看 diff、长目标、循环、定时 |
| `/ready` `/integrate` | 排队集成 / 查看集成状态 |
| `/new-worktree` | 把当前会话搬进新 worktree |
| `/doctor` `/voice` | 自检 / 语音输入 |
| `/code-review` `/security-review` `/simplify` `/verify` | 内置 prompt 模板 |

完整清单见 `F1` 帮助面板或 [DESIGN.md](../DESIGN.md#19-实现参考)。

### 审批与 diff

高风险补丁会弹出**逐 hunk** 的 diff，批准 / 拒绝可以带理由。`Space a` 打开跨会话的审批
收件箱；不想进 TUI 也能用 CLI：

```sh
eix approvals                      # 列出所有待批
eix approve --hunks 0,2            # 只批准第 0、2 个 hunk
eix deny --reason "换个方案"
eix answer 1                       # 回答 agent 的提问
```

## 5. CLI

CLI 和 TUI 操作的是同一批会话，适合脚本、状态栏和远程环境。

### 会话

```sh
eix ls [--archived]                # 列表
eix new [--name N] [--cwd P] [--model M] [--worktree|--shared] [--mode M]
eix chat [--session ID]            # 纯文本交互（无 TUI）
eix send [--session ID] "..."       # 一次性发送并流式打印
eix log [--session ID]             # 打印持久化转录
eix rename [--session ID] NAME
eix cancel [--session ID]
eix delete <session>
eix archive <session> [--undo]     # 隐藏但保留事件
```

### 历史与分支

```sh
eix fork   [--at SEQ] [--name N] [--shared]   # 复制历史到新会话
eix rewind [--at SEQ] [--name N]              # fork 并把工作区回滚到 SEQ 之前
eix undo / eix redo                            # 隐藏 / 恢复最近一轮
eix timeline [--limit N] [--json]              # 导航用的回合列表
eix export [--output FILE]                     # 导出 JSON
eix import <file> [--name N] [--cwd P]         # 导入为新会话
```

`fork` 默认共享文件，`rewind` 才会动工作区；两者都保留完整事件历史。

### 后台进程

```sh
eix ps [--session ID]      # 列出后台进程
eix tail PROCESS           # 看日志尾部
eix kill PROCESS           # 停止
```

### Worktree 与集成

```sh
eix worktree list
eix worktree rm <SESSION_ID> [--force]
eix ready [--session ID]
eix integrate [status | continue | retry]
```

机制（目录约定、rebase 冲突备份、基线分支解析）见 [worktree.md](./worktree.md)。

### 状态与外部工具集成

```sh
eix status [--json]                # daemon + 会话摘要，--json 可直接喂状态栏
eix pick [--query Q] [--limit N]   # id<TAB>name<TAB>status<TAB>cwd，给 rofi/fzf
eix doctor                         # 配置 / 路径 / daemon 诊断
eix daemon                         # 启动或附着并打印状态
```

## 6. 远程与多客户端

daemon 同时开 HTTP 和 TCP 入口，鉴权靠配对令牌。

```sh
eix pair --name laptop             # 生成一次性配对码
eix --tcp 127.0.0.1:7777 --token <TOKEN> tui
```

`--tcp` / `--token` 也可以走环境变量 `EIX_TCP` / `EIX_TCP_TOKEN`。

HTTP 端点是 `/health`、`/openapi.json`、`/stats`、`POST /rpc` 和 SSE `/events`；新客户端先
`POST /pair` 用配对码换 token。Rust 用 [`eix-sdk`](../crates/eix-sdk)，TypeScript 用
[`clients/ts`](../clients/ts)。

编辑器侧：

- **Neovim**：插件是独立的 daemon 客户端，自己不接管 buffer 和键位，见 [nvim.md](./nvim.md)。
- **ACP**：`eix acp` 在 stdio 上跑 Agent Client Protocol 适配器。

```json
{ "agent_servers": { "eix": { "command": "eix", "args": ["acp"], "env": {} } } }
```

## 7. 语音输入

`[voice]` 默认打开，但**只有主动触发才录音**：

- `F2` 按住说话，松开停止并转录（需要终端上报按键释放，即 kitty 键盘协议；不支持的终端
  退化成按两次切换，60 秒上限兜底）。
- `/voice` 手动开始 / 停止，适合长段落。

引擎默认自动选择：装了 SenseVoice 走 `sherpa`，否则 `whisper`。模型放置、下载校验、
`args` 语义、故障排查见 [voice-setup.md](./voice-setup.md) 和 [voice.md](./voice.md)。

## 8. 配置

### 文件层级

1. `--config PATH` 或 `EIX_CONFIG`：**最高优先，且不再合并项目层**（可复现配置）。
2. `~/.config/eix/config.toml`：全局。
3. 各级祖先目录下的 `.eix/config.toml`：离当前目录越近越优先。

表合并、数组替换。项目层被视为不可信：不做 `{env:}` / `{file:}` 展开，并会剥掉可执行策略
类表（`plugins`、`integration`、`worktree`）并给出警告——这些只能写在全局配置里。

### 主要分节

| 分节 | 内容 |
|---|---|
| `[providers.*]` / `[provider_runtime]` | 模型端点、凭据、超时与重试 |
| `[models]` / `[model_roles]` | 模型清单；`plan` / `execute` 分别用哪个模型 |
| `[tools]` | 沙箱、网络白名单、快照、并行 sub-agent 数，以及 allow / ask / deny 规则 |
| `[context]` | token 预算、自动压缩阈值、`AGENTS.md` 等指令文件 |
| `[integration]` / `[worktree]` | 集成队列与 worktree 策略 |
| `[keys]` / `[theme]` | 键位与配色 |
| `[notifications]` | 需要关注时的铃铛 / 外部命令 |
| `[voice]` / `[voice.*]` | 语音开关、采集阈值、引擎、模型 |
| `[mcp.servers.*]` / `[plugins.*]` | MCP 服务器；进程外插件 |
| `[images]` / `[math]` / `[formatter]` / `[lsp]` | 图片、公式渲染、格式化、语言服务器 |
| `[server]` | HTTP / TCP 监听与 token |
| `[compations]` | 兼容层（加载其它 harness 的全局/项目资源） |

`~` 在路径里**不会展开**，写绝对路径或相对路径。

权限规则写在 `[tools]` 下，语义是 **deny 优先，其次取最具体的 ask / allow / sandbox**；
项目层的规则只能收紧（宽松规则会被降级成 `ask`），被拒的条目会进 `security_warnings`
（`eix doctor` 可见）。

## 9. 扩展

- **Skills**：`SKILL.md` 目录，按名字加载，`tool_search` 可以按需发现。
- **Commands**：把常用 prompt 存成斜杠命令。
- **Agents**：自定义主 agent 与 sub-agent 的提示词与工具集。
- **MCP**：`[mcp.servers.*]` 配置，远端服务器支持 OAuth（`eix mcp auth` / `eix mcp logout`）。
- **Plugins**：进程外 JSON-RPC 或进程内 dylib。脚手架到上线一条链：

```sh
eix plugin new my-plugin --python   # 或 --dylib
eix plugin run my-plugin            # 前台跑，看 handshake
eix plugin test my-plugin
eix plugin pack my-plugin           # 生成 eix-plugin.sums
eix plugin verify my-plugin
eix plugin install my-plugin
eix plugin verify-lock              # 对比已装文件与 plugins.lock
```

发布者签名用 `eix plugin trust <publisher> <base64-ed25519-key>`。协议细节见
[plugins.md](./plugins.md) 与 [plugin-dylib.md](./plugin-dylib.md)。

## 10. 安全模型

| 层 | 机制 |
|---|---|
| 模式 | `plan` 只读；`build` 默认；`auto`；`full` 最宽松 |
| 沙箱 | `bwrap` 限制文件系统与网络，`[tools] sandbox = "required"` 强制 |
| 风险评估 | 每次调用单独判定，低/中风险工作区补丁直接应用 |
| 审批 | 高风险补丁逐 hunk；`exec_command` 执行前快照 |
| 审计 | 每个工具决策都留痕 |
| 撤销 | `rewind` 同时回滚对话与文件 |

## 11. 开发与排错

```sh
cargo build --release
cargo test --offline -p eix-core --lib
cargo test --offline -p eix-tui --bin eix     # 注意 --bin eix，否则用例不编译
cargo test --offline -p eixd --lib
```

会话、工具、worktree、MCP、审批都在 daemon 里；TUI 崩溃不会丢会话，重连即可。升级后正在
跑的 daemon 仍持旧版本二进制，需要退出 `eix` 再重开（或 `eix daemon` 重启）才生效。

`eix doctor` 是第一个应该跑的命令：它同时检查配置文件解析、路径可达性和 daemon 状态。
