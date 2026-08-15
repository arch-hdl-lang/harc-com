#!/usr/bin/env bash
# Dual-backend sim<->SV equivalence net.
#
# For each design, build SystemVerilog with `arch build`, then run the HARC
# testbench under BOTH backends via `harc sim --check-backends` (ARCH native
# sim from --dut, Verilator from --sv) and assert the per-cycle traces match.
# Any silent divergence between the two backends fails CI.
#
# Requires both compilers:
#   HARC      - path to the harc binary           (default ./target/release/harc)
#   ARCH      - path to the arch binary            (default $ARCH_COM/target/release/arch)
#   ARCH_COM  - path to an arch-com checkout       (default ../arch-com)
set -uo pipefail

HARC="${HARC:-./target/release/harc}"
ARCH_COM="${ARCH_COM:-../arch-com}"
ARCH="${ARCH:-$ARCH_COM/target/release/arch}"
N="$ARCH_COM/examples/nic400"
# Directory holding this script — used to resolve harc-local fixture rows whose
# source dir is given relative to the repo (the `srcdir` column below).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP" "$N"/*.archi "$SCRIPT_DIR"/fixtures/*/*.archi' EXIT

# Each row: <name> | <top> | <dut .arch files (space-sep, rel to srcdir)> | <test.harc> [| <srcdir>]
# The optional 5th column overrides the design source dir (default $N). It may
# reference $SCRIPT_DIR for harc-local fixtures that don't live in arch-com.
read -r -d '' DESIGNS <<EOF || true
qosfn        | Nic400QosFn  | Nic400QosFn.arch                                                                  | Nic400QosFn_test.harc
regslice     | RegSliceChannel | RegSliceChannel.arch                                                           | RegSliceChannel_test.harc
fabric_multi | Nic400Fabric | BusAxi4.arch Nic400DefaultSlave.arch Nic400Fabric.arch Nic400MasterPort.arch Nic400SlavePort.arch | Nic400FabricMultiMaster_test.harc
busport_ovr  | RwTarget     | BusRw.arch RwTarget.arch                                                          | RwTarget_test.harc | $SCRIPT_DIR/fixtures/bus_port_override
EOF

fail=0
while IFS='|' read -r name top duts test srcdir; do
  name="$(echo "$name" | xargs)"; top="$(echo "$top" | xargs)"; test="$(echo "$test" | xargs)"
  srcdir="$(echo "$srcdir" | xargs)"; [ -z "$srcdir" ] && srcdir="$N"
  [ -z "$name" ] && continue
  dut_args=(); sv_inputs=()
  for f in $duts; do dut_args+=(--dut "$srcdir/$f"); sv_inputs+=("$srcdir/$f"); done
  echo "=== check-backends: $name (top $top) ==="
  "$ARCH" build "${sv_inputs[@]}" -o "$TMP/$name.sv" || { echo "  FAIL: arch build"; fail=1; continue; }
  # `--check-backends` now runs through the default (TB-IR) backend, like
  # every other path; it emits the same TB for both the ARCH-sim and
  # Verilator runs and asserts their traces match. (`--codegen v1` is still
  # selectable for A/B during the v1 deprecation soak.)
  out="$("$HARC" sim --check-backends --arch-bin "$ARCH" "${dut_args[@]}" \
        --sv "$TMP/$name.sv" --top "$top" "$srcdir/$test" 2>&1)"
  if [[ "$out" == *"traces match across backends"* ]]; then
    echo "  PASS: traces match"
  else
    echo "  FAIL: divergence or error"; echo "$out" | tail -15; fail=1
  fi
done <<< "$DESIGNS"

echo "=== check-backends net: $([ $fail -eq 0 ] && echo ALL MATCH || echo DIVERGENCE) ==="
exit $fail
