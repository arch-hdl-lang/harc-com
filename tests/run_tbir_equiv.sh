#!/bin/bash
# v1-vs-tbir codegen equivalence harness.
#
# For every fixture row in tests/tbir_equiv_fixtures.txt, run the HARC
# testbench through BOTH C++ emitters (`harc sim --codegen v1` and
# `--codegen tbir`) against the vendored SV DUT, assert the row's
# expected verdict under each, then `harc trace-diff` the two semantic
# JSONL traces. Any verdict mismatch or trace divergence fails the run.
#
# Verdicts (registry `expect` column, schema v3):
#   pass — each sim must print "ALL TESTS PASSED".
#   fail — each sim must exit nonzero AND not print "ALL TESTS PASSED"
#          (deliberate-failure fixtures, e.g. the log(fatal, ...) path).
# In both cases the v1/tbir traces must trace-diff clean — a failing
# fixture must fail IDENTICALLY under both codegens.
#
# Run from harc-com repo root:
#     ./tests/run_tbir_equiv.sh
#     SEED=7 ./tests/run_tbir_equiv.sh      # override the sim seed
#
# Set HARC=/path/to/harc to override the binary location (default: build
# from `cargo build --release --bin harc`).
#
# Optional ARCH-native-DUT sweep: when ARCH_BIN points at an `arch`
# binary AND the row's arch_dut column names a tests/dut/<file>.arch
# source (rows with `-` self-skip), the same v1/tbir pair also runs
# through `harc sim --dut` (ARCH native sim backend) and is trace-diffed.
# Skipped silently when ARCH_BIN is unset — CI has no arch checkout.
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
# Runs one `harc sim` invocation, requiring the verdict named by the
# ROW_EXPECT global (`pass` or `fail`, set by run_one from the registry
# row). Reads the HARC_FILES and ROW_EXTRA_ARGS global arrays (the
# .harc inputs and the row's --ref-src/--test plumbing, set by
# run_one). Each codegen gets its OWN outdir: harc's write_if_changed /
# obj_dir reuse keys off the emitted .cpp in the outdir, so sharing one
# directory across codegens would alternate rebuilds and risk
# stale-object confusion.
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
    local out rc
    # `${ROW_EXTRA_ARGS[@]+...}` tolerates an empty array under `set -u`
    # (most rows carry no --ref-src/--test extras).
    out="$("$HARC" sim "${backend_args[@]}" "${HARC_FILES[@]}" --top "$top" \
        ${ROW_EXTRA_ARGS[@]+"${ROW_EXTRA_ARGS[@]}"} \
        --codegen "$cg" --seed "$SEED" --outdir "$dir" \
        --record-trace "$dir/t.jsonl" 2>&1)"
    rc=$?

    if [ "$ROW_EXPECT" = "fail" ]; then
        # Expect-fail row: the sim must exit nonzero and must NOT claim
        # success. Requiring the "N TESTS FAILED" verdict line separates
        # a deliberate test failure from infrastructure breakage (compile
        # error, Verilator failure). The trace-diff in run_pair then
        # asserts the failure was recorded identically under both
        # codegens.
        if [ "$rc" -eq 0 ] || echo "$out" | grep -q "ALL TESTS PASSED"; then
            echo "  FAIL  $tag (sim --codegen $cg passed, but registry expects fail)"
            echo "$out" | tail -20 | sed 's/^/      /'
            return 1
        fi
        if ! echo "$out" | grep -q "TESTS FAILED"; then
            echo "  FAIL  $tag (sim --codegen $cg broke before reaching a test verdict)"
            echo "$out" | tail -20 | sed 's/^/      /'
            return 1
        fi
        return 0
    fi

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
    # Schema v3: the last three columns are optional (empty when a v2
    # row omits them; `-` = none).
    local test top sv arch_dut expect extra_harc ref_src test_struct
    IFS='|' read -r test top sv arch_dut expect extra_harc ref_src test_struct <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    sv="$(echo "$sv" | xargs)"
    arch_dut="$(echo "$arch_dut" | xargs)"
    expect="$(echo "$expect" | xargs)"
    extra_harc="$(echo "$extra_harc" | xargs)"
    ref_src="$(echo "$ref_src" | xargs)"
    test_struct="$(echo "$test_struct" | xargs)"
    [ "$extra_harc" = "-" ] && extra_harc=""
    [ "$ref_src" = "-" ] && ref_src=""
    [ "$test_struct" = "-" ] && test_struct=""
    [ -z "$test" ] && return 0
    if [ -z "$top" ] || [ -z "$sv" ] || [ -z "$arch_dut" ] \
        || { [ "$expect" != "pass" ] && [ "$expect" != "fail" ]; }; then
        echo "  FAIL  $test (malformed registry row: '$row')"
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
        return 0
    fi

    HARC_FILES=("$FIX_DIR/$test.harc")
    local f
    for f in $extra_harc; do HARC_FILES+=("$FIX_DIR/$f"); done
    ROW_EXTRA_ARGS=()
    for f in $ref_src; do ROW_EXTRA_ARGS+=("--ref-src" "$DUT_DIR/$f"); done
    [ -n "$test_struct" ] && ROW_EXTRA_ARGS+=("--test" "$test_struct")
    ROW_EXPECT="$expect"

    local sv_files=()
    for f in $sv; do sv_files+=("$DUT_DIR/$f"); done

    # A fixture file with multiple test structs registers one row per
    # struct — suffix the label/outdir so the runs don't collide.
    local label="$test${test_struct:+ [$test_struct]}"
    local pair_dir="$OUT/$test${test_struct:+__$test_struct}"
    if ! run_pair "$label" "$pair_dir" sv "$top" "${sv_files[@]}"; then
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$label")
        return 0
    fi
    echo "  PASS  $label (v1 == tbir, expect=$expect)"
    PASS=$((PASS + 1))

    # Optional ARCH-native-DUT sweep. Only when ARCH_BIN is set AND the
    # row names an arch_dut source; skip silently otherwise.
    if [ -n "${ARCH_BIN:-}" ] && [ -x "${ARCH_BIN:-}" ] && [ "$arch_dut" != "-" ]; then
        if [ ! -f "$DUT_DIR/$arch_dut" ]; then
            echo "  FAIL  $label [arch-dut] (registry arch_dut '$arch_dut' not found in $DUT_DIR)"
            FAIL=$((FAIL + 1))
            FAILED_NAMES+=("$label [arch-dut]")
            return 0
        fi
        if ! run_pair "$label [arch-dut]" "${pair_dir}__arch_dut" dut "$top" "$DUT_DIR/$arch_dut"; then
            FAIL=$((FAIL + 1))
            FAILED_NAMES+=("$label [arch-dut]")
            return 0
        fi
        echo "  PASS  $label [arch-dut] (v1 == tbir, expect=$expect)"
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
