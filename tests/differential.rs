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
    /// v1 refused to emit at all, with the reason it gave. A bare
    /// "refuses" reading sent two probe templates down the wrong path
    /// before the message was carried along with it.
    Refuses(String),
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
        Err(e) => return V1Behaviour::Refuses(e.to_string()),
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
    check_space_with_control(label, template, hole, "", subs)
}

/// `check_space_with_control` plus an assertion that EVERY row lowers.
///
/// The three falsifiable directions are all about verdicts that
/// over-promise. None of them can fail on a verdict that over-REFUSES:
/// re-capping a width gate turns every row into
/// `(Unsupported, v1 compiles)`, which is exactly what `Unsupported`
/// means and is only reported. A space covering a capability that is
/// supposed to work needs to say so, or reverting the capability
/// leaves it green.
fn check_space_all_lower(
    label: &str,
    template: &str,
    hole: &str,
    control: &str,
    subs: &[&str],
) -> String {
    let table = check_space_with_control(label, template, hole, control, subs);
    if table.is_empty() {
        return table; // no compiler on PATH
    }
    let refused: Vec<&str> = table
        .lines()
        .filter(|l| l.starts_with("  ") && !l.contains("tbir=LOWERS "))
        .filter(|l| l.contains("tbir="))
        .collect();
    assert!(
        refused.is_empty(),
        "{table}\n{label}: these rows must lower and do not:\n{}",
        refused.join("\n")
    );
    table
}

/// `check_space` for a hole that cannot simply be deleted — a type
/// annotation, say, where an empty substitution is a parse error rather
/// than a smaller program. `control` is the neutral row: a
/// substitution already known to work, so the skeleton is still proved
/// before any row is believed.
fn check_space_with_control(
    label: &str,
    template: &str,
    hole: &str,
    control: &str,
    subs: &[&str],
) -> String {
    let Some(cc) = cxx() else {
        eprintln!("no C++ compiler on PATH — skipping {label}");
        return String::new();
    };
    let dir = std::env::temp_dir().join(format!("harc-diff-{label}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");

    // The CONTROL: the template with the hole emptied. A broken
    // skeleton has produced false "v1 refuses" readings more than once,
    // and every row is meaningless if the control does not pass.
    let control = template.replace(hole, control);
    assert!(
        matches!(tb_verdict(cc, &control, &dir, "control"), TbVerdict::Lowers),
        "{label}: the control must lower — the template is broken, not the compiler"
    );
    match v1_behaviour(cc, &control, &dir, "control") {
        V1Behaviour::Compiles => {}
        other => panic!("{label}: the control must compile under v1, got {other:?}"),
    }

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
                V1Behaviour::Refuses(m) => format!("refuses: {m}"),
                V1Behaviour::EmitsUncompilable(_) => "emits, g++ refuses".to_string(),
            }
        ));

        // The three falsifiable directions.
        match (&tb, &v1) {
            (TbVerdict::Unsupported(c), V1Behaviour::EmitsUncompilable(e)) => failures.push(
                format!("`{}`: tbir promises `--codegen v1` for `{c}`, but v1's output does not compile: {e}", sub.trim()),
            ),
            (TbVerdict::Unsupported(c), V1Behaviour::Refuses(m)) => failures.push(format!(
                "`{}`: tbir promises `--codegen v1` for `{c}`, but v1 refuses the program: {m}",
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
            //
            // `Unsupported` belongs in that inventory too, and used to
            // be missing from it. Pairing it with a compiling v1 is
            // exactly what the label PROMISES, so no assertion fires —
            // but the promise being kept is precisely what makes it a
            // gap: v1 builds the program and TB-IR refuses it.
            (TbVerdict::NotImplemented(..) | TbVerdict::Unsupported(_), V1Behaviour::Compiles) => {
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

/// `tb_scalar_field_ir_type` is one function deciding for FIVE call
/// sites, and its `w > 64` gate is what refuses a wide declared field.
/// Widening it to close one gap changes the answer at all five, so
/// each gets its own template here before anything is widened:
///
///   * `mod.rs` testbench-field DECLARATION — the guard that refuses;
///   * `mod.rs` testbench-field DEFAULT — no guard at all, an `else
///     if let Some(..)`, so a type the gate rejects is silently
///     DROPPED rather than diagnosed. The two must move together or a
///     wide field becomes a member that does not exist;
///   * `mod.rs` promoted test-scope `let` — falls back to
///     `IrType::UInt(None)` instead of refusing, so a wide promoted
///     let is a 64-bit member today;
///   * `transactors.rs` component-hosted target-state FILTER — decides
///     whether the responder view lowers the field at all;
///   * `transactors.rs` transactor state field.
///
/// The rows use each field rather than only declaring it: a
/// declaration that widens while its read/write path does not is a
/// worse outcome than the refusal it replaced.
#[test]
fn the_shared_scalar_width_gate_across_its_call_sites() {
    // One space, five landings. `uint<65>` and `uint<128>` share the
    // `_harc_u128` storage class, `uint<1024>` crosses into
    // `HarcWide<N>`, and `sint<128>` is the signed half — the emitter
    // seam picks a different C++ type for each, so a fix that only
    // reaches one of them shows up here.
    let widths = &[
        "uint<64>",
        "uint<65>",
        "uint<128>",
        "sint<128>",
        "uint<1024>",
    ];

    // Call site 1 + 2: a testbench field. Declaration and default are
    // separate call sites in the same lowering pass.
    let tb_field = r#"
testbench Tb
    dut : Top
    w : @@TY@@ default 1
end testbench Tb

impl T for Tb
    run
        w = 2
        dut.rst = 1
        wait 2 cycles
    end run
    check
        assert w == 2
            else fail("w=${w}")
    end check
end impl T
"#;
    eprintln!(
        "{}",
        check_space_all_lower("tb-field", tb_field, "@@TY@@", "uint<32>", widths)
    );

    // Call site 3: a test-scope `let` READ IN THE CHECK PHASE, which
    // promotes it to a `_tb` host field. The unpromoted form already
    // lowers wide (it is a plain local, not a field), so probing that
    // one says nothing about this gate.
    let promoted_let = r#"
testbench Tb2
    dut : Top
end testbench Tb2

impl T2 for Tb2
    let w : @@TY@@ = 1
    run
        dut.rst = 1
        wait 2 cycles
    end run
    check
        assert w == 1
            else fail("w=${w}")
    end check
end impl T2
"#;
    eprintln!(
        "{}",
        check_space_all_lower("promoted-let", promoted_let, "@@TY@@", "uint<32>", widths)
    );

    // Call sites 4 + 5: a transactor state field, reached through the
    // responder view.
    let state_field = r#"
bus TlmMemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmMemBus

transactor TlmMemTarget bound to TlmMemBus
    w : @@TY@@ default 1
    thread bus.read(addr: uint<8>)
        w = w + 1
        return 1
    end thread
end transactor TlmMemTarget

testbench Tb3
    dut : TlmReadInitiator
end testbench Tb3

impl T3 for Tb3
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        dut.rst = 1
        wait 2 cycles
    end run
end impl T3
"#;
    eprintln!(
        "{}",
        check_space_all_lower("state-field", state_field, "@@TY@@", "uint<32>", widths)
    );

    // The same scalar-field shape reached through the scoreboard and
    // component field kinds, whose emitters carry their own copy of
    // the `(bool | int64_t | uint64_t)` type choice.
    let sb_field = r#"
scoreboard Sb
    w : @@TY@@ default 1
end scoreboard Sb

testbench Tb4
    dut : Top
    sb : Sb
end testbench Tb4

impl T4 for Tb4
    run
        sb.w = 2
        dut.rst = 1
        wait 2 cycles
    end run
    check
        assert sb.w == 2
            else fail("w=${sb.w}")
    end check
end impl T4
"#;
    eprintln!(
        "{}",
        check_space_all_lower("scoreboard-field", sb_field, "@@TY@@", "uint<32>", widths)
    );

    let comp_field = r#"
transactor Src
    w : @@TY@@ default 1
    hookable bump(v: uint<8>)
        w = w + 1
    end bump
end transactor Src

testbench Tb5
    dut : Top
    src : Src passive
end testbench Tb5

impl T5 for Tb5
    run
        src.bump(1)
        dut.rst = 1
        wait 2 cycles
    end run
    check
        assert src.w == 2
            else fail("w=${src.w}")
    end check
end impl T5
"#;
    eprintln!(
        "{}",
        check_space_all_lower("component-field", comp_field, "@@TY@@", "uint<32>", widths)
    );
}

/// Declaring a wide scalar and USING it are different questions, and
/// the width test above only assigns and compares. `HarcWide<N>` has
/// `operator+` for `HarcWide<N> + HarcWide<N>` and implicit conversions
/// to both `uint64_t` and `_harc_u128`, so `w + 1` is ambiguous rather
/// than wrong — g++ reports "ambiguous overload for `operator+`". A
/// declaration gate that widens without this row shipping green would
/// trade a refusal for a build failure.
#[test]
fn wide_scalar_arithmetic_at_a_local_and_at_a_field() {
    let widths = &["uint<64>", "uint<128>", "uint<1024>"];

    let local = r#"
testbench TbW
    dut : Top
end testbench TbW

impl TW for TbW
    run
        let w : @@TY@@ = 1
        w = w + 1
        dut.rst = 1
        wait 2 cycles
    end run
end impl TW
"#;
    eprintln!(
        "{}",
        check_space_all_lower("wide-arith-local", local, "@@TY@@", "uint<32>", widths)
    );

    let field = r#"
testbench TbW2
    dut : Top
    w : @@TY@@ default 1
end testbench TbW2

impl TW2 for TbW2
    run
        w = w + 1
        dut.rst = 1
        wait 2 cycles
    end run
end impl TW2
"#;
    eprintln!(
        "{}",
        check_space_all_lower("wide-arith-field", field, "@@TY@@", "uint<32>", widths)
    );

    // The OPERATOR space, at the width where a scalar stops being a
    // builtin integer type. `_harc_u128` takes part in ordinary C++
    // arithmetic and none of this arises; `HarcWide<N>` is a struct,
    // and every operator it does not define is a program that lowers
    // and does not build.
    let ops = r#"
testbench TbW3
    dut : Top
    w : uint<1024> default 7
    r : uint<1024> default 0
end testbench TbW3

impl TW3 for TbW3
    run
@@OP@@
        dut.rst = 1
        wait 2 cycles
    end run
end impl TW3
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "wide-ops-unsigned",
            ops,
            "@@OP@@",
            "        r = w",
            &[
                "        r = w + 1",
                "        r = w - 1",
                "        r = w * 3",
                "        r = w / 2",
                "        r = w % 2",
                "        r = w & 1",
                "        r = w | 2",
                "        r = w ^ 1",
                "        r = w << 1",
                "        r = w >> 1",
                "        assert w == 7 else fail(\"eq\")",
                "        assert w != 8 else fail(\"ne\")",
                "        assert w < 8 else fail(\"lt\")",
                "        assert w > 1 else fail(\"gt\")",
                "        assert w <= 7 else fail(\"le\")",
                "        assert w >= 7 else fail(\"ge\")",
            ],
        )
    );

    // Two wide operands of DIFFERENT widths: no `N` deduces for the
    // homogeneous operator, so this is its own ambiguity rather than a
    // special case of the integer one.
    let mixed = r#"
testbench TbW4
    dut : Top
    a : uint<160> default 1
    b : uint<256> default 1
end testbench TbW4

impl TW4 for TbW4
    run
@@OP@@
        dut.rst = 1
        wait 2 cycles
    end run
end impl TW4
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "wide-ops-mixed-width",
            mixed,
            "@@OP@@",
            "        b = b",
            &[
                // The WIDE destination direction, and the narrow one.
                // An earlier version defined mixed-width operators and
                // measured only the first, which was the landing they
                // happened to get right.
                "        b = b + a",
                "        b = b - a",
                "        b = b * a",
                "        b = b & a",
                "        b = b | a",
                "        b = b ^ a",
                "        b = b / a",
                "        a = a + b",
                "        a = a - b",
                "        a = a & b",
                "        assert b == a else fail(\"eq\")",
                "        assert b < a else fail(\"lt\")",
            ],
        )
    );
}

/// Every HOST-STATE read a wide declared field made reachable, against
/// the operators `HarcWide` does not define for a mixed pair.
///
/// The first version of the guard resolved two of these shapes and
/// answered "not wide" for the rest, so six programs lowered into C++
/// nobody could build. `expr_type` returns `None` for all of them, and
/// "it falls back to `expr_type`" was doing no work at all here.
#[test]
fn the_wide_operator_refusal_covers_every_host_state_read() {
    let ops = &["        r = w / 2", "        r = w % 2"];
    let asserts = &["        assert w < 8\n            else fail(\"lt\")"];

    // A scoreboard scalar field, read as `sb.w`.
    let sb = r#"
scoreboard Sb
    w : uint<1024> default 7
    r : uint<1024> default 0
end scoreboard Sb

testbench TbH
    dut : Top
    sb : Sb
end testbench TbH

impl TH for TbH
@@HOLE@@
end impl TH
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "host-scoreboard",
            sb,
            "@@HOLE@@",
            "    run\n        sb.r = sb.w\n        wait 1 cycle\n    end run",
            &[
                "    run\n        sb.r = sb.w / 2\n        wait 1 cycle\n    end run",
                "    run\n        wait 1 cycle\n    end run\n    check\n        assert sb.w < 8\n            else fail(\"lt\")\n    end check",
            ],
        )
    );

    // The same scoreboard held inside an env — a different base, a
    // different emission path, the same field kind.
    let env = r#"
scoreboard SbE
    w : uint<1024> default 7
end scoreboard SbE

env EnvA
    sb : SbE
end env EnvA

testbench TbH2
    dut : Top
    top : EnvA
end testbench TbH2

impl TH2 for TbH2
    run
        wait 1 cycle
    end run
    check
@@HOLE@@
    end check
end impl TH2
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "host-scoreboard-in-env",
            env,
            "@@HOLE@@",
            "        assert top.sb.w == 7\n            else fail(\"eq\")",
            &["        assert top.sb.w < 8\n            else fail(\"lt\")"],
        )
    );

    // A bound-to transactor's state, read from INSIDE the responder
    // body (where the instance name is the empty placeholder) and from
    // the test scope (where it is a real testbench field), plus a leaf
    // of a whole-record state field.
    let state = r#"
struct Last
    w : uint<1024>
end struct Last

bus TlmMemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmMemBus

transactor TlmMemTarget bound to TlmMemBus
    w : uint<1024> default 7
    last : Last
    thread bus.read(addr: uint<8>)
@@HOLE@@
        return 1
    end thread
end transactor TlmMemTarget

testbench TbH3
    dut : TlmReadInitiator
end testbench TbH3

impl TH3 for TbH3
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        dut.rst = 1
        wait 2 cycles
    end run
end impl TH3
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "host-transactor-state",
            state,
            "@@HOLE@@",
            "        w = w + 1",
            &[
                "        w = w / 2",
                "        if w < 8\n            w = w + 1\n        end if",
                "        if last.w < 8\n            w = w + 1\n        end if",
            ],
        )
    );

    let from_test = r#"
bus TlmMemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmMemBus

transactor TlmMemTarget bound to TlmMemBus
    w : uint<1024> default 7
    thread bus.read(addr: uint<8>)
        w = w + 1
        return 1
    end thread
end transactor TlmMemTarget

testbench TbH4
    dut : TlmReadInitiator
end testbench TbH4

impl TH4 for TbH4
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        dut.rst = 1
        wait 2 cycles
    end run
    check
@@HOLE@@
    end check
end impl TH4
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "host-transactor-state-from-test",
            from_test,
            "@@HOLE@@",
            "        assert target.w == 7\n            else fail(\"eq\")",
            &["        assert target.w < 8\n            else fail(\"lt\")"],
        )
    );

    let _ = (ops, asserts);
}

/// Two statement positions consume a value through a SYNTHESIZED
/// comparison or conversion, so the binary-operator guard never sees
/// them. Both were left behind when the field gate widened.
#[test]
fn a_wide_scalar_in_a_loop_bound_or_a_cycle_count_is_refused() {
    let tmpl = r#"
testbench TbL
    dut : Top
    w : uint<1024> default 3
    r : uint<1024> default 0
end testbench TbL

impl TL for TbL
    run
@@HOLE@@
        wait 1 cycle
    end run
end impl TL
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "wide-loop-and-wait",
            tmpl,
            "@@HOLE@@",
            "        r = w + 1",
            &[
                // Each of these consumes the value through a
                // SYNTHESIZED comparison or conversion, so the
                // binary-operator guard never sees it. tbir LOWERED
                // the first two, silently keeping the low 64 bits of a
                // 1024-bit value, while v1 could not build either.
                // A first pass guarded `for` and `wait` and called
                // that "two statement positions"; `repeat` builds the
                // SAME header as `for` through the same helper, whose
                // doc line says so.
                "        for i in 0 .. w\n            r = r + 1\n        end for",
                "        wait w cycles",
                "        repeat w\n            r = r + 1\n        end repeat",
                "        wait until dut.rst == 1 timeout w cycles",
            ],
        )
    );
}

/// Shapes the wide-operator guard must NOT fire on.
///
/// A guard that refuses a program v1 builds is as wrong as one that
/// lets an unbuildable program through, and harder to notice: the
/// harness's falsifiable directions do not catch an over-refusal.
/// These rows must all lower.
#[test]
fn the_wide_operator_refusal_does_not_over_fire() {
    let tmpl = r#"
testbench TbN
    dut : Top
    a : uint<160> default 1
    b : uint<256> default 1
    n : uint<64> default 1
    s : uint<160> default 0
end testbench TbN

impl TN for TbN
    run
@@HOLE@@
        dut.rst = 1
        wait 2 cycles
    end run
end impl TN
"#;
    eprintln!(
        "{}",
        check_space_all_lower(
            "wide-not-over-refused",
            tmpl,
            "@@HOLE@@",
            "        s = a",
            &[
                // `and`/`or` produce a BOOL whatever their operands
                // are. A version of the guard recursed through
                // `Expr::Binary` itself and dropped that rule, so this
                // was refused as "`&&` between scalars of 5 and 8
                // words" — under a label claiming v1 could not build
                // it either.
                "        assert (a == 1) and (b == 1)\n            else fail(\"both\")",
                "        assert (a == 1) or (b == 1)\n            else fail(\"either\")",
                // Same-width wide operands are exactly what the
                // homogeneous operators take.
                "        s = a + a",
                "        s = a / a",
                "        assert a < a + 1\n            else fail(\"lt\")",
                // A shift by an ordinary integer, and by a narrow
                // field: the COUNT is what must not be wide.
                "        s = a << 1",
                "        s = a >> n",
                // Cross-width EQUALITY has its own `<A, B>` form.
                "        assert a == b\n            else fail(\"eq\")",
                "        assert a != b\n            else fail(\"ne\")",
                // Narrow operands are untouched.
                "        assert n < 8\n            else fail(\"n\")",
                // The loop guards must not fire on a narrow bound.
                "        repeat n\n            s = s + 1\n        end repeat",
                "        for i in 0 .. n\n            s = s + 1\n        end for",
            ],
        )
    );
}

/// A wide value narrowed into a narrower slot, and a wide shift COUNT.
///
/// The narrowing check read `expr_type`, which answers `None` for every
/// host-state read, so an assignment between two declared FIELDS
/// skipped it. Harmless while a field could not exceed 64 bits; once
/// they could, `a : uint<160> = b : uint<256>` lowered into
/// `HarcWide<5> = HarcWide<8>`, which has no `operator=`. The `let`
/// spelling was worse — v1 gave a clean narrowing diagnostic there.
#[test]
fn a_wide_value_narrowed_or_used_as_a_shift_count_is_refused() {
    let tmpl = r#"
testbench TbX
    dut : Top
    a : uint<160> default 1
    b : uint<256> default 1
    s : uint<160> default 0
end testbench TbX

impl TX for TbX
    run
@@HOLE@@
        dut.rst = 1
        wait 2 cycles
    end run
end impl TX
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "wide-narrowing-and-shift-count",
            tmpl,
            "@@HOLE@@",
            "        s = a",
            &[
                "        a = b",
                "        let c : uint<160> = b\n        s = c",
                // `HarcWide`'s shifts take an integral count, so a
                // `HarcWide` count is ambiguous at ANY width — two
                // equal ones included, which the mixed-width check
                // cannot see.
                "        s = a << a",
                "        s = a >> a",
            ],
        )
    );
}

/// The component-record LEAF, which `ComponentField` spells as a DOTTED
/// member name (`cur.w`) — so a lookup by the whole name never matched
/// and every wide leaf answered "not wide".
#[test]
fn a_wide_leaf_of_a_component_record_field_is_seen() {
    let tmpl = r#"
struct Payload
    w : uint<1024>
end struct Payload

transactor Src
    cur : Payload
    r : uint<1024> default 0

    hookable bump(x: uint<8>)
@@HOLE@@
    end bump
end transactor Src

testbench TbR
    dut : Top
    src : Src passive
end testbench TbR

impl TR for TbR
    run
        src.bump(1)
        dut.rst = 1
        wait 2 cycles
    end run
end impl TR
"#;
    eprintln!(
        "{}",
        check_space_with_control(
            "component-record-leaf",
            tmpl,
            "@@HOLE@@",
            "        r = cur.w",
            &[
                "        r = cur.w / 2",
                "        if cur.w < 8\n            r = r + 1\n        end if"
            ],
        )
    );
}

/// PAST the declared-field width cap, at the two sites whose
/// diagnostics name v1 as the way out. `Unsupported` and that
/// parenthetical are both promises about v1; a landing where v1's own
/// output does not build falsifies them.
#[test]
fn a_scalar_field_past_the_cap_is_only_promised_to_v1_where_v1_builds() {
    let over = &["uint<1025>", "uint<2048>", "uint<4096>"];

    let tb_field = r#"
testbench TbP
    dut : Top
    w : @@TY@@ default 1
end testbench TbP

impl TP for TbP
    run
        w = w + 1
        dut.rst = 1
        wait 2 cycles
    end run
end impl TP
"#;
    eprintln!(
        "{}",
        check_space_with_control("over-cap-tb-field", tb_field, "@@TY@@", "uint<1024>", over)
    );

    let state = r#"
bus TlmMemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmMemBus

transactor TlmMemTarget bound to TlmMemBus
    w : @@TY@@ default 1
    thread bus.read(addr: uint<8>)
        w = w + 1
        return 1
    end thread
end transactor TlmMemTarget

testbench TbP2
    dut : TlmReadInitiator
end testbench TbP2

impl TP2 for TbP2
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        dut.rst = 1
        wait 2 cycles
    end run
end impl TP2
"#;
    eprintln!(
        "{}",
        check_space_with_control("over-cap-state-field", state, "@@TY@@", "uint<1024>", over)
    );
}

/// Every field schema in the IR carries its `default` as a `u64`, so a
/// declared default that does not fit one has no representation. It
/// must be REFUSED, at every field site, rather than silently becoming
/// a different number — the field is wide precisely so that the value
/// fits.
///
/// Asserted directly rather than through `check_space`, which cannot
/// see this: a truncating implementation reports `(Lowers, v1
/// compiles)`, which is a perfectly consistent pairing. The defect is
/// in the VALUE, and the only thing that distinguishes it is that the
/// backend must not accept the program at all.
#[test]
fn a_field_default_too_wide_for_its_u64_slot_is_never_truncated() {
    let Some(cc) = cxx() else {
        eprintln!("no C++ compiler on PATH — skipping");
        return;
    };
    let dir = std::env::temp_dir().join("harc-diff-wide-default");
    std::fs::create_dir_all(&dir).expect("scratch dir");

    // `u64::MAX`, the first value above it, 2^65 — and the HEX
    // spelling of that same first value. The decimal rows alone are
    // three spellings of ONE landing: the first fix gated on
    // `all(is_ascii_digit)`, so `0x…` fell into the old `Invalid`
    // branch and kept the wrong grade and the wrong words.
    let rows: &[(&str, bool)] = &[
        ("18446744073709551615", true),
        ("0xFFFF_FFFF_FFFF_FFFF", true),
        ("18446744073709551616", false),
        ("0x1_0000_0000_0000_0000", false),
        ("36893488147419103232", false),
        (
            "0b10000000000000000000000000000000000000000000000000000000000000000",
            false,
        ),
    ];

    // Both sites that fold a declared default into a `u64` slot: a
    // testbench field, and a test-scope `let` promoted into one by a
    // check-phase read. The second answered `Invalid` — "no backend
    // runs this" — for a literal v1 compiles.
    let sites: &[(&str, &str)] = &[
        (
            "tb-field",
            r#"
testbench Tb6
    dut : Top
    w : uint<128> default @@D@@
end testbench Tb6

impl T6 for Tb6
    run
        dut.rst = 1
        wait 2 cycles
    end run
end impl T6
"#,
        ),
        (
            "promoted-let",
            r#"
testbench Tb7
    dut : Top
end testbench Tb7

impl T7 for Tb7
    let w : uint<128> = @@D@@
    run
        dut.rst = 1
        wait 2 cycles
    end run
    check
        assert w != 0
            else fail("w=${w}")
    end check
end impl T7
"#,
        ),
    ];

    for (site, template) in sites {
        for (i, (lit, must_lower)) in rows.iter().enumerate() {
            let src = template.replace("@@D@@", lit);
            let stem = format!("{site}{i}");
            let tb = tb_verdict(cc, &src, &dir, &stem);
            if *must_lower {
                assert!(
                    matches!(tb, TbVerdict::Lowers),
                    "{site}: `default {lit}` fits a u64 and must lower, got {tb:?}"
                );
                continue;
            }
            // v1 is measured, not remembered: it emits
            // `_harc_u128 w = <literal>;`, which g++ accepts with a
            // `-Woverflow` warning and evaluates to 0. That is
            // `SilentlyMisLowers`, and it is why `Invalid` — which
            // claims no backend runs the program — was wrong here.
            assert_eq!(
                v1_behaviour(cc, &src, &dir, &stem),
                V1Behaviour::Compiles,
                "{site}: v1 is expected to compile `default {lit}` (and get it wrong)"
            );
            match tb {
                TbVerdict::NotImplemented(lower::V1Status::SilentlyMisLowers, _) => {}
                other => panic!(
                    "{site}: `default {lit}` does not fit a `u64` slot; it must be refused \
                     as NotImplemented{{SilentlyMisLowers}}, got {other:?}"
                ),
            }
        }
    }
}
