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

./target/debug/eix                    # 默认打开全屏 TUI（无 session 时自动创建）
./target/debug/eix chat               # 纯命令行交互
./target/debug/eix send '解释 session 模型'
echo 'review the diff' | ./target/debug/eix send
./target/debug/eix ls
./target/debug/eix log                # 打印持久化转录
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

[notifications]
# command = "notify-send eix \"$EIX_SESSION_NAME $EIX_STATUS\""
bell = false
cooldown_secs = 10
```

环境变量覆盖：`EIX_BASE_URL`、`EIX_MODEL`、`EIX_API_KEY`、`EIX_API_KEY_ENV`、
`EIX_SYSTEM_PROMPT`、`EIX_TIMEOUT_SECS`、`EIX_TOOL_POLICY`、`EIX_TOOL_MAX_OUTPUT`、
`EIX_TOOL_TIMEOUT_SECS`、`EIX_TOOL_SANDBOX`、`EIX_TOOL_SANDBOX_NETWORK`、
`EIX_TOOL_SANDBOX_READ_HOME`、`EIX_TOOL_SNAPSHOTS`、`EIX_NOTIFY_COMMAND`、
`EIX_NOTIFY_COOLDOWN_SECS`、`EIX_NOTIFY_BELL`、`EIX_SOCKET`、`EIX_DATA_DIR`、`EIX_CONFIG`。

协议是 Unix socket 上的 newline-delimited JSON：请求带 `id`/`method`/`params`，
响应带 `id`，事件通知是 `session.updated`、`session.event`、`turn.started`、
`message.delta`、`tool.approval_required`、`tool.started`、`tool.completed`、
`patch.proposed`、`patch.applied`、`patch.rejected`、`patch.conflict`、
`session.attention`、`turn.completed` 等。`eix-core/src/protocol.rs` 是唯一契约来源。

当前限制：`write_file` 在 `ask` 策略下先 stage 到 shadow workspace、不阻塞 turn，读取
（以及后续写入）看到的是“真实工作区 + staged”合并视图；`run_command` 仍在审批处阻塞
（shell 无法 stage）。bubblewrap 沙箱已接入；context ledger 仍是 Phase 2 的后续项。TUI 已支持多
pane；后台 pane 的 attention 用边框/侧栏高亮和 `a` 跳转提示，外部通知命令（dwm/tmux）
已接入（见 `[notifications]`）；输入框已默认使用嵌入的 Neovim（见下）。

## TUI（v1）

```sh
./target/debug/eix [--session ID]      # 无命令时默认打开 TUI
./target/debug/eix tui [--session ID]  # 显式打开
```

自动拉起 daemon；没有 session 时自动创建一个。（stdout 被管道时 `eix` 会退回 `eix chat`。）布局是左侧 session 栏 + 右侧自动平铺的
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
| `d` | 全屏审阅待审批写入的 diff；有 hunk 时 `[`/`]`（或 `J`/`K`）跳 hunk、`Space` 勾选、`a` 全选/全不选、`y` 应用选中、`n` 拒绝 |
| `x` | 取消聚焦 pane 的 turn |
| `j` / `k` / `PgUp` / `PgDn` | 滚动聚焦 pane 的转录 |
| `g` / `G` | 顶部 / 跟随末尾（聚焦 pane） |
| `?` / `F1` | 帮助（F1 任意位置可用） |
| `q` / `Ctrl-C` | 退出 |

同一个 session 只会出现在一个 pane；从 picker 选中已打开的 session 会直接聚焦它。
`write_file` 在 `ask` 策略下先 stage（不阻塞 turn），pane 底部出现 `[y] apply / [n] reject`
卡片并显示 unified diff（最多 10 行，超出标 `… diff truncated`）；按 `d` 可全屏审阅：
`j/k` 滚动，`[`/`]` 跳 hunk，`Space` 勾选，`a` 全选/全不选，`y` 应用选中，`n` 拒绝。
未勾选任何 hunk 时 `y` 会提示先选择。`run_command` 仍走阻塞式审批卡片。session 栏
显示 `⚠`；也可以从命令行用
`eix approve` / `eix deny` 处理（`eix approve --hunks 0,2` 只应用选中的 hunk）。后台 pane 有
pending 审批/补丁或进入 `error` 时会标 `!`、边框和侧栏变黄/红，顶部显示 attention 计数，
按 `a` 跳到下一个；聚焦即清除标记。重新 attach 时会从事件日志恢复未完成的卡片。
### 输入框：嵌入 nvim

TUI 默认拉起一个 `nvim --embed`（加载你的完整 `~/.config/nvim`），用 `ext_linegrid`
把 buffer 画在输入条里；按键原样转发给 Neovim，模式、补全、撤销都由 Neovim 自己决定。
一个 client 只跑一个 nvim，聚焦哪个 pane 就用它写哪个 session 的 prompt。

- `Enter` 提交（nvim buffer-local mapping，带 `pumvisible()` / cmp / blink 守卫）
- `Ctrl-J`（或 `Shift-Enter`）换行：host 直接让 nvim 插入字面换行，绕过补全插件对
  `<C-j>` 的映射（如 `cmp.select_next_item()`）
- `Esc` 回到转录区；再按 `i` / `Enter` 重新聚焦并进入 insert
- 聚焦时高度为 `clamp(行数, 3, 10)`，失焦时保留一行预览
- 提交失败（请求错误或 turn 出错）会把 draft 放回输入条，不会丢字

```toml
[input]
backend = "auto"     # auto（默认，有 nvim 就用）/ nvim（强制，失败则退出）/ native
minimal = false      # true 时用 `nvim -u NONE`，跳过用户配置
```

也支持 `EIX_INPUT_BACKEND=auto|nvim|native`、`EIX_INPUT_MINIMAL=1`。
`eix doctor` 会显示当前 backend 和是否找到 `nvim`；找不到时 `auto` 自动退回
ratatui 原生输入框并在状态栏提示。

完整配置下会做输入条“消毒”：关闭行号/符号列/诊断/`list`，并停用
smear-cursor（动画会留下残影）。cmp 等补全浮窗仍可能被输入条边界裁剪——这是单 grid
模式的已知限制，需要干净输入条时用 `minimal = true`。

### 工具与审批

daemon 目前向模型暴露四个工具：

| 工具 | 行为 | 默认审批 |
| --- | --- | --- |
| `read_file` | 读取 session cwd 内的文本文件 | 自动放行 |
| `list_dir` | 列出 session cwd 内的目录 | 自动放行 |
| `write_file` | stage 到 shadow workspace，审批后写入 cwd 内文件（自动建父目录） | stage + 审批落盘 |
| `run_command` | 在 cwd 内用 `sh -c` 执行命令，带超时；默认套 bubblewrap 沙箱 | 需要审批 |

所有路径都会做 workspace 沙箱检查（拒绝 `..` 和 symlink 逃逸）；工具输出按
`max_output_bytes` 截断。`write_file` 会先生成 unified diff 和逐 hunk 预览，TUI 审批卡片、
`eix chat` 和 `eix log` 都会显示；在 TUI 里用 `d` 全屏审阅可以按 hunk 选择应用，命令行用
`eix approve --hunks 0,2`（不带参数或空选择 = 整文件）。策略由 `[tools]` 配置或环境变量控制：

```toml
[tools]
policy = "ask"            # ask（默认）/ auto / read_only
max_output_bytes = 20000
command_timeout_secs = 60
sandbox = "auto"          # auto（默认，有 bwrap 就用）/ bwrap / off
sandbox_network = true    # false 时加 --unshare-net
sandbox_read_home = true  # false 时 $HOME 完全隐藏
snapshots = true          # false 时跳过执行前快照（大仓库可关）
```

`run_command` 在 `auto`/`bwrap` 下用 bubblewrap 隔离：宿主目录只读挂载，只有 session
workspace 可写，`/tmp` 是私有 tmpfs；`$HOME` 默认只读挂载（工具链缓存、git 配置可读但
不可写），设为 `false` 则 `$HOME` 指向沙箱内的 `/tmp`。默认保留网络。
`bwrap` 未安装时 `auto` 退回直接执行，`bwrap` 模式会拒绝执行；`off` 完全关闭沙箱。
`eix doctor` 会显示当前模式和 bwrap 是否可用。

`auto` 会跳过审批直接执行写操作（危险，显式选择才生效）；`read_only` 直接拒绝写工具
并把原因回传给模型。一次 turn 最多 12 轮工具往返。

### Shadow workspace（staged 写入）

`ask` 策略下，`write_file` 不再阻塞 turn，而是先把提案 stage 起来：

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
- 快照字节只存在 `snapshots/` 目录，事件日志只做索引。目前还没有 rewind 命令，这是
  Phase 3 rewind / 回滚的基础；`Snapshotter::restore` 已实现并有测试覆盖。
- 大仓库可以设 `[tools] snapshots = false`（或 `EIX_TOOL_SNAPSHOTS=0`）关掉；关闭后不再
  记录 `snapshot` 事件。`eix doctor` 会显示当前开关。

### 外部通知

`[notifications]` 让后台 session 进入 attention（`awaiting_approval` / `error`）时提醒你：

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
- `Ctrl-J` 换行（spike 里按 nvim 配置走；TUI 的输入条直接让 nvim 插入字面换行，
  避免被 `cmp.select_next_item()` 之类的映射抢走）
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
- [x] `eix` CLI（chat / send / log / ls / new / rename / cancel / approve[--hunks] / deny / doctor）
- [x] TUI v1（多 pane 自动平铺、焦点切换、转录、session 栏、fuzzy picker、多行输入、流式跟随）
- [x] 工具调用 + 审批门禁（read_file / list_dir / write_file / run_command、workspace 沙箱、审批卡片）
- [x] 写入前 unified diff 预览（TUI 卡片、`d` 全屏审阅、`eix chat` / `eix log`）
- [x] stale-write 冲突检测（stage 后文件被外部修改 → `patch_conflict`，不会覆盖）
- [x] hunk 级审批（TUI `d` 审阅里勾选 hunk，daemon 按选择应用部分改动）
- [x] attention 标记（后台 pane 高亮、`a` 跳转、审批卡片重放）
- [x] 外部 attention 通知（`[notifications]` command + TUI 终端铃、冷却、重放）
- [x] 嵌入 nvim 输入（ext_linegrid 渲染、按键转发、动态高度、失败回填、native 回退）
- [x] bubblewrap 沙箱（`run_command` 默认隔离，`[tools] sandbox` 可配，写入只限 workspace）
- [x] staged shadow workspace（propose → stage → apply、合并读视图、`patch.apply` / `patch.reject`、跨重启恢复）
- [x] 执行前快照（write_file 单文件 / run_command 整个 workspace，`snapshot` 事件，保留 20 份）
- [ ] context ledger、process view
- [ ] rewind / fork、全文搜索、worktree、子 agent
- [ ] daemon 化的 nvim 前端（JSON-RPC over Unix socket）、远程 attach（TCP + token）
