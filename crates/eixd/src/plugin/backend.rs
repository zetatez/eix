//! Transport-neutral extension backend.
//!
//! `PluginManager` owns the contribution registry, capability checks, hooks
//! and lifecycle state. It delegates the actual call to an
//! [`ExtensionBackend`], so a JSON-RPC child process and an in-process dylib
//! share exactly the same registration and routing code.
//!
//! Backend method names are the v2 protocol names (`tools/call`,
//! `commands/run`, `hooks/run`, `rpc.call`). Each backend translates them to
//! its own transport; callers never need to know which backend is in use.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use eix_core::plugin::PluginInitializeResult;
use serde_json::Value;

use super::PluginSpec;

/// A boxed future returned by a backend method. Written explicitly so the
/// trait stays object-safe without adding an `async-trait` dependency.
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One running plugin instance, regardless of transport.
pub trait ExtensionBackend: Send + Sync {
    fn name(&self) -> &str;

    /// Complete the handshake and return the advertised contributions.
    fn initialize<'a>(
        &'a self,
        spec: &'a PluginSpec,
    ) -> BackendFuture<'a, Result<PluginInitializeResult>>;

    /// Send one request using a v2 protocol method name.
    fn request<'a>(&'a self, method: &'a str, params: Value) -> BackendFuture<'a, Result<Value>>;

    /// Deliver a one-way notification. Best-effort: a busy backend may skip it.
    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BackendFuture<'a, Result<()>>;

    /// True once the backing instance can no longer serve requests.
    fn is_closed(&self) -> bool;

    /// A human-readable reason when the backend died, for `plugin.list`.
    fn exit_reason<'a>(&'a self) -> BackendFuture<'a, Option<String>>;

    /// Stop the instance. Must be idempotent.
    fn shutdown<'a>(&'a self) -> BackendFuture<'a, ()>;
}
