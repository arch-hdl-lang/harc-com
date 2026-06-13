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
registered. Amended again 2026-06-13 by the **testbench-field-binding
slice** — see the heartbeat/quiesce cluster section below: a
composite component bound as a `testbench` FIELD (`prod : Producer`)
now lowers identically to a test-scope `let`; `tb_field_agent_test` is
registered. Amended again 2026-06-13 by the **event-record-payload
slice** — see the heartbeat/quiesce cluster section below:
`event<transaction>`/`event<struct>` analysis-port payloads now lower
(value-record by value), fully unlocking `heartbeat_idle_test` and
`wait_until_quiesce_test` (both registered). The **watchdog + periodic
slice (2026-06-13)** then lowers the `watchdog` lifecycle directive
(§8.6) + `on <N> cycles` periodic handlers (§7.10), fully unlocking
`watchdog_quiesce_test` (pass), `watchdog_trip_diagnostic_test` (fail —
trips from cycle 200), and the self-proving `agent_periodic_test` (pass).
Finally the **env quiesced + phase + data-scoreboard-sub slice
(2026-06-13)** closes the cluster: a data-only `scoreboard` bound as an
env SUB-component (`ComponentFieldKind::ScoreboardSub`, accessed by the
nested run-scope path via `ScoreboardOp`/`ScoreboardQuery`'s new
`nested_path`), `<env>.quiesced(N)` (expands to an AND of `idle(N)` over
every leaf sub-component), and named `phase` (call sites inlined). The
heartbeat/quiesce cluster is now FULLY UNLOCKED — `env_quiesced_phase_test`
is registered, trace-diff clean v1↔tbir at seed 1.
Amended again 2026-06-13 by the **probe/force slice**: DUT-internal
signal access via declared probes (read) and force points (write) — the
long-reserved `PortAccess::Probe`/`Force` is now real (was always `Port`;
see tbir-mvp.md divergence 1). `probe_basic_test`, `probe_force_test`,
and `testbench_probe_dut_test` (all `cpu_pipeline.sv`) are registered,
trace-diff clean v1↔tbir at seed 1. See the **probe/force group** below.

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
| `bus` (2) | `bind_remap_test`, `transactor_parse_test` — **`axilite_regs_full_test` REGISTERED 2026-06-13 by the expression-position-call slice: `helper.read(addr)` in assert/bitwise expression positions now hoists into a preceding `Stmt::TransactorCall`; `AxiLiteRegs.sv`, pass, trace-diff clean v1↔tbir** |
| `regblock` (9) | `regblock_basic_test`, `regblock_fields_test`, `regblock_access_test`, `regblock_bitbash_test`, `regblock_addrmap_test`, `regblock_alias_test`, `regblock_record_api_test`, `regblock_record_test`, `regblock_record_recursion_test` — **regblock slice landed 2026-06-12; bus-bound `via` helper (initiator-side BFM) + the regblock-residuals slice (register read in assert/format → `Expr::RegRead`; `bitbash(regs)`) + the field-level/addrmap slice (`regs.REG.FIELD` masked RMW; `addrmap` 3-/4-level access; `alias of`) landed 2026-06-13. The passive `record_write`/`record_read` API (constant-address decode → masked mirror op, no bus) landed 2026-06-13. The per-register `on regs.REG` write callback landed 2026-06-13 (closure-hook cluster, divergence 20): `record_write` → `Stmt::RecordWriteCb` (mirror + recursion guard + callback dispatch) via host-state promotion (shared mirror + `cb_depth` at test scope). ALL NINE regblock fixtures now lower and are registered (`record_recursion_test` is a negative `fail` fixture — the depth-16 guard FATALs identically under both codegens) — see the `regblock` construct group below.** |
| `scoreboard` (2) | `analysis_sink_connect_test`, `axilite_env_test` |
| ~~`tseq` (2)~~ → deeper | `axilite_seqdrv_test`, `transactor_active_test` — **tseq slice landed 2026-06-13; both lower their `tseq` now, and the transactor-state-field slice (2026-06-13) advanced `axilite_seqdrv_test` past its state field too — both now stop at an event field: `axilite_seqdrv_test` → `req : in event<RegOp>` (event-driven unbound transactor), `transactor_active_test` → bound-to transactor event field — see the `tseq` construct group below** |
| ~~transactor state fields~~ → ~~record param~~ → ~~pre/post hooks~~ (resolved) | `axilite_hooks_test` — **REGISTERED 2026-06-13 (closure-hook cluster, divergence 20, pass): the test-scope `on drv.send pre/post` method hooks now lower via host-state promotion (captured test-scope `let`s → `_tb` scalar fields; hook bodies → `FunctionKind::TestHook` functions back-patched onto `TransactorMethodSchema::pre_hooks`/`post_hooks`, fired around the body in `emit_method`); trace-diff clean v1↔tbir at seed 1** |
| transactor state fields (self-proving) | `transactor_state_field_test` — **REGISTERED 2026-06-13** (`cam_dual_basic.sv`, pass): scalar state on the unbound DUT-poking transactor, written + read in method bodies and read back at test scope; trace-diff clean v1↔tbir |
| record-typed method params (self-proving) | `transactor_record_param_test` — **REGISTERED 2026-06-13** (`top_counter.sv`, pass): a `run_for(cmd: RunCmd)` method takes a `transaction` record by value, reads `cmd.ticks` in the body; trace-diff clean v1↔tbir |
| >64-bit method params (self-proving) | `aes_cipher_top_test` — **REGISTERED 2026-06-13** (`aes_cipher_top.sv aes_key_expand_128.sv xtime.sv`, pass) by the wide-value method-param ABI slice: a `uint<128>` value param (`load_block(key, text_in)`) now lowers as a wide-typed local and renders as v1's `_harc_u128`; the body moves it whole-signal to the 128-bit DUT port and compares the 128-bit `text_out`. A param wider than 128 bits (v1's `HarcWide<N>`) stays rejected. Trace-diff clean v1↔tbir at seed 1. |

As with the `transaction` slice, most of the moved fixtures stack
several constructs (bus + scoreboard + sequencer + events); the
counts will keep shifting as those slices land. With the closure-hook
cluster and the wide-value method-param ABI slice landed, every
transactor-construct corpus fixture in this group that lowers is now
registered; the residual transactor-cluster fixtures are blocked on
OTHER constructs (`bus`/`scoreboard`/event-driven forms), not on the
transactor surface itself.

*+ `mshr_cocotb_test` (moved here 2026-06-12 by the singleton batch:
its former `const` blocker is resolved, the next blocker is its
`MshrXactor` transactor).*

### `regblock` construct — PARTIALLY RESOLVED (regblock slice 2026-06-12; bus-bound BFM + residuals + field-level/addrmap 2026-06-13)

The `regblock` register-level frontdoor subset now lowers (synthetic
mirror value-record + `RegblockSchema`; `regs.NAME = v` / `let x =
regs.NAME` → mirror `RecordFieldWrite`/`Expr::RecordField` + the
existing `CallTarget::TransactorMethod` `write`/`read` call edges; rw/ro/
wo policies + reset values + read-side mirror predict; both test-scope
unbound-transactor AND bus-bound initiator-BFM helpers). The
**regblock-residuals slice (2026-06-13)** added register reads in
assert/`${...}` format positions (`Expr::RegRead`) and `bitbash(regs)`
(plus `Expr::ErrorCount`). The **field-level/addrmap slice (2026-06-13)**
added field-level decomposition (`regs.REG.FIELD` — masked RMW on the
whole-register mirror + full-register bus write; shifted extract read;
no new IR), the `addrmap` construct (3-level `chip.inst.REG` + 4-level
`chip.inst.REG.FIELD`, each instance a shifted-offset mirror local), and
`alias of` (the alias shares its target's mirror cell). See
docs/tbir-mvp.md divergences 12 + 20 + 21.

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

**Update 2026-06-13 (initiator-side BFM slice).** The bus-bound `via`
helper (`transactor AxilHelper bound to BusAxiLite`, `hookable` bodies
driving the bus's handshake channels) now lowers — see the *initiator-
side bus-bound BFM* section below and docs/tbir-mvp.md divergence 15.
With that residual cleared, **`regblock_access_test` fully lowers and is
registered** (`AxiLiteRegs.sv`, pass — same BFM helper, but every
register read sits in `let`-RHS position, which the regblock subset
lowers). The other three BFM-helper fixtures advance to their *next*
regblock residual (none is the BFM any more).

Residual first-blocker map (re-run of `harc dump-ir`, 2026-06-13, after
the field-level/addrmap slice):

| Next blocker | Fixtures |
|---|---|
| **(resolved)** initiator-side bus-bound `via` helper | ~~all below~~ — lowers |
| **(resolved)** register read outside `let`-RHS (assert/`${...}` format arg) | `regblock_basic_test` — now lowers (`Expr::RegRead`, registered, pass) |
| **(resolved)** `bitbash(regs)` compile-time walk-all | `regblock_bitbash_test` — now lowers (registered, pass) |
| **(resolved)** field-level decomposition (`field <name> : <ty> @ <bit>` + `regs.REG.FIELD`) | `regblock_fields_test`, `regblock_addrmap_test` — now lower (masked RMW + shifted extract; no new IR; registered, pass) |
| **(resolved)** `addrmap` construct (chip-level composition) + `alias of` | `regblock_addrmap_test`, `regblock_alias_test` — now lower (per-instance shifted-offset mirror locals; alias shares the target's cell; registered, pass) |
| **(resolved)** passive `record_write`/`record_read` API | `regblock_record_api_test` — now lowers (constant-address decode → masked mirror op, no bus; self-authored, registered, pass) |
| **(resolved)** per-register `on regs.REG` write callback | `regblock_record_test`, `regblock_record_recursion_test` — now lower (closure-hook cluster, divergence 20: `Stmt::RecordWriteCb` + `FunctionKind::TestHook` via host-state promotion; registered, `pass`/`fail`) |

The blocker reported is whichever out-of-subset construct lowering hits
first in file order. ALL NINE regblock fixtures now fully lower and are
registered: `regblock_access_test` (reads all `let`-bound),
`regblock_basic_test` (register reads in assert conditions / `${...}`
format args — divergence 12, `Expr::RegRead`), `regblock_bitbash_test`
(`bitbash(regs)` walk-all + `assert errors == 0` via `Expr::ErrorCount`),
`regblock_fields_test` (field-level decomposition),
`regblock_addrmap_test` (`addrmap` 3-/4-level access),
`regblock_alias_test` (`alias of`), `regblock_record_api_test` (the
passive `record_write`/`record_read` API), `regblock_record_test`
(passive record + per-register write callback deriving MM2S_LEN), and the
negative `regblock_record_recursion_test` (a self-write callback trips
the depth-16 recursion guard FATAL identically under both codegens). The
callback is the closure-hook cluster (divergence 20): `record_write`
lowers to `Stmt::RecordWriteCb` (mirror update + per-binding
recursion-depth guard + callback dispatch) and the callback body to a
`FunctionKind::TestHook` function; host-state promotion moves the shared
mirror + `<binding>_cb_depth` to test scope so the run coroutine and the
callback (which may re-enter `record_write`) share the same cell.

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
| ~~`scoreboard` (5)~~ → **ALL RESOLVED 2026-06-13** | ~~`axilite_bound_mon_test`~~, ~~`axilite_multi_payload_test`~~, ~~`transactor_passive_only_test`~~ (bound-to monitor slice), ~~`transactor_agent_mode_test`~~, ~~`transactor_env_mode_test`~~ (agent-mode + cycle-trigger slice) — **all five RESOLVED 2026-06-13; registered, trace-clean.** |
| `sequencer` (1) | `axilite_connect_test` |
| ~~`relation` (1)~~ → **RESOLVED 2026-06-13 (singletons batch 2)** | ~~`relation_inlining_test`~~ — registered, trace-clean. `relation` decls are inert at the file gate; `randomize(r) with R(r)` inlines all three relations' constraints in the typed solver backend (block + alias + alias-of-relations). |
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
| **(resolved 2026-06-13)** initiator-side `fork`/`join_all` TLM issue | ~~`tlm_method_bus_test`~~ (registered, pass) — see the initiator-side fork/join_all section below |
| **(resolved 2026-06-13)** a `fork`/blocking call INSIDE a transactor responder body (nested forwarding — target re-issuing a downstream TLM call) | ~~`tlm_target_forwarding_test`, `tlm_target_fork_forwarding_test`~~ (both registered, pass) — see the nested-forwarding section below |
| **(resolved 2026-06-13)** target-side `out_of_order tags N` RESPONDER threads (hidden tag wires + multi-lane response router) | ~~`tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test`~~ (both registered, pass) — see the OOO-responder lanes section below |
| **(equivalence-proven, gate-blocked)** `tlm_pairing_arch_target_test` | LOWERS + v1↔tbir trace-diff clean, but its ARCH-DUT auto-emitted `_auto_tlm_*_req_stable` TLM SVA `$fatal`s under local Verilator 5.048 for BOTH codegens identically — known local-only artifact, NOT registered |
| **(resolved 2026-06-13)** `bind ... with { ... }` signal remaps | ~~`dma_engine_tlm_target_test`, `dma_engine_tlm_mem_model_test`~~ — see the bus-bind-remap section below |
| **(resolved 2026-06-13)** initiator-side bus-bound BFM (`hookable` bodies driving handshake channels) | ~~`regblock_*`~~ — see the initiator-side BFM section below |

The OOO-responder group (`tlm_target_ooo_lanes_test`,
`tlm_pairing_arch_initiator_test`) is now lowered with the tagged
RESPONDER lanes (per-tag dispatcher + lane coroutines + arbiter); see the
OOO-responder lanes section below. The regblock `via` helpers are the
*initiator-side* BFM, lowered by the separate slice further down.

### target-side `out_of_order tags N` RESPONDER lanes — RESOLVED 2026-06-13

> ~~target-side `out_of_order tags N` RESPONDER threads~~

A bound-to TARGET responder serving an `out_of_order tags N` method now
lowers to the multi-lane topology mirroring v1's
`emit_bound_tagged_tlm_target_actors`: a per-tag **dispatcher** (a
combinational `req_ready` accept gate via `_post_eval_services` + a
coroutine that latches the request args and `req_tag` into a free lane),
**N lane coroutines** (each runs the SAME responder loop-switch body as
the blocking form, then publishes its result + `lane_rsp_valid`), and an
**arbiter** coroutine that routes the highest-index ready lane's response
back on the hidden `rsp_data`/`rsp_tag` wires, so tag 1 can complete
before tag 0. **One new IR field** — `TargetTlmMethodSchema::ooo_tags:
Option<u64>` (folded + range-checked `1..=64` at lowering, `None` for
blocking) — no new IR variants; the lowered responder `function` is
identical to the blocking form, only the surrounding actor topology
differs. The responder body loop-switch is factored into a shared
`emit_responder_loop_switch` reused by the blocking actor and each lane.
The `tlm_call` trace payloads (request edge tagged with the accepted
`_tag`, response edge with the selected `_sel`) match v1 byte-for-byte
(`(int64_t)(...)` cast), so `harc trace-diff` is clean. **Registered**
(2, each passes the v1-vs-tbir equivalence pair, trace-diff clean;
`tlm_pairing_arch_initiator_test` also passes the ARCH-native-DUT sweep):
`tlm_target_ooo_lanes_test` (pure OOO responder, 2 lanes, out-of-order
completion), `tlm_pairing_arch_initiator_test` (mixed `blocking` +
`out_of_order tags 2` responders against an ARCH-authored OOO initiator).
See docs/tbir-mvp.md, the OOO-responder lanes slice. **Divergence from
v1:** the per-tag arg/response arrays are `uint64_t` (the TB-IR value
model) rather than v1's precise per-method C-types — the runtime
`harc_read`/`harc_assign` helpers still width-correct the bus wires, so
behavior and traces are identical.

### nested forwarding (responder re-issues a downstream TLM call) — RESOLVED 2026-06-13

> ~~a `fork`/blocking call INSIDE a transactor responder body~~

A bound-to TARGET responder body may re-issue a downstream TLM call
against a *test-scope* bus binding it does not itself declare —
`thread bus.read(addr) ... let raw = back.read(addr); return raw + ...`
(nested forwarding: front bus → another bus). The responder is lowered
before any test, so the downstream binding's bus type is not in scope at
responder-lowering time. A file-level **pre-scan** of every (desugared)
test's `let <name> : <Bus> = bind ...` declarations builds a
`name → BusDecl` map that is handed to the bound-target responder body's
`bus_bindings` ctx (`src/ir/lower/{mod,transactors}.rs`); the downstream
`back.read(...)` then lowers through the existing `try_lower_bus_call`
(blocking → `TransactorMethod` call edge) or `try_lower_tlm_fork` (OOO →
`Stmt::TlmFork`/`TlmJoinAll`) — the SAME #390 machinery, composed inside
the responder coroutine. **No new IR variants.** The verifier permits a
bus-call edge in an owner-less `TransactorBody` (resolution defers to
emit, which has the test's `bus_bindings`); the tbir backend now passes
the test bindings into the responder loop-switch `emit_stmt` and tags the
downstream `tlm_call` trace event with the responder-instance name
(`ECx::trace_component`, mirroring v1's `current_component_instance`) so
the semantic trace diffs clean. **Registered** (2, each passes the
v1-vs-tbir equivalence pair, trace-diff clean):
`tlm_target_forwarding_test` (blocking downstream `back.read`),
`tlm_target_fork_forwarding_test` (two `fork back.read_ooo` + `join_all`
over an OOO downstream bus). See docs/tbir-mvp.md, the nested-forwarding
slice. A responder SERVING an `out_of_order tags N` method (the
OOO-responder LANE form — distinct from forwarding) is now resolved too;
see the OOO-responder lanes section above. **Still gate-blocked** (not a
lowering gap): the known-artifact `tlm_pairing_arch_target_test`.

### initiator-side `fork`/`join_all` TLM issue — RESOLVED 2026-06-13

> ~~`fork` bus-method calls~~ (initiator side)

A test-scope `let x = fork bus.<method>(args)` issues only the request
side of a bus `tlm_method` and defers the response capture to the next
`join_all`. New IR `Stmt::TlmFork(TlmForkDesc)` + `Stmt::TlmJoinAll(Vec<
TlmForkDesc>)` mirror v1's `try_emit_bus_tlm_fork` / `emit_tlm_join_all`:
a `blocking` method drains issue-order FIFO; an `out_of_order tags N`
method gets a per-`(field,method)` monotonic request tag and drains by
`rsp_tag`-match (multi-lane, so tag 1 can land before tag 0). The pending
list survives `wait` between blocks; a dangling `fork` and a mixed
tagged/untagged barrier are both rejected at lowering. **Registered** (1,
pass, trace-diff clean): `tlm_method_bus_test`. See docs/tbir-mvp.md, the
initiator-side fork/join_all slice. (A `fork` inside a RESPONDER body —
nested forwarding — is now resolved too; see the nested-forwarding
section above.) Target-side OOO responder lanes are now resolved as well
— see the OOO-responder lanes section above.

### bus `bind ... with { ... }` signal remaps — RESOLVED 2026-06-13

> ~~`bind ... with { ch.sig: "port" }` signal remaps~~

A bus binding may override the `<field>_<channel>_<signal>` flat-name
convention per signal: `let mem : Bus = bind dut with { read.addr:
"mem_read_addr", ... }`. `BusBindingSchema` now carries a sorted
`remap: Vec<((channel, signal), port)>` plus a `wire_name(channel,
signal)` resolver (mirrors v1's `bus_remap` / `bus_signal_name`).
`lower_bus_binding` validates each path is exactly `<channel>.<signal>`
(2 segments; malformed → hard `Invalid` error) and records it instead of
rejecting. Both TLM wire-emission sites — `emit_transactor_call` (the
test-scope blocking call edge) and `emit_target_actor` (the target
responder, now passed the test's `bus_bindings`) — route through
`wire_name`, so the override governs both directions. **No new IR
variants.**

**Registered** (3, each passes the v1-vs-tbir equivalence pair,
trace-diff clean): `tlm_bind_remap_test` (self-proving — binds with name
`m` so the convention would drive nonexistent `m_read_*`; every entry
remaps to the real `mem_read_*`/`mem_poke_*` port), `dma_engine_tlm_target_test`,
`dma_engine_tlm_mem_model_test` (corpus — blocking target responders
rejected only for the explicit `bind ... with`). See docs/tbir-mvp.md
bus-bind-remap slice.

### initiator-side bus-bound BFM — RESOLVED 2026-06-13

> ~~initiator-side bus-bound BFM (`hookable` bodies driving handshake channels)~~

The complementary form of the target-side TLM responder: `transactor X
bound to <Bus>` whose `hookable write(addr,data)` / `read(addr)->data`
methods DRIVE the bound bus through handshake channels (`bus.<ch>.send` /
`.recv()` / `bus.<ch>.<sig> = ...`). This is the regblock `via <Helper>`
form. Each hookable lowers to a `TbFunction` (kind `TransactorBody`)
recorded on `TransactorSchema::methods` (NOT `target_methods`), so the
regblock frontdoor's `Helper.write`/`read` call edges (#369) and bare
`helper.method(...)` calls resolve through the existing
`CallTarget::TransactorMethod` dispatch — **no new IR variants**. Inside
the body the bare `bus` keyword (v1's `driver_bus_for_hookables`)
resolves via a placeholder-keyed bus binding, so the existing channel-
handshake lowering applies verbatim; the placeholder flat prefix is
rewritten to the real `let helper = bind <axil>` binding name at test-
binding time. `recv()` now captures every payload signal into a per-field
local (`r.data` / `r.resp`), preserving the bare-scalar `recv()` read.
See docs/tbir-mvp.md divergence 15.

**Registered** (2, pass the v1-vs-tbir equivalence pair, trace-diff
clean): `regblock_access_test`, and `transactor_bound_initiator_state_test`
(bound-initiator-state slice, 2026-06-13 — a BFM whose `read` caches the
bus readback in a `last_read`/`read_count` state struct). The other three
BFM-helper fixtures (`regblock_basic_test`, `regblock_bitbash_test`,
`regblock_record_test`) now lower their `via` helper but stop at deeper
regblock residuals (see the `regblock` construct group above).

Per-instance scalar **STATE fields** on the bound-to initiator BFM now
lower (bound-initiator-state slice, 2026-06-13): a `hookable read(addr)`
body can cache the bus readback into a `last_read : uint<32>` field, read
back at test scope as `helper.last_read` — same `target_state_struct_inst`
machinery as the unbound and bound-target forms. Self-proving fixture
`transactor_bound_initiator_state_test` (`AxiLiteRegs.sv`, pass, trace-diff
clean). One stateful instance per BFM type per file.

**Out of subset for THIS (hookable-BFM) path** (precise rejections):
event/directional fields — a `req : in event<T>` + `on req` transactor is
the **bound-to event-driven driver**, which routes through the
composite-component path instead (RESOLVED 2026-06-13; see the
event-driven-transactor group). The hookable-BFM path itself still rejects
`out_of_order` channels, `fork`-issue, nested transactor calls inside a
BFM body, multiple bound instances of one BFM type per file. A
`bind ... with { ... }` remap on a *handshake-channel* bus (which an
initiator BFM is, e.g. BusAxiLite) is rejected: the bus-bind-remap slice
honors remaps only on `tlm_method`-only buses, since handshake-channel
access bypasses `wire_name` (`bind_remap_test` exercises exactly this and
stays rejected). `tlm_pairing_arch_initiator_test` (a TLM-initiator
fixture pairing an ARCH OOO initiator with HARC `blocking` +
`out_of_order tags 2` RESPONDERS) is now fully lowered and registered —
see the OOO-responder lanes section above.

\* `tlm_pairing_arch_target_test` now LOWERS through the initiator-side
fork/join_all slice and is v1↔tbir trace-diff clean, but its ARCH-DUT
auto-emitted `_auto_tlm_mem_read_ooo_req_stable` TLM SVA `$fatal`s under
local Verilator 5.048 for BOTH codegens identically (the known local-only
5.048-vs-CI-5.034 SVA verdict issue). It is NOT registered — the local
equivalence gate cannot be satisfied — but the equivalence itself is
proven (identical verdict + clean trace-diff). It should pass on CI's
Verilator; register it there once confirmed.

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
| ~~`sequencer`~~ → ~~`tseq`~~ → ~~state field~~ → now **event field / agent-mode** | `axilite_connect_test`, `transactor_agent_mode_test`, `transactor_env_mode_test` — **`sequencer` lowers (sequencer slice), `tseq` lowers (tseq slice), and transactor scalar STATE fields lower (state-field slice, 2026-06-13)**; each now stops at the NEXT tier: `axilite_connect_test` → `req : in event<RegOp>` directional field (event-driven unbound transactor); the agent/env pair → a transactor with **>1 module-typed field** (`dut` + `sb`, agent-mode DUT-handle inheritance), additionally stacking mode-inheritance + cycle-trigger `on dut.x && dut.y` handlers — see the `tseq` construct group |
| ~~`tseq` construct~~ → ~~bound-to state field~~ → ~~bound-to driver~~ → ~~bound-to **MONITOR surface**~~ — **CLUSTER CLOSED 2026-06-13** | ~~`axilite_bound_mon_test`~~, ~~`axilite_multi_payload_test`~~, ~~`transactor_passive_only_test`~~ — **all three RESOLVED 2026-06-13 by the bound-to monitor slice**: `on bus.<ch>.handshake(arg)` observers desugar to rising-edge cycle-trigger `_checkers` (reusing the agent-mode machinery; v1's `emit_bound_monitor_actors` coroutine actor is the documented divergence — equivalent for single-beat handshakes), the `sb : AxilSb` ScoreboardSub feed works, and a `passive` bound instance is accepted. All registered, trace-clean v1↔tbir at seed 1. The full bound-to-agent cluster (driver + monitor) is now closed. |
| ~~`tseq` + `randomize`~~ → sub-component field | `axilite_env_test` — **its `tseq` + `randomize(t)` now lower (tseq slice)**; now stops at an env **sub-component field** type (a deeper env-composition feature, not tseq) |

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

The composite-component **testbench-field binding** the agent slice
flagged as the 5 fixtures' first blocker landed 2026-06-13 (a component
bound as `prod : Producer` inside a `testbench` block now lowers
identically to a test-scope `let`; the impl-for desugaring's `_tb.`
prefix is stripped in every component-access path). **Registered**:
`tb_field_agent_test` — a self-proving fixture (the `agent_on_handler_test`
agent bound as a testbench field), lowering cleanly, passing the v1-vs-tbir
pair, trace-diff clean at seed 1.

### `event<transaction>` / `event<struct>` payloads — **RESOLVED 2026-06-13 (event-record-payload slice)**

Non-scalar analysis-port channels carrying a value-record payload now
lower (`ComponentFieldKind::Event { payload: EventPayload }`,
`OnHandlerSchema::arg_payload`; see docs/tbir-mvp.md divergence 17). #376
had rejected `event<transaction>`/`event<struct>` precisely as a
soundness measure — they parse as `TypeArg::Expr(Ident)`/`Type(Named)`
and would otherwise mis-lower to a scalar callback and fail at C++
compile. `lower_event_payload` resolves the payload against the records
table; the event field becomes `std::vector<std::function<void(
<RecordName>)>>` and `on in_ev(t)` binds a record-typed argument.

**Registered (both pass, trace-diff clean v1↔tbir at seed 1):**

| Fixture | Newly lowers | Next blocker |
|---|---|---|
| `heartbeat_idle_test` | agent + `event<TinyTxn>` + `on in_ev(t)` + `idle_in` poll | — **FULLY UNLOCKED** |
| `wait_until_quiesce_test` | same + `wait until all of … timeout` (agent slice) | — **FULLY UNLOCKED** |
| `watchdog_quiesce_test` | agent + `watchdog period/max_idle` over a record-payload event + `${cycle_count}` log (never trips) | — **FULLY UNLOCKED** (watchdog slice) |
| `watchdog_trip_diagnostic_test` | silent agent + `watchdog` that trips from cycle 200 (`fail`, 9 FAIL lines) | — **FULLY UNLOCKED** (watchdog slice; expect=fail) |
| `agent_periodic_test` | self-proving `on 10 cycles` periodic handler (fires 3× in 35 cycles) | — **FULLY UNLOCKED** (watchdog slice) |
| `env_quiesced_phase_test` | data-only `scoreboard` SUB-component in an env (`DrainSb` held by `HeartbeatEnv`) + named `phase` + `quiesced(N)` env heartbeat aggregation | — **FULLY UNLOCKED** (env quiesced + phase + data-scoreboard-sub slice) |

The heartbeat/quiesce cluster is now FULLY UNLOCKED — every cluster
fixture is registered and trace-diff clean v1↔tbir at seed 1. The env
quiesced + phase + data-scoreboard-sub slice (docs/tbir-mvp.md
divergence 19) closed the last three blockers:
`ComponentFieldKind::ScoreboardSub` (data board as env sub),
`<env>.quiesced(N)` (AND of `idle(N)` over leaf sub-components), and
named `phase` (call sites inlined).

### `sequencer` construct — **RESOLVED 2026-06-13 (sequencer slice)**

The `sequencer` construct now lowers (`ComponentKindTag::Sequencer`,
`CompSource::Sequencer`; see docs/tbir-mvp.md divergence 16). A
`sequencer` is the analysis-source component shape the env-composition
slice already lowers — an `out event<T>` analysis port + `hookable`
methods that generate a stimulus stream and `emit` each item on that
port — so it routes through the existing `ComponentSchema` machinery with
no new IR variants. A `connect <sqr>.<event> -> <drv>.<sink>` edge inside
the composing env wires the emitted stream into a sink method (the UVM
sequencer/driver bridge). **Registered**: `sequencer_connect_test` — a
self-proving fixture (sequencer + literal-range `dispatch(n)` emit loop +
env `connect` → scoreboard sink) authored for this slice, lowering
cleanly, passing the v1-vs-tbir pair, trace-diff clean at seed 1.

The three corpus sequencer fixtures no longer reject on `sequencer` —
each lowers its `sequencer` now but rejects one level deeper. After the
**tseq slice** (below) their `tseq` lowers too, so they shift one more
level deeper (see the tseq residual map).

### agent-mode multi-DUT + cycle-trigger handlers — **RESOLVED 2026-06-13 (agent-mode slice)**

The agent-mode transactor's three remaining blockers now lower
(docs/tbir-mvp.md "agent-mode + cycle-trigger slice"):

1. **Cycle-trigger `on <bool-expr>` monitor handlers** — the always-on
   observer half of an agent-mode transactor (`on dut.axil_w_valid &&
   dut.axil_w_ready`). New IR `ComponentSchema::cycle_handlers`
   (`CycleTriggerHandlerSchema { trigger, edge, function }` +
   `CycleEdge`), distinguished from `on <ev>(arg)` subscriptions by
   `is_event_subscription`. Lowers to a zero-arg `ComponentMethod` body +
   self-relative trigger predicate; the tbir backend installs a
   per-instance `_checkers` closure with edge gating (mirrors v1's
   `emit_cycle_trigger`). Present on BOTH active and passive instances.
2. **Multi-module-field transactor + self-relative sub-scoreboard poke** —
   the agent-mode transactor carries `dut` + `sb` (a `ScoreboardSub`) +
   scalar state + an `in event<RegOp>` simultaneously. `sb.writes =
   sb.writes + 1` inside a cycle/on handler resolves the sub-scoreboard
   self-relatively (`self`-rooted `nested_path`, re-rooted via
   `self_subst` at emission); the component method ctx now carries the
   `scoreboards` table.
3. **Agent + nested-env `connect` bridges** — `connect` is now resolved
   for `Agent` decls (the agent's `sequencer.dispatched -> drv.req`
   bridge), and the tbir backend recurses through `Sub` fields to install
   a nested sub-component's bridges (env→agent→drv). A sequencer
   `hookable dispatch(txns: TSeq<Record>)` param now types `RecordSeq`
   (renders `std::vector<Record>`).

**Registered**: `transactor_agent_mode_test`, `transactor_env_mode_test`
(`AxiLiteRegs.sv`, pass) — same agent/env decl reused active + passive;
active drives 5 AXI round-trips through sequencer→connect→`on req(t)`,
both transactors' cycle-trigger observers tally 5 writes + 5 reads off
the shared DUT. Both trace-diff clean v1↔tbir at seed 1.

**Divergence**: hard `when active` body elision on a passive instance is
NOT implemented (v1 does not elide for these fixtures either — passive
correctness comes from the test never dispatching the passive
sequencer). The `active`/`passive` mode on a composite-component
test-scope `let` is accepted and ignored, matching v1.

### `tseq` construct — **RESOLVED 2026-06-13 (tseq slice)**

> ~~TB-IR lowering does not support the `tseq` construct yet~~

The `tseq` (transaction-sequence) construct now lowers
(`src/ir/lower/tseqs.rs`; see docs/tbir-mvp.md divergence 17). A `tseq` is
a named generator of a sequence of transaction values iterated with `for
t in <TSeq>`. New IR (minimal): `IrType::RecordSeq(RecordId)`,
`FunctionKind::Tseq { record }`, `CallTarget::Tseq(name)`,
`Stmt::SeqPush`, `Expr::SeqLen`/`Expr::SeqIndex`. The generator lowers to
a `[&]`-lambda returning `std::vector<Record>` (v1's `emit_tseq`); `yield
t` → `SeqPush`; `randomize(t)` inside the body reuses the merged
constraint-IR seam (#372 — the solver problem table already catalogs tseq
randomize sites); `let txns = Gen(5)` → a `CallTarget::Tseq` edge typing
the local `RecordSeq`; `for t in txns` → a counted loop copying `txns[i]`
into the record loop variable each iteration (the sequence is
materialized once and may be re-iterated). **Registered**:
`tseq_basic_test` — a self-proving fixture (randomize + post-randomize
field override + reusable double-iteration), and `axilite_fuzz_test` —
the corpus fuzz test (`--test AxiLiteFuzzTest` + `axilite_regs_test.harc`
helpers), a `tseq RandomRegs(5)` of random writes/reads through the
`axil_write`/`axil_read` impure helpers. Both trace-diff clean v1↔tbir at
seed 1; both need Z3.

The remaining corpus tseq fixtures lower their tseq now but reject one
level deeper. After the transactor-state-field slice (2026-06-13) the
state-field blocker is gone for the unbound forms; residual first-blocker
map (re-run of `harc dump-ir`, 2026-06-13):

| Next blocker | Fixtures |
|---|---|
| ~~transactor **event field** (`req : in event<RegOp>`)~~ → **RESOLVED 2026-06-13** for the UNBOUND form (event-driven-transactor slice) AND the BOUND form (bound-to event-driven-driver slice, 2026-06-13); `axilite_seqdrv_test` + `transactor_active_test` now **PASS** (registered). `axilite_connect_test` advanced past the event field to a data-only `scoreboard` SUB-component in its env (see below). | `axilite_connect_test` (env data-scoreboard sub) |
| transactor with **>1 module-typed field** (`dut` + `sb` — agent-mode DUT-handle inheritance) | `transactor_agent_mode_test`, `transactor_env_mode_test` |
| bound-to **monitor/agent surface** (`sb` sub-component field + `on bus.<ch>.handshake` MONITOR handlers) — the bound-to scalar state field (bound-initiator-state slice) AND the bound-to event-driven DRIVER (`in event` + `on req` driving the bound bus; bound-to event-driven-driver slice, 2026-06-13) now lower; what remains is the passive handshake-MONITOR actor + the `sb` scoreboard SUB-component on a bound transactor | `axilite_bound_mon_test`, `axilite_multi_payload_test`, `transactor_passive_only_test`, `transactor_parse_test` |

The **event-driven-transactor slice** (2026-06-13) lowers the consumer
side: an unbound `transactor` with an `in event<T>` pipe + `on req(t)`
handler routes through the composite-component table, with a
`ComponentFieldKind::Dut` handle the synchronous handler pokes and a
`ConnectSink::Event` variant for sequencer→transactor `connect` event
bridges. Proven by `event_driven_transactor_test` (self-proving) +
`axilite_seqdrv_test` (corpus).

The **bound-to event-driven-driver slice** (2026-06-13) extends this to
the *bound* form: a `transactor X bound to <Bus>` with an `in event<T>`
pipe + `on req(t)` handler whose body drives the bound bus's handshake
channels (`bus.<ch>.send/recv`, `bus.<ch>.<sig>`) instead of a private
DUT handle. It routes through the same composite-component table, now
carrying `ComponentSchema::bound_bus`; the `on req` handler body lowers
with the bound `BusDecl` visible under the placeholder prefix
(`transactors::INITIATOR_BUS_PLACEHOLDER`), so `send`/`recv` CFG-inline to
the same bounded valid/ready spin loops as the bound-initiator BFM. At
test scope, `let xact : X active = bind axil` validates the binding and
fills the placeholder prefix with the real binding name; `emit xact.req(t)`
fires the handler synchronously, `xact.<state>` reads the per-instance
scalar state. Proven by `transactor_active_test` (corpus, registered,
trace-clean v1↔tbir). Reuses no new tbir codegen — the synchronous
on-handler dispatch + the existing handshake send/recv lowering compose.
Residual:

- The passive **handshake-MONITOR** half (`on bus.<ch>.handshake(arg)`
  observers sampling valid&&ready per channel into a sub-scoreboard) is a
  distinct slice — it needs the monitor-actor coroutine topology (v1's
  `emit_bound_monitor_actors`) + a `ScoreboardSub` field on a bound
  transactor. `lower_component_schema` now rejects it precisely (no
  mis-lower) and the rejection names that slice. Blocks
  `axilite_bound_mon_test`, `axilite_multi_payload_test`,
  `transactor_passive_only_test`, `transactor_parse_test` (the last is
  check-only — `harc check` passes; only `harc sim` reaches the rejection).
- `axilite_connect_test` — its `env AxilEnv` holds a **data-only
  `scoreboard` SUB-component** (`sb : AxilSb`, queue + scalar, no methods)
  accessed through the env (`env.sb.expected.push/pop`). Queue access
  routes through `ScoreboardOp`, tied to `ScoreboardSchema`, not the
  component path — so an env-held data-scoreboard with through-env queue
  ops is the **env-field-binding / data-scoreboard-sub** slice, not the
  event-driven-transactor surface. The event→event `connect` bridge it
  needs IS implemented (and exercised by `event_driven_transactor_test`).

The transactor-state-field slice (divergence 10) materialized scalar
STATE fields on the UNBOUND DUT-poking transactor (per-instance state
struct, written/read in method bodies + read back at test scope) — the
event-driven transactor reuses the same scalar-state field shape
(`last_read`/`fires`) through the component path's `Scalar` field kind.
Still deferred: state on the *bound-to initiator* BFM, the
`_last_in/out_cycle` heartbeat stamps (idle predicates), and
statement-level pre/post hooks.

The two `transactor_*_mode` fixtures stack mode-inheritance
(`active`/`passive` flowing env→agent→transactor) + cycle-trigger `on
dut.x && dut.y` handlers + the multi-module-field (`dut` + `sb`)
agent-mode form inside the transactor — they own the agent-mode slice.

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
| **(resolved)** non-scalar struct field (`data : Vec<uint<32>, 4>`) | `tlm_pairing_arch_burst_target_test`, `tlm_pairing_arch_burst_initiator_test` — now lower (struct `Vec`-field slice, 2026-06-13: `std::array<T, N>` member + indexed `rec.data[i]` + record-returning `tlm_method` pack/unpack; both registered, pass) |

The two scoreboard residuals belong to the data-only-scoreboard owner
(methods + `queue<Struct>`) and the record-payload-in-queue seam. The
`Vec`-typed struct field + record-returning `tlm_method` seam is now
CLOSED (struct `Vec`-field slice): a `Vec<scalar, N>` field lowers to a
`std::array<T, N>` member with indexed `rec.data[i]` access, and a
record-returning `tlm_method` packs/unpacks the response pin through the
generated `harc_pack_<R>`/`harc_unpack_<R>`/`harc_drive_<R>` helpers —
unblocking both `tlm_pairing_arch_burst_*` fixtures.

### Probe declarations on `let dut` — 3 fixtures — **RESOLVED 2026-06-13 (probe/force slice)**

> ~~TB-IR lowering does not support probe declarations on `let dut` yet~~

All three fixtures now fully lower and are registered, trace-diff clean
v1↔tbir at seed 1:

| Fixture | What it exercises |
|---|---|
| `probe_basic_test` | three read-only `probe`s hoisting `alu0.{a,b,result}`; `dut.<probe>` reads route through the SV bind-stub accessor |
| `probe_force_test` | a read `probe` + a `probe force` (write/`release` fault-injection): force writes lower to the `_drv`/`_en` pair, `release` clears `_en` |
| `testbench_probe_dut_test` | a testbench-OWNED probed DUT + a `function reset()` (regression #204 for the impl-for desugar preserving probes) |

Implementation: `PortAccess::Probe`/`Force` now flows out of lowering
(was always `Port`); `Stmt::ProbeRelease`; lowering enforces
Probe-read-only / Force-write-only; the tbir backend mirrors v1's
`Emitter::probes` (`dut->rootp-><DutType>__DOT__harc_probes__DOT__<name>`,
`_drv`/`_en` writes, `V<DutType>___024root.h` include). The SV bind stub
(`__harc_probe_<DutType>.sv`) is shared by both codegens. See
tbir-mvp.md divergence 1 and docs/probe-signals.md. **Subset**: scalar
probe types only; multi-segment `at`-paths are Verilator-validated, not
harc-validated; `--dut` (ARCH-compiled DUT) probing is out of scope.

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
| `pipe_reg_test` | `property` construct | **RESOLVED + registered (singletons batch 2, 2026-06-13)** — an SVA-style `property NAME ... end property` declaration is accepted at the file gate but inert: it is only observable via an `assert property NAME` / named-property `assert` reference, both of which the test-body lowering still rejects. The fixture's property is unreferenced, so it is a no-op under both codegens — mirrors v1, which only emits a check at a reference site. |
| `width_methods_test` | `.trunc(...)` method call | **RESOLVED + registered** — `.trunc/.zext/.sext/.resize` lower to `Expr::WidthCast` (≤ 64-bit, v1's mask/cast/shift-fill shapes + direction checks); scalar `as uint<W>` casts lower as width relabels |
| `keep_constraints_test` | `enum` construct | **enum RESOLVED; deeper blocker** — enums lower as named integer constants (variant index, first definition wins) and enum-typed transaction fields as scalar indices; the fixture now stops at statement-level `randomize` (constraint-IR seam) |
| `extern_fn_ref_test` | `extern fn` construct | **RESOLVED + registered (singletons batch 2, 2026-06-13)** — `extern function name(...) -> ret` (spec §9) is inert at the file gate; a call lowers to `CallTarget::ExternFn` (raw symbol name, no `harc_helper_` mangling) so it links against the user's `extern "C"` definition supplied via `--ref-src`. The file-scope `extern "C" { … }` forward-declaration block is shared with v1 (`cpp_tb::emit_extern_fn_decls`). Registered via the existing `ref_src` registry column. |
| `async_fifo_test` | time literals in expression position | **RESOLVED + registered** — `wait 80ns` lowers to `Terminator::WaitTimePs` (ps resolved at lowering; v1's inline `eval_clocks_until(now_ps + N)`); `log(debug, ...)` severity also landed here |
| `dma_engine_test` | `scoreboard` construct | **scoreboard RESOLVED (data-only subset); deeper blocker** — the scoreboard now lowers; the fixture stops at its passive `MemXactor` transactor's event-driven `on`-handlers (moved to the scoreboard group's residual map) |
| `linklist_basic_test` | ternary expressions | **RESOLVED + registered** — `Expr::Ternary` emits the C++ `?:`; also forced the `WaitCyclesSync` terminator (waits inlined from helper bodies take v1's synchronous `tick()` path, fixing a `sim_end` clock-attribution trace delta) |
| `mshr_cocotb_test` | `const` construct | **const RESOLVED; deeper blocker** — file-scope `const` (integer-literal initializers) substitutes at use sites; the fixture now stops at the `transactor` construct (moved to that group) |
| `packed_vec_lane_test` | assignment to a non-port, non-local target | **RESOLVED + registered** — constant-lane `dut.<port>[i]` reads/writes lower via `PortRef::lane`; emission splits packed lanes (`harc_vec_lane_*<W>` via the `--sv` lane table) from unpacked-array subscripts, like v1 |
| `if_wait_for_in_then_test` | test-scope `let done_pulses` | **RESOLVED + registered** — plain test-scope lets hoist to the head of the run function (v1 hoists to `main` scope before the coroutine); a check-phase reference is a precise rejection (run/check are separate IR functions, so v1's shared-capture scoping is not representable) |

### Singletons batch 2 (2026-06-13)

Three standalone constructs + one free registration, landed in one PR:

| Fixture | Was blocked on | Outcome |
|---|---|---|
| `relation_inlining_test` | `relation` construct | **RESOLVED + registered** — relation decls inert at the gate; constraint inlining already handled by `constraints::typed_lower` |
| `pipe_reg_test` | `property` construct | **RESOLVED + registered** — property decl inert at the gate; `assert property` references still rejected (unreferenced property is a no-op, mirrors v1) |
| `extern_fn_ref_test` | `extern fn` construct | **RESOLVED + registered** — `CallTarget::ExternFn` (raw symbol) + shared file-scope `extern "C"` block (`cpp_tb::emit_extern_fn_decls`); links via `--ref-src` |
| `transactor_parse_test` | (lowered clean on main, but `tbir::emit` failed) | **RESOLVED + registered** — a TB-IR codegen bug emitted component method / `on`-handler bodies with an EMPTY record table, so a body-local `RecordInit` (`let c : Completion`) errored "references missing record"; both lambda emitters now pass `&prog.records`. |

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
   Transactor-specific follow-ups: scalar state fields on UNBOUND
   transactors **DONE 2026-06-13** (state-field slice — divergence 10,
   self-proving `transactor_state_field_test` registered); >64-bit method
   params **DONE 2026-06-13** (wide-value method-param ABI slice — a
   `uint<N>`/`sint<N>` value param up to 128 bits renders as `_harc_u128`;
   `aes_cipher_top_test` registered); still owed: event-driven /
   bound-initiator transactor state). The `transactor` slice
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
