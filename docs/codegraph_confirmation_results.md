# HARC code graph frozen benchmark result

Date: 2026-09-30. The frozen 24-task set has 12 HARC-user tasks and 12
compiler-development tasks per repeat: four lookups, four impact questions,
and four two-file changes in each class. Three paired repeats produced 144
executions. Lookup/impact used GPT-6 Luna low; changes used GPT-6 Sol low.
Condition order was randomized with seed 29. Every run used a fresh ephemeral
model context and isolated source copy. The graph condition built its own
index before the model call; baseline did not. Two known-file changes per
repeat skipped graph retrieval under the selective routing policy.

The source snapshot is `c3ca30dac9da098085e5e180559761188e5d6755`
plus uncommitted worktree bytes with SHA-256
`6c434268998f040745fef554ea4dcb901f25bd6e7df73c4c20fc2ad59c0262ec`.
The task-manifest SHA-256 is
`10287d2beac8655a0b96f2f7a3c0bd09dfba249ef1f72dcdb5aba5c013d743ac`.
The runner used Codex CLI 0.159.2 and HARC 0.2.0. Raw events, per-run records,
edit patches, and the freeze metadata are at
`/tmp/harc-codegraph-confirmation/` on this machine. All 144 records have
unique task/repeat/condition keys, one source hash, one task hash, and no
routing violations.

## Primary outcome

The primary metric is provider-reported input plus output tokens divided by
successful outcomes. Cached input is included. The fixed rubric was applied
without edits after freeze.

| Class | Baseline tokens / success | Graph tokens / success | Saving | Successes |
| --- | ---: | ---: | ---: | ---: |
| HARC users | 2,379,546 / 35 = 67,987 | 2,043,164 / 36 = 56,755 | 16.5% | 35/36 vs 36/36 |
| Compiler development | 5,089,069 / 31 = 164,164 | 3,680,376 / 36 = 102,233 | 37.7% | 31/36 vs 36/36 |

A 10,000-resample task-cluster bootstrap gives a 95% interval for saving of
8.5%-26.3% for users and 23.6%-55.9% for compiler development. **The
pre-registered 20% gate fails for users and passes for compiler development
on this task population.** Graph-first had no observed success regression,
but this sample is too narrow to establish a general quality guarantee.

| Class / task type | Model | Baseline tokens | Graph tokens | Successes, baseline / graph | Token saving per success |
| --- | --- | ---: | ---: | ---: | ---: |
| User lookup | Luna low | 598,302 | 431,339 | 12/12 / 12/12 | 27.9% |
| User impact | Luna low | 587,410 | 468,177 | 11/12 / 12/12 | 26.9% |
| User change | Sol low | 1,193,834 | 1,143,648 | 12/12 / 12/12 | 4.2% |
| Compiler lookup | Luna low | 828,826 | 476,869 | 12/12 / 12/12 | 42.5% |
| Compiler impact | Luna low | 928,033 | 480,516 | 7/12 / 12/12 | 69.8% |
| Compiler change | Sol low | 3,332,210 | 2,722,991 | 12/12 / 12/12 | 18.3% |

The fixed mix is eight Luna and four Sol tasks per class per repeat; do not
interpret the class aggregate as one model's performance. User edit tasks
barely benefited, especially when the exact files were already named.
Compiler edits varied substantially by task and repeat, despite a favorable
aggregate. The frozen task set is concentrated on shipped fixtures, curated
features, reset assertions, and parser/codegen regression tests; it is not a
sample of arbitrary compiler implementation work.

## Sensitivity and overhead

Manual review found four valid alternate answers rejected by exact strings:
the user StepCov answer named `StepXactor.step` rather than `drv.step`, and
three compiler randomize answers correctly identified the TB-IR backend at
`src/codegen/tbir/func.rs` rather than the curated C++ emitter. Crediting
those answers *without changing the frozen primary score* yields 14.1%
user saving (36/36 baseline successes) and 31.7% compiler saving (34/36
baseline successes). Task-cluster bootstrap intervals become 7.6%-22.6%
and 22.3%-44.5%, respectively. Two other compiler baseline runs genuinely
failed after mistaking the provided HARC binary path for a checkout path.
Excluding both pairs and crediting the alternate backend answers still gives
28.0% raw compiler token saving on the remaining 34 pairs.

Across all runs, cached input tokens were 2,037,504 baseline versus
1,716,224 graph for users, and 4,469,248 versus 3,234,816 for compiler
work. Uncached input plus output fell only 4.4% for users and 28.1% for
compiler work. These are token counts, not a price estimate. Index builds
added 45.9 seconds across 36 user graph runs and 216.2 seconds across 36
compiler graph runs; a real reused index would amortize that cost. Every
edit run passed its task-specific HARC check or Rust test. The HARC checks
validate compilation, not full simulation, and the codegen tests inspect
generated C++ rather than executing it.

## Decision

Keep graph retrieval selective. Use it to locate unknown compiler ownership
and cross-file HARC relationships; skip it when the target files are already
known. Do not make graph-first the default for all HARC-user edit tasks on the
strength of this benchmark. A future user-side test should include broader,
behavioral testbench changes with simulation-based checks and should state
accepted equivalent answers before freeze.
