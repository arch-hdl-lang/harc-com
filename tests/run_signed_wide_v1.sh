#!/bin/bash
# Run the narrow signed-wide ordering fixture through both native backends.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
HARC="${HARC:-$ROOT/target/debug/harc}"
if [ ! -x "$HARC" ]; then
    cargo build --offline --bin harc
fi

out_root="$(mktemp -d "${TMPDIR:-/tmp}/harc_signed_wide_v1.XXXXXX")"
trap 'rm -rf "$out_root"' EXIT

run_backend() {
    local name="$1"
    shift
    local output
    output="$(HARC_NO_LEARN=1 "$HARC" sim \
        --sv tests/dut/top_counter.sv \
        tests/fixtures/signed_wide_ordering_v1_test.harc \
        --top Top --jobs 2 --outdir "$out_root/$name" --rebuild "$@" 2>&1)"
    printf '%s\n' "$output"
    [[ "$output" == *"ALL TESTS PASSED"* ]]
}

run_backend v1 --codegen v1
run_backend default
echo "signed wide v1 regression: PASS"
