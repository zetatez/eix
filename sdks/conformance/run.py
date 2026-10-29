#!/usr/bin/env python3
"""Protocol conformance harness for eix process plugins (API v2).

It spawns a plugin command, performs the handshake and exercises every
advertised surface it can drive without a live model turn:

* ``initialize`` and the advertised tools/commands/hooks/ui
* ``tools/call`` for an ``echo`` tool (or the first tool with empty arguments)
* ``commands/run`` for the first action command
* ``hooks/run`` for the first declared hook point
* ``ui/render`` for the first declared surface
* graceful ``shutdown``

Usage:

    python3 sdks/conformance/run.py -- python3 sdks/python/examples/echo.py
    python3 sdks/conformance/run.py -- node sdks/typescript/examples/echo.mjs

Exit status is 0 when every step passes; failures print the offending frame and
the plugin's stderr.
"""

from __future__ import annotations

import json
import os
import queue
import subprocess
import sys
import threading
import time
from typing import Any, Optional

API_VERSION = 2
PROTOCOL_VERSION = 2
TIMEOUT = 10.0


class Failure(Exception):
    pass


class Harness:
    def __init__(self, command: list[str]) -> None:
        self.command = command
        self.process: Optional[subprocess.Popen[str]] = None
        self.lines: "queue.Queue[Optional[str]]" = queue.Queue()
        self.stderr: list[str] = []
        self.seen: list[dict[str, Any]] = []

    def start(self) -> None:
        self.process = subprocess.Popen(
            self.command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            cwd=os.getcwd(),
        )
        assert self.process.stdout and self.process.stderr and self.process.stdin
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self) -> None:
        assert self.process and self.process.stdout
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def _read_stderr(self) -> None:
        assert self.process and self.process.stderr
        for line in self.process.stderr:
            self.stderr.append(line.rstrip("\n"))

    def send(self, value: dict[str, Any]) -> None:
        assert self.process and self.process.stdin
        self.process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def read(self, timeout: float = TIMEOUT) -> dict[str, Any]:
        try:
            line = self.lines.get(timeout=timeout)
        except queue.Empty:
            raise Failure(f"plugin did not answer within {timeout:.0f}s")
        if line is None:
            raise Failure("plugin closed stdout")
        line = line.strip()
        if not line:
            return self.read(timeout)
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            raise Failure(f"stdout is not NDJSON: {line!r}")
        if not isinstance(value, dict):
            raise Failure(f"frame is not a JSON object: {line!r}")
        self.seen.append(value)
        return value

    def _answer_host_call(self, message: dict[str, Any]) -> None:
        """Reply to plugin-initiated host requests; the real daemon does this."""
        request_id = message.get("id")
        if request_id is None:
            return
        method = str(message.get("method"))
        if method == "host.info":
            result = {
                "name": "eix-conformance",
                "api_version": API_VERSION,
                "capabilities": [],
                "workspace": os.getcwd(),
                "network_allow": [],
                "secrets": [],
                "rpc_allow": [],
            }
        elif method == "host.ping":
            result = {"pong": True}
        elif method == "host.storage.get":
            result = {"value": None}
        else:
            result = {}
        self.send({"jsonrpc": "2.0", "id": request_id, "result": result})

    def request(self, request_id: int, method: str, params: Any) -> Any:
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            message = self.read()
            message_id = message.get("id")
            if message.get("method") is not None:
                self._answer_host_call(message)
                continue  # plugin-initiated host call or notification
            if message_id != request_id:
                raise Failure(
                    f"{method}: expected response id {request_id}, got {message_id!r}"
                )
            if "error" in message:
                error = message["error"]
                raise Failure(
                    f"{method} failed ({error.get('code')}): {error.get('message')}"
                )
            return message.get("result")

    def shutdown(self) -> None:
        self.send({"jsonrpc": "2.0", "method": "shutdown"})
        assert self.process
        try:
            code = self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            raise Failure("plugin did not exit after shutdown")
        if code != 0:
            raise Failure(f"plugin exited with {code} after shutdown")

    def stop(self) -> None:
        if self.process and self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=5)

    def diagnostics(self) -> str:
        tail = "\n".join(self.stderr[-20:])
        frames = "\n".join(json.dumps(frame)[:400] for frame in self.seen[-5:])
        return f"--- plugin stderr ---\n{tail}\n--- last frames ---\n{frames}"


def expect(condition: bool, message: str) -> None:
    if not condition:
        raise Failure(message)


def run(command: list[str]) -> None:
    harness = Harness(command)
    harness.start()
    try:
        init = harness.request(
            1,
            "initialize",
            {
                "protocol_version": PROTOCOL_VERSION,
                "api_version": API_VERSION,
                "plugin": {"name": "conformance", "version": "0.0.0"},
                "host": {"name": "eix-conformance", "version": "0.0.0"},
                "features": ["content_blocks", "cancel", "command_actions", "ui"],
                "capabilities": ["tools", "commands", "hooks", "ui", "events"],
                "workspace": os.getcwd(),
                "config": {"settings": {}},
            },
        )
        expect(isinstance(init, dict), "initialize did not return an object")
        expect(init.get("api_version") == API_VERSION, "initialize api_version mismatch")
        expect(isinstance(init.get("name"), str) and init["name"], "initialize needs a name")
        tools = init.get("tools") or []
        expect(isinstance(tools, list), "initialize tools must be a list")

        if tools:
            names = [tool.get("name") for tool in tools]
            tool = "echo" if "echo" in names else names[0]
            arguments = {"text": "conformance"} if tool == "echo" else {}
            result = harness.request(
                2, "tools/call", {"tool": tool, "arguments": arguments}
            )
            expect(isinstance(result, dict), f"tools/call {tool} did not return an object")
            expect(result.get("ok", True) is not False, f"tools/call {tool} reported failure")
            if tool == "echo":
                content = result.get("content")
                expect(isinstance(content, list) and content, "echo must return content blocks")
                expect(
                    any(
                        isinstance(block, dict) and block.get("type") == "text"
                        for block in content
                    ),
                    "echo must return a text block",
                )

        commands = init.get("commands") or []
        action_command = next(
            (command for command in commands if command.get("action")), None
        )
        if action_command:
            result = harness.request(
                3,
                "commands/run",
                {
                    "command": action_command["action"],
                    "arguments": "conformance",
                    "session_id": "conformance",
                    "workspace": os.getcwd(),
                    "cwd": os.getcwd(),
                },
            )
            expect(isinstance(result, dict), "commands/run did not return an object")
            expect(result.get("ok", True) is not False, "commands/run reported failure")

        hooks = init.get("hooks") or []
        if hooks:
            result = harness.request(
                4,
                "hooks/run",
                {
                    "point": hooks[0].get("point"),
                    "session_id": "conformance",
                    "workspace": os.getcwd(),
                    "cwd": os.getcwd(),
                },
            )
            expect(isinstance(result, dict), "hooks/run did not return an object")

        surfaces = init.get("ui") or []
        if surfaces:
            surface = surfaces[0]
            result = harness.request(
                5,
                "ui/render",
                {"surface": surface.get("surface"), "id": surface.get("id")},
            )
            expect(isinstance(result, dict), "ui/render did not return an object")
            expect(
                "node" in result or "body" in result,
                "ui/render needs a node or body",
            )

        harness.shutdown()
        print(
            "conformance ok: "
            f"{len(tools)} tool(s), {len(commands)} command(s), "
            f"{len(hooks)} hook(s), {len(surfaces)} surface(s)"
        )
    except Failure:
        print(harness.diagnostics(), file=sys.stderr)
        raise
    finally:
        harness.stop()


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] == "--":
        args = args[1:]
    if not args:
        print(__doc__, file=sys.stderr)
        return 2
    started = time.time()
    try:
        run(args)
    except Failure as failure:
        print(f"conformance failed: {failure}", file=sys.stderr)
        return 1
    print(f"finished in {time.time() - started:.2f}s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
