#!/usr/bin/env bash
# Benchmark TB-IR split emission across `--emit-jobs` values.
#
# Measures the frontend only (`--emit-only`), so nothing here depends on
# Verilator. Reports the per-phase numbers harc prints plus total wall
# time, and repeats each configuration so the noise is visible rather than
# averaged away.
#
# Usage:
#   bench/split_emit/run_bench.sh [workdir] [group_size] [reps]
#
# Build the release binary first: `cargo build --release`.
set -euo pipefail

WORKDIR="${1:-/tmp/harc_split_emit_bench}"
GROUP_SIZE="${2:-32}"
REPS="${3:-3}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HARC="$REPO_ROOT/target/release/harc"

if [[ ! -x "$HARC" ]]; then
  echo "error: $HARC not found; run 'cargo build --release' first" >&2
  exit 1
fi

if [[ ! -f "$WORKDIR/suite.harc" ]]; then
  echo "generating suite in $WORKDIR"
  python3 "$REPO_ROOT/bench/split_emit/gen_suite.py" --outdir "$WORKDIR"
fi

echo "harc:       $HARC"
echo "suite:      $WORKDIR/suite.harc"
echo "group size: $GROUP_SIZE"
echo "cpus:       $(nproc)"
echo

for jobs in 1 2 4; do
  for rep in $(seq 1 "$REPS"); do
    out="$WORKDIR/out_j${jobs}"
    # Clean between reps: an unchanged rerun skips every write, which
    # measures a different thing (see README).
    rm -rf "$out"
    start=$(date +%s.%N)
    log=$("$HARC" sim "$WORKDIR/suite.harc" \
      --sv "$WORKDIR/SplitAdder.sv" \
      --top SplitAdder \
      --codegen tbir \
      --cpp-split tests \
      --cpp-split-group-size "$GROUP_SIZE" \
      --emit-only \
      --outdir "$out" \
      --emit-jobs "$jobs" 2>&1)
    end=$(date +%s.%N)
    total=$(echo "$end - $start" | bc)
    parse=$(sed -n 's/^TBIR parse: \(.*\) | merge: .*/\1/p' <<<"$log")
    lower=$(sed -n 's/^TBIR lower: \(.*\) | verify: .*/\1/p' <<<"$log")
    plan=$(sed -n 's/^TBIR split plan: .*, planned in //p' <<<"$log")
    emit=$(sed -n 's/^TBIR split emit: .*, //p' <<<"$log")
    echo "jobs=$jobs rep=$rep  total=${total}s  parse=${parse}  lower=${lower}  plan=${plan}  emit=${emit}"
  done
done

echo
echo "unchanged rerun at jobs=4 (read+compare, no writes) — isolates write I/O:"
for rep in $(seq 1 "$REPS"); do
  "$HARC" sim "$WORKDIR/suite.harc" \
    --sv "$WORKDIR/SplitAdder.sv" \
    --top SplitAdder \
    --codegen tbir \
    --cpp-split tests \
    --cpp-split-group-size "$GROUP_SIZE" \
    --emit-only \
    --outdir "$WORKDIR/out_j4" \
    --emit-jobs 4 2>&1 | sed -n 's/^TBIR split emit: /  /p'
done
