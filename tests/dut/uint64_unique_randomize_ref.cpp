// Reference checker for tests/fixtures/uint64_unique_randomize_test.harc.
// The HARC fixture uses this to make [unique within test] observable; the
// returned 1/0 value lets the test fail at the first repeated bounded sample.

#include <cstdint>
#include <unordered_set>

extern "C" uint64_t ref_unique_u8(uint64_t value) {
    static std::unordered_set<uint64_t> seen;
    return seen.insert(value).second ? 1 : 0;
}

extern "C" uint64_t ref_unique_multi_u8(uint64_t value) {
    static std::unordered_set<uint64_t> seen;
    return seen.insert(value).second ? 1 : 0;
}
