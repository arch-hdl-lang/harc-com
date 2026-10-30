//! Native regressions for regblock / addrmap value typing:
//!
//! * `let x = regs.REG.FIELD` (and `chip.inst.REG.FIELD`) binds a local of
//!   the field's type. Before the fix the local was untyped (`uint64_t`),
//!   so a `sint<4>` field holding -2 compared `>= 0` and `-2 >> 1` printed
//!   9223372036854775807.
//! * A `let` annotation on a RAL read is held to the typed-let rule; it was
//!   dropped, so `let u : uint<2> = regs.R.F` held -2.
//! * `uint<W>` / `uint<0x4>` / `uint<2 + 2>` field widths are their real
//!   width; they silently became one bit.
//! * A frontdoor register write is masked to the register width like
//!   `record_write`; a 12-bit register held 0xFFFF.
//! * Signed field resets accept sized literals (`4'h3`, `-4'h2`) and the
//!   `sint<64>` minimum literal; `reset true` on a `sint<1>` is range
//!   checked; a `const` initialized from a sized literal folds.
//! * A `via` helper whose parameters cannot hold the register offset or
//!   value is a precise error, not a TB-IR verifier internal error.

use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE: &str = "tests/fixtures/regblock_typed_access_test.harc";
const ADDRMAP_FIXTURE: &str = "tests/fixtures/addrmap_typed_read_let_test.harc";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harc_ral_typed_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create output directory");
    dir
}

fn sim(fixture: &Path, outdir: &Path, emit_only: bool) -> std::process::Output {
    let root = repo_root();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_harc"));
    cmd.arg("sim");
    if emit_only {
        cmd.arg("--emit-only");
    }
    cmd.arg("--sv")
        .arg(root.join("tests/dut/AxiLiteRegs.sv"))
        .args(["--top", "AxiLiteRegs", "--jobs", "2", "--outdir"])
        .arg(outdir)
        .arg(fixture)
        .output()
        .expect("run harc sim")
}

fn assert_sim_passes(fixture: &str, marker: &str, tag: &str) {
    let outdir = scratch(tag);
    let output = sim(&repo_root().join(fixture), &outdir, false);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED") && stdout.contains(marker),
        "{fixture} failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(outdir);
}

/// Each `(name, edits, expected)` variant applies its `(from, to)` edits
/// to `fixture` and must be rejected at compile time with `expected` in
/// the diagnostic (whitespace-normalized).
fn assert_variants_rejected(fixture: &str, tag: &str, variants: &[(&str, &[(&str, &str)], &str)]) {
    let root = repo_root();
    let base = std::fs::read_to_string(root.join(fixture)).expect("read fixture");
    let dir = scratch(tag);
    for (name, edits, want) in variants {
        let mut text = base.clone();
        for (from, to) in *edits {
            assert!(text.contains(from), "{name}: fixture shape changed: {from}");
            text = text.replacen(from, to, 1);
        }
        let source = dir.join(format!("{name}.harc"));
        std::fs::write(&source, text).expect("write variant");
        let output = sim(&source, &dir.join(format!("{name}-out")), true);
        let stderr = String::from_utf8_lossy(&output.stderr).replace(['│', '\n'], " ");
        let stderr = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            !output.status.success() && stderr.contains(want),
            "{name}: expected `{want}`:\n{stderr}"
        );
        assert!(
            !stderr.contains("internal error"),
            "{name}: diagnostic is an internal error:\n{stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_regblock_typed_access() {
    assert_sim_passes(FIXTURE, "TYPED_ACCESS_OK", "regblock_sim");
}

#[test]
fn native_addrmap_typed_read_let() {
    assert_sim_passes(ADDRMAP_FIXTURE, "ADDRMAP_TYPED_LET_OK", "addrmap_sim");
}

#[test]
fn regblock_read_let_annotations_follow_the_typed_let_rule() {
    assert_variants_rejected(
        FIXTURE,
        "let_rule",
        &[
            (
                "signedness",
                &[("let g : uint<8> = regs.R.G", "let g : uint<8> = regs.R.F")],
                "`let g` is declared uint<8>, but register field `regs.R.F` is a sint<4> value. \
                 Signedness must match",
            ),
            (
                "field_narrows",
                &[("let g : uint<8> = regs.R.G", "let g : uint<2> = regs.R.G")],
                "`let g` is declared uint<2>, but register field `regs.R.G` is a uint<4> value, \
                 so the initialization narrows",
            ),
            (
                "wo_register_narrows",
                &[("let n = regs.N", "let n : uint<8> = regs.N")],
                "`let n` is declared uint<8>, but register `regs.N` is a uint<12> value",
            ),
            (
                "bus_register_narrows",
                &[("let s = regs.SA.S", "let s : uint<8> = regs.SA")],
                "`let s` is declared uint<8>, but register `regs.SA` is a uint<32> value",
            ),
            (
                "record_read_narrows",
                &[(
                    "let e = regs.R.F",
                    "let e : uint<8> = regs.record_read(0x00)",
                )],
                "`let e` is declared uint<8>, but `regs.record_read(...)` is a uint<32> value",
            ),
        ],
    );
}

#[test]
fn addrmap_read_let_annotation_follows_the_typed_let_rule() {
    assert_variants_rejected(
        ADDRMAP_FIXTURE,
        "addrmap_let_rule",
        &[(
            "narrows",
            &[(
                "let wide : sint<16> = chip.mm2s.SA.S",
                "let wide : sint<4> = chip.mm2s.SA.S",
            )],
            "`let wide` is declared sint<4>, but register field `chip.mm2s.SA.S` is a sint<8> \
             value, so the initialization narrows",
        )],
    );
}

#[test]
fn regblock_declaration_defects_are_precise_errors() {
    assert_variants_rejected(
        FIXTURE,
        "decl",
        &[
            (
                "unknown_width_const",
                &[("field CW : uint<W>", "field CW : uint<NOPE>")],
                "references `NOPE`, which is not a `const`",
            ),
            (
                "sized_reset_pattern",
                &[("reset 4'h3", "reset 4'hE")],
                "field `KP` is 14, which is outside the range of its signed 4-bit type [-8, 7]; \
                 to store the bit pattern 0xE, write -2",
            ),
            (
                "sint1_reset_true",
                &[(
                    "field KN : sint<4> @ 4 reset -4'h2",
                    "field KN : sint<1> @ 4 reset true",
                )],
                "field `KN` is 1, which is outside the range of its signed 1-bit type [-1, 0]",
            ),
            (
                "helper_offset_too_wide",
                &[("register SA @ 0x18", "register SA @ 0x118")],
                "register `SA` cannot use regblock `via` helper `AxilHelper` method `write`: \
                 it declares parameter 1 `addr` as uint<8>, but the frontdoor passes its offset \
                 0x118 as a uint<9>",
            ),
            (
                "helper_data_too_narrow",
                &[
                    ("data: uint<64>", "data: uint<32>"),
                    ("data.trunc<32>()", "data"),
                    (
                        "log(info, \"TYPED_ACCESS_OK\")",
                        "let zv : uint<64> = 1\n        regs.Z = zv",
                    ),
                ],
                "register `Z` cannot use regblock `via` helper `AxilHelper` method `write`: \
                 it declares parameter 2 `data` as uint<32>, but the frontdoor passes its value as \
                 a uint<64>",
            ),
            (
                // A register narrower than 64 bits, written with a
                // non-literal value, behind a data parameter narrower than
                // the register: the masked value used to carry no known type,
                // so the check was skipped and the bus received 31 of the 32
                // bits the mirror held.
                "helper_data_narrower_than_masked_register",
                &[
                    ("data: uint<64>", "data: uint<31>"),
                    ("data.trunc<32>()", "data.zext<32>()"),
                ],
                "register `SA` cannot use regblock `via` helper `AxilHelper` method `write`: \
                 it declares parameter 2 `data` as uint<31>, but the frontdoor passes its value as \
                 a uint<32>",
            ),
            (
                "helper_data_signed",
                &[
                    ("data: uint<64>", "data: sint<64>"),
                    ("data.trunc<32>()", "(data as uint<64>).trunc<32>()"),
                ],
                "declares parameter 2 `data` as sint<64>, but the frontdoor passes its value as \
                 a uint<32>",
            ),
        ],
    );
}

const DIRECT_BUS_FIXTURE: &str = "tests/fixtures/regblock_direct_bus_test.harc";

/// A regblock routed directly through a bus binding's `tlm_method`s gets the
/// same declaration-level parameter check as a transactor helper; a mismatch
/// used to reach the TB-IR verifier as an internal error.
#[test]
fn direct_bus_tlm_method_parameters_are_checked() {
    let root = repo_root();
    let base = std::fs::read_to_string(root.join(DIRECT_BUS_FIXTURE)).expect("read fixture");
    let dir = scratch("direct_bus_params");
    for (name, edits, want) in [
        (
            "addr_too_narrow",
            &[(
                "tlm_method write(addr: uint<8>, data: uint<32>)",
                "tlm_method write(addr: uint<3>, data: uint<32>)",
            )][..],
            "register `R8` cannot use regblock `via` bus `RegBus` tlm_method `write`: it \
             declares parameter 1 `addr` as uint<3>, but the frontdoor passes its offset 0x8 \
             as a uint<4>",
        ),
        (
            "data_too_narrow_literal",
            &[(
                "tlm_method write(addr: uint<8>, data: uint<32>)",
                "tlm_method write(addr: uint<8>, data: uint<31>)",
            )][..],
            "register `R8` cannot use regblock `via` bus `RegBus` tlm_method `write`: it \
             declares parameter 2 `data` as uint<31>, but the frontdoor passes its value as \
             a uint<32>",
        ),
        (
            "data_too_narrow_variable",
            &[
                (
                    "tlm_method write(addr: uint<8>, data: uint<32>)",
                    "tlm_method write(addr: uint<8>, data: uint<31>)",
                ),
                (
                    "        regs.R8 = 3405691582\n",
                    "        let v : uint<32> = 3405691582\n        regs.R8 = v\n",
                ),
            ][..],
            "declares parameter 2 `data` as uint<31>, but the frontdoor passes its value as \
             a uint<32>",
        ),
    ] {
        let mut text = base.clone();
        for (from, to) in edits {
            assert!(text.contains(from), "{name}: fixture shape changed: {from}");
            text = text.replacen(from, to, 1);
        }
        let source = dir.join(format!("{name}.harc"));
        std::fs::write(&source, text).expect("write variant");
        let output = Command::new(env!("CARGO_BIN_EXE_harc"))
            .arg("dump-ir")
            .arg(&source)
            .env("NO_COLOR", "1")
            .output()
            .expect("run harc dump-ir");
        let stderr = String::from_utf8_lossy(&output.stderr).replace(['│', '\n'], " ");
        let stderr = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            !output.status.success() && stderr.contains(want),
            "{name}: expected `{want}`:\n{stderr}"
        );
        assert!(
            !stderr.contains("internal error"),
            "{name}: diagnostic is an internal error:\n{stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A 12-bit register behind a 32-bit direct-bus `read`: the frontdoor write
/// is masked, and both the `let` read and an inline read are 12-bit values.
#[test]
fn native_direct_bus_narrow_register() {
    let root = repo_root();
    let base = std::fs::read_to_string(root.join(DIRECT_BUS_FIXTURE)).expect("read fixture");
    let mut text = base.replacen(
        "register R8 @ 8 reset 0 access rw",
        "register R8 @ 8 width 12 reset 0 access rw",
        1,
    );
    let from = "        regs.R8 = 3405691582\n        assert regs.R8 == 3405691582\n";
    assert!(text.contains(from), "fixture shape changed");
    text = text.replacen(
        from,
        "        regs.R8 = 0xFFFF\n        let x = regs.R8\n        \
         assert x == 0xFFF else fail(\"let read ${x}\")\n        \
         assert regs.R8 == 0xFFF\n",
        1,
    );
    let dir = scratch("direct_bus_w12");
    let source = dir.join("w12.harc");
    std::fs::write(&source, text).expect("write variant");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(root.join("tests/dut/TlmMemory.sv"))
        .args(["--top", "TlmMemory", "--jobs", "2", "--outdir"])
        .arg(dir.join("out"))
        .arg(&source)
        .output()
        .expect("run harc sim");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED"),
        "narrow direct-bus register failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(dir);
}
