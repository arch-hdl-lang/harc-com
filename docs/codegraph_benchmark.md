# HARC graph token-efficiency benchmark

Status: the three-repeat frozen benchmark is complete. See
`codegraph_confirmation_results.md` for the result and decision; exploratory
pilots remain in `codegraph_pilot_results.md`.

The frozen confirmation set is `codegraph_confirmation_tasks.json`: 12
tasks per class, with four lookups, four impact questions, and four verified
two-file changes each. It fixes the task mix at eight GPT-6 Luna low
lookup/impact tasks and four GPT-6 Sol low edits per class. Two edits name
their exact files and require the selective graph policy to skip retrieval;
the others require ownership discovery. The set is deliberately focused on
shipped fixtures and curated compiler features, so conclusions are limited
to that population.

Preflight on 2026-09-30 checked all 16 lookup/impact graph hints against the
compiler and ran isolated paired smoke tasks for a known-file HARC edit and a
scoreboard codegen regression. Both edit pairs passed their checks. The smoke
runs are excluded from the frozen measurement. The manifest status and source
snapshot are frozen before the scored screen.

Run a one-repeat screen first, then continue the same frozen source and task
set to three repeats only if each class has at least a 10% token-per-success
improvement and no more than one additional failed outcome in its first 12
pairs. This is a pre-registered futility stop, not an early success claim. If
the screen stops, report an exploratory result and do not call it confirmation.
For a completed run, apply the 20% gate below. The runner freezes all tracked
and untracked source bytes, stores an index-build time only for graph runs,
and retains model events and edit diffs for audit.

The benchmark must use paired baseline and graph-first runs on one repository
SHA. Both conditions receive the same task text, model, effort, token cap,
non-graph tools, and success rubric. Graph-first may call graph CLI/MCP before
opening files; baseline may use ordinary search/docs/MCP but no graph tools.
Use fresh contexts, randomize condition order, and repeat each task three
times. Record cache state and tool versions. Do not tune on the confirmation
set. The original, focused, and edit pilot tasks are in
`codegraph_pilot_tasks.json`, `codegraph_focused_pilot_tasks.json`, and
`codegraph_edit_pilot_tasks.json`.

Before confirmation, freeze at least 12 tasks per class (four definition
lookups, four impact tasks, four small verified changes) for both HARC users
and compiler development. Ground truth must include required files/symbols
and a task-specific test or review rubric. Use `gpt-6-luna` at low effort for
simple lookups on both sides of a pair, and a fixed stronger model and effort
for complex changes on both sides. Never pool model strata without fixed,
reported task weights.

Each JSONL run record should contain `task_id`, `task_class`, `task_type`,
`model`, `effort`, `repeat`, `condition`, `repo_sha`, `success`,
`input_tokens`, `output_tokens`, `cached_input_tokens`, `latency_s`,
`tool_calls`, `graph_response_tokens`, `other_tool_response_tokens`, and
`index_seconds`. Sum provider-reported tokens across retries before writing
the record. Tool payload token counts are diagnostics only, not extra model
input. Preserve raw model events and the success rubric decision for audit.

Analyze complete pairs with:

```sh
python3 tools/codegraph_bench_analyze.py docs/codegraph_runs.jsonl
```

The primary metric is total provider input plus output tokens divided by
successful outcomes. Report it by class, task type, and model/effort. The
analyzer also reports all-run token totals, success rates, a paired bootstrap
interval for the graph-minus-baseline success difference, cached input tokens,
latency, and the both-success median token change. A zero-success condition
has undefined tokens per success and fails the gate. The initial gate is at
least 20% lower primary tokens per success in each class, no worse than a
five-point observed success-rate regression, and an uncertainty interval that
excludes a larger regression. Negative or inconclusive results must be
reported without changing the frozen set.

The user approved model runs after confirming the repository is public. The
standalone CLI was updated from 0.142.5 to 0.159.2. Both GPT-5.6 Luna and
GPT-6 Luna now complete no-repository probes and report provider usage. An
earlier exploratory pair on GPT-5.5 low is kept separate from Luna results.
