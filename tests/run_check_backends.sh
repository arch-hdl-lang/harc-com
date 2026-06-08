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
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP" "$N"/*.archi' EXIT

# Each row: <name> | <top> | <dut .arch files (space-sep, rel to $N)> | <test.harc>
read -r -d '' DESIGNS <<'EOF' || true
qosfn        | Nic400QosFn  | Nic400QosFn.arch                                                                  | Nic400QosFn_test.harc
regslice     | RegSliceChannel | RegSliceChannel.arch                                                           | RegSliceChannel_test.harc
fabric_multi | Nic400Fabric | BusAxi4.arch Nic400DefaultSlave.arch Nic400Fabric.arch Nic400MasterPort.arch Nic400SlavePort.arch | Nic400FabricMultiMaster_test.harc
EOF

fail=0
while IFS='|' read -r name top duts test; do
  name="$(echo "$name" | xargs)"; top="$(echo "$top" | xargs)"; test="$(echo "$test" | xargs)"
  [ -z "$name" ] && continue
  dut_args=(); sv_inputs=()
  for f in $duts; do dut_args+=(--dut "$N/$f"); sv_inputs+=("$N/$f"); done
  echo "=== check-backends: $name (top $top) ==="
  "$ARCH" build "${sv_inputs[@]}" -o "$TMP/$name.sv" || { echo "  FAIL: arch build"; fail=1; continue; }
  out="$("$HARC" sim --check-backends --arch-bin "$ARCH" "${dut_args[@]}" \
        --sv "$TMP/$name.sv" --top "$top" "$N/$test" 2>&1)"
  if echo "$out" | grep -q "traces match across backends"; then
    echo "  PASS: traces match"
  else
    echo "  FAIL: divergence or error"; echo "$out" | tail -15; fail=1
  fi
done <<< "$DESIGNS"

echo "=== check-backends net: $([ $fail -eq 0 ] && echo ALL MATCH || echo DIVERGENCE) ==="
exit $fail
