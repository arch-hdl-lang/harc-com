import unittest

from codegraph_bench_ci_report import report


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
        graph = row("graph")
        graph["repo_sha"] = "other"
        with self.assertRaises(ValueError):
            report([row("baseline"), graph], TASKS)


if __name__ == "__main__":
    unittest.main()
