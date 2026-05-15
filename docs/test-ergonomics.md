# Design doc: Test ergonomics — `testbench` + inline `run`

**Status:** Partially shipped.
**Date logged:** 2026-05-13. Updated 2026-05-13 after Phase 1 + 2
landed.
**Scope:** Two related changes to remove ceremony from HARC test files:
(1) a `testbench` block that owns the DUT, bus binding, transactor/scoreboard
composition, and shared setup methods — so multiple tests reuse one
structural skeleton; (2) inline `run` inside `test`, replacing the
two-block `test T { ... } / impl sim for T { run ... }` form.

## Implementation status

| Item | Status | Lands in |
|---|---|---|
| Inline `run` / `setup` / `check` / `teardown` / `phase` inside `test` | **Shipped** | PR #91 (parser + 1 canary fixture) |
| Fixture-corpus migration to inline form | **Shipped** | PR #92 (all 60 fixtures + `scripts/migrate_v1_inline.py`) |
| Remove `impl <target> for <Test>` parser entry | **Shipped** | PR #92 |
| Backend selection via CLI subcommand only | **Shipped** | implied by parser removal — no per-test annotation surface |
| `testbench` block (shared structural skeleton + helper methods) | **Shipped** | Parser entry pre-existing; `function` keyword + codegen suppression of hook vectors landed in PR #109. Bus-binding-as-field (`bus : Bus = bind dut`) and `hookable function` deferred. |
| `impl <name> for <Tb> ... end impl <name>` — testbench-bound test form | **Shipped** | Parser + AST + pre-emission desugaring. Canary fixture: `testbench_basic_test.harc`. Phase 2 (separate PR) sweeps the 70-fixture corpus and removes the classic `test T { let dut; ... }` form. |
| Dead-code prune (`Item::Impl`, `ImplDecl`, `ImplItem`, `parse_impl`) | **Pending** | follow-up tidy PR |

## 1. Motivation

### 1.1 Pre-implementation duplication

Before Phase 2 landed, every test repeated the same structural
boilerplate. From the pre-migration `tests/fixtures/axilite_env_test.harc`:

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

Spec §7.2 originally defended the two-block form on the grounds that
one `test` may have multiple impls:

> The same `test SimpleTest` declaration can be implemented for both —
> `impl sim for SimpleTest` and `impl emu for SimpleTest` coexist as
> orthogonal top-level items, possibly in separate files.

Pre-RFC survey of `tests/fixtures/*.harc` (76 files at the time):

- 76 fixtures total.
- 1 fixture contained more than one `impl sim for` (`axi_agent.harc`,
  which holds two *different* tests — not a multi-impl-per-test case).
- **Zero fixtures used the multi-impl feature.**

The `emu` backend ships post-v0. When it lands, the design constraint
isn't "every sim test must have a matching emu impl under the same name"
— a sim-only test and an emu-only test can simply have different names.
The cost of the two-block form was paid universally; the benefit was
paid to a feature that wasn't used and won't need to be used.

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

## 3. Surface — `testbench`  *(Shipped)*

The reference fixture is `tests/fixtures/testbench_basic_test.harc` — two distinct tests share one `testbench CounterTb` declaration with `function reset()` and `function bump(n)` helpers. Run-fixture manifest entries:

```
testbench_basic_test    | Top  | top_counter.sv | | | TestbenchSmoke
testbench_basic_test    | Top  | top_counter.sv | | | TestbenchEnableToggle
```

Spec syntax (updated for what actually lands today — the prose below uses `function` rather than the originally-proposed `fn`, since `function` is already the HARC top-level free-function keyword and reusing it keeps the language to one spelling per concept):

```harc
testbench AxiLiteTb
    // Component composition — declared once, instantiated per test.
    dut : AxiLiteRegs
    bus : BusAxiLite = bind dut
    drv : AxilXactor passive = bind bus
    sb  : AxilSb

    // Shared procedural helpers — callable from any test's run.
    function reset()
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
    end function reset

    function drive_random(n: int)
        let txns = RandomTxns(n)
        for t in txns
            sb.expected.push(t.value)
            drv.axil_write(t.addr, t.value)
            let got = drv.axil_read(t.addr)
            if got != sb.expected.pop()
                sb.errors = sb.errors + 1
            end if
        end for
    end function drive_random
end testbench AxiLiteTb
```

> The `bus : BusAxiLite = bind dut` and transactor-instance-as-field shapes shown above are illustrative — bus-binding inside a component body is a follow-up surface (currently bus bindings work only at test scope as `let axil : BusAxiLite = bind dut`). The shipped subset is: DUT-typed fields, value-typed inner state (scoreboards / primitives), and `function` / `hookable` helper methods. A testbench that needs bus access goes through a test-scope bus binding plus a transactor in env composition today.

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

- `function name(...) [-> T] ... end function [name]` declares a method on the testbench instance. (The originally-drafted `fn` keyword was renamed to `function` during implementation to keep HARC to one spelling per concept — `function` is the existing keyword for free-function declarations at top level.)
- Inside the body, bare field names resolve to `self.<field>` via the same field-substitution path that transactor `hookable` methods use.
- Functions are **synchronous between waits**, same coroutine model as the test's `run` block. They may call `wait`, `for`, `if`, and other procedural constructs.
- Functions are **not hookable**: no `<Type>_<method>_pre` / `<Type>_<method>_post` vectors are emitted, and the method body has no fan-out wrapper. Lowering: a free `[&]`-capturing lambda named `<Type>_<method>`, called as `<Type>_<method>(tb, args)` at every `tb.method(args)` call site (`resolve_component_method_call` walks the same path as for hookables).
- If a testbench function needs the pre/post hook machinery, declare it `hookable` instead of `function` — same body shape, plus the hook vectors.

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

### 3.3 Testbench-bound test form — `impl <name> for <Tb>`

The classic `test T { let dut; let tb; run; }` form forces every test to repeat the same instantiation boilerplate (and the awkward `tb.dut = dut` wire-up). The bound form folds the testbench into the test's scope:

```harc
testbench CounterTb
    dut : Top

    function reset()
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
    end function reset

    function bump(n : uint<32>)
        dut.en = 1
        wait n cycles
        dut.en = 0
    end function bump
end testbench CounterTb

impl Smoke for CounterTb
    run
        reset()                       -- = _tb.reset()
        bump(5)                       -- = _tb.bump(5)
        assert dut.count_out == 5    -- = dut->count_out
    end run
end impl Smoke

impl EnableToggle for CounterTb       -- second test, same testbench
    run
        reset()
        bump(3)
        wait 3 cycles
        let frozen = dut.count_out
        wait 5 cycles
        assert dut.count_out == frozen
    end run
end impl EnableToggle
```

**Semantics.**
- Bare-name lookup inside the bound test body falls through to the testbench instance: identifiers matching testbench fields rewrite to `_tb.<name>`, identifiers matching testbench methods (`function` or `hookable`) rewrite to `<TbType>_<name>(_tb, ...)`.
- `dut` is reserved as the test-scope name for the DUT pointer. The desugarer synthesizes `let dut : <SVType>` at test scope from the testbench's first SV-typed field, then emits `_tb.dut = dut` so the testbench's pointer aliases the test-scope pointer (one allocation, two pointers, same instance). Bare `dut.signal` resolves through the existing pointer-var path — no `_tb.` prefix.
- User-declared `let X` at test scope shadows any testbench field named `X` (other than `dut`, which is always synthesized).
- The testbench instance is **fresh per test** — each `impl Foo for Tb` gets its own default-constructed `Tb` allocated at the start of that test's `main()`.

**Lowering.** A pre-emission AST pass (`desugar_impl_for_test_in_file`) expands each `TestDecl` with `for_testbench: Some(...)` into the classic shape: prepend `let dut : <SVType>` and `let _tb : <TbType>`, prepend `_tb.dut = dut` to the run block, and rewrite bare-name references in run / setup / check / teardown / phase bodies. Once desugared, the test threads through the same codegen as a classic-form test.

**Status.** Surface + canary fixture shipped. Phase 2 (a separate PR) sweeps the remaining 69 fixtures from `test T { ... }` to `impl T for SomeTb { ... }` and then removes the classic-form parser entry.

## 4. Surface — inline `run` inside `test`  *(shipped in PR #91 + #92)*

`impl sim for T` has been removed entirely. The `run` block (and the
existing optional `setup` / `check` / `teardown` phase blocks from
spec §7.2, plus user-defined `phase <name>` blocks) now lives
directly inside `test`:

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

`test T { fields; run; }` is exactly equivalent to the pre-Phase-2
`test T { fields } end test` + `impl sim for T { run } end impl`. The
parser (`src/parser.rs::parse_test`) accumulates inline phase blocks
into one synthetic `TestItem::Scope` — the same AST shape `cpp_tb`
previously synthesized from `Item::Impl` — so generated C++ for the
60 migrated fixtures was byte-for-byte unchanged.

A small AST addition shipped alongside: `TestItem::Phase(Ident, Block)`
captures inline `phase <name>` blocks (the `env_quiesced_phase_test`
fixture uses one). The codegen `custom_phases` table now reads from
both that variant and the legacy `ImplItem::Phase` for parity.

### 4.2 No compatibility with the old form

`impl <target> for <Test>` is **removed**, not deprecated. The parser
now surfaces a directive on encountering the keyword at top level:

```
expected inline `run` / `setup` / `check` / `teardown` block inside
the `test` body (the legacy `impl <target> for <Test>` wrapper was
removed — see docs/test-ergonomics.md and scripts/migrate_v1_inline.py)
```

Reasons this was the right call:

1. The fixture corpus showed zero uses of multi-impl-per-test, so the
   migration cost was mechanical.
2. Keeping both forms would mean readers must know two equivalent
   syntaxes, which is exactly the "two ways to do one thing" pattern
   HARC's spec §1 design principles explicitly reject.
3. The `impl X for T` AST + codegen can be deleted in a follow-up
   tidy PR; the parser entry is already gone.

## 5. Backend selection  *(shipped)*

With `impl sim for T` gone, backend choice lives at the CLI. Tests
carry no backend annotation in the source.

```sh
harc sim my_file.harc --top Foo      # sim backend (today's default)
harc emu my_file.harc --top Foo      # emu backend (post-v0)
```

This matches how `harc sim --sv` / `harc sim --dut` already select
DUT-side flavor at the CLI rather than in the source. The future `emu`
story is its own RFC; this doc only commits to *not needing per-test
backend annotations in the source*.

## 6. Lowering

### 6.1 `testbench`  *(Phase 3, not yet implemented)*

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

### 6.2 `test` with inline `run`  *(shipped)*

A `test` block lowers to a sibling struct holding the testbench
instance plus the `run` coroutine, identical to the pre-Phase-2 `test`
+ `impl sim for` shape — only the parser entry-point changed.

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

## 7. Migration  *(shipped)*

### 7.1 What actually moved

PR #92 migrated the fixture corpus:

- **58 in-file migrations.** Each `impl sim for T { ... }` body was
  moved into the matching `test T { ... }` block; the `impl` wrapper
  was deleted. `axi_agent.harc` (two tests in one file) was handled
  per-test independently.
- **8 cross-file migrations.** Sidecar `*_test_sim.harc` files (the
  pre-RFC pattern where a separate file held `impl sim for X`) were
  concatenated into their companion `*_test.harc` first, then run
  through the same in-file migrator. Sidecars themselves were
  deleted; `tests/run_fixtures.sh` rows that referenced them as
  `extras` were updated to drop those references.
- **1 orphan removed.** `tests/fixtures/axilite_monitor_test_sim.harc`
  had no companion test in the codebase and was not in the manifest;
  it was deleted as dead.
- **Test harness sources migrated.** Raw-string fixtures in
  `tests/codegen.rs` and `tests/round_trip.rs` were collapsed by the
  same regex shape. Two test cases that exercised the now-removed
  parse paths were deleted (`split_test_via_extend_round_trips_to_same_cpp`,
  `impl_emu_only_test_errors_clearly`).
- **Pretty-printer + snapshot.** `src/pretty.rs` switched
  `TestItem::Scope` from emitting a `scope sim ... end scope sim`
  wrapper to inline phase blocks, matching the new canonical form.
  `tests/snapshots/round_trip__axi_agent.snap` was refreshed.

Outcome: 86 files changed, **−115 net lines** of fixture ceremony.
Manifest after migration: **60 passed, 0 failed.** Unit tests: 78
passed across lib + codegen + round_trip.

### 7.2 Tooling

`scripts/migrate_v1_inline.py` (~170 lines of Python) implements the
migrator. The RFC originally called it `harc fmt --migrate-v1`; a
standalone script was cheaper for a one-shot job and is what shipped.
Logic:

- Find every `impl sim for X { ... } end impl X` block; look up the
  matching `test X { ... } end test X` in the same file; move the
  impl body into the test just before `end test`; delete the impl
  wrapper.
- Walk impls in reverse source order so deletes don't invalidate
  later offsets.
- Collapse 3+ consecutive blank lines to 2 to keep output tidy.
- Print a per-file count of migrated pairs plus a total.

The script remains in the tree for any future inbound legacy
sources (third-party HARC files imported from elsewhere).

### 7.3 Spec edits (still TODO)

These spec edits are not part of this RFC's PRs and should land
separately:

- §7.2: drop the multi-impl rationale. Replace with the inline-`run`
  shape. Strike "There is no inheritance / no `super` chain — each
  impl stands alone" and replace with "Each test stands alone — no
  inheritance, no shared mutable state across tests."
- §14 (Phasing): drop references to `impl X for T` from the phase
  descriptions; the phase blocks live inside `test` directly.
- §16 (ARCH Lowering Map): update the "Tests lower to ARCH `testbench`"
  entry to note the user-facing `testbench` keyword (Phase 3) will
  map more directly to the ARCH primitive than the v0 `test` block
  did.

### 7.4 Dead code (still TODO)

The Phase 2 PR kept `Item::Impl`, `ImplDecl`, `ImplItem`,
`parse_impl`, and the `cpp_tb` synthesis block that consumed them as
unreachable code. They compile but are unused — a follow-up tidy PR
should prune them once no in-flight branches reference them.

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

The Phase 1 + Phase 2 PRs locked these in:

- **No multi-impl per test.** Spec §7.2's stated multi-impl design
  goal is dropped. Justified by zero-use in the (then) 76-fixture
  corpus and by the user-stated position that sim and emu tests don't
  need to share names.
- **No backwards compat for `impl sim for T`.** Pre-Phase-2 sources
  must run through `scripts/migrate_v1_inline.py`. Justified by
  mechanical migration + having only one canonical form going forward.
- **Sim is the default backend.** Emu invocation is `harc emu` rather
  than `harc sim --emu`; the two backends are CLI peers, not
  sub-modes of one CLI. This is consistent with how `harc sim`
  already exists as a top-level subcommand.

## 10. References

- Spec §2 (reserved keywords including `testbench`):
  [`../spec.md`](../spec.md)
- Spec §7.2 (`test` semantics — pre-RFC also covered `impl X for T`):
  [`../spec.md`](../spec.md)
- Spec §8 (`env`, `transactor`, `agent`, `scoreboard` composition):
  [`../spec.md`](../spec.md)
- PRs that shipped Phase 1 + Phase 2:
  - PR #91 — parser change + one canary fixture
  - PR #92 — fixture-corpus migration + parser entry removal
- Migration tool: [`../scripts/migrate_v1_inline.py`](../scripts/migrate_v1_inline.py)
- Inline-form fixture examples in the migrated corpus:
  [`tests/fixtures/rom_lut_test.harc`](../tests/fixtures/rom_lut_test.harc),
  [`tests/fixtures/axilite_env_test.harc`](../tests/fixtures/axilite_env_test.harc),
  [`tests/fixtures/axi_agent.harc`](../tests/fixtures/axi_agent.harc),
  [`tests/fixtures/env_quiesced_phase_test.harc`](../tests/fixtures/env_quiesced_phase_test.harc)
  (uses an inline `phase <name>` block)
- Sibling RFC: [`ral-support.md`](./ral-support.md) — the `testbench`
  block proposed here is a natural host for `regblock` / `addrmap`
  instantiations from that RFC.
