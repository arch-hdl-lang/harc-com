# TB-IR migration manifest

Snapshot: 2026-09-30, `origin/main` at `cce5d626` plus this batch.

This is the retirement queue, not a count of every defensive diagnostic.
The source inventory is regenerated with:

```sh
rg -n -F 'unsupported(' src/ir/lower
awk '/^fn [A-Za-z0-9_]+\(/ { fn=$2; sub(/\(.*/, "", fn) }
     /assert_unsupported/ && fn != "assert_unsupported" { print fn }' tests/tbir.rs |
  sort -u
```

The snapshot has 84 textual `unsupported(` matches under `src/ir/lower`:
83 executable constructor sites and the constructor definition. That is down
from 174 constructor sites at the `e17b938d` baseline. The count is only an
inventory: one constructor can fence many source shapes, and adding a precise
fail-closed neighbor can increase it while the supported surface grows.

## State/helper audit queue

| Family | Executable evidence | v1 evidence | TB-IR status | Next action |
|---|---|---|---|---|
| Lazy message values with statement-producing calls | `component_call_in_message_stays_in_the_failure_branch`, `signed_transactor_result_in_message_retains_declared_type`, `a_nested_pop_preserves_short_circuit_laziness` | Existing direct helper/component/transactor message controls emit through v1; `lazy_message_calls_fail_test` runs only the older direct helper-message path. The new conditional call/pop shapes do not have a compiling v1 runtime oracle | **TB-IR extension implemented.** The whole capture lowers through branch-local CFG when a call or queue pop is under `&&`, `||`, or `?:`; direct queue pops use the same selected-message hoist. Immediate checks use a read-only AST preflight before building the selected failure-side CFG. The five per-family fallback constructors now share the retained lazy-message `Unsupported` gate used by concurrent messages that have no materialization seam | Retain the lowering/verifier tests. Add a TB-IR behavioral fixture if this extension needs runtime qualification; do not block retirement on unavailable v1 parity |
| One bound-transactor source type instantiated on distinct bus bindings | `the_event_driven_transactor_shape_arms_follow_v1s_dut_name_rule` (`split_bindings`), `bound_component_split_bindings_test` | v1 emits and runs each instance against its own binding | **TB-IR extension implemented.** Additional concrete bindings specialize the component schema and callable set, preserving a single bus adapter per typed owner. The self-checking fixture binds two passive instances to distinct DUT prefixes and verifies isolated observations in both emitters | Retain the lowering and equivalence coverage |
| Nested free helper calling a testbench method by bare name | `nested_free_helper_does_not_call_testbench_method_by_bare_name` | v1 passes the bare call through into a free C++ helper, where the sibling testbench method is undeclared | **Classified, not a retirement blocker.** TB-IR reports `NotImplemented { EmitsUncompilable }` instead of suggesting v1 | Retain the diagnostic regression |
| Uninstantiated covergroups with malformed hooks or impure sample expressions | `a_covergroup_hook_trigger_has_two_reachable_shape_arms`, `unused_impure_covergroup_expressions_do_not_block_tbir` | v1 defers hook validation and sampler execution until instantiation; unused declarations emit and run. Instantiated malformed hooks are rejected; a `wait` helper compiles and crashes, an `after` helper emits invalid C++, and a non-suspending DUT-reading helper runs | **TB-IR gap fixed.** Reachability pre-scan includes structural fields and statement-local instances in tests, reachable component bodies, and called file-scope helpers. Valid inert schemas remain in the structural catalog, while unsupported inert schemas no longer block lowering. Instantiated malformed hooks and suspending samplers retain no-fallback diagnostics; non-suspending impure helpers retain `Unsupported` and the working v1 fallback | Retain both declaration/instantiation split tests |

Four neighboring diagnostics were also retired from the fallback queue. A
discarded scalar expression now lowers into an intentionally unread temporary;
an unresolved value call is classified as v1-uncompilable; nested `fork`
expressions are classified as rejected by v1; and an unmerged `extend` item is
an invalid pipeline state rather than a backend support gap. Together with the
split-binding implementation, these are the five semantic sites in that batch.

This batch closes five declaration/diagnostic sites: unused malformed bare-hook
and non-path-hook covergroups, unused impure point and bin expressions, and the
statement-position unresolved bare-helper call. The two dedicated covergroup
hook constructors are gone; the impure-helper fallback remains for
non-suspending helpers that v1 can run, as does the shared field-method call
fallback.

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
