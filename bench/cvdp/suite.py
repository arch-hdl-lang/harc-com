#!/usr/bin/env python3
"""Run authored CVDP HARC testbenches through the scorer.

The single-problem scorer remains `score.py`; this script provides the
benchmark-level sweep and an optional v1-vs-TBIR regression lane:

    bench/cvdp/suite.py --compare-codegens
    bench/cvdp/suite.py --problem cvdp_copilot_gray_to_binary_0014
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SCORE = HERE / "score.py"
HARC = ROOT / "target/release/harc"


@dataclass
class RunResult:
    problem: str
    codegen: str
    ok: bool
    returncode: int
    summary: dict | None
    stdout_tail: str
    stderr_tail: str


def authored_problem_dirs(include_missing: bool = False) -> list[Path]:
    out: list[Path] = []
    for d in sorted(HERE.iterdir()):
        if not (d.is_dir() and (d / "meta.json").exists()):
            continue
        has_tb = any((d / "tb").glob("*.harc"))
        if has_tb or include_missing:
            out.append(d)
    return out


def ensure_harc() -> None:
    if HARC.exists():
        return
    subprocess.run(["cargo", "build", "--release", "--bin", "harc"], cwd=ROOT, check=True)


def run_score(problem: Path, codegen: str, root_out: Path) -> RunResult:
    outdir = root_out / problem.name / codegen
    trace = outdir / "trace.jsonl"
    summary = outdir / "score.json"
    cmd = [
        sys.executable,
        str(SCORE),
        str(problem),
        "--codegen",
        codegen,
        "--outdir",
        str(outdir),
        "--record-trace",
        str(trace),
        "--json",
        str(summary),
    ]
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    parsed = json.loads(summary.read_text()) if summary.exists() else None
    return RunResult(
        problem=problem.name,
        codegen=codegen,
        ok=r.returncode == 0,
        returncode=r.returncode,
        summary=parsed,
        stdout_tail=r.stdout[-1200:],
        stderr_tail=r.stderr[-1200:],
    )


def traces_match(problem: Path, root_out: Path) -> tuple[bool, str]:
    v1 = root_out / problem.name / "v1" / "trace.jsonl"
    tbir = root_out / problem.name / "tbir" / "trace.jsonl"
    if not v1.exists() or not tbir.exists():
        return False, "missing trace file"
    r = subprocess.run(
        [str(HARC), "trace-diff", str(v1), str(tbir)],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        return False, (r.stdout + r.stderr)[-1600:]
    return True, ""


def compatible_scores(a: dict | None, b: dict | None) -> bool:
    if not a or not b:
        return False
    keys = [
        "verdict",
        "score_label",
        "score_hit",
        "score_total",
        "line_hit",
        "line_total",
        "control_hit",
        "control_total",
    ]
    return all(a.get(k) == b.get(k) for k in keys)


def compatible_verdicts(a: dict | None, b: dict | None) -> bool:
    if not a or not b:
        return False
    return a.get("verdict") == b.get("verdict") and a.get("returncode") == b.get("returncode")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--problem", action="append", default=[], help="problem directory name to run")
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--cat", default="cid012", help="category filter (default: cid012)")
    ap.add_argument("--out", type=Path, default=HERE / "build_suite")
    ap.add_argument("--codegen", choices=["tbir", "v1"], default="tbir")
    ap.add_argument("--compare-codegens", action="store_true")
    ap.add_argument(
        "--strict-score",
        action="store_true",
        help="with --compare-codegens, require exact line/control coverage counters to match",
    )
    ap.add_argument("--include-missing", action="store_true", help="include dirs without .harc TBs")
    args = ap.parse_args()

    ensure_harc()

    problems = authored_problem_dirs(args.include_missing)
    if args.problem:
        wanted = set(args.problem)
        available = {p.name for p in problems}
        missing = [name for name in args.problem if name not in available]
        if missing:
            print("unknown problem(s): " + ", ".join(missing), file=sys.stderr)
            return 2
        problems = [p for p in problems if p.name in wanted]
    if args.cat:
        filtered = []
        for p in problems:
            meta = json.loads((p / "meta.json").read_text())
            if args.cat in meta.get("categories", []):
                filtered.append(p)
        problems = filtered
    if args.limit:
        problems = problems[: args.limit]

    if not problems:
        print("no CVDP problems selected", file=sys.stderr)
        return 2

    args.out.mkdir(parents=True, exist_ok=True)
    print(f"running {len(problems)} CVDP problem(s); output={args.out}")

    failures: list[str] = []
    for p in problems:
        if args.compare_codegens:
            v1 = run_score(p, "v1", args.out)
            tbir = run_score(p, "tbir", args.out)
            diff_ok = False
            diff_msg = ""
            if v1.ok and tbir.ok:
                diff_ok, diff_msg = traces_match(p, args.out)
            scores_ok = (
                compatible_scores(v1.summary, tbir.summary)
                if args.strict_score
                else compatible_verdicts(v1.summary, tbir.summary)
            )
            ok = v1.ok and tbir.ok and diff_ok and scores_ok
            if ok:
                print(f"  PASS  {p.name} (v1 == tbir)")
                continue
            print(f"  FAIL  {p.name}")
            if not v1.ok:
                print(f"        v1 rc={v1.returncode}: {v1.stderr_tail or v1.stdout_tail}")
            if not tbir.ok:
                print(f"        tbir rc={tbir.returncode}: {tbir.stderr_tail or tbir.stdout_tail}")
            if v1.ok and tbir.ok and not diff_ok:
                print(f"        trace-diff: {diff_msg}")
            if v1.ok and tbir.ok and diff_ok and not scores_ok:
                print("        score summary differs")
            failures.append(p.name)
        else:
            result = run_score(p, args.codegen, args.out)
            if result.ok:
                pct = result.summary.get("score_pct") if result.summary else None
                print(f"  PASS  {p.name} ({args.codegen}, {pct:.2f}%)")
            else:
                print(f"  FAIL  {p.name} ({args.codegen})")
                print(f"        {result.stderr_tail or result.stdout_tail}")
                failures.append(p.name)

    print()
    print(f"Summary: {len(problems) - len(failures)} pass, {len(failures)} fail, {len(problems)} total")
    if failures:
        print("Failed: " + ", ".join(failures))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
