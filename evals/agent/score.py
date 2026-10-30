#!/usr/bin/env python3
"""Turn raw eval runs into a multi-dimensional score.

One number hides where the agent is weak, so a run is graded on seven axes:

- correctness  35  the hidden tests / response checks pass
- adherence    15  the diff stays in scope, no scratch work, no banned patterns
- planning      8  the plan was used and finished (planning tasks only)
- loops        12  model steps against the task budget
- tokens       12  prompt + completion tokens against the task budget
- time         10  wall seconds against the task budget
- cache         8  provider prefix-cache reuse, overall and on step one

Wall time, tokens and loop count are separate axes on purpose: an agent can be
fast and still burn tokens, or cheap and still loop. `planning` only scores
tasks that ask for a plan; on every other task it is dropped and the weights
are renormalised.

Budgets live in `task.json` so a tiny task and a multi-file task are not
compared on the same absolute numbers:

    "budgets": {"steps": [4, 12], "tokens": [30000, 120000], "seconds": [20, 120]}

The pair is `[target, worst]`. At or below the target the component scores 1;
at or above worst it scores 0; between them it falls linearly.

Usage:
    python3 evals/agent/score.py reports/iter-final-prompt.json
    python3 evals/agent/score.py reports/a.json reports/b.json
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

WEIGHTS = {
    "correctness": 35.0,
    "adherence": 15.0,
    "planning": 8.0,
    "loops": 12.0,
    "tokens": 12.0,
    "time": 10.0,
    "cache": 8.0,
}

DEFAULT_BUDGETS = {
    "steps": (4, 12),
    "tokens": (30000, 120000),
    "seconds": (20, 120),
}

# Files that are never an agent edit and must not count against it.
IGNORED_PATHS = ("Cargo.lock", "target/", "Cargo.toml")


def clamp01(value: float) -> float:
    return max(0.0, min(1.0, value))


def scale(value: float, target: float, worst: float) -> float:
    """1 at or below target, 0 at or above worst, linear between."""
    if worst <= target:
        return 1.0 if value <= target else 0.0
    if value <= target:
        return 1.0
    if value >= worst:
        return 0.0
    return 1.0 - (value - target) / (worst - target)


def budgets_for(meta: dict) -> dict:
    raw = meta.get("budgets") or {}
    out = {}
    for key, default in DEFAULT_BUDGETS.items():
        pair = raw.get(key) or default
        out[key] = (float(pair[0]), float(pair[1]))
    return out


def unexpected_changes(result: dict) -> list[str]:
    editable = set(result.get("editable") or [])
    changed = (result.get("changes") or {}).get("files") or []
    out = []
    for path in changed:
        if path in IGNORED_PATHS or path.startswith("target/"):
            continue
        if path in editable:
            continue
        out.append(path)
    return out


def check_ratio(result: dict, axis: str) -> float | None:
    checks = [c for c in result.get("checks") or [] if c.get("axis") == axis]
    if not checks:
        return None
    return sum(1.0 if c.get("ok") else 0.0 for c in checks) / len(checks)


def process_signals(result: dict) -> dict:
    metrics = result.get("metrics") or {}
    process = metrics.get("process") or {}
    tool_calls = max(1, metrics.get("tool_calls") or 0)
    return {
        "tool_calls": tool_calls,
        "redundant": process.get("redundant_tool_calls", 0),
        "scratch": process.get("scratch_writes", 0),
        "verify_runs": process.get("verification_runs", 0),
        "git_status": process.get("git_status_calls", 0),
        "first_step_hit": process.get("first_step_cache_hit", 0.0),
        "plan_updates": process.get("plan_updates", 0),
    }


def score_run(result: dict) -> dict:
    metrics = result.get("metrics") or {}
    budgets = budgets_for(result)
    signals = process_signals(result)
    # Reports written before per-task editable lists existed cannot tell an
    # expected edit from a stray one, so they are not penalised for it.
    unexpected = unexpected_changes(result) if "editable" in result else []
    passed = bool(result.get("passed"))

    # --- correctness -----------------------------------------------------
    correctness = 1.0 if passed else 0.0

    # --- adherence -------------------------------------------------------
    clean = 1.0 - min(1.0, len(unexpected) / 2.0)
    scratch = 1.0 / (1.0 + signals["scratch"])
    constraints = check_ratio(result, "adherence")
    if constraints is None:
        constraints = 1.0
    adherence = 0.4 * clean + 0.3 * scratch + 0.3 * constraints

    # --- planning --------------------------------------------------------
    planning = check_ratio(result, "planning")
    if planning is None and result.get("planning"):
        planning = 0.0
    if planning is not None and signals["plan_updates"] == 0:
        planning = 0.0

    # --- loops / tokens / time ------------------------------------------
    tokens = (metrics.get("prompt_tokens") or 0) + (metrics.get("completion_tokens") or 0)
    loops = scale(metrics.get("model_steps") or 0, *budgets["steps"])
    token_axis = scale(tokens, *budgets["tokens"])
    time_axis = scale(result.get("duration_s") or 0.0, *budgets["seconds"])

    # --- cache -----------------------------------------------------------
    cache_hit = float(metrics.get("cache_hit_rate") or 0.0)
    cache = 0.7 * cache_hit + 0.3 * float(signals["first_step_hit"])

    components = {
        "correctness": correctness,
        "adherence": adherence,
        "loops": loops,
        "tokens": token_axis,
        "time": time_axis,
        "cache": cache,
    }
    if planning is not None:
        components["planning"] = planning

    weights = dict(WEIGHTS)
    if planning is None:
        # Old reports have no plan data, and non-planning tasks don't ask for
        # one. Drop the axis instead of inventing a perfect score.
        weights.pop("planning")
    if "process" not in metrics:
        # Legacy reports have no tool-call data; adherence cannot be judged.
        weights.pop("adherence")
        components.pop("adherence")
    total = sum(weights[name] * value for name, value in components.items())
    total = total / sum(weights.values()) * 100.0
    if not passed:
        # A wrong answer is not partially efficient. Keep the number for
        # diagnostics but make the ranking unambiguous.
        total = min(total, 40.0)

    return {
        "total": round(total, 1),
        "grade": grade(total),
        "passed": passed,
        "components": {name: round(value, 3) for name, value in components.items()},
        "detail": {
            "unexpected_changes": unexpected,
            "clean_diff": round(clean, 3),
            "scratch_writes": signals["scratch"],
            "redundant_tool_calls": signals["redundant"],
            "verification_runs": signals["verify_runs"],
            "git_status_calls": signals["git_status"],
            "plan_updates": signals["plan_updates"],
            "failed_checks": [
                c.get("kind") for c in result.get("checks") or [] if not c.get("ok")
            ],
            "tokens": tokens,
            "steps": metrics.get("model_steps") or 0,
            "seconds": result.get("duration_s") or 0.0,
            "cache_hit_rate": round(cache_hit, 3),
            "first_step_cache_hit": round(float(signals["first_step_hit"]), 3),
            "budgets": {k: list(v) for k, v in budgets.items()},
        },
    }


def grade(total: float) -> str:
    if total >= 90:
        return "A"
    if total >= 80:
        return "B"
    if total >= 70:
        return "C"
    if total >= 60:
        return "D"
    return "F"


def score_report(report: dict) -> dict:
    results = report.get("results") or []
    for result in results:
        # Always recompute so a report scored by an older version of this
        # module is compared on the current rules.
        result["score"] = score_run(result)

    by_axis = {}
    for axis in WEIGHTS:
        values = [
            r["score"]["components"][axis]
            for r in results
            if r.get("score") and axis in r["score"]["components"]
        ]
        if values:
            by_axis[axis] = round(sum(values) / len(values), 3)

    tasks: dict[str, list[dict]] = {}
    categories: dict[str, list[dict]] = {}
    for result in results:
        tasks.setdefault(result["task"], []).append(result)
        categories.setdefault(result.get("category") or "agentic", []).append(result)

    pass_all = sum(
        1 for runs in tasks.values() if runs and all(r.get("passed") for r in runs)
    )
    return {
        "runs": len(results),
        "passed": sum(1 for r in results if r.get("passed")),
        "pass_rate": round(
            sum(1 for r in results if r.get("passed")) / len(results), 4
        )
        if results
        else 0.0,
        "pass_all_tasks": f"{pass_all}/{len(tasks)}",
        "mean_score": round(
            sum(r["score"]["total"] for r in results if r.get("score")) / len(results), 1
        )
        if results
        else 0.0,
        "axes": by_axis,
        "tasks": {
            task: {
                "runs": len(runs),
                "passed": sum(1 for r in runs if r.get("passed")),
                "mean_score": round(
                    sum(r["score"]["total"] for r in runs) / len(runs), 1
                ),
            }
            for task, runs in tasks.items()
        },
        "categories": {
            category: {
                "runs": len(runs),
                "passed": sum(1 for r in runs if r.get("passed")),
                "mean_score": round(
                    sum(r["score"]["total"] for r in runs) / len(runs), 1
                ),
            }
            for category, runs in categories.items()
        },
    }


def markdown(summary: dict, label: str) -> str:
    axes = summary["axes"]
    lines = [
        f"## {label}",
        "",
        f"- score: **{summary['mean_score']:.1f}** / 100",
        f"- pass: **{summary['passed']}/{summary['runs']}** "
        f"(all repeats pass: {summary['pass_all_tasks']})",
        "",
        "| axis | score |",
        "| --- | ---: |",
    ]
    for axis in WEIGHTS:
        if axis not in axes:
            continue
        lines.append(f"| {axis} | {axes[axis] * 100:.0f} |")
    lines += ["", "| task | pass | score |", "| --- | --- | ---: |"]
    for task, info in sorted(summary["tasks"].items()):
        lines.append(
            f"| {task} | {info['passed']}/{info['runs']} | {info['mean_score']:.1f} |"
        )
    lines += ["", "| category | pass | score |", "| --- | --- | ---: |"]
    for category, info in sorted(summary["categories"].items()):
        lines.append(
            f"| {category} | {info['passed']}/{info['runs']} | "
            f"{info['mean_score']:.1f} |"
        )
    return "\n".join(lines) + "\n"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", help="report .json files")
    parser.add_argument("--json", action="store_true", help="print the summary as JSON")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    for name in args.reports:
        report = json.loads(Path(name).read_text())
        summary = score_report(report)
        label = Path(name).stem
        if args.json:
            print(json.dumps({"report": label, **summary}, indent=2))
        else:
            print(markdown(summary, label))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
