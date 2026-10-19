#!/usr/bin/env python3
"""Validate and report a complete CI run of the frozen simulation benchmark."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


METRIC_FIELDS = ("task_id", "repeat", "condition", "model", "effort", "repo_sha",
                 "repo_diff_sha256", "tasks_sha256", "input_tokens", "cached_input_tokens",
                 "output_tokens", "success", "protocol_ok", "exit_code", "index_seconds",
                 "latency_s")


def public_metrics(records: list[dict]) -> str:
    return "".join(json.dumps({key: row[key] for key in METRIC_FIELDS if key in row},
                              sort_keys=True) + "\n" for row in records)


def report(records: list[dict], tasks: dict, max_token_regression: float = 0.20) -> tuple[str, bool]:
    expected = {(task["id"], repeat, condition)
                for task in tasks["tasks"]
                for repeat in range(1, tasks["repeats"] + 1)
                for condition in ("baseline", "graph")}
    actual = [(row["task_id"], row["repeat"], row["condition"]) for row in records]
    if len(actual) != len(set(actual)) or set(actual) != expected:
        raise ValueError(f"incomplete or duplicate benchmark records: missing={sorted(expected - set(actual))}, "
                         f"unexpected={sorted(set(actual) - expected)}")
    if len({(row["repo_sha"], row["repo_diff_sha256"], row["tasks_sha256"]) for row in records}) != 1:
        raise ValueError("benchmark records have different frozen sources or task manifests")
    if any(not row["protocol_ok"] or row["input_tokens"] + row["output_tokens"] <= 0 for row in records):
        raise ValueError("benchmark has a failed protocol or missing provider usage")

    by_condition = {condition: [row for row in records if row["condition"] == condition]
                    for condition in ("baseline", "graph")}
    stats = {}
    for condition, rows in by_condition.items():
        tokens = sum(row["input_tokens"] + row["output_tokens"] for row in rows)
        successes = sum(bool(row["success"]) for row in rows)
        stats[condition] = (len(rows), successes, tokens, tokens / successes if successes else None,
                            sum(row.get("cached_input_tokens", 0) for row in rows),
                            sum(row.get("index_seconds", 0) for row in rows))

    baseline, graph = stats["baseline"], stats["graph"]
    if baseline[3] is None or graph[3] is None:
        saving = None
    else:
        saving = 1 - graph[3] / baseline[3]
    passed = graph[1] >= baseline[1] and saving is not None and saving >= -max_token_regression
    lines = ["# HARC graph CI benchmark", "",
             "| Condition | Attempts | Sim-backed successes | Provider tokens | Tokens/success |",
             "| --- | ---: | ---: | ---: | ---: |"]
    for condition, (attempts, successes, tokens, per_success, _, _) in stats.items():
        lines.append(f"| {condition} | {attempts} | {successes} | {tokens:,} | "
                     f"{per_success:,.0f} |" if per_success is not None else
                     f"| {condition} | {attempts} | {successes} | {tokens:,} | n/a |")
    lines.extend(["", f"Saving: {saving:.1%}" if saving is not None else "Saving: n/a",
                  f"Cached input tokens: baseline {baseline[4]:,}; graph {graph[4]:,}.",
                  f"Graph indexing time: {graph[5]:.1f} s (excluded from tokens).", "",
                  "| Task | Baseline success | Graph success | Baseline tokens | Graph tokens |",
                  "| --- | ---: | ---: | ---: | ---: |"])
    for task in tasks["tasks"]:
        selected = {condition: [row for row in by_condition[condition] if row["task_id"] == task["id"]]
                    for condition in ("baseline", "graph")}
        cells = []
        for condition in ("baseline", "graph"):
            rows = selected[condition]
            cells.extend((f"{sum(bool(row['success']) for row in rows)}/{len(rows)}",
                          f"{sum(row['input_tokens'] + row['output_tokens'] for row in rows):,}"))
        lines.append(f"| {task['id']} | {cells[0]} | {cells[2]} | {cells[1]} | {cells[3]} |")
    lines.extend(["",
                  f"Regression gate: {'PASS' if passed else 'FAIL'} "
                  f"(graph success >= baseline; token/success saving >= {-max_token_regression:.0%}).",
                  "A pass does not establish the 20% savings goal; the four-task sample is too small."])
    return "\n".join(lines) + "\n", passed


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("records", type=Path)
    parser.add_argument("--tasks", type=Path, default=Path(__file__).resolve().parents[1] / "docs/codegraph_user_sim_tasks.json")
    parser.add_argument("--summary", type=Path)
    parser.add_argument("--metrics", type=Path, help="Write allowlisted, artifact-safe metrics")
    args = parser.parse_args()
    records = [json.loads(line) for line in args.records.read_text().splitlines() if line.strip()]
    summary, passed = report(records, json.loads(args.tasks.read_text()))
    print(summary)
    if args.summary:
        args.summary.write_text(summary)
    if args.metrics:
        args.metrics.write_text(public_metrics(records))
    if not passed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
