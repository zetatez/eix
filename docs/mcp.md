# MCP (Model Context Protocol)

MCP lets eix plug tools, resources, and prompts served by external processes or remote services
into the model. Each server is a standalone entry: eix connects at startup, lists capabilities, and
calls into them on demand.

Configuration lives under `[mcp.servers.<name>]` (see `config.tmpl.toml`); the implementation is in
`crates/eixd/src/mcp.rs`.

## 1. Two transports

### Local stdio

Set `command` and `args` and eix spawns the program directly (**no shell**), exchanging
newline-delimited JSON-RPC over stdin/stdout. The child dies with the client.

```toml
[mcp.servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
```

Because no shell is involved, pipes, redirections, and `&&` do not work; put everything you need in
`args`.

### Remote Streamable HTTP

Set `url` (mutually exclusive with `command`). eix uses MCP's Streamable HTTP transport: a
request may answer with JSON or with an SSE stream, the session id and protocol version come back
in response headers, and later requests carry them forward.

```toml
[mcp.servers.docs]
url = "https://mcp.example.com/mcp"
timeout_secs = 30
```

## 2. Settings at a glance

| Field | Default | Meaning |
| --- | --- | --- |
| `command` / `args` | empty | Executable and arguments for a local server, run without a shell |
| `env` | empty | Extra environment variables for the child process |
| `url` | none | Remote endpoint; when set, `command` is ignored |
| `headers` | empty | Remote only; values accept `{env:NAME}` / `{file:path}` placeholders |
| `oauth` | none | Remote OAuth client settings, see section 6 |
| `enabled` | `true` | Set to `false` to keep the entry without connecting |
| `timeout_secs` | `30` | Per-call timeout for `tools/list` and `tools/call` |
| `allowed_tools` | empty | Expose only matching tool names; empty means all |
| `denied_tools` | empty | Hide matching tool names, **taking precedence over** `allowed_tools` |

Tool-name filters accept `*` and `?` wildcards. They match the names the server declares, not the
exposed names; `*` crosses characters such as `/`, because tool names are not paths.

```toml
[mcp.servers.docs.headers]
Authorization = "Bearer {env:MCP_TOKEN}"
```

`allowed_tools` and `denied_tools` are arrays that belong to the same server table. Do not open a
sub-table for them:

```toml
[mcp.servers.docs]
url = "https://mcp.example.com/mcp"
allowed_tools = ["search_*", "read_*"]
denied_tools = ["write_*"]
```

## 3. How tools reach the model

The model sees tool names as `mcp__<server>__<tool>`, for example `mcp__files__read`. Any character
other than a letter, digit, `_`, or `-` in the server or tool name is replaced with `_`.

Consequences:

- Renaming a `[mcp.servers.<name>]` key changes the tool names the model sees.
- When matching MCP tools in `[[tools.rules]]`, use the exposed name, for example
  `tool = "mcp__docs__search"`, or the `mcp__*` wildcard.
- Tool output is flattened to text: multiple `content` blocks are concatenated in order, and an
  `isError` result is still returned as text. Unrecognised shapes fall back to raw JSON.

Tools only reach the request after filtering, so `allowed_tools`/`denied_tools` make a tool
genuinely **invisible**, not "visible but rejected".

## 4. Resources and prompts

Besides tools, MCP servers expose resources and prompts. They do not become model tools; instead
five fixed tools read them:

| Tool | Purpose |
| --- | --- |
| `mcp_list_resources` | List resources, optionally filtered by server |
| `mcp_list_resource_templates` | List RFC 6570 resource templates |
| `mcp_read_resource` | Read one URI |
| `mcp_list_prompts` | List prompt templates |
| `mcp_get_prompt` | Fetch a prompt, with arguments |

For a resource template the caller expands `{var}` first, then reads the URI with
`mcp_read_resource`.

## 5. Observability

The `mcp.list` RPC reports per-server state: name, `enabled`, tool count, resource count, prompt
count, and the error text when a connection failed. Both the TUI and the CLI use it to show
health.

A failed connection never takes the session down: that server is skipped, the error stays in the
status, and the remaining servers keep working.

## 6. OAuth

When a remote server needs OAuth, declare `[mcp.servers.<name>.oauth]` and run
`eix mcp auth <name>` to go through the browser flow; `eix mcp logout` clears the credentials.
Tokens are stored under `oauth/mcp` in the data directory.

| Field | Behaviour |
| --- | --- |
| `client_id` | Empty means register the client dynamically |
| `client_secret` | Needed only for confidential clients; omit it for public ones |
| `scopes` | Empty uses the scope from the server's `WWW-Authenticate` header; with neither, no scope parameter is sent |

With a stored token, requests carry the Bearer header automatically, so do not duplicate it in
`headers`.

```toml
[mcp.servers.docs.oauth]
scopes = ["read"]
# client_id = "..." # optional
# client_secret = "..." # optional
```

## 7. Common problems

**A tool does not appear.** Start with the `mcp.list` status: a non-empty `error` means the
connection failed (missing command, unreachable URL, rejected auth). A tool count of 0 means the
connection worked but the server declared no tools, or `allowed_tools`/`denied_tools` filtered
them all out. Filtering is deny-first, so a tool matching both lists is hidden.

**A config change has no effect.** Servers are rebuilt when the config loads or refreshes, so
reopening the session or restarting the daemon reconnects. Children held by the previous client
are killed only when that client is dropped.

**A remote server returns 401.** Confirm `eix mcp auth <name>` succeeded, then check whether
`headers` hand-writes a stale `Authorization`, which overrides or conflicts with the managed one.

**Tool names do not match.** The model sees `mcp__<server>__<tool>`, not the server's raw tool
name. Replace dots and spaces in the config key with `_` before comparing.
