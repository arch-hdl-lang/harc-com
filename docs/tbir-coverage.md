# TB-IR corpus coverage — blocker report

Snapshot date: **2026-06-12** (registry backfill sweep, full
`tests/run_fixtures.sh` manifest vs `harc dump-ir` / `--codegen tbir`).

**This file is a snapshot, not a source of truth.** The registry
(`tests/tbir_equiv_fixtures.txt`) is the source of truth for what is
covered. Regenerate this report by re-running the sweep: for every
manifest row of `tests/run_fixtures.sh` not in the registry, run
`harc dump-ir tests/fixtures/<test>.harc [extras...]`; rows that lower
cleanly then go through the full v1-vs-tbir equivalence pair exactly as
`tests/run_tbir_equiv.sh` does (both codegens, same seed, separate
outdirs, `harc trace-diff`).

## Sweep result summary

| Outcome | Rows |
|---|---:|
| Already registered before this sweep (manifest rows) | 6 |
| Newly registered by this sweep (lower + equivalence clean) | 10 |
| Lowers + equivalence clean, but registry schema cannot express the row | 1 |
| Blocked by `LowerError::Unsupported` | 79 |
| Lowerable but diverges (v1 ≠ tbir) | **0** |
| Manifest rows total (incl. two double-`--test` fixtures) | 96 |

(The registry held 7 rows before this sweep; the 7th, `fatal_path_test`,
is registry-only — a deliberate-failure fixture cannot appear in
`run_fixtures.sh`, which requires every row to print ALL TESTS PASSED.)

No fixture that lowers cleanly diverges between the two codegens —
every blocked row is an explicit `Unsupported` rejection, never a
silent mis-lowering.

## Registered by this sweep

`rom_lut_inline_test`, `traffic_light_test`, `single_port_ram_test`,
`int_regs_test`, `fsm_counter_test`, `clk_div_counter_test`,
`synchronizer_handshake_test`, `pkt_queue_test`, `cam_basic_test`,
`inst_vec_port_regression_test` — see the registry for the rows.

## Blockers, grouped by construct

Each group lists the exact `LowerError::Unsupported` message (minus the
common "; re-run with `--codegen v1`" suffix) and every fixture it
blocks. A construct worker picking up a group unlocks exactly the
fixtures listed; fixtures appear in `run_fixtures.sh` manifest order
within each group.

### `transactor` construct — 19 fixtures

> TB-IR lowering does not support the `transactor` construct yet

`analysis_sink_connect_test`, `axilite_regs_full_test`,
`regblock_basic_test`, `regblock_fields_test`, `regblock_access_test`,
`regblock_bitbash_test`, `regblock_addrmap_test`, `regblock_alias_test`,
`regblock_record_test`, `bind_remap_test`, `cam_dual_basic_test`,
`cam_value_basic_test`, `cpu_pipeline_test`, `linklist_doubly_test`,
`mac_table_test`, `noc_credit_test`, `buf_mgr_sm_test`,
`aes_cipher_top_test`, `buf_mgr_test`

### `transaction` construct — 17 fixtures

> TB-IR lowering does not support the `transaction` construct yet

`heartbeat_idle_test`, `wait_until_quiesce_test`,
`watchdog_quiesce_test`, `env_quiesced_phase_test`,
`relation_inlining_test`, `axilite_env_test`, `axilite_seqdrv_test`,
`axilite_connect_test`, `axilite_hooks_test`, `axilite_bound_mon_test`,
`axilite_multi_payload_test`, `axilite_constraint_test`
(`AxiLiteConstraintTest`), `transactor_parse_test`,
`transactor_active_test`, `transactor_passive_only_test`,
`transactor_agent_mode_test`, `transactor_env_mode_test`

Note: many `transactor`-group fixtures also declare `transaction`s (the
message reports whichever construct lowering hits first), so the
`transaction` slice is the natural prerequisite for the `transactor`
slice.

### `bus` construct — 15 fixtures

> TB-IR lowering does not support the `bus` construct yet

`axilite_bus_test`, `axilite_bus_extern_test`, `axilite_bus_send_test`,
`tlm_method_bus_test`, `tlm_target_thread_test`,
`tlm_target_thread_if_test`, `tlm_target_thread_runtime_loop_test`,
`tlm_target_thread_early_return_test`, `tlm_target_forwarding_test`,
`tlm_target_fork_forwarding_test`, `tlm_target_ooo_lanes_test`,
`tlm_pairing_arch_target_test`*, `tlm_pairing_arch_initiator_test`,
`dma_engine_tlm_target_test`, `dma_engine_tlm_mem_model_test`

\* `tlm_pairing_arch_target_test` does not lower (bus construct), so the
known local-only Verilator 5.048-vs-CI-5.034 SVA verdict issue on this
fixture never reached the equivalence stage; from CI's perspective it is
simply bus-blocked like its siblings.

### `wait N cycles on <clock>` — 5 fixtures

> TB-IR lowering does not support `wait N cycles on <clock>` yet

`synchronizer_gray_test`, `synchronizer_reset_test`,
`synchronizer_wide_test`, `synchronizer_pulse_test`, `multi_clock_test`

### `struct` construct — 4 fixtures

> TB-IR lowering does not support the `struct` construct yet

`post_eval_provider_test`, `scoreboard_typed_queue_test`,
`tlm_pairing_arch_burst_target_test`,
`tlm_pairing_arch_burst_initiator_test`

(The two `tlm_pairing_arch_burst_*` fixtures hit `struct` before `bus`;
both constructs are needed.)

### Probe declarations on `let dut` — 3 fixtures

> TB-IR lowering does not support probe declarations on `let dut` yet

`probe_basic_test`, `probe_force_test`, `testbench_probe_dut_test`

### Method call `.reset(...)` — 2 fixtures (1 file)

> TB-IR lowering does not support method call `.reset(...)` yet

`testbench_basic_test` (both `--test` structs: `TestbenchSmoke`,
`TestbenchEnableToggle`)

### Testbench field with non-named type — 2 fixtures (1 file)

> TB-IR lowering does not support testbench field `expected` with a
> non-named type yet

`testbench_lifecycle_test` (both `--test` structs:
`LifecycleBumpThree`, `LifecycleBumpFive`)

### Wide (>64-bit) integer literals — 2 fixtures

> TB-IR lowering does not support integer literal yet (`0x...` is not a
> plain literal)

`wide_reg_test`
(`0x0123456789abcdef_fedcba9876543210_aabbccddeeff0011_2233445566778899`),
`sha256_test` (512-bit message-block literal)

### Singleton blockers — 1 fixture each (10 total)

| Fixture | Exact `Unsupported` message |
|---|---|
| `pipe_reg_test` | TB-IR lowering does not support the `property` construct yet |
| `width_methods_test` | TB-IR lowering does not support transactor/method call `.trunc(...)` yet |
| `keep_constraints_test` | TB-IR lowering does not support the `enum` construct yet |
| `extern_fn_ref_test` | TB-IR lowering does not support the `extern fn` construct yet |
| `async_fifo_test` | TB-IR lowering does not support time literals in expression position yet |
| `dma_engine_test` | TB-IR lowering does not support the `scoreboard` construct yet |
| `linklist_basic_test` | TB-IR lowering does not support ternary expressions yet |
| `mshr_cocotb_test` | TB-IR lowering does not support the `const` construct yet |
| `packed_vec_lane_test` | TB-IR lowering does not support assignment to a non-port, non-local target yet |
| `if_wait_for_in_then_test` | TB-IR lowering does not support test-scope `let done_pulses` yet (only `let dut : <Type>` is lowered at test scope) |

## Registry-schema gap (not a lowering gap)

`synchronizer_basic_test` lowers cleanly and passes the full
v1-vs-tbir equivalence pair (verdicts + trace-diff clean), but cannot
be registered: it needs a second HARC file
(`async_fifo_domains.harc`), and registry schema v2
(`test_name | top | sv_files | arch_dut | expect`) has no extra-files
column — `run_tbir_equiv.sh` builds `HARC_FILES` from `test_name`
alone. The same schema gap will eventually bite `--test`-selecting
fixtures (`testbench_basic_test`, `testbench_lifecycle_test`,
`axilite_constraint_test`) and the `--ref-src` fixture
(`extern_fn_ref_test`) once their constructs land. A schema v3 with
optional `extra_harc | ref_src | test_struct` columns should ship with
whichever construct slice first unlocks one of these fixtures.

## Suggested sequencing for construct workers

1. **`transaction` → `transactor`** (17 + 19 fixtures): the two
   biggest groups, and `transactor` depends on `transaction`.
2. **`bus`** (15 fixtures): unlocks the entire TLM family.
3. **`wait N cycles on <clock>`** (5) + the registry schema v3 row
   format: together they unlock the synchronizer/multi-clock family
   (incl. the schema-blocked `synchronizer_basic_test`).
4. **`struct`** (4, two of which also need `bus`).
5. Singletons opportunistically — `.reset(...)`, non-named testbench
   field types, wide literals, `property`, `enum`, `const`,
   `extern fn`, `scoreboard`, ternary, probes, time-literal exprs,
   non-port assignment targets, test-scope `let`.
