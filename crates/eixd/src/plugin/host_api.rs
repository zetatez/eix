//! Host API served to plugin processes.
//!
//! Every method checks the capability granted in configuration. Methods that
//! are reserved by the design but not implemented in this build return
//! `unsupported` instead of silently doing nothing.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use eix_core::plugin::{error_code, PluginCapability, RpcRequest, RpcResponse, PLUGIN_API_VERSION};
use eix_core::redact::redact;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::store::Store;

const MAX_STORAGE_KEY: usize = 512;
const MAX_STORAGE_VALUE: usize = 1_000_000;

/// Everything a host method needs. Cloned per incoming request.
#[derive(Clone)]
pub struct HostContext {
    name: String,
    capabilities: BTreeSet<PluginCapability>,
    workspace: PathBuf,
    store: Arc<Store>,
    frame_tx: mpsc::Sender<Value>,
}

impl HostContext {
    pub fn new(
        name: String,
        capabilities: BTreeSet<PluginCapability>,
        workspace: PathBuf,
        store: Arc<Store>,
        frame_tx: mpsc::Sender<Value>,
    ) -> Self {
        Self {
            name,
            capabilities,
            workspace,
            store,
            frame_tx,
        }
    }

    /// Handle one `host.*` request and write its response back to the plugin.
    pub async fn handle(&self, request: RpcRequest) {
        if request.method.starts_with("host.storage.")
            && !self.capabilities.contains(&PluginCapability::Storage)
        {
            let response = RpcResponse::err(
                request.id,
                error_code::UNAUTHORIZED,
                format!(
                    "plugin {} was not granted the {} capability",
                    self.name,
                    PluginCapability::Storage.as_str()
                ),
            );
            let _ = self
                .frame_tx
                .send(serde_json::to_value(response).unwrap_or(Value::Null))
                .await;
            return;
        }
        let response = match request.method.as_str() {
            "host.log" => self.log_request(request.id, &request.params),
            "host.info" => RpcResponse::ok(
                request.id,
                json!({
                    "name": self.name,
                    "api_version": PLUGIN_API_VERSION,
                    "capabilities": self.capabilities,
                    "workspace": self.workspace,
                }),
            ),
            "host.ping" => RpcResponse::ok(request.id, json!({"pong": true})),
            "host.storage.get" => match self.storage_get(&request.params) {
                Ok(value) => RpcResponse::ok(request.id, json!({ "value": value })),
                Err(err) => {
                    RpcResponse::err(request.id, error_code::INVALID_PARAMS, format!("{err:#}"))
                }
            },
            "host.storage.set" => match self.storage_set(&request.params) {
                Ok(()) => RpcResponse::ok(request.id, json!({"ok": true})),
                Err(err) => {
                    RpcResponse::err(request.id, error_code::INVALID_PARAMS, format!("{err:#}"))
                }
            },
            "host.storage.delete" => match self.storage_delete(&request.params) {
                Ok(deleted) => RpcResponse::ok(request.id, json!({ "deleted": deleted })),
                Err(err) => {
                    RpcResponse::err(request.id, error_code::INVALID_PARAMS, format!("{err:#}"))
                }
            },
            "host.storage.list" => match self.storage_list(&request.params) {
                Ok(items) => RpcResponse::ok(
                    request.id,
                    json!({
                        "items": items
                            .into_iter()
                            .map(|(key, value)| json!({"key": key, "value": value}))
                            .collect::<Vec<_>>()
                    }),
                ),
                Err(err) => {
                    RpcResponse::err(request.id, error_code::INVALID_PARAMS, format!("{err:#}"))
                }
            },
            method if reserved_host_method(method) => RpcResponse::err(
                request.id,
                error_code::UNSUPPORTED,
                format!("{method} is reserved and not implemented in this build"),
            ),
            other => RpcResponse::err(
                request.id,
                error_code::METHOD_NOT_FOUND,
                format!("unknown host method {other:?}"),
            ),
        };
        let _ = self
            .frame_tx
            .send(serde_json::to_value(response).unwrap_or(Value::Null))
            .await;
    }

    /// Support `host.log` sent as a notification (no response expected).
    pub fn log_notification(&self, value: &Value) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        self.write_log(&params);
    }

    fn log_request(&self, id: u64, params: &Value) -> RpcResponse {
        self.write_log(params);
        RpcResponse::ok(id, Value::Null)
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
            redact(&cap(message, 4096))
        );
    }

    fn require(&self, capability: PluginCapability) -> Result<()> {
        if !self.capabilities.contains(&capability) {
            bail!(
                "plugin {} was not granted the {} capability",
                self.name,
                capability.as_str()
            );
        }
        Ok(())
    }

    fn storage_scope(&self, params: &Value) -> Result<String> {
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
                Ok(format!("workspace:{}", workspace.display()))
            }
            other => bail!("unknown storage scope {other:?}; use global or workspace"),
        }
    }

    fn storage_get(&self, params: &Value) -> Result<Option<String>> {
        self.require(PluginCapability::Storage)?;
        let key = storage_key(params)?;
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_get(&self.name, &scope, &key)
    }

    fn storage_set(&self, params: &Value) -> Result<()> {
        self.require(PluginCapability::Storage)?;
        let key = storage_key(params)?;
        let value = params
            .get("value")
            .and_then(Value::as_str)
            .context("host.storage.set needs a string `value`")?;
        if value.len() > MAX_STORAGE_VALUE {
            bail!("storage value exceeds {MAX_STORAGE_VALUE} bytes");
        }
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_set(&self.name, &scope, &key, value)
    }

    fn storage_delete(&self, params: &Value) -> Result<bool> {
        self.require(PluginCapability::Storage)?;
        let key = storage_key(params)?;
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_delete(&self.name, &scope, &key)
    }

    fn storage_list(&self, params: &Value) -> Result<Vec<(String, String)>> {
        self.require(PluginCapability::Storage)?;
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_list(&self.name, &scope)
    }
}

/// Methods that the protocol reserves but this build does not implement.
fn reserved_host_method(method: &str) -> bool {
    matches!(
        method,
        "host.fs.read"
            | "host.fs.write"
            | "host.fs.list"
            | "host.net.request"
            | "host.secrets.get"
            | "host.session.get"
            | "host.session.list"
            | "host.notify"
            | "host.ui.open"
            | "host.ui.update"
            | "host.ui.close"
            | "host.rpc.call"
            | "host.blob.put"
            | "host.blob.get"
            | "host.job.register"
            | "host.job.trigger"
    )
}

fn storage_key(params: &Value) -> Result<String> {
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .context("storage method needs a string `key`")?;
    if key.is_empty() || key.len() > MAX_STORAGE_KEY {
        bail!("storage key must be 1..={MAX_STORAGE_KEY} bytes");
    }
    if key.chars().any(char::is_control) {
        bail!("storage key must not contain control characters");
    }
    Ok(key.to_string())
}

fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_methods_are_known() {
        assert!(reserved_host_method("host.fs.read"));
        assert!(reserved_host_method("host.ui.open"));
        assert!(!reserved_host_method("host.storage.get"));
        assert!(!reserved_host_method("host.unknown"));
    }

    #[test]
    fn storage_keys_are_bounded() {
        assert!(storage_key(&json!({"key": "a"})).is_ok());
        assert!(storage_key(&json!({"key": ""})).is_err());
        assert!(storage_key(&json!({"key": "a\nb"})).is_err());
        assert!(storage_key(&json!({"key": "a".repeat(MAX_STORAGE_KEY + 1)})).is_err());
        assert!(storage_key(&json!({})).is_err());
    }

    fn context(capabilities: &[PluginCapability]) -> (HostContext, mpsc::Receiver<Value>) {
        let (tx, rx) = mpsc::channel(8);
        let host = HostContext::new(
            "acme".into(),
            capabilities.iter().copied().collect::<BTreeSet<_>>(),
            PathBuf::from("/tmp"),
            Arc::new(Store::in_memory().unwrap()),
            tx,
        );
        (host, rx)
    }

    async fn call(
        host: &HostContext,
        rx: &mut mpsc::Receiver<Value>,
        method: &str,
        params: Value,
    ) -> RpcResponse {
        host.handle(RpcRequest::new(1, method, params)).await;
        serde_json::from_value(rx.recv().await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn storage_round_trips_with_the_capability() {
        let (host, mut rx) = context(&[PluginCapability::Storage]);
        let response = call(
            &host,
            &mut rx,
            "host.storage.set",
            json!({"scope": "global", "key": "k", "value": "v"}),
        )
        .await;
        assert!(response.error.is_none());
        let response = call(
            &host,
            &mut rx,
            "host.storage.get",
            json!({"scope": "global", "key": "k"}),
        )
        .await;
        assert_eq!(response.result.unwrap()["value"], "v");
        let response = call(
            &host,
            &mut rx,
            "host.storage.delete",
            json!({"scope": "global", "key": "k"}),
        )
        .await;
        assert_eq!(response.result.unwrap()["deleted"], true);
    }

    #[tokio::test]
    async fn storage_without_the_capability_is_rejected() {
        let (host, mut rx) = context(&[]);
        let response = call(
            &host,
            &mut rx,
            "host.storage.get",
            json!({"scope": "global", "key": "k"}),
        )
        .await;
        assert_eq!(
            response.error.expect("error").code,
            error_code::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn reserved_methods_return_unsupported() {
        let (host, mut rx) = context(&[]);
        let response = call(&host, &mut rx, "host.fs.read", json!({})).await;
        assert_eq!(response.error.expect("error").code, error_code::UNSUPPORTED);
    }
}
