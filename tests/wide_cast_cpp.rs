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
//! This test pins the emitted statements for both backends and then feeds
//! those same statements to the host C++ compiler against the real
//! runtime header, so a regression fails here rather than in a Verilator
//! run nobody runs locally.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Statements the wide-cast fixture must emit, shared by the
/// string-match assertions and the compiled probe below. `src130` is a
/// `uint<130>` (`HarcWide<5>`), `w200` a `uint<256>` slot
/// (`HarcWide<8>`), `low8` a plain `uint64_t`.
const EMITTED_STMTS: [&str; 2] = [
    "w200 = harc_rt::harc_wide_zext<7>(src130, 130)",
    "low8 = ((uint64_t)(((uint64_t)(src130) & 0xFFULL)))",
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
fn both_backends_emit_the_pinned_wide_cast_statements() {
    for codegen in ["v1", "tbir"] {
        let dir = fresh_outdir(codegen);
        let cpp = emit(codegen, &dir);
        for stmt in EMITTED_STMTS {
            assert!(
                cpp.contains(stmt),
                "[{codegen}] expected emitted statement `{stmt}`; got:\n{cpp}"
            );
        }
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
fn emitted_wide_cast_statements_compile_and_evaluate() {
    let Some(cxx) = cxx() else {
        eprintln!(
            "SKIP emitted_wide_cast_statements_compile_and_evaluate: no C++ compiler on PATH. \
             This test compiles the emitted wide-cast statements against harc_thread_rt.h."
        );
        return;
    };

    let dir = fresh_outdir("probe");
    let probe = dir.join("probe.cpp");
    std::fs::write(
        &probe,
        format!(
            r#"#include "harc_thread_rt.h"
#include <cstdio>
int main() {{
    harc_rt::HarcWide<5> src130 = 0x3FF;   // uint<130>
    harc_rt::HarcWide<8> w200 = 0;         // uint<256> slot
    uint64_t low8 = 0;
    {};
    {};
    printf("%d %llu\n", (int)(w200 == 0x3FF), (unsigned long long)low8);
    return 0;
}}
"#,
            EMITTED_STMTS[0], EMITTED_STMTS[1]
        ),
    )
    .expect("write probe");

    let bin = dir.join("probe");
    let build = Command::new(&cxx)
        .arg("-std=c++20")
        .arg("-fcoroutines")
        .arg("-I")
        .arg(repo_root().join("runtime"))
        .arg(&probe)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run C++ compiler");
    assert!(
        build.status.success(),
        "emitted wide-cast statements did not compile:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );

    let run = Command::new(&bin).output().expect("run probe");
    assert!(run.status.success(), "probe exited non-zero");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        "1 255",
        "wide-cast statements compiled but computed the wrong values",
    );
}
