// HARC co-sim runtime (spec §10 DPI-C co-sim pilot, `harc sim --cosim dpi`).
//
// In co-sim mode the HDL simulator owns time and the generated TB is a
// passive library entered through two DPI-C imports declared in the
// generated `HarcCosimTop.sv` harness:
//
//   harc_cosim_init()  — time zero: select the test (HARC_TEST env).
//   harc_cosim_step()  — run the TB until its next time request; the
//                        returned code tells the harness's master
//                        process what to do next (see protocol below).
//
// The TB body (`run_<Test>`) executes on a dedicated OS thread under a
// STRICT handshake with the simulator thread: exactly one of the two is
// ever runnable, exchanged through Bridge::yield_to_sim /
// Bridge::run_until_request. The simulator thread is parked inside the
// `harc_cosim_step` DPI import for the entire time the TB thread runs,
// so every TB-side access to simulator state still happens "inside a
// DPI entrypoint" by delegation — there is no concurrent access by
// construction. This is the same bridge shape cocotb/SystemC use, and
// it is what lets the direct backend's synchronous cycle-advance paths
// (helper functions with `wait`, blocking TLM/bus calls lowered to
// `tick()` loops) work unchanged under co-sim: any plain function can
// block on the bridge; no coroutine transform is needed.
//
// Signal access crosses the boundary through two DPI-C *exports* the
// harness provides (`harc_sv_get` / `harc_sv_set`), reached from TB
// code via the id-keyed `SigProxy` members of the generated DUT shim
// struct. A proxy write lands as an ordinary SV variable assignment
// inside the exported function, so the simulator schedules downstream
// re-evaluation itself — the reason this is sound where raw
// `--public-flat-rw` memory pokes are not.
#pragma once

#include <condition_variable>
#include <cstdint>
#include <cstdlib>
#include <mutex>
#include <thread>

#include "svdpi.h"
#include "verilated.h"

extern "C" {
// Provided by the generated SV harness via `export "DPI-C"`.
long long harc_sv_get(int sig_id);
void harc_sv_set(int sig_id, long long value);
// Word-granular accessors for ports wider than 64 bits (32-bit word
// `word` of port `sig_id`, LSB-first — matching Verilator's VlWide
// word order, which the wide helpers in harc_thread_rt.h assume).
long long harc_sv_get_word(int sig_id, int word);
void harc_sv_set_word(int sig_id, int word, long long value);
// Element accessors for unpacked-array ports (`input logic [W-1:0]
// p [N]`): element `idx` of port `sig_id`, elements <= 64 bits.
long long harc_sv_get_elem(int sig_id, int idx);
void harc_sv_set_elem(int sig_id, int idx, long long value);
}

namespace harc_rt {
namespace cosim {

// harc_cosim_step() → harness master-process protocol:
//   > 0  — advance simulation time by that many picoseconds, then step
//          again. Used to reach the next clock edge.
//   == RC_SETTLE — advance exactly 1 ps ("settle"): lets the simulator
//          apply NBA updates and re-settle combinational logic after TB
//          drives / a clock-edge write, before the TB reads.
//   RC_DONE_PASS / RC_DONE_FAIL — test finished; harness $finishes
//          (or $fatal's, so the process exit status reflects failure).
inline constexpr long long RC_SETTLE = 0;
inline constexpr long long RC_DONE_PASS = -1;
inline constexpr long long RC_DONE_FAIL = -2;

struct Bridge {
    std::mutex m;
    std::condition_variable cv;
    // Exactly one side runs at a time; `turn_tb` says whose turn it is.
    bool turn_tb = false;
    bool started = false;
    bool done = false;
    int exit_code = 1;
    long long request = RC_SETTLE;
    std::thread tb_thread;
    int (*body)() = nullptr;

    // Picosecond bookkeeping for the implicit clock's edge grid.
    // Settle steps consume real picoseconds between edges; the grid
    // stays nominal by advancing to `next_edge` rather than by a fixed
    // delta.
    long long now_ps = 0;
    long long next_edge_ps = 0;
    long long half_period_ps = 5000;

    // Captured on the simulator thread inside the context import, then
    // installed on the TB thread: Verilator resolves the DPI export
    // trampolines through thread-local context/scope, and the TB thread
    // calls the exports while the simulator thread is parked in
    // harc_cosim_step().
    VerilatedContext* vl_ctx = nullptr;
    svScope vl_scope = nullptr;

    // ── TB-thread side ──────────────────────────────────────────────
    // Hand the given request to the simulator and block until the
    // harness calls harc_cosim_step() again.
    void yield_to_sim(long long rc) {
        std::unique_lock<std::mutex> lk(m);
        request = rc;
        turn_tb = false;
        cv.notify_all();
        cv.wait(lk, [&] { return turn_tb; });
    }

    void settle() {
        yield_to_sim(RC_SETTLE);
        now_ps += 1;
    }

    void advance_to_next_edge() {
        next_edge_ps += half_period_ps;
        if (next_edge_ps > now_ps) {
            long long d = next_edge_ps - now_ps;
            yield_to_sim(d);
            now_ps = next_edge_ps;
        }
    }

    // ── simulator-thread side ───────────────────────────────────────
    // Wake the TB thread (starting it on first call) and block until it
    // either yields a request or finishes. Returns the protocol rc.
    long long run_until_request() {
        std::unique_lock<std::mutex> lk(m);
        if (done) return exit_code == 0 ? RC_DONE_PASS : RC_DONE_FAIL;
        if (!started) {
            started = true;
            int (*body_fn)() = body;
            Bridge* self = this;
            tb_thread = std::thread([self, body_fn] {
                {
                    std::unique_lock<std::mutex> lk2(self->m);
                    self->cv.wait(lk2, [&] { return self->turn_tb; });
                }
                // Verilator's export trampolines resolve context and
                // scope through thread-locals — install the ones
                // captured inside the context import.
                if (self->vl_ctx) Verilated::threadContextp(self->vl_ctx);
                if (self->vl_scope) svSetScope(self->vl_scope);
                int rc = body_fn ? body_fn() : 1;
                {
                    std::unique_lock<std::mutex> lk2(self->m);
                    self->exit_code = rc;
                    self->done = true;
                    self->turn_tb = false;
                    self->cv.notify_all();
                }
            });
        }
        turn_tb = true;
        cv.notify_all();
        cv.wait(lk, [&] { return !turn_tb; });
        if (done) {
            lk.unlock();
            if (tb_thread.joinable()) tb_thread.join();
            return exit_code == 0 ? RC_DONE_PASS : RC_DONE_FAIL;
        }
        return request;
    }
};

inline Bridge& bridge() {
    // Intentionally leaked: if the simulation ends while the TB thread
    // is parked in yield_to_sim (e.g. a DUT-initiated $finish), a
    // static Bridge's destructor would destroy a joinable std::thread
    // and std::terminate the process — SIGABRT instead of a
    // diagnosable result. Leaking the singleton makes every exit path
    // safe; the normal pass/fail path still joins the TB thread in
    // run_until_request.
    static Bridge* b = new Bridge;
    return *b;
}

// One DUT port in the generated shim struct. Reads and writes forward to
// the harness's exported accessors; the conversion operator makes reads
// usable in any arithmetic/logical/format-arg position the direct
// backend's Verilated member access supports for <= 64-bit ports.
template <int ID>
struct SigProxy {
    operator uint64_t() const { return static_cast<uint64_t>(harc_sv_get(ID)); }
    SigProxy& operator=(uint64_t v) {
        harc_sv_set(ID, static_cast<long long>(v));
        return *this;
    }
    SigProxy& operator=(const SigProxy& o) { return *this = static_cast<uint64_t>(o); }
};

// One 32-bit word of a wide port, returned by WideSigProxy::operator[].
// Reads and writes forward to the word-granular accessors, so the wide
// helpers' read-modify-write loops (`sig[i] = w`) land as SV part-select
// assignments in the harness.
struct WideWordRef {
    int id;
    int word;
    operator uint32_t() const {
        return static_cast<uint32_t>(harc_sv_get_word(id, word));
    }
    WideWordRef& operator=(uint32_t v) {
        harc_sv_set_word(id, word, static_cast<long long>(v));
        return *this;
    }
};

// A DUT port wider than 64 bits. Mimics the surface of Verilator's
// `VlWide<NWORDS>` that the wide helpers in harc_thread_rt.h rely on:
// indexable with `[]` LSB-word-first, and `sizeof(Sig)` equal to
// `NWORDS * sizeof(uint32_t)` (the `_shape` member exists only to make
// the sizeof-derived word count come out right — it is never read).
template <int ID, int NWORDS>
struct WideSigProxy {
    uint32_t _shape[NWORDS];
    WideWordRef operator[](std::size_t i) const {
        return WideWordRef{ID, static_cast<int>(i)};
    }
};

// One element of an unpacked-array port, returned by
// UnpackedSigProxy::operator[]. The TB-IR emission for unpacked ports
// is a raw subscript on both sides (`dut->p[i]` in expressions,
// `dut->p[i] = e;` as a statement), so a conversion operator plus an
// assignment operator — both callable on temporaries — cover every
// access site.
struct UnpackedElemRef {
    int id;
    int idx;
    operator uint64_t() const {
        return static_cast<uint64_t>(harc_sv_get_elem(id, idx));
    }
    UnpackedElemRef& operator=(uint64_t v) {
        harc_sv_set_elem(id, idx, static_cast<long long>(v));
        return *this;
    }
};

// An unpacked-array DUT port (`input logic [W-1:0] p [NELEMS]`),
// element width <= 64 bits.
template <int ID, int NELEMS>
struct UnpackedSigProxy {
    UnpackedElemRef operator[](std::size_t i) const {
        return UnpackedElemRef{ID, static_cast<int>(i)};
    }
};

} // namespace cosim

// Scalar accessor proxies are <= 64-bit scalars for the signal helpers
// in harc_thread_rt.h (harc_read / harc_vec_lane_write). Wide and
// unpacked proxies are deliberately NOT marked: they go through the
// word-array / raw-subscript paths.
template <int ID>
struct harc_is_accessor_proxy<cosim::SigProxy<ID>> : std::true_type {};

} // namespace harc_rt
