#!/usr/bin/env python3
"""Run a frozen, resumable paired graph benchmark in isolated source copies."""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import os
import random
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


EXCLUDES = {".git", "target", ".harcgraph", ".harcdevgraph", "__pycache__", ".venv"}


def snapshot_hash(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(path for path in root.rglob("*") if path.is_file()):
        digest.update(str(path.relative_to(root)).encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def freeze(repo: Path, tasks_path: Path, output: Path, harc: Path) -> tuple[Path, dict]:
    frozen = output / "frozen_source"
    metadata_path = output / "freeze.json"
    if metadata_path.exists():
        metadata = json.loads(metadata_path.read_text())
        if metadata["tasks_sha256"] != hashlib.sha256(tasks_path.read_bytes()).hexdigest():
            raise ValueError("task manifest changed after freeze")
        if metadata["source_sha256"] != snapshot_hash(frozen):
            raise ValueError("frozen source changed")
        return frozen, metadata
    if frozen.exists():
        raise ValueError("frozen source exists without freeze metadata")
    shutil.copytree(repo, frozen, ignore=shutil.ignore_patterns(*EXCLUDES))
    metadata = {
        "repo_sha": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "source_sha256": snapshot_hash(frozen),
        "tasks_sha256": hashlib.sha256(tasks_path.read_bytes()).hexdigest(),
        "codex_version": subprocess.check_output(["codex", "--version"], text=True).strip(),
        "harc_version": subprocess.check_output([str(harc), "--version"], text=True).strip(),
    }
    metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")
    return frozen, metadata


def prepare_index(task: dict, work: Path, harc: Path) -> float:
    if task.get("known_files"):
        return 0.0
    started = time.monotonic()
    if task["task_class"] == "user":
        cmd = [str(harc), "graph", "index", "tests/fixtures", "tests/dut", "--out", ".harcgraph"]
    else:
        cmd = [str(harc), "graph", "dev-index", "--root", str(work), "--map", str(work / "docs/codegraph_feature_map.json"), "--out", str(work / ".harcdevgraph")]
    result = subprocess.run(cmd, cwd=work, capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise RuntimeError(f"index failed: {result.stderr}")
    return round(time.monotonic() - started, 3)


def prompt_for(task: dict, condition: str, harc: Path) -> str:
    editing = task["task_type"] == "change"
    common = f"Task: {task['prompt']} "
    common += "Make the change and briefly report validation." if editing else "Return only the requested answer."
    common += f" Use bounded file reads and tool output. The HARC compiler is at {harc}. This isolated source snapshot has no .git directory."
    if task.get("known_files"):
        guidance = "The exact target files are given. Inspect them directly; no graph discovery is needed."
    elif condition == "baseline":
        scope = "tests/fixtures and tests/dut" if task["task_class"] == "user" else "src and tests/fixtures"
        guidance = f"Use ordinary repository search, starting with filenames or rg -l under {scope}, then bounded source reads. Do not use graph commands or graph index files."
    else:
        hint = task["graph_hint"].replace("harc graph", f"{harc} graph")
        guidance = f"Ownership is not given. First run this focused graph command: {hint}. Inspect source as needed."
    return f"{common} {guidance}"


def grade(task: dict, answer: str, work: Path, before: dict[str, str | None], harc: Path, target_dir: Path) -> tuple[bool, dict]:
    if task["task_type"] != "change":
        missing = [term for term in task.get("required", []) if term not in answer]
        missing_groups = [group for group in task.get("required_any", []) if not any(term in answer for term in group)]
        return not missing and not missing_groups, {"missing": missing, "missing_any": missing_groups}
    changed = []
    patches = []
    missing_markers = []
    for relative in task["expected_files"]:
        old = before[relative]
        path = work / relative
        new = path.read_text() if path.exists() else None
        if old != new:
            changed.append(relative)
        patches.extend(difflib.unified_diff((old or "").splitlines(keepends=True), (new or "").splitlines(keepends=True),
                                            fromfile=f"a/{relative}", tofile=f"b/{relative}"))
        for marker in task.get("must_contain", {}).get(relative, []):
            if new is None or marker not in new:
                missing_markers.append(f"{relative}: {marker}")
    checks = []
    for check in task["checks"]:
        cmd = [part.replace("{HARC_BIN}", str(harc)) for part in check]
        try:
            result = subprocess.run(cmd, cwd=work, capture_output=True, text=True, timeout=120,
                                    env={**os.environ, "CARGO_TARGET_DIR": str(target_dir)})
            output = result.stdout + result.stderr
            passed = result.returncode == 0 and ("test" not in cmd or "1 passed" in output)
            checks.append({"command": cmd, "passed": passed, "exit_code": result.returncode, "output": output[-2000:]})
        except subprocess.TimeoutExpired:
            checks.append({"command": cmd, "passed": False, "exit_code": None, "output": "timed out"})
    rubric = {"changed_files": changed, "missing_markers": missing_markers, "checks": checks, "patch": "".join(patches)}
    return len(changed) == len(before) and not missing_markers and all(check["passed"] for check in checks), rubric


def run_one(task: dict, condition: str, repeat: int, frozen: Path, repo: Path, output: Path, metadata: dict, harc: Path) -> dict:
    run_id = f"{task['id']}-r{repeat}-{condition}"
    raw_path = output / f"{run_id}.jsonl"
    stderr_path = output / f"{run_id}.stderr.txt"
    with tempfile.TemporaryDirectory(prefix=f"{run_id}-", dir=output) as temporary:
        work = Path(temporary) / "repo"
        shutil.copytree(frozen, work)
        before = {relative: (work / relative).read_text() if (work / relative).exists() else None
                  for relative in task.get("expected_files", [])}
        target_dir = output.resolve() / "shared_target"
        index_seconds = prepare_index(task, work, harc) if condition == "graph" else 0.0
        prompt = prompt_for(task, condition, harc)
        command = ["codex", "exec", "--json", "--ignore-user-config", "-m", task["model"],
                   "-c", f'model_reasoning_effort="{task["effort"]}"',
                   "-s", "workspace-write" if task["task_type"] == "change" else "read-only",
                   "-C", str(work), "--skip-git-repo-check", "--ephemeral", prompt]
        started = time.monotonic()
        with raw_path.open("w") as raw, stderr_path.open("w") as stderr:
            try:
                completed = subprocess.run(command, cwd=work, stdout=raw, stderr=stderr, timeout=600,
                                           env={**os.environ, "CARGO_TARGET_DIR": str(target_dir)})
                exit_code = completed.returncode
            except subprocess.TimeoutExpired:
                exit_code = 124
        elapsed = round(time.monotonic() - started, 3)
        events = [json.loads(line) for line in raw_path.read_text().splitlines() if line.startswith("{")]
        usage = [event["usage"] for event in events if event.get("type") == "turn.completed"]
        answers = [event["item"]["text"] for event in events if event.get("type") == "item.completed" and event.get("item", {}).get("type") == "agent_message"]
        answer = answers[-1] if answers else ""
        rubric_ok, rubric = grade(task, answer, work, before, harc, target_dir)
        graph_chars = 0
        other_chars = 0
        tool_calls = 0
        graph_calls = 0
        first_command = None
        for event in events:
            item = event.get("item", {})
            if event.get("type") == "item.completed" and item.get("type") == "command_execution":
                if first_command is None:
                    first_command = item
                tool_calls += 1
                chars = len(item.get("aggregated_output", ""))
                if " graph " in item.get("command", ""):
                    graph_calls += 1
                    graph_chars += chars
                else:
                    other_chars += chars
        protocol_ok = (graph_calls == 0 if condition == "baseline" or task.get("known_files")
                       else first_command is not None and " graph " in first_command.get("command", "")
                       and first_command.get("exit_code") == 0)
        return {
            "task_id": task["id"], "task_class": task["task_class"], "task_type": task["task_type"],
            "repeat": repeat, "condition": condition, "model": task["model"], "effort": task["effort"],
            "repo_sha": metadata["repo_sha"], "repo_diff_sha256": metadata["source_sha256"],
            "tasks_sha256": metadata["tasks_sha256"], "codex_version": metadata["codex_version"],
            "harc_version": metadata["harc_version"], "cache_state": "fresh_ephemeral_context; provider_cache_as_reported",
            "input_tokens": sum(item["input_tokens"] for item in usage),
            "cached_input_tokens": sum(item.get("cached_input_tokens", 0) for item in usage),
            "output_tokens": sum(item["output_tokens"] for item in usage),
            "latency_s": elapsed, "index_seconds": index_seconds,
            "success": exit_code == 0 and bool(usage) and rubric_ok and protocol_ok,
            "protocol_ok": protocol_ok, "rubric": rubric,
            "tool_calls": tool_calls, "graph_response_tokens": graph_chars // 4,
            "other_tool_response_tokens": other_chars // 4,
            "exit_code": exit_code, "answer": answer,
            "raw_events": str(raw_path), "stderr": str(stderr_path),
        }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--tasks", type=Path, default=Path(__file__).resolve().parents[1] / "docs/codegraph_confirmation_tasks.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--seed", type=int, default=29)
    parser.add_argument("--max-runs", type=int)
    parser.add_argument("--task-id", help="Run one task for harness smoke testing")
    args = parser.parse_args()
    repo = args.repo.resolve()
    tasks_path = args.tasks.resolve()
    tasks = json.loads(tasks_path.read_text())["tasks"]
    if len(tasks) != 24 or {kind: sum(t["task_class"] == kind for t in tasks) for kind in ("user", "compiler")} != {"user": 12, "compiler": 12}:
        raise ValueError("confirmation set must contain 12 tasks per class")
    args.output.mkdir(parents=True, exist_ok=True)
    source_harc = repo / "target/debug/harc"
    harc = args.output.resolve() / "harc"
    if not harc.exists():
        shutil.copy2(source_harc, harc)
    frozen, metadata = freeze(repo, tasks_path, args.output, harc)
    records_path = args.output / "records.jsonl"
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
            record = run_one(task, condition, repeat, frozen, repo, args.output, metadata, harc)
            records.write(json.dumps(record) + "\n")
            records.flush()
            tokens = record["input_tokens"] + record["output_tokens"]
            print(f"{task['id']} r{repeat} {condition}: success={record['success']} tokens={tokens}", flush=True)


if __name__ == "__main__":
    main()
