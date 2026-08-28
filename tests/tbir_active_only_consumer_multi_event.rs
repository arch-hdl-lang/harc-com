use harc::codegen::merge;
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;

fn lower_src(src: &str) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed = parse_source(src).expect("source parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    lower::lower_program(&merged)
}

#[test]
fn passive_multi_input_consumer_retains_each_handlers_activation() {
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

    let prog = lower_src(passive).expect("the passive binding is legal like v1");
    verify::verify_program(&prog).expect("passive program verifies");
    assert_eq!(
        prog.testbenches[0].component_fields[0].mode,
        Some(ir::ComponentInstanceMode::Passive)
    );
    let handlers = &prog.components[0].on_handlers;
    assert_eq!(handlers.len(), 2);
    assert_eq!(handlers[0].event, "req1");
    assert_eq!(handlers[0].activation, ir::Activation::Always);
    assert_eq!(handlers[1].event, "req2");
    assert_eq!(handlers[1].activation, ir::Activation::ActiveOnly);

    let active = passive.replace("t : T passive", "t : T active");
    let prog = lower_src(&active).expect("the same multi-input consumer is valid when active");
    verify::verify_program(&prog).expect("active control verifies");
}
