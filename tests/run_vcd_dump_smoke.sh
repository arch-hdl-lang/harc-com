#!/bin/bash
# End-to-end waveform smoke test for issue #477 plus #619 M4 lifecycle
# feature interactions. A clocked testbench with an active
# `phase post_eval` transactor service, run under VCD tracing, must NOT
# produce Verilator `previous dump at t=..., dump call ignored` warnings
# (which mean a duplicate same-timestamp dump silently dropped the settled
# post_eval state). A reusable-testbench lifecycle also runs under VCD,
# cooperative-vs-MT trace comparison, and functional coverage JSON.
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
LIFECYCLE_FIXTURE="tb_lifecycle_coverage_test"
LIFECYCLE_TEST="LifecycleCoverageFour"
MT_LIFECYCLE_FIXTURE="tb_lifecycle_wait_setup_test"
MT_LIFECYCLE_TEST="WaitSetupSix"

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

# 4. Native out-of-line reusable-testbench lifecycle under VCD. This is a
# separate build because trace support changes Verilator compile flags.
lc_vcd_out="$OUTDIR/lifecycle_vcd"
lc_vcd_out_text="$("$HARC" sim \
    --sv "$DUT_DIR/$SV" \
    "$FIX_DIR/$LIFECYCLE_FIXTURE.harc" \
    --top "$TOP" --test "$LIFECYCLE_TEST" \
    --waves --wave-format vcd \
    --outdir "$lc_vcd_out" 2>&1)"
if [[ "$lc_vcd_out_text" == *"ALL TESTS PASSED"* ]]; then
    echo "  PASS  lifecycle fixture passed under VCD"
else
    echo "  FAIL  lifecycle fixture did not pass under VCD"
    echo "$lc_vcd_out_text" | tail -30 | sed 's/^/      /'
    status=1
fi
lc_dup="$(echo "$lc_vcd_out_text" | grep -c "previous dump at t=")"
if [ "$lc_dup" -eq 0 ]; then
    echo "  PASS  lifecycle VCD has no duplicate-timestamp warnings"
else
    echo "  FAIL  lifecycle VCD produced $lc_dup duplicate-timestamp warning(s)"
    status=1
fi
lc_vcd="$(find "$lc_vcd_out" -name '*.vcd' -size +0c 2>/dev/null | head -1)"
if [ -n "$lc_vcd" ]; then
    echo "  PASS  lifecycle produced a non-empty VCD ($(basename "$lc_vcd"))"
else
    echo "  FAIL  lifecycle produced no non-empty VCD"
    status=1
fi

# 5. Actor-free CORO lifecycle is deterministic under --mt. Unlike the broad
# MT sweep (verdict-only because actor ordering may vary), this fixture has no
# actors, but its shared setup and teardown both suspend, so the cooperative
# and MT semantic traces must match exactly across real lifecycle yield loops.
lc_feature_out="$OUTDIR/lifecycle_features"
lc_mt_outdir="$OUTDIR/lifecycle_mt_features"
coop_trace="$OUTDIR/lifecycle_coop.jsonl"
mt_trace="$OUTDIR/lifecycle_mt.jsonl"
coop_out="$("$HARC" sim \
    --sv "$DUT_DIR/$SV" \
    "$FIX_DIR/$MT_LIFECYCLE_FIXTURE.harc" \
    --top "$TOP" --test "$MT_LIFECYCLE_TEST" \
    --record-trace "$coop_trace" \
    --outdir "$lc_mt_outdir" 2>&1)"
mt_out="$("$HARC" sim \
    --sv "$DUT_DIR/$SV" \
    "$FIX_DIR/$MT_LIFECYCLE_FIXTURE.harc" \
    --top "$TOP" --test "$MT_LIFECYCLE_TEST" --mt \
    --record-trace "$mt_trace" \
    --outdir "$lc_mt_outdir" 2>&1)"
if [[ "$coop_out" == *"ALL TESTS PASSED"* && "$mt_out" == *"ALL TESTS PASSED"* ]]; then
    if trace_diff_out="$("$HARC" trace-diff "$coop_trace" "$mt_trace" 2>&1)"; then
        echo "  PASS  actor-free Coro lifecycle cooperative trace == MT trace"
    else
        echo "  FAIL  actor-free Coro lifecycle cooperative/MT trace divergence"
        echo "$trace_diff_out" | tail -30 | sed 's/^/      /'
        status=1
    fi
else
    echo "  FAIL  actor-free Coro lifecycle did not pass in cooperative and MT modes"
    echo "$coop_out" | tail -15 | sed 's/^/      coop: /'
    echo "$mt_out" | tail -15 | sed 's/^/      mt: /'
    status=1
fi

# 6. Functional coverage JSON emitted from the bound test's covergroup while
# the reusable setup/check lifecycle remains native and out of line.
coverage_json="$OUTDIR/lifecycle_coverage.jsonl"
coverage_out="$("$HARC" sim \
    --sv "$DUT_DIR/$SV" \
    "$FIX_DIR/$LIFECYCLE_FIXTURE.harc" \
    --top "$TOP" --test "$LIFECYCLE_TEST" \
    --coverage-json "$coverage_json" \
    --outdir "$lc_feature_out" 2>&1)"
if [[ "$coverage_out" == *"ALL TESTS PASSED"* ]] \
    && [ -s "$coverage_json" ] \
    && grep -q '"type":"covergroup"' "$coverage_json" \
    && grep -q '"type":"coverpoint_bin"' "$coverage_json"; then
    echo "  PASS  lifecycle functional coverage JSON contains summary and bin records"
else
    echo "  FAIL  lifecycle functional coverage JSON is missing or incomplete"
    echo "$coverage_out" | tail -20 | sed 's/^/      /'
    status=1
fi

echo
if [ "$status" -eq 0 ]; then
    echo "Result: VCD and lifecycle feature-interaction smoke test passed"
else
    echo "Result: VCD and lifecycle feature-interaction smoke test FAILED"
fi
exit "$status"
