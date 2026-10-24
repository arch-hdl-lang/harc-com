#include <cstdint>

static uint64_t next_value_call_count = 0;

extern "C" uint64_t next_value() { return ++next_value_call_count; }

extern "C" uint64_t next_value_calls() {
    return next_value_call_count;
}
