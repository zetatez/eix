#!/usr/bin/env python3
"""Compare two eval reports task by task.

Aggregate numbers hide regressions: a cheaper run can look better while one
task got slower or started failing. This prints per-task pass/score deltas and
flags the two cases that matter: a lost pass, and a change that is within
normal model noise.

Usage:
    python3 evals/agent/compare.py reports/baseline.json reports/iter.json
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import score as scoring

# A single repeat is noisy; below this many points the delta is not a signal.
NOISE_POINTS = 2.0


def load(name: str) -> dict:
    report = json.loads(Path(name).read_text())
    tasks: dict[str, list[dict]] = {}
    for result in report["results"]:
        # Re-score with the current rules so reports produced by an older
        # version of score.py are still comparable.
        result["score"] = scoring.score_run(result)
        tasks.setdefault(result["task"], []).append(result)
    return {
        "label": Path(name).stem,
        "report": report,
        "tasks": tasks,
        "summary": scoring.score_report({"results": report["results"]}),
    }


def task_summary(runs: list[dict]) -> dict:
    if not runs:
        return {"runs": 0, "passed": 0, "score": 0.0, "steps": 0.0, "tokens": 0.0, "cache": 0.0}
    scores = [r.get("score", {}).get("total", 0.0) for r in runs]
    return {
        "runs": len(runs),
        "passed": sum(1 for r in runs if r.get("passed")),
        "score": sum(scores) / len(scores) if scores else 0.0,
        "steps": sum(r.get("metrics", {}).get("model_steps", 0) for r in runs)
        / len(runs),
        "tokens": sum(
            r.get("metrics", {}).get("prompt_tokens", 0)
            + r.get("metrics", {}).get("completion_tokens", 0)
            for r in runs
        )
        / len(runs),
        "cache": sum(r.get("metrics", {}).get("cache_hit_rate", 0.0) for r in runs)
        / len(runs),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before")
    parser.add_argument("after")
    parser.add_argument(
        "--min-delta",
        type=float,
        default=NOISE_POINTS,
        help="score delta treated as noise (default %(default)s)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    before = load(args.before)
    after = load(args.after)
    names = sorted(set(before["tasks"]) | set(after["tasks"]))

    print(f"# {before['label']} -> {after['label']}")
    print()
    print("| task | pass | score | steps | tokens | cache |")
    print("| --- | --- | ---: | ---: | ---: | ---: |")
    regressions: list[str] = []
    for name in names:
        old = task_summary(before["tasks"].get(name, []))
        new = task_summary(after["tasks"].get(name, []))
        if not old["runs"] or not new["runs"]:
            print(f"| {name} | n/a | n/a | n/a | n/a | n/a |")
            continue
        pass_delta = (
            "same"
            if old["passed"] == new["passed"]
            else f"{old['passed']}->{new['passed']}"
        )
        if new["passed"] < old["passed"]:
            regressions.append(f"{name}: lost a passing repeat")
        delta = new["score"] - old["score"]
        if old["passed"] == new["passed"] and abs(delta) < args.min_delta:
            delta_text = f"{delta:+.1f} noise"
        else:
            delta_text = f"{new['score']:.1f} ({delta:+.1f})"
        print(
            f"| {name} | {pass_delta} | {delta_text} | "
            f"{old['steps']:.2f}->{new['steps']:.2f} | "
            f"{old['tokens']:.0f}->{new['tokens']:.0f} | "
            f"{old['cache']:.0%}->{new['cache']:.0%} |"
        )

    old_axes = before["summary"].get("axes", {})
    new_axes = after["summary"].get("axes", {})
    print()
    print("## Axes")
    print()
    print("| axis | before | after | delta |")
    print("| --- | ---: | ---: | ---: |")
    for axis in sorted(set(old_axes) | set(new_axes)):
        old = old_axes.get(axis)
        new = new_axes.get(axis)
        if old is None or new is None:
            print(f"| {axis} | {old or 'n/a'} | {new or 'n/a'} | n/a |")
            continue
        print(f"| {axis} | {old * 100:.0f} | {new * 100:.0f} | {(new - old) * 100:+.0f} |")

    if regressions:
        print()
        print("## Regressions")
        print()
        for line in regressions:
            print(f"- {line}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
