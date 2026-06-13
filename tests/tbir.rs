//! TB-IR pipeline tests: AST → IR lowering snapshots, verifier checks,
//! `LowerError::Unsupported` stubs, and tbir C++ emission smoke tests.
//! The end-to-end v1-vs-tbir trace-equivalence gate runs out-of-band
//! (`harc sim --codegen {v1,tbir}` + `harc trace-diff`); these tests
//! lock the in-process shapes.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::passes::{lower_coroutine, placement};
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;
use std::path::Path;

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn lower_src(src: &str) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed = parse_source(src).expect("fixture parses");
    let merged = merge::merge_for_sim(std::slice::from_ref(&parsed), None).expect("merge");
    lower::lower_program(&merged)
}

/// Merged `SourceFile` for one source string (the input `tbir::emit`
/// needs for the constraint-IR / randomize seam — empty otherwise).
fn merged_src(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("fixture parses");
    merge::merge_for_sim(std::slice::from_ref(&parsed), None).expect("merge")
}

/// Multi-file variant for fixtures that split helpers across files
/// (mirrors how run_fixtures.sh loads them).
fn lower_fixtures(names: &[&str]) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed: Vec<_> = names
        .iter()
        .map(|n| parse_source(&fixture(n)).unwrap_or_else(|e| panic!("{n} parses: {e:?}")))
        .collect();
    let merged = merge::merge_for_sim(&parsed, None).expect("merge");
    lower::lower_program(&merged)
}

/// Lower + verify + emit one registry fixture through the tbir backend
/// with default options (the `--sv` Verilator path the equivalence
/// harness exercises).
fn emit_fixture_cpp(name: &str) -> String {
    let merged = merged_src(&fixture(name));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits")
}

/// The negative-test contract: every out-of-subset fixture must produce
/// `LowerError::Unsupported` whose rendered message names the offending
/// construct and points the user at `--codegen v1`.
fn assert_unsupported(err: &lower::LowerError) -> String {
    assert!(
        matches!(err, lower::LowerError::Unsupported { .. }),
        "must be LowerError::Unsupported: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("--codegen v1"),
        "unsupported error must suggest --codegen v1: {msg}"
    );
    msg
}

/// Locks the dump-ir text for the tracer-bullet fixture: testbench /
/// test schemas, block structure, port hoisting, loop shapes,
/// interpolated format args.
#[test]
fn top_counter_dump_ir_snapshot() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("top_counter_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the arbiter fixture: request-mask
/// writes with single-cycle waits, grant asserts carrying inline port
/// reads, a two-point covergroup, and check-phase CovBin reads.
#[test]
fn bus_arbiter_dump_ir_snapshot() {
    let prog = lower_src(&fixture("bus_arbiter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("bus_arbiter_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the ROM fixture: an impure helper
/// (`read_addr`) CFG-inlined at every call site, a full-address-space
/// covergroup, and the check-phase bin reads.
#[test]
fn rom_lut_dump_ir_snapshot() {
    let prog = lower_src(&fixture("rom_lut_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("rom_lut_dump_ir", format!("{prog}"));
}

// ── Emitted-C++ snapshots — the emission surface for the original
//    five fixtures of the equivalence matrix
//    (tests/tbir_equiv_fixtures.txt). Full files,
//    so any future emitter refactor diffs visibly here instead of
//    silently shifting shapes the marker tests don't cover. ──────────

#[test]
fn top_counter_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "top_counter_emitted_cpp",
        emit_fixture_cpp("top_counter_test.harc")
    );
}

#[test]
fn sync_fifo_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "sync_fifo_emitted_cpp",
        emit_fixture_cpp("sync_fifo_test.harc")
    );
}

#[test]
fn bus_arbiter_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "bus_arbiter_emitted_cpp",
        emit_fixture_cpp("bus_arbiter_test.harc")
    );
}

#[test]
fn wait_until_counter_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "wait_until_counter_emitted_cpp",
        emit_fixture_cpp("wait_until_counter_test.harc")
    );
}

#[test]
fn rom_lut_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "rom_lut_emitted_cpp",
        emit_fixture_cpp("rom_lut_test.harc")
    );
}

/// Locks the dump-ir text for the file-log fixture: `logf` statements
/// carrying `LogLevel::File` (path + severity) alongside console
/// info/warn logs with interpolated port reads.
#[test]
fn log_paths_dump_ir_snapshot() {
    let prog = lower_src(&fixture("log_paths_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("log_paths_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the fatal-path fixture: the
/// `LogLevel::Fatal` statement (errors++ AND `_fatal = true` in both
/// emitters) followed by a wait and a post-fatal statement that the
/// drive loop must never reach.
#[test]
fn fatal_path_dump_ir_snapshot() {
    let prog = lower_src(&fixture("fatal_path_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("fatal_path_dump_ir", format!("{prog}"));
}

/// Transactor fixtures are outside the MVP subset — the error must
/// name the construct and point at `--codegen v1`, never mis-lower.
/// The fixture's `transaction` items now lower (records slice), and
/// its `transactor` declaration now passes the item-level scan
/// (transactor slice); the scan trips on the `tseq` declaration
/// instead. (The transactor itself would also be rejected — its
/// `req : in event<RegOp>` field is out of subset — but `tseq` sits
/// earlier in the file-gate order.)
#[test]
fn transactor_fixture_is_unsupported() {
    let err = lower_src(&fixture("axilite_seqdrv_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    insta::assert_snapshot!("axilite_seqdrv_unsupported", msg);
}

/// Randomize/constraint fixture (`randomize(t) with` + Z3 constraints,
/// loaded together with its helper file exactly as run_fixtures.sh
/// does) now lowers through the constraint-IR seam: every `randomize`
/// site becomes a `Terminator::Randomize` carrying a `ConstraintRef`.
/// (Was a negative test before this slice; see git history.)
#[test]
fn randomize_fixture_lowers() {
    let prog = lower_fixtures(&["axilite_constraint_test.harc", "axilite_regs_test.harc"])
        .expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    assert!(
        !prog.constraint_sites.is_empty(),
        "the `randomize(p) with` site lowered into a constraint site"
    );
    let randomize_blocks = prog
        .functions
        .iter()
        .flat_map(|f| &f.blocks)
        .filter(|b| matches!(b.terminator, ir::Terminator::Randomize { .. }))
        .count();
    assert!(randomize_blocks >= 1, "a Randomize terminator is present");
}

/// Agent/event fixture (`agent`, `event<T>`) is outside the MVP
/// subset. Its `transaction` item now lowers (records slice), so the
/// item-level scan trips on the `agent` declaration instead.
#[test]
fn wait_until_quiesce_fixture_is_unsupported() {
    let err = lower_src(&fixture("wait_until_quiesce_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    insta::assert_snapshot!("wait_until_quiesce_unsupported", msg);
}

/// Untimed `any of` lowers to a `WaitUntil` terminator in `AnyOf`
/// mode with every sub-predicate kept inline (the emitter `||`-joins
/// them, matching v1's disjunction).
#[test]
fn wait_until_any_of_lowers_to_any_of_mode() {
    let src = r#"
test WaitUntilAnyTest
    let dut : Top
    run
        wait until any of dut.ready == 1, dut.count_out == 2
    end run
end test WaitUntilAnyTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntil { preds, mode, .. } = &f.blocks[0].terminator else {
        panic!("expected WaitUntil terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::AnyOf);
    assert_eq!(preds.len(), 2);
    assert_eq!(preds[0].src_text, "dut.ready == 1");
    assert_eq!(preds[1].src_text, "dut.count_out == 2");
}

/// Timed `any of` carries v1's any-of timeout diagnostics: the default
/// header is "wait until any of timed out after %lld cycles" and the
/// breakdown is ONE unguarded "  none of: <src1>, <src2>" line (a
/// timed-out any-of means no predicate ever fired — v1 lists them all
/// without re-checking), not the per-predicate "not yet true:" lines.
#[test]
fn wait_until_any_of_timeout_diag_block() {
    let src = r#"
test WaitAnyTimeoutTest
    let dut : Top
    run
        wait until any of dut.ready == 1, dut.count_out == 2 timeout 50 cycles
    end run
end test WaitAnyTimeoutTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntilTimeout {
        mode, on_timeout, on_fire, ..
    } = &f.blocks[0].terminator
    else {
        panic!("expected WaitUntilTimeout terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::AnyOf);
    let diag = f.block(*on_timeout);
    assert_eq!(diag.stmts.len(), 2, "header + one none-of line:\n{f}");
    let ir::Stmt::FailDiag { guard: None, args } = &diag.stmts[0] else {
        panic!("first diag stmt is the unguarded header:\n{f}");
    };
    assert_eq!(args.fmt, "wait until any of timed out after %lld cycles");
    let ir::Stmt::FailDiag { guard: None, args } = &diag.stmts[1] else {
        panic!("second diag stmt is the unguarded none-of line:\n{f}");
    };
    assert_eq!(args.fmt, "  none of: dut.ready == 1, dut.count_out == 2");
    assert!(args.args.is_empty());
    assert!(
        matches!(diag.terminator, ir::Terminator::Jump(b) if b == *on_fire),
        "on_timeout rejoins on_fire:\n{f}"
    );
}

/// Untimed single-predicate `wait until` becomes a `WaitUntil`
/// terminator with the port read kept inline (re-sampled each cycle)
/// and the source text captured for diagnostics.
#[test]
fn wait_until_single_lowers_to_wait_until_terminator() {
    let src = r#"
test WaitSingleTest
    let dut : Top
    run
        wait until dut.ready == 1
        dut.en = 0
    end run
end test WaitSingleTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntil { preds, mode, succ } = &f.blocks[0].terminator else {
        panic!("expected WaitUntil terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::Single);
    assert_eq!(preds.len(), 1);
    assert_eq!(preds[0].src_text, "dut.ready == 1");
    assert!(
        matches!(&preds[0].expr, ir::Expr::Binary(ir::BinOp::Eq, l, _)
            if matches!(&**l, ir::Expr::Port(_))),
        "port stays inline in the wait predicate:\n{f}"
    );
    // Successor carries the post-wait statements.
    assert!(
        f.block(*succ)
            .stmts
            .iter()
            .any(|s| matches!(s, ir::Stmt::DutWrite(..))),
        "wait successor continues the body:\n{f}"
    );
}

/// `wait until ... timeout N cycles fail("...")` becomes a
/// `WaitUntilTimeout` whose `on_timeout` block carries the v1
/// diagnostic shape: unconditional FAIL header (the user's message),
/// one guarded "not yet true:" line per sub-predicate, then a rejoin
/// to the success path.
#[test]
fn wait_until_timeout_lowers_diag_block() {
    let src = r#"
test WaitTimeoutTest
    let dut : Top
    run
        wait until all of dut.count_out >= 12, dut.en == 1 timeout 100 cycles fail("quiesce conditions not met")
    end run
end test WaitTimeoutTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntilTimeout {
        preds,
        mode,
        cycles,
        on_fire,
        on_timeout,
    } = &f.blocks[0].terminator
    else {
        panic!("expected WaitUntilTimeout terminator:\n{f}");
    };
    assert_eq!(*mode, ir::WaitMode::AllOf);
    assert_eq!(preds.len(), 2);
    assert_eq!(preds[0].src_text, "dut.count_out >= 12");
    assert_eq!(preds[1].src_text, "dut.en == 1");
    // Budget is evaluated once into a local before the wait.
    assert!(
        matches!(cycles, ir::Expr::Local(_)),
        "budget stashed in a local:\n{f}"
    );

    let diag = f.block(*on_timeout);
    assert_eq!(diag.stmts.len(), 3, "header + 2 breakdown lines:\n{f}");
    let ir::Stmt::FailDiag { guard: None, args } = &diag.stmts[0] else {
        panic!("first diag stmt is the unguarded header:\n{f}");
    };
    assert_eq!(args.fmt, "quiesce conditions not met");
    let ir::Stmt::FailDiag {
        guard: Some(_),
        args,
    } = &diag.stmts[1]
    else {
        panic!("second diag stmt is a guarded breakdown line:\n{f}");
    };
    assert_eq!(args.fmt, "  not yet true: dut.count_out >= 12");
    let ir::Stmt::FailDiag {
        guard: Some(_),
        args,
    } = &diag.stmts[2]
    else {
        panic!("third diag stmt is a guarded breakdown line:\n{f}");
    };
    assert_eq!(args.fmt, "  not yet true: dut.en == 1");
    // Timeout arm rejoins the success path.
    assert!(
        matches!(diag.terminator, ir::Terminator::Jump(b) if b == *on_fire),
        "on_timeout rejoins on_fire:\n{f}"
    );
}

/// Default (message-less) timeout header mirrors v1's
/// "<label> timed out after %lld cycles" text, with the budget local
/// as the format argument.
#[test]
fn wait_until_timeout_default_header() {
    let src = r#"
test WaitDefaultHeaderTest
    let dut : Top
    run
        wait until dut.ready == 1 timeout 50 cycles
    end run
end test WaitDefaultHeaderTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitUntilTimeout { on_timeout, .. } = &f.blocks[0].terminator else {
        panic!("expected WaitUntilTimeout terminator:\n{f}");
    };
    let ir::Stmt::FailDiag { guard: None, args } = &f.block(*on_timeout).stmts[0] else {
        panic!("first diag stmt is the header:\n{f}");
    };
    assert_eq!(args.fmt, "wait until timed out after %lld cycles");
    assert_eq!(args.args.len(), 1);
    assert!(matches!(args.args[0].expr, ir::Expr::Local(_)));
}

/// Locks the dump-ir text for the wait-until fixture (terminator
/// shapes, PredSrc source text, timeout diagnostic blocks).
#[test]
fn wait_until_counter_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wait_until_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wait_until_counter_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the any-of fixture (`AnyOf` wait modes,
/// untimed + timed) and its emitted C++ (the `||`-joined awaiter
/// predicates).
#[test]
fn wait_any_of_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wait_any_of_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wait_any_of_dump_ir", format!("{prog}"));
}

#[test]
fn wait_any_of_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "wait_any_of_emitted_cpp",
        emit_fixture_cpp("wait_any_of_test.harc")
    );
}

/// Locks the dump-ir text for the deliberately-failing any-of timeout
/// fixture: both timeout diagnostic blocks (default header and user
/// `fail("…")` header) carry the single unguarded "none of:" line.
#[test]
fn wait_any_of_timeout_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wait_any_of_timeout_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wait_any_of_timeout_dump_ir", format!("{prog}"));
}

// ── lower_coroutine pass: CFG → tagged-FSM metadata. Snapshots lock
//    the `harc dump-ir --pass lower-coroutine` suffix (the metadata
//    section the pass appends after the regular IR dump, which the
//    *_dump_ir snapshots above already lock). ───────────────────────

/// top_counter: three wait-1-cycle loops + a trailing reset wait.
/// Locks resume-point state numbering and the collapsed loop /
/// loop-exit transitions with their branch-condition summaries.
#[test]
fn top_counter_lower_coroutine_snapshot() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let meta = lower_coroutine::run(&prog).expect("tags");
    insta::assert_snapshot!(
        "top_counter_lower_coroutine",
        format!("{}", meta.display(&prog))
    );
}

/// wait_until_counter: chained `WaitUntilTimeout`s. Locks the paired
/// fire/timeout edges and the timeout-handler states falling through
/// to the success path.
#[test]
fn wait_until_counter_lower_coroutine_snapshot() {
    let prog = lower_src(&fixture("wait_until_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let meta = lower_coroutine::run(&prog).expect("tags");
    insta::assert_snapshot!(
        "wait_until_counter_lower_coroutine",
        format!("{}", meta.display(&prog))
    );
}

/// The pass is a side-table: running it must not perturb the IR (the
/// `dump-ir` text is byte-identical before and after), and its own
/// rendering is byte-stable across runs.
#[test]
fn lower_coroutine_leaves_ir_untouched_and_is_deterministic() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let before = format!("{prog}");
    let meta_a = lower_coroutine::run(&prog).expect("tags");
    let meta_b = lower_coroutine::run(&prog).expect("tags");
    assert_eq!(format!("{prog}"), before, "pass must not mutate the IR");
    assert_eq!(
        format!("{}", meta_a.display(&prog)),
        format!("{}", meta_b.display(&prog)),
        "metadata rendering must be byte-stable across runs"
    );
}

/// tbir emission of the wait-until fixture carries the v1 runtime
/// calls: untimed/timed awaiters and the timeout diagnostic text.
#[test]
fn tbir_emit_wait_until_runtime_calls() {
    let merged = merged_src(&fixture("wait_until_counter_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "bool _wu_satisfied = co_await harc_rt::wait_until_timeout(_slot, \
         [&]{ return (harc_rt::harc_read(dut->count_out) == 8); }, (uint32_t)_wu_budget);",
        "[&]{ return ((harc_rt::harc_read(dut->count_out) >= 12)) && \
         ((harc_rt::harc_read(dut->en) == 1)); }",
        "sim_log_line(\"FAIL\", \"count never reached 8\");",
        "sim_log_line(\"FAIL\", \"  not yet true: dut.count_out >= 12\");",
    ] {
        assert!(cpp.contains(marker), "missing wait-until marker `{marker}`");
    }
}

/// `randomize(t)` now lowers to a `Terminator::Randomize` carrying a
/// `ConstraintRef` into the program's constraint-site table, and the
/// tbir backend splices in v1's Z3-solve snippet (the constraint-IR
/// seam — `docs/tbir-mvp.md` §"randomize"). A bare `randomize(t)` of a
/// keep-free transaction routes through the unconstrained-PRNG shell.
#[test]
fn randomize_lowers_to_terminator() {
    let src = r#"
transaction Req
    addr : uint<32>
    keep addr % 4 == 0
end transaction Req

test RandTest
    let dut : Top
    run
        let t : Req
        randomize(t)
    end run
end test RandTest
"#;
    let merged = merged_src(src);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // One constraint site, carrying the merged keep set and a problem id.
    assert_eq!(prog.constraint_sites.len(), 1, "one randomize site");
    let site = &prog.constraint_sites[0];
    assert_eq!(site.record, "Req");
    assert_eq!(site.constraints.len(), 1, "transaction keep merged in");
    assert!(site.problem_id.is_some(), "Z3-ready problem id");
    // The run function ends a block with a Randomize terminator.
    let run = prog.function(prog.tests[0].run);
    assert!(
        run.blocks
            .iter()
            .any(|b| matches!(b.terminator, ir::Terminator::Randomize { .. })),
        "a Randomize terminator is present"
    );
    // tbir emits the v1 Z3-solve block (constraint-IR seam reused).
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    assert!(cpp.contains("z3::solver"), "Z3 solve block emitted");
    assert!(
        cpp.contains("#include \"harc_z3_rt.h\""),
        "z3 runtime header included"
    );
    assert!(
        cpp.contains("trace.randomize("),
        "randomize trace event emitted"
    );
}

// ── Transaction value-records (non-randomize usage) ─────────────────

/// Locks the dump-ir text for the transaction fixture: the records
/// table (defaults, `!` fields, inert keep/attr text), `RecordInit`
/// inside the loop body, `RecordFieldWrite`, and field reads in
/// asserts / branch conditions / loop bounds / format args.
#[test]
fn transaction_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("transaction_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("transaction_basic_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the transaction fixture: the
/// value-record struct (member-initializer defaults, operator==/!=),
/// the record-typed hoisted local, and the let-site re-init.
#[test]
fn transaction_basic_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "transaction_basic_emitted_cpp",
        emit_fixture_cpp("transaction_basic_test.harc")
    );
}

/// Scoreboard data-only subset: the schema's scalar/queue fields, the
/// queue push (statement) / pop (let-RHS, then assign), scalar
/// read/write ops, and the size()/empty() value-queries in
/// assert/log positions, across run and check.
#[test]
fn scoreboard_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("scoreboard_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("scoreboard_basic_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the scoreboard fixture: the
/// scoreboard struct (scalar defaults + `harc_rt::HarcQueue<T>`
/// members), the `_tb`-held instance, and the push/pop/size/empty/
/// scalar accessors.
#[test]
fn scoreboard_basic_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "scoreboard_basic_emitted_cpp",
        emit_fixture_cpp("scoreboard_basic_test.harc")
    );
}

/// Env-composition subset: a `let env : AnalysisEnv` composing an
/// analysis-source transactor (`out event` + `emit`) and two
/// method-bearing scoreboards, wired by `connect`. Locks the dump-ir:
/// the component schemas (sub-component fields, method signatures, the
/// env's resolved connect edges), the self-relative method bodies
/// (`ComponentEmit` / `ComponentFieldWrite`), and the test-body
/// `ComponentCall` / `ComponentField` path access.
#[test]
fn analysis_env_connect_dump_ir_snapshot() {
    let prog = lower_src(&fixture("analysis_sink_connect_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("analysis_env_connect_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the env-composition fixture: the
/// component structs (event-callback vectors, by-value sub-components),
/// the `<Comp>_<method>` lambdas, and the env local + connect push_backs.
#[test]
fn analysis_env_connect_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "analysis_env_connect_emitted_cpp",
        emit_fixture_cpp("analysis_sink_connect_test.harc")
    );
}

/// A method-bearing scoreboard now lowers as a composite component
/// (per-instance state materialized) — but only when bound as a
/// test-scope `let env`-style component. Bound directly as a *testbench
/// field*, it is out of this slice and rejected precisely (composite
/// components bind as test-scope lets), never mis-lowered to a DUT
/// module type.
#[test]
fn scoreboard_method_testbench_field_is_rejected() {
    let src = r#"
scoreboard Sb
    n : uint<32> default 0
    hookable bump()
        n = n + 1
    end bump
end scoreboard Sb

testbench Tb
    dut : Top
    sb  : Sb
end testbench Tb

impl T for Tb
    run
        assert sb.n == 0 else fail("x")
    end run
end impl T
"#;
    let err = lower_src(src).expect_err("composite-component testbench field must be rejected");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("composite-component"), "unexpected error: {msg}");
}

/// A `queue<Struct>` element type is out of the scalar-only subset:
/// rejected at the field, not mis-lowered.
#[test]
fn scoreboard_struct_queue_is_rejected() {
    let src = r#"
struct Pkt
    a : uint<8>
end struct Pkt

scoreboard Sb
    q : queue<Pkt>
end scoreboard Sb

testbench Tb
    dut : Top
    sb  : Sb
end testbench Tb

impl T for Tb
    run
        assert sb.q.empty() else fail("x")
    end run
end impl T
"#;
    let err = lower_src(src).expect_err("queue<struct> must be rejected");
    assert!(
        format!("{err}").contains("scoreboard field"),
        "unexpected error: {err}"
    );
}

/// A record let inside a loop re-runs the defaults each iteration:
/// the lowering must place a `RecordInit` at the let site (loop
/// body), not rely on the hoisted declaration's one-time initializer.
#[test]
fn record_let_in_loop_reinitializes() {
    let src = r#"
transaction Req
    addr : uint<32> default 5
end transaction Req

test ReinitTest
    let dut : Top
    run
        for i in 0 .. 2
            let t : Req
            t.addr = t.addr + i
        end for
    end run
end test ReinitTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    // The loop body block carries RecordInit then RecordFieldWrite.
    let body = run
        .blocks
        .iter()
        .find(|b| {
            b.stmts
                .iter()
                .any(|s| matches!(s, ir::Stmt::RecordInit(..)))
        })
        .expect("a block carries RecordInit");
    let init_pos = body
        .stmts
        .iter()
        .position(|s| matches!(s, ir::Stmt::RecordInit(..)))
        .unwrap();
    let write_pos = body
        .stmts
        .iter()
        .position(|s| matches!(s, ir::Stmt::RecordFieldWrite { .. }))
        .expect("field write lowered");
    assert!(init_pos < write_pos, "init precedes the write:\n{run}");
    // The body block is a loop participant (reachable from the header
    // branch), so the init re-runs per iteration by construction.
    assert!(
        matches!(prog.records[0].fields[0].default, Some(5)),
        "default carried into the schema"
    );
}

/// A `struct` declaration lowers into the same records table as a
/// transaction — a `let s : S` default-constructs and `s.field`
/// reads/writes reuse the record machinery (no struct-specific IR).
#[test]
fn struct_lowers_as_value_record() {
    let src = r#"
struct Pkt
    flag  : bool    default true
    count : uint<16> default 7
    spare : uint<8>
end struct Pkt

test StructTest
    let dut : Top
    run
        let p : Pkt
        p.count = p.count + 1
        assert p.flag else fail("flag")
    end run
end test StructTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // One record schema, fields in declaration order with defaults.
    assert_eq!(prog.records.len(), 1, "struct → one record schema");
    let rec = &prog.records[0];
    assert_eq!(rec.name, "Pkt");
    assert_eq!(rec.fields.len(), 3, "fields not double-counted from body");
    assert_eq!(rec.fields[0].default, Some(1), "bool true → 1");
    assert_eq!(rec.fields[1].default, Some(7));
    assert_eq!(rec.fields[2].default, None, "undefaulted field re-zeroes");
    // The body carries RecordInit + a RecordFieldWrite for `p.count`.
    let run = prog.function(prog.tests[0].run);
    assert!(
        run.blocks
            .iter()
            .any(|b| b.stmts.iter().any(|s| matches!(s, ir::Stmt::RecordInit(..)))),
        "struct local default-constructs via RecordInit:\n{run}"
    );
}

/// A non-scalar struct field (here a `Vec`) is out of the scalar-only
/// subset: rejected at the field, never mis-lowered. (This is the
/// residual blocker for the `tlm_pairing_arch_burst_*` fixtures.)
#[test]
fn struct_non_scalar_field_is_rejected() {
    let src = r#"
struct Resp
    data : Vec<uint<32>, 4>
end struct Resp

test StructVecTest
    let dut : Top
    run
        let r : Resp
    end run
end test StructVecTest
"#;
    let err = lower_src(src).expect_err("Vec field must be rejected");
    let msg = assert_unsupported(&err);
    assert!(msg.contains("struct field"), "names the field: {msg}");
    assert!(msg.contains("non-scalar"), "names the reason: {msg}");
}

/// A `struct` and a `transaction` sharing a name would resolve
/// ambiguously through `record_ids`; reject the collision.
#[test]
fn struct_name_collides_with_transaction() {
    let src = r#"
transaction Dup
    a : uint<8>
end transaction Dup

struct Dup
    b : uint<8>
end struct Dup

test C
    let dut : Top
    run
        wait 1 cycle
    end run
end test C
"#;
    let err = lower_src(src).expect_err("name collision must be rejected");
    assert!(
        format!("{err}").contains("collides"),
        "names the collision: {err}"
    );
}

/// A DUT read in a record-field-write value hoists through a DutRead
/// temp — same no-inline-ports discipline as `Assign`.
#[test]
fn record_field_write_hoists_dut_reads() {
    let src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test HoistTest
    let dut : Top
    run
        let t : Req
        t.addr = dut.count_out
    end run
end test HoistTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    let b = &run.blocks[0];
    assert!(
        matches!(b.stmts[1], ir::Stmt::DutRead(..)),
        "port hoisted before the field write:\n{run}"
    );
    assert!(
        matches!(
            &b.stmts[2],
            ir::Stmt::RecordFieldWrite { value: ir::Expr::Local(_), .. }
        ),
        "field write consumes the hoisted temp:\n{run}"
    );
}

/// Unknown fields are hard lowering errors (v1 would defer to a C++
/// compile failure; the IR rejects at lowering) — both on writes and
/// on reads.
#[test]
fn record_unknown_field_is_invalid() {
    let src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test BadFieldTest
    let dut : Top
    run
        let t : Req
        t.nosuch = 1
    end run
end test BadFieldTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "hard error, not Unsupported: {err:?}"
    );
    assert!(
        err.to_string().contains("no field `nosuch`"),
        "names the field: {err}"
    );
}

/// Whole-record assignment: a same-typed record-local copy lowers
/// (C++ struct assignment in both backends); anything else is a
/// precise lowering rejection, not a verifier error or C++ compile
/// failure.
#[test]
fn record_whole_value_assignment_rules() {
    let copy_src = r#"
transaction Req
    addr : uint<32> default 3
end transaction Req

test CopyTest
    let dut : Top
    run
        let t : Req
        let u : Req
        t.addr = 9
        u = t
        assert u.addr == 9 else fail("copy lost addr=${u.addr}")
    end run
end test CopyTest
"#;
    let prog = lower_src(copy_src).expect("record-to-record copy lowers");
    verify::verify_program(&prog).expect("verifies");

    let bad_src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test BadCopyTest
    let dut : Top
    run
        let t : Req
        t = true
    end run
end test BadCopyTest
"#;
    let err = lower_src(bad_src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("non-`Req` value"),
        "names the record type: {msg}"
    );
}

/// `when` subtype blocks stay outside the lowered record shape.
#[test]
fn record_when_subtype_is_unsupported() {
    let src = r#"
transaction Req
    op : uint<2>
    when op == 1
        addr : uint<32>
    end when
end transaction Req

test WhenTest
    let dut : Top
    run
        wait 1 cycle
    end run
end test WhenTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("`when` subtype"), "names the construct: {msg}");
}

/// Record locals cannot live in *pure* helpers (they emit as
/// scalar-only file-scope C++ functions in the tbir backend). Note the
/// body must stay inside the pure scan subset to reach this gate — a
/// field access would classify the helper impure and CFG-inline it,
/// where record locals are legal.
#[test]
fn record_let_in_pure_helper_is_unsupported() {
    let src = r#"
transaction Req
    addr : uint<32> default 9
end transaction Req

function mk() -> uint<32>
    let t : Req
    return 1
end function mk

test PureHelperTest
    let dut : Top
    run
        let x = mk()
    end run
end test PureHelperTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("pure helper"),
        "names the helper context: {msg}"
    );
}

#[test]
fn helper_call_with_dut_access_is_unsupported() {
    let src = r#"
test HelperTest
    let dut : Top
    run
        poke(dut, 1)
    end run
end test HelperTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("poke"), "names the helper: {msg}");
    assert!(msg.contains("--codegen v1"), "suggests v1: {msg}");
}

// ── Helper functions: pure C++ calls vs CFG inlining ────────────────

/// Source with one impure helper (DUT access + `wait` + `return`),
/// called twice, plus one pure helper called from the run body.
const HELPER_MIX_SRC: &str = r#"
function read_addr(d: Top, addr: uint<3>) -> uint<8>
    d.rd_addr = addr
    d.rd_en = 1
    wait 1 cycle
    return d.rd_data
end function read_addr

function double_it(x: uint<8>) -> uint<8>
    return x + x
end function double_it

test HelperMixTest
    let dut : Top
    run
        assert read_addr(dut, 2) == 90 else fail("bad rom value")
        let d = double_it(read_addr(dut, 3))
        assert d == 236 else fail("bad doubled value")
    end run
end test HelperMixTest
"#;

/// Locks the inlined-CFG dump-ir text: the impure helper's body
/// (DutWrite / WaitCycles / DutRead-return) appears inline in the run
/// function with remapped blocks and deduplicated param locals, while
/// the pure helper stays a standalone `Helper` function invoked via
/// `Expr::Call`.
#[test]
fn helper_inline_dump_ir_snapshot() {
    let prog = lower_src(HELPER_MIX_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("helper_inline_dump_ir", format!("{prog}"));
}

/// Categorization: DUT/sync-touching helpers are inlined (no standalone
/// function); pure helpers lower once as `FunctionKind::Helper` and the
/// call site stays `Expr::Call(CallTarget::Helper, ...)`.
#[test]
fn helper_categorization_pure_vs_impure() {
    let prog = lower_src(HELPER_MIX_SRC).expect("lowers");
    let helper_fns: Vec<&ir::TbFunction> = prog
        .functions
        .iter()
        .filter(|f| f.kind == ir::FunctionKind::Helper)
        .collect();
    assert_eq!(helper_fns.len(), 1, "only the pure helper is standalone");
    assert_eq!(helper_fns[0].name, "double_it");
    assert!(helper_fns[0].ret.is_some(), "pure helper carries a ret slot");

    // The run body inlined read_addr (WaitCyclesSync from the helper
    // body — inlined waits take v1's synchronous lambda path) and
    // calls double_it by name.
    let run = prog.function(prog.tests[0].run);
    let waits = run
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, ir::Terminator::WaitCyclesSync(..)))
        .count();
    assert_eq!(waits, 2, "one inlined wait per read_addr call:\n{run}");
    let calls_double_it = run.blocks.iter().any(|b| {
        b.stmts.iter().any(|s| {
            matches!(s, ir::Stmt::Assign(_, e)
                if format!("{:?}", e).contains("Helper(\"double_it\")"))
        })
    });
    assert!(calls_double_it, "pure call survives as Expr::Call:\n{run}");
}

/// Param remapping: each inline site gets fresh locals for the helper's
/// params — two calls must not share the `addr` slot.
#[test]
fn helper_inline_param_remapping() {
    let prog = lower_src(HELPER_MIX_SRC).expect("lowers");
    let run = prog.function(prog.tests[0].run);
    let addr_locals: Vec<&str> = run
        .locals
        .iter()
        .map(|l| l.name.as_str())
        .filter(|n| n.starts_with("addr"))
        .collect();
    assert_eq!(
        addr_locals,
        vec!["addr", "addr_2"],
        "each inline site declares its own param local:\n{run}"
    );
}

/// Direct recursion is rejected up front with the cycle path.
#[test]
fn helper_direct_recursion_is_unsupported() {
    let src = r#"
function spin(d: Top) -> uint<8>
    return spin(d)
end function spin

test RecTest
    let dut : Top
    run
        let x = spin(dut)
    end run
end test RecTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("recursive helper functions"), "{msg}");
    assert!(msg.contains("spin -> spin"), "names the cycle: {msg}");
}

/// Mutual recursion is rejected even when the helpers are never called
/// (the call-graph DFS runs before any body lowers).
#[test]
fn helper_mutual_recursion_is_unsupported() {
    let src = r#"
function ping(x: uint<8>) -> uint<8>
    return pong(x)
end function ping

function pong(x: uint<8>) -> uint<8>
    return ping(x)
end function pong

test MutRecTest
    let dut : Top
    run
        dut.en = 1
    end run
end test MutRecTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("recursive helper functions"), "{msg}");
    assert!(
        msg.contains("ping -> pong -> ping") || msg.contains("pong -> ping -> pong"),
        "names the cycle: {msg}"
    );
}

/// An impure helper call inside a `${...}` message capture cannot be
/// inlined (messages evaluate lazily at the failure site).
#[test]
fn helper_impure_call_in_message_is_unsupported() {
    let src = r#"
function peek(d: Top) -> uint<8>
    wait 1 cycle
    return d.rd_data
end function peek

test FmtTest
    let dut : Top
    run
        assert dut.rd_data == 0 else fail("got ${peek(dut)}")
    end run
end test FmtTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("peek"), "names the helper: {msg}");
    assert!(msg.contains("--codegen v1"), "suggests v1: {msg}");
}

/// `break` inside an inlined helper body must not bind to a loop open
/// at the call site — helpers are free functions.
#[test]
fn helper_inline_break_cannot_bind_caller_loop() {
    let src = r#"
function bail(d: Top)
    d.en = 0
    break
end function bail

test BreakTest
    let dut : Top
    run
        for i in 0 .. 4
            bail(dut)
        end for
    end run
end test BreakTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "break outside a loop is a structural error: {err:?}"
    );
    assert!(err.to_string().contains("break"), "{err}");
}

/// tbir emission for the helper mix: the pure helper becomes a
/// file-scope C++ function; the impure helper's wait shows up as a
/// synchronous tick loop in the run coroutine (CFG-inlined, not a
/// call — and sync, not co_await, mirroring v1's lambda-body waits).
#[test]
fn tbir_emit_helper_mix() {
    let merged = merged_src(HELPER_MIX_SRC);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "static uint64_t harc_helper_double_it(uint64_t x);",
        "static uint64_t harc_helper_double_it(uint64_t x) {",
        "harc_helper_double_it(__t",
        "for (int _w = 0; _w < 1; _w++) tick();",
    ] {
        assert!(cpp.contains(marker), "missing marker `{marker}` in:\n{cpp}");
    }
    assert!(
        !cpp.contains("read_addr"),
        "impure helper must be fully inlined, not emitted as a function"
    );
}

// ── `wait N cycles on <clock>` ──────────────────────────────────────

const WAIT_ON_CLOCK_SRC: &str = r#"
test WaitOnClockTest
    let dut : Top
    clock clk = 10ns
    clock aux_clk = 4ns
    run
        wait 2 cycles on aux_clk
        dut.en = 1
    end run
end test WaitOnClockTest
"#;

/// A clock-qualified wait lowers to `WaitCycles` carrying the resolved
/// `WaitClock` (declaration-order index == runtime scheduler index);
/// the dump-ir text names the clock; the lower_coroutine trigger
/// renders it too.
#[test]
fn wait_on_clock_lowers_with_clock_qualifier() {
    let prog = lower_src(WAIT_ON_CLOCK_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let ir::Terminator::WaitCycles(_, Some(clock), _) = &f.blocks[0].terminator else {
        panic!("expected clock-qualified WaitCycles terminator:\n{f}");
    };
    assert_eq!(clock.name, "aux_clk");
    assert_eq!(clock.index, 1, "declaration-order index into TestSchema::clocks");
    assert!(
        format!("{f}").contains("WaitCycles(2 on aux_clk, b1)"),
        "display names the clock:\n{f}"
    );
    let meta = lower_coroutine::run(&prog).expect("tags");
    assert!(
        format!("{}", meta.display(&prog)).contains("wait_cycles(2 on aux_clk)"),
        "pass trigger names the clock:\n{}",
        meta.display(&prog)
    );
}

/// An unknown clock after `on` is a structured lowering error naming
/// the clock and the declared ones (v1 deferred this to emission).
#[test]
fn wait_on_unknown_clock_is_invalid() {
    let src = r#"
test WaitBadClockTest
    let dut : Top
    clock clk = 10ns
    clock aux_clk = 4ns
    run
        wait 1 cycle on nope
    end run
end test WaitBadClockTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "unknown clock is Invalid, not Unsupported: {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("no clock named `nope`"), "names the clock: {msg}");
    assert!(
        msg.contains("declared clocks: clk, aux_clk"),
        "lists the declared clocks: {msg}"
    );
}

/// The verifier cross-checks every clock-qualified wait against the
/// test's declared clocks: an out-of-range index (which codegen would
/// turn into an out-of-bounds `clocks_[i]` access) or an index/name
/// disagreement is a programmer-error verify failure.
#[test]
fn verifier_catches_bad_wait_clock() {
    let prog = lower_src(WAIT_ON_CLOCK_SRC).expect("lowers");
    let run_idx = prog.tests[0].run.index();

    let mut broken = prog.clone();
    for b in &mut broken.functions[run_idx].blocks {
        if let ir::Terminator::WaitCycles(_, Some(wc), _) = &mut b.terminator {
            wc.index = 7; // out of range — only 2 clocks declared
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| e.to_string().contains("only 2 clock(s) are declared")),
        "{errs:?}"
    );

    let mut broken = prog;
    for b in &mut broken.functions[run_idx].blocks {
        if let ir::Terminator::WaitCycles(_, Some(wc), _) = &mut b.terminator {
            wc.index = 0; // valid slot, but it is `clk`, not `aux_clk`
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| e.to_string().contains("that slot is `clk`")),
        "{errs:?}"
    );
}

/// tbir emission of a clock-qualified wait mirrors v1's inline
/// eval_clocks_until loop (no coroutine yield): advance to whichever
/// clock's next edge is sooner until the named clock has seen N more
/// rising edges, then run the checkers.
#[test]
fn tbir_emit_wait_on_clock_inline_loop() {
    let merged = merged_src(WAIT_ON_CLOCK_SRC);
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "{ long long _target = clocks_[1].rising_count + (long long)(2); \
         while (clocks_[1].rising_count < _target) {",
        "long long _next = clocks_[0].next_edge_ps;",
        "for (auto& _ck : clocks_) if (_ck.next_edge_ps < _next) _next = _ck.next_edge_ps;",
        "eval_clocks_until(_next);",
        "} for (auto& _c : _checkers) _c(); }",
    ] {
        assert!(cpp.contains(marker), "missing wait-on-clock marker `{marker}` in:\n{cpp}");
    }
    assert!(
        !cpp.contains("co_await harc_rt::wait_cycles"),
        "clock-qualified wait must not yield to the scheduler (v1 parity)"
    );
}

/// Core lowering shape: a `for` loop becomes init / header-branch /
/// body / latch / exit, with the counter init outside the loop.
#[test]
fn for_loop_lowers_to_header_latch_exit() {
    let src = r#"
test LoopTest
    let dut : Top
    run
        for i in 0 .. 4
            dut.en = 1
        end for
    end run
end test LoopTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    // init block jumps to a header that branches on `i < 4`.
    let n_branches = f
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, ir::Terminator::Branch(..)))
        .count();
    assert_eq!(n_branches, 1, "one loop header:\n{f}");
    let has_latch = f.blocks.iter().any(|b| {
        matches!(b.terminator, ir::Terminator::Jump(_))
            && b.stmts
                .iter()
                .any(|s| matches!(s, ir::Stmt::Assign(_, ir::Expr::Binary(ir::BinOp::Add, ..))))
    });
    assert!(has_latch, "latch block increments the counter:\n{f}");
}

/// DUT reads hoist into `DutRead` temps everywhere except the allowed
/// port positions (assert conds, format args, DutWrite values).
#[test]
fn dut_read_in_let_hoists_to_dut_read_stmt() {
    let src = r#"
test HoistTest
    let dut : Top
    run
        let doubled = dut.count_out + dut.count_out
    end run
end test HoistTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let f = prog.function(prog.tests[0].run);
    let reads = f.blocks[0]
        .stmts
        .iter()
        .filter(|s| matches!(s, ir::Stmt::DutRead(..)))
        .count();
    assert_eq!(reads, 2, "both port reads hoisted:\n{f}");
}

/// The verifier rejects programs with dangling successors and
/// use-before-def locals (programmer-error net under IR mutation).
#[test]
fn verifier_catches_bad_successor_and_use_before_def() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let mut broken = prog.clone();
    // Dangling successor.
    let f = &mut broken.functions[0];
    if let Some(b) = f.blocks.first_mut() {
        b.terminator = ir::Terminator::Jump(ir::BlockId(9999));
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::BadSuccessor { .. })),
        "{errs:?}"
    );

    // Use-before-def: read a fresh local that nothing assigns.
    let mut broken = prog.clone();
    let f = &mut broken.functions[0];
    let ghost = ir::LocalId(f.locals.len() as u32);
    f.locals.push(ir::TypedLocal {
        name: "ghost".to_string(),
        ty: ir::IrType::Unknown,
    });
    f.blocks[0].stmts.insert(
        0,
        ir::Stmt::DutWrite(
            ir::PortRef {
                testbench_field: "dut".to_string(),
                port_path: vec!["en".to_string()],
                direction: None,
                width: None,
                access: ir::PortAccess::Port,
                lane: None,
            },
            ir::Expr::Local(ghost),
        ),
    );
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::LocalUseBeforeDef { .. })),
        "{errs:?}"
    );
}

/// The port-position rule: an `Expr::Port` inside an Assign value is a
/// verify error (lowering must hoist it).
#[test]
fn verifier_rejects_port_in_assign_value() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let mut broken = prog;
    let f = &mut broken.functions[0];
    let l = ir::LocalId(0);
    f.blocks[0].stmts.push(ir::Stmt::Assign(
        l,
        ir::Expr::Port(ir::PortRef {
            testbench_field: "dut".to_string(),
            port_path: vec!["count_out".to_string()],
            direction: None,
            width: None,
            access: ir::PortAccess::Port,
            lane: None,
        }),
    ));
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::PortInDisallowedPosition { .. })),
        "{errs:?}"
    );
}

/// tbir emission carries the v1 scaffolding contract markers: context
/// struct, seed log, coroutine slot, loop-switch, dispatcher main.
#[test]
fn tbir_emit_scaffolding_contract() {
    let merged = merged_src(&fixture("top_counter_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "#include \"VTop.h\"",
        "struct HarcTestContext {",
        "struct TopCounterTb {",
        "sim_log_line(\"INFO\", \"seed=%llu\", (long long)harc_rng.state);",
        "harc_rt::trace::harc_start_trace(trace, harc_rng.state, \"Top\", \"TopCounterTest\", cycle_count);",
        "harc_rt::ThreadSlot _run_slot;",
        "co_await harc_rt::wait_cycles(_slot, (uint32_t)(3));",
        "int __bb = 0;",
        "while (!__done) {",
        "_tb.dut = dut;",
        "clocks_.push_back(ClockState{\"clk\", 5000, 5000, 0, 0});",
        "return harc_rt::log::harc_finish_sim_run(log_ctx, trace, cycle_count, errors);",
        "int main(int argc, char** argv) {",
        "if (std::strcmp(test_sel, \"TopCounterTest\") == 0) return run_TopCounterTest(argc, argv);",
    ] {
        assert!(cpp.contains(marker), "missing scaffolding marker `{marker}`");
    }
}

/// The harness's divergence detector (`harc trace-diff` wraps this):
/// one changed log line between two otherwise identical JSONL traces
/// must surface as a divergence.
#[test]
fn trace_diff_flags_single_log_line_change() {
    let a = r#"{"type":"meta","seq":0,"dut_backend":"verilator","top":"Top","test":"T","seed":1}
{"type":"log","seq":1,"cycle":3,"severity":"INFO","message":"PASS: counter counts"}
{"type":"log","seq":2,"cycle":7,"severity":"INFO","message":"PASS: counter holds"}
{"type":"sim_end","seq":3,"cycle":9,"errors":0}
"#;
    let b = a.replace("PASS: counter holds", "FAIL: counter wedged");
    let divs = harc::check_backends::diff_trace_strings(a, &b).expect("diff runs");
    assert_eq!(divs.len(), 1, "exactly the changed line diverges: {divs:?}");
    assert_eq!(divs[0].event_type, "log");
    assert_eq!(divs[0].cycle, Some(7));
    assert!(divs[0].arch_line.contains("counter holds"), "{divs:?}");
    assert!(divs[0].sv_line.contains("counter wedged"), "{divs:?}");
}

/// Backend-implementation noise (`seq` numbering) must NOT count as
/// divergence: traces identical modulo `seq` compare clean.
#[test]
fn trace_diff_ignores_seq_field() {
    let a = r#"{"type":"log","seq":1,"cycle":3,"severity":"INFO","message":"PASS: counter counts"}
{"type":"sim_end","seq":2,"cycle":9,"errors":0}
"#;
    let b = r#"{"type":"log","seq":41,"cycle":3,"severity":"INFO","message":"PASS: counter counts"}
{"type":"sim_end","seq":42,"cycle":9,"errors":0}
"#;
    let divs = harc::check_backends::diff_trace_strings(a, b).expect("diff runs");
    assert!(divs.is_empty(), "seq-only differences are noise: {divs:?}");
}

/// Locks the dump-ir text for the covergroup fixture: covgroup schema
/// (points, bins, trigger), the testbench cov field, the synthesized
/// SamplerAuto function, and check-phase CovReport / CovBin reads.
#[test]
fn sync_fifo_dump_ir_snapshot() {
    let prog = lower_src(&fixture("sync_fifo_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("sync_fifo_dump_ir", format!("{prog}"));
}

/// Bin-spec lowering: set literals and bare integer literals flatten
/// into the schema's finite value sets, in declaration order.
#[test]
fn covergroup_bin_specs_lower_to_value_sets() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            idle = {0}
            busy = {1, 2, 3}
            hexy = {0x10, 0b101}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        dut.en = 1
        wait 1 cycle
    end run
    check
        cov.report()
        assert cov.cp_mode.idle > 0 else fail("idle hole")
    end check
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    assert_eq!(prog.covgroups.len(), 1);
    let cg = &prog.covgroups[0];
    assert_eq!(cg.name, "Cov");
    assert_eq!(cg.points.len(), 1);
    let p = &cg.points[0];
    assert_eq!(p.name, "cp_mode");
    assert_eq!(p.target.port_path, vec!["mode".to_string()]);
    use ir::CovBinValue::Eq;
    let bins: Vec<(&str, &[ir::CovBinValue])> = p
        .bins
        .iter()
        .map(|b| (b.name.as_str(), b.values.as_slice()))
        .collect();
    assert_eq!(
        bins,
        vec![
            ("idle", &[Eq(0)][..]),
            ("busy", &[Eq(1), Eq(2), Eq(3)][..]),
            ("hexy", &[Eq(0x10), Eq(0b101)][..]),
        ]
    );
    // Testbench schema records the cov field; lowering synthesized one
    // SamplerAuto bound to the same covgroup.
    let tb = prog.testbench(prog.tests[0].testbench);
    assert_eq!(tb.cov_fields, vec![("cov".to_string(), ir::CovgroupId(0))]);
    let samplers: Vec<_> = prog
        .functions
        .iter()
        .filter(|f| matches!(f.kind, ir::FunctionKind::SamplerAuto { .. }))
        .collect();
    assert_eq!(samplers.len(), 1);
}

/// Range bin specs lower to inclusive `CovBinValue::Range` entries:
/// closed (`[a..b]`), open-low (`[..b]`), and the set-of-ranges mix
/// (`{[1..3], 7}`). (Open-high `[a..]` does not parse — the `..` infix
/// requires a right operand; only the bracket-prefix `[..b]`/`[..]`
/// forms produce open bounds.) Bounds match v1's hit test
/// (`_v >= lo && _v <= hi` — inclusive on both ends).
#[test]
fn covergroup_range_bins_lower() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            closed   = [4..9]
            openlow  = [..3]
            mixed    = {[1..3], 7}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    use ir::CovBinValue::{Eq, Range};
    let bins: Vec<(&str, &[ir::CovBinValue])> = prog.covgroups[0].points[0]
        .bins
        .iter()
        .map(|b| (b.name.as_str(), b.values.as_slice()))
        .collect();
    assert_eq!(
        bins,
        vec![
            ("closed", &[Range { lo: Some(4), hi: Some(9) }][..]),
            ("openlow", &[Range { lo: None, hi: Some(3) }][..]),
            ("mixed", &[Range { lo: Some(1), hi: Some(3) }, Eq(7)][..]),
        ]
    );
}

/// A range bound that is not a plain integer literal stays rejected
/// (reject, never silently mis-lower).
#[test]
fn covergroup_non_literal_range_bound_unsupported() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            bad = [dut.lo..9]
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("range bound"), "names the bound: {msg}");
}

/// The axilite_cov fixture (range bins + declared cross + randomize)
/// previously tripped on its range bins; those now lower, so the
/// rejection shifted to the first construct still out of subset — the
/// cross-file `axil_write(...)` helper call (the fixture's helper and
/// `RegData` transaction live in axilite_regs_test.harc). When helpers
/// across registries land, this shifts again to `randomize`.
#[test]
fn axilite_cov_fixture_still_unsupported() {
    let err = lower_src(&fixture("axilite_cov_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("axil_write"), "names the helper call: {msg}");
}

/// Declared `cross` items lower into `CovgroupSchema::crosses`,
/// resolving point names to indices and keeping the item position
/// (v1's storage-name discriminator).
#[test]
fn covergroup_cross_lowers_to_schema() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_a : cover dut.a
        bins
            hi = {1}
        end bins
    cp_b : cover dut.b
        bins
            hi = {1}
            lo = {0}
        end bins
    cross cp_a, cp_b
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cg = &prog.covgroups[0];
    assert_eq!(cg.crosses.len(), 1);
    // Item index 2: the cross follows the two point items.
    assert_eq!(cg.crosses[0].item_index, 2);
    assert_eq!(cg.crosses[0].point_indices, vec![0, 1]);
}

/// A cross naming an unknown coverpoint is a hard lowering error
/// (v1 pushes the same complaint into its emission error list).
#[test]
fn covergroup_cross_unknown_point_is_invalid() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_a : cover dut.a
        bins
            hi = {1}
        end bins
    cross cp_a, cp_nope
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
end impl CovTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "unknown cross point is Invalid, not Unsupported: {err:?}"
    );
    assert!(err.to_string().contains("cp_nope"), "{err}");
}

/// A check-phase read of an unknown point or bin is a hard lowering
/// error (v1 deferred this to a C++ compile failure).
#[test]
fn covergroup_unknown_bin_read_is_invalid() {
    let src = r#"
covergroup Cov @(posedge dut.clk)
    cp_mode : cover dut.mode
        bins
            idle = {0}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    cov : Cov
end testbench Tb

impl CovTest for Tb
    run
        wait 1 cycle
    end run
    check
        assert cov.cp_mode.nope > 0 else fail("hole")
    end check
end impl CovTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "unknown bin is Invalid, not Unsupported: {err:?}"
    );
    assert!(msg.contains("nope"), "names the bin: {msg}");
}

/// tbir emission carries the covergroup contract markers: the struct
/// with bin counters and auto-cross matrix, report() print calls, the
/// `_checkers` sampler registration, and the check-phase report/bin
/// reads — all shapes that must match v1's runtime-observable output.
#[test]
fn tbir_emit_covergroup_contract() {
    let merged = merged_src(&fixture("sync_fifo_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "struct FifoCov {",
        "uint64_t yes = 0;",
        "} cp_empty;",
        "uint64_t _auto_cross_cp_empty__cp_full[2][2] = {};",
        "harc_rt::log::harc_print_covergroup_summary(\"FifoCov\", _hit, _total);",
        "harc_rt::log::harc_print_covergroup_bin(\"cp_full\", \"no\", cp_full.no);",
        "harc_rt::log::harc_print_covergroup_cross_summary(\"FifoCov\", \"auto_cross\", \"cp_empty x cp_full\", _cross_hit, 4);",
        "FifoCov cov;",
        "_checkers.push_back([&]() {",
        "uint64_t _v = (uint64_t)(harc_rt::harc_read(dut->empty));",
        "if (((_v == 1))) { _tb.cov.cp_empty.yes++; _cg_hit_cp_empty[0] = true; }",
        "if (_cg_hit_cp_empty[_i] && _cg_hit_cp_full[_j]) _tb.cov._auto_cross_cp_empty__cp_full[_i][_j]++;",
        "_tb.cov.report();",
        "if (!((_tb.cov.cp_empty.yes > 0))) {",
    ] {
        assert!(cpp.contains(marker), "missing covergroup marker `{marker}`");
    }
}

/// Locks the dump-ir text for the declared-cross + range-bin fixture
/// (schema crosses line, range bin rendering) and its emitted C++
/// (flat `_cross_*` storage, range hit tests, "cross" report blocks).
#[test]
fn cov_cross_bins_dump_ir_snapshot() {
    let prog = lower_src(&fixture("cov_cross_bins_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("cov_cross_bins_dump_ir", format!("{prog}"));
}

#[test]
fn cov_cross_bins_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "cov_cross_bins_emitted_cpp",
        emit_fixture_cpp("cov_cross_bins_test.harc")
    );
}

/// tbir emission carries the declared-cross contract markers mirrored
/// from v1's `emit_covergroup_struct` / sample path: flat storage named
/// `_cross_<item_idx>_<p1>__<p2>`, the inclusive range hit test, the
/// "cross" (not "auto_cross") report summary, the suppressed auto-cross
/// for the declared pair, and the row-major sample update.
#[test]
fn tbir_emit_declared_cross_contract() {
    let merged = merged_src(&fixture("cov_cross_bins_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "uint64_t _cross_2_cp_count__cp_en[6] = {};",
        "if (((_v >= 0 && _v <= 3))) { _tb.cov.cp_count.low++; _cg_hit_cp_count[0] = true; }",
        "if (((_v >= 10 && _v <= 14) || (_v == 15))) { _tb.cov.cp_count.high++; _cg_hit_cp_count[2] = true; }",
        "harc_rt::log::harc_print_covergroup_cross_summary(\"CountCov\", \"cross\", \"cp_count x cp_en\", _cross_hit, 6);",
        "harc_rt::log::harc_print_covergroup_missing_bin(\"cp_count.low x cp_en.en0\")",
        "harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, 16, \"cross\");",
        "if (_cg_hit_cp_count[_i0] && _cg_hit_cp_en[_i1]) {",
        "_tb.cov._cross_2_cp_count__cp_en[(_i0 * 2 + _i1)]++;",
    ] {
        assert!(cpp.contains(marker), "missing declared-cross marker `{marker}`");
    }
    // The declared pair suppresses its auto-cross.
    assert!(
        !cpp.contains("_auto_cross_cp_count__cp_en"),
        "declared cp_count x cp_en cross must suppress the auto-cross"
    );
}

/// `--mt` is rejected by the tbir emitter (also rejected upstream by
/// the CLI; this locks the library-level contract).
#[test]
fn tbir_emit_rejects_mt() {
    let merged = merged_src(&fixture("top_counter_test.harc"));
    let prog = lower::lower_program(&merged).expect("lowers");
    let opts = cpp_tb::EmitOpts {
        mt: true,
        ..Default::default()
    };
    let err = tbir::emit(&prog, &merged, &opts).unwrap_err();
    assert!(err.0.contains("--mt"), "{}", err.0);
}

// ── placement pass snapshots — tier/timing annotation per block plus
//    the capability-diagnostic surface under both built-in profiles. ─

/// top_counter under the default single-site profile: pin-driving
/// blocks anchored by WaitCycles classify cycle-exact / Tier 0; pure
/// logging blocks land in Tier 2. Diagnostics must be `none` — the
/// single-site profile can never diagnose (design-doc guarantee).
#[test]
fn top_counter_placement_snapshot() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let profile = placement::TargetProfile::single_site();
    let table = placement::run(&prog, &profile);
    assert!(table.diagnostics.is_empty(), "single-site never diagnoses");
    insta::assert_snapshot!(
        "top_counter_placement",
        format!("{}", table.display(&prog, &profile))
    );
}

/// wait_until_counter under split-strict: wait-until regions are
/// timing-tolerant over architectural ports, so even the constrained
/// profile must place them diagnostic-free.
#[test]
fn wait_until_counter_placement_split_strict_snapshot() {
    let prog = lower_src(&fixture("wait_until_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let profile = placement::TargetProfile::split_strict();
    let table = placement::run(&prog, &profile);
    insta::assert_snapshot!(
        "wait_until_counter_placement_split_strict",
        format!("{}", table.display(&prog, &profile))
    );
}

/// The pass is a side-table: running it must not perturb the IR, and
/// its rendering is byte-stable across runs.
#[test]
fn placement_leaves_ir_untouched_and_is_deterministic() {
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let before = format!("{prog}");
    let profile = placement::TargetProfile::single_site();
    let a = format!("{}", placement::run(&prog, &profile).display(&prog, &profile));
    let b = format!("{}", placement::run(&prog, &profile).display(&prog, &profile));
    assert_eq!(a, b, "rendering must be byte-stable");
    assert_eq!(before, format!("{prog}"), "pass must not perturb the IR");
}

// ── Bus construct: bindings, protocol-typed signal access, channel
//    handshakes, and TLM method-call edges ───────────────────────────

/// Locks the dump-ir text for the Scope-A bus fixture: an inline bus
/// declaration, a `bind dut` binding on the testbench schema, and
/// two-level `<bind>.<ch>.<sig>` accesses lowering to flat-path
/// DutRead/DutWrite (`dut.axil.aw.valid` → `axil_aw_valid`).
#[test]
fn axilite_bus_dump_ir_snapshot() {
    let prog = lower_src(&fixture("axilite_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("axilite_bus_dump_ir", format!("{prog}"));
}

/// Locks the dump-ir text for the blocking TLM fixture: the
/// `TransactorMethod` call edges survive lowering UNINLINED — each
/// `mem.read`/`mem.poke` is `Assign(dest, mem.<method>(args))`, and
/// the binding's method schemas ride the testbench line.
#[test]
fn tlm_blocking_bus_dump_ir_snapshot() {
    let prog = lower_src(&fixture("tlm_method_blocking_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let text = format!("{prog}");
    assert!(
        text.contains("mem.read(5)") && text.contains("mem.poke(8, 3405691582)"),
        "call edges must stay visible (never inlined) in the IR:\n{text}"
    );
    insta::assert_snapshot!("tlm_blocking_bus_dump_ir", text);
}

/// Locks the emitted C++ for the blocking TLM fixture: the call edge
/// expands to v1's req/rsp wire protocol (arg wires, valid/ready
/// budget loops, rsp_data capture, tlm_call trace events).
#[test]
fn tlm_blocking_bus_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "tlm_blocking_bus_emitted_cpp",
        emit_fixture_cpp("tlm_method_blocking_bus_test.harc")
    );
}

const SEND_RECV_SRC: &str = r#"
bus PingBus
    handshake_channel tx: send kind: valid_ready
        data: uint<32>
    end handshake_channel tx

    handshake_channel rx: receive kind: valid_ready
        data: uint<32>
        resp: uint<2>
    end handshake_channel rx
end bus PingBus

testbench PingTb
    dut : PingDut
end testbench PingTb

impl PingTest for PingTb
    let p : PingBus = bind dut

    run
        p.tx.send(7)
        let v = p.rx.recv()
        assert v == 7 else fail("got ${v}")
    end run
end impl PingTest
"#;

/// `bus.<ch>.send/recv` CFG-inline to v1's auto-handshake: drive
/// payload + valid (send) / ready (recv), 16-cycle budget loop on the
/// opposite signal, capture-before-tick (recv), trailing tick, drop.
/// The recv capture reads the FIRST payload signal (documented
/// divergence from v1's payload struct — equivalent for everything
/// the IR can express).
#[test]
fn bus_send_recv_dump_ir_snapshot() {
    let prog = lower_src(SEND_RECV_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let text = format!("{prog}");
    assert!(
        text.contains("DutRead(%v, dut.p.rx.data)"),
        "recv must capture the first payload signal before the tick:\n{text}"
    );
    insta::assert_snapshot!("bus_send_recv_dump_ir", text);
}

/// fork/join_all TLM issue is outside the subset — precise rejection
/// (blocks tlm_method_bus_test + tlm_pairing_arch_target_test).
#[test]
fn bus_fork_join_is_unsupported() {
    let err = lower_src(&fixture("tlm_method_bus_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("`fork` bus-method calls"), "{msg}");
}

/// A direct (non-fork) call of an `out_of_order` method is rejected by
/// mode, naming the mode and the call site.
#[test]
fn bus_ooo_direct_call_is_unsupported() {
    let src = r#"
bus OooBus
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus OooBus

testbench OooTb
    dut : TlmMemory
end testbench OooTb

impl OooTest for OooTb
    let mem : OooBus = bind dut
    run
        let x = mem.read_ooo(9)
    end run
end impl OooTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("`out_of_order` tlm_method calls") && msg.contains("mem.read_ooo"),
        "{msg}"
    );
}

// ── Transactor declarations + method call edges ─────────────────────

/// Shared inline fixture: an unbound DUT-poking transactor with a
/// void method and a value-returning method, instantiated `active`
/// on the testbench, bound and called from `run`.
const XACTOR_SRC: &str = r#"
transactor Xt
    dut : Top

    when active
        hookable pulse(n: uint<8>)
            dut.en = 1
            wait 1 cycle
            dut.en = 0
        end pulse

        hookable readv() -> uint<32>
            wait 1 cycle
            return dut.count_out
        end readv
    end when
end transactor Xt

testbench XtTb
    dut : Top
    xt  : Xt active
end testbench XtTb

impl XtTest for XtTb
    run
        xt.dut = dut
        xt.pulse(3)
        let v = xt.readv()
        assert v == 0 else fail("v=${v}")
    end run
end impl XtTest
"#;

/// The structural contract: one schema per transactor, one
/// `TbFunction` (kind `TransactorBody`) per method with mirrored
/// params and a `ret` slot for `-> T` methods; calls lower to
/// `Stmt::TransactorCall` (statement form `dest: None`, let form
/// `dest: Some`), with the call edge never inlined; the
/// `xt.dut = dut` bind is validated and erased.
#[test]
fn transactor_methods_lower_to_functions_and_call_edges() {
    let prog = lower_src(XACTOR_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");

    assert_eq!(prog.transactors.len(), 1);
    let x = &prog.transactors[0];
    assert_eq!((x.name.as_str(), x.dut_field.as_str(), x.dut_type.as_str()), ("Xt", "dut", "Top"));
    assert_eq!(x.methods.len(), 2);
    let pulse = x.method("pulse").expect("pulse");
    let readv = x.method("readv").expect("readv");
    assert_eq!((pulse.n_params, pulse.has_ret), (1, false));
    assert_eq!((readv.n_params, readv.has_ret), (0, true));

    let pf = prog.function(pulse.function);
    assert_eq!(
        pf.kind,
        ir::FunctionKind::TransactorBody { transactor: ir::TransactorId(0) }
    );
    assert_eq!(pf.params.len(), 1);
    assert_eq!(pf.locals[0].name, "n");
    assert!(pf.ret.is_none());
    // The body suspends (wait 1 cycle) and drives the DUT.
    assert!(
        pf.blocks
            .iter()
            .any(|b| matches!(b.terminator, ir::Terminator::WaitCycles(..))),
        "pulse body keeps its wait:\n{pf}"
    );
    let rf = prog.function(readv.function);
    assert!(rf.ret.is_some(), "-> T method carries a ret slot");

    // The testbench schema records the instance field.
    let tb = prog.testbench(prog.tests[0].testbench);
    assert_eq!(tb.transactor_fields, vec![("xt".to_string(), ir::TransactorId(0))]);

    // Run body: the bind is erased; the calls are TransactorCall stmts.
    let run = prog.function(prog.tests[0].run);
    let calls: Vec<&ir::Stmt> = run
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter(|s| matches!(s, ir::Stmt::TransactorCall { .. }))
        .collect();
    assert_eq!(calls.len(), 2, "two call edges:\n{run}");
    let ir::Stmt::TransactorCall { dest: d0, call: c0 } = calls[0] else { unreachable!() };
    assert!(d0.is_none(), "statement call discards");
    let ir::Expr::Call(ir::CallTarget::TransactorMethod { bus_field, method }, args) = c0 else {
        panic!("call edge payload: {c0:?}");
    };
    assert_eq!((bus_field.as_str(), method.as_str(), args.len()), ("xt", "pulse", 1));
    let ir::Stmt::TransactorCall { dest: d1, .. } = calls[1] else { unreachable!() };
    assert!(d1.is_some(), "let call binds the result");
}

/// Locks the dump-ir text for the smallest corpus transactor fixture:
/// the transactor table, `TransactorBody` functions with mirrored
/// params, erased DUT bind, and TransactorCall statements.
#[test]
fn cam_value_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("cam_value_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("cam_value_basic_dump_ir", format!("{prog}"));
}

/// Locks the emitted tbir C++ for the same fixture: `<Type>_<method>`
/// lambdas with synchronous waits (`for (...) tick();` — v1's hookable
/// contract, no co_await), plain `return`, and direct call sites in
/// the run coroutine.
#[test]
fn cam_value_basic_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "cam_value_basic_emitted_cpp",
        emit_fixture_cpp("cam_value_basic_test.harc")
    );
}

/// A transactor method call is never an expression VALUE — it can
/// advance simulated time, which only statement order can represent.
#[test]
fn transactor_call_in_expression_position_rejected() {
    let src = XACTOR_SRC.replace("let v = xt.readv()", "let v = xt.readv() + 1");
    let err = lower_src(&src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("expression position"), "{msg}");
    assert!(msg.contains("hoist it into a `let`"), "{msg}");
}

/// ...and not inside lazily-evaluated log/fail messages either.
#[test]
fn transactor_call_in_message_rejected() {
    let src = XACTOR_SRC.replace("fail(\"v=${v}\")", "fail(\"v=${xt.readv()}\")");
    let err = lower_src(&src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("inside a message"), "{msg}");
}

/// Mode rules at the instance field: passive instances structurally
/// lack their `when active` methods; mode-less fields have nothing to
/// inherit from at testbench scope.
#[test]
fn transactor_instance_mode_rules() {
    let passive = XACTOR_SRC.replace("xt  : Xt active", "xt  : Xt passive");
    let msg = assert_unsupported(&lower_src(&passive).unwrap_err());
    assert!(msg.contains("passive transactor instance"), "{msg}");

    let modeless = XACTOR_SRC.replace("xt  : Xt active", "xt  : Xt");
    let msg = assert_unsupported(&lower_src(&modeless).unwrap_err());
    assert!(msg.contains("without an `active`/`passive` mode"), "{msg}");
}

/// Unknown methods and arity mismatches are hard lowering errors —
/// v1 deferred both to C++ compile failures.
#[test]
fn transactor_call_resolution_is_checked() {
    let unknown = XACTOR_SRC.replace("xt.pulse(3)", "xt.nosuch(3)");
    let err = lower_src(&unknown).unwrap_err();
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("no method `nosuch`"), "{err}");

    let arity = XACTOR_SRC.replace("xt.pulse(3)", "xt.pulse(3, 4)");
    let err = lower_src(&arity).unwrap_err();
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string().contains("takes 1 argument(s), call passes 2"),
        "{err}"
    );

    let void_let = XACTOR_SRC.replace("let v = xt.readv()", "let v = xt.pulse(3)");
    let err = lower_src(&void_let).unwrap_err();
    assert!(err.to_string().contains("returns no value"), "{err}");
}

/// The DUT bind statement is validated: the target must be the
/// transactor's module-typed field and the value must be the test DUT.
#[test]
fn transactor_dut_bind_is_validated() {
    let src = XACTOR_SRC.replace("xt.dut = dut", "xt.dut = 5");
    let msg = assert_unsupported(&lower_src(&src).unwrap_err());
    assert!(msg.contains("something other than the test DUT"), "{msg}");
}

/// Methods keep v1's synchronous hookable semantics, so the suspension
/// forms whose sync emission is out of this slice are rejected at
/// lowering with method-specific messages.
#[test]
fn transactor_method_sync_only_waits() {
    let timed = XACTOR_SRC.replace(
        "wait 1 cycle\n            dut.en = 0",
        "wait until dut.count_out == 1 timeout 5 cycles\n            dut.en = 0",
    );
    let msg = assert_unsupported(&lower_src(&timed).unwrap_err());
    assert!(
        msg.contains("`wait until ... timeout` inside a transactor method"),
        "{msg}"
    );

    let clocked = XACTOR_SRC.replace(
        "wait 1 cycle\n            dut.en = 0",
        "wait 1 cycle on clk\n            dut.en = 0",
    );
    let msg = assert_unsupported(&lower_src(&clocked).unwrap_err());
    assert!(
        msg.contains("`wait N cycles on <clock>` inside a transactor method"),
        "{msg}"
    );
}

/// Bus calls suspend, so they are statement-level only: nesting one in
/// an expression is a precise rejection, not the generic method-call
/// message.
#[test]
fn bus_call_in_expression_position_is_unsupported() {
    let src = r#"
bus MemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus MemBus

testbench ExprTb
    dut : TlmMemory
end testbench ExprTb

impl ExprTest for ExprTb
    let mem : MemBus = bind dut
    run
        assert mem.read(5) == 261 else fail("nope")
    end run
end impl ExprTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("bus method calls in expression position"),
        "{msg}"
    );
}

/// `bind ... with { ... }` signal remaps are emission-side metadata the
/// IR does not carry yet — rejected at the bind site.
#[test]
fn bus_bind_remap_is_unsupported() {
    let src = r#"
bus RemapBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus RemapBus

testbench RemapTb
    dut : TlmMemory
end testbench RemapTb

impl RemapTest for RemapTb
    let mem : RemapBus = bind dut with {
        read.req_valid: "mem_read_req_valid"
    }
    run
        let x = mem.read(5)
    end run
end impl RemapTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("bus bind signal remaps"), "{msg}");
}

/// Unknown channel signals are hard errors with v1's diagnostic text
/// (v1 surfaces them as codegen errors; the IR rejects at lowering).
#[test]
fn bus_unknown_signal_is_invalid() {
    let src = r#"
bus TypoBus
    handshake_channel aw: send kind: valid_ready
        addr: uint<8>
    end handshake_channel aw
end bus TypoBus

testbench TypoTb
    dut : AxiLiteRegs
end testbench TypoTb

impl TypoTest for TypoTb
    let axil : TypoBus = bind dut
    run
        axil.aw.addrr = 24
    end run
end impl TypoTest
"#;
    let err = lower_src(src).unwrap_err();
    assert!(
        matches!(err, lower::LowerError::Invalid(_)),
        "typo must be a hard error: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("channel `aw` has no signal `addrr`")
            && msg.contains("valid: valid, ready, addr"),
        "{msg}"
    );
}

/// The blocking target-side TLM fixtures now lower end-to-end: a
/// `transactor X bound to <Bus>` with `thread bus.<m>(...)` responder
/// bodies, persistent scalar state fields read from the test, and the
/// per-instance state struct + actor schemas.
#[test]
fn tlm_target_blocking_responder_lowers() {
    let prog = lower_src(&fixture("tlm_target_thread_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    // The bound transactor carries the bus + one blocking target method.
    let x = &prog.transactors[0];
    assert_eq!(x.bound_bus.as_deref(), Some("TlmMemBus"));
    assert_eq!(x.target_methods.len(), 1);
    assert_eq!(x.target_methods[0].name, "read");
    assert!(x.target_methods[0].has_ret);
    // The test binds one passive responder actor on bus binding `mem`.
    let tb = &prog.testbenches[0];
    assert_eq!(tb.target_tlm_actors.len(), 1);
    assert_eq!(tb.target_tlm_actors[0].instance, "target");
    assert_eq!(tb.target_tlm_actors[0].bus_field, "mem");
}

/// State fields lower as `TransactorState` reads/writes, instance-filled
/// at the test bind, and the test reads them back (`target.read_count`).
#[test]
fn tlm_target_state_fields_lower() {
    let prog = lower_src(&fixture("tlm_target_thread_if_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let x = &prog.transactors[0];
    let names: Vec<&str> = x.state_fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["read_count", "prep_acc"]);
    // The responder body's state writes are instance-filled to `target`.
    let body = prog.function(x.target_methods[0].function);
    let filled = body.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            s,
            ir::Stmt::TransactorStateWrite { instance, .. } if instance == "target"
        )
    });
    assert!(filled, "responder body must carry instance-filled state writes");
}

/// `out_of_order tags N` target threads stay out of subset (only
/// `blocking` responders are lowered).
#[test]
fn tlm_target_ooo_responder_unsupported() {
    let err = lower_src(&fixture("tlm_pairing_arch_initiator_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("out_of_order"), "{msg}");
}

/// The responder `TbFunction`s are shared per transactor TYPE; binding
/// the same bound transactor to two instances across two tests would
/// clobber the first test's instance-filled bodies. The subset is one
/// passive instance per bound transactor — lowering rejects the second
/// bind loudly (in ALL build profiles), never silently mis-emits.
#[test]
fn tlm_target_multi_instance_unsupported() {
    let src = r#"
bus MemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus MemBus

transactor MemTarget bound to MemBus
    read_count : uint<32> default 0
    thread bus.read(addr: uint<8>)
        read_count = read_count + 1
        return 256 + addr
    end thread
end transactor MemTarget

testbench TbA
    dut : InitA
end testbench TbA

impl TestA for TbA
    let mem : MemBus = bind dut
    let target : MemTarget passive = bind mem
    run
        dut.rst = 1
        wait 1 cycle
    end run
end impl TestA

testbench TbB
    dut : InitA
end testbench TbB

impl TestB for TbB
    let mem2 : MemBus = bind dut
    let responder : MemTarget passive = bind mem2
    run
        dut.rst = 1
        wait 1 cycle
    end run
end impl TestB
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("more than one instance"),
        "expected the multi-instance rejection: {msg}"
    );
}

/// Locks the dump-ir text for a state-bearing target responder: the
/// `bound to` transactor schema, the state-field list, and the responder
/// body's `TransactorState` reads/writes + loop/branch structure.
#[test]
fn tlm_target_thread_if_dump_ir_snapshot() {
    let prog = lower_src(&fixture("tlm_target_thread_if_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("tlm_target_thread_if_dump_ir", format!("{prog}"));
}

/// Locks the emitted-cpp shape for the target responder actor: the
/// per-instance state struct, the background-coroutine actor (req_ready/
/// rsp_valid handshake, arg capture, body loop-switch, response drive),
/// and the test-scope `target.<field>` reads.
#[test]
fn tlm_target_thread_if_emitted_cpp_snapshot() {
    insta::assert_snapshot!(
        "tlm_target_thread_if_emitted_cpp",
        emit_fixture_cpp("tlm_target_thread_if_test.harc")
    );
}

/// Transactor-call seam rule, verifier side: the call edge is pinned
/// to the whole-Assign-RHS position in Run/Check functions and must
/// resolve against the owning testbench's bus bindings.
#[test]
fn verifier_pins_transactor_call_seam() {
    let prog = lower_src(&fixture("tlm_method_blocking_bus_test.harc")).expect("lowers");
    let run_fn = prog.tests[0].run.index();

    // Locate an Assign whose RHS is a TransactorMethod call.
    let find_call = |f: &ir::TbFunction| -> (usize, usize) {
        for (bi, b) in f.blocks.iter().enumerate() {
            for (si, s) in b.stmts.iter().enumerate() {
                if let ir::Stmt::Assign(_, ir::Expr::Call(ir::CallTarget::TransactorMethod { .. }, _)) = s
                {
                    return (bi, si);
                }
            }
        }
        panic!("no TransactorMethod Assign found");
    };

    // 1. Nested in an expression → seam violation.
    let mut broken = prog.clone();
    {
        let f = &mut broken.functions[run_fn];
        let (bi, si) = find_call(f);
        let ir::Stmt::Assign(l, call) = f.blocks[bi].stmts[si].clone() else {
            unreachable!()
        };
        f.blocks[bi].stmts[si] = ir::Stmt::Assign(
            l,
            ir::Expr::Binary(
                ir::BinOp::Add,
                Box::new(call),
                Box::new(ir::Expr::Literal {
                    value: 1,
                    ty: ir::IrType::Unknown,
                }),
            ),
        );
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::BadTransactorCall { .. })),
        "{errs:?}"
    );

    // 2. Unresolved binding → seam violation.
    let mut broken = prog.clone();
    {
        let f = &mut broken.functions[run_fn];
        let (bi, si) = find_call(f);
        if let ir::Stmt::Assign(
            _,
            ir::Expr::Call(ir::CallTarget::TransactorMethod { bus_field, .. }, _),
        ) = &mut f.blocks[bi].stmts[si]
        {
            *bus_field = "ghost".to_string();
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            verify::VerifyError::BadTransactorCall { detail, .. } if detail.contains("no bus binding `ghost`")
        )),
        "{errs:?}"
    );

    // 3. Arity drift against the schema → seam violation.
    let mut broken = prog.clone();
    {
        let f = &mut broken.functions[run_fn];
        let (bi, si) = find_call(f);
        if let ir::Stmt::Assign(_, ir::Expr::Call(_, args)) = &mut f.blocks[bi].stmts[si] {
            args.push(ir::Expr::Literal {
                value: 0,
                ty: ir::IrType::Unknown,
            });
        }
    }
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            verify::VerifyError::BadTransactorCall { detail, .. } if detail.contains("arity mismatch")
        )),
        "{errs:?}"
    );

    // 4. Call edge in a non-Run/Check function → seam violation
    //    (pure helpers must stay suspension-free and placement-neutral).
    let mut broken = prog.clone();
    broken.functions[run_fn].kind = ir::FunctionKind::Helper;
    let errs = verify::verify_program(&broken).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            verify::VerifyError::BadTransactorCall { detail, .. } if detail.contains("Helper")
        )),
        "{errs:?}"
    );
}

/// Placement classifies blocks carrying a transactor call edge as
/// timing-tolerant — the boundary the lowering now actually produces.
#[test]
fn placement_classifies_transactor_call_block_timing_tolerant() {
    let prog = lower_src(&fixture("tlm_method_blocking_bus_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let profile = placement::TargetProfile::single_site();
    let table = placement::run(&prog, &profile);
    let run_id = prog.tests[0].run;
    let f = prog.function(run_id);
    let (bi, _) = f
        .blocks
        .iter()
        .enumerate()
        .find(|(_, b)| {
            b.stmts.iter().any(|s| {
                matches!(
                    s,
                    ir::Stmt::Assign(_, ir::Expr::Call(ir::CallTarget::TransactorMethod { .. }, _))
                )
            })
        })
        .expect("a block carries the call edge");
    let (_, timing) = table.blocks[&(run_id, ir::BlockId(bi as u32))];
    assert_eq!(timing, placement::TimingClass::TimingTolerant);
}

/// Out-of-subset transactor shapes reject with precise messages:
/// event fields (the sequencer-driven form), scalar state fields, and
/// >64-bit method params (the tbir value model is u64).
#[test]
fn transactor_shape_rejections() {
    let event_src = r#"
transaction Req
    addr : uint<8>
end transaction Req

transactor Ev
    dut : Top

    when active
        req : in event<Req>
    end when
end transactor Ev

testbench EvTb
    dut : Top
    ev  : Ev active
end testbench EvTb

impl EvTest for EvTb
    run
        wait 1 cycle
    end run
end impl EvTest
"#;
    let msg = assert_unsupported(&lower_src(event_src).unwrap_err());
    assert!(msg.contains("event/directional field `req`"), "{msg}");

    let state_src = event_src.replace("req : in event<Req>", "count : uint<32>");
    let msg = assert_unsupported(&lower_src(&state_src).unwrap_err());
    assert!(msg.contains("state field `count`"), "{msg}");

    // The corpus fixture with uint<128> method params.
    let err = lower_src(&fixture("aes_cipher_top_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(msg.contains("wider than 64 bits"), "{msg}");
}

/// Verifier net: a `TransactorMethod` call edge nested in expression
/// position (i.e. anywhere but the root of `Stmt::TransactorCall`) is
/// a `BadTransactorCall` — lowering never produces it, so reaching it
/// means a pass corrupted the IR.
#[test]
fn verifier_rejects_call_edge_in_expression_position() {
    let mut prog = lower_src(XACTOR_SRC).expect("lowers");
    verify::verify_program(&prog).expect("clean before mutation");
    // Rewrite the first TransactorCall into a plain Assign of the
    // call-edge expression.
    let run_id = prog.tests[0].run;
    let run = &mut prog.functions[run_id.index()];
    let mut mutated = false;
    for b in &mut run.blocks {
        for s in &mut b.stmts {
            if let ir::Stmt::TransactorCall { call, .. } = s {
                let dest = ir::LocalId(0);
                *s = ir::Stmt::Assign(dest, call.clone());
                mutated = true;
                break;
            }
        }
        if mutated {
            break;
        }
    }
    assert!(mutated, "fixture carries a TransactorCall");
    let errs = verify::verify_program(&prog).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| matches!(e, verify::VerifyError::BadTransactorCall { .. })),
        "expected BadTransactorCall, got: {errs:?}"
    );
}

/// `lower_coroutine` tags transactor method bodies (they are the
/// Tier-0 FSM candidates): the suspension inside `pulse` becomes a
/// state boundary.
#[test]
fn lower_coroutine_tags_transactor_bodies() {
    let prog = lower_src(XACTOR_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let meta = lower_coroutine::run(&prog).expect("pass runs");
    let pulse = prog.transactors[0].method("pulse").unwrap().function;
    let states = meta.state_enum.get(&pulse).expect("pulse tagged");
    assert!(states.len() >= 2, "wait creates a resume state: {states:?}");
}

/// A method param (or any local) that shadows the DUT field name is
/// host state — `dut.x` through it must NOT silently lower to a DUT
/// access (v1 surfaces the shadowing as a C++ compile error; the IR
/// rejects at lowering).
#[test]
fn local_shadowing_dut_name_does_not_mislower() {
    let src = r#"
transactor Sh
    dut : Top

    when active
        hookable poke(dut: uint<8>)
            dut.en = 1
        end poke
    end when
end transactor Sh

testbench ShTb
    dut : Top
    sh  : Sh active
end testbench ShTb

impl ShTest for ShTb
    run
        sh.dut = dut
        sh.poke(1)
    end run
end impl ShTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("assignment to a non-port, non-local target"),
        "shadowed name must not resolve to the DUT: {msg}"
    );
}

// ── Singleton-blocker batch (ternary, time/wide literals, const/enum,
//    test-scope lets, indexed lanes, testbench methods/fields, width
//    methods): one dump-ir snapshot per newly-registered fixture. ────

/// Ternary expressions inside CFG-inlined impure helpers, plus the
/// `WaitCyclesSync` terminator (v1's synchronous helper-lambda waits).
#[test]
fn linklist_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("linklist_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("linklist_basic_dump_ir", format!("{prog}"));
}

/// Wall-clock waits (`wait 80ns` → `WaitTimePs`) and the `debug` log
/// severity, under the two-clock scheduler.
#[test]
fn async_fifo_dump_ir_snapshot() {
    let prog = lower_fixtures(&["async_fifo_test.harc", "async_fifo_domains.harc"])
        .expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("async_fifo_dump_ir", format!("{prog}"));
}

/// 256-bit literals: `WideLiteral` word lists in DutWrite values and
/// `==`/`!=` assert conditions.
#[test]
fn wide_reg_dump_ir_snapshot() {
    let prog = lower_src(&fixture("wide_reg_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("wide_reg_dump_ir", format!("{prog}"));
}

/// 512-bit message-block literals + `while !dut.done` header re-reads.
#[test]
fn sha256_dump_ir_snapshot() {
    let prog = lower_src(&fixture("sha256_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("sha256_dump_ir", format!("{prog}"));
}

/// Test-scope `let`s hoisted to the head of the run function.
#[test]
fn if_wait_for_in_then_dump_ir_snapshot() {
    let prog = lower_src(&fixture("if_wait_for_in_then_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("if_wait_for_in_then_dump_ir", format!("{prog}"));
}

/// Constant-lane DUT port access (`dut.<port>[i]` reads and writes,
/// `PortRef::lane`) across packed and unpacked port shapes.
#[test]
fn packed_vec_lane_dump_ir_snapshot() {
    let prog = lower_src(&fixture("packed_vec_lane_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("packed_vec_lane_dump_ir", format!("{prog}"));
}

/// Testbench helper methods (`_tb.reset()` / `_tb.bump(n)`) CFG-
/// inlined into two `--test`-selectable tests.
#[test]
fn testbench_basic_dump_ir_snapshot() {
    let prog = lower_src(&fixture("testbench_basic_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("testbench_basic_dump_ir", format!("{prog}"));
}

/// Scalar testbench fields (`expected : uint<32> default 0`):
/// `TbFieldWrite` in run, `TbField` reads in the shared check phase.
#[test]
fn testbench_lifecycle_dump_ir_snapshot() {
    let prog = lower_src(&fixture("testbench_lifecycle_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("testbench_lifecycle_dump_ir", format!("{prog}"));
}

/// Width-method intrinsics (`.trunc/.zext/.sext/.resize`) with
/// receiver widths from typed lets, casts, and chained methods.
#[test]
fn width_methods_dump_ir_snapshot() {
    let prog = lower_src(&fixture("width_methods_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("width_methods_dump_ir", format!("{prog}"));
}

// ── regblock construct (register-level frontdoor subset) ─────────────

/// The regblock subset fixture lowers cleanly: a `regblock` declaration
/// becomes a synthetic mirror record (one scalar field per register,
/// defaulting to its reset value) plus a `RegblockSchema`; the
/// test-scope unbound-transactor `let h : RegHelper active` registers as
/// a transactor instance; and register-level access lowers to mirror
/// `RecordFieldWrite` / reads plus `Helper.write`/`read`
/// `TransactorCall` edges. Snapshotted end-to-end.
#[test]
fn regblock_subset_dump_ir_snapshot() {
    let prog = lower_src(&fixture("regblock_subset_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    insta::assert_snapshot!("regblock_subset_dump_ir", format!("{prog}"));
}

/// Register-level write/read lowering shapes. A RW write emits the
/// mirror `RecordFieldWrite` then the helper `write` call edge; a RW
/// read emits the helper `read` call edge into the destination local
/// then a mirror-predict `RecordFieldWrite`.
#[test]
fn regblock_rw_write_then_read_lowers_to_mirror_plus_call_edge() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register A @ 0x10 access rw
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        regs.A = 5
        let v = regs.A
    end run
end impl Test
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    let body = run
        .blocks
        .iter()
        .map(|b| format!("{b:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    // Mirror init at entry, write = mirror + call edge, read = call edge
    // + mirror predict.
    assert!(body.contains("RecordInit"), "mirror init missing:\n{body}");
    assert!(
        body.matches("RecordFieldWrite").count() >= 2,
        "expected a write-side mirror update and a read-side predict:\n{body}"
    );
    assert!(
        body.matches("TransactorMethod").count() >= 2,
        "expected write + read frontdoor call edges:\n{body}"
    );
}

/// RO write suppresses the bus traffic (mirror update only); WO read
/// serves from the mirror (no bus traffic).
#[test]
fn regblock_ro_write_and_wo_read_skip_the_bus() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register RO @ 0x00 access ro
    register WO @ 0x04 access wo
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        regs.RO = 1
        let w = regs.WO
    end run
end impl Test
"#;
    let prog = lower_src(src).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let run = prog.function(prog.tests[0].run);
    let body = run
        .blocks
        .iter()
        .map(|b| format!("{b:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    // RO write + WO read make NO frontdoor call edges (both stay local).
    assert!(
        !body.contains("TransactorMethod"),
        "RO write and WO read must not reach the bus:\n{body}"
    );
}

/// Field-level access (`regs.REG.FIELD`) is out of subset — rejected
/// with a precise message, never mis-lowered into a mirror read.
#[test]
fn regblock_field_level_access_is_unsupported() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register A @ 0x00 access rw
        field F : bit @ 0 access rw
    end register A
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        regs.A.F = 1
    end run
end impl Test
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("field-level"),
        "expected a field-level-decomposition rejection: {msg}"
    );
}

/// A register read outside `let`-RHS position (here an assert condition)
/// is rejected — v1 reads the bus inline at every read site, which the
/// IR's statement model can't represent without changing the read count.
#[test]
fn regblock_read_in_assert_is_unsupported() {
    let src = r#"
transactor H
    dut : Top
    when active
        hookable write(addr: uint<8>, data: uint<32>)
            dut.en = 1
        end write
        hookable read(addr: uint<8>) -> uint<32>
            return addr
        end read
    end when
end transactor H
regblock R via H width 32
    register A @ 0x00 access rw
end regblock R
testbench Tb
    dut : Top
end testbench Tb
impl Test for Tb
    let h : H active
    let regs : R = bind h
    run
        h.dut = dut
        assert regs.A == 1 else fail("x")
    end run
end impl Test
"#;
    let err = lower_src(src).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("outside a `let`"),
        "expected an out-of-let read rejection: {msg}"
    );
}

/// The corpus `regblock_basic_test` fixture's `via` helper is a
/// `transactor ... bound to BusAxiLite` whose methods are `hookable`
/// bodies driving the bus's handshake channels — the *initiator-side*
/// bus-bound BFM form. That is the documented residual blocker after
/// the target-side TLM slice; lowering rejects it precisely (it now
/// reaches the bound-to transactor path and names the hookable),
/// never mis-lowers.
#[test]
fn regblock_basic_corpus_blocked_on_initiator_side_bfm() {
    let err = lower_src(&fixture("regblock_basic_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    assert!(
        msg.contains("initiator-side method") || msg.contains("hookable"),
        "expected the initiator-side BFM residual blocker: {msg}"
    );
}
