# eix — AI Agent 交互设计

> 状态：v0.2（2026-10-18，spike 后修订）
> 定位：面向 vim / dwm / Arch 工作流的 coding agent；本文件是交互与技术契约，随实现修订。
> 名字：`eix`，取自欧拉公式 `e^{ix} = cos x + i·sin x`；加泰罗尼亚语中意为「轴」。
> v0.2 变更：命名统一为 `eix`（`g:eix_prompt` / `:Eix` / `eix pick`）；补充实测的 redraw/linegrid 契约；修正 `<leader>ac` 键位冲突；加入实现状态快照。

## 1. 目标与非目标

**目标**

- 与 codex 同级的 coding agent（工具调用、补丁、审批、上下文管理）。
- 多 session 是一等公民：常驻、可 attach/detach、可并行、可模糊切换。
- 输入框是**真正的 vim**：嵌入用户自己的 Neovim，加载完整配置。
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
| 输入 | 一个 client 一个嵌入 nvim，对准聚焦 pane | 复杂度从 N 个 nvim 降到 1 个；符合"输入框"心智 | 切 pane 需切 buffer |
| nvim 配置 | **完整加载 `~/.config/nvim`** | 肌肉记忆、插件、colorscheme 全部原生 | 插件 UI 干扰、启动成本、浮动窗口裁剪 |
| 内核 | 从零手写 | 上下文与工具策略完全可控 | 工作量与质量风险 |
| provider | 优先 OpenAI 兼容接口 | 一套 adapter 覆盖 OpenAI / Ollama / vLLM / OpenRouter 等，本地模型零成本接入 | 各家非标准字段需归一 |
| leader | `<Space>`（沿用 `mapleader`） | 现有肌肉记忆与 which-key 直接可用 | insert 模式下不可用，用 `<C-g>` 补充 |
| 转录 | ratatui 自绘 + vim 子集键位 | 流式/审批卡片/性能可控 | 需维护一套只读导航键位 |
| 审批 | propose → stage → apply | hunk 级审批才有意义；可回滚 | 需虚拟工作区/补丁暂存 |
| 上下文 | 可见且可编辑的 context ledger | 既是 UX 也是回答质量 | 中等额外工作量 |

## 3. 架构

```
┌────────────────────────── client (Rust / ratatui) ──────────────────────────┐
│  pane 1 (session A transcript)   pane 2 (session B transcript)              │
│  ┌────────────────────────────────────────────────────────────────────────┐ │
│  │ 全局输入条：唯一一个嵌入 nvim（--embed），buffer 随聚焦 pane 切换      │ │
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

- **rewind**：选择一条消息，截断到它之前，生成新分支头；原分支保留，可 `:Agent branches` 找回。
- **fork**：复制前缀到新 session，命名 `name~2`，header 显示 `name ← name~2`。
- **诚实性约束**：事件日志不能重放工作区。所有写文件/打补丁都记录 diff 快照事件；rewind 时尽可能回退，退不了的明确标红提示。
- fork 默认建议同时起 worktree，避免共享 cwd 的并发冲突。

### 4.6 worktree

- 默认绑定 cwd；`--worktree` 或 picker 中一键创建 `.worktrees/<session>`。
- worktree 与 session 生命周期解耦：session 归档不自动删除 worktree，需显式确认。

## 5. 输入层：嵌入真 nvim

这是交互的核心，也是最大技术风险。**一个 client 只跑一个 nvim 进程**，所有 pane 共用；pane 只决定当前 buffer。

### 5.1 进程与连接

- `nvim --embed --cmd 'let g:eix_prompt = 1'`：`--cmd` 在用户配置加载前执行，便于用户在 init.lua 中做条件分支。
- host 通过 msgpack-rpc 连接；`nvim_get_api_info()` 拿到 channel id，写入 `g:eix_host_chan`。
- `nvim_ui_attach(cols, rows, {ext_linegrid=true, rgb=true, ext_multigrid=false, ext_popupmenu=false, ext_cmdline=false, ext_messages=false})`。
- 让 nvim 自己把 cmdline / popupmenu / 消息画进 grid，host 不实现这些组件，减少一半渲染工作量。
- host 自己占终端最后一行画 eix 状态栏，因此 attach 给 nvim 的高度是 `rows - 1`。
- 启动预热（planned）：client 启动即异步拉起 nvim；配置超时（5s）则回退 `-u NONE` + 内置 prompt profile，并提示用户。

### 5.2 配置与消毒（sanitize）

完整加载配置意味着插件会争夺 UI。启动后由 host 注入 prompt profile（内置 Lua），做一次"输入条消毒"：

- 全局：`laststatus=0`、`showtabline=0`、`cmdheight=1`、`showmode=false`、`ruler=false`、`mouse=`、`shortmess += I`（抑制空 buffer 的 intro）。
- prompt buffer：`buftype=nofile`、`bufhidden=hide`、`noswapfile`、`nonumber`、`norelativenumber`、`signcolumn=no`、`foldcolumn=0`、`wrap`、`linebreak`、`breakindent`、`nolist`、`conceallevel=0`。
- **选项会被用户 autocmd 覆盖**（实测：`InsertEnter` 里开 `number` 的 "smart line number"）。prompt profile 自己注册一个 buffer-local 的 `InsertEnter/InsertLeave/TextChanged` autocmd 重新消毒；后注册的先跑，因此能压住用户配置。
- 关闭该 buffer 的 diagnostics；设置 `vim.g.eix_prompt = 1`（`--cmd` 阶段，用户配置可见）与 `vim.b.eix_prompt = true`（profile 之后，运行时可判断）。
- 常见冲突插件：smear-cursor（动画浮窗残影，实测）、noice、nvim-notify、lualine、incline、indent-blankline、cmp 浮窗。提供 `:Eix doctor` 列出检测结果和冲突。
- 提供 `--minimal` / `prompt.nvim = "minimal"` 逃生配置。
- **推荐**用户在自己的 init.lua 加一段：

```lua
if vim.g.eix_prompt then
  -- 在这里关闭会干扰输入条的 UI 插件
  require("smear_cursor").enabled = false
end
```

### 5.3 渲染（linegrid）

- 维护 cell 网格（char + hl_id）；处理 `grid_resize / grid_line / grid_clear / grid_scroll / grid_cursor_goto / hl_attr_define / default_colors_set / mode_change / mode_info_set / flush`。
- **只在 `flush` 时上屏**：一个 redraw 批次可能包含多屏中间态（多页 batch），中间页只应用到网格、不绘制；批次末尾没有 `flush` 时同样不绘制。
- **redraw 实际形状**（实测 nvim 0.12 + nvim-rs 0.6，与文档不符）：notification 的参数就是 batch 展开后的列表，每个元素是 `[event_name, param_tuple, ...]`，同名连续事件会打包进同一个元素。渲染器必须同时接受文档形状 `[[event, ...], ...]` 和这种展开/打包形状。
- **网格事件自带 grid handle**：即使 `ext_multigrid=false`，`grid_*` 的第一个参数也是 grid 编号（恒为 1）。`hl_attr_define` / `default_colors_set` / `mode_change` / `set_title` 等没有这个前缀。
- `hl_attr_define` 映射为 ratatui style：reverse / bold / italic / underline / strikethrough；**undercurl 降级为 underline**（ratatui 没有下划波浪线）。
- `grid_line` 的 cell 是 `[text, hl_id?, repeat?]`；`text == ""` 表示宽字符的右半格，**该格不能写空格**，否则会打掉半个字；但非宽字符来源的空 cell 必须显式清空，否则 diff 渲染会留下残影。
- 光标形状来自 `mode_info_set` + `mode_change`（block / vertical / horizontal），支持闪烁（planned；spike 只画位置）。
- 输入条高度：spike 固定占满 nvim 区域；目标形态是聚焦时 `clamp(line_count, 3, 10)`、失焦时 1 行，布局变化时 `nvim_ui_try_resize`（Phase 1）。
  - 已知限制：单 grid 模式下，cmp/noice 等**浮动窗口会被输入条边界裁剪**，动画类浮窗（smear-cursor）还会留残影。v1 用"聚焦时给足高度 + `g:eix_prompt` 逃生"缓解；后续用 `ext_multigrid` 把浮窗作为独立 grid 叠加渲染。
- **主题桥接**：启动及 `ColorScheme` 变化时，用 `nvim_get_hl` 读取 `Normal/Comment/String/Function/ErrorMsg/DiffAdd/DiffDelete/Pmenu/Visual`，映射到主机 UI，让转录区与你的 colorscheme 一致。

### 5.3.1 自检（`--probe`）

渲染器必须有一键自检，不依赖 TTY：attach → 等第一帧 → 注入含中文/emoji/全角的样例行 → 打印网格 → 用 `nvim_input` 走一次 `<CR>` 提交往返 → 退出。`--minimal` 与完整配置都要能跑；`EIX_DEBUG=1` 打印启动自检（colorscheme、config 路径、模式）与浮动窗口清单。

### 5.4 按键语义

**语义下沉到 nvim**，而不是在 host 里做键路由：host 把所有按键转发给 nvim，nvim 的 buffer-local mapping 再通过 `rpcnotify` 通知 host。这样模式判断、补全菜单、命令行都由 nvim 自己裁决，避免 host 猜错。

prompt buffer 的 buffer-local mapping：

| 键 | 行为 | 守卫 |
|---|---|---|
| `<CR>` | 提交 | `getcmdtype() != ""` → 原样；`pumvisible()` → 补全确认；否则 submit |
| `<C-j>` | 换行 | `getcmdtype() != ""` → 原样；`pumvisible()` → 交还原 mapping（如 cmp 的 `select_next_item`）；否则插入换行 |
| `<C-g>` | 打开 session picker | 无（planned） |
| `<leader>a…` | eix 命令组 | 无（planned） |
| `<C-w>h/j/k/l/w` | 切换 TUI pane 焦点（仅 normal） | insert 下保持原生删词 |
| `@` | 文件 picker（insert，词首） | 否则插入字面 `@`（planned） |
| `:Eix …` | 用户命令 | 不是 vim 原生 `:` 的替代 |

`<CR>` / `<C-j>` 都采用"先保存、后接管"策略：安装前用 `maparg()` 取回用户原有 mapping，补全菜单可见时把控制权交还。实测本机配置把 `<C-j>` 映射给了 `cmp.select_next_item()`，不接管的话输入框无法可靠换行。

- **leader 镜像**：host 读取 `g:mapleader`（预期为空格）用于转录区/overlay 的同名前缀；用户配置里的 which-key 会自动把 `<leader>a` 组展示出来。
- 已知冲突：`<CR>` 会覆盖 cmp 的确认键，所以必须走 `pumvisible()` 守卫；`<C-g>` 若已被用户映射，启动时告警并允许改键。
- `<C-w>` 在 nvim 里是窗口命令前缀（嵌入的 nvim 只有一个窗口，原生动作基本是空操作）。在 prompt buffer 的 normal 模式里做 buffer-local 覆盖是安全的，但要在 doctor 里说明它在该 buffer 内不再能开/关窗口。
- `:q` 会退出嵌入 nvim：host 监听退出并自动重启，draft 从 daemon 恢复；提示用户用 `<leader>a q` detach（spike 目前是直接退出，重启逻辑 planned）。
- 粘贴：启用 bracketed paste，整段文本一次 `nvim_paste`；键盘输入里的 `<` 转义为 `<LT>`（`nvim_input` 用 `<...>` 记法）。

### 5.5 buffer 与 draft

- 每个访问过的 session 在 nvim 内对应一个隐藏 scratch buffer，保留 undo 历史；上限 20 个，超出按 LRU 序列化到 daemon 后丢弃。
- draft 变更 debounce 300ms 同步到 daemon；提交前用 RPC 读取最新文本，避免竞态。
- 切换 pane：`nvim_set_current_buf`，同时 `nvim_set_current_dir(session.cwd)`。
- nvim 崩溃：host 显示提示，用 daemon 中的 draft 重建 buffer 并重启；连续失败两次则自动切 minimal。

### 5.6 输入条布局

```
┌─ transcript: auth ──────────────────────────────────────────┐
│ ...                                                         │
├─────────────────────────────────────────────────────────────┤
│ auth  [INSERT]                                              │
│ > 把登录改成 JWT，并补测试                                   │
│                                                             │
├─────────────────────────────────────────────────────────────┤
│ ● auth  ⚠ refactor  ✓ docs     main  ~12k tok  $0.08        │
└─────────────────────────────────────────────────────────────┘
```

- 输入条始终可见，失焦时保留最后一行；聚焦时增高。
- 模式指示由 host 根据 `mode_change` 渲染，不依赖 nvim 的 showmode。

## 6. 布局与焦点

- pane 树：水平/垂直分割，每个 leaf 显示一个 session 的转录。
- 全局输入条与 statusline；tabline 显示 session 状态，可隐藏（窄窗口默认隐藏）。
- 焦点：`<C-w>h/j/k/l`（normal 模式与转录区一致）、`<C-w>w` 循环；转录区按 `i` 聚焦输入并进入 insert。
- 双击 `Esc`（可配置）从输入回到转录；默认不强求。

## 7. 键位总表

> 前缀假定 `mapleader = <Space>`；`<C-g>` 作为跨模式补充入口（insert 模式下也能触发）。

**全局（转录区 / picker / 任意非输入焦点）**

| 键 | 行为 |
|---|---|
| `<C-g>` | session picker（直达） |
| `<C-g><C-g>` | 回到上一个 session |
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
| `<leader>a ?` | 帮助 / cheatsheet |
| `<C-w>h/j/k/l` | 切换 pane 焦点 |
| `<C-w>w` | 循环 pane |

**输入条（nvim 内，buffer-local）**

| 键 | 行为 |
|---|---|
| `<CR>` | 提交（带 pum/cmdline 守卫） |
| `<C-j>` | 换行 |
| `@` | 文件 picker |
| `<leader>a…` | 同上 |
| `:Agent …` | 命令（new/fork/context/diff/model/perm/export/doctor） |
| 其余 | 你的完整 nvim 配置 |

**转录区**

| 键 | 行为 |
|---|---|
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

- sessions / files(`@`) / history / commands / models / worktrees / branches / diff hunks / context items。
- 预览器：session → 最后消息 + 待审批摘要 + todo；file → 语法高亮；diff → 着色 diff；message → 完整渲染。
- 排序：attention 优先，其余 frecency；`#` 模式按全文匹配度。
- 多选：多文件插入、多 session 开 pane、多 hunk 审批。
- 外挂：`agent pick` 输出会话列表，供 rofi/dmenu/fzf 使用；tmux 下可选 shell out 到 `tmux display-popup -E fzf`。
- 模糊匹配用 `fuzzy-matcher`（skim 算法）；不做自研打分。

## 9. 审批与工作区

### 9.1 执行模型

- **propose → stage → apply**：agent 的写操作先进入 staging 视图（补丁/影子目录），用户批准后才落到真实工作区。
- agent 的读操作看到的是 `真实工作区 + 已 stage 改动` 的合并视图，避免"自己改了什么自己不知道"。
- shell 命令无法 stage：走审批门禁 + 执行前快照（git stash 或目录快照），风险等级单独标注。

### 9.2 审批 UI

内联卡片：

```
⚠ 修改 src/auth.rs (+12 -4)
[y] 批准  [n] 拒绝  [e] 编辑  [d] 全量 diff  [?] 理由
```

全量 diff 视图（`<leader>a d`）：

- 左文件树 / 右 hunk；`J/K` 跳 hunk，`Space` 选中，`[`/`]` 跳文件。
- `y` 应用选中（未选中则全部），`n` 拒绝，`e` 在 nvim 打开真实文件，`o` 打开原文件。
- 冲突检测：文件在 stage 后被外部修改 → 标红并要求重新 stage。

### 9.3 权限策略

- 级别：`read-only` / `workspace-write` / `full`；按 session 设置，可临时提权（记入事件日志）。
- 自动批准规则：工具名 + 路径 glob + 命令前缀/正则；只读工具默认放行。
- "本 session 全部批准"必须限定到**具体工具 + 路径/命令模式**，不做无条件 blanket。

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
- `turn.started`、`message.delta`、`message.completed`
- `tool.requested`、`tool.approval_required`、`tool.started`、`tool.output`、`tool.completed`
- `patch.proposed`、`patch.applied`、`patch.rejected`、`patch.conflict`
- `process.started|output|exited`
- `context.updated`、`context.compacted`
- `draft.updated`

**client → daemon（request）**

- `session.list|create|attach|detach|rename|archive|fork|rewind`
- `prompt.submit|cancel|queue`
- `approval.respond`、`patch.apply|reject`
- `context.drop|pin|compact`
- `picker.query`（需要跨 session 搜索时）
- `nvim.claim`（输入所有权）

所有事件带 `id`、`session_id`、`seq`、`ts`、`type`、`payload`；协议版本握手。

## 12. 持久化与搜索

- SQLite（WAL）：`sessions`、`events`、`messages`、`drafts`、`approvals`、`patches`、`frecency`。
- 大输出/补丁走内容寻址 blob：`~/.local/share/agent/blobs/<sha256>`。
- FTS5：
  - 主索引 `trigram`（中英混排友好）。
  - **短查询（1–2 字）降级 LIKE 扫描**，先收窄到当前/最近 session；否则中文高频词搜不到。
  - 可选自研 bigram tokenizer 作为后续优化。
- 文件权限：目录 0700、文件 0600；日志写入前做密钥脱敏（API key、token、`.env` 内容）。
- 事件日志天然是回放测试夹具：record/replay 用于集成测试。

## 13. 通知

- 终端：OSC 9 / OSC 777、bell、标题栏 `[⚠2] eix: auth`。
- 桌面：`notify-send`（dunst），可通过配置替换命令。
- dwmblocks：`agent status --json` 输出 `{running, approval, error}`。
- 仅在状态跃迁时通知；当前聚焦 session 不通知；冷却时间可配。

## 14. 安全

- 工具沙箱：bubblewrap。工作区 rw，`/tmp` tmpfs，home 只读（缓存目录按需 rw），网络开关。
- 环境变量白名单；密钥不进入事件日志。
- 网络/破坏性命令默认需要审批。
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

> 实现快照（2026-10-18）：多 pane TUI 已落地（`eix tui`）。左侧 session 栏 + 右侧自动
> 平铺 pane（1/2/3/4+ 分别全屏、左右、左大右二、网格），每个 pane 独立维护 session、
> 转录、滚动与流式跟随；`o` 在新 pane 打开，`p`/`Ctrl-G` 切换聚焦 pane 的 session，
> `C-w h/j/k/l/w` 切焦点、`C-w v/s` 分屏、`C-w c` 关闭；同一 session 只出现一次，
> picker 选中已打开 session 会聚焦既有 pane。输入框暂由 ratatui 自绘（非嵌入 nvim）。
> 后台 pane 的 attention 已接入：daemon 在进入 `awaiting_approval` / `error` 时发送
> `session.attention`，TUI 标记边框/侧栏并支持 `a` 跳到下一个；重新 attach 会从事件日志
> 恢复未完成的审批卡片。外部通知命令（dwm/tmux）仍待做。

**Phase 2**

- 工具执行、staging、审批、diff hunk、沙箱、context ledger、process view。

> 实现快照（2026-10-18，进行中）：工具调用与审批门禁已落地。daemon 暴露
> `read_file` / `list_dir`（只读，自动放行）和 `write_file` / `run_command`（需审批），
> 所有路径经 workspace 沙箱（拒绝 `..` 与 symlink 逃逸），输出按字节截断，
> `run_command` 带超时。provider 侧支持 OpenAI `tools` / `tool_calls` 的流式增量累积，
> 一次 turn 最多 12 轮工具往返；每轮写入 `tool_call` / `approval_required` / `tool_result`
> 事件并广播 `tool.approval_required` / `tool.started` / `tool.completed`。TUI 渲染工具卡片，
> 聚焦 pane 有 `[y]/[n]` 审批卡片，`eix approve/deny` 可无头处理。
> 策略：`[tools] policy = ask|auto|read_only`（默认 ask）。`write_file` 在审批前生成
> unified diff 和逐 hunk 预览并随 `approval_required` 事件广播，TUI 卡片（`d` 全屏审阅）
> 和 CLI 均会展示。`d` 审阅里 `[`/`]`（或 `J`/`K`）跳 hunk、`Space` 勾选、`a` 全选/
> 全不选、`y` 只应用选中的 hunk（全选时整文件），`n` 拒绝；`approval.respond` 新增可选
> `hunks` 索引列表，daemon 用批准时的原文件内容拼出结果再落盘。写入前会重新校验文件
> 内容，审阅期间被外部修改则拒绝本次写入。
> **尚未做**：staged shadow workspace、bubblewrap 沙箱、执行前快照、context ledger、
> process view。

**Phase 3**

- rewind/fork、全文搜索、worktree、子 agent、nvim 前端、远程 attach。

## 17. 待定

- 默认模型与具体的 OpenAI 兼容端点。
- `<leader>a` 命名空间是否与你现有映射冲突。
- `@` 触发是否只在词首；是否需要转义。
- 是否启用鼠标。
- 通知命令与冷却时间。
- session 自动命名规则（首条 prompt / branch / 手动）。
- 是否支持远程 attach（TCP + token）。

## 18. 命名

**项目名：`eix`**（读作 "eks"）

- 出处：欧拉公式 `e^{ix} = cos x + i·sin x`。实数轴上的一点，乘上虚数单位 `i`，便落到复平面的单位圆上——**换一个维度看同一个东西**，正如在多个 session / 视角之间穿梭。
- 双关：加泰罗尼亚语中 `eix` =「轴 / 轴线」。
- 记号也顺：`e`（内核）、`i`（输入）、`x`（可执行文件）。

**已知识别冲突**

- Gentoo 有同名工具 `app-portage/eix`（portage 包搜索）。你在 Arch 下基本遇不到，领域也不相干；若哪天在意，可只用 `eix` 作项目名，二进制另起（或 daemon 用 `eixd` 区分）。

**命名约定**

- 仓库 / crate 前缀：`eix-*` → `eix-core`、`eixd`、`eix-tui`、`eix-nvim`。
- 二进制：`eix`（TUI + CLI）、`eixd`（daemon，通常由 `eix` 自动拉起）。
- 路径：配置 `~/.config/eix/`，数据 `~/.local/share/eix/`，socket `$XDG_RUNTIME_DIR/eix.sock`。
