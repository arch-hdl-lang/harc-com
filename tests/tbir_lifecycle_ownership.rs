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
///
/// #619 M4a sub-step 4 part 2: native lifecycle lowering is now the DEFAULT
/// (unset ⇒ ON), so forcing it OFF requires the explicit `=0` opt-out — not
/// merely clearing the variable, which would leave the default (ON) in
/// place. The `on` case still sets `=1` (any non-"0" value enables).
fn with_switch<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(SWITCH, if on { "1" } else { "0" });
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

#[test]
fn concurrent_assertion_in_check_falls_back() {
    // The bound testbench's `check` phase contains a temporal assertion
    // (`assert dut.en |-> dut.en`). Lowering routes it through the
    // concurrent-property path (a per-test PropertyCheckId), so the scan —
    // using the authoritative `is_concurrent_assertion` predicate, NOT a
    // syntactic named/property_kw shortcut — must classify it UNSAFE and
    // fall back. This is the case the earlier soundness bug shared.
    let merged = merged_fixture("tb_lifecycle_concurrent_assert_test.harc");
    let prog = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        prog
    });
    assert_eq!(
        lifecycle_fn_count(&prog),
        0,
        "a testbench with a concurrent/temporal assertion in a phase must NOT be shared"
    );
    assert!(
        !any_lifecycle_call(&prog),
        "a concurrent-assertion phase must fall back to inlining — no TbLifecycleCall"
    );
}

#[test]
fn method_name_shadowing_testbench_falls_back() {
    // One binding test declares `let prep` colliding with the `prep`
    // testbench METHOD name. `rewrite_expr_for_impl` suppresses bare
    // method-call rewriting through the same flat shadow set as fields, so
    // a method-name collision must force fallback too.
    let merged = merged_fixture("tb_lifecycle_method_shadow_test.harc");
    let prog = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        prog
    });
    assert_eq!(
        lifecycle_fn_count(&prog),
        0,
        "a method-name-shadowed testbench must NOT create a TestbenchLifecycle function"
    );
    assert!(
        !any_lifecycle_call(&prog),
        "a method-name-shadowed testbench must fall back to inlining — no TbLifecycleCall"
    );
}

/// Count non-overlapping occurrences of `needle` in `hay`.
fn count_occurrences(hay: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut n = 0;
    let mut i = 0;
    while let Some(pos) = hay[i..].find(needle) {
        n += 1;
        i += pos + needle.len();
    }
    n
}

#[test]
fn m4b_suspending_setup_emitted_once_as_coroutine() {
    // #619 M4b (suspending slice): a bound testbench whose `setup` waits
    // DIRECTLY in the lifecycle body (`wait N cycles` → coroutine
    // WaitCycles, not the method-call WaitCyclesSync) is emitted OUT-OF-LINE
    // exactly ONCE as a `harc_rt::HarcThread` coroutine, and each binding
    // test drives it via the parent-drives-child loop
    // (`co_await harc_rt::harc_lifecycle_yield()`). The non-suspending
    // `check` is emitted once as a plain `void` function. Two impls bind it.
    let merged = merged_fixture("tb_lifecycle_wait_setup_test.harc");
    let cpp = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits on")
    });

    // The suspending setup coroutine is DEFINED exactly once, out of line.
    let coro_def = "harc_rt::HarcThread _harc_lc__tb_lifecycle_WaitSetupTb_Setup(";
    assert_eq!(
        count_occurrences(&cpp, coro_def),
        1,
        "suspending setup must be emitted exactly once as a HarcThread coroutine"
    );
    // Its signature threads the caller's own slot through.
    assert!(
        cpp.contains(
            "harc_rt::HarcThread _harc_lc__tb_lifecycle_WaitSetupTb_Setup(HarcTestContext& ctx, \
             WaitSetupTb& _tb, harc_rt::ThreadSlot* _slot)"
        ),
        "the out-of-line lifecycle coroutine takes (ctx, _tb, _slot)"
    );

    // The non-suspending check is a plain void function, once.
    assert_eq!(
        count_occurrences(&cpp, "static void _harc_lc__tb_lifecycle_WaitSetupTb_Check("),
        1,
        "non-suspending check must be emitted exactly once as a plain void function"
    );

    // Two binding tests each drive the shared setup coroutine via the
    // parent-drives-child loop (start, yield-loop, destroy).
    assert_eq!(
        count_occurrences(
            &cpp,
            "auto _lc_sub = _harc_lc__tb_lifecycle_WaitSetupTb_Setup(ctx, _tb, _slot); _lc_sub.resume();"
        ),
        2,
        "both impls' run coroutines drive the shared setup coroutine"
    );
    assert_eq!(
        count_occurrences(&cpp, "co_await harc_rt::harc_lifecycle_yield();"),
        2,
        "each drive loop yields to the scheduler while re-driving the child"
    );
    // Two call sites also invoke the shared check (plain call).
    assert_eq!(
        count_occurrences(&cpp, "_harc_lc__tb_lifecycle_WaitSetupTb_Check(ctx, _tb);"),
        2,
        "both impls' check coroutines call the shared check function"
    );
}

#[test]
fn m4b_randomize_in_suspending_lifecycle_falls_back() {
    // #619 M4b: a suspending lifecycle that ALSO randomizes. The M4a
    // sharing desugar marks a randomize-bearing testbench UNSAFE to share,
    // so NO TestbenchLifecycle is minted and the whole thing falls back to
    // per-test re-inline — there is nothing for M4b to emit out of line, so
    // no coroutine/void lifecycle symbol appears. This is why the RNG-order
    // risk never materializes: such bodies are never shared.
    let merged = merged_fixture("tb_lifecycle_rand_suspend_test.harc");
    let (prog, cpp) = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("emits on");
        (prog, cpp)
    });
    assert_eq!(
        lifecycle_fn_count(&prog),
        0,
        "a randomize-bearing lifecycle testbench must fall back (no TestbenchLifecycle fn)"
    );
    assert!(
        !any_lifecycle_call(&prog),
        "a randomize-bearing lifecycle must fall back to inlining — no TbLifecycleCall"
    );
    assert!(
        !cpp.contains("_harc_lc__tb_lifecycle_RandDelayTb"),
        "no out-of-line lifecycle symbol is emitted for a fallback testbench"
    );
}

#[test]
fn m4b_split_common_layout_emits_lifecycle_once_in_common() {
    // #619 M4b (cross-shard de-dup): under the separate/common split layout
    // with the switch ON, each shareable lifecycle body is DEFINED exactly
    // once in the common `.cpp` (external linkage) with a prototype in the
    // interface header, and each shard CALLS it (never re-inlines / redefines
    // it). This is the payoff #619 targets: the shared body compiles once per
    // suite, not once per shard. Two impls, group size 1 → two shards.
    let merged = merged_fixture("tb_lifecycle_wait_setup_test.harc");
    let coro_sig = "harc_rt::HarcThread _harc_lc__tb_lifecycle_WaitSetupTb_Setup(HarcTestContext";
    let check_sig = "void _harc_lc__tb_lifecycle_WaitSetupTb_Check(HarcTestContext";

    let (iface, common, shards) = with_switch(true, || {
        let prog = lower::lower_program(&merged).expect("lowers (switch on)");
        verify::verify_program(&prog).expect("verifies (switch on)");
        let opts = cpp_tb::EmitOpts::default();
        // group_size 1 → one test per shard → two shards, so the
        // once-in-common / zero-in-shard property is exercised across shards.
        let plan =
            tbir::plan_separate_tests(&prog, &merged, &opts, "", 1).expect("separate plan");
        let iface =
            tbir::emit_separate_interface_with_prefix(&prog, &merged, &opts, &plan.scaffold, "")
                .expect("interface");
        let common =
            tbir::emit_separate_common_with_prefix(&prog, &merged, &opts, &plan.scaffold, "")
                .expect("common");
        let shards: Vec<String> = plan
            .shards
            .iter()
            .map(|s| {
                tbir::emit_separate_shard_with_prefix(&prog, &merged, &opts, &plan.scaffold, s, "")
                    .expect("shard")
            })
            .collect();
        (iface, common, shards)
    });

    assert!(shards.len() >= 2, "group_size 1 must produce ≥2 shards");

    // The definition (signature followed by a body `{`) lives ONCE in common.
    assert_eq!(
        count_occurrences(&common, &format!("{coro_sig}& ctx")),
        1,
        "the suspending setup coroutine must be DEFINED exactly once in common.cpp"
    );
    assert_eq!(
        count_occurrences(&common, &format!("{check_sig}& ctx")),
        1,
        "the non-suspending check must be DEFINED exactly once in common.cpp"
    );

    // The header carries a prototype for each (so shards can call them).
    assert!(
        iface.contains(&format!("{coro_sig}& ctx, WaitSetupTb& _tb, harc_rt::ThreadSlot* _slot);")),
        "interface header must declare the setup coroutine prototype"
    );
    assert!(
        iface.contains(&format!("{check_sig}& ctx, WaitSetupTb& _tb);")),
        "interface header must declare the check function prototype"
    );

    // NO shard defines or re-inlines the bodies; each shard CALLS them.
    let mut setup_drives = 0;
    let mut check_calls = 0;
    for (i, shard) in shards.iter().enumerate() {
        assert_eq!(
            count_occurrences(shard, coro_sig),
            0,
            "shard {i} must NOT define/redefine the shared setup coroutine"
        );
        assert_eq!(
            count_occurrences(shard, check_sig),
            0,
            "shard {i} must NOT define/redefine the shared check function"
        );
        // Re-inline would emit the named loop-switch comment; a call must not.
        assert!(
            !shard.contains("// __tb_lifecycle_WaitSetupTb_Setup (TB-IR loop-switch)"),
            "shard {i} must CALL the shared setup, not re-inline its loop-switch"
        );
        setup_drives += count_occurrences(
            shard,
            "_harc_lc__tb_lifecycle_WaitSetupTb_Setup(ctx, _tb, _slot)",
        );
        check_calls +=
            count_occurrences(shard, "_harc_lc__tb_lifecycle_WaitSetupTb_Check(ctx, _tb);");
    }
    assert_eq!(setup_drives, shards.len(), "every shard drives the shared setup coroutine");
    assert_eq!(check_calls, shards.len(), "every shard calls the shared check");
}
