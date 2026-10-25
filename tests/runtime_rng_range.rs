//! Direct runtime regression for legacy v1 signed range sampling.

use std::{fs, process::Command};

#[test]
fn rng_range_handles_full_signed_domain_without_ub() {
    let dir = std::env::temp_dir().join(format!("harc_rng_range_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("range.cpp");
    let binary = dir.join("range");
    fs::write(
        &source,
        r#"
#include "harc_random_rt.h"
#include <cassert>
#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <limits>

using harc_rt::random::harc_rng_range;
using i64 = std::int64_t;
using u64 = std::uint64_t;
constexpr i64 MIN = std::numeric_limits<i64>::min();
constexpr i64 MAX = std::numeric_limits<i64>::max();

struct Draws {
    const u64* values;
    unsigned* count;
    u64 operator()() { return values[(*count)++]; }
};

i64 legacy_defined(u64 draw, i64 lo, i64 hi) {
    return lo + static_cast<i64>(draw % static_cast<u64>(hi - lo + 1));
}

void check(i64 lo, i64 hi, u64 draw, i64 expected, unsigned draws = 1) {
    const u64 values[] = {draw};
    unsigned count = 0;
    Draws next{values, &count};
    const i64 actual = harc_rng_range(next, lo, hi);
    if (actual != expected) {
        std::fprintf(stderr, "value for [%lld, %lld] draw %llu: got %lld expected %lld\n",
            static_cast<long long>(lo), static_cast<long long>(hi),
            static_cast<unsigned long long>(draw), static_cast<long long>(actual),
            static_cast<long long>(expected));
        std::abort();
    }
    if (count != draws) {
        std::fprintf(stderr, "draw count for [%lld, %lld]: got %u expected %u\n",
            static_cast<long long>(lo), static_cast<long long>(hi), count, draws);
        std::abort();
    }
    if (hi > lo) assert(actual >= lo && actual <= hi);
    else assert(actual == lo);
}

int main() {
    // A 2^63-cardinality interval used by the existing v1 fixture.
    check(MIN, -1, 0, MIN);
    check(MIN, -1, 1, MIN + 1);
    check(MIN, -1, std::numeric_limits<u64>::max(), -1);
    check(MIN, -1, u64{1} << 63, MIN);

    // The two half-domains and a zero-crossing range retain modulo mapping.
    check(0, MAX, 0, 0);
    check(0, MAX, MAX, MAX);
    check(0, MAX, std::numeric_limits<u64>::max(), MAX);
    check(-3, 4, 17, legacy_defined(17, -3, 4));

    // The full domain has cardinality 2^64, represented by span == 0.
    check(MIN, MAX, 0, MIN);
    check(MIN, MAX, 1, MIN + 1);
    check(MIN, MAX, std::numeric_limits<u64>::max(), MAX);

    // Degenerate and reversed intervals preserve the no-draw behavior.
    check(7, 7, 123, 7, 0);
    check(9, 3, 123, 9, 0);

    // Previously defined ranges retain their exact one-draw stream mapping.
    for (i64 lo : {-100, -7, 0, 7}) {
        for (i64 hi : {lo + 1, lo + 8, i64{100}}) {
            if (hi <= lo) continue;
            for (u64 draw : {u64{0}, u64{1}, u64{17}, u64{MAX}}) {
                check(lo, hi, draw, legacy_defined(draw, lo, hi));
            }
        }
    }
}
"#,
    )
    .unwrap();

    let include = format!("-I{}/runtime", env!("CARGO_MANIFEST_DIR"));
    let result = Command::new("c++")
        .args([
            "-std=c++20",
            "-fsanitize=undefined",
            "-fno-sanitize-recover=all",
        ])
        .arg(include)
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("C++ compiler required");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = Command::new(&binary).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::remove_dir_all(dir).unwrap();
}
