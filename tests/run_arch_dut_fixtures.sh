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

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc
fi

if [ ! -x "$ARCH_BIN" ]; then
    echo "error: ARCH_BIN=$ARCH_BIN is not executable; set ARCH_BIN=/path/to/arch" >&2
    exit 1
fi

read -r -d '' FIXTURES <<'EOF' || true
tlm_pairing_arch_target_test    | TlmPairingArchTarget    | TlmPairingArchTarget.arch
tlm_pairing_arch_initiator_test | TlmPairingArchInitiator | TlmPairingArchInitiator.arch
tlm_pairing_arch_burst_target_test    | TlmPairingArchBurstTarget    | TlmPairingArchBurstTarget.arch
tlm_pairing_arch_burst_initiator_test | TlmPairingArchBurstInitiator | TlmPairingArchBurstInitiator.arch
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

echo "Running ${PWD}/$DUT_DIR ARCH DUT fixtures..."
while IFS= read -r row; do
    run_one "$row"
done <<<"$FIXTURES"

echo
echo "Result: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "Failed: ${FAILED_NAMES[*]}"
    exit 1
fi
