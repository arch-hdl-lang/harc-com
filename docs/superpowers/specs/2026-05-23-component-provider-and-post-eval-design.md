# Component Provider APIs and Post-Eval Timing Design

Date: 2026-05-23

Related issues: [#258](https://github.com/arch-hdl-lang/harc-com/issues/258), [#259](https://github.com/arch-hdl-lang/harc-com/issues/259)

## Summary

HARC should make reusable component-to-component modeling and reactive `post_eval` responder timing a first-class, fixture-backed pattern. The work should land in two phases.

Phase 1 validates and documents the current concrete-component model: component methods may return typed structs, active handlers may call those methods through component fields, and users get precise guidance for one-cycle handshake state machines under the existing `post_eval` schedule.

Phase 2 designs new language support for explicit component references, provider interfaces, and responder timing helpers. These should not be implicit changes to current component field behavior.

## Goals

- Support a responder transactor delegating semantic prediction to a separate model component.
- Support typed struct return values across component method calls in active code.
- Clarify current component field ownership and assignment semantics.
- Document exactly what `phase post_eval` sees and drives relative to the DUT and scheduler.
- Provide executable fixtures for rich provider calls and one-cycle handshake edge cases.
- Keep larger syntax additions separate from near-term stabilization.

## Non-Goals

- Do not silently change existing component fields from by-value storage to references.
- Do not introduce provider interfaces before concrete component calls are stable and tested.
- Do not attempt to redesign the whole scheduler in Phase 1.
- Do not make `post_eval` same-cycle response consumption magically safe without an explicit DUT-observable boundary.

## Current Context

The current C++ testbench lowering already has pieces that should support the desired shape:

- Structs and transactions lower as C++ value records.
- Component and transactor methods lower as free lambdas named `<Type>_<method>`.
- `obj.method(args)` resolves through a field-access chain and lowers to `<Type>_<method>(obj, args)`.
- Transactors lower through the same component method path after synthesizing always-present and active body items.
- `post_eval` services run after primary-clock DUT evaluation and before the run coroutine resumes; when services drive DUT inputs, the backend performs an immediate re-evaluation if any services exist.

The missing pieces are confidence, coverage, and documentation. The issues describe patterns that are plausible in the implementation but not fixture-backed or clearly specified.

## Phase 1: Stabilize Current Patterns

Phase 1 should make the current syntax dependable without introducing new ownership semantics.

### Rich Component Provider Calls

Add fixtures showing a semantic model component with a method returning a typed struct, called from an active responder.

Target source shape:

```harc
struct ReadResponse
    matched : uint<1>
    data : uint<64>
    resp : uint<2>
    last : uint<1>
end struct ReadResponse

transactor ProtocolModel
    cfg0 : uint<64> default 0
    cfg1 : uint<64> default 0

    function predict_read(addr: uint<64>) -> ReadResponse
        let r : ReadResponse
        r.matched = 0
        r.data = 0
        r.resp = 0
        r.last = 1

        if addr == cfg0
            r.matched = 1
            r.data = 0x1234
        elsif addr == cfg1
            r.matched = 1
            r.data = 0x5678
        end if

        return r
    end predict_read
end transactor ProtocolModel
```

The responder fixture should call `model.predict_read(dut.read_addr)` from active `phase post_eval` code and consume the returned fields directly.

If current syntax only accepts `hookable` in transactors, Phase 1 may use `hookable` for the executable fixture and separately document the intended `function` spelling if parser/codegen support exists or is added as part of the fixture work. The important guarantee is typed return-by-value across a component method call.

### Multiple Consumers

Add fixture coverage where two consumers use the same model output:

- A responder asks the model for a response and drives DUT response pins.
- A scoreboard, collector, or checker uses the same model to validate observed behavior.

Under current by-value component fields, this fixture must avoid implying shared mutable state unless the test explicitly wires state into both copies. If shared mutable state is required, Phase 1 should document the limitation and keep the fixture read-only or deterministic from copied configuration.

### Component Field Semantics

Document current component fields as by-value C++ struct fields.

Required clarification:

- `field : ComponentType` stores a sub-component value.
- Assigning one component field or let-binding to another copies the current value.
- Later mutation of the source does not update the destination copy.
- Stateful shared models need an explicit future reference mechanism; they should not rely on assignment behaving like a pointer.

This clarification belongs in `spec.md` near the existing v0 lowering bullets for components and transactors.

### Post-Eval Scheduling Semantics

Document the existing schedule with a value-visibility example:

1. The DUT evaluates the selected clock edge.
2. `post_eval` services observe outputs from that completed edge.
3. `post_eval` services may drive DUT inputs.
4. The backend re-evaluates combinational logic if any services ran.
5. The run coroutine resumes from waits.
6. Checkers and coverage run after the coroutine and low-clock settle path.

The documentation should explicitly warn that a responder must not set and clear a one-cycle response entirely inside one `post_eval` handler based only on same-cycle ready. A response driven in `post_eval(N)` needs a state boundary before the responder considers it DUT-observable and eligible to clear.

### Handshake Fixtures

Add fixtures that lock down safe responder behavior:

- Single request produces one DUT-observable response pulse.
- Back-to-back requests produce distinct responses without duplicate consumption.
- Delayed ready holds the response until the DUT can consume it.
- A regression-style fixture catches the missed-pulse pattern where valid is set and cleared in the same `post_eval` activation.

The fixture should prefer a small purpose-built DUT under `tests/dut` over a large protocol example. The HARC source should be concise and emphasize the scheduling pattern.

## Phase 2: Design Explicit Support

Phase 2 should add new language support only after Phase 1 confirms the current behavior and remaining ergonomic pain points.

### Component References

Introduce explicit reference semantics instead of changing existing fields.

Possible spelling:

```harc
transactor BusResponder
    model : ref ProtocolModel
end transactor BusResponder
```

Design questions to answer before implementation:

- Whether references are nullable or must be assigned before use.
- Whether references can target any component kind or only selected model-like components.
- How lifetimes are validated for testbench-owned components.
- Whether mutation through references is allowed by default.
- How references lower in C++ for nested component paths.

### Provider Interfaces

Consider interface or trait-like provider APIs after concrete component calls are stable.

Possible spelling:

```harc
interface ReadResponseProvider
    function predict_read(addr: uint<64>) -> ReadResponse
end interface ReadResponseProvider

transactor BusResponder
    provider : ref ReadResponseProvider
end transactor BusResponder
```

The first design should favor static resolution over dynamic dispatch unless there is a concrete need for runtime provider replacement. Static resolution keeps generated C++ simple and matches HARC's existing typed component model.

### Responder Timing Helper

Consider a primitive or library helper for “drive this response for exactly one DUT-observable cycle.”

The helper must have a precise schedule contract. It should make clear when the response becomes visible to the DUT and when consumption is sampled. It should not obscure ready/valid protocol semantics or hide duplicate-consumption bugs.

Potential direction:

- A small state-machine helper for `post_eval` responders.
- A scheduling primitive that splits drive and consume observation into named phases.
- A higher-level bus/TLM target responder surface when the protocol is already expressible as `tlm_method`.

## Architecture

Phase 1 should touch only existing architecture:

- Parser/codegen only if a fixture exposes a current bug in typed struct returns, component method calls, or `post_eval` handlers.
- `spec.md` for semantics and recommended patterns.
- `tests/fixtures` and `tests/dut` for executable coverage.
- Existing fixture runner integration.

Phase 2 should start with a separate design document before implementation. It will likely require parser, AST, type resolution, codegen, and documentation changes.

## Error Handling

Phase 1 should improve diagnostics only where the current behavior is ambiguous or dangerous:

- If a method call through a component field cannot resolve, report the component path and target method.
- If a struct return fails to lower, report the source method and return type.
- If docs introduce a recommended pattern, fixtures should fail clearly when violated.

Phase 2 should define new diagnostics for unassigned references, provider mismatch, unsupported mutation, and unresolved interface methods.

## Testing

Phase 1 acceptance criteria:

- Existing fixtures still pass.
- New provider fixture compiles and runs under `tests/run_fixtures.sh`.
- New `post_eval` handshake fixtures compile and run under `tests/run_fixtures.sh`.
- Generated C++ for struct-return provider calls uses normal C++ value semantics.
- Documentation states by-value component field semantics and safe `post_eval` response timing.

Phase 2 acceptance criteria should be defined in its own implementation plan after syntax is approved.

## Rollout

Implement Phase 1 first and close the immediate support/documentation gaps for issues #258 and #259. Treat Phase 2 as follow-up language design. This split gives users reliable current patterns while avoiding accidental ownership or scheduling semantics changes.
