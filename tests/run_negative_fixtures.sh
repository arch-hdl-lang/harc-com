#!/bin/bash
# Run HARC fixtures that *expect* a specific failure-mode log line.
#
# These are negative tests: the standard run_fixtures.sh runner asserts
# "ALL TESTS PASSED" appears in the output, but here we expect the run
# to end with errors > 0 because the design intentionally trips a
# runtime guard (FATAL abort, etc.). Each fixture lists the substring
# we expect to see in the output; the test passes iff that substring
# is present AND the standard "ALL TESTS PASSED" sentinel is absent.
#
# Run from harc-com repo root:
#     ./tests/run_negative_fixtures.sh
#
# Set HARC=/path/to/harc to override the binary location (default: build
# from `cargo build --release --bin harc`).
set -uo pipefail

HARC="${HARC:-./target/release/harc}"
DUT_DIR="tests/dut"
FIX_DIR="tests/fixtures"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc
fi

# Each row: <test_name> | <top_module> | <sv_files> | <expected_substring>
# The expected substring is matched against the combined stdout+stderr
# of the `harc sim` invocation. It MUST be specific enough that
# unrelated log noise can't accidentally satisfy it.
read -r -d '' FIXTURES <<'EOF' || true
regblock_record_recursion_test | AxiLiteRegs | AxiLiteRegs.sv | RAL record_write callback recursion exceeded HARC_RAL_CB_MAX_DEPTH
queue_empty_pop_test | Top | top_counter.sv | FATAL] pop() on an empty queue
EOF

PASS=0
FAIL=0
FAILED_NAMES=()

run_one() {
    local row="$1"
    IFS='|' read -r test top sv expected <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    sv="$(echo "$sv" | xargs)"
    expected="$(echo "$expected" | sed 's/^ *//;s/ *$//')"
    [ -z "$test" ] && return 0

    local sv_args=()
    for f in $sv; do sv_args+=("--sv" "$DUT_DIR/$f"); done

    rm -rf "${NEG_BUILD_ROOT:-harc_sim_build}"
    local out
    # HARC_SIM_EXTRA_ARGS: optional extra `harc sim` flags (e.g.
    # `--cosim dpi` from run_cosim_negative_fixtures.sh). Word-split on
    # purpose.
    # shellcheck disable=SC2086
    out="$("$HARC" sim "${sv_args[@]}" "$FIX_DIR/$test.harc" --top "$top" --outdir "${NEG_BUILD_ROOT:-harc_sim_build}" ${HARC_SIM_EXTRA_ARGS:-} 2>&1)" || true

    local has_expected=0
    local has_passed=0
    if [[ "$out" == *"$expected"* ]]; then has_expected=1; fi
    if [[ "$out" == *"ALL TESTS PASSED"* ]]; then has_passed=1; fi

    if [ "$has_expected" -eq 1 ] && [ "$has_passed" -eq 0 ]; then
        echo "  PASS  $test  (saw expected failure-mode log line)"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  $test"
        if [ "$has_expected" -eq 0 ]; then
            echo "      missing expected substring: \"$expected\""
        fi
        if [ "$has_passed" -eq 1 ]; then
            echo "      run reported ALL TESTS PASSED, expected failure"
        fi
        echo "$out" | tail -20 | sed 's/^/      /'
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
    fi
}

echo "Running ${PWD}/$DUT_DIR negative fixtures..."
while IFS= read -r row; do
    run_one "$row"
done <<<"$FIXTURES"

echo
echo "Result: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "Failed: ${FAILED_NAMES[*]}"
    exit 1
fi
