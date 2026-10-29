//! Native regression: `sint<N>` / `SInt<N>` regblock fields read back
//! sign-extended as `sint<N>`, from the mirror (wo fields) and from a
//! frontdoor bus read (rw fields), and a negative write stores the
//! field's two's-complement pattern without touching its neighbours.
//! Unsigned fields stay zero-extended. Before the fix a `sint<4>` field
//! holding `0xF` read as `15`.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_signed_regblock_fields_sign_extend() {
    let root = repo_root();
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_regblock_field_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(root.join("tests/dut/AxiLiteRegs.sv"))
        .args(["--top", "AxiLiteRegs", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .arg(root.join("tests/fixtures/regblock_signed_field_test.harc"))
        .output()
        .expect("run signed regblock field fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED"),
        "signed regblock field fixture failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("SIGNED_FIELD_OK"),
        "missing marker:\n{stdout}"
    );
}

const RESET_FIXTURE: &str = "tests/fixtures/regblock_signed_reset_test.harc";

/// A `sint<N>` field's `reset` is a value of that type: `-2` and `-8` on a
/// `sint<4>` reset the mirror to their 4-bit patterns and read back as
/// `-2` / `-8`; unsigned field resets are unchanged.
#[test]
fn native_signed_regblock_field_resets_accept_negative_values() {
    let root = repo_root();
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_regblock_reset_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(root.join("tests/dut/AxiLiteRegs.sv"))
        .args(["--top", "AxiLiteRegs", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .arg(root.join(RESET_FIXTURE))
        .output()
        .expect("run signed reset fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout.contains("ALL TESTS PASSED")
            && stdout.contains("SIGNED_RESET_OK"),
        "signed regblock reset fixture failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Out-of-range signed resets are precise compile errors — including a
/// positive bit pattern such as `0xE` on a `sint<4>`, which names the
/// signed value to write instead.
#[test]
fn signed_regblock_field_reset_out_of_range_is_rejected() {
    let root = repo_root();
    let base = std::fs::read_to_string(root.join(RESET_FIXTURE)).expect("read fixture");
    let dir = std::env::temp_dir().join(format!(
        "harc_signed_regblock_reset_range_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create output directory");
    for (name, from, to, want) in [
        (
            "neg9",
            "reset -8",
            "reset -9",
            "field `M` is -9, which is outside the range of its signed 4-bit type [-8, 7]",
        ),
        (
            "pos8",
            "reset -8",
            "reset 8",
            "field `M` is 8, which is outside the range of its signed 4-bit type [-8, 7]",
        ),
        ("pattern", "reset -2", "reset 0xE", "write -2"),
    ] {
        assert!(base.contains(from), "fixture shape changed: {from}");
        let source = dir.join(format!("{name}.harc"));
        std::fs::write(&source, base.replacen(from, to, 1)).expect("write variant");
        let output = Command::new(env!("CARGO_BIN_EXE_harc"))
            .args(["sim", "--emit-only", "--sv"])
            .arg(root.join("tests/dut/AxiLiteRegs.sv"))
            .args(["--top", "AxiLiteRegs", "--outdir"])
            .arg(dir.join(format!("{name}-out")))
            .arg(&source)
            .output()
            .expect("run harc");
        let stderr = String::from_utf8_lossy(&output.stderr).replace(['│', '\n'], " ");
        let stderr = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            !output.status.success() && stderr.contains(want),
            "{name}: expected `{want}`:\n{stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}
