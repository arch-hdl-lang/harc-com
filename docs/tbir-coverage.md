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
`randomize` seam). Further amended 2026-06-13 by the **env-composition
slice** — see the resolved `env`/`agent` composition cluster section
below: the flat-struct core (env + connect + scoreboard methods +
analysis `out event`/`emit`) landed and `analysis_sink_connect_test`
is registered. Amended again 2026-06-13 by the **agent/on-handler
slice** — see the `agent` + `on <ev>` handlers section below: `agent`
composition, `on <ev>(arg)` self-subscriptions, test-scope path-emit,
and `idle*` heartbeat predicates landed; `agent_on_handler_test` is
registered, and the 5 heartbeat/quiesce fixtures now reject one level
deeper (composite-component testbench-field binding).

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
| `regblock` (7) | `regblock_basic_test`, `regblock_fields_test`, `regblock_access_test`, `regblock_bitbash_test`, `regblock_addrmap_test`, `regblock_alias_test`, `regblock_record_test` — **regblock slice landed 2026-06-12; see the `regblock` construct group below for the per-fixture residual map (all 7 still blocked on the bus-bound `via` helper / field-level access / addrmap).** |
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

### `regblock` construct — PARTIALLY RESOLVED 2026-06-12 (regblock slice)

The `regblock` register-level frontdoor subset now lowers (synthetic
mirror value-record + `RegblockSchema`; `regs.NAME = v` / `let x =
regs.NAME` → mirror `RecordFieldWrite`/`Expr::RecordField` + the
existing `CallTarget::TransactorMethod` `write`/`read` call edges; rw/ro/
wo policies + reset values + read-side mirror predict; test-scope
unbound-transactor helper). No new IR variants — see docs/tbir-mvp.md
divergence 12.

The first-blocker caveat applied in full: **zero** of the 7 corpus
fixtures became fully lowerable — every one's `via` helper is a
`transactor ... bound to BusAxiLite`, the **bus-bound helper form** that
is the dominant residual blocker (its method bodies resolve the `bus`
keyword against a test-scope bus binding, which the IR pipeline does not
model yet). Since the group produced no registrable corpus row, the
slice added a new fixture, `regblock_subset_test` (`top_counter.sv`,
pass — rw/ro/wo + reset + frontdoor + read-predict + WO mirror-read,
over a test-scope unbound-transactor helper), registered in the
equivalence registry.

Residual first-blocker map (re-run of `harc dump-ir`, 2026-06-12):

| Next blocker | Fixtures |
|---|---|
| bus-bound `via` helper (`transactor AxilHelper bound to BusAxiLite`) | `regblock_basic_test`, `regblock_access_test`, `regblock_bitbash_test`, `regblock_record_test` |
| field-level decomposition (`field <name> : <ty> @ <bit>` + `regs.REG.FIELD`) | `regblock_fields_test`, `regblock_addrmap_test` |
| `addrmap` construct (chip-level composition) | `regblock_alias_test` |

The blocker reported is whichever out-of-subset construct lowering hits
first in file order. `regblock_fields_test` / `regblock_addrmap_test`
reach their `field` declarations before the (also-blocking) bus-bound
helper because the regblock decl now lowers far enough to parse fields;
`regblock_alias_test` has no `field`s and reaches its `addrmap`. All 7
ultimately need the bus-bound helper; `fields`/`addrmap` additionally
need field-level access (and `addrmap`/`alias` need the `addrmap`
construct + `alias of`). The passive `record_*` API + per-register `on`
callbacks (`regblock_record_test`) and `bitbash` (`regblock_bitbash_test`)
are further deferred features behind the bus-bound helper.

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
| `randomize` (statement-level, 1) | `axilite_constraint_test` (`AxiLiteConstraintTest`) — **RESOLVED 2026-06-13 by the randomize slice; registered, trace-clean. See the `randomize` group below.** |

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

### target-side TLM / bus-bound transactor — RESOLVED 2026-06-12 (target-TLM slice)

> ~~TB-IR lowering does not support transactor `X` bound to a bus type yet~~

The blocking target-side TLM responder form lowers: `transactor X bound
to <Bus>` with `thread bus.<method>(...)` responder threads, persistent
scalar state fields (read/written in the body **and** read from the test
as `target.<field>`), and the per-instance state struct + one
background-coroutine actor per target method. See docs/tbir-mvp.md
divergence 11.

**Registered** (4, all pass the v1-vs-tbir equivalence pair, trace-diff
clean): `tlm_target_thread_test`, `tlm_target_thread_if_test`,
`tlm_target_thread_runtime_loop_test`,
`tlm_target_thread_early_return_test`.

Residual map for the remaining TLM fixtures (each now stops at its REAL
next blocker; exact `Unsupported` message in parentheses):

| Next blocker | Fixtures |
|---|---|
| `fork`/`join_all` TLM issue (needs `Terminator::Fork` + `ForkArmKind::BusMethodCall`) | `tlm_method_bus_test`, `tlm_target_fork_forwarding_test`, `tlm_pairing_arch_target_test`* ("`fork` bus-method calls") |
| `out_of_order tags N` target threads (hidden tag wires + multi-lane response router) | `tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test` ("serving a `out_of_order` method") |
| `bind ... with { ... }` signal remaps | `dma_engine_tlm_target_test`, `dma_engine_tlm_mem_model_test` ("bus bind signal remaps") |
| nested transactor/method call inside a responder body (forwarding to another target) | `tlm_target_forwarding_test` ("transactor/method call `.read(...)`") |
| initiator-side bus-bound BFM (`hookable` bodies driving handshake channels) | `regblock_basic_test`, `regblock_access_test`, `regblock_bitbash_test`, `regblock_record_test` ("`hookable` (initiator-side method)") |

The fork group's blocking-call halves already lower; the OOO group needs
the tagged responder lanes; the remap group needs custom wire naming;
the forwarding fixture needs nested call edges inside a responder; the
regblock `via` helpers are the *initiator-side* BFM, a separate slice
from this target-side responder work.

\* `tlm_pairing_arch_target_test` still does not reach the equivalence
stage (fork-blocked), so the known local-only Verilator
5.048-vs-CI-5.034 SVA verdict issue on this fixture remains moot from
CI's perspective.

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

### `env` / `agent` composition cluster — **PARTIALLY RESOLVED 2026-06-13 (env-composition slice)**

The env/agent cluster's flat-struct core landed: `env` composition +
`connect` (analysis-port → scoreboard sink) + scoreboard **methods**
(instance state materialized) + analysis-source `out event`/`emit`. Three
source shapes lower into one `ComponentSchema` (see docs/tbir-mvp.md
divergence 14). **Registered**: `analysis_sink_connect_test` — the one
fixture in the cluster whose blocker chain is exactly env + connect +
scoreboard-methods + event/emit (no `agent`/`tseq`/`sequencer`/
`randomize`). It lowers cleanly, passes the v1-vs-tbir equivalence pair,
and trace-diffs clean.

The other cluster fixtures each stack a deeper construct the slice
deliberately rejects. Residual first-blocker map (re-run of `harc
dump-ir`, 2026-06-13):

| Next blocker | Fixtures |
|---|---|
| `agent` construct + `on <ev>` event handlers | `heartbeat_idle_test`, `wait_until_quiesce_test`, `watchdog_quiesce_test`, `watchdog_trip_diagnostic_test`, `env_quiesced_phase_test` (also need `phase`, `idle(N)`/`quiesced(N)` predicates, watchdog) |
| `sequencer` construct | `axilite_connect_test`, `transactor_agent_mode_test`, `transactor_env_mode_test` |
| `tseq` construct | `axilite_bound_mon_test` |
| `tseq` + `randomize` | `axilite_env_test` (env composition itself now lowers; the `RandomTxns` tseq + `randomize(t)` in the run body are the next blockers) |

The env-composition machinery (component structs, methods with instance
state, connect wiring, emit fan-out) is now in place; the residual
fixtures need the **event-handler** (`agent` + `on`), **sequencer**, and
**tseq/randomize** slices layered on top.

### `agent` + `on <ev>` handlers — **PARTIALLY RESOLVED 2026-06-13 (agent/on-handler slice)**

The `agent` construct and `on <ev>(arg)` event handlers now lower
(`ComponentKindTag::Agent`, `ComponentSchema::on_handlers`,
`Stmt::ComponentEmit` with a `base`, `Expr::ComponentIdle`; see
docs/tbir-mvp.md divergence 15). An `agent` composes an `event<scalar>`
self-event + an `on in_ev(t)` handler that registers as a subscriber
closure (with the `_last_in_cycle` activity bump), driven by a test-scope
path `emit tagger.in_ev(v)`; the `idle`/`idle_in`/`idle_out` heartbeat
predicates lower as `Expr::ComponentIdle`. **Registered**:
`agent_on_handler_test` — a self-proving fixture (agent + on-handler +
path-emit + `idle_in`) authored for this slice, lowering cleanly, passing
the v1-vs-tbir pair, trace-diff clean at seed 1.

The 5 heartbeat/quiesce fixtures the env-composition slice flagged as
"need the agent slice" no longer reject on `agent` — they now reject one
level deeper. Residual first-blocker map (re-run of `harc dump-ir`,
2026-06-13):

| Next blocker | Fixtures |
|---|---|
| composite-component **testbench field** binding (`prod : Producer` / `agent : SilentAgent` / `top : HeartbeatEnv` inside a `testbench` block — a separate binding slice; agents/envs bind test-scope `let` today) | `heartbeat_idle_test`, `wait_until_quiesce_test`, `watchdog_quiesce_test`, `watchdog_trip_diagnostic_test`, `env_quiesced_phase_test` |

Once the testbench-field binding lands, these stack further constructs
this slice does not implement: `event<TinyTxn>` (transaction/struct event
payloads, not just scalars), `quiesced(N)` (env heartbeat aggregation
over sub-components), `watchdog` + `on <N> cycles` periodic triggers,
named `phase`, and `wait until <preds> timeout fail` with heartbeat
predicates. Those are the next layered slices.

### `scoreboard` construct — **PARTIALLY RESOLVED 2026-06-12 (scoreboard slice)**

> ~~TB-IR lowering does not support the `scoreboard` construct yet~~

The scoreboard slice landed the **data-only host-state subset**:
`ScoreboardSchema` (scalar counters + `queue<T>` of a scalar element
type), `Stmt::ScoreboardOp` (`QueuePush`/`QueuePop`/`ScalarWrite`),
`Expr::ScoreboardQuery` (scalar read / `size()` / `empty()`), and
scoreboard-instance struct emission held on the `_tb` struct
(`harc_rt::HarcQueue<T>` members; v1's `emit_scoreboard` shape — see
docs/tbir-mvp.md divergence 12).

**First-blocker caveat applied in full.** `scoreboard` was the first
*item-scan* reject in all of its fixtures, masking deeper blockers.
With the gate lifted, **zero** of the corpus fixtures gated on
`scoreboard` became fully lowerable — every one stacks a further
construct behind the scoreboard declaration. The slice therefore added
a self-proving fixture, `scoreboard_basic_test` (queue push/pop/size/
empty + scalar counter read/write, run↔check-shared instance, against
the `Top` counter DUT), registered in the equivalence registry
(`top_counter.sv`, pass).

Residual first-blocker map (re-run of `harc dump-ir`, 2026-06-12 —
each fixture now stops at its REAL next blocker):

| Next blocker | Fixtures |
|---|---|
| `randomize` (constraint-IR seam) | `axilite_sb_test` |
| `env` / `connect` composition + scoreboard methods | ~~`analysis_sink_connect_test`~~ **RESOLVED + registered 2026-06-13 (env-composition slice — scoreboard methods now lower as components)**; `axilite_env_test` (env now lowers; next blocker is `tseq`/`randomize`) |
| `randomize` (constraint-IR seam) — **landed 2026-06-13**, but `axilite_sb_test`'s first blocker is the scoreboard `.push` method, not randomize | `axilite_sb_test` |
| `env` / `connect` composition | `axilite_env_test`, `analysis_sink_connect_test` (also needs scoreboard methods + `event`/`emit`) |
| scoreboard **methods** (per-instance state materialization — out of the data-only subset) | `analysis_sink_connect_test` |
| passive transactor `on`-handlers (event-driven monitor) | `dma_engine_test` |
| `queue<Struct>` element type (record-payload-in-queue seam) + transactor `on ... phase` | `scoreboard_typed_queue_test` |
| `tseq` construct | `axilite_bound_mon_test`, `axilite_multi_payload_test` |

The scoreboard-**methods** residual is resolved by the env-composition
slice (2026-06-13): a method-bearing scoreboard now lowers as a
composite `ComponentSchema` with materialized instance state, unblocking
`analysis_sink_connect_test`. The `queue<Struct>`
(`scoreboard_typed_queue_test`) residual stays with this construct's
owner; the rest belong to the `randomize` / `agent` / `tseq` slices.

### `randomize` construct — **RESOLVED 2026-06-13 (randomize slice)**

> ~~TB-IR lowering does not support `randomize` yet~~

The randomize slice landed the **statement/terminator form** through the
constraint-IR seam: `Terminator::Randomize { target, constraints:
ConstraintRef, succ }` + a `TbProgram::constraint_sites` table the
`ConstraintRef` indexes. Lowering merges transaction `keep`s ahead of
the call-site `with {...}` body (spec §4) and records the
`ConstraintProblemId` handle per site. The tbir backend reuses v1's
Z3-solve emission verbatim (`cpp_tb::emit_randomize_snippets`) — see
docs/tbir-mvp.md divergence 14.

Two fixtures registered (both trace-clean v1↔tbir at seed 1):

| Fixture | Form exercised |
|---|---|
| `keep_constraints_test` | bare `randomize(t)` + transaction-level `keep`s (range membership, `% 4 == 0` alignment, enum exclusion); 30 iterations, host-side asserts |
| `axilite_constraint_test` (`AxiLiteConstraintTest`, + `axilite_regs_test.harc` helpers) | `randomize(p) with` cross-field Z3 constraints, drives AXI-Lite writes/reads |

Residual randomize blockers (the stacked fixtures the seam does **not**
yet unlock — each stops at a DIFFERENT construct's gate, not randomize):

| Next blocker | Fixtures |
|---|---|
| scoreboard `.push(...)` method (per-instance state — out of the data-only scoreboard subset) | `axilite_sb_test` |
| `tseq` construct | `axilite_fuzz_test` |
| no SV DUT in the corpus (`let dut : DummyDut`; lowers + emits both codegens, but the equivalence harness needs a Verilator-buildable DUT) | `uint64_unique_randomize_test` |

Also residual within the construct itself (precise rejections, not in
these fixtures): the `randomize` **expression** form (`let v =
randomize(t)`) and method-body randomize (the constraint-IR problem
table only catalogs test/tseq sites).

### `struct` construct — 4 fixtures — **RESOLVED 2026-06-12 (struct slice)**

> ~~TB-IR lowering does not support the `struct` construct yet~~

The struct slice landed the **shared value-record subset**: a `struct`
lowers into the same `TbProgram::records` table (`RecordSchema`) a
`transaction` uses — v1's `emit_struct_record` already routes through
the same `emit_record_struct` — so it reuses every record-local op
(`RecordInit` / `RecordFieldWrite` / `Expr::RecordField`) with **no new
IR variants** (field lowering shared via `lower_record_field`). See
docs/tbir-mvp.md (the `struct` construct subset note).

**First-blocker caveat applied in full.** With the struct gate lifted,
**zero** of the four corpus fixtures became fully lowerable — every one
stacks a further already-rejected blocker behind its `struct` decl
(confirmed by re-running `harc dump-ir`). Two of them
(`post_eval_provider_test`, `scoreboard_typed_queue_test`) never even
reached the struct gate — their scoreboard internals reject earlier in
the pipeline. The slice therefore added a self-proving fixture,
`struct_basic_test` (scalar fields + literal defaults, default-construct
in a loop, field reads/writes in arithmetic / branch / assert / format
args, against the `Top` counter DUT), registered in the equivalence
registry (`top_counter.sv`, pass).

Residual first-blocker map (re-run of `harc dump-ir`, 2026-06-12 — each
fixture now stops at its REAL next blocker):

| Next blocker | Fixtures |
|---|---|
| scoreboard **methods** (`observe`; per-instance state materialization — out of the data-only scoreboard subset) | `post_eval_provider_test` (also needs transactor state fields, transactor-typed transactor/scoreboard fields, and `on ... phase post_eval` event handlers) |
| `queue<Struct>` element type (record-payload-in-queue seam) | `scoreboard_typed_queue_test` (also needs scoreboard methods + transactor `on ... phase` handlers) |
| non-scalar struct field (`data : Vec<uint<32>, 4>`) | `tlm_pairing_arch_burst_target_test`, `tlm_pairing_arch_burst_initiator_test` (both also need the bus-bound / struct-returning `tlm_method` path: target-TLM `thread bus.method` and named struct-field `recv()` capture / `rsp.data[i]`) |

All four residuals belong to other slices — the data-only-scoreboard
owner (methods + `queue<Struct>`), the record-payload-in-queue seam, and
the target-TLM / non-scalar-record-field seam (`Vec`-typed struct
fields + struct-returning `tlm_method`). As predicted in the
suggested-sequencing note, `struct` alone unlocks no corpus fixture
because each one is a deeper construct stack.

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
| `dma_engine_test` | ~~`scoreboard` construct~~ **scoreboard gate lifted (slice 2026-06-12); next blocker is the passive transactor `on`-handler monitor** — see the scoreboard group's residual map |
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
| `dma_engine_test` | `scoreboard` construct | **scoreboard RESOLVED (data-only subset); deeper blocker** — the scoreboard now lowers; the fixture stops at its passive `MemXactor` transactor's event-driven `on`-handlers (moved to the scoreboard group's residual map) |
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
4. ~~**`struct`** (4, two of which also need `bus`).~~ **LANDED
   2026-06-12** — shared value-record subset (reuses the transaction
   record machinery; no new IR variants); self-proving
   `struct_basic_test` registered. No corpus fixture unlocked fully
   (each stacks scoreboard methods / `queue<Struct>` / non-scalar
   `Vec` struct fields + target-TLM); see the `struct` group above.
   - ~~**`scoreboard`** (data-only host-state subset)~~ **LANDED
     2026-06-12** — `ScoreboardSchema` + `Stmt::ScoreboardOp` +
     `Expr::ScoreboardQuery`; self-proving `scoreboard_basic_test`
     registered. No corpus fixture unlocked fully (every one stacks
     env/agent/tseq/randomize/struct/`on`-handlers); residuals owed to
     this slice: scoreboard methods + `queue<Struct>` (see the
     scoreboard group above).
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
