use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

fn source_file(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harc_check_cli_{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create check-cli temp dir");
    let path = dir.join(name);
    fs::write(&path, source).expect("write check-cli source");
    path
}

fn dump_ir(paths: &[&PathBuf]) -> Output {
    let mut command = Command::new(harc_bin());
    command.arg("dump-ir").env("NO_COLOR", "1");
    for path in paths {
        command.arg(path);
    }
    command.output().expect("run harc dump-ir")
}

fn assert_source_diagnostic(
    output: &Output,
    file: &PathBuf,
    line: usize,
    column: usize,
    source_line: &str,
    expected_message: &str,
) -> String {
    assert!(
        !output.status.success(),
        "dump-ir should reject the fixture"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains(expected_message),
        "expected `{expected_message}` in diagnostic:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("{}:{line}:{column}]", file.display())),
        "expected {} at {line}:{column} in diagnostic:\n{stderr}",
        file.display()
    );
    assert!(
        stderr.contains(source_line),
        "expected source line `{source_line}` in diagnostic:\n{stderr}"
    );
    assert!(
        stderr.contains("lowering failed here"),
        "expected a highlighted source span:\n{stderr}"
    );
    stderr
}

#[test]
fn check_accepts_wide_width_methods_through_1024_bits() {
    let path = source_file(
        "wide_ok.harc",
        r#"function wide_ok(a: uint<64>) -> uint<64>
    let v256 : uint<256> = a.zext<256>()
    let v1024 : uint<1024> = v256.zext<1024>()
    return v1024.trunc<64>()
end function wide_ok
"#,
    );
    let output = Command::new(harc_bin())
        .args(["check", path.to_str().unwrap()])
        .output()
        .expect("run harc check");
    assert!(
        output.status.success(),
        "harc check should accept widths through 1024\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_rejects_width_method_above_1024_bits() {
    let path = source_file(
        "wide_bad.harc",
        r#"function wide_bad(a: uint<64>) -> uint<64>
    let value = a.zext<1025>()
    return value.trunc<64>()
end function wide_bad
"#,
    );
    let output = Command::new(harc_bin())
        .args(["check", path.to_str().unwrap()])
        .output()
        .expect("run harc check");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "harc check should reject 1025 bits"
    );
    assert!(
        stderr.contains(".zext<1025>()") && stderr.contains("1..=1024"),
        "expected source-located language-limit diagnostic:\n{stderr}"
    );
}

#[test]
fn check_rejects_zero_and_nonconstant_width_methods() {
    for (name, width, expected) in [
        ("zero.harc", "0", "1..=1024"),
        ("nonconstant.harc", "WIDTH", "literal width"),
    ] {
        let source = format!(
            "function bad(a: uint<64>) -> uint<64>\n    let value = a.zext<{width}>()\n    return value.trunc<64>()\nend function bad\n"
        );
        let path = source_file(name, &source);
        let output = Command::new(harc_bin())
            .args(["check", path.to_str().unwrap()])
            .output()
            .expect("run harc check");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "harc check should reject {width}");
        assert!(
            stderr.contains(expected),
            "expected `{expected}` in diagnostic for {width}:\n{stderr}"
        );
    }
}

#[test]
fn dump_ir_locates_an_unsupported_component_call_in_wait_predicate() {
    let definitions = source_file(
        "located_call_defs.harc",
        r#"sequencer Counter
    hookable next() -> uint<8>
        return 1
    end next
end sequencer Counter

testbench Tb
    dut : Top
    counter : Counter
end testbench Tb
"#,
    );
    let test = source_file(
        "located_call_test.harc",
        r#"impl LocatedCallTest for Tb
    run
        let first = counter.next()
        wait until counter.next() == 1
        let last = counter.next()
    end run
end impl LocatedCallTest
"#,
    );

    let output = dump_ir(&[&definitions, &test]);
    let stderr = assert_source_diagnostic(
        &output,
        &test,
        4,
        9,
        "wait until counter.next() == 1",
        "value call requiring statement",
    );
    assert!(
        stderr.contains("--codegen v1"),
        "unsupported diagnostics must preserve the v1 escape hatch:\n{stderr}"
    );
}

#[test]
fn dump_ir_accepts_a_nested_synchronous_transactor_call_in_wait_predicate() {
    let definitions = source_file(
        "located_transactor_wait_defs.harc",
        r#"transactor ReadyProbe
    dut : Top

    when active
        hookable ready() -> bool
            return true
        end ready
    end when
end transactor ReadyProbe

testbench ReadyTb
    dut : Top
    xact : ReadyProbe active
end testbench ReadyTb
"#,
    );
    let test = source_file(
        "located_transactor_wait_test.harc",
        r#"impl LocatedTransactorWaitTest for ReadyTb
    run
        xact.dut = dut
        wait until true && xact.ready()
    end run
end impl LocatedTransactorWaitTest
"#,
    );

    let output = dump_ir(&[&definitions, &test]);
    assert!(
        output.status.success(),
        "dump-ir must accept a synchronous transactor call that is re-evaluated in the wait predicate:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("WaitUntil") && stdout.contains("xact.ready"),
        "dump-ir must preserve the nested call in the wait predicate:\n{stdout}"
    );
}

#[test]
fn dump_ir_locates_an_invalid_component_copy_assignment() {
    let definitions = source_file(
        "located_copy_defs.harc",
        r#"agent Expected
    value : uint<8> default 0
end agent Expected

agent Wrong
    value : uint<8> default 0
end agent Wrong

agent Holder
    slot : Expected
end agent Holder

testbench TbCopy
    dut : Top
    holder : Holder
    expected : Expected
    wrong : Wrong
end testbench TbCopy
"#,
    );
    let test = source_file(
        "located_copy_test.harc",
        r#"impl LocatedCopyTest for TbCopy
    run
        holder.slot = expected
        holder.slot = wrong
        holder.slot = expected
    end run
end impl LocatedCopyTest
"#,
    );

    let output = dump_ir(&[&definitions, &test]);
    assert_source_diagnostic(
        &output,
        &test,
        4,
        9,
        "holder.slot = wrong",
        "cannot copy component `Wrong`",
    );
}

#[test]
fn dump_ir_locates_a_genuine_component_field_width_error() {
    let definitions = source_file(
        "located_width_defs.harc",
        r#"agent WidthSink
    wide : uint<64> default 0
    narrow : uint<32> default 0

    function update(value: uint<64>)
        wide = value
        narrow = value
        wide = value
    end update
end agent WidthSink
"#,
    );
    let test = source_file(
        "located_width_test.harc",
        r#"testbench TbWidth
    dut : Top
    sink : WidthSink
end testbench TbWidth

impl LocatedWidthTest for TbWidth
    run
        wait 1 cycle
    end run
end impl LocatedWidthTest
"#,
    );

    let output = dump_ir(&[&test, &definitions]);
    let stderr = assert_source_diagnostic(
        &output,
        &definitions,
        7,
        9,
        "narrow = value",
        "assignment of a 64-bit value to component field `narrow`",
    );
    assert!(
        !stderr.contains("--codegen v1"),
        "invalid source must not suggest another backend:\n{stderr}"
    );
}

#[test]
fn dump_ir_locates_an_invalid_constant_initializer() {
    let source = source_file(
        "located_constant.harc",
        r#"const GOOD : uint<8> = 1
const BAD : uint<8> = MISSING + 1
const ALSO_GOOD : uint<8> = 2

test ConstantDiagnosticTest
    let dut : Top
    run
        wait 1 cycle
    end run
end test ConstantDiagnosticTest
"#,
    );

    let output = dump_ir(&[&source]);
    let stderr = assert_source_diagnostic(
        &output,
        &source,
        2,
        23,
        "const BAD : uint<8> = MISSING + 1",
        "references `MISSING`",
    );
    assert!(
        !stderr.contains("--codegen v1"),
        "an invalid constant must not suggest another backend:\n{stderr}"
    );
}

#[test]
fn dump_ir_locates_invalid_string_value_uses() {
    let bad_argument = source_file(
        "located_string_argument.harc",
        r#"extern function take(value: uint<8>) -> uint<1>
test BadStringArgument
    let dut : Top
    run
        let ok = take("text")
    end run
end test BadStringArgument
"#,
    );
    let output = dump_ir(&[&bad_argument]);
    assert_source_diagnostic(
        &output,
        &bad_argument,
        5,
        23,
        "let ok = take(\"text\")",
        "parameter `value` of extern fn `take`",
    );

    let unknown_argument = source_file(
        "located_string_unknown_argument.harc",
        r#"function take(value) -> uint<1>
    return 1
end function take
test BadStringUnknownArgument
    let dut : Top
    run
        let ok = take("text")
    end run
end test BadStringUnknownArgument
"#,
    );
    let output = dump_ir(&[&unknown_argument]);
    assert_source_diagnostic(
        &output,
        &unknown_argument,
        7,
        23,
        "let ok = take(\"text\")",
        "parameter `value` of helper `take`",
    );

    let bad_component_argument = source_file(
        "located_string_component_argument.harc",
        r#"agent Sink
    function take(value: uint<8>)
        let copy : uint<8> = value
    end take
end agent Sink
testbench Tb
    dut : Top
    sink : Sink
end testbench Tb
impl BadStringComponentArgument for Tb
    run
        sink.take("text")
    end run
end impl BadStringComponentArgument
"#,
    );
    let output = dump_ir(&[&bad_component_argument]);
    assert_source_diagnostic(
        &output,
        &bad_component_argument,
        12,
        19,
        "sink.take(\"text\")",
        "parameter `value` of `Sink.take`",
    );

    let interpolation = source_file(
        "located_string_interpolation.harc",
        r#"extern function take(value: String) -> uint<1>
test BadStringInterpolation
    let dut : Top
    run
        let ok = take("${dut}")
    end run
end test BadStringInterpolation
"#,
    );
    let output = dump_ir(&[&interpolation]);
    assert_source_diagnostic(
        &output,
        &interpolation,
        5,
        23,
        "let ok = take(\"${dut}\")",
        "String value literals do not support interpolation",
    );

    let assignment = source_file(
        "located_string_assignment.harc",
        r#"test BadStringAssignment
    let dut : Top
    run
        let n : uint<8> = "text"
    end run
end test BadStringAssignment
"#,
    );
    let output = dump_ir(&[&assignment]);
    assert_source_diagnostic(
        &output,
        &assignment,
        4,
        27,
        "let n : uint<8> = \"text\"",
        "local `n` has type UInt",
    );

    let bad_return = source_file(
        "located_string_return.harc",
        r#"function bad() -> String
    return 1
end function bad
test BadStringReturn
    let dut : Top
    run
        let value : String = bad()
    end run
end test BadStringReturn
"#,
    );
    let output = dump_ir(&[&bad_return]);
    assert_source_diagnostic(
        &output,
        &bad_return,
        2,
        12,
        "return 1",
        "helper return takes a `String` value",
    );

    let constraint = source_file(
        "located_string_constraint.harc",
        r#"struct Item
    value : uint<8>
end struct Item
test BadStringConstraint
    let dut : Top
    run
        let item : Item
        randomize(item) with
            item.value == "text"
        end randomize
    end run
end test BadStringConstraint
"#,
    );
    let output = dump_ir(&[&constraint]);
    assert_source_diagnostic(
        &output,
        &constraint,
        9,
        27,
        "item.value == \"text\"",
        "String values are not supported",
    );
}

#[test]
fn dump_ir_locates_nested_string_type_declarations() {
    let parameter = source_file(
        "located_nested_string_parameter.harc",
        r#"function bad(values: queue<String>) -> uint<1>
    return 1
end function bad
test NestedStringParameter
    let dut : Top
    run
        wait 1 cycle
    end run
end test NestedStringParameter
"#,
    );
    let output = dump_ir(&[&parameter]);
    assert_source_diagnostic(
        &output,
        &parameter,
        1,
        22,
        "function bad(values: queue<String>) -> uint<1>",
        "String containers and aggregates are not",
    );

    let local = source_file(
        "located_nested_string_local.harc",
        r#"test NestedStringLocal
    let dut : Top
    run
        let values : event<String>
        wait 1 cycle
    end run
end test NestedStringLocal
"#,
    );
    let output = dump_ir(&[&local]);
    assert_source_diagnostic(
        &output,
        &local,
        4,
        22,
        "let values : event<String>",
        "String containers and aggregates are not",
    );
}

#[test]
fn dump_ir_locates_an_invalid_statement_in_the_extension_file() {
    let base = source_file(
        "located_extend_base.harc",
        r#"test ExtendedTest
    let dut : Top
    run
        wait 1 cycle
    end run
end test ExtendedTest
"#,
    );
    let extension = source_file(
        "located_extend_body.harc",
        r#"extend ExtendedTest
    let wide : uint<64> = 1
    let bad : uint<8> = wide
end extend ExtendedTest
"#,
    );

    let output = dump_ir(&[&base, &extension]);
    let stderr = assert_source_diagnostic(
        &output,
        &extension,
        3,
        5,
        "let bad : uint<8> = wide",
        "assignment of a 64-bit value to `bad`, declared 8 bits",
    );
    assert!(!stderr.contains("OutOfBounds"), "{stderr}");
}

#[test]
fn dump_ir_locates_a_testbench_lifecycle_error_in_the_testbench_file() {
    let testbench = source_file(
        "located_lifecycle_tb.harc",
        r#"testbench LifecycleTb
    dut : Top
    check
        let wide : uint<64> = 1
        let bad : uint<8> = wide
    end check
end testbench LifecycleTb
"#,
    );
    let test = source_file(
        "located_lifecycle_test.harc",
        r#"impl LifecycleTest for LifecycleTb
    run
        wait 1 cycle
    end run
end impl LifecycleTest
"#,
    );

    let output = dump_ir(&[&test, &testbench]);
    let stderr = assert_source_diagnostic(
        &output,
        &testbench,
        5,
        9,
        "let bad : uint<8> = wide",
        "assignment of a 64-bit value to `bad`, declared 8 bits",
    );
    assert!(!stderr.contains("OutOfBounds"), "{stderr}");
}

#[test]
fn dump_ir_locates_nested_helper_result_narrowing_at_the_call_argument() {
    let helper = source_file(
        "located_callable_narrowing_helper.harc",
        r#"function read_offset(raw: uint<64>) -> uint<64>
    return raw & 0xffff
end function read_offset
"#,
    );
    let testbench = source_file(
        "located_callable_narrowing_tb.harc",
        r#"
testbench Tb
    dut : Top
    function program(off0: uint<16>, off1: uint<16>, off2: uint<16>, off3: uint<16>, off4: uint<16>)
        log(info, "off4=${off4}")
    end function program
end testbench Tb
"#,
    );
    let test = source_file(
        "located_callable_narrowing_test.harc",
        r#"
impl T for Tb
    run
        program(
            0,
            0,
            0,
            0,
            read_offset(0x1234))
    end run
end impl T
"#,
    );

    let output = dump_ir(&[&helper, &testbench, &test]);
    let stderr = assert_source_diagnostic(
        &output,
        &test,
        9,
        13,
        "read_offset(0x1234))",
        "parameter `off4`",
    );
    assert!(
        stderr.contains("64-bit") && stderr.contains("16 bits"),
        "{stderr}"
    );
    assert!(
        stderr.contains("testbench method") && stderr.contains("`program`"),
        "{stderr}"
    );
    assert!(stderr.contains("narrows"), "{stderr}");
    assert!(!stderr.contains("internal error"), "{stderr}");
}

#[test]
fn dump_ir_locates_widthless_const_composition_narrowing_at_the_call_argument() {
    let constants = source_file("located_callable_const.harc", "const OFFSET = 9\n");
    let testbench = source_file(
        "located_callable_const_tb.harc",
        r#"testbench Tb
    dut : Top
    function program(off: uint<16>)
        log(info, "off=${off}")
    end function program
end testbench Tb
"#,
    );
    let test = source_file(
        "located_callable_const_test.harc",
        r#"impl T for Tb
    run
        let wide : uint<64> = 1
        program(wide | OFFSET)
    end run
end impl T
"#,
    );

    let output = dump_ir(&[&constants, &testbench, &test]);
    let stderr =
        assert_source_diagnostic(&output, &test, 4, 17, "wide | OFFSET", "parameter `off`");
    assert!(
        stderr.contains("64-bit") && stderr.contains("narrows"),
        "{stderr}"
    );
    assert!(!stderr.contains("internal error"), "{stderr}");
}

#[test]
fn dump_ir_locates_tseq_argument_narrowing_at_the_argument() {
    let sequence = source_file(
        "located_tseq_argument_sequence.harc",
        r#"transaction Beat
    value : uint<8>
end transaction Beat

const OFFSET = 9

tseq Make(seed: uint<16>) -> TSeq<Beat>
    let beat : Beat
    beat.value = seed.trunc<8>()
    yield beat
end tseq Make
"#,
    );
    let test = source_file(
        "located_tseq_argument_test.harc",
        r#"test T
    let dut : Top
    run
        let wide : uint<64> = 1
        let values = Make(wide | OFFSET)
    end run
end test T
"#,
    );

    let output = dump_ir(&[&sequence, &test]);
    let stderr = assert_source_diagnostic(
        &output,
        &test,
        5,
        27,
        "wide | OFFSET",
        "parameter of tseq `Make`",
    );
    assert!(
        stderr.contains("64-bit") && stderr.contains("narrows"),
        "{stderr}"
    );
    assert!(!stderr.contains("internal error"), "{stderr}");
}

#[test]
fn dump_ir_ignores_a_recovered_message_error_when_locating_a_later_assignment() {
    let component = source_file(
        "located_recovered_message.harc",
        r#"agent MessageModel
    value : uint<8> = 0

    function read() -> uint<8>
        return value
    end read

    function check_value()
        assert true else fail("value=${read()}")
    end check_value
end agent MessageModel
"#,
    );
    let testbench = source_file(
        "located_component_copy_tb.harc",
        r#"agent Leaf
    value : uint<8> = 0
end agent Leaf

agent Holder
    leaf : Leaf
end agent Holder

testbench CopyTb
    dut : Top
    destination : Leaf
    holder : Holder

    function reset()
        destination = holder
    end reset
end testbench CopyTb
"#,
    );
    let test = source_file(
        "located_component_copy_test.harc",
        r#"impl CopyTest for CopyTb
    run
        reset()
    end run
end impl CopyTest
"#,
    );

    let output = dump_ir(&[&component, &testbench, &test]);
    let stderr = assert_source_diagnostic(
        &output,
        &testbench,
        15,
        9,
        "destination = holder",
        "cannot copy component `Holder` into component destination of type `Leaf`",
    );
    assert!(
        !stderr.contains(&format!("{}:1:1]", component.display())),
        "a recovered message-lowering error must not own the final diagnostic:\n{stderr}"
    );
    assert!(
        !stderr.contains("--codegen v1"),
        "a type-mismatched component copy is invalid in both backends:\n{stderr}"
    );
}

#[test]
fn dump_ir_locates_a_component_default_width_error_at_the_default_expression() {
    let source = source_file(
        "located_default_width.harc",
        r#"agent Defaults
    small : uint<8> default 256
end agent Defaults

test DefaultWidthTest
    let dut : Top
    run
        wait 1 cycle
    end run
end test DefaultWidthTest
"#,
    );

    let output = dump_ir(&[&source]);
    let stderr = assert_source_diagnostic(
        &output,
        &source,
        2,
        29,
        "small : uint<8> default 256",
        "value 256 does not fit",
    );
    assert!(!stderr.contains("OutOfBounds"), "{stderr}");
}
