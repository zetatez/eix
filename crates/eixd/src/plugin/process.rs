//! JSON-RPC child-process backend.
//!
//! This is the existing v2 transport wrapped in [`ExtensionBackend`]. The
//! child owns its own process, so a crash or hang is isolated from the daemon.

use std::sync::Arc;

use anyhow::Result;
use eix_core::plugin::PluginInitializeResult;
use eix_core::Notification;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::store::Store;

use super::backend::{BackendFuture, ExtensionBackend};
use super::client::PluginClient;
use super::host_api::RpcCallRequest;
use super::PluginSpec;

/// One running plugin process.
pub struct ProcessBackend {
    client: Arc<PluginClient>,
}

impl ProcessBackend {
    pub async fn spawn(
        spec: &PluginSpec,
        store: Arc<Store>,
        bus: broadcast::Sender<Notification>,
        rpc_tx: mpsc::Sender<RpcCallRequest>,
    ) -> Result<Arc<Self>> {
        let client = PluginClient::spawn(spec, store, bus, rpc_tx).await?;
        Ok(Arc::new(Self { client }))
    }
}

impl ExtensionBackend for ProcessBackend {
    fn name(&self) -> &str {
        &self.client.name
    }

    fn initialize<'a>(
        &'a self,
        spec: &'a PluginSpec,
    ) -> BackendFuture<'a, Result<PluginInitializeResult>> {
        Box::pin(async move { self.client.initialize(spec, &spec.cwd).await })
    }

    fn request<'a>(&'a self, method: &'a str, params: Value) -> BackendFuture<'a, Result<Value>> {
        Box::pin(async move { self.client.request(method, params).await })
    }

    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BackendFuture<'a, Result<()>> {
        Box::pin(async move { self.client.notify_event(method, &params).await })
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    fn exit_reason<'a>(&'a self) -> BackendFuture<'a, Option<String>> {
        Box::pin(async move { self.client.exit_reason().await })
    }

    fn shutdown<'a>(&'a self) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.client.shutdown().await })
    }
}
