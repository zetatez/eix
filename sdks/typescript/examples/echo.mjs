#!/usr/bin/env node
/**
 * Reference eix plugin built with the TypeScript/JavaScript SDK.
 *
 * Used by `sdks/conformance/run.py` and as a starting point for Node plugins.
 */

import { Plugin, text } from "../index.js";

const plugin = new Plugin({ name: "sdk-echo-ts", version: "0.1.0" });

plugin.event("session.*");

plugin.tool(
  "echo",
  {
    description: "Echo a string.",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
    readOnly: true,
  },
  (arguments_) => text(String(arguments_.text ?? "")),
);

plugin.command(
  "greet",
  { description: "Greet someone.", args: "<name>", action: "greet" },
  (arguments_) => ({ ok: true, output: `hello, ${arguments_.trim() || "world"}` }),
);

plugin.hook("tool.before", () => ({}));

plugin.ui({
  id: "status",
  title: "SDK echo",
  surface: "panel",
  render: (_params, _ctx) => ({
    title: "SDK echo",
    node: {
      kind: "group",
      title: "TypeScript SDK",
      children: [
        { kind: "heading", text: "Conformance fixture", level: 2 },
        { kind: "text", text: "Rendered through ui/render." },
      ],
    },
    body: "TypeScript SDK\nRendered through ui/render.",
  }),
});

plugin.onEvent("session.*", (_params, _ctx) => {});

await plugin.run();
