#!/usr/bin/env python3
"""Unit tests for the scoring math. No third-party deps.

    python3 evals/agent/test_score.py
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import run_eval
import score


def make_result(**overrides) -> dict:
    result = {
        "task": "demo",
        "category": "agentic-coding",
        "repeat": 1,
        "passed": True,
        "duration_s": 20.0,
        "editable": ["src/lib.rs"],
        "budgets": {"steps": [4, 12], "tokens": [20000, 100000], "seconds": [20, 100]},
        "changes": {"files": ["src/lib.rs"]},
        "checks": [],
        "metrics": {
            "model_steps": 4,
            "tool_calls": 3,
            "prompt_tokens": 18000,
            "completion_tokens": 1000,
            "cache_hit_rate": 0.8,
            "process": {
                "redundant_tool_calls": 0,
                "scratch_writes": 0,
                "verification_runs": 1,
                "git_status_calls": 0,
                "first_step_cache_hit": 0.8,
                "plan_updates": 0,
            },
        },
    }
    result.update(overrides)
    return result


class ScaleTests(unittest.TestCase):
    def test_below_target_is_full(self):
        self.assertEqual(score.scale(3, 4, 12), 1.0)

    def test_at_worst_is_zero(self):
        self.assertEqual(score.scale(12, 4, 12), 0.0)

    def test_linear_between(self):
        self.assertAlmostEqual(score.scale(8, 4, 12), 0.5)

    def test_degenerate_budget_does_not_divide_by_zero(self):
        self.assertEqual(score.scale(5, 4, 4), 0.0)


class RunScoreTests(unittest.TestCase):
    def test_perfect_run_scores_high(self):
        result = score.score_run(make_result())
        self.assertGreaterEqual(result["total"], 95)
        self.assertEqual(result["grade"], "A")

    def test_failed_run_is_capped(self):
        result = score.score_run(make_result(passed=False))
        self.assertLessEqual(result["total"], 40)

    def test_unexpected_file_costs_adherence(self):
        result = score.score_run(
            make_result(changes={"files": ["src/lib.rs", "tests/hidden.rs"]})
        )
        self.assertLess(result["components"]["adherence"], 1.0)
        self.assertIn("tests/hidden.rs", result["detail"]["unexpected_changes"])

    def test_generated_files_are_ignored(self):
        result = score.score_run(
            make_result(changes={"files": ["src/lib.rs", "Cargo.lock", "target/x"]})
        )
        self.assertEqual(result["detail"]["unexpected_changes"], [])

    def test_scratch_writes_lower_adherence(self):
        clean = score.score_run(make_result())
        metrics = make_result()["metrics"]
        metrics["process"] = {**metrics["process"], "scratch_writes": 3}
        dirty = score.score_run(make_result(metrics=metrics))
        self.assertLess(
            dirty["components"]["adherence"], clean["components"]["adherence"]
        )

    def test_failed_adherence_check_lowers_adherence(self):
        checks = [{"kind": "source_forbidden", "axis": "adherence", "ok": False}]
        result = score.score_run(make_result(checks=checks))
        self.assertLess(result["components"]["adherence"], 1.0)
        self.assertIn("source_forbidden", result["detail"]["failed_checks"])

    def test_loops_tokens_and_time_are_separate_axes(self):
        fast = score.score_run(make_result())
        slow = score.score_run(make_result(duration_s=95.0))
        wordy = score.score_run(
            make_result(
                metrics={
                    **make_result()["metrics"],
                    "prompt_tokens": 95000,
                    "completion_tokens": 4000,
                }
            )
        )
        loopy = score.score_run(
            make_result(metrics={**make_result()["metrics"], "model_steps": 11})
        )
        self.assertLess(slow["components"]["time"], fast["components"]["time"])
        self.assertLess(wordy["components"]["tokens"], fast["components"]["tokens"])
        self.assertLess(loopy["components"]["loops"], fast["components"]["loops"])
        self.assertEqual(wordy["components"]["time"], fast["components"]["time"])

    def test_planning_axis_only_when_checked(self):
        plain = score.score_run(make_result())
        self.assertNotIn("planning", plain["components"])

        planning_checks = [
            {"kind": "plan_used", "axis": "planning", "ok": True}
        ]
        planned = make_result(planning=True, checks=planning_checks)
        planned["metrics"] = {
            **planned["metrics"],
            "process": {**planned["metrics"]["process"], "plan_updates": 1},
        }
        result = score.score_run(planned)
        self.assertEqual(result["components"]["planning"], 1.0)

    def test_missing_plan_fails_the_planning_axis(self):
        checks = [{"kind": "plan_used", "axis": "planning", "ok": False}]
        result = score.score_run(make_result(planning=True, checks=checks))
        self.assertEqual(result["components"]["planning"], 0.0)

    def test_legacy_report_without_process_drops_data_axes(self):
        result = score.score_run(
            make_result(metrics={"model_steps": 4, "prompt_tokens": 1000})
        )
        self.assertNotIn("adherence", result["components"])
        self.assertNotIn("planning", result["components"])
        self.assertIn("correctness", result["components"])


class ReportTests(unittest.TestCase):
    def test_axes_pass_all_and_categories(self):
        runs = [
            ("demo", "agentic-coding", score.score_run(make_result())),
            ("demo", "agentic-coding", score.score_run(make_result(repeat=2))),
            ("other", "math", score.score_run(make_result(repeat=1))),
        ]
        report = {
            "results": [
                {**result, "task": task, "category": category}
                for task, category, result in runs
            ]
        }
        summary = score.score_report(report)
        self.assertEqual(summary["pass_rate"], 1.0)
        self.assertEqual(summary["pass_all_tasks"], "2/2")
        self.assertIn("correctness", summary["axes"])
        self.assertEqual(summary["categories"]["math"]["runs"], 1)


class CheckTests(unittest.TestCase):
    """Structural checks: correctness gates the pass, the rest only score."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def run_checks(self, checks, changes=None, events=None, process=None):
        meta = {"checks": checks}
        metrics = {"process": process or {}}
        return run_eval.evaluate_checks(
            meta, self.repo, changes or {"files": []}, events or [], metrics
        )

    def test_files_unchanged_passes_clean_tree(self):
        [check] = self.run_checks(
            [{"kind": "files_unchanged", "axis": "correctness"}],
            changes={"files": ["Cargo.lock", "target/debug/x"]},
        )
        self.assertTrue(check["ok"])

    def test_files_unchanged_fails_on_edit(self):
        [check] = self.run_checks(
            [{"kind": "files_unchanged", "axis": "correctness"}],
            changes={"files": ["src/lib.rs"]},
        )
        self.assertFalse(check["ok"])
        self.assertIn("src/lib.rs", check["detail"])

    def test_path_unchanged_only_cares_about_that_path(self):
        [ok] = self.run_checks(
            [{"kind": "path_unchanged", "path": "Cargo.toml"}],
            changes={"files": ["src/lib.rs"]},
        )
        [bad] = self.run_checks(
            [{"kind": "path_unchanged", "path": "Cargo.toml"}],
            changes={"files": ["Cargo.toml"]},
        )
        self.assertTrue(ok["ok"])
        self.assertFalse(bad["ok"])

    def test_text_all_requires_every_needle(self):
        events = [{"kind": "assistant_message", "payload": {"text": "parse_v2 is fine"}}]
        [ok] = self.run_checks(
            [{"kind": "text_all", "needles": ["parse_v2"]}], events=events
        )
        [bad] = self.run_checks(
            [{"kind": "text_all", "needles": ["parse_v2", "fixed"]}], events=events
        )
        self.assertTrue(ok["ok"])
        self.assertFalse(bad["ok"])
        self.assertIn("fixed", bad["detail"])

    def test_text_any_of_accepts_one_needle(self):
        events = [{"kind": "assistant_message", "payload": {"text": "That API does not exist."}}]
        [check] = self.run_checks(
            [{"kind": "text_any_of", "needles": ["not found", "does not exist"]}],
            events=events,
        )
        self.assertTrue(check["ok"])

    def test_text_checks_use_last_non_empty_assistant_message(self):
        events = [
            {"kind": "assistant_message", "payload": {"text": "done"}},
            {"kind": "assistant_message", "payload": {"text": ""}},
        ]
        [check] = self.run_checks(
            [{"kind": "text_all", "needles": ["done"]}], events=events
        )
        self.assertTrue(check["ok"])

    def test_source_forbidden_flags_banned_pattern(self):
        (self.repo / "lib.rs").write_text("fn f() { x.unwrap(); }\n")
        [check] = self.run_checks(
            [{"kind": "source_forbidden", "path": "lib.rs", "patterns": [r"\.unwrap\("]}]
        )
        self.assertFalse(check["ok"])

    def test_source_forbidden_ignores_test_module(self):
        (self.repo / "lib.rs").write_text(
            "fn f() -> i64 { 1 }\n#[cfg(test)]\nmod tests { fn g() { x.unwrap(); } }\n"
        )
        [check] = self.run_checks(
            [{"kind": "source_forbidden", "path": "lib.rs", "patterns": [r"\.unwrap\("]}]
        )
        self.assertTrue(check["ok"])

    def test_plan_used_needs_updates_and_completed_steps(self):
        [good] = self.run_checks(
            [{"kind": "plan_used", "min_steps": 3, "min_completed": 2}],
            process={"plan_updates": 1, "plan_steps": 3, "plan_completed": 2},
        )
        [no_plan] = self.run_checks(
            [{"kind": "plan_used", "min_steps": 3, "min_completed": 2}],
            process={"plan_updates": 0, "plan_steps": 0, "plan_completed": 0},
        )
        [too_few] = self.run_checks(
            [{"kind": "plan_used", "min_steps": 3, "min_completed": 2}],
            process={"plan_updates": 1, "plan_steps": 2, "plan_completed": 2},
        )
        self.assertTrue(good["ok"])
        self.assertFalse(no_plan["ok"])
        self.assertFalse(too_few["ok"])

    def test_unknown_check_is_reported_not_fatal(self):
        [check] = self.run_checks([{"kind": "nonsense"}])
        self.assertFalse(check["ok"])
        self.assertIn("unknown check", check["detail"])

    def test_axis_defaults_to_correctness(self):
        [check] = self.run_checks([{"kind": "files_unchanged"}])
        self.assertEqual(check["axis"], "correctness")


if __name__ == "__main__":
    unittest.main()
