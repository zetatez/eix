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
├── eix-tui    `eix` 前端：TUI + chat / send / log / picker CLI
└── eix-nvim   嵌入 nvim + linegrid 渲染 spike
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

./target/debug/eix chat               # 无 session 时自动创建
./target/debug/eix send '解释 session 模型'
echo 'review the diff' | ./target/debug/eix send
./target/debug/eix ls
./target/debug/eix log                # 打印持久化转录
./target/debug/eix tui                # 全屏界面
```

`eix` 第一次连接时会在后台拉起 `eixd`（新进程组，关终端不会杀掉 daemon）。
socket 默认 `$XDG_RUNTIME_DIR/eix.sock`，数据默认 `~/.local/share/eix/`（SQLite + `eixd.log`）。

配置：

```toml
# ~/.config/eix/config.toml
[provider]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
timeout_secs = 300
# system_prompt = "..."
```

环境变量覆盖：`EIX_BASE_URL`、`EIX_MODEL`、`EIX_API_KEY`、`EIX_API_KEY_ENV`、
`EIX_SYSTEM_PROMPT`、`EIX_TIMEOUT_SECS`、`EIX_TOOL_POLICY`、`EIX_TOOL_MAX_OUTPUT`、
`EIX_TOOL_TIMEOUT_SECS`、`EIX_SOCKET`、`EIX_DATA_DIR`、`EIX_CONFIG`。

协议是 Unix socket 上的 newline-delimited JSON：请求带 `id`/`method`/`params`，
响应带 `id`，事件通知是 `session.updated`、`session.event`、`turn.started`、
`message.delta`、`tool.approval_required`、`tool.started`、`tool.completed`、
`session.attention`、`turn.completed` 等。`eix-core/src/protocol.rs` 是唯一契约来源。

当前限制：工具执行已接入，写入是审批后直接落盘；审批前会展示 unified diff，但按 hunk
选择、冲突检测和 staged shadow workspace 仍未做（bubblewrap 沙箱和 context ledger 也是
Phase 2 的后续项）。TUI 已支持多
pane；后台 pane 的 attention 用边框/侧栏高亮和 `a` 跳转提示，外部通知命令（dwm/tmux）
和嵌入 nvim 输入尚未接上。

## TUI（v1）

```sh
./target/debug/eix tui [--session ID]
```

自动拉起 daemon；没有 session 时自动创建一个。布局是左侧 session 栏 + 右侧自动平铺的
pane（1 个全屏、2 个左右分屏、3 个左侧大 pane + 右侧两个、4+ 个近似网格），每个 pane
独立显示一个 session 的转录、滚动位置和流式输出。底部输入条只作用于当前聚焦 pane。

| 键 | 行为 |
| --- | --- |
| `i` / `Enter` | 聚焦输入框 |
| `Esc` | 从输入框回到转录区 |
| `Enter` | 发送到聚焦 pane（输入框内） |
| `Ctrl-J` | 换行（输入框内） |
| `p` / `Ctrl-G` | session picker：切换聚焦 pane 的 session（输入框内外都可用） |
| `o` | session picker：在**新 pane** 中打开（已打开则聚焦该 pane） |
| `a` | 跳到下一个需要 attention 的 pane（审批/错误） |
| `C-w h/j/k/l` | 按方向切换 pane 焦点（转录区） |
| `C-w v` / `C-w s` | 打开新 pane |
| `C-w c` | 关闭聚焦 pane（至少保留一个） |
| `C-w w` | 循环切换 pane |
| `Tab` / `S-Tab` | 聚焦 pane 切到下一个 / 上一个 session（转录区） |
| `n` | 新建 session 并在新 pane 打开（有待审批时先作为 deny） |
| `y` / `n` | 批准 / 拒绝聚焦 pane 的待审批工具调用 |
| `x` | 取消聚焦 pane 的 turn |
| `j` / `k` / `PgUp` / `PgDn` | 滚动聚焦 pane 的转录 |
| `g` / `G` | 顶部 / 跟随末尾（聚焦 pane） |
| `?` / `F1` | 帮助（F1 任意位置可用） |
| `q` / `Ctrl-C` | 退出 |

同一个 session 只会出现在一个 pane；从 picker 选中已打开的 session 会直接聚焦它。
有待审批的工具调用时，pane 底部会出现 `[y] approve / [n] deny` 卡片，`write_file`
还会显示 unified diff（最多 10 行，超出标 `… diff truncated`），session 栏显示
`⚠`；也可以从命令行用 `eix approve` / `eix deny` 处理。后台 pane 进入
`awaiting_approval` / `error` 时会标 `!`、边框和侧栏变黄/红，顶部显示 attention 计数，
按 `a` 跳到下一个；聚焦即清除标记。重新 attach 时会从事件日志恢复未完成的审批卡片。
输入框是 ratatui 自绘的临时方案；DESIGN 里的「一个 client 一个嵌入 nvim」会在 pane
交互稳定后接入。

### 工具与审批

daemon 目前向模型暴露四个工具：

| 工具 | 行为 | 默认审批 |
| --- | --- | --- |
| `read_file` | 读取 session cwd 内的文本文件 | 自动放行 |
| `list_dir` | 列出 session cwd 内的目录 | 自动放行 |
| `write_file` | 创建/覆盖 cwd 内文件（自动建父目录） | 需要审批 |
| `run_command` | 在 cwd 内用 `sh -c` 执行命令，带超时 | 需要审批 |

所有路径都会做 workspace 沙箱检查（拒绝 `..` 和 symlink 逃逸）；工具输出按
`max_output_bytes` 截断。`write_file` 在审批前会读取原文件并生成 unified diff，
TUI 审批卡片、`eix chat` 和 `eix log` 都会显示（整文件批准，暂不支持按 hunk 选择）。
策略由 `[tools]` 配置或环境变量控制：

```toml
[tools]
policy = "ask"            # ask（默认）/ auto / read_only
max_output_bytes = 20000
command_timeout_secs = 60
```

`auto` 会跳过审批直接执行写操作（危险，显式选择才生效）；`read_only` 直接拒绝写工具
并把原因回传给模型。一次 turn 最多 12 轮工具往返。

## Nvim spike

```sh
# 最小配置，先验证链路
cargo run -p eix-nvim -- --minimal

# 无 TTY 的自检：attach、收帧、注入样例文本、打印网格、跑一次提交往返
cargo run -p eix-nvim -- --probe --minimal

# 加载你自己的 ~/.config/nvim
cargo run -p eix-nvim
```

spike 内：

- `Enter` 提交（nvim buffer-local mapping，带 `pumvisible()` / `getcmdtype()` / cmp / blink 守卫）
- `Ctrl-J` 换行（nvim 原生行为；但如果自己的配置把 `<C-j>` 映射给了补全插件，例如
  `cmp.mapping.select_next_item()`，那就按配置走 —— 语义归 nvim，host 不抢）
- `Ctrl-C` 退出 spike
- 退出后把提交过的 draft 打印到 stdout，用来验证往返

其他参数：

- `--width` / `--height` 覆盖上报尺寸（probe 默认 80x24）
- `-- <args>` 后面的参数直接透传给 nvim，例如
  `-- -c "lua require('smear_cursor').enabled = false"`
- `EIX_DEBUG=1` 打印启动自检（colorscheme / config 路径 / 模式）与浮动窗口清单

## Spike 结论（已验证）

| 问题 | 结论 |
| --- | --- |
| `--embed` + msgpack RPC 链路 | 通。`nvim_get_api_info` → channel id → `nvim_set_var` |
| `ext_linegrid` 自绘 | 通。`ui_attach` 后立刻收到 `redraw`，`flush` 计数正常 |
| redraw 批次形状 | **不是**文档里的 `[[ev, ...]]`，而是 batch 直接展开成 params：`args[i]` 是 `[name, param_tuple, ...]`，同名连续事件会打包进同一个元素 —— 两种形状都要能解析 |
| `grid_*` 坐标 | 每个事件第一个参数是 grid handle（即使不开 `ext_multigrid`），常量 1 |
| 宽字符 | 通。`grid_line` 用「宽字符 + 空串」两个 cell 表示，空串格不能再写空格，否则会打掉半个字 |
| 中文 / emoji / 全角 | 通。`🌍` 占 2 格，`世界` 各占 2 格，对齐正确 |
| `<CR>` 守卫 | 通。走的是真 Neovim 的 mapping，`nvim_input` 打进去的 `<CR>` 会命中它并 `rpcnotify` 回 host |
| 加载完整 `~/.config/nvim` | 能跑，但会带出两个坑，见下 |

### 完整配置的两个坑

1. **插件自动命令会覆盖输入框外观**。本机配置里有 `InsertEnter` → `vim.wo.number = true`（"smart line number"），会把行号重新画进输入框。prompt profile 现在自己注册一个 buffer-local 的 `InsertEnter/InsertLeave/TextChanged` autocmd 重新消毒，因为后注册的先跑。
2. **动画类浮动窗口会在单 grid 上留残影**。`smear-cursor.nvim` 的 1x1 浮动窗口在光标被程序化移动后（probe / 切 pane）会残留几个块字符（`▗▖▝▘`）。这正是 DESIGN 里预判的「插件 UI 干扰」。
   缓解：启动时带 `g:eix_prompt = 1`，让配置自行关闭这类效果；用户在 init.lua 里写
   ```lua
   if vim.g.eix_prompt then require("smear_cursor").enabled = false end
   ```
   后续做成 `eix doctor` 检测 + 提示。`--minimal` 始终是干净回退。

## Status

- [x] 设计文档 v0.1
- [x] workspace 骨架
- [x] nvim linegrid spike（RPC + 自绘 + 宽字符 + 提交往返 + 完整配置）
- [x] daemon + OpenAI 兼容 provider（流式 SSE、取消、错误处理）
- [x] session 事件日志 + SQLite 持久化 + attach 协议（事件流广播）
- [x] `eix` CLI（chat / send / log / ls / new / rename / cancel / approve / deny / doctor）
- [x] TUI v1（多 pane 自动平铺、焦点切换、转录、session 栏、fuzzy picker、多行输入、流式跟随）
- [x] 工具调用 + 审批门禁（read_file / list_dir / write_file / run_command、workspace 沙箱、审批卡片）
- [x] 写入前 unified diff 预览（TUI 卡片 / `eix chat` / `eix log`）
- [x] attention 标记（后台 pane 高亮、`a` 跳转、审批卡片重放）
- [ ] 嵌入 nvim 输入 + 外部 attention 通知（dwm/tmux/terminal bell）
- [ ] staging / shadow workspace、hunk 级审批、冲突检测、沙箱、context ledger
- [ ] daemon 化的 nvim 前端（JSON-RPC over Unix socket）
