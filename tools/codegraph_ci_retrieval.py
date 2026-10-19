#!/usr/bin/env python3
"""Fail CI when graph discovery loses the benchmark fixtures."""

from __future__ import annotations

import argparse
import subprocess
import tempfile
from pathlib import Path


CASES = (
    (("query", "AxiLiteRegsTest", "--limit", "5"), "tests/fixtures/axilite_regs_test.harc"),
    (("context", "scoreboard basic counter Top", "--limit", "8", "--token-budget", "350"),
     "tests/fixtures/scoreboard_basic_test.harc"),
    (("query", "TopCounterTest", "--limit", "3"), "tests/fixtures/top_counter_test.harc"),
)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--harc", type=Path, required=True)
    args = parser.parse_args()
    harc = args.harc.resolve()
    with tempfile.TemporaryDirectory(prefix="harc-graph-ci-") as temporary:
        index = Path(temporary) / "index"
        subprocess.run([str(harc), "graph", "index", "tests/fixtures", "tests/dut",
                        "--out", str(index)], check=True, capture_output=True, text=True)
        for command, expected in CASES:
            result = subprocess.run([str(harc), "graph", *command, "--index", str(index)],
                                    check=True, capture_output=True, text=True)
            if expected not in result.stdout:
                raise AssertionError(f"graph {' '.join(command)} did not find {expected}: {result.stdout[:500]}")
            print(f"graph {' '.join(command[:2])}: {expected}")


if __name__ == "__main__":
    main()
