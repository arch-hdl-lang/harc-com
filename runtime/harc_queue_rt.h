// harc_queue_rt.h — Queue helper for generated HARC scoreboard code.

#pragma once

#include <cstddef>
#include <deque>

namespace harc_rt {

template <typename T>
struct HarcQueue {
    std::deque<T> _d;

    void push(T v) { _d.push_back(v); }

    T pop() {
        T v = _d.front();
        _d.pop_front();
        return v;
    }

    bool empty() const { return _d.empty(); }
    size_t size() const { return _d.size(); }
};

} // namespace harc_rt
