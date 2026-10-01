# Simulation-backed HARC-user graph holdout

This is a separate, prospective user-edit holdout on the reviewed graph PR
source. It does not change the frozen 144-run confirmation set or its published
16.5% HARC-user saving. The four tasks and exact rubric are frozen in
`codegraph_user_sim_tasks.json` before scored runs. Three tasks require
ownership discovery; one names its file and must skip the graph. The simple
two-file counter task uses GPT-6 Luna low, while the other three use GPT-6 Sol
low. Each task has three paired baseline/graph runs (24 executions), a fresh
ephemeral model context, isolated source copy, and randomized condition order
(seed 43).

The required outcome is an actual source edit, task-specific added stimulus
or assertions, no other source changes, and successful `harc sim --sv` execution
with `ALL TESTS PASSED` for each affected fixture. The simulator runs again
independently after the agent turn. Passing `harc check` alone is insufficient.
The baseline receives ordinary scoped search; graph-required runs must make a
successful graph command their first command. The known-file task must not
call the graph in either condition.

Primary measure: total provider-reported input plus output tokens per
successful outcome, cached tokens included. Report each model stratum and the
fixed-mix aggregate, success counts, index time, and uncached-token
sensitivity. The original 20% saving goal is met on this holdout only if the
fixed-mix primary measure improves by at least 20% with no observed success
regression. With just four task clusters, even a positive result is directional
and cannot establish general HARC-user savings. No tasks, prompts, models,
rubric, or source bytes will be changed after freeze; failed runs remain in the
denominator. Raw Codex events, diffs, simulation checks, and source/task hashes
are retained outside the repository under the chosen output directory.
