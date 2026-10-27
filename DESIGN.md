# eix — AI Agent 交互设计

> 状态：v0.3（2026-10-27）
> 定位：面向 vim / dwm / Arch 工作流的 coding agent；本文件是交互与技术契约，随实现修订。
> 名字：`eix`，取自欧拉公式 `e^{ix} = cos x + i·sin x`；加泰罗尼亚语中意为「轴」。
> v0.2 变更：命名统一为 `eix`（`g:eix_prompt` / `:Eix` / `eix pick`）；补充实测的 redraw/linegrid 契约；修正 `<leader>ac` 键位冲突；加入实现状态快照。
> v0.3 变更：README 收敛为项目级介绍；TUI 完整键位、工具/沙箱参考、扩展系统、HTTP/远程 attach 与实现清单合并为 §19–§20。

## 1. 目标与非目标

**目标**

- 与 codex 同级的 coding agent（工具调用、补丁、审批、上下文管理）。
- 多 session 是一等公民：常驻、可 attach/detach、可并行、可模糊切换。
- 输入框是**自研的 Vim 风格编辑器**：常用 vim 键位与编辑语义，无子进程、无配置加载。
- fzf 式体验贯穿所有选择面：session、文件、命令、diff hunk。
- 与 dwm / tmux / rofi / dmenu / dunst 自然协作，不要求换工作流。

**非目标（v1）**

- GUI / Web 前端；多人实时协作；插件市场。

## 2. 核心决策

| 决策 | 选择 | 理由 | 代价 |
|---|---|---|---|
| 形态 | daemon + TUI 主前端，预留 nvim 前端 | session 需跨终端/崩溃存活；前端可替换 | 协议先行，工程量最大 |
| 语言 | Rust | 渲染性能、单二进制、类型化协议 | 开发速度慢于 TS |
| session | append-only 事件日志，SQLite 持久化 | rewind/fork/审计/搜索都是推论 | 需处理"工作区不可重放" |
| 多 session | pane 多开 + 全局 fzf picker + dwm 全局跳转 | 同屏并行与秒切并存 | pane 布局与焦点管理 |
| 输入 | 一个 client 一个内置模态编辑器，对准聚焦 pane | 零启动/按键延迟；模态语义自持 | 只覆盖常用 vim 子集 |
| 内核 | 从零手写 | 上下文与工具策略完全可控 | 工作量与质量风险 |
| provider | 优先 OpenAI 兼容接口 | 一套 adapter 覆盖 OpenAI / Ollama / vLLM / OpenRouter 等，本地模型零成本接入 | 各家非标准字段需归一 |
| leader | `<Space>`（可配置；输入框 normal / 转录区） | 单字母主要留给 vim；TUI 动作走 leader，少量窗口动作（`q`/`Z`/`<`/`>`/Tab）是 normal 默认 | insert 下 TUI 键不可用，全局键用 `Ctrl-*` / `F1` 补充 |
| 转录 | ratatui 自绘 + 对话式布局 + 可折叠块 | 流式 reasoning/工具/审批卡片可见，性能可控 | 需维护一套只读导航键位 |
| 审批 | propose → stage → apply | hunk 级审批才有意义；可回滚 | 需虚拟工作区/补丁暂存 |
| 上下文 | 内部自动管理的 context ledger | 不需要用户维护 | 中等额外工作量 |

## 3. 架构

```
┌────────────────────────── client (Rust / ratatui) ──────────────────────────┐
│  pane 1 (session A transcript)   pane 2 (session B transcript)              │
│  ┌────────────────────────────────────────────────────────────────────────┐ │
│  │ 全局输入条：内置模态编辑器（normal/insert/visual），随聚焦 pane 切换   │ │
│  └────────────────────────────────────────────────────────────────────────┘ │
│  statusline / tabline / overlays（picker、diff、context、help）             │
└───────────────▲──────────────────────────────────┬──────────────────────────┘
                │ 事件流（notifications）           │ 命令（requests）
                │  Unix socket / JSON-RPC           ▼
┌────────────────────────────── daemon (Rust / tokio) ────────────────────────┐
│ session registry · event log (SQLite) · agent runtime · context builder     │
│ tool exec · staging workspace · approval engine · provider adapters         │
│ worktree manager · notification bus · FTS index                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

- daemon 首次调用自动拉起；client 崩溃/退出不影响运行中的 session。
- 协议传**结构化事件**（message delta、tool call、patch、status），不传渲染文本；为未来的 nvim 前端保留全部信息。
- 前端能力握手：`protocol: 1` + `capabilities`。

## 4. Session 模型

### 4.1 定义

一个 session = 持久化实体：

- **事件日志**：user/assistant 消息、工具调用、补丁、审批、状态变化、压缩。
- **工作区绑定**：cwd、可选 git worktree、branch、权限级别。
- **运行时**：正在跑的任务、后台进程、子 agent、待审批项。
- **元数据**：前两轮对话后由模型总结自动命名（保留英文词间空格、去标点、≤32 字符含空格；手动命名优先）、标签、pin、frecency。

### 4.2 状态机

```
idle ──submit──> running ──tool──> tool_running ──> running ──done──> idle
                   │                    │
                   ├──approval──────────> awaiting_approval ──respond──> running
                   ├──question──────────> awaiting_input ──submit──> running
                   └──error─────────────> error
```

状态符号：`○ idle`、`● running`、`⚠ approval`、`◐ waiting`、`✗ error`、`◌ paused`。
statusline 左侧对 idle / running 只显示图标：idle 为静态 `⠶`，running 为逐帧旋转的田字方块 `◰/◳/◲/◱`。

### 4.3 attach / detach / 所有权

- session 常驻 daemon；client attach 只是订阅 + 渲染。
- TUI / `chat` / `send` 未指定 session 时，按启动目录选择最近更新的 user session attach；
  当前目录没有 session 则新建。其他只读 CLI 命令找不到当前目录 session 时回退到最近 session。
- TUI 按启动目录把仍打开的 pane 记录在 `<data_dir>/workspaces.json`：`Ctrl-C` / `Ctrl-D` /
  `Z` / `/quit` / `/exit` 退出时保存 pane 顺序、焦点与 layout，下次启动依次重新 attach；
  `:q` / `:qa` / `:wq` / `:x` 或最后一个 pane 的 `/close` 退出时清除记录。记录为空或 session
  已失效时，回退到该目录最近更新的 user session；旧记录缺少 layout 时按 grid 处理。显式
  session 参数忽略保存的 pane、焦点与 layout。
- 同一 session 可被多 client 观察，但**只有一个输入所有者（controller）**。
- 所有者断开 → 自动转移给最早的 watcher；显式抢占用 `<leader>a C`（claim）。
- draft 由 daemon 持有，client 的 nvim 只是视图；所有权切换时重新加载 draft。
- 新建 session 在首次输入前只是空壳：TUI 退出时对本次创建的空 session 调用
  `session.delete_if_empty`，daemon 用单条 SQL 原子检查“无事件、无草稿、无 worktree”后删除，
  避免污染历史列表；有草稿或 worktree 的 session 保留。

### 4.4 切换与去重

- 同一 client 内，一个 session 只在一个 pane 打开；picker 选中已打开的 session 时**聚焦该 pane**，不重复打开。
- picker 排序：attention（待审批/待输入）置顶，其余按 frecency。
- 支持多选 session → 一次打开多个 pane（`Tab` 选择，`Enter` 打开）。

### 4.5 fork / rewind

- **fork**：复制前缀到新 session，命名 `name~2`，header 显示 `name ← name~2`；只复制对话，不回退文件。
- **rewind**：`session.rewind {session_id, at_seq?, name?}` 先按 `at_seq`（`None` = 末尾）fork，再把工作区回滚到切点：
  daemon 在每个会改文件的工具调用前抓快照，rewind 时取 `seq >= at_seq` 的快照，按 seq **倒序**恢复（最新的先撤，才能回到更早的状态）。
- **诚实性约束**：事件日志不能重放工作区。rewind 是**尽力而为**：返回
  `RewindResult { restored, snapshots, files_restored, files_removed, files_missing, warnings }`，
  `restored = false` 表示有快照恢复失败或文件缺失；被保留策略 prune 的旧快照、快照上限之外的大文件、
  以及 eix 之外的改动都无法察觉，`warnings` 里逐条说明，绝不假装成功。
- busy 的 session 拒绝 rewind（`session_busy`）。
- 已实现（2026-10-18）：`session.fork` 仅复制历史，`session.rewind` 组合 fork + 快照回滚；
  CLI `eix rewind [--session ID] [--at SEQ] [--name NAME]` 走 rewind，`F` / `f` 仍是纯 fork。
  fork 默认共享源工作区；`worktree.auto: true` 时才在 git 仓库里同时起 worktree（见下）。

### 4.6 worktree

- 默认绑定 cwd 并共享当前工作区。`eix new --worktree` 新建隔离 session；TUI
  `/new` 在当前 session 有 worktree 时于同一 worktree 新建 session（新 session 只共享
  目录，不持有 worktree 标记），否则共享其 cwd，不创建新 worktree；`/new-worktree` /
  `<Space>W` 则把**当前 session** 移入
  `.worktrees/<slug>`，已有 worktree 时只提示、不重复创建。
  worktree 从**执行命令时 cwd 所在的分支**拉出，而不是固定从 `master`；`worktree.base`
  或 per-repo `base` 显式设置时覆盖。自动 worktree 的名称由模型根据任务总结成英文 kebab-case；
  模型失败时用 ASCII 词或 `task-<timestamp>`。`worktree.auto: true` 可恢复 CLI /
  自动启动 session 的首条 prompt 自动创建；`eix new --shared` 显式共享工作区。
- repo 内 fork 默认共享源工作区；`worktree.auto: true` 时才给新分支建 worktree；`eix fork --shared` / `session.fork worktree=false` 显式共享。
- worktree 与 session 生命周期解耦：session 归档不自动删除 worktree，需显式确认。
- 已实现（2026-10-18）：`eixd/src/worktree.rs` shell out 给 git，`git worktree add -b eix/<slug> .worktrees/<slug>`；
  main repo 用 `git rev-parse --git-common-dir` 解析并 canonicalize（支持 subdir 和 linked worktree），
  `.worktrees` 已在快照排除表里；创建时也会写入 `.git/info/exclude`，避免未提交
  `.gitignore` 的仓库把 worktree 目录显示成 untracked 并卡住合并。删除走
  `eix worktree remove SESSION [--force]`（`session.worktree.remove`）：
  先算出主仓库，`git worktree remove [--force]`，成功后把 session cwd 重置回仓库根并清空 worktree 字段；
  工作区有未提交改动时 git 会拒绝，除非显式 `--force`（dirty 删除返回 `removed:false` 并给出提示）。

### 4.7 集成队列

- session 的 `pending → ready → syncing → rebasing → testing → merging → merged` 状态存在
  `session_integrations` 表；`retrying` 带 `next_retry_at` 和指数退避，不设硬 blocked。
- `eix ready`（`session.ready`）检查：无 turn、worktree 干净；默认
  `auto_commit = true` 会先提交 dirty worktree（设为 false 则要求先手动提交）。如果存在
  残留 rebase，用户侧 `ready` 会先把冲突/暂存内容备份到 `.git/eix/recovery/`、创建
  `refs/eix/recovery/<stamp>`，再 abort 并继续；agent turn 内调用 `ready` 且仍有冲突或
  暂存修改时则返回明确错误，要求先 `rebase --continue` 或 `rebase --abort`，避免误丢
  正在解决的冲突。agent turn 内也可以用无审批的 `ready` 工具提前置为 ready，队列会等
  turn 结束。
  默认 `auto_ready = false`，`/ready` 只由用户触发；显式设为 `true` 时才扫描空闲
  session（无 turn/审批/输入、草稿为空、至少一条 user message、worktree 干净，且
  `idle_secs` 内无更新）。
- 单个 daemon worker 串行处理队列：基分支是 session 创建时所在的分支；用
  `git worktree list --porcelain` 找到该分支实际 checkout 的 worktree（可能是主 worktree，
  也可能是用户自己的 linked worktree），在它干净时 `fetch` + `pull --ff-only`（本地 feature
  无 upstream 时跳过 pull），把 session 分支 rebase 到基分支，运行 `integration.test`
  （`auto` 识别 Cargo/npm/Go/Python/Make；也支持子目录项目，优先 session cwd 所属项目，
  多候选不猜；没有测试命令则拒绝合并），确认基分支 SHA 未变化后在**这个基 worktree** 里
  `merge --ff-only`。`push=false` 默认只更新本地基分支，不会自动合并 master；feature →
  master 由用户自己控制。worktree 在合并后保留。同一 feature 上的多个 session 串行合并，
  后合并者会 rebase 到包含前者的新 feature HEAD。
- 队列里程碑写为 `integration` 事件（queued / syncing / rebasing / testing / merging /
  merged / retrying / repairing），包含实际执行的 git / 测试命令和输出尾部；transcript
  和 `eix log` 可见，不进入 provider 的 chat context。
- rebase 冲突、测试失败、无测试命令、dirty base 或 merge 失败把错误注入 session
  （`repairing`），agent 在 worktree 内解决、跑测试、再次 `eix ready`，需要人工决定时可用
  `ask_user`。失败发生在 agent 当前 turn 尚未结束时先标记 `repair pending`，等 turn 结束后
  再注入。`conflict_repair=false` 时只重试并通知。
- 连续失败达到 `notify_after` 发 `integration.attention`；`pause_after>0` 时暂停等待
  `integration.continue`；同一错误连续 3 次也会自动暂停，避免重复无效重试。
- 残留 rebase 是 recoverable 状态，而不是硬 blocked：`ready`、队列尝试、repair watchdog
  和 daemon 重启都会先备份（patch、变更文件副本、`refs/eix/recovery/<stamp>`）再 abort；
  `conflict_repair=false` 的冲突重试同样先清理 rebase。daemon 重启时把遗留的 active 行
  恢复成 `retrying`，避免队列永久卡住。

## 5. 输入层：自研 Vim 风格编辑器

这是交互的核心。输入框**不启动子进程、不加载用户 Neovim 配置**，而是 `eix-tui` 内的
一个模态编辑器（`crates/eix-tui/src/tui/editor.rs`），按键直接改本地 buffer。只实现写
prompt 需要的常用 vim 子集，换取零启动成本与零按键往返。

### 5.1 模式与按键

| 模式 | 进入 | 主要按键 |
|---|---|---|
| insert | `i a I A o O` / 聚焦输入框 | 文本输入、`Backspace`/`Delete`、方向键、`Home`/`End`、`Tab`（4 空格）、`Ctrl-w/u/k/a/e`；`Ctrl-J` / `Ctrl-Enter` / `Shift-Enter` 换行；`Enter` 提交 |
| normal | `Esc` | `h j k l w W b B e E 0 ^ $ gg G f F t T ; ,`；`d c y` + motion / `dd cc yy` / `iw aw`；`x X D C s S r ~ J p P u Ctrl-r`；`v`/`V`；`Tab` 正向循环 Plan → Build → Auto → Bypass，`Shift-Tab` 反向循环 |
| visual / v-line | `v` / `V` | 同 normal 的 motion；`d x y c p ~ o`，选区反色 |

- 计数（`3w`、`2dd`）、preferred column（`j/k`）、`;`/`,` 重复 find 均按 vim 习惯实现。
- 撤销是快照栈：一次 insert 会话、一次 `dd`/`p` 各算一步；`u` / `Ctrl-r`。
- 未命名寄存器保存最近一次 yank/delete；`p`/`P` 区分字符与整行；寄存器为空时读取
  系统剪贴板（鼠标选中的转录文本可直接粘贴），且外部粘贴不覆盖内部寄存器。
- leader 前缀默认 `<Space>`，可通过 `[keys].leader` 修改；输入框 normal 与转录区共用同一映射：
  `n` 新建 session 并开新 pane（当前 session 有 worktree 时复用）、`W` 当前 session 移入 worktree、
  `o` 打开 session picker（已有 session 放进新 pane；只列普通/worktree session，
  不列子 agent）、`c` 关闭 pane、`f` 文件、
  `s` 文件内容（类似 rg）、`g` session picker、`h` 用户输入 timeline、`G` git（worktrees + branches）、
  `p` 进程、`a` 审批 inbox、`x` 取消、`m` 完整执行模式 picker、
  `v`/`t`/`r`/`l` 布局。单字母默认留给 vim；转录区裸
  `q` 关闭当前 pane（最后一个 pane 保持打开），裸 `Z` 退出，保留 `i`（聚焦输入框）、
  `j/k`/`g/G` 等导航、`/`/`?`/`#` 当前对话搜索和 `Ctrl-P`。
- 全局 / TUI 动作和 leader 组合都可通过 `[keys]` 覆盖；配置某个动作会替换它的默认绑定，
  不会保留旧键作为别名。裸字母对 normal / 转录区动作安全；可配置为全局的动作
  （`quit`、`help`、`sessions`、`models`、`timeline`、`cycle_reasoning`、`tab_N`）配成裸字母时
  自动降级为 normal / 转录区专用。overlay 内部导航键固定。
- 输入框 normal 模式复用转录区的 TUI 动作：leader、`/`/`?`/`#` 当前对话搜索、翻页/滚动和
  `[keys]` 绑定行为一致；`j/k` 行移动、`Tab` / `Shift-Tab` 循环执行模式等 vim/编辑器键仍归输入框。
- `<` / `>`（即 `Shift+,` / `Shift+.`）在转录区和输入框 normal 模式线性切换上一个 / 下一个 pane。
- 在转录区和输入框 normal 模式，1 秒内连续按两次 `Ctrl-W` 聚焦下一个 pane；最后一个 pane 会回绕到第一个。
  insert 模式的 `Ctrl-W` 仍用于删除前一个词。
- `Ctrl-J`/`Ctrl-K` 上下翻转录历史（`j`/`k`、`3j`、`dj`、`↑`/`↓` 是草稿内行移动）。
- 输入框 normal 模式与转录区的 `Tab` 按 Plan → Build → Auto → Bypass → Plan 正向循环，
  `Shift-Tab` 反向循环（Build → Plan → Bypass → Auto）。`Space m`（或 `/perm`）仍打开完整
  picker 直接选择某个模式。输入框下方 statusline 左侧显示当前模式；本地立即更新以支持
  连续切换，daemon 的 session 事件负责最终校正。
- `Esc`：insert/visual → normal 并保持焦点；normal 下 500ms 内再按一次回到转录区。
- `Enter` 在 insert 提交；normal 模式把当前 pane 的 session 提到第一位。
- `@`（insert，行首/空白后）打开文件 picker；`<Space>g` / `<Space>h` 在 normal / 转录区打开
  session picker / 用户输入 timeline。
- picker：`Ctrl-J`/`Ctrl-K`（或 `↑`/`↓`）上下选择。
- 不再有 `[input] backend` / `minimal` 配置，也没有 nvim 回退路径。

### 5.2 buffer 与 draft

- 一个 client 一个 buffer，随聚焦 pane 切换；切换时先保存旧 session 的 draft，再
  `draft.get` 回填新 session。
- 编辑变化在 200ms tick 内同步到 `draft.set`（只写真实变化）；提交失败会把 draft 放回。
- 粘贴：bracketed paste 整段插入，`\r\n` / `\r` 归一为 `\n`。

### 5.3 输入条布局

```
╭───────────────────────────────────────────────────────────╮
│   把登录改成 JWT，并补测试                               │
│                                                           │
│                                                           │
╰───────────────────────────────────────────────────────────╯
 N · Build · ○ · deepseek-v4.1-flash · deepseek · medium     ~/git/eix · master
```

- 默认 3 行内容 + 上下边框（共 5 行）；短终端优先让给转录区；长草稿最多到 10 行内容
  （总 12 行）后开始滚动。
- 输入框只保留正文和边框，不再有底部信息行；拒绝理由会临时占用顶部边框，错误则显示在
  居中的浮动对话框里，按 `Esc` / `Enter` / `q` 关闭。
- 输入框下方保留一条 Vim 风格 statusline：左侧依次是 `N/I/V/VL · SessionMode · <状态图标> ·
  模型 · provider · reasoning`（模式徽标 `I` 绿 / `N` 蓝 / `V` 琥珀，命令行激活时显示 `C`；
  失焦时降低对比度；
  Plan 琥珀、Build 蓝色、Auto 淡橙、Bypass 粉红；idle 显示 `⠶`，running 显示田字方块动画，
  其他状态显示符号和状态名；四种模式固定 6 列宽（Bypass 最长），切换时状态栏不位移）。
  右侧靠右显示 `cwd（~ 缩写） · git 分支`；
  worktree/图片数、运行中的 integration 状态，以及多 pane 时的 `当前/总数`、排队数和 attention
  数在空间允许时出现在中部或右侧，变窄时优先保留 cwd/branch。只有没有其他可见反馈的失败、
  后台 attention、复制确认和 `/reload` 完成使用右上角浮动通知；单条约 1.5–3 秒，最长 3 秒。
  多条同时存在时逐条堆叠，最新消息立即显示，不设置最低等待时长。成功的命令反馈（`/reload`
  除外）、进度和 picker/弹窗内容不再重复通知，因此状态栏不会随命令消息跳动。
- 终端高度足够时，statusline **下方**常驻一行空白命令行；normal 模式按 `:` 激活并显示
  `:` 前缀 + 文本 + 光标，`Enter` 执行；`Esc` 先收起命令菜单并保留输入，再按一次关闭命令行；
  `↑`/`↓` 浏览历史，`Tab` 显示并补全命令名；
  菜单打开后 `Ctrl-J`/`Ctrl-K`、`Ctrl-N`/`Ctrl-P` 上下选择匹配项，`Enter` 执行选中项。命令行接受 `/命令`
  的同一套名字（不带 `/`），并额外支持 `:q` 关闭 pane、`:qa` 退出、`:w` 保存草稿、
  `:wq`/`:x` 保存并关闭、`:!cmd` 执行 shell、`:r!cmd` / `:r !cmd` 读取输出并插入输入框。
  shell 走与 prompt `!` 相同的沙箱、快照和只读限制；命令行打开时不改动 prompt 草稿；insert / visual 模式里的
  `:` 仍是普通字符。
- 输入框底边保持普通边框，不挂模式徽标，也不堆叠快捷键提示。
- 正文左侧留 1 格内边距；不再显示 `❯` 输入槽和占位文案，空输入时只保留光标。
- 视觉选区逐字符判断并反色渲染；终端光标跟随实际字符位置。

### 5.4 独立 nvim 前端（`eix nvim`）

除了 TUI，也可以让 Neovim 直接当整个前端：

- `eix nvim [--session ID]` 用用户的 nvim 打开一个内嵌 Lua 插件（`eix_core::nvim_plugin` 写在
  `<data-dir>/eix-nvim.lua`，内容变化才重写），插件自己用 `vim.uv`（`vim.loop` 兼容）连 Unix socket
  或 TCP，说同一套换行 JSON-RPC。
- 转录渲染进一个 `eix://<id8>` 的 scratch buffer（`botright` 分屏），用户自己的配置、配色和键位都在。
- `:Eix` 命令：`attach [ID|name]` / `list` / `pick [KIND]`（`KIND` = `sessions`(默认)/`files`/`worktrees`/
  `branches`/`context`；选中 files/branches/worktree 插入光标处，context 在分屏显示条目详情）/
  `new [NAME] [worktree]` / `send TEXT` / `prompt`（输入分屏，`<CR>` 发送、`@` 文件 picker）/
  `fork [SEQ]` / `rewind [SEQ]` / `stop` / `ps` / `mode [MODE]` / `doctor`。
  eix buffer 里 `<C-g>` 打开 session picker，`<C-g><C-g>` 回到上一个 session；`<leader>a` palette
  也含 file / worktree / branch / context 入口。
- 连接和请求都由插件内的 libuv 处理；注意 **libuv 回调是 fast context**，`vim.env` / `vim.fn.*` /
  `nvim_buf_*` 都必须先 `vim.schedule` 或把环境值在加载时缓存进 `state`（实测坑）。
- 每个 nvim 进程是独立 client，共享同一 daemon；daemon 串行化同一 session 的 turn，忙时拒绝。
- 测试：headless e2e 直接跑 `nvim --headless -u NONE --cmd 'lua dofile(...)' -c 'Eix attach ...'`，
  断言 scratch buffer 里出现 prompt 和回答；`#[tokio::test]` 是 current-thread runtime，阻塞式
  `Command::output()` 会饿死 daemon，必须放 `spawn_blocking` 里跑。

## 6. 布局与焦点

- pane 树：水平/垂直分割，每个 leaf 显示一个 session 的转录。
- 布局模式：`grid`（默认自动平铺）、`horizontal`（单列，第一个 pane 固定占 55% 高度；pane 超过 5 个
  时只渲染第一个 pane + 后续 4 个，其余隐藏，焦点移入隐藏 pane 时窗口滑动；空间不足时压缩为
  标题行/单行内容，不退回 tabby）、`tabby`（输入框
  上方显示 tab，只渲染当前 session）、`fibonacci`（左/上/右/下循环切分的螺旋分屏，不显示 tab）。用 leader
  `<Space>v/t/r` 切换，`<Space>l` 循环；`Ctrl-1` … `Ctrl-9` 聚焦第 N 个已打开 session，
  在 `tabby` 下即切换到第 N 个 tab。TUI 启动时请求 kitty 键盘协议（CSI-u），否则传统终端
  会把 `Ctrl-2`/`Ctrl-3` 等编码成 NUL/Esc；不支持该协议时用 `<` / `>` 或
  `Ctrl-W Ctrl-W` 移动焦点。
- session 顺序：`views` 是稳定顺序（可视为双向链表），pane 打开后保持位置。发送任务、turn
  完成、等待审批/输入都不会移动 session；焦点只改变当前节点，不改变 `views` 顺序。
  normal 模式按 `Enter` 会把当前 pane 的 session 主动提到第一位；用 `<` / `>`（`Shift+,` /
  `Shift+.`）或 1 秒内连续两次 `Ctrl-W` 在前后节点间移动，`Ctrl-1` … `Ctrl-9` 直接跳到对应 session。
- 不再有独立顶部 header 行，输入框也不再有底部信息行；输入框下方保留一行 statusline，
  左侧按 `N/I/V/VL · SessionMode · 状态图标 · 模型 · provider · reasoning` 排列，右侧靠右显示
  cwd（`~` 缩写）和 git 分支；worktree/图片数、运行中的 integration 状态、pane 位置、prompt
  队列和 attention 信息在空间允许时出现在中部或右侧，变窄时优先保留 cwd/branch。只有没有
  其他可见反馈的失败、后台 attention、复制确认和 `/reload` 完成显示在右上角浮动通知中，
  多条可同时堆叠、最新一条在上方，不占用 statusline 中部。
- 焦点：`<` / `>`（`Shift+,` / `Shift+.`）线性切换 pane；转录区按 `i` 聚焦输入并进入 insert；
  `Ctrl-1` … `Ctrl-9` 直接跳到对应 session。聚焦 pane 使用
  更亮的单格虚线边框、标题加粗；非聚焦 pane 保持低对比实线灰，attention 仍用警示色。
- `Esc`：insert/visual 下先回到 normal 并保持输入框焦点；normal 模式下 500ms 内连按两次才把
  焦点交回转录区（已实现）。
- 输入条默认显示 3 行内容（加边框共 5 行；短终端优先让给转录区），长草稿会向上生长到
  10 行内容（总 12 行），让长命令和换行文本尽量完整可见；超过上限后在框内滚动。
  左侧不再有常驻 session 栏，session 只经浮动 picker 打开。
- 输入条复用转录区的圆角面板与低对比度调色：正文左侧留 1 格内边距；底边保持普通边框，
  模式徽标、`Plan/Build`、执行状态、模型/provider/reasoning 和 cwd/分支都在下方 statusline，
  不堆叠快捷键提示或占位文案。`/` 补全菜单使用同一套面板样式，选中行整行高亮。

## 7. 键位总表

> 内置编辑器与转录区共用一套可配置的全局 / normal / leader 键位；leader 前缀默认 `<Space>`，
> 可用 `[keys].leader` 修改。`eix nvim` 前端沿用你的 `mapleader`（下表以 `<Space>` 为例）。

**全局与窗口**

| 键 | 行为 |
|---|---|
| `F1` | 任意模式：可搜索帮助 |
| `<Space>g` | 转录区 / 输入框 normal：session picker |
| `<Space>h` | 转录区 / 输入框 normal：当前 session 的 user-input timeline |
| `<Space>G` | 转录区 / 输入框 normal：git picker（worktrees + branches） |
| `<C-t>` | 任意模式：reasoning low / medium / high |
| `<C-/>` / `<C-_>` / `<C-7>` | 任意模式：模型 picker |
| `Ctrl-1…9` | 聚焦第 N 个已打开 session（`tabby` 下为第 N 个 tab；需 kitty 键盘协议；`Ctrl-7` 通常留给模型 picker） |
| `<Space>v/t/r` | horizontal / tabby / fibonacci 布局 |
| `<Space>l` | 循环 grid / horizontal / tabby / fibonacci |
| `q` / `<Space>c` | 转录区 / 输入框 normal：关闭当前 pane（最后一个 pane 保持打开） |
| `<Space>a` | 跨 session 审批收件箱 |
| `<` / `>` | 转录区 / 输入框 normal：上一个 / 下一个 pane |
| `Ctrl-W Ctrl-W` | 转录区 / 输入框 normal 模式 1 秒内连按两次：聚焦下一个 pane，最后一个回绕到第一个 |
| `Ctrl-C` / `Ctrl-D` | 任意模式：保存草稿并退出 |

**`eix nvim` 前端（`:Eix` palette）**

| 键 | 行为 |
|---|---|
| `<leader>a s` | session picker |
| `<leader>a n` | 新建 session |
| `<leader>a f` | fork 当前 session |
| `<leader>a a` | attention 队列（下一个待审批/待输入） |
| `<leader>a p` | 命令面板（搜一切动作） |
| `<leader>a d` | diff 审阅 |
| `<leader>a e` | 用真 nvim 编辑当前 draft |
| `<leader>a x` | 取消当前生成 |
| `<leader>a r` | 审批收件箱（跨 session） |
| `<leader>a q` | detach |
| `<leader>a ?` | 帮助 / cheatsheet（打开后输入即筛选条目，`↑`/`↓` 选择，`Ctrl-U` 清空，`Esc` 关闭） |

**输入条（内置编辑器）**

| 键 | 行为 |
|---|---|
| `<CR>` | insert 提交（turn 运行中进入 prompt 队列）；normal 模式把当前 pane 的 session 提到第一位 |
| `/命令` | 整条消息作为 prompt 命令处理，不发给模型（见下表） |
| `<C-j>` / `<C-Enter>` / `<S-Enter>` | 换行（仅 insert） |
| `@` | 文件 picker（行首/空白后） |
| `<Space>s` / `/grep [query]` | 内容 picker：逐字防抖搜索 workspace，插入 `@path:line` |
| `Esc` | insert/visual → normal；normal 下 500ms 内再按一次：有 turn 在跑时中断它，否则回到转录区 |
| `:` | normal 模式：激活 statusline 下方的命令行；`Tab` 才显示匹配命令并补全；菜单中 `Ctrl-J`/`Ctrl-K`、`Ctrl-N`/`Ctrl-P` 上下选择，`Enter` 执行选中项；菜单打开时 `Esc` 先收起菜单并保留输入，再按一次关闭命令行，`↑`/`↓` 历史；`:!cmd` / `:r!cmd` 直接执行/读取 shell |
| `/` / `?` / `#` | normal 模式：在当前 session 对话中搜索；`/` 向前，`?`/`#` 向后；`n`/`N` 跳匹配，`Esc` 清除高亮 |
| `F1` / `/help` | 打开可搜索帮助；`/`、`?`、`#` 在 normal 模式打开命令行 |
| `Ctrl-J` / `Ctrl-K` | normal 模式下浏览已提交的 prompt 历史（newer / older） |
| `Ctrl-T` | 任意焦点：推理强度 low → medium → high → low（默认 medium） |
| `Ctrl-/` | 任意焦点：打开可搜索的模型 picker（`Ctrl-_` / `Ctrl-7` 也兼容） |
| `Ctrl-P` | 转录区 / 输入框 normal 模式：打开命令面板 |
| `Tab` | normal 模式下正向循环 Plan → Build → Auto → Bypass（命令面板的 `cycle execution mode` 同义） |
| `Shift-Tab` | normal 模式下反向循环 Build → Plan → Bypass → Auto |
| `j` / `k` | normal 模式下在草稿内上下行移动（`3j`、`dj`、方向键同样可用） |
| `<Space>n/W/o/f/s/g/p/c/a/x/m/v/t/r/l` | TUI 动作（见 5.1）；可用 `[keys]` 覆盖 |
| `i a I A o O x X D C s S r ~ J p P u C-r` | 常用 vim 编辑 |
| `d c y` + motion / `dd cc yy` / `iw aw` | 操作符 |
| `v V` + `d x y c p ~ o` | visual |

**prompt 命令**（输入框里以整条消息发送的 `/命令`；输入 `/` 时在输入框上方弹出补全菜单，
`↑↓`/`C-n`/`C-p` 选择、`Tab` 补全、`Enter` 执行）

| 命令 | 行为 |
|---|---|
| `/help [query]` | 可搜索帮助（命令 / 快捷键 / notes），可带初始查询 |
| `/keys [query]` | 只搜索快捷键章节 |
| `/new [name]` | 新建 session；聚焦 session 有 worktree 时复用它，否则共享其 cwd（不建 worktree） |
| `/new-pane` | session picker，把已有 session 放进新 pane |
| `/new-worktree [name]` | 把当前 session 移入独立 git worktree；已有 worktree 时只提示 |
| `/clear` | 清空模型上下文（事件日志与转录保留），追加 `ContextCleared` 事件 |
| `/compact [notes]` | 总结上下文；省略 from/to 时压缩 `message_range()`，`generate: true` 让模型写摘要，生成结果在 transcript 中展示 |
| `/btw <question>` | 一次性旁支问答，不写事件日志；`btw.delta` / `btw.completed` 通知，`btw.cancel` 中断 |
| `/cd [path]` | 切换 session cwd（`session.cd`，canonicalize 后校验在 worktree 内） |
| `/init` | 生成 AGENTS.md 初始化 prompt 放进输入框，供审阅后发送 |
| `/queue [clear]` | 查看/清空本 session 的待发送队列 |
| `/sessions` / `/fork` | 打开 session picker / fork 当前 session |
| `/files` / `/grep` / `/git` | picker；`/files` 插入 `@路径`，`/grep [query]` 搜索内容并插入 `@路径:行号`，`/git` 插入 worktree 路径或分支名（旧名 `/worktrees`、`/branches` 仍可用） |
| `/processes` / `/approvals` / `/attention` | 进程 / 审批收件箱 / 下一个 attention |
| `/mcp` / `/todos` | MCP server picker / 任务列表 picker（均可搜索和预览） |
| `/cancel` | 取消运行 |
| `/perm` | picker 搜索并选择 `plan` / `build` / `auto` / `bypass`；也可直接带 mode；与 `Space m` 相同；旧名 `/mode` 仍可用 |
| `/exit` / `/quit` | 保存草稿并退出整个 eix 客户端（不关闭 session pane） |
| `/close` | 保存草稿并关闭当前 session；如果它是最后一个 pane，同时退出 eix |

队列按 session 保存在 TUI 内存（`HashMap<SessionId, VecDeque<String>>`），tick 时空闲即取一条
发送；`turn.completed` 与 daemon `turns` 移除之间的竞态用“出错重排到队首、下个 tick 重试”处理。

**转录区**

转录按对话渲染：用户消息右对齐并包在圆角气泡框中，agent 输出左对齐并保留 2 空格缩进；
消息正文使用普通文本，不使用 `you>` / `eix>` 前缀。agent 的 reasoning（provider 的 `reasoning_content` / `reasoning`）
以 `Thinking` 模块流式展开，turn 结束后默认保持展开（可点击折叠）；工具调用按 `call_id` 合并
call/approval/patch/result 事件，摘要行显示状态圆点、命令或路径摘要，下一行显示状态
（`ok` / `running` / `needs approval` 等），展开后用四空格左缩进直接显示参数、输出和 diff；
diff 采用 Vim 风格左右分栏（左旧右新），按文件扩展名用 syntect 做语法高亮，多文件 patch 在
正文中保留文件名分隔，窄 pane 回退 unified。`write_file` / `edit` / `patch` 结束后默认保持
展开；审批卡片对这三类使用可用的完整 diff 高度，其他工具仍只显示短预览。命令与路径已由
折叠摘要给出，不再重复 `input` / `output` 标签；摘要因宽度截断时，展开后会在正文补回完整命令。
`thinking` 与工具块都可用鼠标点击折叠；排队中的 prompt 以 `⏳ queued` 行显示在转录末尾。
消息正文（用户消息与 agent 输出）经 `tui/markdown.rs` 渲染：pulldown-cmark 解析 CommonMark
与 GFM 表格/任务列表/删除线，转成保留样式的 `Line`；链接显示目标地址，图片显示
`🖼 alt (path)` 占位，`$...$` / `$$...$$` 数学公式：行内公式转成紧凑 Unicode，块级公式用
2D Unicode 网格排版（分数横线、根号、上下极限、矩阵、可伸缩括号），不引入 TeX 排版引擎。
鼠标按住左键拖拽可选择转录文本（反色高亮），松开后自动复制，并在右上角短暂显示
`Copied N chars`；优先调用
`wl-copy` / `xclip` / `xsel` / `pbcopy`，不可用时回退到 OSC 52。
`/colorscheme` 打开带实时预览的 colorscheme picker：上下移动立即重绘界面，`Enter` 应用，`Esc`
恢复打开前的配色；不再通过 `/colorscheme <name>` 直接切换。内置 preset 覆盖 GitHub、One、
Tokyo Night、Monokai、Mariana、Gruvbox、Catppuccin 与 Rosé Pine 的亮色/暗色版本。亮色主题下
模式徽标会保留色相并自动压暗，保证至少 3:1 的对比度。预设里的 `background` 会同时应用到转录区、
colorscheme picker 和其他浮层；默认配色和没有 `background` 的自定义配色继续使用终端背景。
输入框不再使用独立的 `panel` 填充，始终跟随主题 `background`（默认即终端背景）。
转录渲染按 pane 缓存已存储事件：只有 live tail 和当前可见窗口每帧重绘，长会话的滚动、
选择与流式输出不会反复解析整段历史；事件、宽度、主题或折叠状态变化时才重建缓存。
attach 时只取最近 1000 条事件；滚动到顶部时按 `before_seq` 加载更早的分页，合并后按新增行数
校正 scroll，避免视口跳动。

| 键 | 行为 |
|---|---|
| 鼠标左键 / 滚轮 | 聚焦 pane；拖拽选择转录文本并在松开时自动复制，右上角短暂显示 `Copied N chars`；点击 `Thinking` / 工具块折叠或展开；滚轮滚动转录 |
| `j/k`、`PgUp/PgDn`、`<C-u>`、`gg/G`、`<C-f>/<C-b>` | 滚动（`Ctrl-D` 保留给退出） |
| `/`、`n/N` | 搜索 |
| `za`、`zR/zM` | 折叠当前/全部 |
| `Tab` | 正向循环 Plan → Build → Auto → Bypass |
| `Shift-Tab` | 反向循环 Build → Plan → Bypass → Auto |
| `<CR>` | 提交输入（insert 模式） |
| `y` / `Y` / `v` | 复制当前块 / 整条消息 / 可视选择 |
| `gf` / `gx` | 打开路径 / URL |
| `i` | 聚焦输入（insert） |
| `<leader>a…`、`<C-g>` | 同全局 |

**Picker**

| 键 | 行为 |
|---|---|
| 输入 | 模糊过滤（fzf 语法：`'`、`^`、`$`、`!`、`|`） |
| `<C-j>/<C-k>` 或 `<C-n>/<C-p>` | 移动 |
| `<CR>` | 主动作（打开/应用/切换） |
| `Tab` | 多选 |
| `<C-v>` | 切换预览 |
| `<C-x>` | 删除/关闭 |
| `<C-r>` | 重命名 |
| `<C-f>` | fork |
| `<C-n>` | 新建 |
| `<C-y>` | 复制 |
| `<C-o>` | 在 nvim 中打开 |
| `status:` / `branch:` / `tag:` / `cwd:` | 字段过滤 |
| `Esc` | 关闭 |

## 8. Picker 规格

统一的 `查询 → 过滤 → 预览 → 动作` overlay，provider 可插拔：

- 规划：sessions / files(`@`) / grep（文件内容） / history / commands / models / git（worktrees + branches）/ diff hunks / context items。
- 已实现（2026-10-18，统一 picker 体系）：
  - TUI 用单一 `Picker`（`kind` + `query` + `selected` + `items`）承载所有 provider，左侧列表、右侧预览。
    session 的预览会随选中项通过 `picker.preview` 拉取最近对话（you / assistant / tool / error）；
    session picker 只列普通 session 和 worktree session，`origin = agent` 的子 session 不进入列表，
    只能从父 transcript 的 subagent 卡片打开；child transcript 底部提供可点击的
    `← parent agent`；worktree 标记 `⑂` 单独占一列，紧跟 session id 前；
    files 的预览按需读取文件内容并加行号，超过 64 KiB 或 400 行时截断，避免初始列表携带大 payload；
    grep 的预览直接使用匹配行（含 `path:line`），查询防抖后由 daemon 做大小写不敏感的字面量搜索。
  - 入口：`<Space>g`（sessions，切换 pane）、`<Space>h`（timeline）、`<Space>o`（sessions，新 pane）、
    `<Space>f`（files）、`<Space>s`（grep，文件内容）、`Ctrl-P`（commands，命令面板；转录区与输入框
    normal 模式都可用）、`<Space>G`（git，统一列出 worktrees 和 branches；没有 linked worktree 时
    省略仓库主路径）、prompt 内 `@`（files）、`/grep [query]`、`Eix pick` 经独立前端选择。
  - provider 由 daemon 的 `picker.query {kind}` 提供：`files`（workspace 相对路径）、`grep`
    （大小写不敏感的字面量内容搜索，返回 `path:line`，跳过二进制/派生目录/4 MiB 以上文件）、`git`
    （worktrees + branches，worktree 行在前，detail 标注 `worktree` / `branch`）、`worktrees`
    （`git worktree list --porcelain`）、`branches`（`git branch`）、`context`（值为 entry id）。
    默认 500、上限 5000；跳过 `.git` / `target` 等派生目录。
  - 动作：files → 在 prompt 光标处插入 `@path`；grep → 插入 `@path:line`；git → 插入 worktree 路径或分支名；sessions → 切换/新 pane；
    commands → 执行（session / files / grep / git /
    processes / approvals / new / fork / cancel / help / quit）。
  - 独立 nvim 前端：`:Eix pick [sessions|files|worktrees|branches|context]`（`sessions` 为默认），
    文件/分支/worktree 选中后插入光标处，context 选中后在 scratch 分屏显示条目详情。
- 仍待做：diff hunks provider、
  多选（多文件插入、多 session 开 pane、多 hunk 审批）、frecency 排序与 `#` 匹配度模式、外挂
  `agent pick`/`fzf`、file 语法高亮预览。
- 已补（2026-10-19）：`/model` 打开客户端生成的模型 picker（含 `id#variant` 行），
  `/agents` 打开 primary agent picker；两者都复用同一个 overlay，数据来自
  `model.list` / `agent.list`，选中后发 `session.model` / `session.agent`。
- 已补（2026-10-22）：`[providers.<name>]` 注册多个 OpenAI 兼容 provider；恰好一个
  设置 `default = true`，表名即模型选择前缀。`model.list` 并发拉取每个 provider 的
  `GET /models`，失败只降级为该 provider 的 warning，默认 provider 的 model /
  `[[models]]` 始终保留。健康结果缓存 5 分钟，有失败时 30 秒后重试，`/reload` 清空缓存。
- 模糊匹配用 `fuzzy-matcher`（skim 算法）；不做自研打分。

## 9. 审批与工作区

### 9.1 执行模型

- **propose → stage → apply**：agent 的写操作先进入 staging 视图（补丁/影子目录），用户批准后才落到真实工作区。
- agent 的读操作看到的是 `真实工作区 + 已 stage 改动` 的合并视图，避免"自己改了什么自己不知道"。
- shell 命令无法 stage：写操作走审批门禁 + 执行前快照（git stash 或目录快照），风险等级单独标注。
- 只读 shell 命令（`grep` / `rg` / `fd` / `find` / `cat` / `head` / `tail` / `wc` /
  `ls` / `stat` / `diff` / `uniq` / `tr` / `cut` / `sed -n` / 纯 `print`/`printf` 的
  `awk` / `git status` 等）由统一风险分析判为 `ExecSafe`，不弹审批、不抓快照。
  工作区内的 `cd` 也直接放行；离开工作区、动态目标或 `cd -` 仍询问，避免后续相对路径
  被误判为工作区内读取。`env` / `timeout` / `nice` / `command` 等包装器只按实际执行的
  命令判断，只有 `sudo` / `doas` 额外计入提权；`env` 设置 `PATH` / `LD_PRELOAD` /
  `GIT_*` 等可改变命令查找、动态加载或工具配置的变量时仍视为动态环境，普通 `FOO=1`
  不影响。`xargs` 仅在内层是 `wc` / `ls` / `stat` 等只报告名称、元数据或计数的命令时
  放行，内容读取器和 `-a` / `--arg-file` 保持询问；例外是下一段是 `wc` 的计数管道，
  `xargs cat` / `head` / `tail` / `grep` / `strings` / `od` 只把内容交给计数，不输出原文。

  `< file` 记为输入读取，`> file` / `>> file` 才记为写入。命令替换 `$(...)` / 反引号
  递归分析内部命令；内部安全时允许，但替换结果作为路径、脚本、`cd` 目标、`xargs`
  内层命令，或作为非纯输出命令的普通参数（可能变成 `git` / `date` / `grep` 的选项）
  时仍走动态目标审批。静态 `for` 循环（工作区内的字面量/通配符列表）会先展开再逐条
  分析，`n=$(...)` 等小写局部赋值按无副作用赋值处理，`printf -v` 只信任小写局部变量
  名；绝对外部路径、`..`、隐藏文件通配符、嵌套控制流、首段无字面前缀的通配符（如裸
  `*.rs`）以及循环体内的未知/写入命令仍询问。动态列表
  `for f in $(...)` 只在列表恰好是一个未加引号的命令替换、生成命令本身可放行，且循环体
  只把变量用于输出/测试或带字面相对前缀的路径（如 `"./$f"`）时展开；命令替换的分词
  可能产生以 `-` 或 `/` 开头的片段，因此 `cat "$f"` / `sort "$f"` 仍保持询问。生成命令
  只接受工作区路径列表（`find` 工作区路径、`git diff --name-only`、`git ls-files`、
  `git ls-tree -r --name-only`、`rg --files` / `-l`）和保持整行的 `grep` / `head` /
  `tail` / 简单 `sort` 过滤；写入、网络、secret、外部读取、未知生成命令、带引号/多词
  列表、循环体写入或把输出送入 `sh` / `xargs` 等执行器时仍询问。重定向、
  后台运行、子 shell、`find -exec` / `-delete` / `-files0-from`、`rg --pre`、`fd -x`、
  `sed -i`、`awk` 的 `system`/重定向、`date -s`、`ss -K`、`sort -o` / `-T` /
  `--files0-from`、`diff --paginate`、`xxd` / `uniq` 的第二个文件参数、`wc` / `du
  --files0-from` 以及所有未知命令仍按风险处理（unknown 默认 ask），避免被 shell 语法
  绕过。`jq` 读取 `env` / `$ENV` 视为 secret 读取并硬拒绝，`jq -f` / `--from-file` 等
  外部脚本保持询问。构建命令判为 `ExecSandboxed`，优先在沙箱内执行。

### 9.2 审批 UI

内联卡片（已实现，选项式 picker）：

```
⚠ run_command: cargo test                      [shell]
- [x] Approve  ★ recommended
- [ ] Review diff
- [ ] Open file
- [ ] Deny
- [ ] Deny with reason…
Ctrl-J/K or j/k select · Enter confirm · 1-9 jump · y/n · x cancel
```

- 卡片把动作排成 Markdown checkbox 列表（`- [ ]` / `- [x]`），`[x]` 是当前高亮项（单选），
  首个选项是推荐项；`Ctrl-J`/`Ctrl-K`（或 `j`/`k`、`C-n`/`C-p`）移动、
  `Enter` 执行、`1-9` 直选。卡片键盘在 prompt/transcript 聚焦以及 `tabby` 布局下都优先路由给
  卡片。`y`/`n` 仍是批准/拒绝快捷方式，
  `d`/`e`/`o`/`r` 也保留。
  shell 审批显示完整命令行（按卡片宽度换行），而不是 daemon 摘要里的截断版本。
- `e`：把 staged 的新内容写到临时文件，交给 `$VISUAL`/`$EDITOR`，保存后走
  `patch.revise` 重算 diff/hunks 再审批（工作区在此前不动）。
- `o`：在编辑器里打开工作区中的真实文件（仅查看/对照；写操作仍走审批）。
- `r`：输入拒绝理由，随 `approval.respond` 记录并回传给模型。
- 风险级别：`run_command` → `[shell]`，`write_file`/patch → `[workspace-write]`，
  `spawn_agent` → `[agent]`，只读工具 → `[read]`。
- `Space a` 打开跨 session 审批 inbox；`eix approvals [--json]` 是无头等价物。

全量 diff 视图（`d`）：

- 左右分栏：左侧旧文件、右侧新文件；行号与 `-`/`+` 标记分列，增删行带绿/红背景。
- syntect 按目标文件扩展名选择语法；深色/浅色主题分别使用对应的高亮配色。
- `[`/`]`（或 `J`/`K`）跳 hunk，`Space` 选中，`a` 全选/全不选。
- `y` 应用选中（未选中则全部），`n` 拒绝。
- 冲突检测：文件在 stage 后被外部修改 → 标红并要求重新 stage。

### 9.2.1 ask_user（已实现）

模型可调用 `ask_user {question, options}`（2-6 项，第一项是推荐项），用于需求歧义、
破坏性选择或无法推断的偏好。daemon 追加 `input_required` 事件、广播 `input.required`、
把 session 切到 `awaiting_input` 并阻塞 turn；TUI 在 pane 底部显示同样风格的选项卡片。

- 选项按 Markdown checkbox 渲染（`- [ ]` / `- [x]`，`[x]` 为当前高亮项）；
  按键：`Ctrl-J`/`Ctrl-K`（或 `j`/`k`、`C-n`/`C-p`）移动，`Enter` 确认，`1-9` 直选；
  `x` 取消整个 turn。pane 太矮时列表随光标滚动，保证高亮项可见。
- 用户选择后 daemon 追加 `input_answered`，把选项 label（有 description 时附在 `—` 后）
  作为 tool result 回给模型，并把状态切回 `running`。
- 协议：`input.respond {session_id, input_id?, option_index}`；空 `input_id` 表示当前问题。
  `eix answer N` 是无头等价物；`prompt.cancel` 会关闭等待中的问题。
- 子 agent 不能提问（没有 UI，会阻塞父 turn）。

### 9.3 权限策略

权限按四层处理，层与层之间没有隐式跳过：

1. **执行模式**：session 的 `plan` / `build` / `auto` / `bypass`，存于
   `sessions.mode`（旧库从 `(permission, agent)` 自动迁移，默认 `build`）。`session.create`
   可带 `mode`；`session.mode` 可在运行时切换，`temporary: true` 只存内存并在事件日志
   留下 `mode` 事件，daemon 重启后回到持久值。子 agent 永远按只读处理。
   - `plan`：硬只读。隐藏 `write_file` / `edit` / `patch` / `start_command`；运行时拒绝
     所有写入、危险执行、网络 MCP 和 `ExecUnknown`。保留 `run_command`，以便 `git status`
     / `cat` / `grep` 等只读检查；写重定向和 `rm` 仍会被拒绝。
   - `build`：默认模式，workspace-write，路径限制在 workspace 内。工作区内的低/中风险
     `write_file` / `edit` 默认直接应用（保留 snapshot 和 auto-patch 记录）；
     `[tools] auto_apply_workspace_edits = false` 可改回逐次 stage 审核。外部/secret 路径、
     高风险写入和 shell 写操作按风险、规则和模式默认值决定。
   - `auto`：workspace-write。未命中规则且风险默认为 `ask` 的调用交给意图分类器。
     分类器固定启用，始终使用 session 当前模型（没有 session model 时用默认模型），不暴露
     配置；内置 6 秒超时、最低置信度 `0.8`、最高自动放行风险 `high`、allow 缓存 10 分钟、
     上下文/参数上限 6000 字符。分类器只能把 `ask` 改成 `allow`，不能创建 `deny`；文件工具
     （`write_file` / `edit` / `patch`）和 secret / destructive / privilege / external
     write 类别不进入分类器。超时、非法 JSON、低置信度或无用户意图都保持 `ask`。它不会像
     `bypass` 一样解除外部路径限制。
   - `bypass`：近似 Claude Code 的 `bypassPermissions`：解除 workspace 路径限制、默认 ask
     直接放行；没有 sandbox 时构建命令也直接执行。仍然执行硬拒绝、secret 路径保护、`deny` /
     显式 `ask` 规则。
   - Agent 不再叠加能力：`AgentSpec` 只有 prompt / model / role，execution capability 只由
     session mode 决定。
2. **风险分析**：`eix-core/src/risk.rs` 对具体调用分类：`Read` / `SecretRead` / `WriteLocal` /
   `WriteExternal` / `ExecSafe` / `ExecUnknown` / `ExecSandboxed` / `Destructive` / `Network` /
   `GitPublish` / `GitRewrite` / `McpWrite` / `Privilege` / `SecretExfil`，并给出 low/medium/
   high/critical。shell tokenizer 拆 segment、重定向、wrapper 和嵌套 shell；只读路径、secret
   路径、构建命令、网络命令和 Git 写操作分别归类。不可恢复的灾难命令（`rm -rf /`、块设备
   `dd`、`mkfs`、关机、fork bomb）、私钥读取、网络/命令/MCP 中的 secret 占位符进入
   `hard_deny`。
3. **规则**：`[[tools.rules]]`，字段 `id` / `tool` / `path` / `command` / `command_regex` /
   `signature` / `action = allow|ask|deny|sandbox`。`deny` 先全局扫描，任意来源的 deny 都
   胜出；其余规则按具体度选择，平局按 `Session > User > Project > Builtin`。`command` 是
   argv token 前缀；`deny` 在任一 shell segment 命中时生效（`echo hi && rm -rf build` 命中
   `command = ["rm"]`），非 deny 规则要求所有 segment 都满足，且带重定向的命令不会命中
   `command` 前缀，避免 `cargo test && rm` 或 `echo hi > /etc/passwd` 被 allow。
4. **模式默认值 / sandbox**：`Mode` 只决定未命中规则的默认 ask 如何处理：`build` 询问；
   `auto` 先交给分类器，分类器低置信度 / 超时 / 非法输出仍询问；`bypass` 直接放行。分类器
   输入包含最近用户意图、工具名与参数、静态风险等级 / categories / reasons、workspace；
   输出严格 JSON `{decision, confidence, reason}`，只接受 `allow` 且 confidence
   ≥ `0.8`，其余全部按 `ask`。相同 session + 意图 + 参数 + 模型 + 风险 + categories
   的 allow 判决复用 600 秒；key 包含 session、workspace、意图、工具、参数、
   模型、风险等级和 categories。参数超出 6000 字符时不分类，保持询问。
   `auto` 仍然 workspace-write；`bypass` 另外允许
   外部路径写入和无 sandbox 的构建命令。三者都不绕过硬拒绝、显式 deny、能力上限或“需要沙箱
   但沙箱不可用”（`bypass` 的 unsandboxed fallback 除外）。`sandbox = "auto" | "required" |
   "off"` 只决定在哪执行：构建命令默认 sandbox；bwrap 不可用时 auto 降级为强制审批，
   required 直接阻断。

项目层 `.eix/config.yaml` 只能收紧：`sandbox = "off"`、
`sandbox_network = true`、`external_directory`、`network_allow`、额外
sandbox 只读/可写路径会被忽略；项目 `allow` / `sandbox` 规则降级为 `ask`；
`snapshots = false` 被忽略。
分类器使用固定内置策略，不参与项目配置。`[compations]` 默认全关，
项目层只能关闭、不能开启其他 agent 的资源加载；所有拒绝都写入 `Config::security_warnings`。

审批卡片显示风险等级、原因、解析后的命令、命中规则及来源、沙箱状态和可用 scope。
`HIGH` / `CRITICAL` 不提供 `Always`，服务端也会拒绝将其升级为持久规则；`Once` 不记忆，
`Session` 只存在内存，`User` 追加到用户配置。每次决策写 `permission_evaluated` 审计事件
（action、risk、categories、reasons、rule、rule_source、sandbox；`auto` 分类器还会带
`classifier` 的 decision / confidence / reason / model / cached / latency），网络 URL query 脱敏。

`webfetch` / `websearch` 归入 network 风险，但属于只读 GET：默认 allow；配置非空
`network_allow` 后只 allow 命中域名。已注册的 MCP `get_*` / `list_*` / `read_*` / `search_*`
类工具也按只读放行，其余 MCP 工具仍 ask。secret 占位符只对本地文件工具还原；shell、
网络和 MCP 参数中的占位符会触发 `secret-exfil` 硬拒绝。

## 10. 自动上下文管理

context ledger 是 daemon 内部机制，不提供用户命令或全屏编辑界面；它按 sector 分组：

| sector | 内容 | 预算 |
|---|---|---|
| system | 系统提示 | 5% |
| instructions | AGENTS.md（仓库根 → cwd）、项目约定 | 5% |
| workspace | 文件树、git 状态、选中文件 | 15% |
| history | 对话历史 | 60% |
| tools | 工具输出（可截断/引用 blob） | 10% |
| memory | 跨 session 记忆 + 当前任务列表 | 5% |

- 条目的 token 成本只用于内部预算和自动裁剪。
- 工具结果先按 `tools.max_output_bytes` 截断，再按 `context.tool_output_tokens` 对 provider
  视图做 head/tail 省略；完整输出保留在事件日志。
- `tools` / `history` 超预算时保留最新 4 条；若总预算仍超，emergency trim 只保护当前 user
  消息和最新工具结果，避免一轮大输出让后续请求永久失败。
- 压缩是**显式事件**：历史里出现 `compacted 12 messages → summary`，可展开查看原文。
- 自动压缩在每个工具批次后也会检查；provider 报 context-length / 413 时强制压缩并重试。
  压缩只由上下文压力触发，不按轮数定期执行。保留窗口优先按
  `auto_compact_keep_tokens`（默认 8000）计算，`auto_compact_keep_turns` 只作为最少轮数
  下限；裁剪点只落在 user turn 边界。自动压缩是滚动摘要：新摘要会带上旧摘要继续更新，
  旧摘要不再重复发送；摘要使用当前 session 模型。
- provider 重试按错误类型分派：瞬时故障使用抖动指数退避（默认 8 次、250ms 起）；429 优先
  `Retry-After`，没有时至少 500ms 起步；quota/auth/config 直接失败。`turn.retrying` 带
  `kind`，TUI 用简短标签显示原因。
- 删除/pin 是 session 级策略，可保存为项目默认。
- 系统提示用 Working agreement + Work loop 组织。Working agreement 约定协作方式：简洁直接、
  先读代码再假设、按显式要求验收、端到端完成且不把实现请求降级为计划、保留用户改动、
  不修无关问题；单条命令能答的简单请求直接执行，只有错误假设会影响结果时才问。
  Work loop 压缩为 orient → plan → change → verify → report：非平凡任务才计划，跳过最简单
  四分之一、不做单步计划、每完成一项就更新；改动修根因、复用 helper、保持相关调用面一致；
  用可失败的最强检查覆盖边界/错误路径并检查 diff，不为通过而改测试；结束前交代结果、证据、
  假设与残余风险，卡住时说明缺什么，重复读写无进展时停下。Communication 段约束分组工具
  调用前的简短说明、review 先列 findings、最终答复简洁且不倾倒文件内容；Continuity 段规定
  新消息以最新为准，resume/压缩后从摘要继续而不是重来。
  工具批次按只读段并发：内置读工具和风险分析判定低风险只读、权限放行的 `run_command`
  可以在段内同时执行；无需审批且目标文件互不重叠的直接 `write_file` / `edit` 也可在
  写段内并发。同一/重叠目标、`patch`、需要审批或暂存的写入、非只读命令、审批和交互
  调用作为屏障，写后的读取仍在写入完成后执行，避免读到半应用状态。快照创建/裁剪串行
  化以保护回滚目录，但实际文件写入仍可并发。结果始终按调用顺序回填。
  内置工具描述保持短句；`tests` 用 token 上限防止提示词和 schema 重新膨胀。

## 11. 事件与协议（草案）

**daemon → client（notification）**

- `session.created|updated|status`、`session.attention`
- `integration.updated`、`integration.merged`、`integration.attention`
- `turn.started`、`message.delta`、`reasoning.delta`、`message.reset`、`turn.retrying`、`message.completed`
- `tool.requested`、`tool.approval_required`、`input.required`、`tool.started`、`tool.output`、`tool.completed`
- `patch.proposed`、`patch.applied`、`patch.rejected`、`patch.conflict`
- `process.started|output|exited`
- `context.updated`、`context.compacted`
- 广播可按连接收窄（`session.subscribe`）；`session.updated` 用 `id` 判定归属，其余事件用
  `session_id`，两者都没有的事件（如 `stream.lagged`）发给所有订阅者。

**client → daemon（request）**

- `hello`（版本握手）
- `auth {token}`（仅 TCP：必须是第一个请求）
- `session.list|create|attach|detach|rename|archive|fork|rewind`
- `session.worktree.create {session_id, name?}`
- `session.worktree.remove {session_id, force?}`
- `session.ready {session_id, auto_commit?}`
- `integration.list {session_id?}`、`integration.continue {session_id}`
- `session.subscribe {session_ids?}`（连接级广播过滤；`null` = 全部）
- `prompt.submit|cancel|queue`
- `approval.respond|list`、`input.respond`、`patch.apply|reject|get|revise`
- `draft.get|set`（按 session 的未发送输入；空文本删除）
- `context.compact|clear`（自动压缩与 `/compact` / `/clear` 内部使用）
- `process.list|output|kill`
- `picker.query {kind, session_id?, query?, limit?}`（provider：`files` / `grep` / `git` / `worktrees` / `branches` / `context`；
  返回 `{value, label, detail?, preview?}` 行；`grep` 的 value 是 `path:line`；`context` 的 value 是 entry id）
- `picker.preview {kind, session_id?, value, limit?}`（`files` 读取 workspace 文件内容；`sessions` 读取目标
  session 的最近对话事件；返回 `{kind, value, text}`，供 picker 右侧懒加载）
- `nvim.claim`（输入所有权；planned）

所有事件带 `id`、`session_id`、`seq`、`ts`、`type`、`payload`；协议版本握手。

**传输**：默认 Unix socket（0600）。`[server] tcp = "host:port"` 同时监听 TCP，此时 `token` 必填；
TCP client 的首个请求必须是 `auth {token}`，通过前不返回数据/事件，失败返回 `unauthorized` 并断开。
`[server] http = "host:port"` 另开一个轻量 HTTP surface：`GET /health`（免认证）、`GET /stats`、
`POST /rpc`（Bearer token，单请求单响应）、`GET /events`（SSE，Bearer token，可用
`?session_id=` 重复/逗号分隔过滤，无 session 的全局 notification 仍送达）。HTTP 与 TCP 共用
`server.token`（配置了 HTTP/TCP 就必须非空），body 上限 8 MB；所有响应和错误带 CORS 头，
`OPTIONS` 预检免认证。SSE 直接订阅 `bus`，因此与 Unix/TCP 的广播语义一致；`eix-sdk` 提供
类型化 Rust client 和 notification stream。Unix 连接视为已认证。CLI/TUI 用 `--tcp` / `--token`（或 `EIX_TCP` / `EIX_TCP_TOKEN`）选择端点；
TCP/HTTP 端点不会自动拉起本地 daemon。

## 12. 持久化

- SQLite（WAL）：`sessions`、`events`、`messages`、`drafts`、`approvals`、`patches`、`frecency`、`plugin_kv`。
- 大输出/补丁走内容寻址 blob；prompt 图片走 `<data>/blobs/<sha256 前两位>/<sha256>`（自动去重、保留/GC 策略见上文）。
- 文件权限：目录 0700、文件 0600；日志写入前做密钥脱敏（API key、token、`.env` 内容）。
- 事件日志天然是回放测试夹具：record/replay 用于集成测试。

## 13. 通知

- TUI：只有没有其他 UI 反馈的失败、复制确认、布局切换、后台 attention 和 `/reload` 完成
  显示在右上角浮动通知中（失败 / attention 约 3 秒，复制 / 布局 / `/reload` 约 1.5 秒；
  多条同时存在时逐条堆叠，最新消息立即显示，不设置最低等待时长），不会移动 statusline；
  成功的命令反馈（`/reload` 除外）、进度和 picker/弹窗内容不重复通知；可选终端 bell，
  不弹桌面通知。
- 桌面：`notify-send`（dunst），可通过配置替换命令。
- dwmblocks：`agent status --json` 输出 `{running, approval, error}`。
- 仅在状态跃迁时通知；当前聚焦 session 不通知；冷却时间可配。

## 14. 安全

- 工具沙箱：bubblewrap。工作区 rw，`/tmp` tmpfs，不挂载整个 home，缓存目录按需只读挂载，网络开关。
- 环境变量白名单；密钥不进入 daemon 日志。
- 发往 provider 的消息在 `Provider::stream_chat` 边界做可逆脱敏：密码、API key、token、
  Bearer、URL 凭据、PEM 私钥和敏感赋值替换为 per-provider salt 哈希出的占位符；模型返回的
  tool-call 参数和文本在本地执行/持久化前还原。映射不离开进程，事件日志和 TUI 保留原文。
  图片不做文本脱敏。
- Web GET 默认放行；被 allowlist 限制的网络请求和破坏性命令按风险审批；只读 shell 命令的风险分类见 §9.1。
- 配置文件与日志权限收紧；不把 API key 写进项目目录。

## 15. 风险

1. **完整 nvim 配置的插件干扰**（浮动窗口裁剪、通知弹窗、状态栏）——最高风险，需要 doctor + minimal 回退。
2. **linegrid 渲染器**工程量与边界情况。
3. **按键路由/键盘协议差异**（`S-CR`、`C-j`、IME、bracketed paste）。
4. **中文 FTS** 的短词问题。
5. **staging 工作区**与 agent 读视图的一致性。
6. **上下文质量**决定产品上限，UI 只决定下限。
7. **rewind 的诚实性**，一次说谎就失去信任。
8. **范围膨胀**：UI 复杂度吃掉内核打磨时间。

## 16. Roadmap

> 本节保留阶段计划与当时的实现快照，用于追溯设计决策；当前完整实现状态见 §20。

**Spike（2–3 天）**

- nvim `--embed` + 完整配置 + linegrid 渲染 + 按键转发。
- `<CR>` 提交守卫（`pumvisible()` / `getcmdtype()`）。
- `:Agent submit` / draft 同步 / 主题桥接。
- 验收：中文输入、leader、补全浮窗、Enter 提交、C-j 换行均正常。

**Phase 0（1–2 周）**

- daemon + SQLite + OpenAI 兼容 provider 流式 + 单 session + 转录渲染 + 持久化 + attach/detach。
- 验收：完成 20 轮真实编码对话；关终端再 attach，对话与 draft 都在。

> 实现快照（2026-10-18）：daemon（`eixd`）、SQLite 事件日志、OpenAI 兼容流式 provider、
> CLI（`eix chat/send/log/ls/new/rename/cancel/doctor`）和 auto-start 已落地。
> 未命名 session 在两轮 user turn 完成后由 daemon 调模型生成短标题；标题保留 Unicode
> 字母/数字和英文词间空格，截到 32 字符（含空格），其他标点会被去掉；手动重命名优先。
> 实际协议以 `crates/eix-core/src/protocol.rs` 为准：Phase 0 的事件通知按连接广播
> （0600 socket 是边界），`detach` 等同于断开连接；转录渲染暂由 CLI 完成，TUI 与工具审批尚未开始。

**Phase 1**

- 多 session、picker、pane 分屏、attention 队列、通知、dwm/tmux 集成。

> 实现快照（2026-10-18）：多 pane TUI 已落地（`eix` 无命令默认打开，或 `eix tui`）。左侧 session 栏 + 右侧自动
> 平铺 pane（1/2/3/4+ 分别全屏、左右、左大右二、网格），每个 pane 独立维护 session、
> 转录、滚动与流式跟随；`<Space>o` 在新 pane 打开，`Ctrl-G` 切换聚焦 pane 的 session，
> `Shift+,` / `Shift+.` 切换 pane、`<Space>n` 新建 session（有 worktree 则复用）、`<Space>c` 关闭；同一 session 只出现一次，
> picker 选中已打开 session 会聚焦既有 pane。输入框是自研的 Vim 风格模态编辑器
> （`tui/editor.rs`）：normal / insert / visual / visual-line，常用 motion + operator +
> count、`u`/`Ctrl-r`、未命名寄存器；不启动子进程、不加载 nvim 配置，按键零往返。
> `<CR>` 只在 insert 提交，`<C-j>`/`Shift-Enter` 换行；输入框默认 3 行内容（加边框共 5 行），
> 长草稿最多增长到 10 行内容（总 12 行）后在框内滚动，提交失败会把 draft 放回 buffer。
> 后台 pane 的 attention 已接入：daemon 在进入 `awaiting_approval` / `error` 时发送
> `session.attention`，TUI 标记边框/侧栏并支持 `<Space>A` 跳到下一个；重新 attach 会从事件日志
> 恢复未完成的审批卡片。外部通知也已接入：`[notifications] command` 由 daemon 在 attention
> 时用 `sh -c` 执行并传入 `EIX_SESSION_ID` / `EIX_SESSION_NAME` / `EIX_SESSION_CWD` /
> `EIX_STATUS`，同一 session 有 `cooldown_secs` 冷却；TUI 内提醒显示在右上角浮动通知中，
> 不挤动 statusline，可用 `bell = true` 额外响终端铃。
> 输入框补齐（2026-10-18）：`@`（词首）打开 `picker.query` 的 files provider，选中后以
> `@<相对路径>` 插入；编辑变化按 200ms tick 镜像到 `draft.set`，切 session 时 `draft.get` 回填。
> 独立前端补上 `<C-g>` session picker、`<C-g><C-g>` 回上一个 session、`:Eix pick [KIND]` 与 `:Eix doctor`。
> 统一 picker（2026-10-18）：TUI 用单一 overlay 承载 sessions / files / worktrees / branches /
> context / commands，左侧列表右侧预览；`Ctrl-G`/`<Space>o`/`Ctrl-P`/`<Space>w`/`<Space>b`/`@` 为入口，files 插入 `@path`，
> worktrees/branches 插入值，commands 与 prompt 的 `/` 菜单共用同一份 built-in / 自定义命令表，按 `/命令` 执行。daemon `picker.query` 新增
> `worktrees`（porcelain）、`branches`（`git branch`）两个 provider；
> 独立前端 `:Eix pick files|worktrees|branches|context` 用 `vim.ui.select` 选择。
> 合并 git picker（2026-10-25）：TUI 的 `<Space>w` / `<Space>b` 合并为 `<Space>g`，一次列出
> worktrees 和 branches；daemon 新增 `git` provider，旧的两个 provider 继续供独立前端和兼容使用。
> 内容 picker（2026-10-26）：daemon 新增 `grep` provider，按 case-insensitive 字面量搜索
> workspace 文件内容并返回 `path:line`；TUI 用 `<Space>s` / `/grep [query]` 打开，查询按 tick
> 防抖，Enter 插入 `@path:line`。搜索跳过派生目录、二进制文件和大于 4 MiB 的文件。
> 快捷键调整（2026-10-26）：TUI 的 sessions 从 `Ctrl-G` 移到 `<Space>g`，timeline 从
> `Ctrl-H` / `Ctrl-Backspace` 移到 `<Space>h`；git picker 从 `<Space>g` 移到 `<Space>G`。
> 输入焦点（2026-10-18）：`Esc` 只在 insert/visual 下回到 normal 并保留焦点，500ms 内
> 再按一次才回到转录；`<CR>` 只在 insert 提交；`<C-g>` 在任意模式打开 picker；
> `Ctrl-J` / `Ctrl-Enter` / `Shift-Enter` 只在 insert 换行；`F1` 全局帮助；
> `Ctrl-C` / `Ctrl-D` 在任意模式保存草稿并退出整个 TUI。
> 布局（2026-10-18）：移除左侧常驻 session 栏，pane 占满 body；历史 session 只经浮动
> picker（`Ctrl-G`）打开。normal 模式新增 `<Space>xx` leader 动作
> （复用命令面板动作），单键编辑命令全部留给编辑器。

**Phase 2**

- 工具执行、staging、审批、diff hunk、沙箱、自动上下文管理、process view。

> 实现快照（2026-10-18，进行中）：工具调用与审批门禁已落地。daemon 暴露
> `read_file` / `list_dir`（只读，自动放行）和 `write_file` / `run_command`（需审批），
> 所有路径经 workspace 沙箱（拒绝 `..` 与 symlink 逃逸），输出按字节截断，
> `run_command` 带超时。provider 侧支持 OpenAI `tools` / `tool_calls` 的流式增量累积，
> 一次 turn 不设固定工具往返上限：模型返回不含工具调用的回答即结束，用户可随时中断；
> 重复相同的工具批次会被 doomloop 守卫提前终止。每轮写入
> `tool_call` / `approval_required` / `tool_result` 事件并广播
> `tool.approval_required` / `tool.started` / `tool.completed`。TUI 渲染工具卡片，
> 聚焦 pane 有 `[y]/[n]` 审批卡片，`eix approve/deny` 可无头处理（`--hunks 0,2` 只应用
> 选中的 hunk）。
> 策略：`SessionMode`（`plan` / `build` / `auto` / `bypass`）加 `[[tools.rules]]`；低/中风险
> `write_file` / `edit` 在 workspace-write 模式（`build` / `auto`）下默认直接应用，
> `auto_apply_workspace_edits = false` 时改为
> stage 并生成 unified diff 和逐 hunk 预览、随 `patch_proposed` 事件广播，TUI 卡片与
> `d` 全屏审阅将 unified diff 转成左旧右新的 side-by-side 视图（syntax highlight，窄宽回退
> unified），CLI 保留 unified 输出。高风险/forced ask 写入不 stage，直接弹出带风险元数据的审批
> 卡片。`d` 审阅里 `[`/`]`（或 `J`/`K`）跳 hunk、`Space` 勾选、`a` 全选/
> 全不选、`y` 只应用选中的 hunk（全选时整文件），`n` 拒绝；`patch.apply` / `approval.respond`
> 接受可选 `hunks` 索引列表。应用时用基准内容拼出结果再落盘，并重新校验文件内容：
> stage 后被外部修改则记 conflict、不覆盖。
> `run_command` 默认用 bubblewrap 隔离（`[tools] sandbox = auto|required|off`，
> `sandbox_network` 控制是否 `--unshare-net`，`sandbox_read_paths` 逐个只读挂载工具链缓存，
> `sandbox_writable_roots` 额外开放目录）：系统只读、仅 session workspace 可写、`/tmp`
> 为私有 tmpfs，默认不挂载整个 `$HOME`。bwrap 缺失时 `auto` 把需要沙箱的命令降级为强制
> 审批，`required` 直接阻断。
> 执行前快照已落地：`write_file` 复制目标文件（含“原本不存在”这一事实），`run_command`
> 复制整个 workspace；排除 `.git` / `target` / `node_modules` / `.venv` / `dist` 等派生目录，
> 单文件 8 MB、总量 200 MB、条目 5 万上限，跳过数与失败都记入 `snapshot` 事件（失败时工具
> 输出也带一行说明），每个 session 保留 20 份。`Snapshotter::restore` 已实现并有测试覆盖，
> 供 Phase 3 的 rewind 调用；事件日志只存索引，快照字节在 `<data>/snapshots/<session>/<id>/`。
> `[tools] snapshots = false` 可在大仓库里关闭。
> Staged shadow workspace 已落地：关闭 `auto_apply_workspace_edits` 后，低/中风险 `write_file` /
> `edit` 先把提案写入
> `<data>/staging/<session>/<patch_id>.json` 并立即返回 `staged ...`，turn 不阻塞；
> `read_file` / `list_dir` 看到“真实工作区 + staged”合并视图，同一文件再次写入以最新
> staged 内容为基准，可以叠加。`patch.proposed|applied|rejected|conflict` 事件与同名通知
> 已接入，`patch.apply` / `patch.reject`（以及 `approval.respond` + patch id）负责落盘或
> 丢弃；落盘前重新校验基准，stage 后被外部修改则记 `patch_conflict` 且不覆盖。单文件快照
> 改为应用补丁时抓取。pending 补丁跨 daemon 重启恢复；TUI 卡片、attention、`eix approve` /
> `eix deny`、`eix log` 均已支持补丁。`run_command` 无法 stage，仍在审批处阻塞。
> Context ledger 是内部机制：`eixd/src/context.rs` 从事件日志重建 provider 上下文并按 sector
> 记账（system 5% / instructions 5% / workspace 15% / history 60% / tools 10% / memory 5%），
> 不向普通用户暴露 ledger 或手动 drop / pin 命令。
> `AGENTS.md`（仓库根 → cwd 与 config 目录）注入 `instructions` sector，workspace summary 注入顶层
> 文件树与 git branch/status。上下文压缩落成 `context_compacted` 事件，重启后重放一致；
> `tools` / `history` 超预算时从最旧条目自动裁剪（保留最新 4 条，必要时 emergency trim 到最新
> 一条），system / instructions / workspace / memory 不自动裁剪。`memory` 工具把 workspace
> 记忆写入 `<data>/memory/<hash>.md`，每次重建上下文都注入 `memory` sector；当前 todo 也作为
> 固定任务状态注入。自动压缩在 turn 开始和每个工具批次后运行；provider 返回 context-length
> 错误时强制压缩并重试。工具批次按只读段和“目标不重叠的直接写入”段并发执行（含已放行
> 的低风险只读命令），同一/重叠目标、`patch`、审批/暂存写入、非只读命令、审批和交互调用
> 作为屏障，结果保持调用顺序。
> Process view 已落地：新增 `start_command {command, cwd?}`（与 `run_command` 共用
> `command_plan`，同一沙箱与审批门禁）在 daemon 内启动后台进程并立即返回 id，以及只读的
> `process_output {process_id?, max_bytes?}`（缺省取本 session 最新进程）。进程归属 daemon、
> 按 session 分组，关闭前端不影响；daemon 退出时 `kill_all` 对整个进程组 SIGTERM，2 秒后
> SIGKILL。输出流式广播 `process.output` 并镜像到 `<data>/processes/<session>/<id>.log`
> （0600，文件 16 MB 上限），内存保留 128 KB 尾部；事件日志只写
> `process_started|process_exited`，避免刷爆，广播 8 MB 后停止推送但尾部继续更新。
> `start_command` 不抓执行前快照：长跑进程的改动不是一个可回滚的单元。
> `process.list|output|kill` 请求与 `process.started|exited` 通知已接入；TUI `P` 打开全屏
> 列表（`j/k` 选择、`x` kill、`r` 刷新、`PgUp/PgDn` 滚动、Esc 关闭），列表下方实时跟随选中
> 进程输出；CLI 为 `eix ps`、`eix tail <id>`、`eix kill <id>`，`eix log` 以 `proc>` 行展示
> 进程事件。限制：只管理 daemon 的直接子进程组，daemon 重启后 registry 为空（事件日志里可能
> 出现有 start 无 exit）。

**Phase 3**

- rewind/fork、worktree、子 agent、nvim 前端、远程 attach。

> Fork / rewind 已落地（2026-10-18）：`session.fork {session_id, at_seq?, name?}` 把源
> session 的 `seq < at_seq`（`None` 表示全部）事件复制到新 session，序号原样保留，因此
> 上下文重建结果与源前缀一致；新 session 为 `idle`，复制 `cwd` / 模型 pin，并记录
> `parent_id` + `forked_from_seq`（独占切点）。默认命名去掉已有的数值 `~N` 后取
> `name~2`、`name~3`…；`sessions` 表用 `PRAGMA table_info` 做幂等加列迁移。原 session
> 完全不变。TUI `F` 在末尾 fork，CLI `eix fork --at` 可在选中消息之前 fork；
> 新 session 自动在新 pane attach。CLI 为 `eix fork [--session ID] [--at SEQ] [--name NAME]`。
> **真正的 rewind**：`session.rewind {session_id, at_seq?, name?}` 先 fork，再把
> `seq >= at_seq` 的快照按 seq 倒序（最新先撤）交给 `Snapshotter::restore`；busy session
> 直接拒绝。返回 `RewindResult { restored, snapshots, files_restored, files_removed,
> files_missing, warnings }`，`restored = failed_restores == 0 && files_missing == 0`；
> 没有快照 / 快照关闭 / 被 prune / 恢复失败 / 文件缺失都会进 `warnings`，并提示“eix 之外的
> 改动无法察觉”。CLI `eix rewind` 走这条路径，纯 fork 仍标 `files unchanged`。
>
> Worktree 已落地（2026-10-18，2026-10-24 扩展）：`session.create` / `session.fork` 接受
> `worktree: bool?`。`create` 的 `None` 现在表示“首条 prompt 时自动建英文名 worktree”，
> `Some(true)` 立即建，`Some(false)` 共享；fork 的 `None` 仍在 repo 内自动建。
> `eixd/src/worktree.rs` 用 `git rev-parse --git-common-dir` 解析主仓库（支持 subdir / linked
> worktree，canonicalize 后比较），`git worktree add -b eix/<slug> .worktrees/<slug>`；slug
> 来自 session 名，冲突时加 `-N`。worktree 路径存 `sessions.worktree`，fork 未指定新
> worktree 时继承源值。`session.worktree.create {session_id, name?}` 把已有 session
> 移入新 worktree（TUI `/new-worktree`，已有 worktree 时返回 `invalid_params`）；
> `eix worktree list` / `remove SESSION [--force]` 对
> `session.worktree.remove`：先确定主仓库再 `git worktree remove [--force]`，成功后
> `set_session_cwd(repo_root)` 并清空 worktree；dirty 且没 `--force` 时返回 `removed:false`。
> `.worktrees` 已在快照排除表里，避免把 worktree 自身复制进快照。
>
> 子 agent 已落地（2026-10-18）：模型工具 `spawn_agent {label, prompt}` 由 daemon 建 child
> session（`origin = agent`、`parent_id`、名字 `parent/<label>-N`、cwd 继承），只读工具集
> （`read_file` / `list_dir` / `grep` / `glob` / `lsp_diagnostics` / `process_output` 等）；
> agent session 在 `run_tool_call` 里对写工具
> 直接拒绝，深度上限 1（child 不能再 spawn）。同一 assistant 批次里的多个 `spawn_agent`
> 用 `tokio::spawn` 并发跑，默认上限由 `[tools] max_parallel_subagents` 配置（默认 4，支持
> 2/4/8/16/32/64/128/256）；TUI `/subagents [n]` 通过 `config.subagents` RPC 覆盖运行时值（不写回配置）。
> 同一批次里连续的只读调用（内置读工具，以及风险分析判定低风险只读、权限决定为 allow 的
> `run_command`）在段内并发执行；无需审批且目标互不重叠的直接 `write_file` / `edit` 也会
> 在写段内并发。同一/重叠目标、`patch`、审批或暂存写入、非只读命令、审批和交互调用作为
> 屏障，结果按调用顺序回填；写段结束后按 server 合并收集一次 LSP 诊断，避免逐文件等待。
> 纯只读 + `spawn_agent` 批次会先启动 child，再与读取并行；含写入或不可证明只读调用的批次
> 会在最后一个此类调用完成后启动 child，并与尾随只读调用重叠，避免 child 观察半写入状态。child 使用精简
> 只读 system prompt，而不是重复完整父 prompt；同时复用父 turn 的 instructions/workspace
> 快照（与父 turn 每轮所用快照一致，文件读取仍读实时内容），避免大规模 fan-out 重复
> walk/git。child 的完整 turn 写进自己的事件日志，失败作为
> 普通 tool 结果返回，不打断父 turn。TUI 在父 transcript 里用 subagent 卡片显示；
> 点击卡片会在新 pane 打开 child session；child transcript 底部有可点击的
> `← parent agent`，点击回到父 agent。
>
> 独立 nvim 前端已落地（2026-10-18）：`eix nvim [--session ID]` 把内嵌 Lua 插件写到
> `<data>/eix-nvim.lua`，用 `EIX_NVIM_SOCKET` / `EIX_NVIM_TCP` / `EIX_NVIM_TOKEN` 连 daemon，
> 转录渲染进 `eix://<id8>` scratch buffer；`:Eix attach|list|new|send|prompt|fork|rewind|stop|ps`
> 覆盖主要操作。插件里所有 buffer API 都经 `vim.schedule`（libuv callback 是 fast context），
> 环境值在加载时缓存。headless e2e 在 `nvim --headless -u NONE` 下验证 prompt/回答出现在
> buffer；`#[tokio::test]` 的 current-thread runtime 需要 `spawn_blocking` 跑子进程。
>
> 远程 attach 已落地（2026-10-18）：`[server] tcp` / `token`（env `EIX_TCP` / `EIX_TCP_TOKEN`），
> 设置 `tcp` 时 token 必填（配置校验拒绝空值）。daemon 用泛型 `serve<S: AsyncRead + AsyncWrite>`
> 同时处理 Unix 和 TCP；TCP 连接首请求必须是 `auth {token}`，`secret_eq` 做常数时间比较，
> 失败返回 `unauthorized` 并断开，认证前不广播任何事件。Unix 保持免认证。CLI/TUI 的
> `--tcp` / `--token` 选择 `Endpoint::Tcp`，`ensure_daemon` 对 TCP 端点不自动拉起本地 daemon。
> 广播过滤已落地（2026-10-18）：连接可发 `session.subscribe {session_ids?}` 把广播收窄到
> 指定 session（`null` 恢复全部）；`session.updated` 用 `id`、其余事件用 `session_id` 判定归属，
> 无归属的事件（如 `stream.lagged`）放行。默认（不订阅）仍收到所有 session；`eix nvim`
> attach 后自动只订阅当前 session。
>
>
> **P0 补齐（2026-10-19）**
>
> 工具：`grep {pattern, path?, include?, literal?, caseSensitive?, limit?}`、
> `glob {pattern, path?, hidden?, limit?}`、`edit {path, oldString, newString, replaceAll?}`
> 和 `patch {patchText}` 已落地。`read_file` 增加 `offset`/`limit` 且输出带行号。
> `grep` / `glob` 只读自动放行；`edit` 与 `write_file` 在 workspace-write 模式下默认直接应用，
> 关闭 `auto_apply_workspace_edits` 时共用 staged shadow workspace（唯一匹配校验、生成
> hunks）；`patch` 走阻塞审批，解析 `*** Begin Patch` 的
> Add/Update/Delete，先校验全部文件再写入，并抓工作区树快照。`grep` / `glob` 目前看不到
> staged 内容（MVP 限制）。
>
> 扩展：`eix-core/src/commands.rs` 始终加载 `~/.config/eix/cmds`（兼容旧的 `commands/`）
> 和项目祖先目录的 `.eix/cmds` / `.eix/commands`；命令就是提示词模板，嵌套目录映射为
> `team/name`，frontmatter 支持 `description` / `agent` / `model`，模板支持 `$ARGUMENTS` 和
> `$1..$9`。`[compations]` 可按 Claude Code / OpenCode / Codex CLI / Gemini CLI / Pi 分别打开
> 外部 commands / prompts（Gemini TOML 的 `{{args}}` 转为 `$ARGUMENTS`）。`eix-core/src/agents.rs`
> 同样加载原生 agent 与可选兼容 agent，frontmatter 支持 `role` / `model`，
> 正文追加到 system prompt；内置 `explore`。`session.agent` 持久化到
> `sessions.agent`，`spawn_agent` 接受 `agent` 参数；执行能力只由 session mode 决定。
>
> 模型：`[providers.<name>]` 提供命名 OpenAI 兼容 endpoint，恰好一个 provider 设置
> `default = true`；表名即选择前缀。`/model` 自动拉取每个 provider 的 `/models`，选择 id 为
> `<provider>/<api-model-id>`。`[[models]]` 仍可手动固定模型、补充成本/上下文/variants，或用
> `provider` 复用命名 endpoint。timeout / retry / context_tokens / system_prompt 属于独立的
> `[provider_runtime]`，不重复放在每个 provider 表里。`Config::route` 解析 provider 前缀和 `id#variant`，变体作为 `reasoning_effort`
> 发送；未指定变体时默认 `medium`，保留值 `#none` 表示不发送。`model.list` / `session.model` /
> `/model` picker（含变体行）已接入；`turn.completed`
> 带 `usage` / `cost` / `duration_ms`，用于内部统计与状态消息。
>
> MCP：`eixd/src/mcp.rs` 实现 stdio JSON-RPC 客户端，`[mcp.servers.<name>]` 懒启动并缓存进程。
> `tools/list` / `resources/list` / `prompts/list` 都支持分页 cursor；工具名为
> `mcp__<server>__<tool>`，始终需要审批（`auto` / `bypass` 除外），执行前抓树快照；资源与 prompt
> 通过只读的 `mcp_list_resources` / `mcp_read_resource` / `mcp_list_prompts` / `mcp_get_prompt`
> 暴露，不需要审批。`notifications/*/list_changed` 标记缓存失效，下次 turn 或 `/mcp` 刷新。
> `allowed_tools` / `denied_tools` 支持 `*` / `?`（`*` 也匹配 `/`，deny 优先）；`mcp.list` 与
> `/mcp` 显示状态、工具/资源/prompt 数量和错误。
>
> Plugins：`eix-core/src/plugin.rs` 定义 `eix-plugin.toml`、`PluginConfig`、
> `PLUGIN_API_VERSION = 1` 和 newline-delimited JSON DTO；`eixd/src/plugin.rs` 每个启用的
> server plugin 启动一个子进程并完成 `initialize` 握手。manifest 的 capabilities 是申请，
> 全局 config 的 `capabilities` 是授权（可收窄、不可扩大）；未授权的贡献会让启动失败。
> 工具暴露为 `plugin__<plugin>__<tool>`，默认走 `RiskCategory::Plugin` 审批并抓树快照
> （显式 `allow` 规则或 `bypass` 模式除外；`auto` 分类器不放行），
> `plan` / sub-agent 不可见；命令暴露为 `plugin/<plugin>/<command>`，当前只支持提示词模板。
> 事件通过 mpsc fire-and-forget 投递，慢插件不会阻塞 turn；storage 由 `plugin_kv` 表按
> 插件名和 `global` / `workspace:<path>` 隔离，每个插件限制 10,000 行 / 16 MiB。
> `plugin.list` / 工具 / 命令刷新会检测子进程退出并转为 `failed`、移除其贡献；daemon
> 退出时向空闲插件发送 `shutdown` 通知，500ms 后强制回收。项目层 config 在 merge 前删除
> 整个 `plugins` 表。进程边界只隔离崩溃，不是 OS sandbox：插件仍以 daemon 用户权限运行。
> TUI widget host、action command、hooks/rpc/network/workspace/secrets API 仍是后续阶段。
>
> Session 管理：`session.archive` / `session.delete` / `session.export` / `session.import` /
> `session.timeline` / `session.undo` / `session.redo` / `session.todos` RPC；archive 是持久化
> 标记且默认列表隐藏，delete 级联事件/草稿/搜索，export/import 保留事件 seq（压缩范围仍然有效）。
> undo/redo 用 `events.undone` 批次标记隐藏/恢复最近一个 user turn，并同步 FTS 索引；工作区文件
> 不自动回滚（需要 `rewind`）。TUI 对应 `/undo`、`/redo`、`/timeline`、`/archive`、
> `/delete`（两次确认）、`/todos`，队列支持 `rm N` / `move N M`；JSON 导出/导入仅保留 CLI
> 的 `eix export` / `eix import`。
>
> Skills：`eix-core/src/skills.rs` 始终加载原生 `.eix`，再按 `[compations]` 开关加载 Claude Code /
> OpenCode / Codex CLI / Gemini CLI / Pi 的 `<name>/SKILL.md`（`.agents/skills` 由 Gemini 或
> Pi 开关启用；同目录原生 `.eix` 优先）。system prompt 只放 `<available_skills>` 摘要，模型用
> 只读 `use_skill {name, arguments}` 按需取正文，正文展开 `$ARGUMENTS` / `$1..$9`。
> `allowed-tools` 支持通配，加载后在本 turn 内限制工具（并集，`use_skill` 始终可用，下一条
> user prompt 清空）；Claude 风格工具名映射到 eix 名称。TUI `/skills` 是带正文预览的 picker。
> Post-write formatter：默认按扩展名从内置表（Go/Rust/Python/JS/TS/C/C++/Shell/Lua/TOML/
> SQL/Terraform/Nix/Kotlin/Swift/Dart/Elixir/Haskell/OCaml/Scala/Ruby/PHP/R/Zig/Nim/V/Crystal/
> Gleam/Elm/Clojure/Solidity/Cairo/GDScript/Jsonnet/CMake/Markdown/JSON/YAML/HTML/CSS/GraphQL/
> Proto 等）选择，缺失的可执行文件静默跳过；显式 `[[formatter]]` / `formatter` 条目优先，命令
> 不存在时回退到内置候选，`command: None` 可禁用某扩展，`formatter_defaults: false` 关闭内置
> 回退。直接写入、blocking apply、staged apply 后运行
> （`$1` 为路径，按顺序首个匹配，失败只附注）。`todo` / `webfetch` / `websearch`
> 为新增工具；`todo` 是 daemon 内存态任务表，网络工具无 API key（DuckDuckGo HTML），读取结果会截断。
> TUI 支持 `[theme]` 调色板、`[keys]` 全局 / TUI 动作与 leader 覆盖（输入框的 vim 编辑键仍由编辑器
> 自己处理），以及 prompt 里的
> `!<cmd>`：直接执行 shell（同样沙箱、先抓 tree snapshot、只读 session 拒绝），命令和输出写入 tool 事件。
>
> 图片：`prompt.submit` 接受 `images: [path]`，daemon 经 `resolve_path_in` 校验后按 SHA-256 写入
> `<data>/blobs/<前两位>/<hash>`，事件只存 blob id / MIME / 大小；`ChatMessage::to_wire` 把带图
> 消息编成 OpenAI 多模态 content 数组（text + image_url data URL），文本消息仍保持字符串
> content。旧的内联 base64 事件仍可回放；export 会内联图片，import 重新落 blob。默认最多 4 张、
> 每张 8 MB、无引用 blob 保留 30 天（`[images]` 可调），启动/删除 session/后台每 6 小时 GC。
> TUI `/image`、直接粘贴/拖入路径或 `file://` URI、CLI `eix send --image` 是入口。
>
> LSP：默认内置 bash/cpp/css/go/html/json/lua/markdown/python/rust/sql/toml/typescript/yaml
> 的 server 定义，无需配置；显式 `[lsp.servers.<name>]` 与内置列表合并，同名项覆盖。
> `eixd/src/lsp.rs` 是精简 stdio 客户端（Content-Length framing），按 server+workspace root
> 懒启动并缓存；写入/编辑/apply 成功后收集 `textDocument/publishDiagnostics`，作为附注追加到工具
> 输出，`lsp_diagnostics` 只读工具可主动查询。`lsp.workspace` 扫描 workspace（默认 200 文件、上限
> 2000，跳过 `.git` / `target` / `node_modules` 等，最多 2000 条诊断、5 秒收集窗口），仅保留
> RPC 能力。初始化失败按 key 缓存；配置热重载 `clear()` 后重建。只支持 diagnostics，不支持
> completion/hover/rename。

## 17. 待定

- 默认模型与具体的 OpenAI 兼容端点。
- `<leader>a` 命名空间：独立 nvim 前端已用 buffer-local 映射实现动作 palette；若你的全局映射也叫 `<leader>a`，buffer-local 会优先。
- `@` 触发已定为「行首或空白后」，其余位置插入字面 `@`；暂不转义。
- 是否启用鼠标。
- session 自动命名规则（前两轮对话后总结 / 手动重命名优先；名称只保留字母数字和英文词间空格）。
- 远程 attach 的广播范围：已支持按连接 `session.subscribe` 收窄（默认仍全部）；是否把"只订阅自己打开的 session"设为默认白名单仍待定。

## 18. 命名

**项目名：`eix`**（读作 "eks"）

- 出处：欧拉公式 `e^{ix} = cos x + i·sin x`。实数轴上的一点，乘上虚数单位 `i`，便落到复平面的单位圆上——**换一个维度看同一个东西**，正如在多个 session / 视角之间穿梭。
- 双关：加泰罗尼亚语中 `eix` =「轴 / 轴线」。
- 记号也顺：`e`（内核）、`i`（输入）、`x`（可执行文件）。

**已知识别冲突**

- Gentoo 有同名工具 `app-portage/eix`（portage 包搜索）。你在 Arch 下基本遇不到，领域也不相干；若哪天在意，可只用 `eix` 作项目名，二进制另起（或 daemon 用 `eixd` 区分）。

**命名约定**

- 仓库 / crate 前缀：`eix-*` → `eix-core`、`eixd`、`eix-tui`。
- 二进制：`eix`（TUI + CLI）、`eixd`（daemon，通常由 `eix` 自动拉起）。
- 路径：配置统一 `~/.config/eix/`（macOS 同样；含 `skills/`、`cmds/`、`agents/`）；数据在平台
  data-local 目录下的 `eix/`（Linux 通常 `~/.local/share/eix/`，macOS
  `~/Library/Application Support/eix/`）；socket 优先 `$XDG_RUNTIME_DIR/eix.sock`，没有 runtime
  dir 时回退到平台 cache 目录的 `eix/eix.sock`（Linux 通常 `~/.cache/eix/eix.sock`，macOS
  `~/Library/Caches/eix/eix.sock`）。

## 19. 实现参考

本节是从 README 合并过来的详细操作与实现参考，补充前文的设计决策；两者冲突时以代码和
`crates/eix-core/src/protocol.rs` 为准。

### 19.1 TUI 键位与 prompt 命令

#### 全局与窗口

| 快捷键 | 功能 |
| --- | --- |
| `F1` | 打开可搜索帮助（任意模式） |
| `Ctrl-T` | 切换 reasoning：low / medium / high（任意模式） |
| `Ctrl-/` | 模型 picker（任意模式；部分终端为 `Ctrl-_` / `Ctrl-7`） |
| `Ctrl-1` … `Ctrl-9` | 跳到第 N 个已打开 session / tabby tab（`Ctrl-7` 通常留给模型 picker） |
| `<` / `>` | 转录区 / 输入框 normal：上一个 / 下一个 pane（`Shift+,` / `Shift+.`） |
| `Ctrl-W` `Ctrl-W` | 转录区 / 输入框 normal：1 秒内连按两次聚焦下一个 pane |
| `Z` | normal / 转录区退出 |
| `q` | normal / 转录区关闭当前 pane（最后一个 pane 保持打开） |
| `Ctrl-C` / `Ctrl-D` | 任意模式退出（先保存草稿） |
| `Esc Esc` | normal 模式：中断运行中的 turn，或进入转录区 |

#### 输入框 / 转录区

| 快捷键 | 功能 |
| --- | --- |
| `i` | 聚焦输入框并进入 insert 模式 |
| `Esc` | 输入框：回到 normal 模式 |
| `:` | 打开命令行 |
| `/` / `?` / `#` | 搜索当前 transcript；`n` / `N` 跳下一个 / 上一个，`Esc` 清除 |
| `Enter` | insert 模式发送（turn 运行中自动排队）；normal / 转录区把当前 session 提到第一位 |
| `Ctrl-J` | insert 模式换行 |
| `Ctrl-P` | 命令面板（输入框 normal / 转录区） |
| `Tab` | 输入框 normal / 转录区：Plan → Build → Auto → Bypass 循环 |
| `Shift-Tab` | 反向循环：Build → Plan → Bypass → Auto |
| `j` / `k` | 转录区滚动；输入框 normal 下移动草稿行 |
| `Ctrl-J` / `Ctrl-K` | 输入框 normal：浏览已提交的 prompt 历史 |
| `gg` / `G` | 转录区顶部 / 跟随末尾 |
| `PageUp` / `PageDown` | 翻页 |
| `Ctrl-B` / `Ctrl-U` | 快速向上翻页（`Ctrl-F` 向下） |

#### Leader（默认 `Space`，可配置）+ 键

| 快捷键 | 功能 |
| --- | --- |
| `Space o` | 把已有 session 开到新 pane |
| `Space n` | 新建 session（有 worktree 则复用） |
| `Space W` | 把当前 session 移入新 worktree |
| `Space c` | 保存并关闭当前 pane |
| `Space x` | 取消当前 turn |
| `Space a` | 跨 session 审批收件箱 |
| `Space m` | 完整执行模式 picker |
| `Space f` | 文件 picker |
| `Space s` | 搜索文件内容并插入 `@path:line` |
| `Space g` | session picker（切换聚焦 pane） |
| `Space h` | 用户输入 timeline picker |
| `Space G` | git picker（worktrees + branches） |
| `Space p` | 后台进程列表 |
| `Space v` | horizontal 布局 |
| `Space t` | tabby 布局 |
| `Space r` | fibonacci 布局 |
| `Space l` | 循环切换布局 |

#### 审批 / 问题卡片

| 快捷键 | 功能 |
| --- | --- |
| `y` / `n` | 批准 / 拒绝 |
| `a` | Always allow；批准并记住同类调用 |
| `j` / `k`、`Ctrl-J` / `Ctrl-K`、`Ctrl-N` / `Ctrl-P` | 移动选项 |
| `Enter` | 执行当前选项 / 回答 |
| `1` … `9` | 直接选择对应选项 |
| `d` | 全屏审阅 diff；`Space` 勾选 hunk |
| `e` / `o` | 编辑 staged 内容 / 打开真实文件 |
| `r` | 输入理由并拒绝 |
| `Space a` | 跨 session 审批收件箱 |

#### Picker / 浮层通用

| 快捷键 | 功能 |
| --- | --- |
| 直接输入 | 搜索 / 过滤 |
| `Ctrl-J` / `Ctrl-K`、`Ctrl-N` / `Ctrl-P` | 下一条 / 上一条 |
| `Ctrl-B` / `Ctrl-F` | 大跳 10 条 |
| `Enter` | 选择 / 打开 / 执行 |
| `Esc` | 关闭 |

#### 命令行

| 快捷键 / 命令 | 功能 |
| --- | --- |
| `:` | 打开命令行 |
| `Tab` | 打开并补全命令菜单 |
| `Ctrl-J` / `Ctrl-K`、`Ctrl-N` / `Ctrl-P` | 菜单上下选择 |
| `↑` / `↓` | 浏览命令历史 |
| `Esc` | 先关菜单，再退出命令行 |
| `:q` | 保存并关闭当前 pane |
| `:qa` | 保存并退出 |
| `:w` | 保存草稿 |
| `:wq` / `:x` | 保存并关闭当前 pane |
| `:!cmd` | 直接执行 shell |
| `:r!cmd` | 把 shell 输出插入输入框 |

#### 鼠标

| 操作 | 功能 |
| --- | --- |
| 左键点击 | 聚焦 pane；点击 `Thinking` / 工具块折叠或展开；点击 subagent 卡片打开子 session；点击子 session 底部 `← parent agent` 返回 |
| 拖拽选择 | 复制转录文本，右上角显示 `Copied N chars` |
| 滚轮 | 滚动转录 |

#### prompt 命令

在输入框里把 `/命令` 作为整条消息发送（不会传给模型）。输入 `/` 时会在输入框上方弹出
补全菜单：`↑`/`↓` 或 `Ctrl-N`/`Ctrl-P` 选择，`Tab` 补全，`Enter` 直接执行。
normal 模式下的 `:` 命令行接受同样的命令名（不带 `/`），并保留当前草稿；另外提供
`:q` / `:q!`（关闭 pane）、`:qa` / `:qa!`（退出）、`:w`（保存草稿）、`:wq` / `:x`（保存并关闭）、
`:!<cmd>`（直接执行 shell）和 `:r!<cmd>` / `:r !<cmd>`（读取输出并插入输入框）。

| 命令 | 行为 |
| --- | --- |
| `/help [query]` | 打开可搜索帮助（命令、快捷键、notes）；可带初始查询 |
| `/keys [query]` | 只打开快捷键章节，同样支持即时搜索和上下选择 |
| `/new [name]` | 新建 session：当前 session 有 worktree 时复用它，否则共享当前工作区（不建 worktree） |
| `/new-pane` | 打开 session picker，把已有 session 放进新 pane |
| `/new-worktree [name]` | 把当前 session 移入独立 git worktree；已有 worktree 时只提示，不重复创建 |
| `/ready` | 把当前 worktree 交给 daemon 做 rebase / 测试 / ff merge |
| `/integrate` | 显示当前 session 的集成队列状态与最近错误 |
| `/clear` | 清空模型上下文（更早的 turn 仍留在转录和事件日志里，只是不再发给模型） |
| `/compact [notes]` | 让模型总结当前上下文，用摘要替换历史；生成的 summary 会显示在对话中，`notes` 直接作为摘要文本 |
| `/btw <question>` | 用一次性上下文回答旁支问题；不写入事件日志，也不进入主对话；`Esc` 关闭 |
| `/cd [path]` | 切换当前 session 的 cwd（支持 `~` 和相对路径）；省略时显示当前路径 |
| `/init` | 生成一份 AGENTS.md 初始化 prompt，放进输入框供你审阅后再发送 |
| `/queue [clear]` | 查看队列；`clear` 清空，`rm N` 删除第 N 条，`move N M` 调整顺序 |
| `/sessions` | 打开 session picker |
| `/files` | 文件 picker，选中后插入 `@path` |
| `/grep [query]` | 内容 picker：搜索 workspace 文件内容，选中后插入 `@path:line`；可直接带初始查询 |
| `/git` | git picker：worktrees + branches，选中后插入 worktree 路径或分支名（旧名 `/worktrees`、`/branches` 仍可用） |
| `/processes` / `/approvals` | 背景进程列表 / 审批收件箱 |
| `/attention` / `/cancel` | 跳到下一个需要 attention 的 pane / 取消运行 |
| `/model [id]` | 打开可搜索的模型 picker：合并默认 provider 的 model、所有 `providers.*` 自动拉取的 `/models` 和 `models:`（含变体行）；直接 `/model provider/model#variant` 可固定到某个模型 |
| `/agents [name]` | 选择当前 session 的 primary agent；`build` / `plan` 是内置的 |
| `/subagents [n]` | 调整 daemon 当前运行时的 `spawn_agent` 并发上限（默认 4，可选 2/4/8/16/32/64/128/256）；不带参数打开 picker（仅本次 daemon 生命周期，`/reload` 或重启后回到配置文件默认值） |
| `/mcp` | 打开 MCP server picker（搜索 server，右侧显示命令、工具数、错误） |
| `/reload` | 重新读取 daemon 的分层配置（等价于 `config.reload` RPC），并重新加载自定义 Markdown 命令 |
| `/colorscheme` | 打开 colorscheme picker：`↑`/`↓` 或 `Ctrl-J`/`Ctrl-K` 实时预览，`Enter` 应用，`Esc` 取消并恢复原配色 |
| `/skills [name] [args]` | 打开 skill picker（右侧预览正文）；带名字时把正文放进输入框并展开 `$ARGUMENTS` / `$1..$9` |
| `/todos` | 打开任务列表 picker（搜索任务，显示状态和详情；内存态，daemon 重启后清空） |
| `/image <path>` | 把图片挂到下一条 prompt（png / jpg / gif / webp，最多 4 张、每张 8 MB）；不带参数时显示已挂图片，`/image clear` 清空；也支持直接粘贴或拖入路径 / `file://` URI |
| `/timeline` | 当前 session 用户输入历史，按时间倒序；带时间戳、搜索和完整预览，`Ctrl-J/K` 或 `Ctrl-N/P` 选择，`Enter` 跳到对应消息位置（`Space h` 同效） |
| `/archive` | 归档当前 session（从 picker/列表隐藏，事件保留） |
| `/delete` | 删除当前 session；需要连续执行两次确认 |
| `/undo` / `/redo` | 隐藏/恢复最近一个 user turn（事件仍在日志里，可通过 redo 恢复） |
| `/fork [name]` | fork 当前 session（默认共享工作区；`worktree.auto: true` 时自动建 worktree） |
| `/perm` | 弹窗搜索并选择执行模式：`plan` / `build` / `auto` / `bypass`；也可直接 `/perm <mode>`；`Space m` 打开同一 picker；旧名 `/mode` 仍可用 |
| `/exit` / `/quit` | 保存草稿并退出整个 eix 客户端（不关闭 session pane） |
| `/close` | 保存草稿并关闭当前 session；如果它是最后一个 pane，同时退出 eix |

在 prompt 里以 `!` 开头（例如 `!cargo test`）会直接在当前 session 的 cwd 里执行 shell：走同样的
沙箱；写命令先抓 tree snapshot，只读 session 只允许风险分析判为只读的命令。命令和输出以 tool
卡片的形式留在转录里。

### 19.2 工具、审批与沙箱

daemon 向模型暴露这些工具。下表的“默认审批”按默认配置概述；`build` 模式下工作区内的
低/中风险 `write_file` / `edit` 默认直接应用（仍保留 snapshot 和 patch 记录），
`tools.auto_apply_workspace_edits: false` 或显式规则要求时才改用 stage + 审批。

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
| `write_file` | 写入 cwd 内文件（自动建父目录）；工作区内低/中风险直接应用，否则 stage 到 shadow workspace | 直接应用或 stage + 审批落盘 |
| `edit` | 精确字符串替换（默认要求唯一匹配，`replaceAll` 可全部替换）；工作区内低/中风险直接应用，否则 stage | 直接应用或 stage + 审批落盘 |
| `patch` | 多文件 `*** Begin Patch` / Add / Update / Delete，先全量校验再写入 | 需要审批 |
| `run_command` | 在 cwd 内用 `sh -c` 执行命令，带超时；命令可分析时默认沙箱 | 只读命令放行；构建在沙箱内放行；未知/破坏性命令询问或拒绝 |
| `start_command` | 在沙箱里启动后台进程并立即返回 id，日志落盘 | 按风险分析；无法沙箱时询问 |
| `process_output` | 读取后台进程的尾部输出（只读） | 自动放行 |
| `spawn_agent` | 起一个只读子 agent 并发处理独立问题，可用 `agent` 指定自定义 agent | 自动放行（子 agent 只读） |
| `ask_user` | 让用户从 2-6 个选项中选择 | 交互，不修改文件 |
| `mcp__*` | MCP server 暴露的工具 | 默认询问；已注册的只读类工具（`get_*` / `list_*` / `read_*` / `search_*`）自动放行；resource/prompt 只读 |
| `plugin__*` | 外部插件暴露的工具 | 默认询问（显式 allow / bypass 例外）；plan/sub-agent 不可见；执行前快照 |

所有路径默认都会做 workspace 沙箱检查（拒绝 `..` 和 symlink 逃逸；`bypass` 会解除 workspace
路径限制，但仍执行硬拒绝和 secret 保护）；工具输出先按
`max_output_bytes` 截断，进入 provider 上下文时再按 `context.tool_output_tokens` 做 head/tail
省略，完整输出仍保留在事件日志。一个工具批次里连续的只读工具（`read_file` / `list_dir` /
`grep` / `glob` / `webfetch` / `websearch`）以及风险分析判定为低风险只读且权限已放行的
`run_command` 会并发执行（上限 8）；无需审批的直接 `write_file` / `edit` 只要目标文件
互不重叠，也会并发执行（上限 32）。同一文件或父子路径上的写入、`patch`、需要审批/暂存的写入、非只读
命令、审批和交互调用仍是分段屏障，写后读取在写入完成后执行；结果按调用顺序回填。

`run_command` 使用统一风险分析：shell tokenizer 会拆出每个命令段、输入/输出重定向、
`sudo`/`env` 等包装器、嵌套 `sh -c` 和命令替换。`git status`、`cat`、`grep`、`ls`、
`sed -n`、纯 `print`/`printf` 的 `awk`、`uniq`、`tr`、`cut`、`sort` 等只读过滤/查询命令
直接放行；工作区内的 `cd`（以及 `cd <workspace> && ...`）也直接放行，离开工作区的 `cd`
仍询问。`env` / `timeout` / `nice` / `command` 等包装器按实际执行的命令判断，只有
`sudo` / `doas` 额外标记提权；`env` 设置 `PATH`、`LD_PRELOAD`、`GIT_*` 等能改变命令
查找/加载/配置的变量时仍询问，普通 `FOO=1` 之类不受影响。`xargs` 仅在内层是 `wc` /
`ls` / `stat` 等只报告名称、元数据或计数的命令时放行，内容读取器和 `-a` / `--arg-file`
保持询问，但 `find ... | xargs cat | wc -l` 这类由下一段 `wc` 消费内容的管道可以放行
（只输出计数）。

`< file` 按读取处理，不会误判成写入；`> file` / `>> file` 仍按写入审批。命令替换
`$(...)` 和反引号会递归分析内部命令：内部只读时，`echo "$(find crates -name '*.rs' |
wc -l)"` 可直接放行；但命令替换的结果一旦被当作路径、脚本、`cd` 目标，或作为
非纯输出命令的普通参数（例如 `git` / `date` / `grep` 的选项），仍会询问。
静态、工作区内的 `for` 循环（字面量或通配符列表，如
`for f in crates/*/Cargo.toml; do ...; done`）会展开后逐条分析，`n=$(...)` 这类小写
局部变量赋值也不会再被当成未知命令；`printf -v` 仅允许写入小写局部变量。列表含绝对
外部路径、`..`、隐藏文件通配符，或循环体本身是未知/写入/嵌套控制流时仍会询问；通配符
首段需要字面前缀（`crates/*` 可以，裸 `*` / `*.rs` 会询问，避免展开成 `-o` 之类的选项）。

动态列表 `for f in $(...); do ...; done` 也会先判断生成命令的风险。生成命令必须是只读
且可放行的，例如 `find <工作区路径>`、`git diff --name-only`、`git ls-files`、
`git ls-tree -r --name-only`、`rg --files` / `rg -l`；管道后只允许 `grep` / `head` /
`tail` / 简单 `sort` 这类保持整行路径的过滤。循环体只把 `$f` 用于输出/测试
（`echo`、纯数据 `printf`、`test` / `[` / `true` / `false` / `:`），或用于带字面相对
前缀的路径，如 `cat "./$f"`、`sed -n '1p' "./$f"`、`wc -l "./$f"`（变量未使用也
可以）。`echo "$f"`、`printf '%s\n' "$f"` 和 `for f in $(find crates -name '*.rs'); do
echo "$f"; done | sort | head` 可直接放行。

路径参数必须带安全相对前缀，是因为命令替换会按空白拆分：路径片段可能以 `-` 或 `/`
开头，`cat "$f"` / `sort "$f"` 可能变成选项或外部绝对路径；`cat "./$f"` 则始终留在
当前工作区内。生成命令写到文件/网络/secret、读取工作区外、未知命令、带引号的
`"$(...)"` 列表、多个列表词、嵌套控制流、以及循环体写入或把输出接入 `sh` / `xargs` /
`sed` / `awk` 等执行器时仍会询问。`cargo test` / `npm test` 等构建命令默认在沙箱内
执行；`rm`、写入重定向、未知程序、脚本解释器、`find -exec`、网络命令等按风险询问。
`rm -rf /`、块设备 `dd`、`mkfs`、关机、fork bomb、私钥读取以及把 secret 占位符送进
网络/命令/MCP 调用会硬拒绝，`auto` / `bypass` 和任何 `allow` 规则都不能绕过。
`git push --force` 视为高风险，默认询问；`sed -i`、`awk` 的 `system`/重定向、`date -s`、
`ss -K`、`sort -o`/`-T`/`--files0-from`、`diff --paginate`、`find -files0-from`、
`xxd`/`uniq` 的第二个文件参数、`wc`/`du --files0-from` 等带副作用或动态文件集的选项同样
不会自动放行。`jq` 读取 `env`/`$ENV` 会硬拒绝，`jq -f` / `--from-file` 等无法检查的外部
脚本保持询问。

隐私边界：daemon 在把消息发给 provider 之前，会把密码、`sk-…` / Stripe / GitHub / Slack
等 token、Bearer、URL 凭据、PEM 私钥和 `password=` / `api_key:` 等赋值替换成带随机 salt 的
占位符。占位符只在本地文件工具（`read_file` / `grep` / `write_file` / `edit` / `patch` 等）
的参数和最终文本里还原；`run_command`、`start_command`、`webfetch`、`websearch` 和 MCP
参数不会还原，一旦出现占位符会被判定为 `secret-exfil` 并硬拒绝。URL 在审批卡片和审计事件里
只保留 `?…` 之前的部分。占位符映射只在当前 provider 进程内保存；本地事件日志和 TUI 仍显示
原文，方便审阅。图片内容不做文本脱敏，敏感截图不要上传。

`write_file` 会先生成 unified diff 和逐 hunk 预览，TUI 审批卡片、`eix chat` 和 `eix log`
都会显示；在 TUI 里用 `d` 全屏审阅可以按 hunk 选择应用，命令行用
`eix approve --hunks 0,2`（不带参数或空选择 = 整文件）。策略由 `tools:` 配置控制：

```yaml
tools:
  max_output_bytes: 20000
  command_timeout_secs: 60
  max_parallel_subagents: 4  # 2/4/8/16/32/64/128/256；大规模任务可按需调高
  sandbox: auto            # auto / required / off
  sandbox_network: false
  sandbox_read_paths: ["~/.cargo", "~/.rustup", "~/.cache", "~/.npm"]
  sandbox_writable_roots: []
  snapshots: true
  external_directory: ["/tmp/eix-shared", "~/.cache/eix/**"]
  network_allow: ["docs.rs", "*.github.com"]
```

显式规则按“具体度”选择，`deny` 全局优先，不会被 `auto` / `bypass` 绕过：

```yaml
tools:
  rules:
    - id: block-rm
      tool: run_command
      command_regex: "^rm "
      action: deny
    - id: cargo-sandbox
      tool: run_command
      command: [cargo, test]  # argv token 前缀，不是整条 shell 字符串
      action: sandbox
    - id: review-src
      tool: write_file
      path: "src/**"
      action: ask
    - id: docs-allow
      tool: webfetch
      path: "https://docs.rs/**"
      action: allow
```

- 规则字段是 AND 关系；`tool = "*"` 可匹配任意工具。`path` 对文件工具是 workspace 相对
  glob，对 `webfetch` 是 URL；`command` 是 argv token 前缀。`deny` 只要**任一** shell
  段命中就拒绝（`echo hi && rm -rf build` 会命中 `command = ["rm"]`）；`allow` / `ask` /
  `sandbox` 要求**每个** shell 段都满足，所以 `cargo test && rm -rf /` 不会命中
  `command = ["cargo", "test"]`。带重定向的 shell 命令不会命中非 deny 的 `command` 前缀规则，
  避免 `echo hi` 的 allow 覆盖 `echo hi > /etc/passwd`。`signature` 匹配规范化后的完整参数。
  `command_regex` 用于复杂表达式，但更适合 `deny` 而不是 `allow`。
- `allow` 只放行风险分析本来会 ask 的调用；硬拒绝和命令中的危险段仍然优先。`sandbox` 表示
  必须在沙箱内执行，bwrap 不可用时转为 `ask`（只有 `bypass` 会直接放行）。`action = "deny"` 最先
  扫描所有来源，哪怕更具体的规则是 `allow`。
- 规则来源优先级为 `session > user > project > builtin`；具体度相同时越靠近当前 session 的
  来源胜出。项目 `.eix/config.yaml` 只能收紧：`sandbox = "off"`、
  `sandbox_network = true`、`external_directory`、`network_allow`、额外
  sandbox 路径会被忽略，`allow` / `sandbox` 规则会降级为 `ask`，并写入
  `security_warnings`（`eix doctor` 可见）。
- 审批卡片上的 `a`（`Always allow`）只在中低风险出现：session scope 只存在内存，user scope
  追加到用户配置；文件工具按路径、shell 按单段 argv 前缀、其它工具按精确参数收窄。`HIGH` /
  `CRITICAL` 不提供 Always，服务端也会拒绝把它升级成持久规则。
- 每次工具决策都会写入 `permission_evaluated` 审计事件（action、风险等级、分类、命中规则
  及来源、沙箱状态；`auto` 分类器还会记录 decision / confidence / reason / model /
  cached / latency）；自动批准的写入仍会快照，`patch_applied` message 会带命中的规则 id。
- `webfetch` / `websearch` 是只读 GET 类网络工具：默认放行；配置非空 `network_allow`
  后只放行命中域名。URL query 在展示和日志中脱敏，网络工具不会收到还原后的 secret。

`run_command` 在沙箱模式下用 bubblewrap 隔离：系统目录只读，`/tmp` 是私有 tmpfs，
session workspace 可写；`sandbox_read_paths` 里的工具链缓存逐个只读挂载，整个 `$HOME`
不再默认暴露。网络默认关闭；`sandbox_writable_roots` 可额外开放工作区外目录。
`bwrap` 未安装时 `sandbox = "auto"` 会把需要沙箱的命令降级为强制审批，`required` 直接
阻断，`off` 完全关闭沙箱。`eix doctor` 会显示规则数量、网络和 bwrap 状态。

`build` 对未命中规则的默认 `ask` 仍然询问；`auto` 把这些调用交给意图分类器，只有高置信度、
与当前请求直接相关的安全步骤才放行，超时 / 非法输出 / 低置信度仍然询问；`bypass` 直接放行。
三者都不会绕过硬拒绝、`deny` / 显式 `ask` 规则、`plan` 的只读上限或“必须沙箱”的失败路径
（`bypass` 在无 sandbox 时是例外）。一次 turn 不设固定工具往返上限，由模型自行结束
或用户中断；重复工具批次仍会被 doomloop 守卫终止。

#### Staged shadow workspace

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

#### 执行前快照

`write_file`（直接写入或应用 staged 补丁时）和 `run_command`（执行前）会先做一次快照，落在
`<data-dir>/snapshots/<session>/<snapshot>/`：

- `write_file`：只复制它要覆盖的那个文件；目标原本不存在也会记下来（回滚时删除）。
- `run_command`：复制整个 workspace（session 的 `cwd`），因为 shell 能改任何东西。
  `.git`、`target`、`node_modules`、`.venv`、`dist` 等派生目录不复制；单文件 > 8 MB、
  总量 > 200 MB、条目 > 5 万时跳过并计数；符号链接跳过。
- 每次快照写一条 `snapshot` 事件（`eix log` 显示为 `snap> file before call_1 — 1 file(s),
  0 skipped`），记录文件数、字节数和跳过数；抓取失败也会记 `ok = false`，并在工具输出里
  附一行 `[snapshot failed: ...]`。每个 session 保留最近 20 份，更早的自动清理。
- 快照字节只存在 `snapshots/` 目录，事件日志只做索引。`Snapshotter::restore` 已接进
  `session.rewind`：`eix rewind` 会按 `seq >= at` 倒序恢复快照并返回
  新 session。回滚是尽力而为的，恢复失败、缺失文件、被 prune 的旧快照都会在结果的
  `warnings` 里列出，`restored = false` 表示工作区没有完全回到切点。
- 大仓库可以设 `tools.snapshots: false` 关掉；关闭后不再记录 `snapshot` 事件。
  `eix doctor` 会显示当前开关。

### 19.3 Provider 自动重试

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

### 19.4 扩展系统

#### 其他 agent 兼容

默认只读取 eix 自己的 `.eix` / `~/.config/eix`。需要复用其他 agent 的资源时，在
`compations:` 中逐项打开：

| 开关 | 读取内容 |
| --- | --- |
| `claude_code` | `~/.claude` 与项目 `.claude` 下的 skills、commands、agents，以及 `CLAUDE.md` |
| `opencode` | `~/.config/opencode` 与项目 `.opencode` 下的 skills、commands、agents，以及 `AGENTS.md` |
| `codex_cli` | `$CODEX_HOME`（默认 `~/.codex`）与项目 `.codex` 下的 skills、prompts，以及 `AGENTS.md` |
| `gemini_cli` | `~/.gemini` 与项目 `.gemini` 下的 skills、commands（TOML）、agents，以及 `GEMINI.md`；也读 `.agents/skills` |
| `pi` | `$PI_CODING_AGENT_DIR`（默认 `~/.pi/agent`）与项目 `.pi` 下的 skills、prompts，以及 `AGENTS.md` / `CLAUDE.md`；也读 `.agents/skills` |

- 同一目录下原生 `.eix` 资源优先；更靠近 cwd 的项目层覆盖外层。
- 项目 `.eix/config.yaml` 只能关闭兼容开关，不能开启；尝试开启会被忽略并写入 `security_warnings`。
- 只读取资源文件，不导入其他 agent 的 permissions、hooks、settings、provider 或 MCP 配置。
- Gemini 的 `{{args}}` 会转换成 eix 的 `$ARGUMENTS`；其他 agent 的命名占位符只保证原样保留。
- 兼容 agent 定义时只读取 Markdown 正文和 eix 支持的 frontmatter；Claude/Gemini 的工具列表不会被解释成 eix 权限。

#### Skills

原生和兼容来源如下（越靠近项目越优先；同一目录下 `.eix` > `.opencode` > `.claude`）：

```text
~/.config/eix/skills/<name>/SKILL.md          # 始终加载
<project>/.eix/skills/<name>/SKILL.md         # 始终加载

~/.claude/skills/<name>/SKILL.md              # claude_code
<project>/.claude/skills/<name>/SKILL.md
~/.config/opencode/skills/<name>/SKILL.md     # opencode
<project>/.opencode/skills/<name>.md
$CODEX_HOME/skills/**/SKILL.md                 # codex_cli
<project>/.codex/skills/**/SKILL.md
~/.gemini/skills/**/SKILL.md                   # gemini_cli
<project>/.gemini/skills/**/SKILL.md
~/.pi/agent/skills/**/SKILL.md                 # pi
<project>/.pi/skills/**/SKILL.md
~/.agents/skills/**/SKILL.md                   # gemini_cli 或 pi
<project>/.agents/skills/**/SKILL.md
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

#### 自定义命令与 agent

原生命令与 agent 始终加载；外部来源由 `compations:` 控制：

```text
~/.config/eix/cmds/*.md              # 全局命令
~/.config/eix/commands/*.md          # 旧路径，仍兼容
<project>/.eix/cmds/*.md             # 项目命令（可嵌套，team/name.md → /team/name）
<project>/.eix/commands/*.md         # 旧路径，仍兼容

~/.config/opencode/commands/*.md     # opencode
~/.claude/commands/*.md              # claude_code
$CODEX_HOME/prompts/*.md             # codex_cli
~/.gemini/commands/**/*.toml         # gemini_cli（{{args}} → $ARGUMENTS）
$PI_CODING_AGENT_DIR/prompts/*.md    # pi
<project>/.opencode/commands/*.md
<project>/.claude/commands/*.md
<project>/.codex/prompts/*.md
<project>/.gemini/commands/**/*.toml
<project>/.pi/prompts/*.md

~/.config/eix/agents/*.md            # 全局 agent
<project>/.eix/agents/*.md           # 项目 agent
~/.claude/agents/*.md                # claude_code
<project>/.claude/agents/*.md
~/.config/opencode/agents/*.md       # opencode
<project>/.opencode/agents/*.md
~/.gemini/agents/*.md                # gemini_cli
<project>/.gemini/agents/*.md
```

自定义命令在启动时自动加载一次；修改或新增命令文件后执行 `/reload` 重新加载，不会在每次
打开命令菜单时扫描磁盘。

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

agent 的 frontmatter 支持 `description`、`role`（`primary` / `subagent` / `both`）和
`model`；正文追加到 system prompt。Agent 不再携带权限或模式，执行能力由 session mode 决定。

`/agents` 只列 primary-capable agent；`spawn_agent` 的 `agent` 参数可指定
subagent-capable agent（模型会在 system prompt 里看到列表）。

#### 图片附件

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
turn 运行中建议先等它结束再挂图片。

#### 语言服务器（LSP）

LSP 默认覆盖 bash、cpp、css、go、html、json、lua、markdown、python、rust、sql、toml、
typescript、yaml，无需配置；对应扩展名的文件第一次写入后懒启动相应 stdio server。daemon
发送 `initialize` / `initialized` / `didOpen` / `didChange`，收集
`textDocument/publishDiagnostics`，并把 `error/warning/... file:line:col` 附在写入工具的
结果后面（最多 50 条）。并行写段会先把同一 server 的所有目标文件一起打开，再收集一次诊断，
避免每个文件重复等待收集窗口。没有匹配 server 的文件完全不受影响；server 启动失败只附一条
`[lsp: ...]`（未安装的可执行文件同样如此），不会回滚写入。需要覆盖内置项或添加其他语言时，
可写 `[lsp.servers.<name>]`；显式条目会与内置列表合并，同名项覆盖。模型也可以主动调用只读
工具 `lsp_diagnostics {path}`。

daemon 仍提供 `lsp.workspace` RPC，可扫描整个 workspace 并聚合 diagnostics（默认最多 200 个
文件、上限 2000，跳过 `.git` / `target` / `node_modules` 等目录）。配置热重载会清掉缓存的
server 进程，下一条写入重新启动。

#### MCP

`[mcp.servers.<name>]` 配置的 stdio server 会在首次 turn 时懒启动、握手，并读取
`tools/list`、`resources/list`、`prompts/list`（后两者由 server 能力决定）。工具以
`mcp__<server>__<tool>` 暴露。已注册的只读类工具（如 `get_*`、`list_*`、`read_*`、
`search_*`）默认放行；其余 MCP 工具仍需要审批，执行前会捕获一次工作区树快照。
`plan` session 和 sub-agent 看不到 MCP 工具。

资源与 prompt 通过四个只读 meta-tool 暴露，不需要审批：

- `mcp_list_resources {server?}` / `mcp_read_resource {server, uri}`
- `mcp_list_prompts {server?}` / `mcp_get_prompt {server, name, arguments?}`

`resources/list`、`resources/read`、`prompts/list`、`prompts/get` 都支持分页 cursor；二进制
resource 只报告 MIME 和大小，不会把 base64 塞进上下文。server 发来的
`notifications/{tools,resources,prompts}/list_changed` 会标记缓存失效，下一次 turn 或
`/mcp` 时重新读取。每个 server 可用 `allowed_tools` / `denied_tools` 过滤工具（`*` 和 `?`
通配，deny 优先；allowed 为空表示全部）；被过滤的工具既不出现在模型工具表里，也无法调用。
`/mcp` 显示 server 状态、工具/资源/prompt 数量和错误。

#### 插件（out-of-process）

插件是独立子进程，不是 Rust `dylib`，因此崩溃或死循环不会拖垮 daemon。`eixd` 通过
stdin/stdout 的 newline-delimited JSON 与插件通信，协议版本是独立的
`PLUGIN_API_VERSION = 1`。每个 `[plugins.<name>]` 条目只启动一个 server 进程：

```yaml
plugins:
  echo:
    enabled: true
    manifest: examples/plugins/echo/eix-plugin.toml
    capabilities: [tools, commands, events, storage]
    timeout_secs: 30
```

`manifest` 指向 `eix-plugin.toml`，其中声明 `name`、`version`、`api_version`、
`capabilities` 和 `[server]` 入口。也可以直接写 `command` / `args` 跳过 manifest。
manifest 的 capability 是“申请”，配置里的 `capabilities` 是“授权”；配置可以收窄，
不能扩大 manifest 的申请。没有 manifest 且未写 `capabilities` 时默认只授予
`tools` + `commands`。

启动握手：

1. daemon 启动子进程，发送 `initialize`（协议版本、插件名、host 版本、授权能力、
   workspace、数据目录）。
2. 插件返回 `tools`、`commands`、`events` 和 `api_version`。
3. daemon 校验名称、能力、schema 和数量；未授权却声明贡献会启动失败，而不是静默放大权限。
4. 工具以 `plugin__<plugin>__<tool>` 暴露，命令以 `plugin/<plugin>/<command>` 合并进
   TUI 的 `/` 菜单；命令目前是提示词模板（`$ARGUMENTS` / `$1..$9`），action command 尚未启用。

插件工具**默认需要审批**（显式 `allow` 规则或 `bypass` 模式除外），风险类别为
`plugin`，执行前会抓工作区快照；`auto` 模式的分类器不会替插件工具放行。`plan` session
和 sub-agent 看不到插件工具。插件命令不会覆盖内置命令或用户 Markdown 命令。
`allowed_tools` / `denied_tools` 支持 `*` / `?`，deny 优先。

能力边界：

- `tools`：注册模型工具。
- `commands`：注册提示词命令。
- `events`：订阅 daemon 通知（如 `session.*`、`tool.completed`）。投递是 fire-and-forget，
  慢插件不会阻塞 turn。
- `storage`：使用 host 管理的 SQLite KV（`global` 或 `workspace:<path>`，按插件名隔离）。
- `hooks` / `rpc` / `ui` / `network` / `workspace_read` / `workspace_write` / `secrets`：
  协议中保留，当前 host 不提供对应 API。

**安全边界**：能力校验限制的是 host API，不是操作系统。插件仍以 daemon 用户身份运行，
可以直接读写该用户可访问的文件，因此只应安装可信插件。需要强隔离时应使用单独的
用户/容器运行 daemon；项目配置永远不能添加插件。

相关 RPC：`plugin.list`（状态、工具/命令数、错误）、`plugin.commands`（TUI 命令目录）、
`plugin.restart {name}`（重启单个插件）。`/reload` 会重新读取插件配置并重启全部插件。
`plugin.list`、工具目录和命令刷新时会检查子进程：意外退出会标记为 `failed`，移除它的
工具和命令，`plugin.restart` 可以重新拉起。daemon 正常退出时向空闲插件发送 `shutdown`
通知并短暂等待，不响应的插件会被强制回收。`storage` 每个插件最多 10,000 行 / 16 MiB，
超限的 `host.storage.set` 返回错误，需要删除旧键释放空间。完整示例见
`examples/plugins/echo/`。

### 19.5 HTTP API 与远程 attach

除 Unix socket / TCP 外，还可以只开一个 HTTP 端口给脚本、SDK 或 Web 前端用：

```yaml
server:
  http: 127.0.0.1:7778   # 或用 EIX_HTTP
  token: "..."           # 与 TCP 共用；开 http 时必填
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

默认只监听 Unix socket（0600，靠文件系统权限保护）。要跨机器用，在配置里打开 TCP：

```yaml
server:
  tcp: 127.0.0.1:7777    # 或用 EIX_TCP
  http: 127.0.0.1:7778   # 可选；或用 EIX_HTTP
  token: "..."           # 打开 tcp/http 时必填，非空
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

### 19.6 nvim 前端命令

```sh
eix nvim [--session ID]      # 也可用 EIX_NVIM 覆盖 nvim 可执行文件
```

`eix nvim` 用你的 Neovim 打开一个内嵌 Lua 前端（脚本写在 `<data-dir>/eix-nvim.lua`），
它自己用 libuv 连 Unix socket（或 TCP + token），说同一套换行 JSON-RPC；转录渲染进一个
`eix://<id>` 的 scratch buffer。`:Eix` 子命令：

| 命令 | 作用 |
| --- | --- |
| `:Eix attach [ID\|name]` | attach（省略则第一个 session，没有就新建） |
| `:Eix list` / `:Eix pick [KIND]` | 列出 session / picker（`KIND` 省略为 session；可选 `files`/`worktrees`/`branches`/`context`；files/branches/worktree 选中后插入光标处，context 在分屏显示条目详情） |
| `:Eix new [NAME] [worktree]` | 新建 session（可选独立 worktree） |
| `:Eix send TEXT` / `:Eix prompt` | 发送 prompt；`prompt` 开一个输入分屏，`<CR>` 发送、`@` 文件 picker |
| `:Eix fork [SEQ]` / `:Eix rewind [SEQ]` | 分叉 / 回滚并切到新 session |
| `:Eix stop` | 取消当前 turn |
| `:Eix ps` | 后台进程 |
| `:Eix mode [MODE]` / `:Eix doctor` | 执行模式 / 连接与版本诊断 |
| `<C-g>` / `<C-g><C-g>` | session picker / 回到上一个 session |
| `<leader>a` | 动作 palette（picker / list / new / worktree / file / branch / prompt / stop / fork / rewind / ps / mode / doctor） |

每个 Neovim 进程是一个独立 client，共享同一个 daemon；daemon 会串行化同一 session 的
turn，第二个 prompt 在忙时会被拒绝。attach 后前端会发 `session.subscribe`，只接收当前
session 的事件。`eix tui` 使用内置的 Vim 风格输入框，与这条独立前端互不影响。

## 20. 实现状态

以下清单记录当前已落地的能力，更早的阶段计划见 §16；这不是稳定 API 承诺，具体以代码和
`protocol.rs` 为准。

- [x] 设计文档 v0.1
- [x] workspace 骨架
- [x] 自研 Vim 风格输入框（normal / insert / visual、motion + operator + count、undo/redo、leader `<Space>xx`，无子进程）
- [x] daemon + OpenAI 兼容 provider（流式 SSE、取消、错误处理）
- [x] session 事件日志 + SQLite 持久化 + attach 协议（事件流广播）
- [x] `eix` CLI（chat / send / log / ls / new / rename / cancel / approve[--hunks] / deny / answer / doctor）
- [x] TUI v1（多 pane 自动平铺、焦点切换、转录、session 栏、fuzzy picker、多行输入、流式跟随）
- [x] 转录 Markdown 渲染（强调、代码、列表、引用、表格、链接、图片占位、Unicode 行内公式与 2D 块级公式）
- [x] 转录鼠标选择 + 松开自动复制（native clipboard helper，失败时 OSC 52；右上角短暂确认）
- [x] 工具调用 + 审批门禁（read_file / list_dir / write_file / run_command、workspace 沙箱、审批卡片）
- [x] 写入前 Vim 风格 side-by-side diff 预览（左旧右新、语法高亮、窄宽回退 unified；TUI 卡片、`d` 全屏审阅、`eix chat` / `eix log`）
- [x] stale-write 冲突检测（stage 后文件被外部修改 → `patch_conflict`，不会覆盖）
- [x] hunk 级审批（TUI `d` 审阅里勾选 hunk，daemon 按选择应用部分改动）
- [x] attention 标记（后台 pane 高亮、`a` 跳转、审批卡片重放）
- [x] 外部 attention 通知（`[notifications]` command + TUI 终端铃、冷却、重放）
- [x] bubblewrap 沙箱（`run_command` 默认隔离，`[tools] sandbox` 可配，写入只限 workspace）
- [x] staged shadow workspace（propose → stage → apply、合并读视图、`patch.apply` / `patch.reject`、跨重启恢复）
- [x] 执行前快照（write_file 单文件 / run_command 整个 workspace，`snapshot` 事件，保留 20 份）
- [x] 自动上下文管理（sector 预算 / token 记账、AGENTS.md + workspace 注入、自动 drop / pin / compact）
- [x] process view（daemon 托管后台进程、`start_command` / `process_output`、流式输出与日志、TUI `P`、`eix ps/tail/kill`）
- [x] fork / rewind（按消息裁剪历史复制到新 session；`eix rewind` 还会按快照倒序回滚工作区，并报告无法恢复的改动）
- [x] git worktree（默认共享工作区；`eix new --worktree` 新建隔离 session，TUI `/new-worktree` 把当前 session 移入 `.worktrees/<slug>`，`eix worktree list|remove`）
- [x] 集成队列（`ready` 工具 / `eix ready`、rebase → 测试 → base ff merge、冲突/测试失败注入修复、退避重试与 `eix integrate`）
- [x] 子 agent（`spawn_agent`、只读工具集、并发批次、深度上限 1、独立事件日志）
- [x] `ask_user`（2-6 个选项、首个为推荐项、阻塞式交互选择、`eix answer N` 无头回答）
- [x] daemon 化的 nvim 前端（内嵌 Lua 插件 + JSON-RPC、`:Eix` 命令、headless 测试）
- [x] 远程 attach（TCP + 强制 token、首请求 `auth`、CLI `--tcp/--token`）
- [x] 按连接过滤广播（`session.subscribe`，`null` = 全部；`eix nvim` 只订阅当前 session）
- [x] 审批增强（`e` 编辑 staged 内容、`o` 打开真实文件、`r` 带理由拒绝、`<Space>a` 跨 session 审批 inbox、风险标签）
- [x] 审批/提问卡片改为 Markdown checkbox 列表（`Ctrl-J/K` / `j/k` 选择、`Enter` 执行、`1-9` 直选、首项推荐；prompt 聚焦和 `tabby` 布局下按键优先给卡片）
- [x] `eix pick`（fzf/rofi 友好的按行输出）、`eix status [--json]`、`eix approvals`
- [x] 输入草稿持久化（`draft.get` / `draft.set`，按 session，重启/重连可恢复）
- [x] 客户端通知（后台 attention 在底部状态栏居中提示，可选 `bell`，不弹桌面通知）
- [x] 日志脱敏（`redact`：Bearer / `sk-` / 敏感键赋值）与 provider 上下行可逆脱敏
- [x] GitHub Actions CI（fmt / clippy `-D warnings` / test；无 nvim、git 时自动 skip）
- [x] session 执行模式（`plan` / `build` / `auto` / `bypass`；`session.mode` 支持临时切换并记录事件；旧库从 permission/agent 自动迁移）
- [x] 显式权限规则（`[[tools.rules]]`：工具名 + 路径 glob + argv 前缀 / 正则 / signature；deny 全局优先、按具体度选择、项目配置只能收紧）
- [x] 统一风险分析与审计（只读命令分析、硬 deny、network/secret 保护、`permission_evaluated` 事件、审批 scope / risk UX、`external_directory`）
- [x] `auto` 意图分类器（固定启用、使用 session 模型、内置 timeout / confidence / max risk / cache；只把默认 ask 降为 allow，失败回退询问；审计写入 `permission_evaluated.classifier`）
- [x] 自动压缩（超过 `auto_compact_threshold` 时总结旧 turn，保留最近 N 轮；`context_compacted` 事件）
- [x] 配置分层 + 占位符 + 热重载 + `cli.json`（全局 → 祖先 `.eix/config.yaml`、`{env:}` / `{file:}`、mtime 轮询 + `/reload`、项目默认 model/agent/mode）
- [x] session 管理（undo/redo、export/import、delete、archive、timeline、队列 `rm`/`move`；CLI `eix undo/redo/export/import/delete/archive/timeline`）
- [x] Skills（原生 `.eix` + 可配置 Claude Code / OpenCode / Codex CLI / Gemini CLI / Pi 兼容、system prompt 摘要 + `use_skill {name, arguments}`、`$ARGUMENTS` / `$1..$9`、`allowed-tools` 单 turn 限制、带预览的 `/skills` picker）
- [x] 写入后 formatter（`[[formatter]]` command/extensions/timeout；按顺序首个匹配；直接写入与 staged apply 都生效）
- [x] `todo` 任务列表 + `webfetch` / `websearch` 网络工具
- [x] 可配置 TUI 调色板（`[theme]`，颜色名 / hex / ANSI 索引）
- [x] HTTP API（`[server] http`；`/health`、`/stats`、POST `/rpc`、SSE `/events` + `session_id` 过滤，Bearer token、CORS/OPTIONS；`eix-sdk` 类型化 Rust client）
- [x] 图片附件（`eix send --image` / TUI `/image` / 粘贴拖入路径或 `file://`，OpenAI 多模态 content，SHA-256 blob 存储、保留/GC、export/import 往返）
- [x] 可配置全局 / TUI / leader 快捷键（`[keys] action = "C-x"` 或 `action = "leader N"`，含命名键/F1-F12；overlay 导航键固定）
- [x] LSP diagnostics（内置常见语言 server、懒启动 stdio client、写入后附诊断、`lsp_diagnostics`、`lsp.workspace` 全工作区扫描 RPC；可按语言覆盖）
- [x] prompt `!<cmd>` 直接执行 shell（sandbox + snapshot + 只读拒绝，保留 tool 事件）
- [x] 独立 Neovim 前端交互：`<C-g>` session picker、`<C-g><C-g>` 回上一个 session、`:Eix pick|doctor`
- [x] 输入框 `@` 打开文件 picker（`picker.query` 的 `files` provider）、两次 `Esc` 回转录区、长草稿自动增高（默认 3 行，总高上限 12 行）
- [x] 草稿镜像（编辑器变更同步到 `draft.set`，切 session 时从 daemon 回填）
- [x] 统一 picker 体系（单一 overlay + 右侧预览；`picker.query` 提供 files / grep / git / worktrees / branches，`o`/`Ctrl-P`/`g`/`s`/`@` 入口；独立前端 `:Eix pick [KIND]`）
- [x] 工具补齐：`grep` / `glob` / `edit` / `patch`，`read_file` 行号 + `offset`/`limit` 分页（只读工具自动放行）
- [x] 自定义 Markdown 命令（`~/.config/eix/cmds`、项目 `.eix/cmds`，兼容旧 `commands` 目录；`/name` 快速调用、`$ARGUMENTS` / `$1..$9`、frontmatter `description`/`agent`/`model`）
- [x] 自定义 Markdown agent（`~/.config/eix/agents`、项目 `.eix/agents`；`role`/`model`，`/agents` 选择 primary、`spawn_agent agent:` 选择 subagent）
- [x] 多 provider 模型目录（`providers.*` 命名 endpoint、自动拉取 `/models`、`models[]` 手动补充成本/变体、`/model` 搜索切换；token usage 与成本记录在事件日志，不在状态栏常驻）
- [x] MCP stdio client（`mcp.servers.*`、懒启动、`tools/list` / `tools/call` / `resources/*` / `prompts/*`、分页、`list_changed` 刷新、`allowed_tools` / `denied_tools`、`mcp__server__tool` 审批 + 树快照、`/mcp` 状态）
- [x] 外部插件基础（`plugins.*`、`eix-plugin.toml`、`initialize` 握手、`plugin__*` 工具审批 + 快照、`plugin/*` 提示词命令、`session.*` 事件订阅、SQLite 插件存储、`plugin.list` / `plugin.commands` / `plugin.restart`；TUI widget host 与 action command 尚未实现）
