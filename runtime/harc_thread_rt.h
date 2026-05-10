// harc_thread_rt.h — Coroutine runtime for HARC test simulation.
//
// Slim port of arch-com's `arch_thread_rt.h`. The two runtimes are
// independent (HARC owns its build artifacts and doesn't link against
// arch's runtime), but the cooperative-scheduler model is identical so
// HARC tests can interoperate with arch DUTs that drop in their own
// thread-sim later without two competing schedulers in one process.
//
// Phase 1 (this file):
//   - Single OS thread, single clock domain.
//   - Each `run` block (and later: each `driver` / `agent` / `monitor`)
//     compiles to a C++20 coroutine returning `HarcThread`.
//   - One `ThreadScheduler` per test instance owns all coroutines.
//   - Awaiters:
//       * `wait_until(slot, pred)`  — suspend; resume on next posedge
//                                      where pred() returns true.
//       * `wait_cycles(slot, N)`    — suspend; resume after N posedges
//                                      (N>=1).
//
// Out of Phase 1 scope: fork/join, resource locks, multi-thread
// scheduling on multiple OS threads, cross-domain clocks. Those land
// in subsequent phases under the same surface so test source doesn't
// shift when the runtime grows up.
//
// Why match arch's runtime structure: HARC is the sister verification
// language. When HARC drivers/monitors bind to ARCH `bus` types they
// will eventually run alongside ARCH-emitted threads in the same
// process. Keeping the two runtimes structurally identical (same slot
// states, same tick-pass semantics) means later merging them into a
// shared runtime is a structural rename, not a semantic redesign.

#pragma once

#include <coroutine>
#include <functional>
#include <cstdint>
#include <initializer_list>
#include <type_traits>
#include <vector>
#include <atomic>
#include <thread>

// 65–128-bit integer type for whole-signal arithmetic and >64-bit
// hex literals. File-scope (not inside `harc_rt::`) so emitted code
// can reference it bare. Mirrors arch-com's `_arch_u128` (see
// arch-com/src/sim_codegen/mod.rs:767).
typedef unsigned __int128 _harc_u128;

namespace harc_rt {

// ── Wide-vector interop ─────────────────────────────────────────────────────
//
// Verilator lowers SystemVerilog ports by bit-width:
//   1..32 bits   → uint8_t / uint16_t / uint32_t
//   33..64 bits  → uint64_t
//   >64 bits     → VlWide<N>  (array of N uint32_t words, N=ceil(W/32))
//
// HARC's testbench source language doesn't have separate "narrow"/"wide"
// types — every integer expression flows through the same shape. To make
// that uniform we use a single 128-bit value type for whole-signal
// reads, and accept any integer value (cast up to 128 bits) on whole-
// signal writes. 128 bits covers every arch-com DUT we vendor today
// (AES blocks, AXI4 data lanes up to 128b). Wider signals (>128b)
// fall back to per-word access via VlWide indexing — they round-trip
// at the L-value level via Verilator's `operator[]`.
//
// Mirrors arch-com's `_arch_u128` design (see arch-com src/sim_codegen/
// mod.rs:767 for the equivalent typedef and conversion helpers). The
// `_harc_u128` typedef is at file scope above; the helpers below use
// it for the value type so codegen can emit `_harc_u128` literals
// without namespace qualification.

template<typename Sig, typename Val>
inline void harc_assign(Sig& sig, Val val) {
    if constexpr (std::is_arithmetic_v<Sig>) {
        sig = static_cast<Sig>(val);
    } else {
        // VlWide<N>: write low 128 bits via the first four words; zero
        // anything beyond. `sizeof(Sig) / sizeof(uint32_t)` resolves to
        // N because VlWide is `final` with a single uint32_t[N] member —
        // no vtable, no padding under any standard ABI we target.
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        const _harc_u128 v = static_cast<_harc_u128>(val);
        sig[0] = static_cast<uint32_t>(v);
        if constexpr (N >= 2) sig[1] = static_cast<uint32_t>(v >> 32);
        if constexpr (N >= 3) sig[2] = static_cast<uint32_t>(v >> 64);
        if constexpr (N >= 4) sig[3] = static_cast<uint32_t>(v >> 96);
        for (std::size_t i = 4; i < N; ++i) sig[i] = 0;
    }
}

template<typename Sig>
inline _harc_u128 harc_read(const Sig& sig) {
    if constexpr (std::is_arithmetic_v<Sig>) {
        return static_cast<_harc_u128>(sig);
    } else {
        // VlWide<N>: combine the low four words into a 128-bit value.
        // Upper words (if N > 4) are dropped — caller can use indexed
        // access (`dut.field[i]`) for those.
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        _harc_u128 v = static_cast<uint32_t>(sig[0]);
        if constexpr (N >= 2) v |= (static_cast<_harc_u128>(static_cast<uint32_t>(sig[1])) << 32);
        if constexpr (N >= 3) v |= (static_cast<_harc_u128>(static_cast<uint32_t>(sig[2])) << 64);
        if constexpr (N >= 4) v |= (static_cast<_harc_u128>(static_cast<uint32_t>(sig[3])) << 96);
        return v;
    }
}

// ── Wider-than-128-bit support ───────────────────────────────────────
// 65–128b values flow through `_harc_u128` (above). For wider signals
// (256, 512, 1024, … up to arbitrary `VlWide<N>`), the natural surface
// is a word-array literal: `dut.wdata = 0x<N hex digits>` lowers in
// the codegen to a word-array initializer-list and routes through
// these helpers. Words are LSB-first to match Verilator's `VlWide`
// layout (word 0 = bits[31:0], word 1 = bits[63:32], …).
//
// `harc_assign_words` writes the literal into the signal, padding any
// extra signal words with zero. `harc_eq_words` compares word-by-word,
// treating any unspecified words as zero on both sides.

template<typename Sig>
inline void harc_assign_words(Sig& sig, std::initializer_list<uint32_t> words) {
    if constexpr (std::is_arithmetic_v<Sig>) {
        // Narrow signal — pack the low 1–2 words into a uint64.
        auto it = words.begin();
        uint64_t v = 0;
        if (it != words.end()) v  = static_cast<uint64_t>(*it++);
        if (it != words.end()) v |= static_cast<uint64_t>(*it++) << 32;
        // Extra words beyond uint64 capacity are dropped (would set
        // bits the signal can't hold anyway).
        sig = static_cast<Sig>(v);
    } else {
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        std::size_t i = 0;
        for (auto w : words) {
            if (i < N) sig[i] = w;
            ++i;
        }
        for (std::size_t j = i; j < N; ++j) sig[j] = 0;
    }
}

// Inline hex formatter for 65–128-bit values used by the printf-style
// interpolation lowering. Constructed as a temporary in the printf
// arg list:
//
//   sim_log_line("INFO", "ct=0x%s",
//                (const char*)HarcHexBuf128(harc_read(dut->x), 32, false));
//
// The temporary's lifetime extends through the full surrounding
// expression (the printf call), so the returned `const char*` is
// valid for the duration of the printf. Each call site instantiates
// its own temporary, so multiple wide-hex args in one printf don't
// clobber each other (each has its own on-stack buffer).
//
// `_harc_u128` is up to 32 hex digits; `width` is clamped to that
// range. Narrower than 1 is also clamped (to 1) so no zero-length
// formatting.
struct HarcHexBuf128 {
    char buf[40];
    HarcHexBuf128(_harc_u128 v, int width, bool upper) {
        const char* hex = upper ? "0123456789ABCDEF" : "0123456789abcdef";
        if (width < 1) width = 1;
        if (width > 32) width = 32;
        for (int i = width - 1; i >= 0; --i) {
            buf[i] = hex[(uint32_t)(v & 0xf)];
            v >>= 4;
        }
        buf[width] = '\0';
    }
    operator const char*() const { return buf; }
};

template<typename Sig>
inline bool harc_eq_words(const Sig& sig, std::initializer_list<uint32_t> words) {
    if constexpr (std::is_arithmetic_v<Sig>) {
        // Pack the literal's low 64 bits.
        auto it = words.begin();
        uint64_t expected = 0;
        if (it != words.end()) expected  = static_cast<uint64_t>(*it++);
        if (it != words.end()) expected |= static_cast<uint64_t>(*it++) << 32;
        // Any further literal words must be zero for equality.
        while (it != words.end()) { if (*it++ != 0) return false; }
        return static_cast<uint64_t>(sig) == expected;
    } else {
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        std::size_t i = 0;
        for (auto w : words) {
            uint32_t s = (i < N) ? static_cast<uint32_t>(sig[i]) : 0u;
            if (s != w) return false;
            ++i;
        }
        // Any remaining signal words must be zero for equality.
        for (std::size_t j = i; j < N; ++j) {
            if (sig[j] != 0) return false;
        }
        return true;
    }
}

struct ThreadScheduler;

// Promise type for HARC coroutines. Returns void (the coroutine's
// effect is on the test's signal state, not a return value); the
// handle is owned by the scheduler via a ThreadSlot.
struct HarcThread {
    struct promise_type {
        HarcThread get_return_object() {
            return HarcThread{ std::coroutine_handle<promise_type>::from_promise(*this) };
        }
        std::suspend_always initial_suspend() noexcept { return {}; }
        std::suspend_always final_suspend()   noexcept { return {}; }
        void return_void() noexcept {}
        void unhandled_exception() { std::terminate(); }
    };
    std::coroutine_handle<promise_type> h;
    bool done() const { return !h || h.done(); }
    void resume() { if (h && !h.done()) h.resume(); }
    void destroy() { if (h) { h.destroy(); h = nullptr; } }
};

// State a suspended slot is parked on.
//   Ready       — not currently suspended; will run on next tick().
//   WaitUntil   — resume when pred() returns true at a posedge.
//   WaitCycles  — resume after `cycles_remaining` posedges.
//   Done        — coroutine finished.
enum class WaitKind : uint8_t { Ready, WaitUntil, WaitCycles, Done };

// One scheduled coroutine. The scheduler owns these; awaiters mutate
// the fields when a coroutine suspends.
struct ThreadSlot {
    HarcThread thread;
    WaitKind   kind = WaitKind::Ready;
    uint32_t   cycles_remaining = 0;
    std::function<bool()> pred;  // for WaitUntil
};

// Awaiter: `co_await wait_until(slot, pred)`. The predicate is captured
// by value into the slot; the lambda's own captures must outlive the
// suspend, which is trivially true since the caller is the same
// coroutine.
struct WaitUntilAwaiter {
    std::function<bool()> pred;
    ThreadSlot* slot;
    bool await_ready() noexcept {
        // Don't short-circuit even when pred is true: the semantics is
        // "resumes at the *next* posedge where pred is true", matching
        // arch's lowered-fsm wait-state behavior. Same-cycle resume
        // would skip the implicit one-cycle quantum.
        return false;
    }
    void await_suspend(std::coroutine_handle<>) noexcept {
        slot->kind = WaitKind::WaitUntil;
        slot->pred = std::move(pred);
    }
    void await_resume() noexcept {}
};

// Awaiter: `co_await wait_cycles(slot, N)`. N must be >= 1; passing
// N == 0 is treated as "no wait" (await_ready returns true).
struct WaitCyclesAwaiter {
    uint32_t n;
    ThreadSlot* slot;
    bool await_ready() noexcept { return n == 0; }
    void await_suspend(std::coroutine_handle<>) noexcept {
        slot->kind = WaitKind::WaitCycles;
        slot->cycles_remaining = n;
    }
    void await_resume() noexcept {}
};

// Scheduler: one per test. Owns slot pointers; the slot lifetime is
// the surrounding test class / main scope.
struct ThreadScheduler {
    std::vector<ThreadSlot*> slots;

    // Run all initially-Ready slots until they hit their first wait.
    // Called once after coroutines are constructed and before the
    // first posedge — so test setup statements (driving rst, default
    // signal values) run before the DUT samples anything.
    void bootstrap() {
        for (auto* s : slots) {
            if (s->kind == WaitKind::Ready) {
                s->thread.resume();
                if (s->thread.done()) s->kind = WaitKind::Done;
            }
        }
    }

    // Called by the test loop after the posedge eval / checker pass.
    // Semantics:
    //   - WaitCycles slots: decrement; if hits 0, mark Ready.
    //   - WaitUntil  slots: evaluate pred; if true, mark Ready.
    //   - Ready      slots: resume the coroutine until it suspends or
    //                       finishes. Iterated to a fixed point so
    //                       multi-stage cascades (slot A's resume makes
    //                       slot B's predicate true) settle in one
    //                       tick — matching arch's behavior for
    //                       fork/join unconditional transitions.
    //
    // The resumed[] guard prevents a freshly suspended slot from
    // re-firing in the same tick: `wait_cycles(N)` and `wait_until`
    // both promise at-least-one-cycle quantum from the suspend.
    void tick() {
        std::vector<bool> resumed(slots.size(), false);

        // Pass 1: advance counters / preds based on prior-tick state.
        for (auto* s : slots) {
            if (s->kind == WaitKind::WaitCycles) {
                if (s->cycles_remaining > 0) --s->cycles_remaining;
                if (s->cycles_remaining == 0) s->kind = WaitKind::Ready;
            } else if (s->kind == WaitKind::WaitUntil) {
                if (s->pred && s->pred()) s->kind = WaitKind::Ready;
            }
        }
        // Pass 2 (fixed point): resume Ready slots, then re-check
        // remaining WaitUntil preds (a resumed slot may have changed
        // signal state another slot is waiting on).
        bool changed = true;
        while (changed) {
            changed = false;
            for (size_t i = 0; i < slots.size(); ++i) {
                if (slots[i]->kind == WaitKind::Ready && !resumed[i]) {
                    resumed[i] = true;
                    slots[i]->thread.resume();
                    if (slots[i]->thread.done()) slots[i]->kind = WaitKind::Done;
                    changed = true;
                }
            }
            for (size_t i = 0; i < slots.size(); ++i) {
                if (!resumed[i] && slots[i]->kind == WaitKind::WaitUntil) {
                    if (slots[i]->pred && slots[i]->pred()) {
                        slots[i]->kind = WaitKind::Ready;
                        changed = true;
                    }
                }
            }
        }
    }

    bool all_done() const {
        for (auto* s : slots) if (s->kind != WaitKind::Done) return false;
        return true;
    }
};

// Convenience constructors. Pass the slot the caller is parked in so
// the awaiter can write its suspend state.
inline WaitUntilAwaiter  wait_until (ThreadSlot* s, std::function<bool()> p) { return {std::move(p), s}; }
inline WaitCyclesAwaiter wait_cycles(ThreadSlot* s, uint32_t n)              { return {n, s}; }

// ─── Multi-OS-thread support (Phase 3a) ───────────────────────────────
//
// Atomic spin-wait barrier. ~10–30 ns per round-trip vs ~µs for
// std::condition_variable. Used by emit_main's per-cycle barrier
// dance to synchronize the main thread with N worker threads (one
// per bound-driver/bound-monitor coroutine actor).
//
// Mirrors arch-com's `arch_rt::Barrier` exactly so the two runtimes
// can later be merged. Dual cache-line padding avoids false sharing
// with neighbouring fields — Apple Silicon's strong cache-line
// bouncing penalty makes alignment matter much more than on x86.
//
// Construct with `target` = number of participating threads. Each
// thread calls `wait()` at the synchronization point; all participants
// advance together.
//
// Caveat (matches arch-com's ThreadSimPerf measurements): per-cycle
// barrier sync on Apple Silicon costs ~10s of µs round-trip due to
// P/E core scheduling jitter. With ~µs of per-cycle actor work,
// multi-thread mode is *slower* than single-thread cooperative mode.
// Cycle batching (`run_cycles(K)` API, Phase 3b) is required for
// wall-clock wins. Phase 3a delivers the runtime topology and
// validates correctness against the cooperative baseline; perf comes
// next.
struct alignas(64) Barrier {
    alignas(64) std::atomic<uint32_t> count{0};
    alignas(64) std::atomic<uint32_t> generation{0};
    uint32_t target;
    explicit Barrier(uint32_t target) : target(target) {}
    void wait() {
        uint32_t gen = generation.load(std::memory_order_acquire);
        if (count.fetch_add(1, std::memory_order_acq_rel) + 1 == target) {
            count.store(0, std::memory_order_release);
            generation.fetch_add(1, std::memory_order_release);
        } else {
            // Spin briefly, then yield to avoid pegging a core when
            // other participants are slow (oversubscribed). Long spin
            // window: per-cycle sim work is often sub-µs so a low
            // budget would trigger OS context switches every cycle.
            // ~100k iters ≈ 30–100 µs on modern CPUs — well over
            // typical per-cycle work.
            uint32_t spins = 0;
            while (generation.load(std::memory_order_acquire) == gen) {
                if (++spins > 100000) {
                    std::this_thread::yield();
                    spins = 0;
                }
            }
        }
    }
};

} // namespace harc_rt
