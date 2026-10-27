//! Native regression for signed captured fields widened above their carrier width.

use std::{fs, path::PathBuf, process::Command};

const TEST: &str = r#"
transaction Cap
    s : sint<80>
end transaction Cap
transaction Req128
    s : sint<128>
end transaction Req128
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
