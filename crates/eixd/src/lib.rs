//! `eixd`: the persistent eix daemon.
//!
//! It owns sessions, the SQLite event log and the provider turn loop. Frontends
//! talk to it over a Unix socket using the protocol in `eix-core`.

pub mod provider;
pub mod server;
pub mod snapshot;
pub mod store;
pub mod tools;

pub use server::run;
