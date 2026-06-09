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

HARC="${HARC:-./target/release/harc}"
DUT_DIR="tests/dut"
FIX_DIR="tests/fixtures"

if [ ! -x "$HARC" ]; then
    echo "Building harc..."
    cargo build --release --bin harc
fi

# Each row: <test_name> <top_module> <sv_files> <extra_harc_files> <ref_src> <test_struct>
# Fields are pipe-separated. SV files are relative to DUT_DIR; HARC files
# are relative to FIX_DIR. Multiple files within a field are space-
# separated. The 6th field (test_struct) is passed via `--test` when the
# loaded harc files declare more than one test struct.
read -r -d '' FIXTURES <<'EOF' || true
rom_lut_test            | RomLut         | rom_lut.sv             |
rom_lut_inline_test     | RomLut         | rom_lut.sv             |
bus_arbiter_test        | BusArbiter     | bus_arbiter.sv         |
traffic_light_test      | TrafficLight   | traffic_light.sv       |
sync_fifo_test          | TxQueue        | sync_fifo.sv           |
pipe_reg_test           | PipeRegTest    | pipe_reg_test.sv       |
single_port_ram_test    | SimpleMem      | single_port_ram.sv     |
int_regs_test           | IntRegs        | int_regs.sv            |
top_counter_test        | Top            | top_counter.sv         |
heartbeat_idle_test     | Top            | top_counter.sv         |
analysis_sink_connect_test | Top         | top_counter.sv         |
wait_until_quiesce_test | Top            | top_counter.sv         |
watchdog_quiesce_test   | Top            | top_counter.sv         |
env_quiesced_phase_test | Top            | top_counter.sv         |
testbench_basic_test    | Top            | top_counter.sv         | | | TestbenchSmoke
testbench_basic_test    | Top            | top_counter.sv         | | | TestbenchEnableToggle
testbench_lifecycle_test | Top           | top_counter.sv         | | | LifecycleBumpThree
testbench_lifecycle_test | Top           | top_counter.sv         | | | LifecycleBumpFive
width_methods_test      | Top            | top_counter.sv         |
keep_constraints_test   | Top            | top_counter.sv         |
extern_fn_ref_test      | Top            | top_counter.sv         |     | extern_fn_ref.cpp
relation_inlining_test  | Top            | top_counter.sv         |
fsm_counter_test        | FsmCounter     | fsm_counter.sv         |
clk_div_counter_test    | ClkDivCounter  | clk_div_counter.sv clk_divider.sv |
synchronizer_basic_test | FlagSync       | synchronizer_basic.sv  | async_fifo_domains.harc
synchronizer_gray_test  | PtrSync        | synchronizer_gray.sv   |
synchronizer_handshake_test | BusSync    | synchronizer_handshake.sv |
synchronizer_reset_test | RstSync        | synchronizer_reset.sv  |
synchronizer_wide_test  | DataSync       | synchronizer_wide.sv   |
synchronizer_pulse_test | EventSync      | synchronizer_pulse.sv  | synchronizer_pulse_domains.harc
multi_clock_test        | MultiClockSync | multi_clock.sv         | async_fifo_domains.harc
async_fifo_test         | AsyncBridge    | async_fifo.sv          | async_fifo_domains.harc
axilite_env_test        | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_seqdrv_test     | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_connect_test    | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_hooks_test      | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_bus_test        | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_bus_extern_test | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_bus_send_test   | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_bound_mon_test  | AxiLiteRegs    | AxiLiteRegs.sv         |
axilite_multi_payload_test | AxiLiteRegs | AxiLiteRegs.sv         |
axilite_regs_full_test  | AxiLiteRegs    | AxiLiteRegs.sv         |
regblock_basic_test     | AxiLiteRegs    | AxiLiteRegs.sv         |
regblock_fields_test    | AxiLiteRegs    | AxiLiteRegs.sv         |
regblock_access_test    | AxiLiteRegs    | AxiLiteRegs.sv         |
regblock_bitbash_test   | AxiLiteRegs    | AxiLiteRegs.sv         |
regblock_addrmap_test   | AxiLiteRegs    | AxiLiteRegs.sv         |
regblock_alias_test     | AxiLiteRegs    | AxiLiteRegs.sv         |
regblock_record_test    | AxiLiteRegs    | AxiLiteRegs.sv         |
bind_remap_test         | axil_amba      | axil_amba.sv           |
axilite_constraint_test | AxiLiteRegs    | AxiLiteRegs.sv         | axilite_regs_test.harc | | AxiLiteConstraintTest
transactor_parse_test   | AxiLiteRegs    | AxiLiteRegs.sv         |
transactor_active_test  | AxiLiteRegs    | AxiLiteRegs.sv         |
transactor_passive_only_test | AxiLiteRegs | AxiLiteRegs.sv        |
transactor_agent_mode_test | AxiLiteRegs   | AxiLiteRegs.sv         |
transactor_env_mode_test | AxiLiteRegs     | AxiLiteRegs.sv         |
post_eval_provider_test | PostEvalProvider | post_eval_provider.sv |
scoreboard_typed_queue_test | ScoreboardTypedQueue | scoreboard_typed_queue.sv |
tlm_method_bus_test   | TlmMemory      | TlmMemory.sv           |
tlm_target_thread_test | TlmReadInitiator | TlmReadInitiator.sv  |
tlm_target_thread_if_test | TlmReadInitiatorPair | TlmReadInitiatorPair.sv |
tlm_target_thread_runtime_loop_test | TlmReadInitiatorRuntimeLen | TlmReadInitiatorRuntimeLen.sv |
tlm_target_thread_early_return_test | TlmReadInitiatorRuntimeLen | TlmReadInitiatorRuntimeLen.sv |
tlm_target_forwarding_test | TlmForwardingTop | TlmForwardingTop.sv TlmReadInitiatorPair.sv TlmMemory.sv |
tlm_target_fork_forwarding_test | TlmForkForwardingTop | TlmForkForwardingTop.sv TlmReadInitiatorPair.sv TlmMemory.sv |
tlm_target_ooo_lanes_test | TlmOooReadInitiatorPair | TlmOooReadInitiatorPair.sv |
tlm_pairing_arch_target_test | TlmPairingArchTarget | TlmPairingArchTarget.sv |
tlm_pairing_arch_initiator_test | TlmPairingArchInitiator | TlmPairingArchInitiator.sv |
tlm_pairing_arch_burst_target_test | TlmPairingArchBurstTarget | TlmPairingArchBurstTarget.sv |
tlm_pairing_arch_burst_initiator_test | TlmPairingArchBurstInitiator | TlmPairingArchBurstInitiator.sv |
dma_engine_test         | DmaEngine      | dma_engine.sv          |
dma_engine_tlm_target_test | DmaEngineTlmMem | dma_engine_tlm_mem.sv dma_engine.sv |
dma_engine_tlm_mem_model_test | DmaEngineTlmMem | dma_engine_tlm_mem.sv dma_engine.sv |
pkt_queue_test          | PacketQueue    | pkt_queue.sv           |
linklist_basic_test     | TaskQueue      | linklist_basic.sv      |
cam_basic_test          | Mshr_Addr_Cam  | cam_basic.sv           |
cam_dual_basic_test     | Mshr_Addr_Cam_Dual | cam_dual_basic.sv  |
cam_value_basic_test    | Tag_Value_Cam  | cam_value_basic.sv     |
mshr_cocotb_test        | cache_mshr     | cache_mshr.sv          |
cpu_pipeline_test       | CpuPipe        | cpu_pipeline.sv        |
probe_basic_test        | CpuPipe        | cpu_pipeline.sv        |
probe_force_test        | CpuPipe        | cpu_pipeline.sv        |
testbench_probe_dut_test | CpuPipe       | cpu_pipeline.sv        |
linklist_doubly_test    | SchedList      | linklist_doubly.sv     |
mac_table_test          | mac_table      | mac_table.sv           |
noc_credit_test         | NocCreditTop   | noc_credit.sv          |
inst_vec_port_regression_test | Top      | inst_vec_port_regression.sv |
packed_vec_lane_test    | PackedVecLane  | packed_vec_lane.sv     |
if_wait_for_in_then_test | M             | if_wait_for_in_then.sv |
buf_mgr_sm_test         | BufMgrSm       | buf_mgr_sm.sv data_mem_sm.sv free_list_mem_sm.sv next_ptr_mem_sm.sv |
aes_cipher_top_test     | AesCipherTop   | aes_cipher_top.sv aes_key_expand_128.sv xtime.sv |
wide_reg_test           | WideReg        | wide_reg.sv            |
buf_mgr_test            | BufMgr         | buf_mgr.sv data_mem.sv next_ptr_mem.sv free_list_bank.sv setup_counter.sv |
sha256_test             | Sha256         | sha256.sv              |
lz4dec_test             | Lz4Dec         | lz4dec.sv              |
EOF

PASS=0
FAIL=0
FAILED_NAMES=()

run_one() {
    local row="$1"
    # Optional 5th field: space-separated C/C++ reference-model source
    # files relative to DUT_DIR, passed via `--ref-src`. Used by
    # `extern function` tests (spec §9).
    IFS='|' read -r test top sv extras ref_src test_struct <<<"$row"
    test="$(echo "$test" | xargs)"
    top="$(echo "$top" | xargs)"
    sv="$(echo "$sv" | xargs)"
    extras="$(echo "$extras" | xargs || true)"
    ref_src="$(echo "$ref_src" | xargs || true)"
    test_struct="$(echo "$test_struct" | xargs || true)"
    [ -z "$test" ] && return 0

    local sv_args=()
    for f in $sv; do sv_args+=("--sv" "$DUT_DIR/$f"); done

    local ref_args=()
    for f in $ref_src; do ref_args+=("--ref-src" "$DUT_DIR/$f"); done

    local harc_files=("$FIX_DIR/$test.harc")
    for f in $extras; do harc_files+=("$FIX_DIR/$f"); done

    local test_args=()
    [ -n "$test_struct" ] && test_args=("--test" "$test_struct")

    rm -rf harc_sim_build
    local out
    # `${ref_args[@]:-}` tolerates an empty array under `set -u` (most
    # fixtures don't pass any --ref-src).
    out="$("$HARC" sim "${sv_args[@]}" ${ref_args[@]+"${ref_args[@]}"} "${harc_files[@]}" --top "$top" ${test_args[@]+"${test_args[@]}"} 2>&1)" || true

    if echo "$out" | grep -q "ALL TESTS PASSED"; then
        echo "  PASS  $test"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  $test"
        echo "$out" | tail -20 | sed 's/^/      /'
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$test")
    fi
}

echo "Running ${PWD}/$DUT_DIR fixtures..."
while IFS= read -r row; do
    run_one "$row"
done <<<"$FIXTURES"

echo
echo "Result: $PASS passed, $FAIL failed"
if [ $FAIL -gt 0 ]; then
    echo "Failed: ${FAILED_NAMES[*]}"
    exit 1
fi
