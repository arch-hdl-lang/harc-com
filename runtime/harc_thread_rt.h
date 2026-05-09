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
#include <vector>

namespace harc_rt {

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

} // namespace harc_rt
