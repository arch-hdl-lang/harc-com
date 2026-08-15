#!/bin/bash
# End-to-end smoke test for issue #477: a clocked testbench with an active
# `phase post_eval` transactor service, run under VCD tracing, must NOT
# produce Verilator `previous dump at t=..., dump call ignored` warnings
# (which mean a duplicate same-timestamp dump silently dropped the settled
# post_eval state).
#
# Run from harc-com repo root:
#     ./tests/run_vcd_dump_smoke.sh
#
# Set HARC=/path/to/harc to override the binary (default: build via
# `cargo build --release --bin harc`). Needs Verilator on PATH.
set -uo pipefail

HARC="${HARC:-./target/release/harc}"
DUT_DIR="tests/dut"
FIX_DIR="tests/fixtures"
FIXTURE="post_eval_vcd_smoke_test"
TOP="Top"
SV="top_counter.sv"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc || exit 1
fi
case "$HARC" in
    /*) ;;
    *) HARC="$PWD/$HARC" ;;
esac

OUTDIR="$(mktemp -d "${TMPDIR:-/tmp}/harc_vcd_smoke.XXXXXX")"
trap 'rm -rf "$OUTDIR"' EXIT

echo "Running VCD dup-dump smoke test ($FIXTURE) under --wave-format vcd..."

# Force VCD (the format the bug is specific to — FST uses distinct
# timestamps and never warns) and capture BOTH streams: the Verilator
# warnings land on the simulation binary's stderr.
out="$("$HARC" sim \
    --sv "$DUT_DIR/$SV" \
    "$FIX_DIR/$FIXTURE.harc" \
    --top "$TOP" \
    --waves --wave-format vcd \
    --outdir "$OUTDIR" 2>&1)"

status=0

# 1. The test itself must pass (proves the post_eval service ran and the
#    DUT reached the settled state we assert on).
if [[ "$out" == *"ALL TESTS PASSED"* ]]; then
    echo "  PASS  test reached ALL TESTS PASSED"
else
    echo "  FAIL  test did not report ALL TESTS PASSED"
    echo "$out" | tail -30 | sed 's/^/      /'
    status=1
fi

# 2. No duplicate same-timestamp VCD dump warnings (the issue #477 symptom).
dup="$(echo "$out" | grep -c "previous dump at t=")"
if [ "$dup" -eq 0 ]; then
    echo "  PASS  no 'previous dump at t=' warnings"
else
    echo "  FAIL  found $dup 'previous dump at t=' warning(s) — issue #477 regression"
    echo "$out" | grep "previous dump at t=" | head -5 | sed 's/^/      /'
    status=1
fi

# 3. A non-empty VCD must actually have been produced.
vcd="$(find "$OUTDIR" -name '*.vcd' -size +0c 2>/dev/null | head -1)"
if [ -n "$vcd" ]; then
    echo "  PASS  non-empty VCD produced ($(basename "$vcd"))"
else
    echo "  FAIL  no non-empty .vcd file was produced in $OUTDIR"
    status=1
fi

echo
if [ "$status" -eq 0 ]; then
    echo "Result: VCD dup-dump smoke test passed"
else
    echo "Result: VCD dup-dump smoke test FAILED"
fi
exit "$status"
