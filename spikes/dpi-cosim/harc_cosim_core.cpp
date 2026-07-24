// Simulator-neutral HARC co-sim TB core (spec §10 DPI-C co-sim pilot).
//
// This is a hand-written stand-in for what `harc -emit`'s co-sim backend
// would generate: the run block of tests/fixtures/sync_fifo_test.harc,
// lowered to a C++20 coroutine scheduled by the real production
// scheduler (runtime/harc_thread_rt.h — the same header the direct
// Verilator backend emits against).
//
// The inversion relative to the direct backend: there is no main() and
// no drive_loop here. The HDL simulator owns time and calls in through
// three C entrypoints (contract points 1 and 4 in spec §10):
//
//   harc_init()        — elaboration/time-zero. Constructs the test
//                        coroutine and runs ThreadScheduler::bootstrap()
//                        so setup drives (reset, input defaults) land
//                        before the first posedge.
//   harc_on_posedge()  — once per clock cycle, called at a point in the
//                        time step where post-posedge state is stable
//                        (the harness uses the negedge event). Runs
//                        ThreadScheduler::tick(): the direct backend's
//                        mid-cycle coroutine quantum. Returns nonzero
//                        when the test is finished (all coroutines Done,
//                        or a fatal failure) so the harness can $finish.
//   harc_finish()      — end-of-sim report. Prints the run_fixtures.sh
//                        pass marker ("ALL TESTS PASSED") or the failure
//                        summary.
//
// Signal access crosses the boundary through the two-function adapter
// ABI below (contract point 3: typed accessors, no hierarchical strings
// in TB code). The per-simulator adapter provides:
//
//   harc_dut_get(id)      — read a DUT port
//   harc_dut_set(id, val) — drive a DUT input
//
// On Verilator the adapter forwards to SV functions exported over DPI-C
// (dpi_adapter.cpp); on Icarus it forwards to vpi_get_value /
// vpi_put_value (vpi_adapter.c). This file is compiled unchanged into
// both builds — that portability is the point of the spike.

#include <chrono>
#include <cinttypes>
#include <cstdint>
#include <cstdio>
#include <cstdlib>

#include "harc_thread_rt.h"
#include "harc_cosim_sig_ids.h"

extern "C" {
// Provided by the per-simulator signal-access adapter.
uint64_t harc_dut_get(int sig_id);
void harc_dut_set(int sig_id, uint64_t value);
// Entrypoints the simulator side calls (DPI imports / VPI system tasks).
void harc_init(void);
int harc_on_posedge(void);
void harc_finish(void);
}

namespace {

harc_rt::ThreadSlot g_run_slot;
harc_rt::ThreadScheduler g_sched;
bool g_fatal = false;
int g_fail_count = 0;
uint64_t g_cycle_count = 0;
std::chrono::steady_clock::time_point g_t0;

uint64_t get(int id) { return harc_dut_get(id); }
void set(int id, uint64_t v) { harc_dut_set(id, v); }

void fail(const char* msg) {
    std::fprintf(stderr, "FATAL: %s\n", msg);
    ++g_fail_count;
    g_fatal = true;
}

void log_info(const char* msg) { std::printf("[info] %s\n", msg); }

// The run block of impl SyncFifoTest, hand-lowered. `wait N cycles`
// becomes `co_await harc_rt::wait_cycles(slot, N)` exactly as the TB-IR
// backend emits it; `dut.<port>` reads/writes become adapter calls where
// the direct backend emits Verilated-model member access.
harc_rt::HarcThread run_test(harc_rt::ThreadSlot* slot) {
    // Reset.
    set(HARC_SIG_RST, 1);
    set(HARC_SIG_PUSH_VALID, 0);
    set(HARC_SIG_POP_READY, 0);
    set(HARC_SIG_PUSH_DATA, 0);
    co_await harc_rt::wait_cycles(slot, 2);

    if (get(HARC_SIG_EMPTY) != 1) { fail("after reset, empty should be 1"); co_return; }
    if (get(HARC_SIG_FULL) != 0) { fail("after reset, full should be 0"); co_return; }
    log_info("PASS: reset -> empty FIFO");

    set(HARC_SIG_RST, 0);

    // Push 16 items (fill to capacity).
    for (uint64_t i = 0; i <= 15; ++i) {
        set(HARC_SIG_PUSH_VALID, 1);
        set(HARC_SIG_PUSH_DATA, i + 1);
        set(HARC_SIG_POP_READY, 0);
        if (get(HARC_SIG_PUSH_READY) != 1) { fail("FIFO unexpectedly full during fill"); co_return; }
        co_await harc_rt::wait_cycles(slot, 1);
    }
    set(HARC_SIG_PUSH_VALID, 0);
    co_await harc_rt::wait_cycles(slot, 1);

    if (get(HARC_SIG_FULL) != 1) { fail("FIFO should be full after 16 pushes"); co_return; }
    if (get(HARC_SIG_PUSH_READY) != 0) { fail("push_ready should be 0 when full"); co_return; }
    log_info("PASS: FIFO full after 16 pushes");

    // Pop all 16 items and verify FIFO order.
    for (uint64_t i = 0; i <= 15; ++i) {
        set(HARC_SIG_POP_READY, 1);
        if (get(HARC_SIG_POP_VALID) != 1) { fail("pop_valid unexpectedly low during drain"); co_return; }
        uint64_t expected = i + 1;
        if (get(HARC_SIG_POP_DATA) != expected) {
            std::fprintf(stderr, "FATAL: pop %" PRIu64 ": got %" PRIu64 ", expected %" PRIu64 "\n",
                         i, get(HARC_SIG_POP_DATA), expected);
            ++g_fail_count;
            g_fatal = true;
            co_return;
        }
        co_await harc_rt::wait_cycles(slot, 1);
    }
    set(HARC_SIG_POP_READY, 0);
    co_await harc_rt::wait_cycles(slot, 1);

    if (get(HARC_SIG_EMPTY) != 1) { fail("FIFO should be empty after popping all items"); co_return; }
    log_info("PASS: 16 items popped in FIFO order, empty after drain");

    // Optional throughput soak (HARC_COSIM_SOAK=<cycles>): saturating
    // push+pop stream for measuring the per-cycle boundary-crossing cost
    // of each co-sim flavor. 6 adapter calls per cycle, comparable to a
    // small real TB's per-cycle signal traffic.
    if (const char* soak_env = std::getenv("HARC_COSIM_SOAK")) {
        uint64_t soak = std::strtoull(soak_env, nullptr, 10);
        uint64_t popped = 0;
        for (uint64_t i = 0; i < soak; ++i) {
            set(HARC_SIG_PUSH_VALID, 1);
            set(HARC_SIG_PUSH_DATA, i & 0xff);
            set(HARC_SIG_POP_READY, 1);
            if (get(HARC_SIG_POP_VALID) == 1) ++popped;
            co_await harc_rt::wait_cycles(slot, 1);
        }
        set(HARC_SIG_PUSH_VALID, 0);
        set(HARC_SIG_POP_READY, 0);
        std::printf("[soak] %" PRIu64 " cycles, %" PRIu64 " pops observed\n", soak, popped);
    }
}

} // namespace

void harc_init(void) {
    g_t0 = std::chrono::steady_clock::now();
    g_run_slot.thread = run_test(&g_run_slot);
    g_sched.slots.push_back(&g_run_slot);
    // Run setup statements (reset drive, input defaults) to the first
    // wait, before the simulator's first posedge — same ordering the
    // direct backend gets by calling bootstrap() before its drive loop.
    g_sched.bootstrap();
}

int harc_on_posedge(void) {
    ++g_cycle_count;
    static const bool dbg = std::getenv("HARC_COSIM_DEBUG") != nullptr;
    if (dbg) {
        std::printf(
            "[dbg] cyc=%" PRIu64 " rst=%" PRIu64 " pv=%" PRIu64 " pd=%" PRIu64
            " pr=%" PRIu64 " | prdy=%" PRIu64 " povld=%" PRIu64 " pod=%" PRIu64
            " full=%" PRIu64 " empty=%" PRIu64 "\n",
            g_cycle_count, get(HARC_SIG_RST), get(HARC_SIG_PUSH_VALID),
            get(HARC_SIG_PUSH_DATA), get(HARC_SIG_POP_READY),
            get(HARC_SIG_PUSH_READY), get(HARC_SIG_POP_VALID),
            get(HARC_SIG_POP_DATA), get(HARC_SIG_FULL), get(HARC_SIG_EMPTY));
    }
    g_sched.tick();
    return (g_sched.all_done() || g_fatal) ? 1 : 0;
}

void harc_finish(void) {
    auto dt = std::chrono::duration<double>(std::chrono::steady_clock::now() - g_t0).count();
    std::printf("[cosim] %" PRIu64 " cycles in %.3fs (%.0f cycles/s)\n",
                g_cycle_count, dt, dt > 0 ? (double)g_cycle_count / dt : 0.0);
    if (g_fail_count == 0) {
        std::printf("ALL TESTS PASSED\n");
    } else {
        std::printf("TEST FAILED: %d failure(s)\n", g_fail_count);
    }
    g_run_slot.thread.destroy();
}
