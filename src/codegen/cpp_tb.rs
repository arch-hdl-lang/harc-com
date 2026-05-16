//! Minimal HARC → Verilator C++ TB codegen.
//!
//! Scope (tracking spec §14 "Phase 0" / pre-1a):
//! - One `test T` with one `scope sim` and one `run` block becomes a `main()`
//!   that drives a Verilated DUT.
//! - The conventional name `dut` (per spec §2) marks the DUT instance — the
//!   field-access expression `dut.signal` lowers to `dut->signal`, and
//!   `let dut : <Type>` becomes `V<Type>* dut = new V<Type>;`. The same
//!   pointer-shape rule applies to any `Named`-typed local or function
//!   parameter, not just the literal name `dut`.
//! - **Assignment operators** match ARCH's discipline:
//!     - `=` — blocking assignment (locals, raw DUT signal drives).
//!     - `<-` — channel send (event / buffer / state per spec §17.2; future).
//!     - `<=` — non-blocking assignment, future, valid only inside `seq` /
//!       `thread` blocks (parser disambiguation will mirror ARCH's
//!       `no_lteq` flag once those constructs land in codegen).
//!   Both `=` and `<-` lower to the same `dut->x = v;` C++ today.
//! - `after N cycles ... end after` lowers to `for (int _=0; _<N; _++) tick();`
//!   where `tick()` toggles the clock and calls `eval()` twice.
//! - `assert expr [else fail("msg")]` lowers to a runtime check that
//!   increments an `errors` counter; per-test failure reporting and exit
//!   code mirror the hand-written `examples/counter_tb.cpp` shape.
//! - `log(<severity>, "...")` lowers to a `sim_log_line(<SEV>, ...)` call
//!   that writes `[cycle:N <SEV>] <msg>` to both stdout and `sim.log`.
//!   Test-result semantics from §7.7 are honored:
//!     - `error` increments the `errors` counter (test fails at end of run).
//!     - `fatal` increments + sets `_fatal`, and the main simulation loop
//!       exits at end of the current cycle (this test instance aborts).
//!     - `warn` / `info` / `debug` have no test-result effect.
//!   Verbosity flags, component IDs, and per-component overrides are
//!   spec'd in §7.7 but deferred — the runtime currently prints all
//!   severities unconditionally.
//!
//! Out of scope here:
//! - `tseq`, randomization, properties, coverage groups, fork/join, env/
//!   agent/monitor/scoreboard composition, multi-clock domains.
//! - Type checking — we trust the user's HARC and let the C++ compiler
//!   surface signal-name typos against Verilator's generated header.

use crate::ast::*;
use crate::lexer::Span;
use std::fmt::Write;

/// HARC's coroutine runtime header. Baked into the binary at build
/// time via `include_str!` and dropped into the test build dir by
/// `harc sim` so the emitted `.cpp` can `#include "harc_thread_rt.h"`
/// without a separate file dependency.
pub const THREAD_RT_HEADER: &str = include_str!("../../runtime/harc_thread_rt.h");

const INDENT: &str = "    ";

#[derive(Debug)]
pub struct EmitError(pub String);

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for EmitError {}

/// Walk the file for a single `test T { let dut : <Type> }` decl and return
/// the simple name of `<Type>` — the SystemVerilog top module that the C++
/// codegen will reference as `V<Type>`. Used by the harc-sim driver to set
/// Verilator's `--top-module` when running against a pre-built SV DUT.
pub fn dut_type_name(file: &SourceFile) -> Option<String> {
    let test = file.items.iter().find_map(|it| match it {
        Item::Test(t) => Some(t),
        _ => None,
    })?;
    for it in &test.items {
        if let TestItem::Let(l) = it {
            if l.name.name == "dut" {
                return type_simple_name(l.ty.as_ref()).map(|s| s.to_string());
            }
        }
    }
    None
}

/// If the test's `let dut : T` carries `probe ... at <path>` declarations,
/// return `(T's simple name, &probes)`. `None` for tests without probes —
/// callers skip the SV bind-stub emission entirely in that case.
pub fn dut_probes(file: &SourceFile) -> Option<(String, &[Probe])> {
    let test = file.items.iter().find_map(|it| match it {
        Item::Test(t) => Some(t),
        _ => None,
    })?;
    for it in &test.items {
        if let TestItem::Let(l) = it {
            if l.name.name == "dut" && !l.probes.is_empty() {
                let ty = type_simple_name(l.ty.as_ref())?.to_string();
                return Some((ty, &l.probes));
            }
        }
    }
    None
}

/// Emit a C++ Verilator TB for the single `test` declaration in this file.
/// Per-emit options. Currently just the `--mt` opt-in for Phase 3a's
/// per-actor OS thread topology; defaults to cooperative
/// (single-OS-thread, faster on real fixtures).
#[derive(Default, Clone, Copy)]
pub struct EmitOpts {
    /// Spawn one `std::thread` per bound coroutine actor with dual-
    /// barrier sync per posedge. Off → all coroutines share `sched`
    /// and tick cooperatively in the main thread.
    pub mt: bool,
}

pub fn emit(file: &SourceFile) -> Result<String, EmitError> {
    emit_with_opts(file, EmitOpts::default())
}

pub fn emit_with_opts(file: &SourceFile, opts: EmitOpts) -> Result<String, EmitError> {
    // Desugar `impl <name> for <TbType>` tests into the classic
    // `test <name>` form before any other emission work runs. The
    // testbench-bound form (docs/test-ergonomics.md §3.3) folds a
    // testbench's fields + helper methods into the bound test's
    // scope; desugaring synthesizes the equivalent `let dut : ...` /
    // `let _tb : <TbType>` declarations at test scope and rewrites
    // bare-name references inside run/setup/check/teardown bodies
    // to `_tb.<field>` / `<TbType>_<method>(_tb, ...)`. Once
    // desugared, the test looks identical to a classic-form test
    // and threads through the existing pipeline unchanged.
    let file = desugar_impl_for_test_in_file(file);
    let file = &file;

    // Collect ALL `test` items — every one becomes a `run_<TestName>`
    // function in the emitted binary, and the dispatcher `main()` at
    // the end picks one based on `--test <name>` or the `HARC_TEST`
    // env var (Phase 1b of docs/separate-compilation-plan.md).
    // Multi-test in one binary lets `harc sim --test foo` then
    // `harc sim --test bar` share Verilator output (the .cpp is
    // byte-identical across selectors).
    let tests: Vec<&TestDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Test(t) => Some(t),
            _ => None,
        })
        .collect();
    if tests.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }

    // Validate: all tests in one binary must agree on the DUT type
    // (Verilator builds one V<top> per build; multi-DUT requires
    // separate builds, out of scope for v0). Aggregate the probe
    // accessors across tests so the root-header include gate sees
    // every test's probes — without this, a test with probes that
    // wasn't the first one in source order would compile against a
    // V<Top>.h that didn't pull in V<Top>___024root.h.
    let mut shared_dut_type: Option<&str> = None;
    let mut aggregated_probes: std::collections::HashMap<String, ProbeAccessor> =
        std::collections::HashMap::new();
    for t in &tests {
        for it in &t.items {
            if let TestItem::Let(l) = it {
                if l.name.name == "dut" {
                    let ty_name = type_simple_name(l.ty.as_ref()).ok_or_else(|| {
                        EmitError("`let dut : <Type>` must use a simple named type".into())
                    })?;
                    match shared_dut_type {
                        Some(prev) if prev != ty_name => {
                            return Err(EmitError(format!(
                                "multi-DUT tests in one binary are out of scope for v0; \
                                 test `{}` uses `{}`, but a previous test used `{}`",
                                t.name.name, ty_name, prev,
                            )));
                        }
                        _ => {
                            shared_dut_type = Some(ty_name);
                        }
                    }
                    for p in &l.probes {
                        aggregated_probes
                            .entry(p.name.name.clone())
                            .or_insert_with(|| ProbeAccessor {
                                read: crate::codegen::sv_stub::mangled_accessor(
                                    ty_name,
                                    &p.name.name,
                                ),
                                force: p.force,
                            });
                    }
                }
            }
        }
    }
    let dut_type: &str = shared_dut_type
        .ok_or_else(|| EmitError("expected `let dut : <Type>` declaration in test body".into()))?;

    // Per-test metadata (custom_phases, other_lets, ...) is derived
    // inside the per-test emission loop further down. The few
    // file-scope code paths that previously referenced `test_decl`
    // (e.g. when computing tseq/funcs/etc.) keep working off any of
    // the tests — those collections walk `file.items`, not
    // `test.items`. We bind `test_decl` to the alphabetically-first
    // test only so leftover single-test references compile during
    // the staged migration.
    let _test_decl = tests[0];

    // Collect top-level functions — emitted as lambdas inside main() so they
    // can capture `dut` and `tick` lexically.
    let funcs: Vec<&FunctionDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Function(f) => Some(f),
            _ => None,
        })
        .collect();

    // Collect `tseq` declarations — same hoisting strategy as functions,
    // emitted as `std::function`-shaped lambdas returning `std::vector<T>`
    // built up via `yield`.
    let tseqs: Vec<&TseqDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Tseq(t) => Some(t),
            _ => None,
        })
        .collect();

    // Index `domain Foo freq_mhz: N end domain Foo` decls so a `clock X =
    // Foo` reference can resolve N to a wall-clock period (1/N µs → ps).
    let mut domains: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Domain(d) = it {
            for f in &d.fields {
                if f.name.name == "freq_mhz" {
                    if let ExprKind::Int(s) = &*f.value.kind {
                        if let Ok(n) = s.replace('_', "").parse::<i64>() {
                            // Period in ps = 1_000_000 / freq_mhz.
                            if n > 0 {
                                domains.insert(d.name.name.clone(), 1_000_000 / n);
                            }
                        }
                    }
                }
            }
        }
    }

    // Per-test metadata (dut_type for V<top>* construction, other_lets,
    // bare_stmts, explicit_run, clocks, probe_accessors) is derived
    // inside the per-test emission loop further down (search for
    // `for test in &tests`). The shared `dut_type` resolved above is
    // used by file-scope code paths that need the DUT module name.

    // Collect transactions + enums + scoreboards + monitors for typed-let
    // emission.
    let mut transactions = std::collections::HashSet::new();
    let mut scoreboards = std::collections::HashSet::new();
    let mut components: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    let mut covergroups: std::collections::HashMap<String, CovergroupDecl> =
        std::collections::HashMap::new();
    let mut buses: std::collections::HashMap<String, BusDecl> = std::collections::HashMap::new();
    let mut transactors: std::collections::HashMap<String, TransactorDecl> =
        std::collections::HashMap::new();
    // RAL regblocks indexed by name. Populated in the pre-pass below;
    // consulted at codegen time to (a) emit one POD mirror struct +
    // constexpr address table per regblock at file scope, and (b)
    // lower `regs.NAME` field accesses against the bound helper.
    let mut regblocks: std::collections::HashMap<String, RegblockDecl> =
        std::collections::HashMap::new();
    // RAL addrmaps indexed by name. Chip-level container of regblock
    // instances at distinct base addresses. See docs/ral-support.md §4.
    let mut addrmaps: std::collections::HashMap<String, AddrmapDecl> =
        std::collections::HashMap::new();
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    // Per-transaction `keep` constraints (spec §4 — "constraints are
    // relations, not directives"). Collected here so `randomize(t)`
    // and `randomize(t) with …` both route them through the Z3 solver
    // path. Without this collection the keep blocks parse but emit
    // zero C++, a silent footgun: `keep len in [1..256]` would
    // randomize freely up to the field's full width.
    let mut txn_keeps: std::collections::HashMap<String, Vec<Expr>> =
        std::collections::HashMap::new();
    let mut enums = std::collections::HashMap::new();
    // Global variant-name → index map. Used by the Z3 constraint
    // translator to resolve bare `WRAP` / `INCR` / etc. into their
    // numeric encoding. v0 assumes variant names are globally
    // unique; collisions take the first-declared mapping (warned
    // about in the parser if we add that). Maps to i64 so negative
    // signed values fit naturally; the solver widens at use site.
    let mut enum_variants: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    // Relation declarations indexed by name (spec §4.2). At constraint-
    // emit time, any `Call(Ident(R), args)` whose name is in this map
    // is inlined: the formal parameters substitute into R's body and
    // each expression becomes its own constraint added to the Z3
    // solver block. Recursive expansion handles relation-aliases-of-
    // relations (`relation A(t) = B(t) && t.x == 0`).
    let mut relations: std::collections::HashMap<String, RelationDecl> =
        std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Relation(r) = it {
            relations.insert(r.name.name.clone(), r.clone());
        }
    }
    for it in &file.items {
        if let Item::Enum(e) = it {
            enums.insert(e.name.name.clone(), e.variants.len());
            for (i, v) in e.variants.iter().enumerate() {
                enum_variants.entry(v.name.clone()).or_insert(i as i64);
            }
        }
    }
    for it in &file.items {
        match it {
            Item::Scoreboard(c) => {
                scoreboards.insert(c.name.name.clone());
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) => {
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Covergroup(g) => {
                covergroups.insert(g.name.name.clone(), g.clone());
            }
            Item::Bus(b) => {
                buses.insert(b.name.name.clone(), b.clone());
            }
            Item::Transactor(t) => {
                transactors.insert(t.name.name.clone(), t.clone());
            }
            Item::Regblock(r) => {
                regblocks.insert(r.name.name.clone(), r.clone());
            }
            Item::Addrmap(a) => {
                addrmaps.insert(a.name.name.clone(), a.clone());
            }
            _ => {}
        }
    }

    // Validate addrmap structure: alias targets resolve, no chained
    // aliases, instance windows don't overlap (for pairs with size,
    // skipping any pair where one side aliases the other or both
    // alias the same target).
    for a in addrmaps.values() {
        if let Some(err) = check_addrmap_aliases(a) {
            return Err(EmitError(err));
        }
        if let Some(err) = check_addrmap_overlap(a) {
            return Err(EmitError(err));
        }
    }

    // Enforce: a transactor's always-on body cannot drive DUT signals.
    // Drive-side hookables / on-handlers must live inside `when active`.
    // Without this check, a `passive` instance — which has its
    // `when_active` body elided — would still emit the drive code and
    // could wire an observer to the bus (block-level→chip-level TB
    // reuse footgun). See `check_transactor_no_drive_in_always_on_body`
    // for the full rationale and detection model.
    for t in transactors.values() {
        if let Some(err) = check_transactor_no_drive_in_always_on_body(
            t,
            &transactors,
            &components,
            &scoreboards,
            &covergroups,
            &buses,
            &regblocks,
            &addrmaps,
        ) {
            return Err(EmitError(err));
        }
    }

    for it in &file.items {
        match it {
            Item::Transaction(t) => {
                transactions.insert(t.name.name.clone());
                let fields = t
                    .body
                    .iter()
                    .filter_map(|it| match it {
                        TxnBodyItem::Field(f) => {
                            let (width, signed, enum_variants) = match &f.ty {
                                TypeExpr::Builtin { name, args, .. } => match name {
                                    BuiltinTy::UInt | BuiltinTy::Bits | BuiltinTy::UIntCap => {
                                        (type_arg_width(args).unwrap_or(64), false, None)
                                    }
                                    BuiltinTy::SInt | BuiltinTy::SIntCap => {
                                        (type_arg_width(args).unwrap_or(64), true, None)
                                    }
                                    BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                                        (1, false, None)
                                    }
                                    BuiltinTy::Int => (32, true, None),
                                    _ => (64, false, None),
                                },
                                TypeExpr::Named { name, .. } => {
                                    let last =
                                        name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                                    if let Some(&n) = enums.get(last) {
                                        (enum_width(n), false, Some(n))
                                    } else {
                                        (64, false, None)
                                    }
                                }
                            };
                            Some(TxnFieldInfo {
                                name: f.name.name.clone(),
                                width,
                                signed,
                                enum_variants,
                                non_random: f.non_random,
                                attrs: f.attrs.clone(),
                            })
                        }
                        _ => None,
                    })
                    .collect();
                txn_fields.insert(t.name.name.clone(), fields);
                // Collect transaction-level `keep` constraints. Keeps nested
                // inside `when` subtype bodies lower as guarded implications:
                // `when G { keep K }` contributes `(!G) || K`, so the
                // constraint participates only when the discriminator selects
                // that subtype. Full tagged-ADT solver modeling remains a
                // future backend step; this keeps the current flat solver path
                // semantically honest for keeps over already-visible fields.
                let keeps = collect_txn_keeps(&t.body);
                if !keeps.is_empty() {
                    txn_keeps.insert(t.name.name.clone(), keeps);
                }
            }
            Item::Enum(e) => {
                enums.insert(e.name.name.clone(), e.variants.len());
            }
            _ => {}
        }
    }

    // Seed let_types from test-level `let` decls so `randomize(t)` works
    // when the let appears at test scope (above any scope sim / bare stmts).
    // Also seed let_modes from any `: T active`/`: T passive` annotation;
    // the call-site passive-mode check (further down, in emit_expr) walks
    // this map to determine the effective mode of `<env>.<sub>...<xact>`
    // paths whose root resolves to a let-bound name.
    // Build the property name → body table.
    let mut properties = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Property(p) = it {
            properties.insert(p.name.name.clone(), p.body.clone());
        }
    }

    // Construct the emitter with all FILE-SCOPE shared state. Per-test
    // fields (let_types, let_modes, pointer_vars, probes, clock_names,
    // bus_bindings, bus_remap, covers, actor_threads, field_subs,
    // driver_bus_for_hookables, let_helper) are intentionally left
    // empty here — they're reset+populated inside the per-test
    // emission loop below.
    let mut e = Emitter {
        out: String::new(),
        errors: Vec::new(),
        pointer_vars: std::collections::HashSet::new(),
        let_types: std::collections::HashMap::new(),
        let_widths: std::collections::HashMap::new(),
        let_modes: std::collections::HashMap::new(),
        transactions,
        scoreboards,
        components,
        covergroups,
        txn_fields,
        enums,
        enum_variants,
        properties,
        prop_subs: std::collections::HashMap::new(),
        event_types: std::collections::HashMap::new(),
        field_subs: std::collections::HashMap::new(),
        covers: Vec::new(),
        clock_names: Vec::new(),
        current_yield_target: None,
        tseq_names: tseqs.iter().map(|t| t.name.name.clone()).collect(),
        buses,
        bus_bindings: std::collections::HashMap::new(),
        bus_remap: std::collections::HashMap::new(),
        in_coroutine: false,
        actor_threads: Vec::new(),
        mt: opts.mt,
        driver_bus_for_hookables: std::collections::HashMap::new(),
        transactors,
        current_component_instance: None,
        txn_keeps,
        relations,
        probes: std::collections::HashMap::new(),
        regblocks,
        addrmaps,
        let_helper: std::collections::HashMap::new(),
    };

    // Header.
    writeln!(e.out, "// Auto-generated by harc — do not edit.").ok();
    if tests.len() == 1 {
        writeln!(e.out, "// HARC test: {}", tests[0].name.name).ok();
    } else {
        let names: Vec<&str> = tests.iter().map(|t| t.name.name.as_str()).collect();
        writeln!(
            e.out,
            "// HARC tests ({}): {}",
            tests.len(),
            names.join(", ")
        )
        .ok();
    }
    writeln!(e.out, "").ok();
    // Disable clang optimization for this file. clang 17+ on both
    // Apple Silicon and Linux x86_64 mis-optimizes our `[&]`-
    // capturing C++20 lambda coroutines at `-Os` / `-O2`: closure
    // reference members fold against the original (freed) stack
    // frame after a suspension, causing SEGV on resume. Per-file
    // pragma keeps verilator-generated DUT `.cpp` files at `-Os`
    // for fast simulation.
    //
    // GCC has the same class of miscompile but `#pragma optimize`
    // doesn't propagate through C++20 coroutine codegen there
    // (`-O0`-pragma SEGVs trivial tests; `-O1`-pragma still SEGVs
    // bound-actor tests). CI installs clang on Linux and sets
    // `CXX=clang++` so this pragma applies on both platforms.
    // Member-function refactor of the run / actor coroutines is
    // the durable fix; this pragma is the v0 stop-gap.
    writeln!(e.out, "#ifdef __clang__").ok();
    writeln!(e.out, "#pragma clang optimize off").ok();
    writeln!(e.out, "#endif").ok();
    writeln!(e.out, "").ok();
    writeln!(e.out, "#include \"V{dut_type}.h\"").ok();
    // Probe access needs the root struct's full definition (the
    // `rootp` field on V<Top> is a forward-declared pointer in
    // `V<Top>.h`). When the test declares one or more probes, also
    // include the root header so `dut->rootp-><mangled>` compiles.
    if !aggregated_probes.is_empty() {
        writeln!(e.out, "#include \"V{dut_type}___024root.h\"").ok();
    }
    writeln!(e.out, "#include \"verilated.h\"").ok();
    writeln!(e.out, "#if VM_COVERAGE").ok();
    writeln!(e.out, "#include \"verilated_cov.h\"").ok();
    writeln!(e.out, "#endif").ok();
    writeln!(e.out, "#include <cstdio>").ok();
    writeln!(e.out, "#include <cstdint>").ok();
    writeln!(e.out, "#include <cstdlib>").ok();
    writeln!(e.out, "#include <cstdarg>").ok();
    writeln!(e.out, "#include <cstring>").ok();
    writeln!(e.out, "#include <string>").ok();
    writeln!(e.out, "#include <unordered_map>").ok();
    writeln!(e.out, "#include <vector>").ok();
    writeln!(e.out, "#include <deque>").ok();
    writeln!(e.out, "#include <functional>").ok();
    // Phase 3a: per-actor OS threads + barrier sync. Pulled in
    // unconditionally — `<atomic>` is also indirectly available via
    // `harc_thread_rt.h` but the explicit include keeps intent clear.
    writeln!(e.out, "#include <thread>").ok();
    writeln!(e.out, "#include <atomic>").ok();
    // HARC's coroutine runtime — drives the test's `run` block as a
    // C++20 coroutine via `harc_rt::ThreadScheduler`. Hookable methods
    // and `on`-handler closures still run synchronously between the
    // run coroutine's co_awaits; only the run body itself yields.
    // Multi-actor parallelism (driver + monitor coroutines on the same
    // bus) lands in Phase 2 on top of the same runtime.
    writeln!(e.out, "#include \"harc_thread_rt.h\"").ok();
    let uses_solver = uses_constraint_solver(file);
    if uses_solver {
        writeln!(
            e.out,
            "#include <z3++.h>   // randomize(t) with <constraints>"
        )
        .ok();
    }
    writeln!(e.out, "").ok();

    // ── PRNG runtime ──────────────────────────────────────────────────────
    // SplitMix64 — small, fast, pure stdlib. Seed loaded from HARC_SEED.
    writeln!(e.out, "static uint64_t harc_rng_state = 0;").ok();
    writeln!(e.out, "static inline uint64_t harc_rng_next() {{").ok();
    writeln!(
        e.out,
        "{INDENT}uint64_t z = (harc_rng_state += 0x9E3779B97F4A7C15ULL);"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;"
    )
    .ok();
    writeln!(e.out, "{INDENT}return z ^ (z >> 31);").ok();
    writeln!(e.out, "}}").ok();
    writeln!(
        e.out,
        "static inline int64_t harc_rng_range(int64_t lo, int64_t hi) {{"
    )
    .ok();
    writeln!(e.out, "{INDENT}if (hi <= lo) return lo;").ok();
    writeln!(
        e.out,
        "{INDENT}return lo + (int64_t)(harc_rng_next() % (uint64_t)(hi - lo + 1));"
    )
    .ok();
    writeln!(e.out, "}}").ok();
    writeln!(
        e.out,
        "static inline uint64_t harc_rng_uint(unsigned width) {{"
    )
    .ok();
    writeln!(e.out, "{INDENT}if (width >= 64) return harc_rng_next();").ok();
    writeln!(
        e.out,
        "{INDENT}return harc_rng_next() & ((1ULL << width) - 1);"
    )
    .ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "static inline int64_t harc_rng_dist(const std::vector<std::tuple<int64_t,int64_t,int64_t>>& bins) {{").ok();
    writeln!(
        e.out,
        "{INDENT}int64_t total = 0; for (auto& b : bins) total += std::get<2>(b);"
    )
    .ok();
    writeln!(e.out, "{INDENT}if (total <= 0) return 0;").ok();
    writeln!(
        e.out,
        "{INDENT}int64_t pick = (int64_t)(harc_rng_next() % (uint64_t)total);"
    )
    .ok();
    writeln!(e.out, "{INDENT}int64_t acc = 0;").ok();
    writeln!(e.out, "{INDENT}for (auto& b : bins) {{ acc += std::get<2>(b); if (pick < acc) return harc_rng_range(std::get<0>(b), std::get<1>(b)); }}").ok();
    writeln!(e.out, "{INDENT}return std::get<0>(bins.front());").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "").ok();

    // ── Semantic trace runtime ───────────────────────────────────────────
    // JSONL writer used by `harc sim --record-trace <path>` (plumbed as
    // HARC_TRACE by the CLI). Keep this tiny and dependency-free so the
    // emitted TB remains a single C++ file plus runtime header.
    writeln!(
        e.out,
        "static std::string harc_trace_escape(const std::string& s) {{"
    )
    .ok();
    writeln!(e.out, "{INDENT}std::string out; out.reserve(s.size() + 8);").ok();
    writeln!(e.out, "{INDENT}for (unsigned char c : s) {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}switch (c) {{").ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}{INDENT}case '\"': out += \"\\\\\\\"\"; break;"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}{INDENT}case '\\\\': out += \"\\\\\\\\\"; break;"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}{INDENT}case '\\n': out += \"\\\\n\"; break;"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}{INDENT}case '\\r': out += \"\\\\r\"; break;"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}{INDENT}case '\\t': out += \"\\\\t\"; break;"
    )
    .ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}default:").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}if (c < 0x20) {{ char buf[7]; std::snprintf(buf, sizeof(buf), \"\\\\u%04x\", c); out += buf; }}").ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}{INDENT}{INDENT}else out.push_back((char)c);"
    )
    .ok();
    writeln!(e.out, "{INDENT}{INDENT}}}").ok();
    writeln!(e.out, "{INDENT}}}").ok();
    writeln!(e.out, "{INDENT}return out;").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "struct HarcTraceWriter {{").ok();
    writeln!(e.out, "{INDENT}FILE* out = nullptr;").ok();
    writeln!(e.out, "{INDENT}uint64_t seq = 0;").ok();
    writeln!(e.out, "{INDENT}bool enabled = false;").ok();
    writeln!(e.out, "{INDENT}void open_env() {{ const char* p = std::getenv(\"HARC_TRACE\"); if (p && *p) {{ out = std::fopen(p, \"w\"); enabled = (out != nullptr); }} }}").ok();
    writeln!(e.out, "{INDENT}void close() {{ if (out) {{ std::fflush(out); std::fclose(out); out = nullptr; }} enabled = false; }}").ok();
    writeln!(e.out, "{INDENT}uint64_t next_seq() {{ return seq++; }}").ok();
    writeln!(e.out, "{INDENT}void meta(uint64_t seed, const char* backend, const char* top, const char* test) {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}if (!enabled) return;").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::fprintf(out, \"{{\\\"type\\\":\\\"meta\\\",\\\"schema_version\\\":1,\\\"tool\\\":\\\"harc\\\",\\\"seed\\\":%llu,\\\"dut_backend\\\":\\\"%s\\\",\\\"top\\\":\\\"%s\\\",\\\"test\\\":\\\"%s\\\"}}\\n\", (unsigned long long)seed, backend ? backend : \"unknown\", top ? top : \"\", test ? test : \"\");").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::fflush(out);").ok();
    writeln!(e.out, "{INDENT}}}").ok();
    writeln!(
        e.out,
        "{INDENT}void raw(const char* type, int cycle, const std::string& payload) {{"
    )
    .ok();
    writeln!(e.out, "{INDENT}{INDENT}if (!enabled) return;").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::fprintf(out, \"{{\\\"type\\\":\\\"%s\\\",\\\"cycle\\\":%d,\\\"seq\\\":%llu%s%s}}\\n\", type, cycle, (unsigned long long)next_seq(), payload.empty() ? \"\" : \",\", payload.c_str());").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::fflush(out);").ok();
    writeln!(e.out, "{INDENT}}}").ok();
    writeln!(
        e.out,
        "{INDENT}void log(int cycle, const char* sev, const std::string& msg) {{"
    )
    .ok();
    writeln!(e.out, "{INDENT}{INDENT}std::string payload = \"\\\"severity\\\":\\\"\" + harc_trace_escape(sev ? sev : \"\") + \"\\\",\\\"message\\\":\\\"\" + harc_trace_escape(msg) + \"\\\"\";").ok();
    writeln!(e.out, "{INDENT}{INDENT}raw(\"log\", cycle, payload);").ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}if (sev && std::strcmp(sev, \"FAIL\") == 0) {{"
    )
    .ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::string fail_payload = \"\\\"failure_id\\\":\\\"fail\\\",\\\"message\\\":\\\"\" + harc_trace_escape(msg) + \"\\\"\";").ok();
    writeln!(
        e.out,
        "{INDENT}{INDENT}{INDENT}raw(\"assertion_failure\", cycle, fail_payload);"
    )
    .ok();
    writeln!(e.out, "{INDENT}{INDENT}}}").ok();
    writeln!(e.out, "{INDENT}}}").ok();
    writeln!(e.out, "}};").ok();
    writeln!(e.out, "").ok();

    // Tiny FIFO wrapper for `queue<T>` scoreboard fields. Provides pop()
    // returning the front element (std::queue separates front/pop), and
    // empty()/size(). Emitted only when scoreboards exist.
    let any_scoreboard = file
        .items
        .iter()
        .any(|it| matches!(it, Item::Scoreboard(_)));
    if any_scoreboard {
        writeln!(e.out, "#include <deque>").ok();
        writeln!(e.out, "template<typename T> struct HarcQueue {{").ok();
        writeln!(e.out, "{INDENT}std::deque<T> _d;").ok();
        writeln!(e.out, "{INDENT}void push(T v) {{ _d.push_back(v); }}").ok();
        writeln!(
            e.out,
            "{INDENT}T pop() {{ T v = _d.front(); _d.pop_front(); return v; }}"
        )
        .ok();
        writeln!(e.out, "{INDENT}bool empty() const {{ return _d.empty(); }}").ok();
        writeln!(e.out, "{INDENT}size_t size() const {{ return _d.size(); }}").ok();
        writeln!(e.out, "}};").ok();
        writeln!(e.out, "").ok();
    }

    // ── Transaction structs + per-type randomize_T(&t) functions ─────────
    for it in &file.items {
        if let Item::Transaction(t) = it {
            e.emit_transaction(t);
        }
    }

    // ── Scoreboard structs (just data + default-init; methods inline) ────
    for it in &file.items {
        if let Item::Scoreboard(s) = it {
            e.emit_scoreboard(s);
        }
    }

    // ── Per-channel payload structs ──────────────────────────────────────
    // `bus.<ch>.recv()` and `on bus.<ch>.handshake(arg)` capture the
    // channel's full payload — for multi-payload channels (e.g. AXI's
    // `r` carrying both `data` and `resp`) users need `arg.data` /
    // `arg.resp`. We emit one struct per `handshake_channel`, named
    // `<BusName>_<chan>_payload`, with one field per payload signal.
    //
    // The struct also exposes `operator uint64_t() const` returning
    // the first field — backward compatible with the previous v0
    // behaviour (`recv()` returning a scalar). Existing fixtures that
    // do `assert val == 0xCAFEBABE` keep working without change;
    // multi-payload access just becomes available.
    for it in &file.items {
        if let Item::Bus(b) = it {
            for h in &b.handshakes {
                if h.payload.is_empty() {
                    continue;
                }
                let struct_name = format!("{}_{}_payload", b.name.name, h.name.name);
                writeln!(e.out, "struct {struct_name} {{").ok();
                for sig in &h.payload {
                    let cty = txn_field_c_type(&sig.ty);
                    writeln!(e.out, "{INDENT}{cty} {};", sig.name.name).ok();
                }
                // Implicit conversion to the first payload field — keeps
                // single-field-style usage (`val == N`, `last_read = val`)
                // compiling against the new struct type.
                let first_sig = &h.payload[0];
                let first_cty = txn_field_c_type(&first_sig.ty);
                writeln!(
                    e.out,
                    "{INDENT}operator {first_cty}() const {{ return {}; }}",
                    first_sig.name.name,
                )
                .ok();
                writeln!(e.out, "}};").ok();
                writeln!(e.out, "").ok();
            }
        }
    }

    // ── RAL regblock mirror structs + address tables ────────────────────
    // One POD `struct <Name>_Mirror { uint<W>_t REG; ... };` per
    // declared `regblock`, plus a `constexpr` address table giving
    // each register's byte offset. Accessor calls (`regs.NAME = v` /
    // `let x = regs.NAME`) lower to mirror update + helper.write/read
    // — see docs/ral-support.md §7.4 (frontdoor lowering). Phase 1a
    // keeps the mirror flat — no nested addrmap composition yet.
    for it in &file.items {
        if let Item::Regblock(r) = it {
            let default_w = r.default_width.unwrap_or(32);
            writeln!(e.out, "struct {}_Mirror {{", r.name.name).ok();
            for reg in &r.registers {
                let w = reg.width.unwrap_or(default_w);
                let cty = mirror_field_c_type(w);
                let init = match &reg.reset {
                    Some(rv) => format!(" = {}", c_int_literal_from(&rv.kind)),
                    None => " = 0".to_string(),
                };
                writeln!(e.out, "{INDENT}{cty} {}{};", reg.name.name, init).ok();
            }
            writeln!(e.out, "}};").ok();
            writeln!(e.out, "").ok();

            // constexpr address table — one entry per register, indexed
            // by C++ identifier matching the source register name. Used
            // by future bitbash() lowering; reads/writes inline the
            // offset literal directly for simplicity in Phase 1a.
            writeln!(
                e.out,
                "struct {}_AddrEntry {{ const char* name; uint64_t offset; uint32_t width; }};",
                r.name.name,
            )
            .ok();
            writeln!(
                e.out,
                "static constexpr {}_AddrEntry {}_AddrTable[] = {{",
                r.name.name, r.name.name,
            )
            .ok();
            for reg in &r.registers {
                let w = reg.width.unwrap_or(default_w);
                let off = c_int_literal_from(&reg.offset.kind);
                writeln!(
                    e.out,
                    "{INDENT}{{ \"{name}\", {off}, {w} }},",
                    name = reg.name.name,
                )
                .ok();
            }
            writeln!(e.out, "}};").ok();
            writeln!(e.out, "").ok();
        }
    }

    // ── RAL addrmap mirror structs ──────────────────────────────────────
    // Chip-level container. Each `addrmap A { instance inst : R @ B }`
    // lowers to `struct A_Mirror { R_Mirror inst; ... };` — one
    // member per instance, of the corresponding regblock's Mirror
    // type. Bus addresses for `chip.inst.REG` are computed
    // `BASE_inst + offset(REG)` at codegen time. See docs/ral-support.md §4.
    for it in &file.items {
        if let Item::Addrmap(a) = it {
            writeln!(e.out, "struct {}_Mirror {{", a.name.name).ok();
            for inst in &a.instances {
                // Aliased instances share mirror storage with their
                // target — no separate field. Access through the
                // alias rewrites to the target's mirror path at
                // codegen time. See docs/ral-support.md §4.
                if inst.alias_of.is_some() {
                    writeln!(
                        e.out,
                        "{INDENT}// {name}: alias of {target} — shares mirror",
                        name = inst.name.name,
                        target = inst.alias_of.as_ref().unwrap().name,
                    )
                    .ok();
                    continue;
                }
                writeln!(
                    e.out,
                    "{INDENT}{ty}_Mirror {name};",
                    ty = inst.regblock_ty.name,
                    name = inst.name.name,
                )
                .ok();
            }
            writeln!(e.out, "}};").ok();
            writeln!(e.out, "").ok();
        }
    }

    // ── Covergroup structs (per-bin counters + sample() + report()) ─────
    // Emitted BEFORE component structs so a `testbench Tb { cov : Cg }`
    // body can name `Cg` as a field type without a forward-decl. The
    // testbench/env composition is the only direction the dependency
    // runs — covergroups are leaf observables, they never name a
    // component or transactor.
    for it in &file.items {
        if let Item::Covergroup(g) = it {
            e.emit_covergroup_struct(g);
        }
    }

    // ── Monitor structs ──────────────────────────────────────────────────
    // Same shape as scoreboards; output `event<T>` fields lower to
    // ── Component structs (agent / env / sequencer).
    // Scoreboards have their own dedicated path above; the rest are
    // plain field-bearing structs. `hookable` methods are emitted
    // separately as free `[&]`-capturing lambdas (below) so the
    // method body sees `dut` / `tick` / `_checkers` from the test scope.
    for it in &file.items {
        match it {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) => {
                e.emit_component_struct(c);
            }
            Item::Transactor(t) => {
                // Compose a synthetic ComponentDecl with the union of
                // always-present body items + active body items, and
                // emit a single struct. Both modes get the same C++
                // class layout — passive instances simply don't
                // subscribe to / spawn actors against the active
                // fields. Mode-specific elision lives in the lowering
                // at instantiation, not in the struct shape.
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                e.emit_component_struct(&synth);
            }
            _ => {}
        }
    }

    // ── Top-level `const` items ─────────────────────────────────────────
    // `const NAME : Ty = expr` lowers to a file-scope
    // `static constexpr <c_type> NAME = <expr>;` so the value is
    // available everywhere — inside `main()`, hookable lambdas (which
    // can't capture across translation-unit boundaries but can use
    // file-scope constants directly), `tseq` lambdas, and on-handler
    // closures. constexpr also folds into expressions used at struct
    // field defaults if any future fixture goes there.
    let mut emitted_const = false;
    for it in &file.items {
        if let Item::Const(c) = it {
            let cty =
                c.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or_else(|| "int64_t".to_string());
            write!(e.out, "static constexpr {cty} {} = ", c.name.name).ok();
            e.emit_expr(&c.value);
            writeln!(e.out, ";").ok();
            emitted_const = true;
        }
    }
    if emitted_const {
        writeln!(e.out, "").ok();
    }

    // `extern function name(params) -> ret` (spec §9) — emit C-linkage
    // forward declarations at file scope so the user's `--ref-src
    // <file>.cpp` can satisfy the linker without HARC needing to know
    // its implementation. Wrap in a single `extern "C" { … }` block
    // so even C++ source files that don't add their own `extern "C"`
    // get the right calling convention.
    let extern_fns: Vec<&ExternFnDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::ExternFn(f) => Some(f),
            _ => None,
        })
        .collect();
    if !extern_fns.is_empty() {
        writeln!(
            e.out,
            "// extern reference functions (spec §9) — implementations"
        )
        .ok();
        writeln!(
            e.out,
            "// supplied via `harc sim --ref-src <file>` and linked into the"
        )
        .ok();
        writeln!(e.out, "// verilator-built binary.").ok();
        writeln!(e.out, "extern \"C\" {{").ok();
        for f in &extern_fns {
            let param_names = cpp_param_names(&f.params);
            let ret = f
                .return_ty
                .as_ref()
                .map(c_type_for)
                .unwrap_or_else(|| "void".to_string());
            write!(e.out, "{INDENT}{ret} {}(", f.name.name).ok();
            for (i, p) in f.params.iter().enumerate() {
                if i > 0 {
                    write!(e.out, ", ").ok();
                }
                let pty =
                    p.ty.as_ref()
                        .map(c_type_for)
                        .unwrap_or_else(|| "int64_t".to_string());
                write!(e.out, "{pty} {}", param_names[i]).ok();
            }
            writeln!(e.out, ");").ok();
        }
        writeln!(e.out, "}}").ok();
        writeln!(e.out, "").ok();
    }

    // Per-test entry points. Each `Item::Test` in source gets its
    // own `int run_<TestName>(argc, argv)` function. The dispatcher
    // `int main()` below picks one based on `--test <name>` /
    // `HARC_TEST` env. Each run function owns its DUT pointer,
    // scheduler, `_checkers` vector — tests run against fresh
    // Verilator instances, sequentially (one binary, dispatched).
    // See docs/separate-compilation-plan.md for the future per-test
    // `.o` direction (Phase 2).
    for test in &tests {
        // ── Reset Emitter per-test state ────────────────────────────
        e.let_types.clear();
        e.let_modes.clear();
        e.let_widths.clear();
        e.pointer_vars.clear();
        e.pointer_vars.insert("dut".to_string());
        e.bus_bindings.clear();
        e.bus_remap.clear();
        e.probes.clear();
        e.clock_names.clear();
        e.covers.clear();
        e.actor_threads.clear();
        e.field_subs.clear();
        e.driver_bus_for_hookables.clear();
        e.let_helper.clear();
        e.in_coroutine = false;
        e.current_yield_target = None;
        e.current_component_instance = None;

        // ── Derive per-test metadata ───────────────────────────────
        let custom_phases: Vec<(Ident, Block)> = test
            .items
            .iter()
            .filter_map(|it| match it {
                TestItem::Phase(name, body) => Some((name.clone(), body.clone())),
                _ => None,
            })
            .collect();
        let mut other_lets: Vec<&LetStmt> = Vec::new();
        let mut bare_stmts: Vec<&Stmt> = Vec::new();
        let mut explicit_run: Option<&Block> = None;
        let mut clocks: Vec<&ClockDecl> = Vec::new();
        for it in &test.items {
            match it {
                TestItem::Let(l) => {
                    if l.name.name == "dut" {
                        for p in &l.probes {
                            e.probes.insert(
                                p.name.name.clone(),
                                ProbeAccessor {
                                    read: crate::codegen::sv_stub::mangled_accessor(
                                        dut_type,
                                        &p.name.name,
                                    ),
                                    force: p.force,
                                },
                            );
                        }
                    } else {
                        other_lets.push(l);
                    }
                }
                TestItem::Scope(s) => {
                    if let Some(r) = &s.run {
                        explicit_run = Some(r);
                    }
                }
                TestItem::Stmt(s) => bare_stmts.push(s),
                TestItem::Clock(c) => clocks.push(c),
                _ => {}
            }
        }
        if explicit_run.is_none() && bare_stmts.is_empty() {
            return Err(EmitError(format!(
                "test `{}` has no body — add a `scope sim`, `run`, or bare statements",
                test.name.name,
            )));
        }
        let _ = (&explicit_run, &bare_stmts);

        // ── Seed Emitter per-test state ────────────────────────────
        for l in &other_lets {
            if let Some(s) = type_simple_name(l.ty.as_ref()) {
                e.let_types.insert(l.name.name.clone(), s.to_string());
            }
            if let Some(TypeExpr::Named { mode: Some(m), .. }) = l.ty.as_ref() {
                e.let_modes.insert(l.name.name.clone(), *m);
            }
            // Track bit-widths for uint<W> / sint<W> / bits<W> lets so
            // the width-method intrinsics (`.trunc<N>()` / `.sext<N>()`
            // etc.) can statically check that the requested width
            // direction matches the source width, and so sext can emit
            // its shift-fill from the right MSB position.
            if let Some(TypeExpr::Builtin { name, args, .. }) = l.ty.as_ref() {
                if matches!(name, BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits) {
                    if let Some(w) = type_arg_width(args) {
                        e.let_widths.insert(l.name.name.clone(), w as u32);
                    }
                }
            }
            if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                if e.transactions.contains(simple)
                    || e.scoreboards.contains(simple)
                    || e.components.contains_key(simple)
                    || e.covergroups.contains_key(simple)
                    || e.buses.contains_key(simple)
                    || e.transactors.contains_key(simple)
                    || e.regblocks.contains_key(simple)
                    || e.addrmaps.contains_key(simple)
                {
                    continue;
                }
            }
            if matches!(&l.ty, Some(TypeExpr::Named { .. })) {
                e.pointer_vars.insert(l.name.name.clone());
            }
        }
        e.clock_names = clocks.iter().map(|c| c.name.name.clone()).collect();

        writeln!(
            e.out,
            "int run_{}(int argc, char** argv) {{",
            test.name.name
        )
        .ok();
        writeln!(e.out, "{INDENT}Verilated::commandArgs(argc, argv);").ok();
        writeln!(e.out, "{INDENT}V{dut_type}* dut = new V{dut_type};").ok();
        writeln!(e.out, "{INDENT}int errors = 0;").ok();
        // Per spec §7.7: `log(fatal, ...)` aborts this test instance at
        // the end of the current cycle. The flag is checked by the
        // main simulation-loop guard below.
        writeln!(e.out, "{INDENT}bool _fatal = false;").ok();
        writeln!(e.out, "{INDENT}int cycle_count = 0;").ok();
        writeln!(e.out, "").ok();
        // Seed PRNG from HARC_SEED env (or 1 if unset). Logged after sim_log_line
        // is defined so it lands in sim.log along with normal test output.
        writeln!(e.out, "{INDENT}{{ const char* s = std::getenv(\"HARC_SEED\"); harc_rng_state = s ? std::strtoull(s, nullptr, 0) : 1ULL; }}").ok();
        writeln!(e.out, "{INDENT}HarcTraceWriter trace;").ok();
        writeln!(e.out, "{INDENT}trace.open_env();").ok();
        writeln!(e.out, "{INDENT}trace.meta(harc_rng_state, std::getenv(\"HARC_DUT_BACKEND\"), \"{dut_type}\", \"{}\");", test.name.name).ok();
        writeln!(
            e.out,
            "{INDENT}trace.raw(\"sim_start\", cycle_count, \"\");"
        )
        .ok();
        writeln!(e.out, "").ok();
        // sim.log captures every log()/assert/fail line with cycle + severity
        // prefix. Path is configurable via the HARC_SIM_LOG env var (so the
        // outer harness can put it in the build dir); default `sim.log` in cwd.
        writeln!(
            e.out,
            "{INDENT}const char* sim_log_path = std::getenv(\"HARC_SIM_LOG\");"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}if (!sim_log_path) sim_log_path = \"sim.log\";"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}FILE* sim_log = std::fopen(sim_log_path, \"w\");"
        )
        .ok();
        writeln!(e.out, "").ok();
        // Concurrent assertion hook — every `assert property <expr>` /
        // `assert property NAME` registers a closure here; tick() invokes the
        // whole list after each `eval()`. Same-cycle (`|->`) and one-cycle
        // (`|=>`) properties run on every primary-clock edge.
        writeln!(
            e.out,
            "{INDENT}std::vector<std::function<void()>> _checkers;"
        )
        .ok();
        writeln!(e.out, "").ok();

        if clocks.is_empty() {
            // Single-clock backward-compat path: drives `dut->clk`. cycle_count
            // increments once per tick. Used by tests that don't declare any
            // `clock <name> = <period>` items.
            writeln!(e.out, "{INDENT}auto tick = [&]() {{").ok();
            writeln!(e.out, "{INDENT}{INDENT}dut->clk = 0; dut->eval();").ok();
            writeln!(e.out, "{INDENT}{INDENT}dut->clk = 1; dut->eval();").ok();
            writeln!(e.out, "{INDENT}{INDENT}cycle_count++;").ok();
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}};").ok();
            writeln!(e.out, "").ok();
        } else {
            // Multi-clock scheduler: every declared clock keeps its own next-edge
            // timestamp; we advance simulation time to the earliest pending edge,
            // toggle that clock, call eval(). cycle_count tracks rising edges of
            // the primary clock (first-declared) so existing log lines remain
            // meaningful.
            writeln!(e.out, "{INDENT}long long now_ps = 0;").ok();
            writeln!(e.out, "{INDENT}struct ClockState {{ const char* name; long long half_period_ps; long long next_edge_ps; int level; long long rising_count; }};").ok();
            writeln!(e.out, "{INDENT}std::vector<ClockState> clocks_;").ok();
            for c in &clocks {
                // Period source: time literal `5ns` OR domain reference `FastDomain`
                // (looked up in the domain table → derived from freq_mhz).
                let period_ps = match &*c.period.kind {
                ExprKind::Time(s) => time_literal_to_ps(s).map_err(EmitError)?,
                ExprKind::Ident(id) => *domains.get(&id.name).ok_or_else(|| EmitError(format!(
                    "clock {} references domain `{}` but no `domain {}` declaration was found in any input file",
                    c.name.name, id.name, id.name
                )))?,
                _ => return Err(EmitError(format!(
                    "clock {} period must be a time literal (e.g. 5ns) or a domain name (e.g. FastDomain)",
                    c.name.name
                ))),
            };
                let half = period_ps / 2;
                // First edge fires at half_period (rising) so initial state is 0.
                writeln!(
                    e.out,
                    "{INDENT}clocks_.push_back(ClockState{{\"{}\", {half}, {half}, 0, 0}});",
                    c.name.name
                )
                .ok();
                writeln!(e.out, "{INDENT}dut->{} = 0;", c.name.name).ok();
            }
            writeln!(e.out, "").ok();
            writeln!(
                e.out,
                "{INDENT}auto eval_clocks_until = [&](long long t_ps) {{"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}while (now_ps < t_ps) {{").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}long long next = t_ps;").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}for (auto& c : clocks_) if (c.next_edge_ps < next) next = c.next_edge_ps;").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}now_ps = next;").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}for (size_t i = 0; i < clocks_.size(); i++) {{"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}auto& c = clocks_[i];"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}if (c.next_edge_ps == now_ps) {{"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.level = !c.level;"
            )
            .ok();
            // Per-clock signal write — done by name lookup.
            for (idx, c) in clocks.iter().enumerate() {
                writeln!(
                    e.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == {idx}) dut->{} = c.level;",
                    c.name.name
                )
                .ok();
            }
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.next_edge_ps += c.half_period_ps;"
            )
            .ok();
            // Per-clock rising-edge count (consumed by `wait N cycles on <clock>`).
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (c.level == 1) c.rising_count++;"
            )
            .ok();
            // Primary clock rising edge bumps cycle_count.
            writeln!(
            e.out,
            "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == 0 && c.level == 1) cycle_count++;"
        )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}}}").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}}}").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}dut->eval();").ok();
            writeln!(e.out, "{INDENT}{INDENT}}}").ok();
            writeln!(e.out, "{INDENT}}};").ok();
            writeln!(e.out, "").ok();
            // `tick()` advances by one full primary clock period (one rising
            // edge). Other clocks tick at their natural rate during this span.
            writeln!(e.out, "{INDENT}auto tick = [&]() {{").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}long long target = now_ps + clocks_[0].half_period_ps * 2;"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}eval_clocks_until(target);").ok();
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}};").ok();
            writeln!(e.out, "").ok();
        }
        // Variadic so log()/assert/fail callers can pass printf-style args
        // produced by `${expr}` string-interpolation lowering. Format string
        // and varargs are evaluated twice (once per sink) — no shared state.
        // Per-file log handles, opened on first reference, closed at exit.
        // Relative paths are anchored to HARC_LOG_DIR (set by `harc sim` to the
        // outdir) so per-component files land next to sim.log.
        writeln!(
            e.out,
            "{INDENT}std::unordered_map<std::string, FILE*> log_files;"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}auto resolve_log_path = [&](const char* path) -> std::string {{"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}if (path[0] == '/') return std::string(path);"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}const char* base = std::getenv(\"HARC_LOG_DIR\");"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}if (base) return std::string(base) + \"/\" + path;"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}return std::string(path);").ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(
            e.out,
            "{INDENT}auto get_log_file = [&](const char* path) -> FILE* {{"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}std::string resolved = resolve_log_path(path);"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}auto it = log_files.find(resolved);").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}if (it != log_files.end()) return it->second;"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}FILE* f = std::fopen(resolved.c_str(), \"w\");"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}log_files[resolved] = f;").ok();
        writeln!(e.out, "{INDENT}{INDENT}return f;").ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(e.out, "").ok();
        writeln!(
            e.out,
            "{INDENT}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}va_list ap;").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}std::printf(\"[cycle:%d %s] \", cycle_count, sev);"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}va_start(ap, fmt); std::vprintf(fmt, ap); va_end(ap);"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}std::printf(\"\\n\");").ok();
        writeln!(e.out, "{INDENT}{INDENT}if (f) {{").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}{INDENT}std::fprintf(f, \"[cycle:%d %s] \", cycle_count, sev);"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}{INDENT}va_start(ap, fmt); std::vfprintf(f, fmt, ap); va_end(ap);"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fprintf(f, \"\\n\");").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fflush(f);").ok();
        writeln!(e.out, "{INDENT}{INDENT}}}").ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(e.out, "").ok();
        // After sim_log_line below is defined, emit the seed line so it lands
        // in sim.log on every run — required for reproducing failures.
        let log_seed = true;

        writeln!(
            e.out,
            "{INDENT}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}va_list ap;").ok();
        writeln!(e.out, "{INDENT}{INDENT}char _trace_msg[4096];").ok();
        writeln!(e.out, "{INDENT}{INDENT}va_start(ap, fmt); std::vsnprintf(_trace_msg, sizeof(_trace_msg), fmt, ap); va_end(ap);").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}std::printf(\"[cycle:%d %s] \", cycle_count, sev);"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}std::printf(\"%s\", _trace_msg);").ok();
        writeln!(e.out, "{INDENT}{INDENT}std::printf(\"\\n\");").ok();
        writeln!(e.out, "{INDENT}{INDENT}if (sim_log) {{").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}{INDENT}std::fprintf(sim_log, \"[cycle:%d %s] \", cycle_count, sev);"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}{INDENT}std::fprintf(sim_log, \"%s\", _trace_msg);"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}{INDENT}std::fprintf(sim_log, \"\\n\");"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fflush(sim_log);").ok();
        writeln!(e.out, "{INDENT}{INDENT}}}").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}trace.log(cycle_count, sev, _trace_msg);"
        )
        .ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(e.out, "").ok();

        if log_seed {
            writeln!(
                e.out,
                "{INDENT}sim_log_line(\"INFO\", \"seed=%llu\", (long long)harc_rng_state);"
            )
            .ok();
            writeln!(e.out, "").ok();
        }

        // User-defined functions become lambdas. Emitted before the test body so
        // the body can call them. Capture-all (`[&]`) so they see `dut`/`tick`/
        // any test-level let bindings.
        for f in &funcs {
            e.emit_function(f, 1);
        }
        if !funcs.is_empty() {
            writeln!(e.out, "").ok();
        }
        // Tseqs lower to lambdas returning `std::vector<T>`; emitted alongside
        // functions so the run-block can invoke them and consume the result.
        for t in &tseqs {
            e.emit_tseq(t, 1);
        }
        if !tseqs.is_empty() {
            writeln!(e.out, "").ok();
        }
        // Hookable methods on driver / agent / env / sequencer / scoreboard
        // become free `[&]`-capturing lambdas named `<Type>_<method>`. The
        // method-call site rewrites `obj.method(args)` to
        // `<Type>_<method>(obj, args)` so the body sees `tick` / `_checkers`
        // / etc. from the test scope.
        //
        // Pre/post hook vectors emit FIRST so the method bodies (and the
        // test-scope `on obj.method pre/post` registrations) can `[&]`-
        // capture them. Empty vectors are no-ops; the wrap is unconditional.
        let mut emitted_any_method = false;
        for it in &file.items {
            match it {
                Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                    for ci in &c.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_hook_vectors(c, h, 1);
                        }
                    }
                }
                Item::Transactor(t) => {
                    let synth = synth_component_from_transactor(t, /*include_active*/ true);
                    for ci in &synth.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_hook_vectors(&synth, h, 1);
                        }
                    }
                }
                _ => {}
            }
        }
        // Watchdog hook vectors must be emitted in the same forward-decl
        // pass as the hookable hook vectors — `on <Type>.watchdog pre/post`
        // captures them. The `emit_watchdog` helper below emits BOTH the
        // hook vectors AND the synthetic method body in one go, since the
        // method body refers to those vectors. So we forward-declare the
        // method via the same pass that emits hookable method lambdas
        // below; here, no separate forward decl is needed because the
        // method lambda + its hook vectors are emitted together at that
        // point.
        // Pre-scan: register test-scope bus bindings (`let axil :
        // BusAxiLite = bind dut`) and driver-type → binding mappings
        // BEFORE hookable methods emit. Hookables on `bound to BusType`
        // drivers need the binding active so `bus.<ch>.send/recv` and
        // `bus.<ch>.<sig>` resolve correctly. emit_let later re-registers
        // the bus bindings (idempotent — same key, same value).
        for it in &test.items {
            if let TestItem::Let(l) = it {
                if l.bind {
                    if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                        if let Some(bus_decl) = e.buses.get(simple).cloned() {
                            if let Some(v) = &l.value {
                                let mut buf = String::new();
                                std::mem::swap(&mut e.out, &mut buf);
                                e.emit_expr(v);
                                std::mem::swap(&mut e.out, &mut buf);
                                let prefix = l.name.name.clone();
                                e.bus_bindings
                                    .insert(l.name.name.clone(), (bus_decl, buf, prefix.clone()));
                                // Populate the per-bind signal remap so hookable-
                                // method emission (which precedes `emit_let`) can
                                // resolve `bus.<ch>.<sig>` against the override
                                // table. Without this pre-pass, transactor
                                // bodies use only the prefix-convention name and
                                // the override never fires for indirectly-routed
                                // accesses.
                                if !l.bind_remap.is_empty() {
                                    let mut map: std::collections::HashMap<
                                        (String, String),
                                        String,
                                    > = std::collections::HashMap::new();
                                    for entry in &l.bind_remap {
                                        if entry.path.len() == 2 {
                                            map.insert(
                                                (
                                                    entry.path[0].name.clone(),
                                                    entry.path[1].name.clone(),
                                                ),
                                                entry.port.clone(),
                                            );
                                        }
                                    }
                                    e.bus_remap.insert(prefix, map);
                                }
                            }
                        }
                    }
                }
            }
        }
        for it in &test.items {
            if let TestItem::Let(l) = it {
                if l.bind {
                    if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                        let bound_decl_to_bus = e
                            .components
                            .get(simple)
                            .and_then(|c| c.bound_to.as_ref().map(|_| ()))
                            .is_some()
                            || e.transactors
                                .get(simple)
                                .and_then(|t| t.bound_to.as_ref().map(|_| ()))
                                .is_some();
                        if bound_decl_to_bus {
                            if let Some(v) = &l.value {
                                if let ExprKind::Ident(rhs) = &*v.kind {
                                    if let Some(binding) = e.bus_bindings.get(&rhs.name).cloned() {
                                        // First binding wins; multi-instance is deferred.
                                        e.driver_bus_for_hookables
                                            .entry(simple.to_string())
                                            .or_insert(binding);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Hookable methods on regular components (agent / env /
        // sequencer / scoreboard) plus transactors. Transactor hookables
        // are emitted via the synth ComponentDecl so the existing
        // emit_component_method path finds the same struct shape.
        for it in &file.items {
            match it {
                Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                    for ci in &c.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_component_method(c, h, 1);
                            emitted_any_method = true;
                        }
                        if let ComponentItem::Watchdog(w) = ci {
                            e.emit_watchdog(c, w, 1);
                            emitted_any_method = true;
                        }
                    }
                }
                Item::Transactor(t) => {
                    // include_active = true so any hookable inside `when
                    // active` is also emitted. Active-only hookables on a
                    // passive instance still compile but won't be invoked
                    // at runtime (no input event firing).
                    let synth = synth_component_from_transactor(t, /*include_active*/ true);
                    for ci in &synth.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_component_method(&synth, h, 1);
                            emitted_any_method = true;
                        }
                        if let ComponentItem::Watchdog(w) = ci {
                            e.emit_watchdog(&synth, w, 1);
                            emitted_any_method = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if emitted_any_method {
            writeln!(e.out, "").ok();
        }

        // ── Scheduler — declared before hoisted lets so bound coroutine
        // drivers (Phase 2b) can register their slots inline at the
        // hoisted-let site. The run coroutine's slot is added below
        // alongside its body.
        writeln!(e.out, "{INDENT}harc_rt::ThreadScheduler sched;").ok();

        // Other lets get hoisted up front. Bound coroutine drivers also
        // emit their slot + per-driver transaction queue + actor
        // coroutine here (see the `let drv : Drv = bind axil` arm in
        // emit_let).
        for l in &other_lets {
            e.emit_let(l, 1);
        }

        // Custom `phase <name> ... end phase <name>` blocks from the test
        // lifecycle body (spec §7.2). Emitted after hoisted lets so
        // phase bodies can reference test-level env/agent/scoreboard
        // instances. Calls of the form `<name>()` from inside `run` lower as
        // plain C++ function calls; `wait` inside the phase takes the sync
        // `tick()` path because the lambda body emits with `in_coroutine =
        // false`.
        if !custom_phases.is_empty() {
            for (name, body) in &custom_phases {
                e.pad(1);
                writeln!(e.out, "auto {} = [&]() -> void {{", name.name).ok();
                e.emit_block(body, 2);
                e.pad(1);
                writeln!(e.out, "}};").ok();
            }
            writeln!(e.out, "").ok();
        }

        // ── Coroutine wrap for the test body ───────────────────────────────
        //
        // The whole test body (bare stmts + scope sim/{setup,run,check,
        // teardown}) becomes a single C++20 coroutine driven by
        // `harc_rt::ThreadScheduler`. Setting `in_coroutine = true` flips
        // the wait/tick lowering inside this scope to emit
        // `co_await harc_rt::wait_cycles(_slot, N)` instead of the
        // synchronous `for (...) tick();` form.
        //
        // The lambda captures by reference (`[&]`) so it sees `dut`,
        // `tick`, `cycle_count`, `_checkers`, hookable-method lambdas,
        // and any hoisted lets defined above in `main`'s scope.
        //
        // After the coroutine is constructed, `sched.bootstrap()` resumes
        // every initially-Ready slot once (the run setup statements,
        // plus any bound-driver actors' first wait_until on their queue).
        // The main loop then drives the clock until the run coroutine is
        // `Done` — driver coroutines may still be parked in WaitUntil
        // (queue empty) at that point; abandoning them is intentional
        // since the test is over.
        writeln!(e.out, "{INDENT}harc_rt::ThreadSlot _run_slot;").ok();
        writeln!(e.out, "{INDENT}sched.slots.push_back(&_run_slot);").ok();
        writeln!(
            e.out,
            "{INDENT}_run_slot.thread = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
        )
        .ok();

        e.in_coroutine = true;
        for it in &test.items {
            match it {
                TestItem::Stmt(s) => e.emit_stmt(s, 2),
                TestItem::Scope(s) => {
                    if let Some(b) = &s.setup {
                        e.emit_block(b, 2);
                    }
                    if let Some(b) = &s.run {
                        e.emit_block(b, 2);
                    }
                    if let Some(b) = &s.check {
                        e.emit_block(b, 2);
                    }
                    if let Some(b) = &s.teardown {
                        e.emit_block(b, 2);
                    }
                }
                _ => {}
            }
        }
        e.in_coroutine = false;

        writeln!(e.out, "{INDENT}{INDENT}co_return;").ok();
        writeln!(e.out, "{INDENT}}}(&_run_slot);").ok();
        writeln!(e.out, "").ok();

        // `actor_threads` is populated only when `--mt` is set (cooperative
        // mode pushes actor slots into the global `sched` instead). So
        // `mt` here means "we have per-actor schedulers needing barrier
        // sync"; cooperative mode skips the worker spawn / barrier dance
        // entirely even when actors are present.
        let n_actors = e.actor_threads.len();
        let mt = n_actors > 0;
        debug_assert!(mt == (e.mt && !e.actor_threads.is_empty()));
        let _ = e.mt; // suppress unused warning when no actors

        // ── Bootstrap ──────────────────────────────────────────────────────
        // Single-threaded — workers haven't started yet. Each scheduler
        // (main + per-actor) runs its initially-Ready slots once until
        // they hit their first co_await.
        writeln!(
            e.out,
            "{INDENT}// Resume each coroutine once so initial-setup statements run"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// before the first clock edge. Single-threaded — workers"
        )
        .ok();
        writeln!(e.out, "{INDENT}// haven't been spawned yet.").ok();
        writeln!(e.out, "{INDENT}sched.bootstrap();").ok();
        if mt {
            for (sched_var, _) in &e.actor_threads {
                writeln!(e.out, "{INDENT}{sched_var}.bootstrap();").ok();
            }
        }
        writeln!(e.out, "").ok();

        if mt {
            // ── Phase 3a multi-thread topology ──────────────────────────
            // Each actor coroutine runs on its own `std::thread`. Per
            // posedge: main runs the run-coroutine, two atomic-spin
            // barriers synchronize main with N worker threads, each
            // worker runs `_<n>_sched.tick()` once. Then main does
            // `dut->eval()` (single-threaded — Verilator-generated DUT
            // code is not MT-safe) and runs `_checkers`. The dual
            // barrier mirrors arch-com's Phase 3 design (`Barrier` class
            // shared in `harc_thread_rt.h`); cycle batching to amortize
            // barrier cost is Phase 3b.
            writeln!(
                e.out,
                "{INDENT}// Phase 3a: per-actor OS threads with dual barrier sync."
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// {} actor(s) → {} barrier participants (main + workers).",
                n_actors,
                n_actors + 1
            )
            .ok();
            writeln!(e.out, "{INDENT}std::atomic<bool> _shutdown{{false}};").ok();
            writeln!(
                e.out,
                "{INDENT}harc_rt::Barrier _start_barrier({});",
                n_actors + 1
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}harc_rt::Barrier _end_barrier({});",
                n_actors + 1
            )
            .ok();
            writeln!(e.out, "{INDENT}std::vector<std::thread> _workers;").ok();
            for (sched_var, _) in &e.actor_threads {
                writeln!(e.out, "{INDENT}_workers.emplace_back([&]() {{").ok();
                writeln!(e.out, "{INDENT}{INDENT}while (true) {{").ok();
                writeln!(e.out, "{INDENT}{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(
                    e.out,
                    "{INDENT}{INDENT}{INDENT}if (_shutdown.load(std::memory_order_acquire)) break;"
                )
                .ok();
                writeln!(e.out, "{INDENT}{INDENT}{INDENT}{sched_var}.tick();").ok();
                writeln!(e.out, "{INDENT}{INDENT}{INDENT}_end_barrier.wait();").ok();
                writeln!(e.out, "{INDENT}{INDENT}}}").ok();
                writeln!(e.out, "{INDENT}}});").ok();
            }
            writeln!(e.out, "").ok();
        }

        writeln!(
            e.out,
            "{INDENT}// Drive the clock until the run coroutine completes."
        )
        .ok();
        writeln!(e.out, "{INDENT}//").ok();
        writeln!(
            e.out,
            "{INDENT}// `wait N cycles` matches Verilog's `@(posedge clk)` semantic:"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// values set in the segment BEFORE the wait are sampled at the"
        )
        .ok();
        writeln!(e.out, "{INDENT}// next posedge. Per loop iteration:").ok();
        writeln!(
            e.out,
            "{INDENT}//   1. Posedge (clk 0→1, eval) — DUT FFs latch the current input"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}//      values (set in the previous segment, or in bootstrap on"
        )
        .ok();
        writeln!(e.out, "{INDENT}//      the first iteration).").ok();
        writeln!(
            e.out,
            "{INDENT}//   2. `sched.tick()` — advance the run coroutine to its next"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}//      wait, setting the inputs for the NEXT cycle's posedge."
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}//   3. Falling edge (clk 1→0, eval) — comb re-settles with the"
        )
        .ok();
        writeln!(e.out, "{INDENT}//      newly-set inputs.").ok();
        writeln!(e.out, "{INDENT}//   4. Cycle counter + checkers.").ok();
        writeln!(
            e.out,
            "{INDENT}// One initial `eval(clk=0)` before the loop settles combinational"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// logic with the bootstrap inputs — same role as `initial`-block"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// settle in Verilog. Each `wait 1 cycle` then maps to exactly"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// one posedge that observes the just-set values."
        )
        .ok();
        if mt {
            writeln!(e.out, "{INDENT}//").ok();
            writeln!(
                e.out,
                "{INDENT}// MT mode: workers run between tick() and the falling edge,"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// gated by _start_barrier / _end_barrier. Run-coroutine writes"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// complete BEFORE workers wake → no race on shared queues."
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// Workers' DUT-input writes complete BEFORE the falling-edge"
            )
            .ok();
            writeln!(e.out, "{INDENT}// eval → no race on signal state.").ok();
        }
        if clocks.is_empty() {
            // Initial comb settle — bootstrap's inputs propagate through
            // combinational logic before the first posedge.
            writeln!(e.out, "{INDENT}dut->clk = 0; dut->eval();").ok();
            writeln!(
                e.out,
                "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {{"
            )
            .ok();
            // Posedge first — latches current input values.
            writeln!(e.out, "{INDENT}{INDENT}dut->clk = 1; dut->eval();").ok();
            // Then advance the run coroutine for the next cycle's inputs.
            writeln!(e.out, "{INDENT}{INDENT}sched.tick();").ok();
            if mt {
                writeln!(e.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(e.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
            }
            // Falling edge + comb resettle with the new inputs.
            writeln!(e.out, "{INDENT}{INDENT}dut->clk = 0; dut->eval();").ok();
            writeln!(e.out, "{INDENT}{INDENT}cycle_count++;").ok();
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}}").ok();
        } else {
            // Multi-clock: initial bare eval() to settle combinational
            // logic with bootstrap inputs (no clock advancement). The
            // loop's eval_clocks_until then advances time by one full
            // primary-clock period per iteration. Posedge-vs-tick ordering
            // is constrained by eval_clocks_until's atomic per-edge eval
            // — we tick AFTER eval_clocks_until so the run coroutine
            // observes the just-completed cycle's outputs and sets the
            // next cycle's inputs in time for the following iteration's
            // first edge. Same effect as the single-clock branch, just
            // with the clock toggling factored into eval_clocks_until.
            writeln!(e.out, "{INDENT}dut->eval();").ok();
            writeln!(
                e.out,
                "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {{"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}long long _target = now_ps + clocks_[0].half_period_ps * 2;"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}eval_clocks_until(_target);").ok();
            writeln!(e.out, "{INDENT}{INDENT}sched.tick();").ok();
            if mt {
                writeln!(e.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(e.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
            }
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}}").ok();
        }

        if mt {
            // Shutdown sequence: workers are blocked on _start_barrier
            // (their next iteration). Set _shutdown, wake them via the
            // start barrier; they observe the flag and break out of their
            // loop without reaching _end_barrier. Then join.
            writeln!(e.out, "").ok();
            writeln!(
                e.out,
                "{INDENT}_shutdown.store(true, std::memory_order_release);"
            )
            .ok();
            writeln!(e.out, "{INDENT}_start_barrier.wait();").ok();
            writeln!(e.out, "{INDENT}for (auto& _w : _workers) _w.join();").ok();
        }
        writeln!(e.out, "").ok();

        // Final + return.
        writeln!(e.out, "").ok();
        // Property-cover summary (ARCH-style: header line + per-point lines
        // with `*NOT HIT*` marker; stdout destination — see covergroup
        // report() for the rationale on stdout vs stderr).
        if !e.covers.is_empty() {
            let n_covers = e.covers.len();
            writeln!(e.out, "{INDENT}{{").ok();
            writeln!(e.out, "{INDENT}{INDENT}uint64_t _cov_total = {n_covers};").ok();
            writeln!(e.out, "{INDENT}{INDENT}uint64_t _cov_hit = 0;").ok();
            let covers_clone = e.covers.clone();
            for c in &covers_clone {
                writeln!(
                    e.out,
                    "{INDENT}{INDENT}if (_cov_{tag}_hits > 0) _cov_hit++;",
                    tag = c.tag
                )
                .ok();
            }
            writeln!(e.out, "{INDENT}{INDENT}std::printf(\"[cover] %llu/%llu hit (%.1f%%)\\n\", (unsigned long long)_cov_hit, (unsigned long long)_cov_total, _cov_total ? (100.0 * _cov_hit / _cov_total) : 0.0);").ok();
            for c in &covers_clone {
                writeln!(e.out,
                "{INDENT}{INDENT}std::printf(\"  [{label}]: %llu hits%s\\n\", (unsigned long long)_cov_{tag}_hits, _cov_{tag}_hits ? \"\" : \" *NOT HIT*\");",
                tag = c.tag, label = escape_c(&c.label)).ok();
            }
            writeln!(e.out, "{INDENT}}}").ok();
        }
        writeln!(e.out, "{INDENT}dut->final();").ok();
        // Verilator coverage write — only does anything when the TB was
        // built with `harc sim --coverage` (which sets `--coverage` on
        // verilator → defines `VM_COVERAGE=1`). The .dat file lands in
        // $HARC_LOG_DIR (set by `harc sim`) so the CVDP-style scorer can
        // post-process it with `verilator_coverage`. Always-emitted-but-
        // ifdef'd so a coverage-built TB is the only difference.
        writeln!(e.out, "#if VM_COVERAGE").ok();
        writeln!(e.out, "{INDENT}{{").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}const char* _cov_dir = std::getenv(\"HARC_LOG_DIR\");"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}std::string _cov_path = _cov_dir ? std::string(_cov_dir) + \"/coverage.dat\" : std::string(\"coverage.dat\");").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}Verilated::threadContextp()->coveragep()->write(_cov_path.c_str());"
        )
        .ok();
        writeln!(e.out, "{INDENT}}}").ok();
        writeln!(e.out, "#endif").ok();
        writeln!(e.out, "{INDENT}delete dut;").ok();
        writeln!(e.out, "{INDENT}if (sim_log) std::fclose(sim_log);").ok();
        writeln!(
            e.out,
            "{INDENT}for (auto& kv : log_files) {{ if (kv.second) std::fclose(kv.second); }}"
        )
        .ok();
        writeln!(e.out, "{INDENT}trace.raw(\"sim_end\", cycle_count, \"\\\"errors\\\":\" + std::to_string(errors));").ok();
        writeln!(e.out, "{INDENT}trace.close();").ok();
        writeln!(e.out, "").ok();
        writeln!(
            e.out,
            "{INDENT}if (errors == 0) {{ std::printf(\"\\nALL TESTS PASSED\\n\"); return 0; }}"
        )
        .ok();
        writeln!(
        e.out,
        "{INDENT}else             {{ std::printf(\"\\n%d TESTS FAILED\\n\", errors); return 1; }}"
    )
        .ok();
        writeln!(e.out, "}}").ok();
        writeln!(e.out, "").ok();
    } // end of `for test in &tests`

    // Dispatcher `main()` (Phase 1b). One branch per test: the
    // dispatcher reads `--test <name>` from argv (preferred) or the
    // `HARC_TEST` env var, then `return run_<TestName>(argc, argv)`.
    // When unset, dispatches to the alphabetically-first test
    // (deterministic — matches the sort key in merge_for_sim).
    writeln!(e.out, "int main(int argc, char** argv) {{").ok();
    writeln!(
        e.out,
        "{INDENT}const char* test_sel = std::getenv(\"HARC_TEST\");"
    )
    .ok();
    writeln!(e.out, "{INDENT}for (int i = 1; i + 1 < argc; i++) {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}if (std::strcmp(argv[i], \"--test\") == 0) {{ test_sel = argv[i + 1]; break; }}").ok();
    writeln!(e.out, "{INDENT}}}").ok();
    // Default (no --test, no HARC_TEST) → run the first test.
    let first_test = &tests[0].name.name;
    writeln!(
        e.out,
        "{INDENT}if (!test_sel) return run_{first_test}(argc, argv);"
    )
    .ok();
    // Branches for every test.
    for t in &tests {
        let n = &t.name.name;
        writeln!(
            e.out,
            "{INDENT}if (std::strcmp(test_sel, \"{n}\") == 0) return run_{n}(argc, argv);"
        )
        .ok();
    }
    // Build the human-readable "available tests" list for the
    // unknown-test error message.
    let avail: Vec<&str> = tests.iter().map(|t| t.name.name.as_str()).collect();
    let avail_csv = avail.join(", ");
    writeln!(
        e.out,
        "{INDENT}std::fprintf(stderr, \"unknown test: %s (available: {avail_csv})\\n\", test_sel);"
    )
    .ok();
    writeln!(e.out, "{INDENT}return 1;").ok();
    writeln!(e.out, "}}").ok();

    if !e.errors.is_empty() {
        return Err(EmitError(e.errors.join("\n")));
    }
    Ok(e.out)
}

/// Build a synthetic `ComponentDecl` view of a transactor for codegen
/// reuse. The transactor's always-present `items` + (optionally) its
/// `when_active` items become a single component-shaped item list that
/// existing emit paths (`emit_component_struct`,
/// `emit_bound_monitor_actors`, `try_emit_bound_driver_actor`) can
/// consume without modification. `include_active = false` produces the
/// passive view: only the always-present body, no `when active` fields
/// or handlers — which is what gets used for `let xact : T passive`
/// instantiations so passive instances don't even attempt to spawn the
/// active driver actor.
///
/// `kind: ComponentKind::Driver` is a placeholder — the existing
/// component codegen paths only consult `kind` for the tag prefix in
/// diagnostic strings; correctness doesn't depend on the choice.
fn synth_component_from_transactor(t: &TransactorDecl, include_active: bool) -> ComponentDecl {
    let mut items = t.items.clone();
    if include_active {
        if let Some(active) = &t.when_active {
            items.extend(active.clone());
        }
    }
    ComponentDecl {
        kind: ComponentKind::Transactor,
        name: t.name.clone(),
        params: t.params.clone(),
        bound_to: t.bound_to.clone(),
        items,
        span: t.span,
        doc: t.doc.clone(),
        inner_doc: t.inner_doc.clone(),
    }
}

fn type_simple_name(t: Option<&TypeExpr>) -> Option<&str> {
    match t? {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

/// One registered cover point — what gets reported at end of test.
#[derive(Debug, Clone)]
struct CoverInfo {
    tag: String,
    label: String,
}

enum WaitCondition<'a> {
    Expr(&'a Expr),
    Idle { instance: String, cycles: &'a Expr },
}

impl<'a> WaitCondition<'a> {
    fn label(&self) -> String {
        match self {
            WaitCondition::Expr(e) => expr_source_str(e),
            WaitCondition::Idle { instance, cycles } => {
                format!("{instance}.idle({})", expr_source_str(cycles))
            }
        }
    }

    fn emit(&self, e: &mut Emitter) {
        match self {
            WaitCondition::Expr(expr) => e.emit_expr(expr),
            WaitCondition::Idle { instance, cycles } => {
                e.emit_idle_predicate(instance, "idle", cycles);
            }
        }
    }
}

/// Width-typed transaction field info used by the Z3 solver lowering.
#[derive(Debug, Clone)]
struct TxnFieldInfo {
    name: String,
    width: u32,
    signed: bool,
    enum_variants: Option<usize>,
    /// `!` prefix on a transaction field — pinned to the current value during
    /// solver-backed randomize and skipped during model assignment.
    non_random: bool,
    attrs: Vec<Attr>,
}

fn field_attr_range(f: &TxnFieldInfo) -> Option<(&Expr, &Expr)> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "range" || a.args.len() < 2 {
            return None;
        }
        match (&a.args[0], &a.args[1]) {
            (AttrArg::Expr(lo), AttrArg::Expr(hi)) => Some((lo, hi)),
            _ => None,
        }
    })
}

fn field_attr_dist_entries(f: &TxnFieldInfo) -> Option<&Vec<DistEntry>> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "dist" {
            return None;
        }
        a.args.iter().find_map(|arg| match arg {
            AttrArg::Dist(entries) => Some(entries),
            _ => None,
        })
    })
}

fn randomize_target_field_name(
    target: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
) -> Option<String> {
    match &*target.kind {
        ExprKind::Field { target, name } if matches!(&*target.kind, ExprKind::Ident(_)) => {
            field_info
                .contains_key(&name.name)
                .then(|| name.name.clone())
        }
        ExprKind::Ident(id) if field_info.contains_key(&id.name) => Some(id.name.clone()),
        ExprKind::Paren(inner) => randomize_target_field_name(inner, field_info),
        _ => None,
    }
}

/// Per-probe codegen state. The mangled read accessor is the bare
/// path `<TopModule>__DOT__harc_probes__DOT__<name>`. The `_drv`
/// and `_en` siblings (force probes only) are derived by appending
/// the suffix; see `ProbeAccessor::drive` / `enable`.
#[derive(Clone, Debug)]
struct ProbeAccessor {
    read: String,
    force: bool,
}

impl ProbeAccessor {
    fn drive(&self) -> String {
        format!("{}_drv", self.read)
    }
    fn enable(&self) -> String {
        format!("{}_en", self.read)
    }
}

struct Emitter {
    out: String,
    errors: Vec<String>,
    /// Transaction name → field metadata. Populated alongside `transactions`
    /// before main-body emission so the solver block can declare Z3 vars
    /// of the right widths and walk only the fields it should write back.
    txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>>,
    /// Per-transaction `keep` constraints (spec §4). `randomize(t)`
    /// — bare or with `with …` — merges these into the Z3 solver
    /// block so the constraints are enforced. Empty entry / missing
    /// key means the transaction has no `keep`s; bare `randomize(t)`
    /// stays on the fast PRNG path.
    txn_keeps: std::collections::HashMap<String, Vec<Expr>>,
    /// Map from `probe_name` to the mangled accessor string used after
    /// `dut->rootp->`. Populated from the test's `let dut : T { probe ... }`
    /// block at emit start; consulted when lowering `dut.<X>` so that
    /// probe reads/writes route through the Verilator-bind stub instead of
    /// the (non-existent) top-level signal of the original DUT. Empty for
    /// probe-less tests — current 76 fixtures all sit in this bucket.
    probes: std::collections::HashMap<String, ProbeAccessor>,
    /// RAL regblock declarations indexed by type name. See ast.rs::
    /// `RegblockDecl`. Used to emit one POD mirror struct +
    /// `constexpr` address table per declared regblock at file scope,
    /// and to lower `regs.NAME` field accesses against the bound
    /// helper transactor at call sites.
    regblocks: std::collections::HashMap<String, RegblockDecl>,
    /// RAL addrmap declarations indexed by type name. Chip-level
    /// container; each instance owns one regblock at a distinct base
    /// address. See docs/ral-support.md §4.
    addrmaps: std::collections::HashMap<String, AddrmapDecl>,
    /// Per-test map from a `let regs : <RegblockType> = bind <helper>`
    /// variable name to its helper transactor variable name. Populated
    /// when emitting the test's `let`s; consulted when lowering
    /// `regs.<reg>` accessors so the emitted call resolves to
    /// `<helper>.write(addr, val)` / `<helper>.read(addr)`.
    let_helper: std::collections::HashMap<String, String>,
    /// Free-standing `relation` declarations indexed by name (spec
    /// §4.2). `Call(Ident(R), args)` inside a constraint expression
    /// expands to R's body with formal parameters substituted by the
    /// call arguments. Block-body relations contribute one
    /// constraint per body expression; alias-body relations
    /// contribute one. Expansion is recursive so a relation that
    /// calls another relation flattens fully before reaching Z3.
    relations: std::collections::HashMap<String, RelationDecl>,
    /// Identifiers that are emitted as Verilator-class pointers (`VFoo*`).
    /// Field access on these uses `->` instead of `.`. Populated from
    /// `let x : <NamedType>` declarations and from function parameters
    /// whose type is a Named user type. Function-scoped: entries added by
    /// a function's params are popped when the function body finishes.
    pointer_vars: std::collections::HashSet<String>,
    /// Identifier → simple type name, for `randomize(t)` to resolve which
    /// `randomize_T()` function to call. Populated by `let t : T` and
    /// function parameters.
    let_types: std::collections::HashMap<String, String>,
    /// Per-test bit-widths for `let X : uint<W>` (and sint/bits)
    /// declarations. Used by the width-method intrinsics
    /// (`.trunc<N>()` / `.zext<N>()` / `.sext<N>()` / `.resize<N>()`)
    /// to (a) reject wrong-direction casts at codegen time and (b)
    /// give sext the correct source-width for its shift-fill. Lets
    /// without an explicit type (`let x = expr`) don't populate this
    /// map — width-direction checks then fall back to best-effort
    /// inference on the RHS expression.
    let_widths: std::collections::HashMap<String, u32>,
    /// Identifier → transactor mode, for let-bindings that carry a mode
    /// annotation (`let x : T active`, `let env : E passive`). The mode
    /// is recorded for the root let-name and forms the inheritance root
    /// for nested sub-component / sub-transactor fields; resolution at
    /// any path walks down field-by-field, with field-explicit modes
    /// overriding the inherited mode (same model as
    /// `emit_subcomponent_handler_registrations`). Used by the
    /// call-site passive-mode check (spec §8.1): a method call that
    /// resolves to a hookable inside `T.when_active` on a passive
    /// instance is a HARC-level error.
    let_modes: std::collections::HashMap<String, TransactorMode>,
    /// Set of transaction type names emitted in this file — guards against
    /// `randomize(t)` against a non-transaction type.
    transactions: std::collections::HashSet<String>,
    /// Set of scoreboard type names emitted in this file. `let sb : X`
    /// where X is in this set lowers to `X sb;` (default-constructed)
    /// rather than the int64_t fallback.
    scoreboards: std::collections::HashSet<String>,
    /// Covergroup declarations indexed by name. `let cov : G` instantiates
    /// the struct and registers its `sample()` as a rising-edge `_checkers`
    /// closure on the primary clock. Bin counters live as `uint64_t` fields
    /// inside the struct; user-side access is `cov.cp_name.bin_name`.
    covergroups: std::collections::HashMap<String, CovergroupDecl>,
    /// Per-test list of registered `cover <expr>` points. Each entry pairs
    /// a unique tag (used to name the static hit counter in C++) with a
    /// human label for the report. Aggregated end-of-main report iterates
    /// the list and prints `[cover] H/T hit (X.X%)` plus per-point lines.
    covers: Vec<CoverInfo>,
    /// In-flight bare-identifier substitutions during monitor-body
    /// emission. Empty outside the monitor's handler emission. Lets the
    /// body reference its own fields by short name (`emit write_e(...)`)
    /// while the codegen prefixes with the let-instance name.
    field_subs: std::collections::HashMap<String, String>,
    /// Enum name → variant count; used when randomizing enum-typed fields.
    enums: std::collections::HashMap<String, usize>,
    /// Global enum-variant-name → numeric index. Used by the Z3
    /// constraint translator to resolve bare references to enum
    /// variants (e.g. `keep op != WRAP`) into their numeric encoding.
    enum_variants: std::collections::HashMap<String, i64>,
    /// Property name → body expression. Populated up-front from
    /// `Item::Property` declarations so `assert property NAME` can be
    /// resolved at the call site without a separate pass.
    properties: std::collections::HashMap<String, Expr>,
    /// In-flight substitutions for temporal SystemCall expressions during
    /// property emission. Keyed by AST span so the rewrite finds the right
    /// occurrence. Set/cleared around each `emit_property_check` call.
    prop_subs: std::collections::HashMap<(usize, usize), String>,
    /// Event name → C++ inner type (e.g. `uint64_t`). Populated when
    /// emitting `let e : event<T>`; consulted when emitting `emit e(arg)`
    /// and `on e(arg) ... end on` so the lambda gets the right param type.
    event_types: std::collections::HashMap<String, String>,
    /// Names of declared clocks in declaration order. Empty under
    /// single-clock backward-compat. Used by `wait N cycles on <clock>`
    /// to look up the clock's index in the runtime `clocks_` vector.
    clock_names: Vec<String>,
    /// Name of the C++ `std::vector` accumulator collecting `yield`-ed
    /// values inside the current `tseq` body. `Some("_result")` while
    /// emitting a tseq; `None` everywhere else (so a stray `yield`
    /// outside a tseq surfaces as a compile error rather than silently
    /// pushing into nothing).
    current_yield_target: Option<String>,
    /// Set of declared tseq names. Used by `let x = TseqName(...)` to
    /// pick `auto` over `int64_t` for the local's type — a tseq call
    /// returns `std::vector<T>`, not an integer.
    tseq_names: std::collections::HashSet<String>,
    /// Component declarations indexed by name. Covers `driver`,
    /// `agent`, `env`, `sequencer`, `scoreboard` — anything with
    /// fields + `hookable` methods. Scoreboards are also registered
    /// here (in addition to the legacy `scoreboards` set) so method
    /// dispatch and field-substitution work uniformly.
    components: std::collections::HashMap<String, ComponentDecl>,
    /// Bus declarations indexed by name. Populated from inline
    /// `bus Name { ... }` items + (future) `use Name` extern import.
    /// Consulted at let-time for `let var : BusName = bind <dut-expr>`
    /// to track the bus binding, and at expression-emit time for
    /// `var.signal` and `var.channel.signal` flat-naming.
    buses: std::collections::HashMap<String, BusDecl>,
    /// Active bus bindings in the current emit context. Each entry
    /// maps a source-level identifier to `(bus_decl, dut_root_expr,
    /// signal_prefix)`:
    ///   * `bus_decl`     — bus declaration whose `signals` /
    ///                      `handshakes` constrain typed access
    ///   * `dut_root_expr` — C++ expression text for the DUT root
    ///                       (typically `"dut"`)
    ///   * `signal_prefix` — name used for flat signal lookup on the
    ///                       DUT (e.g. `"axil"` for `dut->axil_aw_addr`)
    ///
    /// For the test-scope `let axil : BusAxiLite = bind dut`, the
    /// identifier and prefix are both `"axil"`. For the driver-bound
    /// alias `let drv = bind axil` (Phase 2), inside the driver's
    /// on-handler body the bare identifier `"bus"` maps to the same
    /// `(bus_decl, root, prefix)` tuple as `"axil"` — the prefix stays
    /// `"axil"` so signal access still resolves to the DUT-level flat
    /// names. Distinguishing the lookup key from the prefix is what
    /// makes the alias work.
    bus_bindings: std::collections::HashMap<String, (BusDecl, String, String)>,
    /// Per-bind per-signal name override table populated from
    /// `bind <dut> with { ch.sig: "port" }` clauses. Outer key is
    /// the bind variable name (matching the `sig_prefix` field of
    /// `bus_bindings`); inner key is `(channel, signal)` tuple.
    /// Lookups consult this map first; missing entries fall through
    /// to the `<prefix>_<channel>_<signal>` convention. Empty for
    /// binds without a `with { ... }` clause — most fixtures sit
    /// in this bucket.
    bus_remap:
        std::collections::HashMap<String, std::collections::HashMap<(String, String), String>>,
    /// True while emitting statements *directly inside the test's run
    /// coroutine body* (the `scope sim/run` block plus bare test-level
    /// stmts). When set, `wait N cycles`, bare `tick()` (the bus
    /// handshake spin loops), and bus.send/recv lowerings emit
    /// `co_await harc_rt::wait_cycles(_slot, ...)` instead of the
    /// synchronous `for (...) tick();` form.
    ///
    /// Component method bodies, `on`-event-handler closures, tseq
    /// lambdas, and free functions all run synchronously between
    /// coroutine yields — they keep the sync `tick()` lowering. This
    /// works because they only execute while the run coroutine is
    /// "running" (between its co_awaits), so a sync `tick()` from
    /// inside a method does not race the scheduler.
    in_coroutine: bool,
    /// Phase 3a: list of `(scheduler_var, slot_var)` pairs for every
    /// actor coroutine that should run on its own OS thread. Each
    /// entry corresponds to a bound-driver or bound-monitor actor.
    /// At main-loop emission time, if non-empty, we wire up
    /// `_start_barrier` / `_end_barrier` (sized to `1 + len`) plus
    /// one `std::thread` per actor running the actor's
    /// per-os-thread `ThreadScheduler::tick()` between the barriers.
    /// Only populated when `mt = true`; in cooperative mode actor
    /// slots are pushed directly into the global `sched`.
    actor_threads: Vec<(String, String)>,
    /// True when `--mt` opt-in is set: emit per-actor schedulers,
    /// dual barriers, and worker thread spawns. False (default):
    /// actors join the global `sched` and tick cooperatively on
    /// the main thread. The cooperative path is faster on typical
    /// fixtures (per-cycle barrier sync exceeds per-cycle actor
    /// work on Apple Silicon — see the bench measurements in PR
    /// #16's commit message).
    mt: bool,
    /// Driver/agent type name → bus binding to use when emitting
    /// hookable method bodies on that type. Populated by a pre-scan
    /// of the test's `let X : Drv = bind axil` statements before
    /// hookable emission, so `bus.<ch>.send/recv` inside a hookable
    /// body resolves to the parent driver's bound bus. Single-
    /// instance per driver type (the first binding encountered);
    /// multi-instance support requires per-instance hookable
    /// emission and is deferred.
    driver_bus_for_hookables: std::collections::HashMap<String, (BusDecl, String, String)>,
    /// Transactor declarations indexed by name (spec §8.1). Looked
    /// up at `let xact : T mode = bind axil` time; codegen
    /// composes a synthetic ComponentDecl from `T.items` plus
    /// `T.when_active` (only when mode == Active) and reuses the
    /// existing bound-driver-actor + bound-monitor-actor lowering.
    transactors: std::collections::HashMap<String, TransactorDecl>,
    /// While we're emitting statements *inside a component instance's
    /// own body* — an `on <event>` subscriber, a bound-driver actor
    /// coroutine, a bound-monitor handshake actor — this names the
    /// fully-qualified instance path (`"ag"`, `"topenv.ag"`, etc.).
    /// `None` outside such bodies (test run code, free functions,
    /// hookable method bodies which use `self` instead).
    ///
    /// Used by the activity-tracking lowering to bump
    /// `<instance>._last_in_cycle` / `_last_out_cycle` at the sites
    /// where the framework knows an in/out has just happened:
    /// `emit ev(arg)`, `bus.<ch>.send(...)`, `bus.<ch>.recv()`. The
    /// in-handler entry bump uses the static `instance` parameter
    /// directly (already in scope), but `emit`/`send`/`recv` can
    /// appear nested inside arbitrary expressions and need this
    /// context to know whose heartbeat to bump.
    current_component_instance: Option<String>,
}

impl Emitter {
    fn pad(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str(INDENT);
        }
    }

    /// Emit `assert <expr> [else fail("msg")]` as an immediate, point-in-
    /// time check (existing behaviour). Compare with `emit_property_check`
    /// which schedules the check on every primary-clock edge.
    fn emit_inline_assert(&mut self, v: &Verify, depth: usize) {
        self.pad(depth);
        let expr = v.expr.as_ref().expect("assert without expr");
        write!(self.out, "if (!(").ok();
        self.emit_expr(expr);
        writeln!(self.out, ")) {{").ok();
        self.pad(depth + 1);
        let msg = v
            .else_fail
            .as_ref()
            .and_then(|e| match &*e.kind {
                ExprKind::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "assertion failed".to_string());
        let (fmt, caps) = process_interp(&msg);
        write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
        for c in &caps {
            self.emit_interp_arg(c);
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(self.out, "errors++;").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Emit a `wait until …` statement (spec §7.9). Four shape axes:
    ///
    /// - **Single / AllOf / AnyOf** — the predicate is one expression
    ///   or the conjunction / disjunction of multiple sub-expressions.
    /// - **With or without `timeout`** — without, the loop is an
    ///   unbounded `co_await wait_until(_slot, [&]{ … })` (or sync
    ///   `while (!cond) tick();`). With timeout, a bounded polling
    ///   loop measured against `cycle_count` plus per-predicate
    ///   diagnostic logging on expiration.
    ///
    /// Diagnostic on `all of` timeout: lists each sub-predicate still
    /// false, identified by its pretty-printed source text (so the
    /// log mentions `env.agent.idle(100)` rather than a synthetic
    /// index). For `any of`, the breakdown lists every sub-predicate
    /// since none became true.
    fn emit_wait_until(
        &mut self,
        mode: &WaitUntilMode,
        conditions: &[Expr],
        timeout: Option<&WaitTimeout>,
        depth: usize,
    ) {
        if conditions.is_empty() {
            self.errors
                .push("wait until: at least one condition required".into());
            return;
        }
        let expanded_conditions = self.expand_wait_conditions(conditions);
        // Combine conditions into one predicate expression in C++.
        // `Single` uses the bare expression; `AllOf` / `AnyOf` are
        // emitted as parenthesized `&&` / `||` chains.
        let joiner = match mode {
            WaitUntilMode::Single | WaitUntilMode::AllOf => "&&",
            WaitUntilMode::AnyOf => "||",
        };
        let emit_overall_cond = |this: &mut Self| {
            if expanded_conditions.len() == 1 {
                expanded_conditions[0].emit(this);
                return;
            }
            for (i, c) in expanded_conditions.iter().enumerate() {
                if i > 0 {
                    write!(this.out, " {joiner} ").ok();
                }
                write!(this.out, "(").ok();
                c.emit(this);
                write!(this.out, ")").ok();
            }
        };

        match timeout {
            None => {
                // Untimed wait. Coroutine context: yield to the
                // scheduler; sync context: a synchronous polling loop.
                self.pad(depth);
                if self.in_coroutine {
                    write!(
                        self.out,
                        "co_await harc_rt::wait_until(_slot, [&]{{ return "
                    )
                    .ok();
                    emit_overall_cond(self);
                    writeln!(self.out, "; }});").ok();
                } else {
                    write!(self.out, "while (!(").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, ")) tick();").ok();
                }
            }
            Some(to) => {
                // Timed wait — open a brace block so the budget /
                // start variables don't leak.
                //
                // Coroutine context uses the runtime's
                // `wait_until_timeout` awaiter (one scheduler
                // round-trip): the scheduler evaluates the predicate
                // each cycle AND decrements a per-slot countdown,
                // resuming the coroutine when EITHER pred fires OR
                // the budget hits 0. The awaiter returns `true` for
                // pred-fired, `false` for timed-out — drives the
                // diagnostic emission below.
                //
                // Synchronous context (hookable body, etc.) has no
                // coroutine to suspend, so it stays on the explicit
                // polling loop (the only mechanism available there).
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                write!(self.out, "int64_t _wu_budget = (int64_t)(").ok();
                self.emit_expr(&to.cycles);
                writeln!(self.out, ");").ok();
                if self.in_coroutine {
                    // Single co_await — runtime handles the wait + countdown.
                    self.pad(depth + 1);
                    write!(self.out, "bool _wu_satisfied = co_await harc_rt::wait_until_timeout(_slot, [&]{{ return ").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, "; }}, (uint32_t)_wu_budget);").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "if (!_wu_satisfied) {{").ok();
                } else {
                    // Sync context: keep the explicit polling loop —
                    // no scheduler to defer the wait to.
                    self.pad(depth + 1);
                    writeln!(self.out, "int64_t _wu_start = (int64_t)cycle_count;").ok();
                    self.pad(depth + 1);
                    write!(self.out, "while (!(").ok();
                    emit_overall_cond(self);
                    writeln!(
                        self.out,
                        ") && ((int64_t)cycle_count - _wu_start) < _wu_budget) {{"
                    )
                    .ok();
                    self.pad(depth + 2);
                    writeln!(self.out, "tick();").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "}}").ok();
                    // Final check: re-evaluate the predicate now that
                    // the loop has ended (either pred fired or budget
                    // expired).
                    self.pad(depth + 1);
                    write!(self.out, "if (!(").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, ")) {{").ok();
                }
                // Header line — user-supplied message or default.
                let header = to.message.as_ref().and_then(|e| match &*e.kind {
                    ExprKind::String(s) => Some(s.clone()),
                    _ => None,
                });
                self.pad(depth + 2);
                if let Some(raw) = header {
                    let (fmt, caps) = process_interp(&raw);
                    write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
                    for c in &caps {
                        self.emit_interp_arg(c);
                    }
                    writeln!(self.out, ");").ok();
                } else {
                    let label = match mode {
                        WaitUntilMode::Single => "wait until",
                        WaitUntilMode::AllOf => "wait until all of",
                        WaitUntilMode::AnyOf => "wait until any of",
                    };
                    writeln!(self.out,
                        "sim_log_line(\"FAIL\", \"{label} timed out after %lld cycles\", (long long)_wu_budget);"
                    ).ok();
                }
                // Per-sub-predicate breakdown.
                match mode {
                    WaitUntilMode::Single | WaitUntilMode::AllOf => {
                        for c in &expanded_conditions {
                            let escaped = escape_c(&c.label());
                            self.pad(depth + 2);
                            write!(self.out, "if (!(").ok();
                            c.emit(self);
                            writeln!(
                                self.out,
                                ")) sim_log_line(\"FAIL\", \"  not yet true: {escaped}\");"
                            )
                            .ok();
                        }
                    }
                    WaitUntilMode::AnyOf => {
                        // None became true — list everything that was
                        // being waited on so the user can spot the
                        // expected-firing condition that never fired.
                        let mut joined = String::new();
                        for (i, c) in expanded_conditions.iter().enumerate() {
                            if i > 0 {
                                joined.push_str(", ");
                            }
                            joined.push_str(&c.label());
                        }
                        let escaped = escape_c(&joined);
                        self.pad(depth + 2);
                        writeln!(
                            self.out,
                            "sim_log_line(\"FAIL\", \"  none of: {escaped}\");"
                        )
                        .ok();
                    }
                }
                self.pad(depth + 2);
                writeln!(self.out, "errors++;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
    }

    /// `assume <expr>` (immediate form) — same as inline assert but logged
    /// as ASSUME so the user can grep separately. No `errors++` because in
    /// sim, assumes are warnings (spec §5.2).
    fn emit_inline_assume(&mut self, v: &Verify, depth: usize) {
        self.pad(depth);
        let expr = v.expr.as_ref().expect("assume without expr");
        write!(self.out, "if (!(").ok();
        self.emit_expr(expr);
        writeln!(
            self.out,
            ")) sim_log_line(\"ASSUME\", \"assumption failed\");"
        )
        .ok();
    }

    /// Register a concurrent (every-primary-clock-edge) property check.
    /// Resolves a bare `Ident` to a declared property body; otherwise uses
    /// the inline expression directly. Translates the top-level temporal
    /// shape (`a |-> b`, `a |=> b`, or a plain bool) to a stateful closure
    /// pushed into `_checkers`.
    fn emit_property_check(&mut self, severity: &str, v: &Verify, depth: usize) {
        let raw = v.expr.as_ref().expect("assert property without body");
        let body: Expr = match &*raw.kind {
            ExprKind::Ident(id) => match self.properties.get(&id.name).cloned() {
                Some(b) => b,
                None => {
                    self.errors.push(format!(
                        "assert property `{}`: no property declaration with that name",
                        id.name
                    ));
                    return;
                }
            },
            _ => raw.clone(),
        };

        // Generate a unique tag from source span for the static state.
        let tag = format!("_p_{}_{}", raw.span.start, raw.span.end);
        let label = property_label(v, raw);

        // Pre-walk the body for `$past`/`$rose`/`$fell`/`$stable` occurrences.
        // Each gets a static delay slot + a per-call current-value local;
        // emit_expr substitutes references into the closure's predicate.
        let temporals = collect_temporal_occurrences(&body);

        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        // Static slots for delay state — survive across closure calls.
        for (i, _t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            writeln!(self.out, "static int64_t {tag}_ps{i} = 0;").ok();
        }
        // Current-value locals + populate prop_subs.
        for (i, t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            write!(self.out, "int64_t {tag}_cur{i} = (int64_t)(").ok();
            self.emit_expr(&t.inner);
            writeln!(self.out, ");").ok();
            let sub = match t.kind {
                SystemFn::Past => format!("{tag}_ps{i}"),
                SystemFn::Rose => format!("(!{tag}_ps{i} && {tag}_cur{i})"),
                SystemFn::Fell => format!("({tag}_ps{i} && !{tag}_cur{i})"),
                SystemFn::Stable => format!("({tag}_ps{i} == {tag}_cur{i})"),
                SystemFn::Clog2 => continue, // not temporal — skip
            };
            self.prop_subs
                .insert((t.call_span.start, t.call_span.end), sub);
        }
        match &*body.kind {
            // a |=> b — one-cycle-delayed implication. State: prev_a.
            ExprKind::Binary {
                op: BinaryOp::PipeImpliesNext,
                lhs,
                rhs,
            } => {
                self.pad(depth + 1);
                writeln!(self.out, "static bool {tag}_prev = false;").ok();
                self.pad(depth + 1);
                write!(self.out, "bool _curr_a = (bool)(").ok();
                self.emit_expr(lhs);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                write!(self.out, "bool _curr_b = (bool)(").ok();
                self.emit_expr(rhs);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                writeln!(self.out, "if ({tag}_prev && !_curr_b) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed (|=>)\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    writeln!(self.out, "errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{tag}_prev = _curr_a;").ok();
            }
            // a |-> b — same-cycle implication.
            ExprKind::Binary {
                op: BinaryOp::PipeImplies,
                lhs,
                rhs,
            } => {
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                self.emit_expr(lhs);
                write!(self.out, ") && !(bool)(").ok();
                self.emit_expr(rhs);
                writeln!(self.out, ")) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed (|->)\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    writeln!(self.out, "errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            // Plain bool — same-cycle invariant.
            _ => {
                self.pad(depth + 1);
                write!(self.out, "if (!(").ok();
                self.emit_expr(&body);
                writeln!(self.out, ")) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    writeln!(self.out, "errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
        }
        // Update delay slots from current values for next-cycle reads.
        for (i, _t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            writeln!(self.out, "{tag}_ps{i} = {tag}_cur{i};").ok();
        }
        // Clear substitutions so they don't leak into other emissions.
        self.prop_subs.clear();

        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    /// Emit a cycle-trigger `on <bool-expr>` handler — registers a closure
    /// that fires per the handler's `edge` mode (rising / falling / level).
    /// Used by both the test-scope `on` form and monitor body handlers; the
    /// `prefix` distinguishes the static-state tags so concurrent handlers
    /// at the same span don't collide.
    ///
    /// Also handles the periodic form `on <N> cycles ... end on` (spec
    /// §7.10) — fires the body once every `<N>` primary-clock cycles.
    /// `N` is re-read each cycle so per-test overrides via component-
    /// field initialization (or a free `int wdog_period = 5000`)
    /// flow through naturally without rebuilding.
    fn emit_cycle_trigger(&mut self, h: &OnHandler, depth: usize, prefix: &str) {
        let tag = format!("{prefix}{}_{}", h.event.span.start, h.event.span.end);
        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        if h.periodic {
            // Periodic: fire body when cycle_count - last_fired >= N.
            // Initial state: last_fired = 0 means first firing is at
            // cycle N (NOT cycle 0 — the user didn't ask for an
            // immediate-fire-on-start; they asked for "every N cycles").
            self.pad(depth + 1);
            writeln!(self.out, "static int64_t {tag}_last = 0;").ok();
            self.pad(depth + 1);
            write!(self.out, "int64_t {tag}_period = (int64_t)(").ok();
            self.emit_expr(&h.event);
            writeln!(self.out, ");").ok();
            // Guard against period <= 0 — a misconfigured period would
            // otherwise spin-fire every cycle (negative) or every cycle
            // forever (zero). Treat as no-op.
            self.pad(depth + 1);
            writeln!(
                self.out,
                "if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "{tag}_last = (int64_t)cycle_count;").ok();
            self.emit_block(&h.body, depth + 2);
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            writeln!(self.out, "}});").ok();
            return;
        }
        match h.edge {
            EdgeMode::Level => {
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                self.emit_expr(&h.event);
                writeln!(self.out, ")) {{").ok();
                self.emit_block(&h.body, depth + 2);
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            EdgeMode::Rising | EdgeMode::Falling => {
                self.pad(depth + 1);
                writeln!(self.out, "static bool {tag}_prev = false;").ok();
                self.pad(depth + 1);
                write!(self.out, "bool {tag}_curr = (bool)(").ok();
                self.emit_expr(&h.event);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                let cond = match h.edge {
                    EdgeMode::Rising => format!("!{tag}_prev && {tag}_curr"),
                    EdgeMode::Falling => format!("{tag}_prev && !{tag}_curr"),
                    EdgeMode::Level => unreachable!(),
                };
                writeln!(self.out, "if ({cond}) {{").ok();
                self.emit_block(&h.body, depth + 2);
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{tag}_prev = {tag}_curr;").ok();
            }
        }
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    /// Generic on-handler registration for any component (agent /
    /// transactor / sequencer / scoreboard). Dispatches each handler
    /// by the shape of its trigger expression:
    ///
    /// - `on event_field(arg) ... end on` (a `Call` expression) →
    ///   register a `[&]`-capturing closure into the corresponding
    ///   event vector. Used for drivers' `on req(t)`, sequencers'
    ///   subscriptions, etc.
    /// - `on <bool-expr> ... end on` (any other shape) → register a
    ///   per-cycle checker closure with the requested edge mode
    ///   (rising/falling/level). Used for monitors' `on dut.x && y`.
    ///
    /// `tag_prefix` distinguishes the static cycle-trigger state
    /// across invocations so concurrent components at the same source
    /// span don't collide. Inside both paths, body bare-references to
    /// component fields rewrite to `<instance>.<field>` via
    /// `field_subs`.
    /// Emit `on`-handler subscriber closures for a component instance.
    ///
    /// `bound_bus = Some((BusDecl, root))` is set when the component is
    /// declared `bound to BusType` and the user instantiated it via
    /// `let drv : Drv = bind <bus_binding>`. While each handler body is
    /// emitted, `bus_bindings["bus"]` is temporarily set to this binding
    /// so `bus.<ch>.send(t.addr, …)` and `bus.<ch>.<sig>` inside the
    /// body resolve to flat DUT signals through the existing bus-typing
    /// lowerers. The temporary entry is popped after each body so it
    /// doesn't leak across components.
    fn emit_component_handler_registrations(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
    ) {
        self.emit_component_handler_registrations_bound(comp, instance, depth, tag_prefix, None);
    }

    /// Recursively walk a component's `Field`s and register handlers
    /// for each sub-component or sub-transactor. Mode flows down the
    /// inheritance chain via `inherited_mode`:
    ///
    /// ```text
    ///   let topenv : OuterEnv passive
    ///                ^^^^^^^^^^^^^^^^^
    ///                root inherited_mode = Some(Passive)
    ///
    ///   env OuterEnv { ag : MyAgent }      → ag inherits Passive
    ///   agent MyAgent { drv : T }          → drv inherits Passive
    /// ```
    ///
    /// At each sub-field, the **field-level explicit mode** (e.g. `drv
    /// : T active`) wins; only when the field has no mode does the
    /// inherited mode apply. If a transactor sub-field ends up with no
    /// mode (neither field-level nor inherited), it's a clear error.
    ///
    /// Sub-component connect edges are emitted here too — paths like
    /// `sequencer.dispatched -> drv.req` inside an agent get prefixed
    /// with the agent's instance path so they wire correctly when the
    /// agent is composed into a parent env.
    fn emit_subcomponent_handler_registrations(
        &mut self,
        comp: &ComponentDecl,
        instance_path: &str,
        inherited_mode: Option<TransactorMode>,
        depth: usize,
    ) {
        for ci in &comp.items {
            let f = match ci {
                ComponentItem::Field(f) => f,
                _ => continue,
            };
            let field_ty = match type_simple_name(Some(&f.ty)) {
                Some(t) => t,
                None => continue,
            };
            // Resolve effective mode: field-explicit wins over inherited.
            let field_mode = match &f.ty {
                TypeExpr::Named { mode: Some(m), .. } => Some(*m),
                _ => None,
            };
            let effective_mode = field_mode.or(inherited_mode);

            // Sub-component (driver/agent/env/sequencer/scoreboard).
            if let Some(sub_comp) = self.components.get(field_ty).cloned() {
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                let sub_tag = format!("_{}_{}_", sub_comp.kind.keyword(), f.name.name);
                self.emit_component_handler_registrations(&sub_comp, &sub_inst, depth, &sub_tag);
                // Sub-component watchdog (spec §8.6) — install the
                // periodic checker prefixed with the sub-instance path
                // so each composed agent inside an env gets its own
                // independent watchdog firing.
                for sub_ci in &sub_comp.items {
                    if let ComponentItem::Watchdog(w) = sub_ci {
                        self.emit_watchdog_checker(&sub_comp, w, &sub_inst, depth);
                    }
                }
                // Recurse into the sub-component's own sub-fields.
                self.emit_subcomponent_handler_registrations(
                    &sub_comp,
                    &sub_inst,
                    effective_mode,
                    depth,
                );
                // Emit the sub-component's connect edges, prefixed
                // with its instance path. Without this, an agent's
                // `connect sequencer.dispatched -> drv.req` would
                // never wire when the agent is composed into a parent
                // env.
                for sub_ci in &sub_comp.items {
                    if let ComponentItem::Connect(cb) = sub_ci {
                        for edge in &cb.edges {
                            let from = expr_path_str(&edge.from);
                            let to = expr_path_str(&edge.to);
                            if let (Some(from), Some(to)) = (from, to) {
                                self.pad(depth);
                                writeln!(
                                    self.out,
                                    "{}.{}.push_back([&](auto _t) {{ for (auto& _s : {}.{}) _s(_t); }});",
                                    sub_inst, from, sub_inst, to,
                                ).ok();
                            } else {
                                self.errors.push(format!(
                                    "connect: edge endpoints must be plain field paths in v0 cpp_tb"
                                ));
                            }
                        }
                    }
                }
                continue;
            }

            // Covergroup sub-field. Register the per-cycle sample
            // closure prefixed with the sub-instance path so the
            // testbench-bound form (`testbench Tb { cov : Cg }`)
            // gets its sample firing for free. Without this, a
            // covergroup declared as a testbench field never
            // samples (the test-scope `let cov : Cg` registration
            // is the only path otherwise — and the desugarer doesn't
            // mint a parallel let at test scope).
            if let Some(g) = self.covergroups.get(field_ty).cloned() {
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                self.emit_covergroup_sample_registration(&g, &sub_inst, depth);
                continue;
            }

            // Transactor sub-field. Resolve mode and emit handlers
            // through the synth ComponentDecl path.
            if let Some(t) = self.transactors.get(field_ty).cloned() {
                if t.bound_to.is_some() {
                    self.errors.push(format!(
                        "transactor field `{}.{} : {}` has a `bound to` clause; bound sub-components inside an env/agent are out of v0 scope",
                        instance_path, f.name.name, field_ty,
                    ));
                    continue;
                }
                let mode = match effective_mode {
                    Some(m) => m,
                    None => {
                        self.errors.push(format!(
                            "transactor field `{}.{} : {}` has no mode and no parent specifies one; annotate the field or one of its parent let/field sites with `active`/`passive`",
                            instance_path, f.name.name, field_ty,
                        ));
                        continue;
                    }
                };
                let include_active = matches!(mode, TransactorMode::Active);
                let synth = synth_component_from_transactor(&t, include_active);
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                let sub_tag = format!("_xactor_{}_", sub_inst.replace('.', "_"));
                self.emit_component_handler_registrations(&synth, &sub_inst, depth, &sub_tag);
            }
        }
    }

    /// Phase 2c: emit each `on bus.<ch>.handshake(arg)` handler in a
    /// `bound to BusType` monitor as an independent coroutine actor:
    ///
    /// ```text
    ///     while (true) {
    ///         co_await wait_until(_slot, [&]{ return valid && ready; });
    ///         auto <arg> = <first payload signal>;
    ///         <body — bus.<ch>.<sig> resolves through the bus binding>
    ///         co_await wait_cycles(_slot, 1);   // skip past this handshake
    ///     }
    /// ```
    ///
    /// Non-handshake on-handlers in the monitor (event subscribers,
    /// cycle triggers on bool expressions) fall through to the
    /// existing sync `_checkers`-based path. A monitor can mix both —
    /// each handler is independently classified.
    ///
    /// The trailing `wait_cycles(1)` matters: a handshake completes
    /// in exactly one cycle (valid && ready both high at the posedge),
    /// but the producer/consumer may keep the signals high across
    /// multiple cycles for back-to-back handshakes. Skipping forward
    /// one cycle ensures we don't re-fire on the same handshake.
    /// Back-to-back handshakes with no idle gap will fire on every
    /// cycle, which is exactly what arch §19 specifies.
    fn emit_bound_monitor_actors(
        &mut self,
        mon: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) {
        // Build field substitution map (same shape as
        // emit_component_handler_registrations) so bare names inside
        // the handler body resolve to `instance.field`.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        for it in &mon.items {
            if let ComponentItem::Field(f) = it {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        // Walk handlers, classify each as handshake-actor vs sync.
        let mut sync_handlers: Vec<&OnHandler> = Vec::new();
        for it in &mon.items {
            if let ComponentItem::OnHandler(h) = it {
                if let Some((ch_name, arg_name)) = extract_bus_handshake_event(&h.event, "bus") {
                    // Resolve the channel in the bound bus.
                    let channel = match binding
                        .0
                        .handshakes
                        .iter()
                        .find(|hs| hs.name.name == ch_name)
                    {
                        Some(c) => c.clone(),
                        None => {
                            self.errors.push(format!(
                                "monitor {instance}: bus has no channel `{ch_name}`"
                            ));
                            continue;
                        }
                    };
                    if channel.payload.is_empty() {
                        self.errors.push(format!(
                            "monitor {instance}: channel `{ch_name}` has no payload signals to capture"
                        ));
                        continue;
                    }
                    let (bus_decl, root, sig_prefix) = binding;
                    let slot_var = format!("_{instance}_{ch_name}_slot");
                    let sched_var = format!("_{instance}_{ch_name}_sched");

                    if self.mt {
                        self.pad(depth);
                        writeln!(self.out, "harc_rt::ThreadScheduler {sched_var};").ok();
                    }
                    self.pad(depth);
                    writeln!(self.out, "harc_rt::ThreadSlot {slot_var};").ok();
                    self.pad(depth);
                    if self.mt {
                        writeln!(self.out, "{sched_var}.slots.push_back(&{slot_var});").ok();
                        self.actor_threads
                            .push((sched_var.clone(), slot_var.clone()));
                    } else {
                        writeln!(self.out, "sched.slots.push_back(&{slot_var});").ok();
                    }
                    self.pad(depth);
                    writeln!(
                        self.out,
                        "{slot_var}.thread = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
                    ).ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "while (true) {{").ok();
                    let valid_port = self.bus_signal_name(&sig_prefix, &ch_name, "valid");
                    let ready_port = self.bus_signal_name(&sig_prefix, &ch_name, "ready");
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{valid_port} && {root}->{ready_port}; }});",
                    ).ok();
                    // Bind `arg` to the per-channel payload struct so the
                    // body can use `arg.data`, `arg.resp`, etc. The
                    // struct's implicit-conversion-to-first-field
                    // operator means scalar use (`sb.queue.push(arg)`)
                    // also keeps working — push receives the first
                    // payload value, matching pre-multi-payload behaviour.
                    self.pad(depth + 2);
                    let struct_name = format!("{}_{}_payload", bus_decl.name.name, ch_name);
                    write!(self.out, "{struct_name} {arg_name} = {{").ok();
                    let mut first = true;
                    for sig in &channel.payload {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        let sig_port = self.bus_signal_name(&sig_prefix, &ch_name, &sig.name.name);
                        write!(self.out, "{root}->{sig_port}").ok();
                    }
                    writeln!(self.out, "}};").ok();
                    // Activity tracking (spec §7.x): an observed bus
                    // handshake counts as an "in" for this monitor
                    // instance.
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "{instance}._last_in_cycle = (uint64_t)cycle_count;"
                    )
                    .ok();

                    // Body: install field subs + bus binding, mark
                    // coroutine context, emit, restore state.
                    let prev_subs = std::mem::replace(&mut self.field_subs, subs.clone());
                    let mut added_events = Vec::new();
                    for (name, ty) in &local_event_types {
                        if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                            added_events.push(name.clone());
                        }
                    }
                    let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
                    let prior_corout = self.in_coroutine;
                    self.in_coroutine = true;
                    let prior_inst = std::mem::replace(
                        &mut self.current_component_instance,
                        Some(instance.to_string()),
                    );

                    self.emit_block(&h.body, depth + 2);

                    self.current_component_instance = prior_inst;
                    self.in_coroutine = prior_corout;
                    match prior_bus {
                        Some(prev) => {
                            self.bus_bindings.insert("bus".into(), prev);
                        }
                        None => {
                            self.bus_bindings.remove("bus");
                        }
                    }
                    for n in added_events {
                        self.event_types.remove(&n);
                    }
                    self.field_subs = prev_subs;

                    // Skip past this handshake before re-arming.
                    self.pad(depth + 2);
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "}}").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "co_return;").ok();
                    self.pad(depth);
                    writeln!(self.out, "}}(&{slot_var});").ok();
                } else {
                    sync_handlers.push(h);
                }
            }
        }

        // Other on-handlers fall through to the existing sync
        // registration. We do this by emitting a temporary clone of
        // the monitor whose `items` contain only the leftover
        // handlers + the original fields, so existing field-sub /
        // event-subscription paths run unchanged.
        if !sync_handlers.is_empty() {
            let mut sync_view = mon.clone();
            sync_view.items.retain(|it| match it {
                ComponentItem::OnHandler(h) => {
                    extract_bus_handshake_event(&h.event, "bus").is_none()
                }
                _ => true,
            });
            self.emit_component_handler_registrations_bound(
                &sync_view,
                instance,
                depth,
                "_m_",
                Some(binding.clone()),
            );
        }
    }

    /// Phase 2b: if `comp` is a `bound to BusType` driver/agent with
    /// exactly one `in event<T>` field plus a matching
    /// `on <event_name>(t)` handler, emit it as an independent
    /// coroutine actor:
    ///
    /// 1. A per-instance transaction queue (`std::deque<T>`).
    /// 2. A subscriber on the input event that *pushes* incoming
    ///    transactions onto the queue (no longer runs the body
    ///    inline — the actor body lives in the coroutine).
    /// 3. A `ThreadSlot` registered with `sched`.
    /// 4. The actor coroutine: loop forever, `co_await wait_until`
    ///    until the queue is non-empty, pop a transaction, run the
    ///    `on T t` handler body in coroutine context (so internal
    ///    `wait N cycles` and `bus.<ch>.send/recv` lower to `co_await`),
    ///    repeat. The actor never returns; the main loop terminates
    ///    when the *run* coroutine finishes, abandoning auxiliary
    ///    actors mid-WaitUntil.
    ///
    /// Returns `true` on success — caller should NOT also emit the
    /// sync subscriber path. Returns `false` to fall through.
    fn try_emit_bound_driver_actor(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) -> bool {
        // Only `agent` and `transactor` (synth ComponentDecl from a
        // bound active transactor) are eligible — envs / sequencers /
        // scoreboards aren't actor-shaped at all.
        if !matches!(comp.kind, ComponentKind::Agent | ComponentKind::Transactor) {
            return false;
        }

        // Find a single `in event<T>` field. Multi-input drivers stay
        // sync for now (each input would need its own queue + the
        // coroutine would have to multi-way wait — separate PR).
        let mut input_event: Option<(String, String)> = None;
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                if matches!(f.direction, Some(Direction::In)) {
                    if let TypeExpr::Builtin {
                        name: BuiltinTy::Event,
                        args,
                        ..
                    } = &f.ty
                    {
                        let payload = self.payload_type_for_arg(args.first());
                        if input_event.is_some() {
                            return false; // multi-input — bail out
                        }
                        input_event = Some((f.name.name.clone(), payload));
                    }
                }
            }
        }
        let Some((event_name, payload_ty)) = input_event else {
            return false; // no input event — nothing to actor-ify
        };

        // Find the matching on-handler. If absent or there's more
        // than one matching, fall back. (Other handlers on different
        // events are fine — they just don't get coroutine-ified by
        // this PR; the sync registration still runs for them.)
        let mut matched_handler: Option<&OnHandler> = None;
        let mut other_handlers: Vec<&OnHandler> = Vec::new();
        for it in &comp.items {
            if let ComponentItem::OnHandler(h) = it {
                if let Some((ev, _arg)) = extract_event_subscription(&h.event) {
                    if ev == event_name {
                        if matched_handler.is_some() {
                            return false; // ambiguous multi-handler — bail
                        }
                        matched_handler = Some(h);
                    } else {
                        other_handlers.push(h);
                    }
                }
            }
        }
        let Some(handler) = matched_handler else {
            return false;
        };
        let arg_name = match extract_event_subscription(&handler.event) {
            Some((_, a)) => a,
            None => return false,
        };

        // ── Emit the actor topology ─────────────────────────────────
        // Cooperative mode (default): all actor slots go into the
        // single `sched` and tick together with `_run_slot` on the
        // main thread. This is faster than MT on typical fixtures
        // because there's no barrier overhead.
        //
        // MT mode (`--mt`, Phase 3a): each actor gets its own
        // `ThreadScheduler` and lives on a dedicated OS thread,
        // synchronized via dual barriers per posedge. `tick()`
        // itself is not MT-safe so per-actor schedulers avoid locks.
        let queue_var = format!("_{instance}_q");
        let slot_var = format!("_{instance}_slot");
        let sched_var = format!("_{instance}_sched");

        self.pad(depth);
        writeln!(self.out, "std::deque<{payload_ty}> {queue_var};").ok();
        if self.mt {
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadScheduler {sched_var};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "harc_rt::ThreadSlot {slot_var};").ok();
        self.pad(depth);
        if self.mt {
            writeln!(self.out, "{sched_var}.slots.push_back(&{slot_var});").ok();
            self.actor_threads
                .push((sched_var.clone(), slot_var.clone()));
        } else {
            writeln!(self.out, "sched.slots.push_back(&{slot_var});").ok();
        }

        // Pusher subscriber on the input event. Replaces the sync
        // body-callback; emit/connect bridges that fan out to this
        // event now feed the actor's queue.
        self.pad(depth);
        writeln!(
            self.out,
            "{instance}.{event_name}.push_back([&]({payload_ty} {arg_name}) {{ {queue_var}.push_back({arg_name}); }});",
        ).ok();

        // Spawn the coroutine. Body emits the on-handler body in
        // coroutine context with the bus binding active for typed
        // signal access.
        self.pad(depth);
        writeln!(
            self.out,
            "{slot_var}.thread = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ return !{queue_var}.empty(); }});",
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "auto {arg_name} = {queue_var}.front();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "{queue_var}.pop_front();").ok();
        // Activity tracking (spec §7.x): popping a transaction off
        // the per-actor queue counts as an "in" for this instance.
        self.pad(depth + 2);
        writeln!(
            self.out,
            "{instance}._last_in_cycle = (uint64_t)cycle_count;"
        )
        .ok();

        // Build field-name substitution map: bare names inside the
        // handler body resolve to `instance.field`. Event-typed
        // fields also register their payload types so any nested
        // `emit <ev>(arg)` finds the right param shape.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);
        let mut added_events = Vec::new();
        for (name, ty) in &local_event_types {
            if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                added_events.push(name.clone());
            }
        }

        // Push the bus binding under the alias `"bus"` so
        // `bus.<ch>.send/recv` and `bus.<ch>.<sig>` inside the actor
        // body resolve to flat DUT signals via the bus-typing
        // lowerers — same mechanism Phase 2a's sync path uses.
        let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
        // Mark coroutine context so wait/tick lower to co_await.
        let prior_corout = self.in_coroutine;
        self.in_coroutine = true;
        // Set current-component scope so `emit`/`bus.<ch>.send/recv`
        // nested in the actor body bump this instance's heartbeat
        // fields.
        let prior_inst = std::mem::replace(
            &mut self.current_component_instance,
            Some(instance.to_string()),
        );

        self.emit_block(&handler.body, depth + 2);

        self.current_component_instance = prior_inst;
        self.in_coroutine = prior_corout;
        match prior_bus {
            Some(prev) => {
                self.bus_bindings.insert("bus".into(), prev);
            }
            None => {
                self.bus_bindings.remove("bus");
            }
        }
        self.field_subs = prev_subs;
        for n in added_events {
            self.event_types.remove(&n);
        }

        self.pad(depth + 1);
        writeln!(self.out, "}}").ok(); // close while(true)
        self.pad(depth + 1);
        writeln!(self.out, "co_return;").ok(); // unreachable but required
        self.pad(depth);
        writeln!(self.out, "}}(&{slot_var});").ok();

        // Other on-handlers (on different events) still register
        // sync — they don't go through the actor's queue and may
        // legitimately want sync dispatch.
        if !other_handlers.is_empty() {
            let tag = format!("_{}_", comp.kind.keyword());
            // Only register the non-matching handlers via the sync
            // path. We do that by emitting them one-at-a-time using
            // the existing helper after temporarily replacing comp's
            // items list. Simpler: filter manually here.
            let mut filtered = comp.clone();
            filtered.items.retain(|it| match it {
                ComponentItem::OnHandler(h) => extract_event_subscription(&h.event)
                    .map(|(ev, _)| ev != event_name)
                    .unwrap_or(true),
                _ => true,
            });
            self.emit_component_handler_registrations_bound(
                &filtered,
                instance,
                depth,
                &tag,
                Some(binding.clone()),
            );
        }

        true
    }

    fn emit_component_handler_registrations_bound(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
        bound_bus: Option<(BusDecl, String, String)>,
    ) {
        // Build field-name substitution map for the body. Component
        // fields visible by bare name inside the handler body get
        // prefixed with the instance path.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types = Vec::new();
        let mut event_field_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    event_field_names.insert(f.name.name.clone());
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        let prev_subs = std::mem::replace(&mut self.field_subs, subs);
        let mut added_events = Vec::new();
        for (name, ty) in &local_event_types {
            if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                added_events.push(name.clone());
            }
        }

        for it in &comp.items {
            if let ComponentItem::OnHandler(h) = it {
                if let Some((event_name, arg_name)) = extract_event_subscription(&h.event) {
                    if event_field_names.contains(&event_name) {
                        // Subscriber to a component event field.
                        let arg_ty = self
                            .event_types
                            .get(&event_name)
                            .cloned()
                            .unwrap_or_else(|| "int64_t".into());
                        self.pad(depth);
                        writeln!(
                            self.out,
                            "{instance}.{event_name}.push_back([&]({arg_ty} {arg_name}) {{",
                        )
                        .ok();
                        // Activity tracking (spec §7.x): an incoming event
                        // counts as an "in" for the component instance —
                        // bump _last_in_cycle to the current cycle. The
                        // current-component-instance scope is also set
                        // so any `emit`/`bus.send`/`bus.recv` *inside*
                        // this body bumps the same instance's
                        // _last_out_cycle (those sites can't see the
                        // static `instance` parameter directly).
                        self.pad(depth + 1);
                        writeln!(
                            self.out,
                            "{instance}._last_in_cycle = (uint64_t)cycle_count;"
                        )
                        .ok();
                        let prior_inst = std::mem::replace(
                            &mut self.current_component_instance,
                            Some(instance.to_string()),
                        );
                        // For `bound to BusType` components, expose the
                        // driver's bus binding inside the handler body
                        // as the bare identifier `bus`. Same root +
                        // BusDecl as the test-scope binding it was
                        // bound from, so `bus.<ch>.send/recv` and
                        // `bus.<ch>.<sig>` lower through the existing
                        // bus_handshake / bus_field_access paths.
                        let prior_bus = bound_bus
                            .as_ref()
                            .and_then(|b| self.bus_bindings.insert("bus".into(), b.clone()));
                        self.emit_block(&h.body, depth + 1);
                        if bound_bus.is_some() {
                            match prior_bus {
                                Some(prev) => {
                                    self.bus_bindings.insert("bus".into(), prev);
                                }
                                None => {
                                    self.bus_bindings.remove("bus");
                                }
                            }
                        }
                        self.current_component_instance = prior_inst;
                        self.pad(depth);
                        writeln!(self.out, "}});").ok();
                        continue;
                    }
                }
                // Fallback: bool-expression cycle trigger (monitors).
                self.emit_cycle_trigger(h, depth, tag_prefix);
            }
        }

        self.field_subs = prev_subs;
        for n in added_events {
            self.event_types.remove(&n);
        }
    }

    /// Emit the C++ boolean expression for "is `_v` in this bin?".
    ///   `{x}`        — `(_v == x)`
    ///   `{a, b, c}`  — `(_v == a || _v == b || _v == c)`
    ///   `[a..b]`     — `(_v >= a && _v <= b)`
    ///   `(inner)`    — recurse
    ///   anything else — falls back to `(_v == <expr>)` (e.g. a single Int).
    fn emit_bin_membership(&mut self, spec: &Expr) {
        match &*spec.kind {
            ExprKind::SetLit(items) => {
                write!(self.out, "(").ok();
                if items.is_empty() {
                    write!(self.out, "false").ok();
                } else {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            write!(self.out, " || ").ok();
                        }
                        // Recurse — set-of-ranges (`{[1..3], 7}`) works.
                        self.emit_bin_membership(it);
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::RangeLit { lo, hi } => {
                write!(self.out, "(").ok();
                if let Some(l) = lo {
                    write!(self.out, "_v >= ").ok();
                    self.emit_expr(l);
                    if hi.is_some() {
                        write!(self.out, " && ").ok();
                    }
                }
                if let Some(h) = hi {
                    write!(self.out, "_v <= ").ok();
                    self.emit_expr(h);
                }
                if lo.is_none() && hi.is_none() {
                    write!(self.out, "true").ok();
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => self.emit_bin_membership(inner),
            // Default: equality test against the expression. Covers the
            // single-value form (`{1}` parses as a SetLit but `1` alone
            // would land here).
            _ => {
                write!(self.out, "(_v == ").ok();
                self.emit_expr(spec);
                write!(self.out, ")").ok();
            }
        }
    }

    /// Emit a covergroup as a C++ struct: one nested struct per cover-point
    /// holding `uint64_t` bin counters; a `sample()` method that runs each
    /// cover-point's expression once and increments the matching bin's
    /// counter; a `report()` method that prints a coverage summary.
    /// `cross` is parsed but not lowered in v0 — coverage of combinations
    /// would need a 2D matrix per cross declaration.
    fn emit_covergroup_struct(&mut self, g: &CovergroupDecl) {
        writeln!(self.out, "struct {} {{", g.name.name).ok();
        // Per-cover-point nested struct of bin counters. The struct is
        // pure data; the sample() logic is emitted at `let cov : G` time
        // as a `_checkers` closure (so `dut` is in scope via `[&]`
        // capture). report() lives here because it only reads its own
        // counters.
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                writeln!(self.out, "{INDENT}struct {{").ok();
                for b in &p.bins {
                    writeln!(self.out, "{INDENT}{INDENT}uint64_t {} = 0;", b.name.name).ok();
                }
                writeln!(self.out, "{INDENT}}} {};", p.name.name).ok();
            }
        }
        writeln!(self.out, "").ok();
        // report() — ARCH-format coverage dump (header line + per-bin
        // lines + `*NOT HIT*` marker). Mirrors arch-com's
        // sim_codegen/fsm.rs `_arch_cov_dump` shape, but writes to stdout
        // (not stderr): in a HARC TB the program's stdout already IS
        // the framework log (everything the user wrote via log() goes
        // there too). ARCH writes to stderr because in ARCH-sim, stdout
        // belongs to the user's test program — that distinction doesn't
        // apply to a HARC-emitted TB.
        let total_bins: usize = g
            .items
            .iter()
            .map(|it| match it {
                CoverItem::Point(p) => p.bins.len(),
                _ => 0,
            })
            .sum();
        writeln!(self.out, "{INDENT}void report() const {{").ok();
        self.pad(2);
        writeln!(
            self.out,
            "uint64_t _total = {total_bins}; uint64_t _hit = 0;"
        )
        .ok();
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                for b in &p.bins {
                    self.pad(2);
                    writeln!(self.out, "if ({}.{} > 0) _hit++;", p.name.name, b.name.name).ok();
                }
            }
        }
        self.pad(2);
        writeln!(self.out,
            "std::printf( \"[{}] coverage: %llu/%llu hit (%.1f%%)\\n\", (unsigned long long)_hit, (unsigned long long)_total, _total ? (100.0 * _hit / _total) : 0.0);",
            g.name.name).ok();
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                for b in &p.bins {
                    self.pad(2);
                    writeln!(self.out,
                        "std::printf( \"  {0} (bin) [{1}]: %llu hits%s\\n\", (unsigned long long){0}.{1}, {0}.{1} ? \"\" : \" *NOT HIT*\");",
                        p.name.name, b.name.name).ok();
                }
            }
        }
        writeln!(self.out, "{INDENT}}}").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// At a `let cov : G` site, emit a `_checkers` closure that runs the
    /// covergroup's sample logic each cycle. The closure has `[&]` capture
    /// so `dut` and the cov instance are both visible.
    fn emit_covergroup_sample_registration(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        depth: usize,
    ) {
        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                self.pad(depth + 1);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 2);
                write!(self.out, "uint64_t _v = (uint64_t)(").ok();
                self.emit_expr(&p.target);
                writeln!(self.out, ");").ok();
                for b in &p.bins {
                    self.pad(depth + 2);
                    write!(self.out, "if (").ok();
                    self.emit_bin_membership(&b.spec);
                    writeln!(self.out, ") {instance}.{}.{}++;", p.name.name, b.name.name).ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
        }
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    /// Emit a scoreboard declaration as a C++ struct. Only the field list
    /// is lowered for v0 — `connect` clauses and `on` handlers in the
    /// scoreboard body would need event registration that's not in scope.
    /// Each field default-initialises (queues to empty, ints to 0). The
    /// user drives push/pop/etc. directly from test code.
    /// Resolve a bus-bound field access. Returns `Some(c++_string)`
    /// when `target.name` (or `target.ch.name`) names a signal under
    /// a known bus binding; `None` otherwise (caller falls back to
    /// generic field-access lowering).
    ///
    /// Naming convention mirrors arch-com §19.6: each plain bus
    /// signal flattens to `<binding>_<signal>` on the DUT pointer,
    /// each handshake_channel signal flattens to
    /// `<binding>_<channel>_<signal>` (and the implicit `valid` /
    /// `ready` signals flatten the same way).
    /// Detect `<bus>.<channel>.send(args)` / `<bus>.<channel>.recv()`
    /// calls and emit the auto-generated valid/ready handshake. The
    /// channel must be a `handshake_channel` on the bus the binding
    /// names; the call's arity (for `send`) must match the channel's
    /// payload signal count.
    ///
    /// `let_name = Some("x")` for the let-rhs form (`let x = bus.r.recv()`):
    /// the captured payload is assigned into `auto x = ...;`. With
    /// `None` the call is a discarded statement (drives the dance,
    /// throws away any received data).
    ///
    /// Returns `true` when the call was a recognized bus handshake
    /// and was emitted; `false` to fall through to plain Call lowering.
    fn try_emit_bus_handshake(&mut self, e: &Expr, let_name: Option<&str>, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return false;
        };
        let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        else {
            return false;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return false;
        };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else {
            return false;
        };
        let Some(h) = bus
            .handshakes
            .iter()
            .find(|h| h.name.name == ch.name)
            .cloned()
        else {
            return false;
        };
        let valid_port = self.bus_signal_name(&sig_prefix, &ch.name, "valid");
        let ready_port = self.bus_signal_name(&sig_prefix, &ch.name, "ready");

        match method.name.as_str() {
            "send" => {
                if args.len() != h.payload.len() {
                    self.errors.push(format!(
                        "bus.{}.send: expected {} payload arg(s), got {}",
                        ch.name,
                        h.payload.len(),
                        args.len(),
                    ));
                    return true;
                }
                if let_name.is_some() {
                    self.errors.push(format!(
                        "bus.{}.send returns no value; use it as a statement",
                        ch.name,
                    ));
                    return true;
                }
                self.pad(depth);
                writeln!(self.out, "// bus.{}.send", ch.name).ok();
                // Drive payload signals.
                for (sig, arg) in h.payload.iter().zip(args.iter()) {
                    let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &sig.name.name);
                    self.pad(depth);
                    write!(self.out, "{root}->{sig_port} = ").ok();
                    match arg {
                        CallArg::Expr(e) => self.emit_expr(e),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                    writeln!(self.out, ";").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{valid_port} = 1;").ok();
                self.pad(depth);
                if self.in_coroutine {
                    // Coroutine path: yield until ready=1 (bounded). The
                    // bound matches the sync 16-cycle budget so a stuck
                    // DUT still terminates the test rather than hanging.
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{ready_port} && _b > 0) {{ co_await harc_rt::wait_cycles(_slot, 1); _b--; }} }}").ok();
                    self.pad(depth);
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                } else {
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{ready_port} && _b > 0) {{ tick(); _b--; }} }}").ok();
                    self.pad(depth);
                    writeln!(self.out, "tick();").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{valid_port} = 0;").ok();
                // Activity tracking (spec §7.x): a completed
                // bus.<ch>.send counts as an "out" for the surrounding
                // component instance, if any. Bus calls inside
                // free/test-run code (no enclosing component) skip the
                // bump.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
                }
                true
            }
            "recv" => {
                if !args.is_empty() {
                    self.errors.push(format!(
                        "bus.{}.recv: expected 0 args, got {}",
                        ch.name,
                        args.len(),
                    ));
                    return true;
                }
                if h.payload.is_empty() {
                    self.errors.push(format!(
                        "bus.{}.recv: channel has no payload signals to receive",
                        ch.name,
                    ));
                    return true;
                }
                // Multi-payload receive captures the full payload into
                // a per-channel struct (`<BusName>_<chan>_payload`),
                // emitted at file scope. The struct exposes an implicit
                // conversion to the first payload field's C++ type so
                // pre-existing scalar usage (`assert val == 0xCAFE`,
                // `field = val`) keeps compiling without change. Users
                // who want named field access write `val.data`,
                // `val.resp`, etc.
                self.pad(depth);
                writeln!(self.out, "// bus.{}.recv", ch.name).ok();
                self.pad(depth);
                writeln!(self.out, "{root}->{ready_port} = 1;").ok();
                self.pad(depth);
                if self.in_coroutine {
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{valid_port} && _b > 0) {{ co_await harc_rt::wait_cycles(_slot, 1); _b--; }} }}").ok();
                } else {
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{valid_port} && _b > 0) {{ tick(); _b--; }} }}").ok();
                }
                // Capture BEFORE the trailing tick: the destination signals
                // are valid in the same cycle as `valid` is high.
                if let Some(name) = let_name {
                    self.pad(depth);
                    let struct_name = format!("{}_{}_payload", bus.name.name, ch.name);
                    write!(self.out, "{struct_name} {name} = {{").ok();
                    let mut first = true;
                    for sig in &h.payload {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &sig.name.name);
                        write!(self.out, "{root}->{sig_port}").ok();
                    }
                    writeln!(self.out, "}};").ok();
                }
                self.pad(depth);
                if self.in_coroutine {
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                } else {
                    writeln!(self.out, "tick();").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{ready_port} = 0;").ok();
                // Activity tracking (spec §7.x): a completed
                // bus.<ch>.recv counts as an "in" for the surrounding
                // component instance, if any.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_in_cycle = (uint64_t)cycle_count;").ok();
                }
                true
            }
            _ => false,
        }
    }

    /// Intrinsic width-method lowering: `.trunc<N>()`, `.zext<N>()`,
    /// `.sext<N>()`, `.resize<N>()`. Ported from arch-com's
    /// `cpp_method_call` (src/sim_codegen/mod.rs:2688). The parser
    /// emits these as `Call { callee: Field{recv, name},
    /// args: [width_const] }`. We dispatch on `name`, evaluate the
    /// width as a compile-time constant, and emit the C++ narrow or
    /// extend. Returns `true` when the call matched and was emitted;
    /// `false` otherwise (caller falls through to normal call
    /// emission).
    ///
    /// Behavior summary (matches arch-com's semantics):
    /// - `.trunc<N>()`  — narrow to N bits: `((uintN_t)((expr) & MASK))`
    ///                    where MASK = (1 << N) - 1. Errors if N >=
    ///                    source width when source width is known
    ///                    (would widen, wrong direction).
    /// - `.zext<N>()`   — zero-extend to N bits: `((uintN_t)(expr))`.
    /// - `.sext<N>()`   — sign-extend to N bits: shift-fill trick.
    /// - `.resize<N>()` — direction-agnostic: narrow if N < source
    ///                    width, zero-extend otherwise.
    fn try_emit_width_method(&mut self, callee: &Expr, args: &[CallArg]) -> bool {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return false;
        };
        let kind = match name.name.as_str() {
            "trunc" => "trunc",
            "zext" => "zext",
            "sext" => "sext",
            "resize" => "resize",
            _ => return false,
        };
        let width_expr = match args.first() {
            Some(CallArg::Expr(e)) => e,
            _ => {
                self.errors
                    .push(format!("`.{kind}<N>()` requires a constant width argument",));
                return true;
            }
        };
        let width = match eval_const_width(width_expr) {
            Some(w) => w,
            None => {
                self.errors.push(format!(
                    "`.{kind}<N>()` requires a constant integer width \
                     (saw a non-const expression); span at parse-time \
                     should pin the offending token",
                ));
                return true;
            }
        };
        if width == 0 || width > 64 {
            // arch-com supports wider (VlWide) but harc-com lowers all
            // ≤64-bit ints to int64_t — wide-int support exists for
            // 128-bit literals but the cast helpers used here assume
            // ≤64. Surface the limit explicitly.
            self.errors.push(format!(
                "`.{kind}<{width}>()`: width must be in 1..=64 (wide-int \
                 narrow/extend not yet supported in harc-com)",
            ));
            return true;
        }
        // Best-effort source-width inference for the type-direction check.
        // We can detect width when the receiver is a literal-typed `let`
        // or an explicit `as uint<W>` / `as sint<W>` cast. Otherwise the
        // width is unknown and we skip the type-direction check.
        let source_width = self.infer_expr_width_best_effort(target);
        if let Some(sw) = source_width {
            match kind {
                "trunc" if width >= sw => {
                    self.errors.push(format!(
                        "`.trunc<{width}>()` on a {sw}-bit value: width \
                         must be strictly less than the source width \
                         (otherwise it's a no-op or wrong-direction). \
                         Use `.zext<{width}>()` to widen, or remove the \
                         cast if you meant a no-op.",
                    ));
                    return true;
                }
                "zext" | "sext" if width < sw => {
                    self.errors.push(format!(
                        "`.{kind}<{width}>()` on a {sw}-bit value: width \
                         must be ≥ the source width (otherwise it \
                         narrows, wrong direction). Use `.trunc<{width}>()` \
                         to narrow.",
                    ));
                    return true;
                }
                _ => {}
            }
        }
        // Emit. Pattern follows arch-com's cast_to_bits / shift-fill
        // for sext.
        let c_unsigned = cpp_uint_for_width(Some(width));
        match kind {
            "trunc" => {
                if width == 64 {
                    write!(self.out, "(({c_unsigned})(").ok();
                    self.emit_expr(target);
                    write!(self.out, "))").ok();
                } else {
                    let mask = (1u64 << width) - 1;
                    write!(self.out, "(({c_unsigned})(((").ok();
                    self.emit_expr(target);
                    write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                }
            }
            "zext" => {
                // Mask to the source width before widening so any high bits
                // in the underlying int64_t storage (from a prior no-op cast)
                // are dropped. When source width is unknown we conservatively
                // assume the receiver already fits in N bits — same shape
                // as arch-com.
                write!(self.out, "(({c_unsigned})(").ok();
                self.emit_expr(target);
                write!(self.out, "))").ok();
            }
            "sext" => {
                // Sign-extend from source width to dest width. Strategy:
                // shift source-width MSB into bit 63 of an int64_t, then
                // arithmetic-shift right back (the standard idiom).
                // The result is masked to the dest width via the C++
                // cast `((cpp_uint_for_width(width))<expr>)`. When source
                // width is unknown, fall back to a plain widening cast
                // — the receiver almost certainly already has the right
                // sign-bit pattern in its underlying storage.
                if let Some(sw) = source_width {
                    if sw < width {
                        let shift = 64 - sw;
                        if width == 64 {
                            // Want full 64-bit signed-extended view.
                            write!(self.out, "((uint64_t)(((int64_t)((uint64_t)(").ok();
                            self.emit_expr(target);
                            write!(self.out, ") << {shift})) >> {shift}))").ok();
                        } else {
                            let mask = (1u64 << width) - 1;
                            write!(self.out, "(({c_unsigned})(((int64_t)((uint64_t)(").ok();
                            self.emit_expr(target);
                            write!(self.out, ") << {shift})) >> {shift}) & 0x{mask:X}ULL)").ok();
                        }
                    } else {
                        write!(self.out, "(({c_unsigned})(").ok();
                        self.emit_expr(target);
                        write!(self.out, "))").ok();
                    }
                } else {
                    write!(self.out, "(({c_unsigned})(").ok();
                    self.emit_expr(target);
                    write!(self.out, "))").ok();
                }
            }
            "resize" => {
                // Direction-agnostic: narrow with mask when narrowing,
                // plain cast otherwise.
                if let Some(sw) = source_width {
                    if width < sw {
                        // Narrowing — mask + cast.
                        if width == 64 {
                            write!(self.out, "(({c_unsigned})(").ok();
                            self.emit_expr(target);
                            write!(self.out, "))").ok();
                        } else {
                            let mask = (1u64 << width) - 1;
                            write!(self.out, "(({c_unsigned})(((").ok();
                            self.emit_expr(target);
                            write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                        }
                    } else {
                        // Widening or same width — plain cast.
                        write!(self.out, "(({c_unsigned})(").ok();
                        self.emit_expr(target);
                        write!(self.out, "))").ok();
                    }
                } else {
                    // Unknown source width — default to mask-narrow,
                    // since `.resize<N>()` with `N <= 64` always wants
                    // a value bounded to N bits regardless of source.
                    if width == 64 {
                        write!(self.out, "(({c_unsigned})(").ok();
                        self.emit_expr(target);
                        write!(self.out, "))").ok();
                    } else {
                        let mask = (1u64 << width) - 1;
                        write!(self.out, "(({c_unsigned})(((").ok();
                        self.emit_expr(target);
                        write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                    }
                }
            }
            _ => unreachable!(),
        }
        true
    }

    /// Best-effort source-width inference for the width-method type
    /// checks. Returns the width in bits when it can be statically
    /// determined; `None` otherwise (callers skip the type-direction
    /// check in that case). Recognized shapes:
    ///   - Parenthesized expression → recurse into inner.
    ///   - `<expr> as uint<W>` / `<expr> as sint<W>` → W.
    ///   - `<expr>.trunc<W>()` / `.zext<W>()` / `.sext<W>()` / `.resize<W>()`
    ///     → W (the prior width-method's target width).
    ///   - Bare integer literal → minimum unsigned bit-width of the
    ///     literal value (cheap heuristic: `64 - leading_zeros(v)` for
    ///     positive values; `None` for negatives).
    fn infer_expr_width_best_effort(&self, e: &Expr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.infer_expr_width_best_effort(inner),
            ExprKind::Cast { ty, .. } => match ty {
                TypeExpr::Builtin {
                    name: BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits,
                    args,
                    ..
                } => type_arg_width(args).map(|w| w as u32),
                _ => None,
            },
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { name, .. } = &*callee.kind {
                    if Emitter::is_width_method_name(&name.name) {
                        if let Some(CallArg::Expr(w)) = args.first() {
                            return eval_const_width(w);
                        }
                    }
                }
                None
            }
            ExprKind::Int(s) => {
                // Parse the literal text to a u64 and use bit-width.
                let stripped = s.replace('_', "");
                let v: Option<u64> = if let Some(rest) = stripped
                    .strip_prefix("0x")
                    .or_else(|| stripped.strip_prefix("0X"))
                {
                    u64::from_str_radix(rest, 16).ok()
                } else if let Some(rest) = stripped
                    .strip_prefix("0b")
                    .or_else(|| stripped.strip_prefix("0B"))
                {
                    u64::from_str_radix(rest, 2).ok()
                } else {
                    stripped.parse::<u64>().ok()
                };
                v.map(|v| if v == 0 { 1 } else { 64 - v.leading_zeros() })
            }
            ExprKind::Ident(id) => self.let_widths.get(&id.name).copied(),
            _ => None,
        }
    }

    fn is_width_method_name(name: &str) -> bool {
        matches!(name, "trunc" | "zext" | "sext" | "resize")
    }

    fn try_emit_bus_field_access(&mut self, target: &Expr, name: &Ident) -> Option<String> {
        // <binding>.<signal>
        if let ExprKind::Ident(id) = &*target.kind {
            if let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() {
                if bus.signals.iter().any(|s| s.name.name == name.name) {
                    return Some(format!("{root}->{}_{}", sig_prefix, name.name));
                }
                // Already-flattened `<chan>_<sig>` form (or `<chan>_valid`
                // / `<chan>_ready`).
                for h in &bus.handshakes {
                    let chprefix = format!("{}_", h.name.name);
                    if name.name.starts_with(&chprefix) {
                        let tail = &name.name[chprefix.len()..];
                        if tail == "valid"
                            || tail == "ready"
                            || h.payload.iter().any(|s| s.name.name == tail)
                        {
                            return Some(format!("{root}->{}_{}", sig_prefix, name.name));
                        }
                    }
                }
                // Channel-name itself (used as a leaf in the two-level
                // form `bus.ch.sig`) — defer to the field-access path.
                if bus.handshakes.iter().any(|h| h.name.name == name.name) {
                    return None;
                }
                self.errors.push(format!(
                    "bus `{}` (binding `{}`) has no signal or channel named `{}`",
                    bus.name.name, id.name, name.name,
                ));
                return Some(format!("/* unresolved: {}.{} */ 0", id.name, name.name));
            }
        }
        // <binding>.<channel>.<signal>
        if let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        {
            if let ExprKind::Ident(id) = &*outer.kind {
                if let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() {
                    if let Some(h) = bus.handshakes.iter().find(|h| h.name.name == ch.name) {
                        if name.name == "valid"
                            || name.name == "ready"
                            || h.payload.iter().any(|s| s.name.name == name.name)
                        {
                            let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &name.name);
                            return Some(format!("{root}->{sig_port}"));
                        }
                        let valid_options: Vec<&str> = std::iter::once("valid")
                            .chain(std::iter::once("ready"))
                            .chain(h.payload.iter().map(|s| s.name.name.as_str()))
                            .collect();
                        self.errors.push(format!(
                            "bus `{}` channel `{}` has no signal `{}` (valid: {})",
                            bus.name.name,
                            ch.name,
                            name.name,
                            valid_options.join(", "),
                        ));
                        return Some(format!(
                            "/* unresolved: {}.{}.{} */ 0",
                            id.name, ch.name, name.name
                        ));
                    }
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) has no channel `{}`",
                        bus.name.name, id.name, ch.name,
                    ));
                    return Some(format!(
                        "/* unresolved: {}.{}.{} */ 0",
                        id.name, ch.name, name.name
                    ));
                }
            }
        }
        None
    }

    /// If `event` is the trigger of an `on obj.method pre/post`
    /// handler, resolve `obj.method` to a known hookable on a known
    /// component type. Returns `(component_type_name, method_name,
    /// params)`. Walks one or two levels of field-access chain
    /// (covers `<var>.<method>` and `<env>.<sub>.<method>`).
    fn resolve_component_hookable(&self, event: &Expr) -> Option<(String, String, Vec<Param>)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*event.kind
        else {
            return None;
        };
        // Walk the field-access chain, collecting path segments. Same
        // shape as `resolve_component_method_call` — see that function
        // for the rationale (env→agent→transactor.method requires
        // arbitrary-depth chain resolution, not just two levels).
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        // Resolve type at each step.
        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }

        // Find the hookable on cur_ty (component or transactor).
        let find_method = |items: &[ComponentItem], m: &str| -> Option<Vec<Param>> {
            for it in items {
                if let ComponentItem::Hookable(h) = it {
                    if h.name.name == m {
                        return Some(h.params.clone());
                    }
                }
            }
            None
        };
        let params = if let Some(comp) = self.components.get(&cur_ty) {
            find_method(&comp.items, &method.name)?
        } else if let Some(t) = self.transactors.get(&cur_ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            find_method(&synth.items, &method.name)?
        } else {
            return None;
        };
        Some((cur_ty, method.name.clone(), params))
    }

    /// If `callee` is a method call on a known component, return the
    /// triple `(<component_type>, <self_expr>, <method>)` so the caller
    /// can lower to `<component_type>_<method>(<self_expr>, args)`.
    /// Returns `None` when the call is not a method-on-component (so the
    /// generic plain-call path runs).
    ///
    /// Handles two shapes:
    /// - `<var>.<method>` — `var` is a let-bound component instance.
    /// - `<env>.<sub>.<method>` — `env` is a let-bound env-style
    ///   component, `<sub>` is one of its sub-component fields, and
    ///   `<method>` is a `hookable` on the sub-component's type.
    /// Detect built-in activity-tracking predicates `idle(N)`,
    /// `idle_in(N)`, `idle_out(N)` on a component-typed binding. Walks
    /// the field-access chain (same shape as `resolve_component_method_call`,
    /// but doesn't require a `hookable` method — these predicates read
    /// the auto-injected `_last_in_cycle` / `_last_out_cycle` fields
    /// instead). Returns `Some((instance_path, predicate_kind))` on
    /// match, where `predicate_kind` is `"idle"`, `"idle_in"`, or
    /// `"idle_out"`.
    ///
    /// **User-defined hookables win.** If the resolved component type
    /// already declares a `hookable <same-name>` method, we return
    /// `None` so the call falls through to the normal hookable-dispatch
    /// path. That keeps pre-existing fixtures (e.g. `buf_mgr_test`,
    /// which has a `hookable idle(n)` that holds bus valids low for
    /// `n` cycles) compiling unchanged. The built-in predicate is
    /// effectively a *default* — users override by declaring the
    /// method themselves.
    fn resolve_component_idle_predicate(&self, callee: &Expr) -> Option<(String, String)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        match method.name.as_str() {
            "idle" | "idle_in" | "idle_out" => {}
            _ => return None,
        }
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }
        // The target must resolve to a known component or transactor
        // type — those are what get the auto-injected heartbeat fields.
        let has_hookable_override = |items: &[ComponentItem]| -> bool {
            items.iter().any(|it| {
                matches!(
                    it, ComponentItem::Hookable(h) if h.name.name == method.name
                )
            })
        };
        let user_overrides = if let Some(comp) = self.components.get(&cur_ty) {
            has_hookable_override(&comp.items)
        } else if let Some(t) = self.transactors.get(&cur_ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            has_hookable_override(&synth.items)
        } else {
            return None;
        };
        if user_overrides {
            // User has a hookable with this name — defer to the
            // normal `Type_method(obj, args)` dispatch path.
            return None;
        }
        Some((path.join("."), method.name.clone()))
    }

    fn resolve_component_path(&self, target: &Expr) -> Option<(Vec<String>, String)> {
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }

        if self.components.contains_key(&cur_ty) || self.transactors.contains_key(&cur_ty) {
            Some((path, cur_ty))
        } else {
            None
        }
    }

    fn component_has_hookable(&self, ty: &str, method: &str) -> bool {
        let has = |items: &[ComponentItem]| -> bool {
            items.iter().any(|it| {
                matches!(
                    it, ComponentItem::Hookable(h) if h.name.name == method
                )
            })
        };
        if let Some(comp) = self.components.get(ty) {
            has(&comp.items)
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            has(&synth.items)
        } else {
            false
        }
    }

    fn collect_quiesced_paths(
        &self,
        ty: &str,
        instance: &str,
        stack: &mut std::collections::HashSet<String>,
        out: &mut Vec<String>,
    ) {
        if !stack.insert(ty.to_string()) {
            out.push(instance.to_string());
            return;
        }

        let mut found_subcomponent = false;
        if let Some(comp) = self.components.get(ty) {
            for it in &comp.items {
                if let ComponentItem::Field(f) = it {
                    let Some(field_ty) = type_simple_name(Some(&f.ty)) else {
                        continue;
                    };
                    if self.components.contains_key(field_ty)
                        || self.transactors.contains_key(field_ty)
                    {
                        found_subcomponent = true;
                        let sub_instance = format!("{instance}.{}", f.name.name);
                        self.collect_quiesced_paths(field_ty, &sub_instance, stack, out);
                    }
                }
            }
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            for it in &synth.items {
                if let ComponentItem::Field(f) = it {
                    let Some(field_ty) = type_simple_name(Some(&f.ty)) else {
                        continue;
                    };
                    if self.components.contains_key(field_ty)
                        || self.transactors.contains_key(field_ty)
                    {
                        found_subcomponent = true;
                        let sub_instance = format!("{instance}.{}", f.name.name);
                        self.collect_quiesced_paths(field_ty, &sub_instance, stack, out);
                    }
                }
            }
        }

        if !found_subcomponent {
            out.push(instance.to_string());
        }
        stack.remove(ty);
    }

    fn resolve_component_quiesced_predicate<'a>(
        &self,
        callee: &Expr,
        args: &'a [CallArg],
    ) -> Option<(Vec<String>, &'a Expr)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        if method.name != "quiesced" {
            return None;
        }
        let (path, ty) = self.resolve_component_path(target)?;
        if self.component_has_hookable(&ty, "quiesced") {
            return None;
        }
        if args.len() != 1 {
            return None;
        }
        let n_expr = match &args[0] {
            CallArg::Expr(e) => e,
            CallArg::Named { value, .. } => value,
        };
        let mut paths = Vec::new();
        self.collect_quiesced_paths(
            &ty,
            &path.join("."),
            &mut std::collections::HashSet::new(),
            &mut paths,
        );
        Some((paths, n_expr))
    }

    fn emit_idle_predicate(&mut self, instance: &str, kind: &str, n_expr: &Expr) {
        match kind {
            "idle_in" => {
                write!(
                    self.out,
                    "(((uint64_t)cycle_count - {instance}._last_in_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(self.out, "))").ok();
            }
            "idle_out" => {
                write!(
                    self.out,
                    "(((uint64_t)cycle_count - {instance}._last_out_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(self.out, "))").ok();
            }
            "idle" => {
                write!(
                    self.out,
                    "((((uint64_t)cycle_count - {instance}._last_in_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(
                    self.out,
                    ")) && (((uint64_t)cycle_count - {instance}._last_out_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(self.out, ")))").ok();
            }
            _ => unreachable!(),
        }
    }

    fn expand_wait_conditions<'a>(&self, conditions: &'a [Expr]) -> Vec<WaitCondition<'a>> {
        let mut out = Vec::new();
        for c in conditions {
            if let ExprKind::Call { callee, args } = &*c.kind {
                if let Some((paths, cycles)) =
                    self.resolve_component_quiesced_predicate(callee, args)
                {
                    out.extend(
                        paths
                            .into_iter()
                            .map(|instance| WaitCondition::Idle { instance, cycles }),
                    );
                    continue;
                }
            }
            out.push(WaitCondition::Expr(c));
        }
        out
    }

    fn resolve_component_method_call(&self, callee: &Expr) -> Option<(String, String, String)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        // Walk the field-access chain to its root, collecting the path
        // segments. `topenv.ag.sequencer.dispatch` produces
        // path = ["topenv", "ag", "sequencer"] with method = "dispatch".
        // The root must be an `Ident` (a let-bound name); inner
        // segments are field names we resolve through the type chain.
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        // Resolve the type chain. Start from let_types[root]; walk
        // remaining path segments looking each up as a Field on the
        // current type.
        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }

        // Does cur_ty (component or transactor) have a `hookable
        // <method>`?
        let has_method = |items: &[ComponentItem]| -> bool {
            items.iter().any(|it| {
                matches!(
                    it, ComponentItem::Hookable(h) if h.name.name == method.name
                )
            })
        };
        let found = if let Some(comp) = self.components.get(&cur_ty) {
            has_method(&comp.items)
        } else if let Some(t) = self.transactors.get(&cur_ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            has_method(&synth.items)
        } else {
            false
        };
        if !found {
            return None;
        }

        Some((cur_ty, path.join("."), method.name.clone()))
    }

    /// Walk the field-access chain rooted at `callee` (which must be a
    /// `Field { target, name }` call target) and return the path of
    /// segment names from the root let-binding down to (but excluding)
    /// the method name. Mirrors the path-extraction half of
    /// `resolve_component_method_call`; factored out so the passive-mode
    /// check can re-use the same path without duplicating the walker.
    /// Returns `None` if the callee isn't a field-access chain rooted
    /// at an `Ident`.
    fn extract_method_call_path(callee: &Expr) -> Option<Vec<String>> {
        let ExprKind::Field { target, name: _ } = &*callee.kind else {
            return None;
        };
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();
        Some(path)
    }

    /// Resolve the effective transactor mode at the end of a
    /// field-access path. The root segment must be a let-bound name
    /// whose mode is in `self.let_modes` (the inheritance root); each
    /// subsequent segment can override via its field-level mode
    /// annotation, otherwise it inherits. Same model as
    /// `emit_subcomponent_handler_registrations` — kept here as a pure
    /// `&self` resolver so the call-site check can run during
    /// `emit_expr` without conflicting with the mutable emitter.
    ///
    /// Returns `None` when the root has no recorded mode (e.g. the
    /// path roots at a non-transactor non-env let), or when an
    /// intermediate type doesn't resolve cleanly. Both cases mean "no
    /// passive-mode constraint to enforce at this site."
    fn resolve_path_mode(&self, path: &[String]) -> Option<TransactorMode> {
        let root = path.first()?;
        let mut effective: Option<TransactorMode> = self.let_modes.get(root).copied();
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let lookup = |items: &[ComponentItem]| -> Option<(String, Option<TransactorMode>)> {
                items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            let ty_name = type_simple_name(Some(&f.ty)).map(String::from)?;
                            let mode = match &f.ty {
                                TypeExpr::Named { mode: Some(m), .. } => Some(*m),
                                _ => None,
                            };
                            return Some((ty_name, mode));
                        }
                    }
                    None
                })
            };
            let next = if let Some(comp) = self.components.get(&cur_ty) {
                lookup(&comp.items)
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                lookup(&synth.items)
            } else {
                None
            };
            let (ty, mode_opt) = next?;
            cur_ty = ty;
            if let Some(m) = mode_opt {
                effective = Some(m);
            }
        }
        effective
    }

    /// True iff `method_name` is declared as a `hookable` inside
    /// `transactor_name`'s `when active { ... }` block (and is NOT also
    /// shadowed in the always-on body — though that's a parse error in
    /// practice).
    fn method_lives_in_when_active(&self, transactor_name: &str, method_name: &str) -> bool {
        let Some(t) = self.transactors.get(transactor_name) else {
            return false;
        };
        let Some(when_active) = &t.when_active else {
            return false;
        };
        when_active
            .iter()
            .any(|it| matches!(it, ComponentItem::Hookable(h) if h.name.name == method_name))
    }

    /// Emit a single `hookable` method on a component as a free
    /// `[&]`-capturing lambda named `<Type>_<method>`. The first
    /// parameter is `<Type>& self`; subsequent parameters mirror the
    /// HARC declaration. Inside the body, bare references to component
    /// fields are rewritten to `self.<field>` via `field_subs`. DUT
    /// pointer fields stay arrow-accessed: `self.dut->aw_addr = ...`.
    /// Component-aware C++ type rendering for a HARC `TypeExpr`.
    /// Wraps `c_type_for` with awareness of declared transactions /
    /// enums / sub-components: those Named types lower to their bare
    /// struct/enum name rather than the default `V<Name>*` pointer.
    /// Used by hookable-method param types and other component-body
    /// contexts where DUT vs. value-type ambiguity would otherwise
    /// produce wrong code.
    fn c_type_for_param(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last() {
                let n = &last.name;
                if self.transactions.contains(n)
                    || self.enums.contains_key(n)
                    || self.components.contains_key(n)
                    || self.scoreboards.contains(n)
                    || self.transactors.contains_key(n)
                    || self.covergroups.contains_key(n)
                {
                    return n.clone();
                }
            }
        }
        c_type_for(t)
    }

    /// Emit the two `<Type>_<method>_pre` and `<Type>_<method>_post`
    /// `std::vector<std::function<void(args)>>` declarations that
    /// hold the registered hook subscribers for one hookable method.
    /// Empty by default; users push closures via `on obj.method pre`
    /// / `on obj.method post` at test scope.
    fn emit_hook_vectors(&mut self, c: &ComponentDecl, h: &HookableMethod, depth: usize) {
        // `function` methods (testbench / env helpers, docs/test-
        // ergonomics.md §3.2) carry `is_hookable = false`: no pre/post
        // vectors emitted, and the corresponding fan-out in
        // `emit_component_method` is suppressed. The method itself
        // still emits as a free `<Type>_<method>` lambda.
        if !h.is_hookable {
            return;
        }
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let arg_tys: Vec<String> = h
            .params
            .iter()
            .map(|p| {
                p.ty.as_ref()
                    .map(|t| self.c_type_for_param(t))
                    .unwrap_or_else(|| "int64_t".to_string())
            })
            .collect();
        let arg_csv = arg_tys.join(", ");
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_pre;",
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_post;",
        )
        .ok();
    }

    fn emit_component_method(&mut self, c: &ComponentDecl, h: &HookableMethod, depth: usize) {
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let param_names = cpp_param_names(&h.params);
        let ret = h
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or_else(|| "void".to_string());
        self.pad(depth);
        write!(self.out, "auto {comp_ty}_{m_name} = [&]({comp_ty}& self").ok();
        // Track Named-typed params as pointers so dut.field rewrites
        // properly in the body. Restore on exit. Transaction / enum /
        // sub-component params are by-value (not pointer-shaped).
        let mut added: Vec<String> = Vec::new();
        for (i, p) in h.params.iter().enumerate() {
            let pty =
                p.ty.as_ref()
                    .map(|t| self.c_type_for_param(t))
                    .unwrap_or_else(|| "int64_t".to_string());
            write!(self.out, ", {pty} {}", param_names[i]).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. }))
                && self.is_dut_pointer_field_type(p.ty.as_ref().unwrap())
                && p.name.name != "_"
            {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        writeln!(self.out, ") -> {ret} {{").ok();
        // Build field-name substitution: bare `count` inside the body
        // resolves to `self.count`. Dut-pointer fields also get a
        // pointer_vars entry so `dut.field` lowers to `dut->field`.
        let mut subs = std::collections::HashMap::new();
        let mut added_pointer_fields: Vec<String> = Vec::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("self.{}", f.name.name));
                if self.is_dut_pointer_field_type(&f.ty) {
                    if self.pointer_vars.insert(f.name.name.clone()) {
                        added_pointer_fields.push(f.name.name.clone());
                    }
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        // For a `bound to BusType` driver, the parent's bus binding
        // (resolved at codegen time from the test's `let drv : Drv =
        // bind axil` statement) propagates into hookable method
        // bodies. Inside the body, the bare identifier `bus` resolves
        // to the same `(BusDecl, root, prefix)` tuple as the
        // test-scope binding, so `bus.<ch>.send(...)`, `bus.<ch>.recv()`,
        // and `bus.<ch>.<sig>` all work identically to the patterns
        // available inside `on T t` handlers.
        //
        // Restriction (single-instance): we use the FIRST binding
        // discovered for this driver type. If the test instantiates
        // two `let A : Drv = bind X` and `let B : Drv = bind Y` with
        // different buses, both share A's binding inside the
        // hookable body. v0 doesn't have a use case for
        // multi-instance bound drivers; per-instance hookable
        // emission is a follow-up.
        let pushed_bus = self.driver_bus_for_hookables.get(comp_ty).cloned();
        let prior_bus = pushed_bus
            .as_ref()
            .and_then(|b| self.bus_bindings.insert("bus".into(), b.clone()));

        // Pre-hooks: fire `<Type>_<method>_pre` subscribers before the
        // body. The hook closures see the same args as the method —
        // empty vectors are a no-op so the wrap is always safe to
        // emit.
        let arg_csv = param_names.join(", ");
        // Pre/post hook fan-out is skipped for non-hookable `function`
        // methods (docs/test-ergonomics.md §3.2): no vectors were
        // emitted for them, so the fan-out would reference undeclared
        // symbols.
        if h.is_hookable {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (auto& _h : {comp_ty}_{m_name}_pre) _h({arg_csv});"
            )
            .ok();
        }

        self.emit_block(&h.body, depth + 1);

        if h.is_hookable {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (auto& _h : {comp_ty}_{m_name}_post) _h({arg_csv});"
            )
            .ok();
        }
        let _ = arg_csv;
        // Restore state.
        if pushed_bus.is_some() {
            match prior_bus {
                Some(prev) => {
                    self.bus_bindings.insert("bus".into(), prev);
                }
                None => {
                    self.bus_bindings.remove("bus");
                }
            }
        }
        self.field_subs = prev_subs;
        for k in added_pointer_fields {
            self.pointer_vars.remove(&k);
        }
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    /// Default watchdog period (cycles) when the user writes `watchdog`
    /// without an explicit `period` clause. Spec §8.6.
    const WATCHDOG_DEFAULT_PERIOD: i64 = 1000;
    /// Default watchdog idle threshold (cycles) when the user writes
    /// `watchdog` without an explicit `max_idle` clause.
    const WATCHDOG_DEFAULT_MAX_IDLE: i64 = 10000;

    /// Emit hook-vector declarations + the synthetic `<Type>_watchdog`
    /// method for a component's `watchdog` block (spec §8.6). The
    /// method body, in order:
    ///   1. Pre-hooks (`<Type>_watchdog_pre`) — for `on <Type>.watchdog
    ///      pre` aspect attachments.
    ///   2. User-supplied body statements (typically `log(info, …)`
    ///      debug prints). Field references rewrite to `self.<field>`
    ///      via the same `field_subs` mechanism used for hookable
    ///      methods.
    ///   3. The idle check: if BOTH `_last_in_cycle` AND
    ///      `_last_out_cycle` are ≥ `max_idle` cycles behind
    ///      `cycle_count`, log `FAIL` with a watchdog-specific message
    ///      and bump `errors`.
    ///   4. Post-hooks (`<Type>_watchdog_post`).
    ///
    /// `disabled` watchdogs emit nothing — no hook vectors, no method.
    /// External `on <Type>.watchdog pre/post` referencing a disabled
    /// component's watchdog will surface as a missing-symbol C++
    /// compile error, which is the intended signal that the aspect
    /// has no target. (Reasonable people who turned the watchdog
    /// off will know to remove their hooks.)
    fn emit_watchdog(&mut self, c: &ComponentDecl, w: &WatchdogDecl, depth: usize) {
        if w.disabled {
            return;
        }
        let comp_ty = &c.name.name;
        // Hook vectors — `watchdog` takes no args, so `void()` signature.
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void()>> {comp_ty}_watchdog_pre;"
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void()>> {comp_ty}_watchdog_post;"
        )
        .ok();
        // The method itself: a `[&]`-capturing lambda parallelling the
        // shape of `emit_component_method` so the hookable-dispatch
        // path (`<Type>_<method>(obj, args)`) finds the same symbol.
        self.pad(depth);
        writeln!(
            self.out,
            "auto {comp_ty}_watchdog = [&]({comp_ty}& self) -> void {{"
        )
        .ok();
        // Field substitution: bare `wdog_max_idle` inside the user body
        // resolves to `self.wdog_max_idle`. Same shape as
        // emit_component_method's `subs` setup.
        let mut subs = std::collections::HashMap::new();
        let mut added_pointer_fields: Vec<String> = Vec::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("self.{}", f.name.name));
                if self.is_dut_pointer_field_type(&f.ty) {
                    if self.pointer_vars.insert(f.name.name.clone()) {
                        added_pointer_fields.push(f.name.name.clone());
                    }
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        // Pre-hooks.
        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_watchdog_pre) _h();").ok();

        // User body (typically debug logging).
        self.emit_block(&w.body, depth + 1);

        // Idle check. We emit the C++ directly (not through the HARC
        // `idle()` predicate dispatch) because the receiver is `self`,
        // not a let-bound variable, and `resolve_component_idle_predicate`
        // expects to resolve through `let_types`. Direct C++ keeps
        // the synthetic method self-contained and avoids polluting
        // let_types with the synthetic name `self`.
        self.pad(depth + 1);
        write!(self.out, "int64_t _wdog_max_idle = (int64_t)(").ok();
        if let Some(m) = &w.max_idle {
            self.emit_expr(m);
        } else {
            write!(self.out, "{}", Self::WATCHDOG_DEFAULT_MAX_IDLE).ok();
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (_wdog_max_idle > 0 \
             && (int64_t)((uint64_t)cycle_count - self._last_in_cycle) >= _wdog_max_idle \
             && (int64_t)((uint64_t)cycle_count - self._last_out_cycle) >= _wdog_max_idle) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "sim_log_line(\"FAIL\", \"watchdog: {comp_ty} has been idle for >= %lld cycles\", \
             (long long)_wdog_max_idle);"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "errors++;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        // Post-hooks.
        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_watchdog_post) _h();").ok();

        // Restore state.
        self.field_subs = prev_subs;
        for k in added_pointer_fields {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    /// Emit a periodic `_checkers` closure that calls `<Type>_watchdog(<inst>)`
    /// every `period` cycles. Installed at every `let foo : Agent` site
    /// (including sub-component composition). The closure re-reads
    /// the period expression each cycle so per-test overrides via
    /// field assignment work without re-installation.
    fn emit_watchdog_checker(
        &mut self,
        c: &ComponentDecl,
        w: &WatchdogDecl,
        instance: &str,
        depth: usize,
    ) {
        if w.disabled {
            return;
        }
        let comp_ty = &c.name.name;
        // The instance path may contain dots (`env.agent`); the static
        // tag needs to be a valid C++ identifier, so flatten dots to
        // underscores.
        let inst_tag = instance.replace('.', "_");
        // Field substitution: a period expression like `wdog_period`
        // resolves to `<instance>.wdog_period` (parallels how field
        // refs inside `on event(t)` bodies work at let-time).
        let mut subs = std::collections::HashMap::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "static int64_t _wdog_{inst_tag}_last = 0;").ok();
        self.pad(depth + 1);
        write!(self.out, "int64_t _wdog_{inst_tag}_period = (int64_t)(").ok();
        if let Some(p) = &w.period {
            self.emit_expr(p);
        } else {
            write!(self.out, "{}", Self::WATCHDOG_DEFAULT_PERIOD).ok();
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (_wdog_{inst_tag}_period > 0 \
             && (int64_t)cycle_count - _wdog_{inst_tag}_last >= _wdog_{inst_tag}_period) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "_wdog_{inst_tag}_last = (int64_t)cycle_count;").ok();
        self.pad(depth + 2);
        writeln!(self.out, "{comp_ty}_watchdog({instance});").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        writeln!(self.out, "}});").ok();

        self.field_subs = prev_subs;
    }

    /// Emit a `driver` / `agent` / `env` / `sequencer` body as a C++
    /// struct. Field types are resolved through `component_field_c_type`,
    /// which recognizes:
    ///
    /// - `event<T>` → `std::vector<std::function<void(T)>>`
    /// - `queue<T>` → `HarcQueue<T>` (when scoreboards are also present)
    /// - Named DUT module → `V<Name>*` pointer (the same shape the
    ///   test-level `let dut : T` already uses)
    /// - Named sub-component → that struct directly (for `env` composing
    ///   `agent` / `driver` / etc. by-value)
    /// - Builtins → uint64_t / int64_t / bool
    ///
    /// `hookable` methods are emitted separately as free `[&]`-capturing
    /// lambdas (see `emit_component_methods`); the struct body holds
    /// data only.
    fn emit_component_struct(&mut self, c: &ComponentDecl) {
        writeln!(self.out, "struct {} {{", c.name.name).ok();
        for it in &c.items {
            if let ComponentItem::Field(f) = it {
                let cty = self.component_field_c_type(&f.ty);
                let init = if let Some(d) = &f.default {
                    format!(" = {}", format_simple_expr(d))
                } else if matches!(&f.ty, TypeExpr::Named { .. })
                    && self.is_dut_pointer_field_type(&f.ty)
                {
                    // Pointer fields default to nullptr — caller assigns
                    // via `drv.dut = dut` after construction.
                    " = nullptr".into()
                } else {
                    "".into()
                };
                writeln!(self.out, "{INDENT}{cty} {}{};", f.name.name, init).ok();
            }
        }
        // Auto-injected activity-tracking fields (spec §7.x). Bumped by
        // codegen at every place the framework knows an in/out has just
        // happened — `on <ev>` handler body entry, bus handshake actor
        // body entry, `bus.<ch>.send/recv`, `emit ev(arg)` — and read
        // by the `obj.idle(N)` / `idle_in(N)` / `idle_out(N)` predicate
        // lowering. Initial value 0 means "no activity yet"; the
        // predicate `idle(N)` correctly reports false until at least N
        // cycles have elapsed since the last bump.
        writeln!(self.out, "{INDENT}uint64_t _last_in_cycle = 0;").ok();
        writeln!(self.out, "{INDENT}uint64_t _last_out_cycle = 0;").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// True if a Named-type field on a component refers to a DUT module
    /// (i.e. nothing the codegen knows about other than that it's a
    /// Verilator-compiled module type). Sub-components / scoreboards /
    /// transactors / covergroups are excluded — they're held by-value.
    fn is_dut_pointer_field_type(&self, t: &TypeExpr) -> bool {
        if let Some(name) = type_simple_name(Some(t)) {
            return !self.transactions.contains(name)
                && !self.scoreboards.contains(name)
                && !self.covergroups.contains_key(name)
                && !self.components.contains_key(name)
                && !self.transactors.contains_key(name)
                && !self.enums.contains_key(name);
        }
        false
    }

    /// Field-type lowering for `driver`/`agent`/`env`/`sequencer` bodies.
    fn component_field_c_type(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Builtin {
                name: BuiltinTy::Event,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("std::vector<std::function<void({inner})>>")
            }
            TypeExpr::Builtin {
                name: BuiltinTy::Queue,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("HarcQueue<{inner}>")
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if self.is_dut_pointer_field_type(t) {
                    // DUT module → Verilator pointer.
                    format!("V{last}*")
                } else {
                    // Sub-component / scoreboard / transaction → by-value.
                    last.to_string()
                }
            }
            _ => txn_field_c_type(t),
        }
    }

    /// Resolve a `<T>` payload arg (the inside of `event<T>`,
    /// `queue<T>`, etc.) to a C++ type name. User-declared
    /// transactions and enums get their bare name (so payloads
    /// round-trip through C++ as the struct/enum directly);
    /// everything else falls back to `txn_field_c_type` which
    /// widens narrow ints into `int64_t`/`uint64_t`.
    fn payload_type_for_arg(&self, arg: Option<&TypeArg>) -> String {
        match arg {
            Some(TypeArg::Type(ty)) => {
                if let Some(name) = type_simple_name(Some(ty)) {
                    if self.transactions.contains(name) || self.enums.contains_key(name) {
                        return name.to_string();
                    }
                }
                txn_field_c_type(ty)
            }
            Some(TypeArg::Expr(e)) => {
                // `event<RegOp>` parses as TypeArg::Expr(Ident) at the
                // type-arg layer — the parser doesn't always know the
                // arg's a type until the user actually references it.
                if let ExprKind::Ident(id) = &*e.kind {
                    if self.transactions.contains(&id.name) || self.enums.contains_key(&id.name) {
                        return id.name.clone();
                    }
                }
                "uint64_t".into()
            }
            _ => "uint64_t".into(),
        }
    }

    fn emit_scoreboard(&mut self, s: &ComponentDecl) {
        writeln!(self.out, "struct {} {{", s.name.name).ok();
        for it in &s.items {
            if let ComponentItem::Field(f) = it {
                let cty = scoreboard_field_c_type(&f.ty);
                let init = if let Some(d) = &f.default {
                    format!(" = {}", format_simple_expr(d))
                } else {
                    "".into()
                };
                writeln!(self.out, "{INDENT}{cty} {}{};", f.name.name, init).ok();
            }
        }
        // Auto-injected activity-tracking fields (spec §7.x). Same shape
        // as for driver/agent/env/sequencer in `emit_component_struct`
        // — scoreboards also count as components, so `sb.idle(N)` is
        // a valid watchdog predicate.
        writeln!(self.out, "{INDENT}uint64_t _last_in_cycle = 0;").ok();
        writeln!(self.out, "{INDENT}uint64_t _last_out_cycle = 0;").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// Emit a transaction as a C++ struct + a `randomize_T(&t)` function
    /// that fills random fields per-attribute. Non-random (`!`-prefixed)
    /// fields are zero-initialized in the struct's default ctor and left
    /// alone by randomize. The user can set them in code.
    fn emit_transaction(&mut self, t: &TransactionDecl) {
        // Struct definition.
        writeln!(self.out, "struct {} {{", t.name.name).ok();
        for it in &t.body {
            if let TxnBodyItem::Field(f) = it {
                let cty = txn_field_c_type(&f.ty);
                let init = field_default(f);
                writeln!(self.out, "{INDENT}{cty} {} = {init};", f.name.name).ok();
            }
        }
        writeln!(self.out, "}};").ok();

        // Structural equality (spec §3.3) — transactions are value records
        // with built-in deep-equal semantics. UVM's `t.compare(exp)` boils
        // down to this for free. `!=` follows by negation.
        let field_names: Vec<&str> = t
            .body
            .iter()
            .filter_map(|it| match it {
                TxnBodyItem::Field(f) => Some(f.name.name.as_str()),
                _ => None,
            })
            .collect();
        if field_names.is_empty() {
            writeln!(self.out, "inline bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
                t.name.name).ok();
        } else {
            write!(
                self.out,
                "inline bool operator==(const {0}& a, const {0}& b) {{ return ",
                t.name.name
            )
            .ok();
            for (i, fname) in field_names.iter().enumerate() {
                if i > 0 {
                    write!(self.out, " && ").ok();
                }
                write!(self.out, "a.{fname} == b.{fname}").ok();
            }
            writeln!(self.out, "; }}").ok();
        }
        writeln!(
            self.out,
            "inline bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
            t.name.name
        )
        .ok();
        writeln!(self.out, "").ok();

        // randomize_T(t) function.
        writeln!(
            self.out,
            "static void randomize_{}({}* t) {{",
            t.name.name, t.name.name
        )
        .ok();
        for it in &t.body {
            if let TxnBodyItem::Field(f) = it {
                if f.non_random {
                    if let Some(d) = &f.default {
                        write!(self.out, "{INDENT}t->{} = ", f.name.name).ok();
                        self.emit_expr(d);
                        writeln!(self.out, ";").ok();
                    }
                    continue;
                }
                self.emit_field_random(f);
            }
        }
        writeln!(self.out, "}}").ok();
        writeln!(self.out, "").ok();
    }

    fn emit_randomize_trace_event(&mut self, ty: &str, target: &Expr, depth: usize) {
        let Some(fields) = self.txn_fields.get(ty).cloned() else {
            return;
        };
        self.pad(depth);
        writeln!(self.out, "if (trace.enabled) {{").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "std::string _trace_fields = \"\\\"txn_type\\\":\\\"{}\\\",\\\"fields\\\":{{\";",
            escape_c(ty)
        )
        .ok();
        for (i, f) in fields.iter().enumerate() {
            self.pad(depth + 1);
            let prefix = if i == 0 {
                format!("\\\"{}\\\":", escape_c(&f.name))
            } else {
                format!(",\\\"{}\\\":", escape_c(&f.name))
            };
            write!(
                self.out,
                "_trace_fields += \"{prefix}\" + std::to_string((unsigned long long)("
            )
            .ok();
            self.emit_expr(target);
            writeln!(self.out, ".{}));", f.name).ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "_trace_fields += \"}}\";").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "trace.raw(\"randomize\", cycle_count, _trace_fields);"
        )
        .ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Emit one random-field assignment honouring `[range(...)]` and
    /// `[dist {...}]` attributes. Falls back to a uniform sample over the
    /// declared type's value range.
    fn emit_field_random(&mut self, f: &Field) {
        // Look for a `[range(lo, hi)]` or `[dist {...}]` attribute.
        let mut handled = false;
        for a in &f.attrs {
            match a.name.name.as_str() {
                "range" => {
                    if a.args.len() >= 2 {
                        if let (AttrArg::Expr(lo), AttrArg::Expr(hi)) = (&a.args[0], &a.args[1]) {
                            write!(self.out, "{INDENT}t->{} = harc_rng_range(", f.name.name).ok();
                            self.emit_expr(lo);
                            write!(self.out, ", ").ok();
                            self.emit_expr(hi);
                            writeln!(self.out, ");").ok();
                            handled = true;
                        }
                    }
                }
                "dist" => {
                    let dist_args: Option<&Vec<DistEntry>> = a.args.iter().find_map(|x| match x {
                        AttrArg::Dist(d) => Some(d),
                        _ => None,
                    });
                    if let Some(entries) = dist_args {
                        write!(self.out, "{INDENT}t->{} = harc_rng_dist({{", f.name.name).ok();
                        self.emit_rng_dist_entries(entries);
                        writeln!(self.out, "}});").ok();
                        handled = true;
                    }
                }
                _ => {}
            }
            if handled {
                break;
            }
        }
        if handled {
            return;
        }

        // Fallback: type-driven uniform sampling.
        match &f.ty {
            TypeExpr::Builtin { name, args, .. } => {
                let width = type_arg_width(args);
                match name {
                    BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rng_uint({});",
                            f.name.name,
                            width.unwrap_or(32)
                        )
                        .ok();
                    }
                    BuiltinTy::SInt | BuiltinTy::SIntCap => {
                        let w = width.unwrap_or(32);
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rng_range(-(1LL << {}), (1LL << {}) - 1);",
                            f.name.name,
                            w - 1,
                            w - 1
                        )
                        .ok();
                    }
                    BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rng_range(0, 1);",
                            f.name.name
                        )
                        .ok();
                    }
                    BuiltinTy::Int => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rng_range(0, 0x7FFFFFFF);",
                            f.name.name
                        )
                        .ok();
                    }
                    _ => {
                        writeln!(
                            self.out,
                            "{INDENT}// {} : <unsupported type for v0 randomize>",
                            f.name.name
                        )
                        .ok();
                    }
                }
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if let Some(&n) = self.enums.get(last) {
                    let hi = if n == 0 { 0 } else { (n - 1) as i64 };
                    writeln!(
                        self.out,
                        "{INDENT}t->{} = harc_rng_range(0, {});",
                        f.name.name, hi
                    )
                    .ok();
                } else {
                    writeln!(
                        self.out,
                        "{INDENT}// {} : {} (named, not yet supported)",
                        f.name.name, last
                    )
                    .ok();
                }
            }
        }
    }

    fn emit_rng_dist_entries(&mut self, entries: &[DistEntry]) {
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            // Each dist entry: (lo, hi, weight). The `value` expression is
            // either a RangeLit (use lo/hi) or a scalar (use as both lo/hi).
            match &*e.value.kind {
                ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(lo);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(hi);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
                _ => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
            }
        }
    }

    fn emit_solver_range_constraint(
        &mut self,
        f: &TxnFieldInfo,
        lo: &Expr,
        hi: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        depth: usize,
    ) {
        self.pad(depth);
        write!(self.out, "_s.add(").ok();
        if f.signed {
            write!(self.out, "_z_{} >= ", f.name).ok();
            self.emit_constraint_expr_w(lo, field_info, 64);
            write!(self.out, " && _z_{} <= ", f.name).ok();
            self.emit_constraint_expr_w(hi, field_info, 64);
        } else {
            write!(self.out, "z3::uge(_z_{}, ", f.name).ok();
            self.emit_constraint_expr_w(lo, field_info, 64);
            write!(self.out, ") && z3::ule(_z_{}, ", f.name).ok();
            self.emit_constraint_expr_w(hi, field_info, 64);
            write!(self.out, ")").ok();
        }
        writeln!(self.out, ");").ok();
    }

    /// Inline a list of top-level constraint expressions, expanding
    /// any `Call(Ident(R), args)` whose name resolves to a known
    /// `relation R(params) … end relation` (spec §4.2). Each call:
    ///   * Looks up R in `self.relations`.
    ///   * Builds a substitution map `formal_param → actual_arg`.
    ///   * For `RelationBody::Block(exprs)`, returns each expr with
    ///     substitution applied — each becomes its own constraint at
    ///     top level (`_s.add(...)` per body expr).
    ///   * For `RelationBody::Alias(expr)`, returns a 1-element Vec
    ///     of the substituted expr.
    ///
    /// Non-call expressions are also walked with `expand_relation_subtree`
    /// so a nested relation call (e.g. inside a `Binary &&`) expands too.
    /// Recursion handles relations of relations.
    fn expand_relation_calls(&self, exprs: &[Expr]) -> Vec<Expr> {
        let mut out: Vec<Expr> = Vec::with_capacity(exprs.len());
        for e in exprs {
            // Top-level Call: expand to potentially multiple constraints.
            if let Some(expanded) = self.try_expand_top_level_call(e) {
                // Each body expr is itself walked so block-form
                // relations that contain nested calls flatten too.
                for be in &expanded {
                    out.extend(self.expand_relation_calls(&[be.clone()]));
                }
                continue;
            }
            // Non-call (or unknown-name call): walk the subtree so any
            // nested R(args) inside Binary / Unary / Paren / etc.
            // expands inline.
            out.push(self.expand_relation_subtree(e));
        }
        out
    }

    /// Try to expand `e` as a top-level relation call. Returns the
    /// substituted body expressions if `e` is `Call(Ident(R), args)`
    /// for a known relation; `None` otherwise (caller falls back to
    /// subtree walking). Arity-mismatched calls return `None` so the
    /// downstream translator surfaces a "constraint expression not
    /// supported" error with a useful span — better than swallowing.
    fn try_expand_top_level_call(&self, e: &Expr) -> Option<Vec<Expr>> {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(id) = &*callee.kind else {
            return None;
        };
        let rel = self.relations.get(&id.name)?;
        if rel.params.len() != args.len() {
            return None;
        }
        let mut subst: std::collections::HashMap<String, Expr> = std::collections::HashMap::new();
        for (p, a) in rel.params.iter().zip(args.iter()) {
            let arg_expr = match a {
                CallArg::Expr(ex) => ex.clone(),
                CallArg::Named { value, .. } => value.clone(),
            };
            subst.insert(p.name.name.clone(), arg_expr);
        }
        let body_exprs: Vec<Expr> = match &rel.body {
            RelationBody::Block(exprs) => {
                exprs.iter().map(|x| substitute_idents(x, &subst)).collect()
            }
            RelationBody::Alias(expr) => vec![substitute_idents(expr, &subst)],
        };
        Some(body_exprs)
    }

    /// Walk a constraint expression, replacing any nested
    /// `Call(Ident(R), args)` (for a known relation R) with R's body.
    /// Block-form bodies collapse to a `&&`-chain so they fit where a
    /// single expression is expected (e.g. inside a `Binary &&`); use
    /// `expand_relation_calls` at the top level to get one constraint
    /// per body expression.
    fn expand_relation_subtree(&self, expr: &Expr) -> Expr {
        let span = expr.span;
        // Recognize a relation Call anywhere in the tree. For block-form
        // bodies, build the AND-of-all-body-exprs expression so the
        // call site (which expected one Expr) gets one Expr back.
        if let Some(body_exprs) = self.try_expand_top_level_call(expr) {
            let exprs: Vec<Expr> = body_exprs
                .iter()
                .map(|x| self.expand_relation_subtree(x))
                .collect();
            return and_join(&exprs, span);
        }
        let new_kind: ExprKind = match &*expr.kind {
            ExprKind::Field { target, name } => ExprKind::Field {
                target: self.expand_relation_subtree(target),
                name: name.clone(),
            },
            ExprKind::Index { target, index } => ExprKind::Index {
                target: self.expand_relation_subtree(target),
                index: self.expand_relation_subtree(index),
            },
            ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
                target: self.expand_relation_subtree(target),
                hi: self.expand_relation_subtree(hi),
                lo: self.expand_relation_subtree(lo),
            },
            ExprKind::Call { callee, args } => ExprKind::Call {
                callee: self.expand_relation_subtree(callee),
                args: args
                    .iter()
                    .map(|a| match a {
                        CallArg::Expr(e) => CallArg::Expr(self.expand_relation_subtree(e)),
                        CallArg::Named { name, value } => CallArg::Named {
                            name: name.clone(),
                            value: self.expand_relation_subtree(value),
                        },
                    })
                    .collect(),
            },
            ExprKind::Cast { expr, ty } => ExprKind::Cast {
                expr: self.expand_relation_subtree(expr),
                ty: ty.clone(),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op: *op,
                expr: self.expand_relation_subtree(expr),
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: *op,
                lhs: self.expand_relation_subtree(lhs),
                rhs: self.expand_relation_subtree(rhs),
            },
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => ExprKind::Ternary {
                cond: self.expand_relation_subtree(cond),
                then_branch: self.expand_relation_subtree(then_branch),
                else_branch: self.expand_relation_subtree(else_branch),
            },
            ExprKind::Paren(inner) => ExprKind::Paren(self.expand_relation_subtree(inner)),
            ExprKind::Membership { expr, set } => ExprKind::Membership {
                expr: self.expand_relation_subtree(expr),
                set: self.expand_relation_subtree(set),
            },
            ExprKind::SetLit(items) => ExprKind::SetLit(
                items
                    .iter()
                    .map(|x| self.expand_relation_subtree(x))
                    .collect(),
            ),
            ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
                lo: lo.as_ref().map(|x| self.expand_relation_subtree(x)),
                hi: hi.as_ref().map(|x| self.expand_relation_subtree(x)),
            },
            other => other.clone(),
        };
        Expr::new(new_kind, span)
    }

    /// Emit an inline Z3 solver block for `randomize(t) with { ... }`.
    /// Each call builds a fresh Z3 context, declares one bitvector variable
    /// per field at its declared width, translates the constraint
    /// expressions, sets `random_seed` from the PRNG so `--seed` flows
    /// through, then assigns the satisfying model back into `t`. UNSAT
    /// raises a FAIL log line and increments errors.
    fn emit_constraint_solver_block(
        &mut self,
        ty: &str,
        target: &Expr,
        with_body: &[Expr],
        depth: usize,
    ) {
        // Expand relation calls into their bodies up-front so the
        // rest of the function sees a flat list of constraints. Both
        // the user's `with` body AND the transaction's keeps reach
        // this function via the merge in StmtKind::Randomize.
        let with_body_owned: Vec<Expr> = self.expand_relation_calls(with_body);
        let with_body: &[Expr] = &with_body_owned;
        let fields = match self.txn_fields.get(ty).cloned() {
            Some(f) => f,
            None => {
                self.errors
                    .push(format!("internal: no field info for transaction `{ty}`"));
                return;
            }
        };
        // Tracking for the constraint translator: field names plus their
        // signedness/domain metadata. Z3 vars remain 64-bit in this migration
        // step, but operator selection and domain constraints now honor the
        // source field type.
        let field_info: std::collections::HashMap<String, TxnFieldInfo> =
            fields.iter().map(|f| (f.name.clone(), f.clone())).collect();
        let mut dist_directives: std::collections::HashMap<String, Vec<DistEntry>> =
            std::collections::HashMap::new();
        let mut hard_constraints: Vec<&Expr> = Vec::new();
        for c in with_body {
            if let ExprKind::DistDirective { target, entries } = &*c.kind {
                if let Some(field) = randomize_target_field_name(target, &field_info) {
                    dist_directives.insert(field, entries.clone());
                    continue;
                }
            }
            hard_constraints.push(c);
        }

        self.pad(depth);
        writeln!(self.out, "{{   // randomize(t) with — Z3 solver block").ok();

        // Context + solver. We use `z3::solver` (not `optimize`) so UNSAT
        // is reported faithfully — `optimize` can return a "best partial"
        // model when soft+hard constraints conflict, hiding real UNSAT.
        // Diversity comes from the per-call-site blocking-clause cache
        // built up across iterations (see below).
        self.pad(depth + 1);
        writeln!(self.out, "z3::context _ctx;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "z3::solver _s(_ctx);").ok();
        self.pad(depth + 1);
        writeln!(self.out, "z3::params _p(_ctx);").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "_p.set(\"random_seed\", static_cast<unsigned>(harc_rng_next() & 0x7fffffffU));"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "_s.set(_p);").ok();

        // All Z3 vars declared at 64 bits so binops with literals and across
        // fields don't trip the width-compatibility check. Each field then
        // gets a range constraint enforcing its declared width — uniform
        // emission, no zext bookkeeping.
        for f in &fields {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "z3::expr _z_{} = _ctx.bv_const(\"{}\", 64);",
                f.name, f.name
            )
            .ok();
            if let Some(n) = f.enum_variants {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ule(_z_{}, _ctx.bv_val((uint64_t){}, 64)));",
                    f.name,
                    n.saturating_sub(1)
                )
                .ok();
            } else if f.signed && f.width < 64 {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(_z_{} >= _ctx.bv_val((uint64_t)(-(1LL << {})), 64));",
                    f.name,
                    f.width.saturating_sub(1)
                )
                .ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(_z_{} <= _ctx.bv_val((uint64_t)((1LL << {}) - 1), 64));",
                    f.name,
                    f.width.saturating_sub(1)
                )
                .ok();
            } else if f.width < 64 {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ult(_z_{}, _ctx.bv_val((uint64_t)1ULL << {}, 64)));",
                    f.name, f.width
                )
                .ok();
            }
            if f.non_random {
                self.pad(depth + 1);
                write!(self.out, "_s.add(_z_{} == _ctx.bv_val((uint64_t)(", f.name).ok();
                self.emit_expr(target);
                writeln!(self.out, ".{}), 64));", f.name).ok();
            }
            if let Some((lo, hi)) = field_attr_range(f) {
                self.emit_solver_range_constraint(f, lo, hi, &field_info, depth + 1);
            }
        }

        // Translated constraints.
        for c in &hard_constraints {
            self.pad(depth + 1);
            write!(self.out, "_s.add(").ok();
            self.emit_constraint_expr(c, &field_info);
            writeln!(self.out, ");").ok();
        }

        // Detect fields the user has equality-pinned (e.g. `t.addr == 24`).
        // Those fields have only one satisfying value, so adding a blocking
        // clause for them makes the whole problem UNSAT after the first
        // call. We block only the *free* fields. Diversity then comes from
        // the free-field cache.
        let pinned: std::collections::HashSet<String> = hard_constraints
            .iter()
            .copied()
            .filter_map(|e| {
                if let ExprKind::Binary {
                    op: BinaryOp::Eq,
                    lhs,
                    rhs,
                } = &*e.kind
                {
                    let pin_from = |side: &Expr, other: &Expr| -> Option<String> {
                        if let ExprKind::Field { target, name } = &*side.kind {
                            if matches!(&*target.kind, ExprKind::Ident(_)) {
                                if matches!(&*other.kind, ExprKind::Int(_)) {
                                    return Some(name.name.clone());
                                }
                            }
                        }
                        None
                    };
                    pin_from(lhs, rhs).or_else(|| pin_from(rhs, lhs))
                } else {
                    None
                }
            })
            .collect();

        let cache_tag = format!("_div_cache_{}", target.span.start);
        let free_fields: Vec<&TxnFieldInfo> = fields
            .iter()
            .filter(|f| !f.non_random && !pinned.contains(&f.name))
            .collect();

        // One static history vector per free field — persists across loop
        // iterations to push the solver away from previously-seen answers.
        for f in &free_fields {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static std::vector<uint64_t> {cache_tag}_{};",
                f.name
            )
            .ok();
        }

        // Seeded preference values. These are hard clauses only for the first
        // check; if the preferred tuple is incompatible with the user's
        // constraints, we drop the preference stack and fall back to the
        // diversity-only/base solve. This makes solver-backed randomize
        // consume the HARC seed without turning preferences into false UNSATs.
        for f in &free_fields {
            self.pad(depth + 1);
            let dist_entries = dist_directives
                .get(&f.name)
                .map(Vec::as_slice)
                .or_else(|| field_attr_dist_entries(f).map(Vec::as_slice));
            if let Some(entries) = dist_entries {
                write!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = (uint64_t)harc_rng_dist({{",
                    f.name
                )
                .ok();
                self.emit_rng_dist_entries(entries);
                writeln!(self.out, "}});").ok();
            } else if let Some(n) = f.enum_variants {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = (uint64_t)harc_rng_range(0, {});",
                    f.name,
                    n.saturating_sub(1)
                )
                .ok();
            } else if f.signed && f.width > 0 && f.width < 63 {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = (uint64_t)harc_rng_range(-(1LL << {}), (1LL << {}) - 1);",
                    f.name,
                    f.width.saturating_sub(1),
                    f.width.saturating_sub(1)
                )
                .ok();
            } else if f.width < 64 {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = harc_rng_uint({});",
                    f.name, f.width
                )
                .ok();
            } else {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = harc_rng_next();",
                    f.name
                )
                .ok();
            }
        }
        self.pad(depth + 1);
        writeln!(
            self.out,
            "_s.push();   // seeded preference + diversity clauses (free fields only)"
        )
        .ok();
        for f in &free_fields {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "_s.add(_z_{} == _ctx.bv_val(_pref_{cache_tag}_{}, 64));",
                f.name, f.name
            )
            .ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "auto _r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(self.out, "if (_r != z3::sat) {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "_s.pop();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "_s.push();   // retry without seeded preferences").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        for f in &free_fields {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "if ({cache_tag}_{}.size() > 32) {cache_tag}_{}.clear();",
                f.name, f.name
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (auto _v : {cache_tag}_{}) _s.add(_z_{} != _ctx.bv_val(_v, 64));",
                f.name, f.name
            )
            .ok();
        }
        // First check: with blocking. If UNSAT (cache has saturated the
        // satisfiable space), drop the blocks and clear the cache.
        self.pad(depth + 1);
        writeln!(self.out, "_r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(self.out, "if (_r != z3::sat) {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "_s.pop();").ok();
        for f in &free_fields {
            self.pad(depth + 2);
            writeln!(self.out, "{cache_tag}_{}.clear();", f.name).ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "_r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        // Assign model values back into t.
        self.pad(depth + 1);
        writeln!(self.out, "if (_r == z3::sat) {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "z3::model _m = _s.get_model();").ok();
        for f in &fields {
            if f.non_random {
                continue;
            }
            // Every declared field is assigned from the model — equality-
            // pinned fields take their constrained value; free fields take
            // a Z3-chosen satisfying value. Only free fields get pushed
            // into the diversity cache.
            let val_ty = if f.signed { "int64_t" } else { "uint64_t" };
            self.pad(depth + 2);
            writeln!(
                self.out,
                "{} _val_{} = ({})_m.eval(_z_{}).get_numeral_uint64();",
                val_ty, f.name, val_ty, f.name
            )
            .ok();
            self.pad(depth + 2);
            write!(self.out, "").ok();
            self.emit_expr(target);
            writeln!(self.out, ".{} = _val_{};", f.name, f.name).ok();
            if !pinned.contains(&f.name) {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "{cache_tag}_{}.push_back(_val_{});",
                    f.name, f.name
                )
                .ok();
            }
        }
        self.emit_randomize_trace_event(ty, target, depth + 2);
        self.pad(depth + 1);
        writeln!(self.out, "}} else {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "sim_log_line(\"FAIL\", \"randomize(t) with: constraint UNSAT\");"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "errors++;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    fn constraint_expr_is_signed(
        &self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    ) -> bool {
        match &*e.kind {
            ExprKind::Ident(id) => field_info.get(&id.name).map(|f| f.signed).unwrap_or(false),
            ExprKind::Field { target, name } => {
                matches!(&*target.kind, ExprKind::Ident(_))
                    && field_info
                        .get(&name.name)
                        .map(|f| f.signed)
                        .unwrap_or(false)
            }
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
                self.constraint_expr_is_signed(inner, field_info)
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.constraint_expr_is_signed(lhs, field_info)
                    || self.constraint_expr_is_signed(rhs, field_info)
            }
            ExprKind::Membership { expr, .. } => self.constraint_expr_is_signed(expr, field_info),
            _ => false,
        }
    }

    /// Translate a HARC expression to a z3++ C++ expression. Field accesses
    /// `t.<name>` resolve to the per-field `_z_<name>` Z3 var declared in
    /// the surrounding solver block. Integer literals become `_ctx.bv_val(N, W)`
    /// at the field's width inferred from context. v0 is permissive — any
    /// untranslatable form falls back to a comment + `_ctx.bool_val(true)`.
    fn emit_constraint_expr(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    ) {
        // All Z3 vars are 64-bit; literals match.
        self.emit_constraint_expr_w(e, field_info, 64);
    }

    fn emit_constraint_expr_w(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
    ) {
        match &*e.kind {
            // `t.<name>` → _z_<name>. Strip the `t.` prefix.
            ExprKind::Field { target, name } => {
                if matches!(&*target.kind, ExprKind::Ident(_))
                    && field_info.contains_key(&name.name)
                {
                    write!(self.out, "_z_{}", name.name).ok();
                } else {
                    self.errors.push(format!(
                        "constraint references unknown field `{}` (only `t.<field>` of the randomize target is supported)",
                        name.name
                    ));
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
            }
            ExprKind::Ident(id) => {
                // Bare ident: try fields first, then enum variants.
                // Lets a constraint like `keep op != WRAP` work — the
                // RHS isn't a field, it's the enum variant `WRAP`.
                if field_info.contains_key(&id.name) {
                    write!(self.out, "_z_{}", id.name).ok();
                } else if let Some(idx) = self.enum_variants.get(&id.name).copied() {
                    write!(self.out, "_ctx.bv_val((uint64_t){}, {})", idx, width).ok();
                } else {
                    self.errors
                        .push(format!("constraint references unknown name `{}`", id.name));
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
            }
            ExprKind::Int(s) => {
                write!(
                    self.out,
                    "_ctx.bv_val((uint64_t){}, {})",
                    c_int_literal(s),
                    width
                )
                .ok();
            }
            ExprKind::Bool(b) => {
                write!(
                    self.out,
                    "_ctx.bool_val({})",
                    if *b { "true" } else { "false" }
                )
                .ok();
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_constraint_expr_w(inner, field_info, width);
                write!(self.out, ")").ok();
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not | UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                write!(self.out, "{s}").ok();
                self.emit_constraint_expr_w(expr, field_info, width);
            }
            // `e in <range-or-set>` parses as ExprKind::Membership
            // (not Binary(In, …) — the `in` keyword is lexed as
            // TokenKind::In and lowered structurally). Expand to an
            // OR-chain of equality / range-comparison sub-expressions
            // the solver handles natively.
            ExprKind::Membership { expr, set } => {
                self.emit_constraint_membership(expr, set, field_info, width);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                use BinaryOp::*;
                // Defensive: BinaryOp::In/Inside isn't produced by the
                // current parser (Membership is used instead) but the
                // op variants exist, so keep the handler in case
                // future parser paths emit them.
                if matches!(op, In | Inside) {
                    self.emit_constraint_membership(lhs, rhs, field_info, width);
                    return;
                }
                // Z3's operator overloads on `z3::expr` default to
                // *signed* semantics for `<`, `>`, `<=`, `>=`, `/`,
                // `%`. HARC fields are unsigned (uint<N> / bits<N>),
                // so we explicitly route those through the
                // unsigned-named functions: ult/ule/ugt/uge for
                // comparisons, udiv for division, urem for modulus.
                // C++ has no `%%` infix — the old `" %% "` mapping was
                // a half-finished placeholder. Equality (`==`, `!=`),
                // logical / bitwise / shift use the natural overloads.
                let signed = self.constraint_expr_is_signed(lhs, field_info)
                    || self.constraint_expr_is_signed(rhs, field_info);
                let (sep, fname) = match op {
                    Add => (" + ", None),
                    Sub => (" - ", None),
                    Mul => (" * ", None),
                    Div if signed => (" / ", None),
                    Div => ("", Some("udiv")),
                    Mod if signed => (" % ", None),
                    Mod => ("", Some("urem")),
                    Eq => (" == ", None),
                    Ne => (" != ", None),
                    Lt if signed => (" < ", None),
                    Lt => ("", Some("ult")),
                    Le if signed => (" <= ", None),
                    Le => ("", Some("ule")),
                    Gt if signed => (" > ", None),
                    Gt => ("", Some("ugt")),
                    Ge if signed => (" >= ", None),
                    Ge => ("", Some("uge")),
                    AndAnd | AndKw => (" && ", None),
                    OrOr | OrKw => (" || ", None),
                    BitAnd => (" & ", None),
                    BitOr => (" | ", None),
                    BitXor => (" ^ ", None),
                    Shl => (" << ", None),
                    Shr => (" >> ", None),
                    _ => {
                        self.errors.push(format!(
                            "constraint operator `{:?}` not supported in v0 solver path",
                            op
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                        return;
                    }
                };
                if let Some(fn_name) = fname {
                    // Unsigned op via z3::<fn>(lhs, rhs).
                    write!(self.out, "z3::{fn_name}(").ok();
                    self.emit_constraint_expr_w(lhs, field_info, width);
                    write!(self.out, ", ").ok();
                    self.emit_constraint_expr_w(rhs, field_info, width);
                    write!(self.out, ")").ok();
                } else {
                    self.emit_constraint_expr_w(lhs, field_info, width);
                    write!(self.out, "{sep}").ok();
                    self.emit_constraint_expr_w(rhs, field_info, width);
                }
            }
            _ => {
                self.errors.push(format!(
                    "constraint expression not supported in v0 solver path"
                ));
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
        }
    }

    /// Translate `<lhs> in <rhs>` (and `inside`) for the Z3 constraint
    /// path. Rhs shapes:
    ///   * `[lo..hi]` (RangeLit) → `lhs >= lo && lhs <= hi`
    ///   * `{a, b, c}` (SetLit)  → `lhs == a || lhs == b || lhs == c`
    ///   * Anything else         → falls back to `lhs == rhs`
    ///                              (treats rhs as a singleton).
    ///
    /// Open and unbounded sides on RangeLit (`[..hi]`, `[lo..]`)
    /// collapse the missing comparison. Set elements that are
    /// themselves RangeLits recurse (so `{[0..3], 7}` expands to
    /// `(_z>=0 && _z<=3) || _z==7`).
    fn emit_constraint_membership(
        &mut self,
        lhs: &Expr,
        rhs: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
    ) {
        match &*rhs.kind {
            ExprKind::RangeLit { lo, hi } => {
                write!(self.out, "(").ok();
                let mut has_any = false;
                let signed = self.constraint_expr_is_signed(lhs, field_info);
                if let Some(l) = lo {
                    if signed {
                        self.emit_constraint_expr_w(lhs, field_info, width);
                        write!(self.out, " >= ").ok();
                        self.emit_constraint_expr_w(l, field_info, width);
                    } else {
                        write!(self.out, "z3::uge(").ok();
                        self.emit_constraint_expr_w(lhs, field_info, width);
                        write!(self.out, ", ").ok();
                        self.emit_constraint_expr_w(l, field_info, width);
                        write!(self.out, ")").ok();
                    }
                    has_any = true;
                }
                if let Some(h) = hi {
                    if has_any {
                        write!(self.out, " && ").ok();
                    }
                    if signed {
                        self.emit_constraint_expr_w(lhs, field_info, width);
                        write!(self.out, " <= ").ok();
                        self.emit_constraint_expr_w(h, field_info, width);
                    } else {
                        write!(self.out, "z3::ule(").ok();
                        self.emit_constraint_expr_w(lhs, field_info, width);
                        write!(self.out, ", ").ok();
                        self.emit_constraint_expr_w(h, field_info, width);
                        write!(self.out, ")").ok();
                    }
                    has_any = true;
                }
                if !has_any {
                    // Open-ended on both sides — vacuously true.
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
                write!(self.out, ")").ok();
            }
            ExprKind::SetLit(items) => {
                write!(self.out, "(").ok();
                if items.is_empty() {
                    write!(self.out, "_ctx.bool_val(false)").ok();
                } else {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            write!(self.out, " || ").ok();
                        }
                        // Recurse so `{[0..3], 7}` expands correctly.
                        match &*it.kind {
                            ExprKind::RangeLit { .. } | ExprKind::SetLit(_) => {
                                self.emit_constraint_membership(lhs, it, field_info, width);
                            }
                            ExprKind::Paren(inner) => {
                                self.emit_constraint_membership(lhs, inner, field_info, width);
                            }
                            _ => {
                                // Singleton element — equality test.
                                self.emit_constraint_expr_w(lhs, field_info, width);
                                write!(self.out, " == ").ok();
                                self.emit_constraint_expr_w(it, field_info, width);
                            }
                        }
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => {
                self.emit_constraint_membership(lhs, inner, field_info, width);
            }
            _ => {
                // Fallback: treat rhs as a singleton — `a in b` becomes
                // `a == b`. Same shape `emit_bin_membership` uses.
                self.emit_constraint_expr_w(lhs, field_info, width);
                write!(self.out, " == ").ok();
                self.emit_constraint_expr_w(rhs, field_info, width);
            }
        }
    }

    /// Emit a `log` or `logf` call. When `file_path` is `Some`, lower to
    /// `sim_logf_line(get_log_file(path), sev, fmt, args)`; otherwise lower
    /// to `sim_log_line(sev, fmt, args)`. Severity / message extraction
    /// matches `log()`'s rules (first ident is severity; first string is
    /// the message).
    fn emit_log(&mut self, args: &[CallArg], file_path: Option<String>, depth: usize) {
        let sev = args
            .iter()
            .find_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::Ident(id) => Some(id.name.to_uppercase()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| "INFO".to_string());
        let msg = args
            .iter()
            .find_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::String(s) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| "".to_string());
        let (fmt, caps) = process_interp(&msg);
        self.pad(depth);
        match file_path {
            Some(p) => {
                write!(
                    self.out,
                    "sim_logf_line(get_log_file(\"{}\"), \"{}\", \"{}\"",
                    escape_c(&p),
                    sev,
                    escape_c(&fmt)
                )
                .ok();
            }
            None => {
                write!(self.out, "sim_log_line(\"{}\", \"{}\"", sev, escape_c(&fmt)).ok();
            }
        }
        for c in &caps {
            self.emit_interp_arg(c);
        }
        writeln!(self.out, ");").ok();

        // Spec §7.7 test-result semantics:
        //   `log(error, ...)` increments the failure counter; the test
        //                     fails at end of run if any error logged.
        //   `log(fatal, ...)` increments + sets `_fatal` so the main
        //                     simulation loop exits at end of the
        //                     current cycle (this test instance aborts;
        //                     others in a regression continue).
        // `warn` / `info` / `debug` have no test-result effect.
        match sev.as_str() {
            "ERROR" => {
                self.pad(depth);
                writeln!(self.out, "errors++;").ok();
            }
            "FATAL" => {
                self.pad(depth);
                writeln!(self.out, "errors++; _fatal = true;").ok();
            }
            _ => {}
        }
    }

    /// Emit one captured `${expr}` value as a printf argument. Routes the
    /// captured source text through the parser so `dut.x` (a member access
    /// on a pointer) gets emitted as `dut->x` via the normal field-rewrite
    /// machinery, rather than the literal source text. Falls back to raw
    /// text if the fragment doesn't parse — preserves whatever the user
    /// wrote so they get a meaningful C++ error rather than a hidden one.
    fn emit_interp_arg(&mut self, cap: &InterpCap) {
        write!(self.out, ", ").ok();
        match cap.wide_hex {
            Some((width, upper)) => {
                // Wide-hex: route through HarcHexBuf128 so the full
                // 128-bit value prints (printf with `%s`). The
                // constructor takes `_harc_u128`; pointer-rooted
                // Field accesses already wrap with `harc_read` in
                // emit_expr (returning `_harc_u128`), and bare local
                // ints widen via implicit conversion. No extra
                // wrap needed here.
                let upper_str = if upper { "true" } else { "false" };
                write!(self.out, "(const char*)harc_rt::HarcHexBuf128(").ok();
                match crate::parser::parse_expr_fragment(&cap.expr) {
                    Ok(e) => self.emit_expr(&e),
                    Err(_) => {
                        write!(self.out, "{}", cap.expr).ok();
                    }
                }
                write!(self.out, ", {width}, {upper_str})").ok();
            }
            None => {
                // Narrow / decimal path: cast to long long for printf.
                write!(self.out, "(long long)(").ok();
                match crate::parser::parse_expr_fragment(&cap.expr) {
                    Ok(e) => self.emit_expr(&e),
                    Err(_) => {
                        write!(self.out, "{}", cap.expr).ok();
                    }
                }
                write!(self.out, ")").ok();
            }
        }
    }

    /// Emit a top-level `function` as a `[&]`-captured lambda. Named-typed
    /// parameters are recognised as DUT pointers (`VName*`); their field
    /// access in the body uses `->`. Capture-all (`[&]`) so the body can
    /// still see `tick` and any test-level state.
    /// Emit a `tseq` declaration as a `[&]`-capturing lambda that returns
    /// a `std::vector<T>` filled in by `yield` statements. The inner type
    /// `T` is the argument of `TSeq<T>` in the return-type slot.
    fn emit_tseq(&mut self, t: &TseqDecl, depth: usize) {
        // Inner type: pull T out of `TSeq<T>`. Default to `int64_t` if
        // the user wrote a bare `tseq`-as-block without a return clause.
        let inner = t
            .return_ty
            .as_ref()
            .and_then(tseq_inner_type)
            .unwrap_or_else(|| "int64_t".to_string());
        self.pad(depth);
        write!(self.out, "auto {} = [&](", t.name.name).ok();
        let mut added: Vec<String> = Vec::new();
        let param_names = cpp_param_names(&t.params);
        for (i, p) in t.params.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", param_names[i]).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) && p.name.name != "_" {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        writeln!(self.out, ") -> std::vector<{inner}> {{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "std::vector<{inner}> _result;").ok();
        let prev = self.current_yield_target.replace("_result".to_string());
        self.emit_block(&t.body, depth + 1);
        self.current_yield_target = prev;
        self.pad(depth + 1);
        writeln!(self.out, "return _result;").ok();
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    fn emit_function(&mut self, f: &FunctionDecl, depth: usize) {
        self.pad(depth);
        let ret = f
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or("void".to_string());
        write!(self.out, "auto {} = [&](", f.name.name).ok();
        // Track which params are Named-typed (pointer-shaped). Add to
        // `pointer_vars` while emitting the body, then remove on exit so
        // siblings don't leak each other's params.
        let mut added: Vec<String> = Vec::new();
        let param_names = cpp_param_names(&f.params);
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", param_names[i]).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) && p.name.name != "_" {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        writeln!(self.out, ") -> {ret} {{").ok();
        self.emit_block(&f.body, depth + 1);
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    fn emit_block(&mut self, b: &Block, depth: usize) {
        for s in &b.stmts {
            self.emit_stmt(s, depth);
        }
    }

    fn emit_stmt(&mut self, s: &Stmt, depth: usize) {
        match &s.kind {
            StmtKind::Let(l) => self.emit_let(l, depth),
            StmtKind::Send { target, value } => {
                self.pad(depth);
                self.emit_signal_assignment(target, value, depth);
            }
            StmtKind::Assign { target, value } => {
                self.pad(depth);
                self.emit_signal_assignment(target, value, depth);
            }
            StmtKind::For(f) => {
                self.pad(depth);
                let var = &f.var.name;
                if let ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } = &*f.iter.kind
                {
                    // `for i in lo .. hi` — emit as an indexed C++ for loop.
                    write!(self.out, "for (int64_t {var} = ").ok();
                    self.emit_expr(lo);
                    write!(self.out, "; {var} < ").ok();
                    self.emit_expr(hi);
                    writeln!(self.out, "; {var}++) {{").ok();
                    self.emit_block(&f.body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                } else {
                    // `for x in <seq-expression>` — assume the rhs evaluates
                    // to something C++ can range-iterate over (e.g. a
                    // `std::vector<T>` returned by a `tseq`). The bound
                    // variable is `auto&` so transactions don't get copied
                    // on each iteration.
                    write!(self.out, "for (auto& {var} : ").ok();
                    self.emit_expr(&f.iter);
                    writeln!(self.out, ") {{").ok();
                    self.emit_block(&f.body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                }
            }
            StmtKind::Repeat(r) => {
                self.pad(depth);
                write!(self.out, "for (int64_t _r = 0; _r < ").ok();
                self.emit_expr(&r.count);
                writeln!(self.out, "; _r++) {{").ok();
                self.emit_block(&r.body, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Loop(b) => {
                self.pad(depth);
                writeln!(self.out, "while (true) {{").ok();
                self.emit_block(b, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::While { cond, body, .. } => {
                self.pad(depth);
                write!(self.out, "while (").ok();
                self.emit_expr(cond);
                writeln!(self.out, ") {{").ok();
                self.emit_block(body, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Break { .. } => {
                self.pad(depth);
                writeln!(self.out, "break;").ok();
            }
            StmtKind::Continue { .. } => {
                self.pad(depth);
                writeln!(self.out, "continue;").ok();
            }
            StmtKind::If(i) => {
                self.pad(depth);
                write!(self.out, "if (").ok();
                self.emit_expr(&i.cond);
                writeln!(self.out, ") {{").ok();
                self.emit_block(&i.then_block, depth + 1);
                for (c, b) in &i.elsifs {
                    self.pad(depth);
                    write!(self.out, "}} else if (").ok();
                    self.emit_expr(c);
                    writeln!(self.out, ") {{").ok();
                    self.emit_block(b, depth + 1);
                }
                if let Some(eb) = &i.else_block {
                    self.pad(depth);
                    writeln!(self.out, "}} else {{").ok();
                    self.emit_block(eb, depth + 1);
                }
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::After { duration, body, .. } => {
                self.pad(depth);
                if self.in_coroutine {
                    // Coroutine path: yield to the scheduler so other
                    // coroutines can advance during the wait.
                    write!(self.out, "co_await harc_rt::wait_cycles(_slot, (uint32_t)(").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "));").ok();
                } else {
                    write!(self.out, "for (int _ck = 0; _ck < ").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "; _ck++) tick();").ok();
                }
                if !body.stmts.is_empty() {
                    self.pad(depth);
                    writeln!(self.out, "{{").ok();
                    self.emit_block(body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                }
            }
            StmtKind::Wait {
                duration, clock, ..
            } => {
                self.pad(depth);
                // `wait N cycles on <clock>` — advance simulated time
                // until the named clock has seen N more rising edges.
                // Other clocks continue ticking at their natural rate.
                // Useful for cycle-relative reasoning in multi-clock
                // tests (e.g. "after 2 dst_clk cycles, X should hold").
                if let Some(c) = clock {
                    let idx = match self.clock_names.iter().position(|n| n == &c.name) {
                        Some(i) => i,
                        None => {
                            self.errors.push(format!(
                                "wait ... on {}: no clock named `{}` declared in this test",
                                c.name, c.name
                            ));
                            return;
                        }
                    };
                    // `wait N cycles on <named-clock>` always lowers to
                    // an inline `eval_clocks_until` loop, regardless of
                    // coroutine context. The main loop's
                    // `eval_clocks_until(next_primary_posedge)` advances
                    // by a FULL primary period per iteration, which is
                    // too coarse when the named clock runs faster than
                    // the primary (e.g. `wait 1 cycle on dst_clk` where
                    // dst is 2× src would skip past the target). The
                    // sync path advances to whichever clock's next edge
                    // is sooner, preserving sub-primary-cycle precision.
                    //
                    // Phase 1 is single-actor anyway (only the run
                    // coroutine exists), so not yielding to the
                    // scheduler during this wait is harmless. When
                    // multi-actor lands and named-clock waits need to
                    // cooperate with other coroutines, the main loop
                    // gets reworked to advance by next-edge-of-any-clock
                    // and the coroutine path resumes.
                    write!(
                        self.out,
                        "{{ long long _target = clocks_[{idx}].rising_count + (long long)("
                    )
                    .ok();
                    self.emit_expr(duration);
                    writeln!(
                        self.out,
                        "); while (clocks_[{idx}].rising_count < _target) {{"
                    )
                    .ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "long long _next = clocks_[0].next_edge_ps;").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "for (auto& _ck : clocks_) if (_ck.next_edge_ps < _next) _next = _ck.next_edge_ps;").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "eval_clocks_until(_next);").ok();
                    self.pad(depth);
                    writeln!(self.out, "}} for (auto& _c : _checkers) _c(); }}").ok();
                    return;
                }
                // Wall-clock duration (e.g. `wait 100ns`) advances absolute
                // time when multi-clock; under single-clock backward compat
                // we don't have absolute time, so fall back to a comment +
                // a single tick (rare path; the documented form for single
                // clock is `wait N cycles`).
                if let ExprKind::Time(s) = &*duration.kind {
                    match time_literal_to_ps(s) {
                        Ok(ps) => {
                            writeln!(self.out, "eval_clocks_until(now_ps + {ps});").ok();
                        }
                        Err(e) => self.errors.push(e),
                    }
                } else if self.in_coroutine {
                    write!(self.out, "co_await harc_rt::wait_cycles(_slot, (uint32_t)(").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "));").ok();
                } else {
                    write!(self.out, "for (int _w = 0; _w < ").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "; _w++) tick();").ok();
                }
            }
            StmtKind::WaitUntil {
                mode,
                conditions,
                timeout,
                ..
            } => {
                self.emit_wait_until(mode, conditions, timeout.as_ref(), depth);
            }
            StmtKind::Assert(v) => {
                // Spec §2 LL(1) table: bare IDENT → named property
                // reference (concurrent); expression with temporal ops →
                // concurrent inline; plain bool expression → immediate
                // point-in-time check. The legacy `property` keyword still
                // parses (back-compat) but is no longer required and no
                // longer changes dispatch.
                if let Some(expr) = &v.expr {
                    if is_concurrent_assertion(expr, &self.properties) {
                        self.emit_property_check("FAIL", v, depth);
                    } else {
                        self.emit_inline_assert(v, depth);
                    }
                }
            }
            StmtKind::Fail { msg, .. } => {
                // Standalone `fail("...")` — unconditional failure log
                // + error counter bump. Same emission as the failure
                // arm of an inline assert, just without the surrounding
                // `if (!cond)` guard.
                let raw = match &*msg.kind {
                    ExprKind::String(s) => s.clone(),
                    _ => "fail() with non-string arg".to_string(),
                };
                let (fmt, caps) = process_interp(&raw);
                self.pad(depth);
                write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
                for c in &caps {
                    self.emit_interp_arg(c);
                }
                writeln!(self.out, ");").ok();
                self.pad(depth);
                writeln!(self.out, "errors++;").ok();
            }
            StmtKind::Assume(v) => {
                if let Some(expr) = &v.expr {
                    if is_concurrent_assertion(expr, &self.properties) {
                        self.emit_property_check("ASSUME-FAIL", v, depth);
                    } else {
                        self.emit_inline_assume(v, depth);
                    }
                }
            }
            StmtKind::Cover(v) => {
                // Concurrent cover (spec §5): a witness counter — every
                // primary-clock edge where the expression is true bumps
                // the hit count. End-of-test report aggregates all
                // registered covers (similar to a covergroup, but flat).
                // `cover NAME` resolves a declared property; otherwise
                // the inline expression is used directly.
                let expr = match &v.expr {
                    Some(e) => e,
                    None => return,
                };
                let (label, body) = if let ExprKind::Ident(id) = &*expr.kind {
                    if let Some(b) = self.properties.get(&id.name).cloned() {
                        (id.name.clone(), b)
                    } else {
                        (id.name.clone(), expr.clone())
                    }
                } else {
                    (
                        format!("cov_{}_{}", expr.span.start, expr.span.end),
                        expr.clone(),
                    )
                };
                let tag = format!("c_{}_{}", expr.span.start, expr.span.end);
                self.covers.push(CoverInfo {
                    tag: tag.clone(),
                    label,
                });
                self.pad(depth);
                writeln!(self.out, "static uint64_t _cov_{tag}_hits = 0;").ok();
                self.pad(depth);
                writeln!(self.out, "_checkers.push_back([&]() {{").ok();
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                // Note: temporal ops (|->, |=>, past/rose/etc.) inside a
                // cover body aren't translated for v0 — the same
                // limitation as inline-temporal asserts before the
                // property-decl path was added. Use a `property` decl
                // and `cover <name>` for temporal patterns, or stick to
                // same-cycle bool expressions inline.
                self.emit_expr(&body);
                writeln!(self.out, ")) _cov_{tag}_hits++;").ok();
                self.pad(depth);
                writeln!(self.out, "}});").ok();
            }
            StmtKind::Log { args, .. } => self.emit_log(args, None, depth),
            StmtKind::LogF { args, .. } => {
                // First positional string literal is the file path; the
                // remaining args follow `log` semantics (severity ident +
                // message string).
                let (path, rest): (Option<String>, Vec<&CallArg>) = {
                    let mut path = None;
                    let mut rest = Vec::new();
                    for a in args {
                        if path.is_none() {
                            if let CallArg::Expr(e) = a {
                                if let ExprKind::String(s) = &*e.kind {
                                    path = Some(s.clone());
                                    continue;
                                }
                            }
                        }
                        rest.push(a);
                    }
                    (path, rest)
                };
                if path.is_none() {
                    self.errors
                        .push("logf requires a string-literal file path as the first arg".into());
                    return;
                }
                let collected: Vec<CallArg> = rest.into_iter().cloned().collect();
                self.emit_log(&collected, path, depth);
            }
            StmtKind::Expr(e) => {
                // `bus.<channel>.send(args)` and `bus.<channel>.recv()`
                // expand into a multi-statement v/r handshake. When
                // it's a discarded-result `recv()`, just run the dance.
                if self.try_emit_bus_handshake(e, None, depth) {
                    return;
                }
                // `bitbash(regs)` — RFC §7.7 compile-time-unrolled
                // walk-all over each RW register in the regblock.
                // Emits write(all-ones) + read + assert + write(0) +
                // read + assert for every RW register. RO/WO are
                // skipped with a comment. See docs/ral-support.md.
                if self.try_emit_bitbash(e, depth) {
                    return;
                }
                self.pad(depth);
                self.emit_expr(e);
                writeln!(self.out, ";").ok();
            }
            StmtKind::Release(e) => {
                // `release <expr>` — disable the active SV procedural
                // force on a `probe force` signal. Lowers to a single
                // store: `<name>_en = 0`. Read-only probes (no `force`
                // modifier) error.
                if let Some(probe) = self.resolve_force_probe(e) {
                    self.pad(depth);
                    writeln!(self.out, "dut->rootp->{} = 0;", probe.enable()).ok();
                } else {
                    self.errors.push(
                        "`release` target must be `dut.<probe_name>` where the named probe was declared with `probe force`".into(),
                    );
                }
            }
            StmtKind::Return(opt) => {
                self.pad(depth);
                if let Some(e) = opt {
                    write!(self.out, "return ").ok();
                    self.emit_expr(e);
                    writeln!(self.out, ";").ok();
                } else {
                    writeln!(self.out, "return;").ok();
                }
            }
            StmtKind::Yield(e) => {
                self.pad(depth);
                if let Some(target) = self.current_yield_target.clone() {
                    write!(self.out, "{target}.push_back(").ok();
                    self.emit_expr(e);
                    writeln!(self.out, ");").ok();
                } else {
                    self.errors
                        .push("`yield` outside a `tseq` body is not supported in v0 cpp_tb".into());
                }
            }
            StmtKind::Emit { name, args, .. } => {
                // `emit e(v)` → call every subscriber.
                //   - Multi-segment path (`emit drv.req(t)`) → emit
                //     verbatim as `drv.req`.
                //   - Single segment (`emit write_seen(...)` inside a
                //     monitor body) → check field_subs to resolve the
                //     bare name to the instance-qualified field
                //     (`mon.write_seen`).
                let event_name = if name.segments.len() > 1 {
                    name.segments
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".")
                } else {
                    let raw = name
                        .segments
                        .last()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.field_subs.get(&raw).cloned().unwrap_or(raw)
                };
                let arg = args.first();
                self.pad(depth);
                write!(self.out, "for (auto& _s : {event_name}) _s(").ok();
                if let Some(a) = arg {
                    match a {
                        CallArg::Expr(e) => self.emit_expr(e),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                writeln!(self.out, ");").ok();
                // Activity tracking (spec §7.x): an `emit` counts as an
                // "out" for the surrounding component instance. Emits
                // outside a component (test-run / free function) don't
                // attribute to any instance.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
                }
            }
            StmtKind::On(h) => {
                // Three shapes:
                //   `on obj.method pre/post ... end on` — pre/post hook
                //       on a hookable method. Resolves obj to a known
                //       component-typed binding, then pushes the body
                //       closure into the global `<Type>_<method>_<side>`
                //       vector. The method's body wrap fires hooks
                //       around the body each call.
                //   `on event_name(arg) ... end on` — event subscription.
                //   `on <bool-expr> ... end on`     — cycle trigger.
                if let Some(side) = h.hook {
                    if let Some((comp_ty, method_name, params)) =
                        self.resolve_component_hookable(&h.event)
                    {
                        let side_str = match side {
                            HookSide::Pre => "pre",
                            HookSide::Post => "post",
                        };
                        let param_names = cpp_param_names(&params);
                        let arg_decls: Vec<String> = params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                let ty =
                                    p.ty.as_ref()
                                        .map(|t| self.c_type_for_param(t))
                                        .unwrap_or_else(|| "int64_t".to_string());
                                format!("{ty} {}", param_names[i])
                            })
                            .collect();
                        self.pad(depth);
                        writeln!(
                            self.out,
                            "{comp_ty}_{method_name}_{side_str}.push_back([&]({}) {{",
                            arg_decls.join(", "),
                        )
                        .ok();
                        self.emit_block(&h.body, depth + 1);
                        self.pad(depth);
                        writeln!(self.out, "}});").ok();
                        return;
                    } else {
                        self.errors.push(
                            "on <obj>.<method> pre/post: obj.method must resolve to a `hookable` on a known component type".into()
                        );
                        return;
                    }
                }
                if let ExprKind::Call { callee, args } = &*h.event.kind {
                    // Event-subscription path.
                    let raw = match &*callee.kind {
                        ExprKind::Ident(id) => id.name.clone(),
                        ExprKind::Field { target: _, name } => {
                            // `mon.write_e` → emit raw "mon.write_e".
                            // Use a string buffer to capture the qualified name.
                            let mut tmp = String::new();
                            std::mem::swap(&mut self.out, &mut tmp);
                            self.emit_expr(callee);
                            std::mem::swap(&mut self.out, &mut tmp);
                            let _ = name;
                            tmp
                        }
                        _ => {
                            self.errors
                                .push("on <event>(arg): event must be `name` or `obj.name`".into());
                            return;
                        }
                    };
                    let event_ref = self.field_subs.get(&raw).cloned().unwrap_or(raw.clone());
                    let arg = match args.first() {
                        Some(CallArg::Expr(e)) => match &*e.kind {
                            ExprKind::Ident(id) => id.name.clone(),
                            _ => "_v".into(),
                        },
                        _ => "_v".into(),
                    };
                    let arg_ty = self
                        .event_types
                        .get(&raw)
                        .cloned()
                        .unwrap_or_else(|| "int64_t".into());
                    self.pad(depth);
                    writeln!(self.out, "{event_ref}.push_back([&]({arg_ty} {arg}) {{").ok();
                    self.emit_block(&h.body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}});").ok();
                } else {
                    // Cycle-trigger path. Edge mode comes from the parser
                    // (`rising` / `falling` / `level`). Default is rising
                    // — matches typical handshake protocols.
                    self.emit_cycle_trigger(h, depth, "_t_");
                }
            }
            StmtKind::Randomize {
                blocking: _,
                target,
                with_body,
            } => {
                let ty = match &*target.kind {
                    ExprKind::Ident(id) => self.let_types.get(&id.name).cloned(),
                    _ => None,
                };
                let ty = match ty {
                    Some(t) if self.transactions.contains(&t) => t,
                    Some(t) => {
                        self.errors.push(format!(
                            "randomize(t): t has type `{t}` but no `transaction {t}` is declared in this file"
                        ));
                        return;
                    }
                    None => {
                        self.errors.push(
                            "randomize(t): could not resolve t's type — declare with `let t : <Transaction>`".into()
                        );
                        return;
                    }
                };
                // Spec §4: transaction-level `keep` constraints are
                // semantically part of every `randomize(t)` of that
                // type. Merge them with any user-supplied `with …`
                // body before handing to the solver. Without this
                // merge, `keep` would silently parse but emit zero
                // C++ — a correctness footgun (caught while auditing
                // §4 implementation status).
                let txn_keeps = self.txn_keeps.get(&ty).cloned().unwrap_or_default();
                let mut combined: Vec<Expr> = Vec::with_capacity(txn_keeps.len() + with_body.len());
                combined.extend(txn_keeps);
                combined.extend(with_body.iter().cloned());
                if combined.is_empty() {
                    // No constraints anywhere: simple field-by-field PRNG path.
                    self.pad(depth);
                    write!(self.out, "randomize_{ty}(&").ok();
                    self.emit_expr(target);
                    writeln!(self.out, ");").ok();
                    self.emit_randomize_trace_event(&ty, target, depth);
                } else {
                    // Constraint-solving path via Z3.
                    self.emit_constraint_solver_block(&ty, target, &combined, depth);
                }
            }
            other => {
                self.errors.push(format!(
                    "statement not supported in v0 cpp_tb: {:?}",
                    std::mem::discriminant(other)
                ));
            }
        }
    }

    fn emit_let(&mut self, l: &LetStmt, depth: usize) {
        if l.name.name == "_" {
            self.pad(depth);
            if let Some(v) = &l.value {
                write!(self.out, "(void)(").ok();
                self.emit_expr(v);
                writeln!(self.out, ");").ok();
            } else {
                writeln!(self.out, "// let _ (discard)").ok();
            }
            return;
        }
        // Track let-binding type for randomize(t) resolution. Done first
        // (before the dut shortcut) so even nested lets register.
        if let Some(s) = type_simple_name(l.ty.as_ref()) {
            self.let_types.insert(l.name.name.clone(), s.to_string());
        }
        // Track mode annotation for the call-site passive-mode check.
        // Same gate as the pre-pass at lowering entry — only typed lets
        // with an explicit `active`/`passive` populate; inheritance is
        // resolved at lookup time via `resolve_path_mode`.
        if let Some(TypeExpr::Named { mode: Some(m), .. }) = l.ty.as_ref() {
            self.let_modes.insert(l.name.name.clone(), *m);
        }
        // Track explicit bit-width for width-method intrinsics
        // (`.trunc<N>()` etc.). Same logic as the per-test loop seed
        // in `emit_with_opts` — typed lets only; bare `let x = expr`
        // falls back to RHS inference.
        if let Some(TypeExpr::Builtin { name, args, .. }) = l.ty.as_ref() {
            if matches!(name, BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits) {
                if let Some(w) = type_arg_width(args) {
                    self.let_widths.insert(l.name.name.clone(), w as u32);
                }
            }
        }
        // Bus binding: `let axil : BusAxiLite = bind dut`. Track the
        // (bus, dut-root) pair so subsequent `axil.signal` accesses
        // can lower to flat DUT signals (`dut->axil_signal`). The
        // binding itself emits no C++ — it's purely a typing
        // assertion and a name-prefix declaration.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if let Some(bus) = self.buses.get(simple).cloned() {
                if let Some(v) = &l.value {
                    // Capture the bind expression as a string. For the
                    // common case of `bind dut`, that's just "dut".
                    let mut buf = String::new();
                    std::mem::swap(&mut self.out, &mut buf);
                    self.emit_expr(v);
                    std::mem::swap(&mut self.out, &mut buf);
                    // Test-scope binding: identifier and prefix are
                    // the same. Aliases (e.g. driver's `bus`) reuse
                    // this prefix when registering — see
                    // `emit_component_handler_registrations_bound`.
                    let prefix = l.name.name.clone();
                    self.bus_bindings
                        .insert(l.name.name.clone(), (bus, buf, prefix.clone()));
                    // Translate the AST's bind_remap entries into a
                    // (channel, signal) → port_name lookup table. The
                    // path is required to be exactly `<channel>.<signal>`
                    // (two segments) — single-segment paths and 3+
                    // levels error out cleanly.
                    if !l.bind_remap.is_empty() {
                        let mut map: std::collections::HashMap<(String, String), String> =
                            std::collections::HashMap::new();
                        for entry in &l.bind_remap {
                            if entry.path.len() != 2 {
                                self.errors.push(format!(
                                    "bind {} with: signal path `{}` must be exactly `<channel>.<signal>` (2 segments, got {})",
                                    l.name.name,
                                    entry.path.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join("."),
                                    entry.path.len(),
                                ));
                                continue;
                            }
                            map.insert(
                                (entry.path[0].name.clone(), entry.path[1].name.clone()),
                                entry.port.clone(),
                            );
                        }
                        self.bus_remap.insert(prefix, map);
                    }
                    return;
                }
            }
        }
        // RAL addrmap binding: `let chip : <AddrmapType> = bind <helper>`.
        // Same shape as regblock binding — emits a value-typed mirror
        // struct and records the helper.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if self.addrmaps.contains_key(simple) {
                if l.bind {
                    if let Some(v) = &l.value {
                        if let ExprKind::Ident(rhs) = &*v.kind {
                            self.let_helper
                                .insert(l.name.name.clone(), rhs.name.clone());
                        } else {
                            self.errors.push(format!(
                                "let {} : {simple} = bind <expr>: addrmap binding RHS must be a helper transactor identifier",
                                l.name.name,
                            ));
                        }
                    }
                } else {
                    self.errors.push(format!(
                        "let {} : {simple}: addrmap instantiation requires `= bind <helper>`",
                        l.name.name,
                    ));
                }
                self.pad(depth);
                writeln!(self.out, "{simple}_Mirror {};", l.name.name).ok();
                return;
            }
        }
        // RAL regblock binding: `let regs : <RegblockType> = bind <helper>`.
        // Emit `<T>_Mirror regs;` (reset values populate from the struct
        // default initializers emitted at file scope) and record the
        // helper variable name so accessor lowering can resolve it.
        // See docs/ral-support.md.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if self.regblocks.contains_key(simple) {
                if l.bind {
                    if let Some(v) = &l.value {
                        if let ExprKind::Ident(rhs) = &*v.kind {
                            self.let_helper
                                .insert(l.name.name.clone(), rhs.name.clone());
                        } else {
                            self.errors.push(format!(
                                "let {} : {simple} = bind <expr>: regblock binding RHS must be a helper transactor identifier",
                                l.name.name,
                            ));
                        }
                    }
                } else {
                    self.errors.push(format!(
                        "let {} : {simple}: regblock instantiation requires `= bind <helper>` (a transactor with write/read methods)",
                        l.name.name,
                    ));
                }
                self.pad(depth);
                writeln!(self.out, "{simple}_Mirror {};", l.name.name).ok();
                return;
            }
        }
        // Component binding: `let drv : AxilDrv = bind axil` for a
        // `driver` / `agent` / `monitor` declared `bound to BusType`.
        // The binding ties this driver instance to the named bus
        // binding so its on-handlers can use the bare `bus` identifier
        // for typed signal/handshake access. Emits the struct exactly
        // as the no-value path would, plus on-handler registrations
        // with the bus binding pushed for the duration of each body.
        if l.bind {
            if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                // Bound transactor: `let xact : T mode = bind axil`
                // for a `transactor T bound to BusType`. Compose a
                // synthetic ComponentDecl from the transactor body —
                // including or excluding the `when active` items
                // based on the instantiation's mode — and route
                // through the existing bound-driver-actor +
                // bound-monitor-actor paths. (Spec §8.1, T-2 in the
                // implementation roadmap.)
                if let Some(t) = self.transactors.get(simple).cloned() {
                    // Mode is mandatory at the binding site for
                    // transactors. We read it from the let's
                    // TypeExpr::Named.mode field, populated by
                    // parse_let_stmt.
                    let mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => *m,
                        _ => {
                            self.errors.push(format!(
                                "let {}: transactor instantiation requires a mode annotation (`{} active` or `{} passive`)",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                    };
                    let include_active = matches!(mode, TransactorMode::Active);
                    let synth = synth_component_from_transactor(&t, include_active);

                    // Four shapes:
                    //   bound transactor + `= bind axil`  → bus-actor path
                    //   bound transactor + no value       → error
                    //   unbound transactor + no value     → handler-registration path (same as plain driver/agent)
                    //   unbound transactor + `= bind axil`→ error
                    match (&t.bound_to, &l.value) {
                        (Some(bus_ty), Some(v)) => {
                            if let ExprKind::Ident(rhs) = &*v.kind {
                                if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned() {
                                    let want = type_simple_name(Some(bus_ty));
                                    if want != Some(binding.0.name.name.as_str()) {
                                        self.errors.push(format!(
                                            "let {} : {} = bind {}: transactor is bound to `{}`, but `{}` is a `{}`",
                                            l.name.name, simple, rhs.name,
                                            want.unwrap_or("?"),
                                            rhs.name, binding.0.name.name,
                                        ));
                                    }
                                    self.pad(depth);
                                    writeln!(self.out, "{simple} {};", l.name.name).ok();

                                    // Driver-actor path only fires for
                                    // active mode (passive synth has no
                                    // input event field, so it would
                                    // bail anyway — but skipping the
                                    // call is cleaner).
                                    if include_active {
                                        self.try_emit_bound_driver_actor(
                                            &synth,
                                            &l.name.name,
                                            depth,
                                            &binding,
                                        );
                                    }
                                    // Monitor-actor path: handshake
                                    // handlers in the always-present
                                    // body always fire, regardless of
                                    // mode. This is the observation
                                    // half of the transactor.
                                    self.emit_bound_monitor_actors(
                                        &synth,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    );
                                    return;
                                } else {
                                    self.errors.push(format!(
                                        "let {} : {} = bind {}: `{}` is not a known bus binding",
                                        l.name.name, simple, rhs.name, rhs.name,
                                    ));
                                    return;
                                }
                            }
                        }
                        (None, None) => {
                            // Unbound transactor: default-construct
                            // the struct, then walk the synth
                            // ComponentDecl to wire up `on`-handler
                            // subscribers. Same path as plain
                            // `driver`/`agent` instantiation. No bus
                            // actor coroutines spawn — the active
                            // half drives raw DUT signals (or
                            // sideband), reacting to `emit
                            // xact.req(t)` events from the test
                            // scope. The passive half observes via
                            // event-typed `on` handlers if present.
                            self.pad(depth);
                            writeln!(self.out, "{simple} {};", l.name.name).ok();
                            let tag = format!("_xactor_{}_", l.name.name);
                            self.emit_component_handler_registrations(
                                &synth,
                                &l.name.name,
                                depth,
                                &tag,
                            );
                            return;
                        }
                        (Some(_), None) => {
                            self.errors.push(format!(
                                "let {} : {}: transactor `{}` has a `bound to` clause; instantiation requires `= bind <bus>`",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                        (None, Some(_)) => {
                            self.errors.push(format!(
                                "let {} : {} = bind ...: transactor `{}` has no `bound to` clause; remove the `= bind` clause",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                    }
                    self.errors.push(format!(
                        "let {} : {} = bind <expr>: rhs must be a bare bus-binding name in v0",
                        l.name.name, simple,
                    ));
                    return;
                }
                // (Legacy bound `monitor MonName` instantiation path
                // is removed — `monitor` construct retired in PR-B.
                // Observation handlers live in transactor always-on
                // bodies, lowered via the synth ComponentDecl path
                // above.)
                if let Some(comp) = self.components.get(simple).cloned() {
                    if let Some(bus_ty) = &comp.bound_to {
                        // The rhs must name an existing bus binding (a
                        // previous `let X : BusType = bind dut`). Plain
                        // ident only in v0; richer expressions deferred.
                        if let Some(v) = &l.value {
                            if let ExprKind::Ident(rhs) = &*v.kind {
                                if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned() {
                                    // Type-check: the bus binding's
                                    // BusDecl name must match the
                                    // driver's `bound to <BusType>`.
                                    let want = type_simple_name(Some(bus_ty));
                                    if want != Some(binding.0.name.name.as_str()) {
                                        self.errors.push(format!(
                                            "let {} : {} = bind {}: driver is bound to `{}`, but `{}` is a `{}`",
                                            l.name.name, simple, rhs.name,
                                            want.unwrap_or("?"),
                                            rhs.name, binding.0.name.name,
                                        ));
                                    }
                                    self.pad(depth);
                                    writeln!(self.out, "{simple} {};", l.name.name).ok();
                                    // Try the Phase 2b coroutine actor
                                    // path first: if the driver has a
                                    // single input event with a
                                    // matching `on <ev>(t)` handler,
                                    // spawn an independent coroutine
                                    // that loops over a transaction
                                    // queue. Falls through to the
                                    // sync subscriber path when the
                                    // driver isn't actor-shaped (no
                                    // input event, or multiple
                                    // handlers — the latter stays sync
                                    // for now to keep the model
                                    // tractable).
                                    if self.try_emit_bound_driver_actor(
                                        &comp,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    ) {
                                        return;
                                    }
                                    let tag = format!("_{}_", comp.kind.keyword());
                                    self.emit_component_handler_registrations_bound(
                                        &comp,
                                        &l.name.name,
                                        depth,
                                        &tag,
                                        Some(binding),
                                    );
                                    return;
                                } else {
                                    self.errors.push(format!(
                                        "let {} : {} = bind {}: `{}` is not a known bus binding (declare it first via `let {} : <BusType> = bind dut`)",
                                        l.name.name, simple, rhs.name, rhs.name, rhs.name,
                                    ));
                                    return;
                                }
                            }
                        }
                        self.errors.push(format!(
                            "let {} : {} = bind <expr>: rhs must be a bare bus-binding name in v0",
                            l.name.name, simple,
                        ));
                        return;
                    }
                }
            }
        }
        // Skip the `let dut : T` decl — already emitted at main() prelude.
        if l.name.name == "dut" {
            return;
        }
        // `let e : event<T>` — pub/sub primitive. Lower to a vector of
        // std::function<void(T)>; `on e(arg) body end on` registers a
        // closure; `emit e(v)` calls every subscriber. (Spec §3.4 + §7.3.)
        if let Some(TypeExpr::Builtin {
            name: BuiltinTy::Event,
            args,
            ..
        }) = &l.ty
        {
            let inner = args
                .first()
                .map(|a| match a {
                    TypeArg::Type(t) => txn_field_c_type(t),
                    _ => "uint64_t".into(),
                })
                .unwrap_or_else(|| "uint64_t".into());
            self.pad(depth);
            writeln!(
                self.out,
                "std::vector<std::function<void({inner})>> {};",
                l.name.name
            )
            .ok();
            self.event_types.insert(l.name.name.clone(), inner);
            return;
        }
        if let Some(v) = &l.value {
            // `bus.<ch>.recv()` on the rhs expands the handshake +
            // captures the payload signal directly into the let.
            // Defer pad emission so the handshake helper can write its
            // own indentation.
            if self.try_emit_bus_handshake(v, Some(&l.name.name), depth) {
                return;
            }
            self.pad(depth);
            // Default to `int64_t` for integer-shaped lets so 32-bit
            // DUT signals zero-extend on assignment (matters for the
            // `assert got == expected` pattern when comparing widened
            // C++ ints against narrow Verilator outputs). Switch to
            // `auto` when the rhs is a call — function/tseq/method
            // returns can be `std::vector<T>` or a transaction value,
            // neither of which fit in int64_t.
            let ty = if rhs_wants_auto(v, &self.tseq_names) {
                "auto"
            } else {
                "int64_t"
            };
            write!(self.out, "{ty} {} = ", l.name.name).ok();
            self.emit_expr(v);
            writeln!(self.out, ";").ok();
        } else if let Some(t) = &l.ty {
            self.pad(depth);
            // No initializer. Transactions / scoreboards default-
            // construct (their struct field defaults run). Transactors
            // additionally register `on`-handlers (see the unbound-
            // transactor arm below).
            let simple = type_simple_name(Some(t));
            if let Some(name) = simple {
                if self.transactions.contains(name) || self.scoreboards.contains(name) {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    return;
                }
                // Unbound transactor: `let xact : T mode` for a
                // `transactor T` declared without `bound to BusType`.
                // Same lowering as plain `driver`/`agent` instantiation —
                // default-construct the struct, then walk the synth
                // ComponentDecl to register `on`-handler subscribers.
                // The active half drives raw DUT signals (or sideband)
                // reacting to `emit xact.req(t)`; the passive half (if
                // present) observes via event-typed `on` handlers.
                if let Some(t) = self.transactors.get(name).cloned() {
                    let mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => *m,
                        _ => {
                            self.errors.push(format!(
                                "let {}: transactor instantiation requires a mode annotation (`{} active` or `{} passive`)",
                                l.name.name, name, name,
                            ));
                            return;
                        }
                    };
                    if t.bound_to.is_some() {
                        self.errors.push(format!(
                            "let {} : {}: transactor `{}` has a `bound to` clause; instantiation requires `= bind <bus>`",
                            l.name.name, name, name,
                        ));
                        return;
                    }
                    let include_active = matches!(mode, TransactorMode::Active);
                    let synth = synth_component_from_transactor(&t, include_active);
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    let tag = format!("_xactor_{}_", l.name.name);
                    self.emit_component_handler_registrations(&synth, &l.name.name, depth, &tag);
                    return;
                }
                if let Some(comp) = self.components.get(name).cloned() {
                    // driver / agent / env / sequencer → default-construct
                    // the struct. The user assigns DUT pointers and other
                    // field values explicitly afterward (`drv.dut = dut;`).
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    // Register on-handlers in the body the same way
                    // monitors do — events get subscriber closures,
                    // bool exprs get cycle-trigger checkers. Tag prefix
                    // is the kind keyword so concurrent components at
                    // the same source span don't collide.
                    let tag = format!("_{}_", comp.kind.keyword());
                    self.emit_component_handler_registrations(&comp, &l.name.name, depth, &tag);
                    // Recurse through sub-component / sub-transactor
                    // fields with mode propagation. The let-instance's
                    // mode is the root of the inheritance chain:
                    //
                    //   let topenv : OuterEnv passive
                    //                ^^^^^^^^^^^^^^^^^
                    //   propagates →  env's agent fields         (no mode → passive)
                    //   propagates →    agent's transactor fields (no mode → passive)
                    //
                    // At each level a field-level explicit mode wins
                    // over the inherited mode. See
                    // `emit_subcomponent_handler_registrations`.
                    let root_mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => Some(*m),
                        _ => None,
                    };
                    self.emit_subcomponent_handler_registrations(
                        &comp,
                        &l.name.name,
                        root_mode,
                        depth,
                    );
                    // Top-level watchdog (spec §8.6). Install a periodic
                    // `_checkers` closure that fires `<Type>_watchdog(<inst>)`
                    // every `period` cycles. Sub-component watchdogs are
                    // installed by `emit_subcomponent_handler_registrations`.
                    for ci in &comp.items {
                        if let ComponentItem::Watchdog(w) = ci {
                            self.emit_watchdog_checker(&comp, w, &l.name.name, depth);
                        }
                    }
                    // Top-level connect edges (the let's own component's
                    // connect block, if any). Nested sub-component
                    // connects are emitted inside the recursive helper
                    // above so they're prefixed by the sub-instance path.
                    for ci in &comp.items {
                        if let ComponentItem::Connect(cb) = ci {
                            for edge in &cb.edges {
                                let from = expr_path_str(&edge.from);
                                let to = expr_path_str(&edge.to);
                                if let (Some(from), Some(to)) = (from, to) {
                                    self.pad(depth);
                                    writeln!(
                                        self.out,
                                        "{}.{}.push_back([&](auto _t) {{ for (auto& _s : {}.{}) _s(_t); }});",
                                        l.name.name, from, l.name.name, to,
                                    ).ok();
                                } else {
                                    self.errors.push(format!(
                                        "connect: edge endpoints must be plain field paths in v0 cpp_tb"
                                    ));
                                }
                            }
                        }
                    }
                    return;
                }
                if let Some(g) = self.covergroups.get(name).cloned() {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    // Register an inline sample closure (per-cycle level
                    // trigger). Spec §6 says sampling fires on the
                    // covergroup's clocking expression — for our v0
                    // single-clock primary-clock model, every tick is a
                    // sample. Multi-clock would tie this to the named
                    // clock instead.
                    self.emit_covergroup_sample_registration(&g, &l.name.name, depth);
                    return;
                }
            }
            writeln!(self.out, "int64_t {} = 0;", l.name.name).ok();
        } else {
            self.pad(depth);
            writeln!(self.out, "// let {} (no type / no value)", l.name.name).ok();
        }
    }

    /// Lvalue side of `<-` / `=` — `dut.x` becomes `dut->x`.
    fn emit_lvalue(&mut self, e: &Expr) {
        self.emit_expr_with_arrow(e, true);
    }

    fn emit_expr(&mut self, e: &Expr) {
        self.emit_expr_with_arrow(e, false);
    }

    /// Emit a single-line `<lhs> = <rhs>` style statement, dispatching
    /// on three orthogonal axes:
    ///   1. Pointer-rooted signal target → wrap with the
    ///      `harc_rt::harc_*` template helpers so `VlWide<N>` ports
    ///      compile against integer values.
    ///   2. Plain local target → bare `lhs = rhs;`.
    ///   3. RHS is a > 128-bit hex literal → route through
    ///      `harc_rt::harc_assign_words` with an
    ///      `std::initializer_list<uint32_t>`. Without this, the
    ///      literal would clamp at 128 bits in `c_int_literal`'s
    ///      composite path and silently lose the high bits.
    /// If `target` is a `regs.NAME` field access where `regs` is a
    /// RAL regblock binding, return `(regs_var, helper_var, helper_ty,
    /// offset_literal, register-access-policy)`. Used by
    /// `emit_signal_assignment` (write side) and `emit_expr_with_arrow`
    /// (read side).
    /// Compile-time-unrolled bitbash walk. Detects a bare statement
    /// `bitbash(<regs_ident>)` where the argument is a `let regs : R =
    /// bind helper` binding, and emits one write-all-ones / read-back
    /// + write-zero / read-back pair per RW register in the regblock.
    /// RO/WO registers are skipped (RO can't accept the write; WO
    /// can't be read meaningfully). Failures bump the `errors`
    /// counter and log via `sim_log_line` so the existing test-result
    /// machinery picks them up.
    fn try_emit_bitbash(&mut self, e: &Expr, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Ident(name) = &*callee.kind else {
            return false;
        };
        if name.name != "bitbash" || args.len() != 1 {
            return false;
        }
        let arg = match &args[0] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let ExprKind::Ident(regs_id) = &*arg.kind else {
            return false;
        };
        let Some(regs_ty) = self.let_types.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(block) = self.regblocks.get(&regs_ty).cloned() else {
            return false;
        };
        let Some(helper_var) = self.let_helper.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(helper_ty) = self.let_types.get(&helper_var).cloned() else {
            return false;
        };

        let default_w = block.default_width.unwrap_or(32);
        self.pad(depth);
        writeln!(
            self.out,
            "// bitbash({}) — RAL walk-all over RW regs of {}",
            regs_id.name, regs_ty
        )
        .ok();
        for reg in &block.registers {
            let w = reg.width.unwrap_or(default_w);
            let mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let off = c_int_literal_from(&reg.offset.kind);
            let regname = &reg.name.name;
            if !reg.access.writes_to_bus() || !reg.access.reads_from_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// bitbash: skipping {} (access {})",
                    regname,
                    reg.access.keyword()
                )
                .ok();
                continue;
            }
            // Two patterns: all-ones (masked to register width), then
            // zero. Each pair: write → read → compare → bump errors
            // if mismatch + sim_log.
            for (pat_label, pat) in [("ones", mask), ("zero", 0u64)] {
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                writeln!(self.out, "uint64_t _bb_pat = 0x{pat:x}ull;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{helper_ty}_write({helper_var}, {off}, _bb_pat);").ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "uint64_t _bb_got = {helper_ty}_read({helper_var}, {off});"
                )
                .ok();
                self.pad(depth + 1);
                writeln!(self.out, "if (_bb_got != _bb_pat) {{").ok();
                self.pad(depth + 2);
                writeln!(self.out,
                    "sim_log_line(\"FAIL\", \"bitbash {regname} {pat_label}: wrote 0x%llx, got 0x%llx\", (long long)_bb_pat, (long long)_bb_got);").ok();
                self.pad(depth + 2);
                writeln!(self.out, "errors++;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
        true
    }

    /// 3-level `chip.inst.REG` addrmap access. Returns
    /// `(chip_var, instance_path, helper_var, helper_ty,
    ///   effective_offset_expr, register-access-policy)`.
    /// 3-level `chip.inst.REG` addrmap access. If `inst` is aliased
    /// to another instance via `alias of`, the returned
    /// `instance_path` points at the TARGET's mirror cell (one
    /// storage shared across windows) while the effective bus
    /// offset still uses this instance's own base.
    fn resolve_addrmap_register_lookup(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, String, RegAccess)> {
        let ExprKind::Field {
            target: mid,
            name: reg_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: inst_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip_id) = &*outer.kind else {
            return None;
        };
        let chip_ty = self.let_types.get(&chip_id.name)?;
        let amap = self.addrmaps.get(chip_ty)?;
        let inst = amap
            .instances
            .iter()
            .find(|i| i.name.name == inst_name.name)?;
        let block = self.regblocks.get(&inst.regblock_ty.name)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let helper_var = self.let_helper.get(&chip_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let base = c_int_literal_from(&inst.base_addr.kind);
        let off = c_int_literal_from(&reg.offset.kind);
        let effective = format!("({base} + {off})");
        let mirror_inst_name = inst
            .alias_of
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| inst.name.name.clone());
        let instance_path = format!("{}.{}", chip_id.name, mirror_inst_name);
        Some((
            chip_id.name.clone(),
            instance_path,
            helper_var,
            helper_ty,
            effective,
            reg.access,
        ))
    }

    /// 4-level `chip.inst.REG.FIELD` addrmap+field access. Alias-
    /// aware like its 3-level sibling.
    fn resolve_addrmap_subfield_lookup(
        &self,
        target: &Expr,
    ) -> Option<(
        String,
        String,
        String,
        String,
        String,
        String,
        &'static str,
        u32,
        u32,
        RegAccess,
    )> {
        let ExprKind::Field {
            target: lvl3,
            name: fld_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl2,
            name: reg_name,
        } = &*lvl3.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl1,
            name: inst_name,
        } = &*lvl2.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip_id) = &*lvl1.kind else {
            return None;
        };
        let chip_ty = self.let_types.get(&chip_id.name)?;
        let amap = self.addrmaps.get(chip_ty)?;
        let inst = amap
            .instances
            .iter()
            .find(|i| i.name.name == inst_name.name)?;
        let block = self.regblocks.get(&inst.regblock_ty.name)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name.name == fld_name.name)?;
        let helper_var = self.let_helper.get(&chip_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let base = c_int_literal_from(&inst.base_addr.kind);
        let off = c_int_literal_from(&reg.offset.kind);
        let effective = format!("({base} + {off})");
        let reg_width = reg.width.unwrap_or(block.default_width.unwrap_or(32));
        let reg_c_type = mirror_field_c_type(reg_width);
        let bit_width = field_bit_width(&fld.ty);
        let mirror_inst_name = inst
            .alias_of
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| inst.name.name.clone());
        let instance_path = format!("{}.{}", chip_id.name, mirror_inst_name);
        Some((
            chip_id.name.clone(),
            instance_path,
            reg.name.name.clone(),
            helper_var,
            helper_ty,
            effective,
            reg_c_type,
            fld.bit_pos,
            bit_width,
            fld.access,
        ))
    }

    fn resolve_regblock_field_lookup(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, RegAccess)> {
        let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(regs_id) = &*outer.kind else {
            return None;
        };
        let regs_ty = self.let_types.get(&regs_id.name)?;
        let block = self.regblocks.get(regs_ty)?;
        let reg = block.registers.iter().find(|r| r.name.name == name.name)?;
        let helper_var = self.let_helper.get(&regs_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let offset_lit = c_int_literal_from(&reg.offset.kind);
        Some((
            regs_id.name.clone(),
            helper_var,
            helper_ty,
            offset_lit,
            reg.access,
        ))
    }

    fn resolve_regblock_field_write(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, RegAccess)> {
        self.resolve_regblock_field_lookup(target)
    }

    /// If `target` is a 3-level `regs.REG.FIELD` access where `regs`
    /// is a RAL regblock binding and FIELD is declared inside REG,
    /// return all metadata needed to emit a masked write / shifted
    /// read: `(regs_var, helper_var, helper_ty, offset_lit, reg_name,
    /// reg_c_type, bit_pos, bit_width, field-access-policy)`.
    fn resolve_regblock_subfield_lookup(
        &self,
        target: &Expr,
    ) -> Option<(
        String,
        String,
        String,
        String,
        String,
        &'static str,
        u32,
        u32,
        RegAccess,
    )> {
        let ExprKind::Field {
            target: mid,
            name: fld_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: reg_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(regs_id) = &*outer.kind else {
            return None;
        };
        let regs_ty = self.let_types.get(&regs_id.name)?;
        let block = self.regblocks.get(regs_ty)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name.name == fld_name.name)?;
        let helper_var = self.let_helper.get(&regs_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let offset_lit = c_int_literal_from(&reg.offset.kind);
        let reg_width = reg.width.unwrap_or(block.default_width.unwrap_or(32));
        let reg_c_type = mirror_field_c_type(reg_width);
        let bit_width = field_bit_width(&fld.ty);
        Some((
            regs_id.name.clone(),
            helper_var,
            helper_ty,
            offset_lit,
            reg.name.name.clone(),
            reg_c_type,
            fld.bit_pos,
            bit_width,
            fld.access,
        ))
    }

    /// Resolve the SV port name for `<bus>.<channel>.<signal>` access.
    /// Checks the `bind ... with { ch.sig: "port" }` remap first;
    /// falls back to the `<prefix>_<channel>_<signal>` convention.
    /// `prefix` is typically the bind variable's name (same as the
    /// `sig_prefix` field stored in `bus_bindings`).
    fn bus_signal_name(&self, prefix: &str, channel: &str, signal: &str) -> String {
        if let Some(map) = self.bus_remap.get(prefix) {
            if let Some(port) = map.get(&(channel.to_string(), signal.to_string())) {
                return port.clone();
            }
        }
        format!("{prefix}_{channel}_{signal}")
    }

    /// If `target` is `dut.<probe>` where the probe was declared with
    /// the `force` modifier, return its `ProbeAccessor`. Used by
    /// `emit_signal_assignment` and the `release` statement to emit
    /// the two-store (drv + en) lowering.
    fn resolve_force_probe(&self, target: &Expr) -> Option<ProbeAccessor> {
        let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return None;
        };
        if id.name != "dut" {
            return None;
        }
        let probe = self.probes.get(&name.name)?;
        if probe.force {
            Some(probe.clone())
        } else {
            None
        }
    }

    fn emit_signal_assignment(&mut self, target: &Expr, value: &Expr, depth: usize) {
        // `probe force <name>` write: lower to a paired store of
        // `<name>_drv = expr` and `<name>_en = 1`. See
        // docs/probe-signals.md. The bound SV stub's `always_comb`
        // picks up `_en=1` next cycle and procedurally forces the
        // target path with the latched `_drv` value.
        if let Some(probe) = self.resolve_force_probe(target) {
            write!(self.out, "dut->rootp->{} = ", probe.drive()).ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            self.pad(depth);
            writeln!(self.out, "dut->rootp->{} = 1;", probe.enable()).ok();
            return;
        }
        // Plain-probe write: not allowed. Surface a clear codegen
        // error rather than emit invalid C++. Read-only probes
        // can't drive their target signal — declare with `force`
        // for fault injection.
        if let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        {
            if let ExprKind::Ident(id) = &*outer.kind {
                if id.name == "dut" && self.probes.contains_key(&name.name) {
                    self.errors.push(format!(
                        "write to `dut.{}`: read-only probe — declare with `probe force` to enable fault injection",
                        name.name,
                    ));
                    return;
                }
            }
        }

        // RAL addrmap subfield write: `chip.inst.REG.FIELD = expr`.
        // Identical lowering to the regblock subfield path, but the
        // bus offset is `base(inst) + offset(REG)` and the mirror path
        // walks through the instance: `chip.inst.REG`.
        if let Some((
            _chip_var,
            inst_path,
            reg_name,
            helper_var,
            helper_ty,
            effective_off,
            reg_c_type,
            bit_pos,
            bit_width,
            access,
        )) = self.resolve_addrmap_subfield_lookup(target)
        {
            let mask = field_mask_literal(bit_width);
            write!(
                self.out,
                "{inst_path}.{reg_name} = ({inst_path}.{reg_name} & ~(({reg_c_type})0x{mask:x}u << {bit_pos})) | (((({reg_c_type})(",
            ).ok();
            self.emit_expr(value);
            writeln!(self.out, ")) & 0x{mask:x}u) << {bit_pos});",).ok();
            if access.writes_to_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {effective_off}, {inst_path}.{reg_name});",
                )
                .ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO field — write to bus suppressed (chip mirror still updated)"
                )
                .ok();
            }
            return;
        }

        // RAL addrmap register write: `chip.inst.REG = expr`.
        if let Some((_chip_var, inst_path, helper_var, helper_ty, effective_off, access)) =
            self.resolve_addrmap_register_lookup(target)
        {
            // The outer Field's `name` is the register name itself.
            write!(self.out, "{inst_path}.").ok();
            if let ExprKind::Field { name, .. } = &*target.kind {
                write!(self.out, "{}", name.name).ok();
            }
            write!(self.out, " = ").ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            if access.writes_to_bus() {
                self.pad(depth);
                write!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {effective_off}, "
                )
                .ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO register — write to bus suppressed (mirror updated)"
                )
                .ok();
            }
            return;
        }

        // RAL frontdoor field-level write: `regs.REG.FIELD = expr`.
        // Lowers to a read-modify-write on the mirror (mask out the
        // FIELD bits, OR in the new value shifted into POS) + a bus
        // write of the full register word. Checked before the
        // register-level path because it's strictly more specific
        // (3-level Field expr vs 2-level).
        if let Some((
            regs_var,
            helper_var,
            helper_ty,
            offset_lit,
            reg_name,
            reg_c_type,
            bit_pos,
            bit_width,
            access,
        )) = self.resolve_regblock_subfield_lookup(target)
        {
            let mask = field_mask_literal(bit_width);
            // Mirror update always happens; even RO mirrors might be
            // useful for the read-predict side. For RO we then DROP
            // the bus write — matches the RFC `ro` semantics.
            write!(
                self.out,
                "{regs_var}.{reg_name} = ({regs_var}.{reg_name} & ~(({reg_c_type})0x{mask:x}u << {bit_pos})) | (((({reg_c_type})(",
            ).ok();
            self.emit_expr(value);
            writeln!(self.out, ")) & 0x{mask:x}u) << {bit_pos});",).ok();
            if access.writes_to_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {offset_lit}, {regs_var}.{reg_name});",
                )
                .ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO field — write to bus suppressed (regs.{reg_name}.<field> mirror still updated)",
                ).ok();
            }
            return;
        }

        // RAL frontdoor write: `regs.NAME = expr` where `regs` is a
        // `let regs : <Regblock> = bind <helper>` instantiation. Lowers
        // to mirror update + `<HelperType>_write(helper, OFFSET, expr)`.
        if let Some((regs_var, helper_var, helper_ty, offset_lit, access)) =
            self.resolve_regblock_field_write(target)
        {
            write!(self.out, "{regs_var}.").ok();
            if let ExprKind::Field { name, .. } = &*target.kind {
                write!(self.out, "{}", name.name).ok();
            }
            write!(self.out, " = ").ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            if access.writes_to_bus() {
                self.pad(depth);
                write!(self.out, "{helper_ty}_write({helper_var}, {offset_lit}, ").ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO register — write to bus suppressed (mirror updated)"
                )
                .ok();
            }
            return;
        }

        let pointer_rooted = self.is_pointer_rooted_signal_lvalue(target);
        let wide_words = is_wide_int_literal(value);

        match (pointer_rooted, wide_words) {
            (true, Some(words)) => {
                // > 128-bit literal into a wide signal — word-list path.
                write!(self.out, "harc_rt::harc_assign_words(").ok();
                self.emit_lvalue(target);
                write!(self.out, ", {{").ok();
                for (i, w) in words.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ").ok();
                    }
                    write!(self.out, "{w}").ok();
                }
                writeln!(self.out, "}});").ok();
            }
            (true, None) => {
                // Pointer-rooted signal, ≤128b RHS — uniform helper path.
                write!(self.out, "harc_rt::harc_assign(").ok();
                self.emit_lvalue(target);
                write!(self.out, ", ").ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            }
            (false, _) => {
                // Plain local — bare assignment. (If RHS happens to be
                // a > 128-bit literal here it'll clamp via
                // `c_int_literal`'s fallback; locals aren't wide
                // today so this is a corner that doesn't show up.)
                self.emit_lvalue(target);
                write!(self.out, " = ").ok();
                self.emit_expr(value);
                writeln!(self.out, ";").ok();
            }
        }
    }

    /// Returns true if `e` is an L-value that ultimately lowers to a
    /// `<root>-><field>...` access chain (where `<root>` is a DUT pointer
    /// or bus-binding root). These are the cases where Verilator may
    /// have given the leaf signal a wide type (`VlWide<N>` for >64-bit
    /// ports), so plain `lhs = rhs` won't compile and we instead emit
    /// a `harc_rt::harc_assign(lhs, rhs);` call.
    fn is_pointer_rooted_signal_lvalue(&self, e: &Expr) -> bool {
        // Walk down field-access chain; root must be Ident in either
        // pointer_vars (e.g. `dut`) or bus_bindings (e.g. `axil`).
        let mut cur: &Expr = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, .. } => cur = target,
                ExprKind::Ident(id) => {
                    return self.pointer_vars.contains(&id.name)
                        || self.bus_bindings.contains_key(&id.name);
                }
                _ => return false,
            }
        }
    }

    fn emit_expr_with_arrow(&mut self, e: &Expr, lvalue: bool) {
        // Property-time substitutions for temporal SystemCall expressions.
        // `emit_property_check` populates `prop_subs` with span → snippet for
        // each `$past`/`$rose`/`$fell`/`$stable` occurrence; if we encounter
        // one here, emit the snippet instead of recursing into its inner.
        if !self.prop_subs.is_empty() {
            if let Some(s) = self.prop_subs.get(&(e.span.start, e.span.end)) {
                write!(self.out, "{s}").ok();
                return;
            }
        }
        match &*e.kind {
            ExprKind::Int(s) => {
                write!(self.out, "{}", c_int_literal(s)).ok();
            }
            ExprKind::Float(s) => {
                write!(self.out, "{s}").ok();
            }
            ExprKind::Time(s) => {
                // Strip the unit suffix and emit the numeric portion.
                let n: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_')
                    .collect();
                write!(self.out, "{n}").ok();
            }
            ExprKind::String(s) => {
                write!(self.out, "\"{}\"", escape_c(s)).ok();
            }
            ExprKind::Bool(b) => {
                write!(self.out, "{}", if *b { "true" } else { "false" }).ok();
            }
            ExprKind::Ident(id) => {
                // Monitor-body bare-name rewrite — see field_subs.
                if let Some(s) = self.field_subs.get(&id.name) {
                    write!(self.out, "{s}").ok();
                } else {
                    write!(self.out, "{}", id.name).ok();
                }
            }
            ExprKind::ImplicitSelf => {}
            ExprKind::Field { target, name } => {
                // RAL addrmap subfield read: `chip.inst.REG.FIELD`.
                // Mirror path is `chip.inst.REG`; offset is base+reg_off.
                if !lvalue {
                    if let Some((
                        _chip_var,
                        inst_path,
                        reg_name,
                        helper_var,
                        helper_ty,
                        effective_off,
                        _reg_c_type,
                        bit_pos,
                        bit_width,
                        access,
                    )) = self.resolve_addrmap_subfield_lookup(e)
                    {
                        let mask = field_mask_literal(bit_width);
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "((({inst_path}.{reg_name} = {helper_ty}_read({helper_var}, {effective_off})) >> {bit_pos}) & 0x{mask:x}u)",
                            ).ok();
                        } else {
                            write!(
                                self.out,
                                "(({inst_path}.{reg_name} >> {bit_pos}) & 0x{mask:x}u)",
                            )
                            .ok();
                        }
                        return;
                    }
                }
                // RAL addrmap register read: `chip.inst.REG`.
                if !lvalue {
                    if let Some((
                        _chip_var,
                        inst_path,
                        helper_var,
                        helper_ty,
                        effective_off,
                        access,
                    )) = self.resolve_addrmap_register_lookup(e)
                    {
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "({inst_path}.{} = {helper_ty}_read({helper_var}, {effective_off}))",
                                name.name,
                            ).ok();
                        } else {
                            write!(self.out, "{inst_path}.{}", name.name).ok();
                        }
                        return;
                    }
                }
                // RAL frontdoor field-level read: `regs.REG.FIELD`.
                // For RW/RO: `((regs.REG = <H>_read(helper, OFFSET)) >> POS) & MASK`
                // — bus read updates the mirror (read-side predict) AND
                // returns the value, then bit-extract.
                // For WO: `((regs.REG >> POS) & MASK)` — the bus would
                // return garbage on a WO register, so we serve from the
                // mirror.
                if !lvalue {
                    if let Some((
                        regs_var,
                        helper_var,
                        helper_ty,
                        offset_lit,
                        reg_name,
                        _reg_c_type,
                        bit_pos,
                        bit_width,
                        access,
                    )) = self.resolve_regblock_subfield_lookup(e)
                    {
                        let mask = field_mask_literal(bit_width);
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "((({regs_var}.{reg_name} = {helper_ty}_read({helper_var}, {offset_lit})) >> {bit_pos}) & 0x{mask:x}u)",
                            ).ok();
                        } else {
                            // WO — serve from mirror only.
                            write!(
                                self.out,
                                "(({regs_var}.{reg_name} >> {bit_pos}) & 0x{mask:x}u)",
                            )
                            .ok();
                        }
                        return;
                    }
                }
                // RAL frontdoor register-level read: `regs.NAME` rvalue.
                // RW/RO: `(regs.NAME = <H>_read(helper, OFFSET))` —
                // assignment-expression form so the mirror updates AND
                // the expression yields the value. WO: serve from
                // mirror only.
                if !lvalue {
                    if let Some((regs_var, helper_var, helper_ty, offset_lit, access)) =
                        self.resolve_regblock_field_lookup(e)
                    {
                        // The `name` of the outer Field expr is the
                        // register name itself.
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "({regs_var}.{} = {helper_ty}_read({helper_var}, {offset_lit}))",
                                name.name,
                            )
                            .ok();
                        } else {
                            write!(self.out, "{regs_var}.{}", name.name).ok();
                        }
                        return;
                    }
                }
                // Probe lowering. `dut.<name>` where <name> was declared
                // as a `probe` on the `let dut : T` decl resolves to
                // `dut->rootp-><mangled>`, not the (non-existent) top-
                // level `dut-><name>`. Reads still wrap with harc_read
                // so wide signals downcast cleanly. See
                // docs/probe-signals.md.
                if let ExprKind::Ident(id) = &*target.kind {
                    if id.name == "dut" {
                        if let Some(probe) = self.probes.get(&name.name).cloned() {
                            // Reads always come from the read-side
                            // accessor regardless of `force`. Writes
                            // are handled elsewhere (emit_signal_
                            // assignment) since they expand to a
                            // two-statement drv+en pair.
                            if lvalue {
                                write!(self.out, "dut->rootp->{}", probe.read).ok();
                            } else {
                                write!(self.out, "harc_rt::harc_read(dut->rootp->{})", probe.read)
                                    .ok();
                            }
                            return;
                        }
                    }
                }
                // Bus-bound binding lowering. If `target` is an Ident
                // bound to a bus (`let axil : BusAxiLite = bind dut`),
                // resolve `axil.signal` to `<root>-><axil>_<signal>`.
                // Two-level form (`axil.aw.addr`) walks into the bus's
                // handshake_channel groupings and emits
                // `<root>-><axil>_aw_addr`.
                if let Some(s) = self.try_emit_bus_field_access(target, name) {
                    if lvalue {
                        write!(self.out, "{s}").ok();
                    } else {
                        // Wrap reads with harc_rt::harc_read so wide
                        // signals (Verilator's VlWide<N> for >64-bit
                        // ports) implicitly convert to uint64_t. For
                        // narrow signals it's a no-op cast.
                        write!(self.out, "harc_rt::harc_read({s})").ok();
                    }
                    return;
                }
                // Pointer-typed identifiers (DUTs, threaded as Named-type
                // params) use `->`. Everything else uses `.`. Tracked in
                // `pointer_vars` — populated from `let x : <NamedType>` and
                // function params with a Named type (see emit_function).
                let is_pointer_root = matches!(&*target.kind,
                    ExprKind::Ident(id) if self.pointer_vars.contains(&id.name)
                );
                let wrap_read = is_pointer_root && !lvalue;
                if wrap_read {
                    write!(self.out, "harc_rt::harc_read(").ok();
                }
                self.emit_expr(target);
                if is_pointer_root {
                    write!(self.out, "->").ok();
                } else {
                    write!(self.out, ".").ok();
                }
                write!(self.out, "{}", name.name).ok();
                if wrap_read {
                    write!(self.out, ")").ok();
                }
            }
            ExprKind::Index { target, index } => {
                // Pass `lvalue=true` to suppress the `harc_rt::harc_read`
                // wrap on a pointer-rooted Field target. Indexing a
                // wide signal (VlWide<N>) needs the raw `dut->field`
                // expression so VlWide's `operator[]` is reachable;
                // wrapping with `harc_read` would yield a `uint64_t`
                // which can't be indexed. Narrow signals shouldn't be
                // indexed in the first place — that path errors at
                // C++ compile, which is the desired outcome.
                self.emit_expr_with_arrow(target, /*lvalue*/ true);
                write!(self.out, "[").ok();
                self.emit_expr(index);
                write!(self.out, "]").ok();
            }
            ExprKind::Call { callee, args } => {
                // Env/test-level quiescence helper: `env.quiesced(N)`
                // aggregates the built-in `idle(N)` predicate over all
                // nested component fields registered under that component.
                // Timed `wait until` expands the same helper into per-leaf
                // predicates so timeout diagnostics name the blocking
                // sub-component.
                if let Some((instances, n_expr)) =
                    self.resolve_component_quiesced_predicate(callee, args)
                {
                    if instances.is_empty() {
                        write!(self.out, "true").ok();
                    } else if instances.len() == 1 {
                        self.emit_idle_predicate(&instances[0], "idle", n_expr);
                    } else {
                        for (i, instance) in instances.iter().enumerate() {
                            if i > 0 {
                                write!(self.out, " && ").ok();
                            }
                            write!(self.out, "(").ok();
                            self.emit_idle_predicate(instance, "idle", n_expr);
                            write!(self.out, ")").ok();
                        }
                    }
                    return;
                }
                // Built-in activity-tracking predicates: `obj.idle(N)`,
                // `obj.idle_in(N)`, `obj.idle_out(N)` lower directly to
                // arithmetic on the auto-injected heartbeat fields.
                // Recognized BEFORE hookable method dispatch so a user
                // can't shadow them. (Spec §7.x — activity tracking.)
                if let Some((instance, kind)) = self.resolve_component_idle_predicate(callee) {
                    if args.len() != 1 {
                        self.errors.push(format!(
                            "{instance}.{kind}: expected 1 cycle-count arg, got {}",
                            args.len(),
                        ));
                        write!(self.out, "false").ok();
                        return;
                    }
                    let n_expr = match &args[0] {
                        CallArg::Expr(e) => e,
                        CallArg::Named { value, .. } => value,
                    };
                    self.emit_idle_predicate(&instance, &kind, n_expr);
                    return;
                }
                // Width-method intrinsics: `.trunc<N>()` / `.zext<N>()` /
                // `.sext<N>()` / `.resize<N>()`. Mirrors arch-com's
                // sim_codegen lowering (src/sim_codegen/mod.rs:2688).
                // Parser emits these as `Call { callee: Field{recv, name},
                // args: [width_expr] }`. The width_expr resolves to a
                // const integer at emit time (literal or const path).
                if self.try_emit_width_method(callee, args) {
                    return;
                }

                // Method-call rewrite: `obj.method(args)` where `obj`'s
                // type is a known component with a `hookable method`
                // lowers to `<Type>_method(obj, args)`. Falls through to
                // the generic call shape when no method match is found,
                // so plain free-function calls keep working.
                if let Some((comp_ty, instance, method)) =
                    self.resolve_component_method_call(callee)
                {
                    // Passive-mode call-site check (spec §8.1):
                    // a hookable declared inside `T.when_active` is
                    // structurally absent from passive instances (the
                    // `when_active` block is elided by
                    // `synth_component_from_transactor` for passive).
                    // Today both halves still emit as free C++ functions
                    // — the actor coroutine is the only thing the
                    // passive mode suppresses — so a `passive_instance
                    // .when_active_method(...)` call would otherwise
                    // silently dispatch into orphan code. Surface it.
                    if self.method_lives_in_when_active(&comp_ty, &method) {
                        let path = Self::extract_method_call_path(callee)
                            .unwrap_or_else(|| vec![instance.clone()]);
                        if let Some(TransactorMode::Passive) = self.resolve_path_mode(&path) {
                            self.errors.push(format!(
                                "method `{}.{}(...)`: hookable `{}` lives inside `when active` of transactor `{}`, \
                                 so it does not exist on a passive instance. Change the let-binding to `{} active`, \
                                 or remove the call. See spec §8.1.",
                                instance, method, method, comp_ty, comp_ty,
                            ));
                            return;
                        }
                    }
                    write!(self.out, "{comp_ty}_{method}({instance}").ok();
                    for a in args.iter() {
                        write!(self.out, ", ").ok();
                        match a {
                            CallArg::Expr(ex) => self.emit_expr(ex),
                            CallArg::Named { value, .. } => self.emit_expr(value),
                        }
                    }
                    write!(self.out, ")").ok();
                    return;
                }
                self.emit_expr(callee);
                write!(self.out, "(").ok();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ").ok();
                    }
                    match a {
                        CallArg::Expr(ex) => self.emit_expr(ex),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Cast { expr, ty } => {
                // Emit `((<c_type>)(<inner>))` when the target type
                // maps to a known C++ integer type — mirrors arch-com's
                // postfix `expr as Type` lowering. Width-widening
                // casts MATTER in C++: `1 << 31` against a `int`
                // literal is undefined behavior, while
                // `((uint64_t)1) << 31` is well-defined. Bool↔UInt
                // narrowing also flows through here, with C++'s
                // implicit conversions handling the no-op cases.
                //
                // For non-Builtin target types (struct, named type),
                // drop the cast and emit just the inner expression —
                // those are usually identity casts at the type-system
                // level that don't need a C++ representation.
                if matches!(ty, TypeExpr::Builtin { .. }) {
                    let cty = c_type_for(ty);
                    write!(self.out, "(({cty})(").ok();
                    self.emit_expr(expr);
                    write!(self.out, "))").ok();
                } else {
                    self.emit_expr(expr);
                }
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "!",
                    UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                write!(self.out, "{s}").ok();
                self.emit_expr(expr);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // Wide-literal == / != routing: when either operand is
                // a > 128-bit hex literal, route through
                // `harc_rt::harc_eq_words(sig, {w0, w1, ...})` so the
                // comparison happens word-by-word instead of through
                // `_harc_u128` (which would silently truncate the
                // literal to 128 bits and produce wrong matches).
                if matches!(op, BinaryOp::Eq | BinaryOp::Ne) {
                    let lhs_words = is_wide_int_literal(lhs);
                    let rhs_is_wide = is_wide_int_literal(rhs).is_some();
                    let words = lhs_words.or_else(|| is_wide_int_literal(rhs));
                    if let Some(words) = words {
                        // The signal side is whichever operand is not the
                        // wide literal. Prefer rhs if both happen to be
                        // (which won't normally happen — rejected by
                        // typechecking — but keeps codegen total).
                        let sig_side = if rhs_is_wide { lhs } else { rhs };
                        if matches!(op, BinaryOp::Ne) {
                            write!(self.out, "(!").ok();
                        }
                        write!(self.out, "harc_rt::harc_eq_words(").ok();
                        // The signal side: pass as L-value (no harc_read
                        // wrap) so the helper sees the raw VlWide<N>.
                        self.emit_expr_with_arrow(sig_side, /*lvalue*/ true);
                        write!(self.out, ", {{").ok();
                        for (i, w) in words.iter().enumerate() {
                            if i > 0 {
                                write!(self.out, ", ").ok();
                            }
                            write!(self.out, "{w}").ok();
                        }
                        write!(self.out, "}})").ok();
                        if matches!(op, BinaryOp::Ne) {
                            write!(self.out, ")").ok();
                        }
                        return;
                    }
                }
                self.emit_expr(lhs);
                let s = c_binary_op(*op);
                write!(self.out, " {s} ").ok();
                self.emit_expr(rhs);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                // Wrap in parens so the ternary doesn't bind into a
                // surrounding higher-precedence operator on the C++
                // side (e.g. `a + (cond ? x : y)` lowers correctly).
                write!(self.out, "(").ok();
                self.emit_expr(cond);
                write!(self.out, " ? ").ok();
                self.emit_expr(then_branch);
                write!(self.out, " : ").ok();
                self.emit_expr(else_branch);
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_expr(inner);
                write!(self.out, ")").ok();
            }
            ExprKind::RangeLit { lo, hi } => {
                // Bare ranges outside of `for`/`in` aren't representable in C++.
                // Emit a comment so the user sees the issue without a hard fail.
                write!(self.out, "/* range ").ok();
                if let Some(l) = lo {
                    self.emit_expr(l);
                }
                write!(self.out, "..").ok();
                if let Some(h) = hi {
                    self.emit_expr(h);
                }
                write!(self.out, " */ 0").ok();
            }
            other => {
                self.errors.push(format!(
                    "expression not supported in v0 cpp_tb: {:?}",
                    std::mem::discriminant(other)
                ));
                write!(self.out, "/* unsupported */ 0").ok();
            }
        }
    }
}

/// True if any code path will emit the Z3 solver block. Drives the
/// `<z3++.h>` include + Z3 link flags. The check needs to mirror the
/// codegen's actual decision:
///   * `randomize(t) with <body>` — always solver (user wrote constraints)
///   * bare `randomize(t)` where `t`'s transaction has `keep` items —
///     also solver (the keeps merge into a solver block at the call site)
pub fn uses_constraint_solver(file: &SourceFile) -> bool {
    // First pass: collect names of transactions that declare any
    // `keep` items. Any bare `randomize(t)` against one of these
    // routes through Z3 after the §4 keep-merge.
    let keep_bearing: std::collections::HashSet<&str> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transaction(t) => {
                let has_keep = txn_body_has_keep(&t.body);
                if has_keep {
                    Some(t.name.name.as_str())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    fn block(b: &Block, kb: &std::collections::HashSet<&str>) -> bool {
        b.stmts.iter().any(|s| stmt(s, kb))
    }
    fn stmt(s: &Stmt, kb: &std::collections::HashSet<&str>) -> bool {
        match &s.kind {
            // `with <body>` always solves; bare randomize solves when
            // the target's transaction carries keeps. The target's
            // type isn't on the AST node directly here — we conservatively
            // return `true` for bare randomize when the file has ANY
            // keep-bearing transaction. False positives are cheap
            // (an unused include); false negatives are compile failures.
            StmtKind::Randomize { with_body, .. } => !with_body.is_empty() || !kb.is_empty(),
            StmtKind::For(f) => block(&f.body, kb),
            StmtKind::Repeat(r) => block(&r.body, kb),
            StmtKind::Loop(b) => block(b, kb),
            StmtKind::While { body, .. } => block(body, kb),
            StmtKind::If(i) => {
                block(&i.then_block, kb)
                    || i.else_block.as_ref().map_or(false, |b| block(b, kb))
                    || i.elsifs.iter().any(|(_, b)| block(b, kb))
            }
            StmtKind::Fork(f) => f.branches.iter().any(|b| block(b, kb)),
            StmtKind::Parallel(bs) | StmtKind::Schedule(bs) => bs.iter().any(|b| block(b, kb)),
            StmtKind::Select(arms) => arms.iter().any(|a| block(&a.action, kb)),
            StmtKind::On(h) => block(&h.body, kb),
            StmtKind::After { body, .. } => block(body, kb),
            _ => false,
        }
    }
    file.items.iter().any(|it| match it {
        Item::Function(f) => block(&f.body, &keep_bearing),
        Item::Test(t) => t.items.iter().any(|ti| match ti {
            TestItem::Stmt(s) => stmt(s, &keep_bearing),
            TestItem::Scope(sc) => {
                sc.setup.as_ref().map_or(false, |b| block(b, &keep_bearing))
                    || sc.run.as_ref().map_or(false, |b| block(b, &keep_bearing))
                    || sc.check.as_ref().map_or(false, |b| block(b, &keep_bearing))
                    || sc
                        .teardown
                        .as_ref()
                        .map_or(false, |b| block(b, &keep_bearing))
            }
            _ => false,
        }),
        _ => false,
    })
}

/// Pick a C++ representation for a scoreboard field. Mostly the same as
/// `txn_field_c_type` but supports `queue<T>` → `HarcQueue<T>` (the small
/// runtime template emitted at file scope when scoreboards are present).
fn scoreboard_field_c_type(t: &TypeExpr) -> String {
    if let TypeExpr::Builtin {
        name: BuiltinTy::Queue,
        args,
        ..
    } = t
    {
        let inner = args
            .first()
            .map(|a| match a {
                TypeArg::Type(ty) => txn_field_c_type(ty),
                _ => "uint64_t".into(),
            })
            .unwrap_or_else(|| "uint64_t".into());
        return format!("HarcQueue<{inner}>");
    }
    txn_field_c_type(t)
}

/// Pick a C++ representation for a transaction field's HARC type. Conservative
/// — small ints get widened to `uint64_t`/`int64_t`; bool stays bool; named
/// types get the bare name (likely an enum which is `int64_t` in v0).
fn txn_field_c_type(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
                cpp_uint_for_width(int_width_from_args(args)).into()
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => {
                cpp_sint_for_width(int_width_from_args(args)).into()
            }
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "bool".into(),
            _ => "uint64_t".into(),
        },
        // Enums (and other named user types) lower to int64_t. Real type
        // emission lands when we have a proper type system.
        TypeExpr::Named { .. } => "int64_t".into(),
    }
}

/// Default initialiser for a transaction field. Uses the user's `default
/// <expr>` clause when given; falls back to type-appropriate zero / false.
/// Default exprs are restricted to simple forms (int / bool / ident / float)
/// in v0 — anything more complex falls back to `0`.
fn field_default(f: &Field) -> String {
    if let Some(d) = &f.default {
        return format_simple_expr(d);
    }
    match &f.ty {
        TypeExpr::Builtin { name, .. } => match name {
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "false".into(),
            _ => "0".into(),
        },
        _ => "0".into(),
    }
}

fn format_simple_expr(e: &Expr) -> String {
    match &*e.kind {
        ExprKind::Int(s) => c_int_literal(s),
        ExprKind::Ident(id) => id.name.clone(),
        ExprKind::Bool(b) => (if *b { "true" } else { "false" }).into(),
        ExprKind::Float(s) => s.clone(),
        _ => "0".into(),
    }
}

/// Extract a width from a builtin's first type-arg if it's a literal (e.g.
/// `uint<32>` → Some(32)). Returns None if the arg isn't a parseable int.
fn type_arg_width(args: &[TypeArg]) -> Option<u32> {
    let arg = args.first()?;
    match arg {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn enum_width(variant_count: usize) -> u32 {
    if variant_count <= 1 {
        1
    } else {
        usize::BITS - (variant_count - 1).leading_zeros()
    }
}

/// Translate a HARC type expression to a C++ type for function parameters
/// and return types. Best-effort and conservative — integer-shaped HARC
/// types collapse to `uint64_t` / `int64_t`, since the actual width on the
/// Verilator side is determined by the DUT port type and we just shovel
/// values through. Bool stays bool. Unknown named types emit the bare
/// identifier (likely won't compile — surfaces the gap to the user).
/// Render a field-access chain like `seq.dispatched` or just `req`
/// as a dot-separated string. Returns `None` for shapes the connect
/// codegen can't handle in v0 (anything other than nested
/// `Field { Field { Ident, _ }, _ }` chains rooted at a bare ident).
fn expr_path_str(e: &Expr) -> Option<String> {
    match &*e.kind {
        ExprKind::Ident(id) => Some(id.name.clone()),
        ExprKind::Field { target, name } => {
            let head = expr_path_str(target)?;
            Some(format!("{head}.{}", name.name))
        }
        _ => None,
    }
}

/// If `event` is the trigger of an `on event_name(arg)` handler,
/// returns `(event_name, arg_binding_name)`. Anything else (a bool
/// expression like `on dut.x && y`, or a malformed call) returns
/// `None`. The binding name falls back to `_v` when the user wrote
/// `on event(_)` or omitted the arg.
/// Detect the `<bus>.<channel>.handshake(<arg>)` form used by bound
/// monitors (Phase 2c). Returns `Some((channel_name, arg_name))`
/// when the expression matches AND the outer `<bus>` ident matches
/// the expected binding name (typically `"bus"`); `None` otherwise.
///
/// The expression is a 3-level tree:
///   ExprKind::Call {
///     callee: Field { target: Field { target: Ident(bus), name: ch },
///                     name: "handshake" },
///     args:   [Expr::Ident(arg)] }
fn extract_bus_handshake_event(event: &Expr, bus_ident: &str) -> Option<(String, String)> {
    let ExprKind::Call { callee, args } = &*event.kind else {
        return None;
    };
    let ExprKind::Field {
        target,
        name: method,
    } = &*callee.kind
    else {
        return None;
    };
    if method.name != "handshake" {
        return None;
    }
    let ExprKind::Field {
        target: outer,
        name: ch,
    } = &*target.kind
    else {
        return None;
    };
    let ExprKind::Ident(id) = &*outer.kind else {
        return None;
    };
    if id.name != bus_ident {
        return None;
    }
    let arg_name = match args.first() {
        Some(CallArg::Expr(e)) => match &*e.kind {
            ExprKind::Ident(id) => id.name.clone(),
            _ => "_v".into(),
        },
        _ => "_v".into(),
    };
    Some((ch.name.clone(), arg_name))
}

fn extract_event_subscription(event: &Expr) -> Option<(String, String)> {
    let ExprKind::Call { callee, args } = &*event.kind else {
        return None;
    };
    let event_name = match &*callee.kind {
        ExprKind::Ident(id) => id.name.clone(),
        _ => return None,
    };
    let arg_name = match args.first() {
        Some(CallArg::Expr(e)) => match &*e.kind {
            ExprKind::Ident(id) => id.name.clone(),
            _ => "_v".into(),
        },
        _ => "_v".into(),
    };
    Some((event_name, arg_name))
}

/// True when `let x = EXPR` should use `auto` rather than `int64_t`
/// for the local's declared type. Picks `auto` whenever the rhs is a
/// call (which can return `std::vector<T>` from a tseq, or a
/// transaction by value from `queue<T>::pop()`, etc.) and falls back
/// to `int64_t` for everything else (DUT signal reads, plain numeric
/// expressions — `int64_t` zero-extends them safely for comparisons
/// against int literals). The `tseq_names` arg is kept for future use
/// but the current decision is broader than tseq alone.
fn rhs_wants_auto(e: &Expr, _tseq_names: &std::collections::HashSet<String>) -> bool {
    matches!(&*e.kind, ExprKind::Call { .. })
}

/// Extract `T` from `TSeq<T>`. Returns the C++ rendering of the inner
/// type. The TSeq builtin always carries exactly one type-arg in
/// well-formed source; if missing or malformed, returns `None` so the
/// caller can fall back to a sentinel type (the user already wrote
/// something well-formed in practice — this guards against synth from
/// degenerate ASTs).
fn tseq_inner_type(t: &TypeExpr) -> Option<String> {
    if let TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    } = t
    {
        if let Some(TypeArg::Type(inner)) = args.first() {
            return Some(c_type_for(inner));
        }
        if let Some(TypeArg::Expr(e)) = args.first() {
            // `TSeq<MyTxn>` parses as Expr(Ident) at the type level — if
            // the path is a single identifier, treat it as a Named type.
            if let ExprKind::Ident(id) = &*e.kind {
                return Some(id.name.clone());
            }
        }
    }
    None
}

/// Pull the bit-width literal out of a builtin integer's args list
/// (`uint<N>`, `bits<N>`, etc.). Returns `None` if the args don't have
/// a recognizable integer literal — caller falls back to a default
/// (typically 64).
fn int_width_from_args(args: &[TypeArg]) -> Option<u32> {
    args.first().and_then(|a| match a {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    })
}

/// Evaluate a HARC expression as a compile-time integer width. Used by
/// the width-method intrinsics (`.trunc<N>()` / `.zext<N>()` /
/// `.sext<N>()` / `.resize<N>()`) to extract `N`. Today recognizes only
/// integer literals (the common case); a future pass could fold `const
/// NAME : <Ty> = <expr>` references too.
fn eval_const_width(e: &Expr) -> Option<u32> {
    match &*e.kind {
        ExprKind::Paren(inner) => eval_const_width(inner),
        ExprKind::Int(s) => {
            let stripped = s.replace('_', "");
            if let Some(rest) = stripped
                .strip_prefix("0x")
                .or_else(|| stripped.strip_prefix("0X"))
            {
                u32::from_str_radix(rest, 16).ok()
            } else if let Some(rest) = stripped
                .strip_prefix("0b")
                .or_else(|| stripped.strip_prefix("0B"))
            {
                u32::from_str_radix(rest, 2).ok()
            } else {
                stripped.parse::<u32>().ok()
            }
        }
        _ => None,
    }
}

/// Pick the C++ value-type for a HARC unsigned integer of the given
/// bit-width:
///   1..64    → uint64_t       (every narrow case widens; native ops)
///   65..128  → _harc_u128     (typedef of __uint128_t; mirrors arch-com)
///   >128     → _harc_u128     (truncates to low 128; whole-signal access
///                              beyond 128b is rare; per-word indexing
///                              via `dut.field[i]` covers the gap)
fn cpp_uint_for_width(w: Option<u32>) -> &'static str {
    match w {
        Some(n) if n > 64 => "_harc_u128",
        _ => "uint64_t",
    }
}

fn cpp_sint_for_width(w: Option<u32>) -> &'static str {
    match w {
        // No native __int128 signed in C++ portably — but unsigned
        // ops bit-wise emulate signed for the common ops we care
        // about. Cast at use sites if signedness matters above 64b.
        Some(n) if n > 64 => "_harc_u128",
        _ => "int64_t",
    }
}

fn c_type_for(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
                cpp_uint_for_width(int_width_from_args(args)).into()
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => {
                cpp_sint_for_width(int_width_from_args(args)).into()
            }
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "bool".into(),
            BuiltinTy::String => "const char*".into(),
            BuiltinTy::Time => "uint64_t".into(),
            BuiltinTy::TSeq => {
                // `TSeq<T>` → `const std::vector<T>&` for params (pass-
                // by-reference avoids copying every transaction across
                // method calls). Matches the tseq lowering — a tseq
                // emits a `std::vector<T>` accumulator filled by
                // `yield`. Outside param context this is also a sane
                // local-variable type.
                let inner = tseq_inner_type(t).unwrap_or_else(|| "uint64_t".into());
                format!("const std::vector<{inner}>&")
            }
            BuiltinTy::Queue => {
                // `queue<T>` as a function param — pass by reference to
                // avoid copying the runtime queue. Mirrors the field-type
                // lowering.
                let inner = args
                    .first()
                    .map(|a| match a {
                        TypeArg::Type(ty) => txn_field_c_type(ty),
                        TypeArg::Expr(e) => match &*e.kind {
                            ExprKind::Ident(id) => id.name.clone(),
                            _ => "uint64_t".into(),
                        },
                        _ => "uint64_t".into(),
                    })
                    .unwrap_or_else(|| "uint64_t".into());
                format!("HarcQueue<{inner}>&")
            }
            // Aggregates / verification-only types fall back to the spelling
            // — caller will get a compile error pointing at the gap.
            _ => format!("/* TODO: type {:?} */ uint64_t", name),
        },
        TypeExpr::Named { name, .. } => {
            // User-defined module types lower to Verilator pointers (`VFoo*`).
            // Matches the `let dut : AxiLiteRegs` → `VAxiLiteRegs* dut` rule
            // already used for the test-level DUT decl.
            let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            format!("V{last}*")
        }
    }
}

fn cpp_param_names(params: &[Param]) -> Vec<String> {
    let mut discard_idx = 0usize;
    params
        .iter()
        .map(|p| {
            if p.name.name == "_" {
                let name = format!("_discard{discard_idx}");
                discard_idx += 1;
                name
            } else {
                p.name.name.clone()
            }
        })
        .collect()
}

fn c_binary_op(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        AndAnd | AndKw => "&&",
        OrOr | OrKw => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        // Temporal / membership operators have no direct C++ equivalent — they
        // shouldn't appear in v0 cpp_tb input. Emit a placeholder rather than
        // fail hard so the whole emit doesn't abort on one stray operator.
        PipeImplies | PipeImpliesNext | Throughout | Within | Intersect | In | Inside => {
            "/* unsupported-op */ ,"
        }
    }
}

/// Constant-fold a base/size expression to `u64` for the addrmap
/// overlap check. Handles plain integer literals only (hex/decimal/
/// underscored). Non-literal expressions fall back to `None` — the
/// overlap check skips pairs whose bounds aren't computable, which
/// keeps the static analysis honest without surfacing false
/// positives.
fn fold_int_literal(e: &Expr) -> Option<u64> {
    match &*e.kind {
        ExprKind::Int(s) => parse_int_str(s),
        _ => None,
    }
}

fn parse_int_str(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    if let Some(idx) = cleaned.find('\'') {
        let (_, tail) = cleaned.split_at(idx + 1);
        let kind = tail.chars().next()?;
        let digits: &str = &tail[1..];
        match kind {
            'h' | 'H' => u64::from_str_radix(digits, 16).ok(),
            'b' | 'B' => u64::from_str_radix(digits, 2).ok(),
            'o' | 'O' => u64::from_str_radix(digits, 8).ok(),
            _ => digits.parse::<u64>().ok(),
        }
    } else if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        u64::from_str_radix(bin, 2).ok()
    } else {
        cleaned.parse::<u64>().ok()
    }
}

/// Validates `alias of` targets: target instance must exist in the
/// same addrmap, must not itself be aliased (chained aliases are
/// out of scope for Phase 1g), and must have the same regblock
/// type as the alias (otherwise the shared mirror would be a
/// type error).
fn check_addrmap_aliases(a: &AddrmapDecl) -> Option<String> {
    for inst in &a.instances {
        let Some(target_name) = &inst.alias_of else {
            continue;
        };
        let target = a.instances.iter().find(|i| i.name.name == target_name.name);
        let Some(target) = target else {
            return Some(format!(
                "addrmap `{}`: instance `{}` aliases `{}`, but no such instance exists in this addrmap",
                a.name.name, inst.name.name, target_name.name,
            ));
        };
        if target.alias_of.is_some() {
            return Some(format!(
                "addrmap `{}`: instance `{}` aliases `{}`, which is itself an alias — chained aliases are not supported",
                a.name.name, inst.name.name, target_name.name,
            ));
        }
        if target.regblock_ty.name != inst.regblock_ty.name {
            return Some(format!(
                "addrmap `{}`: instance `{}` (type `{}`) aliases `{}` (type `{}`) — alias target must share the regblock type",
                a.name.name, inst.name.name, inst.regblock_ty.name,
                target_name.name, target.regblock_ty.name,
            ));
        }
    }
    None
}

/// Walks an addrmap's instance list and returns an error message
/// when any two sized instances have overlapping windows. Windows
/// are half-open `[base, base + size)`. Skips pairs where either
/// instance lacks `size`, or where either instance aliases the
/// other (or both alias the same target — RFC §4 explicitly
/// permits this).
fn pair_is_aliased(a: &InstanceDecl, b: &InstanceDecl) -> bool {
    if let Some(t) = &a.alias_of {
        if t.name == b.name.name {
            return true;
        }
    }
    if let Some(t) = &b.alias_of {
        if t.name == a.name.name {
            return true;
        }
    }
    // Both alias the same third instance.
    match (&a.alias_of, &b.alias_of) {
        (Some(ta), Some(tb)) if ta.name == tb.name => true,
        _ => false,
    }
}

fn check_addrmap_overlap(a: &AddrmapDecl) -> Option<String> {
    let sized: Vec<(usize, &InstanceDecl, u64, u64)> = a
        .instances
        .iter()
        .enumerate()
        .filter_map(|(i, inst)| {
            let base = fold_int_literal(&inst.base_addr)?;
            let size = fold_int_literal(inst.size.as_ref()?)?;
            Some((i, inst, base, base.saturating_add(size)))
        })
        .collect();
    for w in 0..sized.len() {
        for x in (w + 1)..sized.len() {
            let (_i_a, a_inst, a_lo, a_hi) = &sized[w];
            let (_i_b, b_inst, b_lo, b_hi) = &sized[x];
            if pair_is_aliased(a_inst, b_inst) {
                continue;
            }
            // Half-open overlap test.
            if *a_lo < *b_hi && *b_lo < *a_hi {
                return Some(format!(
                    "addrmap `{}`: instance `{}` [0x{:x}, 0x{:x}) overlaps instance `{}` [0x{:x}, 0x{:x})",
                    a.name.name,
                    a_inst.name.name, a_lo, a_hi,
                    b_inst.name.name, b_lo, b_hi,
                ));
            }
        }
    }
    None
}

/// Desugar `impl <name> for <TbType>` tests into the classic
/// `test <name>` form (docs/test-ergonomics.md §3.3). For each test
/// with `for_testbench: Some(tb)`:
///
/// 1. Find the bound testbench's declaration (in `file.items`).
/// 2. Classify its fields:
///    - First named-type field whose type isn't a known HARC
///      construct → the DUT (e.g. `dut : Top`). Synthesize a test-
///      scope `let dut : <SVType>` so the existing Verilator-init
///      path picks up the allocation. (Other DUT-typed fields are
///      currently out of scope — multi-DUT testbenches will need a
///      separate plumbing pass.)
///    - All fields participate in the bare-name-rewrite set.
/// 3. Synthesize a test-scope `let _tb : <TbType>` so the existing
///    component-instantiation path constructs the testbench struct.
/// 4. Synthesize `_tb.dut = dut` at the start of the run block so
///    the testbench's DUT pointer is wired to the test-scope let.
/// 5. Walk every Stmt / Expr in the test items and rewrite bare
///    `Ident(name)` references where `name` matches a testbench
///    field (other than `dut`) or a testbench helper method to
///    `_tb.<name>` field-access or `_tb.<name>(...)` call,
///    respectively. `dut` keeps its bare form — it refers to the
///    synthesized test-scope let, which is the same underlying
///    VTop instance as `_tb.dut` (one allocation, two pointers).
///
/// Tests with `for_testbench: None` (classic form) pass through
/// unchanged.
fn desugar_impl_for_test_in_file(file: &SourceFile) -> SourceFile {
    // Index components by name so the desugarer can resolve the
    // bound testbench's field list without re-walking the file.
    let mut components: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    for it in &file.items {
        match it {
            Item::Env(c) | Item::Agent(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                components.insert(c.name.name.clone(), c.clone());
            }
            _ => {}
        }
    }
    let transactors: std::collections::HashMap<String, TransactorDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) => Some((t.name.name.clone(), t.clone())),
            _ => None,
        })
        .collect();
    let scoreboards: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Scoreboard(c) => Some(c.name.name.clone()),
            _ => None,
        })
        .collect();
    let buses: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Bus(b) => Some(b.name.name.clone()),
            _ => None,
        })
        .collect();
    let covergroups: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Covergroup(g) => Some(g.name.name.clone()),
            _ => None,
        })
        .collect();
    let regblocks: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Regblock(r) => Some(r.name.name.clone()),
            _ => None,
        })
        .collect();
    let addrmaps: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Addrmap(a) => Some(a.name.name.clone()),
            _ => None,
        })
        .collect();

    let mut out = file.clone();
    for it in out.items.iter_mut() {
        let Item::Test(t) = it else { continue };
        let Some(tb_ident) = t.for_testbench.clone() else {
            continue;
        };
        let Some(tb) = components.get(&tb_ident.name) else {
            // Bound testbench not found — leave as is; the main
            // pipeline will surface a sensible error when nothing
            // resolves `_tb`'s type.
            continue;
        };

        // Classify testbench fields.
        let mut dut_field: Option<(String, TypeExpr)> = None;
        let mut field_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut field_is_pointer: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for ci in &tb.items {
            if let ComponentItem::Field(f) = ci {
                field_names.insert(f.name.name.clone());
                if let TypeExpr::Named { name, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    let is_harc = components.contains_key(simple)
                        || transactors.contains_key(simple)
                        || scoreboards.contains(simple)
                        || buses.contains(simple)
                        || covergroups.contains(simple)
                        || regblocks.contains(simple)
                        || addrmaps.contains(simple);
                    if !is_harc {
                        // Treat as a DUT pointer (Verilator-named SV
                        // module type). First wins for the synthesized
                        // `let dut : ...`.
                        if dut_field.is_none() {
                            dut_field = Some((f.name.name.clone(), f.ty.clone()));
                        }
                        field_is_pointer.insert(f.name.name.clone());
                    }
                }
            }
        }

        // Method names — both `hookable` and `function` items.
        let mut method_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for ci in &tb.items {
            if let ComponentItem::Hookable(h) = ci {
                method_names.insert(h.name.name.clone());
            }
        }

        // Build the rewriter's "skip set" — names that must NOT be
        // rewritten because they shadow testbench fields at test
        // scope. `dut` always shadows (we synthesize `let dut : ...`
        // for it). Any user-declared `let X` at test scope also
        // shadows — capture them up-front.
        let mut shadow: std::collections::HashSet<String> = std::collections::HashSet::new();
        shadow.insert("dut".into());
        shadow.insert("_tb".into());
        for ti in &t.items {
            if let TestItem::Let(l) = ti {
                shadow.insert(l.name.name.clone());
            }
        }

        // Rewrite every Stmt / Expr in the test body.
        for ti in t.items.iter_mut() {
            match ti {
                TestItem::Stmt(s) => rewrite_stmt_for_impl(
                    s,
                    &field_names,
                    &method_names,
                    &field_is_pointer,
                    &shadow,
                ),
                TestItem::Scope(sc) => {
                    if let Some(b) = sc.run.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                        );
                    }
                    if let Some(b) = sc.setup.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                        );
                    }
                    if let Some(b) = sc.check.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                        );
                    }
                    if let Some(b) = sc.teardown.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                        );
                    }
                }
                TestItem::Phase(_, b) => {
                    rewrite_block_for_impl(
                        b,
                        &field_names,
                        &method_names,
                        &field_is_pointer,
                        &shadow,
                    );
                }
                _ => {}
            }
        }

        // Prepend synthesized lets. Order: `let dut : Top` (so the
        // Verilator-init path sees it as a top-level DUT pointer),
        // then `let _tb : TopTb`. Inserted at the head of items so
        // they win over any user-declared lets that happen to shadow
        // (a defensive choice; today shadowing is also a HARC error
        // via duplicate-let detection).
        let mut prefix: Vec<TestItem> = Vec::new();
        if let Some((_dut_name, dut_ty)) = &dut_field {
            // Synthesize: `let dut : <SVType>`
            prefix.push(TestItem::Let(LetStmt {
                name: Ident {
                    name: "dut".into(),
                    span: tb_ident.span,
                },
                ty: Some(dut_ty.clone()),
                value: None,
                bind: false,
                probes: Vec::new(),
                bind_remap: Vec::new(),
                span: tb_ident.span,
            }));
        }
        // Synthesize: `let _tb : <TbType>` — default-constructed
        // through the existing component-let path.
        prefix.push(TestItem::Let(LetStmt {
            name: Ident {
                name: "_tb".into(),
                span: tb_ident.span,
            },
            ty: Some(TypeExpr::Named {
                name: Path {
                    segments: vec![tb_ident.clone()],
                    span: tb_ident.span,
                },
                generics: Vec::new(),
                mode: None,
                span: tb_ident.span,
            }),
            value: None,
            bind: false,
            probes: Vec::new(),
            bind_remap: Vec::new(),
            span: tb_ident.span,
        }));

        // Splice synthesized lets at the head, preserving the rest
        // of the items in original order.
        let original: Vec<TestItem> = std::mem::take(&mut t.items);
        t.items = prefix;
        for ti in original {
            // Inject `_tb.dut = dut` as the first stmt of the
            // run block so the wiring happens before any user code.
            if dut_field.is_some() {
                if let TestItem::Scope(sc) = &ti {
                    if let Some(run) = &sc.run {
                        let mut sc = sc.clone();
                        let mut new_stmts = Vec::with_capacity(run.stmts.len() + 1);
                        new_stmts.push(make_wire_dut_stmt(tb_ident.span));
                        new_stmts.extend(run.stmts.iter().cloned());
                        sc.run = Some(Block {
                            stmts: new_stmts,
                            span: run.span,
                        });
                        t.items.push(TestItem::Scope(sc));
                        continue;
                    }
                }
            }
            t.items.push(ti);
        }

        // Mark as desugared so any downstream consumer (pretty-
        // printer for diagnostics, etc.) sees the classic shape.
        t.for_testbench = None;
    }
    out
}

/// Build the synthetic `_tb.dut = dut` statement that wires the
/// testbench's DUT pointer to the test-scope `let dut : ...`. Same
/// shape as a user-written assignment, so it threads through
/// `emit_stmt` unchanged.
fn make_wire_dut_stmt(span: Span) -> Stmt {
    let _tb = Expr {
        kind: Box::new(ExprKind::Ident(Ident {
            name: "_tb".into(),
            span,
        })),
        span,
    };
    let _tb_dut = Expr {
        kind: Box::new(ExprKind::Field {
            target: _tb,
            name: Ident {
                name: "dut".into(),
                span,
            },
        }),
        span,
    };
    let dut = Expr {
        kind: Box::new(ExprKind::Ident(Ident {
            name: "dut".into(),
            span,
        })),
        span,
    };
    Stmt {
        kind: StmtKind::Assign {
            target: _tb_dut,
            value: dut,
        },
        span,
    }
}

/// Walk a block and rewrite bare-ident references that match a
/// testbench field or method to `_tb.<name>`. See
/// `desugar_impl_for_test_in_file` for the rewrite rules.
fn rewrite_block_for_impl(
    b: &mut Block,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) {
    for s in b.stmts.iter_mut() {
        rewrite_stmt_for_impl(s, fields, methods, pointers, shadow);
    }
}

fn rewrite_stmt_for_impl(
    s: &mut Stmt,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) {
    match &mut s.kind {
        StmtKind::Let(l) => {
            if let Some(v) = l.value.as_mut() {
                rewrite_expr_for_impl(v, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            rewrite_expr_for_impl(target, fields, methods, pointers, shadow);
            rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
        }
        StmtKind::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
        StmtKind::For(f) => {
            rewrite_expr_for_impl(&mut f.iter, fields, methods, pointers, shadow);
            rewrite_block_for_impl(&mut f.body, fields, methods, pointers, shadow);
        }
        StmtKind::Repeat(r) => {
            rewrite_expr_for_impl(&mut r.count, fields, methods, pointers, shadow);
            rewrite_block_for_impl(&mut r.body, fields, methods, pointers, shadow);
        }
        StmtKind::Loop(b) => rewrite_block_for_impl(b, fields, methods, pointers, shadow),
        StmtKind::While { cond, body, .. } => {
            rewrite_expr_for_impl(cond, fields, methods, pointers, shadow);
            rewrite_block_for_impl(body, fields, methods, pointers, shadow);
        }
        StmtKind::If(ifs) => {
            rewrite_expr_for_impl(&mut ifs.cond, fields, methods, pointers, shadow);
            rewrite_block_for_impl(&mut ifs.then_block, fields, methods, pointers, shadow);
            for (c, b) in ifs.elsifs.iter_mut() {
                rewrite_expr_for_impl(c, fields, methods, pointers, shadow);
                rewrite_block_for_impl(b, fields, methods, pointers, shadow);
            }
            if let Some(b) = ifs.else_block.as_mut() {
                rewrite_block_for_impl(b, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Fork(fk) => {
            for b in fk.branches.iter_mut() {
                rewrite_block_for_impl(b, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
            for b in blocks.iter_mut() {
                rewrite_block_for_impl(b, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Select(arms) => {
            for a in arms.iter_mut() {
                rewrite_expr_for_impl(&mut a.event, fields, methods, pointers, shadow);
                rewrite_block_for_impl(&mut a.action, fields, methods, pointers, shadow);
            }
        }
        StmtKind::On(h) => {
            rewrite_expr_for_impl(&mut h.event, fields, methods, pointers, shadow);
            rewrite_block_for_impl(&mut h.body, fields, methods, pointers, shadow);
        }
        StmtKind::After { duration, body, .. } => {
            rewrite_expr_for_impl(duration, fields, methods, pointers, shadow);
            rewrite_block_for_impl(body, fields, methods, pointers, shadow);
        }
        StmtKind::Wait { duration, .. } => {
            rewrite_expr_for_impl(duration, fields, methods, pointers, shadow);
        }
        StmtKind::WaitUntil {
            conditions,
            timeout,
            ..
        } => {
            for c in conditions.iter_mut() {
                rewrite_expr_for_impl(c, fields, methods, pointers, shadow);
            }
            if let Some(t) = timeout.as_mut() {
                rewrite_expr_for_impl(&mut t.cycles, fields, methods, pointers, shadow);
                if let Some(m) = t.message.as_mut() {
                    rewrite_expr_for_impl(m, fields, methods, pointers, shadow);
                }
            }
        }
        StmtKind::Yield(e) | StmtKind::Release(e) => {
            rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
        }
        StmtKind::Return(opt) => {
            if let Some(e) = opt.as_mut() {
                rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
            if let Some(ex) = v.expr.as_mut() {
                rewrite_expr_for_impl(ex, fields, methods, pointers, shadow);
            }
            if let Some(else_fail) = v.else_fail.as_mut() {
                rewrite_expr_for_impl(else_fail, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Randomize {
            target, with_body, ..
        } => {
            rewrite_expr_for_impl(target, fields, methods, pointers, shadow);
            for e in with_body.iter_mut() {
                rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Log { args, .. } | StmtKind::LogF { args, .. } => {
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
                    }
                }
            }
        }
        StmtKind::Emit { name, args, .. } => {
            // Multi-segment `emit X.Y(t)` lowers verbatim as
            // `X.Y(...)` in the codegen at StmtKind::Emit handling
            // (see lower_emit). If the head segment is a testbench
            // field, prepend `_tb` so the lowered C++ resolves
            // through the testbench struct.
            if let Some(head) = name.segments.first() {
                if fields.contains(&head.name) && !shadow.contains(&head.name) {
                    name.segments.insert(
                        0,
                        Ident {
                            name: "_tb".into(),
                            span: head.span,
                        },
                    );
                }
            }
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
                    }
                }
            }
        }
        StmtKind::Fail { msg, .. } => {
            rewrite_expr_for_impl(msg, fields, methods, pointers, shadow);
        }
        StmtKind::Apply(_) | StmtKind::Break { .. } | StmtKind::Continue { .. } => {}
    }
}

fn rewrite_expr_for_impl(
    e: &mut Expr,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    _pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) {
    match e.kind.as_mut() {
        ExprKind::Ident(id) => {
            // Bare ident matching a testbench field (non-shadowed)
            // → `_tb.<id>`.
            if fields.contains(&id.name) && !shadow.contains(&id.name) {
                let new_id = id.clone();
                let span = e.span;
                let inner = Expr {
                    kind: Box::new(ExprKind::Ident(Ident {
                        name: "_tb".into(),
                        span,
                    })),
                    span,
                };
                *e.kind = ExprKind::Field {
                    target: inner,
                    name: new_id,
                };
            }
        }
        ExprKind::Call { callee, args } => {
            // Bare-name method call `<m>(args)` where `<m>` is a
            // testbench helper (function/hookable) and not shadowed
            // → rewrite callee to `_tb.<m>` so the existing
            // `resolve_component_method_call` dispatcher picks it up.
            if let ExprKind::Ident(id) = callee.kind.as_ref() {
                if methods.contains(&id.name) && !shadow.contains(&id.name) {
                    let new_id = id.clone();
                    let span = callee.span;
                    let inner = Expr {
                        kind: Box::new(ExprKind::Ident(Ident {
                            name: "_tb".into(),
                            span,
                        })),
                        span,
                    };
                    *callee.kind = ExprKind::Field {
                        target: inner,
                        name: new_id,
                    };
                }
            } else {
                rewrite_expr_for_impl(callee, fields, methods, _pointers, shadow);
            }
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(x) => {
                        rewrite_expr_for_impl(x, fields, methods, _pointers, shadow)
                    }
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
                    }
                }
            }
        }
        ExprKind::Field { target, .. } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
        }
        ExprKind::Index { target, index } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(index, fields, methods, _pointers, shadow);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(hi, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(lo, fields, methods, _pointers, shadow);
        }
        ExprKind::Cast { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::Send { target, value } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
        }
        ExprKind::Unary { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            rewrite_expr_for_impl(lhs, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(rhs, fields, methods, _pointers, shadow);
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            rewrite_expr_for_impl(cond, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(then_branch, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(else_branch, fields, methods, _pointers, shadow);
        }
        ExprKind::HashHash { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::SeqRepeat { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo.as_mut() {
                rewrite_expr_for_impl(lo, fields, methods, _pointers, shadow);
            }
            if let Some(hi) = hi.as_mut() {
                rewrite_expr_for_impl(hi, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::SetLit(es) => {
            for x in es.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::SystemCall { args, .. } => {
            for x in args.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::Randomize {
            target, with_body, ..
        } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            for x in with_body.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::DistDirective { target, .. } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
        }
        ExprKind::Paren(x) => rewrite_expr_for_impl(x, fields, methods, _pointers, shadow),
        ExprKind::NamedArg { value, .. } => {
            rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
        }
        ExprKind::StructLit { fields: nfs, .. } => {
            for nf in nfs.iter_mut() {
                rewrite_expr_for_impl(&mut nf.value, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, .. } => {
            rewrite_expr_for_impl(lhs, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(rhs, fields, methods, _pointers, shadow);
        }
        ExprKind::Solve { args, .. } => {
            for x in args.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::Membership { expr, set } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(set, fields, methods, _pointers, shadow);
        }
        ExprKind::String(s) => {
            // Rewrite testbench-field references inside `${...}`
            // interpolations. Without this, `fail("count = ${env.sb}")`
            // emits raw `env.sb` after the desugarer has already
            // rewritten the rest of the AST, producing a compile-time
            // "undeclared identifier" in C++. Textual rewrite is
            // safe — `${...}` spans don't nest in v0.
            *s = rewrite_interp_in_string(s, fields, methods, shadow);
        }
        ExprKind::DistLit(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::Bool(_)
        | ExprKind::ImplicitSelf => {}
    }
}

/// Scan a string literal for `${...}` interpolation segments and
/// prepend `_tb.` to any segment whose leading identifier matches
/// a testbench field (or method that's been folded into scope) and
/// isn't shadowed. Operates textually so it can't get confused by
/// quote-escaping nuances inside the expression — `${...}` doesn't
/// nest in v0.
fn rewrite_interp_in_string(
    s: &str,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for the start of a `${` interpolation (not preceded by
        // a backslash). v0 doesn't ship a fancier escape grammar, so
        // a bare `${` is always an interpolation.
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            out.push_str("${");
            i += 2;
            // Capture up to the matching `}`.
            let start = i;
            while i < bytes.len() && bytes[i] != b'}' {
                i += 1;
            }
            let segment = &s[start..i];
            // Extract the leading identifier (before any `.`, `:`,
            // `[`, or whitespace). The format-spec suffix `:<fmt>` is
            // preserved unchanged so widthhex / decimal hints
            // continue to work.
            let head_end = segment
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(segment.len());
            let head = &segment[..head_end];
            if !head.is_empty()
                && (fields.contains(head) || methods.contains(head))
                && !shadow.contains(head)
            {
                out.push_str("_tb.");
            }
            out.push_str(segment);
            // Consume the closing `}` (if present — malformed
            // interpolations pass through untouched).
            if i < bytes.len() {
                out.push('}');
                i += 1;
            }
        } else {
            out.push(s.as_bytes()[i] as char);
            i += 1;
        }
    }
    out
}

/// Enforce: a transactor's always-on body (anything not under
/// `when active { ... }`) must not drive DUT signals. Drive-capable
/// hookables / on-handlers must live inside `when active`.
///
/// Rationale (spec §8.1): a `passive` instance literally has its
/// `when active` body elided at codegen (see
/// `synth_component_from_transactor`). If drive code lived in the
/// always-on portion, a passive instance would still emit that code
/// and could end up wiring an observer to the bus — exactly the
/// footgun this check prevents. The common block-level→chip-level
/// TB reuse pattern (passive instance at chip level monitoring a bus
/// the chip already drives) silently miscompiles otherwise.
///
/// "Drive" is:
/// - assignment whose LHS is a field-access chain rooted at a
///   non-HARC-inner-typed field (treated as a DUT pointer), or
///   rooted at the implicit `bus` alias of a `bound to BusType`
///   transactor.
/// - call to `<bus>.<ch>.send(...)` or `<bus>.<ch>.recv()` — both
///   drive the channel's valid (send) or ready (recv) line.
/// - `release <expr>` — pairs with `probe force` writes.
/// - `<lhs> <- <rhs>` channel-send statement on a DUT-rooted target.
///
/// Returns an error message string on the first violation (caller
/// converts to `EmitError`).
fn check_transactor_no_drive_in_always_on_body(
    t: &TransactorDecl,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    components: &std::collections::HashMap<String, ComponentDecl>,
    scoreboards: &std::collections::HashSet<String>,
    covergroups: &std::collections::HashMap<String, CovergroupDecl>,
    buses: &std::collections::HashMap<String, BusDecl>,
    regblocks: &std::collections::HashMap<String, RegblockDecl>,
    addrmaps: &std::collections::HashMap<String, AddrmapDecl>,
) -> Option<String> {
    // 1) Decide which of T's fields are HARC inner constructs (and so
    //    safe to read/write at the field level) versus DUT pointers
    //    (member-write counts as a drive). Builtins (uint/sint/bool/
    //    queue/event/...) are excluded — they're plain inner state.
    let mut dut_field_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for it in &t.items {
        if let ComponentItem::Field(f) = it {
            let TypeExpr::Named { name, .. } = &f.ty else {
                continue; // builtin / event / queue → not a DUT pointer
            };
            let Some(simple) = name.segments.last().map(|s| s.name.as_str()) else {
                continue;
            };
            let is_harc_inner = transactors.contains_key(simple)
                || components.contains_key(simple)
                || scoreboards.contains(simple)
                || covergroups.contains_key(simple)
                || regblocks.contains_key(simple)
                || addrmaps.contains_key(simple)
                || buses.contains_key(simple);
            if !is_harc_inner {
                dut_field_names.insert(f.name.name.clone());
            }
        }
    }
    let is_bound_to_bus = t.bound_to.is_some();

    // 2) Walk every hookable + on-handler in the always-on body
    //    (skipping `t.when_active`, which is the legitimate home for
    //    drive code).
    for it in &t.items {
        match it {
            ComponentItem::Hookable(h) => {
                if let Some(violation) =
                    find_drive_in_block(&h.body, &dut_field_names, is_bound_to_bus)
                {
                    return Some(format!(
                        "transactor `{}` hookable `{}` drives a DUT signal ({}) from the always-on body. \
                         Passive instances would still execute this code. Move the hookable into a \
                         `when active ... end when` block, or remove the drive. See spec §8.1.",
                        t.name.name, h.name.name, violation,
                    ));
                }
            }
            ComponentItem::OnHandler(h) => {
                if let Some(violation) =
                    find_drive_in_block(&h.body, &dut_field_names, is_bound_to_bus)
                {
                    return Some(format!(
                        "transactor `{}` `on`-handler drives a DUT signal ({}) from the always-on body. \
                         Passive instances would still execute this code. Move the handler into a \
                         `when active ... end when` block, or remove the drive. See spec §8.1.",
                        t.name.name, violation,
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// Returns the offending source snippet describing the first drive
/// statement found in `block`, or `None` if the block is drive-free.
fn find_drive_in_block(
    block: &Block,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    for s in &block.stmts {
        if let Some(v) = find_drive_in_stmt(s, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    None
}

fn find_drive_in_stmt(
    s: &Stmt,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    match &s.kind {
        StmtKind::Assign { target, value: _ } | StmtKind::Send { target, value: _ } => {
            if let Some(root) = expr_root_ident(target) {
                if has_field_access(target) {
                    if dut_fields.contains(&root) {
                        return Some(format!("`{}` = ...", expr_source_str(target)));
                    }
                    if bound_to_bus && root == "bus" {
                        return Some(format!("`{}` = ...", expr_source_str(target)));
                    }
                }
            }
            None
        }
        StmtKind::Release(e) => Some(format!("release {}", expr_source_str(e))),
        StmtKind::Expr(e) => find_drive_in_expr(e, dut_fields, bound_to_bus),
        StmtKind::Let(l) => l
            .value
            .as_ref()
            .and_then(|v| find_drive_in_expr(v, dut_fields, bound_to_bus)),
        StmtKind::For(f) => find_drive_in_block(&f.body, dut_fields, bound_to_bus),
        StmtKind::Repeat(r) => find_drive_in_block(&r.body, dut_fields, bound_to_bus),
        StmtKind::Loop(b) => find_drive_in_block(b, dut_fields, bound_to_bus),
        StmtKind::While { body, .. } => find_drive_in_block(body, dut_fields, bound_to_bus),
        StmtKind::If(ifs) => find_drive_in_if(ifs, dut_fields, bound_to_bus),
        StmtKind::Fork(fk) => fk
            .branches
            .iter()
            .find_map(|b| find_drive_in_block(b, dut_fields, bound_to_bus)),
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => blocks
            .iter()
            .find_map(|b| find_drive_in_block(b, dut_fields, bound_to_bus)),
        StmtKind::Select(arms) => arms
            .iter()
            .find_map(|a| find_drive_in_block(&a.action, dut_fields, bound_to_bus)),
        StmtKind::On(h) => find_drive_in_block(&h.body, dut_fields, bound_to_bus),
        StmtKind::After { body, .. } => find_drive_in_block(body, dut_fields, bound_to_bus),
        _ => None,
    }
}

fn find_drive_in_if(
    ifs: &IfStmt,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    if let Some(v) = find_drive_in_block(&ifs.then_block, dut_fields, bound_to_bus) {
        return Some(v);
    }
    for (_cond, body) in &ifs.elsifs {
        if let Some(v) = find_drive_in_block(body, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    if let Some(else_b) = &ifs.else_block {
        if let Some(v) = find_drive_in_block(else_b, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    None
}

fn find_drive_in_expr(
    e: &Expr,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    // The only expression-position drive we flag is a method call
    // `<bus>.<ch>.send(...)` or `<bus>.<ch>.recv()` — both drive a
    // handshake line.
    if let ExprKind::Call { callee, args } = &*e.kind {
        if let ExprKind::Field { target, name } = &*callee.kind {
            if (name.name == "send" || name.name == "recv") && bound_to_bus {
                if let ExprKind::Field {
                    target: inner,
                    name: _,
                } = &*target.kind
                {
                    if let ExprKind::Ident(id) = &*inner.kind {
                        if id.name == "bus" {
                            return Some(format!("{}(...)", expr_source_str(callee)));
                        }
                    }
                }
            }
        }
        // Recurse into args in case a deeply-nested call drives.
        for a in args {
            let arg_expr = match a {
                CallArg::Expr(x) => x,
                CallArg::Named { value, .. } => value,
            };
            if let Some(v) = find_drive_in_expr(arg_expr, dut_fields, bound_to_bus) {
                return Some(v);
            }
        }
    }
    None
}

fn expr_root_ident(e: &Expr) -> Option<String> {
    let mut cur = e;
    loop {
        match &*cur.kind {
            ExprKind::Ident(id) => return Some(id.name.clone()),
            ExprKind::Field { target, .. } => cur = target,
            ExprKind::Index { target, .. } => cur = target,
            ExprKind::Paren(inner) => cur = inner,
            _ => return None,
        }
    }
}

fn has_field_access(e: &Expr) -> bool {
    matches!(&*e.kind, ExprKind::Field { .. })
        || matches!(&*e.kind, ExprKind::Index { target, .. } if has_field_access(target))
        || matches!(&*e.kind, ExprKind::Paren(inner) if has_field_access(inner))
}

/// Width in bits of a RAL field declared as `field name : <ty>`.
/// `bit` / `bool` collapse to 1; `uint<N>` / `sint<N>` / `bits<N>` /
/// `UInt<N>` / `SInt<N>` use the type argument. Falls back to 1 for
/// shapes the parser shouldn't have accepted (caller reports a clean
/// error path).
fn field_bit_width(t: &TypeExpr) -> u32 {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => 1,
            BuiltinTy::UInt
            | BuiltinTy::SInt
            | BuiltinTy::Bits
            | BuiltinTy::UIntCap
            | BuiltinTy::SIntCap => type_arg_width(args).unwrap_or(1),
            _ => 1,
        },
        _ => 1,
    }
}

/// Right-aligned bit mask for a `width`-bit field, suitable for emit
/// as `0x{mask:x}u`. Clamped at 32 bits because Phase 1b fields cap at
/// register width 32 (mirror is `uint32_t`); wider fields are a
/// downstream extension along with the wider mirror types.
fn field_mask_literal(width: u32) -> u64 {
    if width >= 32 {
        0xFFFFFFFFu64
    } else {
        (1u64 << width) - 1
    }
}

/// C++ unsigned integer type wide enough to hold a register of `width`
/// bits. Phase 1a caps at 64; wider registers (e.g. AES blocks) would
/// need the `harc_rt::harc_u128` story used elsewhere in this file.
fn mirror_field_c_type(width: u32) -> &'static str {
    match width {
        1..=8 => "uint8_t",
        9..=16 => "uint16_t",
        17..=32 => "uint32_t",
        _ => "uint64_t",
    }
}

/// Render an integer-literal `ExprKind` as a C++ integer literal.
/// Non-integer expressions fall back to `0` so the emitted code still
/// compiles; the parser path that calls this only fires on `Int(...)`
/// today because reset values and addresses are parsed with
/// `parse_expr` and the codegen surfaces dynamic expressions through
/// the main emit path. Phase 1a callers always supply Int kinds.
fn c_int_literal_from(k: &ExprKind) -> String {
    match k {
        ExprKind::Int(s) => c_int_literal(s),
        _ => "0".to_string(),
    }
}

fn c_int_literal(s: &str) -> String {
    // ARCH-style sized literals (`8'hAB`) → C++ `0xAB`. Then the
    // unsized-literal post-pass below catches any >64-bit cases.
    let normalized = if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            'b' | 'B' => format!("0b{digits}"),
            _ => digits,
        }
    } else {
        s.replace('_', "")
    };

    // Hex literals wider than 64 bits (>16 hex digits) overflow C++'s
    // unsigned long long. Lower as a composite `_harc_u128` shifted-OR
    // so the value flows through `harc_assign` / `harc_read` and 128-
    // bit comparisons. Two halves are sufficient for the v0 cap of
    // 128 bits; literals above that are clamped to the low 128 bits.
    let (prefix, digits) = if let Some(d) = normalized.strip_prefix("0x") {
        ("0x", d)
    } else if let Some(d) = normalized.strip_prefix("0X") {
        ("0x", d)
    } else {
        ("", normalized.as_str())
    };
    if !prefix.is_empty() && digits.len() > 16 && digits.len() <= 32 {
        // 65..128 bits — fits in `_harc_u128`. Split into two 64-bit
        // halves and emit the composite shifted-OR.
        let hex_lo_start = digits.len() - 16;
        let lo = &digits[hex_lo_start..];
        let hi = &digits[..hex_lo_start];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    // > 128 bits: handled by `c_wide_lit_words` at the assign / equality
    // call sites — they emit `harc_assign_words` / `harc_eq_words` with
    // an `std::initializer_list<uint32_t>` instead. If we got here with
    // a > 128b literal it means the literal escaped into a context that
    // can't take a word-list (e.g. used as an arithmetic operand);
    // fall back to a clamped composite to keep the output compilable
    // with a clear narrowing warning rather than a crash.
    if !prefix.is_empty() && digits.len() > 32 {
        let lo = &digits[digits.len() - 16..];
        let hi = &digits[digits.len() - 32..digits.len() - 16];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    normalized
}

/// If the integer literal `s` is hex with > 32 hex digits (i.e. >
/// 128 bits), return its decomposition into 32-bit words in LSB-
/// first order — `vec!["0x...", "0x...", ...]`. Each string is a
/// `uint32_t` hex literal. Returns `None` for narrower or non-hex
/// literals; callers fall back to `c_int_literal` for those.
///
/// Used by the assign and equality lowering paths: a literal that
/// can't fit in `_harc_u128` flows through `harc_assign_words` /
/// `harc_eq_words` instead, which take `std::initializer_list<uint32_t>`.
fn c_wide_lit_words(s: &str) -> Option<Vec<String>> {
    let normalized = if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            _ => return None,
        }
    } else {
        s.replace('_', "")
    };
    let hex = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))?;
    if hex.len() <= 32 {
        return None;
    }
    // Split into 8-hex-digit (32-bit) chunks, LSB-first.
    let mut words = Vec::new();
    let mut remaining = hex.len();
    while remaining > 0 {
        let start = remaining.saturating_sub(8);
        let chunk = &hex[start..remaining];
        words.push(format!("0x{chunk}u"));
        remaining = start;
    }
    Some(words)
}

/// Returns true if `e` is an `Int` literal whose value is wider than
/// 128 bits — i.e. the assign / equality call site should route
/// through `harc_assign_words` / `harc_eq_words` with the word-list
/// from `c_wide_lit_words` rather than the `_harc_u128` composite
/// from `c_int_literal`.
fn is_wide_int_literal(e: &Expr) -> Option<Vec<String>> {
    if let ExprKind::Int(s) = &*e.kind {
        c_wide_lit_words(s)
    } else {
        None
    }
}

/// Parse a HARC-style interpolated string into a printf format string and
/// the list of capture expressions in source form.
///
/// Syntax:
///   `${expr}`         — default decimal: `%lld`
///   `${expr:d}`       — explicit decimal: `%lld`
///   `${expr:x}`       — lowercase hex: `%llx`
///   `${expr:X}`       — uppercase hex: `%llX`
///   `${expr:o}`       — octal: `%llo`
///   `${expr:08x}`     — width + zero-pad: `%08llx` (Python/Rust f-string)
///   `${expr:8d}`      — width + space-pad
///
/// Plain `%` is escaped to `%%`. Every captured expression is widened to
/// `long long` at the call site. v0 limitations: bit-slice `a[7:0]` cannot
/// appear inside `${...}` (the format separator is `:`); strings and chars
/// are not yet supported as interpolation targets — hoist into a let.
/// One captured `${expr:spec}` from a HARC interpolated string.
/// `wide_hex` is `Some((width_hex_digits, upper_case))` when the spec
/// is `:WWx` or `:WWX` with WW > 16 — those route through the
/// `HarcHexBuf128` runtime helper at codegen time so values up to 128
/// bits print in full instead of being truncated to a `long long`.
/// All other specs use the legacy `(long long)(...)` cast.
struct InterpCap {
    expr: String,
    wide_hex: Option<(usize, bool)>,
}

fn process_interp(s: &str) -> (String, Vec<InterpCap>) {
    let mut fmt = String::with_capacity(s.len());
    let mut captures: Vec<InterpCap> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Walk to matching `}` (no nested braces in v0).
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            } // unmatched — bail and keep what we have
            let inner = std::str::from_utf8(&bytes[i + 2..j]).unwrap_or("").trim();
            // Split on the last `:` to extract an optional format spec.
            let (expr_src, spec) = match inner.rfind(':') {
                Some(idx) => (inner[..idx].trim(), inner[idx + 1..].trim()),
                None => (inner, ""),
            };
            let (fmt_token, wide_hex) = translate_fmt_spec(spec);
            captures.push(InterpCap {
                expr: expr_src.to_string(),
                wide_hex,
            });
            fmt.push_str(&fmt_token);
            i = j + 1;
        } else if bytes[i] == b'%' {
            fmt.push_str("%%");
            i += 1;
        } else {
            fmt.push(bytes[i] as char);
            i += 1;
        }
    }
    (fmt, captures)
}

/// Translate a HARC format spec (Python/Rust f-string subset) to a
/// printf conversion specifier. Returns `(token, wide_hex_info)`:
/// - `token` is the printf format substring (e.g. `%08llx` or `%s`).
/// - `wide_hex_info` is `Some((width, upper))` when the spec is hex
///   wider than 16 digits — the caller must emit the value via
///   `HarcHexBuf128` so it prints in full 128-bit precision.
fn translate_fmt_spec(spec: &str) -> (String, Option<(usize, bool)>) {
    if spec.is_empty() {
        return ("%lld".to_string(), None);
    }
    let last = spec.chars().last().unwrap();
    let prefix = &spec[..spec.len() - last.len_utf8()];

    // Pull the leading width digits (after an optional `0` flag) so we
    // can decide whether to route through the wide-hex helper. Examples:
    //   "032x" → width 32, hex. "8d" → width 8, decimal. "x" → width 0.
    let width: usize = {
        let trimmed = prefix.trim_start_matches('0');
        trimmed
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<usize>()
            .unwrap_or(0)
    };

    match last {
        'd' => (format!("%{prefix}lld"), None),
        'o' => (format!("%{prefix}llo"), None),
        'x' | 'X' if width > 16 => {
            // Wide-hex: use `%s` and let the runtime helper format
            // into a stack buffer. Width-pad and zero-pad both come
            // from the helper itself.
            ("%s".to_string(), Some((width, last == 'X')))
        }
        'x' => (format!("%{prefix}llx"), None),
        'X' => (format!("%{prefix}llX"), None),
        _ => ("%lld".to_string(), None),
    }
}

/// Convert a HARC time literal (`5ns`, `20ns`, `100ps`, `2us`, `1ms`, `1s`)
/// to picoseconds. Returns `Err(msg)` on unrecognised units. The `cycles`
/// suffix is rejected here — clock periods must be wall-clock time.
fn time_literal_to_ps(s: &str) -> Result<i64, String> {
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .collect();
    let unit: String = s.chars().skip(digits.len()).collect();
    let n: i64 = digits
        .replace('_', "")
        .parse()
        .map_err(|_| format!("bad number in time literal `{s}`"))?;
    match unit.as_str() {
        "ps" => Ok(n),
        "ns" => Ok(n * 1_000),
        "us" => Ok(n * 1_000_000),
        "ms" => Ok(n * 1_000_000_000),
        "s" => Ok(n * 1_000_000_000_000),
        other => Err(format!(
            "unsupported time unit `{other}` in `{s}` (expected ps/ns/us/ms/s)"
        )),
    }
}

/// Recognise a temporal helper call in either form — bare `Call` AST with
/// callee `Ident("past"|"rose"|"fell"|"stable")` (the ARCH-aligned form,
/// what the parser produces today) or the legacy `SystemCall` AST (pre-
/// `$past`-removal). Returns `(kind, inner_arg)` on match.
fn match_temporal_call(e: &Expr) -> Option<(SystemFn, &Expr)> {
    match &*e.kind {
        ExprKind::SystemCall { name, args } => {
            let kind = match name {
                SystemFn::Past => SystemFn::Past,
                SystemFn::Rose => SystemFn::Rose,
                SystemFn::Fell => SystemFn::Fell,
                SystemFn::Stable => SystemFn::Stable,
                _ => return None,
            };
            args.first().map(|a| (kind, a))
        }
        ExprKind::Call { callee, args } => {
            let id = match &*callee.kind {
                ExprKind::Ident(id) => id,
                _ => return None,
            };
            let kind = match id.name.as_str() {
                "past" => SystemFn::Past,
                "rose" => SystemFn::Rose,
                "fell" => SystemFn::Fell,
                "stable" => SystemFn::Stable,
                _ => return None,
            };
            match args.first()? {
                CallArg::Expr(inner) => Some((kind, inner)),
                CallArg::Named { value, .. } => Some((kind, value)),
            }
        }
        _ => None,
    }
}

/// True if an `assert <expr>` should lower to a concurrent (every-cycle)
/// check rather than a one-shot inline check. Per spec §2 LL(1) table:
/// a bare ident referencing a declared property, or any expression
/// containing temporal operators (`|->`, `|=>`, `##N`, `throughout`,
/// `within`, `intersect`) or temporal system calls (`$past`, `$rose`,
/// `$fell`, `$stable`), runs as a concurrent property. Everything else —
/// plain boolean / arithmetic / comparison — is an immediate check.
fn is_concurrent_assertion(
    expr: &Expr,
    properties: &std::collections::HashMap<String, Expr>,
) -> bool {
    if let ExprKind::Ident(id) = &*expr.kind {
        if properties.contains_key(&id.name) {
            return true;
        }
    }
    contains_temporal(expr)
}

fn contains_temporal(e: &Expr) -> bool {
    if match_temporal_call(e).is_some() {
        return true;
    }
    match &*e.kind {
        ExprKind::Binary { op, lhs, rhs } => {
            matches!(
                op,
                BinaryOp::PipeImplies
                    | BinaryOp::PipeImpliesNext
                    | BinaryOp::Throughout
                    | BinaryOp::Within
                    | BinaryOp::Intersect
            ) || contains_temporal(lhs)
                || contains_temporal(rhs)
        }
        ExprKind::SystemCall { args, .. } => args.iter().any(contains_temporal),
        ExprKind::HashHash { .. } | ExprKind::SeqRepeat { .. } => true,
        ExprKind::Field { target, .. } => contains_temporal(target),
        ExprKind::Index { target, index } => contains_temporal(target) || contains_temporal(index),
        ExprKind::BitSlice { target, hi, lo } => {
            contains_temporal(target) || contains_temporal(hi) || contains_temporal(lo)
        }
        ExprKind::Call { callee, args } => {
            contains_temporal(callee)
                || args.iter().any(|a| match a {
                    CallArg::Expr(x) => contains_temporal(x),
                    CallArg::Named { value, .. } => contains_temporal(value),
                })
        }
        ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => contains_temporal(expr),
        ExprKind::Paren(inner) => contains_temporal(inner),
        ExprKind::Send { target, value } => contains_temporal(target) || contains_temporal(value),
        ExprKind::SetLit(items) => items.iter().any(contains_temporal),
        _ => false,
    }
}

/// One temporal SystemCall occurrence ($past/$rose/$fell/$stable) collected
/// from a property body. The codegen allocates a static delay slot per
/// occurrence; references to the call get substituted to the slot.
struct Temporal {
    /// Span of the SystemCall expression itself — used as the substitution
    /// key in `prop_subs`.
    call_span: Span,
    /// Which kind of system call this is.
    kind: SystemFn,
    /// The argument expression — captured each cycle into a `_curN` local.
    inner: Expr,
}

/// Pre-walk a property body and return one `Temporal` entry per
/// $past/$rose/$fell/$stable call. Order is left-to-right depth-first.
fn collect_temporal_occurrences(body: &Expr) -> Vec<Temporal> {
    fn walk(e: &Expr, out: &mut Vec<Temporal>) {
        if let Some((kind, inner)) = match_temporal_call(e) {
            out.push(Temporal {
                call_span: e.span,
                kind,
                inner: inner.clone(),
            });
            // Don't recurse into the argument — nested `past(past(x))` not
            // supported in v0 and would need slot-of-slot accounting.
            return;
        }
        match &*e.kind {
            ExprKind::Field { target, .. } => walk(target, out),
            ExprKind::Index { target, index } => {
                walk(target, out);
                walk(index, out);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                walk(target, out);
                walk(hi, out);
                walk(lo, out);
            }
            ExprKind::Call { callee, args } => {
                walk(callee, out);
                for a in args {
                    if let CallArg::Expr(e) = a {
                        walk(e, out);
                    } else if let CallArg::Named { value, .. } = a {
                        walk(value, out);
                    }
                }
            }
            ExprKind::Cast { expr, .. } => walk(expr, out),
            ExprKind::Send { target, value } => {
                walk(target, out);
                walk(value, out);
            }
            ExprKind::Unary { expr, .. } => walk(expr, out),
            ExprKind::Binary { lhs, rhs, .. } => {
                walk(lhs, out);
                walk(rhs, out);
            }
            ExprKind::Paren(inner) => walk(inner, out),
            ExprKind::SystemCall { args, .. } => {
                for a in args {
                    walk(a, out);
                }
            }
            ExprKind::HashHash { expr, .. } => walk(expr, out),
            ExprKind::SeqRepeat { expr, .. } => walk(expr, out),
            ExprKind::SetLit(items) => {
                for i in items {
                    walk(i, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(body, &mut out);
    out
}

/// Best-effort human label for a property assertion — used in FAIL log
/// lines so the user can identify which check tripped. Prefers the named
/// form (`assert property foo` → "foo"), then falls back to "<inline>".
fn property_label(v: &Verify, raw: &Expr) -> String {
    if let Some(n) = &v.named {
        return n.name.clone();
    }
    if let ExprKind::Ident(id) = &*raw.kind {
        return id.name.clone();
    }
    "<inline>".to_string()
}

fn escape_c(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Render a HARC `Expr` back to source-level text via the pretty
/// printer. Used by `wait until` codegen to label each sub-predicate
/// in the timeout diagnostic with the user's original expression
/// (e.g. `env.agent.idle(100)` rather than a synthetic index).
fn expr_source_str(e: &Expr) -> String {
    let mut buf = String::new();
    crate::pretty::print_expr(&mut buf, e);
    buf
}

fn txn_body_has_keep(items: &[TxnBodyItem]) -> bool {
    items.iter().any(|item| match item {
        TxnBodyItem::Keep(_) => true,
        TxnBodyItem::When(w) => txn_body_has_keep(&w.items),
        TxnBodyItem::Field(_) => false,
    })
}

fn collect_txn_keeps(items: &[TxnBodyItem]) -> Vec<Expr> {
    let mut out = Vec::new();
    collect_txn_keeps_with_guard(items, None, &mut out);
    out
}

fn collect_txn_keeps_with_guard(items: &[TxnBodyItem], guard: Option<Expr>, out: &mut Vec<Expr>) {
    for item in items {
        match item {
            TxnBodyItem::Keep(k) => {
                let expr = match &guard {
                    Some(g) => guarded_keep_expr(g.clone(), k.expr.clone(), k.span),
                    None => k.expr.clone(),
                };
                out.push(expr);
            }
            TxnBodyItem::When(w) => {
                let next_guard = match &guard {
                    Some(g) => and_join(&[g.clone(), w.discriminant.clone()], w.span),
                    None => w.discriminant.clone(),
                };
                collect_txn_keeps_with_guard(&w.items, Some(next_guard), out);
            }
            TxnBodyItem::Field(_) => {}
        }
    }
}

fn guarded_keep_expr(guard: Expr, keep: Expr, span: Span) -> Expr {
    let not_guard = Expr::new(
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr: Expr::new(ExprKind::Paren(guard), span),
        },
        span,
    );
    Expr::new(
        ExprKind::Binary {
            op: BinaryOp::OrOr,
            lhs: not_guard,
            rhs: keep,
        },
        span,
    )
}

/// Combine a list of constraint expressions into one
/// `Binary(AndAnd, …)` chain. Used by `expand_relation_subtree` when a
/// block-form relation appears in a position that expects a single
/// expression (e.g. nested inside another `Binary &&`). Empty input
/// returns a literal `true`. Single-element input returns the element
/// unchanged.
fn and_join(exprs: &[Expr], span: Span) -> Expr {
    if exprs.is_empty() {
        return Expr::new(ExprKind::Bool(true), span);
    }
    let mut iter = exprs.iter().cloned();
    let mut acc = iter.next().unwrap();
    for next in iter {
        let s = acc.span.merge(next.span);
        acc = Expr::new(
            ExprKind::Binary {
                op: BinaryOp::AndAnd,
                lhs: acc,
                rhs: next,
            },
            s,
        );
    }
    acc
}

/// Walk `expr` and replace each `ExprKind::Ident(name)` whose name is
/// in `subst` with the corresponding substitute expression (cloned in
/// place). Used by `expand_relation_calls` to bind each relation's
/// formal parameter to the actual call argument before the body is
/// added to the Z3 solver block.
///
/// The substitute can be any expression — a bare ident (the common
/// case: `R(pkt)` → `pkt`), a field access (`R(env.agent.txn)`), or
/// a deeper subtree. Field-access names (`Field { name, .. }`) are
/// attribute references, not bindings, so they're never substituted.
fn substitute_idents(expr: &Expr, subst: &std::collections::HashMap<String, Expr>) -> Expr {
    let span = expr.span;
    let new_kind: ExprKind = match &*expr.kind {
        ExprKind::Ident(id) => {
            if let Some(replacement) = subst.get(&id.name) {
                // Splice the replacement's *kind* in at the current
                // span. Keeping the original ident's span preserves
                // source attribution in any future diagnostic.
                return Expr::new((*replacement.kind).clone(), span);
            }
            ExprKind::Ident(id.clone())
        }
        ExprKind::Field { target, name } => ExprKind::Field {
            target: substitute_idents(target, subst),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: substitute_idents(target, subst),
            index: substitute_idents(index, subst),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: substitute_idents(target, subst),
            hi: substitute_idents(hi, subst),
            lo: substitute_idents(lo, subst),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: substitute_idents(callee, subst),
            args: args
                .iter()
                .map(|a| match a {
                    CallArg::Expr(e) => CallArg::Expr(substitute_idents(e, subst)),
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: substitute_idents(value, subst),
                    },
                })
                .collect(),
        },
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: substitute_idents(expr, subst),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: substitute_idents(expr, subst),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: substitute_idents(lhs, subst),
            rhs: substitute_idents(rhs, subst),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: substitute_idents(cond, subst),
            then_branch: substitute_idents(then_branch, subst),
            else_branch: substitute_idents(else_branch, subst),
        },
        ExprKind::Paren(inner) => ExprKind::Paren(substitute_idents(inner, subst)),
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: substitute_idents(expr, subst),
            set: substitute_idents(set, subst),
        },
        ExprKind::SetLit(items) => {
            ExprKind::SetLit(items.iter().map(|x| substitute_idents(x, subst)).collect())
        }
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo.as_ref().map(|x| substitute_idents(x, subst)),
            hi: hi.as_ref().map(|x| substitute_idents(x, subst)),
        },
        // Literals, ImplicitSelf, and the spec-§5 / §6 / §8 forms
        // (HashHash, SeqRepeat, DistLit, etc.) don't bind names and
        // aren't expected inside constraint bodies; pass through
        // verbatim. If a relation body happens to contain one of these
        // shapes, the downstream constraint translator will reject it
        // with a clear "not supported" error, same as it would for an
        // un-inlined body.
        other => other.clone(),
    };
    Expr::new(new_kind, span)
}
