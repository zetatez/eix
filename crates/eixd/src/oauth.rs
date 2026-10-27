//! OAuth 2.1 + PKCE for remote MCP servers.
//!
//! `eix mcp auth <server>` runs discovery, optional dynamic client
//! registration and the loopback browser callback. Tokens are stored in
//! `<data_dir>/oauth/mcp/<server>.json` with owner-only permissions; the
//! daemon only loads and refreshes them when it connects.
//!
//! Discovery follows the MCP authorization spec: the first unauthenticated
//! request yields `WWW-Authenticate: Bearer resource_metadata="..."`, the
//! protected-resource document names the authorization server, and its
//! metadata supplies the endpoints. A pre-registered client can be configured
//! with `mcp.servers.<name>.oauth.client_id`; otherwise RFC 7591 dynamic
//! registration is used.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine;
use eix_core::McpServerConfig;
use reqwest::header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const CLIENT_NAME: &str = "eix";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Owner-only token directory and files.
#[derive(Debug, Clone)]
pub struct Store {
    dir: std::path::PathBuf,
}

/// Options for the interactive authorization flow.
#[derive(Debug, Clone, Copy, Default)]
pub struct AuthOptions {
    /// Print the URL without trying to launch the user's browser.
    pub no_browser: bool,
}

/// The persisted token plus everything needed to refresh it later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Unix seconds. Absent when the server did not send `expires_in`.
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub resource: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

impl Token {
    fn expires_soon(&self) -> bool {
        self.expires_at
            .is_some_and(|at| at <= now_secs().saturating_add(60))
    }
}

impl Store {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Path used for one server's token. Server names are config keys, so
    /// sanitize them before touching the filesystem.
    pub fn token_path(&self, server: &str) -> std::path::PathBuf {
        let safe: String = server
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    pub fn load(&self, server: &str) -> Result<Option<Token>> {
        let path = self.token_path(server);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        let token = serde_json::from_str(&text)
            .with_context(|| format!("invalid OAuth token in {}", path.display()))?;
        Ok(Some(token))
    }

    pub fn save(&self, server: &str, token: &Token) -> Result<()> {
        create_private_dir(&self.dir)?;
        let path = self.token_path(server);
        write_private(&path, serde_json::to_vec_pretty(token)?)
            .with_context(|| format!("cannot save OAuth token to {}", path.display()))
    }

    pub fn remove(&self, server: &str) -> Result<()> {
        match std::fs::remove_file(self.token_path(server)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).context("cannot remove the OAuth token"),
        }
    }

    /// Bearer value for a server, refreshing an expired token when possible.
    /// Errors are reported but never block the connection: the MCP transport
    /// will surface a 401 with an actionable hint.
    pub async fn bearer(&self, server: &str, _config: &McpServerConfig) -> Option<String> {
        let mut token = match self.load(server) {
            Ok(Some(token)) => token,
            Ok(None) => return None,
            Err(err) => {
                eprintln!("eixd: oauth token for {server:?}: {err:#}");
                return None;
            }
        };
        if token.expires_soon() {
            match self.refresh(&mut token).await {
                Ok(()) => {
                    if let Err(err) = self.save(server, &token) {
                        eprintln!("eixd: oauth token for {server:?}: {err:#}");
                    }
                }
                Err(err) => {
                    eprintln!("eixd: cannot refresh the OAuth token for {server:?}: {err:#}")
                }
            }
        }
        Some(format!("Bearer {}", token.access_token))
    }

    async fn refresh(&self, token: &mut Token) -> Result<()> {
        let refresh_token = token
            .refresh_token
            .clone()
            .context("the token has no refresh_token")?;
        if token.token_endpoint.trim().is_empty() {
            bail!("the token has no token_endpoint");
        }
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", token.client_id.as_str()),
        ];
        if let Some(resource) = token.resource.as_deref() {
            form.push(("resource", resource));
        }
        if let Some(secret) = token.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let payload = post_form(
            &token.token_endpoint,
            &form,
            Duration::from_secs(30),
            "refresh the OAuth token",
        )
        .await?;
        apply_token_response(token, &payload)?;
        Ok(())
    }

    /// Run the interactive authorization flow and persist the token.
    pub async fn authorize(
        &self,
        server: &str,
        config: &McpServerConfig,
        options: AuthOptions,
    ) -> Result<Token> {
        let url = config
            .url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .with_context(|| format!("mcp server {server:?} has no url"))?;
        let endpoint = Url::parse(url).context("invalid MCP server url")?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("eix/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .context("cannot build the OAuth HTTP client")?;

        // Bind before registering so the redirect URI is exact.
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("cannot bind the OAuth callback listener")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let challenge = probe_challenge(&client, url).await;
        let resource_doc = discover_resource(&client, &endpoint, challenge.as_deref()).await;
        let issuer = resource_doc
            .as_ref()
            .and_then(|doc| doc.get("authorization_servers"))
            .and_then(Value::as_array)
            .and_then(|servers| servers.first())
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| origin(&endpoint));
        let metadata = discover_authorization_server(&client, &issuer).await?;

        let scope = scope_for(config, challenge.as_deref());
        let mut registered_secret = None;
        let client_id = match config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.client_id.clone())
            .filter(|id| !id.trim().is_empty())
        {
            Some(id) => id,
            None => {
                let (id, secret) =
                    register_client(&client, &metadata, &redirect_uri, scope.as_deref()).await?;
                registered_secret = secret;
                id
            }
        };
        let client_secret = config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.client_secret.clone())
            .or(registered_secret);
        let authorization_endpoint = metadata
            .get("authorization_endpoint")
            .and_then(Value::as_str)
            .context("authorization server metadata has no authorization_endpoint")?;
        let token_endpoint = metadata
            .get("token_endpoint")
            .and_then(Value::as_str)
            .context("authorization server metadata has no token_endpoint")?;

        let verifier = random_verifier();
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        let state = random_state();
        let mut authorize = Url::parse(authorization_endpoint)
            .context("invalid authorization_endpoint in server metadata")?;
        {
            let mut query = authorize.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &client_id);
            query.append_pair("redirect_uri", &redirect_uri);
            query.append_pair("state", &state);
            query.append_pair("code_challenge", &challenge);
            query.append_pair("code_challenge_method", "S256");
            query.append_pair("resource", url);
            if let Some(scope) = scope.as_deref() {
                query.append_pair("scope", scope);
            }
        }

        println!();
        println!("Authorize eix for MCP server {server:?}:");
        println!();
        println!("  {authorize}");
        println!();
        if options.no_browser {
            println!("Open the URL above in a browser to continue.");
        } else {
            println!("Opening it in your browser; otherwise copy the URL above.");
            open_browser(authorize.as_str());
        }

        let code = wait_for_callback(listener, &state, "/callback").await?;
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
            ("resource", url),
        ];
        if let Some(scope) = scope.as_deref() {
            form.push(("scope", scope));
        }
        if let Some(secret) = client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let payload = post_form(
            token_endpoint,
            &form,
            Duration::from_secs(30),
            "exchange the authorization code",
        )
        .await?;
        let mut token = Token {
            access_token: String::new(),
            token_type: default_token_type(),
            expires_at: None,
            refresh_token: None,
            scope: scope.clone(),
            client_id,
            client_secret,
            token_endpoint: token_endpoint.to_string(),
            resource: Some(url.to_string()),
        };
        apply_token_response(&mut token, &payload)?;
        self.save(server, &token)?;
        Ok(token)
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn apply_token_response(token: &mut Token, payload: &Value) -> Result<()> {
    let access = payload
        .get("access_token")
        .and_then(Value::as_str)
        .context("the token response has no access_token")?;
    token.access_token = access.to_string();
    if let Some(kind) = payload.get("token_type").and_then(Value::as_str) {
        token.token_type = kind.to_string();
    }
    if let Some(expires_in) = payload.get("expires_in").and_then(Value::as_u64) {
        token.expires_at = Some(now_secs().saturating_add(expires_in));
    }
    if let Some(refresh) = payload.get("refresh_token").and_then(Value::as_str) {
        token.refresh_token = Some(refresh.to_string());
    }
    if let Some(scope) = payload.get("scope").and_then(Value::as_str) {
        token.scope = Some(scope.to_string());
    }
    Ok(())
}

/// Ask the MCP endpoint for its `WWW-Authenticate` challenge. Any HTTP answer
/// is fine; discovery still has well-known fallbacks.
async fn probe_challenge(client: &reqwest::Client, url: &str) -> Option<String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
        }
    });
    let response = client
        .post(url)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .with_context(|| format!("cannot reach MCP server at {url}"))
        .ok()?;
    response
        .headers()
        .get(WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn header_param(header: &str, key: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let needle = format!("{key}=");
    let start = lower.find(&needle)? + needle.len();
    let rest = &header[start..];
    let rest = rest.trim_start();
    if let Some(rest) = rest.strip_prefix('"') {
        return rest.find('"').map(|end| rest[..end].to_string());
    }
    let end = rest
        .find(|ch: char| ch == ',' || ch.is_ascii_whitespace())
        .unwrap_or(rest.len());
    (!rest[..end].is_empty()).then(|| rest[..end].to_string())
}

async fn discover_resource(
    client: &reqwest::Client,
    endpoint: &Url,
    challenge: Option<&str>,
) -> Option<Value> {
    if let Some(url) = challenge.and_then(|header| header_param(header, "resource_metadata")) {
        if let Some(doc) = get_json(client, &url).await {
            return Some(doc);
        }
    }
    let origin = origin(endpoint);
    let path = endpoint.path().trim_matches('/');
    let mut candidates = vec![format!("{origin}/.well-known/oauth-protected-resource")];
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource/{path}"
        ));
    }
    for candidate in candidates {
        if let Some(doc) = get_json(client, &candidate).await {
            return Some(doc);
        }
    }
    None
}

async fn discover_authorization_server(client: &reqwest::Client, issuer: &str) -> Result<Value> {
    let base = issuer.trim_end_matches('/');
    let parsed = Url::parse(issuer).context("invalid authorization server issuer")?;
    let origin = origin(&parsed);
    let path = parsed.path().trim_matches('/');
    let mut candidates = vec![format!("{base}/.well-known/oauth-authorization-server")];
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-authorization-server/{path}"
        ));
    }
    candidates.push(format!("{base}/.well-known/openid-configuration"));
    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/openid-configuration/{path}"));
    }
    candidates.dedup();

    let mut errors = Vec::new();
    for candidate in candidates {
        match get_json(client, &candidate).await {
            Some(doc) if doc.get("token_endpoint").is_some() => return Ok(doc),
            Some(_) => errors.push(format!("{candidate}: metadata has no token_endpoint")),
            None => errors.push(format!("{candidate}: not available")),
        }
    }
    bail!(
        "cannot discover OAuth metadata for {issuer}: {}",
        errors.join("; ")
    )
}

async fn register_client(
    client: &reqwest::Client,
    metadata: &Value,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<(String, Option<String>)> {
    let endpoint = metadata
        .get("registration_endpoint")
        .and_then(Value::as_str)
        .context(
            "the authorization server has no registration_endpoint; \
             set mcp.servers.*.oauth.client_id for a pre-registered client",
        )?;
    let mut body = json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(scope) = scope {
        body["scope"] = Value::String(scope.to_string());
    }
    let response = client
        .post(endpoint)
        .header(ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .context("dynamic client registration failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "dynamic client registration failed ({status}): {}",
            short(&text, 300)
        );
    }
    let doc: Value =
        serde_json::from_str(&text).context("invalid dynamic client registration response")?;
    let client_id = doc
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("dynamic client registration returned no client_id")?;
    let client_secret = doc
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((client_id, client_secret))
}

fn scope_for(config: &McpServerConfig, challenge: Option<&str>) -> Option<String> {
    let configured = config
        .oauth
        .as_ref()
        .map(|oauth| oauth.scopes.clone())
        .unwrap_or_default();
    if !configured.is_empty() {
        return Some(configured.join(" "));
    }
    if let Some(scope) = challenge.and_then(|header| header_param(header, "scope")) {
        if !scope.trim().is_empty() {
            return Some(scope);
        }
    }
    // Avoid requesting every scope the server supports: it broadens the
    // consent screen without the user asking for it.
    None
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let response = client
        .get(url)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok()
}

pub(crate) async fn post_form(
    url: &str,
    form: &[(&str, &str)],
    timeout: Duration,
    action: &str,
) -> Result<Value> {
    let body = form_body(form);
    let response = reqwest::Client::builder()
        .user_agent(concat!("eix/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .context("cannot build the OAuth HTTP client")?
        .post(url)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .with_context(|| format!("cannot {action}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("cannot {action} ({status}): {}", short(&text, 300));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("invalid response while trying to {action}"))
}

pub(crate) async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    expected_path: &str,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + CALLBACK_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for the OAuth callback");
        }
        let (mut stream, _) = tokio::time::timeout(remaining, listener.accept())
            .await
            .context("timed out waiting for the OAuth callback")?
            .context("OAuth callback listener failed")?;
        let mut buffer = vec![0u8; 16 * 1024];
        let read = match tokio::time::timeout(remaining, stream.read(&mut buffer)).await {
            Ok(Ok(read)) => read,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buffer[..read]);
        let request_line = request.lines().next().unwrap_or_default();
        let target = request_line.split_whitespace().nth(1).unwrap_or_default();
        if !target.starts_with(expected_path) {
            // Browsers may probe favicon.ico or preconnect before the redirect;
            // answer and keep waiting for the real callback.
            let response =
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
            continue;
        }
        let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
        let params = parse_query(query);
        let message = if let Some(error) = params.get("error") {
            let detail = params
                .get("error_description")
                .map(String::as_str)
                .unwrap_or_default();
            format!("authorization failed: {error} {detail}")
        } else if params.get("state").map(String::as_str) != Some(expected_state) {
            "authorization failed: the callback state did not match".to_string()
        } else if let Some(code) = params.get("code") {
            let body = "<!doctype html><meta charset=\"utf-8\"><title>eix</title>\
                        <p>Authorization complete. You can close this tab.</p>";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            return Ok(code.clone());
        } else {
            "authorization failed: the callback has no code".to_string()
        };
        let body = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>eix</title><p>{}</p>",
            html_escape(&message)
        );
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        bail!(message);
    }
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn form_body(form: &[(&str, &str)]) -> String {
    form.iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        decoded.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        decoded.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn origin(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

pub(crate) fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn random_verifier() -> String {
    b64url(&random_bytes())
}

pub(crate) fn random_state() -> String {
    random_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn random_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes
}

pub(crate) fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    let _ = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub(crate) fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot secure {}", dir.display()))?;
    }
    Ok(())
}

pub(crate) fn write_private(path: &std::path::Path, contents: Vec<u8>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies when the file is created; tighten an existing
        // token written by an older eix version.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&contents)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_round_trip() {
        let encoded = form_body(&[("scope", "read write"), ("state", "a+b/c")]);
        assert_eq!(encoded, "scope=read%20write&state=a%2Bb%2Fc");
        let params = parse_query(&encoded);
        assert_eq!(params["scope"], "read write");
        assert_eq!(params["state"], "a+b/c");
    }

    #[test]
    fn www_authenticate_params_support_quotes_and_tokens() {
        let header = r#"Bearer resource_metadata="https://example.com/meta", scope="read write""#;
        assert_eq!(
            header_param(header, "resource_metadata").as_deref(),
            Some("https://example.com/meta")
        );
        assert_eq!(header_param(header, "scope").as_deref(), Some("read write"));
        assert_eq!(header_param("Bearer error=invalid_token", "scope"), None);
    }

    #[test]
    fn token_paths_are_confined_to_the_store() {
        let store = Store::new("/tmp/eix-oauth-test");
        assert_eq!(
            store.token_path("../evil/name").file_name().unwrap(),
            "___evil_name.json"
        );
        assert_eq!(
            store.token_path("docs"),
            std::path::Path::new("/tmp/eix-oauth-test/docs.json")
        );
    }

    #[test]
    fn token_responses_preserve_refresh_tokens() {
        let mut token = Token {
            access_token: "old".into(),
            token_type: "Bearer".into(),
            expires_at: Some(0),
            refresh_token: Some("refresh".into()),
            scope: None,
            client_id: "client".into(),
            client_secret: None,
            token_endpoint: "https://example.com/token".into(),
            resource: None,
        };
        apply_token_response(
            &mut token,
            &json!({"access_token": "new", "expires_in": 3600}),
        )
        .unwrap();
        assert_eq!(token.access_token, "new");
        assert_eq!(token.refresh_token.as_deref(), Some("refresh"));
        assert!(token.expires_at.unwrap() > now_secs());
    }

    #[test]
    fn pkce_verifier_matches_the_s256_budget() {
        let verifier = random_verifier();
        assert!((43..=128).contains(&verifier.len()));
        assert!(verifier
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_".contains(ch)));
        assert_eq!(b64url(&Sha256::digest(verifier.as_bytes())).len(), 43);
    }

    #[tokio::test]
    async fn refresh_posts_the_stored_client_and_updates_the_token() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let read = stream.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            assert!(request.contains("grant_type=refresh_token"), "{request}");
            assert!(request.contains("client_id=client"), "{request}");
            assert!(request.contains("refresh_token=old-refresh"), "{request}");
            let body = json!({
                "access_token": "new-access",
                "expires_in": 3600,
                "refresh_token": "new-refresh"
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let mut token = Token {
            access_token: "old".into(),
            token_type: "Bearer".into(),
            expires_at: Some(0),
            refresh_token: Some("old-refresh".into()),
            scope: None,
            client_id: "client".into(),
            client_secret: None,
            token_endpoint: format!("http://{addr}/token"),
            resource: Some("https://example.com/mcp".into()),
        };
        let store =
            Store::new(std::env::temp_dir().join(format!("eix-oauth-test-{}", Uuid::new_v4())));
        store.refresh(&mut token).await.unwrap();
        assert_eq!(token.access_token, "new-access");
        assert_eq!(token.refresh_token.as_deref(), Some("new-refresh"));
        assert!(token.expires_at.unwrap() > now_secs());

        // Saving also creates an owner-only token file.
        store.save("docs", &token).unwrap();
        let loaded = store.load("docs").unwrap().unwrap();
        assert_eq!(loaded.access_token, "new-access");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.token_path("docs"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(store.dir());
        server.await.unwrap();
    }
}
