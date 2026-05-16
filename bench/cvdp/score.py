#!/usr/bin/env python3
"""HARC-flavored equivalent of CVDP's cid012 scoring step.

CVDP's reference harness runs Cadence Xcelium + IMC and asserts
per-instance functional coverage ≥ TARGET (default 90). The
open-source-tool analog here is Verilator line + toggle + expression
coverage, scoped to the DUT source file. Concretely:

  1. Run `harc sim --coverage` over (DUT.sv, TB.harc) — emits
     `<outdir>/coverage.dat` (Verilator's binary format).
  2. Post-process with `verilator_coverage --write-info` into an
     LCOV-style `.info` file.
  3. Filter the LCOV records to the DUT source file (drop wrappers,
     drop the TB-side scoring file from total).
  4. Compute lines-covered % = (lines with hit > 0) / (total lines).
  5. Compare to the threshold; exit 0 on pass, 1 on fail.

Verilator's line metric on the DUT file is a strict subset of what
Xcelium's IMC reports (no covergroup / cross-coverage), so a TB that
clears this bar on Verilator is hitting the DUT's executable paths
thoroughly — directionally aligned with what CVDP scores even if the
absolute number is different.

Usage:

  bench/cvdp/score.py <problem_dir>

Where `<problem_dir>` contains:
  - dut/<dut>.sv               — the CVDP-provided DUT source
  - dut/<dut>_top.sv           — (optional) thin clocked wrapper
  - tb/<tb>.harc               — the candidate HARC testbench
  - meta.json                  — { top_module, target_coverage }

Outputs to stdout:
  - PASS / FAIL: <pct>% (>= threshold <T>%)
  - per-file coverage breakdown
  - exit 0 on PASS, 1 on FAIL
"""

from __future__ import annotations
import json, os, subprocess, sys, shutil
from pathlib import Path


def parse_lcov(info_path: Path) -> dict[str, dict[str, tuple[int, int]]]:
    """Parse LCOV info file → { sourcefile: { metric: (hit, total) } }
    with metrics = "line" and "branch".

    - **Line coverage** (DA records). Forgiving for combinational DUTs
      where one `always @(*)` block covers many lines on any input;
      meaningful for sequential DUTs where unexercised cases show up
      as unhit lines.
    - **Branch coverage** (BRDA records). Counts each `if`-branch
      independently. The closest open-source analog to Cadence IMC's
      branch-coverage report that CVDP cid012 nominally targets, but
      Verilator counts structurally-unreachable branches in the
      denominator (e.g. `if (i != 0)` in a `genvar` loop with size 1,
      or an over-wide condition the input range can't satisfy), so the
      branch percentage has a per-DUT ceiling that may sit below the
      problem's nominal target.

    The score command reports both; the user decides which to gate on.
    """
    out: dict[str, dict[str, tuple[int, int]]] = {}
    cur_file = None
    line_hit = line_total = 0
    br_hit = br_total = 0
    for line in info_path.read_text().splitlines():
        if line.startswith("SF:"):
            cur_file = line[3:]
            line_hit = line_total = br_hit = br_total = 0
        elif line.startswith("DA:"):
            _, count = line[3:].split(",", 1)
            line_total += 1
            if int(count) > 0:
                line_hit += 1
        elif line.startswith("BRDA:"):
            count = line.split(",")[-1]
            br_total += 1
            if count != "-" and int(count) > 0:
                br_hit += 1
        elif line.startswith("end_of_record") and cur_file:
            out[cur_file] = {
                "line": (line_hit, line_total),
                "branch": (br_hit, br_total),
            }
            cur_file = None
    return out


def score(problem_dir: Path) -> int:
    meta = json.loads((problem_dir / "meta.json").read_text())
    top = meta["top_module"]
    dut_module = meta.get("dut_module", top)  # default: top == dut (no wrapper)
    target = float(meta.get("target_coverage", 90))
    repo_root = Path(__file__).resolve().parents[2]

    # ── 1. Gather files ──────────────────────────────────────────────
    # CVDP source files come as either `.sv` (SystemVerilog) or `.v`
    # (plain Verilog). Verilator accepts both via the same `--sv`
    # / `-cc` flow, so we hand both extensions to harc-sim.
    sv_files = sorted(
        list((problem_dir / "dut").glob("*.sv"))
        + list((problem_dir / "dut").glob("*.v"))
    )
    harc_files = sorted((problem_dir / "tb").glob("*.harc"))
    if not sv_files:
        print(f"ERROR: no .sv / .v files in {problem_dir}/dut/", file=sys.stderr)
        return 2
    if not harc_files:
        print(f"ERROR: no .harc files in {problem_dir}/tb/", file=sys.stderr)
        return 2

    # DUT-of-record for coverage scoring = the .sv whose stem matches
    # meta.dut_module. If the test wraps the CVDP DUT in a clocked
    # `_top` shim (so HARC's posedge loop has something to drive),
    # we still want to score against the inner module.
    dut_sv_candidates = [s for s in sv_files if s.stem == dut_module]
    if not dut_sv_candidates:
        # Fallback: take the first non-`_top`-suffixed .sv.
        dut_sv_candidates = [s for s in sv_files if not s.stem.endswith("_top")]
    if not dut_sv_candidates:
        dut_sv_candidates = sv_files
    dut_sv = dut_sv_candidates[0]
    print(f"[score] DUT-of-record: {dut_sv.name}")
    print(f"[score] coverage target: ≥{target}%")

    outdir = problem_dir / "build"
    outdir.mkdir(exist_ok=True)

    # ── 2. Build + run via harc sim --coverage ───────────────────────
    harc = repo_root / "target/release/harc"
    if not harc.exists():
        print(f"ERROR: harc binary not found at {harc}; run `cargo build --release --bin harc`", file=sys.stderr)
        return 2

    cmd = [str(harc), "sim"]
    for s in sv_files:
        cmd += ["--sv", str(s)]
    cmd += [str(harc_files[0])]
    cmd += ["--top", top, "--coverage", "--outdir", str(outdir)]
    print(f"[score] $ {' '.join(cmd)}")
    r = subprocess.run(cmd, capture_output=True, text=True)
    (outdir / "harc_sim.stdout").write_text(r.stdout)
    (outdir / "harc_sim.stderr").write_text(r.stderr)
    if r.returncode != 0:
        print(f"[score] FAIL (harc-sim build/run failed; see {outdir}/harc_sim.stderr)")
        return 1
    if "ALL TESTS PASSED" not in r.stdout:
        print(f"[score] FAIL (TB did not print 'ALL TESTS PASSED'; see {outdir}/harc_sim.stdout)")
        return 1

    cov_dat = outdir / "coverage.dat"
    if not cov_dat.exists():
        print(f"[score] FAIL (no coverage.dat at {cov_dat}; was harc built with --coverage support?)")
        return 1

    # ── 3. verilator_coverage --write-info → LCOV-style info ─────────
    vcov = shutil.which("verilator_coverage")
    if not vcov:
        print("ERROR: verilator_coverage not in PATH", file=sys.stderr)
        return 2
    info_path = outdir / "coverage.info"
    r = subprocess.run(
        [vcov, "--write-info", str(info_path), str(cov_dat)],
        capture_output=True, text=True,
    )
    if r.returncode != 0:
        print(f"[score] verilator_coverage failed: {r.stderr}", file=sys.stderr)
        return 2

    # ── 4. Parse, filter to DUT-of-record, compute % ─────────────────
    per_file = parse_lcov(info_path)
    if not per_file:
        print(f"[score] FAIL (no coverage points found in {info_path})")
        return 1

    def pct(h: int, t: int) -> str:
        return f"{100.0*h/t:6.2f}%" if t else "  n/a "

    print(f"[score] per-file coverage (line  /  branch):")
    for sf, m in sorted(per_file.items()):
        lh, lt = m["line"]
        bh, bt = m["branch"]
        marker = "  ←  DUT" if Path(sf).name == dut_sv.name else ""
        print(f"           {pct(lh,lt)} ({lh}/{lt})  /  "
              f"{pct(bh,bt)} ({bh}/{bt})    {Path(sf).name}{marker}")

    dut_key = next((k for k in per_file if Path(k).name == dut_sv.name), None)
    if not dut_key:
        print(f"[score] FAIL (no coverage record for {dut_sv.name} — was the DUT linked?)")
        return 1

    dut_line_hit, dut_line_total = per_file[dut_key]["line"]
    dut_br_hit, dut_br_total = per_file[dut_key]["branch"]
    dut_line_pct = 100.0 * dut_line_hit / dut_line_total if dut_line_total else 0.0
    dut_br_pct = 100.0 * dut_br_hit / dut_br_total if dut_br_total else 0.0

    # ── 5. Verdict ───────────────────────────────────────────────────
    # Score on **branch coverage** as the primary metric (closest analog
    # to Cadence IMC's branch report that CVDP targets). Line coverage
    # is reported alongside as a sanity check — a TB that's 100% line
    # but well below target on branch is doing something interesting
    # (DUT has structurally unreachable branches; ceiling reached).
    passed = dut_br_pct >= target
    verdict = "PASS" if passed else "FAIL"
    ceiling_note = ""
    if not passed and dut_line_pct >= 99.0:
        ceiling_note = (
            "  [note: 100% line cov reached — the missing branches are "
            "likely structurally unreachable under default DUT params]"
        )
    print(f"[score] {verdict}: branch coverage {dut_br_pct:.2f}% "
          f"({dut_br_hit}/{dut_br_total})  line {dut_line_pct:.2f}% "
          f"({dut_line_hit}/{dut_line_total})  threshold ≥{target}%"
          f"{ceiling_note}")
    return 0 if passed else 1


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(f"usage: {argv[0]} <problem_dir>", file=sys.stderr)
        return 2
    return score(Path(argv[1]).resolve())


if __name__ == "__main__":
    sys.exit(main(sys.argv))
