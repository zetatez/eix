# eix

A coding agent designed for a vim / dwm / Arch workflow: native multi-session,
fuzzy switching, and a **real Neovim** as the input box.

> 名字取自欧拉公式 `e^{ix} = cos x + i·sin x`；加泰罗尼亚语中 `eix` 意为「轴」。
> 设计文档见 [DESIGN.md](./DESIGN.md)。

## Layout

```
crates/
├── eix-core   共享类型：session、事件、消息、协议、配置
├── eixd       daemon：session registry、SQLite 事件日志、agent runtime、OpenAI 兼容 provider
├── eix-sdk    HTTP JSON-RPC / SSE 类型化 Rust client
└── eix-tui    `eix` 前端：TUI + chat / send / log / picker CLI（内置 Vim 风格输入框）
```

## Phase 0：daemon + provider + CLI

已经能用的闭环：**auto-start daemon → 创建 session → 流式对话 → SQLite 持久化 → detach / 重新 attach**。

```sh
cargo build                          # 同时构建 eix 与 eixd
./target/debug/eix doctor

# 配置 provider（任选一种）
export EIX_BASE_URL=http://localhost:11434/v1   # Ollama / vLLM / LM Studio
export EIX_MODEL=qwen2.5-coder:7b
export OPENAI_API_KEY=...                       # 云端 OpenAI 兼容端点
# 或者写 ~/.config/eix/config.toml

./target/debug/eix                    # 默认打开全屏 TUI（无 session 时自动创建）
./target/debug/eix chat               # 纯命令行交互
./target/debug/eix send '解释 session 模型'
echo 'review the diff' | ./target/debug/eix send
./target/debug/eix send --image screenshot.png '这个报错怎么修'
./target/debug/eix ls
./target/debug/eix log                # 打印持久化转录
```

`eix` 第一次连接时会在后台拉起 `eixd`（新进程组，关终端不会杀掉 daemon）。
socket 在 Linux / Arch 默认 `$XDG_RUNTIME_DIR/eix.sock`；macOS 没有 XDG runtime dir，回退到
`~/Library/Caches/eix/eix.sock`（其他没有 runtime dir 的平台回退到 `~/.cache/eix/eix.sock`）。
数据默认 `~/.local/share/eix/`（SQLite + `eixd.log`）。可以用 `EIX_SOCKET` / `--socket` 覆盖。

配置：

```toml
# ~/.config/eix/config.toml
[provider]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
timeout_secs = 300
retry_attempts = 8            # 首次失败后的自动重试次数；0 关闭
retry_base_ms = 250           # 抖动指数退避起点
retry_max_secs = 60           # 单次等待上限（也限制 Retry-After）
# system_prompt = "..."

[notifications]
# command = "notify-send eix \"$EIX_SESSION_NAME $EIX_STATUS\""
bell = false
cooldown_secs = 10

[context]
budget_tokens = 32000          # soft ceiling for one request
instructions_files = ["AGENTS.md"]  # repo root → cwd；也读 <config>/AGENTS.md
workspace_summary = true       # top-level entries + git branch/status
auto_compact = true            # 超过阈值时先总结旧历史再继续本轮
auto_compact_threshold = 0.8   # 占 budget_tokens 的比例 (0, 1]
auto_compact_keep_turns = 2    # 最近 N 个 user turn 原样保留
tool_output_tokens = 4000      # 单条工具结果进入 provider 上下文的 token 上限

# 额外 provider（可选）。/model 会自动请求每个 provider 的 GET /models，
# 选择 id 为 "<provider>/<api-model-id>"；模型 id 里的 "/" 会保留。
# [provider] 的自动项使用 "default/<api-model-id>"，默认模型仍排在第一位。
[providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[providers.local]
base_url = "http://localhost:11434/v1"
# api_key 或 api_key_env 可选；本地端点通常不需要
# provider 名不能是 default，也不能包含 "/"、"#" 或空白。

# 手动固定的模型（可选）。适合给模型补充成本、上下文或 variants，
# 也可用 provider 复用 [providers.*] 的 endpoint/key。
[[models]]
id = "openrouter/gpt-5"        # /model 里的选择 id；API model 默认取 "/" 之后
name = "GPT-5 via OpenRouter"
provider = "openrouter"        # 可选；引用 [providers.openrouter]
model = "openai/gpt-5"         # 可选；显式指定 API model id
input_cost = 1.25              # USD / 1M tokens，用于成本统计
output_cost = 10.0
context_tokens = 400000
variants = ["high", "medium", "low"]  # 可在 /model 里选 id#variant；未指定时默认 medium

# MCP server（stdio，可选）
[mcp.servers.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
# env = { KEY = "value" }
# timeout_secs = 30
# 只暴露匹配的工具；deny 优先。支持 * 和 ?（* 也匹配 /）
# allowed_tools = ["read_*", "search"]
# denied_tools = ["read_secret"]

# 图片附件的存储与保留（可选）
[images]
# max_prompt_images = 4
# max_image_bytes = 8388608
# retention_days = 30        # 只清理无事件引用的 blob；0 关闭按时间清理
# max_store_bytes = 2147483648

# 写入后的格式化（可选；写入成功后执行，失败只附注不撤销文件）
[formatter]
command = "rustfmt --edition 2021"
extensions = ["rs"]
timeout_secs = 30

# TUI 调色板（可选；颜色名 / #rrggbb / 0-255 ANSI 索引）
# 不设置 preset 时使用终端背景；输入框不再额外填充背景，跟随主题 background/终端。
# 内置 preset：github-dark、github-light、gruvbox-dark、solarized-dark、
# solarized-light、nord、dracula、catppuccin-mocha。
[theme]
# normal = "white"
# comment = "darkgray"
# function = "cyan"
# diff_add = "#4ec9b0"
# diff_delete = "#f44747"
# panel = "black"

# 语言服务器（可选；stdio，写入后自动收集 diagnostics）
[lsp.servers.rust]
command = "rust-analyzer"
extensions = ["rs"]
timeout_secs = 10

[lsp.servers.python]
command = "pyright-langserver"
args = ["--stdio"]
extensions = ["py"]

# 转录区快捷键覆盖（动作 = 键；输入框保持 vim 行为，不受影响）
# 裸字母不会被接受：请用 C-/A-、F1-F12 等命名键或标点键。
[keys]
# sessions = "C-t"
# new_session = "C-n"
# help = "F2"
# palette = "C-p"
# quit = "C-q"
```

环境变量覆盖：`EIX_BASE_URL`、`EIX_MODEL`、`EIX_API_KEY`、`EIX_API_KEY_ENV`、
`EIX_SYSTEM_PROMPT`、`EIX_TIMEOUT_SECS`、`EIX_RETRY_ATTEMPTS`、`EIX_RETRY_BASE_MS`、
`EIX_RETRY_MAX_SECS`、`EIX_TOOL_POLICY`、`EIX_TOOL_MAX_OUTPUT`、
`EIX_TOOL_TIMEOUT_SECS`、`EIX_TOOL_SANDBOX`、`EIX_TOOL_SANDBOX_NETWORK`、
`EIX_TOOL_SANDBOX_READ_HOME`、`EIX_TOOL_SNAPSHOTS`、`EIX_CONTEXT_BUDGET`、
`EIX_CONTEXT_AUTO_COMPACT`、`EIX_CONTEXT_AUTO_COMPACT_THRESHOLD`、`EIX_CONTEXT_TOOL_OUTPUT_TOKENS`、
`EIX_CONTEXT_WORKSPACE`、`EIX_NOTIFY_COMMAND`、`EIX_NOTIFY_COOLDOWN_SECS`、
`EIX_NOTIFY_BELL`、`EIX_SOCKET`、`EIX_DATA_DIR`、`EIX_CONFIG`、`EIX_CONFIG_DIR`、`EIX_CLI_CONFIG`。

eix 自己的全局文件统一在 `~/.config/eix/`（macOS 也是，不跟随 `~/Library/Application Support`）：
`config.toml`、`AGENTS.md`、`skills/`、`cmds/`、`agents/`。可用 `EIX_CONFIG_DIR` 覆盖这个目录。

配置分层（daemon 从启动目录向上查找）：

1. `~/.config/eix/config.toml`（或 `EIX_CONFIG` / `--config`，显式路径不再合并项目层）
2. `<祖先目录>/.eix/config.toml`，从最外层到最近层依次覆盖

表递归合并，数组整体替换。字符串里支持 `{env:NAME}` 和 `{file:path}`（相对当前配置文件），
例如 `api_key = "{env:OPENAI_API_KEY}"`。daemon 每 2 秒检查这些文件的 mtime，变化后重新加载；
解析失败保留旧配置并通过 `config.error` 通知，TUI 里也可以 `/reload`。工具/context/MCP
设置在下一轮生效，已有 session 的 model 与 permission 不变。

项目里还可以放 `cli.json`（最近的 `.eix/cli.json` 或 `cli.json`，`EIX_CLI_CONFIG` 可指定），
作为新建 session 的默认值：

```json
{ "model": "openai/gpt-5", "agent": "plan", "permission": "read-only" }
```

命令行显式参数优先于 `cli.json`。


协议是 Unix socket 上的 newline-delimited JSON：请求带 `id`/`method`/`params`，
响应带 `id`，事件通知是 `session.updated`、`session.event`、`turn.started`、
`message.delta`、`reasoning.delta`、`tool.approval_required`、`input.required`、
`tool.started`、`tool.completed`、
`patch.proposed`、`patch.applied`、`patch.rejected`、`patch.conflict`、
`context.updated`、`context.compacted`、`session.attention`、`turn.completed` 等。
`eix-core/src/protocol.rs` 是唯一契约来源。

当前限制：低/中风险的 `write_file` / `edit` 在 `ask` 策略下先 stage 到 shadow workspace、
不阻塞 turn，读取（以及后续写入）看到的是“真实工作区 + staged”合并视图；高风险/forced
ask 写入、以及 `run_command` 的写操作仍在审批处阻塞（shell 无法 stage），风险分析判定为
只读的命令直接执行。bubblewrap 沙箱已接入；context ledger 与 process view 已接入
（见下）。TUI 已支持多 pane；后台 pane 的 attention 用边框/侧栏高亮、`a` 跳转提示，
并在底部状态栏中间显示提醒（不弹桌面通知）；外部通知命令（dwm/tmux）已接入
（见 `[notifications]`）；输入框使用内置的 Vim 风格编辑器（见下），外观是无独立填充的透明圆角面板：顶部左侧显示
`Build · ○ deepseek-v4-flash`（当前模式 · 状态图标 + 模型，Plan 为黄色），右侧对齐 cwd、git 分支和工作树；
正文左侧留 1 格内边距，底边保持普通边框；输入框下方是一行 statusline，左侧显示 `N` / `I` / `V` / `VL`
模式徽标、session 名和最近状态，中间显示后台 attention 提醒（约 8 秒），右侧显示模型、pane 位置、排队数和 attention。

## TUI（v1）

```sh
./target/debug/eix [--session ID]      # 无命令时默认打开 TUI
./target/debug/eix tui [--session ID]  # 显式打开
```

自动拉起 daemon；没有 session 时自动创建一个。（stdout 被管道时 `eix` 会退回 `eix chat`。）pane 自动平铺
（1 个全屏、2 个左右分屏、3 个左侧大 pane + 右侧两个、4+ 个近似网格），每个 pane
独立显示一个 session 的转录、滚动位置和流式输出；相邻 pane 共用一条边框线。
底部输入条只作用于当前聚焦 pane。

用 leader 快捷键 `<Space>v` / `<Space>t` / `<Space>r` 可切换 `horizontal`（纵向排列、队首更大）、
`tabby`（输入框 Build 行上方显示 tab，只显示当前 session）和 `fibonacci`（螺旋分屏，不显示 tab）；`<Space>l` 在四种布局
（含默认 grid）间循环。所有布局共享同一个**活跃队列**：pane 1 是当前工作槽位；turn 完成或
session 等待审批/输入时，它移到第二个位置（pane 2），不会替换 pane 1 正在编辑的内容；
给它发送任务后移到队尾，第二个位置的 session 会补到 pane 1。
`Ctrl-1` … `Ctrl-9` 直接聚焦第 N 个已打开 session；在 `tabby` 布局下就是切换到第 N 个 tab
（输入框内也可用）。终端需要支持 kitty 键盘协议才能完整区分这些组合键；不支持时可用
`Alt-1` … `Alt-9`。

历史 session 不再占用左侧栏：按 `<Space>s` 打开浮动 picker，输入即模糊搜索，`Enter` 选择、
`Esc` 关闭。输入框标题行直接显示 Build/Plan、session 状态和模型，cwd 右对齐并以 `~` 缩写，
模型省略 provider 前缀；输入框下方保留一行 statusline 展示模式、session、最近状态、居中提醒和队列信息。

转录区按对话渲染：用户消息和 agent 输出都使用普通文本（左侧 2 空格缩进，不加角色前缀），不再使用气泡；agent 的
思考过程收在 `Thinking` 模块里流式展开；每次工具调用（shell / read / write / agent …）
显示为一行状态徽标（状态点、命令/路径摘要、右侧 `ok` / `running` / `needs approval`），
展开后用四空格左缩进直接显示参数、side-by-side diff 和输出；diff 左侧是改动前、右侧是改动后，
代码按文件扩展名做语法高亮，窄 pane 自动回退到 unified 视图。命令/路径已在摘要里，不再重复
`input` / `output` 标签。两类模块都支持鼠标点击折叠；`Thinking` 默认
展开（包括已结束的 turn），工具块在流式中展开、结束后收起为一行摘要。等待发送的 prompt 会以
`⏳ queued` 行显示在转录末尾。

agent 输出和用户消息都按 Markdown 渲染：标题、粗体/斜体/删除线、行内代码与围栏代码块、
有序/无序/任务列表、引用、分隔线、表格、链接（在文本后显示目标地址）、图片（显示为
`🖼 alt (path)` 占位）以及 `$...$` / `$$...$$` 数学公式。行内公式转成紧凑 Unicode；
块级公式用 2D Unicode 网格排版（分数横线、根号、上下极限、矩阵、可伸缩括号），仍是终端近似，
不是 TeX 引擎。纯文本不受影响，CLI 的 `eix log` / `eix chat` 仍输出原始文本。

| 键 | 行为 |
| --- | --- |
| 鼠标左键 / 滚轮 | 聚焦 pane；点击 `Thinking` / 工具块折叠或展开；拖拽选择转录文本并在松开时自动复制；滚轮滚动转录 |
| `i` / `Enter` / `<Space>i` | 聚焦输入框（进入 insert 模式） |
| `Esc` / `Esc Esc` | 单次 `Esc` 回到 normal 模式并**保持输入框焦点**（不计数）；normal 模式下 500ms 内连按两次：有 turn 在跑时中断它，否则回到转录区 |
| `Enter` | 发送到聚焦 pane（仅 insert 模式）；turn 还在跑时自动排队，结束后按顺序发出 |
| `/exit` / `/quit` | 保存草稿并退出整个 eix 客户端（不关闭 session pane） |
| `/close` | 保存草稿并关闭当前 session；如果它是最后一个 pane，同时退出 eix |
| `Ctrl-J` | 换行（仅 insert 模式；`Ctrl-Enter` / `Shift-Enter` 同） |
| `Tab` | normal 模式与转录区：在 Build / Plan 模式间切换；Plan 为只读规划，Build 可执行工具 |
| `<Space>s` / `Ctrl-G` | session picker：切换聚焦 pane 的 session（`Ctrl-G` 输入框内外都可用） |
| `Ctrl-T` | 快速轮换推理强度：low → medium → high → 空 → low（默认 medium，输入框内外都可用） |
| `<Space>o` | session picker：在**新 pane** 中打开（已打开则聚焦该 pane） |
| `:` | 命令面板（转录区、以及输入框的 normal 模式都可用）：搜所有动作与 provider（session / files / worktrees / branches / ledger / search / processes / approvals / new / fork / cancel / help / quit） |
| `<Space>w` | worktree picker：选中后把路径插入聚焦 pane 的输入框 |
| `<Space>b` | branch picker：选中后把分支名插入输入框 |
| `@`（输入框内） | 文件 picker：选中后插入 `@相对路径`（行首或空白后触发） |
| `<Space>A` | 跳到下一个需要 attention 的 pane（审批/错误） |
| `Shift+,` / `Shift+.` | 线性切换到上一个 / 下一个 pane（`<` / `>`；转录区与输入框 normal 模式） |
| `<Space>v` / `<Space>t` / `<Space>r` | horizontal / tabby / fibonacci 布局；`<Space>l` 循环所有布局 |
| `Ctrl-1` … `Ctrl-9` | 聚焦第 N 个已打开 session；`tabby` 下即切换到第 N 个 tab（输入框内也可用） |
| `Alt-1` … `Alt-9` | 同上；用于终端无法区分 `Ctrl-1…9` 的情况 |
| `<Space>n` / `<Space>N` | 新建 session 并在新 pane 打开；有待审批时卡片上的 `n` 仍是 deny |
| `<Space>W` | 新建 session，同时在 `.worktrees/<name>` 建 git worktree |
| `y` / `n` | 批准 / 拒绝聚焦 pane 的待审批工具调用（卡片快捷方式） |
| `a` | `Always allow`：批准并为此 session 记住同类调用（卡片快捷方式） |
| `Ctrl-J` / `Ctrl-K` · `Enter` | 审批/提问卡片是 Markdown checkbox 列表（`- [ ]` / `- [x]`）：`Ctrl-J`/`Ctrl-K`（或 `j`/`k`、`C-n`/`C-p`）移动，`Enter` 执行高亮项，`1-9` 直接选；首个选项是推荐项 |
| 提问卡片 | 模型用 `ask_user` 提问时显示选项：`Ctrl-J`/`Ctrl-K` 移动、`Enter` 确认（`1-9` 直接选）；无头场景用 `eix answer N` |
| `e` / `o` | 编辑 staged 的新内容（改完重新 stage）/ 在 `$EDITOR` 打开真实文件 |
| `r` | 输入理由并拒绝（Enter 发送，Esc 取消） |
| `<Space>R` | 跨 session 审批 inbox（`j/k` 选择、`Enter` 跳转、`y/n` 处理、`Esc` 关闭） |
| `d` | 全屏审阅待审批写入的 side-by-side diff（左旧右新、语法高亮）；有 hunk 时 `[`/`]`（或 `J`/`K`）跳 hunk、`Space` 勾选、`a` 全选/全不选、`y` 应用选中、`n` 拒绝 |
| `<Space>C` | context ledger：`j/k` 移动、`o` 展开、`p` pin、`d` drop/恢复、`m` 标记、`s` 压缩（单条或 mark 到光标的范围）、`f` fork、`w` rewind 并回滚工作区、`Esc` 关闭 |
| `<Space>p` | 后台进程：`j/k` 选择、`x` kill、`r` 刷新、`PgUp`/`PgDn` 滚动输出、`Esc` 关闭 |
| `<Space>F` | fork 当前 session；在 git 仓库里自动建 worktree，仓库外共享工作区 |
| `#` | 全文搜索历史消息：输入查询回车执行，`↑/↓` 选择，再回车打开对应 session |
| `<Space>x` | 取消聚焦 pane 的 turn |
| `j` / `k` / `PgUp` / `PgDn` | 转录区聚焦时滚动；输入框 normal 模式下 `j`/`k` 是草稿内行移动，`Ctrl-J`/`Ctrl-K` 浏览已提交的 prompt 历史 |
| `g` / `G` | 顶部 / 跟随末尾（聚焦 pane） |
| `?` / `F1` | 帮助（F1 任意位置可用）；打开后直接输入即筛选 key/命令，`↑`/`↓` 选择匹配项，`Ctrl-U` 清空，`Esc` 关闭 |
| `Z` / `Ctrl-C` | 退出（normal 模式下的 `Shift+Z`；输入框内 `Ctrl-C` 是 vim 中断：回到 normal / 离开 insert，不会退出） |

### prompt 命令

在输入框里把 `/命令` 作为整条消息发送（不会传给模型）。输入 `/` 时会在输入框上方弹出
补全菜单：`↑`/`↓` 或 `Ctrl-N`/`Ctrl-P` 选择，`Tab` 补全，`Enter` 直接执行。

| 命令 | 行为 |
| --- | --- |
| `/help [query]` | 打开可搜索帮助（命令、快捷键、notes）；可带初始查询 |
| `/keys [query]` | 只打开快捷键章节，同样支持即时搜索和上下选择 |
| `/new [name]` | 在聚焦 session 的 cwd 里新建 session，并在新 pane 打开 |
| `/new-pane` | 打开 session picker，把已有 session 放进新 pane |
| `/new-worktree [name]` | 在独立 git worktree 里新建 session |
| `/clear` | 清空模型上下文（更早的 turn 仍留在转录和事件日志里，只是不再发给模型） |
| `/compact [notes]` | 让模型总结当前上下文，用摘要替换历史；`notes` 直接作为摘要文本 |
| `/btw <question>` | 用一次性上下文回答旁支问题；不写入事件日志，也不进入主对话；`Esc` 关闭 |
| `/cd [path]` | 切换当前 session 的 cwd（支持 `~` 和相对路径）；省略时显示当前路径 |
| `/init` | 生成一份 AGENTS.md 初始化 prompt，放进输入框供你审阅后再发送 |
| `/queue [clear]` | 查看队列；`clear` 清空，`rm N` 删除第 N 条，`move N M` 调整顺序 |
| `/sessions` | 打开 session picker |
| `/files` / `/worktrees` / `/branches` | 交互式 picker，选中后插入 `@path` / worktree 路径 / 分支名 |
| `/context` / `/ledger` | context 条目 picker（右侧预览）/ 打开完整 context ledger |
| `/history` | 打开全文历史搜索弹窗 |
| `/processes` / `/approvals` | 背景进程列表 / 审批收件箱 |
| `/attention` / `/cancel` / `/toggle-agent` | 跳到下一个需要 attention 的 pane / 取消运行 / 切换 Build·Plan |
| `/model [id]` | 打开可搜索的模型 picker：合并 `[provider]`、`[providers.*]` 自动拉取的 `/models` 和 `[[models]]`（含变体行）；直接 `/model provider/model#variant` 可固定到某个模型 |
| `/agents [name]` | 选择当前 session 的 primary agent；`build` / `plan` 是内置的 |
| `/mcp` | 打开 MCP server picker（搜索 server，右侧显示命令、工具数、错误） |
| `/commands` | 从磁盘重新加载自定义 Markdown 命令 |
| `/reload` | 重新读取 daemon 的分层配置（等价于 `config.reload` RPC） |
| `/theme` | 打开主题 picker：`↑`/`↓` 或 `Ctrl-J`/`Ctrl-K` 实时预览，`Enter` 应用，`Esc` 取消并恢复原主题 |
| `/skills [name] [args]` | 打开 skill picker（右侧预览正文）；带名字时把正文放进输入框并展开 `$ARGUMENTS` / `$1..$9` |
| `/todos` | 打开任务列表 picker（搜索任务，显示状态和详情；内存态，daemon 重启后清空） |
| `/image <path>` | 把图片挂到下一条 prompt（png / jpg / gif / webp，最多 4 张、每张 8 MB）；不带参数时显示已挂图片，`/image clear` 清空；也支持直接粘贴或拖入路径 / `file://` URI |

在 prompt 里以 `!` 开头（例如 `!cargo test`）会直接在当前 session 的 cwd 里执行 shell：走同样的沙箱；
写命令先抓 tree snapshot，只读 session 只允许风险分析判为只读的命令。命令和输出以 tool 卡片的形式留在转录里。
| `/timeline [n]` | 只读浮层：最近 n 条 user / assistant / tool 事件（`j/k` 滚动，`Esc` 关闭） |
| `/archive` | 归档当前 session（从 picker/列表隐藏，事件保留） |
| `/delete` | 删除当前 session；需要连续执行两次确认 |
| `/undo` / `/redo` | 隐藏/恢复最近一个 user turn（事件仍在日志里，可通过 redo 恢复） |
| `/fork [name]` | fork 当前 session（在 git 仓库里自动建 worktree） |
| `/permission <level>` | 设置工具权限：`read-only` / `workspace-write` / `full` |
| `/exit` / `/quit` | 保存草稿并退出整个 eix 客户端（不关闭 session pane） |
| `/close` | 保存草稿并关闭当前 session；如果它是最后一个 pane，同时退出 eix |

### Skills

支持以下位置（越靠近项目越优先，同目录下 `.eix` > `.opencode` > `.claude`）：

```text
~/.config/eix/skills/<name>/SKILL.md
~/.config/opencode/skills/<name>/SKILL.md
~/.claude/skills/<name>/SKILL.md
<project>/.eix/skills/<name>/SKILL.md
<project>/.opencode/skills/<name>.md
<project>/.claude/skills/<name>/SKILL.md
```

frontmatter 支持 `name`、`description`、`allowed-tools`；正文是完整指令，可使用
`$ARGUMENTS`（全部参数）和 `$1`..`$9`（最高位占位符吃掉剩余参数）。daemon 在 system prompt
里只列名字和描述，模型需要时调用 `use_skill {name, arguments}` 取正文，所以 skill 库很大
也不会占用上下文。

`allowed-tools` 用逗号或空格分隔，支持 `*` / `?` 通配；加载该 skill 后，本 turn 内只允许
这些工具（多个 skill 的允许列表取并集，`use_skill` 自身始终可用）。Claude 风格的工具名
（`Read`、`Write`、`Edit`、`Bash`、`Grep`、`Glob`、`Task` 等）会映射到 eix 的对应工具。
限制在下一轮 user prompt 时清空。TUI 里 `/skills` 打开带正文预览的 picker，
`/skills <name> [args]` 直接把展开后的正文放进输入框。

### 自定义命令与 agent

Markdown 文件放在以下目录（越靠近项目越优先）：

```text
~/.config/eix/cmds/*.md              # 全局命令
~/.config/eix/commands/*.md          # 旧路径，仍兼容
~/.config/opencode/commands/*.md     # OpenCode 兼容
<project>/.eix/cmds/*.md             # 项目命令（可嵌套，team/name.md → /team/name）
<project>/.eix/commands/*.md         # 旧路径，仍兼容
<project>/.opencode/commands/*.md

~/.config/eix/agents/*.md            # 全局 agent
<project>/.eix/agents/*.md           # 项目 agent
```

命令本质上就是提示词模板：`~/.config/eix/cmds/review.md` 会出现在 `/` 命令菜单里，输入
`/review <args>` 即展开模板并发送到当前 session。frontmatter 支持 `description`、`agent`、`model`；正文是模板，支持
`$ARGUMENTS`（全部参数）和 `$1`..`$9`（最高位占位符吃掉剩余参数）。没有占位符时参数
追加到末尾。例：

```markdown
---
description: Review a path
agent: reviewer
model: openai/gpt-5#high
---
Review $ARGUMENTS for bugs. Report file:line and a concrete fix.
```

agent 的 frontmatter 支持 `description`、`mode`（`primary` / `subagent` / `all`）、
`model`、`permission`（`read-only` / `workspace-write` / `full`）；正文追加到 system prompt。
`permission` 只会收紧 session 权限，不会放宽。`/agents` 只列 primary-capable agent；
`spawn_agent` 的 `agent` 参数可指定 subagent-capable agent（模型会在 system prompt 里看到列表）。

### 图片附件

`eix send --image shot.png "这个报错怎么修"`、TUI 里 `/image shot.png`，或直接把文件拖进
终端 / 粘贴路径后再发送。路径可以是相对/绝对路径、带引号或多个文件，也可以是
`file://` URI；TUI 只在所有 token 都是存在的图片时才接管粘贴，否则按普通文本处理。

图片按 OpenAI 多模态 `content` 数组（`text` + `image_url` data URL）发送。原始字节存在
`<data>/blobs/<hash 前两位>/<sha256>`（内容寻址、自动去重），user 事件里只保留 hash、
MIME 和大小，所以回放/重连仍能看到同一张图，即使原文件被移动。旧 session 的内联 base64
仍然可读；`session.export` 会把 blob 内联进 JSON 以便跨机器导入，导入时再重新写入本地 blob。

路径同样受 workspace / `external_directory` 限制，默认最多 4 张、每张 8 MB。daemon 启动、
删除 session 时以及后台每 6 小时会清理没有被任何事件引用的 blob；默认保留 30 天
（`images.retention_days`），目录超过 `images.max_store_bytes` 时优先淘汰最旧的无引用 blob。
turn 运行中建议先等它结束
再挂图片。

### 语言服务器（LSP）

`[lsp.servers.<name>]` 配置的 stdio server 会在第一次写入该扩展名的文件后懒启动。daemon
发送 `initialize` / `initialized` / `didOpen` / `didChange`，收集
`textDocument/publishDiagnostics`，并把 `error/warning/... file:line:col` 附在写入工具的
结果后面（最多 50 条）。没有匹配 server 的文件完全不受影响；server 启动失败只附一条
`[lsp: ...]`，不会回滚写入。模型也可以主动调用只读工具 `lsp_diagnostics {path}`。

daemon 仍提供 `lsp.workspace` RPC，可扫描整个 workspace 并聚合 diagnostics（默认最多 200 个
文件、上限 2000，跳过 `.git` / `target` / `node_modules` 等目录）。配置热重载会清掉缓存的
server 进程，下一条写入重新启动。

### MCP

`[mcp.servers.<name>]` 配置的 stdio server 会在首次 turn 时懒启动、握手，并读取
`tools/list`、`resources/list`、`prompts/list`（后两者由 server 能力决定）。工具以
`mcp__<server>__<tool>` 暴露。已注册的只读类工具（如 `get_*`、`list_*`、`read_*`、
`search_*`）默认放行；其余 MCP 工具仍需要审批，执行前会捕获一次工作区树快照。
`read-only` session 和 sub-agent 看不到 MCP 工具。

资源与 prompt 通过四个只读 meta-tool 暴露，不需要审批：

- `mcp_list_resources {server?}` / `mcp_read_resource {server, uri}`
- `mcp_list_prompts {server?}` / `mcp_get_prompt {server, name, arguments?}`

`resources/list`、`resources/read`、`prompts/list`、`prompts/get` 都支持分页 cursor；二进制
resource 只报告 MIME 和大小，不会把 base64 塞进上下文。server 发来的
`notifications/{tools,resources,prompts}/list_changed` 会标记缓存失效，下一次 turn 或
`/mcp` 时重新读取。每个 server 可用 `allowed_tools` / `denied_tools` 过滤工具（`*` 和 `?`
通配，deny 优先；allowed 为空表示全部）；被过滤的工具既不出现在模型工具表里，也无法调用。
`/mcp` 显示 server 状态、工具/资源/prompt 数量和错误。

排队只保存在 TUI 内存里：退出应用会丢弃尚未发送的 prompt。`/clear`、`/compact`、`/cd`
在 turn 运行时会被拒绝，先 `Esc Esc` 中断（或按 `<Space>x`）再执行。

**转录区不响应裸字母快捷键**：动作一律走 `<Space>` leader（见下），但 `i` 仍用于聚焦
输入框并进入 insert 模式；`j`/`k` 等导航键和 `:`/`?` 等标点键也保留。输入框聚焦时（normal 模式）单字母是 vim 命令，TUI 动作同样用
leader；`?`、`#`、翻页/滚动和 `[keys]` 绑定也与转录区一致，只有输入框自己的
vim 编辑键（如 `j`/`k` 行移动、`Tab` 切换 Build/Plan）保留编辑器语义。
转录区动作可以通过 `[keys]` 覆盖，但裸字母会被忽略，请用 `C-`/`A-` 修饰键或
命名键，例如 `sessions = "C-t"`、`new_session = "C-n"`、`help = "F2"`。可用动作：`quit`、`help`、`new_session`、
`new_worktree`、`sessions`、`new_pane`、`palette`、`worktrees`、`branches`、`ledger`、
`processes`、`inbox`、`search`、`fork`、`attention`、`cancel`、`next_session`、
`prev_session`、`scroll_down`、`scroll_up`、`timeline`、`todos`、`undo`、`redo`。
键写法支持 `Space`、`Tab`、`BackTab`、`Enter`、`Esc`、方向键、`PageUp/PageDown`、
`F1`..`F12` 以及 `C-` / `S-` / `A-` 修饰符；裸字母会被忽略，标点键（如 `:`、`?`）可用。

同一个 session 只会出现在一个 pane；从 picker 选中已打开的 session 会直接聚焦它。
所有 picker 共用同一个 overlay：上方输入框做模糊过滤，左侧是候选列表，右侧是预览
（session 显示 id/status/cwd/worktree/permission/model；文件、worktree、branch、context
显示 daemon 给出的详情；skills 显示来源、allowed-tools 和正文）。files / worktrees /
branches / context 由 daemon 的 `picker.query` 提供，其余（sessions、命令面板、skills）在
客户端生成。历史 session 不在左侧栏
常驻，只通过这个浮动 picker 打开（`<Space>s`）。

输入框是自研的 Vim 风格编辑器（不再内嵌 Neovim 进程，输入零往返延迟）。外框使用透明背景的
圆角边框，跟随主题的 `background`（未设置时用终端背景），顶部显示 `Build · ○ deepseek-v4-flash`（idle 为静态圆点，running 为动画 spinner），右侧对齐 cwd、git 分支/工作树和图片数；底边保持普通边框，输入框下方一行 statusline 显示彩色单字母模式徽标、session 名和最近状态，中间显示后台 attention 提醒（约 8 秒），右侧显示模型、pane 位置、排队数和 attention；
insert / normal / visual 分别用绿 / 蓝 / 琥珀色标识，失焦时自动转为低对比度。单次 `Esc`
只退出 insert、焦点仍留在输入框，normal 模式支持常用按键：

- 移动：`h j k l w W b B e E 0 ^ $ gg G f F t T ; ,`，可带计数（`3w`）
- 编辑：`i a I A o O x X D C s S r ~ J p P u Ctrl-r`，操作符 `d c y` 可配合上面的
  motion、双写整行（`dd cc yy`）或文本对象（`iw aw`）
- normal 模式 `p` / `P`：内部寄存器有内容时按 Vim 语义粘贴；寄存器为空时读取系统
  剪贴板，可直接粘贴鼠标选中的转录文本
- visual：`v` / `V` 进入，`d x y c p ~ o` 操作；选中区域反色显示
- 浏览历史：normal 模式下 `Ctrl-K` / `Ctrl-J` 在已提交的 prompt 之间上下浏览（更早 / 更新）；
  越过最新一条会恢复当前草稿。`j`、`k`、`3j`、`dj`、方向键是草稿内的行移动
- 模式切换：输入框 normal 模式与转录区都可用 `Tab` 在 Build / Plan 之间切换；Plan 只读，Build 可执行工具
- `Enter` 只在 insert 模式提交；turn 运行中提交的内容自动排队（`/queue` 查看，`Esc Esc` 中断当前 turn）

TUI 动作在**转录区**和**输入框 normal 模式**下共用同一套 **leader（空格）+ 键**：`<Space>s` session
picker、`<Space>n` 新建 session 并在新 pane 打开、`<Space>o` 把已有 session 放进新 pane
（`<Space>c` 关闭当前 pane）、`<Space>f` 文件、
`<Space>w`/`b` worktree/分支、`<Space>C` ledger、`<Space>h` 历史、
`<Space>p` 进程、`<Space>R` 审批、`<Space>N` 新建、`<Space>W` 新建 worktree、
`<Space>F` fork、`<Space>x` 取消、`<Space>A` attention、`<Space>i` 聚焦输入框、
`<Space>v`/`t`/`r` 布局、`<Space>l` 循环布局、`<Space>a` 或
`<Space>:` 命令面板。normal 模式下裸 `Z`（`Shift+Z`）退出 eix；
除此之外，裸字母 `o`/`p`/`n`/`q` 等不再触发任何 TUI 动作。
insert 模式下保留 `Ctrl-G`（session picker）、`Ctrl-Enter`/`Shift-Enter`/`Ctrl-J`（换行）
和 `@`（文件 picker）。所有 picker / 历史搜索都支持 `Ctrl-J`/`Ctrl-K` 或
`Ctrl-N`/`Ctrl-P`（以及 `↑`/`↓`）上下选择。normal 模式下 `Esc Esc` 先中断运行中的 turn；
空闲时回到转录区。
输入框最少 6 行内容（加边框；终端过矮时优先让给转录区），长草稿最多向上生长到 12 行，
再在框内滚动；normal 模式不会让输入框折叠。
低/中风险的 `write_file` / `edit` 在 `ask` 策略下先 stage（不阻塞 turn），pane 底部出现
选项卡片；高风险/forced ask 写入直接弹出审批卡片，不进入 stage。卡片选项为
（`Apply change` / `Review hunks` / `Open file` / `Edit staged content` / `Reject` /
`Reject with reason…`，第一个是推荐项）并显示 side-by-side diff（宽 pane 左旧右新、语法高亮，
窄 pane 回退 unified；最多 8 行，超出标 `… diff truncated`）。卡片本身是交互式 picker：
选项按 Markdown checklist 渲染（`- [ ]` / `- [x]`），
`Ctrl-J`/`Ctrl-K`（或 `j/k`、`C-n/C-p`）移动、`Enter` 执行，`1-9` 直接执行；`[x]` 表示当前
高亮项（单选），不是多选。卡片键在 prompt/transcript 聚焦以及 `tabby` 布局下都优先给卡片。
`y`/`n` 仍是批准/拒绝的快捷方式。按 `d` 可全屏审阅：
`j/k` 滚动，`[`/`]` 跳 hunk，`Space` 勾选，`a` 全选/全不选，`y` 应用选中，`n` 拒绝。
未勾选任何 hunk 时 `y` 会提示先选择。`e` 把 staged 内容写进临时文件交给
`$EDITOR`，保存后通过 `patch.revise` 更新 diff 再审批；`o` 打开工作区里的真实文件；
`r` 输入拒绝理由，随 `approval.respond` 一起发给模型；卡片标题会标出
`[LOW]` / `[MEDIUM]` / `[HIGH]` / `[CRITICAL]` 及命中的规则和沙箱状态。只读命令不会打断
turn；写入、未命中 allowlist 的网络请求和未知命令按风险弹审批，`HIGH`/`CRITICAL` 不提供 `Always`。session 栏显示 `⚠`；也可以从命令行用
`eix approve` / `eix deny` 处理（`eix approve --hunks 0,2` 只应用选中的 hunk，
`eix deny --reason "..."` 会记录理由）。模型调用 `ask_user` 时 pane 底部出现同样的选项
卡片：选项同样按 Markdown checkbox 渲染，`Ctrl-J`/`Ctrl-K`（或 `j/k`）移动、`Enter` 确认、
`1-9` 直接选，第一个选项标为推荐；无头场景用 `eix answer N`。`A` 打开跨 session 的审批 inbox，`Enter` 跳到
对应 pane，`y/n` 直接处理；`eix approvals --json` 供脚本使用。后台 pane 有
pending 审批/补丁或进入 `error` 时会标 `!`、边框和侧栏变黄/红，顶部显示 attention 计数，
按 `<Space>A` 跳到下一个；聚焦即清除标记。重新 attach 时会从事件日志恢复未完成的卡片，输入框
里的未发送文字也会通过 daemon 草稿恢复。

每个 session 有独立的权限上限：`workspace-write`（默认，由 `[tools] approval` 和规则决定
是否询问）、`read-only`（隐藏 `write_file` / `edit` / `patch` / `start_command`，运行时拒绝
所有写入和危险执行；保留 `run_command` 以便 `git status` 等只读检查）和 `full`（解除能力上限，
但仍然执行 `deny`、硬安全拦截和审批规则；只有 `approval = "auto"` 才会自动放行默认 ask）。
创建时用 `eix new --permission read-only`，运行时用
`eix permission LEVEL [--session ID]` 切换；加 `--temporary` 只改内存并在事件日志留下
`permission` 记录，daemon 重启后回到持久值。TUI pane 标题和 `eix ls` 会显示非默认级别，
nvim 里可用 `:Eix permission [LEVEL]`。

后台进程由 daemon 托管：`start_command`（与 `run_command` 共用沙箱与审批门禁）启动后
立即返回进程 id，`process_output`（只读）读取最新/指定进程的尾部输出。`P` 打开全屏列表，
下方实时跟随选中进程的输出（`j/k` 选择、`x` kill、`r` 刷新、`PgUp/PgDn` 回看）。
关闭 TUI 不会停止进程；daemon 退出时对整个进程组 SIGTERM，2 秒后 SIGKILL。输出同时
写入 `<data>/processes/<session>/<id>.log`；`eix ps`、`eix tail <id>`、`eix kill <id>`
可无头操作，`eix log` 用 `proc>` 行展示 start/exit 事件。

### fork / rewind / worktree

fork 和 rewind 都按“复制历史前缀”工作：新 session 名为 `name~2`、`name~3`…，记录
`parent_id`、切点和 `origin`（`fork` / `agent`），序号原样保留，所以新分支的上下文
与源前缀一致。CLI 为
`eix fork [--session ID] [--at SEQ] [--name NAME] [--shared]`，`--at` 是独占切点
（复制 `seq < at`）。源 session 不变，旧分支仍在列表里。

- **fork** 只复制对话；在 git 仓库里默认给新分支建 `.worktrees/<name>` 并切到独立
  branch（`eix-<slug>`），`--shared` 可显式共享源工作区。仓库外保持共享。
- **rewind** 在 fork 之后把工作区回滚到切点之前：daemon 会在每个会改文件的工具调用前
  抓快照，rewind 时按 `seq >= at` 倒序恢复这些快照，再返回新 session。CLI 为
  `eix rewind [--session ID] [--at SEQ] [--name NAME]`。
- 回滚是**尽力而为**的：工具改不到的文件（超过快照大小上限、被排除的路径）、eix 之外
  的改动、以及被保留策略 prune 掉的旧快照都无法恢复。结果里会列 `warnings`，
  `restored=false` 表示有快照恢复失败或有文件缺失，命令会明确打印，不会假装成功。
- 快照默认开启（`tools.snapshots = false` 可关）；`.worktrees`、`.git`、缓存目录等
  不参与快照。session 栏里 `⑂` 表示该 session 在独立 worktree，`↳` 表示子 agent。

worktree 的生命周期与 session 解耦：`eix worktree list` 列出所有带 worktree 的
session，`eix worktree remove SESSION` 删除空闲 session 的 worktree；有未提交改动时
git 会拒绝，需显式加 `--force`。删除后 session 的 `cwd` 回到主仓库根目录，worktree
字段清空。TUI 里 `N` 直接用新 worktree 开 session，`F` 在 repo 内 fork 时也会自动建。

### 输入框：自研 Vim 风格编辑器

输入框不再启动 Neovim 子进程，而是 `eix-tui` 内的一个模态编辑器
（`crates/eix-tui/src/tui/editor.rs`）：没有 RPC、没有用户配置加载，按键即时生效。
一个 client 只有一个 prompt，聚焦哪个 pane 就用它写哪个 session。

- 模式：insert / normal / visual / visual-line；标题栏显示当前模式
- normal：常用 motion（`h j k l w b e 0 $ gg G f t` 等）、操作符
  （`d c y` + motion / `dd` / `iw`）、`x D C s S r ~ J p P u Ctrl-r`
- visual：`v` / `V` 选中后 `d x y c p ~ o`；选中区域反色显示
- `Enter` 只在 insert 模式提交；`Ctrl-J` / `Ctrl-Enter` / `Shift-Enter` 换行
- `@`（行首或空白后）打开 workspace 文件 picker，选中后插入 `@相对路径`；其余位置插入字面 `@`
- 草稿镜像到 `draft.set`，切换 session 时回填；提交失败会把 draft 放回输入框
- 输入框最少 6 行内容高（加边框），长草稿最多增长到 12 行后在框内滚动，normal 模式也不会收缩
- 转录区使用固定配色（不再从编辑器 colorscheme 采样）

`eix doctor` 的 `input` 一行显示 `native (built-in Vim editor)`。

### 工具与审批

daemon 目前向模型暴露这些工具：

| 工具 | 行为 | 默认审批 |
| --- | --- | --- |
| `read_file` | 读取 session cwd 内的文本文件；输出带行号，支持 `offset` / `limit` 分页 | 自动放行 |
| `list_dir` | 列出 session cwd 内的目录 | 自动放行 |
| `grep` | 在工作区内按正则/字面量搜索，支持 `include`、忽略大小写、结果上限 | 自动放行 |
| `glob` | 按 glob 查找工作区路径（`**/*.rs`），可含隐藏文件 | 自动放行 |
| `use_skill` | 按名字加载 `.eix/skills` 里的 Markdown skill 正文 | 自动放行 |
| `todo` | 整表替换当前 session 的任务列表（pending / in_progress / completed / cancelled） | 自动放行 |
| `memory` | 读写当前 workspace 的跨 session 记忆（`add` / `replace` / `clear`） | 自动放行（只改 daemon 数据目录） |
| `lsp_diagnostics` | 查询某个文件的语言服务器 diagnostics（未配置对应扩展时返回 no-op） | 自动放行 |
| `webfetch` | 抓取 http(s) URL 并转成纯文本（HTML 去标签，约 100 KB 上限） | 默认放行（只读 GET）；配置 `network_allow` 后只放行命中域名 |
| `websearch` | DuckDuckGo HTML 搜索，返回标题和链接（无需 API key） | 默认放行；配置 `network_allow` 时需包含 `duckduckgo.com` |
| `write_file` | stage 到 shadow workspace，审批后写入 cwd 内文件（自动建父目录） | stage + 审批落盘 |
| `edit` | 精确字符串替换（默认要求唯一匹配，`replaceAll` 可全部替换），同样走 stage | stage + 审批落盘 |
| `patch` | 多文件 `*** Begin Patch` / Add / Update / Delete，先全量校验再写入 | 需要审批 |
| `run_command` | 在 cwd 内用 `sh -c` 执行命令，带超时；命令可分析时默认沙箱 | 只读命令放行；构建在沙箱内放行；未知/破坏性命令询问或拒绝 |
| `start_command` | 在沙箱里启动后台进程并立即返回 id，日志落盘 | 按风险分析；无法沙箱时询问 |
| `process_output` | 读取后台进程的尾部输出（只读） | 自动放行 |
| `spawn_agent` | 起一个只读子 agent 并发处理独立问题，可用 `agent` 指定自定义 agent | 自动放行（子 agent 只读） |
| `ask_user` | 让用户从 2-6 个选项中选择 | 交互，不修改文件 |
| `mcp__*` | MCP server 暴露的工具 | 默认询问；读取型 resource/prompt 放行 |

所有路径都会做 workspace 沙箱检查（拒绝 `..` 和 symlink 逃逸）；工具输出先按
`max_output_bytes` 截断，进入 provider 上下文时再按 `context.tool_output_tokens` 做 head/tail
省略，完整输出仍保留在事件日志。同一批次全是只读工具（`read_file` / `list_dir` / `grep` /
`glob` / `webfetch` / `websearch`）时并发执行，结果按调用顺序回填。

`run_command` 使用统一风险分析：shell tokenizer 会拆出每个命令段、重定向、`sudo`/`env`
等包装器、嵌套 `sh -c` 和命令替换。`git status`、`cat`、`grep`、`ls` 等只读命令直接放行；
`cargo test` / `npm test` 等构建命令默认在沙箱内执行；`rm`、写入重定向、未知程序、脚本
解释器、`find -exec`、网络命令等按风险询问。`rm -rf /`、块设备 `dd`、`mkfs`、关机、
fork bomb、私钥读取以及把 secret 占位符送进网络/命令/MCP 调用会硬拒绝，`auto` / `full`
和任何 `allow` 规则都不能绕过。`git push --force` 视为高风险，默认询问。

隐私边界：daemon 在把消息发给 provider 之前，会把密码、`sk-…` / Stripe / GitHub / Slack
等 token、Bearer、URL 凭据、PEM 私钥和 `password=` / `api_key:` 等赋值替换成带随机 salt 的
占位符。占位符只在本地文件工具（`read_file` / `grep` / `write_file` / `edit` / `patch` 等）
的参数和最终文本里还原；`run_command`、`start_command`、`webfetch`、`websearch` 和 MCP
参数不会还原，一旦出现占位符会被判定为 `secret-exfil` 并硬拒绝。URL 在审批卡片和审计事件里
只保留 `?…` 之前的部分。占位符映射只在当前 provider 进程内保存；本地事件日志和 TUI 仍显示
原文，方便审阅。图片内容不做文本脱敏，敏感截图不要上传。

`write_file` 会先生成 unified diff 和逐 hunk 预览，TUI 审批卡片、
`eix chat` 和 `eix log` 都会显示；在 TUI 里用 `d` 全屏审阅可以按 hunk 选择应用，命令行用
`eix approve --hunks 0,2`（不带参数或空选择 = 整文件）。策略由 `[tools]` 配置或环境变量控制：

```toml
[tools]
approval = "ask"          # ask（默认）/ auto；只影响默认 ask，不绕过规则或硬拒绝
max_output_bytes = 20000
command_timeout_secs = 60
sandbox = "auto"          # auto（默认，可用则隔离）/ required（不可用就询问）/ off
sandbox_network = false   # false 时加 --unshare-net
sandbox_read_paths = ["~/.cargo", "~/.rustup", "~/.cache", "~/.npm"]  # 只读挂载，不再挂整个 $HOME
sandbox_writable_roots = []  # workspace 之外额外可写目录
snapshots = true          # false 时跳过执行前快照（大仓库可关）
external_directory = ["/tmp/eix-shared", "~/.cache/eix/**"]  # 允许文件工具访问 workspace 之外
network_allow = ["docs.rs", "*.github.com"]  # 可选：配置后 webfetch/websearch 只放行命中域名
```

显式规则按“具体度”选择，`deny` 全局优先，不会被 `auto` / `full` 绕过：

```toml
[[tools.rules]]
id = "block-rm"
tool = "run_command"
command_regex = "^rm "
action = "deny"

[[tools.rules]]
id = "cargo-sandbox"
tool = "run_command"
command = ["cargo", "test"]  # argv token 前缀，不是整条 shell 字符串
action = "sandbox"

[[tools.rules]]
id = "review-src"
tool = "write_file"
path = "src/**"
action = "ask"

[[tools.rules]]
id = "docs-allow"
tool = "webfetch"
path = "https://docs.rs/**"
action = "allow"
```

- 规则字段是 AND 关系；`tool = "*"` 可匹配任意工具。`path` 对文件工具是 workspace 相对
  glob，对 `webfetch` 是 URL；`command` 是 argv token 前缀。`deny` 只要**任一** shell
  段命中就拒绝（`echo hi && rm -rf build` 会命中 `command = ["rm"]`）；`allow` / `ask` /
  `sandbox` 要求**每个** shell 段都满足，所以 `cargo test && rm -rf /` 不会命中
  `command = ["cargo", "test"]`。带重定向的 shell 命令不会命中非 deny 的 `command` 前缀规则，
  避免 `echo hi` 的 allow 覆盖 `echo hi > /etc/passwd`。`signature` 匹配规范化后的完整参数。
  `command_regex` 用于复杂表达式，但更适合 `deny` 而不是 `allow`。
- `allow` 只放行风险分析本来会 ask 的调用；硬拒绝和命令中的危险段仍然优先。`sandbox` 表示
  必须在沙箱内执行，bwrap 不可用时转为 `ask` 且 `auto` 不会绕过。`action = "deny"` 最先
  扫描所有来源，哪怕更具体的规则是 `allow`。
- 规则来源优先级为 `session > user > project > builtin`；具体度相同时越靠近当前 session 的
  来源胜出。项目 `.eix/config.toml` 只能收紧：`approval = "auto"`、`sandbox = "off"`、
  `sandbox_network = true`、`external_directory`、`network_allow`、额外 sandbox 路径会被忽略，
  `allow` 规则会降级为 `ask`，并写入 `security_warnings`（`eix doctor` 可见）。
- 审批卡片上的 `a`（`Always allow`）只在中低风险出现：session scope 只存在内存，user scope
  追加到用户配置；文件工具按路径、shell 按单段 argv 前缀、其它工具按精确参数收窄。`HIGH` /
  `CRITICAL` 不提供 Always，服务端也会拒绝把它升级成持久规则。
- 每次工具决策都会写入 `permission_evaluated` 审计事件（action、风险等级、分类、命中规则
  及来源、沙箱状态）；自动批准的写入仍会快照，`patch_applied` message 会带命中的规则 id。
- `webfetch` / `websearch` 是只读 GET 类网络工具：默认放行；配置非空 `network_allow`
  后只放行命中域名。URL query 在展示和日志中脱敏，网络工具不会收到还原后的 secret。

`run_command` 在沙箱模式下用 bubblewrap 隔离：系统目录只读，`/tmp` 是私有 tmpfs，
session workspace 可写；`sandbox_read_paths` 里的工具链缓存逐个只读挂载，整个 `$HOME`
不再默认暴露。网络默认关闭；`sandbox_writable_roots` 可额外开放工作区外目录。
`bwrap` 未安装时 `sandbox = "auto"` 会把需要沙箱的命令降级为强制审批，`required` 直接
阻断，`off` 完全关闭沙箱。`eix doctor` 会显示 approval、规则数量、网络和 bwrap 状态。

`approval = "auto"` 只自动放行风险分析给出的默认 `ask`，不会绕过硬拒绝、`deny` 规则、
只读上限或“必须沙箱”的失败路径。一次 turn 最多 64 轮工具往返。

### Shadow workspace（staged 写入）

`ask` 策略下，低/中风险的 `write_file` / `edit` 不再阻塞 turn，而是先把提案 stage
起来；高风险、critical 或显式 `ask` 规则的写入仍会弹出带完整风险信息的审批卡片：

1. **propose**：`write_file` 读取基准内容（磁盘或更新的 staged 内容）、生成 diff/hunk，
   返回 `staged <path> as patch <id>; the file is not written yet`，并发出
   `patch_proposed` 事件和 `patch.proposed` 通知。
2. **stage**：补丁落在 `<data-dir>/staging/<session>/<patch_id>.json`（新旧内容、路径、
   call id）。同一文件的多次写入会以最新的 staged 内容为基准，所以可以叠加；
   读取工具看到的是“真实工作区 + staged”合并视图：`read_file` 返回 staged 内容，
   `list_dir` 会列出只存在于 shadow 里的文件/目录。daemon 重启后仍能恢复 pending 补丁。
3. **apply / reject**：TUI 里仍按 `y`/`n`（或 `eix approve` / `eix deny`），底层是
   `patch.apply` / `patch.reject`（也接受 `approval.respond` + patch id）。落盘前会重新
   校验基准文件：如果 stage 之后文件被外部修改，补丁标记为 conflict（`patch_conflict`
   事件），不会覆盖；可以重新让模型 stage。应用时按选择写 hunk，并在写入前做单文件快照。

`run_command` 无法 stage（shell 会读写真实工作区），仍然在审批处阻塞；所以同一个 turn
里先 staged 写入、再执行 shell 时，shell 看到的是旧文件——这是有意保留的边界。
模型不会自动收到 apply / reject 的结果，它下一轮通过 `read_file` 看到真实内容（应用后）
或旧内容（拒绝后）；前端通过 `patch.*` 事件和卡片看到结果。

### 执行前快照

`write_file`（应用 staged 补丁时）和 `run_command`（执行前）会先做一次快照，落在
`<data-dir>/snapshots/<session>/<snapshot>/`：

- `write_file`：只复制它要覆盖的那个文件；目标原本不存在也会记下来（回滚时删除）。
- `run_command`：复制整个 workspace（session 的 `cwd`），因为 shell 能改任何东西。
  `.git`、`target`、`node_modules`、`.venv`、`dist` 等派生目录不复制；单文件 > 8 MB、
  总量 > 200 MB、条目 > 5 万时跳过并计数；符号链接跳过。
- 每次快照写一条 `snapshot` 事件（`eix log` 显示为 `snap> file before call_1 — 1 file(s),
  0 skipped`），记录文件数、字节数和跳过数；抓取失败也会记 `ok = false`，并在工具输出里
  附一行 `[snapshot failed: ...]`。每个 session 保留最近 20 份，更早的自动清理。
- 快照字节只存在 `snapshots/` 目录，事件日志只做索引。`Snapshotter::restore` 已接进
  `session.rewind`：`eix rewind` / TUI ledger 的 `w` 会按 `seq >= at` 倒序恢复快照并返回
  新 session。回滚是尽力而为的，恢复失败、缺失文件、被 prune 的旧快照都会在结果的
  `warnings` 里列出，`restored = false` 表示工作区没有完全回到切点。
- 大仓库可以设 `[tools] snapshots = false`（或 `EIX_TOOL_SNAPSHOTS=0`）关掉；关闭后不再
  记录 `snapshot` 事件。`eix doctor` 会显示当前开关。

### Provider 自动重试

每轮 provider 调用失败后，daemon 会先判断错误类型再决定是否重试：

| 类型 | 例子 | 行为 |
| --- | --- | --- |
| 瞬时故障 | 连接重置/DNS/超时、HTTP 408/409/425、5xx、流被截断 | 抖动指数退避重试 |
| 限流 | HTTP 429、`overloaded` / `try again` | 有 `Retry-After` 时按它等待；否则至少 500ms 起步，再抖动指数退避 |
| 空响应 | 模型只返回空内容或空 reasoning | 视为瞬时故障重试，并在通知里标记 `empty_response` |
| 上下文超限 | HTTP 413、`context_length_exceeded`、`prompt is too long` | 先强制压缩上下文，再从压缩后的上下文重试 |
| 套餐/余额 | `insufficient_quota`、402、billing/credit 类错误 | 不重试，直接报错并保留草稿 |
| 鉴权/配置 | 400/401/403/404/422、无效 key、错误的 base URL | 不重试 |

- `retry_attempts` 是首次请求之外的额外次数，默认 8 次；瞬时故障从 `retry_base_ms`
  开始按 1x → 2x → 4x 增长（默认 250ms → 500ms → 1s → 2s …），加入最多 25% 抖动，
  单次不超过 `retry_max_secs`（默认 60s）。早期重试更密，后期等待更长，兼顾快速恢复和
  避免持续打满上游。
- `turn.retrying` 会带 `kind`（`transient` / `rate_limited` / `empty_response`），
  TUI 状态栏会显示“upstream error”“rate limited”或“empty response”。
- 重试发生在当前工具轮次内部，不会把整轮对话重新跑一遍；已经进入上下文的事件不变。
- 如果失败前已经向 TUI 流式输出了部分文本，daemon 会先发 `message.reset` 清掉这部分，
  再发 `turn.retrying` 显示“第几次重试/等待多久”；重试成功后不会重复显示旧内容。
- 取消（`Esc` / `eix cancel`）会立即打断退避等待；重试耗尽后才把 session 标为 error。
- `/btw` 和自动摘要目前不参与这套 turn 级重试。

### Context ledger

每次请求的上下文由 daemon 从事件日志重建，并按 sector 记账（`eix context` 或 TUI 里按 `<Space>C`）：

| sector | 内容 | 预算 |
| --- | --- | --- |
| system | 内置系统提示 | 5% |
| instructions | `AGENTS.md`（仓库根 → session cwd，以及 `<config>/AGENTS.md`） | 5% |
| workspace | 顶层文件树 + git branch/status | 15% |
| history | 对话（user / assistant） | 60% |
| tools | 工具调用与输出（同一 assistant 的 tool_calls 与其结果算一步） | 10% |
| memory | 跨 session 记忆 + 当前任务列表（`todo`） | 5% |

- token 是启发式估算：ASCII 约 4 字符/token，CJK 约 1 字符/token，每条消息另加 4 token
  结构开销；底部显示总量、预算和剩余。
- `tools` / `history` 超出 sector 预算时，最旧的未 pin 条目会自动标 `cut`；通常保留最新 4 条
  （模型刚拿到的工具输出不会立刻消失）。若 4 条大输出仍超过总预算，会进入 emergency trim：
  保留当前 user 消息和最新工具结果，继续丢弃更旧的历史/工具步骤。`system` / `instructions` /
  `workspace` / `memory` 不会被自动裁剪，只显示用量——静默丢掉 `AGENTS.md` 或记忆会改变
  agent 行为。
- 单条工具结果超过 `context.tool_output_tokens`（默认 4000）时只对 provider 视图做
  head/tail 省略，事件日志和 TUI 中的完整输出不受影响。
- TUI：`j/k` 移动、`o` 展开、`p` pin/unpin、`d` drop/恢复、`m` 标记、`s` 压缩（单条，或从
  mark 到光标的范围）、`Esc` 关闭。`eix context --drop/--undrop/--pin/--unpin/--compact` 等价。
- drop / pin / compact 都是事件（`context_dropped` / `context_pinned` /
  `context_compacted`），重启后重放一致。compact 把一段历史换成一条 summary；原始事件仍在
  日志里并显示为 `sum`，但不再发给 provider；范围不能与已有 summary 重叠。
- workspace summary、instructions 和 memory 在每次重建上下文时重新读取，所以改 `AGENTS.md`
  或切 branch 后下一次请求就会生效。`[context]` 可改预算、指令文件名或关掉 workspace summary。
- 自动压缩：每轮开始前**以及每个工具批次之后**都会重建 ledger；如果 token 超过
  `auto_compact_threshold`（默认 0.8），daemon 先把最旧的 turn 总结掉，保留最近
  `auto_compact_keep_turns` 个 user turn，再发下一次请求。摘要使用固定结构（目标/约束、
  决策、文件、命令与错误、任务状态、未决问题），并带上当前 todo。摘要失败只记录日志，不会
  阻塞本轮；事件同样是 `context_compacted`。
- provider 返回 context-length / prompt-too-long 类错误时，daemon 会强制压缩并重试当前请求，
  最多两次；第一次优先保留配置的 turn 数（没有可压缩范围时退到 1），第二次只保留 1 个 user
  turn。这比盲目退避更有用。
- 记忆：`memory` 工具把 durable workspace facts 写到 `<data>/memory/<hash>.md`，下一次
  turn（包括新 session）会以固定 `memory` 条目注入；`todo` 的当前任务列表也放在同一个
  sector，因此压缩后计划不会丢。

### 全文搜索

用户和 assistant 的消息在写入事件日志时同步进入 SQLite FTS5 索引（`search_docs` 内容表
+ `search_fts` 虚表 + 触发器，trigram tokenizer）；启动时会把旧库中尚未索引的消息补上。
3 个字符以上的查询走 FTS5；更短的查询（中文常用的一两个字）退化为大小写不敏感的子串
扫描，否则 trigram 根本匹配不到。结果默认 20 条（最多 100），带 session、事件序号、角色
和匹配附近的 snippet。

- TUI：转录区或 session picker 里按 `#` 打开全屏搜索。输入查询回车执行，`↑/↓` 选择，
  再回车在 pane 中打开对应 session（已打开则聚焦）。
- CLI：`eix search 'sqlite event log' [--session ID] [--limit N]`。
- 索引只覆盖 `user_message` / `assistant_message` 的 `text`；工具输出不进入全文索引。
  查询里的标点按字面处理（每个词加引号后再交给 FTS5）。

### 外部通知

`[notifications]` 让后台 session 进入 attention（`awaiting_approval` / `error`）时提醒你。
TUI 内的提醒固定显示在底部状态栏中间，不会弹桌面通知：

| 字段 | 默认 | 行为 |
| --- | --- | --- |
| `command` | 空（关闭） | 用 `sh -c` 执行；通过 `EIX_SESSION_ID`、`EIX_SESSION_NAME`、`EIX_SESSION_CWD`、`EIX_STATUS` 传入上下文 |
| `bell` | `false` | `eix tui` 在**后台** pane 需要 attention 时响终端铃（聚焦 pane 不响） |
| `cooldown_secs` | `10` | 同一 session 的通知冷却 |

```toml
[notifications]
command = "notify-send eix \"$EIX_SESSION_NAME $EIX_STATUS\""
bell = true
cooldown_secs = 10
```

`command` 在独立线程里执行，卡住也不会阻塞 daemon；失败只记日志。tmux 用户可用
`command = "tmux display-message \"eix: $EIX_SESSION_NAME $EIX_STATUS\""`。

### 子 agent

`spawn_agent` 让模型把独立的探索拆给子 agent。daemon 为每个调用建一个 child session：
`origin = agent`、`parent_id` 指向父 session、名字是 `parent/<label>-N`、`cwd` 继承父的
工作区。子 session 只拿到 `read_file` / `list_dir` / `grep` / `glob` / `use_skill` / `process_output`
这些只读工具，模型即使硬调 `write_file` 也会被 daemon 直接拒绝，所以子 agent 不可能改
工作区或提权；它也不能再 `spawn_agent`（深度上限 1，避免递归失控）。`agent` 参数可选择
任意 subagent-capable 的 Markdown agent（内置 `explore`），其正文会作为子 session 的
额外 system prompt，`model` / `permission` 也随之生效（权限只会收紧）。

一个 assistant 批次里的多个 `spawn_agent` 会并发跑（`tokio::spawn`）；全是只读工具的批次也会
并发执行，其它工具仍按顺序执行；结果按调用顺序回填给父模型。子 agent 的 turn 完整写进它自己的事件日志，因此
`eix ls` 能看到、`eix log` 能回放、TUI 侧栏用 `↳` 标出。子 agent 失败时会作为 tool
结果返回给父模型（父可以重试或忽略），不会把父 turn 直接打成 error。

### HTTP API

除 Unix socket / TCP 外，还可以只开一个 HTTP 端口给脚本、SDK 或 Web 前端用：

```toml
[server]
http = "127.0.0.1:7778"  # 或用 EIX_HTTP
token = "..."            # 与 TCP 共用；开 http 时必填
```

- `GET /health`：无需认证，返回 `{"ok":true,"version":"..."}`。
- `GET /stats`：daemon 统计（version / uptime / sessions / events / db_bytes / blob 数量与
  大小 / skills / MCP server 数 / 默认模型），需要认证。
- `POST /rpc`：body 是一个 JSON-RPC 请求，返回一个 JSON 响应；认证头为
  `Authorization: Bearer <token>` 或 `X-Eix-Token: <token>`。
- `GET /events`：同样认证，Server-Sent Events 流；每条 `data: {notification}`。
  可加 `?session_id=<id>`（可重复或逗号分隔）只接收该 session 的 notification；没有
  `session_id` 的全局 notification 仍会送达。`?all=1` 或不带参数接收全部。

所有响应（包括错误和 SSE）都带 CORS 头，浏览器可直接调用；`OPTIONS` 预检无需 token，
返回 204。HTTP 是一次请求一条连接（SSE 除外）；body 上限 8 MB，`/health` 不需要 token。

```sh
curl -s localhost:7778/health
curl -s localhost:7778/stats -H "Authorization: Bearer $EIX_TOKEN"
curl -s localhost:7778/rpc -H "Authorization: Bearer $EIX_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"id":1,"method":"session.list","params":{}}'
curl -N "localhost:7778/events?session_id=$SESSION_ID" \
  -H "Authorization: Bearer $EIX_TOKEN"
```

Rust 项目可以直接依赖 `crates/eix-sdk`（`eix-sdk`），它封装了 health/stats/session.create/
prompt.submit/cancel/approval.respond，并把 `/events` 变成 `Stream<Item = Notification>`：

```rust
use futures_util::StreamExt;

let client = eix_sdk::Client::new("http://127.0.0.1:7778", std::env::var("EIX_TOKEN")?);
let stats = client.stats().await?;
let mut events = client.events(&[session_id.as_str()]).await?;
while let Some(notification) = events.next().await {
    let notification = notification?;
    // ...
}
```

### 远程 attach（TCP + token）

默认只监听 Unix socket（0600，靠文件系统权限保护）。要跨机器用，在配置里打开 TCP：

```toml
[server]
tcp = "127.0.0.1:7777"   # 或用 EIX_TCP
http = "127.0.0.1:7778"  # 可选；或用 EIX_HTTP
token = "..."            # 打开 tcp/http 时必填，非空
```

设置 `tcp` 后 token 是强制的（配置校验会拒绝空 token）。TCP 连接必须把 `auth {token}`
作为**第一个**请求：校验通过前不会返回任何数据或事件，失败则返回 `unauthorized` 并断开。
token 比较不会在第一个不同字节短路。Unix socket 仍然不需要 auth。

```sh
eix --tcp 127.0.0.1:7777 --token "$EIX_TOKEN" ls
eix --tcp 127.0.0.1:7777 --token "$EIX_TOKEN" tui
```

CLI/TUI 的 `--tcp`（或 `EIX_TCP`）会改用 TCP，`--token`（`EIX_TCP_TOKEN`）带上凭证；
TCP 端点是远端，`eix` 不会尝试在本地拉起 daemon。Unix 连接默认收到所有 session 的广播；
任何连接（含认证后的 TCP）都可以用 `session.subscribe {session_ids}` 把广播收窄到指定
session（`null` 恢复全部），`eix nvim` attach 后会自动只订阅当前 session。

### `eix nvim`：独立的 Neovim 前端

```sh
eix nvim [--session ID]      # 也可用 EIX_NVIM 覆盖 nvim 可执行文件
```

`eix nvim` 用你的 Neovim 打开一个内嵌 Lua 前端（脚本写在 `<data-dir>/eix-nvim.lua`），
它自己用 libuv 连 Unix socket（或 TCP + token），说同一套换行 JSON-RPC；转录渲染进一个
`eix://<id>` 的 scratch buffer。`:Eix` 子命令：

| 命令 | 作用 |
| --- | --- |
| `:Eix attach [ID\|name]` | attach（省略则第一个 session，没有就新建） |
| `:Eix list` / `:Eix pick [KIND]` | 列出 session / picker（`KIND` 省略为 session；可选 `files`/`worktrees`/`branches`/`context`，选中后把值插入光标处，`context` 在分屏显示条目详情） |
| `:Eix new [NAME] [worktree]` | 新建 session（可选独立 worktree） |
| `:Eix send TEXT` / `:Eix prompt` | 发送 prompt；`prompt` 开一个输入分屏，`<CR>` 发送、`@` 文件 picker |
| `:Eix fork [SEQ]` / `:Eix rewind [SEQ]` | 分叉 / 回滚并切到新 session |
| `:Eix stop` | 取消当前 turn |
| `:Eix ps` / `:Eix ledger` | 后台进程 / context ledger |
| `:Eix permission [LEVEL]` / `:Eix doctor` | 权限级别 / 连接与版本诊断 |
| `<C-g>` / `<C-g><C-g>` | session picker / 回到上一个 session |
| `<leader>a` | 动作 palette（picker / list / new / worktree / file / branch / context / prompt / stop / fork / rewind / ps / ledger / permission / doctor） |

每个 Neovim 进程是一个独立 client，共享同一个 daemon；daemon 会串行化同一 session 的
turn，第二个 prompt 在忙时会被拒绝。attach 后前端会发 `session.subscribe`，只接收当前
session 的事件。`eix tui` 使用内置的 Vim 风格输入框，与这条独立前端互不影响。

## 构建与打包

需要 Rust stable、`cargo`、`make`。在 Arch 上还需要 `base-devel`；打 Debian/Ubuntu 包需要
`dpkg-deb`；macOS 包必须在 macOS 上构建。

```sh
make release                  # 构建当前 OS/arch 并生成原生安装包
make dist                     # 构建当前 OS 支持的所有格式
make package-arch             # Arch Linux: dist/eix-<version>-1-<arch>.pkg.tar.zst
make package-deb              # Debian/Ubuntu: dist/eix_<version>_<x86_64|arm64>.deb
make package-tar              # 通用包: dist/eix-<version>-<os>-<arch>.tar.gz
make package-darwin           # Darwin/macOS: tar.gz + Homebrew formula
```

`package-deb` 的文件名使用 `x86_64`/`arm64`，但 `DEBIAN/control` 仍写入 Debian 要求的
`amd64`/`arm64`/`armhf`，避免 `dpkg` 拒绝安装。

产物都在 `dist/`（Arch 的 `makepkg` 工作目录也在 `dist/makepkg/`，不会污染仓库根目录）。
Darwin/macOS 会生成二进制 tarball 和对应的 Homebrew formula（例如 `dist/eix-arm64.rb`），可以直接本地安装：

```sh
brew install --formula ./dist/eix-arm64.rb
```

`make release` / `make package` 只针对当前主机和架构；跨 OS/架构的 release 由
[`.github/workflows/release.yml`](./.github/workflows/release.yml) 的原生 runner matrix 生成：
Linux x86_64/aarch64、macOS Intel/Apple Silicon，以及 Arch x86_64 容器。Windows 不在支持范围内。

> Debian 包最好在目标发行版或 CI 的 Ubuntu runner 上构建。在 Arch 上构建的 `.deb` 会链接
> 当前 Arch 的 glibc，较老的 Debian/Ubuntu 可能无法运行。CI 使用 Ubuntu 22.04 作为 Linux
> glibc baseline，macOS 使用 `MACOSX_DEPLOYMENT_TARGET=11.0`。

## Status

- [x] 设计文档 v0.1
- [x] workspace 骨架
- [x] 自研 Vim 风格输入框（normal / insert / visual、motion + operator + count、undo/redo、leader `<Space>xx`，无子进程）
- [x] daemon + OpenAI 兼容 provider（流式 SSE、取消、错误处理）
- [x] session 事件日志 + SQLite 持久化 + attach 协议（事件流广播）
- [x] `eix` CLI（chat / send / log / ls / new / rename / cancel / approve[--hunks] / deny / answer / doctor）
- [x] TUI v1（多 pane 自动平铺、焦点切换、转录、session 栏、fuzzy picker、多行输入、流式跟随）
- [x] 转录 Markdown 渲染（强调、代码、列表、引用、表格、链接、图片占位、Unicode 行内公式与 2D 块级公式）
- [x] 转录鼠标选择 + 松开自动复制（native clipboard helper，失败时 OSC 52）
- [x] 工具调用 + 审批门禁（read_file / list_dir / write_file / run_command、workspace 沙箱、审批卡片）
- [x] 写入前 Vim 风格 side-by-side diff 预览（左旧右新、语法高亮、窄宽回退 unified；TUI 卡片、`d` 全屏审阅、`eix chat` / `eix log`）
- [x] stale-write 冲突检测（stage 后文件被外部修改 → `patch_conflict`，不会覆盖）
- [x] hunk 级审批（TUI `d` 审阅里勾选 hunk，daemon 按选择应用部分改动）
- [x] attention 标记（后台 pane 高亮、`a` 跳转、审批卡片重放）
- [x] 外部 attention 通知（`[notifications]` command + TUI 终端铃、冷却、重放）
- [x] bubblewrap 沙箱（`run_command` 默认隔离，`[tools] sandbox` 可配，写入只限 workspace）
- [x] staged shadow workspace（propose → stage → apply、合并读视图、`patch.apply` / `patch.reject`、跨重启恢复）
- [x] 执行前快照（write_file 单文件 / run_command 整个 workspace，`snapshot` 事件，保留 20 份）
- [x] context ledger（sector 预算 / token 记账、AGENTS.md + workspace 注入、drop / pin / compact、TUI `C` 与 `eix context`）
- [x] process view（daemon 托管后台进程、`start_command` / `process_output`、流式输出与日志、TUI `P`、`eix ps/tail/kill`）
- [x] fork / rewind（按消息裁剪历史复制到新 session；`eix rewind` / ledger `w` 还会按快照倒序回滚工作区，并报告无法恢复的改动）
- [x] git worktree（`--worktree` / TUI `N` / repo 内 fork 自动建 `.worktrees/<name>`、`eix worktree list|remove`）
- [x] 子 agent（`spawn_agent`、只读工具集、并发批次、深度上限 1、独立事件日志）
- [x] `ask_user`（2-6 个选项、首个为推荐项、阻塞式交互选择、`eix answer N` 无头回答）
- [x] daemon 化的 nvim 前端（内嵌 Lua 插件 + JSON-RPC、`:Eix` 命令、headless 测试）
- [x] 远程 attach（TCP + 强制 token、首请求 `auth`、CLI `--tcp/--token`）
- [x] 全文搜索（FTS5 trigram + 短查询子串回退、TUI `#`、`eix search`）
- [x] 按连接过滤广播（`session.subscribe`，`null` = 全部；`eix nvim` 只订阅当前 session）
- [x] 审批增强（`e` 编辑 staged 内容、`o` 打开真实文件、`r` 带理由拒绝、`<Space>R` 跨 session 审批 inbox、风险标签）
- [x] 审批/提问卡片改为 Markdown checkbox 列表（`Ctrl-J/K` / `j/k` 选择、`Enter` 执行、`1-9` 直选、首项推荐；prompt 聚焦和 `tabby` 布局下按键优先给卡片）
- [x] `eix pick`（fzf/rofi 友好的按行输出）、`eix status [--json]`、`eix approvals`
- [x] 输入草稿持久化（`draft.get` / `draft.set`，按 session，重启/重连可恢复）
- [x] 客户端通知（后台 attention 在底部状态栏居中提示，可选 `bell`，不弹桌面通知）
- [x] 日志脱敏（`redact`：Bearer / `sk-` / 敏感键赋值）与 provider 上下行可逆脱敏
- [x] GitHub Actions CI（fmt / clippy `-D warnings` / test；无 nvim、git 时自动 skip）
- [x] 按 session 权限级别（`read-only` / `workspace-write` / `full`；`session.permission` 支持临时升级并记录事件）
- [x] 显式权限规则（`[[tools.rules]]`：工具名 + 路径 glob + argv 前缀 / 正则 / signature；deny 全局优先、按具体度选择、项目配置只能收紧）
- [x] 统一风险分析与审计（只读命令分析、硬 deny、network/secret 保护、`permission_evaluated` 事件、审批 scope / risk UX、`external_directory`）
- [x] 自动压缩（超过 `auto_compact_threshold` 时总结旧 turn，保留最近 N 轮；`context_compacted` 事件）
- [x] 配置分层 + 占位符 + 热重载 + `cli.json`（全局 → 祖先 `.eix/config.toml`、`{env:}` / `{file:}`、mtime 轮询 + `/reload`、项目默认 model/agent/permission）
- [x] session 管理（undo/redo、export/import、delete、archive、timeline、队列 `rm`/`move`；CLI `eix undo/redo/export/import/delete/archive/timeline`）
- [x] Skills（`.eix` / `.opencode` / `.claude` 的全局与项目 skills、system prompt 摘要 + `use_skill {name, arguments}`、`$ARGUMENTS` / `$1..$9`、`allowed-tools` 单 turn 限制、带预览的 `/skills` picker）
- [x] 写入后 formatter（`[formatter]` command/extensions/timeout；直接写入与 staged apply 都生效）
- [x] `todo` 任务列表 + `webfetch` / `websearch` 网络工具
- [x] 可配置 TUI 调色板（`[theme]`，颜色名 / hex / ANSI 索引）
- [x] HTTP API（`[server] http`；`/health`、`/stats`、POST `/rpc`、SSE `/events` + `session_id` 过滤，Bearer token、CORS/OPTIONS；`eix-sdk` 类型化 Rust client）
- [x] 图片附件（`eix send --image` / TUI `/image` / 粘贴拖入路径或 `file://`，OpenAI 多模态 content，SHA-256 blob 存储、保留/GC、export/import 往返）
- [x] 可配置转录区快捷键（`[keys] action = "C-x"`，含命名键/F1-F12）
- [x] LSP diagnostics（`[lsp.servers.*]`、懒启动 stdio client、写入后附诊断、`lsp_diagnostics`、`lsp.workspace` 全工作区扫描 RPC）
- [x] prompt `!<cmd>` 直接执行 shell（sandbox + snapshot + 只读拒绝，保留 tool 事件）
- [x] 独立 Neovim 前端交互：`<C-g>` session picker、`<C-g><C-g>` 回上一个 session、`:Eix pick|doctor`
- [x] 输入框 `@` 打开文件 picker（`picker.query` 的 `files` provider）、两次 `Esc` 回转录区、长草稿自动增高（上限 12 行）
- [x] 草稿镜像（编辑器变更同步到 `draft.set`，切 session 时从 daemon 回填）
- [x] 统一 picker 体系（单一 overlay + 右侧预览；`picker.query` 提供 files / worktrees / branches / context，`p`/`o`/`:`/`w`/`b`/`@` 入口；独立前端 `:Eix pick [KIND]`）
- [x] 工具补齐：`grep` / `glob` / `edit` / `patch`，`read_file` 行号 + `offset`/`limit` 分页（只读工具自动放行）
- [x] 自定义 Markdown 命令（`~/.config/eix/cmds`、项目 `.eix/cmds`，兼容旧 `commands` 目录；`/name` 快速调用、`$ARGUMENTS` / `$1..$9`、frontmatter `description`/`agent`/`model`）
- [x] 自定义 Markdown agent（`~/.config/eix/agents`、项目 `.eix/agents`；`mode`/`model`/`permission`，`/agents` 选择 primary、`spawn_agent agent:` 选择 subagent）
- [x] 多 provider 模型目录（`[providers.*]` 命名 endpoint、自动拉取 `/models`、`[[models]]` 手动补充成本/变体、`/model` 搜索切换、内部统计 tokens / tok/s / 成本）
- [x] MCP stdio client（`[mcp.servers.*]`、懒启动、`tools/list` / `tools/call` / `resources/*` / `prompts/*`、分页、`list_changed` 刷新、`allowed_tools` / `denied_tools`、`mcp__server__tool` 审批 + 树快照、`/mcp` 状态）
