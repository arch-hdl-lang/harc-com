// Run from repository root:
// c++ -std=c++20 -g -fsanitize=address,undefined -fno-omit-frame-pointer -Iruntime tests/scheduler_growth.cpp -o /tmp/scheduler_growth
// /tmp/scheduler_growth
#include "harc_thread_rt.h"
#include <cassert>
using namespace harc_rt;

HarcThread child(ThreadSlot* s, int& count) {
    ++count;
    co_await wait_cycles(s, 1);
    ++count;
}
HarcThread parent(ThreadScheduler& scheduler, ThreadSlot* children, int& count) {
    for (int i = 0; i < 300; ++i) {
        children[i].thread = child(&children[i], count);
        scheduler.slots.push_back(&children[i]);
    }
    co_return;
}
void resume_growth() {
    ThreadScheduler scheduler;
    int count = 0;
    ThreadSlot children[300], root;
    root.thread = parent(scheduler, children, count);
    scheduler.slots.push_back(&root);
    scheduler.tick();
    assert(count == 300);
    scheduler.tick();
    assert(count == 600 && scheduler.all_done());
}

void predicate_growth() {
    ThreadScheduler scheduler;
    int count = 0;
    ThreadSlot children[300], root, trigger;
    bool ready = false;
    bool appended = false;
    auto enable = [](bool& flag) -> HarcThread {
        flag = true;
        co_return;
    };
    root.thread = enable(ready);
    trigger.kind = WaitKind::WaitUntil;
    trigger.pred = [&] {
        // Pass 1 sees false; only the fixed-point predicate sweep grows.
        if (ready && !appended) {
            appended = true;
            for (int i = 0; i < 300; ++i) {
                children[i].thread = child(&children[i], count);
                scheduler.slots.push_back(&children[i]);
            }
        }
        return ready;
    };
    trigger.thread = enable(ready);
    scheduler.slots = {&root, &trigger};
    scheduler.tick();
    assert(appended && count == 300);
    scheduler.tick();
    assert(count == 600 && scheduler.all_done());
}

int main() {
    resume_growth();
    predicate_growth();
}
