//! Runtime-value gate for the mixed-operand `HarcWide` operators.
//!
//! `HarcWide<N>` converts implicitly to both `uint64_t` and
//! `_harc_u128`, so every operator it does not define for a mixed pair
//! is an AMBIGUOUS call rather than a missing one. Both backends emit
//! `w + 1` and `b + a` for scalars past 128 bits, and both produced C++
//! that g++ rejected. `harc_thread_rt.h` defines the sign-agnostic six
//! (`+ - * & | ^`) for HarcWide-with-integer and for two HarcWide values
//! of DIFFERENT widths.
//!
//! A typecheck cannot gate this. Two of the three defects here compiled
//! perfectly and computed the wrong number:
//!
//!   * `HarcWide<N>(v)` for a NEGATIVE `v` zero-filled above bit 128
//!     instead of sign-extending, so `w + (0 - 1)` answered 2^128 where
//!     `w - 1`, the same arithmetic, answered 0;
//!   * defining `< > <= >=` for a mixed pair would resolve a `sint`
//!     compare to the UNSIGNED HarcWide comparison — `expr.rs` emits a
//!     bare `<` and there is no signed-wide path outside
//!     `harc_wide_slt`. They are deliberately absent, and this test
//!     pins their absence as a compile error rather than a wrong
//!     answer.
//!
//! So the probe is built AND RUN, and its values are checked, in the
//! style of `wide_cast_cpp.rs`.

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

/// Compile `body` against the real runtime header. `None` on success,
/// the compiler's stderr otherwise.
fn build(cxx: &str, body: &str, tag: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("harc-wide-mixed-{tag}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let probe = dir.join("probe.cpp");
    std::fs::write(&probe, body).expect("write probe");
    let bin = dir.join("probe");
    let out = Command::new(cxx)
        .arg("-std=gnu++20")
        .arg("-I")
        .arg(repo_root().join("runtime"))
        .arg(&probe)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run C++ compiler");
    if out.status.success() {
        Ok(bin)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

const PRELUDE: &str = r#"
#include "harc_thread_rt.h"
#include <cstdio>
using harc_rt::HarcWide;
template<std::size_t N>
static bool all_ones(const HarcWide<N>& v) {
    for (std::size_t i = 0; i < N; ++i) if (v.words[i] != 0xFFFFFFFFu) return false;
    return true;
}
template<std::size_t N>
static bool is(const HarcWide<N>& v, uint64_t lo) {
    if (static_cast<uint64_t>(v) != lo) return false;
    for (std::size_t i = 2; i < N; ++i) if (v.words[i] != 0u) return false;
    return true;
}
"#;

#[test]
fn mixed_harcwide_operands_compile_and_compute_the_right_values() {
    // No silent skip: this gates a defect that shipped in both
    // backends, and a machine without a compiler must say so rather
    // than pass.
    let cxx = cxx().expect("a host C++ compiler is required for this test");

    let body = format!(
        r#"{PRELUDE}
int main() {{
    HarcWide<32> w = 7;         // past 128 bits: the HarcWide storage class
    HarcWide<8>  n = 5;         // a NARROWER wide value, for the mixed-width form
    int bad = 0;
    auto check = [&](const char* what, bool ok) {{
        if (!ok) {{ std::printf("FAIL %s\n", what); ++bad; }}
    }};

    // HarcWide with an integer, both argument orders.
    check("w + 1",  is(w + 1,  8));
    check("1 + w",  is(1 + w,  8));
    check("w - 1",  is(w - 1,  6));
    check("w * 3",  is(w * 3,  21));
    check("w & 6",  is(w & 6,  6));
    check("w | 8",  is(w | 8,  15));
    check("w ^ 1",  is(w ^ 1,  6));

    // A NEGATIVE integer operand must sign-extend across every word,
    // not just the low four. Zero-filling above bit 128 made this 2^128.
    check("w + (0 - 1)", is(w + (0 - 1), 6));
    check("HarcWide<32>(-1) is all ones", all_ones(HarcWide<32>(-1)));

    (void)n;
    static_assert(std::is_same_v<decltype(w + 1), HarcWide<32>>);
    static_assert(std::is_same_v<decltype(w + w), HarcWide<32>>);

    // `harc_wide_zext` ZERO-extends whatever the source's sign. The
    // converting constructor sign-extends (the `w + (0 - 1)` row above
    // depends on it), and a function named zero-extend must not inherit
    // that: zero-extending a 64-bit -1 is 2^64-1, so exactly the low two
    // words are set. It answered 2^128-1 before the constructor gained
    // sign-extension, and 2^1024-1 for one commit after.
    {{
        const auto z = harc_rt::harc_wide_zext<32>(int64_t(-1));
        check("zext<32>(-1) is not all ones", !all_ones(z));
        check("zext<32>(-1) sets exactly the low two words",
              z.words[0] == 0xFFFFFFFFu && z.words[1] == 0xFFFFFFFFu
                  && z.words[2] == 0u && z.words[31] == 0u);
        // The unsigned spelling of the same bits is unchanged.
        const auto zu = harc_rt::harc_wide_zext<32>(uint64_t(0xFFFFFFFFFFFFFFFFull));
        check("zext<32>(u64 max) matches", zu == z);
    }}

    // Equality already carried both mixed forms; kept here so the six
    // new ones are not the only thing holding the shapes up.
    check("w == 7", w == 7);
    check("w != n", w != n);

    std::printf("%d\n", bad);
    return 0;
}}
"#
    );

    let bin = match build(&cxx, &body, "values") {
        Ok(b) => b,
        Err(e) => panic!("the mixed-operand probe did not compile:\n{e}"),
    };
    let run = Command::new(&bin).output().expect("run probe");
    assert!(run.status.success(), "probe exited non-zero");
    let out = String::from_utf8_lossy(&run.stdout);
    assert_eq!(
        out.trim().lines().last().unwrap_or(""),
        "0",
        "mixed-operand HarcWide expressions compiled but computed wrong values:\n{out}"
    );
}

/// Two shapes are deliberately left UNDEFINED, and both were defined
/// once before being measured.
///
/// 1. `/ % < > <= >=` against an integer. All six are unsigned on
///    `HarcWide`, and lowering emits a bare operator for a `sint` too,
///    so defining them answers `w < 0` on a negative `sint<1024>` with
///    `false` instead of failing to build.
/// 2. ANY operator between two `HarcWide`s of different widths.
///    Sign-agnosticism does not survive a width change: widening the
///    narrower side is where the sign matters, and the C++ type does
///    not carry it. A version that widened with `harc_wide_zext`
///    answered `b + a` as `b + (2^160 - 1)` for a negative
///    `sint<160>` while `b + (-1)`, through the integer overload
///    beside it, answered correctly.
///
/// Lowering refuses both with named diagnostics
/// (`reject_unbuildable_wide_operator`). This pins the header half, so
/// nobody closes an "ambiguous overload" error by adding them back.
#[test]
fn the_sign_sensitive_shapes_stay_undefined() {
    let cxx = cxx().expect("a host C++ compiler is required for this test");
    let mut cases: Vec<(String, String)> = Vec::new();
    for op in ["/", "%", "<", ">", "<=", ">="] {
        cases.push((
            format!("HarcWide<32> {op} int"),
            format!("HarcWide<32> w = 7; auto r = w {op} 2; (void)r;"),
        ));
    }
    for op in ["+", "-", "*", "&", "|", "^", "/", "%", "<", ">", "<=", ">="] {
        cases.push((
            format!("HarcWide<32> {op} HarcWide<8>"),
            format!("HarcWide<32> w = 7; HarcWide<8> n = 5; auto r = w {op} n; (void)r;"),
        ));
    }
    for (i, (what, stmt)) in cases.iter().enumerate() {
        let body = format!("{PRELUDE}\nint main() {{ {stmt} return 0; }}\n");
        match build(&cxx, &body, &format!("nodef{i}")) {
            Ok(_) => panic!(
                "`{what}` compiles. It must not: the operation is not sign-agnostic in \
                 that shape, so defining it turns a build failure into a wrong answer. \
                 Refuse it in lowering instead."
            ),
            Err(e) => assert!(
                e.contains("ambiguous") || e.contains("no match") || e.contains("no operator"),
                "`{what}` failed to compile for an unexpected reason:\n{e}"
            ),
        }
    }
}
