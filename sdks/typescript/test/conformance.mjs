#!/usr/bin/env node
/** Run the protocol conformance harness against the JavaScript fixture. */

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..", "..", "..");
const harness = path.join(root, "sdks", "conformance", "run.py");
const fixture = path.join(root, "sdks", "typescript", "examples", "echo.mjs");

const result = spawnSync(
  "python3",
  [harness, "--", process.execPath, fixture],
  { cwd: root, encoding: "utf8" },
);
process.stdout.write(result.stdout ?? "");
process.stderr.write(result.stderr ?? "");
if (result.error) {
  console.error(result.error);
  process.exit(1);
}
process.exit(result.status ?? 1);
