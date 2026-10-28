#!/usr/bin/env python3
"""Minimal eix server plugin (plugin API v2).

Wire format: JSON-RPC 2.0, one object per line on stdin/stdout. The daemon can
send several requests at once, so responses are matched by ``id`` rather than
by order. Notifications (events, ``$/cancel``, ``shutdown``) carry no ``id``.

The plugin advertises two tools, one prompt command, a declarative command
declared in ``eix-plugin.toml``, and a ``session.*`` event subscription.
``remember`` / ``recall`` use the host-managed KV store, scoped to the plugin
name, so the plugin never sees another plugin's data.
"""

import json
import sys

PLUGIN_NAME = "echo"
API_VERSION = 2
PROTOCOL_VERSION = 2
_next_host_id = 1000

# JSON-RPC 2.0 error codes used by this example.
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL = -32603


def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def reply(request_id, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result if result is not None else {}
    send(message)


def text(value):
    """One text content block."""
    return {"type": "text", "text": value}


def host_request(method, params):
    """Call a daemon host method and wait for its matching response.

    The daemon may interleave notifications and requests from other calls while
    we wait. This example only needs the answer to its own request; a more
    complete plugin would buffer or dispatch the others instead of dropping
    them.
    """
    global _next_host_id
    request_id = _next_host_id
    _next_host_id += 1
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }
    )
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
    # `host.log` is also accepted as a notification, but using the request form
    # here shows how a plugin waits for a host acknowledgement.
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
        "features": ["content_blocks", "cancel"],
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
        value = str(arguments.get("text", ""))
        return {
            "ok": True,
            "content": [text(f"{len(value.split())} word(s)")],
        }
    if name == "remember":
        key = str(arguments.get("key", "")).strip()
        value = str(arguments.get("value", ""))
        if not key:
            return {
                "ok": False,
                "is_error": True,
                "content": [text("key must not be empty")],
            }
        host_request(
            "host.storage.set",
            {"scope": "global", "key": key, "value": value},
        )
        return {"ok": True, "content": [text(f"remembered {key}")]}
    if name == "recall":
        key = str(arguments.get("key", "")).strip()
        result = host_request(
            "host.storage.get",
            {"scope": "global", "key": key},
        )
        value = result.get("value") if result else None
        return {
            "ok": True,
            "content": [text(value if value is not None else "(missing)")],
        }
    return {
        "ok": False,
        "is_error": True,
        "content": [text(f"unknown tool {name!r}")],
    }


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

        # Notifications have no id. Events, cancellation and shutdown use this
        # shape.
        if method is not None and request_id is None:
            if method == "shutdown":
                return
            if method == "$/cancel":
                continue
            if method.startswith("session."):
                print(f"echo plugin: event {method}", file=sys.stderr)
            continue

        try:
            if method == "initialize":
                protocol = params.get("protocol_version")
                if protocol != PROTOCOL_VERSION:
                    reply(
                        request_id,
                        error={
                            "code": INVALID_PARAMS,
                            "message": f"unsupported protocol version {protocol!r}",
                        },
                    )
                    continue
                reply(request_id, initialize(params))
            elif method == "tools/call":
                reply(request_id, call_tool(params))
            else:
                reply(
                    request_id,
                    error={
                        "code": METHOD_NOT_FOUND,
                        "message": f"unknown method {method!r}",
                    },
                )
        except Exception as exc:
            reply(
                request_id,
                error={"code": INTERNAL, "message": str(exc)},
            )


if __name__ == "__main__":
    main()
