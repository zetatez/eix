# Skills、Commands 与 Agents

这三类扩展都是 Markdown 文件，都在启动或使用时从磁盘读取，都不需要重新编译。区别在于
它们进入模型的方式。

| | Skills | Commands | Agents |
|---|---|---|---|
| 目录 | `skills/<name>/SKILL.md` | `cmds/**.md` | `agents/*.md` |
| 触发 | 模型按需 `use_skill` | 用户输入 `/name` | 用户选择或 `spawn_agent` |
| 进入上下文 | 先只放一行目录，调用后放全文 | 展开后的提示词 | 追加到系统提示词 |
| 主要字段 | `description`、`allowed_tools` | `description`、`agent`、`model` | `role`、`model`、`description` |

## 1. Skills

### 目录与优先级

全局：`~/.config/eix/skills/<name>/SKILL.md`（受 `EIX_CONFIG_DIR` 影响）。
项目：`.eix/skills/`。同名时项目覆盖全局，用户覆盖内置。

内置 skill（例如 `frontend`）随程序提供，可以直接使用，也可以被同名文件覆盖。

### 文件格式

带 frontmatter 的 Markdown：

```markdown
---
name: release-notes
description: Turn a git log into release notes. Use for "release notes" or "changelog".
allowed_tools: ["exec_command", "read_file"]
---

# Release notes

Read the range with `git log`, group changes by type, then write the notes.
```

`description` 决定模型是否知道该在什么时候用它，写成“做什么 + 什么时候用”。正文是完整
指令，模型调用 `use_skill` 后读到的就是这部分。

`allowed_tools` 是可选限制：加载该 skill 后，模型仍可使用的工具；留空表示不限制，支持
`*` 和 `?` 通配。

### 目录如何进入上下文

eix 不会把所有 skill 正文塞进请求。它只放一个 `<available_skills>` 目录，每条一行名字加
描述，正文留在磁盘上，由 `use_skill` 按需读取。

目录本身也占预算：上限是上下文窗口的 2%，最多 8 KiB。超了会按顺序降级——先截断描述，
再减半，再去掉描述，最后丢掉末尾条目。也就是说 skill 装得太多时，靠后的条目可能不出现在
目录里；精简描述比堆数量更有用。

`use_skill` 支持 `arguments`，供正文里使用 `$ARGUMENTS` / `$1`..`$9` 的 skill 使用。

## 2. Commands

### 目录

| 位置 | 用途 |
|---|---|
| `~/.config/eix/cmds/` | 全局命令 |
| `<project>/.eix/cmds/` | 项目命令，可用子目录分组 |
| `~/.config/eix/commands/` | 旧目录名，仍然可读 |
| `<project>/.opencode/commands/` | OpenCode 兼容目录 |

子目录会进入命令名：`cmds/team/review.md` 的命令是 `/team/review`，名字用 `/` 分隔。
同名时更近的项目源覆盖全局与祖先目录。

### 正文与参数

文件正文就是提示词模板，frontmatter 可选：

```markdown
---
description: Review the staged diff
agent: reviewer
model: claude-sonnet-4
---

Review the staged changes. Focus on $ARGUMENTS.
Diff stat: $1
```

| 占位符 | 展开为 |
|---|---|
| `$ARGUMENTS` | 整个参数字符串 |
| `$1`、`$2`… | 按位置取，最高的占位符吞掉剩余参数 |

`agent` 指定用哪个 agent 处理，`model` 覆盖本次使用的模型；两者都可省略。

## 3. Agents

### 定义

`~/.config/eix/agents/*.md` 或项目 `.eix/agents/*.md`：

```markdown
---
name: reviewer
description: Review changes for correctness and scope
role: primary
model: claude-sonnet-4
---

You review diffs. Report findings by severity with file:line evidence.
```

`prompt`（即正文）追加到内置系统提示词之后，不会替换它。

`role` 决定用途：

| 值 | 含义 |
|---|---|
| `primary` / `main` | 只能作为主 agent 使用 |
| `subagent` / `sub` | 只能被 `spawn_agent` 调用 |
| 其他或省略 | 两者皆可 |

### 内置 agent

`explore` 是内置的只读 sub-agent：只做检索与分析，不编辑文件、不跑变更命令、不提问，返回
带 `file:line` 的证据。需要“先查清再改”的时候直接用它，比让主 agent 边查边改更稳。

内置的读写工作流属于会话模式，不是 agent，不会出现在 agent 列表里。

### 覆盖顺序

项目覆盖全局，用户覆盖内置。按名字覆盖，不做合并——同名文件整份替换。

## 4. 兼容其他工具

除上述目录外，eix 还能读取其他 harness 的资源，由配置里的兼容开关逐项控制：
`claude_code`、`opencode`、`codex_cli`、`gemini_cli`、`pi`。

打开后会额外扫描对应位置，例如 Claude Code 的 `~/.claude` 与项目 `.claude`、OpenCode 的
`~/.config/opencode` 与 `.opencode`、Codex 的 `$CODEX_HOME` 与 `.codex`、Gemini 的
`~/.gemini` 与 `.gemini`，以及 Pi 的 `.pi` / `.agents`。技能、命令、agent 的映射规则各自
不同：例如 Codex 的 `prompts/` 被当作命令读入。

兼容层只读不写，不会修改这些目录。

## 5. 排错

**斜杠命令不出现。** 确认文件在 `cmds/` 下、扩展名是 `.md`、并且名字没有拼错；子目录会
变成命令名的一部分（`/team/review`）。

**skill 不触发。** 先看 `description` 是否写清“何时使用”。目录预算紧张时，过长的描述会被
截断甚至整条省略。

**改了文件没生效。** skill 与 agent 在查找时重新读盘，命令在会话加载时读入；改动后重开
会话最稳。

**agent 不可选。** 检查 `role`：标成 `subagent` 的只能被 `spawn_agent` 调用，不会出现在
主 agent 选择里。
