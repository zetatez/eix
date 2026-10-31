# Sub-agent

sub-agent 是一个由主 agent 派生、在后台独立跑一轮（或几轮）的子会话。它有自己的会话 id、
自己的工具调用记录，最后把结果作为一条工具结果回给父会话。

适合：一个明确、可独立完成的调查或改造任务，且中间过程不需要人盯着。

## 1. 定义 sub-agent

就是普通的 agent 文件，`role` 标成 `subagent`：

```markdown
---
name: test-writer
description: Add or repair tests for one module
role: subagent
model: claude-sonnet-4
---

You write tests. Read the module, find the untested branches, add focused tests,
and run them. Report the command you ran and its result.
```

字段与覆盖规则见 `docs/skills.md`。标成 `subagent` 的 agent 不会出现在主 agent 选择里，
只能被 `spawn_agent` 调用。

内置的 `explore` 可以直接用：只读检索与分析，返回带 `file:line` 的证据，不编辑文件、不跑
变更命令、不提问。

## 2. 调用

模型通过 `spawn_agent` 工具发起，参数里用 `agent` 指定名字。没有合适的自定义 agent 时，
可以省略 `agent` 走默认配置。

一次回复里的多个 `spawn_agent` 会并发启动，上限由 `[tools] max_parallel_subagents`（默认值
见 `config.tmpl.toml`）控制。运行中可以在 TUI 里临时调整，取值必须是
2、4、8、16、32、64、128、256 之一；配置重载或 daemon 重启会回到配置里的值。

## 3. 深度与并发

**深度限制为 1。** sub-agent 不能再派生 sub-agent，尝试时会得到明确错误，而不是无限递归。

**并发规则。** 同一批工具调用里，`spawn_agent` 一律并行；其他工具只有在被判定为只读时才
并行，写操作是屏障。屏障保证子 agent 不会看到“改了一半”的工作区。

**结果回填。** 每个子会话的结果作为一条工具结果追加到父会话，父 agent 看到的是结论，不是
子会话的完整过程。过程留在子会话的事件日志里，需要时可以单独打开。

## 4. 成本与可观测性

每个 sub-agent 是独立的模型会话，token 与调用次数都单独计费。并发度直接决定峰值成本，
`max_parallel_subagents` 既是性能旋钮也是预算旋钮。

子会话的来源标记为 agent，因此：

- 它们会出现在会话列表里，可以像普通会话一样查看与回放。
- 子会话不会自己弹出审批；需要人决定的操作在父会话里处理。
- 取消父轮次会传播到子会话。

## 5. 写好一个 sub-agent

- **一个任务，一个子 agent。** “调查为什么 X 失败”比“看看这个仓库”有效得多。任务越窄，
  结果越可用。
- **在 prompt 里写清产出格式。** 要求结论 + 证据（`file:line`）+ 未决问题，父 agent 才能直接
  用，而不必再问一轮。
- **明确禁止越界。** 只读调查就写明不编辑、不跑变更命令，避免子 agent 顺手改文件。
- **别让它读整份 skill。** skill 由父会话按需加载；把需要的片段写进 prompt 更省预算。

## 6. 排错

**报“depth limit is 1”。** 当前会话已经是 sub-agent，不能再派生。把任务拆到父会话里发起。

**并发没提上去。** 检查 `max_parallel_subagents` 是否被配置重载改回；TUI 里的临时调整
不持久。

**结果很泛。** 通常是任务描述太宽，或者没有要求证据。收窄问题并在 prompt 里规定输出格式。

**子会话卡住。** 取消父轮次会一并取消；也可以直接打开子会话查看它停在哪一步。
