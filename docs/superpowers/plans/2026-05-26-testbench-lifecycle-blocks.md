# Testbench Lifecycle Blocks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `setup`, `check`, and `teardown` blocks inside `testbench` declarations and automatically compose them into every bound `impl <Test> for <Tb>`.

**Architecture:** Represent testbench lifecycle blocks as a new `ComponentItem::Lifecycle(ScopeDecl)` item, accepted only in `ComponentKind::Testbench`. Reuse existing `ScopeDecl` and `emit_block` machinery, and splice the bound testbench blocks into the existing test coroutine before/after test-local lifecycle blocks.

**Tech Stack:** Rust compiler crate, recursive-descent parser, AST pretty-printer, C++ testbench codegen, Rust integration tests, Verilator fixture manifest.

---

## File Structure

- Modify `src/ast.rs`: add `ComponentItem::Lifecycle(ScopeDecl)`.
- Modify `src/parser.rs`: parse `setup`/`check`/`teardown` only inside `testbench`; reject `run`; reject lifecycle blocks in non-testbench components.
- Modify `src/pretty.rs`: print component lifecycle blocks using existing lifecycle spelling.
- Modify `src/codegen/cpp_tb.rs`: collect lifecycle blocks from bound testbench and emit them in the approved order.
- Modify `tests/round_trip.rs`: add parse/pretty/reparse coverage and parser diagnostic tests.
- Modify `tests/codegen.rs`: add codegen ordering and inherited-check coverage.
- Add `tests/fixtures/testbench_lifecycle_test.harc`: runnable fixture with two bound tests sharing a testbench final check.
- Modify `tests/run_fixtures.sh`: add fixture entries.

## Task 1: Parser And Pretty-Printer

**Files:**
- Modify: `src/ast.rs`
- Modify: `src/parser.rs`
- Modify: `src/pretty.rs`
- Test: `tests/round_trip.rs`

- [ ] **Step 1: Write failing parser/round-trip tests**

Add tests to `tests/round_trip.rs`:

```rust
#[test]
fn testbench_lifecycle_blocks_round_trip() {
    let src = r#"
testbench Tb
    dut : DummyDut

    setup
        dut.rst = 1
    end setup

    check
        assert dut.done == 1 else fail("not done")
    end check

    teardown
        log(info, "tb teardown")
    end teardown
end testbench Tb

impl Smoke for Tb
    run
        wait 1 cycle
    end run
end impl Smoke
"#;
    let printed = parse_print_reparse(src);
    assert!(printed.contains("setup\n        dut.rst = 1\n    end setup"));
    assert!(printed.contains("check\n        assert dut.done == 1 else fail(\"not done\")\n    end check"));
    assert!(printed.contains("teardown\n        log(info, \"tb teardown\")\n    end teardown"));
}

#[test]
fn testbench_rejects_run_lifecycle_block() {
    let src = r#"
testbench Tb
    dut : DummyDut
    run
        wait 1 cycle
    end run
end testbench Tb
"#;
    let err = parse_source(src).expect_err("run inside testbench should be rejected");
    assert!(err.to_string().contains("`run` belongs to a testcase"));
}

#[test]
fn non_testbench_component_rejects_lifecycle_block() {
    let src = r#"
env Env
    check
        log(info, "bad")
    end check
end env Env
"#;
    let err = parse_source(src).expect_err("env lifecycle block should be rejected");
    assert!(err.to_string().contains("lifecycle blocks are currently supported only inside `test`/`impl` and `testbench`"));
}

#[test]
fn duplicate_testbench_lifecycle_block_is_rejected() {
    let src = r#"
testbench Tb
    dut : DummyDut
    check
        log(info, "first")
    end check
    check
        log(info, "second")
    end check
end testbench Tb
"#;
    let err = parse_source(src).expect_err("duplicate testbench check should be rejected");
    assert!(err.to_string().contains("duplicate `check` block in testbench `Tb`"));
}
```

- [ ] **Step 2: Verify tests fail**

Run:

```bash
cargo test --test round_trip testbench_lifecycle -- --nocapture
```

Expected: parser errors because `setup`/`check`/`teardown` are not accepted inside `testbench`.

- [ ] **Step 3: Implement AST/parser/pretty support**

Changes:

```rust
// src/ast.rs
pub enum ComponentItem {
    Field(ComponentField),
    Connect(ConnectBlock),
    OnHandler(OnHandler),
    TargetTlmThread(TargetTlmThread),
    Hookable(HookableMethod),
    Lifecycle(ScopeDecl),
    Apply(ApplyDecl),
    Watchdog(WatchdogDecl),
}
```

In `src/parser.rs`, add a helper that consumes one testbench lifecycle block into a `ComponentItem::Lifecycle(ScopeDecl)` with exactly one populated field. Use duplicate detection by scanning existing `ComponentItem::Lifecycle` entries for the same populated block. Reject `TokenKind::Run` in `ComponentKind::Testbench`, and reject `Setup`/`Check`/`Teardown` in other component kinds.

In `src/pretty.rs`, add a `ComponentItem::Lifecycle(scope)` arm that prints `setup`, `check`, and `teardown` using the same body formatting as `TestItem::Scope`.

- [ ] **Step 4: Verify parser/round-trip tests pass**

Run:

```bash
cargo test --test round_trip testbench_lifecycle -- --nocapture
```

Expected: all four tests pass.

- [ ] **Step 5: Commit parser/pretty support**

```bash
git add src/ast.rs src/parser.rs src/pretty.rs tests/round_trip.rs
git commit -m "feat: parse testbench lifecycle blocks"
```

## Task 2: Codegen Lifecycle Composition

**Files:**
- Modify: `src/codegen/cpp_tb.rs`
- Test: `tests/codegen.rs`

- [ ] **Step 1: Write failing codegen tests**

Add tests to `tests/codegen.rs`:

```rust
#[test]
fn bound_test_inherits_testbench_check_without_local_check() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    check
        log(info, "tb final check")
    end check
end testbench Tb

impl Smoke for Tb
    run
        log(info, "test run")
    end run
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(cpp.contains("tb final check"), "testbench check should be emitted:\n{cpp}");
    let run_pos = cpp.find("test run").unwrap();
    let check_pos = cpp.find("tb final check").unwrap();
    assert!(run_pos < check_pos, "testbench check must run after test run:\n{cpp}");
}

#[test]
fn testbench_and_test_lifecycle_blocks_emit_in_order() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    setup
        log(info, "tb setup")
    end setup
    check
        log(info, "tb check")
    end check
    teardown
        log(info, "tb teardown")
    end teardown
end testbench Tb

impl Smoke for Tb
    setup
        log(info, "test setup")
    end setup
    run
        log(info, "test run")
    end run
    check
        log(info, "test check")
    end check
    teardown
        log(info, "test teardown")
    end teardown
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    let ordered = [
        "tb setup",
        "test setup",
        "test run",
        "tb check",
        "test check",
        "test teardown",
        "tb teardown",
    ];
    let mut last = 0usize;
    for needle in ordered {
        let pos = cpp.find(needle).unwrap_or_else(|| panic!("missing `{needle}` in:\n{cpp}"));
        assert!(pos >= last, "`{needle}` emitted out of order in:\n{cpp}");
        last = pos;
    }
}
```

- [ ] **Step 2: Verify tests fail**

Run:

```bash
cargo test --test codegen testbench_and_test_lifecycle bound_test_inherits -- --nocapture
```

Expected: tests fail because testbench lifecycle blocks are parsed but not emitted.

- [ ] **Step 3: Implement codegen composition**

In `src/codegen/cpp_tb.rs`, when deriving per-test metadata, resolve `test.for_testbench` to the matching `ComponentKind::Testbench` component and collect its `ComponentItem::Lifecycle` blocks. Emit lifecycle blocks in this order inside the run coroutine:

```text
testbench.setup
test.setup
test.run and bare statements
testbench.check
test.check
test.teardown
testbench.teardown
```

Use existing `e.emit_block(block, 2)` so testbench fields and helper functions use the already-established bound-test substitutions.

- [ ] **Step 4: Verify codegen tests pass**

Run:

```bash
cargo test --test codegen testbench_and_test_lifecycle bound_test_inherits -- --nocapture
```

Expected: both tests pass.

- [ ] **Step 5: Commit codegen support**

```bash
git add src/codegen/cpp_tb.rs tests/codegen.rs
git commit -m "feat: inherit testbench lifecycle blocks"
```

## Task 3: Runnable Fixture

**Files:**
- Create: `tests/fixtures/testbench_lifecycle_test.harc`
- Modify: `tests/run_fixtures.sh`

- [ ] **Step 1: Add fixture**

Create `tests/fixtures/testbench_lifecycle_test.harc`:

```harc
testbench CounterLifecycleTb
    dut : Top
    expected : uint<32> default 0

    function reset()
        dut.rst = 1
        dut.en = 0
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
    end function reset

    setup
        reset()
    end setup

    check
        assert dut.count_out == expected
            else fail("shared final check: count_out=${dut.count_out}, expected=${expected}")
        log(info, "shared final check passed")
    end check
end testbench CounterLifecycleTb

impl LifecycleBumpThree for CounterLifecycleTb
    run
        dut.en = 1
        wait 3 cycles
        dut.en = 0
        expected = 3
    end run
end impl LifecycleBumpThree

impl LifecycleBumpFive for CounterLifecycleTb
    run
        dut.en = 1
        wait 5 cycles
        dut.en = 0
        expected = 5
    end run
end impl LifecycleBumpFive
```

- [ ] **Step 2: Add fixture manifest entries**

Add two rows to `tests/run_fixtures.sh`:

```text
testbench_lifecycle_test | Top            | top_counter.sv         | | | LifecycleBumpThree
testbench_lifecycle_test | Top            | top_counter.sv         | | | LifecycleBumpFive
```

- [ ] **Step 3: Verify fixture emits**

Run:

```bash
cargo test --test codegen all_fixtures_emit_cleanly -- --nocapture
```

Expected: fixture sweep passes.

- [ ] **Step 4: Verify fixture runs**

Run:

```bash
cargo run --bin harc -- sim --sv tests/dut/top_counter.sv tests/fixtures/testbench_lifecycle_test.harc --top Top --test LifecycleBumpThree
cargo run --bin harc -- sim --sv tests/dut/top_counter.sv tests/fixtures/testbench_lifecycle_test.harc --top Top --test LifecycleBumpFive
```

Expected: both runs exit 0 and report `ALL TESTS PASSED`.

- [ ] **Step 5: Commit fixture**

```bash
git add tests/fixtures/testbench_lifecycle_test.harc tests/run_fixtures.sh
git commit -m "test: add testbench lifecycle fixture"
```

## Task 4: Final Verification

**Files:**
- No new files.

- [ ] **Step 1: Run focused tests**

```bash
cargo test --test round_trip testbench_lifecycle -- --nocapture
cargo test --test codegen testbench_and_test_lifecycle bound_test_inherits -- --nocapture
cargo test --test codegen all_fixtures_emit_cleanly -- --nocapture
```

- [ ] **Step 2: Run broader Rust tests for touched areas**

```bash
cargo test --test round_trip -- --nocapture
cargo test --test codegen -- --nocapture
```

- [ ] **Step 3: Check git status**

```bash
git status --short --branch
```

Expected: branch contains only intentional commits and no unstaged changes.
