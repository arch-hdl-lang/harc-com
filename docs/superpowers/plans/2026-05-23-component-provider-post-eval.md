# Component Provider Post-Eval Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Phase 1 of the component-provider and `post_eval` timing design executable, documented, and fixture-backed.

**Architecture:** Stabilize existing concrete component calls first: fix value-type method returns and component-field method dispatch, then add a small DUT plus HARC fixture proving provider calls from active `post_eval` responders. Document current by-value component fields and the safe `post_eval` response boundary without adding new reference or interface syntax.

**Tech Stack:** Rust compiler/codegen, HARC fixtures, SystemVerilog DUTs, shell fixture runner, C++ generated testbench via Verilator.

---

## File Structure

- Modify `src/codegen/cpp_tb.rs`: add named value-type return lowering and make component method resolution work for bare component fields inside handler/method bodies.
- Modify `tests/codegen.rs`: add focused red tests for struct-return component methods and component-field method calls inside active `post_eval` handlers.
- Create `tests/dut/post_eval_provider.sv`: small clocked DUT that issues requests, samples response pulses on posedge, tracks accepted responses, and delays ready for one request.
- Create `tests/fixtures/post_eval_provider_test.harc`: end-to-end fixture with a `ProtocolModel`, a `BusResponder`, and a checker/scoreboard consumer using typed `ReadResponse` values.
- Modify `tests/run_fixtures.sh`: add the new fixture row.
- Modify `spec.md`: clarify component field copy semantics, typed struct method returns, and `post_eval` value visibility.

## Task 1: Struct Return Type Lowering

**Files:**
- Modify: `tests/codegen.rs`
- Modify: `src/codegen/cpp_tb.rs`

- [ ] **Step 1: Add a failing codegen test for component method struct returns**

Add this test near existing component/hookable codegen tests in `tests/codegen.rs`:

```rust
#[test]
fn component_method_returns_struct_value() {
    let parsed = parse_source(
        r#"struct ReadResponse
    matched : uint<1>
    data : uint<64>
end struct ReadResponse

transactor ProtocolModel
    function predict_read(addr: uint<64>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.data = addr + 16
        return r
    end predict_read
end transactor ProtocolModel

testbench Tb
    dut : DummyDut
    model : ProtocolModel
end testbench Tb

impl ComponentMethodStructReturnTest for Tb
    run
        let r : ReadResponse = model.predict_read(32)
        assert r.matched != 0 else fail("no match")
        assert r.data == 48 else fail("bad data")
    end run
end impl ComponentMethodStructReturnTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("auto ProtocolModel_predict_read = [&](ProtocolModel& self, uint64_t addr) -> ReadResponse"),
        "expected struct return value in component method; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("-> VReadResponse*"),
        "struct returns must not lower as Verilator module pointers; got:\n{cpp}"
    );
}
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `cargo test --test codegen component_method_returns_struct_value`

Expected: FAIL because the generated method return type contains `VReadResponse*` or the emit fails when assigning a `ReadResponse` return.

- [ ] **Step 3: Add value-type return lowering helper**

In `src/codegen/cpp_tb.rs`, near `c_type_for_param`, add this helper:

```rust
    fn c_type_for_value(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last() {
                let n = &last.name;
                if self.is_record_type(n)
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
```

- [ ] **Step 4: Use the helper for component method returns**

In `emit_component_method`, replace the return type expression:

```rust
        let ret = h
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or_else(|| "void".to_string());
```

with:

```rust
        let ret = h
            .return_ty
            .as_ref()
            .map(|t| self.c_type_for_value(t))
            .unwrap_or_else(|| "void".to_string());
```

- [ ] **Step 5: Run the focused test and verify it passes**

Run: `cargo test --test codegen component_method_returns_struct_value`

Expected: PASS.

- [ ] **Step 6: Commit Task 1**

Run:

```bash
git add tests/codegen.rs src/codegen/cpp_tb.rs
git commit -m "Support struct-valued component method returns"
```

## Task 2: Component Field Method Calls In Handlers

**Files:**
- Modify: `tests/codegen.rs`
- Modify: `src/codegen/cpp_tb.rs`

- [ ] **Step 1: Add a failing codegen test for active `post_eval` component-field calls**

Add this test near `on_phase_post_eval_lowers_to_post_eval_service` in `tests/codegen.rs`:

```rust
#[test]
fn active_post_eval_handler_calls_component_field_method() {
    let parsed = parse_source(
        r#"struct ReadResponse
    matched : uint<1>
    data : uint<32>
end struct ReadResponse

transactor ProtocolModel
    function predict_read(addr: uint<8>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.data = addr + 256
        return r
    end predict_read
end transactor ProtocolModel

transactor BusResponder
    dut : ProviderDut
    model : ProtocolModel

    when active
        on 1 cycles phase post_eval
            if dut.req_valid != 0
                let r : ReadResponse = model.predict_read(dut.req_addr)
                dut.rsp_data = r.data
            end if
        end on
    end when
end transactor BusResponder

testbench Tb
    dut : ProviderDut
    responder : BusResponder active
end testbench Tb

impl ActivePostEvalProviderCallTest for Tb
    run
        responder.dut = dut
        wait 1 cycle
    end run
end impl ActivePostEvalProviderCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("ProtocolModel_predict_read(responder.model, harc_rt::harc_read(dut->req_addr))"),
        "expected component-field provider call to dispatch through generated method; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("model.predict_read"),
        "bare component field calls must not fall through to C++ member calls; got:\n{cpp}"
    );
}
```

- [ ] **Step 2: Run the test and verify it fails**

Run: `cargo test --test codegen active_post_eval_handler_calls_component_field_method`

Expected: FAIL because `model.predict_read(...)` does not resolve through the generated component method dispatcher from inside the handler body.

- [ ] **Step 3: Make method-call resolution substitute field-root instance paths**

In `resolve_component_method_call`, keep the existing type-chain walk but change the returned instance path. After the `found` check and before `Some(...)`, replace:

```rust
        Some((cur_ty, path.join("."), method.name.clone()))
```

with:

```rust
        let instance = if let Some(root_sub) = self.field_subs.get(root) {
            if path.len() == 1 {
                root_sub.clone()
            } else {
                format!("{}.{}", root_sub, path[1..].join("."))
            }
        } else {
            path.join(".")
        };

        Some((cur_ty, instance, method.name.clone()))
```

- [ ] **Step 4: Add field types to `let_types` while emitting component method bodies**

In `emit_component_method`, extend the field-substitution setup. Replace the current setup block:

```rust
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
```

with:

```rust
        let mut subs = std::collections::HashMap::new();
        let mut added_pointer_fields: Vec<String> = Vec::new();
        let mut added_field_types: Vec<String> = Vec::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("self.{}", f.name.name));
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    if self.let_types.insert(f.name.name.clone(), ty.to_string()).is_none() {
                        added_field_types.push(f.name.name.clone());
                    }
                }
                if self.is_dut_pointer_field_type(&f.ty) {
                    if self.pointer_vars.insert(f.name.name.clone()) {
                        added_pointer_fields.push(f.name.name.clone());
                    }
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);
```

Then in the restore block after `self.field_subs = prev_subs;`, add:

```rust
        for k in added_field_types {
            self.let_types.remove(&k);
        }
```

- [ ] **Step 5: Add field types to `let_types` while emitting sync component handler bodies**

In `emit_component_handler_registrations_bound`, where it builds `subs` for handler bodies, add the same `added_field_types` pattern as Step 4. Register `type_simple_name(Some(&f.ty))` for each component field before emitting the handler body, and remove those keys after restoring `field_subs`.

The implementation must keep existing `pointer_vars`, `event_types`, and `bus_bindings` restore behavior unchanged.

- [ ] **Step 6: Add field types to actor handler bodies that can contain provider calls**

Apply the same `added_field_types` pattern in these actor/body emission helpers where `field_subs` is installed:

- `try_emit_bound_driver_actor`
- `emit_bound_monitor_actors`

The registered keys must be removed immediately after each body emission, before restoring caller state.

- [ ] **Step 7: Run the focused test and related existing tests**

Run:

```bash
cargo test --test codegen active_post_eval_handler_calls_component_field_method
cargo test --test codegen passive_instance_calling_when_active_hookable_errors_clearly
cargo test --test codegen hook_triggered_covergroups_resolve_nested_paths
```

Expected: all PASS.

- [ ] **Step 8: Commit Task 2**

Run:

```bash
git add tests/codegen.rs src/codegen/cpp_tb.rs
git commit -m "Resolve component field method calls in handlers"
```

## Task 3: Provider And Post-Eval Fixture

**Files:**
- Create: `tests/dut/post_eval_provider.sv`
- Create: `tests/fixtures/post_eval_provider_test.harc`
- Modify: `tests/run_fixtures.sh`

- [ ] **Step 1: Add the small SystemVerilog DUT**

Create `tests/dut/post_eval_provider.sv` with this complete module:

```systemverilog
module PostEvalProvider(
    input  logic        clk,
    input  logic        rst,
    output logic        req_valid,
    output logic [7:0]  req_addr,
    input  logic        rsp_valid,
    input  logic [31:0] rsp_data,
    output logic        rsp_ready,
    output logic [31:0] accepted_count,
    output logic [31:0] duplicate_count,
    output logic [31:0] last_data,
    output logic        done
);
    logic [31:0] cycle_count;
    logic [31:0] last_accepted_cycle;

    always_ff @(posedge clk) begin
        if (rst) begin
            cycle_count <= 0;
            req_valid <= 0;
            req_addr <= 0;
            rsp_ready <= 0;
            accepted_count <= 0;
            duplicate_count <= 0;
            last_data <= 0;
            last_accepted_cycle <= 32'hffff_ffff;
            done <= 0;
        end else begin
            cycle_count <= cycle_count + 1;
            req_valid <= 0;
            rsp_ready <= 1;

            case (cycle_count)
                1: begin
                    req_valid <= 1;
                    req_addr <= 8'h10;
                end
                2: begin
                    req_valid <= 1;
                    req_addr <= 8'h20;
                end
                3: begin
                    req_valid <= 1;
                    req_addr <= 8'h30;
                    rsp_ready <= 0;
                end
                4: begin
                    rsp_ready <= 0;
                end
                5: begin
                    rsp_ready <= 1;
                end
                8: begin
                    done <= 1;
                end
                default: begin
                end
            endcase

            if (rsp_valid && rsp_ready) begin
                accepted_count <= accepted_count + 1;
                last_data <= rsp_data;
                if (last_accepted_cycle + 1 == cycle_count && rsp_data == last_data) begin
                    duplicate_count <= duplicate_count + 1;
                end
                last_accepted_cycle <= cycle_count;
            end
        end
    end
endmodule
```

- [ ] **Step 2: Add the HARC fixture**

Create `tests/fixtures/post_eval_provider_test.harc` with this complete fixture:

```harc
struct ReadResponse
    matched : uint<1>
    data : uint<32>
    resp : uint<2>
    last : uint<1>
end struct ReadResponse

transactor ProtocolModel
    function predict_read(addr: uint<8>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.resp = 0
        r.last = 1
        r.data = 0

        if addr == 0x10
            r.data = 0x11110010
        elsif addr == 0x20
            r.data = 0x22220020
        elsif addr == 0x30
            r.data = 0x33330030
        else
            r.matched = 0
            r.resp = 1
        end if

        return r
    end predict_read
end transactor ProtocolModel

scoreboard ResponseScoreboard
    seen_count : uint<32> default 0
    last_expected : uint<32> default 0

    function observe(addr: uint<8>, model: ProtocolModel)
        let r : ReadResponse = model.predict_read(addr)
        assert r.matched != 0 else fail("scoreboard saw unexpected addr 0x${addr:02x}")
        seen_count = seen_count + 1
        last_expected = r.data
    end observe
end scoreboard ResponseScoreboard

transactor BusResponder
    dut : PostEvalProvider
    model : ProtocolModel
    sb : ResponseScoreboard
    pending : uint<1> default 0
    visible : uint<1> default 0
    pending_data : uint<32> default 0

    when active
        on 1 cycles phase post_eval
            if pending != 0
                dut.rsp_valid = 1
                dut.rsp_data = pending_data
                visible = 1
            else
                dut.rsp_valid = 0
            end if

            if visible != 0 && pending != 0 && dut.rsp_ready != 0
                pending = 0
                visible = 0
            end if

            if pending == 0 && dut.req_valid != 0
                let r : ReadResponse = model.predict_read(dut.req_addr)
                assert r.matched != 0 else fail("unexpected read addr=0x${dut.req_addr:02x}")
                sb.observe(dut.req_addr, model)
                pending_data = r.data
                pending = 1
                visible = 0
                dut.rsp_valid = 1
                dut.rsp_data = r.data
            end if
        end on
    end when
end transactor BusResponder

testbench PostEvalProviderTb
    dut : PostEvalProvider
    model : ProtocolModel
    sb : ResponseScoreboard
    responder : BusResponder active
end testbench PostEvalProviderTb

impl PostEvalProviderTest for PostEvalProviderTb
    run
        responder.dut = dut
        responder.model = model
        responder.sb = sb

        dut.rst = 1
        dut.rsp_valid = 0
        dut.rsp_data = 0
        wait 2 cycles
        dut.rst = 0

        wait until dut.done == 1 timeout 20 cycles fail("post_eval provider DUT did not finish")

        assert dut.accepted_count == 3
            else fail("accepted ${dut.accepted_count}, expected 3")
        assert dut.duplicate_count == 0
            else fail("duplicate responses ${dut.duplicate_count}, expected 0")
        assert dut.last_data == 0x33330030
            else fail("last data 0x${dut.last_data:08x}, expected 0x33330030")
        assert responder.sb.seen_count == 3
            else fail("scoreboard saw ${responder.sb.seen_count}, expected 3")
        assert responder.sb.last_expected == 0x33330030
            else fail("scoreboard expected 0x${responder.sb.last_expected:08x}, expected 0x33330030")

        log(info, "ALL TESTS PASSED - post_eval_provider_test")
    end run
end impl PostEvalProviderTest
```

- [ ] **Step 3: Run the fixture directly and verify failure if codegen is incomplete**

Run:

```bash
cargo build --release --bin harc
./target/release/harc sim --sv tests/dut/post_eval_provider.sv tests/fixtures/post_eval_provider_test.harc --top PostEvalProvider
```

Expected after Tasks 1 and 2: PASS and output contains `ALL TESTS PASSED`. If it fails, inspect whether the failure is a HARC syntax/codegen issue or a DUT timing expectation mismatch; do not weaken assertions without identifying the cause.

- [ ] **Step 4: Add the fixture row**

In `tests/run_fixtures.sh`, add this row after the existing transactor rows and before the TLM rows:

```sh
post_eval_provider_test | PostEvalProvider | post_eval_provider.sv |
```

- [ ] **Step 5: Run the full fixture runner**

Run: `./tests/run_fixtures.sh`

Expected: every fixture row PASS, and the output includes `PASS  post_eval_provider_test`.

- [ ] **Step 6: Commit Task 3**

Run:

```bash
git add tests/dut/post_eval_provider.sv tests/fixtures/post_eval_provider_test.harc tests/run_fixtures.sh
git commit -m "Add post-eval provider fixture"
```

## Task 4: Documentation Clarification

**Files:**
- Modify: `spec.md`

- [ ] **Step 1: Update component field and method lowering bullets**

In `spec.md`, replace the v0 bullets around component fields and methods with this wording, preserving the existing bound-bus paragraph after the second bullet:

```markdown
- `transactor` / `agent` / `env` / `sequencer` → plain C++ struct of fields. DUT-typed fields lower to Verilator pointers (`V<Name>*`). Sub-component fields lower as by-value C++ structs: `field : ComponentType` stores a distinct component value, and assignment copies the current state. Later mutation of the source component does not update the destination copy. Shared mutable model state requires a future explicit reference/provider feature; it is not implicit in ordinary component fields.
- `hookable name(args) -> T ... end name` and component-local `function name(args) -> T ... end name` on any of the above → free `[&]`-capturing lambda named `<Type>_<method>`. Return values use ordinary C++ value semantics for HARC records, including typed struct returns such as `ReadResponse`. Inside the body, bare references to component fields rewrite to `self.<field>`. `dut.<port>` keeps the arrow-access form.
- `obj.method(args)` and arbitrarily-deep field chains like `env.ag.seq.method(args)` or `responder.model.predict_read(addr)` rewrite to `<Type>_<method>(<self>, args)`. The call-site dispatcher walks the field-access chain from its leaf to the root, resolving the type at each step against the let-binding's declared type or the current component field context, and looks up the method on the leaf type.
```

Keep the existing bound-bus explanation that follows the current hookable bullet immediately after the revised method bullet.

- [ ] **Step 2: Expand the `post_eval` service point documentation**

In `spec.md`, expand the `Post-eval service point` bullet near the current run-coroutine bootstrap text with this schedule:

```markdown
For the direct-Verilator backend, one primary-clock iteration is:

1. The DUT evaluates the selected clock edge.
2. `post_eval` services observe outputs from that completed edge.
3. `post_eval` services may drive DUT inputs.
4. If any `post_eval` services are registered, the backend immediately re-evaluates combinational DUT logic.
5. The run coroutine resumes from waits.
6. The backend performs the clk-low settle path and then checkers/coverage sample.

A response driven in `post_eval(N)` is not a value the DUT sampled at the already-completed edge `N`. A one-cycle responder must keep a state boundary between first driving `valid` and clearing it based on `ready`; setting and clearing a pulse entirely inside one `post_eval` activation can hide the pulse from the DUT.
```

- [ ] **Step 3: Run documentation-adjacent tests**

Run:

```bash
cargo test --test codegen main_loop_runs_post_eval_services_before_coroutine_tick
cargo test --test round_trip on_cycles_and_watchdog_round_trip
```

Expected: both PASS.

- [ ] **Step 4: Commit Task 4**

Run:

```bash
git add spec.md
git commit -m "Document component copy and post-eval timing semantics"
```

## Task 5: Full Verification

**Files:**
- No planned edits.

- [ ] **Step 1: Run the focused codegen tests**

Run:

```bash
cargo test --test codegen component_method_returns_struct_value
cargo test --test codegen active_post_eval_handler_calls_component_field_method
cargo test --test codegen main_loop_runs_post_eval_services_before_coroutine_tick
```

Expected: all selected tests PASS.

- [ ] **Step 2: Run all Rust tests**

Run: `cargo test`

Expected: all Rust tests PASS.

- [ ] **Step 3: Run the new fixture directly**

Run:

```bash
cargo build --release --bin harc
./target/release/harc sim --sv tests/dut/post_eval_provider.sv tests/fixtures/post_eval_provider_test.harc --top PostEvalProvider
```

Expected: command exits 0 and output contains `ALL TESTS PASSED`.

- [ ] **Step 4: Run the fixture runner**

Run: `./tests/run_fixtures.sh`

Expected: all fixture rows PASS, including `post_eval_provider_test`.

- [ ] **Step 5: Inspect final diff and status**

Run:

```bash
git status --short
git diff --stat HEAD~4..HEAD
```

Expected: working tree clean. Diff stat contains only `src/codegen/cpp_tb.rs`, `tests/codegen.rs`, `tests/dut/post_eval_provider.sv`, `tests/fixtures/post_eval_provider_test.harc`, `tests/run_fixtures.sh`, and `spec.md`.
