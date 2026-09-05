use harc::codegen::common_artifacts::{self, ArtifactRole};
use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

fn set_dut_interface(
    opts: &mut cpp_tb::EmitOpts,
    dut_type: &str,
    ports: Vec<ir::passes::dut_access::DutInterfacePort>,
) {
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(dut_type, ports)
            .expect("test DUT interface catalog"),
    );
}

fn set_clock_interface_for_program(program: &ir::TbProgram, opts: &mut cpp_tb::EmitOpts) {
    let test = program.tests.first().expect("clock fixture has a test");
    let dut_type = program.testbench(test.testbench).dut_type.clone();
    let mut clock_names = std::collections::BTreeSet::new();
    for test in &program.tests {
        for clock in &test.clocks {
            clock_names.insert(clock.name.clone());
        }
    }
    set_dut_interface(
        opts,
        &dut_type,
        clock_names
            .into_iter()
            .map(|name| {
                ir::passes::dut_access::DutInterfacePort::new_typed(
                    name,
                    ir::PortDirection::In,
                    1,
                    ir::IrType::UInt(Some(1)),
                    None,
                    None,
                )
            })
            .collect(),
    );
}

fn set_common_reg_interface(opts: &mut cpp_tb::EmitOpts) {
    set_dut_interface(
        opts,
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    );
}

fn component_solver_site_tags(cpp: &str) -> std::collections::BTreeSet<&str> {
    cpp.match_indices("_solver_site_h")
        .map(|(start, _)| {
            let tail = &cpp[start..];
            let end_marker = tail.find("_e").expect("site tag has an end-span marker") + 2;
            let digit_count = tail[end_marker..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            &tail[..end_marker + digit_count]
        })
        .collect()
}

fn mutable_namespace_state_declarations(cpp: &str) -> Vec<String> {
    #[derive(Clone)]
    struct Token {
        text: String,
        line: usize,
    }

    fn tokens(cpp: &str) -> Vec<Token> {
        let chars = cpp.chars().collect::<Vec<_>>();
        let mut out = Vec::new();
        let mut index = 0;
        let mut line = 1;
        while index < chars.len() {
            match chars[index] {
                '\n' => {
                    line += 1;
                    index += 1;
                }
                c if c.is_whitespace() => index += 1,
                '/' if chars.get(index + 1) == Some(&'/') => {
                    index += 2;
                    while index < chars.len() && chars[index] != '\n' {
                        index += 1;
                    }
                }
                '/' if chars.get(index + 1) == Some(&'*') => {
                    index += 2;
                    while index + 1 < chars.len()
                        && !(chars[index] == '*' && chars[index + 1] == '/')
                    {
                        if chars[index] == '\n' {
                            line += 1;
                        }
                        index += 1;
                    }
                    index = (index + 2).min(chars.len());
                }
                quote @ ('"' | '\'') => {
                    let start_line = line;
                    index += 1;
                    while index < chars.len() {
                        if chars[index] == '\\' {
                            index = (index + 2).min(chars.len());
                            continue;
                        }
                        if chars[index] == quote {
                            index += 1;
                            break;
                        }
                        if chars[index] == '\n' {
                            line += 1;
                        }
                        index += 1;
                    }
                    out.push(Token {
                        text: "<literal>".to_string(),
                        line: start_line,
                    });
                }
                c if c == '_' || c.is_ascii_alphabetic() => {
                    let start = index;
                    index += 1;
                    while index < chars.len()
                        && (chars[index] == '_' || chars[index].is_ascii_alphanumeric())
                    {
                        index += 1;
                    }
                    out.push(Token {
                        text: chars[start..index].iter().collect(),
                        line,
                    });
                }
                c if c.is_ascii_digit() => {
                    let start = index;
                    index += 1;
                    while index < chars.len()
                        && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
                    {
                        index += 1;
                    }
                    out.push(Token {
                        text: chars[start..index].iter().collect(),
                        line,
                    });
                }
                c => {
                    out.push(Token {
                        text: c.to_string(),
                        line,
                    });
                    index += 1;
                }
            }
        }
        out
    }

    fn identifier(token: &str) -> bool {
        token
            .chars()
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && token.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    }

    fn matching_paren(tokens: &[Token], open: usize) -> Option<usize> {
        let mut depth = 0;
        for (index, token) in tokens.iter().enumerate().skip(open) {
            match token.text.as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn is_function_declaration(declaration: &[Token], static_index: usize) -> bool {
        for open in static_index + 1..declaration.len() {
            if declaration[open].text != "(" || open == 0 {
                continue;
            }
            let name = &declaration[open - 1].text;
            if !identifier(name)
                || declaration.get(open + 1).is_some_and(|token| {
                    token.text == "*"
                        || token.text == "&"
                        || token.text == "<literal>"
                        || token
                            .text
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_digit())
                })
                || declaration[static_index..open]
                    .iter()
                    .any(|token| token.text == "=")
            {
                continue;
            }
            let Some(close) = matching_paren(declaration, open) else {
                continue;
            };
            if declaration[open + 1..close]
                .windows(2)
                .any(|pair| pair[0].text == "(" && pair[1].text == "*")
            {
                continue;
            }
            let tail = declaration.get(close + 1).map(|token| token.text.as_str());
            if matches!(
                tail,
                Some(";") | Some("{") | Some("const") | Some("noexcept") | Some("->") | Some("[")
            ) {
                return true;
            }
        }
        false
    }

    fn is_immutable_object(declaration: &[Token], static_index: usize) -> bool {
        if declaration[static_index + 1..]
            .iter()
            .any(|token| token.text == "constexpr" || token.text == "consteval")
        {
            return true;
        }
        let end = declaration
            .iter()
            .position(|token| matches!(token.text.as_str(), "=" | ";" | "{"))
            .unwrap_or(declaration.len());
        let prefix = &declaration[static_index + 1..end];
        let Some(last_const) = prefix.iter().rposition(|token| token.text == "const") else {
            return false;
        };
        match prefix.iter().rposition(|token| token.text == "*") {
            Some(last_pointer) => last_const > last_pointer,
            None => !prefix.iter().any(|token| token.text == "&"),
        }
    }

    let tokens = tokens(cpp);
    let mut findings = Vec::new();
    let mut seen_declarations = std::collections::BTreeSet::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.text != "static" && token.text != "thread_local" {
            continue;
        }
        let start = (0..index)
            .rev()
            .find(|candidate| matches!(tokens[*candidate].text.as_str(), ";" | "{" | "}"))
            .map_or(0, |candidate| candidate + 1);
        let end = (index..tokens.len())
            .find(|candidate| matches!(tokens[*candidate].text.as_str(), ";" | "{"))
            .map_or(tokens.len(), |candidate| candidate + 1);
        if !seen_declarations.insert((start, end)) {
            continue;
        }
        let declaration = &tokens[start..end];
        let local_index = index - start;
        if token.text == "static"
            && (is_function_declaration(declaration, local_index)
                || is_immutable_object(declaration, local_index))
        {
            continue;
        }
        let source = declaration
            .iter()
            .map(|token| token.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        findings.push(format!("{}: {source}", token.line));
    }
    findings
}

#[test]
fn mutable_namespace_state_scanner_classifies_qualified_declarations() {
    let cpp = r#"
static const uint64_t immutable_scalar = 1;
static const char* const immutable_pointer = "anchor";
inline static constexpr unsigned immutable_inline = 2;
static void helper();
[[maybe_unused]] static int function(int value) { return value; }
inline static int counter;
[[maybe_unused]] static CustomState state{};
static const char* mutable_pointer = "mutable";
static void (*callback)();
inline static thread_local CustomState tls_state;
"#;
    let findings = mutable_namespace_state_declarations(cpp);
    assert_eq!(findings.len(), 5, "{findings:#?}");
    for expected in [
        "counter",
        "state",
        "mutable_pointer",
        "callback",
        "tls_state",
    ] {
        assert!(
            findings.iter().any(|finding| finding.contains(expected)),
            "missing `{expected}` in {findings:#?}"
        );
    }
}

const MINIMAL_COMMON_SRC: &str = r#"
test Common17
    let dut : CommonReg
    clock clk = 10ns
    run
        dut.d = 17
        wait 1 cycle
        assert dut.q == 17 else fail("Common17 observed the wrong value")
        log(info, "COMMON_RESULT=17")
    end run
end test Common17

test Common203
    let dut : CommonReg
    clock clk = 10ns
    run
        dut.d = 203
        wait 1 cycle
        assert dut.q == 203 else fail("Common203 observed the wrong value")
        log(info, "COMMON_RESULT=203")
    end run
end test Common203
"#;

const STATEMENT_RUNTIME_CELLS_SRC: &str = r#"
property transfer
    dut.d != 0 |=> dut.q == dut.d
end property transfer

test RuntimeCells
    let dut : CommonReg
    clock clk = 10ns
    run
        let local_events : event<uint<8>>
        on local_events(value)
            log(info, "LOCAL=${value}")
        end on
        emit local_events(3)
        assert property transfer
        assert rose(dut.q)
        on dut.q != 0
            log(info, "EDGE")
        end on
        on 2 cycles phase post_eval
            log(info, "PERIODIC")
        end on
        dut.d = 7
        wait 3 cycles
    end run
end test RuntimeCells
"#;

const SHARED_TYPES_AND_CALLABLES_SRC: &str = r#"
domain CommonDomain
  freq_mhz: 100
end domain CommonDomain

struct InnerValue
    wide : uint<130>
    lanes : Vec<uint<8>, 2>
end struct InnerValue

struct OuterValue
    tag : uint<8> default 3
    inner : InnerValue
    history : list<uint<16>>
end struct OuterValue

scoreboard SharedScoreboard
    pending : queue<InnerValue>
    writes : uint<16> default 2
end scoreboard SharedScoreboard

function plus_one(x: uint<8>) -> uint<8>
    return x + 1
end function plus_one

function widen_plus_one(x: uint<8>) -> uint<130>
    return plus_one(x).zext<130>()
end function widen_plus_one

function wide_identity(value: uint<130>) -> uint<130>
    return value
end function wide_identity

// Both wrappers intentionally precede their callees. The shared and
// self-contained emitters must use the same dependency order.
tseq ForwardPure(seed: uint<8>) -> TSeq<uint<16>>
    let values = PureValues(seed)
    for value in values
        yield value
    end for
end tseq ForwardPure

tseq ForwardTimed(seed: uint<8>) -> TSeq<uint<16>>
    let values = TimedValues(seed)
    for value in values
        yield value
    end for
end tseq ForwardTimed

tseq CopyScalarValues(values: TSeq<uint<32>>) -> TSeq<uint<32>>
    for value in values
        yield value
    end for
end tseq CopyScalarValues

tseq CopyRecordValues(values: TSeq<InnerValue>) -> TSeq<InnerValue>
    for value in values
        yield value
    end for
end tseq CopyRecordValues

tseq EchoRecord(value: InnerValue) -> TSeq<InnerValue>
    yield value
end tseq EchoRecord

tseq PureValues(seed: uint<8>) -> TSeq<uint<16>>
    yield plus_one(seed).zext<16>()
end tseq PureValues

tseq RecordValues(seed: uint<8>) -> TSeq<InnerValue>
    let value : InnerValue
    value.wide = seed.zext<130>()
    yield value
end tseq RecordValues

tseq TimedValues(seed: uint<8>) -> TSeq<uint<16>>
    wait 1 cycle
    yield seed.zext<16>()
end tseq TimedValues

testbench SharedTypesTb
    dut : CommonReg
    sb : SharedScoreboard
    last : InnerValue
    count : uint<16> default 7
    pending_values : queue<uint<16>>
end testbench SharedTypesTb

impl SharedTypesA for SharedTypesTb
    clock clk = CommonDomain
    run
        let item : InnerValue
        item.wide = widen_plus_one(4)
        let direct_wide = 5.zext<130>()
        item.lanes[0] = 9
        item.lanes[1] = 11
        sb.pending.push(item)
        sb.writes = sb.writes + 1
        _tb.last = sb.pending.pop()
        let raw_record_values = RecordValues(5)
        let record_values = CopyRecordValues(raw_record_values)
        for record_value in record_values
            assert record_value.wide == 5 else fail("record tseq value")
        end for
        let echoed_values = EchoRecord(item)
        for echoed_value in echoed_values
            assert echoed_value.wide == 5 else fail("record parameter value")
        end for
        count = count + 1
        pending_values.push(13)
        let queued = pending_values.pop()
        let raw_values = ForwardPure(5)
        let copied_values = CopyScalarValues(raw_values)
        let before = cycle_count
        let ys = ForwardTimed(6)
        let pure_value = 0
        for value in copied_values
            pure_value = value
        end for
        let timed_value = 0
        for value in ys
            timed_value = value
        end for
        assert pure_value == 6 else fail("pure tseq result")
        assert timed_value == 6 else fail("stateful tseq result")
        assert cycle_count == before + 1 else fail("stateful tseq did not advance")
        assert wide_identity(last.wide) == 5 else fail("wide helper/record value")
        assert direct_wide == last.wide else fail("wide local value")
        assert queued == 13 else fail("testbench queue value")
    end run
    check
        assert sb.writes == 3 else fail("scoreboard state did not persist")
        assert count == 8 else fail("testbench scalar state did not persist")
    end check
end impl SharedTypesA

impl SharedTypesB for SharedTypesTb
    clock clk = CommonDomain
    run
        let values = PureValues(9)
        let observed = 0
        for value in values
            observed = value
        end for
        assert observed == 10 else fail("second capsule helper/tseq")
    end run
end impl SharedTypesB
"#;

const STRUCTURAL_SHARED_TYPES_SRC: &str = r#"
transaction Payload
    value : uint<16> default 9
end transaction Payload

scoreboard DataBoard
    last : Payload
    pending : queue<Payload>
end scoreboard DataBoard

agent LeafState
    wide : uint<130>
    lanes : Vec<Vec<uint<8>, 2>, 3>
    last : Payload
    pending : queue<Payload>
    board : DataBoard
end agent LeafState

env ParentState
    leaf : LeafState
end env ParentState

transactor StatefulTarget
    dut : CommonReg
    count : uint<16> default 4
    last : Payload
    pending : queue<Payload>
end transactor StatefulTarget

covergroup StateCov @(posedge dut.clk)
    cp_data : cover dut.d
        bins
            zero = {0}
            nonzero = [1..255]
        end bins
end covergroup StateCov

test StructuralTypes
    let dut : CommonReg
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test StructuralTypes
"#;

const COMMON_COMPONENT_METHODS_SRC: &str = r#"
agent LeftCounter
    value : uint<16> default 1
    function bump(delta: uint<16>) -> uint<16>
        value = value + delta
        return value
    end function bump
end agent LeftCounter

agent RightCounter
    value : uint<16> default 10
    function bump(delta: uint<16>) -> uint<16>
        value = value + delta
        return value
    end function bump
end agent RightCounter

env CounterPair
    left : LeftCounter
    right : RightCounter
    function bump_copy(model: LeftCounter, delta: uint<16>) -> uint<16>
        return model.bump(delta)
    end function bump_copy
    function sum_after(delta: uint<16>) -> uint<16>
        let left_before = left.value
        let copied = bump_copy(left, delta)
        assert copied == left_before + delta else fail("component parameter value")
        assert left.value == left_before else fail("component parameter aliased receiver")
        let left_value = left.bump(delta)
        let right_value = right.bump(delta)
        return left_value + right_value
    end function sum_after
end env CounterPair

testbench CounterTb
    dut : CommonReg
    counters : CounterPair
end testbench CounterTb

impl CounterA for CounterTb
    clock clk = 10ns
    run
        let result = counters.sum_after(2)
        assert result == 15 else fail("CounterA result")
    end run
end impl CounterA

impl CounterB for CounterTb
    clock clk = 10ns
    run
        let result = counters.sum_after(3)
        assert result == 17 else fail("CounterB result")
    end run
end impl CounterB
"#;

const COMMON_TESTBENCH_METHODS_SRC: &str = r#"
transaction Beat
    value : uint<16> default 0
end transaction Beat

function combine(first: uint<16>, second: uint<16>) -> uint<32>
    return first * 1000 + second
end function combine

testbench MethodTb
    dut : CommonReg
    count : uint<16> default 0
    saved : Beat

    function later(delta: uint<16>) -> uint<16>
        count = count + delta
        return count
    end function later

    function ordered() -> uint<32>
        return combine(count, later(2))
    end function ordered

    function mirror(beat: Beat) -> Beat
        return beat
    end function mirror

    function save(beat: Beat) -> uint<16>
        saved = beat
        saved.value = saved.value + 1
        return saved.value
    end function save

    function lazy_take(enable: bool) -> bool
        return enable && later(3) != 0
    end function lazy_take
end testbench MethodTb

impl MethodA for MethodTb
    clock clk = 10ns
    run
        count = 1
        let ordered_value = ordered()
        assert ordered_value == 1003 else fail("ordered=${ordered_value}")
        let skipped = lazy_take(false)
        assert !skipped && count == 3 else fail("lazy false count=${count}")
        let beat : Beat
        beat.value = 9
        let copied = mirror(beat)
        assert copied.value == 9 else fail("record=${copied.value}")
        let saved_value = save(beat)
        assert saved_value == 10 else fail("saved=${saved_value}")
    end run
end impl MethodA

impl MethodB for MethodTb
    clock clk = 10ns
    run
        count = 4
        let taken = lazy_take(true)
        assert taken && count == 7 else fail("lazy true count=${count}")
        let beat : Beat
        beat.value = 20
        let saved_value = save(beat)
        assert saved_value == 21 else fail("saved=${saved_value}")
    end run
end impl MethodB
"#;

const BOUND_BUS_PLACEMENT_SRC: &str = r#"
bus TinyBus
    handshake_channel req: send kind: valid_ready
        data: uint<8>
    end handshake_channel req
    tlm_method read(addr: uint<8>) -> uint<8>: blocking;
end bus TinyBus

transactor TinyDriver bound to TinyBus
    calls : uint<8> default 0
    when active
        function drive_raw(value: uint<8>)
            calls = calls + 1
            bus.req.data = value
            bus.req.valid = 1
            wait 1 cycle
            bus.req.valid = 0
        end drive_raw

        hookable drive(value: uint<8>)
            drive_raw(value)
        end drive
    end when
end transactor TinyDriver

testbench BusTbA
    dut : CommonReg
end testbench BusTbA

impl BoundBusA for BusTbA
    let first : TinyBus = bind dut with {
        req.data: "first_data", req.valid: "first_valid", req.ready: "first_ready"
    }
    let driver_a : TinyDriver active = bind first
    clock clk = 10ns
    run
        dut.bus_status = 1
        first.req.valid = 0
        driver_a.drive(3)
    end run
end impl BoundBusA

testbench BusTbB
    dut : CommonReg
end testbench BusTbB

impl BoundBusB for BusTbB
    let second : TinyBus = bind dut with {
        req.data: "second_data", req.valid: "second_valid", req.ready: "second_ready"
    }
    let driver_b : TinyDriver active = bind second
    clock clk = 10ns
    run
        dut.bus_status = 2
        second.req.valid = 0
        driver_b.drive(7)
    end run
end impl BoundBusB
"#;

const TEST_HOOK_IDENTITY_SRC: &str = r#"
agent HookIdentityCell
    hookable ping(value: uint<8>)
        log(info, "PING=${value}")
    end ping
end agent HookIdentityCell

testbench HookIdentityTb
    dut : CommonReg
    cell : HookIdentityCell

    on 1 cycles
        log(info, "TB_PERIODIC_A")
    end on
    on 2 cycles
        log(info, "TB_PERIODIC_B")
    end on
    on dut.q == 1
        log(info, "TB_CYCLE_A")
    end on
    on dut.q == 2
        log(info, "TB_CYCLE_B")
    end on
end testbench HookIdentityTb

impl HookIdentity for HookIdentityTb
    clock clk = 10ns
    run
        let first : event<uint<8>>
        let second : event<uint<8>>
        on first(value)
            log(info, "EVENT_A=${value}")
        end on
        on second(value)
            log(info, "EVENT_B=${value}")
        end on
        on cell.ping pre
            log(info, "METHOD_A=${value}")
        end on
        on cell.ping pre
            log(info, "METHOD_B=${value}")
        end on
        on 1 cycles
            log(info, "STMT_A")
        end on
        on 2 cycles
            log(info, "STMT_B")
        end on
        wait 1 cycle
    end run
end impl HookIdentity
"#;

const COMMON_PROBE_ACCESS_SRC: &str = r#"
agent ProbeAccess
    function sample() -> uint<8>
        return dut.status
    end function sample

    function drive(value: uint<8>)
        dut.status = value
    end function drive

    function release_drive()
        release dut.status
    end function release_drive

    function sample_wide() -> uint<16>
        return dut.status.zext<16>()
    end function sample_wide

    function drive_narrow(value: uint<16>)
        dut.status = value.trunc<8>()
    end function drive_narrow
end agent ProbeAccess

testbench ProbeAccessTb
    let dut : ProbeCollisionTop
        probe force status : uint<8> at core.status
    end let dut
    access : ProbeAccess
end testbench ProbeAccessTb

impl ProbeAccessA for ProbeAccessTb
    clock clk = 10ns
    run
        let before : uint<8> = access.sample()
        access.drive(before + 1)
        access.release_drive()
    end run
end impl ProbeAccessA

impl ProbeAccessB for ProbeAccessTb
    clock clk = 10ns
    run
        let before : uint<8> = access.sample()
        access.drive(before + 2)
        access.release_drive()
    end run
end impl ProbeAccessB
"#;

fn shared_program() -> (harc::ir::TbProgram, cpp_tb::EmitOpts) {
    let source = parse_source(SHARED_TYPES_AND_CALLABLES_SRC).expect("shared source parses");
    let program = lower::lower_program(&source).expect("shared source lowers");
    verify::verify_program(&program).expect("shared program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_dut_interface(
        &mut opts,
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    );
    (program, opts)
}

fn tseq_call_mut<'a>(
    program: &'a mut harc::ir::TbProgram,
    name: &str,
) -> (&'a mut harc::ir::Expr, harc::ir::LocalId) {
    for function in &mut program.functions {
        for block in &mut function.blocks {
            for stmt in &mut block.stmts {
                if let harc::ir::Stmt::Assign(dest, expr) = stmt {
                    if matches!(
                        expr,
                        harc::ir::Expr::Call(
                            harc::ir::CallTarget::Tseq { name: target, .. },
                            _,
                        )
                            if target == name
                    ) {
                        return (expr, *dest);
                    }
                }
            }
        }
    }
    panic!("missing `{name}` tseq call")
}

fn minimal_program() -> (harc::ir::TbProgram, cpp_tb::EmitOpts) {
    let source = parse_source(MINIMAL_COMMON_SRC).expect("minimal common source parses");
    let program = lower::lower_program(&source).expect("minimal common source lowers");
    verify::verify_program(&program).expect("minimal common program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_dut_interface(
        &mut opts,
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    );
    (program, opts)
}

fn common_probe_program() -> (harc::ir::TbProgram, cpp_tb::EmitOpts) {
    let source = parse_source(COMMON_PROBE_ACCESS_SRC).expect("common probe source parses");
    let program = lower::lower_program(&source).expect("common probe source lowers");
    verify::verify_program(&program).expect("common probe program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1), ("status".to_string(), 8)]);
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "ProbeCollisionTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "status",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("probe DUT interface is valid"),
    );
    (program, opts)
}

fn first_probe_write(program: &mut ir::TbProgram) -> &mut ir::PortRef {
    program
        .functions
        .iter_mut()
        .flat_map(|function| &mut function.blocks)
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::DutWrite(port, _) if port.probe.is_some() => Some(port),
            _ => None,
        })
        .expect("fixture has a probe write")
}

#[test]
fn common_shared_callable_uses_the_verified_probe_identity_despite_port_name_collision() {
    let (program, opts) = common_probe_program();
    let before = format!("{program}");
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("ticket07 admits the verified suite probe contract");
    let interface = tbir::common::emit_common_interface(&plan).expect("interface emits");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    let accessor = "ProbeCollisionTop__DOT__harc_probes__DOT__status";
    let stub_artifact = plan
        .artifact_plan()
        .probe_stub()
        .expect("probe stub is an owned common artifact");
    let stub = harc::codegen::sv_stub::emit_stub_from_plan(plan.dut_access())
        .expect("stub renders from the immutable DUT access plan");

    assert_eq!(stub_artifact.filename(), "suite__probe_stub.sv");
    assert!(
        stub.contains("assign status = ProbeCollisionTop.core.status;"),
        "{stub}"
    );
    assert!(
        stub.contains("force ProbeCollisionTop.core.status = status_drv;"),
        "{stub}"
    );
    assert!(
        !interface.contains("___024root.h"),
        "the declarative interface must not pull in a probe implementation header:\n{interface}"
    );
    assert!(
        runtime.contains("#include \"VProbeCollisionTop___024root.h\""),
        "the common unit that dereferences a probe must include its root definition:\n{runtime}"
    );
    assert!(runtime.contains(accessor), "{runtime}");
    assert!(runtime.contains(&format!("{accessor}_drv")), "{runtime}");
    assert!(runtime.contains(&format!("{accessor}_en")), "{runtime}");
    assert!(
        !runtime.contains("harc_read(dut->status)"),
        "the same-named top-level port must not replace the verified probe:\n{runtime}"
    );
    assert_eq!(
        format!("{program}"),
        before,
        "DUT access planning mutated IR"
    );
}

#[test]
fn common_plan_closes_immutable_dut_access_profiles_per_artifact() {
    let (mut program, opts) = common_probe_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "profile__")
        .expect("DUT access profiles plan");
    let mut other_opts = opts.clone();
    other_opts.build_profile_inputs.extend([
        "param:00000000:WIDTH=17".to_string(),
        "cxx=/opt/toolchain/c++".to_string(),
        "z3_inc=/opt/z3/include".to_string(),
        "z3_lib=/opt/z3/lib".to_string(),
    ]);
    let other_plan = tbir::common::plan_common_tests(&program, &other_opts, "profile__")
        .expect("alternate build profile plans");
    assert_ne!(plan.build_profile(), other_plan.build_profile());
    other_opts.build_profile_inputs.reverse();
    let reordered_plan = tbir::common::plan_common_tests(&program, &other_opts, "profile__")
        .expect("reordered build-profile inputs plan");
    assert_eq!(other_plan.build_profile(), reordered_plan.build_profile());
    let runtime_profile = plan.runtime_dut_access();
    assert!(runtime_profile
        .sites()
        .iter()
        .any(|site| matches!(site, ir::passes::dut_access::DutAccessSite::Clock(_))));
    assert!(!runtime_profile.functions().is_empty());
    assert!(runtime_profile.uses_probe());
    let runtime = tbir::common::emit_common_runtime(&plan).expect("profile-owned runtime emits");
    assert!(runtime.contains("// harc build-profile: "), "{runtime}");
    assert!(
        plan.publication()
            .expect("publication plans")
            .registry()
            .contains(&format!("// harc build-profile: {}", plan.build_profile())),
        "the suite-global build identity must remain owned by the registry and manifest"
    );
    program.functions.clear();
    assert_eq!(
        tbir::common::emit_common_runtime(&plan)
            .expect("caller-side IR mutation cannot affect the frozen plan"),
        runtime
    );
    assert!(
        runtime.contains(&format!(
            "// harc dut-access-profile: {}",
            runtime_profile.digest()
        )),
        "{runtime}"
    );
    for capsule in plan.capsules() {
        let emitted = tbir::common::emit_common_capsule(&plan, capsule.index())
            .expect("profile-owned capsule emits");
        assert!(
            emitted.contains(&format!(
                "// harc dut-access-profile: {}",
                capsule.dut_access().digest()
            )),
            "{emitted}"
        );
        assert_eq!(
            emitted.contains("___024root.h"),
            capsule.dut_access().uses_probe(),
            "root-header decision must come from the closed capsule profile"
        );
    }
}

#[test]
fn dut_access_plan_rejects_corrupted_probe_identity_metadata_before_rendering() {
    let (program, opts) = common_probe_program();
    let interface = opts.dut_interface.as_ref().expect("resolved DUT interface");

    let mut stale_id = program.clone();
    first_probe_write(&mut stale_id).probe = Some(ir::ProbeId(99));
    let error = ir::passes::dut_access::analyze(&stale_id, interface)
        .expect_err("a stale ProbeId must fail access planning");
    assert!(error.0.contains("missing probe p99"), "{error}");

    let mut wrong_path = program.clone();
    first_probe_write(&mut wrong_path).port_path = vec!["other_status".to_string()];
    let error = ir::passes::dut_access::analyze(&wrong_path, interface)
        .expect_err("a probe path drift must fail access planning");
    assert!(
        error.0.contains("does not match probe p0 `status`"),
        "{error}"
    );

    let mut wrong_width = program.clone();
    first_probe_write(&mut wrong_width).width = Some(7);
    let error = ir::passes::dut_access::analyze(&wrong_width, interface)
        .expect_err("a probe width drift must fail access planning");
    assert!(
        error.0.contains("does not match probe p0 `status`"),
        "{error}"
    );

    let mut duplicate_name = program.clone();
    let mut duplicate = duplicate_name.probes[0].clone();
    duplicate.id = ir::ProbeId(1);
    duplicate.shared = false;
    duplicate_name.probes.push(duplicate);
    let error = ir::passes::dut_access::analyze(&duplicate_name, interface)
        .expect_err("duplicate accessor names must fail access planning");
    assert!(
        error
            .0
            .contains("probe p1 `status` conflicts with probe p0"),
        "{error}"
    );

    let mut generated_collision = program.clone();
    let mut duplicate = generated_collision.probes[0].clone();
    duplicate.id = ir::ProbeId(1);
    duplicate.name = "status_en".to_string();
    duplicate.sv_path = "core.other_status".to_string();
    duplicate.force = false;
    duplicate.shared = false;
    generated_collision.probes.push(duplicate);
    let error = ir::passes::dut_access::analyze(&generated_collision, interface)
        .expect_err("generated probe signal collisions must fail access planning");
    assert!(error.0.contains("generated signal `status_en`"), "{error}");

    let mut overlapping_force = program.clone();
    let mut duplicate = overlapping_force.probes[0].clone();
    duplicate.id = ir::ProbeId(1);
    duplicate.name = "nested_status".to_string();
    duplicate.sv_path = "core.status.field".to_string();
    duplicate.shared = false;
    overlapping_force.probes.push(duplicate);
    let error = ir::passes::dut_access::analyze(&overlapping_force, interface)
        .expect_err("overlapping force paths must fail access planning");
    assert!(error.0.contains("overlaps force probe p0"), "{error}");

    let mut missing_capability = program.clone();
    missing_capability.testbenches[0]
        .probes
        .push(ir::ProbeId(99));
    let error = ir::passes::dut_access::analyze(&missing_capability, interface)
        .expect_err("a missing testbench probe capability must fail access planning");
    assert!(error.0.contains("references missing probe p99"), "{error}");

    let mut wrong_dut = program.clone();
    wrong_dut.probes[0].dut_type = "OtherTop".to_string();
    let error = ir::passes::dut_access::analyze(&wrong_dut, interface)
        .expect_err("a probe for another DUT must fail access planning");
    assert!(error.0.contains("targets `OtherTop`"), "{error}");

    let mut wrong_access = program.clone();
    first_probe_write(&mut wrong_access).access = ir::PortAccess::Probe;
    let error = ir::passes::dut_access::analyze(&wrong_access, interface)
        .expect_err("a force-probe access-class drift must fail access planning");
    assert!(
        error.0.contains("does not match probe p0 `status`"),
        "{error}"
    );

    let mut erased_identity = program;
    let port = first_probe_write(&mut erased_identity);
    port.access = ir::PortAccess::Port;
    port.probe = None;
    port.aggregate_path = false;
    port.width = None;
    port.value_type = None;
    let errors = verify::verify_program(&erased_identity)
        .expect_err("a same-name top port must not erase the declared probe identity");
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("declared probe `status`")),
        "{errors:?}"
    );
}

#[test]
fn dut_access_plan_rejects_resolved_output_writes_and_interface_width_drift() {
    let (program, mut opts) = minimal_program();
    let interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("minimal DUT interface");
    let mut wrong_receiver = program.clone();
    let ordinary_write = wrong_receiver
        .functions
        .iter_mut()
        .flat_map(|function| &mut function.blocks)
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::DutWrite(port, _) if port.access == ir::PortAccess::Port => Some(port),
            _ => None,
        })
        .expect("minimal fixture writes a DUT port");
    ordinary_write.testbench_field = "shadow".to_string();
    let error = ir::passes::dut_access::analyze(&wrong_receiver, &interface)
        .expect_err("a noncanonical DUT receiver must fail access planning");
    assert!(
        error
            .0
            .contains("uses receiver `shadow` but its verified DUT receiver set is [dut]"),
        "{error}"
    );

    let mut nested_port = program.clone();
    let ordinary_write = nested_port
        .functions
        .iter_mut()
        .flat_map(|function| &mut function.blocks)
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::DutWrite(port, _) if port.access == ir::PortAccess::Port => Some(port),
            _ => None,
        })
        .expect("minimal fixture writes a DUT port");
    ordinary_write.port_path.push("field".to_string());
    ordinary_write.aggregate_path = true;
    let error = ir::passes::dut_access::analyze(&nested_port, &interface)
        .expect_err("a multi-segment direct DUT path must fail access planning");
    assert!(
        error.0.contains("is absent from the DUT interface catalog"),
        "{error}"
    );

    let output_d = ir::passes::dut_access::DutInterfaceCatalog::new(
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("valid direction-control catalog");
    let error = ir::passes::dut_access::analyze(&program, &output_d)
        .expect_err("writing an output-only DUT port must fail");
    assert!(
        error.0.contains("writes an output-only DUT port"),
        "{error}"
    );

    let output_clock = ir::passes::dut_access::DutInterfaceCatalog::new(
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::Out,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("valid output-clock catalog metadata");
    let error = ir::passes::dut_access::analyze(&program, &output_clock)
        .expect_err("a declared clock must be a writable DUT input");
    assert!(error.0.contains("IR direction Some(In)"), "{error}");

    let wide_clock = ir::passes::dut_access::DutInterfaceCatalog::new(
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                2,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("valid wide-clock catalog metadata");
    let error = ir::passes::dut_access::analyze(&program, &wide_clock)
        .expect_err("a declared clock must resolve to one bit");
    assert!(error.0.contains("IR width 1 conflicts"), "{error}");

    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "CommonReg",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "d",
                    ir::PortDirection::In,
                    65,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "q",
                    ir::PortDirection::Out,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("valid wide-port catalog"),
    );
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("wide DUT carriers are part of the verified ticket07 surface");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("wide DUT carrier emits");
    assert!(
        capsule.contains("harc_rt::harc_assign(ctx.dut->d"),
        "{capsule}"
    );
}

#[test]
fn partial_probe_cohort_is_capsule_owned_and_only_dereferencing_artifacts_include_root() {
    let source = parse_source(
        r#"test ProbeOnly
    let dut : ProbeCollisionTop
        probe status : uint<8> at core.status
    end let dut
    clock clk = 10ns
    run
        let observed : uint<8> = dut.status
        assert observed == 0 else fail("probe read")
    end run
end test ProbeOnly

test Plain
    let dut : ProbeCollisionTop
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test Plain"#,
    )
    .expect("partial-probe source parses");
    let program = lower::lower_program(&source).expect("partial-probe source lowers");
    verify::verify_program(&program).expect("partial-probe program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1), ("status".to_string(), 8)]);
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "ProbeCollisionTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "status",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("partial-probe DUT interface"),
    );
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("test-owned probe cohort plans");
    let probe = &plan.dut_access().probes()[0];
    assert!(!probe.shared());
    assert_eq!(probe.test_names(), &["ProbeOnly".to_string()]);
    assert!(plan.artifact_plan().probe_stub().is_some());

    let interface =
        tbir::common::emit_common_interface(&plan).expect("partial-probe interface emits");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    let probe_capsule = tbir::common::emit_common_capsule(&plan, 0).expect("probe capsule emits");
    let plain_capsule = tbir::common::emit_common_capsule(&plan, 1).expect("plain capsule emits");
    assert!(!interface.contains("___024root.h"), "{interface}");
    assert!(!runtime.contains("___024root.h"), "{runtime}");
    assert!(
        probe_capsule.contains("#include \"VProbeCollisionTop___024root.h\""),
        "{probe_capsule}"
    );
    assert!(!plain_capsule.contains("___024root.h"), "{plain_capsule}");
}

#[test]
fn testbench_lifecycle_probe_predicate_uses_capsule_plan_bindings() {
    let source = parse_source(
        r#"testbench ProbeServiceTb
    let dut : ProbeCollisionTop
        probe status : uint<8> at core.status
    end let dut
    hits : uint<8> default 0

    on dut.status != 0
        hits = hits + 1
    end on
end testbench ProbeServiceTb

impl ProbeService for ProbeServiceTb
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end impl ProbeService"#,
    )
    .expect("probe lifecycle source parses");
    let program = lower::lower_program(&source).expect("probe lifecycle source lowers");
    verify::verify_program(&program).expect("probe lifecycle program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1), ("status".to_string(), 8)]);
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "ProbeCollisionTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "status",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("probe lifecycle DUT interface"),
    );
    let plan =
        tbir::common::plan_common_tests(&program, &opts, "suite__").expect("probe lifecycle plans");
    let self_contained =
        tbir::emit(&program, &source, &opts).expect("self-contained probe lifecycle emits");
    let mut fallback_opts = opts.clone();
    fallback_opts.dut_interface = None;
    let fallback = tbir::emit(&program, &source, &fallback_opts)
        .expect("legacy self-contained probe lifecycle emits");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("capsule emits");
    let accessor = "ProbeCollisionTop__DOT__harc_probes__DOT__status";
    assert!(!runtime.contains("___024root.h"), "{runtime}");
    assert!(
        self_contained.contains("#include \"VProbeCollisionTop___024root.h\"")
            && self_contained.contains(&format!("dut->rootp->{accessor}")),
        "{self_contained}"
    );
    assert!(
        fallback.contains("#include \"VProbeCollisionTop___024root.h\""),
        "{fallback}"
    );
    assert!(
        capsule.contains("#include \"VProbeCollisionTop___024root.h\"")
            && capsule.contains(&format!("ctx.dut->rootp->{accessor}")),
        "{capsule}"
    );
}

#[test]
fn common_testbench_method_preserves_ordered_dut_lane_snapshots() {
    let source = parse_source(
        r#"function set_en(dut: SnapshotTop, value: uint<8>) -> uint<8>
    dut.en = value
    return value
end function set_en

testbench SnapshotTb
    dut : SnapshotTop

    function report_lane()
        fail("lane=${dut.count_out[set_en(dut, 0)]}")
    end function report_lane
end testbench SnapshotTb

impl SnapshotTest for SnapshotTb
    clock clk = 10ns
    run
        report_lane()
    end run
end impl SnapshotTest"#,
    )
    .expect("ordered snapshot source parses");
    let program = lower::lower_program(&source).expect("ordered snapshot source lowers");
    verify::verify_program(&program).expect("ordered snapshot program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("count_out".to_string(), 24),
        ("en".to_string(), 8),
    ]);
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "SnapshotTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "count_out",
                    ir::PortDirection::Out,
                    24,
                    Some(8),
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "en",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("ordered snapshot DUT interface"),
    );
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("ordered snapshot common plan");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("common runtime emits");
    let snapshot = "decltype(harc_rt::harc_port_snapshot(ctx.dut->count_out))";
    assert!(runtime.contains(snapshot), "{runtime}");
    assert!(
        runtime.contains("harc_rt::harc_vec_lane_read<8>"),
        "{runtime}"
    );

    let mut scalar_opts = cpp_tb::EmitOpts::default();
    scalar_opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "SnapshotTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "count_out",
                    ir::PortDirection::Out,
                    24,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "en",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("scalar snapshot DUT interface"),
    );
    for error in [
        tbir::emit(&program, &source, &scalar_opts)
            .expect_err("self-contained emission must reject a snapshotted scalar index"),
        tbir::common::plan_common_tests(&program, &scalar_opts, "scalar_snapshot__")
            .expect_err("common planning must reject a snapshotted scalar index"),
    ] {
        assert!(error.0.contains("resolved interface is scalar"), "{error}");
    }
}

#[test]
fn dut_access_artifacts_are_stable_under_test_and_probe_id_permutation() {
    let test = |name: &str, probe: &str, path: &str| {
        format!(
            r#"test {name}
    let dut : ProbeOrderTop
        probe {probe} : uint<8> at {path}
    end let dut
    clock clk = 10ns
    run
        let observed : uint<8> = dut.{probe}
        assert observed == 0 else fail("{name} probe")
    end run
end test {name}
"#
        )
    };
    let a = test("ProbeOrderA", "alpha", "core.alpha");
    let b = test("ProbeOrderB", "beta", "core.beta");
    let lower = |text: String| {
        let source = parse_source(&text).expect("probe-order source parses");
        let program = lower::lower_program(&source).expect("probe-order source lowers");
        verify::verify_program(&program).expect("probe-order program verifies");
        program
    };
    let first = lower(format!("{a}{b}"));
    let second = lower(format!("{b}{a}"));
    assert_ne!(
        first.probes[0].name, second.probes[0].name,
        "fixture must exercise different positional ProbeIds"
    );
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "ProbeOrderTop",
            vec![ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            )],
        )
        .expect("probe-order DUT interface"),
    );
    let first_plan =
        tbir::common::plan_common_tests(&first, &opts, "suite__").expect("first probe order plans");
    let second_plan = tbir::common::plan_common_tests(&second, &opts, "suite__")
        .expect("second probe order plans");
    assert_eq!(
        tbir::common::emit_common_interface(&first_plan).expect("first interface"),
        tbir::common::emit_common_interface(&second_plan).expect("second interface")
    );
    assert_eq!(
        tbir::common::emit_common_runtime(&first_plan).expect("first runtime"),
        tbir::common::emit_common_runtime(&second_plan).expect("second runtime")
    );
    assert_eq!(
        harc::codegen::sv_stub::emit_stub_from_plan(first_plan.dut_access()).expect("first stub"),
        harc::codegen::sv_stub::emit_stub_from_plan(second_plan.dut_access()).expect("second stub")
    );
    let capsule = |plan: &tbir::common::CommonCppPlan, name: &str| {
        let index = plan
            .artifact_plan()
            .tests()
            .iter()
            .position(|test| test.name() == name)
            .expect("named capsule");
        tbir::common::emit_common_capsule(plan, index).expect("capsule emits")
    };
    for name in ["ProbeOrderA", "ProbeOrderB"] {
        assert_eq!(capsule(&first_plan, name), capsule(&second_plan, name));
    }
}

#[test]
fn shared_direct_dut_read_uses_the_resolved_signed_interface_type() {
    let source = parse_source(
        r#"agent SignedReader
    function sample() -> sint<8>
        let raw = dut.signed_out
        return raw
    end function sample
end agent SignedReader

testbench SignedTb
    dut : SignedTop
    reader : SignedReader
end testbench SignedTb

impl SignedRead for SignedTb
    clock clk = 10ns
    run
        let observed : sint<8> = reader.sample()
        assert observed <= 0 else fail("signed read")
    end run
end impl SignedRead"#,
    )
    .expect("signed direct-port source parses");
    let program = lower::lower_program(&source).expect("signed direct-port source lowers");
    verify::verify_program(&program).expect("signed direct-port program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1), ("signed_out".to_string(), 8)]);
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "SignedTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new_typed(
                    "signed_out",
                    ir::PortDirection::Out,
                    8,
                    ir::IrType::SInt(Some(8)),
                    None,
                    None,
                ),
            ],
        )
        .expect("signed DUT interface"),
    );
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("signed direct-port access plans");
    let inferred = program
        .functions
        .iter()
        .flat_map(|function| {
            function
                .locals
                .iter()
                .enumerate()
                .filter_map(|(index, local)| {
                    matches!(local.ty, ir::IrType::Unknown).then(|| {
                        plan.dut_access()
                            .inferred_local_type(function.id, ir::LocalId(index as u32))
                    })?
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(inferred, vec![&ir::IrType::SInt(Some(8))]);
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    assert!(
        runtime.contains("harc_sext_u128") && runtime.contains("ctx.dut->signed_out"),
        "{runtime}"
    );
    let self_contained = tbir::emit(&program, &source, &opts)
        .expect("self-contained emission uses the same resolved DUT catalog");
    assert!(
        self_contained.contains("harc_sext_u128") && self_contained.contains("dut->signed_out"),
        "{self_contained}"
    );
}

#[test]
fn inferred_dut_read_types_revalidate_assign_call_return_and_write_uses() {
    let interface = || {
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "InferenceTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new_typed(
                    "signed_out",
                    ir::PortDirection::Out,
                    8,
                    ir::IrType::SInt(Some(8)),
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new_typed(
                    "signed_out16",
                    ir::PortDirection::Out,
                    16,
                    ir::IrType::SInt(Some(16)),
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new_typed(
                    "signed_out65",
                    ir::PortDirection::Out,
                    65,
                    ir::IrType::SInt(Some(65)),
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new_typed(
                    "signed_in",
                    ir::PortDirection::In,
                    16,
                    ir::IrType::SInt(Some(16)),
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "unsigned_in",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("inference catalog")
    };
    let lower_case = |body: &str| {
        let source = parse_source(&format!(
            r#"function take_signed(value: sint<16>) -> sint<16>
    return value
end function take_signed

test InferenceUse
    let dut : InferenceTop
    clock clk = 10ns
    run
{body}
    end run
end test InferenceUse"#
        ))
        .expect("inference source parses");
        let program = lower::lower_program(&source).expect("inference source lowers");
        verify::verify_program(&program).expect("inference source verifies before DUT metadata");
        program
    };

    let valid = lower_case(
        "        let raw = dut.signed_out\n        let copied = raw + 0\n        let widened : sint<16> = copied\n        let called : sint<16> = take_signed(copied)\n        dut.signed_in = copied",
    );
    let plan = ir::passes::dut_access::analyze(&valid, &interface())
        .expect("exact and widening uses of an inferred signed read are valid");
    let run = valid.tests[0].run;
    for name in ["raw", "copied"] {
        let local = valid.functions[run.index()]
            .locals
            .iter()
            .position(|local| local.name == name)
            .map(|index| ir::LocalId(index as u32))
            .expect("inferred local exists");
        assert_eq!(
            plan.inferred_local_type(run, local),
            Some(&ir::IrType::SInt(Some(8)))
        );
    }

    let mut joined = valid.clone();
    let run = joined.tests[0].run;
    let raw = joined.functions[run.index()]
        .locals
        .iter()
        .position(|local| local.name == "raw")
        .map(|index| ir::LocalId(index as u32))
        .expect("raw local exists");
    let mut wider_port = joined.functions[run.index()].blocks[0]
        .stmts
        .iter()
        .find_map(|stmt| match stmt {
            ir::Stmt::DutRead(destination, port) if *destination == raw => Some(port.clone()),
            _ => None,
        })
        .expect("original inferred DUT read exists");
    wider_port.port_path = vec!["signed_out16".to_string()];
    joined.functions[run.index()].blocks[0]
        .stmts
        .insert(1, ir::Stmt::DutRead(raw, wider_port));
    let joined_plan = ir::passes::dut_access::analyze(&joined, &interface())
        .expect("multiple inferred DUT reads join to their common signed width");
    assert_eq!(
        joined_plan.inferred_local_type(run, raw),
        Some(&ir::IrType::SInt(Some(16)))
    );

    let rejected = |body: &str| {
        ir::passes::dut_access::analyze(&lower_case(body), &interface())
            .expect_err("resolved inferred use must be rejected")
    };
    assert!(
        rejected("        let raw = dut.signed_out\n        let narrow : sint<4> = raw")
            .0
            .contains("resolved DUT read types")
    );
    assert!(
        rejected("        let raw = dut.signed_out\n        let unsigned : uint<8> = raw")
            .0
            .contains("resolved DUT read types")
    );
    assert!(
        rejected("        let raw = dut.signed_out\n        dut.unsigned_in = raw")
            .0
            .contains("signedness")
    );

    let call_source = parse_source(
        r#"function take_unsigned(value: uint<8>) -> uint<8>
    return value
end function take_unsigned
test InferenceCall
    let dut : InferenceTop
    clock clk = 10ns
    run
        let raw = dut.signed_out
        let result : uint<8> = take_unsigned(raw)
    end run
end test InferenceCall"#,
    )
    .expect("inference-call source parses");
    let call_program = lower::lower_program(&call_source).expect("inference-call source lowers");
    let error = ir::passes::dut_access::analyze(&call_program, &interface())
        .expect_err("inferred sign mismatch is rejected at a call boundary");
    assert!(error.0.contains("call argument") || error.0.contains("resolved DUT read types"));

    let return_source = parse_source(
        r#"agent Reader
    function sample() -> uint<8>
        let raw = dut.signed_out
        return raw
    end function sample
end agent Reader
testbench InferenceTb
    dut : InferenceTop
    reader : Reader
end testbench InferenceTb
impl InferenceReturn for InferenceTb
    clock clk = 10ns
    run
        let result : uint<8> = reader.sample()
    end run
end impl InferenceReturn"#,
    )
    .expect("inference-return source parses");
    let return_program =
        lower::lower_program(&return_source).expect("inference-return source lowers");
    let error = ir::passes::dut_access::analyze(&return_program, &interface())
        .expect_err("inferred sign mismatch is rejected at a return boundary");
    assert!(error.0.contains("resolved DUT read types"), "{error}");

    let sink_source = |statement: &str| {
        parse_source(&format!(
            r#"testbench InferenceSinkTb
    dut : InferenceTop
    unsigned_value : uint<8> default 0
    unsigned_values : queue<uint<8>>
end testbench InferenceSinkTb
impl InferenceSink for InferenceSinkTb
    clock clk = 10ns
    run
        let raw = dut.signed_out
        {statement}
    end run
end impl InferenceSink"#
        ))
        .expect("inferred sink source parses")
    };
    for (statement, destination) in [
        ("unsigned_value = raw", "testbench field"),
        ("unsigned_values.push(raw)", "testbench queue"),
    ] {
        let program = lower::lower_program(&sink_source(statement))
            .unwrap_or_else(|error| panic!("{destination} source lowers: {error}"));
        verify::verify_program(&program)
            .unwrap_or_else(|errors| panic!("{destination} verifies before metadata: {errors:?}"));
        let error = ir::passes::dut_access::analyze(&program, &interface())
            .expect_err("inferred signedness drift into host state must reject");
        assert!(error.0.contains(destination), "{destination}: {error}");
    }

    let wide_source = parse_source(
        r#"testbench InferenceWideSinkTb
    dut : InferenceTop
    wide_value : sint<200> default 0
    wide_values : queue<sint<200>>
end testbench InferenceWideSinkTb
impl InferenceWideSink for InferenceWideSinkTb
    clock clk = 10ns
    run
        let raw = dut.signed_out65
        wide_value = raw
        wide_values.push(raw)
    end run
end impl InferenceWideSink"#,
    )
    .expect("wide inferred sink source parses");
    let wide_program =
        lower::lower_program(&wide_source).expect("wide inferred sink source lowers");
    verify::verify_program(&wide_program).expect("wide inferred sink verifies before metadata");
    let mut wide_opts = cpp_tb::EmitOpts::default();
    wide_opts.dut_interface = Some(interface());
    let wide_plan = tbir::common::plan_common_tests(&wide_program, &wide_opts, "wide_sink__")
        .expect("wide inferred storage sinks plan");
    let common = tbir::common::emit_common_capsule(&wide_plan, 0)
        .expect("wide inferred common capsule emits");
    let self_contained =
        tbir::emit(&wide_program, &wide_source, &wide_opts).expect("wide inferred self emits");
    for (layout, cpp) in [("common", common), ("self", self_contained)] {
        assert!(
            cpp.matches("harc_wide_sext<7>").count() >= 2,
            "{layout} must sign-extend the 65-bit inferred value at both typed state sinks: {cpp}"
        );
    }
}

#[test]
fn dut_access_plan_enforces_directional_scalar_transfer_types() {
    let source = parse_source(
        r#"test SignedMismatch
    let dut : SignedTransferTop
    clock clk = 10ns
    run
        let unsigned_value : uint<8> = dut.signed_out
        dut.signed_in = unsigned_value
    end run
end test SignedMismatch"#,
    )
    .expect("signed-transfer source parses");
    let program = lower::lower_program(&source).expect("signed-transfer source lowers");
    verify::verify_program(&program).expect("structural verification precedes DUT catalog typing");
    let interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "SignedTransferTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "signed_out",
                ir::PortDirection::Out,
                8,
                ir::IrType::SInt(Some(8)),
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "signed_in",
                ir::PortDirection::In,
                8,
                ir::IrType::SInt(Some(8)),
                None,
                None,
            ),
        ],
    )
    .expect("signed-transfer interface");
    let error = ir::passes::dut_access::analyze(&program, &interface)
        .expect_err("signed DUT data may not enter an unsigned destination implicitly");
    assert!(
        error.0.contains("incompatible read destination type"),
        "{error}"
    );

    let explicit = parse_source(
        r#"test SignedExplicit
    let dut : SignedTransferTop
    clock clk = 10ns
    run
        let unsigned_value : uint<8> = dut.signed_out.trunc<8>()
        dut.signed_in = unsigned_value as sint<8>
    end run
end test SignedExplicit"#,
    )
    .expect("explicit signed-transfer source parses");
    let explicit = lower::lower_program(&explicit).expect("explicit signed transfer lowers");
    verify::verify_program(&explicit).expect("explicit signed transfer verifies");
    ir::passes::dut_access::analyze(&explicit, &interface)
        .expect("explicit relabels establish both transfer boundaries");
}

#[test]
fn dut_writes_use_contextual_ranges_and_destination_signed_widening() {
    let source = parse_source(
        r#"test ContextualWrites
    let dut : ContextualTop
        probe force forced_signed : sint<200> at core.forced_signed
    end let dut
    clock clk = 10ns
    run
        let negative : sint<8> = -1
        let positive : uint<8> = 127
        dut.signed8 = -1
        dut.signed8 = 1 << 6
        dut.unsigned8 = 255
        dut.unsigned8 = 1 << 7
        dut.signed129 = negative
        dut.signed200 = negative
        dut.unsigned200 = positive
        dut.forced_signed = negative
        release dut.forced_signed
    end run
end test ContextualWrites"#,
    )
    .expect("contextual-write source parses");
    let program = lower::lower_program(&source).expect("contextual-write source lowers");
    verify::verify_program(&program).expect("contextual-write source verifies structurally");
    let interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "ContextualTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "signed8",
                ir::PortDirection::In,
                8,
                ir::IrType::SInt(Some(8)),
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "unsigned8",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "signed129",
                ir::PortDirection::In,
                129,
                ir::IrType::SInt(Some(129)),
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "signed200",
                ir::PortDirection::In,
                200,
                ir::IrType::SInt(Some(200)),
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "unsigned200",
                ir::PortDirection::In,
                200,
                None,
                None,
            ),
        ],
    )
    .expect("contextual-write catalog");
    ir::passes::dut_access::analyze(&program, &interface)
        .expect("contextual signed and exact literal writes plan");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_interface = Some(interface);
    let plan = tbir::common::plan_common_tests(&program, &opts, "contextual__")
        .expect("contextual common plan");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("contextual common capsule");
    assert!(capsule.contains("harc_wide_sext<5>"), "{capsule}");
    assert!(
        capsule.matches("harc_wide_sext<7>").count() >= 2,
        "{capsule}"
    );
    assert!(capsule.contains("harc_wide_zext<7>"), "{capsule}");
    assert!(capsule.contains("forced_signed_drv") && capsule.contains("forced_signed_en = 0"));
    let standalone = tbir::emit(&program, &source, &opts).expect("contextual standalone emits");
    assert!(standalone.contains("harc_wide_sext<5>"), "{standalone}");
    assert!(
        standalone.matches("harc_wide_sext<7>").count() >= 2,
        "{standalone}"
    );

    let reject = |statement: &str| {
        let source = parse_source(&format!(
            r#"test BadWrite
    let dut : ContextualTop
    clock clk = 10ns
    run
        {statement}
    end run
end test BadWrite"#
        ))
        .expect("bad-write source parses");
        let program = lower::lower_program(&source).expect("bad-write source lowers");
        verify::verify_program(&program).expect("bad-write source verifies structurally");
        let interface = ir::passes::dut_access::DutInterfaceCatalog::new(
            "ContextualTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "unsigned8",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new_typed(
                    "signed8",
                    ir::PortDirection::In,
                    8,
                    ir::IrType::SInt(Some(8)),
                    None,
                    None,
                ),
            ],
        )
        .expect("bad-write catalog");
        ir::passes::dut_access::analyze(&program, &interface)
            .expect_err("bad write must fail after DUT metadata resolution")
    };
    assert!(reject("dut.unsigned8 = 256").0.contains("needs 9 bits"));
    assert!(reject("dut.unsigned8 = 1 << 8").0.contains("needs 9 bits"));
    assert!(reject("dut.signed8 = 1 << 7").0.contains("needs 9 bits"));
    assert!(reject("dut.unsigned8 = -1").0.contains("negative"));
    assert!(
        reject("let signed : sint<8> = -1\n        dut.unsigned8 = signed")
            .0
            .contains("signedness")
    );
    let explicit = parse_source(
        r#"test ExplicitWrite
    let dut : ContextualTop
    clock clk = 10ns
    run
        let signed : sint<16> = -1
        dut.unsigned8 = signed.trunc<8>()
    end run
end test ExplicitWrite"#,
    )
    .expect("explicit-write source parses");
    let explicit = lower::lower_program(&explicit).expect("explicit-write source lowers");
    let interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "ContextualTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "unsigned8",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("explicit-write catalog");
    ir::passes::dut_access::analyze(&explicit, &interface)
        .expect("explicit truncation and unsigned relabel remain legal");
}

#[test]
fn aggregate_ports_use_one_physical_leaf_and_reject_alias_spellings() {
    let source = parse_source(
        r#"test AggregateLeaf
    let dut : AggregateTop
    clock clk = 10ns
    run
        dut.group.value = 1
    end run
end test AggregateLeaf"#,
    )
    .expect("aggregate source parses");
    let program = lower::lower_program(&source).expect("aggregate source lowers");
    verify::verify_program(&program).expect("aggregate source verifies");
    let catalog = |leaf_direction| {
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "AggregateTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "group",
                    ir::PortDirection::Out,
                    8,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "group_value",
                    leaf_direction,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("aggregate catalog")
    };
    let interface = catalog(ir::PortDirection::In);
    let plan = ir::passes::dut_access::analyze(&program, &interface)
        .expect("leaf direction is not borrowed from the aggregate root");
    let leaf = plan
        .accesses()
        .iter()
        .find(|access| access.path() == ["group_value"])
        .expect("flattened physical leaf");
    assert_eq!(leaf.direction(), Some(ir::PortDirection::In));

    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_interface = Some(interface);
    let common = tbir::common::plan_common_tests(&program, &opts, "aggregate__")
        .expect("aggregate common plan");
    let capsule = tbir::common::emit_common_capsule(&common, 0).expect("aggregate capsule renders");
    assert!(capsule.contains("ctx.dut->group_value"), "{capsule}");
    assert!(!capsule.contains("ctx.dut->group.value"), "{capsule}");
    let self_contained = tbir::emit(&program, &source, &opts).expect("aggregate suite renders");
    assert!(
        self_contained.contains("dut->group_value"),
        "{self_contained}"
    );

    let error = ir::passes::dut_access::analyze(&program, &catalog(ir::PortDirection::Out))
        .expect_err("the physical leaf's output direction rejects writes");
    assert!(error.0.contains("output-only"), "{error}");

    let mut aliases = program.clone();
    let run = aliases.tests[0].run;
    aliases.functions[run.index()].blocks[0]
        .stmts
        .push(ir::Stmt::DutWrite(
            ir::PortRef {
                testbench_field: "dut".to_string(),
                origin: ir::PortOrigin::Dut,
                port_path: vec!["group_value".to_string()],
                aggregate_path: true,
                deferred_bus_binding: None,
                direction: None,
                width: None,
                value_type: None,
                access: ir::PortAccess::Port,
                probe: None,
                lane: None,
            },
            ir::Expr::Literal {
                value: 2,
                ty: ir::IrType::Unknown,
            },
        ));
    let error = ir::passes::dut_access::analyze(&aliases, &catalog(ir::PortDirection::In))
        .expect_err("logical aliases of one physical binding are rejected");
    assert!(error.0.contains("aliases or conflicts"), "{error}");

    let error = ir::passes::dut_access::DutInterfaceCatalog::new(
        "AggregateTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "group.value",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "group_value",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
        ],
    )
    .expect_err("catalog aliases are rejected");
    assert!(error.0.contains("same physical binding"), "{error}");
}

#[test]
fn signed_aggregate_coverpoints_use_the_self_contained_access_plan() {
    let source = parse_source(
        r#"covergroup SignedCov @(posedge dut.clk)
    cp_signed : cover dut.group.signed_value
        bins
            negative = {-1}
        end bins
end covergroup SignedCov

testbench SignedCovTb
    dut : SignedCovTop
    cov : SignedCov
end testbench SignedCovTb

impl SignedCovTest for SignedCovTb
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end impl SignedCovTest"#,
    )
    .expect("signed aggregate covergroup parses");
    let program = lower::lower_program(&source).expect("signed aggregate covergroup lowers");
    verify::verify_program(&program).expect("signed aggregate covergroup verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    set_dut_interface(
        &mut opts,
        "SignedCovTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "group_signed_value",
                ir::PortDirection::Out,
                8,
                ir::IrType::SInt(Some(8)),
                None,
                None,
            ),
        ],
    );

    let standalone = tbir::emit(&program, &source, &opts).expect("standalone covergroup emits");
    assert!(
        standalone.contains("int64_t _v = (int64_t)(harc_rt::harc_read(dut->group_signed_value))"),
        "{standalone}"
    );
    assert!(
        !standalone.contains("dut->group.signed_value"),
        "{standalone}"
    );
}

#[test]
fn clock_is_validated_as_a_one_bit_input_in_the_access_plan() {
    let (program, _) = minimal_program();
    let catalog = |direction, width| {
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "CommonReg",
            vec![
                ir::passes::dut_access::DutInterfacePort::new("clk", direction, width, None, None),
                ir::passes::dut_access::DutInterfacePort::new(
                    "d",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "q",
                    ir::PortDirection::Out,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("clock catalog")
    };
    ir::passes::dut_access::analyze(&program, &catalog(ir::PortDirection::In, 1))
        .expect("one-bit input clock plans");
    for (direction, width, expected) in [
        (ir::PortDirection::Out, 1, "direction"),
        (ir::PortDirection::In, 2, "width"),
    ] {
        let error = ir::passes::dut_access::analyze(&program, &catalog(direction, width))
            .expect_err("invalid clock metadata must fail");
        assert!(error.0.contains(expected), "{error}");
    }
    let missing = ir::passes::dut_access::DutInterfaceCatalog::new(
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "q",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("missing-clock catalog itself is well formed");
    let error = ir::passes::dut_access::analyze(&program, &missing)
        .expect_err("absent clock fails planning");
    assert!(
        error.0.contains("clk") && error.0.contains("absent"),
        "{error}"
    );
}

#[test]
fn module_typed_testbench_parameters_keep_their_exact_receiver_identity() {
    let source = parse_source(
        r#"function helper_sample(model: ModuleParamTop) -> uint<8>
    return model.q
end function helper_sample

testbench ModuleParamTb
    dut : ModuleParamTop

    function sample(model: ModuleParamTop) -> uint<8>
        return model.q
    end function sample

    function forward(model: ModuleParamTop) -> uint<8>
        return sample(model)
    end function forward

    function through_helper(model: ModuleParamTop) -> uint<8>
        return helper_sample(model)
    end function through_helper
end testbench ModuleParamTb

impl ModuleParamTest for ModuleParamTb
    clock clk = 10ns
    run
        assert forward(dut) == 0 else fail("module receiver")
        assert through_helper(dut) == 0 else fail("helper module receiver")
    end run
end impl ModuleParamTest"#,
    )
    .expect("module-parameter source parses");
    let program = lower::lower_program(&source).expect("module-parameter source lowers");
    verify::verify_program(&program).expect("module-parameter program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_interface = Some(
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "ModuleParamTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "q",
                    ir::PortDirection::Out,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("module-parameter DUT interface"),
    );
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("module-parameter access plans");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("common runtime emits");
    assert_eq!(
        runtime.matches("harc_read(model->q)").count(),
        2,
        "{runtime}"
    );
    assert!(
        runtime.contains("ModuleParamTb_sample(ctx, _tb, model)"),
        "{runtime}"
    );

    let forward = program.testbench_types[0]
        .method("forward")
        .expect("forward method")
        .function;
    let mut corrupted = program.clone();
    let argument = corrupted.functions[forward.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::TestbenchCall { args, dut_args, .. } if dut_args == &[0] => args.first_mut(),
            _ => None,
        })
        .expect("forwarded module argument");
    *argument = ir::Expr::Literal {
        value: 1,
        ty: ir::IrType::Unknown,
    };
    let errors = verify::verify_program(&corrupted)
        .expect_err("a module argument without current-DUT or typed-parameter identity must fail");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("DUT argument 1 has a non-canonical payload")),
        "{errors:?}"
    );
}

#[test]
fn probe_coverpoints_pull_the_self_contained_root_header_from_the_access_plan() {
    let source = parse_source(
        r#"covergroup ProbeCov @(posedge dut.clk)
    cp_status : cover dut.status
        bins
            zero = {0}
        end bins
end covergroup ProbeCov

testbench ProbeCovTb
    let dut : ProbeCollisionTop
        probe status : uint<8> at core.status
    end let dut
    cov : ProbeCov
end testbench ProbeCovTb

impl ProbeCovTest for ProbeCovTb
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end impl ProbeCovTest"#,
    )
    .expect("probe covergroup parses");
    let mut program = lower::lower_program(&source).expect("probe covergroup lowers");
    let probe = program.probes[0].clone();
    let ir::Expr::Port(port) = &mut program.covgroups[0].points[0].target else {
        panic!("probe coverpoint target is a port")
    };
    port.port_path = vec![probe.name];
    port.aggregate_path = true;
    port.direction = None;
    port.width = Some(probe.ty.width());
    port.value_type = Some(probe.ty.ir_type());
    port.access = if probe.force {
        ir::PortAccess::Force
    } else {
        ir::PortAccess::Probe
    };
    port.probe = Some(probe.id);
    verify::verify_program(&program).expect("probe covergroup verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    set_dut_interface(
        &mut opts,
        "ProbeCollisionTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "status",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
        ],
    );
    let emitted = tbir::emit(&program, &source, &opts).expect("probe covergroup emits");
    assert!(
        emitted.contains("#include \"VProbeCollisionTop___024root.h\"")
            && emitted.contains("dut->rootp->ProbeCollisionTop__DOT__harc_probes__DOT__status"),
        "{emitted}"
    );
    let mut fallback_opts = opts;
    fallback_opts.dut_interface = None;
    let fallback =
        tbir::emit(&program, &source, &fallback_opts).expect("fallback probe covergroup emits");
    assert!(
        fallback.contains("#include \"VProbeCollisionTop___024root.h\""),
        "{fallback}"
    );
}

#[test]
fn dut_access_plan_checks_lane_bounds_and_qualifies_packed_lane_emission() {
    let lower_lane = |index: u32| {
        let source = parse_source(&format!(
            r#"test LaneAccess
    let dut : LaneTop
    clock clk = 10ns
    run
        dut.lanes[{index}] = 7
    end run
end test LaneAccess"#
        ))
        .expect("lane source parses");
        let program = lower::lower_program(&source).expect("lane source lowers");
        verify::verify_program(&program).expect("lane program verifies");
        program
    };
    let interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "LaneTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "lanes",
                ir::PortDirection::In,
                16,
                Some(8),
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "selector",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("lane DUT interface");

    let invalid = lower_lane(2);
    let error = ir::passes::dut_access::analyze(&invalid, &interface)
        .expect_err("constant lane outside the resolved shape must fail");
    assert!(
        error.0.contains("outside the resolved 2-element"),
        "{error}"
    );

    let scalar_interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "LaneTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "lanes",
                ir::PortDirection::In,
                16,
                None,
                None,
            ),
        ],
    )
    .expect("scalar-shape control catalog");
    let error = ir::passes::dut_access::analyze(&lower_lane(1), &scalar_interface)
        .expect_err("an indexed scalar interface must fail before rendering");
    assert!(error.0.contains("resolved interface is scalar"), "{error}");

    let error = ir::passes::dut_access::DutInterfaceCatalog::new(
        "LaneTop",
        vec![ir::passes::dut_access::DutInterfacePort::new(
            "lanes",
            ir::PortDirection::In,
            16,
            Some(6),
            None,
        )],
    )
    .expect_err("a non-integral packed-lane shape must fail catalog construction");
    assert!(error.0.contains("not an integral number"), "{error}");

    let wide_source = parse_source(
        r#"test WideLaneAccess
    let dut : LaneTop
    clock clk = 10ns
    run
        dut.lanes[1] = 7
    end run
end test WideLaneAccess"#,
    )
    .expect("wide-lane source parses");
    let wide_program = lower::lower_program(&wide_source).expect("wide-lane source lowers");
    verify::verify_program(&wide_program).expect("wide-lane program verifies structurally");
    let wide_interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "LaneTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "lanes",
                ir::PortDirection::In,
                256,
                Some(128),
                None,
            ),
        ],
    )
    .expect("wide packed-lane catalog is representable");
    let mut wide_opts = cpp_tb::EmitOpts::default();
    wide_opts.dut_interface = Some(wide_interface);
    for error in [
        tbir::emit(&wide_program, &wide_source, &wide_opts)
            .expect_err("self-contained layout must reject truncating packed-lane helpers"),
        tbir::common::plan_common_tests(&wide_program, &wide_opts, "wide_lane__")
            .expect_err("common layout must reject truncating packed-lane helpers"),
    ] {
        assert!(
            error.0.contains("wider than the supported 64-bit"),
            "{error}"
        );
    }

    let valid = lower_lane(1);
    ir::passes::dut_access::analyze(&valid, &interface)
        .expect("in-range lane is represented exactly in the neutral plan");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("lanes".to_string(), 16),
        ("selector".to_string(), 8),
    ]);
    opts.dut_interface = Some(interface);
    let plan = tbir::common::plan_common_tests(&valid, &opts, "suite__")
        .expect("the resolved packed-lane access plans");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("lane capsule emits");
    assert!(
        capsule.contains("harc_rt::harc_vec_lane_write<8>(ctx.dut->lanes"),
        "{capsule}"
    );

    let dynamic_source = parse_source(
        r#"function select_lane(value: uint<8>) -> uint<8>
    return value
end function select_lane

function select_lane_after_write(model: LaneTop, value: uint<8>) -> uint<8>
    model.lanes[0] = 8
    return value
end function select_lane_after_write

test DynamicLaneAccess
    let dut : LaneTop
    clock clk = 10ns
    run
        dut.lanes = 0
        dut.lanes[0] = 8
        dut.lanes[select_lane(dut.selector)] = 9
        assert dut.lanes[select_lane(dut.selector)] == 9 else fail("dynamic packed lane")
        log(info, "dynamic lane=${dut.lanes[select_lane_after_write(dut, dut.selector)]}")
    end run
end test DynamicLaneAccess"#,
    )
    .expect("dynamic-lane source parses");
    let dynamic = lower::lower_program(&dynamic_source).expect("dynamic-lane source lowers");
    verify::verify_program(&dynamic).expect("dynamic-lane program verifies");
    let dynamic_plan = tbir::common::plan_common_tests(&dynamic, &opts, "dynamic__")
        .expect("dynamic packed-lane access plans");
    assert!(
        dynamic_plan
            .dut_access()
            .accesses()
            .iter()
            .any(|access| access.path() == ["selector"]),
        "the dynamic lane selector must be part of the immutable access plan"
    );
    let lane_accesses = dynamic_plan
        .dut_access()
        .accesses()
        .iter()
        .filter(|access| access.path() == ["lanes"])
        .collect::<Vec<_>>();
    assert_eq!(lane_accesses.len(), 1, "one physical packed-port binding");
    assert_eq!(
        lane_accesses[0].lane_shapes(),
        &std::collections::BTreeSet::from([
            ir::passes::dut_access::DutLaneShape::None,
            ir::passes::dut_access::DutLaneShape::Constant,
            ir::passes::dut_access::DutLaneShape::Dynamic,
        ])
    );
    let dynamic_capsule = tbir::common::emit_common_capsule(&dynamic_plan, 0)
        .expect("dynamic packed-lane capsule emits");
    assert!(
        dynamic_capsule.matches("harc_rt::harc_vec_lane_").count() >= 2,
        "{dynamic_capsule}"
    );

    let signed_interface = ir::passes::dut_access::DutInterfaceCatalog::new(
        "LaneTop",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "lanes",
                ir::PortDirection::In,
                16,
                ir::IrType::SInt(Some(16)),
                Some(8),
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "selector",
                ir::PortDirection::Out,
                8,
                None,
                None,
            ),
        ],
    )
    .expect("signed-lane DUT interface");
    let signed_plan = ir::passes::dut_access::analyze(&dynamic, &signed_interface)
        .expect("snapshot lane temporaries inherit signed lane type");
    assert!(
        dynamic
            .functions
            .iter()
            .any(|function| function
                .locals
                .iter()
                .enumerate()
                .any(|(index, local)| matches!(local.ty, ir::IrType::Unknown)
                    && signed_plan.inferred_local_type(function.id, ir::LocalId(index as u32))
                        == Some(&ir::IrType::SInt(Some(8))))),
        "formatted dynamic-lane snapshots must retain the resolved lane type"
    );

    let probe_selector_source = parse_source(
        r#"test ProbeLaneAccess
    let dut : LaneTop
        probe selector_probe : uint<8> at core.selector
    end let dut
    clock clk = 10ns
    run
        dut.lanes[dut.selector_probe] = 11
    end run
end test ProbeLaneAccess"#,
    )
    .expect("probe-selector source parses");
    let probe_selector =
        lower::lower_program(&probe_selector_source).expect("probe-selector source lowers");
    verify::verify_program(&probe_selector).expect("probe-selector program verifies");
    let probe_plan = tbir::common::plan_common_tests(&probe_selector, &opts, "probe_lane__")
        .expect("probe-valued dynamic selector is traversed and planned");
    assert!(probe_plan
        .dut_access()
        .probes()
        .iter()
        .any(|probe| probe.name() == "selector_probe"));
    let probe_capsule =
        tbir::common::emit_common_capsule(&probe_plan, 0).expect("probe selector capsule emits");
    assert!(probe_capsule.contains("___024root.h"), "{probe_capsule}");

    let mut bus_selector = dynamic.clone();
    let lane = bus_selector
        .functions
        .iter_mut()
        .flat_map(|function| &mut function.blocks)
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::DutWrite(port, _) => port.lane.as_mut(),
            _ => None,
        })
        .expect("dynamic lane write");
    *lane = ir::LaneIndex::Var(Box::new(ir::Expr::Port(ir::PortRef {
        testbench_field: "dut".to_string(),
        origin: ir::PortOrigin::BusBinding {
            binding: ir::BusBindingId(0),
            field: "bus".to_string(),
        },
        port_path: vec!["bus".to_string(), "ctrl".to_string(), "index".to_string()],
        aggregate_path: false,
        deferred_bus_binding: None,
        direction: None,
        width: Some(8),
        value_type: Some(ir::IrType::UInt(Some(8))),
        access: ir::PortAccess::Port,
        probe: None,
        lane: None,
    })));
    let error = tbir::common::plan_common_tests(&bus_selector, &opts, "bus_lane__")
        .expect_err("a bus-relative dynamic selector requires an explicit typed binding");
    assert!(error.0.contains("missing concrete bus binding"), "{error}");
}

#[test]
fn capsule_dut_access_profile_uses_only_its_own_lane_shapes() {
    let source = |variant_access: &str| {
        parse_source(&format!(
            r#"test StableA
    let dut : LaneTop
    clock clk = 10ns
    run
        dut.lanes[0] = 1
    end run
end test StableA

test VariantB
    let dut : LaneTop
    clock clk = 10ns
    run
        {variant_access}
    end run
end test VariantB"#
        ))
        .expect("profile-isolation source parses")
    };
    let interface = || {
        ir::passes::dut_access::DutInterfaceCatalog::new(
            "LaneTop",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "clk",
                    ir::PortDirection::In,
                    1,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "lanes",
                    ir::PortDirection::In,
                    16,
                    Some(8),
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "selector",
                    ir::PortDirection::Out,
                    8,
                    None,
                    None,
                ),
            ],
        )
        .expect("profile-isolation DUT interface")
    };
    let plan = |source: &harc::ast::SourceFile| {
        let program = lower::lower_program(source).expect("profile-isolation source lowers");
        verify::verify_program(&program).expect("profile-isolation program verifies");
        let mut opts = cpp_tb::EmitOpts::default();
        opts.dut_interface = Some(interface());
        tbir::common::plan_common_tests(&program, &opts, "lane_profile__")
            .expect("profile-isolation suite plans")
    };
    let constant = source("dut.lanes[0] = 2");
    let dynamic = source("dut.lanes[dut.selector] = 2");
    let constant_plan = plan(&constant);
    let dynamic_plan = plan(&dynamic);
    assert_eq!(
        constant_plan.capsules()[0].dut_access().digest(),
        dynamic_plan.capsules()[0].dut_access().digest(),
        "an unrelated test's lane shape must not invalidate StableA"
    );
    assert_eq!(
        tbir::common::emit_common_capsule(&constant_plan, 0).expect("constant capsule emits"),
        tbir::common::emit_common_capsule(&dynamic_plan, 0).expect("dynamic capsule emits"),
        "an unrelated test's lane edit must not rewrite StableA"
    );
}

#[test]
fn parameterized_catalog_is_shared_by_self_contained_and_common_planning() {
    let source = parse_source(MINIMAL_COMMON_SRC).expect("minimal source parses");
    let (program, opts) = minimal_program();
    tbir::emit(&program, &source, &opts)
        .expect("self-contained TBIR consumes the effective parameterized catalog");
    harc::codegen::tbir::common::plan_common_tests(&program, &opts, "parameterized__")
        .expect("common TBIR consumes the same effective parameterized catalog");
}

#[test]
fn common_planning_requires_a_resolved_dut_interface_before_any_rendering() {
    let (program, mut opts) = minimal_program();
    opts.dut_interface = None;
    let error = harc::codegen::tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("common planning must fail closed without typed DUT metadata");
    assert!(
        error.0.contains("resolved DUT interface catalog"),
        "{error}"
    );
}

fn structural_program() -> (harc::ir::TbProgram, cpp_tb::EmitOpts) {
    let source = parse_source(STRUCTURAL_SHARED_TYPES_SRC).expect("structural source parses");
    let program = lower::lower_program(&source).expect("structural source lowers");
    verify::verify_program(&program).expect("structural program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_common_reg_interface(&mut opts);
    (program, opts)
}

fn bound_bus_program(two_tests: bool) -> (harc::ir::TbProgram, cpp_tb::EmitOpts) {
    let source_text = if two_tests {
        BOUND_BUS_PLACEMENT_SRC
    } else {
        BOUND_BUS_PLACEMENT_SRC
            .split_once("\ntestbench BusTbB")
            .expect("fixture has a second testbench")
            .0
    };
    let source = parse_source(source_text).expect("bound-bus source parses");
    let program = lower::lower_program(&source).expect("bound-bus source lowers");
    verify::verify_program(&program).expect("bound-bus program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("bus_status".to_string(), 8),
        ("first_data".to_string(), 8),
        ("first_valid".to_string(), 1),
        ("first_ready".to_string(), 1),
        ("second_data".to_string(), 8),
        ("second_valid".to_string(), 1),
        ("second_ready".to_string(), 1),
    ]);
    set_dut_interface(
        &mut opts,
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "bus_status",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "first_data",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "first_valid",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "first_ready",
                ir::PortDirection::Out,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "second_data",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "second_valid",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "second_ready",
                ir::PortDirection::Out,
                1,
                None,
                None,
            ),
        ],
    );
    (program, opts)
}

fn regblock_program(fixture_name: &str) -> (harc::ir::TbProgram, cpp_tb::EmitOpts) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = std::fs::read_to_string(root.join("tests/fixtures").join(fixture_name))
        .expect("read register-block fixture");
    let bus = r#"
bus BusAxiLite
  handshake_channel aw: send kind: valid_ready
    addr: UInt<8>;
    prot: UInt<3>;
  end handshake_channel aw
  handshake_channel w: send kind: valid_ready
    data: UInt<32>;
    strb: UInt<4>;
  end handshake_channel w
  handshake_channel b: receive kind: valid_ready
    resp: UInt<2>;
  end handshake_channel b
  handshake_channel ar: send kind: valid_ready
    addr: UInt<8>;
    prot: UInt<3>;
  end handshake_channel ar
  handshake_channel r: receive kind: valid_ready
    data: UInt<32>;
    resp: UInt<2>;
  end handshake_channel r
end bus BusAxiLite
"#;
    let merged = merge::merge_for_sim(
        vec![
            parse_source(&fixture).expect("register-block fixture parses"),
            parse_source(bus).expect("BusAxiLite declaration parses"),
        ],
        None,
    )
    .expect("register-block fixture merges");
    let program = lower::lower_program(&merged).expect("register-block fixture lowers");
    verify::verify_program(&program).expect("register-block fixture verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_interface = cpp_tb::dut_interface_catalog(
        &[root.join("tests/dut/AxiLiteRegs.sv")],
        &[],
        "AxiLiteRegs",
        &HashMap::new(),
    )
    .expect("scan register DUT interface");
    (program, opts)
}

#[test]
fn minimal_common_plan_uses_the_canonical_one_capsule_per_test_contract() {
    let (program, opts) = minimal_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");

    assert_eq!(plan.dut_type(), "CommonReg");
    assert_eq!(plan.clock_topologies().len(), 2);
    assert_eq!(plan.clock_topologies()[0].clocks()[0].name(), "clk");
    assert_eq!(plan.clock_topologies()[0].clocks()[0].period_ps(), 10_000);
    assert_eq!(plan.clock_topologies()[0].clocks()[0].domain(), None);
    assert_eq!(plan.capsules().len(), 2);
    assert!(plan.capsules().iter().all(|capsule| capsule
        .test_bodies()
        .iter()
        .all(|body| body.placement_reason() == tbir::common::CapsulePlacementReason::TestBody)));
    assert_eq!(plan.capsules()[0].test_bodies()[0].test_index(), 0);
    assert_eq!(plan.capsules()[1].test_bodies()[0].test_index(), 1);

    let artifacts: Vec<_> = plan
        .artifact_plan()
        .artifacts()
        .iter()
        .map(|artifact| (artifact.role(), artifact.filename()))
        .collect();
    assert_eq!(
        artifacts,
        vec![
            (ArtifactRole::Interface, "suite__suite_api.hpp"),
            (ArtifactRole::Common, "suite__runtime.cpp"),
            (ArtifactRole::Capsule, "suite__test_Common17.cpp"),
            (ArtifactRole::Capsule, "suite__test_Common203.cpp"),
            (ArtifactRole::Registry, "suite__registry.cpp"),
            (ArtifactRole::RuntimeHeader, "harc_thread_rt.h"),
            (ArtifactRole::RuntimeHeader, "harc_random_rt.h"),
            (ArtifactRole::RuntimeHeader, "harc_queue_rt.h"),
            (ArtifactRole::RuntimeHeader, "harc_trace_rt.h"),
            (ArtifactRole::RuntimeHeader, "harc_log_rt.h"),
            (ArtifactRole::RuntimeHeader, "harc_z3_rt.h"),
        ]
    );
    assert_eq!(
        plan.artifact_plan().manifest_filename(),
        "suite__artifacts.json"
    );
}

#[test]
fn common_capsule_render_rejects_foreign_and_detached_handles() {
    let (program, opts) = minimal_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");
    let (foreign_program, foreign_opts) = structural_program();
    let foreign = tbir::common::plan_common_tests(&foreign_program, &foreign_opts, "foreign__")
        .expect("foreign plan");
    let publication = plan.publication().expect("publication plans");

    let foreign_error = publication
        .capsule(&foreign.capsules()[0])
        .expect_err("a capsule from another plan must not render");
    assert!(
        foreign_error.0.contains("does not belong"),
        "{foreign_error}"
    );

    let detached = plan.capsules()[0].clone();
    let detached_error = publication
        .capsule(&detached)
        .expect_err("a detached capsule copy must not render");
    assert!(
        detached_error.0.contains("does not belong"),
        "{detached_error}"
    );

    let selected = publication
        .capsule(&plan.capsules()[0])
        .expect("the plan-owned capsule renders");
    assert!(selected.contains("Common17"), "{selected}");

    let error = tbir::common::emit_common_capsule(&plan, plan.capsules().len())
        .expect_err("an out-of-range selector must fail before producing capsule bytes");
    assert!(error.0.contains("out of range"), "{error}");
}

#[test]
fn minimal_common_rendering_owns_runtime_once_and_capsules_only_their_test() {
    let (program, opts) = minimal_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");
    let publication = plan.publication().expect("publication plans");
    let interface = publication.interface().to_string();
    let runtime = publication.runtime().expect("runtime emits");
    let capsules: Vec<String> = plan
        .capsules()
        .iter()
        .map(|capsule| publication.capsule(capsule).expect("capsule emits"))
        .collect();
    let registry = publication.registry();

    assert!(interface.contains("struct HarcTestContext"));
    assert!(interface.contains("struct HarcTestDescriptor"));
    assert!(!interface.contains(
        "harc_run_test(const char* test_name, HarcTestBody body, int argc, char** argv) {"
    ));
    assert_eq!(runtime.matches("HarcTestContext ctx;").count(), 1);
    assert_eq!(runtime.matches("new VCommonReg").count(), 1);
    assert_eq!(runtime.matches("int harc_run_test(").count(), 1);
    assert!(!runtime.contains("harc_body_Common"));
    assert!(!runtime.contains("static harc_rt::random::HarcRng"));
    assert!(!runtime.contains("thread_local"));

    assert!(capsules[0].contains("harc_body_Common17"));
    assert!(capsules[0].contains("harc_test_Common17"));
    assert!(capsules[0].contains(publication.abi_symbol()));
    assert!(!capsules[0].contains("Common203"));
    assert!(capsules[1].contains("harc_body_Common203"));
    assert!(capsules[1].contains("harc_test_Common203"));
    assert!(capsules[1].contains(publication.abi_symbol()));
    assert!(!capsules[1].contains("Common17"));
    for capsule in &capsules {
        assert!(!capsule.contains("new VCommonReg"));
        assert!(!capsule.contains("ThreadScheduler scheduler"));
        assert!(!capsule.contains("int main("));
        assert!(!capsule.contains("thread_local"));
    }
    assert!(registry.contains("&harc_test_Common17"));
    assert!(registry.contains("&harc_test_Common203"));
    assert!(registry.contains("->abi_anchor"));
    assert!(registry.contains(publication.abi_symbol()));
    assert!(
        registry.find("&harc_test_Common17").unwrap()
            < registry.find("&harc_test_Common203").unwrap()
    );
}

#[test]
fn minimal_common_capsules_are_deterministic_across_emit_jobs() {
    let (program, opts) = minimal_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");
    let serial = Mutex::new(Vec::new());
    let serial_order = tbir::common::emit_common_capsules(&plan, 1, |capsule, cpp, _| {
        serial.lock().unwrap().push((capsule.index(), cpp));
        Ok(())
    })
    .expect("serial capsules emit");
    let parallel = Mutex::new(Vec::new());
    let parallel_order = tbir::common::emit_common_capsules(&plan, 2, |capsule, cpp, _| {
        parallel.lock().unwrap().push((capsule.index(), cpp));
        Ok(())
    })
    .expect("parallel capsules emit");

    let mut serial = serial.into_inner().unwrap();
    let mut parallel = parallel.into_inner().unwrap();
    serial.sort_by_key(|(index, _)| *index);
    parallel.sort_by_key(|(index, _)| *index);
    assert_eq!(serial_order, vec![0, 1]);
    assert_eq!(parallel_order, vec![0, 1]);
    assert_eq!(serial, parallel);
}

#[test]
fn common_parallel_capsule_errors_report_the_lowest_semantic_index() {
    let (program, opts) = minimal_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");
    for jobs in [1, 2, 8] {
        let error = tbir::common::emit_common_capsules(&plan, jobs, |capsule, _, _| {
            if capsule.index() == 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(cpp_tb::EmitError(format!(
                "injected capsule {} failure",
                capsule.index()
            )))
        })
        .expect_err("injected delivery failure");
        assert_eq!(error.0, "injected capsule 0 failure", "emit jobs {jobs}");
    }
}

#[test]
fn common_plan_owns_each_tests_ordered_clock_topology() {
    let source = parse_source(
        r#"
test MultiClock
    let dut : CommonReg
    clock clk = 10ns
    clock aux_clk = 4ns
    run
        wait 1 cycle
    end run
end test MultiClock
"#,
    )
    .expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1), ("aux_clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("multi-clock topology plans");
    assert_eq!(plan.clock_topologies().len(), 1);
    assert_eq!(plan.clock_topologies()[0].test_index(), 0);
    assert_eq!(plan.clock_topologies()[0].clocks().len(), 2);
    assert_eq!(plan.clock_topologies()[0].clocks()[0].name(), "clk");
    assert_eq!(plan.clock_topologies()[0].clocks()[0].period_ps(), 10_000);
    assert_eq!(plan.clock_topologies()[0].clocks()[1].name(), "aux_clk");
    assert_eq!(plan.clock_topologies()[0].clocks()[1].period_ps(), 4_000);

    let mut mt_opts = opts.clone();
    mt_opts.mt = true;
    let mt_plan = tbir::common::plan_common_tests(&program, &mt_opts, "suite__")
        .expect("the same topology plans in MT mode");
    assert!(mt_plan.mt());
    assert_ne!(
        plan.build_profile(),
        mt_plan.build_profile(),
        "worker topology is part of the common build identity"
    );

    let mut alternate_period = program.clone();
    alternate_period.tests[0].clocks[1].period_ps = 6_000;
    let alternate_plan = tbir::common::plan_common_tests(&alternate_period, &opts, "suite__")
        .expect("an alternate valid clock topology plans");
    assert_eq!(
        plan.build_profile(),
        alternate_plan.build_profile(),
        "test-local clock semantics are not native toolchain identity"
    );
    assert_ne!(
        tbir::common::emit_common_capsule(&plan, 0).expect("original clock capsule renders"),
        tbir::common::emit_common_capsule(&alternate_plan, 0)
            .expect("alternate clock capsule renders"),
        "clock periods and primary ordering must change their owning capsule"
    );
}

#[test]
fn common_plan_rejects_corrupt_target_binding_actor_and_clock_without_publication() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("tests/fixtures/tlm_target_thread_test.harc"))
        .expect("read target responder fixture");
    let parsed = parse_source(&source).expect("target responder fixture parses");
    let program = lower::lower_program(&parsed).expect("target responder fixture lowers");
    verify::verify_program(&program).expect("target responder fixture verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_interface = cpp_tb::dut_interface_catalog(
        &[root.join("tests/dut/TlmReadInitiator.sv")],
        &[],
        "TlmReadInitiator",
        &HashMap::new(),
    )
    .expect("target responder interface scans");

    let reject = |program: &ir::TbProgram, expected: &str| {
        let error = tbir::common::plan_common_tests(program, &opts, "corrupt__")
            .expect_err("corrupt topology must fail before a publication can be created");
        assert!(error.0.contains(expected), "{error}");
    };

    let mut missing_binding = program.clone();
    missing_binding.testbenches[0].target_tlm_actors[0].bus_field = "missing".to_string();
    reject(&missing_binding, "no concrete bus binding `missing`");

    let mut missing_actor_type = program.clone();
    missing_actor_type.testbenches[0].target_tlm_actors[0].transactor = ir::TransactorId(u32::MAX);
    reject(&missing_actor_type, "transactor");

    let (mut invalid_clock, clock_opts) = minimal_program();
    invalid_clock.tests[0].clocks[0].period_ps = 0;
    let error = tbir::common::plan_common_tests(&invalid_clock, &clock_opts, "corrupt_clock__")
        .expect_err("an invalid clock must fail before a publication can be created");
    assert!(error.0.contains("non-positive"), "{error}");
}

#[test]
fn common_runner_context_owns_every_generic_run_resource() {
    let (program, opts) = minimal_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");
    let interface = tbir::common::emit_common_interface(&plan).expect("interface emits");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");

    for field in [
        "VerilatedContext verilated;",
        "harc_rt::ThreadScheduler scheduler;",
        "harc_rt::ThreadSlot run_slot;",
        "std::vector<std::function<void()>> _checkers;",
        "std::vector<std::function<void()>> _post_eval_services;",
        "std::vector<std::function<void()>> _auto_cov_reports;",
    ] {
        assert!(
            interface.contains(field),
            "per-run context is missing `{field}`:\n{interface}"
        );
    }
    assert!(
        interface.contains("struct HarcTestRunDescriptor {")
            && interface.contains("const char* name;")
            && interface.contains("HarcTestBody body;")
            && interface.contains(
                "int harc_run_test(const HarcTestRunDescriptor& test, int argc, char** argv);"
            ),
        "the shared runner must consume a typed capsule descriptor:\n{interface}"
    );
    assert!(
        runtime.contains("ctx.verilated.commandArgs(argc, argv);")
            && runtime.contains("new VCommonReg(&ctx.verilated)"),
        "the common runner must use a fresh VerilatedContext per invocation:\n{runtime}"
    );
    assert!(
        runtime.contains("void* run_state = test.create_state(ctx);")
            && runtime.contains("ctx.run_slot.thread = test.body(ctx, &ctx.run_slot, run_state);")
            && runtime.contains("ctx.scheduler.slots.push_back(&ctx.run_slot);"),
        "scheduler and run slot must be context-owned:\n{runtime}"
    );
    assert!(
        runtime.contains("HarcQueueFatalScope")
            && runtime.contains("harc_rt::harc_destroy_scheduler_threads(ctx.scheduler);"),
        "the runner must install scoped queue reporting and explicitly release suspended frames:\n{runtime}"
    );
    let report = runtime
        .find("for (auto& _r : ctx._auto_cov_reports) _r();")
        .expect("final reports");
    let clear = runtime
        .find("ctx._checkers.clear();")
        .expect("callback clear");
    let destroy = runtime
        .find("harc_rt::harc_destroy_scheduler_threads(ctx.scheduler);")
        .expect("scheduler cleanup");
    let destroy_state = runtime
        .find("test.destroy_state(run_state);")
        .expect("run-state cleanup");
    let final_dut = runtime.find("ctx.dut->final();").expect("DUT final");
    assert!(
        report < clear && clear < destroy && destroy < destroy_state && destroy_state < final_dut
    );
    assert!(
        runtime.contains("int harc_run_test(const HarcTestRunDescriptor& test")
            && runtime.contains("test.body(ctx, &ctx.run_slot, run_state)")
            && runtime.contains("test.name"),
        "the runtime lifecycle must be descriptor-driven:\n{runtime}"
    );

    let descriptor = interface
        .split("struct HarcTestDescriptor {")
        .nth(1)
        .and_then(|tail| tail.split("};").next())
        .expect("descriptor body");
    assert!(descriptor.contains("const char* name;"));
    assert!(descriptor.contains("int (*run)(int argc, char** argv);"));
    assert!(descriptor.contains("const char* abi_anchor;"));
}

#[test]
fn common_randomize_uses_typed_targets_and_capsule_owned_site_state() {
    let source = parse_source(
        r#"transaction Req
    errors : uint<8> with [unique within test]
    keep errors in [1..3]
end transaction Req

test RandomizeNames
    let dut : Top
    run
        let ctx : Req
        randomize(ctx)
        let rng : Req
        randomize(rng)
        let errors : Req
        randomize(errors)
        let _cells : Req
        randomize(_cells)
        assert ctx.errors >= 1
        assert rng.errors >= 1
        assert errors.errors >= 1
        assert _cells.errors >= 1
    end run
end test RandomizeNames
"#,
    )
    .expect("source parses");
    let merged = merge::merge_for_sim(vec![source], None).expect("source merges");
    let program = lower::lower_program(&merged).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    set_dut_interface(&mut opts, "Top", vec![]);
    let plan =
        tbir::common::plan_common_tests_with_source(&program, &merged, &opts, "randomize_names__")
            .expect("common plan");
    let publication = plan.publication().expect("publication");
    let interface = publication.interface();
    let runtime = publication.runtime().expect("runtime");
    let capsule = publication.capsule(&plan.capsules()[0]).expect("capsule");

    assert!(!interface.contains("_harc_runtime_random_problem_table"));
    assert!(!interface.contains("_harc_solver_call_sites"));
    assert!(!interface.contains("_solver_site_"));
    assert!(runtime.contains("static constexpr harc_rt::random::HarcRuntimeProblemDescriptor"));
    assert!(capsule.contains("constraint_run_s0"), "{capsule}");
    for expected in [
        "_u_ctx.errors",
        "rng.errors",
        "_u_errors.errors",
        "_cells.errors",
    ] {
        assert!(
            capsule.contains(expected),
            "missing `{expected}`:\n{capsule}"
        );
    }
    assert!(
        !capsule.contains('\u{1e}'),
        "internal binding sentinel leaked"
    );
}

#[test]
fn common_multi_file_component_randomize_sites_keep_distinct_shared_state() {
    const ALPHA: &str = r#"agent Alpha
    hookable draw()
        let ctx : TxA
        randomize(ctx)
    end draw
end agent Alpha

transaction TxA
    value : uint<8> with [unique within test]
end transaction TxA
"#;
    const BRAVO: &str = r#"agent Bravo
    hookable draw()
        let ctx : TxB
        randomize(ctx)
    end draw
end agent Bravo

transaction TxB
    value : uint<130> with [unique within test]
end transaction TxB
"#;
    const TEST: &str = r#"test EqualOffsetComponents
    let dut : Top
    let alpha : Alpha
    let bravo : Bravo
    run
        alpha.draw()
        bravo.draw()
    end run
end test EqualOffsetComponents
"#;

    let render = |reverse_parse_order: bool| {
        let (alpha, bravo) = if reverse_parse_order {
            let bravo = harc::parser::parse_source_named("bravo.harc", BRAVO).expect("bravo");
            let alpha = harc::parser::parse_source_named("alpha.harc", ALPHA).expect("alpha");
            (alpha, bravo)
        } else {
            let alpha = harc::parser::parse_source_named("alpha.harc", ALPHA).expect("alpha");
            let bravo = harc::parser::parse_source_named("bravo.harc", BRAVO).expect("bravo");
            (alpha, bravo)
        };
        let test = harc::parser::parse_source_named("test.harc", TEST).expect("test");
        let merged = merge::merge_for_sim(vec![alpha, bravo, test], None).expect("merge");
        let program = lower::lower_program(&merged).expect("lower");
        verify::verify_program(&program).expect("verify");
        let mut opts = cpp_tb::EmitOpts::default();
        set_dut_interface(&mut opts, "Top", vec![]);
        let plan = tbir::common::plan_common_tests_with_source(
            &program,
            &merged,
            &opts,
            "equal_offsets__",
        )
        .expect("common plan");
        let publication = plan.publication().expect("publication");
        let capsule = publication
            .capsule(&plan.capsules()[0])
            .expect("capsule")
            .to_string();
        (
            publication.interface().to_string(),
            publication.runtime().expect("runtime").to_string(),
            capsule,
            publication.interface_abi().to_string(),
        )
    };

    let (interface, runtime, capsule, interface_abi) = render(false);
    let (reordered_interface, reordered_runtime, reordered_capsule, reordered_abi) = render(true);
    assert_eq!(
        interface.matches("_harc_randomize_c").count(),
        2,
        "{interface}"
    );
    assert_eq!(
        component_solver_site_tags(&interface).len(),
        2,
        "{interface}"
    );
    assert_eq!(interface, reordered_interface);
    assert_eq!(runtime, reordered_runtime);
    assert_eq!(capsule, reordered_capsule);
    assert_eq!(interface_abi, reordered_abi);
    assert!(runtime.contains("Alpha_draw"), "{runtime}");
    assert!(runtime.contains("Bravo_draw"), "{runtime}");
}

#[test]
fn one_test_randomize_edit_preserves_interface_and_unrelated_capsule() {
    let render = |limit: u8| {
        let source = parse_source(&format!(
            r#"transaction Req
    value : uint<8> with [unique within test]
end transaction Req

test Alpha
    let dut : Top
    run
        let req : Req
        randomize(req) with
            req.value <= {limit}
        end randomize
    end run
end test Alpha

test Bravo
    let dut : Top
    run
        log(info, "unchanged")
    end run
end test Bravo
"#
        ))
        .expect("source parses");
        let merged = merge::merge_for_sim(vec![source], None).expect("source merges");
        let program = lower::lower_program(&merged).expect("source lowers");
        verify::verify_program(&program).expect("program verifies");
        let mut opts = cpp_tb::EmitOpts::default();
        set_dut_interface(&mut opts, "Top", vec![]);
        let plan = tbir::common::plan_common_tests_with_source(
            &program,
            &merged,
            &opts,
            "randomize_locality__",
        )
        .expect("common plan");
        let publication = plan.publication().expect("publication");
        (
            publication.interface_abi().to_string(),
            publication.interface().to_string(),
            publication.runtime().expect("runtime"),
            publication.capsule(&plan.capsules()[0]).expect("alpha"),
            publication.capsule(&plan.capsules()[1]).expect("bravo"),
        )
    };

    let before = render(3);
    let after = render(4);
    assert_eq!(
        before.0, after.0,
        "test-local constraints changed interface ABI"
    );
    assert_eq!(
        before.1, after.1,
        "test-local constraints changed the interface"
    );
    assert_ne!(before.2, after.2, "runtime descriptor table did not change");
    assert_ne!(before.3, after.3, "owning capsule did not change");
    assert_eq!(before.4, after.4, "unrelated capsule changed");
}

#[test]
fn one_test_idle_edit_preserves_interface_and_unrelated_capsule() {
    let render = |check_idle: bool| {
        let idle = if check_idle {
            "        assert driver.idle(1)\n"
        } else {
            ""
        };
        let source = parse_source(&format!(
            r#"transactor PlainDriver
    dut : Top
    when active
        function drive(value: uint<8>)
            dut.d = value
        end drive
    end when
end transactor PlainDriver

testbench IdleTb
    dut : Top
    driver : PlainDriver active
end testbench IdleTb

impl Alpha for IdleTb
    run
        driver.drive(7)
{idle}    end run
end impl Alpha

impl Bravo for IdleTb
    run
        driver.drive(9)
    end run
end impl Bravo
"#
        ))
        .expect("source parses");
        let program = lower::lower_program(&source).expect("source lowers");
        verify::verify_program(&program).expect("program verifies");
        let mut opts = cpp_tb::EmitOpts::default();
        set_dut_interface(
            &mut opts,
            "Top",
            vec![ir::passes::dut_access::DutInterfacePort::new(
                "d",
                ir::PortDirection::In,
                8,
                None,
                None,
            )],
        );
        let plan = tbir::common::plan_common_tests(&program, &opts, "idle_locality__")
            .expect("common plan");
        let publication = plan.publication().expect("publication");
        (
            publication.interface_abi().to_string(),
            publication.interface().to_string(),
            publication.runtime().expect("runtime"),
            publication.capsule(&plan.capsules()[0]).expect("alpha"),
            publication.capsule(&plan.capsules()[1]).expect("bravo"),
        )
    };

    let before = render(false);
    let after = render(true);
    assert_eq!(before.0, after.0, "test-local idle changed interface ABI");
    assert_eq!(before.1, after.1, "test-local idle changed interface");
    assert_eq!(before.2, after.2, "test-local idle changed runtime");
    assert_ne!(before.3, after.3, "owning capsule did not change");
    assert_eq!(before.4, after.4, "unrelated capsule changed");
}

#[test]
fn one_test_dut_access_edit_preserves_shared_and_unrelated_artifacts() {
    let render = |port: &str| {
        let source = parse_source(&format!(
            r#"test Alpha
    let dut : Top
    run
        dut.{port} = 1
    end run
end test Alpha

test Bravo
    let dut : Top
    run
        log(info, "unchanged")
    end run
end test Bravo
"#
        ))
        .expect("source parses");
        let program = lower::lower_program(&source).expect("source lowers");
        verify::verify_program(&program).expect("program verifies");
        let mut opts = cpp_tb::EmitOpts::default();
        set_dut_interface(
            &mut opts,
            "Top",
            vec![
                ir::passes::dut_access::DutInterfacePort::new(
                    "a",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
                ir::passes::dut_access::DutInterfacePort::new(
                    "b",
                    ir::PortDirection::In,
                    8,
                    None,
                    None,
                ),
            ],
        );
        let plan = tbir::common::plan_common_tests(&program, &opts, "dut_access_locality__")
            .expect("common plan");
        let publication = plan.publication().expect("publication");
        (
            plan.build_profile().to_string(),
            publication.interface().to_string(),
            publication.runtime().expect("runtime"),
            publication.capsule(&plan.capsules()[0]).expect("alpha"),
            publication.capsule(&plan.capsules()[1]).expect("bravo"),
        )
    };

    let before = render("a");
    let after = render("b");
    assert_eq!(
        before.0, after.0,
        "test-local DUT access changed build identity"
    );
    assert_eq!(before.1, after.1, "test-local DUT access changed interface");
    assert_eq!(before.2, after.2, "test-local DUT access changed runtime");
    assert_ne!(before.3, after.3, "owning capsule did not change");
    assert_eq!(before.4, after.4, "unrelated capsule changed");
}

#[test]
fn inserting_a_randomize_site_preserves_unrelated_problem_and_capsule_identity() {
    let render = |extra_site: bool| {
        let extra = if extra_site {
            "        let second : Req\n        randomize(second)\n"
        } else {
            ""
        };
        let source = harc::parser::parse_source_named(
            "stable_randomize_ids.harc",
            &format!(
                r#"transaction Req
    value : uint<8> with [unique within test]
    keep value in [1..7]
end transaction Req

test Alpha
    let dut : Top
    run
        let first : Req
        randomize(first)
{extra}    end run
end test Alpha

test Bravo
    let dut : Top
    run
        let target : Req
        randomize(target)
        log(info, "BRAVO=${{target.value}}")
    end run
end test Bravo
"#
            ),
        )
        .expect("source parses");
        let table = harc::solver::problem_table::build_typed_solver_problem_table(&source);
        let bravo_id = table
            .entries
            .iter()
            .find_map(|entry| match (&entry.source, &entry.build) {
                (
                    harc::solver::problem_table::TypedSolverProblemSource::RandomizeSite {
                        context,
                        ..
                    },
                    harc::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. },
                ) if context.starts_with("Bravo:") => Some(typed.problem_id.0),
                _ => None,
            })
            .expect("Bravo problem id");
        let merged = merge::merge_for_sim(vec![source], None).expect("source merges");
        let program = lower::lower_program(&merged).expect("source lowers");
        verify::verify_program(&program).expect("program verifies");
        let mut opts = cpp_tb::EmitOpts::default();
        set_dut_interface(&mut opts, "Top", vec![]);
        let plan = tbir::common::plan_common_tests_with_source(
            &program,
            &merged,
            &opts,
            "stable_randomize_ids__",
        )
        .expect("common plan");
        let publication = plan.publication().expect("publication");
        (
            bravo_id,
            publication.interface_abi().to_string(),
            publication.interface().to_string(),
            publication.runtime().expect("runtime"),
            publication.capsule(&plan.capsules()[0]).expect("alpha"),
            publication.capsule(&plan.capsules()[1]).expect("bravo"),
        )
    };

    let before = render(false);
    let after = render(true);
    assert_eq!(before.0, after.0, "Bravo's problem identity was renumbered");
    assert_eq!(before.1, after.1, "interface ABI changed");
    assert_eq!(before.2, after.2, "interface bytes changed");
    assert_ne!(before.3, after.3, "runtime descriptor table did not change");
    assert_ne!(before.4, after.4, "Alpha capsule did not change");
    assert_eq!(before.5, after.5, "Bravo capsule changed");
}

#[test]
fn extern_signatures_are_part_of_the_common_abi_anchor() {
    let render = |two_args: bool| {
        let declaration = if two_args {
            "extern function ref_value(x: uint<8>, y: uint<8>) -> uint<8>"
        } else {
            "extern function ref_value(x: uint<8>) -> uint<8>"
        };
        let call = if two_args {
            "ref_value(1, 2)"
        } else {
            "ref_value(1)"
        };
        let source = parse_source(&format!(
            "{declaration}\n\n\
             test ExternAbi\n\
                 let dut : Top\n\
                 run\n\
                     let value = {call}\n\
                     assert value >= 0\n\
                 end run\n\
             end test ExternAbi\n"
        ))
        .expect("source parses");
        let merged = merge::merge_for_sim(vec![source], None).expect("source merges");
        let program = lower::lower_program(&merged).expect("source lowers");
        verify::verify_program(&program).expect("program verifies");
        let mut opts = cpp_tb::EmitOpts::default();
        set_dut_interface(&mut opts, "Top", vec![]);
        let plan =
            tbir::common::plan_common_tests_with_source(&program, &merged, &opts, "extern_abi__")
                .expect("common plan");
        let publication = plan.publication().expect("publication");
        (
            publication.interface_abi().to_string(),
            publication.interface().to_string(),
        )
    };

    let one = render(false);
    let two = render(true);
    let begin = one.1.find("// === iface-begin ===").unwrap();
    let signature = one.1.find("uint64_t ref_value(uint64_t x);").unwrap();
    let end = one.1.find("// === iface-end ===").unwrap();
    assert!(begin < signature && signature < end, "{}", one.1);
    assert!(one.1[begin..end].contains("// harc-extern-signatures:"));
    assert_ne!(one.0, two.0, "extern signature drift did not change ABI");
}

#[test]
fn common_plan_owns_statement_lifecycle_cells_without_mutable_static_state() {
    let source = parse_source(STATEMENT_RUNTIME_CELLS_SRC).expect("runtime-cell source parses");
    let program = lower::lower_program(&source).expect("runtime-cell source lowers");
    verify::verify_program(&program).expect("runtime-cell program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_common_reg_interface(&mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("ticket-06 lifecycle cells are valid common-layout input");
    let publication = plan.publication().expect("publication plans");
    let interface = publication.interface().to_string();
    let runtime = publication.runtime().expect("runtime emits");
    let capsule = publication
        .capsule(&plan.capsules()[0])
        .expect("capsule emits");
    let registry = publication.registry();
    let self_contained = tbir::emit(&program, &source, &opts).expect("self-contained emits");

    assert!(
        capsule.contains("struct HarcRuntimeCells_RuntimeCells"),
        "test-owned persistent cells need one explicit capsule-local state block:\n{capsule}"
    );
    assert!(
        !interface.contains("HarcRuntimeCells_RuntimeCells"),
        "a test-local property must not perturb the suite interface ABI:\n{interface}"
    );
    assert!(
        self_contained.contains("struct HarcRuntimeCells_t0"),
        "self-contained emission must consume the same runtime-cell plan:\n{self_contained}"
    );
    for (name, artifact) in [
        ("interface", &interface),
        ("runtime", &runtime),
        ("capsule", &capsule),
        ("registry", &registry),
    ] {
        let mutable = mutable_namespace_state_declarations(artifact);
        assert!(
            mutable.is_empty(),
            "{name} carries mutable namespace/TLS declarations: {mutable:#?}\n{artifact}"
        );
    }
    for forbidden in [
        "static int64_t _p_",
        "static bool _p_",
        "static int64_t _cyc_",
        "static bool _cyc_",
        "static int64_t _tbper_",
        "static bool _tbcyc_",
    ] {
        assert!(
            !self_contained.contains(forbidden),
            "self-contained lifecycle state must be per run, not `{forbidden}`:\n{self_contained}"
        );
    }
}

#[test]
fn runtime_cell_plan_is_semantic_deterministic_and_exhaustive_for_statement_state() {
    use harc::ir::passes::runtime_cells::{
        analyze, CallbackRegistryKind, RuntimeCellInitializer, RuntimeCellKind, RuntimeCellOwner,
        RuntimeCellRegistrationPhase, RuntimeCellStorage, TemporalCheck,
    };

    let source = parse_source(STATEMENT_RUNTIME_CELLS_SRC).expect("runtime-cell source parses");
    let program = lower::lower_program(&source).expect("runtime-cell source lowers");
    verify::verify_program(&program).expect("runtime-cell program verifies");
    let first = analyze(&program).expect("runtime cells plan");
    let second = analyze(&program).expect("runtime cells re-plan");
    assert_eq!(
        first, second,
        "planning must not depend on mutable iteration state"
    );
    for cell in first.cells() {
        assert_eq!(cell.id().owner(), cell.owner());
        assert_eq!(cell.id().site(), cell.site());
        assert!(!cell.symbol().is_empty());
    }

    let runtime = RuntimeCellOwner::Runtime;
    assert!(first.find(&runtime, &RuntimeCellKind::Rng).is_some());
    for registry in [
        CallbackRegistryKind::Checker,
        CallbackRegistryKind::PostEval,
        CallbackRegistryKind::AutomaticCoverageReport,
    ] {
        assert!(
            first
                .find(&runtime, &RuntimeCellKind::CallbackRegistry(registry))
                .is_some(),
            "missing runtime callback registry {registry:?}"
        );
    }

    let owner = RuntimeCellOwner::Test {
        test: program.tests[0].id,
        name: program.tests[0].name.clone(),
    };
    let owned = first.for_owner(&owner).collect::<Vec<_>>();
    assert_eq!(owned.len(), 8, "unexpected test-owned cells: {owned:#?}");
    assert!(owned.iter().any(|cell| matches!(
        cell.kind(),
        RuntimeCellKind::PropertyImplicationPrevious { property } if property.0 == 0
    )));
    assert!(owned.iter().any(|cell| matches!(
        cell.kind(),
        RuntimeCellKind::TemporalPrevious {
            check: TemporalCheck::Property(property),
            slot: 0,
        } if property.0 == 1
    )));
    assert!(owned.iter().any(|cell| matches!(
        cell.kind(),
        RuntimeCellKind::StatementEdgePrevious { handler } if handler.0 == 0
    )));
    assert!(owned.iter().any(|cell| matches!(
        cell.kind(),
        RuntimeCellKind::StatementPeriodicLast { handler } if handler.0 == 1
    )));
    assert!(owned.iter().any(|cell| matches!(
        cell.kind(),
        RuntimeCellKind::LocalEventSubscribers { member, .. }
            if *member == ir::TestCallableMember::Run
    )));
    for cell in &owned {
        match cell.kind() {
            RuntimeCellKind::TemporalPrevious { .. } => {
                assert_eq!(cell.storage(), RuntimeCellStorage::TemporalValue);
                assert_eq!(cell.initializer(), RuntimeCellInitializer::Zero);
            }
            RuntimeCellKind::PropertyImplicationPrevious { .. }
            | RuntimeCellKind::StatementEdgePrevious { .. } => {
                assert_eq!(cell.storage(), RuntimeCellStorage::Latch);
                assert_eq!(cell.initializer(), RuntimeCellInitializer::False);
            }
            RuntimeCellKind::StatementPeriodicLast { .. } => {
                assert_eq!(cell.storage(), RuntimeCellStorage::CycleStamp);
                assert_eq!(cell.initializer(), RuntimeCellInitializer::Zero);
            }
            RuntimeCellKind::LocalEventSubscribers { .. } => {
                assert_eq!(cell.storage(), RuntimeCellStorage::EventRegistry);
                assert_eq!(cell.initializer(), RuntimeCellInitializer::Empty);
            }
            RuntimeCellKind::TestHookClosure { .. } => {
                assert_eq!(cell.storage(), RuntimeCellStorage::CallbackBody);
                assert_eq!(cell.initializer(), RuntimeCellInitializer::Empty);
                assert_eq!(cell.registration(), RuntimeCellRegistrationPhase::TestSetup);
                continue;
            }
            other => panic!("unexpected statement cell {other:?}"),
        }
        assert_eq!(
            cell.registration(),
            RuntimeCellRegistrationPhase::StatementExecution
        );
    }
}

#[test]
fn runtime_cell_plan_rejects_missing_duplicate_and_misowned_state() {
    use harc::ir::passes::runtime_cells::analyze;

    let source = parse_source(STATEMENT_RUNTIME_CELLS_SRC).expect("runtime-cell source parses");
    let program = lower::lower_program(&source).expect("runtime-cell source lowers");
    verify::verify_program(&program).expect("runtime-cell program verifies");

    let mut duplicate = program.clone();
    let run = duplicate.tests[0].run;
    let property = duplicate.functions[run.index()]
        .blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .find(|stmt| matches!(stmt, ir::Stmt::PropertyCheck(_)))
        .cloned()
        .expect("property registration");
    duplicate.functions[run.index()].blocks[0]
        .stmts
        .push(property);
    let error = analyze(&duplicate).expect_err("duplicate registration must fail");
    assert!(error.0.contains("more than one runtime owner"), "{error}");

    let mut missing = program.clone();
    missing.property_checks.clear();
    let error = analyze(&missing).expect_err("missing property schema must fail");
    assert!(error.0.contains("missing property"), "{error}");

    let mut wrong_service = program.clone();
    wrong_service.cycle_handlers[0].function = ir::FunctionId(u32::MAX);
    let error = analyze(&wrong_service).expect_err("missing handler body must fail");
    assert!(error.0.contains("statement cycle handler"), "{error}");
    assert!(error.0.contains("missing fn"), "{error}");

    let mut wrong_test = program;
    wrong_test.tests[0].id = ir::TestId(7);
    let error = analyze(&wrong_test).expect_err("mismatched test identity must fail");
    assert!(error.0.contains("mismatched id"), "{error}");

    let source = parse_source(STATEMENT_RUNTIME_CELLS_SRC).expect("runtime-cell source parses");
    let mut duplicate_test = lower::lower_program(&source).expect("runtime-cell source lowers");
    duplicate_test.tests.push(duplicate_test.tests[0].clone());
    let error = analyze(&duplicate_test).expect_err("duplicate test identity must fail");
    assert!(error.0.contains("duplicate test identity"), "{error}");

    let component_source = parse_source(
        r#"
agent TimedCell
    pulse : out event<uint<8>>
    on 1 cycles
        log(info, "tick")
    end on
    watchdog
        period 2 cycles
        max_idle 100 cycles
        log(info, "watchdog")
    end watchdog
end agent TimedCell
test Timed
    let dut : CommonReg
    let cell : TimedCell
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test Timed
"#,
    )
    .expect("component lifecycle source parses");
    let mut component_program =
        lower::lower_program(&component_source).expect("component lifecycle source lowers");
    verify::verify_program(&component_program).expect("component lifecycle source verifies");
    let component_plan = analyze(&component_program).expect("component runtime cells plan");
    let component_owner = ir::passes::runtime_cells::RuntimeCellOwner::ComponentInstance {
        component: ir::ComponentId(0),
        name: "TimedCell".to_string(),
    };
    let component_cells = component_plan
        .for_owner(&component_owner)
        .collect::<Vec<_>>();
    assert_eq!(component_cells.len(), 5, "{component_cells:#?}");
    assert!(component_cells.iter().any(|cell| matches!(
        cell.kind(),
        ir::passes::runtime_cells::RuntimeCellKind::ComponentEventSubscribers { .. }
    )));
    assert!(component_cells.iter().any(|cell| matches!(
        cell.kind(),
        ir::passes::runtime_cells::RuntimeCellKind::ComponentPeriodicLast { .. }
    )));
    assert!(component_cells.iter().any(|cell| matches!(
        cell.kind(),
        ir::passes::runtime_cells::RuntimeCellKind::ComponentWatchdogLast { .. }
    )));
    assert!(component_cells.iter().all(|cell| {
        cell.registration()
            == ir::passes::runtime_cells::RuntimeCellRegistrationPhase::ComponentSetup
    }));
    component_program.components[0].periodic_handlers[0].function = ir::FunctionId(u32::MAX);
    let error = analyze(&component_program).expect_err("missing component handler must fail");
    assert!(error.0.contains("periodic handler"), "{error}");
    assert!(error.0.contains("missing fn"), "{error}");
}

#[test]
fn runtime_cell_plan_owns_all_emitted_heartbeat_and_coverage_hook_storage() {
    use harc::ir::passes::runtime_cells::{
        analyze, ComponentHeartbeat, RuntimeCellKind, RuntimeCellOwner, RuntimeHookSide,
    };

    let source = parse_source(
        r#"scoreboard Counts
    total : uint<16> default 0
end scoreboard Counts

transactor Driver
    last : uint<8> default 0
    hookable step(value: uint<8>)
        last = value
    end step
end transactor Driver

testbench OwnedStateTb
    dut : CommonReg
    counts : Counts
    driver : Driver
    total : uint<16> default 0
end testbench OwnedStateTb

impl OwnedState for OwnedStateTb
    clock clk = 10ns
    run
        driver.step(3)
        wait 1 cycle
    end run
end impl OwnedState"#,
    )
    .expect("owned-state source parses");
    let program = lower::lower_program(&source).expect("owned-state source lowers");
    verify::verify_program(&program).expect("owned-state source verifies");
    let plan = analyze(&program).expect("owned-state runtime plan");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);
    let common = tbir::common::plan_common_tests(&program, &opts, "owned__")
        .expect("owned-state common plan");
    let publication = common.publication().expect("owned publication plans");
    let interface = publication.interface().to_string();
    let runtime = publication.runtime().expect("owned runtime");
    let capsule = publication
        .capsule(&common.capsules()[0])
        .expect("owned capsule");
    let registry = publication.registry();
    for (name, artifact) in [
        ("interface", &interface),
        ("runtime", &runtime),
        ("capsule", &capsule),
        ("registry", &registry),
    ] {
        let mutable = mutable_namespace_state_declarations(artifact);
        assert!(
            mutable.is_empty(),
            "{name} carries mutable namespace/TLS declarations: {mutable:#?}\n{artifact}"
        );
    }
    let struct_body = |name: &str| {
        interface
            .split(&format!("struct {name} {{"))
            .nth(1)
            .and_then(|tail| tail.split("};").next())
            .unwrap_or_else(|| panic!("missing `{name}` in interface:\n{interface}"))
    };
    let tb_body = struct_body("OwnedStateTb");
    let scoreboard_body = struct_body("Counts");
    let transactor_body = struct_body("Driver");
    assert_eq!(tb_body.matches("_last_in_cycle").count(), 1, "{tb_body}");
    assert_eq!(tb_body.matches("_last_out_cycle").count(), 1, "{tb_body}");
    assert_eq!(
        scoreboard_body.matches("_last_in_cycle").count(),
        1,
        "{scoreboard_body}"
    );
    assert_eq!(
        scoreboard_body.matches("_last_out_cycle").count(),
        1,
        "{scoreboard_body}"
    );
    assert_eq!(
        transactor_body.matches("_last_in_cycle").count(),
        1,
        "{transactor_body}"
    );
    assert_eq!(
        transactor_body.matches("_last_out_cycle").count(),
        1,
        "{transactor_body}"
    );
    assert_eq!(
        transactor_body.matches("_harc_hook_step_pre").count(),
        1,
        "{transactor_body}"
    );
    assert_eq!(
        transactor_body.matches("_harc_hook_step_post").count(),
        1,
        "{transactor_body}"
    );

    let testbench = RuntimeCellOwner::Testbench {
        testbench: ir::TestbenchId(0),
        name: "OwnedStateTb".to_string(),
    };
    let scoreboard = RuntimeCellOwner::ScoreboardInstance {
        scoreboard: ir::ScoreboardId(0),
        name: "Counts".to_string(),
    };
    for heartbeat in [ComponentHeartbeat::Input, ComponentHeartbeat::Output] {
        assert!(plan
            .find(&testbench, &RuntimeCellKind::TestbenchHeartbeat(heartbeat))
            .is_some());
        assert!(plan
            .find(
                &scoreboard,
                &RuntimeCellKind::ScoreboardHeartbeat(heartbeat)
            )
            .is_some());
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let transactor_source =
        std::fs::read_to_string(root.join("tests/fixtures/tlm_target_thread_test.harc"))
            .expect("read target-transactor fixture");
    let merged = merge::merge_for_sim(
        vec![parse_source(&transactor_source).expect("transactor source parses")],
        None,
    )
    .expect("merge transactor source");
    let transactor_program =
        lower::lower_program(&merged).expect("stateful-transactor source lowers");
    let transactor_id = transactor_program
        .transactors
        .iter()
        .position(|schema| schema.name == "TlmMemTarget")
        .map(|index| ir::TransactorId(index as u32))
        .expect("TlmMemTarget schema");
    let transactor_plan = analyze(&transactor_program).expect("transactor runtime plan");
    let transactor = RuntimeCellOwner::TransactorInstance {
        transactor: transactor_id,
        name: "TlmMemTarget".to_string(),
    };
    for heartbeat in [ComponentHeartbeat::Input, ComponentHeartbeat::Output] {
        assert!(
            transactor_plan
                .find(
                    &transactor,
                    &RuntimeCellKind::TransactorHeartbeat(heartbeat)
                )
                .is_some(),
            "missing planned {heartbeat:?} transactor heartbeat"
        );
    }

    let hook_source = std::fs::read_to_string(
        root.join("tests/fixtures/transactor_hook_exact_instance_test.harc"),
    )
    .expect("read exact-instance transactor-hook fixture");
    let hook_program =
        lower::lower_program(&parse_source(&hook_source).expect("hook source parses"))
            .expect("hook source lowers");
    let hook_transactor_id = hook_program
        .transactors
        .iter()
        .position(|schema| schema.name == "HookedDriver")
        .map(|index| ir::TransactorId(index as u32))
        .expect("HookedDriver schema");
    let hook_transactor = &hook_program.transactors[hook_transactor_id.index()];
    let hook_method = hook_transactor.method("go").expect("hookable go method");
    let hook_plan = analyze(&hook_program).expect("transactor-hook runtime plan");
    let hook_owner = RuntimeCellOwner::TransactorInstance {
        transactor: hook_transactor_id,
        name: hook_transactor.name.clone(),
    };
    for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
        assert!(
            hook_plan
                .find(
                    &hook_owner,
                    &RuntimeCellKind::HookSubscribers {
                        hook: harc::ir::passes::runtime_cells::HookOwner::Transactor {
                            function: hook_method.function,
                        },
                        side,
                    }
                )
                .is_some(),
            "missing planned {side:?} exact-instance transactor hook registry"
        );
    }

    let fixture = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/component_early_return_post_cover_test.harc"),
    )
    .expect("read component coverage-hook fixture");
    let source = parse_source(&fixture).expect("coverage-hook source parses");
    let program = lower::lower_program(&source).expect("coverage-hook source lowers");
    verify::verify_program(&program).expect("coverage-hook source verifies");
    let plan = analyze(&program).expect("coverage-hook runtime plan");
    let component = &program.components[0];
    let owner = RuntimeCellOwner::ComponentInstance {
        component: ir::ComponentId(0),
        name: component.name.clone(),
    };
    for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
        assert!(
            plan.find(
                &owner,
                &RuntimeCellKind::ComponentCoverageHookSubscribers {
                    component: ir::ComponentId(0),
                    member: ir::ComponentCallableId(0),
                    side,
                }
            )
            .is_some(),
            "missing planned {side:?} coverage-hook registry: {:#?}",
            plan.cells()
        );
    }
}

#[test]
fn common_plan_shares_unbound_active_transactor_methods() {
    let source = parse_source(
        r#"transactor Driver
    dut : CommonReg
    last : uint<8> default 0
    hookable reset_state()
        last = 0
    end reset_state
    when active
        hookable step(value: uint<8>)
            dut.d = value
            last = value
        end step
    end when
end transactor Driver

testbench DriverTb
    dut : CommonReg
    driver : Driver active
    function setup_tb()
        driver.reset_state()
        driver.last = 1
    end function setup_tb
end testbench DriverTb

impl UsesDriver for DriverTb
    clock clk = 10ns
    run
        setup_tb()
        driver.step(3)
        wait 1 cycle
    end run
end impl UsesDriver"#,
    )
    .expect("active transactor source parses");
    let program = lower::lower_program(&source).expect("active transactor source lowers");
    verify::verify_program(&program).expect("active transactor source verifies");
    let catalog = harc::ir::passes::callable_placement::analyze(&program)
        .expect("active transactor placement analyzes");
    for method in ["Driver_reset_state", "Driver_step"] {
        let callable = catalog
            .callables()
            .iter()
            .find(|callable| callable.name == method)
            .unwrap_or_else(|| panic!("missing callable `{method}`: {catalog:#?}"));
        assert_eq!(
            callable.placement,
            harc::ir::passes::callable_placement::CallablePlacement::Common,
            "{callable:#?}"
        );
    }

    let mut opts = cpp_tb::EmitOpts::default();
    set_common_reg_interface(&mut opts);
    let common = tbir::common::plan_common_tests(&program, &opts, "driver__")
        .expect("active transactor common plan");
    let publication = common.publication().expect("active transactor publication");
    let interface = publication.interface();
    let runtime = publication.runtime().expect("active transactor runtime");
    let capsule = publication
        .capsule(&common.capsules()[0])
        .expect("active transactor capsule");
    for method in ["reset_state", "step"] {
        let declaration = format!("void Driver_{method}(");
        assert!(interface.contains(&declaration), "{interface}");
        assert!(runtime.contains(&declaration), "{runtime}");
        assert!(!capsule.contains(&declaration), "{capsule}");
    }
    assert!(
        interface.contains(
            "DriverTb_setup_tb(HarcTestContext& ctx, DriverTb& _tb, struct _Driver_state& _harc_tb_transactor_state_driver)"
        ),
        "{interface}"
    );
    assert!(
        runtime.contains("_harc_tb_transactor_state_driver.last = 1;"),
        "{runtime}"
    );
    assert!(
        capsule.contains(
            "DriverTb_setup_tb(ctx, _harc_run_state._harc_testbench, _harc_run_state.driver)"
        ),
        "{capsule}"
    );
}

#[test]
fn common_testbench_method_abi_excludes_implementation_local_transactors() {
    let source = parse_source(
        r#"transactor SharedDriver
    dut : CommonReg
    value : uint<8> default 0
    when active
        hookable set(value_in: uint<8>)
            value = value_in
        end set
    end when
end transactor SharedDriver

transactor AlphaLocal
    dut : CommonReg
    value : uint<16> default 0
    when active
        function touch()
            value = value + 1
        end touch
    end when
end transactor AlphaLocal

transactor BetaLocal
    dut : CommonReg
    value : uint<32> default 0
    when active
        function touch()
            value = value + 1
        end touch
    end when
end transactor BetaLocal

testbench SharedDriverTb
    dut : CommonReg
    driver : SharedDriver active
    function set_shared(value: uint<8>)
        driver.set(value)
    end function set_shared
    function configure(value: uint<8>)
        set_shared(value)
    end function configure
end testbench SharedDriverTb

impl DriverAlpha for SharedDriverTb
    let alpha_local : AlphaLocal active
    clock clk = 10ns
    run
        configure(1)
        wait 1 cycle
    end run
end impl DriverAlpha

impl DriverBeta for SharedDriverTb
    let beta_local : BetaLocal active
    clock clk = 10ns
    run
        configure(2)
        wait 1 cycle
    end run
end impl DriverBeta"#,
    )
    .expect("implementation-local transactor source parses");
    let program =
        lower::lower_program(&source).expect("implementation-local transactor source lowers");
    verify::verify_program(&program).expect("implementation-local transactor source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    set_common_reg_interface(&mut opts);
    let common = tbir::common::plan_common_tests(&program, &opts, "driver_local__")
        .expect("implementation-local transactors stay outside the shared method ABI");
    let publication = common.publication().expect("common publication");
    let interface = publication.interface();
    let runtime = publication.runtime().expect("common runtime");
    assert!(
        interface.contains(
            "SharedDriverTb_set_shared(HarcTestContext& ctx, SharedDriverTb& _tb, struct _SharedDriver_state& _harc_tb_transactor_state_driver, uint64_t value)"
        ),
        "{interface}"
    );
    assert!(
        interface.contains(
            "SharedDriverTb_configure(HarcTestContext& ctx, SharedDriverTb& _tb, struct _SharedDriver_state& _harc_tb_transactor_state_driver, uint64_t value)"
        ),
        "{interface}"
    );
    for local in ["alpha_local", "beta_local"] {
        assert!(!interface.contains(&format!("_harc_tb_transactor_state_{local}")));
        assert!(!runtime.contains(&format!("_harc_tb_transactor_state_{local}")));
    }
    for index in 0..common.capsules().len() {
        let capsule = publication
            .capsule(&common.capsules()[index])
            .expect("common capsule");
        assert!(
            capsule.contains("SharedDriverTb_configure(ctx, _harc_run_state._harc_testbench, _harc_run_state.driver, "),
            "{capsule}"
        );
    }
}

#[test]
fn common_capsule_supports_test_local_component_values() {
    let source = parse_source(
        r#"transactor Model
    value : uint<8> default 0
    function set(value_in: uint<8>)
        value = value_in
    end set
    hookable get() -> uint<8>
        return value
    end get
end transactor Model

test LocalModel
    let dut : CommonReg
    clock clk = 10ns
    run
        let model : Model passive
        model.set(7)
        assert model.get() == 7 else fail("component-local state")
        wait 1 cycle
    end run
end test LocalModel"#,
    )
    .expect("component-local source parses");
    let program = lower::lower_program(&source).expect("component-local source lowers");
    verify::verify_program(&program).expect("component-local source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    set_clock_interface_for_program(&program, &mut opts);
    let common = tbir::common::plan_common_tests(&program, &opts, "local__")
        .expect("component-local common plan");
    let publication = common.publication().expect("component-local publication");
    let runtime = publication.runtime().expect("component-local runtime");
    let capsule = publication
        .capsule(&common.capsules()[0])
        .expect("component-local capsule");
    assert!(runtime.contains("void Model_set("), "{runtime}");
    assert!(runtime.contains("uint64_t Model_get("), "{runtime}");
    assert!(capsule.contains("Model model{};"), "{capsule}");
}

#[test]
fn common_testbench_method_uses_explicit_component_receiver() {
    let source = parse_source(
        r#"agent Counter
    value : uint<8> default 0
    hookable clear()
        value = 0
    end clear
end agent Counter

testbench ComponentTb
    dut : CommonReg
    counter : Counter
    function setup_tb()
        counter.clear()
    end function setup_tb
end testbench ComponentTb

impl UsesComponent for ComponentTb
    clock clk = 10ns
    run
        setup_tb()
        wait 1 cycle
    end run
end impl UsesComponent"#,
    )
    .expect("component-receiver source parses");
    let program = lower::lower_program(&source).expect("component-receiver source lowers");
    verify::verify_program(&program).expect("component-receiver source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    set_common_reg_interface(&mut opts);
    let common = tbir::common::plan_common_tests(&program, &opts, "component__")
        .expect("component-receiver common plan");
    let publication = common
        .publication()
        .expect("component-receiver publication");
    let interface = publication.interface();
    let runtime = publication.runtime().expect("component-receiver runtime");
    let capsule = publication
        .capsule(&common.capsules()[0])
        .expect("component-receiver capsule");
    assert!(
        interface.contains(
            "ComponentTb_setup_tb(HarcTestContext& ctx, ComponentTb& _tb, Counter& _harc_tb_component_counter)"
        ),
        "{interface}"
    );
    assert!(
        runtime.contains("Counter_clear(ctx, _harc_tb_component_counter);"),
        "{runtime}"
    );
    assert!(
        capsule.contains(
            "ComponentTb_setup_tb(ctx, _harc_run_state._harc_testbench, _harc_run_state.counter)"
        ),
        "{capsule}"
    );
}

#[test]
fn common_event_runtime_bindings_fail_closed_on_corrupted_ir() {
    let source = parse_source(STATEMENT_RUNTIME_CELLS_SRC).expect("event source parses");
    let program = lower::lower_program(&source).expect("event source lowers");
    verify::verify_program(&program).expect("event source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_common_reg_interface(&mut opts);

    let mut missing_handler = program.clone();
    let run = missing_handler.tests[0].run;
    let subscription = missing_handler.functions[run.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find(|stmt| matches!(stmt, ir::Stmt::EventSubscribe { .. }))
        .expect("event subscription");
    let ir::Stmt::EventSubscribe { handler, .. } = subscription else {
        unreachable!()
    };
    *handler = ir::FunctionId(u32::MAX);
    let error = tbir::common::plan_common_tests(&missing_handler, &opts, "suite__")
        .expect_err("missing event handler must fail before rendering");
    assert!(
        error.0.contains("event subscription") && error.0.contains("missing fn"),
        "{error}"
    );

    let mut wrong_channel = program;
    let run = wrong_channel.tests[0].run;
    let event = wrong_channel.functions[run.index()]
        .locals
        .iter()
        .position(|local| matches!(local.ty, ir::IrType::Event(_)))
        .expect("event local");
    wrong_channel.functions[run.index()].locals[event].ty = ir::IrType::UInt(Some(8));
    let error = tbir::common::plan_common_tests(&wrong_channel, &opts, "suite__")
        .expect_err("non-event subscription target must fail before rendering");
    assert!(error.0.contains("non-event local"), "{error}");
}

#[test]
fn test_hook_site_identity_rejects_same_abi_body_swaps() {
    let source = parse_source(TEST_HOOK_IDENTITY_SRC).expect("hook-identity source parses");
    let program = lower::lower_program(&source).expect("hook-identity source lowers");
    verify::verify_program(&program).expect("hook-identity source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_common_reg_interface(&mut opts);
    tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("hook-identity source plans");

    fn hook_ids(
        program: &ir::TbProgram,
        family: fn(&ir::TestHookMember) -> bool,
    ) -> Vec<ir::FunctionId> {
        program
            .functions
            .iter()
            .filter_map(|function| match &function.kind {
                ir::FunctionKind::TestHook { member } if family(member) => Some(function.id),
                _ => None,
            })
            .collect()
    }

    fn swap_members(program: &mut ir::TbProgram, first: ir::FunctionId, second: ir::FunctionId) {
        let first_kind = program.functions[first.index()].kind.clone();
        let second_kind = program.functions[second.index()].kind.clone();
        program.functions[first.index()].kind = second_kind;
        program.functions[second.index()].kind = first_kind;
    }

    let assert_rejected = |program: ir::TbProgram, family: &str| {
        let errors = verify::verify_program(&program).expect_err("swapped hook identity verifies");
        assert!(
            format!("{errors:?}").contains("test-hook"),
            "{family}: {errors:?}"
        );
        let error = tbir::common::plan_common_tests(&program, &opts, "suite__")
            .expect_err("swapped hook identity plans");
        assert!(
            error.0.contains("owner")
                || error.0.contains("identity")
                || error.0.contains("invalid body"),
            "{family}: {error}"
        );
    };

    let mut event = program.clone();
    let ids = hook_ids(&event, |member| {
        matches!(member, ir::TestHookMember::EventSubscription(_))
    });
    assert_eq!(ids.len(), 2);
    for function in &mut event.functions {
        for block in &mut function.blocks {
            for stmt in &mut block.stmts {
                if let ir::Stmt::EventSubscribe { handler, .. } = stmt {
                    *handler = if *handler == ids[0] {
                        ids[1]
                    } else if *handler == ids[1] {
                        ids[0]
                    } else {
                        *handler
                    };
                }
            }
        }
    }
    swap_members(&mut event, ids[0], ids[1]);
    assert_rejected(event, "event subscriptions");

    let mut coordinated_event = program.clone();
    let ids = hook_ids(&coordinated_event, |member| {
        matches!(member, ir::TestHookMember::EventSubscription(_))
    });
    let mut sites = coordinated_event
        .functions
        .iter()
        .flat_map(|function| &function.blocks)
        .flat_map(|block| &block.stmts)
        .filter_map(|stmt| match stmt {
            ir::Stmt::EventSubscribe { site, .. } => Some(site.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 2);
    sites.swap(0, 1);
    let mut next = 0usize;
    for function in &mut coordinated_event.functions {
        for block in &mut function.blocks {
            for stmt in &mut block.stmts {
                if let ir::Stmt::EventSubscribe { site, handler, .. } = stmt {
                    *site = sites[next].clone();
                    *handler = ids[1 - next];
                    next += 1;
                }
            }
        }
    }
    swap_members(&mut coordinated_event, ids[0], ids[1]);
    for id in ids {
        let ir::FunctionKind::TestHook { member } = &coordinated_event.functions[id.index()].kind
        else {
            unreachable!()
        };
        coordinated_event.functions[id.index()].name = member.function_name("");
    }
    let errors = verify::verify_program(&coordinated_event)
        .expect_err("coordinated hook-site reordering must not verify");
    assert!(
        format!("{errors:?}").contains("out of source order"),
        "{errors:?}"
    );

    let mut method = program.clone();
    let ids = hook_ids(&method, |member| {
        matches!(member, ir::TestHookMember::MethodSubscription(_))
    });
    assert_eq!(ids.len(), 2);
    for function in &mut method.functions {
        for block in &mut function.blocks {
            for stmt in &mut block.stmts {
                if let ir::Stmt::MethodHookSubscribe { handler, .. } = stmt {
                    *handler = if *handler == ids[0] {
                        ids[1]
                    } else if *handler == ids[1] {
                        ids[0]
                    } else {
                        *handler
                    };
                }
            }
        }
    }
    swap_members(&mut method, ids[0], ids[1]);
    assert_rejected(method, "method subscriptions");

    let mut statement = program.clone();
    let ids = hook_ids(&statement, |member| {
        matches!(member, ir::TestHookMember::StatementCycle(_))
    });
    assert_eq!(ids.len(), 2);
    for schema in &mut statement.cycle_handlers {
        schema.function = if schema.function == ids[0] {
            ids[1]
        } else if schema.function == ids[1] {
            ids[0]
        } else {
            schema.function
        };
    }
    swap_members(&mut statement, ids[0], ids[1]);
    assert_rejected(statement, "statement cycle handlers");

    let mut periodic = program.clone();
    let ids = hook_ids(&periodic, |member| {
        matches!(member, ir::TestHookMember::TestbenchPeriodic { .. })
    });
    assert_eq!(ids.len(), 2);
    periodic.testbenches[0].periodic_services.swap(0, 1);
    swap_members(&mut periodic, ids[0], ids[1]);
    assert_rejected(periodic, "testbench periodic services");

    let mut cycle = program;
    let ids = hook_ids(&cycle, |member| {
        matches!(member, ir::TestHookMember::TestbenchCycle { .. })
    });
    assert_eq!(ids.len(), 2);
    cycle.testbenches[0].cycle_services.swap(0, 1);
    swap_members(&mut cycle, ids[0], ids[1]);
    assert_rejected(cycle, "testbench cycle services");
}

#[test]
fn common_registration_sites_in_reusable_callbacks_fail_before_rendering() {
    let source = parse_source(STATEMENT_RUNTIME_CELLS_SRC).expect("callback source parses");
    let mut program = lower::lower_program(&source).expect("callback source lowers");
    verify::verify_program(&program).expect("callback source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_common_reg_interface(&mut opts);

    let run = program.tests[0].run;
    let (block, statement) = program.functions[run.index()]
        .blocks
        .iter()
        .enumerate()
        .find_map(|(block, cfg)| {
            cfg.stmts
                .iter()
                .position(|stmt| matches!(stmt, ir::Stmt::PropertyCheck(_)))
                .map(|statement| (block, statement))
        })
        .expect("property registration");
    let property = program.functions[run.index()].blocks[block]
        .stmts
        .remove(statement);
    let handler = program.functions[run.index()]
        .blocks
        .iter()
        .flat_map(|cfg| &cfg.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::EventSubscribe { handler, .. } => Some(*handler),
            _ => None,
        })
        .expect("event callback");
    program.functions[handler.index()].blocks[0]
        .stmts
        .push(property);

    let error = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("callback registration must fail during planning");
    assert!(
        error.0.contains("undefined once-versus-per-call semantics"),
        "{error}"
    );
    assert!(
        error.0.contains("ticket 06 fail-closed boundary"),
        "{error}"
    );
}

#[test]
fn common_capsule_preserves_setup_run_check_teardown_order() {
    let source = parse_source(
        r#"
test Phased
    let dut : CommonReg
    clock clk = 10ns
    setup
        log(info, "PHASE: setup")
        dut.d = 7
    end setup
    run
        log(info, "PHASE: run")
        wait 1 cycle
    end run
    check
        log(info, "PHASE: check")
        assert dut.q == 7 else fail("wrong final value")
    end check
    teardown
        log(info, "PHASE: teardown")
    end teardown
end test Phased
"#,
    )
    .expect("lifecycle source parses");
    let program = lower::lower_program(&source).expect("lifecycle source lowers");
    verify::verify_program(&program).expect("lifecycle program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([
        ("clk".to_string(), 1),
        ("d".to_string(), 8),
        ("q".to_string(), 8),
    ]);
    set_common_reg_interface(&mut opts);
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("simple lifecycle phases are ticket-03 common-layout input");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("capsule emits");

    let mut prior = 0;
    for marker in [
        "PHASE: setup",
        "PHASE: run",
        "PHASE: check",
        "PHASE: teardown",
    ] {
        let at = capsule
            .find(marker)
            .unwrap_or_else(|| panic!("missing `{marker}`:\n{capsule}"));
        assert!(
            at >= prior,
            "`{marker}` was emitted out of order:\n{capsule}"
        );
        prior = at;
    }
}

#[test]
fn common_plan_owns_shared_types_and_classifies_tseq_context() {
    let (program, opts) = shared_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");

    let type_names: Vec<_> = plan
        .shared_types()
        .iter()
        .map(|planned| planned.name())
        .collect();
    assert!(
        type_names.iter().position(|name| *name == "InnerValue")
            < type_names.iter().position(|name| *name == "OuterValue"),
        "nested records must be dependency ordered: {type_names:?}"
    );
    assert!(
        type_names.iter().position(|name| *name == "InnerValue")
            < type_names
                .iter()
                .position(|name| *name == "SharedScoreboard"),
        "record queue elements must precede scoreboards: {type_names:?}"
    );
    assert_eq!(
        plan.shared_callables()
            .iter()
            .map(|callable| (callable.name(), callable.kind()))
            .collect::<Vec<_>>(),
        vec![
            ("plus_one", tbir::common::CommonCallableKind::Helper),
            ("widen_plus_one", tbir::common::CommonCallableKind::Helper),
            ("wide_identity", tbir::common::CommonCallableKind::Helper),
            (
                "PureValues",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: false
                }
            ),
            (
                "ForwardPure",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: false
                }
            ),
            (
                "TimedValues",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: true,
                },
            ),
            (
                "ForwardTimed",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: true,
                },
            ),
            (
                "CopyScalarValues",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: false,
                },
            ),
            (
                "CopyRecordValues",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: false,
                },
            ),
            (
                "EchoRecord",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: false,
                },
            ),
            (
                "RecordValues",
                tbir::common::CommonCallableKind::Tseq {
                    needs_context: false,
                },
            ),
        ]
    );
}

#[test]
fn common_interface_is_declarative_and_runtime_owns_callable_definitions_once() {
    let (program, opts) = shared_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");
    let interface = tbir::common::emit_common_interface(&plan).expect("interface emits");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    let capsules: Vec<_> = plan
        .capsules()
        .iter()
        .map(|capsule| {
            tbir::common::emit_common_capsule(&plan, capsule.index()).expect("capsule emits")
        })
        .collect();

    assert!(interface.contains("struct InnerValue"));
    assert!(interface.contains("struct OuterValue"));
    assert!(interface.contains("harc_rt::HarcWide<5> wide = 0;"));
    assert!(interface.contains("std::array<uint64_t, 2> lanes = {};"));
    assert!(interface.contains("uint64_t tag = 3;"));
    assert!(interface.contains("std::vector<uint64_t> history{};"));
    assert!(interface.contains("struct SharedScoreboard"));
    assert!(interface.contains("struct SharedTypesTb"));
    assert!(interface.contains("harc_helper_widen_plus_one("));
    assert!(interface
        .contains("harc_rt::HarcWide<5> harc_helper_wide_identity(harc_rt::HarcWide<5> value);"));
    assert!(interface.contains("harc_tseq_PureValues(uint64_t seed);"));
    assert!(interface.contains("harc_tseq_ForwardPure(uint64_t seed);"));
    assert!(interface.contains("harc_tseq_CopyScalarValues(std::vector<uint64_t> values);"));
    assert!(interface.contains("harc_tseq_CopyRecordValues(std::vector<InnerValue> values);"));
    assert!(interface.contains("harc_tseq_EchoRecord(InnerValue value);"));
    assert!(interface.contains("std::vector<InnerValue> harc_tseq_RecordValues(uint64_t seed);"));
    assert!(interface.contains("harc_tseq_TimedValues(HarcTestContext& ctx, uint64_t seed);"));
    assert!(interface.contains("harc_tseq_ForwardTimed(HarcTestContext& ctx, uint64_t seed);"));
    assert!(!interface.contains("harc_helper_widen_plus_one(uint64_t x) {"));
    assert!(!interface.contains("harc_tseq_PureValues(uint64_t seed) {"));

    assert_eq!(runtime.matches("harc_helper_widen_plus_one(").count(), 1);
    assert_eq!(
        runtime
            .matches("std::vector<uint64_t> harc_tseq_PureValues(uint64_t seed) {")
            .count(),
        1
    );
    assert_eq!(
        runtime
            .matches("std::vector<uint64_t> harc_tseq_ForwardPure(")
            .count(),
        1
    );
    assert!(runtime.contains("harc_tseq_PureValues(seed)"));
    assert_eq!(
        runtime
            .matches("std::vector<uint64_t> harc_tseq_CopyScalarValues(")
            .count(),
        1
    );
    assert_eq!(
        runtime
            .matches("std::vector<InnerValue> harc_tseq_CopyRecordValues(")
            .count(),
        1
    );
    assert_eq!(
        runtime
            .matches("std::vector<InnerValue> harc_tseq_EchoRecord(InnerValue value) {")
            .count(),
        1
    );
    assert_eq!(
        runtime
            .matches("std::vector<uint64_t> harc_tseq_TimedValues(")
            .count(),
        1
    );
    assert_eq!(
        runtime
            .matches("std::vector<uint64_t> harc_tseq_ForwardTimed(")
            .count(),
        1
    );
    assert!(runtime.contains("harc_tseq_TimedValues(ctx, seed)"));
    assert!(runtime.contains("harc_tseq_tick(ctx)"));
    for capsule in &capsules {
        assert!(!capsule.contains("harc_helper_widen_plus_one(uint64_t x) {"));
        assert!(!capsule.contains("harc_tseq_PureValues(uint64_t seed) {"));
        assert!(!capsule.contains("harc_tseq_TimedValues(HarcTestContext& ctx"));
        assert!(!capsule.contains("harc_tseq_ForwardTimed(HarcTestContext& ctx"));
    }
    assert!(capsules[0].contains("harc_tseq_ForwardPure("));
    assert!(capsules[0].contains("harc_tseq_ForwardTimed(ctx,"));
    assert!(capsules[1].contains("harc_tseq_PureValues("));
}

#[test]
fn common_plan_rejects_a_malformed_record_dependency_cycle() {
    let (mut program, opts) = shared_program();
    let outer = harc::ir::RecordId(1);
    program.records[0].fields[0].ty = harc::ir::IrType::Record(outer);

    let error = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("malformed by-value type cycle must fail before rendering");
    assert!(error.0.contains("dependency cycle"), "{error}");
    assert!(
        error.0.contains("InnerValue -> OuterValue -> InnerValue"),
        "{error}"
    );
}

#[test]
fn common_plan_orders_records_reached_through_nested_fixed_vectors() {
    let source = parse_source(
        r#"
struct Matrix
    rows : Vec<Row, 3>
end struct Matrix

struct Row
    cells : Vec<Cell, 2>
end struct Row

struct Cell
    value : uint<8>
end struct Cell

test NestedRecordContainer
    let dut : CommonReg
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test NestedRecordContainer
"#,
    )
    .expect("nested record container source parses");
    let program = lower::lower_program(&source).expect("nested record container source lowers");
    verify::verify_program(&program).expect("nested record container program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("nested record dependency plans");
    let names: Vec<_> = plan
        .shared_types()
        .iter()
        .map(|shared| shared.name())
        .collect();
    let cell = names.iter().position(|name| *name == "Cell").unwrap();
    let row = names.iter().position(|name| *name == "Row").unwrap();
    let matrix = names.iter().position(|name| *name == "Matrix").unwrap();
    assert!(cell < row && row < matrix, "dependency order was {names:?}");
}

#[test]
fn common_layout_emits_fixed_vector_test_locals() {
    let source = parse_source(
        r#"
transaction Lane
    value : uint<8> default 0
end transaction Lane

function keep_grid(values: Vec<Vec<Lane, 2>, 3>) -> Vec<Vec<Lane, 2>, 3>
    return values
end function keep_grid

test FixedVectorLocal
    let dut : CommonReg
    clock clk = 10ns
    run
        let lanes : Vec<Vec<Lane, 2>, 3>
        let copy = keep_grid(lanes)
        wait 1 cycle
    end run
end test FixedVectorLocal
"#,
    )
    .expect("fixed-vector local source parses");
    let program = lower::lower_program(&source).expect("fixed-vector local source lowers");
    verify::verify_program(&program).expect("fixed-vector local program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("fixed-vector locals are valid common-layout input");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("capsule emits");
    assert!(capsule.contains("std::array<std::array<Lane, 2>, 3> lanes{};"));
    assert!(capsule.contains("lanes = decltype(lanes){};"));
    assert!(capsule.contains("harc_helper_keep_grid(lanes)"));

    let mut malformed = program.clone();
    let lanes = malformed
        .functions
        .iter_mut()
        .flat_map(|function| &mut function.locals)
        .find(|local| local.name == "lanes")
        .expect("lanes local exists");
    lanes.ty = harc::ir::IrType::FixedVec {
        elem: Box::new(harc::ir::IrType::FixedVec {
            elem: Box::new(harc::ir::IrType::Record(harc::ir::RecordId(999))),
            len: 2,
        }),
        len: 3,
    };
    let error = tbir::common::plan_common_tests(&malformed, &opts, "suite__")
        .expect_err("missing nested record leaf fails closed");
    assert!(error.0.contains("missing record r999"), "{error}");
}

#[test]
fn common_layout_emits_sequences_of_fixed_vectors() {
    let source = parse_source(
        r#"
test FixedVectorSequence
    let dut : CommonReg
    clock clk = 10ns
    run
        let rows : TSeq<Vec<uint<8>, 2>>
        wait 1 cycle
    end run
end test FixedVectorSequence
"#,
    )
    .expect("fixed-vector sequence source parses");
    let program = lower::lower_program(&source).expect("fixed-vector sequence source lowers");
    verify::verify_program(&program).expect("fixed-vector sequence program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("fixed-vector sequences are valid common-layout input");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("capsule emits");
    assert!(capsule.contains("std::vector<std::array<uint64_t, 2>> rows{};"));

    let mut malformed = program.clone();
    let rows = malformed
        .functions
        .iter_mut()
        .flat_map(|function| &mut function.locals)
        .find(|local| local.name == "rows")
        .expect("rows local exists");
    rows.ty = harc::ir::IrType::Seq(Box::new(harc::ir::IrType::Record(harc::ir::RecordId(0))));
    let error = tbir::common::plan_common_tests(&malformed, &opts, "suite__")
        .expect_err("direct Seq<Record> is malformed; RecordSeq is canonical");
    assert!(error.0.contains("sequence element with record type"), "{error}");
}

#[test]
fn common_plan_orders_and_owns_all_ticket04_structural_types() {
    let (program, opts) = structural_program();
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__").expect("plans");
    let type_names: Vec<_> = plan
        .shared_types()
        .iter()
        .map(|planned| planned.name())
        .collect();
    let position = |name: &str| {
        type_names
            .iter()
            .position(|candidate| *candidate == name)
            .unwrap_or_else(|| panic!("missing `{name}` in {type_names:?}"))
    };
    assert!(position("Payload") < position("DataBoard"));
    assert!(position("Payload") < position("LeafState"));
    assert!(position("DataBoard") < position("LeafState"));
    assert!(position("LeafState") < position("ParentState"));

    let interface = tbir::common::emit_common_interface(&plan).expect("interface");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime");
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("capsule");
    for declaration in [
        "struct Payload {",
        "struct DataBoard {",
        "struct LeafState {",
        "struct ParentState {",
        "struct _StatefulTarget_state {",
        "struct StateCov {",
    ] {
        assert_eq!(interface.matches(declaration).count(), 1, "{interface}");
        assert!(!runtime.contains(declaration), "{runtime}");
        assert!(!capsule.contains(declaration), "{capsule}");
    }
    for member in [
        "uint64_t value = 9;",
        "harc_rt::HarcQueue<Payload> pending;",
        "harc_rt::HarcWide<5> wide = 0;",
        "std::array<std::array<uint64_t, 2>, 3> lanes{};",
        "DataBoard board;",
        "LeafState leaf;",
        "uint64_t count = 4;",
    ] {
        assert!(
            interface.contains(member),
            "missing `{member}`:\n{interface}"
        );
    }
    assert!(interface.contains("void report(harc_rt::log::HarcLogContext& log_ctx) const;"));
    assert!(
        !interface.contains("void StateCov::report(harc_rt::log::HarcLogContext& log_ctx) const {")
    );
    assert_eq!(
        runtime
            .matches("void StateCov::report(harc_rt::log::HarcLogContext& log_ctx) const {")
            .count(),
        1
    );
}

#[test]
fn common_plan_rejects_a_malformed_component_dependency_cycle() {
    let (mut program, opts) = structural_program();
    let parent = harc::ir::ComponentId(
        program
            .components
            .iter()
            .position(|component| component.name == "ParentState")
            .expect("parent component") as u32,
    );
    let leaf = program
        .components
        .iter_mut()
        .find(|component| component.name == "LeafState")
        .expect("leaf component");
    leaf.fields.push(harc::ir::ComponentFieldSchema {
        name: "parent".into(),
        kind: harc::ir::ComponentFieldKind::Sub {
            component: parent,
            mode: None,
        },
        activation: harc::ir::Activation::Always,
    });

    let error = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("malformed by-value component cycle must fail before rendering");
    assert!(error.0.contains("dependency cycle"), "{error}");
    assert!(
        error.0.contains("LeafState -> ParentState -> LeafState"),
        "{error}"
    );
}

#[test]
fn common_plan_places_an_ordinary_component_method_in_common_runtime() {
    let source = parse_source(
        r#"
agent Stateful
    count : uint<16> default 0
    function bump(value: uint<16>) -> uint<16>
        count = count + value
        return count
    end function bump
end agent Stateful

test StructuralOnly
    let dut : CommonReg
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test StructuralOnly
"#,
    )
    .expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("ordinary component methods are ticket-05 common callables");
    let method = plan
        .callables()
        .iter()
        .find(|callable| callable.kind() == tbir::common::CommonCallableKind::ComponentMethod)
        .expect("component method is cataloged");
    assert_eq!(method.placement(), &tbir::common::CallablePlacement::Common);
}

#[test]
fn hookable_component_method_is_common_until_a_test_owns_a_subscription() {
    let source_for = |subscription: &str| {
        format!(
            r#"
agent Counter
    count : uint<16> default 0
    hookable bump(value: uint<16>) -> uint<16>
        count = count + value
        return count
    end bump
end agent Counter

testbench HookTb
    dut : CommonReg
    counter : Counter
end testbench HookTb

impl HookTest for HookTb
    clock clk = 10ns
{subscription}
    run
        let value = counter.bump(2)
        assert value == 2 else fail("hookable body")
    end run
end impl HookTest
"#
        )
    };
    let mut opts = {
        let mut opts = cpp_tb::EmitOpts::default();
        opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
        opts
    };

    let source = parse_source(&source_for("")).expect("hook-free source parses");
    let program = lower::lower_program(&source).expect("hook-free source lowers");
    verify::verify_program(&program).expect("hook-free source verifies");
    set_clock_interface_for_program(&program, &mut opts);
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("a hookable method with no subscriber has no test-owned hook state");
    let method = plan
        .callables()
        .iter()
        .find(|callable| callable.kind() == tbir::common::CommonCallableKind::ComponentMethod)
        .expect("hookable method is cataloged");
    assert_eq!(method.placement(), &tbir::common::CallablePlacement::Common);
    let component_owner = harc::ir::passes::runtime_cells::RuntimeCellOwner::ComponentInstance {
        component: ir::ComponentId(0),
        name: "Counter".to_string(),
    };
    assert_eq!(
        plan.runtime_cells()
            .for_owner(&component_owner)
            .filter(|cell| matches!(
                cell.kind(),
                harc::ir::passes::runtime_cells::RuntimeCellKind::HookSubscribers { .. }
            ))
            .count(),
        2,
        "a hookable declaration owns stable pre/post receiver storage before subscription"
    );
    let hook_free_interface =
        tbir::common::emit_common_interface(&plan).expect("hook-free interface");
    let hook_free_runtime = tbir::common::emit_common_runtime(&plan).expect("hook-free runtime");

    let source = parse_source(&source_for(
        "    on counter.bump pre\n        log(info, \"pre\")\n    end on\n",
    ))
    .expect("subscribed source parses");
    let program = lower::lower_program(&source).expect("subscribed source lowers");
    verify::verify_program(&program).expect("subscribed source verifies");
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("component hook vectors are receiver-owned runtime cells");
    let component_owner = harc::ir::passes::runtime_cells::RuntimeCellOwner::ComponentInstance {
        component: ir::ComponentId(0),
        name: "Counter".to_string(),
    };
    let hook_cells = plan
        .runtime_cells()
        .for_owner(&component_owner)
        .filter(|cell| {
            matches!(
                cell.kind(),
                harc::ir::passes::runtime_cells::RuntimeCellKind::HookSubscribers { .. }
            )
        })
        .count();
    assert_eq!(
        hook_cells, 2,
        "one subscribed method owns pre/post registries"
    );
    assert_eq!(
        hook_free_interface,
        tbir::common::emit_common_interface(&plan).expect("subscribed interface"),
        "adding a capsule-owned subscription must not rewrite shared component types"
    );
    assert_eq!(
        hook_free_runtime,
        tbir::common::emit_common_runtime(&plan).expect("subscribed runtime"),
        "adding a capsule-owned subscription must not rewrite common method bodies"
    );

    let hookable_testbench = COMMON_TESTBENCH_METHODS_SRC
        .replacen("    function later", "    hookable later", 1)
        .replacen("    end function later", "    end later", 1);
    let source = parse_source(&hookable_testbench).expect("hookable testbench source parses");
    let program = lower::lower_program(&source).expect("hookable testbench source lowers");
    verify::verify_program(&program).expect("hookable testbench source verifies");
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("a testbench hookable has no subscription surface");
    let later = program.testbench_types[0]
        .method("later")
        .expect("hookable testbench method");
    assert!(later.hookable);
    assert_eq!(
        plan.callables()
            .iter()
            .find(|callable| callable.function() == later.function)
            .expect("hookable testbench method is cataloged")
            .placement(),
        &tbir::common::CallablePlacement::Common
    );
}

#[test]
fn testcase_hooks_are_capsule_owned_and_do_not_renumber_other_tests() {
    fn emit_suite(source: &str) -> (String, String, HashMap<String, String>) {
        let source = parse_source(source).expect("hook-isolation source parses");
        let program = lower::lower_program(&source).expect("hook-isolation source lowers");
        verify::verify_program(&program).expect("hook-isolation source verifies");
        let mut opts = cpp_tb::EmitOpts::default();
        opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
        set_clock_interface_for_program(&program, &mut opts);
        let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
            .expect("hook-isolation suite plans");
        let interface = tbir::common::emit_common_interface(&plan).expect("interface emits");
        let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
        let capsules = plan
            .capsules()
            .iter()
            .map(|capsule| {
                let test_index = capsule.test_bodies()[0].test_index();
                let name = program.tests[test_index].name.clone();
                let bytes = tbir::common::emit_common_capsule(&plan, capsule.index())
                    .expect("capsule emits");
                (name, bytes)
            })
            .collect();
        (interface, runtime, capsules)
    }

    fn suite(order: &[&str], a_extra: bool) -> String {
        let declarations = format!(
            r#"
property HookAProperty
    1 == 1 |=> 1 == 1
end property HookAProperty

{}
property HookBProperty
    1 == 1 |=> 1 == 1
end property HookBProperty

agent Counter
    hookable bump(value: uint<8>)
        log(info, "bump=${{value}}")
    end bump
end agent Counter

testbench SharedHookTb
    dut : CommonReg
    counter : Counter
end testbench SharedHookTb
"#,
            if a_extra {
                "property HookAExtraProperty\n    1 == 1 |=> 1 == 1\nend property HookAExtraProperty\n"
            } else {
                ""
            }
        );
        let test_a = format!(
            r#"
impl HookA for SharedHookTb
    clock clk = 10ns
    run
        let events : event<uint<8>>
        on events(value)
            log(info, "A event=${{value}}")
        end on
        on counter.bump pre
            log(info, "A pre")
        end on
        assert property HookAProperty
{}
        emit events(1)
        counter.bump(1)
        wait 1 cycle
    end run
end impl HookA
"#,
            if a_extra {
                "        assert property HookAExtraProperty\n        on 2 cycles\n            log(info, \"A tick\")\n        end on"
            } else {
                ""
            }
        );
        let test_b = r#"
impl HookB for SharedHookTb
    clock clk = 10ns
    run
        let events : event<uint<8>>
        on events(value)
            log(info, "B event=${value}")
        end on
        on counter.bump post
            log(info, "B post")
        end on
        assert property HookBProperty
        emit events(2)
        counter.bump(2)
        wait 1 cycle
    end run
end impl HookB
"#;
        let body = order
            .iter()
            .map(|name| {
                if *name == "A" {
                    test_a.as_str()
                } else {
                    test_b
                }
            })
            .collect::<String>();
        format!("{declarations}{body}")
    }

    let (base_interface, base_runtime, base_capsules) = emit_suite(&suite(&["A", "B"], false));
    let (edited_interface, edited_runtime, edited_capsules) = emit_suite(&suite(&["A", "B"], true));
    assert_eq!(base_interface, edited_interface);
    assert_eq!(base_runtime, edited_runtime);
    assert_eq!(base_capsules["HookB"], edited_capsules["HookB"]);
    assert_ne!(base_capsules["HookA"], edited_capsules["HookA"]);

    let (reordered_interface, reordered_runtime, reordered_capsules) =
        emit_suite(&suite(&["B", "A"], false));
    assert_eq!(base_interface, reordered_interface);
    assert_eq!(base_runtime, reordered_runtime);
    assert_eq!(base_capsules["HookA"], reordered_capsules["HookA"]);
    assert_eq!(base_capsules["HookB"], reordered_capsules["HookB"]);

    for (name, capsule) in &base_capsules {
        let other = if name == "HookA" { "HookB" } else { "HookA" };
        assert!(
            !capsule.contains(other),
            "{name} capsule leaked {other}:\n{capsule}"
        );
    }
}

#[test]
fn common_plan_emits_receiver_owned_component_event_fanout() {
    let source = parse_source(
        r#"
agent EventSource
    observed : out event<uint<8>>
    function publish(value: uint<8>)
        emit observed(value)
    end function publish
end agent EventSource

testbench EventTb
    dut : CommonReg
    source : EventSource
end testbench EventTb

impl EventTest for EventTb
    clock clk = 10ns
    run
        source.publish(3)
    end run
end impl EventTest
"#,
    )
    .expect("event source parses");
    let program = lower::lower_program(&source).expect("event source lowers");
    verify::verify_program(&program).expect("event source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("component event fanout has receiver-owned state");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    assert!(
        runtime.contains("for (auto& _s : self.observed) _s(value);"),
        "component event fanout must use the typed receiver:\n{runtime}"
    );
    let owner = harc::ir::passes::runtime_cells::RuntimeCellOwner::ComponentInstance {
        component: ir::ComponentId(0),
        name: "EventSource".to_string(),
    };
    let event = plan
        .runtime_cells()
        .for_owner(&owner)
        .find(|cell| {
            matches!(
                cell.kind(),
                harc::ir::passes::runtime_cells::RuntimeCellKind::ComponentEventSubscribers {
                    field: 0
                }
            )
        })
        .expect("component event callback registry is planned");
    assert_eq!(
        event.storage(),
        harc::ir::passes::runtime_cells::RuntimeCellStorage::EventRegistry
    );
    assert_eq!(
        event.registration(),
        harc::ir::passes::runtime_cells::RuntimeCellRegistrationPhase::ComponentSetup
    );
}

#[test]
fn callable_placement_shares_an_explicit_identical_bus_adapter() {
    let source = parse_source(
        r#"bus TinyBus
    handshake_channel req: send kind: valid_ready
        data: uint<8>
    end handshake_channel req
    tlm_method read(addr: uint<8>) -> uint<8>: blocking;
    tlm_method read_ooo(addr: uint<8>) -> uint<8>: out_of_order tags 2;
end bus TinyBus

testbench SharedBusTb
    dut : CommonReg

    function drive(value: uint<8>)
        link.req.data = value
        link.req.valid = 1
        wait 1 cycle
        link.req.valid = 0
    end function drive

    function read_once(addr: uint<8>) -> uint<8>
        let value = link.read(addr)
        return value
    end function read_once

    function read_pair(addr: uint<8>) -> uint<8>
        let first = fork link.read_ooo(addr)
        let second = fork link.read_ooo(addr + 1)
        join_all
        return first + second
    end function read_pair
end testbench SharedBusTb

impl SharedBusA for SharedBusTb
    let link : TinyBus = bind dut with {
        req.data: "shared_data", req.valid: "shared_valid", req.ready: "shared_ready"
    }
    clock clk = 10ns
    run
        link.req.valid = 0
        let direct = link.read(1)
        drive(3)
        assert direct == direct else fail("unreachable")
    end run
end impl SharedBusA

impl SharedBusB for SharedBusTb
    let link : TinyBus = bind dut with {
        req.data: "shared_data", req.valid: "shared_valid", req.ready: "shared_ready"
    }
    clock clk = 10ns
    run
        link.req.valid = 0
        let direct = link.read(2)
        drive(7)
        assert direct == direct else fail("unreachable")
    end run
end impl SharedBusB
"#,
    )
    .expect("identical-adapter source parses");
    let program = lower::lower_program(&source).expect("identical-adapter source lowers");
    verify::verify_program(&program).expect("identical-adapter program verifies");
    let catalog = ir::passes::callable_placement::analyze(&program)
        .expect("identical explicit adapters have deterministic placement");
    let drive = program.testbench_types[0]
        .method("drive")
        .expect("shared testbench bus method");
    let read_once = program.testbench_types[0]
        .method("read_once")
        .expect("shared testbench TLM method");
    let read_pair = program.testbench_types[0]
        .method("read_pair")
        .expect("shared testbench fork/join method");
    for method in [drive, read_once, read_pair] {
        let callable = catalog
            .callables()
            .iter()
            .find(|callable| callable.function == method.function)
            .expect("shared testbench bus method is cataloged");
        assert_eq!(callable.placement, tbir::common::CallablePlacement::Common);
    }

    let mut opts = cpp_tb::EmitOpts::default();
    set_dut_interface(
        &mut opts,
        "CommonReg",
        vec![
            ir::passes::dut_access::DutInterfacePort::new(
                "clk",
                ir::PortDirection::In,
                1,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "shared_data",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new(
                "alternate_data",
                ir::PortDirection::In,
                8,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "shared_valid",
                ir::PortDirection::In,
                1,
                ir::IrType::Bool,
                None,
                None,
            ),
            ir::passes::dut_access::DutInterfacePort::new_typed(
                "shared_ready",
                ir::PortDirection::Out,
                1,
                ir::IrType::Bool,
                None,
                None,
            ),
        ]
        .into_iter()
        .chain(
            ["link_read", "link_read_ooo"]
                .into_iter()
                .flat_map(|method| {
                    let mut ports = vec![
                        ir::passes::dut_access::DutInterfacePort::new(
                            format!("{method}_addr"),
                            ir::PortDirection::In,
                            8,
                            None,
                            None,
                        ),
                        ir::passes::dut_access::DutInterfacePort::new_typed(
                            format!("{method}_req_valid"),
                            ir::PortDirection::In,
                            1,
                            ir::IrType::Bool,
                            None,
                            None,
                        ),
                        ir::passes::dut_access::DutInterfacePort::new_typed(
                            format!("{method}_req_ready"),
                            ir::PortDirection::Out,
                            1,
                            ir::IrType::Bool,
                            None,
                            None,
                        ),
                        ir::passes::dut_access::DutInterfacePort::new_typed(
                            format!("{method}_rsp_valid"),
                            ir::PortDirection::Out,
                            1,
                            ir::IrType::Bool,
                            None,
                            None,
                        ),
                        ir::passes::dut_access::DutInterfacePort::new(
                            format!("{method}_rsp_data"),
                            ir::PortDirection::Out,
                            8,
                            None,
                            None,
                        ),
                        ir::passes::dut_access::DutInterfacePort::new_typed(
                            format!("{method}_rsp_ready"),
                            ir::PortDirection::In,
                            1,
                            ir::IrType::Bool,
                            None,
                            None,
                        ),
                    ];
                    if method.ends_with("ooo") {
                        ports.push(ir::passes::dut_access::DutInterfacePort::new(
                            format!("{method}_req_tag"),
                            ir::PortDirection::In,
                            1,
                            None,
                            None,
                        ));
                        ports.push(ir::passes::dut_access::DutInterfacePort::new(
                            format!("{method}_rsp_tag"),
                            ir::PortDirection::Out,
                            1,
                            None,
                            None,
                        ));
                    }
                    ports
                }),
        )
        .collect(),
    );
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("identical explicit adapters are valid common-layout input");
    assert!(!plan.bus_access().is_empty());
    let drive_plan = plan
        .shared_callables()
        .iter()
        .find(|callable| callable.function() == drive.function)
        .expect("shared drive plan");
    assert_eq!(drive_plan.bus_bindings().len(), 1);
    assert_eq!(
        drive_plan.bus_bindings()[0].wire_name("req", "data"),
        "shared_data"
    );
    let runtime = tbir::common::emit_common_runtime(&plan)
        .expect("the shared method renders through its planned adapter");
    assert!(runtime.contains("HarcBusSignalRef<uint64_t>"), "{runtime}");
    assert!(!runtime.contains("ctx.dut->shared_data"), "{runtime}");
    assert!(!runtime.contains("ctx.dut->shared_valid"), "{runtime}");
    assert!(!runtime.contains("dut->link_read_req_valid"), "{runtime}");
    assert!(!runtime.contains("dut->link_read_ooo_req_tag"), "{runtime}");
    assert!(runtime.contains("_tlm_pending = 2"), "{runtime}");
    assert!(!runtime.contains("ctx.dut->link_req_data"), "{runtime}");
    let capsule = tbir::common::emit_common_capsule(&plan, 0)
        .expect("test-local bus adapter renders in its capsule");
    assert!(capsule.contains("ctx.dut->shared_valid"), "{capsule}");
    assert!(capsule.contains("dut->link_read_req_valid"), "{capsule}");

    let mut wrong_direction_opts = opts.clone();
    let wrong_direction_ports = opts
        .dut_interface
        .as_ref()
        .expect("typed fixture catalog")
        .ports()
        .iter()
        .cloned()
        .map(|port| {
            if port.name() == "shared_data" {
                ir::passes::dut_access::DutInterfacePort::new(
                    "shared_data",
                    ir::PortDirection::Out,
                    8,
                    None,
                    None,
                )
            } else {
                port
            }
        })
        .collect();
    set_dut_interface(
        &mut wrong_direction_opts,
        "CommonReg",
        wrong_direction_ports,
    );
    let common_error =
        tbir::common::plan_common_tests(&program, &wrong_direction_opts, "wrong_bus_direction__")
            .expect_err("common planning rejects a write adapter bound to an output port");
    assert!(common_error.0.contains("shared_data"), "{common_error}");
    assert!(common_error.0.contains("expected In"), "{common_error}");
    let self_error = tbir::emit(&program, &source, &wrong_direction_opts)
        .expect_err("self-contained planning rejects the same physical direction mismatch");
    assert!(self_error.0.contains("shared_data"), "{self_error}");
    assert!(self_error.0.contains("expected In"), "{self_error}");

    let mut alternate = program.clone();
    for testbench in &mut alternate.testbenches {
        let binding = testbench
            .bus_bindings
            .iter_mut()
            .find(|binding| binding.field == "link")
            .expect("link binding");
        let (_, port) = binding
            .remap
            .iter_mut()
            .find(|((channel, signal), _)| channel == "req" && signal == "data")
            .expect("req.data remap");
        *port = "alternate_data".to_string();
    }
    let alternate_plan = tbir::common::plan_common_tests(&alternate, &opts, "suite__")
        .expect("the alternate identical adapter also plans");
    assert_eq!(
        plan.build_profile(),
        alternate_plan.build_profile(),
        "test-local bus semantics are not native toolchain identity"
    );
    assert_ne!(
        capsule,
        tbir::common::emit_common_capsule(&alternate_plan, 0)
            .expect("the alternate adapter renders in its capsule"),
        "the exact physical adapter must change its owning capsule"
    );
}

#[test]
fn callable_placement_shares_one_explicit_bound_bus_adapter() {
    let (program, opts) = bound_bus_program(false);
    let before = format!("{program}");
    let before_hash = common_artifacts::stable_hash_hex(before.as_bytes());
    let catalog = ir::passes::callable_placement::analyze(&program)
        .expect("one concrete binding has a deterministic placement");
    let driver = catalog
        .callables()
        .iter()
        .find(|callable| callable.name == "TinyDriver_drive")
        .expect("driver method is cataloged");
    assert!(matches!(
        driver.owner,
        tbir::common::CallableOwner::Transactor {
            transactor: ir::TransactorId(0),
            ..
        }
    ));
    assert_eq!(driver.placement, tbir::common::CallablePlacement::Common);
    assert_eq!(
        common_artifacts::stable_hash_hex(format!("{program}").as_bytes()),
        before_hash,
        "placement analysis must not rewrite verified IR"
    );

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("an explicit bound-bus adapter is common-plan eligible");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime");
    assert_eq!(runtime.matches("TinyDriver_drive(").count(), 1, "{runtime}");
}

/// A rising-edge component handler shares the bound-bus adapter ABI with its
/// method body. In common layout the function lives in the runtime while each
/// capsule supplies its physical DUT-port adapters.
#[test]
fn common_bound_bus_rising_cycle_handler_passes_adapter_arguments() {
    let source_text = BOUND_BUS_PLACEMENT_SRC.replacen(
        "    end when\nend transactor TinyDriver",
        "    end when\n\n    on calls < 200 rising\n        calls = calls + bus.req.data\n    end on\nend transactor TinyDriver",
        1,
    );
    let source = parse_source(&source_text).expect("bound-bus cycle source parses");
    let program = lower::lower_program(&source).expect("bound-bus cycle source lowers");
    verify::verify_program(&program).expect("bound-bus cycle program verifies");
    let (_, opts) = bound_bus_program(false);

    let plan = tbir::common::plan_common_tests(&program, &opts, "cycle_bus__")
        .expect("bound-bus cycle handler plans in the common layout");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    assert!(
        runtime.contains("void TinyDriver_cycle_h")
            && runtime.contains("HarcBusSignalRef<uint64_t> _harc_bus_signal_0"),
        "a bound-bus cycle handler owns an adapter parameter:\n{runtime}"
    );
    let capsule = tbir::common::emit_common_capsule(&plan, 0).expect("capsule emits");
    assert!(
        capsule.contains("TinyDriver_cycle_h")
            && capsule.contains("ctx, _harc_run_state.driver_a, "),
        "a rising-edge callback must pass its bound-bus adapter:\n{capsule}"
    );
}

#[test]
fn shared_tlm_adapter_preserves_record_response_type() {
    let source = parse_source(
        r#"struct Reply
    data : uint<8>
end struct Reply

bus RecordBus
    tlm_method read(addr: uint<8>) -> Reply: blocking;
end bus RecordBus

testbench RecordTb
    dut : RecordTop

    function read_one(addr: uint<8>) -> Reply
        let reply = mem.read(addr)
        return reply
    end function read_one
end testbench RecordTb

impl RecordA for RecordTb
    let mem : RecordBus = bind dut with {
        read.req_valid: "a_req_valid", read.addr: "a_addr",
        read.req_ready: "a_req_ready", read.rsp_valid: "a_rsp_valid",
        read.rsp_data: "a_rsp_data", read.rsp_ready: "a_rsp_ready"
    }
    clock clk = 10ns
    run
        let reply = read_one(1)
        assert reply.data == 1 else fail("A")
    end run
end impl RecordA

impl RecordB for RecordTb
    let mem : RecordBus = bind dut with {
        read.req_valid: "b_req_valid", read.addr: "b_addr",
        read.req_ready: "b_req_ready", read.rsp_valid: "b_rsp_valid",
        read.rsp_data: "b_rsp_data", read.rsp_ready: "b_rsp_ready"
    }
    clock clk = 10ns
    run
        let reply = read_one(2)
        assert reply.data == 2 else fail("B")
    end run
end impl RecordB"#,
    )
    .expect("record TLM source parses");
    let program = lower::lower_program(&source).expect("record TLM source lowers");
    verify::verify_program(&program).expect("record TLM source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    let mut ports = vec![ir::passes::dut_access::DutInterfacePort::new(
        "clk",
        ir::PortDirection::In,
        1,
        None,
        None,
    )];
    for prefix in ["a", "b"] {
        for (suffix, direction, width) in [
            ("req_valid", ir::PortDirection::In, 1),
            ("addr", ir::PortDirection::In, 8),
            ("req_ready", ir::PortDirection::Out, 1),
            ("rsp_valid", ir::PortDirection::Out, 1),
            ("rsp_data", ir::PortDirection::Out, 8),
            ("rsp_ready", ir::PortDirection::In, 1),
        ] {
            ports.push(ir::passes::dut_access::DutInterfacePort::new(
                format!("{prefix}_{suffix}"),
                direction,
                width,
                None,
                None,
            ));
        }
    }
    set_dut_interface(&mut opts, "RecordTop", ports);

    let plan = tbir::common::plan_common_tests(&program, &opts, "record_tlm__")
        .expect("record-valued logical TLM adapter plans");
    let interface = tbir::common::emit_common_interface(&plan).expect("interface emits");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime emits");
    assert!(interface.contains("harc_unpack_Reply"), "{interface}");
    assert!(
        runtime.contains("harc_rt::HarcBusSignalRef<Reply>"),
        "{runtime}"
    );
    assert!(
        runtime.contains("reply = _harc_bus_signal_")
            && runtime.contains(".harc_read();")
            && !runtime.contains("harc_unpack_Reply(_harc_bus_signal_"),
        "{runtime}"
    );
    for (index, wire) in [(0, "a_rsp_data"), (1, "b_rsp_data")] {
        let capsule = tbir::common::emit_common_capsule(&plan, index).expect("capsule emits");
        assert!(capsule.contains(wire), "{capsule}");
        assert!(capsule.contains("harc_unpack_Reply"), "{capsule}");
    }
}

#[test]
fn callable_placement_shares_logically_identical_physical_bus_remaps_deterministically() {
    let (program, opts) = bound_bus_program(true);
    let before = format!("{program}");
    let placement = |program: &ir::TbProgram| {
        let catalog = ir::passes::callable_placement::analyze(program)
            .expect("typed ownership resolves before placement conflict");
        catalog
            .callables()
            .iter()
            .find(|callable| callable.name == "TinyDriver_drive")
            .expect("driver method is cataloged")
            .placement
            .clone()
    };
    let expected = tbir::common::CallablePlacement::Common;
    assert_eq!(placement(&program), expected);
    assert_eq!(format!("{program}"), before, "analysis mutated verified IR");

    let (prefix, tests) = BOUND_BUS_PLACEMENT_SRC
        .split_once("\ntestbench BusTbA")
        .expect("fixture has the first testbench");
    let (first, second) = tests
        .split_once("\ntestbench BusTbB")
        .expect("fixture has the second testbench");
    let permuted_source = format!("{prefix}\ntestbench BusTbB{second}\ntestbench BusTbA{first}");
    let permuted_source = parse_source(&permuted_source).expect("permuted source parses");
    let permuted = lower::lower_program(&permuted_source).expect("permuted source lowers");
    verify::verify_program(&permuted).expect("permuted source verifies");
    assert_eq!(
        placement(&permuted),
        expected,
        "test declaration order must not change the deterministic conflict class"
    );

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("logical bus identity is independent of each physical remap");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime");
    assert_eq!(runtime.matches("TinyDriver_drive(").count(), 1, "{runtime}");
    assert_eq!(format!("{program}"), before, "planning mutated verified IR");
}

#[test]
fn callable_placement_rejects_semantically_divergent_bound_bus_schemas() {
    let (mut program, opts) = bound_bus_program(true);
    program.testbenches[1].bus_bindings[0].bus = "DifferentBus".to_string();
    let catalog = ir::passes::callable_placement::analyze(&program)
        .expect("corrupt binding remains structurally inspectable");
    let driver = catalog
        .callables()
        .iter()
        .find(|callable| callable.name == "TinyDriver_drive")
        .expect("driver method is cataloged");
    assert!(matches!(
        driver.placement,
        tbir::common::CallablePlacement::Invalid {
            reason: tbir::common::InvalidPlacementReason::ConflictingBusBindings { .. }
        }
    ));
    let error = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("different logical bus schemas must not share one callable body");
    assert!(error.0.contains("ConflictingBusBindings"), "{error}");
}

#[test]
fn common_plan_owns_typed_regblock_state_and_frontdoor_adapters() {
    for (fixture, callback_bearing) in [
        ("regblock_basic_test.harc", false),
        ("regblock_record_test.harc", true),
    ] {
        let (program, opts) = regblock_program(fixture);
        let run = program.tests[0].run;
        let catalog = ir::passes::callable_placement::analyze(&program)
            .expect("register-block ownership resolves");
        let run_plan = catalog.callable(run).expect("run is cataloged");
        assert!(
            matches!(
                run_plan.placement,
                tbir::common::CallablePlacement::CapsuleLocal { .. }
            ),
            "run placement for {fixture}: {:?}",
            run_plan.placement
        );

        let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
            .expect("common register-block state and frontdoor plan");
        let publication = plan.publication().expect("publication");
        let capsule = publication
            .capsule(&plan.capsules()[0])
            .expect("register-block capsule");
        assert!(capsule.contains("DmaRegs"), "{capsule}");
        if callback_bearing {
            assert!(
                publication.interface().contains("HARC_RAL_CB_MAX_DEPTH"),
                "{}",
                publication.interface()
            );
            assert!(capsule.contains("DmaRegs regs{};"), "{capsule}");
            assert!(
                capsule.contains("auto& regs = _harc_run_state.regs;"),
                "{capsule}"
            );
            assert!(capsule.contains("_cb_depth"), "{capsule}");
        } else {
            assert!(capsule.contains("AxilHelper_write"), "{capsule}");
            assert!(capsule.contains("AxilHelper_read"), "{capsule}");
        }
    }
}

#[test]
fn verifier_rejects_corrupted_bound_bus_provenance_and_adapters() {
    let (program, _) = bound_bus_program(false);

    let mut missing_adapter = program.clone();
    missing_adapter.testbenches[0].bound_bus_instances.clear();
    let errors = verify::verify_program(&missing_adapter)
        .expect_err("a bound transactor field requires a typed adapter");
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("has no typed bus adapter")),
        "{errors:?}"
    );

    let mut bad_direct_binding = program.clone();
    let run = bad_direct_binding.tests[0].run;
    let direct_port = bad_direct_binding.functions[run.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::DutWrite(port, _)
                if matches!(port.origin, ir::PortOrigin::BusBinding { .. }) =>
            {
                Some(port)
            }
            _ => None,
        })
        .expect("direct bus write");
    direct_port.origin = ir::PortOrigin::BusBinding {
        binding: ir::BusBindingId(99),
        field: "bus".to_string(),
    };
    let errors = verify::verify_program(&bad_direct_binding)
        .expect_err("a missing concrete binding id must fail verification");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("references bus binding bb99 outside its owning testbench")),
        "{errors:?}"
    );

    let mut concrete_origin_in_shared_body = program;
    let driver = concrete_origin_in_shared_body.transactors[0]
        .method("drive_raw")
        .expect("driver method")
        .function;
    let shared_port = concrete_origin_in_shared_body.functions[driver.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::DutWrite(port, _) if port.origin == ir::PortOrigin::BoundBus => Some(port),
            _ => None,
        })
        .expect("shared bound-bus write");
    shared_port.origin = ir::PortOrigin::BusBinding {
        binding: ir::BusBindingId(0),
        field: "bus".to_string(),
    };
    let errors = verify::verify_program(&concrete_origin_in_shared_body)
        .expect_err("shared callable cannot capture one test's binding id");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("references bus binding bb0 outside its owning testbench")),
        "{errors:?}"
    );
}

#[test]
fn common_plan_assigns_stable_owner_and_common_placement_to_component_methods() {
    let source = parse_source(COMMON_COMPONENT_METHODS_SRC).expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("ordinary component methods are reusable");
    let methods = plan
        .callables()
        .iter()
        .filter(|callable| {
            matches!(
                callable.kind(),
                tbir::common::CommonCallableKind::ComponentMethod
            )
        })
        .map(|callable| (callable.name(), callable.owner(), callable.placement()))
        .collect::<Vec<_>>();

    assert_eq!(
        methods.len(),
        4,
        "all component methods need one plan entry"
    );
    assert!(methods.iter().all(|(_, owner, placement)| {
        matches!(owner, tbir::common::CallableOwner::Component { .. })
            && **placement == tbir::common::CallablePlacement::Common
    }));
    assert_ne!(
        methods[0].1, methods[1].1,
        "same-name methods on distinct component owners must not alias"
    );
}

#[test]
fn common_component_method_definitions_are_owned_once_by_runtime() {
    let source = parse_source(COMMON_COMPONENT_METHODS_SRC).expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("ordinary component methods are reusable");
    let interface = tbir::common::emit_common_interface(&plan).expect("interface");
    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime");
    let capsules = (0..plan.capsules().len())
        .map(|index| tbir::common::emit_common_capsule(&plan, index).unwrap())
        .collect::<Vec<_>>();

    for symbol in [
        "LeftCounter_bump",
        "RightCounter_bump",
        "CounterPair_bump_copy",
        "CounterPair_sum_after",
    ] {
        let signature = format!("uint64_t {symbol}(HarcTestContext& ctx");
        assert_eq!(
            interface.matches(&signature).count(),
            1,
            "{symbol} declaration"
        );
        assert_eq!(
            runtime.matches(&signature).count(),
            1,
            "{symbol} definition"
        );
        assert!(
            capsules
                .iter()
                .all(|capsule| !capsule.contains(&format!("auto {symbol} ="))),
            "{symbol} must not be redefined in a capsule"
        );
    }
}

#[test]
fn common_plan_rejects_missing_or_duplicate_component_method_ownership() {
    let source = parse_source(COMMON_COMPONENT_METHODS_SRC).expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let mut orphan = program.clone();
    orphan.components[0].methods.clear();
    let error = tbir::common::plan_common_tests(&orphan, &opts, "suite__")
        .expect_err("an orphan component method must fail planning");
    assert!(error.0.contains("exactly one owner"), "{error}");
    assert!(error.0.contains("fn0"), "{error}");

    let mut duplicate = program;
    let repeated = duplicate.components[0].methods[0].clone();
    duplicate.components[1].methods.push(repeated);
    let error = tbir::common::plan_common_tests(&duplicate, &opts, "suite__")
        .expect_err("a multiply owned component method must fail planning");
    assert!(error.0.contains("exactly one owner"), "{error}");
    assert!(error.0.contains("fn0"), "{error}");

    let source = parse_source(COMMON_COMPONENT_METHODS_SRC).expect("source parses");
    let mut wrong_kind = lower::lower_program(&source).expect("source lowers");
    let method = wrong_kind.components[0].methods[0].function;
    wrong_kind.functions[method.index()].kind = ir::FunctionKind::Helper;
    let error = tbir::common::plan_common_tests(&wrong_kind, &opts, "suite__")
        .expect_err("a schema claim cannot relabel a component callable as a helper");
    assert!(
        error.0.contains("owner whose schema does not match"),
        "{error}"
    );

    let source = parse_source(COMMON_COMPONENT_METHODS_SRC).expect("source parses");
    let mut out_of_range = lower::lower_program(&source).expect("source lowers");
    let method = out_of_range.components[0].methods[0].function;
    let ir::FunctionKind::ComponentMethod {
        component,
        method_name,
        ..
    } = out_of_range.functions[method.index()].kind.clone()
    else {
        panic!("component method kind");
    };
    out_of_range.functions[method.index()].kind = ir::FunctionKind::ComponentMethod {
        component,
        member: ir::ComponentCallableId(999),
        method_name,
    };
    let error = tbir::common::plan_common_tests(&out_of_range, &opts, "suite__")
        .expect_err("out-of-range component member identity must fail without panicking");
    assert!(
        error.0.contains("owner whose schema does not match"),
        "{error}"
    );
}

#[test]
fn verifier_rejects_a_stale_component_callable_identity() {
    let source = parse_source(COMMON_COMPONENT_METHODS_SRC).expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    let mut corrupted = program.clone();
    let target = corrupted
        .functions
        .iter_mut()
        .flat_map(|function| &mut function.blocks)
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::ComponentCall { function, .. } => Some(function),
            _ => None,
        })
        .expect("component call");
    *target = program.tests[0].run;

    let errors = verify::verify_program(&corrupted)
        .expect_err("a component call must retain the exact callable identity");
    assert!(
        errors.iter().any(|error| {
            let message = error.to_string();
            message.contains("component method")
                && message.contains("resolves to fn")
                && message.contains("call carries fn")
        }),
        "{errors:?}"
    );
}

#[test]
fn verifier_rejects_same_abi_component_and_testbench_method_identity_swaps() {
    let source = parse_source(
        r#"
agent IdentityAgent
    function first(value: uint<16>) -> uint<16>
        return value + 1
    end function first
    function second(value: uint<16>) -> uint<16>
        return value + 2
    end function second
end agent IdentityAgent

testbench IdentityTb
    dut : CommonReg
    function first(value: uint<16>) -> uint<16>
        return value + 3
    end function first
    function second(value: uint<16>) -> uint<16>
        return value + 4
    end function second
end testbench IdentityTb

impl IdentityTest for IdentityTb
    clock clk = 10ns
    run
        assert first(1) == 4 else fail("testbench identity")
    end run
end impl IdentityTest
"#,
    )
    .expect("identity source parses");
    let program = lower::lower_program(&source).expect("identity source lowers");
    verify::verify_program(&program).expect("identity source verifies");
    let verify_identity_error = |program: &ir::TbProgram| {
        let errors = verify::verify_program(program)
            .expect_err("coordinated method identity swap must fail verification");
        assert!(
            errors.iter().any(|error| {
                let message = error.to_string();
                message.contains("inconsistent callable") || message.contains("method points at")
            }),
            "{errors:?}"
        );
    };

    let mut component_swap = program.clone();
    let first = component_swap.components[0].methods[0].function;
    let second = component_swap.components[0].methods[1].function;
    component_swap.components[0].methods[0].function = second;
    component_swap.components[0].methods[1].function = first;
    component_swap.functions[first.index()].kind = ir::FunctionKind::ComponentMethod {
        component: ir::ComponentId(0),
        member: ir::ComponentCallableId(1),
        method_name: Some("first".to_string()),
    };
    component_swap.functions[second.index()].kind = ir::FunctionKind::ComponentMethod {
        component: ir::ComponentId(0),
        member: ir::ComponentCallableId(0),
        method_name: Some("second".to_string()),
    };
    verify_identity_error(&component_swap);

    let mut testbench_swap = program;
    let first = testbench_swap.testbench_types[0].methods[0].function;
    let second = testbench_swap.testbench_types[0].methods[1].function;
    testbench_swap.testbench_types[0].methods[0].function = second;
    testbench_swap.testbench_types[0].methods[1].function = first;
    testbench_swap.functions[first.index()].kind = ir::FunctionKind::TestbenchMethod {
        testbench: ir::TestbenchTypeId(0),
        method: ir::TestbenchMethodId(1),
        name: "first".to_string(),
    };
    testbench_swap.functions[second.index()].kind = ir::FunctionKind::TestbenchMethod {
        testbench: ir::TestbenchTypeId(0),
        method: ir::TestbenchMethodId(0),
        name: "second".to_string(),
    };
    verify_identity_error(&testbench_swap);
}

#[test]
fn verifier_and_common_plan_reject_test_body_identity_rebinding() {
    let source = parse_source(
        r#"
test Alpha
    let dut : CommonReg
    clock clk = 10ns
    run
        wait 1 cycle
    end run
    check
        assert 1 == 1 else fail("alpha")
    end check
end test Alpha

test Beta
    let dut : CommonReg
    clock clk = 10ns
    run
        wait 1 cycle
    end run
    check
        assert 1 == 1 else fail("beta")
    end check
end test Beta
"#,
    )
    .expect("test identity source parses");
    let program = lower::lower_program(&source).expect("test identity source lowers");
    verify::verify_program(&program).expect("test identity source verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let assert_rejected = |program: &ir::TbProgram, label: &str| {
        let errors = match verify::verify_program(program) {
            Err(errors) => errors,
            Ok(()) => panic!("{label} unexpectedly verified"),
        };
        assert!(
            errors.iter().any(|error| {
                let message = error.to_string();
                message.contains("test")
                    && (message.contains("callable identity") || message.contains("mismatched id"))
            }),
            "{label}: {errors:?}"
        );
        let error = tbir::common::plan_common_tests(program, &opts, "suite__")
            .expect_err("corrupt test identity must fail before publication");
        assert!(
            error.0.contains("owner whose schema does not match")
                || error.0.contains("mismatched id"),
            "{label}: {error}"
        );
    };

    let mut run_swap = program.clone();
    let alpha_run = run_swap.tests[0].run;
    let beta_run = run_swap.tests[1].run;
    run_swap.tests[0].run = beta_run;
    run_swap.tests[1].run = alpha_run;
    run_swap.functions[alpha_run.index()].owner = Some(run_swap.tests[1].testbench);
    run_swap.functions[beta_run.index()].owner = Some(run_swap.tests[0].testbench);
    assert_rejected(&run_swap, "coordinated run swap");

    let mut check_swap = program.clone();
    let alpha_check = check_swap.tests[0].check.expect("Alpha check");
    let beta_check = check_swap.tests[1].check.expect("Beta check");
    check_swap.tests[0].check = Some(beta_check);
    check_swap.tests[1].check = Some(alpha_check);
    check_swap.functions[alpha_check.index()].owner = Some(check_swap.tests[1].testbench);
    check_swap.functions[beta_check.index()].owner = Some(check_swap.tests[0].testbench);
    assert_rejected(&check_swap, "coordinated check swap");

    let mut id_swap = program.clone();
    let alpha_id = id_swap.tests[0].id;
    id_swap.tests[0].id = id_swap.tests[1].id;
    id_swap.tests[1].id = alpha_id;
    for test_index in 0..id_swap.tests.len() {
        let test_id = id_swap.tests[test_index].id;
        for function in [
            Some(id_swap.tests[test_index].run),
            id_swap.tests[test_index].check,
        ]
        .into_iter()
        .flatten()
        {
            let ir::FunctionKind::TestBody { test, .. } =
                &mut id_swap.functions[function.index()].kind
            else {
                panic!("test slot must point at a test body")
            };
            *test = test_id;
        }
    }
    assert_rejected(&id_swap, "coordinated test id permutation");

    let mut renamed = program;
    renamed.tests[0].name = "RenamedAlpha".to_string();
    assert_rejected(&renamed, "test rename");
}

#[test]
fn common_plan_owns_one_canonical_testbench_method_set_for_multiple_impls() {
    let source = parse_source(COMMON_TESTBENCH_METHODS_SRC).expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    assert_eq!(program.testbench_types.len(), 1);
    assert_eq!(program.testbench_types[0].methods.len(), 5);
    let plan = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("state-only testbench methods are reusable");
    let methods = plan
        .callables()
        .iter()
        .filter(|callable| callable.kind() == tbir::common::CommonCallableKind::TestbenchMethod)
        .collect::<Vec<_>>();
    assert_eq!(methods.len(), 5);
    assert!(methods.iter().all(|callable| {
        matches!(
            callable.owner(),
            tbir::common::CallableOwner::TestbenchType(ir::TestbenchTypeId(0))
        ) && callable.placement() == &tbir::common::CallablePlacement::Common
    }));

    let runtime = tbir::common::emit_common_runtime(&plan).expect("runtime");
    let interface = tbir::common::emit_common_interface(&plan).expect("interface");
    for method in ["later", "ordered", "mirror", "save", "lazy_take"] {
        let symbol = format!("MethodTb_{method}");
        assert_eq!(runtime.matches(&symbol).count() >= 1, true, "{symbol}");
        assert_eq!(
            interface.matches(&symbol).count(),
            1,
            "{symbol} declaration"
        );
    }
    for index in 0..plan.capsules().len() {
        let capsule = tbir::common::emit_common_capsule(&plan, index).expect("capsule renders");
        assert!(!capsule.contains("auto MethodTb_"));
    }

    let save = program.testbench_types[0]
        .method("save")
        .expect("record-field method");
    let save_body = program.function(save.function);
    assert_eq!(
        save_body.testbench_record_locals,
        vec![ir::TbRecordLocalBinding {
            local: save_body
                .locals
                .iter()
                .position(|local| local.name == "saved")
                .map(|index| ir::LocalId(index as u32))
                .expect("saved synthetic local"),
            field: "saved".to_string(),
            record: ir::RecordId(0),
        }]
    );
    assert!(runtime.contains("_tb.saved = beat;"), "{runtime}");
    assert!(runtime.contains("_tb.saved.value"), "{runtime}");
}

#[test]
fn canonical_testbench_component_inventory_excludes_impl_local_components() {
    let source = parse_source(
        r#"
agent InventoryCell
    value : uint<8> default 0
    function read() -> uint<8>
        return value
    end function read
end agent InventoryCell

testbench InventoryTb
    dut : CommonReg
    declared : InventoryCell
    function read_declared() -> uint<8>
        return declared.read()
    end function read_declared
end testbench InventoryTb

impl InventoryTest for InventoryTb
    let local : InventoryCell
    clock clk = 10ns
    run
        assert read_declared() == 0
        assert local.read() == 0
    end run
end impl InventoryTest
"#,
    )
    .expect("component inventory source parses");
    let program = lower::lower_program(&source).expect("component inventory source lowers");
    verify::verify_program(&program).expect("component inventory source verifies");
    assert_eq!(
        program.testbench_types[0].component_fields,
        vec![("declared".to_string(), ir::ComponentId(0))]
    );
    assert!(program.testbenches[0]
        .component_fields
        .iter()
        .any(|binding| binding.field == "local"));

    let mut missing = program.clone();
    missing.testbenches[0]
        .component_fields
        .retain(|binding| binding.field != "declared");
    let errors = verify::verify_program(&missing)
        .expect_err("every implementation must bind canonical declared components");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("missing declared component field `declared`")),
        "{errors:?}"
    );

    let mut missing_type_binding = program;
    missing_type_binding.testbenches[0]
        .component_fields
        .retain(|binding| binding.field != "declared");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&missing_type_binding, &mut opts);
    let error = tbir::common::plan_common_tests(&missing_type_binding, &opts, "suite__")
        .expect_err("common planning cannot infer a missing typed receiver binding by name");
    assert!(
        error
            .0
            .contains("typed binding for declared component field `declared`"),
        "{error}"
    );
}

#[test]
fn verifier_rejects_missing_or_corrupted_testbench_record_local_provenance() {
    let source = parse_source(COMMON_TESTBENCH_METHODS_SRC).expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    let save = program.testbench_types[0]
        .method("save")
        .expect("record-field method")
        .function;

    let mut missing = program.clone();
    missing.functions[save.index()]
        .testbench_record_locals
        .clear();
    let errors = verify::verify_program(&missing)
        .expect_err("synthetic record local without provenance must fail");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("aliases a testbench record field without typed provenance")),
        "{errors:?}"
    );

    let mut wrong_field = program;
    wrong_field.functions[save.index()].testbench_record_locals[0].field = "ghost".to_string();
    let errors = verify::verify_program(&wrong_field)
        .expect_err("record provenance must resolve against the owning schema");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("does not match every owning testbench schema")),
        "{errors:?}"
    );
}

#[test]
fn common_plan_rejects_corrupt_testbench_method_ownership_and_cycles() {
    let source = parse_source(COMMON_TESTBENCH_METHODS_SRC).expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let mut orphan = program.clone();
    orphan.testbench_types[0].methods.remove(2);
    let error = tbir::common::plan_common_tests(&orphan, &opts, "suite__")
        .expect_err("orphan canonical method must fail");
    assert!(error.0.contains("exactly one owner"), "{error}");

    let mut out_of_range = program.clone();
    let first = out_of_range.testbench_types[0].methods[0].function;
    let ir::FunctionKind::TestbenchMethod {
        testbench, name, ..
    } = out_of_range.functions[first.index()].kind.clone()
    else {
        panic!("canonical testbench method kind");
    };
    out_of_range.functions[first.index()].kind = ir::FunctionKind::TestbenchMethod {
        testbench,
        method: ir::TestbenchMethodId(999),
        name,
    };
    let error = tbir::common::plan_common_tests(&out_of_range, &opts, "suite__")
        .expect_err("out-of-range canonical method identity must fail without panicking");
    assert!(
        error.0.contains("owner whose schema does not match"),
        "{error}"
    );

    let mut recursive = program;
    let first = recursive.testbench_types[0].methods[0].function;
    recursive.functions[first.index()].blocks[0]
        .stmts
        .push(ir::Stmt::TestbenchCall {
            function: first,
            args: vec![ir::Expr::Literal {
                value: 1,
                ty: ir::IrType::UInt(Some(16)),
            }],
            dut_args: Vec::new(),
            dest: Some(ir::LocalId(1)),
        });
    let error = tbir::common::plan_common_tests(&recursive, &opts, "suite__")
        .expect_err("recursive canonical methods must fail before rendering");
    assert!(
        error.0.contains("testbench method dependency cycle"),
        "{error}"
    );
    let self_contained = tbir::emit(&recursive, &source, &opts)
        .expect_err("recursive canonical methods must fail self-contained emission");
    assert!(
        self_contained
            .0
            .contains("testbench method dependency cycle"),
        "{self_contained}"
    );
}

#[test]
fn component_method_dependency_cycles_fail_before_either_layout_renders_cpp() {
    let source = parse_source(
        r#"
agent RecursiveAgent
    function first(value: uint<8>) -> uint<8>
        return second(value)
    end function first
    function second(value: uint<8>) -> uint<8>
        return value
    end function second
end agent RecursiveAgent

test RecursiveTest
    let dut : CommonReg
    let agent : RecursiveAgent
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test RecursiveTest
"#,
    )
    .expect("recursive component source parses");
    let mut program = lower::lower_program(&source).expect("component source lowers");
    verify::verify_program(&program).expect("component source verifies");
    let first = program.components[0].methods[0].function;
    let second = program.components[0].methods[1].function;
    let mut recursive_call = program.functions[first.index()].blocks[0]
        .stmts
        .iter()
        .find_map(|stmt| match stmt {
            ir::Stmt::ComponentCall { .. } => Some(stmt.clone()),
            _ => None,
        })
        .expect("first calls second");
    let ir::Stmt::ComponentCall {
        function, method, ..
    } = &mut recursive_call
    else {
        unreachable!()
    };
    *function = first;
    *method = "first".to_string();
    program.functions[second.index()].blocks[0]
        .stmts
        .push(recursive_call);
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let common = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("recursive component methods must fail common planning");
    let self_contained = tbir::emit(&program, &source, &opts)
        .expect_err("recursive component methods must fail self-contained emission");
    for error in [common, self_contained] {
        assert!(
            error.0.contains("component method dependency cycle"),
            "{error}"
        );
        assert!(error.0.contains("comp_method_"), "{error}");
    }
}

#[test]
fn tseq_dependency_cycles_fail_before_either_layout_renders_cpp() {
    let source = parse_source(
        r#"
tseq First(seed: uint<8>) -> TSeq<uint<8>>
    let values = Second(seed)
    for value in values
        yield value
    end for
end tseq First

tseq Second(seed: uint<8>) -> TSeq<uint<8>>
    let values = First(seed)
    for value in values
        yield value
    end for
end tseq Second

test Cycle
    let dut : CommonReg
    clock clk = 10ns
    run
        let values = First(1)
        for value in values
            assert value == 1
        end for
    end run
end test Cycle
"#,
    )
    .expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);

    let common = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("recursive tseqs must fail common planning");
    let self_contained = tbir::emit(&program, &source, &opts)
        .expect_err("recursive tseqs must fail self-contained emission");
    assert_eq!(common.0, self_contained.0);
    assert!(common.0.contains("tseq dependency cycle"), "{common}");
    assert!(common.0.contains("First -> Second -> First"), "{common}");
}

#[test]
fn verifier_rejects_tseq_parameter_metadata_drift() {
    let (mut program, _) = shared_program();
    let function = program
        .functions
        .iter_mut()
        .find(|function| function.name == "CopyRecordValues")
        .expect("record-sequence parameter function");
    assert_eq!(
        function.params[0].ty,
        harc::ir::IrType::RecordSeq(harc::ir::RecordId(0))
    );
    assert_eq!(function.locals[0].ty, function.params[0].ty);
    function.locals[0].ty = harc::ir::IrType::Unknown;

    let errors = verify::verify_program(&program)
        .expect_err("a TSeq parameter/local type mismatch must not verify");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("tseq `CopyRecordValues` param 0 metadata")),
        "{errors:?}"
    );
}

#[test]
fn lowering_rejects_tseq_call_arity_before_common_planning() {
    let source_for = |args: &str| {
        format!(
            r#"
tseq Pair(a: uint<8>, b: uint<8>) -> TSeq<uint<8>>
    yield a
    yield b
end tseq Pair

test WrongArity
    let dut : CommonReg
    clock clk = 10ns
    run
        let values = Pair({args})
    end run
end test WrongArity
"#
        )
    };

    for (args, expected) in [("1", 1), ("1, 2, 3", 3)] {
        let source = parse_source(&source_for(args)).expect("wrong-arity source parses");
        let error = lower::lower_program(&source).expect_err("wrong TSeq arity must not lower");
        let message = error.to_string();
        assert!(
            message.contains(&format!(
                "tseq `Pair` takes 2 argument(s), call passes {expected}"
            )),
            "{message}"
        );
    }
}

#[test]
fn tseq_sequence_arguments_use_directional_element_compatibility() {
    let source_for = |call: &str| {
        format!(
            r#"
struct Beat
    value : uint<8>
end struct Beat

tseq GenNarrow() -> TSeq<uint<8>>
    yield 1
end tseq GenNarrow

tseq GenWide() -> TSeq<uint<32>>
    yield 1
end tseq GenWide

tseq GenSigned() -> TSeq<sint<8>>
    yield 1
end tseq GenSigned

tseq GenBool() -> TSeq<bool>
    yield true
end tseq GenBool

tseq Gen64() -> TSeq<uint<64>>
    yield 1
end tseq Gen64

tseq GenRecord() -> TSeq<Beat>
    let beat : Beat
    yield beat
end tseq GenRecord

tseq CopyWide(values: TSeq<uint<32>>) -> TSeq<uint<32>>
    for value in values
        yield value
    end for
end tseq CopyWide

tseq CopyNarrow(values: TSeq<uint<8>>) -> TSeq<uint<8>>
    for value in values
        yield value
    end for
end tseq CopyNarrow

tseq Copy65(values: TSeq<uint<65>>) -> TSeq<uint<65>>
    for value in values
        yield value
    end for
end tseq Copy65

test SequenceArgs
    let dut : CommonReg
    clock clk = 10ns
    run
        let narrow = GenNarrow()
        let wide = GenWide()
        let signed = GenSigned()
        let booleans = GenBool()
        let values64 = Gen64()
        let records = GenRecord()
{call}
    end run
end test SequenceArgs
"#
        )
    };

    let source = parse_source(&source_for("        let copied = CopyWide(narrow)"))
        .expect("widening source parses");
    let program = lower::lower_program(&source).expect("sequence-element widening lowers");
    verify::verify_program(&program).expect("sequence-element widening verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);
    tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect("sequence-element widening plans for common layout");

    for (call, expected) in [
        (
            "        let copied = CopyNarrow(wide)",
            "takes Seq(UInt(Some(8))) and was given Seq(UInt(Some(32)))",
        ),
        (
            "        let copied = CopyWide(signed)",
            "takes Seq(UInt(Some(32))) and was given Seq(SInt(Some(8)))",
        ),
        (
            "        let copied = CopyNarrow(booleans)",
            "takes Seq(UInt(Some(8))) and was given Seq(Bool)",
        ),
        (
            "        let copied = Copy65(values64)",
            "takes Seq(UInt(Some(65))) and was given Seq(UInt(Some(64)))",
        ),
        (
            "        let copied = CopyWide(records)",
            "takes a scalar `TSeq` and was given a `TSeq<Beat>`",
        ),
    ] {
        let source = parse_source(&source_for(call)).expect("negative source parses");
        let message = lower::lower_program(&source)
            .expect_err("incompatible TSeq carrier must not lower")
            .to_string();
        assert!(message.contains(expected), "{call}: {message}");
    }
}

#[test]
fn verifier_rejects_corrupted_tseq_call_contracts() {
    let verify_error = |program: &harc::ir::TbProgram, expected: &str| {
        let errors = verify::verify_program(program).expect_err("corrupted TSeq call must fail");
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains(expected)),
            "missing `{expected}` in {errors:?}"
        );
    };

    let (mut missing, _) = shared_program();
    let (expr, _) = tseq_call_mut(&mut missing, "ForwardPure");
    let harc::ir::Expr::Call(harc::ir::CallTarget::Tseq { name, .. }, _) = expr else {
        unreachable!()
    };
    *name = "MissingTseq".to_string();
    verify_error(&missing, "references missing tseq `MissingTseq`");

    let (mut under, _) = shared_program();
    let (expr, _) = tseq_call_mut(&mut under, "ForwardPure");
    let harc::ir::Expr::Call(_, args) = expr else {
        unreachable!()
    };
    args.clear();
    verify_error(
        &under,
        "tseq `ForwardPure` takes 1 argument(s), call carries 0",
    );

    let (mut over, _) = shared_program();
    let (expr, _) = tseq_call_mut(&mut over, "ForwardPure");
    let harc::ir::Expr::Call(_, args) = expr else {
        unreachable!()
    };
    args.push(harc::ir::Expr::Literal {
        value: 2,
        ty: harc::ir::IrType::UInt(Some(8)),
    });
    verify_error(
        &over,
        "tseq `ForwardPure` takes 1 argument(s), call carries 2",
    );

    let (mut wrong_type, _) = shared_program();
    let (expr, _) = tseq_call_mut(&mut wrong_type, "CopyScalarValues");
    let harc::ir::Expr::Call(_, args) = expr else {
        unreachable!()
    };
    args[0] = harc::ir::Expr::Literal {
        value: 1,
        ty: harc::ir::IrType::UInt(Some(8)),
    };
    verify_error(
        &wrong_type,
        "tseq `CopyScalarValues` argument 1 has type UInt(Some(8)), expected Seq(UInt(Some(32)))",
    );

    let set_copy_scalar_param = |program: &mut harc::ir::TbProgram, ty: harc::ir::IrType| {
        let function = program
            .functions
            .iter_mut()
            .find(|function| function.name == "CopyScalarValues")
            .expect("CopyScalarValues function");
        function.params[0].ty = ty.clone();
        function.locals[0].ty = ty;
    };

    let (mut widening, _) = shared_program();
    set_copy_scalar_param(
        &mut widening,
        harc::ir::IrType::Seq(Box::new(harc::ir::IrType::UInt(Some(32)))),
    );
    verify::verify_program(&widening).expect("uint16 sequence may widen into uint32 sequence slot");

    let (mut narrowing, _) = shared_program();
    set_copy_scalar_param(
        &mut narrowing,
        harc::ir::IrType::Seq(Box::new(harc::ir::IrType::UInt(Some(8)))),
    );
    verify_error(
        &narrowing,
        "tseq `CopyScalarValues` argument 1 has type Seq(UInt(Some(16))), expected Seq(UInt(Some(8)))",
    );

    let (mut signedness, _) = shared_program();
    set_copy_scalar_param(
        &mut signedness,
        harc::ir::IrType::Seq(Box::new(harc::ir::IrType::SInt(Some(16)))),
    );
    verify_error(
        &signedness,
        "tseq `CopyScalarValues` argument 1 has type Seq(UInt(Some(16))), expected Seq(SInt(Some(16)))",
    );

    let (mut bool_carrier, _) = shared_program();
    set_copy_scalar_param(
        &mut bool_carrier,
        harc::ir::IrType::Seq(Box::new(harc::ir::IrType::Bool)),
    );
    verify_error(
        &bool_carrier,
        "tseq `CopyScalarValues` argument 1 has type Seq(UInt(Some(16))), expected Seq(Bool)",
    );

    let (mut wide_carrier, _) = shared_program();
    set_copy_scalar_param(
        &mut wide_carrier,
        harc::ir::IrType::Seq(Box::new(harc::ir::IrType::UInt(Some(65)))),
    );
    verify_error(
        &wide_carrier,
        "tseq `CopyScalarValues` argument 1 has type Seq(UInt(Some(16))), expected Seq(UInt(Some(65)))",
    );

    let (mut stale_identity, mut opts) = shared_program();
    let run = stale_identity.tests[0].run;
    let (expr, _) = tseq_call_mut(&mut stale_identity, "ForwardPure");
    let harc::ir::Expr::Call(harc::ir::CallTarget::Tseq { function, .. }, _) = expr else {
        unreachable!()
    };
    *function = run;
    verify_error(&stale_identity, "references missing tseq `ForwardPure`");
    opts.dut_port_widths.insert("clk".to_string(), 1);
    let error = tbir::common::plan_common_tests(&stale_identity, &opts, "suite__")
        .expect_err("common placement must reject a non-TSeq FunctionId at a TSeq edge");
    assert!(
        error.0.contains("references missing callable tseq fn"),
        "{error}"
    );
}

#[test]
fn helper_call_edges_keep_their_exact_function_identity() {
    let source = parse_source(
        r#"
function add_one(value: uint<8>) -> uint<8>
    return value + 1
end function add_one

test HelperIdentity
    let dut : CommonReg
    clock clk = 10ns
    run
        let result = add_one(4)
        assert result == 5
    end run
end test HelperIdentity
"#,
    )
    .expect("helper identity source parses");
    let program = lower::lower_program(&source).expect("helper identity source lowers");
    verify::verify_program(&program).expect("helper identity source verifies");
    let mut corrupted = program.clone();
    let run = corrupted.tests[0].run;
    let call = corrupted.functions[run.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::Assign(_, ir::Expr::Call(ir::CallTarget::Helper { function, .. }, _)) => {
                Some(function)
            }
            _ => None,
        })
        .expect("direct helper call");
    *call = run;
    let errors = verify::verify_program(&corrupted)
        .expect_err("helper call cannot name a non-helper FunctionId");
    assert!(
        errors.iter().any(|error| error
            .to_string()
            .contains("references missing helper `add_one`")),
        "{errors:?}"
    );

    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);
    set_clock_interface_for_program(&program, &mut opts);
    let error = tbir::common::plan_common_tests(&corrupted, &opts, "suite__")
        .expect_err("common placement must reject the same stale helper edge");
    assert!(
        error.0.contains("references missing callable helper fn"),
        "{error}"
    );
}

#[test]
fn verifier_rejects_corrupted_tseq_result_destination_shapes() {
    let verify_error = |program: &harc::ir::TbProgram| {
        let errors = verify::verify_program(program)
            .expect_err("a TSeq result assigned to the wrong sequence shape must fail");
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, harc::ir::verify::VerifyError::TypeMismatch { .. })),
            "{errors:?}"
        );
    };

    let (mut scalar_to_record, _) = shared_program();
    let (_, dest) = tseq_call_mut(&mut scalar_to_record, "ForwardPure");
    let function = scalar_to_record
        .functions
        .iter_mut()
        .find(|function| {
            function.locals.get(dest.index()).is_some_and(|local| {
                local.name == "raw_values"
                    && local.ty == harc::ir::IrType::Seq(Box::new(harc::ir::IrType::UInt(Some(16))))
            })
        })
        .expect("owning scalar-sequence function");
    function.locals[dest.index()].ty = harc::ir::IrType::RecordSeq(harc::ir::RecordId(0));
    verify_error(&scalar_to_record);

    let (mut record_to_scalar, _) = shared_program();
    let (_, dest) = tseq_call_mut(&mut record_to_scalar, "RecordValues");
    let function = record_to_scalar
        .functions
        .iter_mut()
        .find(|function| {
            function.locals.get(dest.index()).is_some_and(|local| {
                local.name == "raw_record_values"
                    && local.ty == harc::ir::IrType::RecordSeq(harc::ir::RecordId(0))
            })
        })
        .expect("owning record-sequence function");
    function.locals[dest.index()].ty =
        harc::ir::IrType::Seq(Box::new(harc::ir::IrType::UInt(Some(16))));
    verify_error(&record_to_scalar);
}

#[test]
fn common_plan_requires_source_backed_tseq_randomization() {
    let source = parse_source(
        r#"
struct Item
    value : uint<8>
end struct Item
tseq RandomItems() -> TSeq<Item>
    let item : Item
    randomize(item)
    yield item
end tseq RandomItems
test Randomized
    let dut : CommonReg
    clock clk = 10ns
    run
        let items = RandomItems()
    end run
end test Randomized
"#,
    )
    .expect("source parses");
    let program = lower::lower_program(&source).expect("source lowers");
    verify::verify_program(&program).expect("program verifies");
    let mut opts = cpp_tb::EmitOpts::default();
    opts.dut_port_widths = HashMap::from([("clk".to_string(), 1)]);

    let runtime_cells = ir::passes::runtime_cells::analyze(&program)
        .expect("solver state is inventoried before common-layout gating");
    assert!(runtime_cells
        .find(
            &ir::passes::runtime_cells::RuntimeCellOwner::Runtime,
            &ir::passes::runtime_cells::RuntimeCellKind::Solver,
        )
        .is_some());
    assert!(runtime_cells.cells().iter().any(|cell| matches!(
        cell.kind(),
        ir::passes::runtime_cells::RuntimeCellKind::ConstraintState { .. }
    )));

    let error = tbir::common::plan_common_tests(&program, &opts, "suite__")
        .expect_err("solver-backed tseq randomization requires source-backed planning");
    assert!(error.0.contains("randomization constraints"), "{error}");
    assert!(
        error
            .0
            .contains("source-backed randomization plan required"),
        "{error}"
    );
}
