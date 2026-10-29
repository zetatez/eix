//! In-process native dylib backend (ABI v1).
//!
//! The plugin is a `cdylib` that exports five C symbols:
//! `eix_plugin_abi_version`, `create`, `dispatch`, `free`, `destroy`. Every
//! capability travels through `dispatch(method, JSON)`, so this loader only
//! needs to translate the v2 method names to ABI names.
//!
//! A dylib runs in the daemon's address space. It is trusted code: a panic is
//! caught at the boundary, but a segfault or `process::exit` takes the daemon
//! down. See `docs/plugin-dylib.md` for the trust model.
//!
//! The library is never unloaded. Rust TLS, statics and background threads make
//! `dlclose` unsafe, so hot reload must load a new copy under a new path.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use eix_core::plugin::{
    PluginCapability, PluginCommand, PluginHook, PluginInitializeResult, PluginRpc, PluginTool,
    PluginUi, PLUGIN_API_VERSION,
};
use eix_core::redact::redact;
use eix_core::Notification;
use eix_plugin_abi as abi;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::store::Store;

use super::backend::{BackendFuture, ExtensionBackend};
use super::host_api::{blob_get, blob_put, required_capability, WorkspaceFs};
use super::PluginSpec;

const MAX_LOG: usize = 4096;
const MAX_STORAGE_KEY: usize = 512;
const MAX_STORAGE_VALUE: usize = 1_000_000;

/// One running native plugin.
pub struct DylibBackend {
    name: String,
    inner: Arc<Mutex<Option<Instance>>>,
    timeout: Duration,
    closed: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl DylibBackend {
    /// Load and create one instance. The caller is responsible for calling
    /// [`ExtensionBackend::initialize`] before routing contributions to it.
    pub async fn load(
        library: &Path,
        spec: &PluginSpec,
        store: Arc<Store>,
        bus: broadcast::Sender<Notification>,
        logs: Arc<super::logs::PluginLogs>,
    ) -> Result<Arc<Self>> {
        let library = canonical_library(library)?;
        let host = Arc::new(DylibHost {
            name: spec.name.clone(),
            capabilities: spec.capabilities.iter().copied().collect(),
            workspace: spec.cwd.clone(),
            data_dir: spec.data_dir.clone(),
            store,
            bus,
            network_allow: spec.network_allow.clone(),
            secrets: spec.secrets.clone(),
            env: spec.env.clone(),
            logs,
        });
        let config = json!({
            "data_dir": spec.data_dir,
            "settings": spec.settings,
        });
        let name = spec.name.clone();
        let host_for_task = Arc::clone(&host);
        let load_library = library.clone();
        let instance = tokio::task::spawn_blocking(move || {
            load_instance(&load_library, host_for_task, config, name)
        })
        .await
        .context("plugin loader task panicked")??;

        Ok(Arc::new(Self {
            name: spec.name.clone(),
            inner: Arc::new(Mutex::new(Some(instance))),
            timeout: spec.timeout,
            closed: AtomicBool::new(false),
            reason: Mutex::new(None),
        }))
    }

    fn mark_closed(&self, reason: &str) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.reason.lock() {
            if guard.is_none() {
                *guard = Some(redact(reason));
            }
        }
    }
}

impl ExtensionBackend for DylibBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn initialize<'a>(
        &'a self,
        _spec: &'a PluginSpec,
    ) -> BackendFuture<'a, Result<PluginInitializeResult>> {
        Box::pin(async move {
            let value = self.request("describe", json!({})).await?;
            let descriptor: Descriptor =
                serde_json::from_value(value).context("plugin returned an invalid descriptor")?;
            if descriptor.abi != abi::ABI_VERSION {
                bail!(
                    "plugin {} describes ABI {}, host expects {}",
                    self.name,
                    descriptor.abi,
                    abi::ABI_VERSION
                );
            }
            if descriptor.name != self.name {
                bail!(
                    "plugin {} describes itself as {:?}, which does not match its manifest",
                    self.name,
                    descriptor.name
                );
            }
            let result = descriptor_to_init(descriptor);
            // Best-effort: a plugin that does not implement the lifecycle hook
            // still works.
            let _ = self.request("lifecycle.start", json!({})).await;
            Ok(result)
        })
    }

    fn request<'a>(&'a self, method: &'a str, params: Value) -> BackendFuture<'a, Result<Value>> {
        Box::pin(async move {
            if self.is_closed() {
                bail!("plugin {} is not running", self.name);
            }
            let abi_method = translate_method(method).to_string();
            let abi_params = translate_params(method, params);
            let inner = Arc::clone(&self.inner);
            let name = self.name.clone();
            let task = tokio::task::spawn_blocking(move || {
                let mut guard = inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let instance = guard
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("plugin {name} is not running"))?;
                dispatch_sync(instance, &abi_method, &abi_params)
            });

            match tokio::time::timeout(self.timeout, task).await {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => {
                    let message = format!("plugin {} call {method} aborted: {err}", self.name);
                    self.mark_closed(&message);
                    Err(anyhow::anyhow!(message))
                }
                Err(_) => {
                    let message = format!(
                        "plugin {} call {method} timed out after {}s",
                        self.name,
                        self.timeout.as_secs()
                    );
                    self.mark_closed(&message);
                    Err(anyhow::anyhow!(message))
                }
            }
        })
    }

    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BackendFuture<'a, Result<()>> {
        Box::pin(async move {
            if method == "config.changed" {
                // The SDK has a dedicated arm so `Plugin::config_changed` runs
                // and the instance can update its cached settings.
                self.request(method, params).await?;
            } else {
                self.request("event.notify", json!({"method": method, "params": params}))
                    .await?;
            }
            Ok(())
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn exit_reason<'a>(&'a self) -> BackendFuture<'a, Option<String>> {
        Box::pin(async move {
            self.reason
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
    }

    fn shutdown<'a>(&'a self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
            let inner = Arc::clone(&self.inner);
            let _ = tokio::task::spawn_blocking(move || {
                // A timed-out call may still own the instance. Do not block
                // shutdown forever: when that task finishes it drops the last
                // Arc, which runs `Instance::drop` and calls destroy.
                let Ok(mut guard) = inner.try_lock() else {
                    return;
                };
                if let Some(mut instance) = guard.take() {
                    let _ = dispatch_sync(&mut instance, "lifecycle.stop", &json!({}));
                    // `Instance::drop` calls `eix_plugin_destroy`.
                    drop(instance);
                }
            })
            .await;
        })
    }
}

/// The plugin-owned instance. `host` keeps the callback context alive for as
/// long as the plugin can call it.
struct Instance {
    handle: *mut c_void,
    dispatch: abi::PluginDispatchFn,
    free: abi::PluginFreeFn,
    destroy: abi::PluginDestroyFn,
    /// Lifetime anchor for the callback context passed to the plugin. Never
    /// read directly, but must outlive `handle`.
    _host: Arc<DylibHost>,
}

// SAFETY: the raw handle is only touched from the blocking task that owns the
// mutex, and the host context is `Send + Sync`.
unsafe impl Send for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: `handle` came from `eix_plugin_create` and is destroyed
            // exactly once here.
            unsafe { (self.destroy)(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

fn canonical_library(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("dylib path is empty");
    }
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve dylib {}", path.display()))?;
    let meta = std::fs::metadata(&path)
        .with_context(|| format!("cannot stat dylib {}", path.display()))?;
    if !meta.is_file() {
        bail!("dylib {} is not a regular file", path.display());
    }
    Ok(path)
}

fn load_instance(
    library: &Path,
    host: Arc<DylibHost>,
    config: Value,
    name: String,
) -> Result<Instance> {
    // SAFETY: loading arbitrary native code is exactly what this backend is
    // for; the caller has already selected a trusted plugin.
    let lib = unsafe { libloading::Library::new(library) }
        .with_context(|| format!("cannot load dylib {}", library.display()))?;

    // SAFETY: the five symbols are part of ABI v1 and their signatures are
    // checked by the ABI version probe below.
    let abi_version: abi::PluginAbiVersionFn =
        unsafe { symbol(&lib, b"eix_plugin_abi_version\0") }?;
    let create: abi::PluginCreateFn = unsafe { symbol(&lib, b"eix_plugin_create\0") }?;
    let dispatch: abi::PluginDispatchFn = unsafe { symbol(&lib, b"eix_plugin_dispatch\0") }?;
    let free: abi::PluginFreeFn = unsafe { symbol(&lib, b"eix_plugin_free\0") }?;
    let destroy: abi::PluginDestroyFn = unsafe { symbol(&lib, b"eix_plugin_destroy\0") }?;

    // SAFETY: `abi_version` points at the plugin's exported function.
    let version = unsafe { abi_version() };
    if version != abi::ABI_VERSION {
        bail!(
            "plugin {name} exports ABI {version}, host expects {}",
            abi::ABI_VERSION
        );
    }

    // Never `dlclose`. Rust TLS and statics are not safe to unload, and a
    // reloaded plugin may have handed out pointers to this mapping. Leaking the
    // handle keeps every symbol valid for the daemon's lifetime.
    let _ = Box::leak(Box::new(lib));

    let api = abi::HostApiV1 {
        abi_version: abi::ABI_VERSION,
        size: std::mem::size_of::<abi::HostApiV1>(),
        ctx: Arc::as_ptr(&host).cast_mut().cast(),
        dispatch: Some(host_dispatch),
        free: Some(host_free),
    };

    let config = serde_json::to_vec(&config)?;
    let mut handle = std::ptr::null_mut();
    // SAFETY: `api`, `config` and `handle` outlive the call.
    let code = unsafe { create(&api, abi::EixStr::from_slice(&config), &mut handle) };
    if code != abi::EIX_OK || handle.is_null() {
        bail!("plugin {name} create failed: {}", abi::status_name(code));
    }

    Ok(Instance {
        handle,
        dispatch,
        free,
        destroy,
        _host: host,
    })
}

unsafe fn symbol<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Result<T> {
    // SAFETY: the caller guarantees `T` matches the exported symbol.
    let symbol: libloading::Symbol<'_, T> = unsafe { lib.get(name) }
        .with_context(|| format!("plugin is missing symbol {}", String::from_utf8_lossy(name)))?;
    Ok(*symbol)
}

/// Translate a v2 protocol method to an ABI method. Unknown methods pass
/// through so `Plugin::dispatch` remains an escape hatch.
fn translate_method(method: &str) -> &str {
    match method {
        "tools/call" => "tool.call",
        "commands/run" => "command.run",
        "hooks/run" => "hook.run",
        // Same name in both protocols; listed so the ABI surface is explicit.
        "rpc.call" => "rpc.call",
        other => other,
    }
}

fn translate_params(method: &str, params: Value) -> Value {
    let mut params = params;
    if method == "tools/call" {
        if let Some(object) = params.as_object_mut() {
            if let Some(tool) = object.remove("tool") {
                object.insert("name".to_string(), tool);
            }
        }
    }
    params
}

fn dispatch_sync(instance: &mut Instance, method: &str, params: &Value) -> Result<Value> {
    let params = serde_json::to_vec(params)?;
    let mut out = abi::EixBuf::EMPTY;
    // SAFETY: `instance` is exclusive for this call and the input slices live
    // until the call returns.
    let code = unsafe {
        (instance.dispatch)(
            instance.handle,
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
        unsafe { (instance.free)(instance.handle, out) };
        value
    };

    if code == abi::EIX_OK {
        Ok(value)
    } else {
        Err(anyhow::anyhow!(
            "plugin method {method} failed: {} ({})",
            abi::status_name(code),
            value
        ))
    }
}

fn descriptor_to_init(descriptor: Descriptor) -> PluginInitializeResult {
    PluginInitializeResult {
        name: descriptor.name,
        version: descriptor.version,
        api_version: PLUGIN_API_VERSION,
        features: Vec::new(),
        tools: descriptor.contributions.tools,
        commands: descriptor.contributions.commands,
        events: descriptor.contributions.events,
        hooks: descriptor.contributions.hooks,
        ui: descriptor.contributions.ui,
        rpc: descriptor.contributions.rpc,
    }
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default = "default_abi")]
    abi: u32,
    #[serde(default)]
    contributions: Contributions,
}

fn default_abi() -> u32 {
    abi::ABI_VERSION
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Contributions {
    tools: Vec<PluginTool>,
    commands: Vec<PluginCommand>,
    events: Vec<String>,
    hooks: Vec<PluginHook>,
    ui: Vec<PluginUi>,
    rpc: Vec<PluginRpc>,
}

/// Host services exposed to an in-process plugin.
///
/// Only methods that are safe to call synchronously are implemented: logging,
/// storage, secrets, workspace files, session reads and blobs. Async host
/// services (network, UI panels, jobs, inter-plugin RPC) still require the
/// hosted/process backend, where the host can await without blocking the
/// plugin thread.
struct DylibHost {
    name: String,
    capabilities: BTreeSet<PluginCapability>,
    workspace: PathBuf,
    data_dir: PathBuf,
    store: Arc<Store>,
    bus: broadcast::Sender<Notification>,
    network_allow: Vec<String>,
    secrets: Vec<String>,
    env: BTreeMap<String, String>,
    logs: Arc<super::logs::PluginLogs>,
}

struct HostError {
    code: i32,
    message: String,
}

impl HostError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl DylibHost {
    fn dispatch(&self, method: &str, params: &Value) -> std::result::Result<Value, HostError> {
        if let Some(capability) = required_capability(method) {
            if !self.capabilities.contains(&capability) {
                return Err(HostError::new(
                    abi::EIX_ERR_DENIED,
                    format!(
                        "plugin {} was not granted the {} capability",
                        self.name,
                        capability.as_str()
                    ),
                ));
            }
        }

        match method {
            "host.log" => {
                self.write_log(params);
                Ok(Value::Null)
            }
            "host.info" => Ok(json!({
                "name": self.name,
                "backend": "dylib",
                "api_version": PLUGIN_API_VERSION,
                "capabilities": self.capabilities,
                "workspace": self.workspace,
                "data_dir": self.data_dir,
                "network_allow": self.network_allow,
                "secrets": self.secrets,
            })),
            "host.ping" => Ok(json!({"pong": true})),
            "host.notify" => self.notify(params),
            "host.storage.get" => {
                let key = self.storage_key(params)?;
                let scope = self.storage_scope(params)?;
                let value = self
                    .store
                    .plugin_kv_get(&self.name, &scope, &key)
                    .map_err(internal)?;
                Ok(json!({"value": value}))
            }
            "host.storage.set" => {
                let key = self.storage_key(params)?;
                let value = params.get("value").and_then(Value::as_str).ok_or_else(|| {
                    HostError::new(
                        abi::EIX_ERR_INVALID,
                        "host.storage.set needs a string `value`",
                    )
                })?;
                if value.len() > MAX_STORAGE_VALUE {
                    return Err(HostError::new(
                        abi::EIX_ERR_INVALID,
                        format!("storage value exceeds {MAX_STORAGE_VALUE} bytes"),
                    ));
                }
                let scope = self.storage_scope(params)?;
                self.store
                    .plugin_kv_set(&self.name, &scope, &key, value)
                    .map_err(internal)?;
                Ok(json!({"ok": true}))
            }
            "host.storage.delete" => {
                let key = self.storage_key(params)?;
                let scope = self.storage_scope(params)?;
                let deleted = self
                    .store
                    .plugin_kv_delete(&self.name, &scope, &key)
                    .map_err(internal)?;
                Ok(json!({"deleted": deleted}))
            }
            "host.storage.list" => {
                let scope = self.storage_scope(params)?;
                let items = self
                    .store
                    .plugin_kv_list(&self.name, &scope)
                    .map_err(internal)?
                    .into_iter()
                    .map(|(key, value)| json!({"key": key, "value": value}))
                    .collect::<Vec<_>>();
                Ok(json!({"items": items}))
            }
            "host.secrets.get" => self.secret_get(params),
            "host.fs.read" => WorkspaceFs::new(&self.workspace)
                .read(params)
                .map_err(invalid),
            "host.fs.write" => WorkspaceFs::new(&self.workspace)
                .write(params)
                .map_err(invalid),
            "host.fs.list" => WorkspaceFs::new(&self.workspace)
                .list(params)
                .map_err(invalid),
            "host.session.get" => self.session_get(params).map_err(invalid),
            "host.session.list" => self.session_list(params).map_err(invalid),
            "host.blob.put" => blob_put(&self.data_dir, params).map_err(invalid),
            "host.blob.get" => blob_get(&self.data_dir, params).map_err(invalid),
            "host.net.request"
            | "host.ui.open"
            | "host.ui.update"
            | "host.ui.close"
            | "host.rpc.call"
            | "host.job.register"
            | "host.job.trigger"
            | "host.job.unregister" => Err(HostError::new(
                abi::EIX_ERR_DENIED,
                format!(
                    "{method} is not available to in-process dylib plugins yet; \
                         use hosted mode for network, UI panels, jobs and inter-plugin RPC"
                ),
            )),
            other => Err(HostError::new(
                abi::EIX_ERR_UNKNOWN_METHOD,
                format!("unknown host method {other:?}"),
            )),
        }
    }

    fn write_log(&self, params: &Value) {
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("info");
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        eprintln!(
            "eix-plugin[{}] {level}: {}",
            self.name,
            redact(&cap_text(message, MAX_LOG))
        );
        self.logs
            .push(&self.name, level, &cap_text(message, MAX_LOG));
    }

    fn notify(&self, params: &Value) -> std::result::Result<Value, HostError> {
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("info");
        let message = cap_text(
            params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            MAX_LOG,
        );
        eprintln!("eix-plugin[{}] {level}: {}", self.name, redact(&message));
        let _ = self.bus.send(Notification::from_value(
            "plugin.notify",
            json!({"plugin": self.name, "level": level, "message": message}),
        ));
        Ok(json!({"ok": true}))
    }

    fn storage_key(&self, params: &Value) -> std::result::Result<String, HostError> {
        let key = params.get("key").and_then(Value::as_str).ok_or_else(|| {
            HostError::new(abi::EIX_ERR_INVALID, "storage method needs a string `key`")
        })?;
        if key.is_empty() || key.len() > MAX_STORAGE_KEY {
            return Err(HostError::new(
                abi::EIX_ERR_INVALID,
                format!("storage key must be 1..={MAX_STORAGE_KEY} bytes"),
            ));
        }
        if key.chars().any(char::is_control) {
            return Err(HostError::new(
                abi::EIX_ERR_INVALID,
                "storage key must not contain control characters",
            ));
        }
        Ok(key.to_string())
    }

    fn storage_scope(&self, params: &Value) -> std::result::Result<String, HostError> {
        let scope = params
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        match scope {
            "global" => Ok("global".to_string()),
            "workspace" => {
                let workspace = params
                    .get("workspace")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.workspace.clone());
                if !same_path(&workspace, &self.workspace) {
                    return Err(HostError::new(
                        abi::EIX_ERR_INVALID,
                        "workspace scope must name the plugin's workspace",
                    ));
                }
                Ok(format!("workspace:{}", workspace.display()))
            }
            other => Err(HostError::new(
                abi::EIX_ERR_INVALID,
                format!("unknown storage scope {other:?}; use global or workspace"),
            )),
        }
    }

    fn secret_get(&self, params: &Value) -> std::result::Result<Value, HostError> {
        let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
            HostError::new(
                abi::EIX_ERR_INVALID,
                "host.secrets.get needs a string `name`",
            )
        })?;
        if !self.secrets.iter().any(|allowed| allowed == name) {
            return Err(HostError::new(
                abi::EIX_ERR_DENIED,
                format!("secret {name:?} is not in plugins.{}.secrets", self.name),
            ));
        }
        let value = self
            .env
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .ok_or_else(|| {
                HostError::new(abi::EIX_ERR_INTERNAL, format!("secret {name:?} is not set"))
            })?;
        // Never log the value; the audit line only names the variable.
        eprintln!("eix-plugin[{}] secret read: {name}", self.name);
        Ok(json!({"name": name, "value": value}))
    }
    // ---- sessions -------------------------------------------------------

    fn session_get(&self, params: &Value) -> Result<Value> {
        let id = params
            .get("session_id")
            .and_then(Value::as_str)
            .context("host.session.get needs a string `session_id`")?;
        let session = self
            .store
            .get_session(id)?
            .with_context(|| format!("unknown session {id:?}"))?;
        serde_json::to_value(session).context("cannot encode session")
    }

    fn session_list(&self, params: &Value) -> Result<Value> {
        let include_archived = params
            .get("include_archived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let sessions = self.store.list_sessions_including(include_archived)?;
        let rows = sessions
            .into_iter()
            .take(500)
            .map(|session| {
                json!({
                    "id": session.id,
                    "name": session.name,
                    "cwd": session.cwd,
                    "status": session.status,
                    "origin": session.origin,
                    "mode": session.mode,
                    "archived": session.archived,
                    "updated_at": session.updated_at,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"sessions": rows}))
    }
}

fn invalid(err: anyhow::Error) -> HostError {
    HostError::new(abi::EIX_ERR_INVALID, format!("{err:#}"))
}

fn internal(err: anyhow::Error) -> HostError {
    HostError::new(abi::EIX_ERR_INTERNAL, format!("{err:#}"))
}

fn cap_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn same_path(left: &Path, right: &Path) -> bool {
    fn normalize(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }
    normalize(left) == normalize(right)
}

unsafe extern "C" fn host_dispatch(
    ctx: *mut c_void,
    method: abi::EixStr,
    params: abi::EixStr,
    out: *mut abi::EixBuf,
) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> std::result::Result<Value, HostError> {
            if ctx.is_null() {
                return Err(HostError::new(abi::EIX_ERR_INVALID, "null host context"));
            }
            // SAFETY: `ctx` is the `DylibHost` pointer passed in `HostApiV1`
            // and the backend keeps it alive for the instance's lifetime.
            let host = unsafe { &*(ctx as *const DylibHost) };
            let method = method
                .as_str()
                .map_err(|_| HostError::new(abi::EIX_ERR_INVALID, "host method is not UTF-8"))?;
            let params: Value = if params.as_bytes().is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(params.as_bytes()).map_err(|err| {
                    HostError::new(abi::EIX_ERR_INVALID, format!("invalid host params: {err}"))
                })?
            };
            host.dispatch(method, &params)
        },
    ));

    match result {
        Ok(Ok(value)) => {
            if !out.is_null() {
                let bytes = serde_json::to_vec(&value).unwrap_or_default();
                // SAFETY: `out` is non-null and owned by the plugin.
                unsafe { *out = abi::EixBuf::from_vec(bytes) };
            }
            abi::EIX_OK
        }
        Ok(Err(err)) => {
            if !out.is_null() {
                let bytes = serde_json::to_vec(&json!({
                    "code": abi::status_name(err.code),
                    "message": err.message,
                }))
                .unwrap_or_default();
                // SAFETY: `out` is non-null and owned by the plugin.
                unsafe { *out = abi::EixBuf::from_vec(bytes) };
            }
            err.code
        }
        Err(_) => {
            if !out.is_null() {
                let bytes = serde_json::to_vec(&json!({
                    "code": "panic",
                    "message": "host panicked while handling the call",
                }))
                .unwrap_or_default();
                // SAFETY: `out` is non-null and owned by the plugin.
                unsafe { *out = abi::EixBuf::from_vec(bytes) };
            }
            abi::EIX_ERR_PANIC
        }
    }
}

unsafe extern "C" fn host_free(_ctx: *mut c_void, buf: abi::EixBuf) {
    if buf.is_null() {
        return;
    }
    // SAFETY: the buffer was created by `host_dispatch` in this crate and is
    // released exactly once.
    drop(unsafe { buf.into_vec() });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn protocol_methods_map_to_abi_methods() {
        assert_eq!(translate_method("tools/call"), "tool.call");
        assert_eq!(translate_method("commands/run"), "command.run");
        assert_eq!(translate_method("hooks/run"), "hook.run");
        assert_eq!(translate_method("rpc.call"), "rpc.call");
        assert_eq!(translate_method("custom.method"), "custom.method");
    }

    #[test]
    fn tool_call_params_rename_tool_to_name() {
        let params = translate_params(
            "tools/call",
            json!({"tool": "echo", "arguments": {"text": "hi"}}),
        );
        assert_eq!(params["name"], "echo");
        assert!(params.get("tool").is_none());
        assert_eq!(params["arguments"]["text"], "hi");

        let params = translate_params("commands/run", json!({"command": "sync"}));
        assert_eq!(params["command"], "sync");
    }

    #[test]
    fn descriptor_maps_to_initialize_result() {
        let descriptor: Descriptor = serde_json::from_value(json!({
            "name": "acme",
            "version": "1.2.3",
            "abi": 1,
            "contributions": {
                "tools": [{"name": "echo", "description": "echo", "input_schema": {"type": "object"}}],
                "commands": [{"name": "sync", "action": "sync"}],
                "events": ["session.*"],
                "hooks": [{"point": "tool.before", "priority": 1}],
                "ui": [{"surface": "panel", "id": "issues", "title": "Issues"}],
                "rpc": [{"name": "issues.search"}]
            }
        }))
        .unwrap();
        let init = descriptor_to_init(descriptor);
        assert_eq!(init.name, "acme");
        assert_eq!(init.version, "1.2.3");
        assert_eq!(init.api_version, PLUGIN_API_VERSION);
        assert_eq!(init.tools[0].name, "echo");
        assert_eq!(init.commands[0].action.as_deref(), Some("sync"));
        assert_eq!(init.events, vec!["session.*"]);
        assert_eq!(init.hooks[0].point, "tool.before");
        assert_eq!(init.ui[0].id, "issues");
        assert_eq!(init.rpc[0].name, "issues.search");
    }

    fn test_spec(library: &Path) -> PluginSpec {
        PluginSpec {
            name: "hello-dylib".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            command: String::new(),
            args: Vec::new(),
            library: Some(library.to_path_buf()),
            env: BTreeMap::new(),
            cwd: std::env::temp_dir(),
            data_dir: std::env::temp_dir().join("eix-test").join("hello-dylib"),
            timeout: Duration::from_secs(10),
            capabilities: vec![PluginCapability::Tools, PluginCapability::Commands],
            allowed_tools: Vec::new(),
            denied_tools: Vec::new(),
            settings: BTreeMap::new(),
            settings_schema: None,
            sandbox: false,
            network_allow: Vec::new(),
            secrets: Vec::new(),
            rpc_allow: Vec::new(),
            has_server: true,
            declarative_commands: Vec::new(),
        }
    }

    /// End-to-end smoke test against the workspace example.
    ///
    /// The example is a cdylib, so `cargo test` does not build it by itself.
    /// Run:
    /// `cargo build -p eix-plugin-hello-dylib && cargo test -p eixd --lib -- --ignored dylib`
    #[test]
    #[ignore = "needs target/debug/libeix_plugin_hello_dylib.so"]
    fn example_dylib_round_trips() {
        let library = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug/libeix_plugin_hello_dylib.so");
        if !library.exists() {
            eprintln!("skipping: {} does not exist", library.display());
            return;
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let spec = test_spec(&library);
            let store = Arc::new(crate::store::Store::in_memory().unwrap());
            let (bus, _) = broadcast::channel(8);
            let logs = Arc::new(crate::plugin::logs::PluginLogs::new());
            let backend = DylibBackend::load(&library, &spec, store, bus, logs)
                .await
                .unwrap();

            let init = backend.initialize(&spec).await.unwrap();
            assert_eq!(init.name, "hello-dylib");
            assert!(init.tools.iter().any(|tool| tool.name == "echo"));
            assert!(init
                .commands
                .iter()
                .any(|command| command.action.as_deref() == Some("sync")));

            let value = backend
                .request(
                    "tools/call",
                    json!({"tool": "echo", "arguments": {"text": "hi"}}),
                )
                .await
                .unwrap();
            let result: eix_core::plugin::PluginToolCallResult =
                serde_json::from_value(value).unwrap();
            assert!(result.ok && !result.is_error);
            assert_eq!(result.to_text(), "hi");

            let value = backend
                .request(
                    "commands/run",
                    json!({"command": "sync", "arguments": "--full"}),
                )
                .await
                .unwrap();
            let result: eix_core::plugin::PluginCommandRunResult =
                serde_json::from_value(value).unwrap();
            assert!(result.ok);
            assert_eq!(result.output.as_deref(), Some("synced --full"));

            backend.shutdown().await;
            assert!(backend.is_closed());
        });
    }
}
