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
REGISTRY="tests/tbir_equiv_fixtures.txt"
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

if [ ! -f "$REGISTRY" ]; then
    echo "error: registry $REGISTRY not found (run from the repo root)" >&2
    exit 1
fi

# TLM fixtures: run once with the default (v1) codegen. These four
# still carry TB-IR blockers past the bus slice (`fork`/`join_all` TLM
# issue for the pairing pair, `struct` for the burst pair — see
# docs/tbir-coverage.md), so they stay v1-only and are NOT equivalence
# fixtures — they keep their own table here rather than living in the
# equivalence registry.
read -r -d '' FIXTURES <<'EOF' || true
tlm_pairing_arch_target_test    | TlmPairingArchTarget    | TlmPairingArchTarget.arch
tlm_pairing_arch_initiator_test | TlmPairingArchInitiator | TlmPairingArchInitiator.arch
tlm_pairing_arch_burst_target_test    | TlmPairingArchBurstTarget    | TlmPairingArchBurstTarget.arch
tlm_pairing_arch_burst_initiator_test | TlmPairingArchBurstInitiator | TlmPairingArchBurstInitiator.arch
EOF

# TB-IR-capable fixtures: read from the equivalence registry
# (tests/tbir_equiv_fixtures.txt — single source of truth; schema v3:
# test_name | top | sv_files | arch_dut | expect | extra_harc | ref_src
# | test_struct, where the last three columns are optional and `-` =
# none; v2 5-column rows parse unchanged). Every row whose arch_dut
# column is not `-` runs under BOTH codegens (v1 and tbir) against the
# ARCH-native DUT, then `harc trace-diff` the two semantic traces —
# same structure as tests/run_tbir_equiv.sh, but on the `--dut`
# backend. extra_harc files join the .harc input list and test_struct
# is passed via `--test`; ref_src is parsed but ignored here (it names
# C/C++ reference models for the Verilator path — not applicable to
# the ARCH-native backend).

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

    if [[ "$out" == *"ALL TESTS PASSED"* ]]; then
        echo "  PASS  $test"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  $test"
        echo "$out" | tail -30 | sed 's/^/      /'
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
    fi
}

# run_equiv_sim <tag> <outdir> <codegen> <test> <top> <dut> <expect>
# One `harc sim --dut` invocation under the given codegen, recording a
# semantic trace and requiring the registry verdict (`pass` rows must
# print ALL TESTS PASSED; `fail` rows must exit nonzero without it).
# Each codegen gets its OWN outdir (mirrors run_tbir_equiv.sh:
# write_if_changed/obj_dir reuse keys off the emitted .cpp, so sharing
# one directory across codegens risks stale objects).
run_equiv_sim() {
    local tag="$1" dir="$2" cg="$3" test="$4" top="$5" dut="$6" expect="$7"
    mkdir -p "$dir"
    local out rc
    # HARC_FILES / ROW_TEST_ARGS are set per-row by run_equiv_one
    # (schema-v3 extra_harc files and the optional --test selector).
    out="$("$HARC" sim --arch-bin "$ARCH_BIN" --dut "$DUT_DIR/$dut" \
        "${HARC_FILES[@]}" --top "$top" \
        ${ROW_TEST_ARGS[@]+"${ROW_TEST_ARGS[@]}"} \
        --codegen "$cg" --seed "$SEED" --outdir "$dir" \
        --record-trace "$dir/t.jsonl" 2>&1)"
    rc=$?

    if [ "$expect" = "fail" ]; then
        # Mirrors run_tbir_equiv.sh: nonzero exit, no success sentinel,
        # and a real "N TESTS FAILED" verdict (not infra breakage).
        if [ "$rc" -eq 0 ] || [[ "$out" == *"ALL TESTS PASSED"* ]]; then
            echo "  FAIL  $tag (sim --codegen $cg passed, but registry expects fail)"
            echo "$out" | tail -20 | sed 's/^/      /'
            return 1
        fi
        if [[ "$out" != *"TESTS FAILED"* ]]; then
            echo "  FAIL  $tag (sim --codegen $cg broke before reaching a test verdict)"
            echo "$out" | tail -20 | sed 's/^/      /'
            return 1
        fi
        return 0
    fi

    if [[ "$out" != *"ALL TESTS PASSED"* ]]; then
        echo "  FAIL  $tag (sim --codegen $cg did not pass)"
        echo "$out" | tail -20 | sed 's/^/      /'
        return 1
    fi
    return 0
}

run_equiv_one() {
    local row="$1"
    # Schema v3: the last three columns are optional (empty when a v2
    # row omits them; `-` = none).
    local test top sv arch_dut expect extra_harc ref_src test_struct
    IFS='|' read -r test top sv arch_dut expect extra_harc ref_src test_struct <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    arch_dut="$(echo "$arch_dut" | xargs)"
    expect="$(echo "$expect" | xargs)"
    extra_harc="$(echo "$extra_harc" | xargs)"
    test_struct="$(echo "$test_struct" | xargs)"
    [ "$extra_harc" = "-" ] && extra_harc=""
    [ "$test_struct" = "-" ] && test_struct=""
    # ref_src intentionally unused here — see the registry-consumer
    # comment above.
    [ -z "$test" ] && return 0
    if [ -z "$top" ] || [ -z "$arch_dut" ] \
        || { [ "$expect" != "pass" ] && [ "$expect" != "fail" ]; }; then
        echo "  FAIL  $test (malformed registry row: '$row')"
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
        return 0
    fi
    # Rows without a proven ARCH DUT sibling skip the --dut sweep.
    [ "$arch_dut" = "-" ] && return 0
    if [ ! -f "$DUT_DIR/$arch_dut" ]; then
        echo "  FAIL  $test (registry arch_dut '$arch_dut' not found in $DUT_DIR)"
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
        return 0
    fi

    HARC_FILES=("$FIX_DIR/$test.harc")
    local f
    for f in $extra_harc; do HARC_FILES+=("$FIX_DIR/$f"); done
    ROW_TEST_ARGS=()
    [ -n "$test_struct" ] && ROW_TEST_ARGS+=("--test" "$test_struct")

    local pair_dir="$OUT/$test${test_struct:+__$test_struct}"
    local tag="$test${test_struct:+ [$test_struct]} [arch-dut]"
    if ! run_equiv_sim "$tag" "$pair_dir/v1" v1 "$test" "$top" "$arch_dut" "$expect" \
        || ! run_equiv_sim "$tag" "$pair_dir/tbir" tbir "$test" "$top" "$arch_dut" "$expect"; then
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
    echo "  PASS  $tag (v1 == tbir, expect=$expect)"
    PASS=$((PASS + 1))
}

echo "Running ${PWD}/$DUT_DIR ARCH DUT fixtures..."
while IFS= read -r row; do
    run_one "$row"
done <<<"$FIXTURES"

rm -rf "$OUT"
mkdir -p "$OUT"
echo "Running ${PWD}/$REGISTRY ARCH DUT v1-vs-tbir equivalence fixtures (seed $SEED)..."
while IFS= read -r row; do
    # Strip comments; skip blank lines.
    row="${row%%#*}"
    [ -z "$(echo "$row" | xargs)" ] && continue
    run_equiv_one "$row"
done < "$REGISTRY"

echo
echo "Result: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "Failed: ${FAILED_NAMES[*]}"
    exit 1
fi
