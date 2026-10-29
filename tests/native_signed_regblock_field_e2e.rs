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
