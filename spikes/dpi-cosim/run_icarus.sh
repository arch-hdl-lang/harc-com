#!/usr/bin/env bash
# Build + run the co-sim spike under Icarus Verilog. Icarus has no DPI-C,
# so the same HARC C ABI is reached through a VPI bridge (vpi_adapter.c):
# the TB core object is identical to the Verilator build; only the
# adapter differs.
#
#   ./run_icarus.sh              # functional test
#   HARC_COSIM_SOAK=100000 ./run_icarus.sh   # + throughput soak
set -euo pipefail
cd "$(dirname "$0")"

REPO_ROOT="$(cd ../.. && pwd)"
OUT="${HARC_COSIM_OUT:-$REPO_ROOT/target/spike-dpi-cosim/icarus}"
mkdir -p "$OUT"

# Icarus ships vpi_user.h; iverilog-vpi reports the install prefix.
IVL_CFLAGS="$(iverilog-vpi --cflags 2>/dev/null || echo "-I/usr/include/iverilog")"

# The .vpi module = VPI bridge (C) + unchanged HARC TB core (C++20
# coroutines, same harc_thread_rt.h scheduler as the direct backend).
gcc -O2 -fPIC $IVL_CFLAGS -I. -c vpi_adapter.c -o "$OUT/vpi_adapter.o"
"${HARC_CXX:-c++}" -std=gnu++20 -O2 -fPIC -I"$REPO_ROOT/runtime" -I. \
  -c harc_cosim_core.cpp -o "$OUT/harc_cosim_core.o"
"${HARC_CXX:-c++}" -shared -o "$OUT/harc_cosim.vpi" \
  "$OUT/vpi_adapter.o" "$OUT/harc_cosim_core.o"

# -DHARC_COSIM_VPI selects the system-task flavor of the harness
# (Icarus rejects `import "DPI-C"` — see the exploration doc).
iverilog -g2012 -DHARC_COSIM_VPI \
  -o "$OUT/sim.vvp" \
  harness.sv "$REPO_ROOT/tests/dut/sync_fifo.sv"

vvp -M "$OUT" -m harc_cosim "$OUT/sim.vvp" | tee "$OUT/sim.log"
grep -q "ALL TESTS PASSED" "$OUT/sim.log"
echo "icarus VPI co-sim: OK"
