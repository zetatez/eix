#!/usr/bin/env python3
"""Reference eix plugin built with the Python SDK.

Used by ``sdks/conformance/run.py`` and by ``eix plugin new --python`` style
packages as a starting point. Run it directly for a manual smoke test; the
daemon normally spawns it.
"""

from __future__ import annotations

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from eix_plugin import Context, Plugin, text

plugin = Plugin("sdk-echo", version="0.1.0")

plugin.event("session.*")


@plugin.tool(
    "echo",
    description="Echo a string.",
    input_schema={
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
    },
    read_only=True,
)
def echo(arguments: dict, ctx: Context):
    return text(str(arguments.get("text", "")))


@plugin.command("greet", description="Greet someone.", args="<name>", action="greet")
def greet(arguments: str, ctx: Context):
    who = arguments.strip() or "world"
    return {"ok": True, "output": f"hello, {who}"}


@plugin.hook("tool.before")
def before_tool(params: dict, ctx: Context):
    # Returning `{}` allows the tool to run; `{"action": "deny"}` blocks it.
    return {}


def _status_render(params: dict, ctx: Context):
    return {
        "title": "SDK echo",
        "node": {
            "kind": "group",
            "title": "Python SDK",
            "children": [
                {"kind": "heading", "text": "Conformance fixture", "level": 2},
                {"kind": "text", "text": "Rendered through ui/render."},
            ],
        },
        "body": "Python SDK\nRendered through ui/render.",
    }


plugin.ui("status", title="SDK echo", surface="panel", render=_status_render)


@plugin.on_event("session.*")
def on_session(params: dict, ctx: Context) -> None:
    pass


if __name__ == "__main__":
    plugin.run()
