# Context 与上下文管理

每一轮请求都要装下：系统提示词、指令文件、工具定义、skill 目录、对话历史、工具输出。
`[context]` 决定这个包如何被裁剪，以及什么时候触发压缩。

## 1. 请求里有什么

按稳定性从高到低：

1. **系统提示词**：内置部分加上 agent 的 `prompt`。
2. **指令文件**：`AGENTS.md` 等，见第 3 节。
3. **工具定义**：内置工具 + MCP 工具 + 插件工具。
4. **skill 目录**：`<available_skills>`，只有一行描述，正文按需读取。
5. **工作区摘要**：cwd 与顶层条目（`workspace_summary`），git 状态单独放在环境段。
6. **对话历史**：用户消息、助手消息、工具调用与结果。

前几项基本是稳定前缀，最后一项持续增长。压缩只动历史，不动前缀。

## 2. 配置项

```toml
[context]
budget_tokens = 120000
auto_compact = true
auto_compact_threshold = 0.8
auto_compact_keep_tokens = 20000
auto_compact_keep_turns = 2
tool_output_tokens = 6000
image_tokens = 1500
instructions_files = ["AGENTS.md"]
workspace_summary = true
```

上面是**示范值**，不是出厂默认。改之前先看 `config.tmpl.toml` 的 `[context]` 段注释，
那里给出的是与你所用模型窗口匹配的默认值。

| 字段 | 作用 |
|---|---|
| `budget_tokens` | 单次请求的软上限，超出时先裁旧工具输出与历史 |
| `auto_compact` | 是否在溢出前自动摘要旧历史 |
| `auto_compact_threshold` | 触发比例，取值 (0, 1]；0.8 表示用到 80% 预算时压缩 |
| `auto_compact_keep_tokens` | 摘要之外**原样保留**的新近对话预算，这是主要的保留控制 |
| `auto_compact_keep_turns` | 至少保留多少个最新用户轮次，即使它们超了上面的预算 |
| `tool_output_tokens` | 单条工具结果进入请求的上限；完整输出仍在事件日志里 |
| `image_tokens` | 一张图片的保守估算，视觉定价因模型而异，可覆盖 |

`auto_compact_keep_tokens` 与 `auto_compact_keep_turns` 的关系是“预算优先、轮次兜底”：
前者决定摘要后留多少原文，后者保证最近几轮永远不会只以摘要形式存在。

## 3. 指令文件

`instructions_files` 是 workspace 相对路径列表，缺文件就跳过。此外，`AGENTS.md` 还会从
eix 配置目录（`~/.config/eix/AGENTS.md`）读取一次。

层级规则与常见 agent 工具一致：越靠近当前目录的文件越晚加入，冲突时后者生效。全局文件放
通用偏好，项目文件放仓库约定。

这些内容进的是“指令”扇区，优先于对话历史被保留。

## 4. 手动控制

| 命令 | 作用 |
|---|---|
| `/context` | 显示当前占用：各扇区估算、预算、剩余空间 |
| `/compact` | 立刻压缩一次，不等阈值 |
| `/effort` | 调整推理力度档位，影响每轮产出长度 |

自动压缩在每轮开始前检查。压缩会把较旧的历史总结成一段摘要，保留最近
`auto_compact_keep_tokens` 的原文，并至少保住 `auto_compact_keep_turns` 个用户轮次。

压缩是有损的：摘要之后的细节只存在于事件日志里，不在模型上下文中。重要事实应当在对话里
复述一次，或者写进 `AGENTS.md`。

## 5. 预算紧张时先牺牲什么

顺序大致是：

1. 单条工具输出超过 `tool_output_tokens` 的部分（最老的先裁）。
2. 更旧的历史进入摘要。
3. skill 目录：先截描述，再去掉描述，最后丢末尾条目。

工具定义与系统提示词不会被裁掉，所以**装太多 MCP server 会挤占对话空间**，而不是被自动
缩减。工具很多时用 `allowed_tools`/`denied_tools` 收窄，或者按需禁用 server。

## 6. 排错

**每轮都很慢、花费高。** 用 `/context` 看是不是工具定义或指令文件吃掉了大半预算；工具输出
上限设得过大也会让历史膨胀。

**压缩后模型“忘了”之前的决定。** 摘要只保留要点。把关键约束写进 `AGENTS.md`，它在压缩中
不受影响。

**刚聊几句就触发压缩。** 检查 `budget_tokens` 是否设得低于模型真实窗口，以及
`auto_compact_threshold` 是否太小。

**skill 目录里少了几个。** 目录上限是上下文窗口的 2%、最多 8 KiB，超出时按顺序丢弃末尾条目。
