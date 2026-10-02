# HARC graph exploratory token pilot

Date: 2026-09-30. Repository base SHA: `c3ca30da`, with the uncommitted
code-graph worktree changes held constant across the pair. Both conditions
used Codex CLI, `gpt-5.5` at low reasoning effort, a fresh ephemeral context,
read-only sandbox, and the same user lookup task. Condition instructions
allowed graph CLI use only for graph-first.

Task: identify the HARC test for the `top_counter` SystemVerilog DUT.
Ground truth: `TopCounterTest` in `tests/fixtures/top_counter_test.harc`.

| Condition | Input tokens | Cached input | Output tokens | Total tokens | Correct |
| --- | ---: | ---: | ---: | ---: | --- |
| Baseline | 130,348 | 99,200 | 562 | 130,910 | Yes |
| Graph-first | 85,318 | 64,384 | 474 | 85,792 | Yes |

The observed total-token change is -34.5% for this one successful pair.
This is not a savings estimate: the baseline's first repository-wide search
returned an unusually large payload, while graph-first also spent tokens
locating the local compiler binary. The baseline raw event stream was not
preserved; the graph-first stream is at
`/tmp/harc-pilot-user-dut-graph.jsonl` on this machine. The next pilot runs
must save both raw streams, use scoped baseline search guidance, and provide
the graph condition the executable path. No index-build overhead is included
in these provider token totals.

The initial CLI (0.142.5) rejected GPT-6 Luna and required a newer version
for GPT-5.6 Luna. After the standalone CLI was updated to 0.159.2, both
models completed no-repository probes and returned provider usage. A
four-task paired pilot on GPT-6 Luna low is reported below; confirmation
still requires a frozen task set.

## GPT-6 Luna paired pilot

After the CLI update, the four tasks in `codegraph_pilot_tasks.json` ran once
per condition on `gpt-6-luna` at low effort. Condition order was randomized
with seed 7, and each run used a fresh ephemeral context. Raw CLI event
streams, stderr, automated records, and reviewed records are retained at
`/tmp/harc-codegraph-luna-pilot/` on this machine. The model and effort were
held fixed within every pair; no retry was used.

The exact-string grader incorrectly failed both transactor answers for naming
the valid `axilite_bound_mon_test.harc` fixture instead of the one example in
the pilot manifest. It also failed the baseline regblock answer, which named
the valid `regblock_subset_test.harc` fixture and correct lowerer path but not
the exact function name. Those three answers were manually marked successful
in `reviewed_records.jsonl`. The baseline DUT answer gave the filename stem
`top_counter_test` instead of the declared `TopCounterTest` symbol, so it
remained unsuccessful under the task's test-name rubric. This ambiguity should
be removed in the confirmation prompt.

| Class | Baseline tokens | Graph tokens | Baseline success | Graph success | Tokens per baseline success | Tokens per graph success |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| HARC users | 65,164 | 61,029 | 1/2 | 2/2 | 65,164 | 30,515 |
| Compiler development | 71,783 | 61,579 | 2/2 | 2/2 | 35,892 | 30,790 |

All-run tokens fell 10.5% (136,947 to 122,608). The compiler class's
tokens per successful outcome fell 14.2%. The user class's larger per-success
drop depends on one baseline symbol-name failure; if its filename-stem answer
were accepted, the user-class token change would instead be 6.3%. This small
pilot does not establish the 20% gate, a success-rate bound, or an edit-task
result. Index build time was not attributed to individual runs because both
conditions used a prebuilt index; it must be reported separately before a
deployment decision.

## Focused-filter pilot

Steps 1-3 added developer `--roles`, user `--node-kind`/`--edge-kind`,
and compact MCP queries that build missing or refresh stale indexes. An MCP
smoke test verified auto-build and stale refresh with temporary indexes.

The focused task manifest is `codegraph_focused_pilot_tasks.json`. Four
lookup/impact tasks ran once per condition on GPT-6 Luna low, with random
condition order (seed 13), scoped baseline search, and a fresh ephemeral
context per run. All eight answers satisfied the corrected rubric. Equivalent
shipped fixtures count as valid; a stale-index attempt and an ambiguous
fixture rubric were discarded before the recorded rerun. Raw event streams
and records are at `/tmp/harc-codegraph-focused-pilot-v2/`.

| Class | Baseline tokens | Graph tokens | Change | Successes |
| --- | ---: | ---: | ---: | ---: |
| HARC users | 96,853 | 62,224 | -35.8% | 2/2 each |
| Compiler development | 80,055 | 60,979 | -23.8% | 2/2 each |

These are pilot observations, not confirmation results. In particular,
compiler regblock impact was effectively flat (30,412 baseline versus
30,607 graph), while the parser lookup drove the compiler aggregate. Cached
input and fixed context are included in provider totals.

## Isolated multi-file edit pilot

The edit manifest is `codegraph_edit_pilot_tasks.json`. Each GPT-6 Sol low
run used a separate source snapshot, randomized condition order (seed 17),
and the same verification checks within its pair. The user task updated two
HARC fixture comments and both `harc check` commands passed in both
conditions. The compiler task updated AST and parser Rust doc comments, and
`cargo check --offline --lib` passed in both conditions. Changed-file and
check results, provider usage, and raw events are at
`/tmp/harc-codegraph-edit-pilot-v3/`. Neither task changed executable
behavior. Both conditions built an index for isolation, so build time was
held approximately equal rather than attributed only to graph-first.

| Class | Baseline tokens | Graph tokens | Change | Successes | Index build, graph |
| --- | ---: | ---: | ---: | ---: | ---: |
| HARC users | 110,929 | 90,692 | -18.2% | 1/1 each | 0.57 s |
| Compiler development | 133,109 | 133,737 | +0.5% | 1/1 each | 2.83 s |

The edit results do not meet the 20% gate in either class. They are also
documentation-only edits with explicit target files; they do not establish
the effect on substantive compiler or testbench changes. The next frozen
confirmation set needs more diverse changes, repeats, and task-specific
review beyond compile/check success. Do not pool Luna lookup and Sol edit
strata into one savings percentage.
