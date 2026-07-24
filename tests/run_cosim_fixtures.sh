#!/usr/bin/env bash
# Run the fixture table through the `--cosim dpi` backend (spec §10
# simulator-owned-time DPI-C co-sim) instead of the direct-Verilator
# drive loop. Thin wrapper over run_fixtures.sh via HARC_SIM_EXTRA_ARGS.
#
# Full parity with the direct backend (130/130) is the standing
# invariant — CI runs this as a must-be-green gate (the
# `run-cosim-fixtures` job). See
# docs/2026-07-24-dpi-cosim-exploration.md for the design notes and
# the support matrix.
set -euo pipefail
cd "$(dirname "$0")/.."

export HARC_SIM_EXTRA_ARGS="--cosim dpi"
export FIXTURE_BUILD_ROOT="${FIXTURE_BUILD_ROOT:-harc_sim_build_cosim}"
exec bash tests/run_fixtures.sh "$@"
