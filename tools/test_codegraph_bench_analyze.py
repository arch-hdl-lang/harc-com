import unittest

from codegraph_bench_analyze import summarize


class AnalyzeTests(unittest.TestCase):
    def test_discordant_pair_contributes_to_success_and_total_tokens(self):
        common = dict(task_id="a", repeat=1, task_class="user", task_type="lookup", model="gpt-6-luna", effort="low", latency_s=1)
        records = [
            dict(common, condition="baseline", input_tokens=100, output_tokens=20, success=True),
            dict(common, condition="graph", input_tokens=50, output_tokens=10, success=False),
        ]
        summary = summarize(records, 100)["user/lookup/gpt-6-luna/low"]
        self.assertEqual(summary["conditions"]["baseline"]["tokens_per_success"], 120)
        self.assertIsNone(summary["conditions"]["graph"]["tokens_per_success"])
        self.assertEqual(summary["conditions"]["graph"]["total_provider_tokens"], 60)
        self.assertEqual(summary["success_rate_difference"], -1)
        self.assertEqual(summary["both_success_pairs"], 0)

    def test_incomplete_pair_rejected(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            summarize([dict(task_id="a", repeat=1, task_class="user", task_type="lookup", model="x", effort="low", condition="baseline")])

    def test_mismatched_repository_revision_rejected(self):
        common = dict(task_id="a", repeat=1, task_class="user", task_type="lookup", model="x", effort="low")
        with self.assertRaisesRegex(ValueError, "different repository revisions"):
            summarize([
                dict(common, condition="baseline", repo_sha="a"),
                dict(common, condition="graph", repo_sha="b"),
            ])


if __name__ == "__main__":
    unittest.main()
