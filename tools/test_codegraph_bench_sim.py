import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import codegraph_bench_sim as sim


class SimGradeTests(unittest.TestCase):
    def test_requires_added_behavior_simulation_and_no_other_source_edits(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sim.FROZEN = root / "frozen"
            work = root / "work"
            sim.FROZEN.mkdir()
            work.mkdir()
            original = "module A\n"
            (sim.FROZEN / "case.harc").write_text(original)
            (work / "case.harc").write_text(original + "assert dut.count_out == 0\n")
            task = {"expected_files": ["case.harc"], "must_add": {"case.harc": ["assert dut.count_out == 0"]}}
            rubric = {"checks": [{"passed": True, "output": "ALL TESTS PASSED"}]}
            with patch.object(sim.common, "grade", return_value=(True, rubric)):
                passed, details = sim.grade(task, "", work, {"case.harc": original}, root / "harc", root)
                self.assertTrue(passed)
                self.assertEqual(details["unexpected_source_changes"], [])
                (work / "other.rs").write_text("changed\n")
                passed, details = sim.grade(task, "", work, {"case.harc": original}, root / "harc", root)
                self.assertFalse(passed)
                self.assertEqual(details["unexpected_source_changes"], ["other.rs"])

    def test_rejects_vacuous_edit_and_missing_sim_marker(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sim.FROZEN = root / "frozen"
            work = root / "work"
            sim.FROZEN.mkdir()
            work.mkdir()
            (sim.FROZEN / "case.harc").write_text("module A\n")
            (work / "case.harc").write_text("module B\n")
            task = {"expected_files": ["case.harc"], "must_add": {"case.harc": ["assert dut.count_out == 0"]}}
            rubric = {"checks": [{"passed": True, "output": "build succeeded"}]}
            with patch.object(sim.common, "grade", return_value=(True, rubric)):
                passed, details = sim.grade(task, "", work, {"case.harc": "module A\n"}, root / "harc", root)
                self.assertFalse(passed)
                self.assertFalse(details["simulation_passed"])
                self.assertEqual(len(details["missing_added"]), 1)


if __name__ == "__main__":
    unittest.main()
