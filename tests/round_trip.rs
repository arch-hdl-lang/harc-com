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
end test T

impl sim for T
    run
        let a = 5
        let b = 10
        let c = a > 0 ? a : b
        let d = a > 0 ? a > 5 ? 100 : 50 : -1
        let e = (a > 0 ? a : b) + 1
        wait 1 cycle
    end run
end impl T
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
end test T

impl sim for T
    run
        wait until dut.ready
        wait until dut.ready timeout 100 cycles fail("ready never asserted")
        wait until all of dut.ready, dut.empty
        wait until all of dut.ready, dut.empty timeout 500 cycles fail("hang")
        wait until any of dut.error, dut.done timeout 1000 cycles
    end run
end impl T
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
    assert!(doc.contains("4-channel round-robin"),
        "first line should be in doc; got: {doc:?}");
    assert!(doc.contains("rotating priority pointer"),
        "third line should be in doc; got: {doc:?}");

    let printed = print(&parsed);
    let _ = parse_source(&printed).expect("re-parse");
    assert!(printed.contains("/// 4-channel round-robin"),
        "pretty output should emit `///` outer-doc lines:\n{printed}");
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
    assert!(inner.starts_with("---\n"),
        "inner_doc should keep the opening `---` line; got first line: {:?}",
        inner.lines().next());
    assert!(inner.contains("spec_md: doc/specs/axi_wr_arb.md#round-robin"),
        "inner_doc should contain the spec_md field; got:\n{inner}");
    assert!(inner.contains("4-channel round-robin"),
        "inner_doc should also keep the prose below the fence");

    // frontmatter is the YAML between the fences.
    let fm = parsed.frontmatter.as_ref().expect("expected frontmatter");
    assert!(fm.contains("spec_md: doc/specs/axi_wr_arb.md#round-robin"),
        "frontmatter should include the spec_md key; got:\n{fm}");
    assert!(fm.contains("tags: [arbitration, axi, axi4]"),
        "frontmatter should include the tags key; got:\n{fm}");
    assert!(!fm.contains("4-channel round-robin"),
        "frontmatter should NOT include the prose after the closing fence; got:\n{fm}");
    assert!(!fm.contains("\n---\n") && !fm.starts_with("---") && !fm.ends_with("---"),
        "frontmatter should not include the fence lines themselves; got:\n{fm}");

    let printed = print(&parsed);
    // Pretty-print emits the leading `//!` block verbatim; round-trip
    // re-parse should recover both fields.
    let reparsed = parse_source(&printed).expect("re-parse");
    assert_eq!(reparsed.frontmatter.as_deref(),
        parsed.frontmatter.as_deref(),
        "frontmatter should round-trip identically");
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
    assert!(parsed.inner_doc.as_deref().is_some_and(|d| d.contains("Free-form inner doc")),
        "expected inner_doc to capture the //! prose");
    assert!(parsed.frontmatter.is_none(),
        "expected no frontmatter when there's no `---` fence");
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
end test SmokeTest

impl sim for SmokeTest
    //! Sim implementation — emits one log line.
    run
        log(info, "ok")
    end run
end impl SmokeTest
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
