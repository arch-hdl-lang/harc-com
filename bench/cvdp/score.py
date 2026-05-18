#!/usr/bin/env python3
"""HARC-flavored equivalent of CVDP's cid012 scoring step.

CVDP's reference harness runs Cadence Xcelium + IMC and asserts
per-instance functional coverage ≥ TARGET (default 90). The
open-source-tool analog here is Verilator line + toggle + expression
coverage, scoped to the DUT source file. Concretely:

  1. Run `harc sim --coverage` over (DUT.sv, TB.harc) — emits
     `<outdir>/coverage.dat` (Verilator's binary format).
  2. Post-process with `verilator_coverage --write-info` into an
     LCOV-style `.info` file for line coverage.
  3. Parse Verilator's typed `coverage.dat` records directly for
     semantic control coverage (`page=v_branch/...` and `page=v_expr/...`).
     This avoids counting toggle points that `--write-info` flattens into
     LCOV `BRDA` records.
  4. Filter records to the DUT source file (drop wrappers, drop the
     TB-side scoring file from total).
  5. Apply explicit structural waivers from a generated Verilator control
     file. The scorer mirrors those same waivers while parsing coverage because
     Verilator 5.034 parses but does not reliably honor file-specific
     `coverage_off` directives for these absolute CVDP paths.
  6. Compute lines-covered % = (lines with hit > 0) / (total lines).
  7. Compare to the threshold; exit 0 on pass, 1 on fail.

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
from dataclasses import dataclass
import json, os, subprocess, sys, shutil
from pathlib import Path


@dataclass(frozen=True)
class CoverageWaiver:
    dut_file: str
    first_line: int
    last_line: int
    reason: str


# Verilator coverage waivers for CVDP DUT coverage points that are structurally
# unreachable under the problem's fixed default parameters.
COVERAGE_WAIVERS: dict[str, list[CoverageWaiver]] = {
    "cvdp_copilot_Synchronous_Muller_C_Element_0003": [
        CoverageWaiver(
            "sync_muller_c_element.sv",
            26,
            32,
            "PIPE_DEPTH defaults to 1, so generated pipeline stages with i != 0 are unreachable",
        ),
    ],
    "cvdp_copilot_apb_dsp_unit_0003": [
        CoverageWaiver(
            "apb_dsp_unit.v",
            71,
            72,
            "paddr is 10 bits wide and cannot be greater than MEM_SIZE=1024",
        ),
    ],
    "cvdp_copilot_bcd_adder_0007": [
        CoverageWaiver(
            "bcd_adder.sv",
            43,
            43,
            "four_bit_adder cin is tied constant in both DUT instances",
        ),
    ],
    "cvdp_copilot_binary_to_BCD_0030": [
        CoverageWaiver(
            "binary_to_bcd.sv",
            21,
            21,
            "8-bit input cannot drive the hundreds BCD digit to 5 or greater",
        ),
    ],
    "cvdp_copilot_decode_firstbit_0017": [
        CoverageWaiver(
            "cvdp_copilot_decode_firstbit.sv",
            124,
            129,
            "OutputFormat_g defaults to 0, so the one-hot output format branch is unreachable",
        ),
    ],
    "cvdp_copilot_nbit_swizzling_0009": [
        CoverageWaiver(
            "nbit_swizzling.sv",
            46,
            48,
            "sel is 2 bits and all four case values are covered, making default unreachable",
        ),
    ],
    "cvdp_copilot_simple_spi_0003": [
        CoverageWaiver(
            "spi_fsm.v",
            120,
            120,
            "outer !i_enable and i_fault priority branches make those inner IDLE-condition subterms unreachable",
        ),
    ],
}


def coverage_waivers_for(problem_id: str, dut_sv: Path) -> list[CoverageWaiver]:
    return [
        waiver
        for waiver in COVERAGE_WAIVERS.get(problem_id, [])
        if waiver.dut_file == dut_sv.name
    ]


def waiver_applies(source_file: str, line_no: int, waivers: list[CoverageWaiver]) -> bool:
    source_name = Path(source_file).name
    return any(
        waiver.dut_file == source_name and waiver.first_line <= line_no <= waiver.last_line
        for waiver in waivers
    )


def write_verilator_control_file(
    problem_id: str, dut_sv: Path, outdir: Path
) -> tuple[Path | None, list[CoverageWaiver]]:
    waivers = coverage_waivers_for(problem_id, dut_sv)
    if not waivers:
        return None, []

    control_path = outdir / "cvdp_coverage_waivers.vlt"
    lines = ["`verilator_config"]
    for waiver in waivers:
        source = dut_sv.resolve()
        for line_no in range(waiver.first_line, waiver.last_line + 1):
            lines.append(f'coverage_off -file "{source}" -lines {line_no}')
    control_path.write_text("\n".join(lines) + "\n")
    return control_path, waivers


def parse_lcov(
    info_path: Path, waivers: list[CoverageWaiver] | None = None
) -> dict[str, dict[str, tuple[int, int]]]:
    """Parse LCOV info file → { sourcefile: { metric: (hit, total) } }
    with metrics = "line" and LCOV's raw "branch".

    - **Line coverage** (DA records). Forgiving for combinational DUTs
      where one `always @(*)` block covers many lines on any input;
      meaningful for sequential DUTs where unexercised cases show up
      as unhit lines.
    - **Raw LCOV branch coverage** (BRDA records). Parsed here only as a
      fallback shape; the scorer replaces it with semantic `v_branch` data
      from `coverage.dat` because Verilator can flatten toggle points into
      LCOV BRDA records.
    """
    out: dict[str, dict[str, tuple[int, int]]] = {}
    cur_file = None
    line_hit = line_total = 0
    br_hit = br_total = 0
    active_waivers = waivers or []
    for line in info_path.read_text().splitlines():
        if line.startswith("SF:"):
            cur_file = line[3:]
            line_hit = line_total = br_hit = br_total = 0
        elif line.startswith("DA:"):
            line_no_text, count = line[3:].split(",", 1)
            if cur_file and waiver_applies(cur_file, int(line_no_text), active_waivers):
                continue
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


def parse_verilator_dat_control(
    cov_dat: Path, waivers: list[CoverageWaiver] | None = None
) -> dict[str, tuple[int, int]]:
    """Parse semantic control coverage from Verilator's typed coverage.dat.

    `verilator_coverage --write-info` emits LCOV BRDA rows for some non-branch
    points, notably `page=v_toggle/...` signal-bit points on declarations.
    Those are useful toggle coverage, but they are not source-level control
    flow. The raw coverage records preserve the type in the `page` field, so
    count `v_branch` and `v_expr` pages here and leave toggle coverage out.
    """
    out: dict[str, list[int]] = {}
    active_waivers = waivers or []
    for line in cov_dat.read_text(errors="replace").splitlines():
        if not line.startswith("C '"):
            continue
        try:
            payload, count_text = line[3:].rsplit("' ", 1)
            count = int(count_text)
        except ValueError:
            continue

        fields: dict[str, str] = {}
        for item in payload.split("\x01"):
            if "\x02" not in item:
                continue
            key, value = item.split("\x02", 1)
            fields[key] = value

        page = fields.get("page", "")
        if not (page.startswith("v_branch/") or page.startswith("v_expr/")):
            continue
        source_file = fields.get("f")
        if not source_file:
            continue
        line_text = fields.get("l")
        if line_text and waiver_applies(source_file, int(line_text), active_waivers):
            continue
        hit_total = out.setdefault(source_file, [0, 0])
        hit_total[1] += 1
        if count > 0:
            hit_total[0] += 1

    return {source_file: (hit, total) for source_file, (hit, total) in out.items()}


def score(problem_dir: Path) -> int:
    meta = json.loads((problem_dir / "meta.json").read_text())
    problem_id = meta.get("id", problem_dir.name)
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
    control_file, coverage_waivers = write_verilator_control_file(problem_id, dut_sv, outdir)
    if control_file:
        print(f"[score] Verilator coverage control: {control_file}")

    # ── 2. Build + run via harc sim --coverage ───────────────────────
    harc = repo_root / "target/release/harc"
    if not harc.exists():
        print(f"ERROR: harc binary not found at {harc}; run `cargo build --release --bin harc`", file=sys.stderr)
        return 2

    cmd = [str(harc), "sim"]
    if control_file:
        cmd += ["--vlt", str(control_file)]
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
    per_file = parse_lcov(info_path, coverage_waivers)
    semantic_branches = parse_verilator_dat_control(cov_dat, coverage_waivers)
    for sf in per_file:
        per_file[sf]["branch"] = semantic_branches.get(sf, (0, 0))
    for sf, branch_metric in semantic_branches.items():
        if sf not in per_file:
            per_file[sf] = {"line": (0, 0), "branch": branch_metric}
    if not per_file:
        print(f"[score] FAIL (no coverage points found in {info_path})")
        return 1

    def pct(h: int, t: int) -> str:
        return f"{100.0*h/t:6.2f}%" if t else "  n/a "

    print(f"[score] per-file coverage (line  /  control):")
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
    # Score on semantic **control coverage** as the primary metric: Verilator
    # `v_branch` + `v_expr` points from coverage.dat. LCOV BRDA also contains
    # flattened toggle points on some versions of Verilator, so don't use BRDA
    # directly. For DUTs with no branch/expression points, fall back to line
    # coverage rather than inventing a 0/0 failure.
    score_hit = dut_br_hit
    score_total = dut_br_total
    score_pct = dut_br_pct
    score_label = "control coverage"
    if dut_br_total == 0:
        score_hit = dut_line_hit
        score_total = dut_line_total
        score_pct = dut_line_pct
        score_label = "line coverage (no control points)"

    passed = score_pct >= target
    verdict = "PASS" if passed else "FAIL"
    ceiling_note = ""
    if not passed and dut_br_total > 0 and dut_line_pct >= 99.0:
        ceiling_note = (
            "  [note: 100% line cov reached — the missing control points are "
            "likely structurally unreachable under default DUT params]"
        )
    if dut_br_total == 0:
        print(f"[score] {verdict}: {score_label} {score_pct:.2f}% "
              f"({score_hit}/{score_total})  control n/a (0/0)  "
              f"threshold ≥{target}%")
    else:
        print(f"[score] {verdict}: {score_label} {score_pct:.2f}% "
              f"({score_hit}/{score_total})  line {dut_line_pct:.2f}% "
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
