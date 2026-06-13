# TB-IR corpus coverage — blocker report

Snapshot date: **2026-06-12** (registry backfill sweep, full
`tests/run_fixtures.sh` manifest vs `harc dump-ir` / `--codegen tbir`).
Amended the same day by the `transaction` slice — see the resolved
`transaction` group below for its residual blocker map — and by the
`transactor` slice (resolved group + residual map below; 8 fixtures
registered, plus one newly-lowerable-but-divergent fixture recorded
under the singleton table).
singleton-blocker batch (see the resolved singleton table below: 11
new registry rows across 9 fixtures; `mshr_cocotb_test` moved to the
`transactor` group and `keep_constraints_test` to the
`randomize` seam).

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

*(Amended by the transactor slice: the divergence count is no longer
zero — `linklist_basic_test`, unblocked by ternary support, lowers
cleanly but trace-diverges on the final `sim_end` event's clock
attribution. See the singleton table and docs/tbir-mvp.md
divergence 11. It is the only known lowerable-but-divergent fixture
and stays out of the registry.)*

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

### `transactor` construct — RESOLVED 2026-06-12 (transactor slice)

`transactor` declarations now lower in their unbound DUT-poking BFM
subset (one module-typed field, `active` instances, hookable methods
with scalar ≤64-bit params, synchronous waits; see docs/tbir-mvp.md).
Of the 24 fixtures gated on `transactor` at slice start (the 19 below
plus the 5 the `transaction` slice moved into this group), **8 became
fully lowerable**, passed the v1-vs-tbir equivalence pair, and are
registered:

`cam_dual_basic_test`, `cam_value_basic_test`, `cpu_pipeline_test`,
`linklist_doubly_test`, `mac_table_test`, `noc_credit_test`,
`buf_mgr_sm_test`, `buf_mgr_test`

The slice also brought ternary expressions into the subset (needed by
`linklist_doubly_test`'s method bodies), which moved the singleton
`linklist_basic_test` out of its ternary blocker — see the singleton
table for its new (divergence, not lowering) status.

Residual first-blocker map for the other 16 (re-run of
`harc dump-ir`, 2026-06-12):

| Moved to group | Fixtures |
|---|---|
| `bus` (3) | `axilite_regs_full_test`, `bind_remap_test`, `transactor_parse_test` |
| `regblock` (7) | `regblock_basic_test`, `regblock_fields_test`, `regblock_access_test`, `regblock_bitbash_test`, `regblock_addrmap_test`, `regblock_alias_test`, `regblock_record_test` |
| `scoreboard` (2) | `analysis_sink_connect_test`, `axilite_env_test` |
| `tseq` (2) | `axilite_seqdrv_test`, `transactor_active_test` |
| transactor state fields (1) | `axilite_hooks_test` — "transactor `HookXactor` state field `last_read`" (scalar state on the transactor; needs instance-state materialization) |
| >64-bit method params (1) | `aes_cipher_top_test` — "transactor method `AesXactor.load_block` parameter `key` wider than 64 bits (uint<128>)" (the tbir value model is u64) |

As with the `transaction` slice, most of the moved fixtures stack
several constructs (bus + scoreboard + sequencer + events); the
counts will keep shifting as those slices land. The 2 transactor-
specific residuals (`axilite_hooks_test`, `aes_cipher_top_test`) stay
with this construct's owner.

*+ `mshr_cocotb_test` (moved here 2026-06-12 by the singleton batch:
its former `const` blocker is resolved, the next blocker is its
`MshrXactor` transactor).*

### `transaction` construct — RESOLVED 2026-06-12 (transaction slice)

`transaction` declarations and non-randomize record usage now lower
(records table + `RecordInit`/`RecordFieldWrite`/`Expr::RecordField`;
see docs/tbir-mvp.md). The first-blocker caveat applied in full:
**zero** of the 17 fixtures in this group became fully lowerable —
every one revealed a deeper blocker behind its `transaction`
declaration. Since the group produced no registrable corpus row, the
slice added a new fixture, `transaction_basic_test` (declaration +
defaults + let-site re-init in a loop + field reads/writes + inert
keep/attr), registered in the equivalence registry
(`top_counter.sv` / `top_counter.arch`, pass).

Residual first-blocker map (re-run of `harc dump-ir`, 2026-06-12):

| Moved to group | Fixtures |
|---|---|
| `agent` (4) | `heartbeat_idle_test`, `wait_until_quiesce_test`, `watchdog_quiesce_test`, `env_quiesced_phase_test` |
| `transactor` (5) | `axilite_env_test`, `axilite_seqdrv_test`, `axilite_hooks_test`, `transactor_parse_test`, `transactor_active_test` |
| `scoreboard` (5) | `axilite_bound_mon_test`, `axilite_multi_payload_test`, `transactor_passive_only_test`, `transactor_agent_mode_test`, `transactor_env_mode_test` |
| `sequencer` (1) | `axilite_connect_test` |
| `relation` (1) | `relation_inlining_test` |
| `randomize` (statement-level, 1) | `axilite_constraint_test` (`AxiLiteConstraintTest`) — now reaches the body and stops at `randomize(p) with`, which points at the constraint-IR seam |

The blocker message reports whichever construct lowering hits first
in file order, so the `transactor`-group counts above will keep
shifting as `agent`/`scoreboard`/`sequencer` slices land — most of
these fixtures need several of those constructs at once (they are
full env/agent stacks).

### `bus` construct — 15 fixtures — **PARTIALLY RESOLVED 2026-06-12**

> ~~TB-IR lowering does not support the `bus` construct yet~~

The bus slice landed (declarations, `= bind dut` bindings, signal
access, `send`/`recv` auto-handshakes CFG-inlined, blocking
`tlm_method` calls as `CallTarget::TransactorMethod` call edges —
see docs/tbir-mvp.md divergence 9). **Registered**: 3 of the 15
unlocked fully and pass the v1-vs-tbir equivalence pair —
`axilite_bus_test`, `axilite_bus_extern_test`,
`axilite_bus_send_test` — plus a new corpus fixture
`tlm_method_blocking_bus_test` (blocking-only twin of
`tlm_method_bus_test`) authored to prove the TransactorMethod call
edge end-to-end, since every pre-existing TLM fixture also carries a
deeper blocker.

Residual map for the other 12 (each now stops at its REAL next
blocker; exact `Unsupported` message in parentheses):

| Next blocker | Fixtures |
|---|---|
| `transactor` construct (target-side TLM method threads → `FunctionKind::TransactorBody`) | `tlm_target_thread_test`, `tlm_target_thread_if_test`, `tlm_target_thread_runtime_loop_test`, `tlm_target_thread_early_return_test`, `tlm_target_forwarding_test`, `tlm_target_fork_forwarding_test`, `tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test`, `dma_engine_tlm_target_test`, `dma_engine_tlm_mem_model_test` ("the `transactor` construct") |
| `fork`/`join_all` TLM issue (needs `Terminator::Fork` + `ForkArmKind::BusMethodCall`) | `tlm_method_bus_test`, `tlm_pairing_arch_target_test`* ("`fork` bus-method calls") |

Both groups' bus prerequisites are in place: the transactor group
will additionally need `bind ... with { ... }` remaps
(`dma_engine_tlm_*`) and per-instance state fields; the fork group's
blocking-call halves already lower.

\* `tlm_pairing_arch_target_test` still does not reach the equivalence
stage (now fork-blocked rather than bus-blocked), so the known
local-only Verilator 5.048-vs-CI-5.034 SVA verdict issue on this
fixture remains moot from CI's perspective.

### `wait N cycles on <clock>` — 5 fixtures — **RESOLVED 2026-06-12**

> TB-IR lowering does not support `wait N cycles on <clock>` yet

`synchronizer_gray_test`, `synchronizer_reset_test`,
`synchronizer_wide_test`, `synchronizer_pulse_test`, `multi_clock_test`

**Resolved**: clock-qualified waits now lower (the `WaitCycles`
terminator carries an `Option<WaitClock>` resolved against the test's
declared clocks; tbir emission mirrors v1's inline `eval_clocks_until`
loop). All five fixtures lower cleanly, pass the full v1-vs-tbir
equivalence pair, and are registered (no deeper blockers surfaced —
the group was exactly as advertised).

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

### Method call `.reset(...)` — 2 fixtures (1 file) — **RESOLVED 2026-06-12**

> ~~TB-IR lowering does not support method call `.reset(...)` yet~~

`testbench_basic_test` (both `--test` structs: `TestbenchSmoke`,
`TestbenchEnableToggle`) — testbench helper methods
(`function`/`hookable` declared in the bound testbench) now CFG-inline
at `_tb.<m>(...)` call sites like impure helpers. Both structs pass
the equivalence pair and are registered (via the schema-v3
`test_struct` column).

### Testbench field with non-named type — 2 fixtures (1 file) — **RESOLVED 2026-06-12**

> ~~TB-IR lowering does not support testbench field `expected` with a
> non-named type yet~~

`testbench_lifecycle_test` (both `--test` structs:
`LifecycleBumpThree`, `LifecycleBumpFive`) — scalar
(uint/sint/bits/bool, ≤ 64-bit) testbench fields now lower as
run/check-shared `_tb`-struct members
(`TestbenchSchema::scalar_fields`, `Expr::TbField` /
`Stmt::TbFieldWrite`). Both structs registered.

### Wide (>64-bit) integer literals — 2 fixtures — **RESOLVED 2026-06-12**

> ~~TB-IR lowering does not support integer literal yet (`0x...` is not
> a plain literal)~~

`wide_reg_test` (256-bit), `sha256_test` (512-bit message block) —
hex literals wider than 64 bits lower to `Expr::WideLiteral` word
lists; emission mirrors v1 (`_harc_u128` composite ≤ 128 bits,
`HarcWide<N>` above, `harc_assign_words_checked` /
`harc_eq_words` at the assign / `==` / `!=` sites). Both registered.

### Singleton blockers — 1 fixture each (10 total)

| Fixture | Exact `Unsupported` message |
|---|---|
| `pipe_reg_test` | TB-IR lowering does not support the `property` construct yet |
| `width_methods_test` | TB-IR lowering does not support transactor/method call `.trunc(...)` yet |
| `keep_constraints_test` | TB-IR lowering does not support the `enum` construct yet |
| `extern_fn_ref_test` | TB-IR lowering does not support the `extern fn` construct yet |
| `async_fifo_test` | TB-IR lowering does not support time literals in expression position yet |
| `dma_engine_test` | TB-IR lowering does not support the `scoreboard` construct yet |
| `linklist_basic_test` | ~~ternary expressions~~ **lowers since the transactor slice (ternary landed), but diverges**: v1 stamps `sim_end` with `clock:""` because the fixture's run body has no top-level wait — under v1's sync-helper model the whole test runs inside `sched.bootstrap()` and the pre-loop settle dump is the last timing update. tbir's CFG-inlined helpers suspend for real and stamp `"clk"`. Verdict + all other events identical; unregistered pending trace-normalization reconciliation (docs/tbir-mvp.md divergence 11) |
| `mshr_cocotb_test` | TB-IR lowering does not support the `const` construct yet |
| `packed_vec_lane_test` | TB-IR lowering does not support assignment to a non-port, non-local target yet |
| `if_wait_for_in_then_test` | TB-IR lowering does not support test-scope `let done_pulses` yet (only `let dut : <Type>` is lowered at test scope) |
Status after the singleton-blocker batch (2026-06-12):

| Fixture | Was blocked on | Outcome |
|---|---|---|
| `pipe_reg_test` | `property` construct | **Skipped** — SVA-shaped, belongs to its own slice |
| `width_methods_test` | `.trunc(...)` method call | **RESOLVED + registered** — `.trunc/.zext/.sext/.resize` lower to `Expr::WidthCast` (≤ 64-bit, v1's mask/cast/shift-fill shapes + direction checks); scalar `as uint<W>` casts lower as width relabels |
| `keep_constraints_test` | `enum` construct | **enum RESOLVED; deeper blocker** — enums lower as named integer constants (variant index, first definition wins) and enum-typed transaction fields as scalar indices; the fixture now stops at statement-level `randomize` (constraint-IR seam) |
| `extern_fn_ref_test` | `extern fn` construct | **Skipped** — needs `--ref-src` end-to-end; deferred with the registry plumbing already in place |
| `async_fifo_test` | time literals in expression position | **RESOLVED + registered** — `wait 80ns` lowers to `Terminator::WaitTimePs` (ps resolved at lowering; v1's inline `eval_clocks_until(now_ps + N)`); `log(debug, ...)` severity also landed here |
| `dma_engine_test` | `scoreboard` construct | **Skipped** — wave-4 construct |
| `linklist_basic_test` | ternary expressions | **RESOLVED + registered** — `Expr::Ternary` emits the C++ `?:`; also forced the `WaitCyclesSync` terminator (waits inlined from helper bodies take v1's synchronous `tick()` path, fixing a `sim_end` clock-attribution trace delta) |
| `mshr_cocotb_test` | `const` construct | **const RESOLVED; deeper blocker** — file-scope `const` (integer-literal initializers) substitutes at use sites; the fixture now stops at the `transactor` construct (moved to that group) |
| `packed_vec_lane_test` | assignment to a non-port, non-local target | **RESOLVED + registered** — constant-lane `dut.<port>[i]` reads/writes lower via `PortRef::lane`; emission splits packed lanes (`harc_vec_lane_*<W>` via the `--sv` lane table) from unpacked-array subscripts, like v1 |
| `if_wait_for_in_then_test` | test-scope `let done_pulses` | **RESOLVED + registered** — plain test-scope lets hoist to the head of the run function (v1 hoists to `main` scope before the coroutine); a check-phase reference is a precise rejection (run/check are separate IR functions, so v1's shared-capture scoping is not representable) |

## Registry-schema gap (not a lowering gap) — **RESOLVED 2026-06-12**

`synchronizer_basic_test` lowers cleanly and passes the full
v1-vs-tbir equivalence pair (verdicts + trace-diff clean), but could
not be registered: it needs a second HARC file
(`async_fifo_domains.harc`), and registry schema v2
(`test_name | top | sv_files | arch_dut | expect`) had no extra-files
column — `run_tbir_equiv.sh` built `HARC_FILES` from `test_name`
alone.

**Resolved**: schema v3 shipped with the wait-on-clock slice —
optional trailing `extra_harc | ref_src | test_struct` columns (`-` =
none; 5-column v2 rows parse unchanged in both consumers,
`run_tbir_equiv.sh` and `run_arch_dut_fixtures.sh`).
`synchronizer_basic_test` is registered with its domains file, as are
the wait-on-clock fixtures needing extras (`synchronizer_pulse_test`,
`multi_clock_test`). `ref_src` and `test_struct` are wired through to
`--ref-src` / `--test` so the `--test`-selecting fixtures
(`testbench_basic_test`, `testbench_lifecycle_test`,
`axilite_constraint_test`) and the `--ref-src` fixture
(`extern_fn_ref_test`) can register as soon as their constructs land —
no schema v4 needed.

## Suggested sequencing for construct workers

1. ~~**`transaction` → `transactor`** (17 + 19 fixtures): the two
   biggest groups, and `transactor` depends on `transaction`.~~
   **BOTH DONE 2026-06-12** — `transaction` slice (see its resolved
   group), then `transactor` slice (8 fixtures registered; the
   predicted first-blocker churn happened — see its residual map.
   Transactor-specific follow-ups still owed: scalar state fields on
   transactors and >64-bit method params). The `transactor` slice
   also unlocks 10 of the TLM-family fixtures (see the bus group's
   residual map).
2. ~~**`bus`** (15 fixtures): unlocks the entire TLM family.~~
   **LANDED 2026-06-12** — 3 fixtures + 1 new fixture registered;
   12 residuals moved to `transactor` (10) and `fork` (2).
3. ~~**`wait N cycles on <clock>`** (5) + the registry schema v3 row
   format: together they unlock the synchronizer/multi-clock family
   (incl. the schema-blocked `synchronizer_basic_test`).~~ **DONE
   2026-06-12** — all six fixtures registered.
4. **`struct`** (4, two of which also need `bus`).
5. ~~Singletons opportunistically — `.reset(...)`, non-named testbench
   field types, wide literals, `property`, `enum`, `const`,
   `extern fn`, `scoreboard`, ternary, probes, time-literal exprs,
   non-port assignment targets, test-scope `let`.~~ **DONE
   2026-06-12** (singleton-blocker batch) — 11 rows registered
   (9 fixtures, incl. both `--test` structs of the two testbench
   files); residuals: `pipe_reg_test` (property slice),
   `extern_fn_ref_test` (extern fn), `dma_engine_test` (scoreboard),
   `keep_constraints_test` (randomize seam), `mshr_cocotb_test`
   (transactor), probes (own group above).
