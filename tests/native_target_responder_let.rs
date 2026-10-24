//! Native v1 regression for target-responder `let` initializers after return.

use std::{fs, path::PathBuf, process::Command};

const TEST: &str = r#"
extern function unexpected_touch() -> uint<32>
extern function touch_count() -> uint<32>

bus B10Bus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus B10Bus

transactor B10Target bound to B10Bus
    reads : uint<32> default 0

    thread bus.read(addr: uint<8>)
        let snapshot : uint<32> = reads
        reads = reads + 1
        if addr < 8
            return 100 + snapshot
        else
            return 200 + snapshot
        end if
        let unreachable : uint<32> = unexpected_touch()
    end thread
end transactor B10Target

testbench B10Tb
    dut : TlmReadInitiatorPair
end testbench B10Tb

impl B10Test for B10Tb
    let mem : B10Bus = bind dut
    let target : B10Target passive = bind mem

    run
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
        wait until dut.done == 1 timeout 80 cycles fail("DUT did not finish")
        assert dut.result0 == 100 else fail("first response mismatch")
        assert dut.result1 == 201 else fail("second response mismatch")
        assert touch_count() == 0 else fail("unreachable let initializer executed")
        assert target.reads == 2 else fail("target local scope/lifetime regression")
    end run
end impl B10Test
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_v1_target_responder_skips_unreachable_let_initializer() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_target_responder_let_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&outdir);
    fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("b10_target_responder_let.harc");
    fs::write(&source, TEST).expect("write target responder fixture");
    let reference = outdir.join("b10_unreachable_let_ref.cpp");
    fs::write(
        &reference,
        "#include <cstdint>\nstatic uint64_t touches = 0;\nextern \"C\" uint64_t unexpected_touch() { return ++touches; }\nextern \"C\" uint64_t touch_count() { return touches; }\n",
    )
    .expect("write unique reference source");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/TlmReadInitiatorPair.sv"))
        .arg(&source)
        .args([
            "--top",
            "TlmReadInitiatorPair",
            "--codegen",
            "v1",
            "--ref-src",
        ])
        .arg(&reference)
        .args(["--jobs", "2", "--outdir"])
        .arg(&outdir)
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run native v1 target responder fixture");
    assert!(
        output.status.success(),
        "native target responder fixture failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ALL TESTS PASSED"),
        "native target responder fixture did not report success:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
