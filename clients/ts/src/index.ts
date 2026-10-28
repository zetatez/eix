/**
 * Typed client for the eix daemon HTTP API.
 *
 * The daemon exposes a small HTTP surface (`/health`, `/openapi.json`,
 * `/pair`, `/stats`, `POST /rpc` and the `/events` SSE stream). This module is
 * dependency-free: it uses the global `fetch` and parses SSE frames itself, so
 * it runs in Node 20+, Bun, Deno, and browsers.
 *
 * ```ts
 * const eix = new EixClient({ baseUrl: "http://127.0.0.1:7778", token });
 * const session = await eix.createSession({ name: "web", cwd: "/repo" });
 * for await (const note of eix.promptStream(session.id, "fix the tests")) {
 *   if (note.method === "message.delta") process.stdout.write(note.params.delta);
 * }
 * ```
 */

export type SessionId = string;
export type SessionMode = "plan" | "build" | "auto" | "full";
export type SessionOrigin = "user" | "fork" | "agent";
export type SessionStatus =
  | "idle"
  | "running"
  | "awaiting_approval"
  | "awaiting_input"
  | "error"
  | "paused";

export type ApprovalDecision = "approve" | "deny" | "always";
/** Where an `always` decision is remembered. */
export type ApprovalScope = "once" | "session" | "project" | "user";

/** Session event kinds, matching `eix_core::EventKind`. */
export type EventKind =
  | "user_message"
  | "assistant_message"
  | "tool_call"
  | "tool_result"
  | "patch_proposed"
  | "patch_applied"
  | "patch_rejected"
  | "patch_conflict"
  | "approval_required"
  | "input_required"
  | "input_answered"
  | "snapshot"
  | "process_started"
  | "process_exited"
  | "status_changed"
  | "context_compacted"
  | "context_pinned"
  | "context_dropped"
  | "context_cleared"
  | "mode"
  | "permission_evaluated"
  | "error"
  | "integration";

export interface Session {
  id: SessionId;
  name: string;
  cwd: string;
  status: SessionStatus;
  model: string | null;
  parent_id: string | null;
  forked_from_seq: number | null;
  worktree: string | null;
  origin: SessionOrigin;
  mode: SessionMode;
  agent: string | null;
  archived: boolean;
  created_at: number;
  updated_at: number;
}

/** One persisted session event. `payload` depends on `kind`. */
export interface Event<P = Record<string, unknown>> {
  session_id: SessionId;
  seq: number;
  ts: number;
  kind: EventKind;
  payload: P;
}

/** A daemon notification, e.g. `message.delta` or `tool.approval_required`. */
export interface Notification<P = Record<string, unknown>> {
  method: string;
  params: P;
}

export interface Health {
  ok: boolean;
  version: string;
}

export interface Stats {
  version: string;
  uptime_secs: number;
  sessions: number;
  archived_sessions: number;
  running_sessions: number;
  events: number;
  db_bytes: number;
  blob_count: number;
  blob_bytes: number;
  skills: number;
  mcp_servers: number;
  default_model: string;
}

/** A durable client credential minted by pairing. Never includes the token. */
export interface ClientToken {
  id: string;
  name: string;
  created_at: number;
  last_used_at: number | null;
  revoked: boolean;
}

/** Result of `POST /pair`, or of `pair()` on this client. */
export interface PairResult {
  /** Show once: the daemon stores only its hash. */
  token: string;
  client: ClientToken;
}

export interface CreateSessionParams {
  name?: string;
  cwd?: string;
  model?: string;
  worktree?: boolean;
  mode?: SessionMode;
  agent?: string;
}

export interface SessionResult {
  session: Session;
}

export interface SessionList {
  sessions: Session[];
}

export interface AttachResult {
  session: Session;
  events: Event[];
  has_more: boolean;
}

export interface SubmitResult {
  session: Session;
  turn_id: string;
}

export interface CancelResult {
  cancelled: boolean;
}

export interface ApprovalInfo {
  session_id: SessionId;
  session_name: string;
  approval_id: string;
  /** `tool` for a blocking tool call, `patch` for a staged write. */
  kind: string;
  tool: string;
  summary: string;
  created_ms: number;
}

export interface ApprovalListResult {
  approvals: ApprovalInfo[];
}

export interface ApprovalResult {
  accepted: boolean;
}

export interface InputResult {
  accepted: boolean;
}

/** Payload of `tool.approval_required`. */
export interface ApprovalRequiredParams {
  session_id: SessionId;
  turn_id: string;
  approval_id: string;
  call_id: string;
  tool: string;
  summary: string;
  arguments: Record<string, unknown>;
  diff?: string | null;
  hunks?: Array<{ index: number; header: string; lines: string[] }>;
  risk?: string;
  risk_categories?: string[];
  reasons?: string[];
  rule?: string | null;
  rule_source?: string;
  sandbox?: Record<string, unknown>;
  parsed_command?: unknown;
  allow_always?: boolean;
}

/** Payload of `input.required` (the model asked the user a question). */
export interface InputRequiredParams {
  session_id: SessionId;
  turn_id: string;
  input_id: string;
  call_id: string;
  question: string;
  options: Array<{ label: string; description?: string }>;
}

/** Payload of `message.delta` / `reasoning.delta`. */
export interface DeltaParams {
  session_id: SessionId;
  turn_id: string;
  delta: string;
}

/** Payload of `turn.completed`. */
export interface TurnCompletedParams {
  session_id: SessionId;
  turn_id: string;
  cancelled: boolean;
  error: { code: string; message: string } | null;
  usage?: Record<string, number>;
  cost?: number | null;
  duration_ms?: number;
}

export interface EventOptions {
  /** Only deliver these sessions; global notifications are always delivered. */
  sessionId?: SessionId | SessionId[];
  /** Deliver every notification (no session filter). */
  all?: boolean;
  signal?: AbortSignal;
}

export interface ApprovalOptions {
  reason?: string;
  /** Hunk indices to apply for a staged write; omitted applies all of them. */
  hunks?: number[];
}

export interface ResponseStreamOptions {
  signal?: AbortSignal;
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

export interface EixClientOptions {
  /** Base HTTP URL, e.g. `http://127.0.0.1:7778`. A path prefix is allowed. */
  baseUrl: string;
  /** `server.token` or a token returned by pairing. */
  token?: string;
  /** Override the global `fetch` (tests, proxies, custom agents). */
  fetch?: FetchLike;
  /** Extra headers sent with every request. */
  headers?: Record<string, string>;
}

/** An HTTP or JSON-RPC error returned by the daemon. */
export class EixError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(message: string, status: number, code = "http_error") {
    super(message);
    this.name = "EixError";
    this.status = status;
    this.code = code;
  }
}

interface RpcEnvelope {
  id?: number;
  result?: unknown;
  error?: { code?: string; message?: string };
}

interface ErrorBody {
  error?: string;
  message?: string;
}

export class EixClient {
  readonly baseUrl: string;
  private readonly token: string | undefined;
  private readonly fetchImpl: FetchLike;
  private readonly extraHeaders: Record<string, string>;
  private nextId = 1;

  constructor(options: EixClientOptions) {
    if (!options.baseUrl) throw new TypeError("EixClient needs a baseUrl");
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.token = options.token;
    this.fetchImpl = options.fetch ?? ((input, init) => fetch(input, init));
    this.extraHeaders = { ...options.headers };
  }

  /** A copy of this client with a different token. */
  withToken(token: string | undefined): EixClient {
    return new EixClient({
      baseUrl: this.baseUrl,
      token,
      fetch: this.fetchImpl,
      headers: this.extraHeaders,
    });
  }

  // ---------------------------------------------------------------- low level

  /** Send one JSON-RPC request and return its `result`. */
  async rpc<T = unknown>(method: string, params: unknown = null): Promise<T> {
    const id = this.nextId++;
    const response = await this.post("/rpc", { id, method, params }, true);
    const envelope = (await this.decode(response, "/rpc")) as RpcEnvelope | null;
    if (envelope === null || typeof envelope !== "object") {
      throw new EixError("malformed JSON-RPC response", response.status, "bad_response");
    }
    if (envelope.error) {
      throw new EixError(
        envelope.error.message ?? "JSON-RPC request failed",
        response.status,
        envelope.error.code ?? "rpc_error",
      );
    }
    return envelope.result as T;
  }

  // -------------------------------------------------------------- HTTP surface

  /** Liveness probe. Does not require a token. */
  async health(): Promise<Health> {
    const response = await this.get("/health", false);
    return (await this.decode(response, "/health")) as Health;
  }

  /** The daemon's OpenAPI 3.1 document. Does not require a token. */
  async openapi(): Promise<Record<string, unknown>> {
    const response = await this.get("/openapi.json", false);
    return (await this.decode(response, "/openapi.json")) as Record<string, unknown>;
  }

  async stats(): Promise<Stats> {
    const response = await this.get("/stats", true);
    return (await this.decode(response, "/stats")) as Stats;
  }

  /**
   * Redeem a one-time pairing code (from `eix pair`) for a durable token.
   * This request is deliberately unauthenticated; the code is the credential.
   */
  async pair(code: string, name?: string): Promise<PairResult> {
    const body: Record<string, string> = { code };
    if (name !== undefined && name !== "") body.name = name;
    const response = await this.post("/pair", body, false);
    const result = (await this.decode(response, "/pair")) as PairResult | null;
    if (result === null || typeof result.token !== "string") {
      throw new EixError("malformed pairing response", response.status, "bad_response");
    }
    return result;
  }

  /**
   * Server-Sent Events stream of daemon notifications. Iteration ends when the
   * stream closes; abort via `options.signal` or `return()` on the generator.
   */
  async *events(options: EventOptions = {}): AsyncGenerator<Notification, void, void> {
    const url = new URL(`${this.baseUrl}/events`);
    for (const id of normalizeIds(options.sessionId)) {
      url.searchParams.append("session_id", id);
    }
    if (options.all) url.searchParams.set("all", "1");

    const response = await this.fetchImpl(url.toString(), {
      headers: this.headers(true),
      ...(options.signal ? { signal: options.signal } : {}),
    });
    if (!response.ok) throw await this.errorFrom(response, "/events");
    if (!response.body) return;

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let boundary = nextBoundary(buffer);
        while (boundary) {
          const frame = buffer.slice(0, boundary.index);
          buffer = buffer.slice(boundary.index + boundary.length);
          const notification = parseFrame(frame);
          if (notification) yield notification;
          boundary = nextBoundary(buffer);
        }
      }
      const tail = parseFrame(buffer);
      if (tail) yield tail;
    } finally {
      try {
        reader.releaseLock();
      } catch {
        // The stream may already be closed by an abort.
      }
    }
  }

  // ------------------------------------------------------------- session API

  async sessions(includeArchived = false): Promise<Session[]> {
    const result = await this.rpc<SessionList>("session.list", {
      include_archived: includeArchived,
    });
    return result.sessions;
  }

  async createSession(params: CreateSessionParams = {}): Promise<Session> {
    const result = await this.rpc<SessionResult>("session.create", params);
    return result.session;
  }

  /** Replay a session's persisted events, optionally only the last `tail`. */
  async attach(sessionId: SessionId, tail?: number): Promise<AttachResult> {
    const params: Record<string, unknown> = { session_id: sessionId };
    if (tail !== undefined) params.tail = tail;
    return this.rpc<AttachResult>("session.attach", params);
  }

  /** Start a turn. Stream it with `events()` or `promptStream()`. */
  async prompt(sessionId: SessionId, text: string, images: string[] = []): Promise<SubmitResult> {
    return this.rpc<SubmitResult>("prompt.submit", {
      session_id: sessionId,
      text,
      images,
    });
  }

  async cancel(sessionId: SessionId): Promise<CancelResult> {
    return this.rpc<CancelResult>("prompt.cancel", { session_id: sessionId });
  }

  async setMode(sessionId: SessionId, mode: SessionMode, temporary = false): Promise<Session> {
    const result = await this.rpc<SessionResult>("session.mode", {
      session_id: sessionId,
      mode,
      temporary,
    });
    return result.session;
  }

  async rename(sessionId: SessionId, name: string): Promise<Session> {
    const result = await this.rpc<SessionResult>("session.rename", {
      session_id: sessionId,
      name,
    });
    return result.session;
  }

  // ------------------------------------------------------------ approvals/ask

  /** Pending approvals across sessions, unless `sessionId` narrows them. */
  async approvals(sessionId?: SessionId): Promise<ApprovalInfo[]> {
    const params: Record<string, unknown> = {};
    if (sessionId !== undefined) params.session_id = sessionId;
    const result = await this.rpc<ApprovalListResult>("approval.list", params);
    return result.approvals;
  }

  /** Approve one blocking tool call, optionally only selected hunks. */
  async approve(
    sessionId: SessionId,
    approvalId: string,
    options: ApprovalOptions = {},
  ): Promise<ApprovalResult> {
    return this.respondApproval(sessionId, approvalId, {
      decision: "approve",
      ...options,
    });
  }

  /** Approve and remember a matching rule for `scope` (default: session). */
  async always(
    sessionId: SessionId,
    approvalId: string,
    scope: ApprovalScope = "session",
    options: ApprovalOptions = {},
  ): Promise<ApprovalResult> {
    return this.respondApproval(sessionId, approvalId, {
      decision: "always",
      scope,
      ...options,
    });
  }

  async deny(
    sessionId: SessionId,
    approvalId: string,
    reason?: string,
  ): Promise<ApprovalResult> {
    return this.respondApproval(sessionId, approvalId, {
      decision: "deny",
      ...(reason !== undefined ? { reason } : {}),
    });
  }

  async respondApproval(
    sessionId: SessionId,
    approvalId: string,
    choice: {
      decision: ApprovalDecision;
      scope?: ApprovalScope;
      reason?: string;
      hunks?: number[];
    },
  ): Promise<ApprovalResult> {
    const params: Record<string, unknown> = {
      session_id: sessionId,
      approval_id: approvalId,
      decision: choice.decision,
    };
    if (choice.reason !== undefined) params.reason = choice.reason;
    if (choice.hunks !== undefined) params.hunks = choice.hunks;
    if (choice.scope !== undefined) params.scope = choice.scope;
    return this.rpc<ApprovalResult>("approval.respond", params);
  }

  /** Answer an `input.required` question by zero-based option index. */
  async answer(
    sessionId: SessionId,
    inputId: string,
    optionIndex: number,
  ): Promise<InputResult> {
    return this.rpc<InputResult>("input.respond", {
      session_id: sessionId,
      input_id: inputId,
      option_index: optionIndex,
    });
  }

  // ------------------------------------------------------------ turn streaming

  /**
   * Submit a prompt and yield every notification for that turn until
   * `turn.completed`. The event stream is opened before the prompt is sent, so
   * early deltas are not lost.
   *
   * Use `events()` instead when you need approvals in the same loop or want to
   * drive several turns from one subscription.
   */
  async *promptStream(
    sessionId: SessionId,
    text: string,
    images: string[] = [],
    options: ResponseStreamOptions = {},
  ): AsyncGenerator<Notification, void, void> {
    const abort = new AbortController();
    const forwardAbort = () => abort.abort();
    options.signal?.addEventListener("abort", forwardAbort, { once: true });

    const queue = new AsyncQueue<Notification>();
    const pump = (async () => {
      try {
        for await (const notification of this.events({
          sessionId: [sessionId],
          signal: abort.signal,
        })) {
          queue.push(notification);
        }
        queue.close();
      } catch (error) {
        if (abort.signal.aborted) queue.close();
        else queue.fail(error);
      }
    })();

    try {
      const started = await this.prompt(sessionId, text, images);
      const turnId = started.turn_id;
      for await (const notification of queue) {
        yield notification;
        const params = notification.params as Partial<TurnCompletedParams>;
        if (notification.method === "turn.completed" && params.turn_id === turnId) return;
      }
    } finally {
      options.signal?.removeEventListener("abort", forwardAbort);
      abort.abort();
      await pump.catch(() => undefined);
    }
  }

  // ------------------------------------------------------------------ private

  private headers(authenticated: boolean): Record<string, string> {
    const headers: Record<string, string> = { ...this.extraHeaders };
    if (authenticated && this.token !== undefined && this.token !== "") {
      headers.authorization = `Bearer ${this.token}`;
    }
    return headers;
  }

  private async get(path: string, authenticated: boolean): Promise<Response> {
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, {
      headers: this.headers(authenticated),
    });
    if (!response.ok) throw await this.errorFrom(response, path);
    return response;
  }

  private async post(path: string, body: unknown, authenticated: boolean): Promise<Response> {
    const headers = this.headers(authenticated);
    headers["content-type"] = "application/json";
    headers.accept = "application/json";
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
    });
    if (!response.ok) throw await this.errorFrom(response, path);
    return response;
  }

  private async decode(response: Response, path: string): Promise<unknown> {
    try {
      return await response.json();
    } catch (error) {
      throw new EixError(
        `${path} returned invalid JSON: ${(error as Error).message}`,
        response.status,
        "bad_response",
      );
    }
  }

  private async errorFrom(response: Response, path: string): Promise<EixError> {
    let code = "http_error";
    let message = `${path} failed with HTTP ${response.status}`;
    try {
      const payload = (await response.json()) as ErrorBody;
      if (typeof payload.error === "string") code = payload.error;
      if (typeof payload.message === "string") message = payload.message;
      else if (typeof payload.error === "string") message = payload.error;
    } catch {
      // Not every error path returns JSON; keep the status-based message.
    }
    return new EixError(message, response.status, code);
  }
}

function normalizeIds(sessionId: SessionId | SessionId[] | undefined): SessionId[] {
  if (sessionId === undefined) return [];
  return Array.isArray(sessionId) ? sessionId : [sessionId];
}

/** Earliest SSE frame terminator and its length. */
function nextBoundary(
  buffer: string,
): { index: number; length: number } | null {
  const lf = buffer.indexOf("\n\n");
  const crlf = buffer.indexOf("\r\n\r\n");
  if (lf === -1 && crlf === -1) return null;
  if (lf === -1) return { index: crlf, length: 4 };
  if (crlf === -1) return { index: lf, length: 2 };
  return lf < crlf ? { index: lf, length: 2 } : { index: crlf, length: 4 };
}

/** Parse one SSE frame; comment-only frames (heartbeats) return `null`. */
function parseFrame(frame: string): Notification | null {
  const data: string[] = [];
  for (const line of frame.split(/\r?\n/)) {
    if (line === "" || line.startsWith(":")) continue;
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    if (field !== "data") continue;
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    data.push(value);
  }
  if (data.length === 0) return null;
  try {
    return JSON.parse(data.join("\n")) as Notification;
  } catch {
    return null;
  }
}

type Waiter<T> = {
  resolve: (result: IteratorResult<T>) => void;
  reject: (error: unknown) => void;
};

/**
 * Minimal push/pull queue used to merge an SSE subscription with a prompt turn.
 * Deliberately small: no buffering policy, no backpressure.
 */
class AsyncQueue<T> implements AsyncIterable<T> {
  private items: T[] = [];
  private waiters: Array<Waiter<T>> = [];
  private closed = false;
  private failed = false;
  private failure: unknown = undefined;

  push(item: T): void {
    if (this.closed) return;
    const waiter = this.waiters.shift();
    if (waiter) waiter.resolve({ value: item, done: false });
    else this.items.push(item);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) {
      waiter.resolve({ value: undefined, done: true });
    }
  }

  fail(error: unknown): void {
    if (this.closed) return;
    this.failed = true;
    this.failure = error;
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) waiter.reject(error);
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: (): Promise<IteratorResult<T>> => {
        if (this.items.length > 0) {
          return Promise.resolve({ value: this.items.shift() as T, done: false });
        }
        if (this.failed) return Promise.reject(this.failure);
        if (this.closed) return Promise.resolve({ value: undefined, done: true });
        return new Promise<IteratorResult<T>>((resolve, reject) => {
          this.waiters.push({ resolve, reject });
        });
      },
      return: (): Promise<IteratorResult<T>> => {
        this.close();
        return Promise.resolve({ value: undefined, done: true });
      },
    };
  }
}
