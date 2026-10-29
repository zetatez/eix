"""Dependency-free Python SDK for eix server plugins (plugin API v2).

The host speaks JSON-RPC 2.0 over NDJSON on stdin/stdout. This module hides the
framing, request dispatch and host calls; a plugin only registers handlers and
calls :meth:`Plugin.run`.

```python
from eix_plugin import Plugin, text

plugin = Plugin("hello", version="0.1.0")


@plugin.tool(
    "echo",
    description="Echo a string.",
    input_schema={
        "type": "object",
        "properties": {"text": {"type": "string"}},
    },
)
def echo(arguments, ctx):
    return text(str(arguments.get("text", "")))


if __name__ == "__main__":
    plugin.run()
```

Handlers are synchronous and run on the reader thread. A handler may call
``ctx.host(...)``; while waiting for the host's answer the SDK defers any
incoming request and processes it after the current handler returns.
"""

from __future__ import annotations

import json
import sys
import traceback
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

API_VERSION = 2
PROTOCOL_VERSION = 2

METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL = -32603

# The daemon allocates its request ids from 1 upward. Starting plugin ids in
# another range keeps a simultaneous request in each direction from being
# mistaken for the other's response.
_HOST_ID_BASE = 1 << 32

ContentBlock = dict[str, Any]
Handler = Callable[..., Any]


def text(value: Any) -> ContentBlock:
    """One text content block."""
    return {"type": "text", "text": str(value)}


def image(mime: str, blob: str) -> ContentBlock:
    """One inline image block; `blob` is base64."""
    return {"type": "image", "mime": mime, "blob": blob}


def resource(uri: str, title: Optional[str] = None, mime: Optional[str] = None) -> ContentBlock:
    block: ContentBlock = {"type": "resource", "uri": uri}
    if title is not None:
        block["title"] = title
    if mime is not None:
        block["mime"] = mime
    return block


def diff(path: str, patch: str) -> ContentBlock:
    """A unified diff the TUI can render."""
    return {"type": "diff", "path": path, "patch": patch}


def json_block(value: Any) -> ContentBlock:
    """Structured JSON rendered as a block instead of a string."""
    return {"type": "json", "value": value}


def tool_result(
    blocks: Optional[list[ContentBlock]] = None,
    *,
    ok: bool = True,
    is_error: bool = False,
    structured: Any = None,
) -> dict[str, Any]:
    """Build a ``tools/call`` result. ``text(value)`` alone also works."""
    result: dict[str, Any] = {"ok": ok, "content": blocks or []}
    if is_error:
        result["is_error"] = True
    if structured is not None:
        result["structured"] = structured
    return result


@dataclass
class Context:
    """Per-call context passed to every handler."""

    plugin: "Plugin"
    params: dict[str, Any] = field(default_factory=dict)
    session_id: Optional[str] = None
    workspace: Optional[str] = None

    def host(self, method: str, params: Any = None) -> Any:
        """Call a daemon host method and return its result."""
        return self.plugin.host_request(method, params or {})

    def log(self, message: str, level: str = "info") -> None:
        """Write a line to the host's redacted plugin log."""
        self.plugin.log(message, level)

    def notify(self, message: str, level: str = "info") -> None:
        """Show a transient notification in every attached TUI."""
        self.plugin.notify(message, level)

    def storage_get(self, key: str, scope: str = "global") -> Any:
        result = self.host("host.storage.get", {"scope": scope, "key": key})
        return result.get("value") if isinstance(result, dict) else None

    def storage_set(self, key: str, value: str, scope: str = "global") -> None:
        self.host("host.storage.set", {"scope": scope, "key": key, "value": value})


class Plugin:
    """A process plugin with a blocking NDJSON request loop."""

    def __init__(
        self,
        name: str,
        *,
        version: str = "0.1.0",
        features: Optional[list[str]] = None,
        capabilities: Optional[list[str]] = None,
    ) -> None:
        self.name = name
        self.version = version
        self.features = features or ["content_blocks", "cancel", "command_actions", "ui"]
        self.capabilities = capabilities or []
        self.tools: dict[str, dict[str, Any]] = {}
        self.commands: list[dict[str, Any]] = []
        self.hooks: list[dict[str, Any]] = []
        self.events: list[str] = []
        self.ui_surfaces: list[dict[str, Any]] = []
        self.rpcs: list[dict[str, Any]] = []
        self._tool_handlers: dict[str, Handler] = {}
        self._command_handlers: dict[str, Handler] = {}
        self._hook_handlers: dict[str, Handler] = {}
        self._event_handlers: dict[str, Handler] = {}
        self._ui_renderers: dict[str, Handler] = {}
        self._ui_actions: dict[str, Handler] = {}
        self._ui_pickers: dict[str, Handler] = {}
        self._rpc_handlers: dict[str, Handler] = {}
        self._next_host_id = _HOST_ID_BASE
        self._deferred: list[dict[str, Any]] = []
        self._closed = False
        self._initialized = False

    # -- registration -----------------------------------------------------

    def tool(
        self,
        name: str,
        *,
        description: str = "",
        input_schema: Optional[dict[str, Any]] = None,
        read_only: bool = False,
    ) -> Callable[[Handler], Handler]:
        """Register a model tool. The function receives ``(arguments, ctx)``."""

        def register(handler: Handler) -> Handler:
            self.tools[name] = {
                "name": name,
                "description": description,
                "input_schema": input_schema
                or {"type": "object", "properties": {}},
                "read_only": read_only,
            }
            self._tool_handlers[name] = handler
            return handler

        return register

    def command(
        self,
        name: str,
        *,
        description: str = "",
        args: str = "",
        action: Optional[str] = None,
        prompt: Optional[str] = None,
    ) -> Callable[[Handler], Handler]:
        """Register a command.

        ``action`` makes the command call ``commands/run``; pass a handler that
        receives ``(arguments, ctx)``. ``prompt`` declares a prompt-only command
        and needs no handler. Providing neither makes the command name itself
        the action.
        """
        entry: dict[str, Any] = {
            "name": name,
            "description": description,
            "args": args,
        }
        if prompt is not None:
            entry["prompt"] = prompt
        else:
            entry["action"] = action or name
        self.commands.append(entry)

        def register(handler: Handler) -> Handler:
            if prompt is None:
                self._command_handlers[entry["action"]] = handler
            return handler

        return register

    def hook(
        self,
        point: str,
        *,
        priority: int = 0,
        timeout_ms: int = 0,
        on_error: str = "allow",
    ) -> Callable[[Handler], Handler]:
        """Register a hook for one of ``tool.before``, ``tool.after``,
        ``prompt.build``, ``turn.stop`` or ``turn.end``."""
        entry = {
            "point": point,
            "priority": priority,
            "timeout_ms": timeout_ms,
            "on_error": on_error,
        }
        self.hooks.append(entry)

        def register(handler: Handler) -> Handler:
            self._hook_handlers[point] = handler
            return handler

        return register

    def event(self, pattern: str) -> None:
        """Subscribe to daemon notifications such as ``session.*``."""
        self.events.append(pattern)

    def on_event(self, method: str, handler: Optional[Handler] = None) -> Any:
        """Handle a daemon notification. Usable as a decorator."""

        def register(candidate: Handler) -> Handler:
            self._event_handlers[method] = candidate
            return candidate

        return register if handler is None else register(handler)

    def ui(
        self,
        surface_id: str,
        *,
        title: str = "",
        surface: str = "panel",
        render: Optional[Handler] = None,
        action: Optional[Handler] = None,
        pick: Optional[Handler] = None,
    ) -> None:
        """Declare a UI surface and optional ``ui/render`` / ``ui/action`` /
        ``ui/pick`` handlers."""
        self.ui_surfaces.append(
            {"surface": surface, "id": surface_id, "title": title or surface_id}
        )
        if render is not None:
            self._ui_renderers[surface_id] = render
        if action is not None:
            self._ui_actions[surface_id] = action
        if pick is not None:
            self._ui_pickers[surface_id] = pick

    def rpc(self, name: str, *, description: str = "") -> Callable[[Handler], Handler]:
        """Expose a plugin RPC method. The handler receives ``(arguments, ctx)``."""
        self.rpcs.append({"name": name, "description": description})

        def register(handler: Handler) -> Handler:
            self._rpc_handlers[name] = handler
            return handler

        return register

    # -- transport --------------------------------------------------------

    def send(self, value: Any) -> None:
        """Write one NDJSON frame to stdout."""
        sys.stdout.write(json.dumps(value, separators=(",", ":"), default=str) + "\n")
        sys.stdout.flush()

    def reply(
        self,
        request_id: Any,
        result: Any = None,
        error: Optional[dict[str, Any]] = None,
    ) -> None:
        message = {"jsonrpc": "2.0", "id": request_id}
        if error is not None:
            message["error"] = error
        else:
            message["result"] = {} if result is None else result
        self.send(message)

    def host_request(self, method: str, params: Any = None) -> Any:
        """Send a request to the daemon and block until its response arrives."""
        request_id = self._next_host_id
        self._next_host_id += 1
        self.send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params or {},
            }
        )
        while True:
            message = self._read_message()
            if message is None:
                raise RuntimeError("host closed stdin")
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    error = message["error"]
                    raise RuntimeError(error.get("message", "host error"))
                return message.get("result")
            self._deferred.append(message)

    def log(self, message: str, level: str = "info") -> None:
        """Host log without waiting for an acknowledgement."""
        self.send(
            {
                "jsonrpc": "2.0",
                "method": "host.log",
                "params": {"level": level, "message": str(message)},
            }
        )

    def notify(self, message: str, level: str = "info") -> None:
        """Ask the host to surface a notification; failure is non-fatal."""
        try:
            self.host_request("host.notify", {"message": str(message), "level": level})
        except Exception:
            pass

    # -- dispatch ---------------------------------------------------------

    def run(self) -> None:
        """Serve requests until stdin closes or the host sends ``shutdown``."""
        while not self._closed:
            if self._deferred:
                message = self._deferred.pop(0)
            else:
                message = self._read_message()
            if message is None:
                break
            self._handle(message)

    def _read_message(self) -> Optional[dict[str, Any]]:
        while True:
            line = sys.stdin.readline()
            if not line:
                return None
            line = line.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                print(f"eix-plugin[{self.name}]: invalid JSON frame", file=sys.stderr)
                continue
            if not isinstance(value, dict):
                continue
            return value

    def _handle(self, message: dict[str, Any]) -> None:
        method = message.get("method")
        request_id = message.get("id")
        params = message.get("params") or {}
        if method is None:
            return
        if request_id is None:
            self._handle_notification(method, params)
            return
        try:
            result = self._dispatch(method, params)
            self.reply(request_id, result)
        except _MethodNotFound as exc:
            self.reply(request_id, error={"code": METHOD_NOT_FOUND, "message": str(exc)})
        except Exception as exc:  # keep the plugin alive on handler bugs
            traceback.print_exc(file=sys.stderr)
            self.reply(request_id, error={"code": INTERNAL, "message": str(exc)})

    def _dispatch(self, method: str, params: dict[str, Any]) -> Any:
        if method == "initialize":
            protocol = params.get("protocol_version")
            if protocol != PROTOCOL_VERSION:
                raise ValueError(f"unsupported protocol version {protocol!r}")
            self._initialized = True
            self._log_safely("info", "initialized")
            return self.describe()
        if method == "tools/call":
            return self._call_tool(params)
        if method == "commands/run":
            return self._call_command(params)
        if method == "hooks/run":
            return self._call_hook(params)
        if method == "rpc.call":
            return self._call_rpc(params)
        if method == "ui/render":
            return self._render_ui(params)
        if method == "ui/action":
            return self._act_ui(params)
        if method == "ui/pick":
            return self._pick_ui(params)
        raise _MethodNotFound(f"unknown method {method!r}")

    def _handle_notification(self, method: str, params: dict[str, Any]) -> None:
        if method == "shutdown":
            self._closed = True
            return
        if method == "$/cancel":
            return
        handler = self._event_handlers.get(method)
        if handler is None:
            for pattern, candidate in self._event_handlers.items():
                if pattern.endswith("*") and method.startswith(pattern[:-1]):
                    handler = candidate
                    break
        if handler is not None:
            try:
                handler(params, self._context(params))
            except Exception:
                traceback.print_exc(file=sys.stderr)
        elif method == "host.log":
            pass

    def describe(self) -> dict[str, Any]:
        """The ``initialize`` result."""
        result: dict[str, Any] = {
            "name": self.name,
            "version": self.version,
            "api_version": API_VERSION,
            "features": self.features,
            "tools": list(self.tools.values()),
            "commands": self.commands,
            "events": self.events,
            "hooks": self.hooks,
            "ui": self.ui_surfaces,
            "rpc": self.rpcs,
        }
        return result

    def _context(self, params: dict[str, Any]) -> Context:
        return Context(
            plugin=self,
            params=params,
            session_id=params.get("session_id"),
            workspace=params.get("workspace") or params.get("cwd"),
        )

    def _call_tool(self, params: dict[str, Any]) -> dict[str, Any]:
        name = str(params.get("tool", ""))
        handler = self._tool_handlers.get(name)
        if handler is None:
            return tool_result([text(f"unknown tool {name!r}")], ok=False, is_error=True)
        value = handler(params.get("arguments") or {}, self._context(params))
        return _normalize_tool_result(value)

    def _call_command(self, params: dict[str, Any]) -> dict[str, Any]:
        action = str(params.get("command", ""))
        handler = self._command_handlers.get(action)
        if handler is None:
            return {
                "ok": False,
                "output": f"unknown command action {action!r}",
            }
        value = handler(str(params.get("arguments", "")), self._context(params))
        if isinstance(value, str):
            return {"ok": True, "output": value}
        return value

    def _call_hook(self, params: dict[str, Any]) -> Any:
        point = str(params.get("point", ""))
        handler = self._hook_handlers.get(point)
        if handler is None:
            raise _MethodNotFound(f"unknown hook point {point!r}")
        value = handler(params, self._context(params))
        return {} if value is None else value

    def _call_rpc(self, params: dict[str, Any]) -> Any:
        method = str(params.get("method", ""))
        handler = self._rpc_handlers.get(method)
        if handler is None:
            raise _MethodNotFound(f"unknown rpc method {method!r}")
        value = handler(params.get("arguments") or {}, self._context(params))
        return {} if value is None else value

    def _render_ui(self, params: dict[str, Any]) -> dict[str, Any]:
        surface_id = str(params.get("id", ""))
        renderer = self._ui_renderers.get(surface_id)
        if renderer is None:
            title = next(
                (
                    entry["title"]
                    for entry in self.ui_surfaces
                    if entry["id"] == surface_id
                ),
                surface_id,
            )
            return {"title": title, "body": ""}
        value = renderer(
            {
                "surface": params.get("surface", "panel"),
                "id": surface_id,
            },
            self._context(params),
        )
        if isinstance(value, str):
            return {"title": surface_id, "body": value}
        return value

    def _act_ui(self, params: dict[str, Any]) -> dict[str, Any]:
        surface_id = str(params.get("id", ""))
        handler = self._ui_actions.get(surface_id)
        if handler is None:
            return {"ok": True}
        value = handler(params, self._context(params))
        return value if value is not None else {"ok": True}

    def _pick_ui(self, params: dict[str, Any]) -> dict[str, Any]:
        surface_id = str(params.get("id", ""))
        handler = self._ui_pickers.get(surface_id)
        if handler is None:
            return {"items": []}
        items = handler(str(params.get("query", "")), self._context(params))
        return {"items": items or []}

    def _log_safely(self, level: str, message: str) -> None:
        try:
            self.log(message, level)
        except Exception:
            pass


class _MethodNotFound(Exception):
    pass


def _normalize_tool_result(value: Any) -> dict[str, Any]:
    """Accept a plain string, one content block, a list or a full result dict."""
    if isinstance(value, dict):
        if value.get("type") in {"text", "image", "resource", "diff", "json"}:
            return tool_result([value])
        return value
    if isinstance(value, str):
        return tool_result([text(value)])
    if isinstance(value, list):
        return tool_result(value)
    return tool_result([text(value)])


__all__ = [
    "API_VERSION",
    "PROTOCOL_VERSION",
    "Context",
    "Plugin",
    "diff",
    "image",
    "json_block",
    "resource",
    "text",
    "tool_result",
]
