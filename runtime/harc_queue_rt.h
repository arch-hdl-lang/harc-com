// harc_queue_rt.h — Queue helper for generated HARC scoreboard code.

#pragma once

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <deque>

namespace harc_rt {

// Hard abort for a pop off an empty queue.
//
// `std::deque::front()` on an empty deque is undefined behaviour, so an
// unguarded `pop()` used to read garbage — or crash somewhere unrelated
// to the actual bug — when a testbench popped an empty scoreboard or
// component queue. HARC's runtime-checking philosophy for out-of-range
// access is a hard abort with an actionable message (cf. arch-sim's
// out-of-bounds abort), so that is what this does.
//
// Kept out of the template so the diagnostic string exists once per
// program rather than once per queue element type.
[[noreturn]] inline void harc_queue_empty_pop() {
    // The sim's own stdout log is block-buffered when redirected to a
    // file or a pipe; abort() does not flush it, so without this the
    // last lines before the failure — the ones that say what the
    // testbench was doing — are lost exactly when they matter.
    std::fflush(stdout);
    std::fprintf(
        stderr,
        "HARC-ERROR: pop() on an empty queue\n"
        "  guard the pop with `.empty()`/`.size()`, or wait until the "
        "producer has pushed\n");
    std::fflush(stderr);
    std::abort();
}

template <typename T>
struct HarcQueue {
    std::deque<T> _d;

    void push(T v) { _d.push_back(v); }

    T pop() {
        if (_d.empty()) harc_queue_empty_pop();
        T v = _d.front();
        _d.pop_front();
        return v;
    }

    bool empty() const { return _d.empty(); }
    size_t size() const { return _d.size(); }
};

} // namespace harc_rt
