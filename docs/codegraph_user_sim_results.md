# Simulation-backed HARC-user graph holdout: results

The [pre-registered protocol](codegraph_user_sim_protocol.md) and
[four-task manifest](codegraph_user_sim_tasks.json) were committed before the
scored run. This is a separate holdout on the reviewed graph-feature branch;
it does not replace the earlier 144-run confirmation result. The benchmark
made actual HARC test edits and independently ran the affected tests against
SystemVerilog DUTs with `harc sim --sv` after each agent turn.

## Primary result

| Condition | Attempts | Frozen-rubric successes | Provider tokens, total | Tokens/success | Uncached tokens/success | Index time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Scoped-search baseline | 12 | 11 | 2,545,760 | 231,433 | 25,213 | 0 s |
| Code graph | 12 | 11 | 2,017,911 | 183,446 | 21,492 | 7.8 s |

The fixed-mix primary saving is **20.7%** in provider-reported input plus
output tokens per frozen-rubric success, with no observed success-count
regression. It narrowly meets the protocol's 20% numerical gate. The
uncached-token sensitivity is **14.8%**, below that gate. Provider-reported
cached input tokens were 2,268,416 (baseline) and 1,781,504 (graph); they
remain in the primary measure. Index time is measured separately and does
not consume provider tokens.

| Task / model | Baseline success | Graph success | Baseline tokens/success | Graph tokens/success | Graph saving |
| --- | ---: | ---: | ---: | ---: | ---: |
| Register overwrite / Sol low | 3/3 | 3/3 | 153,451 | 174,381 | -13.6% |
| Scoreboard four rounds / Sol low | 3/3 | 3/3 | 167,305 | 147,031 | 12.1% |
| Known-file transactor / Sol low | 3/3 | 3/3 | 156,461 | 179,506 | -14.7% |
| Two-test post-reset hold / Luna low | 2/3 | 2/3 | 557,055 | 257,578 | 53.8% |

Across the three Sol tasks, graph cost **5.0% more** per success (166,973
versus 159,072 tokens). The known-file control did not use the graph in either
condition; its -14.7% apparent saving is run-to-run variation, not a graph
effect. The two-file Luna task accounts for more than the entire aggregate
gain. The median paired graph-minus-baseline token change is **+5,798**
tokens; seven of twelve pairs used more tokens with graph. A 20,000-resample
task-cluster bootstrap (four tasks, seed 43) gives a **-14.2% to +45.6%**
95% interval for the primary saving. This small holdout cannot establish a
general token-saving effect despite crossing the numerical gate.

## Outcome audit

All 24 runs had valid provider usage, exited successfully, followed their
assigned graph/search protocol, changed no unexpected source file on the 22
frozen-rubric successes, and passed their independent full simulations. The
two frozen-rubric failures were:

- Luna baseline, counter task repeat 2: changed the direct counter test but
  omitted the required scoreboard-backed test. It also changed
  `tests/fixtures/counter_test.harc` outside the requested scope. This is a
  genuine task failure even though the two independent simulations passed.
- Luna graph, counter task repeat 1: changed both requested tests. Each uses
  `for _ in 0 .. 1`, `wait 1 cycle`, and `assert dut.count_out == 0` to test
  the two post-reset disabled cycles; both simulations passed. The frozen
  rubric required the literal added text `wait 2 cycles` in each file, so it
  marked this behaviorally equivalent implementation unsuccessful. The
  primary score above is unchanged.

Crediting only that equivalent graph solution in a post-hoc sensitivity gives
12/12 graph successes and **27.3%** provider-token saving per success (21.9%
on uncached tokens), versus 11/12 baseline. It does not repair the narrow
task sample or the Sol-only regression. Future rubrics should accept
semantically equivalent two-cycle assertions; this frozen rubric was not
edited after seeing the result.

## Audit and limits

- Scored source commit: `a957b70d097805033113a82a66b434656d2bb255`.
  Source snapshot SHA-256:
  `57c7a871f44291218d6c99af5f956b353157bc7e1db81e72f3493c8dfac71d14`.
  Task manifest SHA-256:
  `d017d418b8ec92aa074f2637f2f191ae5d8354f3b673d65788b3f02e7b00d660`.
- Each of the four tasks has three complete baseline/graph pairs, using
  Codex CLI 0.159.2 and HARC 0.2.0. Graph commands were first in all nine
  graph-discovery runs; all six known-file runs skipped graph commands.
- Raw records, agent events, diffs, and simulator outputs are at
  `/private/tmp/harc-codegraph-user-sim-scored/`. An earlier five-record
  sandbox-blocked startup attempt in a different directory had zero usage
  and was excluded from the scored run.
- This tests edits to four shipped test fixtures with one fixed model per
  task, not broad real-user sessions. Provider caching, stochastic model
  behavior, and the large baseline counter outlier all matter. The result
  warrants a larger, independently sampled simulation-backed user holdout
  before claiming the 20% goal for HARC users generally.
