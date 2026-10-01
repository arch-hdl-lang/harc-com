#!/usr/bin/env python3
"""Run the frozen HARC-user behavioral-edit graph holdout."""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import random
import shutil
from pathlib import Path

import codegraph_bench_confirmation as common


SOURCE_EXTENSIONS = {".harc", ".arch", ".sv", ".rs", ".py"}
FROZEN: Path


def grade(task: dict, answer: str, work: Path, before: dict[str, str | None], harc: Path, target_dir: Path) -> tuple[bool, dict]:
    base_ok, rubric = common.grade(task, answer, work, before, harc, target_dir)
    missing_added = []
    for relative, markers in task["must_add"].items():
        old = (before[relative] or "").splitlines()
        new = (work / relative).read_text().splitlines()
        added = "\n".join(line[1:] for line in difflib.ndiff(old, new) if line.startswith("+ "))
        missing_added.extend(f"{relative}: {marker}" for marker in markers if marker not in added)

    baseline_sources = {
        path.relative_to(FROZEN): hashlib.sha256(path.read_bytes()).digest()
        for path in FROZEN.rglob("*") if path.is_file() and path.suffix in SOURCE_EXTENSIONS
    }
    current_sources = {
        path.relative_to(work): hashlib.sha256(path.read_bytes()).digest()
        for path in work.rglob("*") if path.is_file() and path.suffix in SOURCE_EXTENSIONS
        and ".bench_sim" not in path.relative_to(work).parts
    }
    unexpected = sorted(str(path) for path in baseline_sources.keys() | current_sources.keys()
                        if baseline_sources.get(path) != current_sources.get(path)
                        and str(path) not in task["expected_files"])
    sim_passed = all(check["passed"] and "ALL TESTS PASSED" in check["output"] for check in rubric["checks"])
    rubric.update({"missing_added": missing_added, "unexpected_source_changes": unexpected,
                   "simulation_passed": sim_passed})
    return base_ok and not missing_added and not unexpected and sim_passed, rubric


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--tasks", type=Path, default=Path(__file__).resolve().parents[1] / "docs/codegraph_user_sim_tasks.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--seed", type=int, default=43)
    parser.add_argument("--max-runs", type=int)
    parser.add_argument("--task-id")
    args = parser.parse_args()
    repo = args.repo.resolve()
    tasks_path = args.tasks.resolve()
    manifest = json.loads(tasks_path.read_text())
    tasks = manifest["tasks"]
    if manifest["status"] != "frozen_before_scored_runs" or len(tasks) != 4 or args.repeats != manifest["repeats"]:
        raise ValueError("holdout requires four frozen tasks and three repeats")
    if {task["id"] for task in tasks} != {"sim01_register_overwrite", "sim02_scoreboard_four_rounds",
                                             "sim03_known_transactor_six", "sim04_counter_post_reset_hold"}:
        raise ValueError("holdout task identities changed")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    harc = output / "harc"
    if not harc.exists():
        shutil.copy2(repo / "target/debug/harc", harc)
    global FROZEN
    FROZEN, metadata = common.freeze(repo, tasks_path, output, harc)
    records_path = output / "records.jsonl"
    done = {(row["task_id"], row["repeat"], row["condition"]) for row in
            (json.loads(line) for line in records_path.read_text().splitlines())} if records_path.exists() else set()
    schedule = [(task, repeat, condition) for task in tasks for repeat in range(1, args.repeats + 1)
                for condition in ("baseline", "graph")]
    if args.task_id:
        schedule = [entry for entry in schedule if entry[0]["id"] == args.task_id]
        if not schedule:
            raise ValueError(f"unknown task id: {args.task_id}")
    random.Random(args.seed).shuffle(schedule)
    pending = [(task, repeat, condition) for task, repeat, condition in schedule
               if (task["id"], repeat, condition) not in done]
    if args.max_runs is not None:
        pending = pending[:args.max_runs]
    with records_path.open("a") as records:
        for task, repeat, condition in pending:
            record = common.run_one(task, condition, repeat, FROZEN, repo, output, metadata, harc, grader=grade)
            records.write(json.dumps(record) + "\n")
            records.flush()
            tokens = record["input_tokens"] + record["output_tokens"]
            print(f"{task['id']} r{repeat} {condition}: success={record['success']} tokens={tokens}", flush=True)


if __name__ == "__main__":
    main()
