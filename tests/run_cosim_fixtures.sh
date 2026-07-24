#!/usr/bin/env bash
# Run the fixture table through the `--cosim dpi` backend (spec §10
# simulator-owned-time DPI-C co-sim) instead of the direct-Verilator
# drive loop. Thin wrapper over run_fixtures.sh via HARC_SIM_EXTRA_ARGS.
#
# Not all fixtures are expected to pass yet — the v0 co-sim backend
# rejects declared clocks / probes / --mt / split builds by design, and
# fixtures whose transactor methods advance cycles synchronously
# (blocking TLM/bus calls) or that touch >64-bit ports are documented
# gaps. See docs/2026-07-24-dpi-cosim-exploration.md for the support
# matrix. Use this runner to measure the current pass set, not as a
# must-be-green gate.
set -euo pipefail
cd "$(dirname "$0")/.."

export HARC_SIM_EXTRA_ARGS="--cosim dpi"
export FIXTURE_BUILD_ROOT="${FIXTURE_BUILD_ROOT:-harc_sim_build_cosim}"
exec bash tests/run_fixtures.sh "$@"
