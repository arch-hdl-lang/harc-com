#!/usr/bin/env python3
"""
One-time migrator for docs/test-ergonomics.md.

Collapses every `test T { ... } end test` + `impl sim for T { ... } end impl`
pair in a HARC fixture into a single inline-form `test T { ... } end test`
block, moving the impl's body items (run / setup / check / teardown / phase)
inside the matching test just before `end test`.

Per RFC §7.2 this is described as a `harc fmt --migrate-v1` tool; the
migration is genuinely one-shot, so a small Python script is enough.
The Phase 1 parser change accepts both forms, so this script's output
is validated by running tests/run_fixtures.sh after rewrite.

Edge cases handled:
- Multiple `test`/`impl` pairs in one file (axi_agent.harc).
- Trailing `end test <Name>` / `end impl <Name>` variants.
- Doc / inner-doc comments before each phase block are preserved.
- Blank line normalisation: collapses 3+ consecutive blank lines to 2.

Edge cases NOT handled (and currently absent from the corpus):
- Multiple impls for one test (e.g. `impl sim` + `impl emu`).
- Custom `phase <name>` blocks. Their body would migrate fine but the
  end-keyword `end phase <name>` needs matching; not in the corpus,
  so left for follow-up if it ever lands.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

TEST_HEADER = re.compile(r"^test\s+(\w+)\b", re.MULTILINE)
IMPL_HEADER = re.compile(r"^impl\s+sim\s+for\s+(\w+)\b", re.MULTILINE)
END_TEST    = re.compile(r"^end\s+test(?:\s+\w+)?\s*$",  re.MULTILINE)
END_IMPL    = re.compile(r"^end\s+impl(?:\s+\w+)?\s*$",  re.MULTILINE)


def find_block(text: str, header_re: re.Pattern, end_re: re.Pattern, start: int = 0):
    """Find the next (header_start, header_end_line, body_start, end_match, name)
    after `start`. Returns None when no header matches.
    """
    m = header_re.search(text, start)
    if not m:
        return None
    name = m.group(1)
    # Header line ends at the next newline after the match end.
    nl = text.find("\n", m.end())
    if nl < 0:
        return None
    body_start = nl + 1
    e = end_re.search(text, body_start)
    if not e:
        return None
    return (m.start(), m.end(), body_start, e, name)


def migrate(text: str) -> tuple[str, int]:
    """Migrate one file's contents. Returns (new_text, n_pairs_collapsed)."""
    # Find every impl sim for X block, look up matching test X, splice body.
    # Walk impls in reverse source order so earlier offsets stay valid as
    # we delete text. For each impl, find the matching test block and
    # inject the impl body just before its `end test`.
    impls: list[tuple[int, int, int, re.Match, str]] = []
    pos = 0
    while True:
        f = find_block(text, IMPL_HEADER, END_IMPL, pos)
        if not f:
            break
        impls.append(f)
        pos = f[3].end()
    if not impls:
        return text, 0

    # Index tests by name once. find_block iterates forward; tests is a
    # dict { name: (header_start, header_end_pos, body_start, end_match) }.
    # If a test name appears twice it's ambiguous — that doesn't happen
    # in the current corpus but we'd warn rather than silently mis-merge.
    tests: dict[str, tuple[int, int, int, re.Match]] = {}
    pos = 0
    while True:
        f = find_block(text, TEST_HEADER, END_TEST, pos)
        if not f:
            break
        h_start, h_end, b_start, end_m, name = f
        if name in tests:
            print(f"  WARN: duplicate test name `{name}` in file; ambiguous merge", file=sys.stderr)
        tests[name] = (h_start, h_end, b_start, end_m)
        pos = end_m.end()

    # Build a list of edits: (impl_start, impl_end, test_insert_pos, body).
    # impl_end is the byte just after the impl's `end impl` line + trailing newline.
    edits: list[tuple[int, int, int, str]] = []
    n = 0
    for h_start, h_end, b_start, end_m, name in impls:
        if name not in tests:
            print(f"  WARN: no matching `test {name}` for `impl sim for {name}`", file=sys.stderr)
            continue
        t_h_start, t_h_end, t_b_start, t_end_m = tests[name]

        # Strip the impl body's trailing blank lines so we don't carry
        # excess blank lines into the test.
        impl_body = text[b_start:end_m.start()]
        impl_body = impl_body.rstrip() + "\n"

        # Where to insert in the test: just before `end test` (start of
        # that line).
        insert_at = t_end_m.start()

        # Where to delete: from the impl header's line start to the
        # newline after `end impl`.
        impl_line_start = text.rfind("\n", 0, h_start) + 1
        impl_end_line_end = text.find("\n", t_end_m.end())  # placeholder
        impl_end_line_end = text.find("\n", end_m.end())
        if impl_end_line_end < 0:
            impl_end_line_end = len(text)
        else:
            impl_end_line_end += 1  # include trailing newline

        edits.append((impl_line_start, impl_end_line_end, insert_at, impl_body))
        n += 1

    # Apply edits highest-offset-first. Inserts happen at offsets that
    # are unaffected by later (earlier-offset) deletions — guaranteed
    # because impls always appear AFTER their matching tests in
    # well-formed sources (current corpus invariant).
    # We apply by reverse-sorting by the deletion offset; within that,
    # the insertion offset is always smaller, so insertions also stay
    # valid relative to remaining text.
    edits.sort(key=lambda e: e[0], reverse=True)
    parts = text
    for impl_start, impl_end, insert_at, body in edits:
        # Indent the impl body so the phase keywords sit at the test's
        # body indentation. Inspect the test by-line to detect indent.
        # Heuristic: keep current indentation; existing impl bodies
        # already use 4-space indent matching test bodies.
        snippet = "\n" + body.rstrip() + "\n"
        # Delete impl block first (higher offset).
        parts = parts[:impl_start] + parts[impl_end:]
        # Insert into test. insert_at unaffected (lower offset).
        parts = parts[:insert_at] + snippet + parts[insert_at:]

    # Collapse 3+ consecutive blank lines to 2.
    parts = re.sub(r"\n{4,}", "\n\n\n", parts)
    # Trim trailing whitespace on each line.
    parts = re.sub(r"[ \t]+\n", "\n", parts)
    return parts, n


def main(argv: list[str]) -> int:
    paths = [Path(p) for p in argv[1:]]
    if not paths:
        # Default: all .harc fixtures.
        paths = sorted(Path("tests/fixtures").glob("*.harc"))
    total = 0
    for p in paths:
        if not p.exists():
            print(f"  SKIP missing: {p}", file=sys.stderr)
            continue
        original = p.read_text()
        new, n = migrate(original)
        if n == 0:
            continue
        p.write_text(new)
        total += n
        print(f"  migrated {n} pair(s)  {p}")
    print(f"Total: {total} impl-block(s) collapsed across {len(paths)} files.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
