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
pub fn emit(file: &SourceFile) -> Result<String, EmitError> {
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
    let mut covergroups: std::collections::HashMap<String, CovergroupDecl> =
        std::collections::HashMap::new();
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    let mut enums = std::collections::HashMap::new();
    for it in &file.items {
        match it {
            Item::Scoreboard(c) => { scoreboards.insert(c.name.name.clone()); }
            Item::Monitor(c) => { monitors.insert(c.name.name.clone(), c.clone()); }
            Item::Covergroup(g) => { covergroups.insert(g.name.name.clone(), g.clone()); }
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
                || covergroups.contains_key(simple)
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
        covergroups,
        txn_fields,
        enums,
        properties,
        prop_subs: std::collections::HashMap::new(),
        event_types: std::collections::HashMap::new(),
        field_subs: std::collections::HashMap::new(),
        covers: Vec::new(),
        clock_names: clocks.iter().map(|c| c.name.name.clone()).collect(),
    };

    // Header.
    writeln!(e.out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(e.out, "// HARC test: {}", test.name.name).ok();
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
    writeln!(e.out, "#include <functional>").ok();
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

    // ── Monitor structs ──────────────────────────────────────────────────
    // Same shape as scoreboards; output `event<T>` fields lower to
    // std::vector<std::function<void(T)>>. The on-handlers don't go in
    // the struct — they're emitted at `let mon : MonName` time.
    for it in &file.items {
        if let Item::Monitor(m) = it {
            e.emit_monitor_struct(m);
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

    // Other lets get hoisted up front.
    for l in &other_lets {
        e.emit_let(l, 1);
    }

    // Walk all test items in declaration order. Bare stmts and scope sim/run
    // both contribute to the test body; lets are hoisted (handled above);
    // applies/uses/clocks have no runtime effect here.
    for it in &test.items {
        match it {
            TestItem::Stmt(s) => e.emit_stmt(s, 1),
            TestItem::Scope(s) => {
                if let Some(b) = &s.setup { e.emit_block(b, 1); }
                if let Some(b) = &s.run   { e.emit_block(b, 1); }
                if let Some(b) = &s.check { e.emit_block(b, 1); }
                if let Some(b) = &s.teardown { e.emit_block(b, 1); }
            }
            _ => {}
        }
    }

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
                let cty = monitor_field_c_type(&f.ty);
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
        // Build field-name substitution map for the body.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types = Vec::new();
        for it in &mon.items {
            if let ComponentItem::Field(f) = it {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
                // If field is event<T>, register its inner type so emit
                // codegen inside the body can use it.
                if let TypeExpr::Builtin { name: BuiltinTy::Event, args, .. } = &f.ty {
                    let inner = args.first().map(|a| match a {
                        TypeArg::Type(t) => txn_field_c_type(t),
                        _ => "uint64_t".into(),
                    }).unwrap_or_else(|| "uint64_t".into());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        let prev_subs = std::mem::replace(&mut self.field_subs, subs);
        // Add monitor field event types to event_types, so `on field(arg)`
        // inside the body resolves the arg type. Save/restore.
        let mut added_events = Vec::new();
        for (name, ty) in &local_event_types {
            if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                added_events.push(name.clone());
            }
        }

        for it in &mon.items {
            if let ComponentItem::OnHandler(h) = it {
                self.emit_cycle_trigger(h, depth, "_m_");
            }
        }

        // Restore state.
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
                // Expect `lo .. hi` range; bail otherwise for v0.
                if let ExprKind::RangeLit { lo: Some(lo), hi: Some(hi) } = &*f.iter.kind {
                    write!(self.out, "for (int64_t {var} = ").ok();
                    self.emit_expr(lo);
                    write!(self.out, "; {var} < ").ok();
                    self.emit_expr(hi);
                    writeln!(self.out, "; {var}++) {{").ok();
                    self.emit_block(&f.body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                } else {
                    self.errors.push("for-iter must be a range `lo .. hi` in v0 cpp_tb".into());
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
                write!(self.out, "for (int _ck = 0; _ck < ").ok();
                self.emit_expr(duration);
                writeln!(self.out, "; _ck++) tick();").ok();
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
            StmtKind::Emit { name, args, .. } => {
                // `emit e(v)` → call every subscriber. Bare-name lookup
                // first checks field_subs (so `emit write_e(...)` inside a
                // monitor body resolves to `mon.write_e`).
                let raw = name.segments.last()
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                let event_name = self.field_subs.get(&raw).cloned().unwrap_or(raw);
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
                // Two shapes:
                //   `on event_name(arg) ... end on` — event subscription.
                //       Event must be a bare ident or `<obj>.<event>`;
                //       arg is the lambda parameter name.
                //   `on <bool-expr> ... end on`     — cycle trigger; body
                //       runs every cycle the expression is true.
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
        self.pad(depth);
        if let Some(v) = &l.value {
            // `int64_t` (rather than `auto`) so 32-bit DUT signals zero-
            // extend on assignment into the local instead of going through
            // a narrower `int` and sign-extending on later widening. v0
            // assumes lets hold integer-shaped values; non-integer lets
            // need an explicit `let x : <Type> = ...`.
            write!(self.out, "int64_t {} = ", l.name.name).ok();
            self.emit_expr(v);
            writeln!(self.out, ";").ok();
        } else if let Some(t) = &l.ty {
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

/// Pick a C++ representation for a monitor field. `event<T>` (with or
/// without an `out` direction marker — we don't enforce direction in v0)
/// lowers to a vector of subscriber closures.
fn monitor_field_c_type(t: &TypeExpr) -> String {
    if let TypeExpr::Builtin { name: BuiltinTy::Event, args, .. } = t {
        let inner = args.first().map(|a| match a {
            TypeArg::Type(ty) => txn_field_c_type(ty),
            _ => "uint64_t".into(),
        }).unwrap_or_else(|| "uint64_t".into());
        return format!("std::vector<std::function<void({inner})>>");
    }
    scoreboard_field_c_type(t)
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
fn c_type_for(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Builtin { name, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => "uint64_t".into(),
            BuiltinTy::SInt | BuiltinTy::SIntCap => "int64_t".into(),
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "bool".into(),
            BuiltinTy::String => "const char*".into(),
            BuiltinTy::Time => "uint64_t".into(),
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
