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
//! - `log(info, "...")` lowers to `printf` (severity dropped for v0; full
//!   severity / verbosity / id machinery from §7.7 is deferred).
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
pub const THREAD_RT_HEADER: &str =
    include_str!("../../runtime/harc_thread_rt.h");

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
    // Find a single `test` item — the entry point.
    let test = file.items.iter().find_map(|it| match it {
        Item::Test(t) => Some(t),
        _ => None,
    }).ok_or_else(|| EmitError("no `test` declaration found".into()))?;

    // Collect top-level functions — emitted as lambdas inside main() so they
    // can capture `dut` and `tick` lexically.
    let funcs: Vec<&FunctionDecl> = file.items.iter().filter_map(|it| match it {
        Item::Function(f) => Some(f),
        _ => None,
    }).collect();

    // Collect `tseq` declarations — same hoisting strategy as functions,
    // emitted as `std::function`-shaped lambdas returning `std::vector<T>`
    // built up via `yield`.
    let tseqs: Vec<&TseqDecl> = file.items.iter().filter_map(|it| match it {
        Item::Tseq(t) => Some(t),
        _ => None,
    }).collect();

    // Index `domain Foo freq_mhz: N end domain Foo` decls so a `clock X =
    // Foo` reference can resolve N to a wall-clock period (1/N µs → ps).
    let mut domains: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
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

    // Find the `let dut : <Type>` declaration to learn the V-class name.
    // Walk both top-level Let items and Lets nested inside `scope sim` blocks.
    let mut dut_type: Option<&str> = None;
    let mut other_lets: Vec<&LetStmt> = Vec::new();
    let mut bare_stmts: Vec<&Stmt> = Vec::new();
    let mut explicit_run: Option<&Block> = None;
    let mut clocks: Vec<&ClockDecl> = Vec::new();
    for it in &test.items {
        match it {
            TestItem::Let(l) => {
                if l.name.name == "dut" {
                    let ty_name = type_simple_name(l.ty.as_ref()).ok_or_else(|| {
                        EmitError("`let dut : <Type>` must use a simple named type".into())
                    })?;
                    dut_type = Some(ty_name);
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
    let dut_type = dut_type.ok_or_else(|| EmitError(
        "expected `let dut : <Type>` declaration in test body".into()
    ))?;

    // Mixing rule was previously "pick one of scope sim or bare statements";
    // relaxed now to "items emit in declaration order" so a property-extend
    // file can place `assert property X` lines alongside an existing
    // `scope sim/run` block (typically extends provide setup-like code).
    if explicit_run.is_none() && bare_stmts.is_empty() {
        return Err(EmitError(
            "test has no body — add a `scope sim` or bare statements".into(),
        ));
    }
    let _ = (&explicit_run, &bare_stmts); // kept for parser-level error checking

    // Collect transactions + enums + scoreboards + monitors for typed-let
    // emission.
    let mut transactions = std::collections::HashSet::new();
    let mut scoreboards = std::collections::HashSet::new();
    let mut monitors: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    let mut components: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    let mut covergroups: std::collections::HashMap<String, CovergroupDecl> =
        std::collections::HashMap::new();
    let mut buses: std::collections::HashMap<String, BusDecl> =
        std::collections::HashMap::new();
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    let mut enums = std::collections::HashMap::new();
    for it in &file.items {
        match it {
            Item::Scoreboard(c) => {
                scoreboards.insert(c.name.name.clone());
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Monitor(c) => { monitors.insert(c.name.name.clone(), c.clone()); }
            Item::Driver(c) | Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) => {
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Covergroup(g) => { covergroups.insert(g.name.name.clone(), g.clone()); }
            Item::Bus(b) => { buses.insert(b.name.name.clone(), b.clone()); }
            _ => {}
        }
    }
    for it in &file.items {
        match it {
            Item::Transaction(t) => {
                transactions.insert(t.name.name.clone());
                let fields = t.body.iter().filter_map(|it| match it {
                    TxnBodyItem::Field(f) => {
                        let width = match &f.ty {
                            TypeExpr::Builtin { name, args, .. } => match name {
                                BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits
                                | BuiltinTy::UIntCap | BuiltinTy::SIntCap =>
                                    type_arg_width(args).unwrap_or(64),
                                BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => 1,
                                _ => 64,
                            },
                            _ => 64,
                        };
                        Some(TxnFieldInfo {
                            name: f.name.name.clone(),
                            width,
                            non_random: f.non_random,
                        })
                    }
                    _ => None,
                }).collect();
                txn_fields.insert(t.name.name.clone(), fields);
            }
            Item::Enum(e) => { enums.insert(e.name.name.clone(), e.variants.len()); }
            _ => {}
        }
    }

    // Seed let_types from test-level `let` decls so `randomize(t)` works
    // when the let appears at test scope (above any scope sim / bare stmts).
    let mut let_types = std::collections::HashMap::new();
    for l in &other_lets {
        if let Some(s) = type_simple_name(l.ty.as_ref()) {
            let_types.insert(l.name.name.clone(), s.to_string());
        }
    }

    let mut pointer_vars = std::collections::HashSet::new();
    // Test-level `let dut : <NamedType>` is a DUT pointer. Other Named-
    // typed lets are pointer-shaped only if they're not value-struct
    // types (transactions / scoreboards) — those default-construct on
    // the stack and use `.` for field access.
    pointer_vars.insert("dut".to_string());
    for l in &other_lets {
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if transactions.contains(simple)
                || scoreboards.contains(simple)
                || monitors.contains_key(simple)
                || components.contains_key(simple)
                || covergroups.contains_key(simple)
                || buses.contains_key(simple)
            {
                continue;
            }
        }
        if matches!(&l.ty, Some(TypeExpr::Named { .. })) {
            pointer_vars.insert(l.name.name.clone());
        }
    }

    // Build the property name → body table.
    let mut properties = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Property(p) = it {
            properties.insert(p.name.name.clone(), p.body.clone());
        }
    }

    let mut e = Emitter {
        out: String::new(),
        errors: Vec::new(),
        pointer_vars,
        let_types,
        transactions,
        scoreboards,
        monitors,
        components,
        covergroups,
        txn_fields,
        enums,
        properties,
        prop_subs: std::collections::HashMap::new(),
        event_types: std::collections::HashMap::new(),
        field_subs: std::collections::HashMap::new(),
        covers: Vec::new(),
        clock_names: clocks.iter().map(|c| c.name.name.clone()).collect(),
        current_yield_target: None,
        tseq_names: tseqs.iter().map(|t| t.name.name.clone()).collect(),
        buses,
        bus_bindings: std::collections::HashMap::new(),
        in_coroutine: false,
        actor_threads: Vec::new(),
        mt: opts.mt,
    };

    // Header.
    writeln!(e.out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(e.out, "// HARC test: {}", test.name.name).ok();
    writeln!(e.out, "").ok();
    // Disable clang optimization for this file. clang 17+ on Apple
    // Silicon mis-optimizes our `[&]`-capturing C++20 lambda
    // coroutines at `-Os` / `-O2`: closure reference members get
    // folded against the original (freed) stack frame after a
    // suspension, causing SEGV on resume. The pragma is per-file so
    // verilator-generated DUT code (in separate .cpp files) keeps
    // its `-Os` for fast simulation. The TB itself is not perf-
    // critical — most cycles are spent in `dut->eval()` which
    // remains optimized. Revisit when clang ships a fix.
    writeln!(e.out, "#ifdef __clang__").ok();
    writeln!(e.out, "#pragma clang optimize off").ok();
    writeln!(e.out, "#endif").ok();
    writeln!(e.out, "").ok();
    writeln!(e.out, "#include \"V{dut_type}.h\"").ok();
    writeln!(e.out, "#include \"verilated.h\"").ok();
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
    let uses_solver = file_uses_constraint_solver(file);
    if uses_solver {
        writeln!(e.out, "#include <z3++.h>   // randomize(t) with <constraints>").ok();
    }
    writeln!(e.out, "").ok();

    // ── PRNG runtime ──────────────────────────────────────────────────────
    // SplitMix64 — small, fast, pure stdlib. Seed loaded from HARC_SEED.
    writeln!(e.out, "static uint64_t harc_rng_state = 0;").ok();
    writeln!(e.out, "static inline uint64_t harc_rng_next() {{").ok();
    writeln!(e.out, "{INDENT}uint64_t z = (harc_rng_state += 0x9E3779B97F4A7C15ULL);").ok();
    writeln!(e.out, "{INDENT}z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;").ok();
    writeln!(e.out, "{INDENT}z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;").ok();
    writeln!(e.out, "{INDENT}return z ^ (z >> 31);").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "static inline int64_t harc_rng_range(int64_t lo, int64_t hi) {{").ok();
    writeln!(e.out, "{INDENT}if (hi <= lo) return lo;").ok();
    writeln!(e.out, "{INDENT}return lo + (int64_t)(harc_rng_next() % (uint64_t)(hi - lo + 1));").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "static inline uint64_t harc_rng_uint(unsigned width) {{").ok();
    writeln!(e.out, "{INDENT}if (width >= 64) return harc_rng_next();").ok();
    writeln!(e.out, "{INDENT}return harc_rng_next() & ((1ULL << width) - 1);").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "static inline int64_t harc_rng_dist(const std::vector<std::tuple<int64_t,int64_t,int64_t>>& bins) {{").ok();
    writeln!(e.out, "{INDENT}int64_t total = 0; for (auto& b : bins) total += std::get<2>(b);").ok();
    writeln!(e.out, "{INDENT}if (total <= 0) return 0;").ok();
    writeln!(e.out, "{INDENT}int64_t pick = (int64_t)(harc_rng_next() % (uint64_t)total);").ok();
    writeln!(e.out, "{INDENT}int64_t acc = 0;").ok();
    writeln!(e.out, "{INDENT}for (auto& b : bins) {{ acc += std::get<2>(b); if (pick < acc) return harc_rng_range(std::get<0>(b), std::get<1>(b)); }}").ok();
    writeln!(e.out, "{INDENT}return std::get<0>(bins.front());").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "").ok();

    // Tiny FIFO wrapper for `queue<T>` scoreboard fields. Provides pop()
    // returning the front element (std::queue separates front/pop), and
    // empty()/size(). Emitted only when scoreboards exist.
    let any_scoreboard = file.items.iter().any(|it| matches!(it, Item::Scoreboard(_)));
    if any_scoreboard {
        writeln!(e.out, "#include <deque>").ok();
        writeln!(e.out, "template<typename T> struct HarcQueue {{").ok();
        writeln!(e.out, "{INDENT}std::deque<T> _d;").ok();
        writeln!(e.out, "{INDENT}void push(T v) {{ _d.push_back(v); }}").ok();
        writeln!(e.out, "{INDENT}T pop() {{ T v = _d.front(); _d.pop_front(); return v; }}").ok();
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
                if h.payload.is_empty() { continue; }
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
                ).ok();
                writeln!(e.out, "}};").ok();
                writeln!(e.out, "").ok();
            }
        }
    }

    // ── Monitor structs ──────────────────────────────────────────────────
    // Same shape as scoreboards; output `event<T>` fields lower to
    // std::vector<std::function<void(T)>>. The on-handlers don't go in
    // the struct — they're emitted at `let mon : MonName` time.
    for it in &file.items {
        if let Item::Monitor(m) = it {
            e.emit_monitor_struct(m);
        }
    }

    // ── Component structs (driver / agent / env / sequencer).
    // Scoreboards have their own dedicated path above; the rest are
    // plain field-bearing structs. `hookable` methods are emitted
    // separately as free `[&]`-capturing lambdas (below) so the
    // method body sees `dut` / `tick` / `_checkers` from the test scope.
    for it in &file.items {
        match it {
            Item::Driver(c) | Item::Agent(c) | Item::Env(c)
            | Item::Sequencer(c) => {
                e.emit_component_struct(c);
            }
            _ => {}
        }
    }

    // ── Covergroup structs (per-bin counters + sample() + report()) ─────
    for it in &file.items {
        if let Item::Covergroup(g) = it {
            e.emit_covergroup_struct(g);
        }
    }

    writeln!(e.out, "int main(int argc, char** argv) {{").ok();
    writeln!(e.out, "{INDENT}Verilated::commandArgs(argc, argv);").ok();
    writeln!(e.out, "{INDENT}V{dut_type}* dut = new V{dut_type};").ok();
    writeln!(e.out, "{INDENT}int errors = 0;").ok();
    writeln!(e.out, "{INDENT}int cycle_count = 0;").ok();
    writeln!(e.out, "").ok();
    // Seed PRNG from HARC_SEED env (or 1 if unset). Logged after sim_log_line
    // is defined so it lands in sim.log along with normal test output.
    writeln!(e.out, "{INDENT}{{ const char* s = std::getenv(\"HARC_SEED\"); harc_rng_state = s ? std::strtoull(s, nullptr, 0) : 1ULL; }}").ok();
    writeln!(e.out, "").ok();
    // sim.log captures every log()/assert/fail line with cycle + severity
    // prefix. Path is configurable via the HARC_SIM_LOG env var (so the
    // outer harness can put it in the build dir); default `sim.log` in cwd.
    writeln!(e.out, "{INDENT}const char* sim_log_path = std::getenv(\"HARC_SIM_LOG\");").ok();
    writeln!(e.out, "{INDENT}if (!sim_log_path) sim_log_path = \"sim.log\";").ok();
    writeln!(e.out, "{INDENT}FILE* sim_log = std::fopen(sim_log_path, \"w\");").ok();
    writeln!(e.out, "").ok();
    // Concurrent assertion hook — every `assert property <expr>` /
    // `assert property NAME` registers a closure here; tick() invokes the
    // whole list after each `eval()`. Same-cycle (`|->`) and one-cycle
    // (`|=>`) properties run on every primary-clock edge.
    writeln!(e.out, "{INDENT}std::vector<std::function<void()>> _checkers;").ok();
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
            writeln!(e.out, "{INDENT}clocks_.push_back(ClockState{{\"{}\", {half}, {half}, 0, 0}});",
                c.name.name).ok();
            writeln!(e.out, "{INDENT}dut->{} = 0;", c.name.name).ok();
        }
        writeln!(e.out, "").ok();
        writeln!(e.out, "{INDENT}auto eval_clocks_until = [&](long long t_ps) {{").ok();
        writeln!(e.out, "{INDENT}{INDENT}while (now_ps < t_ps) {{").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}long long next = t_ps;").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}for (auto& c : clocks_) if (c.next_edge_ps < next) next = c.next_edge_ps;").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}now_ps = next;").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}for (size_t i = 0; i < clocks_.size(); i++) {{").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}auto& c = clocks_[i];").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}if (c.next_edge_ps == now_ps) {{").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.level = !c.level;").ok();
        // Per-clock signal write — done by name lookup.
        for (idx, c) in clocks.iter().enumerate() {
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == {idx}) dut->{} = c.level;",
                c.name.name).ok();
        }
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.next_edge_ps += c.half_period_ps;").ok();
        // Per-clock rising-edge count (consumed by `wait N cycles on <clock>`).
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (c.level == 1) c.rising_count++;").ok();
        // Primary clock rising edge bumps cycle_count.
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == 0 && c.level == 1) cycle_count++;").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}}}").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}}}").ok();
        writeln!(e.out, "{INDENT}{INDENT}{INDENT}dut->eval();").ok();
        writeln!(e.out, "{INDENT}{INDENT}}}").ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(e.out, "").ok();
        // `tick()` advances by one full primary clock period (one rising
        // edge). Other clocks tick at their natural rate during this span.
        writeln!(e.out, "{INDENT}auto tick = [&]() {{").ok();
        writeln!(e.out, "{INDENT}{INDENT}long long target = now_ps + clocks_[0].half_period_ps * 2;").ok();
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
    writeln!(e.out, "{INDENT}std::unordered_map<std::string, FILE*> log_files;").ok();
    writeln!(e.out, "{INDENT}auto resolve_log_path = [&](const char* path) -> std::string {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}if (path[0] == '/') return std::string(path);").ok();
    writeln!(e.out, "{INDENT}{INDENT}const char* base = std::getenv(\"HARC_LOG_DIR\");").ok();
    writeln!(e.out, "{INDENT}{INDENT}if (base) return std::string(base) + \"/\" + path;").ok();
    writeln!(e.out, "{INDENT}{INDENT}return std::string(path);").ok();
    writeln!(e.out, "{INDENT}}};").ok();
    writeln!(e.out, "{INDENT}auto get_log_file = [&](const char* path) -> FILE* {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::string resolved = resolve_log_path(path);").ok();
    writeln!(e.out, "{INDENT}{INDENT}auto it = log_files.find(resolved);").ok();
    writeln!(e.out, "{INDENT}{INDENT}if (it != log_files.end()) return it->second;").ok();
    writeln!(e.out, "{INDENT}{INDENT}FILE* f = std::fopen(resolved.c_str(), \"w\");").ok();
    writeln!(e.out, "{INDENT}{INDENT}log_files[resolved] = f;").ok();
    writeln!(e.out, "{INDENT}{INDENT}return f;").ok();
    writeln!(e.out, "{INDENT}}};").ok();
    writeln!(e.out, "").ok();
    writeln!(e.out, "{INDENT}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}va_list ap;").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::printf(\"[cycle:%d %s] \", cycle_count, sev);").ok();
    writeln!(e.out, "{INDENT}{INDENT}va_start(ap, fmt); std::vprintf(fmt, ap); va_end(ap);").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::printf(\"\\n\");").ok();
    writeln!(e.out, "{INDENT}{INDENT}if (f) {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fprintf(f, \"[cycle:%d %s] \", cycle_count, sev);").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}va_start(ap, fmt); std::vfprintf(f, fmt, ap); va_end(ap);").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fprintf(f, \"\\n\");").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fflush(f);").ok();
    writeln!(e.out, "{INDENT}{INDENT}}}").ok();
    writeln!(e.out, "{INDENT}}};").ok();
    writeln!(e.out, "").ok();
    // After sim_log_line below is defined, emit the seed line so it lands
    // in sim.log on every run — required for reproducing failures.
    let log_seed = true;

    writeln!(e.out, "{INDENT}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}va_list ap;").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::printf(\"[cycle:%d %s] \", cycle_count, sev);").ok();
    writeln!(e.out, "{INDENT}{INDENT}va_start(ap, fmt); std::vprintf(fmt, ap); va_end(ap);").ok();
    writeln!(e.out, "{INDENT}{INDENT}std::printf(\"\\n\");").ok();
    writeln!(e.out, "{INDENT}{INDENT}if (sim_log) {{").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fprintf(sim_log, \"[cycle:%d %s] \", cycle_count, sev);").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}va_start(ap, fmt); std::vfprintf(sim_log, fmt, ap); va_end(ap);").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fprintf(sim_log, \"\\n\");").ok();
    writeln!(e.out, "{INDENT}{INDENT}{INDENT}std::fflush(sim_log);").ok();
    writeln!(e.out, "{INDENT}{INDENT}}}").ok();
    writeln!(e.out, "{INDENT}}};").ok();
    writeln!(e.out, "").ok();

    if log_seed {
        writeln!(e.out, "{INDENT}sim_log_line(\"INFO\", \"seed=%llu\", (long long)harc_rng_state);").ok();
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
        let c = match it {
            Item::Driver(c) | Item::Agent(c) | Item::Env(c)
            | Item::Sequencer(c) | Item::Scoreboard(c) => c,
            _ => continue,
        };
        for ci in &c.items {
            if let ComponentItem::Hookable(h) = ci {
                e.emit_hook_vectors(c, h, 1);
            }
        }
    }
    for it in &file.items {
        let c = match it {
            Item::Driver(c) | Item::Agent(c) | Item::Env(c)
            | Item::Sequencer(c) | Item::Scoreboard(c) => c,
            _ => continue,
        };
        for ci in &c.items {
            if let ComponentItem::Hookable(h) = ci {
                e.emit_component_method(c, h, 1);
                emitted_any_method = true;
            }
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
    writeln!(e.out, "{INDENT}_run_slot.thread = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{").ok();

    e.in_coroutine = true;
    for it in &test.items {
        match it {
            TestItem::Stmt(s) => e.emit_stmt(s, 2),
            TestItem::Scope(s) => {
                if let Some(b) = &s.setup    { e.emit_block(b, 2); }
                if let Some(b) = &s.run      { e.emit_block(b, 2); }
                if let Some(b) = &s.check    { e.emit_block(b, 2); }
                if let Some(b) = &s.teardown { e.emit_block(b, 2); }
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
    writeln!(e.out, "{INDENT}// Resume each coroutine once so initial-setup statements run").ok();
    writeln!(e.out, "{INDENT}// before the first clock edge. Single-threaded — workers").ok();
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
        writeln!(e.out, "{INDENT}// Phase 3a: per-actor OS threads with dual barrier sync.").ok();
        writeln!(e.out, "{INDENT}// {} actor(s) → {} barrier participants (main + workers).",
            n_actors, n_actors + 1).ok();
        writeln!(e.out, "{INDENT}std::atomic<bool> _shutdown{{false}};").ok();
        writeln!(e.out, "{INDENT}harc_rt::Barrier _start_barrier({});", n_actors + 1).ok();
        writeln!(e.out, "{INDENT}harc_rt::Barrier _end_barrier({});",   n_actors + 1).ok();
        writeln!(e.out, "{INDENT}std::vector<std::thread> _workers;").ok();
        for (sched_var, _) in &e.actor_threads {
            writeln!(e.out, "{INDENT}_workers.emplace_back([&]() {{").ok();
            writeln!(e.out, "{INDENT}{INDENT}while (true) {{").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}_start_barrier.wait();").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}if (_shutdown.load(std::memory_order_acquire)) break;").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}{sched_var}.tick();").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}_end_barrier.wait();").ok();
            writeln!(e.out, "{INDENT}{INDENT}}}").ok();
            writeln!(e.out, "{INDENT}}});").ok();
        }
        writeln!(e.out, "").ok();
    }

    writeln!(e.out, "{INDENT}// Drive the clock until the run coroutine completes.").ok();
    if mt {
        writeln!(e.out, "{INDENT}// Per-cycle order: run-tick (may push to actor queues) →").ok();
        writeln!(e.out, "{INDENT}// _start_barrier (release workers) → workers run their").ok();
        writeln!(e.out, "{INDENT}// schedulers → _end_barrier (main waits) → eval + checkers.").ok();
        writeln!(e.out, "{INDENT}// Run-coroutine writes complete BEFORE workers wake → no").ok();
        writeln!(e.out, "{INDENT}// race on shared queues. Workers' DUT-input writes complete").ok();
        writeln!(e.out, "{INDENT}// BEFORE eval → no race on signal state at posedge.").ok();
    }
    if clocks.is_empty() {
        writeln!(e.out, "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done) {{").ok();
        writeln!(e.out, "{INDENT}{INDENT}sched.tick();").ok();
        if mt {
            writeln!(e.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
            writeln!(e.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
        }
        writeln!(e.out, "{INDENT}{INDENT}dut->clk = 0; dut->eval();").ok();
        writeln!(e.out, "{INDENT}{INDENT}dut->clk = 1; dut->eval();").ok();
        writeln!(e.out, "{INDENT}{INDENT}cycle_count++;").ok();
        writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
        writeln!(e.out, "{INDENT}}}").ok();
    } else {
        writeln!(e.out, "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done) {{").ok();
        writeln!(e.out, "{INDENT}{INDENT}sched.tick();").ok();
        if mt {
            writeln!(e.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
            writeln!(e.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
        }
        writeln!(e.out, "{INDENT}{INDENT}long long _target = now_ps + clocks_[0].half_period_ps * 2;").ok();
        writeln!(e.out, "{INDENT}{INDENT}eval_clocks_until(_target);").ok();
        writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
        writeln!(e.out, "{INDENT}}}").ok();
    }

    if mt {
        // Shutdown sequence: workers are blocked on _start_barrier
        // (their next iteration). Set _shutdown, wake them via the
        // start barrier; they observe the flag and break out of their
        // loop without reaching _end_barrier. Then join.
        writeln!(e.out, "").ok();
        writeln!(e.out, "{INDENT}_shutdown.store(true, std::memory_order_release);").ok();
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
            writeln!(e.out, "{INDENT}{INDENT}if (_cov_{tag}_hits > 0) _cov_hit++;",
                tag = c.tag).ok();
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
    writeln!(e.out, "{INDENT}delete dut;").ok();
    writeln!(e.out, "{INDENT}if (sim_log) std::fclose(sim_log);").ok();
    writeln!(e.out, "{INDENT}for (auto& kv : log_files) {{ if (kv.second) std::fclose(kv.second); }}").ok();
    writeln!(e.out, "").ok();
    writeln!(e.out, "{INDENT}if (errors == 0) {{ std::printf(\"\\nALL TESTS PASSED\\n\"); return 0; }}").ok();
    writeln!(e.out, "{INDENT}else             {{ std::printf(\"\\n%d TESTS FAILED\\n\", errors); return 1; }}").ok();
    writeln!(e.out, "}}").ok();

    if !e.errors.is_empty() {
        return Err(EmitError(e.errors.join("\n")));
    }
    Ok(e.out)
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

/// Width-typed transaction field info used by the Z3 solver lowering.
#[derive(Debug, Clone)]
struct TxnFieldInfo {
    name: String,
    width: u32,
    /// `!` prefix on a transaction field — non-random; carried for future
    /// solver use. Not yet consulted by the lowering, but kept so the field
    /// info round-trips through codegen.
    #[allow(dead_code)]
    non_random: bool,
}

struct Emitter {
    out: String,
    errors: Vec<String>,
    /// Transaction name → field metadata. Populated alongside `transactions`
    /// before main-body emission so the solver block can declare Z3 vars
    /// of the right widths and walk only the fields it should write back.
    txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>>,
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
    /// Monitor declarations indexed by name. When `let mon : MonName` is
    /// encountered, we emit the struct instance + register each `on`
    /// handler from the declaration as a `_checkers` closure (with field-
    /// name substitution so `emit write_e(...)` resolves to `mon.write_e`).
    monitors: std::collections::HashMap<String, ComponentDecl>,
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
}

impl Emitter {
    fn pad(&mut self, depth: usize) {
        for _ in 0..depth { self.out.push_str(INDENT); }
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
        let msg = v.else_fail.as_ref()
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

    /// `assume <expr>` (immediate form) — same as inline assert but logged
    /// as ASSUME so the user can grep separately. No `errors++` because in
    /// sim, assumes are warnings (spec §5.2).
    fn emit_inline_assume(&mut self, v: &Verify, depth: usize) {
        self.pad(depth);
        let expr = v.expr.as_ref().expect("assume without expr");
        write!(self.out, "if (!(").ok();
        self.emit_expr(expr);
        writeln!(self.out, ")) sim_log_line(\"ASSUME\", \"assumption failed\");").ok();
    }

    /// Register a concurrent (every-primary-clock-edge) property check.
    /// Resolves a bare `Ident` to a declared property body; otherwise uses
    /// the inline expression directly. Translates the top-level temporal
    /// shape (`a |-> b`, `a |=> b`, or a plain bool) to a stateful closure
    /// pushed into `_checkers`.
    fn emit_property_check(&mut self, severity: &str, v: &Verify, depth: usize) {
        let raw = v.expr.as_ref().expect("assert property without body");
        let body: Expr = match &*raw.kind {
            ExprKind::Ident(id) => {
                match self.properties.get(&id.name).cloned() {
                    Some(b) => b,
                    None => {
                        self.errors.push(format!(
                            "assert property `{}`: no property declaration with that name",
                            id.name
                        ));
                        return;
                    }
                }
            }
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
                SystemFn::Past   => format!("{tag}_ps{i}"),
                SystemFn::Rose   => format!("(!{tag}_ps{i} && {tag}_cur{i})"),
                SystemFn::Fell   => format!("({tag}_ps{i} && !{tag}_cur{i})"),
                SystemFn::Stable => format!("({tag}_ps{i} == {tag}_cur{i})"),
                SystemFn::Clog2  => continue, // not temporal — skip
            };
            self.prop_subs.insert((t.call_span.start, t.call_span.end), sub);
        }
        match &*body.kind {
            // a |=> b — one-cycle-delayed implication. State: prev_a.
            ExprKind::Binary { op: BinaryOp::PipeImpliesNext, lhs, rhs } => {
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
                writeln!(self.out, "sim_log_line(\"{severity}\", \"property `{}` failed (|=>)\");", escape_c(&label)).ok();
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
            ExprKind::Binary { op: BinaryOp::PipeImplies, lhs, rhs } => {
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                self.emit_expr(lhs);
                write!(self.out, ") && !(bool)(").ok();
                self.emit_expr(rhs);
                writeln!(self.out, ")) {{").ok();
                self.pad(depth + 2);
                writeln!(self.out, "sim_log_line(\"{severity}\", \"property `{}` failed (|->)\");", escape_c(&label)).ok();
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
                writeln!(self.out, "sim_log_line(\"{severity}\", \"property `{}` failed\");", escape_c(&label)).ok();
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

    /// Emit a monitor declaration's data layout — just the fields. Each
    /// `out event<T>` becomes `std::vector<std::function<void(T)>>`. The
    /// on-handlers in the monitor body are NOT emitted here — they live
    /// at the `let mon : MonName` site, registered into `_checkers`.
    fn emit_monitor_struct(&mut self, m: &ComponentDecl) {
        writeln!(self.out, "struct {} {{", m.name.name).ok();
        for it in &m.items {
            if let ComponentItem::Field(f) = it {
                // Resolve via `component_field_c_type` so named
                // sub-components (scoreboards, transactions, enums)
                // get their proper C++ type rather than falling back
                // to `int64_t`. Necessary for bound monitors
                // (Phase 2c) that hold a `sb : AxilSb` scoreboard
                // and access `mon.sb.<queue>.size()` from outside
                // the monitor body.
                let cty = self.component_field_c_type(&f.ty);
                writeln!(self.out, "{INDENT}{cty} {};", f.name.name).ok();
            }
        }
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// Emit a cycle-trigger `on <bool-expr>` handler — registers a closure
    /// that fires per the handler's `edge` mode (rising / falling / level).
    /// Used by both the test-scope `on` form and monitor body handlers; the
    /// `prefix` distinguishes the static-state tags so concurrent handlers
    /// at the same span don't collide.
    fn emit_cycle_trigger(&mut self, h: &OnHandler, depth: usize, prefix: &str) {
        let tag = format!("{prefix}{}_{}", h.event.span.start, h.event.span.end);
        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
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

    /// At a `let mon : MonName` site, emit the registrations for each
    /// on-handler in the monitor's declaration. Each handler becomes a
    /// `_checkers` closure with `[&]` capture; the body's bare references
    /// to the monitor's own fields get prefixed with `<instance>.` via
    /// the `field_subs` rewrite map.
    fn emit_monitor_handler_registrations(
        &mut self,
        mon: &ComponentDecl,
        instance: &str,
        depth: usize,
    ) {
        self.emit_component_handler_registrations(mon, instance, depth, "_m_");
    }

    /// Generic on-handler registration for any component (monitor /
    /// driver / agent / sequencer / scoreboard). Dispatches each
    /// handler by the shape of its trigger expression:
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
                if let TypeExpr::Builtin { name: BuiltinTy::Event, args, .. } = &f.ty {
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
                    let channel = match binding.0.handshakes.iter()
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
                    let chan_prefix = format!("{}_{}", sig_prefix, ch_name);
                    let slot_var  = format!("_{instance}_{ch_name}_slot");
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
                        self.actor_threads.push((sched_var.clone(), slot_var.clone()));
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
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{chan_prefix}_valid && {root}->{chan_prefix}_ready; }});",
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
                        if !first { write!(self.out, ", ").ok(); }
                        first = false;
                        write!(self.out, "{root}->{chan_prefix}_{}", sig.name.name).ok();
                    }
                    writeln!(self.out, "}};").ok();

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

                    self.emit_block(&h.body, depth + 2);

                    self.in_coroutine = prior_corout;
                    match prior_bus {
                        Some(prev) => { self.bus_bindings.insert("bus".into(), prev); }
                        None       => { self.bus_bindings.remove("bus"); }
                    }
                    for n in added_events { self.event_types.remove(&n); }
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
                &sync_view, instance, depth, "_m_", Some(binding.clone()),
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
        // Only `driver` and `agent` are eligible — monitors observe
        // signals, not transaction queues; envs / sequencers /
        // scoreboards aren't actor-shaped at all.
        if !matches!(comp.kind, ComponentKind::Driver | ComponentKind::Agent) {
            return false;
        }

        // Find a single `in event<T>` field. Multi-input drivers stay
        // sync for now (each input would need its own queue + the
        // coroutine would have to multi-way wait — separate PR).
        let mut input_event: Option<(String, String)> = None;
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                if matches!(f.direction, Some(Direction::In)) {
                    if let TypeExpr::Builtin { name: BuiltinTy::Event, args, .. } = &f.ty {
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
        let slot_var  = format!("_{instance}_slot");
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
            self.actor_threads.push((sched_var.clone(), slot_var.clone()));
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
        ).ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ return !{queue_var}.empty(); }});",
        ).ok();
        self.pad(depth + 2);
        writeln!(self.out, "auto {arg_name} = {queue_var}.front();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "{queue_var}.pop_front();").ok();

        // Build field-name substitution map: bare names inside the
        // handler body resolve to `instance.field`. Event-typed
        // fields also register their payload types so any nested
        // `emit <ev>(arg)` finds the right param shape.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
                if let TypeExpr::Builtin { name: BuiltinTy::Event, args, .. } = &f.ty {
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

        self.emit_block(&handler.body, depth + 2);

        self.in_coroutine = prior_corout;
        match prior_bus {
            Some(prev) => { self.bus_bindings.insert("bus".into(), prev); }
            None       => { self.bus_bindings.remove("bus"); }
        }
        self.field_subs = prev_subs;
        for n in added_events { self.event_types.remove(&n); }

        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();          // close while(true)
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
                ComponentItem::OnHandler(h) => {
                    extract_event_subscription(&h.event)
                        .map(|(ev, _)| ev != event_name)
                        .unwrap_or(true)
                }
                _ => true,
            });
            self.emit_component_handler_registrations_bound(
                &filtered, instance, depth, &tag, Some(binding.clone()),
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
                if let TypeExpr::Builtin { name: BuiltinTy::Event, args, .. } = &f.ty {
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
                if let Some((event_name, arg_name)) =
                    extract_event_subscription(&h.event)
                {
                    if event_field_names.contains(&event_name) {
                        // Subscriber to a component event field.
                        let arg_ty = self.event_types.get(&event_name).cloned()
                            .unwrap_or_else(|| "int64_t".into());
                        self.pad(depth);
                        writeln!(
                            self.out,
                            "{instance}.{event_name}.push_back([&]({arg_ty} {arg_name}) {{",
                        ).ok();
                        // For `bound to BusType` components, expose the
                        // driver's bus binding inside the handler body
                        // as the bare identifier `bus`. Same root +
                        // BusDecl as the test-scope binding it was
                        // bound from, so `bus.<ch>.send/recv` and
                        // `bus.<ch>.<sig>` lower through the existing
                        // bus_handshake / bus_field_access paths.
                        let prior_bus = bound_bus.as_ref()
                            .and_then(|b| self.bus_bindings.insert("bus".into(), b.clone()));
                        self.emit_block(&h.body, depth + 1);
                        if bound_bus.is_some() {
                            match prior_bus {
                                Some(prev) => { self.bus_bindings.insert("bus".into(), prev); }
                                None       => { self.bus_bindings.remove("bus"); }
                            }
                        }
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
        for n in added_events { self.event_types.remove(&n); }
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
                        if i > 0 { write!(self.out, " || ").ok(); }
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
                    if hi.is_some() { write!(self.out, " && ").ok(); }
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
        let total_bins: usize = g.items.iter().map(|it| match it {
            CoverItem::Point(p) => p.bins.len(),
            _ => 0,
        }).sum();
        writeln!(self.out, "{INDENT}void report() const {{").ok();
        self.pad(2);
        writeln!(self.out, "uint64_t _total = {total_bins}; uint64_t _hit = 0;").ok();
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
    fn try_emit_bus_handshake(
        &mut self,
        e: &Expr,
        let_name: Option<&str>,
        depth: usize,
    ) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else { return false; };
        let ExprKind::Field { target, name: method } = &*callee.kind else { return false; };
        let ExprKind::Field { target: outer, name: ch } = &*target.kind else { return false; };
        let ExprKind::Ident(id) = &*outer.kind else { return false; };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else { return false; };
        let Some(h) = bus.handshakes.iter().find(|h| h.name.name == ch.name).cloned() else {
            return false;
        };
        let prefix = format!("{}_{}", sig_prefix, ch.name);  // axil_aw

        match method.name.as_str() {
            "send" => {
                if args.len() != h.payload.len() {
                    self.errors.push(format!(
                        "bus.{}.send: expected {} payload arg(s), got {}",
                        ch.name, h.payload.len(), args.len(),
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
                    self.pad(depth);
                    write!(self.out, "{root}->{prefix}_{} = ", sig.name.name).ok();
                    match arg {
                        CallArg::Expr(e) => self.emit_expr(e),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                    writeln!(self.out, ";").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{prefix}_valid = 1;").ok();
                self.pad(depth);
                if self.in_coroutine {
                    // Coroutine path: yield until ready=1 (bounded). The
                    // bound matches the sync 16-cycle budget so a stuck
                    // DUT still terminates the test rather than hanging.
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{prefix}_ready && _b > 0) {{ co_await harc_rt::wait_cycles(_slot, 1); _b--; }} }}").ok();
                    self.pad(depth);
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                } else {
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{prefix}_ready && _b > 0) {{ tick(); _b--; }} }}").ok();
                    self.pad(depth);
                    writeln!(self.out, "tick();").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{prefix}_valid = 0;").ok();
                true
            }
            "recv" => {
                if !args.is_empty() {
                    self.errors.push(format!(
                        "bus.{}.recv: expected 0 args, got {}",
                        ch.name, args.len(),
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
                writeln!(self.out, "{root}->{prefix}_ready = 1;").ok();
                self.pad(depth);
                if self.in_coroutine {
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{prefix}_valid && _b > 0) {{ co_await harc_rt::wait_cycles(_slot, 1); _b--; }} }}").ok();
                } else {
                    writeln!(self.out, "{{ int _b = 16; while (!{root}->{prefix}_valid && _b > 0) {{ tick(); _b--; }} }}").ok();
                }
                // Capture BEFORE the trailing tick: the destination signals
                // are valid in the same cycle as `valid` is high.
                if let Some(name) = let_name {
                    self.pad(depth);
                    let struct_name = format!("{}_{}_payload", bus.name.name, ch.name);
                    write!(self.out, "{struct_name} {name} = {{").ok();
                    let mut first = true;
                    for sig in &h.payload {
                        if !first { write!(self.out, ", ").ok(); }
                        first = false;
                        write!(self.out, "{root}->{prefix}_{}", sig.name.name).ok();
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
                writeln!(self.out, "{root}->{prefix}_ready = 0;").ok();
                true
            }
            _ => false,
        }
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
                        if tail == "valid" || tail == "ready"
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
        if let ExprKind::Field { target: outer, name: ch } = &*target.kind {
            if let ExprKind::Ident(id) = &*outer.kind {
                if let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() {
                    if let Some(h) = bus.handshakes.iter().find(|h| h.name.name == ch.name) {
                        if name.name == "valid" || name.name == "ready"
                            || h.payload.iter().any(|s| s.name.name == name.name)
                        {
                            return Some(format!(
                                "{root}->{}_{}_{}",
                                sig_prefix, ch.name, name.name,
                            ));
                        }
                        let valid_options: Vec<&str> = std::iter::once("valid")
                            .chain(std::iter::once("ready"))
                            .chain(h.payload.iter().map(|s| s.name.name.as_str()))
                            .collect();
                        self.errors.push(format!(
                            "bus `{}` channel `{}` has no signal `{}` (valid: {})",
                            bus.name.name, ch.name, name.name,
                            valid_options.join(", "),
                        ));
                        return Some(format!("/* unresolved: {}.{}.{} */ 0", id.name, ch.name, name.name));
                    }
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) has no channel `{}`",
                        bus.name.name, id.name, ch.name,
                    ));
                    return Some(format!("/* unresolved: {}.{}.{} */ 0", id.name, ch.name, name.name));
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
        let ExprKind::Field { target, name: method } = &*event.kind else { return None; };
        let find_method = |c: &ComponentDecl, m: &str| -> Option<Vec<Param>> {
            for it in &c.items {
                if let ComponentItem::Hookable(h) = it {
                    if h.name.name == m { return Some(h.params.clone()); }
                }
            }
            None
        };

        // <ident>.<method>
        if let ExprKind::Ident(id) = &*target.kind {
            let comp_ty = self.let_types.get(&id.name)?;
            let comp = self.components.get(comp_ty)?;
            let params = find_method(comp, &method.name)?;
            return Some((comp_ty.clone(), method.name.clone(), params));
        }

        // <ident>.<sub>.<method>
        if let ExprKind::Field { target: outer, name: sub } = &*target.kind {
            let ExprKind::Ident(id) = &*outer.kind else { return None; };
            let outer_ty = self.let_types.get(&id.name)?;
            let outer_comp = self.components.get(outer_ty)?;
            let sub_ty = outer_comp.items.iter().find_map(|it| {
                if let ComponentItem::Field(f) = it {
                    if f.name.name == sub.name {
                        return type_simple_name(Some(&f.ty));
                    }
                }
                None
            })?;
            let sub_comp = self.components.get(sub_ty)?;
            let params = find_method(sub_comp, &method.name)?;
            return Some((sub_ty.to_string(), method.name.clone(), params));
        }
        None
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
    fn resolve_component_method_call(&self, callee: &Expr) -> Option<(String, String, String)> {
        let ExprKind::Field { target, name: method } = &*callee.kind else { return None; };
        let comp_has_method = |c: &ComponentDecl, m: &str| -> bool {
            c.items.iter().any(|it| matches!(
                it, ComponentItem::Hookable(h) if h.name.name == m
            ))
        };

        // <ident>.<method>
        if let ExprKind::Ident(id) = &*target.kind {
            let comp_ty = self.let_types.get(&id.name)?;
            let comp = self.components.get(comp_ty)?;
            if comp_has_method(comp, &method.name) {
                return Some((comp_ty.clone(), id.name.clone(), method.name.clone()));
            }
            return None;
        }

        // <ident>.<sub>.<method>
        if let ExprKind::Field { target: outer, name: sub } = &*target.kind {
            let ExprKind::Ident(id) = &*outer.kind else { return None; };
            let outer_ty = self.let_types.get(&id.name)?;
            let outer_comp = self.components.get(outer_ty)?;
            let sub_ty = outer_comp.items.iter().find_map(|it| {
                if let ComponentItem::Field(f) = it {
                    if f.name.name == sub.name {
                        return type_simple_name(Some(&f.ty));
                    }
                }
                None
            })?;
            let sub_comp = self.components.get(sub_ty)?;
            if comp_has_method(sub_comp, &method.name) {
                return Some((
                    sub_ty.to_string(),
                    format!("{}.{}", id.name, sub.name),
                    method.name.clone(),
                ));
            }
        }
        None
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
                if self.transactions.contains(n) || self.enums.contains_key(n)
                    || self.components.contains_key(n) || self.scoreboards.contains(n)
                    || self.monitors.contains_key(n) || self.covergroups.contains_key(n)
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
    fn emit_hook_vectors(
        &mut self,
        c: &ComponentDecl,
        h: &HookableMethod,
        depth: usize,
    ) {
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let arg_tys: Vec<String> = h.params.iter()
            .map(|p| p.ty.as_ref()
                .map(|t| self.c_type_for_param(t))
                .unwrap_or_else(|| "int64_t".to_string()))
            .collect();
        let arg_csv = arg_tys.join(", ");
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_pre;",
        ).ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_post;",
        ).ok();
    }

    fn emit_component_method(
        &mut self,
        c: &ComponentDecl,
        h: &HookableMethod,
        depth: usize,
    ) {
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let ret = h.return_ty.as_ref()
            .map(c_type_for)
            .unwrap_or_else(|| "void".to_string());
        self.pad(depth);
        write!(
            self.out,
            "auto {comp_ty}_{m_name} = [&]({comp_ty}& self"
        ).ok();
        // Track Named-typed params as pointers so dut.field rewrites
        // properly in the body. Restore on exit. Transaction / enum /
        // sub-component params are by-value (not pointer-shaped).
        let mut added: Vec<String> = Vec::new();
        for p in &h.params {
            let pty = p.ty.as_ref()
                .map(|t| self.c_type_for_param(t))
                .unwrap_or_else(|| "int64_t".to_string());
            write!(self.out, ", {pty} {}", p.name.name).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. }))
               && self.is_dut_pointer_field_type(p.ty.as_ref().unwrap())
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

        // Pre-hooks: fire `<Type>_<method>_pre` subscribers before the
        // body. The hook closures see the same args as the method —
        // empty vectors are a no-op so the wrap is always safe to
        // emit.
        let arg_list: Vec<String> = h.params.iter()
            .map(|p| p.name.name.clone())
            .collect();
        let arg_csv = arg_list.join(", ");
        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_{m_name}_pre) _h({arg_csv});").ok();

        self.emit_block(&h.body, depth + 1);

        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_{m_name}_post) _h({arg_csv});").ok();
        // Restore state.
        self.field_subs = prev_subs;
        for k in added_pointer_fields { self.pointer_vars.remove(&k); }
        for k in added { self.pointer_vars.remove(&k); }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
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
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// True if a Named-type field on a component refers to a DUT module
    /// (i.e. nothing the codegen knows about other than that it's a
    /// Verilator-compiled module type). Sub-components / scoreboards /
    /// monitors / covergroups are excluded — they're held by-value.
    fn is_dut_pointer_field_type(&self, t: &TypeExpr) -> bool {
        if let Some(name) = type_simple_name(Some(t)) {
            return !self.transactions.contains(name)
                && !self.scoreboards.contains(name)
                && !self.monitors.contains_key(name)
                && !self.covergroups.contains_key(name)
                && !self.components.contains_key(name)
                && !self.enums.contains_key(name);
        }
        false
    }

    /// Field-type lowering for `driver`/`agent`/`env`/`sequencer` bodies.
    fn component_field_c_type(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Builtin { name: BuiltinTy::Event, args, .. } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("std::vector<std::function<void({inner})>>")
            }
            TypeExpr::Builtin { name: BuiltinTy::Queue, args, .. } => {
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
        let field_names: Vec<&str> = t.body.iter().filter_map(|it| match it {
            TxnBodyItem::Field(f) => Some(f.name.name.as_str()),
            _ => None,
        }).collect();
        if field_names.is_empty() {
            writeln!(self.out, "inline bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
                t.name.name).ok();
        } else {
            write!(self.out, "inline bool operator==(const {0}& a, const {0}& b) {{ return ", t.name.name).ok();
            for (i, fname) in field_names.iter().enumerate() {
                if i > 0 { write!(self.out, " && ").ok(); }
                write!(self.out, "a.{fname} == b.{fname}").ok();
            }
            writeln!(self.out, "; }}").ok();
        }
        writeln!(self.out, "inline bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
            t.name.name).ok();
        writeln!(self.out, "").ok();

        // randomize_T(t) function.
        writeln!(self.out, "static void randomize_{}({}* t) {{", t.name.name, t.name.name).ok();
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
                        for (i, e) in entries.iter().enumerate() {
                            if i > 0 { write!(self.out, ", ").ok(); }
                            // Each dist entry: (lo, hi, weight). The `value`
                            // expression is either a RangeLit (use lo/hi) or
                            // a scalar (use as both lo and hi).
                            match &*e.value.kind {
                                ExprKind::RangeLit { lo: Some(lo), hi: Some(hi) } => {
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
                        writeln!(self.out, "}});").ok();
                        handled = true;
                    }
                }
                _ => {}
            }
            if handled { break; }
        }
        if handled { return; }

        // Fallback: type-driven uniform sampling.
        match &f.ty {
            TypeExpr::Builtin { name, args, .. } => {
                let width = type_arg_width(args);
                match name {
                    BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                        writeln!(self.out, "{INDENT}t->{} = harc_rng_uint({});",
                            f.name.name, width.unwrap_or(32)).ok();
                    }
                    BuiltinTy::SInt | BuiltinTy::SIntCap => {
                        let w = width.unwrap_or(32);
                        writeln!(self.out, "{INDENT}t->{} = harc_rng_range(-(1LL << {}), (1LL << {}) - 1);",
                            f.name.name, w - 1, w - 1).ok();
                    }
                    BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                        writeln!(self.out, "{INDENT}t->{} = harc_rng_range(0, 1);", f.name.name).ok();
                    }
                    BuiltinTy::Int => {
                        writeln!(self.out, "{INDENT}t->{} = harc_rng_range(0, 0x7FFFFFFF);", f.name.name).ok();
                    }
                    _ => {
                        writeln!(self.out, "{INDENT}// {} : <unsupported type for v0 randomize>",
                            f.name.name).ok();
                    }
                }
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if let Some(&n) = self.enums.get(last) {
                    let hi = if n == 0 { 0 } else { (n - 1) as i64 };
                    writeln!(self.out, "{INDENT}t->{} = harc_rng_range(0, {});", f.name.name, hi).ok();
                } else {
                    writeln!(self.out, "{INDENT}// {} : {} (named, not yet supported)",
                        f.name.name, last).ok();
                }
            }
        }
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
        let fields = match self.txn_fields.get(ty).cloned() {
            Some(f) => f,
            None => {
                self.errors.push(format!("internal: no field info for transaction `{ty}`"));
                return;
            }
        };
        // Tracking for the constraint translator: which field names exist.
        let field_set: std::collections::HashSet<String> =
            fields.iter().map(|f| f.name.clone()).collect();

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

        // All Z3 vars declared at 64 bits so binops with literals and across
        // fields don't trip the width-compatibility check. Each field then
        // gets a range constraint enforcing its declared width — uniform
        // emission, no zext bookkeeping.
        for f in &fields {
            self.pad(depth + 1);
            writeln!(self.out, "z3::expr _z_{} = _ctx.bv_const(\"{}\", 64);",
                f.name, f.name).ok();
            if f.width < 64 {
                self.pad(depth + 1);
                writeln!(self.out,
                    "_s.add(z3::ult(_z_{}, _ctx.bv_val((uint64_t)1ULL << {}, 64)));",
                    f.name, f.width).ok();
            }
        }

        // Translated constraints.
        for c in with_body {
            self.pad(depth + 1);
            write!(self.out, "_s.add(").ok();
            self.emit_constraint_expr(c, &field_set, &fields);
            writeln!(self.out, ");").ok();
        }

        // Detect fields the user has equality-pinned (e.g. `t.addr == 24`).
        // Those fields have only one satisfying value, so adding a blocking
        // clause for them makes the whole problem UNSAT after the first
        // call. We block only the *free* fields. Diversity then comes from
        // the free-field cache.
        let pinned: std::collections::HashSet<String> = with_body.iter().filter_map(|e| {
            if let ExprKind::Binary { op: BinaryOp::Eq, lhs, rhs } = &*e.kind {
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
            } else { None }
        }).collect();

        let cache_tag = format!("_div_cache_{}", target.span.start);
        let free_fields: Vec<&TxnFieldInfo> = fields.iter()
            .filter(|f| !pinned.contains(&f.name))
            .collect();

        // One static history vector per free field — persists across loop
        // iterations to push the solver away from previously-seen answers.
        for f in &free_fields {
            self.pad(depth + 1);
            writeln!(self.out,
                "static std::vector<uint64_t> {cache_tag}_{};",
                f.name).ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "_s.push();   // diversity-blocking clauses (free fields only)").ok();
        for f in &free_fields {
            self.pad(depth + 1);
            writeln!(self.out,
                "if ({cache_tag}_{}.size() > 32) {cache_tag}_{}.clear();",
                f.name, f.name).ok();
            self.pad(depth + 1);
            writeln!(self.out,
                "for (auto _v : {cache_tag}_{}) _s.add(_z_{} != _ctx.bv_val(_v, 64));",
                f.name, f.name).ok();
        }
        // First check: with blocking. If UNSAT (cache has saturated the
        // satisfiable space), drop the blocks and clear the cache.
        self.pad(depth + 1);
        writeln!(self.out, "auto _r = _s.check();").ok();
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
            // Every declared field is assigned from the model — equality-
            // pinned fields take their constrained value; free fields take
            // a Z3-chosen satisfying value. Only free fields get pushed
            // into the diversity cache.
            self.pad(depth + 2);
            writeln!(self.out, "uint64_t _val_{} = _m.eval(_z_{}).get_numeral_uint64();",
                f.name, f.name).ok();
            self.pad(depth + 2);
            write!(self.out, "").ok();
            self.emit_expr(target);
            writeln!(self.out, ".{} = _val_{};", f.name, f.name).ok();
            if !pinned.contains(&f.name) {
                self.pad(depth + 2);
                writeln!(self.out, "{cache_tag}_{}.push_back(_val_{});", f.name, f.name).ok();
            }
        }
        self.pad(depth + 1);
        writeln!(self.out, "}} else {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "sim_log_line(\"FAIL\", \"randomize(t) with: constraint UNSAT\");").ok();
        self.pad(depth + 2);
        writeln!(self.out, "errors++;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Translate a HARC expression to a z3++ C++ expression. Field accesses
    /// `t.<name>` resolve to the per-field `_z_<name>` Z3 var declared in
    /// the surrounding solver block. Integer literals become `_ctx.bv_val(N, W)`
    /// at the field's width inferred from context. v0 is permissive — any
    /// untranslatable form falls back to a comment + `_ctx.bool_val(true)`.
    fn emit_constraint_expr(
        &mut self,
        e: &Expr,
        field_set: &std::collections::HashSet<String>,
        _fields: &[TxnFieldInfo],
    ) {
        // All Z3 vars are 64-bit; literals match.
        self.emit_constraint_expr_w(e, field_set, 64);
    }

    fn emit_constraint_expr_w(
        &mut self,
        e: &Expr,
        field_set: &std::collections::HashSet<String>,
        width: u32,
    ) {
        match &*e.kind {
            // `t.<name>` → _z_<name>. Strip the `t.` prefix.
            ExprKind::Field { target, name } => {
                if matches!(&*target.kind, ExprKind::Ident(_)) && field_set.contains(&name.name) {
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
                // Bare ident treated as field shorthand.
                if field_set.contains(&id.name) {
                    write!(self.out, "_z_{}", id.name).ok();
                } else {
                    self.errors.push(format!(
                        "constraint references unknown name `{}`", id.name
                    ));
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
            }
            ExprKind::Int(s) => {
                write!(self.out, "_ctx.bv_val((uint64_t){}, {})", c_int_literal(s), width).ok();
            }
            ExprKind::Bool(b) => {
                write!(self.out, "_ctx.bool_val({})", if *b { "true" } else { "false" }).ok();
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_constraint_expr_w(inner, field_set, width);
                write!(self.out, ")").ok();
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not | UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                write!(self.out, "{s}").ok();
                self.emit_constraint_expr_w(expr, field_set, width);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                use BinaryOp::*;
                // Comparisons / equality return Bool in z3; arithmetic stays bv.
                let (sep, ucmp) = match op {
                    Add => (" + ", None),
                    Sub => (" - ", None),
                    Mul => (" * ", None),
                    Div => (" / ", None),
                    Mod => (" %% ", None),  // in printf, %% — actually z3 has urem; prefer that
                    Eq => (" == ", None),
                    Ne => (" != ", None),
                    Lt => (" < ", Some("ult")),
                    Le => (" <= ", Some("ule")),
                    Gt => (" > ", Some("ugt")),
                    Ge => (" >= ", Some("uge")),
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
                if let Some(fn_name) = ucmp {
                    // Unsigned comparison — use z3::ult/ule/ugt/uge to match
                    // HARC's uint semantics. The default `<` on z3::expr is
                    // signed, which doesn't match HARC `uint<N>` fields.
                    write!(self.out, "z3::{fn_name}(").ok();
                    self.emit_constraint_expr_w(lhs, field_set, width);
                    write!(self.out, ", ").ok();
                    self.emit_constraint_expr_w(rhs, field_set, width);
                    write!(self.out, ")").ok();
                } else {
                    self.emit_constraint_expr_w(lhs, field_set, width);
                    write!(self.out, "{sep}").ok();
                    self.emit_constraint_expr_w(rhs, field_set, width);
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

    /// Emit a `log` or `logf` call. When `file_path` is `Some`, lower to
    /// `sim_logf_line(get_log_file(path), sev, fmt, args)`; otherwise lower
    /// to `sim_log_line(sev, fmt, args)`. Severity / message extraction
    /// matches `log()`'s rules (first ident is severity; first string is
    /// the message).
    fn emit_log(&mut self, args: &[CallArg], file_path: Option<String>, depth: usize) {
        let sev = args.iter().find_map(|a| match a {
            CallArg::Expr(e) => match &*e.kind {
                ExprKind::Ident(id) => Some(id.name.to_uppercase()),
                _ => None,
            }
            _ => None,
        }).unwrap_or_else(|| "INFO".to_string());
        let msg = args.iter().find_map(|a| match a {
            CallArg::Expr(e) => match &*e.kind {
                ExprKind::String(s) => Some(s.clone()),
                _ => None,
            }
            _ => None,
        }).unwrap_or_else(|| "".to_string());
        let (fmt, caps) = process_interp(&msg);
        self.pad(depth);
        match file_path {
            Some(p) => {
                write!(self.out, "sim_logf_line(get_log_file(\"{}\"), \"{}\", \"{}\"",
                    escape_c(&p), sev, escape_c(&fmt)).ok();
            }
            None => {
                write!(self.out, "sim_log_line(\"{}\", \"{}\"", sev, escape_c(&fmt)).ok();
            }
        }
        for c in &caps {
            self.emit_interp_arg(c);
        }
        writeln!(self.out, ");").ok();
    }

    /// Emit one captured `${expr}` value as a printf argument. Routes the
    /// captured source text through the parser so `dut.x` (a member access
    /// on a pointer) gets emitted as `dut->x` via the normal field-rewrite
    /// machinery, rather than the literal source text. Falls back to raw
    /// text if the fragment doesn't parse — preserves whatever the user
    /// wrote so they get a meaningful C++ error rather than a hidden one.
    fn emit_interp_arg(&mut self, capture: &str) {
        write!(self.out, ", (long long)(").ok();
        match crate::parser::parse_expr_fragment(capture) {
            Ok(e) => self.emit_expr(&e),
            Err(_) => { write!(self.out, "{capture}").ok(); }
        }
        write!(self.out, ")").ok();
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
        let inner = t.return_ty.as_ref()
            .and_then(tseq_inner_type)
            .unwrap_or_else(|| "int64_t".to_string());
        self.pad(depth);
        write!(self.out, "auto {} = [&](", t.name.name).ok();
        let mut added: Vec<String> = Vec::new();
        for (i, p) in t.params.iter().enumerate() {
            if i > 0 { write!(self.out, ", ").ok(); }
            let pty = p.ty.as_ref().map(c_type_for).unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", p.name.name).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) {
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
        for k in added { self.pointer_vars.remove(&k); }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    fn emit_function(&mut self, f: &FunctionDecl, depth: usize) {
        self.pad(depth);
        let ret = f.return_ty.as_ref().map(c_type_for).unwrap_or("void".to_string());
        write!(self.out, "auto {} = [&](", f.name.name).ok();
        // Track which params are Named-typed (pointer-shaped). Add to
        // `pointer_vars` while emitting the body, then remove on exit so
        // siblings don't leak each other's params.
        let mut added: Vec<String> = Vec::new();
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 { write!(self.out, ", ").ok(); }
            let pty = p.ty.as_ref().map(c_type_for).unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", p.name.name).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        writeln!(self.out, ") -> {ret} {{").ok();
        self.emit_block(&f.body, depth + 1);
        for k in added { self.pointer_vars.remove(&k); }
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
                self.emit_lvalue(target);
                write!(self.out, " = ").ok();
                self.emit_expr(value);
                writeln!(self.out, ";").ok();
            }
            StmtKind::Assign { target, value } => {
                self.pad(depth);
                self.emit_lvalue(target);
                write!(self.out, " = ").ok();
                self.emit_expr(value);
                writeln!(self.out, ";").ok();
            }
            StmtKind::For(f) => {
                self.pad(depth);
                let var = &f.var.name;
                if let ExprKind::RangeLit { lo: Some(lo), hi: Some(hi) } = &*f.iter.kind {
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
            StmtKind::Wait { duration, clock, .. } => {
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
                    write!(self.out, "{{ long long _target = clocks_[{idx}].rising_count + (long long)(").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "); while (clocks_[{idx}].rising_count < _target) {{").ok();
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
                    (format!("cov_{}_{}", expr.span.start, expr.span.end), expr.clone())
                };
                let tag = format!("c_{}_{}", expr.span.start, expr.span.end);
                self.covers.push(CoverInfo { tag: tag.clone(), label });
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
                    self.errors.push("logf requires a string-literal file path as the first arg".into());
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
                self.pad(depth);
                self.emit_expr(e);
                writeln!(self.out, ";").ok();
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
                    self.errors.push(
                        "`yield` outside a `tseq` body is not supported in v0 cpp_tb".into(),
                    );
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
                    name.segments.iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".")
                } else {
                    let raw = name.segments.last()
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
                        let arg_decls: Vec<String> = params.iter().map(|p| {
                            let ty = p.ty.as_ref()
                                .map(|t| self.c_type_for_param(t))
                                .unwrap_or_else(|| "int64_t".to_string());
                            format!("{ty} {}", p.name.name)
                        }).collect();
                        self.pad(depth);
                        writeln!(
                            self.out,
                            "{comp_ty}_{method_name}_{side_str}.push_back([&]({}) {{",
                            arg_decls.join(", "),
                        ).ok();
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
                            self.errors.push("on <event>(arg): event must be `name` or `obj.name`".into());
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
                    let arg_ty = self.event_types.get(&raw).cloned()
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
            StmtKind::Randomize { blocking: _, target, with_body } => {
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
                if with_body.is_empty() {
                    // No cross-field constraints: simple field-by-field PRNG path.
                    self.pad(depth);
                    write!(self.out, "randomize_{ty}(&").ok();
                    self.emit_expr(target);
                    writeln!(self.out, ");").ok();
                } else {
                    // Constraint-solving path via Z3.
                    self.emit_constraint_solver_block(&ty, target, with_body, depth);
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
        // Track let-binding type for randomize(t) resolution. Done first
        // (before the dut shortcut) so even nested lets register.
        if let Some(s) = type_simple_name(l.ty.as_ref()) {
            self.let_types.insert(l.name.name.clone(), s.to_string());
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
                    self.bus_bindings.insert(l.name.name.clone(), (bus, buf, prefix));
                    return;
                }
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
                // Bound monitor: `let mon : MonName = bind axil` for a
                // `monitor MonName bound to BusType`. Each `on
                // bus.<ch>.handshake(arg)` handler in the monitor
                // becomes its own coroutine actor that fires once per
                // valid+ready cycle on the channel. Other handler
                // shapes (event subscribers, cycle triggers) fall
                // through to the existing sync path.
                if let Some(mon) = self.monitors.get(simple).cloned() {
                    if let Some(bus_ty) = &mon.bound_to {
                        if let Some(v) = &l.value {
                            if let ExprKind::Ident(rhs) = &*v.kind {
                                if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned() {
                                    let want = type_simple_name(Some(bus_ty));
                                    if want != Some(binding.0.name.name.as_str()) {
                                        self.errors.push(format!(
                                            "let {} : {} = bind {}: monitor is bound to `{}`, but `{}` is a `{}`",
                                            l.name.name, simple, rhs.name,
                                            want.unwrap_or("?"),
                                            rhs.name, binding.0.name.name,
                                        ));
                                    }
                                    self.pad(depth);
                                    writeln!(self.out, "{simple} {};", l.name.name).ok();
                                    self.emit_bound_monitor_actors(
                                        &mon, &l.name.name, depth, &binding,
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
                        self.errors.push(format!(
                            "let {} : {} = bind <expr>: rhs must be a bare bus-binding name in v0",
                            l.name.name, simple,
                        ));
                        return;
                    }
                }
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
                                        &comp, &l.name.name, depth, &binding,
                                    ) {
                                        return;
                                    }
                                    let tag = format!("_{}_", comp.kind.keyword());
                                    self.emit_component_handler_registrations_bound(
                                        &comp, &l.name.name, depth, &tag, Some(binding),
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
        if let Some(TypeExpr::Builtin { name: BuiltinTy::Event, args, .. }) = &l.ty {
            let inner = args.first().map(|a| match a {
                TypeArg::Type(t) => txn_field_c_type(t),
                _ => "uint64_t".into(),
            }).unwrap_or_else(|| "uint64_t".into());
            self.pad(depth);
            writeln!(self.out, "std::vector<std::function<void({inner})>> {};", l.name.name).ok();
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
            let ty = if rhs_wants_auto(v, &self.tseq_names) { "auto" } else { "int64_t" };
            write!(self.out, "{ty} {} = ", l.name.name).ok();
            self.emit_expr(v);
            writeln!(self.out, ";").ok();
        } else if let Some(t) = &l.ty {
            self.pad(depth);
            // No initializer. Transactions / scoreboards / monitors all
            // default-construct (their struct field defaults run). Monitors
            // additionally trigger `on`-handler registration into _checkers
            // — see emit_monitor_handler_registrations.
            let simple = type_simple_name(Some(t));
            if let Some(name) = simple {
                if self.transactions.contains(name) || self.scoreboards.contains(name) {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    return;
                }
                if let Some(mon) = self.monitors.get(name).cloned() {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    self.emit_monitor_handler_registrations(&mon, &l.name.name, depth);
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
                    // Also register handlers for sub-component fields
                    // (e.g. an `env` whose fields are `drv : MyDriver` —
                    // each sub-component's on-handlers wire to the
                    // sub-instance's path).
                    for ci in &comp.items {
                        if let ComponentItem::Field(f) = ci {
                            if let Some(field_ty) = type_simple_name(Some(&f.ty)) {
                                if let Some(sub) = self.components.get(field_ty).cloned() {
                                    let sub_inst = format!("{}.{}", l.name.name, f.name.name);
                                    let sub_tag = format!("_{}_{}_", comp.kind.keyword(), f.name.name);
                                    self.emit_component_handler_registrations(&sub, &sub_inst, depth, &sub_tag);
                                }
                            }
                        }
                    }
                    // Wire `connect a -> b` edges. Each edge installs a
                    // bridge subscriber on `<env>.<a>` that fans out to
                    // every subscriber of `<env>.<b>`. Generic-lambda
                    // payload (`auto`) so we don't need to look up the
                    // event's type at the connect site.
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

    fn emit_expr_with_arrow(&mut self, e: &Expr, _lvalue: bool) {
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
            ExprKind::Int(s) => { write!(self.out, "{}", c_int_literal(s)).ok(); }
            ExprKind::Float(s) => { write!(self.out, "{s}").ok(); }
            ExprKind::Time(s) => {
                // Strip the unit suffix and emit the numeric portion.
                let n: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '_').collect();
                write!(self.out, "{n}").ok();
            }
            ExprKind::String(s) => { write!(self.out, "\"{}\"", escape_c(s)).ok(); }
            ExprKind::Bool(b) => { write!(self.out, "{}", if *b { "true" } else { "false" }).ok(); }
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
                // Bus-bound binding lowering. If `target` is an Ident
                // bound to a bus (`let axil : BusAxiLite = bind dut`),
                // resolve `axil.signal` to `<root>-><axil>_<signal>`.
                // Two-level form (`axil.aw.addr`) walks into the bus's
                // handshake_channel groupings and emits
                // `<root>-><axil>_aw_addr`.
                if let Some(s) = self.try_emit_bus_field_access(target, name) {
                    write!(self.out, "{s}").ok();
                    return;
                }
                // Pointer-typed identifiers (DUTs, threaded as Named-type
                // params) use `->`. Everything else uses `.`. Tracked in
                // `pointer_vars` — populated from `let x : <NamedType>` and
                // function params with a Named type (see emit_function).
                let is_pointer_root = matches!(&*target.kind,
                    ExprKind::Ident(id) if self.pointer_vars.contains(&id.name)
                );
                self.emit_expr(target);
                if is_pointer_root {
                    write!(self.out, "->").ok();
                } else {
                    write!(self.out, ".").ok();
                }
                write!(self.out, "{}", name.name).ok();
            }
            ExprKind::Index { target, index } => {
                self.emit_expr(target);
                write!(self.out, "[").ok();
                self.emit_expr(index);
                write!(self.out, "]").ok();
            }
            ExprKind::Call { callee, args } => {
                // Method-call rewrite: `obj.method(args)` where `obj`'s
                // type is a known component with a `hookable method`
                // lowers to `<Type>_method(obj, args)`. Falls through to
                // the generic call shape when no method match is found,
                // so plain free-function calls keep working.
                if let Some((comp_ty, instance, method)) =
                    self.resolve_component_method_call(callee)
                {
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
                    if i > 0 { write!(self.out, ", ").ok(); }
                    match a {
                        CallArg::Expr(ex) => self.emit_expr(ex),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Cast { expr, .. } => {
                // C++ TB drops casts for v0 — the underlying integer math is the same.
                self.emit_expr(expr);
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-", UnaryOp::Not => "!", UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                write!(self.out, "{s}").ok();
                self.emit_expr(expr);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.emit_expr(lhs);
                let s = c_binary_op(*op);
                write!(self.out, " {s} ").ok();
                self.emit_expr(rhs);
            }
            ExprKind::Ternary { cond, then_branch, else_branch } => {
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
                if let Some(l) = lo { self.emit_expr(l); }
                write!(self.out, "..").ok();
                if let Some(h) = hi { self.emit_expr(h); }
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

/// True if any `randomize(t) with <constraints>` exists anywhere in the
/// merged AST. Drives the `<z3++.h>` include + Z3 link flags.
fn file_uses_constraint_solver(file: &SourceFile) -> bool {
    fn block(b: &Block) -> bool { b.stmts.iter().any(stmt) }
    fn stmt(s: &Stmt) -> bool {
        match &s.kind {
            StmtKind::Randomize { with_body, .. } => !with_body.is_empty(),
            StmtKind::For(f) => block(&f.body),
            StmtKind::Repeat(r) => block(&r.body),
            StmtKind::Loop(b) => block(b),
            StmtKind::While { body, .. } => block(body),
            StmtKind::If(i) =>
                block(&i.then_block)
                || i.else_block.as_ref().map_or(false, block)
                || i.elsifs.iter().any(|(_, b)| block(b)),
            StmtKind::Fork(f) => f.branches.iter().any(block),
            StmtKind::Parallel(bs) | StmtKind::Schedule(bs) => bs.iter().any(block),
            StmtKind::Select(arms) => arms.iter().any(|a| block(&a.action)),
            StmtKind::On(h) => block(&h.body),
            StmtKind::After { body, .. } => block(body),
            _ => false,
        }
    }
    file.items.iter().any(|it| match it {
        Item::Function(f) => block(&f.body),
        Item::Test(t) => t.items.iter().any(|ti| match ti {
            TestItem::Stmt(s) => stmt(s),
            TestItem::Scope(sc) =>
                sc.setup.as_ref().map_or(false, block)
                || sc.run.as_ref().map_or(false, block)
                || sc.check.as_ref().map_or(false, block)
                || sc.teardown.as_ref().map_or(false, block),
            _ => false,
        }),
        _ => false,
    })
}

/// Pick a C++ representation for a scoreboard field. Mostly the same as
/// `txn_field_c_type` but supports `queue<T>` → `HarcQueue<T>` (the small
/// runtime template emitted at file scope when scoreboards are present).
fn scoreboard_field_c_type(t: &TypeExpr) -> String {
    if let TypeExpr::Builtin { name: BuiltinTy::Queue, args, .. } = t {
        let inner = args.first().map(|a| match a {
            TypeArg::Type(ty) => txn_field_c_type(ty),
            _ => "uint64_t".into(),
        }).unwrap_or_else(|| "uint64_t".into());
        return format!("HarcQueue<{inner}>");
    }
    txn_field_c_type(t)
}

/// Pick a C++ representation for a transaction field's HARC type. Conservative
/// — small ints get widened to `uint64_t`/`int64_t`; bool stays bool; named
/// types get the bare name (likely an enum which is `int64_t` in v0).
fn txn_field_c_type(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Builtin { name, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => "uint64_t".into(),
            BuiltinTy::SInt | BuiltinTy::SIntCap => "int64_t".into(),
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
    let ExprKind::Call { callee, args } = &*event.kind else { return None; };
    let ExprKind::Field { target, name: method } = &*callee.kind else { return None; };
    if method.name != "handshake" { return None; }
    let ExprKind::Field { target: outer, name: ch } = &*target.kind else { return None; };
    let ExprKind::Ident(id) = &*outer.kind else { return None; };
    if id.name != bus_ident { return None; }
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
    let ExprKind::Call { callee, args } = &*event.kind else { return None; };
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
    if let TypeExpr::Builtin { name: BuiltinTy::TSeq, args, .. } = t {
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

fn c_type_for(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => "uint64_t".into(),
            BuiltinTy::SInt | BuiltinTy::SIntCap => "int64_t".into(),
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
                let inner = args.first().map(|a| match a {
                    TypeArg::Type(ty) => txn_field_c_type(ty),
                    TypeArg::Expr(e) => match &*e.kind {
                        ExprKind::Ident(id) => id.name.clone(),
                        _ => "uint64_t".into(),
                    },
                    _ => "uint64_t".into(),
                }).unwrap_or_else(|| "uint64_t".into());
                format!("HarcQueue<{inner}>&")
            }
            // Aggregates / verification-only types fall back to the spelling
            // — caller will get a compile error pointing at the gap.
            _ => format!("/* TODO: type {:?} */ uint64_t", name),
        }
        TypeExpr::Named { name, .. } => {
            // User-defined module types lower to Verilator pointers (`VFoo*`).
            // Matches the `let dut : AxiLiteRegs` → `VAxiLiteRegs* dut` rule
            // already used for the test-level DUT decl.
            let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            format!("V{last}*")
        }
    }
}

fn c_binary_op(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+", Sub => "-", Mul => "*", Div => "/", Mod => "%",
        Eq => "==", Ne => "!=", Lt => "<", Le => "<=", Gt => ">", Ge => ">=",
        AndAnd | AndKw => "&&",
        OrOr | OrKw => "||",
        BitAnd => "&", BitOr => "|", BitXor => "^",
        Shl => "<<", Shr => ">>",
        // Temporal / membership operators have no direct C++ equivalent — they
        // shouldn't appear in v0 cpp_tb input. Emit a placeholder rather than
        // fail hard so the whole emit doesn't abort on one stray operator.
        PipeImplies | PipeImpliesNext | Throughout | Within | Intersect
        | In | Inside => "/* unsupported-op */ ,",
    }
}

fn c_int_literal(s: &str) -> String {
    // ARCH-style sized literals (`8'hAB`) → C++ `0xAB`.
    if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        return match kind {
            'h' | 'H' => format!("0x{digits}"),
            'b' | 'B' => format!("0b{digits}"),
            _ => digits,
        };
    }
    s.replace('_', "")
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
fn process_interp(s: &str) -> (String, Vec<String>) {
    let mut fmt = String::with_capacity(s.len());
    let mut captures = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Walk to matching `}` (no nested braces in v0).
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'}' { j += 1; }
            if j >= bytes.len() { break; } // unmatched — bail and keep what we have
            let inner = std::str::from_utf8(&bytes[i + 2..j]).unwrap_or("").trim();
            // Split on the last `:` to extract an optional format spec.
            let (expr_src, spec) = match inner.rfind(':') {
                Some(idx) => (inner[..idx].trim(), inner[idx + 1..].trim()),
                None => (inner, ""),
            };
            captures.push(expr_src.to_string());
            fmt.push_str(&translate_fmt_spec(spec));
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

/// Translate a HARC format spec (Python/Rust f-string subset) to a printf
/// conversion specifier. Supports flags + width + integer type; falls back
/// to `%lld` for unrecognised specs.
fn translate_fmt_spec(spec: &str) -> String {
    if spec.is_empty() { return "%lld".to_string(); }
    let last = spec.chars().last().unwrap();
    let prefix = &spec[..spec.len() - last.len_utf8()];
    match last {
        'd' => format!("%{prefix}lld"),
        'x' => format!("%{prefix}llx"),
        'X' => format!("%{prefix}llX"),
        'o' => format!("%{prefix}llo"),
        _   => "%lld".to_string(),
    }
}

/// Convert a HARC time literal (`5ns`, `20ns`, `100ps`, `2us`, `1ms`, `1s`)
/// to picoseconds. Returns `Err(msg)` on unrecognised units. The `cycles`
/// suffix is rejected here — clock periods must be wall-clock time.
fn time_literal_to_ps(s: &str) -> Result<i64, String> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '_').collect();
    let unit: String = s.chars().skip(digits.len()).collect();
    let n: i64 = digits.replace('_', "").parse()
        .map_err(|_| format!("bad number in time literal `{s}`"))?;
    match unit.as_str() {
        "ps" => Ok(n),
        "ns" => Ok(n * 1_000),
        "us" => Ok(n * 1_000_000),
        "ms" => Ok(n * 1_000_000_000),
        "s"  => Ok(n * 1_000_000_000_000),
        other => Err(format!("unsupported time unit `{other}` in `{s}` (expected ps/ns/us/ms/s)")),
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
            let id = match &*callee.kind { ExprKind::Ident(id) => id, _ => return None };
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
        if properties.contains_key(&id.name) { return true; }
    }
    contains_temporal(expr)
}

fn contains_temporal(e: &Expr) -> bool {
    if match_temporal_call(e).is_some() { return true; }
    match &*e.kind {
        ExprKind::Binary { op, lhs, rhs } => {
            matches!(op, BinaryOp::PipeImplies | BinaryOp::PipeImpliesNext
                       | BinaryOp::Throughout | BinaryOp::Within | BinaryOp::Intersect)
                || contains_temporal(lhs) || contains_temporal(rhs)
        }
        ExprKind::SystemCall { args, .. } => args.iter().any(contains_temporal),
        ExprKind::HashHash { .. } | ExprKind::SeqRepeat { .. } => true,
        ExprKind::Field { target, .. } => contains_temporal(target),
        ExprKind::Index { target, index } => contains_temporal(target) || contains_temporal(index),
        ExprKind::BitSlice { target, hi, lo } =>
            contains_temporal(target) || contains_temporal(hi) || contains_temporal(lo),
        ExprKind::Call { callee, args } => {
            contains_temporal(callee) || args.iter().any(|a| match a {
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
            ExprKind::Index { target, index } => { walk(target, out); walk(index, out); }
            ExprKind::BitSlice { target, hi, lo } => { walk(target, out); walk(hi, out); walk(lo, out); }
            ExprKind::Call { callee, args } => {
                walk(callee, out);
                for a in args {
                    if let CallArg::Expr(e) = a { walk(e, out); }
                    else if let CallArg::Named { value, .. } = a { walk(value, out); }
                }
            }
            ExprKind::Cast { expr, .. } => walk(expr, out),
            ExprKind::Send { target, value } => { walk(target, out); walk(value, out); }
            ExprKind::Unary { expr, .. } => walk(expr, out),
            ExprKind::Binary { lhs, rhs, .. } => { walk(lhs, out); walk(rhs, out); }
            ExprKind::Paren(inner) => walk(inner, out),
            ExprKind::SystemCall { args, .. } => for a in args { walk(a, out); },
            ExprKind::HashHash { expr, .. } => walk(expr, out),
            ExprKind::SeqRepeat { expr, .. } => walk(expr, out),
            ExprKind::SetLit(items) => for i in items { walk(i, out); },
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
