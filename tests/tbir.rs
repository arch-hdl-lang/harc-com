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
    let prog = lower_src(&fixture(name)).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    tbir::emit(&prog, &cpp_tb::EmitOpts::default()).expect("emits")
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
/// The fixture's `transaction` items now lower (records slice); the
/// item-level scan trips on the `transactor` declaration instead.
#[test]
fn transactor_fixture_is_unsupported() {
    let err = lower_src(&fixture("axilite_seqdrv_test.harc")).unwrap_err();
    let msg = assert_unsupported(&err);
    insta::assert_snapshot!("axilite_seqdrv_unsupported", msg);
}

/// Randomize/constraint fixture (`randomize(t) with` + Z3 constraints,
/// loaded together with its helper file exactly as run_fixtures.sh
/// does) is outside the MVP subset. Its `transaction` declaration now
/// lowers (records slice), so the rejection shifted to the
/// statement-level `randomize` gate pointing at the constraint-IR
/// seam — exactly the shift docs/tbir-mvp.md §"Negative tests"
/// predicted.
#[test]
fn randomize_fixture_is_unsupported() {
    let err = lower_fixtures(&["axilite_constraint_test.harc", "axilite_regs_test.harc"])
        .unwrap_err();
    let msg = assert_unsupported(&err);
    insta::assert_snapshot!("axilite_constraint_unsupported", msg);
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
    let prog = lower_src(&fixture("wait_until_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &cpp_tb::EmitOpts::default()).expect("emits");
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

#[test]
fn randomize_is_unsupported() {
    let src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test RandTest
    let dut : Top
    run
        let t : Req
        randomize(t)
    end run
end test RandTest
"#;
    let err = lower_src(src).unwrap_err();
    let msg = err.to_string();
    // The `transaction` declaration and the `let t : Req` lower fine
    // now (records slice); the statement-level `randomize` gate fires
    // and points at the constraint-IR seam.
    assert!(msg.contains("`randomize`"), "names randomize: {msg}");
    assert!(
        msg.contains("constraint-IR seam"),
        "points at the constraint seam: {msg}"
    );
    assert!(msg.contains("--codegen v1"), "suggests v1: {msg}");
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

    // The run body inlined read_addr (WaitCycles from the helper body)
    // and calls double_it by name.
    let run = prog.function(prog.tests[0].run);
    let waits = run
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, ir::Terminator::WaitCycles(..)))
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
/// co_await in the run coroutine (CFG-inlined, not a call).
#[test]
fn tbir_emit_helper_mix() {
    let prog = lower_src(HELPER_MIX_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &cpp_tb::EmitOpts::default()).expect("emits");
    for marker in [
        "static uint64_t harc_helper_double_it(uint64_t x);",
        "static uint64_t harc_helper_double_it(uint64_t x) {",
        "harc_helper_double_it(__t",
        "co_await harc_rt::wait_cycles(_slot, (uint32_t)(1));",
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
    let prog = lower_src(WAIT_ON_CLOCK_SRC).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &cpp_tb::EmitOpts::default()).expect("emits");
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
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &cpp_tb::EmitOpts::default()).expect("emits");
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
    let prog = lower_src(&fixture("sync_fifo_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &cpp_tb::EmitOpts::default()).expect("emits");
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
    let prog = lower_src(&fixture("cov_cross_bins_test.harc")).expect("lowers");
    verify::verify_program(&prog).expect("verifies");
    let cpp = tbir::emit(&prog, &cpp_tb::EmitOpts::default()).expect("emits");
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
    let prog = lower_src(&fixture("top_counter_test.harc")).expect("lowers");
    let opts = cpp_tb::EmitOpts {
        mt: true,
        ..Default::default()
    };
    let err = tbir::emit(&prog, &opts).unwrap_err();
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
