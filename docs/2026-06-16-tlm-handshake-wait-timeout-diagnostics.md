# HARC: TLM/handshake BFM wait loops fall through silently on timeout

**Date:** 2026-06-16
**Status:** ✅ Implemented (2026-06-17) — see *Resolution* at the end. Codegen quality, no language-surface change.
**Related:** harc-com#413 (blocking TLM fork ready-wait), harc-com#407 (OOO initiator handshake), harc-com#414

---

## Summary

The generated C++ testbench drives every `tlm_method` request/response handshake
(and the `send`/`recv` handshake channels) with a **bounded** spin loop of the
shape:

```cpp
{ int _b = 16; while (!dut->mem_read_req_ready && _b > 0) { co_await harc_rt::wait_cycles(_slot, 1); _b--; } }
```

When the bound `_b` reaches `0` the loop simply exits and the BFM **proceeds as if
the handshake completed** — it deasserts `req_valid`, then (on the response side)
reads `rsp_data` whatever its current value is. There is no diagnostic. A genuine
protocol hang (the DUT never asserts `req_ready`/`rsp_valid` — an RTL deadlock, a
mis-wired bind-remap, or a back-pressure bug) is therefore **invisible at its
cause** and only shows up downstream as a wrong-value assertion failure, e.g.

```
FAIL: forked blocking read got 0x00000000, expected 0x00000105
```

The user then debugs the *consumer* assertion when the real fault was that the
request was never accepted.

## Where it occurs

15 bounded-wait sites across both emitters (`grep -rn "&& _b > 0" src/codegen/`):

- `src/codegen/cpp_tb.rs` — blocking + OOO `tlm_method` request (`req_ready`) and
  response (`rsp_valid`) waits, fork issue waits (the path #413 just restored for
  blocking forks), and `send`/`recv` handshake-channel ready/valid waits.
- `src/codegen/tbir/func.rs` — the mirrored TB-IR emissions.

Two bound values are in use: `_b = 16` for request-ready / handshake waits and
`_b = 64` for the `join_all` response-drain wait. The asymmetry is intentional
(a response may legitimately take longer to compute than a request takes to
accept) but neither bound emits anything when it expires.

## Why this matters for HARC

HARC's whole value proposition is that a fixture **self-tests at run time and
exits non-zero on failure**. A silent handshake timeout undermines that in the
exact scenario HARC is meant to catch — a DUT that stalls a transaction. The
failure still surfaces (the downstream value is wrong), but the signal points at
the wrong line, which is precisely the kind of confusing debug session
`--debug`-style instrumentation exists to prevent on the ARCH side.

This is also a *latent correctness risk for the test itself*: if a DUT is slow
but correct (accepts at cycle 17 for a request, or 65 for a response), the BFM
gives up early, drops `req_valid` mid-transaction, and can itself **create** a
`valid && !ready` protocol violation — the same class of bug #407/#413 were about.
The fixed 16/64 bounds are an implicit, undocumented latency ceiling.

## Proposed change

Low-risk, codegen-only. When a bounded wait expires, emit a structured failure
through the existing log/error channel instead of falling through silently:

```cpp
{ int _b = 16; while (!dut->mem_read_req_ready && _b > 0) { co_await harc_rt::wait_cycles(_slot, 1); _b--; }
  if (_b == 0 && !dut->mem_read_req_ready) {
      sim_log_line("FAIL", "TLM mem.read request not accepted after 16 cycles (req_ready stuck low)");
      ctx.errors++;
  }
}
```

Design points to settle before implementing:

1. **Severity.** `FAIL` (bump `ctx.errors`, fail the test) is the safe default —
   a stuck handshake is almost always a real bug. A `--tlm-wait-timeout-warn`
   escape hatch could downgrade to `WARN` for intentionally slow models, but the
   simpler first cut is hard-fail.
2. **Bound as a knob.** The 16/64 ceilings should become a single named constant
   (or a `--tlm-wait-bound N` CLI flag) so a legitimately slow DUT is not forced
   to hit the diagnostic. Today the ceiling is a magic number duplicated 15×.
3. **Shared helper.** The 15 sites should route through one emitter helper
   (`emit_bounded_wait(signal, bound, kind, label)`) rather than the current
   copy-pasted `while`/`co_await` pairs — this is the natural place to add the
   timeout diagnostic once and keep v1 and TB-IR in lockstep (they already
   diverged once, which #413 had to re-sync).

## Scope / non-goals

- **No language-surface or spec change.** This is purely the generated TB's
  runtime behavior; `.harc` source, `tlm_method` syntax, and handshake semantics
  are unchanged. No spec sign-off required.
- Keep the happy path byte-identical — the diagnostic only fires on the
  previously-silent timeout branch, so existing passing fixtures and their
  emitted-C++ snapshots change only by the added `if (_b == 0 ...)` tail.
- The snapshot tests (e.g. `tlm_blocking_fork_bus_emitted_cpp`) would need
  regeneration; that is the expected, reviewable cost.

## Suggested follow-up test

A dedicated fixture with a DUT that holds `req_ready` low forever (or longer than
the bound) would lock the new diagnostic *and* give the blocking-fork path the
end-to-end coverage it currently lacks — today `tlm_method_blocking_fork_bus_test`
is only snapshot-tested, and `TlmMemory.sv` drives `req_ready` high whenever no
response is pending, so a single uncontended fork never exercises the multi-cycle
wait that #413 fixed.

## Resolution (2026-06-17)

Implemented as proposed, codegen-only, with the design points settled as follows:

- **Severity: hard-`FAIL`.** Each expiry emits
  `sim_log_line("FAIL", "<label> timed out after N cycles (<signal> stuck low)")`
  and bumps `ctx.errors`, so a stalled DUT fails the run at its cause. No
  warn-only escape hatch in this cut — a stuck handshake is a real bug.
- **Bounds promoted to named constants** in `src/codegen/mod.rs`:
  `TLM_WAIT_BOUND = 16` (request / `send` ready / `recv` valid / non-fork
  response) and `TLM_JOIN_DRAIN_BOUND = 64` (the `join_all` forked-response
  drain). A `--tlm-wait-bound` CLI knob was **not** added — it is user surface
  and no fixture needs it yet; the named constants are the right first step and
  keep the change purely internal. Re-open if a legitimately slow model needs a
  larger budget without editing the source.
- **One shared helper**, `codegen::bounded_handshake_wait(signal, bound, advance,
  label)`, now backs all 15 former copy-paste sites across both emitters
  (`cpp_tb.rs` v1 and `tbir/func.rs`), so the two stay in lockstep. `advance` is
  `"co_await harc_rt::wait_cycles(_slot, 1)"` in a coroutine or `"tick()"`
  straight-line; `label` carries the source-level bus/channel name while the
  message also prints the actual stuck wire.

**Verification.** The happy path is byte-identical apart from the `if (_b == 0 …)`
tail (three emitted-C++ snapshots regenerated). The full Verilator fixture suite
(`tests/run_fixtures.sh`, 103 fixtures) and `trace_merge_blocking_and_ooo_tags_e2e`
stay green. The follow-up stall test landed as
`tests/tlm_wait_timeout_e2e.rs` + `tests/dut/TlmStallMemory.sv` +
`tests/fixtures/tlm_stall_timeout_test.harc`: a `read` target that never asserts
`req_ready`/`rsp_valid`, run under **both** `--codegen v1` and `tbir`, asserting
the FAIL diagnostic fires and the run exits non-zero. Confirmed under Verilator
5.048 — request times out at cycle 19, response at 36, run reports `2 TESTS FAILED`.
