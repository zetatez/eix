/**
 * Type definitions for the eix plugin SDK.
 *
 * The runtime is dependency-free ESM (`index.js`); this file gives TypeScript
 * plugins the same surface without requiring a build step for the SDK itself.
 */

export const API_VERSION: 2;
export const PROTOCOL_VERSION: 2;
export const METHOD_NOT_FOUND: -32601;
export const INVALID_PARAMS: -32602;
export const INTERNAL: -32603;

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; mime: string; blob: string }
  | { type: "resource"; uri: string; title?: string; mime?: string }
  | { type: "diff"; path: string; patch: string }
  | { type: "json"; value: unknown };

export interface ToolResult {
  ok?: boolean;
  content?: ContentBlock[];
  structured?: unknown;
  is_error?: boolean;
}

export interface UiAction {
  id: string;
  label: string;
  kind?: "default" | "primary" | "danger" | string;
  key?: string;
}

export type UiNode =
  | { kind: "text"; text: string }
  | { kind: "markdown"; text: string }
  | { kind: "heading"; text: string; level?: number }
  | { kind: "divider" }
  | { kind: "list"; items: string[] }
  | { kind: "table"; columns: string[]; rows: string[][] }
  | { kind: "actions"; actions: UiAction[] }
  | {
      kind: "form";
      id: string;
      title?: string;
      fields: Array<Record<string, unknown>>;
      submit?: UiAction;
    }
  | {
      kind: "picker";
      id: string;
      title?: string;
      items: Array<{ id: string; title: string; subtitle?: string }>;
    }
  | { kind: "group"; title?: string; children: UiNode[] };

export interface RenderResult {
  title?: string;
  node?: UiNode;
  body?: string;
}

export interface ActionResult {
  ok?: boolean;
  node?: UiNode;
  body?: string;
  prompt?: string;
  close?: boolean;
  error?: string;
}

export interface PickerItem {
  id: string;
  title: string;
  subtitle?: string;
}

export class Context {
  constructor(plugin: Plugin, params?: Record<string, unknown>);
  readonly plugin: Plugin;
  readonly params: Record<string, unknown>;
  readonly sessionId?: string;
  readonly workspace?: string;
  host<T = unknown>(method: string, params?: Record<string, unknown>): Promise<T>;
  log(message: string, level?: string): void;
  notify(message: string, level?: string): Promise<void>;
  storageGet(key: string, scope?: "global" | "workspace"): Promise<string | null>;
  storageSet(key: string, value: string, scope?: "global" | "workspace"): Promise<unknown>;
}

export interface PluginOptions {
  name: string;
  version?: string;
  features?: string[];
  capabilities?: string[];
}

export type ToolHandler = (
  arguments_: Record<string, unknown>,
  ctx: Context,
) =>
  | ToolResult
  | ContentBlock
  | ContentBlock[]
  | string
  | Promise<ToolResult | ContentBlock | ContentBlock[] | string>;

export type CommandHandler = (
  arguments_: string,
  ctx: Context,
) => { ok?: boolean; prompt?: string; output?: string } | string;

export type HookHandler = (
  params: Record<string, unknown>,
  ctx: Context,
) => Record<string, unknown> | void | Promise<Record<string, unknown> | void>;

export type UiHandler = (
  params: Record<string, unknown>,
  ctx: Context,
) => RenderResult | ActionResult | string | Promise<RenderResult | ActionResult | string>;

export class Plugin {
  constructor(options: PluginOptions);
  readonly name: string;
  readonly version: string;
  tool(
    name: string,
    options?: {
      description?: string;
      inputSchema?: Record<string, unknown>;
      readOnly?: boolean;
    },
    handler?: ToolHandler,
  ): this;
  command(
    name: string,
    options?: { description?: string; args?: string; action?: string; prompt?: string },
    handler?: CommandHandler,
  ): this;
  hook(
    point: "tool.before" | "tool.after" | "prompt.build" | "turn.stop" | "turn.end" | string,
    handler: HookHandler,
    options?: { priority?: number; timeoutMs?: number; onError?: string },
  ): this;
  event(pattern: string): this;
  onEvent(pattern: string, handler: HookHandler): this;
  ui(options: {
    id: string;
    title?: string;
    surface?: "panel" | "status" | "picker" | string;
    render?: UiHandler;
    action?: UiHandler;
    pick?: (query: string, ctx: Context) => PickerItem[] | Promise<PickerItem[]>;
  }): this;
  rpc(
    name: string,
    options?: { description?: string },
    handler?: (arguments_: Record<string, unknown>, ctx: Context) => unknown,
  ): this;
  hostRequest<T = unknown>(method: string, params?: Record<string, unknown>): Promise<T>;
  log(message: string, level?: string): void;
  notify(message: string, level?: string): Promise<void>;
  run(): Promise<void>;
}

export function text(value: unknown): ContentBlock;
export function image(mime: string, blob: string): ContentBlock;
export function resource(uri: string, title?: string, mime?: string): ContentBlock;
export function diff(path: string, patch: string): ContentBlock;
export function jsonBlock(value: unknown): ContentBlock;
export function toolResult(
  blocks?: ContentBlock[],
  options?: { ok?: boolean; isError?: boolean; structured?: unknown },
): ToolResult;

export default Plugin;
