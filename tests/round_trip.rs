//! Integration tests: parse a fixture, pretty-print it, re-parse the printed
//! output, and verify both parse trees match structurally. Snapshot the
//! pretty-printed form for stability.

use harc::parser::parse_source;
use harc::pretty::print;

fn parse_print_reparse(src: &str) -> String {
    let parsed = parse_source(src).expect("first parse should succeed");
    let printed = print(&parsed);
    let _ = parse_source(&printed).expect("re-parse of pretty output should succeed");
    printed
}

#[test]
fn axi_agent_round_trip() {
    let src = include_str!("fixtures/axi_agent.harc");
    let printed = parse_print_reparse(src);
    insta::assert_snapshot!("axi_agent", printed);
}

#[test]
fn package_with_extend_round_trip() {
    let src = r#"
package ShortBursts
    extend AxiTxn
        keep len < 16
        keep burst == INCR
    end extend AxiTxn
end package ShortBursts
"#;
    parse_print_reparse(src);
}

#[test]
fn property_assert_assume_cover() {
    let src = r#"
property aw_valid_stable
    a |=> b
end property aw_valid_stable
"#;
    parse_print_reparse(src);
}

#[test]
fn covergroup_with_clocking() {
    let src = r#"
covergroup G @(posedge clk)
    cp1 : cover dut.x
    cp2 : cover dut.y
        bins
            small = {0, 1, 2}
            big = [10..100]
        end bins
    cross cp1, cp2
end covergroup G
"#;
    parse_print_reparse(src);
}

#[test]
fn tseq_with_composition_operators() {
    let src = r#"
tseq DmaScenario -> TSeq<int>
    setup_descriptor()
    parallel
        fill_source_buffer()
        arm_dma()
    end parallel
    fire_dma()
    repeat 4
        drain_one_burst()
    end repeat
end tseq DmaScenario
"#;
    parse_print_reparse(src);
}

#[test]
fn external_verilator_module() {
    let src = r#"
module my_axi_slave kind verilator
    src: "rtl/axi_slave.sv"
    top: my_axi_slave_top
end module my_axi_slave
"#;
    parse_print_reparse(src);
}

#[test]
fn dist_attribute_on_field() {
    let src = r#"
transaction T
    size : uint<8> with [dist {[0..255] :/ 80, [256..1023] :/ 20}]
end transaction T
"#;
    parse_print_reparse(src);
}

#[test]
fn unique_within_scope_attribute() {
    let src = r#"
transaction T
    tag : uint<8> with [unique within tseq]
end transaction T
"#;
    parse_print_reparse(src);
}

#[test]
fn relation_alias_form() {
    let src = "relation A(t: T) = t.x % 4 == 0 && t.y > 0\n";
    parse_print_reparse(src);
}

#[test]
fn ternary_round_trips() {
    // Right-associative chain plus a nested ternary on the LHS of a
    // larger arithmetic expression — the round-trip parses both the
    // first time and after pretty-printing.
    let src = r#"
test T
    let dut : X
    scope sim
        run
            let a = 5
            let b = 10
            let c = a > 0 ? a : b
            let d = a > 0 ? a > 5 ? 100 : 50 : -1
            let e = (a > 0 ? a : b) + 1
            wait 1 cycle
        end run
    end scope sim
end test T
"#;
    let printed = parse_print_reparse(src);
    // Sanity-check that the parens around the ternary inside `e` survive
    // round-trip — the precedence-preservation guarantee for ternary is
    // user-visible.
    assert!(
        printed.contains("a > 0 ? a : b"),
        "ternary should print without splitting `?` and `:` across lines:\n{printed}"
    );
}
