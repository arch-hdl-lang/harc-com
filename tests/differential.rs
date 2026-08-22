//! v1-differential verdict checks: measure what v1 does with a program,
//! and hold TB-IR's verdict about that program to it.
//!
//! # Why this exists
//!
//! A `LowerError` is a CLAIM ABOUT V1. `Unsupported` renders "re-run
//! with `--codegen v1`", which promises v1 handles the program;
//! `Invalid` says no backend runs it. Those claims were being made by
//! hand, from one probed example per arm, and they were wrong
//! repeatedly — an arm guarded on "is this an event field" was labelled
//! from `event<uint<8>>` and also admitted `event<Color>`, which v1
//! emits and g++ refuses with five errors.
//!
//! Writing the test first would not have caught that: the same
//! incomplete enumeration produces the same incomplete test. What
//! catches it is asking the ORACLE — v1 plus a C++ compiler — across a
//! type space enumerated mechanically rather than from memory.
//!
//! # What can soundly be asserted
//!
//! An arm's verdict is the WORST thing v1 does anywhere under it, so
//! most per-landing checks are one-directional. Given a single program:
//!
//!   * tbir says `Unsupported` => v1 MUST compile it. `Unsupported` is
//!     universally quantified over the arm ("v1 handles this"), so one
//!     landing that fails to compile falsifies it. This is where every
//!     mislabel in this series lived.
//!   * tbir says `Invalid` => v1 must NOT compile it. Same shape,
//!     other direction.
//!   * tbir LOWERS it => v1 must compile it, or tbir accepts a program
//!     the escape hatch cannot build.
//!   * tbir says `NotImplemented{..}` => nothing per-landing. The label
//!     is the arm's worst, so an individual landing under it may
//!     compile perfectly well (`queue<T> default q0` does).
//!
//! Only the falsifiable directions are asserted. The rest is reported.
//!
//! # What this CANNOT see — validated, not assumed
//!
//! Three historical bugs from this series were re-introduced to check
//! the harness against them. It caught one and missed two, and the
//! misses are structural rather than fixable:
//!
//!   * CAUGHT — a whole event arm labelled `Unsupported` from one
//!     probe, admitting `event<Color>`, which v1 emits and the
//!     compiler rejects. This is the false-promise shape, and it is
//!     what the harness is for.
//!   * MISSED — a bare `event` OVER-refused as
//!     `NotImplemented{SilentlyMisLowers}`. v1 handles it, but under
//!     worst-wins a single landing cannot falsify that label, so no
//!     assertion may fire. Over-refusals appear in the report below
//!     instead; that list doubles as the real-gap inventory.
//!   * MISSED — a directional module handle that started lowering,
//!     with TB-IR silently dropping the `in` marker. v1 drops it too
//!     and compiles, so both backends agree on C++ that does not mean
//!     what was written. A typecheck cannot see semantics.
//!
//! A green run therefore means "no verdict promises a v1 that does not
//! typecheck". It does NOT mean the verdicts are right.
//!
//! Skipped when no C++ compiler is on PATH, so CI without one is
//! unaffected.

use harc::codegen::{cpp_tb, merge};
use harc::ir::lower;
use harc::parser::parse_source;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What v1 measurably does with a program.
#[derive(Debug, PartialEq, Eq)]
enum V1Behaviour {
    /// v1 refused to emit at all.
    Refuses,
    /// v1 emitted C++ and it typechecks.
    Compiles,
    /// v1 emitted C++ and the compiler rejected it.
    EmitsUncompilable(String),
}

/// What TB-IR says about the same program.
#[derive(Debug)]
enum TbVerdict {
    /// Lowered, and its own emitted C++ typechecks.
    Lowers,
    /// Lowered into C++ the compiler rejects. Always a defect: the
    /// backend produced something no one can build, with no diagnostic.
    LowersUncompilable(String),
    Unsupported(String),
    NotImplemented(lower::V1Status, #[allow(dead_code)] String),
    Invalid(String),
}

fn cxx() -> Option<&'static str> {
    for c in ["g++", "c++", "clang++"] {
        if Command::new(c)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Some(c);
        }
    }
    None
}

fn manifest(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn tb_verdict(cc: &str, src: &str, dir: &Path, stem: &str) -> TbVerdict {
    let parsed = match parse_source(src) {
        Ok(p) => p,
        Err(e) => panic!("probe does not parse — fix the template, not the compiler: {e}"),
    };
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    match lower::lower_program(&merged) {
        // Lowering is only half of it. A backend that lowers into C++
        // nobody can compile has produced a defect with no diagnostic,
        // so the emitted output is typechecked too — the suite had no
        // way to notice that before this harness existed.
        Ok(prog) => match harc::codegen::tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()) {
            Err(e) => TbVerdict::LowersUncompilable(format!("tbir emitter refused: {e}")),
            Ok(cpp) => match compile(cc, &cpp, dir, &format!("{stem}-tbir")) {
                None => TbVerdict::Lowers,
                Some(err) => TbVerdict::LowersUncompilable(err),
            },
        },
        Err(lower::LowerError::Unsupported { construct, .. }) => TbVerdict::Unsupported(construct),
        Err(lower::LowerError::NotImplemented { construct, v1, .. }) => {
            TbVerdict::NotImplemented(v1, construct)
        }
        Err(lower::LowerError::Invalid(m)) => TbVerdict::Invalid(m),
    }
}

fn v1_behaviour(cc: &str, src: &str, dir: &Path, stem: &str) -> V1Behaviour {
    let parsed = parse_source(src).expect("probe parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = match cpp_tb::emit(&merged) {
        Ok(c) => c,
        Err(_) => return V1Behaviour::Refuses,
    };
    match compile(cc, &cpp, dir, stem) {
        None => V1Behaviour::Compiles,
        Some(err) => V1Behaviour::EmitsUncompilable(err),
    }
}

/// `None` when the emitted C++ typechecks; the first `error:` line
/// otherwise.
fn compile(cc: &str, cpp: &str, dir: &Path, stem: &str) -> Option<String> {
    let path = dir.join(format!("{stem}.cpp"));
    std::fs::write(&path, cpp).expect("write emitted C++");
    let out = Command::new(cc)
        .args(["-std=gnu++20", "-fcoroutines", "-fsyntax-only"])
        .arg("-I")
        .arg(manifest("runtime"))
        .arg("-I")
        .arg(manifest("tests/vstub"))
        .arg(&path)
        .output()
        .expect("spawn compiler");
    if out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| l.contains("error:"))
            .unwrap_or("(no error: line)")
            .to_string(),
    )
}

/// Run every substitution through both backends and hold TB-IR's
/// verdict to what v1 measurably does. Returns a human-readable table
/// so a passing run still shows the shape of the space.
fn check_space(label: &str, template: &str, hole: &str, subs: &[&str]) -> String {
    let Some(cc) = cxx() else {
        eprintln!("no C++ compiler on PATH — skipping {label}");
        return String::new();
    };
    let dir = std::env::temp_dir().join(format!("harc-diff-{label}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");

    // The CONTROL: the template with the hole emptied. A broken
    // skeleton has produced false "v1 refuses" readings more than once,
    // and every row is meaningless if the control does not pass.
    let control = template.replace(hole, "");
    assert!(
        matches!(tb_verdict(cc, &control, &dir, "control"), TbVerdict::Lowers),
        "{label}: the control must lower — the template is broken, not the compiler"
    );
    assert_eq!(
        v1_behaviour(cc, &control, &dir, "control"),
        V1Behaviour::Compiles,
        "{label}: the control must compile under v1"
    );

    let mut table = format!("\n{label}: control lowers and compiles\n");
    let mut failures = Vec::new();
    let mut gaps: Vec<String> = Vec::new();
    for (i, sub) in subs.iter().enumerate() {
        let src = template.replace(hole, sub);
        let tb = tb_verdict(cc, &src, &dir, &format!("row{i}"));
        let v1 = v1_behaviour(cc, &src, &dir, &format!("row{i}"));
        table.push_str(&format!(
            "  {:<34} tbir={:<22} v1={:?}\n",
            sub.trim(),
            match &tb {
                TbVerdict::Lowers => "LOWERS".to_string(),
                TbVerdict::LowersUncompilable(_) => "LOWERS-BROKEN".to_string(),
                TbVerdict::Unsupported(_) => "Unsupported".to_string(),
                TbVerdict::NotImplemented(s, _) => format!("{s:?}"),
                TbVerdict::Invalid(_) => "Invalid".to_string(),
            },
            match &v1 {
                V1Behaviour::Compiles => "compiles".to_string(),
                V1Behaviour::Refuses => "refuses".to_string(),
                V1Behaviour::EmitsUncompilable(_) => "emits, g++ refuses".to_string(),
            }
        ));

        // The three falsifiable directions.
        match (&tb, &v1) {
            (TbVerdict::Unsupported(c), V1Behaviour::EmitsUncompilable(e)) => failures.push(
                format!("`{}`: tbir promises `--codegen v1` for `{c}`, but v1's output does not compile: {e}", sub.trim()),
            ),
            (TbVerdict::Unsupported(c), V1Behaviour::Refuses) => failures.push(format!(
                "`{}`: tbir promises `--codegen v1` for `{c}`, but v1 refuses the program",
                sub.trim()
            )),
            (TbVerdict::Invalid(m), V1Behaviour::Compiles) => failures.push(format!(
                "`{}`: tbir says `Invalid` ({m}), but v1 compiles it",
                sub.trim()
            )),
            // TB-IR lowered into C++ that does not build. Always a
            // defect, whatever v1 does — no diagnostic, no output.
            (TbVerdict::LowersUncompilable(e), _) => failures.push(format!(
                "`{}`: tbir lowers it and its own emitted C++ does not compile: {e}",
                sub.trim()
            )),
            (TbVerdict::Lowers, V1Behaviour::EmitsUncompilable(e)) => failures.push(format!(
                "`{}`: tbir lowers it, but v1 — the documented escape hatch — cannot build it: {e}",
                sub.trim()
            )),
            // An OVER-refusal: v1 handles the program and TB-IR does
            // not. Not falsifiable per-landing (the label belongs to
            // the arm, whose worst landing may be genuinely bad), so
            // it is reported rather than asserted — and the report is
            // the real-gap inventory for this arm.
            (TbVerdict::NotImplemented(..), V1Behaviour::Compiles) => {
                gaps.push(sub.trim().to_string())
            }
            _ => {}
        }
    }
    if !gaps.is_empty() {
        table.push_str(&format!(
            "  -- v1 handles these {} and TB-IR does not (real gaps, not failures):\n",
            gaps.len()
        ));
        for g in &gaps {
            table.push_str(&format!("       {g}\n"));
        }
    }
    assert!(failures.is_empty(), "{table}\n{}", failures.join("\n"));
    table
}

/// The transactor state-field arms, over the type space their guards
/// actually admit.
///
/// Five review rounds went into these by hand, and each round missed a
/// landing the next one found: `event<Color>`, then a bare `event`,
/// then a directional module handle. Every one of them is a row here.
#[test]
fn transactor_state_field_verdicts_match_v1() {
    let template = r#"
struct Beat
    p : uint<8>
end struct Beat

enum Color { RED, GREEN }

bus TlmMemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmMemBus

transactor TlmMemTarget bound to TlmMemBus
    n : uint<32> default 0
@@FIELD@@
    thread bus.read(addr: uint<8>)
        n = n + 1
        return 1
    end thread
end transactor TlmMemTarget

testbench Tb
    dut : TlmReadInitiator
end testbench Tb

impl T for Tb
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        dut.rst = 1
        wait 2 cycles
    end run
end impl T
"#;
    // Enumerated from the GRAMMAR the guards test, not from memory:
    // every event payload shape, both `default` positions, each
    // direction, and the non-scalar type families.
    let table = check_space(
        "state-fields",
        template,
        "@@FIELD@@",
        &[
            // event payloads — the allow-list's whole job
            "    ev : out event<uint<8>>",
            "    ev : out event<uint<128>>",
            "    ev : out event<uint<1024>>",
            "    ev : out event<sint<8>>",
            "    ev : out event<bits<8>>",
            "    ev : out event<bool>",
            "    ev : out event",
            "    ev : out event<Beat>",
            "    ev : out event<Color>",
            "    ev : out event<string>",
            "    ev : out event<queue<uint<8>>>",
            "    ev : out event<uint<8>, uint<16>>",
            "    ev : in event<uint<8>>",
            "    ev : inout event<uint<8>>",
            "    ev : out event<uint<8>> default 0",
            // directional non-event
            "    p : in uint<8>",
            "    p : out uint<8>",
            "    p : in Vec<uint<8>, 4>",
            "    p : in Beat",
            // non-scalar state types
            "    v : Vec<uint<8>, 4>",
            "    s : stream<uint<8>>",
            "    m : Color",
            "    w : uint<128>",
            "    w : sint<128>",
            // defaults
            "    q : queue<uint<8>> default 0",
            "    b : Beat default 0",
            "    b : Beat<uint<8>>",
        ],
    );
    eprintln!("{table}");
}

/// `tb_scalar_field_ir_type` is one function deciding for FOUR call
/// sites: a testbench field, a test-scope `let`, and a transactor state
/// field (twice). Widening its `w > 64` gate to close one gap changes
/// the answer at all of them, so each gets its own template here before
/// anything is widened.
#[test]
fn the_shared_scalar_width_gate_across_its_call_sites() {
    let widths = &[
        "    w : uint<64>",
        "    w : uint<65>",
        "    w : uint<128>",
        "    w : sint<128>",
        "    w : uint<1024>",
    ];

    // Call site 1: a testbench field.
    let tb_field = r#"
testbench Tb
    dut : Top
@@FIELD@@
end testbench Tb

impl T for Tb
    run
        dut.rst = 1
        wait 2 cycles
    end run
end impl T
"#;
    eprintln!("{}", check_space("tb-field", tb_field, "@@FIELD@@", widths));

    // Call site 2: a test-scope `let`. Spelled as a statement, so the
    // hole sits in the run body rather than the field list.
    let scope_let = r#"
testbench Tb2
    dut : Top
end testbench Tb2

impl T2 for Tb2
    run
@@FIELD@@
        dut.rst = 1
        wait 2 cycles
    end run
end impl T2
"#;
    eprintln!(
        "{}",
        check_space(
            "scope-let",
            scope_let,
            "@@FIELD@@",
            &[
                "        let w : uint<64> = 1",
                "        let w : uint<65> = 1",
                "        let w : uint<128> = 1",
                "        let w : sint<128> = 1",
            ],
        )
    );
}
