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
    | a queue method outside `push`/`pop`/`size`/`empty` (10 sites) | a call to a method `HarcQueue` never defines — superseded by divergences 82 and 84, which split all ten arms on measurement (the row's original count of 9 was one short) |
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
    mixed-verdict and had to be split. The table below is the current
    state, not the state at the batch that wrote it: later batches have
    split rows and re-measured verdicts in place, so the row count no
    longer matches any one batch's arm count.

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
    rejected fixture as evidence. Four of the five that do surface are
    program errors under any backend — a relation that does not exist,
    one called with the wrong arity, one that expands into itself, and
    (since divergence 72) one whose expansion is finite but past the
    shared size limit — so they are `Invalid` and name no escape hatch.

    The ODD ONE OUT is not. A misplaced argument name is accepted by v1, which
    emits working C++ with the values swapped, so `Invalid`'s "program
    error under every backend" is literally false for it. It carries
    `SilentlyMisLowers` instead — the sweep's ordinary shape, and the
    verdict that keeps the diagnostic from naming v1 as a way out.
    Siblings sharing one code path is not a reason to share one verdict.

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

    **Closed in divergence 72.** That separate change was made, and the
    reasoning above turned out to be the wrong conclusion from a right
    observation: both expanders run on every `harc sim`, so leaving one
    unbudgeted left the pair unbudgeted. The limits are shared now and
    both backstops ARE pinned, at the same boundary in both backends.
    The "corpus's deepest real nest is 3" line above stands; the
    "passes at 96 and fails at 88" figure quoted with it does not
    reproduce — see the constant's own doc in `ast.rs` for the
    re-measurement.

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
    | testbench field | `on w.seen > 0` | emits uncompilable C++ |
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
    the two where it is (`on hits > 0` and `on dut.rst`, both in a
    testbench container).

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

    The arms are load-bearing, not decorative: stub any one of them to
    `=> {}` and TB-IR lowers the program with the periodic handler
    simply absent from the IR. (Two of the three do that outright; the
    initiator always-on position needs an `active` binding to show it,
    because the initiator BFM form refuses a `passive` one before the
    arm is reached. The first version of this entry claimed the plain
    silent drop at all three and was measured only at two.)

    The VERDICT, though, was wrong — `Unsupported`, kept from the old
    code and asserted on one probe. `<N>` is any integer expression
    (spec §7.10), and v1 registers the closure near the top of the run
    function, so whether its output compiles depends on where the
    period expression's names are emitted:

    | period | v1 emits | compiles |
    |---|---|---|
    | `5`, `2 + 3` | `(int64_t)(5)`, `(int64_t)(2 + 3)` | yes |
    | `NPER`, a file-scope `const` | `(int64_t)(NPER)`, declared at namespace scope ~80 lines earlier | yes |
    | `read_count`, a state field | `(int64_t)(target.read_count)`, instance declared 3 lines earlier | yes |
    | `limit`, a `let` declared AFTER the transactor's binding | `(int64_t)(limit)`, declared **64 lines later** | **no** |
    | the same `let`, moved between the BUS binding and that one | same, declared before the registration | yes, and correct |
    | `limit`, with a file-scope `const limit = 7` as well | same, and it RESOLVES — to the const | yes, **and runs at the wrong rate** |

    The fourth row is compiler-measured, not read off the text: g++ on
    the spliced closure says "'limit' was not declared in this scope".
    It reaches the arm because the name resolver does not visit a
    bound-to transactor's `on` trigger at all — `on some_undefined_name
    cycles` also passes `harc check`.

    The fifth row is that same program with the `let` moved one line
    up, above the transactor's binding, and it compiles and runs
    correctly (built and run: 4 firings in 21 cycles at period 5). So
    `--codegen v1` is a real escape hatch there, and the discriminator
    is the `let`'s POSITION relative to the binding — not "an
    impl-scope `let`" as a category, which is what the first version of
    this entry and the user-facing detail both asserted. An arm whose
    input space is an expression does not get to be described by the
    category of one probe.

    The sixth is the one that sets the status, and it was BUILT AND
    RUN, not just compiled: with `const limit = 7` at file scope and
    `let limit = 5` in the impl, the closure resolves to the
    `constexpr` at namespace scope while the rest of the run body sees
    the `let` that shadows it. The handler fires twice in 21 cycles
    instead of four. It runs at a rate the program never asks for, and
    nothing says so.

    That is the same silent const-capture the transactor-parameter arm
    at the top of the same file already reports, and it means the
    discriminator is once more name resolution in the emitted C++
    rather than the shape of the trigger — exactly as on the scoreboard
    wiring arm, with no predicate over the trigger to split on.
    Worst-under-arm makes all three `SilentlyMisLowers`. The literal
    case pays for that by losing a suggestion it would have deserved.

    A first pass at this entry stopped at the fourth row and labelled
    the arms `EmitsUncompilable`. The sixth row was found by asking the
    next question rather than the obvious one — not "does it compile?"
    but "is there a program where it compiles and is still wrong?" —
    and every arm whose input space is an EXPRESSION has that question
    waiting in it. The fifth row came from the mirror-image question,
    "is there a program where the category I just named is fine?", and
    the answer was yes.

    One row needed care in the other direction. A `when active`
    periodic handler on a `passive` instance produces v1 output
    byte-identical to the same program with the handler deleted — which
    reads like a silent drop and is not: it is v1 obeying `when
    active`. The first version of the test asserted emission on the
    fixture's own `passive` binding and failed for exactly that reason.
    Both halves are pinned now.

    Two general shapes, both of which have bitten this sweep before:
    the arms in a file do not tell you what reaches the file — only the
    gate does; and one probe of one shape does not measure an arm whose
    input space is an expression.

72. **Bounding one of two expanders bounds nothing (2026-08-17).**

    An earlier batch gave v1's relation expander a node budget and a
    depth backstop, and closed the entry noting that the same shape was
    "bottlenecked in `typed_lower`'s own un-budgeted expander long
    before v1's backstop is reached, so a test here would be measuring
    something else". Both halves of that sentence were true. The
    conclusion drawn from them was wrong: it treated the second
    expander as someone else's problem, when it is the same programs
    running through it on the same command.

    There are TWO expanders that inline `relation` bodies —
    `codegen::cpp_tb::expand_relation_calls` and
    `constraints::typed_lower::expand_top_level_relation_call` — and
    BOTH run on every `harc sim`, whatever `--codegen` says. Measured
    on a chain of distinct relations each calling the previous one
    twice (nothing cyclic, so neither name stack fires):

    | links | v1 | tbir |
    |---|---|---|
    | 16 | 2.5s | 7.3s |
    | 18 | 12.1s | 33.5s |
    | 20 | did not finish in 60s | did not finish in 60s |

    Instrumenting both expanders with call counters settles which one:
    at 16 links v1's is capped at ~4096 calls by its budget, while
    `typed_lower`'s runs 2^16 and climbing. The v1 column is slow for
    the same reason the tbir column is — `typed_lower` is on the path
    under both.

    That measurement also corrects a reading taken minutes earlier from
    a modulo-100000 counter, which showed "2 calls" under `--codegen
    v1` and looked like proof the typed path was tbir-only. A counter
    that prints every 100000th call cannot distinguish 2 from 65536.

    The fix is one budget, not two: both limits and the node counter
    that charges them moved to `ast.rs`, the module both expanders
    already depend on, and `typed_lower` now charges the same constants
    v1 does. They are a property of how large an expansion the compiler
    accepts, not of either emitter.

    With that, the boundary is symmetric and measured: a 9-link
    doubling chain expands in both backends, a 10-link one is refused
    by both; a 63-link linear chain expands in both, a 64-link one is
    refused by both. A 24-link chain that never finished now answers in
    37ms. `typed_lower` gets a `RelationExpansionTooLarge` variant that
    surfaces as `Invalid` — v1 stops at the same point and its
    translator then says "constraint function call not supported in v0
    solver path", so neither backend runs these and naming one as the
    way out would be false.

    One thing the new arm must not do is what the old code did on the
    way to its error: return an empty clause list and let the program
    lower with the constraint silently gone. It records the error, and
    `is_relation_error` includes it, so the walk stops.

73. **One of three `emit` branches checked the payload arity
    (2026-08-17).**

    `emit <ev>(...)` lowers through three branches in `components.rs`:
    the test-scope `let e : event<T>` local, the dotted path
    (`emit tagger.in_ev(v)`), and the self-relative form
    (`emit observed(v)` inside a method body). Only the first checked
    that an event payload is exactly one argument. The other two took
    whatever they were given, and the asymmetry showed up incidentally
    while probing something else — the same source shape refused as a
    local and accepted as a path.

    Measured on both backends at both unchecked sites:

    | arity | tbir | v1 |
    |---|---|---|
    | over — `(v, 2)` | `_s(v, 2)` — uncompilable | `_s(v)` — **silently drops the extra payload** |
    | under — `()` | `_s()` — uncompilable | `_s()` — uncompilable |

    The uncompilable cells are compiler-measured against the emitted
    channel type: g++ on `_s(v, 2)` against
    `std::function<void(uint64_t)>` says "no match for call to
    '(std::function<void(long unsigned int)>) (int, int)'".

    No backend runs any of the four cells as written, so `Invalid` —
    the same verdict the local-event branch has given all along, now
    given by all three. The worst cell is v1's silent drop, which is
    what makes this worth more than a tidier diagnostic: `emit
    tagger.in_ev(i + 1, 2)` ran green under v1 with the second payload
    thrown away.

    `Invalid` refuses programs outright, so the obvious way to get this
    wrong is a legal spelling where the arity is NOT one. Two candidates
    exist and both pass `harc check`: a bare `in_ev : event` with no
    type argument, and `in_ev : event<uint<8>, uint<8>>` with two. So
    the rule was checked against v1 rather than read off the spec's
    `type event<T>` (§3.4 / §7.3; §17.2 is the PSS flow-object section
    and defines no emit arity),
    and it holds for a reason stronger than the spec:

      * bare `event` — v1 emits the channel as
        `std::vector<std::function<void(uint64_t)>>` anyway and
        registers `on in_ev()`'s handler as a ONE-parameter lambda,
        then emits `_s()` at the emit site. g++: "no match for call to
        '(std::function<void(long unsigned int)>) ()'".
      * `event<A, B>` — same `void(uint64_t)` channel, and the emit
        drops the second payload silently.

    v1 has exactly one payload slot however the field is spelled, so
    there is no arity but one to be right about, and neither
    alternative spelling is an escape hatch.

74. **Six `connect` arms, and two wrong verdicts before the right ones
    (2026-08-18).**

    `lower_connect_edge` refuses malformed edges at two quite different
    points, and an earlier batch gave both the same `Unsupported`.

    The ENDPOINT-SHAPE arms — a non-path endpoint, a one-segment path,
    an unresolvable segment — are genuinely mixed, and that batch
    measured why: a malformed PATH can still mean something to v1,
    because a single-segment endpoint resolves against the owner's own
    hookable and works. Those keep their suggestion.

    The SEMANTIC arms are different, and getting them right took two
    passes. Measured on an INSTANTIATED env, which is the only place v1
    looks at the edge:

    | edge | v1 | verdict |
    |---|---|---|
    | sink is a plain `function` | `for (auto& _s : sink.plain)` — g++: "'struct Sink' has no member named 'plain'" | `EmitsUncompilable` |
    | sink is a scalar field | `for (auto& _s : sink.other)` over a `uint64_t` | `EmitsUncompilable` |
    | sink method takes 2 parameters | "connect: hookable sink `sink.two` must take exactly one payload argument, got 2" | `Rejects` |
    | sink method returns a value | "connect: hookable sink `sink.ret` must return void" | `Rejects` |
    | payload mismatch, RECORD vs scalar | g++: "no match for call to '(<lambda(Sink&, Beat)>) (Sink&, long unsigned int&)'" | `EmitsUncompilable` |
    | payload mismatch, SIGNEDNESS only | **compiles and runs correctly** | `Unsupported` |

    The payload rows are one arm covering several shapes, because
    `event_payload_matches_ir_type` compares signedness and record
    identity only — width is irrelevant to it. The non-signedness side
    is not just record-vs-scalar either: two DIFFERENT records, and a
    component-typed sink parameter (`method_schema_ir_type` can produce
    `IrType::Component`), land there too, and the arm's detail named
    only the first until review pointed at the others. v1's bridge lambda is
    GENERIC (`push_back([&](auto _t) { ... })`) and looks type-agnostic;
    converting it to the source's `std::function<void(uint64_t)>`
    instantiates it at the wiring line. A record cannot survive that. A
    sign difference passes straight through as an ordinary implicit
    conversion: built and run, an `event<uint<8>>` into a `sint<8>` sink
    gives `count=2 sum=8`, exactly what the program asks for.

    The first pass called all six `Invalid`, on the argument that an
    uninstantiated env is "v1 not looking, not v1 running the program".
    Both halves of that are wrong. v1 emits, compiles and RUNS an
    uninstantiated malformed env to completion, so "a program error
    under every backend" is false for all six; and it was the same
    observation the endpoint-shape arms cite for KEEPING their
    suggestion, so one observation was reaching opposite verdicts in one
    function. The narrower true difference is that an instantiated
    malformed PATH can still mean something to v1, while an instantiated
    bad SINK never does — except the signedness row, which is why it is
    split out.

    A test in the suite was the counterexample the whole time: the
    pre-existing `analysis_connect_rejects_non_hookable_and_payload_
    mismatched_sinks` builds `hookable accept(v: sint<8>)` against a
    `uint<8>` source, and the first pass changed it from
    `assert_unsupported` to `assert_invalid` — moving the one row that
    disproved the verdict under the verdict.

    Separately, one of the two tests over this family had written its
    conclusion down without measuring it: its doc said the signature
    checks "keep the suggestion too, for the same reason", and it only
    ever built the instantiated case. It builds both now. Four of the
    six arms had no test at all.

75. **The same period expression, a fourth landing (2026-08-18).**

    Divergence 71 measured the non-literal `on <N> cycles` period at
    three bound-to transactor arms. Grouping by what a construct DOES
    rather than where it is spelled turned up a fourth: the
    testbench-scoped periodic handler in `mod.rs`, still `Unsupported`
    and untested.

    It behaves identically, because v1 emits it through the same
    machinery — the period expression verbatim into a `_checkers`
    closure registered near the top of the run function, ahead of the
    impl's own `let`s:

    | period | v1 |
    |---|---|
    | `2` — a literal | fine, and a registered fixture proves it |
    | `per`, an impl-scope `let` | used at line 161, declared at 175 — does not compile |
    | `per`, with a file-scope `const per = 7` too | resolves to the const: compiles, runs at 7 |

    The last row was built and RUN: two firings in 21 cycles where the
    source asks for a period of 2. Same verdict as the other three,
    `SilentlyMisLowers`, and it now has the test the arm never had.

    Worth noting what found it. Not a probe of this arm — a survey of
    `mod.rs`'s remaining sites by CONSTRUCT, in which "a
    testbench-scoped `on <N> cycles` handler with a non-literal period"
    read as the same sentence as three arms already measured in another
    file. The measurement then took one probe rather than a batch.

76. **A missing mode annotation and a wrong one are not the same
    failure (2026-08-18).**

    Two bound-to instance arms — the initiator BFM, which must be
    `active`, and the target-TLM responder, which must be `passive` —
    each answered BOTH ways of getting the annotation wrong with one
    `Unsupported`. v1 answers them very differently:

    | instance | v1 |
    |---|---|
    | no annotation at all | refuses: "let helper: transactor instantiation requires a mode annotation (`AxilHelper active` or `AxilHelper passive`)" |
    | initiator BFM declared `passive` | emits, byte-identical to the `active` program |
    | target responder declared `active` | emits, byte-identical to the `passive` program |

    A missing annotation is a program error under both backends, so
    `Invalid` and no suggestion. The WRONG annotation is v1 dropping it:
    the user asks for a passive instance and gets one that drives the
    bus, or asks for an active responder and gets a passive one, with
    nothing said either way. That is `SilentlyMisLowers`.

    "Byte-identical" needs its anti-vacuity check here, because it could
    just mean v1 has no notion of mode: for a transactor that HAS both
    halves — `axilite_bound_mon_test`'s `AxilXactor`, an active driver
    plus an always-on monitor — flipping the instance's mode changes 67
    lines of v1's output. It is specifically the hookable-only and
    thread-only shapes whose annotation v1 drops.

    The split is on `mode: None` versus `mode: Some(wrong)` in the AST,
    which is the exact distinction rather than a proxy for it. That
    matters because the sweep has twice tried to split an arm on
    something that merely correlated with the discriminator, and both
    times the split was backwards.

77. **The same split at the FIELD position, and the half that does not
    move (2026-08-18).**

    Divergence 76 split missing-annotation from wrong-annotation at two
    instance (`let`) arms. The field arms have the same two halves, and
    already had them as separate arms — both `Unsupported`.

    | field | v1 |
    |---|---|
    | `drv : CounterDrv` — no mode | refuses: "transactor field `_tb.drv : CounterDrv` has no mode and ..." |
    | `p : Poker` — no mode, plain transactor | refuses, same message |
    | `drv : CounterDrv passive`, handler inside `when active` | emits, and correctly OMITS the registration |
    | `c : Consumer passive`, handler in the ALWAYS-ON body | emits, and correctly KEEPS it — byte-identical to `active` |

    So the two halves part company. A missing annotation is a program
    error under both backends → `Invalid`. The `passive` one is a legal
    program v1 runs FAITHFULLY, in both of its shapes: with the handler
    inside `when active` v1 omits the registration, which is the
    language's own rule; with the handler in the ALWAYS-ON body v1 keeps
    it and the output is byte-identical to the `active` program. It
    keeps its suggestion for exactly that reason.

    The second shape also corrected the arm's DETAIL, which said the
    handler "only registers on an `active` instance". That is true of
    the shape it was written from and false of the other one under the
    same arm — the verdict survived the second measurement, the
    explanation did not.

    The mode-less arm's own comment has always said the rules "mirror
    v1". This one does — which is precisely why pointing at v1 was
    never going to help.

    Two arms alongside these were initially left alone — the
    DUT-poking-BFM pair — on the grounds that reaching them needs the
    transactor held by an `env` (they gate on `dut_poking_bfm_names`,
    the by-value-in-a-component routing) and no probe here built that.
    Review built it in ten lines: a `when active` hookable transactor
    with a `dut` field, held by an `env`, plus a mode-less testbench
    field of the same type. v1 refuses it with the same "has no mode and
    no parent specifies one" that made the two siblings `Invalid`, so
    the mode-less half is now `Invalid` too. "Not probed" is a reason to
    go and probe, not a reason to leave a false suggestion standing.

    The `passive` half of that pair does stay put, for the reason the
    rest of this entry gives: variants sharing a code path do not share
    a verdict.

    One probe went wrong in a way worth recording: the first attempt
    edited the fixture's FIRST `drv : CounterDrv active`, which is an
    `env` field, and all three variants lowered. An env-held mode-less
    field never reaches these arms at all. The arm being measured has to
    be the arm the probe actually hits.

78. **The fifth landing is the one that does not move (2026-08-18).**

    Four arms now carry `SilentlyMisLowers` for a non-literal
    `on <N> cycles` period. The fifth — a statement-position `on <N>
    cycles`, in `stmts.rs` — keeps `Unsupported`, and measuring it is
    what makes the other four's verdict mean anything.

    | landing | v1 registers the closure | `on per cycles` with `let per = 2` |
    |---|---|---|
    | three bound-to transactor arms, plus the testbench-scoped one | near the top of the run function, ahead of the impl's `let`s | uncompilable, or silently resolves to a same-named file-scope `const` |
    | statement position | at the statement, after the `let`s | resolves to 2 |

    Built and run with `const per = 7` present as well — the exact
    program that makes the other four mis-lower — this one fires 10
    times in 21 cycles at period 2. Correct, because the `let` shadows
    the const at the point of use rather than the other way round.

    So the construct is identical at all five sites and the verdict is
    not, and what separates them is nothing about the construct: it is
    where v1 happens to emit the registration. Applying the other
    landings' verdict here by analogy — which the grouping-by-construct
    rule might have invited — would have been wrong.

    That is the honest form of the rule. Group by what a construct DOES
    to find the sites; measure each site anyway.

    **And then measure each INPUT.** This entry originally closed there,
    leaving the fifth arm on `Unsupported`, and that was one question
    short again. `tb_periodic_literal` answers `None` for a NON-POSITIVE
    literal as well as for a non-literal, so `on 0 cycles` lands on the
    same arm — and there v1 emits the handler and its own `period > 0`
    guard never lets it fire. Built and run: 0 firings in 21 cycles. The
    program asked for a handler and got a silent no-op.

    Worst-under-arm makes the fifth arm `SilentlyMisLowers` after all —
    not because the row this entry measured mis-lowers, but because a
    row it never looked at does. The named-period row remains a genuine
    escape hatch and the detail says so; splitting on
    `parse_int_literal_expr(..) == Some(0)` would recover the suggestion
    for it and is not done here.

    The same `on 0 cycles` input reaches the testbench-scoped arm
    (divergence 75), where it only confirms an existing
    `SilentlyMisLowers`. Both arms' CONSTRUCT text said "with a
    non-literal period", which is false for `0`; both now say
    "non-literal or non-positive".

79. **Six arms, two measurements (2026-08-18).**

    Naming a component method that does not exist, and using a `void`
    method's result as a value. Six arms across `stmts.rs` and
    `components.rs` said `Unsupported` for these, and v1 emits both,
    verbatim:

    | source | v1 emits | g++ |
    |---|---|---|
    | `let x : uint<32> = c.nosuch(3)` | `uint64_t x = c.nosuch(3);` | "'struct Calc' has no member named 'nosuch'" |
    | `let x = c.noret(3)` | `auto x = Calc_noret(c, 3);` | "deduced type 'void' for 'x' is incomplete" |
    | `let x : uint<32> = c.noret(3)` | `uint64_t x = Calc_noret(c, 3);` | "void value not ignored as it ought to be" |

    Both are type errors, and unlike the `connect` arms there is no
    uninstantiated position for a statement in a run body to hide in.

    **But "has no DECLARED method" is a wider set than "does not
    exist", and the first pass conflated them.** The built-in component
    predicates — `idle`, `idle_in`, `idle_out`, `quiesced` — are not
    declared methods, so they land on the same arm. v1 implements all
    four, and so does TB-IR, one statement position over:
    `assert c.idle(2)` lowers and emits, while `let q = c.idle(2)` came
    through this arm and was told the program was invalid. `Invalid` was
    false about BOTH backends — the same failure undone for `connect` a
    few entries earlier, from the same reasoning error. Built-ins in a
    binding position are carved out to `Unsupported`, which is what they
    were before and should have stayed.

    The rest of the arm is a genuine program error, and `Invalid` —
    which is what `exprs.rs`'s transactor-shaped sibling has said all
    along.

    The scoping was wrong too. Mutating each arm says which is which:

      * the resolver's path-form arm in `components.rs` — reached.
      * the untyped-`let` "returns no value" arm — reached.
      * the three "has no method" arms in `stmts.rs` — UNREACHABLE.
        `as_component_method_call` validates the method on every path
        that returns `Ok(Some(..))`, so a caller holding a resolved
        method always has one. Neutering any of them fails nothing.
        (This claim survived review; the next one did not.)
      * the typed-`let` and assignment "returns no value" arms, and the
        parameter-form resolver arm — recorded as NOT PROBED, and all
        three are reachable with a two-line probe. A typed `let` is
        claimed by the untyped handler only for SCALAR types, and that
        arm is guarded on a record: `let t : TinyTxn = c.noret(3)` lands
        on it first try. An assignment is not a `let`, so nothing claims
        it. The parameter-form arm fires for a COMPONENT-typed
        parameter — the transactor-method arm blamed for claiming it
        only handles transactor-typed ones. All three are pinned now,
        which also means a missing method has TWO landings, not the one
        this entry originally claimed.

    "Not probed" turned out to mean "I stopped building probes", and
    writing it into the code made it read as a measured property of the
    arm.

80. **Two subscription arms, adjacent, opposite verdicts
    (2026-08-18).**

    `lower_event_subscription` refuses a statement-position `on
    <event>(...)` two ways within four lines of each other, and both
    said `Unsupported`.

    | subscription | v1 |
    |---|---|
    | `on s.obs(v)` — a component's `event` field, by path | `_tb.s.obs.push_back(...)` against a real member — **compiles and runs** |
    | `on nosuch(v)` — a name that resolves to nothing | `nosuch.push_back(...)` — "'nosuch' was not declared in this scope" |

    The first row is a real escape hatch: v1 compiles AND runs it. That
    was verified with a firing stimulus (`s.fire(3)`, which emits
    `obs` inside the agent) — the test originally subscribed to
    `s.obs` and then emitted on `ch`, a channel nobody subscribed to, so
    the `seen=3` first recorded here came from a hand-built program and
    not from the source the test names. Conclusion unchanged, evidence
    corrected, and the test fires the right stimulus now.

    The second is `Invalid` — but NOT because it is an undefined
    identifier, which is what this entry first claimed. `lookup` failing
    means the name is not a LOCAL. Measured, all of these land there and
    every one is declared somewhere: a testbench component field, a
    testbench scalar field, the clock, the DUT binding, an agent TYPE
    name, and a component METHOD name. v1 emits `<name>.push_back(...)`
    for each and g++ refuses all six, so the verdict holds and the
    message ("names no event channel in scope") is accurate — the
    REASONING was wrong, in the sentence that claimed to have checked
    it. All six are pinned now.

    One neighbour claim was not merely wrong but vacuous: the test
    asserted that a testbench `event` field "is claimed by its own arm",
    but that source dies at FIELD-DECLARATION lowering, long before the
    `run` body — so the assertion passed for an unrelated reason and
    would have passed with the whole subscription arm deleted. It pins
    the real cause now.

    Worth noting for its own sake: these two arms are four lines apart,
    handle the same statement, and needed opposite verdicts. Proximity
    is not evidence.

81. **A shadowed name makes v1 write to the DUT (2026-08-18).**

    Two arms, probed together because both looked like "the target is
    not a thing you can assign to".

    `release n`, where `n` is a testbench scalar rather than a DUT probe
    force: v1 refuses too, with "`release` target must be
    `dut.<probe_name>`". A program error under both backends →
    `Invalid`.

    The assignment arm looked the same and is not. It spans:

    | source | v1 |
    |---|---|
    | `5 = n` | emits `5 = _tb.n;` — g++: "lvalue required as left operand of assignment" |
    | `hookable poke(dut: uint<8>)` with `dut.en = 1` in the body | emits `harc_rt::harc_assign(self.dut->en, 1)` — **writes to the DUT port** |

    The second row was built and RUN: `dut.en=1`, and the `uint<8>`
    parameter was never touched. v1 ignores the shadowing entirely and
    resolves `dut` to the transactor's own DUT handle. The source says
    the name is a parameter; the program pokes hardware. That is the
    worst thing under the arm, so `SilentlyMisLowers` — and it is the
    reason this arm is not `Invalid` alongside its `release` neighbour,
    since v1 does run that program, just not the one that was written.

    The existing test over this arm carried the opposite claim in its
    doc — "v1 surfaces the shadowing as a C++ compile error" — asserted,
    never measured, and false. It now asserts the emitted line instead
    of describing it.

    Third time in this sweep that asking "what ELSE is under this arm"
    changed a verdict rather than confirming one.

82. **A queue method in statement position splits on the runtime's own
    API (2026-08-18).**

    FIVE arms catch a queue method in statement position that is not
    `push` or `pop` — testbench-owned field, scoreboard queue,
    component queue, bare target-state field, instance-qualified
    target-state field. (The first pass said "two", split those two,
    and left the other three carrying a hand-written
    `EmitsUncompilable`; see the correction below.) v1 emits the call
    against `harc_rt::HarcQueue`, whose entire API is `push`, `pop`,
    `size` and `empty`:

    | statement | g++ |
    |---|---|
    | `sb.q.size()` | compiles — the value is discarded, so it is a legal no-op |
    | `sb.q.empty()` | compiles, same |
    | `sb.q.clear()` | "'struct harc_rt::HarcQueue<long unsigned int>' has no member named 'clear'" |
    | `sb.q.front()` | same, no `front` |

    So `size`/`empty` keep the suggestion — v1 genuinely runs those
    programs — and everything else is a program error no backend runs.
    The discriminator is the runtime header for every name except four:
    `try_emit_width_method` claims `trunc`/`zext`/`sext`/`resize` by
    name before the member-call path, so `sb.q.trunc(2)` comes out as
    `((uint64_t)(((uint64_t)(_tb.sb.q) & 0x3ULL)));` and not as a
    `.trunc(2)` call at all. The `Invalid` verdict survives for those
    four — g++ rejects the cast — but for a different reason than the
    one first recorded here, which said v1 "passes whatever name is
    written straight through". It does, except where it does not.

    All five landings were probed independently rather than four
    inferred from one, and the test enumerates the member declarations
    in `runtime/harc_queue_rt.h` and compares the whole set — if
    `HarcQueue` grows a `back`, the test fails instead of a working
    call being reported as a program error.

    That enumeration took three tries to become what it claimed. The
    line-oriented version missed a declaration whose return type sits
    on its own line (`std::deque<T>` / `drain() {...}`): adding one
    left the test green while `pend.drain()` was being reported as a
    program error for a statement v1 compiles and runs. It scans the
    struct body at brace DEPTH 0 now, which is what actually separates
    a declaration from the `_d.front()` call inside `pop`'s body —
    indentation never did. An overload and an inherited member are
    still invisible to a name-set comparison, and the test says so
    instead of implying otherwise.

    A footnote on that scan, because it repeated the sweep's own lesson
    at miniature scale: the first version asked whether the header
    `contains("front(")` and failed, because `pop`'s body calls
    `_d.front()` — and the second version failed too, because
    `_d.pop_front()` contains `front(`. A substring test is not a name
    test, in the same way a shape test is not a resolution test. The
    third version fixed the boundary but still only spot-checked two
    ABSENT names, which is a blacklist wearing an API check's clothes:
    adding `T back() const` to the header left it green. Enumerating
    the set is what finally made it the check it claimed to be.

    **Correction, same day.** "Two arms" was wrong twice over. There
    are five, and the three left untouched kept telling users that
    `--codegen v1` "accepts it but emits C++ that does not compile" for
    `pend.size()`, `pending.size()` and `model.pending.size()` — all
    three of which v1 emits as `_tb.pend.size();`,
    `self.pending.size();` and `_tb.model.pending.size();`, and g++
    compiles all three. Grouping by construct found the sites; it did
    not transfer the verdict, and three of the five were shipped with a
    verdict nothing had measured. All five share the helper now.

83. **Six covergroup hook-trigger arms, two of them reachable
    (2026-08-18).**

    `lower_hook_call` and `hook_call_arg_names` between them refuse a
    malformed `covergroup G @(<trigger> post)` six ways, all
    `Unsupported`. Probing each shape says which arm actually sees it:

    | trigger | who refuses it |
    |---|---|
    | `@(drv.step(n) post)` | nobody — the control |
    | `@((drv).step(n) post)` | nobody — the paren is unwrapped |
    | `@(step(n) post)` | LOWERING: the callee is not a field access |
    | `@((drv.x + 1).step(n) post)` | LOWERING: the receiver is not a path |
    | `@(drv.step post)` | the PARSER: "must be a method call before `pre` or `post`" |
    | `@(drv.step(n + 1) post)` | the PARSER: "arguments must be identifiers" |

    So `validate_cover_hook_trigger` checks the call shape and the
    argument shape, and nothing checks the callee form — which is
    exactly what the two functions' own doc comments already said, and
    the first time in this sweep that a code comment's reachability
    claim turned out to be right.

    ~~Both reachable arms are `Invalid`~~ — **retracted, see divergence
    89.** They are `Unsupported`: v1's refusal only fires where the
    covergroup is INSTANTIATED, and an uninstantiated one builds. The
    sentence is left standing with this marker because the reasoning
    that produced it ("v1 refuses each with its own …") is the
    instructive part — it was true of the only configuration probed. The four
    parser-guarded arms take the same verdict as invariant guards —
    an unreachable arm cannot emit a false `--codegen v1` suggestion,
    and if one ever did fire the program would be malformed.

    The test pins the parser rows too, since "the parser gets there
    first" is the entire reason those four are annotated rather than
    measured.

    One thing the test caught about itself: the trigger text
    `@(drv.step(n) post)` appears in a COMMENT eight lines above the
    declaration, so a `replacen(.., 1)` edited the comment and left the
    program lowering cleanly. The CLI probes had used `sed`, which
    rewrites both lines, so they were right by accident. Anchoring on
    the whole declaration fixes it — the probe measuring the wrong line,
    one more time, in miniature.

84. **A queue method in EXPRESSION position is a program error, by two
    different mechanisms (2026-08-18).**

    Five more arms, siblings of the statement-position five, sit in the
    `lower_*_queue_query_call` families. `size` and `empty` lower there
    and `pop` has its own arm, so what reaches the fallback is either
    `push` — a real `HarcQueue` member that returns void — or a name
    the runtime never declares. All five said
    `NotImplemented { v1: EmitsUncompilable }`, which is the right
    shape for the wrong reason: "HARC does not implement this yet"
    describes a gap, and `q.front()` is not a gap, it is a call to
    something that does not exist.

    Measured at all five landings, v1 emits `uint64_t z =
    <recv>.<name>(...);` every time, and g++ rejects every one:

    | call | g++ |
    |---|---|
    | `q.push(3)` | "void value not ignored as it ought to be" |
    | `q.push()` | "no matching function for call to `HarcQueue<...>::push()`" |
    | `q.front()` / `q.clear()` / a typo | "has no member named `front`" |
    | `q.size()` | compiles — which is why it never reaches this arm |

    So all ten are `Invalid`, and the two halves carry different
    messages because they fail for different reasons: `push` names the
    mechanism (returns no value), everything else names the API — at
    four of the five expression landings. The testbench-owned one runs
    its `!args.is_empty()` arity check BEFORE the method match, so
    `let z = pend.push(3)` is refused for its arguments instead. Same
    verdict, different message, and the test pins that rather than
    letting the sentence above be read as covering all five.

85. **`pop` ignored its argument list at all nine branches
    (2026-08-18).**

    Every `pop` branch checked the method NAME and dropped `args` on the
    floor, so `q.pop(7, 9)` lowered and emitted cleanly under TB-IR
    while v1 emitted `_tb.pend.pop(7, 9);` — g++: "no matching function
    for call to `harc_rt::HarcQueue<long unsigned int>::pop(int, int)`".
    The `push` branches three lines away have always matched
    `[CallArg::Expr(arg)]` exactly, so this was an asymmetry nothing
    had looked at, in the same functions the queue-method split had
    just been written into. `Invalid`.

    NINE guards across TEN landings — the tenth (`let v = sb.q.pop(7,
    9)`) is claimed first by the older `as_scoreboard_pop` arity check.
    The first version said "eight" and pinned only the testbench
    flavour in both positions: deleting the three `let`-RHS guards left
    the whole suite green, so three of the nine sites were carrying a
    fix nothing measured. Counting the branches is not the same as
    reaching them, which is the same lesson as "N arms is not N
    measurements" one level down.

86. **The built-in component predicates are a DEFAULT, and TB-IR
    treated them as reserved (2026-08-18).**

    v1's `resolve_component_idle_predicate` and
    `resolve_component_quiesced_predicate` both return `None` when the
    receiver's component declares a `hookable` of that name, deferring
    to the user's method — its comment names the shipped `buf_mgr_test`
    fixture (a `hookable idle(n)` that holds bus valids low) as the
    reason. TB-IR's `as_component_idle` had no such guard, and it runs
    BEFORE component-method resolution, so the heartbeat won every
    time.

    Measured on an `agent Calc` declaring `hookable idle(n: uint<8>) ->
    uint<32>` that returns 7, against `assert c.idle(2) == 7`:

    | backend | emitted | outcome |
    |---|---|---|
    | v1 | `if (!(Calc_idle(c, 2) == 7))` | the method runs, assertion holds |
    | TB-IR (before) | `if (!((((cycle_count - c._last_in_cycle) >= 2) && ((cycle_count - c._last_out_cycle) >= 2)) == 7))` | the heartbeat runs, assertion fails |

    Both compile, both run, and they disagree with no diagnostic — the
    worst shape a divergence takes, which is why this is a fix and not
    a classification. All four names behave this way (`idle`,
    `idle_in`, `idle_out`, `quiesced`).

    With the guard, a declared name is an ordinary component-method
    call and takes that path's pre-existing gap — value-returning
    component methods do not lower in expression position for ANY name,
    which the test pins by asserting the refusal is byte-identical to
    the one an ordinary method gets. That gap is real and separate;
    what is fixed here is TB-IR quietly answering a different question.

    Found by review, not by the sweep: the previous commit added a note
    that `is_builtin_component_predicate` "has to grow when
    `as_component_idle` grows" and never checked the other direction,
    which is where the live bug was.

87. **One coverpoint-constant helper, four roles, four different v1
    behaviours (2026-08-18).**

    `cover_const_u64` folded `Vec` lane indices, bit-slice bounds,
    `.trunc<N>()`-family widths and `as uint<N>` cast widths, and gave
    all four the same refusal — including the same TEXT, so
    `.trunc<N>()` came out as a "non-constant index/slice bound". Five
    call sites, one message, one verdict, and the verdict was wrong for
    three of them.

    Measured per role by mutating `cov_expr_targets_test` and
    `packed_vec_lane_test`, and compiling v1's emission against a stub
    `VTop`:

    | role | v1 on an unresolvable name | verdict |
    |---|---|---|
    | `Vec` lane index | `dut->lane_id_out[EOF]` — compiles, indexes at -1 | `SilentlyMisLowers` |
    | bit-slice bound | `harc_bits(v, (uint32_t)(EOF), 0)` — compiles, slices at 4294967295 | `SilentlyMisLowers` |
    | width-method width | refuses: "requires a constant integer width" | `Rejects` |
    | cast width | `(uint64_t)(...)` — compiles, width ignored entirely | `SilentlyMisLowers` |

    `EOF` is what makes the first two `SilentlyMisLowers` rather than
    `EmitsUncompilable`. Probing with `N` says "'N' was not declared in
    this scope" and probing with `stderr` says "cast from `FILE*` to
    `uint32_t` loses precision" — both compiler errors, both the wrong
    answer, because v1 pastes the HARC identifier into C++ without
    looking at it. Enumerate the INPUTS, not the shapes: one input that
    also names a macro turns the whole arm over.

    Two things were implemented rather than classified:

    * **Constant EXPRESSIONS fold.** `[1 + 2:0]` is emitted by v1 as
      `harc_bits(v, (uint32_t)(1 + 2), 0)` and means exactly `[3:0]`.
      Routing the helper through `fold_const` — the evaluator `const`
      declarations already use — closes that with no new arithmetic,
      and the test asserts byte-equality with the literal spelling
      rather than a shape.
    * ~~**Widths above 64 clamp.**~~ **Retracted the same day — see
      divergence 91.** The clamp was a value divergence, not an
      identity, and the entry that argued for it is left here because
      the argument is the instructive part: "a coverpoint samples 64
      bits, so widening past it is the identity" is true only of a
      value that is sampled DIRECTLY.

    Sized literals stay a plain subset gap — `[4'd3:0]` is folded
    correctly by v1 to `(uint32_t)(3)`, and TB-IR does not lower sized
    literals anywhere yet, so this is one face of a cross-cutting gap
    rather than something to special-case here.

88. **A bin spec is the same four roles again, and one of its arms was
    promising an escape hatch that does not exist (2026-08-18).**

    `lower_bin_bound` folded a bare literal and a bare `const` name and
    sent everything else to the runtime path, with two hand-written
    `Unsupported` arms in between. Measured by emitting the whole
    testbench under v1 and diffing against the literal spelling — which
    is how the comparison line was found rather than guessed; a filter
    for "the bin's name" matched the counter DECLARATION (`uint64_t
    zero = 0;`), identical across every case, and would have said
    nothing at all:

    | spec | v1 emits | outcome |
    |---|---|---|
    | `1 - 1` | `_v == 1 - 1` | correct |
    | `Z` (a `const`) | folded | correct |
    | `dut.en` | `_v == harc_rt::harc_read(dut->en)` | correct, per-sample |
    | `N` (undeclared) | `_v == N` | "'N' was not declared in this scope" |
    | `EOF` | `_v == EOF` | **compiles**; the bin can never match |
    | `4'd0` | folded to `_v == 0` | correct |
    | `99999999999999999999999` | verbatim | compiles with a warning, truncates |

    The unresolvable-name arm said `Unsupported` — "re-run with
    `--codegen v1`" — and v1 cannot build it. The test defending that
    arm asserted "the escape hatches the message names all work", which
    was true of the message's ADVICE and said nothing about the
    suggestion attached to it. `SilentlyMisLowers` now, `EOF` being the
    input that sets it, exactly as for a slice bound.

    Constant expressions fold here too, so `{1 - 1}` becomes a `Const`
    bin rather than a per-sample comparison, matching the literal
    spelling byte for byte. The hook-parameter precedence survives it:
    a hook param beats a file-scope `const` of the same name, so the
    fold is skipped whenever one appears anywhere in the bound, and the
    ~~test pins that by adding an unrelated `const ticks = 99` to a
    fixture whose hook parameter is named `ticks`~~ — **wrong, see
    divergence 92.** That fixture's hook PARAMETER is `cmd`; `ticks` is
    a field of the record it carries and never appears as a bare
    identifier in a bound, so the assertion passed under every mutation
    including deleting the guard. It compares a real hook-parameter bin
    now.

    The `parse_bound` literal arm is a SEPARATE landing, reached before
    `fold_const` is consulted, so it took its own probe rather than the
    other arm's verdict — same split, independently measured.

89. **`Invalid` on an uninstantiated covergroup, which is the third
    time this rule has been broken (2026-08-18).**

    The two reachable `lower_hook_call` shape arms — a trigger with no
    receiver (`@(step(n) post)`) and one whose receiver is not a path
    (`@((drv.x + 1).step(n) post)`) — were `Invalid` on the strength of
    v1 refusing them with "covergroup `StepCov` hook trigger must
    resolve to a `hookable` on a known component type".

    That refusal lives in `emit_covergroup_hook_sample_registration`,
    which runs at the INSTANTIATION site — once per `cov : StepCov`
    field or `let cov : G`. A covergroup declared and never
    instantiated never reaches it. TB-IR refuses at DECLARATION.
    Measured on `covergroup_hook_trigger_test` with the `cov` field and
    its readers removed:

    | | `cov : StepCov` present | uninstantiated |
    |---|---|---|
    | v1 | refuses | emits 298 lines, g++ `-fsyntax-only` clean |
    | TB-IR | refuses | refuses |

    `Unsupported` now. The rule is unchanged and was simply not applied:
    `Invalid` means no backend runs it in ANY reachable configuration,
    and "nothing instantiates it" is a configuration. Same shape as the
    `connect` sinks and the built-in predicates before it — three
    times, in three different files, each time because the probe used
    the fixture as shipped rather than the fixture with the instance
    taken out.

90. **The built-in predicates on a TRANSACTOR receiver: twelve
    landings carrying a false `Invalid` (2026-08-18).**

    Divergence 86 fixed the declared-vs-built-in interaction on the
    component path and restated, in
    `is_builtin_component_predicate`'s doc, that the list has to track
    the resolvers "or a working construct starts being reported as a
    program error". A working construct was being reported as a program
    error one file over: `as_transactor_method_call` had no carve-out
    at all.

    v1 resolves the predicates on a transactor receiver too —
    `resolve_component_idle_predicate` walks `self.transactors` through
    `synth_component_from_transactor`, and both backends stamp
    `_last_in_cycle`/`_last_out_cycle` on transactor state structs.
    Measured on a `transactor Drv` with a `when active` hookable and a
    testbench field `d : Drv active`, compiling the whole emitted
    testbench:

    | source | v1 emits | g++ |
    |---|---|---|
    | `assert d.idle(2)` | `if (!(((cycle_count - _tb.d._last_in_cycle) >= 2) && …))` | compiles |
    | `d.idle(2)` | the same expression, discarded | compiles |
    | `let v = d.idle(2)` | `auto v = …` | compiles |
    | `d.nosuch(2)` | `_tb.d.nosuch(2)` | "'struct Drv' has no member named 'nosuch'" |

    Four names × three positions = twelve landings, `Unsupported` now;
    `nosuch` keeps its `Invalid`, which is what the surrounding arm was
    written for.

    Not closed, only classified honestly: `Expr::ComponentIdle` takes a
    `ComponentBase`, which names a component instance and has no
    transactor-field spelling. The runtime state is already there on
    both sides, so closing it is a matter of giving the predicate a
    transactor receiver rather than of adding anything to the emitted
    struct.

    Also corrected in the same doc: the claim that
    `user_override_wins` consults this list. It does not — it asks
    `ComponentSchema::method`, and the resolvers match their names
    inline, so a fifth predicate has to be added in two places, not
    one. A maintainer following the old sentence would have added a
    name here and found it had no override behaviour.

91. **The clamp was a value divergence, and the argument for it was
    an unexamined "identity" (2026-08-18).**

    Divergence 87 clamped a coverpoint width above 64 down to 64,
    reasoning that the sample is 64 bits wide so widening past it
    cannot matter. It matters as soon as the widened value is SLICED
    before it is sampled:

    ```
    cover dut.count_out[3:0].sext<128>()[70:65]
    ```

    | | emitted sampler | at `count_out = 15` |
    |---|---|---|
    | v1 | `harc_bits(harc_sext_u128(harc_bits(v,3,0), 4, 128), 70, 65)` | 63 |
    | TB-IR, clamped | `(((uint64_t)(nibble sign-extended to 64)) >> 65) & 0x3F` | 0 |

    Both backends build. g++ warns "right shift count >= width of
    type" on the TB-IR side; HARC says nothing. With `[100:70]` the
    numbers are 2147483647 and 0. Before the clamp the program did not
    lower at all, and `Unsupported` was accurate — v1 compiles it and
    samples the right value. The clamp turned a correct
    "re-run with `--codegen v1`" into a silently wrong sample, which is
    the single outcome this sweep exists to prevent.

    Two more followed from it, both gone with the revert:

    * `.trunc<W>()` with W > 64 was `Rejects`, and
      `(dut.count_out as uint<128>).trunc<100>()` is something v1
      compiles and runs — the clamp had made TB-IR believe the source
      was 64 bits wide while v1 correctly saw 128. The arm now splits
      on the source width it is actually given: a truncation naming
      at least as many bits as its source is `Invalid` (v1's own rule,
      measured on the 4-bit nibble), and anything else keeps the
      suggestion. Computing the source width BEFORE the width argument
      is what makes that check trustworthy.
    * `(dut.count_out as uint<128>).zext<100>()` LOWERED, because the
      clamped source width was 64 and so the narrowing-`zext`
      direction check never fired. v1 calls it a program error in
      plain words. TB-IR was accepting a program the language rejects.

    ~~What the refusals split on now is where v1 stops working:
    65..=128 is `Unsupported`, above 128 is `EmitsUncompilable`~~ —
    **the 128 split was measured with the wrong compiler flag and is
    retracted; see divergence 96.** Every width above 64 is
    `Unsupported`: v1 builds all of them under the `-std=gnu++20` the
    product uses.

    The lesson is narrower than "measure v1", which had been done —
    every direct-sample form really is byte-identical between the two
    backends, and the test asserted exactly that. What was missing is
    that an equality proved for one CONTEXT was used to justify a
    change that alters the value in every other context. A fold is
    safe when it preserves the value; a clamp discards information,
    and information nobody is reading yet is still information.

92. **Three smaller holes the same fold opened (2026-08-18).**

    * **Hook-parameter precedence was guarded in bins only.** A hook
      parameter beats a file-scope `const` of the same name, and
      `mentions_hook_param` enforced that for bin specs while the four
      constant ROLES never consulted `hook_params` at all. With
      `hookable run_for(cmd, k)` and an unrelated `const k = 7`:
      v1 emits `harc_bits(cmd.ticks, (uint32_t)(k), 0)` — the argument
      — and TB-IR emitted a fixed `[7:0]`. Both compile, both run,
      different bits, no diagnostic. The bare-`k` spelling predates the
      fold; `k + 0` is one the fold newly reached, so the commit
      widened the hole in the same breath as documenting the guard.
      `hook_params` is threaded through all five helpers now and a
      hook-parameter bound refuses, naming the parameter.
    * **A negative fold became `u64::MAX`.** `[0 - 1]` folded and TB-IR
      emitted `dut->lane_id_out[18446744073709551615]` with a clean
      `verify_program`; before the fold it was not a constant
      expression and the user got an error. `ConstVal::signed` was
      threaded through and never read — the bit pattern alone cannot
      tell -1 from `u64::MAX`. Now it is read, and a negative bound is
      `Invalid` at every role. (An out-of-range POSITIVE lane index is
      a different matter: `dut.lane_id_out[9]` on a four-lane port
      emits identically under both backends, so that hole is shared and
      pre-existing, and the lane count is not available at this layer.)
    * **The unfoldable-literal verdict flipped on operand order.**
      Returning the first pre-order hit meant
      `[4'd3 + 999…:0]` promised `--codegen v1` while
      `[999… + 4'd3:0]` refused to, for a program v1 truncates either
      way. The worse of the two kinds wins now, which is the same
      worst-thing-under-the-arm rule one level down.

    And two claims that were not what they looked like:

    * The precedence test asserted `const ticks = 99` against a
      fixture whose hook PARAMETER is `cmd` — `ticks` is a field of
      the record it carries and never appears as a bare identifier in
      a bound. It passed under every mutation of the guard, including
      deleting it. It compares a real hook-parameter bin now.
    * `walk_expr`'s recursion had no coverage at all: stubbing it to
      "visit the top node only" left 482 tests green, while
      `[1 + EOF:0]` silently fell through to the wrong arm and
      `{k + 0}` lost hook-parameter precedence. Both are pinned.

93. **Retracting a bad verdict is not the same as reaching a good one
    (2026-08-18).**

    Divergence 91 retracted the width clamp. Review round three found
    that the retraction had converted a silent acceptance into a FALSE
    ESCAPE HATCH rather than into an honest verdict — the same defect
    class, one step along.

    `067d632` threaded a source width into `cover_width_arg` and then
    consulted it only inside `if method == "trunc"`. Everything else
    fell through to `Unsupported`:

    | coverpoint | TB-IR said | v1 |
    |---|---|---|
    | `dut.count_out[100:0].zext<70>()` | re-run with `--codegen v1` | error: "width must be ≥ the source width" |
    | `dut.count_out[100:0].sext<70>()` | same | same |
    | `(dut.count_out as uint<128>).zext<100>()` | same | same |

    The third row is the exact input the retraction's own commit
    message names as the bug it was fixing. The direction check lives
    in `cover_width_arg` now and runs for every width, phrased in v1's
    own words so a user who re-runs reads the same sentence twice.

    A second, quieter one: `cover_infer_expr_width` passed `None` for
    the receiver width of a NESTED width method, with a comment saying
    the receiver "is not yet known at this call". It is —it is the
    callee's own target. So `[3:0].trunc<128>()` was `Invalid` alone
    and `Unsupported` the moment anything wrapped it, for the same
    inner program v1 refuses either way. A slice or a cast wrapper kept
    the right answer; only the width-method path lost it. Comments that
    assert an absence are worth checking: this one was wrong and it was
    load-bearing.

    Making the cast width available to that check needed a policy
    split. `cover_cast_width` refuses a `>64` width where the cast is
    LOWERED, which is right — TB-IR cannot model the value — but the
    direction check needs the same width as a NUMBER, and refusing
    there hid the more accurate error behind a less accurate one.

94. **Two guards written to answer a review, neither measured
    (2026-08-18).**

    Third occurrence on this branch, and this time in the commits
    written to answer the first two:

    * the hook-parameter guard on the four constant roles — the
      headline of divergence 92 — survived `.filter(|_| false)` with
      482 tests green;
    * the negative-fold rejection survived `&& false` likewise.

    The pattern is stable enough to name: a guard added in response to
    review gets the care that went into finding it and none of the care
    that goes into pinning it, because the finding feels like the work.
    Both are pinned now, along with three expression-position queue
    landings and the transactor built-in carve-out, all of which
    "measured at all N landings" covered at the CLI and no test covered
    at all.

    The negative-fold verdict was also wrong, and wrong in a way the
    document already contained the answer to. `EOF` is `(-1)` on glibc,
    so `dut.lane_id_out[0 - 1]` and `dut.lane_id_out[EOF]` are the same
    C++ after preprocessing; v1 emits both and both index at -1. The
    role table two entries earlier calls the `EOF` form
    `SilentlyMisLowers` precisely because v1 compiles it. Calling the
    arithmetic spelling `Invalid` contradicted that at a distance of
    twenty lines.

    Same for the hook-parameter guard, which returned a flat
    `Unsupported` before `role` was consulted, bypassing the four-way
    split built for exactly this question: `cover cmd.ticks.trunc<k>()`
    is something v1 refuses outright, and `(cmd.ticks as uint<k>)` is
    one it accepts while dropping the width. Both now take their role's
    verdict, from a single `ConstRole::v1_on_unfoldable` the refusal
    paths share, because keeping them in step by hand did not work.

95. **A rule invented instead of looked up, and a leak reopened one
    arm over (2026-08-18).**

    Round four, on the commit whose subject was round three. Three
    blocking findings.

    **`resize` is direction-agnostic, and I made it an error.** The
    direction check moved into `cover_width_arg` with the arm written
    as `"zext" | "sext" | "resize" => width < sw`. So
    `dut.count_out[7:0].resize<4>()` became `Invalid` — "a program
    error under every backend in every reachable configuration" — for a
    construct both backends had been compiling to identical C++
    (`((count_out >> 0) & 0xFF) & 0xF` under TB-IR, the same value
    under v1, g++ clean).

    Three places already said so, and none was read before the rule was
    written: the spec ("`.resize<N>()` remains direction-agnostic"),
    v1's own check (`"zext" | "sext" if width < sw`), and TB-IR's
    general expression lowering, which excludes it for the same reason.
    The covergroup path is a THIRD implementation of a rule stated
    twice already; writing it from intuition rather than copying it is
    what put a method in the set that does not belong there.

    **The `>128` verdict leaked.** `CastWidthPolicy::Report` was
    introduced so the direction check could see a wide cast's width
    without the cast refusing first. It also suppressed the `>128`
    refusal, and `cover_width_arg` had no split at 128 to carry it —
    so `(dut.count_out as uint<300>).trunc<200>()` came out
    `Unsupported` where v1 gives "no matching function for call to
    `harc_rt::HarcWide<10>::HarcWide(__int128 unsigned)`". Exactly the
    false escape hatch the policy split's own doc-comment describes,
    reappearing one arm along.

    Fixing it took a second measurement, because the obvious condition
    was wrong too: `max(width, src_width) > 128` reports
    `[3:0].zext<200>()` as uncompilable, and it is not — the
    `harc_wide_*` helpers take a narrow argument and g++ accepts them.
    The `HarcWide` constructor failure belongs to the CAST, so the
    condition is the SOURCE width alone.

    **And none of the three claims was pinned.** 485 tests stayed green
    under a mutation reverting each: the direction check back below the
    `>64` refusal, the nested receiver width back to `None`, and
    `Report` back to `Refuse`. Fourth consecutive round of this
    finding, on the commit whose subject is that finding — the previous
    entry named the pattern and then repeated it in the same breath.

    Naming it again is clearly not enough, so: the rule now is that a
    guard and its mutation test are one edit. Not "add the guard, then
    add a test" — the mutation is how you find out the guard is
    load-bearing at all, and running it after the fact is what keeps
    getting skipped.

    Two smaller ones from the same round. The negative-fold test
    asserted its MESSAGE and not its verdict, so replacing the arm with
    the `Invalid` the previous entry spends four paragraphs rejecting
    left it green; it asserts the verdict now, at all three roles
    rather than one. And `v1_on_unfoldable`'s doc said `None` meant
    "`--codegen v1` is a real way out", which is true at the
    hook-parameter site and false at the negative-fold one — the two
    callers ask different questions and their answers coincide for
    unrelated reasons, which the comment now records as measured rather
    than structural.

96. **Four rounds of machinery on one mis-set compiler flag
    (2026-08-18).**

    Round five enumerated the width/cast space mechanically — four
    methods × seven receiver widths × eight widths — instead of by
    example, and the first thing that fell out was that every
    `EmitsUncompilable` verdict in this file was measured wrongly.

    `harc_rt::HarcWide<N>`'s converting constructor is gated on
    `std::is_integral_v<T>`. libstdc++ reports that FALSE for
    `__int128` under `-std=c++20` and TRUE under `-std=gnu++20`, and
    `src/main.rs` builds the emitted testbench with
    `CFG_CXXFLAGS_STD=-std=gnu++20`. Every probe here used
    `-std=c++20`. So `(dut.count_out as uint<300>).trunc<200>()` — the
    flagship case for the whole `>128` family — compiles fine under the
    flags the product uses, and the honest verdict is `Unsupported`,
    which is what the code said before any of this work.

    `tests/wide_cast_cpp.rs` already carried a comment naming the right
    standard. Same failure as `resize`: a fact written down in the
    repo, re-derived wrongly from a local experiment. The probe-method
    section now states the flag, because nothing else in this document
    did.

    So the fifth commit is subtractive. Gone: the `>128`
    `EmitsUncompilable` arms in `cover_width_arg` and
    `cover_cast_width`, the `src_width > 128` condition, and
    `CastWidthPolicy` — which existed only to keep that verdict
    reachable, and whose `Report` mode had in the meantime introduced
    its own regression (a slice-derived source width above 128 was
    reported as a problem with "the receiver's own cast" on programs
    containing no cast).

    Three real holes the enumeration found alongside it:

    * **The 1024-bit language limit was missing.** v1 checks it, TB-IR's
      general expression path checks it, and the spec states it in the
      same sentence this file quotes for `resize` — "a positive
      constant integer literal in `1..=1024`". The covergroup copy
      checked `0` and stopped, so `.zext<2000>()` was told to re-run
      under a v1 that refuses it.
    * **The direction check reached spellings v1's never does.** v1
      infers a receiver width with `eval_const_width` (literal only), as
      does TB-IR's general path with `const_eval_width`; the covergroup
      copy inferred through the folding `cover_const_u32`. So
      `const HI = 100` + `[HI:0].zext<70>()` was `Invalid` while the
      identical program compiled and ran under v1 — the verdict decided
      by whether the bound was written with a name. Inference is
      literal-only now; the bound itself still folds.
    * **The width argument accepted non-literals.** `.zext<1 + 7>()`
      lowered while v1 refused it. The spec says literal.

    The pattern across all three: this file is a THIRD implementation
    of rules that `cpp_tb.rs` and `exprs.rs` already state, and every
    place it was written from intuition rather than copied is a place
    it drifted. That is the argument for the covergroup path consulting
    the same helpers rather than paraphrasing them, which is the next
    piece of work here and is deliberately not being done in a fifth
    consecutive fix commit.

    On whether to revert the line of work: measured against v1 across
    224 cells, the state before it had 72 wrong verdicts and the state
    after has none. It converged; the residue was one wrong idea and
    two missing checks, all localized. Reverting would have traded a
    small, identified residue for a larger diffuse one.

97. **Seven `records.rs` arms — and the first two passes at them were
    both wrong, in opposite directions (2026-08-18, corrected
    2026-08-18).**

    ~~Every one said `Unsupported` and six were false escape hatches.~~
    Struck: two of those six reclassifications were themselves wrong,
    and the leaf table below was wrong in both directions at once. The
    corrected reading:

    | construct | v1 does | verdict |
    |---|---|---|
    | `when` subtype in a transaction | emits `if (q.op == 1) { … q.addr = _val_addr; }` — the guard is honoured | a real escape hatch |
    | `when` subtype in a struct | a struct byte-identical to the same struct without the block — the field is gone | `SilentlyMisLowers` |
    | `keep` in a struct | `_s.add(z3::ult(_z_a, _ctx.bv_val((uint64_t)10, 64)))` — it reaches the solver | a real escape hatch |
    | `default` on a nested-record field | `Inner i = 0;` — "could not convert '0' from 'int' to 'Inner'" | `EmitsUncompilable` |
    | `default` on a `Vec` field | `std::array<T, N> v = 0;` — same conversion error | `EmitsUncompilable` |
    | `default 4'd3`, `8'hFF`, `4'b1010` | folds to the same value | a real escape hatch |
    | `default 128'hFF…`, `0xFF…` | folds to a `_harc_u128` composite the 64-bit member truncates (`-Woverflow`) | `SilentlyMisLowers` |
    | `default 999…` | pasted verbatim; "integer constant is too large for its type", truncates | `SilentlyMisLowers` |

    **Both wrong verdicts were measured on a program that never
    randomizes the record.** The `when` probe's run body was `wait 1
    cycle`, so there was no solve site for a guard to appear in; what it
    read instead — an unconditional `static void randomize_Req(Req*)` —
    is only ever called from an OUTER record's randomize. The struct
    `keep` probe checked for a randomize METADATA entry, which a struct
    does not get, and concluded the constraint never reaches the solver;
    the solver lambda emits it directly. `tests/fixtures/axi_agent.harc`
    is a `when` subtype with `keep`s, and spec §714 / §2787 describe
    when-subtypes as shipped, both of which would have contradicted the
    verdict before any probe ran.

    The sized-literal split was right about the outcomes and wrong about
    the line: it tested `lit.contains('\'')`, but the width prefix is
    not the value. `4'd3` folds to a correct `3` and
    `128'hFFFFFFFFFFFFFFFFFFFF` folds to a `_harc_u128` composite the
    64-bit member truncates. The guard now normalizes through
    `cpp_tb::normalized_int_literal` — the same rewrite v1 folds with —
    and asks whether the result fits.

    The non-scalar-leaf arm is the one worth the space. It is a single
    arm serving a dozen shapes that take THREE verdicts:

    | field type | v1 emits | |
    |---|---|---|
    | `uint<65>` … `uint<256>` | `_harc_u128` / `harc_rt::HarcWide<n>`, with a matching pack, unpack and draw | real |
    | `list<uint<8>>`, `list<sint<8>>`, `list<uint<256>>`, `list<bool>` | `std::vector<T>` + resize + per-element draw | real |
    | `Vec<uint, 4>`, `Vec<uint<128>, 4>`, `Vec<Vec<uint<8>, 2>, 4>` | the nested `std::array` | real |
    | `list<Vec<uint<8>, 2>>` | `std::vector<std::array<uint64_t, 2>>` and then `[_i] = 0` | **does not compile** |
    | `list<Inner>`, `list<string>` | `std::vector<uint64_t>`, and randomize writes `// data : list (named, not yet supported)` | silent |
    | `list<queue<uint<8>>>`, `list<event<T>>`, `list<int>`, `list<Vec<uint<8>, N>>` | `std::vector<uint64_t>` + `[_i] = 0` | silent |
    | `Vec<queue<uint<8>>, 4>` | `std::array<uint64_t, 4>` — the array survives, the element does not | silent |
    | `Vec<string, 4>`, `Vec<uint<8>, N>` | `uint64_t data = 0;` — the whole array collapses | silent |
    | `queue` / `string` / `event` / `object` | a bare scalar | silent |

    Three versions of this arm got it wrong. A single `queue` probe
    generalised to everything denied `list` its working hatch and broke
    `the_escape_hatch_phrases_the_parity_gate_greps_are_stable`, whose
    fixture is a `list<uint<8>>`. Then a hand-written copy of v1's type
    rules called a 128-bit field a flattening — v1 gives it a correct
    `_harc_u128` — while calling `list<Inner>` a working hatch, and it
    was self-contradictory inside one file: the same `uint<128>` counted
    as modelling when it was a `Vec` element and as flattening when it
    was the leaf, thirty lines apart. Neither noticed the third outcome
    at all.

    So the rules are not restated in the lowering any more.
    `cpp_tb::record_leaf_fate` sits next to `txn_field_c_type` and
    `emit_field_random` — the two functions that actually choose the
    member and the draw — recurses through the same `list_elem_type` /
    `fixed_vec_type_args` helpers, and returns
    `Models` / `Flattens` / `Uncompilable`. `emit_field_random`'s
    per-element draw was extracted into `list_elem_random_expr` so both
    the emitter and the predicate read one copy of it; v1's output over
    the whole fixture corpus is byte-identical across that refactor (190 fixtures parse; 160 emit).

    The two consumers map the same fate differently, and that difference
    is measured too: a scoreboard emits no `randomize_*` body, so the
    leaf whose randomize body is what stops compiling keeps a correct
    member there. `list<Vec<uint<8>, 2>>` is `EmitsUncompilable` as a
    transaction field and a working escape hatch as a scoreboard field.

    ~~A zero-width leaf (`uint<0>`, `Vec<uint<0>, 4>`) is `Invalid`~~ —
    struck, see divergence 107. The panic it rested on is a debug-build
    artifact; a release build, which is how `harc` ships, emits and runs
    the program.
    `Vec<uint<8>, 0>` is a zero-LENGTH array, a different thing, and
    stays a suggestion v1 honours (`std::array<uint64_t, 0>`).

    An unresolved type name (`data : Unknown`) stays `SilentlyMisLowers`
    rather than becoming `Invalid`: v1 emits `int64_t data = 0;` and
    runs it, and `Invalid` on this branch means no backend runs it in
    any configuration.

98. **Four `helpers.rs` arms, and the routing gate decided which
    probe even reached them (2026-08-18).**

    | arm | v1 | verdict |
    |---|---|---|
    | a DUT/sync-touching helper call in a message | compiles; calls it AT the failure site | `Unsupported` — correct already |
    | a testbench method call in a message | compiles; same | `Unsupported` — correct already |
    | a helper param of module type, non-DUT argument | "no match for call to `<lambda(VTop*)>` (Model&)" | `EmitsUncompilable` |
    | a testbench method param of module type, non-DUT argument | "no match for call to `<lambda(Tb&, VTop*)>` (Tb&, Model&)" | `EmitsUncompilable` |

    The first two are a negative result, and recording them is the
    point: two of four arms on this branch turning out to be right is
    only worth anything if the measurement happened.

    Getting to them took three failed probes. A `log(...)` message
    HOISTS a CFG-inlined call ahead of the statement and lowers fine
    (#494 P2d), so probing with a `log` measures nothing — the arms
    fire only for a CONDITIONALLY-evaluated message, an assert's `else
    fail(...)`, where hoisting would run the inlined body even when the
    message never fires. The gate is `lower_fmt` vs
    `lower_fmt_hoisting`, one call level up, and no amount of reading
    the arm would have shown that. Adding a `wait` to the helper to
    make it "sync-touching" was the wrong lever twice before the gate
    was read.

    The module-param pair is the familiar shape: v1 types the emitted
    lambda on the MODULE (`[&](VTop* d)`) and passes through whatever
    was written, so the DUT spelling compiles clean and the component
    spelling does not. Both measured with `-std=gnu++20`, and the
    testbench-method sibling probed on its own rather than inferred
    from the helper — they agree, which is a result, not an assumption.

99. **The width rules now have one statement per backend, which is the
    minimum (2026-08-18).**

    Round five's root-cause finding, acted on rather than patched
    around: the covergroup path was a THIRD implementation of rules
    that `cpp_tb.rs` (v1's) and `exprs.rs` (TB-IR's general path)
    already stated, and it drifted from both on every one of them —
    `resize` wrongly in the direction set, the 1024-bit language limit
    missing entirely, the receiver width inferred through a constant
    fold where v1 uses literals only. Three separate review findings,
    one cause.

    `exprs::width_method_violation` is now the single TB-IR statement of
    zero-width, the language limit, and the direction check (with
    `resize`'s exemption written once, beside the rule it is exempt
    from). Both the general path and the covergroup path call it; only
    the diagnostic PREFIX is per-caller, so the sentences stay
    identical and there is nothing left to drift.

    Two statements remain — one per backend — and that is irreducible:
    v1 has its own copy because it is a different compiler. What is
    gone is the third.

    The check that this is real rather than cosmetic is a mutation on
    the SHARED function: breaking the `trunc` direction rule now turns
    five tests red and breaking the 1024-bit limit turns three, across
    both paths. Before, breaking either in one place left the other
    silently disagreeing — which is precisely how `resize` shipped as
    `Invalid` for a construct both backends compile. The 352-cell grid
    stays at 0 disagreements with v1.

100. **Seven `scoreboards.rs` arms, one of them provably dead
     (2026-08-18).**

     | construct | v1 emits | verdict |
     |---|---|---|
     | `bound to` on the scoreboard | output BYTE-IDENTICAL to the unbound one | `SilentlyMisLowers` |
     | `bound to` on a field | byte-identical likewise | `SilentlyMisLowers` |
     | a directional (port) field | `uint64_t p;` — uninitialized, direction dropped | `SilentlyMisLowers` |
     | a `default` on a queue field | `HarcQueue<uint64_t> q = 0;` — no such constructor | `EmitsUncompilable` |
     | `list<uint<8>>` field | `std::vector<uint64_t> l;` | a real escape hatch |
     | `string` / `event<T>` field | `int64_t s;` / `uint64_t e;` — uninitialized | `SilentlyMisLowers` |
     | a method on the scoreboard | — | UNREACHABLE |

     The `bound to` rows show why "v1 emits" is never the measurement.
     It emits for both, and diffing against an unbound control is what
     shows the clause left no trace whatsoever — the binding silently
     does not happen.

     The method arm is dead code, and provably: `lower_program` routes
     a scoreboard to the composite-component table when
     `components::scoreboard_is_component` holds, and that predicate is
     `any(ComponentItem::Hookable(_))` — the exact condition of the arm.
     Replacing its body with `unreachable!()` leaves the whole suite
     green. Its comment described an intent the routing gate had since
     made moot, which is a thing to look for: an arm whose justification
     is written in the past tense of a design that moved.

     The field-type arm asks the same flatten question as the record
     one and now asks it through the SAME predicate —
     `cpp_tb::record_leaf_fate` — rather than a second copy. The
     supported SETS differ (a `queue<T>` is a legal scoreboard field and
     not a legal record leaf) but the rule about what v1 does with the
     rest does not, and this file has already paid twice for
     paraphrasing shared rules.

### The probe method

Every classification above came from the same mechanical check rather
than from reading v1's source: emit the construct under both backends
with `harc sim --emit-only`, and when v1 emits, READ the generated C++.
"v1 emits" is not the same as "v1 works" — of the ten constructs the
first sweep flagged as gaps, five turned out to be v1 emitting code that
does not compile or silently means something else. Only the ones where
v1's output is genuinely usable are worth mirroring; the rest want an
honest `NotImplemented` diagnostic instead.

**Compile with `-std=gnu++20`, which is what `src/main.rs` passes to
the emitted testbench (`CFG_CXXFLAGS_STD`).** This is not a detail. An
`EmitsUncompilable` family spanning two divergence entries and four
commits rested on g++ rejecting
`harc_rt::HarcWide<N>::HarcWide(__int128 unsigned)` — a rejection that
exists only under `-std=c++20`, where libstdc++ reports
`is_integral_v<__int128>` false. Under the flags the product actually
uses, v1 compiles every one of those programs, and the verdict should
have been `Unsupported` throughout. `tests/wide_cast_cpp.rs` already
carried a comment saying which standard to match; the probe did not
read it. "It does not compile" is a claim about a specific compiler
invocation, and the invocation has to be the real one.

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

101. **Six `on <event>(arg)` arms in `components.rs`, five of them
     provably dead (2026-08-18).**

     `event_subscription` is the predicate that ROUTES an `on` handler
     to the subscription path, and it already establishes three facts:
     the trigger is a `Call`, its callee is a bare identifier, and that
     identifier names an `event` field — which is FOUR ways to fail,
     because the last one splits into "no such field" and "a field of
     another kind". The resolver it routed into re-derived all of them
     and carried a rejection arm for each, plus one for `h.periodic` —
     which the item split at the top of the function has already sent to
     `periodic_asts`.

     All five are unreachable. Replacing each with `unreachable!()`
     leaves the whole suite green, and each of the five shapes lands on
     a different diagnostic entirely:

     | trigger | where it lands |
     |---|---|
     | `on 3 cycles` | lowers — a periodic handler |
     | `on clk` | the unresolved-name arm |
     | `on tagger.in_ev(t)` | "transactor/method call `.in_ev(...)`" |
     | `on other(t)` (a scalar field) | "helper call `other(...)`" |
     | `on nosuch(t)` | "helper call `nosuch(...)`" |

     So the resolution moved into the routing predicate, which now
     returns what it found rather than a bool. This is the same shape of
     defect as divergence 97's leaf table and divergence 100's dead
     scoreboard arm: a fact established in one place, restated in
     another, and the restatement kept for shapes the first place had
     already excluded.

     The two arms that survive split on measurement, and they had been
     one arm:

     | trigger | v1 emits | verdict |
     |---|---|---|
     | `on in_ev()` | `[&](uint64_t _v) { … }` — a synthesized name for a payload the body cannot reference anyway | a real escape hatch |
     | `on in_ev(t, u)` | `[&](uint64_t t) { … }` — the extra parameter is dropped without a word | `SilentlyMisLowers` |

     The multi-argument half was first labelled `EmitsUncompilable`, on
     a body whose `u` had nothing else to resolve to. That is the LESSER
     of the two things v1 does here. Give `u` something to bind to and
     v1 compiles clean and runs to a value the source never asked for:

     | the handler's sibling | v1 emits | result |
     |---|---|---|
     | a component field `u : uint<8> default 7` | `tagger.seen = tagger.seen + tagger.u;` | 0 errors, runs |
     | a file-scope `const u = 9` | `tagger.seen = tagger.seen + u;` | 0 errors, runs |

     A body with nothing named `u` in scope does fail to compile, and a
     body that never names it is byte-identical to the one-argument
     form. An arm's status is the WORST of what v1 does under it, so it
     is `SilentlyMisLowers`; `validate_cycle_handler`, a hundred lines
     above, is the same two-shape arm and had already resolved it that
     way. Reconstructing the rule instead of copying the neighbour that
     states it is the same mistake as divergence 97's leaf table.


102. **The unbound-transactor item walk was written twice, and none of
     its arms had been measured (2026-08-18).**

     `lower_transactor`'s always-on walk and its `when active` walk were
     two copies of the same 120-line match. They differed in exactly one
     expression — `methods_ast.push((h, false))` versus `(h, true)` —
     and in comment text. Five of the ten rejections in them were the
     same rejection written twice, so a change to one position and not
     the other would have silently diverged by where the user wrote the
     item.

     They are now one walk over
     `t.items.chain(t.when_active.iter().flatten())` carrying the flag,
     which is also what v1's `synth_component_from_transactor` does with
     `include_active = true`.

     Two of the arms are dead:

     * every `on` handler — subscription, cycle-trigger, or periodic —
       routes an unbound transactor to the COMPONENT path
       (`transactor_is_component` returns true for `has_on_handler ||
       has_periodic_handler`), so it never reaches this walk;
     * a lifecycle block is refused by the PARSER inside a transactor
       ("lifecycle blocks are currently supported only inside
       `test`/`impl` and `testbench`"), so only the `apply` half of the
       lifecycle/apply arm is live.

     Both confirmed with `unreachable!()` against the whole suite.

     The four live arms all said "re-run with `--codegen v1`", and one
     of them was right:

     | item | v1 emits | verdict |
     |---|---|---|
     | `req : in event<uint<8>>` | `std::vector<std::function<void(uint64_t)>> req;`, and a real fan-out at the emit site: `for (auto& _s : _tb.drv.req) _s(1);` | a real escape hatch |
     | `p : in uint<8>` / `out uint<8>` | `uint64_t p;` — the direction is dropped; uninitialized unless the field also carries a `default` | `SilentlyMisLowers` |
     | `dut : Top default <lit>` | `VTop* dut = <lit>;` | `EmitsUncompilable` |
     | a second module-typed field | `_tb.drv.dut = dut; _tb.drv.other = dut;` — both bound, both driven | a real escape hatch (corrected; see below) |
     | `apply Some.Policy` | nothing; byte-identical output (offsets normalized) | `SilentlyMisLowers` |

     The directional arm covered an event field and a scalar field under
     one message, and they are opposite verdicts: v1 gives the event a
     real subscriber vector and the scalar an uninitialized member. The
     split now asks `components::is_event_field`, the routing
     predicate's own helper, rather than a second copy of it.

     The `default` row is the "worst thing v1 does anywhere under the
     arm" rule paying out. `dut : Top default 0` COMPILES — `0` is a
     null pointer constant, and the test binds the handle anyway, so
     that one spelling works. `default 1` is "invalid conversion from
     'int' to 'VTop*'". One working spelling does not make the arm an
     escape hatch.

     **The second-DUT-handle row was wrong, and review caught it.** I
     measured a two-handle program against a control that was equally
     broken. Neither backend auto-binds ANY transactor handle:
     `VTop* dut = nullptr;` is what the SUPPORTED single-handle shape
     emits too, and `<inst>.dut = dut` in the run body is the required
     idiom, which TB-IR accepts. Write both binds and v1 emits

     ```cpp
     _tb.drv.dut = dut;
     _tb.drv.other = dut;
     Drv_step(_tb.drv, 1);
     ```

     and compiles clean with both handles poked. The null dereference
     belonged to the missing bind, not to the second field, so the row
     is `Unsupported`. The assertion that should have caught this
     checked only the STATUS, and a later unrelated arm carried the same
     one — it now checks the message too.

     Two smaller corrections from the same review: the directional-scalar
     detail said "UNINITIALIZED", which `p : in uint<8> default 5` is
     not (`uint64_t p = 5;`), and the `in event<T>` row was measured on
     the declaration alone — the emit-site fan-out it claims is now
     asserted as well. The `in event<T>` claim is measured for scalar
     payloads; a record payload with an impl-level subscriber hits a
     separate `on <path>.<event>(arg)` arm whose v1 output does not
     compile, which is that arm's problem and not this one's.

103. **Four corrections from review, three of them `Invalid` on programs
     v1 runs (2026-08-18).**

     The branch's own rule — `Invalid` means no backend runs it in ANY
     configuration — was broken three ways by one guard and one
     predicate.

     **`zero_width_leaf` recursed through every type argument.** A
     zero-width scalar under a `list`/`queue`/`event` PAYLOAD is not the
     shape this arm is about: it emits `std::vector<uint64_t>` /
     `uint64_t` and compiles clean. Only the type's own width slot and a
     `Vec` element count. Three constructs went from `Invalid` back to
     an honest verdict.

     **It read the width with the wrong parser.** It used
     `parse_int_literal_expr`, which understands `0x`/`0b`; v1's
     `int_width_from_args` and TB-IR's own `field_ir_type` both do a
     plain decimal `parse::<u32>()`. So `uint<0x0>` was `Invalid` while
     v1 compiled it. The width reader is now one function
     (`declared_scalar_width`) that `field_ir_type` also calls.

     That measurement turned up a real gap the old code had papered
     over. v1 cannot read a hex width either, and it does not say so —
     it substitutes a DIFFERENT fallback everywhere it needs one:

     | site | `uint<8>` | `uint<0x8>` |
     |---|---|---|
     | pack | `harc_wide_write_bits(_packed, 0, 8, …)` | `…, 0, 64, …` |
     | unpack | `harc_bits(_packed, 7, 0)` | `harc_bits(_packed, 63, 0)` |
     | randomize | `harc_rng_uint(harc_rng_next, 8)` | `…, 32)` |
     | problem table | `field data u8` | `field data u8` |

     It compiles and runs, so an unreadable width is now
     `SilentlyMisLowers` rather than the escape hatch its member type
     would suggest.

     **`record_leaf_fate` asked the wrong member-picker.** It consulted
     the free `txn_field_c_type`, which maps every `TypeExpr::Named` to
     `int64_t`. The function that actually picks members for scoreboard
     and record fields is the METHOD `Emitter::record_field_c_type`,
     which adds one layer: a named type that IS a declared record gets
     the record's own name. So `scoreboard Sb { l : Inner }` was
     `SilentlyMisLowers` with the detail "v1 emits an UNINITIALIZED
     plain scalar", and v1 emits `Inner l;` and compiles. The predicate
     now takes an `is_record` callback and reproduces that layer.

     Divergence 97's claim that "`txn_field_c_type` is the only caller
     that picks a member type" was simply false.

     **The `when`-subtype arm needed a third measurement.** Round one
     probed a program with no `randomize` and concluded the guard was
     dropped — right label, no evidence. Round two added `randomize(q)`,
     found `if (q.op == 1) { … }` honoured, and flipped the arm to an
     escape hatch. Round three is the shape round two's own comment
     named and did not run: nest the subtype in another record and
     randomize the OUTER one, and v1 reaches it through

     ```cpp
     static void randomize_Req(Req* t) {
         t->op   = harc_rng_uint(harc_rng_next, 8);
         t->addr = harc_rng_uint(harc_rng_next, 16);   // no guard
     }
     ```

     `op == 1` appears zero times in the file and `inner.addr` is absent
     from the solver's problem table. Worst-thing-anywhere makes the arm
     `SilentlyMisLowers`.

     Also corrected from the same review: divergence 100 named the
     shared predicate `records::record_leaf_flattens` (it is
     `cpp_tb::record_leaf_fate`); divergence 99 claimed six and four
     tests redden on the shared width mutations (it is five and three);
     a `scalar_leaf_c_type` doc referenced a function that never
     existed; inserting `zero_width_leaf` had orphaned `field_ir_type`'s
     doc comment onto it; and a stale scoreboard comment sat directly
     above the one that contradicted it.


104. **Five copies of one `= bind` check, and a hole between them
     (2026-08-18).**

     A regblock, an addrmap, an initiator-BFM instance, a bound-to
     event-driven transactor and a target-TLM responder all require a
     bare identifier on the right of `= bind`, and each checked for it
     with its own copy of the same four-line match:

     ```rust
     let x = match l.value.as_ref().map(|v| &*v.kind) {
         Some(ExprKind::Ident(id)) => id.name.clone(),
         _ => return Err(unsupported(…, "only `= bind <…>` is lowered")),
     };
     ```

     All five said "re-run with `--codegen v1`". v1 refuses a
     non-identifier RHS itself, with its own diagnostic — measured on
     `bind helper.x`, `bind helper()`, `bind (helper)` and `bind 5` at
     each of the five landings:

     ```
     let regs : DmaRegs = bind <expr>: regblock binding RHS must be a
       helper transactor identifier
     let helper : AxilHelper = bind <expr>: rhs must be a bare
       bus-binding name in v0
     ```

     So all five are `NotImplemented { Rejects }`, and they now share one
     `bind_rhs_ident`.

     **The hole is the side all five copies were guarding the wrong way
     round.** Each arm is gated on `l.bind`, so a `let` with NO `= bind`
     reaches none of them — and a regblock's mirror record shares the
     regblock's name, so `let regs : DmaRegs` landed on the ordinary
     record-local path and lowered, verified and emitted clean. The
     difference in the emitted testbench:

     ```cpp
     // let regs : DmaRegs = bind helper
     AxilHelper_write(40, 64);
     v = AxilHelper_read(40);
     regs.MM2S_LEN = v;

     // let regs : DmaRegs
     v = regs.MM2S_LEN;
     ```

     Every register access served from the mirror, no bus traffic at
     all, and the test passes without ever touching the DUT. v1 refuses
     the same program outright — "regblock instantiation requires
     `= bind <helper>` (a transactor with write/read methods)" — for
     every spelling: at test scope, inside `run`, with no initializer,
     and with a same-typed mirror on the right. TB-IR states that rule
     now too, as `Invalid`, in v1's own words. `addrmap` had the same
     hole reaching a different arm ("uninitialized `let chip` without a
     scalar type", which named neither the construct nor the fix).

     This is the first defect on this sweep that is not a
     misclassification: TB-IR was ACCEPTING a program v1 rejects, and
     tbir is the default backend.


105. **Five corrections to the corrections (2026-08-18).**

     Re-review of the four correction commits found one REGRESSION and
     four arms right only for the shape that prompted them.

     **The regression.** Divergence 103 gave `record_leaf_fate` an
     `is_record` callback to reproduce `Emitter::record_field_c_type`'s
     named-record layer, and wired it to `record_ids.contains_key`.
     `record_ids` is not `is_record_type`: `lower_program` also inserts
     every REGBLOCK's mirror record into it, and scoreboards lower
     afterwards. So a `scoreboard Sb { l : DmaRegs }` became a
     `--codegen v1` suggestion, and v1 flattens it to `int64_t l;`. The
     callback now takes `declared_record_names`, a snapshot of
     `record_ids` taken BEFORE the regblock loop — which is exactly
     transactions ∪ structs.

     **The second-DUT-handle arm** (divergence 102, corrected) is right
     for a second field of the SAME module type and wrong for anything
     else. v1 includes only the one Verilated header the testbench's DUT
     needs:

     | second field | v1 emits | g++ |
     |---|---|---|
     | `other : Top` | `VTop* other = nullptr;` + both binds | 0 errors |
     | `other : AxiLiteRegs` | `VAxiLiteRegs* other = nullptr;` | "'VAxiLiteRegs' does not name a type" |
     | `other : Nonesuch` | `VNonesuch* other = nullptr;` | "'VNonesuch' does not name a type" |
     | `mode : Color` (an enum) | `Color mode;` | "'Color' does not name a type" |

     The arm splits on whether the field's type name matches the DUT
     handle's; the other three are `EmitsUncompilable`.

     **The unreadable-width arm** (divergence 103) did not reach through
     a `Vec`, though its sibling `zero_width_leaf` does.
     `Vec<uint<0x8>, 4>` packs four 64-bit slots where
     `Vec<uint<8>, 4>` packs four 8-bit ones — the same substitution,
     per element.

     It also claimed a `queue<T>` / `event<T>` / `list<T>` payload
     "arrives as `TypeArg::Type` and is a different arm's business",
     which is false for a RECORD payload: `queue<Inner>` arrives as
     `TypeArg::Expr(Ident)` — the exact shape the arm reads as an
     unreadable width — and got told "a width must be a plain decimal
     literal" when there is no width slot at all. `cpp_tb.rs` states
     this ("`event<RegOp>` parses as `TypeArg::Expr(Ident)` at the
     type-arg layer") and so does `fixed_vec_field`'s NOTE. The set of
     width-taking builtins is taken from `scalar_leaf_c_type` now rather
     than inferred from the argument shape — its integer arms exactly,
     minus `Bool`/`BoolLower`/`Bit`, which return `"bool"` whatever their
     arguments say and so have no width slot to mis-read. Same failure
     mode as divergence 97's leaf table, three commits later.

     **The regblock-without-a-bind hole** (divergence 104) was closed at
     test scope only. `regblock_instance_types` was populated in
     `lower_test` and left empty in the helper, tseq, method and three
     transactor contexts, so `let regs : DmaRegs` inside a `hookable`
     body still lowered, verified and emitted; the addrmap half in the
     same position still returned `Unsupported`, the false hatch the
     divergence says it replaced. Every `LowerCtx` gets the set now,
     built once by `regblock_instance_names`.

     Smaller: `bind_rhs_ident` rendered an unbalanced backtick at two of
     its five call sites (a fragment passed into a template that wrapped
     it); two source comments cited divergence 103 for the hole
     documented as 104; and `scalar_leaf_c_type`'s doc still claimed
     `txn_field_c_type` "is the only caller that picks a member type",
     which divergence 103 had already called false in the same commit.


106. **The second-handle arm, third pass — and it cannot see what
     decides it (2026-08-18).**

     Divergence 105 split the transactor multi-handle arm on
     `simple != dut_ty`: the second field's type against the
     transactor's OWN first handle. The thing that decides whether v1's
     output compiles is a different quantity — v1 includes exactly one
     Verilated header, the TESTBENCH's DUT type — and
     `lower_unbound_item` cannot see it. Transactors lower before
     testbenches, and a transactor with two module fields never reaches
     the testbench-side check that does know it, because it errors out
     first.

     ```
     transactor Drv
         d1 : Foo
         d2 : Foo
         ...
     testbench Tb
         dut : Top
         drv : Drv active
     ```

     `simple == dut_ty` (`Foo` == `Foo`), so the split said "a real
     escape hatch". v1 emits `VFoo* d1 = nullptr; VFoo* d2 = nullptr;`
     and includes only `VTop.h` — `'VFoo' does not name a type`, twice.

     So the whole arm is `EmitsUncompilable`. The `other : Top` row
     really does compile, but only when the testbench's DUT is also
     `Top`, and an arm's status is the worst thing under it.

     The same measurement applies to the arm that DOES know the DUT
     type — `lower_test`'s "field type `X` differs from the test DUT
     type `Y`" — which was `Unsupported` and is now `EmitsUncompilable`
     for the same emitted C++.

     **The regblock contamination had one more consumer.**
     `components.rs`'s record-typed-field arm asked
     `record_ids.contains_key`, which by then holds every regblock's
     mirror record, so an `agent Ag { r : DmaRegs }` got a `--codegen
     v1` suggestion; v1 emits `VDmaRegs* r = nullptr;` and does not
     compile. The record arm asks `declared_records` now, and a
     regblock-typed component field gets its own `EmitsUncompilable`
     arm. Every other `record_ids` consumer was audited: the ones in
     `records.rs` run before the regblock loop, so they are already
     exactly transactions ∪ structs.

     Two guards that were correct but unmeasured now have rows: the
     `tseq` and component-method `LowerCtx`s (emptying either reopened
     the divergence-104 hole with the suite still green), and
     `bind_rhs_ident`'s rendered phrase, whose unbalanced backtick no
     test noticed.


107. **A verdict that rested on a debug-only panic, and the CI run that
     caught it (2026-08-18).**

     The zero-width record leaf was `Invalid` (divergences 103, 105) on
     the grounds that v1 PANICS on it — "attempt to subtract with
     overflow" in `emit_unpack_bits`.

     Rust turns integer overflow checks OFF under `--release`. CI builds
     `cargo build --release --all-targets` and runs `cargo test
     --release`; the shipped `harc` binary is a release build. So the
     panic never happens for a user. In release v1 emits a complete
     testbench that compiles clean:

     | field | v1 emits | packed as |
     |---|---|---|
     | `uint<0>` | `uint64_t data = 0;` | `harc_wide_write_bits(_packed, 0, 0, value.data)` |
     | `Vec<uint<0>, 4>` | `std::array<uint64_t, 4> data = {};` | the same zero-width write, per element |

     A full-width member carrying no packed bits, silently. `Invalid`
     claims no backend runs it in ANY configuration, and a release-built
     v1 runs it — so the arm is `SilentlyMisLowers`.

     Two things worth keeping from how this was found:

     * **Every local check on this branch has been a debug build.** The
       first CI run the branch got in hours failed on this within ninety
       seconds. Nothing in the local loop — `cargo test`, the mutation
       harness, five review passes, all debug — could have caught it,
       because they all observe the panic the release build does not
       have. The full-suite check now runs `--release` as well.
     * **A panic is not evidence about v1 without naming the profile.**
       It was the only verdict on this branch resting on one; the rest
       rest on emitted text or g++ exit codes, which do not vary this
       way.

     The test is written to hold in both profiles: the verdict is
     asserted unconditionally and the emitted member only where v1 got
     far enough to produce it.


108. **Five record-assignment arms, and a sixth spelling with no arm at
     all (2026-08-18).**

     Each of these rejected a whole family under one `Unsupported`, so
     each promised `--codegen v1` for programs v1 cannot compile. Only
     one family has a well-typed member:

     | assignment | v1 emits | verdict |
     |---|---|---|
     | `b = sb.q.pop()`, `q : queue<Beat>` | `b = _tb.sb.q.pop();` — compiles | a real escape hatch |
     | `b = sb.q.pop()`, `q : queue<uint<8>>` | the same line — "operand types are 'Beat' and 'long unsigned int'" | `Invalid` |
     | `b = drv.get()` | `b = Drv_get(_tb.drv);` — "'Beat' and 'uint64_t'" | `Invalid` |
     | `b = o` / `b = 5` | "'Beat' and 'Other'" / "'Beat' and 'int'" | `Invalid` |
     | `rec = 5` in a method body | "'Beat' and 'int'" | `Invalid` |
     | `drv.rec = 5` from test scope | the same — and TB-IR LOWERED it | `Invalid` |

     The last row is the find. The bare-name spelling of a whole-record
     state write was guarded; the dotted test-scope spelling was not. So
     `drv.rec = 5` lowered, verified, and emitted `drv.rec = 5;` from the
     DEFAULT backend — as uncompilable as v1's `_tb.drv.rec = 5;`. Test
     scope is where a user writes that line; the guarded spelling is the
     one they write less often. Same shape as divergence 104: one lane
     of a construct checked, another not, and the unchecked lane is the
     ordinary one.

     **`Invalid` is only for a mismatch the compiler can SEE.** The
     first version of these guards read the RHS with
     `record_id_of_expr`, which answers `None` for two very different
     things: an expression that is definitely not a record (a literal,
     an arithmetic result), and one it could not type. It had no arm for
     `Expr::ComponentField`, and record-typed composite-component fields
     are first class (`ComponentFieldKind::Record`) — so
     `drv.rec = src.cur`, a whole-record copy that BOTH backends
     compile and that lowered cleanly before this commit, became a type
     error. That is the same regression class as divergence 103's, one
     divergence later.

     `record_id_of_expr` types `Expr::ComponentField` now, so
     `drv.rec = src.cur` and `b = src.cur` both lower and emit C++ that
     compiles under both backends.

     Re-review found a third member of the same class, then a fourth,
     and the fix's own mechanism turned out to be the wrong shape:

       * A record-valued TERNARY (`b = c ? x : y`) was untyped too, so
         it was a false type error as well — v1 emits
         `b = (c ? x : y);` and g++ accepts it. That one was
         reconstructable from the repo: `expr_type` already types the
         shape a hundred lines away, `expr_type(t).or_else(expr_type(e))`.
       * `record_capable_expr` — the helper added to keep an escape
         hatch for an RHS the compiler could not type — listed
         `TbField` and `ComponentValue` among the shapes that might
         hold a record. Neither can: a `TbField` is built only where
         `ctx.tb_scalar_fields` matched (a RECORD testbench field is in
         `tb_record_fields` and resolves to a `Local`), and a
         `ComponentValue` is a whole component. So `b = count` and
         `b = src` kept promising `--codegen v1` for programs v1
         answers with "no match for 'operator='".

     The hatch was the wrong shape because the untypability was not
     real. Inside a component METHOD body `src.cur` lowered to
     `ComponentField { base: Path(["self", "src"]) }` —
     `component_path_head` builds a `"self"`-rooted path for a
     sub-component of the method's own component — and
     `component_base_id`, written as the inverse of that function,
     resolved only its OTHER head form, the test-scope instance. One of
     two head forms, so every component-field access in a method body
     looked untypable and something had to catch it.

     `component_base_id` inverts both head forms now, and a new
     `component_field_record` walks the rest: `field` is not always a
     single name — `as_component_record_field` returns a DOTTED one for
     a nested subfield (`ComponentField { base: SelfField, field:
     "cur.v" }`) — so it steps segment by segment exactly as that
     function's own `validate` closure does, stopping at anything that
     is not a traversable record. Answering the HEAD's record for a
     dotted path would lower `b = self.mine.v;` on a scalar leaf;
     answering `None` for every dotted path would reject `b = mine.inn`,
     a nested-record struct copy both backends emit.

     With that, every RHS shape types definitively and the hatch is
     gone: `record_assign_mismatch` is `Invalid` in all cases, and
     `b = src.cur`, `b = c ? x : src.cur`, `b = mine.inn` and
     `let c = src.cur` all LOWER — none of which they did before.

     Two more sites fell out. `lower_let`'s untyped tail typed its local
     from `expr_type`, which has no arm for a record-valued component
     field or transactor state field, so `let c = src.cur` declared
     `uint64_t c = 0;` and the DEFAULT backend emitted C++ that does not
     compile ("cannot convert 'Beat' to 'uint64_t'"); it asks
     `record_id_of_expr` as a fallback now, and tbir emits `Beat c{}`
     where v1 still emits an uncompilable `int64_t c = self.src.cur;`.
     And the `Vec<Record, N>` ELEMENT write — the same rule stated a
     third time — was still an `Unsupported`, promising v1 for
     `tbl.entries[1] = 5`, which v1 emits against a
     `std::array<Entry, 4>` and g++ refuses. It and its already-classified
     sibling (the whole nested-record field write) both route through
     `record_assign_mismatch` now.

     The mirror of the find, one line away in the same guard: a SCALAR
     state field assigned a record (`drv.st = b`) lowered, verified and
     emitted `drv.st = b;` — g++ "cannot convert 'Beat' to 'uint64_t' in
     assignment", the same failure v1 has. Also `Invalid` now.

     A third review round found no false `Invalid` but four more
     spellings of the same rule, plus one defect the `let` fix above had
     just introduced:

       * **The regression.** `record_ty` beats `declared_scalar_ty` in
         `lower_let`'s type chain, so teaching `record_ty` the component
         shapes made `let c : uint<8> = s.cur` type `c` as a `Beat` and
         DISCARD the annotation — the default backend laundering a type
         with no diagnostic, where v1 at least emitted
         `uint64_t c = _tb.s.cur;` for g++ to reject. A disagreement
         between a declared scalar type and a record RHS is `Invalid`
         now; it is visible precisely because the RHS types
         definitively. Keyed on the ANNOTATION'S PRESENCE, not on
         `typed_let_ir_type` — that function answers `None` for `int`,
         and `bit` parses as a `Named` type, so keying on its result
         left both spellings laundering the type while v1 emitted
         `uint64_t c = ...` and `Vbit* c = ...`.
       * **Five queue-pop lanes** — testbench, component, scoreboard,
         and the responder-body and test-scope spellings of a
         target-state queue — each type the popped local from the queue
         element and ignored the `let` annotation entirely, so
         `let b : Other = sb.q.pop()` on a `queue<Beat>` declared `Beat`
         and RAN, while v1's `Other b = _tb.sb.q.pop();` gets
         "conversion from 'Beat' to non-scalar type 'Other' requested".
         One shared `check_pop_let_type`, asked from all five. The first
         pass wired it to three and left out the testbench-queue lane
         and the TEST-SCOPE spelling of the same target-state queue
         whose responder-body spelling it had just fixed — the same
         one-lane-checked-one-not shape as the headline find, inside
         the fix for it.
       * A REGBLOCK name is in `record_ids` too (its mirror record is
         filed under the regblock's own name) and the pop lanes run
         BEFORE divergence 104's instantiation guard, so
         `let z : DmaRegs = sb.q.pop()` reached a pop lane first. The
         rule is stated once now — `regblock_instantiation_error` — and
         asked from both places, so the actionable "requires
         `= bind <helper>`" wins wherever the spelling lands.
       * **The record-annotated INITIALIZER** — `let b : Beat = 5` — was
         the `let` spelling of the write the assignment arms reject, and
         it still promised `--codegen v1` for a program v1 answers with
         "conversion from 'int' to non-scalar type 'Beat' requested".
       * **A whole-record COMPONENT field** was not writable at all,
         from either spelling, though v1 writes both. The dotted
         test-scope one (`s.cur = y`) hit "not a scalar component
         field", `Unsupported`; the bare method-body one (`cur = b`) hit
         "assignment to unknown name `cur`" labelled
         `EmitsUncompilable` — false in both halves, since `cur` is a
         declared field of that very component and v1 emits
         `self.cur = b;` and compiles. Both lower now, and a mismatched
         RHS is `Invalid` through the same shared verdict.

     A fifth round confirmed those and found the MIRROR of the whole
     divergence missing at ten more sites. The record-DESTINATION
     direction had been guarded arm by arm; the scalar-destination
     direction was guarded at exactly one site (a scalar
     transactor-state field), and everywhere else — a DUT port, a
     testbench field, a scoreboard counter, a scalar component field at
     both spellings, a scalar record field, a scalar `Vec` element, a
     record-state scalar subfield, a scalar local, and the assignment
     spelling of a record-queue pop — `x = <record>` LOWERED, VERIFIED
     and emitted from the DEFAULT backend, where g++ answers "cannot
     convert 'Beat' to 'uint64_t' in assignment". The scalar-local case
     did not even reach a diagnostic: it tripped the VERIFIER's
     `TypeMismatch`, which `main.rs` renders as "internal error: TB-IR
     failed verification after lowering" — a compiler-bug report for a
     program error. One shared `reject_record_into_scalar`, asked from
     all ten, and callable only because `record_id_of_expr` now types
     every record-carrying RHS. A sixth round found three more of the
     same shape, so it is asked from fourteen: the BARE-NAME spelling
     of a scalar state-field write (the polarity of that hole reversed
     — there the dotted spelling was unguarded, here the bare one), and
     all four RAL write spellings, which funnel through
     `lower_reg_write` / `lower_field_write` and return from inside the
     `try_lower_*` helpers, so none of `lower_assign`'s destination
     guards ever saw them.

     Deferred with its measurement: the same rule in ARGUMENT position
     — a record pushed onto a scalar queue, passed to a scalar method
     or hookable parameter, or emitted on a scalar-payload event. All
     lower and emit today, and the testbench-queue push answers a
     program error through the verifier's internal-compiler-error
     channel. That is a different surface from assignment and gets its
     own divergence.

     One further arm was wrong in both directions at once. A
     whole-record write of a TESTBENCH record field (`tbrec = b`) fell
     through every lane to the catch-all, which claimed
     `SilentlyMisLowers`. v1 emits `_tb.tbrec = b;` and g++ ACCEPTS it,
     so a working escape hatch was being withheld; and `tbrec = 5`
     emits a line g++ refuses, so that member is a type error. A
     `tb_record_field_target` resolver splits it like every other
     record destination.

     `Invalid` rather than `EmitsUncompilable` for the rest, on the
     distinction this sweep has been using: `EmitsUncompilable` is a
     subset gap a future TB-IR could implement and v1 currently botches
     (`Vec<uint<8>, 4> default 0` could sensibly zero-fill). A record
     local assigned an integer is a type error — there is nothing to
     implement, and no backend runs it.

     The transactor-method arm has no well-typed member at all: a record
     RETURN type is refused upstream, so every method reaching it
     returns a scalar.

109. **Seven `bound to` arms, three files, one rule stated three times
     (2026-08-18).**

     A `bound to <Ty>` clause has two out-of-subset type shapes, and
     the match resolving them was copied BYTE-IDENTICALLY into three
     lowering paths — the event-driven consumer BFM
     (`components.rs`), the bound TARGET responder and the bound
     INITIATOR BFM (`transactors.rs`). All six copies said
     `Unsupported`, so all six promised `--codegen v1`. Measured on
     each of the three paths separately:

     | clause | v1 | verdict |
     |---|---|---|
     | `bound to Bus#(ADDR_W=12, DATA_W=64)` | byte-identical C++ to `#(ADDR_W=32, DATA_W=32)`, and to the bare `bound to Bus` | `SilentlyMisLowers` |
     | `bound to uint<8>`, and `bound to <named-non-bus>` | refused at every instantiation; an uninstantiated declaration emits an inert struct | `Rejects` |
     | `bound to` on an `env`/`agent`/`sequencer`/`scoreboard` | three behaviours over nine cells; the worst COMPILES and silently drops the responder | `SilentlyMisLowers` |

     The generic row's evidence is the byte-identity between two
     parameterizations of the SAME textual length — so an identical
     emission cannot be explained away as a shifted source offset.
     v1's `type_simple_name` reads the last path segment and never
     looks at the argument list, so a user who writes
     `#(ADDR_W=12, DATA_W=64)` gets the bus declaration's defaults with
     no diagnostic.

     The second row is `Rejects`, not `Invalid`. v1 refuses it at both
     instantiation positions, but a NEVER-INSTANTIATED declaration gets
     through and emits an inert `struct T { … };`. Some configuration
     of that program runs, so the arm has something to implement, which
     is what separates `Rejects` from `Invalid` (divergence 108). The
     same argument condemns the NAMED-non-bus spelling, which was
     answering `Invalid` for a program shape one type-variant over —
     in all THREE copies of that check (both in `transactors.rs`, and
     the consumer-BFM one in `mod.rs` that the first correction missed
     because the probe's data-only transactor never routed through it).

     **The third row took three attempts, and the first two were wrong
     in opposite directions.** Version one measured it on
     `agent Watcher { hits : uint<32> default 0 }` — no event, no
     handler, nothing to bind — found v1's output byte-identical with
     and without the clause, and called the whole arm
     `SilentlyMisLowers`. That withdrew a `--codegen v1` promise which
     is, for one shape, honest: an `in event<T>` plus an `on <ev>`
     handler writing `bus.<ch>.<sig>`, bound at a `let x : C = bind
     <bus>` site, emits a complete working driver. Version two split on
     "is there a non-periodic `on bus.<ch>.handshake(...)` handler",
     which was over-promising again for three more shapes.

     The measurement that settles it counts bare `bus` identifiers
     outside comments in v1's emitted C++, over four handler shapes ×
     two instantiation positions:

     | shape | at a `= bind` | as a testbench field |
     |---|---|---|
     | `on <ev>` driver | **0 — resolved, compiles** | 4 |
     | `hookable` body | **0 — resolved, compiles** | 4 |
     | `on <bool-expr>` cycle trigger | 1 | 1 |
     | `on N cycles` writing the bus | 1 | 1 |
     | `on bus.<ch>.handshake(...)` | 1 | 1 |

     TWO cells resolve the bus and emit a working driver — the
     `on <ev>` handler body and, found a round later, the `hookable`
     body, which behaves identically (0 bare `bus` at a bind, 4 as a
     field). v1 REFUSES the transactor spelling of that same hookable
     program ("drives a DUT signal from the always-on body … spec
     §8.1"); the env/agent spelling bypasses that check. `bus` is
     declared nowhere in the non-working cells, so g++ answers "'bus'
     was not declared in this scope". Version two's predicate missed three of
     them, including — because `parse_on_handler` reads the trigger
     BEFORE the `cycles` decoration — `on bus.w.handshake(d) cycles`,
     the canonical broken shape wearing one extra word.

     A NINTH cell decides the label, and it is worse than either: a
     `thread bus.<method>(...)` responder. v1 COMPILES that one — zero
     bare `bus` references — because it drops the responder coroutine
     outright. Against `tlm_target_thread_if_test`, changing only the
     keyword `transactor` to `agent` deletes the whole
     `_target_read_target_slot` block (300 emitted lines to 242); the
     DUT's blocking read is then never answered and the fixture's own
     assertions fail at run time. All four declaration kinds emit
     byte-identically. `SilentlyMisLowers` outranks the seven
     uncompilable cells, so that is the arm's label.

     A third version justified the single label by claiming a predicate
     could not name the working cell "because the deciding fact is the
     INSTANTIATION POSITION and this code runs per DECLARATION". That
     was false: `lower_program` pre-scans every
     `TestItem::Let { bind: true }` before components lower and already
     threads five whole-file pre-scans into `lower_component_schema`,
     and v1 does exactly this pre-scan itself
     (`driver_bus_for_hookables`). The real reason not to split is
     simpler and survives the ninth cell: a position split does not
     help, because the `thread` case is silent in BOTH columns.

     The scoreboard spelling in `scoreboards.rs` is a FOURTH copy of
     the same rule, and it carried the same wrong verdict from the same
     degenerate measurement — `on bus.w.handshake(d)` on a bound
     scoreboard emits `(bool)(bus.w.handshake(d))` too, and a `thread`
     responder on one is dropped just as silently. A scoreboard WITH a
     `hookable` is a component, so it routes through `components.rs`
     instead — a lane nothing covered until a mutation survived there.

     A FIFTH copy of the rule sits on the per-FIELD spelling
     (`seen : uint<32> bound to Drv` on an env/agent/sequencer/
     transactor, sibling of the scoreboard one). It was the last arm in
     the family still promising `--codegen v1`, and v1 discards the
     clause: with the bound and unbound sources padded to the SAME byte
     length — so no source-offset residue can explain it — v1's output
     is byte-identical. Its per-FIELD
     sibling (`seen : uint<32> bound to Drv`) is a different construct
     and keeps `SilentlyMisLowers`, which the byte-identity does
     measure.

     The six copies are now one `bound_bus_name(bound_to, subject)` in
     `lower/mod.rs`. The consolidation has its own finding: the bus is
     the LAST path segment, and nothing tested that. A dotted
     `bound to arc.stdlib.BusAxiLite` lowers today and v1 emits it;
     resolving the FIRST segment instead would look up a bus named
     `arc`, find none, and return `Invalid` — divergence 108's bug
     class again, this time reachable through a refactor rather than a
     new guard. Eighteen mutations, all caught — including that one, one
     that reads the generic guard as `generics.len() > 1` (every probe
     passed two arguments, so arity-1 was unexercised), one per label on
     the two `bound to`-clause arms, one on the bound-INITIATOR non-bus
     arm (its byte-identical twin on the target path was covered and it
     was not, because a data-only probe transactor routes to the target
     path), and one that drops the per-site `subject`, which review
     found unpinned: the only thing the three call sites pass
     differently was never asserted.

110. **Five event-driven-transactor shape arms, one v1 naming rule, and
     a dead arm (2026-08-18).**

     v1's poke lowering hardwires the name `dut`: a `dut.<sig>` write in
     an `on`-handler body is rewritten to the TEST's bound DUT pointer
     (`harc_rt::harc_assign(dut->en, 1)`), and the transactor's own
     module-typed field is emitted as an INERT `VTop* dut = nullptr;`
     member that nothing in the file ever reads. That single fact
     decides four of these five arms.

     | arm | v1 | verdict |
     |---|---|---|
     | `bound to` transactor WITH a module-typed field | member inert, poke retargets to the test DUT — compiles and runs | **honest `Unsupported`, kept** |
     | more than one DUT handle | un-poked: an inert second member. POKED: `_tb.drv.dut2.en` — a `.` on a `VTop*` | `EmitsUncompilable` |
     | handle not named `dut` | same split; v1 rewrites only that name | `EmitsUncompilable` |
     | `on bus.<ch>.handshake(...)` on a NON-bound component | emits the handler anyway: "'bus' was not declared in this scope" | `EmitsUncompilable` |
     | "malformed `on bus.<ch>.handshake(...)`" | — | **dead arm** |

     The first row is a correction against my own first pass. Seeing
     `VAxiLiteRegs* dut = nullptr;` and `harc_rt::harc_assign(dut->rst,
     0);` in one emitted file, I read it as a null dereference and
     labelled the arm `SilentlyMisLowers`. They are two different
     `dut`s: the one being poked is the run function's bound test DUT,
     captured by the handler lambda's `[&]`. Checking which binding a
     name resolves to was the missing step — the same shape as
     measuring against a control that is equally broken. The arm's
     `--codegen v1` promise is honest, so it keeps it, and the test
     states the reason directly rather than resting on a byte-count.

     The dead arm is the batch's find. `is_bus_handshake_monitor` and
     `bus_handshake_monitor_channel` tested the SAME three conditions —
     an `ExprKind::Call`, a callee `Field { name: "handshake" }`, and a
     `Field` target — and the caller ran the predicate, then the
     extractor, then reported "a malformed `on bus.<ch>.handshake(...)`
     handler" if they disagreed. They cannot disagree: whenever the
     predicate passed, the extractor returned `Some`. One rule stated
     twice, and the second copy guarded nothing. Confirmed by
     construction and by probe — `on bus.w.handshake()` and
     `on bus.w.handshake(d, e)` both lower cleanly on a bound
     transactor. The extractor is the predicate now
     (`if let Some(channel) = ...`), so the two cannot drift back apart.

     Thirteen mutations, all caught. Three needed new coverage rather
     than a new guard: nothing tested that the predicate DECLINES a
     different method name or a one-level target, and nothing tested
     the arm that was deliberately left alone.

111. **The assignment rule at the other half of the surface: a typed
     SLOT (2026-08-19).**

     Divergences 104 and 108 swept assignment destinations. The mirror
     of that rule — a value entering a queue element, a method
     parameter or an event payload — was open at every family, in BOTH
     directions. Measured across four slot families × the combinations
     each admits, on both backends:

     | slot | value | v1 / tbir C++ |
     |---|---|---|
     | `queue<uint<8>>` | a record | "cannot convert 'Beat' to 'long unsigned int'" |
     | `queue<Beat>` | the same record | **compiles** |
     | `queue<Beat>` | a different record | "cannot convert 'Other' to 'Beat'" |
     | `queue<Beat>` | a scalar | "cannot convert 'long unsigned int' to 'Beat'" |
     | scalar parameter | a record | "no match for call to ..." |
     | record parameter | a scalar / other record | "no match for call to ..." |
     | `event<uint<8>>` payload | a record | "no match for call to ..." |
     | `event<Beat>` payload | a scalar / other record | "no match for call to ..." |

     Sixteen mismatched cells; five matching ones compile. Every
     mismatch is refused by BOTH backends, so all sixteen are `Invalid`
     — a type error with nothing to implement.

     Three things this turned up that a single probe would have missed:

       * **The testbench queue answered a PROGRAM error through the
         COMPILER-BUG channel.** Two of its four cells reached the
         verifier's `BadProgramRef` — "internal error: TB-IR failed
         verification after lowering" — for a program the user wrote
         wrong. The other two lowered and emitted freely, so the
         verifier check was not merely misplaced, it was asymmetric.
       * **The `emit` shape check that existed was keyed on
         `expr_type`,** whose `(_, IrType::Unknown) => true` arm waved
         through exactly the record-carrying shapes divergence 108 had
         just taught the compiler to type. It reads `record_id_of_expr`
         now. The other two `emit` lanes (`ComponentEmit`, both the
         dotted and self-relative spellings) checked arity and never
         the payload type at all.
       * **`TransactorMethodSchema` carried `param_names` but not
         `param_tys`,** so the transactor call site had nothing to
         type-check against — the same gap its own doc comment records
         for names ("this was `n_params` and the names were dropped on
         the floor at construction"). Extended the same way.

     The queue family has SIX lanes, not three. Beyond the scoreboard,
     component and testbench pushes, a bound-to TLM responder's
     target-state queue takes a push in two spellings — bare
     (`pending.push(x)`, inside the responder body) and test-scope
     (`responder.pending.push(x)`) — and both were open. Its element
     lives in `target_state_fields` rather than a queue table, which is
     why it reads differently at each site.

     A note on how that one was measured, because the first attempt
     proved nothing: all four target-state cells failed to compile,
     which looked like confirmation until the error turned out to be a
     missing `VTlmReadInitiator.h` — the CONTROLS were failing too. With
     a stub written from the `dut->` references in the generated file,
     the two matching cells compile and the two mismatched ones fail in
     both backends, which is the actual evidence.

     **The guards found a pre-existing bug by breaking on it.** An
     untyped `let` bound from a RECORD-returning component method
     (`let b = s.mk(1)`) never applied the method's `ret_ty`, so the
     local carried no record type: tbir emitted `uint64_t b; b =
     Src_mk(...)` — which does not compile — while v1's
     `auto b = Src_mk(_tb.s, 1);` does. Silent, and invisible until the
     slot guards started reading that local's type, saw "a scalar", and
     called five well-typed programs `Invalid`. `lower_let` applies
     `m.ret_ty` now, which fixes the false rejections and the silent
     mis-emission together.

     A second review round found the same shape twice more. The
     TESTBENCH-METHOD spelling of that `let` was untyped for the same
     reason and broke the same way. And the `ret_ty` inference itself
     regressed: gated on `declared_scalar_ty`, which
     `typed_let_ir_type` answers `None` for on `int` and `bit`, it
     typed `let n : int = s.mk(1)` as the method's RECORD — the default
     backend emitted `Beat n{}` and RAN while v1 refused to build it.
     That is the defect the untyped-`let` guard one screen up keys on
     `l.ty` to prevent; the same mistake was made in code written next
     to the fix for it. Both now key on `l.ty`.

     A third round found the defect a THIRD time, at the `fork`
     spelling: `try_lower_tlm_fork` declared its destination without
     typing it, where the non-fork path one screen up computes
     `tlm_ret_record_id(m.ret)`. That one is the worst of the three,
     because with no slot involved nothing fails — tbir emitted
     `uint64_t r = 0; r = harc_read(...)` and v1
     `Resp r = {}; r = harc_unpack_Resp(...)`, and BOTH compile, so for
     a multi-field struct the two backends silently computed different
     values.

     One `check_slot_type(value, want, what)` behind all of it, with a
     `check_queue_push` adapter for `QueueElem`. Twenty-six mutations,
     all caught — but only after review found FOUR of the eight advertised
     call sites unpinned: the test exercised statement-position calls
     and the dotted `emit` spelling only, so removing the guard at the
     record-annotated `let`, the untyped `let`, the assignment site, the
     self-relative `emit`, or the bound-initiator `param_tys` left the
     suite green. Each spelling needed its own case.

     **Deferred, measured:** three slot families, on the belief that
     each needed a parameter-type table that did not exist. None of the
     three did — see divergence 112.

112. **None of the three deferred slot families needed a new table
     (2026-08-19).**

     Divergence 111 deferred the transactor SIBLING-method call,
     testbench-method parameters, and `tseq` arguments together, on the
     stated ground that each needs a parameter-type table that does not
     exist. That was wrong about all three.

     **Testbench-method parameters** already had the types in hand:
     `lower_tb_method_call` calls `ir_type_of_param(p.ty, ctx)` two
     lines above the argument loop, to decide the module-typed-parameter
     arm. The check is three lines. Measured, both backends refuse every
     mismatch and accept both matching cells:

     | call | v1 / tbir |
     |---|---|
     | `tf(b)` into `t: Beat`, `ts(1)` into `x: uint<8>` | compile |
     | `tf(1)` | "no match for call to" |
     | `tf(o)` on a different record | **reached the VERIFIER** |
     | `ts(b)` | "no match for call to" |

     The third row is why this one was worth doing first: a mismatched
     record reached `verify_program`'s `TypeMismatch`, which `main.rs`
     renders as "internal error: TB-IR failed verification after
     lowering". A program error answered through the compiler-bug
     channel — the same pathology divergence 111 fixed for the testbench
     queue, still live one spelling over.

     **The sibling-transactor call** needed a fourth element on an
     existing tuple, not a new table: `self_transactor_methods` is built
     from `h.params` right after `method_ctx`, so
     `ir_type_of_with_records(p.ty, &method_ctx.record_ids)` was
     available at both construction sites. Same three mismatched cells,
     same verdicts, both controls compiling.

     **`tseq` call arguments** looked like they needed nothing either:
     every reachable tseq parameter is a scalar, the reasoning went, so
     the slot is always `None` and `ctx.tseqs` needs nothing added.
     That was wrong, and divergence 114 has the correction — a
     `TSeq<T>` tseq parameter is reachable and v1 compiles it, so the
     hard-coded `None` rejected a working call while passing a broken
     one. What the measurement below DOES establish is the narrower
     claim about RECORD parameters, which still holds: a record-typed
     tseq parameter has no working member however the callee is
     written:

     | shape | outcome |
     |---|---|
     | `yield <param>` | refused upstream — not a record local |
     | `<param>.<field>` | refused — "field access on a non-DUT value" |
     | declared, UNUSED | tbir `[&](uint64_t seed)`, v1 `[&](VBeat* seed)` — v1 does not compile; tbir DOES (see divergence 115) |

     That last row is a separate, pre-existing defect at the tseq
     DECLARATION site: both backends silently mis-type a record
     parameter, v1 reading it as a Verilated module pointer. Left for
     its own batch — the call-site guard here is right regardless,
     since a record argument is wrong under every spelling of the
     callee.

     Five mutations, all caught, including one that empties the sibling
     `param_tys` at both construction sites (emptying only one is caught
     by the other's test) and one that maps a record parameter to a
     scalar slot.

     That "all caught" was measured over five mutants and stated as if
     it covered the change. It did not: a later round truncating each
     parameter loop to `.take(1)` found the sibling spelling had no
     mismatched cell past parameter #1, so the whole loop index was
     tested only at zero. The five mutants were the ones this entry
     thought to write, which is not the same as the ones the code
     admits. See divergence 113.

113. **A slot rule with a third value shape it could not name
     (2026-08-19).**

     Divergences 111 and 112 built the slot check around one question —
     "is this a record, and if so which?" — because the two shapes they
     had measured were a record and a scalar. A `TSeq<T>` parameter is
     neither, and flattening it into "not a record" was wrong in both
     directions at once:

     | call, on `function fseq(s: TSeq<Beat>)` | before | v1 |
     |---|---|---|
     | `k.fseq(xs)` | lowers | compiles |
     | `k.fseq(b)` | rejected: "takes a non-record value" — **false** | "no match for call to `<lambda(Sink&, const std::vector<Beat>&)>`" |
     | `k.fseq(1)` | **lowered clean** | same "no match for call to" |

     The second row states something untrue about the declaration the
     user wrote; the third is the hole that wording was hiding, since an
     `int` is not a record either.

     `IrType` already had `RecordSeq` and `Seq`, and
     `method_schema_ir_type` already resolved `TSeq<T>` into them for the
     component-method schema — `param_tys` was carrying the right type
     all along and the slot check was throwing it away. The fix names
     the shapes the check can actually decide and routes the
     COMPONENT-METHOD and TESTBENCH-METHOD parameter positions through
     the declared `IrType` instead of a boolean. The `TSeq` resolver
     moved to `helpers::tseq_ir_type` so the schema side and the check
     side cannot disagree about what `TSeq<Beat>` means.

     Enumerated mechanically rather than sampled — 3 value shapes (a
     scalar, a `Beat`, a `TSeq<Beat>`) into 8 slots (scalar, record and
     sequence parameters at the component and testbench-method
     spellings, plus a scalar and a record scoreboard queue), on both
     backends. 24 cells, a clean diagonal: the 8 matching cells compile
     under v1 and lower under tbir, all 16 mismatches get a g++ error
     from v1. `Invalid` throughout, as everywhere else in this family.

     The grid covers the two spellings it names and no others. Reading
     it as a statement about "the parameter positions" was the mistake
     divergence 114 had to undo: the transactor-method, `tseq` and bus
     `tlm_method` positions were NOT routed through `IrType`, and two of
     them went on to reject working programs. A grid is evidence about
     its own cells.

     Three further mutants from the same round, each a real gap:

     - the sibling-transactor parameter loop truncated to `.take(1)` —
       every mismatched cell in the suite sat at parameter #1;
     - the scalar half of divergence 111's `fork` destination typing
       deleted — the record cell kept it green while a
       `-> uint<128>` fork silently declared `uint64_t` and truncated;
     - `.last()` → `.first()` on a `let` annotation's type path, which
       chased down a better answer than a test: the guard it sat in can
       never see a record-naming annotation at all, because
       `lower_let`'s record-typed-local branch claims every one of them
       first and returns on every path. The check was a condition with
       one reachable verdict dressed as two, and is now written as one.


114. **"Cannot tell" is not "is not a record" (2026-08-19).**

     Divergences 111-113 built a slot check that answers with a small
     enum, and every type it could not name fell into `Scalar`. That
     turns an ABSENCE of information into a positive claim, and three
     well-typed programs were called `Invalid` on the strength of it —
     each one compiled by v1, each one refused by the DEFAULT backend:

     | program | v1 | before 113 | after 113 |
     |---|---|---|---|
     | `let ys = k.gen(xs)` on `-> TSeq<Beat>`, then `k.fseq(ys)` | compiles | lowered, emitted `uint64_t ys = 0` | **`Invalid`** |
     | `drv.dispatch(xs)` on `dispatch(txns: TSeq<Beat>)` | compiles | lowered, emitted `std::function<void(long unsigned int)>` | **`Invalid`** |
     | `Wrap(xs)` on `tseq Wrap(xs: TSeq<Beat>)` | compiles | lowered, emitted `[&](uint64_t xs)` | **`Invalid`** |

     The middle column is the point. Before 113 each of these was an
     `EmitsUncompilable` gap — bad, but the honest verdict for it is
     `Unsupported`/`NotImplemented`. Turning it into `Invalid` did not
     tighten anything; it took a program v1 builds and had the default
     backend refuse it outright. The repo already stated the rule this
     broke, in `component_base_id`'s own doc: callers "must treat that
     as 'cannot tell', never as 'not a record'."

     So the enum grows an `Unknown` and the guard rejects only when it
     can name BOTH sides. That is the whole safety property, and it is
     what makes the check safe to extend: a slot the compiler cannot
     resolve is now unchecked rather than assumed scalar.

     `Unknown` alone would have been a retreat, because the hole runs
     the other way too — a scalar into a `TSeq` slot is exactly as wrong
     and v1 refuses it. So the four positions that were guessing got
     their types instead:

     - the transactor method schema learned `TSeq` (`method_param_ir_type`);
     - `ctx.tseqs` carries declared parameter TYPES, not just names;
     - a `-> TSeq<T>` method result now types its `let` at the
       component-method spelling (the testbench-method one cannot yet —
       see below). Typing the `let` is all this does: the METHOD's own
       return slot is still emitted `uint64_t __ret`, so tbir gets
       `std::vector<Beat> ys{}` and then `__ret = s;` — "cannot convert
       `std::vector<Beat>` to `uint64_t`" — while v1 compiles. That
       half reproduces on the merge base too, so the row below is about
       the REJECTION going away, not about the program building;
     - the bus `tlm_method` request path gained a check at all — it had
       the declared type in hand as a WIDTH hint and never asked whether
       the argument belonged in the slot. `mem.read(b)` with a record
       argument lowered, verified, and emitted
       `harc_rt::harc_assign(dut->mem_read_addr, b);` from BOTH backends,
       both failing g++ with "invalid `static_cast` from type `Beat`".
       Guarded at the blocking and `fork` spellings together.

     Two shapes deliberately stay unchecked, and it is worth being
     explicit about which:

     - An `int`- or `time`-typed parameter reached through a SCHEMA's
       `param_tys`. Those tables also type the parameter's local and
       therefore the emitted C++, so widening them to call `int` a
       scalar would change what the backend emits — out of scope for a
       slot check. Where the declared `TypeExpr` is in hand,
       `helpers::slot_ir_type` answers precisely; where only the schema
       is, the slot is unchecked. A missed rejection, never a false one.
     - A literal is read as a scalar directly rather than through its
       `IrType`, because a bare integer literal carries `ty: Unknown`
       (the width is inferred at the use site). Without that,
       `q.push(1)` into a `queue<Beat>` — the cell this whole family
       started from — would have become unknown and stopped being
       rejected.

     One thing this did NOT fix, and the first attempt claimed it had.
     A testbench method's return temp resolves through
     `ir_type_of_with_records`, which answers `Unknown` for `TSeq<T>`,
     so the whole inlined chain is untyped — parameter included:

     ```
     v1    auto Tb_passthru = [&](Tb& self, const std::vector<Beat>& s)
                                  -> const std::vector<Beat>& { ... };
           auto ys = Tb_passthru(_tb, xs);          // compiles
     tbir  uint64_t s = 0;  uint64_t ys = 0;
           s = xs;                                  // g++: cannot convert
                                                    // `std::vector<Beat>`
                                                    // to `uint64_t`
     ```

     Reproducible on the merge base, so it is an older
     `EmitsUncompilable` gap and not this rule's to close — typing that
     chain changes the emitted C++ and deserves its own measurement. The
     code written for it here read `expr_type` for a sequence that is
     never there, i.e. it was dead the moment it was written, and has
     been removed rather than left looking like a fix. The `Unknown`
     rule is what keeps the slot check from adding a false rejection on
     top of the gap meanwhile.

     Found by review, not by the mutation round: 21 mutants over the 113
     change surface were all caught, and every one of these defects
     survived that, because a mutant can only ask whether the code does
     what its tests say. None of the three false `Invalid`s had a test,
     since the grid that "proved" the change covered two spellings and
     the defects were in the other three.

115. **The fix that dropped the rejection it was quoting (2026-08-19).**

     Divergence 114 gave the `tseq` call site a real parameter table so
     it would stop hard-coding the slot to "not a record". Resolving
     that table through `slot_ir_type` turned a `tseq Wrap(seed: Beat)`
     parameter into `IrType::Record(Beat)` — so a `Beat` argument now
     MATCHED, and the call lowered. The comment sitting on that exact
     line still said a record argument is refused, and listed the
     evidence for it. The evidence was still true; the code had stopped
     acting on it.

     | call | HEAD before this fix | v1 | tbir |
     |---|---|---|---|
     | `Wrap(s)` on `seed: Beat` | **lowered** | `[&](VBeat* seed)` — "`VBeat` has not been declared" | `[&](uint64_t seed)` — no match for call |
     | `Wrap(7)` on `seed: Beat` | `Invalid`, wrong shape | same | rejected |

     Two spellings of one unbuildable program getting two different
     verdicts is the tell: the defect is the PARAMETER, not the
     argument. Neither emitter honours a record there — v1 renders it as
     a Verilated module handle, tbir as a scalar — so no call to such a
     `tseq` compiles under v1. It is now named on the parameter, once,
     and classified `NotImplemented { EmitsUncompilable }` rather than
     `Invalid`, because the DECLARATION alone is fine under tbir: an
     uncalled `tseq Wrap(seed: Beat)` compiles there (`uint64_t seed` is
     a valid lambda parameter, merely wrong). Divergence 112's table
     said "neither compiles" for that row; that was measured on the
     CALLED form and stated of the unused one.

     `extern function ref_add(a: uint<8>, b: Beat)` is a NEIGHBOURING
     defect, and the comment there was worse — it asserted that every
     extern-fn parameter is a scalar (citing this module's own header)
     and quoted a diagnostic to match: "both backends emit
     `ref_add(1, <Beat>)`: cannot convert `Beat` to `uint64_t`". The
     declaration parses, and both backends emit

     ```
     uint64_t ref_add(uint64_t a, VBeat* b);
     ```

     — g++: "`VBeat` has not been declared". Right verdict, invented
     reason.

     It is NOT the same rule as the `tseq` one, and the first fix said
     it was ("for the same reason … and measured the same way"). See
     divergence 116: `emit_extern_fn_decls` is shared — tbir calls
     straight into v1's — so unlike the `tseq` lambda there is no
     backend that compiles it, which makes it `Invalid` and puts the
     verdict at the DECLARATION rather than at each call.

     **And the same "absence dressed as a claim" on the value side.**
     `expr_type` propagates its operand's type through `Binary`,
     `Unary` and `Ternary` without asking whether the operator PRODUCES
     that type, while `record_id_of_expr` does ask (its ternary arm
     requires both arms to name the same record). Reading the value's
     shape off `expr_type` alone therefore made the guard state:

         parameter `t` of `Src.obo` takes a `Other` and was given a `Beat`

     about `s.obo(b + 1)` — asserting that `b + 1` IS a `Beat`. It is
     not; it is a type error, and `s.obs(b + 1)` lowered clean because
     the assertion happened to point the right way there.

     The two type sources disagreeing is not noise — it is the signature
     of a malformed record expression, and it is now the rejection
     itself. Measured on both backends with a compiling control
     (`s.obs(b)`): `s.obs(b + 1)` gives "no match for `operator+`
     (operand types are `Beat` and `int`)" and a mismatched-arm ternary
     "operands to `?:` have different types `Beat` and `Other`", from v1
     and tbir alike. `Invalid`, and the message names the shape of the
     mistake rather than claiming a type for the expression.

     Three review rounds in a row have now found that a verdict was
     resting on a sentence nobody re-measured after the code moved. The
     pattern is specific enough to name: when a check changes what it
     consults, the comment justifying it is evidence about the OLD
     consultation, and it has to be re-run, not re-read.

116. **The check meant to stop false `Invalid`s introduced one
     (2026-08-19).**

     Divergence 113 gave the extern-fn call a slot check and hard-coded
     the slot to scalar, on the ground that "every extern-fn parameter
     is a scalar — this module's own header says so". Divergence 115
     rewrote that function, put the declared parameter TYPES in scope
     two statements above, used them for the record case, and left the
     loop reading `None` — re-asserting the same comment on the way
     past.

     `TSeq<T>` is the counterexample. `cpp_tb::c_type_for` renders it
     `const std::vector<T>&`:

     | | verdict |
     |---|---|
     | `extern function ref_sum(xs: TSeq<Beat>)`, called with a sequence | **`Invalid`** at HEAD |
     | v1 | `uint64_t ref_sum(const std::vector<Beat>& xs);` — compiles |
     | tbir at the merge base | same line — compiles |

     A false `Invalid` on a program BOTH backends build, introduced by
     the family whose entire purpose is removing them, in the commit
     written to remove the previous three. The slot now reads the
     declared type like every other parameter position.

     **And the record case was classified off the wrong measurement.**
     Divergence 115 argued `NotImplemented` rather than `Invalid`
     because an uncalled declaration compiles — true for `tseq`, where
     tbir emits its own `[&](uint64_t seed)` lambda, and asserted of
     extern fns without re-measuring. `emit_extern_fn_decls` is shared:
     `src/codegen/tbir/mod.rs` calls straight into v1's, so both
     backends emit the byte-identical `VBeat*` forward declaration and
     NEITHER compiles — called or not:

     ```
     v1    uint64_t ref_add(uint64_t a, VBeat* b);   // 'VBeat' has not been declared
     tbir  uint64_t ref_add(uint64_t a, VBeat* b);   // identical
     ```

     So it is `Invalid`, and the call site was the wrong place to put
     it: a declared-but-uncalled record-parameter extern fn lowered
     clean while the default backend emitted C++ that does not build.
     Moved to the declaration, where the breakage actually is.

     That is the same "measured on one form, stated of the other" error
     divergence 115 was written to correct, committed one commit later
     inside the correction. The two spellings look alike and are not:
     `tseq` emits a lambda per backend, `extern function` emits one
     shared declaration. Sharing a code path is what decides whether a
     backend can differ, and it has to be read, not assumed from the
     surface similarity of two constructs.

117. **One mechanism, asked about one type (2026-08-19).**

     Divergence 116 moved the extern-fn record check to the declaration,
     keyed on `IrType::Record`, looking only at parameters. Both halves
     of that were narrower than the mechanism they were built on.

     `emit_extern_fn_decls` runs every type in the signature through the
     free `c_type_for`, whose `Named` arm is

     ```rust
     TypeExpr::Named { name, .. } => format!("V{last}*")
     ```

     So it is not about records. Two shapes the record question could
     not see, each lowering clean at the time and each emitting C++ that
     neither backend compiles:

     (This entry described that arm as having "no conditions at all".
     It has one, three lines above it, and divergence 118 is what that
     cost.)

     | declaration | both backends emit | g++ |
     |---|---|---|
     | `extern function ref_mk(a: uint<8>) -> Beat` | `VBeat* ref_mk(uint64_t a);` | "`VBeat` does not name a type" |
     | `extern function ref_en(c: Color) -> uint<8>` | `uint64_t ref_en(VColor* c);` | "`VColor` was not declared" |

     The return type goes through the same call two lines down from the
     parameters, and an enum is rendered `V*` exactly like a record —
     `IrType::Record` just cannot represent one. The check now asks
     whether HARC DECLARES the name (transaction, struct, enum, agent,
     env, scoreboard, sequencer, covergroup, transactor, regblock), and
     covers the return type alongside the parameters.

     The exception is measured, not assumed: a DUT-module-typed
     parameter still lowers, because `VTop*` is the one `V<name>` that
     IS in scope — `extern function ref_peek(d: Top)` emits
     `uint64_t ref_peek(VTop* d);` and compiles under both backends.
     That is precisely why the rule asks what HARC declares rather than
     what looks like a module handle.

     Three smaller things from the same round, all in code this sweep
     wrote:

     - **No arity check on extern calls at all.** The slot loop's
       `get(i)` stopped at the shorter side, so `f(1, 2)` on a
       one-parameter fn lowered — both backends: "too many arguments to
       function `uint64_t f(uint64_t)`". Worse, a surplus RECORD
       argument was checked against a fabricated scalar slot and
       reported that "an argument of extern fn `f` takes a non-record
       value", describing a parameter that does not exist. A slot that
       is not there has no type to disagree with.
     - **A nondeterministic diagnostic.** The declaration loop iterated
       `extern_fn_decls`, a `HashMap`, so with two offending extern fns
       the compiler named a different one run to run on identical input.
       Every other declaration walk in `lower_program` iterates
       `file.items`; so does this one now.
     - **"Extern fns are pure scalar C functions"** survived in three
       comments after divergence 116 removed the two that quoted it.
       `TSeq<T>` disproves the "scalar"; PURITY is the property the
       lowering actually depends on, and the two had been travelling
       together as one claim.

     The recurring shape across rounds 5-8 is worth stating once more,
     because this is its fourth variant: a rule gets built from one
     measured example, and the example's incidental features get baked
     in alongside its essential one. Record rather than "named type".
     Parameters rather than "signature". Scalar rather than "pure".
     Reading the emitter is what separates them, and it is cheaper than
     four review rounds.

118. **"Reading the emitter" — while quoting the wrong line of it
     (2026-08-19).**

     Divergence 117 closed with "Reading the emitter is what separates
     them", and rested the rule on a claim about `c_type_for`: that its
     `Named` arm is *unconditional*. It is not. The function opens with
     a guard, three lines above the match:

     ```rust
     fn c_type_for(t: &TypeExpr) -> String {
         if is_list_type(t) || fixed_vec_type_args(t).is_some() {
             return txn_field_c_type(t);
         }
     ```

     and `is_list_type` is defined ON `TypeExpr::Named` — it matches any
     named type whose last segment is `list` or `List`. So a `Named`
     type can and does bypass the `V{last}*` arm.

     The rule keyed on "does HARC declare this name", which was wrong in
     BOTH directions at once:

     | signature | both backends emit | verdict before |
     |---|---|---|
     | `struct List` parameter | `std::vector<uint64_t>` — **compiles** | **`Invalid`** |
     | bare undeclared `Nope` | `VNope*` — undeclared | lowered |
     | a `domain` name | `VSysDomain*` — undeclared | lowered |
     | `string` (parses as `Named`) | `Vstring*` — undeclared | lowered |

     A false `Invalid` on a program v1 builds, and three live holes, from
     one whitelist. And the message asserted `V{ty}*` for the `List`
     case, where no such handle is emitted at all.

     The rule now asks the emitter instead of restating it:
     `cpp_tb::verilated_handle_name` sits next to `c_type_for`, applies
     the same guard, and answers "does this render as `V<name>*`". A
     name is refused unless it is the DUT's own type — measured, and now
     pinned by a negative: `ref_peek(d: Top)` lowers while
     `ref_peek(d: Nope)` does not, which is what makes the DUT cell mean
     anything. The previous version of that test used a fixture whose
     type name was merely absent from the whitelist, so it would have
     passed under any implementation that forgot an item kind.

     Two smaller ones from the same round, both in code this sweep
     wrote: the comment recording that `lower_extern_fn_call` "has no
     arity check anywhere in the pipeline" survived the commit that
     added one, worked example and all; and the arity check left behind
     the dead `None` arms it made unreachable — including the one
     producing "an argument of extern fn `f`", the exact diagnostic that
     commit was written to retire.

     Fifth variant of one shape, and the sharpest: the previous four
     were rules built from an example's incidental features. This one
     was a rule built from a *quotation* of the emitter that stopped one
     line short. "Read the emitter" is not a technique — reading the
     whole function is.

119. **Three arms paste a name; the rule modelled one (2026-08-19).**

     Divergence 118 said the rule "asks the emitter instead of restating
     it". It did not: `verilated_handle_name` was written NEXT to
     `c_type_for` with the guard and the segment extraction duplicated,
     which is the same restatement one file over — and the two had
     already drifted (`c_type_for` used `unwrap_or("")` on an empty
     path, the new function returned `None`; unreachable through the
     parser, but a real difference between two functions asserted to be
     one). `c_type_for` now calls it, so the claim is true by
     construction rather than by intention.

     And "every parameter AND the return type through `c_type_for`" was
     true of the EMITTER and false of the CHECK. `c_type_for` pastes a
     HARC name in three places: the `Named` fall-through (`V{name}*`),
     the `TSeq<T>` element (`const std::vector<T>&`) and the `queue<T>`
     element (`HarcQueue<T>&`). The rule covered the first. Measured
     against the compiling controls `TSeq<Beat>` / `queue<Beat>`:

     | extern parameter | both backends emit | g++ | verdict before |
     |---|---|---|---|
     | `TSeq<Nope>` | `const std::vector<Nope>&` | not declared | lowered |
     | `TSeq<Color>` (enum) | `const std::vector<Color>&` | not declared | lowered |
     | `TSeq<Top>` (the DUT) | `const std::vector<Top>&` | not declared | lowered |
     | `queue<Nope>` | `harc_rt::HarcQueue<Nope>&` | not declared | lowered |

     The element position takes a DECLARED RECORD and nothing else —
     transactions and structs are emitted as C++ structs of the same
     name. Not even the DUT: `TSeq<Top>` pastes `Top`, while the struct
     that exists is `VTop`. The DUT exception belongs to the handle
     position only, and assuming it extended inward would have been the
     next variant of this same mistake.

     **The comment corrected in 118 was wrong again, and worse.** It
     said the extern path had "since GAINED an arity check" so only the
     component path still arrives mis-counted, witnessed by
     `axil_write(t.value)`. Every part of that fails:

     - `axil_write(t.value)` does not lower — the component-method arity
       check added five commits EARLIER in this same sweep rejects it,
       so the sentence was false the day it was written;
     - it is positional, so it can never reach the branch in question,
       which `continue`s on anything that is not a named argument;
     - the reachability argument is about ORDER, not about which callers
       check arity: every caller that checks does so AFTER calling this,
       so mis-counted calls arrive from both paths. The real witness is
       `ref_add(b = 2, a = 1, 3)`;
     - "two callers" was inherited from the claim being corrected. There
       are fourteen.

     Two further copies of the same dead claim were sitting in
     `components_impl.rs` and in `tests/tbir.rs` — the latter
     contradicting its own assertions eight lines below it. A grep for
     the worked example would have found all three; correcting one and
     not searching for the others is how a false sentence survives being
     noticed.

     Sixth variant, and the honest summary of the series: every one of
     these was a claim that was true when written and never re-run. The
     defect is not carelessness about the facts, it is treating a
     comment as a record of a conclusion rather than of a measurement
     that has an expiry date.

120. **A feature and a set of mistakes sharing one verdict (2026-08-19).**

     The `connect` endpoint arm refused every edge whose source or sink
     was a single segment, and its note said why in a sentence that also
     named the exception: *"a single-segment endpoint resolves against
     the owner's own hookable / `out event` and works"*. That was
     recorded as a reason the verdict had to stay vague — "one site,
     three outcomes" — rather than as a feature to implement.

     It is a feature, in both directions, and v1 genuinely WIRES it:

     ```
     own_ev -> sb.write_obs
       → env.own_ev.push_back([&](auto _t) { AnalysisSb_write_obs(env.sb, _t); });

     source.observed -> own_sink
       → env.source.observed.push_back([&](auto _t) { AnalysisEnv_own_sink(env, _t); });
     ```

     Both compile. tbir now lowers both, and its wiring lines are
     byte-identical to v1's — verified by diffing the emitted
     `push_back` sets, not by reading them.

     The EXECUTING layers needed no changing, which is most of the
     point: `src_path`/`sink_path` are relative to the owning scope, so
     the empty path already meant "the owner" —
     `resolve_component_path_mode` returns the start component untouched
     for it, and the emitter chains it onto the instance prefix. The gap
     was one `len() < 2` guard rejecting a shape those layers already
     handled.

     "Nothing downstream needed changing" was the first draft of that
     sentence, and it was false in the way this file keeps having to
     record. Three of the four `ConnectEdgeSchema` doc comments said
     "sub-component", which the change falsifies; and `display.rs`
     rendered these paths with a naive `join(".")`, so `dump-ir` printed

         connect .own_ev -> sb.write_obs (method) (c1)

     — the exact leading dot `endpoint_label` had just been written to
     prevent, in the one renderer the change did not look at. Adding a
     helper does not fix the sites that never call it; `dump-ir` is also
     the tool this sweep uses as its own verdict oracle, so the bug was
     in front of it the whole time.

     **Two things this did NOT license.** Splitting a feature out of a
     mixed arm routes new shapes into the neighbouring arms, and those
     were measured on different inputs:

     - A single segment naming something that is neither a hookable nor
       an event — a sub-component field, say — reaches the arm that
       already covered the sub-component spelling, and takes its
       `NotImplemented { EmitsUncompilable }`. A first pass carved it out
       to `Unsupported`, reasoning that `EmitsUncompilable` "would be
       false half the time" because an uninstantiated env compiles. True,
       and true of every shape that arm already covered, so it justified
       nothing — and the split fell in the wrong place besides, leaving
       an owner-relative non-`hookable` method on the other side of it.
       Measured across all four (instantiated; every one compiles
       uninstantiated): `-> source`, `-> own_scalar`, `-> own_fn` and
       `-> sb.count` all get the same g++ refusal. Whether that arm
       should be `EmitsUncompilable` at all is a question about the
       family, answered where the arm is.
     - The testbench-owned `connect` resolver gets `None` for the owner
       id, because a testbench is not itself a component in the table.
       The first draft called that form "unmeasured", which was doing
       the same work as calling it impossible. Measured: v1 implements
       it — a testbench declaring `own_ev : out event<uint<8>>` with
       `own_ev -> direct.accept` emits
       `_tb.own_ev.push_back([&](auto _t) { TbSink_accept(_tb.direct, _t); });`
       and compiles. tbir refuses earlier, on an unrelated pre-existing
       gap ("testbench field `own_ev` with a non-scalar, non-named
       type"), so `None` is unobservable rather than right. The fix is
       to give a testbench an id, and it belongs with that field gap.

     The placement rule the old note recorded still governs everything
     else, and this round re-measured it across six malformations × two
     placements × both backends: instantiated, v1 emits the path
     verbatim and g++ refuses; uninstantiated, v1 emits no wiring and
     succeeds. Uniform. That is why the `--codegen v1` suggestion stays
     on the genuinely malformed edges — it is true somewhere — and it is
     the same evidence, now separated from the feature it was tangled
     with.

121. **"No hazard here" is a reason not to warn, not a reason to refuse
     (2026-08-19).**

     `idle(n = 2)` and `quiesced(n = 2)` were `Unsupported`, under a note
     that worked the case out correctly and then stopped one step short:
     the arity check above has already established exactly one argument,
     so v1's name-dropping positional binding "lands the value in the
     only slot there is and emits code identical to the positional form
     … it emits the predicate correctly". Every clause of that describes
     something to implement.

     The reasoning it came from was about a different question. The
     general named-argument guard splits three ways — a name at its own
     position (inert), a REORDERED name (a silent swap, and the hazard
     the guard exists for), and a name matching no parameter. These
     predicates take one argument, so no reordering is expressible, and
     the note recorded that as "they keep `Unsupported`". Absence of the
     hazard is a reason the verdict is not `SilentlyMisLowers`; it was
     never a reason to refuse, and nothing anywhere argued for the
     refusal itself.

     Implemented, and pinned by the property that made it safe rather
     than by a remembered string: the named and positional spellings
     must emit the SAME program. They do — the whole emitted file is
     byte-identical, under both backends, for both predicates.

     The name is still checked against nothing, and that half of the old
     note stands unchanged: no parameter name is stated anywhere for
     these (the arity diagnostic says "exactly one cycle-count
     argument"; the docs write `idle(N)`, a value placeholder), so there
     is nothing to check against and inventing one would be the
     `record_write` mistake. v1 accepts `nosuch = 4` here too, and emits
     the same predicate.

     **Two neighbours were left alone, deliberately.** The probe that
     found this also re-opened `p.two(bogus = 1, 2)`, which is `Invalid`
     while v1 compiles it and emits the correct positional call — the
     shape of a false `Invalid`. It is not one: the note there had
     already considered `SilentlyMisLowers` and refuted it in writing,
     because v1 emits exactly the right code and claiming otherwise
     would be a false explanation. `Invalid` rests on "no backend can
     HONOUR it" rather than "no backend runs it", and re-deciding a
     question already measured and written down is the mistake
     divergence 120 was about.

     The arity arms reject genuine errors — but this entry first added
     "likewise the arity arms", certifying them as fine, and they were
     not. Both carried `Unsupported`, promising a `--codegen v1` escape
     hatch neither has, and they do not even share a verdict. Measured:

     | call | v1 |
     |---|---|
     | `p.idle()`, `p.idle(1, 2)` | its own error, "expected 1 cycle-count arg, got N" — **rejects** |
     | `p.quiesced()`, `p.quiesced(1, 2)` | falls through `resolve_component_quiesced_predicate` (`None` on a wrong count) to the generic method shape, emits `if (!(_tb.p.quiesced()))` — g++: no such member |

     So `NotImplemented { Rejects }` and
     `NotImplemented { EmitsUncompilable }` respectively. Blessing two
     sites because they sat next to the one being fixed, and pinning
     that with a test, is worse than leaving them unexamined: the test
     made the wrong verdict load-bearing.

122. **A refusal worth keeping, and the measurement that says so
     (2026-08-19).**

     Five sites refuse `pop()` in a nested expression — scoreboard,
     testbench, component and target-state queues (two spellings) — each
     with the same one-line reason, "bind it to its own `let` first —
     `pop` mutates the queue". v1 compiles every nested form, which is
     the profile of an implementable gap, and the previous two batches
     had both turned out to be exactly that.

     This one is not, and the difference is worth recording because the
     surface looks identical. Lowering an expression-position call means
     hoisting it into a statement ahead of the expression. For a MUTATING
     call that is equivalent only when the surrounding expression
     evaluates it unconditionally, exactly once — and C++ short-circuits:

     | | |
     |---|---|
     | source | `assert (guard == 1 && sb.q.pop() == 7) \|\| sb.q.size() == 1` |
     | v1 emits | `if (!((guard == 1 && _tb.sb.q.pop() == 7) \|\| _tb.sb.q.size() == 1))` |

     With `guard == 0` the pop never runs and the queue keeps its
     element. A hoisted lowering would pop first, empty the queue, and
     fail an assert v1 passes — a silent behavioural divergence, the one
     outcome worth refusing a program over.

     Repeated evaluation is an obstacle at ONE of the two looping
     constructs, and this entry first claimed it was neither.

     `while` is fine: `lower_while` opens the header block before
     lowering the condition, so a hoisted call lands IN the header and
     the back-edge targets it. Measured on `while tick() < 3` over a
     TESTBENCH METHOD — `b1` holds the inlined body
     (`TbFieldWrite(_tb.n, (_tb.n + 1))`, `Assign(%__t0, _tb.n)`) and
     `b5` jumps back to `b1`. The distinction matters: a pure file-scope
     helper does not CFG-inline and is not a witness for this.

     `wait until` is not fine. `lower_wait_until` lowers the predicate
     into the block PRECEDING the terminator, so a hoisted call runs
     exactly once — while v1 emits the predicate as a lambda,
     `wait_until_timeout(_slot, [&]{ return _tb.sb.q.pop() == 7; }, …)`,
     and re-runs it every cycle. That same function already says so
     thirty lines up, about transactor edges: "hoisting would run it
     exactly once, the wrong semantics".

     So "it's in a loop" is the wrong objection at one construct and
     exactly the right one at the other, and implementing this needs
     BOTH: `&&`/`||` lowered to branches when a side-effecting call sits
     under them, and a `wait until` predicate that re-evaluates rather
     than hoists. `wait until sb.q.pop() == 7` contains no short-circuit
     at all, so the first alone would never reach it — the scoped work
     as first recorded was not just incomplete but insufficient for the
     case that motivated it. Recorded as scoped work rather than
     attempted.

     The five copies of the reason are now one function carrying the
     measurement, and a test pins v1's short-circuited output — so the
     obvious "fix" fails a test that explains itself rather than
     silently changing what a program means.

123. **Three misdirections in the record-traversal loop (2026-08-19).**

     Three arms of the record field-chain walk told the user to re-run
     with `--codegen v1`, and v1 emits code g++ refuses. Measured, each
     against controls that compile under BOTH backends
     (`r.kids[1].p`, `r.inner.p`, `r.data[2]`, `s.kids = r.kids`):

     | source | v1 |
     |---|---|
     | `r.data[0].p` — field on a scalar element | emits, g++ refuses |
     | `r.kids.p` — traverse a `Vec` with no index | emits, g++ refuses |
     | `r.n.p` — field on a scalar | emits, g++ refuses |

     `NotImplemented { EmitsUncompilable }` now, matching `r.n[0]` — the
     same family, one site over, which already carried that verdict.
     Following the in-file precedent rather than inventing one.

     One message also stopped being a sentence: "field `Rec.n` is not a
     nested record; cannot access `.p`" reads as nonsense once spliced
     into "HARC does not implement … yet", so it is a noun phrase now.

     **The mutation round is the part worth recording.** The first three
     mutants all reported CAUGHT, and all three were vacuous: deleting a
     `V1Status` argument from `not_implemented` leaves a two-argument
     call to a three-argument function, so the mutant fails to COMPILE,
     and a harness that scores "tests not green" counts that as caught.
     Rewritten to flip `EmitsUncompilable` → `Rejects` — which compiles
     — two of the three survived. Only the `Vec`-traversal arm had a
     test; the other two sites had none, and the vacuous run would have
     shipped that.

     A mutant that does not compile tests the type checker, not the
     suite. The harness cannot tell the difference, so the check has to
     be that the mutation is a legal program — which is the same
     discipline as running a compiling control beside every failing
     cell, applied to the tool instead of the fixture.

124. **Whole-`Vec` equality, and why the permission is an allow-list
     (2026-08-19).**

     `assert r.data == s.data` was refused with every other whole-`Vec`
     read, under a note that already said the comparison works:
     "`assert r.data == r.data` emits `r.data == r.data`, which compiles
     and works (`std::array` has `operator==`)". It does, for record
     elements too — v1 generates `operator==` for the element struct, so
     `Vec<Kid, N>` compares element-wise. One site, several outcomes, and
     splitting them is the better answer than labelling the site with
     whichever outcome is true somewhere.

     The IR needed nothing: `IrType` has no `Vec` variant, but the read
     lowers as an ordinary `RecordField` with `index: None`, the verifier
     accepts it, and the emitter prints `r.data == s.data` — the same
     text v1 emits. Confirmed by permitting the read globally as an
     experiment and compiling the result.

     **That experiment is also what settled the shape of the fix.** With
     the read permitted everywhere, `let d = r.data` and `${r.data}` both
     emit C++ that g++ refuses. So the permission is keyed on the LANDING
     and defaults to off: a landing nobody enumerated keeps today's clean
     diagnostic. The inverse — permit at the read, block the known-bad
     landings — would mean a landing nobody enumerated silently emits
     code that does not build, and there is no way to check by inspection
     that the list is complete.

     Two things the first draft got wrong, both caught by running rather
     than reading:

     - **It regressed mismatched operands.** Permitting the read on any
       equality let `r.data == s.kids` (scalar elements against record
       ones) and `r.data == s.n` (a `Vec` against a scalar) through,
       where both backends emit a comparison g++ refuses. The read used
       to catch those, so the permission had to carry the pairing check
       itself — same length, same element type.
     - **It broke regblock lowering.** The pairing helper calls
       `try_record_field_chain` on every `==` operand, and propagated its
       error with `?`. A regblock access like `regs.DMACR.RS` makes that
       resolver fail, so a working program became a hard error before its
       real lowering path ever saw it. Errors are swallowed now: "not a
       matching pair" is the only answer this question can give.

     The second one surfaced as five failing tests, three of them in
     regblock corpora that have nothing to do with `Vec` — a reminder
     that a helper called speculatively on every operand of a common
     operator is not a local change.

     Two tests that pinned the old refusal were rewritten rather than
     deleted. One had justified it with "it only worked by luck"; the
     site's own note said `std::array::operator==` is why it works, and
     the measurement agrees with the note.

125. **One landing, three lanes: what "gated" turned out to mean
     (2026-08-20).**

     Divergence 124 permitted a whole-`Vec` record-field read in the
     `==`/`!=` landing and called the job done. It had gated ONE of the
     three sites that refuse such a read. `grep vec_read_ok src/` showed
     a single gate; the other two never saw the flag. Measured:
     `assert a.data == b.data` inside an agent method, and
     `assert ba.data == bb.data` inside a bound responder's thread, were
     both still refused, while v1 emitted `self.a.data == self.b.data` /
     `target.ba.data == target.bb.data` and g++ accepted each with zero
     errors.

     The three lanes spell the same landing and are three functions: a
     record LOCAL (`try_record_field_chain`), a bound responder's record
     STATE field (`as_transactor_state_record_field`), and a COMPONENT
     record field (`as_component_record_field`). Nothing about the first
     one said "there are two more"; the flag's own doc comment described
     the landing, not its coverage. **A permission is not implemented
     until every site that refuses the thing consults it** — and the way
     to know is to grep for the flag and count the refusals, not to read
     the one you edited.

     The fix also had to change the shape of the component resolver.
     `as_component_record_field` served BOTH the read and the write path
     and erred on a `Vec` leaf itself, which hid the leaf's shape from
     the pairing check that needs it. It now REPORTS the shape and each
     caller judges: the write refuses unconditionally, the read consults
     the flag. Reporting rather than judging is what let one resolver
     serve two callers that disagree.

     Verdicts corrected on measurement, all `Unsupported` →
     `NotImplemented { EmitsUncompilable }`:

     - the two remaining whole-`Vec` read refusals (`let d = a.data`
       gives "cannot convert `std::array<…, 4>` to `int64_t`" from v1's
       own output);
     - the component walk's nested-record arm, split into the same two
       arms the record-local walk already had ("traversing the `Vec`
       record field … without an element index" vs "… which is not a
       nested record"), with the same wording — copied, not
       reconstructed.

     One verdict was deliberately left alone and should not have been.
     The component whole-`Vec` WRITE kept `Unsupported` on the reasoning
     that `a.data = b.data` compiles under v1, so the escape hatch is
     real — while the same arm also covers `a.data = 5`, which v1 emits
     and g++ refuses. "Mixed, so keep the weaker claim" is not a rule
     the repo has: `Unsupported` is the STRONGER claim, and divergence
     126 splits the arm instead.

     Two smaller things, same root:

     - **A detail string printed its own placeholders.** The state-field
       refusal's detail was a plain `&str` containing
       "`{field}.{vec}[i]`" — no `format!`, so the braces reached the
       user verbatim. The identical bug had just been fixed one site
       over. A message written by copying a `format!`-shaped sentence
       into a non-`format!` slot fails silently in both directions.
     - **The pairing check answered on spelling, not on path.**
       `(r.data) == s.data` was refused because `dotted_path` sees
       through parentheses and two of the three resolvers do not. The
       oracle peels parentheses first, so the answer is a property of
       the path.

     The regression guard for divergence 124's critical bug is now a
     test rather than a note: a testbench `function` CFG-inlines into
     its caller, so `assert r.kids[bump()].p == 0` emits `bump()`'s body
     exactly once — the count that fails the moment the pairing check
     starts lowering its operands again. Eleven mutants over the guards
     in this round, all caught; the harness itself had to be fixed
     first, because `cargo test` prints "error: test failed" to stderr
     and a NOBUILD check keyed on `"error: "` scored every CAUGHT mutant
     as unbuildable.

126. **A pairing rule that asked the wrong question, and the three
     writes behind it (2026-08-20).**

     Divergence 125 paired whole-`Vec` equality operands on `IrType`
     equality. That is not the question. Both backends declare a
     `Vec<T, N>` record field as `std::array<elem, N>` and collapse
     every unsigned scalar of 64 bits or fewer to `uint64_t`, so
     `Vec<uint<8>, 4>` and `Vec<uint<32>, 4>` are the SAME C++ member
     and compare element-wise fine. Pairing on `IrType` refused that
     program — and because 125 had just flipped the read refusal to
     `EmitsUncompilable`, it refused it while telling the user no
     backend runs it. Measured: v1 emits `r.data == s.eight` with both
     members `std::array<uint64_t, 4>`, g++ 0 errors.

     **The rule was already written down, one file over.** The whole-
     `Vec` WRITE arm in `stmts.rs` carried a comment spelling out the
     `uint64_t` collapse in full, as the reason it kept `Unsupported`.
     It was read during 124 and reconstructed anyway. The predicate now
     lives in `cpp_tb::ir_vec_elem_class`, beside the
     `cpp_uint_for_width` / `cpp_sint_for_width` / `scalar_leaf_c_type`
     family it has to agree with, and BOTH the read pairing and the
     write shape check call it. Two things follow from having one
     predicate:

     - `Vec<uint<32>, 4> = Vec<uint<16>, 4>` is a copy, not a refusal —
       the gap the old comment described but did not close.
     - With that landing gone the write arm is no longer mixed, and it
       loses `Unsupported`: a length mismatch, a signedness split at or
       below 64 bits, a record-vs-scalar element and a scalar RHS each
       have v1 emitting an assignment g++ refuses (one error apiece,
       measured on all four).

     The same "one landing, three lanes" sweep 125 claimed then had to
     be finished for real:

     - the responder-state lane's mid-segment traversal arm still said
       `Unsupported` (v1 emits `target.ba.kids.p` / `target.ba.n.p`,
       g++ refuses each);
     - the whole-`Vec` WRITE now lowers in all three lanes, through one
       `whole_vec_copy_rhs` helper that turns the read permission on for
       the RHS — the responder-state and component spellings each emit
       what v1 emits and compile. (The record-local lane was left
       resolving its own RHS with `try_record_field_chain`, which sees
       record LOCALS only; divergence 127 finishes that.);
     - component-record `Vec` ELEMENT access lowers, reusing the
       `ComponentVecElement` / `ComponentVecElementWrite` nodes a fixed-
       vector component FIELD already had. It was carrying two false
       verdicts: `let z = a.data[0]` claimed `EmitsUncompilable` and
       `a.data[0] = 1` claimed `SilentlyMisLowers`, the loudest verdict
       in the enum, while v1 emitted `self.a.data[0]` and g++ accepted.

     That last one closed a loop rather than a gap. The whole-`Vec`
     diagnostics say "index the field element-wise (`X[i]`)", and in the
     component lane `X[i]` did not lower — the suggestion sent the user
     to a second refusal. There is a test whose whole purpose is to
     forbid that; it only exercised the write arm, so the two read
     details added in 125 walked straight past it. It now follows its
     own advice and checks the suggested spelling lowers.

     **And the suggestion has to be typeable.** `chain.dotted` is rooted
     at the record TYPE, so "index the field element-wise
     (`Bundle.data[i]`)" handed back something that does not parse. The
     chain carries the user's own spelling now (`r.data[i]`) for
     details, and keeps the record-rooted one for construct names, where
     naming the field by its record is the point. The predecessor of
     this bug was the same sentence in a plain `&str` slot printing
     `{rec}.{field}` braces and all — one detail string, two ways to
     hand the user a path they cannot use.

127. **Two silent miscompiles behind one convenience (2026-08-20).**

     Divergence 126 lowered component-record `Vec` element access by
     reusing the `ComponentVecElement` / `ComponentVecElementWrite`
     nodes a fixed-vector component FIELD already had. The node's
     `field` is a member SUFFIX, and for the new lane it is DOTTED
     (`a.data`). Every consumer resolved it with
     `fields.iter().find(|f| f.name == *field)` — a lookup that can
     never match a dotted name. Width came back unknown, signedness
     `false`, type `None`.

     None of that surfaced as a diagnostic. It decided:

     - `>>` between an ARITHMETIC and a LOGICAL shift. With a
       `Vec<sint<32>, 4>` element holding `-8`, v1 answers `-4` and tbir
       answered `9223372036854775804`.
     - whether a >64-bit element was truncated to `uint64_t` before use.
       With a `Vec<uint<128>, 2>` element holding `1 << 100`, v1 answers
       `1 << 99` and tbir answered `0`.
     - whether the write guards could see a record element at all. The
       record-local element write carries a matched PAIR of guards — a
       scalar-leaf check and a record-leaf check — and the new lane
       copied one of them, which was inert anyway because it routes
       through the same broken type lookup. `a.kids[0] = 5` emitted
       `self.a.kids[0] = 5;`, which g++ refuses, for a program that had
       been cleanly refused before the lane existed.

     Both files compiled. **A reused node is not a free implementation:
     the reuse changed what its key field can contain, and every reader
     of that field was part of the change.** The type walk lives in one
     helper now (`component_vec_elem_type`), and `record_id_of_expr`
     answers through `expr_type` so the two cannot disagree.

     Two more false verdicts came out of the same "resolvers stop at
     `[`" fact, in the opposite direction. An element selection INSIDE a
     component-record or responder-state path (`ba.data[0]`,
     `c.b.tbl[0].data`, `ba.kids[0].p`) resolves in neither lane, so it
     fell past every one of them onto whichever generic arm caught the
     leftovers: "index expressions" (`EmitsUncompilable`), "assignment
     to a target that is neither a DUT port nor a local"
     (`SilentlyMisLowers` — the loudest verdict in the enum), "field
     access on a non-DUT value" (`EmitsUncompilable`). v1 compiles the
     whole family, measured at 0 errors each. Those arms cover much
     else, so the answer comes BEFORE them, as one precise `Unsupported`
     — which is true, and names the gap instead of mislabelling it.

     And the write pairing was still asking the wrong resolver.
     `r.data = c.a.data` — a record local copied from a component record
     field — was reported "non-matching RHS" under
     `EmitsUncompilable`, because that arm resolved its RHS with
     `try_record_field_chain`, which sees record locals only. v1 emits
     `r.data = c.a.data;` and compiles. Every write lane now goes
     through `whole_vec_copy_rhs`, which lowers the RHS ONCE and asks
     the resulting IR for its shape rather than the AST — so an indexed
     RHS (`r.tbl[0].data`) pairs too, instead of being refused for its
     spelling. The read-side pairing still has to ask the AST, because
     both operands are lowered again afterwards; the two positions get
     different mechanisms for a reason, and the comments say which.

     Smaller, same root: `spelled` was built from field names alone, so
     `r.tbl[0].data` suggested `r.tbl.data[i]` — a DIFFERENT field,
     refused by a different diagnostic. It carries the selection now
     (`r.tbl[…].data[i]`), as a template rather than a paste.

128. **One arm, two situations, and only one of them reachable
     (2026-08-20).**

     Three transactor paths carried the same refusal:
     `ComponentItem::Lifecycle(..) | ComponentItem::Apply(_)` →
     "lifecycle/apply items", `Unsupported`. Two different facts had
     been merged into one verdict, and the `Unsupported` was wrong about
     both.

     A lifecycle block never reaches lowering. `parser.rs` refuses
     `setup`/`check`/`teardown` in any component that is not a
     `testbench` ("lifecycle blocks are currently supported only inside
     `test`/`impl` and `testbench`"), so neither backend sees one on a
     transactor — measured, both print that same parser error byte for
     byte, and `TransactorDecl` is constructed at exactly one place with
     no desugaring pass that could build the item another way. The arm
     was offering `--codegen v1` for a program that does not parse
     anywhere. It is `unreachable!` now — which is what the sibling arm
     20 lines up in the same file already said about the same
     impossible state. The first version returned a user-facing
     `Invalid` instead: one fact, two treatments, and a diagnostic
     nothing can ever print.

     `apply` does reach it, and v1 does not implement it: `cpp_tb` has
     `ComponentItem::Apply(_) => {}`, so v1 emits the file with no trace
     of the aspect — and without resolving the name, so `apply Whatever`
     naming nothing at all emits clean. That is exactly the `connect`
     arm's situation one step above, which already carried
     `SilentlyMisLowers`; the `apply` arm takes the same verdict.

     **A control is what tells you whether you measured the thing you
     meant to.** The first initiator-side probe reported v1 REJECTING —
     which would have made the verdict `Rejects` — and the reject was
     "transactor instantiation requires a mode annotation", nothing to
     do with `apply`. The same file with the `apply` line deleted failed
     identically. Every case in the test now runs its own no-`apply`
     control first.

130. **Probing an arm is not measuring it (2026-08-20).**

     Five refusals in the transactor state-field lowering were probed
     one landing each and labelled from that one probe. Two came out
     wrong, and both were wrong in the direction that matters: an
     `Unsupported` pointing users at a backend that silently changes
     what their program means.

     The guards admit more than the probes covered:

     | guard | probed | also admits |
     |---|---|---|
     | `f.direction.is_some()` | `out event<T>` — v1 emits a real subscriber vector | a directional SCALAR — v1 emits `uint64_t p;`, the direction DROPPED, and it compiles |
     | `tb_scalar_field_ir_type(..) == None` | `Vec<uint<8>, 4>` — v1 emits a usable `std::array` | `stream`/`buffer` — v1 emits a bare `uint64_t`, which compiles and is not a stream; and enums, which do not compile at all |

     The non-event and non-scalar arms take `SilentlyMisLowers`, because
     an arm's verdict is the worst thing v1 does anywhere under it. The
     directional arm was SPLIT rather than relabelled — see below, where
     the event half turned out to be mixed too.

     **The directional split already existed in this file.**
     `lower_unbound_item` separates the event half from the scalar half
     of the identical `f.direction.is_some()` test, with the scalar half
     already labelled `SilentlyMisLowers` and the reason written out. It
     was copied this time instead of re-derived. That is the third
     occasion in this sweep where the rule was already stated somewhere
     in the repo and got reconstructed from one example instead.

     Two more things the one-probe method hid:

     - **The two `default` arms are mixed.** `default 0` gives
       `HarcQueue<uint64_t> q = 0;` and g++ refuses it, but
       `format_simple_expr` pastes a bare `Ident` verbatim, so
       `default q0` naming another queue field compiles. The label is
       `EmitsUncompilable` — the worst under the arm — where the tree
       this lands on had `Unsupported`.
     - **`lower_state_field` has four callers**, and every message said
       "bound-to transactor" — including on the unbound DUT-poking form,
       which is bound to nothing, and on the two initiator-side paths,
       whose sibling arms in the same functions say "initiator-side
       bound-to transactor". A `StateFieldOwner` label now travels with
       the call.

     And the generic-applied justification was false twice over. A
     record declaration CAN take generic parameters — `parse_transaction`
     calls `parse_optional_generic_params`; only `struct` cannot — and
     "the file compiles" does not hold for the regblock-mirror landing,
     where v1 emits `VDmaRegs* b = nullptr;` and g++ refuses. The label
     survives (`SilentlyMisLowers` outranks `EmitsUncompilable`), but a
     wrong measurement recorded as a reason not to re-measure is worse
     than no comment at all.

     Method, stated so the next batch inherits it: **work out what the
     GUARD admits before labelling the arm.** One probe measures one
     landing. A control — the same program with the field removed —
     goes with every row; the control caught an invalid `enum` in the
     probe skeleton that had every row, including itself, reporting "v1
     rejects".

131. **The event half was mixed too, and the fix was unreachable
     (2026-08-20).**

     Divergence 130 split the directional guard and kept `Unsupported`
     for the event half, with a comment saying it had been measured. It
     had been measured on ONE landing — `out event<uint<8>>` — which is
     the method 130 exists to condemn, applied inside 130 itself.

     The event guard says nothing about the payload type or about
     `default`, and both vary:

     | landing | v1 |
     |---|---|
     | `event<uint<8>>`, `event<uint<128>>`, `event<Beat>` | subscriber vector, 0 g++ errors |
     | `event<Color>` | v1 emits no C++ enum at all, so the payload name in the signature is undeclared — 5 errors |
     | `event<T> default 0` | pasted into the vector's initialiser — 1 error |

     `Unsupported` now holds only where v1 declares the payload AND
     there is no default. `record_ids` is the "does v1 declare this
     type" test, which is the same map the `queue<Record>` lowering
     already uses.

     Reading the payload took a second correction: `event<Color>`
     arrives as `TypeArg::Expr(Ident)`, not `TypeArg::Type`, so the
     first check silently passed the enum through and the probe still
     reported `Unsupported`. A guard that cannot see its input is
     indistinguishable from one that approves of it.

     **And the split was dead code on three of its four call sites.**
     Both initiator-side paths tested `f.direction.is_some()` and
     refused two lines above the call, so a directional scalar kept a
     blanket `Unsupported` there while v1 dropped the direction and
     compiled — the exact defect 130 opens by describing, still live in
     the same function. Adding a rule is not the same as reaching the
     code that needs it. (Two of the three pre-checks were deleted at
     this point; the third survived another round — see 132.)

     Smaller, same round: the non-event arm called itself "scalar" while
     catching `in Vec<…>` and `in Beat`; the `uint<N>` wider than 64
     bits landing was missing from the non-scalar enumeration, and it is
     one where v1 is CORRECT, so the reason now says so; the generic
     arm's reason said "a struct type" for an arm spanning transactions
     and regblock mirrors; and `lower_state_field`'s doc comment had
     been orphaned onto the new `StateFieldOwner` enum, taking three
     false claims with it (a transaction-typed field is not out of
     subset, there is no `guard`/`reset` clause to reject, and
     `record_ids` is never empty).

132. **A blacklist that grew three times, and the pre-check that was
     never counted (2026-08-20).**

     Divergence 131 said "the pre-checks are gone, and
     `lower_state_field` owns the directional case for all three
     owners". It deleted TWO. The third, in `lower_unbound_item`,
     survived — so the unbound form kept the pre-split blanket
     `Unsupported` for exactly the two landings 131 had just measured as
     uncompilable (`event<Color>`, `event<T> default 0`). The sentence
     was false when it was written, and its own analysis ("dead on three
     of four call sites") said so.

     **The event guard is an allow-list now, after the blacklist grew in
     three consecutive rounds.** Each round added the landings the
     previous one had missed, and each time the next round found more:
     enum payloads, then `string` / bus names / transactor names, then
     `queue` / `stream` / dotted paths / multi-arg / `TypeArg::Named` /
     regblock mirrors — all silently FLATTENED by v1 to
     `void(uint64_t)`. Enumerating a blacklist means proving a negative
     over a space the parser keeps extending.

     What can be certified at that site is a single positional BUILTIN
     scalar payload — or none at all — with no `default`. That keeps
     `Unsupported`. An uncertified payload takes `SilentlyMisLowers`; a
     `default` on an otherwise-certified payload takes
     `EmitsUncompilable`, and the payload check has to answer FIRST,
     since a field can be under both.

     `event<Beat>` is over-refused by this: v1 declares the record and
     emits `void(Beat)` correctly. It is refused because `record_ids`
     **cannot tell a struct from a regblock MIRROR**, and the mirror is
     one of the flattening rows — a fact stated in a comment 70 lines
     below the guard that 131's doc comment nonetheless described as the
     "does v1 declare this type" test. Certifying a record payload needs
     a regblock set that does not reach this function. Over-cautious
     under a mixed arm is the repo's own worst-wins rule; an
     `Unsupported` promising v1 works where it silently does not is not.

     Two tests were ENTRENCHING the old blanket verdicts
     (`in event<uint<8>>` and `in event<Req>` asserted as `Unsupported`
     on the unbound path). A test that pins a wrong verdict is worse
     than no test: it makes the next sweep read the mistake as settled.

     Four rounds on five arms. The method that finally held was not a
     better enumeration — it was giving up on enumerating the bad cases
     and certifying the good ones instead.

133. **Round five: what an allow-list fixed, and what it did not
     (2026-08-20).**

     The event allow-list from 132 held. A 40-row sweep over builtin ×
     width × direction × owner × position found v1 correct on every
     certified row — the one part of this series that is finished. What
     did not hold was everything AROUND it, in the same three ways as
     every previous round.

     - **A regression.** Deleting 132's third pre-check let a
       directional MODULE handle through: `dut : in TlmReadInitiator`
       stopped being refused and lowered, with tbir dropping the `in`
       marker itself — byte-identical output to the undirected
       spelling. The delete removed a check that ran BEFORE the
       named-type branches; those branches then answered first. A
       directional field is dispatched to the shared rule explicitly
       now.
     - **The arm in FRONT of the fix was graded from a subset.** A
       field that is both defaulted and uncertified is under two arms,
       and the `default` one is graded a notch lower;
       `event<string> default ev2` therefore claimed "v1 emits C++ that
       does not compile" while v1 compiled it and flattened the payload.
       The payload check answers first now. This is structurally the
       same finding 130 made about the queue and record default arms —
       third occurrence.
     - **A rule the repo already stated, reconstructed wrong again.** A
       bare `event` with no payload was refused as "uncertified", while
       `lower_event_payload` says in as many words that it "defaults to
       an unsigned scalar" and v1 emits the SAME member it gives
       `event<uint<8>>`. Two spellings of one C++ member, opposite
       verdicts. Fourth occurrence of this specific failure.

     **Named and NOT fixed**, with the measurement recorded at the site:
     `record_ids` holds regblock MIRRORS, so `b : DmaRegs` is accepted
     as a value-record — tbir emits `DmaRegs b{};` and compiles, v1
     emits `VDmaRegs* b = nullptr;` and g++ refuses. `queue<DmaRegs>`
     is the same hole with both backends building different element
     types. Gating it needs a regblock-name set that does not reach the
     function. The event allow-list sidesteps that map rather than
     trusting it; these two arms still trust it.

     **Out of scope, also unfixed:** a FOURTH owner. A transactor
     carrying an `on` handler routes through `lower_field`
     (`components_impl.rs:1321` — an earlier draft of this entry named
     a `lower_component_field` that does not exist anywhere in the
     tree), whose four directional arms still answer a blanket
     `Unsupported` for landings where v1 drops the direction and
     compiles — the defect this whole series was opened to remove,
     three of them shipping an empty reason string.

     The suite could not previously catch a reinstated pre-check on the
     INITIATOR owner: there was no directional-field case there at all.
     The sentence this entry first carried — that the suite could not
     catch a reinstated pre-check at all — is false, and names the
     wrong owner besides: the check this round restored was on the
     UNBOUND owner, which already had two directional cases with their
     `V1Status` pinned, and reinstating it fails them. There is a case
     on each of the three owners now.

     Two of the tests added for this round did not test what they
     claimed, which a mutant showed and the assertions did not:

     - the both-arms event probe declared the SECOND field first, so
       lowering errored on the wrong field and the assertion was met
       under either guard order — swapping the guards back passed all
       553 tests;
     - the unbound directional row used a SECOND module handle, which
       the handle-count arm refuses before the directional dispatch is
       reached, so it passed whether or not the dispatch existed.

     Both now fail when their guard is removed. A test written in the
     same edit as its guard is not automatically a test OF that guard.

134. **A width cap written around a missing header overload
     (2026-08-22).**

     `w : uint<128>` as a declared field was refused at five call
     sites, naming `--codegen v1` as the way out. The refusal was real
     — v1 emits `_harc_u128 w` and builds — so this was one of the
     thirteen measured gaps where v1 works and TB-IR does not.

     Closing it needed three separate things, and only the first was
     the one the gap list named.

     - **The width rule.** `tb_scalar_field_ir_type` capped at 64 and
       decides for FIVE sites: the testbench field declaration, the
       testbench field DEFAULT (an `else if let Some(..)` with no
       guard of its own — a type the gate rejects is silently dropped
       there rather than diagnosed, so the two cannot move
       separately), the promoted test-scope `let`, the component-hosted
       target-state filter, and the transactor state field.
       `components_impl.rs` capped at 64 through `scalar_ir_type`,
       which ALSO decides event payloads and fixed-vector elements —
       widening that one function would have widened both of those
       unmeasured, so the declared-field subset is now its own
       function. `scoreboards.rs` held a third copy of the same
       decoder, differing from `decoded_scalar_ir_type` in exactly its
       name and its width line — an earlier draft of this entry called
       the two byte-identical, which they were not; it is deleted, not
       edited.
     - **The emitters.** Four of them — transactor state, scoreboard,
       component, testbench — each carried its own
       `(bool | int64_t | uint64_t)` triple, while the queue-element
       emitter already went through `field_scalar_cty`, which knows
       `_harc_u128` and `HarcWide<N>`. A wide type reaching any of the
       four would have produced a 64-bit member that compiles, runs,
       and drops the top half. No typecheck can see that, which is why
       the member type is asserted directly.
     - **A missing runtime overload, in BOTH backends.** The cap was
       very nearly set at 128 instead of 1024, because above 128 a
       field is declared `HarcWide<N>` and `w = w + 1` on one was
       rejected: "ambiguous overload for `operator+` (operand types
       are `harc_rt::HarcWide<32>` and `int`)" — `HarcWide` converts
       to `uint64_t` and to `_harc_u128` equally well. That reproduces
       on clean `main` for a plain wide LOCAL, with nothing to do with
       fields: `let w : uint<1024> = 1; w = w + 1` lowered and emitted
       C++ that nobody could build, in tbir and in v1 alike. The rule
       was already stated twice in the tree — `operator==`/`operator!=`
       carry the mixed HarcWide/integer form, and `harc_wide_negate`
       writes `(~value) + HarcWide<N>(1)` by hand. A cap at 128 would
       have written a language limit around a header omission.

       That fix took two review rounds. The first version defined ALL
       TWELVE operators; the second still defined six of them across
       widths. Both times the defect was the same one, and both times
       it was found by measurement rather than by reading:

       * `/ % < > <= >=` are **not** the rule `operator==` states.
         Equality is sign-agnostic; ordering and division are not, and
         every `HarcWide` implementation of the six is UNSIGNED.
         `expr.rs` emits a bare `<` for a `sint` compare (the only
         signed-wide path, `harc_wide_slt`, is reached from covergroup
         lowering alone), so defining them turned "this program cannot
         be built" into `w < 0` quietly answering false on a negative
         `sint<1024>` — a loud correct diagnostic traded for a silent
         wrong answer, in `--codegen v1` as well, since the header is
         shared. They are undefined now, refused at lowering by
         `reject_unbuildable_wide_operator` with the
         `EmitsUncompilable` grade v1 measurably earns, and
         `tests/wide_mixed_ops_cpp.rs` pins their absence so nobody
         closes the ambiguity error by adding them back.
       * `HarcWide<N>(v)` for a NEGATIVE `v` zero-filled above bit 128
         instead of sign-extending, so `w + (0 - 1)` answered 2^128
         while `w - 1` — the same arithmetic — answered 0. The mixed
         operators routed every negative operand through it.
       * The ambiguity was only half retired: two wide values of
         DIFFERENT widths deduce no `N` either. Reachable straight
         from source once fields could be wide (`a : uint<160>`,
         `b : uint<256>`, `b = b + a`), and `LowersUncompilable` in
         both backends.

       The second round's fix for that last one was to define the six
       across widths too, widening the narrower side with
       `harc_wide_zext`. That is the SAME defect a third time, and the
       sharpest instance of it: **sign-agnosticism does not survive a
       width change.** `+ - * & | ^` give one N-word answer for signed
       and unsigned alike only while N is fixed; widening is exactly
       where the sign matters, and the C++ type does not carry it. So
       `b + a` for a negative `sint<160>` answered `b + (2^160 - 1)`
       while `b + (-1)`, through the integer overload directly beside
       it, answered correctly — two halves of one macro disagreeing
       about one value, in v1 as well.

       Both shapes are refused at lowering now, by
       `reject_unbuildable_wide_operator`, with the grade v1
       measurably earns. `==`/`!=` are the exception and are NOT
       refused across widths: they carry their own `<A, B>` form, and
       a first version of the refusal blanket-covered them, refusing a
       program v1 builds under a label the measurement contradicts.

       NAMED and not fixed: that `<A, B>` equality compares raw words,
       so two SIGNED values of different widths compare unequal when
       both are -1. Both backends agree on the wrong answer; it
       predates the declared-field widening.

       None of these could be caught by a typecheck — most compiled
       and computed the wrong number — so the operators are gated by a
       probe that is built AND RUN (`tests/wide_mixed_ops_cpp.rs`), in
       the style of `wide_cast_cpp.rs`.

     - **FOUR statement positions consume a value through a
       SYNTHESIZED comparison or conversion**, so the binary-operator
       guard never sees them, and all four were left behind when the
       field gate widened. `for i in 0 .. w` builds its `i <= hi`
       header in `control.rs`; tbir LOWERED it, silently iterating the
       low 64 bits of a 1024-bit bound through `HarcWide`'s implicit
       `uint64_t` conversion, while v1 could not build the same
       program at all. `wait w cycles` narrows to a `uint32_t`,
       `wait until ... timeout w cycles` to an `int64_t`, and both are
       ambiguous in v1.

       `repeat w` is the one worth naming twice. A first pass guarded
       `for` and `wait` and called that "two statement positions" —
       but `repeat` builds the SAME header through the SAME
       `lower_counted_loop`, whose doc line says "shared header /
       body / latch / exit shape for `for` **and `repeat`**". That
       sentence was sitting directly above the new guard, because the
       guard had been inserted in front of the function it documents.
       The re-parented comment named the missed landing.

     What did NOT change: a `default` literal above `u64::MAX` is
     still refused, because every field schema carries its default in a
     `u64`. That refusal is correct rather than conservative — v1
     emits `_harc_u128 w = 36893488147419103232;`, which g++ accepts
     with a warning and evaluates to **0**. The differential harness
     scores that row as "v1 compiles"; only running it shows the
     value, which is the harness's documented blind spot and exactly
     what the `SilentlyMisLowers` label already said.

     Two more findings from the same review, both instances of the
     recurring shape:

     * **One landing measured, the rule written from it — twice, on
       the same rule.** A `default` literal above `u64::MAX` is
       refused at the testbench-field site as
       `NotImplemented{SilentlyMisLowers}`, which is honest. At the
       PROMOTED-`let` site the same class of literal answered
       `Invalid` — "no backend runs this" — against a v1 that compiles
       it, and said "non-integer initializer" about an integer. The
       fix for that gated on `all(is_ascii_digit)`, so the HEX
       spelling of the very same value kept the wrong grade and the
       wrong words, and the three replacement test rows were three
       decimal spellings of one landing. `parse_int_literal_checked`
       answers "overflows" and "not an integer" separately now — one
       parse, two answers — and the rows cover decimal, hex and
       binary. The related diagnostic that called a constant-but-wide
       literal "a non-constant default" is fixed too.
     * **`is_wide_scalar` resolved two of the four host-state reads
       its own doc comment named.** Scoreboard fields (directly and
       through an env), transactor state fields (inside a responder
       body and from the test scope) and record leaves of a state
       field all answered "not wide", so six programs lowered into C++
       nobody could build. Its escape clause — a shape it cannot
       resolve keeps the pre-existing behaviour — did not apply: none
       of those shapes could carry a >128-bit value before this branch
       widened the field gate.
     * **`harc_wide_zext` inherited the constructor's sign
       extension.** Fixing `HarcWide<N>(negative)` to sign-extend made
       the one-argument `harc_wide_zext` — a function named
       *zero*-extend — answer 2^1024-1 for a 64-bit -1 where it had
       answered 2^128-1. Neither is right; it zero-extends explicitly
       now and answers 2^64-1.
     * **A space that could not fail.** None of the harness's three
       falsifiable directions fires on a verdict that over-REFUSES:
       re-capping a width gate turns every row into
       `(Unsupported, v1 compiles)`, which is what `Unsupported`
       means. The five width spaces were green under a reverted
       widening. `check_space_all_lower` asserts the rows of a
       capability that is meant to work actually lower, and the
       wide-default test asserts its refusal directly rather than
       through a report line — a truncating implementation reports
       `(Lowers, v1 compiles)`, a perfectly consistent pairing whose
       only defect is in the value.

     The gap was found by asking v1 across a mechanically enumerated
     width space rather than from one probe, and every step above is
     mutation-tested.

     **Measured and NAMED, not fixed** — each recorded at its site: the
     `<A, B>` equality compares raw words, so two SIGNED values of
     different widths compare unequal when both are -1; the homogeneous
     `/ % < > <= >=` are unsigned, so `x < y` on two negative
     `sint<1024>`s answers by magnitude; a `Vec<uint<1024>, N>` element
     and a nested-`pop()` expression sit under `Unsupported` arms this
     branch did not edit but did make reachable, and v1 cannot build
     either; and an unknown-width DUT port past 64 bits reaches a
     `uint64_t` temp in tbir where v1 refuses to build. Every one
     predates the declared-field widening or belongs to an arm outside
     it. Recording them beats widening the change until nothing is left
     to record.

     Eighteen mutations are checked: re-capping the shared rule (caught
     by the differential harness as well as `tbir.rs`), re-capping
     either per-file rule, restoring the hardcoded emitter triple,
     deleting any one of the six runtime operators, reverting the
     sign-extension, neutering the wide-operator refusal, dropping the
     mixed-width refusal, neutering either host-state lookup, dropping
     the `Bool`-result rule, firing the six-operator guard on a
     same-width wide pair, removing the wide shift-count guard,
     removing any of the `for` / `repeat` / `wait` / `timeout` guards,
     and truncating the promoted-`let` default. The 206-fixture corpus
     lowers identically to the merge base.

135. **The allow-list was a second copy of a path that already worked
     (2026-08-22).**

     Fourteen event rows on a `bound to` target transactor were refused
     while v1 handled them — the largest cluster in the measured gap
     list. Five review rounds had gone into the allow-list doing the
     refusing (divergences 131-133). It should never have existed.

     The SAME `ev : out event<uint<8>>` on an unbound transactor lowers
     and emits, through `lower_field` → `lower_event_payload`. A
     `bound to` transactor reaches that path too — but only when
     `transactor_is_component` says so, and its bound-to branch was
     `return has_on_handler`. An event field was not evidence. So a
     bound target with an `on` handler had its events lowered by the
     component view, and the identical transactor without one had them
     refused by a hand-built allow-list in the target view.

     The rule is `has_on_handler || has_event` now. What made that a
     one-line change safely is that the SAME question was being asked
     in two places: `transactor_is_component`'s bound-to branch, and a
     `component_hosted` re-derivation of it inline in
     `lower_bound_target_transactor` that decides which fields the
     target view skips. Widening one without the other would have left
     the target pass refusing fields the component pass had taken over,
     or lowering them twice. They are one function now, extracted first
     and verified to change nothing, then widened.

     Eight of the fourteen rows lower as a result. The other six keep
     measured refusals — and two of them were WRONG on the shared path,
     which is to say wrong for the unbound site as well, all along:

     - **`event<Color>` promised a v1 that cannot build it.** v1's
       `payload_type_for_arg` emits the bare TYPE NAME for a record or
       an ENUM and routes everything else through
       `record_field_c_type`. It declares the records it emits and no
       C++ enum at all, so an enum payload becomes
       `std::function<void(Color)>` with `Color` undeclared. One
       `Unsupported` covered every non-record payload and promised
       `--codegen v1` for the half it cannot build. Splitting on "is it
       a NAMED type" — the obvious guess — gets `event<string>` wrong
       in the other direction, because `string` parses as a named type
       and v1 builds it. The discriminator is enum-ness, which is what
       v1 keys on; `enum_names` is threaded to the payload rule so it
       can ask the same question rather than approximate it.
     - **A `default` on an event field was accepted and dropped.** v1
       emits it into the member initializer, and the member is a
       subscriber LIST: `std::vector<std::function<void(uint64_t)>> ev
       = 0;` — g++ refuses. The target view's allow-list HAD this
       check; the shared component path, which every unbound
       transactor goes through, did not. The sibling `queue<T>
       default` and `Record default` arms state the same rule with the
       same grade, two arms away.

     The allow-list is deleted. What replaced it is not a
     reimplementation: it is the pre-existing path, plus the two rules
     it was missing.

     One process note. A probe said the `default` fix had not reached
     the unbound site, and the reading was false — `cargo test` builds
     test binaries and leaves `target/debug/harc` stale, so the probe
     was running the previous build. The 206-fixture corpus sweep had
     the same problem and was re-run against a fresh binary before
     being believed. Both come out identical to the merge base.

136. **One question, asked three times, answered wrong twice
     (2026-08-22).**

     `components_impl.rs` holds the largest remaining `Unsupported`
     cluster — 52 of the 296 left in the tree. A differential space
     enumerated from `lower_field`'s own grammar (event / queue / `Vec`
     / other-builtin / named, each with its directional sub-arm) across
     two hosts found twelve real gaps and two false promises.

     Both false promises were the SAME rule the previous divergence had
     just fixed at the event-payload seam: v1 emits the bare TYPE NAME
     for a record or an ENUM, declares the records it emits, and emits
     no C++ enum at all. `queue<Color>` and a bare `m : Color` field
     each promised `--codegen v1` for a program v1 cannot build. Three
     seams, one question, found one at a time. It is
     `v1_leaves_the_type_name_undeclared` now, asked in one place —
     which is what should have happened when the second one turned up.

     Two shipped tests had PINNED the wrong grade, and one of them
     spelled out the reasoning that made it wrong: "v1 handles it, so
     v1 is a real escape hatch here", asserted from the presence of
     `Mode weird` in v1's output. The member is exactly the problem. A
     test that checks a construct appears in the emitted text has not
     checked that the text compiles.

     **The directional arms.** Four arms of `lower_field` carry a
     `f.direction.is_some()` guard, and all four answered `Unsupported`
     — three of them behind an EMPTY reason string. v1 compiles every
     one, which is how the label survived. What it does is DISCARD the
     marker: `a_direction_on_a_component_field_is_discarded_by_v1` pins
     v1's output for the directional spelling as byte-identical to the
     undirected one, with the sources padded to equal length so no
     source-offset residue can explain it — the technique the `bound
     to` field arm two hundred lines away already uses. A program that
     builds and runs meaning something other than what was written is
     `SilentlyMisLowers`, two grades from what these arms claimed.

     Ten real gaps remain in this space, and they are genuine
     implementation work rather than mislabels: `Vec<Record, N>`,
     `Vec<wide, N>`, nested `Vec`, `queue<string>`, `stream`, `buffer`,
     and a zero-width scalar.

     One reading worth recording: `h : Helper` with no mode LOWERS
     under tbir and is REFUSED by v1, which wants an `active`/`passive`
     annotation. tbir is the more permissive backend there. That
     pairing is outside the harness's three falsifiable directions, so
     it is reported here rather than asserted.

137. **The harness was measuring a prefix of the compiler
     (2026-08-23).**

     `scalar_ir_type` gated two things — an event PAYLOAD type and a
     fixed-vector ELEMENT type — and divergence 134 deliberately left
     it alone, recording why: "neither emitter has been shown to carry
     a width past 64 — widening the field sites through this one
     function would have widened both of those unmeasured."

     Measured now. v1 emits
     `std::array<harc_rt::HarcWide<32>, 4>` for a `Vec<uint<1024>, 4>`
     and `std::vector<std::function<void(harc_rt::HarcWide<32>)>>` for
     an `event<uint<1024>>`. Both are real gaps, not mislabels.

     Only the VECTOR half moved, and the reason the two are still
     separate outlived the comment that separated them:
     `FixedVecSchema` carries a full `IrType`, so its width was
     representable and only the emitter truncated;
     `EventPayload::Scalar { signed: bool }` has no width field at all,
     so the payload half needs the IR to be able to say a width before
     anything else can happen.

     **The finding is about the harness, not the feature.** Opening the
     vector gate, `differential.rs` reported every width as `LOWERS` —
     and the real pipeline rejected them. `tb_verdict` ran
     `lower_program` then `emit`, and `harc dump-ir` / `harc sim` both
     run `verify_program` BETWEEN those two. A program that lowered and
     failed verification scored as "works". The tool built to stop
     exactly this class of self-deception had a hole of the same
     shape, and what caught the defect was an ordinary assertion in
     `tests/tbir.rs`.

     It runs the verifier now. Re-running every space produced zero
     other verifier refusals, so the hole was hiding exactly one
     defect — one this batch had just created. A paired mutation
     records the hole itself: revert the verifier's cap AND the
     harness's verify step, and `--test differential` passes a program
     the compiler rejects.

     **Third site, again.** `verify.rs` carried its own hardcoded
     `*w <= 64` for fixed-vector elements, the lowering gate and the
     emitter being the other two. It asks `field_scalar_width_ok` now.
     That makes three consecutive divergences whose central defect was
     one rule living in three places — the enum grade (134-136), the
     scalar decoder (adopted from `main` at a merge), and now this. It
     is the dominant failure mode in this file, not an incidental one.

     The FixedVec emitter held the fifth and last copy of the
     `(bool | int64_t | uint64_t)` triple.

138. **A schema that could only say sixty-four (2026-08-23).**

     The other half of divergence 137. `event<uint<1024>>` was refused
     while v1 emitted
     `std::vector<std::function<void(harc_rt::HarcWide<32>)>>` for it,
     and the reason was not a gate but a representation:
     `EventPayload::Scalar { signed: bool }` could say `int64_t` or
     `uint64_t` and nothing else. Widening the gate alone would have
     produced a 64-bit subscriber parameter for a 1024-bit payload.

     The obvious fix — carry an `IrType`, like every sibling slot
     (`QueueElem::Scalar { ty }`, `StateFieldKind::Scalar { ty }`,
     `FixedVecSchema { elem }`) — does not compile, and the tree says
     why: `IrType::Event(EventPayload)` makes the two mutually
     recursive, so an `IrType` inside the payload is a type of infinite
     size and costs `IrType` its `Copy`. That is exactly why the
     siblings can carry one and this cannot. It carries
     `{ signed, width }` instead, and the doc comment records the
     constraint so the next person does not spend the same twenty
     minutes discovering it.

     Two call sites had already open-coded payload → `IrType` with a
     `None` width to talk to the rest of lowering, one of them
     commenting that the param "IS widthless". They call
     `EventPayload::scalar_ir_type()` now and keep the declared width —
     which is visible in a snapshot: an `on` handler's parameter for
     `event<uint<8>>` types as `uint<8>` rather than a bare `uint`.

     **A pre-existing divergence the measurement surfaced.** v1 emits
     `std::function<void(bool)>` for an `event<bool>`; TB-IR emits
     `void(uint64_t)`. Both compile, so no differential space can see
     it — the observable difference is that `emit ev(2)` notifies with
     `true` under v1 and `2` here. Closing it needs the payload schema
     to be able to SAY bool, which `{ signed, width }` cannot:
     `width: Some(1)` means `uint<1>`, which v1 renders `uint64_t`.
     Named in the test that measured it, not asserted away.

     Three mutations: re-capping the gate, restoring the emitter's
     64-bit pair, and dropping the width on the way into the schema.

139. **A derived `PartialEq` is a rule nobody wrote down
     (2026-08-23).**

     Divergence 138 added `width` to `EventPayload::Scalar`. The
     `connect` event-sink branch asked `*payload != src_payload`, and
     `EventPayload` derives `PartialEq` — so the new field silently
     joined a comparison written when signedness was the only thing
     the variant could hold. `event<uint<8>> -> event<uint<16>>`,
     which v1 renders as two `std::function<void(uint64_t)>` and which
     TB-IR had lowered all along, started failing with "source and
     sink scalar payloads must agree in signedness": a refusal of a
     legal program, under a message that was false about it.

     Nothing in the tree caught it. The full suite passed, and the
     206-fixture corpus lowered identically — because no fixture
     connects two events of different declared widths. It took an
     adversarial review pass reading the schema change against every
     consumer of the type.

     The lesson is narrower than "review your changes". A derived
     `PartialEq` on a schema type makes every `==` in the tree a
     silent participant in a field addition, and the compiler cannot
     flag it: the code still type-checks and still means something,
     just not what it meant. Adding a field to a type that derives
     comparison is a change to every comparison of it. The two
     questions the bridge actually turns on are now asked separately
     and named — `event_payloads_agree_in_shape` for signedness and
     record identity, `connect_delivery_verdict` for width — so a
     third field cannot join either one by accident.

     **The width question, measured rather than assumed.** Opening the
     payload gate also made a wide payload deliverable into a narrow
     subscriber, which main refused only because wide payloads were
     refused outright. v1's bridge is a generic lambda, so delivery is
     one C++ implicit conversion; g++ `-std=gnu++20` was asked what
     each ordered pair of the four storage classes does:

     | src → sink | result |
     | --- | --- |
     | same class | exact |
     | narrower → wider, any pair | exact — `HarcWide<A>` → `HarcWide<B>` with `A < B` preserves every word, and does NOT round-trip through `uint64_t` as first assumed |
     | `HarcWide<A>` → `HarcWide<B>`, `A > B` | does not compile |
     | anything wider → `uint64_t` / `_harc_u128` | compiles, drops the high bits, no diagnostic |

     Widening lowers; the two narrowing rows are refused, split by
     what v1 does with them (`EmitsUncompilable` for the wide-into-
     narrower-wide row, `SilentlyMisLowers` for the rest). The
     first guess about the `HarcWide<A>` → `HarcWide<B>` row was
     wrong in the safe direction, which is the argument for compiling
     the claim rather than reasoning about the header.

     A `check_space_all_lower` space pins the widening direction. The
     neighbouring payload space substitutes one hole into both the
     payload and the sink parameter, so source and sink always agree
     there — which is precisely why a mismatch regression could pass
     it. Five mutations: restoring the struct equality, disabling the
     delivery guard, collapsing the two v1 grades into one, ranking
     every wide width alike, and refusing on any width difference.

140. **The same rule, one level down (2026-08-23).**

     Divergence 139 said a derived `PartialEq` makes every `==` in the
     tree a silent participant in a field addition, then fixed ONE of
     the two. `verify.rs` compared whole `EventPayload` values in the
     `connect` sink arm as well, so lowering emitted a
     `ConnectSink::Event` edge for two payloads of different declared
     widths and the verifier rejected it.

     That is worse than what it replaced. Before 139 these programs got
     a clean `Unsupported` with a wrong message; after it they got
     `internal error: TB-IR failed verification after lowering`. Twelve
     shapes `main` lowers were affected, including
     `event<uint<8>> -> event<uint<16>>` — verbatim the program named
     in 139's own commit subject — and the widening capability 139 was
     built to add was unreachable on that path.

     THREE independent reasons nothing caught it, all inside 139:

     - its new test called `lower_src`, which stops at lowering. The
       assertion that names this exact hazard — "the verifier must not
       reject what lowering deliberately accepts" — sits twenty lines
       above it in the same file.
     - the test wired the connect inside an `env`, and `verify.rs`
       walks only `tb.connects`. An `env` connect reaches no verifier
       at all, so adding the verify call alone would still have passed.
     - its differential space DOES run the verifier, but probes a
       METHOD sink inside an `env` — neither the branch that broke nor
       the scope where verification happens.

     Three ways of missing one thing, each of which looks like
     coverage. The harness fix in divergence 137 was about a tool that
     skipped the verifier; this is the same omission committed by hand,
     in a test written after that fix, by the person who wrote it.

     The verifier ASKS lowering's predicates now rather than restating
     them, and both tests run at both scopes through
     `verify_program`. A verifier that re-derives a rule is a second
     place for it to be wrong — and this one was wrong in the direction
     that produces an internal error rather than a refusal.

     **Measured and left alone.** The dump prints `out` for every event
     field, so an input `inev : event<uint<16>>` renders as
     `out event<uint<16>>`. `ComponentFieldKind::Event { payload }`
     records no direction, so the renderer has nothing to consult;
     recording it is a schema change, not a renderer fix. Named in the
     test that measured it.

141. **Three passes to fix one `==`, and what is still open
     (2026-08-23).**

     Divergence 140's fix was itself incomplete, in the same shape a
     third time. `verify.rs` has TWO connect sink arms; 140 fixed the
     event one and left the method one calling
     `event_payload_matches_type`, which compares signedness and
     ignores width. Delete lowering's `connect_delivery_verdict` call
     and `event<uint<1024>> -> observe(v: uint<8>)` lowers, verifies
     clean, and emits a `std::function<void(harc_rt::HarcWide<32>)>`
     feeding a `uint64_t` parameter — 960 bits dropped per
     notification, silently. Both arms ask the shared predicate now,
     and a test builds the IR a forgetful lowering site would emit and
     watches each arm refuse it.

     The scale was also understated. 140 said "twelve shapes"; that was
     the row count of a test table, three rows of which `main` does not
     lower anyway. Measured against a rebuilt pre-fix binary over a
     15-type alphabet: 30 shapes regressed, 72 internal errors. The
     true set is every pair of distinct declared widths at the same
     storage class with matching signedness, which is unbounded.
     Counting a defect's blast radius from the test that found it
     reports the test, not the defect.

     **Two holes this branch surfaced and does NOT close**, recorded
     here rather than left implicit:

     - `verify.rs` walks `TestbenchSchema::connects` only.
       `ComponentSchema::connects` (an `env` `connect` block) and
       `TbComponentBinding::connects` are never verified — and the env
       form is the MAJORITY shape, 6 of the 10 connect-using fixtures.
       Every backstop in this divergence therefore covers one scope of
       three. The tests say which one.
     - the boundary constant is shared across all four sites in
       `lower` now, but `codegen/tbir/mod.rs` still spells 128 in four
       places of its own. Crossing that needs a home for the constant
       that neither module owns, which is a bigger change than this
       branch should carry.

     A rule that is one function in one place is worth the refactor
     that gets it there. This branch spent three review passes proving
     it by not doing it: the payload width lived in a derived
     `PartialEq`, two verifier arms and a lowering arm, and each pass
     found the copy the previous one had not thought to look for.

142. **Two field shapes that promised v1 but v1 breaks them the same
     way at every landing (2026-08-23).**

     Two `unsupported` field-declaration arms — which is to say, two
     arms telling the user to *re-run with `--codegen v1`* — where v1
     measurably does the wrong thing:

     - `Vec<T,N> default {…}` emitted `std::array<uint64_t, N> v = 0;`,
       which g++ rejects ("conversion from `int` to non-scalar type").
       `EmitsUncompilable`, the grade its sibling `queue`/`Record`/
       event `default` arms already carry three to twenty lines away.
     - a `buffer<T>` / `stream<T>` field fell through to the scalar
       catch-all; v1 has no runtime for either and emits a bare
       `uint64_t`, dropping the message-passing semantics.
       `SilentlyMisLowers`. Now a named arm rather than a generic
       "unsupported type".

     Both measured with `cpp_tb::emit` + g++, on a scoreboard AND a
     transactor, before regrading — because the third candidate did not
     survive that check.

     **The one that got away, and why the check mattered.** A
     directional (`in`/`inout`) event field looked like a third row of
     the same kind. It is not: v1's behavior splits by landing. On a
     scoreboard v1 emits a bare `uint64_t` (mis-lowering); on a
     TRANSACTOR v1 emits the real
     `std::vector<std::function<void(uint64_t)>> ev;` subscriber list.
     Grading the two together put a false detail — "v1 emits a bare
     scalar" — on the transactor case, and a transactor test that
     pinned the old grade failed the moment the regrade was grouped in.
     That is the batch-45 lesson arriving on schedule: a grade is per
     landing, and "measure it on more than one landing" is how you find
     out the grade is not uniform. The directional-event arm stays
     `unsupported` pending a landing-split slice, with the split
     recorded at the site.

143. **Nested fixed-vector component fields (2026-08-23).**

     `Vec<Vec<uint<8>, 2>, 2>` — a fixed vector whose element is itself
     a fixed vector — was refused ("nested vectors ... are not yet
     supported") while v1 emitted the correct
     `std::array<std::array<uint64_t, 2>, 2>` for it. A genuine gap,
     not a mis-grade: it is an IMPLEMENT batch, and the one recursive
     change in this whole stack.

     The design is `IrType::FixedVec { elem: Box<IrType>, len }`, a
     recursive variant like the existing `Seq(Box<IrType>)` (which is
     why `IrType` is not `Copy` — the `EventPayload` doc's worry about
     "costing `IrType` its `Copy`" was already moot). `FixedVecSchema.elem`
     holds a nested `FixedVec`, the decoder recurses to the scalar
     leaf, and `field_scalar_cty` recurses to a nested `std::array`.
     The IR carries the second subscript in a new
     `inner_index: Option<Box<Expr>>` on `ComponentVecElement` /
     `ComponentVecElementWrite`, because the existing `index_pos`
     spreads indices across dotted member SEGMENTS and cannot carry two
     indices on the same leaf.

     Measured against v1 at every storage class:
     `std::array<std::array<{uint64_t|int64_t|_harc_u128|HarcWide<32>},
     2>, 2>` for `uint<8>`/`sint<8>`/`uint<128>`/`uint<1024>` leaves,
     and `self.v[i][j]` for the two-level read/write — all byte-for-byte,
     and the emitted C++ compiles under g++ `-std=gnu++20`.

     The cut line, each refusal matching v1's own limit or this batch's
     scope: both index dimensions are bounds-checked (`Invalid`); a
     record leaf, a `default`, a whole-vector copy of a nested field,
     and a THREE-level index (`v[i][j][k]`, and a triple-nested field
     read) all stay refused. A triple-nested field DECLARES and emits
     (v1's three-deep `std::array` is correct), it just cannot be
     element-indexed past two levels yet.

     What made this a plan-first batch rather than a sit-down edit: the
     recursion touched ~15 files, and adding the `IrType` variant plus
     the two node fields made the compiler enumerate every obligation —
     two exhaustive `IrType` matches (`type_str`, covergroup
     `type_name`), the element-type resolvers on both the lowering and
     codegen sides (which must descend one `FixedVec` when
     `inner_index` is set, or a nested read types as the inner array),
     the port/traversal walkers, and the verifier's field-validity and
     two-level bounds checks. A first pass threaded `inner_index`
     through construction, emit, and the verifier bounds but missed the
     READ-ONLY traversal walkers (placement's `visit_expr`,
     `for_each_port_in_expr`, `expr_has_probe`, `for_each_local`, and
     the lowering rewrite/fill passes) — an adversarial review caught
     it: a DUT port hidden in a nested INNER index
     (`sb.v[0][dut.count_out]`, in a non-hoisting `wait until`) was
     visible to the emitter but not to the analysis passes. Every
     walker that visits `index` visits `inner_index` now. The corpus
     lowers and dumps identically to the branch point; no existing
     program changed.

144. **Testbench fixed-vector host field — and the `let Vec` split it
     surfaced (2026-08-23).**

     A `Vec<T, N>` declared as TESTBENCH host state (`mem : Vec<uint<8>,
     4>` on a reusable testbench, read/written `mem[i] = x` in run/check)
     was refused ("non-scalar, non-named type") while v1 emitted the
     correct `std::array<uint64_t, 4> mem{};` member and `_tb.mem[i]`
     element access. An IMPLEMENT batch, the field analogue of divergence
     143's component field: the member renders through the SAME shared
     `field_scalar_cty` seam v1 uses, so every element class and nesting
     depth v1 handles is handled here for free. The IR carries the reads
     as `Expr::TbFieldVecElement { field, index, inner_index }` and the
     writes as `Stmt::TbFieldVecElementWrite` — distinct from the
     component nodes only because the receiver is a `_tb` field name, not
     a `ComponentBase`.

     Measured against v1 byte-for-byte: `std::array<{uint64_t | int64_t |
     _harc_u128 | HarcWide<32>}, 4> mem{};` for `uint<8>` / `sint<8>` /
     `uint<128>` / `uint<1024>` leaves, `std::array<std::array<uint64_t,
     2>, 2> mem{};` nested, and `_tb.mem[i]` / `_tb.mem[i][j]` for the
     read/write — all compiling under g++ `-std=gnu++20`. The scalar
     resolvers (`as_tb_scalar_field`, and the bare-name capture-scope
     lane) SKIP a fixed-vector-typed entry: it is scalar-shaped storage in
     the same `tb_scalar_fields` table but a whole-`Vec` value, and
     letting it answer the scalar lane would make a bare `_tb.mem` a
     scalar `TbField` and a subscript on it fall through to the
     undeclared-name path. Element access takes the new indexed lane
     (`as_tb_vec_field`) instead.

     The cut line, each refusal MEASURED, not assumed: a `default` on the
     field is refused `NotImplemented`/`EmitsUncompilable` because v1
     emits `std::array<...> mem = <lit>;`, which `std::array` has no
     constructor for; both index dimensions are bounds-checked (`Invalid`).

     **The split this batch's measurement forced.** The requested scope
     was "testbench field AND test-scope `let Vec`", and the two do not
     share a verdict. A test-scope `let m : Vec<T, N>` makes v1 size the
     local from a SCALAR fallback — `int64_t m = 0;` — and then subscript
     it (`m[0] = 5;`), which g++ rejects: the local is a scalar, not an
     array. So the `let` half is NOT implementable to match v1 (matching
     it would mean matching uncompilable output, and diverging from it to
     "fix" it breaks the byte-parity contract). It is regraded
     `NotImplemented`/`EmitsUncompilable` with a message that names v1's
     actual failure and points at the working spelling (host the vector on
     the testbench), never a `--codegen v1` promise. The batch-45 rule
     again: one construct name, two landings, worst-wins — and here the
     two landings live in two different source positions, one an implement
     and one a regrade. `Expr::LocalVecElement` / `LocalVecElementWrite`
     nodes were built out during the first pass and then removed: with the
     `let` half regraded, nothing produces them, and an IR node with no
     producer is a dead, untested path.

     Two walker misses an adversarial review caught, both the §143 class
     (a node the emitter visits but an analysis pass does not). A
     value-returning transactor-method call in a vec INDEX
     (`mem[xt.idx()]`): in a statement context it must hoist to a
     `Stmt::TransactorCall` like the write path already did, so
     `hoist_transactor_calls` gained the `TbFieldVecElement` arm; in a
     `wait until` predicate it cannot hoist, so `expr_has_transactor_edge`
     — the predicate's call scanner — gained it too (and the long-latent
     `ComponentVecElement` case beside it), so the honest "hoist the call
     into a `let`" refusal fires instead of a downstream verifier
     `BadTransactorCall`. The corpus dumps identically across all of it.

145. **No-payload `on <event>()` handler (implement) + record-field
     `default` (regrade) (2026-08-24).**

     Two independent single-site closures, each decided by measurement.

     *Implement:* a no-argument `on <event>()` handler was refused
     ("...v1 synthesizes one") while v1 compiles it — measured uniform
     across agent / env / transactor hosts, emitting the SAME subscriber
     list and lambda signature as the one-argument form, only with a
     throwaway parameter. The whole lowering path already handled it: the
     schema's `arg_payload` comes from the event field's declared payload
     (independent of the handler's args), and `on_handler_arg_name` already
     falls back to a synthesized `_v`. The only thing refusing it was one
     validation arm — removed. tbir now binds the payload to a name the
     body never reads, exactly as v1's throwaway parameter, so the emitted
     handler is trace-equivalent (variable names don't affect runtime; the
     one-argument form is already in the equivalence corpus, and the
     no-arg form differs only by an unread binding). A scoreboard event
     field is refused earlier, so there is no fourth landing.

     *Regrade:* a `default` on a record-typed COMPONENT field was an
     `unsupported` ("re-run with `--codegen v1`"), but v1 emits `<Record>
     r = <lit>;` and g++ refuses the int-to-record conversion (measured on
     agent and transactor hosts) — a false promise, now
     `NotImplemented`/`EmitsUncompilable`. The family SPLITS and only the
     record site moved: the DUT-handle sibling (`dut : Top default 0`)
     keeps its v1 suggestion because v1 DOES compile `VTop* dut = 0;` (the
     bind overwrites it harmlessly) — lumping the two would have repeated
     the batch-45 mistake. The transactor-state record-field default was
     already `EmitsUncompilable` on its own path; this is the
     component-field counterpart. Corpus dumps identically — no existing
     program declared either shape.

     *Two review catches on the implement half.* (a) The first cut bound
     the synthesized payload to a local named `_v` via `declare`, which
     enters the name-resolution scope — so a component FIELD named `_v`
     was SHADOWED: the body's `_v` read the throwaway param, not the field,
     a silent trace divergence from v1 (which qualifies field reads and so
     never shadows). Fixed by binding the no-payload slot with a fresh
     TEMP instead: `fresh_temp` pushes the local for the C++ signature but
     does NOT register it in the scope, so a body identifier resolves to
     the field it names, matching v1 exactly. (b) `validate_event_handler`
     was not the only no-arg gate — the TEST-SCOPE subscription path
     (`on e()` on an `event` local, or `on s.obs()` in test scope) refused
     with a false `Invalid` ("takes exactly one payload binding, got 0")
     while v1 compiles it. Implemented there too, same fresh-temp shape;
     the two-or-more-argument arm keeps its arity error (a different
     construct, out of scope).

146. **Bare `queue` state-field op + non-`bus.<method>` target thread —
     two regrades (2026-08-24).**

     Both were `unsupported` ("re-run with `--codegen v1`") on constructs
     v1 does not actually handle; neither is an implement (v1's output is
     wrong or refused), so each is a pure error-classification fix. No IR,
     no emit change; the corpus dumps identically.

     *Bare queue op → `EmitsUncompilable`.* A bare read (`n = q`) or write
     (`q = 1`) of a `queue` state field in a bound-to target responder
     body (`exprs.rs`, `stmts.rs`) — v1 emits the bare op against the
     `harc_rt::HarcQueue<...>` member and g++ refuses: `no match for
     'operator=' (HarcQueue<...> and int)` on the write, `cannot convert
     HarcQueue<...> to uint64_t` on the read (measured, with a `q.push(1)`
     control that compiles under both backends, isolating the op as the
     cause). The queue field is read/written through its ops
     (`.push`/`.pop`/`.size`/`.empty`); the bare form was never a v1
     escape. Single host (the bound-to responder), two call sites (read +
     write), both measured to the same verdict — no batch-45 split.

     *Non-`bus.<method>` target thread → `Invalid`.* A `thread <x>` whose
     path is not `bus.<method>` (`transactors.rs`) is a program error, not
     a subset gap: v1 refuses it too ("target TLM thread ... must target
     `bus.<method>`", measured), so it joins the four `Invalid` siblings
     in the same loop (unknown method, arity mismatch, bad tag count)
     rather than pointing at v1. Its two neighbouring `unsupported` arms
     (non-literal `out_of_order tags`, non-`blocking`/`out_of_order` mode)
     are parser-unreachable — the bus grammar admits only `blocking` /
     `out_of_order tags <literal>` — so they are left as defensive dead
     code rather than regraded on an unmeasurable path.

147. **Record-element fixed-vector component field — regrade, not
     implement (2026-08-24).**

     `v : Vec<Beat, N>` (a fixed vector whose element is a declared record)
     as a component field looked like an implement gap — a survey measured
     the DECLARATION alone as `v1=compiles` — but measuring the USED
     construct flipped the verdict. v1 does not recognize a record-element
     `Vec` field: it falls back to a SCALAR member, emitting `uint64_t v{};`
     for `Vec<Beat, 2>`. The bare declaration compiles (an unused
     `uint64_t`), but the instant an element is touched — `v[i]`,
     `v[i].field`, `v[i] = r` — the body subscripts the scalar and g++
     refuses: `invalid types 'uint64_t[int]' for array subscript`
     (measured uniform on scoreboard / agent / env; a `q.push`-style
     declared-only control compiles, isolating the access as the cause).
     So the honest grade is `NotImplemented`/`EmitsUncompilable`, not a
     `--codegen v1` promise — matching the sibling `Vec`-`default` and
     regblock-field arms that grade "v1 emits the wrong member, any use
     fails to compile" the same way.

     The lesson is the batch's own: **measure the USED construct, not the
     declaration.** The declaration-only measurement (compiles) is exactly
     the trap — a field type only mis-lowers once something reads or writes
     it, and a grammar-table probe that only declares the field never sees
     it. This is the record-element counterpart to §143/§144's scalar and
     nested-scalar fixed vectors, which v1 DOES emit correctly (as
     `std::array`) and which are therefore implemented — the record element
     is the one v1 mis-declares.

     The refusal site is a catch-all for any non-scalar element type, so
     the regrade is scoped precisely: a helper detects a declared-record
     leaf (through nested `Vec<Vec<Beat,…>,…>` layers, and whether the leaf
     parses as a type or a bare-identifier type-arg) and regrades only
     that; other unsupported element kinds (enum / string) keep the
     catch-all, unmeasured on their access paths. Scalar and nested-scalar
     fixed vectors are untouched — they still lower. Corpus dumps
     identically.

148. **Wide wrapping operator (`+%`/`-%`/`*%` at width > 64) — regrade
     (2026-08-24).**

     A wrapping operator whose operand width exceeds 64 bits was an
     `unsupported` ("re-run with `--codegen v1`"), but v1 has its OWN
     identical gate (`wrap_mask_width` in `cpp_tb.rs`) that returns an
     error and emits no C++ for a wide wrapping op — measured: v1 refuses
     `+%`, `-%`, `*%` at width 128 verbatim, while both backends lower the
     same operators at 64 bits (control). Both reject it, so the honest
     `V1Status` is `Rejects` — and this false promise was previously
     UNMEASURED (the `wide-ops-unsigned` differential space omitted the
     wrapping operators). One site, all three operators; the v1 gate is a
     pure expression-position width check, so there is no host split and no
     used-form subtlety (v1 never emits C++). Corpus dumps identically.

     A second candidate this batch investigated — regrading the whole
     "non-scalar queue element" arm to `EmitsUncompilable` — was DROPPED on
     measurement: the arm is a batch-45 split. `queue<string>` does get
     v1's scalar fallback (`HarcQueue<uint64_t>`, uncompilable on a
     `push("hi")` — measured), but `queue<Vec<..>>` / `queue<list<..>>`
     get a REAL v1 element type and build (pinned by the existing
     `scalar_queue_rollout_keeps_aggregate_elements_unsupported` test), so
     they are genuine implement gaps, not false promises. Regrading the
     shared arm uniformly would have mis-graded Vec/list; splitting `string`
     out precisely needs its own measured sub-batch, so the arm is left as
     is for now.

149. **`yield` of a wrong-typed local in a record-element tseq — regrade
     (2026-08-24).**

     In a `TSeq<Record>` generator body, `yield <ident>` where the local
     is not that same record was an `unsupported` ("re-run with
     `--codegen v1`"). v1 accepts it: it pushes the local verbatim and
     emits the mismatched `std::vector<Record>::push_back(<other>)`, which
     g++ refuses — so the promise is false and the honest grade is
     `NotImplemented`/`EmitsUncompilable`.

     Two locals reach the arm, and both were measured: a DISTINCT record
     (`let u : Other; yield u` into `TSeq<RegOp>`) → v1 emits
     `push_back(Other&)`; and a SCALAR (`let m : uint<8>; yield m`, which
     hits the same arm because `record_of_local` is `None`) → v1 emits
     `push_back(uint64_t&)`. Both are `EmitsUncompilable`, one verdict, so
     the whole arm regrades cleanly. The control — a same-typed record
     local — still lowers and emits `push_back`.

     This is the record-element twin of the scalar-element yield check in
     the arm above (a record yielded into a `TSeq<scalar>`), graded the
     same way and for the same reason. The SEPARATE non-identifier site
     (`yield w.inner`, a field access rather than a bare local) is a
     batch-45 split — v1 compiles it when the field is the element type —
     and is left as `unsupported`, untouched. Corpus dumps identically.

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
