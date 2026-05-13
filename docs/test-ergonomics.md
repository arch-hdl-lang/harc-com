# Design doc: Test ergonomics — `testbench` + inline `run`

**Status:** Proposed (RFC, not yet implemented).
**Date logged:** 2026-05-13.
**Scope:** Two related changes to remove ceremony from HARC test files:
(1) a `testbench` block that owns the DUT, bus binding, transactor/scoreboard
composition, and shared setup methods — so multiple tests reuse one
structural skeleton; (2) inline `run` inside `test`, replacing the
two-block `test T { ... } / impl sim for T { run ... }` form.

## 1. Motivation

### 1.1 Current duplication

Every test today repeats the same structural boilerplate. From
`tests/fixtures/axilite_env_test.harc`:

```harc
test AxiLiteEnvTest
    let dut : AxiLiteRegs
    let env : AxilEnv
end test AxiLiteEnvTest

impl sim for AxiLiteEnvTest
    run
        log(info, "AxiLiteRegs env-composite test")
        env.drv.dut = dut             // wire DUT into transactor

        dut.rst = 1                   // 20+ lines of reset/init
        dut.axil_aw_addr = 0
        dut.axil_aw_valid = 0
        dut.axil_w_data = 0
        dut.axil_w_strb = 15
        dut.axil_w_valid = 0
        dut.axil_b_ready = 1
        // ... 15 more init lines ...
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle

        // ... actual test stimulus ...
    end run
end impl AxiLiteEnvTest
```

Two distinct sources of repetition:

1. **Structural** — every additional test for the same DUT copies the
   `let dut : AxiLiteRegs`, `let env : AxilEnv`, and DUT-to-env wiring.
2. **Procedural** — the 20-line reset/init sequence is copy-pasted into
   every test's `run` block.

A 12th axilite test today costs ~30 lines of pure ceremony before any
test-specific logic. Across the 11 axilite fixtures, this is ~250 lines
of redundant structure.

### 1.2 Ceremony from the `test` / `impl sim for` split

Spec §7.2 defends the two-block form on the grounds that one `test` may
have multiple impls:

> The same `test SimpleTest` declaration can be implemented for both —
> `impl sim for SimpleTest` and `impl emu for SimpleTest` coexist as
> orthogonal top-level items, possibly in separate files.

Survey of the actual fixture corpus (`tests/fixtures/*.harc`, 76 files):

- 76 fixtures total.
- 1 fixture contains more than one `impl sim for` (`axi_agent.harc`,
  which holds two *different* tests — not a multi-impl-per-test case).
- **Zero fixtures use the multi-impl feature.**

The `emu` backend ships post-v0. When it lands, the design constraint
isn't "every sim test must have a matching emu impl under the same name"
— a sim-only test and an emu-only test can simply have different names.
The cost of the two-block form is paid universally; the benefit is paid
to a feature that isn't used and won't need to be used.

## 2. Goals and non-goals

**Goals:**

1. One shared structural declaration (`testbench`) that multiple tests
   reuse for DUT + bus + verification components + setup helpers.
2. Inline `run` inside `test`, eliminating the redundant `impl sim for T`
   block.
3. Clear backend selection that doesn't require per-test annotations
   when sim is the default.
4. Mechanical migration path for the existing 76 fixtures.
5. Net source-line reduction in every fixture; no fixture grows.

**Non-goals:**

1. Test inheritance. Spec §7.2's "each test stands alone" property is
   preserved — tests share *declarations* (testbench), not *behavior*.
2. Runtime testbench discovery or string-keyed lookup. Composition is
   compile-time.
3. Multi-impl-per-test machinery. Dropped entirely; replaced by
   different tests with different names targeting different backends.
4. Implicit reset / auto-setup. Reset stays explicit at the top of every
   `run` (`tb.reset()`), consistent with HARC's "no UVM phase machinery"
   stance.

## 3. Surface — `testbench`

```harc
testbench AxiLiteTb
    // Component composition — declared once, instantiated per test.
    dut : AxiLiteRegs
    bus : BusAxiLite = bind dut
    drv : AxilXactor passive = bind bus
    sb  : AxilSb

    // Shared procedural helpers — callable from any test's run.
    fn reset()
        dut.rst = 1
        dut.axil_aw_valid = 0
        dut.axil_w_valid = 0
        dut.axil_b_ready = 1
        dut.axil_ar_valid = 0
        dut.axil_r_ready = 1
        // ... (full reset/init for AxiLiteRegs)
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
    end fn

    fn drive_random(n: int)
        let txns = RandomTxns(n)
        for t in txns
            sb.expected.push(t.value)
            drv.axil_write(t.addr, t.value)
            let got = drv.axil_read(t.addr)
            if got != sb.expected.pop()
                sb.errors = sb.errors + 1
            end if
        end for
    end fn
end testbench AxiLiteTb
```

### 3.1 Field semantics

- A `testbench` field may be:
  - A DUT module (`dut : AxiLiteRegs`).
  - A bus binding (`bus : BusAxiLite = bind dut`) — `bind` resolves
    against the testbench's own fields.
  - A transactor / scoreboard / env / agent — all existing component
    types from spec §8.
  - A primitive (counter, flag, etc.) — same as current `env` body.
- Field order matters for `bind` resolution (forward refs not allowed),
  matching the existing `env` rule.

### 3.2 Function semantics

- `fn name(...) [-> T]` declares a method on the testbench instance.
- Inside the body, bare field names resolve to `self.<field>` (consistent
  with how `transactor` already resolves `bus`).
- Functions are **synchronous between waits**, same coroutine model as
  the test's `run` block. They may call `wait`, `for`, `if`, and other
  procedural constructs.
- Functions are **not hookable** by default. If a testbench function
  needs the pre/post hook machinery, it's declared `hookable fn` (same
  keyword shape as transactor methods).

### 3.3 Why a new keyword (`testbench`) instead of extending `env`?

The `testbench` keyword is already reserved in spec §2 (shared with
ARCH) and is the lowering target for `test`. Promoting it from "lowering
target" to "user-facing TB-owning container" matches the ARCH mental
model.

The split between `env` and `testbench` becomes meaningful:

| Construct | Owns DUT? | Reusable across DUTs? | Has setup methods? |
|---|---|---|---|
| `env` | No | Yes (same env type works against any DUT that exposes the right bus) | No — pure composition |
| `testbench` | Yes | No (bound to a specific DUT type) | Yes (`fn reset`, `fn preload`, etc.) |

A `testbench` typically *contains* an `env`. The env stays DUT-agnostic
and reusable across testbenches; the testbench is the DUT-specific
binding point.

## 4. Surface — inline `run` inside `test`

Drop `impl sim for T` entirely. The `run` block (and the existing
optional `setup` / `check` / `teardown` phase blocks from spec §7.2)
lives directly inside `test`:

```harc
test AxiLiteSmokeTest
    let tb : AxiLiteTb

    run
        tb.reset()
        tb.drv.axil_write(0x00, 0x1)
        assert (tb.drv.axil_read(0x00) & 1) == 1
    end run
end test

test AxiLiteFuzzTest
    let tb : AxiLiteTb

    run
        tb.reset()
        tb.drive_random(20)
        assert tb.sb.errors == 0
    end run
end test
```

A test that needs the optional phases:

```harc
test AxiLiteCheckedTest
    let tb : AxiLiteTb

    setup
        tb.reset()
    end setup

    run
        tb.drive_random(20)
    end run

    check
        assert tb.sb.errors == 0
    end check

    teardown
        log(info, "errors=${tb.sb.errors}")
    end teardown
end test
```

### 4.1 Equivalence to the old form

`test T { fields; run; }` is exactly equivalent to the old
`test T { fields } end test` + `impl sim for T { run } end impl`.
The compiler synthesizes the same AST nodes downstream of the parser;
codegen is unchanged.

### 4.2 No compatibility with the old form

`impl sim for T` is **removed**, not deprecated. Reasons:

1. The fixture corpus shows zero uses of multi-impl-per-test, so the
   migration cost is mechanical (a regex rewrite over 76 files).
2. Keeping both forms means readers must know two equivalent syntaxes,
   which is exactly the "two ways to do one thing" pattern HARC's spec
   §1 design principles explicitly reject.
3. The `impl X for T` machinery in the parser, AST, and codegen can be
   deleted, reducing implementation surface.

## 5. Backend selection

With `impl sim for T` gone, backend choice moves to the CLI. Tests carry
no backend annotation in the source.

```sh
harc sim my_file.harc --top Foo      # sim backend (today's default)
harc emu my_file.harc --top Foo      # emu backend (post-v0)
```

This matches how `harc sim --sv` / `harc sim --dut` already select
DUT-side flavor at the CLI rather than in the source. The future `emu`
story is its own RFC; this doc only commits to *not needing per-test
backend annotations in the source*.

## 6. Lowering

### 6.1 `testbench`

A `testbench` declaration lowers to a C++ struct whose fields are the
component composition, plus methods for each `fn`:

```cpp
struct AxiLiteTb {
    AxiLiteRegs   dut;
    BusAxiLite    bus;          // holds binding metadata, not actual storage
    AxilXactor    drv;
    AxilSb        sb;

    HarcCoro<void> reset() {
        dut.rst = 1;
        // ...
        co_await wait_cycles(2);
        dut.rst = 0;
        co_await wait_cycles(1);
    }

    HarcCoro<void> drive_random(int n) { /* ... */ }
};
```

Bus bindings and the DUT-to-component wiring are resolved at codegen
time using field-relative `bind` resolution, the same machinery already
used for `env` composition.

### 6.2 `test` with inline `run`

A `test` block lowers to a sibling struct holding the testbench instance
plus the `run` coroutine, identical to today's `test` + `impl sim for`
shape — only the parser entry-point changes.

```cpp
struct AxiLiteSmokeTest {
    AxiLiteTb tb;

    HarcCoro<void> run() {
        co_await tb.reset();
        co_await tb.drv.axil_write(0x00, 0x1);
        // assert (tb.drv.axil_read(0x00) & 1) == 1
    }
};
```

`setup`, `check`, `teardown` lower to additional coroutine methods on
the same struct, called by the runtime in fixed order per spec §7.2.

## 7. Migration

### 7.1 Scope

- 76 fixture files in `tests/fixtures/`.
- All convert mechanically: the `impl sim for T { ... }` block's body
  moves into the `test T { ... }` block, and the `impl` wrapper is
  deleted.
- Files with multiple `test`s (e.g. `axi_agent.harc`) get each test
  converted independently; no structural change beyond the per-test
  collapse.
- Files using `env` composition keep the env decl unchanged; only the
  test blocks shrink.
- Fixtures with copy-pasted reset sequences (the axilite cluster,
  notably) can additionally be refactored to a shared `testbench` —
  but that's an opportunistic follow-up, not part of the syntax-level
  migration.

### 7.2 Tooling

`harc fmt --migrate-v1` flag (one-time tool): regex over `.harc` files,
collapse the two-block form into the one-block form. Confidence is
high because:

- `impl sim for T { ... }` always has exactly one body (`run`, plus
  optional `setup`/`check`/`teardown` phases) that can move verbatim.
- No fixture uses multi-impl; nothing to merge.
- The `end impl` keyword is unambiguous to delete.

A pre-PR dry-run against all fixtures verifies the migration produces
files that parse and pass `tests/run_fixtures.sh`.

### 7.3 Spec edits

- §7.2: drop the multi-impl rationale. Replace with the inline-`run`
  shape. Strike "There is no inheritance / no `super` chain — each
  impl stands alone" and replace with "Each test stands alone — no
  inheritance, no shared mutable state across tests."
- §14 (Phasing): drop references to `impl X for T` from the phase
  descriptions; the phase blocks live inside `test` directly.
- §16 (ARCH Lowering Map): update the "Tests lower to ARCH `testbench`"
  entry to note the user-facing `testbench` keyword (this RFC) maps
  more directly to the ARCH primitive than the v0 `test` block did.

## 8. Open questions

### 8.1 `testbench` instantiation in `test`

```harc
test AxiLiteSmokeTest
    let tb : AxiLiteTb
    // ...
end test
```

The `let tb : AxiLiteTb` line is unavoidable boilerplate when every
test against the same DUT uses the same testbench. Two options:

- **Status quo (recommended):** explicit `let tb : AxiLiteTb`.
  Reader sees the testbench binding at the top of every test.
- **Sugar:** `test AxiLiteSmokeTest uses AxiLiteTb { ... }` —
  implicit `tb` field of type `AxiLiteTb`. Saves one line per test.

The savings are small (1 line vs the ~25 lines this RFC already
removes). Recommend deferring to a follow-up RFC if users find the
boilerplate annoying after the bulk of the migration lands. Don't
add the sugar speculatively.

### 8.2 Multiple testbenches in one test

Could a `test` declare two testbench fields (e.g. one wrapping a
producer DUT, one wrapping a consumer DUT, both driven from the same
`run`)? Yes — falls out of `let` composition. No special syntax needed.
Worth documenting in §7.2 once the spec edit lands.

### 8.3 `testbench` parameterization

Should `testbench` accept parameters (depth, width, address-base)?
Today `transactor` and `env` don't have parameter syntax; same gap.
Defer until `mem` / `regblock` work (RAL RFC) forces parameterization,
then introduce a uniform parameter mechanism across `testbench` /
`transactor` / `env`.

### 8.4 Phase blocks order

Spec §7.2 currently allows `setup` / `run` / `check` / `teardown` in
any order in the source; runtime always invokes them in that fixed
sequence. With phase blocks inside `test`, should the parser enforce
source-order = invocation-order for readability?

Recommended: no enforcement. The runtime order is documented; the
source author can group phases by relevance to whatever they're
emphasizing in that test. Same flexibility as today's `impl sim for T`.

## 9. Trade-offs accepted

- **No multi-impl per test.** Spec §7.2's stated multi-impl design
  goal is dropped. Justified by zero-use in the current fixture
  corpus and by the user-stated position that sim and emu tests don't
  need to share names.
- **No backwards compat for `impl sim for T`.** Pre-RFC sources need
  the migration tool. Justified by mechanical migration + having only
  one canonical form going forward.
- **Sim is the default backend.** Emu invocation is `harc emu` rather
  than `harc sim --emu`; the two backends are CLI peers, not
  sub-modes of one CLI. This is consistent with how `harc sim`
  already exists as a top-level subcommand.

## 10. References

- Spec §2 (reserved keywords including `testbench`):
  [`../spec.md`](../spec.md)
- Spec §7.2 (current `test` / `impl X for T` semantics):
  [`../spec.md`](../spec.md)
- Spec §8 (`env`, `transactor`, `agent`, `scoreboard` composition):
  [`../spec.md`](../spec.md)
- Existing test patterns:
  [`tests/fixtures/axilite_env_test.harc`](../tests/fixtures/axilite_env_test.harc),
  [`tests/fixtures/axilite_regs_full_test.harc`](../tests/fixtures/axilite_regs_full_test.harc),
  [`tests/fixtures/axi_agent.harc`](../tests/fixtures/axi_agent.harc)
- Sibling RFC: [`ral-support.md`](./ral-support.md) — the `testbench`
  block introduced here is a natural host for `regblock` / `addrmap`
  instantiations from that RFC.
