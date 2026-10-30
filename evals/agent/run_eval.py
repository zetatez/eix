#!/usr/bin/env python3
"""Run eix on a small, deterministic coding benchmark.

The harness owns the whole loop: it creates a throwaway git repo per task,
starts an isolated daemon, lets eix work in full mode, then applies hidden
tests and records quality/efficiency metrics.

It never reads the provider API key: the daemon and CLI load the user's
config directly, and every run gets its own data dir and Unix socket.

Usage:
    python3 evals/agent/run_eval.py --tasks fix_pagination
    python3 evals/agent/run_eval.py --repeats 2 --output reports/baseline.json

Exit status is 0 when every run passes, 1 when a task fails or the harness
itself errors.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import socket
import sqlite3
import subprocess
import sys
import time
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

import score as scoring

ROOT = Path(__file__).resolve().parents[2]
TASKS_DIR = Path(__file__).resolve().parent / "tasks"
DEFAULT_CONFIG = Path.home() / ".config" / "eix" / "config.toml"

# Commands that create throwaway files or delete things the task didn't ask
# about. They are not fatal, but they burn steps, so the score counts them.
SCRATCH_MARKERS = (
    "tmp_verify",
    "edge_cases",
    "scratch",
    "mkdir -p tests",
)
# Ad-hoc verification outside the workspace: mktemp dirs, /tmp probe programs,
# and include!-based harnesses. The agent should trust the task's own tests.
SCRATCH_PROGRAM = re.compile(
    r"(mktemp\s+-d|include!\(|cat\s*>\s*(?:\"?\$work|/tmp/|/var/tmp/)|mkdir\s+-p\s+/tmp/)"
)
DELETE_MARKER = re.compile(r"\brm\s+(?:-\w+\s+)*(?:Cargo\.lock|tests/|src/[^\s]+\.rs)\b")
HEREDOC_WRITE = re.compile(r"cat\s*>+\s*(?:tests/|src/)[^\s]*\.rs\b")


def now_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def eprint(*args: object) -> None:
    print(*args, file=sys.stderr, flush=True)


def wait_for_socket(path: Path, timeout: float = 20.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                client.connect(str(path))
                return
            except OSError:
                pass
            finally:
                client.close()
        time.sleep(0.05)
    raise RuntimeError(f"daemon socket did not become ready: {path}")


def run(cmd: list[str], *, cwd: Path, env: dict[str, str], timeout: float) -> dict:
    started = time.monotonic()
    try:
        proc = subprocess.run(
            cmd,
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        result = {
            "exit": proc.returncode,
            "stdout": proc.stdout,
            "stderr": proc.stderr,
            "timed_out": False,
        }
    except subprocess.TimeoutExpired as exc:
        result = {
            "exit": None,
            "stdout": (exc.stdout or b"").decode("utf-8", "replace")
            if isinstance(exc.stdout, bytes)
            else (exc.stdout or ""),
            "stderr": (exc.stderr or b"").decode("utf-8", "replace")
            if isinstance(exc.stderr, bytes)
            else (exc.stderr or ""),
            "timed_out": True,
        }
    result["duration_s"] = round(time.monotonic() - started, 3)
    return result


def copy_tree(source: Path, target: Path) -> None:
    for item in source.rglob("*"):
        rel = item.relative_to(source)
        dest = target / rel
        if item.is_dir():
            dest.mkdir(parents=True, exist_ok=True)
        else:
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(item, dest)


def git_init(repo: Path) -> None:
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.email", "eval@eix.local"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.name", "eix eval"], cwd=repo, check=True)
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
    subprocess.run(
        ["git", "commit", "-qm", "seed", "--allow-empty"], cwd=repo, check=True
    )


def latest_session(db: Path, cwd: Path) -> dict | None:
    for _ in range(50):
        if db.exists():
            try:
                conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
                try:
                    row = conn.execute(
                        "SELECT id, name, status, mode, model FROM sessions "
                        "WHERE cwd = ? ORDER BY created_at DESC LIMIT 1",
                        (str(cwd),),
                    ).fetchone()
                finally:
                    conn.close()
                if row:
                    keys = ("id", "name", "status", "mode", "model")
                    return dict(zip(keys, row))
            except sqlite3.Error:
                pass
        time.sleep(0.05)
    return None


def read_events(db: Path, session_id: str) -> list[dict]:
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        rows = conn.execute(
            "SELECT seq, ts, kind, payload FROM events "
            "WHERE session_id = ? ORDER BY seq",
            (session_id,),
        ).fetchall()
    finally:
        conn.close()
    events = []
    for seq, ts, kind, payload in rows:
        try:
            data = json.loads(payload)
        except json.JSONDecodeError:
            data = {}
        events.append({"seq": seq, "ts": ts, "kind": kind, "payload": data})
    return events


def _tool_text(name: str, raw_args: str) -> str:
    """Return the free text a tool call carries, for cheap heuristics."""
    try:
        args = json.loads(raw_args)
    except json.JSONDecodeError:
        return ""
    if name == "apply_patch":
        return str(args.get("input") or "")
    if name in ("exec_command", "shell"):
        return str(args.get("cmd") or args.get("command") or "")
    return ""


def summarize_events(events: list[dict]) -> dict:
    kinds = Counter(event["kind"] for event in events)
    steps = []
    prompt = completion = cache_read = cache_creation = reasoning = 0
    tool_calls = 0
    tools = Counter()
    errors = []
    model = None
    tool_log: list[tuple[str, str]] = []
    for event in events:
        payload = event["payload"]
        if event["kind"] == "assistant_message":
            model = model or payload.get("model")
            usage = payload.get("usage") or {}
            step = {
                "seq": event["seq"],
                "prompt_tokens": usage.get("prompt_tokens", 0),
                "completion_tokens": usage.get("completion_tokens", 0),
                "cache_read_tokens": usage.get("cache_read_tokens", 0),
                "cache_creation_tokens": usage.get("cache_creation_tokens", 0),
                "reasoning_tokens": usage.get("reasoning_tokens", 0),
                "tool_calls": len(payload.get("tool_calls") or []),
                "chars": len(payload.get("text") or ""),
            }
            steps.append(step)
            prompt += step["prompt_tokens"]
            completion += step["completion_tokens"]
            cache_read += step["cache_read_tokens"]
            cache_creation += step["cache_creation_tokens"]
            reasoning += step["reasoning_tokens"]
        elif event["kind"] == "tool_call":
            tool_calls += 1
            name = payload.get("tool") or "?"
            tools[name] += 1
            args = payload.get("arguments") or {}
            tool_log.append((name, json.dumps(args, sort_keys=True)))
        elif event["kind"] == "error":
            errors.append(str(payload.get("message") or "")[:300])
    total_prompt = prompt
    uncached = max(0, prompt - cache_read)

    seen = Counter()
    redundant = 0
    scratch = 0
    verification = 0
    git_status = 0
    reads_before_edit = 0
    edited = False
    for name, raw in tool_log:
        if seen[(name, raw)]:
            redundant += 1
        seen[(name, raw)] += 1
        text = _tool_text(name, raw)
        lowered = text.lower()
        if (
            any(marker in lowered for marker in SCRATCH_MARKERS)
            or SCRATCH_PROGRAM.search(text)
            or DELETE_MARKER.search(text)
            or HEREDOC_WRITE.search(text)
        ):
            scratch += 1
        if name == "apply_patch":
            edited = True
            if re.search(r"\*\*\* Add File:\s*\S*(?:tests?/|tmp)", text):
                scratch += 1
        if "cargo test" in lowered or "cargo nextest" in lowered or "cargo check" in lowered:
            verification += 1
        if "git status" in lowered or "git diff" in lowered:
            git_status += 1
        if not edited and name in ("exec_command", "shell", "read_file", "list_files", "grep"):
            reads_before_edit += 1

    first_prompt = steps[0]["prompt_tokens"] if steps else 0
    first_hit = steps[0]["cache_read_tokens"] / first_prompt if first_prompt else 0.0

    plan_updates = 0
    plan_steps = 0
    plan_completed = 0
    for name, raw in tool_log:
        if name != "update_plan":
            continue
        plan_updates += 1
        try:
            plan = json.loads(raw).get("plan") or []
        except (json.JSONDecodeError, AttributeError):
            plan = []
        plan_steps = len(plan)
        plan_completed = sum(
            1
            for step in plan
            if isinstance(step, dict) and step.get("status") == "completed"
        )

    return {
        "model": model,
        "model_steps": len(steps),
        "steps_with_usage": sum(
            1
            for step in steps
            if step["prompt_tokens"] or step["completion_tokens"] or step["cache_read_tokens"]
        ),
        "usage_coverage": round(
            sum(
                1
                for step in steps
                if step["prompt_tokens"]
                or step["completion_tokens"]
                or step["cache_read_tokens"]
            )
            / len(steps),
            4,
        )
        if steps
        else 0.0,
        "tool_calls": tool_calls,
        "tools": dict(tools),
        "event_kinds": dict(kinds),
        "prompt_tokens": total_prompt,
        "completion_tokens": completion,
        "cache_read_tokens": cache_read,
        "cache_creation_tokens": cache_creation,
        "reasoning_tokens": reasoning,
        "uncached_prompt_tokens": uncached,
        "cache_hit_rate": round(cache_read / prompt, 4) if prompt else 0.0,
        "process": {
            "redundant_tool_calls": redundant,
            "scratch_writes": scratch,
            "verification_runs": verification,
            "git_status_calls": git_status,
            "reads_before_first_edit": reads_before_edit,
            "first_step_cache_hit": round(first_hit, 4),
            "plan_updates": plan_updates,
            "plan_steps": plan_steps,
            "plan_completed": plan_completed,
        },
        "errors": errors,
        "steps": steps,
    }


def remove_injected(paths: list[Path], repo: Path) -> None:
    """Delete injected hidden tests so a later run cannot read the answer key."""
    for path in paths:
        path.unlink(missing_ok=True)
        parent = path.parent
        while parent != repo and parent.is_dir() and not any(parent.iterdir()):
            parent.rmdir()
            parent = parent.parent


def final_assistant_text(events: list[dict]) -> str:
    """Last assistant message that actually wrote prose (not just a tool call)."""
    for event in reversed(events):
        if event["kind"] != "assistant_message":
            continue
        text = (event["payload"].get("text") or "").strip()
        if text:
            return text
    return ""


def strip_tests(source: str) -> str:
    """Drop a trailing `#[cfg(test)]` module so test helpers can use `unwrap`."""
    index = source.find("#[cfg(test)]")
    return source[:index] if index >= 0 else source


def _flatten_latex(text: str) -> str:
    """Remove LaTeX punctuation so notation variants compare equal.

    `x^{2}`, `x^2` and `x²` collapse together, as do `\\(n\\)-th` and `n-th`,
    `\\frac{\\varepsilon}{3}` and `fracvarepsilon3`, and the Unicode minus with
    an ASCII hyphen.
    """
    return (
        text.replace("\\(", "")
        .replace("\\)", "")
        .replace("\\[", "")
        .replace("\\]", "")
        .replace("\\", "")
        .replace("{", "")
        .replace("}", "")
        .replace("$", "")
        .replace("²", "^2")
        .replace("³", "^3")
        .replace("−", "-")
    )


def text_has(text: str, needle: str) -> bool:
    """Match a prose needle, tolerating LaTeX/markdown decoration."""
    text = text.lower()
    needle = needle.lower()
    if needle in text:
        return True
    flat_needle = _flatten_latex(needle)
    return len(flat_needle) >= 2 and flat_needle in _flatten_latex(text)


def evaluate_checks(
    meta: dict, repo: Path, changes: dict, events: list[dict], metrics: dict
) -> list[dict]:
    """Run the task's structural checks.

    Checks with `axis: correctness` gate the pass; the others only feed the
    score, so a stylistic instruction cannot hide a broken implementation.
    """
    process = metrics.get("process") or {}
    results = []
    for check in meta.get("checks") or []:
        kind = check.get("kind", "")
        ok = False
        detail = ""
        if kind == "files_unchanged":
            files = [
                path
                for path in changes.get("files") or []
                if path not in ("Cargo.lock",) and not path.startswith("target/")
            ]
            ok = not files
            detail = ", ".join(files)
        elif kind == "path_unchanged":
            path = check.get("path", "")
            changed = path in (changes.get("files") or [])
            ok = not changed
            detail = f"changed: {path}" if changed else ""
        elif kind == "text_all":
            text = final_assistant_text(events)
            missing = [needle for needle in check.get("needles", []) if not text_has(text, needle)]
            ok = not missing
            detail = "missing: " + ", ".join(missing) if missing else ""
        elif kind == "text_any_of":
            text = final_assistant_text(events)
            ok = any(text_has(text, needle) for needle in check.get("needles", []))
            detail = "" if ok else "none of: " + ", ".join(check.get("needles", []))
        elif kind == "source_contains":
            path = repo / check.get("path", "")
            source = path.read_text(errors="replace") if path.exists() else ""
            if check.get("ignore_tests", False):
                source = strip_tests(source)
            missing = [needle for needle in check.get("needles", []) if needle not in source]
            ok = not missing
            detail = "missing: " + ", ".join(missing) if missing else ""
        elif kind == "source_forbidden":
            path = repo / check.get("path", "")
            source = path.read_text(errors="replace") if path.exists() else ""
            if check.get("ignore_tests", True):
                source = strip_tests(source)
            hits = [pattern for pattern in check.get("patterns", []) if re.search(pattern, source)]
            ok = not hits
            detail = "found: " + ", ".join(hits) if hits else ""
        elif kind == "plan_used":
            steps = process.get("plan_steps", 0)
            completed = process.get("plan_completed", 0)
            ok = (
                process.get("plan_updates", 0) >= 1
                and steps >= check.get("min_steps", 1)
                and completed >= check.get("min_completed", steps)
            )
            detail = f"updates={process.get('plan_updates', 0)} steps={steps} completed={completed}"
        else:
            detail = f"unknown check: {kind}"
        results.append(
            {
                "kind": kind,
                "axis": check.get("axis", "correctness"),
                "ok": ok,
                "detail": detail,
            }
        )
    return results


def git_changes(repo: Path) -> dict:
    status = subprocess.run(
        ["git", "status", "--porcelain"], cwd=repo, capture_output=True, text=True
    )
    files = [line[3:] for line in status.stdout.splitlines() if line.strip()]
    stat = subprocess.run(
        ["git", "diff", "--stat"], cwd=repo, capture_output=True, text=True
    )
    return {"files": files, "diff_stat": stat.stdout.strip()}


def run_task(task_dir: Path, args: argparse.Namespace, repeat: int) -> dict:
    meta = json.loads((task_dir / "task.json").read_text())
    # Parallel shards start in the same second, so the pid keeps their run
    # directories distinct; the stamp keeps runs sortable.
    run_id = f"{now_stamp()}-{meta['id']}-r{repeat}-p{os.getpid()}"
    root = Path(args.workdir) / run_id
    repo = root / "repo"
    data_dir = root / "data"
    socket_path = root / "eix.sock"
    root.mkdir(parents=True, exist_ok=True)
    repo.mkdir(parents=True)
    copy_tree(task_dir / "template", repo)
    git_init(repo)

    eix = Path(args.bin_dir) / "eix"
    eixd = Path(args.bin_dir) / "eixd"
    if not eix.exists() or not eixd.exists():
        raise RuntimeError(f"missing binaries in {args.bin_dir}: build with `cargo build`")

    env = os.environ.copy()
    env.update(
        {
            "EIX_SOCKET": str(socket_path),
            "EIX_DATA_DIR": str(data_dir),
            "EIX_CONFIG": str(Path(args.config).expanduser()),
            "NO_COLOR": "1",
            "TERM": "dumb",
        }
    )

    daemon_log = open(root / "eixd.log", "w")
    daemon = subprocess.Popen(
        [
            str(eixd),
            "--socket",
            str(socket_path),
            "--data-dir",
            str(data_dir),
            "--config",
            str(Path(args.config).expanduser()),
        ],
        cwd=repo,
        env=env,
        stdout=daemon_log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )

    result = {
        "task": meta["id"],
        "category": meta.get("category") or "agentic",
        "planning": bool(meta.get("planning")),
        "repeat": repeat,
        "run_id": run_id,
        "repo": str(repo),
        "prompt": meta["prompt"],
        "passed": False,
        "verify_exit": None,
        "verify_output": "",
        "send": None,
        "session": None,
        "metrics": {},
        "changes": {},
        "editable": meta.get("editable") or [],
        "budgets": meta.get("budgets") or {},
        "daemon_log": "",
    }
    injected: list[Path] = []

    try:
        wait_for_socket(socket_path, timeout=args.daemon_timeout)
        create = run(
            [
                str(eix),
                "new",
                "--shared",
                "--mode",
                "full",
                "--cwd",
                str(repo),
                "--name",
                meta["id"],
            ],
            cwd=repo,
            env=env,
            timeout=60,
        )
        if create["exit"] != 0:
            result["send"] = {"phase": "new", **create}
            return result

        session = latest_session(data_dir / "eix.sqlite3", repo)
        if not session:
            result["send"] = {"phase": "new", "error": "session was not persisted"}
            return result
        result["session"] = session

        send = run(
            [str(eix), "send", "--session", session["id"], meta["prompt"]],
            cwd=repo,
            env=env,
            timeout=args.timeout,
        )
        result["send"] = send
        result["changes"] = git_changes(repo)
        result["duration_s"] = send["duration_s"]

        events = read_events(data_dir / "eix.sqlite3", session["id"])
        result["metrics"] = summarize_events(events)
        result["model"] = result["metrics"].get("model") or session.get("model")
        # Structural checks look at the agent's own output, so they must run
        # before hidden tests are copied in.
        result["checks"] = evaluate_checks(
            meta, repo, result["changes"], events, result["metrics"]
        )

        if (task_dir / "hidden").is_dir():
            hidden_root = task_dir / "hidden"
            injected = [
                repo / item.relative_to(hidden_root)
                for item in hidden_root.rglob("*")
                if item.is_file()
            ]
            copy_tree(hidden_root, repo)

        verify_cmd = meta.get("verify")
        if verify_cmd:
            verify = run(verify_cmd, cwd=repo, env=env, timeout=args.verify_timeout)
            result["verify_exit"] = verify["exit"]
            result["verify_output"] = (verify["stdout"] + verify["stderr"])[-8000:]
            result["verify_duration_s"] = verify["duration_s"]
            verify_ok = verify["exit"] == 0
        else:
            verify_ok = True

        gating = [
            check
            for check in result["checks"]
            if check["axis"] == "correctness"
        ]
        result["passed"] = verify_ok and all(check["ok"] for check in gating)
        result["score"] = scoring.score_run(result)
        return result
    except Exception as exc:  # keep the run report even when the harness fails
        result["harness_error"] = f"{type(exc).__name__}: {exc}"
        return result
    finally:
        remove_injected(injected, repo)
        daemon.terminate()
        try:
            daemon.wait(timeout=8)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=5)
        daemon_log.close()
        log_path = root / "eixd.log"
        if log_path.exists():
            result["daemon_log"] = log_path.read_text(errors="replace")[-6000:]
        # Each run leaves a repo plus its cargo target behind. At 50+ tasks
        # that fills /tmp, so drop the work tree unless explicitly kept.
        if not getattr(args, "keep_workdir", False):
            shutil.rmtree(root, ignore_errors=True)


def aggregate(results: list[dict]) -> dict:
    runs = [r for r in results if "metrics" in r and r["metrics"]]
    passed = [r for r in results if r["passed"]]
    prompt = sum(r["metrics"].get("prompt_tokens", 0) for r in runs)
    completion = sum(r["metrics"].get("completion_tokens", 0) for r in runs)
    cache_read = sum(r["metrics"].get("cache_read_tokens", 0) for r in runs)
    steps = sum(r["metrics"].get("model_steps", 0) for r in runs)
    steps_with_usage = sum(r["metrics"].get("steps_with_usage", 0) for r in runs)
    return {
        "runs": len(results),
        "passed": len(passed),
        "pass_rate": round(len(passed) / len(results), 4) if results else 0.0,
        "total_prompt_tokens": prompt,
        "total_completion_tokens": completion,
        "total_tokens": prompt + completion,
        "total_cache_read_tokens": cache_read,
        "usage_coverage": round(steps_with_usage / steps, 4) if steps else 0.0,
        "cache_hit_rate": round(cache_read / prompt, 4) if prompt else 0.0,
        "mean_duration_s": round(
            sum(r.get("duration_s", 0) for r in results) / len(results), 3
        )
        if results
        else 0.0,
        "mean_model_steps": round(
            sum(r["metrics"].get("model_steps", 0) for r in runs) / len(runs), 2
        )
        if runs
        else 0.0,
        "mean_tool_calls": round(
            sum(r["metrics"].get("tool_calls", 0) for r in runs) / len(runs), 2
        )
        if runs
        else 0.0,
        "score": (
            scoring.score_report({"results": results})
        ),
    }


def markdown_report(results: list[dict], summary: dict, args: argparse.Namespace) -> str:
    lines = [
        "# eix agent eval",
        "",
        f"- model: `{results[0].get('model') if results else '?'}`",
        f"- binary: `{args.bin_dir}`",
        f"- tasks: {', '.join(sorted({r['task'] for r in results}))}",
        f"- repeats: {args.repeats}",
        "",
        "## Summary",
        "",
        f"- score: **{summary['score']['mean_score']:.1f}** / 100",
        f"- pass: **{summary['passed']}/{summary['runs']}** ({summary['pass_rate']:.0%})",
        f"- axes: "
        + ", ".join(
            f"{axis} {value * 100:.0f}"
            for axis, value in summary["score"]["axes"].items()
        ),
        f"- mean duration: **{summary['mean_duration_s']:.1f}s**",
        f"- mean model steps: **{summary['mean_model_steps']:.2f}**",
        f"- mean tool calls: **{summary['mean_tool_calls']:.2f}**",
        f"- prompt tokens: **{summary['total_prompt_tokens']}**",
        f"- completion tokens: **{summary['total_completion_tokens']}**",
        f"- usage coverage: **{summary['usage_coverage']:.0%}** of model steps",
        f"- cache hit rate: **{summary['cache_hit_rate']:.1%}**",
        "",
        "## Runs",
        "",
        "| task | category | pass | score | time | steps | tools | prompt | completion | cache hit |",
        "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for result in results:
        metrics = result.get("metrics") or {}
        cache = (
            f"{metrics.get('cache_hit_rate', 0.0):.0%}"
            if metrics.get("prompt_tokens")
            else "n/a"
        )
        score = result.get("score") or {}
        lines.append(
            "| {task} | {category} | {passed} | {score} | {time:.1f}s | {steps} | {tools} | {prompt} | {completion} | {cache} |".format(
                task=result["task"],
                category=result.get("category") or "agentic",
                passed="yes" if result["passed"] else "no",
                score=f"{score.get('total', 0):.0f} {score.get('grade', '-')}",
                time=result.get("duration_s", 0),
                steps=metrics.get("model_steps", 0),
                tools=metrics.get("tool_calls", 0),
                prompt=metrics.get("prompt_tokens", 0),
                completion=metrics.get("completion_tokens", 0),
                cache=cache,
            )
        )
    lines += ["", "## Failures", ""]
    failed = [r for r in results if not r["passed"]]
    if not failed:
        lines.append("None.")
    else:
        for result in failed:
            lines.append(f"### {result['task']} (repeat {result['repeat']})")
            lines.append("")
            if result.get("harness_error"):
                lines.append(f"harness error: `{result['harness_error']}`")
            if result.get("send"):
                send = result["send"]
                lines.append(f"- send exit: `{send.get('exit')}` timed_out=`{send.get('timed_out')}`")
                if send.get("stderr"):
                    lines.append("```\n" + send["stderr"][-2000:] + "\n```")
            if result["metrics"].get("errors"):
                lines.append("- errors: " + "; ".join(result["metrics"]["errors"]))
            if result.get("verify_output"):
                lines.append("```\n" + result["verify_output"][-3000:] + "\n```")
            if result.get("daemon_log"):
                lines.append("daemon log tail:")
                lines.append("```\n" + result["daemon_log"][-1500:] + "\n```")
            lines.append("")
    return "\n".join(lines) + "\n"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--tasks",
        default="",
        help="comma-separated task ids (default: every directory under tasks/)",
    )
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--bin-dir", default=str(ROOT / "target" / "debug"))
    parser.add_argument("--config", default=str(DEFAULT_CONFIG))
    parser.add_argument("--workdir", default="/tmp/eix-agent-eval")
    parser.add_argument(
        "--output",
        default=str(ROOT / "evals" / "agent" / "reports" / "latest.json"),
    )
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument(
        "--keep-workdir",
        action="store_true",
        help="keep each run's repo and daemon data under --workdir",
    )
    parser.add_argument("--verify-timeout", type=float, default=180.0)
    parser.add_argument("--daemon-timeout", type=float, default=20.0)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not Path(args.config).expanduser().exists():
        eprint(f"config not found: {args.config}")
        return 2
    task_ids = [item for item in args.tasks.split(",") if item]
    task_dirs = (
        [TASKS_DIR / item for item in task_ids]
        if task_ids
        else sorted(path for path in TASKS_DIR.iterdir() if path.is_dir())
    )
    for task_dir in task_dirs:
        if not (task_dir / "task.json").exists():
            eprint(f"task missing task.json: {task_dir}")
            return 2

    results = []
    for task_dir in task_dirs:
        for repeat in range(1, args.repeats + 1):
            eprint(f"==> {task_dir.name} repeat {repeat}/{args.repeats}")
            result = run_task(task_dir, args, repeat)
            score = result.get("score") or {}
            eprint(
                "    {status} score {score} in {time:.1f}s ({steps} steps, {tools} tools, {tokens} tokens)".format(
                    status="PASS" if result["passed"] else "FAIL",
                    score=f"{score.get('total', 0):.0f}{score.get('grade', '')}",
                    time=result.get("duration_s", 0),
                    steps=result.get("metrics", {}).get("model_steps", 0),
                    tools=result.get("metrics", {}).get("tool_calls", 0),
                    tokens=result.get("metrics", {}).get("prompt_tokens", 0)
                    + result.get("metrics", {}).get("completion_tokens", 0),
                )
            )
            results.append(result)

    summary = aggregate(results)
    report = {
        "generated_at": now_stamp(),
        "args": vars(args),
        "summary": summary,
        "results": results,
    }
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    md_path = output.with_suffix(".md")
    md_path.write_text(markdown_report(results, summary, args))
    eprint(f"\nwrote {output} and {md_path}")
    eprint(json.dumps(summary, indent=2))
    return 0 if summary["passed"] == summary["runs"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
