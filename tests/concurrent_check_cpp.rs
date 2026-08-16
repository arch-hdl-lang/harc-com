//! C++-level gate for the concurrent `assert`/`assume`/`cover` emission
//! (spec §5).
//!
//! The per-cycle `_checkers` closures TB-IR emits for a concurrent check
//! only run end-to-end under Verilator, which `cargo test` never invokes.
//! Everything else about them is string-matched in `tests/tbir.rs`, and a
//! string match cannot tell a correct latch state machine from one that
//! reads its own write. This test takes the closures the emitter actually
//! produces, splices them verbatim into a probe with a stub DUT, drives a
//! fixed stimulus sequence, and checks the resulting error count, ASSUME
//! log count, and cover hit count.
//!
//! It also pins the thing v1 gets wrong: the `_cov_<tag>_hits` counter is
//! declared at FILE scope, so a translation unit that reads it from the
//! end-of-test summary (outside the run coroutine) compiles. v1 declares
//! the same counter as a `static` local inside the coroutine lambda and
//! then reads it from the enclosing function, which does not.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Four concurrent checks covering every shape the emitter classifies:
/// a named-property same-cycle implication, an inline next-cycle
/// implication, a cover with a temporal latch, and an `assume` (which
/// must log without bumping the error counter).
const TB: &str = r#"property a_implies_b
    dut.a |-> dut.b
end property a_implies_b

testbench ChkTb
    dut : Top
end testbench ChkTb

impl ChkTest for ChkTb
    run
        assert property a_implies_b
        assert dut.a |=> dut.b
        cover rose(dut.c)
        assume dut.a |-> dut.b
        wait 1 cycle
    end run
end impl ChkTest
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fresh_outdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_concurrent_check_cpp_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    dir
}

fn emit(dir: &Path) -> String {
    let tb = dir.join("chk_tb.harc");
    std::fs::write(&tb, TB).expect("write TB");
    let out = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg("--codegen")
        .arg("tbir")
        .arg("--emit-only")
        .arg("--outdir")
        .arg(dir)
        .output()
        .expect("run harc sim --emit-only");
    assert!(
        out.status.success(),
        "emit failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    std::fs::read_to_string(dir.join("chk_tb.cpp")).expect("read emitted cpp")
}

/// Pull out every `_checkers.push_back([&]() { … });` registration, by
/// brace matching from the opening `{` of the lambda. The bodies are
/// spliced verbatim into the probe, so any emitter drift is compiled.
fn checker_closures(cpp: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cpp;
    while let Some(at) = rest.find("_checkers.push_back(") {
        let from = &rest[at..];
        let open = from.find('{').expect("lambda body opens");
        let mut depth = 0usize;
        let mut end = None;
        for (i, ch) in from[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.expect("lambda body closes");
        // Include the trailing `});`.
        let close = from[end..].find(");").expect("registration closes") + end + 2;
        out.push(from[..close].to_string());
        rest = &from[close..];
    }
    out
}

/// The file-scope cover-counter declarations.
fn cover_counter_decls(cpp: &str) -> Vec<String> {
    cpp.lines()
        .filter(|l| l.starts_with("static uint64_t _cov_") && l.ends_with("_hits = 0;"))
        .map(|l| l.to_string())
        .collect()
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

#[test]
fn emitted_concurrent_checks_compile_and_evaluate() {
    // A silent skip would let a latch-state regression ship on any machine
    // without a compiler, so an absent toolchain fails unless the operator
    // opts out explicitly — same policy as `wide_cast_cpp.rs`.
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++). This test compiles the \
             emitted concurrent-check closures against harc_thread_rt.h — without it the \
             `_checkers` state machines are untested outside the Verilator fixture job. \
             Install a compiler, or set HARC_SKIP_CXX_PROBE=1 to skip deliberately."
        );
        eprintln!("SKIP emitted_concurrent_checks_compile_and_evaluate: HARC_SKIP_CXX_PROBE set.");
        return;
    };

    let dir = fresh_outdir("probe");
    let cpp = emit(&dir);
    let closures = checker_closures(&cpp);
    assert_eq!(
        closures.len(),
        4,
        "one closure per concurrent check; got:\n{cpp}"
    );
    let counters = cover_counter_decls(&cpp);
    assert_eq!(
        counters.len(),
        1,
        "one file-scope cover counter; got:\n{cpp}"
    );
    let counter_name = counters[0]
        .trim_start_matches("static uint64_t ")
        .trim_end_matches(" = 0;")
        .to_string();

    // Stimulus, one row per primary-clock edge: (a, b, c). Chosen so each
    // shape fires at a different cycle, and so a latch that read its own
    // write (updating `_harc_ps` before evaluating the body) would give a
    // different cover count.
    //
    //  cyc0  a=0 b=0 c=0   nothing
    //  cyc1  a=1 b=1 c=1   rose(c): 0→1 → cover hit
    //  cyc2  a=1 b=0 c=1   `|->` fires (+1 err), `|=>` fires on cyc1's a
    //                      (+1 err), assume fires (log only), c stable
    //  cyc3  a=0 b=0 c=0   `|=>` fires on cyc2's a (+1 err); c falls
    //
    // Expected: 3 errors, 1 ASSUME-FAIL line, 1 cover hit.
    let probe = dir.join("probe.cpp");
    let body = closures.join("\n");
    let decl = &counters[0];
    std::fs::write(
        &probe,
        format!(
            r#"#include "harc_thread_rt.h"
#include <cstdarg>
#include <cstdio>
#include <functional>
#include <vector>

// Stub DUT: the ports the checks read, as plain scalars.
struct Dut {{ uint64_t a = 0, b = 0, c = 0; }};
static Dut _dut_storage;
static Dut* dut = &_dut_storage;

// The two scaffolding names a checker closure captures.
static struct {{ long errors = 0; }} ctx;
static long assume_fails = 0;
static void sim_log_line(const char* sev, const char* fmt, ...) {{
    (void)fmt;
    if (sev[0] == 'A') assume_fails++;
}}

{decl}

int main() {{
    std::vector<std::function<void()>> _checkers;
{body}
    const uint64_t stim[4][3] = {{ {{0,0,0}}, {{1,1,1}}, {{1,0,1}}, {{0,0,0}} }};
    for (int i = 0; i < 4; i++) {{
        dut->a = stim[i][0];
        dut->b = stim[i][1];
        dut->c = stim[i][2];
        for (auto& c : _checkers) c();
    }}
    printf("%ld %ld %llu\n", ctx.errors, assume_fails,
           (unsigned long long){counter_name});
    return 0;
}}
"#
        ),
    )
    .expect("write probe");

    let bin = dir.join("probe");
    let compile = Command::new(&cxx)
        .arg("-std=c++20")
        .arg("-I")
        .arg(repo_root().join("runtime"))
        .arg(&probe)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run the C++ compiler");
    assert!(
        compile.status.success(),
        "the emitted concurrent-check closures did not compile:\n{}\n--- probe ---\n{}",
        String::from_utf8_lossy(&compile.stderr),
        std::fs::read_to_string(&probe).unwrap_or_default(),
    );

    let run = Command::new(&bin).output().expect("run the probe");
    assert!(run.status.success(), "probe crashed");
    let got = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert_eq!(
        got, "3 1 1",
        "expected 3 errors / 1 ASSUME-FAIL / 1 cover hit from the emitted closures; got `{got}`"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
