use std::fs;
use std::process::Command;

#[test]
fn native_pure_void_helpers_and_controls_continue() {
    let root = std::env::temp_dir().join(format!("harc_void_helper_e2e_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create e2e directory");
    fs::write(
        root.join("Top.sv"),
        "module Top(input logic clk); endmodule\n",
    )
    .expect("write DUT");
    fs::write(
        root.join("void.harc"),
        r#"
function empty()
end function empty

function local_only()
    let x: uint<8> = 7
end function local_only

function bare_return()
    return
end function bare_return

function typed() -> uint<8>
    return 7
end function typed

function impure()
    log(info, "IMPURE_EXECUTED")
end function impure

function consume_pair(first: uint<8>, second: uint<8>)
end function consume_pair

function argument(value: uint<8>) -> uint<8>
    log(info, "ARGUMENT_${value}")
    return value
end function argument

test PureVoidNative
    let dut : Top
    run
        empty()
        local_only()
        bare_return()
        let value = typed()
        assert value == 7 else fail("typed helper")
        impure()
        consume_pair(argument(1), argument(2))
        log(info, "CALLER_CONTINUED")
    end run
end test PureVoidNative
"#,
    )
    .expect("write HARC test");

    for common in [false, true] {
        let outdir = root.join(if common { "common" } else { "default" });
        let mut command = Command::new(env!("CARGO_BIN_EXE_harc"));
        command
            .args([
                "sim",
                "--sv",
                "Top.sv",
                "void.harc",
                "--top",
                "Top",
                "--outdir",
                outdir.to_str().expect("UTF-8 outdir"),
                "--rebuild",
            ])
            .current_dir(&root)
            .env("HARC_NO_LEARN", "1")
            .env("ARCH_NO_LEARN", "1")
            .env("HARC_REQUIRE_VERILATOR", "1");
        if common {
            command.args(["--cpp-split", "tests", "--cpp-split-layout", "common"]);
        }
        let output = command.output().expect("run native HARC test");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("IMPURE_EXECUTED"), "{stdout}");
        assert_eq!(stdout.matches("ARGUMENT_1").count(), 1, "{stdout}");
        assert_eq!(stdout.matches("ARGUMENT_2").count(), 1, "{stdout}");
        assert!(
            stdout.find("ARGUMENT_1").unwrap() < stdout.find("ARGUMENT_2").unwrap(),
            "{stdout}"
        );
        assert!(stdout.contains("CALLER_CONTINUED"), "{stdout}");
        assert!(stdout.contains("ALL TESTS PASSED"), "{stdout}");
    }
    let _ = fs::remove_dir_all(&root);
}
