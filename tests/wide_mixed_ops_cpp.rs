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

    // Two wide values of DIFFERENT widths: no `N` deduces for the
    // homogeneous operator, so this is its own overload set. The result
    // takes the wider of the two.
    check("w + n", is(w + n, 12));
    check("n + w", is(n + w, 12));
    check("w - n", is(w - n, 2));
    check("w * n", is(w * n, 35));
    check("w & n", is(w & n, 5));
    check("w | n", is(w | n, 7));
    check("w ^ n", is(w ^ n, 2));
    static_assert(std::is_same_v<decltype(w + n), HarcWide<32>>);
    static_assert(std::is_same_v<decltype(n + w), HarcWide<32>>);
    static_assert(std::is_same_v<decltype(w + 1), HarcWide<32>>);

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

/// `/ % < > <= >=` are deliberately NOT defined for a mixed pair.
///
/// All six are unsigned on `HarcWide`, and lowering emits a bare
/// operator for a `sint` too, so defining them here would answer `w < 0`
/// on a negative `sint<1024>` with `false` instead of failing to build.
/// Lowering refuses them with a named diagnostic
/// (`reject_unbuildable_wide_operator`); this pins the header half, so
/// nobody closes the "ambiguous overload" error by adding them back.
#[test]
fn the_sign_sensitive_operators_stay_undefined_for_mixed_operands() {
    let cxx = cxx().expect("a host C++ compiler is required for this test");
    for op in ["/", "%", "<", ">", "<=", ">="] {
        let body = format!(
            "{PRELUDE}\nint main() {{ HarcWide<32> w = 7; auto r = w {op} 2; (void)r; return 0; }}\n"
        );
        let tag = format!("nodef{}", op.chars().map(|c| c as u32).sum::<u32>());
        match build(&cxx, &body, &tag) {
            Ok(_) => panic!(
                "`HarcWide<32> {op} int` compiles. It must not: `{op}` is unsigned on \
                 HarcWide and lowering emits it for `sint` too, so defining it turns a \
                 build failure into a wrong answer. Refuse it in lowering instead."
            ),
            Err(e) => assert!(
                e.contains("ambiguous") || e.contains("no match"),
                "`{op}` failed to compile for an unexpected reason:\n{e}"
            ),
        }
    }
}
