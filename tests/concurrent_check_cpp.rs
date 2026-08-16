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

/// A test-scope event channel: two subscribers on one channel, then two
/// emits. Pins the fan-out order and that every subscriber sees every
/// emit — a `push_back` that captured the wrong thing, or a fan-out that
/// stopped at the first subscriber, would still string-match.
const EVENT_TB: &str = r#"testbench EvTb
    dut : Top
end testbench EvTb

impl EvTest for EvTb
    run
        let e : event<uint<8>>
        on e(v)
            log(info, "a")
        end on
        on e(w)
            log(info, "b")
        end on
        emit e(1)
        emit e(2)
        wait 1 cycle
    end run
end impl EvTest
"#;

/// Statement-position `on` handlers: a rising-edge trigger and a
/// periodic one. Both arm a `_checkers` closure at the statement
/// position, like v1's `emit_cycle_trigger`.
const ON_TB: &str = r#"testbench OnTb
    dut : Top
end testbench OnTb

impl OnTest for OnTb
    run
        on dut.c == 1
            log(info, "rose")
        end on
        on 2 cycles
            log(info, "tick")
        end on
        wait 1 cycle
    end run
end impl OnTest
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
    emit_src(dir, "chk_tb", TB)
}

fn emit_src(dir: &Path, stem: &str, src: &str) -> String {
    let tb = dir.join(format!("{stem}.harc"));
    std::fs::write(&tb, src).expect("write TB");
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
    std::fs::read_to_string(dir.join(format!("{stem}.cpp"))).expect("read emitted cpp")
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

/// The same treatment for statement-position `on` handlers: splice the
/// emitted `_checkers` registrations plus the body lambdas they call into
/// a probe and drive cycles. Pins the edge latch (a rising-edge handler
/// must fire once per 0→1 transition, not once per cycle the predicate
/// holds) and the periodic stamp (first firing at cycle N, not cycle 0).
#[test]
fn emitted_on_handlers_compile_and_fire_on_the_right_cycles() {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++)."
        );
        eprintln!("SKIP emitted_on_handlers_compile_and_fire_on_the_right_cycles.");
        return;
    };

    let dir = fresh_outdir("onprobe");
    let cpp = emit_src(&dir, "on_tb", ON_TB);
    let closures = checker_closures(&cpp);
    assert_eq!(closures.len(), 2, "one closure per handler; got:\n{cpp}");

    // The body lambdas are declared at test scope; lift their
    // declarations verbatim so the probe calls the real emitted bodies.
    // Each is `std::function<void()> _on_handler_N; _on_handler_N = [&]() -> void {…};`
    let mut bodies = String::new();
    for i in 0..2 {
        let decl = format!("std::function<void()> _on_handler_{i};");
        assert!(cpp.contains(&decl), "missing `{decl}` in:\n{cpp}");
        let at = cpp.find(&format!("_on_handler_{i} = [&]")).expect("body");
        let from = &cpp[at..];
        let open = from.find('{').expect("body opens");
        let mut depth = 0usize;
        let mut end = 0usize;
        for (j, ch) in from[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + j;
                        break;
                    }
                }
                _ => {}
            }
        }
        bodies.push_str(&decl);
        bodies.push('\n');
        bodies.push_str(&from[..end + 1]);
        bodies.push_str(";\n");
    }

    // Stimulus: c = 0,1,1,0,1 over five edges.
    //   rising `on dut.c == 1` fires at cycles 1 and 4 → 2 firings.
    //   `on 2 cycles` with `last = 0` fires when cycle_count - last >= 2.
    //     The probe bumps cycle_count before running the checkers, so
    //     cycle_count is 1..5 → fires at 2 and 4 → 2 firings.
    let probe = dir.join("probe.cpp");
    let body = closures.join("\n");
    std::fs::write(
        &probe,
        format!(
            r#"#include "harc_thread_rt.h"
#include <cstdarg>
#include <cstdio>
#include <functional>
#include <vector>

struct Dut {{ uint64_t a = 0, b = 0, c = 0; }};
static Dut _dut_storage;
static Dut* dut = &_dut_storage;
static struct {{ long errors = 0; }} ctx;
static long long cycle_count = 0;
static long rose_hits = 0, tick_hits = 0;
static void sim_log_line(const char* sev, const char* fmt, ...) {{
    (void)sev;
    if (fmt[0] == 'r') rose_hits++; else tick_hits++;
}}

int main() {{
    std::vector<std::function<void()>> _checkers;
{bodies}
{body}
    const uint64_t stim[5] = {{0, 1, 1, 0, 1}};
    for (int i = 0; i < 5; i++) {{
        dut->c = stim[i];
        cycle_count++;
        for (auto& c : _checkers) c();
    }}
    printf("%ld %ld\n", rose_hits, tick_hits);
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
        "the emitted `on` handler closures did not compile:\n{}\n--- probe ---\n{}",
        String::from_utf8_lossy(&compile.stderr),
        std::fs::read_to_string(&probe).unwrap_or_default(),
    );
    let run = Command::new(&bin).output().expect("run the probe");
    assert!(run.status.success(), "probe crashed");
    let got = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert_eq!(
        got, "2 2",
        "expected 2 rising-edge firings and 2 periodic firings; got `{got}`"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Build the emitted event-channel declaration, subscriptions, and
/// fan-out against stub subscriber bodies, then check that each of two
/// emits reached both subscribers in subscription order.
#[test]
fn emitted_event_channel_fans_out_to_every_subscriber() {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++)."
        );
        eprintln!("SKIP emitted_event_channel_fans_out_to_every_subscriber.");
        return;
    };

    let dir = fresh_outdir("evprobe");
    let cpp = emit_src(&dir, "ev_tb", EVENT_TB);
    // Pull the channel declaration plus every statement that touches it,
    // in emitted order, so the probe runs the real sequence.
    let decl = cpp
        .lines()
        .find(|l| l.contains("std::vector<std::function<void(uint64_t)>> e;"))
        .unwrap_or_else(|| panic!("missing the channel declaration in:\n{cpp}"))
        .trim()
        .to_string();
    let ops: Vec<String> = cpp
        .lines()
        .filter(|l| l.contains("e.push_back(") || l.contains("for (auto& _s : e)"))
        .map(|l| l.trim().to_string())
        .collect();
    assert_eq!(
        ops.len(),
        4,
        "two subscriptions and two emits, in order; got {ops:?}"
    );

    let probe = dir.join("probe.cpp");
    let ops_src = ops.join("\n    ");
    std::fs::write(
        &probe,
        format!(
            r#"#include <cstdint>
#include <cstdio>
#include <functional>
#include <vector>

// Stand-ins for the test-scope subscriber lambdas the emitter declares.
static long long seen_a = 0, seen_b = 0;
static void _on_event_0(uint64_t v) {{ seen_a += (long long)v; }}
static void _on_event_1(uint64_t v) {{ seen_b += (long long)v * 10; }}

int main() {{
    {decl}
    {ops_src}
    printf("%lld %lld\n", seen_a, seen_b);
    return 0;
}}
"#
        ),
    )
    .expect("write probe");

    let bin = dir.join("probe");
    let compile = Command::new(&cxx)
        .arg("-std=c++20")
        .arg(&probe)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run the C++ compiler");
    assert!(
        compile.status.success(),
        "the emitted event-channel statements did not compile:\n{}\n--- probe ---\n{}",
        String::from_utf8_lossy(&compile.stderr),
        std::fs::read_to_string(&probe).unwrap_or_default(),
    );
    let run = Command::new(&bin).output().expect("run the probe");
    assert!(run.status.success(), "probe crashed");
    let got = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert_eq!(
        got, "3 30",
        "both subscribers must see both emits (1+2 and 10*(1+2)); got `{got}`"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
