import unittest
import tempfile
import subprocess
from pathlib import Path

from codegraph_bench_ci_report import public_metrics, report


TASKS = {"repeats": 1, "tasks": [{"id": "one"}]}


def row(condition, tokens=100, success=True):
    return {"task_id": "one", "repeat": 1, "condition": condition,
            "repo_sha": "abc", "repo_diff_sha256": "source", "tasks_sha256": "tasks",
            "protocol_ok": True, "exit_code": 0, "input_tokens": tokens, "output_tokens": 1,
            "success": success}


class ReportTests(unittest.TestCase):
    def test_good_pair(self):
        summary, passed = report([row("baseline"), row("graph", 80)], TASKS)
        self.assertTrue(passed)
        self.assertIn("Regression gate: PASS", summary)

    def test_missing_or_duplicate_pair(self):
        for records in ([row("baseline")], [row("baseline"), row("graph"), row("graph")]):
            with self.assertRaises(ValueError):
                report(records, TASKS)

    def test_success_and_token_regression(self):
        self.assertFalse(report([row("baseline"), row("graph", success=False)], TASKS)[1])
        self.assertFalse(report([row("baseline"), row("graph", 140)], TASKS)[1])

    def test_invalid_usage_or_source(self):
        graph = row("graph")
        graph["protocol_ok"] = False
        with self.assertRaises(ValueError):
            report([row("baseline"), graph], TASKS)

    def test_agent_failure_counts_as_failure(self):
        graph = row("graph", success=False)
        graph["exit_code"] = 1
        summary, passed = report([row("baseline"), graph], TASKS)
        self.assertFalse(passed)
        self.assertIn("0/1", summary)

    def test_public_metrics_drops_agent_text(self):
        baseline = row("baseline")
        baseline["answer"] = "OPENAI_API_KEY=secret"
        baseline["rubric"] = {"patch": "secret"}
        output = public_metrics([baseline])
        self.assertNotIn("secret", output)
        self.assertIn('"input_tokens": 100', output)

    def test_cli_writes_diagnostic_artifacts_for_missing_records(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            summary = root / "not-created" / "summary.md"
            metrics = root / "not-created" / "metrics.jsonl"
            result = subprocess.run(
                ["python3", str(Path(__file__).with_name("codegraph_bench_ci_report.py")),
                 str(root / "missing.jsonl"), "--summary", str(summary), "--metrics", str(metrics)],
                capture_output=True, text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Benchmark integrity failed", summary.read_text())
            self.assertEqual(metrics.read_text(), "")
        graph = row("graph")
        graph["repo_sha"] = "other"
        with self.assertRaises(ValueError):
            report([row("baseline"), graph], TASKS)


if __name__ == "__main__":
    unittest.main()
