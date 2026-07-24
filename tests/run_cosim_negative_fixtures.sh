#!/usr/bin/env bash
# Negative fixtures through the `--cosim dpi` backend: exercises the
# failure half of the co-sim step protocol (RC_DONE_FAIL -> $fatal ->
# nonzero exit) that the all-green parity gate never touches. Same
# pass criterion as run_negative_fixtures.sh: the expected failure-mode
# log line appears AND "ALL TESTS PASSED" does not.
set -euo pipefail
cd "$(dirname "$0")/.."

export HARC_SIM_EXTRA_ARGS="--cosim dpi"
export NEG_BUILD_ROOT="harc_sim_build_cosim_neg"
exec bash tests/run_negative_fixtures.sh "$@"
