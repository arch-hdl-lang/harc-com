#!/bin/bash
# Refresh vendored DUTs from a sibling arch-com clone.
# Run from harc-com repo root.
set -euo pipefail

ARCH_REPO="${ARCH_REPO:-../arch-com}"
DST_DIR="$(dirname "$0")"

if [ ! -d "$ARCH_REPO" ]; then
    echo "error: ARCH_REPO=$ARCH_REPO not a directory; clone arch-hdl-lang/arch-com next to harc-com or set ARCH_REPO" >&2
    exit 1
fi

# (source-file, vendored-name) pairs.
copy() {
    local src="$1"
    local dst="$2"
    if [ ! -f "$ARCH_REPO/$src" ]; then
        echo "warn: $ARCH_REPO/$src not found; skipping" >&2
        return 0
    fi
    cp "$ARCH_REPO/$src" "$DST_DIR/$dst"
    echo "  $src → $DST_DIR/$dst"
}

echo "Refreshing DUTs from $ARCH_REPO/..."

copy examples/rom_lut.sv               rom_lut.sv
copy examples/bus_arbiter.sv           bus_arbiter.sv
copy examples/traffic_light.sv         traffic_light.sv
copy examples/sync_fifo.sv             sync_fifo.sv
copy examples/pipe_reg_test.sv         pipe_reg_test.sv
copy examples/single_port_ram.sv       single_port_ram.sv
copy examples/synchronizer_basic.sv    synchronizer_basic.sv
copy examples/synchronizer_pulse.sv    synchronizer_pulse.sv
copy examples/synchronizer_gray.sv     synchronizer_gray.sv
copy examples/synchronizer_handshake.sv synchronizer_handshake.sv
copy examples/synchronizer_reset.sv    synchronizer_reset.sv
copy examples/synchronizer_wide.sv     synchronizer_wide.sv
copy examples/async_fifo.sv            async_fifo.sv
copy examples/multi_clock.sv           multi_clock.sv
copy examples/clk_div_counter.sv       clk_div_counter.sv
copy examples/clk_divider.sv           clk_divider.sv
copy examples/top_counter.sv           top_counter.sv
copy examples/fsm_counter.sv           fsm_counter.sv
copy examples/int_regs.sv              int_regs.sv
copy examples/pkt_queue.sv             pkt_queue.sv
copy examples/linklist_basic.sv        linklist_basic.sv
copy examples/linklist_doubly.sv       linklist_doubly.sv
copy examples/dma_engine.sv            dma_engine.sv
copy examples/cpu_pipeline.sv          cpu_pipeline.sv
copy tests/cam_basic.sv                cam_basic.sv
copy tests/cam_dual_basic.sv           cam_dual_basic.sv
copy tests/cam_value_basic.sv          cam_value_basic.sv
copy tests/cvdp/cache_mshr.sv          cache_mshr.sv
copy tests/axi_dma/AxiLiteRegs.sv      AxiLiteRegs.sv
copy tests/mac_table.sv                mac_table.sv
copy tests/noc_credit/noc_credit.sv    noc_credit.sv
copy tests/if_wait_for_in_then.sv      if_wait_for_in_then.sv
copy tests/inst_vec_port_regression.sv inst_vec_port_regression.sv
copy tests/buf_mgr_sm/buf_mgr_sm.sv         buf_mgr_sm.sv
copy tests/buf_mgr_sm/data_mem_sm.sv        data_mem_sm.sv
copy tests/buf_mgr_sm/free_list_mem_sm.sv   free_list_mem_sm.sv
copy tests/buf_mgr_sm/next_ptr_mem_sm.sv    next_ptr_mem_sm.sv
copy tests/aes/aes_cipher_top.sv       aes_cipher_top.sv
copy tests/aes/aes_key_expand_128.sv   aes_key_expand_128.sv
copy tests/aes/xtime.sv                xtime.sv
# NOTE: the vendored buf_mgr.sv has a small local patch for
# Verilator-5.020 (Ubuntu apt) compatibility — three reset for-
# loops use `=` instead of `<=`. Re-running this script overwrites
# the patch; reapply by re-running the patching block below or by
# diffing against an arch-com PR that lands the same fix upstream.
# See the comment at line ~217 in buf_mgr.sv.
copy tests/buf_mgr/buf_mgr.sv          buf_mgr.sv
sed -i.bak \
    -e 's/count_arr\[__ri0\] <= 0;/count_arr[__ri0] = 0;/' \
    -e 's/head_arr\[__ri0\] <= 0;/head_arr[__ri0] = 0;/' \
    -e 's/tail_arr\[__ri0\] <= 0;/tail_arr[__ri0] = 0;/' \
    "$DST_DIR/buf_mgr.sv" && rm -f "$DST_DIR/buf_mgr.sv.bak"
copy tests/buf_mgr/data_mem.sv         data_mem.sv
copy tests/buf_mgr/next_ptr_mem.sv     next_ptr_mem.sv
copy tests/buf_mgr/free_list_bank.sv   free_list_bank.sv
copy tests/buf_mgr/setup_counter.sv    setup_counter.sv

echo "Done."
