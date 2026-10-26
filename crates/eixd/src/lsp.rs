//! Minimal stdio Language Server Protocol client.
//!
//! eix asks a configured server for diagnostics after a write and exposes an
//! `lsp_diagnostics` tool. Servers are spawned lazily per workspace and kept
//! alive for the daemon's lifetime. Only `textDocument/publishDiagnostics` is
//! consumed; completion, hover and other LSP features are out of scope.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::{Config, LspDiagnosticInfo, LspServerConfig, LspWorkspaceResult};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: u32,
    pub character: u32,
    /// `error`, `warning`, `info`, or `hint`.
    pub severity: &'static str,
    pub message: String,
}

impl Diagnostic {
    pub fn label(&self, path: &Path) -> String {
        format!(
            "{}:{}:{}: {}: {}",
            path.display(),
            self.line + 1,
            self.character + 1,
            self.severity,
            self.message
        )
    }
}

struct Client {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
    /// Latest diagnostics per document URI.
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Open document versions, for `didChange`.
    versions: HashMap<String, i64>,
}

impl Client {
    async fn send(&mut self, message: Value) -> Result<()> {
        let body = serde_json::to_vec(&message)?;
        self.stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        self.stdin.write_all(&body).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let Some(message) = self.read_message(self.timeout).await? else {
                bail!("lsp {method} timed out");
            };
            if self.observe(&message).await? {
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("lsp {method} failed: {error}");
            }
            return message
                .get("result")
                .cloned()
                .context("lsp response has no result");
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    /// Handle server-initiated messages while waiting for our response.
    /// Returns true when the message was consumed and the caller should keep
    /// waiting.
    async fn observe(&mut self, message: &Value) -> Result<bool> {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(params) = message.get("params") {
                    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                        let list = parse_diagnostics(params);
                        self.diagnostics.insert(uri.to_string(), list);
                    }
                }
                return Ok(false);
            }
            if let Some(id) = message.get("id") {
                // workspace/configuration, client/registerCapability, ...
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": Value::Null,
                }))
                .await?;
                return Ok(true);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Read one framed JSON message. `wait` only bounds the first byte, so a
    /// message already in flight is never cut in half by the idle timeout.
    async fn read_message(&mut self, wait: Duration) -> Result<Option<Value>> {
        let mut first = [0u8; 1];
        match tokio::time::timeout(wait, self.stdout.read_exact(&mut first)).await {
            Err(_) => return Ok(None),
            Ok(Err(err)) => return Err(err).context("lsp server closed the connection"),
            Ok(Ok(_)) => {}
        }
        let mut header = vec![first[0]];
        loop {
            let mut byte = [0u8; 1];
            tokio::time::timeout(self.timeout, self.stdout.read_exact(&mut byte))
                .await
                .context("lsp header timed out")??;
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") || header.ends_with(b"\n\n") {
                break;
            }
        }
        let header = String::from_utf8_lossy(&header);
        let length = header
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .context("lsp message has no Content-Length")?;
        if length > 16 * 1024 * 1024 {
            bail!("lsp message is too large ({length} bytes)");
        }
        let mut body = vec![0u8; length];
        tokio::time::timeout(self.timeout, self.stdout.read_exact(&mut body))
            .await
            .context("lsp body timed out")??;
        Ok(Some(serde_json::from_slice(&body)?))
    }

    async fn initialize(&mut self, root: &Path, server: &LspServerConfig) -> Result<()> {
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "eix", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": path_to_uri(root),
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {}
                },
                "workspace": {
                    "configuration": false,
                    "workspaceFolders": false
                }
            },
        });
        if let Some(options) = &server.initialization_options {
            params["initializationOptions"] = options.clone();
        }
        self.request("initialize", params).await?;
        self.notify("initialized", json!({})).await
    }

    async fn open_document(&mut self, path: &Path, language_id: &str) -> Result<String> {
        let uri = path_to_uri(path);
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("cannot read {} for LSP", path.display()))?;
        if let Some(version) = self.versions.get_mut(&uri) {
            *version += 1;
            let version = *version;
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [{"text": text}],
                }),
            )
            .await?;
        } else {
            self.versions.insert(uri.clone(), 1);
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": text,
                    }
                }),
            )
            .await?;
        }
        Ok(uri)
    }

    /// Read notifications until the deadline. `idle` bounds how long the
    /// server may stay silent after the first diagnostic, so a chatty server
    /// does not consume the whole timeout.
    async fn collect(&mut self, deadline: Instant, idle: Duration) -> Result<()> {
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(message) = self.read_message(remaining.min(idle)).await? else {
                break;
            };
            self.observe(&message).await?;
        }
        Ok(())
    }

    async fn diagnostics(&mut self, path: &Path, language_id: &str) -> Result<Vec<Diagnostic>> {
        let uri = self.open_document(path, language_id).await?;
        let deadline = Instant::now() + self.timeout.min(Duration::from_secs(2));
        self.collect(deadline, Duration::from_millis(500)).await?;
        Ok(self.diagnostics.remove(&uri).unwrap_or_default())
    }

    async fn workspace_diagnostics(
        &mut self,
        paths: &[(PathBuf, String)],
    ) -> Vec<(PathBuf, Result<Vec<Diagnostic>>)> {
        let mut opened = Vec::new();
        let mut failed = Vec::new();
        for (path, language_id) in paths {
            match self.open_document(path, language_id).await {
                Ok(uri) => opened.push((path.clone(), uri)),
                Err(err) => {
                    eprintln!("eixd: lsp open {}: {err:#}", path.display());
                    failed.push((path.clone(), err));
                }
            }
        }
        if !opened.is_empty() {
            let deadline = Instant::now() + self.timeout.min(Duration::from_secs(5));
            if let Err(err) = self.collect(deadline, Duration::from_millis(800)).await {
                eprintln!("eixd: lsp workspace collection failed: {err:#}");
            }
        }
        let mut results: Vec<_> = opened
            .into_iter()
            .map(|(path, uri)| (path, Ok(self.diagnostics.remove(&uri).unwrap_or_default())))
            .collect();
        results.extend(failed.into_iter().map(|(path, err)| (path, Err(err))));
        results
    }
}

/// Lazily-started language servers keyed by server name and workspace root.
#[derive(Default)]
pub struct LspSet {
    clients: HashMap<(String, PathBuf), Client>,
    failures: HashMap<(String, PathBuf), String>,
}

impl LspSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every server. Called on config reload.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// True when some enabled server claims this file's extension.
    pub fn handles(config: &Config, path: &Path) -> bool {
        server_for(config, path).is_some()
    }

    /// Diagnostics for one file. Returns an empty list when no server handles
    /// the extension.
    pub async fn diagnostics(
        &mut self,
        config: &Config,
        cwd: &Path,
        path: &Path,
    ) -> Result<Vec<Diagnostic>> {
        let Some((name, server)) = server_for(config, path) else {
            return Ok(Vec::new());
        };
        let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let key = (name.to_string(), root.clone());
        ensure_client(&mut self.clients, &mut self.failures, name, server, &root).await?;
        let client = self.clients.get_mut(&key).expect("inserted above");
        let language_id = language_id(server, path);
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        client.diagnostics(&path, &language_id).await
    }

    /// Diagnostics for several explicit files. Files handled by the same
    /// language server are opened together and collected in one pass, so a
    /// parallel write segment pays the collection window once instead of once
    /// per file. Paths with no configured server return an empty list.
    pub async fn diagnostics_many(
        &mut self,
        config: &Config,
        cwd: &Path,
        paths: &[PathBuf],
    ) -> Vec<(PathBuf, std::result::Result<Vec<Diagnostic>, String>)> {
        let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut grouped: HashMap<(String, PathBuf), Vec<(PathBuf, String)>> = HashMap::new();
        let mut targets = Vec::with_capacity(paths.len());
        for path in paths {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            targets.push((path.clone(), canonical.clone()));
            let Some((name, server)) = server_for(config, path) else {
                continue;
            };
            grouped
                .entry((name.to_string(), root.clone()))
                .or_default()
                .push((canonical, language_id(server, path)));
        }

        let mut collected: HashMap<PathBuf, std::result::Result<Vec<Diagnostic>, String>> =
            HashMap::new();
        for (key, entries) in grouped {
            let Some(server) = config.lsp.servers.get(&key.0) else {
                continue;
            };
            if let Err(err) = ensure_client(
                &mut self.clients,
                &mut self.failures,
                &key.0,
                server,
                &key.1,
            )
            .await
            {
                let message = format!("{err:#}");
                for (path, _) in entries {
                    collected.insert(path, Err(message.clone()));
                }
                continue;
            }
            let Some(client) = self.clients.get_mut(&key) else {
                continue;
            };
            for (path, result) in client.workspace_diagnostics(&entries).await {
                collected.insert(path, result.map_err(|err| format!("{err:#}")));
            }
        }

        targets
            .into_iter()
            .map(|(path, canonical)| {
                let result = collected
                    .get(&canonical)
                    .cloned()
                    .unwrap_or_else(|| Ok(Vec::new()));
                (path, result)
            })
            .collect()
    }

    /// Open every source file under `cwd` that a configured server claims and
    /// aggregate published diagnostics. This is intentionally on-demand: some
    /// language servers are expensive to warm up.
    pub async fn workspace(
        &mut self,
        config: &Config,
        cwd: &Path,
        max_files: usize,
    ) -> Result<LspWorkspaceResult> {
        let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let (files, skipped) = workspace_files(&root, config, max_files);
        if files.is_empty() {
            return Ok(LspWorkspaceResult {
                skipped,
                ..Default::default()
            });
        }

        let mut grouped: HashMap<(String, PathBuf), Vec<(PathBuf, String)>> = HashMap::new();
        for path in files {
            let Some((name, server)) = server_for(config, &path) else {
                continue;
            };
            let language = language_id(server, &path);
            grouped
                .entry((name.to_string(), root.clone()))
                .or_default()
                .push((path, language));
        }
        let mut diagnostics = Vec::new();
        let mut servers = Vec::new();
        let mut opened = 0usize;
        let mut keys: Vec<_> = grouped.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let Some(server) = config.lsp.servers.get(&key.0) else {
                continue;
            };
            if let Err(err) = ensure_client(
                &mut self.clients,
                &mut self.failures,
                &key.0,
                server,
                &key.1,
            )
            .await
            {
                eprintln!("eixd: lsp server {:?}: {err:#}", key.0);
                continue;
            }
            let Some(paths) = grouped.get(&key) else {
                continue;
            };
            opened += paths.len();
            let client = self.clients.get_mut(&key).expect("inserted above");
            let results = client.workspace_diagnostics(paths).await;
            for (path, list) in results {
                let list = match list {
                    Ok(list) => list,
                    Err(err) => {
                        eprintln!("eixd: lsp diagnostics {}: {err:#}", path.display());
                        continue;
                    }
                };
                for diagnostic in list {
                    diagnostics.push(LspDiagnosticInfo {
                        path: path.clone(),
                        line: diagnostic.line,
                        character: diagnostic.character,
                        severity: diagnostic.severity.to_string(),
                        message: diagnostic.message,
                    });
                }
            }
            servers.push(key.0);
        }
        diagnostics.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then(a.line.cmp(&b.line))
                .then(a.character.cmp(&b.character))
        });
        diagnostics.truncate(2000);
        Ok(LspWorkspaceResult {
            diagnostics,
            files: opened,
            servers,
            skipped,
        })
    }
}

async fn ensure_client(
    clients: &mut HashMap<(String, PathBuf), Client>,
    failures: &mut HashMap<(String, PathBuf), String>,
    name: &str,
    server: &LspServerConfig,
    root: &Path,
) -> Result<()> {
    let key = (name.to_string(), root.to_path_buf());
    if let Some(error) = failures.get(&key) {
        bail!("lsp server {name:?} is unavailable: {error}");
    }
    if clients.contains_key(&key) {
        return Ok(());
    }
    let mut client = spawn(server, root)
        .await
        .with_context(|| format!("cannot start lsp server {name:?} ({})", server.command))?;
    if let Err(err) = client.initialize(root, server).await {
        failures.insert(key, format!("{err:#}"));
        return Err(err);
    }
    clients.insert(key, client);
    Ok(())
}

fn workspace_files(root: &Path, config: &Config, max: usize) -> (Vec<PathBuf>, usize) {
    fn walk(dir: &Path, config: &Config, max: usize, out: &mut Vec<PathBuf>, skipped: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if matches!(
                    name.as_ref(),
                    ".git"
                        | ".hg"
                        | ".svn"
                        | ".eix"
                        | ".opencode"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                ) {
                    continue;
                }
                walk(&path, config, max, out, skipped);
            } else if kind.is_file() && server_for(config, &path).is_some() {
                if out.len() >= max {
                    *skipped += 1;
                } else {
                    out.push(path);
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut skipped = 0;
    walk(root, config, max, &mut out, &mut skipped);
    (out, skipped)
}

fn server_for<'a>(config: &'a Config, path: &Path) -> Option<(&'a str, &'a LspServerConfig)> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();
    config
        .lsp
        .servers
        .iter()
        .find(|(_, server)| {
            server.enabled
                && server
                    .extensions
                    .iter()
                    .any(|ext| ext.trim_start_matches('.').eq_ignore_ascii_case(&extension))
        })
        .map(|(name, server)| (name.as_str(), server))
}

fn language_id(server: &LspServerConfig, path: &Path) -> String {
    if let Some(id) = &server.language_id {
        return id.clone();
    }
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "go" => "go",
        "c" => "c",
        "h" | "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "json" => "json",
        "toml" => "toml",
        "md" => "markdown",
        "sh" | "bash" => "shellscript",
        other => other,
    }
    .to_string()
}

async fn spawn(server: &LspServerConfig, root: &Path) -> Result<Client> {
    let mut command = Command::new(&server.command);
    command
        .current_dir(root)
        .args(&server.args)
        .envs(&server.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().context("spawn failed")?;
    let stdin = child.stdin.take().context("lsp server has no stdin")?;
    let stdout = child.stdout.take().context("lsp server has no stdout")?;
    Ok(Client {
        _child: child,
        stdin,
        stdout: BufReader::new(stdout),
        next_id: 1,
        timeout: Duration::from_secs(server.timeout_secs.clamp(1, 120)),
        diagnostics: HashMap::new(),
        versions: HashMap::new(),
    })
}

fn parse_diagnostics(params: &Value) -> Vec<Diagnostic> {
    params
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let message = item.get("message").and_then(Value::as_str)?.trim();
                    if message.is_empty() {
                        return None;
                    }
                    let range = item.get("range")?;
                    Some(Diagnostic {
                        line: range
                            .get("start")
                            .and_then(|start| start.get("line"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        character: range
                            .get("start")
                            .and_then(|start| start.get("character"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        severity: match item.get("severity").and_then(Value::as_u64) {
                            Some(1) => "error",
                            Some(2) => "warning",
                            Some(3) => "info",
                            _ => "hint",
                        },
                        message: message.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `file://` URI with the bytes that matter for paths percent-encoded.
pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_severity_and_positions() {
        let params = json!({
            "uri": "file:///x",
            "diagnostics": [
                {"range": {"start": {"line": 2, "character": 4}}, "severity": 1, "message": "bad"},
                {"range": {"start": {"line": 0, "character": 0}}, "severity": 4, "message": "meh"}
            ]
        });
        let diagnostics = parse_diagnostics(&params);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].severity, "error");
        assert_eq!(diagnostics[0].line, 2);
        assert_eq!(diagnostics[1].severity, "hint");
    }

    #[test]
    fn encodes_uris() {
        assert_eq!(
            path_to_uri(Path::new("/tmp/a b#c.rs")),
            "file:///tmp/a%20b%23c.rs"
        );
    }

    #[tokio::test]
    async fn talks_to_a_stdio_server_and_refreshes_diagnostics() {
        if Command::new("python3")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            eprintln!("python3 unavailable; skipping LSP integration test");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "eix-lsp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(
            &script,
            r#"
import sys, json

def read():
    headers = {}
    line = sys.stdin.buffer.readline()
    if not line:
        return None
    while line not in (b"\r\n", b"\n"):
        key, value = line.split(b":", 1)
        headers[key.strip().lower()] = value.strip()
        line = sys.stdin.buffer.readline()
    return json.loads(sys.stdin.buffer.read(int(headers[b"content-length"])))

def send(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()

while True:
    message = read()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        uri = message["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": uri, "diagnostics": [{"range": {"start": {"line": 0, "character": 0}},
            "severity": 2, "message": "fake warning"}]}})
    elif method == "textDocument/didChange":
        uri = message["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": uri, "diagnostics": [{"range": {"start": {"line": 1, "character": 0}},
            "severity": 1, "message": "fake error"}]}})
"#,
        )
        .unwrap();
        let file = dir.join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let mut config = Config::default();
        config.lsp.servers.clear();
        config.lsp.servers.insert(
            "fake".into(),
            LspServerConfig {
                command: "python3".into(),
                args: vec![script.display().to_string()],
                extensions: vec!["rs".into()],
                timeout_secs: 5,
                ..Default::default()
            },
        );
        let mut set = LspSet::new();
        let first = set.diagnostics(&config, &dir, &file).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].severity, "warning");
        assert_eq!(first[0].message, "fake warning");
        std::fs::write(&file, "fn main() { let x = 1; }\n").unwrap();
        let second = set.diagnostics(&config, &dir, &file).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].severity, "error");
        assert_eq!(second[0].line, 1);

        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/other.rs"), "fn other() {}\n").unwrap();
        std::fs::write(dir.join("target/ignored.rs"), "fn ignored() {}\n").unwrap();
        let workspace = set.workspace(&config, &dir, 10).await.unwrap();
        assert_eq!(workspace.files, 2, "{:?}", workspace.diagnostics);
        assert_eq!(workspace.servers, vec!["fake".to_string()]);
        assert_eq!(workspace.diagnostics.len(), 2);
        assert!(workspace
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "fake error"));
        assert!(workspace
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "fake warning"));
        assert!(!workspace
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path.ends_with("ignored.rs")));
        let limited = set.workspace(&config, &dir, 1).await.unwrap();
        assert_eq!(limited.files, 1);
        assert_eq!(limited.skipped, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn batch_diagnostics_opens_every_file_before_collecting() {
        if Command::new("python3")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            eprintln!("python3 unavailable; skipping LSP batch test");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "eix-lsp-batch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(
            &script,
            r#"
import sys, json

def read():
    headers = {}
    line = sys.stdin.buffer.readline()
    if not line:
        return None
    while line not in (b"\r\n", b"\n"):
        key, value = line.split(b":", 1)
        headers[key.strip().lower()] = value.strip()
        line = sys.stdin.buffer.readline()
    return json.loads(sys.stdin.buffer.read(int(headers[b"content-length"])))

def send(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()

opened = []
while True:
    message = read()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        uri = message["params"]["textDocument"]["uri"]
        opened.append(uri)
        if len(opened) == 2:
            for index, uri in enumerate(opened):
                send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
                    "uri": uri, "diagnostics": [{"range": {"start": {"line": 0, "character": 0}},
                    "severity": 2, "message": "batch %d" % index}]}})
"#,
        )
        .unwrap();
        let first = dir.join("a.rs");
        let second = dir.join("b.rs");
        std::fs::write(&first, "fn a() {}\n").unwrap();
        std::fs::write(&second, "fn b() {}\n").unwrap();
        let mut config = Config::default();
        config.lsp.servers.clear();
        config.lsp.servers.insert(
            "fake".into(),
            LspServerConfig {
                command: "python3".into(),
                args: vec![script.display().to_string()],
                extensions: vec!["rs".into()],
                timeout_secs: 5,
                ..Default::default()
            },
        );

        let mut set = LspSet::new();
        let results = set
            .diagnostics_many(&config, &dir, &[first.clone(), second.clone()])
            .await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, first);
        assert_eq!(results[1].0, second);
        for (_, result) in results {
            let diagnostics = result.expect("batch diagnostics");
            assert_eq!(diagnostics.len(), 1);
            assert!(diagnostics[0].message.starts_with("batch "));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matches_extension_case_insensitively() {
        let mut config = Config::default();
        config.lsp.servers.clear();
        config.lsp.servers.insert(
            "rust".into(),
            LspServerConfig {
                command: "rust-analyzer".into(),
                extensions: vec![".RS".into()],
                ..Default::default()
            },
        );
        config.lsp.servers.get_mut("rust").unwrap().enabled = true;
        assert!(LspSet::handles(&config, Path::new("src/lib.rs")));
        assert!(!LspSet::handles(&config, Path::new("src/lib.py")));
    }
}
