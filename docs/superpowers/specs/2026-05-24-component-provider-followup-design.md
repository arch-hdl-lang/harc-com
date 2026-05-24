# Component Provider Follow-Up Design

Date: 2026-05-24

Related issue: [#279](https://github.com/arch-hdl-lang/harc-com/issues/279)

## Summary

The `component-provider-post-eval` branch stabilizes cross-component provider calls from active `post_eval` code, including typed struct returns. Two smaller codegen gaps remain and should be fixed as a focused follow-up:

- Bare same-component helper calls inside component methods should dispatch through the current component object.
- `scoreboard` fields of type `queue<StructType>` should lower to `HarcQueue<StructType>`, not `HarcQueue<uint64_t>`.

Both fixes belong in the C++ testbench backend and should not introduce new language syntax, component reference semantics, provider interfaces, or scheduler changes.

## Goals

- Support a component method calling a sibling method by bare name.
- Apply sibling-call support to `testbench`, `transactor`, `scoreboard`, and other component-like declarations that already emit component methods.
- Preserve user struct types when lowering `scoreboard` `queue<T>` fields.
- Ensure `harc check`, C++ emission, and fixture compile/run behavior agree for typed scoreboard queues.
- Add focused regression tests plus at least one compile/run fixture for the user-facing issue.

## Non-Goals

- Do not add provider interfaces or trait-like dispatch.
- Do not change component fields from by-value storage to references.
- Do not change `post_eval` scheduling.
- Do not redesign method lookup or typechecking beyond the diagnostics needed for this issue.
- Do not change scalar queue representation unless required by existing tests.

## Current Context

The C++ backend already emits component and transactor methods as lambdas named `<Type>_<method>` with a first parameter of `<Type>& self`. Existing `obj.method(args)` calls lower through `resolve_component_method_call()` to `<Type>_<method>(obj, args)`, including nested component-field paths such as `_tb.responder.model.predict_read(...)`.

Inside `emit_component_method()`, component fields are visible by bare name through `field_subs`, so `dut.addr = addr` becomes `self.dut->addr = addr` when `dut` is a DUT pointer field. However, sibling helper calls such as `write_reg(...)` use an `Ident` callee, not a `Field` callee, so they bypass `resolve_component_method_call()` and emit as bare C++ free-function calls.

The scoreboard queue issue has a separate root cause. Normal component fields use `component_field_c_type()`, which calls `payload_type_for_arg()` and knows about user structs and transactions. Scoreboard fields use the free helper `scoreboard_field_c_type()`, which calls `txn_field_c_type()` for queue payloads. That helper does not have access to the backend's record set, so `queue<CheckerError>` can lower as `HarcQueue<uint64_t>` even though `CheckerError` is emitted as a C++ struct.

## Design

### Same-Component Bare Helper Calls

Add a small component-method context to the C++ emitter while emitting each component method body. The context should record the current component type and the expression to pass as the receiver, which is `self` for component method bodies.

During expression emission for `Call { callee: Ident(name), args }`, check this context before falling through to generic free-function emission. If the current component type declares a method named `name`, lower the call to:

```cpp
<CurrentType>_<name>(self, <args...>)
```

This must use the same generated helper path as explicit receiver calls. A bare call is rewritten only when the target method exists on the current component/transactor. Otherwise it remains eligible for normal free-function or builtin lowering.

The method existence check should work for the same component-like declarations already covered by `emit_component_method()`: testbenches, agents, envs, sequencers, scoreboards, and synthesized transactor component views. For transactors, the check should use the synthesized component view that includes always-present methods and, when appropriate, active methods.

The emitter must restore the component-method context after each method body so nested emissions do not leak state into unrelated code.

### Unresolved Bare Calls

When a bare call appears inside a component method and cannot be resolved as a sibling method, it should not immediately become a hard error if it is a valid top-level function, intrinsic, or existing backend special form. The near-term diagnostic should target the concrete failure mode: if the backend can identify that a bare name matches no known callable and emits it unchanged from a component method, report a HARC codegen diagnostic that names the component and method body.

This keeps compatibility for legitimate top-level helper functions while preventing known component-helper typos from surfacing only as C++ compiler errors.

### Scoreboard `queue<struct>` Lowering

Replace `scoreboard_field_c_type()` with a record-aware backend method, or pass record awareness into it. Queue payload lowering should reuse `payload_type_for_arg()` so it preserves:

- User `struct` types, e.g. `queue<CheckerError>` -> `HarcQueue<CheckerError>`.
- User `transaction` types.
- User enum types.
- Existing scalar behavior for integer and bool queue payloads.

The helper should handle both parser shapes used elsewhere in the backend:

- `TypeArg::Type(TypeExpr::Named { name: CheckerError, ... })`
- `TypeArg::Expr(Ident(CheckerError))`

No runtime queue change is expected. `HarcQueue<T>` is already templated and exposes `push`, `pop`, `empty`, and `size`.

## Testing

Add codegen tests for same-component sibling calls:

- `testbench` method `program_defaults()` calls `write_reg(...)` by bare name and emits `Tb_write_reg(self, ...)`.
- `transactor` method calls a sibling method by bare name and emits `<Transactor>_<method>(self, ...)`.
- `scoreboard` method calls a sibling method by bare name and emits `<Scoreboard>_<method>(self, ...)`.
- Tests assert the bare C++ call is absent.

Add codegen tests for scoreboard typed queues:

- A `scoreboard` with `errors : queue<CheckerError>` emits `HarcQueue<CheckerError> errors;`.
- A method creating a `CheckerError` and calling `errors.push(err)` emits a type-consistent push.

Add an executable fixture:

- A small DUT exposes a value that an active `post_eval` checker can compare.
- A `GlobalScoreboard` owns `errors : queue<CheckerError>` and a `record_error(...)` method that pushes a typed record.
- A checker transactor calls `sb.record_error(...)` from active `post_eval`.
- The test runs through `tests/run_fixtures.sh` and compiles generated C++.

## Error Handling

The same-component call fix should prefer exact sibling-method resolution over broad name rewriting. Ambiguous or unsupported call forms should remain diagnostics rather than generating invalid C++.

The scoreboard queue fix should make codegen agree with frontend acceptance. If a queue payload type is unresolved, the backend may keep the current scalar fallback, but fixture coverage must ensure declared record payloads do not use that fallback.

## Acceptance Criteria

- Bare sibling method calls inside component methods lower through `self`.
- Existing explicit component-provider calls continue to lower through `<Provider>_<method>(instance, ...)`.
- `scoreboard` `queue<struct>` fields lower as `HarcQueue<StructType>`.
- Typed record queue methods compile when pushing and popping records.
- A `post_eval` checker can record typed errors into a scoreboard queue in a compile/run fixture.
- Existing codegen tests and fixture tests continue to pass.
