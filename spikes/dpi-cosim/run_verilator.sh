#!/usr/bin/env bash
# Build + run the DPI-C co-sim spike under Verilator (simulator-owned
# time: --timing, SV clock generator, HARC called through DPI imports).
#
#   ./run_verilator.sh              # functional test
#   HARC_COSIM_SOAK=1000000 ./run_verilator.sh   # + throughput soak
set -euo pipefail
cd "$(dirname "$0")"

REPO_ROOT="$(cd ../.. && pwd)"
OUT="${HARC_COSIM_OUT:-$REPO_ROOT/target/spike-dpi-cosim/verilator}"
mkdir -p "$OUT"

# --binary = --main --exe --build --timing: Verilator supplies main(),
# the harness's `forever #5 clk = ~clk` owns time, and the HARC runtime
# is a passive library entered only through the DPI imports. Contrast
# with the direct backend (src/main.rs run_verilator), which passes
# --no-timing and puts the clock loop in generated C++.
verilator --binary --timing \
  -Wno-fatal -Wno-WIDTH \
  --top-module HarcCosimTop \
  --Mdir "$OUT/obj_dir" \
  -CFLAGS "-I$REPO_ROOT/runtime -I$PWD" \
  -MAKEFLAGS "CFG_CXXFLAGS_STD=-std=gnu++20 CXX=${HARC_CXX:-c++}" \
  harness.sv "$REPO_ROOT/tests/dut/sync_fifo.sv" \
  "$PWD/harc_cosim_core.cpp" "$PWD/dpi_adapter.cpp"

"$OUT/obj_dir/VHarcCosimTop" | tee "$OUT/sim.log"
grep -q "ALL TESTS PASSED" "$OUT/sim.log"
echo "verilator DPI co-sim: OK"
