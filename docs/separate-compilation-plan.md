# Plan: build-once-run-many — staged delivery

Status: **In progress.**

## Staged delivery

| Phase | Status | Scope |
|---|---|---|
| 1a — dispatcher scaffolding | **Shipped (PR #112)** | `int run_<TestName>(argc, argv)` per test + dispatcher `int main()` reading `--test <name>` / `HARC_TEST`. |
| 1b — multi-test emission | **Shipped (PR #113)** | `emit_with_opts` iterates over all tests; one `run_<TestName>` per test; dispatcher's multi-branch logic; CLI passes `--test <name>` to binary at runtime. |
| 1c — build reuse | **Shipped (PR `feat/v0-build-reuse`)** | Drop the per-invocation `obj_dir/` wipe so Make's mtime check kicks in. Combined with `write_if_changed` for the emitted `.cpp`, `harc sim --test foo` then `harc sim --test bar` against the same source skips Verilator entirely — ~45× speedup measured (6.85s → 0.15s). `--rebuild` flag forces the cleanup when needed (Verilator-version change, etc.). |
| 1d — focused/split generated C++ | **Shipped** | `harc sim --compile-scope test --test <name>` emits only the selected test; `harc sim --cpp-split tests` emits a dispatcher plus one generated C++ translation unit per test for Verilator/SV builds. |
| 2 — full per-test `.o` (member-function refactor) | **Deferred** | The long-term end state below. Requires converting every `[&]`-capturing lambda in the codegen to a member function (or explicit-param function). ~2-3 weeks of careful work. Wait until benchmarks justify the spend. |

The remainder of this doc describes phase 2 — the end state.

Phase 1d is a bridge, not the final member-function architecture:
per-test generated `.cpp` files compile to separate objects and link
through one dispatcher binary. Byte-identical shared generated slices
are factored into generated headers, and byte-identical run-function
prologue/epilogue slices are factored into generated `.inc` snippets,
but C++ still parses those includes once per split translation unit
because the inline run bodies need complete generated record/component
types. To balance compiler startup/header-parse overhead against
incremental granularity, `--cpp-split tests` groups tests into shards
(`--cpp-split-group-size`, default 4). That gives useful test-edit
granularity, smaller emitted split sources, and faster clean builds on
the SRAM FD 8-test benchmark while preserving the existing single-file
default and leaving the deeper context/member refactor for Phase 2. The
first phase-2 bridge is also in place:
each `run_<TestName>` now creates a generated `HarcTestContext` and
binds legacy local names (`dut`, `_checkers`, coverage/report vectors,
trace/log state, `errors`, `_fatal`, `cycle_count`) as references to
context fields, so shared helpers can be migrated to explicit
`HarcTestContext&` parameters incrementally.

## Goal

Each compilation unit (DUT, testbench, individual test) compiles to
its own `.o` and links into a single binary. Touching one test
recompiles only that test's `.o`; touching the testbench recompiles
the testbench `.o` plus relinks; touching the DUT triggers Verilator
+ relinks. Today (post-MVP) all of those collapse into one huge
TU — a one-line edit to a test triggers a multi-second TU rebuild.

## What's hard

The current codegen emits every helper as a `[&]`-capturing lambda
inside `main()`. The capture pulls in the test-scope state:
`_checkers`, `tick`, `dut`, `_run_slot`, the scheduler, `sim_log`,
log_files, error counter, etc. Splitting compilation units means
each captured symbol must become either:

- A parameter explicitly threaded into the function.
- A field on a struct that the function lives on (`this->_checkers`).

The first scales poorly: dozens of dispatch helpers, each gaining
6-8 boilerplate parameters. The second is cleaner: components +
testbenches become proper C++ classes, methods access state via
`this->`.

## Concrete shape (member-function model)

```cpp
// tb/<TbName>.h
struct CounterTb {
    VTop* dut = nullptr;
    int* errors = nullptr;
    harc_rt::ThreadScheduler* sched = nullptr;
    std::vector<std::function<void()>>* _checkers = nullptr;
    // ... ctx fields ...

    void reset();
    void bump(uint32_t n);
};

// tb/<TbName>.cpp
void CounterTb::reset() { dut->rst = 1; /* ... */ }
void CounterTb::bump(uint32_t n) { /* ... */ }

// test/<TestName>.cpp
int run_TestbenchSmoke(int argc, char** argv) {
    VTop* dut = new VTop;
    int errors = 0;
    harc_rt::ThreadScheduler sched;
    std::vector<std::function<void()>> _checkers;

    CounterTb _tb {
        .dut = dut, .errors = &errors,
        .sched = &sched, ._checkers = &_checkers,
    };

    // ... per-test body: tb.reset(); tb.bump(5); assert(dut->count_out == 5); ...

    delete dut;
    return errors;
}

// main.cpp
extern int run_TestbenchSmoke(int, char**);
extern int run_TestbenchEnableToggle(int, char**);

int main(int argc, char** argv) {
    // ... parse --test / HARC_TEST ...
    if (strcmp(name, "TestbenchSmoke") == 0) return run_TestbenchSmoke(argc, argv);
    // ...
}
```

## Surface affected

- `emit_component_struct` → emit struct + method declarations to a
  header.
- `emit_component_method` → emit method body as `<Tb>::<method>(...)`
  with `this->` access to fields. Remove `[&]` capture.
- `emit_hook_vectors` → hook vectors become members on the testbench
  struct (each fixture's `_pre` / `_post` vector lives on its
  enclosing Tb instance, not at file scope).
- `emit_tseq` → tseq lambdas become free functions taking explicit
  `dut` / `tick` / `sched` parameters (these don't naturally fit on
  a struct).
- All call sites of `obj.method(args)` lower from `<Type>_<method>
  (obj, args)` to `obj.<method>(args)`. The dispatcher in
  `resolve_component_method_call` already walks the path; only the
  emission side changes.
- Build orchestration (in `src/main.rs` sim path): emit per-unit
  files, run compiler per unit, link.

## Migration cost estimate

~2–3 weeks of careful work, paced by:
- Plumbing struct-membership through the field-substitution code
  (today's `field_subs` already does similar work for transactor
  hookable bodies — the same path generalizes, but with state).
- Verifying all 70 fixtures cycle clean (many subtle capture sites:
  every `_checkers.push_back(...)`, every event subscriber, every
  monitor-actor body).
- Build orchestration: incremental Makefile generation, dep tracking
  so `harc sim --test foo` re-uses the previous build's `.o` files.

## Bridge from MVP

The MVP keeps the single-`.cpp` shape but wraps `main()`'s body into
per-test `run_<TestName>(argc, argv)` functions plus a dispatcher
`main()`. That single PR delivers the user-facing "one binary, many
tests" win at ~1/10 the cost of the full refactor.

When this deferred work lands, the per-test `run_<TestName>` functions
already exist as the natural unit boundary — they just move to their
own translation units, and the test-scope state they capture today via
locals gets re-shaped into testbench struct members.

## Decision log

- 2026-05-15: User asked for separate `.so` per testbench / DUT / test.
  After scoping (this doc), agreed to ship the single-`.cpp` MVP first
  (PR `feat/v0-multi-test-binary`) and defer the full refactor.
