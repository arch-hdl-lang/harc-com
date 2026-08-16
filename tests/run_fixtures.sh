#!/bin/bash
# Run every HARC fixture that has a vendored DUT through `harc sim --sv`
# and assert "ALL TESTS PASSED" appears in the output.
#
# Run from harc-com repo root:
#     ./tests/run_fixtures.sh
#
# Set HARC=/path/to/harc to override the binary location (default: build
# from `cargo build --release --bin harc`).
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HARC="${HARC:-./target/release/harc}"
DUT_DIR="tests/dut"
FIX_DIR="tests/fixtures"
# Per-fixture verilator build dirs live under BUILD_ROOT so parallel
# workers never clobber each other's obj_dir.
BUILD_ROOT="${FIXTURE_BUILD_ROOT:-harc_sim_build}"
# Parallelism: each fixture is a verilator+clang compile (~3s, dominated
# by compilation, not simulation), so fan out across cores. Override with
# JOBS=N; default to the online CPU count.
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc
fi

# Make HARC an absolute path so re-exec'd workers (which inherit the repo
# root as cwd) resolve the same binary regardless of how it was passed.
case "$HARC" in
    /*) ;;
    *) HARC="$PWD/$HARC" ;;
esac

# This table is the ONLY thing that decides what runs — fixtures are not
# auto-discovered from tests/fixtures/. A new fixture without a row here is
# silently never executed, so tests/check_fixture_registration.sh enforces that
# every DUT-driving fixture is reachable from some runner.
#
# Each row: <test_name> <top_module> <sv_files> <extra_harc_files> <ref_src> <test_struct>
# Fields are pipe-separated. SV files are relative to DUT_DIR; HARC files
# are relative to FIX_DIR. Multiple files within a field are space-
# separated. The 6th field (test_struct) is passed via `--test` when the
# loaded harc files declare more than one test struct.
TABLE="$SCRIPT_DIR/fixtures.tbl"
# Fail CLOSED. This was a heredoc until the table was shared with
# run_emit_parity.sh; a heredoc could not go missing, a path can. Without
# this the primary sim gate reports success over an empty corpus.
[ -s "$TABLE" ] || { echo "error: $TABLE missing or empty" >&2; exit 1; }
FIXTURES="$(cat "$TABLE")"

# run_one <outdir> <row>
# Pure worker: runs one fixture and prints its human log, ending with a
# single machine-readable status line `__STATUS__ <PASS|FAIL> <name>`.
# It mutates no globals (so it is safe to run in a parallel subshell) —
# the parent aggregates the status lines.
run_one() {
    local outdir="$1" row="$2"
    # Optional 5th field: space-separated C/C++ reference-model source
    # files relative to DUT_DIR, passed via `--ref-src`. Used by
    # `extern function` tests (spec §9).
    local test top sv extras ref_src test_struct
    IFS='|' read -r test top sv extras ref_src test_struct <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    sv="$(echo "$sv" | xargs)"
    extras="$(echo "$extras" | xargs || true)"
    ref_src="$(echo "$ref_src" | xargs || true)"
    test_struct="$(echo "$test_struct" | xargs || true)"
    [ -z "$test" ] && return 0

    local sv_args=()
    local f
    for f in $sv; do sv_args+=("--sv" "$DUT_DIR/$f"); done

    local ref_args=()
    for f in $ref_src; do ref_args+=("--ref-src" "$DUT_DIR/$f"); done

    local harc_files=("$FIX_DIR/$test.harc")
    for f in $extras; do harc_files+=("$FIX_DIR/$f"); done

    local test_args=()
    [ -n "$test_struct" ] && test_args=("--test" "$test_struct")

    rm -rf "$outdir"
    local out
    # `${ref_args[@]:-}` tolerates an empty array under `set -u` (most
    # fixtures don't pass any --ref-src). `--outdir` keeps each worker's
    # verilator build isolated so parallel runs never collide.
    # HARC_SIM_EXTRA_ARGS: optional extra `harc sim` flags, e.g.
    # `--cosim dpi` (see tests/run_cosim_fixtures.sh). Word-split on
    # purpose.
    # shellcheck disable=SC2086
    out="$("$HARC" sim "${sv_args[@]}" ${ref_args[@]+"${ref_args[@]}"} "${harc_files[@]}" --top "$top" ${test_args[@]+"${test_args[@]}"} --outdir "$outdir" ${HARC_SIM_EXTRA_ARGS:-} 2>&1)" || true

    if [[ "$out" == *"ALL TESTS PASSED"* ]]; then
        echo "  PASS  $test"
        echo "__STATUS__ PASS $test"
    else
        echo "  FAIL  $test"
        echo "$out" | tail -20 | sed 's/^/      /'
        echo "__STATUS__ FAIL $test"
    fi
}

# Hidden re-exec entry point: `run_fixtures.sh __worker <outdir> <row>`.
# Lets the parent fan out one process per fixture via xargs -P.
if [ "${1:-}" = "__worker" ]; then
    run_one "$2" "$3"
    exit 0
fi

echo "Running ${PWD}/$DUT_DIR fixtures (JOBS=$JOBS)..."

# Materialize each non-empty row to a numbered file so workers can be
# addressed by index — avoids quoting the pipe-delimited row through xargs.
RESDIR="$(mktemp -d "${TMPDIR:-/tmp}/harc_fixtures.XXXXXX")"
trap 'rm -rf "$RESDIR"' EXIT
n=0
while IFS= read -r row; do
    [ -z "$(echo "$row" | xargs)" ] && continue
    printf '%s\n' "$row" >"$RESDIR/$n.row"
    n=$((n + 1))
done <<<"$FIXTURES"

# Fan out: one worker per row, JOBS at a time. Each worker's stdout is
# captured to <idx>.log so the aggregated output below is ordered and
# never interleaved.
export HARC DUT_DIR FIX_DIR BUILD_ROOT HARC_SIM_EXTRA_ARGS
read -r -d '' WORKER_CMD <<'EOF' || true
idx="$1"; resdir="$2"; self="$3"; build_root="$4"
bash "$self" __worker "$build_root/worker_$idx" "$(cat "$resdir/$idx.row")" \
    >"$resdir/$idx.log" 2>&1
EOF
seq 0 $((n - 1)) | xargs -P "$JOBS" -I {} bash -c "$WORKER_CMD" _ {} "$RESDIR" "$0" "$BUILD_ROOT"

PASS=0
FAIL=0
FAILED_NAMES=()
for ((i = 0; i < n; i++)); do
    # Echo the worker's log minus the machine status line.
    if [ -f "$RESDIR/$i.log" ]; then
        grep -v '^__STATUS__ ' "$RESDIR/$i.log"
    fi
    status="$(grep '^__STATUS__ ' "$RESDIR/$i.log" 2>/dev/null | tail -1)"
    case "$status" in
        "__STATUS__ PASS "*) PASS=$((PASS + 1)) ;;
        "__STATUS__ FAIL "*) FAIL=$((FAIL + 1)); FAILED_NAMES+=("${status#__STATUS__ FAIL }") ;;
        *)
            # Worker produced no status line — crash / killed. Treat as FAIL.
            name="$(head -1 "$RESDIR/$i.row" 2>/dev/null | cut -d'|' -f1 | xargs)"
            echo "  FAIL  ${name:-<row $i>} (worker produced no verdict)"
            FAIL=$((FAIL + 1)); FAILED_NAMES+=("${name:-<row $i>}")
            ;;
    esac
done

echo
echo "Result: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "Failed: ${FAILED_NAMES[*]}"
    exit 1
fi
