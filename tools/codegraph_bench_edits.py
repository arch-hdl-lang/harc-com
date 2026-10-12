#!/usr/bin/env python3
"""Run isolated, paired HARC graph edit tasks without touching the source worktree."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def execute(task: dict, condition: str, repo: Path, output: Path, model: str, effort: str, repeat: int, metadata: dict) -> dict:
    run_id = f"{task['id']}-r{repeat}-{condition}"
    raw_path = output / f"{run_id}.jsonl"
    stderr_path = output / f"{run_id}.stderr.txt"
    with tempfile.TemporaryDirectory(prefix=f"{run_id}-", dir=output) as temporary:
        work = Path(temporary) / "repo"
        shutil.copytree(repo, work, ignore=shutil.ignore_patterns(".git", "target", ".harcgraph", ".harcdevgraph", "__pycache__", ".venv"))
        before = {name: digest(work / name) for name in task["expected_files"]}
        harc = repo / "target/debug/harc"
        index_started = time.monotonic()
        if task["task_class"] == "user":
            index_command = [str(harc), "graph", "index", "tests/fixtures", "tests/dut", "--out", ".harcgraph"]
        else:
            index_command = [str(harc), "graph", "dev-index", "--root", str(work), "--map", str(work / "docs/codegraph_feature_map.json"), "--out", str(work / ".harcdevgraph")]
        index = subprocess.run(index_command, cwd=work, capture_output=True, text=True, timeout=120)
        if index.returncode:
            raise RuntimeError(f"Index build failed: {index.stderr}")
        index_seconds = round(time.monotonic() - index_started, 3)
        common = (f"Task: {task['prompt']} Make the changes, run the requested checks, and briefly report what changed. "
                  f"The compiler is at {harc}. This isolated source snapshot has no .git directory.")
        if condition == "baseline":
            scope = "tests/fixtures" if task["task_class"] == "user" else "src and tests/fixtures"
            guidance = f"Use ordinary repository search, starting with filenames or rg -l under {scope}, then bounded source reads. Do not use graph commands or graph index files."
        else:
            hint = task["graph_hint"].replace("harc graph", f"{harc} graph")
            guidance = f"First run this graph command: {hint}. Inspect source only as needed."
        prompt = f"{common} {guidance}"
        command = ["codex", "exec", "--json", "--ignore-user-config", "-m", model,
                   "-c", f'model_reasoning_effort="{effort}"', "-s", "workspace-write",
                   "-C", str(work), "--skip-git-repo-check", "--ephemeral", prompt]
        started = time.monotonic()
        with raw_path.open("w") as raw, stderr_path.open("w") as stderr:
            completed = subprocess.run(command, cwd=work, stdout=raw, stderr=stderr, timeout=600)
        elapsed = round(time.monotonic() - started, 3)
        events = [json.loads(line) for line in raw_path.read_text().splitlines() if line.startswith("{")]
        usage = [event["usage"] for event in events if event.get("type") == "turn.completed"]
        answers = [event["item"]["text"] for event in events if event.get("type") == "item.completed" and event.get("item", {}).get("type") == "agent_message"]
        changed = [name for name, old in before.items() if digest(work / name) != old]
        checks = []
        for check in task["checks"]:
            cmd = [part.replace("{HARC_BIN}", str(harc)) for part in check]
            result = subprocess.run(cmd, cwd=work, capture_output=True, text=True, timeout=120,
                                    env={**os.environ, "CARGO_TARGET_DIR": str(repo / "target")})
            checks.append({"command": cmd, "exit_code": result.returncode, "output": (result.stdout + result.stderr)[-2000:]})
        return {
            "task_id": task["id"], "task_class": task["task_class"], "task_type": "change",
            "repeat": repeat, "condition": condition, "model": model, "effort": effort,
            **metadata, "cache_state": "fresh_ephemeral_context; provider_cache_as_reported",
            "input_tokens": sum(item["input_tokens"] for item in usage),
            "cached_input_tokens": sum(item.get("cached_input_tokens", 0) for item in usage),
            "output_tokens": sum(item["output_tokens"] for item in usage),
            "latency_s": elapsed, "index_seconds": index_seconds,
            "success": completed.returncode == 0 and len(changed) == len(before) and all(check["exit_code"] == 0 for check in checks),
            "changed_files": changed, "checks": checks,
            "tool_calls": sum(event.get("type") == "item.completed" and event.get("item", {}).get("type") == "command_execution" for event in events),
            "graph_response_tokens": None, "other_tool_response_tokens": None,
            "exit_code": completed.returncode, "answer": answers[-1] if answers else "",
            "raw_events": str(raw_path), "stderr": str(stderr_path),
        }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--tasks", type=Path, default=Path(__file__).resolve().parents[1] / "docs/codegraph_edit_pilot_tasks.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--effort", default="low")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--seed", type=int, default=17)
    args = parser.parse_args()
    repo = args.repo.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    metadata = {
        "repo_sha": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "repo_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "--binary"], cwd=repo)).hexdigest(),
        "codex_version": subprocess.check_output(["codex", "--version"], text=True).strip(),
        "harc_version": subprocess.check_output([str(repo / "target/debug/harc"), "--version"], text=True).strip(),
    }
    tasks = json.loads(args.tasks.read_text())["tasks"]
    schedule = [(task, repeat, condition) for task in tasks for repeat in range(1, args.repeats + 1) for condition in ("baseline", "graph")]
    random.Random(args.seed).shuffle(schedule)
    with (args.output / "records.jsonl").open("a") as records:
        for task, repeat, condition in schedule:
            result = execute(task, condition, repo, args.output, args.model, args.effort, repeat, metadata)
            records.write(json.dumps(result) + "\n")
            records.flush()
            print(f"{task['id']} r{repeat} {condition}: success={result['success']} tokens={result['input_tokens'] + result['output_tokens']}", flush=True)


if __name__ == "__main__":
    main()
