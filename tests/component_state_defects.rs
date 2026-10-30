//! Regressions for component state defects:
//!
//! * a `default` on a field wider than 64 bits is carried at full width:
//!   a negative signed default starts negative (it was zero-extended) and a
//!   value past 63/64 bits is kept (it was refused);
//! * a component-typed parameter accepts a test-scope instance and a
//!   sub-component path (they were "an unresolved name" / "field access on
//!   a non-DUT value");
//! * generic arguments on a non-generic component, and a second type
//!   argument on a scalar integer type, are compile errors (both were
//!   dropped silently);
//! * field read/write through a component held in a local (run-local
//!   instance, sub-component of one, by-value copy, component-typed
//!   parameter) lowers (it was "field access on a non-DUT value" / "a target
//!   that is neither a DUT port nor a local"), and a lowering error inside a
//!   `${...}` capture points at its statement rather than at line 1, column 1.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("harc_component_state_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, source).expect("write source");
    path
}

fn harc(args: &[&str], file: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(args)
        .arg(file)
        .env("NO_COLOR", "1")
        .output()
        .expect("run harc")
}

/// Whitespace-normalized stderr with the miette frame glyphs removed.
fn stderr_text(output: &Output) -> String {
    let raw = String::from_utf8_lossy(&output.stderr).replace(['│', '\n'], " ");
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn assert_rejects(output: &Output, want: &str, case: &str) {
    let text = stderr_text(output);
    assert!(
        !output.status.success() && text.contains(want),
        "{case}: expected `{want}`:\n{text}"
    );
    assert!(
        !text.contains("internal error"),
        "{case}: diagnostic is an internal error:\n{text}"
    );
}

fn sim_fixture(fixture: &str, marker: &str, tag: &str) {
    let root = repo_root();
    let outdir = scratch(tag);
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(root.join("tests/dut/top_counter.sv"))
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .arg(root.join(fixture))
        .output()
        .expect("run harc sim");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED") && stdout.contains(marker),
        "{fixture} failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(outdir);
}

#[test]
fn native_negative_wide_signed_defaults_start_negative() {
    sim_fixture(
        "tests/fixtures/wide_signed_default_test.harc",
        "WIDE_SIGNED_DEFAULT_OK",
        "wide_default",
    );
}

#[test]
fn native_wide_testbench_field_defaults() {
    sim_fixture(
        "tests/fixtures/wide_testbench_default_test.harc",
        "WIDE_TESTBENCH_DEFAULT_OK",
        "wide_tb_default",
    );
}

/// Transactor state fields take the same full-width default: the IR carries
/// every carrier word, sign-extended for a negative value.
#[test]
fn wide_transactor_state_defaults_carry_full_width() {
    let root = repo_root();
    let base = std::fs::read_to_string(root.join("tests/fixtures/dma_engine_tlm_target_test.harc"))
        .expect("read fixture");
    const ANCHOR: &str = "    read_count : uint<32> default 0\n";
    assert!(base.contains(ANCHOR), "fixture shape changed");
    let source = base.replacen(
        ANCHOR,
        "    read_count : uint<32> default 0\n    \
         wide_neg : sint<65> default -1\n    \
         wide_top : sint<65> default 0x8000_0000_0000_0000\n    \
         wide_big : uint<200> default 0x1_0000_0000_0000_0000\n",
        1,
    );
    let dir = scratch("wide_state_default");
    let path = write(&dir, "state.harc", &source);
    let output = harc(&["dump-ir"], &path);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "dump-ir failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for want in [
        "state wide_neg = 0xffffffffffffffffffffffffffffffff",
        "state wide_top = 0x8000000000000000",
        "state wide_big = 0x10000000000000000",
    ] {
        assert!(stdout.contains(want), "missing `{want}`:\n{stdout}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A full-width default is still range-checked against the declared width
/// and signedness.
#[test]
fn wide_default_out_of_range_is_rejected() {
    let dir = scratch("wide_default_range");
    for (name, field, want) in [
        (
            "signed_2_pow_64",
            "a : sint<65> default 0x1_0000_0000_0000_0000",
            "needs 65 bits of magnitude, which does not fit its declared sint<65>",
        ),
        (
            "signed_below_min",
            "a : sint<65> default -0x1_0000_0000_0000_0001",
            "needs 65 bits of magnitude, which does not fit its declared sint<65>",
        ),
        (
            "unsigned_too_wide",
            "a : uint<70> default 0x40_0000_0000_0000_0000",
            "needs 71 bits of magnitude, which does not fit its declared uint<70>",
        ),
        (
            "unsigned_negative",
            "a : uint<200> default -1",
            "is negative, but the field is unsigned (uint<200>)",
        ),
    ] {
        let source = write(
            &dir,
            &format!("{name}.harc"),
            &format!(
                "agent A\n    {field}\nend agent A\ntest T\n    let dut : Top\n    let x : A\n    \
                 run\n        log(info, \"x\")\n    end run\nend test T\n"
            ),
        );
        assert_rejects(&harc(&["dump-ir"], &source), want, name);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_component_value_arguments() {
    sim_fixture(
        "tests/fixtures/component_value_argument_test.harc",
        "COMPONENT_VALUE_ARGUMENT_OK",
        "value_args",
    );
}

const GENERIC_COMPONENT: &str = "agent A\n    v : uint<8> default 1\nend agent A\n";

#[test]
fn generic_arguments_on_a_non_generic_component_are_rejected() {
    let dir = scratch("generic_args");
    let cases: &[(&str, String, &str)] = &[
        (
            "test_scope_let",
            format!(
                "{GENERIC_COMPONENT}test T\n    let dut : Top\n    let x : A#(1, 2, 3)\n    run\n        \
                 log(info, \"x\")\n    end run\nend test T\n"
            ),
            "`let x` passes 3 generic argument(s) to component `A`, which declares no generic \
             parameters",
        ),
        (
            "component_field",
            format!(
                "{GENERIC_COMPONENT}agent Hd\n    inner : A#(W=99, Q=nosuch)\nend agent Hd\n\
                 test T\n    let dut : Top\n    let h : Hd\n    run\n        log(info, \"h\")\n    \
                 end run\nend test T\n"
            ),
            "field `Hd.inner` passes 2 generic argument(s) to component `A`",
        ),
        (
            "run_local_let",
            format!(
                "{GENERIC_COMPONENT}test T\n    let dut : Top\n    run\n        let z : A#(4)\n        \
                 log(info, \"z\")\n    end run\nend test T\n"
            ),
            "`let z` passes 1 generic argument(s) to component `A`",
        ),
        (
            "parameter",
            format!(
                "{GENERIC_COMPONENT}agent H\n    hookable take(src : A#(4))\n        log(info, \"t\")\n    \
                 end take\nend agent H\ntest T\n    let dut : Top\n    let h : H\n    run\n        \
                 log(info, \"h\")\n    end run\nend test T\n"
            ),
            "a component-typed parameter passes 1 generic argument(s) to component `A`",
        ),
        (
            "testbench_field",
            format!(
                "{GENERIC_COMPONENT}testbench Tb\n    dut : Top\n    a : A#(7)\nend testbench Tb\n\
                 impl T for Tb\n    run\n        log(info, \"a\")\n    end run\nend impl T\n"
            ),
            "testbench field `a` passes 1 generic argument(s) to component `A`",
        ),
    ];
    for (name, source, want) in cases {
        let path = write(&dir, &format!("{name}.harc"), source);
        assert_rejects(&harc(&["dump-ir"], &path), want, name);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A scalar integer type has exactly one parameter; a second one is a parse
/// error, so `harc check` reports it too.
#[test]
fn scalar_integer_type_with_extra_arguments_is_rejected() {
    let dir = scratch("extra_type_args");
    for (name, ty, keyword) in [
        ("uint", "uint<8, 9>", "uint"),
        ("sint", "sint<8, 9, 10>", "sint"),
        ("bits", "bits<4, 4>", "bits"),
    ] {
        let path = write(
            &dir,
            &format!("{name}.harc"),
            &format!(
                "test T\n    let dut : Top\n    run\n        let y : {ty} = 0\n        \
                 log(info, \"y\")\n    end run\nend test T\n"
            ),
        );
        let output = harc(&["check"], &path);
        assert_rejects(
            &output,
            &format!("`{keyword}` takes exactly one type argument, its width"),
            name,
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

const LOCAL_COMPONENT: &str = "agent A\n    v : uint<8> default 0\nend agent A\n";

#[test]
fn native_component_local_field_access() {
    sim_fixture(
        "tests/fixtures/component_local_field_test.harc",
        "COMPONENT_LOCAL_FIELD_OK",
        "local_field",
    );
}

/// An error inside a `${...}` capture is located at its statement, not at
/// line 1, column 1 (the capture's fragment-relative span read as a file
/// offset). Not component-specific: a scalar local's bad field too.
#[test]
fn interpolation_capture_errors_point_at_their_statement() {
    let dir = scratch("interp_span");
    for (name, source, line) in [
        (
            "component_local",
            format!(
                "{LOCAL_COMPONENT}test T\n    let dut : Top\n    run\n        let z : A\n        \
                 log(info, \"z=${{z.nosuch}}\")\n    end run\nend test T\n"
            ),
            ":8:9]",
        ),
        (
            "scalar_local",
            "test T\n    let dut : Top\n    run\n        let q : uint<8> = 1\n        \
             log(info, \"q=${q.nosuch}\")\n    end run\nend test T\n"
                .to_string(),
            ":5:9]",
        ),
    ] {
        let path = write(&dir, &format!("{name}.harc"), &source);
        let output = harc(&["dump-ir"], &path);
        let text = stderr_text(&output);
        assert!(
            !output.status.success() && text.contains(line) && !text.contains(":1:1]"),
            "{name}: expected the diagnostic at `{line}`:\n{text}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The verifier holds every persistent scalar default to its field's type,
/// not only scoreboard ones: a `Wide` default with bits above the declared
/// width, one that does not fill the carrier, one whose carrier words are
/// not a consistent sign extension, and a `Narrow` default on a > 64-bit
/// field at or above 2^63 (rendered as an unsigned decimal) are rejected.
#[test]
fn verifier_rejects_malformed_wide_defaults() {
    use harc::ir::{ComponentFieldKind, ScalarDefault};
    let src = "agent A\n    u : uint<100> default 0\n    s : sint<65> default 0\nend agent A\n\
               test T\n    let dut : Top\n    let x : A\n    run\n        log(info, \"x\")\n    \
               end run\nend test T\n";
    let parsed = harc::parser::parse_source(src).expect("parses");
    let merged = harc::codegen::merge::merge_for_sim(vec![parsed], None).expect("merge");
    let prog = harc::ir::lower::lower_program(&merged).expect("lowers");
    harc::ir::verify::verify_program(&prog).expect("verifies as lowered");

    let verify_with = |field: &str, default: ScalarDefault| {
        let mut broken = prog.clone();
        let target = broken.components[0]
            .fields
            .iter_mut()
            .find(|f| f.name == field)
            .expect("field");
        let ComponentFieldKind::Scalar { default: slot, .. } = &mut target.kind else {
            panic!("scalar field");
        };
        *slot = default;
        harc::ir::verify::verify_program(&broken).err().map(|errs| {
            errs.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        })
    };
    for (case, field, default) in [
        (
            "bit above uint<100>",
            "u",
            ScalarDefault::Wide(vec![0, 0, 0, 0x10]),
        ),
        (
            "wrong carrier length",
            "u",
            ScalarDefault::Wide(vec![1, 0, 0, 0, 0]),
        ),
        (
            "sign bit set without sign extension on sint<65>",
            "s",
            ScalarDefault::Wide(vec![0, 0, 1, 0]),
        ),
        (
            "sign extension without the sign bit on sint<65>",
            "s",
            ScalarDefault::Wide(vec![0, 0, 0xFFFF_FFFE, 0xFFFF_FFFF]),
        ),
        (
            "narrow at 2^63 on uint<100>",
            "u",
            ScalarDefault::Narrow(1 << 63),
        ),
    ] {
        let err = verify_with(field, default)
            .unwrap_or_else(|| panic!("{case}: the verifier accepted it"));
        assert!(
            err.contains("has invalid scalar default"),
            "{case}: unexpected diagnostic:\n{err}"
        );
    }
    // Controls: well-formed defaults verify.
    for (field, default) in [
        ("u", ScalarDefault::Wide(vec![0, 0, 0, 0x8])),
        (
            "s",
            ScalarDefault::Wide(vec![0, 0, 0xFFFF_FFFF, 0xFFFF_FFFF]),
        ),
        ("s", ScalarDefault::Wide(vec![u32::MAX; 4])),
        ("u", ScalarDefault::Narrow(i64::MAX as u64)),
    ] {
        assert!(
            verify_with(field, default.clone()).is_none(),
            "{field} = {default:?} should verify"
        );
    }
}
