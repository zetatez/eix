# eix dylib 插件系统设计（ABI v1）

- 状态：**P1 已落地，P2 部分落地**。`eix-plugin-abi` / `eix-plugin-sdk` 骨架、`eixd` 的 `ExtensionBackend` 抽象、`ProcessBackend` 适配与 `DylibBackend` 加载器均已实现；`describe` / `tool.call` / `command.run` 可在真实插件路径中调用，`hook.run` / `rpc.call` / `event.notify` / `config.changed` 也已接通。
- 目标：原生性能、极深扩展点、新增能力不需要修改 ABI
- 关联代码：`crates/eix-plugin-abi/`、`crates/eix-plugin-sdk/`、`crates/eixd/src/plugin/{backend,process,dylib}.rs`、`examples/plugins/hello-dylib/`
- 现有实现：`crates/eixd/src/plugin/` 同时支持进程外 JSON-RPC（`ProcessBackend`）与进程内 dylib（`DylibBackend`），共享同一套注册表、能力校验与 hook 路由。

本文描述如何用 **dylib（动态链接库）** 实现一个比现有进程外 JSON-RPC 插件更深、更快、更原生的扩展系统，同时保留可演进性。当前已覆盖 `describe` / `tool.call` / `command.run` / `hook.run` / `rpc.call` / `event.notify` / `config.changed`，以及同步安全的 host API（log / storage / secrets / fs / session 读 / blob）；`net` / `ui` / `job` / `host.rpc.call` 需要 async 桥接，仍是后续阶段目标。本文不替代 [`plugins.md`](plugins.md) 中已落地的 v2 协议。

### 1.0 P1 使用方式

```toml
# ~/.config/eix/config.toml
[plugins.hello-dylib]
enabled = true
# 绝对路径；相对路径按 config cwd 解析。
library = "/opt/eix/plugins/hello-dylib/libeix_plugin_hello_dylib.so"
capabilities = ["tools", "commands"]
timeout_secs = 30
```

或以 manifest 声明（相对路径按 manifest 目录解析）：

```toml
# eix-plugin.toml
schema = 2
name = "hello-dylib"
version = "0.1.0"
abi = 1
capabilities = ["tools", "commands"]

[components.dylib]
path = "libeix_plugin_hello_dylib.so"
```

```toml
[plugins.hello-dylib]
enabled = true
manifest = "/opt/eix/plugins/hello-dylib/eix-plugin.toml"
```

规则：

- `library` 与 `command` / `components.server` 互斥，同时声明会启动失败。
- `library` 可以是绝对路径；相对路径按 config cwd 解析。manifest 的 `components.dylib.path` 相对 manifest 目录解析。
- manifest 的 `abi` 与实际导出的 `eix_plugin_abi_version` 都会校验，不匹配直接拒绝加载。
- `plugin.list` 的 `command` 字段展示库路径；实际后端由 `library` 是否存在决定。

---

## 1. 目标与非目标

### 1.1 目标

- 插件与内置功能共享同一套贡献注册表、hook 管线、UI 模型和权限系统。
- 插件可以贡献工具、命令、hooks、prompt/context、provider、agent、skill、theme、keymap、MCP、UI、RPC、后台任务。
- 新增一种贡献点或一个 host 方法时，**不修改 ABI、不要求旧插件重新编译**。
- 调用开销接近函数调用，而不是管道 + JSON。
- 插件作者写起来像写内置 Rust 功能：实现一个 trait，加一个宏。
- 保留签名、安装路径、能力 allowlist 等供应链约束。

### 1.2 非目标

- 不把 dylib 当作安全沙箱。in-process dylib 拥有 daemon 的全部权限。
- 不追求“任意第三方代码安全运行”。需要隔离时使用 hosted 模式或 WASM。
- 不保证跨 Rust 版本二进制兼容；ABI 稳定的是 C 边界，不是 Rust 类型布局。
- 不做进程内 `dylib` 的硬超时/强制终止；这需要 helper process。
- 不替代现有 JSON-RPC 后端；两者共存，按插件选择。

---

## 2. 硬约束

设计必须围绕以下事实：

1. **Rust 没有稳定 ABI**。跨 dylib 边界只能传 `#[repr(C)]` POD、指针、长度、整数、不透明句柄；不能传 `String`、`Vec`、`Arc`、trait object、`Result`。
2. **dylib 与宿主同进程**。插件 panic、段错误、`process::exit` 会直接影响 daemon。
3. **dylib 基本不能安全卸载**。Rust TLS、静态变量、后台线程使 `dlclose` 不可靠；热重载只能加载新副本、切换注册表、旧副本常驻。
4. **同步 ABI 不能硬杀**。插件卡死时，in-process 方案只能超时隔离，不能像子进程一样 `kill`。
5. **能力系统只约束 host API**。插件可以直接调用 `libc` 绕过 `host.fs` / `host.net`。

因此，dylib 插件应被视为**完全可信代码**；安全边界放在“加载前”的签名与安装路径校验。

---

## 3. 总体架构

```text
┌──────────────────────────────────────────────────────────────┐
│ eixd                                                         │
│                                                              │
│  turn / tool / prompt / approval / UI pipeline                │
│                     │                                        │
│           ContributionRegistry（强类型、按 kind 索引）         │
│                     │                                        │
│           ExtensionBackend trait                             │
│      ┌──────────────┼──────────────┬──────────────┐           │
│      │              │              │              │           │
│  BuiltinBackend  DylibBackend  ProcessBackend  WasmBackend   │
│   (编译进主程序)   (dlopen)      (JSON-RPC)      (可选)        │
│                     │                                        │
│              HostApiV1 + plugin dispatch                     │
│              （5 个 C 符号，永不增长）                         │
└──────────────────────────────────────────────────────────────┘
```

关键点：**dylib 只是 backend 之一**。注册表、权限、UI、hook 管线对内置功能和插件一视同仁，这才算“原生”。

---

## 4. ABI v1

### 4.1 设计原则

- **ABI 永不增长**：所有能力走字符串方法 + JSON 参数。
- 只暴露 5 个插件符号：`abi_version`、`create`、`dispatch`、`free`、`destroy`。
- host 侧只暴露一张 `HostApiV1` 表：`dispatch` + `free`。
- 新贡献点 = 新 JSON section；新方法 = 新字符串。旧插件不受影响。
- 边界只传字节、长度、指针、整数。

### 4.2 基础类型

```rust
pub const ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EixStr {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct EixBuf {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}
```

- `EixStr`：借用的 UTF-8 字节，生命周期到调用返回。
- `EixBuf`：所有权的字节 buffer，不能 `Copy`，必须且只能释放一次。
- 二进制大对象不内联，走 `host.blob.*` 或 base64。

### 4.3 Host API 表

```rust
pub type HostDispatchFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    method: EixStr,
    params: EixStr,
    out: *mut EixBuf,
) -> i32;

pub type HostFreeFn = unsafe extern "C" fn(ctx: *mut c_void, buf: EixBuf);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApiV1 {
    pub abi_version: u32,
    /// sizeof(HostApiV1)，允许未来追加字段
    pub size: usize,
    pub ctx: *mut c_void,
    pub dispatch: Option<HostDispatchFn>,
    pub free: Option<HostFreeFn>,
}
```

- 插件检查 `abi_version == ABI_VERSION` 且 `size >= size_of::<HostApiV1>()`。
- 未来 host 追加字段时，旧插件只读自己认识的 prefix。
- `dispatch` / `free` 用 `Option` 表示可空函数指针，FFI 布局与裸指针一致。

### 4.4 插件导出符号

```rust
pub type PluginAbiVersionFn = unsafe extern "C" fn() -> u32;

pub type PluginCreateFn = unsafe extern "C" fn(
    host: *const HostApiV1,
    config: EixStr,
    out: *mut *mut c_void,
) -> i32;

pub type PluginDispatchFn = unsafe extern "C" fn(
    instance: *mut c_void,
    method: EixStr,
    params: EixStr,
    out: *mut EixBuf,
) -> i32;

pub type PluginFreeFn = unsafe extern "C" fn(instance: *mut c_void, buf: EixBuf);

pub type PluginDestroyFn = unsafe extern "C" fn(instance: *mut c_void);
```

对应符号名：

```text
eix_plugin_abi_version
eix_plugin_create
eix_plugin_dispatch
eix_plugin_free
eix_plugin_destroy
```

### 4.5 状态码

```rust
pub const EIX_OK: i32 = 0;
pub const EIX_ERR_UNKNOWN_METHOD: i32 = -1;
pub const EIX_ERR_INVALID: i32 = -2;
pub const EIX_ERR_DENIED: i32 = -3;
pub const EIX_ERR_PANIC: i32 = -4;
pub const EIX_ERR_VERSION: i32 = -5;
pub const EIX_ERR_REENTRANT: i32 = -6;
pub const EIX_ERR_TIMEOUT: i32 = -7;
pub const EIX_ERR_INTERNAL: i32 = -8;
```

错误详情放 `out` 的 JSON：

```json
{"code":"invalid_params","message":"missing field `path`"}
```

即使状态码非 0，host 也应读取 `out`（如果有）并调用 `eix_plugin_free`，否则会泄漏错误详情。

### 4.6 内存所有权

| 方向 | 谁分配 | 谁释放 |
| --- | --- | --- |
| host → plugin 入参 `EixStr` | host | host，调用返回后 |
| plugin → host 返回值 `EixBuf` | plugin | host 调 `eix_plugin_free` |
| plugin → host 入参 `EixStr` | plugin | plugin，调用返回后 |
| host → plugin 返回值 `EixBuf` | host | plugin 调 `HostApiV1::free` |

规则：

- 禁止跨边界传 `String` / `Vec` / `Arc` / trait object。
- 插件返回 `EixBuf` 用 `Vec::into_raw_parts` 构造；`eix_plugin_free` 用 `Vec::from_raw_parts` 还原。
- 入参 buffer 由调用方持有，被调用方不得保存指针。
- 禁止返回指向静态内存的 buffer。

### 4.7 Panic、线程与重入

- 每个 `extern "C"` 入口内部 `catch_unwind(AssertUnwindSafe(...))`。
- 插件和宿主都使用 `panic = "unwind"`；`panic = "abort"` 无法 catch。
- 绝不让 unwind 跨 FFI 边界。
- 插件禁止调用 `std::process::exit`。
- host 必须保证同一实例的 `dispatch` 不并发。ABI v1 只支持 `"threading": "serialized"`。
- 未来如需真正并发，把 trait 改成 `&self` + 内部可变性，或由 host 为每个实例加锁；`&mut self` 不能被多线程同时调用。
- dispatch 期间禁止回调同一实例；需要递归时返回 `EIX_ERR_REENTRANT`，由插件改为异步任务。
- dylib 调用是同步的；host 必须放在 blocking worker 上，不能阻塞 async executor。

---

## 5. 方法表

### 5.1 host → plugin

| 方法 | 参数 | 返回 |
| --- | --- | --- |
| `describe` | `{}` | `Descriptor` |
| `tool.call` | `{name, arguments, session_id, workspace, cwd}` | 工具结果 |
| `command.run` | `{command, arguments, session_id, workspace, cwd}` | `{ok, prompt?, output?}` |
| `hook.run` | `{point, session_id, turn_id, ...}` | hook 结果 |
| `ui.render` | `{view_id, revision, surface}` | `UiNode` |
| `ui.action` | `{view_id, action_id, payload}` | 任意 JSON |
| `event.notify` | `{method, ...}` | `{}` |
| `config.changed` | 新设置 | `{}` |
| `provider.list` | `{}` | 模型列表 |
| `provider.stream` | `{model, messages, ...}` | `{handle}` |
| `provider.next` | `{handle}` | `{chunk?, done}` |
| `rpc.call` | `{method, arguments, caller, depth}` | 任意 JSON |
| `job.run` | `{job_id}` | `{}` |
| `lifecycle.start` | `{}` | `{}` |
| `lifecycle.stop` | `{}` | `{}` |
| `health` | `{}` | `{ok}` |

未识别的方法交给 `Plugin::dispatch` 的默认实现，返回 `unknown_method`；插件可以覆盖它来支持更新的 host 方法。

### 5.2 plugin → host

| 方法 | P1 | 说明 |
| --- | --- | --- |
| `host.log` | 已支持 | 写 daemon 日志 |
| `host.info` | 已支持 | 返回插件名、后端类型、能力与路径 |
| `host.ping` | 已支持 | 健康检查 |
| `host.notify` | 已支持 | toast / `plugin.notify` 事件 |
| `host.storage.get/set/delete/list` | 已支持 | KV，`global` / `workspace` 作用域 |
| `host.secrets.get` | 已支持 | 环境变量白名单；只允许 `plugins.<name>.secrets` |
| `host.fs.read/list/write` | 已支持 | 与进程后端共用 `WorkspaceFs`：canonicalize、拒绝 `..` 与符号链接、单文件 ≤1 MiB |
| `host.net.request` | 暂不支持 | 需要 async 桥接 |
| `host.session.get/list` | 已支持 | 只读，需 `sessions_read` 能力 |
| `host.ui.open/update/close` | 暂不支持 | 需要 UI 事件桥接 |
| `host.rpc.call` | 暂不支持 | 需要 async 桥接 |
| `host.blob.put/get` | 已支持 | 与进程后端共用内容寻址实现，单个 ≤8 MiB |
| `host.job.register/trigger/unregister` | 暂不支持 | 需要 async 桥接 |
| `host.register/unregister` | 设计目标 | 动态贡献 |
| `host.config.get` | 设计目标 | 当前设置 |

同步安全的方法（log / storage / secrets / fs / session 读 / blob）直接实现；net / ui / rpc / job 需要 await 或回到 daemon 事件循环，在 P2 之前明确返回 `EIX_ERR_DENIED`，并提示改用 hosted/process 模式。

`config.changed` 是唯一的非事件通知：`DylibBackend::notify` 把它翻译成 `config.changed` dispatch，SDK 收到后更新缓存并调用 `Plugin::config_changed`。其余通知仍包成 `event.notify {method, params}` 交给 `Plugin::event`。

新增 host 方法只需在 host 侧加一个字符串分支；ABI 不变。

---

## 6. 描述符与贡献注册

`create` 之后，host 调用：

```text
dispatch("describe", "{}") -> Descriptor
```

```json
{
  "name": "acme",
  "version": "2.1.0",
  "abi": 1,
  "capabilities": ["tools", "hooks", "ui", "network", "storage"],
  "threading": "serialized",
  "contributions": {
    "tools": [
      {"name": "search", "description": "...", "input_schema": {...}, "read_only": true}
    ],
    "commands": [
      {"name": "sync", "action": "sync", "args": "[--full]"}
    ],
    "hooks": [
      {"point": "tool.before", "priority": 10, "on_error": "deny"}
    ],
    "events": ["session.*", "turn.*"],
    "ui": [
      {"surface": "panel", "id": "issues", "title": "Issues"}
    ],
    "providers": [
      {"id": "acme", "models": ["acme-large"], "streaming": true}
    ],
    "agents": [],
    "skills": [],
    "themes": [],
    "keymaps": [],
    "mcp_servers": [],
    "rpc": [{"name": "issues.search", "description": "Search cached issues"}]
  },
  "settings_schema": {"type": "object", "properties": {}}
}
```

host 把它拆进 `ContributionRegistry`，并做三件事：

1. 命名空间校验：`plugin__<name>__<tool>`、`plugin/<name>/<command>`、`plugin.<name>.*`。
2. 能力校验：声明 hooks 但没拿 `hooks` 能力 → 启动失败。
3. 冲突检测：同名贡献拒绝；deny 优先。

运行期也可以动态注册：

```text
host.dispatch("register", {"kind": "tool", ...})
host.dispatch("unregister", {"kind": "tool", "name": "search"})
```

例如按 workspace 动态暴露工具，不需要重启。

---

## 7. 深度扩展点

| 管线 | 扩展方式 |
| --- | --- |
| 模型工具 | `tools` + `tool.call`，支持流式输出、`read_only`、审批策略 |
| 命令 | `commands` + `command.run`，prompt/action 二选一 |
| Prompt | `prompt.build` hook，可 append context blocks、改写 system prompt |
| 模型请求 | `model.request` hook，可改 temperature、工具白名单、注入消息 |
| 模型响应 | `model.response` hook，可脱敏、标注 |
| Provider | `providers` + `provider.*`，插件可贡献 OpenAI-compatible provider |
| Agent | `agents`，声明 prompt、工具集、模型偏好 |
| Skill | `skills`，提供 `SKILL.md` |
| Hook | `tool.before/after`、`approval.before`、`file.before_write/after_write`、`session.*`、`compact.*`、`turn.*` |
| UI | `ui.render` 返回节点树，host 渲染；`ui.action` 回传交互 |
| Keymap | `keymaps`，命名空间 `plugin.<name>.*` |
| Theme | `themes`，只能引用/新增 token，不能覆盖内置主题 |
| Settings | `settings_schema` + `config.changed` |
| Event | `events`，带过滤和背压策略 |
| 存储 | `host.storage.*` / `host.blob.*` |
| 后台任务 | `host.job.*` |
| 插件间调用 | `host.rpc.call` |
| MCP | 声明 `mcp_servers`，复用现有 MCP 客户端 |

### UI 节点树

```json
{
  "kind": "list",
  "id": "issues",
  "title": "Issues",
  "items": [{"id": "42", "label": "#42 Fix parser", "detail": "open"}],
  "actions": [
    {"id": "open", "label": "Open", "key": "Enter"},
    {"id": "refresh", "label": "Refresh", "key": "r"}
  ]
}
```

支持节点：`text`、`markdown`、`list`、`table`、`tree`、`tabs`、`form`、`diff`、`log`、`progress`、`sparkline`、`actions`、`spacer`。

限制：2000 节点、深度 16、单文本 64 KiB；未知 `kind` 渲染为错误占位，不崩溃。

### 新增一种贡献点需要改什么

只需要三处，**都不碰 ABI**：

1. Descriptor JSON 增加一个 section。
2. host `ContributionRegistry` 增加一个索引和管线接入点。
3. SDK 增加一个链式 helper（如 `.provider(...)`）。

---

## 8. 生命周期

```text
dlopen
  → eix_plugin_abi_version
  → eix_plugin_create(host, config)
  → dispatch("describe")
  → 注册贡献
  → dispatch("lifecycle.start")
  → 运行
  → dispatch("lifecycle.stop")
  → eix_plugin_destroy
  → 不 dlclose
```

### 8.1 热重载

```text
新 .so 复制到唯一路径（libacme-<hash>.so）
  → dlopen 新副本
  → create + describe
  → 原子切换注册表
  → destroy 旧实例
  → 旧 Library 常驻（不 dlclose）
```

不要覆盖原文件后 `dlopen` 同一路径，动态链接器可能返回旧 handle。用版本化文件名。

P1 的 `plugin.restart` 会重新走 `DylibBackend::load` 并销毁旧实例，但库句柄按设计常驻；要让新代码生效，请把新构建复制成带版本号的新路径，再修改配置指向它。覆盖原路径后重启只能保证旧映射仍被引用，不保证拿到新代码。

### 8.2 超时

- host 在 blocking worker 上调用 `dispatch`，外层 `tokio::time::timeout`。
- 同一实例的调用由 `std::sync::Mutex` 串行化；`spawn_blocking` 保证不阻塞 async executor。
- 超时后调用 `mark_closed`，`plugin.list` 显示 failed，后续请求直接拒绝。
- **不能杀线程**；如果插件可能永久卡死，使用 hosted 模式。P1 的 `shutdown` 用 `try_lock`，不会等待卡死的调用；实例由最后一个 `Arc` 释放时销毁。

### 8.3 崩溃

in-process dylib 崩溃 = daemon 崩溃。缓解：

- 只加载签名且受信任的插件。
- 关键插件放 helper process。
- 崩溃后由 supervisor 重启 daemon，并标记插件 failed。

### 8.4 双模式

P1 只实现 `inproc`（daemon 进程内）。hosted 模式是后续目标：

```toml
[plugins.acme]
# P1：直接声明库
library = "/path/to/libacme.so"

# P4 目标：dylib 跑在专用子进程
# mode = "hosted"
```

`hosted` 模式仍然是 dylib，只是加载进专用子进程，通过共享内存或 Unix socket 通信。它保留原生代码和低开销，同时恢复崩溃隔离和硬超时。

**建议：第三方默认 `hosted`，first-party 用 `inproc`。**

---

## 9. 安全模型

必须明确：

- **in-process dylib 没有沙箱**。插件可以直接 `libc::open`、读环境变量、发网络，绕过 `host.*` 能力检查。
- 因此 dylib 插件必须被视为**完全可信代码**。
- 安全边界放在“加载前”：
  - 只从 `~/.config/eix/plugins` 加载；
  - 校验 `eix-plugin.sums` + Ed25519 签名；
  - manifest 声明 `abi`、`target`、`artifact`；
  - host 拒绝路径逃逸、symlink、target 不匹配。
- 需要真正隔离时用 `hosted` 模式 + Landlock/seccomp/单独用户。
- 能力 allowlist 仍然保留，用于 hosted 模式、defense-in-depth 和审计。

不要宣称 in-process dylib 是安全的。它不是。

---

## 10. SDK 使用

### 10.1 插件侧

```rust
use eix_plugin_sdk::prelude::*;

#[derive(Default)]
struct Acme;

impl Plugin for Acme {
    fn describe(&self) -> Descriptor {
        Descriptor::new("acme")
            .version(env!("CARGO_PKG_VERSION"))
            .capabilities(["tools", "hooks", "ui", "storage"])
            .tool(
                Tool::new("search", json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                }))
                .description("Search cached issues")
                .read_only(),
            )
            .command(Command::action("sync", "sync").args("[--full]"))
            .hook(Hook::new("tool.before").priority(10).on_error("deny"))
            .ui_panel("issues", "Issues")
            .rpc("issues.search")
    }

    fn call_tool(&mut self, ctx: &mut Ctx<'_>, name: &str, args: Value) -> Result<Value> {
        match name {
            "search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or_default();
                ctx.log("info", &format!("search: {query}"))?;
                Ok(json!({
                    "content": [{"type": "text", "text": "found 0 issues"}],
                }))
            }
            other => Err(Error::UnknownMethod(other.to_string())),
        }
    }
}

eix_plugin!(Acme);
```

`eix_plugin!` 负责：

- 生成 5 个 C 符号；
- 包装 panic；
- JSON 编解码；
- 内存释放；
- 把 `describe` / `tool.call` / `command.run` / `hook.run` / `ui.render` / `ui.action` / `event.notify` / `config.changed` / `lifecycle.*` 路由到 trait 方法；
- 未识别方法转给 `Plugin::dispatch`。

### 10.2 host 侧加载器（示意）

```rust
pub struct DylibBackend {
    _lib: libloading::Library,
    instance: *mut c_void,
    dispatch: abi::PluginDispatchFn,
    free: abi::PluginFreeFn,
    destroy: abi::PluginDestroyFn,
}

impl DylibBackend {
    pub fn load(path: &Path, host: &abi::HostApiV1) -> anyhow::Result<Self> {
        let lib = unsafe { libloading::Library::new(path)? };
        let version: libloading::Symbol<abi::PluginAbiVersionFn> =
            unsafe { lib.get(b"eix_plugin_abi_version\0")? };
        if unsafe { version() } != abi::ABI_VERSION {
            anyhow::bail!("plugin ABI mismatch");
        }
        let create: libloading::Symbol<abi::PluginCreateFn> =
            unsafe { lib.get(b"eix_plugin_create\0")? };
        let mut instance = std::ptr::null_mut();
        let code = unsafe { create(host, abi::EixStr::EMPTY, &mut instance) };
        anyhow::ensure!(code == abi::EIX_OK, "create failed: {}", abi::status_name(code));
        // 取出 dispatch/free/destroy 符号并保存。
        // ...
    }
}
```

实际实现中要注意 `libloading::Symbol` 的生命周期：符号借用 `Library`，保存函数指针时要先解引用再存成裸 fn pointer。

---

## 11. 构建与分发

### 11.1 构建

```bash
cargo build -p eix-plugin-hello-dylib --release
```

输出：

```text
target/release/libeix_plugin_hello_dylib.so   # Linux
target/release/libeix_plugin_hello_dylib.dylib # macOS
target/release/eix_plugin_hello_dylib.dll      # Windows
```

### 11.2 Manifest 扩展

P1 已支持 `abi` 与 `components.dylib`：

```toml
schema = 2
name = "acme"
version = "2.1.0"
abi = 1
capabilities = ["tools", "commands"]

[components.dylib]
path = "libacme.so"

[package]
publisher = "acme"
signature = "eix-plugin.sig"
checksums = "eix-plugin.sums"
```

`components.dylib.path` 相对 manifest 目录解析，必须是非空相对路径（绝对路径只在 config 的 `library` 中允许）。`components.server` 与 `components.dylib` 互斥。

`target` / `artifact` 已解析：`artifact` 优先于 `path`；`target` 支持 `linux-x86_64`、`x86_64-unknown-linux-gnu`、`amd64` 等别名，加载时与当前平台比较，不匹配直接拒绝。

### 11.3 安装

```bash
eix plugin pack dist/
# 用外部 Ed25519 工具签 eix-plugin.sums，把 base64 签名写到 eix-plugin.sig
eix plugin verify dist/
eix plugin install dist/
```

本地开发可以 `eix plugin install dist/ --allow-unsigned`；它会为没有 `[package]` 的目录补表并重新 `pack`。安装时校验：

- `abi` 与 host 支持范围匹配；
- `min_host_version` 不高于当前 host；
- `target` 与当前平台匹配；
- checksums 与 Ed25519 签名（`require_signature = true` 时强制）；
- `artifact` 在包内，无路径逃逸、无 symlink。

安装后版本与 checksums 写入 `plugins/plugins.lock`；`eix plugin verify-lock`（或库函数 `verify_lock`）会报告漂移。

---

## 12. 与现有系统的关系

现有 `PluginManager` 不推翻，改成 backend。P1 实际签名用显式 `Pin<Box<dyn Future + Send + 'a>>`，避免引入 `async-trait`：

```rust
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ExtensionBackend: Send + Sync {
    fn name(&self) -> &str;
    fn initialize<'a>(&'a self, spec: &'a PluginSpec)
        -> BackendFuture<'a, Result<PluginInitializeResult>>;
    fn request<'a>(&'a self, method: &'a str, params: Value)
        -> BackendFuture<'a, Result<Value>>;
    fn notify<'a>(&'a self, method: &'a str, params: Value)
        -> BackendFuture<'a, Result<()>>;
    fn is_closed(&self) -> bool;
    fn exit_reason<'a>(&'a self) -> BackendFuture<'a, Option<String>>;
    fn shutdown<'a>(&'a self) -> BackendFuture<'a, ()>;
}
```

后端方法名沿用 v2 协议名（`tools/call`、`commands/run`、`hooks/run`、`rpc.call`）；`DylibBackend` 内部再翻译成 `tool.call` / `command.run` / `hook.run`，并把 `tools/call` 的 `tool` 参数重命名为 `name`。

- `ProcessBackend`：现有 JSON-RPC 子进程，直接适配。
- `DylibBackend`：新增，P1 已接入 `PluginManager` 的启动、工具、命令、hook、事件、RPC 与 shutdown 路径。
- `BuiltinBackend`：内置功能也可以走同一注册表（后续）。
- `ContributionRegistry`：复用现有 `PluginManager` 的 `tools/commands/subscriptions` 注册表，不再单独抽象。
- TUI 不需要知道 backend 类型。

这样 dylib 是增量能力，不是重写。

### 迁移顺序

1. ✅ 抽出 `ExtensionBackend`，现有 `PluginManager` 适配为 `ProcessBackend`。
2. ✅ 接入 `DylibBackend`，先支持 `describe` / `tool.call` / `command.run`。
3. 加入 hooks / UI / RPC / provider（hook/event/RPC 路由已复用，host 侧 async 方法待补）。
4. 增加 hosted 模式与签名校验。
5. 最后再考虑 WASM。

---

## 13. 陷阱清单

1. **Rust ABI 不稳定**：宿主升级 Rust 后，旧 dylib 可能布局不兼容。要求 `abi` 版本 + 重新编译。
2. **allocator 不能跨边界**：谁分配谁释放，或严格走 `free` 函数。
3. **panic 必须 catch**：入口全部 `catch_unwind`。
4. **不要 `dlclose`**：Rust TLS、线程、静态变量会让卸载不安全。
5. **符号冲突**：`RTLD_LOCAL`，只导出 5 个符号，其他符号隐藏。
6. **不要 `println!` 到 stdout**：用 `host.log`。
7. **插件不能 `process::exit`**：会杀 daemon。
8. **异步运行时**：dylib 调用是同步的；host 用 `spawn_blocking`。插件可以自建 tokio runtime，但 `destroy` 必须关闭，线程不能泄漏。
9. **重入死锁**：dispatch 期间禁止回调同一实例。
10. **超时不能硬杀线程**：需要硬超时就用 hosted 模式。
11. **全局状态**：每个 dylib 有自己的 `std` 副本、TLS、全局分配器；不要假设与宿主共享。
12. **静态初始化**：避免在 dylib 的静态构造/初始化里 panic 或启动线程。

---

## 14. 路线图

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| P0 ✅ | `eix-plugin-abi` + `eix-plugin-sdk` 骨架、示例 dylib 可编译 | `cargo check --workspace --all-targets` |
| P1 ✅ | `DylibBackend` 加载器、`describe`、`tool.call`、`command.run` | 插件工具可在真实 turn 中调用 |
| P2 🚧 | hooks、events、`rpc.call`、`config.changed`、同步 host API（fs/session/blob）已落地；UI 面板与 async host 方法待做 | 与现有 JSON-RPC 后端功能对齐 |
| P3 🚧 | 进程插件已支持 UI 节点树、picker、`ui.render/action/pick`；dylib 的 UI 桥接待做 | 高自定义场景可用 |
| P4 ✅ | `pack/verify/install/update`、签名、`plugins.lock`、`require_signature`、`target`/`artifact`、Linux opt-in sandbox；registry 待做 | 第三方插件可安全分发 |
| P5 | 可选 WASM backend | 不可信插件沙箱 |

---

## 15. 代码映射

| 位置 | 现状 |
| --- | --- |
| `crates/eix-plugin-abi/src/lib.rs` | `EixStr` / `EixBuf` / `HostApiV1` / 5 个插件函数类型 / 状态码 |
| `crates/eix-plugin-sdk/src/lib.rs` | `Plugin` trait、`Descriptor` builder、`Ctx`、`RpcCall`、`eix_plugin!` 宏、FFI 包装；路由 `describe` / `tool.call` / `command.run` / `hook.run` / `rpc.call` / `event.notify` / `config.changed` / `lifecycle.*` |
| `crates/eixd/src/plugin/backend.rs` | `ExtensionBackend` trait 与 `BackendFuture` |
| `crates/eixd/src/plugin/process.rs` | `ProcessBackend`：现有 JSON-RPC 子进程的适配器 |
| `crates/eixd/src/plugin/dylib.rs` | `DylibBackend`：加载、ABI 校验、方法翻译、超时、dylib host API |
| `crates/eixd/src/plugin/host_api.rs` | 共享的 `WorkspaceFs` 与 blob 实现，供进程与 dylib 后端复用 |
| `crates/eixd/src/plugin/mod.rs` | `PluginManager` 按 `spec.library` 选择 backend，注册表与 hook/tool/command/RPC/event 路由共用；连续失败计数与 `degraded` |
| `crates/eix-core/src/plugin.rs` | `components.dylib`、`abi`、`library`、`PLUGIN_DYLIB_ABI_VERSION`、settings schema 校验与 UI 表面类型 |
| `examples/plugins/hello-dylib/` | 最小 cdylib 示例（`echo` 工具 + `sync` 命令）与 `eix-plugin.toml` |
| `docs/plugins.md` | 已落地的 v2 进程外插件系统 |

P1 已打通：`PluginManager::start_backend` 依据 `spec.library` 选择 `DylibBackend`，`describe` 注册工具/命令，`tools/call` / `commands/run` / `hooks/run` / `rpc.call` 经 trait 分发，`shutdown` 销毁实例但不 `dlclose`。P2 补上了同步 host API、`config.changed` 热更新与 `degraded` 计数。
