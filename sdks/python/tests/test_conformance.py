"""Run the protocol conformance harness against the Python SDK fixture."""

from __future__ import annotations

import pathlib
import subprocess
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[3]
HARNESS = ROOT / "sdks" / "conformance" / "run.py"
FIXTURE = ROOT / "sdks" / "python" / "examples" / "echo.py"


class ConformanceTest(unittest.TestCase):
    def test_python_fixture(self) -> None:
        result = subprocess.run(
            [sys.executable, str(HARNESS), "--", sys.executable, str(FIXTURE)],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("conformance ok", result.stdout)


if __name__ == "__main__":
    unittest.main()
