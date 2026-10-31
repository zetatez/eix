# Worktree 使用指南

eix 的每个 session 默认与其他 session 共享同一个 `cwd`。想让两个 session 同时改同一
个仓库而不互相踩，就给 session 一个 **git worktree**：独立的目录、独立的分支，提交
通过 git 合并回来。

```console
$ eix new --worktree fix-login           # 立刻建 worktree，session 在里面跑
$ eix worktree list                      # 看谁有 worktree、目录还在不在
$ eix ready                              # 排队等集成（或在 TUI 里按 /ready）
```

## 目录与分支约定

创建的 worktree 固定落在**主仓库**下：

```
<repo>/.worktrees/<slug>        目录
eix/<slug>                      分支
```

- `<slug>` 来自会话名（或 `--name` / 自动生成的短名），会被转成合法的分支片段。
- 名字或分支已存在时自动加后缀 `-2`、`-3`…，不会静默复用已有目录或分支。
- `.worktrees/` 由 eix 追加到 `.git/info/exclude`，**不写你的 `.gitignore`**，所以不会
  出现在 `git status` 里，也不会被提交。
- 路径是 canonicalize 后的绝对路径，仓库在软链后面时 `cwd` 也保持稳定。
- session 本身已经在一个 linked worktree 里也没关系：创建时会先解析回主仓库
  （`git rev-parse --git-common-dir` 的父目录），避免嵌套 `.worktrees`。
- 会话原本在仓库的**子目录**里时，worktree 内的 `cwd` 会落在同一相对子目录
  （目录不存在则退回 worktree 根），不会把你挪到别处。

## 四种创建方式

### 1. 命令行

```console
eix new --worktree                 # 用当前目录所在仓库，worktree 名取自会话名
eix new --worktree --name feat-x   # 指定名字
eix new --shared                   # 明确相反：共享当前目录，不建 worktree
```

`--worktree` 与 `--shared` 互斥（同时给会直接报错）。创建成功后 CLI 会多打一行
`worktree /path/to/.worktrees/feat-x`。不传 `--name` 时会话名默认取 `cwd` 的目录名，
所以 worktree 名通常就是仓库目录名（`fix-login` 这类短名要靠 `--name` 或 TUI 自动命名）。

### 2. TUI

在 normal 模式按 `Space W`（默认绑定 `new_worktree`），把**当前 pane 的 session** 搬进
新 worktree。它有三条前置检查，命中时只给提示、不发请求：

- 还没 attach 任何 session；
- session 已经有 worktree；
- session 正在跑回合（`busy`），要等这一轮结束。

搬移不丢历史：只改 `cwd` 和 `worktree` 字段，事件日志和上下文都留着。底层方法与
`Space W` 等价的是 RPC `session.worktree.create`。

### 3. fork 时自动隔离

`session.fork` 的 `worktree` 默认是 `None`，语义是 **auto**：

```rust
None => self.config.load().worktree.auto && repo_root(source.cwd).is_some()
```

也就是说，**只有 `[worktree] auto = true` 且源 session 在 git 工作树里**，fork 才会拿到
自己的 worktree；其余情况共享源工作区。远端客户端（HTTP/SDK）更保守：`None` 一律按
`false` 处理，必须显式传 `worktree: true`。

```console
eix fork --at 12 --name spike       # 名字可用 --name；是否隔离取决于 [worktree] auto
eix fork --shared                   # 显式共享源工作区（传 worktree: false）
```

两个细节值得记住：

- 有 worktree 时 CLI 提示 `files are shared through git only`——fork 出来的目录是**新
  checkout**，不复制未提交改动；想回滚文件用 `eix rewind`。
- 共享时提示 `the workspace is shared; files were not rolled back`。
- fork 的工作树从**源 session 的基线分支**拉出，而不是它临时的 `eix/<name>` 分支。

### 4. 全局开关：每个新 session 自动建

```toml
[worktree]
auto = true
base = "auto"
```

开启后，新 session 在**第一次提交 prompt 时**才建 worktree（而不是创建那一刻）——这样名字
可以按任务内容生成：模型回一个 2–5 个词的英文短名，失败时用本地兜底规则。

这条自动路径有几处刻意收窄：

- 只对 eix 自己创建的 session 生效。手工导入或历史遗留的行没有 integration 记录，
  不会突然被搬走。
- 会话带 `worktree_disabled`（即当初用 `--shared` 建的）时跳过。
- 同一 session 的两条 prompt 同时到达只会建一个 worktree，另一条收到 `busy`。
- 远端 session 若仓库根不在 daemon 暴露的工作区内，就留在原目录。
- 名字生成要花一次模型调用，所以只发生在首条 prompt，且已建好就不再触发。

## 查看与切换

```console
$ eix worktree list
●  fix-login                    3f2a91c4   ok       /home/me/repo/.worktrees/fix-login
○  spike                         9b1e0077   missing  /home/me/repo/.worktrees/spike
```

依次是状态符号、名字、会话短 id、目录状态（`ok` / `missing`）和绝对路径。`missing` 说明
目录被手工删过，`git worktree prune` 之前这条记录会一直留着。没有任何 session 有
worktree 时只打印一行提示。

TUI 里按 `Space g` 打开 git picker，可以看到仓库的 worktree 和分支。选中一项是
**把路径插入 prompt**（方便让 agent 去看那个目录），不是 `cd`，也不切 session。

## 集成：把分支合回基线

worktree 里的提交不会自己回到 `main`。集成队列负责这件事：

```console
eix ready                    # 标记当前目录所属 session 的 worktree 待集成
eix ready <SESSION_ID>       # 指定 session（只认 id 或 id 前缀，不认名字）
eix integrate status         # 看队列状态
eix integrate retry          # 重新排队
eix integrate continue       # 同上，冲突修好后用
```

`retry` 和 `continue` 在实现里是同一个分支，都调 `integration.continue` 重新排队，区别只在
语义上。不带参数时用「当前目录所属的 worktree session」——从 worktree 的子目录里跑也能
找到它。

TUI 里等价命令是 `/ready`、`/integrate`。

### 队列做了什么

1. 若 `[integration] auto_commit = true`（**默认开**），先把 worktree 里的脏改动提交掉，
   免得 `ready` 因为 agent 的编辑而失败；
2. 解析基线分支并校验基线 worktree：必须**正好停在**该分支上且**干净**，否则直接报错
   （`base worktree … is on …`、`base worktree … has uncommitted changes`）；
3. 先 `git fetch --prune origin`，再 `pull --ff-only`；没有上游
   但存在 `origin/<base>` 时改用 `merge --ff-only origin/<base>`；
4. 在 worktree 里 `git rebase <base>`；
5. 跑测试命令（默认 `auto`，按 Cargo/npm/Go/Python 自动探测仓库该跑什么）；
6. 在基线 worktree 里 `git merge --ff-only eix/<name>`；
7. `push = false` 时不推远端，**从不 force push**。

第 3、5、7 步的开关目前**只能取内置默认值**：`IntegrationConfig` 的 `test`、
`timeout_secs`、`pull_base`、`push` 都标了 `#[serde(skip)]`，写进 TOML 会被忽略。默认是
`test = "auto"`、`timeout_secs = 1800`、`pull_base = true`、`push = false`。`[integration]`
里真正能从配置读到的只有 `auto_ready`、`auto_commit` 和 `repos`；而 `repos.*` 下也只有
`base` 生效（`test` / `timeout_secs` / `pull_base` 同样是 `skip`）。

所以 `server.rs` 里「Set `[integration] test` …」那句报错提示在当前版本是**无效的**：探测
不到测试命令时只能靠 `test = "auto"` 的自动识别，或让 agent 直接跑具体命令。

### 基线分支怎么选

`[worktree] base = "auto"`（默认）时按顺序取：

1. `origin/HEAD` 指向的分支（若本地存在同名分支）；
2. `main`；
3. `master`；
4. 仓库当前分支。

全都不成立时报 `cannot determine base branch; set [worktree] base explicitly`，把它写成
具体分支名即可。也可以用 `[integration.repos."/abs/path"]` 的 `base` 按仓库覆盖。

### rebase 冲突时你的工作不会丢

冲突修复失败会先做备份，再 `git rebase --abort` 把分支恢复到 pre-rebase 状态，备份放在
**主仓库**的 `.git/eix/recovery/`（link 出来的 worktree 也写这里，因为 common git dir 在
worktree 删除后仍然存在）：

```
.git/eix/recovery/<stamp>.patch        暂存区 + 工作区 diff（含二进制）
.git/eix/recovery/<stamp>-files/       冲突/变更文件的逐份副本
.git/eix/recovery/<stamp>.txt          元数据：分支、HEAD、onto、冲突文件清单
refs/eix/recovery/<stamp>              指向原 HEAD 的 ref
```

未跟踪文件不进 patch、也不复制（避免把整个构建目录搬一遍），只记在元数据里；因为
`--abort` 本身不会删掉它们。`--abort` 失败时会退一步 `--quit` 并显式恢复分支。

## 删除

```console
eix worktree list                    # 先拿到会话 id
eix worktree rm 3f2a91c4             # 有未提交改动时会被 git 拒绝
eix worktree rm 3f2a91c4 --force     # 丢弃未提交文件
```

位置参数走 `resolve_session`，只匹配 session id 或其前缀（多个前缀命中会报 `ambiguous`），
**不认会话名或 worktree 名**。

实现是 `git worktree remove [--force] <path>` 之后补一次 `git worktree prune`。两点注意：

- **分支 `eix/<name>` 不会被删除**，想清理要自己 `git branch -d`。
- 目录已经手工删过时，`remove` 会失败；此时直接 `git worktree prune` 更省事。
- 删除成功后 session 仍在，`cwd` 被改回**主仓库根目录**、`worktree` 字段清空，集成状态
  重置为 `pending`——也就是「搬回共享目录」这条路是存在的。
- session 正在跑回合、或它的集成处于 active 状态时，`remove` 会被拒绝（`busy`），要先等
  回合结束或 `eix integrate continue`。

## 与其他机制的关系

- **快照/rewind 不交叉**：`.worktrees` 在快照排除列表里（与 `.git`、`target`、
  `node_modules` 同级），所以回滚一个 worktree 的会话不会碰到另一个。单个 worktree 的
  快照仍然跳过 `.git`、`target` 等目录。
- **fork/rewind 语义不变**：有 worktree 的 fork 是新 checkout，想回滚文件用 `eix rewind`。
- **HTTP/SDK 客户端**同样走 `session.create { worktree: true }` 与
  `session.worktree.create` / `session.worktree.remove`，字段见
  [`crates/eix-core/src/protocol.rs`](../crates/eix-core/src/protocol.rs)。

## 配置速查

```toml
[worktree]
auto = false              # true: 每个新 session 首条 prompt 时建 worktree
base = "auto"             # 或写死分支名，如 "main"

[integration]
auto_ready = false        # true: 空闲 session 自动 ready
auto_commit = true        # ready 前先提交 worktree 的脏改动
```

除上面这四项（以及 `[integration.repos."<path>"]` 的 `base`）以外，`[integration]` 的其余
键当前不参与反序列化，详见上一节。

`[worktree]` 与 `[integration]` **只能写在全局配置里**：项目配置（仓库内的配置文件）带
这两张表会被丢弃并给出警告，因为它们是可执行策略。

## 常见问题

**`not inside a git work tree`**
`--cwd`（或 daemon 的 cwd）不在 git 仓库里。先 `git init` 或换目录。

**`--worktree` 之后目录建了，但 `eix worktree list` 看不到**
列表只显示带 worktree 的 session；worktree 建好但 session 没创建成时，那次 `git worktree
add` 不会被回滚，用 `git worktree list` 确认后手工 `git worktree remove`。

**想让某个 session 回到共享目录**
`eix worktree rm <SESSION_ID>`：会删掉 worktree 目录，并把 session 的 `cwd` 改回主仓库
根目录（不是它当初的子目录）。不想删目录就新建一个不带 `--worktree` 的 session。

**集成一直失败**
先 `eix integrate status` 看错误；冲突类失败按提示修好后 `eix integrate continue`，
基线不干净就先手动提交/清理基线 worktree，再 `eix integrate retry`。
