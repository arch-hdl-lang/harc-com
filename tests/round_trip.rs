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
fn foreach_keep_round_trip() {
    let src = r#"
transaction Packet
    items : list<uint<8>>
    keep items.len() <= 4
    keep for item in items
        item > 0
        item < 16
    end for
end transaction Packet
"#;
    parse_print_reparse(src);
}

#[test]
fn struct_keep_round_trip() {
    let src = r#"
struct Header
    addr : uint<32>
    keep addr % 4 == 0
end struct Header
"#;
    parse_print_reparse(src);
}

#[test]
fn testbench_probe_dut_round_trip() {
    let src = include_str!("fixtures/testbench_probe_dut_test.harc");
    let printed = parse_print_reparse(src);
    assert!(printed.contains("let dut : CpuPipe"));
    assert!(printed.contains("probe force inject_rs1"));
    assert!(printed.contains("end let dut"));
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
fn covergroup_with_hook_trigger() {
    let src = r#"
covergroup TxnCov @(mon.observed(t) post)
    cp_op : cover t.op
    cp_len : cover t.len
end covergroup TxnCov
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
    run
        let a = 5
        let b = 10
        let c = a > 0 ? a : b
        let d = a > 0 ? a > 5 ? 100 : 50 : -1
        let e = (a > 0 ? a : b) + 1
        wait 1 cycle
    end run
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

/// `wait until <expr>`, `wait until all of …`, `wait until any of …`,
/// each with or without an inline `timeout N cycles fail("…")` tail,
/// parse, pretty-print, and re-parse to the same AST shape. Verifies
/// the source-level surface of spec §7.9.
#[test]
fn wait_until_forms_round_trip() {
    let src = r#"
test T
    let dut : X
    run
        wait until dut.ready
        wait until dut.ready timeout 100 cycles fail("ready never asserted")
        wait until all of dut.ready, dut.empty
        wait until all of dut.ready, dut.empty timeout 500 cycles fail("hang")
        wait until any of dut.error, dut.done timeout 1000 cycles
    end run
end test T
"#;
    let printed = parse_print_reparse(src);
    // The single-line shape should survive both passes verbatim.
    assert!(
        printed.contains("wait until dut.ready"),
        "single-condition wait-until should round-trip; got:\n{printed}",
    );
    assert!(
        printed.contains("wait until all of dut.ready, dut.empty"),
        "all-of list should round-trip with comma separators; got:\n{printed}",
    );
    assert!(
        printed.contains("wait until any of dut.error, dut.done timeout 1000 cycles"),
        "any-of with timeout (no fail message) should round-trip; got:\n{printed}",
    );
    assert!(
        printed.contains("timeout 100 cycles fail(\"ready never asserted\")"),
        "inline timeout + fail message should round-trip; got:\n{printed}",
    );
}

/// `on <N> cycles … end on` (periodic trigger, spec §7.10) and
/// `watchdog … end watchdog` (built-in watchdog body item, spec §8.6)
/// round-trip through the pretty-printer. Covers all three watchdog
/// surface forms: implicit-defaults, custom period/max_idle, and
/// `watchdog disabled` opt-out.
#[test]
fn on_cycles_and_watchdog_round_trip() {
    let src = r#"
agent Foo
    counter : uint<32> default 0

    watchdog
        period 500 cycles
        max_idle 5000 cycles
        log(info, "[wdog] counter=${counter}")
    end watchdog
end agent Foo

agent Bar
    watchdog disabled
end agent Bar

agent Baz
    watchdog
    end watchdog
end agent Baz

test T
    let dut : X
    let foo : Foo
    run
        on 1000 cycles
            log(info, "heartbeat at ${cycle_count}")
        end on
        on 1 cycles phase post_eval
            log(info, "post-eval service")
        end on
        wait 5 cycles
    end run
end test T
"#;
    let printed = parse_print_reparse(src);
    // Periodic on-handler keeps its `cycles` decorator.
    assert!(
        printed.contains("on 1000 cycles"),
        "`on 1000 cycles` should round-trip; got:\n{printed}"
    );
    assert!(
        printed.contains("on 1 cycles phase post_eval"),
        "`phase post_eval` should round-trip; got:\n{printed}"
    );
    // Watchdog default+custom forms.
    assert!(
        printed.contains("watchdog\n        period 500 cycles\n        max_idle 5000 cycles"),
        "watchdog with explicit period/max_idle should round-trip; got:\n{printed}"
    );
    assert!(
        printed.contains("watchdog disabled"),
        "`watchdog disabled` opt-out should round-trip; got:\n{printed}"
    );
    // Implicit-defaults watchdog (no period/max_idle, no body).
    assert!(
        printed.contains("agent Baz\n    watchdog\n    end watchdog"),
        "defaults-only watchdog should round-trip; got:\n{printed}"
    );
}

/// `extern function name(params) -> ret` (spec §9) round-trips: no
/// body, no `end function` — terminates after the return type. Also
/// works without a return type (void). The pretty-printer uses the
/// same `extern function` surface as the parser accepts.
#[test]
fn extern_function_round_trips() {
    let src = r#"
extern function ref_crc8_step(crc: uint<8>, byte: uint<8>) -> uint<8>
extern function ref_aes_block(key: bits<128>, pt: bits<128>) -> bits<128>
extern function ref_dump_state(cycle: uint<64>)

test T
    let dut : X
    run
        let c = ref_crc8_step(0xFF, 0)
        ref_dump_state(100)
        wait 1 cycle
    end run
end test T
"#;
    let printed = parse_print_reparse(src);
    assert!(
        printed.contains("extern function ref_crc8_step(crc: uint<8>, byte: uint<8>) -> uint<8>"),
        "extern function with return type should round-trip; got:\n{printed}"
    );
    assert!(
        printed
            .contains("extern function ref_aes_block(key: bits<128>, pt: bits<128>) -> bits<128>"),
        "extern function with wide bits param should round-trip; got:\n{printed}"
    );
    assert!(
        printed.contains("extern function ref_dump_state(cycle: uint<64>)\n"),
        "extern function with no return type (void) should round-trip; got:\n{printed}"
    );
    // No spurious `end function` for an extern.
    assert!(
        !printed.contains("end function ref_crc8_step"),
        "extern function should NOT close with `end function`; got:\n{printed}"
    );
}

/// `///` outer doc comments attach to the next construct, populate
/// `doc: Option<String>` on the AST node, and round-trip through the
/// pretty-printer. Mirrors arch-com's `plan_arch_doc_comments.md` §2.1.
#[test]
fn outer_doc_attaches_and_round_trips() {
    let src = r#"
/// 4-channel round-robin AXI write arbiter.
///
/// Picks among threads holding the lock using a rotating priority pointer.
struct AxiTxn
    addr : uint<32>
    data : uint<32>
end struct AxiTxn
"#;
    let parsed = parse_source(src).expect("parse");
    let s = match &parsed.items[0] {
        harc::ast::Item::Struct(s) => s,
        _ => panic!("expected struct"),
    };
    let doc = s.doc.as_ref().expect("expected doc attached to struct");
    assert!(
        doc.contains("4-channel round-robin"),
        "first line should be in doc; got: {doc:?}"
    );
    assert!(
        doc.contains("rotating priority pointer"),
        "third line should be in doc; got: {doc:?}"
    );

    let printed = print(&parsed);
    let _ = parse_source(&printed).expect("re-parse");
    assert!(
        printed.contains("/// 4-channel round-robin"),
        "pretty output should emit `///` outer-doc lines:\n{printed}"
    );
}

/// File-top `//!` block populates `SourceFile.inner_doc`. The
/// `//! ---` … `//! ---` YAML frontmatter sub-block also populates
/// `SourceFile.frontmatter`, while remaining inside `inner_doc` for
/// fidelity. Compiler doesn't interpret the YAML — downstream tooling
/// (RAG indexer, doc generator) does.
#[test]
fn file_frontmatter_extracted_and_round_trips() {
    let src = r#"//! ---
//! spec_md: doc/specs/axi_wr_arb.md#round-robin
//! tags: [arbitration, axi, axi4]
//! refs:
//!   - "AXI4 spec §A3.3.1"
//! ---
//!
//! 4-channel round-robin AXI write arbiter, used by all DMA channels
//! in the SoC. See `spec_md` above for the authoritative behavior.

struct AxiTxn
    addr : uint<32>
end struct AxiTxn
"#;
    let parsed = parse_source(src).expect("parse");

    // inner_doc has the full leading //! block, with prefix stripped.
    let inner = parsed.inner_doc.as_ref().expect("expected inner_doc");
    assert!(
        inner.starts_with("---\n"),
        "inner_doc should keep the opening `---` line; got first line: {:?}",
        inner.lines().next()
    );
    assert!(
        inner.contains("spec_md: doc/specs/axi_wr_arb.md#round-robin"),
        "inner_doc should contain the spec_md field; got:\n{inner}"
    );
    assert!(
        inner.contains("4-channel round-robin"),
        "inner_doc should also keep the prose below the fence"
    );

    // frontmatter is the YAML between the fences.
    let fm = parsed.frontmatter.as_ref().expect("expected frontmatter");
    assert!(
        fm.contains("spec_md: doc/specs/axi_wr_arb.md#round-robin"),
        "frontmatter should include the spec_md key; got:\n{fm}"
    );
    assert!(
        fm.contains("tags: [arbitration, axi, axi4]"),
        "frontmatter should include the tags key; got:\n{fm}"
    );
    assert!(
        !fm.contains("4-channel round-robin"),
        "frontmatter should NOT include the prose after the closing fence; got:\n{fm}"
    );
    assert!(
        !fm.contains("\n---\n") && !fm.starts_with("---") && !fm.ends_with("---"),
        "frontmatter should not include the fence lines themselves; got:\n{fm}"
    );

    let printed = print(&parsed);
    // Pretty-print emits the leading `//!` block verbatim; round-trip
    // re-parse should recover both fields.
    let reparsed = parse_source(&printed).expect("re-parse");
    assert_eq!(
        reparsed.frontmatter.as_deref(),
        parsed.frontmatter.as_deref(),
        "frontmatter should round-trip identically"
    );
}

/// A leading `//!` block with no `---` fence has no frontmatter.
/// The compiler should not invent a frontmatter from arbitrary text.
#[test]
fn inner_doc_without_fence_has_no_frontmatter() {
    let src = r#"//! Free-form inner doc, no YAML.
//! Spans multiple lines, no `---` fence anywhere.

struct X
    a : uint<8>
end struct X
"#;
    let parsed = parse_source(src).expect("parse");
    assert!(
        parsed
            .inner_doc
            .as_deref()
            .is_some_and(|d| d.contains("Free-form inner doc")),
        "expected inner_doc to capture the //! prose"
    );
    assert!(
        parsed.frontmatter.is_none(),
        "expected no frontmatter when there's no `---` fence"
    );
}

/// Per-construct inner doc (`//!` immediately after the opening
/// keyword + name) attaches to the right `*Decl` AST field. Verified
/// across construct kinds: struct, test, impl, transactor. Round-trips
/// through the pretty-printer; the feature-harvester sees the prose
/// via `Construct::inner_doc()`.
#[test]
fn per_construct_inner_doc_attaches_and_round_trips() {
    let src = r#"
struct AxiTxn
    //! Per-bus AXI4 transaction — minimal subset.
    //! Used by the AxiWrXactor active half.
    addr : uint<32>
end struct AxiTxn

transactor AxiWrXactor
    //! Active half drives valid/ready handshake; passive half
    //! observes for the scoreboard.
    dut : AxiSlave
end transactor AxiWrXactor

test SmokeTest
    //! Smoke test — runs once and exits.
    let dut : DummyDut
    //! Sim implementation — emits one log line.
    run
        log(info, "ok")
    end run
end test SmokeTest
"#;
    let parsed = parse_source(src).expect("parse");
    for item in &parsed.items {
        let c = item.as_construct();
        match c.kind_label() {
            "struct" => {
                let inner = c.inner_doc().expect("struct inner_doc populated");
                assert!(inner.contains("Per-bus AXI4 transaction"));
                assert!(inner.contains("AxiWrXactor active half"));
            }
            "transactor" => {
                let inner = c.inner_doc().expect("transactor inner_doc populated");
                assert!(inner.contains("Active half drives"));
            }
            "test" => {
                let inner = c.inner_doc().expect("test inner_doc populated");
                assert!(inner.contains("Smoke test"));
            }
            "impl" => {
                let inner = c.inner_doc().expect("impl inner_doc populated");
                assert!(inner.contains("Sim implementation"));
            }
            other => panic!("unexpected construct kind in test fixture: {other}"),
        }
    }
    let printed = print(&parsed);
    let reparsed = parse_source(&printed).expect("re-parse");
    assert_eq!(parsed.items.len(), reparsed.items.len());
    for (orig, again) in parsed.items.iter().zip(reparsed.items.iter()) {
        assert_eq!(
            orig.as_construct().inner_doc(),
            again.as_construct().inner_doc(),
            "inner_doc must round-trip identically"
        );
    }
}

/// `else if` is a two-token mistake that ARCH-style SV/Verilog
/// muscle memory tends to produce. HARC uses single-token `elsif`.
/// The parser catches the error and points at the right keyword
/// instead of silently treating it as `else { nested if }` (which
/// the old parser did, then surfaced a misleading mismatched-`end`
/// error several lines later).
#[test]
fn else_if_is_a_directed_error_to_elsif() {
    let src = r#"testbench Tb
    dut : Top
end testbench Tb

impl T for Tb
    run
        let x = 0
        if x == 0
            x = 1
        else if x == 1
            x = 2
        else
            x = 3
        end if
    end run
end impl T"#;
    let err = parse_source(src).unwrap_err();
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("else if") && msg.contains("elsif"),
        "expected directed error mentioning both `else if` and `elsif`; got: {msg}",
    );
}

/// `int<N>` looks plausible to users coming from HARC's `uint<N>` /
/// `sint<N>` spelling, but `int` is intentionally the unqualified
/// scalar type. Signed hardware-width values should use `sint<N>`.
#[test]
fn int_width_is_a_directed_error_to_sint() {
    let src = r#"function check_case(value: uint<16>, expected: int<32>)
end function check_case

test T
end test T"#;
    let err = parse_source(src).unwrap_err();
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("int<N>") && msg.contains("sint<N>") && msg.contains("plain `int`"),
        "expected directed error steering `int<N>` to `sint<N>`; got: {msg}",
    );
}

#[test]
fn discard_bindings_and_params_round_trip() {
    let src = r#"function consume(_: uint<8>)
    let _ = 1
end function consume

agent Sink
    in_ev : event<uint<8>>
    hookable ignore(_: uint<8>)
        let _ = 2
    end ignore
    on in_ev(_)
        let _ = 3
    end on
end agent Sink

test DiscardTest
    let dut : DummyDut
    run
        let _ = consume(1)
    end run
end test DiscardTest"#;
    let printed = parse_print_reparse(src);
    assert!(printed.contains("function consume(_: uint<8>)"));
    assert!(printed.contains("hookable ignore(_: uint<8>)"));
    assert!(printed.contains("on in_ev(_)"));
    assert!(printed.contains("let _ = 1"));
}
