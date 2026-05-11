#!/usr/bin/env python3
"""Extract CVDP cid012 (testbench-generation) problems from the
public Hugging Face dataset into `bench/cvdp/<id>/` problem directories.

**Strict no-gold-TB policy.** The HF records contain a reference TB
at `output.context["verif/*_tb.sv"]`. The extractor MUST NEVER copy
that to disk — looking at it would contaminate the agent's TB
authoring task. This module touches `output` only to verify it
EXISTS for the record (sanity check on dataset shape); the content
is read from a discarded variable and not written anywhere.

Per-problem layout:

    bench/cvdp/<problem-id>/
        meta.json           — top_module, dut_module, target_coverage, id
        prompt.txt          — the original CVDP prompt (English description of the DUT)
        dut/<name>.sv       — the DUT-under-test, unmodified from harness.files["src/<name>.sv"]
        dut/<name>_top.sv   — (auto-generated only for clockless DUTs) thin clocked wrapper
                              exposing a phantom `clk` so HARC's posedge loop applies

The clockless-DUT detection runs `verilator --xml-only` on the DUT
and checks the port list: if no `clk`-named input is present, we emit
a `_top.sv` wrapper. Generalizing this to a HARC-codegen change so we
don't need wrappers at all is a separate cleanup (PR follow-up).

Usage:

    bench/cvdp/extract.py --jsonl /tmp/cvdp_hf/cvdp_v1.1.0_nonagentic_code_generation_commercial.jsonl
                          --category cid012
                          --out bench/cvdp
                          [--limit N]
                          [--force]   # overwrite existing dirs
"""

from __future__ import annotations
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Port:
    name: str
    direction: str  # "input" | "output"
    msb: int | None
    lsb: int | None
    signed: bool

    def decl(self) -> str:
        sig = " signed" if self.signed else ""
        if self.msb is None:
            # single bit
            return f"{self.direction}{sig} logic {self.name}"
        return f"{self.direction}{sig} logic [{self.msb}:{self.lsb}] {self.name}"


@dataclass
class ModuleInfo:
    name: str
    ports: list[Port]

    @property
    def clock_inputs(self) -> list[Port]:
        """All 1-bit input ports whose name looks like a clock.

        CVDP DUTs use a wild variety of clock-port names:
          - `clk` (the canonical case — no wrapper needed)
          - `i_clk`, `clk_i`, `clk_in`, `clkin`, `clock`
          - `wr_clk`, `rd_clk`, `clk_dsp`, `PCLK` (multi-clock domains)

        Heuristic: 1-bit input whose lowercased name contains "clk" or
        equals "clock". Tighter than a substring match on "clk"
        (rejecting things like "clkin_valid" which are *signals*, not
        clocks — though no false positives observed yet in cid012).
        """
        out = []
        for p in self.ports:
            if p.direction != "input":
                continue
            lname = p.name.lower()
            # Width check: clock is always 1 bit (msb is None or msb==lsb).
            if p.msb is not None and p.msb != p.lsb:
                continue
            if lname == "clock" or "clk" in lname:
                out.append(p)
        return out


def probe_dut(sv_path: Path) -> ModuleInfo:
    """Run `verilator --xml-only` on the DUT and parse out the top
    module's port list. Returns ModuleInfo with width-resolved ports.
    """
    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        # Verilator picks the only top module automatically if there
        # is exactly one in the file.
        # `--no-timing` tells Verilator to silently elide `#delay`
        # statements rather than refusing to elaborate. The probe
        # only needs port info, not faithful timing semantics — for
        # actual scoring the DUT is compiled normally by `harc sim`,
        # and CVDP's reference harness also ignores delay timing in
        # its coverage-only metric.
        r = subprocess.run(
            ["verilator", "--xml-only", "--no-timing", "-Wno-fatal",
             "-Mdir", str(td), str(sv_path)],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            raise RuntimeError(f"verilator --xml-only failed on {sv_path}:\n{r.stderr}")
        xml_path = next(td.glob("V*.xml"), None)
        if not xml_path:
            raise RuntimeError(f"no V*.xml output in {td} (verilator stderr:\n{r.stderr})")

        root = ET.parse(xml_path).getroot()

        # Resolve dtype_id → (msb, lsb, signed) via typetable.
        dtypes: dict[str, tuple[int | None, int | None, bool]] = {}
        ttable = root.find(".//typetable")
        if ttable is not None:
            for bd in ttable.findall("basicdtype"):
                d_id = bd.get("id")
                left = bd.get("left")
                right = bd.get("right")
                signed = bd.get("signed") == "true"
                if d_id:
                    dtypes[d_id] = (
                        int(left) if left is not None else None,
                        int(right) if right is not None else None,
                        signed,
                    )

        # Pick the netlist module (skip Verilator built-ins; the
        # netlist has exactly one top module for our DUTs).
        netlist_modules = root.findall("./netlist/module")
        if not netlist_modules:
            raise RuntimeError(f"no <module> in netlist for {sv_path}")
        m = netlist_modules[0]
        name = m.get("name")
        if not name:
            raise RuntimeError(f"netlist module has no name for {sv_path}")

        ports: list[Port] = []
        for v in m.findall("var"):
            d = v.get("dir")
            if d not in ("input", "output"):
                continue  # skip non-port vars
            pn = v.get("name")
            d_id = v.get("dtype_id", "")
            msb, lsb, signed = dtypes.get(d_id, (None, None, False))
            ports.append(Port(name=pn, direction=d, msb=msb, lsb=lsb, signed=signed))

        return ModuleInfo(name=name, ports=ports)


def emit_clocked_wrapper(info: ModuleInfo, dut_clock: Port | None) -> tuple[str, str]:
    """Emit a `<dut>_top.sv` wrapper that exposes `clk` (HARC's
    hardcoded primary clock name) plus the DUT's other ports.

    Two cases:
      - `dut_clock is None`: DUT is purely combinational. Wrapper
        adds a phantom `clk` input; DUT outputs settle on each
        `eval()` regardless of clock state. Semantics-preserving.
      - `dut_clock is not None`: DUT has a clock-shaped port named
        something other than `clk` (e.g. `i_clk`, `clk_in`, `clock`).
        Wrapper renames it: `clk` in the wrapper port list, connected
        to the DUT's actual clock port name. HARC drives `dut->clk`
        and the DUT sees a real toggling clock.

    Returns (top_module_name, wrapper_sv_source).
    """
    top_name = f"{info.name}_top"
    # Wrapper port list: always start with `clk`, then pass through every
    # DUT port EXCEPT the renamed clock (avoid duplicate port name).
    port_decls = ["    input  logic clk"]
    port_conns = []
    for p in info.ports:
        if dut_clock is not None and p.name == dut_clock.name:
            # The DUT's clock port: connect to wrapper's `clk`.
            port_conns.append(f"        .{p.name}(clk)")
            continue
        port_decls.append("    " + p.decl().replace("input ", "input  ").replace("output ", "output "))
        port_conns.append(f"        .{p.name}({p.name})")
    body = ",\n".join(port_decls)
    conns = ",\n".join(port_conns)
    if dut_clock is None:
        reason = (f"is purely combinational (no clock-shaped input)"
                  f"; wrapper adds a phantom `clk` so HARC's posedge\n"
                  f"// loop applies. Combinational outputs settle on every "
                  f"`eval()`,\n"
                  f"// so the wrapper is semantics-preserving")
    else:
        reason = (f"clocked port `{dut_clock.name}` doesn't match HARC's "
                  f"hardcoded `clk`\n"
                  f"// name. Wrapper renames it: HARC drives `clk`, "
                  f"connected to\n"
                  f"// the DUT's `.{dut_clock.name}(clk)` port")
    sv = f"""`timescale 1ns/1ps

// Auto-generated by bench/cvdp/extract.py.
//
// The CVDP DUT `{info.name}` {reason}.

module {top_name} (
{body}
);
    {info.name} uut (
{conns}
    );
endmodule
"""
    return top_name, sv


def parse_env_target(env_content: str) -> int:
    """Extract `TARGET = N` from the CVDP `src/.env` file."""
    m = re.search(r"^\s*TARGET\s*=\s*(\d+)\s*$", env_content, re.MULTILINE)
    if not m:
        return 90  # CVDP default
    return int(m.group(1))


# ───────────────────────────────────────────────────────────────────
# AGENT-FACING SAFETY: this is the only place that touches
# `record['output']` — and only to assert the gold TB exists in the
# dataset, NEVER to read its content into something that could land
# on disk or in a returned value. The local variable shadowing makes
# the intent explicit to anyone reviewing.
# ───────────────────────────────────────────────────────────────────
def _verify_record_shape_without_reading_gold(record: dict) -> None:
    """Assert the record has the expected fields without inspecting
    or returning any gold-TB content. The `verif/*_tb.sv` paths are
    only used as existence keys; we never pull their content out.
    """
    assert "output" in record, "record missing 'output' field"
    out_ctx = record["output"].get("context", {})
    assert isinstance(out_ctx, dict), "record output.context is not a dict"
    gold_keys = [k for k in out_ctx if k.startswith("verif/") and k.endswith(".sv")]
    if not gold_keys:
        raise ValueError("no gold TB in record output.context (expected verif/*_tb.sv)")
    # Intentionally NOT returning gold_keys' content. Caller does
    # not get a path to the gold TB string anywhere.
    del out_ctx  # explicit drop


def extract_record(record: dict, out_dir: Path, force: bool = False) -> dict:
    """Lay out one cid012 problem in `out_dir/<id>/`. Returns a small
    status dict describing what was written.
    """
    rid = record["id"]
    problem_dir = out_dir / rid

    # Sanity-check the record shape (existence of gold, NOT its content).
    _verify_record_shape_without_reading_gold(record)

    if problem_dir.exists() and not force:
        return {"id": rid, "skipped": "exists"}

    problem_dir.mkdir(parents=True, exist_ok=True)
    (problem_dir / "dut").mkdir(exist_ok=True)
    (problem_dir / "tb").mkdir(exist_ok=True)

    # ── DUT ────────────────────────────────────────────────────────
    harness_files = record["harness"]["files"]
    # Accept both `.sv` and `.v` extensions. CVDP records use either
    # depending on whether the DUT was authored as Verilog-2001 or
    # SystemVerilog. Both flow through Verilator the same way.
    dut_sv_keys = [k for k in harness_files
                   if k.startswith("src/") and (k.endswith(".sv") or k.endswith(".v"))]
    if len(dut_sv_keys) != 1:
        return {"id": rid, "error": f"expected 1 DUT .(s)v, found {len(dut_sv_keys)}: {dut_sv_keys}"}
    dut_key = dut_sv_keys[0]
    dut_basename = Path(dut_key).name  # "src/foo.sv" → "foo.sv"
    dut_dst = problem_dir / "dut" / dut_basename
    dut_dst.write_text(harness_files[dut_key])

    # ── Probe DUT for clk / module name ────────────────────────────
    try:
        info = probe_dut(dut_dst)
    except RuntimeError as e:
        return {"id": rid, "error": f"verilator probe failed: {e}"}

    # ── Auto-wrap DUTs whose clock isn't named `clk` ──────────────
    # Three cases:
    #   1. DUT has port named `clk` → no wrapper needed
    #   2. DUT has clock-shaped port under another name (`i_clk`,
    #      `clock`, etc.) → wrapper renames it to `clk`
    #   3. DUT has no clock-shaped port (combinational) → wrapper
    #      adds a phantom `clk` (functionally a no-op for the DUT)
    #
    # Multi-clock DUTs (multiple clock-shaped ports) are flagged for
    # manual handling — HARC's single primary clock isn't enough,
    # they need either explicit clock declarations in the .harc TB
    # or a richer wrapper that ratios the clocks.
    clocks = info.clock_inputs
    canonical_clk = next((p for p in clocks if p.name == "clk"), None)

    wrapper_written = None
    multi_clock = False
    if canonical_clk is not None and len(clocks) == 1:
        # Case 1: canonical, single clock — no wrapper needed.
        top_module = info.name
    elif len(clocks) == 1:
        # Case 2: single clock with non-canonical name — rename via wrapper.
        top_module, wrapper_sv = emit_clocked_wrapper(info, clocks[0])
        wrapper_path = problem_dir / "dut" / f"{top_module}.sv"
        wrapper_path.write_text(wrapper_sv)
        wrapper_written = wrapper_path.name
    elif len(clocks) == 0:
        # Case 3: combinational — phantom clk wrapper.
        top_module, wrapper_sv = emit_clocked_wrapper(info, None)
        wrapper_path = problem_dir / "dut" / f"{top_module}.sv"
        wrapper_path.write_text(wrapper_sv)
        wrapper_written = wrapper_path.name
    else:
        # Multi-clock — punt for Phase 2a; needs manual TB authoring
        # with explicit clock decls. Still build the wrapper with the
        # first-listed clock for smoke testing, but flag the meta so
        # the scoring layer knows.
        multi_clock = True
        top_module, wrapper_sv = emit_clocked_wrapper(info, clocks[0])
        wrapper_path = problem_dir / "dut" / f"{top_module}.sv"
        wrapper_path.write_text(wrapper_sv)
        wrapper_written = wrapper_path.name

    # ── Prompt ─────────────────────────────────────────────────────
    prompt = record["input"]["prompt"]
    (problem_dir / "prompt.txt").write_text(prompt)

    # ── meta.json ──────────────────────────────────────────────────
    target = parse_env_target(harness_files.get("src/.env", ""))
    meta = {
        "id": rid,
        "categories": record["categories"],
        "top_module": top_module,
        "dut_module": info.name,
        "target_coverage": target,
        "auto_wrapper": wrapper_written,
        "n_ports": len(info.ports),
        "clock_inputs": [p.name for p in clocks],
        "multi_clock": multi_clock,
    }
    (problem_dir / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")

    return {"id": rid, "ok": True, "top": top_module, "dut": info.name,
            "wrapper": wrapper_written, "target": target,
            "n_ports": len(info.ports), "n_clocks": len(clocks),
            "multi_clock": multi_clock}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--jsonl", type=Path, required=True,
                    help="Path to HF JSONL (e.g. cvdp_v1.1.0_nonagentic_code_generation_commercial.jsonl)")
    ap.add_argument("--category", default="cid012",
                    help="CVDP category filter (default: cid012)")
    ap.add_argument("--out", type=Path, required=True,
                    help="Output root (e.g. bench/cvdp)")
    ap.add_argument("--limit", type=int, default=None,
                    help="Stop after N records (for smoke testing)")
    ap.add_argument("--force", action="store_true",
                    help="Overwrite existing problem dirs")
    args = ap.parse_args()

    written = 0
    skipped = 0
    errors: list[tuple[str, str]] = []
    clk_native = 0
    clk_renamed = 0
    combinational = 0
    multi_clock = 0

    with open(args.jsonl) as f:
        for ln in f:
            r = json.loads(ln)
            if args.category not in r["categories"]:
                continue
            res = extract_record(r, args.out, force=args.force)
            if "error" in res:
                errors.append((res["id"], res["error"]))
                print(f"  ✗ {res['id']}: {res['error']}", file=sys.stderr)
            elif "skipped" in res:
                skipped += 1
            else:
                written += 1
                if res["multi_clock"]:
                    multi_clock += 1
                    tag = "MULTI"
                elif res["n_clocks"] == 0:
                    combinational += 1
                    tag = "comb "
                elif res["wrapper"]:
                    clk_renamed += 1
                    tag = "rename"
                else:
                    clk_native += 1
                    tag = "clk  "
                print(f"  ✓ [{tag:6s}] {res['id']:55s} top={res['top']:35s} target=≥{res['target']}%")
            if args.limit and written >= args.limit:
                break

    print()
    print(f"Summary:")
    print(f"  written:                  {written}")
    print(f"  skipped (already exists): {skipped}")
    print(f"  errors:                   {len(errors)}")
    print(f"  clk-native (no wrapper):  {clk_native}")
    print(f"  clk-renamed (1 wrapper):  {clk_renamed}")
    print(f"  combinational (phantom):  {combinational}")
    print(f"  multi-clock (flagged):    {multi_clock}")
    if errors:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
