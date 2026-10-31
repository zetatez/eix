# 审批、沙箱与权限规则

每次工具调用都要先过一遍权限判断。结果只有四种：直接放行、询问用户、直接拒绝、进沙箱
执行。判断顺序是固定的：先看显式规则，再看工具自身的风险等级。

## 1. 一条规则长什么样

规则写在 `[[tools.rules]]`（`config.tmpl.toml` 里有注释示例）：

```toml
[[tools.rules]]
id = "deny-rm"
tool = "exec_command"
command_regex = "^rm "
action = "deny"
```

| 字段 | 说明 |
|---|---|
| `id` | 稳定标识，出现在审批载荷和审计日志里 |
| `tool` | 工具名，`apply_patch`、`exec_command`、`web_fetch`、`mcp__*` 等，或 `"*"` |
| `path` | workspace 相对 glob，匹配调用的 `path` 参数；`web_fetch` 匹配 URL |
| `command` | argv 前缀，例如 `["git", "status"]` |
| `command_regex` | 正则，逃生口；能用 `command` 就别用它 |
| `signature` | 精确的 JSON 参数，用于没有 path/command 的工具 |
| `action` | `allow` / `ask` / `deny` / `sandbox`，默认 `ask` |

一条规则里的所有已写字段都必须命中才算匹配。`tool` 之外的字段都是可选的，写得越多越具体。

## 2. 优先级

所有匹配的规则都参与判断，不是“第一条命中就结束”：

1. 只要有一条 `deny` 命中，就拒绝。deny 永远优先。
2. 否则在 `allow` / `ask` / `sandbox` 中取**最具体**的一条。具体程度按写了几个约束字段
   计算：`tool != "*"`、`path`、`command`、`command_regex`、`signature` 各算一分。

这意味着“宽泛放行 + 精确收紧”是安全的：`allow` 一条大范围规则，再用更具体的 `deny` 或
`ask` 覆盖特例即可。

## 3. 命令匹配的两个细节

`command` 是 argv token 前缀，不是字符串前缀，所以 `["git", "status"]` 不会误伤
`git statusfoo`。

**多段命令。** `cargo test && rm -rf x` 会被拆成多个 shell 段；对 `allow`/`ask`/`sandbox`
规则，只有**每一段**都以该前缀开头才算匹配。这挡住了“先写个合法前缀，后面接危险命令”的
绕过。

**动态命令。** 含变量展开、命令替换等无法静态确定的写法，不会被命令前缀规则匹配放行；
但 `deny` 规则对这类命令仍会尽量匹配。遇到 `eval`、`$(...)` 一类写法，默认落到 `ask`。

`command_regex` 在配置加载时编译并校验，写错会直接报
`tools.rules[N].command_regex is not a valid regex`，而不是静默失效。

## 4. `sandbox` 动作

`action = "sandbox"` 表示“只在沙箱可用时放行，否则降级为询问”。它适合那些本身需要跑、
但希望被文件系统隔离的命令：

```toml
[[tools.rules]]
id = "cargo-test-sandbox"
tool = "exec_command"
command = ["cargo", "test"]
action = "sandbox"
```

## 5. 沙箱本身

沙箱只作用于 `exec_command`，配置在 `[tools]`：

| 字段 | 默认 | 说明 |
|---|---|---|
| `sandbox` | `"auto"` | `auto` 有 bubblewrap 就用、没有就直接跑；`required` 必须沙箱，否则失败；`off` 不用 |
| `sandbox_network` | `false` | 沙箱内是否允许联网 |
| `sandbox_read_paths` | 空 | 只读挂载的额外路径，支持 `~`；默认不挂整个 home |
| `sandbox_writable_roots` | 空 | 额外可写路径，相对路径按 workspace 解析；workspace 本身始终可写 |

`auto` 是“尽力而为”，不等于隔离保证：机器上没装 bubblewrap 时命令直接在宿主上跑。需要硬
保证时用 `required`，它会选择失败而不是降级。

工具链缓存（`~/.cargo`、`~/.rustup` 等）默认不可见，需要时显式加进 `sandbox_read_paths`。

## 6. 快照与 diff

权限之外还有两层保护，都在 `[tools]`：

| 字段 | 默认 | 说明 |
|---|---|---|
| `snapshots` | 开 | 变更类工具执行前保存可回滚的快照 |
| `auto_apply_workspace_edits` | 开 | 低/中风险写入直接落盘，不再逐次审批；快照仍保留 |
| `turn_diff` | 开 | 每轮结束发 `turn.diff` 通知，给出受跟踪文件的差异 |

把 `auto_apply_workspace_edits` 设为 `false` 就回到“每次文件修改都要看 diff”的模式。

## 7. 常用配方

```toml
# 只读检查全部放行
[[tools.rules]]
id = "read-only-shell"
tool = "exec_command"
command = ["cargo", "check"]
action = "allow"

# 危险命令一律拒绝
[[tools.rules]]
id = "deny-rm"
tool = "exec_command"
command_regex = "^rm "
action = "deny"

# 只允许读某个目录
[[tools.rules]]
id = "review-src"
tool = "apply_patch"
path = "src/**"
action = "ask"

# 只放行某个文档站点
[[tools.rules]]
id = "docs-web"
tool = "web_fetch"
path = "https://docs.rs/**"
action = "allow"
```

最后一条说明 `path` 的双重语义：普通工具匹配文件路径，`web_fetch` 匹配 URL。

## 8. 排错

**规则不生效。** 检查 `tool` 是否是模型看到的工具名：MCP 工具是 `mcp__server__tool`，
不是原工具名。`path` 是 workspace 相对路径，绝对路径匹配不上。

**明明写了 allow 还是被问。** 很可能有更具体的规则命中并给出 `ask`，或者根本就是另一条
deny 命中了。记得 deny 无条件优先，具体度只用于比较 allow/ask/sandbox。

**沙箱没起作用。** `auto` 没装 bubblewrap 时会直接跑。用 `required` 验证，或者在沙箱里
跑一条 `id` 看输出是否来自隔离环境。

**命令被拒但看不出原因。** 多段命令要求每一段都匹配前缀；`cargo test && rm -rf x` 对
`command = ["cargo", "test"]` 的 allow 规则不匹配，这是有意为之。
