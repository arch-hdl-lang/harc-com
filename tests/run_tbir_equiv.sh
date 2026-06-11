#!/bin/bash
# v1-vs-tbir codegen equivalence harness.
#
# For every fixture row in tests/tbir_equiv_fixtures.txt, run the HARC
# testbench through BOTH C++ emitters (`harc sim --codegen v1` and
# `--codegen tbir`) against the vendored SV DUT, assert "ALL TESTS
# PASSED" under each, then `harc trace-diff` the two semantic JSONL
# traces. Any test failure or trace divergence fails the run.
#
# Run from harc-com repo root:
#     ./tests/run_tbir_equiv.sh
#     SEED=7 ./tests/run_tbir_equiv.sh      # override the sim seed
#
# Set HARC=/path/to/harc to override the binary location (default: build
# from `cargo build --release --bin harc`).
#
# Optional ARCH-native-DUT sweep: when ARCH_BIN points at an `arch`
# binary AND a matching tests/dut/<stem>.arch exists for every SV file
# in a row, the same v1/tbir pair also runs through `harc sim --dut`
# (ARCH native sim backend) and is trace-diffed. Skipped silently
# otherwise — CI has no arch checkout.
set -uo pipefail

HARC="${HARC:-./target/release/harc}"
DUT_DIR="tests/dut"
FIX_DIR="tests/fixtures"
REGISTRY="tests/tbir_equiv_fixtures.txt"
SEED="${SEED:-1}"
OUT="${TBIR_EQUIV_OUT:-harc_tbir_equiv_build}"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc
fi

if [ ! -f "$REGISTRY" ]; then
    echo "error: registry $REGISTRY not found (run from the repo root)" >&2
    exit 1
fi

rm -rf "$OUT"
mkdir -p "$OUT"

PASS=0
FAIL=0
FAILED_NAMES=()

# run_sim <verdict_tag> <outdir> <codegen> <dut|sv> <top> <files...>
# Runs one `harc sim` invocation, requiring "ALL TESTS PASSED" in the
# output. Reads the HARC_FILES global array (the .harc inputs for the
# current registry row, set by run_one). Each codegen gets its OWN outdir: harc's write_if_changed /
# obj_dir reuse keys off the emitted .cpp in the outdir, so sharing one
# directory across codegens would alternate rebuilds and risk stale-
# object confusion.
run_sim() {
    local tag="$1" dir="$2" cg="$3" backend="$4" top="$5"
    shift 5
    local backend_args=()
    local f
    for f in "$@"; do backend_args+=("--$backend" "$f"); done
    if [ "$backend" = "dut" ]; then
        backend_args+=("--arch-bin" "$ARCH_BIN")
    fi

    mkdir -p "$dir"
    local out
    out="$("$HARC" sim "${backend_args[@]}" "${HARC_FILES[@]}" --top "$top" \
        --codegen "$cg" --seed "$SEED" --outdir "$dir" \
        --record-trace "$dir/t.jsonl" 2>&1)" || true

    if ! echo "$out" | grep -q "ALL TESTS PASSED"; then
        echo "  FAIL  $tag (sim --codegen $cg did not pass)"
        echo "$out" | tail -20 | sed 's/^/      /'
        return 1
    fi
    return 0
}

# run_pair <verdict_tag> <pair_dir> <dut|sv> <top> <files...>
# v1 run + tbir run + trace-diff. Fails fast within the pair: a failed
# v1 run skips the tbir run and the diff.
run_pair() {
    local tag="$1" pair_dir="$2" backend="$3" top="$4"
    shift 4

    run_sim "$tag" "$pair_dir/v1" v1 "$backend" "$top" "$@" || return 1
    run_sim "$tag" "$pair_dir/tbir" tbir "$backend" "$top" "$@" || return 1

    local out
    out="$("$HARC" trace-diff "$pair_dir/v1/t.jsonl" "$pair_dir/tbir/t.jsonl" 2>&1)"
    if [ $? -ne 0 ]; then
        echo "  FAIL  $tag (v1 vs tbir trace divergence)"
        echo "$out" | tail -30 | sed 's/^/      /'
        return 1
    fi
    return 0
}

run_one() {
    local row="$1"
    local test top sv
    IFS='|' read -r test top sv <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    sv="$(echo "$sv" | xargs)"
    [ -z "$test" ] && return 0
    if [ -z "$top" ] || [ -z "$sv" ]; then
        echo "  FAIL  $test (malformed registry row: '$row')"
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
        return 0
    fi

    HARC_FILES=("$FIX_DIR/$test.harc")

    local sv_files=()
    local f
    for f in $sv; do sv_files+=("$DUT_DIR/$f"); done

    if ! run_pair "$test" "$OUT/$test" sv "$top" "${sv_files[@]}"; then
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
        return 0
    fi
    echo "  PASS  $test (v1 == tbir)"
    PASS=$((PASS + 1))

    # Optional ARCH-native-DUT sweep. Only when ARCH_BIN is set AND every
    # SV file has a sibling .arch source; skip silently otherwise.
    if [ -n "${ARCH_BIN:-}" ] && [ -x "${ARCH_BIN:-}" ]; then
        local arch_files=()
        for f in $sv; do
            local a="$DUT_DIR/${f%.sv}.arch"
            [ -f "$a" ] || return 0
            arch_files+=("$a")
        done
        if ! run_pair "$test [arch-dut]" "$OUT/${test}__arch_dut" dut "$top" "${arch_files[@]}"; then
            FAIL=$((FAIL + 1))
            FAILED_NAMES+=("$test [arch-dut]")
            return 0
        fi
        echo "  PASS  $test [arch-dut] (v1 == tbir)"
        PASS=$((PASS + 1))
    fi
}

echo "Running ${PWD}/$REGISTRY v1-vs-tbir equivalence fixtures (seed $SEED)..."
while IFS= read -r row; do
    # Strip comments; skip blank lines.
    row="${row%%#*}"
    [ -z "$(echo "$row" | xargs)" ] && continue
    run_one "$row"
done < "$REGISTRY"

echo
echo "Result: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "Failed: ${FAILED_NAMES[*]}"
    exit 1
fi
if [ $PASS -eq 0 ]; then
    echo "error: no fixtures ran — registry $REGISTRY has no active rows" >&2
    exit 1
fi
