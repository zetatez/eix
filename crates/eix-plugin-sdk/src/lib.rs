//! Safe Rust SDK for eix dylib plugins.
//!
//! Plugin authors implement [`Plugin`] and invoke [`eix_plugin!`]. The macro
//! generates the five C symbols from [`eix_plugin_abi`], catches panics, and
//! converts JSON at the boundary.
//!
//! ```ignore
//! use eix_plugin_sdk::prelude::*;
//!
//! #[derive(Default)]
//! struct Hello;
//!
//! impl Plugin for Hello {
//!     fn describe(&self) -> Descriptor {
//!         Descriptor::new("hello")
//!             .version("0.1.0")
//!             .tool(Tool::new("echo", json!({"type": "object"})))
//!     }
//!
//!     fn call_tool(&mut self, ctx: &mut Ctx<'_>, name: &str, args: Value) -> Result<Value> {
//!         match name {
//!             "echo" => Ok(json!({"content": [{"type": "text", "text": args["text"]}]})),
//!             _ => Err(Error::UnknownMethod(name.into())),
//!         }
//!     }
//! }
//!
//! eix_plugin!(Hello);
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

pub use eix_plugin_abi as abi;

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Common imports for plugin crates.
pub mod prelude {
    pub use crate::{
        eix_plugin, Command, Ctx, Descriptor, Error, Hook, Plugin, Provider, Result, RpcMethod,
        Tool, Ui,
    };
    pub use serde_json::{json, Value};
}

/// Errors a plugin may return from a dispatch method.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("invalid params: {0}")]
    Invalid(String),
    #[error("denied: {0}")]
    Denied(String),
    #[error("host unavailable")]
    HostUnavailable,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("plugin panicked")]
    Panic,
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn code(&self) -> i32 {
        match self {
            Self::UnknownMethod(_) => abi::EIX_ERR_UNKNOWN_METHOD,
            Self::Invalid(_) | Self::Json(_) => abi::EIX_ERR_INVALID,
            Self::Denied(_) => abi::EIX_ERR_DENIED,
            Self::HostUnavailable => abi::EIX_ERR_INTERNAL,
            Self::Panic => abi::EIX_ERR_PANIC,
            Self::Other(_) => abi::EIX_ERR_INTERNAL,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "code": abi::status_name(self.code()),
            "message": self.to_string(),
        })
    }

    fn from_host(code: i32, value: Value) -> Self {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| abi::status_name(code))
            .to_string();
        match code {
            abi::EIX_ERR_DENIED => Self::Denied(message),
            abi::EIX_ERR_UNKNOWN_METHOD => Self::UnknownMethod(message),
            _ => Self::Other(message),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Everything a plugin can contribute.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Contributions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<Ui>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keymaps: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rpc: Vec<RpcMethod>,
}

/// The `describe` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default = "default_abi")]
    pub abi: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_threading")]
    pub threading: String,
    #[serde(default)]
    pub contributions: Contributions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_schema: Option<Value>,
    /// Forward-compatible escape hatch for contributions the SDK does not
    /// model yet. Prefer adding a typed builder when a contribution stabilizes.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_abi() -> u32 {
    abi::ABI_VERSION
}

fn default_threading() -> String {
    "serialized".to_string()
}

impl Descriptor {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: String::new(),
            abi: abi::ABI_VERSION,
            capabilities: Vec::new(),
            threading: default_threading(),
            contributions: Contributions::default(),
            settings_schema: None,
            extra: Map::new(),
        }
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn capability(mut self, capability: impl Into<String>) -> Self {
        self.capabilities.push(capability.into());
        self
    }

    pub fn capabilities<I, S>(mut self, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.capabilities
            .extend(capabilities.into_iter().map(Into::into));
        self
    }

    /// Reserved for a future concurrent dispatch model. ABI v1 serializes
    /// calls per instance, so plugins must not rely on this field.
    pub fn threading(mut self, threading: impl Into<String>) -> Self {
        self.threading = threading.into();
        self
    }

    pub fn tool(mut self, tool: Tool) -> Self {
        self.contributions.tools.push(tool);
        self
    }

    pub fn command(mut self, command: Command) -> Self {
        self.contributions.commands.push(command);
        self
    }

    pub fn hook(mut self, hook: Hook) -> Self {
        self.contributions.hooks.push(hook);
        self
    }

    pub fn event(mut self, pattern: impl Into<String>) -> Self {
        self.contributions.events.push(pattern.into());
        self
    }

    pub fn ui(mut self, ui: Ui) -> Self {
        self.contributions.ui.push(ui);
        self
    }

    pub fn ui_panel(mut self, id: impl Into<String>, title: impl Into<String>) -> Self {
        self.contributions.ui.push(Ui::panel(id, title));
        self
    }

    pub fn provider(mut self, provider: Provider) -> Self {
        self.contributions.providers.push(provider);
        self
    }

    pub fn rpc(mut self, name: impl Into<String>) -> Self {
        self.contributions.rpc.push(RpcMethod {
            name: name.into(),
            description: String::new(),
        });
        self
    }

    pub fn settings_schema(mut self, schema: Value) -> Self {
        self.settings_schema = Some(schema);
        self
    }

    pub fn extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub read_only: bool,
}

impl Tool {
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            input_schema,
            read_only: false,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

impl Command {
    pub fn prompt(name: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            args: String::new(),
            prompt: Some(prompt.into()),
            action: None,
        }
    }

    pub fn action(name: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            args: String::new(),
            prompt: None,
            action: Some(action.into()),
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn args(mut self, args: impl Into<String>) -> Self {
        self.args = args.into();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hook {
    pub point: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default = "default_on_error")]
    pub on_error: String,
}

fn default_on_error() -> String {
    "allow".to_string()
}

impl Hook {
    pub fn new(point: impl Into<String>) -> Self {
        Self {
            point: point.into(),
            priority: 0,
            timeout_ms: 0,
            on_error: default_on_error(),
        }
    }

    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn on_error(mut self, on_error: impl Into<String>) -> Self {
        self.on_error = on_error.into();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ui {
    pub surface: String,
    pub id: String,
    #[serde(default)]
    pub title: String,
}

impl Ui {
    pub fn panel(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            surface: "panel".to_string(),
            id: id.into(),
            title: title.into(),
        }
    }

    pub fn status(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            surface: "status".to_string(),
            id: id.into(),
            title: title.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub streaming: bool,
}

impl Provider {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            models: Vec::new(),
            streaming: false,
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.models.push(model.into());
        self
    }

    pub fn streaming(mut self) -> Self {
        self.streaming = true;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcMethod {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Host services available to a plugin during a dispatch call.
pub struct Ctx<'a> {
    api: &'a abi::HostApiV1,
    plugin: &'a str,
}

impl<'a> Ctx<'a> {
    pub fn plugin(&self) -> &str {
        self.plugin
    }

    /// Call a host method. The host owns the returned buffer; this wrapper
    /// releases it with `HostApiV1::free`.
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let dispatch = self.api.dispatch.ok_or(Error::HostUnavailable)?;
        let params = serde_json::to_vec(&params)?;
        let mut out = abi::EixBuf::EMPTY;
        // SAFETY: `self.api` was validated by `create`; the method and params
        // buffers outlive the call.
        let code = unsafe {
            dispatch(
                self.api.ctx,
                abi::EixStr::from_slice(method.as_bytes()),
                abi::EixStr::from_slice(&params),
                &mut out,
            )
        };

        let value = if out.is_null() {
            Value::Null
        } else {
            // SAFETY: the host returned a buffer valid for `out.len` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
            let value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
            if let Some(free) = self.api.free {
                // SAFETY: the buffer belongs to the host and is released once.
                unsafe { free(self.api.ctx, out) };
            }
            value
        };

        if code == abi::EIX_OK {
            Ok(value)
        } else {
            Err(Error::from_host(code, value))
        }
    }

    pub fn log(&self, level: &str, message: &str) -> Result<()> {
        self.call("host.log", json!({"level": level, "message": message}))?;
        Ok(())
    }

    pub fn notify(&self, level: &str, message: &str) -> Result<()> {
        self.call("host.notify", json!({"level": level, "message": message}))?;
        Ok(())
    }

    pub fn storage_get(&self, key: &str) -> Result<Value> {
        self.call("host.storage.get", json!({"key": key}))
    }

    pub fn storage_set(&self, key: &str, value: Value) -> Result<()> {
        self.call("host.storage.set", json!({"key": key, "value": value}))?;
        Ok(())
    }

    pub fn secrets_get(&self, name: &str) -> Result<Option<String>> {
        let value = self.call("host.secrets.get", json!({"name": name}))?;
        Ok(value
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    pub fn rpc_call(&self, plugin: &str, method: &str, arguments: Value) -> Result<Value> {
        self.rpc_call_with_depth(plugin, method, arguments, 0)
    }

    /// Forward an inbound RPC call. Pass the [`RpcCall::depth`] the host gave
    /// you so the `MAX_RPC_DEPTH` guard keeps working across hops.
    pub fn rpc_call_with_depth(
        &self,
        plugin: &str,
        method: &str,
        arguments: Value,
        depth: u32,
    ) -> Result<Value> {
        self.call(
            "host.rpc.call",
            json!({
                "plugin": plugin,
                "method": method,
                "arguments": arguments,
                "depth": depth,
            }),
        )
    }
}

/// One inbound cross-plugin RPC call, as delivered by the host.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcCall {
    /// Method name exported through `Descriptor::rpc`.
    pub method: String,
    /// Caller-supplied arguments. Opaque to the host.
    pub arguments: Value,
    /// Plugin that started the chain.
    pub caller: String,
    /// Host-side hop count. Forward it with [`Ctx::rpc_call_with_depth`].
    pub depth: u32,
}

/// The trait a dylib plugin implements.
pub trait Plugin: Default + 'static {
    /// Describe every contribution. Called once after `initialize`.
    fn describe(&self) -> Descriptor;

    /// Optional setup with the manifest/config passed by the host.
    fn initialize(&mut self, _ctx: &mut Ctx<'_>, _config: &Value) -> Result<()> {
        Ok(())
    }

    fn call_tool(&mut self, _ctx: &mut Ctx<'_>, name: &str, _arguments: Value) -> Result<Value> {
        Err(Error::UnknownMethod(format!("tool {name}")))
    }

    fn run_command(
        &mut self,
        _ctx: &mut Ctx<'_>,
        command: &str,
        _arguments: &str,
    ) -> Result<Value> {
        Err(Error::UnknownMethod(format!("command {command}")))
    }

    fn run_hook(&mut self, _ctx: &mut Ctx<'_>, point: &str, _payload: Value) -> Result<Value> {
        Err(Error::UnknownMethod(format!("hook {point}")))
    }

    /// Handle a method exported through [`Descriptor::rpc`]. The default
    /// rejects every method, so a plugin only opts in by overriding this.
    fn rpc(&mut self, _ctx: &mut Ctx<'_>, call: RpcCall) -> Result<Value> {
        Err(Error::UnknownMethod(format!("rpc {}", call.method)))
    }

    fn render_ui(&mut self, _ctx: &mut Ctx<'_>, view: &str, _revision: u64) -> Result<Value> {
        Err(Error::UnknownMethod(format!("ui view {view}")))
    }

    fn ui_action(
        &mut self,
        _ctx: &mut Ctx<'_>,
        _view: &str,
        _action: &str,
        _payload: Value,
    ) -> Result<Value> {
        Ok(json!({}))
    }

    fn event(&mut self, _ctx: &mut Ctx<'_>, _method: &str, _params: Value) -> Result<()> {
        Ok(())
    }

    fn config_changed(&mut self, _ctx: &mut Ctx<'_>, _config: &Value) -> Result<()> {
        Ok(())
    }

    fn start(&mut self, _ctx: &mut Ctx<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx<'_>) -> Result<()> {
        Ok(())
    }

    /// Escape hatch for methods the SDK does not model yet. Implementing this
    /// keeps a plugin compatible with newer host methods without an ABI bump.
    fn dispatch(&mut self, _ctx: &mut Ctx<'_>, method: &str, _params: Value) -> Result<Value> {
        Err(Error::UnknownMethod(method.to_string()))
    }
}

struct Instance<P: Plugin> {
    plugin: P,
    host: abi::HostApiV1,
    name: String,
    #[allow(dead_code)]
    config: Value,
}

impl<P: Plugin> Instance<P> {
    fn dispatch(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut ctx = Ctx {
            api: &self.host,
            plugin: &self.name,
        };
        match method {
            "describe" => Ok(serde_json::to_value(self.plugin.describe())?),
            "tool.call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("tool.call needs `name`".into()))?;
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                self.plugin.call_tool(&mut ctx, name, arguments)
            }
            "command.run" => {
                let command = params
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("command.run needs `command`".into()))?;
                let arguments = params
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.plugin.run_command(&mut ctx, command, arguments)
            }
            "hook.run" => {
                let point = params
                    .get("point")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| Error::Invalid("hook.run needs `point`".into()))?;
                self.plugin.run_hook(&mut ctx, &point, params)
            }
            "rpc.call" => {
                let method = params
                    .get("method")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("rpc.call needs `method`".into()))?
                    .to_string();
                let call = RpcCall {
                    method,
                    arguments: params.get("arguments").cloned().unwrap_or(Value::Null),
                    caller: params
                        .get("caller")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    depth: params.get("depth").and_then(Value::as_u64).unwrap_or(0) as u32,
                };
                self.plugin.rpc(&mut ctx, call)
            }
            "ui.render" => {
                let view = params
                    .get("view_id")
                    .or_else(|| params.get("view"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("ui.render needs `view_id`".into()))?;
                let revision = params.get("revision").and_then(Value::as_u64).unwrap_or(0);
                self.plugin.render_ui(&mut ctx, view, revision)
            }
            "ui.action" => {
                let view = params
                    .get("view_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("ui.action needs `view_id`".into()))?;
                let action = params
                    .get("action_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Invalid("ui.action needs `action_id`".into()))?;
                let payload = params.get("payload").cloned().unwrap_or(Value::Null);
                self.plugin.ui_action(&mut ctx, view, action, payload)
            }
            "event.notify" => {
                let name = params
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("event")
                    .to_string();
                self.plugin.event(&mut ctx, &name, params)?;
                Ok(json!({}))
            }
            "config.changed" => {
                self.config = params.clone();
                self.plugin.config_changed(&mut ctx, &params)?;
                Ok(json!({}))
            }
            "lifecycle.start" => {
                self.plugin.start(&mut ctx)?;
                Ok(json!({}))
            }
            "lifecycle.stop" => {
                self.plugin.stop(&mut ctx)?;
                Ok(json!({}))
            }
            "health" => Ok(json!({"ok": true})),
            other => self.plugin.dispatch(&mut ctx, other, params),
        }
    }
}

#[doc(hidden)]
pub mod ffi {
    use super::*;

    pub unsafe fn create<P: Plugin>(
        host: *const abi::HostApiV1,
        config: abi::EixStr,
        out: *mut *mut c_void,
    ) -> i32 {
        guard(|| {
            if host.is_null() || out.is_null() {
                return Err(abi::EIX_ERR_INVALID);
            }
            // SAFETY: `host` is non-null and points to a table owned by the
            // host for the duration of the call.
            let host = unsafe { *host };
            if !host.is_compatible() {
                return Err(abi::EIX_ERR_VERSION);
            }
            let config: Value = if config.as_bytes().is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(config.as_bytes()).map_err(|_| abi::EIX_ERR_INVALID)?
            };

            let mut plugin = P::default();
            {
                let mut ctx = Ctx {
                    api: &host,
                    plugin: "",
                };
                plugin
                    .initialize(&mut ctx, &config)
                    .map_err(|err| err.code())?;
            }
            let name = plugin.describe().name;
            let instance = Box::new(Instance {
                plugin,
                host,
                name,
                config,
            });
            // SAFETY: `out` is non-null and owned by the host for this call.
            unsafe { *out = Box::into_raw(instance).cast() };
            Ok(())
        })
    }

    pub unsafe fn dispatch<P: Plugin>(
        instance: *mut c_void,
        method: abi::EixStr,
        params: abi::EixStr,
        out: *mut abi::EixBuf,
    ) -> i32 {
        guard(|| {
            if instance.is_null() {
                return Err(abi::EIX_ERR_INVALID);
            }
            // SAFETY: the host passes back the pointer returned by `create`.
            let Some(instance) = (unsafe { (instance as *mut Instance<P>).as_mut() }) else {
                return Err(abi::EIX_ERR_INVALID);
            };
            let method = method.as_str().map_err(|_| abi::EIX_ERR_INVALID)?;
            let params: Value = if params.as_bytes().is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(params.as_bytes()).map_err(|_| abi::EIX_ERR_INVALID)?
            };

            match instance.dispatch(method, params) {
                Ok(value) => {
                    if !out.is_null() {
                        let bytes = serde_json::to_vec(&value).map_err(|_| abi::EIX_ERR_INVALID)?;
                        // SAFETY: `out` is non-null and owned by the host.
                        unsafe { *out = abi::EixBuf::from_vec(bytes) };
                    }
                    Ok(())
                }
                Err(err) => {
                    if !out.is_null() {
                        let bytes = serde_json::to_vec(&err.to_value()).unwrap_or_default();
                        // SAFETY: `out` is non-null and owned by the host.
                        unsafe { *out = abi::EixBuf::from_vec(bytes) };
                    }
                    Err(err.code())
                }
            }
        })
    }

    pub unsafe fn free<P: Plugin>(_instance: *mut c_void, buf: abi::EixBuf) {
        if buf.is_null() {
            return;
        }
        // SAFETY: the host calls this exactly once with the buffer returned by
        // `dispatch`.
        drop(unsafe { buf.into_vec() });
    }

    pub unsafe fn destroy<P: Plugin>(instance: *mut c_void) {
        if instance.is_null() {
            return;
        }
        // SAFETY: the host calls this exactly once with the pointer returned by
        // `create` and never uses it afterwards.
        let mut instance = unsafe { Box::from_raw(instance as *mut Instance<P>) };
        {
            let Instance {
                plugin, host, name, ..
            } = &mut *instance;
            let mut ctx = Ctx {
                api: host,
                plugin: name,
            };
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let _ = plugin.stop(&mut ctx);
            }));
        }
        drop(instance);
    }

    fn guard<F>(f: F) -> i32
    where
        F: FnOnce() -> std::result::Result<(), i32>,
    {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(Ok(())) => abi::EIX_OK,
            Ok(Err(code)) => code,
            Err(_) => abi::EIX_ERR_PANIC,
        }
    }
}

/// Generate the five C symbols a dylib plugin must export.
///
/// The type must implement [`Plugin`] and [`Default`].
#[macro_export]
macro_rules! eix_plugin {
    ($ty:ty) => {
        #[no_mangle]
        pub extern "C" fn eix_plugin_abi_version() -> u32 {
            $crate::abi::ABI_VERSION
        }

        #[no_mangle]
        pub unsafe extern "C" fn eix_plugin_create(
            host: *const $crate::abi::HostApiV1,
            config: $crate::abi::EixStr,
            out: *mut *mut ::core::ffi::c_void,
        ) -> i32 {
            // SAFETY: the host upholds the ABI contract for `host` and `out`.
            unsafe { $crate::ffi::create::<$ty>(host, config, out) }
        }

        #[no_mangle]
        pub unsafe extern "C" fn eix_plugin_dispatch(
            instance: *mut ::core::ffi::c_void,
            method: $crate::abi::EixStr,
            params: $crate::abi::EixStr,
            out: *mut $crate::abi::EixBuf,
        ) -> i32 {
            // SAFETY: `instance` is the pointer returned by `eix_plugin_create`.
            unsafe { $crate::ffi::dispatch::<$ty>(instance, method, params, out) }
        }

        #[no_mangle]
        pub unsafe extern "C" fn eix_plugin_free(
            instance: *mut ::core::ffi::c_void,
            buf: $crate::abi::EixBuf,
        ) {
            // SAFETY: `buf` was returned by `eix_plugin_dispatch` for this instance.
            unsafe { $crate::ffi::free::<$ty>(instance, buf) }
        }

        #[no_mangle]
        pub unsafe extern "C" fn eix_plugin_destroy(instance: *mut ::core::ffi::c_void) {
            // SAFETY: `instance` is the pointer returned by `eix_plugin_create`.
            unsafe { $crate::ffi::destroy::<$ty>(instance) }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    #[derive(Default)]
    struct Echo;

    impl Plugin for Echo {
        fn describe(&self) -> Descriptor {
            Descriptor::new("echo").tool(Tool::new("echo", json!({"type": "object"})))
        }

        fn call_tool(&mut self, ctx: &mut Ctx<'_>, name: &str, arguments: Value) -> Result<Value> {
            match name {
                "echo" => {
                    ctx.log("info", "echo")?;
                    Ok(json!({"content": [{"type": "text", "text": arguments["text"]}]}))
                }
                other => Err(Error::UnknownMethod(other.to_string())),
            }
        }

        fn rpc(&mut self, _ctx: &mut Ctx<'_>, call: RpcCall) -> Result<Value> {
            Ok(json!({
                "method": call.method,
                "arguments": call.arguments,
                "caller": call.caller,
                "depth": call.depth,
            }))
        }
    }

    #[derive(Default)]
    struct Panicky;

    impl Plugin for Panicky {
        fn describe(&self) -> Descriptor {
            panic!("boom")
        }
    }

    unsafe extern "C" fn test_host(
        _ctx: *mut c_void,
        method: abi::EixStr,
        _params: abi::EixStr,
        out: *mut abi::EixBuf,
    ) -> i32 {
        if method.as_str().unwrap_or_default() != "host.log" {
            return abi::EIX_ERR_UNKNOWN_METHOD;
        }
        if !out.is_null() {
            // SAFETY: `out` is non-null and owned by the plugin.
            unsafe { *out = abi::EixBuf::from_vec(b"null".to_vec()) };
        }
        abi::EIX_OK
    }

    fn host() -> abi::HostApiV1 {
        abi::HostApiV1 {
            abi_version: abi::ABI_VERSION,
            size: std::mem::size_of::<abi::HostApiV1>(),
            ctx: std::ptr::null_mut(),
            dispatch: Some(test_host),
            free: None,
        }
    }

    fn dispatch<P: Plugin>(instance: *mut c_void, method: &str, params: Value) -> (i32, Value) {
        let params = serde_json::to_vec(&params).unwrap();
        let mut out = abi::EixBuf::EMPTY;
        // SAFETY: `instance` comes from `ffi::create` for this plugin type.
        let code = unsafe {
            ffi::dispatch::<P>(
                instance,
                abi::EixStr::from_slice(method.as_bytes()),
                abi::EixStr::from_slice(&params),
                &mut out,
            )
        };
        let value = if out.is_null() {
            Value::Null
        } else {
            // SAFETY: the plugin returned a buffer valid for `out.len` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
            let value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
            // SAFETY: the buffer is plugin-owned and released exactly once.
            unsafe { ffi::free::<P>(instance, out) };
            value
        };
        (code, value)
    }

    #[test]
    fn descriptor_builder_serializes_contributions() {
        let descriptor = Descriptor::new("acme")
            .version("1.0.0")
            .capability("tools")
            .tool(Tool::new("echo", json!({"type": "object"})))
            .command(Command::action("sync", "sync"))
            .hook(Hook::new("tool.before").priority(10).on_error("deny"))
            .ui_panel("issues", "Issues")
            .rpc("issues.search");

        let value = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(value["name"], "acme");
        assert_eq!(value["abi"], abi::ABI_VERSION);
        assert_eq!(value["threading"], "serialized");
        assert_eq!(value["contributions"]["tools"][0]["name"], "echo");
        assert_eq!(value["contributions"]["commands"][0]["action"], "sync");
        assert_eq!(value["contributions"]["hooks"][0]["on_error"], "deny");
        assert_eq!(value["contributions"]["ui"][0]["surface"], "panel");
        assert_eq!(value["contributions"]["rpc"][0]["name"], "issues.search");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(
            Error::UnknownMethod("x".into()).code(),
            abi::EIX_ERR_UNKNOWN_METHOD
        );
        assert_eq!(Error::Denied("x".into()).code(), abi::EIX_ERR_DENIED);
        assert_eq!(Error::Panic.code(), abi::EIX_ERR_PANIC);
    }

    #[test]
    fn ffi_round_trips_describe_and_tool_call() {
        let host = host();
        let mut instance = std::ptr::null_mut();
        // SAFETY: the host table is valid for the duration of the call.
        let code = unsafe { ffi::create::<Echo>(&host, abi::EixStr::EMPTY, &mut instance) };
        assert_eq!(code, abi::EIX_OK);
        assert!(!instance.is_null());

        let (code, descriptor) = dispatch::<Echo>(instance, "describe", json!({}));
        assert_eq!(code, abi::EIX_OK);
        assert_eq!(descriptor["name"], "echo");

        let (code, result) = dispatch::<Echo>(
            instance,
            "tool.call",
            json!({"name": "echo", "arguments": {"text": "hi"}}),
        );
        assert_eq!(code, abi::EIX_OK);
        assert_eq!(result["content"][0]["text"], "hi");

        let (code, _) = dispatch::<Echo>(instance, "tool.missing", json!({}));
        assert_eq!(code, abi::EIX_ERR_UNKNOWN_METHOD);

        // SAFETY: the instance is destroyed exactly once.
        unsafe { ffi::destroy::<Echo>(instance) };
    }

    #[test]
    fn ffi_round_trips_rpc_calls() {
        let host = host();
        let mut instance = std::ptr::null_mut();
        // SAFETY: the host table is valid for the duration of the call.
        let code = unsafe { ffi::create::<Echo>(&host, abi::EixStr::EMPTY, &mut instance) };
        assert_eq!(code, abi::EIX_OK);

        let (code, result) = dispatch::<Echo>(
            instance,
            "rpc.call",
            json!({
                "method": "issues.search",
                "arguments": {"query": "bug"},
                "caller": "acme",
                "depth": 2,
            }),
        );
        assert_eq!(code, abi::EIX_OK);
        assert_eq!(result["method"], "issues.search");
        assert_eq!(result["arguments"]["query"], "bug");
        assert_eq!(result["caller"], "acme");
        assert_eq!(result["depth"], 2);

        // SAFETY: the instance is destroyed exactly once.
        unsafe { ffi::destroy::<Echo>(instance) };
    }

    #[test]
    fn ffi_turns_panics_into_status_codes() {
        let host = host();
        let mut instance = std::ptr::null_mut();
        // `create` calls `describe` to learn the plugin name.
        // SAFETY: the host table is valid for the duration of the call.
        let code = unsafe { ffi::create::<Panicky>(&host, abi::EixStr::EMPTY, &mut instance) };
        assert_eq!(code, abi::EIX_ERR_PANIC);
        assert!(instance.is_null());
    }

    #[test]
    fn ffi_rejects_a_foreign_host_version() {
        let mut host = host();
        host.abi_version = abi::ABI_VERSION + 1;
        let mut instance = std::ptr::null_mut();
        // SAFETY: the host table is valid for the duration of the call.
        let code = unsafe { ffi::create::<Echo>(&host, abi::EixStr::EMPTY, &mut instance) };
        assert_eq!(code, abi::EIX_ERR_VERSION);
        assert!(instance.is_null());
    }
}
