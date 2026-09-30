# TB-IR migration manifest

Snapshot: 2026-09-30, `origin/main` at `692a1f97`.

This is the retirement queue, not a count of every defensive diagnostic.
The source inventory is regenerated with:

```sh
rg -n -F 'unsupported(' src/ir/lower
awk '/^fn [A-Za-z0-9_]+\(/ { fn=$2; sub(/\(.*/, "", fn) }
     /assert_unsupported/ && fn != "assert_unsupported" { print fn }' tests/tbir.rs |
  sort -u
```

The snapshot has 90 textual `unsupported(` matches under `src/ir/lower`:
89 executable constructor sites and the constructor definition. That is down
from 174 constructor sites at the `e17b938d` baseline. The count is only an
inventory: one constructor can fence many source shapes, and adding a precise
fail-closed neighbor can increase it while the supported surface grows.

## State/helper audit queue

| Family | Executable evidence | v1 evidence | TB-IR status | Next action |
|---|---|---|---|---|
| Lazy message values with statement-producing calls | `component_call_in_message_stays_in_the_failure_branch`, `signed_transactor_result_in_message_retains_declared_type`, `a_nested_pop_preserves_short_circuit_laziness` | Existing direct helper/component/transactor message controls emit through v1; `lazy_message_calls_fail_test` runs only the older direct helper-message path. The new conditional call/pop shapes do not have a compiling v1 runtime oracle | **TB-IR extension implemented.** The whole capture lowers through branch-local CFG when a call or queue pop is under `&&`, `||`, or `?:`; direct queue pops use the same selected-message hoist. Immediate checks use a read-only AST preflight before building the selected failure-side CFG. The five per-family fallback constructors now share the retained lazy-message `Unsupported` gate used by concurrent messages that have no materialization seam | Retain the lowering/verifier tests. Add a TB-IR behavioral fixture if this extension needs runtime qualification; do not block retirement on unavailable v1 parity |
| One bound-transactor source type instantiated on distinct bus bindings | `the_event_driven_transactor_shape_arms_follow_v1s_dut_name_rule` (`split_bindings`) | v1 emits per-instance binding text; current TB-IR rejects the shared-body conflict | Candidate migration blocker, but the current test proves emission rather than a compiling/running behavioral control | Build a two-binding self-checking fixture before changing specialization |
| Nested free helper calling a testbench method by bare name | `nested_free_helper_does_not_call_testbench_method_by_bare_name` | No compiling v1 control is pinned; scope ownership is ambiguous | Unclassified, not yet a retirement blocker | Probe v1 compile/runtime behavior and reclassify before implementation |

No other state/helper `Unsupported` assertion currently has both a positive v1
runtime control and an unambiguous intended value semantics.

## Explicitly outside the retirement queue

| Family | Reason |
|---|---|
| Calls or queue pops in `wait until` predicates | The predicate is re-evaluated; statement materialization changes scheduling and needs a separate semantic design, not message-hoist reuse |
| Suspended helpers in concurrent checks or `on` triggers | v1 recursively re-enters checker execution or emits uncompilable callback code for the proven shapes |
| Recursive aggregate lists | Deliberately deferred; the user requested that this obscure feature not drive the burn-down |
| Bare `queue<Vec>` / `queue<list>` element spellings | Incomplete type spellings with no precise element schema; v1 merely chooses a fallback C++ type |
| `String` containers, persistent `String`, and non-scalar String operations | Intentional storage/ABI boundary; neighboring tests verify rejection before type erasure |
| Enum event/queue payloads and malformed component/connect shapes | v1 rejects, emits uncompilable C++, or silently changes the program; these are diagnostics, not escape hatches |
| Broken `axilite_cov_test` fixture | The fixture omits a member read by its check block, so v1 is not runnable either |

## Completion rule

A row is classified as a v1-proven retirement blocker only when its test
contains a positive v1 control that compiles and runs with the intended
behavior. Emission-only,
inert-declaration, uncompilable, and silently-mis-lowered controls remain in
the classification queue. A TB-IR-only extension may still be implemented when
it has precise lowering and verifier coverage, but its row must say that v1
runtime parity is unproven. Each v1-proven implemented family must add or retain
a TB-IR lowering test, verifier coverage, and a behavioral fixture where v1 can
serve as an honest oracle.
