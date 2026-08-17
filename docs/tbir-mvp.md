# TB-IR MVP — shipped status and documented divergences

Status: **Shipped (MVP subset), merged on main.**
Date logged: 2026-06-11.
Companion: [tb-ir-design.md](tb-ir-design.md) (the IR contract this doc
diverges from), [tb-ir-plan.md](tb-ir-plan.md) (delivery plan; phase
table annotated with what the MVP discharged).

This is the honest record of what the TB-IR MVP actually is: which
parts of the design doc are implemented as specified, where the
implementation deliberately deviates and why, and which gates from the
plan doc are discharged versus still owed. Every claim below was
checked against the code at the cited location.

## What shipped

| PR | Content |
|---|---|
| #347 | MVP spine: IR types (`src/ir/mod.rs`), textual form (`src/ir/display.rs`), structural verifier (`src/ir/verify.rs`), AST → IR lowering (`src/ir/lower/`), loop-switch C++ backend (`src/codegen/tbir/`) behind `harc sim --codegen tbir`, and the `harc dump-ir` CLI. |
| #348 | v1-vs-tbir equivalence harness (`tests/run_tbir_equiv.sh` + `tests/tbir_equiv_fixtures.txt`), CI step, negative tests for out-of-subset constructs. |
| #349 | Covergroup lowering + emission (`CovgroupSchema`, `SamplerAuto` functions, `CovReport`/`CovBin`, auto-cross matrices). |
| #350 | Helper handling: pure helpers stay `Expr::Call` and emit as file-scope C++ functions; impure helpers (DUT access or sync) CFG-inline at every call site; recursion rejected with the cycle path. |
| #351 | `wait until` lowering + emission: `Single`/`AllOf` modes, optional `timeout N cycles [fail("...")]` with the v1 diagnostic block shape, plus the `wait_until_counter_test` fixture. |
| #364 | `transaction` declarations + non-randomize usage: `RecordSchema` records table on `TbProgram`, record-typed locals (`let t : TxnType` default construction with let-site re-init), field writes/reads (`RecordFieldWrite` / `Expr::RecordField`), value-record struct emission, plus the `transaction_basic_test` fixture. `randomize` stays rejected at statement level, pointing at the constraint-IR seam. |
| #366 | `transactor` declarations (unbound DUT-poking BFM subset): `TransactorSchema` table on `TbProgram`, one `TbFunction` per method (`kind: TransactorBody` — v1's `field_subs` substitution replaced by lowering-time DUT resolution), `Stmt::TransactorCall` carrying the never-inlined `Expr::Call(CallTarget::TransactorMethod ..)` edge, sync-wait method-lambda emission, ternary expressions (`Expr::Ternary`), plus 8 corpus fixtures registered. |
| #368 | `scoreboard` declarations (data-only host-state subset): `ScoreboardSchema` table on `TbProgram`, scalar-counter + `queue<T>` fields, `Stmt::ScoreboardOp` (`QueuePush`/`QueuePop`/`ScalarWrite`) and `Expr::ScoreboardQuery` (scalar read / `size()` / `empty()`), scoreboard-instance struct emission (`harc_rt::HarcQueue<T>` members) held on the `_tb` struct, plus the self-proving `scoreboard_basic_test` fixture. Scoreboard methods and event-driven `on`/`connect` wiring stay rejected (see the divergence note). `queue<Struct>`/`queue<transaction>` payloads on a data-only scoreboard now lower (#431 — see the queue-record slice below). |
| #431 | `queue<struct>`/`queue<transaction>` payloads on a data-only scoreboard. `scoreboard_field_kind` (`src/ir/lower/scoreboards.rs`) no longer caps queue elements at scalars ≤ 64 bits: it routes a `queue<T>` element through the shared composite-component helper `components::lower_queue_elem` (made `pub(crate)`), so a value-record element resolves against the program `record_ids` table to `QueueElem::Record(rid)` (mirroring v1's `HarcQueue<Struct>`), exactly as the env-nested data-scoreboard path already did. A `let s : Sample = sb.q.pop()` record-element pop is recognized as a queue pop before the record-typed-let block (`src/ir/lower/stmts.rs`), so it is not mis-rejected as a record-typed let with an initializer. Scalar (≤ 64-bit) elements are unchanged; an enum/Vec/nested/unknown element is still rejected precisely. No new IR variants — reuses the existing `QueueElem`/`ScoreboardOp` machinery. |
| env-composition slice (2026-06-13) | env/agent cluster's flat-struct core: `ComponentSchema` table on `TbProgram` (method-bearing scoreboards, analysis-source transactors, composing `env`s), `FunctionKind::ComponentMethod`, `Stmt::{ComponentFieldWrite, ComponentEmit, ComponentCall}`, `Expr::ComponentField`, `ConnectEdgeSchema`. `connect` (analysis-port → scoreboard sink), scoreboard methods (instance state materialized), and `out event`/`emit` lower; the `analysis_sink_connect_test` corpus fixture is registered. `agent`/`on`-handlers, `sequencer`/`tseq`, watchdog/phase, `idle`/`quiesced` predicates stay rejected (divergence 14). |
| agent/on-handler slice (2026-06-13) | `agent` composition + `on <ev>(arg)` event handlers (builds on the env-composition core, `src/ir/lower/components.rs`): `ComponentKindTag::Agent`, `ComponentSchema::on_handlers` (`OnHandlerSchema { event, arg_signed, function }`), `Stmt::ComponentEmit` extended with a `base: ComponentBase` (test-scope path-emit `emit tagger.in_ev(v)` + self-relative both lower), and `Expr::ComponentIdle { base, kind: IdleKind::{In,Out,Both}, n }` for the `idle`/`idle_in`/`idle_out` heartbeat predicates. An `on in_ev(t)` handler lowers as a one-param `ComponentMethod` and registers at component construction as a subscriber closure that bumps `_last_in_cycle` then runs the handler body (mirrors v1's `on`-subscriber registration); registration recurses into by-value sub-components. Directionless `event<scalar>` self-events join the existing `out event` analysis-port form on `ComponentFieldKind::Event`. Fixture: `agent_on_handler_test` (`top_counter.sv`, pass) — agent + on-handler + path-emit + `idle_in`, trace-diff clean v1↔tbir at seed 1. `event<Struct/transaction>` payloads, testbench-field-bound components, `quiesced(N)`, `watchdog`, `phase`, `on <N> cycles`/cycle-trigger handlers, `wait until` with heartbeat predicates, and `sequencer`/`tseq` stay rejected (divergence 15). |
| initiator-BFM slice (2026-06-13) | initiator-side bus-bound BFM: a `transactor X bound to <Bus>` whose `hookable` methods drive the bound bus's handshake channels (the regblock `via <Helper>` form). Lowered in `src/ir/lower/transactors.rs` (`lower_bound_initiator_transactor`) — methods become `TransactorBody` `TbFunction`s on `TransactorSchema::methods`, `bus` resolves through a placeholder-keyed binding filled at test-bind time (`fill_initiator_bus_prefix`), and `recv()` field access (`r.data`) is supported via per-field capture. No new IR variants. Fixture `regblock_access_test` registered (trace-diff clean). See divergence 15. |
| #372 | `randomize` via the constraint-IR seam: `Terminator::Randomize { target, constraints: ConstraintRef, succ }` + a `TbProgram::constraint_sites` table (the `ConstraintRef` handle resolves into it). Lowering merges transaction `keep`s ahead of the call-site `with {...}` body (spec §4) and records each site with its `ConstraintProblemId` handle. The tbir backend reuses v1's Z3-solve emission verbatim (`cpp_tb::emit_randomize_snippets` → `emit_constraint_solver_block` / unconstrained-PRNG shell / `emit_randomize_trace_event`) — "the constraint runtime is shared; only the call site moves to the IR backend." The runtime problem table + `harc_z3_rt.h` include are emitted iff a site exists. New passes wiring: `lower_coroutine` treats Randomize as a host-sync transition (`Trigger::Solved`); `placement` tiers a solve block at Tier-2 host-service and capability-checks `solve_*`. Fixtures: `keep_constraints_test` (bare `randomize(t)` + transaction keeps), `axilite_constraint_test` (`randomize(p) with` + Z3 cross-field constraints). Both trace-diff clean v1↔tbir at seed 1. `randomize` *expressions* (`let v = randomize(t)`), scoreboard-`.push`/`tseq`-gated randomize fixtures, and method-body randomize stay rejected (residual map below). |
| tb-component-field slice (2026-06-13) | composite-component **testbench-field binding** — a component bound as a `testbench` FIELD (`prod : Producer` / `sb : Sb` / `top : HeartbeatEnv` inside the `testbench` block, alongside `dut : Top`), the complement of the already-shipped test-scope `let env : <Env>` binding. NO new IR variants: the testbench-field walk in `lower/mod.rs` routes a component-typed field into the SAME `test_scope_components` collector a test-scope `let` uses, so it flows into `ComponentFieldBinding`/`component_fields` and lowers to a default-constructed run-scope instance identically. The impl-for desugaring prefixes a testbench-field access with `_tb` (`prod.in_ev` → `_tb.prod.in_ev`); a new `FuncBuilder::strip_tb_prefix` helper (`components.rs`) strips that prefix in every component-access path (`as_component_method_call`, `as_component_field_{target,read}`, `lower_emit`, `as_component_idle`) and `as_port_ref` skips a `_tb.<component>` root, so `emit`/`idle_in`/field reads/writes all resolve to the bare-name instance. `validate_testbench_component` now ACCEPTS a component-typed field (a `mode` keyword on one is rejected — it's a transactor concept). Hardening: `event<transaction/struct>` payloads now reject precisely at component-schema lowering (`event<TinyTxn>` parses the payload as `TypeArg::Expr`/`Named`, previously mis-lowered to a scalar callback and failed at C++ compile). Fixture `tb_field_agent_test` (`top_counter.sv`, pass) — the agent from `agent_on_handler_test` bound as a testbench field instead of a test-scope let; trace-diff clean v1↔tbir at seed 1. See divergence 16. `event<transaction>` payloads, `quiesced(N)`, `watchdog`, named `phase`, `on <N> cycles`, scoreboard-`queue` SUB-components in an env, and `wait until` with heartbeat predicates stay rejected (residual map below). |
| tseq slice (2026-06-13) | `tseq` (transaction-sequence) construct (`src/ir/lower/tseqs.rs`): a named generator of a sequence of transaction values, iterated with `for t in <TSeq>`. New IR (minimal): `IrType::RecordSeq(RecordId)`, `FunctionKind::Tseq { record }`, `CallTarget::Tseq(name)`, `Stmt::SeqPush`, `Expr::SeqLen`/`Expr::SeqIndex`. The generator lowers to a `[&]`-lambda returning `std::vector<Record>` (v1's `emit_tseq`); `yield t` → `SeqPush`; `randomize(t)` reuses the merged constraint-IR seam (#372 — the solver problem table already catalogs tseq randomize sites); `let txns = Gen(5)` → a `CallTarget::Tseq` edge typing the local `RecordSeq`; `for t in txns` → a counted loop copying `txns[i]` into the record loop variable each iteration (reusable — the sequence is materialized once). Fixtures: `tseq_basic_test` (self-proving — randomize + override + reusable double-iteration) and `axilite_fuzz_test` (corpus fuzz test, `--test AxiLiteFuzzTest` + helpers) — both trace-diff clean v1↔tbir at seed 1, both need Z3. The three sequencer corpus fixtures lower their tseq but stop at deeper blockers (transactor state field — divergence 10; agent-mode multi-DUT-handle); those stay rejected (divergence 17). |
| sequencer slice (2026-06-13) | `sequencer` construct (builds on the env-composition + agent/on-handler core, `src/ir/lower/components.rs`): a `sequencer` is the analysis-source component shape — an `out event<T>` analysis port plus `hookable` methods that generate a stimulus stream and `emit` each item on that port. It routes through the same `CompSource`/`ComponentSchema` machinery as a method-bearing scoreboard or analysis-source transactor; the only addition is `ComponentKindTag::Sequencer` (+ `CompSource::Sequencer`) for dump-ir/diagnostics. A `connect <sqr>.dispatched -> <drv>.<sink>` edge inside the composing env wires the emitted stream into a sink method (the UVM sequencer/driver bridge). No new IR variants, no new statement/expr forms — sequencer methods, `emit`, and `connect` all reuse the existing component lowering. Fixture: `sequencer_connect_test` (`top_counter.sv`/`top_counter.arch`, pass) — a `dispatch(n)` hookable emitting over a literal-range `for i in 0 .. n` loop, connected to a scoreboard sink; trace-diff clean v1↔tbir at seed 1. The three corpus sequencer fixtures (`axilite_connect_test`, `transactor_agent_mode_test`, `transactor_env_mode_test`) stay blocked: each iterates a `tseq` (`for t in <TSeq>` + `randomize`) inside its `dispatch`, and the agent/env fixtures additionally stack mode-inheritance + cycle-trigger `on dut.x && dut.y` handlers; those gate on the `tseq`/`randomize-in-tseq` + agent-mode slices (divergence 16). |
| event-record-payload slice (2026-06-13) | `event<transaction>` / `event<struct>` analysis ports — **non-scalar event channels carrying a value-record payload** (the heartbeat/quiesce cluster's blocker; #376 rejected these as a soundness measure). `ComponentFieldKind::Event` now carries an `EventPayload` (`Scalar { signed }` \| `Record(RecordId)`) instead of a bare `signed: bool`, and `OnHandlerSchema::arg_signed` becomes `arg_payload: EventPayload`. Field lowering (`lower_event_payload`, `src/ir/lower/components.rs`) resolves a payload that parses as `TypeArg::Type(Named)` or `TypeArg::Expr(Ident)` against the `record_ids` table — a known transaction/struct → `Record`, a scalar ≤ 64 bits → `Scalar`, anything else (enum/Vec/nested/`TypeArg::Named` keyword arg) is rejected precisely. An `on in_ev(t)` handler whose event is a record now binds a record-typed param (`IrType::Record(rid)`), so `t.field` reads in the body resolve against the schema; `emit prod.in_ev(t)` carries the record local. The tbir backend mirrors v1's `payload_type_for_arg`: the event field becomes `std::vector<std::function<void(<RecordName>)>>` (`runtime::event_payload_cty`) and the component-fn lambda renders a record param by value (`func.rs`); the `[&](auto _t)` subscriber/connect lambdas were already payload-generic. No new statement/expr forms — `emit`/`on`/`connect` reuse the existing component lowering. Fixtures unlocked (both `top_counter.sv`, pass, trace-diff clean v1↔tbir at seed 1): `heartbeat_idle_test` (agent + record-payload event + `on` handler + `idle_in` poll) and `wait_until_quiesce_test` (same + `wait until all of … timeout`, already supported by the agent slice). `watchdog_quiesce_test` (a `watchdog` directive) and `env_quiesced_phase_test` (a data-only `scoreboard` SUB-component in an env + `phase` + `quiesced(N)`) stay rejected — separate slices. See divergence 17. |
| watchdog + periodic slice (2026-06-13) | `watchdog ... end watchdog` lifecycle directive (spec §8.6) + `on <N> cycles` periodic handlers (spec §7.10) in the agent/component subset (`src/ir/lower/components.rs`). New IR: `ComponentSchema::watchdog: Option<WatchdogSchema>` (`period`/`max_idle` clause exprs + a zero-arg body `function`), `ComponentSchema::periodic_handlers: Vec<PeriodicHandlerSchema>` (`period` expr + body `function`), and `Expr::CycleCount` (a bare `cycle_count` ident — the framework cycle counter, conventionally referenced from `${cycle_count}` in a watchdog/log diagnostic). Lowering: a `watchdog` lowers to a zero-arg `ComponentMethod` body (the user heartbeat statements; field reads self-relative); `period`/`max_idle` lower in the same self-component context (a field-backed clause reads `self.<field>`). A `disabled` watchdog emits nothing (no FunctionId, no schema entry — mirrors v1's `emit_watchdog` early return). An `on <N> cycles` handler lowers to a zero-arg `ComponentMethod` body + its period expr; pass-1 reserves FunctionIds methods→event-on-handlers→periodic→watchdog so the table stays monotonic. The tbir backend installs one per-instance `_checkers` closure per watchdog/periodic (recursing into sub-components), gated on a uniquely-named `static` last-fire stamp; the watchdog closure runs the body then the idle check (`_last_in_cycle`/`_last_out_cycle` ≥ `max_idle` ⇒ `FAIL` + `errors++`). The period/max_idle exprs render against the instance path via `ECx::self_subst` (`SelfField` → instance, since the closure has no `self`). Fixtures (all `top_counter.sv`, trace-diff clean v1↔tbir at seed 1): `watchdog_quiesce_test` (agent + watchdog over a record-payload event, never trips — pass), `watchdog_trip_diagnostic_test` (silent agent, watchdog trips every firing from cycle 200 — `fail`, 9 identical FAIL lines), `agent_periodic_test` (self-proving `on 10 cycles` firing 3× in 35 cycles — pass). `quiesced(N)` env aggregation and named `phase` orchestration stay rejected (`env_quiesced_phase_test` still blocked). See divergence 18. |
| env quiesced + phase + data-scoreboard-sub slice (2026-06-13) | the heartbeat/quiesce cluster's last three blockers (`src/ir/lower/{components,stmts,mod}.rs`, divergence 19): (1) **data-only `scoreboard` as an env SUB-component** — new `ComponentFieldKind::ScoreboardSub { scoreboard: ScoreboardId }` (field lowering resolves a `Named` type against `scoreboard_ids` before the component table), accessed by the nested run-scope path via a new `nested_path: Option<Vec<String>>` on `Stmt::ScoreboardOp` / `Expr::ScoreboardQuery` (`None` = the established `_tb.<field>` form, `Some(path)` = the dotted env-nested path `top.sb`); the scoreboard struct now always emits `_last_in/out_cycle` (matching v1's unconditional `emit_scoreboard` — a latent tbir omission this slice also closes); (2) **`<env>.quiesced(N)`** — no new IR variant: `as_component_quiesced` + `collect_quiesce_leaves` walk the receiver's sub-component tree (mirroring v1's `collect_quiesced_paths`) and expand to an AND of `Expr::ComponentIdle { kind: Both }` over every leaf (`top.quiesced(8)` → `top.prod.idle(8) && top.sb.idle(8)`); (3) **named `phase <name>`** — no new IR variant: each phase block is collected into a name→block map and a `<name>()` call site in the run/check body is INLINED with the phase's statements (recursively; a cycle is rejected), matching v1's captured-lambda-plus-call (the body runs at the call site inside the coroutine). Fixture `env_quiesced_phase_test` (`top_counter.sv`, pass) exercises all three; trace-diff clean v1↔tbir at seed 1. `event<enum/Vec/nested>` payloads, `on <expr>` cycle-trigger handlers, and `tseq` stay rejected. |
| transactor-state-field slice (2026-06-13) | persistent **scalar STATE fields on the unbound DUT-poking transactor** (`transactor SeqXactor { dut : DUT; last_read : uint<32> default 0; ... }`) — divergence 10. The bound-to TARGET responder form already materialized state fields (#371); this extends the SAME machinery (`TbScalarFieldSchema`, `Expr::TransactorState` / `Stmt::TransactorStateWrite`, `target_state_struct_inst`) to the unbound BFM. No new IR variants. `lower/transactors.rs` (unbound path) now classifies a `Builtin`-typed field as scalar state (vs the single `Named` module-typed DUT handle) via the shared `lower_state_field`, and sets each method builder's `target_state_fields` so bare-name reads/writes (`last_read = dut.x`; `n = n + 1`) lower to `TransactorState`/`TransactorStateWrite` with an empty instance placeholder. At test-binding (`lower/mod.rs`) each stateful unbound instance registers in the `target_state` map (so the test reads `xact.last_read`) and fills its name into the type-shared method bodies; `as_transactor_state` (`lower/exprs.rs`) now also resolves the impl-for-desugared `_tb.<instance>.<field>` shape. Emission (`codegen/tbir/mod.rs`) declares one per-instance state struct (`runtime::target_state_struct_inst`) BEFORE the method lambdas, so the `[&]`-captured lambdas and the run/check coroutine share it. New `TestbenchSchema::unbound_state_actors` records which instances need a struct. Subset: ONE stateful instance per transactor type per file (the bodies are type-shared) — a second is rejected precisely (`stateful_type_seen` guard, independent of whether the type has methods). Fixture: `transactor_state_field_test` (self-proving, `cam_dual_basic.sv`, pass) — a CAM BFM whose `do_probe(key)` stashes `dut.search_first`/`dut.search_any` into state and bumps a `probe_count` accumulator, read back at test scope; trace-diff clean v1↔tbir at seed 1. The three sequencer/mode corpus fixtures advance past the state-field blocker but stop at their NEXT tier (residual map below): `axilite_seqdrv_test`/`axilite_connect_test` on the `req : in event<RegOp>` directional field (event-driven transactor); `transactor_active_test` on the bound-to event field; `axilite_hooks_test` on a record-typed (transaction) method param + pre/post hooks; `transactor_agent_mode_test`/`transactor_env_mode_test` on the multi-module-field (`dut` + `sb`) agent-mode form. |
| bound-initiator-state slice (2026-06-13) | persistent **scalar STATE fields on the bound-to INITIATOR BFM transactor** (`transactor AxilHelper bound to BusAxiLite { last_read : uint<32> default 0; read_count : uint<32> default 0; hookable read(addr) -> uint<32> { ... last_read = r.data; ... } }`) — divergence 10. PR #375 added the bound-to initiator BFM but rejected any state field; PR #381 added state for the UNBOUND form and #371 for the bound-to TARGET responder. This extends the SAME machinery (`TbScalarFieldSchema`, `Expr::TransactorState` / `Stmt::TransactorStateWrite`, `runtime::target_state_struct_inst`, `TestbenchSchema::unbound_state_actors`) to the bound-to initiator. No new IR variants. `lower/transactors.rs` (`lower_bound_initiator_transactor`) now classifies a `Builtin`-typed field as scalar state via the shared `lower_state_field` (rejecting module/transaction/non-scalar types) and sets each method builder's `target_state_fields` so bare-name reads/writes lower to `TransactorState`/`TransactorStateWrite` with an empty instance placeholder; an event/directional field (`req : in event<T>`) is rejected precisely (the bound-to event-driven driver form — divergence 17). At test-binding (`lower/mod.rs`) the stateful-instance loop's skip condition narrows from `bound_bus.is_some() || state_fields.is_empty()` to just `state_fields.is_empty()` — the only `bound_bus.is_some()` entries in `transactor_fields` are bound-INITIATOR BFMs (bound-TARGETs live in `target_tlm_actors`), so they now register in `target_state` (test reads `helper.last_read`), fill the type-shared method bodies' state placeholders, and join `unbound_state_actors`, coexisting with the earlier bus-prefix fill (same instance, same shared bodies). Subset: ONE stateful instance per transactor type per file (`stateful_type_seen` guard). Fixture: `transactor_bound_initiator_state_test` (self-proving, `AxiLiteRegs.sv`, pass) — an AXI-Lite BFM whose `read(addr)` caches the bus readback into `last_read` and bumps `read_count`, read back at test scope; trace-diff clean v1↔tbir at seed 1. **Still rejected**: the bound-to EVENT-driven form (`transactor_active_test`, `transactor_parse_test` — `in event<T>` + `on <ev>` driving the bound bus, plus `on bus.<ch>.handshake` monitor handlers and `sb` sub-component fields) and record-typed method params + pre/post hooks (`axilite_hooks_test`) — each a deeper slice (residual map below). |
| nonscalar-target-state slice (#494 P0a) | persistent **NON-SCALAR STATE fields on the bound-to TARGET responder** (and, by the shared code path, the unbound / bound-to-initiator forms) — a `queue<scalar ≤ 64 bits>` and a `queue<Record>` (`transactor MemoryResponder bound to TlmMemBus { pending : queue<Beat>; log_addrs : queue<uint<8>>; thread bus.read(addr) { ... pending.push(b); let out : Beat = pending.pop(); return out.data } }`). Before this slice `lower_state_field` (`src/ir/lower/transactors.rs`) rejected any non-scalar state ("state field ... with a non-scalar type ... re-run with --codegen v1"); state had to be scalar `uint<N>`/`sint<N>`/`bool` (≤ 64 bits). This REUSES the scoreboard/component queue machinery verbatim: `StateFieldSchema { name, kind: StateFieldKind::{ Scalar { ty, default } \| Queue { elem: QueueElem } } }` replaces the scalar-only `TbScalarFieldSchema` on `TransactorSchema::state_fields`, with `QueueElem`/`RecordId` shared from the scoreboard seam (`lower_queue_elem`). New IR ops (NOT overloading scalar `TransactorState`): `Stmt::TransactorStateQueuePush`/`TransactorStateQueuePop` and `Expr::TransactorStateQueueQuery` (`.size()`/`.empty()`), all carrying the empty-instance placeholder filled at test-bind exactly like `TransactorState`. The builder's `target_state_fields` becomes a `HashMap<String, StateFieldKind>` so bare-name `pending.push(x)` / `pending.pop()` / `pending.size()` inside a responder body route to the queue ops (a bare read/assign of a queue field is rejected precisely); the test-scope `responder.pending.size()` / `.pop()` resolve through `as_transactor_state_any` + the enriched `ctx.target_state` (`instance → { field → StateFieldKind }`). C++ emission (`runtime::target_state_struct_inst`) emits `harc_rt::HarcQueue<Beat>` / `HarcQueue<uint64_t>` members and the same `.push`/`.pop`/`.size`/`.empty` calls scoreboards use. Verifier/placement/display extended for the new nodes; scalar path unchanged. Fixture: `target_nonscalar_state_test` (self-proving, `TlmReadInitiator.sv`, pass) — a target responder that queues a `Beat` record + a scalar addr per served read, checks depth, pops the record and returns its `data`, and the test inspects the scalar log queue at test scope; trace-diff clean v1↔tbir at seed 1. **Residual (follow-up)**: (1) record-TYPED (whole-struct) state fields — `state : SomeRecord` on a target transactor — are not in this slice (queues of records ARE); (2) MULTIPLE instances of the same target type carrying non-scalar state — the type-shared responder bodies fill a single instance name into the `TransactorState*` placeholders (the existing one-stateful-instance-per-type gate applies to the queue forms too), so per-instance non-scalar state for multiple same-type instances needs the state-receiver refactor and stays rejected. |
| event-driven-transactor slice (2026-06-13) | **consumer side of the analysis-event machinery** — an unbound `transactor` with an `in event<T>` input pipe driven by an `on req(t)` handler (the UVM driver's `req` sink). `transactor_is_component` now routes such a transactor through the **composite-component** table (it already supports `event` fields, `on` handlers, `emit`, and `connect`) rather than the DUT-poking `TransactorSchema` — even when the transactor carries a module-typed DUT handle field. New IR: `ComponentFieldKind::Dut { dut_type }` (the `V<dut_type>* dut = nullptr;` handle the handler pokes), and `ConnectEdgeSchema::sink` becomes a `ConnectSink { Method { method } \| Event { event } }` enum so a `connect` edge can feed an `in event` pipe (event→event bridge) as well as a hookable sink. Field lowering (`lower/components.rs`) accepts `in event<T>` ON A TRANSACTOR (still rejected elsewhere) and a non-component `Named` type as the single DUT handle (named `dut`); the `on` handler body reuses the component-method lowering, so its `wait N cycles` lower to `WaitCycles(n, None, …)` → v1's synchronous `for(_w) tick();` (the handler is a sync subscriber, not a coroutine actor) and `dut.<sig> = x` lower to `DutWrite` via the shared `ctx.dut_field = "dut"`. `emit drv.req(t)` (test scope) → `ComponentEmit` fan-out over the pipe's subscriber list; `connect <sqr>.dispatched -> <drv>.req` → the source event's bridge closure iterates the sink event's subscribers (`for(auto& _s : drv.req) _s(_t);`). The DUT bind (`drv.dut = dut`) is erased like the `TransactorSchema` bind — the handler's `DutWrite`s already target the test `dut` pointer. An `active`/`passive` mode IS accepted on these (a transactor concept): `active` required, `passive` rejected. Fixtures: `event_driven_transactor_test` (self-proving, `top_counter.sv`/`top_counter.arch`, pass) — direct `emit drv.req(n)` + a sequencer→transactor `connect` event bridge, both pulsing `en` and latching `count_out`; `axilite_seqdrv_test` (corpus, `AxiLiteRegs.sv`, pass) — sequencer→transactor full AXI-Lite write/read round-trip via direct emit. Both trace-diff clean v1↔tbir at seed 1. **Still rejected** (residual map): `transactor_active_test` (bound-to `transactor X bound to <Bus>` event-driven form — needs the coroutine-actor + bus-binding driver, a separate slice) and `axilite_connect_test` (its env holds a data-only `scoreboard` SUB-component with through-env `queue` access — the env-field-binding/data-scoreboard-sub slice, not the event-driven-transactor surface). |

| bus-bind-remap slice (2026-06-13) | **`bind ... with { ch.sig: "port", ... }` bus signal remaps** (`src/ir/lower/bus.rs`, `src/codegen/tbir/func.rs`, `src/ir/{mod,display}.rs`). Mirrors v1's `bind_remap → bus_remap → bus_signal_name`: `BusBindingSchema` gains a `remap: Vec<((channel, signal), port)>` (sorted by key for deterministic dump-ir) plus a `wire_name(channel, signal)` resolver that returns the override if present else the `<field>_<channel>_<signal>` convention. `lower_bus_binding` no longer rejects `bind_remap` — it validates each path is exactly `<channel>.<signal>` (2 segments, malformed is a hard `Invalid` error) and records it. The two `wire` closures in `emit_transactor_call` (initiator call edge) and `emit_target_actor` (target responder, which now also receives `&tb.bus_bindings`) route through `wire_name`, so both TLM directions honor the override. For a `tlm_method` the channel is the method name and the signal is a protocol wire (`req_valid`/`addr`/`rsp_data`/...). No new IR variants, no statement/expr forms. Fixtures (all trace-diff clean v1↔tbir at seed 1): `tlm_bind_remap_test` (self-proving — binds with name `m` so the convention would drive nonexistent `m_read_*`; every entry remaps to the real `mem_read_*`/`mem_poke_*` port, proving the table is load-bearing), `dma_engine_tlm_target_test` + `dma_engine_tlm_mem_model_test` (corpus — blocking target responders that were rejected only for the explicit `bind ... with`). **Still rejected** (residual map): `fork`/`join_all` TLM issue (`tlm_method_bus_test`, `tlm_target_fork_forwarding_test`, `tlm_pairing_arch_target_test`), `out_of_order tags N` target lanes (`tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test`), and nested responder forwarding (`tlm_target_forwarding_test`) — each a distinct deeper slice. |
| initiator-side fork/join_all TLM slice (2026-06-13) | **`let x = fork bus.<method>(args)` + `join_all` over bus `tlm_method`s — initiator-side concurrent issue** (`src/ir/lower/{bus,stmts,mod}.rs`, `src/ir/{mod,display,verify,passes/placement}.rs`, `src/codegen/tbir/{mod,func}.rs`). Mirrors v1's `try_emit_bus_tlm_fork` / `emit_tlm_join_all`: a `fork` issues ONLY the request side (drive arg wires + optional `req_tag`, raise `req_valid`, 16-cycle budget-wait `req_ready`, tick, drop `req_valid`); the response is captured at the next `join_all`. New IR: `Stmt::TlmFork(TlmForkDesc)` (request issue) + `Stmt::TlmJoinAll(Vec<TlmForkDesc>)` (drain), where `TlmForkDesc` is self-contained (`bus_field`, `method`, `args`, `dest`, `has_ret`, `tag`) so the join statement carries its own descriptors — no cross-statement lowering replay in the backend. Tag allocation is per-`(bus_field, method)` monotonic on the builder (v1's `next_tlm_fork_tag`): a `blocking` method gets `tag: None` (issue-order FIFO drain — `emit_ordered_tlm_join_all`: per fork, raise `rsp_ready`, 64-cycle budget-wait `rsp_valid`, capture `rsp_data`, tick, drop); an `out_of_order tags N` method gets `tag: Some(n)` (tag-routed drain — `emit_tagged_tlm_join_all`: poll every lane per tick, accept any `rsp_tag`-matching not-yet-seen fork, 256-cycle budget — so tag 1 can land before tag 0). The pending-fork accumulator survives `WaitCycles` between blocks (it is builder-state, not block-state), so a fork in one block + `wait` + a second fork + `join_all` in the next block lowers correctly. `lower_function::finish` rejects a dangling `fork` with no matching `join_all`; `lower_tlm_join_all` rejects a mixed tagged/untagged barrier (the two routing strategies can't share a join). The verifier routes both statements through `check_bus_call_edge` (Run/Check only, binding resolves on the owner tb, method exists, arg arity + no-inline-port purity); the def/use pass defines `dest` at the fork site (v1's `T x = {};` zero-init), placement classifies both as the TLM seam (`has_transactor_call` → TimingTolerant, Tier-1). Fixture: `tlm_method_bus_test` (`TlmMemory.sv`, pass) — blocking `read`/`poke` + two `out_of_order` `read_ooo` forks joined together; trace-diff clean v1↔tbir at seed 1. **Still rejected** (residual map): (a) a `fork` INSIDE a transactor responder body (target re-issuing a downstream TLM call — fork-forwarding: `tlm_target_fork_forwarding_test`, `tlm_target_forwarding_test`) — needs the responder to be both target+initiator with a request arbiter/response router; (b) target-side `out_of_order tags N` RESPONDER lanes (`tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test`) — hidden tag wires + multi-lane response router; (c) `tlm_pairing_arch_target_test` (ARCH DUT) LOWERS and is v1↔tbir trace-diff clean, but its auto-emitted `_auto_tlm_*_req_stable` TLM SVA `$fatal`s under **local Verilator 5.048** for both codegens identically — a known local-only artifact, NOT registered. |
| TLM responder nested-forwarding slice (2026-06-13) | **a bound-to TARGET responder re-issuing a downstream TLM call inside its body — nested forwarding** (`src/ir/lower/{mod,transactors}.rs`, `src/ir/verify.rs`, `src/codegen/tbir/{func,expr}.rs`). The deferred residual (a) from the initiator-side fork/join_all slice. A responder thread `thread bus.read(addr) ... let raw = back.read(addr); return raw + ...` forwards each front request through a SECOND test-scope bus binding (`back`) to a downstream target. The responder is lowered before any test, so `back`'s bus type is not in scope at responder-lowering time. A file-level **pre-scan** of every (desugared) test's `let <name> : <Bus> = bind ...` declarations builds a `name → BusDecl` map (first binding wins on collision — the responder body is type-shared, so its downstream bus type must be unambiguous) that is handed to the bound-target responder body's `bus_bindings` ctx. The downstream call then lowers through the EXISTING #390 machinery — `try_lower_bus_call` (blocking → `CallTarget::TransactorMethod` Assign-RHS edge) or `try_lower_tlm_fork` (`out_of_order tags N` downstream → `Stmt::TlmFork`/`TlmJoinAll`) — composed inside the responder coroutine loop-switch. **No new IR variants.** The verifier's `check_bus_call_edge` now permits a bus-call edge in an owner-less `TransactorBody` (resolution defers to emit, which has the test's `bus_bindings`; an unresolved downstream binding surfaces as an EmitError). The tbir backend passes the test's `&tb.bus_bindings` (not `&[]`) into the responder loop-switch `emit_stmt`, and a new `ECx::trace_component` field tags the downstream `tlm_call` trace event with the responder-instance name (mirroring v1's `current_component_instance`), so the semantic trace diffs clean. Fixtures (both pass, trace-diff clean v1↔tbir at seed 1): `tlm_target_forwarding_test` (blocking downstream `back.read` — 3 SV files: `TlmForwardingTop` + `TlmMemory` + `TlmReadInitiatorPair`), `tlm_target_fork_forwarding_test` (two `fork back.read_ooo(...)` + `join_all` over an OOO downstream bus). **Still rejected** (residual map): the OOO-RESPONDER LANE form — a responder SERVING an `out_of_order tags N` method (hidden tag wires + per-tag dispatcher/lane/arbiter coroutines): `tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test` ("serving a `out_of_order` method"); and `tlm_pairing_arch_target_test` (LOWERS + trace-diff clean, but its ARCH-DUT auto-emitted TLM SVA `$fatal`s under local Verilator 5.048 for both codegens — known local-only artifact, NOT registered). |
| agent-mode + cycle-trigger slice (2026-06-13) | **agent-mode multi-DUT-handle transactor + cycle-trigger `on <expr>` monitor handlers + agent/env `connect` bridges** (`src/ir/lower/{components,stmts,mod}.rs`, `src/codegen/tbir/{mod,func,expr}.rs`). Three pieces, each unlocking the `transactor_agent_mode_test`/`transactor_env_mode_test` corpus fixtures: (1) **cycle-trigger handlers** — new `ComponentSchema::cycle_handlers: Vec<CycleTriggerHandlerSchema>` (`trigger` predicate expr + `CycleEdge { Rising \| Falling \| Level }` + zero-arg body `function`). An `on <bool-expr> ... end on` handler (distinguished from `on <ev>(arg)` by `is_event_subscription` — a `Call` whose callee names a self `event` field is a subscription; anything else is a cycle-trigger) lowers to a zero-arg `ComponentMethod` body + its trigger predicate (lowered self-relatively, so `dut.<sig>` reads route to the DUT handle and bare field reads resolve self-relative). Pass-1 reserves FunctionIds methods→event-on-handlers→periodic→cycle-trigger→watchdog (monotonic). The tbir backend installs one per-instance `_checkers` closure per cycle-handler (recursing into sub-components), gated on a uniquely-named `static` prev-state for edge detection (mirrors v1's `emit_cycle_trigger`). It is the always-on OBSERVER half — present on BOTH active and passive instances. (2) **self-relative sub-scoreboard poke** — `sb.writes = sb.writes + 1` inside a transactor body, where `sb` is a `ScoreboardSub` field of the self component: `scoreboard_root` (`stmts.rs`) now resolves a `ScoreboardSub` receiver of `self_component` to a `self`-rooted `nested_path`, and the tbir `ScoreboardOp`/`ScoreboardQuery` emission re-roots `self` at the running instance via `self_subst`. The component method ctx (`mod.rs`) now carries the `scoreboards` table so the scalar-field validation succeeds. (3) **agent + nested-env `connect` bridges** — `connect` edges are now resolved for `Agent` decls (not only `Env`), and the tbir backend recurses through `Sub` fields to install a nested sub-component's bridges (`emit_nested_connects`), so an env→agent→drv `sequencer.dispatched -> drv.req` bridge wires at `<env>.<agent>` scope. Plus: a sequencer's `hookable dispatch(txns: TSeq<Record>)` param now types as `RecordSeq` (`method_param_ir_type` resolves the `TSeq<RegOp>` element against `record_ids`, handling both `TypeArg::Type(Named)` and the bare-ident `TypeArg::Expr` parse), so `for t in txns` iterates it and the C++ param renders `std::vector<Record>`. The `active`/`passive` mode on a composite-component test-scope `let` (`let act : AxilAgent active`) is accepted (and ignored, matching v1 — the fixtures' passive correctness comes from the test never dispatching the passive sequencer, not from hard `when active` elision). Fixtures: `transactor_agent_mode_test` + `transactor_env_mode_test` (`AxiLiteRegs.sv`, pass) — same agent/env decl reused active + passive, active drives 5 AXI round-trips via sequencer→connect→`on req(t)`, both transactors' cycle-trigger observers tally 5 writes + 5 reads off the shared DUT; trace-diff clean v1↔tbir at seed 1. **Divergence**: hard `when active` body elision on a passive instance is NOT implemented (v1 doesn't elide for these fixtures either; the body is structurally present but never stimulated on passive). A `post_eval`-phased or hooked cycle-trigger is rejected precisely (checker phase only). |
| bound-to monitor slice (2026-06-13) | **bound-to event-driven transactor's passive MONITOR surface — `on bus.<ch>.handshake(arg)` observers + `sb` ScoreboardSub feed + a `passive` bound instance** (`src/ir/lower/{components,mod}.rs`, `src/ir/{mod,display}.rs`). The deferred passive half of the bound-to-agent cluster (PR #391 landed the DRIVER half). An `on bus.<ch>.handshake(arg)` handler on a `bound to <Bus>` transactor desugars into a `CycleTriggerHandlerSchema` with a new `monitor_channel: Some(<ch>)` flag: the synthesized trigger is the channel's `<ch>.valid && <ch>.ready` (rising edge), and the body preamble captures the channel payload into the handler's `arg` local — first payload signal aliases `arg` (scalar `sb.q.push(arg)`), every signal also a per-field alias in `recv_payloads` (so `beat.data`/`beat.resp` resolve, like a `recv()` capture) — then the user body feeds the sub-scoreboard. `lower_monitor_handshake_body` reads the channel payload from the bound `BusDecl` (placeholder-prefixed in the per-component body ctx). **No new actor IR / no new tbir codegen** — reuses the agent-mode cycle-trigger `_checkers` machinery; the monitor triggers live on the schema, so a new `fill_initiator_bus_prefix_expr` (sharing the `fill_visit_*` walkers factored out of `fill_initiator_bus_prefix`) fills their placeholder bus prefix. A `passive` bound instance is now accepted when the transactor declares monitor handlers (pure-driver-no-monitor passive still rejected — inert); both modes register the always-on monitors, `active` adds the `on req` driver. **Divergence (resolved by #435, 2026-06-19)**: v1 emits a per-channel coroutine ACTOR (`emit_bound_monitor_actors`) whose `wait_until(valid && ready)` + `wait_cycles(1)` loop samples a continuously-held handshake every OTHER cycle. The IR originally used a plain rising-edge cycle-trigger — observably equivalent only for single-beat valid/ready handshakes (they diverged for a multi-cycle held handshake). #435 closes this: `emit_lifecycle_checkers` now emits a fire-then-cooldown latch for a `monitor_channel` handler (fire when the predicate holds, then consume the next cycle as the `wait_cycles(1)` re-arm), reproducing v1's every-other-cycle cadence exactly. Verified by the multi-cycle held-handshake fixture `stream_burst_mon_test` (`stream_burst.sv`, trace-diff clean v1↔tbir). Fixtures: `transactor_passive_only_test` (pure monitor), `axilite_bound_mon_test` (active driver + passive monitor concurrent), `axilite_multi_payload_test` (multi-payload `beat.data`/`beat.resp`) — all `AxiLiteRegs.sv`, pass, trace-diff clean v1↔tbir at seed 1. Closes the bound-to-agent cluster. |
| probe/force slice (2026-06-13) | **DUT-internal signal access via declared probes (read) and force points (write)** (`src/ir/{mod,lower/{mod,stmts,exprs},passes/placement,verify,display}.rs`, `src/codegen/tbir/{mod,runtime,func,expr}.rs`). Makes the long-reserved `PortAccess::Probe`/`Force` real (was always `Port`, divergence 1). A `probe <name> : <T> at <path>` / `probe force <name> : <T> at <path>` on `let dut` (classic OR testbench-owned, the impl-for desugar preserves probes) is collected into `LowerCtx::probes` (name → `{force, width}`); a single-segment `dut.<name>` whose name is a probe lowers `as_port_ref` to a `PortRef` with `access = Probe`/`Force` + the declared scalar width. New IR: `Stmt::ProbeRelease(PortRef)` for `release dut.<probe>`. Lowering enforces the access discipline: writing a read-only probe, or `release`-ing a non-force probe / ordinary port, is a precise hard error. The tbir backend mirrors v1's `Emitter::probes`: a `Probe`/`Force` read routes through `dut->rootp-><DutType>__DOT__harc_probes__DOT__<name>` (the SV bind-stub accessor, `expr::port_signal`/`probe_read_accessor`); a `Force` write emits the `_drv = expr; _en = 1;` pair (`func.rs`); `ProbeRelease` emits `_en = 0;`. The preamble pulls in `V<DutType>___024root.h` when any probe is used (`program_has_probes`, gated like v1's `aggregated_probes`). The SV stub (`__harc_probe_<DutType>.sv`) is emitted by the shared `emit_probe_stub_if_needed` path — identical for both codegens. Fixtures (all `cpu_pipeline.sv`, pass, trace-diff clean v1↔tbir at seed 1): `probe_basic_test` (3 read-only probes hoisting `alu0.{a,b,result}`), `probe_force_test` (read probe + `probe force` write/release fault-injection), `testbench_probe_dut_test` (testbench-OWNED probed DUT + `function reset()`, regression for the impl-for desugar). **Subset / divergence**: probe types must be scalar (`uint<N>`/`sint<N>`/`bits<N>`/`bit`/`bool` — the SV stub only surfaces scalar logic); an aggregate probe type is rejected precisely. Multi-segment probe paths (`at alu0.a`) are stored verbatim in the stub `at`-target and validated by Verilator, NOT harc (mirrors v1; docs/probe-signals.md §4.4). Probing inside an ARCH-compiled DUT (`--dut`) is out of scope — these fixtures self-skip the arch-DUT sweep (`-` in the registry, no `.arch` sibling). |
| regblock passive record API + struct `Vec` fields (2026-06-13) | **(A) the passive `record_write`/`record_read` RAL API (constant-address decode), and (B) a `Vec<T, N>` record/struct field** (`src/ir/lower/{records,regblock,bus,transactors,stmts,exprs,mod}.rs`, `src/codegen/tbir/{mod,func,expr}.rs`, `src/ir/{mod,verify,display,passes/placement}.rs`). **(A)** `regs.record_write(addr, data)` decodes a compile-time-constant `addr` to its register at lowering time and emits a masked mirror `RecordFieldWrite` (no bus traffic — the monitor already saw the write); `let v = regs.record_read(addr)` emits a mirror `RecordField` read. No new IR — reuses the regblock mirror record. A non-constant address (the v1 runtime decode chain) and an unmatched address are rejected precisely. The per-register `on regs.REG` write callback is STILL rejected: it lowers (in v1) to a `[&]`-capturing closure over run-scope state (mirror cell, callbacks holder, `cb_depth`) fired from inside `record_write`, which the function-per-CFG IR cannot express (the SAME blocker as the `axilite_hooks` pre/post method hooks). So the callback-bearing corpus fixtures (`regblock_record_test`, `regblock_record_recursion_test`) stay residual; the API on its own is proven by the self-authored `regblock_record_api_test`. **(B)** `RecordFieldSchema` gains `vec_len: Option<usize>` — a `Vec<scalar, N>` field lowers to a `std::array<T, N>` member (v1's `record_field_c_type` Vec branch); element access `rec.data[i]` lowers to an indexed `Expr::RecordField` / `Stmt::RecordFieldWrite` (both gain `index: Option<…>`). `record_struct` now emits v1's `harc_pack_<R>` / `harc_unpack_<R>` / `harc_drive_<R>` helpers for EVERY record with a defined packed width (matching v1's unconditional `emit_record_pack_helpers`); a record-returning `tlm_method` captures via `harc_unpack_<R>` (initiator) and the responder drives the response pin via `harc_drive_<R>` (target) — the dest/ret record type is carried on the local (`bus.rs` sets the `let` dest type, `transactors.rs` the responder ret slot). Fixtures (pass, trace-diff clean v1↔tbir at seed 1): `regblock_record_api_test` (`AxiLiteRegs.sv`), `tlm_pairing_arch_burst_target_test` (HARC initiator unpacks the ARCH struct response, `TlmPairingArchBurstTarget.sv`), `tlm_pairing_arch_burst_initiator_test` (HARC target builds + packs the struct response, `TlmPairingArchBurstInitiator.sv`). **Still rejected** (residual map): the per-register `on regs.REG` write callback (closure-capture, as deep as `axilite_hooks`); a non-constant `record_*` address; a widthless / nested / list `Vec` element type; a whole-`Vec` field read/write (only element access lowers). |
| record-typed method params + value-returning method call in expression position (2026-06-13) | **(a) a transactor method param typed as a declared `transaction`/`struct` record (`hookable run_for(cmd: RunCmd)`) — passed BY VALUE; (b) a value-returning transactor method call in EXPRESSION position (`(helper.read(0) & 1) == 1`, `assert helper.read(24) == ...`)** (`src/ir/lower/{transactors,exprs,stmts,mod}.rs`, `src/codegen/tbir/func.rs`). **(a)** `method_param_ir_type` (in `transactors.rs`) resolves a `Named` param type that names a record (via the method ctx's `record_ids`) to `IrType::Record(rid)` — applied to both the unbound and bound-to method-lowering loops — instead of the prior scalar-only `check_scalar_ty` + `ir_type_of`. The param local is record-typed (default-constructed at entry like any record local), and the body reads its fields via the existing `Expr::RecordField` machinery (#364). The tbir `emit_method` renders a record/recordseq param as the by-value struct (`<Record> n` / `std::vector<Record> n`), mirroring `emit_component_method`'s already-record-aware signature. No new IR variants — reuses `RecordSchema`/`IrType::Record`. **(b)** A value-bearing `CallTarget::TransactorMethod` edge in expression position is now hoisted into its own `Stmt::TransactorCall { dest: Some(temp), .. }` and the result temp substituted — the seam rule's sanctioned home (the edge never stays nested), and the call's internal `tick()` runs at the hoist point in SOURCE ORDER. The hoist lives in `hoist_ports` (`Expr::Call` arm → `hoist_transactor_edge`) for the `lower_expr_no_ports` two-phase path (correct interleaving with DUT-port reads), and in a sibling `hoist_transactor_calls` walk invoked by `lower_assert` (where DUT ports stay inline/lazy but a transactor edge still cannot stay nested). `lower_expr`'s Call arm now builds the edge (`lower_transactor_call(.., need_ret=true)` — a void method used as a value is rejected, mirroring v1's C++ type error) instead of rejecting; the `in_fmt_args` rejection (call inside a lazily-evaluated log/fail message) is preserved. Fixtures (pass, trace-diff clean v1↔tbir at seed 1): `transactor_record_param_test` (self-proving record-by-value param, `top_counter.sv`), `axilite_regs_full_test` (`helper.read(addr)` in assert/`&`/`>>` expression positions across 6 sub-tests, `AxiLiteRegs.sv`). **Still rejected** (residual map): test-scope `on <obj>.<method> pre/post` METHOD HOOKS (`axilite_hooks_test`) — the hook body is a `[&]`-capturing closure that mutates test-scope `let`s by reference, which the function-per-CFG IR cannot express as a closure lexically nested in the run coroutine; a >64-bit method param (`aes_cipher_top_test` `load_block(key: uint<128>)`) — **now shipped** by the wide-value method-param ABI slice (admits a `uint<N>`/`sint<N>` value param up to 128 bits via `_harc_u128`); a transactor method call inside a log/fail message (lazy eval). |
| closure-hook cluster (2026-06-13, divergence 20) | **the two closure-hook forms — `on <obj>.<method> pre/post` METHOD HOOKS and `on regs.REG` PER-REGISTER WRITE CALLBACKS — via host-state promotion** (`src/ir/{mod,display,verify,passes/placement}.rs`, `src/ir/lower/{mod,regblock,stmts,exprs,transactors}.rs`, `src/codegen/tbir/{mod,func}.rs`). Both share one root cause: v1 emits the hook body as a `[&]`-capturing C++ closure over RUN-SCOPE state, fired from elsewhere; the function-per-CFG IR has no inline-closure-capturing-locals node. The fix is **host-state promotion** — the captured run-scope state moves to host state both sites reference. New `FunctionKind::TestHook` (a test-scope closure-hook body lowered with the firing site's param surface but in the TEST scope's `LowerCtx`, emitted as a free `[&]`-capturing lambda named by `TbFunction::name`). **(a) method hooks**: a captured test-scope `let` (`pre_count`) is PROMOTED to a `_tb` scalar field (`LowerCtx::promoted_tb_lets` drives bare-ident → `Expr::TbField`/`Stmt::TbFieldWrite`, the let-decl is dropped + registered as a scalar host field with its const init as default); the hook body lowers as a `TestHook` over the method's by-value params (`t.addr`/`t.value`), the promoted `_tb` fields, and the firing transactor's state (`_tb.drv.last_read` → `Expr::TransactorState`, the existing #381 machinery); `TransactorMethodSchema` gains `pre_hooks`/`post_hooks: Vec<FunctionId>` and `emit_method` fires the pre fan-out before the body and the post fan-out before each void return (v1's `<Type>_<method>_pre/_post` loops). **(b) per-register callbacks**: `RegblockBinding` gains `callbacks: Vec<(register, FunctionId)>`; a `record_write` targeting a callback-bearing binding lowers to the new `Stmt::RecordWriteCb` (mirror update + per-binding recursion-depth guard + callback dispatch, v1's `<binding>_cb_depth`/`HARC_RAL_CB_MAX_DEPTH`); the callback is a `TestHook` with a single `data` param. The callback-bearing mirror + depth counter are declared ONCE at TEST scope (shared by `[&]` capture between the run coroutine and every callback) — `func::shared_mirror_names` skips their per-function declaration + RecordInit, and `emit_test_hook` declares each hook as a forward `std::function` slot then assigns the lambda so a recursive callback can call ITSELF (a direct `auto` lambda cannot). Callback FunctionIds are reserved before run/check lowering so `RecordWriteCb` firing sites can reference them. **Divergence**: post-hooks fire before every void return (v1 fires at the body's natural end; in this subset hooked methods are void single-end, so identical); the recursion-guard FATAL uses the const-decoded `at addr 0x..` to match v1 exactly. Fixtures: `axilite_hooks_test` (pre+post counters + `post_value_sum` accumulation + `drv.last_read` read — pass), `regblock_record_test` (passive record + MM2S_SA callback deriving MM2S_LEN — pass), `regblock_record_recursion_test` (self-write callback trips the depth-16 guard FATAL — `fail`); all three trace-diff clean v1↔tbir at seed 1. Closes the closure-hook cluster. **Still rejected**: a hook-captured `let` with a non-constant initializer; a callback nested inside a `run`/`scope` block (only impl-item-level `on` collected); a value-returning hook. |
| wide-value (>64-bit) method-param ABI slice (2026-06-13) | **a transactor method value param typed `uint<N>`/`sint<N>` wider than 64 bits, up to 128 bits — the wide-value method ABI** (component-method param rendering still widens to `uint64_t`; no corpus fixture needs a wide component-method param) (`src/ir/lower/transactors.rs`, `src/codegen/tbir/{mod,func}.rs`). The last genuinely-large standalone residual (`aes_cipher_top_test` `load_block(key: uint<128>, text_in: uint<128>)`). The tbir value model's locals are `uint64_t`; a wide param needs v1's `_harc_u128` (`__uint128_t`) storage. **No new IR variant** — the width already rides on `IrType::UInt(Some(w))`/`SInt(Some(w))` and `display.rs` already prints `uint<128>`; the change is (1) a new `check_method_param_ty` gate that admits a method *value param* up to 128 bits (`check_scalar_ty` factored into `check_scalar_ty_max(.., max_w)`; method-return types and TLM bus-target args stay ≤64), so `method_param_ir_type` lowers the param as a wide-typed local; (2) a `local_scalar_cty` codegen helper (mirroring v1's `cpp_uint_for_width`/`cpp_sint_for_width`) that renders a >64-bit scalar local/param as `_harc_u128`, used by `declare_locals` and the `emit_method` param signature. The body's existing wide-value paths do the rest: a `uint<128>` literal arg lowers to `Expr::WideLiteral` (rendered as v1's `((_harc_u128)hi<<64)|lo` composite); `dut.key = key` (a wide `Local`) routes through `harc_rt::harc_assign(sig, key)` (the non-literal `DutWrite` arm — `key` is a `_harc_u128` lvalue Verilator widens to its `VlWide` port); `dut.text_out == 0x…` reads the wide port as `_harc_u128` and compares against the wide-literal composite; the `${dut.text_out:032x}` interp uses `HarcHexBuf128`. **Subset / divergence**: only a *value param up to 128 bits* — the body MOVES the value to a wide DUT port and COMPARES it (the fixture's whole use). Host-side wide *arithmetic* beyond what `__uint128_t` natively expresses is not specially modeled (≤128-bit native ops match v1; the corpus does no wide arithmetic). A method param **wider than 128 bits** (v1's `HarcWide<N>` word-array model + its full ABI) stays **rejected precisely** at `check_method_param_ty` — a larger slice. Method-return types remain ≤64-bit. Fixture: `aes_cipher_top_test` (AES-128 iterative cipher, `aes_cipher_top.sv aes_key_expand_128.sv xtime.sv`, pass) — drives 128-bit `key`/`text_in` whole-signal, compares 128-bit `text_out`; trace-diff clean v1↔tbir at seed 1. |
| TLM OOO-responder lanes slice (2026-06-13) | **a bound-to TARGET responder SERVING an `out_of_order tags N` method — the multi-lane RESPONDER topology** (`src/ir/{mod,display}.rs`, `src/ir/lower/{transactors,exprs}.rs`, `src/codegen/tbir/func.rs`). The last deferred TLM residual (the OOO-RESPONDER LANE form, distinct from the nested-forwarding slice's downstream-OOO-call). A `thread bus.read_ooo(addr) ... return ...` serving an `out_of_order tags N` `tlm_method` lowers to v1's `emit_bound_tagged_tlm_target_actors` topology: a per-tag **dispatcher** (a combinational `req_ready` accept gate pushed onto `_post_eval_services` + a coroutine latching args/`req_tag` into a free lane), **N lane coroutines** (each runs the responder loop-switch body, publishes its result + `lane_rsp_valid`), and an **arbiter** routing the highest-index ready lane's response back on the hidden `rsp_data`/`rsp_tag` wires — so tag 1 can complete before tag 0. **One new IR field** — `TargetTlmMethodSchema::ooo_tags: Option<u64>` (folded by a new `exprs::parse_int_literal_expr`, range-checked `1..=64` at lowering; `None` = blocking single-responder). No new IR variants: the lowered responder `function` is byte-identical to the blocking form; the blocking rejection in `lower_bound_target_transactor` becomes a fold+validate. The responder loop-switch body is factored into a shared `emit_responder_loop_switch` reused by the blocking actor and each lane. **Divergence**: the per-tag arg/response arrays are `uint64_t` (the TB-IR value model), not v1's precise per-method C-types — the runtime `harc_read`/`harc_assign` helpers still width-correct the bus wires, so behavior + traces are identical. The `tlm_call` trace payloads (request edge tagged with the accepted `_tag`, response with the selected `_sel`, `(int64_t)(...)` cast) match v1 byte-for-byte. Fixtures (both pass, trace-diff clean v1↔tbir at seed 1): `tlm_target_ooo_lanes_test` (pure 2-lane OOO responder, out-of-order completion), `tlm_pairing_arch_initiator_test` (mixed `blocking` + `out_of_order tags 2` responders against an ARCH OOO initiator — also passes the ARCH-native-DUT sweep). Closes the TLM residual map; the only remaining TLM holdout is `tlm_pairing_arch_target_test` (LOWERS + trace-diff clean, gate-blocked by a local-Verilator-5.048 TLM-SVA `$fatal` artifact, NOT a lowering gap). |
| transactor-composition cluster (2026-06-13) | **reactive-monitor transactor routing** (`src/ir/lower/{components,mod}.rs`). Broadens the composite-component routing predicate (`transactor_is_component`): a **reactive monitor / checker** transactor — cycle-trigger (`on dut.<sig>`, `on <expr> level`) and/or periodic (`on <N> cycles`) handlers with NO `in event` consumer pipe — now routes to the component table (which already lowers those handler shapes against an optional `dut` handle + scoreboard subs). A reactive-monitor instance accepts (and ignores) an `active`/`passive` mode at the field site (`transactor_is_reactive_monitor` → the new `reactive_monitor_names` gate in `validate_testbench_component`; its handlers are always-on, registered regardless of mode, so a `passive` instance is valid — unlike an `in event` consumer whose `on req` registration needs `active`). No new IR variants — the component path's existing cycle-trigger/periodic `_checkers` machinery handles these handler shapes unchanged. Also hardened: a `queue<Record>` element on a component queue field is now rejected PRECISELY (`queue_elem_signedness`) instead of silently mis-lowering a named element as an unsigned scalar (the prior `_ => false` fall-through). Fixture: `dma_engine_test` (`dma_engine.sv`, pass) — a `passive` reactive-monitor (`on dut.<v> && dut.<r>` handshake observers feeding a scoreboard + an `on dut.mem_rd_valid level` combinational memory model) plus an `active` DUT-poking APB BFM; trace-diff clean v1↔tbir at seed 1. **Deferred** (each stacks one or more further seams, all rejected precisely): `scoreboard_typed_queue_test` (`queue<Struct>` on a method-bearing scoreboard component — needs the component-queue record-element seam — plus `phase post_eval`), `post_eval_provider_test` (function-library transactor as a sub-field + component-as-method-argument: `sb.observe(addr, model)` passes a transactor instance + dispatches `model.predict_read(...)` on the param + `phase post_eval`); `axilite_env_test` is now **shipped** by the env-of-DUT-poking-transactor slice below. |
| env-of-DUT-poking-transactor slice (2026-06-13) | **an `env` holding a DUT-poking hookable BFM transactor as a by-value Sub-component** (`env AxilEnv { drv : AxilXactor active; sb : AxilSb }`, with `env.drv.axil_write(t.addr, t.value)` / `let got = env.drv.axil_read(t.addr)` dispatch, `env.drv.dut = dut` bind, and `env.sb.expected.push/pop` scalar-queue access through the env) (`src/ir/lower/{components,mod}.rs`). The `transactor_is_component` predicate gains a trailing **per-use-site** arm: a **purely structural DUT-poking BFM** — `hookable` methods + a module-typed `dut` handle, NO `on`/event handler, NOT `bound to` — routes to the composite-component table ONLY when it is `env_held` (referenced as a by-value sub-component field of some `env`/`agent` decl). Standalone (a top-level testbench field or test-scope `let`) it stays on the dedicated `TransactorSchema` path — its long-standing default that every standalone fixture + the contract unit tests rely on. The `env_held` set is computed once in `lower_program` from every `env`/`agent` decl's `Named` field types (a `testbench`, which also parses as `Item::Env`, is excluded — a testbench FIELD is a top-level binding, not an env holding the BFM by value) and threaded to each `transactor_is_component(t, env_held)` site. When env-held, the env resolves the BFM as a `ComponentFieldKind::Sub` and the existing component sub-component machinery threads `env.drv.<method>(...)` dispatch (`ComponentCall`), the `env.drv.dut = dut` bind (erased — the method body's `DutWrite`s already target the test `dut`), and `env.sb.expected` scalar-queue ops (`ScoreboardSub`). v1 emits this BFM identically to a component (a `struct AxilXactor { VAxiLiteRegs* dut; … }` + `AxilXactor_<m>(AxilXactor& self, …)` free-function lambdas with `wait N cycles` → synchronous `tick()` loops), which IS the component-method emission shape — so routing the env-held form here is byte-faithful at the placement v1 already used. **No new IR variants.** Being a transactor, an env-held BFM still requires an explicit `active` mode at every binding site (a `passive` instance has no methods — every method lives under `when active`): a new `transactor_is_dut_poking_bfm(t, env_held)` classifier (true exactly for the env-held BFM) feeds a `dut_poking_bfm_names` gate enforcing this at the testbench-field site (`validate_testbench_component`) and the test-scope-`let` site (`lower_test`), checked BEFORE the no-mode composite-component gate. Fixture: `axilite_env_test` (`AxiLiteRegs.sv`, pass) — 5 randomized AXI-Lite round-trips through `env.drv` checked against an `env.sb` scoreboard queue; trace-diff clean v1↔tbir at seed 1. **Still rejected**: a `passive` env-held DUT-poking BFM instance (no methods); a DUT handle not named `dut`; >1 DUT handle field; a record-typed method param on an env-held BFM (the standalone `TransactorSchema` path already supports it, but no env-held equiv fixture exercises it yet — gated precisely as "field access on a non-DUT value"). |

| component-queue + record-element + `phase post_eval` slice (2026-06-13) | **(a) a `queue<Record>` element on a scoreboard/component queue field, (b) the component-queue ops, (c) a self-relative sub-component method call + whole-value copy, and (d) the general `on … phase post_eval` handler-phase seam** (`src/ir/{mod,display,verify}.rs`, `src/ir/lower/{components,exprs,stmts}.rs`, `src/codegen/tbir/{expr,func,runtime,mod}.rs`). **(a)** `ScoreboardFieldKind::Queue` / `ComponentFieldKind::Queue` now carry a shared `QueueElem { Scalar { signed } \| Record(RecordId) }` (mirroring `EventPayload`); `lower_queue_elem` resolves a `queue<CheckerError>` element against `record_ids`, rejecting enum/Vec/nested/>64-bit precisely. The C++ element type is the record struct (`queue_elem_cty` → `harc_rt::HarcQueue<Rec>`). **(b)** new `Stmt::ComponentQueuePush`/`ComponentQueuePop` + `Expr::ComponentQueueQuery` (size/empty) carry a `ComponentBase` receiver — `errors.push(err)` (self) / `checker.sb.errors.{push,pop,size}()` (path) resolve via `as_component_queue_call` (a queue-field-typed terminal segment) and `component_queue_elem` (the record id for a record-element pop's dest type). **(c)** `as_component_method_call` now resolves a self-relative `<self-sub>.<method>()` (`sb.record_error(...)`) to a `self`-rooted `ComponentBase::Path` (re-rooted at the running instance in `comp_base_cpp_subst`, mirroring `ScoreboardOp`); a whole sub-component value copy `checker.sb = sb` lowers to a new `Stmt::ComponentSubAssign { dst, field, src }` (a plain C++ struct copy of two run-scope component locals). **(d)** new IR `HandlerPhase { Checker \| PostEval }` carried on `PeriodicHandlerSchema.phase` (lowered from `ast::OnPhase`); the tbir backend registers a `PostEval` periodic handler into `_post_eval_services` (run after the DUT posedge `eval`, before the run coroutine resumes) instead of `_checkers` — the shared seam a sibling `post_eval_provider_test` reuses. Fixture: `scoreboard_typed_queue_test` (`scoreboard_typed_queue.sv`, pass) — a `GlobalScoreboard.record_error` builds + pushes a `CheckerError` record onto `queue<CheckerError>`, driven from a `Checker` transactor's `on 1 cycles phase post_eval` mismatch checker; the test pops the record and asserts its fields. Trace-diff clean v1↔tbir at seed 1. |
| function-library transactor + component-arg dispatch slice (2026-06-13) | **a DUT/event/`on`-less, method-only transactor lowered as a component, plus a component-typed method parameter dispatched on, plus a component passed by value as a method argument, plus a record-returning component method** (`src/ir/{mod,display,verify,passes/placement}.rs`, `src/ir/lower/{components,exprs,stmts,mod}.rs`, `src/codegen/tbir/{expr,func}.rs`). **(a) function-library transactor** — `transactor_is_function_library` (pure `function`/`hookable` methods + optional scalar state, NO module-typed DUT handle, NO event field, NO `on`/periodic handler, no generics/`bound to`) now folds into `transactor_is_component`, so a handle-less method-only transactor (`ProtocolModel`) routes to a `ComponentSchema` instead of the DUT-poking `TransactorSchema` (which structurally requires a DUT handle). It emits exactly v1's shape: a by-value struct + `<Comp>_<method>` free-function lambdas. A `function_library_names` gate in `validate_testbench_component` accepts (and ignores) an `active`/`passive` mode on its testbench field — a function library has no `when active` registration to gate, mirroring the reactive-monitor case. **(b) component-typed method param** — new `IrType::Component(ComponentId)`; `method_param_ir_type` resolves a param typed by a component name (`observe(addr, model: ProtocolModel)`) against the program component table. The lambda takes it by value as the component struct (`ProtocolModel model`). **(c) method-call-on-param dispatch** — new `ComponentBase::Local(LocalId)`; `as_component_method_call` resolves `model.predict_read(addr)` where `model` is a component-typed param local, dispatching `<Comp>_<method>(model, addr)`. **(d) component-as-method-argument** — new `Expr::ComponentValue { base }`; a bare ident / path that names a `Sub` sub-component field (`sb.observe(addr, model)` reads `model`, a self `Sub`) lowers to a by-value component value (`as_component_value_read` → a `self`-rooted `Path` base). **(e) record-returning method** — `let r : ReadResponse = model.predict_read(...)` lowers to a record-typed `ComponentCall` dest; the method's `__ret` slot is typed as the record (so codegen declares the struct), and `emit_component_fn_lambda`'s return type resolves to the record name (default-return is `return {};`). The shared `phase post_eval` seam is reused unchanged. **Pre-existing bug fixed**: the verifier's cross-block use-before-def dataflow (`gen_kill`) did not register a `ComponentCall { dest: Some(_) }` (nor `ComponentQueuePop`) as a definition, so a record-returning component call whose result was read in a later block tripped a false `LocalUseBeforeDef` (the statement-walk handled it, the fixpoint `ins` did not). Fixture: `post_eval_provider_test` (`post_eval_provider.sv`, pass) — a `ProtocolModel` function library held in a `BusResponder` transactor (`on 1 cycles phase post_eval`), a `ResponseScoreboard` (`observe(addr, model)`), and the testbench (`model : ProtocolModel active`); the responder dispatches `model.predict_read(...)`, passes `model` into `sb.observe(...)`, and the scoreboard re-dispatches it. Trace-diff clean v1↔tbir at seed 1. With this the corpus reaches full coverage modulo `tlm_pairing_arch_target_test` (resolved separately by the OOO TLM-initiator handshake fix below). **Still rejected**: a component-typed param passed as a bare *value* (only method-receiver + arg forms exercised); a function-library transactor with a module/transaction-typed sub-field. |
| OOO TLM-initiator handshake fix (2026-06-14) | **Closes the last TLM holdout, `tlm_pairing_arch_target_test`** (`src/codegen/cpp_tb.rs::try_emit_bus_tlm_fork`, `src/codegen/tbir/func.rs::emit_tlm_fork`). The fixture lowered + trace-diffed clean but `$fatal`ed under Verilator on the ARCH DUT's auto-emitted `_auto_tlm_mem_read_ooo_req_stable` SVA. **Corrected diagnosis** (earlier slices called this a "local-Verilator-5.048 artifact" — it was not): the SVA is the standard, correct AXI-style handshake-stability rule and fires under any conformant simulator. The real bug was HARC's own OOO TLM *initiator* BFM: at a `fork`-issue the emitted C++ sampled `req_ready` *before* the DUT mirror re-evaluated with the just-written `req_tag`, reading a stale `0` (the previous tag's now-busy slot) and entering its bounded wait loop — leaving `req_valid` asserted ~30 cycles through a `valid && !ready` window, then dropping it while the slot was still busy (a genuine handshake violation the SVA correctly caught). Fix: present the request (valid + tag + payload) for exactly the acceptance cycle, then deassert (`req_valid = 1; co_await wait_cycles(_slot, 1); req_valid = 0;`) — no stale-ready spin. Both emitters fixed identically. `tlm_pairing_arch_target_test` is now registered (v1==tbir, trace-diff clean, ARCH-native sweep passes); arch-com #588 closed as not-an-arch-bug. No new IR, no surface change. |
| #417 — bare sibling TB-method calls + TBIR `--cpp-split tests` (2026-06-17) | **(a) a bare-identifier call to a sibling testbench method, and (b) `--cpp-split tests` sharding for the tbir backend** (`src/ir/lower/{exprs,stmts}.rs`, `src/codegen/tbir/mod.rs`, `src/main.rs`). **(a)** Inside a `_tb.`-prefixed inline frame, a bare call `m(...)` that names a known testbench method now dispatches through the existing `lower_tb_method_call` (one TB method calling a sibling TB method without the dotted `_tb.m()` form). The new `tb_methods` lookup is placed ahead of the `lower_transactor_self_call` / `helpers` / `extern_fns` checks, so a bare name matching a TB method resolves to it. No new IR variants. **(b)** `emit_split_tests_with_file_prefix` shards tests *after* lowering — emits a dispatcher `main.cpp` plus one self-contained translation unit per shard, each re-emitting full scaffolding and only its selected `run_<Test>` functions, with the dispatcher `main()` stripped via `strip_generated_dispatcher` (`rfind` of the trailing dispatcher signature, returns a clean `EmitError` if absent). All tests are validated to share one DUT *before* sharding (`validate_tests_share_dut`). Mirrors the existing v1 split path. The remaining ~90% of the PR is rustfmt churn + a bulk CVDP-bench rewrite toward TB-method helpers. Tests: `split_build_e2e.rs` runs the full build/link/dispatch under BOTH v1 and tbir at default and group-size-1, plus `split_build_rejects_mixed_dut_before_sharding`; the bare-call path is regression-covered by `TestbenchMethodCallsMethod` (`bump_twice`) in `tbir_equiv_fixtures.txt` (v1==tbir) with a committed dump-IR snapshot. **Residual**: helper-vs-TB-method name-collision *precedence* (the placement above) is not exercised by any fixture. |
| #418 — nested TBIR transactor helpers (2026-06-17) | **(a) a bare sibling call between methods of the same DUT-poking transactor, and (b) a testbench helper wrapper calling an active transactor method** (`src/ir/lower/{exprs,stmts,transactors,mod}.rs`, `src/codegen/tbir/{expr,func,mod}.rs`, `src/ir/{mod,display,verify,passes/placement}.rs`). **(a)** A bare call `idle()` inside another method (`write`/`invalid_access`/…) lowers through a new `Stmt::TransactorSelfCall` carrying `CallTarget::TransactorSelfMethod { transactor, method }`, resolved against a per-builder `self_transactor` + `self_transactor_methods` signature table built in a full pre-pass so FORWARD references work. Value-bearing sibling calls are hoisted to a `TransactorSelfCall { dest: Some(temp) }` via `hoist_transactor_edge` (every sibling edge becomes a statement; none stays nested in an expression). **(b)** Field-resolution is widened to accept a non-bare `transactor_fields` receiver, guarded by a `self.lookup(name).is_none()` local-shadowing check. **C++ ABI**: `declare_method_slot` predeclares each transactor method as a `std::function<ret(params)>` slot *before* the lambda is assigned (was `auto X = [&]…`, now `std::function<…> X; X = [&]…`), so a forward sibling reference compiles. The new IR node is threaded through `display`, placement (`expr_has_transactor_edge`, `block_features`), `verify::check_transactor_self_call` (name/arity/void-dest validation), and the def-before-use dataflow. A transactor self-call inside a `wait until` predicate is rejected precisely. **Recursion guard (#420)**: because the `std::function` slot makes a self-reference *compile* (v1's `auto` lambda rejected it at the C++ level) and methods emit as synchronous lambdas, a method-call cycle (`a→a` or `a→b→a`) would recurse the C++ stack one frame per cycle until it overflows. `reject_recursive_transactor_methods` (`lower/mod.rs`) DFS-detects the cycle and returns `LowerError::Invalid` with the rendered path — mirroring the #350 helper-recursion and phase-recursion guards. Fixtures: CVDP `apb_dsp_unit`/`events_to_apb`/`simple_spi` benches refactored to transactor-helper style; `tests/tbir.rs` covers bare sibling dispatch, value-returning sibling hoist, `wait until` self-call rejection, the helper-wrapper path, and (from #420) direct/mutual recursion rejection + acyclic-chain acceptance. The `cam_value_basic` emitted-C++ snapshot locks the `auto`→`std::function` predeclaration shape. **Residual**: a >64-bit transactor method *return* still truncates to `uint64_t` (pre-existing; the slot faithfully mirrors the lambda); no emitted-C++ snapshot yet exercises an actual sibling call. |
| bit-slice + uninitialized-scalar-let (2026-06-18) | **Two bread-and-butter expression/statement gaps closed** (found by the v1-vs-tbir gap audit; both were undocumented and latent — no corpus fixture exercised them, but v1 always supported them, so they would hard-fail real TBs once v1 is deleted). **(a) constant bit-slice `x[hi:lo]`** — `src/ir/lower/exprs.rs` no longer rejects `ExprKind::BitSlice`; it const-folds the bounds (`parse_int_literal_expr`) and, for literal `hi >= lo`, emits the already-existing `Expr::BitSlice { target, hi: u32, lo: u32 }` IR node (right-shift + mask) that the tbir backend (`tbir/expr.rs`) and v1 (`harc_rt::harc_bits`) already render identically. A variable part-select (`x[s +: W]` with a non-const offset) does not fold and stays out of subset. **(b) uninitialized scalar `let x: uint<N>;`** — `src/ir/lower/stmts.rs` declares the typed local and pushes an explicit zero-init `Stmt::Assign(id, Literal{0})` (matches v1's `<cty> x = 0;` and gives the verifier's dominance check a definition), enabling the declare-then-assign-in-loop idiom. An untyped `let x;` with no initializer is still rejected (cannot be sized). Fixture: `bitslice_uninit_test` (`top_counter.sv`, pass; trace-diff clean v1↔tbir). |
| #439 — scalar tseq (2026-06-19) | **`TSeq<scalar>` / `TSeq<uint<N>>` generator lowering** (`src/codegen/tbir/func.rs`, `src/ir/{display,mod}.rs`, `src/ir/lower/{control,mod,tseqs}.rs`). `FunctionKind::Tseq` now carries a `TseqElem { Record(RecordId) \| Scalar(IrType) }` (was `{ record }`), and a new `IrType::Seq(Box<IrType>)` models a scalar transaction-sequence accumulator alongside `RecordSeq`. A `tseq Gen(...) -> TSeq<uint<8>>` lowers its generator to a `std::vector<T>` over the scalar C++ type (`local_scalar_cty`); `yield v` → `SeqPush`; `let s = Gen(..)` types the local `Seq`; `for v in s` iterates element-typed (the `lower_for_in_seq` loop variable copy generalized from record-only to any element `IrType`). `declare_locals` emits a `std::vector<cty>` for a `Seq` local. **Residual**: a scalar tseq as a *sequencer method parameter* still resolves to `IrType::Unknown` in `method_param_ir_type` (`src/ir/lower/components.rs` — the element-name lookup consults only `record_ids`); only the generator-return / `let =` / `for-in` surface lowers a scalar tseq. |
| #431 — `queue<struct>` on a data-only scoreboard (2026-06-19) | **A `queue<struct>`/`queue<transaction>` element on a data-only scoreboard** (`src/ir/lower/{components,scoreboards,mod,stmts}.rs`). `scoreboard_field_kind` (`scoreboards.rs`) routes a `queue<T>` element through the shared `components::lower_queue_elem` (made `pub(crate)`), resolving a value-record element against the program `record_ids` to `QueueElem::Record(rid)` (mirroring v1's `HarcQueue<Struct>` and the already-shipped env-nested data-scoreboard path); a record-element pop `let s : Sample = sb.q.pop()` is recognized as a queue pop ahead of the record-typed-let block (`stmts.rs`). Scalar ≤ 64-bit elements unchanged; enum/Vec/nested/unknown elements stay rejected precisely. No new IR variants. |
| #433 — variable lane index on DUT vec ports (2026-06-19) | **Both constant AND variable lane indices on DUT `Vec` ports now lower** (`src/codegen/tbir/{expr,func}.rs`, `src/ir/display.rs`). `PortRef::lane` carries a `LaneIndex { Const(u64) \| Var(Expr) }`; a new `expr::lane_index_cpp` renders a const to its literal and a var through the regular `expr_cpp` value path (mirroring v1's `dut_packed_lane` re-rendering an arbitrary `&Expr`). `port_read`/`DutWrite`/`DutRead` all route lane reads/writes through it (packed → `harc_vec_lane_read/write<W>`, unpacked → raw `[idx]` subscript). Covergroup lane points stay constant-only (the schema lowers before any runtime scope); a runtime cover lane is a precise codegen error. |
| #434 / #537 — wide width casts through 1024 bits (2026-06-19, extended 2026-08-13) | **`.trunc/zext/sext/resize<N>()` width casts cover the language range `1..=1024`** (`src/codegen/tbir/expr.rs`, `src/ir/lower/{exprs,stmts}.rs`, `src/main.rs`, `runtime/harc_thread_rt.h`). Casts through 64 use native scalar shapes, 65..128 use `_harc_u128`, and 129..1024 use `HarcWide<ceil(N/32)>` with exact logical-width masking for non-word-aligned values. Chained sign extension accepts `HarcWide` sources. `harc check`, v1, TB-IR lowering, and the verifier share the 1024-bit ceiling; `wide_cast_test` is self-proving and trace-equivalent across v1/TB-IR. Covergroup sample expressions retain their independent 64-bit model. |
| #444 — check-phase test-scope let promotion (2026-06-19) | **A test-scope `let` read in the `check`/`teardown` phase is auto-promoted to a `_tb` host field** (`src/codegen/tbir/mod.rs`, `src/ir/lower/mod.rs`). v1 hoists every test-scope let to `main` scope so run AND check capture it by reference; the IR splits run and check into separate functions. `lower_test` now collects check/teardown ident reads and promotes a matching test-scope let into the SAME `_tb` scalar host field the closure-hook path uses (reads → `Expr::TbField`, writes → `Stmt::TbFieldWrite`), so the value persists across the run→check boundary (trace-equivalent — a `_tb` field write emits no trace event). A synthetic (classic `test`-form) testbench gains a `_tb` struct only when it carries such a promoted field (`needs_tb_struct`). Fixture: `check_phase_let_test` (`top_counter.sv`, pass; trace-diff clean). **Residual**: a promoted let with a non-constant initializer is rejected precisely (its `_tb` default must be a compile-time constant — assign the computed value in `run` instead); a `let` declared inside the check/teardown body shadows (and so does not promote) a same-named test-scope let. |
| #435 — multi-cycle bound-monitor cadence (2026-06-19) | **Bound-bus handshake monitor sampling now matches v1 for multi-cycle held handshakes** (`src/codegen/tbir/mod.rs`, `src/ir/{lower/components,mod}.rs`). v1 lowers a bound monitor as a `wait_until(valid && ready)` + `wait_cycles(1)` coroutine, so a continuously-held handshake samples every OTHER cycle (one beat, then the re-arm consumes the next). The earlier IR used a plain rising-edge cycle-trigger (correct only for single-beat handshakes — see the bound-to monitor slice divergence). `emit_lifecycle_checkers` now emits a fire-then-cooldown latch for a `monitor_channel` handler (fire when the predicate holds, then consume the following cycle as the `wait_cycles(1)` re-arm), reproducing the v1 cadence exactly; the stored `edge` is vestigial for a monitor channel. `transactor_is_component` also now routes a `bound to` transactor with a passive monitor (`on bus.<ch>.handshake` and no event/driver half) through the component table whenever it has any `on` handler. Fixture: `stream_burst_mon_test` (`stream_burst.sv`, multi-cycle held valid/ready burst, pass; trace-diff clean). |

### Construct subset

From `src/ir/lower/mod.rs`: classic-form and impl-for testbench-bound
tests with a single DUT, declared clocks (time-literal or `domain`
periods), and the core statement set — DUT port read/write,
`let`/assign, `log`/`logf`, inline `assert ... else fail`, `fail`,
`wait N cycles` (plain and clock-qualified `wait N cycles on <clock>`
— the qualifier rides as an `Option<WaitClock>` on the `WaitCycles`
terminator, resolved at lowering against the test's declared clocks;
an unknown clock name is a structured lowering error listing the
declared clocks, where v1 deferred it to emission. Emission mirrors
v1's inline `eval_clocks_until` loop — no coroutine yield — so
sub-primary-cycle precision and checker timing are identical),
`wait until` (single, `all of`, and `any of`, optional timeout),
`if`/`for`/`while`/`repeat`/`loop`/`break`/`continue`, covergroups
with set-literal and range (`[a..b]`, inclusive, open bounds allowed)
bins plus declared `cross` items, helper functions, `transaction`
value-records in their non-randomize usage: declarations lower into
the `TbProgram::records` table (scalar fields ≤64 bits with literal
defaults; `keep`/attr constraint metadata carried as inert source
text), `let t : TxnType` default-constructs (re-running field
defaults at the let site, so loop iterations re-initialize like v1's
in-place declaration), and `t.field` reads/writes work everywhere a
scalar local does — including format args, branch conditions, and
loop bounds. Ternary expressions (`cond ? a : b`) lower to
`Expr::Ternary` (both backends emit the C++ ternary; the port-hoisted
form evaluates both arms' side-effect-free port reads eagerly).
The `bus`
construct subset (added 2026-06-12, `src/ir/lower/bus.rs`):
declarations (inline or `use`-imported), test-scope `= bind dut`
bindings, protocol-typed signal access (`<bind>.<sig>`,
`<bind>.<ch>.<sig>`, pre-flattened `<bind>.<ch>_<sig>`), channel
auto-handshakes (`<bind>.<ch>.send(...)` / `.recv()`, CFG-inlined to
v1's 16-cycle-budget valid/ready dance), and **blocking `tlm_method`
calls**, which lower to `Expr::Call(CallTarget::TransactorMethod {
bus_field, method }, args)` — the design doc's sequence→transactor
call edge, never inlined at the IR level.
The `regblock`
construct subset (added 2026-06-12, `src/ir/lower/regblock.rs`):
register-block declarations (`register NAME @ ADDR [reset V] access
rw|ro|wo`) lower to a synthetic mirror value-record plus a
`RegblockSchema`; `let regs : R = bind <helper>` over a test-scope
unbound-transactor helper; and the **register-level frontdoor** —
`regs.NAME = v` (mirror update + `Helper.write` call edge, RO
suppressed) and `let x = regs.NAME` (`Helper.read` call edge + mirror
predict, WO served from mirror). The mirror is an `IrType::Record` and
the let-RHS/statement traffic reuses the `CallTarget::TransactorMethod`
edge. The bus-bound `via` helper form lowers via the initiator-BFM
slice. The **regblock-residuals slice** (2026-06-13, divergence 12)
adds two more: (a) **register reads outside `let`-RHS** — assert
conditions and `log`/`fail` format args — lower to a new `Expr::RegRead`
(v1's inline assignment-expression `(regs.NAME = <Helper>_read(off))`
for RW/RO, `regs.NAME` for WO), firing exactly one bus read per textual
occurrence (the `via` helper's `read` is a plain hookable lambda, not
the TLM seam, so it is a legitimate sub-expression value — unlike the
verifier-pinned call edge); and (b) **`bitbash(regs)`** —
compile-time-unrolled walk-all over the RW registers (write/read both
patterns + compare; RO/WO skipped), unrolled into the existing
`TransactorCall`/`AssertCheck` statements, plus a new `Expr::ErrorCount`
framework value for the trailing `assert errors == 0`. The
**field-level + addrmap slice** (2026-06-13, divergence 12) adds the
remaining regblock residuals: (a) **field-level decomposition**
(`regs.REG.FIELD`) — a register split into named bit-fields lowers to a
masked read-modify-write on the whole-register mirror cell
(`(mirror.REG & ~(mask<<pos)) | ((v & mask)<<pos)`) plus a
full-register bus write, and a shifted-extract read
(`((mirror.REG = <H>_read(off)) >> pos) & mask` for RW/RO,
`(mirror.REG >> pos) & mask` for WO) — v1's bit-slice insert/extract,
composed entirely from existing IR (`RecordFieldWrite` / `RegRead` /
`RecordField` / `Binary` / `Unary`), no new variant; (b) the
**`addrmap` construct** (incl. **`alias of`**) — each `instance NAME :
R @ BASE [size S] [alias of OTHER]` becomes its own whole-register
mirror local (mangled `__addrmap_<chip>_<inst>`) with a register table
whose offsets are pre-shifted to `base + reg_off`, reusing the flat-
regblock register/field access lowering verbatim; an `alias of` instance
shares its target's mirror local (one storage cell across windows) while
keeping its own base for bus traffic. Alias/overlap validation mirrors
v1's `check_addrmap_aliases`/`check_addrmap_overlap`
(`src/ir/lower/addrmap.rs`). The **passive `record_write`/`record_read`
API** also lowers now (2026-06-13, divergence 12): a constant-address
`record_write(addr, data)` decodes to a masked mirror `RecordFieldWrite`
and `record_read(addr)` to a mirror `RecordField` read, both with no bus
traffic — `regblock_record_api_test`. The per-register `on regs.REG`
write callback ALSO lowers now (closure-hook cluster, 2026-06-13,
divergence 20): a `record_write` targeting a callback-bearing binding
lowers to `Stmt::RecordWriteCb` (mirror update + per-binding
recursion-depth guard + callback dispatch), and the callback body is a
`FunctionKind::TestHook` function back-patched onto the binding —
host-state promotion (the SHARED mirror + `<binding>_cb_depth` move to
test scope, captured by `[&]`) is what lets the function-per-CFG IR
express v1's reference-capturing closure fired from inside
`record_write`. Both corpus fixtures now pass: `regblock_record_test`
and the negative `regblock_record_recursion_test` (the depth-16 guard
FATALs identically under both codegens, `at addr 0x18`).
`transactor` declarations lower in their **unbound DUT-poking BFM
subset**: no `bound to <BusType>`, no generics, exactly one
module-typed field (the DUT handle — its type must match the test's
DUT, since the IR keeps the single-DUT model), and
`hookable`/`function` methods with scalar params (a `uint<N>`/`sint<N>`
value param up to 128 bits via the wide-value `_harc_u128` ABI; bool;
or a by-value record) and an optional ≤64-bit scalar return. Each method body lowers to its own
`TbFunction` (`kind: TransactorBody`) with the transactor's DUT field
as the body's DUT name — v1's emission-time `field_subs` substitution
moved to lowering-time resolution, per the design doc. Instances are
`active`-mode testbench fields; `<inst>.dut = dut` is validated and
erased (the bind is static); calls lower to `Stmt::TransactorCall`
carrying the `Expr::Call(CallTarget::TransactorMethod { .. })` edge —
**never inlined** (the Tier-1/Tier-0 placement seam; the placement
pass classifies call-carrying blocks `TimingTolerant`, and
`lower_coroutine` tags method bodies as Tier-0 FSM candidates).
Methods keep v1's synchronous hookable semantics — their waits emit
as `tick()` loops, never scheduler suspensions — so clock-qualified
waits and timed `wait until` are rejected *inside method bodies*
(untimed `wait until` emits v1's `while (!(pred)) tick();`).
The `CallTarget::TransactorMethod` edge thus has TWO sanctioned
homes, dispatched by which testbench namespace `bus_field` resolves
in (the namespaces are disjoint — a name collision is rejected at
lowering): a bus binding → the entire Assign RHS, expanded by the
tbir backend into v1's req/rsp wire protocol; a transactor field →
the `Stmt::TransactorCall` payload, emitted as a direct
`<Type>_<method>` lambda call. Everything else —
call edge, never inlined at the IR level. The **singleton-blocker
batch** (2026-06-12) added: ternary `?:` (`Expr::Ternary`, emitted as
the lazy C++ conditional), wall-clock waits (`wait 80ns` →
`Terminator::WaitTimePs`, picoseconds resolved at lowering; clocked
tests only), the `debug` log severity, hex integer literals wider
than 64 bits (`Expr::WideLiteral` word lists; v1's `_harc_u128` /
`HarcWide<N>` / `harc_assign_words_checked` / `harc_eq_words`
emission shapes), file-scope `const` declarations and `enum` variant
names (substituted as integer literals at use sites; enum-typed
transaction fields lower as scalar variant indices), plain test-scope
`let`s (hoisted to the head of the run function; check-phase
references are precisely rejected — see the divergence note below),
constant-lane DUT port access (`dut.<port>[i]` with a literal/const
index → `PortRef::lane`; emission splits packed lanes through
`harc_rt::harc_vec_lane_*<W>` from unpacked-array subscripts via the
`--sv` lane table, like v1), testbench helper methods
(`function`/`hookable` in the bound testbench, CFG-inlined at
`_tb.<m>(...)` call sites like impure helpers), scalar testbench
fields (`expected : uint<32> default 0` →
`TestbenchSchema::scalar_fields`, read/written through
`Expr::TbField` / `Stmt::TbFieldWrite` as run/check-shared `_tb`
members), width-method intrinsics (`.trunc<N>()` / `.zext<N>()` /
`.sext<N>()` / `.resize<N>()` with N ≤ 1024 → `Expr::WidthCast`,
using scalar, `_harc_u128`, or `HarcWide<N>` mask/cast/extension emission
and mirroring v1's direction checks, with best-effort receiver-width
inference from typed lets / casts / chained methods / literals), and scalar `as uint<W>`-family
casts (≤ 64-bit width relabels — value-identity in the uint64 local
model, exactly v1's same-storage C cast).
The `scoreboard` construct subset (added 2026-06-12,
`src/ir/lower/scoreboards.rs`): a scoreboard is a **data-only
host-state record** — a testbench field holding scalar counters
(uint/sint/bits/bool ≤ 64 bits with literal defaults) and `queue<T>`
FIFOs of a scalar element type. Declarations lower into the
`TbProgram::scoreboards` table (`ScoreboardSchema`); a scoreboard-typed
testbench field becomes a default-constructed member of the `_tb`
struct (`TestbenchSchema::scoreboard_fields`), so run and check share
it. The test body manipulates it through `Stmt::ScoreboardOp`
(`sb.q.push(x)` statement, `let v = sb.q.pop()` / `v = sb.q.pop()`
bind, `sb.counter = ...` scalar write) and `Expr::ScoreboardQuery`
(`sb.counter` scalar read, `sb.q.size()`, `sb.q.empty()` — value
positions everywhere a scalar local is allowed). Emission mirrors
v1's `emit_scoreboard`: a C++ struct with the scalar members and
`harc_rt::HarcQueue<T>` queues, accessed as direct `_tb.<sb>.<field>`
member ops — so the two codegens trace-diff clean. Scoreboard
`hookable`/`function` **methods** are out of this subset: they mutate
scoreboard instance state, which v0 does not materialize, so a
method-bearing scoreboard is rejected at the declaration (not silently
dropped). Event-driven `on`/`connect` wiring is likewise rejected at the
site — it gates on the agent/env/event slices. A `queue<Struct>`/
`queue<transaction>` field now lowers (#431, the record element resolves
through the shared `lower_queue_elem` seam — see below); a >64-bit
*scalar* queue element and an enum/Vec/nested element type stay rejected
at the field.
The `struct` construct subset (added 2026-06-12,
`src/ir/lower/records.rs`): a `struct` is the **shared value-record
shape** — v1's `emit_struct_record` routes through the same
`emit_record_struct` a `transaction` uses, so a struct lowers into the
SAME `TbProgram::records` table (`RecordSchema`) and reuses every
record-local operation with **no new IR variants**: `let s : S`
default-constructs (`Stmt::RecordInit`), `s.field = v` writes
(`Stmt::RecordFieldWrite`), and `s.field` reads (`Expr::RecordField`)
work everywhere a transaction local does. Field lowering (scalar
uint/sint/bits/bool/bit ≤ 64 bits, literal defaults, inert `with [...]`
attribute text, enum-typed fields as scalar variant indices) is shared
with transactions (`lower_record_field`). The parser fills
`StructDecl::fields` as a filtered copy of the `Field` items in
`StructDecl::body`, so lowering reads `fields` only (matching v1) and
scans the body solely to reject non-field items. A name shared with a
transaction, struct, or regblock resolves ambiguously through
`record_ids`, so the collision is rejected, not shadowed. A
`Vec<scalar, N>` field DOES lower (a `std::array<T, N>` member with
indexed `rec.data[i]` element access — the struct `Vec`-field slice,
2026-06-13; it unblocked the `tlm_pairing_arch_burst_*` fixtures). A
whole-`Vec` field read/write (`dst.data = src.data`) also lowers now
(`Expr::RecordField` / `Stmt::RecordFieldWrite` with `index: None` →
v1's plain `std::array` member copy — the whole-Vec-field slice,
2026-06-19, `record_vec_field_copy_test`). Out of
subset and rejected at the field/decl, never mis-lowered: a widthless /
nested / list `Vec` element type, other non-scalar / >64-bit fields
(nested structs), non-literal defaults, and
`keep`/`when` items in a struct body (constraint / tagged-ADT
machinery deferred with `randomize`; transaction-body `keep`s now lower
into the constraint-IR seam, but struct-body `keep`/`when` still don't).
Everything else —
`randomize` *expressions* (`let v = randomize(t)` — only the
statement/terminator form lowers in #372) and method-body `randomize`,
`agent`/`event`, bus-bound/event-driven transactors (an UNBOUND
DUT-poking transactor's scalar STATE fields now lower — see divergence
10; bound-initiator and event-driven state still don't; a `passive`
bound event-driven transactor instance with `on bus.<ch>.handshake`
monitor handlers now lowers — see the bound-to monitor slice),
scoreboard *methods* / event-driven
`on`/`connect` wiring (the data-only scoreboard subset lowers — see
below; its `queue<Struct>`/`queue<transaction>` payloads now lower too,
#431), the block-form `fork ... and ...
join` statement (initiator-side `let x = fork bus.method(...)` +
`join_all` TLM issue now lowers — see the initiator-side fork/join_all
slice), a `fork` inside a transactor responder body (fork-forwarding),
out-of-order TLM RESPONDER lanes,
bus bind-site generics (signal *remaps* now lower — see the bus-bind-remap
slice), transaction `when` subtype blocks,
non-scalar / wider-than-64-bit transaction fields and method
params, ... —
is rejected at lowering time with `LowerError::Unsupported` naming the
construct and pointing at `--codegen v1`. Lowering never silently
mis-lowers; that property is load-bearing and tested
(`tests/tbir.rs`).
The **env-composition (analysis-connect) subset** (added 2026-06-13,
`src/ir/lower/components.rs`): the env/agent cluster's flat-struct core
— `env` composition + `connect` (analysis-port → scoreboard sink) +
scoreboard **methods** + analysis-source `out event` / `emit`. Three
source shapes lower into one `ComponentSchema` (`TbProgram::components`),
mirroring v1's uniform `emit_component_struct` + `emit_component_method`
treatment: a method-bearing `scoreboard` (the data-only board stays on
`ScoreboardSchema`), a `transactor` used as a pure analysis source (one
`out event<T>` port + `emit`, NO module-typed DUT field — the DUT-poking
BFM stays on `TransactorSchema`), and an `env` that composes them as
by-value sub-component fields and `connect`s a source event to a sink
method. New IR: `ComponentSchema`/`ComponentFieldKind`/
`ComponentMethodSchema`/`ConnectEdgeSchema` tables,
`FunctionKind::ComponentMethod`, `Stmt::{ComponentFieldWrite,
ComponentEmit, ComponentCall}`, `Expr::ComponentField`, and a
`ComponentBase` (`SelfField` self-relative inside a method body, `Path`
for a test-scope `env.sub.field` access). A composite `env` binds BOTH as
a test-scope `let env : <Env>` AND (since the testbench-field-binding
slice, divergence 16) as a `testbench` FIELD `top : <Env>` — both routes
resolve to the same run-scope `ComponentBase::Path` instance. Method
bodies are loop-switch `<Comp>_<method>(<Comp>& self, args)` lambdas; the
env is a run-scope local with its `connect` push_backs wired at
construction (`<env>.<src>.<event>.push_back([&](auto _t){
<Sink>_<m>(<env>.<sink>, _t); })`) — byte-for-byte v1's shape, so
`analysis_sink_connect_test` trace-diffs clean.

The **agent + on-handler subset** (added 2026-06-13, same file) extends
this with the agent capability the env-composition slice flagged: an
`agent` lowers as a `ComponentSchema` (`ComponentKindTag::Agent`), bound
test-scope as `let <name> : <Agent>`. `on <ev>(arg)` handlers lower as
one-param `ComponentMethod`s recorded in `ComponentSchema::on_handlers`
and register at construction as subscriber closures (bump
`_last_in_cycle`, run the `<Comp>_on_h<fid>` body) — v1's `on`-subscriber
shape. `Stmt::ComponentEmit` now carries a `base` so a test-scope path
`emit tagger.in_ev(v)` and a self-relative `emit observed(v)` both lower;
`Expr::ComponentIdle` covers `idle`/`idle_in`/`idle_out` (v1's
`emit_idle_predicate`). New fixture `agent_on_handler_test` trace-diffs
clean.

The **testbench-field-binding subset** (added 2026-06-13, same file,
divergence 16) lifts the composite-component *testbench field* the
agent/env slices flagged: a component bound as `prod : Producer` inside a
`testbench` block (driven by `impl ... for`) now lowers identically to a
test-scope `let`. The impl-for desugaring prefixes the access with `_tb`;
`FuncBuilder::strip_tb_prefix` drops it in every component-access path so
`emit`/`idle`/field reads/writes resolve to the bare-name instance. New
fixture `tb_field_agent_test` trace-diffs clean. **Out of subset** (precise
rejections — the residual stack behind the binding): `quiesced(N)`,
`watchdog`, named `phase`, `wait until` with heartbeat predicates, `on
<expr>`/`on <N> cycles` triggers, a `queue` SUB-component inside an env,
`sequencer`/`tseq`. Those gate on later slices.

The **event-record-payload subset** (added 2026-06-13, same file,
divergence 17) lifts the `event<transaction>` / `event<struct>` rejection
the binding slice introduced: an analysis port whose payload is a
value-record now lowers. `ComponentFieldKind::Event` carries an
`EventPayload` (`Scalar { signed }` \| `Record(RecordId)`); a record
payload makes the field a `std::vector<std::function<void(<RecordName>)>>`
and binds a record-typed `on`-handler argument so `t.field` reads resolve.
This mirrors v1's `payload_type_for_arg` exactly. New fixtures
`heartbeat_idle_test` and `wait_until_quiesce_test` trace-diff clean.

The **watchdog + periodic subset** (added 2026-06-13, same file,
divergence 18) lowers the `watchdog` lifecycle directive (spec §8.6) and
`on <N> cycles` periodic handlers (spec §7.10) in the agent/component
subset. A `watchdog` becomes a zero-arg `ComponentMethod` body (the
heartbeat statements) plus a `WatchdogSchema` (`period`/`max_idle` clause
exprs); the tbir backend installs a per-instance `_checkers` closure that
gates on a `static` last-fire stamp, runs the body, then the idle check
(`_last_in_cycle`/`_last_out_cycle` ≥ `max_idle` ⇒ `FAIL` + framework
error-counter bump).
An `on <N> cycles` handler lowers the same way (period-gated checker, no
idle check). `Expr::CycleCount` carries the framework `cycle_count` for
`${cycle_count}` diagnostics. New fixtures `watchdog_quiesce_test` (pass,
never trips), `watchdog_trip_diagnostic_test` (fail, trips from cycle
200), `agent_periodic_test` (pass, self-proving 3×) trace-diff clean.

The **env quiesced(N) + phase + data-scoreboard-sub subset** (added
2026-06-13, `src/ir/lower/{components,stmts,mod}.rs`, divergence 19)
closes the heartbeat/quiesce cluster's last three blockers:

1. **Data-only `scoreboard` as an env SUB-component** — a method-less
   `scoreboard` (a `ScoreboardSchema`, not a `ComponentSchema`) bound as
   an env field (`sb : DrainSb`). New IR
   `ComponentFieldKind::ScoreboardSub { scoreboard: ScoreboardId }`
   (distinct from `Sub`, which references a `ComponentId`). Field
   lowering resolves a `Named` type against the `scoreboard_ids` table
   before the component table. Access uses the nested run-scope path
   (`top.sb.expected.push(...)`, not `_tb.sb`): `Stmt::ScoreboardOp` and
   `Expr::ScoreboardQuery` carry a new `nested_path: Option<Vec<String>>`
   (`None` = the established `_tb.<field>` testbench-field form,
   `Some(path)` = the dotted env-nested path). The scoreboard struct now
   always emits the `_last_in/out_cycle` activity stamps (matching v1's
   unconditional `emit_scoreboard` — tbir previously omitted them, a
   latent divergence this slice also closes).
2. **`<env>.quiesced(N)`** — env-level aggregation, no new IR variant.
   `as_component_quiesced` walks the receiver's sub-component tree
   (`collect_quiesce_leaves`, mirroring v1's `collect_quiesced_paths`)
   and expands to an AND of `Expr::ComponentIdle { kind: Both }` over
   every LEAF (a component with no sub-components, or a `ScoreboardSub`),
   e.g. `top.quiesced(8)` → `top.prod.idle(8) && top.sb.idle(8)`.
3. **Named `phase <name>`** — orchestration, no new IR variant. Each
   `phase` block is collected into a name→block map; a `<name>()` call
   site in the run/check body is INLINED with the phase's statements
   (recursively; a cycle is rejected). v1 emits a captured `[&]() ->
   void` lambda + a plain call — observably identical, since the body
   runs at the call site inside the run/check coroutine.

Fixture `env_quiesced_phase_test` (`top_counter.sv`, pass) exercises all
three; trace-diff clean v1↔tbir at seed 1.

**Out of subset** (precise rejections): non-record/non-scalar event
payloads (enum / Vec / nested), `on <expr>` cycle-trigger handlers,
`tseq`. Those gate on later slices.

### The equivalence matrix

`tests/tbir_equiv_fixtures.txt` (append-only registry consumed by
`tests/run_tbir_equiv.sh` — wired into CI — and by the registry-driven
sweep in `tests/run_arch_dut_fixtures.sh`; schema v3 rows are
`test_name | top | sv_files | arch_dut | expect | extra_harc |
ref_src | test_struct`, the last three columns optional with `-` =
none — `extra_harc` joins additional `tests/fixtures/` files to the
.harc input list, `ref_src`/`test_struct` plumb `--ref-src`/`--test`;
5-column v2 rows parse unchanged):

| Fixture | Top | DUT | Expect | Exercises |
|---|---|---|---|---|
| `top_counter_test` | `Top` | `top_counter.sv` | pass | resets, loops, asserts, format args |
| `sync_fifo_test` | `TxQueue` | `sync_fifo.sv` | pass | covergroups, auto-cross, check phase |
| `bus_arbiter_test` | `BusArbiter` | `bus_arbiter.sv` | pass | multi-point covergroup, CovBin reads |
| `wait_until_counter_test` | `Top` | `top_counter.sv` | pass | wait-until single/all-of + timeout diags |
| `rom_lut_test` | `RomLut` | `rom_lut.sv` | pass | impure-helper CFG inlining ×8, coverage |
| `log_paths_test` | `Top` | `top_counter.sv` | pass | `logf` per-file streams, warn severities |
| `fatal_path_test` | `Top` | `top_counter.sv` | fail | `log(fatal)` → errors++ + `_fatal` loop exit |
| `cov_cross_bins_test` | `Top` | `top_counter.sv` | pass | declared `cross`, range + open-bound bins |
| `wait_any_of_test` | `Top` | `top_counter.sv` | pass | `any of` wait (untimed + timed, fires) |
| `wait_any_of_timeout_test` | `Top` | `top_counter.sv` | fail | `any of` timeout: "none of:" diags ×2 headers |
| `transaction_basic_test` | `Top` | `top_counter.sv` | pass | record declaration/defaults, let-site re-init in a loop, field reads/writes, inert keep/attr |
| `axilite_bus_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | bus bind, protocol signal access |
| `axilite_bus_extern_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | use-imported bus declaration |
| `axilite_bus_send_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | channel send/recv auto-handshakes |
| `tlm_method_blocking_bus_test` | `TlmMemory` | `TlmMemory.sv` | pass | blocking tlm_method → TransactorMethod call edge |
| `tlm_method_bus_test` | `TlmMemory` | `TlmMemory.sv` | pass | initiator-side `fork`/`join_all` (blocking + `out_of_order` tags) → `TlmFork`/`TlmJoinAll` |
| `cam_dual_basic_test` | `Mshr_Addr_Cam_Dual` | `cam_dual_basic.sv` | pass | transactor: void methods, sync waits, statement calls |
| `cam_value_basic_test` | `Tag_Value_Cam` | `cam_value_basic.sv` | pass | transactor: bool/wide-scalar params, reset pulse method |
| `cpu_pipeline_test` | `CpuPipe` | `cpu_pipeline.sv` | pass | transactor: wait-less methods (pure pokes) |
| `linklist_doubly_test` | `SchedList` | `linklist_doubly.sv` | pass | transactor: `-> T` methods, ternary over port reads, let-bound calls |
| `mac_table_test` | `mac_table` | `mac_table.sv` | pass | transactor: 48-bit args, lowercase module type |
| `noc_credit_test` | `NocCreditTop` | `noc_credit.sv` | pass | transactor: `for`-loop waits with param bounds |
| `buf_mgr_sm_test` | `BufMgrSm` | `buf_mgr_sm.sv` + 3 | pass | transactor: `if` inside `for` with waits, multi-file DUT |
| `buf_mgr_test` | `BufMgr` | `buf_mgr.sv` + 4 | pass | transactor: `while` over port read, `return dut.port`, assert in method |
| `scoreboard_basic_test` | `Top` | `top_counter.sv` | pass | scoreboard: queue push/pop/size/empty, scalar counter read/write, run↔check-shared `_tb` instance |
| `regblock_subset_test` | `Top` | `top_counter.sv` | pass | regblock: rw/ro/wo + reset, register-level frontdoor (mirror + Helper.write/read call edges), read-predict, WO mirror-read, test-scope-let helper |
| `regblock_basic_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | regblock: initiator-BFM `via` helper + register read in assert conditions AND `${...}` fail-message format args (`Expr::RegRead`, divergence 12) — one bus read per textual occurrence (eager cond, lazy fail-branch) |
| `regblock_bitbash_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | regblock: `bitbash(regs)` compile-time-unrolled walk-all over 3 RW regs (write/read ones+zero + compare; RO/WO skipped) + `assert errors == 0` (`Expr::ErrorCount`) |
| `regblock_fields_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | regblock: field-level decomposition (`regs.DMACR.RS`/`.MODE`) — masked RMW on the whole-register mirror + full-register bus write; shifted-extract read (`Expr::RegRead`); coexists with whole-register access (`regs.MM2S_SA`) (divergence 12) |
| `regblock_addrmap_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | addrmap: two `DmaChan` instances at distinct bases; 3-level `chip.inst.REG` + 4-level `chip.inst.REG.FIELD` access; per-instance shifted-offset mirror locals route each window independently (divergence 12) |
| `regblock_alias_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | addrmap `alias of`: `mm2s_view` shares `mm2s`'s mirror cell (one storage local) while issuing bus traffic at its own base — write via the alias moves the shared mirror (divergence 12) |
| `struct_basic_test` | `Top` | `top_counter.sv` | pass | struct: scalar fields (uint/sint/bool) + literal defaults, default-construct in a loop (re-init), field reads/writes in arithmetic/branch/assert/format args (reuses the transaction record machinery) |
| `regblock_record_api_test` | `AxiLiteRegs` | `AxiLiteRegs.sv` | pass | regblock: passive `record_write(addr, data)` (masked mirror write, constant-addr decode) + `let v = record_read(addr)` (mirror read), NO bus traffic, NO `on regs.REG` callback (divergence 12) |
| `tlm_pairing_arch_burst_target_test` | `TlmPairingArchBurstTarget` | `TlmPairingArchBurstTarget.sv` | pass | struct `Vec` field: HARC initiator calls a record-returning `tlm_method`, unpacks the `HarcBurstResp32x4 { data : Vec<uint<32>,4>; len; resp }` response pin via `harc_unpack_<R>`, reads `rsp.data[i]` |
| `tlm_pairing_arch_burst_initiator_test` | `TlmPairingArchBurstInitiator` | `TlmPairingArchBurstInitiator.sv` | pass | struct `Vec` field: HARC target responder builds the record field-wise (`rsp.data[i] = …`) and packs it onto the response pin via `harc_drive_<R>`; ARCH DUT initiator unpacks through its TLM return |
| `tlm_target_thread_test` | `TlmReadInitiator` | `TlmReadInitiator.sv` | pass | target-side TLM: blocking `thread bus.read` responder actor, single-cycle wait, value return |
| `tlm_target_thread_if_test` | `TlmReadInitiatorPair` | `TlmReadInitiatorPair.sv` | pass | target-side TLM: persistent state fields (read in body + from test), `for` loop, `if`/`else` return |
| `tlm_target_thread_runtime_loop_test` | `TlmReadInitiatorRuntimeLen` | `TlmReadInitiatorRuntimeLen.sv` | pass | target-side TLM: runtime `for i in 0..len` loop bound |
| `tlm_target_thread_early_return_test` | `TlmReadInitiatorRuntimeLen` | `TlmReadInitiatorRuntimeLen.sv` | pass | target-side TLM: early `return` from nested `if` inside a runtime loop |
| `tlm_target_ooo_lanes_test` | `TlmOooReadInitiatorPair` | `TlmOooReadInitiatorPair.sv` | pass | target-side TLM: `out_of_order tags N` RESPONDER lanes — per-tag dispatcher + N lane coroutines + arbiter; out-of-order completion (tag 1 before tag 0) |
| `tlm_pairing_arch_initiator_test` | `TlmPairingArchInitiator` | `TlmPairingArchInitiator.sv` (+ `.arch` sweep) | pass | target-side TLM: mixed `blocking` + `out_of_order tags 2` responders against an ARCH-authored OOO initiator |

(The registry has since grown past this table via the backfill sweep —
see [tbir-coverage.md](tbir-coverage.md); the registry file is the
source of truth, this table documents the construct-slice fixtures.)

`expect=fail` rows invert the verdict check: both codegens must exit
nonzero with a real `N TESTS FAILED` verdict (never `ALL TESTS
PASSED`), and the two traces must still trace-diff clean — a
deliberate failure has to fail *identically* under both emitters.

The original five fixtures and `transaction_basic_test` have
full-file insta snapshots locking
both the dump-ir text and the emitted tbir C++ (`tests/tbir.rs`,
`tests/snapshots/tbir__*`), so emitter refactors diff visibly; the two
log-path fixtures lock their dump-ir text.

## The behavioral gate vs the plan's byte-identical gate

[tb-ir-plan.md](tb-ir-plan.md) phase 4 prescribes a **byte-identical**
`.cpp` diff against the v1 emitter as the migration safety net, and is
explicit that a behavioral diff "hides shape differences". The MVP's
gate is behavioral instead: both codegens must print `ALL TESTS
PASSED` and their semantic JSONL traces must compare clean under
`harc trace-diff` (normalized — backend-implementation noise like
`seq` numbering is ignored; see `check_backends::diff_trace_strings`).

This is not a quiet weakening of the plan's gate; it is a different
deliverable. The plan's phase 4 migrates *v1's own emission* onto the
IR, where byte parity is meaningful and achievable. The MVP instead
added a **second, parallel backend** (`src/codegen/tbir/`) whose
function bodies are a loop-switch over `BlockId` (`while (!__done)
switch (__bb) { case N: ... }`) rather than v1's re-structured control
flow — byte parity with v1 is impossible by construction for that
shape, and forcing it would have meant writing a relooper before
writing anything else. The scaffolding (preamble, `HarcTestContext`,
clock scheduler, log/trace plumbing, dispatcher `main()`) does mirror
the v1 contract closely (see `src/codegen/tbir/mod.rs` and
`runtime.rs`), which is what makes the traces line up cycle-exactly.

> **Superseded 2026-06-12.** The plan's phase-4 gate was redefined to
> behavioral equivalence + full construct coverage (see the
> [tb-ir-plan.md](tb-ir-plan.md) decision log). The byte-parity
> requirement is dropped; this section is kept as the record of why the
> MVP's gate differed from the plan as originally written. Under the
> redefined gate, the tbir backend *is* the phase-4 deliverable, and
> what it owes is coverage (empty `Unsupported` list for v1's feature
> set, full fixture corpus in the equivalence registry), not parity.

## Documented divergences from tb-ir-design.md

Each item names the design-doc shape, the implemented shape, and the
reason. Code locations are authoritative.

1. **`PortRef.direction` / `PortRef.width` are `Option` and currently
   resolved only for probes.** The design says lowering resolves dotted
   DUT access against the DUT's port list (Verilator header on `--sv`,
   ARCH `.archi` on `--dut`) and produces a fully typed `PortRef`. The
   MVP lowering does not consult a DUT *port* table at all for
   architectural ports (`src/ir/mod.rs`, `PortRef` doc comment), so
   `direction` is always `None` and `width` is `None` for ordinary
   ports. Consequence: the width/direction half of design invariant 12
   is unimplementable today and the verifier does not attempt it.

   **`PortAccess` now flows (probe/force slice, 2026-06-13).** Probes
   and force points are in the subset: a `probe <name> : <T> at <path>`
   on `let dut` lowers a `dut.<name>` access to a `PortRef` with
   `access = Probe` (read-only) or `Force` (force-capable), and the
   probe's declared scalar width is recorded in `PortRef.width`. The
   tbir backend routes a `Probe`/`Force` access through the SV bind-stub
   accessor `dut->rootp-><DutType>__DOT__harc_probes__DOT__<name>`
   (mirroring v1's `Emitter::probes`), force writes lower to the
   `_drv`/`_en` pair, and `release dut.<probe>` lowers to a new
   `Stmt::ProbeRelease` that clears `_en`. The Probe-read-only /
   Force-write-only half of invariant 12 is enforced at lowering
   (`src/ir/lower/stmts.rs`): writing a read-only probe or releasing a
   non-force probe/ordinary port is a precise hard error. The SV stub
   itself (`__harc_probe_<DutType>.sv`) and its
   `V<DutType>___024root.h` include are shared by both codegens. See
   docs/probe-signals.md and the probe group in docs/tbir-coverage.md.
   Architectural-port `direction`/`width` remain `None` (unchanged).

2. **`Expr::Port` exists, with relaxed-but-checked positions.** The
   design's `Expr` deliberately has no DUT-read variant — `DutRead` is
   a `Stmt` and that is called the IR's load-bearing discipline. The
   MVP keeps `Stmt::DutRead` hoisting as the default rule but adds
   `Expr::Port`, permitted in exactly five positions (the
   port-position rule in `src/ir/verify.rs`): wait predicates (the
   scheduler must re-sample the DUT every cycle inside the predicate
   closure — a hoisted temp would freeze the value), format-arg
   expressions and `FailDiag` guards (v1 evaluates failure messages
   lazily at the failure site, after the wait has timed out),
   `DutWrite` values, and `AssertCheck` condition subtrees (v1 parity:
   the assert samples at check time). Everywhere else — `Assign`
   values, `Branch`/`WaitCycles` operands — an inline port read is a
   verify error and lowering hoists through a `DutRead` temp
   (`dut_read_in_let_hoists_to_dut_read_stmt` in `tests/tbir.rs`).

3. **No passes.** `src/ir/passes/` does not exist. `placement`,
   `lower_coroutine`, `randomize_analysis`, `hoist_stimulus`, and
   `extract_port_set` are all unimplemented, as is `TargetProfile`.
   The tbir backend consumes the raw CFG directly; the loop-switch
   shape is precisely what lets it skip `lower_coroutine` (no
   relooping needed — `co_await` inside a `switch` is legal C++20).

4. **Typed scalar locals preserve declared storage width.** Earlier
   MVP snapshots hoisted every IR local as `uint64_t` because the
   loop-switch backend needs locals to survive across `case` arms. The
   current lowering seeds `IrType` for explicitly typed scalar `let`s
   and helper parameters/returns, and the tbir backend emits
   width-faithful storage for them: `uint64_t` through 64 bits,
   `_harc_u128` for 65..128 bits, and `HarcWide<N>` beyond that. This
   keeps CVDP-style packed constants such as `uint<240>` tree vectors
   intact. Untyped integer lets still use the tbir scalar value model,
   so sign-sensitive untyped intermediates remain a residual area to
   cover with focused fixtures if they become observable.

5. **Invariant 8 amended in the verifier.** The design's literal text:
   "No block is empty unless its terminator is `Return` or `Jump`."
   That text contradicts the design doc's own worked example 2, where
   `b_header` is an empty block terminated by `Branch` — every loop
   header the lowering rules produce has that shape, and a loop body
   whose first statement is `wait N cycles` lowers to an empty block
   whose terminator *is* the content. `src/ir/verify.rs` (module doc)
   therefore permits empty blocks terminated by `Branch` or a
   suspension as well; only an empty `Fatal` block is flagged, because
   the design synthesizes the fail action into that block's statements
   — emptiness there means the synthesis dropped its body. Two more
   invariants need no runtime check at all: 5 (exactly one terminator)
   and 7 (no suspending `Stmt`) hold by construction of the
   `BasicBlock`/`Stmt` types.

6. **Pragmatic IR nodes not in the design.**
   - `Stmt::FailDiag { guard, args }` (`src/ir/mod.rs`): one
     `wait until ... timeout` diagnostic line. v1 bumps `errors`
     exactly once per timed-out wait — that bump rides the
     `WaitUntilTimeout` terminator's timeout edge — while the header
     and per-sub-predicate "not yet true:" lines print without
     bumping. A guarded/unguarded log-only statement is the smallest
     node that reproduces that contract; reusing `AssertCheck` would
     double-count errors.
   - `Stmt::CovReport` + `Expr::CovBin`: check-phase `cov.report()`
     and `cov.<point>.<bin>` reads. The design routed coverage through
     `Stmt::CoverSample(CovgroupId, Vec<Expr>)`; in the MVP, sampling
     is schema-driven at emission (the bin counters live in the
     emitted covergroup struct, sampled from a `_checkers` closure in
     registration order), and `SamplerAuto` function bodies are empty
     registration markers. Unknown point/bin names are hard lowering
     errors — v1 deferred those to a C++ compile failure.
   - `WaitUntil`/`WaitUntilTimeout` carry `Vec<PredSrc>` + `WaitMode`
     (`Single`/`AllOf`/`AnyOf`) instead of the design's single
     `pred: Expr`. `PredSrc` keeps each sub-predicate's pretty-printed
     source text so the timeout breakdown names the user's expressions
     exactly as v1 does (`AllOf`: one "not yet true:" line per
     still-false predicate; `AnyOf`: one "none of:" line listing them
     all, since a timed-out any-of means nothing fired). The
     `on_timeout` block also
     **rejoins `on_fire`** rather than terminating in `Return` as the
     design's synthesized-fail-handler rule prescribed — v1 semantics
     are log-FAIL, bump errors once, and continue the test.

7. **`TestSchema.clocks: Vec<ClockSpec>` with resolved `period_ps`.**
   The design references a `TestSchema` but never pins its fields.
   Codegen needs concrete picoseconds for the clock scheduler, so the
   schema carries each declared clock with its period resolved from
   the time literal or `domain ... freq_mhz` declaration at lowering
   time (`src/ir/mod.rs`, `ClockSpec`).

8. **`FmtArg.wide_hex: Option<(usize, bool)>`.** The design-era
   skeleton had a `bool`; emission of a `:WWx`/`:WWX` capture with
   WW > 16 needs the digit width and the case to route through the
   wide-hex runtime helper, so the flag is widened to carry both.

9. **`TbProgram.records` holds structural `RecordSchema`s; constraint
   metadata is carried as inert text, not a `ConstraintIrRef`.** The
   design sketches `records: ConstraintIrRef` ("owned by
   constraint-system layer"); the plan doc names the table
   `IndexVec<RecordId, RecordSchema>` "from constraint IR". What
   shipped is the structural half only: name + fields
   (type/default/`!` flag) in declaration order — exactly what the
   backends need to emit v1's value-record struct shape. `keep`
   clauses and `with [...]` field attributes are pretty-printed into
   inert strings (`RecordSchema::keeps`, `RecordFieldSchema::
   attr_src`) for dump-ir visibility; nothing may interpret them.
   `randomize` (#372) re-elaborates constraints from the AST through
   the constraint layer (`src/constraints`, `elaborate_constraints` →
   `CTypedProblem`); the randomize terminator carries a `ConstraintRef`
   handle into `TbProgram::constraint_sites` — the inert `RecordSchema`
   strings are *not* the seam (see divergence 13). Three
   subset edges, all explicit rejections (never silent): `when`
   subtype blocks (v1 flattens their fields into the struct;
   deferred until the tagged-ADT story lands with randomize),
   non-scalar (other than `Vec<scalar, N>`, which DOES lower as a
   `std::array<T, N>` member — the struct `Vec`-field slice) /
   wider-than-64-bit fields and non-literal defaults
   (v1 lowers enums/lists/wide ints; the tbir expression model is
   u64), and record locals in *pure* helpers (those emit as
   scalar-only file-scope C++ functions; impure helpers CFG-inline,
   where record locals are fine). Emission-side delta: tbir now emits the
   struct + `operator==`/`!=` + the `harc_pack_<R>`/`harc_unpack_<R>`/
   `harc_drive_<R>` pack helpers (matching v1's unconditional
   `emit_record_pack_helpers`) — v1 also emits `randomize_<T>`
   and pack/unpack helpers, which are unreachable dead text under
   this subset and land with their constructs (`randomize`, bus
   sends). One IR node carries the v1 timing contract:
   `Stmt::RecordInit` re-default-constructs at the source `let` site,
   because the loop-switch hoists declarations to function top while
   v1 declares in place — without it, a `let t : Txn` inside a loop
   would keep stale field values across iterations.
9. **Bus subset (2026-06-12): schema placement, two lowering shapes,
   and v1-surface boundaries.**
   - *Binding schema lives on `TestbenchSchema.bus_bindings`* (`field`
     = binding name = flat signal prefix, plus per-method
     `TlmMethodSchema { name, args, has_ret }`), not in a program-
     level `BusId` table as the design skeleton sketches. Bindings are
     per-test, and the schema is exactly what a backend needs to
     expand a `TransactorMethod` call edge into wire names — same
     "extended by compilation necessity" rationale as `ClockSpec`.
   - *Two lowering shapes, deliberately different.* Channel
     `send`/`recv` handshakes CFG-inline (they are pin-level protocol
     the TB performs itself; placement correctly sees cycle-anchored
     Tier-0 pin work). `tlm_method` calls stay **call edges** —
     `Assign(dest, Call(TransactorMethod, args))`, pinned by a new
     verifier rule (see `src/ir/verify.rs` module docs): whole-Assign-
     RHS position only, Run/Check functions only, binding + method +
     arity resolved against the owning testbench. The edge is the
     sanctioned exception to "no statement may suspend" — its
     suspension lives behind the call boundary, which placement
     classifies timing-tolerant by construction. The tbir backend
     expands the edge to v1's blocking req/rsp wire protocol
     (`emit_transactor_call` in `src/codegen/tbir/func.rs`),
     including both `tlm_call` trace events.
   - *`recv()` captures the first payload signal*, not v1's generated
     `<Bus>_<ch>_payload` struct. Observably equivalent for everything
     the IR can express: scalar reads see the first field either way
     (v1's struct converts implicitly to it), and named payload-field
     access (`v.resp`) is rejected at lowering, never mis-lowered.
   - *v1's call-position surface is kept*: bus calls lower only in
     `let`-RHS and statement position. `x = bus.m(...)` into an
     existing local is rejected (v1 errors on that form too);
     expression-nested calls get a precise rejection.
   - *`bind ... with { ch.sig: "port" }` signal remaps now lower* — the
     binding's `remap` table overrides the `<field>_<channel>_<signal>`
     convention at wire emission (see the bus-bind-remap slice).
   - *Initiator-side `fork`/`join_all` TLM issue now lowers* — a test-scope
     `let x = fork bus.m(args)` issues the request side as a `Stmt::TlmFork`
     and the matching `join_all` drains every pending fork as a
     `Stmt::TlmJoinAll` (issue-order FIFO for `blocking`, tag-routed lanes
     for `out_of_order tags N`); see the initiator-side fork/join_all slice.
   - *Target-side `out_of_order tags N` RESPONDER lanes now lower* — a
     `thread bus.m(...)` serving an `out_of_order tags N` method emits the
     per-tag dispatcher + N lane coroutines + arbiter topology, gated on
     `TargetTlmMethodSchema::ooo_tags`; see the TLM OOO-responder lanes
     slice.
   - *A `fork` INSIDE a transactor responder body now lowers* (downstream
     re-issue — nested forwarding); see the TLM responder nested-forwarding
     slice.
   - *Rejected at the bind/call site* (emission-side metadata the IR
     does not carry, or machinery deferred): bind-site generics
     (`Bus#(P=...)`), buses with `generate_if`-gated signals (gate
     evaluation needs the DUT-port param-override layering only
     `EmitOpts` has), and a direct (non-`fork`) `out_of_order` method call.

12. **Regblock subset (2026-06-12): register-level frontdoor over a
    transactor helper, with the mirror modeled as a synthetic record.**
    (`src/ir/lower/regblock.rs`, `RegblockSchema` in `src/ir/mod.rs`.)
    - *No new IR variants.* A `regblock R via <Helper> [width N]`
      lowers to a synthetic value-record (`TbProgram::records`, one
      scalar field per register, defaulting to its reset value) plus a
      `RegblockSchema` carrying offset/width/access metadata. The mirror
      local is an `IrType::Record`, so the host-side state rides the
      existing `RecordInit` / `RecordFieldWrite` / `Expr::RecordField`
      machinery — exactly the shape v1's `<Name>_Mirror` POD struct
      holds. Frontdoor traffic lowers to the existing
      `Stmt::TransactorCall` (`CallTarget::TransactorMethod`) edge.
      So no Stmt/Expr/Terminator variant was added; the verifier and
      both backends inherit regblock for free.
    - *Two access shapes lowered, register-level only.*
      `regs.NAME = v` → mirror `RecordFieldWrite` then a discarded
      `Helper.write(off, v)` call edge (RW/WO); RO suppresses the bus
      write (mirror update only). `let x = regs.NAME` → a
      `Helper.read(off)` call edge into the new local then a
      read-predict `RecordFieldWrite` (RW/RO); WO serves from the mirror
      with no bus traffic. This mirrors v1's `resolve_regblock_field_*` +
      `RegAccess::{reads,writes}_to_bus` emission and its read-side
      predict.
    - *Register reads only lower in `let`-RHS position.* v1 reads the
      bus inline at every read site (a C++ assignment-expression
      `(regs.NAME = read())` usable in any rvalue position, evaluating
      the message arm lazily). The IR's statement model can't represent
      that without a hoist that changes the bus-read count, so a
      register read in an assert condition, log/fail message, or branch
      operand is an explicit `Unsupported` — never silently rewritten to
      a mirror read (the mirror IS a record local, so the record-field
      path would otherwise pick it up). Hoist the read into a `let`
      first.
    - *Test-scope unbound-transactor helper.* v1 routes the regblock
      frontdoor only through a helper that lives in its `let_types`
      (i.e. a test-scope `let h : Helper active`), not a testbench
      field. The IR previously modeled transactor instances only as
      testbench fields, so this slice also added test-scope-let
      transactor instances (`let h : Xactor active`, accessed by bare
      name — the impl-for desugaring leaves test-scope lets unqualified,
      where testbench fields become `_tb.<field>`), merged into the same
      `transactor_fields` machinery (`bare_transactor_fields` records
      which shape to expect). The mirror is **run-scoped**: a
      check-phase regblock access fails the binding lookup and is a
      precise rejection, like a test-scope let.
    - *Rejected, never mis-lowered (at this first slice):* the
      **bus-bound `via` helper** (`transactor H bound to BusT` — the
      dominant residual blocker for the corpus `regblock_*` fixtures,
      whose method bodies resolve `bus` against a test-scope bus
      binding), field-level decomposition (`regs.REG.FIELD`),
      `bitbash(regs)`, the passive `record_write`/`record_read` API,
      per-register `on regs.REG` callbacks, `addrmap` composition (incl.
      `alias of`), non-literal register offsets/reset values, and
      >64-bit register widths. Each is an `Unsupported` naming the
      deferred feature. **Subsequently closed:** the bus-bound helper
      (initiator-BFM slice), `bitbash` + register-read-in-assert
      (regblock-residuals slice), and field-level decomposition +
      `addrmap` + `alias of` (field-level/addrmap slice, 2026-06-13).
      Only the passive `record_*` API and `on regs.REG` callbacks remain
      rejected. New fixture:
      `regblock_subset_test` (`top_counter.sv`, pass) exercises
      rw/ro/wo + reset values + mirror predict + WO mirror-read +
      test-scope-let helper routing, registered in the equivalence
      registry.

10. **Transactor instance state — scalar STATE fields now materialize
    (2026-06-13); heartbeat stamps + pre/post hook vectors still don't.**
    v1 emits a per-transactor C++ struct (DUT pointer member + heartbeat
    fields + state fields), a testbench member for each instance,
    `<Type>_<method>_pre`/`_post` hook vectors with fan-out loops in
    every method lambda, and the `xact.dut = dut` pointer copy.
    *Scalar state fields* (`last_read : uint<32> default 0`) on the
    UNBOUND DUT-poking transactor, the bound-to TARGET responder (#371),
    AND the bound-to INITIATOR BFM (bound-initiator-state slice,
    2026-06-13) now lower: each stateful instance gets a per-instance
    state struct (`runtime::target_state_struct_inst`, the same
    machinery across all three forms), bare-name reads/writes in method
    bodies lower to `Expr::TransactorState`/`Stmt::TransactorStateWrite`
    (filled with the instance name at test-bind), and the test reads them
    back via `xact.last_read`. For the bound-to initiator the state
    struct coexists with the bus-prefix fill: the same test-bind loop in
    `lower/mod.rs` that registers the instance and fills the type-shared
    method bodies now also covers `bound_bus.is_some()` BFM instances
    (the only such entries in `transactor_fields` are bound-INITIATORs;
    bound-TARGETs live in `target_tlm_actors`, not `transactor_fields`),
    so a bound-initiator `read` body can both drive `bus.r.recv()` and
    cache the readback in `last_read`. The method lambdas still take no
    `self` — the body references `<instance>.<field>` directly (filled at
    bind), so the subset is ONE stateful instance per transactor type per
    file (the bodies are type-shared); a second is rejected precisely.
    An event/directional field (`req : in event<T>`) on a bound-to
    transactor is still rejected — that is the bound-to event-driven
    driver form (divergence 17 residual), a separate slice. Still
    NOT emitted: the `_last_in/out_cycle` HEARTBEAT stamps are carried on
    the struct but unread (`idle()` predicates are out of subset), and
    statement-level `on obj.method pre/post` HOOK vectors are rejected at
    lowering (so they would be permanently empty). `Stmt::TransactorCall`
    emits a direct `<Type>_<method>(args)` call. Observable only on
    broken programs: a method call without a preceding `xact.dut = dut`
    null-derefs under v1 but drives the DUT under tbir (lowering
    validates the bind statement *when present* — binding anything other
    than the test DUT is rejected).

11. **`sim_end` clock attribution when the run body never suspends.**
    Found via the ternary addition: `linklist_basic_test` now lowers
    and matches v1 on verdict and on every trace event except the
    final `sim_end`, whose `clock`/`clock_cycle` fields are `""`/`0`
    under v1 but `"clk"`/`N` under tbir. Root cause is the
    helper-handling split (divergence of PR #350, latent until now):
    that fixture's `run` body contains no top-level wait — every wait
    lives inside impure helpers, which v1 emits as synchronous
    lambdas — so under v1 the entire test executes inside
    `sched.bootstrap()` and the pre-loop settle dump
    (`_harc_trace_dump_at(now_ps, "", 0)`) is the *last* timing
    update before `sim_end`. Under tbir the helpers are CFG-inlined,
    the run coroutine suspends for real, and the drive loop's final
    edge stamps `"clk"`. Verdicts and all other events are identical;
    the fixture stays **unregistered** until the trace-diff
    normalization (or v1's pre-loop stamp) is reconciled.
10. **Singleton-batch notes (2026-06-12).**
    - *`Terminator::WaitCyclesSync`* — a `wait N cycles` that
      originated inside an inlined helper / testbench-method body
      emits as v1's synchronous `for (...) tick()` loop, not
      `co_await`. v1 emits helper bodies as plain lambdas whose waits
      never yield, so a helpers-only test body completes inside
      `sched.bootstrap()`; emitting `co_await` instead left a real
      trace delta (the final `sim_end` event's clock attribution) on
      `linklist_basic_test`. Cycle counts and checker observations
      are identical either way; the sync form mirrors v1's execution
      structure exactly.
    - *Test-scope `let`s* lower as run-function locals initialized at
      entry. v1 hoists them to `main` scope and the run/check
      coroutine captures them by reference — shared state across
      phases. The IR's run and check are separate functions, so a
      test-scope let written in `run` and read in the `check`/`teardown`
      phase is **auto-promoted** to a `_tb` scalar host field (#444): its
      `let` declaration is dropped, reads lower to `Expr::TbField` and
      writes to `Stmt::TbFieldWrite`, so the value persists across the
      run→check boundary inside the shared `_tb` struct — trace-equivalent
      (a `_tb` field write emits no trace event, exactly like a v1
      shared-scope local mutation). The promoted field's `default` is the
      let's constant initializer, so a **non-constant-init** check-read let
      is rejected precisely (assign the computed value in the `run` body
      instead), and a `let` declared inside the check/teardown body
      shadows — and therefore does not promote — a same-named test-scope
      let. The closure-hook path (divergence 20) uses the same promotion.
    - *Ternary port hoisting*: in positions where ports must hoist
      (e.g. `let` RHS), both arms' DUT reads hoist eagerly, while
      v1's C++ `?:` reads the taken arm only. DUT port reads are
      side-effect-free and untraced, so the difference is
      unobservable; in port-allowed positions (assert conditions,
      format args, wait predicates) the ternary emits as `?:` with
      inline reads, byte-equivalent to v1.
    - *`const` initializers* are restricted to plain integer literals
      (v1 forwards arbitrary exprs into a C++ `constexpr`); wider
      shapes are explicit rejections until needed.
    - *Width methods* cover 1..=1024 bits (#434, #537): casts ≤ 64 bits
      target `uint64_t`, 65..128-bit casts target v1's `_harc_u128`
      (`cpp_uint_for_width`) — trunc via `harc_rt::harc_trunc_u128`,
      sext via `harc_rt::harc_sext_u128`, zext/resize via a plain
      `_harc_u128` cast. Wider targets use the `HarcWide<N>` word-array
      helpers, including exact masks for partial final words. `zext` on an
      unknown-width receiver is a plain cast — v1's exact shape, including
      its documented "assume the receiver already fits" looseness. The
      `1..=1024` ceiling is on the cast **destination** only: a
      `WidthCast`'s `src_width` is best-effort receiver metadata read off
      a declared type, which the language does not bound (a
      `uint<2048>` local narrowed by `.trunc<64>()` is legal), so the
      verifier checks it for nonzero and nothing more. Lowering reports an
      unusable declared width (`uint<0>`) as `None`, never `Some(0)`.
    - *Lane indices* on DUT `Vec` ports may be compile-time constants
      OR runtime expressions (#433): a `PortRef` lane carries a
      `LaneIndex { Const \| Var }`, and a `Var` index renders through the
      regular value-expression path (v1's `dut_packed_lane`), so both
      `dut.data[2]` and `dut.data[i]` lower for read and write. Covergroup
      lane points stay constant-only (the schema lowers before any runtime
      scope); a runtime cover lane is a precise codegen error.

12. **Scoreboard data-only subset (2026-06-12).** The design doc's
    `Stmt::ScoreboardOp(ScoreboardId, ScoreboardOp)` is implemented
    with `ScoreboardOp` as `QueuePush`/`QueuePop`/`ScalarWrite` (plus
    a sibling `Expr::ScoreboardQuery` for the value-producing reads
    `size`/`empty`/scalar). The implemented op carries the resolved
    `field` (testbench-field name) alongside the `ScoreboardId` so
    emission resolves `_tb.<field>` without a second lookup — a
    compilation-necessity extension of the design's two-tuple. The
    subset is **data-only**: a scoreboard is a struct of scalar
    counters and `queue<T>` FIFOs of a scalar element type, held as a
    `_tb` member, mutated directly from the run/check body (v1's
    `emit_scoreboard` shape). Scoreboard `hookable`/`function`
    **methods** are NOT lowered — they mutate scoreboard instance state,
    which would need the same per-instance materialization deferred for
    transactor state (divergence 10); a method-bearing scoreboard is
    rejected at the declaration so it never lowers to a struct missing
    its methods. Event-driven `on`/`connect` wiring (gates on the
    agent/env/event slices) is likewise rejected at the site. A
    `queue<Struct>`/`queue<transaction>` element now lowers (#431, via the
    shared `lower_queue_elem` record-resolution seam); a >64-bit scalar
    queue element and an enum/Vec/nested element type stay rejected at the
    field. No divergence in
    observable behavior for the covered subset: `scoreboard_basic_test`
    trace-diffs clean against v1.

13. **Target-side TLM / bus-bound transactor (2026-06-12).** A
    `transactor X bound to <Bus>` whose body is one or more `thread
    bus.<method>(...)` responder threads now lowers (blocking methods
    only). Each thread lowers to a `TbFunction` (kind `TransactorBody`),
    `params` = the method args; persistent scalar state fields
    (`read_count : uint<32> default 0`) carry on `TransactorSchema::
    state_fields`. Two new IR nodes model state access:
    `Expr::TransactorState { instance, field }` and
    `Stmt::TransactorStateWrite { instance, field, value }` — host state,
    allowed wherever a `Local` is. Inside the responder body the
    `instance` is lowered as an empty placeholder (the bind is not yet
    known) and filled at the test-binding stage
    (`fill_transactor_state_instance`); the subset has exactly one
    `passive` instance per bound transactor per file, so the fill is
    unambiguous. Emission (`emit_target_actor`, mirroring v1's
    `emit_bound_tlm_target_actors` blocking path) generates a test-scope
    per-instance state struct (state fields + `_last_in/out_cycle`
    activity stamps) plus one background-coroutine actor per target
    method: hold `req_ready=0/rsp_valid=0`, await `req_valid&&req_ready`,
    capture args, tick, drop `req_ready`, trace `request`, run the body
    loop-switch (a real coroutine — its waits `co_await` the scheduler),
    drive `rsp_data`, trace `response`, raise `rsp_valid`, await
    `rsp_ready`, tick, drop `rsp_valid`. Trace payloads match v1 exactly
    (`tlm_call(cycle, instance, "bus", method, phase, "target")`), so the
    four registered fixtures trace-diff clean. `bind ... with { ... }`
    signal remaps on a target responder now lower (the actor receives the
    test's bus bindings and routes its wires through `wire_name`; see the
    bus-bind-remap slice). Target-side `out_of_order tags N` threads
    (tagged RESPONDER lanes) now lower too — see the TLM OOO-responder
    lanes slice — as does a `fork` inside a responder body (a responder
    re-issuing a downstream TLM call — forwarding; see the nested-forwarding
    slice). (Initiator-side `fork`/`join_all`
    over bus methods — test-scope `let x = fork mem.read_ooo(...)` —
    lowers; see the initiator-side fork/join_all slice.) The
    complementary *initiator-side* bus-bound BFM
    (`hookable` bodies driving handshake channels — the regblock `via`
    helpers) landed 2026-06-13; see divergence 15.

14. **Env-composition (analysis-connect) subset (2026-06-13).** The
    env/agent cluster's flat-struct core lowers
    (`src/ir/lower/components.rs`, `ComponentSchema` in `src/ir/mod.rs`).
    - *Three source shapes, one schema.* A method-bearing `scoreboard`,
      an analysis-source `transactor` (`out event` + `emit`, no DUT
      field), and an `env` composing them all lower into
      `TbProgram::components` (`ComponentSchema`), routed there BEFORE the
      `ScoreboardSchema`/`TransactorSchema` loops by
      `components::{scoreboard_is_component, transactor_is_component}`.
      This mirrors v1, which treats env/agent/scoreboard/transactor
      uniformly as `ComponentDecl`s (`synth_component_from_transactor`).
    - *Component instance state IS materialized* — the divergence-10
      reason a data-only scoreboard could not carry methods. A component
      is a plain C++ struct of its fields; a method is a free
      `<Comp>_<method>(<Comp>& self, args)` lambda whose body addresses
      fields self-relatively. So scoreboard methods (the residual the
      scoreboard slice flagged) lower here, not on `ScoreboardSchema`.
    - *`connect` resolves at lowering* into `ConnectEdgeSchema`s on the
      env's `ComponentSchema`; emission wires them as v1's
      `<env>.<src>.<event>.push_back([&](auto _t){ <Sink>_<m>(<env>.<sink>,
      _t); })` at the env local's construction. `emit observed(v)` fans
      out over `self.<event>` plus the `_last_out_cycle` heartbeat bump,
      exactly v1's emit lowering.
    - *Test-scope binding only.* A composite env binds as `let env :
      <Env>` (a run-scope local, run/check-shared — v1's `AnalysisEnv
      env;` placement). A composite-component *testbench field* (e.g. a
      method-bearing scoreboard bound `sb : Sb`) is a precise
      `Unsupported` at the field — it would otherwise mis-lower to the
      "assume DUT module type" arm. The impl-form env-typed testbench
      field (`top : HeartbeatEnv`) is the agent-slice form, not here.
    - *Rejected, never mis-lowered (at this slice):* `agent` declarations
      and `on <ev>` handlers, `sequencer`/`tseq`, watchdog/phase
      orchestration, `idle(N)`/`quiesced(N)` predicates, dotted-path `emit
      top.prod.in_ev(t)` (cross-component emit), bus-bound source
      transactors, generics, non-scalar event payloads. New fixture:
      `analysis_sink_connect_test` (`top_counter.sv`, pass) — env + two
      method-bearing scoreboards + analysis source + two connect edges,
      registered in the equivalence registry. **(`agent`, `on <ev>`,
      test-scope path-emit, and `idle*` since lift in divergence 15.)**
15. **Agent composition + `on <ev>` handlers (2026-06-13).** Extends
    divergence 14 with the agent capability the env-composition slice
    flagged as residual (`src/ir/lower/components.rs`).
    - *`agent` is a component.* `ComponentKindTag::Agent`; an `agent`
      lowers through the same `ComponentSchema` path as an env
      (`CompSource::Agent`), bound test-scope as `let <name> : <Agent>`.
      Directionless `event<scalar>` self-events join the `out event`
      analysis-port form on `ComponentFieldKind::Event`.
    - *`on <ev>(arg)` → subscriber + one-param method.* Each handler
      lowers as a one-param `ComponentMethod` (the event arg) recorded in
      `ComponentSchema::on_handlers` (`OnHandlerSchema`). At component
      construction it registers as a `push_back` closure on the event
      field that bumps `_last_in_cycle` (activity tracking) then runs the
      `<Comp>_on_h<fid>` body lambda — v1's `on`-subscriber shape.
      Registration recurses into by-value sub-components (an env holding
      an agent). Only bare `on <event>(arg)` self-subscriptions lower;
      `pre`/`post` hooks, `on <expr>` cycle-triggers, and `on <N> cycles`
      periodic forms are precise `Unsupported` (later slices).
    - *`emit` gains a base.* `Stmt::ComponentEmit` carries
      `base: ComponentBase`: self-relative `emit observed(v)` inside a
      body (`SelfField`) and test-scope path `emit tagger.in_ev(v)`
      (`Path`) both lower, fanning out over `<base>.<event>` plus the
      emitter's `_last_out_cycle` bump.
    - *`idle` predicates.* `agent.idle_in(N)` / `.idle_out(N)` / `.idle(N)`
      lower to `Expr::ComponentIdle { base, kind, n }`, emitted as v1's
      `emit_idle_predicate` (`cycle_count - _last_{in,out}_cycle >= N`).
      A user `hookable` of the same name still wins in v1; in this IR
      subset agents carry no such override, so the built-in always applies.
    - *Rejected, never mis-lowered:* `event<Struct/transaction>` payloads
      (now LIFTED — divergence 17 lowers record-payload events),
      composite-component *testbench fields* (now LIFTED — divergence 16
      lowers `prod : Producer` testbench-field binding),
      `quiesced(N)` (env heartbeat aggregation), `watchdog`, named
      `phase`, `wait until` with heartbeat predicates, `sequencer`/`tseq`
      (`sequencer` has since lifted — divergence 16; `tseq` still pends).
      New fixture: `agent_on_handler_test` (`top_counter.sv`, pass) —
      agent + on-handler + path-emit + `idle_in`, registered in the
      equivalence registry; trace-diffs clean v1↔tbir at seed 1.
14. **Randomize seam carries AST constraints (2026-06-13).** The design
    pins `ConstraintRef` as "a handle into the constraint IR, not a
    copy." The shipped `ConstraintRef` IS a pure handle — `ConstraintRef(u32)`
    indexing `TbProgram::constraint_sites` — and the *true* solver handle
    it pairs with is `ConstraintSite::problem_id`, the
    `ConstraintProblemId` into `build_typed_solver_problem_table`. **But**
    each `ConstraintSite` also carries the AST `target` expression and the
    merged AST constraint set (`Vec<ast::Expr>`). This is a deliberate
    divergence from the IR's otherwise AST-free discipline: v1's Z3-solve
    codegen (`emit_constraint_solver_block`) is AST-driven, and this slice
    *reuses it verbatim* ("the constraint runtime is shared; only the call
    site moves to the IR backend") rather than reimplementing the solver
    over the typed constraint IR. The tbir backend builds a focused v1
    `Emitter` (`build_randomize_emitter`) and splices the per-site snippet
    (`emit_randomize_snippets`, indexed by `ConstraintRef`) into the
    loop-switch. Consequences: (a) the IR core gains one `ast::Expr`
    dependency, confined to `ConstraintSite`; (b) the target local's
    emitted C++ name must equal its source name (true for all record
    locals — they are never on the loop-switch `RESERVED` list); (c)
    transaction `keep`s are merged into the site at lowering (spec §4),
    so the v1 dispatch runs keep-free of its own merge. When the typed
    constraint IR grows a complete AST-free Z3 emitter, this AST payload
    can be dropped and `emit_randomize_for_site` retargeted at
    `problem_id` alone. Verifier invariant 9 is now checked (the
    `ConstraintRef` resolves and the target is record-typed —
    `DanglingConstraintRef`). `keep_constraints_test` and
    `axilite_constraint_test` trace-diff clean v1↔tbir.

15. **Initiator-side bus-bound BFM (2026-06-13).** The complement of the
    target-side responder (divergence 13): a `transactor X bound to
    <Bus>` whose `hookable write(addr,data)` / `read(addr)->data` methods
    DRIVE the bound bus through handshake channels now lowers (this is the
    regblock `via <Helper>` form and the TLM-initiator BFM). Dispatched
    from `lower_transactor` by item shape — a bound-to transactor with any
    `hookable` is the initiator form, one with `thread bus.<m>(...)`
    bodies is the target form (a file mixing both is rejected).
    - *No new IR variants.* Each `hookable` lowers like the unbound
      DUT-poking BFM — a `TbFunction` (kind `TransactorBody`) recorded on
      `TransactorSchema::methods`, with `bound_bus = Some(<Bus>)`. So a
      regblock frontdoor's `Helper.write`/`read` call edges (#369,
      divergence 12) and bare `helper.method(...)` calls resolve through
      the existing `CallTarget::TransactorMethod` dispatch; the tbir
      backend emits the method via `emit_method` (the synchronous-hookable
      lambda — waits are `tick()` loops), and the regblock mirror +
      read-predict ride unchanged.
    - *`bus` resolves via a placeholder-keyed binding.* The method body
      is lowered before the test's `let helper = bind <axil>` names the
      binding, so the body's `bus_bindings` map is keyed by the bare `bus`
      keyword (`transactors::INITIATOR_BUS_PLACEHOLDER`, matching v1's
      `driver_bus_for_hookables` where `bus` inside a hookable resolves to
      the parent's binding). Every `bus.<ch>.send/recv` /
      `bus.<ch>.<sig>` access lowers through the **existing** channel-
      handshake machinery (CFG-inlined 16-cycle-budget valid/ready dance);
      the resulting `PortRef`s carry `bus` as their flat prefix. At test-
      binding time `fill_initiator_bus_prefix` rewrites that prefix to the
      real binding name (`axil` → `axil_aw_valid`, the arch-com §19.6 flat
      name). The bodies are shared per transactor TYPE, so the subset is
      one bound instance per type per file — a second bind to a different
      binding is rejected (mirrors the target-responder one-instance
      gate).
    - *`recv()` field access.* v1 captures the whole `<Bus>_<ch>_payload`
      struct and reads `.data`/`.resp` off it. The IR's scalar model
      captures the FIRST payload signal into the bound local (preserving
      bare-scalar `let v = bus.r.recv(); v == ...`) AND each remaining
      payload signal into a `<recv>__<field>` local at recv time,
      recorded in `recv_payloads` so a later `r.<field>` read resolves to
      the matching local — same capture cycle as v1.
    - *Out of subset* (precise rejections): per-instance BFM state
      fields, `out_of_order` channels, `fork`-issue, nested transactor
      calls inside a BFM body, and a second bound instance of one BFM
      type. (The bus-bind-remap slice landed `bind ... with` overrides
      for the test-scope TLM call edge and the target responder; an
      initiator BFM's *own* bound bus resolves through the placeholder
      prefix, not `wire_name`, so a remap on it stays unmodeled.) New
      fixture:
      `regblock_access_test` (`AxiLiteRegs.sv`, pass) — register-level RW/
      RO/WO frontdoor over the bus-bound `via` helper, trace-diff clean
      v1↔tbir.

16. **Composite-component testbench-field binding (2026-06-13).** The
    complement of the test-scope `let env : <Env>` binding (divergences
    14/15): a component bound as a `testbench` FIELD — `prod : Producer`
    / `sb : Sb` / `top : HeartbeatEnv` declared inside the `testbench`
    block alongside `dut : Top`, driven by an `impl ... for` body. This
    was the *first* blocker flagged by the env/agent slices for the 5
    heartbeat/quiesce fixtures.
    - *No new IR variants — same instance model as a test-scope let.* The
      testbench-field walk in `lower/mod.rs` routes a component-typed
      field (resolved through `component_ids`) into the SAME
      `test_scope_components` collector a test-scope `let` uses, so it
      flows into `ComponentFieldBinding` / `LowerCtx::component_fields`
      and lowers to a default-constructed run-scope instance with its
      `connect`/`on` wiring. `validate_testbench_component` now ACCEPTS a
      component-typed field (a `mode` keyword on one is rejected — that
      keyword is a transactor concept). A method-bearing scoreboard /
      analysis-source transactor lands here (it lives in `component_ids`,
      NOT `prog.scoreboards`/`prog.transactors`), so it routes to the
      component path, not the data-only scoreboard/transactor routes.
    - *`_tb`-prefix stripping.* v1 holds a testbench-field component on
      the `_tb` struct (`_tb.prod`), and the shared impl-for desugaring
      rewrites `prod.in_ev` → `_tb.prod.in_ev` for BOTH codegens. tbir
      instead emits every component at run scope (bare `prod`), so a new
      `FuncBuilder::strip_tb_prefix` helper (`components.rs`) drops a
      leading `_tb` segment (only when it matches `ctx.tb_field` AND a
      real `component_fields` entry follows — a user component literally
      named `_tb` is untouched) in every component-access path:
      `as_component_method_call`, `as_component_field_{target,read}`,
      `lower_emit`, `as_component_idle`. `as_port_ref` skips a
      `_tb.<component>` root so a component access is never mis-read as a
      DUT port. Both binding shapes therefore resolve to the same
      `ComponentBase::Path` rooted at the bare name — IR identical to the
      test-scope-let form. **This is the C++-shape divergence: v1's
      `_tb.prod` member vs tbir's run-scope `prod` instance — same trace
      behavior, verified by trace-diff.**
    - *Hardening: `event<transaction/struct>` payloads reject precisely.*
      `event<TinyTxn>` parses the payload as `TypeArg::Expr`/`Named` (a
      bare type name — every scalar payload `uint<W>`/`bool` parses as
      `TypeArg::Type`). The IR event model carries a single ≤64-bit
      scalar, so a struct payload previously fell through to an
      unsigned-scalar callback and failed at C++ compile (`emit
      prod.in_ev(t)` passing a `TinyTxn` to a `std::function<void(u64)>`).
      Component-schema lowering now rejects it at the source with a
      precise `Unsupported` (transaction-payload events gate on a later
      slice).
    - *Out of subset* (precise rejections, the residual stack behind the
      binding for the 5 target fixtures — `event<transaction>` payloads
      have since LIFTED in divergence 17): `watchdog` (watchdog_quiesce
      / watchdog_trip_diagnostic), `quiesced(N)` env aggregation, named
      `phase`, `on <N> cycles`, and a `queue`/data-only-`scoreboard`
      SUB-component inside an env (env_quiesced's `DrainSb` holds
      `expected : queue<uint<8>>`). New self-proving
      fixture: `tb_field_agent_test` (`top_counter.sv`, pass) — the
      `agent_on_handler_test` agent bound as a testbench field instead of
      a test-scope let; registered in the equivalence registry,
      trace-diffs clean v1↔tbir at seed 1.
16. **Sequencer construct (2026-06-13).** A `sequencer` is the stimulus-
    source half of the UVM sequencer/driver pattern. Structurally it is
    the analysis-source component shape the env-composition slice already
    lowers: an `out event<T>` analysis port plus `hookable` methods that
    generate a stream and `emit` each item on that port. It therefore
    reuses the existing `CompSource`/`ComponentSchema`/`ComponentMethod`
    machinery wholesale.
    - *Only addition is a tag.* `ComponentKindTag::Sequencer` (+
      `CompSource::Sequencer`) — used only for dump-ir / diagnostics and
      the testbench-field precise-rejection set. No new IR variant, no new
      statement/expr form, no new codegen path: sequencer methods, `emit`,
      and the env's `connect <sqr>.<event> -> <drv>.<sink>` bridge all
      flow through the same lowering as a method-bearing scoreboard or
      analysis-source transactor. The connect bridge feeds the emitted
      stream into the sink (here a scoreboard tally standing in for a
      driver's `req` sink).
    - *Binding scope.* A sequencer binds as an env/agent sub-component or
      a test-scope `let`; a sequencer **testbench field** is a precise
      rejection (it joins `component_type_names`), mirroring divergence 14
      for other composite components.
    - *Fixture:* `sequencer_connect_test` (`top_counter.sv`/
      `top_counter.arch`, pass) — a `dispatch(n)` hookable emitting over a
      literal-range `for i in 0 .. n` loop, connected to a scoreboard
      sink; trace-diff clean v1↔tbir at seed 1.
    - *Out of subset (corpus residuals).* The three corpus sequencer
      fixtures (`axilite_connect_test`, `transactor_agent_mode_test`,
      `transactor_env_mode_test`) each lower their `sequencer` now but
      stop at the **next** blocker: every `dispatch` body iterates a
      `tseq` (`for t in <TSeq>` over a `let txns = RandomTxns(5)`, with
      `randomize(t)` inside the `tseq`), which needs the `tseq` /
      `TSeq<T>`-value / `randomize-in-tseq` slice. The agent/env fixtures
      additionally stack mode-inheritance (`active`/`passive` flowing
      through env→agent→transactor) and cycle-trigger `on dut.x && dut.y`
      handlers, which need the agent-mode + cycle-trigger slices. None of
      the three fully unlock from the sequencer construct alone.
17. **Event record payloads (2026-06-13).** Lifts the `event<transaction>`
    / `event<struct>` rejection the testbench-field-binding slice
    introduced (#376, a soundness measure). A non-scalar analysis-port
    channel now carries a value-record payload by value.
    - *IR change.* `ComponentFieldKind::Event { signed: bool }` becomes
      `Event { payload: EventPayload }` where `EventPayload` is
      `Scalar { signed }` or `Record(RecordId)`; `OnHandlerSchema::
      arg_signed: bool` becomes `arg_payload: EventPayload`. All exhaustive
      matches (display, lowering, runtime codegen) updated.
    - *Lowering.* `lower_event_payload` (`src/ir/lower/components.rs`)
      resolves the `event<T>` arg against the `record_ids` table — a
      known transaction/struct name (parsed as `TypeArg::Type(Named)` or
      `TypeArg::Expr(Ident)`) → `Record`, a scalar ≤ 64 bits → `Scalar`.
      A named type that is neither (enum / Vec / nested) and the
      keyword-style `TypeArg::Named` are rejected precisely. An
      `on in_ev(t)` handler over a record event binds a record-typed
      param (`IrType::Record(rid)`), so `t.field` reads resolve against
      the schema; `emit prod.in_ev(t)` carries the record local.
    - *Codegen.* Mirrors v1's `payload_type_for_arg`: the event field is
      `std::vector<std::function<void(<RecordName>)>>`
      (`runtime::event_payload_cty`) and the component-fn lambda renders
      a record param by value (`func.rs`). The `[&](auto _t)` subscriber
      and `connect` lambdas were already payload-generic. **This is a
      type-name divergence only (scalar `uint64_t`/`int64_t` vs the record
      struct); same trace behavior, verified by trace-diff.**
    - *Fixtures unlocked* (both `top_counter.sv`, pass, trace-diff clean
      v1↔tbir at seed 1): `heartbeat_idle_test` (agent + record-payload
      event + `on` handler + `idle_in` poll) and `wait_until_quiesce_test`
      (same + `wait until all of … timeout`, already supported). Registered
      in the equivalence registry.
    - *Out of subset* (still rejected, behind this slice): `watchdog`
      (watchdog_quiesce_test), a data-only `scoreboard` SUB-component in an
      env + named `phase` + `quiesced(N)` (env_quiesced_phase_test), and
      non-record/non-scalar event payloads (enum / Vec / nested).

17. **`tseq` (transaction-sequence) construct (2026-06-13).** A `tseq`
    is a named generator of a sequence of transaction values, iterated
    with `for t in <TSeq>`. v1 (`cpp_tb::emit_tseq`) lowers it to a
    `[&]`-capturing lambda filling a `std::vector<T> _result` via `yield
    t` and returning it; `for t in <TSeq>` then range-iterates the
    vector. The TB-IR mirrors this with one element-record-typed
    sequence and the existing randomize seam — `src/ir/lower/tseqs.rs`.
    - *New IR (minimal).* `IrType::RecordSeq(RecordId)` (a
      `std::vector<Record>` local), `FunctionKind::Tseq { record }` (the
      generator body, whose `ret` slot is the RecordSeq accumulator),
      `CallTarget::Tseq(name)` (the generator call edge),
      `Stmt::SeqPush { seq, value }` (`yield t`), and two value forms for
      iteration: `Expr::SeqLen(seq)` (`seq.size()`, the loop bound) and
      `Expr::SeqIndex { seq, index }` (`seq[i]`, the record-valued
      element). All exhaustive matches (display/verify/passes/codegen)
      carry the new arms.
    - *Randomize reuse.* `randomize(t)` inside a tseq body lowers through
      the SAME `Terminator::Randomize` + `ConstraintRef` seam as a
      test-body randomize (#372): the typed solver problem table already
      catalogs tseq randomize sites
      (`problem_table::collect_tseq_randomize_sites`), so a tseq site
      resolves its `problem_id` by span exactly like a test-body site,
      and the tbir backend splices v1's shared Z3-solve snippet. No
      second constraint path.
    - *Generator lowering.* A `tseq Gen(n) -> TSeq<Req>` becomes a
      `FunctionKind::Tseq` function: params first (locals 0..nparams), a
      `RecordSeq` accumulator (`__result`, the `ret` slot, live from
      entry since the backend always default-constructs it), then the
      body. `yield t` is `SeqPush(__result, t)` (the value must be a
      same-typed record local, else a precise rejection — v1's
      `_result.push_back(t)` would fail to compile otherwise);
      `Terminator::Return` returns `__result`. Emitted as a
      `[&]`-capturing lambda `auto Gen = [&](uint64_t n) ->
      std::vector<Req> { … };` declared before the run coroutine (v1's
      `emit_tseq` placement) so the `[&]` capture sees it.
    - *Call + iteration.* `let txns = Gen(5)` is
      `Assign(txns, Call(Tseq("Gen"), [5]))` with `txns` typed
      `RecordSeq` — emitted as a direct `Gen(5)` lambda call. `for t in
      txns` lowers to a counted loop `i = 0 .. SeqLen(txns)` whose body
      first copies `txns[i]` (`SeqIndex`) into the record-typed loop
      variable, then runs the user body. The sequence is materialized
      once and may be iterated repeatedly (a reusable value, not a
      consumed stream) — the `tseq_basic_test` fixture iterates the same
      `txns` twice.
    - *Element type may be a record OR a scalar.* `tseq_element_name`
      accepts `-> TSeq<Record>` (a declared `transaction`/`struct` →
      `TseqElem::Record`/`IrType::RecordSeq`) and, since #439, a
      `TSeq<scalar>` / `TSeq<uint<N>>` (→ `TseqElem::Scalar`/`IrType::Seq`).
      A scalar tseq lowers its generator to a `std::vector<T>` accumulator
      over the scalar C++ type (`local_scalar_cty`), `yield v` →
      `Stmt::SeqPush`, and `for v in <seq>` iterates it just like the
      record form (the loop variable copy is element-typed, not
      record-only). A missing return type or a non-`TSeq` return is still a
      precise `Unsupported` at `collect_tseq_records`. **Residual gap**: a
      scalar tseq as a *sequencer method parameter* (`hookable
      dispatch(txns: TSeq<uint<8>>)`) still resolves to `IrType::Unknown`
      in `method_param_ir_type` (`src/ir/lower/components.rs` ~L1409) — the
      element-name lookup only consults `record_ids`, so a scalar element
      falls through. Only the generator return-type / `let = Gen(..)` /
      `for ... in` surface lowers a scalar tseq today. `yield` outside a
      tseq body is rejected with v1's "`yield` outside a `tseq` body"
      intent.
    - *Fixtures.* `tseq_basic_test` (`top_counter.sv`/`top_counter.arch`,
      pass) — a self-proving `Gen(5)` with `randomize` + a post-randomize
      field override + reusable double-iteration; trace-diff clean
      v1↔tbir at seed 1. `axilite_fuzz_test` (`AxiLiteRegs.sv`, pass,
      `--test AxiLiteFuzzTest` + `axilite_regs_test.harc` helpers) — the
      corpus fuzz test fully unlocks: `tseq RandomRegs(5)` of random
      `RegData` writes/reads driven through the `axil_write`/`axil_read`
      impure helpers; trace-diff clean at seed 1. Both need Z3.
    - *Corpus residuals (deeper, NOT unlocked by tseq alone).* The
      sequencer corpus fixtures still stop past the tseq, and the
      transactor-state-field slice (2026-06-13) advanced them one more
      tier. State fields (`last_read : uint<32>`) now lower in each, so
      the next blockers are: `axilite_connect_test` and
      `axilite_seqdrv_test` on the `req : in event<RegOp>` directional
      field (the event-driven sequencer → transactor.req form);
      `transactor_agent_mode_test`/`transactor_env_mode_test` on a
      transactor with **more than one module-typed field** (`dut` + `sb`
      — agent-mode DUT-handle inheritance). Those are separate slices.

18. **`watchdog` + `on <N> cycles` periodic handlers (2026-06-13).**
    Lowers the `watchdog` lifecycle directive (spec §8.6) and the
    periodic time-trigger handler (spec §7.10) in the agent/component
    subset (`src/ir/lower/components.rs`) — the heartbeat cluster's
    remaining blocker (`watchdog_quiesce_test`,
    `watchdog_trip_diagnostic_test`).
    - *New IR.* `ComponentSchema::watchdog: Option<WatchdogSchema>`
      (`period`/`max_idle` clause exprs + a zero-arg body `function`) and
      `ComponentSchema::periodic_handlers: Vec<PeriodicHandlerSchema>`
      (`period` expr + body `function`). `Expr::CycleCount` represents the
      framework cycle counter — a bare `cycle_count` ident (conventionally
      `${cycle_count}` in a watchdog/log diagnostic) resolves here; a
      same-named local shadows it. All exhaustive matches (display,
      verify, hoist-ports, transactor-state fill, regblock-fill,
      placement) updated.
    - *Lowering.* A `watchdog` lowers to a zero-arg `ComponentMethod` body
      (the user heartbeat statements; field reads self-relative). Its
      `period`/`max_idle` clauses lower in the SAME self-component context
      (a field-backed clause reads `self.<field>`), so they could only be
      lowered once a body builder existed — pass 2 resolves them and
      patches the pass-1 schema placeholders. A `disabled` watchdog emits
      nothing (no FunctionId, no schema entry; mirrors v1's `emit_watchdog`
      early return). An `on <N> cycles` handler (`h.periodic`) lowers to a
      zero-arg `ComponentMethod` body + its period expr (the parser stashes
      the cycle count in `h.event`). Pass 1 reserves FunctionIds
      methods → event-on-handlers → periodic → watchdog so the function
      table stays monotonic.
    - *Codegen.* The tbir backend installs one per-instance `_checkers`
      closure per watchdog/periodic (recursing into by-value
      sub-components), gated on a uniquely-named `static` last-fire stamp
      (one closure per instance, so the static is effectively per-instance
      — v1 uses the same shape). The watchdog closure re-reads the period
      each cycle, fires the body when due, then runs the idle check
      (`(cycle_count - _last_in_cycle) >= max_idle` AND likewise
      `_last_out_cycle` ⇒ `sim_log_line("FAIL", "watchdog: <Comp> has been
      idle for >= %lld cycles", …)` + framework error-counter bump). The period/max_idle
      exprs render against the instance path via a new `ECx::self_subst`
      (`SelfField` → instance, since the `_checkers` closure has no `self`
      in scope). **Implementation divergence from v1 only:** v1 emits the
      idle check INSIDE the `<Comp>_watchdog` method and the period gating
      in the checker via a `field_subs`-rewritten period; tbir emits the
      idle check + period gating in the checker (the lowered body holds
      only the user statements). Behavior is identical — same FAIL text,
      same firing cycles, same error-counter bump — verified by trace-diff.
    - *Fixtures* (all `top_counter.sv`, trace-diff clean v1↔tbir at
      seed 1): `watchdog_quiesce_test` (agent + watchdog over a
      record-payload event; never trips — `pass`),
      `watchdog_trip_diagnostic_test` (silent agent, watchdog trips every
      firing from cycle 200 — `fail`, 9 identical FAIL lines, NOT in the
      pass regression), `agent_periodic_test` (self-proving `on 10 cycles`
      firing exactly 3× in 35 cycles — `pass`). All registered.
    - *Out of subset* (still rejected, separate slices):
      `quiesced(N)` env aggregation and named `phase` orchestration —
      `env_quiesced_phase_test` (a data-only `scoreboard` SUB-component in
      an env + `phase` + `quiesced(N)`) stays blocked.

19. **Env `quiesced(N)` + named `phase` + data-only `scoreboard`
    SUB-component (2026-06-13).** Closes the heartbeat/quiesce cluster's
    last three blockers (`env_quiesced_phase_test`).
    - *New IR.* `ComponentFieldKind::ScoreboardSub { scoreboard:
      ScoreboardId }` — a data-only `scoreboard` (a `ScoreboardSchema`,
      NOT a `ComponentSchema`) held as an env sub-component; distinct from
      `Sub` (which references a `ComponentId`) because the two lower to
      different schema tables. `Stmt::ScoreboardOp` and
      `Expr::ScoreboardQuery` gain `nested_path: Option<Vec<String>>`
      (`None` = the established `_tb.<field>` testbench-field access,
      `Some(path)` = the dotted env-nested path, e.g. `["top","sb"]`).
      `quiesced(N)` and `phase` add NO IR variants. All exhaustive matches
      (display, verify, tbir codegen, field lowering) updated.
    - *Lowering.* Field lowering resolves a `Named` type against the
      `scoreboard_ids` table BEFORE the component table (a data board
      becomes `ScoreboardSub`, a method-bearing one stays `Sub`).
      `scoreboard_root` (`stmts.rs`) gained an env-nested arm: it strips a
      `_tb` prefix, resolves the head through `component_fields`, walks
      `Sub` segments to a terminal `ScoreboardSub`, and returns the full
      dotted access path as `nested_path`. `as_component_quiesced`
      (`components.rs`) walks the receiver sub-component tree
      (`collect_quiesce_leaves`, mirroring v1's `collect_quiesced_paths`)
      and expands `<env>.quiesced(N)` to an AND of `Expr::ComponentIdle {
      kind: Both }` over every leaf (a component with no sub-components, or
      a `ScoreboardSub`): `top.quiesced(8)` → `top.prod.idle(8) &&
      top.sb.idle(8)`. Named `phase` blocks are collected into a name→block
      map; `expand_phase_calls` (`mod.rs`) inlines each `<name>()` call
      site in the run/check stmt list with the phase body (recursively; a
      cycle is rejected). `verify::check_scoreboard` skips the
      testbench-field binding check when `nested_path.is_some()` (the board
      lives inside the env, not on `_tb`).
    - *Codegen.* The tbir scoreboard struct now always emits the
      `_last_in/out_cycle` activity stamps (v1's `emit_scoreboard` always
      did — tbir previously omitted them, a latent divergence this slice
      closes; the stamps are needed because `quiesced` reads
      `top.sb._last_in_cycle`). `ScoreboardOp`/`ScoreboardQuery` emission
      joins `nested_path` directly when present, else falls back to
      `_tb.<field>`. The env `component_struct` emits a `ScoreboardSub`
      field by its scoreboard-struct name.
    - *Implementation divergence from v1 only.* v1 emits each `phase` as a
      captured `[&]() -> void` lambda plus a `<name>()` call; tbir inlines
      the phase body at the call site. Observably identical — the body runs
      in the run/check coroutine context either way — verified by
      trace-diff.
    - *Fixture.* `env_quiesced_phase_test` (`top_counter.sv`, `pass`):
      a `Producer` agent + a `DrainSb` data scoreboard as env subs, a
      `phase drain` doing `wait until all of top.quiesced(8),
      top.prod.seen == 3 timeout 200 cycles`, the run filling/draining
      `top.sb.expected` and calling `drain()`. Trace-diff clean v1↔tbir
      at seed 1; registered.
    - *Out of subset* (still rejected): `event<enum/Vec/nested>` payloads,
      `on <expr>` cycle-trigger handlers, `tseq`.

20. **Regblock residuals: register read in assert/format position +
    `bitbash(regs)` (2026-06-13, divergence 12).** Two of the three
    regblock-residual blockers (`regblock_basic_test`,
    `regblock_bitbash_test`). Builds on the register-level frontdoor
    (#369) and the initiator-BFM `via` helper (#375).
    - *New IR.* `Expr::RegRead { mirror, helper_ty, field, offset,
      reads_bus }` — a register-level frontdoor read in a general
      EXPRESSION position (assert condition, `log`/`fail` format arg),
      NOT a `let`-RHS. Emits v1's inline assignment-expression: RW/RO →
      `(regs.NAME = <Helper>_read(off))` (bus read + mirror predict in
      one expression), WO → `regs.NAME` (mirror only). This is deliberately
      NOT a `CallTarget::TransactorMethod` call edge — the `via` helper's
      `read` lowers to an ordinary hookable lambda (a plain C++ call), not
      the bus req/rsp wire protocol, so it is a legitimate sub-expression
      value; the verifier's seam rule (which pins call edges to statement
      position) does not apply. The inline form fires exactly one bus read
      per textual occurrence — matching v1's read-count semantics (eager
      in conditions, lazy in fail messages, which both backends emit inside
      the `if (!cond)` branch). Also `Expr::ErrorCount` — a bare HARC
      `errors` ident resolves to the framework error counter (bumped by
      `AssertCheck`/error logs), for the trailing `assert errors == 0`
      after a `bitbash` walk. Codegen emits the framework counter directly
      so a user-local `errors` cannot shadow failure accounting.
    - *`bitbash(regs)`.* Lowered (`try_lower_bitbash`, `regblock.rs`) by
      compile-time unrolling — NO new statement form. For each RW register
      and each pattern (all-ones masked to width, then zero):
      `TransactorCall(Helper.write(off, pat))`, `TransactorCall(got =
      Helper.read(off))`, `AssertCheck { got == pat, "bitbash <reg>
      <label>: wrote 0x%llx, got 0x%llx" }`. RO/WO registers are skipped
      (RO can't accept the write; WO reads are mirror-only) — matching v1's
      `try_emit_bitbash`.
    - *Placement.* `Expr::RegRead { reads_bus: true }` marks the block
      transactor-touching (the helper read may advance the clock), same as
      a `TransactorMethod` call edge; a WO read is pure host state.
    - *Fixtures.* `regblock_basic_test` (register read in assert cond +
      `${...}` fail-message format arg) and `regblock_bitbash_test`
      (`bitbash(regs)` over 3 RW + 1 RO + 1 WO). Both `AxiLiteRegs.sv`,
      `pass`, trace-diff clean v1↔tbir at seed 1; both registered.
    - *Out of subset* (still rejected, precisely at this slice): the
      passive `record_write`/`record_read` API and per-register
      `on regs.REG` write callbacks — the passive API was closed by slice
      22 below; the callback stays rejected. Field-level
      `regs.REG.FIELD` access and `addrmap` composition (incl. `alias of`)
      stayed rejected at this slice — closed by slice 21 below.

21. **Regblock residuals: field-level decomposition + `addrmap` +
    `alias of` (2026-06-13, divergence 12).** The remaining regblock
    residual blockers (`regblock_fields_test`, `regblock_addrmap_test`,
    `regblock_alias_test`). Builds on the register-level frontdoor (#369)
    and the residuals slice (#385).
    - *No new IR variants.* Field-level access composes entirely from
      existing IR. Each register grows a `RegRegisterSchema::fields`
      table (`RegFieldSchema { name, bit_pos, bit_width, access }`).
      `regs.REG.FIELD = v` lowers to a masked read-modify-write on the
      whole-register mirror cell —
      `RecordFieldWrite(mirror.REG, (RecordField & ~(mask<<pos)) | ((v &
      mask)<<pos))` — followed by a full-register `Helper.write(off,
      mirror.REG)` (v1 writes the updated whole word, not the field; RO
      fields update the mirror only). A field read is the shifted extract
      `((mirror.REG = <H>_read(off)) >> pos) & mask` (RW/RO, reusing
      `Expr::RegRead`) or `(mirror.REG >> pos) & mask` (WO, mirror-only)
      — same one-bus-read-per-occurrence semantics as the whole-register
      form. Masks clamp at 32 bits (v1's `field_mask_literal`); the
      64-bit IR mirror makes the clamp value-identical for the corpus.
    - *`addrmap` + `alias of`* (`src/ir/lower/addrmap.rs`). Each
      `instance NAME : R @ BASE [size S] [alias of OTHER]` becomes its
      own whole-register mirror local (type = the regblock's synthetic
      mirror record), declared with the mangled name
      `__addrmap_<chip>_<inst>`, with a per-instance register table whose
      offsets are pre-shifted to `base + reg_off`. The 3-level
      `chip.inst.REG` and 4-level `chip.inst.REG.FIELD` accesses resolve
      to that local + shifted table and then reuse the flat-regblock
      register/field write/read lowering verbatim (`lower_reg_write` /
      `lower_reg_read_let` / `reg_read_expr` / `lower_field_write` /
      `field_read_expr`, refactored out of the regblock path). An
      `alias of OTHER` instance shares OTHER's mirror local — only one
      storage cell is declared (no per-alias mirror), matching v1's
      "shares mirror" comment — while keeping its OWN base for bus
      traffic. Validation mirrors v1: alias targets must exist, must not
      themselves be aliases (no chains), must reference the same regblock
      type; sized non-aliased windows must not overlap.
    - *Binding.* `let chip : A = bind <helper>` is recognized alongside
      the regblock binding; the helper validation (active transactor
      field with `write(addr,data)`/`read(addr)`) and the per-instance
      mirror-local init (declared + `RecordInit` at the head of the Run
      function) mirror the regblock path.
    - *Fixtures.* `regblock_fields_test`, `regblock_addrmap_test`,
      `regblock_alias_test` (all `AxiLiteRegs.sv`, `pass`, trace-diff
      clean v1↔tbir at seed 1; all registered).
    - *Out of subset* (still rejected, precisely at this slice): the
      passive `record_write`/`record_read` API (closed by slice 22) and
      per-register `on regs.REG` write callbacks (the remaining residual).

22. **Regblock passive record API + struct `Vec` fields (2026-06-13,
    divergence 12 + the `tlm_pairing_arch_burst_*` non-scalar-field
    blocker).** Two independent features.
    - *(A) Passive `record_write`/`record_read` API.* `regs.record_write(
      addr, data)` and `let v = regs.record_read(addr)`
      (`regblock::try_lower_record_write` / `try_lower_record_read_let`).
      With a compile-time-constant `addr`, the register is decoded at
      lowering time (`const_eval_index`) and the op lowers to existing IR:
      a masked mirror `RecordFieldWrite(mirror.REG, data & width_mask)`
      (no bus traffic — the monitor already observed the write) for
      `record_write`, and a mirror `RecordField` read for `record_read`.
      A non-constant address (v1's runtime decode chain) and an address
      matching no register offset are rejected precisely. The per-register
      `on regs.REG` write callback ALSO lowers now (divergence 20, the
      closure-hook cluster): a `record_write` targeting a callback-bearing
      binding lowers to `Stmt::RecordWriteCb` (mirror update + recursion
      guard + callback dispatch via host-state promotion — the SHARED
      mirror + `cb_depth` move to test scope). The API on its own remains
      proven by `regblock_record_api_test` (`AxiLiteRegs.sv`, pass,
      trace-diff clean).
    - *(B) `Vec<T, N>` record/struct field.* `RecordFieldSchema` gains
      `vec_len: Option<usize>` (the element scalar type/width stays in
      `ty`). `records::fixed_vec_field` recognizes a `Vec<scalar, N>`
      field (rejecting a widthless / nested / list element). Emission
      (`tbir/mod.rs record_struct`) renders it as a `std::array<T, N>`
      member (v1's `record_field_c_type` Vec branch). `Expr::RecordField`
      and `Stmt::RecordFieldWrite` gain `index: Option<…>` so
      `rec.data[i]` reads/writes lower to indexed element access (a
      whole-`Vec` read/write is rejected — no array-copy expression in
      the subset). `record_struct` now emits v1's
      `harc_pack_<R>`/`harc_unpack_<R>`/`harc_drive_<R>` helpers for every
      record with a defined packed width (matching v1's unconditional
      `emit_record_pack_helpers`; the pack lays fields LSB-first in
      reverse declaration order, the unpack/drive carry the `requires`
      struct-pin fast-path). A record-returning `tlm_method` captures the
      response pin via `harc_unpack_<R>` (initiator,
      `tlm_capture_expr`) and the bound-target responder drives it via
      `harc_drive_<R>` (target) — the dest/ret record type is carried on
      the local (`bus.rs` sets the `let` dest type via `tlm_ret_record_id`;
      `transactors.rs` sets the responder ret slot via
      `record_id_of_type`, and allows a record return type past the
      scalar-only gate). Fixtures: `tlm_pairing_arch_burst_target_test`
      (HARC initiator unpacks the `HarcBurstResp32x4 { data : Vec<uint<32>,
      4>; len; resp }` response) and `tlm_pairing_arch_burst_initiator_test`
      (HARC target builds the record field-wise + packs it) — both `pass`,
      trace-diff clean v1↔tbir at seed 1.
    - *Out of subset* (still rejected, precisely): a non-constant
      `record_*` address; a widthless / nested / list `Vec` element type.
      (The whole-`Vec` field read/write, previously listed here, now
      lowers — `Expr::RecordField` / `Stmt::RecordFieldWrite` with
      `index: None` mirror v1's plain `std::array` member copy, no new IR;
      `record_vec_field_copy_test`, 2026-06-19. The per-register
      `on regs.REG` write callback, also previously listed here, lowers as
      of divergence 20.)

21. **Bound-to event-driven driver (2026-06-13).** Composes the
    event-driven-transactor consumer (divergence 11, unbound) with the
    bound-bus handshake driver (divergence 15, hookable-BFM): a
    `transactor X bound to <Bus>` with an `in event<T>` pipe + `on req(t)`
    handler whose body drives the bound bus's handshake channels
    (`bus.<ch>.send/recv`, `bus.<ch>.<sig>`) instead of a private DUT
    handle. The full UVM-style sequencer→driver over a bound bus.
    - *New schema field.* `ComponentSchema::bound_bus: Option<String>` —
      the bus a bound event-driven transactor's `on <ev>` handler bodies
      drive. `transactor_is_component`/`transactor_is_event_driven` now
      route a `bound to` transactor to the composite-component table when
      (and only when) it is event-driven (`in event` + `on` handler); a
      bound hookable-BFM or `thread bus.<m>` responder still takes the
      dedicated transactor path. A module-typed (DUT) field on a bound
      transactor is rejected (it drives the bus, not a private DUT).
    - *Body lowering.* No new IR and no new tbir codegen — the `on req`
      handler is a synchronous component subscriber (divergence 11), and
      its `bus.<ch>.send/recv` accesses CFG-inline to the same bounded
      valid/ready spin loops as the bound-initiator BFM (divergence 15).
      `lower_program` injects a per-component `LowerCtx` (now `#[derive(
      Clone)]`) carrying the bound `BusDecl` under the placeholder prefix
      (`transactors::INITIATOR_BUS_PLACEHOLDER`); the bodies otherwise
      mirror the shared `method_ctx`.
    - *Test binding.* `let xact : X active = bind axil` validates the
      binding matches `bound_bus`, fills the placeholder prefix in the
      (type-shared) on-handler bodies with the real binding name (reusing
      `fill_initiator_bus_prefix`), and registers the instance as a
      composite-component field. `active` mode required (the `on req`
      driver lives under `when active`); one bound instance per type per
      file (shared bodies). `emit xact.req(t)` fires the handler;
      `xact.<state>` reads per-instance scalar state.
    - *Fixture.* `transactor_active_test` (`AxiLiteRegs.sv`, pass,
      trace-diff clean v1↔tbir at seed 1; registered).
    - *Follow-up (now landed — see divergence 22): the passive
      handshake-MONITOR half.*

22. **Bound-to event-driven monitor (2026-06-13).** The deferred passive
    half of divergence 21: a `transactor X bound to <Bus>`'s always-on
    `on bus.<ch>.handshake(arg)` observer handlers + `sb` ScoreboardSub
    field, and a `passive` bound instance. The full UVM-style monitor over
    a bound bus (`emit_bound_monitor_actors` in v1).
    - *Desugaring (no new actor IR).* An `on bus.<ch>.handshake(arg)`
      handler lowers into a `CycleTriggerHandlerSchema` carrying a new
      `monitor_channel: Some(<ch>)`. The trigger is the channel's
      `<ch>.valid && <ch>.ready` (synthesized, rising edge); the body
      preamble captures the channel payload into the handler's `arg` local
      — the first payload signal aliases `arg` itself (so a scalar
      `sb.q.push(arg)` push sees it, matching v1's implicit-conversion-to-
      first-field), and every payload signal also lands in a per-field
      alias recorded in `recv_payloads` (so `arg.<field>` reads, e.g.
      `beat.data`/`beat.resp`, resolve, exactly like a `let r =
      bus.<ch>.recv()` capture) — then the user body runs and feeds the
      sub-scoreboard. `lower_monitor_handshake_body` (`lower/components.rs`)
      reads the channel payload from the bound `BusDecl` (visible under the
      placeholder prefix in the per-component body ctx). Reuses the entire
      existing cycle-trigger `_checkers` machinery in the tbir backend — no
      new codegen path. `ComponentFieldKind::ScoreboardSub` (the `sb`
      field) already existed (agent-mode + scoreboard-sub slices).
    - *Test binding.* A `passive` bound instance is now accepted when the
      transactor declares monitor handlers (a pure-driver transactor with
      no monitor half is still rejected — a passive instance would be
      inert). Both `active` and `passive` instances register the always-on
      monitor cycle-handlers; the `active` instance additionally fires its
      `on req` driver on `emit`. The synthesized monitor triggers live on
      the schema (rendered standalone in the per-instance `_checkers`
      closure), so a new `fill_initiator_bus_prefix_expr` fills their
      placeholder bus prefix with the real binding name alongside the body
      fill (the `fill_visit_*` walkers were factored out of
      `fill_initiator_bus_prefix` and shared).
    - *Divergence from v1.* v1 emits a per-channel coroutine ACTOR
      (`co_await wait_until(valid && ready)`, capture, run body,
      `co_await wait_cycles(1)`); the IR uses a rising-edge cycle-trigger
      `_checkers` closure instead. Observably equivalent for the lowered
      subset — single-beat valid/ready handshakes (valid && ready high for
      exactly one cycle per beat, as AXI-Lite drives them): both fire once
      per handshake. They could diverge only for a multi-cycle held
      handshake (a sustained burst with valid && ready high across several
      cycles), where v1's actor would fire once per cycle (minus the 1-cycle
      skip) while the rising edge fires once per 0→1 transition; such held
      handshakes are not in the lowered subset.
    - *Fixtures.* `transactor_passive_only_test` (pure monitor, no driver),
      `axilite_bound_mon_test` (active driver + passive monitor,
      concurrent), `axilite_multi_payload_test` (multi-payload
      `beat.data`/`beat.resp` capture on the observation side) — all
      `AxiLiteRegs.sv`, pass, trace-diff clean v1↔tbir at seed 1;
      registered.
    - *Out of subset* (still rejected, precisely): a non-bound component
      carrying handshake-monitor handlers (nothing to observe), and a
      bound transactor with a module-typed DUT handle (it drives the bus).

23. **Record-typed method params + value-returning method call in
    expression position (2026-06-13).** Two transactor-method subset
    extensions, no new IR variants.
    - *(a) Record param by value.* A method param typed as a declared
      `transaction`/`struct` (`hookable run_for(cmd: RunCmd)`) lowers to
      `IrType::Record(rid)` (resolved via the method ctx's `record_ids` in
      the new `method_param_ir_type`, replacing the scalar-only
      `check_scalar_ty` + `ir_type_of` in BOTH the unbound and bound-to
      method loops). The param local is a record (default-constructed at
      entry); the body reads its fields via the existing #364
      `Expr::RecordField`. tbir `emit_method` renders a record/recordseq
      param as the by-value struct, mirroring the already-record-aware
      `emit_component_method` signature. No divergence from v1 (v1 passes
      structs by value too).
    - *(b) Value-returning call in expression position.* Previously a hard
      rejection ("transactor method call in expression position — hoist it
      into a `let` first"). Now a value-bearing `TransactorMethod` edge in
      expression position is HOISTED into its own
      `Stmt::TransactorCall { dest: Some(temp), .. }` and the result temp
      substituted — the seam rule's sanctioned home (the edge never stays
      nested) is preserved, and the call's internal `tick()` runs at the
      hoist point in source order. The hoist rides the existing
      left-to-right traversals: `hoist_ports` (for `lower_expr_no_ports`
      contexts, correctly interleaving with DUT-port `DutRead` hoists) and
      a sibling `hoist_transactor_calls` walk for `lower_assert` (DUT ports
      stay inline/lazy there, but a transactor edge still cannot stay
      nested). `lower_expr` now builds the edge via
      `lower_transactor_call(.., need_ret=true)` (a void method used as a
      value is rejected, as v1 surfaces a C++ type error); the
      `in_fmt_args` rejection (call inside a lazy log/fail message) is
      kept. **Implementation note vs v1:** v1 emits the call inline in C++
      (left-to-right C++ evaluation order does the sequencing); the IR
      makes that sequencing explicit by hoisting to a preceding statement —
      observably identical for the lowered fixtures (no fixture mixes a
      hoisted DUT-port read and a transactor call in one expression).
    - *Fixtures.* `transactor_record_param_test` (self-proving record param,
      `top_counter.sv`), `axilite_regs_full_test` (`helper.read(addr)` in
      assert/bitwise expression positions, 6 sub-tests, `AxiLiteRegs.sv`) —
      both pass, trace-diff clean v1↔tbir at seed 1; registered.
    - *Out of subset* (still rejected, precisely): a transactor call inside
      a lazy log/fail message. (A >64-bit method *value* param up to 128 bits
      — `aes_cipher_top_test` `load_block(key: uint<128>)` — previously
      listed here, now lowers via the wide-value method-param ABI: admitted
      by `check_method_param_ty` and rendered as v1's `_harc_u128`; a param
      WIDER than 128 bits, v1's `HarcWide<N>` word-array model, stays
      rejected. Test-scope `on <obj>.<method> pre/post` METHOD HOOKS, also
      previously listed here, lower via host-state promotion — divergence 20.)

Minor, same spirit: `IndexVec` is a plain `Vec` plus typed id structs;
the design's `AssertFail` enum collapsed into a single
`FmtArgs on_fail` because both source forms bump `errors` identically
in v1. (`FunctionKind::TransactorBody` carries `transactor: TransactorId`
rather than the design's `{ bus, method }` pair — the unbound BFM, the
bound-to target-responder, and the bound-to initiator-BFM forms all reuse
it; the method name lives in the schema, and for a bound transactor the
served bus is on `TransactorSchema::bound_bus`.)

### Verifier coverage summary

Implemented: invariants 1–4, 6, 8 (amended), 10, 15, plus the
port-position rule and the transactor-call seam rule (divergence 9 —
position, function kind, binding/method/arity resolution; the
seam-rule half of design invariant 11's intent, ported from `Fork`
arms to the call-edge form actually produced). Invariant 9 (every
`Terminator::Randomize`'s `ConstraintRef` resolves, and its target is
record-typed) is now checked via `DanglingConstraintRef` (divergence
14). By construction: 5, 7
(with the documented `TransactorMethod` exception). Not implemented:
11 (no `Fork`), 12 (no DUT port
table — see divergence 1), 13 (not separately checked; the
port-position rule covers the `PortRef` half), 14 and 16 (the v0
front end does not type-check, so `IrType::Unknown` is the common
case and only locally-determinable `Assign` types are compared).

24. **Time-literal digit separators (2026-06-19, authorized divergence).**
    A bare `time` value in expression position with a digit separator —
    `let t : time = 1_000ns;` — lowers in tbir to the underscore-stripped
    value `1000` (`ExprKind::Time` in `src/ir/lower/exprs.rs` strips `_`
    before parsing). v1's `emit_expr_with_arrow` emits the prefix verbatim
    (`uint64_t t = 1_000;`), which is a **C++ compile error** (no
    `operator""_000`). This is an intentional, authorized divergence: tbir
    is the more-correct backend (it lowers what the source plainly means);
    v1's behavior is a legacy limitation, not a contract tbir preserves.
    The equivalence harness's v1==tbir invariant does not apply to a
    digit-separated time literal — no equivalence fixture uses one (v1
    cannot compile it). Non-separated literals (`100ns` → `100`) still
    mirror v1 exactly. See issue #451.

25. **Concurrent `assert` / `assume` / `cover` (spec §5, 2026-08-16).**
    The concurrent verification statements now lower to
    `TbProgram::property_checks` / `TbProgram::cover_checks` plus a
    `Stmt::PropertyCheck` / `Stmt::CoverCheck` registration at the source
    statement's position, emitted as the same per-primary-clock-edge
    `_checkers` closure v1 builds in `emit_property_check`. Dispatch
    matches v1 exactly (`is_concurrent_assertion`: a bare identifier
    naming a declared `property`, or any expression carrying a temporal
    operator, is concurrent; everything else is the immediate
    point-in-time check — the legacy `property` keyword does not change
    dispatch). The three shapes (`a |-> b`, `a |=> b`, plain invariant),
    the failure lines, and the error-counter policy (`assert` bumps,
    `assume` does not) are v1's.

    Two deliberate improvements over v1 in this area, both in cases where
    v1 emits C++ that does not compile — so neither can change the
    behavior of a program v1 can actually build:

    - **`cover` hit counters live at file scope.** v1 declares each
      `_cov_<tag>_hits` as a `static` LOCAL at the statement's position
      *inside the run coroutine lambda*, then reads it from the enclosing
      function's end-of-test summary — an out-of-scope reference. TB-IR
      hoists the counter to file scope, which has the same lifetime and
      compiles. This is also why the old "mixing a bare `cover` statement
      with a `scope`/`run` block" rejection is gone: it existed only
      because there was no correct v1 behavior to mirror.
    - **Temporal readings work inside a `cover` body.** v1 only installs
      its span-keyed `prop_subs` substitutions around
      `emit_property_check`, and `emit_expr` has no arm for a temporal
      system call, so `cover rose(dut.x)` emits a call to a nonexistent
      `rose(...)`. TB-IR gives a cover body the same latch machinery as a
      property body.

    A `cover` counter is keyed by its REGISTRATION index, not by the
    source span alone: one source `cover` inside a helper inlined at two
    call sites is two registrations, and two file-scope statics may not
    share a name.

    Carried over from v1 unchanged: `past(e, N)` ignores `N` and reads
    the immediately previous cycle (one latch slot per occurrence). A
    deeper history would diverge from v1, so fixing it belongs in a
    change that moves both backends together.

    Outside a concurrent check body a temporal reading has no per-cycle
    latch to read, and v1 emits nothing for it; that is a program error
    under every backend, so it surfaces as `LowerError::Invalid` — no
    `--codegen v1` suggestion. Same for `assert property NAME` /
    `assume property NAME` / `cover property NAME` naming an undeclared
    property.

    Gated by `tests/concurrent_check_cpp.rs`, which splices the emitter's
    actual `_checkers` closures into a probe with a stub DUT, drives a
    fixed stimulus, and checks the resulting error / ASSUME / cover-hit
    counts — a string match cannot tell a correct latch state machine
    from one that reads its own write.

26. **Statement-position `on` handlers (2026-08-16).**
    An `on <bool-expr> ... end on` or `on <N> cycles ... end on` written
    inside a run or check body now lowers to `TbProgram::cycle_handlers`
    plus a `Stmt::CycleHandler` registration at the statement's position,
    emitted as the same `_checkers` / `_post_eval_services` closure v1's
    `emit_cycle_trigger` installs — the same rising/falling edge latch,
    the same last-fire stamp, the same phase routing. Arming at the
    statement position (not at test setup) is v1's behavior too: a
    handler written after a `wait` never observes the earlier cycles
    under either backend. This is distinct from the
    testbench-DECLARATION-scoped forms in `TestbenchSchema::
    {periodic_services, cycle_services}`, which arm during setup.

    Differences from v1, both structural to the function-per-CFG IR:

    - **The body is its own `FunctionKind::TestHook` function**, emitted
      as a free `[&]`-capturing lambda at test scope and called from the
      registration closure. v1 inlines the body into the closure. The
      consequence is that a handler body cannot read the enclosing run
      function's locals — the same run/check split that forces a
      test-scope `let` to be promoted to a `_tb` field (#444). Such a
      reference is reported by the ordinary name-resolution path rather
      than silently dropped. Testbench fields and DUT ports resolve
      normally.
    - **A periodic handler's period must be a positive integer literal.**
      v1 re-reads a variable period every cycle so a test can override it
      from host state; the registration closure here carries no such read.
      A non-literal period is rejected with that explanation.

    Out of subset with precise messages, because a statement position
    cannot supply what they need: `on <obj>.<method> pre/post` (the hook
    must be in the method's pre/post vector before any call site runs)
    and `on <event>(arg)` (needs a subscriber list on a component field).

    Handler bodies are lowered out of line, mid-statement, where no
    `FunctionId` can be reserved — the builder does not see
    `TbProgram::functions`. They park in `SideTables::pending_functions`
    with their slot index as a placeholder id, and `lower_program`
    assigns dense ids once every source-order function is pushed.

    Gated by `tests/concurrent_check_cpp.rs`, which drives the emitted
    closures against a stub DUT and checks that a rising-edge handler
    fires once per 0→1 transition (not once per cycle the predicate
    holds) and that a periodic handler first fires at cycle N.

27. **Diagnostic honesty: `--codegen v1` is only suggested when v1 has
    it (2026-08-16).**
    Every TB-IR subset rejection used to end in "re-run with
    `--codegen v1`". For a large class of constructs that advice is a dead
    end: v1 raises its own `statement/expression not supported in v0
    cpp_tb` error, or accepts the source and emits C++ that does not
    compile, or accepts it and quietly emits something else.

    `LowerError` therefore has a third variant. `Unsupported` keeps its
    meaning — a TB-IR subset gap where v1 IS a working escape hatch — and
    `NotImplemented { construct, detail, v1: V1Status }` covers the rest,
    ending in what v1 actually does (`Rejects` / `EmitsUncompilable` /
    `SilentlyMisLowers`) instead of a suggestion. `Invalid` is unchanged
    (a program error under every backend).

    Reclassified after checking v1's behavior directly (`harc sim
    --emit-only --codegen v1` on a probe per construct):

    | Construct | v1 |
    |---|---|
    | `parallel` / `schedule` / `select` (spec §17.1) | rejects |
    | block-form `fork ... branch ... join_all` | rejects |
    | `apply` (aspect activation, spec §3.6) | rejects |
    | `randomize` in expression position | rejects |
    | `clog2`, `##N` / `[*N]`, cover-sequence `=>`, named arguments, struct/set/`dist` literals, `in` membership, `soft`, `solve_order`, constraint `for` — all in ordinary VALUE position | rejects (`emit_expr` has no arm) |
    | `= bind ...` in statement position | silently mis-lowers — emits a plain `let`, dropping the bind |
    | a range expression in value position | silently mis-lowers — emits `/* range a..b */ 0` |

    The constraint-only forms in that list all lower correctly *inside*
    `randomize ... with`: the typed constraint backend handles them and
    `lower_expr` never sees them. The rejection is about position, and the
    messages say so.

    A `package` declaration is now accepted as **inert**, matching v1
    exactly — v1 has no `Item::Package` arm at all, `merge_for_sim` passes
    a package through whole rather than hoisting the `extend` blocks
    inside it, and a package's contents only take effect at an `apply`
    site. So the gap a user actually has is the `apply`, and that is now
    what gets reported.

28. **Recursive PURE helpers (2026-08-16).**
    A helper with no DUT access and no waits lowers to a file-scope C++
    function whose prototype is emitted ahead of every body
    (`emit_helper_prototype`), so it can call itself — directly or
    mutually. The registry's recursion check therefore runs AFTER the
    purity fixpoint rather than before it, and rejects a cycle only when
    some member of it is impure (an impure helper is CFG-inlined at each
    call site, and inlining a cycle does not terminate).

    **TB-IR is strictly ahead of v1 here.** v1 emits every helper as an
    `auto` lambda, and a lambda that names itself inside its own
    initializer is a C++ compile error ("use of `f` before deduction of
    `auto`"), so no recursive helper has ever worked under v1. The
    diagnostics for the cases that remain rejected — recursion through an
    impure helper, and recursive testbench methods (always inlined,
    because they capture the shared `_tb` host state) — say that rather
    than suggesting `--codegen v1`.

    `tests/pure_recursion_cpp.rs` extracts the emitted helper functions
    and builds them, so `fact(5) == 120` is checked by the host compiler,
    not by a string match on the prototype.

    Also reclassified in the same pass, after checking v1 directly: a
    **string value in expression position** (v1 emits `int64_t s =
    "hello";` — a compile error) and a **float literal** (v1 emits
    `int64_t f = 1.5;`, which compiles and silently truncates to 1).

29. **Test-scope event channels (spec §3.4, 2026-08-16).**
    `let e : event<T>` inside a run or check body now lowers to an
    `IrType::Event` local — v1's subscriber-vector shape, a
    `std::vector<std::function<void(payload)>>` in the enclosing
    coroutine. `on e(v) ... end on` is a `Stmt::EventSubscribe` (push a
    closure), `emit e(x)` is a `Stmt::EventEmit` (synchronous fan-out in
    subscription order, `for (auto& _s : e) _s(x);`). Payload resolution
    reuses the component-field rules: a scalar ≤ 64 bits widens to
    `uint64_t`/`int64_t`, a `transaction`/`struct` payload is carried by
    value as the record struct.

    Same structural difference as the cycle handlers: the subscriber body
    is its own ONE-parameter `FunctionKind::TestHook` function declared at
    test scope, because a lambda declared inside the run coroutine's
    `switch` case would die with the case block while the pushed closure
    still referenced it. v1 inlines the body into the pushed closure.

    This is distinct from a component's `in`/`out event<T>` FIELD, which
    lives on the component struct and is reached through `ComponentEmit`
    and the `connect` graph — that path already lowered.

    Malformed use is a program error under every backend, so it surfaces
    as `Invalid`: an initializer on the channel, an `emit` arity other
    than one, a non-name payload binding.

    `tests/concurrent_check_cpp.rs` builds the emitted declaration,
    subscriptions, and fan-out and checks that two emits each reach both
    of two subscribers — a fan-out that stopped at the first subscriber
    would still string-match.

30. **Where a registration may appear (2026-08-16).**
    The registration statements — concurrent `assert`/`assume`/`cover`,
    statement-position `on` handlers, event subscriptions — install a
    closure that outlives the statement. That is only sound where the
    statement runs exactly once, so lowering admits them **only in a
    test's own `run` / `check` body** (`FuncBuilder::in_test_body`).

    In a transactor method, a helper, or another handler's body, a
    registration would re-register on every call (unbounded growth of the
    checker list) and its `[&]` capture would read parameters that died
    with the call. v1 emits exactly that — the program builds and then
    misbehaves — so the rejection carries `V1Status::SilentlyMisLowers`,
    not a `--codegen v1` suggestion.

    Two related rules from the same pass:

    - **A `wait` inside a statement-position `on` body is rejected.** The
      body runs from the per-cycle checker pass, and a `wait` there lowers
      to `tick()`, which re-enters that same pass — unbounded recursion
      once the trigger fires. v1 has the identical shape and the identical
      hazard.
    - **Out-of-line handler bodies reserve their table slot before
      lowering**, so a nested registration inside a body claims the next
      slot rather than colliding on the same index
      (`reserve_pending_function` / `commit_pending_function`). The
      test-body rule above already forbids the nesting; reserving first
      makes the invariant hold independently of it.

    A check body may legitimately read a local of the function that
    registered it — the emitted closure captures it by reference, as v1's
    does — so `harc dump-ir`, which renders check bodies outside any
    local table, falls back to the raw id instead of indexing a scope
    that does not hold it.

    A statement-position `on <trigger>` predicate renders inside the
    registration closure, so its port reads are real program port
    accesses even though `Stmt::CycleHandler` carries none: the
    probe-detection and gated-bus walks visit `cycle_handlers` alongside
    the check bodies. Without that, a probe read appearing only in a
    trigger left `V<Top>___024root.h` out of the preamble while the
    trigger still emitted `dut->rootp->…`.

31. **`for t in <tseq-call>` and timed waits in method bodies
    (2026-08-16).** Two more constructs v1 implements and TB-IR refused,
    both found by the batch probe sweep described below.

    - **`for t in S()`** — the generator call written inline rather than
      bound to a `let` first. The bound form (`let xs = S(); for t in xs`)
      already lowered; only the inline spelling was missing. v1's
      `for (auto& t : S())` binds the returned vector to a temporary that
      lives for the whole loop, so the generator runs ONCE; TB-IR
      materializes it into a synthesized local, which has the same shape
      and the same single evaluation, then reuses the existing
      `SeqLen`/`SeqIndex` iteration.
    - **`wait until … timeout` inside a transactor method.** A method body
      has no scheduler to defer to, so the timed wait takes v1's
      SYNCHRONOUS shape (spec §7.4's "synchronous context"): budget read
      once, predicate polled with `tick()` per cycle, bounded by elapsed
      cycles — not the coroutine `wait_until_timeout` awaiter the run body
      uses. The error bump rides the timeout edge exactly as on the
      coroutine path. Only the emitter was missing; the
      `Terminator::WaitUntilTimeout` the lowering already produced needed
      a sync arm alongside the existing sync `WaitCycles`/`WaitUntil`
      ones. `wait N cycles on <clock>` in a method remains out of subset.

    Six more diagnostics reclassified in the same pass, each checked by
    emitting the construct with `--codegen v1` and reading the generated
    C++:

    | Construct | v1 |
    |---|---|
    | `return <expr>` in a run/check body | emits `return <expr>;` inside a coroutine — only `co_return` is legal, so it does not compile |
    | a bare DUT reference (`let x = dut`) | emits the DUT pointer into an integer slot (`int64_t x = dut;`) |
    | `.field` shorthand | emits the shorthand verbatim (`int64_t x = .a;`) |
    | `as` casts outside scalar uint/sint/bits | drops the cast and emits the operand alone |
    | bit-slice bounds above 2^32 | casts the bound to `uint32_t` with no range check — silently slices the wrong bits |
    | `yield` outside a `tseq` body, `randomize` of a non-identifier | raises its own error |

32. **Second probe sweep (2026-08-16).** Six more diagnostics
    reclassified after emitting each construct under `--codegen v1` and
    reading the C++:

    | Construct | v1 |
    |---|---|
    | non-string-literal `fail(...)` / `else fail(...)` message | emits the FIXED text (`"fail() with non-string arg"` / `"assertion failed"`) and drops the expression, so the failure line says nothing about the value |
    | bind remaps on `let dut` (`= bind top with { … }`) | parses the clause and emits nothing for it — the TB drives the un-remapped port |
    | probe declarations on a transactor instance | emits no probe accessor at all; the declaration is silently inert |
    | `bitbash(<non-identifier>)` | emits a call to a `bitbash(...)` function it never defines |
    | `emit` with no resolvable channel | emits the fan-out anyway (`for (auto& _s : a.b) _s(x);`), naming a symbol that does not exist |

    The probe / bind-remap rejections are **one family with one rule**:
    v1 emits no probe accessor for any binding but `dut` (so the
    declaration is inert and any read of it fails to compile) and drops a
    `with { … }` remap clause entirely. All fourteen sites — bus,
    regblock, addrmap, initiator BFM, bound-to event-driven transactor,
    target-TLM responder, component, transactor, and `let dut` itself —
    carry that rule. Most need a differently-shaped fixture to reach, so
    `probe_and_remap_rejections_are_one_family_with_one_rule` checks it
    structurally over the lowering sources and a companion test exercises
    one arm end-to-end, so the scan cannot pass over dead code.

    **A third class exists.** `log(<unknown severity>, …)` is neither a
    TB-IR gap nor a v1 escape hatch. Spec §7.7 defines `Severity` as a
    closed five-variant enum, so `trace` names no severity at all; v1
    "handles" it only by uppercasing whatever ident it finds into the log
    tag (`sim_log_line("TRACE", "x")`), which is exactly what makes a
    typo dangerous — `log(errror, …)` prints an `ERRROR` line and never
    bumps the failure counter, so a test that should fail passes. That
    diagnostic is a `LowerError::Invalid` naming the five variants
    (divergence 33), not a gap report in either direction. The three
    classes only stay useful if the line between them tracks what v1
    actually does *and* what the spec actually says.

33. **Third probe sweep: runtime bit slices, check messages, and the
    `Invalid` class (2026-08-16).** The sweep moved from statement- and
    item-level constructs to ordinary *expression* surface. Two real
    gaps closed, and the rest of the batch turned out to want a
    diagnostic rather than an implementation.

    - **Runtime-bounded bit slices** (`x[i:0]`, `x[hi:hi-3]`). TB-IR
      folded a slice into a shift-and-mask, which needs both bounds at
      lowering time, so anything else was refused. v1 has never folded:
      it emits `harc_rt::harc_bits(value, hi, lo)` for *every* slice,
      constant bounds included, and that helper takes runtime bounds and
      guards its own range. New `Expr::BitSliceDyn` carries the two bound
      expressions and emits that helper; the constant form keeps folding,
      because a known width is worth having where it exists. The target
      reaches the call UNCAST, so overload resolution picks the right
      `harc_bits`: a scalar converts to `_harc_u128`, a `HarcWide<N>`
      binds the wide overload that slices out of all N words. Casting to
      `_harc_u128` first would go through `HarcWide::operator
      _harc_u128`, which keeps only the low four words — `w[200:193]` on
      a `uint<256>` would read 0. The one shape needing a wrapper is a
      whole port: the raw Verilator signal is a `WData` array with no
      `harc_bits` overload, so it widens through `harc_read` (v1's shape)
      first. The value types as `UInt(None)` — invariant 15's widthless
      wildcard, which is what a `uint64_t` helper return is.
    - **`else fail("…")` on a concurrent `assert` / `assume`.** v1
      *parses* the clause and then discards it, so every concurrent
      failure there prints the same anonymous ``property `<inline>`
      failed`` line no matter how many checks are registered — the
      message is written, accepted, and lost. `PropertyCheckSchema` now
      carries an optional `FmtArgs`, lowered through `with_check_body` so
      it obeys the same rule as the condition (it may read locals and
      ports; it may not push statements into the test), and the emitted
      closure prints it in place of the generic line, suffix included.
      The immediate `assume` path was fixed to honor the clause too, so
      `else fail(…)` means the same thing in all four positions rather
      than only three, and `cover … else fail(…)` — which has no failure
      to name — is now rejected instead of dropped, closing the last
      "written, accepted, lost" position. `TbProgram`'s probe-detection
      and gated-bus walks visit the message's interpolation captures, for
      the same reason the trigger predicates are walked (divergence 30).

      The message is lowered with an EMPTY slot map, like a latch
      operand. `lower_fmt` re-parses each `${…}` capture as a standalone
      fragment, whose spans are relative to the fragment rather than to
      the file, so a capture's span can collide with a real temporal
      occurrence's and get silently rewritten into that occurrence's
      `Expr::TemporalSlot` — a `${dut.b}` emitting `_harc_ps0`. With the
      map empty a `${past(x)}` reaches the ordinary temporal gate and is
      rejected by name; the verifier holds the message to 0 slots, so
      the guarantee stays testable rather than implicit.

    Diagnostics reclassified in the same pass:

    | Construct | v1 | new class |
    |---|---|---|
    | index expression on a non-indexable value (`a[0]` on a scalar) | emits the subscript verbatim (`int64_t b = a[0];`) | `NotImplemented` / emits-uncompilable |
    | field access on a non-DUT value (`x.foo`) | emits the member access verbatim (`int64_t y = x.foo;`) | `NotImplemented` / emits-uncompilable |
    | `throughout` / `within` / `intersect` | emits the C++ **comma operator** (`a /* unsupported-op */ , b`), which compiles and evaluates to the right operand alone | `NotImplemented` / silently-mis-lowers |
    | `\|->` / `\|=>` outside the top level of a check — a value position, or nested (`a \|-> (b \|-> c)`, legal property syntax this subset does not lower) | same comma operator, so the antecedent is silently dropped | `NotImplemented` / silently-mis-lowers |
    | reversed literal slice (`x[0:3]`) | emits `harc_bits(v, 0, 3)`, whose `hi < lo` guard returns 0 — a silent always-zero read | `Invalid` |
    | `log(<unknown severity>, …)` | uppercases any ident into the log tag, so a typo'd `error` never bumps the failure counter | `Invalid` |

    The `Invalid` rows are the point of the sweep as much as the two
    implementations. A malformed construct is not a backend gap in
    either direction, and calling it one — in either the
    `Unsupported` or the `NotImplemented` sense — tells the user to go
    looking for a workaround instead of fixing their code. The line runs
    the other way too: a *nested* implication is not malformed, just
    unlowered here, so it belongs in the `NotImplemented` row beside the
    value-position spelling rather than in the `Invalid` block with the
    reversed slice.

34. **Fourth probe sweep: `Vec`-field iteration, discarded pops, and a
    landing-dependent site (2026-08-16).** Twenty-five constructs across
    queues, `Vec` record fields, and name resolution. Two gaps closed;
    twelve diagnostics reclassified.

    - **`for x in <rec>.<vecfield>`** — iterating a `Vec<T, N>` record
      field. v1 emits `for (auto& x : rec.data)` over the `std::array`,
      which works; TB-IR refused the form entirely, so a loop this plain
      needed `--codegen v1`. The length is a schema constant, so it
      lowers to a counted loop over `0 … N-1` whose body binds the loop
      variable to the same `Expr::RecordField` an explicit
      `<rec>.<vecfield>[i]` read produces. Nested paths
      (`a.b.<vecfield>`) reach it too. `record_vec_field_iter_test` is
      the equivalence fixture.

      v1's `auto&` is a REFERENCE; the IR has no by-reference local, so
      the loop variable is a copy. That difference is observable only
      through a WRITE to the loop variable, which
      `lower_counted_loop_with_prologue` rejects for THIS form: it is
      new here, so rejecting costs no working program, and shipping a
      silent divergence would be worse. `for t in <tseq-result>`
      deliberately keeps its by-copy variable and no rejection — that
      behavior predates the sweep and `for t in txns … t.addr = … end
      for` is an idiom both backends accept today, so turning it into an
      error would break working programs. Giving the tseq form real
      write-back is its own change, and follow-up.

      The check runs on the lowered CFG rather than the AST, and covers
      every statement that names a local DESTINATION, not just `Assign`:
      `lower_assign` routes `x = sb.q.pop()` into
      `ScoreboardOp::QueuePop { dest }` and `x = xact.m(…)` into
      `TransactorCall { dest }`, so an `Assign`-only check would pass
      exactly those writes through.

      Non-leaf element selectors (`t.entries[k].data`) are snapshotted
      into temps BEFORE the loop, unconditionally — a bare local is
      precisely what the body can reassign. v1's range-for evaluates the
      container expression once, so a selector left inside the
      per-iteration bind would walk a different row each time the body
      advanced `k`, and a port read there reached the verifier as
      `PortInDisallowedPosition` — an internal-bug channel printed to
      the user as raw IR.

    - **A discarded `q.pop()` in statement position** — five sites
      (testbench, scoreboard, component, bare target-responder state,
      instance-qualified target state), each of which told the user to
      "bind the popped value". `pop` mutates the queue, so the statement
      has a point even with the value thrown away, and v1 emits exactly
      that (`_tb.q.pop();`). Every IR pop carries a destination, so the
      value now lands in a temp nothing reads (`discard_slot`), typed
      from the queue element so a record pop keeps its struct slot. The
      family is checked structurally, like the probe/bind-remap family
      in divergence 32.

    Diagnostics reclassified, all `NotImplemented` / emits-uncompilable:

    | Construct | what v1 emits |
    |---|---|
    | an unresolved name (`let x = nope`) | `int64_t x = nope;` |
    | assignment to an unknown name (`nope = 1`) | `nope = 1;` |
    | indexing a scalar record field, read or write | the subscript verbatim against a `uint64_t` member |
    | `for x in <scalar record field>` | `for (auto& x : rec.v)` over a `uint64_t` |
    | whole-record write with a non-matching RHS | `o.p = q;` between two unrelated structs |
    | a queue method outside `push`/`pop`/`size`/`empty` (9 sites) | a call to a method `HarcQueue` never defines |
    | a `default` on a queue field (2 sites) | `HarcQueue<uint64_t> q = 0;` — no such constructor |

    **One site, several outcomes.** Three reclassifications were made
    and then reverted, which is the most useful thing this sweep
    produced. A rejection site can cover landings with DIFFERENT v1
    outcomes, and then no single `V1Status` is honest:

    - whole-`Vec` READ — `assert r.data == r.data` emits
      `r.data == r.data`, which compiles and works (`std::array` has
      `operator==`); `let d = r.data` emits `int64_t d = _tb.r.data;`
      and `${r.data}` emits `harc_printf_ll(r.data)`, neither of which
      does.
    - whole-`Vec` WRITE with a non-matching RHS — "non-matching" is a
      HARC judgement, and v1 collapses every scalar of 64 bits or fewer
      to `uint64_t`. A length mismatch or a scalar RHS does fail there,
      but `Vec<uint<8>, 4> = Vec<uint<32>, 4>` emits
      `std::array<uint64_t, 4> = std::array<uint64_t, 4>` and compiles.
    - uninitialized `let x` with no type — v1 emits only a COMMENT for
      the declaration, so whether its output compiles depends on
      whether the name is later USED. The rejection fires at the
      declaration, before that is known.

    All three keep the `Unsupported` label — true somewhere — and lead
    with the detail that works everywhere. The classes describe what a
    backend actually does, so a site that is not one thing must not
    claim to be. Checking a construct in ONE landing and generalizing is
    the mistake this method is supposed to prevent, and it took a review
    pass to catch each time.

    Two sites were left alone rather than guessed at: the whole-`Vec`
    read and write of a transactor *state* record field. Both sit behind
    an earlier gate (record-typed transactor fields), so no fixture
    built for this sweep reached them and no v1 output was observed. An
    unverified reclassification is the failure this method exists to
    prevent.

35. **Fifth probe sweep: constant-folded field defaults (2026-08-16).**
    The first pass over `components.rs` (73 sites) and `transactors.rs`
    (55) — the two files earlier sweeps never touched. One gap closed
    across three paths, six diagnostics reclassified.

    - **A field `default` now folds through the file's constant table**,
      on component, scoreboard, and transactor-state declarations alike.
      Each path had its own hand-rolled matcher accepting an integer
      literal and a bool and nothing else, so `n : uint<8> default K`
      with `const K = 7` in scope was refused — while v1 emitted `= K`,
      which compiles and works, because the const is emitted as a C++
      constant.

      Reading v1's output on the wider shape is what made this worth
      doing: v1 emits the default's SOURCE TEXT into the member
      initializer and silently degrades to `= 0` for anything it cannot
      spell that way, so `default 1 + 1` starts the field at 0 rather
      than 2 — accepted, compiled, and wrong. Routing all three paths
      through the existing `fold_const` covers both of v1's working
      shapes and every other constant expression besides, which puts
      TB-IR AHEAD of v1 here rather than level with it. The only
      remaining rejection is a default that is not constant at all
      (`default "x"`), now a `NotImplemented` / silently-mis-lowers.

    The fold reaches component, scoreboard, transactor-state AND
    testbench fields, so `default K` means the same thing everywhere in
    one source file. Range checking rides along: the folded value goes
    through the same `check_const_decl_type` a `const` declaration gets,
    so `uint<8> default -1` and `default 300` are rejected here rather
    than emitted as a 64-bit bit pattern. The three error classes stay
    distinct — a non-constant expression is a `NotImplemented`, while an
    illegal evaluation (division by zero, a value that does not fit) is
    a `LowerError::Invalid`, matching what a `const` declaration reports
    for the same expression.

    **A reclassification that did not survive review.** The five
    directional-component-field rejections (`in`/`inout` on an event,
    queue, scalar, or named-type field) were reclassified on the premise
    that v1 "never reads the direction" — and that premise is false. v1
    emits byte-identical, WORKING C++ for `event`, `in event`, and
    `inout event` on an agent, and honors defaults on directional
    scalars just as it does on plain ones. `--codegen v1` is an honest
    escape hatch for all five, so all five keep `Unsupported`. This is
    the third sweep in a row where a plausible reading of one emission
    turned out not to generalize; the rule that keeps surviving is that
    a `V1Status` claim needs v1's output for the shape being claimed, not
    for a neighbouring one.

    Left open, and worth a slice of its own: a **transaction-typed field
    on an unbound `transactor`** (`cur : Txn`). v1 emits a real `Txn`
    member that works, and the IR already has `StateFieldKind::Record`
    for the bound-to path — but the unbound path reaches record fields
    through different machinery, so this is a design step, not a missing
    arm. It keeps its `--codegen v1` suggestion, which is honest.

36. **Sixth probe sweep: `connect` endpoints and record-typed
    transactor fields (2026-08-16).** Twelve `connect` endpoint shapes
    plus the transaction-typed transactor field divergence 35 left open.
    One gap closed, seven diagnostics reclassified.

    - **A record-typed field on an UNBOUND `transactor`** (`cur : Beat`).
      v1 emits a real struct member, writes it from the method body as
      `self.cur.tag = …`, and reads it back from the test — a working
      feature TB-IR refused outright. The unbound path now routes record
      fields through the same `lower_state_field` the bound-to path
      already used, which produces `StateFieldKind::Record`.

      Reaching it exposed a latent emitter bug worth recording, because
      it is what makes "close the gap" different from "delete the
      rejection": `Stmt::TransactorStateRecordFieldWrite` interpolated
      its `instance` RAW, while the read side and the scalar write both
      went through `resolve_state_instance`. A transactor's own method
      body carries an EMPTY instance for a self-reference, so the first
      emission was `.cur.tag = 5;` — a leading-dot member access, not
      C++. The rejection had been hiding a broken path, and lifting it
      without compiling the result would have shipped exactly the
      silent-mis-lowering this sweep exists to remove.
      `transactor_record_field_test` is the equivalence fixture.

    - **`connect` endpoint diagnostics: reclassified, then reverted.**
      Six endpoint shapes (a sink with no method, a source with no event
      field, a source that is not an `out event`, a path segment naming
      something that is not a sub-component) and two sink SIGNATURE
      checks were reclassified — and all eight came back. What v1 does
      with a bad edge depends on where the edge SITS, not on how it is
      malformed:

      | landing | what v1 does |
      |---|---|
      | instantiated env, multi-segment path | emits the path verbatim into a `push_back` / range-for; usually does not compile |
      | single-segment endpoint | resolves the owner's own hookable / `out event` and WORKS (`E_take(_tb.top, _t)`) |
      | uninstantiated env | emits no wiring at all, so the malformed edge is invisible and v1 SUCCEEDS |

      tbir resolves `connect` for every env in the merged file, so it
      sees edges v1 never reaches at all. One site, three outcomes: no
      `V1Status` is honest, and all eight keep `Unsupported`.

    The record unlock also surfaced three follow-on gaps that the
    rejection had been hiding, each a design step rather than a missing
    arm, and each left with an honest diagnostic:

    - A `Vec` subfield inside a record state field (`cur.lanes[1] = 7`).
      v1 emits `self.cur.lanes[1] = 7;` and it works; the write path has
      no indexed form for `TransactorStateRecordFieldWrite`.
    - A record-typed field on a transactor reached through an `env`,
      which comes through the COMPONENT-field machinery.
      `ComponentFieldKind` has no record variant, so the field used to
      fall through to the DUT-handle arm and report "more than one DUT
      handle field (dut, cur)" — a diagnostic naming the wrong problem
      entirely. It now names the real one.
    - Record state in the `when active` position was closed in the same
      pass (v1 compiles either position, so half a feature was not worth
      shipping), as was the duplicate-name check the new branch had
      skipped — `cur : uint<32>` plus `cur : Beat` emitted two `cur`
      members instead of an error.

    **Three sweeps, three reverted reclassifications.** Batch 4 read one
    landing of a whole-`Vec` read, batch 5 read one landing of a
    directional field, batch 6 read one landing of a `connect` edge —
    each generalized, each wrong, each caught only by review. The
    working rule from here: a `V1Status` claim needs v1's output from
    EVERY landing the site can be reached from, and a site that cannot
    be pinned to one outcome keeps `Unsupported`. Closing a gap is the
    cheap half; classifying one honestly is the expensive half.

37. **Seventh probe sweep: a construct v1 emits but never runs
    (2026-08-16).** Three transactor item shapes, each probed in the
    outer, `when active`, and passive landings before anything was
    claimed.

    - **`on N cycles` on a transactor** already lowers in both
      declaration positions. Not a gap; nothing to do.
    - **`watchdog` on a transactor** looked like a clean gap — v1 emits
      a complete `<T>_watchdog` lambda with pre/post hook vectors, the
      `max_idle` check against `_last_in_cycle`/`_last_out_cycle`, the
      FAIL line and the error bump. Grepping for a CALL SITE turns up
      nothing, in all three landings. The control settles it: an AGENT
      watchdog does get one (`Producer_watchdog(_tb.prod)` inside a
      periodic closure), so this is specific to the transactor flavor.
      v1 compiles the construct and the watchdog silently never fires.
      `NotImplemented` / silently-mis-lowers, on ALL FIVE sites —
      unbound, bound-to target, and initiator-side.

      The first pass reclassified only the two unbound sites, on the
      belief that the bound flavors needed a bus declaration from a
      sibling file and so could not be reached. That was wrong: `bus …
      end bus` sits inline beside a bound-to transactor in
      `dma_engine_tlm_target_test`, and single-file probes of both bound
      flavors show the same defined-never-called lambda. Being too
      cautious is not free either — it left three sites telling users to
      re-run under a backend where their watchdog would not fire.
    - **`connect` on a transactor** was left UNCLASSIFIED at first — the
      initial probe used an empty block (nothing to wire, so it proved
      nothing) and the second tripped the separate `out event`-field
      gate before reaching the path. Settled in the next pass by a
      CONTROL DIFF rather than a grep: v1's emitted C++ is byte-identical
      with and without the block, in every landing. It does not even
      RESOLVE the edges — a nonsense edge naming two endpoints that do
      not exist produces the same identical output, where a backend that
      resolved anything would have errored. All five sites are
      `NotImplemented` / silently-mis-lowers.

      A regex-driven edit caught one neighbouring site
      (`transactor … TLM target threads`) that had never been probed;
      it was reverted before commit. Bulk-editing diagnostics is how an
      unverified claim gets in without anyone deciding to make it, so
      the per-construct count is asserted after every such pass.

    The control diff generalizes the earlier lesson: to tell "v1
    implements this" from "v1 discards it", emit the SAME program with
    and without the construct and compare. A construct that changes
    nothing in the output was never implemented, however cleanly it
    parses — and that is invisible to any probe that only looks at
    whether the compile succeeded.

    A diff needs BOTH anchors to carry weight. A positive one, so
    equality cannot pass by both sides being empty; and a negative one —
    the same construct somewhere it IS implemented, shown to change the
    output — so the equality is a property of the path under test rather
    than of the fixture. The `connect` test carries both, plus the `env`
    contrast, and fails if v1 ever grows a transactor implementation.

    **The refinement this sweep adds to the probe method:** "v1 emits"
    was never the question, and neither is "v1 emits code that
    compiles". The question is whether the emitted code RUNS. A
    definition with no call site compiles perfectly and does nothing,
    which is indistinguishable from a working feature until you look for
    the call — and the control matters as much as the finding, because
    "the name appears in the output" is satisfied by the dead shape
    itself (an unscheduled watchdog still emits its `_pre`/`_post`
    vectors and two internal hook loops). The test that pins this
    asserts a call COUNT against v1's emitter, and goes red if either
    call site is removed.

38. **Ninth probe sweep: the control diff gets a negative anchor, and
    a reachability check (2026-08-16).**

    - **`thread bus.<m>(…)` on an UNBOUND transactor** is discarded by
      v1: byte-identical output with and without the item, in both
      declaration positions. The NEGATIVE anchor makes that a statement
      about the unbound path rather than about target threads in
      general — the same item on a `bound to` transactor changes 42
      lines, so v1 serves them where it owns them.
      `NotImplemented` / silently-mis-lowers on both unbound sites.

    - **A lifecycle hook was reclassified and reverted, for a NEW
      reason.** `on build` on a transactor does change v1's output — so
      the control diff read "implemented" — and what it emits is a
      cycle-trigger closure whose predicate is the phase NAME as an
      expression, `(bool)(build)`, against a `build` it never declares.
      Differing output is not working output.

      But the reclassification still came out, because the construct
      never reaches the arm it was applied to: TB-IR parses `on build`
      as a cycle-trigger `on <expr>` too, and rejects it earlier with
      "the unresolved name `build`" — a diagnostic that is already
      correct and already carries the right status.
      `ComponentItem::Lifecycle` is reached by some other syntax that
      was never probed, so its arm keeps `Unsupported`.

    **The check this sweep adds: does the probe reach the SITE?** Every
    earlier rule was about reading v1 correctly. This one is about the
    other half of the comparison — a probe that trips a different tbir
    gate first tells you nothing about the arm you are editing, however
    good the v1 evidence is. The cheap test is to confirm the error
    message you get back is the one produced by the site you intend to
    change.

39. **Tenth probe sweep: addrmap bases, and a comment that was wrong
    (2026-08-16).**

    A non-literal `@ <base>` or `size` on an addrmap instance is
    `NotImplemented` / silently-mis-lowers. The comment on
    `addrmap.rs::fold_const` had claimed v1 "const-folds arbitrary
    expressions; the corpus uses literals exclusively" — the second half
    was true and the first half was not. `@ 0x50 + 0x10` folds to ZERO
    under v1, which emits `AxilHelper_write(helper, (0 + 0x18), …)`: the
    testbench writes register 0x18 instead of 0x78 and reports nothing.
    A non-literal `size` collapses the same way, taking the
    window-overlap check with it — which is why v1 accepted an addrmap
    whose instances overlap.

    **Reaching the site took three attempts, and rule 4 caught all
    three.** The fixture `use`s a bus declaration the CLI cannot
    resolve; an inline bus fixed that but used the wrong channel
    keyword; and even then the addrmap arm never ran, because an
    addrmap that is declared but never BOUND AND USED is inert. The
    tell each time was the control failing with the same diagnostic as
    the test case — a probe whose control does not lower is measuring
    something other than the construct.

    Folding constants here (as the field defaults do, divergence 35)
    would put TB-IR ahead of v1 and is the natural next step; it needs
    the file constant table threaded to that call site.

    Two covergroup findings from the same batch are recorded WITHOUT a
    classification, because their anchors failed:

    - A runtime slice bound in a coverpoint (`cover dut.a[dut.b:0]`)
      emits identically to a whole-port coverpoint under v1 — which
      reads as "the slice is dropped" until the negative anchor is
      tried: a CONSTANT slice (`dut.a[3:0]`) emits identically too. So
      v1 appears to drop EVERY coverpoint slice, and TB-IR, which emits
      a real one for the constant case, is ahead in a way nothing has
      documented. Establishing that properly wants TB-IR's output as
      the anchor rather than v1's.
    - A non-constant bin spec (`lo = {dut.b}`) is unclassified: the
      control compared one bin against two, so the diff was not
      measuring the bin spec at all.

40. **Eleventh probe sweep: a const coverpoint slice bound, and a
    withdrawn claim (2026-08-16).**

    - **A file-scope `const` as a coverpoint slice bound**
      (`cover dut.count_out[K:0]`) now folds. v1 emits one as
      `(uint32_t)(K)` against its own `static constexpr K` — identical
      semantics to the literal — while TB-IR accepted only a plain
      integer. Same shape as the field-default fold in divergence 35,
      through the constant table this file already carried.

    - **The claim that v1 drops every coverpoint slice is WITHDRAWN.**
      It came from two synthetic covergroup fixtures whose diffs showed
      byte-identical output — because a covergroup that is never
      INSTANTIATED emits no sampling logic at all, so both files were
      being compared on scaffolding. `cov_expr_targets_test` covers
      `dut.count_out[3:0]`, `[0:0]`, `[3:0].sext<8>()` and
      `[3:0].trunc<2>()`, is in the equivalence registry, and passes
      under both backends. Constant coverpoint slices were never broken.

    **The check this sweep adds, and it would have saved two batches:
    look in the equivalence registry BEFORE writing a probe.** A
    registry row is stronger evidence than any synthetic fixture — it
    says both backends emit AND that the traces match. And when a probe
    is needed, mutating a registered fixture one token at a time gives a
    control that is known-good in both backends, which is exactly what
    the synthetic attempts lacked. Three of this session's rules are
    about reading v1 correctly; this one is about not writing the probe
    at all when the answer is already recorded.

41. **Twelfth probe sweep: regblock offsets and resets fold to zero
    too (2026-08-16).**

    The same defect divergence 39 found in `addrmap`, in `regblock`: v1
    folds a non-literal register `@ <addr>` offset or `reset` value to
    ZERO. Both sites are `NotImplemented` / silently-mis-lowers.

    The offset case is the worse of the two. The emitted address TABLE
    entry becomes `{ "SRC", 0, 32 }` and the decode becomes
    `addr == 0`, so the register aliases whatever lives at offset 0 —
    `CTRL`, in the fixture — and its reads and writes silently hit a
    different register. The reset case starts the mirror at 0 instead of
    the declared value, so every readback compares against the wrong
    baseline.

    Three shapes verified — a `const` offset, an arithmetic offset, and
    a `const` reset — each by mutating `regblock_subset_test` (in the
    equivalence registry, passing under both backends) ONE TOKEN at a
    time, per the rule the previous sweep added. The control lowers in
    both backends, so the probes provably reach the regblock arms; the
    negative anchor is a literal `0x00` offset, which changes the same
    eight lines the fold does.

    That `addrmap` and `regblock` share the defect is not a
    coincidence — both hand-rolled a literals-only `fold_const` next to
    a comment assuming v1 was more capable. The pattern to watch for is
    a local constant-folder: every one found so far has been narrower
    than the file's own comment claimed, and v1's behaviour on what it
    rejects has been wrong rather than absent.

42. **Thirteenth probe sweep: the local-folder lead, and what it did
    NOT predict (2026-08-16).**

    Divergence 41 flagged a grep-able pattern: a hand-rolled
    literals-only `fold_const` sitting next to a comment that assumed v1
    was more capable. Grepping the lowering for it found a fourth
    instance, in `records.rs`, on `transaction` / `struct` field
    defaults.

    **A `const` default on a record field now folds**, through the same
    shared table the component / scoreboard / transactor-state defaults
    use (divergence 35).

    The lead was worth following and its prediction was wrong, which is
    the point worth recording. One code pattern, four sites, THREE
    different v1 behaviours behind them:

    | site | what v1 does with a non-literal |
    |---|---|
    | component / scoreboard / transactor-state default | emits `= K` against its own `static constexpr` — correct (closed, divergence 35) |
    | record / struct field default | same — correct (closed here) |
    | addrmap `@ <base>` / `size` | folds to ZERO — silently wrong (divergence 39) |
    | regblock `@ <addr>` / `reset` | folds to ZERO — silently wrong (divergence 41) |

    So the pattern is a good way to FIND candidates and no way at all to
    classify them. Had the records site been reclassified by analogy
    with the two nearest neighbours, it would have been labelled
    silently-mis-lowers and left rejected, when v1 handles it correctly
    and the right move was to close the gap. Every instance still needs
    its own probe; what the lead buys is knowing where to point one.

43. **Fourteenth probe sweep: `bus.rs` reaches zero `Unsupported`
    sites (2026-08-17).**

    The six arms left in `src/ir/lower/bus.rs` all classified, none of
    them a v1 escape hatch:

    | shape | v1 |
    |---|---|
    | direct (non-`fork`) call on an `out_of_order` tlm_method | rejects — "supports only `blocking` tlm_method calls" |
    | `fork <not-a-call>` | rejects |
    | `fork <bare-ident>(args)` | rejects |
    | `fork <a>.<b>.<m>(args)` | rejects — "requires `bus.method(args)`" |
    | bus channel method outside `send` / `recv` | **splits** — see below |
    | `= bind <non-dut>` | **emits uncompilable C++** |

    Four of the six are `fork` shape guards that v1's
    `try_emit_bus_tlm_fork` carries one-for-one, in the same order and at
    the same granularity — unsurprising once seen, since the TB-IR arms
    were written against that function. That correspondence is the useful
    positive result: where one backend's guard was transcribed from the
    other's, the classification is `Rejects` and the arms can be swept
    together rather than one at a time.

    The two that break the pattern are the two that had no v1
    counterpart to transcribe, and both needed a SECOND probe to get
    right:

    * **Channel methods split on the method NAME.** `strm.s.poke()` is
      not a signal on `s`, so v1 resolves it against the channel's
      signal list and refuses, with a better message than ours. But
      `strm.s.data()` IS a signal, and v1 emits it as a signal read with
      the call parens left on: `harc_rt::harc_read(dut->strm_s_data)()`.
      `harc_read` returns a value, so that is "expression cannot be used
      as a function". One probe (`.poke`) said `Rejects`; the arm is
      `EmitsUncompilable`, because the status of an arm is the WORST
      thing v1 does anywhere under it, not the first thing it does.

    * **Bind targets fail two different ways.** v1 never resolves a bus
      bind target — it substitutes the bind EXPRESSION where the DUT
      pointer goes and dereferences it. `= bind nope` emits
      `nope->mem_read_addr` and 71 more lines against a symbol it never
      declares; `= bind dut.core` emits
      `harc_rt::harc_read(dut->core)->mem_read_addr`, a real DUT member
      read with `operator->` applied to the value it returns. The
      classification survived the second probe, but the DIAGNOSTIC did
      not: written from the bare-ident case alone, it told users v1
      "names a symbol that does not exist", which is false for every
      field path.

    Both are the same lesson at the level of a single arm that "every
    landing" is at the level of a construct: one arm can cover more than
    one v1 behaviour, so the probe has to sample the arm's INPUT space,
    not just reach it once.

    Two mode arms also turned out to be **unreachable**: the shared
    parser (`parse_tlm_method_decl`) admits only `blocking` and
    `out_of_order`, and `try_lower_tlm_fork` handles both. The arm is
    kept as a defensive `Rejects` because v1's fork path carries the
    identical unreachable arm and errors in it — so a third mode added
    to the shared parser would be rejected by both backends, and the
    classification stays true without anyone re-probing it.

    A probing note: the channel-method site needed a `.send` / `.recv`
    call in a fixture whose bus is declared LOCALLY (the registered
    `axilite_bus_send_test` resolves its bus through `use BusAxiLite`,
    which the unit-test harness does not do). `stream_burst_mon_test`
    fits, and adding a `recv()` to it is itself a control — both
    backends emit — which pins the `.poke` rejection on the method name
    rather than on the call site being new.

44. **Addrmap and regblock addresses now fold — the last two
    fold-to-zero sites closed (2026-08-17).**

    Divergences 39 and 41 found the two places where v1 does not reject
    a non-literal address; it accepts one and yields ZERO:

    | site | what v1 emits |
    |---|---|
    | addrmap `@ <base>` | `AxilHelper_write(helper, (0 + 0x18), …)` — the write lands at 0x18 instead of 0x78 |
    | addrmap `size` | the window collapses to 0, so the overlap check stops firing |
    | regblock `@ <addr>` | table entry `{ "SRC", 0, 32 }` and decode `addr == 0` — the register aliases offset 0 |
    | regblock `reset` | `uint32_t CTRL = 0;` — the mirror starts wrong and every readback compares against it |

    All four now fold through the file constant table, via a shared
    `fold_addr_const` that is the address counterpart to
    `components::fold_field_default` (divergence 35). The two differ in
    exactly the way their call sites do: a field default carries a
    `TypeExpr` and is range-checked against it, an address carries none
    and is checked against zero instead.

    **This puts TB-IR ahead of v1 rather than level with it**, which
    changes what a test can assert. A program using a folded address
    cannot go in `tests/tbir_equiv_fixtures.txt` — the two backends
    genuinely disagree, because one of them is wrong — so the registry
    cannot be the end-to-end evidence here the way it is for a closed
    gap.

    What replaces it: every folding spelling is asserted **byte-identical
    to the literal it computes**, against `regblock_subset_test` and the
    self-contained addrmap testbench. Byte-identity with a program CI
    already runs under Verilator is the end-to-end argument — the fold
    emits exactly the translation unit the registered fixture emits — and
    the paired `assert_ne!` against a different constant is what stops
    that equality from being the value silently dropped. v1's
    fold-to-zero is pinned in the same tests, so a change to it is caught
    rather than assumed.

    Folding also widened the accepted set, which needed two checks the
    literals-only gate never had to make:

    * a **negative** address (`const B = 0 - 8`) is `Invalid` — an
      address cannot be negative under any backend;
    * a **reset wider than the register** (`const R = 4294967296` on a
      `uint<32>`) is `Invalid`. A literal past the width used to be
      caught downstream by C++ narrowing; a `const` reaches lowering
      first, so lowering checks it.

    That is the same failure mode divergence 36 recorded for the field
    defaults, arriving a second time at a different site. Worth stating
    as a rule: **whenever a fold replaces a literals-only gate, the range
    check the literal got for free has to be written by hand.**

    A shape that still does not fold (`@ dut.count_out`) stays
    `SilentlyMisLowers`, not `Rejects`: v1 accepts it and emits address
    0, so pointing the user there would hand them a register at the
    wrong address.

    **One shape here is the exception, and rule 6 caught it twice.**
    A Verilog-sized literal (`@ 32'h18`) is `Unsupported` — pointing at
    v1 — because TB-IR does not lower one at an address site while v1's
    `c_int_literal` emits `{ "SRC", 0x18, 32 }`, correctly. (The scope of
    that claim was later narrowed: TB-IR *does* lower sized literals in
    `keep` constraints. See divergence 49 — the arm's classification is
    unaffected, since it is about this site.) Sweeping it into the
    `SilentlyMisLowers` mapping with everything else that will not fold
    would have replaced a correct diagnostic with a false claim about
    v1. That was the first catch.

    The next two came from the fix, and both were the SAME
    over-generalization from that one probe:

    * having probed `32'h18`, the obvious move is a walk over the
      expression tree so `32'h10 + 0x08` is classified by its literal
      too. **Wrong** — v1's `c_int_literal_from` matches `ExprKind::Int`
      and nothing else, so a sized literal inside an expression, and
      even a parenthesised one, falls to its `"0"` arm. `@ 32'h18` and
      `@ (32'h18)` are opposite classifications.
    * the equally obvious "any literal `parse_int_literal` rejects" test
      is also wrong: an over-wide literal (`0x10000000000000000`) is
      unreadable by that parser too, but v1 emits it as a `_harc_u128`
      composite that truncates into the 64-bit table field —
      `{ "SRC", (((_harc_u128)0x1ULL << 64) | ...), 32 }`, offset 0
      again.

    So the arm is narrowed twice, top-level AND sized-only, each
    narrowing pinned by a test that reads v1's actual table entry.
    One arm, one probe, three wrong generalizations available from it —
    first by not looking, then twice by reasoning outward from a single
    look instead of running the next probe.

45. **Covergroup bin exact VALUES may be runtime expressions
    (2026-08-17).**

    `cg-nonconst-bin` had been carried as unresolved for several sweeps
    because the first probe measured nothing — the control compared a
    one-bin covergroup against a two-bin one, so the diff was dominated
    by the bin count and said nothing about the bin spec. Re-probed
    properly against `cov_runtime_bound_test` (registered, passing under
    both backends), mutating one token at a time, it is a **real gap**:

    | spelling | v1 | TB-IR (before) |
    |---|---|---|
    | `en0 = dut.en` | `_v == harc_rt::harc_read(dut->en)` | rejected |
    | `en0 = {dut.en}` | same | rejected |
    | `en0 = {0, dut.en}` | same, alongside the literal | rejected |
    | `en0 = {dut.en + 0}` | the expression, per-sample | rejected |

    All four landings, working code in every one. The odd part is what
    the code already contained: range BOUNDS had carried
    `CovBinBound::Runtime` since #494, so `[dut.en .. 7]` worked while
    `{dut.en}` was refused **in the same bins block**. v1 never had that
    split — it renders both with the same `emit_expr`.

    The fix is the shape of the IR, not a new code path: `CovBinValue::
    Eq(u64)` becomes `Eq(CovBinBound)`, and the exact-value arm calls
    the same `lower_bin_bound` a range end does. The constant fast path
    is untouched, so the emitter, the dump-IR printer, and every
    existing bin keep their behaviour; the sampler-subset diagnostic is
    now shared rather than duplicated. Two `Unsupported` sites removed
    by deleting code.

    Registered as `cov_runtime_bin_value_test`, so the equivalence
    harness trace-diffs the two backends on it — unlike divergence 44's
    folds, this one is a gap CLOSED, so the registry is available as
    evidence and is the right place for it.

    The lesson is about the first probe, not the second: a control that
    differs from the mutation in more than one respect measures the
    wrong thing, and "no signal" reads identically to "no gap". This one
    sat unresolved for five batches because of it.

    **Closing the gap uncovered a second, opposite one.** Runtime bin
    members and bounds are NOT emitted byte-for-byte with v1:
    `cover_expr_cpp` parenthesises a compound expression, v1's
    `emit_expr` does not. TB-IR's rendering therefore always differs;
    what varies is whether the difference MATTERS. For an operator that
    binds tighter than the comparison it does not — the two group the
    same way —

        {dut.en + 4}  ->  _v == harc_read(dut->en) + 4      (v1)
                      ->  _v == (harc_read(dut->en) + 4)    (TB-IR)

    — which is why the registry fixture can use `+`: the trace diff
    compares behaviour, not text. For an operator that binds looser, the
    difference is the whole meaning, and v1 is **wrong**:

        {dut.en | 8}  ->  _v == harc_read(dut->en) | 8      (v1)
                      ->  _v == (harc_read(dut->en) | 8)    (TB-IR)

    C++ groups v1's as `(_v == en) | 8` — non-zero on every sample, so
    the bin always hits. TB-IR counts what the user wrote. The same was
    already true of range bounds; the widening made it reachable in one
    more position.

    So a single construct is a gap in one direction and a divergence in
    the other, depending on the operator: `+` goes in the equivalence
    registry, `|` cannot, because the trace diff would fail on v1's bug.
    Both are pinned by
    `a_low_precedence_bin_value_is_a_place_v1_is_wrong`, so a later
    "make the emitters byte-identical" change has to confront the bug
    rather than adopt it.

    Worth generalizing: **byte-identity with v1 is the goal only where
    v1 is right.** Every previous slice could treat "matches v1's
    output" as the success condition; here that would mean reproducing a
    precedence bug. The equivalence registry encodes agreement, not
    correctness, and those come apart exactly where v1 is wrong —
    divergence 44's addrmap folds and this precedence case are the two
    known places so far.

46. **The coverpoint subset gate: one arm, four v1 behaviours
    (2026-08-17).**

    `lower_point_target`'s catch-all was a single blanket `Unsupported`
    — "re-run with `--codegen v1`" for every expression the coverpoint
    subset declines. It serves four landings (point target, hook param,
    bin range bound, bin exact value), so it is the single most-reached
    rejection in covergroup lowering.

    **v1 has no subset gate here at all.** It renders the expression
    with `emit_expr` and casts to `uint64_t`, so whatever the user wrote
    lands in the sampler verbatim. What varies is whether that text
    compiles and whether it means anything — which is why one
    classification could never have been right.

    Enumerating every `ExprKind` the arm can receive, and probing each
    at all four landings:

    | v1 does | shapes | status |
    |---|---|---|
    | compiles, samples the WRONG value | `[1..2]` in scalar position, `1.5`, `"x"` in a target | `SilentlyMisLowers` |
    | emits, does not compile | unknown name, `foo.bar`, `foo[1]`, `.en`, `undefined_fn(1)`, `dut.en.nope()` | `EmitsUncompilable` |
    | refuses | `{1,2}`, `randomize(e)`, `fork`, `##1 e`, `$clog2(e)` | `Rejects` |
    | never reaches it | `dist{}`, `a <- b`, `e[*3]` — parse errors in both | unreachable |

    The load-bearing case is `cover [1..2]`: v1 emits
    `(uint64_t)(/* range 1..2 */ 0)` — its own emitter leaves a comment
    admitting it dropped the bounds — and the coverpoint then samples
    ZERO on every cycle with no diagnostic on either side. Sending a
    user there was the worst thing the old blanket arm did.

    Two things nearly went in wrong, both about the probe rather than
    the code:

    * **The first sample was unrepresentative even though it reached
      the arm at every landing.** It was all garbage input — unknown
      names, undefined calls, strings, floats. Those are malformed
      programs; the arm exists to reject expressions that are
      well-formed HARC but outside the SAMPLER subset. "Reaches the
      arm" and "is the population the arm serves" are different
      properties, and only the second one classifies it. Re-probing
      with well-formed input is what surfaced `[1..2]`.

    * **`$clog2` was probed with the wrong spelling.** Written
      `clog2(dut.en)`, without the sigil, it is not a system call — it
      is an ordinary call to a name nothing declares, which v1 emits
      verbatim into uncompilable C++. That made it look like an
      `EmitsUncompilable` gap in legitimate language surface, and it was
      briefly written up as "a real gap worth implementing". With the
      sigil, v1 rejects it outright. Two spellings, two arms, opposite
      classifications — pinned by
      `the_sigil_is_what_makes_a_system_call_a_system_call`.

    One more trap avoided: `{[1..2]}`, a range NESTED in a set, never
    reaches this arm — `lower_bin_values` has its own `RangeLit` arm and
    lowers it correctly today. Only a range in SCALAR position lands
    here. Sweeping the two together would have broken working code.

    A method note that cost a wrong reading: `dut.en.nope()` first
    compile-tested as OK because the probe declared a stand-in struct
    with a `nope()` member. Against `harc_read`'s real return type it
    fails. **A compile probe has to include the actual runtime header,
    not a plausible model of it.**

    **Review then found the same input-space mistake a third time, in
    the fix itself.** The `Ident` arm was probed with UNKNOWN names only,
    so a bare `Ident` was classified `EmitsUncompilable` wholesale — but
    `cover MY_CONST` is a known const, and v1 emits its own
    `static constexpr uint64_t MY_CONST = 7;` and samples it, correctly.
    That is v1 working, which the blanket `Unsupported` had (accidentally)
    been right about. Fixed by closing the gap instead: a const target
    now folds through the same table `lower_bin_bound` uses, byte-identical
    to the literal it computes.

    Two structural bugs came out of the same review, both invisible to
    the probe suite because they are about which NODE gets classified:

    * the classifier was closed over the top-level `target`, while
      `lower_point_target` descends through parentheses before failing —
      so `cover ([1..2])` was classified by the parenthesis and reached
      the catch-all, losing the range arm the whole change exists for.
      It now takes the node the walk failed on.
    * a sized literal (`cover 32'h7`) reached the same catch-all. v1
      lowers a bare one correctly, so that arm keeps `Unsupported` —
      the identical split divergence 44 records for addrmap and regblock
      addresses.

    Three rounds, three variants of one error: **an arm is not classified
    until its input space is sampled, and "I probed that arm" is not the
    same claim as "I probed that arm's inputs."**

47. **The four small files: `addrmap.rs` reaches zero (2026-08-17).**

    Ten sites across the four files with the fewest of them. Eight moved,
    two were already right, and the two that were right are the
    interesting ones — both sit beside a site that moved.

    | site | was | is | v1 |
    |---|---|---|---|
    | `tseq X()` with no `-> TSeq<T>` | `Unsupported` | **unchanged** | defaults to `std::vector<int64_t>` — works |
    | `TSeq<BadType>` | `Unsupported` | `EmitsUncompilable` | emits `std::vector<NoSuchType>` |
    | regblock register width 0 / 65 | `Unsupported` | `Invalid` | falls back to `uint64_t`, silently loses a bit |
    | regblock zero-width field | `Unsupported` | `Invalid` | mask `0x0u` — reads 0 forever, writes are no-ops |
    | regblock unknown register access | `Unsupported` | `EmitsUncompilable` | emits `NOPE.RS = 1`, not a member |
    | addrmap unknown instance access | `Unsupported` | `EmitsUncompilable` | same |
    | `for x in <scalar>` | `Unsupported` | `EmitsUncompilable` | range-for over a value with no `begin()` |
    | non-literal timeout `fail(...)` | `Unsupported` | `SilentlyMisLowers` | **discards the message**, substitutes its own |
    | record-API non-constant address | `Unsupported` | **unchanged** | genuine runtime decode — works |

    `src/ir/lower/addrmap.rs` now has zero `Unsupported` sites, joining
    `bus.rs`. regblock went 4 → 1, control 3 → 1, tseqs 2 → 1.

    **The two `tseqs.rs` sites are four lines apart and classify
    oppositely.** The difference is whether the element-type annotation
    is ABSENT or PRESENT-but-unresolvable: absent makes v1 substitute a
    working default, present-and-bad makes it print the name into a type
    position. One code path handles both, which is exactly how they came
    to share a classification. Worth naming as a shape — **an absent
    annotation and an invalid one are different input classes**, and a
    probe that only omits the annotation will never find the second.

    The `control.rs` pair splits the same way for a different reason:
    `for x in <scalar>` fails loudly at compile time, while a non-literal
    timeout message fails silently at runtime — v1 emits
    `sim_log_line("FAIL", "wait until timed out after %lld cycles", …)`
    in place of whatever the user wrote. The failure still fires; the
    diagnostic just isn't theirs.

    **Rule 4 earned its keep, via a test rather than a probe.** The
    regblock access catch-all was probed with two mutations, an unknown
    register and a method call, and both were rejected — so both looked
    like they reached it. They did not: `regs.reset_all()` is
    intercepted by generic statement lowering (`stmts.rs`) well before
    regblock access resolution, and is still `Unsupported` because that
    is a different site in a file this batch never touched. The probe's
    exit status could not tell them apart; the test asserting the
    message did. **A rejection is evidence that SOMETHING rejected, not
    that the thing under edit did** — and the cheapest way to hold
    yourself to that is to assert on the message text, not on the error
    kind.

    Review then found the same shape twice more, in this batch's own
    comments and tests: the `reset_all()` example was left in BOTH
    access-catch-all comments as though it reached them, and the test
    named for both catch-alls exercised only the regblock one — so the
    `addrmap.rs` change, the batch's headline, shipped untested. Both
    fixed; the addrmap case now uses the self-contained `ADDRMAP_TB`
    helper already in the file.

    A fixture note, since it cost two attempts. A zero-width FIELD is
    reachable only past the register-width check, and the natural
    fixture for it (`regblock_fields_test`) resolves its bus through
    `use BusAxiLite`, which the unit-test harness does not search for —
    so the field has to be grown on a self-contained fixture instead.
    And a field with no ACCESS site emits nothing at all, so v1's `0x0u`
    mask is not visible there either (rule 2, in a place it is easy to
    forget). What the test can show is the claim that classifies the
    site: v1 does not check.


48. **`tseqs.rs` reaches zero — and a probe conclusion that the test
    suite overturned (2026-08-17).**

    Two sites left in the small files, both already probed in divergence
    47 as cases where **v1 is correct** — so both were gaps to CLOSE, not
    diagnostics to reclassify.

    **`tseq X()` with no `-> TSeq<T>` now defaults** to a signed 64-bit
    element, byte-identically to v1's `-> std::vector<int64_t>`. The
    annotated form is untouched and still emits the type it declares.
    `tseqs.rs` joins `bus.rs` and `addrmap.rs` at zero `Unsupported`
    sites.

    Deliberately NOT merged with the bad-element-type arm beside it: an
    absent annotation is defaultable, a present-but-unresolvable one is
    not (divergence 47).

    **The other site produced a wrong conclusion, and the existing test
    suite caught it.** Four probes at `control.rs`'s transactor-edge arm
    each landed in a *different* upstream interceptor — a non-DUT field
    access, `exprs.rs`'s transactor/method call arm, a helper-call arm,
    and (for a regblock read) nothing at all, since `RegRead` is not a
    transactor edge. From four misses I concluded the arm was
    unreachable and reclassified it as a defensive
    `SilentlyMisLowers`.

    `cargo test` then failed on
    `transactor_self_call_in_wait_until_predicate_is_rejected`, a test
    that predates this whole sweep and reaches the arm on its first try
    — with the one spelling I had not written: a bare call to a sibling
    **hookable** inside a transactor's own `when active` block. My
    probes used `function`, which is a helper, and cross-transactor
    calls, which are refused earlier.

    Probing the right spelling shows v1 emits
    `while (!(Xt_ready(self))) tick();` — exactly what re-evaluating a
    predicate means, and it works. **`Unsupported` was right all
    along**, and the change was reverted.

    Two things worth keeping from that:

    * **A negative result from N probes is not a proof of
      unreachability.** It is evidence about the N spellings tried.
      "Unreachable" is a claim about the whole input space and needs a
      structural argument — the parser excludes it (divergence 43), or
      the enum has no other variants — not an accumulation of misses.
    * The regression suite is a probe corpus that was already written.
      Grepping it for a site's own message before concluding anything
      about reachability would have answered this in one command.

    The input-space sample did find something the single spelling would
    have missed: if the called hookable itself BLOCKS, v1 emits the same
    loop while the callee contains `for (int _w = 0; _w < 1; _w++)
    tick();`, so time advances inside the callee and again in the loop.
    It compiles and runs; the timing is not what the source reads like.
    That is recorded at the site as the reason a future split may be
    wanted — it needs the callee's body, which lowering does not have
    there.

    **Review then caught the default swallowing two things it should
    have rejected** — the same failure mode as divergences 36 and 44,
    third time in this sweep: *a default that replaces a rejection
    inherits the rejection's job.*

    * The default was the only thing rejecting an UNANNOTATED tseq whose
      body yields a RECORD. That began lowering to
      `std::vector<int64_t>` with `push_back(t)` on a struct —
      uncompilable C++, no diagnostic. `lower_yield`'s scalar arm now
      refuses a record value; it had never needed to, because every
      scalar-element sequence used to come from an explicit
      `TSeq<uint<N>>`.
    * The arm was the fall-through for any annotation that is neither a
      scalar builtin nor a single identifier, not just an absent one, so
      `TSeq<int>`, `TSeq<time>` and `TSeq<Vec<uint<8>, 4>>` all became
      `vector<int64_t>` — where v1 renders `vector<uint64_t>` and
      `vector<std::array<uint64_t,4>>`. Now gated on v1's own condition
      (`tseq_args` absent), so absent and present-but-unusable stay
      separate, which was the point of the split in the first place.

    A second review round found three more leaks from the same widening,
    which is the number worth recording: a **sequence**-typed yield slid
    past a record-only guard, a **present non-`TSeq` return** (`-> Req`,
    `-> uint<8>`) has no `TSeq` args either and so fell into the default
    arm, and the guard's own message shipped a literal `<Record>`
    placeholder. The gate is now `return_ty` being absent — not "the
    element type failed to resolve" — and the yield guard rejects any
    non-scalar local, naming the record to annotate with.

    Five leaks across two rounds, from one nine-line default. The rule
    is not new; what this batch adds is its cost. **Widening a gate
    inherits every check that gate was silently performing, and they are
    only visible by enumerating what the OLD arm used to reject** — not
    by testing what the new one now accepts.

49. **Correction: TB-IR does lower sized literals — in `keep`
    constraints (2026-08-17).**

    Divergences 44 and 46, and the diagnostics they shipped, asserted
    that TB-IR "does not lower sized literals ANYWHERE yet". That is
    **false**, and it reached users: the messages in `fold_addr_const`
    and the coverpoint classifier both said it.

    ```
    transaction Req
        a : uint<8>
        keep a == 8'h0F
    end transaction Req
    ```

    lowers under **both** backends today. The gap is specific to
    STATEMENT position (`let z = 32'h18`) and to the address/coverpoint
    sites; the constraint path has always handled these.

    Caught by PR #591 (a different session, fixing sized-literal width
    semantics), whose author found the same false claim in the spec and
    corrected it there. Re-verified here directly rather than taken on
    report, then fixed in all four places — two shipped diagnostics, one
    test comment, and divergence 44's own wording.

    The lesson is about how the claim was formed. It came from a probe
    that hit five sites — statement position, regblock offsets, regblock
    resets, coverpoint targets, addrmap bases — and generalized from
    "rejected at every site I tried" to "rejected everywhere". Five
    agreeing observations felt like enough. But they were five instances
    of *one* code path — `exprs.rs`'s `parse_int_literal`, which every
    lowering entry point reaches and which does not strip the `N'r`
    prefix. Constraints never route a sized literal through it: they have
    their own prefix-stripping parser in
    `src/constraints/typed_lower.rs`. **A sample that is large but not
    diverse measures one thing repeatedly.**

    Stated that precisely on the second attempt. The first draft of this
    entry said "the constraint path never goes through
    `parse_int_literal`" — which is false, since `cpp_tb.rs`'s wrap-mask
    width oracle calls it directly; it just short-circuits on the `'`
    first. A structural argument is only worth more than a count if it
    is checked as carefully as one, and the version that survives a grep
    is the one to write down.

    That is the same error as divergence 48's unreachability claim, at a
    different scale: N misses is evidence about the N routes tried, not
    about the space. The fix in both cases is the same — a structural
    argument, or a grep for the construct across the corpus, rather than
    an accumulating count.

    Practical consequence: **sized-literal lowering is no longer a
    candidate for this sweep.** #591 owns that surface, and the honest
    remaining gap is narrower than the "five sites" this file previously
    recorded.

    Re-verified after #591 merged, since it changes sized-literal
    PARSING and could have invalidated any of this: `keep a == 8'h0F`
    still lowers under both backends, `register SRC @ 32'h18` still
    rejects under TB-IR while v1 still emits `{ "SRC", 0x18, 32 }`, and
    the coverpoint control still lowers. The corrections above survive
    the merge. Worth doing rather than assuming — a claim about another
    change's blast radius is exactly the kind that reads as obviously
    fine and is cheap to check.

50. **`components.rs`: a wide surface, and the first family probed
    (2026-08-17).**

    72 sites but **62 distinct messages** — the opposite shape from the
    coverpoint gate (divergence 46: one arm, four landings). Almost every
    site here is its own construct, so no single split clears a large
    fraction, and file order would be arbitrary. Grouped instead by what
    a user would have to WRITE to reach them, so one fixture serves a
    family: field declarations, paths/connects, handlers, method calls,
    plus one genuine catch-all.

    **Family A (field declarations) — the uniformity hypothesis was
    wrong, which is why it was tested.** Several of these sites share a
    `lower_*_field` helper, so v1's behaviour looked likely to be uniform
    across them. Three probes, three different behaviours:

    | field | v1 emits | compiles? |
    |---|---|---|
    | `event<Vec<uint<8>, 4>>` | `std::vector<std::function<void(std::array<uint64_t,4>)>>` | **yes** |
    | `queue<Vec<uint<8>, 4>>` | `harc_rt::HarcQueue<std::array<uint64_t,4>>` | **yes** |
    | `weird : NoSuchThing` | `VNoSuchThing* weird = nullptr;` | no — undeclared |

    The first two are genuine escape hatches and keep `Unsupported`. The
    third is the sub-component catch-all, and it splits again on its own
    input space: a name declared NOWHERE is a typo (v1 invents a
    Verilated handle type), while a DECLARED type that merely is not a
    supported sub-component kind — a `covergroup` — v1 handles
    **correctly**, emitting `ExtraCov weird;` and wiring
    `env.weird.cp.b0++`.

    So the arm now splits: undeclared → `Invalid` (a program error under
    every backend), declared-but-unsupported → `Unsupported` (v1 works).
    Distinguishing them needed the set of declared type names threaded
    into `lower_field`, which had component/scoreboard/record tables but
    no covergroup or regblock names.

    Two method notes from this batch:

    * A probe aimed at the "scalar field of an unsupported type" site
      (`odd : float`) landed in the sub-component arm instead — `float`
      parses as a Named type, not a builtin. Rule 4 again, and again the
      tell was the message rather than the exit status.
    * The compile probe for `HarcQueue` first reported "not a member of
      `harc_rt`" because the probe included `harc_thread_rt.h` but not
      `harc_queue_rt.h`, which the emitted file does include. Including
      *the real header* is not enough — it has to be the same SET the
      generated translation unit uses.

    **Review then found the fix repeating the bug it fixed.** The
    declared-type set was written as a whitelist of the item kinds that
    seemed relevant, and it omitted `enum` — so `weird : Mode` against a
    declared `enum Mode { A, B }` began hard-failing with "not declared
    anywhere in the file", while v1 emits a working `Mode weird;`. A
    declared-but-unsupported kind swept into the typo bucket: exactly the
    defect the split existed to remove, inverted.

    The repair is not "add `enum`" but noticing the failure modes are
    **asymmetric**. A name missing from the set is a false hard error on
    a valid program; a spurious extra name merely routes back to the
    honest `Unsupported` the arm gave before. So the set is now
    deliberately over-inclusive — every item that carries a name goes in,
    whether or not it can currently appear in field position.

    Worth stating generally: **when a lookup drives a claim, work out
    which direction of error is recoverable and bias the lookup that
    way.** A whitelist assembled from "what seems relevant" fails toward
    the unrecoverable side by construction, and no amount of care in
    assembling it changes that.

    The same review found the batch's other arm documented but not
    fixed: the coverpoint `ExprKind::Int` arm had a comment explaining
    that sized and over-wide literals need opposite classifications, and
    still returned one for both. Split now, on the same `'` the address
    site uses.

51. **`components.rs` family E: the catch-all, and a probe that
    measured nothing twice (2026-08-17).**

    `ComponentItem::TargetTlmThread | Apply` shared one arm and one
    message. The variant list is the whole input space — enumerable, so
    this is a structural argument rather than a count.

    **`thread` on an env/agent is `SilentlyMisLowers`.** v1 accepts it,
    emits the component struct, and drops the thread: no
    `harc_rt::ThreadSlot`, no `sched.slots.push_back`, no serving
    coroutine. A user writes a target-serving thread and the target
    never serves, with no diagnostic.

    Getting there took three probes, and the first two measured nothing:

    * Adding a `thread` to the `analysis_sink_connect_test` env left
      v1's output byte-identical, which reads as "v1 drops it".
    * So did adding the same thread to a TRANSACTOR in that fixture —
      where the construct is supposed to be supported. That is the
      tell: an **unbound** thread emits nothing under v1 wherever it
      sits, so neither comparison had any signal in it. Rule 2, and the
      only reason it surfaced was checking the anchor rather than
      banking the first agreeing result.
    * Against `tlm_target_thread_test`, where the transactor is
      bus-bound, removing the thread DOES change v1's output. With that
      anchor established, moving the same thread into an `env` adds only
      an empty `struct WrapEnv { … }` — and the conclusion is finally
      supported.

    The test carries both anchors, and counts `ThreadSlot` occurrences
    rather than testing presence: other machinery emits them too, so a
    `contains` check passes on those and measures nothing. That is the
    same class of error as the byte-identity above, one level down.

    **`apply` in a component was NOT reclassified.** It shares the arm,
    and the obvious move is to give it the verdict the thread earned.
    But v1's handling of `apply` varies by position and by whether the
    named package is declared — in a test body a declared package is
    rejected while an undeclared name is accepted — so the component
    landing needs its own anchored probe. It keeps `Unsupported` and the
    site says why. **Splitting an arm is also permission to leave half
    of it alone.**

    **Review then found the reclassification too broad, in the direction
    that costs the most.** `transactor_is_component` routes a `bound to
    <bus>` transactor down this same component path when it also has a
    non-periodic `on` handler — so the arm catches a `thread` sitting on
    exactly the construct that serves it. There v1 emits the target
    actor normally (6 `ThreadSlot`s against the control's 4;
    `emit_bound_tlm_target_actors` is not gated on `on` handlers), so
    the shipped `SilentlyMisLowers` was false for that input, the hint
    told the user to move the thread somewhere it already was, and
    `not_implemented` suppressed the `--codegen v1` pointer that had
    been correct. A regression, from a probe that only ever put a
    `thread` on an env.

    No fixture in the corpus has that shape, which is why it went
    unprobed: the arm is reachable from a construct combination the
    whole test suite never writes. Building it took adding a
    `handshake_channel` to a bus that had only a `tlm_method`.

    Two smaller findings from the same review, both about *pointing*
    rather than classifying: the `apply` detail said "aspects apply at
    test scope", but test scope rejects `apply` too — a hint that names
    a destination which also fails is worse than the empty detail it
    replaced. And the negative anchor declared `env WrapEnv` without
    instantiating it; v1 registers scheduler slots at the `let` site, so
    the `ThreadSlot` count equality held trivially. **The test written
    to avoid measuring nothing was measuring nothing.**

52. **`components.rs` families C and B: one lands, one is reverted by a
    test written three batches earlier (2026-08-17).**

    **Family C (handlers) — three arms, two behaviours.** A `pre`/`post`
    hook on a cycle-trigger or periodic `on` handler is
    `SilentlyMisLowers`: v1 emits the handler with the hook side
    DISCARDED, byte-identical to the same handler written without it, so
    the requested ordering is silently ignored. A non-default PHASE is
    NOT the same — v1 implements it, emitting
    `_post_eval_services.push_back` where the default emits
    `_checkers.push_back`, so that arm keeps `Unsupported`.

    The two sit four lines apart and were first probed with a shared
    control that changed the trigger kind AND the modifier at once. Both
    then read as "differs", and the split was invisible. Against a
    one-token control they separate immediately. Rule 3, and the cost of
    breaking it is not a wrong answer but a *missing* one.

    **Family B (connect endpoints) — probed, reclassified, reverted.**
    Three arms; the probe said non-path endpoints are `Rejects` (v1
    refuses with its own message) and an unresolvable path segment is
    `EmitsUncompilable` (v1 prints `env.source.nope.observed.push_back`
    verbatim). Both readings were correct for the fixture used, and both
    are wrong as classifications.

    `cargo test` failed on
    `a_malformed_connect_endpoint_keeps_its_v1_suggestion`, written
    three batches earlier, whose doc comment already records why: **what
    v1 does with a bad edge depends on WHERE THE EDGE SITS.** In an
    instantiated env it reaches its endpoint check and refuses; in an
    UNINSTANTIATED env it emits no wiring at all and simply succeeds —
    and tbir resolves `connect` for every env in the merged file, so it
    sees edges v1 never reaches. Re-probed to confirm rather than taken
    on the test's word: an uninstantiated env accepts the non-path
    endpoint under v1. One site, three outcomes; no single `V1Status` is
    honest, so the suggestion stays.

    This is the second time the `connect` sites have been reclassified
    and reverted — divergence 40 reverted eight of them for the same
    reason. Both times the probe was correct about the fixture in front
    of it. **Position-dependence is invisible to a single-fixture probe
    by construction**, and the only defence that has actually worked is
    the regression suite: grep it for the site's own message before
    touching an arm, because a previous batch may already have paid for
    the answer.

    Family C was re-run in the uninstantiated position too, and that
    re-run **proved nothing** — a mistake worth recording, because it
    was made while writing up the family-B lesson about positions. With
    `Ticker` uninstantiated, v1 emits nothing for the agent at all:
    hook-vs-control is byte-identical, but so is handler-vs-no-handler.
    The second position cannot distinguish "the hook was dropped" from
    "the handler was inert" — the exact anchorless reading divergence 51
    records — so citing it as corroboration was divergence 51 repeated
    one entry later. The verdict stands on the instantiated probe, which
    carries a real anchor; the second position is silent, not agreeing.
    **A control that is vacuously equal agrees with every hypothesis.**

53. **The cycle-trigger hook arm has a second input, and v1 fails it
    differently (2026-08-17).**

    The arm above was probed only with a stray modifier on a genuine
    cycle trigger (`on beats > 0 pre`). It also catches a **spec §7.3
    method hook written in a component body** (`on s.send pre`) — not an
    event subscription, not a handshake monitor, so it falls through to
    the same place. For that input v1 does not silently drop the hook and
    stop: it drops the hook and then lowers `s.send` as a cycle trigger,
    emitting `(bool)(e.s.send)` against a `struct Sender` whose only
    members are `dut`, `_last_in_cycle` and `_last_out_cycle`. That does
    not compile.

    Two consequences, and only the second changed any code. The verdict
    is unchanged: `SilentlyMisLowers` is the worse of the two outcomes
    and an arm's status is the worst thing v1 does anywhere under it. But
    the DETAIL claimed byte-identity, which is true of one input and
    false of the other, so it was reworded to describe what v1 actually
    does to both — drop the hook and lower the trigger as a plain cycle
    trigger.

    A first attempt at that rewording also added "a §7.3 method hook
    belongs at test scope, where it is lowered". It was removed on
    review. The claim is true for a hook on a DIRECT transactor testbench
    field (`axilite_hooks_test`, in the equivalence registry), and false
    for the nested target a component body implies: `on e.s.send pre` at
    test scope is refused by `resolve_method_hook_target`. Being right
    about the destination in general is not the same as being right about
    where THIS arm's user would land, and **a hint naming a destination
    that also fails is worse than no hint** — which is divergence 51's
    lesson, reintroduced two entries after writing it down.

54. **The `on <obj>.<method> pre/post` hook spans nine sites across
    four files, and four verdicts (2026-08-17).**

    Divergences 52 and 53 treated "the hook family" as the arms inside
    `components.rs`. It is not a file-local construct. A user writes the
    same source in four different places, and grouping by construct
    rather than by file turns up NINE existing sites across four files —
    three in `components.rs` and six that were invisible from inside it.
    Every one carried at least one wrong verdict, and four were
    mixed-verdict and had to be split, leaving fifteen arms:

    | site (→ arms after splitting) | v1 | was | now |
    |---|---|---|---|
    | component body cycle-trigger hook (`components.rs`) | drops the hook | `Unsupported` | `SilentlyMisLowers` |
    | component body periodic hook (`components.rs`) | drops the hook | `Unsupported` | `SilentlyMisLowers` |
    | component body event-subscription hook (`components.rs`) | drops the hook | `Unsupported` | `SilentlyMisLowers` |
    | `testbench` declaration hook (`mod.rs`) | drops the hook | `Unsupported` | `SilentlyMisLowers` |
    | test-scope `phase post_eval` pre-check (`mod.rs`) | rejects | `Unsupported` | `Rejects` |
    | statement position (`stmts.rs`) → `phase post_eval` | rejects | `Unsupported` | `Rejects` |
    | …→ non-path trigger | rejects | `Unsupported` | `Rejects` |
    | …→ method path that RESOLVES to a hookable | **implements it** | `Unsupported` | `Unsupported` (kept) |
    | …→ method path that does not resolve | rejects | `Unsupported` | `Rejects` (entry 69) |
    | test-scope target resolution (`mod.rs`) → non-path trigger | rejects | `Unsupported` | `Rejects` |
    | …→ nested component path | **implements it** | `Unsupported` | `Unsupported` (kept) |
    | test-scope non-transactor field (`mod.rs`) → hookable found | **implements it** | `Invalid` | `Unsupported` |
    | …→ no hookable | rejects | `Invalid` | `Invalid` (kept) |
    | …→ bare field, no method | rejects | `Invalid` | `Invalid` (message fixed) |
    | scoreboard body (`scoreboards.rs`) → hooked `on` | drops the hook | `Unsupported` | `SilentlyMisLowers` |
    | …→ unhooked `on` | drops it in a transactor container | `Unsupported` | `SilentlyMisLowers` (entry 70) |
    | …→ `connect` | drops it in a transactor container | `Unsupported` | `SilentlyMisLowers` (entry 70) |

    The "now" column is kept CURRENT, not frozen at the batch that
    wrote the table — three rows above carry a later entry number
    because a later batch re-measured them. A ledger whose only job is
    to be the one consolidated view is worth less than nothing when it
    disagrees with the code.

    Two of these are worth reading closely.

    The **statement-position** arm was classified correctly — v1 emits
    the same `Sender_send_pre.push_back` registration a test-scope hook
    gets — but its detail said "declare the hook on the component or
    testbench instead", and BOTH of those placements are themselves
    rejected, by the two rows above it. The suggestion sent the user in a
    circle around the same table. It now names the test / `impl ... for`
    body against a direct transactor field, the one placement that
    lowers. A classification can be right while the sentence attached to
    it is the most expensive thing on the page.

    The **non-transactor field** arm answered `Invalid` — "a program
    error under every backend" — for `on w.note pre` where `w` is an
    agent, env or method-bearing scoreboard field. v1 emits a working
    `Watcher_note_pre.push_back`. That is not a broken program; it is a
    TB-IR subset gap wearing a hard error, and `Invalid` gives the user
    nothing to re-run. The site now applies v1's own condition (the field
    binds to a component type declaring a `hookable` of that name) and
    keeps `Invalid` only for the half v1 also refuses: an undeclared
    field, the DUT handle, a `function` rather than `hookable` method, or
    a transactor missing the method. Each of those four was probed and v1
    refuses each — with the SAME message in all four cases ("obj.method
    must resolve to a `hookable` on a known component type"), not four
    distinct ones. That is enough to keep them `Invalid` but not enough
    to claim v1 diagnoses them individually.

    Two more sites turned up on the next pass, both mixed-verdict, and
    both were split on the same predicate rather than given one label.
    v1 routes EVERY hooked `on` through its method-hook resolver, which
    accepts an `<obj>.<method>` path and refuses anything else outright.
    So at both the statement position and the test-scope target
    resolution, a path trigger keeps `Unsupported` (v1 wires it) and a
    non-path trigger — `on <bool-expr> pre`, `on <N> cycles pre`,
    `on ev(x) pre` — becomes `NotImplemented { Rejects }`.

    The first predicate tried was `dotted_path`, and it LEAKED TWICE. It
    returns `Some` for a bare identifier and it unwraps `Paren`, so
    `on ok pre` and `on (s.send) pre` both slid into the `--codegen v1`
    branch for programs v1 refuses — and the paren case is one character
    away from a program that works, which is the worst kind of wrong
    suggestion to hand someone. v1 matches `ExprKind::Field` directly, so
    `is_v1_method_hook_shape` now checks the top-level shape directly
    too. **Reaching for the nearest existing helper is not the same as
    writing the predicate you meant**; `dotted_path` exists to parse
    `connect` endpoints, where a parenthesised path is fine.

    A `phase post_eval` modifier flips the verdict without touching the
    trigger, but only where v1 routes the handler through its method-hook
    resolver — the statement position and test scope, where it refuses
    the modifier by name. At the component-body and testbench-declaration
    positions v1 still emits, with hook AND phase dropped, so those keep
    `SilentlyMisLowers`. (An earlier draft of this entry said v1 "refuses
    it by name at every position that carries a hook", which is the same
    over-generalisation from two positions to all of them that the entry
    above it is about.) The modifier gets its own message rather than
    being blamed on the path shape, and it turned up a fifth site, the
    test-scope `phase post_eval` pre-check, which was `Unsupported` for
    an input v1 names in its own refusal.

    A ninth site sat in a fourth file: `scoreboards.rs` answered every
    `connect`/`on` in a scoreboard body with one `Unsupported`. Its
    HOOKED half is uniform and was reclassified — v1 drops the hook
    byte-identically at both trigger shapes, anchored.

    Its UNHOOKED half was split too, and the split was **reverted**,
    which is the most useful thing in this entry. The arm is genuinely
    mixed: `on w.note` makes v1 emit `(bool)(w.note)` against a
    `struct Watcher` with no `note` member, while `on hits > 0` makes it
    emit `(bool)(_tb.b.hits > 0)`, which compiles. Reading that as
    "method path bad, expression good" and reusing
    `is_v1_method_hook_shape` was wrong in both directions at once:
    `on dut.en` is a two-segment path that COMPILES
    (`harc_read(dut->en)`), and `on w.seen > 0` is an expression that
    does NOT (no `w` in the checker lambda's scope). What separates the
    inputs is name resolution in the emitted C++, not the syntax of the
    trigger. The split also leaked `on (w.note)`, `on w.note cycles` and
    `on w.note phase post_eval` — the predicate's `Paren`, periodic and
    phase strictness are right for the hook resolver's question and
    meaningless for this one.

    **Borrowing a predicate borrows its question.** That is the same
    mistake as the `dotted_path` one above, made one commit later, on
    the predicate written to fix it. Classifying this arm needs the
    scope analysis; the site now says so instead of guessing, and
    `an_unhooked_scoreboard_handler_is_one_verdict_for_its_whole_input_space`
    pins the revert. The `connect` half of the same arm is untouched and
    was never probed at all.

    That test was first justified as "without it, re-applying the split
    leaves the suite green", and that was **wrong** — the sibling hooked
    test's own `on w.note` control already fails when the split comes
    back. The test still earns its place, because it pins `dut.en`,
    `(w.note)`, `w.note cycles` and `w.note phase post_eval`, which the
    sibling does not, and because it asserts the two rows the split got
    backwards (`on dut.en` compiles, `on w.seen > 0` does not) rather
    than just a verdict. But the justification was written from one
    run of one test rather than from removing the test and running the
    suite — **checking that a new test fails is not the same as checking
    that nothing else already did.**

    A tenth candidate in `transactors.rs` (a hooked `on` on a `bound to`
    transactor) is NOT classified here. Every well-formed bound
    transactor routes its handlers through the `components.rs` arms
    above — verified against `axilite_bound_mon_test`, where hook and
    control differ only in source-offset-derived symbol names
    (`_solver_site_2233` vs `_2237`), so the hook is dropped there too.
    The `transactors.rs` arm was only reachable in probes whose
    instantiation v1 rejects for an unrelated reason, which measures
    nothing. Recorded as unprobed rather than classified on that.

    The residual is bounded and stated: a path that is well-formed but
    does not resolve to a `hookable` still gets the suggestion, and the
    user lands on v1's own message rather than on silence. That covers
    more inputs than it may read as — `e.inner.plain` (a `function`, not
    a `hookable`), `e.nosuch.note`, `s.send.x`, `dut.en.x` are all
    well-formed paths that v1 refuses, and `e.inner.note` is the only
    nested path in that set v1 actually wires. Closing the residual means
    resolving the path against the component tree, which is the same
    scope analysis the scoreboard arm below is waiting on.

    The lesson is about SCOPE, not about hooks. Batch 20's plan grouped
    `components.rs` by "what a user would have to write to reach the
    site" — and then applied that grouping only inside one file. The
    construct does not respect the file boundary: it has at least NINE
    sites across four files, and six of them were invisible from inside
    `components.rs`. **Group by construct, then find every file that
    implements it.** Note also that this entry's own table was written
    saying "four positions" and had to be corrected twice as further
    positions appeared, a third time when the phase modifier turned up
    two more, and a fourth when `scoreboards.rs` turned up — a count of
    sites is a claim like any other, and it needs a search, not a
    recollection. It now says "at least nine". Six review rounds on one
    construct, each finding real leaks the previous round's probe had
    not sampled, is the honest cost of a WIDE surface: the input space,
    not the arm, is the unit of work.

    The predicate itself took three attempts, and the failure mode was
    the same each time — reaching for an existing helper instead of
    writing the question. `dotted_path` accepts a bare identifier and
    unwraps `Paren` at every level, because `connect` endpoints allow
    both; guarding only the top-level node moved the leak one segment
    inward (`on (s).send pre`). `is_v1_method_hook_shape` now does its
    own walk with no `Paren` arm, matching v1's own pattern match. It
    also carries no `!h.periodic` clause: `on s.send cycles pre` is a
    path with a period, v1's hook branch ignores `h.periodic` and wires
    it, and the clause made the statement position disagree with v1 AND
    with the sibling arm that shares the predicate. **A conjunct that is
    redundant on the inputs you sampled is not free — it is an untested
    claim.**

    A fourth leak closed the same way: the impl-for desugarer rewrites a
    bare testbench field to `_tb.<field>`, so a plain length test counts
    the synthetic root as a real segment and read `on s pre` — one
    identifier, no method — as `<obj>.<method>`. The walk now mirrors
    `resolve_method_hook_target`'s two accepted forms exactly
    (`<field>.<method>`, `_tb.<field>.<method>`). The same synthetic root
    was leaking into a user-facing message, quoting back a `_tb` nobody
    typed; that shape now gets its own sentence.

55. **Named arguments are decoration to v1, and `#(...)` parameters do
    not exist to it (2026-08-17).**

    Two `components.rs` families that share a shape: the user writes
    something to make their intent explicit, and v1 throws that
    something away.

    **Named arguments — and TB-IR was doing it too.** The named-argument
    construct turned out to have the same shape as the parameter one:
    arms that REPORT it, and sites that silently DO it. `bus.rs` and
    `regblock.rs` each carried a private `call_arg` helper that took
    `value` and dropped `name`, so TB-IR itself bound reordered named
    arguments by position — `bus.w.send(strb = 15, data = t.value)`
    emitted `axil_w_data = 15` and `axil_w_strb = t.value`, swapped, with
    no diagnostic from either backend.

    That is the same class as the seventh parameter landing and was found
    the same way: by looking for the BEHAVIOUR outside the file the first
    fix was written in.

    The guard took two tries. The first keyed on ARITY alone — reject any
    multi-argument call carrying a name — and so refused
    `bus.w.send(data = t.value, strb = 15)`, names in declaration order,
    which both backends lower correctly, while telling the user v1
    "silently emits something else". That sentence was false for the
    program in front of it. **Refusing a correct program with a false
    explanation is not the safe side of a classification**, and the
    excuse for the shortcut did not even apply: unlike
    `lower_component_call_args`, which sees only `&[CallArg]`, the three
    bus callers have the declaration in hand — from the channel payload
    or `m.args`. The guard now compares each name against the parameter
    at its position and says which parameter was written where.

    `record_write` is the exception that proves the rule: it is a builtin
    with no declaration node, so its list was written from memory as
    `["reg", "value"]`. The real signature is `(addr, data)` — the
    compiler's own `Invalid` message three lines above the guard says so,
    and so do the docs and every fixture. The consequence was the very
    thing the rewrite was for: `record_write(addr = .., data = ..)`, the
    documented form, refused as a silent mis-lowering. Worse, `reg` is a
    lexer keyword and does not parse as an argument name at all, so
    position 1 could never match and the site degenerated into "refuse
    every named first argument" — the arity behaviour, reintroduced by
    accident behind a check that looked precise.

    Three of the four guard CALL SITES could be deleted with the suite
    still green, which is how that shipped. **A test that pins a
    predicate does not pin the places it is called from.** Each site now
    has its own assertion, and each was verified by deleting the call.

    An unknown parameter name is now `Invalid` rather than
    `SilentlyMisLowers`: no backend can honour a name that matches no
    parameter, and for a typo sitting in a valid position v1 emits
    exactly the right code — so claiming a silent mis-lowering there was
    the same false explanation one layer down.

    Sites that remain UNFIXED, recorded rather than guessed at. The
    first is the most serious thing left in this construct:

    * **`log`/`logf` downgrade the SEVERITY.** `log(level = fatal,
      "BOOM")` emits `sim_log_line("INFO", "BOOM")` under BOTH backends —
      measured. Not "swallows the message": the message survives and the
      severity does not, so there is no `ctx.errors++` and no `_fatal`,
      and a test that should abort passes green. This is a live silent
      mis-lowering in the DEFAULT backend and should be first in the next
      batch.
    * Relation calls in `randomize ... with` (`typed_lower.rs`) drop the
      name and bind by position.
    * `record_read` and the other one-argument regblock builtins accept
      an unknown name silently, which is now inconsistent with the
      guarded one-argument `fork` site that refuses it.
    * Six arms still answer `Unsupported` for calls v1 swaps — helper,
      extern-fn, testbench-method, two transactor-method sites and the
      covergroup helper target.

    One classification is also left open rather than settled. The guard
    reports an unknown parameter name as `Invalid`, and a review pass
    showed that is not quite right in either direction: for a typo in a
    VALID position v1 emits byte-identical correct code, so `Invalid`
    over-claims and `--codegen v1` does in fact work; while
    `record_write(nosuch = 0x18, addr = 305419896)` really is swapped by
    v1, and returning on the first bad argument hides it. Settling it
    means scanning every argument and reporting the worst rather than the
    first, which is a change with its own input space and belongs to its
    own batch.

    No CODEGEN site in v1 reads an argument name:
    of the 30 `CallArg::Named` matches in `cpp_tb.rs`, 25 are
    `{ value, .. }` and one is `{ value: e, .. }` — all 26 drop the name
    — and the 4 that bind `name` are AST-rewrite passes that reconstruct
    the node and pass it along. (The first draft said "all 30 destructure
    `{ value, .. }`", the second "26 destructure `{ value, .. }`". The
    conclusion survived both corrections; the sentence was a count that
    had not been counted, twice.) Binding is by position everywhere, and that was
    measured, not only read:
    `axil_write(data = t.value, addr = t.addr)` emits
    `AxilXactor_axil_write(_tb.env.drv, t.value, t.addr)` — the two
    arguments SWAPPED, in code that compiles and runs. A name matching
    no parameter draws no diagnostic at all. Reordering is the entire
    point of naming arguments, so this is `SilentlyMisLowers` and v1 is
    the last place to send the user.

    Split on the ARGUMENT count, because a single argument cannot be
    reordered — there is no other position for it to land in, so v1
    emits exactly the positional call. There v1 really is an escape
    hatch, and the same reasoning keeps the two one-argument predicates
    (`idle_in`/`idle_out`, `quiesced`) on `Unsupported` rather than
    sweeping them up with the general call. Arity ≥ 2 is not split
    further: same-order names emit correctly too, but telling that apart
    needs the callee's parameter list, which the seam does not have, and
    an arm's status is the worst thing under it.

    The first wording of that arm said "with a single PARAMETER that is
    the same thing", which the seam cannot know and does not check. A
    one-argument call to a two-parameter method emits an uncompilable
    call under v1 — but so does the positional `axil_write(t.value)`,
    which tbir lowers and verifies clean today. The arity gap is real
    and pre-existing; naming the argument did not cause it, and the
    message no longer implies the call is well-formed. **Splitting on a
    quantity you have is not the same as splitting on the quantity your
    sentence is about.**

    **`#(...)` parameters on a component.** v1 drops the list, and three
    things follow. Declared but unused, v1's output is BYTE-IDENTICAL to
    the same component written without the parameter, and a `#(4)`
    argument at the instantiation vanishes with it — nothing is
    mis-lowered, the knob simply did nothing. Referenced with no name to
    fall back on, `limit : uint<32> default N` emits `uint64_t limit = N;`
    with `N` declared nowhere, which does not compile. Referenced from a
    HANDLER BODY while a file-scope `const N = 9` exists, the reference
    resolves to the const: that file compiles, and the component runs
    with 9 instead of the 4 that was passed.

    Only the third case earns `SilentlyMisLowers`, and it took two tries
    to record one that does. The first version asserted the label off the
    first two cases, where the honest reading is `EmitsUncompilable` for
    one and "correct" for the other. The second version added a
    shadowing case — the FIELD DEFAULT one — and asserted it compiles on
    two `contains` checks that never looked at order. It does not: v1
    emits the const at namespace scope AFTER the component struct, so
    `uint64_t limit = N;` inside the struct is still a
    use-before-declaration. Spliced into g++ with the generated file's
    own header set, against a control that moves only the const, it fails
    with `'N' was not declared in this scope`; the handler-body use,
    emitted a hundred lines later, compiles clean.

    Twice in a row the LABEL was right and the evidence recorded for it
    was not, which is a correct answer with a wrong proof and reads
    exactly like a correct one. **"It compiles" is a claim that requires
    a compiler.** Two `contains` checks on the same file establish that
    two strings exist, never that one may refer to the other — and this
    entry already carried a rule about reading the generated C++, which
    is not the same as reading two lines out of it.

    A fifth landing, `records.rs`'s `parameters on transaction`, agrees
    too, and its silent position is the worst of the set. A `keep`
    constraint referencing the parameter does not emit the name at all:
    the constraint IR CONST-FOLDS it against a same-named file-scope
    `const`, so v1 emits
    `_s.add(z3::ult(_z_tag, _ctx.bv_val((uint64_t)5, 64)))` and records
    `(tag:u8 < 5:u8)` in the runtime problem table. The randomizer runs
    against the const's bound with no `N` left anywhere to notice. Strip
    the FAIL log line, which echoes source text verbatim, and
    `keep tag < N` and `keep tag < 5` are the same program.

    This arm was left UNCLASSIFIED for one commit on the grounds that
    three probed positions — field default, `range(0, N)`, and the
    `keep` — all emitted ahead of the consts or, for the `keep`, "only
    into a log string". The log line is real. It also sits thirty lines
    BELOW the solver line that does the folding, and reading down to it
    meant reading past the answer. The caution attached to that note
    ("evidence about three positions, not about the space") was right,
    and the position it was worried about was one already looked at.

    Both anchors were needed here and both were nearly skipped. Byte
    identity means nothing unless the component contributes at all, and
    unless something the component carries is VISIBLE in the output — so
    the probe pins `uint64_t limit = 7;` in the control before trusting
    any identity. This is divergence 51's lesson applied before being
    caught by it rather than after.

    Probed at all SEVEN landings rather than one, because divergence
    47's tseq pair sat four lines apart and classified differently. They
    all agree on `SilentlyMisLowers` — a result, not a reason to have
    assumed it, and it took two wrong labels and six review rounds to
    establish, with the set of landings growing at four of them (2 → 4 →
    5 → 6 → 7).

    The sixth, `mod.rs`'s `test parameters`, is the only one whose
    surface syntax is paren params (`test T(N: int = 3)`) rather than
    `#(...)`, which is why searching for the `#(...)` spelling missed it
    five rounds running. **Grouping by construct means grouping by what
    the construct DOES, not by how it is spelled.** The search that
    finally found the last two was over the thirteen `params: Vec<Param>`
    fields in `ast.rs` — the declaration sites — rather than over any
    spelling.

    The seventh is different in kind and is the most serious thing in
    this entry. `testbench Tb #(N: int = 3)` was not misclassified; it
    was **not rejected at all**. `ComponentDecl` carries a `Testbench`
    kind, and `comp_sources` admits `Item::Env` only when the kind is
    `Env`, so a testbench reaches no parameter check anywhere. With a
    file-scope `const N = 9` to shadow, TB-IR lowered it, VERIFIED it and
    emitted `harc_assign(dut->rst, ((int64_t)(9)))` — byte-identical to
    the same source with the parameter list deleted. That is precisely
    what `SilentlyMisLowers` is documented as ("the worst outcome, and
    the reason TB-IR refuses rather than matching it"), and TB-IR was
    matching it.

    Only half the shape leaked: with nothing to shadow, the
    unresolved-name path already caught it. A probe that used a fresh
    parameter name would have reported the hole closed. **The shadowing
    case is not an exotic corner of this construct — it is the only case
    that distinguishes a dropped parameter from a rejected one**, and six
    of the seven landings needed it to classify at all.

    The fourth landing, `scoreboards.rs`, was first labelled
    `EmitsUncompilable` on a structural argument: a data-only scoreboard
    has only fields, so its only way to name a parameter is a field
    default or width, both emitted inside the struct ahead of every
    `const`, so no silent case is reachable. **The argument is wrong at
    its first step.** `scoreboard_is_component` routes a scoreboard to
    the composite table on `Hookable` ALONE, so one carrying fields plus
    an `on` handler stays data-only and reaches the arm — and the
    parameter check runs before the `on` rejection further down. v1 emits
    that handler's trigger into a checker lambda ~110 lines after the
    const, so `on hits > N` becomes `(bool)(_tb.b.hits > N)` resolving to
    a file-scope `const N = 5`: it compiles, and the scoreboard runs with
    5. `#(7)` and `#(8)` emit byte-identically, so the argument is
    provably invisible.

    "Its only items are fields" was read off the ARMS in `scoreboards.rs`,
    which reject methods and `on` handlers, without checking the GATE
    that decides which file gets the declaration in the first place. A
    structural argument is only worth more than a count if it is checked
    as carefully as one, and this one was checked one level too shallow —
    two entries after that rule was written down.

    The first version of this entry also claimed the two `components.rs`
    arms "sit four lines apart". They are 41 lines apart in the base and
    82 after this change — a detail invented to decorate a point that
    stood without it.

56. **`log` downgrades a named severity to `info` (2026-08-17).**

    `lower_log` finds the severity as "the first bare ident among the
    args" and the message as "the first string literal", and both
    searches match `CallArg::Expr` only. v1's do the same. So a NAMED
    argument hides whatever it wraps, under both backends:

    * `log(level = fatal, "BOOM")` → `sim_log_line("INFO", "BOOM")`
    * `log(fatal, msg = "BOOM")`   → `sim_log_line("FATAL", "")`

    The first is why this led the batch. A `fatal` silently becomes an
    `info`: nothing bumps `ctx.errors++`, nothing sets `_fatal`, and a
    test that should abort passes green. Four lines below the extractor,
    the severity guard rejects a TYPO (`log(errror, ...)`) with the
    comment *"rejecting it is what makes `log(error, ...)` trustworthy"*
    — and a named severity walked straight past that guard into exactly
    the outcome it exists to prevent.

    Gated on what the name hides AND on the slot being empty, which took
    three tries. The extractors take positional matches only, so a named
    argument costs the user something exactly when a slot it could have
    filled is left unfilled — `log` needs one string, `logf` needs two
    (path then message), and a severity slot is filled by any positional
    bare ident.

    Each earlier version refused correct programs:

    * keyed on named-ness alone → refused `log(fatal, "BOOM", extra = 1)`;
    * keyed on the named VALUE → refused `log(fatal, "BOOM", lvl = warn)`
      and `logf(p = "a.log", "t.log", error, "BOOM")`, where a positional
      candidate wins under both backends;
    * modelled the message slot but not `logf`'s PATH slot → let
      `logf(path = "t.log", error, "BOOM")` through, which writes to a
      file literally called `BOOM`. That one was a REGRESSION: the
      cruder version had caught it.

    A fourth edge: a named ident is only a hidden severity if it names a
    real one. `log("BOOM", who = nosuch)` hides nothing, because
    positionally `nosuch` would have been rejected by the severity guard
    rather than silently used — so `is_log_severity` is now shared
    between the gate and that guard, and the two answer "is this a
    severity" the same way.

    Every sub-condition is pinned by mutation: removing the guard,
    dropping either slot-filled check, collapsing `logf`'s two-string
    requirement to one, and dropping the severity-name check each fail
    the test. **Widening a gate is as much a claim as narrowing one, and
    needs its own case.**

57. **A control that could not tell "did not fire" from "fired and was
    thrown away" (2026-08-17).**

    Relation calls in `randomize ... with` bind by position and drop the
    name, so `Between(r, hi = 131072, lo = 65536)` inlines
    `addr >= 131072 && addr <= 65536` — unsatisfiable, silently, from a
    program written to mean the opposite.

    A check was written in `expand_top_level_relation_call`
    (`constraints/typed_lower.rs`), and the swap still lowered. The
    control run to explain that: the EXISTING arity check beside it,
    given `Between(r, 65536)` on a three-parameter relation. That
    program lowered clean too, and the conclusion drawn was "the
    function is not on this path". **The conclusion was wrong and the
    control is why.** It measured whether the PROGRAM lowers, which
    cannot distinguish a check that never ran from a check that ran and
    whose error was discarded.

    Inspecting the table directly instead of the program shows the
    truth: `build_typed_solver_problem_table` really does emit
    `LowerError([RelationArityMismatch { expected: 3, found: 2 }])`. The
    check fires. The loop in `mod.rs` that consumes the table then reads
    only `TypedSolverProblemBuild::Z3` entries and `continue`s past
    everything else, so every constraint-lowering error is dropped on the
    floor before it can reach a user.

    So the reverted guard was in the right place after all, and the
    prescription that went with it — "thread a relations table into
    `LowerCtx`" — was also wrong, since `lower_program` already builds
    that table. **Ask the instrument, not the outcome**: one call to the
    table builder answered in a line what a program-level control could
    not answer at all.

    The real gap is the discarded `LowerError` entries, and it is NOT
    closed here. Surfacing them would start reporting every constraint
    error currently swallowed — arity mismatches, unknown relations,
    recursive expansions — against a corpus that has never had them
    reported. That is a blast radius that needs its own probe over the
    fixture set, and it is the next batch's first item.

58. **`logf` finds its path by VALUE, not position (2026-08-17).**

    `lower_log` skips the logf path by comparing each string literal
    against the path string, so a message that happens to equal the path
    is skipped as though it were the path. `logf("t.log", "t.log", error,
    "BOOM")` gives TB-IR the message `"BOOM"` where v1 emits `"t.log"`,
    and `logf("t.log", error, "t.log")` gives TB-IR `""` where v1 emits
    `"t.log"`. Both backends accept both programs, so this is a live
    silent DIVERGENCE rather than a shared mis-lowering — the only one
    found in this batch. v1 takes the path positionally; matching that is
    the fix. **Closed in divergence 64.**

59. **Every constraint diagnostic was thrown away, and v1 crashes on
    one of them (2026-08-17).**

    `build_typed_solver_problem_table` records the constraint lowerer's
    errors as `TypedSolverProblemBuild::LowerError` entries.
    `lower_program`'s consuming loop read only `Z3` entries and
    `continue`d past the rest, so **none of them ever reached a user**.
    `randomize(r) with NoSuchRelation(r)` lowered clean under TB-IR while
    v1 refused it outright.

    Only the RELATION errors now surface, and the split was MEASURED, not
    reasoned — though the first measurement was over the wrong set. It
    swept the 173 entries of `tbir_equiv_fixtures.txt` and reported
    "exactly one" non-relation `LowerError`. Sweeping every `.harc` in
    `tests/fixtures` instead — 190 files, 184 of which merge — gives the
    real numbers: **two** fixtures produce non-relation errors
    (`uint64_unique_randomize_test`, whose `s.sample[63:32] != 0` trips
    `DisallowedInConstraint`, and `axi_agent`, with `UnresolvedIdent`),
    and **zero** produce a relation error. The second number is the one
    that matters and it is stronger than what was claimed: surfacing the
    relation variants breaks nothing in the corpus at all. *Measuring the
    registry is not measuring the corpus* — the registry is what runs
    under equivalence, not what exists.

    Only the first of those two actually supports the discard decision:
    `uint64_unique_randomize_test` is lowered by BOTH backends and passes
    trace equivalence, so surfacing `DisallowedInConstraint` would reject
    a working fixture. `axi_agent` is rejected by both backends anyway,
    for an unrelated covergroup reason — an earlier draft of this entry
    said "v1 lowers both", which was false and would have counted a
    rejected fixture as evidence. Three of the four that do surface are program errors under
    any backend — a relation that does not exist, one called with the
    wrong arity, and one that expands into itself — so they are `Invalid`
    and name no escape hatch.

    The FOURTH is not. A misplaced argument name is accepted by v1, which
    emits working C++ with the values swapped, so `Invalid`'s "program
    error under every backend" is literally false for it. It carries
    `SilentlyMisLowers` instead — the sweep's ordinary shape, and the
    verdict that keeps the diagnostic from naming v1 as a way out. Three
    siblings sharing one code path is not a reason to share one verdict.

    v1's behaviour, measured for each: it rejects the first two
    ("constraint function call not supported in v0 solver path") and on
    the third it **STACK-OVERFLOWED and aborted the process**.
    `expand_relation_subtree` in `cpp_tb.rs` had no depth guard, so
    `relation R(r) = R(r)` took the compiler down. No `V1Status` fits a
    SIGABRT, which is part of why `Invalid` is the right verdict — and
    the regression test asserted on TB-IR only and never called
    `cpp_tb::emit` on that input, because a test cannot catch an abort.
    **Closed in divergence 62**, which is why that test now does call
    it.

    With the diagnostics surfacing, the misplaced-named-argument check
    from divergence 57 finally does something. Relation calls bind by
    position and drop the name, so `Between(r, hi = 131072, lo = 65536)`
    inlined `addr >= 131072 && addr <= 65536` — unsatisfiable, silently.
    The check was written a batch earlier and reverted as "does not
    fire". **It fired. Its error was discarded.** The control that hid
    that asked whether the program lowers, which cannot tell the two
    apart; one call to the table builder can.

    Both halves of the split are pinned by mutation: widening the arm to
    surface every variant fails the capability-gap fixture, and neutering
    the name comparison fails the swap test.

    Three limits are known and NOT closed here, recorded so the next
    batch takes them deliberately:

    * **Only Test and Tseq randomize sites are collected.** Closed in
      divergence 60 below.
    * **`MAX_ERRORS = 5` can disable the refusal.** Closed in
      divergence 61 below.
    * **Every Ident-callee constraint call is treated as a relation
      call.** Closed in divergence 63 below.

60. **A constraint written in a component body reached C++ unchecked
    (2026-08-17).**

    Divergence 59 recorded this as a known limit; this closes it.
    `collect_randomize_sites` walks `Item::Test` and `Item::Tseq` and
    nothing else, so the solver problem table had no entry for a
    `randomize ... with` written in an agent's `on` handler, a component
    method, a `testbench` lifecycle phase, a transactor TLM target
    thread or a file-scope `function`. The refusal from divergence 59
    reads that table, so it never saw those sites.

    They are not skipped at EMISSION. Both backends emit them through
    `cpp_tb::emit_randomize_for_site`, which lowers the constraint
    itself. Measured on `component_method_randomize_test` with a
    two-parameter relation added and called from the agent handler,
    `Band(r, hi = 2000, lo = 1000)` emitted

    ```cpp
    _s.add(z3::ugt(_z_value, _ctx.bv_val((uint64_t)2000, 64))
        && z3::ult(_z_value, _ctx.bv_val((uint64_t)1000, 64)));
    ```

    byte-identically under **both** codegens — an unsatisfiable
    constraint, silently. So this was TB-IR mis-lowering, not a v1 gap
    TB-IR happened to inherit, and `SilentlyMisLowers` is the verdict on
    both sides of the split (the `Invalid` relation errors reach these
    sites too).

    The fix is a **separate, validation-only** table
    (`build_component_scope_problem_table`) rather than more arms in
    `collect_randomize_sites`. Entry order in the main table assigns the
    `problem_id`s both backends bake into emitted symbol names, so
    widening it would renumber every site that follows a component-scope
    one and churn emitted output for reasons unrelated to the check. The
    new table is read for its `LowerError`s and never reaches emission.

    Blast radius, measured before the change: of the 190 `.harc` files in
    `tests/fixtures` (184 merge), exactly **one** contains a
    component-scope randomize site, and it builds clean — zero new
    refusals across the corpus.

    Walking the bodies is only half of it, and the first version shipped
    only that half. **A randomize target is resolved by NAME**, so a body
    whose scope is empty contributes nothing however carefully it is
    walked — and a component binds names three ways a `test` body does
    not: a field (`r : RegOp`), a method parameter
    (`hookable go(r: RegOp)`), and an `on` handler's event payload
    (`req : event<RegOp>` + `on req(t)`). Seeding only the
    statement-position `let`s collected **zero** sites for all three,
    including the `on req(t)` shape `transactor_active_test` uses. The
    justifying comment reasoned about `let`s and stopped there; walking
    the right blocks with the wrong scope looks exactly like walking the
    right blocks. *Two more shapes were missed the same way*: a
    `watchdog` body hosts statements, and `event<RegOp>` parses its
    argument as an EXPRESSION (`TypeArg::Expr`), not a type, so reading
    only the `TypeArg::Type` arm resolved nothing.

    Ten body shapes and three target bindings are each pinned by
    mutation in `every_component_body_that_can_host_a_randomize_is_
    walked`; deleting the consuming loop in `lower_program` fails
    `a_component_scope_relation_argument_swap_is_refused`. The shape
    count is deliberately larger than the arm count — the parser maps
    both `hookable` and `function` in any component body to
    `ComponentItem::Hookable`, so three shapes share one arm — and an
    earlier draft claimed "deleting any one arm fails this test", which
    was false for `Item::Scoreboard` and `Item::Sequencer` because no
    shape exercised either. They have shapes now.

    Assembling a scope from the wrong set of declarations turned out to
    be the recurring mistake, and it had two more landings. A
    transactor's two halves were walked as INDEPENDENT scopes, so a
    field declared in the always-present half was invisible inside
    `when active` and those bodies collected nothing —
    `synth_component_from_transactor` concatenates the halves, so the
    field really is in scope there. And a `let` or parameter whose type
    does not resolve to a plain named type failed to UNBIND the name a
    field had seeded, recording the site under the field's transaction:
    `agent A { r : RegOp; hookable go() { let r = 5; randomize(r) ... } }`
    was collected as `RegOp`. Nothing on today's surfaced error set
    reads the target transaction — all four variants come out of
    `expand_top_level_relation_call`, which reads only the relation and
    the call's arguments — so the verdict was right anyway. A wrong
    attribution that happens not to matter is still wrong, and it
    becomes a wrong refusal the moment the surfaced set widens.

    Two smaller corrections, recorded because each is the kind of claim
    this document exists to keep honest. The table is **not** a strict
    complement of the emission table: a `testbench` lifecycle phase
    lands in both, because `desugar_impl_for_test_in_file` folds those
    blocks into the bound test while leaving the component intact
    (measured: one entry in each). And the `!w.disabled` guard on the
    watchdog arm is belt-and-braces, not a live filter — `watchdog
    disabled` takes no body at all, the parser refuses the first
    statement, so the body is empty there either way.

61. **A diagnostics-volume guard was load-bearing for correctness
    (2026-08-17).**

    Divergence 59 recorded this as a known limit; this closes it.
    `MAX_ERRORS = 5` bounds how many constraint-lowering errors are
    collected, and `at_error_cap()` was doubling as the stop condition
    for the whole clause walk. So five clauses tripping an error that is
    *deliberately discarded* — `t.addr == t.value` trips
    `WidthMismatch`, a capability gap, not a bad program — filled the
    vector and stopped the walk before a later relation call was ever
    expanded.

    Measured on the exact boundary: four noise clauses refuse, **five
    lower**, and at five both backends emit
    `value > 2000 && value < 1000` from
    `Band(t, hi = 2000, lo = 1000)`. A program was mis-lowered because
    of how many *other*, unrelated, unreported things were wrong with
    it.

    The split is now explicit, and `MAX_ERRORS` is out of the
    control-flow decision entirely. The walk stops exactly when a
    relation error is in hand — that is the only class a caller acts on,
    and only the first one is ever reported. A program with no relation
    error is walked to the end and its diagnostics are capped by
    `record_error` alone, which is what the cap was for.

    The FIRST relation error is stored cap or no cap; dropping it as the
    sixth error would convert a refusal into a mis-lowering just as
    surely. Later ones are dropped rather than also exempted, and that
    distinction is not cosmetic: a first draft exempted *every* relation
    error, which made the vector unbounded. A depth-12 relation
    fan-out over two unknown relations produced **8192** errors where
    the cap had held it to 5. The bound is now `MAX_ERRORS + 1`, and in
    practice 1 — the walk stops at the first one. Removing a bound while
    fixing a bug the bound caused is not a fix.

    The four relation variants live behind
    `LowerError::is_relation_error`, next to the enum.
    `surface_constraint_lower_error` still matches them by hand to word
    each diagnostic, so the list really is written twice — an earlier
    draft of this entry claimed the consumer read the predicate, and it
    did not. What keeps the copies honest is a `debug_assert!` on that
    function's skip arm: adding a fifth variant to the predicate alone
    fails the suite with the variant named. Silent drift would
    reintroduce this very bug — the walk stopping on an error nobody
    acts on, or not stopping on one that matters.

    Both halves are pinned by mutation: restoring `should_stop` to
    `at_error_cap` fails the five-noise-clause case, and dropping the
    exemption for the first relation error fails it too. Blast radius, measured across all
    190 fixtures: the error sets are unchanged and no entry gains a
    relation error, so the longer walk costs nothing and reports
    nothing new.

62. **The compiler died instead of complaining (2026-08-17).**

    Divergence 59 measured this and left it open: `expand_relation_subtree`
    in `cpp_tb.rs` had no guard of any kind, so `relation R(r) = R(r)`
    recursed until the stack ran out. SIGABRT — no message, no exit code
    a build system can interpret, nothing a user can act on. It was left
    open on the reasoning that "a test cannot catch an abort", which is
    true and is exactly why the guard had to come before the test.

    **Three shapes ran away, and each defeated the guard written for the
    one before it. The lesson is where the guard belongs, not how big
    the number is.**

    The first attempt was a work budget, on the argument that every step
    which grows the expression passes through one choke point. Correct,
    and it still aborted: the expander recurses once per level, so
    10 000 levels overflow the stack long before a 10 000-unit budget is
    spent. Depth got its own, smaller, limit.

    The second attempt — depth 64 plus a budget counting EXPANSIONS —
    was measured against `relation R(r: Req) = R(r + r)` and killed the
    process by OOM instead: the argument is substituted into both
    occurrences of `r`, so it doubles every level, and 64 expansions
    build about 2^64 nodes. The budget was re-charged per node PRODUCED.

    The third attempt was measured against
    `relation R(r: Req) = R((((…r…))))` — 60 nested parens, **418 bytes
    of source** — and **still aborted with a stack overflow**. The
    argument does not get bigger, it gets DEEPER: 60x per level, until
    the structural walk runs out of stack. Node count does not see
    depth. That is the very SIGABRT the guard was written to prevent,
    and two rounds of "the guard now covers X" had already been written
    down as closed.

    There is no end to that list, because **bounding the output of an
    unbounded loop is the wrong place to stand.** The fix is a
    relation-NAME stack: a relation already being expanded is expanding
    into itself, and is refused before any tree is built, so it does not
    matter how fast the body would have grown. Every shape above now
    returns a diagnostic in under 4 ms.

    `constraints::typed_lower` already guarded the same recursion this
    way. Three attempts were spent inventing worse versions of a guard
    that existed one module over — each one measured, each one shipped
    as a fix, each one wrong. The reviewer's counter-example, not the
    author's argument, ended each round.

    The budget and depth limit stay as backstops for growth that is
    finite but exponential — a chain of DISTINCT relations, each calling
    the previous one twice. Both numbers are measured: the corpus passes
    at a node budget of **96** and fails at **88**, so 8192 is about 90x
    the deepest real need (wide because the budget is shared across a
    whole constraint list, so it scales with program size); and a
    63-deep chain still expands with its innermost bound reaching the
    emitted C++, while at 64 the call is left unexpanded. That last one
    does refuse a **finite, correct** program, with v1's generic
    "constraint function call not supported in v0 solver path" — bought
    cheaply, since the corpus's deepest real nest is 3.

    Neither backstop is pinned by a test, and that is stated rather than
    papered over: the non-cyclic doubling chain they exist for is
    bottlenecked in `typed_lower`'s own un-budgeted expander (103 ms at
    12 levels, 11.3 s at 18) long before v1's backstop is reached, so a
    test would be measuring the other component. That blowup is
    pre-existing — it reproduces on a clean `origin/main` worktree — and
    budgeting that expander is a separate change.

    The regression test steps over the boundary divergence 59 documented:
    it calls `cpp_tb::emit` on five shapes: self-recursive, mutually
    recursive, both of those with a growing argument, and the
    paren-deepening one that defeated two earlier guards. Controls in
    the same test keep the guard honest about what it is not: a 40-deep
    chain of distinct relations and a 60-paren expression both still
    emit with the innermost bound intact. Disabling the name stack
    reproduces the SIGABRT and the test binary dies with signal 6.

63. **Not every `name(...)` in a constraint is a relation call
    (2026-08-17).**

    Divergence 59 recorded this as the last of its three open limits.
    Any `Ident`-callee call in a constraint went down the relation path,
    and a name that is not a declared relation came back as
    `UnknownRelation` — which divergence 59 had just promoted to a hard
    `Invalid`. v1 handles a small set of these itself, so that is a
    false refusal of a program v1 compiles, carrying a diagnostic that
    sends the reader looking for a `relation` declaration they never
    meant to write.

    v1's whole list for this shape is one entry, read off
    `cpp_tb::try_emit_constraint_list_call` rather than recalled:
    `sum(<list>[lo..hi])`, one argument. (`<list>.len()` is the other
    constraint builtin, but its callee is a `Field`, so it never reaches
    the relation path.) Everything else with an `Ident` callee v1
    rejects too — "constraint function call not supported in v0 solver
    path" — so `nosuchfn(p.n)` keeps its refusal. The verdict is right
    there even though the wording is about relations, because refusing
    is what both backends do.

    The builtin now records an ordinary capability gap, which
    `lower_program` discards, so the program lowers and reaches the
    shared emitter that knows how to emit it.

    **Three call sites, one fix.** The search found `expand_relation_
    subtree` already guarding correctly (`relation(name).is_some()`
    before expanding), which made it look like the top-level path was
    the only offender. The probe disagreed: a `sum(...)` nested inside
    a `==` also produced `UnknownRelation`. The third site is
    `lower_expr`'s `Call { Ident }` arm, and it reaches the same
    `expand_top_level_relation_call` — so the fix lands once, in the
    function all three funnel through. *Reading two of three call sites
    and generalising is how the previous batch got a verdict wrong; the
    probe is what found the third.*

    **Only the FALSE-REFUSAL case is latent — the first draft of this
    entry said the whole predicate was, and that was wrong.** TB-IR
    refuses any transaction carrying a `list<T>` field before constraint
    lowering runs (measured: the `sum` probe, an unknown-relation probe,
    and a clause with no call in it at all are all refused with the same
    "unsupported (non-scalar) leaf type" message), and every `sum` v1
    ACCEPTS needs a list field. So no v1-compiling program reaches the
    fix today, and the list-bearing assertions are on the constraint
    table: end-to-end there would pass for the wrong reason, since the
    list gate fires first and would keep passing however this were
    classified.

    But `sum` over a SCALAR reaches the fixed line right now, with no
    list field anywhere. `randomize(p) with sum(p.n) == 1` used to be
    refused as "`sum` names no `relation` declared in this file" and now
    lowers — an improvement, since v1 refuses it too and the user now
    gets v1's own accurate wording instead of a fiction about relations.
    *"Nothing can reach this" was asserted from the shape that motivated
    the fix, not from the predicate's actual domain.*

    That case also exposes what makes the fix safe, which is **not** the
    predicate. The check is on NAME and ARITY only, deliberately wider
    than v1 — which also requires a range-sliced list field, so
    `sum(p.n)` and `sum(items[0])` are v1 errors this predicate waves
    through. They do not become accepted programs: `tbir::emit` routes
    every constraint site back through v1's own emitter, which refuses
    them in v1's own words. The scalar case is now asserted end to end,
    because a predicate that is safe only because of what happens
    downstream needs the downstream asserted.

    What makes it worth fixing rather than filing as unreachable: the
    list gate's `--codegen v1` suggestion is **honest**. Give the list a
    bound (`items.len() <= 4`) and v1 emits the whole thing, `sum` call
    included — verified in the test. The false `UnknownRelation` was
    sitting directly in front of a form v1 compiles.

    One shape further out, same bug, found by the same review: a
    **relation whose name shadows the builtin at a different arity**.
    v1's expander declines on the arity mismatch and its list-`sum`
    builtin takes over, so v1 EMITS; TB-IR reported
    `RelationArityMismatch`, a hard `Invalid`. The arity arm now defers
    to the builtin the same way v1 does.

    Both directions are pinned by mutation, because widening a gate is
    as much a claim as narrowing one: emptying the builtin list makes
    `sum` a relation error again, and dropping the NAME check so any
    one-argument call counts makes `NoSuchRel(p)` stop being one.

64. **`logf`'s message is positional, and now TB-IR agrees
    (2026-08-17).**

    Divergence 58 measured this and deferred it because it changes an
    extractor the equivalence corpus exercises heavily. This closes it.

    v1 **consumes** the path: `StmtKind::LogF` splits the first
    positional string out of the argument list and hands `emit_log` what
    is left, so the message is simply the next positional string.
    TB-IR's `lower_log` instead searched for the first string whose
    VALUE differs from the path — the same answer only while the message
    happens not to equal the path. The fix is one line: take the first
    positional string for `log`, the second for `logf`.

    Measured on the two divergence-58 cases, comparing the emitted call
    from both backends:

    | source | v1 | TB-IR before | after |
    |---|---|---|---|
    | `logf("t.log", "t.log", error, "BOOM")` | `"t.log"` | `"BOOM"` | `"t.log"` |
    | `logf("t.log", error, "t.log")` | `"t.log"` | `""` | `"t.log"` |

    Five control shapes that already agreed still agree, `log`'s own
    first-string rule included — the same line has to get both right,
    which is why the plain-`log` controls are in the test rather than
    assumed. Full suite green, the equivalence registry included.

    Both directions are pinned by mutation: restoring the value
    comparison fails the message-equals-path case, and dropping the
    path-consumption so the first string is always taken fails the
    ordinary-`logf` case.

    One thing deliberately NOT moved: the named-argument guard above
    still runs on the FULL argument list, before the path is consumed.
    It exists to catch a named argument that leaves a positional slot
    empty, and counting a list the path has already been removed from
    would tell it there was one fewer slot to fill.

65. **The worst argument, not the first (2026-08-17).**

    Two loose ends on `reject_misplaced_named_args`, closed together
    because they are the same function.

    **`record_read` was unguarded.** It was the only record-API site
    that accepted an unknown parameter name silently. One argument does
    not make the check pointless: a name matching nothing is still a
    program error no backend can honour. Its parameter list — `addr` —
    comes from the compiler's own `Invalid` message two lines above the
    guard and from `docs/ral-support.md`, not from memory, which is the
    discipline `record_write` earned the hard way when it was given an
    invented `["reg", "value"]`.

    That lesson bit again while writing the test. The natural example
    for an unknown name is `record_read(reg = 4)` — and `reg` is a lexer
    keyword, so that program does not parse, and the assertion would
    have been measuring the parser rather than the guard. The test now
    asserts that it does not parse, and uses `nosuch` for the real case.
    *The same trap, at the same site, caught twice.*

    **The guard reported the first bad argument, not the worst.** Its
    two verdicts are not equally bad — an unknown name is `Invalid` (v1
    binds by position and emits exactly the right code), a misplaced
    known name is `SilentlyMisLowers` (v1 emits working C++ with the
    values swapped) — and the arguments are not examined in order of
    badness. So in `record_write(nosuch = 0x18, addr = 305419896)` the
    unknown name came first, its `Invalid` was returned, and the genuine
    swap behind it was never reported. Fixing the typo would then
    reveal a second error: exactly the experience a diagnostic should
    not give. The unknown-name verdict is now held rather than returned,
    so a swap anywhere in the list outranks it, and with no swap present
    it is still reported.

    Both are pinned by mutation: deleting the `record_read` call site
    fails the guard test, and restoring first-wins ordering fails the
    worst-argument case.

    Not guarded, and deliberately: `bitbash(regs)`. Its single argument
    has no declared parameter name anywhere — the compiler's message
    calls it "the regblock binding" and the docs write `bitbash(regs)`,
    where `regs` is the user's binding, not a parameter. Guarding it
    would mean inventing a name to check against, which is the mistake
    `record_write` already made once.

66. **A named argument in its own position is inert (2026-08-17).**

    Six arms answered `Unsupported` for any named argument, refusing the
    whole construct. v1 drops argument names and binds strictly by
    position, so the reordered form silently swaps values — already
    classified in divergence 57 — but the IN-ORDER form is inert.
    Measured per family, comparing emitted C++:

    | call | v1 emits |
    |---|---|
    | `hlp(111, 222)` | `hlp(111, 222)` |
    | `hlp(a = 111, b = 222)` | `hlp(111, 222)` |
    | `hlp(b = 222, a = 111)` | `hlp(222, 111)` |

    Byte-identical for the in-order form, in every family measured:
    free-function helper, testbench method (`Tb_hlp(_tb, 111, 222)`),
    extern fn (`ref_add(111, 222)`), transactor/component method
    (`AxilXactor_axil_write(_tb.env.drv, t.addr, t.value)`), and tseq
    (`mine(111, 222)`). Refusing the whole construct refused a form that
    costs the user nothing.

    Three of those families now route through
    `reject_misplaced_named_args`, which splits the three cases: name in
    its own position lowers, name in another position is
    `SilentlyMisLowers`, name matching no parameter is `Invalid`. Each
    parameter list is read off the DECLARATION the call resolves to.

    **Three were not converted at first, for the same reason in each:
    the seam had no parameter names to check against.** Divergence 67
    supplies them and closes all three.

    * The transactor-method sites in `stmts.rs` lower under a schema
      SNAPSHOT. `TransactorMethodSchema` carries `n_params` and not the
      names — its own doc comment says the count is "duplicated from the
      function so call sites (which lower under a schema snapshot,
      without the functions table) can check arity". Adding names to the
      schema is the enabling change.
    * `lower_component_call_args` serves **seven** callers, and some of
      them have no parameter list at all — it also lowers the payload of
      `emit <ev>(...)`. Its existing comment already recorded this
      ("telling the two apart needs the callee's parameter list"); this
      entry records that the measurement now exists, so only the
      plumbing is missing.
    * The covergroup helper target is reached from the covergroup
      lowering path, which resolves helpers through a different
      registry. Closed in divergence 68 — that registry turned out to
      carry the declaration too.

    That leaves the component-method arm still refusing an in-order
    named argument v1 emits identically — a known, measured, un-closed
    gap rather than an unexamined one.

    Each of the three converted call sites is pinned by mutation:
    deleting any one of them fails
    `a_named_argument_in_its_own_position_lowers_for_the_families_that_
    know_their_parameters`.

    Two keyword traps turned up while writing the probes, both the same
    class as the `reg` one from divergence 65: `seq` cannot name a
    `tseq` and `reg` cannot name a variable, so an example using either
    measures the parser rather than the thing under test.

67. **Carrying a count where the names were needed (2026-08-17).**

    Divergence 66 converted three call families and left three, each
    blocked on the same thing: the seam had a parameter COUNT and not
    the parameter NAMES, so it could not tell an inert named argument
    from a reordered one and refused both. The names were available at
    every construction site and thrown away.

    * `TransactorMethodSchema::n_params` → `param_names`. Built from
      `f.params`, which has the names.
    * `self_transactor_methods`, the sibling-call map, carried
      `(usize, bool, bool)` → `(Vec<String>, bool, bool)`. The same
      information was dropped twice, in two places, for the same reason.
    * `ComponentMethodSchema::n_params` → `param_names`.
    * `lower_component_call_args` takes `Option<&[String]>`: the four
      method callers pass the callee's names, the three
      `emit <ev>(...)` payload callers pass `None`, because an event
      payload genuinely has no declared name to check against and
      inventing one is the `record_write` mistake.

    Replacing the counts rather than adding a field beside them is
    deliberate — arity is now `param_names.len()`, so the two cannot
    disagree.

    **Two defects this introduced, both caught by running it.**

    The first: the new path lowered arguments with `lower_expr` where
    the positional path uses `lower_expr_no_ports`. Four snapshot tests
    failed with `PortInDisallowedPosition` on a `ComponentCall arg`. A
    named argument has to lower through the SAME seam as a positional
    one, or "the name is inert" stops being true. The line had been
    copied from a grep of the new code and assumed to match the old.

    The second is subtler and is the one worth remembering. With the
    names in hand, `axil_write(data = t.value)` on a TWO-parameter
    method reported a misplaced argument — "`data` is parameter 2 but
    was written in position 1 … this silently swaps them". Nothing is
    swapped. The call is UNDER-SUPPLIED, v1 emits the same
    under-supplied call the positional `axil_write(t.value)` emits, and
    TB-IR lowers that one. The guard was describing a pre-existing arity
    gap as a swap — a false explanation, the exact failure mode it had
    been rewritten to stop producing. `reject_misplaced_named_args` now
    claims a swap only when `args.len() == declared.len()`, because that
    is the only case where the positions correspond at all.

    Three tests that pinned the old blanket refusals were rewritten
    rather than deleted, and each now asserts v1's behaviour FIRST — the
    in-order form byte-identical to positional, the reordered one
    different — so the classification rests on a measurement instead of
    the arm agreeing with itself. One of them,
    `transactor_sibling_call_named_argument_is_rejected`, had pinned a
    refusal of `inner(n = 5)`: a name in its own position, the inert
    form. It was asserting that a working program was rejected.

68. **The last named-argument family (2026-08-17).**

    The covergroup helper call was the one site divergence 66 left and
    divergence 67 did not reach. It was assumed to need a fourth
    enabling change; it needed none. Both registries the site already
    takes as parameters carry the declaration — `HelperRegistry` for a
    file-level `function` (via `HelperEntry::decl`) and the extern map
    for an `extern function` — so the parameter names were in scope the
    whole time. *Checking beat assuming, again.*

    Measured for this family specifically rather than inherited from its
    five siblings, because variants sharing a shape do not share a
    verdict. In a coverpoint target:

    | call | v1 emits |
    |---|---|
    | `pick(<slice>, 1)` | `pick(<slice>, 1)` |
    | `pick(a = <slice>, b = 1)` | `pick(<slice>, 1)` |
    | `pick(b = 1, a = <slice>)` | `pick(1, <slice>)` |

    The swap lands inside the sampler that decides which bin gets hit,
    so a covergroup would report coverage against the wrong values with
    nothing to show for it.

    The first version of this also wrote a fallback refusing every
    named argument when neither registry resolved. **That arm was dead
    code**, and the commit message described a behaviour it did not
    implement: both call sites are inside `if let Some(...)` on those
    same registries, so the lookup could not fail. (A callee in neither
    registry is refused earlier, by the coverpoint call classifier — so
    the *program* behaviour the message claimed was real; the arm
    written to produce it was not.) The names are passed IN now, which
    makes the fact structural rather than accidental.

    A **seventh** family turned up in the same review: the `tseq` call.
    Measured — `RandomTxns(n = 5)` emits `RandomTxns(5)` under v1,
    byte-identical to positional, against
    `auto RandomTxns = [&](uint64_t n)` — and converted the same way, by
    carrying `TseqDecl::params` names in the `tseqs` map beside the
    element type.

    Two sites stay unconverted, and both for the same stated reason:
    `emit <ev>(...)` (an event payload has no parameter list) and
    `idle(N)` / `quiesced(N)`. The second is worth spelling out because
    it looks inconsistent with `record_read`, which WAS given a
    hand-written one-element list. The difference is whether a name
    exists to check against: `record_read`'s `addr` is stated by the
    compiler's own diagnostic and by `docs/ral-support.md`, while
    `idle`'s arity message says "exactly one cycle-count argument" and
    the docs write `idle(N)` with `N` a value placeholder. No parameter
    name is stated anywhere, so guarding it would mean inventing one —
    the `record_write` mistake. It stays as `bitbash` does.

    The `emit` arm's ≥2-argument diagnostic also said "named arguments
    in a component METHOD call". Every method caller now takes the
    guarded path, so that branch is reachable only from the three `emit`
    callers, and it was measured saying "method call" about
    `emit tagger.in_ev(a = 1, b = 2)`. Reworded to "component call", the
    wording the adjacent single-argument arm had already been given for
    exactly this reason.

69. **Shape is not resolution (2026-08-17).**

    The statement-position hooked-`on` arm asked
    `is_v1_method_hook_shape`, which accepts any dotted path of the
    right length. So `drv.send.x`, `nosuch.send`, `drv.plain` and
    `dut.rst.x` all reached an `Unsupported` — and therefore a
    "re-run with `--codegen v1`" suggestion.

    Measured, one program per shape against the `axilite_hooks_test`
    fixture:

    | trigger | v1 | suggestion honest? |
    |---|---|---|
    | `drv.send` | **emits** | yes |
    | `drv.send.x` | refuses | no |
    | `nosuch.send` | refuses | no |
    | `drv.plain` | refuses | no |
    | `dut.rst.x` | refuses | no |

    v1's message for all four is "obj.method must resolve to a
    `hookable` on a known component type". So the suggestion was honest
    for exactly ONE of the five, and the other four sent the user to a
    second error.

    A previous batch recorded this deliberately: the arm's comment said
    such paths "keep the suggestion, and the user lands on v1's own
    precise message rather than on silence". That is a defensible thing
    to want and the wrong verdict to encode — `Unsupported` promises v1
    is a way to run the program, and here it is not. The four now get
    `Rejects`, whose rendering ends "`--codegen v1` does not implement
    it either".

    The gate is v1's own condition and the same one the test-scope arm
    in `mod.rs` already applied: does `<obj>.<method>` name a `hookable`
    on a transactor or component testbench field? Checked in the
    recoverable direction — a miss yields the honest `Rejects`, a hit
    only ever upgrades to the suggestion.

    One implementation note, because it cost a wrong first attempt:
    `strip_tb_prefix` does not strip the desugarer's `_tb` root here.
    It only strips ahead of a COMPONENT field, and a hook target is
    usually a TRANSACTOR field, so `_tb.drv.send` stayed three segments
    long and every path failed the two-segment test — including the one
    that should have passed. The strip is done locally instead;
    widening the shared helper would change what its other callers
    resolve.

70. **The container, not the syntax, decides what v1 does with a
    scoreboard's `connect`/`on` (2026-08-17).**

    `lower_scoreboard` answers every `connect` and every unhooked `on`
    in a data-only scoreboard body with one verdict. That verdict was
    `Unsupported` — "re-run with `--codegen v1`" — and a previous batch
    tried to split it syntactically, on `is_v1_method_hook_shape`. The
    split got its rows backwards and was reverted: `on dut.en` is a
    two-segment PATH that v1 compiles, and `on w.seen > 0` is an
    EXPRESSION that v1 does not.

    Re-measured, and the reason no shape predicate can work here is
    that the discriminator is the CONTAINER the scoreboard is
    instantiated in, which a declaration-lowering seam cannot see. The
    same `scoreboard Board` type can be a transactor field or a
    testbench field:

    | container | input | v1 |
    |---|---|---|
    | transactor field | `connect` / `on` | **silently drops the wiring** |
    | testbench field | `connect` | emits uncompilable C++ |
    | testbench field | `on hits > 0` | emits a working checker |
    | testbench field | `on dut.rst` | emits a working checker |

    The transactor-field row is byte-measured, and on a program built
    to isolate it — a `Board` held by `transactor Sender` — the output
    with `on` wiring and with `connect` wiring is byte-identical to the
    same program whose scoreboard body is empty, with no residue at
    all. Nothing in the output observes the event. A scoreboard that
    should catch a mismatch sees no traffic and the test passes green.
    This is the row the whole status rests on, and it is now pinned by
    `a_transactor_held_scoreboard_has_its_wiring_dropped_by_v1`,
    anti-vacuity anchor included.

    The testbench-field `connect` row is compiler-measured: v1 emits
    `_tb.b.hits.push_back(...)`, and g++ says "request for member
    'push_back' in '_tb.Tb::b.Board::hits', which is of non-class type
    **'uint64_t'**".

    That type is worth its own line, because the first version of this
    entry said `uint32_t` — read off the `hits : uint<32>` in the HARC
    source rather than off the compiler, in the very paragraph claiming
    the opposite. `cpp_uint_for_width` widens every scalar ≤ 64 bits to
    `uint64_t`, so v1 cannot emit `uint32_t` for any scoreboard scalar;
    `scoreboards.rs`'s own module header says as much. The verdict for
    the row was unaffected, the evidence for it was fabricated, and
    "compiler-measured" is exactly the phrase that has to be earned.

    So `--codegen v1` genuinely IS a way to run a testbench-field `on`,
    and this seam does not offer it. It lowers a declaration and is not
    given the container — though the caller COULD supply one:
    `lower_program` already builds `env_held_type_names` and threads it
    into `transactor_is_component` for the same kind of question. "Does
    not", then, rather than "cannot".

    An arm's status is the worst thing v1 does anywhere under it, and a
    silent drop is the worst of the three, so the whole arm is now
    `SilentlyMisLowers` — strictly better than the old `Unsupported`
    for the rows where v1 is no escape hatch, worse than necessary for
    the one row where it is.

    Splitting on the container alone would not recover that row. A
    container-split testbench arm still spans `connect` and
    `on w.seen > 0`, both uncompilable, so worst-under-arm lands it on
    `EmitsUncompilable` — not `Unsupported`. Getting the suggestion
    back where it belongs needs the container AND the per-trigger scope
    analysis this arm has wanted since the reverted syntactic split:
    both, not either.

71. **Three arms named a construct no program reaching them can
    contain (2026-08-17).**

    The `on`-handler arms on the two bound-to transactor paths —
    `lower_bound_target_transactor`, plus the always-on and `when
    active` loops of `lower_bound_initiator_transactor` — all reported
    "event-driven transactors await the event slice".

    No program that reaches them is event-driven. The routing gate is
    `components::transactor_is_component`, which for a `bound to`
    transactor returns `has_on_handler`, and that flag is set by
    NON-periodic handlers alone. An event subscriber, a
    `bus.<ch>.handshake` monitor and a cycle-trigger therefore all go to
    the composite table; `on <N> cycles` is the only shape that falls
    through. A user who lands here wrote a periodic handler and was told
    to wait for a slice that will never cover it.

    The verdict itself is right, and now measured. v1 emits a
    `_checkers` closure holding a `static ..._last` stamp and the
    period, firing the body every N cycles against the instance's state
    struct — spliced into g++ with the emitted state struct and
    verified compilable, at all three positions. The handler is not
    merely present but load-bearing: removing any one arm makes TB-IR
    lower the program SILENTLY with the periodic handler dropped from
    the IR, which is what the arms are holding back.

    One row needed care. A `when active` periodic handler on a
    `passive` instance produces v1 output byte-identical to the same
    program with the handler deleted — which reads like a silent drop
    and is not: it is v1 obeying `when active`. The first version of
    the test asserted emission on the fixture's own `passive` binding
    and failed for exactly that reason. Both halves are pinned now, the
    `active` binding for emission and the `passive` one for the
    scoping.

    The general shape here is the same one that has bitten this sweep
    repeatedly: the arms in a file do not tell you what reaches the
    file. Only the gate does.

### The probe method

Every classification above came from the same mechanical check rather
than from reading v1's source: emit the construct under both backends
with `harc sim --emit-only`, and when v1 emits, READ the generated C++.
"v1 emits" is not the same as "v1 works" — of the ten constructs the
first sweep flagged as gaps, five turned out to be v1 emitting code that
does not compile or silently means something else. Only the ones where
v1's output is genuinely usable are worth mirroring; the rest want an
honest `NotImplemented` diagnostic instead.

## Negative tests: where rejection actually fires

As of #372 the randomize fixtures are no longer must-reject: both
`keep_constraints_test` and `axilite_constraint_test` lower and
trace-diff clean against v1 (the old `axilite_constraint_unsupported`
snapshot was retired). The agent/event fixture
(`wait_until_quiesce_test.harc`) remains a registered must-reject test.
Until the transaction slice, both tripped the
**item-level** gate on their `transaction` declarations before any
deeper construct was reached — the file-level scan in
`src/ir/lower/mod.rs` runs before body lowering — so the snapshot
text named `transaction`. That predicted shift has now happened:
with `transaction` in the subset, `wait_until_quiesce_unsupported`
names the `agent` construct (next item in file order),
`axilite_seqdrv_unsupported` named `transactor` — and shifted again
with the transactor slice (snapshot named `tseq`), then again with the
tseq slice (snapshot named the transactor **state field** `last_read`),
then again with the transactor-state-field slice (2026-06-13): that
fixture's state field now lowers, so the snapshot names the `req : in
event<RegOp>` directional field — the event-driven transactor form
(deferred). The `axilite_constraint`
fixture was the last member of this
group and has since left it — `randomize` lowers as of #372. The same
mechanics apply to the next construct slice: a fixture's snapshot always names
whichever out-of-subset construct lowering hits first, and shifts
deeper as slices land. The per-fixture residual map for the whole
former `transaction` group lives in
[tbir-coverage.md](tbir-coverage.md).

## Next steps

The remaining work is the plan doc's (gate redefined 2026-06-12 —
see its decision log):

- **Phase-4 completion** (plan phases 4–6): grow the tbir backend to
  v1's full feature set with equivalence-registry rows (including
  expect-fail) for the whole fixture corpus, flip the default to tbir,
  delete v1. No byte-parity step.
- **Passes** (plan phase 7): `placement` over a `TargetProfile`,
  `randomize_analysis`, `extract_port_set`, `hoist_stimulus`,
  `lower_coroutine` for FSM-shaped backends.
- **Subset growth**: randomize statement/terminator form + the
  constraint-IR `ConstraintRef` seam landed 2026-06-13 (#372); the
  residual randomize edges are the *expression* form (`let v =
  randomize(t)`) and method-body randomize. `tseq` (transaction-sequence
  iteration + `randomize`-in-tseq) landed 2026-06-13 (divergence 17) —
  the `tseq`-gated randomize edge is now lowered. Other
  growth: transaction *declarations* and non-randomize record usage
  landed 2026-06-12, as did range/cross bins, `any of`, the bus subset,
  and unbound DUT-poking transactors — the initiator-side
  `CallTarget::TransactorMethod` call edge is produced by both bus
  `tlm_method` lowering and transactor-field call lowering; bus-bound
  and event-driven transactor forms await the event slice. Remaining:
  `fork` (incl. `ForkArmKind::BusMethodCall` for the OOO TLM lanes),
  agents/events.
- **Placement-split backends** proceed per the multi-target placement
  model in [tb-ir-design.md](tb-ir-design.md) (tiers, timing classes,
  `TargetProfile` capability checks) once the passes exist.
