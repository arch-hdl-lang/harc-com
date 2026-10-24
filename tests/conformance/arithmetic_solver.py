#!/usr/bin/env python3
"""Independent Python-integer oracle and deterministic native/SMT conformance runner.

No HARC evaluator, runtime helper, or solver is used to calculate expectations.
"""
import argparse
import ctypes
import ctypes.util
import dataclasses
import json
import os
from pathlib import Path
import random
import re
import signal
import shutil
import subprocess
import sys

SEED = 0x48415243


def mask(width):
    return (1 << width) - 1


def quotient(a, b):
    if b == 0:
        raise ValueError("division by zero has no oracle value")
    q = abs(a) // abs(b)
    return -q if (a < 0) != (b < 0) else q


def evaluate(op, a, b, width):
    if op == "/":
        return quotient(a, b)
    if op == "%":
        return a - quotient(a, b) * b
    if op in ("+%", "-%", "*%"):
        return evaluate(op[0], a, b, width) & mask(width)
    return {
        "+": lambda: a + b, "-": lambda: a - b, "*": lambda: a * b,
        "&": lambda: a & b, "|": lambda: a | b, "^": lambda: a ^ b,
        "<": lambda: int(a < b), "<=": lambda: int(a <= b),
        ">": lambda: int(a > b), ">=": lambda: int(a >= b),
        "==": lambda: int(a == b), "!=": lambda: int(a != b),
        "<<": lambda: a << b, ">>": lambda: a >> b,
    }[op]()


def ty(width, signed):
    return f"{'sint' if signed else 'uint'}<{width}>"


def literal(value):
    # Construct negative native inputs with decimal literals. Legacy v1
    # inherits C++'s unsigned rank for unary minus on e.g. 0x80000000;
    # hexadecimal literal typing is outside this arithmetic-operation suite.
    if value == -(1 << 63):
        return "(-9223372036854775807 - 1)"
    if value < 0 and abs(value) < 1 << 63:
        return str(value)
    return ("-" if value < 0 else "") + hex(abs(value))


def declare(name, value, width, signed):
    t = ty(width, signed)
    if signed and width > 64 and abs(value) > (1 << 63) - 1:
        # Wide `as sint<N>` casts past 128 bits are deliberately unsupported.
        # Build a runtime signed value from small positive limbs instead.
        text = f"        let {name} : {t} = 0\n"
        for lo in reversed(range(0, abs(value).bit_length(), 32)):
            text += f"        {name} = ({name} << 32) | {hex((abs(value) >> lo) & mask(32))}\n"
        if value < 0:
            text += f"        {name} = 0 - {name}\n"
        return text
    return f"        let {name} : {t} = {literal(value)}\n"


def bounds(width, signed):
    return (-(1 << (width - 1)), (1 << (width - 1)) - 1) if signed else (0, mask(width))


@dataclasses.dataclass
class Arithmetic:
    id: str
    width: int
    signed: bool
    a: int
    b: int
    op: str
    expected: int
    a_width: int = 0
    b_width: int = 0
    a_dividend: int | None = None
    b_dividend: int | None = None

    @property
    def output_width(self):
        return 1 if self.op in ("<", "<=", ">", ">=", "==", "!=") else self.width

    def source(self, operands=None):
        a, b = operands or (f"a{self.id}", f"b{self.id}")
        declarations = "" if operands else (operand(self, True, a) + operand(self, False, b))
        return (declarations + f"        let r{self.id} = {a} {self.op} {b}\n"
                + observe(self.id, f"r{self.id}", self.output_width))


def operand(case, left, name):
    value = case.a if left else case.b
    width = (case.a_width if left else case.b_width) or case.width
    dividend = case.a_dividend if left else case.b_dividend
    if dividend is not None:
        return (declare(name + "input", dividend, width, case.signed)
                + f"        let {name} : {ty(width, case.signed)} = {name}input / 3\n")
    return declare(name, value, width, case.signed)


def arithmetic_body(cases):
    operands = {}
    body = ""
    for c in cases:
        key = (c.width, c.signed, c.a, c.b, c.a_width, c.b_width, c.a_dividend, c.b_dividend)
        if key not in operands:
            a, b = f"a{c.id}", f"b{c.id}"
            operands[key] = (a, b)
            body += operand(c, True, a) + operand(c, False, b)
        body += c.source(operands[key])
    return body


def observe(case_id, expr, width):
    if width <= 128:
        rendered = "${" + expr + f":0{max(16, (width + 3) // 4)}x" + "}"
    else:
        rendered = "".join("${" + expr + f"[{min(width-1, lo+31)}:{lo}]:08x" + "}"
                           for lo in reversed(range(0, width, 32)))
    return f'        log(info, "CONFORM {case_id} {rendered}")\n'


def test(name, body):
    return f"test {name}\n    let dut : Top\n    run\n{body}    end run\nend test {name}\n"


def arithmetic_cases(seed, extended=False):
    rng = random.Random(seed)
    cases = []
    widths = [2, 3, 8, 31, 32, 63, 64, 65, 100, 127, 128, 129, 256]
    for width in widths:
        for signed in [False, True]:
            lo, hi = bounds(width, signed)
            if width <= 3:
                pairs = [(a, b) for a in range(lo, hi + 1) for b in range(lo, hi + 1)]
            else:
                anchors = [lo, lo + 1, -1 if signed else 0, 0, 1, hi - 1, hi]
                pairs = [(a, b) for a, b in zip(anchors, reversed(anchors))]
                pairs += [(rng.randint(lo, hi), rng.randint(lo, hi))
                          for _ in range(16 if extended else 3)]
            for a, b in pairs:
                ops = ["+", "-", "*", "&", "|", "^", "<", "<=", ">", ">=", "==", "!="]
                if b and not (signed and a == lo and b == -1):
                    ops += ["/", "%"]
                if width <= 64:
                    ops += ["+%", "-%", "*%"]
                for op in ops:
                    result = evaluate(op, a, b, width)
                    # Normal arithmetic overflow is deliberately not inferred
                    # from C++ UB or assignment truncation; explicit wraps are.
                    if op in ("+", "-", "*") and not lo <= result <= hi:
                        continue
                    cases.append(Arithmetic(f"a{len(cases):05d}", width, signed, a, b, op, result))
            for a in [lo, 0, hi]:
                for shift in sorted({0, 1, width - 1}):
                    for op in [">>", "<<"]:
                        result = evaluate(op, a, shift, width)
                        if op == "<<" and (a < 0 or not lo <= result <= hi):
                            continue
                        # Shift counts are unsigned and independent of LHS width.
                        cases.append(Arithmetic(f"a{len(cases):05d}", width, signed, a, shift, op, result))
    for aw, bw in [(65, 100), (100, 65), (65, 128), (128, 65)]:
        for other in [0, 1]:
            a, b = (quotient(-8, 3), other) if aw == 65 else (other, quotient(-8, 3))
            for op in ["+", "-", "*", "&", "|", "^"]:
                cases.append(Arithmetic(f"a{len(cases):05d}", max(aw, bw), True, a, b, op,
                                        evaluate(op, a, b, max(aw, bw)), aw, bw,
                                        -8 if aw == 65 else None, -8 if bw == 65 else None))
    return cases


@dataclasses.dataclass
class Solver:
    id: str
    width: int
    signed: bool
    a: int
    b: int
    op: str
    expected: int
    sat: bool = True

    def declarations(self):
        t = ty(self.width, self.signed)
        value = self.expected
        if not self.sat:
            lo, hi = bounds(self.width, self.signed)
            value = value + 1 if value < hi else lo
        return (f"const A{self.id} : {t} = {literal(self.a)}\n"
                f"const B{self.id} : {t} = {literal(self.b)}\n"
                f"const E{self.id} : {t} = {literal(value)}\n"
                f"transaction T{self.id}\n    a : {t}\n    b : {t}\n"
                f"    keep a == A{self.id}\n    keep b == B{self.id}\n"
                f"    keep a {self.op} b == E{self.id}\nend transaction T{self.id}\n")

    def body(self):
        body = f'        log(info, "BEGIN {self.id}")\n        let t{self.id} : T{self.id}\n        randomize(t{self.id})\n'
        if self.sat:
            body += observe(self.id + "a", f"t{self.id}.a", self.width)
            body += observe(self.id + "b", f"t{self.id}.b", self.width)
            body += f"        if t{self.id}.b != 0\n"
            body += f"            let r{self.id} = t{self.id}.a {self.op} t{self.id}.b\n"
            body += "    " + observe(self.id, f"r{self.id}", self.width)
            body += "        end if\n"
        body += f'        log(info, "COMPLETE {self.id}")\n'
        return body

    def source(self):
        return self.declarations() + test("C" + self.id, self.body())


def solver_cases(seed, extended=False):
    rng = random.Random(seed)
    cases = []
    for width in [3, 8, 32, 64]:
        for signed in [False, True]:
            lo, hi = bounds(width, signed)
            pairs = [(lo, 1), (hi, 2), (lo + 1, hi)]
            if signed:
                pairs += [(-5 if lo <= -5 else lo, 3), (hi, -3), (lo + 1, -3)]
            pairs += [(rng.randint(lo, hi), rng.choice([1, 2, 3]))
                      for _ in range(8 if extended else 2)]
            for a, b in pairs:
                for op in ["/", "%"]:
                    expected = evaluate(op, a, b, width)
                    for sat in [True, False]:
                        cases.append(Solver(f"s{len(cases):04d}", width, signed, a, b, op, expected, sat))
    return cases


@dataclasses.dataclass
class MixedSolver(Solver):
    b_signed: bool = False

    def declarations(self):
        comparison = self.op in ("<", "<=", ">", ">=")
        value = self.expected if self.sat else (1 - self.expected if comparison else (self.expected + 1) & mask(64))
        result_type = "bool" if comparison else "uint<64>"
        result = str(bool(value)).lower() if comparison else literal(value)
        predicate = f"a {self.op} b"
        keep = (predicate if value else f"!({predicate})") if comparison else f"({predicate}) == E{self.id}"
        return (f"const A{self.id} : {ty(64, self.signed)} = {literal(self.a)}\n"
                f"const B{self.id} : {ty(64, self.b_signed)} = {literal(self.b)}\n"
                f"const E{self.id} : {result_type} = {result}\n"
                f"transaction T{self.id}\n    a : {ty(64, self.signed)}\n    b : {ty(64, self.b_signed)}\n"
                f"    keep a == A{self.id}\n    keep b == B{self.id}\n"
                f"    keep {keep}\nend transaction T{self.id}\n")


def mixed_solver_cases():
    cases = []
    for a, b, sa, sb in [(mask(64) - 1, 3, False, True), (-5, 3, True, False),
                         (5, -3, False, True), (-5, mask(64) - 2, True, False)]:
        for op in ["/", "%", "<", "<=", ">", ">="]:
            # At equal 64-bit rank a typed unsigned operand wins. Convert
            # independently in Python before evaluating division or ordering.
            expected = evaluate(op, a & mask(64), b & mask(64), 64)
            for sat in [True, False]:
                cases.append(MixedSolver(f"m{len(cases):04d}", 64, sa, a, b, op, expected, sat, sb))
    return cases


def run(command, directory, timeout=180, env=None):
    process = subprocess.Popen([str(x) for x in command], cwd=directory, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, start_new_session=True)
    try:
        output, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        # Kill only this invocation's process group, including native build
        # children. A timed-out compiler must not leak into later CI tests.
        os.killpg(process.pid, signal.SIGKILL)
        output, _ = process.communicate()
        return -signal.SIGKILL, output + f"\nCONFORMANCE TIMEOUT after {timeout}s: {command}\n"
    return process.returncode, output


def check_observations(output, expected):
    observations = re.findall(r"CONFORM (\w+)[ \t]+([0-9a-fA-F]+)", output)
    seen = set()
    for key, value in observations:
        if key not in expected or key in seen:
            raise AssertionError(f"unexpected or duplicate observation {key}")
        seen.add(key)
        want, width = expected[key]
        if width == 1 and int(value, 16) not in (0, 1):
            raise AssertionError(f"{key}: noncanonical Boolean observation {value}")
        if int(value, 16) & mask(width) != want & mask(width):
            raise AssertionError(f"{key}: expected {want & mask(width):x}, observed {value}")
    if seen != set(expected):
        raise AssertionError(f"missing observations: {sorted(set(expected) - seen)[:10]}")


def generate(root, seed, extended):
    root.mkdir(parents=True, exist_ok=True)
    arithmetic = arithmetic_cases(seed, extended)
    solver = solver_cases(seed, extended)
    (root / "dut.sv").write_text("module Top(input logic clk); endmodule\n")
    for backend in ["tbir", "v1"]:
        selected = [c for c in arithmetic if backend == "tbir" or c.width <= 64 or
                    (c.width <= 128 and c.op in ("<", "<=", ">", ">=", "==", "!="))]
        (root / f"arithmetic_{backend}.harc").write_text("\n".join(
            test(name, arithmetic_body(group)) for name, group in arithmetic_groups(selected)))
    # Compile once, then select the SAT and UNSAT batches in fresh processes.
    native_solver = solver + mixed_solver_cases()
    (root / "solver.harc").write_text("\n".join(c.declarations() for c in native_solver)
        + test("SolverSat", "".join(c.body() for c in native_solver if c.sat))
        + test("SolverUnsat", "".join(c.body() for c in native_solver if not c.sat)))
    (root / "cases.json").write_text(json.dumps({"seed": seed, "arithmetic": [dataclasses.asdict(c) for c in arithmetic],
                                               "solver": [dataclasses.asdict(c) for c in solver],
                                               "native_extra": [dataclasses.asdict(c) for c in mixed_solver_cases()]}, indent=2))
    typed = root / "typed"
    typed.mkdir(exist_ok=True)
    for c in solver:
        (typed / (c.id + ".harc")).write_text(c.declarations())
    return arithmetic, solver


def failure(root, case, output):
    (root / "failure.log").write_text(output)
    if case is not None:
        (root / "failure.json").write_text(json.dumps(dataclasses.asdict(case), indent=2))
        if isinstance(case, Solver):
            body = case.body()
            if case.sat:
                body += (f'        assert t{case.id}.a == A{case.id} else fail("pinned a")\n'
                         f'        assert t{case.id}.b == B{case.id} else fail("pinned b")\n'
                         f'        if t{case.id}.b != 0\n            assert (t{case.id}.a {case.op} t{case.id}.b) == E{case.id} else fail("oracle result")\n        end if\n')
            source = case.declarations() + test("Repro", body)
        else:
            body = case.source()
            if case.output_width == 1:
                expected = str(bool(case.expected)).lower()
            else:
                signed = case.signed and case.op not in ("+%", "-%", "*%")
                body += declare("expected", case.expected, case.width, signed)
                expected = "expected"
            body += f'        assert r{case.id} == {expected} else fail("oracle {case.id}")\n'
            source = test("Repro", body)
        (root / "repro.harc").write_text(source)
    print(f"Failure artifacts retained in {root}", file=sys.stderr)


def arithmetic_groups(cases):
    groups = {}
    for c in cases:
        groups.setdefault(f"Arithmetic{c.width}{'s' if c.signed else 'u'}", []).append(c)
    return list(groups.items())


def native(root, repo, harc, arithmetic, solver, timeout, only="all", backend_choice="both"):
    env = {**os.environ, "HARC_NO_LEARN": "1"}
    for backend in (["tbir", "v1"] if backend_choice == "both" else [backend_choice]):
        selected = [c for c in arithmetic if backend == "tbir" or c.width <= 64 or
                    (c.width <= 128 and c.op in ("<", "<=", ">", ">=", "==", "!="))]
        for suite in (["arithmetic", "solver"] if only == "all" else [only]):
            source = root / (f"arithmetic_{backend}.harc" if suite == "arithmetic" else "solver.harc")
            out = root / f"{suite}_{backend}"
            selected_test = arithmetic_groups(selected)[0][0] if suite == "arithmetic" else "SolverSat"
            command = [harc, "sim", source, "--sv", root / "dut.sv", "--top", "Top", "--codegen", backend,
                       "--test", selected_test, "--seed", "1", "--jobs", "2", "--outdir", out]
            status, output = run(command, repo, timeout, env)
            (root / f"{suite}_{backend}.log").write_text(output)
            if status or "ALL TESTS PASSED" not in output:
                candidates = selected if suite == "arithmetic" else solver
                ids = re.findall(r"[abr]([as]\d{4,5})", output)
                case = next((c for c in candidates if c.id in ids), None)
                if suite == "solver":
                    for c in solver:
                        if c.sat and f"BEGIN {c.id}\n" in output:
                            segment = output.split(f"BEGIN {c.id}\n", 1)[1].split(f"COMPLETE {c.id}", 1)[0]
                            if "UNSAT" in segment or f"CONFORM {c.id} " not in segment:
                                case = c
                                break
                failure(root, case, output)
                raise AssertionError(f"{suite}/{backend} build or initial SAT run failed: {output[-4000:]}")
            binary = out / "obj_dir" / "VTop"
            if suite == "arithmetic":
                for name, group in arithmetic_groups(selected):
                    status, output = run([binary, "--test", name, "--seed", "1"], root, 30, env)
                    (root / f"{backend}_{name}.log").write_text(output)
                    try:
                        if status or "ALL TESTS PASSED" not in output:
                            raise AssertionError(f"{name} failed to complete: {output[-2000:]}")
                        check_observations(output, {c.id: (c.expected, c.output_width) for c in group})
                    except AssertionError as error:
                        case = next((c for c in group if c.id in str(error)), None)
                        failure(root, case, output)
                        raise
                print(f"arithmetic/{backend}: {len(selected)} oracle comparisons", flush=True)
            else:
                for sat, name in [(True, "SolverSat"), (False, "SolverUnsat")]:
                    status, output = run([binary, "--test", name, "--seed", "1"], root, 60, env)
                    (root / f"{backend}_{name}.log").write_text(output)
                    group = [c for c in solver if c.sat == sat]
                    for c in group:
                        try:
                            segment = output.split(f"BEGIN {c.id}\n", 1)[1].split(f"COMPLETE {c.id}", 1)[0]
                            if sat:
                                assert status == 0 and "ALL TESTS PASSED" in output, output[-2000:]
                                check_observations(segment, {c.id: (c.expected, c.width),
                                                            c.id + "a": (c.a, c.width), c.id + "b": (c.b, c.width)})
                            else:
                                assert status > 0 and "UNSAT" in segment and f"COMPLETE {c.id}" in output, segment[-2000:]
                        except (AssertionError, IndexError):
                            failure(root, c, output)
                            raise AssertionError(f"solver/{backend}/{c.id} expected {'SAT' if c.sat else 'UNSAT'}: {output[-2000:]}")
                print(f"solver/{backend}: {len(solver)} SAT/UNSAT oracle queries", flush=True)


class SmtRunner:
    """Execute exported SMT using installed libz3; no Python Z3 package needed."""
    def __init__(self):
        candidates = [os.environ.get("HARC_CONFORMANCE_Z3_LIB"), ctypes.util.find_library("z3")]
        executable = shutil.which("z3")
        if executable:
            candidates.append(str(Path(executable).resolve().parents[1] / "lib/libz3.dylib"))
        candidates += ["libz3.so.4", "libz3.so", "libz3.dylib"]
        for candidate in candidates:
            if candidate:
                try:
                    self.lib = ctypes.CDLL(candidate)
                    break
                except OSError:
                    pass
        else:
            raise RuntimeError("libz3 required; set HARC_CONFORMANCE_Z3_LIB for a custom installation")
        ptr, text, uint = ctypes.c_void_p, ctypes.c_char_p, ctypes.c_uint
        self.handler_type = ctypes.CFUNCTYPE(None, ptr, uint)
        self.handler = self.handler_type(lambda _context, _code: None)
        for name, args, result in [
            ("Z3_mk_config", [], ptr), ("Z3_del_config", [ptr], None),
            ("Z3_set_param_value", [ptr, text, text], None),
            ("Z3_mk_context", [ptr], ptr), ("Z3_del_context", [ptr], None),
            ("Z3_set_error_handler", [ptr, self.handler_type], None),
            ("Z3_eval_smtlib2_string", [ptr, text], text),
            ("Z3_get_error_code", [ptr], uint), ("Z3_get_error_msg", [ptr, uint], text),
        ]:
            function = getattr(self.lib, name)
            function.argtypes, function.restype = args, result

    def check(self, source):
        config = self.lib.Z3_mk_config()
        self.lib.Z3_set_param_value(config, b"timeout", b"10000")
        context = self.lib.Z3_mk_context(config)
        self.lib.Z3_del_config(config)
        self.lib.Z3_set_error_handler(context, self.handler)
        try:
            result = self.lib.Z3_eval_smtlib2_string(context, source.encode())
            error = self.lib.Z3_get_error_code(context)
            if error:
                raise RuntimeError(self.lib.Z3_get_error_msg(context, error).decode())
            return result.decode()
        finally:
            self.lib.Z3_del_context(context)


def typed(root, solver):
    engine = SmtRunner()
    for c in solver:
        smt = root / "typed" / (c.id + ".smt2")
        assert smt.is_file(), f"missing typed SMT artifact: {smt}"
        try:
            output = engine.check(smt.read_text())
        except RuntimeError as error:
            failure(root, c, str(error))
            raise
        expected = "sat" if c.sat else "unsat"
        (root / "typed" / (c.id + ".log")).write_text(output)
        if output.strip() != expected:
            failure(root, c, output)
            raise AssertionError(f"typed/{c.id}: expected {expected}, got {output!r}")
    print(f"typed SMT/Z3: {len(solver)} SAT/UNSAT oracle queries", flush=True)


def self_test():
    assert quotient(-5, 3) == -1 and evaluate("%", -5, 3, 8) == -2
    assert quotient(5, -3) == -1 and evaluate("%", 5, -3, 8) == 2
    assert evaluate("%", -5, -3, 8) == -2
    assert evaluate("+%", 255, 1, 8) == 0
    assert evaluate("-%", 0, 1, 8) == 255
    assert evaluate("*%", 255, 3, 8) == 253
    assert evaluate(">>", -8, 1, 65) == -4
    for a in range(-16, 17):
        for b in range(-16, 17):
            if b:
                q, r = quotient(a, b), evaluate("%", a, b, 8)
                assert a == b * q + r and abs(r) < abs(b)
                assert r == 0 or (r < 0) == (a < 0)
    for output in ["", "CONFORM x 02", "CONFORM x 01\nCONFORM x 01", "CONFORM extra 01"]:
        try:
            check_observations(output, {"x": (1, 8)})
        except AssertionError:
            pass
        else:
            raise AssertionError("observation checker accepted injected defect")
    check_observations("CONFORM x ff", {"x": (-1, 8)})
    arithmetic = arithmetic_cases(SEED)
    solver = solver_cases(SEED)
    assert len(arithmetic) == 5967 and len(solver) == 208 and len(mixed_solver_cases()) == 48
    for width in [2, 3]:
        for signed in [False, True]:
            pairs = {(c.a, c.b) for c in arithmetic if c.width == width and c.signed == signed and c.op == "+%"}
            assert len(pairs) == 1 << (2 * width), "small-width exhaustive coverage was reduced"
    try:
        check_observations("CONFORM b 03", {"b": (1, 1)})
    except AssertionError:
        pass
    else:
        raise AssertionError("checker accepted a noncanonical Boolean")
    assert arithmetic == arithmetic_cases(SEED)
    assert solver == solver_cases(SEED)
    print("oracle and observation-checker self-tests: passed", flush=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", required=True, type=Path)
    p.add_argument("--harc", type=Path)
    p.add_argument("--seed", type=lambda x: int(x, 0), default=SEED)
    p.add_argument("--extended", action="store_true")
    p.add_argument("--phase", choices=["generate", "native", "typed"], default="generate")
    p.add_argument("--timeout", type=int, default=240)
    p.add_argument("--only", choices=["all", "arithmetic", "solver"], default="all", help="native replay subset")
    p.add_argument("--backend", choices=["both", "tbir", "v1"], default="both", help="native replay backend")
    args = p.parse_args()
    if not __debug__:
        p.error("Python optimization disables conformance assertions; run without -O/PYTHONOPTIMIZE")
    self_test()
    root = args.out.resolve()
    arithmetic, solver = generate(root, args.seed, args.extended)
    print(f"seed={args.seed:#x}; arithmetic cases={len(arithmetic)}; typed solver cases={len(solver)}; native solver cases={len(solver) + len(mixed_solver_cases())}", flush=True)
    if args.phase == "native":
        if not args.harc:
            p.error("--harc is required for native phase")
        native(root, Path(__file__).resolve().parents[2], args.harc.resolve(), arithmetic, solver + mixed_solver_cases(), args.timeout, args.only, args.backend)
    elif args.phase == "typed":
        typed(root, solver)


if __name__ == "__main__":
    main()
