# Component Provider Follow-Up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fully support issue #279 by fixing same-component helper dispatch and typed `scoreboard` `queue<struct>` lowering.

**Architecture:** Keep the fix inside the existing C++ testbench backend. Add a small component-method context for bare sibling calls, and make scoreboard field lowering reuse the record-aware queue payload resolver already used by normal component fields.

**Tech Stack:** Rust compiler/codegen, HARC parser/codegen tests, HARC fixtures, SystemVerilog DUTs, generated C++ testbenches via Verilator.

---

## File Structure

- Modify `src/codegen/cpp_tb.rs`: add current component method context, bare sibling-call resolution, and record-aware scoreboard queue field lowering.
- Modify `tests/codegen.rs`: add focused codegen tests for sibling calls in `testbench`, `transactor`, and `scoreboard`, plus `scoreboard` `queue<struct>` lowering.
- Create `tests/dut/scoreboard_typed_queue.sv`: tiny DUT that exposes a mismatching value for the executable typed-queue fixture.
- Create `tests/fixtures/scoreboard_typed_queue_test.harc`: fixture with `CheckerError`, `GlobalScoreboard.errors : queue<CheckerError>`, and an active `post_eval` checker pushing typed records.
- Modify `tests/run_fixtures.sh`: add the fixture row.
- Keep `docs/superpowers/specs/2026-05-24-component-provider-followup-design.md`: design reference only; no further doc change is required unless implementation reveals a behavior change.

## Task 1: Bare Sibling Calls in Component Methods

**Files:**
- Modify: `tests/codegen.rs`
- Modify: `src/codegen/cpp_tb.rs`

- [ ] **Step 1: Add failing codegen tests**

Append these tests near the existing component provider tests in `tests/codegen.rs`:

```rust
#[test]
fn testbench_method_bare_sibling_call_dispatches_through_self() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut

    function write_reg(addr: uint<32>, data: uint<32>)
        dut.addr = addr
        dut.wdata = data
    end write_reg

    function program_defaults()
        write_reg(0x1000, 0)
        write_reg(0x1004, 1)
    end program_defaults
end testbench Tb

impl BareSiblingTestbenchCallTest for Tb
    run
        program_defaults()
    end run
end impl BareSiblingTestbenchCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("Tb_write_reg(self, 0x1000, 0)")
            && cpp.contains("Tb_write_reg(self, 0x1004, 1)"),
        "testbench sibling calls should dispatch through self; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("write_reg(0x1000, 0)") && !cpp.contains("write_reg(0x1004, 1)"),
        "testbench sibling calls must not emit as bare C++ calls; got:\n{cpp}"
    );
}

#[test]
fn transactor_method_bare_sibling_call_dispatches_through_self() {
    let parsed = parse_source(
        r#"transactor HelperTransactor
    function write_value(data: uint<32>)
        last = data
    end write_value

    function program_defaults()
        write_value(7)
    end program_defaults

    last : uint<32> default 0
end transactor HelperTransactor

testbench Tb
    dut : DummyDut
    helper : HelperTransactor active
end testbench Tb

impl BareSiblingTransactorCallTest for Tb
    run
        helper.program_defaults()
    end run
end impl BareSiblingTransactorCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("HelperTransactor_write_value(self, 7)"),
        "transactor sibling call should dispatch through self; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("write_value(7)"),
        "transactor sibling call must not emit as a bare C++ call; got:\n{cpp}"
    );
}

#[test]
fn scoreboard_method_bare_sibling_call_dispatches_through_self() {
    let parsed = parse_source(
        r#"scoreboard Score
    count : uint<32> default 0

    function bump()
        count = count + 1
    end bump

    function observe()
        bump()
    end observe
end scoreboard Score

testbench Tb
    dut : DummyDut
    sb : Score
end testbench Tb

impl BareSiblingScoreboardCallTest for Tb
    run
        sb.observe()
    end run
end impl BareSiblingScoreboardCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("Score_bump(self)"),
        "scoreboard sibling call should dispatch through self; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("bump()"),
        "scoreboard sibling call must not emit as a bare C++ call; got:\n{cpp}"
    );
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```bash
cargo test --test codegen bare_sibling
```

Expected: FAIL. The generated C++ still contains bare calls such as `write_reg(0x1000, 0)` or `bump()`.

- [ ] **Step 3: Add current component method context to the emitter**

In `src/codegen/cpp_tb.rs`, add this field to `struct Emitter` near `current_component_instance`:

```rust
    /// While emitting a component method body, this records the HARC
    /// component type and C++ receiver expression used for bare sibling
    /// method calls such as `helper()` -> `Type_helper(self)`.
    current_component_method: Option<(String, String)>,
```

In the emitter initializer near `current_component_instance: None,`, add:

```rust
        current_component_method: None,
```

- [ ] **Step 4: Add a sibling-method resolver**

In the `impl Emitter` block near `resolve_component_method_call`, add:

```rust
    fn component_declares_method(&self, ty: &str, method: &str) -> bool {
        let has_method = |items: &[ComponentItem]| -> bool {
            items.iter().any(|it| {
                matches!(
                    it,
                    ComponentItem::Hookable(h) if h.name.name == method
                )
            })
        };
        if let Some(comp) = self.components.get(ty) {
            return has_method(&comp.items);
        }
        if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            return has_method(&synth.items);
        }
        false
    }

    fn resolve_bare_sibling_method_call(&self, callee: &Expr) -> Option<(String, String, String)> {
        let ExprKind::Ident(id) = &*callee.kind else {
            return None;
        };
        let (comp_ty, receiver) = self.current_component_method.as_ref()?;
        if self.component_declares_method(comp_ty, &id.name) {
            return Some((comp_ty.clone(), receiver.clone(), id.name.clone()));
        }
        None
    }
```

- [ ] **Step 5: Set and restore context around method body emission**

In `emit_component_method`, immediately before `self.emit_block(&h.body, depth + 1);`, add:

```rust
        let prior_component_method = std::mem::replace(
            &mut self.current_component_method,
            Some((comp_ty.clone(), "self".to_string())),
        );
```

Immediately after `self.emit_block(&h.body, depth + 1);`, add:

```rust
        self.current_component_method = prior_component_method;
```

- [ ] **Step 6: Emit bare sibling calls through the generated helper**

In `emit_expr_with_arrow`, inside the `ExprKind::Call { callee, args }` branch, after the `len()` special case and before quiescence/idleness/component-field dispatch, add:

```rust
                if let Some((comp_ty, receiver, method)) =
                    self.resolve_bare_sibling_method_call(callee)
                {
                    write!(self.out, "{comp_ty}_{method}({receiver}").ok();
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
```

- [ ] **Step 7: Run focused tests and verify pass**

Run:

```bash
cargo test --test codegen bare_sibling
```

Expected: PASS. The generated C++ includes `Tb_write_reg(self, ...)`, `HelperTransactor_write_value(self, ...)`, and `Score_bump(self)`.

## Task 2: Record-Aware Scoreboard Queue Lowering

**Files:**
- Modify: `tests/codegen.rs`
- Modify: `src/codegen/cpp_tb.rs`

- [ ] **Step 1: Add failing codegen tests**

Append these tests near other scoreboard codegen tests in `tests/codegen.rs`:

```rust
#[test]
fn scoreboard_queue_of_struct_lowers_to_typed_harc_queue() {
    let parsed = parse_source(
        r#"struct CheckerError
    checker_id : uint<8>
    code : uint<16>
    got : uint<64>
    expected : uint<64>
end struct CheckerError

scoreboard GlobalScoreboard
    errors : queue<CheckerError>
end scoreboard GlobalScoreboard

testbench Tb
    dut : DummyDut
    sb : GlobalScoreboard
end testbench Tb

impl TypedScoreboardQueueLoweringTest for Tb
    run
        assert sb.errors.empty() else fail("expected empty")
    end run
end impl TypedScoreboardQueueLoweringTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("HarcQueue<CheckerError> errors;"),
        "scoreboard queue<struct> should preserve record type; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("HarcQueue<uint64_t> errors;"),
        "scoreboard queue<struct> must not fall back to uint64_t; got:\n{cpp}"
    );
}

#[test]
fn scoreboard_method_pushes_struct_into_typed_queue() {
    let parsed = parse_source(
        r#"struct CheckerError
    checker_id : uint<8>
    code : uint<16>
    got : uint<64>
    expected : uint<64>
end struct CheckerError

scoreboard GlobalScoreboard
    errors : queue<CheckerError>

    function record_error(checker_id: uint<8>, code: uint<16>, got: uint<64>, expected: uint<64>)
        let err : CheckerError
        err.checker_id = checker_id
        err.code = code
        err.got = got
        err.expected = expected
        errors.push(err)
    end record_error
end scoreboard GlobalScoreboard

testbench Tb
    dut : DummyDut
    sb : GlobalScoreboard
end testbench Tb

impl TypedScoreboardQueuePushTest for Tb
    run
        sb.record_error(1, 0x1001, 2, 3)
    end run
end impl TypedScoreboardQueuePushTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("HarcQueue<CheckerError> errors;"),
        "scoreboard queue should preserve CheckerError element type; got:\n{cpp}"
    );
    assert!(
        cpp.contains("self.errors.push(err);"),
        "scoreboard method should push the typed record into the queue; got:\n{cpp}"
    );
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```bash
cargo test --test codegen scoreboard_queue_of_struct_lowers_to_typed_harc_queue scoreboard_method_pushes_struct_into_typed_queue
```

Expected: FAIL. The first test should show `HarcQueue<uint64_t> errors;` for `queue<CheckerError>`.

- [ ] **Step 3: Make scoreboard field lowering record-aware**

In `emit_scoreboard`, replace:

```rust
                let cty = scoreboard_field_c_type(&f.ty);
```

with:

```rust
                let cty = self.scoreboard_field_c_type(&f.ty);
```

In `impl Emitter`, near `component_field_c_type`, add:

```rust
    fn scoreboard_field_c_type(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Builtin {
                name: BuiltinTy::Queue,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("HarcQueue<{inner}>")
            }
            _ => self.record_field_c_type(t),
        }
    }
```

Remove the old free function `fn scoreboard_field_c_type(t: &TypeExpr) -> String` near the bottom of the file.

- [ ] **Step 4: Run focused tests and verify pass**

Run:

```bash
cargo test --test codegen scoreboard_queue_of_struct_lowers_to_typed_harc_queue scoreboard_method_pushes_struct_into_typed_queue
```

Expected: PASS. `queue<CheckerError>` emits `HarcQueue<CheckerError>` and `errors.push(err)` remains type-consistent.

## Task 3: Compile/Run Fixture for Typed Scoreboard Queues from Post-Eval

**Files:**
- Create: `tests/dut/scoreboard_typed_queue.sv`
- Create: `tests/fixtures/scoreboard_typed_queue_test.harc`
- Modify: `tests/run_fixtures.sh`

- [ ] **Step 1: Add the DUT**

Create `tests/dut/scoreboard_typed_queue.sv`:

```systemverilog
module ScoreboardTypedQueue(
    input  logic        clk,
    input  logic        rst_n,
    output logic [63:0] dut_value,
    output logic [63:0] expected_value
);
    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            dut_value <= 64'd0;
            expected_value <= 64'd0;
        end else begin
            dut_value <= 64'd7;
            expected_value <= 64'd9;
        end
    end
endmodule
```

- [ ] **Step 2: Add the HARC fixture**

Create `tests/fixtures/scoreboard_typed_queue_test.harc`:

```harc
struct CheckerError
    checker_id : uint<8>
    code : uint<16>
    got : uint<64>
    expected : uint<64>
end struct CheckerError

scoreboard GlobalScoreboard
    errors : queue<CheckerError>

    function record_error(checker_id: uint<8>, code: uint<16>, got: uint<64>, expected: uint<64>)
        let err : CheckerError
        err.checker_id = checker_id
        err.code = code
        err.got = got
        err.expected = expected
        errors.push(err)
    end record_error
end scoreboard GlobalScoreboard

transactor Checker
    dut : ScoreboardTypedQueue
    sb : GlobalScoreboard

    when active
        on 1 cycles phase post_eval
            if dut.dut_value != dut.expected_value
                sb.record_error(1, 0x1001, dut.dut_value, dut.expected_value)
            end if
        end on
    end when
end transactor Checker

testbench Tb
    dut : ScoreboardTypedQueue
    sb : GlobalScoreboard
    checker : Checker active
end testbench Tb

impl ScoreboardTypedQueueTest for Tb
    run
        checker.dut = dut
        checker.sb = sb
        wait 2 cycles
        assert checker.sb.errors.size() != 0 else fail("expected typed scoreboard error")
        let err : CheckerError = checker.sb.errors.pop()
        assert err.checker_id == 1 else fail("bad checker id")
        assert err.code == 0x1001 else fail("bad error code")
        assert err.got == 7 else fail("bad got value")
        assert err.expected == 9 else fail("bad expected value")
    end run
end impl ScoreboardTypedQueueTest
```

- [ ] **Step 3: Register the fixture**

In `tests/run_fixtures.sh`, add a row near `post_eval_provider_test`:

```text
scoreboard_typed_queue_test | ScoreboardTypedQueue | scoreboard_typed_queue.sv |
```

- [ ] **Step 4: Run the new fixture and verify pass**

Run:

```bash
cargo build --release --bin harc
HARC=./target/release/harc ./tests/run_fixtures.sh
```

Expected: PASS for `scoreboard_typed_queue_test`, with the final summary showing zero failed fixtures.

## Task 4: Final Verification

**Files:**
- No new files.

- [ ] **Step 1: Run targeted codegen tests**

Run:

```bash
cargo test --test codegen bare_sibling
cargo test --test codegen scoreboard_queue_of_struct_lowers_to_typed_harc_queue scoreboard_method_pushes_struct_into_typed_queue
```

Expected: all targeted tests PASS.

- [ ] **Step 2: Run broader codegen tests**

Run:

```bash
cargo test --test codegen
```

Expected: all codegen tests PASS.

- [ ] **Step 3: Run fixture sweep**

Run:

```bash
HARC=./target/release/harc ./tests/run_fixtures.sh
```

Expected: all fixtures PASS, including `scoreboard_typed_queue_test`.

- [ ] **Step 4: Inspect changed files**

Run:

```bash
git status --short
git diff -- src/codegen/cpp_tb.rs tests/codegen.rs tests/dut/scoreboard_typed_queue.sv tests/fixtures/scoreboard_typed_queue_test.harc tests/run_fixtures.sh docs/superpowers/specs/2026-05-24-component-provider-followup-design.md docs/superpowers/plans/2026-05-24-component-provider-followup.md
```

Expected: diff contains only the issue #279 implementation, tests, fixture, design doc, and this plan.

## Self-Review Notes

- Spec coverage: Task 1 covers bare same-component helper calls. Task 2 covers `scoreboard` `queue<struct>` lowering. Task 3 covers active `post_eval` typed scoreboard recording and compile/run coverage. Task 4 covers verification.
- Completeness scan: no deferred implementation steps remain.
- Type consistency: `CheckerError`, `GlobalScoreboard`, `Checker`, `ScoreboardTypedQueue`, and method names match across tests, fixture, and runner entry.
- Commit handling: this plan intentionally omits commit steps because this environment should not create commits unless the user explicitly asks.
