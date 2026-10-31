#!/usr/bin/env python3
"""Compiler-boundary properties with Python-integer expectations.

Native observations are compared to this oracle independently for each backend.
Typed SMT is executed through libz3, with both SAT and contradictory UNSAT cases.
"""
import argparse
import json
from pathlib import Path
from arithmetic_solver import (SmtRunner, check_observations, declare, literal,
                               mask, observe, run, test)


def generate(root):
    root.mkdir(parents=True, exist_ok=True)
    constants, helpers, body, expected = [], [], "", {}

    def observation(key, expr, value, width=64):
        nonlocal body
        if key in expected:
            raise AssertionError(f"duplicate property observation {key}")
        expected[key] = [value, width]
        body += observe(key, expr, width)

    # Full-width bitwise algebra applies even when the syntax originated in
    # bool. Expected bits come from Python integers, not fold_binary metadata.
    atoms = [("false", 0), ("true", 1), ("~false", -1), ("~true", -2)]
    for i, (a, x) in enumerate(atoms):
        constants.append(f"const Atom{i} = {a}\n")
        for j, (b, y) in enumerate(atoms):
            for k, (op, value) in enumerate([( "&", x & y), ("|", x | y), ("^", x ^ y)]):
                key = f"bits{i}{j}{k}"
                constants.append(f"const C{key} = Atom{i} {op} ({b})\n")
                observation(key, f"C{key}", value)
    for i, (expr, value) in enumerate([
        ("false && (1 / 0)", 0), ("true || (1 / 0)", 1),
        ("false and (1 / 0)", 0), ("true or (1 / 0)", 1),
        ("true ? 7 : (1 / 0)", 7), ("false ? (1 / 0) : 7", 7),
    ]):
        constants.append(f"const Lazy{i} = {expr}\n")
        observation(f"lazy{i}", f"Lazy{i}", value)

    helpers += ["""function pure_pred(x: sint<64>) -> bool
    return x
end function pure_pred
function fast_pred(d: Top, x: sint<64>) -> bool
    return x
end function fast_pred
function full_pred(d: Top, x: sint<64>) -> bool
    let copy = x
    return copy
end function full_pred
function port_fast(d: Top) -> bool
    return d.scalar
end function port_fast
function port_full(d: Top) -> bool
    let ignored : uint<64> = 0
    return d.scalar
end function port_full
"""]
    for i, value in enumerate([-(1 << 63), -17, -2, -1, 0, 1, 2, 17, (1 << 63) - 1]):
        body += declare(f"bx{i}", value, 64, True)
        for kind in ["pure", "fast", "full"]:
            args = f"bx{i}" if kind == "pure" else f"dut, bx{i}"
            key = f"bool{kind}{i}"
            body += f"        let {key} = {kind}_pred({args})\n"
            observation(key, key, int(value != 0), 1)

    for i, value in enumerate([0, 2, 1 << 63, 0, (1 << 64) - 1]):
        body += f"        dut.scalar = {hex(value)}\n"
        for kind in ["fast", "full"]:
            key = f"port{kind}{i}"
            body += f"        let {key} = port_{kind}(dut)\n"
            observation(key, key, int(value != 0), 1)

    for i, value in enumerate([-2147483648, -1, 0, 1, 2147483648]):
        body += declare(f"nx{i}", value, 64, True)
        for depth in [1, 2, 3]:
            key = f"neg{i}{depth}"
            expr = " ".join(["-"] * depth + [f"nx{i}"])
            body += f"        let {key} : sint<64> = {expr}\n"
            observation(key, key, (-1 if depth % 2 else 1) * value)
            observation(key + "unchanged", f"nx{i}", value)
    for i, value in enumerate([1, (1 << 31)-1, 1 << 31, (1 << 32)-1, 1 << 32, (1 << 63)-1]):
        for radix, spelling in [("dec", str(value)), ("hex", hex(value)), ("bin", bin(value))]:
            key = f"radix{radix}{i}"
            body += f"        let {key} : sint<64> = -{spelling}\n"
            observation(key, key, -value)

    assert len(expected) == 139, "shared property coverage changed; review the domain table"
    common = "\n".join(constants + helpers)
    (root / "native_v1.harc").write_text(common + test("Properties", body))
    (root / "expected_v1.json").write_text(json.dumps(expected, indent=2))

    helpers.append("""function wide_port_fast(d: Top) -> bool
    return d.value
end function wide_port_fast
function wide_port_full(d: Top) -> bool
    let ignored : uint<64> = 0
    return d.value
end function wide_port_full
""")
    for i, value in enumerate([0, 2, 1 << 100, 0, (1 << 128) - 1]):
        body += f"        dut.value = {hex(value)}\n"
        for kind in ["fast", "full"]:
            key = f"wideport{kind}{i}"
            body += f"        let {key} = wide_port_{kind}(dut)\n"
            observation(key, key, int(value != 0), 1)

    # These scalar-wide operations are supported by TBIR. v1 is deliberately
    # not used as an oracle for its documented wide arithmetic limitations.
    for aw, bw in [(8, 65), (32, 100), (65, 100), (65, 128), (100, 128)]:
        for reverse in [False, True]:
            key = f"wide{aw}to{bw}r{int(reverse)}"
            left, right = ("b", "a") if reverse else ("a", "b")
            helpers.append(f"""function select_{key}(choose: bool, a: sint<{aw}>, b: sint<{bw}>) -> sint<{bw}>
    return choose ? {left} : {right}
end function select_{key}
""")
            # Division forces a masked narrow signed carrier rather than
            # relying only on a sign-filled literal representation.
            body += declare(f"input{key}", -8, aw, True)
            body += f"        let a{key} : sint<{aw}> = input{key} / 3\n"
            body += declare(f"b{key}", 7, bw, True)
            for choose in [False, True]:
                case = key + str(int(choose))
                body += f"        let {case} = select_{key}({str(choose).lower()}, a{key}, b{key})\n"
                chosen_a = choose != reverse
                observation(case, case, -2 if chosen_a else 7, bw)
                observation(case + "sign", f"{case} < 0", int(chosen_a), 1)
    # A wrapping result is unsigned, so a conditional pairing it with the
    # all-ones pattern is unsigned in either arm order and never `< 0`.
    # The pattern is spelled as an explicit unsigned conversion: a bare
    # `-1` arm is signed and the conditional would mix signedness (D1).
    for i, op in enumerate(["+%", "-%", "*%"]):
        for choose in [False, True]:
            for reverse in [False, True]:
                ones = "(-1 as uint<64>)"
                left, right = (ones, f"(7 {op} 1)") if reverse else (f"(7 {op} 1)", ones)
                key = f"wrapshape{i}{int(choose)}{int(reverse)}"
                constants.append(f"const C{key} : bool = ({str(choose).lower()} ? {left} : {right}) < 0\n")
                observation(key, f"C{key}", 0, 1)
    assert len(expected) == 201, "TBIR property coverage changed; review the domain table"
    (root / "native_tbir.harc").write_text("\n".join(constants + helpers) + test("Properties", body))
    (root / "expected_tbir.json").write_text(json.dumps(expected, indent=2))
    (root / "dut.sv").write_text("module Top(input logic clk, input logic [127:0] value, input logic [63:0] scalar); endmodule\n")

    typed = root / "typed"
    typed.mkdir(exist_ok=True)
    cases = []
    def smt_case(source, sat, label):
        key = f"q{len(cases):04d}"
        (typed / f"{key}.harc").write_text(source)
        cases.append({"id": key, "sat": sat, "label": label})
    # Probe every declared enum member and every spare encoding at small
    # cardinalities, including both sides of power-of-two width changes.
    for size in [1, 2, 3, 4, 5, 7, 8, 9, 15, 16]:
        width = max(1, (size - 1).bit_length())
        enum = "enum Op { " + ", ".join(f"V{i}" for i in range(size)) + " }\n"
        # Keep typed enum equality in its supported syntax. Spare encodings
        # are pinned directly in SMT by the verifier, never by enum literals.
        source = enum + "transaction P\n    op : Op\nend transaction P\n"
        for value in range(1 << width):
            smt_case(source, value < size, f"enum:{size}:{width}:{value}")
    for signed in [False, True]:
        for nw in [4, 8, 16]:
            for op in ["<<", ">>"]:
                for x, n in [(1, 0), (1, 7), (1, 8), (1, 15), (1, 256)] + ([(-128, 1), (-1, 15)] if signed else []):
                    if n > mask(nw):
                        continue
                    width = 8
                    result = (x << n if op == "<<" else x >> n) & mask(width)
                    source = (f"transaction P\n    x : {'sint' if signed else 'uint'}<8>\n    n : uint<{nw}>\n"
                              f"    keep (x {op} n) == 0\nend transaction P\n")
                    # Replace the zero-result assertion at SMT level with
                    # pinned input/result equations. Expectations are Python.
                    for sat in [True, False]:
                        value = result if sat else result ^ 1
                        smt_case(source, sat, f"shift:{nw}:{x & 255}:{n}:{value}")
    assert len(cases) == 212, "typed property coverage changed; review the domain table"
    (root / "typed_cases.json").write_text(json.dumps(cases, indent=2))
    return len(expected), len(cases)


def typed(root):
    engine = SmtRunner()
    cases = json.loads((root / "typed_cases.json").read_text())
    for case in cases:
        path = root / "typed" / (case["id"] + ".smt2")
        smt = path.read_text()
        parts = case["label"].split(":")
        if parts[0] == "enum":
            _, _size, width, value = parts
            equation = f"(assert (= f_op (_ bv{value} {width})))\n"
            smt = smt.replace("(check-sat)", equation + "(check-sat)", 1)
        else:
            _, nw, x, n, value = parts
            # The renderer owns the actual shift term; replacing the expected
            # RHS preserves it and all domain/sort generation under test.
            needle = "(_ bv0 8))"
            if smt.count(needle) != 1:
                raise AssertionError(f"ambiguous shift result assertion: {path}\n{smt}")
            smt = smt.replace(needle, f"(_ bv{value} 8))", 1)
            equation = f"(assert (= f_x (_ bv{x} 8)))\n(assert (= f_n (_ bv{n} {nw})))\n"
            smt = smt.replace("(check-sat)", equation + "(check-sat)", 1)
        path.write_text(smt)
        output = engine.check(smt).strip()
        (path.with_suffix(".log")).write_text(output)
        want = "sat" if case["sat"] else "unsat"
        if output != want:
            raise AssertionError(f"{case['label']}: expected {want}, got {output}; {path}")
    print(f"typed properties: {len(cases)} SAT/UNSAT checks", flush=True)


def native(root, harc):
    for backend in ["v1", "tbir"]:
        code, output = run([harc, "sim", root / f"native_{backend}.harc", "--sv", root / "dut.sv",
                            "--top", "Top", "--codegen", backend, "--jobs", "2", "--outdir", root / backend], root, 240)
        (root / f"native_{backend}.log").write_text(output)
        if code != 0 or "ALL TESTS PASSED" not in output:
            raise AssertionError(f"native {backend} failed: {output[-5000:]}; artifacts: {root}")
        expected = json.loads((root / f"expected_{backend}.json").read_text())
        check_observations(output, expected)
        print(f"native {backend}: {len(expected)} independent observations", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=["generate", "typed", "native"], required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--harc", type=Path)
    args = parser.parse_args()
    if not __debug__:
        parser.error("Python optimization disables oracle checks")
    root = args.out.resolve()
    if args.phase == "generate":
        print("generated properties:", generate(root))
    elif args.phase == "typed":
        typed(root)
    elif args.phase == "native":
        if args.harc is None:
            parser.error("--harc required for native execution")
        native(root, args.harc.resolve())


if __name__ == "__main__":
    main()
