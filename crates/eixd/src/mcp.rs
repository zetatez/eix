//! Model Context Protocol client (stdio transport).
//!
//! One JSON-RPC 2.0 message per line over the child's stdin/stdout. Servers are
//! configured under `[mcp.servers.<name>]` and spawned lazily the first time a
//! turn asks for tools. Their tools are exposed to the model as
//! `mcp__<server>__<tool>` and require approval, so a third-party server cannot
//! mutate the workspace silently.
//!
//! Supported methods: `tools/list`, `tools/call`, `resources/list`,
//! `resources/read`, `prompts/list` and `prompts/get`. Change notifications
//! (`notifications/*/list_changed`) mark the cached catalog dirty and are
//! applied on the next operation. Resources and prompts are exposed through
//! read-only `mcp_*` tools rather than becoming one tool per item.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use eix_core::{Config, McpServerConfig, ToolDefinition};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Prefix every MCP tool name shares, so the daemon can recognize and route it.
pub const PREFIX: &str = "mcp__";

pub const LIST_RESOURCES: &str = "mcp_list_resources";
pub const LIST_TEMPLATES: &str = "mcp_list_resource_templates";
pub const READ_RESOURCE: &str = "mcp_read_resource";
pub const LIST_PROMPTS: &str = "mcp_list_prompts";
pub const GET_PROMPT: &str = "mcp_get_prompt";

/// One tool advertised by an MCP server.
#[derive(Debug, Clone)]
pub struct McpTool {
    /// Name the model sees, e.g. `mcp__files__read`.
    pub exposed: String,
    /// Configured server key.
    pub server: String,
    /// Tool name as the server knows it.
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// A resource advertised by an MCP server.
#[derive(Debug, Clone)]
pub struct McpResource {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime: Option<String>,
}

/// A parameterized resource template advertised by an MCP server. Callers
/// expand the RFC 6570 `uriTemplate` and read the result with
/// `mcp_read_resource`.
#[derive(Debug, Clone)]
pub struct McpResourceTemplate {
    pub server: String,
    pub uri_template: String,
    pub name: String,
    pub description: String,
    pub mime: Option<String>,
}

/// A prompt template advertised by an MCP server.
#[derive(Debug, Clone)]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    pub description: String,
    pub arguments: Value,
}

/// A configured server's health, for `mcp.list`.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub enabled: bool,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
    pub error: Option<String>,
}

#[derive(Default)]
struct Flags {
    tools_dirty: bool,
    resources_dirty: bool,
    prompts_dirty: bool,
}

struct Client {
    kind: Kind,
    next_id: u64,
    timeout: Duration,
    flags: Flags,
    resources: Vec<Value>,
    templates: Vec<Value>,
    prompts: Vec<Value>,
}

enum Kind {
    Stdio(StdioClient),
    Http(HttpClient),
}

struct StdioClient {
    /// Kept only to own the child and its pipes; killed on drop.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct HttpClient {
    http: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl StdioClient {
    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn request(
        &mut self,
        method: &str,
        message: &Value,
        id: u64,
        timeout: Duration,
        flags: &mut Flags,
    ) -> Result<Value> {
        self.send(message).await?;

        let mut buf = String::new();
        loop {
            buf.clear();
            let read = tokio::time::timeout(timeout, self.stdout.read_line(&mut buf))
                .await
                .with_context(|| format!("mcp {method} timed out"))??;
            if read == 0 {
                bail!("mcp server closed the connection");
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                continue; // servers may log to stdout despite the spec
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                observe(&value, flags);
                continue; // a notification or a response to an earlier request
            }
            return Ok(value);
        }
    }
}

impl HttpClient {
    async fn post(&mut self, message: &Value, timeout: Duration) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .post(&self.url)
            .timeout(timeout)
            .header(ACCEPT, "application/json, text/event-stream")
            .json(message);
        if let Some(session) = self.session_id.as_deref() {
            request = request.header("mcp-session-id", session);
        }
        if let Some(version) = self.protocol_version.as_deref() {
            request = request.header("mcp-protocol-version", version);
        }
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("cannot reach mcp server at {}", self.url))?;
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session.to_string());
        }

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!(
                "mcp server at {} requires authentication; configure \
                 mcp.servers.*.headers or run the OAuth flow",
                self.url
            );
        }
        if status == reqwest::StatusCode::NOT_FOUND
            || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            bail!(
                "mcp server at {} rejected the Streamable HTTP request ({status})",
                self.url
            );
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("mcp http {status}: {}", redact(&short(&body, 300)));
        }
        Ok(response)
    }

    async fn request(
        &mut self,
        method: &str,
        message: &Value,
        id: u64,
        timeout: Duration,
        flags: &mut Flags,
    ) -> Result<Value> {
        let response = self.post(message, timeout).await?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let body = response.text().await?;
        if body.trim().is_empty() {
            bail!("mcp {method} returned an empty response");
        }

        let values = if content_type.contains("text/event-stream") {
            parse_sse(&body)
        } else if content_type.contains("application/json")
            || body.trim_start().starts_with('{')
            || body.trim_start().starts_with('[')
        {
            match serde_json::from_str::<Value>(&body) {
                Ok(Value::Array(items)) => items,
                Ok(value) => vec![value],
                Err(err) if content_type.contains("text/event-stream") => {
                    let _ = err;
                    parse_sse(&body)
                }
                Err(err) => return Err(err.into()),
            }
        } else {
            // Some gateways omit the content type; prefer a JSON body and fall
            // back to SSE rather than dropping a valid response.
            match serde_json::from_str::<Value>(&body) {
                Ok(Value::Array(items)) => items,
                Ok(value) => vec![value],
                Err(_) => parse_sse(&body),
            }
        };

        for value in values {
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(value);
            }
            observe(&value, flags);
        }
        bail!("mcp {method} did not return a response for id {id}")
    }
}

impl Client {
    async fn send(&mut self, message: Value) -> Result<()> {
        match &mut self.kind {
            Kind::Stdio(stdio) => stdio.send(&message).await,
            Kind::Http(http) => {
                http.post(&message, self.timeout).await?;
                Ok(())
            }
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let response = match &mut self.kind {
            Kind::Stdio(stdio) => {
                stdio
                    .request(method, &message, id, self.timeout, &mut self.flags)
                    .await?
            }
            Kind::Http(http) => {
                http.request(method, &message, id, self.timeout, &mut self.flags)
                    .await?
            }
        };
        if let Some(error) = response.get("error") {
            bail!("mcp {method} failed: {error}");
        }
        response
            .get("result")
            .cloned()
            .context("mcp response has no result")
    }

    async fn initialize(&mut self) -> Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "eix", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        if let Kind::Http(http) = &mut self.kind {
            if let Some(version) = result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|version| !version.is_empty())
            {
                http.protocol_version = Some(version.to_string());
            }
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }))
        .await
    }

    async fn list_tools(&mut self) -> Result<Vec<Value>> {
        self.list_paged("tools/list", "tools").await
    }

    async fn list_resources(&mut self) -> Result<Vec<Value>> {
        self.list_paged("resources/list", "resources").await
    }

    async fn list_resource_templates(&mut self) -> Result<Vec<Value>> {
        self.list_paged("resources/templates/list", "resourceTemplates")
            .await
    }

    async fn list_prompts(&mut self) -> Result<Vec<Value>> {
        self.list_paged("prompts/list", "prompts").await
    }

    async fn list_paged(&mut self, method: &str, field: &str) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        // The spec allows cursors; bound the loop to protect against a broken
        // server that keeps returning the same cursor forever.
        for _ in 0..32 {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({ "cursor": cursor }))
                .unwrap_or_else(|| json!({}));
            let result = self.request(method, params).await?;
            if let Some(page) = result.get(field).and_then(Value::as_array) {
                items.extend(page.iter().cloned());
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|cursor| !cursor.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        Ok(items)
    }

    async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    async fn read_resource(&mut self, uri: &str) -> Result<Value> {
        self.request("resources/read", json!({ "uri": uri })).await
    }

    async fn get_prompt(&mut self, name: &str, arguments: &Value) -> Result<Value> {
        self.request(
            "prompts/get",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

/// Lazily-started MCP servers and the catalog they advertise.
pub struct McpSet {
    clients: BTreeMap<String, Client>,
    tools: Vec<McpTool>,
    resources: Vec<McpResource>,
    templates: Vec<McpResourceTemplate>,
    prompts: Vec<McpPrompt>,
    failures: BTreeMap<String, String>,
    loaded: bool,
    /// OAuth tokens shared with `eix mcp auth`.
    oauth: crate::oauth::Store,
}

impl McpSet {
    pub fn new(oauth_dir: impl Into<PathBuf>) -> Self {
        Self {
            clients: BTreeMap::new(),
            tools: Vec::new(),
            resources: Vec::new(),
            templates: Vec::new(),
            prompts: Vec::new(),
            failures: BTreeMap::new(),
            loaded: false,
            oauth: crate::oauth::Store::new(oauth_dir),
        }
    }

    /// Drop cached clients so the next turn re-reads the config and respawns
    /// servers. Called on hot reload and after `eix mcp auth`.
    pub fn clear(&mut self) {
        let oauth_dir = self.oauth.dir().to_path_buf();
        *self = Self::new(oauth_dir);
    }

    /// Spawn and handshake every enabled server once. Safe to call repeatedly.
    pub async fn ensure_loaded(&mut self, config: &Config) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if config.mcp.servers.is_empty() {
            return;
        }
        for (name, server) in &config.mcp.servers {
            if !server.enabled {
                continue;
            }
            match self.start(name, server).await {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("eixd: mcp server {name:?}: {}", redact(&format!("{err:#}")));
                    self.failures.insert(name.clone(), format!("{err:#}"));
                }
            }
        }
    }

    async fn start(&mut self, name: &str, server: &McpServerConfig) -> Result<()> {
        let mut client = connect(name, server, &self.oauth).await?;
        client.initialize().await?;
        let raw_tools = client.list_tools().await?;
        let resources = client.list_resources().await.unwrap_or_default();
        let templates = client.list_resource_templates().await.unwrap_or_default();
        let prompts = client.list_prompts().await.unwrap_or_default();
        client.resources = resources;
        client.templates = templates;
        client.prompts = prompts;
        self.clients.insert(name.to_string(), client);
        self.tools.retain(|tool| tool.server != name);
        for tool in raw_tools {
            if let Some(parsed) = parse_tool(name, server, &tool) {
                self.tools.push(parsed);
            }
        }
        self.rebuild_meta(name);
        Ok(())
    }

    /// Re-list catalogs for servers that sent a `list_changed` notification.
    /// Notifications are only observed while another request is in flight, so
    /// this is intentionally called at the start of each turn and on `mcp.list`.
    pub async fn refresh(&mut self, config: &Config) {
        let names: Vec<String> = self.clients.keys().cloned().collect();
        for name in names {
            let Some(server) = config.mcp.servers.get(&name) else {
                continue;
            };
            // A later successful refresh should clear a stale startup/listing
            // error shown by `mcp.list`.
            self.failures.remove(&name);
            let Some(client) = self.clients.get_mut(&name) else {
                continue;
            };
            if client.flags.tools_dirty {
                client.flags.tools_dirty = false;
                match client.list_tools().await {
                    Ok(tools) => {
                        self.tools.retain(|tool| tool.server != name);
                        for tool in tools {
                            if let Some(parsed) = parse_tool(&name, server, &tool) {
                                self.tools.push(parsed);
                            }
                        }
                    }
                    Err(err) => {
                        client.flags.tools_dirty = true;
                        self.failures.insert(name.clone(), format!("{err:#}"));
                    }
                }
            }
            if client.flags.resources_dirty {
                client.flags.resources_dirty = false;
                match client.list_resources().await {
                    Ok(resources) => client.resources = resources,
                    Err(err) => {
                        client.flags.resources_dirty = true;
                        self.failures.insert(name.clone(), format!("{err:#}"));
                    }
                }
                match client.list_resource_templates().await {
                    Ok(templates) => client.templates = templates,
                    Err(err) => {
                        client.flags.resources_dirty = true;
                        self.failures.insert(name.clone(), format!("{err:#}"));
                    }
                }
            }
            if client.flags.prompts_dirty {
                client.flags.prompts_dirty = false;
                match client.list_prompts().await {
                    Ok(prompts) => client.prompts = prompts,
                    Err(err) => {
                        client.flags.prompts_dirty = true;
                        self.failures.insert(name.clone(), format!("{err:#}"));
                    }
                }
            }
            self.rebuild_meta(&name);
        }
    }

    fn rebuild_meta(&mut self, name: &str) {
        let Some(client) = self.clients.get(name) else {
            return;
        };
        let resources = client.resources.clone();
        let templates = client.templates.clone();
        let prompts = client.prompts.clone();
        self.resources.retain(|resource| resource.server != name);
        for resource in resources {
            let Some(uri) = resource.get("uri").and_then(Value::as_str) else {
                continue;
            };
            self.resources.push(McpResource {
                server: name.to_string(),
                uri: uri.to_string(),
                name: resource
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(uri)
                    .to_string(),
                description: resource
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                mime: resource
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        self.templates.retain(|template| template.server != name);
        for template in templates {
            let Some(uri_template) = template.get("uriTemplate").and_then(Value::as_str) else {
                continue;
            };
            self.templates.push(McpResourceTemplate {
                server: name.to_string(),
                uri_template: uri_template.to_string(),
                name: template
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(uri_template)
                    .to_string(),
                description: template
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                mime: template
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        self.prompts.retain(|prompt| prompt.server != name);
        for prompt in prompts {
            let Some(prompt_name) = prompt.get("name").and_then(Value::as_str) else {
                continue;
            };
            self.prompts.push(McpPrompt {
                server: name.to_string(),
                name: prompt_name.to_string(),
                description: prompt
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: prompt
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            });
        }
    }

    /// Definitions to append to the provider's tool list.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| {
                ToolDefinition::function(&tool.exposed, &tool.description, tool.schema.clone())
            })
            .collect()
    }

    pub fn knows(&self, exposed: &str) -> bool {
        self.tools.iter().any(|tool| tool.exposed == exposed)
    }

    /// Call an advertised tool and flatten its content to text.
    pub async fn call(&mut self, exposed: &str, args: &Value) -> Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.exposed == exposed)
            .cloned()
            .with_context(|| format!("unknown MCP tool {exposed:?}"))?;
        let Some(client) = self.clients.get_mut(&tool.server) else {
            bail!("MCP server {:?} is not running", tool.server);
        };
        let result = client.call_tool(&tool.name, args).await?;
        Ok(flatten_content(&result))
    }

    pub fn list_resources(&self, server: Option<&str>) -> Result<String> {
        let items: Vec<&McpResource> = self
            .resources
            .iter()
            .filter(|resource| server.is_none_or(|server| resource.server == server))
            .collect();
        if items.is_empty() {
            return Ok(match server {
                Some(server) => format!("server {server:?} exposes no resources"),
                None => "no MCP resources".to_string(),
            });
        }
        let mut out = String::new();
        for resource in items {
            out.push_str(&format!(
                "{}: {}\n  uri: {}",
                resource.server, resource.name, resource.uri
            ));
            if let Some(mime) = &resource.mime {
                out.push_str(&format!("\n  mime: {mime}"));
            }
            if !resource.description.is_empty() {
                out.push_str(&format!("\n  {}", resource.description));
            }
            out.push('\n');
        }
        Ok(out)
    }

    pub fn list_resource_templates(&self, server: Option<&str>) -> Result<String> {
        let items: Vec<&McpResourceTemplate> = self
            .templates
            .iter()
            .filter(|template| server.is_none_or(|server| template.server == server))
            .collect();
        if items.is_empty() {
            return Ok(match server {
                Some(server) => format!("server {server:?} exposes no resource templates"),
                None => "no MCP resource templates".to_string(),
            });
        }
        let mut out = String::new();
        for template in items {
            out.push_str(&format!(
                "{}: {}\n  uriTemplate: {}",
                template.server, template.name, template.uri_template
            ));
            if let Some(mime) = &template.mime {
                out.push_str(&format!("\n  mime: {mime}"));
            }
            if !template.description.is_empty() {
                out.push_str(&format!("\n  {}", template.description));
            }
            out.push('\n');
        }
        Ok(out)
    }

    pub async fn read_resource(&mut self, server: &str, uri: &str) -> Result<String> {
        let Some(client) = self.clients.get_mut(server) else {
            bail!("MCP server {server:?} is not running");
        };
        let result = client.read_resource(uri).await?;
        Ok(flatten_resource(&result))
    }

    pub fn list_prompts(&self, server: Option<&str>) -> Result<String> {
        let items: Vec<&McpPrompt> = self
            .prompts
            .iter()
            .filter(|prompt| server.is_none_or(|server| prompt.server == server))
            .collect();
        if items.is_empty() {
            return Ok(match server {
                Some(server) => format!("server {server:?} exposes no prompts"),
                None => "no MCP prompts".to_string(),
            });
        }
        let mut out = String::new();
        for prompt in items {
            out.push_str(&format!("{}: {}", prompt.server, prompt.name));
            if !prompt.description.is_empty() {
                out.push_str(&format!(" — {}", prompt.description));
            }
            if let Some(arguments) = prompt.arguments.as_array() {
                if !arguments.is_empty() {
                    out.push_str("\n  arguments: ");
                    out.push_str(
                        &arguments
                            .iter()
                            .filter_map(|argument| argument.get("name").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                }
            }
            out.push('\n');
        }
        Ok(out)
    }

    pub async fn get_prompt(
        &mut self,
        server: &str,
        name: &str,
        arguments: &Value,
    ) -> Result<String> {
        let Some(client) = self.clients.get_mut(server) else {
            bail!("MCP server {server:?} is not running");
        };
        let result = client.get_prompt(name, arguments).await?;
        Ok(flatten_prompt(&result))
    }

    pub fn status(&self) -> Vec<McpServerStatus> {
        self.clients
            .keys()
            .cloned()
            .chain(self.failures.keys().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|name| McpServerStatus {
                tools: self.tools.iter().filter(|tool| tool.server == name).count(),
                resources: self
                    .resources
                    .iter()
                    .filter(|resource| resource.server == name)
                    .count(),
                prompts: self
                    .prompts
                    .iter()
                    .filter(|prompt| prompt.server == name)
                    .count(),
                error: self.failures.get(&name).cloned(),
                enabled: true,
                name,
            })
            .collect()
    }
}

fn parse_tool(server_name: &str, server: &McpServerConfig, tool: &Value) -> Option<McpTool> {
    let tool_name = tool.get("name").and_then(Value::as_str)?;
    if !server.allows_tool(tool_name) {
        return None;
    }
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("MCP tool {tool_name} from {server_name}"));
    let schema = tool
        .get("inputSchema")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    Some(McpTool {
        exposed: exposed_name(server_name, tool_name),
        server: server_name.to_string(),
        name: tool_name.to_string(),
        description: if description.starts_with("MCP") {
            description
        } else {
            format!("{description} (MCP: {server_name})")
        },
        schema,
    })
}

/// One-line summary for the approval card.
pub fn summarize(exposed: &str, args: &Value) -> String {
    let pretty = serde_json::to_string(args).unwrap_or_default();
    let args = if pretty.len() > 120 {
        format!("{}…", &pretty[..120.min(pretty.len())])
    } else {
        pretty
    };
    format!("{exposed} {args}")
}

async fn connect(
    name: &str,
    server: &McpServerConfig,
    oauth: &crate::oauth::Store,
) -> Result<Client> {
    let kind = if server.is_remote() {
        let mut headers = server.headers.clone();
        if !headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"))
        {
            if let Some(value) = oauth.bearer(name, server).await {
                headers.insert("Authorization".to_string(), value);
            }
        }
        Kind::Http(HttpClient {
            http: reqwest::Client::builder()
                .user_agent(concat!("eix/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("cannot build the MCP HTTP client")?,
            url: server.url.as_deref().unwrap_or_default().trim().to_string(),
            headers,
            session_id: None,
            protocol_version: None,
        })
    } else {
        Kind::Stdio(spawn_stdio(name, server)?)
    };
    Ok(Client {
        kind,
        next_id: 1,
        timeout: Duration::from_secs(server.timeout_secs.max(1)),
        flags: Flags::default(),
        resources: Vec::new(),
        templates: Vec::new(),
        prompts: Vec::new(),
    })
}

fn spawn_stdio(name: &str, server: &McpServerConfig) -> Result<StdioClient> {
    let mut command = Command::new(server.command.trim());
    command
        .args(&server.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in &server.env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start mcp server {name:?}"))?;
    let stdin = child.stdin.take().context("mcp server has no stdin pipe")?;
    let stdout = child
        .stdout
        .take()
        .context("mcp server has no stdout pipe")?;
    Ok(StdioClient {
        _child: child,
        stdin,
        stdout: BufReader::new(stdout),
    })
}

fn exposed_name(server: &str, tool: &str) -> String {
    format!("{PREFIX}{}__{}", sanitize(server), sanitize(tool))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// MCP responses carry `content: [{type: "text", text: ...}]`. Join the text
/// parts and fall back to the raw JSON when a server sends something exotic.
fn flatten_content(result: &Value) -> String {
    let failed = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        let mut parts = Vec::new();
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
        }
        if !parts.is_empty() {
            let text = parts.join("\n");
            return if failed {
                format!("error: {text}")
            } else {
                text
            };
        }
    }
    if failed {
        return format!("error: {result}");
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

fn flatten_resource(result: &Value) -> String {
    let Some(contents) = result.get("contents").and_then(Value::as_array) else {
        return serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
    };
    let mut parts = Vec::new();
    for item in contents {
        let uri = item
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or("resource");
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            parts.push(format!("{uri}:\n{text}"));
        } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
            let mime = item
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("binary");
            parts.push(format!(
                "{uri}: ({mime}, {} base64 bytes; binary resources are not expanded)",
                blob.len()
            ));
        }
    }
    if parts.is_empty() {
        serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
    } else {
        parts.join("\n\n")
    }
}

fn flatten_prompt(result: &Value) -> String {
    let mut out = String::new();
    if let Some(description) = result.get("description").and_then(Value::as_str) {
        if !description.is_empty() {
            out.push_str(description);
            out.push_str("\n\n");
        }
    }
    if let Some(messages) = result.get("messages").and_then(Value::as_array) {
        for message in messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let text = message
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
                .or_else(|| message.get("content").and_then(Value::as_str))
                .unwrap_or("");
            out.push_str(&format!("[{role}]\n{text}\n\n"));
        }
    }
    if out.trim().is_empty() {
        serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
    } else {
        out.trim_end().to_string()
    }
}

fn observe(value: &Value, flags: &mut Flags) {
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return;
    };
    match method {
        "notifications/tools/list_changed" => flags.tools_dirty = true,
        "notifications/resources/list_changed" => flags.resources_dirty = true,
        "notifications/prompts/list_changed" => flags.prompts_dirty = true,
        _ => {}
    }
}

/// Parse the JSON-RPC payloads in a `text/event-stream` body. Comments,
/// `event:` and `id:` lines are ignored; data lines of one event are joined
/// with newlines as the SSE spec requires.
fn parse_sse(body: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut data = String::new();
    let normalized = body.replace("\r\n", "\n");
    for line in normalized.split('\n') {
        if line.is_empty() {
            if !data.is_empty() {
                if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
                    events.push(value);
                }
                data.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if !data.is_empty() {
        if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
            events.push(value);
        }
    }
    events
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Strip credentials from an error before it reaches a log.
fn redact(message: &str) -> String {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)(api[_-]?key|token|authorization|bearer)([=:\s]+)\S+")
            .expect("valid redaction regex")
    });
    regex.replace_all(message, "$1$2***").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn remote_server_uses_a_stored_oauth_bearer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((head, body)) = read_request(&mut stream).await else {
                    return;
                };
                if step == 0 {
                    assert!(
                        head.lines()
                            .any(|line| line
                                .eq_ignore_ascii_case("authorization: Bearer stored-token")),
                        "{head}"
                    );
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let response = json_response(
                        &json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {},
                                "serverInfo": {"name": "mock", "version": "1"}
                            }
                        }),
                        None,
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                } else {
                    // notifications/initialized
                    let response =
                        "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            }
        });

        let dir = std::env::temp_dir().join(format!("eix-mcp-oauth-{}", uuid::Uuid::new_v4()));
        let store = crate::oauth::Store::new(&dir);
        store
            .save(
                "mock",
                &crate::oauth::Token {
                    access_token: "stored-token".into(),
                    token_type: "Bearer".into(),
                    expires_at: None,
                    refresh_token: None,
                    scope: None,
                    client_id: "client".into(),
                    client_secret: None,
                    token_endpoint: String::new(),
                    resource: None,
                },
            )
            .unwrap();
        let config = McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            ..McpServerConfig::default()
        };
        let mut client = connect("mock", &config, &store).await.unwrap();
        client.initialize().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        server.await.unwrap();
    }

    #[test]
    fn sse_parser_handles_multiline_data_and_comments() {
        let body = ": keep-alive\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{\"ok\":true}}\n\n";
        let events = parse_sse(body);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["result"]["ok"], true);
        assert_eq!(parse_sse("data: not-json\n\n").len(), 0);
    }

    #[tokio::test]
    async fn remote_server_uses_streamable_http_session_and_sse() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let server = tokio::spawn(serve_mock(listener, Arc::clone(&seen)));

        let config = McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            ..McpServerConfig::default()
        };
        let mut client = connect(
            "mock",
            &config,
            &crate::oauth::Store::new("/tmp/eix-oauth-test"),
        )
        .await
        .unwrap();
        client.initialize().await.unwrap();

        // The mock answers tools/list as SSE and prepends a list_changed
        // notification in the same stream.
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools[0]["name"], "echo");
        assert!(
            client.flags.tools_dirty,
            "list_changed notification was not observed"
        );

        let result = client
            .call_tool("echo", &json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(flatten_content(&result), "hello");

        // The session id returned by initialize must be echoed on later calls.
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|entry| entry == "tools/list:mock-session"),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|entry| entry == "tools/call:mock-session"),
            "{seen:?}"
        );

        server.abort();
    }

    async fn serve_mock(listener: TcpListener, seen: Arc<StdMutex<Vec<String>>>) {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut stream).await else {
                    return;
                };
                let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = request
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let id = request.get("id").cloned();
                if let Some(session) = head.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("mcp-session-id")
                        .then(|| value.trim().to_string())
                }) {
                    seen.lock().unwrap().push(format!("{method}:{session}"));
                }

                let response = match method {
                    "initialize" => json_response(
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {},
                                "serverInfo": {"name": "mock", "version": "1"}
                            }
                        }),
                        Some("mock-session"),
                    ),
                    "notifications/initialized" => {
                        "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_string()
                    }
                    "tools/list" => sse_response(&[
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/tools/list_changed"
                        }),
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"tools": [{
                                "name": "echo",
                                "description": "Echo text",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"text": {"type": "string"}}
                                }
                            }]}
                        }),
                    ]),
                    "tools/call" => json_response(
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"content": [{"type": "text", "text": "hello"}]}
                        }),
                        None,
                    ),
                    _ => json_response(
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32601, "message": "unknown method"}
                        }),
                        None,
                    ),
                };
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }

    async fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buf).await.ok()?;
            if read == 0 {
                return None;
            }
            raw.extend_from_slice(&buf[..read]);
            if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            if raw.len() > 64 * 1024 {
                return None;
            }
        };
        let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while raw.len() < header_end + length {
            let read = stream.read(&mut buf).await.ok()?;
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..read]);
        }
        Some((
            head,
            raw.get(header_end..header_end + length)
                .unwrap_or_default()
                .to_vec(),
        ))
    }

    fn json_response(body: &Value, session: Option<&str>) -> String {
        let body = serde_json::to_string(body).unwrap();
        let session = session
            .map(|session| format!("mcp-session-id: {session}\r\n"))
            .unwrap_or_default();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{session}connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn sse_response(events: &[Value]) -> String {
        let mut body = String::new();
        for event in events {
            body.push_str("event: message\ndata: ");
            body.push_str(&serde_json::to_string(event).unwrap());
            body.push_str("\n\n");
        }
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn exposed_names_are_sanitized() {
        assert_eq!(
            exposed_name("my.files", "exec/command"),
            "mcp__my_files__exec_command"
        );
        assert_eq!(sanitize("a b"), "a_b");
        assert_eq!(sanitize("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn content_is_flattened() {
        let result = json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ]
        });
        assert_eq!(flatten_content(&result), "line one\nline two");

        let failed = json!({
            "isError": true,
            "content": [{"type": "text", "text": "boom"}]
        });
        assert_eq!(flatten_content(&failed), "error: boom");
    }

    #[test]
    fn tool_filters_support_wildcards_and_deny_wins() {
        let config: McpServerConfig = serde_json::from_value(json!({
            "allowed_tools": ["read_*", "search"],
            "denied_tools": ["read_secret"]
        }))
        .unwrap();
        assert!(config.allows_tool("read_rows"));
        assert!(config.allows_tool("search"));
        assert!(!config.allows_tool("read_secret"));
        assert!(!config.allows_tool("write_rows"));
    }

    #[test]
    fn resources_and_prompts_flatten() {
        let resource = json!({"contents":[{"uri":"file:///a","text":"hello"}]});
        assert!(flatten_resource(&resource).contains("hello"));
        let prompt = json!({
            "description": "review",
            "messages": [{"role":"user","content":{"type":"text","text":"do it"}}]
        });
        assert!(flatten_prompt(&prompt).contains("do it"));
    }
}
