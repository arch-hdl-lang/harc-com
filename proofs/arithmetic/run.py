#!/usr/bin/env python3
"""Run source-bound CBMC/Kani obligations; missing tools/timeouts fail closed."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
PREFIX = """typedef unsigned __int128 _harc_u128;
typedef unsigned int uint32_t;
typedef unsigned long long uint64_t;
"""
RNG_PREFIX = PREFIX + ("typedef signed long long int64_t;\n"
                       "#define INT64_MAX 9223372036854775807LL\n"
                       "#define UINT64_MAX 18446744073709551615ULL\n")
RUST_HARNESSES = (
    "const_add_sub", "const_mul", "const_wrap", "const_compare", "const_shift",
    "const_bit_logic", "const_bitwise_laws", "const_wrapping_roundtrip", "const_div_rem", "const_errors", "solver_promotion", "solver_unknown_widths",
)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def slice_source(source, name):
    begin, end = f"// BEGIN FORMAL {name} KERNELS", f"// END FORMAL {name} KERNELS"
    if source.count(begin) != 1 or source.count(end) != 1 or source.index(begin) >= source.index(end):
        raise ValueError(f"missing or ambiguous production slice: {name}")
    return source.split(begin)[1].split(end)[0]


def rng_source(source):
    # Extract the actual template, not a maintained model of its body. The
    # unique signature and unindented terminator fail closed on source drift.
    signature = "template <typename Next>\ninline int64_t harc_rng_range(Next next, int64_t lo, int64_t hi) {"
    if source.count(signature) != 1:
        raise ValueError("missing or ambiguous production RNG template")
    body = source[source.index(signature):]
    if "\n}" not in body:
        raise ValueError("missing production RNG terminator")
    return body.split("\n}", 1)[0] + "\n}\n"


def function_source(source, signature):
    """Extract one actual, unindented production function, failing on drift."""
    if source.count(signature) != 1:
        raise ValueError(f"missing or ambiguous production function: {signature}")
    body = source[source.index(signature):]
    if "\n}" not in body:
        raise ValueError(f"missing production function terminator: {signature}")
    return body.split("\n}", 1)[0] + "\n}\n"


SEXT_SIGNATURE = "inline _harc_u128 harc_sext_u128(_harc_u128 value, unsigned source_width, unsigned dest_width) {"
COUNT_SIGNATURES = (
    "inline uint64_t harc_u64_shift_count(uint64_t value, uint64_t limit) {",
    "inline uint64_t harc_u128_shift_count(_harc_u128 value, uint64_t limit) {",
)


def conversion_source(source, group):
    if group == "sext":
        return slice_source(source, "U128") + function_source(source, SEXT_SIGNATURE)
    if group == "counts":
        return "".join(function_source(source, signature) for signature in COUNT_SIGNATURES)
    raise ValueError(f"unknown conversion group: {group}")


def run(command, timeout):
    process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, start_new_session=True)
    try:
        output, _ = process.communicate(timeout=timeout)
        return process.returncode, output
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        output, _ = process.communicate()
        return None, output + f"\nPROOF TIMEOUT after {timeout}s\n"


def cbmc_passed(code, output):
    if code != 0:
        return False
    try:
        messages = json.loads(output)
    except (ValueError, TypeError):
        return False
    results = [r for message in messages for r in message.get("result", [])]
    return (any(m.get("cProverStatus") == "success" for m in messages)
            and any(r.get("property", "").startswith("main.assertion.") for r in results)
            and all(r.get("status") == "SUCCESS" for r in results))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", choices=["u128", "wide", "rng", "rust", "all"], default="all")
    parser.add_argument("--timeout", type=int, default=180, help="seconds per CBMC obligation or Kani batch")
    parser.add_argument("--out", type=Path, default=ROOT / "target/formal")
    parser.add_argument("--only", help="run one named C++ obligation or Rust harness")
    parser.add_argument("--extended-division", action="store_true", help="also attempt costly 32/64/128-bit division equivalence")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("timeout must be positive")
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    cases = []
    if args.group in ("u128", "all"):
        for name in ("mask", "compare", "shift", "divzero"):
            cases.append((f"u128-{name}", "u128", f"PROOF_{name.upper()}", [], 1))
        for width in ((1, 8, 32, 64, 128) if args.extended_division else tuple(range(1, 9))):
            cases.append((f"u128-divrem-{width}", "u128", "PROOF_DIVREM", [f"-DCHECK_WIDTH={width}"], 1))
        cases += [("u128-sext", "sext", "PROOF_SEXT", [], 1),
                  ("u128-shift-count", "counts", "PROOF_SHIFT_COUNT", [], 1)]
    if args.group in ("wide", "all"):
        for words in (1, 2, 4, 5, 8, 32):
            for operation in ("arith", "extend"):
                cases.append((f"wide-{operation}-{words}", "wide", f"PROOF_WIDE_{operation.upper()}",
                              [f"-DWORDS={words}", f"-DPACK_BITS={words * 32}"], words + 1))
    if args.group in ("rng", "all"):
        for operation in ("full", "half", "small", "degenerate"):
            cases.append((f"rng-{operation}", "rng", f"PROOF_RNG_{operation.upper()}", [], 1))
    rust = args.group in ("rust", "all")
    if args.only:
        cases = [case for case in cases if case[0] == args.only]
        rust = rust and args.only in RUST_HARNESSES
        if not cases and not rust:
            parser.error("unknown obligation for selected group")
    cbmc, kani = shutil.which("cbmc"), shutil.which("kani")
    if cases and not cbmc:
        parser.error("CBMC is required; no proof was run")
    if rust and not kani:
        parser.error("Kani is required; no proof was run")
    needs_z3 = rust or any(c[0] == "wide-extend-32" for c in cases)
    if needs_z3 and not shutil.which("z3"):
        parser.error("Z3 is required by these arithmetic harnesses")
    runtime = ROOT / "runtime/harc_thread_rt.h"
    source = runtime.read_text()
    inputs = [runtime, ROOT / "runtime/harc_random_rt.h", ROOT / "src/numeric_semantics.rs", ROOT / "src/ast.rs",
              ROOT / "src/ir/lower/mod.rs", ROOT / "src/codegen/native/constraint_solver_renderer.rs",
              ROOT / "src/codegen/randomize_renderer.rs", Path(__file__),
              *(ROOT / f"proofs/arithmetic/{name}" for name in ("u128.cpp", "wide.cpp", "rng.cpp", "kani.rs"))]
    report = {"scope": "See docs/formal-arithmetic.md for domains and exclusions",
              "inputs": {str(p.relative_to(ROOT)): digest(p) for p in inputs}, "tools": {}, "results": []}
    for tool in (cbmc if cases else None, kani if rust else None, shutil.which("z3") if needs_z3 else None):
        if tool:
            code, version = run([tool, "--version"], 30)
            if code != 0:
                parser.error(f"cannot run proof tool {tool}: {version}")
            report["tools"][Path(tool).name] = version.strip()
    failed = False

    def record(name, command, code, log, passed, started):
        nonlocal failed
        status = "timeout" if code is None else "passed" if passed else "failed"
        (args.out / f"{name}.log").write_text(log)
        entry = {"name": name, "status": status, "seconds": round(time.monotonic() - started, 2), "command": command}
        report["results"].append(entry)
        (args.out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"{name}: {status} ({entry['seconds']}s)", flush=True)
        failed |= status != "passed"

    for name, group, macro, defines, unwind in cases:
        if group == "rng":
            body = rng_source((ROOT / "runtime/harc_random_rt.h").read_text())
        elif group in ("sext", "counts"):
            body = conversion_source(source, group)
        else:
            body = slice_source(source, "U128" if group == "u128" else "WORD")
        prefix = RNG_PREFIX if group == "rng" else PREFIX
        unit = args.out / f"{name}.cpp"
        harness = "u128" if group in ("sext", "counts") else group
        unit.write_text(prefix + body + (ROOT / f"proofs/arithmetic/{harness}.cpp").read_text())
        command = [cbmc, str(unit), "--cpp11", f"-D{macro}", *defines, "--unwind", str(unwind),
                   "--unwinding-assertions", "--trace", "--json-ui"]
        if name == "wide-extend-32":
            command += ["--smt2", "--z3"]
        started = time.monotonic()
        code, log = run(command, args.timeout)
        record(name, command, code, log, cbmc_passed(code, log), started)
    if rust:
        harnesses = [args.only] if args.only else list(RUST_HARNESSES)
        command = [kani, str(ROOT / "proofs/arithmetic/kani.rs"), "--default-unwind", "5",
                   "--output-format", "terse", "--target-dir", str(args.out / "kani-build")]
        for harness in harnesses:
            command += ["--harness", harness, "--exact"] if len(harnesses) == 1 else ["--harness", harness]
        started = time.monotonic()
        code, log = run(command, args.timeout)
        summaries = re.findall(r"Complete - (\d+) successfully verified harnesses, (\d+) failures, (\d+) total", log)
        passed = code == 0 and summaries == [(str(len(harnesses)), "0", str(len(harnesses)))]
        record("rust", command, code, log, passed, started)
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
