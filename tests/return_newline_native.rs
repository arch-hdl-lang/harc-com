use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

#[test]
fn bare_return_does_not_call_next_line_statement() {
    let root = std::env::temp_dir().join(format!("harc_return_newline_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create native regression directory");
    let harc = root.join("return_newline.harc");
    let dut = root.join("Top.sv");
    let outdir = root.join("out");
    fs::write(
        &harc,
        r#"testbench ReturnTb
    dut : Top
end testbench ReturnTb
impl ReturnTest for ReturnTb
    phase g
        log(info, "UNREACHABLE_CALLED")
    end phase g
    phase early_exit
        return
        g()
    end phase early_exit
    run
        early_exit()
        log(info, "CALLER_CONTINUES")
    end run
end impl ReturnTest
"#,
    )
    .expect("write HARC regression");
    fs::write(
        &dut,
        "module Top(input logic clk, input logic rst, output logic out); assign out = rst; endmodule\n",
    )
    .expect("write DUT regression");

    let output = Command::new(harc_bin())
        .args([
            "sim",
            "--sv",
            dut.to_str().expect("DUT path is valid UTF-8"),
            harc.to_str().expect("HARC path is valid UTF-8"),
            "--top",
            "Top",
            "--outdir",
            outdir.to_str().expect("output path is valid UTF-8"),
        ])
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .env("HARC_REQUIRE_VERILATOR", "1")
        .output()
        .expect("run native return regression");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "native regression failed:\n{log}");
    assert!(
        log.contains("ALL TESTS PASSED"),
        "native test did not pass:\n{log}"
    );
    assert!(
        !log.contains("UNREACHABLE_CALLED"),
        "next-line call executed:\n{log}"
    );
}
