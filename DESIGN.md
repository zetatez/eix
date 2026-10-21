# eix — AI Agent 交互设计

> 状态：v0.2（2026-10-18，spike 后修订）
> 定位：面向 vim / dwm / Arch 工作流的 coding agent；本文件是交互与技术契约，随实现修订。
> 名字：`eix`，取自欧拉公式 `e^{ix} = cos x + i·sin x`；加泰罗尼亚语中意为「轴」。
> v0.2 变更：命名统一为 `eix`（`g:eix_prompt` / `:Eix` / `eix pick`）；补充实测的 redraw/linegrid 契约；修正 `<leader>ac` 键位冲突；加入实现状态快照。

## 1. 目标与非目标

**目标**

- 与 codex 同级的 coding agent（工具调用、补丁、审批、上下文管理）。
- 多 session 是一等公民：常驻、可 attach/detach、可并行、可模糊切换。
- 输入框是**自研的 Vim 风格编辑器**：常用 vim 键位与编辑语义，无子进程、无配置加载。
- fzf 式体验贯穿所有选择面：session、文件、历史、命令、diff hunk。
- 与 dwm / tmux / rofi / dmenu / dunst 自然协作，不要求换工作流。

**非目标（v1）**

- GUI / Web 前端；多人实时协作；插件市场；Windows / macOS 优先。

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
| leader | `<Space>`（输入框 normal / 转录区） | 单字母全部留给 vim；转录区动作也必须经过 leader | insert 下不可用，用 `<C-g>` 补充 |
| 转录 | ratatui 自绘 + 对话式布局 + 可折叠块 | 流式 reasoning/工具/审批卡片可见，性能可控 | 需维护一套只读导航键位 |
| 审批 | propose → stage → apply | hunk 级审批才有意义；可回滚 | 需虚拟工作区/补丁暂存 |
| 上下文 | 可见且可编辑的 context ledger | 既是 UX 也是回答质量 | 中等额外工作量 |

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
- **元数据**：自动命名（首条 prompt 或 branch）、标签、pin、frecency。

### 4.2 状态机

```
idle ──submit──> running ──tool──> tool_running ──> running ──done──> idle
                   │                    │
                   ├──approval──────────> awaiting_approval ──respond──> running
                   ├──question──────────> awaiting_input ──submit──> running
                   └──error─────────────> error
```

状态符号：`○ idle`、`● running`、`⚠ approval`、`◐ waiting`、`✗ error`、`◌ paused`。
输入框顶部对 idle / running 只显示图标：idle 为静态 `○`，running 为逐帧切换的 braille spinner。

### 4.3 attach / detach / 所有权

- session 常驻 daemon；client attach 只是订阅 + 渲染。
- 同一 session 可被多 client 观察，但**只有一个输入所有者（controller）**。
- 所有者断开 → 自动转移给最早的 watcher；显式抢占用 `<leader>a C`（claim，避免与 context ledger 的 `<leader>a c` 冲突）。
- draft 由 daemon 持有，client 的 nvim 只是视图；所有权切换时重新加载 draft。

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
  TUI ledger `w`、CLI `eix rewind [--session ID] [--at SEQ] [--name NAME]` 走 rewind，`F` / `f` 仍是纯 fork。
  fork 在 git 仓库里默认同时起 worktree，避免共享 cwd 的并发冲突（见下）。

### 4.6 worktree

- 默认绑定 cwd；`--worktree`（`eix new --worktree`、TUI `N`）或 picker 中一键创建 `.worktrees/<slug>`。
- repo 内 fork 默认给新分支建 worktree；`eix fork --shared` / `session.fork worktree=false` 显式共享源工作区。
- worktree 与 session 生命周期解耦：session 归档不自动删除 worktree，需显式确认。
- 已实现（2026-10-18）：`eixd/src/worktree.rs` shell out 给 git，`git worktree add -b eix/<slug> .worktrees/<slug>`；
  main repo 用 `git rev-parse --git-common-dir` 解析并 canonicalize（支持 subdir 和 linked worktree），
  `.worktrees` 已在快照排除表里。删除走 `eix worktree remove SESSION [--force]`（`session.worktree.remove`）：
  先算出主仓库，`git worktree remove [--force]`，成功后把 session cwd 重置回仓库根并清空 worktree 字段；
  工作区有未提交改动时 git 会拒绝，除非显式 `--force`（dirty 删除返回 `removed:false` 并给出提示）。

## 5. 输入层：自研 Vim 风格编辑器

这是交互的核心。输入框**不启动子进程、不加载用户 Neovim 配置**，而是 `eix-tui` 内的
一个模态编辑器（`crates/eix-tui/src/tui/editor.rs`），按键直接改本地 buffer。只实现写
prompt 需要的常用 vim 子集，换取零启动成本与零按键往返。

### 5.1 模式与按键

| 模式 | 进入 | 主要按键 |
|---|---|---|
| insert | `i a I A o O` / 聚焦输入框 | 文本输入、`Backspace`/`Delete`、方向键、`Home`/`End`、`Tab`（4 空格）、`Ctrl-w/u/k/a/e`；`Ctrl-J` / `Ctrl-Enter` / `Shift-Enter` 换行；`Enter` 提交 |
| normal | `Esc` | `h j k l w W b B e E 0 ^ $ gg G f F t T ; ,`；`d c y` + motion / `dd cc yy` / `iw aw`；`x X D C s S r ~ J p P u Ctrl-r`；`v`/`V`；`Tab` 切换 Build / Plan |
| visual / v-line | `v` / `V` | 同 normal 的 motion；`d x y c p ~ o`，选区反色 |

- 计数（`3w`、`2dd`）、preferred column（`j/k`）、`;`/`,` 重复 find 均按 vim 习惯实现。
- 撤销是快照栈：一次 insert 会话、一次 `dd`/`p` 各算一步；`u` / `Ctrl-r`。
- 未命名寄存器保存最近一次 yank/delete；`p`/`P` 区分字符与整行；寄存器为空时读取
  系统剪贴板（鼠标选中的转录文本可直接粘贴），且外部粘贴不覆盖内部寄存器。
- `<Space>` 是 leader（输入框 normal 与转录区共用同一映射）：`s` sessions、`n` 新建 session 并开新 pane、
  `o` 打开 session picker（已有 session 放进新 pane）、`c`/`q` 关闭 pane、`f` 文件、
  `w` worktree、`b` 分支、`C` ledger、`h` 历史、`p` 进程、`r` 审批、
  `N` 新建、`W` 新建 worktree、`F` fork、`x` 取消、`A` attention、`i` 聚焦输入框、
  `a`/`:` 命令面板、`?` 帮助。单字母基本留给 vim；转录区裸字母不触发动作，仅保留
  `i`（聚焦输入框）、`Z`（退出）、`j/k`/`g/G` 等导航和 `:`/`?` 等标点入口；布局动作
  同样走 leader（`v`/`t`/`r`/`l`）。
- `Shift+,` / `Shift+.`（即 `<` / `>`）在转录区和输入框 normal 模式线性切换上一个 / 下一个 pane。
- `Ctrl-J`/`Ctrl-K` 上下翻转录历史（`j`/`k`、`3j`、`dj`、`↑`/`↓` 是草稿内行移动）。
- normal 模式下 `Tab` 在 Build / Plan 之间切换：`None` 表示默认 Build，`Some("plan")`
  选择只读 Plan；输入框标题行显示 `Build` / `Plan`。本地立即更新以支持连续切换，daemon 的
  session 事件负责最终校正。
- `Esc`：insert/visual → normal 并保持焦点；normal 下 500ms 内再按一次回到转录区。
- `Enter` 只在 insert 提交；normal 的 `<CR>` 是 vim 的“下一行”。
- `@`（insert，行首/空白后）打开文件 picker；`Ctrl-G` 在任意模式打开 session picker。
- picker / 历史搜索：`Ctrl-J`/`Ctrl-K`（或 `↑`/`↓`）上下选择。
- 不再有 `[input] backend` / `minimal` 配置，也没有 nvim 回退路径。

### 5.2 buffer 与 draft

- 一个 client 一个 buffer，随聚焦 pane 切换；切换时先保存旧 session 的 draft，再
  `draft.get` 回填新 session。
- 编辑变化在 200ms tick 内同步到 `draft.set`（只写真实变化）；提交失败会把 draft 放回。
- 粘贴：bracketed paste 整段插入，`\r\n` / `\r` 归一为 `\n`。

### 5.3 输入条布局

```
╭─ Build · ○ deepseek-v4-flash         ~/git/eix  main ─╮
│   把登录改成 JWT，并补测试                               │
│                                                           │
│                                                           │
╰───────────────────────────────────────────────────────────╯
 N  prompt polish · running                     gpt-5 · 1/2
```

- 最少 6 行内容 + 上下边框；短终端优先让给转录区；长草稿向上生长到 12 行后开始滚动。
- 顶部左侧固定为 `Build/Plan · <状态图标> <model>`（Build 蓝色、Plan 黄色），右侧对齐 cwd、git 分支/工作树和图片数；
  idle 显示 `○`，running 显示动画 spinner；其他状态显示符号和状态名；
  Build/Plan 固定 5 列宽，切换时状态和模型不位移。不再显示 `eix` 品牌；拒绝理由会临时占用这一行，
  错误则显示在居中的浮动对话框里，按 `Esc` / `Enter` / `q` 关闭，输入框顶部保持 Build/Plan 与 cwd 信息。
- 输入框下方保留一条 Vim 风格 statusline：左侧是彩色的单字母模式徽标（`N` / `I` / `V` / `VL`）、
  当前 session 名和最近状态；中间显示后台 attention 提醒；右侧是模型、多 pane 时的
  `当前/总数`、排队数和 attention 数。
- 输入框底边保持普通边框，不再挂模式徽标，也不堆叠快捷键提示。
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
  `fork [SEQ]` / `rewind [SEQ]` / `stop` / `ps` / `ledger` / `permission [LEVEL]` / `doctor`。
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
- 布局模式：`grid`（默认自动平铺）、`horizontal`（单列，队首占 55% 高度）、`tabby`（顶部 tab，
  只渲染当前 session）、`fibonacci`（tab + 左/上/右/下循环切分的螺旋分屏）。用 leader
  `<Space>v/t/r` 切换，`<Space>l` 循环；`Ctrl-1` … `Ctrl-9` 聚焦第 N 个已打开 session，
  在 `tabby` 下即切换到第 N 个 tab。TUI 启动时请求 kitty 键盘协议（CSI-u），否则传统终端
  会把 `Ctrl-2`/`Ctrl-3` 等编码成 NUL/Esc；不支持该协议时用 `Alt-1` … `Alt-9`。
- 活跃队列：`views` 顺序即队列顺序，位置 0 是 pane 1，也是当前工作槽位。turn 完成、等待
  审批/输入时把对应 session 移到第二个位置（position 1，即 pane 2），避免替换 pane 1 正在
  编辑的内容；提交流程（包括排队和 `!` shell）把它移到队尾。重排时保持聚焦槽位，因此发送后
  第二个位置的 session 会补到 pane 1；若被移动的正是聚焦 session，promotion 会跟随到
  第二个位置，demotion 则留在原槽位等待下一个 session。
- 不再有独立顶部 header 行：Build/Plan、session 状态、模型和 cwd 作为输入框标题行，直接压在
  prompt 网格上方（cwd 用 `~` 缩写，模型省略 provider 前缀）；输入框下方保留一行 statusline，
  左侧显示模式、session 和最近状态，中间显示 attention 提醒，右侧显示模型与队列/attention 信息。
- 焦点：`<C-w>h/j/k/l`（normal 模式与转录区一致）、`<C-w>w` 循环；转录区按 `i`、`Enter` 或 `<Space>i` 聚焦输入并进入 insert。
- `Esc`：insert/visual 下先回到 normal 并保持输入框焦点；normal 模式下 500ms 内连按两次才把
  焦点交回转录区（已实现）。
- 输入条最少显示 6 行内容（加边框；短终端优先让给转录区），长草稿会向上生长到 12 行，让长命令
  和换行文本尽量完整可见；超过上限后在框内滚动。左侧不再有常驻 session 栏，session 只经浮动 picker 打开。
- 输入条复用转录区的圆角面板与低对比度调色：顶部左/右分栏显示 `Build/Plan · status model` 与
  cwd、git 分支/工作树/图片数；正文左侧留 1 格内边距；底边保持普通边框，模式与状态信息移到下方
  statusline（单字母模式徽标 `I` 绿 / `N` 蓝 / `V` 琥珀），不堆叠快捷键提示或占位文案。
  `/` 补全菜单使用同一套面板样式，选中行整行高亮。

## 7. 键位总表

> 内置编辑器与转录区共用 `<Space>` leader（输入框 normal 模式 / 转录区）；`<C-g>` 是跨模式补充入口。
> `eix nvim` 前端沿用你的 `mapleader`（下表以 `<Space>` 为例）。

**全局（转录区 / picker / 任意非输入焦点）**

| 键 | 行为 |
|---|---|
| `<C-g>` | session picker（直达） |
| `<C-g><C-g>` | 回到上一个 session |
| `<C-w>h/j/k/l` | 切换 pane 焦点 |
| `<C-w>w` | 循环 pane |
| `<Space>v/t/r` | horizontal / tabby / fibonacci 布局 |
| `<Space>l` | 循环 grid / horizontal / tabby / fibonacci |
| `<Space>R` | 跨 session 审批收件箱 |
| `Ctrl-1…9` | 聚焦第 N 个已打开 session（`tabby` 下为第 N 个 tab；需 kitty 键盘协议） |
| `Alt-1…9` | 同上；终端无法区分 `Ctrl-1…9` 时使用 |

**`eix nvim` 前端（`:Eix` palette）**

| 键 | 行为 |
|---|---|
| `<leader>a s` | session picker |
| `<leader>a n` | 新建 session |
| `<leader>a f` | fork 当前 session |
| `<leader>a a` | attention 队列（下一个待审批/待输入） |
| `<leader>a p` | 命令面板（搜一切动作） |
| `<leader>a d` | diff 审阅 |
| `<leader>a c` | context ledger |
| `<leader>a e` | 用真 nvim 编辑当前 draft |
| `<leader>a x` | 取消当前生成 |
| `<leader>a r` | 审批收件箱（跨 session） |
| `<leader>a q` | detach |
| `<leader>a ?` | 帮助 / cheatsheet（打开后输入即筛选条目，`↑`/`↓` 选择，`Ctrl-U` 清空，`Esc` 关闭） |

**输入条（内置编辑器）**

| 键 | 行为 |
|---|---|
| `<CR>` | 提交（仅 insert）；turn 运行中提交会进入该 session 的队列，当前 turn 结束后按序发出 |
| `/命令` | 整条消息作为 prompt 命令处理，不发给模型（见下表） |
| `<C-j>` / `<C-Enter>` / `<S-Enter>` | 换行（仅 insert） |
| `@` | 文件 picker（行首/空白后） |
| `Esc` | insert/visual → normal；normal 下 500ms 内再按一次：有 turn 在跑时中断它，否则回到转录区 |
| `Ctrl-J` / `Ctrl-K` | normal 模式下浏览已提交的 prompt 历史（newer / older） |
| `Tab` | normal 模式下切换 Build / Plan 模式（命令面板的 `Build / Plan` 同义） |
| `j` / `k` | normal 模式下在草稿内上下行移动（`3j`、`dj`、方向键同样可用） |
| `<Space>s/f/w/b/c/C/h/p/R/N/W/F/v/t/r/l/x/q/Q/i/A/a/:/?` | TUI 动作（见 5.1）；裸字母不触发动作 |
| `i a I A o O x X D C s S r ~ J p P u C-r` | 常用 vim 编辑 |
| `d c y` + motion / `dd cc yy` / `iw aw` | 操作符 |
| `v V` + `d x y c p ~ o` | visual |

**prompt 命令**（输入框里以整条消息发送的 `/命令`；输入 `/` 时在输入框上方弹出补全菜单，
`↑↓`/`C-n`/`C-p` 选择、`Tab` 补全、`Enter` 执行）

| 命令 | 行为 |
|---|---|
| `/help [query]` | 可搜索帮助（命令 / 快捷键 / notes），可带初始查询 |
| `/keys [query]` | 只搜索快捷键章节 |
| `/new [name]` | 在聚焦 session 的 cwd 新建 session 并打开 pane |
| `/new-pane` | session picker，把已有 session 放进新 pane |
| `/new-worktree [name]` | 在独立 git worktree 新建 session |
| `/clear` | 清空模型上下文（事件日志与转录保留），追加 `ContextCleared` 事件 |
| `/compact [notes]` | 总结上下文；省略 from/to 时压缩 `message_range()`，`generate: true` 让模型写摘要 |
| `/btw <question>` | 一次性旁支问答，不写事件日志；`btw.delta` / `btw.completed` 通知，`btw.cancel` 中断 |
| `/cd [path]` | 切换 session cwd（`session.cd`，canonicalize 后校验在 worktree 内） |
| `/init` | 生成 AGENTS.md 初始化 prompt 放进输入框，供审阅后发送 |
| `/queue [clear]` | 查看/清空本 session 的待发送队列 |
| `/sessions` / `/fork` | 打开 session picker / fork 当前 session |
| `/files` / `/worktrees` / `/branches` | picker；选中后插入路径 / worktree / 分支名 |
| `/context` / `/ledger` / `/history` | context picker / 完整 ledger / 历史搜索 |
| `/processes` / `/approvals` / `/attention` | 进程 / 审批收件箱 / 下一个 attention |
| `/mcp` / `/todos` | MCP server picker / 任务列表 picker（均可搜索和预览） |
| `/cancel` / `/toggle-agent` | 取消运行 / 切换 Build·Plan |
| `/permission <level>` | 设置 `read-only` / `workspace-write` / `full` |
| `/exit` / `/quit` | 保存草稿并退出整个 eix 客户端（不关闭 session pane） |
| `/close` | 保存草稿并关闭当前 session；如果它是最后一个 pane，同时退出 eix |

队列按 session 保存在 TUI 内存（`HashMap<SessionId, VecDeque<String>>`），tick 时空闲即取一条
发送；`turn.completed` 与 daemon `turns` 移除之间的竞态用“出错重排到队首、下个 tick 重试”处理。

**转录区**

转录按对话渲染：用户消息和 agent 输出都使用普通文本、左对齐并保留 2 空格缩进，不使用气泡，
不使用 `you>` / `eix>` 前缀。agent 的 reasoning（provider 的 `reasoning_content` / `reasoning`）
以 `Thinking` 模块流式展开，turn 结束后默认保持展开（可点击折叠）；工具调用按 `call_id` 合并
call/approval/patch/result 事件，一行显示状态圆点、命令或路径摘要与右侧状态
（`ok` / `running` / `needs approval`），展开后用四空格左缩进直接显示参数、输出和 diff；
diff 采用 Vim 风格左右分栏（左旧右新），按文件扩展名用 syntect 做语法高亮，窄 pane 回退
unified。命令与路径已由折叠摘要给出，不再重复 `input` / `output` 标签；摘要因宽度截断时，
展开后会在正文补回完整命令。
`thinking` 与工具块都可用鼠标点击折叠；排队中的 prompt 以 `⏳ queued` 行显示在转录末尾。
消息正文（用户消息与 agent 输出）经 `tui/markdown.rs` 渲染：pulldown-cmark 解析 CommonMark
与 GFM 表格/任务列表/删除线，转成保留样式的 `Line`；链接显示目标地址，图片显示
`🖼 alt (path)` 占位，`$...$` / `$$...$$` 数学公式：行内公式转成紧凑 Unicode，块级公式用
2D Unicode 网格排版（分数横线、根号、上下极限、矩阵、可伸缩括号），不引入 TeX 排版引擎。
鼠标按住左键拖拽可选择转录文本（反色高亮），松开后自动复制；优先调用
`wl-copy` / `xclip` / `xsel` / `pbcopy`，不可用时回退到 OSC 52。
`/theme` 打开带实时预览的主题 picker：上下移动立即重绘界面，`Enter` 应用，`Esc`
恢复打开前的主题；不再通过 `/theme <name>` 直接切换。预设里的 `background` 会同时应用到
转录区、主题 picker 和其他浮层；默认主题和没有 `background` 的自定义主题继续使用终端背景。
输入框不再使用独立的 `panel` 填充，始终跟随主题 `background`（默认即终端背景）。
转录渲染按 pane 缓存已存储事件：只有 live tail 和当前可见窗口每帧重绘，长会话的滚动、
选择与流式输出不会反复解析整段历史；事件、宽度、主题或折叠状态变化时才重建缓存。
attach 时只取最近 1000 条事件；滚动到顶部时按 `before_seq` 加载更早的分页，合并后按新增行数
校正 scroll，避免视口跳动。

| 键 | 行为 |
|---|---|
| 鼠标左键 / 滚轮 | 聚焦 pane；拖拽选择转录文本并在松开时自动复制；点击 `Thinking` / 工具块折叠或展开；滚轮滚动转录 |
| `j/k`、`<C-d>/<C-u>`、`gg/G`、`<C-f>/<C-b>` | 滚动 |
| `/`、`n/N` | 搜索 |
| `za`、`zR/zM` | 折叠当前/全部 |
| `Tab` / `<S-Tab>` | 下一个/上一个可交互块 |
| `<CR>` | 展开或执行当前块 |
| `y` / `Y` / `v` | 复制当前块 / 整条消息 / 可视选择 |
| `gf` / `gx` | 打开路径 / URL |
| `i` | 聚焦输入（insert） |
| `<leader>a…`、`<C-g>`、`<C-w>…` | 同全局 |

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
| `#` | 切换到全文搜索（跨 session 历史） |
| `status:` / `branch:` / `tag:` / `cwd:` | 字段过滤 |
| `Esc` | 关闭 |

## 8. Picker 规格

统一的 `查询 → 过滤 → 预览 → 动作` overlay，provider 可插拔：

- 规划：sessions / files(`@`) / history / commands / models / worktrees / branches / diff hunks / context items。
- 已实现（2026-10-18，统一 picker 体系）：
  - TUI 用单一 `Picker`（`kind` + `query` + `selected` + `items`）承载所有 provider，左侧列表、右侧预览
    （session 显示 status/cwd/worktree/permission/model；data provider 显示 daemon 给的 `preview` 文本）。
  - 入口：`<Space>s`/`<C-g>`（sessions，切换 pane）、`<Space>o`（sessions，新 pane）、`:`（commands，命令面板；转录区与输入框 normal 模式都可用）、
    `<Space>w`（worktrees）、`<Space>b`（branches）、prompt 内 `@`（files）、`Eix pick` 经独立前端选择。
  - provider 由 daemon 的 `picker.query {kind}` 提供：`files`（workspace 相对路径）、`worktrees`
    （`git worktree list --porcelain`）、`branches`（`git branch`）、`context`（session ledger 条目，
    值为 entry id）。默认 500、上限 5000；跳过 `.git` / `target` 等派生目录。
  - 动作：files → 在 prompt 光标处插入 `@path`；worktrees/branches → 插入对应值；sessions → 切换/新 pane；
    context → 打开 ledger；commands → 执行（session / files / worktrees / branches / ledger / search /
    processes / approvals / new / fork / cancel / help / quit）。
  - 独立 nvim 前端：`:Eix pick [sessions|files|worktrees|branches|context]`（`sessions` 为默认），
    文件/分支/worktree 选中后插入光标处，context 选中后在 scratch 分屏显示条目详情。
- 仍待做：history 仍走独立的 `#` 全文搜索视图（未并入 picker）；diff hunks provider、
  多选（多文件插入、多 session 开 pane、多 hunk 审批）、frecency 排序与 `#` 匹配度模式、外挂
  `agent pick`/`fzf`、file 语法高亮预览。
- 已补（2026-10-19）：`/models` 打开客户端生成的模型 picker（含 `id#variant` 行），
  `/agents` 打开 primary agent picker；两者都复用同一个 overlay，数据来自 `model.list` /
  `agent.list`，选中后发 `session.model` / `session.agent`。
- 模糊匹配用 `fuzzy-matcher`（skim 算法）；不做自研打分。

## 9. 审批与工作区

### 9.1 执行模型

- **propose → stage → apply**：agent 的写操作先进入 staging 视图（补丁/影子目录），用户批准后才落到真实工作区。
- agent 的读操作看到的是 `真实工作区 + 已 stage 改动` 的合并视图，避免"自己改了什么自己不知道"。
- shell 命令无法 stage：写操作走审批门禁 + 执行前快照（git stash 或目录快照），风险等级单独标注。
- 只读 shell 命令（`grep` / `rg` / `fd` / `find` / `cat` / `head` / `tail` / `wc` /
  `ls` / `stat` / `diff` 等）走保守白名单，不弹审批、不抓快照。重定向、后台运行、
  命令替换、子 shell、`find -exec` / `-delete`、`rg --pre`、`fd -x` 以及所有未知命令
  仍按写操作处理，避免白名单被 shell 语法绕过。

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
- `A` 打开跨 session 审批 inbox；`eix approvals [--json]` 是无头等价物。

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

- 级别（已实现，按 session）：`read-only` / `workspace-write` / `full`，存于 `sessions.permission`
  （旧库自动迁移，默认 `workspace-write`）。`session.create` 可带 `permission`；
  `session.permission` 可在运行时切换，`temporary: true` 只存内存并在事件日志留下
  `permission` 事件，daemon 重启后回到持久值。
  - `read-only`：不向模型暴露写工具，运行时拒绝 `write_file` / `start_command` 和会修改文件的
    `run_command`；白名单内的只读 shell 命令仍可执行（例如用户显式 `shell.run cat ...`）。
  - `workspace-write`：仍受全局 `[tools] policy` 的 `ask` / `auto` 约束，路径限制在 workspace 内。
  - `full`：跳过审批直接执行写工具（路径限制仍作为纵深防御保留）；全局 `read-only` 仍是硬上限。
  - 子 agent 永远只读，与父 session 的级别无关。
- 全局 `[tools] policy`（`ask` / `auto` / `read-only`）仍作为默认值与上限。
- 自动批准规则（已实现）：`[[tools.auto_approve]]`，字段 `tool` + `path` glob +
  `command_prefix` / `command_regex`（全部给出时是 AND）。只放宽 `ask`；全局或 session
  的 `read-only`、子 agent 只读仍是硬上限，`full` 不经过规则。`path` 先做词法归一化，
  绝对路径和 `..` 逃逸不会匹配；`command_prefix` 匹配整条 shell 字符串，需要更严格时用
  `command_regex`。只读工具本来就不进审批门禁。

## 10. Context ledger

`<leader>a c` 打开全屏 ledger，按 sector 分组：

| sector | 内容 | 预算 |
|---|---|---|
| system | 系统提示 | 5% |
| instructions | AGENTS.md、项目约定 | 5% |
| workspace | 文件树、git 状态、选中文件 | 15% |
| history | 对话历史 | 60% |
| tools | 工具输出（可截断/引用 blob） | 10% |
| memory | 跨 session 笔记/事实 | 5% |

- 操作：`dd` 删除、`p` pin、`o` 展开、`s` 压缩、`/` 搜索、`y` 复制、`<CR>` 打开来源。
- 每条显示 token 成本；底部显示总预算与剩余。
- 压缩是**显式事件**：历史里出现 `compacted 12 messages → summary`，可展开查看原文。
- 删除/pin 是 session 级策略，可保存为项目默认。

## 11. 事件与协议（草案）

**daemon → client（notification）**

- `session.created|updated|status`、`session.attention`
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
- `session.worktree.remove {session_id, force?}`
- `session.subscribe {session_ids?}`（连接级广播过滤；`null` = 全部）
- `prompt.submit|cancel|queue`
- `approval.respond|list`、`input.respond`、`patch.apply|reject|get|revise`
- `draft.get|set`（按 session 的未发送输入；空文本删除）
- `context.drop|pin|compact`
- `process.list|output|kill`
- `search.query`（跨 session 全文搜索；`session_id` 可限定范围）
- `picker.query {kind, session_id?, query?, limit?}`（provider：`files` / `worktrees` / `branches` / `context`；
  返回 `{value, label, detail?, preview?}` 行；`context` 的 value 是 entry id）
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

## 12. 持久化与搜索

- SQLite（WAL）：`sessions`、`events`、`messages`、`drafts`、`approvals`、`patches`、`frecency`。
- 大输出/补丁走内容寻址 blob；prompt 图片走 `<data>/blobs/<sha256 前两位>/<sha256>`（自动去重、保留/GC 策略见上文）。
- FTS5：
  - 主索引 `trigram`（中英混排友好）。
  - **短查询（1–2 字）降级 LIKE 扫描**，先收窄到当前/最近 session；否则中文高频词搜不到。
  - 可选自研 bigram tokenizer 作为后续优化。
  - 已实现（2026-10-18）：`search_docs` 内容表 + `search_fts` 外部内容虚表，触发器随
    `append_event` 同步；启动时补索引。短查询用 `instr(lower(...))`（等价于不带通配符的
    LIKE，且不需要转义 `%`/`_`），默认不做 session 收窄——消息量还小，先保证能搜到；
    等索引变大再加“当前/最近 session 优先”。
- 文件权限：目录 0700、文件 0600；日志写入前做密钥脱敏（API key、token、`.env` 内容）。
- 事件日志天然是回放测试夹具：record/replay 用于集成测试。

## 13. 通知

- TUI：后台 attention 提醒显示在底部状态栏中间（约 8 秒），可选终端 bell；不弹桌面通知。
- 桌面：`notify-send`（dunst），可通过配置替换命令。
- dwmblocks：`agent status --json` 输出 `{running, approval, error}`。
- 仅在状态跃迁时通知；当前聚焦 session 不通知；冷却时间可配。

## 14. 安全

- 工具沙箱：bubblewrap。工作区 rw，`/tmp` tmpfs，home 只读（缓存目录按需 rw），网络开关。
- 环境变量白名单；密钥不进入 daemon 日志。
- 发往 provider 的消息在 `Provider::stream_chat` 边界做可逆脱敏：密码、API key、token、
  Bearer、URL 凭据、PEM 私钥和敏感赋值替换为 per-provider salt 哈希出的占位符；模型返回的
  tool-call 参数和文本在本地执行/持久化前还原。映射不离开进程，事件日志和 TUI 保留原文。
  图片不做文本脱敏。
- 网络/破坏性命令默认需要审批；只读 shell 命令的白名单见 §9.1。
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
> 实际协议以 `crates/eix-core/src/protocol.rs` 为准：Phase 0 的事件通知按连接广播
> （0600 socket 是边界），`detach` 等同于断开连接；转录渲染暂由 CLI 完成，TUI 与工具审批尚未开始。

**Phase 1**

- 多 session、picker、pane 分屏、attention 队列、通知、dwm/tmux 集成。

> 实现快照（2026-10-18）：多 pane TUI 已落地（`eix` 无命令默认打开，或 `eix tui`）。左侧 session 栏 + 右侧自动
> 平铺 pane（1/2/3/4+ 分别全屏、左右、左大右二、网格），每个 pane 独立维护 session、
> 转录、滚动与流式跟随；`<Space>o` 在新 pane 打开，`<Space>s`/`Ctrl-G` 切换聚焦 pane 的 session，
> `C-w h/j/k/l/w` 切焦点、`C-w v/s` 分屏、`C-w c` 关闭；同一 session 只出现一次，
> picker 选中已打开 session 会聚焦既有 pane。输入框是自研的 Vim 风格模态编辑器
> （`tui/editor.rs`）：normal / insert / visual / visual-line，常用 motion + operator +
> count、`u`/`Ctrl-r`、未命名寄存器；不启动子进程、不加载 nvim 配置，按键零往返。
> `<CR>` 只在 insert 提交，`<C-j>`/`Shift-Enter` 换行；输入框最少 6 行内容高（加边框），
> 长草稿最多增长到 12 行后在框内滚动，提交失败会把 draft 放回 buffer。
> 后台 pane 的 attention 已接入：daemon 在进入 `awaiting_approval` / `error` 时发送
> `session.attention`，TUI 标记边框/侧栏并支持 `<Space>A` 跳到下一个；重新 attach 会从事件日志
> 恢复未完成的审批卡片。外部通知也已接入：`[notifications] command` 由 daemon 在 attention
> 时用 `sh -c` 执行并传入 `EIX_SESSION_ID` / `EIX_SESSION_NAME` / `EIX_SESSION_CWD` /
> `EIX_STATUS`，同一 session 有 `cooldown_secs` 冷却；TUI 内提醒显示在底部状态栏中间，
> 可用 `bell = true` 额外响终端铃。
> 输入框补齐（2026-10-18）：`@`（词首）打开 `picker.query` 的 files provider，选中后以
> `@<相对路径>` 插入；编辑变化按 200ms tick 镜像到 `draft.set`，切 session 时 `draft.get` 回填。
> 独立前端补上 `<C-g>` session picker、`<C-g><C-g>` 回上一个 session、`:Eix pick [KIND]` 与 `:Eix doctor`。
> 统一 picker（2026-10-18）：TUI 用单一 overlay 承载 sessions / files / worktrees / branches /
> context / commands，左侧列表右侧预览；`<Space>s`/`<Space>o`/`:`/`<Space>w`/`<Space>b`/`@` 为入口，files 插入 `@path`，
> worktrees/branches 插入值，context 打开 ledger，commands 直接执行。daemon `picker.query` 新增
> `worktrees`（porcelain）、`branches`（`git branch`）、`context`（ledger 条目）三个 provider；
> 独立前端 `:Eix pick files|worktrees|branches|context` 用 `vim.ui.select` 选择。
> 输入焦点（2026-10-18）：`Esc` 只在 insert/visual 下回到 normal 并保留焦点，500ms 内
> 再按一次才回到转录；`<CR>` 只在 insert 提交；`<C-g>` 在任意模式打开 picker；
> `Ctrl-J` / `Ctrl-Enter` / `Shift-Enter` 只在 insert 换行；`F1` 全局帮助，`Ctrl-C` 在输入框内
> 等同 `Esc`（中断 insert / 失焦），在其它焦点上仍直接退出。
> 布局（2026-10-18）：移除左侧常驻 session 栏，pane 占满 body；历史 session 只经浮动
> picker（`<Space>s`）打开。normal 模式新增 `<Space>xx` leader 动作
> （复用命令面板动作），单键编辑命令全部留给编辑器。

**Phase 2**

- 工具执行、staging、审批、diff hunk、沙箱、context ledger、process view。

> 实现快照（2026-10-18，进行中）：工具调用与审批门禁已落地。daemon 暴露
> `read_file` / `list_dir`（只读，自动放行）和 `write_file` / `run_command`（需审批），
> 所有路径经 workspace 沙箱（拒绝 `..` 与 symlink 逃逸），输出按字节截断，
> `run_command` 带超时。provider 侧支持 OpenAI `tools` / `tool_calls` 的流式增量累积，
> 一次 turn 最多 64 轮工具往返；每轮写入 `tool_call` / `approval_required` / `tool_result`
> 事件并广播 `tool.approval_required` / `tool.started` / `tool.completed`。TUI 渲染工具卡片，
> 聚焦 pane 有 `[y]/[n]` 审批卡片，`eix approve/deny` 可无头处理（`--hunks 0,2` 只应用
> 选中的 hunk）。
> 策略：`[tools] policy = ask|auto|read_only`（默认 ask）。`write_file` 在 stage 时生成
> unified diff 和逐 hunk 预览并随 `patch_proposed` 事件广播，TUI 卡片与 `d` 全屏审阅
> 将 unified diff 转成左旧右新的 side-by-side 视图（syntax highlight，窄宽回退 unified），
> CLI 保留 unified 输出。`d` 审阅里 `[`/`]`（或 `J`/`K`）跳 hunk、`Space` 勾选、`a` 全选/
> 全不选、`y` 只应用选中的 hunk（全选时整文件），`n` 拒绝；`patch.apply` / `approval.respond`
> 接受可选 `hunks` 索引列表。应用时用基准内容拼出结果再落盘，并重新校验文件内容：
> stage 后被外部修改则记 conflict、不覆盖。
> `run_command` 默认用 bubblewrap 隔离（`[tools] sandbox = auto|bwrap|off`，`sandbox_network`
> 控制是否 `--unshare-net`，`sandbox_read_home` 决定 `$HOME` 只读挂载还是隐藏）：宿主只读、
> 仅 session workspace 可写、`/tmp` 为私有 tmpfs；bwrap 缺失时 `auto` 退回直接执行，
> `bwrap` 模式则拒绝执行。
> 执行前快照已落地：`write_file` 复制目标文件（含“原本不存在”这一事实），`run_command`
> 复制整个 workspace；排除 `.git` / `target` / `node_modules` / `.venv` / `dist` 等派生目录，
> 单文件 8 MB、总量 200 MB、条目 5 万上限，跳过数与失败都记入 `snapshot` 事件（失败时工具
> 输出也带一行说明），每个 session 保留 20 份。`Snapshotter::restore` 已实现并有测试覆盖，
> 供 Phase 3 的 rewind 调用；事件日志只存索引，快照字节在 `<data>/snapshots/<session>/<id>/`。
> `[tools] snapshots = false`（`EIX_TOOL_SNAPSHOTS=0`）可在大仓库里关闭。
> Staged shadow workspace 已落地：`ask` 策略下 `write_file` 先把提案写入
> `<data>/staging/<session>/<patch_id>.json` 并立即返回 `staged ...`，turn 不阻塞；
> `read_file` / `list_dir` 看到“真实工作区 + staged”合并视图，同一文件再次写入以最新
> staged 内容为基准，可以叠加。`patch.proposed|applied|rejected|conflict` 事件与同名通知
> 已接入，`patch.apply` / `patch.reject`（以及 `approval.respond` + patch id）负责落盘或
> 丢弃；落盘前重新校验基准，stage 后被外部修改则记 `patch_conflict` 且不覆盖。单文件快照
> 改为应用补丁时抓取。pending 补丁跨 daemon 重启恢复；TUI 卡片、attention、`eix approve` /
> `eix deny`、`eix log` 均已支持补丁。`run_command` 无法 stage，仍在审批处阻塞。
> Context ledger 已落地：`eixd/src/context.rs` 从事件日志重建 provider 上下文并按 sector
> 记账（system 5% / instructions 5% / workspace 15% / history 60% / tools 10% / memory 5%），
> `context.ledger|drop|pin|compact` 请求与 `context.updated|compacted` 通知已接入；
> `AGENTS.md`（cwd 与 config 目录）注入 `instructions` sector，workspace summary 注入顶层
> 文件树与 git branch/status。drop / pin / compact 落成事件
> （`context_dropped|pinned|compacted`），重启后重放一致；`tools` / `history` 超预算时从最旧
> 未 pin 条目自动裁剪（保留最新 4 条），system / instructions / workspace 不自动裁剪。TUI `c`
> 打开全屏 ledger（`o` 展开、`p` pin、`d` drop、`m` 标记、`s` 压缩范围），CLI 为
> `eix context [--drop|--undrop|--pin|--unpin|--compact]`。
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

- rewind/fork、全文搜索、worktree、子 agent、nvim 前端、远程 attach。

> Fork / rewind 已落地（2026-10-18）：`session.fork {session_id, at_seq?, name?}` 把源
> session 的 `seq < at_seq`（`None` 表示全部）事件复制到新 session，序号原样保留，因此
> ledger 重建结果与源前缀一致；新 session 为 `idle`，复制 `cwd` / 模型 pin，并记录
> `parent_id` + `forked_from_seq`（独占切点）。默认命名去掉已有的数值 `~N` 后取
> `name~2`、`name~3`…；`sessions` 表用 `PRAGMA table_info` 做幂等加列迁移。原 session
> 完全不变。TUI `F` 在末尾 fork，ledger 里 `f` 在选中消息之前 fork（`seq_of` 非空时）；
> 新 session 自动在新 pane attach。CLI 为 `eix fork [--session ID] [--at SEQ] [--name NAME]`。
> **真正的 rewind**：`session.rewind {session_id, at_seq?, name?}` 先 fork，再把
> `seq >= at_seq` 的快照按 seq 倒序（最新先撤）交给 `Snapshotter::restore`；busy session
> 直接拒绝。返回 `RewindResult { restored, snapshots, files_restored, files_removed,
> files_missing, warnings }`，`restored = failed_restores == 0 && files_missing == 0`；
> 没有快照 / 快照关闭 / 被 prune / 恢复失败 / 文件缺失都会进 `warnings`，并提示“eix 之外的
> 改动无法察觉”。TUI ledger `w` 和 CLI `eix rewind` 走这条路径，纯 fork 仍标 `files unchanged`。
>
> Worktree 已落地（2026-10-18）：`session.create` / `session.fork` 接受 `worktree: bool?`
> （`None` = fork 时在 git 仓库里自动建，create 时表示默认不建；`Some(false)` = 共享）。
> `eixd/src/worktree.rs` 用 `git rev-parse --git-common-dir` 解析主仓库（支持 subdir / linked
> worktree，canonicalize 后比较），`git worktree add -b eix/<slug> .worktrees/<slug>`；slug
> 来自 session 名，冲突时加 `-N`。worktree 路径存 `sessions.worktree`，fork 未指定新
> worktree 时继承源值。`eix worktree list` / `remove SESSION [--force]` 对
> `session.worktree.remove`：先确定主仓库再 `git worktree remove [--force]`，成功后
> `set_session_cwd(repo_root)` 并清空 worktree；dirty 且没 `--force` 时返回 `removed:false`。
> `.worktrees` 已在快照排除表里，避免把 worktree 自身复制进快照。
>
> 子 agent 已落地（2026-10-18）：模型工具 `spawn_agent {label, prompt}` 由 daemon 建 child
> session（`origin = agent`、`parent_id`、名字 `parent/<label>-N`、cwd 继承），只读工具集
> `read_file` / `list_dir` / `process_output`；agent session 在 `run_tool_call` 里对写工具
> 直接拒绝，深度上限 1（child 不能再 spawn）。同一 assistant 批次里的多个 `spawn_agent`
> 用 `tokio::spawn` 并发跑，其余工具保持顺序，结果按调用顺序回填；child 的完整 turn 写进
> 自己的事件日志，失败作为普通 tool 结果返回，不打断父 turn。TUI 侧栏用 `↳` 标记。
>
> 独立 nvim 前端已落地（2026-10-18）：`eix nvim [--session ID]` 把内嵌 Lua 插件写到
> `<data>/eix-nvim.lua`，用 `EIX_NVIM_SOCKET` / `EIX_NVIM_TCP` / `EIX_NVIM_TOKEN` 连 daemon，
> 转录渲染进 `eix://<id8>` scratch buffer；`:Eix attach|list|new|send|prompt|fork|rewind|stop|ps|ledger`
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
> 全文搜索已落地（2026-10-18）：`search_docs`（内容表）+ `search_fts`（FTS5 trigram，
> 外部内容）+ 触发器在 `append_event` 写 `user_message` / `assistant_message` 时同步索引；
> 启动时用 `INSERT OR IGNORE ... SELECT` 补齐旧库。3+ 字符走 `MATCH`（每个词加引号，
> 标点按字面处理，空格为隐式 AND，`bm25` 排序）；更短的查询退化为
> `instr(lower(text), lower(query))` 子串扫描，解决中文 1–2 字词 trigram 匹配不到的问题。
> 结果带 session 名、seq、角色和字符窗口 snippet（CJK 按 `char` 切分，不会产生非法字节
> 偏移）。协议 `search.query {query, session_id?, limit?}`，`limit` 夹在 `1..=100`（默认
> 20）。TUI `#` 打开全屏搜索：输入回车执行、`↑/↓` 选择、再回车在 pane 中打开；CLI
> `eix search QUERY [--session ID] [--limit N]`。工具输出不进入索引。fork 时把前缀的
> `search_docs` 行一并复制（走同一批触发器），所以新分支立刻可搜，不必等重启补索引。
>
> **P0 补齐（2026-10-19）**
>
> 工具：`grep {pattern, path?, include?, literal?, caseSensitive?, limit?}`、
> `glob {pattern, path?, hidden?, limit?}`、`edit {path, oldString, newString, replaceAll?}`
> 和 `patch {patchText}` 已落地。`read_file` 增加 `offset`/`limit` 且输出带行号。
> `grep` / `glob` 只读自动放行；`edit` 与 `write_file` 共用 staged shadow workspace
> （唯一匹配校验、生成 hunks）；`patch` 走阻塞审批，解析 `*** Begin Patch` 的
> Add/Update/Delete，先校验全部文件再写入，并抓工作区树快照。`grep` / `glob` 目前看不到
> staged 内容（MVP 限制）。
>
> 扩展：`eix-core/src/commands.rs` 从 `~/.config/eix/commands`、`~/.config/opencode/commands`
> 和项目祖先目录的 `.eix/commands` / `.opencode/commands` 加载 Markdown 命令；嵌套目录映射为
> `team/name`，frontmatter 支持 `description` / `agent` / `model`，模板支持 `$ARGUMENTS` 和
> `$1..$9`。`eix-core/src/agents.rs` 用同样的目录规则加载 agent，frontmatter 支持
> `mode` / `model` / `permission`，正文追加到 system prompt；内置 `build` / `plan` / `explore`。
> `session.agent` 持久化到 `sessions.agent`，`spawn_agent` 接受 `agent` 参数，权限只能收紧。
>
> 模型：`[[models]]` 提供多 provider 注册（每个条目独立的 `base_url` / `api_key(_env)` /
> `model` / 成本 / `variants`）；`Config::route` 解析 `id#variant`，变体作为
> `reasoning_effort` 发送。`model.list` / `session.model` / `/models` picker（含变体行）已接入；
> `turn.completed` 带 `usage` / `cost` / `duration_ms`，用于内部统计与状态消息。
>
> MCP：`eixd/src/mcp.rs` 实现 stdio JSON-RPC 客户端，`[mcp.servers.<name>]` 懒启动并缓存进程。
> `tools/list` / `resources/list` / `prompts/list` 都支持分页 cursor；工具名为
> `mcp__<server>__<tool>`，始终需要审批（`auto` / `full` 除外），执行前抓树快照；资源与 prompt
> 通过只读的 `mcp_list_resources` / `mcp_read_resource` / `mcp_list_prompts` / `mcp_get_prompt`
> 暴露，不需要审批。`notifications/*/list_changed` 标记缓存失效，下次 turn 或 `/mcp` 刷新。
> `allowed_tools` / `denied_tools` 支持 `*` / `?`（`*` 也匹配 `/`，deny 优先）；`mcp.list` 与
> `/mcp` 显示状态、工具/资源/prompt 数量和错误。
>
> Session 管理：`session.archive` / `session.delete` / `session.export` / `session.import` /
> `session.timeline` / `session.undo` / `session.redo` / `session.todos` RPC；archive 是持久化
> 标记且默认列表隐藏，delete 级联事件/草稿/搜索，export/import 保留事件 seq（压缩范围仍然有效）。
> undo/redo 用 `events.undone` 批次标记隐藏/恢复最近一个 user turn，并同步 FTS 索引；工作区文件
> 不自动回滚（需要 `rewind`）。TUI 对应 `/undo`、`/redo`、`/timeline`、`/archive`、
> `/delete`（两次确认）、`/todos`，队列支持 `rm N` / `move N M`；JSON 导出/导入仅保留 CLI
> 的 `eix export` / `eix import`。
>
> Skills：`eix-core/src/skills.rs` 从全局与项目祖先目录加载 `<name>/SKILL.md`（含 `.opencode` /
> `.claude` 兼容，同目录 `.eix` > `.opencode` > `.claude`）；system prompt 只放
> `<available_skills>` 摘要，模型用只读 `use_skill {name, arguments}` 按需取正文，正文展开
> `$ARGUMENTS` / `$1..$9`。`allowed-tools` 支持通配，加载后在本 turn 内限制工具（并集，`use_skill`
> 始终可用，下一条 user prompt 清空）；Claude 风格工具名映射到 eix 名称。TUI `/skills` 是带正文
> 预览的 picker。`[formatter]`
> 在直接写入和 staged apply 之后运行（`$1` 为路径，失败只附注）。`todo` / `webfetch` / `websearch`
> 为新增工具；`todo` 是 daemon 内存态任务表，网络工具无 API key（DuckDuckGo HTML），读取结果会截断。
> TUI 支持 `[theme]` 调色板、`[keys]` 转录区快捷键覆盖（输入框保持 vim 行为），以及 prompt 里的
> `!<cmd>`：直接执行 shell（同样沙箱、先抓 tree snapshot、只读 session 拒绝），命令和输出写入 tool 事件。
>
> 图片：`prompt.submit` 接受 `images: [path]`，daemon 经 `resolve_path_in` 校验后按 SHA-256 写入
> `<data>/blobs/<前两位>/<hash>`，事件只存 blob id / MIME / 大小；`ChatMessage::to_wire` 把带图
> 消息编成 OpenAI 多模态 content 数组（text + image_url data URL），文本消息仍保持字符串
> content。旧的内联 base64 事件仍可回放；export 会内联图片，import 重新落 blob。默认最多 4 张、
> 每张 8 MB、无引用 blob 保留 30 天（`[images]` 可调），启动/删除 session/后台每 6 小时 GC。
> TUI `/image`、直接粘贴/拖入路径或 `file://` URI、CLI `eix send --image` 是入口。
>
> LSP：`eixd/src/lsp.rs` 是精简 stdio 客户端（Content-Length framing），按 server+workspace root
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
- session 自动命名规则（首条 prompt / branch / 手动）。
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
- 路径：配置 `~/.config/eix/`，数据 `~/.local/share/eix/`，socket `$XDG_RUNTIME_DIR/eix.sock`。
