//! Runtime gate for the checked arithmetic helpers in `harc_thread_rt.h`.
//!
//! Two runtime semantics are pinned here, at every carrier tier, by
//! compiling the shipped header with a host compiler (no Verilator):
//!
//!   * A shift of an operand of exact declared width W < 64 keeps W bits and
//!     saturates at a count >= W (`harc_shl_uw` and friends).
//!   * A shift count of 64 or more on a <= 64-bit carrier saturates —
//!     `<<` and logical `>>` give 0, arithmetic `>>` gives the sign fill.
//!     The emitter used to spell a bare C++ `<<` / `>>`, which is undefined
//!     for such counts (arm64 and x86-64 both reduce the count mod 64, so
//!     `1 << 64` came back as 1).
//!   * Division and modulo by zero, and signed `MIN / -1` / `MIN % -1` at
//!     the operation's declared width, report a runtime FATAL through the
//!     installed reporter at every width (<= 64, 65..128, > 128). The wide
//!     kernels used to answer `x / 0 = 0` and `x % 0 = x` silently, and the
//!     <= 64-bit tier was C++ UB (SIGFPE on x86-64).
//!
//! With no reporter installed the fault aborts, like an empty-queue read.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn cxx() -> Option<String> {
    for cc in [
        std::env::var("CXX").unwrap_or_default().as_str(),
        "g++",
        "clang++",
    ] {
        if cc.is_empty() {
            continue;
        }
        if Command::new(cc)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(cc.to_string());
        }
    }
    None
}

const PROBE: &str = r#"#include "harc_thread_rt.h"
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

using namespace harc_rt;

int main(int argc, char** argv) {
    const char* mode = argc > 1 ? argv[1] : "";
    if (std::strcmp(mode, "abort") == 0) {
        volatile uint64_t zero = 0;
        uint64_t q = harc_checked_div((uint64_t)5, (uint64_t)zero);
        std::printf("UNREACHABLE %llu\n", (unsigned long long)q);
        return 0;
    }

    // Shifts on the <= 64-bit tier.
    volatile uint64_t n64 = 64, n63 = 63, n200 = 200;
    std::printf("shl %llu %llu %llu\n",
        (unsigned long long)harc_shl_u64(1, n64),
        (unsigned long long)harc_shl_u64(1, n63),
        (unsigned long long)harc_shl_u64(3, n200));
    std::printf("shr %llu %llu\n",
        (unsigned long long)harc_shr_u64(~0ULL, n64),
        (unsigned long long)harc_shr_u64(~0ULL, n63));
    std::printf("ashr %lld %lld %lld\n",
        (long long)harc_ashr_i64(-8, n64),
        (long long)harc_ashr_i64(8, n64),
        (long long)harc_ashr_i64(-8, 2));
    // Declared-width (W < 64) shifts keep W bits and saturate at W.
    volatile uint64_t n1 = 1, n3 = 3, n4 = 4, n7 = 7, n8 = 8, n9 = 9;
    std::printf("shlw %llu %llu %llu %lld %lld\n",
        (unsigned long long)harc_shl_uw(9, n1, 4),
        (unsigned long long)harc_shl_uw(9, n3, 4),
        (unsigned long long)harc_shl_uw(9, n4, 4),
        (long long)harc_shl_iw(1, n7, 8),
        (long long)harc_shl_iw(1, n8, 8));
    std::printf("shrw %llu %llu %lld %lld %lld\n",
        (unsigned long long)harc_shr_uw(0x80, n7, 8),
        (unsigned long long)harc_shr_uw(0x80, n9, 8),
        (long long)harc_ashr_iw(-128, n7, 8),
        (long long)harc_ashr_iw(-128, n9, 8),
        (long long)harc_ashr_iw(64, n9, 8));

    std::vector<std::string> faults;
    HarcArithFatalScope scope([&](const char* message) { faults.push_back(message); });
    auto drain = [&](const char* label) {
        std::printf("%s", label);
        for (auto& f : faults) std::printf(" [%s]", f.c_str());
        std::printf("\n");
        faults.clear();
    };

    // <= 64-bit tier.
    volatile uint64_t uz = 0;
    volatile int64_t sz = 0, m1 = -1;
    unsigned long long u = harc_checked_div((uint64_t)200, (uint64_t)uz);
    unsigned long long um = harc_checked_mod((uint64_t)200, (uint64_t)uz);
    long long s8 = harc_sdiv_i64(-128, m1, 8);
    long long s8m = harc_smod_i64(-128, m1, 8);
    long long s64 = harc_sdiv_i64(INT64_MIN, m1, 64);
    long long s64z = harc_smod_i64(5, sz, 64);
    std::printf("narrow %llu %llu %lld %lld %lld %lld\n", u, um, s8, s8m, s64 == INT64_MIN, s64z);
    drain("narrow-faults");
    // Ordinary operands do not fault: -128 / -1 at width 16 is 128.
    std::printf("narrow-ok %lld %lld %lld %llu\n",
        (long long)harc_sdiv_i64(-128, -1, 16), (long long)harc_sdiv_i64(-7, 2, 8),
        (long long)harc_smod_i64(-7, 2, 8), (unsigned long long)harc_checked_div((uint64_t)200, (uint64_t)7));
    drain("narrow-ok-faults");

    // 65..128-bit tier.
    _harc_u128 min100 = (_harc_u128)1 << 99;
    _harc_u128 neg1_100 = harc_mask_u128(100);
    (void)harc_sdiv_u128_checked(min100, neg1_100, 100);
    (void)harc_smod_u128_checked(min100, neg1_100, 100);
    (void)harc_sdiv_u128_checked(7, 0, 100);
    // Bits above the declared width are padding: a divisor of 2^100 is zero at width 100.
    (void)harc_smod_u128_checked(7, (_harc_u128)1 << 100, 100);
    (void)harc_checked_div((_harc_u128)7, (_harc_u128)0);
    drain("u128-faults");
    std::printf("u128-ok %d\n",
        (int)(harc_sdiv_u128_checked(harc_mask_u128(100) - 6, 2, 100) == harc_mask_u128(100) - 2));
    drain("u128-ok-faults");

    // > 128-bit tier.
    HarcWide<10> zero;
    HarcWide<10> seven(7);
    HarcWide<10> min300;
    harc_wide_set_bit(min300, 299);
    HarcWide<10> neg1_300 = harc_wide_mask_bits(~HarcWide<10>(), 300);
    (void)harc_wide_sdiv_checked(min300, neg1_300, 300);
    (void)harc_wide_smod_checked(min300, neg1_300, 300);
    (void)harc_wide_sdiv_checked(seven, zero, 300);
    (void)harc_checked_div(seven, zero);
    (void)harc_checked_mod(seven, zero);
    drain("wide-faults");
    std::printf("wide-ok %d\n", (int)(harc_checked_div(seven, HarcWide<10>(2)) == HarcWide<10>(3)));
    drain("wide-ok-faults");
    return 0;
}
"#;

#[test]
fn runtime_shift_saturation_and_division_faults() {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++). This test compiles \
             harc_thread_rt.h to pin runtime shift saturation and division faults. \
             Install a compiler, or set HARC_SKIP_CXX_PROBE=1 to skip deliberately."
        );
        return;
    };

    let dir =
        std::env::temp_dir().join(format!("harc_runtime_arith_guards_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    let probe = dir.join("probe.cpp");
    std::fs::write(&probe, PROBE).expect("write probe");
    let bin = dir.join("probe");
    let build = Command::new(&cxx)
        .arg("-std=gnu++20")
        .arg("-I")
        .arg(repo_root().join("runtime"))
        .arg(&probe)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run C++ compiler");
    assert!(
        build.status.success(),
        "arithmetic runtime probe did not compile:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );

    let out = Command::new(&bin).output().expect("run probe");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "probe exited non-zero:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let div0 = "runtime arithmetic error: division by zero";
    let mod0 = "runtime arithmetic error: modulo by zero";
    let divov = "runtime arithmetic error: signed division overflow (MIN / -1)";
    let modov = "runtime arithmetic error: signed modulo overflow (MIN % -1)";
    let expected = [
        "shl 0 9223372036854775808 0".to_string(),
        "shr 0 1".to_string(),
        "ashr -1 0 -2".to_string(),
        "shlw 2 8 0 -128 0".to_string(),
        "shrw 1 0 -1 -1 0".to_string(),
        "narrow 0 0 -128 0 1 0".to_string(),
        format!("narrow-faults [{div0}] [{mod0}] [{divov}] [{modov}] [{divov}] [{mod0}]"),
        "narrow-ok 128 -3 -1 28".to_string(),
        "narrow-ok-faults".to_string(),
        format!("u128-faults [{divov}] [{modov}] [{div0}] [{mod0}] [{div0}]"),
        "u128-ok 1".to_string(),
        "u128-ok-faults".to_string(),
        format!("wide-faults [{divov}] [{modov}] [{div0}] [{div0}] [{mod0}]"),
        "wide-ok 1".to_string(),
        "wide-ok-faults".to_string(),
    ];
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, expected, "probe output:\n{stdout}");

    // No reporter installed: the fault aborts instead of returning a value.
    let out = Command::new(&bin).arg("abort").output().expect("run probe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && !stdout.contains("UNREACHABLE"), "{stdout}\n{stderr}");
    assert!(stderr.contains(&format!("HARC-ERROR: {div0}")), "{stderr}");
}
