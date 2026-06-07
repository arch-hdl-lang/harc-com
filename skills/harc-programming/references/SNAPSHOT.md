# HARC Skill Reference Snapshot

These reference files are copied from the same `harc-com` repository commit
that contains this skill folder. They are bundled so users can install
`skills/harc-programming` into `~/.codex/skills` and still have core HARC
guidance available outside a repo checkout.

## Bundled Sources

- `README.md` from the repository root
- `spec.md` from the repository root
- `docs/harc-sim-cli.md`
- `docs/semantic-trace.md`
- `docs/ral-support.md`
- `docs/tb-ir-design.md`
- `docs/test-ergonomics.md`
- `LICENSE` from the repository root

## Link Notes

The copied markdown keeps upstream prose intact. Some links still point to
repo-local paths such as `tests/fixtures/`, `tests/dut/`, `docs/...`, or
helper scripts. Those links require a local `harc-com` checkout; they are not
all bundled inside the installed skill.

For runnable examples, prefer the live checkout's `tests/fixtures/` directory
or the HARC MCP `harc_examples()` tool when available.

## Refresh

When HARC docs change, refresh this snapshot from the repository root:

```sh
cp README.md skills/harc-programming/references/README.md
cp spec.md skills/harc-programming/references/spec.md
cp docs/harc-sim-cli.md skills/harc-programming/references/harc-sim-cli.md
cp docs/semantic-trace.md skills/harc-programming/references/semantic-trace.md
cp docs/ral-support.md skills/harc-programming/references/ral-support.md
cp docs/tb-ir-design.md skills/harc-programming/references/tb-ir-design.md
cp docs/test-ergonomics.md skills/harc-programming/references/test-ergonomics.md
cp LICENSE skills/harc-programming/references/LICENSE
```
