// Regression for #936. Compile against the production runtime:
// c++ -std=c++20 -g -O1 -fsanitize=address,undefined -fno-omit-frame-pointer \
//     -Iruntime tests/scheduler_tick_growth.cpp -o /tmp/scheduler_tick_growth
// ASAN_OPTIONS=halt_on_error=1 UBSAN_OPTIONS=halt_on_error=1 /tmp/scheduler_tick_growth
#include "harc_thread_rt.h"
#include <array>
#include <cassert>

using namespace harc_rt;

namespace {
constexpr size_t count = 300; // Cross vector<bool>'s initial allocation.
using Children = std::array<ThreadSlot, count>;

HarcThread child(ThreadSlot* slot, size_t& entered, size_t& finished) {
    ++entered;
    // A true predicate must not cause a second resume in this tick.
    co_await wait_until(slot, [] { return true; });
    ++finished;
}

void append_children(ThreadScheduler& sched, Children& children,
                     size_t& entered, size_t& finished) {
    for (auto& slot : children) {
        slot.thread = child(&slot, entered, finished);
        sched.slots.push_back(&slot);
    }
}

HarcThread spawn(ThreadScheduler& sched, Children& children,
                 size_t& entered, size_t& finished) {
    append_children(sched, children, entered, finished);
    co_return;
}

HarcThread arm(bool& armed) {
    armed = true;
    co_return;
}

HarcThread finish() { co_return; }

void check_ticks(ThreadScheduler& sched, size_t& entered, size_t& finished) {
    sched.tick();
    assert(entered == count);
    assert(finished == 0);
    sched.tick();
    assert(finished == count);
    assert(sched.all_done());
    harc_destroy_scheduler_threads(sched);
}

void resume_growth() {
    Children children;
    ThreadSlot parent;
    ThreadScheduler sched;
    size_t entered = 0, finished = 0;
    parent.thread = spawn(sched, children, entered, finished);
    sched.slots.push_back(&parent);
    check_ticks(sched, entered, finished);
}

void predicate_growth() {
    Children children;
    ThreadSlot parent, waiter;
    ThreadScheduler sched;
    size_t entered = 0, finished = 0;
    bool armed = false, spawned = false;
    parent.thread = arm(armed);
    waiter.thread = finish();
    waiter.kind = WaitKind::WaitUntil;
    waiter.pred = [&] {
        if (!armed) return false; // No growth in pass 1.
        if (!spawned) {
            spawned = true;
            append_children(sched, children, entered, finished);
        }
        return true;
    };
    sched.slots = {&parent, &waiter};
    check_ticks(sched, entered, finished);
}
} // namespace

int main() {
    resume_growth();
    predicate_growth();
}
