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

[context]
budget_tokens = 32000          # soft ceiling for one request
instructions_files = ["AGENTS.md"]  # cwd-relative; also reads <config>/AGENTS.md
workspace_summary = true       # top-level entries + git branch/status
```

环境变量覆盖：`EIX_BASE_URL`、`EIX_MODEL`、`EIX_API_KEY`、`EIX_API_KEY_ENV`、
`EIX_SYSTEM_PROMPT`、`EIX_TIMEOUT_SECS`、`EIX_TOOL_POLICY`、`EIX_TOOL_MAX_OUTPUT`、
`EIX_TOOL_TIMEOUT_SECS`、`EIX_TOOL_SANDBOX`、`EIX_TOOL_SANDBOX_NETWORK`、
`EIX_TOOL_SANDBOX_READ_HOME`、`EIX_TOOL_SNAPSHOTS`、`EIX_CONTEXT_BUDGET`、
`EIX_CONTEXT_WORKSPACE`、`EIX_NOTIFY_COMMAND`、`EIX_NOTIFY_COOLDOWN_SECS`、
`EIX_NOTIFY_BELL`、`EIX_SOCKET`、`EIX_DATA_DIR`、`EIX_CONFIG`。

协议是 Unix socket 上的 newline-delimited JSON：请求带 `id`/`method`/`params`，
响应带 `id`，事件通知是 `session.updated`、`session.event`、`turn.started`、
`message.delta`、`tool.approval_required`、`tool.started`、`tool.completed`、
`patch.proposed`、`patch.applied`、`patch.rejected`、`patch.conflict`、
`context.updated`、`context.compacted`、`session.attention`、`turn.completed` 等。
`eix-core/src/protocol.rs` 是唯一契约来源。

当前限制：`write_file` 在 `ask` 策略下先 stage 到 shadow workspace、不阻塞 turn，读取
（以及后续写入）看到的是“真实工作区 + staged”合并视图；`run_command` 仍在审批处阻塞
（shell 无法 stage）。bubblewrap 沙箱已接入；context ledger 与 process view 已接入
（见下）。TUI 已支持多 pane；后台 pane 的 attention 用边框/侧栏高亮和
`a` 跳转提示，外部通知命令（dwm/tmux）已接入（见 `[notifications]`）；输入框已默认使用
嵌入的 Neovim（见下）。

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
| `n` / `N` | 新建 session 并在新 pane 打开（有待审批时 `n` 先作为 deny；`N` 同时在 `.worktrees/<name>` 建 git worktree） |
| `y` / `n` | 批准 / 拒绝聚焦 pane 的待审批工具调用 |
| `d` | 全屏审阅待审批写入的 diff；有 hunk 时 `[`/`]`（或 `J`/`K`）跳 hunk、`Space` 勾选、`a` 全选/全不选、`y` 应用选中、`n` 拒绝 |
| `c` | context ledger：`j/k` 移动、`o` 展开、`p` pin、`d` drop/恢复、`m` 标记、`s` 压缩（单条或 mark 到光标的范围）、`f` fork、`w` rewind 并回滚工作区、`Esc` 关闭 |
| `P` | 后台进程：`j/k` 选择、`x` kill、`r` 刷新、`PgUp`/`PgDn` 滚动输出、`Esc` 关闭 |
| `F` | fork 当前 session；在 git 仓库里自动建 worktree，仓库外共享工作区 |
| `#` | 全文搜索历史消息：输入查询回车执行，`↑/↓` 选择，再回车打开对应 session |
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

daemon 目前向模型暴露这些工具：

| 工具 | 行为 | 默认审批 |
| --- | --- | --- |
| `read_file` | 读取 session cwd 内的文本文件 | 自动放行 |
| `list_dir` | 列出 session cwd 内的目录 | 自动放行 |
| `write_file` | stage 到 shadow workspace，审批后写入 cwd 内文件（自动建父目录） | stage + 审批落盘 |
| `run_command` | 在 cwd 内用 `sh -c` 执行命令，带超时；默认套 bubblewrap 沙箱 | 需要审批 |
| `start_command` | 在沙箱里启动后台进程并立即返回 id，日志落盘 | 需要审批 |
| `process_output` | 读取后台进程的尾部输出（只读） | 自动放行 |
| `spawn_agent` | 起一个只读子 agent 并发处理独立问题，返回最终回答 | 自动放行（子 agent 只读） |

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
- 快照字节只存在 `snapshots/` 目录，事件日志只做索引。`Snapshotter::restore` 已接进
  `session.rewind`：`eix rewind` / TUI ledger 的 `w` 会按 `seq >= at` 倒序恢复快照并返回
  新 session。回滚是尽力而为的，恢复失败、缺失文件、被 prune 的旧快照都会在结果的
  `warnings` 里列出，`restored = false` 表示工作区没有完全回到切点。
- 大仓库可以设 `[tools] snapshots = false`（或 `EIX_TOOL_SNAPSHOTS=0`）关掉；关闭后不再
  记录 `snapshot` 事件。`eix doctor` 会显示当前开关。

### Context ledger

每次请求的上下文由 daemon 从事件日志重建，并按 sector 记账（`eix context` 或 TUI 里按 `c`）：

| sector | 内容 | 预算 |
| --- | --- | --- |
| system | 内置系统提示 | 5% |
| instructions | `AGENTS.md`（session cwd，以及 `<config>/AGENTS.md`） | 5% |
| workspace | 顶层文件树 + git branch/status | 15% |
| history | 对话（user / assistant） | 60% |
| tools | 工具调用与输出（同一 assistant 的 tool_calls 与其结果算一步） | 10% |
| memory | 预留 | 5% |

- token 是启发式估算：ASCII 约 4 字符/token，CJK 约 1 字符/token，每条消息另加 4 token
  结构开销；底部显示总量、预算和剩余。
- `tools` / `history` 超出 sector 预算时，最旧的未 pin 条目会自动标 `cut`；始终保留最新 4 条
  （模型刚拿到的工具输出不会立刻消失）。`system` / `instructions` / `workspace` 不会被自动
  裁剪，只显示用量——静默丢掉 `AGENTS.md` 会改变 agent 行为。
- TUI：`j/k` 移动、`o` 展开、`p` pin/unpin、`d` drop/恢复、`m` 标记、`s` 压缩（单条，或从
  mark 到光标的范围）、`Esc` 关闭。`eix context --drop/--undrop/--pin/--unpin/--compact` 等价。
- drop / pin / compact 都是事件（`context_dropped` / `context_pinned` /
  `context_compacted`），重启后重放一致。compact 把一段历史换成一条 summary；原始事件仍在
  日志里并显示为 `sum`，但不再发给 provider；范围不能与已有 summary 重叠。
- workspace summary 和 instructions 在每次 turn 开始时重新读取，所以改 `AGENTS.md` 或切
  branch 后下一条消息就会生效。`[context]` 可改预算、指令文件名或关掉 workspace summary。

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

### 子 agent

`spawn_agent` 让模型把独立的探索拆给子 agent。daemon 为每个调用建一个 child session：
`origin = agent`、`parent_id` 指向父 session、名字是 `parent/<label>-N`、`cwd` 继承父的
工作区。子 session 只拿到 `read_file` / `list_dir` / `process_output` 三个只读工具，模型
即使硬调 `write_file` 也会被 daemon 直接拒绝，所以子 agent 不可能改工作区或提权；它也
不能再 `spawn_agent`（深度上限 1，避免递归失控）。

一个 assistant 批次里的多个 `spawn_agent` 会并发跑（`tokio::spawn`），其它工具仍按顺序
执行；结果按调用顺序回填给父模型。子 agent 的 turn 完整写进它自己的事件日志，因此
`eix ls` 能看到、`eix log` 能回放、TUI 侧栏用 `↳` 标出。子 agent 失败时会作为 tool
结果返回给父模型（父可以重试或忽略），不会把父 turn 直接打成 error。

### 远程 attach（TCP + token）

默认只监听 Unix socket（0600，靠文件系统权限保护）。要跨机器用，在配置里打开 TCP：

```toml
[server]
tcp = "127.0.0.1:7777"   # 或用 EIX_TCP
token = "..."            # 打开 tcp 时必填，非空
```

设置 `tcp` 后 token 是强制的（配置校验会拒绝空 token）。TCP 连接必须把 `auth {token}`
作为**第一个**请求：校验通过前不会返回任何数据或事件，失败则返回 `unauthorized` 并断开。
token 比较不会在第一个不同字节短路。Unix socket 仍然不需要 auth。

```sh
eix --tcp 127.0.0.1:7777 --token "$EIX_TOKEN" ls
eix --tcp 127.0.0.1:7777 --token "$EIX_TOKEN" tui
```

CLI/TUI 的 `--tcp`（或 `EIX_TCP`）会改用 TCP，`--token`（`EIX_TCP_TOKEN`）带上凭证；
TCP 端点是远端，`eix` 不会尝试在本地拉起 daemon。注意：Unix 连接会收到所有 session 的
广播，TCP 连接在认证后同样如此——按 session 过滤连接留待后续。

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
| `:Eix list` | 列出 session |
| `:Eix new [NAME] [worktree]` | 新建 session（可选独立 worktree） |
| `:Eix send TEXT` / `:Eix prompt` | 发送 prompt；`prompt` 开一个输入分屏，`<CR>` 发送 |
| `:Eix fork [SEQ]` / `:Eix rewind [SEQ]` | 分叉 / 回滚并切到新 session |
| `:Eix stop` | 取消当前 turn |
| `:Eix ps` / `:Eix ledger` | 后台进程 / context ledger |

每个 Neovim 进程是一个独立 client，共享同一个 daemon；daemon 会串行化同一 session 的
turn，第二个 prompt 在忙时会被拒绝。`eix tui` 内嵌的 nvim 仍只负责输入条，二者互不影响。

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
- [x] context ledger（sector 预算 / token 记账、AGENTS.md + workspace 注入、drop / pin / compact、TUI `c` 与 `eix context`）
- [x] process view（daemon 托管后台进程、`start_command` / `process_output`、流式输出与日志、TUI `P`、`eix ps/tail/kill`）
- [x] fork / rewind（按消息裁剪历史复制到新 session；`eix rewind` / ledger `w` 还会按快照倒序回滚工作区，并报告无法恢复的改动）
- [x] git worktree（`--worktree` / TUI `N` / repo 内 fork 自动建 `.worktrees/<name>`、`eix worktree list|remove`）
- [x] 子 agent（`spawn_agent`、只读工具集、并发批次、深度上限 1、独立事件日志）
- [x] daemon 化的 nvim 前端（内嵌 Lua 插件 + JSON-RPC、`:Eix` 命令、headless 测试）
- [x] 远程 attach（TCP + 强制 token、首请求 `auth`、CLI `--tcp/--token`）
- [x] 全文搜索（FTS5 trigram + 短查询子串回退、TUI `#`、`eix search`）
- [ ] 按 session 过滤 TCP 广播、审批 `e`/`o`/reason UI、`eix pick`、`status --json`、CI
