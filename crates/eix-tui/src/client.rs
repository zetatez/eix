//! Minimal CLI-side client for the daemon protocol, plus lazy daemon startup.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use eix_core::{Notification, Request, ServerMessage};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::UnixStream;

pub struct Client {
    writer: WriteHalf<UnixStream>,
    reader: BufReader<ReadHalf<UnixStream>>,
    next_id: u64,
    /// Notifications that arrived while waiting for a response.
    pending: VecDeque<Notification>,
}

impl Client {
    pub async fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("cannot connect to {}", socket.display()))?;
        let (read, write) = tokio::io::split(stream);
        Ok(Self {
            writer: write,
            reader: BufReader::new(read),
            next_id: 1,
            pending: VecDeque::new(),
        })
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request::new(id, method, params);
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;

        loop {
            match self.read_message().await? {
                ServerMessage::Response(response) if response.id == id => {
                    if let Some(error) = response.error {
                        bail!("{}: {}", error.code, error.message);
                    }
                    return Ok(response.result.unwrap_or(Value::Null));
                }
                ServerMessage::Response(response) => {
                    bail!("unexpected response id {} (waiting for {id})", response.id)
                }
                ServerMessage::Notification(notification) => self.pending.push_back(notification),
            }
        }
    }

    pub async fn next_notification(&mut self) -> Result<Notification> {
        if let Some(notification) = self.pending.pop_front() {
            return Ok(notification);
        }
        match self.read_message().await? {
            ServerMessage::Notification(notification) => Ok(notification),
            ServerMessage::Response(response) => {
                bail!(
                    "unexpected response id {} (no request pending)",
                    response.id
                )
            }
        }
    }

    async fn read_message(&mut self) -> Result<ServerMessage> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).await?;
        if read == 0 {
            bail!("daemon closed the connection");
        }
        serde_json::from_str(line.trim_end())
            .with_context(|| format!("invalid message from daemon: {}", line.trim_end()))
    }
}

/// Connect to the daemon, starting `eixd` in the background if needed.
pub async fn ensure_daemon(
    socket: &Path,
    data_dir: &Path,
    config: Option<&Path>,
) -> Result<Client> {
    ensure_running(socket, data_dir, config).await?;
    Client::connect(socket).await
}

/// Make sure a daemon is listening, without opening a protocol client.
pub async fn ensure_running(socket: &Path, data_dir: &Path, config: Option<&Path>) -> Result<()> {
    if UnixStream::connect(socket).await.is_ok() {
        return Ok(());
    }

    spawn_daemon(socket, data_dir, config)?;

    let mut last_error = None;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        match UnixStream::connect(socket).await {
            Ok(_) => return Ok(()),
            Err(err) => last_error = Some(err),
        }
    }
    let log = data_dir.join("eixd.log");
    match last_error {
        Some(err) => bail!(
            "daemon did not start at {}: {err}\ncheck {}",
            socket.display(),
            log.display()
        ),
        None => bail!(
            "daemon did not start at {}; check {}",
            socket.display(),
            log.display()
        ),
    }
}

fn spawn_daemon(socket: &Path, data_dir: &Path, config: Option<&Path>) -> Result<()> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700)).ok();
    }

    let log_path = data_dir.join("eixd.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600)).ok();
    }
    let stderr = log.try_clone()?;

    let exe = std::env::current_exe().context("cannot locate the eix executable")?;
    let sibling = exe.with_file_name("eixd");
    let program = if sibling.exists() {
        sibling
    } else {
        PathBuf::from("eixd")
    };

    let mut command = Command::new(&program);
    command
        .arg("--socket")
        .arg(socket)
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));

    if let Some(config) = config {
        command.arg("--config").arg(config);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Keep the daemon alive when the terminal sends SIGINT to the client.
        command.process_group(0);
    }

    command.spawn().with_context(|| {
        format!(
            "cannot start {} (build the workspace with `cargo build` first)",
            program.display()
        )
    })?;
    Ok(())
}
