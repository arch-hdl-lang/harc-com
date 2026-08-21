//! C++-level gate for the wide width-method cast shapes.
//!
//! `wide_cast_test.harc` only runs end-to-end under `run_fixtures.sh`
//! (Verilator), so nothing in `cargo test` compiles the C++ the wide-cast
//! path emits — the emitter tests string-match the helper calls and stop
//! there. Two shapes that pass every Rust-side check still failed `g++`:
//!
//!   * a cast sized by its own width (`HarcWide<7>` for `zext<200>`)
//!     assigned into a slot sized by the local's declared width
//!     (`HarcWide<8>` for `uint<256>`), and
//!   * a sub-64 mask applied straight to a `HarcWide` receiver, which
//!     converts implicitly to both `uint64_t` and `_harc_u128` and so
//!     makes `operator&` ambiguous.
//!
//! This test pins the emitted declarations and cast expressions for both
//! backends and then feeds those same strings to the host C++ compiler
//! against the real runtime header, so a regression fails here rather
//! than in a Verilator run nobody runs locally. The two halves interlock:
//! the emitters cannot drift without failing the string match, and the
//! pinned strings cannot be updated to something that does not compile.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Local declarations the emitters must produce. The destination slot is
/// sized from the local's *declared* width (`uint<256>` → 8 words),
/// independently of the cast's own width (`zext<200>` → 7 words) — that
/// mismatch is the whole point of the first shape under test, so pinning
/// the declarations is what makes the probe gate slot sizing rather than
/// just expression syntax.
const EMITTED_DECLS: [&str; 2] = ["harc_rt::HarcWide<5> src130", "harc_rt::HarcWide<8> w200"];

/// The cast expressions themselves, RHS only, so the probe can build both
/// the TB-IR shape (declare, then assign) and the v1 shape (copy-initialise
/// the slot from the cast) out of one pinned string.
const CAST_EXPRS: [&str; 2] = [
    "harc_rt::harc_wide_zext<7>(src130, 130)",
    "((uint64_t)(((uint64_t)(src130) & 0xFFULL)))",
];

const TB: &str = r#"testbench WideSlotTb
    dut : Top
end testbench WideSlotTb

impl WideSlotTest for WideSlotTb
    run
        let src130 : uint<130> = 0x3FF
        let w200 : uint<256> = src130.zext<200>()
        assert w200 == 0x3FF
            else fail("zext<200> into a uint<256> slot changed the value")
        let low8 = src130.trunc<8>()
        assert low8 == 0xFF
            else fail("trunc<8> of a HarcWide source")
    end run
end impl WideSlotTest
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fresh_outdir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("harc_wide_cast_cpp_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    dir
}

/// Emit the testbench C++ with `--emit-only` (no Verilator involved) and
/// return the generated translation unit.
fn emit(codegen: &str, dir: &Path) -> String {
    let tb = dir.join("wide_slot_tb.harc");
    std::fs::write(&tb, TB).expect("write TB");
    let out = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg("--codegen")
        .arg(codegen)
        .arg("--emit-only")
        .arg("--outdir")
        .arg(dir)
        .output()
        .expect("run harc sim --emit-only");
    assert!(
        out.status.success(),
        "[{codegen}] emit failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    std::fs::read_to_string(dir.join("wide_slot_tb.cpp")).expect("read emitted cpp")
}

#[test]
fn both_backends_emit_the_pinned_wide_cast_shapes() {
    for codegen in ["v1", "tbir"] {
        let dir = fresh_outdir(codegen);
        let cpp = emit(codegen, &dir);
        let expected = EMITTED_DECLS
            .iter()
            .map(|d| d.to_string())
            .chain([
                format!("w200 = {}", CAST_EXPRS[0]),
                format!("low8 = {}", CAST_EXPRS[1]),
            ])
            .collect::<Vec<_>>();
        for want in &expected {
            assert!(
                cpp.contains(want.as_str()),
                "[{codegen}] expected emitted text `{want}`; got:\n{cpp}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Whether a usable host C++ compiler is on PATH.
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

#[test]
fn emitted_wide_cast_shapes_compile_and_evaluate() {
    // A silent skip would let the regression this test exists for ship on
    // any machine without a compiler, so an absent toolchain fails unless
    // the operator opts out explicitly. Everything this repo produces is
    // C++ that a host compiler has to build, so requiring one is fair.
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++). This test compiles the \
             emitted wide-cast shapes against harc_thread_rt.h — without it the wide-cast \
             C++ is untested outside the Verilator fixture job. Install a compiler, or set \
             HARC_SKIP_CXX_PROBE=1 to skip deliberately."
        );
        eprintln!("SKIP emitted_wide_cast_shapes_compile_and_evaluate: HARC_SKIP_CXX_PROBE set.");
        return;
    };

    let dir = fresh_outdir("probe");
    let probe = dir.join("probe.cpp");
    // Both emitter shapes: TB-IR declares the slot and assigns into it,
    // v1 copy-initialises the slot from the cast. Each needs the widening
    // `HarcWide<7>` → `HarcWide<8>` conversion, but they are distinct C++
    // constructs (`operator=` vs a converting constructor), so pinning
    // only one would leave the other untested.
    std::fs::write(
        &probe,
        format!(
            r#"#include "harc_thread_rt.h"
#include <cstdio>
int main() {{
    int ok = 0;
    {{  // TB-IR shape: declare, then assign
        {decl_src} = 0;
        {decl_slot} = 0;
        uint64_t low8 = 0;
        src130 = 0x3FF;
        w200 = {cast};
        low8 = {mask};
        ok += (w200 == 0x3FF) && (low8 == 0xFF);
    }}
    {{  // v1 shape: copy-initialise the slot from the cast
        {decl_src} = 0x3FF;
        {decl_slot} = {cast};
        uint64_t low8 = {mask};
        ok += (w200 == 0x3FF) && (low8 == 0xFF);
    }}
    {{  // Signed wide comparison + arithmetic shift used by cover expressions
        auto neg200 = harc_rt::harc_wide_sext<7>(0xF, 4, 200);
        auto pos200 = harc_rt::harc_wide_sext<7>(0x1, 4, 200);
        auto shifted = harc_rt::harc_wide_ashr(neg200, 1, 200);
        auto shifted_by_wide_source = harc_rt::harc_wide_mask_bits(
            (pos200 << harc_rt::harc_wide_shift_count(pos200, 200)), 200);
        harc_rt::HarcWide<7> high_count;
        harc_rt::harc_wide_set_bit(high_count, 100);
        auto neg_fifteen = harc_rt::harc_wide_sext<7>(0xF1, 8, 200);
        auto pos_two = harc_rt::harc_wide_sext<7>(0x2, 8, 200);
        auto neg_seven = harc_rt::harc_wide_sext<7>(0xF9, 8, 200);
        auto neg_one = harc_rt::harc_wide_sext<7>(0xFF, 8, 200);
        const _harc_u128 neg65 = harc_rt::harc_sext_u128(0xF, 4, 65);
        ok += harc_rt::harc_wide_slt(neg200, pos200, 200)
            && harc_rt::harc_wide_get_bit(shifted, 199)
            && (shifted_by_wide_source == 2)
            && (harc_rt::harc_wide_shift_count(high_count, 200) == 200)
            && (harc_rt::harc_u128_shift_count((_harc_u128{{1}} << 100), 65) == 65)
            && (harc_rt::harc_wide_sdiv(neg_fifteen, pos_two, 200) == neg_seven)
            && (harc_rt::harc_wide_smod(neg_fifteen, pos_two, 200) == neg_one)
            && (harc_rt::harc_sdiv_u128(harc_rt::harc_sext_u128(0xF1, 8, 65), 2, 65)
                == harc_rt::harc_sext_u128(0xF9, 8, 65))
            && (harc_rt::harc_smod_u128(harc_rt::harc_sext_u128(0xF1, 8, 65), 2, 65)
                == harc_rt::harc_sext_u128(0xFF, 8, 65))
            && harc_rt::harc_slt_u128(neg65, 1, 65);
    }}
    printf("%d\n", ok);
    return 0;
}}
"#,
            decl_src = EMITTED_DECLS[0],
            decl_slot = EMITTED_DECLS[1],
            cast = CAST_EXPRS[0],
            mask = CAST_EXPRS[1],
        ),
    )
    .expect("write probe");

    let bin = dir.join("probe");
    let build = Command::new(&cxx)
        // Match the real build: `run_verilator` (`src/main.rs`) sets
        // `CFG_CXXFLAGS_STD=-std=gnu++20`. That standard level is enough
        // for the coroutine machinery the runtime header pulls in, and
        // unlike `-fcoroutines` it is not gcc-specific — CI compiles the
        // fixtures with clang++.
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
        "emitted wide-cast shapes did not compile:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );

    let run = Command::new(&bin).output().expect("run probe");
    assert!(run.status.success(), "probe exited non-zero");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "3",
        "wide-cast shapes compiled but computed the wrong values",
    );
    let _ = std::fs::remove_dir_all(&dir);
}
