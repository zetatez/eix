#!/usr/bin/env python3
"""Minimal eix server plugin.

Wire format: one JSON object per line on stdin/stdout. Host requests and
plugin requests share the same shape (``id`` + ``method`` + ``params``);
notifications have no ``id``.

The plugin advertises two tools, one prompt command and a ``session.*`` event
subscription. ``remember`` / ``recall`` use the host-managed KV store, scoped
to the plugin name, so it never sees another plugin's data.
"""

import json
import sys

PLUGIN_NAME = "echo"
API_VERSION = 1
_next_host_id = 1000


def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def reply(request_id, result=None, error=None):
    message = {"id": request_id}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result if result is not None else {}
    send(message)


def host_request(method, params):
    """Call a daemon host method and wait for its matching response.

    Event notifications can arrive while we wait; ignore them. The daemon
    handles this request from inside its own request loop, so a plugin can
    safely use storage during a tool call or during initialize.
    """
    global _next_host_id
    request_id = _next_host_id
    _next_host_id += 1
    send({"id": request_id, "method": method, "params": params})
    while True:
        line = sys.stdin.readline()
        if not line:
            raise RuntimeError("host closed stdin")
        line = line.strip()
        if not line:
            continue
        value = json.loads(line)
        if value.get("id") == request_id and "method" not in value:
            if "error" in value:
                raise RuntimeError(value["error"].get("message", "host error"))
            return value.get("result")


def initialize(params):
    # Demonstrate the host RPC path without making startup depend on it.
    try:
        host_request(
            "host.log",
            {"level": "info", "message": "echo plugin initialized"},
        )
    except Exception as exc:  # pragma: no cover - only a diagnostic
        print(f"echo plugin: host.log failed: {exc}", file=sys.stderr)

    return {
        "name": PLUGIN_NAME,
        "version": "0.1.0",
        "api_version": API_VERSION,
        "tools": [
            {
                "name": "word_count",
                "description": "Count words in a string.",
                "input_schema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                },
                "read_only": True,
            },
            {
                "name": "remember",
                "description": "Store a short note in the plugin KV store.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "value": {"type": "string"},
                    },
                    "required": ["key", "value"],
                },
                "read_only": False,
            },
            {
                "name": "recall",
                "description": "Read a note from the plugin KV store.",
                "input_schema": {
                    "type": "object",
                    "properties": {"key": {"type": "string"}},
                    "required": ["key"],
                },
                "read_only": True,
            },
        ],
        "commands": [
            {
                "name": "review",
                "description": "Ask the model to review a path.",
                "args": "<path>",
                "prompt": "Review $ARGUMENTS. Focus on correctness and tests.",
            }
        ],
        "events": ["session.*"],
    }


def call_tool(params):
    name = params.get("tool")
    arguments = params.get("arguments") or {}
    if name == "word_count":
        text = str(arguments.get("text", ""))
        return {"ok": True, "output": f"{len(text.split())} word(s)"}
    if name == "remember":
        key = str(arguments.get("key", "")).strip()
        value = str(arguments.get("value", ""))
        if not key:
            return {"ok": False, "output": "key must not be empty"}
        host_request(
            "host.storage.set",
            {"scope": "global", "key": key, "value": value},
        )
        return {"ok": True, "output": f"remembered {key}"}
    if name == "recall":
        key = str(arguments.get("key", "")).strip()
        result = host_request(
            "host.storage.get",
            {"scope": "global", "key": key},
        )
        value = result.get("value") if result else None
        return {"ok": True, "output": value if value is not None else "(missing)"}
    return {"ok": False, "output": f"unknown tool {name!r}"}


def main():
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            print(f"echo plugin: invalid JSON: {line!r}", file=sys.stderr)
            continue

        method = message.get("method")
        request_id = message.get("id")
        params = message.get("params") or {}

        # Event notifications from the daemon have no id.
        if method is not None and request_id is None:
            if method.startswith("session."):
                print(
                    f"echo plugin: event {method}",
                    file=sys.stderr,
                )
            continue

        try:
            if method == "initialize":
                reply(request_id, initialize(params))
            elif method == "tools/call":
                reply(request_id, call_tool(params))
            elif method == "host.ping":
                reply(request_id, {"pong": True})
            else:
                reply(
                    request_id,
                    error={
                        "code": "unsupported",
                        "message": f"unknown method {method!r}",
                    },
                )
        except Exception as exc:
            reply(
                request_id,
                error={"code": "plugin_error", "message": str(exc)},
            )


if __name__ == "__main__":
    main()
