# HARC: dual-backend equivalence gap and the SFG connection

**Date:** 2026-05-28
**Status:** Observation — cross-reference to arch-com enhancement proposal
**Related:** arch-com#244 (dual-backend equivalence test suite), arch-com#437 (native sim vs Verilator timing disagreement)

---

## Summary

This note captures a gap between HARC's simulation harness and arch-com's native C++
sim backend that was surfaced by arch-com#437, and explains how it connects to the
signal flow graph (SFG) enhancement proposed in `arch-com/ideas/2026-05-28-signal-flow-graph.md`.

---

## The observed gap

arch-com#437 reports that the ARCH native C++ sim (`arch sim`) disagrees with the
Verilator/HARC path (`harc sim --dut ... --sv`) for the same `SoftmaxEngine` RTL:

- HARC/Verilator path: `PASS` — first weight output is correct (76592).
- ARCH native sim path: `FAIL` — first weight output is 0.

The hypothesis in #437 is a "registered output timing/alignment issue" in the
thread-lowered sub-module's C++ sim model. This is consistent with arch-com#306
(thread `wait until cond; X <= Y;` lands one cycle late) and with the 13 sim
backend bugs fixed in arch-com#241 (all latent before a deliberate dual-backend
comparison was done).

## Why HARC needs this fixed

HARC tests exercise a design via `harc sim --sv` (Verilator path) or
`harc sim --dut --sv` (ARCH-native C++ sim path). If the two backends disagree on
timing, HARC tests that pass on one backend may silently fail to catch RTL bugs on
the other. More concretely:

- A HARC fixture that only runs under Verilator will not catch bugs that only the
  native sim exposes (and vice versa).
- The `--dut` (ARCH native sim) path is faster and requires no Verilator install,
  but it currently has latent correctness differences from the gold Verilator model.

## Proposed HARC-side addition: `--check-backends` smoke flag

Once arch-com's SFG (signal flow graph) pass exists, it can identify registered
outputs whose thread is the sole writer (the "sole-writer query" from the SFG
proposal). That information makes it straightforward to construct a minimal
stimulus that exercises the first-output boundary — exactly the scenario that
#437 exposed.

A new `harc sim --check-backends` flag could:

1. Run the testbench under both Verilator and the ARCH native C++ sim.
2. Apply the same stimulus (from the HARC test, or an auto-generated LFSR sweep).
3. Compare per-cycle outputs byte-for-byte.
4. Report any divergence with the cycle number and signal name.

This is the same idea as arch-com#244 (dual-backend equivalence test suite) but
surfaced as a first-class HARC flag rather than a separate test framework.

### Minimum viable version

Before the SFG exists, a simpler version is still useful:

```
harc sim --sv <dut.sv> <test.harc> --check-backends
```

Runs the test twice — once via Verilator, once via `arch sim` with the same TB —
and diffs the per-cycle port vectors. No SFG needed; the test harness already
captures per-cycle output via the trace machinery added in PR #303–#307.

### Scope estimate

| Sub-task | Estimate |
|----------|----------|
| Re-run harness with `arch sim` backend and capture per-cycle port vectors | 1–2 days |
| Byte-identical comparison + diff report | 0.5 days |
| Integration with `run_arch_dut_fixtures.sh` | 0.5 days |
| **Total** | **~3 days** |

## Connection to the arch-com SFG proposal

The SFG's sole-writer query (Check 3 in the arch-com proposal) tells the lowering
pass when it's safe to exit-fold a thread state — which would fix the #437 timing
disagreement at the compiler level rather than the test level. That fix lands in
arch-com. The `--check-backends` HARC addition is a regression net that catches
future divergences before they reach users, regardless of which side introduces them.

The two are complementary: the arch-com fix eliminates a known category of sim
divergence; the HARC check detects any future divergence at the earliest point in
the workflow.

---

## Action items

- [ ] **arch-com** — implement SFG + thread sole-writer query (arch-com proposal)
- [ ] **arch-com** — apply exit-fold optimization for #306, which also fixes #437 class of bugs
- [ ] **harc-com** — add `harc sim --check-backends` as a minimum-viable dual-backend check
- [ ] **harc-com** — add a fixture that covers the first-output boundary scenario from #437
      (a design with a thread-lowered registered output, checked on both backends)
