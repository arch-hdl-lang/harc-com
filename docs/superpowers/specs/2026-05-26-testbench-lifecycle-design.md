# Testbench Lifecycle Blocks

Date: 2026-05-26

## Summary

Add lifecycle blocks to `testbench` declarations so shared DV bench
infrastructure can own setup, final checks, and cleanup that automatically run
for every `impl <Test> for <Tb>`.

The immediate driver is GitHub issue #286: the `pwr_mgr` bench needs an
end-of-test check owned by `testbench PwrMgrTb`, not repeated manually in every
testcase. Current HARC supports per-test `check` blocks and testbench helper
methods, but it has no inherited testbench-level lifecycle hook.

## Goals

- Let a `testbench` declare `setup`, `check`, and `teardown` blocks.
- Automatically compose those blocks into every bound `impl <Test> for <Tb>`.
- Keep stimulus owned by the testcase; do not add `run` to `testbench`.
- Preserve deterministic lifecycle order with no runtime phase registry.
- Keep existing per-test lifecycle blocks working.
- Keep classic unbound `test` behavior unchanged.

## Non-Goals

- No UVM-style distributed phase executor.
- No named phase registration in this change.
- No component-wide inheritance for `agent`, `env`, `scoreboard`, or
  `transactor` lifecycle blocks yet.
- No implicit reset or default end-of-test condition.

## User Surface

```harc
testbench PwrMgrTb
    dut : PwrMgrTop

    function reset()
        dut.rst = 1
        wait 20 cycles
        dut.rst = 0
    end function reset

    function final_check()
        assert_no_checker_errors()
    end function final_check

    setup
        reset()
    end setup

    check
        final_check()
    end check
end testbench PwrMgrTb

impl Smoke for PwrMgrTb
    run
        idle(10)
    end run

    check
        assert dut.done == 1 else fail("smoke did not complete")
    end check
end impl Smoke
```

Generated behavior for `Smoke`:

```text
PwrMgrTb.setup
Smoke.run
PwrMgrTb.check
Smoke.check
```

If the test also declares `setup` or `teardown`, the complete order is:

```text
testbench.setup
test.setup
test.run
testbench.check
test.check
test.teardown
testbench.teardown
```

The testbench `check` runs before test-local `check` so shared infrastructure
can fail early on global drain/accounting issues while still allowing the test
to add scenario-specific assertions. The testbench `teardown` runs last so it
can clean up resources after both shared and test-specific checks.

## Parsing And AST

`testbench` already parses through `ComponentDecl` with
`ComponentKind::Testbench`. Extend the component item model to carry optional
lifecycle blocks for testbench declarations:

- `setup`
- `check`
- `teardown`

Only `ComponentKind::Testbench` accepts these blocks. If a user writes a
lifecycle block inside `agent`, `env`, `scoreboard`, `sequencer`, or
`transactor`, the parser should emit a targeted diagnostic:

```text
`check` blocks are currently supported only inside `test`/`impl` and `testbench`
```

`run` inside `testbench` is rejected with a clear diagnostic:

```text
`run` belongs to a testcase; use `setup`, `check`, or `teardown` in `testbench`
```

Duplicate lifecycle blocks of the same kind in one testbench are errors.

## Codegen

For each `impl <Test> for <Tb>`, codegen already resolves the bound testbench
and folds testbench fields and helper methods into the test body scope. Extend
that path to collect the bound testbench lifecycle blocks and emit them into the
same run coroutine as the test-local lifecycle blocks.

Emission order:

1. Bound testbench `setup`, if present.
2. Test-local `setup`, if present.
3. Test-local `run` plus bare test statements.
4. Bound testbench `check`, if present.
5. Test-local `check`, if present.
6. Test-local `teardown`, if present.
7. Bound testbench `teardown`, if present.

The blocks use the same scope rules as testbench methods and bound tests:

- Bare testbench field names resolve to the bound testbench instance.
- Testbench helper functions are callable without qualification.
- Existing `wait`, `assert`, `log`, coverage, and method-call lowering apply.

No new runtime scheduler or executor registry is introduced. These blocks are
compile-time composition into the existing generated coroutine.

## Diagnostics

Required diagnostics:

- Duplicate `setup`, `check`, or `teardown` in one testbench.
- `run` inside a testbench.
- Testbench lifecycle block in any non-testbench component.
- `impl <Test> for <Name>` where `<Name>` is not a known testbench should
  continue to report the existing unknown/non-testbench binding error, with the
  lifecycle feature relying on that same resolution path.

## Testing

Add parser/round-trip coverage for:

- A testbench with `setup`, `check`, and `teardown`.
- Duplicate lifecycle block rejection.
- `run` in testbench rejection.
- Lifecycle block in non-testbench component rejection.

Add codegen coverage for:

- Bound test inherits `testbench.check` without a testcase-local `check`.
- Testbench and test-local checks both emit in the expected order.
- Testbench `setup` runs before test-local setup/run.
- Testbench `teardown` runs after test-local teardown.
- Testbench lifecycle bodies can call testbench functions and access fields.

Add one runnable fixture based on a small DUT, preferably `top_counter.sv`, with
two `impl`s sharing a testbench-level final check so omission from the testcase
body is proven by generated behavior.

## Future Extension

If more shared phase use cases appear, add named phase registration as a
separate design:

```harc
testbench Tb
    phase eot register check
        final_check()
    end phase eot
end testbench Tb
```

That should wait until lifecycle blocks prove insufficient. The first change is
deliberately smaller: fixed lifecycle names, deterministic ordering, and no
runtime registry.
