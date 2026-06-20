//! TB-IR → C++ testbench backend (`--codegen tbir`).
//!
//! Consumes a verified `ir::TbProgram` and emits a Verilator-class C++
//! TB whose scaffolding mirrors the live v1 (`cpp_tb`) contract —
//! preamble, `HarcTestContext`, clock scheduler, `sim_log_line`
//! plumbing, trace events, exit status, and the `--test`/`HARC_TEST`
//! dispatcher `main()` — so a tbir binary is a drop-in replacement on
//! both the `--sv` (Verilator) and `--dut` (arch sim) paths and its
//! semantic trace diffs clean against v1.
//!
//! Function bodies use a loop-switch over `BlockId` instead of
//! re-structured control flow; see `func.rs`.

mod covergroup;
mod expr;
mod func;
mod runtime;

use crate::ast::SourceFile;
use crate::codegen::cpp_tb::{EmitError, EmitOpts, GeneratedCppFile, SplitCppOutput};
use crate::ir::{self, TbProgram};
use std::collections::HashSet;
use std::fmt::Write as _;

const INDENT: &str = "    ";

/// Whether a testbench needs a `_tb` host struct emitted (and a `_tb`
/// instance declared in each owning test). Non-synthetic testbenches
/// always do — they own the DUT handle plus cov/scoreboard/transactor
/// state. A SYNTHETIC testbench (classic `test` form, no `testbench`
/// binding) normally has none, but it acquires one when it carries
/// promoted scalar fields: a test-scope `let` written in `run` and read
/// in `check` is promoted to a `_tb` scalar field so its value persists
/// across the run→check boundary (the two phases lower to separate IR
/// functions). The promoted field is the only `_tb` member used in that
/// case — the unused `dut` handle stays a nullptr (synthetic tests use a
/// bare `dut` local).
fn needs_tb_struct(tb: &ir::TestbenchSchema) -> bool {
    !tb.synthetic || !tb.scalar_fields.is_empty()
}

pub fn emit(prog: &TbProgram, file: &SourceFile, opts: &EmitOpts) -> Result<String, EmitError> {
    if prog.tests.is_empty() {
        return Err(EmitError("no `test` declaration found".to_string()));
    }

    // All tests in one binary share the DUT type (same v0 rule as v1).
    let dut_type = validate_tests_share_dut(prog, "one binary")?;

    // `generate_if`-gated bus signals: lowering kept every binding's gates
    // intact but could not evaluate them (no param env). Resolve each
    // ACCESSED bus-bound signal's gate against the effective param env now
    // (the emitter has `EmitOpts` + the `SourceFile`), erroring on a
    // gated-OFF access exactly as v1's `bus_signal_present` / gated-OFF
    // diagnostic does. A gated-OFF signal that is never accessed is silent.
    check_gated_bus_access(prog, file, opts)?;

    let test_names: Vec<String> = prog.tests.iter().map(|t| t.name.clone()).collect();

    // Constraint-solver wiring (randomize sites). The runtime problem
    // table + per-site Z3-solve snippets are emitted by v1's shared
    // constraint codegen ("only the call site moves to the IR backend").
    // Empty when the program has no randomize site — the TB then never
    // links Z3, exactly like v1.
    let problem_table_cpp = if prog.constraint_sites.is_empty() {
        String::new()
    } else {
        let solver_table = crate::solver::problem_table::build_typed_solver_problem_table(file);
        let runtime_table =
            crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(&solver_table);
        if runtime_table.problems.is_empty() {
            String::new()
        } else {
            runtime_table.render_cpp_table("_harc_runtime_random_problem_table")
        }
    };
    // Per-`ConstraintRef` Z3-solve snippets, emitted at the loop-switch
    // body depth (run/check fn = depth 2 → block stmts at depth 5).
    let randomize_snippets =
        crate::codegen::cpp_tb::emit_randomize_snippets(file, opts, &prog.constraint_sites, 5)?;

    let mut out = String::new();
    // Probe reads/forces dereference `dut->rootp->...`, which needs the
    // root struct's full definition (`V<Top>___024root.h`) — the `rootp`
    // member in `V<Top>.h` is only a forward-declared pointer. Mirrors
    // v1's `aggregated_probes` include gate. See docs/probe-signals.md.
    let has_probes = program_has_probes(&prog);
    runtime::preamble(
        &mut out,
        &dut_type,
        &test_names,
        &problem_table_cpp,
        has_probes,
    );

    // Transaction value-record structs, in declaration order. Mirrors
    // v1's `emit_record_struct` shape (field defaults as member
    // initializers, `operator==`/`!=`). v1's other record companions
    // — `randomize_<T>` and the pack/unpack helpers — are NOT emitted:
    // every construct that could reach them (`randomize`, bus sends)
    // is rejected at lowering, so they would be dead text here. They
    // land with their constructs.
    for r in &prog.records {
        record_struct(&mut out, r);
    }

    // RAL per-register write-callback recursion-depth limit — emitted once
    // when any testbench registers `on regs.REG` callbacks (mirrors v1's
    // `#ifndef HARC_RAL_CB_MAX_DEPTH` block). Guards a callback that
    // re-enters `record_write` from blowing the host stack.
    if prog
        .testbenches
        .iter()
        .any(|tb| tb.regblock_bindings.iter().any(|b| !b.callbacks.is_empty()))
    {
        out.push_str(
            "#ifndef HARC_RAL_CB_MAX_DEPTH\n\
             static constexpr uint32_t HARC_RAL_CB_MAX_DEPTH = 16;\n\
             #endif\n",
        );
    }

    // Scoreboard structs (data-only host-state records — they never name
    // a TB or DUT type), before the testbench structs that hold them.
    for sb in &prog.scoreboards {
        runtime::scoreboard_struct(&mut out, sb, &prog.records);
    }

    // Composite-component structs (env/agent cluster). A component holds
    // its sub-components by value, so each held type's struct must be
    // defined first. Source order usually puts subs before the env, but
    // a user may declare the env first — so emit in dependency order
    // (DFS over `Sub` fields), mirroring v1's `topo_sort_component_indices`.
    for ci in component_emit_order(prog) {
        runtime::component_struct(
            &mut out,
            &prog.components[ci],
            &prog.components,
            &prog.scoreboards,
            &prog.records,
        );
    }

    // Covergroup structs (leaf observables — they never name a TB or
    // DUT type), then one struct per unique non-synthetic testbench.
    for cg in &prog.covgroups {
        covergroup::covgroup_struct(&mut out, cg);
    }
    // Lowered pure helpers — file-scope C++ functions. Declaration
    // order is source order, which is not necessarily topological for
    // helper-to-helper calls, so prototypes go first.
    let helpers: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| f.kind == ir::FunctionKind::Helper)
        .collect();
    for h in &helpers {
        func::emit_helper_prototype(&mut out, h);
    }
    if !helpers.is_empty() {
        writeln!(out).ok();
    }
    for h in &helpers {
        func::emit_helper_function(&mut out, h)?;
        writeln!(out).ok();
    }

    // File-scope `extern "C" { … }` forward declarations for every
    // `extern function name(...) -> ret` (spec §9). Shared with v1 so
    // both codegens emit byte-identical declarations; the call sites
    // resolve to `CallTarget::ExternFn` (raw symbol name). Writes
    // nothing when the program declares no extern fns.
    crate::codegen::cpp_tb::emit_extern_fn_decls(&mut out, file);

    // One struct per unique testbench that needs a `_tb` host struct.
    // Non-synthetic testbenches always get one. A SYNTHETIC testbench
    // (classic `test` form, no `testbench` binding) normally has no
    // `_tb` — but it still needs one when it carries promoted scalar
    // fields: a test-scope `let` written in `run` and read in `check`
    // is promoted to a `_tb` field so it persists across the run→check
    // boundary (run and check are separate IR functions). See
    // `needs_tb_struct`.
    let mut seen = HashSet::new();
    for tb in &prog.testbenches {
        if needs_tb_struct(tb) && seen.insert(tb.name.clone()) {
            let cov_fields: Vec<(String, String)> = tb
                .cov_fields
                .iter()
                .map(|(f, cg)| (f.clone(), prog.covgroups[cg.index()].name.clone()))
                .collect();
            let sb_fields: Vec<(String, String)> = tb
                .scoreboard_fields
                .iter()
                .map(|(f, sb)| (f.clone(), prog.scoreboards[sb.index()].name.clone()))
                .collect();
            runtime::tb_struct(
                &mut out,
                &tb.name,
                &dut_type,
                &cov_fields,
                &tb.scalar_fields,
                &sb_fields,
            );
        }
    }
    runtime::context_struct(&mut out, &dut_type);

    for t in &prog.tests {
        emit_test(&mut out, prog, t, &dut_type, opts, &randomize_snippets)?;
    }

    runtime::dispatcher(&mut out, &test_names);
    Ok(out)
}

/// Emit a dispatcher plus one or more self-contained C++ translation units
/// for TB-IR tests. The split happens after lowering: each shard keeps the
/// full lowered scaffolding (records, components, helpers, randomize tables)
/// and emits only its selected `run_<Test>` functions. This mirrors the v1
/// split-linkage contract while avoiding a shared C++ ABI for TB-IR runtime
/// internals.
pub fn emit_split_tests_with_file_prefix(
    prog: &TbProgram,
    file: &SourceFile,
    opts: EmitOpts,
    file_prefix: &str,
    group_size: usize,
) -> Result<SplitCppOutput, EmitError> {
    let group_size = group_size.max(1);
    if prog.tests.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }
    validate_tests_share_dut(prog, "one split binary")?;

    let test_names: Vec<String> = prog.tests.iter().map(|t| t.name.clone()).collect();
    let shard_count = test_names.len().div_ceil(group_size);
    let mut files = Vec::with_capacity(shard_count + 1);
    files.push(GeneratedCppFile {
        filename: format!("{file_prefix}main.cpp"),
        contents: emit_split_dispatcher(&test_names),
    });

    for (shard_idx, shard_names) in test_names.chunks(group_size).enumerate() {
        let mut shard_prog = prog.clone();
        shard_prog.tests = prog
            .tests
            .iter()
            .filter(|t| shard_names.iter().any(|name| name == &t.name))
            .cloned()
            .collect();
        let cpp = emit(&shard_prog, file, &opts)?;
        let filename = if group_size == 1 {
            format!(
                "{file_prefix}test_{}.cpp",
                crate::codegen::cpp_tb::sanitize_file_component(&shard_names[0])
            )
        } else {
            format!("{file_prefix}shard{}.cpp", shard_idx + 1)
        };
        files.push(GeneratedCppFile {
            filename,
            contents: strip_generated_dispatcher(&cpp)?,
        });
    }

    Ok(SplitCppOutput { files, test_names })
}

fn validate_tests_share_dut(prog: &TbProgram, scope: &str) -> Result<String, EmitError> {
    let dut_type = prog.testbench(prog.tests[0].testbench).dut_type.clone();
    for t in &prog.tests {
        let tb = prog.testbench(t.testbench);
        if tb.dut_type != dut_type {
            return Err(EmitError(format!(
                "multi-DUT tests in {scope} are out of scope for v0; \
                 test `{}` uses `{}`, but a previous test used `{}`",
                t.name, tb.dut_type, dut_type,
            )));
        }
    }
    Ok(dut_type)
}

fn strip_generated_dispatcher(cpp: &str) -> Result<String, EmitError> {
    let marker = "\nint main(int argc, char** argv) {";
    let Some(idx) = cpp.rfind(marker) else {
        return Err(EmitError(
            "internal error: generated TB-IR C++ did not contain dispatcher main()".into(),
        ));
    };
    let mut out = cpp[..idx].trim_end().to_string();
    out.push('\n');
    Ok(out)
}

fn emit_split_dispatcher(test_names: &[String]) -> String {
    let mut out = String::new();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(out, "// HARC TB-IR split-test dispatcher.").ok();
    writeln!(out).ok();
    writeln!(out, "#include <cstring>").ok();
    writeln!(out, "#include \"harc_log_rt.h\"").ok();
    writeln!(out).ok();
    for name in test_names {
        writeln!(out, "extern int run_{name}(int argc, char** argv);").ok();
    }
    writeln!(out).ok();
    runtime::dispatcher(&mut out, test_names);
    out
}

/// Does any function in the program access a DUT-internal probe (a
/// `PortRef` whose access class is `Probe`/`Force`)? Drives the
/// `V<Top>___024root.h` include in the preamble — required for the
/// `dut->rootp->...` probe accessor to compile. Probe reads can sit
/// inline in expression position (assert conditions, format args, RHS),
/// so this walks both statement-level `PortRef`s and the expression
/// trees those statements carry.
fn program_has_probes(prog: &TbProgram) -> bool {
    prog.functions
        .iter()
        .any(|f| f.blocks.iter().any(|b| b.stmts.iter().any(stmt_has_probe)))
}

fn port_is_probe(p: &ir::PortRef) -> bool {
    !matches!(p.access, ir::PortAccess::Port)
}

fn expr_has_probe(e: &ir::Expr) -> bool {
    use ir::Expr::*;
    match e {
        Port(p) => port_is_probe(p),
        Binary(_, a, b) => expr_has_probe(a) || expr_has_probe(b),
        Unary(_, a) => expr_has_probe(a),
        BitSlice { target, .. } => expr_has_probe(target),
        Ternary(c, a, b) => expr_has_probe(c) || expr_has_probe(a) || expr_has_probe(b),
        WidthCast { inner, .. } => expr_has_probe(inner),
        Call(_, args) => args.iter().any(expr_has_probe),
        ComponentIdle { n, .. } => expr_has_probe(n),
        _ => false,
    }
}

fn fmt_has_probe(args: &ir::FmtArgs) -> bool {
    args.args.iter().any(|a| expr_has_probe(&a.expr))
}

fn stmt_has_probe(s: &ir::Stmt) -> bool {
    use ir::Stmt::*;
    match s {
        DutWrite(p, e) => port_is_probe(p) || expr_has_probe(e),
        DutRead(_, p) | ProbeRelease(p) => port_is_probe(p),
        Assign(_, e)
        | RecordFieldWrite { value: e, .. }
        | RecordWriteCb { value: e, .. }
        | TbFieldWrite { value: e, .. }
        | TransactorStateWrite { value: e, .. }
        | ComponentFieldWrite { value: e, .. }
        | TransactorCall { call: e, .. }
        | TransactorSelfCall { call: e, .. } => expr_has_probe(e),
        AssertCheck { cond, on_fail } => expr_has_probe(cond) || fmt_has_probe(on_fail),
        Log { args, .. } => fmt_has_probe(args),
        FailDiag { guard, args } => {
            guard.as_ref().is_some_and(expr_has_probe) || fmt_has_probe(args)
        }
        ScoreboardOp { op, .. } => match op {
            ir::ScoreboardOp::QueuePush { value, .. }
            | ir::ScoreboardOp::ScalarWrite { value, .. } => expr_has_probe(value),
            ir::ScoreboardOp::QueuePop { .. } => false,
        },
        ComponentEmit { args, .. } | ComponentCall { args, .. } => args.iter().any(expr_has_probe),
        SeqPush { value, .. } | ComponentQueuePush { value, .. } => expr_has_probe(value),
        ComponentQueuePop { .. } | ComponentSubAssign { .. } => false,
        TlmFork(desc) => desc.args.iter().any(expr_has_probe),
        TlmJoinAll(pending) => pending.iter().any(|p| p.args.iter().any(expr_has_probe)),
        RecordInit(_, _) | CovReport(_) => false,
    }
}

/// Per-bind effective `generate_if` param env, mirroring v1's
/// `bus_param_envs` population (`cpp_tb.rs` ~2200): bus defaults overlaid
/// with the bind-site generic (`let s : BusRw<...> = bind dut`) and then
/// the DUT-port override (`port s: target BusRw<WRITE=0>`, sourced into
/// `opts.dut_bus_port_overrides`). The bind name equals the DUT port name
/// by convention, so the override map is keyed by the bind name.
struct GatedBus<'a> {
    decl: &'a crate::ast::BusDecl,
    env: std::collections::HashMap<String, i64>,
}

/// Error out if any function ACCESSES a `generate_if`-gated bus signal
/// that is gated OFF under its bind's effective param env. Mirrors v1's
/// access-site behavior: lowering carries the gates, emission decides
/// presence against the override-applied env so the tbir port set matches
/// `arch build`'s flattened port set for the same DUT override. Ungated
/// signals, and gated-ON signals, resolve normally; a gated-OFF signal
/// that is never accessed is silent (it simply never reaches a PortRef).
fn check_gated_bus_access(
    prog: &TbProgram,
    file: &SourceFile,
    opts: &EmitOpts,
) -> Result<(), EmitError> {
    use crate::ast::{BusDecl, Item, TestItem, TypeExpr};

    // Bus declarations in the file, by simple name (inline or `use`-imported).
    let mut buses: std::collections::HashMap<&str, &BusDecl> = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Bus(b) = it {
            buses.insert(b.name.name.as_str(), b);
        }
    }
    // Bind-site type expr per bind name, for the bind-site generic layer
    // (`let s : BusRw<...> = bind dut`). Recovered from the file's test
    // lets — lowering does not carry the bind `TypeExpr`. First binding
    // name wins on a cross-test collision (matches v1's downstream-bind
    // pre-scan), which is irrelevant in practice since binds are per-test.
    let mut bind_ty: std::collections::HashMap<&str, &TypeExpr> = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Test(t) = it {
            for ti in &t.items {
                if let TestItem::Let(l) = ti {
                    if l.bind {
                        if let Some(ty) = l.ty.as_ref() {
                            bind_ty.entry(l.name.name.as_str()).or_insert(ty);
                        }
                    }
                }
            }
        }
    }

    // bind-name -> (BusDecl, effective env). Drawn from every testbench's
    // `bus_bindings` (the binding's `field` is the bind name == flat signal
    // prefix == DUT port name). Buses with no gated signals at all are
    // skipped — they can never produce a gated-OFF access.
    let mut gated: std::collections::HashMap<String, GatedBus<'_>> = std::collections::HashMap::new();
    for tb in &prog.testbenches {
        for b in &tb.bus_bindings {
            if gated.contains_key(&b.field) {
                continue;
            }
            let Some(&decl) = buses.get(b.bus.as_str()) else {
                continue;
            };
            // Only plain bus signals are gate-checked (mirroring v1's
            // `bus_signal_present`), so a bus whose only gates sit on
            // handshake payloads needs no env built.
            if !decl.signals.iter().any(|s| s.gate.is_some()) {
                continue;
            }
            let env = crate::codegen::cpp_tb::bus_param_env_with_port_override(
                decl,
                bind_ty.get(b.field.as_str()).copied(),
                opts.dut_bus_port_overrides.get(&b.field),
            );
            gated.insert(b.field.clone(), GatedBus { decl, env });
        }
    }
    if gated.is_empty() {
        return Ok(());
    }

    // Walk every PortRef the program accesses; collect gated-OFF errors.
    let mut errors: Vec<String> = Vec::new();
    let mut check = |p: &ir::PortRef| {
        if let Some(err) = gated_off_error(p, &gated) {
            if !errors.contains(&err) {
                errors.push(err);
            }
        }
    };
    for f in &prog.functions {
        for blk in &f.blocks {
            for s in &blk.stmts {
                for_each_port_in_stmt(s, &mut check);
            }
            for_each_port_in_term(&blk.terminator, &mut check);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(EmitError(errors.join("\n")))
    }
}

/// `Some(message)` when PortRef `p` is rooted at a gated bus bind and
/// names a PLAIN bus signal that is gated OFF under that bind's effective
/// env. `None` otherwise. The message text mirrors v1's gated-OFF
/// diagnostic verbatim.
///
/// Scope deliberately matches v1's `bus_signal_present` exactly: only the
/// 2-segment `[bind, plain-signal]` form is gate-checked. v1 resolves
/// handshake-channel access (`[bind, ch, sig]` and the pre-flattened
/// `[bind, ch_sig]` form) WITHOUT a gate check — see
/// `try_emit_bus_field_access` (`cpp_tb.rs` ~9255+), where the handshake
/// branches return the port unconditionally. Mirroring that here keeps
/// the tbir backend from rejecting an access v1 accepts. A 1-segment
/// remapped path (`bind...with`) is likewise not gate-checked by v1.
fn gated_off_error(
    p: &ir::PortRef,
    gated: &std::collections::HashMap<String, GatedBus<'_>>,
) -> Option<String> {
    if !matches!(p.access, ir::PortAccess::Port) {
        return None;
    }
    let [bind, sig] = p.port_path.as_slice() else {
        return None;
    };
    let g = gated.get(bind)?;
    let s = g.decl.signals.iter().find(|s| &s.name.name == sig)?;
    if crate::codegen::cpp_tb::gate_passes(s.gate.as_ref(), &g.env) {
        return None;
    }
    Some(format!(
        "bus `{}` (binding `{}`) signal `{}` is gated OFF by its \
         `generate_if` condition under the bind's params — `arch build` \
         omits this port, so the testbench must not access it",
        g.decl.name.name, bind, sig,
    ))
}

/// Invoke `f` on every `PortRef` reachable from statement `s` (the
/// statement's own port operands and any port reads nested in its
/// expression trees). Parallels `stmt_has_probe`'s traversal, collecting
/// instead of testing.
fn for_each_port_in_stmt(s: &ir::Stmt, f: &mut impl FnMut(&ir::PortRef)) {
    use ir::Stmt::*;
    match s {
        DutWrite(p, e) => {
            f(p);
            for_each_port_in_expr(e, f);
        }
        DutRead(_, p) | ProbeRelease(p) => f(p),
        Assign(_, e)
        | RecordFieldWrite { value: e, .. }
        | RecordWriteCb { value: e, .. }
        | TbFieldWrite { value: e, .. }
        | TransactorStateWrite { value: e, .. }
        | ComponentFieldWrite { value: e, .. }
        | TransactorCall { call: e, .. }
        | TransactorSelfCall { call: e, .. } => for_each_port_in_expr(e, f),
        AssertCheck { cond, on_fail } => {
            for_each_port_in_expr(cond, f);
            for_each_port_in_fmt(on_fail, f);
        }
        Log { args, .. } => for_each_port_in_fmt(args, f),
        FailDiag { guard, args } => {
            if let Some(g) = guard {
                for_each_port_in_expr(g, f);
            }
            for_each_port_in_fmt(args, f);
        }
        ScoreboardOp { op, .. } => match op {
            ir::ScoreboardOp::QueuePush { value, .. }
            | ir::ScoreboardOp::ScalarWrite { value, .. } => for_each_port_in_expr(value, f),
            ir::ScoreboardOp::QueuePop { .. } => {}
        },
        ComponentEmit { args, .. } | ComponentCall { args, .. } => {
            args.iter().for_each(|a| for_each_port_in_expr(a, f))
        }
        SeqPush { value, .. } | ComponentQueuePush { value, .. } => for_each_port_in_expr(value, f),
        ComponentQueuePop { .. } | ComponentSubAssign { .. } => {}
        TlmFork(desc) => desc.args.iter().for_each(|a| for_each_port_in_expr(a, f)),
        TlmJoinAll(pending) => pending
            .iter()
            .for_each(|p| p.args.iter().for_each(|a| for_each_port_in_expr(a, f))),
        RecordInit(_, _) | CovReport(_) => {}
    }
}

/// Invoke `f` on every `PortRef` in a block terminator's expression
/// operands (`Branch`/`WaitCycles`/`WaitUntil` conditions can read a bus
/// signal).
fn for_each_port_in_term(t: &ir::Terminator, f: &mut impl FnMut(&ir::PortRef)) {
    use ir::Terminator::*;
    match t {
        Branch(e, _, _)
        | WaitCycles(e, _, _)
        | WaitCyclesSync(e, _) => for_each_port_in_expr(e, f),
        WaitUntil { preds, .. } => preds.iter().for_each(|p| for_each_port_in_expr(&p.expr, f)),
        WaitUntilTimeout { preds, cycles, .. } => {
            preds.iter().for_each(|p| for_each_port_in_expr(&p.expr, f));
            for_each_port_in_expr(cycles, f);
        }
        Fatal(args) => for_each_port_in_fmt(args, f),
        Jump(_) | WaitTimePs(_, _) | Randomize { .. } | Return => {}
    }
}

fn for_each_port_in_fmt(args: &ir::FmtArgs, f: &mut impl FnMut(&ir::PortRef)) {
    args.args.iter().for_each(|a| for_each_port_in_expr(&a.expr, f));
}

/// Invoke `f` on every `PortRef` in an expression tree. Parallels
/// `expr_has_probe`'s structural traversal.
fn for_each_port_in_expr(e: &ir::Expr, f: &mut impl FnMut(&ir::PortRef)) {
    use ir::Expr::*;
    match e {
        Port(p) => f(p),
        RecordField { index, .. } => {
            if let Some(i) = index {
                for_each_port_in_expr(i, f);
            }
        }
        Binary(_, a, b) => {
            for_each_port_in_expr(a, f);
            for_each_port_in_expr(b, f);
        }
        Unary(_, a) => for_each_port_in_expr(a, f),
        BitSlice { target, .. } => for_each_port_in_expr(target, f),
        Ternary(c, a, b) => {
            for_each_port_in_expr(c, f);
            for_each_port_in_expr(a, f);
            for_each_port_in_expr(b, f);
        }
        WidthCast { inner, .. } => for_each_port_in_expr(inner, f),
        SeqIndex { index, .. } => for_each_port_in_expr(index, f),
        Call(_, args) => args.iter().for_each(|a| for_each_port_in_expr(a, f)),
        ComponentIdle { n, .. } => for_each_port_in_expr(n, f),
        _ => {}
    }
}

/// One transaction value-record struct. Field C types follow v1's
/// `txn_field_c_type` for the lowered (≤64-bit scalar) subset:
/// unsigned → `uint64_t`, signed → `int64_t`, bool/bit → `bool`.
/// C++ storage type for a record field's scalar (or Vec element) type,
/// mirroring v1's `record_field_c_type` / `txn_field_c_type` choices:
/// `bool` for Bool, `int64_t` for SInt, `uint64_t` otherwise.
fn field_scalar_cty(ty: &ir::IrType) -> &'static str {
    match ty {
        ir::IrType::Bool => "bool",
        ir::IrType::SInt(_) => "int64_t",
        _ => "uint64_t",
    }
}

/// C++ storage type for a loop-switch local / method param. Every scalar
/// ≤64 bits widens to `uint64_t` (the established tbir value model — even
/// `bool`/`sint` locals are u64-backed here, distinct from v1's narrower
/// per-type choice, which is value-identical in the loop-switch model). A
/// 65..128-bit `uint`/`sint` uses v1's `_harc_u128` (`__uint128_t`), while
/// wider declared scalars use the shared `HarcWide<N>` runtime storage.
/// Aggregate types (`Record`/`RecordSeq`) are handled by their own
/// declaration sites, never this helper.
pub(super) fn local_scalar_cty(ty: &ir::IrType) -> String {
    match ty {
        ir::IrType::UInt(Some(w)) | ir::IrType::SInt(Some(w)) if *w > 128 => {
            format!("harc_rt::HarcWide<{}>", (*w as usize).div_ceil(32).max(1))
        }
        ir::IrType::UInt(Some(w)) | ir::IrType::SInt(Some(w)) if *w > 64 => {
            "_harc_u128".to_string()
        }
        _ => "uint64_t".to_string(),
    }
}

/// Packed-bit width of a record field's scalar (or Vec element) type —
/// the declared width (`Bool` → 1). Mirrors v1's `packed_width` for the
/// scalar leaves the record subset lowers. `None` for a widthless
/// scalar (no defined layout); lowering already rejects those for Vec
/// fields, and scalar fields never reach the pack helpers when any sum
/// is undefined.
fn field_packed_width(ty: &ir::IrType) -> Option<usize> {
    match ty {
        ir::IrType::Bool => Some(1),
        ir::IrType::UInt(w) | ir::IrType::SInt(w) => w.map(|w| w as usize),
        _ => None,
    }
}

/// Total packed-bit width of a record (sum of every field's packed
/// width; a `Vec<T, N>` field contributes `N * width(T)`). `None` when
/// any field has no defined packed width — the pack helpers are then
/// skipped, exactly as v1's `try_fold` over `packed_width` does.
fn record_packed_width(r: &ir::RecordSchema) -> Option<usize> {
    r.fields.iter().try_fold(0usize, |acc, f| {
        let w = field_packed_width(&f.ty)?;
        Some(acc + w * f.vec_len.unwrap_or(1))
    })
}

fn record_struct(out: &mut String, r: &ir::RecordSchema) {
    writeln!(out, "struct {} {{", r.name).ok();
    for f in &r.fields {
        let cty = field_scalar_cty(&f.ty);
        if let Some(n) = f.vec_len {
            // `Vec<T, N>` field → `std::array<T, N>` member, zero-filled
            // (v1's `record_field_c_type` Vec branch + `{}` default).
            writeln!(out, "{INDENT}std::array<{cty}, {n}> {} = {{}};", f.name).ok();
            continue;
        }
        let init = match f.ty {
            ir::IrType::Bool => if f.default.is_some_and(|d| d != 0) {
                "true"
            } else {
                "false"
            }
            .to_string(),
            _ => f.default.unwrap_or(0).to_string(),
        };
        writeln!(out, "{INDENT}{cty} {} = {init};", f.name).ok();
    }
    writeln!(out, "}};").ok();
    if r.fields.is_empty() {
        writeln!(
            out,
            "inline bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
            r.name
        )
        .ok();
    } else {
        let eq = r
            .fields
            .iter()
            .map(|f| format!("a.{0} == b.{0}", f.name))
            .collect::<Vec<_>>()
            .join(" && ");
        writeln!(
            out,
            "inline bool operator==(const {0}& a, const {0}& b) {{ return {eq}; }}",
            r.name
        )
        .ok();
    }
    writeln!(
        out,
        "inline bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
        r.name
    )
    .ok();
    writeln!(out).ok();
    record_pack_helpers(out, r);
}

/// Emit `harc_pack_<R>` / `harc_unpack_<R>` / `harc_drive_<R>` for a
/// record that crosses a lowered TLM response pin — v1's
/// `emit_record_pack_helpers`. `harc_pack` lays each field into a
/// `HarcWide<words>` LSB-first in *reverse* declaration order (so the
/// first field occupies the high bits, matching the SV packed-struct
/// convention); `harc_unpack` / `harc_drive` carry a `requires`
/// fast-path that copies field-wise when the response pin is exposed as
/// a struct (Verilator packed struct), falling back to the bit layout
/// for a flat wide wire. Skipped when the record has no defined packed
/// width (a widthless scalar field) — exactly v1's `try_fold` guard.
fn record_pack_helpers(out: &mut String, r: &ir::RecordSchema) {
    let Some(width) = record_packed_width(r) else {
        return;
    };
    let name = &r.name;
    let words = width.div_ceil(32).max(1);

    // harc_pack: LSB-first, reverse declaration order.
    writeln!(
        out,
        "static harc_rt::HarcWide<{words}> harc_pack_{name}(const {name}& value) {{"
    )
    .ok();
    writeln!(out, "{INDENT}harc_rt::HarcWide<{words}> _packed{{}};").ok();
    let mut offset = 0usize;
    for f in r.fields.iter().rev() {
        let w = field_packed_width(&f.ty).unwrap_or(0);
        if let Some(n) = f.vec_len {
            for i in 0..n {
                writeln!(
                    out,
                    "{INDENT}harc_rt::harc_wide_write_bits(_packed, {off}, {w}, value.{fld}[{i}]);",
                    off = offset + i * w,
                    fld = f.name
                )
                .ok();
            }
            offset += w * n;
        } else {
            writeln!(
                out,
                "{INDENT}harc_rt::harc_wide_write_bits(_packed, {offset}, {w}, value.{fld});",
                fld = f.name
            )
            .ok();
            offset += w;
        }
    }
    writeln!(out, "{INDENT}return _packed;").ok();
    writeln!(out, "}}").ok();

    // harc_unpack: struct-shaped pin fast-path, else bit layout.
    writeln!(
        out,
        "template<typename Raw> static {name} harc_unpack_{name}(const Raw& raw) {{"
    )
    .ok();
    let raw_checks: Vec<String> = r.fields.iter().map(|f| format!("raw.{}", f.name)).collect();
    if !raw_checks.is_empty() {
        writeln!(
            out,
            "{INDENT}if constexpr (requires {{ {}; }}) {{",
            raw_checks.join("; ")
        )
        .ok();
        writeln!(out, "{0}{0}{name} value{{}};", INDENT).ok();
        for f in &r.fields {
            if let Some(n) = f.vec_len {
                for i in 0..n {
                    writeln!(out, "{0}{0}value.{1}[{i}] = raw.{1}[{i}];", INDENT, f.name).ok();
                }
            } else {
                writeln!(out, "{0}{0}value.{1} = raw.{1};", INDENT, f.name).ok();
            }
        }
        writeln!(out, "{0}{0}return value;", INDENT).ok();
        writeln!(out, "{INDENT}}} else {{").ok();
    }
    writeln!(
        out,
        "{INDENT}auto _packed = harc_rt::harc_wide_zext<{words}>(harc_rt::harc_read(raw));"
    )
    .ok();
    writeln!(out, "{INDENT}{name} value{{}};").ok();
    let mut offset = 0usize;
    for f in r.fields.iter().rev() {
        let w = field_packed_width(&f.ty).unwrap_or(0);
        let cty = field_scalar_cty(&f.ty);
        if let Some(n) = f.vec_len {
            for i in 0..n {
                let off = offset + i * w;
                writeln!(
                    out,
                    "{INDENT}value.{fld}[{i}] = ({cty})harc_rt::harc_bits(_packed, {hi}, {off});",
                    fld = f.name,
                    hi = off + w - 1
                )
                .ok();
            }
            offset += w * n;
        } else {
            writeln!(
                out,
                "{INDENT}value.{fld} = ({cty})harc_rt::harc_bits(_packed, {hi}, {offset});",
                fld = f.name,
                hi = offset + w - 1
            )
            .ok();
            offset += w;
        }
    }
    writeln!(out, "{INDENT}return value;").ok();
    if !raw_checks.is_empty() {
        writeln!(out, "{INDENT}}}").ok();
    }
    writeln!(out, "}}").ok();

    // harc_drive: struct-shaped sig fast-path, else pack-and-assign.
    writeln!(
        out,
        "template<typename Sig> static void harc_drive_{name}(Sig& sig, const {name}& value) {{"
    )
    .ok();
    if !raw_checks.is_empty() {
        let sig_checks: Vec<String> = raw_checks
            .iter()
            .map(|s| s.replacen("raw.", "sig.", 1))
            .collect();
        writeln!(
            out,
            "{INDENT}if constexpr (requires {{ {}; }}) {{",
            sig_checks.join("; ")
        )
        .ok();
        for f in &r.fields {
            if let Some(n) = f.vec_len {
                for i in 0..n {
                    writeln!(out, "{0}{0}sig.{1}[{i}] = value.{1}[{i}];", INDENT, f.name).ok();
                }
            } else {
                writeln!(out, "{0}{0}sig.{1} = value.{1};", INDENT, f.name).ok();
            }
        }
        writeln!(out, "{INDENT}}} else {{").ok();
        writeln!(
            out,
            "{0}{0}harc_rt::harc_assign(sig, harc_pack_{name}(value));",
            INDENT
        )
        .ok();
        writeln!(out, "{INDENT}}}").ok();
    } else {
        writeln!(
            out,
            "{INDENT}harc_rt::harc_assign(sig, harc_pack_{name}(value));"
        )
        .ok();
    }
    writeln!(out, "}}").ok();
    writeln!(out).ok();
}

/// Dependency order for component-struct emission: a component appears
/// after every component it holds as a by-value `Sub` field, so the held
/// struct is already defined. DFS post-order over the `Sub` edges, in
/// id order for determinism (mirrors v1's `topo_sort_component_indices`).
/// The IR rejects sub-component cycles at lowering (a by-value cycle is
/// not constructible), so the visited-set DFS terminates.
fn component_emit_order(prog: &TbProgram) -> Vec<usize> {
    let n = prog.components.len();
    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    fn visit(i: usize, prog: &TbProgram, visited: &mut [bool], order: &mut Vec<usize>) {
        if visited[i] {
            return;
        }
        visited[i] = true;
        for f in &prog.components[i].fields {
            if let ir::ComponentFieldKind::Sub { component } = &f.kind {
                visit(component.index(), prog, visited, order);
            }
        }
        order.push(i);
    }
    for i in 0..n {
        visit(i, prog, &mut visited, &mut order);
    }
    order
}

/// Register every `on <ev>(arg)` handler on `component` (and nested
/// sub-components) as a subscriber closure on the corresponding event
/// field of the instance reached by `inst_path`. The closure bumps the
/// owning instance's `_last_in_cycle` activity stamp, then runs the
/// handler body lambda — mirroring v1's `on`-subscriber registration.
fn emit_on_handler_regs(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    inst_path: &str,
    // When true, the top component's OWN `on <ev>` handlers are NOT
    // registered as synchronous subscribers — they were re-lowered into a
    // queue-fed worker-coroutine actor (`emit_active_bound_driver_actor`)
    // under `--mt`, which replaces the synchronous driver. Nested
    // sub-components are still registered normally. Always `false` on the
    // cooperative-default path, so default output is unchanged.
    skip_top_on_handlers: bool,
) {
    let comp = &prog.components[component.index()];
    if !skip_top_on_handlers {
        for oh in &comp.on_handlers {
            let lambda = func::on_handler_lambda_name(comp, oh);
            writeln!(
                out,
                "{INDENT}{inst_path}.{}.push_back([&](auto _t) {{ {inst_path}._last_in_cycle = (uint64_t)cycle_count; {lambda}({inst_path}, _t); }});",
                oh.event
            )
            .ok();
        }
    }
    // Recurse into by-value sub-components (an env holding an agent).
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            emit_on_handler_regs(out, prog, *sub, &sub_path, false);
        }
    }
}

/// Emit one resolved `connect` edge's subscriber push_back, rooted at
/// `inst_path` (the instance reaching the connect's owning component).
fn emit_one_connect(
    out: &mut String,
    prog: &TbProgram,
    inst_path: &str,
    edge: &ir::ConnectEdgeSchema,
) {
    let src = std::iter::once(inst_path.to_string())
        .chain(edge.src_path.iter().cloned())
        .collect::<Vec<_>>()
        .join(".");
    let sink = std::iter::once(inst_path.to_string())
        .chain(edge.sink_path.iter().cloned())
        .collect::<Vec<_>>()
        .join(".");
    match &edge.sink {
        ir::ConnectSink::Method { method } => {
            let sink_comp = &prog.components[edge.sink_component.index()].name;
            writeln!(
                out,
                "{INDENT}{src}.{}.push_back([&](auto _t) {{ {sink_comp}_{method}({sink}, _t); }});",
                edge.src_event
            )
            .ok();
        }
        ir::ConnectSink::Event { event } => {
            // event→event bridge: forward each emit on the source event into
            // the sink event's own subscriber list, firing the sink driver's
            // registered `on <ev>` handler(s). Mirrors v1's
            // `for (auto& _s : <sink>.<event>) _s(_t);` bridge closure.
            writeln!(
                out,
                "{INDENT}{src}.{}.push_back([&](auto _t) {{ for (auto& _s : {sink}.{event}) _s(_t); }});",
                edge.src_event
            )
            .ok();
        }
    }
}

/// Install the `connect` bridges of every by-value sub-component of
/// `component` (reached via `inst_path`), recursing depth-first. Used for
/// an env that holds an agent: the agent's own
/// `sequencer.dispatched -> drv.req` bridge lives on the agent's schema and
/// must be installed at `<env>.<agent>` scope. The top component's OWN
/// connects are emitted by the caller (`cf.connects`); this only walks the
/// nested sub-components.
fn emit_nested_connects(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    inst_path: &str,
) {
    let comp = &prog.components[component.index()];
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            let sub_comp = &prog.components[sub.index()];
            for edge in &sub_comp.connects {
                emit_one_connect(out, prog, &sub_path, edge);
            }
            emit_nested_connects(out, prog, *sub, &sub_path);
        }
    }
}

/// Default watchdog clause values (spec §8.6), applied when the source
/// omits the `period`/`max_idle` clause. Mirror v1's
/// `WATCHDOG_DEFAULT_PERIOD` / `WATCHDOG_DEFAULT_MAX_IDLE`.
const WATCHDOG_DEFAULT_PERIOD: i64 = 1000;
const WATCHDOG_DEFAULT_MAX_IDLE: i64 = 10000;

/// Install the `_checkers` closures for a component's `on <N> cycles`
/// periodic handlers and its `watchdog` (and those of any nested
/// sub-component), each gated on a per-instance static last-fire stamp.
/// Mirrors v1's `emit_watchdog_checker` / periodic `_checkers` shape:
/// every cycle the closure re-reads the period (so a field-backed period
/// stays test-overridable), fires once it is due, and — for the watchdog
/// — runs the user body then the idle check.
fn emit_lifecycle_checkers(
    out: &mut String,
    prog: &TbProgram,
    component: ir::ComponentId,
    inst_path: &str,
) -> Result<(), EmitError> {
    let comp = &prog.components[component.index()];
    // A valid C++ identifier for the static tag (`env.agent` → `env_agent`).
    let inst_tag = inst_path.replace('.', "_");

    for ph in &comp.periodic_handlers {
        let lambda = func::periodic_handler_lambda_name(comp, ph);
        let period = func::clause_expr_cpp(prog, ph.function, inst_path, &ph.period)?;
        let tag = format!("_per_{inst_tag}_{}", ph.function.0);
        let svc = ph.phase.service_vec();
        writeln!(out, "{INDENT}{svc}.push_back([&]() {{").ok();
        writeln!(out, "{INDENT}{INDENT}static int64_t {tag}_last = 0;").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{tag}_last = (int64_t)cycle_count;"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }

    // Cycle-trigger handlers (`on <bool-expr>`). Each installs a
    // `_checkers` closure that re-evaluates the trigger predicate every
    // primary-clock cycle and fires the body when the predicate satisfies
    // the requested edge mode. Mirrors v1's `emit_cycle_trigger`.
    for ch in &comp.cycle_handlers {
        let lambda = func::cycle_handler_lambda_name(comp, ch);
        let trigger = func::clause_expr_cpp(prog, ch.function, inst_path, &ch.trigger)?;
        let tag = format!("_cyc_{inst_tag}_{}", ch.function.0);
        if ch.monitor_channel.is_some() {
            // Bound-bus handshake monitor (v1's `emit_bound_monitor_actors`).
            // v1 lowers it as a coroutine: `while (true) { co_await
            // wait_until(valid && ready); <capture+body>; co_await
            // wait_cycles(1); }`. Because the post-body `wait_cycles(1)`
            // re-parks in `wait_until` (which never resumes same-tick), v1
            // captures one beat, then SKIPS exactly the next cycle before
            // re-arming — so a continuously-held handshake samples every
            // OTHER cycle (e.g. held over cycles 5,6,7 → beats at 5 and 7),
            // NOT every cycle (Level) and NOT only the rising edge.
            //
            // The `_checkers` pass runs once per primary cycle at the same
            // phase the monitor coroutine would resume (`sched.tick()` then
            // checkers), so the cadence is reproduced exactly with a
            // fire-then-cooldown latch: fire when the predicate holds, then
            // consume the following cycle as the `wait_cycles(1)` re-arm.
            //
            // NOTE on `--mt`: the monitor stays a cooperative `_checkers`
            // latch even under `--mt`, deliberately NOT a worker coroutine.
            // v1 can run the monitor on its own OS thread only because v1
            // ALSO re-lowers the active bound *driver* transactor into a
            // queue-fed worker coroutine that yields (`co_await
            // wait_cycles`) every cycle — so the handshake is established
            // and observed inside the same barrier window. tbir keeps the
            // driver in the run coroutine (synchronous `tick()` spins that
            // never reach the main-loop barrier window), so a worker-thread
            // monitor would miss every handshake. Re-lowering active
            // transactors into actors is a separate, larger change (see
            // issue #425 / the WS2 follow-up). Keeping the monitor as the
            // `_checkers` latch is trace-correct under both `--mt` and the
            // default — the latch already fires at the right phases on
            // every `tick()`.
            writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
            writeln!(out, "{INDENT}{INDENT}static bool {tag}_cool = false;").ok();
            writeln!(out, "{INDENT}{INDENT}if ({tag}_cool) {{").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{tag}_cool = false;").ok();
            writeln!(out, "{INDENT}{INDENT}}} else if ((bool)({trigger})) {{").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
            writeln!(out, "{INDENT}{INDENT}{INDENT}{tag}_cool = true;").ok();
            writeln!(out, "{INDENT}{INDENT}}}").ok();
            writeln!(out, "{INDENT}}});").ok();
            continue;
        }
        writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
        match ch.edge {
            ir::CycleEdge::Level => {
                writeln!(out, "{INDENT}{INDENT}if ((bool)({trigger})) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
            }
            ir::CycleEdge::Rising | ir::CycleEdge::Falling => {
                writeln!(out, "{INDENT}{INDENT}static bool {tag}_prev = false;").ok();
                writeln!(out, "{INDENT}{INDENT}bool {tag}_curr = (bool)({trigger});").ok();
                let cond = match ch.edge {
                    ir::CycleEdge::Rising => format!("!{tag}_prev && {tag}_curr"),
                    ir::CycleEdge::Falling => format!("{tag}_prev && !{tag}_curr"),
                    ir::CycleEdge::Level => unreachable!(),
                };
                writeln!(out, "{INDENT}{INDENT}if ({cond}) {{").ok();
                writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
                writeln!(out, "{INDENT}{INDENT}}}").ok();
                writeln!(out, "{INDENT}{INDENT}{tag}_prev = {tag}_curr;").ok();
            }
        }
        writeln!(out, "{INDENT}}});").ok();
    }

    if let Some(w) = &comp.watchdog {
        let lambda = func::watchdog_lambda_name(comp, w);
        let period = match &w.period {
            Some(e) => func::clause_expr_cpp(prog, w.function, inst_path, e)?,
            None => WATCHDOG_DEFAULT_PERIOD.to_string(),
        };
        let max_idle = match &w.max_idle {
            Some(e) => func::clause_expr_cpp(prog, w.function, inst_path, e)?,
            None => WATCHDOG_DEFAULT_MAX_IDLE.to_string(),
        };
        let tag = format!("_wdog_{inst_tag}_{}", w.function.0);
        writeln!(out, "{INDENT}_checkers.push_back([&]() {{").ok();
        writeln!(out, "{INDENT}{INDENT}static int64_t {tag}_last = 0;").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}int64_t {tag}_period = (int64_t)({period});"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{tag}_last = (int64_t)cycle_count;"
        )
        .ok();
        // 1. User body (typically a heartbeat log).
        writeln!(out, "{INDENT}{INDENT}{INDENT}{lambda}({inst_path});").ok();
        // 2. Idle check — trips FAIL when BOTH activity stamps are
        //    `max_idle` cycles behind. Mirrors v1's emit_watchdog idle
        //    block (framework error-counter bump on trip).
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}int64_t {tag}_max_idle = (int64_t)({max_idle});"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}if ({tag}_max_idle > 0 \
             && (int64_t)((uint64_t)cycle_count - {inst_path}._last_in_cycle) >= {tag}_max_idle \
             && (int64_t)((uint64_t)cycle_count - {inst_path}._last_out_cycle) >= {tag}_max_idle) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{INDENT}sim_log_line(\"FAIL\", \"watchdog: {} has been idle for >= %lld cycles\", (long long){tag}_max_idle);",
            comp.name
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}ctx.errors++;").ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}});").ok();
    }

    // Recurse into by-value sub-components (an env holding an agent that
    // carries a watchdog / periodic handler).
    for f in &comp.fields {
        if let ir::ComponentFieldKind::Sub { component: sub } = &f.kind {
            let sub_path = format!("{inst_path}.{}", f.name);
            emit_lifecycle_checkers(out, prog, *sub, &sub_path)?;
        }
    }
    Ok(())
}

fn emit_test(
    out: &mut String,
    prog: &TbProgram,
    test: &ir::TestSchema,
    dut_type: &str,
    opts: &EmitOpts,
    randomize_snippets: &[String],
) -> Result<(), EmitError> {
    let tb = prog.testbench(test.testbench);
    let clocked = !test.clocks.is_empty();
    let mt = opts.mt;
    // `(sched_var, slot_var)` pairs for actors that run on a dedicated
    // OS worker thread under `--mt`. Empty in cooperative mode (actor
    // slots go into the global `sched` instead). Collected as actors are
    // emitted; consumed by the worker-spawn / barrier dance below.
    let mut actor_threads: Vec<(String, String)> = Vec::new();

    runtime::run_prologue(out, &test.name, dut_type);
    if clocked {
        runtime::clocked_scheduler(out, &test.clocks);
    } else {
        runtime::clockless_scheduler(out);
    }
    runtime::log_helpers_and_seed(out);

    if needs_tb_struct(tb) {
        writeln!(out, "{INDENT}{} _tb;", tb.name).ok();
    }
    // Closure-hook regblock mirrors: a binding with `on regs.REG` write
    // callbacks holds its mirror struct + recursion-depth counter as
    // SHARED test-scope state (declared once, captured by `[&]`), so the
    // run coroutine and every callback lambda hit the same cell. The
    // per-function mirror locals (Run + callbacks) are name-matched to
    // these and skipped at declaration time. Plain regblock bindings keep
    // their run-local mirror (no callbacks → no sharing needed).
    for b in &tb.regblock_bindings {
        if b.callbacks.is_empty() {
            continue;
        }
        let mirror_ty = &prog.records[prog.regblocks[b.regblock.index()].record.index()].name;
        writeln!(out, "{INDENT}{mirror_ty} {}{{}};", b.field).ok();
        writeln!(out, "{INDENT}uint32_t {}_cb_depth = 0;", b.field).ok();
    }
    // Hook-vector spine for hook-triggered covergroups
    // (`covergroup G @(drv.send(t) post)`). One
    // `std::vector<std::function<void(args)>> <Type>_<method>_pre/_post`
    // per transactor method that any cov field subscribes to, declared
    // here so both the cov sample-closure push (below) and the method
    // fan-out (`emit_method`) reach the same vectors by `[&]` capture.
    // Mirrors v1's `emit_hook_vectors`. Only methods used by THIS
    // testbench's transactor fields are declared.
    for (_field, xid) in &tb.transactor_fields {
        let schema = prog.transactor(*xid);
        for m in &schema.methods {
            if m.cov_hook_subs.is_empty() {
                continue;
            }
            covergroup::hook_vector_decls(out, prog, schema, m, INDENT)?;
        }
    }
    // Covergroup auto-sampler registration, in testbench-field
    // declaration order — the same `_checkers` slot v1 uses, so
    // sampling happens at the identical point in the cycle. Lowering
    // synthesized one SamplerAuto function per cov field; cross-check
    // the pairing so a lowering drift fails loudly here.
    let samplers: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| {
            f.owner == Some(test.testbench)
                && matches!(f.kind, ir::FunctionKind::SamplerAuto { .. })
        })
        .collect();
    if samplers.len() != tb.cov_fields.len() {
        return Err(EmitError(format!(
            "tbir: test `{}` has {} cov field(s) but {} SamplerAuto function(s)",
            test.name,
            tb.cov_fields.len(),
            samplers.len()
        )));
    }
    for ((field, cg), sampler) in tb.cov_fields.iter().zip(&samplers) {
        let ir::FunctionKind::SamplerAuto { covgroup } = &sampler.kind else {
            unreachable!("filtered to SamplerAuto above");
        };
        if covgroup != cg {
            return Err(EmitError(format!(
                "tbir: sampler `{}` is bound to cg{} but field `{field}` expects cg{}",
                sampler.name, covgroup.0, cg.0
            )));
        }
        let schema = &prog.covgroups[cg.index()];
        match &schema.trigger {
            ir::CovTrigger::PosedgeDutClk => {
                covergroup::sampler_registration(
                    out,
                    schema,
                    &format!("_tb.{field}"),
                    &opts.vec_lane_widths,
                )?;
            }
            ir::CovTrigger::Hook { method, side, .. } => {
                // Resolve the target method (the `covergroup_hooks` pass
                // already validated it) to learn its param signature, then
                // push the sample closure onto `<Type>_<method>_<side>`.
                let (xschema, mschema) = tb
                    .transactor_fields
                    .iter()
                    .find_map(|(_f, xid)| {
                        let xs = prog.transactor(*xid);
                        xs.method(method).map(|m| (xs, m))
                    })
                    .ok_or_else(|| {
                        EmitError(format!(
                            "tbir: hook-triggered covergroup `{}` references method \
                             `{method}` not found on any transactor field",
                            schema.name
                        ))
                    })?;
                covergroup::hook_sampler_registration(
                    out,
                    prog,
                    schema,
                    xschema,
                    mschema,
                    *side,
                    &format!("_tb.{field}"),
                    &opts.vec_lane_widths,
                )?;
            }
        }
    }
    // tseq generator lambdas — one `<Name>` per `tseq` declaration in the
    // file, declared before the run coroutine so its `[&]` capture sees
    // them (v1's `emit_tseq` placement). Each returns a
    // `std::vector<Record>` and runs the body's randomize/yield loop.
    for f in &prog.functions {
        if let ir::FunctionKind::Tseq { .. } = f.kind {
            func::emit_tseq(out, prog, f, &prog.records, randomize_snippets, 1)?;
        }
    }
    // Transactor method lambdas — one `<Type>_<method>` per method of
    // every transactor the testbench instantiates, declared before the
    // run coroutine so its `[&]` capture sees them (v1 emission order).
    // Two fields of the same transactor type share one lambda set: the
    // subset carries no per-instance state (the DUT bind is static).
    // Per-instance state structs for the unbound DUT-poking transactors
    // that carry persistent scalar state (`drv.last_read`). Declared
    // BEFORE the method lambdas so the lambdas' `[&]` capture binds the
    // instance struct the method bodies write (`drv.last_read = ...`) and
    // the run/check coroutine reads. Same per-instance struct shape as
    // the bound-to target form. The subset is one stateful instance per
    // transactor type (enforced at lowering), so the type-shared method
    // lambda references exactly this one instance.
    for (instance, xid) in &tb.unbound_state_actors {
        let schema = prog.transactor(*xid);
        runtime::target_state_struct_inst(out, schema, instance);
    }
    // Closure-hook bodies (`on <obj>.<method> pre/post` method hooks and
    // `on regs.REG` per-register write callbacks) — emitted as free
    // `[&]`-capturing lambdas BEFORE the transactor method lambdas and the
    // run coroutine, so both the firing method (`emit_method` pre/post
    // fan-out) and the `record_write` dispatch see them. They capture the
    // shared `_tb` host struct + transactor-state structs + regblock
    // mirror by reference — the host-state-promotion mechanism.
    for f in &prog.functions {
        if matches!(f.kind, ir::FunctionKind::TestHook) && f.owner == Some(test.testbench) {
            func::emit_test_hook(out, prog, f, dut_type, 1)?;
        }
    }
    let mut emitted_xactors = HashSet::new();
    for (_, xid) in &tb.transactor_fields {
        if !emitted_xactors.insert(*xid) {
            continue;
        }
        let schema = prog.transactor(*xid);
        for m in &schema.methods {
            func::declare_method_slot(out, prog, schema, m, 1)?;
        }
        for m in &schema.methods {
            func::emit_method(out, prog, schema, m, randomize_snippets, 1)?;
        }
    }
    // Bound-to target-side TLM responder instances: one per-instance
    // state struct (state fields + activity stamps), declared in the
    // test scope so both the run/check coroutine (`target.read_count`)
    // and the actor coroutines capture it by reference, then one
    // background-coroutine actor per target method.
    for actor in &tb.target_tlm_actors {
        let schema = prog.transactor(actor.transactor);
        runtime::target_state_struct_inst(out, schema, &actor.instance);
    }
    for actor in &tb.target_tlm_actors {
        func::emit_target_actor(
            out,
            prog,
            actor,
            &tb.bus_bindings,
            mt,
            &mut actor_threads,
            1,
        )?;
    }

    // Composite-component method lambdas — one `<Comp>_<method>` per
    // method of every component in the file, declared before the run
    // coroutine so its `[&]` capture (and the connect push_backs below)
    // see them. Dependency order (subs before holders) so a method body
    // that calls a sub-component's method sees that lambda first.
    for ci in component_emit_order(prog) {
        let comp = &prog.components[ci];
        for m in &comp.methods {
            func::emit_component_method(out, prog, comp, m, randomize_snippets, 1)?;
        }
        for oh in &comp.on_handlers {
            func::emit_component_on_handler(out, prog, comp, oh, randomize_snippets, 1)?;
        }
        for ph in &comp.periodic_handlers {
            func::emit_component_periodic_handler(out, prog, comp, ph, randomize_snippets, 1)?;
        }
        for ch in &comp.cycle_handlers {
            func::emit_component_cycle_handler(out, prog, comp, ch, randomize_snippets, 1)?;
        }
        if let Some(w) = &comp.watchdog {
            func::emit_component_watchdog(out, prog, comp, w, randomize_snippets, 1)?;
        }
    }
    // Composite-component test-scope instances (`let env : AnalysisEnv`):
    // a default-constructed run-scope local, then its env `connect`
    // push_backs (`<env>.<src_path>.<event>.push_back([&](auto _t){
    // <SinkComp>_<method>(<env>.<sink_path>, _t); })`). Declared at test
    // scope (before the coroutine) so run and check share the instance —
    // v1's `AnalysisEnv env;` placement.
    for cf in &tb.component_fields {
        let cname = &prog.components[cf.component.index()].name;
        writeln!(out, "{INDENT}{cname} {};", cf.field).ok();
        // The top component's own `connect` edges (an env's source→sink, or
        // an agent instantiated directly at test scope), plus any nested
        // sub-component's connects (an env holding an agent whose own
        // `sequencer.dispatched -> drv.req` bridge must be installed).
        for edge in &cf.connects {
            emit_one_connect(out, prog, &cf.field, edge);
        }
        emit_nested_connects(out, prog, cf.component, &cf.field);
        // An `active` bound event-driven transactor (`let drv : X active =
        // bind axil`) re-lowers its `on <ev>` driver into a queue-fed
        // worker-coroutine actor on its own `ThreadScheduler` under `--mt`,
        // mirroring v1's `try_emit_bound_driver_actor`: a pusher subscriber
        // makes `emit drv.req(t)` ENQUEUE (non-blocking), and a worker
        // coroutine drains the queue and drives the bus, yielding
        // (`co_await wait_cycles`) each cycle so it shares the per-posedge
        // barrier window with the bound monitor — which is exactly what lets
        // the cooperative `_checkers` monitor latch observe every handshake
        // under `--mt` (the gap #448 deferred).
        //
        // The synchronous on-handler subscriber (`emit_on_handler_regs`) is
        // SUPPRESSED for this instance under `--mt`: the worker coroutine IS
        // the driver now. (v1 leaves a second synchronous subscriber
        // registered too, but its `tick()`-spinning body would drive the bus
        // a SECOND time, and the tbir cooperative `_checkers` monitor latch
        // would then count each handshake twice — 10 writes instead of 5.
        // v1's monitor is itself a worker coroutine whose `wait_cycles(1)`
        // cadence happens to mask the redundant second drive; the tbir latch
        // does not, so emitting both double-counts. Running the driver as the
        // single concurrent worker is the correct execution model and yields
        // the right per-codegen verdict.) Cooperative default emits neither
        // queue nor worker and keeps the synchronous subscriber —
        // byte-identical output.
        let bound_drv = &prog.components[cf.component.index()];
        let relower_driver =
            mt && cf.active && bound_drv.bound_bus.is_some() && !bound_drv.on_handlers.is_empty();
        if relower_driver {
            func::emit_active_bound_driver_actor(
                out,
                prog,
                cf.component,
                &cf.field,
                &tb.bus_bindings,
                &mut actor_threads,
                1,
            )?;
        }
        // `on <ev>(arg)` handler registrations, for this component and any
        // nested sub-components (an env holding an agent). Each subscribes
        // to the event field on its owning instance, bumps the instance's
        // `_last_in_cycle` activity stamp, then runs the handler body —
        // mirroring v1's `on`-subscriber registration. Suppressed for the
        // top component when its driver was re-lowered into a worker actor
        // above (the worker replaces the synchronous driver under `--mt`).
        emit_on_handler_regs(out, prog, cf.component, &cf.field, relower_driver);
        // `on <N> cycles` periodic + `watchdog` lifecycle `_checkers`
        // closures, for this component and any nested sub-components.
        // Bound-bus handshake monitors stay cooperative `_checkers`
        // latches even under `--mt` (see the NOTE in
        // `emit_lifecycle_checkers`), so no actor registration is needed.
        emit_lifecycle_checkers(out, prog, cf.component, &cf.field)?;
    }

    writeln!(out, "{INDENT}harc_rt::ThreadSlot _run_slot;").ok();
    writeln!(out, "{INDENT}sched.slots.push_back(&_run_slot);").ok();
    writeln!(
        out,
        "{INDENT}_run_slot.thread = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
    )
    .ok();
    if !tb.synthetic {
        writeln!(out, "{INDENT}{INDENT}_tb.dut = dut;").ok();
    }
    func::emit_function(
        out,
        prog,
        prog.function(test.run),
        &prog.records,
        &tb.bus_bindings,
        &opts.vec_lane_widths,
        randomize_snippets,
        dut_type,
        2,
    )?;
    if let Some(check) = test.check {
        func::emit_function(
            out,
            prog,
            prog.function(check),
            &prog.records,
            &tb.bus_bindings,
            &opts.vec_lane_widths,
            randomize_snippets,
            dut_type,
            2,
        )?;
    }
    writeln!(out, "{INDENT}{INDENT}co_return;").ok();
    writeln!(out, "{INDENT}}}(&_run_slot);").ok();

    // Bootstrap (single-threaded) → spawn workers (`--mt` only) → drive
    // loop → shutdown workers. Ordering is load-bearing: per-actor
    // schedulers must be bootstrapped before their OS threads start, and
    // the shutdown handshake must follow the loop. The worker-setup /
    // shutdown emitters are no-ops when `actor_threads` is empty, so the
    // cooperative single-thread output stays byte-identical to before.
    runtime::drive_bootstrap(out, &actor_threads);
    runtime::mt_worker_setup(out, &actor_threads);
    runtime::drive_loop(out, clocked, &actor_threads);
    runtime::mt_worker_shutdown(out, &actor_threads);
    runtime::run_epilogue(out);
    Ok(())
}
