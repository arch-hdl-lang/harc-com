#!/usr/bin/env python3
"""Smoke-test the extracted CVDP problem set: for each problem dir,
drop a trivial TB that just instantiates the DUT and runs for one
cycle. The test passes if `harc sim` compiles + runs to "ALL TESTS
PASSED" — coverage will be near zero, the point is to confirm the
build pipeline works on every problem before any real TB authoring.

This catches: HARC parser errors, Verilator compile failures on the
DUT, missing wrappers, port-name mismatches. Problems that fail smoke
need either an extract.py fix or a per-problem fixup before they're
viable targets for HARC TB authoring.

Usage:

    bench/cvdp/smoke.py [--cat cid012] [--limit N]
"""

from __future__ import annotations
import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
HARC = ROOT / "target/release/harc"
SMOKE_TB_TEMPLATE = (HERE / "_smoke_tb.harc").read_text()


def smoke_one(problem_dir: Path) -> tuple[bool, str]:
    meta = json.loads((problem_dir / "meta.json").read_text())
    top = meta["top_module"]

    tb_path = problem_dir / "tb" / "_smoke_tb.harc"
    tb_path.write_text(SMOKE_TB_TEMPLATE.replace("{TOP_MODULE}", top))

    sv_files = sorted((problem_dir / "dut").glob("*.sv")) \
             + sorted((problem_dir / "dut").glob("*.v"))
    outdir = problem_dir / "build_smoke"

    cmd = [str(HARC), "sim"]
    for s in sv_files:
        cmd += ["--sv", str(s)]
    cmd += [str(tb_path), "--top", top, "--outdir", str(outdir)]

    r = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    tb_path.unlink()  # clean up the smoke TB regardless of result

    if r.returncode != 0:
        return False, f"harc sim returncode={r.returncode}\nstderr tail:\n{r.stderr[-800:]}"
    if "ALL TESTS PASSED" not in r.stdout:
        return False, f"missing 'ALL TESTS PASSED'\nstdout tail:\n{r.stdout[-400:]}"
    return True, "ok"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cat", default="cid012",
                    help="category filter on meta.categories (default cid012)")
    ap.add_argument("--limit", type=int, default=None)
    args = ap.parse_args()

    problems = []
    for d in sorted(HERE.iterdir()):
        if not (d.is_dir() and (d / "meta.json").exists()):
            continue
        meta = json.loads((d / "meta.json").read_text())
        if args.cat in meta.get("categories", []):
            problems.append(d)

    if args.limit:
        problems = problems[:args.limit]

    print(f"smoke-testing {len(problems)} {args.cat} problems...\n")

    pass_n = fail_n = 0
    failures = []
    for d in problems:
        ok, msg = smoke_one(d)
        if ok:
            pass_n += 1
            print(f"  ✓  {d.name}")
        else:
            fail_n += 1
            failures.append((d.name, msg))
            print(f"  ✗  {d.name}")

    print()
    print(f"Summary:  {pass_n} pass, {fail_n} fail, {len(problems)} total")
    if failures:
        print(f"\nFirst few failures:\n")
        for name, msg in failures[:5]:
            print(f"=== {name} ===")
            print(msg[:1200])
            print()
    return 0 if fail_n == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
