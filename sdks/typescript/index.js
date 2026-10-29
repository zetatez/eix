/**
 * Dependency-free TypeScript/JavaScript SDK for eix server plugins (API v2).
 *
 * The daemon frames JSON-RPC 2.0 as one JSON object per line on stdin/stdout.
 * `Plugin` owns framing, dispatch and host calls; handlers may be sync or async.
 *
 * ```js
 * import { Plugin, text } from "eix-plugin";
 *
 * const plugin = new Plugin({ name: "hello" });
 * plugin.tool("echo", { description: "Echo a string." }, (args) => text(args.text));
 * await plugin.run();
 * ```
 *
 * Host requests use ids in a high range so a request in each direction never
 * shares an id with the daemon's own counter.
 */

import readline from "node:readline";

export const API_VERSION = 2;
export const PROTOCOL_VERSION = 2;

export const METHOD_NOT_FOUND = -32601;
export const INVALID_PARAMS = -32602;
export const INTERNAL = -32603;

const HOST_ID_BASE = 1 << 32;

/** One text content block. */
export function text(value) {
  return { type: "text", text: String(value) };
}

/** One inline base64 image block. */
export function image(mime, blob) {
  return { type: "image", mime, blob };
}

/** A resource reference the host can resolve. */
export function resource(uri, title, mime) {
  const block = { type: "resource", uri };
  if (title !== undefined) block.title = title;
  if (mime !== undefined) block.mime = mime;
  return block;
}

/** A unified diff block. */
export function diff(path, patch) {
  return { type: "diff", path, patch };
}

/** Structured JSON instead of a stringified value. */
export function jsonBlock(value) {
  return { type: "json", value };
}

/** Build a `tools/call` result. A plain string/list from a handler also works. */
export function toolResult(blocks = [], { ok = true, isError = false, structured } = {}) {
  const result = { ok, content: blocks };
  if (isError) result.is_error = true;
  if (structured !== undefined) result.structured = structured;
  return result;
}

class MethodNotFound extends Error {}

/** Per-call context handed to every handler. */
export class Context {
  constructor(plugin, params) {
    this.plugin = plugin;
    this.params = params ?? {};
    this.sessionId = this.params.session_id;
    this.workspace = this.params.workspace ?? this.params.cwd;
  }

  /** Call a daemon host method and return its result. */
  host(method, params = {}) {
    return this.plugin.hostRequest(method, params);
  }

  /** Write a line to the host's redacted plugin log. */
  log(message, level = "info") {
    this.plugin.log(message, level);
  }

  /** Show a transient notification in every attached TUI. */
  notify(message, level = "info") {
    return this.plugin.notify(message, level);
  }

  async storageGet(key, scope = "global") {
    const result = await this.host("host.storage.get", { scope, key });
    return result?.value ?? null;
  }

  storageSet(key, value, scope = "global") {
    return this.host("host.storage.set", { scope, key, value: String(value) });
  }
}

export class Plugin {
  /**
   * @param {{name: string, version?: string, features?: string[], capabilities?: string[]}} options
   */
  constructor({ name, version = "0.1.0", features, capabilities = [] }) {
    this.name = name;
    this.version = version;
    this.features = features ?? ["content_blocks", "cancel", "command_actions", "ui"];
    this.capabilities = capabilities;
    this.tools = new Map();
    this.commands = [];
    this.hooks = [];
    this.events = [];
    this.uiSurfaces = [];
    this.rpcs = [];
    this.toolHandlers = new Map();
    this.commandHandlers = new Map();
    this.hookHandlers = new Map();
    this.eventHandlers = new Map();
    this.uiRenderers = new Map();
    this.uiActions = new Map();
    this.uiPickers = new Map();
    this.rpcHandlers = new Map();
    this.nextHostId = HOST_ID_BASE;
    this.pending = new Map();
    this.closed = false;
    this.reader = null;
  }

  // -- registration -----------------------------------------------------

  /** Register a model tool. The handler receives `(arguments, ctx)`. */
  tool(name, { description = "", inputSchema, readOnly = false } = {}, handler) {
    this.tools.set(name, {
      name,
      description,
      input_schema: inputSchema ?? { type: "object", properties: {} },
      read_only: readOnly,
    });
    this.toolHandlers.set(name, handler);
    return this;
  }

  /**
   * Register a command. With `prompt` it is prompt-only; otherwise `action`
   * (defaulting to the name) makes the host call `commands/run`.
   */
  command(name, { description = "", args = "", action, prompt } = {}, handler) {
    const entry = { name, description, args };
    if (prompt !== undefined) {
      entry.prompt = prompt;
    } else {
      entry.action = action ?? name;
      if (handler) this.commandHandlers.set(entry.action, handler);
    }
    this.commands.push(entry);
    return this;
  }

  /** Register a hook handler. */
  hook(point, handler, { priority = 0, timeoutMs = 0, onError = "allow" } = {}) {
    this.hooks.push({
      point,
      priority,
      timeout_ms: timeoutMs,
      on_error: onError,
    });
    this.hookHandlers.set(point, handler);
    return this;
  }

  /** Subscribe to daemon notifications such as `session.*`. */
  event(pattern) {
    this.events.push(pattern);
    return this;
  }

  /** Handle daemon notifications; supports a trailing `*` wildcard. */
  onEvent(pattern, handler) {
    this.eventHandlers.set(pattern, handler);
    return this;
  }

  /** Declare a UI surface with optional render/action/pick handlers. */
  ui({ id, title, surface = "panel", render, action, pick }) {
    this.uiSurfaces.push({ surface, id, title: title ?? id });
    if (render) this.uiRenderers.set(id, render);
    if (action) this.uiActions.set(id, action);
    if (pick) this.uiPickers.set(id, pick);
    return this;
  }

  /** Expose a plugin RPC method. */
  rpc(name, { description = "" } = {}, handler) {
    this.rpcs.push({ name, description });
    this.rpcHandlers.set(name, handler);
    return this;
  }

  // -- transport --------------------------------------------------------

  write(value) {
    process.stdout.write(JSON.stringify(value) + "\n");
  }

  reply(id, result, error) {
    const message = { jsonrpc: "2.0", id };
    if (error) message.error = error;
    else message.result = result ?? {};
    this.write(message);
  }

  /** Send a request to the daemon and resolve with its result. */
  hostRequest(method, params = {}) {
    const id = this.nextHostId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.write({ jsonrpc: "2.0", id, method, params });
    });
  }

  /** Fire-and-forget host log line. */
  log(message, level = "info") {
    this.write({
      jsonrpc: "2.0",
      method: "host.log",
      params: { level, message: String(message) },
    });
  }

  async notify(message, level = "info") {
    try {
      await this.hostRequest("host.notify", { message: String(message), level });
    } catch {
      // Notifications are best effort.
    }
  }

  // -- dispatch ---------------------------------------------------------

  /** Serve requests until stdin closes or the host sends `shutdown`. */
  run() {
    this.reader = readline.createInterface({
      input: process.stdin,
      crlfDelay: Infinity,
    });
    this.reader.on("line", (line) => this.onLine(line));
    this.reader.on("close", () => {
      this.closed = true;
      for (const { reject } of this.pending.values()) {
        reject(new Error("host closed stdin"));
      }
      this.pending.clear();
    });
    return new Promise((resolve) => this.reader.on("close", resolve));
  }

  onLine(line) {
    line = line.trim();
    if (!line) return;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      process.stderr.write(`eix-plugin[${this.name}]: invalid JSON frame\n`);
      return;
    }
    if (!message || typeof message !== "object") return;
    const method = message.method;
    const id = message.id;
    if (method !== undefined) {
      if (id === undefined || id === null) {
        this.notification(method, message.params ?? {});
      } else {
        void this.request(id, method, message.params ?? {});
      }
      return;
    }
    if (id !== undefined && id !== null) {
      const pending = this.pending.get(id);
      if (!pending) return;
      this.pending.delete(id);
      if (message.error) {
        pending.reject(new Error(message.error.message ?? "host error"));
      } else {
        pending.resolve(message.result);
      }
    }
  }

  async request(id, method, params) {
    try {
      this.reply(id, await this.dispatch(method, params));
    } catch (error) {
      if (error instanceof MethodNotFound) {
        this.reply(id, undefined, { code: METHOD_NOT_FOUND, message: error.message });
      } else {
        process.stderr.write(`eix-plugin[${this.name}]: ${error?.stack ?? error}\n`);
        this.reply(id, undefined, { code: INTERNAL, message: String(error?.message ?? error) });
      }
    }
  }

  async dispatch(method, params) {
    switch (method) {
      case "initialize": {
        if (params.protocol_version !== PROTOCOL_VERSION) {
          throw new Error(`unsupported protocol version ${params.protocol_version}`);
        }
        this.log("initialized");
        return this.describe();
      }
      case "tools/call":
        return this.callTool(params);
      case "commands/run":
        return this.callCommand(params);
      case "hooks/run":
        return this.callHook(params);
      case "rpc.call":
        return this.callRpc(params);
      case "ui/render":
        return this.renderUi(params);
      case "ui/action":
        return this.actUi(params);
      case "ui/pick":
        return this.pickUi(params);
      default:
        throw new MethodNotFound(`unknown method ${JSON.stringify(method)}`);
    }
  }

  notification(method, params) {
    if (method === "shutdown") {
      this.closed = true;
      this.reader?.close();
      if (typeof process.stdin.destroy === "function") process.stdin.destroy();
      return;
    }
    if (method === "$/cancel") return;
    let handler = this.eventHandlers.get(method);
    if (!handler) {
      for (const [pattern, candidate] of this.eventHandlers) {
        if (pattern.endsWith("*") && method.startsWith(pattern.slice(0, -1))) {
          handler = candidate;
          break;
        }
      }
    }
    if (!handler) return;
    Promise.resolve(handler(params, new Context(this, params))).catch((error) => {
      process.stderr.write(`eix-plugin[${this.name}]: event ${method}: ${error}\n`);
    });
  }

  describe() {
    return {
      name: this.name,
      version: this.version,
      api_version: API_VERSION,
      features: this.features,
      tools: [...this.tools.values()],
      commands: this.commands,
      events: this.events,
      hooks: this.hooks,
      ui: this.uiSurfaces,
      rpc: this.rpcs,
    };
  }

  context(params) {
    return new Context(this, params);
  }

  async callTool(params) {
    const name = String(params.tool ?? "");
    const handler = this.toolHandlers.get(name);
    if (!handler) {
      return toolResult([text(`unknown tool '${name}'`)], { ok: false, isError: true });
    }
    return normalizeToolResult(await handler(params.arguments ?? {}, this.context(params)));
  }

  async callCommand(params) {
    const action = String(params.command ?? "");
    const handler = this.commandHandlers.get(action);
    if (!handler) {
      return { ok: false, output: `unknown command action '${action}'` };
    }
    const value = await handler(String(params.arguments ?? ""), this.context(params));
    return typeof value === "string" ? { ok: true, output: value } : value;
  }

  async callHook(params) {
    const point = String(params.point ?? "");
    const handler = this.hookHandlers.get(point);
    if (!handler) throw new MethodNotFound(`unknown hook point '${point}'`);
    return (await handler(params, this.context(params))) ?? {};
  }

  async callRpc(params) {
    const method = String(params.method ?? "");
    const handler = this.rpcHandlers.get(method);
    if (!handler) throw new MethodNotFound(`unknown rpc method '${method}'`);
    return (await handler(params.arguments ?? {}, this.context(params))) ?? {};
  }

  async renderUi(params) {
    const id = String(params.id ?? "");
    const renderer = this.uiRenderers.get(id);
    if (!renderer) {
      const title = this.uiSurfaces.find((entry) => entry.id === id)?.title ?? id;
      return { title, body: "" };
    }
    const value = await renderer(
      { surface: params.surface ?? "panel", id },
      this.context(params),
    );
    return typeof value === "string" ? { title: id, body: value } : value;
  }

  async actUi(params) {
    const handler = this.uiActions.get(String(params.id ?? ""));
    if (!handler) return { ok: true };
    return (await handler(params, this.context(params))) ?? { ok: true };
  }

  async pickUi(params) {
    const handler = this.uiPickers.get(String(params.id ?? ""));
    if (!handler) return { items: [] };
    return { items: (await handler(String(params.query ?? ""), this.context(params))) ?? [] };
  }
}

function normalizeToolResult(value) {
  if (value && typeof value === "object" && !Array.isArray(value)) {
    if (["text", "image", "resource", "diff", "json"].includes(value.type)) {
      return toolResult([value]);
    }
    return value;
  }
  if (typeof value === "string") return toolResult([text(value)]);
  if (Array.isArray(value)) return toolResult(value);
  return toolResult([text(String(value))]);
}

export default Plugin;
