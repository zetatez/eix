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
import shutil
import socket
import sqlite3
import subprocess
import sys
import time
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TASKS_DIR = Path(__file__).resolve().parent / "tasks"
DEFAULT_CONFIG = Path.home() / ".config" / "eix" / "config.toml"


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
    subprocess.run(["git", "commit", "-qm", "seed"], cwd=repo, check=True)


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


def summarize_events(events: list[dict]) -> dict:
    kinds = Counter(event["kind"] for event in events)
    steps = []
    prompt = completion = cache_read = cache_creation = reasoning = 0
    tool_calls = 0
    tools = Counter()
    errors = []
    model = None
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
            tools[payload.get("tool") or "?"] += 1
        elif event["kind"] == "error":
            errors.append(str(payload.get("message") or "")[:300])
    total_prompt = prompt
    uncached = max(0, prompt - cache_read)
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
        "errors": errors,
        "steps": steps,
    }


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
    run_id = f"{now_stamp()}-{meta['id']}-r{repeat}"
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
        "daemon_log": "",
    }

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

        if (task_dir / "hidden").is_dir():
            copy_tree(task_dir / "hidden", repo)

        verify_cmd = meta.get("verify", ["cargo", "test", "--quiet"])
        verify = run(verify_cmd, cwd=repo, env=env, timeout=args.verify_timeout)
        result["verify_exit"] = verify["exit"]
        result["verify_output"] = (verify["stdout"] + verify["stderr"])[-8000:]
        result["verify_duration_s"] = verify["duration_s"]
        result["passed"] = verify["exit"] == 0

        events = read_events(data_dir / "eix.sqlite3", session["id"])
        result["metrics"] = summarize_events(events)
        result["duration_s"] = send["duration_s"]
        result["model"] = result["metrics"].get("model") or session.get("model")
        return result
    except Exception as exc:  # keep the run report even when the harness fails
        result["harness_error"] = f"{type(exc).__name__}: {exc}"
        return result
    finally:
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
        f"- pass: **{summary['passed']}/{summary['runs']}** ({summary['pass_rate']:.0%})",
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
        "| task | pass | time | steps | tools | prompt | completion | cache hit |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for result in results:
        metrics = result.get("metrics") or {}
        cache = (
            f"{metrics.get('cache_hit_rate', 0.0):.0%}"
            if metrics.get("prompt_tokens")
            else "n/a"
        )
        lines.append(
            "| {task} | {passed} | {time:.1f}s | {steps} | {tools} | {prompt} | {completion} | {cache} |".format(
                task=result["task"],
                passed="yes" if result["passed"] else "no",
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
            eprint(
                "    {status} in {time:.1f}s ({steps} steps, {tools} tools, {tokens} tokens)".format(
                    status="PASS" if result["passed"] else "FAIL",
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
