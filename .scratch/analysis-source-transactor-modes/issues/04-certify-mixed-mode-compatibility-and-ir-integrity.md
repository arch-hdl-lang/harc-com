# 04 — Certify mixed-mode compatibility and IR integrity

**What to build:** Make mixed-mode analysis-source transactors diagnosable and regression-safe across the complete compiler pipeline. The IR exposes mode and activation decisions, malformed metadata is rejected, and v1/TB-IR behavior remains equivalent in cooperative and multithreaded execution.

**Parent:** https://github.com/arch-hdl-lang/harc-com/issues/534

**Blocked by:** 01 — Make direct analysis-source transactor bindings mode-correct; 02 — Gate `when active` surfaces by effective mode; 03 — Preserve nested mode inheritance and overrides.

**Status:** ready-for-agent

- [ ] The verifier rejects missing effective modes, illegal structural modes, invalid active-surface ownership, duplicate or dangling field/function references, and activation-incompatible connect metadata.
- [ ] Dump-IR identifies binding modes, root inheritance context, and always versus active-only members.
- [ ] Shared-schema function definitions are emitted once while per-instance registrations remain mode-gated.
- [ ] The new fixture is registered for v1/TB-IR cooperative trace equivalence at a fixed seed.
- [ ] The new fixture passes both codegens under `--mt` with verdict-level checking.
- [ ] Existing testbench-owned analysis fanout, passive transactor, mode inheritance, and active bound-driver regressions remain green.
- [ ] The full documented verification sequence passes before the issue is considered resolved.
