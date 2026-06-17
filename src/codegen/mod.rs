//! Backends. Today: only `cpp_tb` (Verilator-class C++ TB harness emitter).
//!
//! Future per spec §10: `sv_uvm` (transpile to SV+UVM, phase 5),
//! `formal` (BTOR2 / SMT-LIB2 export, phase 4), and a real Phase 1a native
//! runtime that lowers `tseq` to coroutines instead of straight-line C++.

pub mod cpp_tb;
pub mod merge;
pub mod sv_stub;
pub mod tbir;

/// Default cycle budget for an inline handshake wait in a generated BFM:
/// a `tlm_method` `req_ready`/`rsp_valid` wait on the blocking call path, a
/// blocking-fork `req_ready` wait, a `send` channel's `ready`, and a `recv`
/// channel's `valid`.
pub(crate) const TLM_WAIT_BOUND: u32 = 16;
/// Cycle budget for the forked-response drain at `join_all` (`rsp_valid`). A
/// response may legitimately take longer to compute than a request takes to
/// accept, so the drain gets a larger budget than [`TLM_WAIT_BOUND`].
pub(crate) const TLM_JOIN_DRAIN_BOUND: u32 = 64;

/// Emit a single-line bounded handshake-wait loop **with a timeout
/// diagnostic** for a generated C++ testbench BFM.
///
/// `signal` is the full C++ boolean lvalue to spin on (e.g.
/// `dut->mem_read_req_ready`). `advance` is the per-iteration advance
/// statement WITHOUT a trailing `;` — `"co_await harc_rt::wait_cycles(_slot, 1)"`
/// inside a coroutine, `"tick()"` in straight-line code. `label` names the
/// stalled handshake for the failure message (e.g. `"TLM mem.read request"`).
///
/// Historically these loops fell through **silently** when the bound expired,
/// so a stalled DUT (an RTL deadlock, a mis-wired bind-remap, back-pressure
/// bug, or a slow-but-correct model that exceeds the budget) surfaced only as
/// a confusing downstream wrong-value assertion. On expiry this now emits a
/// structured `FAIL` and bumps `ctx.errors`, mirroring the assertion idiom.
/// The happy path (handshake completes within the bound) is byte-identical
/// to the old emission apart from the added `if (_b == 0 ...)` tail, so
/// passing fixtures are unaffected.
///
/// `ctx.errors` and the `sim_log_line` lambda are in scope at every BFM
/// emission site (the test-run / worker coroutines capture `[&]`, and the
/// straight-line path runs inside the same run function).
pub(crate) fn bounded_handshake_wait(
    signal: &str,
    bound: u32,
    advance: &str,
    label: &str,
) -> String {
    format!(
        "{{ int _b = {bound}; while (!{signal} && _b > 0) {{ {advance}; _b--; }} \
         if (_b == 0 && !{signal}) {{ \
         sim_log_line(\"FAIL\", \"{label} timed out after {bound} cycles \
         ({signal} stuck low)\"); ctx.errors++; }} }}"
    )
}
