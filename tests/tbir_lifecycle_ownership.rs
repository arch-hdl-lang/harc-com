//! #619 M4a sub-steps 2–4: reusable testbench lifecycle ownership in the
//! TB-IR, and the conservative-fallback shareability decision.
//!
//! Under `HARC_TBIR_NATIVE_LIFECYCLE`, a bound testbench's `setup`/`check`/
//! `teardown` body is lowered ONCE into a `FunctionKind::TestbenchLifecycle`
//! function, each per-test `__harc_tb_lifecycle_<phase>()` marker lowers to
//! a `Terminator::TbLifecycleCall` call edge, and the emitter re-inlines the
//! callee body at the call site — BUT only when sharing is provably
//! trace-safe. A testbench whose lifecycle (or any of its methods) mints
//! per-test side-table state, or whose fields are shadowed by a binding
//! test, falls back to the historical per-test inlining (byte-identical to
//! switch-OFF).
//!
//! End-to-end trace equality (v1 == tbir) is the primary gate in
//! `tests/run_tbir_equiv.sh`; this file pins the IR/emit shape and the
//! share/fallback decision deterministically in-process.
//!
//! The switch is read from the environment. Integration-test functions run
//! in parallel threads within one binary, so every env-touching section is
//! serialized through `ENV_LOCK` and always clears the variable (even on a
//! panic) via `with_switch`.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;
use std::path::Path;
use std::sync::Mutex;

const SWITCH: &str = "HARC_TBIR_NATIVE_LIFECYCLE";

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with the native-lifecycle switch forced on/off, serialized
/// against every other env-touching test and always clearing the variable
/// afterward (panics are re-raised after cleanup).
fn with_switch<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if on {
        std::env::set_var(SWITCH, "1");
    } else {
        std::env::remove_var(SWITCH);
    }
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::env::remove_var(SWITCH);
    match r {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}

fn merged_fixture(name: &str) -> harc::ast::SourceFile {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let src = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    let parsed = parse_source(&src).expect("fixture parses");
    merge::merge_for_sim(vec![parsed], None).expect("merge")
}

fn lifecycle_fn_count(prog: &ir::TbProgram) -> usize {
    prog.functions
        .iter()
        .filter(|f| matches!(f.kind, ir::FunctionKind::TestbenchLifecycle { .. }))
        .count()
}

fn lifecycle_call_targets(f: &ir::TbFunction) -> Vec<ir::FunctionId> {
    f.blocks
        .iter()
        .filter_map(|b| match &b.terminator {
            ir::Terminator::TbLifecycleCall { function, .. } => Some(*function),
            _ => None,
        })
        .collect()
}

fn any_lifecycle_call(prog: &ir::TbProgram) -> bool {
    prog.functions
        .iter()
        .any(|f| !lifecycle_call_targets(f).is_empty())
}

#[test]
fn native_lifecycle_lowers_once_and_re_inlines() {
    let merged = merged_fixture("testbench_lifecycle_test.harc");

    // --- switch OFF: historical per-test inlining, no ownership ---
    let (prog_off, cpp_off) = with_switch(false, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch off)");
        verify::verify_program(&prog).expect("verifies (switch off)");
        let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits off");
        (prog, cpp)
    });
    assert_eq!(
        lifecycle_fn_count(&prog_off),
        0,
        "switch OFF must not create any TestbenchLifecycle function"
    );
    assert!(
        !any_lifecycle_call(&prog_off),
        "switch OFF must not emit any TbLifecycleCall"
    );

    // --- switch ON: shared ownership, marker → call edge, re-inline ---
    let (prog_on, cpp_on) = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits on");
        (prog, cpp)
    });

    // The fixture's bound testbench (CounterLifecycleTb) declares `setup`
    // and `check` (no teardown), is bound by TWO impls, and is SAFE to
    // share (setup calls a plain `reset()` method; check is an immediate
    // assert). Ownership means the two phase bodies are lowered EXACTLY
    // ONCE each — shared across both tests — so there are two
    // TestbenchLifecycle functions total, not two-per-test.
    let lifecycle_fns: Vec<&ir::TbFunction> = prog_on
        .functions
        .iter()
        .filter(|f| matches!(f.kind, ir::FunctionKind::TestbenchLifecycle { .. }))
        .collect();
    assert_eq!(
        lifecycle_fns.len(),
        2,
        "exactly one TestbenchLifecycle function per (testbench, phase): setup + check"
    );
    let mut setup_fn = None;
    let mut check_fn = None;
    for f in &lifecycle_fns {
        if let ir::FunctionKind::TestbenchLifecycle { phase, .. } = &f.kind {
            match phase {
                harc::ast::LifecyclePhase::Setup => setup_fn = Some(f.id),
                harc::ast::LifecyclePhase::Check => check_fn = Some(f.id),
                harc::ast::LifecyclePhase::Teardown => panic!("fixture declares no teardown"),
            }
        }
    }
    let setup_fn = setup_fn.expect("a setup lifecycle function");
    let check_fn = check_fn.expect("a check lifecycle function");

    // Every binding test references the SAME shared functions via a
    // TbLifecycleCall — run calls setup, check calls check.
    let mut run_calls = 0;
    let mut check_calls = 0;
    for test in &prog_on.tests {
        let run = prog_on.function(test.run);
        assert!(
            lifecycle_call_targets(run).contains(&setup_fn),
            "run body of `{}` must TbLifecycleCall the shared setup function",
            test.name
        );
        run_calls += 1;
        if let Some(check) = test.check {
            let check = prog_on.function(check);
            assert!(
                lifecycle_call_targets(check).contains(&check_fn),
                "check body of `{}` must TbLifecycleCall the shared check function",
                test.name
            );
            check_calls += 1;
        }
    }
    assert_eq!(run_calls, 2, "both impls call the shared setup");
    assert_eq!(check_calls, 2, "both impls call the shared check");

    // The re-inline reproduces the lifecycle body: switch-ON C++ still
    // drives the reset (`rst`) and runs the shared final check
    // (`shared final check passed`), like switch-OFF, and carries the
    // re-inlined named lifecycle loop-switch.
    for needle in ["rst", "shared final check passed"] {
        assert!(
            cpp_off.contains(needle),
            "switch-OFF C++ should contain `{needle}` (sanity)"
        );
        assert!(
            cpp_on.contains(needle),
            "switch-ON re-inline must reproduce lifecycle body containing `{needle}`"
        );
    }
    assert!(
        cpp_on.contains("__tb_lifecycle_CounterLifecycleTb"),
        "switch-ON C++ re-inlines the named TestbenchLifecycle loop-switch"
    );
    assert!(
        !cpp_off.contains("__tb_lifecycle_CounterLifecycleTb"),
        "switch-OFF C++ has no lifecycle function (bodies are inlined directly)"
    );
}

#[test]
fn unsafe_testbench_side_table_state_falls_back() {
    // A testbench that owns a `randomize`-bearing helper method is UNSAFE
    // to share (the conservative union scan flags it), so even with the
    // switch ON it must fall back to historical inlining: no
    // TestbenchLifecycle function, no TbLifecycleCall.
    let merged = merged_fixture("tb_lifecycle_unsafe_share_test.harc");
    let prog = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        prog
    });
    assert_eq!(
        lifecycle_fn_count(&prog),
        0,
        "an unsafe (randomize-in-method) testbench must NOT create a TestbenchLifecycle function"
    );
    assert!(
        !any_lifecycle_call(&prog),
        "an unsafe testbench must fall back to inlining — no TbLifecycleCall"
    );
}

#[test]
fn field_shadowing_testbench_falls_back() {
    // One binding test shadows the `warmup` testbench field with a
    // test-scope `let`, so the shared (first-bind) rewrite would diverge
    // for it. The decision must fall back to historical inlining.
    let merged = merged_fixture("tb_lifecycle_shadow_share_test.harc");
    let prog = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        prog
    });
    assert_eq!(
        lifecycle_fn_count(&prog),
        0,
        "a field-shadowed testbench must NOT create a TestbenchLifecycle function"
    );
    assert!(
        !any_lifecycle_call(&prog),
        "a field-shadowed testbench must fall back to inlining — no TbLifecycleCall"
    );
}
