# HARC graph CI regression

Ordinary pull requests run `tools/codegraph_ci_retrieval.py` after building
HARC. It indexes the shipped fixtures and DUTs and checks that the graph still
finds the three ownership-discovery targets from the simulation benchmark.
This check is deterministic and has no model cost; it cannot measure token
savings or simulation quality.

The separate **HARC graph simulation benchmark** GitHub Actions workflow is
manual and runs only from `main`. It uses the frozen four-task, three-repeat
manifest in `codegraph_user_sim_tasks.json`: 12 scoped-search baselines and 12
graph runs. Each agent edits an isolated source copy; an independent
`harc sim --sv` verifies the result. The workflow reports provider tokens per
success, success counts, cached tokens, index time, and per-task totals in the
Actions summary and uploads allowlisted `metrics.jsonl`, `freeze.json`, and
the summary. Agent answers, patches, and raw events stay on the ephemeral
runner and are not uploaded.

To run it, configure a dedicated, limited `OPENAI_API_KEY` repository secret,
then dispatch the workflow on `main`. Model access for GPT-6 Sol and GPT-6 Luna
is required. The secret is exposed to the agent process during this manually
dispatched job, so use a key dedicated to this trusted-repository benchmark and
rotate it if exposure is suspected. Do not enable this workflow on untrusted
pull-request code. There is no secret configured as of this change, so CI
cannot run the model-backed portion yet.

The agent copy omits benchmark task manifests, benchmark results, and harness
scripts; the runner retains them outside that copy for grading. CI scores the
current `main` source, so its numbers are not directly comparable to the
September holdout source snapshot. The prompt/task manifest remains fixed.

The CI regression gate fails if graph has fewer successful outcomes than
baseline or costs more than 20% extra provider tokens per success. It also
rejects missing/duplicate runs, mismatched source snapshots, missing usage,
and graph-protocol violations. A pass is **not** a claim that the 20% saving
goal was met: that requires positive evidence from a larger, independently
sampled user corpus. The frozen fourth-task literal rubric has the known
behavioral-equivalence limitation documented in `codegraph_user_sim_results.md`.
The CI grader accepts the specific two-single-cycle-assertion equivalent
observed there, so CI uses a rubric v2 and must not be presented as an exact
repeat of that historical scored run.
