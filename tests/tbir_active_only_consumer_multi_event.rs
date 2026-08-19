use harc::codegen::merge;
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;

fn lower_src(src: &str) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed = parse_source(src).expect("source parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    lower::lower_program(&merged)
}

#[test]
fn unrelated_always_on_handler_does_not_mask_dead_active_only_input() {
    let passive = r#"
transactor T
    req1 : in event<uint<8>>
    req2 : in event<uint<8>>
    seen : out event<uint<8>>
    n    : uint<32> default 0

    on req1(v)
        n = n + 1
    end on

    when active
        on req2(v)
            n = n + 1
        end on
    end when
end transactor T

testbench Tb
    dut : Top
    t : T passive
end testbench Tb

impl MultiInputTest for Tb
    run
        emit t.req2(1)
    end run
end impl MultiInputTest
"#;

    let err = lower_src(passive)
        .expect_err("req1's always-on subscriber must not hide req2's passive dead emit");
    assert!(
        matches!(err, lower::LowerError::Unsupported { .. }),
        "expected the event-driven passive-binding diagnostic, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("when active") && msg.contains("no subscriber"),
        "{msg}"
    );

    let active = passive.replace("t : T passive", "t : T active");
    let prog = lower_src(&active).expect("the same multi-input consumer is valid when active");
    verify::verify_program(&prog).expect("active control verifies");
}
