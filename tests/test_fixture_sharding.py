#!/usr/bin/env python3
"""Exercise real runner dispatch with a fake compiler; no Verilator required."""
import collections
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent


class FixtureShardingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        tests = self.root / "tests"
        tests.mkdir()
        for name in ("fixture_sharding.sh", "run_fixtures.sh",
                     "run_cosim_fixtures.sh", "run_tbir_equiv.sh"):
            shutil.copy2(SCRIPTS / name, tests / name)
        self.names = [f"fixture{i}" for i in range(9)]
        # Blank/comment lines must not affect shard assignment.
        (tests / "fixtures.tbl").write_text("# header\n\n" + "\n".join(
            f"{name}|Top|top.sv|||" for name in self.names) + "\n")
        (tests / "tbir_equiv_fixtures.txt").write_text("# header\n\n" + "\n".join(
            f"{name}|Top|top.sv|-|{'fail' if name == 'fixture3' else 'pass'}"
            for name in self.names) + "\n")
        self.compiler = self.root / "fake-harc"
        self.compiler.write_text('''#!/usr/bin/env bash
if [ "$1" = trace-diff ]; then exit 0; fi
name=""
equiv=0
for arg in "$@"; do
    case "$arg" in
        *.harc) name="${arg##*/}"; name="${name%.harc}" ;;
        --codegen) equiv=1 ;;
    esac
done
if [ "$name" = "${FAKE_FAIL:-}" ] || { [ "$equiv" = 1 ] && [ "$name" = fixture3 ]; }; then
    echo "1 TESTS FAILED"
    exit 1
fi
echo "ALL TESTS PASSED"
''')
        self.compiler.chmod(0o755)

    def run_runner(self, runner, **overrides):
        env = dict(os.environ)
        for key in ("SHARD_INDEX", "SHARD_COUNT", "HARC_SIM_EXTRA_ARGS",
                    "ARCH_BIN", "MT", "FAKE_FAIL", "TBIR_EQUIV_OUT",
                    "FIXTURE_BUILD_ROOT"):
            env.pop(key, None)
        env.update(HARC=str(self.compiler), JOBS="2")
        env.update(overrides)
        return subprocess.run(["bash", f"tests/{runner}"], cwd=self.root,
                              env=env, capture_output=True, text=True, timeout=30)

    @staticmethod
    def passes(result):
        return re.findall(r"^  PASS  (\S+)", result.stdout, re.MULTILINE)

    def test_full_run_and_four_shards_cover_each_row_once(self):
        for runner in ("run_fixtures.sh", "run_cosim_fixtures.sh", "run_tbir_equiv.sh"):
            with self.subTest(runner=runner):
                full = self.run_runner(runner)
                self.assertEqual(full.returncode, 0, full.stdout + full.stderr)
                self.assertEqual(self.passes(full), self.names)
                combined = []
                for index in range(4):
                    result = self.run_runner(runner, SHARD_INDEX=str(index), SHARD_COUNT="4")
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(self.passes(result), self.names[index::4])
                    combined += self.passes(result)
                self.assertEqual(collections.Counter(combined), collections.Counter(self.names))

    def test_bad_configuration_and_empty_shards_fail(self):
        configurations = [dict(SHARD_COUNT=count) for count in ("0", "-1", "abc", "", "04", "9999999999")]
        configurations += [dict(SHARD_INDEX=index, SHARD_COUNT="4")
                           for index in ("-1", "4", "abc", "", "01", "9999999999")]
        configurations += [dict(SHARD_INDEX="9", SHARD_COUNT="10")]
        for runner in ("run_fixtures.sh", "run_tbir_equiv.sh"):
            for config in configurations:
                with self.subTest(runner=runner, config=config):
                    result = self.run_runner(runner, **config)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertIn("error:", result.stderr)

    def test_failure_is_reported_only_by_owning_shard(self):
        for runner in ("run_fixtures.sh", "run_cosim_fixtures.sh", "run_tbir_equiv.sh"):
            for index in range(4):
                with self.subTest(runner=runner, index=index):
                    result = self.run_runner(runner, SHARD_INDEX=str(index),
                                             SHARD_COUNT="4", FAKE_FAIL="fixture5")
                    self.assertEqual(result.returncode, 1 if index == 1 else 0,
                                     result.stdout + result.stderr)
                    if index == 1:
                        self.assertIn("FAIL  fixture5", result.stdout)


if __name__ == "__main__":
    unittest.main()
