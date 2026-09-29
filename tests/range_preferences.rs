//! Executed regressions for issue #844, independent of backend agreement.
use harc::codegen::{cpp_tb, tbir};
use harc::parser::parse_source;
use std::{fs, process::Command};

#[test]
fn range_sampler_executes_with_extreme_bounds_and_unbiased_mapping() {
    let dir = std::env::temp_dir().join(format!("harc_range_preferences_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("sampler.cpp");
    let binary = dir.join("sampler");
    fs::write(
        &source,
        r#"
#include "harc_random_rt.h"
#include <cassert>
#include <climits>
using namespace harc_rt::random;
int main() {
    for (uint64_t n : {3, 7, 32, 256}) {
        uint64_t counts[256] = {};
        for (uint64_t seed = 0; seed < 65536; ++seed) {
            auto x = harc_prefer_range_u64(seed, 19, 0, n-1);
            assert(x < n);
            assert(x == harc_prefer_range_u64(seed, 19, 0, n-1));
            ++counts[x];
        }
        double chi = 0, expected = 65536.0/n;
        for (uint64_t i = 0; i < n; ++i) {
            double delta = counts[i]-expected;
            chi += delta*delta/expected;
        }
        assert(chi < 2*n+30); // fixed corpus; catches gross bias/collapse
    }
    uint64_t signed_counts[8] = {};
    for (uint64_t seed = 0; seed < 10000; ++seed) {
        uint64_t draw = harc_preference_draw(seed, 7);
        assert(harc_prefer_range_u64(seed, 7, 0, UINT64_MAX) == draw);
        assert(harc_prefer_range_u64(seed, 7, UINT64_MAX, UINT64_MAX) == UINT64_MAX);
        auto high = harc_prefer_range_u64(seed, 7, UINT64_MAX-6, UINT64_MAX);
        assert(high >= UINT64_MAX-6);
        auto s = harc_prefer_range_i64(seed, 7, -4, 3);
        assert(s >= -4 && s <= 3);
        ++signed_counts[s + 4];
        assert(harc_prefer_range_i64(seed, 7, INT64_MIN, INT64_MIN) == INT64_MIN);
        assert(harc_prefer_range_i64(seed, 7, INT64_MAX, INT64_MAX) == INT64_MAX);
        auto full = harc_prefer_range_i64(seed, 7, INT64_MIN, INT64_MAX);
        assert(static_cast<uint64_t>(full) == draw + (uint64_t{1} << 63));
        auto near_min = harc_prefer_range_i64(seed, 7, INT64_MIN, INT64_MIN+6);
        assert(near_min >= INT64_MIN && near_min <= INT64_MIN+6);
        const uint64_t span = (uint64_t{1} << 63) + 1;
        auto large = harc_prefer_range_u64(seed, 7, 0, span-1);
        assert(large < span);
    }
    // Fixed stream vectors exercise zero, one, two, and four rejected draws.
    // Naive modulo sampling fails the latter three deterministically.
    const uint64_t upper = uint64_t{1} << 63;
    assert(harc_prefer_range_u64(0, 7, 0, upper) == 5009149828745571131ULL);
    assert(harc_prefer_range_u64(4, 7, 0, upper) == 1901690445840535798ULL);
    assert(harc_prefer_range_u64(1, 7, 0, upper) == 7985454055576811505ULL);
    assert(harc_prefer_range_u64(2, 7, 0, upper) == 853104706901562097ULL);
    for (auto count : signed_counts) assert(count > 1000 && count < 1500);
    const char* labels[] = {"zero", "one"};
    HarcAutoCovPointMeta points[] = {{labels, 2}, {labels, 2}};
    HarcAutoCovCrossMeta crosses[] = {{labels, 1, 2}};
    HarcAutoCovPlan plan{"test", 0, points, 2, crosses, 1};
    HarcAutoCovState state;
    const uint64_t values[] = {0, 1}, one[] = {0};
    uint64_t a = 99, b = 99;
    // Every failed cross/point gets one turn, even across distinct groups.
    for (int attempt = 0; attempt < 6; ++attempt) {
        HarcAutoCovSelection selection;
        harc_auto_cov_apply_cross_preference(plan, state, selection, 0, one, values, a, b);
        harc_auto_cov_apply_point_preference(plan, state, selection, 0, values, a);
        harc_auto_cov_apply_point_preference(plan, state, selection, 1, values, b);
        assert(harc_auto_cov_has_preference(selection));
        assert(selection.kind == (attempt < 2 ? 2 : 1));
        harc_auto_cov_defer_selected(plan, state, selection);
    }
    HarcAutoCovSelection none;
    assert(!harc_auto_cov_apply_cross_preference(plan, state, none, 0, one, values, a, b));
    assert(!harc_auto_cov_apply_point_preference(plan, state, none, 0, values, a));
    assert(!harc_auto_cov_apply_point_preference(plan, state, none, 1, values, b));
    harc_auto_cov_finish_selection(state, none);
    // A changed hard-constraint context can make the same goal reachable.
    HarcAutoCovSelection retry;
    assert(harc_auto_cov_apply_cross_preference(plan, state, retry, 0, one, values, a, b));
    assert(retry.i == 0 && retry.j == 0);
    for (auto flag : state.point_blocked) assert(!flag);
    for (auto flag : state.cross_blocked) assert(!flag);

}
"#,
    )
    .unwrap();
    let result = Command::new("c++")
        .args([
            "-std=c++20",
            "-fsanitize=undefined",
            "-fno-sanitize-recover=all",
            "-Iruntime",
        ])
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

#[test]
fn both_emitters_use_ranges_and_never_infer_blocked_from_preference_failure() {
    let parsed = parse_source(
        r#"
transaction T
    a : uint<8> with [range(0, (31))]
    b : sint<8> with [range(-4, 3)]
    c : uint<100> with [range(0, 7)]
    d : uint<200> with [range(0, 7)]
    e : uint<64> with [range(0x8000000000000000, 0xffffffffffffffff)]
    f : sint<64> with [range(-9223372036854775808, -1)]
    g : uint<8>
end transaction T
test RangePreferencesTest
    let dut : Top
    run
        let t : T
        randomize(t)
    end run
end test RangePreferencesTest
"#,
    )
    .unwrap();
    let opts = cpp_tb::EmitOpts::default();
    let program = harc::ir::lower::lower_program(&parsed).unwrap();
    for cpp in [
        cpp_tb::emit(&parsed).unwrap(),
        tbir::emit(&program, &parsed, &opts).unwrap(),
    ] {
        assert!(cpp.contains("harc_prefer_range_u64(_harc_rt_seed, 0, 0ULL, 31ULL)"));
        assert!(cpp.contains("harc_prefer_range_i64(_harc_rt_seed, 1, -4LL, 3LL)"));
        assert!(cpp.contains("harc_prefer_range_u64(_harc_rt_seed, 2, 0ULL, 7ULL)"));
        assert!(cpp.contains("harc_prefer_range_u64(_harc_rt_seed, 3, 0ULL, 7ULL)"));
        assert!(cpp.contains("harc_prefer_uint(_harc_rt_seed, 4, 64)"));
        assert!(cpp.contains("harc_prefer_sint(_harc_rt_seed, 5, 64)"));
        assert!(cpp.contains("harc_prefer_uint(_harc_rt_seed, 6, 8)"));
        assert!(!cpp.contains("harc_auto_cov_mark_selected_point_blocked("));
        assert!(!cpp.contains("harc_auto_cov_mark_selected_cross_blocked("));
    }
}

#[test]
fn generated_ranges_replay_diversify_and_match_backends() {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "Verilator required"
    );
    if !present {
        eprintln!("skipping range simulation: Verilator absent");
        return;
    }
    let dir = std::env::temp_dir().join(format!("harc_range_sim_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut backend_traces = Vec::new();
    for backend in ["v1", "tbir"] {
        let mut traces = Vec::new();
        for seed in [1, 2, 424242, 1] {
            let trace = dir.join(format!("{backend}-{seed}.jsonl"));
            let out = Command::new(env!("CARGO_BIN_EXE_harc"))
                .env("HARC_NO_LEARN", "1")
                .args([
                    "sim",
                    "--sv",
                    "tests/dut/top_counter.sv",
                    "--top",
                    "Top",
                    "--codegen",
                    backend,
                    "--seed",
                    &seed.to_string(),
                    "tests/fixtures/randomize_range_preference_test.harc",
                    "--outdir",
                ])
                .arg(dir.join(backend))
                .arg("--record-trace")
                .arg(&trace)
                .output()
                .unwrap();
            let log = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                out.status.success() && log.contains("ALL TESTS PASSED"),
                "{backend}/{seed}: {log}"
            );
            assert!(!log.contains("*BLOCKED*"), "false unreachable goal: {log}");
            assert!(log.contains("100.0%"), "range coverage incomplete: {log}");
            let samples: Vec<String> = fs::read_to_string(trace)
                .unwrap()
                .lines()
                .filter(|line| line.contains("\"type\":\"randomize\""))
                .map(str::to_owned)
                .collect();
            assert!(samples.len() >= 599);
            traces.push(samples);
        }
        assert_eq!(traces[0], traces[3], "same seed must replay");
        assert_ne!(traces[0], traces[1], "different seeds must vary");
        assert_ne!(traces[0], traces[2], "different seeds must vary");
        backend_traces.push(traces);
    }
    assert_eq!(backend_traces[0], backend_traces[1]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn range_preference_boundary_and_fallback_contract() {
    for (ty, bounds, expected) in [
        ("uint<64>", "0x8000000000000000, 0xffffffffffffffff", "harc_prefer_range_u64(_harc_rt_seed, 0, 9223372036854775808ULL, 18446744073709551615ULL)"),
        ("sint<64>", "-9223372036854775808, -1", "harc_prefer_range_i64(_harc_rt_seed, 0, (-9223372036854775807LL - 1), -1LL)"),
        ("uint<8>", "8'h01, 0x1ff", "harc_prefer_range_u64(_harc_rt_seed, 0, 1ULL, 255ULL)"),
        ("sint<8>", "-1000, 1000", "harc_prefer_range_i64(_harc_rt_seed, 0, -128LL, 127LL)"),
        ("sint<63>", "-9223372036854775808, 9223372036854775807", "harc_prefer_range_i64(_harc_rt_seed, 0, -4611686018427387904LL, 4611686018427387903LL)"),
        ("uint<8>", "300, 400", "harc_prefer_uint(_harc_rt_seed, 0, 8)"),
        ("uint<8>", "7, 2", "harc_prefer_uint(_harc_rt_seed, 0, 8)"),
        ("uint<8>", "-1, 7", "harc_prefer_uint(_harc_rt_seed, 0, 8)"),
        ("uint<8>", "0, (3 + 4)", "harc_prefer_uint(_harc_rt_seed, 0, 8)"),
        ("sint<100>", "-4, 3", "harc_prefer_u128(_harc_rt_seed, 0, 100)"),
    ] {
        let source = format!("transaction T\n a : {ty} with [range({bounds})]\nend transaction T\ntest BoundsTest\n let dut : Top\n run\n let t : T\n randomize(t) with\n t.a == t.a\n end randomize\n end run\nend test BoundsTest");
        let parsed = parse_source(&source).unwrap();
        let program = harc::ir::lower::lower_program(&parsed).unwrap();
        for cpp in [cpp_tb::emit(&parsed).unwrap(), tbir::emit(&program, &parsed, &cpp_tb::EmitOpts::default()).unwrap()] {
            assert!(cpp.contains(expected), "{ty} [{bounds}]: missing {expected}");
        }
    }
}

#[test]
fn generated_boundary_ranges_preserve_adjacent_policies() {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "Verilator required"
    );
    if !present {
        eprintln!("skipping boundary simulation: Verilator absent");
        return;
    }
    let dir = std::env::temp_dir().join(format!("harc_range_boundary_{}", std::process::id()));
    for backend in ["v1", "tbir"] {
        for seed in [1, 2, 424242] {
            let out = Command::new(env!("CARGO_BIN_EXE_harc"))
                .env("HARC_NO_LEARN", "1")
                .args([
                    "sim",
                    "--sv",
                    "tests/dut/top_counter.sv",
                    "--top",
                    "Top",
                    "--codegen",
                    backend,
                    "--seed",
                    &seed.to_string(),
                    "tests/fixtures/randomize_range_boundary_test.harc",
                    "--outdir",
                ])
                .arg(dir.join(backend))
                .output()
                .unwrap();
            let log = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                out.status.success() && log.contains("ALL TESTS PASSED"),
                "{backend}/{seed}: {log}"
            );
            assert!(
                !log.contains("*BLOCKED*"),
                "preference/unique conflict is not unreachability: {log}"
            );
        }
    }
    fs::remove_dir_all(dir).unwrap();
}
