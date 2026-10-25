//! `eix nvim`: launch the user's Neovim with the embedded Lua frontend.
//!
//! Neovim is the client; the daemon stays the source of truth. The plugin
//! speaks newline-delimited JSON-RPC over the same Unix socket (or TCP with a
//! token) as the TUI, renders the transcript into a scratch buffer, and sends
//! prompts from a normal buffer.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use eix_core::{nvim_plugin, Endpoint};

pub async fn launch(endpoint: Endpoint, data_dir: &Path, session: Option<String>) -> Result<()> {
    let plugin = nvim_plugin::write(data_dir)?;
    let exe = std::env::var("EIX_NVIM").unwrap_or_else(|_| "nvim".to_string());

    let mut command = Command::new(&exe);
    // Load the plugin from an environment variable so paths never need Lua
    // escaping. `--cmd` runs before the user's config; `-c` runs after.
    command
        .env("EIX_NVIM_PLUGIN", &plugin)
        .env("EIX_NVIM_SESSION", session.as_deref().unwrap_or(""))
        .env("EIX_NVIM_DATA", data_dir)
        // Always define all endpoint variables so a stale inherited value
        // cannot redirect the plugin to another socket.
        .env("EIX_NVIM_SOCKET", "")
        .env("EIX_NVIM_TCP", "")
        .env("EIX_NVIM_TOKEN", "")
        .arg("--cmd")
        .arg("lua dofile(vim.env.EIX_NVIM_PLUGIN)");
    match &endpoint {
        Endpoint::Unix(path) => {
            command.env("EIX_NVIM_SOCKET", path);
        }
        Endpoint::Tcp { addr, token } => {
            command.env("EIX_NVIM_TCP", addr);
            command.env("EIX_NVIM_TOKEN", token.as_deref().unwrap_or(""));
        }
    }
    if let Some(session) = session.as_deref() {
        command.arg("-c").arg(format!("Eix attach {session}"));
    }
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command
        .status()
        .with_context(|| format!("cannot start {exe}; set EIX_NVIM to the Neovim binary"))?;
    if !status.success() {
        bail!("nvim exited with {status}");
    }
    Ok(())
}
