#!/bin/bash
# Run HARC fixtures that use ARCH sources directly through `harc sim --dut`.
#
# This is intentionally separate from run_fixtures.sh because HARC's normal CI
# path uses vendored SystemVerilog snapshots and does not require a sibling
# arch-com checkout. Use this script when validating the ARCH/HARC co-sim path.
set -uo pipefail

HARC="${HARC:-./target/release/harc}"
ARCH_BIN="${ARCH_BIN:-../arch-com/target/release/arch}"
FIX_DIR="tests/fixtures"
DUT_DIR="tests/dut"
SEED="${SEED:-1}"
OUT="${ARCH_DUT_OUT:-harc_arch_dut_build}"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc
fi

if [ ! -x "$ARCH_BIN" ]; then
    echo "error: ARCH_BIN=$ARCH_BIN is not executable; set ARCH_BIN=/path/to/arch" >&2
    exit 1
fi

# TLM fixtures: run once with the default (v1) codegen. TB-IR lowering
# does not support the `bus` construct yet, so these stay v1-only.
read -r -d '' FIXTURES <<'EOF' || true
tlm_pairing_arch_target_test    | TlmPairingArchTarget    | TlmPairingArchTarget.arch
tlm_pairing_arch_initiator_test | TlmPairingArchInitiator | TlmPairingArchInitiator.arch
tlm_pairing_arch_burst_target_test    | TlmPairingArchBurstTarget    | TlmPairingArchBurstTarget.arch
tlm_pairing_arch_burst_initiator_test | TlmPairingArchBurstInitiator | TlmPairingArchBurstInitiator.arch
EOF

# TB-IR-capable fixtures: run under BOTH codegens (v1 and tbir) against
# the ARCH-native DUT, then `harc trace-diff` the two semantic traces —
# same structure as tests/run_tbir_equiv.sh, but on the `--dut` backend.
read -r -d '' EQUIV_FIXTURES <<'EOF' || true
top_counter_test | Top        | top_counter.arch
sync_fifo_test   | TxQueue    | sync_fifo.arch
rom_lut_test     | RomLut     | rom_lut.arch
bus_arbiter_test | BusArbiter | bus_arbiter.arch
EOF

PASS=0
FAIL=0
FAILED_NAMES=()

run_one() {
    local row="$1"
    IFS='|' read -r test top dut <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    dut="$(echo "$dut" | xargs)"
    [ -z "$test" ] && return 0

    rm -rf harc_sim_build
    local out
    out="$("$HARC" sim --arch-bin "$ARCH_BIN" --dut "$DUT_DIR/$dut" "$FIX_DIR/$test.harc" --top "$top" 2>&1)" || true

    if echo "$out" | grep -q "ALL TESTS PASSED"; then
        echo "  PASS  $test"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  $test"
        echo "$out" | tail -30 | sed 's/^/      /'
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
    fi
}

# run_equiv_sim <tag> <outdir> <codegen> <test> <top> <dut>
# One `harc sim --dut` invocation under the given codegen, recording a
# semantic trace. Each codegen gets its OWN outdir (mirrors
# run_tbir_equiv.sh: write_if_changed/obj_dir reuse keys off the emitted
# .cpp, so sharing one directory across codegens risks stale objects).
run_equiv_sim() {
    local tag="$1" dir="$2" cg="$3" test="$4" top="$5" dut="$6"
    mkdir -p "$dir"
    local out
    out="$("$HARC" sim --arch-bin "$ARCH_BIN" --dut "$DUT_DIR/$dut" \
        "$FIX_DIR/$test.harc" --top "$top" \
        --codegen "$cg" --seed "$SEED" --outdir "$dir" \
        --record-trace "$dir/t.jsonl" 2>&1)" || true

    if ! echo "$out" | grep -q "ALL TESTS PASSED"; then
        echo "  FAIL  $tag (sim --codegen $cg did not pass)"
        echo "$out" | tail -20 | sed 's/^/      /'
        return 1
    fi
    return 0
}

run_equiv_one() {
    local row="$1"
    IFS='|' read -r test top dut <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    dut="$(echo "$dut" | xargs)"
    [ -z "$test" ] && return 0

    local pair_dir="$OUT/$test"
    local tag="$test [arch-dut]"
    if ! run_equiv_sim "$tag" "$pair_dir/v1" v1 "$test" "$top" "$dut" \
        || ! run_equiv_sim "$tag" "$pair_dir/tbir" tbir "$test" "$top" "$dut"; then
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$tag")
        return 0
    fi

    local out
    if ! out="$("$HARC" trace-diff "$pair_dir/v1/t.jsonl" "$pair_dir/tbir/t.jsonl" 2>&1)"; then
        echo "  FAIL  $tag (v1 vs tbir trace divergence)"
        echo "$out" | tail -30 | sed 's/^/      /'
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$tag")
        return 0
    fi
    echo "  PASS  $tag (v1 == tbir)"
    PASS=$((PASS + 1))
}

echo "Running ${PWD}/$DUT_DIR ARCH DUT fixtures..."
while IFS= read -r row; do
    run_one "$row"
done <<<"$FIXTURES"

rm -rf "$OUT"
mkdir -p "$OUT"
echo "Running ${PWD}/$DUT_DIR ARCH DUT v1-vs-tbir equivalence fixtures (seed $SEED)..."
while IFS= read -r row; do
    run_equiv_one "$row"
done <<<"$EQUIV_FIXTURES"

echo
echo "Result: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "Failed: ${FAILED_NAMES[*]}"
    exit 1
fi
