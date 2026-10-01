#!/usr/bin/env python3
"""Run the read-only HARC code-graph lookup pilot through Codex CLI."""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import subprocess
import time
from pathlib import Path


def command_output(*args: str, cwd: Path) -> str:
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def prompt_for(task: dict, condition: str, harc_bin: Path, repo: Path) -> str:
    common = f"Task: {task['prompt']} Return only the requested answer."
    if condition == "baseline":
        scope = "tests/fixtures, tests/dut, and tests/fixtures.tbl" if task["task_class"] == "user" else "src and tests/fixtures"
        return f"{common} Use ordinary repository search, scoped first to {scope}. Start with filenames or rg -l, then inspect only matching files with bounded output. Do not use graph commands or graph index files."
    hint = task["graph_hint"].replace("harc graph", f"{harc_bin} graph")
    hint = hint.replace(".harcgraph", str(repo / ".harcgraph"))
    hint = hint.replace(".harcdevgraph", str(repo / ".harcdevgraph"))
    return f"{common} First run this graph command: {hint}. Inspect source only if its answer is uncertain."


def run_one(task: dict, condition: str, repeat: int, model: str, effort: str, repo: Path, output: Path, metadata: dict) -> dict:
    run_id = f"{task['id']}-r{repeat}-{condition}"
    raw_path = output / f"{run_id}.jsonl"
    stderr_path = output / f"{run_id}.stderr.txt"
    prompt = prompt_for(task, condition, repo / "target/debug/harc", repo)
    command = [
        "codex", "exec", "--json", "--ignore-user-config", "-m", model,
        "-c", f'model_reasoning_effort="{effort}"', "-s", "read-only",
        "-C", str(repo), "--ephemeral", prompt,
    ]
    started = time.monotonic()
    with raw_path.open("w") as raw, stderr_path.open("w") as stderr:
        completed = subprocess.run(command, cwd=repo, stdout=raw, stderr=stderr, timeout=300, check=False)
    elapsed = time.monotonic() - started
    events = [json.loads(line) for line in raw_path.read_text().splitlines() if line.startswith("{")]
    usage = [event["usage"] for event in events if event.get("type") == "turn.completed"]
    answers = [event["item"]["text"] for event in events if event.get("type") == "item.completed" and event.get("item", {}).get("type") == "agent_message"]
    answer = answers[-1] if answers else ""
    record = {
        "task_id": task["id"], "task_class": task["task_class"], "task_type": task["task_type"],
        "repeat": repeat, "condition": condition, "model": model, "effort": effort,
        "repo_sha": metadata["repo_sha"], "repo_diff_sha256": metadata["repo_diff_sha256"],
        "codex_version": metadata["codex_version"], "harc_version": metadata["harc_version"],
        "cache_state": "fresh_ephemeral_context; provider_cache_as_reported",
        "input_tokens": sum(item["input_tokens"] for item in usage),
        "cached_input_tokens": sum(item.get("cached_input_tokens", 0) for item in usage),
        "output_tokens": sum(item["output_tokens"] for item in usage),
        "latency_s": round(elapsed, 3),
        "success": completed.returncode == 0 and all(term in answer for term in task["required"])
        and all(any(term in answer for term in group) for group in task.get("required_any", [])),
        "tool_calls": sum(event.get("type") == "item.completed" and event.get("item", {}).get("type") == "command_execution" for event in events),
        "graph_response_tokens": None, "other_tool_response_tokens": None,
        "index_seconds": 0, "exit_code": completed.returncode,
        "answer": answer, "raw_events": str(raw_path), "stderr": str(stderr_path),
    }
    return record


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--tasks", type=Path, default=Path(__file__).resolve().parents[1] / "docs/codegraph_pilot_tasks.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--effort", default="low")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()
    repo = args.repo.resolve()
    tasks = json.loads(args.tasks.read_text())["tasks"]
    args.output.mkdir(parents=True, exist_ok=True)
    metadata = {
        "repo_sha": command_output("git", "rev-parse", "HEAD", cwd=repo),
        "repo_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "--binary"], cwd=repo)).hexdigest(),
        "codex_version": command_output("codex", "--version", cwd=repo),
        "harc_version": command_output(str(repo / "target/debug/harc"), "--version", cwd=repo),
    }
    rng = random.Random(args.seed)
    schedule = [(task, repeat, condition) for task in tasks for repeat in range(1, args.repeats + 1) for condition in ("baseline", "graph")]
    rng.shuffle(schedule)
    with (args.output / "records.jsonl").open("a") as records:
        for task, repeat, condition in schedule:
            record = run_one(task, condition, repeat, args.model, args.effort, repo, args.output, metadata)
            records.write(json.dumps(record) + "\n")
            records.flush()
            print(f"{task['id']} r{repeat} {condition}: success={record['success']} tokens={record['input_tokens'] + record['output_tokens']}", flush=True)


if __name__ == "__main__":
    main()
