#!/usr/bin/env python3
"""HARC MCP server.

MVP goal: keep agents grounded in shipped HARC behavior by searching current
docs/fixtures, running the compiler, and querying the local learning store.
"""

from __future__ import annotations

import os
import pathlib
import re
import subprocess
import tempfile
from dataclasses import dataclass

from mcp.server.fastmcp import FastMCP

SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR.parent
MAX_OUTPUT_CHARS = 24_000
MIN_TIMEOUT_SECONDS = 1
MAX_TIMEOUT_SECONDS = 300


def _workspace_roots_from_env() -> list[pathlib.Path]:
    roots = []
    raw_roots = os.environ.get("HARC_MCP_WORKSPACE_ROOTS", "")
    for raw_root in raw_roots.split(os.pathsep):
        raw_root = raw_root.strip()
        if raw_root:
            roots.append(pathlib.Path(raw_root).expanduser())
    roots.append(PROJECT_ROOT)
    return list(dict.fromkeys(root.resolve() for root in roots))


WORKSPACE_ROOTS = _workspace_roots_from_env()
HARC_BIN = os.environ.get("HARC_BIN", str(PROJECT_ROOT / "target" / "release" / "harc"))

_INSTRUCTIONS = (SCRIPT_DIR / "instructions.md").read_text()
mcp = FastMCP("harc", instructions=_INSTRUCTIONS)


@dataclass(frozen=True)
class SearchHit:
    path: pathlib.Path
    line_no: int
    line: str
    score: int = 0


def _resolve_safe(path: str) -> pathlib.Path:
    raw_path = pathlib.Path(path).expanduser()
    if raw_path.is_absolute():
        resolved = raw_path.resolve()
    else:
        for root in WORKSPACE_ROOTS:
            candidate = (root / raw_path).resolve()
            if candidate.exists() and _is_allowed_path(candidate):
                return candidate
        resolved = (WORKSPACE_ROOTS[0] / raw_path).resolve()

    if _is_allowed_path(resolved):
        return resolved
    raise ValueError(
        f"Path escapes allowed workspace roots: {path} "
        f"(allowed: {', '.join(str(root) for root in WORKSPACE_ROOTS)})"
    )


def _is_allowed_path(path: pathlib.Path) -> bool:
    for root in WORKSPACE_ROOTS:
        try:
            path.relative_to(root)
            return True
        except ValueError:
            pass
    return False


def _display_path(path: pathlib.Path) -> str:
    for root in WORKSPACE_ROOTS:
        try:
            return str(path.relative_to(root))
        except ValueError:
            pass
    return str(path)


def _owning_root(path: pathlib.Path) -> pathlib.Path:
    for root in WORKSPACE_ROOTS:
        try:
            path.relative_to(root)
            return root
        except ValueError:
            pass
    return PROJECT_ROOT


def _run(args: list[str], timeout: int = 30, cwd: pathlib.Path | None = None) -> str:
    timeout = max(MIN_TIMEOUT_SECONDS, min(int(timeout), MAX_TIMEOUT_SECONDS))
    try:
        result = subprocess.run(
            args,
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=str(cwd or PROJECT_ROOT),
        )
    except subprocess.TimeoutExpired:
        return f"[ERROR] Command timed out after {timeout}s\n$ {' '.join(args)}"
    except FileNotFoundError:
        return (
            f"[ERROR] binary not found: {args[0]}\n"
            "Build HARC first with `cargo build --release --bin harc`, "
            "or set HARC_BIN."
        )

    parts = [f"$ {' '.join(args)}"]
    if result.stdout.strip():
        parts.append(result.stdout.strip())
    if result.stderr.strip():
        parts.append(result.stderr.strip())
    prefix = "OK" if result.returncode == 0 else f"ERROR (exit {result.returncode})"
    output = f"[{prefix}]\n" + "\n".join(parts)
    if len(output) > MAX_OUTPUT_CHARS:
        omitted = len(output) - MAX_OUTPUT_CHARS
        output = output[:MAX_OUTPUT_CHARS] + f"\n\n[TRUNCATED: omitted {omitted} characters]"
    return output


def _grep_files(query: str, files: list[pathlib.Path], max_hits: int) -> list[SearchHit]:
    terms = [t.lower() for t in re.findall(r"[A-Za-z0-9]+", query)]
    if not terms:
        return []
    hits: list[SearchHit] = []
    for path in files:
        if not path.exists() or not path.is_file():
            continue
        filename_tokens = set(re.findall(r"[A-Za-z0-9]+", path.name.lower()))
        filename_score = 50 if all(term in filename_tokens for term in terms) else 0
        if not filename_score and any(term in filename_tokens for term in terms):
            filename_score = 15
        try:
            lines = path.read_text(errors="replace").splitlines()
        except OSError:
            continue
        for idx, line in enumerate(lines, start=1):
            lower = line.lower()
            line_tokens = set(re.findall(r"[A-Za-z0-9]+", lower))
            all_terms = all(term in line_tokens for term in terms)
            any_terms = any(term in line_tokens for term in terms)
            if all_terms or any_terms or filename_score:
                line_score = 100 if all_terms else 10 if any_terms else 0
                status_score = 25 if re.search(r"\b(shipped|partial|proposed|rfc|roadmap)\b", lower) else 0
                score = filename_score + line_score + status_score
                hits.append(SearchHit(path, idx, line.rstrip(), score))
    hits.sort(key=lambda hit: (-hit.score, _display_path(hit.path), hit.line_no))
    return hits[:max_hits]


def _snippet(path: pathlib.Path, line_no: int, context: int = 18) -> str:
    lines = path.read_text(errors="replace").splitlines()
    start = max(1, line_no - context)
    end = min(len(lines), line_no + context)
    body = []
    for idx in range(start, end + 1):
        marker = ">" if idx == line_no else " "
        body.append(f"{marker}{idx:4d}: {lines[idx - 1]}")
    return f"--- {_display_path(path)}:{line_no} ---\n" + "\n".join(body)


def _fixture_files() -> list[pathlib.Path]:
    fixture_dir = PROJECT_ROOT / "tests" / "fixtures"
    if not fixture_dir.exists():
        return []
    return sorted(fixture_dir.glob("*.harc"))


def _dut_files() -> list[pathlib.Path]:
    dut_dir = PROJECT_ROOT / "tests" / "dut"
    if not dut_dir.exists():
        return []
    return sorted([*dut_dir.glob("*.sv"), *dut_dir.glob("*.arch")])


def _format_hits(hits: list[SearchHit], include_snippets: bool) -> str:
    if not hits:
        return "(no matches)"
    if include_snippets:
        return "\n\n".join(_snippet(hit.path, hit.line_no) for hit in hits)
    return "\n".join(f"{_display_path(hit.path)}:{hit.line_no}: {hit.line}" for hit in hits)


@mcp.resource("harc://readme")
def readme() -> str:
    """Current HARC README: shipped status, CLI overview, and examples."""
    return (PROJECT_ROOT / "README.md").read_text()


@mcp.resource("harc://specification")
def specification() -> str:
    """Full HARC v1 specification. Verify feature status before relying on roadmap items."""
    return (PROJECT_ROOT / "spec.md").read_text()


@mcp.resource("harc://sim-cli")
def sim_cli() -> str:
    """Complete `harc sim` CLI option reference."""
    return (PROJECT_ROOT / "docs" / "harc-sim-cli.md").read_text()


@mcp.resource("harc://test-ergonomics")
def test_ergonomics() -> str:
    """Test/testbench ergonomics status and syntax guidance."""
    return (PROJECT_ROOT / "docs" / "test-ergonomics.md").read_text()


@mcp.resource("harc://ral-support")
def ral_support() -> str:
    """Register abstraction layer status and syntax guidance."""
    return (PROJECT_ROOT / "docs" / "ral-support.md").read_text()


@mcp.tool()
def get_harc_syntax(query: str, max_hits: int = 8) -> str:
    """Search current HARC docs, parser source, and fixtures for syntax.

    Use this before writing unfamiliar `.harc` code. It intentionally searches
    fixtures and parser source along with docs because HARC is pre-1.0 and some
    prose describes roadmap design.
    """
    max_hits = max(1, min(max_hits, 20))
    files = [
        PROJECT_ROOT / "README.md",
        PROJECT_ROOT / "docs" / "harc-sim-cli.md",
        PROJECT_ROOT / "docs" / "test-ergonomics.md",
        PROJECT_ROOT / "docs" / "ral-support.md",
        PROJECT_ROOT / "src" / "parser.rs",
        *_fixture_files(),
    ]
    hits = _grep_files(query, files, max_hits)
    return _format_hits(hits, include_snippets=True)


@mcp.tool()
def harc_feature_status(query: str, max_hits: int = 12) -> str:
    """Return shipped/partial/proposed evidence for a HARC feature query.

    Read this before relying on advanced HARC features. Evidence is gathered
    from README status text, design-doc status tables, parser source, and
    runnable fixtures. A fixture or parser hit is stronger evidence of shipped
    syntax than a roadmap/spec-only hit.
    """
    max_hits = max(1, min(max_hits, 30))
    status_files = [
        PROJECT_ROOT / "README.md",
        PROJECT_ROOT / "docs" / "test-ergonomics.md",
        PROJECT_ROOT / "docs" / "ral-support.md",
        PROJECT_ROOT / "docs" / "tb-ir-design.md",
        PROJECT_ROOT / "spec.md",
        PROJECT_ROOT / "src" / "parser.rs",
        *_fixture_files(),
    ]
    hits = _grep_files(query, status_files, max_hits)
    has_fixture = any("tests/fixtures" in _display_path(hit.path) for hit in hits)
    has_parser = any(_display_path(hit.path) == "src/parser.rs" for hit in hits)
    has_proposed = any(
        "proposed" in hit.line.lower()
        or "rfc" in hit.line.lower()
        or "roadmap" in hit.line.lower()
        or "future" in hit.line.lower()
        or "deferred" in hit.line.lower()
        for hit in hits
    )
    has_partial = any("partial" in hit.line.lower() or "partially shipped" in hit.line.lower() for hit in hits)
    has_shipped = any("shipped" in hit.line.lower() for hit in hits) or has_fixture or has_parser

    if has_shipped and has_proposed:
        verdict = "MIXED: shipped evidence exists, but some docs also mention proposed/RFC scope."
    elif has_partial:
        verdict = "PARTIAL/MIXED: status text reports partial support. Validate exact syntax with fixtures/compiler."
    elif has_shipped:
        verdict = "LIKELY SHIPPED: parser or fixture/status evidence exists. Still validate with `harc_check`."
    elif has_proposed:
        verdict = "LIKELY PROPOSED/ROADMAP: only proposed/RFC evidence found. Do not assume user-facing syntax."
    else:
        verdict = "UNKNOWN: no clear shipped/proposed signal. Check fixtures/parser and validate with compiler."

    return f"{verdict}\n\n" + _format_hits(hits, include_snippets=False)


@mcp.tool()
def harc_examples(query: str, max_files: int = 5) -> str:
    """Find runnable fixture examples matching a query and return compact snippets."""
    max_files = max(1, min(max_files, 12))
    hits = _grep_files(query, _fixture_files(), max_files)
    if not hits:
        dut_hits = _grep_files(query, _dut_files(), max_files)
        return "No matching `.harc` fixture hits.\n\nDUT matches:\n" + _format_hits(dut_hits, False)
    seen: set[pathlib.Path] = set()
    snippets = []
    for hit in hits:
        if hit.path in seen:
            continue
        seen.add(hit.path)
        snippets.append(_snippet(hit.path, hit.line_no, context=28))
        if len(snippets) >= max_files:
            break
    return "\n\n".join(snippets)


@mcp.tool()
def harc_check(files: list[str], ast: bool = False, timeout: int = 30) -> str:
    """Run `harc check` on one or more `.harc` files and return diagnostics."""
    paths = [str(_resolve_safe(f)) for f in files]
    cmd = [HARC_BIN, "check"] + paths
    if ast:
        cmd.append("--ast")
    return _run(cmd, timeout=timeout)


@mcp.tool()
def harc_sim_emit_only(
    harc_files: list[str],
    sv_files: list[str] | None = None,
    dut_files: list[str] | None = None,
    top: str | None = None,
    test: str | None = None,
    outdir: str | None = None,
    ref_src: list[str] | None = None,
    timeout: int = 60,
) -> str:
    """Run `harc sim --emit-only` for codegen validation without full simulation.

    Pass exactly one backend: `sv_files` for Verilator-backed SV DUTs, or
    `dut_files` for ARCH-authored DUTs. Use full `harc sim` outside this MVP
    when the user explicitly wants to run the simulation.
    """
    if bool(sv_files) == bool(dut_files):
        return "[ERROR] Pass exactly one of sv_files or dut_files."

    resolved_harc_files = [_resolve_safe(f) for f in harc_files]
    resolved_sv_files = [_resolve_safe(f) for f in sv_files or []]
    resolved_dut_files = [_resolve_safe(f) for f in dut_files or []]
    resolved_ref_src = [_resolve_safe(f) for f in ref_src or []]

    cmd = [HARC_BIN, "sim"]
    for f in resolved_sv_files:
        cmd += ["--sv", str(f)]
    for f in resolved_dut_files:
        cmd += ["--dut", str(f)]
    for f in resolved_ref_src:
        cmd += ["--ref-src", str(f)]
    cmd += [str(f) for f in resolved_harc_files]
    if top:
        cmd += ["--top", top]
    if test:
        cmd += ["--test", test]
    if outdir:
        cmd += ["--outdir", str(_resolve_safe(outdir))]
    else:
        cmd += ["--outdir", tempfile.mkdtemp(prefix="harc_mcp_emit_")]
    cmd.append("--emit-only")
    arch_bin = os.environ.get("ARCH_BIN")
    if arch_bin and dut_files:
        cmd += ["--arch-bin", arch_bin]
    cwd = _owning_root(resolved_harc_files[0]) if resolved_harc_files else WORKSPACE_ROOTS[0]
    return _run(cmd, timeout=timeout, cwd=cwd)


@mcp.tool()
def harc_advise(query: str, top: int = 3) -> str:
    """Retrieve past HARC error-to-fix pairs from the local learning store."""
    top = max(1, min(top, 20))
    return _run([HARC_BIN, "advise", "-k", str(top), query])


@mcp.tool()
def harc_graph_index(paths: list[str], out: str = ".harcgraph", timeout: int = 60) -> str:
    """Build the compiler-native JSONL graph index for HARC/DUT paths."""
    resolved_paths = [_resolve_safe(path) for path in paths]
    out_path = _resolve_safe(out)
    cmd = [HARC_BIN, "graph", "index", *[str(path) for path in resolved_paths], "--out", str(out_path)]
    cwd = _owning_root(resolved_paths[0]) if resolved_paths else WORKSPACE_ROOTS[0]
    return _run(cmd, timeout=timeout, cwd=cwd)


@mcp.tool()
def harc_graph_query(query: str, index: str = ".harcgraph", limit: int = 20, timeout: int = 30) -> str:
    """Search graph nodes and edges for a symbol or text query."""
    limit = max(1, min(limit, 80))
    index_path = _resolve_safe(index)
    return _run([HARC_BIN, "graph", "query", query, "--index", str(index_path), "--limit", str(limit)], timeout=timeout)


@mcp.tool()
def harc_graph_context(task: str, index: str = ".harcgraph", limit: int = 20, timeout: int = 30) -> str:
    """Return a compact graph context slice for a task description."""
    limit = max(1, min(limit, 80))
    index_path = _resolve_safe(index)
    return _run([HARC_BIN, "graph", "context", task, "--index", str(index_path), "--limit", str(limit)], timeout=timeout)


@mcp.tool()
def harc_graph_impact(symbol: str, index: str = ".harcgraph", depth: int = 2, limit: int = 40, timeout: int = 30) -> str:
    """Return a bounded dependency/impact slice around a graph symbol."""
    depth = max(1, min(depth, 8))
    limit = max(1, min(limit, 120))
    index_path = _resolve_safe(index)
    return _run(
        [HARC_BIN, "graph", "impact", symbol, "--index", str(index_path), "--depth", str(depth), "--limit", str(limit)],
        timeout=timeout,
    )


@mcp.tool()
def harc_graph_tests_for_dut(symbol: str, index: str = ".harcgraph", limit: int = 30, timeout: int = 30) -> str:
    """List tests that reference a DUT, type, or symbol."""
    limit = max(1, min(limit, 100))
    index_path = _resolve_safe(index)
    return _run([HARC_BIN, "graph", "tests-for", symbol, "--index", str(index_path), "--limit", str(limit)], timeout=timeout)


@mcp.tool()
def harc_graph_examples_for(query: str, index: str = ".harcgraph", limit: int = 20, timeout: int = 30) -> str:
    """Find compact graph-backed examples for a feature or construct query."""
    limit = max(1, min(limit, 80))
    index_path = _resolve_safe(index)
    return _run([HARC_BIN, "graph", "query", query, "--index", str(index_path), "--limit", str(limit)], timeout=timeout)


@mcp.tool()
def list_harc_files(directory: str = ".") -> str:
    """List `.harc` files under an allowed workspace directory."""
    resolved = _resolve_safe(directory)
    if not resolved.is_dir():
        return f"[ERROR] Not a directory: {directory}"
    files = sorted(resolved.rglob("*.harc"))
    return "\n".join(_display_path(f) for f in files) if files else "(no .harc files found)"


if __name__ == "__main__":
    mcp.run(transport="stdio")
