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
| #368 | `scoreboard` declarations (data-only host-state subset): `ScoreboardSchema` table on `TbProgram`, scalar-counter + `queue<T>` fields, `Stmt::ScoreboardOp` (`QueuePush`/`QueuePop`/`ScalarWrite`) and `Expr::ScoreboardQuery` (scalar read / `size()` / `empty()`), scoreboard-instance struct emission (`harc_rt::HarcQueue<T>` members) held on the `_tb` struct, plus the self-proving `scoreboard_basic_test` fixture. Scoreboard methods, event-driven `on`/`connect` wiring, and `queue<Struct>` payloads stay rejected (see the divergence note). |
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
| event-driven-transactor slice (2026-06-13) | **consumer side of the analysis-event machinery** — an unbound `transactor` with an `in event<T>` input pipe driven by an `on req(t)` handler (the UVM driver's `req` sink). `transactor_is_component` now routes such a transactor through the **composite-component** table (it already supports `event` fields, `on` handlers, `emit`, and `connect`) rather than the DUT-poking `TransactorSchema` — even when the transactor carries a module-typed DUT handle field. New IR: `ComponentFieldKind::Dut { dut_type }` (the `V<dut_type>* dut = nullptr;` handle the handler pokes), and `ConnectEdgeSchema::sink` becomes a `ConnectSink { Method { method } \| Event { event } }` enum so a `connect` edge can feed an `in event` pipe (event→event bridge) as well as a hookable sink. Field lowering (`lower/components.rs`) accepts `in event<T>` ON A TRANSACTOR (still rejected elsewhere) and a non-component `Named` type as the single DUT handle (named `dut`); the `on` handler body reuses the component-method lowering, so its `wait N cycles` lower to `WaitCycles(n, None, …)` → v1's synchronous `for(_w) tick();` (the handler is a sync subscriber, not a coroutine actor) and `dut.<sig> = x` lower to `DutWrite` via the shared `ctx.dut_field = "dut"`. `emit drv.req(t)` (test scope) → `ComponentEmit` fan-out over the pipe's subscriber list; `connect <sqr>.dispatched -> <drv>.req` → the source event's bridge closure iterates the sink event's subscribers (`for(auto& _s : drv.req) _s(_t);`). The DUT bind (`drv.dut = dut`) is erased like the `TransactorSchema` bind — the handler's `DutWrite`s already target the test `dut` pointer. An `active`/`passive` mode IS accepted on these (a transactor concept): `active` required, `passive` rejected. Fixtures: `event_driven_transactor_test` (self-proving, `top_counter.sv`/`top_counter.arch`, pass) — direct `emit drv.req(n)` + a sequencer→transactor `connect` event bridge, both pulsing `en` and latching `count_out`; `axilite_seqdrv_test` (corpus, `AxiLiteRegs.sv`, pass) — sequencer→transactor full AXI-Lite write/read round-trip via direct emit. Both trace-diff clean v1↔tbir at seed 1. **Still rejected** (residual map): `transactor_active_test` (bound-to `transactor X bound to <Bus>` event-driven form — needs the coroutine-actor + bus-binding driver, a separate slice) and `axilite_connect_test` (its env holds a data-only `scoreboard` SUB-component with through-env `queue` access — the env-field-binding/data-scoreboard-sub slice, not the event-driven-transactor surface). |

| bus-bind-remap slice (2026-06-13) | **`bind ... with { ch.sig: "port", ... }` bus signal remaps** (`src/ir/lower/bus.rs`, `src/codegen/tbir/func.rs`, `src/ir/{mod,display}.rs`). Mirrors v1's `bind_remap → bus_remap → bus_signal_name`: `BusBindingSchema` gains a `remap: Vec<((channel, signal), port)>` (sorted by key for deterministic dump-ir) plus a `wire_name(channel, signal)` resolver that returns the override if present else the `<field>_<channel>_<signal>` convention. `lower_bus_binding` no longer rejects `bind_remap` — it validates each path is exactly `<channel>.<signal>` (2 segments, malformed is a hard `Invalid` error) and records it. The two `wire` closures in `emit_transactor_call` (initiator call edge) and `emit_target_actor` (target responder, which now also receives `&tb.bus_bindings`) route through `wire_name`, so both TLM directions honor the override. For a `tlm_method` the channel is the method name and the signal is a protocol wire (`req_valid`/`addr`/`rsp_data`/...). No new IR variants, no statement/expr forms. Fixtures (all trace-diff clean v1↔tbir at seed 1): `tlm_bind_remap_test` (self-proving — binds with name `m` so the convention would drive nonexistent `m_read_*`; every entry remaps to the real `mem_read_*`/`mem_poke_*` port, proving the table is load-bearing), `dma_engine_tlm_target_test` + `dma_engine_tlm_mem_model_test` (corpus — blocking target responders that were rejected only for the explicit `bind ... with`). **Still rejected** (residual map): `fork`/`join_all` TLM issue (`tlm_method_bus_test`, `tlm_target_fork_forwarding_test`, `tlm_pairing_arch_target_test`), `out_of_order tags N` target lanes (`tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test`), and nested responder forwarding (`tlm_target_forwarding_test`) — each a distinct deeper slice. |
| initiator-side fork/join_all TLM slice (2026-06-13) | **`let x = fork bus.<method>(args)` + `join_all` over bus `tlm_method`s — initiator-side concurrent issue** (`src/ir/lower/{bus,stmts,mod}.rs`, `src/ir/{mod,display,verify,passes/placement}.rs`, `src/codegen/tbir/{mod,func}.rs`). Mirrors v1's `try_emit_bus_tlm_fork` / `emit_tlm_join_all`: a `fork` issues ONLY the request side (drive arg wires + optional `req_tag`, raise `req_valid`, 16-cycle budget-wait `req_ready`, tick, drop `req_valid`); the response is captured at the next `join_all`. New IR: `Stmt::TlmFork(TlmForkDesc)` (request issue) + `Stmt::TlmJoinAll(Vec<TlmForkDesc>)` (drain), where `TlmForkDesc` is self-contained (`bus_field`, `method`, `args`, `dest`, `has_ret`, `tag`) so the join statement carries its own descriptors — no cross-statement lowering replay in the backend. Tag allocation is per-`(bus_field, method)` monotonic on the builder (v1's `next_tlm_fork_tag`): a `blocking` method gets `tag: None` (issue-order FIFO drain — `emit_ordered_tlm_join_all`: per fork, raise `rsp_ready`, 64-cycle budget-wait `rsp_valid`, capture `rsp_data`, tick, drop); an `out_of_order tags N` method gets `tag: Some(n)` (tag-routed drain — `emit_tagged_tlm_join_all`: poll every lane per tick, accept any `rsp_tag`-matching not-yet-seen fork, 256-cycle budget — so tag 1 can land before tag 0). The pending-fork accumulator survives `WaitCycles` between blocks (it is builder-state, not block-state), so a fork in one block + `wait` + a second fork + `join_all` in the next block lowers correctly. `lower_function::finish` rejects a dangling `fork` with no matching `join_all`; `lower_tlm_join_all` rejects a mixed tagged/untagged barrier (the two routing strategies can't share a join). The verifier routes both statements through `check_bus_call_edge` (Run/Check only, binding resolves on the owner tb, method exists, arg arity + no-inline-port purity); the def/use pass defines `dest` at the fork site (v1's `T x = {};` zero-init), placement classifies both as the TLM seam (`has_transactor_call` → TimingTolerant, Tier-1). Fixture: `tlm_method_bus_test` (`TlmMemory.sv`, pass) — blocking `read`/`poke` + two `out_of_order` `read_ooo` forks joined together; trace-diff clean v1↔tbir at seed 1. **Still rejected** (residual map): (a) a `fork` INSIDE a transactor responder body (target re-issuing a downstream TLM call — fork-forwarding: `tlm_target_fork_forwarding_test`, `tlm_target_forwarding_test`) — needs the responder to be both target+initiator with a request arbiter/response router; (b) target-side `out_of_order tags N` RESPONDER lanes (`tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test`) — hidden tag wires + multi-lane response router; (c) `tlm_pairing_arch_target_test` (ARCH DUT) LOWERS and is v1↔tbir trace-diff clean, but its auto-emitted `_auto_tlm_*_req_stable` TLM SVA `$fatal`s under **local Verilator 5.048** for both codegens identically — a known local-only artifact, NOT registered. |
| TLM responder nested-forwarding slice (2026-06-13) | **a bound-to TARGET responder re-issuing a downstream TLM call inside its body — nested forwarding** (`src/ir/lower/{mod,transactors}.rs`, `src/ir/verify.rs`, `src/codegen/tbir/{func,expr}.rs`). The deferred residual (a) from the initiator-side fork/join_all slice. A responder thread `thread bus.read(addr) ... let raw = back.read(addr); return raw + ...` forwards each front request through a SECOND test-scope bus binding (`back`) to a downstream target. The responder is lowered before any test, so `back`'s bus type is not in scope at responder-lowering time. A file-level **pre-scan** of every (desugared) test's `let <name> : <Bus> = bind ...` declarations builds a `name → BusDecl` map (first binding wins on collision — the responder body is type-shared, so its downstream bus type must be unambiguous) that is handed to the bound-target responder body's `bus_bindings` ctx. The downstream call then lowers through the EXISTING #390 machinery — `try_lower_bus_call` (blocking → `CallTarget::TransactorMethod` Assign-RHS edge) or `try_lower_tlm_fork` (`out_of_order tags N` downstream → `Stmt::TlmFork`/`TlmJoinAll`) — composed inside the responder coroutine loop-switch. **No new IR variants.** The verifier's `check_bus_call_edge` now permits a bus-call edge in an owner-less `TransactorBody` (resolution defers to emit, which has the test's `bus_bindings`; an unresolved downstream binding surfaces as an EmitError). The tbir backend passes the test's `&tb.bus_bindings` (not `&[]`) into the responder loop-switch `emit_stmt`, and a new `ECx::trace_component` field tags the downstream `tlm_call` trace event with the responder-instance name (mirroring v1's `current_component_instance`), so the semantic trace diffs clean. Fixtures (both pass, trace-diff clean v1↔tbir at seed 1): `tlm_target_forwarding_test` (blocking downstream `back.read` — 3 SV files: `TlmForwardingTop` + `TlmMemory` + `TlmReadInitiatorPair`), `tlm_target_fork_forwarding_test` (two `fork back.read_ooo(...)` + `join_all` over an OOO downstream bus). **Still rejected** (residual map): the OOO-RESPONDER LANE form — a responder SERVING an `out_of_order tags N` method (hidden tag wires + per-tag dispatcher/lane/arbiter coroutines): `tlm_target_ooo_lanes_test`, `tlm_pairing_arch_initiator_test` ("serving a `out_of_order` method"); and `tlm_pairing_arch_target_test` (LOWERS + trace-diff clean, but its ARCH-DUT auto-emitted TLM SVA `$fatal`s under local Verilator 5.048 for both codegens — known local-only artifact, NOT registered). |
| agent-mode + cycle-trigger slice (2026-06-13) | **agent-mode multi-DUT-handle transactor + cycle-trigger `on <expr>` monitor handlers + agent/env `connect` bridges** (`src/ir/lower/{components,stmts,mod}.rs`, `src/codegen/tbir/{mod,func,expr}.rs`). Three pieces, each unlocking the `transactor_agent_mode_test`/`transactor_env_mode_test` corpus fixtures: (1) **cycle-trigger handlers** — new `ComponentSchema::cycle_handlers: Vec<CycleTriggerHandlerSchema>` (`trigger` predicate expr + `CycleEdge { Rising \| Falling \| Level }` + zero-arg body `function`). An `on <bool-expr> ... end on` handler (distinguished from `on <ev>(arg)` by `is_event_subscription` — a `Call` whose callee names a self `event` field is a subscription; anything else is a cycle-trigger) lowers to a zero-arg `ComponentMethod` body + its trigger predicate (lowered self-relatively, so `dut.<sig>` reads route to the DUT handle and bare field reads resolve self-relative). Pass-1 reserves FunctionIds methods→event-on-handlers→periodic→cycle-trigger→watchdog (monotonic). The tbir backend installs one per-instance `_checkers` closure per cycle-handler (recursing into sub-components), gated on a uniquely-named `static` prev-state for edge detection (mirrors v1's `emit_cycle_trigger`). It is the always-on OBSERVER half — present on BOTH active and passive instances. (2) **self-relative sub-scoreboard poke** — `sb.writes = sb.writes + 1` inside a transactor body, where `sb` is a `ScoreboardSub` field of the self component: `scoreboard_root` (`stmts.rs`) now resolves a `ScoreboardSub` receiver of `self_component` to a `self`-rooted `nested_path`, and the tbir `ScoreboardOp`/`ScoreboardQuery` emission re-roots `self` at the running instance via `self_subst`. The component method ctx (`mod.rs`) now carries the `scoreboards` table so the scalar-field validation succeeds. (3) **agent + nested-env `connect` bridges** — `connect` edges are now resolved for `Agent` decls (not only `Env`), and the tbir backend recurses through `Sub` fields to install a nested sub-component's bridges (`emit_nested_connects`), so an env→agent→drv `sequencer.dispatched -> drv.req` bridge wires at `<env>.<agent>` scope. Plus: a sequencer's `hookable dispatch(txns: TSeq<Record>)` param now types as `RecordSeq` (`method_param_ir_type` resolves the `TSeq<RegOp>` element against `record_ids`, handling both `TypeArg::Type(Named)` and the bare-ident `TypeArg::Expr` parse), so `for t in txns` iterates it and the C++ param renders `std::vector<Record>`. The `active`/`passive` mode on a composite-component test-scope `let` (`let act : AxilAgent active`) is accepted (and ignored, matching v1 — the fixtures' passive correctness comes from the test never dispatching the passive sequencer, not from hard `when active` elision). Fixtures: `transactor_agent_mode_test` + `transactor_env_mode_test` (`AxiLiteRegs.sv`, pass) — same agent/env decl reused active + passive, active drives 5 AXI round-trips via sequencer→connect→`on req(t)`, both transactors' cycle-trigger observers tally 5 writes + 5 reads off the shared DUT; trace-diff clean v1↔tbir at seed 1. **Divergence**: hard `when active` body elision on a passive instance is NOT implemented (v1 doesn't elide for these fixtures either; the body is structurally present but never stimulated on passive). A `post_eval`-phased or hooked cycle-trigger is rejected precisely (checker phase only). |
| bound-to monitor slice (2026-06-13) | **bound-to event-driven transactor's passive MONITOR surface — `on bus.<ch>.handshake(arg)` observers + `sb` ScoreboardSub feed + a `passive` bound instance** (`src/ir/lower/{components,mod}.rs`, `src/ir/{mod,display}.rs`). The deferred passive half of the bound-to-agent cluster (PR #391 landed the DRIVER half). An `on bus.<ch>.handshake(arg)` handler on a `bound to <Bus>` transactor desugars into a `CycleTriggerHandlerSchema` with a new `monitor_channel: Some(<ch>)` flag: the synthesized trigger is the channel's `<ch>.valid && <ch>.ready` (rising edge), and the body preamble captures the channel payload into the handler's `arg` local — first payload signal aliases `arg` (scalar `sb.q.push(arg)`), every signal also a per-field alias in `recv_payloads` (so `beat.data`/`beat.resp` resolve, like a `recv()` capture) — then the user body feeds the sub-scoreboard. `lower_monitor_handshake_body` reads the channel payload from the bound `BusDecl` (placeholder-prefixed in the per-component body ctx). **No new actor IR / no new tbir codegen** — reuses the agent-mode cycle-trigger `_checkers` machinery; the monitor triggers live on the schema, so a new `fill_initiator_bus_prefix_expr` (sharing the `fill_visit_*` walkers factored out of `fill_initiator_bus_prefix`) fills their placeholder bus prefix. A `passive` bound instance is now accepted when the transactor declares monitor handlers (pure-driver-no-monitor passive still rejected — inert); both modes register the always-on monitors, `active` adds the `on req` driver. **Divergence**: v1 emits a per-channel coroutine ACTOR (`emit_bound_monitor_actors`); the IR uses a rising-edge cycle-trigger instead — observably equivalent for single-beat valid/ready handshakes (the lowered subset; they'd diverge only for a multi-cycle held handshake, out of subset). Fixtures: `transactor_passive_only_test` (pure monitor), `axilite_bound_mon_test` (active driver + passive monitor concurrent), `axilite_multi_payload_test` (multi-payload `beat.data`/`beat.resp`) — all `AxiLiteRegs.sv`, pass, trace-diff clean v1↔tbir at seed 1. Closes the bound-to-agent cluster. |
| probe/force slice (2026-06-13) | **DUT-internal signal access via declared probes (read) and force points (write)** (`src/ir/{mod,lower/{mod,stmts,exprs},passes/placement,verify,display}.rs`, `src/codegen/tbir/{mod,runtime,func,expr}.rs`). Makes the long-reserved `PortAccess::Probe`/`Force` real (was always `Port`, divergence 1). A `probe <name> : <T> at <path>` / `probe force <name> : <T> at <path>` on `let dut` (classic OR testbench-owned, the impl-for desugar preserves probes) is collected into `LowerCtx::probes` (name → `{force, width}`); a single-segment `dut.<name>` whose name is a probe lowers `as_port_ref` to a `PortRef` with `access = Probe`/`Force` + the declared scalar width. New IR: `Stmt::ProbeRelease(PortRef)` for `release dut.<probe>`. Lowering enforces the access discipline: writing a read-only probe, or `release`-ing a non-force probe / ordinary port, is a precise hard error. The tbir backend mirrors v1's `Emitter::probes`: a `Probe`/`Force` read routes through `dut->rootp-><DutType>__DOT__harc_probes__DOT__<name>` (the SV bind-stub accessor, `expr::port_signal`/`probe_read_accessor`); a `Force` write emits the `_drv = expr; _en = 1;` pair (`func.rs`); `ProbeRelease` emits `_en = 0;`. The preamble pulls in `V<DutType>___024root.h` when any probe is used (`program_has_probes`, gated like v1's `aggregated_probes`). The SV stub (`__harc_probe_<DutType>.sv`) is emitted by the shared `emit_probe_stub_if_needed` path — identical for both codegens. Fixtures (all `cpu_pipeline.sv`, pass, trace-diff clean v1↔tbir at seed 1): `probe_basic_test` (3 read-only probes hoisting `alu0.{a,b,result}`), `probe_force_test` (read probe + `probe force` write/release fault-injection), `testbench_probe_dut_test` (testbench-OWNED probed DUT + `function reset()`, regression for the impl-for desugar). **Subset / divergence**: probe types must be scalar (`uint<N>`/`sint<N>`/`bits<N>`/`bit`/`bool` — the SV stub only surfaces scalar logic); an aggregate probe type is rejected precisely. Multi-segment probe paths (`at alu0.a`) are stored verbatim in the stub `at`-target and validated by Verilator, NOT harc (mirrors v1; docs/probe-signals.md §4.4). Probing inside an ARCH-compiled DUT (`--dut`) is out of scope — these fixtures self-skip the arch-DUT sweep (`-` in the registry, no `.arch` sibling). |

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
(`src/ir/lower/addrmap.rs`). The passive `record_write`/`record_read`
API and per-register `on regs.REG` callbacks (rejected precisely via
`regblock::detect_regblock_residual`) remain the only out-of-subset
regblock residuals (see divergence 12).
`transactor` declarations lower in their **unbound DUT-poking BFM
subset**: no `bound to <BusType>`, no generics, exactly one
module-typed field (the DUT handle — its type must match the test's
DUT, since the IR keeps the single-DUT model), and
`hookable`/`function` methods with scalar (≤64-bit) params and
optional scalar return. Each method body lowers to its own
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
`.sext<N>()` / `.resize<N>()` with N ≤ 64 → `Expr::WidthCast`,
mirroring v1's mask/cast/shift-fill emission and direction checks,
with v1's best-effort receiver-width inference from typed lets /
casts / chained methods / literals), and scalar `as uint<W>`-family
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
dropped). Event-driven `on`/`connect` wiring and non-scalar
(`queue<Struct>`, >64-bit) field types are likewise rejected at the
field/site — those gate on the agent/env/event slices and the
record-payload-in-queue seam respectively.
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
`record_ids`, so the collision is rejected, not shadowed. Out of subset
and rejected at the field/decl, never mis-lowered: non-scalar /
>64-bit fields (`Vec<...>`, nested structs — the residual blocker for
the `tlm_pairing_arch_burst_*` fixtures), non-literal defaults, and
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
`on`/`connect` wiring / `queue<Struct>` payloads (the data-only
scoreboard subset lowers — see below), the block-form `fork ... and ...
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
(`_last_in_cycle`/`_last_out_cycle` ≥ `max_idle` ⇒ `FAIL` + `errors++`).
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
| `tlm_target_thread_test` | `TlmReadInitiator` | `TlmReadInitiator.sv` | pass | target-side TLM: blocking `thread bus.read` responder actor, single-cycle wait, value return |
| `tlm_target_thread_if_test` | `TlmReadInitiatorPair` | `TlmReadInitiatorPair.sv` | pass | target-side TLM: persistent state fields (read in body + from test), `for` loop, `if`/`else` return |
| `tlm_target_thread_runtime_loop_test` | `TlmReadInitiatorRuntimeLen` | `TlmReadInitiatorRuntimeLen.sv` | pass | target-side TLM: runtime `for i in 0..len` loop bound |
| `tlm_target_thread_early_return_test` | `TlmReadInitiatorRuntimeLen` | `TlmReadInitiatorRuntimeLen.sv` | pass | target-side TLM: early `return` from nested `if` inside a runtime loop |

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

4. **All locals hoist as `uint64_t`.** `src/codegen/tbir/func.rs`
   declares every IR local as `uint64_t <name> = 0;` at function top
   (hoisting is forced by the loop-switch — a local must survive
   across `case` arms). v1 emits declared-width C types for typed
   lets (`local_value_c_type`: `uint<75>` → `_harc_u128`, narrow
   widths → narrow C types) and `int64_t` for untyped integer lets.
   Two observable deltas follow: (a) a typed narrow local that
   overflows its declared width truncates on assignment under v1 but
   not under tbir — **narrowing semantics differ**; (b) untyped lets
   holding negative intermediates are signed under v1, unsigned under
   tbir. None of the five fixtures exercises either case, so the
   behavioral gate cannot see this; it is a known, latent divergence.
   Fix path: `IrType` already carries `UInt(Option<u32>)` /
   `SInt(Option<u32>)`; lowering currently leaves locals
   `IrType::Unknown`. Populating widths at lowering (from `let`
   type annotations, as v1's `let_widths` pass does) and emitting
   width-faithful types — or masking at `Assign` — closes it without
   IR shape changes.

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
   non-scalar / wider-than-64-bit fields and non-literal defaults
   (v1 lowers enums/lists/wide ints; the tbir expression model is
   u64), and record locals in *pure* helpers (those emit as
   scalar-only file-scope C++ functions; impure helpers CFG-inline,
   where record locals are fine). Emission-side delta: tbir emits the
   struct + `operator==`/`!=` only — v1 also emits `randomize_<T>`
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
   - *Rejected at the bind/call site* (emission-side metadata the IR
     does not carry, or machinery deferred): bind-site generics
     (`Bus#(P=...)`), buses with `generate_if`-gated signals (gate
     evaluation needs the DUT-port param-override layering only
     `EmitOpts` has), a direct (non-`fork`) `out_of_order` method call,
     a `fork` INSIDE a transactor responder body (fork-forwarding), and
     target-side `out_of_order tags N` RESPONDER lanes.

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
      check-phase reference to a test-scope let is an explicit
      `Unsupported` (never a silent zero); run/check-shared state is
      what testbench scalar fields are for.
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
    - *Width methods* cover the ≤ 64-bit subset only (the tbir
      expression model is u64); > 64-bit targets are explicit
      rejections. `zext` on an unknown-width receiver is a plain
      cast — v1's exact shape, including its documented
      "assume the receiver already fits" looseness.
    - *Lane indices* must be compile-time constants (literal, paren,
      or `const`/enum name). v1 accepts arbitrary index expressions;
      a variable index is an explicit rejection until a fixture
      needs it.

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
    agent/env/event slices) and non-scalar field/element types
    (`queue<Struct>`, > 64-bit — needs the record-payload-in-queue
    seam) are likewise rejected at the field/site. No divergence in
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
    bus-bind-remap slice). **Out of subset** (precise rejections):
    target-side `out_of_order tags N` threads (tagged RESPONDER lanes),
    and a `fork` inside a responder body (a responder re-issuing a
    downstream TLM call — forwarding). (Initiator-side `fork`/`join_all`
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
    - *Element type must be a record.* `tseq_element_name` requires
      `-> TSeq<Record>` where `Record` is a declared
      `transaction`/`struct`; a missing return type, a non-`TSeq`
      return, or a `TSeq<scalar>` is a precise `Unsupported` at
      `collect_tseq_records` (the IR's sequence-element model is a
      value-record). `yield` outside a tseq body is rejected with v1's
      "`yield` outside a `tseq` body" intent.
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
      idle for >= %lld cycles", …)` + `errors++`). The period/max_idle
      exprs render against the instance path via a new `ECx::self_subst`
      (`SelfField` → instance, since the `_checkers` closure has no `self`
      in scope). **Implementation divergence from v1 only:** v1 emits the
      idle check INSIDE the `<Comp>_watchdog` method and the period gating
      in the checker via a `field_subs`-rewritten period; tbir emits the
      idle check + period gating in the checker (the lowered body holds
      only the user statements). Behavior is identical — same FAIL text,
      same firing cycles, same `errors++` — verified by trace-diff.
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
      the `if (!cond)` branch). Also `Expr::ErrorCount` — a bare `errors`
      ident (the framework error counter, bumped by `AssertCheck`/error
      logs), for the trailing `assert errors == 0` after a `bitbash` walk.
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
    - *Out of subset* (still rejected, precisely): the passive
      `record_write`/`record_read` API and per-register `on regs.REG`
      write callbacks (`regblock_record_test`,
      `regblock_record_recursion_test`) — `regblock::detect_regblock_residual`
      names the callback/record-API feature before the generic
      bare-statement/scope mixing error fires. Field-level
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
    - *Out of subset* (still rejected, precisely): the passive
      `record_write`/`record_read` API and per-register `on regs.REG`
      write callbacks remain the only regblock residuals.

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
