#!/usr/bin/env python3
"""Generate a synthetic TB-IR-compatible suite for split-emission benchmarking.

The production suite that motivated harc#538 cannot be used as a benchmark:
it is proprietary, and it does not lower through TB-IR yet (a passive
composite-component gap, tracked separately). This generator stands in for
it, reproducing the shape that matters to split emission:

  * enough tests to produce a double-digit shard count at a realistic
    `--cpp-split-group-size`;
  * test bodies large enough that emitting one shard is real work rather
    than noise;
  * suite-global scaffolding (transaction records with `keep` constraints
    and a `randomize` site per test) so the run exercises the solver
    problem table and randomize snippets, which are built once per suite
    and reused by every shard.

Defaults give 352 tests / 11 shards at group size 32 — the same shape as
the measurement recorded in harc#538.

Usage:
    python3 bench/split_emit/gen_suite.py --outdir /tmp/split_bench
"""

import argparse
import os

DUT_SV = """\
module SplitAdder(input logic [7:0] a, input logic [7:0] b, output logic [8:0] sum);
  always_comb sum = a + b;
endmodule
"""


def record(i):
    return (
        f"transaction Rec{i}\n"
        "    addr : uint<32>\n"
        "    data : uint<32>\n"
        "    tag : uint<8>\n"
        "\n"
        "    keep addr < 4096\n"
        "    keep tag < 16\n"
        f"end transaction Rec{i}"
    )


def test(i, stmts, nrec):
    body = [
        f"test B{i}",
        "    let dut : SplitAdder",
        "    run",
        f"        let r : Rec{i % nrec}",
        "        randomize(r) with",
        f"            r.tag == {i % 16}",
        "        end randomize",
        f"        assert r.tag == {i % 16}",
    ]
    for j in range(stmts):
        a = (i + j) % 200
        b = (j * 3) % 50
        body += [
            f"        dut.a = {a}",
            f"        dut.b = {b}",
            "        wait 1 cycle",
            f"        assert dut.sum == {a + b}",
        ]
    body += ["    end run", f"end test B{i}"]
    return "\n".join(body)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--outdir", required=True)
    ap.add_argument("--tests", type=int, default=352)
    ap.add_argument(
        "--stmts",
        type=int,
        default=2000,
        help="clocked statement groups per test; drives generated C++ volume",
    )
    ap.add_argument("--records", type=int, default=24)
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    parts = [record(r) for r in range(args.records)]
    parts += [test(i, args.stmts, args.records) for i in range(args.tests)]

    harc_path = os.path.join(args.outdir, "suite.harc")
    sv_path = os.path.join(args.outdir, "SplitAdder.sv")
    with open(harc_path, "w") as f:
        f.write("\n\n".join(parts) + "\n")
    with open(sv_path, "w") as f:
        f.write(DUT_SV)

    print(f"wrote {harc_path} ({os.path.getsize(harc_path) / 1e6:.1f} MB)")
    print(f"wrote {sv_path}")


if __name__ == "__main__":
    main()
