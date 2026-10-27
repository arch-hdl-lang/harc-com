//! Native regression for signed captured fields widened above their carrier width.

use std::{fs, path::PathBuf, process::Command};

const TEST: &str = r#"
transaction Cap
    s : sint<80>
end transaction Cap
transaction Req128
    s : sint<128>
end transaction Req128
transaction Req64
    s : sint<64>
end transaction Req64
transaction Req80
    s : sint<80>
end transaction Req80
transaction Req200
    s : sint<200>
end transaction Req200
transaction UCap
    u : uint<80>
end transaction UCap
transaction UReq
    u : uint<200>
end transaction UReq
test SignedCapture
    let dut : Top
    run
        let c : Cap
        c.s = -1
        let n64 : Req64
        blocking randomize(n64) with
            n64.s == c.s
            n64.s < 0
        end randomize
        assert n64.s == -1 else fail("signed 80 to 64 capture")
        let n80 : Req80
        blocking randomize(n80) with
            n80.s == c.s
            n80.s < 0
        end randomize
        assert n80.s == -1 else fail("signed 80 to 80 capture")
        let n : Req128
        blocking randomize(n) with
            n.s == c.s
            n.s < 0
        end randomize
        assert n.s == -1 else fail("signed 80 to 128 capture")
        let w : Req200
        blocking randomize(w) with
            w.s == c.s
            w.s < 0
        end randomize
        assert w.s == -1 else fail("signed 80 to 200 capture")
        let p : Cap
        p.s = 1
        blocking randomize(w) with
            w.s == p.s
            w.s > 0
        end randomize
        assert w.s == 1 else fail("positive signed capture")
        let uc : UCap
        uc.u = 7
        let uw : UReq
        blocking randomize(uw) with
            uw.u == uc.u
            uw.u > 0
        end randomize
        assert uw.u == 7 else fail("unsigned capture")
    end run
end test SignedCapture
"#;

#[test]
fn native_signed_capture_declared_width_controls() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_capture_e2e_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("signed_capture.harc");
    fs::write(&source, TEST).expect("write signed capture fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run signed capture fixture");
    assert!(
        output.status.success(),
        "signed capture fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"));
}

const LARGE_POSITIVE: &str = r#"
transaction Cap
    s : sint<80>
end transaction Cap
transaction Req
    s : sint<8>
end transaction Req
test LargePositiveCapture
    let dut : Top
    run
        let c : Cap
        c.s = 1 << 70
        let r : Req
        blocking randomize(r) with
            r.s == c.s
        end randomize
    end run
end test LargePositiveCapture
"#;

const LARGE_RELATIONAL: &str = r#"
transaction Cap
    s : sint<80>
end transaction Cap
transaction Req
    s : sint<8>
end transaction Req
test LargeRelationalCapture
    let dut : Top
    run
        let c : Cap
        c.s = 1 << 70
        let r : Req
        blocking randomize(r) with
            r.s == 0
            r.s < c.s
        end randomize
        assert r.s == 0 else fail("large positive less-than capture")
        blocking randomize(r) with
            r.s == 0 || c.s == 1 << 70
        end randomize
        assert r.s == 0 else fail("unused large capture disjunction")
    end run
end test LargeRelationalCapture
"#;

const UNEQUAL_CAPTURES: &str = r#"
transaction Cap80
    s : sint<80>
end transaction Cap80
transaction Cap200
    s : sint<200>
end transaction Cap200
transaction Req
    s : sint<8>
end transaction Req
test UnequalCaptureWidths
    let dut : Top
    run
        let c : Cap80
        c.s = -1
        let d : Cap200
        d.s = -1
        let r : Req
        blocking randomize(r) with
            r.s == c.s
            d.s == r.s
        end randomize
        assert r.s == -1 else fail("unequal capture widths")
    end run
end test UnequalCaptureWidths
"#;

const SOFT_CAPTURE: &str = r#"
transaction Cap
    s : sint<80>
end transaction Cap
transaction Req
    s : sint<8>
end transaction Req
test SoftCapture
    let dut : Top
    run
        let c : Cap
        c.s = -1
        let r : Req
        blocking randomize(r) with
            r.s >= -1
            soft r.s == c.s weight 10
        end randomize
        assert r.s == -1 else fail("soft signed capture")
    end run
end test SoftCapture
"#;

#[test]
fn native_signed_capture_scans_soft_constraints() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_capture_soft_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("soft.harc");
    fs::write(&source, SOFT_CAPTURE).expect("write soft fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run soft fixture");
    assert!(
        output.status.success(),
        "soft fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"));
}

#[test]
fn native_signed_capture_handles_unequal_capture_widths() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_capture_unequal_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("unequal.harc");
    fs::write(&source, UNEQUAL_CAPTURES).expect("write unequal fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run unequal fixture");
    assert!(
        output.status.success(),
        "unequal fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"));
}

#[test]
fn native_signed_capture_preserves_large_relational_values() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_capture_relational_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("large_relational.harc");
    fs::write(&source, LARGE_RELATIONAL).expect("write relational fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run relational fixture");
    assert!(
        output.status.success(),
        "relational fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"));
}

#[test]
fn native_signed_capture_rejects_large_positive_narrowing() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_capture_large_positive_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("large_positive.harc");
    fs::write(&source, LARGE_POSITIVE).expect("write large positive fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run large positive fixture");
    let log = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(!output.status.success(), "large positive unexpectedly solved: {log}");
    assert!(log.contains("constraint UNSAT"), "expected UNSAT diagnostic: {log}");
}
