#!/usr/bin/env python3
"""Summarize paired HARC graph benchmark JSONL without counting tool payloads twice."""

from __future__ import annotations

import argparse
import json
import random
import statistics
from collections import defaultdict
from pathlib import Path


def summarize(records: list[dict], bootstrap_samples: int = 10000) -> dict:
    strata = defaultdict(list)
    for record in records:
        key = (record["task_class"], record["task_type"], record["model"], record["effort"])
        strata[key].append(record)
        strata[(record["task_class"], "all", record["model"], record["effort"])].append(record)

    result = {}
    for key, rows in sorted(strata.items()):
        pairs = defaultdict(dict)
        for row in rows:
            pair_key = (row["task_id"], row["repeat"])
            condition = row["condition"]
            if condition not in {"baseline", "graph"} or condition in pairs[pair_key]:
                raise ValueError(f"duplicate or unknown condition for {pair_key}: {condition}")
            pairs[pair_key][condition] = row
        incomplete = [pair_key for pair_key, pair in pairs.items() if set(pair) != {"baseline", "graph"}]
        if incomplete:
            raise ValueError(f"incomplete paired runs: {incomplete}")
        paired = list(pairs.values())
        for pair in paired:
            if pair["baseline"].get("repo_sha") != pair["graph"].get("repo_sha"):
                raise ValueError("paired runs use different repository revisions")
            if pair["baseline"].get("repo_diff_sha256") != pair["graph"].get("repo_diff_sha256"):
                raise ValueError("paired runs use different working-tree changes")
        conditions = {}
        for condition in ("baseline", "graph"):
            condition_rows = [pair[condition] for pair in paired]
            total_tokens = sum(row["input_tokens"] + row["output_tokens"] for row in condition_rows)
            successes = sum(bool(row["success"]) for row in condition_rows)
            conditions[condition] = {
                "attempts": len(condition_rows),
                "successes": successes,
                "success_rate": successes / len(condition_rows),
                "total_provider_tokens": total_tokens,
                "tokens_per_success": total_tokens / successes if successes else None,
                "cached_input_tokens": sum(row.get("cached_input_tokens", 0) for row in condition_rows),
                "median_latency_s": statistics.median(row["latency_s"] for row in condition_rows),
                "total_index_seconds": sum(row.get("index_seconds", 0) for row in condition_rows),
            }
        both_success = [
            ((pair["graph"]["input_tokens"] + pair["graph"]["output_tokens"])
             - (pair["baseline"]["input_tokens"] + pair["baseline"]["output_tokens"]))
            for pair in paired if pair["baseline"]["success"] and pair["graph"]["success"]
        ]
        differences = [int(bool(pair["graph"]["success"])) - int(bool(pair["baseline"]["success"])) for pair in paired]
        rng = random.Random(0)
        estimates = sorted(
            sum(rng.choice(differences) for _ in differences) / len(differences)
            for _ in range(bootstrap_samples)
        )
        result["/".join(key)] = {
            "conditions": conditions,
            "success_rate_difference": statistics.mean(differences),
            "success_rate_difference_ci95": [
                estimates[int(0.025 * (bootstrap_samples - 1))],
                estimates[int(0.975 * (bootstrap_samples - 1))],
            ],
            "both_success_pairs": len(both_success),
            "median_both_success_token_change": statistics.median(both_success) if both_success else None,
        }
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("records", type=Path, help="One run record per JSONL line")
    args = parser.parse_args()
    records = [json.loads(line) for line in args.records.read_text().splitlines() if line.strip()]
    print(json.dumps(summarize(records), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
