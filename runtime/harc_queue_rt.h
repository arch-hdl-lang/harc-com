// harc_queue_rt.h — Queue helper for generated HARC scoreboard code.

#pragma once

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <deque>
#include <functional>
#include <utility>

namespace harc_rt {

// Reporter for a pop off an empty queue, installed for the duration of a
// test run by the generated `run_<Test>()` prologue — see
// `HarcQueueFatalScope` below.
//
// `pop()` lives in a standalone header and has no way to reach the test
// context (`ctx.errors`, `_fatal`, `sim_log_line`), and threading one
// through every queue would change how every scoreboard and component
// struct is built. An installed reporter keeps the queue helper free of
// sim state while still routing the failure through the sim's own FATAL
// path, so the run tears down cleanly with its log and trace intact.
//
// Installed once in the prologue before any worker thread spawns and
// only read afterwards. The reporter body touches `ctx.errors` /
// `_fatal`, so under `--mt` it carries the same race profile as every
// other FATAL raised from an actor body — no better, no worse.
inline std::function<void()> harc_queue_empty_pop_reporter;

// Backstop for an empty pop with NO reporter installed — a unit test, or
// any future caller outside a generated `run_<Test>()`. There is no test
// context to fail cleanly through, so this is the one place that keeps
// the hard abort: silently returning a default would turn a real bug
// into a wrong-answer run nobody notices.
//
// Kept out of the template so the diagnostic string exists once per
// program rather than once per queue element type.
[[noreturn]] inline void harc_queue_empty_pop_abort() {
    // stdout is block-buffered when redirected to a file or a pipe, and
    // abort() does not flush it, so without this the lines before the
    // failure — the ones that say what the caller was doing — are lost
    // exactly when they matter.
    std::fflush(stdout);
    std::fprintf(
        stderr,
        "HARC-ERROR: pop() on an empty queue\n"
        "  guard the pop with `.empty()`/`.size()`, or wait until the "
        "producer has pushed\n");
    std::fflush(stderr);
    std::abort();
}

inline void harc_queue_empty_pop() {
    if (harc_queue_empty_pop_reporter) {
        harc_queue_empty_pop_reporter();
        return;
    }
    harc_queue_empty_pop_abort();
}

// Installs `report` as the empty-pop reporter for its own lifetime.
// Scoped rather than assigned once so a process that runs more than one
// test cannot leave a dangling reference to a dead test context behind.
struct HarcQueueFatalScope {
    explicit HarcQueueFatalScope(std::function<void()> report) {
        harc_queue_empty_pop_reporter = std::move(report);
    }
    ~HarcQueueFatalScope() { harc_queue_empty_pop_reporter = nullptr; }
    HarcQueueFatalScope(const HarcQueueFatalScope&) = delete;
    HarcQueueFatalScope& operator=(const HarcQueueFatalScope&) = delete;
};

template <typename T>
struct HarcQueue {
    std::deque<T> _d;

    void push(T v) { _d.push_back(v); }

    // `std::deque::front()` on an empty deque is undefined behaviour, so
    // an unguarded pop used to read garbage — or die somewhere unrelated
    // to the mistake that caused it. Report through the installed
    // reporter and hand back a value-initialised `T`: the run is already
    // marked fatal and stops at the next scheduler tick, and a zero is
    // deterministic where the old read was not.
    T pop() {
        if (_d.empty()) {
            harc_queue_empty_pop();
            return T{};
        }
        T v = _d.front();
        _d.pop_front();
        return v;
    }

    bool empty() const { return _d.empty(); }
    size_t size() const { return _d.size(); }
};

} // namespace harc_rt
