use harc::codegen::common_artifacts::{AbiAnchor, ABI_ANCHOR_PLACEHOLDER};
use std::path::{Path, PathBuf};
use std::process::Command;

const DUT: &str = "\
module TbirCommonReg(
  input logic clk,
  input logic [7:0] d,
  output logic [7:0] q,
  output logic signed [7:0] signed_q8,
  output logic signed [64:0] signed_q65
);
  always_ff @(posedge clk) q <= d;
  assign signed_q8 = -1;
  assign signed_q65 = -1;
endmodule
";

const TESTBENCH: &str = r#"
test Common17
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        dut.d = 17
        wait 1 cycle
        assert dut.q == 17 else fail("Common17 observed the wrong value")
        log(info, "COMMON_RESULT=17")
    end run
end test Common17

test Common203
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        dut.d = 203
        wait 1 cycle
        assert dut.q == 203 else fail("Common203 observed the wrong value")
        log(info, "COMMON_RESULT=203")
    end run
end test Common203
"#;

const PROBE_COLLISION_DUT: &str = r#"
module TbirProbeCollision(
  input logic clk,
  input logic [7:0] status,
  input logic [7:0] drive,
  output logic [7:0] mirror,
  input logic [1:0] [7:0] lane_drive,
  output logic [1:0] [7:0] lane_mirror
);
  logic [7:0] internal_status;
  always_ff @(posedge clk) internal_status <= drive;
  assign mirror = internal_status;
  assign lane_mirror = lane_drive;
endmodule
"#;

const PROBE_COLLISION_TESTBENCH: &str = r#"
agent ProbeController
    function set_drive(value: uint<8>)
        dut.drive = value
    end function set_drive

    function mirror() -> uint<8>
        return dut.mirror
    end function mirror

    function set_lane(index: uint<8>, value: uint<8>)
        dut.lane_drive[index] = value
    end function set_lane

    function lane(index: uint<8>) -> uint<8>
        return dut.lane_mirror[index]
    end function lane

    function sample() -> uint<8>
        return dut.status
    end function sample

    function inject(value: uint<8>)
        dut.status = value
    end function inject

    function release_inject()
        release dut.status
    end function release_inject
end agent ProbeController

testbench ProbeCollisionTb
    let dut : TbirProbeCollision
        probe force status : uint<8> at internal_status
    end let dut
    probe_ctl : ProbeController
end testbench ProbeCollisionTb

impl ProbeCollisionA for ProbeCollisionTb
    clock clk = 10ns
    run
        probe_ctl.set_drive(4)
        probe_ctl.set_lane(0, 0x2a)
        wait 1 cycle
        assert probe_ctl.sample() == 4 else fail("A natural probe")
        assert probe_ctl.lane(0) == 0x2a else fail("A packed DUT lane")
        probe_ctl.inject(91)
        wait 1 cycle
        assert probe_ctl.sample() == 91 else fail("A forced probe")
        assert probe_ctl.mirror() == 91 else fail("A forced DUT path")
        probe_ctl.release_inject()
        probe_ctl.set_drive(6)
        wait 1 cycle
        assert probe_ctl.sample() == 6 else fail("A released probe")
        log(info, "PROBE_RESULT=A:${probe_ctl.sample()}")
    end run
end impl ProbeCollisionA

impl ProbeCollisionB for ProbeCollisionTb
    clock clk = 10ns
    run
        probe_ctl.set_drive(20)
        probe_ctl.set_lane(1, 0xb4)
        wait 1 cycle
        assert probe_ctl.sample() == 20 else fail("B natural probe")
        assert probe_ctl.lane(1) == 0xb4 else fail("B packed DUT lane")
        probe_ctl.inject(177)
        wait 1 cycle
        assert probe_ctl.sample() == 177 else fail("B forced probe")
        assert probe_ctl.mirror() == 177 else fail("B forced DUT path")
        probe_ctl.release_inject()
        probe_ctl.set_drive(22)
        wait 1 cycle
        assert probe_ctl.sample() == 22 else fail("B released probe")
        log(info, "PROBE_RESULT=B:${probe_ctl.sample()}")
    end run
end impl ProbeCollisionB
"#;

const PROBE_LIFECYCLE_TESTBENCH: &str = r#"
testbench ProbeLifecycleTb
    let dut : TbirProbeCollision
        probe status : uint<8> at internal_status
    end let dut

    on dut.status != 0
        log(info, "PROBE_LIFECYCLE_TRIGGERED")
    end on
end testbench ProbeLifecycleTb

impl ProbeLifecycle for ProbeLifecycleTb
    clock clk = 10ns
    run
        dut.drive = 9
        wait 2 cycles
    end run
end impl ProbeLifecycle
"#;

const PROBE_COLLISION_REGISTRY: &str = r#"#include "probe_collision__suite_api.hpp"
#include <cstdlib>
#include <string>

extern "C" const HarcTestDescriptor harc_test_ProbeCollisionA;
extern "C" const HarcTestDescriptor harc_test_ProbeCollisionB;

static int invoke(
    const HarcTestDescriptor& test,
    const std::string& directory,
    const char* tag) {
    std::string log = directory + "/" + tag + ".log";
    setenv("HARC_SEED", "707", 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    char arg0[] = "probe_collision";
    char* argv[] = {arg0, nullptr};
    return test.run(1, argv);
}

int main(int argc, char** argv) {
    if (argc != 2) return 90;
    std::string directory = argv[1];
    if (invoke(harc_test_ProbeCollisionA, directory, "a_first") != 0) return 1;
    if (invoke(harc_test_ProbeCollisionB, directory, "b_first") != 0) return 2;
    if (invoke(harc_test_ProbeCollisionB, directory, "b_second") != 0) return 3;
    if (invoke(harc_test_ProbeCollisionA, directory, "a_second") != 0) return 4;
    return 0;
}
"#;

const PROBELESS_COLLISION_TESTBENCH: &str = r#"
test ProbeLess
    let dut : TbirProbeCollision
    clock clk = 10ns
    run
        dut.drive = 3
        wait 1 cycle
        assert dut.mirror == 3 else fail("probe-less control")
    end run
end test ProbeLess
"#;

const DUT_ACCESS_MATRIX_DUT: &str = r#"
typedef logic signed [7:0] signed_byte_t;
module TbirDutAccessMatrix #(
  parameter int UNUSED = 1
)(
  input logic clk,
  input logic [64:0] d65,
  output logic [64:0] q65,
  input logic [127:0] d128,
  output logic [127:0] q128,
  input logic [128:0] d129,
  output logic [128:0] q129,
  input logic [199:0] d200,
  output logic [199:0] q200,
  input logic signed [7:0] signed_d8,
  output logic signed [64:0] signed_q65,
  output logic signed [128:0] signed_q129,
  output logic signed [199:0] signed_q200,
  input logic [7:0] send_req_data,
  output logic [7:0] send_rsp_data,
  input logic [1:0][7:0] lanes,
  output logic [7:0] selector,
  output signed_byte_t signed_out
);
  logic [199:0] internal_wide;
  logic signed [199:0] internal_signed_wide;
  assign q65 = d65;
  assign q128 = d128;
  assign q129 = d129;
  assign q200 = internal_wide;
  assign signed_q65 = signed_d8;
  assign signed_q129 = signed_d8;
  assign signed_q200 = internal_signed_wide;
  assign send_rsp_data = send_req_data;
  assign selector = 1;
  assign signed_out = -1;
  always_ff @(posedge clk) internal_wide <= d200;
  always_ff @(posedge clk) internal_signed_wide <= signed_d8;
endmodule
"#;

const DUT_ACCESS_MATRIX_TESTBENCH: &str = r#"
function choose_lane(value: uint<8>) -> uint<8>
    return value
end function choose_lane

function widen_signed(value: sint<65>) -> sint<200>
    return value
end function widen_signed

transactor DutAccessMatrixReader
    dut : TbirDutAccessMatrix

    hookable sample_aggregate() -> uint<8>
        return dut.send.rsp_data
    end sample_aggregate
end transactor DutAccessMatrixReader

testbench DutAccessMatrixTb
    let dut : TbirDutAccessMatrix
        probe force forced : uint<200> at internal_wide
        probe force signed_forced : sint<200> at internal_signed_wide
    end let dut
    reader : DutAccessMatrixReader

    function sample_aggregate(model: TbirDutAccessMatrix) -> uint<8>
        return model.send.rsp_data
    end function sample_aggregate

    function forward_aggregate(model: TbirDutAccessMatrix) -> uint<8>
        return sample_aggregate(model)
    end function forward_aggregate
end testbench DutAccessMatrixTb

impl DutAccessMatrix for DutAccessMatrixTb
    clock clk = 10ns
    run
        let value65 : uint<65> = 65.zext<65>()
        let value128 : uint<128> = 128.zext<128>()
        let value129 : uint<129> = 129.zext<129>()
        let value200 : uint<200> = 200.zext<200>()
        let negative : sint<8> = -1
        let negative129 : sint<129> = negative.sext<129>()
        let negative200 : sint<200> = negative.sext<200>()
        dut.d65 = value65
        dut.d128 = value128
        dut.d129 = value129
        dut.d200 = value200
        dut.signed_d8 = negative
        dut.send.req_data = 0x6b
        wait 1 cycle
        dut.lanes[choose_lane(dut.selector)] = 0x5a
        wait 1 cycle
        assert dut.q65 == value65 else fail("wide65")
        assert dut.q128 == value128 else fail("wide128")
        assert dut.q129 == value129 else fail("wide129")
        assert dut.q200 == value200 else fail("wide200")
        assert dut.signed_q129 == negative129 else fail("signed wide129")
        assert dut.signed_q200 == negative200 else fail("signed wide200")
        let inferred65 = dut.signed_q65
        let widened_from65 : sint<200> = inferred65
        let returned_from65 : sint<200> = widen_signed(inferred65)
        assert widened_from65 == negative200 else fail("signed inferred assignment widening")
        assert returned_from65 == negative200 else fail("signed helper return widening")
        assert dut.q200.trunc<8>() == 200 else fail("wide explicit narrow")
        assert dut.lanes[1] == 0x5a else fail("dynamic lane")
        assert sample_aggregate(dut) == 0x6b else fail("aggregate/module parameter")
        assert forward_aggregate(dut) == 0x6b else fail("forwarded module parameter")
        reader.dut = dut
        assert reader.sample_aggregate() == 0x6b else fail("aggregate/module field")
        let signed_value : sint<8> = dut.signed_out
        assert signed_value < 0 else fail("signed typedef")
        assert dut.signed_out < 0 else fail("signed inline expression")
        let inferred_signed = dut.signed_out
        assert inferred_signed < 0 else fail("signed inferred local")
        let unsigned_bits : uint<8> = dut.signed_out.trunc<8>()
        assert unsigned_bits == 0xff else fail("signed explicit normalization")
        let forced_value : uint<200> = 77.zext<200>()
        dut.forced = forced_value
        wait 1 cycle
        assert dut.forced == forced_value else fail("wide force")
        release dut.forced
        dut.d200 = 88.zext<200>()
        wait 1 cycle
        assert dut.forced == 88 else fail("wide release")
        dut.signed_forced = negative
        wait 1 cycle
        assert dut.signed_forced == negative200 else fail("signed wide force")
        release dut.signed_forced
        let restored : sint<8> = -2
        let restored200 : sint<200> = restored.sext<200>()
        dut.signed_d8 = restored
        wait 1 cycle
        assert dut.signed_forced == restored200 else fail("signed wide release")
        log(info, "DUT_ACCESS_MATRIX_PASS")
    end run
end impl DutAccessMatrix
"#;

const SEQUENTIAL_TESTBENCH: &str = r#"
test Common17
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        dut.d = 17
        wait 1 cycle
        assert dut.q == 17 else fail("Common17 observed the wrong value")
        log(info, "COMMON_RESULT=17")
    end run
end test Common17

test Common203
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        dut.d = 203
        wait 1 cycle
        assert dut.q == 203 else fail("Common203 observed the wrong value")
        log(info, "COMMON_RESULT=203")
    end run
end test Common203

test CommonFail
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        log(fatal, "EXPECTED_COMMON_FAILURE")
        wait 1 cycle
        log(error, "UNREACHABLE_AFTER_FATAL")
    end run
    check
        log(error, "UNREACHABLE_CHECK_AFTER_FATAL")
    end check
    teardown
        log(error, "UNREACHABLE_TEARDOWN_AFTER_FATAL")
    end teardown
end test CommonFail
"#;

const PHASED_TESTBENCH: &str = r#"
test Phased
    let dut : TbirCommonReg
    clock clk = 10ns
    setup
        log(info, "PHASE: setup")
        dut.d = 73
    end setup
    run
        log(info, "PHASE: run")
        wait 1 cycle
    end run
    check
        log(info, "PHASE: check")
        assert dut.q == 73 else fail("wrong final value")
    end check
    teardown
        log(info, "PHASE: teardown")
    end teardown
end test Phased
"#;

const RUNTIME_CELL_TESTBENCH: &str = r#"
property previous_q_zero
    past(dut.q) == 0
end property previous_q_zero

testbench RuntimeCellTb
    dut : TbirCommonReg
    service_hits : uint<16> default 0

    on 1 cycles phase post_eval
        service_hits = service_hits + 1
        log(info, "TB_PERIODIC")
    end on

    on dut.q != 0
        service_hits = service_hits + 10
        log(info, "TB_EDGE")
    end on
end testbench RuntimeCellTb

impl RuntimeCellA for RuntimeCellTb
    clock clk = 10ns
    run
        let local_events : event<uint<8>>
        on local_events(value)
            log(info, "A_LOCAL=${value}")
        end on
        assert property previous_q_zero
        on dut.q != 0
            log(info, "A_EDGE")
        end on
        on 1 cycles phase post_eval
            log(info, "A_PERIODIC")
        end on
        assert rose(dut.q)
        assert !fell(dut.q)
        assert !stable(dut.q)
        emit local_events(5)
        dut.d = 7
        wait 1 cycle
        assert service_hits == 1 else fail("A post-eval service count")
        log(info, "A_DONE")
    end run
end impl RuntimeCellA

impl RuntimeCellB for RuntimeCellTb
    clock clk = 10ns
    run
        let local_events : event<uint<8>>
        on local_events(value)
            log(info, "B_LOCAL=${value}")
        end on
        assert property previous_q_zero
        on dut.q != 0
            log(info, "B_EDGE")
        end on
        on 1 cycles phase post_eval
            log(info, "B_PERIODIC")
        end on
        assert rose(dut.q)
        assert !fell(dut.q)
        assert !stable(dut.q)
        emit local_events(6)
        dut.d = 9
        wait 1 cycle
        assert service_hits == 1 else fail("B post-eval service count")
        log(info, "B_DONE")
    end run
end impl RuntimeCellB

impl RuntimeCellFatal for RuntimeCellTb
    clock clk = 10ns
    run
        let local_events : event<uint<8>>
        on local_events(value)
            log(error, "UNREACHABLE_FATAL_LOCAL=${value}")
        end on
        assert property previous_q_zero
        on dut.q != 0
            log(error, "UNREACHABLE_FATAL_EDGE")
        end on
        on 1 cycles phase post_eval
            log(error, "UNREACHABLE_FATAL_PERIODIC")
        end on
        log(fatal, "EXPECTED_RUNTIME_CELL_FATAL")
        wait 1 cycle
    end run
end impl RuntimeCellFatal
"#;

const COMPONENT_LIFECYCLE_TESTBENCH: &str = r#"
agent LifecycleCell
    marker : uint<16> default 0
    periodic_hits : uint<16> default 0
    edge_hits : uint<16> default 0
    watchdog_hits : uint<16> default 0
    armed : uint<1> default 0

    function configure(value: uint<16>, enable: uint<1>)
        marker = value
        armed = enable
    end function configure

    function arm()
        armed = 1
    end function arm

    on 1 cycles phase post_eval
        periodic_hits = periodic_hits + 1
        log(info, "CELL_PERIODIC=${marker}:${periodic_hits}")
    end on

    on armed != 0
        edge_hits = edge_hits + 1
        log(info, "CELL_EDGE=${marker}:${edge_hits}")
    end on

    watchdog
        period 1 cycles
        max_idle 100 cycles
        watchdog_hits = watchdog_hits + 1
        log(info, "CELL_WATCHDOG=${marker}:${watchdog_hits}")
    end watchdog
end agent LifecycleCell

testbench ComponentLifecycleTb
    dut : TbirCommonReg
    left : LifecycleCell
    right : LifecycleCell
end testbench ComponentLifecycleTb

impl ComponentLifecycle for ComponentLifecycleTb
    clock clk = 10ns
    run
        left.configure(1, 1)
        right.configure(2, 0)
        wait 2 cycles
        assert left.periodic_hits == 2 else fail("left periodic")
        assert right.periodic_hits == 2 else fail("right periodic")
        assert left.edge_hits == 1 else fail("left rising")
        assert right.edge_hits == 0 else fail("right false edge")
        assert left.watchdog_hits == 1 else fail("left watchdog")
        assert right.watchdog_hits == 1 else fail("right watchdog")
        right.arm()
        wait 2 cycles
        assert left.periodic_hits == 4 else fail("left periodic second")
        assert right.periodic_hits == 4 else fail("right periodic second")
        assert left.edge_hits == 1 else fail("left edge repeated")
        assert right.edge_hits == 1 else fail("right rising")
        assert left.watchdog_hits == 3 else fail("left watchdog second")
        assert right.watchdog_hits == 3 else fail("right watchdog second")
        log(info, "COMPONENT_LIFECYCLE_DONE")
    end run
end impl ComponentLifecycle
"#;

const COMPONENT_HOOK_ISOLATION_TESTBENCH: &str = r#"
agent HookCell
    value : uint<16> default 0
    armed : uint<1> default 0
    edge_hits : uint<16> default 0
    _harc_hook_bump_pre : uint<16> default 41
    _u__harc_hook_bump_pre : uint<16> default 42
    _harc_hook_bump_post : uint<16> default 43

    hookable bump(delta: uint<16>)
        value = value + delta
    end bump

    hookable touch()
        value = value + 1
    end touch

    function arm()
        armed = 1
    end function arm

    on armed != 0
        edge_hits = edge_hits + 1
    end on
end agent HookCell

testbench HookIsolationTb
    dut : TbirCommonReg
    left : HookCell
    right : HookCell
    observed : uint<16> default 0
end testbench HookIsolationTb

impl HookIsolation for HookIsolationTb
    clock clk = 10ns
    run
        let offset : uint<16> = 3
        on left.bump pre
            observed = observed + delta + offset
            log(info, "LEFT_PRE=${observed}")
        end on
        on left.bump post
            observed = observed + 10
            log(info, "LEFT_POST=${observed}")
        end on
        on left.touch pre
            observed = observed + 100
            log(info, "LEFT_TOUCH=${observed}")
        end on
        right.bump(4)
        assert observed == 0 else fail("right instance cross-fired left hooks")
        left.bump(2)
        assert observed == 15 else fail("left hook order/capture ${observed}")
        right.touch()
        assert observed == 15 else fail("right touch cross-fired left hook")
        left.touch()
        assert observed == 115 else fail("single-side hook ${observed}")
        assert left.value == 3 else fail("left body")
        assert right.value == 5 else fail("right body")
        assert left._harc_hook_bump_pre == 41 else fail("user pre-named field")
        assert left._u__harc_hook_bump_pre == 42 else fail("user nested pre-named field")
        assert left._harc_hook_bump_post == 43 else fail("user post-named field")
        left.arm()
        wait 1 cycle
        wait 1 cycle
        assert left.edge_hits == 1 else fail("source lifecycle edge")
        assert right.edge_hits == 0 else fail("destination lifecycle edge before copy")
        right = left
        right.bump(4)
        assert observed == 115 else fail("component copy cloned hook metadata")
        wait 1 cycle
        assert left.edge_hits == 1 else fail("source lifecycle edge repeated")
        assert right.edge_hits == 2 else fail("component copy cloned lifecycle metadata")
        log(info, "HOOK_ISOLATION_DONE")
    end run
    check
        left.bump(1)
        assert observed == 129 else fail("run hook capture did not survive into check")
        assert left.value == 4 else fail("check hook body")
        log(info, "HOOK_CHECK_DONE")
    end check
end impl HookIsolation

impl HookNoSubscription for HookIsolationTb
    clock clk = 10ns
    run
        right.bump(7)
        left.touch()
        assert observed == 0 else fail("hook subscription leaked across runs")
        assert left.value == 1 else fail("left fresh state")
        assert right.value == 7 else fail("right fresh state")
        log(info, "HOOK_NO_SUBSCRIPTION_DONE")
    end run
end impl HookNoSubscription

impl ZHookFatal for HookIsolationTb
    clock clk = 10ns
    run
        let offset : uint<16> = 77
        on left.bump pre
            observed = observed + delta + offset
            log(error, "UNREACHABLE_FATAL_HOOK=${observed}")
        end on
        log(fatal, "EXPECTED_HOOK_FATAL")
        wait 1 cycle
    end run
end impl ZHookFatal

"#;

const COMPONENT_EVENT_CONNECT_TESTBENCH: &str = r#"
agent EventSourceCell
    observed : out event<uint<16>>

    function publish(value: uint<16>)
        emit observed(value)
    end function publish
end agent EventSourceCell

scoreboard EventSinkCell
    total : uint<16> default 0

    hookable accept(value: uint<16>)
        total = total + value
        log(info, "EVENT_ACCEPT=${value}:${total}")
    end accept
end scoreboard EventSinkCell

agent EventRelayCell
    incoming : event<uint<16>>
    total : uint<16> default 0

    on incoming(value)
        total = total + value
        log(info, "EVENT_RELAY=${value}:${total}")
    end on
end agent EventRelayCell

env EventPipe
    source : EventSourceCell
    sink : EventSinkCell
    relay : EventRelayCell
    connect
        source.observed -> sink.accept
        source.observed -> relay.incoming
    end connect
end env EventPipe

testbench EventConnectTb
    dut : TbirCommonReg
    left : EventPipe
    right : EventPipe
end testbench EventConnectTb

impl EventConnect for EventConnectTb
    clock clk = 10ns
    run
        left.source.publish(3)
        left.source.publish(4)
        emit left.source.observed(5)
        assert left.sink.total == 12 else fail("left event order/duplication")
        assert left.relay.total == 12 else fail("left relay order/duplication")
        assert right.sink.total == 0 else fail("right event cross-fire")
        assert right.relay.total == 0 else fail("right relay cross-fire")
        right.source.publish(9)
        assert left.sink.total == 12 else fail("left event cross-fire")
        assert left.relay.total == 12 else fail("left relay cross-fire")
        assert right.sink.total == 9 else fail("right event delivery")
        assert right.relay.total == 9 else fail("right relay delivery")
        log(info, "EVENT_CONNECT_DONE")
    end run
end impl EventConnect
"#;

const PERSISTENT_CAPTURE_TESTBENCH: &str = r#"
tseq DurableValues(seed: uint<8>) -> TSeq<uint<16>>
    yield seed.zext<16>()
end tseq DurableValues

agent DurableCell
    total : uint<16> default 0

    function add(value: uint<16>)
        total = total + value
    end function add

    hookable sample()
        total = total
    end sample
end agent DurableCell

testbench PersistentCaptureTb
    dut : TbirCommonReg
    cell : DurableCell
    hits : uint<16> default 0

    function bump()
        hits = hits + 1
    end function bump
end testbench PersistentCaptureTb

impl PersistentCapture for PersistentCaptureTb
    clock clk = 10ns
    run
        dut.d = 4
        wait 1 cycle
        let captured : uint<8> = 3
        let captured_signed8 = dut.signed_q8
        let captured_signed65 = dut.signed_q65
        assert captured == 3 && past(captured) <= 3
            else fail("captured property value ${captured}")
        on captured == 3
            log(info, "PERSISTENT_EDGE")
        end on
        on captured_signed8 < 0 && captured_signed65 < 0
            log(info, "PERSISTENT_SIGNED")
        end on
        on cell.sample post
            assert captured_signed8 < 0 else fail("method-hook signed8 capture")
            assert captured_signed65 < 0 else fail("method-hook signed65 capture")
            log(info, "METHOD_HOOK_SIGNED")
        end on
        cell.sample()
        on 1 cycles
            let values = DurableValues(3)
            for value in values
                cell.add(dut.q.zext<16>() + value)
            end for
            bump()
            log(info, "PERSISTENT_TICK=${dut.q}:${hits}")
        end on
    end run
    check
        wait 2 cycles
        assert hits == 2 else fail("persistent cycle capture count ${hits}")
        assert cell.total == 14 else fail("persistent component state ${cell.total}")
        log(info, "PERSISTENT_CAPTURE_DONE=${hits}")
    end check
end impl PersistentCapture
"#;

const HOOK_BINDINGS_TESTBENCH: &str = r#"
tseq HookTimed(seed: uint<8>) -> TSeq<uint<16>>
    wait 1 cycle
    yield seed.zext<16>()
end tseq HookTimed

agent HookBindingsCell
    total : uint<16> default 0

    hookable fire(delta: uint<8>)
        total = total + delta
    end fire

    function add(delta: uint<16>)
        total = total + delta
    end function add
end agent HookBindingsCell

testbench HookBindingsTb
    dut : TbirCommonReg
    cell : HookBindingsCell
    total : uint<16> default 0

    function bump(delta: uint<16>)
        total = total + delta
    end function bump

    on 1 cycles phase post_eval
        cell.add(1)
        bump(1)
        log(info, "HOOK_SERVICE")
    end on
end testbench HookBindingsTb

impl HookBindings for HookBindingsTb
    clock clk = 10ns
    run
        let local_event : event<uint<8>>
        on local_event(value)
            cell.add(value.zext<16>())
            bump(value.zext<16>())
            log(info, "EVENT_HOOK=${value}")
        end on
        on cell.fire post
            let values = HookTimed(delta)
            for value in values
                cell.add(value)
            end for
            bump(delta.zext<16>())
            wait 1 cycle
            cell.add(4)
            log(info, "METHOD_HOOK=${delta}")
        end on
        cell.fire(2)
        emit local_event(3)
        wait 1 cycle
        assert cell.total == 14 else fail("hook component total ${cell.total}")
        assert total == 8 else fail("hook testbench total ${total}")
        log(info, "HOOK_BINDINGS_DONE")
    end run
end impl HookBindings
"#;

const RUNTIME_CELL_NAME_COLLISION_TESTBENCH: &str = r#"
agent HarcRuntimeCells_RuntimeCellNames
    value : uint<8> default 1
end agent HarcRuntimeCells_RuntimeCellNames

agent HarcRunState_RuntimeCellNames
    value : uint<8> default 2
end agent HarcRunState_RuntimeCellNames

testbench RuntimeCellNameTb
    dut : TbirCommonReg
    _harc_runtime_cells : HarcRuntimeCells_RuntimeCellNames
    _harc_run_state : HarcRunState_RuntimeCellNames
    _last_in_cycle : uint<8> default 11
    _last_out_cycle : uint<8> default 13
    ticks : uint<8> default 0

    on 1 cycles
        ticks = ticks + 1
    end on
end testbench RuntimeCellNameTb

impl RuntimeCellNames for RuntimeCellNameTb
    clock clk = 10ns
    run
        let runtime_cells : uint<8> = 4
        let _harc_opaque_state : uint<8> = 5
        _harc_runtime_cells.value = runtime_cells
        _harc_run_state.value = _harc_opaque_state
        _last_in_cycle = _last_in_cycle + 1
        _last_out_cycle = _last_out_cycle + 1
        wait 2 cycles
        assert _harc_runtime_cells.value == 4 else fail("runtime-cell member collision")
        assert _harc_run_state.value == 5 else fail("run-state member collision")
        assert _last_in_cycle == 12 else fail("input-heartbeat member collision")
        assert _last_out_cycle == 14 else fail("output-heartbeat member collision")
        assert ticks == 1 else fail("runtime-cell callback collision")
        log(info, "RUNTIME_CELL_NAMES_DONE")
    end run
end impl RuntimeCellNames
"#;

const COMMON_COMPONENT_METHODS_TESTBENCH: &str = r#"
agent Counter
    value : uint<16> default 1
    hookable bump(delta: uint<16>) -> uint<16>
        value = value + delta
        return value
    end bump
end agent Counter

env CounterPair
    left : Counter
    right : Counter
    function sum_after(left_delta: uint<16>, right_delta: uint<16>) -> uint<16>
        let left_before = left.value
        let copied = bump_copy(left, left_delta)
        assert copied == left_before + left_delta else fail("component parameter value")
        assert left.value == left_before else fail("component parameter aliased receiver")
        let left_value = left.bump(left_delta)
        let right_value = right.bump(right_delta)
        return left_value + right_value
    end function sum_after
    function bump_copy(model: Counter, delta: uint<16>) -> uint<16>
        return model.bump(delta)
    end function bump_copy
end env CounterPair

testbench CounterTb
    dut : TbirCommonReg
    counters : CounterPair
end testbench CounterTb

impl CounterA for CounterTb
    clock clk = 10ns
    run
        let first = counters.sum_after(2, 7)
        let second = counters.sum_after(3, 11)
        assert first == 11 else fail("CounterA first result")
        assert second == 25 else fail("CounterA receiver state")
        log(info, "COMPONENT_RESULT=A:${first}:${second}")
    end run
end impl CounterA

impl CounterB for CounterTb
    clock clk = 10ns
    run
        let first = counters.sum_after(5, 1)
        assert first == 8 else fail("CounterB isolated state")
        log(info, "COMPONENT_RESULT=B:${first}")
    end run
end impl CounterB
"#;

const COMMON_TESTBENCH_METHODS_TESTBENCH: &str = r#"
transaction Beat
    value : uint<16> default 0
end transaction Beat

function combine(first: uint<16>, second: uint<16>) -> uint<32>
    return first * 1000 + second
end function combine

testbench MethodTb
    dut : TbirCommonReg
    count : uint<16> default 0
    saved : Beat

    function ordered() -> uint<32>
        return combine(count, later(2))
    end function ordered

    hookable later(delta: uint<16>) -> uint<16>
        count = count + delta
        return count
    end later

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

    function lazy_skip_or(disable: bool) -> bool
        return disable || later(5) != 0
    end function lazy_skip_or

    function lazy_choose(enable: bool) -> uint<16>
        return enable ? later(4) : count
    end function lazy_choose
end testbench MethodTb

impl MethodA for MethodTb
    clock clk = 10ns
    run
        count = 1
        let ordered_value = ordered()
        assert ordered_value == 1003 else fail("ordered=${ordered_value}")
        let skipped = lazy_take(false)
        assert !skipped && count == 3 else fail("lazy false count=${count}")
        let or_skipped = lazy_skip_or(true)
        assert or_skipped && count == 3 else fail("lazy true-or count=${count}")
        let unchosen = lazy_choose(false)
        assert unchosen == 3 && count == 3 else fail("lazy false ternary count=${count}")
        let beat : Beat
        beat.value = 9
        let copied = mirror(beat)
        assert copied.value == 9 else fail("record=${copied.value}")
        let saved_value = save(beat)
        assert saved_value == 10 else fail("saved=${saved_value}")
        log(info, "TB_METHOD_RESULT=A:${ordered_value}:${count}:${copied.value}:${saved_value}")
    end run
end impl MethodA

impl MethodB for MethodTb
    clock clk = 10ns
    run
        count = 4
        let taken = lazy_take(true)
        assert taken && count == 7 else fail("lazy true count=${count}")
        let or_taken = lazy_skip_or(false)
        assert or_taken && count == 12 else fail("lazy false-or count=${count}")
        let chosen = lazy_choose(true)
        assert chosen == 16 && count == 16 else fail("lazy true ternary count=${count}")
        let beat : Beat
        beat.value = 20
        let saved_value = save(beat)
        assert saved_value == 21 else fail("saved=${saved_value}")
        log(info, "TB_METHOD_RESULT=B:${count}:${saved_value}")
    end run
end impl MethodB
"#;

const COMMON_METHOD_TIMING_TESTBENCH: &str = r#"
function wait_once(value: uint<16>) -> uint<16>
    wait 1 cycle
    return value
end function wait_once

tseq PureValues(seed: uint<8>) -> TSeq<uint<16>>
    yield seed.zext<16>()
end tseq PureValues

tseq TimedValues(ctx: uint<8>) -> TSeq<uint<16>>
    wait 1 cycle
    yield ctx.zext<16>()
end tseq TimedValues

agent TimingAgent
    total : uint<16> default 0

    function wall(delta: uint<16>) -> uint<16>
        wait 1ps
        total = total + delta
        return total
    end function wall

    function helper_wait(value: uint<16>) -> uint<16>
        return wait_once(value)
    end function helper_wait

    function from_tseqs(seed: uint<8>) -> uint<16>
        let pure = PureValues(seed)
        let timed = TimedValues(seed + 1)
        let result : uint<16> = 0
        for value in pure
            result = result + value
        end for
        for value in timed
            result = result + value
        end for
        return result
    end function from_tseqs
end agent TimingAgent

testbench TimingTb
    dut : TbirCommonReg
    agent : TimingAgent
    total : uint<16> default 0

    function wall(delta: uint<16>) -> uint<16>
        wait 1ps
        total = total + delta
        return total
    end function wall

    function helper_wait(value: uint<16>) -> uint<16>
        return wait_once(value)
    end function helper_wait

    function from_tseqs(seed: uint<8>) -> uint<16>
        let pure = PureValues(seed)
        let timed = TimedValues(seed + 1)
        let result : uint<16> = 0
        for value in pure
            result = result + value
        end for
        for value in timed
            result = result + value
        end for
        return result
    end function from_tseqs
end testbench TimingTb

impl CommonMethodTiming for TimingTb
    clock clk = 10ns
    run
        let before = cycle_count
        let component_wall = agent.wall(3)
        let tb_wall = wall(5)
        let component_helper = agent.helper_wait(7)
        let tb_helper = helper_wait(11)
        let component_tseq = agent.from_tseqs(13)
        let tb_tseq = from_tseqs(17)
        assert component_wall == 3 else fail("component wall wait")
        assert tb_wall == 5 else fail("testbench wall wait")
        assert component_helper == 7 else fail("component helper wait")
        assert tb_helper == 11 else fail("testbench helper wait")
        assert component_tseq == 27 else fail("component tseq")
        assert tb_tseq == 35 else fail("testbench tseq")
        assert cycle_count == before + 4 else fail("method timing cycle count")
        log(info, "METHOD_TIMING_RESULT=${cycle_count}")
    end run
end impl CommonMethodTiming
"#;

const TESTBENCH_HOOK_RECORD_STATE_TESTBENCH: &str = r#"
transaction HookBeat
    value : uint<16> default 0
end transaction HookBeat

agent HookCounter
    value : uint<16> default 0
    hookable bump(delta: uint<16>)
        value = value + delta
    end bump
end agent HookCounter

testbench HookStateTb
    dut : TbirCommonReg
    counter : HookCounter
    saved : HookBeat
end testbench HookStateTb

impl HookRecordState for HookStateTb
    on counter.bump pre
        saved.value = saved.value + delta
    end on

    on counter.bump post
        saved.value = saved.value + 10
    end on

    clock clk = 10ns
    run
        saved.value = 1
        counter.bump(2)
        assert counter.value == 2 else fail("hookable body")
        assert saved.value == 13 else fail("hook record state ${saved.value}")
        log(info, "HOOK_RECORD_RESULT=${saved.value}")
    end run
end impl HookRecordState
"#;

const COMMON_RECEIVER_STATE_TESTBENCH: &str = r#"
transaction ReceiverBeat
    value : uint<16> default 0
end transaction ReceiverBeat

scoreboard ReceiverBoard
    count : uint<16> default 0
    pending : queue<ReceiverBeat>
end scoreboard ReceiverBoard

agent ReceiverCell
    value : uint<16> default 1
    board : ReceiverBoard

    function bump(ctx: uint<16>) -> uint<16>
        value = value + ctx
        board.count = board.count + 1
        let beat : ReceiverBeat
        beat.value = value
        board.pending.push(beat)
        let observed = board.pending.pop()
        assert observed.value == value else fail("component scoreboard queue")
        return value + board.count
    end function bump
end agent ReceiverCell

env ReceiverEnv
    cell : ReceiverCell

    function copied_bump(delta: uint<16>) -> uint<16>
        let before = cell.value
        let temp : ReceiverCell
        temp = cell
        let result = temp.bump(delta)
        assert cell.value == before else fail("component copy aliased")
        return result
    end function copied_bump
end env ReceiverEnv

testbench ReceiverTb
    dut : TbirCommonReg
    receiver : ReceiverEnv
    board : ReceiverBoard

    function invoke(_harc_tb_component_receiver: uint<16>) -> uint<16>
        board.count = board.count + 1
        let result = receiver.copied_bump(_harc_tb_component_receiver)
        return result + board.count
    end function invoke

    function local_collision(delta: uint<16>) -> uint<16>
        let _harc_tb_component_receiver = delta
        return receiver.copied_bump(_harc_tb_component_receiver)
    end function local_collision

    function receiver_value() -> uint<16>
        return receiver.cell.value
    end function receiver_value
end testbench ReceiverTb

impl ReceiverA for ReceiverTb
    clock clk = 10ns
    run
        let first = invoke(2)
        let second = invoke(3)
        assert first == 5 else fail("ReceiverA first ${first}")
        assert second == 7 else fail("ReceiverA second ${second}")
        assert local_collision(4) == 6 else fail("ReceiverA generated-name collision")
        assert receiver_value() == 1 else fail("ReceiverA receiver state")
        log(info, "RECEIVER_RESULT=A:${first}:${second}")
    end run
end impl ReceiverA

impl ReceiverB for ReceiverTb
    clock clk = 10ns
    run
        let first = invoke(5)
        assert first == 8 else fail("ReceiverB first ${first}")
        assert local_collision(4) == 6 else fail("ReceiverB generated-name collision")
        assert receiver_value() == 1 else fail("ReceiverB receiver state")
        log(info, "RECEIVER_RESULT=B:${first}")
    end run
end impl ReceiverB
"#;

const TESTBENCH_TLM_BINDING_ORDER_TESTBENCH: &str = r#"
bus LogicalOrderBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus LogicalOrderBus

testbench LogicalOrderTb
    dut : TlmMemory

    function read_once(addr: uint<8>) -> uint<32>
        let value = mem.read(addr)
        return value
    end function read_once

    function read_pair(addr: uint<8>) -> uint<32>
        let first = fork mem.read_ooo(addr)
        let second = fork mem.read_ooo(addr + 1)
        join_all
        return first + second
    end function read_pair
end testbench LogicalOrderTb

impl LogicalOrderA for LogicalOrderTb
    let mem : LogicalOrderBus = bind dut
    let spare : LogicalOrderBus = bind dut
    clock clk = 10ns
    run
        dut.rst = 1
        dut.mem_read_req_valid = 0
        dut.mem_read_rsp_ready = 0
        dut.mem_read_ooo_req_valid = 0
        dut.mem_read_ooo_rsp_ready = 0
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
        let observed = read_once(5)
        assert observed == 261 else fail("A observed=${observed}")
        let pair = read_pair(6)
        assert pair == 525 else fail("A pair=${pair}")
        log(info, "TLM_ORDER_RESULT=A:${observed}:${pair}")
    end run
end impl LogicalOrderA

impl LogicalOrderB for LogicalOrderTb
    let spare : LogicalOrderBus = bind dut
    let mem : LogicalOrderBus = bind dut
    clock clk = 10ns
    run
        dut.rst = 1
        dut.mem_read_req_valid = 0
        dut.mem_read_rsp_ready = 0
        dut.mem_read_ooo_req_valid = 0
        dut.mem_read_ooo_rsp_ready = 0
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
        let observed = read_once(9)
        assert observed == 265 else fail("B observed=${observed}")
        let pair = read_pair(10)
        assert pair == 533 else fail("B pair=${pair}")
        log(info, "TLM_ORDER_RESULT=B:${observed}:${pair}")
    end run
end impl LogicalOrderB
"#;

const SHARED_TYPES_AND_CALLABLES_TESTBENCH: &str = r#"
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

agent LeafState
    wide : uint<130>
    lanes : Vec<Vec<uint<8>, 2>, 3>
    last : InnerValue
    pending : queue<InnerValue>
    board : SharedScoreboard
end agent LeafState

env ParentState
    leaf : LeafState
end env ParentState

transactor StatefulTarget
    dut : TbirCommonReg
    count : uint<16> default 4
    last : InnerValue
    pending : queue<InnerValue>
end transactor StatefulTarget

covergroup StateCov @(posedge dut.clk)
    cp_data : cover dut.d
        bins
            zero = {0}
            nonzero = [1..255]
        end bins
end covergroup StateCov

function plus_one(x: uint<8>) -> uint<8>
    return x + 1
end function plus_one

function widen_plus_one(x: uint<8>) -> uint<130>
    return plus_one(x).zext<130>()
end function widen_plus_one

function wide_identity(value: uint<130>) -> uint<130>
    return value
end function wide_identity

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
    dut : TbirCommonReg
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
        assert last.lanes[0] == 9 && last.lanes[1] == 11 else fail("fixed vector changed")
        assert queued == 13 else fail("testbench queue value")
        log(info, "SHARED_RESULT=A")
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
        log(info, "SHARED_RESULT=B")
    end run
end impl SharedTypesB
"#;

const SEQUENTIAL_REGISTRY: &str = r#"#include "sequence__suite_api.hpp"
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <string>
#ifdef HARC_TEST_VALIDATE_FST
#include "gtkwave/fstapi.h"
#endif

#ifndef HARC_TEST_WAVE_EXTENSION
#define HARC_TEST_WAVE_EXTENSION "vcd"
#endif

extern "C" const HarcTestDescriptor harc_test_Common17;
extern "C" const HarcTestDescriptor harc_test_Common203;
extern "C" const HarcTestDescriptor harc_test_CommonFail;

static int live_frames = 0;
static int checker_hits = 0;
static int post_eval_hits = 0;
static int report_hits = 0;
static int late_frame_destructions = 0;
static int state_destructions = 0;
static int bad_state_destructions = 0;
static uint64_t first_probe_draw = 0;
static bool have_probe_draw = false;

struct ProbeState {
    HarcTestContext* ctx;
    bool alive = true;
    explicit ProbeState(HarcTestContext* context) : ctx(context) {}
    ~ProbeState() {
        if (!ctx->_checkers.empty() || !ctx->_post_eval_services.empty()
            || !ctx->_auto_cov_reports.empty() || live_frames != 0
            || ctx->dut == nullptr) {
            bad_state_destructions++;
        }
        alive = false;
        state_destructions++;
    }
};

static void* create_probe_state(HarcTestContext& ctx) {
    return new ProbeState(&ctx);
}

static void destroy_probe_state(void* opaque) {
    delete static_cast<ProbeState*>(opaque);
}

struct FrameGuard {
    HarcTestContext* ctx;
    explicit FrameGuard(HarcTestContext* context) : ctx(context) { live_frames++; }
    ~FrameGuard() {
        if (ctx->dut == nullptr) late_frame_destructions++;
        live_frames--;
    }
};

static harc_rt::HarcThread probe_body(
    HarcTestContext& ctx,
    harc_rt::ThreadSlot* slot,
    void* opaque) {
    auto& state = *static_cast<ProbeState*>(opaque);
    FrameGuard guard(&ctx);
    if (!ctx._checkers.empty() || !ctx._post_eval_services.empty()
        || !ctx._auto_cov_reports.empty() || ctx.errors != 0 || ctx.fatal
        || ctx.cycle_count != 0) {
        ctx.errors++;
    }
    uint64_t draw = ctx.rng.next();
    if (!have_probe_draw) {
        first_probe_draw = draw;
        have_probe_draw = true;
    }
    else if (draw != first_probe_draw) ctx.errors++;
    ctx._checkers.push_back([&]() { if (!state.alive) std::abort(); checker_hits++; });
    ctx._post_eval_services.push_back([&]() { if (!state.alive) std::abort(); post_eval_hits++; });
    ctx._auto_cov_reports.push_back([&]() { if (!state.alive) std::abort(); report_hits++; });
    co_await harc_rt::wait_cycles(slot, 1);
}

static harc_rt::HarcThread queue_fail_body(
    HarcTestContext& ctx,
    harc_rt::ThreadSlot* slot,
    void* opaque) {
    (void)opaque;
    FrameGuard guard(&ctx);
    harc_rt::HarcQueue<int> queue;
    (void)queue.pop();
    co_await harc_rt::wait_cycles(slot, 1);
}

static void configure_probe_clocks(HarcTestContext& ctx) {
    ctx.clocks.clear();
    ctx.clocks.push_back(HarcClockState{
        "clk", 5000, 5000, 0, 0, [&ctx](int level) { ctx.dut->clk = level; }});
    ctx.dut->clk = 0;
}

static const HarcTestRunDescriptor probe_test = {
    "Probe", &configure_probe_clocks, &create_probe_state, &probe_body, nullptr, &destroy_probe_state};
static const HarcTestRunDescriptor queue_fail_test = {
    "QueueFail", &configure_probe_clocks, &create_probe_state, &queue_fail_body, nullptr, &destroy_probe_state};

static int invoke(
    const HarcTestDescriptor& test,
    const std::string& directory,
    const char* tag) {
    std::string trace = directory + "/" + tag + ".jsonl";
    std::string log = directory + "/" + tag + ".log";
    std::string wave = directory + "/" + tag + "." HARC_TEST_WAVE_EXTENSION;
    setenv("HARC_SEED", "424242", 1);
    setenv("HARC_TRACE", trace.c_str(), 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    setenv("HARC_WAVE_FILE", wave.c_str(), 1);
    char arg0[] = "sequence";
    char* argv[] = {arg0, nullptr};
    return test.run(1, argv);
}

static int invoke_body(
    const HarcTestRunDescriptor& test,
    const std::string& directory,
    const char* tag) {
    std::string trace = directory + "/" + tag + ".jsonl";
    std::string log = directory + "/" + tag + ".log";
    std::string wave = directory + "/" + tag + "." HARC_TEST_WAVE_EXTENSION;
    setenv("HARC_SEED", "424242", 1);
    setenv("HARC_TRACE", trace.c_str(), 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    setenv("HARC_WAVE_FILE", wave.c_str(), 1);
    char arg0[] = "sequence";
    char* argv[] = {arg0, nullptr};
    return harc_run_test(test, 1, argv);
}

static bool artifact_is_closed(const std::filesystem::path& expected) {
    std::error_code error;
    for (const auto& entry : std::filesystem::directory_iterator("/proc/self/fd", error)) {
        auto target = std::filesystem::read_symlink(entry.path(), error);
        if (!error && target == expected) return false;
        error.clear();
    }
    return !error;
}

static bool finalized_artifacts(
    const std::string& directory,
    const char* tag) {
    std::filesystem::path base(directory);
    auto trace_path = base / (std::string(tag) + ".jsonl");
    auto log_path = base / (std::string(tag) + ".log");
    auto wave_path = base / (std::string(tag) + "." HARC_TEST_WAVE_EXTENSION);
    if (!artifact_is_closed(trace_path) || !artifact_is_closed(log_path)
        || !artifact_is_closed(wave_path)) return false;
    std::ifstream trace(trace_path);
    std::string trace_text(
        (std::istreambuf_iterator<char>(trace)), std::istreambuf_iterator<char>());
    std::ifstream wave(wave_path, std::ios::binary | std::ios::ate);
    if (trace_text.find("\"type\":\"sim_end\"") == std::string::npos
        || !wave || wave.tellg() <= 100) return false;
#ifdef HARC_TEST_VALIDATE_FST
    auto* fst = fstReaderOpen(wave_path.c_str());
    if (fst == nullptr) return false;
    fstReaderClose(fst);
#endif
    return true;
}

int main(int argc, char** argv) {
    if (argc != 2) return 90;
    std::string directory = argv[1];

    if (invoke(harc_test_Common17, directory, "same_a") != 0) return 1;
    if (invoke(harc_test_Common17, directory, "same_b") != 0) return 2;

    if (invoke(harc_test_Common17, directory, "ab_a") != 0) return 3;
    if (invoke(harc_test_Common203, directory, "ab_b") != 0) return 4;
    if (invoke(harc_test_Common203, directory, "ba_b") != 0) return 5;
    if (invoke(harc_test_Common17, directory, "ba_a") != 0) return 6;

    if (invoke_body(probe_test, directory, "probe_a") != 0) return 7;
    if (live_frames != 0) return 8;
    if (invoke_body(probe_test, directory, "probe_b") != 0) return 9;
    if (live_frames != 0) return 10;
    if (checker_hits != 2 || post_eval_hits != 2 || report_hits != 2) return 11;
    if (state_destructions != 2 || bad_state_destructions != 0) return 19;

    int outer_queue_hits = 0;
    {
        harc_rt::HarcQueueFatalScope outer([&]() { outer_queue_hits++; });
        if (invoke_body(queue_fail_test, directory, "queue_fail") == 0) return 12;
        if (live_frames != 0) return 13;
        if (!finalized_artifacts(directory, "queue_fail")) return 21;
        harc_rt::HarcQueue<int> queue;
        (void)queue.pop();
    }
    if (outer_queue_hits != 1) return 14;

    if (invoke(harc_test_CommonFail, directory, "harc_fail") == 0) return 15;
    if (!finalized_artifacts(directory, "harc_fail")) return 22;
    if (invoke(harc_test_Common17, directory, "after_fail") != 0) return 16;
    if (!finalized_artifacts(directory, "after_fail")) return 23;
    if (live_frames != 0) return 17;
    if (late_frame_destructions != 0) return 18;
    if (state_destructions != 3 || bad_state_destructions != 0) return 20;
    return 0;
}
"#;

const SOLVER_STATE_TESTBENCH: &str = r#"
transaction SolverStim
    tag : uint<4> with [unique within test]
    payload : uint<4>
    choice : uint<2> with [range(0, 3)]
    delta : sint<4>
    wide : uint<130>
    nonce : uint<64>
end transaction SolverStim

tseq SolverDraws(count: uint<8>) -> TSeq<SolverStim>
    let stimulus : SolverStim
    for i in 1 .. count
        randomize(stimulus) with
            stimulus.payload >= 0
            stimulus.payload <= 15
            stimulus.delta >= -8
            stimulus.delta <= 7
        end randomize
        assert stimulus.delta >= -8 && stimulus.delta <= 7
            else fail("signed solver result ${stimulus.delta}")
        yield stimulus
    end for
end tseq SolverDraws

test SolverState
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        let wide : uint<200> = 1.zext<200>() << 196
        logf("solver_details.log", info, "WIDE=${wide:050x}")
        let draws = SolverDraws(20)
        for stimulus in draws
            log(info, "SOLVER_DRAW=${stimulus.tag}:${stimulus.payload}:${stimulus.delta}:${stimulus.wide:033x}")
        end for
    end run
end test SolverState

test YSolverUnsat
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        let stimulus : SolverStim
        randomize(stimulus) with
            stimulus.payload < 4
            stimulus.payload > 12
        end randomize
    end run
end test YSolverUnsat

test ZSolverFatal
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        log(fatal, "intentional solver-state cleanup probe")
    end run
end test ZSolverFatal
"#;

const SOLVER_STATE_REGISTRY: &str = r#"#include "solver_state__suite_api.hpp"
#include <cstdlib>
#include <string>

extern "C" const HarcTestDescriptor harc_test_SolverState;
extern "C" const HarcTestDescriptor harc_test_YSolverUnsat;
extern "C" const HarcTestDescriptor harc_test_ZSolverFatal;

static int invoke(
    const HarcTestDescriptor& test,
    const std::string& directory,
    const char* tag,
    const char* seed) {
    std::string trace = directory + "/" + tag + ".jsonl";
    std::string log = directory + "/" + tag + ".log";
    std::string coverage = directory + "/" + tag + ".coverage.jsonl";
    setenv("HARC_SEED", seed, 1);
    setenv("HARC_TRACE", trace.c_str(), 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    setenv("HARC_COVERAGE_JSONL", coverage.c_str(), 1);
    char arg0[] = "solver_state";
    char* argv[] = {arg0, nullptr};
    return test.run(1, argv);
}

int main(int argc, char** argv) {
    if (argc != 2) return 90;
    std::string directory = argv[1];
    if (invoke(harc_test_SolverState, directory, "first", "909") != 0) return 1;
    if (invoke(harc_test_SolverState, directory, "different", "910") != 0) return 2;
    if (invoke(harc_test_YSolverUnsat, directory, "unsat", "909") == 0) return 3;
    if (invoke(harc_test_ZSolverFatal, directory, "fatal", "909") == 0) return 4;
    if (invoke(harc_test_SolverState, directory, "third", "909") != 0) return 5;
    return 0;
}
"#;

const COVERAGE_STATE_TESTBENCH: &str = r#"
covergroup RunCov @(posedge dut.clk)
    cp_data : cover dut.d
        bins
            zero = {0}
            nonzero = [1..255]
        end bins
    cp_q : cover dut.q
        bins
            zero = {0}
            nonzero = [1..255]
        end bins
    cross cp_data, cp_q
end covergroup RunCov

covergroup InputCov @(posedge dut.clk)
    cp_input : cover dut.d
        bins
            seven = {7}
            other = [0..6]
        end bins
end covergroup InputCov

testbench CoverageStateTb
    dut : TbirCommonReg
    cov : RunCov
    input_cov : InputCov
end testbench CoverageStateTb

impl CoverageState for CoverageStateTb
    clock clk = 10ns
    run
        cover dut.d == 7
        dut.d = 7
        wait 1 cycle
    end run
    check
        cov.report()
        input_cov.report()
    end check
end impl CoverageState
"#;

const COVERAGE_STATE_REGISTRY: &str = r#"#include "coverage_state__suite_api.hpp"
#include <cstdlib>
#include <string>

extern "C" const HarcTestDescriptor harc_test_CoverageState;

static int invoke(const std::string& directory, const char* tag) {
    std::string trace = directory + "/" + tag + ".jsonl";
    std::string log = directory + "/" + tag + ".log";
    std::string coverage = directory + "/" + tag + ".coverage.jsonl";
    setenv("HARC_SEED", "101", 1);
    setenv("HARC_TRACE", trace.c_str(), 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    setenv("HARC_COVERAGE_JSONL", coverage.c_str(), 1);
    char arg0[] = "coverage_state";
    char* argv[] = {arg0, nullptr};
    return harc_test_CoverageState.run(1, argv);
}

int main(int argc, char** argv) {
    if (argc != 2) return 90;
    std::string directory = argv[1];
    if (invoke(directory, "first") != 0) return 1;
    if (invoke(directory, "second") != 0) return 2;
    return 0;
}
"#;

const RUNTIME_CELL_REGISTRY: &str = r#"#include "runtime_cells__suite_api.hpp"
#include <cstdlib>
#include <string>

extern "C" const HarcTestDescriptor harc_test_RuntimeCellA;
extern "C" const HarcTestDescriptor harc_test_RuntimeCellB;
extern "C" const HarcTestDescriptor harc_test_RuntimeCellFatal;

static int invoke(
    const HarcTestDescriptor& test,
    const std::string& directory,
    const char* tag) {
    std::string trace = directory + "/" + tag + ".jsonl";
    std::string log = directory + "/" + tag + ".log";
    setenv("HARC_SEED", "606", 1);
    setenv("HARC_TRACE", trace.c_str(), 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    char arg0[] = "runtime_cells";
    char* argv[] = {arg0, nullptr};
    return test.run(1, argv);
}

int main(int argc, char** argv) {
    if (argc != 2) return 90;
    std::string directory = argv[1];
    if (invoke(harc_test_RuntimeCellA, directory, "a_first") != 0) return 1;
    if (invoke(harc_test_RuntimeCellB, directory, "b") != 0) return 2;
    if (invoke(harc_test_RuntimeCellFatal, directory, "fatal") == 0) return 3;
    if (invoke(harc_test_RuntimeCellA, directory, "a_second") != 0) return 4;
    return 0;
}
"#;

const COMPONENT_HOOK_REGISTRY: &str = r#"#include "component_hooks__suite_api.hpp"
#include <cstdlib>
#include <string>

extern "C" const HarcTestDescriptor harc_test_HookIsolation;
extern "C" const HarcTestDescriptor harc_test_HookNoSubscription;
extern "C" const HarcTestDescriptor harc_test_ZHookFatal;

static int invoke(
    const HarcTestDescriptor& test,
    const std::string& directory,
    const char* tag) {
    std::string log = directory + "/" + tag + ".log";
    setenv("HARC_SEED", "606", 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    char arg0[] = "component_hooks";
    char* argv[] = {arg0, nullptr};
    return test.run(1, argv);
}

int main(int argc, char** argv) {
    if (argc != 2) return 90;
    std::string directory = argv[1];
    if (invoke(harc_test_HookIsolation, directory, "hook_first") != 0) return 1;
    if (invoke(harc_test_ZHookFatal, directory, "hook_fatal") == 0) return 2;
    if (invoke(harc_test_HookNoSubscription, directory, "hook_none") != 0) return 3;
    if (invoke(harc_test_HookIsolation, directory, "hook_second") != 0) return 4;
    return 0;
}
"#;

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH"
    );
    present
}

fn fresh_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("harc_tbir_common_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temporary directory");
    path
}

fn write_fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let sv = dir.join("TbirCommonReg.sv");
    let tb = dir.join("tbir_common.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, TESTBENCH).expect("write HARC fixture");
    (sv, tb)
}

fn build_common_suite(sv: &Path, tb: &Path, top: &str, outdir: &Path) -> String {
    build_common_suite_with_args(sv, tb, top, outdir, &[])
}

fn build_common_suite_with_args(
    sv: &Path,
    tb: &Path,
    top: &str,
    outdir: &Path,
    extra: &[&str],
) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "tbir"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .args(["--emit-jobs", "2", "--jobs", "2"])
        .args(extra)
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run TB-IR common build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "common build failed:\n{log}");
    log
}

fn build_common_suite_mt(sv: &Path, tb: &Path, top: &str, outdir: &Path) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "tbir", "--mt"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .args(["--emit-jobs", "2", "--jobs", "2"])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run MT TB-IR common build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "MT common build failed:\n{log}");
    log
}

fn build_self_contained(sv: &Path, tb: &Path, top: &str, outdir: &Path) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "tbir"])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run TB-IR self-contained build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "self-contained build failed:\n{log}"
    );
    log
}

fn build_self_contained_mt(sv: &Path, tb: &Path, top: &str, outdir: &Path) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "tbir", "--mt"])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run MT TB-IR self-contained build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "MT self-contained build failed:\n{log}"
    );
    log
}

fn build_self_contained_with_param(
    sv: &Path,
    tb: &Path,
    top: &str,
    parameter: &str,
    outdir: &Path,
) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "tbir", "--param", parameter])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run parameterized TB-IR self-contained build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "parameterized self-contained build failed:\n{log}"
    );
    log
}

fn build_common_with_param(
    sv: &Path,
    tb: &Path,
    top: &str,
    parameter: &str,
    outdir: &Path,
) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "tbir", "--param", parameter])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .args(["--emit-jobs", "2", "--jobs", "2"])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run parameterized TB-IR common build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "parameterized common build failed:\n{log}"
    );
    log
}

fn build_v1(sv: &Path, tb: &Path, top: &str, outdir: &Path) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "v1"])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run v1 build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "v1 build failed:\n{log}");
    log
}

fn build_v1_common(sv: &Path, tb: &Path, top: &str, outdir: &Path) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "v1"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .args(["--emit-jobs", "2", "--jobs", "2"])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run v1 common build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "v1 common build failed:\n{log}");
    log
}

fn build_v1_mt(sv: &Path, tb: &Path, top: &str, outdir: &Path) -> String {
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(sv)
        .arg(tb)
        .args(["--top", top, "--codegen", "v1", "--mt"])
        .arg("--outdir")
        .arg(outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run MT v1 build");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "MT v1 build failed:\n{log}");
    log
}

fn manifest_abi(outdir: &Path, prefix: &str) -> String {
    let manifest = std::fs::read_to_string(outdir.join(format!("{prefix}artifacts.json")))
        .expect("read common manifest");
    serde_json::from_str::<serde_json::Value>(&manifest).expect("parse common manifest")
        ["interface_abi"]
        .as_str()
        .expect("manifest interface ABI")
        .to_string()
}

fn rebind_generated_abi_inputs(
    outdir: &Path,
    prefix: &str,
    old_abi: &str,
    abi_inputs: &[String],
) -> String {
    let old_symbol = format!("harc_suite_abi_{old_abi}");
    let interface_path = outdir.join(format!("{prefix}suite_api.hpp"));
    let interface = std::fs::read_to_string(&interface_path).expect("read suite interface");
    let interface_template = interface.replace(&old_symbol, ABI_ANCHOR_PLACEHOLDER);
    assert_ne!(interface, interface_template, "old ABI symbol was absent");
    let new_anchor = AbiAnchor::from_marked_interface_with_identity(
        &interface_template,
        harc::codegen::common_artifacts::CodegenBackend::Tbir,
        harc::codegen::common_artifacts::CppLayout::Common,
        abi_inputs,
    )
    .expect("derive second ABI anchor");
    assert_ne!(old_abi, new_anchor.digest());
    let interface = new_anchor
        .bind_declarations(&interface_template)
        .expect("bind second interface anchor");
    std::fs::write(&interface_path, interface).expect("write second suite interface");

    for filename in [
        format!("{prefix}runtime.cpp"),
        format!("{prefix}test_Common17.cpp"),
        format!("{prefix}test_Common203.cpp"),
        format!("{prefix}registry.cpp"),
    ] {
        let path = outdir.join(filename);
        let old = std::fs::read_to_string(&path).expect("read generated ABI artifact");
        let new = old.replace(old_abi, new_anchor.digest());
        assert_ne!(old, new, "generated artifact did not carry the old ABI");
        std::fs::write(path, new).expect("write second ABI artifact");
    }

    let manifest_path = outdir.join(format!("{prefix}artifacts.json"));
    let manifest = std::fs::read_to_string(&manifest_path).expect("read generated manifest");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&manifest).expect("parse generated manifest");
    manifest["interface_abi"] = serde_json::json!(new_anchor.digest());
    let mut manifest = serde_json::to_string(&manifest).expect("render second manifest");
    manifest.push('\n');
    std::fs::write(manifest_path, manifest).expect("write second manifest");
    new_anchor.digest().to_string()
}

fn undefined_symbols(object: &Path) -> String {
    let output = Command::new("nm")
        .arg("-u")
        .arg(object)
        .output()
        .expect("inspect generated object with nm");
    assert!(
        output.status.success(),
        "nm failed for {}",
        object.display()
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn relink(obj_dir: &Path, top: &str) -> std::process::Output {
    let binary = obj_dir.join(format!("V{top}"));
    if binary.exists() {
        std::fs::remove_file(&binary).expect("remove prior suite executable");
    }
    Command::new("make")
        .arg("-C")
        .arg(obj_dir)
        .arg("-f")
        .arg(format!("V{top}.mk"))
        .arg("-j1")
        .arg(format!("V{top}"))
        .arg("CFG_CXXFLAGS_STD=-std=gnu++20")
        .arg("CXX=c++")
        .output()
        .expect("relink generated suite")
}

#[test]
fn tbir_common_descriptors_are_isolated_across_sequential_same_process_runs() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_descriptors_are_isolated_across_sequential_same_process_runs: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("sequence_inputs");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("sequence.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, SEQUENTIAL_TESTBENCH).expect("write sequential HARC fixture");
    for format in ["vcd", "fst"] {
        let outdir = fresh_dir(&format!("sequence_output_{format}"));
        build_common_suite_with_args(
            &sv,
            &tb,
            "TbirCommonReg",
            &outdir,
            &["--waves", "--wave-format", format],
        );

        let registry = outdir.join("sequence__registry.cpp");
        let registry_source = if format == "fst" {
            format!(
                "#define HARC_TEST_WAVE_EXTENSION \"fst\"\n#define HARC_TEST_VALIDATE_FST 1\n{SEQUENTIAL_REGISTRY}"
            )
        } else {
            SEQUENTIAL_REGISTRY.to_string()
        };
        std::fs::write(&registry, registry_source).expect("install sequential registry harness");
        let registry_object = outdir.join("obj_dir/sequence__registry.o");
        if registry_object.exists() {
            std::fs::remove_file(&registry_object).expect("remove original registry object");
        }
        let link = relink(&outdir.join("obj_dir"), "TbirCommonReg");
        let link_log = format!(
            "{}{}",
            String::from_utf8_lossy(&link.stdout),
            String::from_utf8_lossy(&link.stderr)
        );
        assert!(
            link.status.success(),
            "{format} sequence harness failed to link:\n{link_log}"
        );

        let run = Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .arg(&outdir)
            .current_dir(&outdir)
            .output()
            .expect("run sequential harness");
        let run_log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{format} same-process sequence failed with {:?}:\n{run_log}",
            run.status.code()
        );

        let trace = |tag: &str| {
            std::fs::read(outdir.join(format!("{tag}.jsonl")))
                .unwrap_or_else(|error| panic!("read {tag} trace: {error}"))
        };
        assert_eq!(trace("same_a"), trace("same_b"));
        assert_eq!(trace("ab_a"), trace("ba_a"));
        assert_eq!(trace("ab_b"), trace("ba_b"));
        let after_fail = String::from_utf8(trace("after_fail")).expect("UTF-8 trace");
        assert!(after_fail.contains("\"cycle\":0,\"seq\":0"));
        assert!(after_fail.contains("\"errors\":0"));
        let fail_log =
            std::fs::read_to_string(outdir.join("harc_fail.log")).expect("read fail log");
        assert!(fail_log.contains("EXPECTED_COMMON_FAILURE"));
        assert!(!fail_log.contains("UNREACHABLE_AFTER_FATAL"));
        assert!(!fail_log.contains("UNREACHABLE_CHECK_AFTER_FATAL"));
        assert!(!fail_log.contains("UNREACHABLE_TEARDOWN_AFTER_FATAL"));

        for tag in [
            "same_a",
            "same_b",
            "ab_a",
            "ab_b",
            "ba_b",
            "ba_a",
            "probe_a",
            "probe_b",
            "queue_fail",
            "harc_fail",
            "after_fail",
        ] {
            let wave =
                std::fs::read(outdir.join(format!("{tag}.{format}"))).unwrap_or_else(|error| {
                    panic!("read {tag} {format} waveform after same-process run: {error}")
                });
            assert!(wave.len() > 100, "{tag} {format} waveform was empty");
            if format == "vcd" {
                assert!(
                    wave.windows(1).any(|window| window == b"#"),
                    "{tag} waveform has no timestamps"
                );
            }
        }

        let _ = std::fs::remove_dir_all(outdir);
    }

    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn tbir_common_cli_emits_vcd_fst_and_semantic_trace() {
    if !verilator_present() {
        eprintln!("SKIP tbir_common_cli_emits_vcd_fst_and_semantic_trace: `verilator` not found");
        return;
    }

    let inputs = fresh_dir("wave_trace_inputs");
    let (sv, tb) = write_fixture(&inputs);
    for format in ["vcd", "fst"] {
        let outdir = fresh_dir(&format!("wave_trace_{format}"));
        let trace = outdir.join("semantic.jsonl");
        build_common_suite_with_args(
            &sv,
            &tb,
            "TbirCommonReg",
            &outdir,
            &[
                "--waves",
                "--wave-format",
                format,
                "--record-trace",
                trace.to_str().expect("UTF-8 trace path"),
                "--test",
                "Common17",
            ],
        );
        let wave = std::fs::read(outdir.join(format!("Common17.{format}")))
            .unwrap_or_else(|error| panic!("read {format} waveform: {error}"));
        assert!(wave.len() > 100, "{format} waveform was empty");
        if format == "vcd" {
            assert!(wave.windows(1).any(|window| window == b"#"));
        }
        let semantic = std::fs::read_to_string(&trace).expect("read semantic trace");
        assert!(semantic.contains("\"type\":\"sim_start\""), "{semantic}");
        assert!(semantic.contains("\"type\":\"sim_end\""), "{semantic}");
        let _ = std::fs::remove_dir_all(outdir);
    }
    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn randomize_target_names_compile_and_run_in_every_layout() {
    if !verilator_present() {
        eprintln!(
            "SKIP randomize_target_names_compile_and_run_in_every_layout: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("randomize_names_inputs");
    let sv = inputs.join("RandomizeNamesTop.sv");
    let tb = inputs.join("randomize_names.harc");
    std::fs::write(
        &sv,
        "module RandomizeNamesTop(input logic clk);\nendmodule\n",
    )
    .expect("write randomize-name DUT fixture");
    std::fs::write(
        &tb,
        r#"transaction Req
    value : uint<8> with [unique within test]
    keep value in [1..7]
end transaction Req

test RandomizeNames
    let dut : RandomizeNamesTop
    run
        let ctx : Req
        randomize(ctx)
        let rng : Req
        randomize(rng)
        let errors : Req
        randomize(errors)
        let _cells : Req
        randomize(_cells)
        let _harc_randomize_context : Req
        randomize(_harc_randomize_context)
        let _harc_randomize_state : Req
        randomize(_harc_randomize_state)
        let _harc_randomize_state_ : Req
        randomize(_harc_randomize_state_)
        let _harc_randomize_capsule_state : Req
        randomize(_harc_randomize_capsule_state)
        assert ctx.value >= 1
        assert rng.value >= 1
        assert errors.value >= 1
        assert _cells.value >= 1
        assert _harc_randomize_state.value >= 1
        assert _harc_randomize_state_.value >= 1
        assert _harc_randomize_capsule_state.value >= 1
        log(info, "RANDOM_NAMES=${ctx.value}:${rng.value}:${errors.value}:${_cells.value}:${_harc_randomize_context.value}:${_harc_randomize_state.value}:${_harc_randomize_state_.value}:${_harc_randomize_capsule_state.value}")
    end run
end test RandomizeNames
"#,
    )
    .expect("write randomize-name HARC fixture");

    let v1_out = fresh_dir("randomize_names_v1");
    let v1_common_out = fresh_dir("randomize_names_v1_common");
    let self_out = fresh_dir("randomize_names_self");
    let common_out = fresh_dir("randomize_names_common");
    for log in [
        build_v1(&sv, &tb, "RandomizeNamesTop", &v1_out),
        build_v1_common(&sv, &tb, "RandomizeNamesTop", &v1_common_out),
        build_self_contained(&sv, &tb, "RandomizeNamesTop", &self_out),
        build_common_suite(&sv, &tb, "RandomizeNamesTop", &common_out),
    ] {
        assert!(
            log.contains("RANDOM_NAMES="),
            "randomize run was silent:\n{log}"
        );
        assert!(
            log.contains("ALL TESTS PASSED"),
            "randomize run failed:\n{log}"
        );
    }

    for path in [inputs, v1_out, v1_common_out, self_out, common_out] {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[test]
fn generated_runtime_binders_do_not_capture_user_globals_in_every_layout() {
    if !verilator_present() {
        eprintln!(
            "SKIP generated_runtime_binders_do_not_capture_user_globals_in_every_layout: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("global_name_collision_inputs");
    let sv = inputs.join("GlobalNameCollisionTop.sv");
    let tb = inputs.join("global_name_collision.harc");
    std::fs::write(
        &sv,
        "module GlobalNameCollisionTop(input logic clk);\nendmodule\n",
    )
    .expect("write global-name collision DUT fixture");
    std::fs::write(
        &tb,
        r#"const errors : uint<8> = 3
const rng : uint<8> = 4
const _cells : uint<8> = 5

function ctx(value : uint<8>) -> uint<8>
    return value + errors + rng + _cells
end function ctx

tseq harc_rng_next() -> TSeq<uint<8>>
    yield ctx(1)
    yield ctx(2)
end tseq harc_rng_next

test GlobalNameCollision
    let dut : GlobalNameCollisionTop
    run
        let values = harc_rng_next()
        let observed : uint<8> = 0
        for value in values
            observed = observed + value
        end for
        assert observed == 27
        log(info, "GLOBAL_NAME_COLLISION_OK")
    end run
end test GlobalNameCollision
"#,
    )
    .expect("write global-name collision HARC fixture");

    let builders: [(&str, fn(&Path, &Path, &str, &Path) -> String); 4] = [
        ("v1", build_v1),
        ("v1_common", build_v1_common),
        ("tbir_self", build_self_contained),
        ("tbir_common", build_common_suite),
    ];
    for (layout, build) in builders {
        let out = fresh_dir(&format!("global_name_collision_{layout}"));
        let log = build(&sv, &tb, "GlobalNameCollisionTop", &out);
        assert!(log.contains("GLOBAL_NAME_COLLISION_OK"), "{layout}: {log}");
        assert!(log.contains("ALL TESTS PASSED"), "{layout}: {log}");
        let _ = std::fs::remove_dir_all(out);
    }
    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn generated_runtime_binders_do_not_capture_enum_variants_in_every_layout() {
    if !verilator_present() {
        eprintln!(
            "SKIP generated_runtime_binders_do_not_capture_enum_variants_in_every_layout: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("enum_name_collision_inputs");
    let sv = inputs.join("EnumNameCollisionTop.sv");
    let tb = inputs.join("enum_name_collision.harc");
    std::fs::write(
        &sv,
        "module EnumNameCollisionTop(input logic clk);\nendmodule\n",
    )
    .expect("write enum-name collision DUT fixture");
    std::fs::write(
        &tb,
        r#"enum RuntimeName { ctx, errors, rng, _cells }

test EnumNameCollision
    let dut : EnumNameCollisionTop
    run
        let observed : uint<8> = ctx + errors + rng + _cells
        assert observed == 6
        log(info, "ENUM_NAME_COLLISION_OK")
    end run
end test EnumNameCollision
"#,
    )
    .expect("write enum-name collision HARC fixture");

    let builders: [(&str, fn(&Path, &Path, &str, &Path) -> String); 4] = [
        ("v1", build_v1),
        ("v1_common", build_v1_common),
        ("tbir_self", build_self_contained),
        ("tbir_common", build_common_suite),
    ];
    for (layout, build) in builders {
        let out = fresh_dir(&format!("enum_name_collision_{layout}"));
        let log = build(&sv, &tb, "EnumNameCollisionTop", &out);
        assert!(log.contains("ENUM_NAME_COLLISION_OK"), "{layout}: {log}");
        assert!(log.contains("ALL TESTS PASSED"), "{layout}: {log}");
        let _ = std::fs::remove_dir_all(out);
    }
    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn inserting_an_earlier_randomize_site_preserves_unrelated_capsule_and_seed_stream() {
    if !verilator_present() {
        eprintln!(
            "SKIP inserting_an_earlier_randomize_site_preserves_unrelated_capsule_and_seed_stream: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("stable_randomize_ids_inputs");
    let before_out = fresh_dir("stable_randomize_ids_before");
    let after_out = fresh_dir("stable_randomize_ids_after");
    let sv = inputs.join("StableRandomizeTop.sv");
    let tb = inputs.join("stable_ids.harc");
    std::fs::write(
        &sv,
        "module StableRandomizeTop(input logic clk);\nendmodule\n",
    )
    .expect("write stable-id DUT fixture");
    let source = |extra_site: bool| {
        let extra = if extra_site {
            "        let second : Req\n        randomize(second)\n"
        } else {
            ""
        };
        format!(
            r#"transaction Req
    value : uint<8> with [unique within test]
    keep value in [1..7]
end transaction Req

test Alpha
    let dut : StableRandomizeTop
    run
        let first : Req
        randomize(first)
{extra}    end run
end test Alpha

test Bravo
    let dut : StableRandomizeTop
    run
        let target : Req
        randomize(target)
        log(info, "BRAVO=${{target.value}}")
    end run
end test Bravo
"#
        )
    };

    let before_trace = before_out.join("bravo.jsonl");
    std::fs::write(&tb, source(false)).expect("write generation-A HARC fixture");
    build_common_suite_with_args(
        &sv,
        &tb,
        "StableRandomizeTop",
        &before_out,
        &[
            "--test",
            "Bravo",
            "--record-trace",
            before_trace.to_str().expect("UTF-8 trace path"),
        ],
    );
    let before_capsule = std::fs::read(before_out.join("stable_ids__test_Bravo.cpp"))
        .expect("read generation-A Bravo capsule");
    let before_trace = std::fs::read(&before_trace).expect("read generation-A Bravo trace");

    let after_trace = after_out.join("bravo.jsonl");
    std::fs::write(&tb, source(true)).expect("write generation-B HARC fixture");
    build_common_suite_with_args(
        &sv,
        &tb,
        "StableRandomizeTop",
        &after_out,
        &[
            "--test",
            "Bravo",
            "--record-trace",
            after_trace.to_str().expect("UTF-8 trace path"),
        ],
    );
    let after_capsule = std::fs::read(after_out.join("stable_ids__test_Bravo.cpp"))
        .expect("read generation-B Bravo capsule");
    let after_trace = std::fs::read(&after_trace).expect("read generation-B Bravo trace");

    assert_eq!(
        before_capsule, after_capsule,
        "Bravo capsule was renumbered"
    );
    assert_eq!(
        before_trace, after_trace,
        "Bravo's same-seed stream changed"
    );

    for path in [inputs, before_out, after_out] {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[test]
fn inserting_an_earlier_randomize_site_preserves_later_same_test_stream_in_every_layout() {
    if !verilator_present() {
        eprintln!(
            "SKIP inserting_an_earlier_randomize_site_preserves_later_same_test_stream_in_every_layout: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("same_test_stable_randomize_inputs");
    let sv = inputs.join("SameTestStableRandomizeTop.sv");
    let tb = inputs.join("same_test_stable_ids.harc");
    std::fs::write(
        &sv,
        "module SameTestStableRandomizeTop(input logic clk);\nendmodule\n",
    )
    .expect("write same-test stable-id DUT fixture");
    let source = |extra_site: bool| {
        let extra = if extra_site {
            "        randomize(earlier)\n"
        } else {
            ""
        };
        format!(
            r#"transaction Earlier
    value : uint<64>
end transaction Earlier

transaction Target
    first : uint<64>
    second : uint<64>
end transaction Target

test Stable
    let dut : SameTestStableRandomizeTop
    run
        let earlier : Earlier
        let target : Target
{extra}        randomize(target)
        log(info, "SAME_TEST_TARGET=${{target.first}}:${{target.second}}")
    end run
end test Stable
"#
        )
    };
    let result_line = |log: &str| {
        log.lines()
            .find(|line| line.contains("SAME_TEST_TARGET="))
            .unwrap_or_else(|| panic!("missing stable-target line:\n{log}"))
            .to_string()
    };
    let builders: [(&str, fn(&Path, &Path, &str, &Path) -> String); 4] = [
        ("v1", build_v1),
        ("v1_common", build_v1_common),
        ("tbir_self", build_self_contained),
        ("tbir_common", build_common_suite),
    ];

    for (layout, build) in builders {
        let before_out = fresh_dir(&format!("same_test_stable_{layout}_before"));
        let after_out = fresh_dir(&format!("same_test_stable_{layout}_after"));
        std::fs::write(&tb, source(false)).expect("write before-insertion fixture");
        let before = result_line(&build(&sv, &tb, "SameTestStableRandomizeTop", &before_out));
        std::fs::write(&tb, source(true)).expect("write after-insertion fixture");
        let after = result_line(&build(&sv, &tb, "SameTestStableRandomizeTop", &after_out));
        assert_eq!(before, after, "{layout} later-site stream changed");
        let _ = std::fs::remove_dir_all(before_out);
        let _ = std::fs::remove_dir_all(after_out);
    }

    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn unique_within_test_is_shared_across_sites_in_every_layout() {
    if !verilator_present() {
        eprintln!(
            "SKIP unique_within_test_is_shared_across_sites_in_every_layout: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("unique_across_sites_inputs");
    let sv = inputs.join("UniqueAcrossSitesTop.sv");
    let tb = inputs.join("unique_across_sites.harc");
    std::fs::write(
        &sv,
        "module UniqueAcrossSitesTop(input logic clk);\nendmodule\n",
    )
    .expect("write unique-scope DUT fixture");
    std::fs::write(
        &tb,
        r#"transaction Token
    value : uint<2> with [unique within test]
end transaction Token

test UniqueAcrossSites
    let dut : UniqueAcrossSitesTop
    run
        let a : Token
        let b : Token
        let c : Token
        let d : Token
        randomize(a)
        randomize(b)
        randomize(c)
        randomize(d)
        assert a.value != b.value
        assert a.value != c.value
        assert a.value != d.value
        assert b.value != c.value
        assert b.value != d.value
        assert c.value != d.value
        log(info, "UNIQUE_ACROSS_SITES=${a.value}:${b.value}:${c.value}:${d.value}")
    end run
end test UniqueAcrossSites
"#,
    )
    .expect("write unique-scope HARC fixture");

    let builders: [(&str, fn(&Path, &Path, &str, &Path) -> String); 4] = [
        ("v1", build_v1),
        ("v1_common", build_v1_common),
        ("tbir_self", build_self_contained),
        ("tbir_common", build_common_suite),
    ];
    for (layout, build) in builders {
        let out = fresh_dir(&format!("unique_across_sites_{layout}"));
        let log = build(&sv, &tb, "UniqueAcrossSitesTop", &out);
        assert!(log.contains("UNIQUE_ACROSS_SITES="), "{layout}: {log}");
        assert!(log.contains("ALL TESTS PASSED"), "{layout}: {log}");
        let _ = std::fs::remove_dir_all(out);
    }
    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn sequencer_unique_registry_avoids_user_field_collision_in_every_layout() {
    if !verilator_present() {
        eprintln!(
            "SKIP sequencer_unique_registry_avoids_user_field_collision_in_every_layout: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("sequencer_unique_collision_inputs");
    let sv = inputs.join("SequencerUniqueCollisionTop.sv");
    let tb = inputs.join("sequencer_unique_collision.harc");
    std::fs::write(
        &sv,
        "module SequencerUniqueCollisionTop(input logic clk);\nendmodule\n",
    )
    .expect("write sequencer unique collision DUT fixture");
    std::fs::write(
        &tb,
        r#"transaction Token
    value : uint<2> with [unique within sequencer]
end transaction Token

transaction TseqToken
    value : uint<2> with [unique within tseq]
end transaction TseqToken

transaction NestedToken
    value : uint<1> with [unique within tseq]
end transaction NestedToken

tseq InnerUnique() -> TSeq<NestedToken>
    let inner : NestedToken
    randomize(inner) with
        inner.value == 1
    end randomize
    assert inner.value == 1
    yield inner
end tseq InnerUnique

tseq OuterUnique() -> TSeq<NestedToken>
    let before : NestedToken
    let final_token : NestedToken
    randomize(before) with
        before.value == 0
    end randomize
    let nested = InnerUnique()
    randomize(final_token) with
        final_token.value >= 0
    end randomize
    assert final_token.value == 1
    log(info, "TSEQ_RESET=${before.value}:${final_token.value}")
    yield before
    yield final_token
end tseq OuterUnique

tseq Pair() -> TSeq<TseqToken>
    let _harc_unique_tseq : uint<8> = 0
    let _harc_unique_tseq_1 : uint<8> = 0
    let first : TseqToken
    let second : TseqToken
    randomize(first) with
        first.value >= 0
    end randomize
    randomize(second) with
        second.value >= 0
    end randomize
    assert _harc_unique_tseq == 0
    assert _harc_unique_tseq_1 == 0
    assert first.value != second.value
    log(info, "TSEQ_UNIQUE=${first.value}:${second.value}")
    yield first
    yield second
end tseq Pair

sequencer Source
    _harc_unique : uint<8> default 0
    _harc_unique_1 : uint<8> default 0

    hookable draw(owner : uint<8>)
        let first : Token
        let second : Token
        randomize(first) with
            first.value >= 0
        end randomize
        randomize(second) with
            second.value >= 0
        end randomize
        assert _harc_unique == 0
        assert _harc_unique_1 == 0
        assert first.value != second.value
        log(info, "SEQUENCER_UNIQUE=${owner}:${first.value}:${second.value}")
    end draw
end sequencer Source

test SequencerUniqueCollision
    let dut : SequencerUniqueCollisionTop
    let first : Source
    let second : Source
    run
        first.draw(1)
        second.draw(2)
        first.draw(1)
        second.draw(2)
        let first_pair = Pair()
        let second_pair = Pair()
        let nested_pair = OuterUnique()
    end run
end test SequencerUniqueCollision
"#,
    )
    .expect("write sequencer unique collision HARC fixture");

    let builders: [(&str, fn(&Path, &Path, &str, &Path) -> String); 4] = [
        ("v1", build_v1),
        ("v1_common", build_v1_common),
        ("tbir_self", build_self_contained),
        ("tbir_common", build_common_suite),
    ];
    for (layout, build) in builders {
        let out = fresh_dir(&format!("sequencer_unique_collision_{layout}"));
        let log = build(&sv, &tb, "SequencerUniqueCollisionTop", &out);
        let mut sequencer_values = std::collections::BTreeMap::<u64, Vec<u64>>::new();
        for line in log
            .lines()
            .filter(|line| line.contains("SEQUENCER_UNIQUE="))
        {
            let values = line
                .split("SEQUENCER_UNIQUE=")
                .nth(1)
                .expect("sequencer unique payload")
                .split(':')
                .map(|value| value.parse::<u64>().expect("sequencer unique integer"))
                .collect::<Vec<_>>();
            assert_eq!(values.len(), 3, "{layout}: {line}");
            sequencer_values
                .entry(values[0])
                .or_default()
                .extend_from_slice(&values[1..]);
        }
        assert_eq!(sequencer_values.len(), 2, "{layout}: {log}");
        for (owner, values) in sequencer_values {
            assert_eq!(values.len(), 4, "{layout} owner {owner}: {log}");
            assert_eq!(
                values
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                4,
                "{layout} owner {owner} reset or shared its unique history: {log}"
            );
        }
        let tseq_lines = log
            .lines()
            .filter(|line| line.contains("TSEQ_UNIQUE="))
            .collect::<Vec<_>>();
        assert_eq!(tseq_lines.len(), 2, "{layout}: {log}");
        assert!(log.contains("TSEQ_RESET=0:1"), "{layout}: {log}");
        assert!(log.contains("ALL TESTS PASSED"), "{layout}: {log}");
        let _ = std::fs::remove_dir_all(out);
    }
    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn same_record_same_offset_auto_coverage_has_distinct_source_qualified_ids() {
    if !verilator_present() {
        eprintln!(
            "SKIP same_record_same_offset_auto_coverage_has_distinct_source_qualified_ids: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("source_qualified_auto_cov_inputs");
    let sv = inputs.join("AutoCovTop.sv");
    let types = inputs.join("types.harc");
    let alpha = inputs.join("alpha.harc");
    let bravo = inputs.join("bravo.harc");
    let test = inputs.join("test.harc");
    std::fs::write(&sv, "module AutoCovTop(input logic clk);\nendmodule\n")
        .expect("write auto-coverage DUT fixture");
    std::fs::write(
        &types,
        "transaction Req\n    value : uint<8>\nend transaction Req\n",
    )
    .expect("write shared record fixture");
    std::fs::write(
        &alpha,
        r#"agent Alpha
    hookable draw()
        let item : Req
        randomize(item)
    end draw
end agent Alpha
"#,
    )
    .expect("write Alpha fixture");
    std::fs::write(
        &bravo,
        r#"agent Bravo
    hookable draw()
        let item : Req
        randomize(item)
    end draw
end agent Bravo
"#,
    )
    .expect("write Bravo fixture");
    std::fs::write(
        &test,
        r#"test AutoCovIds
    let dut : AutoCovTop
    let alpha : Alpha
    let bravo : Bravo
    run
        alpha.draw()
        bravo.draw()
    end run
end test AutoCovIds
"#,
    )
    .expect("write auto-coverage test fixture");

    let mut reference_ids = None;
    for (layout, common) in [("v1", false), ("self", false), ("common", true)] {
        let outdir = fresh_dir(&format!("source_qualified_auto_cov_{layout}"));
        let coverage = outdir.join("coverage.jsonl");
        let codegen = if layout == "v1" { "v1" } else { "tbir" };
        let mut command = Command::new(harc_bin());
        command
            .arg("sim")
            .arg("--sv")
            .arg(&sv)
            .arg(&types)
            .arg(&alpha)
            .arg(&bravo)
            .arg(&test)
            .args(["--top", "AutoCovTop", "--codegen", codegen])
            .arg("--outdir")
            .arg(&outdir)
            .env("HARC_SEED", "17")
            .env("HARC_COVERAGE_JSONL", &coverage);
        if common {
            command
                .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
                .args(["--emit-jobs", "2", "--jobs", "2"]);
        }
        let output = command.output().expect("build and run auto-coverage suite");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "{layout} auto-coverage run failed:\n{log}"
        );
        let report = std::fs::read_to_string(&coverage).expect("read auto-coverage JSON");
        let ids = report
            .lines()
            .filter_map(|line| {
                let event: serde_json::Value = serde_json::from_str(line).expect("coverage JSON");
                (event["type"] == "auto_cover" && event["record"] == "Req")
                    .then(|| event["span"].as_u64().expect("u64 site identity"))
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 2, "{layout} merged same-offset sites:\n{report}");
        if let Some(reference) = &reference_ids {
            assert_eq!(&ids, reference, "{layout} site identities differ by layout");
        } else {
            reference_ids = Some(ids);
        }
        let _ = std::fs::remove_dir_all(outdir);
    }

    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn tbir_common_solver_state_is_fresh_across_same_seed_same_process_runs() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_solver_state_is_fresh_across_same_seed_same_process_runs: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("solver_state_inputs");
    let self_out = fresh_dir("solver_state_self");
    let v1_out = fresh_dir("solver_state_v1");
    let outdir = fresh_dir("solver_state_output");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("solver_state.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, SOLVER_STATE_TESTBENCH).expect("write solver-state fixture");
    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_v1(&sv, &tb, "TbirCommonReg", &v1_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &outdir);

    let registry = outdir.join("solver_state__registry.cpp");
    std::fs::write(&registry, SOLVER_STATE_REGISTRY).expect("install solver-state registry");
    let registry_object = outdir.join("obj_dir/solver_state__registry.o");
    if registry_object.exists() {
        std::fs::remove_file(&registry_object).expect("remove original registry object");
    }
    let link = relink(&outdir.join("obj_dir"), "TbirCommonReg");
    let link_log = format!(
        "{}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );
    assert!(
        link.status.success(),
        "solver-state harness failed to link:\n{link_log}"
    );

    let run = Command::new(outdir.join("obj_dir/VTbirCommonReg"))
        .arg(&outdir)
        .current_dir(&outdir)
        .output()
        .expect("run solver-state harness");
    let run_log = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.status.success(), "solver-state run failed:\n{run_log}");

    let first = std::fs::read(outdir.join("first.jsonl")).expect("read first trace");
    let third = std::fs::read(outdir.join("third.jsonl")).expect("read third trace");
    assert_eq!(
        first, third,
        "same-seed runs in one process must reset solver iterations and unique history"
    );

    let parse_draws = |bytes: &[u8]| {
        String::from_utf8(bytes.to_vec())
            .expect("UTF-8 trace")
            .lines()
            .filter_map(|line| {
                let event: serde_json::Value =
                    serde_json::from_str(line).expect("parse trace event");
                (event["type"] == "randomize").then(|| {
                    (
                        event["fields"]["tag"].as_u64().expect("unsigned tag"),
                        event["fields"]["payload"]
                            .as_u64()
                            .expect("unsigned payload"),
                        event["fields"]["choice"].as_u64().expect("unsigned choice"),
                        event["fields"]["delta"].as_i64().expect("signed delta"),
                        event["fields"]["wide"]
                            .as_str()
                            .expect("wide hexadecimal string")
                            .to_string(),
                        event["fields"]["nonce"]
                            .as_u64()
                            .expect("seed-sensitive nonce"),
                    )
                })
            })
            .collect::<Vec<_>>()
    };
    let first_draws = parse_draws(&first);
    let different = std::fs::read(outdir.join("different.jsonl")).expect("read different trace");
    let different_draws = parse_draws(&different);
    assert_eq!(
        first_draws.len(),
        20,
        "expected one randomize event per loop iteration"
    );
    assert_eq!(different_draws.len(), 20);
    assert_ne!(
        first_draws, different_draws,
        "different seeds reused one random stream"
    );
    assert_eq!(
        first_draws
            .iter()
            .take(16)
            .map(|draw| draw.0)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        16,
        "[unique within test] repeated before exhausting its 4-bit domain"
    );
    assert_eq!(
        first_draws
            .iter()
            .skip(16)
            .map(|draw| draw.0)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4,
        "[unique within test] did not begin a fresh no-repeat epoch after exhaustion"
    );
    assert!(
        first_draws.iter().all(|draw| draw.1 <= 15),
        "constraint solve produced an out-of-range payload: {first_draws:?}"
    );
    assert!(
        first_draws.iter().all(|draw| draw.2 <= 3),
        "range metadata produced an out-of-range choice: {first_draws:?}"
    );
    assert!(
        first_draws.iter().all(|draw| (-8..=7).contains(&draw.3)),
        "signed constraint solve produced an out-of-range delta: {first_draws:?}"
    );
    assert!(
        first_draws.iter().all(|draw| {
            let digits = draw.4.strip_prefix("0x").expect("wide hexadecimal prefix");
            digits.len() == 33
                && digits
                    .as_bytes()
                    .first()
                    .is_some_and(|digit| matches!(digit, b'0'..=b'3'))
        }),
        "wide solver value escaped its 130-bit representation: {first_draws:?}"
    );
    for (layout, layout_out) in [("self", &self_out), ("v1", &v1_out)] {
        let trace = layout_out.join(format!("{layout}.jsonl"));
        let run = Command::new(layout_out.join("obj_dir/VTbirCommonReg"))
            .args(["--test", "SolverState"])
            .current_dir(layout_out)
            .env("HARC_SEED", "909")
            .env("HARC_TRACE", &trace)
            .env("HARC_SIM_LOG", layout_out.join(format!("{layout}.log")))
            .env(
                "HARC_COVERAGE_JSONL",
                layout_out.join(format!("{layout}.coverage.jsonl")),
            )
            .output()
            .unwrap_or_else(|error| panic!("run {layout} solver state: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} solver-state run failed:\n{log}"
        );
        let layout_trace = std::fs::read(&trace).expect("read layout solver trace");
        assert_eq!(
            parse_draws(&layout_trace),
            first_draws,
            "{layout} signed/wide solver stream differs from common"
        );
    }

    let first_coverage = std::fs::read(outdir.join("first.coverage.jsonl"))
        .expect("read first auto-coverage report");
    let third_coverage = std::fs::read(outdir.join("third.coverage.jsonl"))
        .expect("read third auto-coverage report");
    assert_eq!(
        first_coverage, third_coverage,
        "automatic coverage state leaked between descriptor runs"
    );
    let coverage = String::from_utf8(first_coverage).expect("UTF-8 auto-coverage report");
    assert_eq!(coverage.matches("\"type\":\"auto_cover\"").count(), 1);
    assert!(coverage.contains("\"record\":\"SolverStim\""));
    let detail =
        std::fs::read_to_string(outdir.join("solver_details.log")).expect("read per-run file log");
    assert!(
        detail.contains("WIDE=10000000000000000000000000000000000000000000000000"),
        "wide file formatting truncated or changed:\n{detail}"
    );
    assert_eq!(
        detail.lines().count(),
        1,
        "per-run file logging appended through a stale handle:\n{detail}"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(outdir);
}

#[test]
fn tbir_common_coverage_json_is_owned_and_closed_per_run() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_coverage_json_is_owned_and_closed_per_run: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("coverage_state_inputs");
    let self_out = fresh_dir("coverage_state_self");
    let v1_out = fresh_dir("coverage_state_v1");
    let outdir = fresh_dir("coverage_state_output");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("coverage_state.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COVERAGE_STATE_TESTBENCH).expect("write coverage-state fixture");
    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_v1(&sv, &tb, "TbirCommonReg", &v1_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &outdir);

    let registry = outdir.join("coverage_state__registry.cpp");
    std::fs::write(&registry, COVERAGE_STATE_REGISTRY).expect("install coverage-state registry");
    let registry_object = outdir.join("obj_dir/coverage_state__registry.o");
    if registry_object.exists() {
        std::fs::remove_file(&registry_object).expect("remove original registry object");
    }
    let link = relink(&outdir.join("obj_dir"), "TbirCommonReg");
    let link_log = format!(
        "{}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );
    assert!(
        link.status.success(),
        "coverage-state harness failed to link:\n{link_log}"
    );

    let run = Command::new(outdir.join("obj_dir/VTbirCommonReg"))
        .arg(&outdir)
        .current_dir(&outdir)
        .output()
        .expect("run coverage-state harness");
    let run_log = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.status.success(),
        "coverage-state run failed:\n{run_log}"
    );

    for (layout, layout_out) in [("self", &self_out), ("v1", &v1_out)] {
        let coverage = layout_out.join(format!("{layout}.coverage.jsonl"));
        let run = Command::new(layout_out.join("obj_dir/VTbirCommonReg"))
            .args(["--test", "CoverageState"])
            .current_dir(layout_out)
            .env("HARC_SEED", "101")
            .env("HARC_COVERAGE_JSONL", &coverage)
            .env("HARC_SIM_LOG", layout_out.join(format!("{layout}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} coverage fixture: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(run.status.success(), "{layout} coverage run failed:\n{log}");
    }

    let first = std::fs::read_to_string(outdir.join("first.coverage.jsonl"))
        .expect("read first coverage report");
    let second = std::fs::read_to_string(outdir.join("second.coverage.jsonl"))
        .expect("read second coverage report");
    assert_eq!(
        first, second,
        "coverage state leaked between descriptor runs"
    );
    assert_eq!(first.matches("\"type\":\"covergroup\"").count(), 2);
    assert_eq!(first.matches("\"type\":\"coverpoint_bin\"").count(), 6);
    assert_eq!(first.matches("\"type\":\"cross\"").count(), 1);
    assert_eq!(first.matches("\"type\":\"cross_bin\"").count(), 4);
    assert_eq!(first.matches("\"type\":\"cover\"").count(), 1);
    assert_eq!(first.matches("\"type\":\"cover_point\"").count(), 1);
    assert_eq!(
        first,
        std::fs::read_to_string(self_out.join("self.coverage.jsonl"))
            .expect("read self-contained coverage report"),
        "self-contained coverage JSON differs from common"
    );
    assert_eq!(
        first,
        std::fs::read_to_string(v1_out.join("v1.coverage.jsonl")).expect("read v1 coverage report"),
        "v1 coverage JSON differs from common"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(outdir);
}

#[test]
fn tbir_common_hook_triggered_coverage_json_matches_all_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_hook_triggered_coverage_json_matches_all_layouts: \
             `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let self_out = fresh_dir("hook_coverage_self");
    let v1_out = fresh_dir("hook_coverage_v1");
    let common_out = fresh_dir("hook_coverage_common");
    let sv = root.join("tests/dut/top_counter.sv");
    let tb = root.join("tests/fixtures/component_early_return_post_cover_test.harc");
    build_self_contained(&sv, &tb, "Top", &self_out);
    build_v1(&sv, &tb, "Top", &v1_out);
    build_common_suite(&sv, &tb, "Top", &common_out);

    let mut reports = Vec::new();
    for (layout, outdir) in [
        ("self", &self_out),
        ("v1", &v1_out),
        ("common", &common_out),
    ] {
        let coverage = outdir.join(format!("{layout}.coverage.jsonl"));
        let run = Command::new(outdir.join("obj_dir/VTop"))
            .args(["--test", "EarlyReturnTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5150")
            .env("HARC_COVERAGE_JSONL", &coverage)
            .env("HARC_SIM_LOG", outdir.join(format!("{layout}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} hook coverage: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} hook coverage failed:\n{log}"
        );
        reports.push(std::fs::read_to_string(coverage).expect("read hook coverage report"));
    }
    assert_eq!(reports[0], reports[1], "v1 hook coverage JSON differs");
    assert_eq!(reports[0], reports[2], "common hook coverage JSON differs");
    assert_eq!(reports[0].matches("\"type\":\"covergroup\"").count(), 1);
    assert_eq!(reports[0].matches("\"type\":\"coverpoint_bin\"").count(), 2);
    assert!(!reports[0].contains("\"type\":\"cross\""));
    assert!(!reports[0].contains("\"type\":\"cross_bin\""));

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_extern_string_and_ref_source_profile_match_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_extern_string_and_ref_source_profile_match_self_contained: \
             `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("extern_string_inputs");
    let self_out = fresh_dir("extern_string_self");
    let common_out = fresh_dir("extern_string_common");
    let sv = root.join("tests/dut/top_counter.sv");
    let tb = root.join("tests/fixtures/string_extern_args_parity_test.harc");
    let ref_src = inputs.join("string_extern_args.cpp");
    std::fs::copy(root.join("tests/dut/string_extern_args.cpp"), &ref_src)
        .expect("copy reference source");

    let build = |layout: &str, outdir: &Path| {
        let output = Command::new(harc_bin())
            .arg("sim")
            .arg("--sv")
            .arg(&sv)
            .arg(&tb)
            .args(["--top", "Top", "--codegen", "tbir"])
            .args(["--cpp-split", "tests", "--cpp-split-layout", layout])
            .arg("--ref-src")
            .arg(&ref_src)
            .arg("--outdir")
            .arg(outdir)
            .env("HARC_SEED", "31337")
            .output()
            .expect("build extern/string fixture");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{layout} build failed:\n{log}");
        log
    };
    build("self-contained", &self_out);
    build("common", &common_out);

    let run = |outdir: &Path, tag: &str| {
        Command::new(outdir.join("obj_dir/VTop"))
            .args(["--test", "StringExternArgsParityTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "31337")
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .output()
            .expect("run extern/string fixture")
    };
    let self_run = run(&self_out, "self");
    let common_run = run(&common_out, "common");
    for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.status.success(), "{layout} run failed:\n{log}");
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).expect("read self trace"),
        std::fs::read(common_out.join("common.jsonl")).expect("read common trace"),
        "extern/string trace differs by layout"
    );
    let interface =
        std::fs::read_to_string(common_out.join("string_extern_args_parity_test__suite_api.hpp"))
            .expect("read common interface");
    assert!(interface.contains("extern \"C\" {"));
    assert!(interface.contains("ref_string_choice(const char* key"));

    let manifest_path = common_out.join("string_extern_args_parity_test__artifacts.json");
    let first_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).expect("read first manifest"),
    )
    .expect("parse first manifest");
    // A reference-source edit is Verilator's makefile's business: the
    // build profile keys on the source *path*, so the common objects and
    // the published artifacts stay valid, while the native build still
    // recompiles the edited translation unit and relinks the binary.
    let identity_path = common_out.join("obj_dir/.harc_build_identity");
    let first_identity =
        std::fs::read_to_string(&identity_path).expect("read first build identity");
    let binary_path = common_out.join("obj_dir/VTop");
    let first_binary_mtime = std::fs::metadata(&binary_path)
        .and_then(|meta| meta.modified())
        .expect("first binary mtime");
    let mut changed = std::fs::read_to_string(&ref_src).expect("read reference source copy");
    changed.push_str("\n// profile invalidation probe\n");
    std::fs::write(&ref_src, changed).expect("update reference source copy");
    let rebuild_log = build("common", &common_out);
    let second_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).expect("read rebuilt manifest"),
    )
    .expect("parse rebuilt manifest");
    assert_eq!(
        first_manifest["build_profile"], second_manifest["build_profile"],
        "reference-source content must not invalidate the common build profile"
    );
    assert!(
        rebuild_log.contains(", 0 rewritten,"),
        "reference-source edit must not republish common artifacts:\n{rebuild_log}"
    );
    assert_eq!(
        std::fs::read_to_string(&identity_path).expect("read rebuilt build identity"),
        first_identity,
        "reference-source content must not evict the native build directory"
    );
    let second_binary_mtime = std::fs::metadata(&binary_path)
        .and_then(|meta| meta.modified())
        .expect("rebuilt binary mtime");
    assert!(
        second_binary_mtime > first_binary_mtime,
        "the native build must relink after a reference-source edit:\n{rebuild_log}"
    );
    let rerun = run(&common_out, "common_rebuilt");
    assert!(
        rerun.status.success(),
        "rebuilt common binary run failed:\n{}{}",
        String::from_utf8_lossy(&rerun.stdout),
        String::from_utf8_lossy(&rerun.stderr)
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn common_extern_signature_change_rejects_optimized_stale_capsule_in_both_backends() {
    if !verilator_present() {
        eprintln!(
            "SKIP common_extern_signature_change_rejects_optimized_stale_capsule_in_both_backends: \
             `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("extern_signature_inputs");
    let sv = root.join("tests/dut/top_counter.sv");
    let tb = inputs.join("extern_sig.harc");
    let reference = inputs.join("extern_impl.cpp");
    for codegen in ["tbir", "v1"] {
        let outdir = fresh_dir(&format!("extern_signature_output_{codegen}"));
        let build = |two_args: bool, common: bool, build_out: &Path| {
            let declaration = if two_args {
                "extern function ctx(x: uint<8>, y: uint<8>) -> uint<8>"
            } else {
                "extern function ctx(x: uint<8>) -> uint<8>"
            };
            let call = if two_args { "ctx(1, 2)" } else { "ctx(3)" };
            std::fs::write(
                &tb,
                format!(
                    "{declaration}\n\n\
                     test ExternSig\n\
                         let dut : Top\n\
                         run\n\
                             let value = {call}\n\
                             assert value == 3\n\
                         end run\n\
                     end test ExternSig\n"
                ),
            )
            .expect("write extern HARC source");
            let cpp = if two_args {
                "#include <cstdint>\nextern \"C\" uint64_t ctx(uint64_t x, uint64_t y) { return x + y; }\n"
            } else {
                "#include <cstdint>\nextern \"C\" uint64_t ctx(uint64_t x) { return x; }\n"
            };
            std::fs::write(&reference, cpp).expect("write extern reference source");
            let mut command = Command::new(harc_bin());
            command.arg("sim").arg("--sv").arg(&sv).arg(&tb).args([
                "--top",
                "Top",
                "--codegen",
                codegen,
            ]);
            if common {
                command.args(["--cpp-split", "tests", "--cpp-split-layout", "common"]);
            }
            let output = command
                .arg("--ref-src")
                .arg(&reference)
                .arg("--outdir")
                .arg(build_out)
                .output()
                .expect("build extern-signature suite");
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "{codegen} {} extern-signature build failed:\n{log}",
                if common { "common" } else { "self-contained" }
            );
        };

        build(false, true, &outdir);
        let self_out = fresh_dir(&format!("extern_signature_self_{codegen}"));
        build(false, false, &self_out);
        let _ = std::fs::remove_dir_all(self_out);
        let abi_a = manifest_abi(&outdir, "extern_sig__");
        let stale_object_path = outdir.join("obj_dir/extern_sig__test_ExternSig.o");
        let stale_object = std::fs::read(&stale_object_path).expect("save stale capsule");
        assert!(
            undefined_symbols(&stale_object_path).contains(&format!("harc_suite_abi_{abi_a}")),
            "{codegen} optimized capsule did not retain its ABI relocation"
        );

        build(true, true, &outdir);
        let abi_b = manifest_abi(&outdir, "extern_sig__");
        assert_ne!(
            abi_a, abi_b,
            "{codegen} extern signature did not change common ABI"
        );
        std::fs::write(&stale_object_path, stale_object).expect("restore stale capsule");
        let stale_link = relink(&outdir.join("obj_dir"), "Top");
        let stale_log = format!(
            "{}{}",
            String::from_utf8_lossy(&stale_link.stdout),
            String::from_utf8_lossy(&stale_link.stderr)
        );
        assert!(
            !stale_link.status.success(),
            "{codegen} stale extern capsule linked:\n{stale_log}"
        );
        assert!(
            stale_log.contains(&format!("harc_suite_abi_{abi_a}")),
            "{codegen} stale-link error did not name old ABI anchor:\n{stale_log}"
        );

        let _ = std::fs::remove_dir_all(outdir);
    }

    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn tbir_common_probe_plan_owns_the_stub_and_matches_self_contained_runtime() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_probe_plan_owns_the_stub_and_matches_self_contained_runtime: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("probe_access_inputs");
    let common_out = fresh_dir("probe_access_common");
    let self_out = fresh_dir("probe_access_self");
    let sv = inputs.join("TbirProbeCollision.sv");
    let tb = inputs.join("probe_collision.harc");
    std::fs::write(&sv, PROBE_COLLISION_DUT).expect("write probe DUT fixture");
    std::fs::write(&tb, PROBE_COLLISION_TESTBENCH).expect("write probe HARC fixture");

    build_self_contained(&sv, &tb, "TbirProbeCollision", &self_out);
    build_common_suite(&sv, &tb, "TbirProbeCollision", &common_out);

    let manifest = std::fs::read_to_string(common_out.join("probe_collision__artifacts.json"))
        .expect("read probe manifest");
    let manifest: serde_json::Value =
        serde_json::from_str(&manifest).expect("parse probe manifest");
    let artifacts = manifest["artifacts"]
        .as_array()
        .expect("manifest artifact list");
    assert!(
        artifacts.iter().any(
            |artifact| artifact["filename"] == "probe_collision__probe_stub.sv"
                && artifact["role"] == "probe_stub"
        ),
        "probe stub is not manifest-owned: {manifest}"
    );
    let stub = std::fs::read_to_string(common_out.join("probe_collision__probe_stub.sv"))
        .expect("read manifest-owned probe stub");
    let self_stub = std::fs::read_to_string(self_out.join("__harc_probe_TbirProbeCollision.sv"))
        .expect("read self-contained probe stub");
    assert_eq!(stub, self_stub, "verified-IR and AST probe stubs diverged");
    assert!(
        stub.contains("assign status = TbirProbeCollision.internal_status;"),
        "{stub}"
    );
    assert!(
        stub.contains("force TbirProbeCollision.internal_status = status_drv;"),
        "{stub}"
    );
    assert!(
        !common_out
            .join("__harc_probe_TbirProbeCollision.sv")
            .exists(),
        "common layout must not emit a second AST-derived probe stub"
    );
    let interface = std::fs::read_to_string(common_out.join("probe_collision__suite_api.hpp"))
        .expect("read probe interface");
    let runtime = std::fs::read_to_string(common_out.join("probe_collision__runtime.cpp"))
        .expect("read probe runtime");
    let capsule =
        std::fs::read_to_string(common_out.join("probe_collision__test_ProbeCollisionA.cpp"))
            .expect("read probe capsule");
    assert!(!interface.contains("___024root.h"), "{interface}");
    assert!(
        runtime.contains("#include \"VTbirProbeCollision___024root.h\""),
        "{runtime}"
    );
    assert!(!capsule.contains("___024root.h"), "{capsule}");

    for test in ["ProbeCollisionA", "ProbeCollisionB"] {
        let mut logs = Vec::new();
        for (layout, outdir) in [("self", &self_out), ("common", &common_out)] {
            let log_path = outdir.join(format!("{test}_{layout}.log"));
            let output = Command::new(outdir.join("obj_dir/VTbirProbeCollision"))
                .args(["--test", test])
                .env("HARC_SEED", "707")
                .env("HARC_SIM_LOG", &log_path)
                .current_dir(outdir)
                .output()
                .unwrap_or_else(|error| panic!("run {layout} {test}: {error}"));
            let output_log = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "{layout} {test} failed with {:?}:\n{output_log}",
                output.status.code()
            );
            let log = std::fs::read_to_string(&log_path)
                .unwrap_or_else(|error| panic!("read {layout} {test} log: {error}"));
            assert!(log.contains("PROBE_RESULT="), "{layout} {test}:\n{log}");
            assert!(!log.contains("FAIL"), "{layout} {test}:\n{log}");
            logs.push(log);
        }
        assert_eq!(
            logs[0], logs[1],
            "self/common probe trace diverged for {test}"
        );
    }

    let registry = common_out.join("probe_collision__registry.cpp");
    std::fs::write(&registry, PROBE_COLLISION_REGISTRY)
        .expect("install same-process probe registry");
    let registry_object = common_out.join("obj_dir/probe_collision__registry.o");
    if registry_object.exists() {
        std::fs::remove_file(&registry_object).expect("remove original probe registry object");
    }
    let link = relink(&common_out.join("obj_dir"), "TbirProbeCollision");
    let link_log = format!(
        "{}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );
    assert!(
        link.status.success(),
        "probe sequence relink failed:\n{link_log}"
    );
    let run = Command::new(common_out.join("obj_dir/VTbirProbeCollision"))
        .arg(&common_out)
        .current_dir(&common_out)
        .output()
        .expect("run A/B/B/A probe sequence");
    let run_log = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.status.success(),
        "same-process probe sequence failed with {:?}:\n{run_log}",
        run.status.code()
    );
    for (first, second) in [("a_first", "a_second"), ("b_first", "b_second")] {
        let first = std::fs::read(common_out.join(format!("{first}.log")))
            .expect("read first same-process probe log");
        let second = std::fs::read(common_out.join(format!("{second}.log")))
            .expect("read repeated same-process probe log");
        assert_eq!(
            first, second,
            "probe state leaked between same-process runs"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_probe_only_lifecycle_trigger_compiles_and_runs_in_both_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_probe_only_lifecycle_trigger_compiles_and_runs_in_both_layouts: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("probe_lifecycle_inputs");
    let common_out = fresh_dir("probe_lifecycle_common");
    let self_out = fresh_dir("probe_lifecycle_self");
    let sv = inputs.join("TbirProbeCollision.sv");
    let tb = inputs.join("probe_lifecycle.harc");
    std::fs::write(&sv, PROBE_COLLISION_DUT).expect("write lifecycle probe DUT");
    std::fs::write(&tb, PROBE_LIFECYCLE_TESTBENCH).expect("write lifecycle probe test");

    build_self_contained(&sv, &tb, "TbirProbeCollision", &self_out);
    build_common_suite(&sv, &tb, "TbirProbeCollision", &common_out);

    for (layout, outdir) in [("self", &self_out), ("common", &common_out)] {
        let log_path = outdir.join(format!("{layout}.log"));
        let output = Command::new(outdir.join("obj_dir/VTbirProbeCollision"))
            .args(["--test", "ProbeLifecycle"])
            .env("HARC_SEED", "707")
            .env("HARC_SIM_LOG", &log_path)
            .current_dir(outdir)
            .output()
            .unwrap_or_else(|error| panic!("run {layout} lifecycle probe: {error}"));
        let output_log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{layout}: {output_log}");
        let log = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|error| panic!("read {layout} lifecycle probe log: {error}"));
        assert!(log.contains("PROBE_LIFECYCLE_TRIGGERED"), "{layout}: {log}");
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_dut_access_matrix_matches_common_and_self_contained_runtime() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_dut_access_matrix_matches_common_and_self_contained_runtime: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("dut_access_matrix_inputs");
    let common_out = fresh_dir("dut_access_matrix_common");
    let self_out = fresh_dir("dut_access_matrix_self");
    let parameterized_out = fresh_dir("dut_access_matrix_parameterized");
    let parameterized_common_out = fresh_dir("dut_access_matrix_parameterized_common");
    let sv = inputs.join("TbirDutAccessMatrix.sv");
    let tb = inputs.join("dut_access_matrix.harc");
    std::fs::write(&sv, DUT_ACCESS_MATRIX_DUT).expect("write DUT-access matrix RTL");
    std::fs::write(&tb, DUT_ACCESS_MATRIX_TESTBENCH).expect("write DUT-access matrix HARC");

    build_self_contained(&sv, &tb, "TbirDutAccessMatrix", &self_out);
    build_common_suite(&sv, &tb, "TbirDutAccessMatrix", &common_out);
    build_self_contained_with_param(
        &sv,
        &tb,
        "TbirDutAccessMatrix",
        "UNUSED=2",
        &parameterized_out,
    );
    build_common_with_param(
        &sv,
        &tb,
        "TbirDutAccessMatrix",
        "UNUSED=2",
        &parameterized_common_out,
    );

    let mut logs = Vec::new();
    for (layout, outdir) in [("self", &self_out), ("common", &common_out)] {
        let log_path = outdir.join(format!("{layout}.log"));
        let output = Command::new(outdir.join("obj_dir/VTbirDutAccessMatrix"))
            .args(["--test", "DutAccessMatrix"])
            .env("HARC_SEED", "707")
            .env("HARC_SIM_LOG", &log_path)
            .current_dir(outdir)
            .output()
            .unwrap_or_else(|error| panic!("run {layout} DUT-access matrix: {error}"));
        let output_log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "{layout} DUT-access matrix failed with {:?}:\n{output_log}",
            output.status.code()
        );
        let log = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|error| panic!("read {layout} DUT-access log: {error}"));
        assert!(log.contains("DUT_ACCESS_MATRIX_PASS"), "{layout}:\n{log}");
        assert!(!log.contains("FAIL"), "{layout}:\n{log}");
        logs.push(log);
    }
    assert_eq!(logs[0], logs[1], "DUT-access layout traces diverged");

    let parameterized_cpp =
        std::fs::read_to_string(parameterized_out.join("dut_access_matrix.cpp"))
            .expect("read parameterized self-contained source");
    assert!(parameterized_cpp.contains("dut->send_rsp_data"));
    assert!(!parameterized_cpp.contains("dut->send.rsp_data"));
    let parameterized_common_runtime =
        std::fs::read_to_string(parameterized_common_out.join("dut_access_matrix__runtime.cpp"))
            .expect("read parameterized common runtime");
    assert!(parameterized_common_runtime.contains("self.dut->send_rsp_data"));
    assert!(!parameterized_common_runtime.contains("self.dut->send.rsp_data"));

    let mut parameterized_logs = Vec::new();
    for (layout, outdir) in [
        ("self", &parameterized_out),
        ("common", &parameterized_common_out),
    ] {
        let log_path = outdir.join(format!("parameterized_{layout}.log"));
        let output = Command::new(outdir.join("obj_dir/VTbirDutAccessMatrix"))
            .args(["--test", "DutAccessMatrix"])
            .env("HARC_SEED", "707")
            .env("HARC_SIM_LOG", &log_path)
            .current_dir(outdir)
            .output()
            .unwrap_or_else(|error| panic!("run parameterized {layout}: {error}"));
        let output_log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "parameterized {layout}: {output_log}"
        );
        parameterized_logs.push(
            std::fs::read_to_string(&log_path)
                .unwrap_or_else(|error| panic!("read parameterized {layout} log: {error}")),
        );
    }
    assert_eq!(
        parameterized_logs[0], parameterized_logs[1],
        "parameterized self/common traces diverged"
    );
    let common_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(common_out.join("dut_access_matrix__artifacts.json"))
            .expect("read default common manifest"),
    )
    .expect("parse default common manifest");
    let parameterized_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            parameterized_common_out.join("dut_access_matrix__artifacts.json"),
        )
        .expect("read parameterized common manifest"),
    )
    .expect("parse parameterized common manifest");
    assert_ne!(
        common_manifest["build_profile"], parameterized_manifest["build_profile"],
        "a changed --param value must invalidate the common build profile"
    );

    let common_capsule =
        std::fs::read_to_string(common_out.join("dut_access_matrix__test_DutAccessMatrix.cpp"))
            .expect("read DUT-access capsule");
    let common_runtime = std::fs::read_to_string(common_out.join("dut_access_matrix__runtime.cpp"))
        .expect("read DUT-access runtime");
    assert!(common_runtime.contains("self.dut->send_rsp_data"));
    assert!(common_runtime.contains("model->send_rsp_data"));
    assert!(common_runtime.contains(
        "DutAccessMatrixTb_sample_aggregate(ctx, _tb, _harc_tb_component_reader, model)"
    ));
    assert!(common_capsule.contains("harc_vec_lane_write<8>"));
    assert!(common_capsule.contains("harc_assign") && common_capsule.contains("forced_drv"));
    assert!(common_capsule.contains("harc_wide_sext"));
    assert!(common_runtime.contains("harc_wide_sext<7>"));
    assert!(common_capsule.contains("___024root.h"));

    let rebuild_log =
        build_common_with_param(&sv, &tb, "TbirDutAccessMatrix", "UNUSED=2", &common_out);
    assert!(
        !rebuild_log.contains(", 0 rewritten,"),
        "a changed --param value must republish common artifacts:\n{rebuild_log}"
    );
    let rebuilt_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(common_out.join("dut_access_matrix__artifacts.json"))
            .expect("read rebuilt common manifest"),
    )
    .expect("parse rebuilt common manifest");
    assert_eq!(
        rebuilt_manifest["build_profile"], parameterized_manifest["build_profile"],
        "the rebuilt manifest must carry the parameterized plan profile"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(parameterized_out);
    let _ = std::fs::remove_dir_all(parameterized_common_out);
}

#[test]
fn tbir_common_probe_stub_is_incremental_and_removed_with_its_manifest_owner() {
    let inputs = fresh_dir("probe_incremental_inputs");
    let outdir = fresh_dir("probe_incremental_output");
    let sv = inputs.join("TbirProbeCollision.sv");
    let tb = inputs.join("probe_incremental.harc");
    let sv_source = PROBE_COLLISION_DUT.replace(
        "logic [7:0] internal_status;",
        "logic [7:0] internal_status;\n  logic [7:0] alternate_status;",
    );
    std::fs::write(&sv, sv_source).expect("write incremental probe DUT");

    let emit = |source: &str| {
        std::fs::write(&tb, source).expect("write incremental HARC source");
        let output = Command::new(harc_bin())
            .arg("sim")
            .arg("--sv")
            .arg(&sv)
            .arg(&tb)
            .args(["--top", "TbirProbeCollision", "--codegen", "tbir"])
            .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
            .arg("--emit-only")
            .arg("--outdir")
            .arg(&outdir)
            .output()
            .expect("emit incremental common suite");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "incremental emit failed:\n{log}");
    };

    emit(PROBE_COLLISION_TESTBENCH);
    let stub_path = outdir.join("probe_incremental__probe_stub.sv");
    let first_stub = std::fs::read_to_string(&stub_path).expect("read first probe stub");
    let first_manifest = std::fs::read_to_string(outdir.join("probe_incremental__artifacts.json"))
        .expect("read first manifest");
    let first_manifest: serde_json::Value =
        serde_json::from_str(&first_manifest).expect("parse first manifest");
    let stable_paths = [
        "probe_incremental__suite_api.hpp",
        "probe_incremental__runtime.cpp",
        "probe_incremental__test_ProbeCollisionA.cpp",
        "probe_incremental__test_ProbeCollisionB.cpp",
        "probe_incremental__probe_stub.sv",
    ];
    let stable_bytes = stable_paths
        .iter()
        .map(|path| {
            std::fs::read(outdir.join(path))
                .unwrap_or_else(|error| panic!("read stable artifact {path}: {error}"))
        })
        .collect::<Vec<_>>();

    let with_unrelated = format!(
        "{PROBE_COLLISION_TESTBENCH}\nimpl ProbeCollisionC for ProbeCollisionTb\n    clock clk = 10ns\n    run\n        wait 1 cycle\n    end run\nend impl ProbeCollisionC\n"
    );
    emit(&with_unrelated);
    for (path, expected) in stable_paths.iter().zip(&stable_bytes) {
        assert_eq!(
            std::fs::read(outdir.join(path)).unwrap_or_else(|error| panic!(
                "read artifact {path} after adding a test: {error}"
            )),
            *expected,
            "adding an unrelated test rewrote existing artifact {path}"
        );
    }

    let (prefix, implementations) = PROBE_COLLISION_TESTBENCH
        .split_once("\nimpl ProbeCollisionA")
        .expect("fixture has A implementation");
    let (a_body, b_body) = implementations
        .split_once("\nimpl ProbeCollisionB")
        .expect("fixture has B implementation");
    let permuted = format!("{prefix}\nimpl ProbeCollisionB{b_body}\nimpl ProbeCollisionA{a_body}");
    emit(&permuted);
    for (path, expected) in stable_paths.iter().zip(&stable_bytes) {
        assert_eq!(
            std::fs::read(outdir.join(path)).unwrap_or_else(|error| {
                panic!("read artifact {path} after test permutation: {error}")
            }),
            *expected,
            "test declaration order rewrote stable artifact {path}"
        );
    }

    let alternate = PROBE_COLLISION_TESTBENCH.replace("at internal_status", "at alternate_status");
    emit(&alternate);
    let second_stub = std::fs::read_to_string(&stub_path).expect("read second probe stub");
    let second_manifest = std::fs::read_to_string(outdir.join("probe_incremental__artifacts.json"))
        .expect("read second manifest");
    let second_manifest: serde_json::Value =
        serde_json::from_str(&second_manifest).expect("parse second manifest");
    assert_ne!(
        first_stub, second_stub,
        "probe path edit did not rebuild the stub"
    );
    assert!(second_stub.contains("TbirProbeCollision.alternate_status"));
    assert_ne!(
        first_manifest["interface_abi"], second_manifest["interface_abi"],
        "probe path is part of the common interface ABI"
    );
    assert_ne!(
        first_manifest["build_profile"], second_manifest["build_profile"],
        "probe path is part of the build profile"
    );

    emit(PROBELESS_COLLISION_TESTBENCH);
    assert!(
        !stub_path.exists(),
        "the manifest transaction did not remove its stale probe stub"
    );
    let final_manifest = std::fs::read_to_string(outdir.join("probe_incremental__artifacts.json"))
        .expect("read probe-less manifest");
    assert!(
        !final_manifest.contains("probe_stub.sv"),
        "probe-less manifest still owns the stub: {final_manifest}"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(outdir);
}

#[test]
fn tbir_common_probe_contract_conflicts_fail_before_publication() {
    let inputs = fresh_dir("probe_conflict_inputs");
    let sv = inputs.join("TbirProbeCollision.sv");
    let tb = inputs.join("probe_conflict.harc");
    std::fs::write(&sv, PROBE_COLLISION_DUT).expect("write probe-conflict DUT");

    for (label, second_probe) in [
        ("width", "probe force status : uint<7> at internal_status"),
        ("access", "probe status : uint<8> at internal_status"),
        ("path", "probe force status : uint<8> at other_status"),
    ] {
        let outdir = fresh_dir(&format!("probe_conflict_{label}"));
        let source = format!(
            r#"test ProbeA
    let dut : TbirProbeCollision
        probe force status : uint<8> at internal_status
    end let dut
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test ProbeA

test ProbeB
    let dut : TbirProbeCollision
        {second_probe}
    end let dut
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test ProbeB"#
        );
        std::fs::write(&tb, source).expect("write conflicting probe source");
        let output = Command::new(harc_bin())
            .arg("sim")
            .arg("--sv")
            .arg(&sv)
            .arg(&tb)
            .args(["--top", "TbirProbeCollision", "--codegen", "tbir"])
            .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
            .arg("--emit-only")
            .arg("--outdir")
            .arg(&outdir)
            .output()
            .expect("run conflicting probe preflight");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "{label} conflict unexpectedly passed"
        );
        assert!(log.contains("conflicting declarations"), "{label}: {log}");
        assert!(
            log.contains("ProbeA") && log.contains("ProbeB"),
            "{label}: {log}"
        );
        assert!(
            !outdir.join("probe_conflict__artifacts.json").exists()
                && !outdir.join("probe_conflict__probe_stub.sv").exists(),
            "{label} conflict published common artifacts"
        );
        let _ = std::fs::remove_dir_all(outdir);
    }

    let outdir = fresh_dir("partial_shared_probe");
    let partial = r#"agent SharedProbeReader
    function sample() -> uint<8>
        return dut.status
    end function sample
end agent SharedProbeReader

test ProbeA
    let dut : TbirProbeCollision
        probe status : uint<8> at internal_status
    end let dut
    let reader : SharedProbeReader
    clock clk = 10ns
    run
        let value : uint<8> = reader.sample()
    end run
end test ProbeA

test ProbeB
    let dut : TbirProbeCollision
    let reader : SharedProbeReader
    clock clk = 10ns
    run
        wait 1 cycle
    end run
end test ProbeB"#;
    std::fs::write(&tb, partial).expect("write partial shared-probe source");
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(&sv)
        .arg(&tb)
        .args(["--top", "TbirProbeCollision", "--codegen", "tbir"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .arg("--emit-only")
        .arg("--outdir")
        .arg(&outdir)
        .output()
        .expect("run partial shared-probe preflight");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "partial shared probe unexpectedly passed"
    );
    assert!(
        log.contains("not declared") && log.contains("identically by every test"),
        "{log}"
    );
    assert!(
        !outdir.join("probe_conflict__artifacts.json").exists()
            && !outdir.join("probe_conflict__probe_stub.sv").exists(),
        "partial shared probe published common artifacts"
    );
    let _ = std::fs::remove_dir_all(outdir);

    let _ = std::fs::remove_dir_all(inputs);
}

#[test]
fn tbir_common_statement_runtime_cells_reset_across_same_process_runs() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_statement_runtime_cells_reset_across_same_process_runs: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("runtime_cells_inputs");
    let outdir = fresh_dir("runtime_cells_output");
    let self_out = fresh_dir("runtime_cells_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("runtime_cells.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, RUNTIME_CELL_TESTBENCH).expect("write runtime-cell HARC fixture");
    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &outdir);

    let registry = outdir.join("runtime_cells__registry.cpp");
    std::fs::write(&registry, RUNTIME_CELL_REGISTRY).expect("install runtime-cell registry");
    let registry_object = outdir.join("obj_dir/runtime_cells__registry.o");
    if registry_object.exists() {
        std::fs::remove_file(&registry_object).expect("remove original registry object");
    }
    let link = relink(&outdir.join("obj_dir"), "TbirCommonReg");
    let link_log = format!(
        "{}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );
    assert!(
        link.status.success(),
        "runtime-cell harness failed to link:\n{link_log}"
    );

    let run = Command::new(outdir.join("obj_dir/VTbirCommonReg"))
        .arg(&outdir)
        .current_dir(&outdir)
        .output()
        .expect("run runtime-cell sequence");
    let run_log = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.status.success(),
        "runtime-cell sequence failed:\n{run_log}"
    );

    let fatal_log = std::fs::read_to_string(outdir.join("fatal.log")).expect("read fatal log");
    assert!(
        fatal_log.contains("EXPECTED_RUNTIME_CELL_FATAL"),
        "{fatal_log}"
    );
    assert!(!fatal_log.contains("UNREACHABLE_FATAL_"), "{fatal_log}");

    for (tag, markers) in [
        (
            "a_first",
            [
                "A_LOCAL=5",
                "TB_PERIODIC",
                "A_PERIODIC",
                "A_DONE",
                "TB_EDGE",
                "A_EDGE",
            ],
        ),
        (
            "b",
            [
                "B_LOCAL=6",
                "TB_PERIODIC",
                "B_PERIODIC",
                "B_DONE",
                "TB_EDGE",
                "B_EDGE",
            ],
        ),
        (
            "a_second",
            [
                "A_LOCAL=5",
                "TB_PERIODIC",
                "A_PERIODIC",
                "A_DONE",
                "TB_EDGE",
                "A_EDGE",
            ],
        ),
    ] {
        let log = std::fs::read_to_string(outdir.join(format!("{tag}.log")))
            .unwrap_or_else(|error| panic!("read {tag} log: {error}"));
        assert!(!log.contains("FAIL"), "{tag} leaked temporal state:\n{log}");
        let mut prior = 0;
        for marker in markers {
            let at = log
                .find(marker)
                .unwrap_or_else(|| panic!("{tag} log lacks `{marker}`:\n{log}"));
            assert!(at >= prior, "{tag} lifecycle order changed:\n{log}");
            prior = at;
        }
    }

    let first = std::fs::read(outdir.join("a_first.jsonl")).expect("read first A trace");
    let second = std::fs::read(outdir.join("a_second.jsonl")).expect("read second A trace");
    assert_eq!(first, second, "A must be deterministic after intervening B");

    for (test, tag) in [("RuntimeCellA", "a_first"), ("RuntimeCellB", "b")] {
        let self_trace_path = self_out.join(format!("{test}.jsonl"));
        let self_run = Command::new(self_out.join("obj_dir/VTbirCommonReg"))
            .args(["--test", test])
            .current_dir(&self_out)
            .env("HARC_SEED", "606")
            .env("HARC_TRACE", &self_trace_path)
            .env("HARC_SIM_LOG", self_out.join(format!("{test}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run self-contained {test}: {error}"));
        let self_log = format!(
            "{}{}",
            String::from_utf8_lossy(&self_run.stdout),
            String::from_utf8_lossy(&self_run.stderr)
        );
        assert!(
            self_run.status.success(),
            "self-contained {test} failed:\n{self_log}"
        );
        assert_eq!(
            std::fs::read(&self_trace_path).expect("read self-contained trace"),
            std::fs::read(outdir.join(format!("{tag}.jsonl"))).expect("read common trace"),
            "runtime-cell trace diverged for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(outdir);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_component_lifecycle_cells_are_instance_owned_and_match_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_component_lifecycle_cells_are_instance_owned_and_match_self_contained: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("component_lifecycle_inputs");
    let common_out = fresh_dir("component_lifecycle_common");
    let self_out = fresh_dir("component_lifecycle_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("component_lifecycle.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COMPONENT_LIFECYCLE_TESTBENCH).expect("write component lifecycle fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let interface = std::fs::read_to_string(common_out.join("component_lifecycle__suite_api.hpp"))
        .expect("read component lifecycle interface");
    let capsule = std::fs::read_to_string(
        common_out.join("component_lifecycle__test_ComponentLifecycle.cpp"),
    )
    .expect("read component lifecycle capsule");
    let self_cpp = std::fs::read_to_string(self_out.join("component_lifecycle.cpp"))
        .expect("read self-contained component lifecycle source");
    for artifact in [&interface, &self_cpp] {
        for field in [
            "component_periodic",
            "component_cycle",
            "component_watchdog",
        ] {
            assert!(artifact.contains(field), "missing `{field}`:\n{artifact}");
        }
        assert!(
            !artifact.contains("static int64_t _harc_"),
            "component lifecycle state must be receiver-owned:\n{artifact}"
        );
        assert!(
            !artifact.contains("static bool _harc_"),
            "component lifecycle state must be receiver-owned:\n{artifact}"
        );
    }
    assert!(
        !capsule.contains(".push_back([&]"),
        "common component lifecycle callbacks retained coroutine-local captures:\n{capsule}"
    );

    let run_layout = |outdir: &Path, log_name: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .current_dir(outdir)
            .env("HARC_SEED", "606")
            .env("HARC_SIM_LOG", outdir.join(log_name))
            .output()
            .expect("run component lifecycle binary")
    };
    let self_run = run_layout(&self_out, "self.log");
    let common_run = run_layout(&common_out, "common.log");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} lifecycle run failed:\n{output}"
        );
    }
    let self_log = std::fs::read_to_string(self_out.join("self.log")).expect("read self log");
    let common_log =
        std::fs::read_to_string(common_out.join("common.log")).expect("read common log");
    assert_eq!(self_log, common_log, "component lifecycle traces diverged");
    for marker in [
        "CELL_PERIODIC=1:1",
        "CELL_PERIODIC=2:1",
        "CELL_EDGE=1:1",
        "CELL_WATCHDOG=1:1",
        "CELL_WATCHDOG=2:1",
        "CELL_PERIODIC=1:2",
        "CELL_PERIODIC=2:2",
        "CELL_EDGE=2:1",
        "CELL_WATCHDOG=1:4",
        "CELL_WATCHDOG=2:4",
        "CELL_PERIODIC=1:4",
        "CELL_PERIODIC=2:4",
        "COMPONENT_LIFECYCLE_DONE",
    ] {
        assert!(
            common_log.contains(marker),
            "missing `{marker}`:\n{common_log}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_component_hooks_are_receiver_owned_and_preserve_captures() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_component_hooks_are_receiver_owned_and_preserve_captures: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("component_hooks_inputs");
    let common_out = fresh_dir("component_hooks_common");
    let self_out = fresh_dir("component_hooks_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("component_hooks.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COMPONENT_HOOK_ISOLATION_TESTBENCH).expect("write component hook fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let registry = common_out.join("component_hooks__registry.cpp");
    std::fs::write(&registry, COMPONENT_HOOK_REGISTRY).expect("install component-hook registry");
    let registry_object = common_out.join("obj_dir/component_hooks__registry.o");
    if registry_object.exists() {
        std::fs::remove_file(&registry_object).expect("remove original hook registry object");
    }
    let link = relink(&common_out.join("obj_dir"), "TbirCommonReg");
    let link_log = format!(
        "{}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );
    assert!(
        link.status.success(),
        "component-hook harness failed to link:\n{link_log}"
    );

    let run_layout = |outdir: &Path, test: Option<&str>, log_name: &str| {
        let mut command = Command::new(outdir.join("obj_dir/VTbirCommonReg"));
        if let Some(test) = test {
            command.args(["--test", test]);
        } else {
            command.arg(outdir);
        }
        command
            .current_dir(outdir)
            .env("HARC_SEED", "606")
            .env("HARC_SIM_LOG", outdir.join(log_name))
            .output()
            .expect("run component hook binary")
    };
    let self_run = run_layout(&self_out, Some("HookIsolation"), "self.log");
    let common_run = run_layout(&common_out, None, "unused.log");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(run.status.success(), "{layout} hook run failed:\n{output}");
    }
    let self_log = std::fs::read_to_string(self_out.join("self.log")).expect("read self log");
    let common_log =
        std::fs::read_to_string(common_out.join("hook_first.log")).expect("read first common log");
    let repeated_log = std::fs::read_to_string(common_out.join("hook_second.log"))
        .expect("read repeated common log");
    let no_subscription_log = std::fs::read_to_string(common_out.join("hook_none.log"))
        .expect("read no-subscription common log");
    let fatal_log =
        std::fs::read_to_string(common_out.join("hook_fatal.log")).expect("read fatal hook log");
    assert_eq!(self_log, common_log, "component hook traces diverged");
    assert_eq!(
        common_log, repeated_log,
        "component hooks leaked across runs"
    );
    assert!(
        no_subscription_log.contains("HOOK_NO_SUBSCRIPTION_DONE")
            && !no_subscription_log.contains("LEFT_PRE=")
            && !no_subscription_log.contains("LEFT_POST=")
            && !no_subscription_log.contains("LEFT_TOUCH="),
        "hook registration leaked into unsubscribed run:\n{no_subscription_log}"
    );
    assert!(fatal_log.contains("EXPECTED_HOOK_FATAL"), "{fatal_log}");
    assert!(!fatal_log.contains("UNREACHABLE_FATAL_HOOK"), "{fatal_log}");
    assert_eq!(common_log.matches("LEFT_PRE=5").count(), 1, "{common_log}");
    assert_eq!(
        common_log.matches("LEFT_POST=15").count(),
        1,
        "{common_log}"
    );
    assert_eq!(
        common_log.matches("LEFT_PRE=119").count(),
        1,
        "{common_log}"
    );
    assert_eq!(
        common_log.matches("LEFT_POST=129").count(),
        1,
        "{common_log}"
    );
    assert_eq!(
        common_log.matches("LEFT_TOUCH=115").count(),
        1,
        "{common_log}"
    );
    assert!(common_log.contains("HOOK_ISOLATION_DONE"), "{common_log}");
    assert!(common_log.contains("HOOK_CHECK_DONE"), "{common_log}");

    let capsule =
        std::fs::read_to_string(common_out.join("component_hooks__test_HookIsolation.cpp"))
            .expect("read common hook capsule");
    assert!(capsule.contains("left._u__u__harc_hook_bump_pre.push_back"));
    assert!(capsule.contains("left._u__harc_hook_bump_post.push_back"));
    assert!(capsule.contains("left._harc_hook_touch_pre.push_back"));
    assert!(!capsule.contains("right._u__u__harc_hook_bump_pre.push_back"));

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_persistent_callbacks_outlive_run_scope_and_final_completion_tick() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_persistent_callbacks_outlive_run_scope_and_final_completion_tick: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("persistent_capture_inputs");
    let common_out = fresh_dir("persistent_capture_common");
    let self_out = fresh_dir("persistent_capture_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("persistent_capture.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, PERSISTENT_CAPTURE_TESTBENCH).expect("write persistent-capture fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let capsule =
        std::fs::read_to_string(common_out.join("persistent_capture__test_PersistentCapture.cpp"))
            .expect("read persistent-capture capsule");
    assert!(capsule.contains("struct HarcRunState_PersistentCapture"));
    assert!(capsule.contains("callback_capture_run"));
    assert!(
        capsule.contains("int64_t callback_capture_run_n16_captured_signed8{};")
            && capsule.contains("_harc_u128 callback_capture_run_n17_captured_signed65{};"),
        "persistent inferred DUT values need exact signed storage:\n{capsule}"
    );
    assert!(
        capsule.contains("std::function<void(int64_t&, _harc_u128&)>")
            && capsule.contains("int64_t& captured_signed8")
            && capsule.contains("_harc_u128& captured_signed65"),
        "method-hook captures need the inferred DUT types in storage and handler signatures:\n{capsule}"
    );
    assert!(
        capsule
            .contains("[_harc_callback_state = &_harc_run_state, _harc_callback_context = &ctx]"),
        "persistent callbacks must capture only durable run-owned pointers:\n{capsule}"
    );
    assert!(
        !capsule.contains("test_hook_cycle_run_hs0 = [&]"),
        "a common persistent callback must not capture coroutine locals by reference:\n{capsule}"
    );
    assert!(
        !capsule.contains(".push_back([&]"),
        "a common callback registration must capture only durable state/context:\n{capsule}"
    );
    assert!(capsule.contains("test.destroy_state") == false);

    let run_layout = |outdir: &Path, tag: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .args(["--test", "PersistentCapture"])
            .current_dir(outdir)
            .env("HARC_SEED", "606")
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .output()
            .expect("run persistent-capture binary")
    };
    let self_run = run_layout(&self_out, "self");
    let common_run = run_layout(&common_out, "common");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} capture run failed:\n{output}"
        );
    }
    let self_log = std::fs::read_to_string(self_out.join("self.log")).expect("read self log");
    let common_log =
        std::fs::read_to_string(common_out.join("common.log")).expect("read common log");
    assert_eq!(self_log, common_log, "persistent-capture traces diverged");
    assert!(
        common_log.contains("PERSISTENT_CAPTURE_DONE=2")
            && common_log.contains("PERSISTENT_SIGNED")
            && common_log.contains("METHOD_HOOK_SIGNED")
            && common_log.matches("PERSISTENT_TICK=4:").count() == 3
            && common_log.contains("PERSISTENT_TICK=4:3")
            && !common_log.contains("FAIL"),
        "{common_log}"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_hook_bodies_use_complete_typed_runtime_bindings() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_hook_bodies_use_complete_typed_runtime_bindings: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("hook_bindings_inputs");
    let common_out = fresh_dir("hook_bindings_common");
    let self_out = fresh_dir("hook_bindings_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("hook_bindings.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, HOOK_BINDINGS_TESTBENCH).expect("write hook-binding fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let runtime = std::fs::read_to_string(common_out.join("hook_bindings__runtime.cpp"))
        .expect("read hook-binding runtime");
    let capsule = std::fs::read_to_string(common_out.join("hook_bindings__test_HookBindings.cpp"))
        .expect("read hook-binding capsule");
    assert!(runtime.contains("void harc_tseq_tick(HarcTestContext& ctx)"));
    assert!(capsule.contains("harc_tseq_HookTimed(ctx,"), "{capsule}");
    assert!(capsule.contains("harc_tseq_tick(ctx)"), "{capsule}");
    assert!(capsule.contains("HookBindingsCell_add(ctx,"), "{capsule}");
    assert!(capsule.contains("HookBindingsTb_bump(ctx,"), "{capsule}");
    assert!(
        !capsule.contains(".push_back([&]"),
        "common hook/service registration retained a coroutine-local blanket capture:\n{capsule}"
    );

    let run_layout = |outdir: &Path, tag: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .args(["--test", "HookBindings"])
            .current_dir(outdir)
            .env("HARC_SEED", "606")
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .output()
            .expect("run hook-binding binary")
    };
    let self_run = run_layout(&self_out, "self");
    let common_run = run_layout(&common_out, "common");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} hook-binding run failed:\n{output}"
        );
    }
    let self_log = std::fs::read_to_string(self_out.join("self.log")).expect("read self log");
    let common_log =
        std::fs::read_to_string(common_out.join("common.log")).expect("read common log");
    assert_eq!(self_log, common_log, "hook-binding traces diverged");
    assert!(common_log.contains("EVENT_HOOK=3"), "{common_log}");
    assert!(common_log.contains("METHOD_HOOK=2"), "{common_log}");
    assert!(common_log.contains("HOOK_BINDINGS_DONE"), "{common_log}");

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_exact_instance_method_hook_fixture_runs() {
    if !verilator_present() {
        eprintln!("SKIP tbir_exact_instance_method_hook_fixture_runs: `verilator` not found");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let outdir = fresh_dir("method_hook_exact_instance");
    build_self_contained(
        &root.join("tests/dut/top_counter.sv"),
        &root.join("tests/fixtures/method_hook_exact_instance_test.harc"),
        "Top",
        &outdir,
    );
    let run = Command::new(outdir.join("obj_dir/VTop"))
        .args(["--test", "MethodHookFamilyTest"])
        .current_dir(&outdir)
        .env("HARC_SEED", "606")
        .env("HARC_SIM_LOG", outdir.join("method_hook.log"))
        .output()
        .expect("run exact-instance method-hook fixture");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.status.success(),
        "method-hook fixture failed:\n{output}"
    );
    let log =
        std::fs::read_to_string(outdir.join("method_hook.log")).expect("read method-hook log");
    assert!(
        log.contains("PASS: nested and statement-position method hooks") && !log.contains("FAIL"),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(outdir);
}

#[test]
fn tbir_transactor_method_hooks_are_exact_instance_owned() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_transactor_method_hooks_are_exact_instance_owned: `verilator` not found"
        );
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let outdir = fresh_dir("transactor_hook_exact_instance");
    build_self_contained(
        &root.join("tests/dut/top_counter.sv"),
        &root.join("tests/fixtures/transactor_hook_exact_instance_test.harc"),
        "Top",
        &outdir,
    );
    let cpp = std::fs::read_to_string(outdir.join("transactor_hook_exact_instance_test.cpp"))
        .expect("read transactor-hook source");
    for declaration in [
        "uint64_t _last_in_cycle = 10;",
        "uint64_t _last_out_cycle = 20;",
        "uint64_t _u__last_in_cycle = 0;",
        "uint64_t _u__last_out_cycle = 0;",
    ] {
        assert!(
            cpp.contains(declaration),
            "missing collision-proof transactor heartbeat `{declaration}`:\n{cpp}"
        );
    }
    assert!(
        cpp.contains("left._u__last_in_cycle") && cpp.contains("left._u__last_out_cycle"),
        "transactor idle predicate must read collision-proof heartbeat fields:\n{cpp}"
    );
    let run = Command::new(outdir.join("obj_dir/VTop"))
        .args(["--test", "TransactorHookExactInstanceTest"])
        .current_dir(&outdir)
        .env("HARC_SEED", "606")
        .env("HARC_SIM_LOG", outdir.join("transactor_hook.log"))
        .output()
        .expect("run exact-instance transactor-hook fixture");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.status.success(),
        "transactor-hook fixture failed:\n{output}"
    );
    let log = std::fs::read_to_string(outdir.join("transactor_hook.log"))
        .expect("read transactor-hook log");
    assert!(
        log.contains("PASS: transactor hooks are exact-instance owned") && !log.contains("FAIL"),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(outdir);
}

#[test]
fn tbir_runtime_cell_symbols_are_collision_proof_in_both_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_runtime_cell_symbols_are_collision_proof_in_both_layouts: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("runtime_cell_name_inputs");
    let common_out = fresh_dir("runtime_cell_name_common");
    let self_out = fresh_dir("runtime_cell_name_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("runtime_cell_names.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, RUNTIME_CELL_NAME_COLLISION_TESTBENCH)
        .expect("write runtime-cell name fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let self_cpp = std::fs::read_to_string(self_out.join("runtime_cell_names.cpp"))
        .expect("read self-contained source");
    let capsule =
        std::fs::read_to_string(common_out.join("runtime_cell_names__test_RuntimeCellNames.cpp"))
            .expect("read common capsule");
    assert!(
        self_cpp.contains("HarcRuntimeCells_t0 _u__harc_runtime_cells{}"),
        "{self_cpp}"
    );
    assert!(
        capsule.contains("struct HarcRuntimeCells_RuntimeCellNames_1"),
        "{capsule}"
    );
    assert!(
        capsule.contains("struct HarcRunState_RuntimeCellNames_1"),
        "{capsule}"
    );
    assert!(capsule.contains("_u__harc_runtime_cells"), "{capsule}");
    for declaration in [
        "uint64_t _last_in_cycle = 11;",
        "uint64_t _last_out_cycle = 13;",
        "uint64_t _u__last_in_cycle = 0;",
        "uint64_t _u__last_out_cycle = 0;",
    ] {
        assert!(
            self_cpp.contains(declaration),
            "missing collision-proof heartbeat declaration `{declaration}`:\n{self_cpp}"
        );
    }

    let run_layout = |outdir: &Path, tag: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .args(["--test", "RuntimeCellNames"])
            .current_dir(outdir)
            .env("HARC_SEED", "606")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .output()
            .expect("run runtime-cell name fixture")
    };
    let self_run = run_layout(&self_out, "self");
    let common_run = run_layout(&common_out, "common");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} collision run failed:\n{output}"
        );
    }
    let self_log = std::fs::read_to_string(self_out.join("self.log")).expect("read self log");
    let common_log =
        std::fs::read_to_string(common_out.join("common.log")).expect("read common log");
    assert_eq!(
        self_log, common_log,
        "runtime-cell collision traces diverged"
    );
    assert!(
        common_log.contains("RUNTIME_CELL_NAMES_DONE"),
        "{common_log}"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_event_connects_are_instance_owned_ordered_and_not_duplicated() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_event_connects_are_instance_owned_ordered_and_not_duplicated: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("event_connect_inputs");
    let common_out = fresh_dir("event_connect_common");
    let self_out = fresh_dir("event_connect_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("event_connect.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COMPONENT_EVENT_CONNECT_TESTBENCH).expect("write event-connect fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);
    let capsule = std::fs::read_to_string(common_out.join("event_connect__test_EventConnect.cpp"))
        .expect("read event-connect capsule");
    assert!(
        !capsule.contains(".push_back([&]"),
        "common connect registration retained coroutine-local captures:\n{capsule}"
    );

    let run_layout = |outdir: &Path, log_name: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .current_dir(outdir)
            .env("HARC_SEED", "606")
            .env("HARC_SIM_LOG", outdir.join(log_name))
            .output()
            .expect("run event-connect binary")
    };
    let self_run = run_layout(&self_out, "self.log");
    let common_run = run_layout(&common_out, "common.log");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} event-connect run failed:\n{output}"
        );
    }
    let self_log = std::fs::read_to_string(self_out.join("self.log")).expect("read self log");
    let common_log =
        std::fs::read_to_string(common_out.join("common.log")).expect("read common log");
    assert_eq!(self_log, common_log, "event-connect traces diverged");
    for marker in [
        "EVENT_ACCEPT=3:3",
        "EVENT_RELAY=3:3",
        "EVENT_ACCEPT=4:7",
        "EVENT_RELAY=4:7",
        "EVENT_ACCEPT=5:12",
        "EVENT_RELAY=5:12",
        "EVENT_ACCEPT=9:9",
        "EVENT_RELAY=9:9",
    ] {
        assert_eq!(common_log.matches(marker).count(), 1, "{common_log}");
    }
    assert!(common_log.contains("EVENT_CONNECT_DONE"), "{common_log}");

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_and_self_contained_share_lifecycle_ordering() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_and_self_contained_share_lifecycle_ordering: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("lifecycle_inputs");
    let common_out = fresh_dir("lifecycle_common");
    let self_out = fresh_dir("lifecycle_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("phased.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, PHASED_TESTBENCH).expect("write lifecycle HARC fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let run_layout = |outdir: &Path, trace_name: &str, log_name: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .current_dir(outdir)
            .env("HARC_SEED", "9917")
            .env("HARC_DUT_BACKEND", "verilator")
            .env("HARC_TRACE", outdir.join(trace_name))
            .env("HARC_SIM_LOG", outdir.join(log_name))
            .output()
            .expect("run generated lifecycle binary")
    };
    let self_run = run_layout(&self_out, "self.jsonl", "self.log");
    let common_run = run_layout(&common_out, "common.jsonl", "common.log");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} lifecycle run failed:\n{output}"
        );
        let mut prior = 0;
        for marker in [
            "PHASE: setup",
            "PHASE: run",
            "PHASE: check",
            "PHASE: teardown",
        ] {
            let at = output
                .find(marker)
                .unwrap_or_else(|| panic!("{layout} output lacks `{marker}`:\n{output}"));
            assert!(
                at >= prior,
                "{layout} emitted `{marker}` out of order:\n{output}"
            );
            prior = at;
        }
    }

    let self_trace = std::fs::read(self_out.join("self.jsonl")).expect("read self trace");
    let common_trace = std::fs::read(common_out.join("common.jsonl")).expect("read common trace");
    assert_eq!(
        self_trace, common_trace,
        "common and self-contained lifecycle traces diverged"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_component_methods_compile_once_and_match_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_component_methods_compile_once_and_match_self_contained: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("component_methods_inputs");
    let common_out = fresh_dir("component_methods_common");
    let self_out = fresh_dir("component_methods_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("component_methods.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COMMON_COMPONENT_METHODS_TESTBENCH).expect("write HARC fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let runtime = std::fs::read_to_string(common_out.join("component_methods__runtime.cpp"))
        .expect("read common runtime");
    assert_eq!(
        runtime
            .matches("uint64_t Counter_bump(HarcTestContext& ctx")
            .count(),
        1
    );
    assert_eq!(
        runtime
            .matches("uint64_t CounterPair_sum_after(HarcTestContext& ctx")
            .count(),
        1
    );
    for capsule in [
        common_out.join("component_methods__test_CounterA.cpp"),
        common_out.join("component_methods__test_CounterB.cpp"),
    ] {
        let source = std::fs::read_to_string(capsule).expect("read component capsule");
        assert!(!source.contains("auto Counter_bump ="));
        assert!(!source.contains("auto CounterPair_sum_after ="));
    }

    for test in ["CounterA", "CounterB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VTbirCommonReg"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5150")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let self_run = run(&self_out, &format!("self_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("ALL TESTS PASSED"), "{layout} {test}:\n{log}");
        }
        assert_eq!(
            std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_testbench_methods_compile_once_and_match_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_testbench_methods_compile_once_and_match_self_contained: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("testbench_methods_inputs");
    let common_out = fresh_dir("testbench_methods_common");
    let self_out = fresh_dir("testbench_methods_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("testbench_methods.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COMMON_TESTBENCH_METHODS_TESTBENCH).expect("write HARC fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let runtime = std::fs::read_to_string(common_out.join("testbench_methods__runtime.cpp"))
        .expect("read common runtime");
    for signature in [
        "uint64_t MethodTb_later(",
        "uint64_t MethodTb_ordered(",
        "Beat MethodTb_mirror(",
        "uint64_t MethodTb_save(",
        "uint64_t MethodTb_lazy_take(",
        "uint64_t MethodTb_lazy_skip_or(",
        "uint64_t MethodTb_lazy_choose(",
    ] {
        assert_eq!(
            runtime.matches(signature).count(),
            1,
            "{signature} definition"
        );
    }
    for capsule in [
        common_out.join("testbench_methods__test_MethodA.cpp"),
        common_out.join("testbench_methods__test_MethodB.cpp"),
    ] {
        let source = std::fs::read_to_string(capsule).expect("read method capsule");
        assert!(!source.contains("auto MethodTb_"));
    }

    for test in ["MethodA", "MethodB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VTbirCommonReg"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5150")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let self_run = run(&self_out, &format!("self_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("ALL TESTS PASSED"), "{layout} {test}:\n{log}");
        }
        assert_eq!(
            std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_method_timing_and_tseq_calls_match_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_method_timing_and_tseq_calls_match_self_contained: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("method_timing_inputs");
    let common_out = fresh_dir("method_timing_common");
    let self_out = fresh_dir("method_timing_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("method_timing.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COMMON_METHOD_TIMING_TESTBENCH).expect("write HARC fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let runtime = std::fs::read_to_string(common_out.join("method_timing__runtime.cpp"))
        .expect("read common runtime");
    assert!(runtime.contains("harc_eval_clocks_until(ctx, ctx.now_ps + 1);"));
    assert!(runtime.contains("harc_tseq_tick(ctx);"));
    assert!(runtime.contains("harc_tseq_PureValues("));
    assert!(runtime.contains("harc_tseq_TimedValues(ctx,"));
    assert!(
        runtime.contains("harc_tseq_TimedValues(HarcTestContext& ctx, uint64_t _u_ctx)"),
        "TSeq parameter collided with the generated context:\n{runtime}"
    );

    let run = |outdir: &Path, tag: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .args(["--test", "CommonMethodTiming"])
            .current_dir(outdir)
            .env("HARC_SEED", "5150")
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run CommonMethodTiming in {tag}: {error}"))
    };
    let self_run = run(&self_out, "self");
    let common_run = run(&common_out, "common");
    for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.status.success(), "{layout} run failed:\n{log}");
        assert!(log.contains("METHOD_TIMING_RESULT=4"), "{layout}:\n{log}");
        assert!(log.contains("ALL TESTS PASSED"), "{layout}:\n{log}");
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "method timing trace mismatch"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_hooks_use_typed_testbench_record_state_in_both_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_hooks_use_typed_testbench_record_state_in_both_layouts: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("hook_record_inputs");
    let self_out = fresh_dir("hook_record_self");
    let common_out = fresh_dir("hook_record_common");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("hook_record.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, TESTBENCH_HOOK_RECORD_STATE_TESTBENCH).expect("write HARC fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    let cpp = std::fs::read_to_string(self_out.join("hook_record.cpp"))
        .expect("read self-contained source");
    assert!(
        cpp.contains("_tb.saved.value"),
        "typed record receiver absent:\n{cpp}"
    );
    assert!(
        !cpp.lines().any(|line| {
            let line = line.trim();
            line.starts_with("saved.value") || line.contains(" harc_assign(saved.value")
        }),
        "hook rendered a bare record local:\n{cpp}"
    );

    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);
    let common_cpp =
        std::fs::read_to_string(common_out.join("hook_record__test_HookRecordState.cpp"))
            .expect("read common capsule");
    assert!(
        common_cpp.contains("_harc_run_state._harc_testbench.saved.value"),
        "{common_cpp}"
    );
    assert!(
        !common_cpp.lines().any(|line| {
            let line = line.trim();
            line.starts_with("saved.value") || line.contains(" harc_assign(saved.value")
        }),
        "common hook rendered a bare record local:\n{common_cpp}"
    );

    let run_layout = |outdir: &Path, tag: &str| {
        Command::new(outdir.join("obj_dir/VTbirCommonReg"))
            .args(["--test", "HookRecordState"])
            .current_dir(outdir)
            .env("HARC_SEED", "5150")
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .output()
            .expect("run hook record fixture")
    };
    let self_run = run_layout(&self_out, "self");
    let common_run = run_layout(&common_out, "common");
    for (layout, run) in [("self-contained", &self_run), ("common", &common_run)] {
        let run_log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} hook record run failed:\n{run_log}"
        );
        assert!(run_log.contains("HOOK_RECORD_RESULT=13"), "{run_log}");
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).expect("read self-contained trace"),
        std::fs::read(common_out.join("common.jsonl")).expect("read common trace"),
        "hook record traces diverged"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_declared_component_scoreboard_and_copy_state_match_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_declared_component_scoreboard_and_copy_state_match_self_contained: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("receiver_state_inputs");
    let common_out = fresh_dir("receiver_state_common");
    let self_out = fresh_dir("receiver_state_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("receiver_state.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, COMMON_RECEIVER_STATE_TESTBENCH).expect("write HARC fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let interface = std::fs::read_to_string(common_out.join("receiver_state__suite_api.hpp"))
        .expect("read common interface");
    let runtime = std::fs::read_to_string(common_out.join("receiver_state__runtime.cpp"))
        .expect("read common runtime");
    assert!(
        interface.contains("ReceiverEnv& _harc_tb_component_receiver"),
        "declared component receiver missing:\n{interface}"
    );
    assert!(
        interface.contains("uint64_t _u__harc_tb_component_receiver"),
        "user parameter collided with the generated component receiver:\n{interface}"
    );
    assert!(
        runtime.contains(
            "ReceiverCell_bump(HarcTestContext& ctx, ReceiverCell& self, uint64_t _u_ctx)"
        ),
        "user parameter collided with the generated run context:\n{runtime}"
    );
    assert!(
        runtime.contains("uint64_t _u__harc_tb_component_receiver = 0;"),
        "user local collided with the generated component receiver:\n{runtime}"
    );
    assert_eq!(runtime.matches("uint64_t ReceiverTb_invoke(").count(), 1);
    assert_eq!(
        runtime
            .matches("uint64_t ReceiverTb_receiver_value(")
            .count(),
        1
    );
    assert_eq!(
        runtime.matches("uint64_t ReceiverEnv_copied_bump(").count(),
        1
    );
    assert_eq!(runtime.matches("uint64_t ReceiverCell_bump(").count(), 1);
    assert!(
        runtime.contains("ReceiverCell temp{};"),
        "component local absent:\n{runtime}"
    );

    for test in ["ReceiverA", "ReceiverB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VTbirCommonReg"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5150")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let self_run = run(&self_out, &format!("self_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("ALL TESTS PASSED"), "{layout} {test}:\n{log}");
        }
        assert_eq!(
            std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "receiver-state trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn canonical_testbench_tlm_binding_order_matches_v1_and_common() {
    if !verilator_present() {
        eprintln!(
            "SKIP canonical_testbench_tlm_binding_order_matches_v1_and_common: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("tlm_binding_order_inputs");
    let tbir_out = fresh_dir("tlm_binding_order_tbir");
    let v1_out = fresh_dir("tlm_binding_order_v1");
    let common_out = fresh_dir("tlm_binding_order_common");
    let sv = inputs.join("TlmMemory.sv");
    let tb = inputs.join("tlm_binding_order.harc");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/dut/TlmMemory.sv"),
        &sv,
    )
    .expect("copy TLM DUT fixture");
    std::fs::write(&tb, TESTBENCH_TLM_BINDING_ORDER_TESTBENCH).expect("write logical TLM fixture");

    build_self_contained(&sv, &tb, "TlmMemory", &tbir_out);
    build_v1(&sv, &tb, "TlmMemory", &v1_out);
    build_common_suite(&sv, &tb, "TlmMemory", &common_out);

    for test in ["LogicalOrderA", "LogicalOrderB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VTlmMemory"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5150")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let tbir_run = run(&tbir_out, &format!("tbir_{test}"));
        let v1_run = run(&v1_out, &format!("v1_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [
            ("TB-IR", &tbir_run),
            ("v1", &v1_run),
            ("common", &common_run),
        ] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("ALL TESTS PASSED"), "{layout} {test}:\n{log}");
        }
        assert_eq!(
            std::fs::read(tbir_out.join(format!("tbir_{test}.jsonl"))).unwrap(),
            std::fs::read(v1_out.join(format!("v1_{test}.jsonl"))).unwrap(),
            "v1/TB-IR trace mismatch for {test}"
        );
        assert_eq!(
            std::fs::read(tbir_out.join(format!("tbir_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "common/TB-IR trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(tbir_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn divergent_testbench_tlm_adapters_preserve_blocking_and_reverse_ooo_results() {
    if !verilator_present() {
        eprintln!(
            "SKIP divergent_testbench_tlm_adapters_preserve_blocking_and_reverse_ooo_results: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("divergent_tlm_inputs");
    let self_out = fresh_dir("divergent_tlm_self");
    let v1_out = fresh_dir("divergent_tlm_v1");
    let common_out = fresh_dir("divergent_tlm_common");
    let sv = inputs.join("DualTlmMemory.sv");
    let tb = inputs.join("divergent_tlm.harc");
    std::fs::write(
        &sv,
        r#"module TlmEndpoint #(parameter logic [31:0] BASE = 32'h100) (
  input logic clk,
  input logic rst,
  input logic read_req_valid,
  input logic [7:0] read_addr,
  output logic read_req_ready,
  output logic read_rsp_valid,
  output logic [31:0] read_rsp_data,
  input logic read_rsp_ready,
  input logic read_ooo_req_valid,
  input logic [7:0] read_ooo_addr,
  input logic [0:0] read_ooo_req_tag,
  output logic read_ooo_req_ready,
  output logic read_ooo_rsp_valid,
  output logic [31:0] read_ooo_rsp_data,
  output logic [0:0] read_ooo_rsp_tag,
  input logic read_ooo_rsp_ready
);
  logic ooo_valid [0:1];
  logic [31:0] ooo_data [0:1];
  logic [0:0] ooo_tag [0:1];
  assign read_req_ready = !read_rsp_valid || read_rsp_ready;
  assign read_ooo_req_ready = !ooo_valid[0] || !ooo_valid[1];
  assign read_ooo_rsp_valid = ooo_valid[1] || ooo_valid[0];
  assign read_ooo_rsp_data = ooo_valid[1] ? ooo_data[1] : ooo_data[0];
  assign read_ooo_rsp_tag = ooo_valid[1] ? ooo_tag[1] : ooo_tag[0];
  always_ff @(posedge clk) begin
    if (rst) begin
      read_rsp_valid <= 1'b0;
      read_rsp_data <= '0;
      ooo_valid[0] <= 1'b0;
      ooo_valid[1] <= 1'b0;
      ooo_data[0] <= '0;
      ooo_data[1] <= '0;
      ooo_tag[0] <= '0;
      ooo_tag[1] <= '0;
    end else begin
      if (read_rsp_valid && read_rsp_ready) read_rsp_valid <= 1'b0;
      if (read_req_valid && read_req_ready) begin
        read_rsp_valid <= 1'b1;
        read_rsp_data <= BASE + read_addr;
      end
      if (read_ooo_rsp_valid && read_ooo_rsp_ready) begin
        if (ooo_valid[1]) ooo_valid[1] <= 1'b0;
        else ooo_valid[0] <= 1'b0;
      end
      if (read_ooo_req_valid && read_ooo_req_ready) begin
        if (!ooo_valid[0]) begin
          ooo_valid[0] <= 1'b1;
          ooo_data[0] <= BASE + read_ooo_addr;
          ooo_tag[0] <= read_ooo_req_tag;
        end else begin
          ooo_valid[1] <= 1'b1;
          ooo_data[1] <= BASE + read_ooo_addr;
          ooo_tag[1] <= read_ooo_req_tag;
        end
      end
    end
  end
endmodule

module DualTlmMemory(
  input logic clk,
  input logic rst,
  input logic a_read_req_valid,
  input logic [7:0] a_read_addr,
  output logic a_read_req_ready,
  output logic a_read_rsp_valid,
  output logic [31:0] a_read_rsp_data,
  input logic a_read_rsp_ready,
  input logic a_read_ooo_req_valid,
  input logic [7:0] a_read_ooo_addr,
  input logic [0:0] a_read_ooo_req_tag,
  output logic a_read_ooo_req_ready,
  output logic a_read_ooo_rsp_valid,
  output logic [31:0] a_read_ooo_rsp_data,
  output logic [0:0] a_read_ooo_rsp_tag,
  input logic a_read_ooo_rsp_ready,
  input logic b_read_req_valid,
  input logic [7:0] b_read_addr,
  output logic b_read_req_ready,
  output logic b_read_rsp_valid,
  output logic [31:0] b_read_rsp_data,
  input logic b_read_rsp_ready,
  input logic b_read_ooo_req_valid,
  input logic [7:0] b_read_ooo_addr,
  input logic [0:0] b_read_ooo_req_tag,
  output logic b_read_ooo_req_ready,
  output logic b_read_ooo_rsp_valid,
  output logic [31:0] b_read_ooo_rsp_data,
  output logic [0:0] b_read_ooo_rsp_tag,
  input logic b_read_ooo_rsp_ready
);
  TlmEndpoint #(.BASE(32'h100)) a (
    .clk, .rst,
    .read_req_valid(a_read_req_valid), .read_addr(a_read_addr),
    .read_req_ready(a_read_req_ready), .read_rsp_valid(a_read_rsp_valid),
    .read_rsp_data(a_read_rsp_data), .read_rsp_ready(a_read_rsp_ready),
    .read_ooo_req_valid(a_read_ooo_req_valid), .read_ooo_addr(a_read_ooo_addr),
    .read_ooo_req_tag(a_read_ooo_req_tag), .read_ooo_req_ready(a_read_ooo_req_ready),
    .read_ooo_rsp_valid(a_read_ooo_rsp_valid), .read_ooo_rsp_data(a_read_ooo_rsp_data),
    .read_ooo_rsp_tag(a_read_ooo_rsp_tag), .read_ooo_rsp_ready(a_read_ooo_rsp_ready)
  );
  TlmEndpoint #(.BASE(32'h200)) b (
    .clk, .rst,
    .read_req_valid(b_read_req_valid), .read_addr(b_read_addr),
    .read_req_ready(b_read_req_ready), .read_rsp_valid(b_read_rsp_valid),
    .read_rsp_data(b_read_rsp_data), .read_rsp_ready(b_read_rsp_ready),
    .read_ooo_req_valid(b_read_ooo_req_valid), .read_ooo_addr(b_read_ooo_addr),
    .read_ooo_req_tag(b_read_ooo_req_tag), .read_ooo_req_ready(b_read_ooo_req_ready),
    .read_ooo_rsp_valid(b_read_ooo_rsp_valid), .read_ooo_rsp_data(b_read_ooo_rsp_data),
    .read_ooo_rsp_tag(b_read_ooo_rsp_tag), .read_ooo_rsp_ready(b_read_ooo_rsp_ready)
  );
endmodule
"#,
    )
    .expect("write divergent TLM DUT");
    std::fs::write(
        &tb,
        r#"bus LogicalBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus LogicalBus

testbench DivergentTlmTb
    dut : DualTlmMemory

    function read_once(addr: uint<8>) -> uint<32>
        let value = mem.read(addr)
        return value
    end function read_once

    function read_pair(addr: uint<8>) -> uint<32>
        let first = fork mem.read_ooo(addr)
        let second = fork mem.read_ooo(addr + 1)
        join_all
        return first + second
    end function read_pair

    function read_all(addr: uint<8>) -> uint<32>
        let direct = read_once(addr)
        let pair = read_pair(addr + 1)
        return direct + pair
    end function read_all
end testbench DivergentTlmTb

impl DivergentTlmA for DivergentTlmTb
    let mem : LogicalBus = bind dut with {
        read.addr: "a_read_addr", read.req_valid: "a_read_req_valid",
        read.req_ready: "a_read_req_ready", read.rsp_valid: "a_read_rsp_valid",
        read.rsp_data: "a_read_rsp_data", read.rsp_ready: "a_read_rsp_ready",
        read_ooo.addr: "a_read_ooo_addr", read_ooo.req_valid: "a_read_ooo_req_valid",
        read_ooo.req_ready: "a_read_ooo_req_ready", read_ooo.req_tag: "a_read_ooo_req_tag",
        read_ooo.rsp_valid: "a_read_ooo_rsp_valid", read_ooo.rsp_data: "a_read_ooo_rsp_data",
        read_ooo.rsp_tag: "a_read_ooo_rsp_tag", read_ooo.rsp_ready: "a_read_ooo_rsp_ready"
    }
    clock clk = 10ns
    run
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
        let total = read_all(5)
        assert total == 786 else fail("A total=${total}")
        let repeated = read_pair(8)
        assert repeated == 529 else fail("A repeated=${repeated}")
        log(info, "DIVERGENT_TLM=A:${total}:${repeated}")
    end run
end impl DivergentTlmA

impl DivergentTlmB for DivergentTlmTb
    let mem : LogicalBus = bind dut with {
        read.addr: "b_read_addr", read.req_valid: "b_read_req_valid",
        read.req_ready: "b_read_req_ready", read.rsp_valid: "b_read_rsp_valid",
        read.rsp_data: "b_read_rsp_data", read.rsp_ready: "b_read_rsp_ready",
        read_ooo.addr: "b_read_ooo_addr", read_ooo.req_valid: "b_read_ooo_req_valid",
        read_ooo.req_ready: "b_read_ooo_req_ready", read_ooo.req_tag: "b_read_ooo_req_tag",
        read_ooo.rsp_valid: "b_read_ooo_rsp_valid", read_ooo.rsp_data: "b_read_ooo_rsp_data",
        read_ooo.rsp_tag: "b_read_ooo_rsp_tag", read_ooo.rsp_ready: "b_read_ooo_rsp_ready"
    }
    clock clk = 10ns
    run
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
        let total = read_all(5)
        assert total == 1554 else fail("B total=${total}")
        let repeated = read_pair(8)
        assert repeated == 1041 else fail("B repeated=${repeated}")
        log(info, "DIVERGENT_TLM=B:${total}:${repeated}")
    end run
end impl DivergentTlmB
"#,
    )
    .expect("write divergent TLM HARC fixture");

    build_self_contained(&sv, &tb, "DualTlmMemory", &self_out);
    build_v1(&sv, &tb, "DualTlmMemory", &v1_out);
    build_common_suite(&sv, &tb, "DualTlmMemory", &common_out);
    let runtime = std::fs::read_to_string(common_out.join("divergent_tlm__runtime.cpp"))
        .expect("read divergent TLM runtime");
    assert!(!runtime.contains("ctx.dut->a_read_"), "{runtime}");
    assert!(!runtime.contains("ctx.dut->b_read_"), "{runtime}");
    assert_eq!(
        runtime.matches("uint64_t DivergentTlmTb_read_all(").count(),
        1,
        "{runtime}"
    );

    for test in ["DivergentTlmA", "DivergentTlmB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VDualTlmMemory"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5150")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let self_run = run(&self_out, &format!("self_{test}"));
        let v1_run = run(&v1_out, &format!("v1_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [
            ("self-contained", &self_run),
            ("v1", &v1_run),
            ("common", &common_run),
        ] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("DIVERGENT_TLM="), "{layout} {test}:\n{log}");
        }
        let self_trace = std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap();
        let v1_trace = std::fs::read(v1_out.join(format!("v1_{test}.jsonl"))).unwrap();
        let common_trace = std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap();
        assert_eq!(self_trace, v1_trace, "v1 trace mismatch for {test}");
        assert_eq!(self_trace, common_trace, "common trace mismatch for {test}");
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_qualified_method_wait_uses_each_test_clock_binding_in_both_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_qualified_method_wait_uses_each_test_clock_binding_in_both_layouts: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("qualified_wait_inputs");
    let self_out = fresh_dir("qualified_wait_self");
    let common_out = fresh_dir("qualified_wait_common");
    let sv = inputs.join("QualifiedClockTop.sv");
    let tb = inputs.join("qualified_wait.harc");
    std::fs::write(
        &sv,
        r#"module QualifiedClockTop(
  input logic fast_clk,
  input logic slow_clk,
  output logic [7:0] fast_count,
  output logic [7:0] slow_count
);
  initial begin
    fast_count = 0;
    slow_count = 0;
  end
  always_ff @(posedge fast_clk) fast_count <= fast_count + 1'b1;
  always_ff @(posedge slow_clk) slow_count <= slow_count + 1'b1;
endmodule
"#,
    )
    .expect("write dual-clock DUT");
    std::fs::write(
        &tb,
        r#"testbench QualifiedWaitTb
    dut : QualifiedClockTop

    function wait_slow()
        wait 1 cycle on slow_clk
    end function wait_slow
end testbench QualifiedWaitTb

impl QualifiedWaitA for QualifiedWaitTb
    clock fast_clk = 2ns
    clock slow_clk = 10ns
    run
        let before_slow = dut.slow_count
        let before_fast = dut.fast_count
        wait_slow()
        assert dut.slow_count == before_slow + 1 else fail("A wrong slow clock")
        assert dut.fast_count > before_fast else fail("A fast clock did not advance")
        log(info, "QUALIFIED_WAIT=A")
    end run
end impl QualifiedWaitA

impl QualifiedWaitB for QualifiedWaitTb
    clock slow_clk = 10ns
    clock fast_clk = 2ns
    run
        let before_slow = dut.slow_count
        let before_fast = dut.fast_count
        wait_slow()
        assert dut.slow_count == before_slow + 1 else fail("B wrong slow clock")
        assert dut.fast_count > before_fast else fail("B fast clock did not advance")
        log(info, "QUALIFIED_WAIT=B")
    end run
end impl QualifiedWaitB
"#,
    )
    .expect("write qualified-wait fixture");

    build_self_contained(&sv, &tb, "QualifiedClockTop", &self_out);
    let cpp = std::fs::read_to_string(self_out.join("qualified_wait.cpp"))
        .expect("read qualified-wait source");
    assert!(cpp.contains("wait_slow"));
    for test in ["QualifiedWaitA", "QualifiedWaitB"] {
        let run = Command::new(self_out.join("obj_dir/VQualifiedClockTop"))
            .args(["--test", test])
            .current_dir(&self_out)
            .env("HARC_SEED", "5150")
            .env("HARC_SIM_LOG", self_out.join(format!("{test}.log")))
            .env("HARC_TRACE", self_out.join(format!("self_{test}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {test}: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(run.status.success(), "{test} failed:\n{log}");
        assert!(log.contains("QUALIFIED_WAIT="), "{log}");
    }

    build_common_suite(&sv, &tb, "QualifiedClockTop", &common_out);
    let runtime = std::fs::read_to_string(common_out.join("qualified_wait__runtime.cpp"))
        .expect("read qualified-wait common runtime");
    assert!(runtime.contains("harc_wait_clock_cycles"), "{runtime}");
    for test in ["QualifiedWaitA", "QualifiedWaitB"] {
        let run = Command::new(common_out.join("obj_dir/VQualifiedClockTop"))
            .args(["--test", test])
            .current_dir(&common_out)
            .env("HARC_SEED", "5150")
            .env("HARC_SIM_LOG", common_out.join(format!("{test}.log")))
            .env(
                "HARC_TRACE",
                common_out.join(format!("common_{test}.jsonl")),
            )
            .output()
            .unwrap_or_else(|error| panic!("run common {test}: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(run.status.success(), "common {test} failed:\n{log}");
        assert!(log.contains("QUALIFIED_WAIT="), "{log}");
        assert_eq!(
            std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "clock scheduler trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn qualified_clock_wait_services_passive_target_in_all_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP qualified_clock_wait_services_passive_target_in_all_layouts: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("qualified_wait_target_inputs");
    let sv = inputs.join("QualifiedWaitTargetTop.sv");
    let tb = inputs.join("qualified_wait_target.harc");
    let v1_out = fresh_dir("qualified_wait_target_v1");
    let self_out = fresh_dir("qualified_wait_target_self");
    let common_out = fresh_dir("qualified_wait_target_common");
    let v1_mt_out = fresh_dir("qualified_wait_target_v1_mt");
    let self_mt_out = fresh_dir("qualified_wait_target_self_mt");
    let common_mt_out = fresh_dir("qualified_wait_target_common_mt");
    std::fs::write(
        &sv,
        r#"module QualifiedWaitTargetTop(
  input logic clk,
  input logic aux_clk,
  input logic rst,
  input logic start,
  output logic mem_read_req_valid,
  output logic [7:0] mem_read_addr,
  input logic mem_read_req_ready,
  input logic mem_read_rsp_valid,
  input logic [31:0] mem_read_rsp_data,
  output logic mem_read_rsp_ready,
  output logic done,
  output logic [31:0] result,
  output logic [7:0] accepted_count,
  output logic [7:0] accepted_cycle,
  output logic [7:0] primary_count,
  output logic [7:0] aux_count
);
  logic started;
  always_ff @(posedge clk) begin
    if (rst) begin
      started <= 1'b0;
      mem_read_req_valid <= 1'b0;
      mem_read_addr <= '0;
      mem_read_rsp_ready <= 1'b0;
      done <= 1'b0;
      result <= '0;
      accepted_count <= '0;
      accepted_cycle <= '0;
      primary_count <= '0;
    end else begin
      primary_count <= primary_count + 1'b1;
      if (start && !started) begin
        started <= 1'b1;
        mem_read_req_valid <= 1'b1;
        mem_read_addr <= 8'h2a;
      end else if (mem_read_req_valid) begin
        mem_read_req_valid <= 1'b0;
      end
      if (mem_read_req_valid && mem_read_req_ready) begin
        mem_read_req_valid <= 1'b0;
        mem_read_rsp_ready <= 1'b1;
        accepted_count <= accepted_count + 1'b1;
        accepted_cycle <= primary_count;
      end
      if (mem_read_rsp_valid && mem_read_rsp_ready) begin
        result <= mem_read_rsp_data;
        done <= 1'b1;
        mem_read_rsp_ready <= 1'b0;
      end
    end
  end
  always_ff @(posedge aux_clk) begin
    if (rst) aux_count <= '0;
    else aux_count <= aux_count + 1'b1;
  end
endmodule
"#,
    )
    .expect("write qualified-wait target DUT");
    std::fs::write(
        &tb,
        r#"bus WaitBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus WaitBus

transactor WaitTarget bound to WaitBus
    thread bus.read(addr: uint<8>)
        wait 1 cycle
        return 0x100 + addr
    end thread
end transactor WaitTarget

testbench QualifiedWaitTargetTb
    dut : QualifiedWaitTargetTop

    function wait_aux(cycles: uint<8>)
        wait cycles cycles on aux_clk
    end function wait_aux
end testbench QualifiedWaitTargetTb

impl QualifiedWaitTargetTest for QualifiedWaitTargetTb
    let mem : WaitBus = bind dut
    let target : WaitTarget passive = bind mem
    clock clk = 10ns
    clock aux_clk = 10ns
    run
        dut.rst = 1
        dut.start = 0
        wait_aux(2)
        dut.rst = 0
        dut.start = 1
        wait 1 cycle
        dut.start = 0
        let before_primary = dut.primary_count
        let before_aux = dut.aux_count
        let before_cycles = cycle_count
        wait_aux(6)
        assert dut.accepted_count == 1 else fail("entry-edge request was not accepted exactly once")
        assert dut.accepted_cycle == before_primary else fail("entry-edge request accepted at ${dut.accepted_cycle}, expected ${before_primary}")
        assert dut.done == 1 else fail("passive target froze during qualified wait")
        assert dut.result == 0x12a else fail("qualified-wait response ${dut.result}")
        assert dut.primary_count == before_primary + 6 else fail("primary count ${dut.primary_count}")
        assert dut.aux_count == before_aux + 6 else fail("aux count ${dut.aux_count}")
        assert cycle_count == before_cycles + 6 else fail("primary accounting ${cycle_count}")
        log(info, "PASS: qualified wait serviced passive target")
    end run
end impl QualifiedWaitTargetTest
"#,
    )
    .expect("write qualified-wait target test");

    build_v1(&sv, &tb, "QualifiedWaitTargetTop", &v1_out);
    build_self_contained(&sv, &tb, "QualifiedWaitTargetTop", &self_out);
    build_common_suite(&sv, &tb, "QualifiedWaitTargetTop", &common_out);
    build_v1_mt(&sv, &tb, "QualifiedWaitTargetTop", &v1_mt_out);
    build_self_contained_mt(&sv, &tb, "QualifiedWaitTargetTop", &self_mt_out);
    build_common_suite_mt(&sv, &tb, "QualifiedWaitTargetTop", &common_mt_out);
    for (layout, outdir, tag) in [
        ("v1", &v1_out, "v1"),
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
        ("v1-mt", &v1_mt_out, "v1_mt"),
        ("self-contained-mt", &self_mt_out, "self_mt"),
        ("common-mt", &common_mt_out, "common_mt"),
    ] {
        let trace = outdir.join(format!("{tag}.jsonl"));
        let run = Command::new(outdir.join("obj_dir/VQualifiedWaitTargetTop"))
            .args(["--test", "QualifiedWaitTargetTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5858")
            .env("HARC_TRACE", &trace)
            .output()
            .unwrap_or_else(|error| panic!("run {layout} qualified target wait: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} qualified target wait failed:\n{log}"
        );
        assert!(
            log.contains("PASS: qualified wait serviced passive target"),
            "{layout}:\n{log}"
        );
    }
    let self_trace = std::fs::read(self_out.join("self.jsonl")).unwrap();
    assert_eq!(
        self_trace,
        std::fs::read(v1_out.join("v1.jsonl")).unwrap(),
        "v1 qualified-wait target trace mismatch"
    );
    assert_eq!(
        self_trace,
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "common qualified-wait target trace mismatch"
    );
    assert_eq!(
        self_trace,
        std::fs::read(v1_mt_out.join("v1_mt.jsonl")).unwrap(),
        "MT v1 qualified-wait target trace mismatch"
    );
    assert_eq!(
        self_trace,
        std::fs::read(self_mt_out.join("self_mt.jsonl")).unwrap(),
        "MT self-contained qualified-wait target trace mismatch"
    );
    assert_eq!(
        self_trace,
        std::fs::read(common_mt_out.join("common_mt.jsonl")).unwrap(),
        "MT common qualified-wait target trace mismatch"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(v1_mt_out);
    let _ = std::fs::remove_dir_all(self_mt_out);
    let _ = std::fs::remove_dir_all(common_mt_out);
}

#[test]
fn tbir_common_scheduler_supports_clockless_and_multiclock_tests_together() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_scheduler_supports_clockless_and_multiclock_tests_together: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("mixed_clock_inputs");
    let self_out = fresh_dir("mixed_clock_self");
    let common_out = fresh_dir("mixed_clock_common");
    let sv = inputs.join("MixedClockTop.sv");
    let tb = inputs.join("mixed_clock.harc");
    std::fs::write(
        &sv,
        r#"module MixedClockTop(
  input logic clk,
  input logic aux_clk,
  input logic [7:0] d,
  output logic [7:0] q,
  output logic [7:0] aux_count
);
  initial begin
    q = 0;
    aux_count = 0;
  end
  always_ff @(posedge clk) q <= d;
  always_ff @(posedge aux_clk) aux_count <= aux_count + 1'b1;
endmodule
"#,
    )
    .expect("write mixed-clock DUT");
    std::fs::write(
        &tb,
        r#"test ClocklessRun
    let dut : MixedClockTop
    run
        dut.d = 17
        wait 1 cycle
        assert dut.q == 17 else fail("clockless legacy tick missed clk")
        log(info, "MIXED_CLOCK=clockless")
    end run
end test ClocklessRun

test MultiClockRun
    let dut : MixedClockTop
    clock clk = 10ns
    clock aux_clk = 4ns
    run
        dut.d = 29
        wait 1 cycle
        assert dut.q == 29 else fail("primary clock missed data")
        assert dut.aux_count == 3 else fail("simultaneous-edge scheduler lost aux edges")
        log(info, "MIXED_CLOCK=multi")
    end run
end test MultiClockRun
"#,
    )
    .expect("write mixed-clock fixture");

    build_self_contained(&sv, &tb, "MixedClockTop", &self_out);
    build_common_suite(&sv, &tb, "MixedClockTop", &common_out);
    for test in ["ClocklessRun", "MultiClockRun"] {
        for (layout, outdir, tag) in [
            ("self-contained", &self_out, "self"),
            ("common", &common_out, "common"),
        ] {
            let run = Command::new(outdir.join("obj_dir/VMixedClockTop"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5252")
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}_{test}.log")))
                .env("HARC_TRACE", outdir.join(format!("{tag}_{test}.jsonl")))
                .output()
                .unwrap_or_else(|error| panic!("run {layout} {test}: {error}"));
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
            assert!(run.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("MIXED_CLOCK="), "{layout} {test}:\n{log}");
        }
        assert_eq!(
            std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "mixed clock trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_target_responder_matches_self_contained_runtime() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_target_responder_matches_self_contained_runtime: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmReadInitiator.sv");
    let tb = root.join("tests/fixtures/tlm_target_thread_test.harc");
    let self_out = fresh_dir("target_responder_self");
    let common_out = fresh_dir("target_responder_common");

    build_self_contained(&sv, &tb, "TlmReadInitiator", &self_out);
    build_common_suite(&sv, &tb, "TlmReadInitiator", &common_out);
    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmReadInitiator"))
            .args(["--test", "TlmTargetThreadTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5353")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} target responder: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} target responder failed:\n{log}"
        );
        assert!(
            log.contains("PASS: HARC target-side TLM thread served SV initiator"),
            "{layout} target responder:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "target responder trace mismatch"
    );

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_bound_monitor_component_uses_typed_adapter_runtime() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_bound_monitor_component_uses_typed_adapter_runtime: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmReadInitiator.sv");
    let tb = root.join("tests/fixtures/tlm_target_thread_monitor_test.harc");
    let self_out = fresh_dir("target_monitor_self");
    let common_out = fresh_dir("target_monitor_common");

    build_self_contained(&sv, &tb, "TlmReadInitiator", &self_out);
    let build_log = build_common_suite(&sv, &tb, "TlmReadInitiator", &common_out);
    assert!(
        build_log.contains("TargetResponder=1"),
        "capsule telemetry must count target responders:\n{build_log}"
    );
    let runtime =
        std::fs::read_to_string(common_out.join("tlm_target_thread_monitor_test__runtime.cpp"))
            .expect("read common monitor runtime");
    assert_eq!(
        runtime.matches("void TlmMemTarget_cycle_h2(").count(),
        1,
        "the component monitor body must be emitted once:\n{runtime}"
    );
    assert!(
        runtime.contains("HarcBusSignalRef<uint64_t> _harc_bus_signal_0"),
        "the shared monitor must receive its bound payload through a typed adapter:\n{runtime}"
    );

    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmReadInitiator"))
            .args(["--test", "TlmTargetThreadMonitorTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5354")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} bound monitor: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} bound monitor failed:\n{log}"
        );
        assert!(
            log.contains("PASS: bound target thread and monitor share one instance"),
            "{layout} bound monitor:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "bound monitor trace mismatch"
    );

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_passive_mixed_target_runs_only_its_always_responder() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_passive_mixed_target_runs_only_its_always_responder: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmReadInitiator.sv");
    let tb = root.join("tests/fixtures/tlm_target_always_mixed_passive_test.harc");
    let self_out = fresh_dir("target_always_passive_self");
    let common_out = fresh_dir("target_always_passive_common");

    build_self_contained(&sv, &tb, "TlmReadInitiator", &self_out);
    build_common_suite(&sv, &tb, "TlmReadInitiator", &common_out);
    let capsule = std::fs::read_to_string(
        common_out
            .join("tlm_target_always_mixed_passive_test__test_TlmTargetAlwaysMixedPassiveTest.cpp"),
    )
    .expect("read passive mixed target capsule");
    assert!(capsule.contains("_target_read_target_slot"), "{capsule}");
    assert!(!capsule.contains("active event"), "{capsule}");

    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmReadInitiator"))
            .args(["--test", "TlmTargetAlwaysMixedPassiveTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5252")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} passive mixed target: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} passive mixed target failed:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "passive always-responder trace mismatch"
    );

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_mt_target_workers_are_selected_run_owned_and_torn_down() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_mt_target_workers_are_selected_run_owned_and_torn_down: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmReadInitiator.sv");
    let tb = root.join("tests/fixtures/tlm_target_thread_test.harc");
    let self_out = fresh_dir("target_responder_mt_self");
    let common_out = fresh_dir("target_responder_mt_common");

    build_self_contained_mt(&sv, &tb, "TlmReadInitiator", &self_out);
    build_common_suite(&sv, &tb, "TlmReadInitiator", &common_out);
    let manifest_path = common_out.join("tlm_target_thread_test__artifacts.json");
    let single_threaded_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).expect("read single-threaded target manifest"),
    )
    .expect("parse single-threaded target manifest");
    let mt_rebuild = build_common_suite_mt(&sv, &tb, "TlmReadInitiator", &common_out);
    assert!(
        !mt_rebuild.contains(", 0 rewritten,"),
        "changing worker topology must republish common artifacts:\n{mt_rebuild}"
    );
    let mt_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).expect("read MT target manifest"),
    )
    .expect("parse MT target manifest");
    assert_ne!(
        single_threaded_manifest["build_profile"], mt_manifest["build_profile"],
        "--mt must be part of the published common build identity"
    );
    let runtime = std::fs::read_to_string(common_out.join("tlm_target_thread_test__runtime.cpp"))
        .expect("read MT common runtime");
    let capsule = std::fs::read_to_string(
        common_out.join("tlm_target_thread_test__test_TlmTargetThreadTest.cpp"),
    )
    .expect("read MT target capsule");
    assert!(runtime.contains("worker_shutdown"), "{runtime}");
    assert!(runtime.contains("worker.join()"), "{runtime}");
    assert!(
        capsule.contains("ctx.actor_schedulers.emplace_back"),
        "{capsule}"
    );

    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmReadInitiator"))
            .args(["--test", "TlmTargetThreadTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5454")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} MT target responder: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} MT target responder failed:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "MT target responder trace mismatch"
    );

    let registry = common_out.join("tlm_target_thread_test__registry.cpp");
    std::fs::write(
        &registry,
        r#"#include "tlm_target_thread_test__suite_api.hpp"
#include <cstdlib>
#include <string>

extern "C" const HarcTestDescriptor harc_test_TlmTargetThreadTest;

static int invoke(const std::string& directory, const char* tag) {
    std::string trace = directory + "/" + tag + ".jsonl";
    std::string log = directory + "/" + tag + ".log";
    setenv("HARC_SEED", "5454", 1);
    setenv("HARC_TRACE", trace.c_str(), 1);
    setenv("HARC_SIM_LOG", log.c_str(), 1);
    char arg0[] = "target_mt_sequence";
    char* argv[] = {arg0, nullptr};
    return harc_test_TlmTargetThreadTest.run(1, argv);
}

int main(int argc, char** argv) {
    if (argc != 2) return 90;
    if (invoke(argv[1], "first") != 0) return 1;
    if (invoke(argv[1], "second") != 0) return 2;
    return 0;
}
"#,
    )
    .expect("install MT target sequential harness");
    let registry_object = common_out.join("obj_dir/tlm_target_thread_test__registry.o");
    if registry_object.exists() {
        std::fs::remove_file(&registry_object).expect("remove original MT registry object");
    }
    let link = relink(&common_out.join("obj_dir"), "TlmReadInitiator");
    let link_log = format!(
        "{}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );
    assert!(
        link.status.success(),
        "MT sequential harness link failed:\n{link_log}"
    );
    let run = Command::new(common_out.join("obj_dir/VTlmReadInitiator"))
        .arg(&common_out)
        .current_dir(&common_out)
        .output()
        .expect("run MT target sequential harness");
    let run_log = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.status.success(),
        "MT sequential target runs failed:\n{run_log}"
    );
    assert_eq!(
        std::fs::read(common_out.join("first.jsonl")).unwrap(),
        std::fs::read(common_out.join("second.jsonl")).unwrap(),
        "selected-run MT state leaked across sequential runs"
    );

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_mt_fatal_run_joins_parked_target_workers() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_mt_fatal_run_joins_parked_target_workers: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("target_fatal_mt_inputs");
    let self_out = fresh_dir("target_fatal_mt_self");
    let common_out = fresh_dir("target_fatal_mt_common");
    let sv = root.join("tests/dut/TlmReadInitiator.sv");
    let tb = inputs.join("target_fatal_mt.harc");
    std::fs::write(
        &tb,
        r#"bus TlmMemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmMemBus

transactor TlmMemTarget bound to TlmMemBus
    thread bus.read(addr: uint<8>)
        return 256 + addr
    end thread
end transactor TlmMemTarget

testbench TargetFatalTb
    dut : TlmReadInitiator
end testbench TargetFatalTb

impl TargetBuildPass for TargetFatalTb
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
        wait until dut.done == 1 timeout 40 cycles fail("build-pass target did not respond")
    end run
end impl TargetBuildPass

impl TargetFatalTest for TargetFatalTb
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        dut.rst = 1
        wait 2 cycles
        log(fatal, "EXPECTED_MT_TARGET_FATAL")
    end run
end impl TargetFatalTest
"#,
    )
    .expect("write MT fatal target fixture");

    build_self_contained_mt(&sv, &tb, "TlmReadInitiator", &self_out);
    build_common_suite_mt(&sv, &tb, "TlmReadInitiator", &common_out);
    for (layout, outdir) in [("self-contained", &self_out), ("common", &common_out)] {
        let run = Command::new(outdir.join("obj_dir/VTlmReadInitiator"))
            .args(["--test", "TargetFatalTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5959")
            .env("HARC_SIM_LOG", outdir.join(format!("{layout}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} MT fatal target: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(!run.status.success(), "{layout} fatal run passed:\n{log}");
        assert!(log.contains("EXPECTED_MT_TARGET_FATAL"), "{layout}:\n{log}");
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_target_responder_forwards_through_explicit_back_bus() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_target_responder_forwards_through_explicit_back_bus: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("target_forwarding_inputs");
    let self_out = fresh_dir("target_forwarding_self");
    let common_out = fresh_dir("target_forwarding_common");
    let sv = inputs.join("TlmForwardingAll.sv");
    let mut source = String::new();
    for file in [
        "tests/dut/TlmReadInitiatorPair.sv",
        "tests/dut/TlmMemory.sv",
        "tests/dut/TlmForwardingTop.sv",
    ] {
        source.push_str(&std::fs::read_to_string(root.join(file)).expect("read forwarding RTL"));
        source.push('\n');
    }
    std::fs::write(&sv, source).expect("write combined forwarding RTL");
    let tb = root.join("tests/fixtures/tlm_target_forwarding_test.harc");

    build_self_contained(&sv, &tb, "TlmForwardingTop", &self_out);
    build_common_suite(&sv, &tb, "TlmForwardingTop", &common_out);
    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmForwardingTop"))
            .args(["--test", "TlmTargetForwardingTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5555")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} target forwarding: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} target forwarding failed:\n{log}"
        );
        assert!(
            log.contains("ALL TESTS PASSED - tlm_target_forwarding_test"),
            "{layout} target forwarding:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "target forwarding trace mismatch"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_mt_target_forwarding_yields_to_its_owned_scheduler() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_mt_target_forwarding_yields_to_its_owned_scheduler: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("target_forwarding_mt_inputs");
    let self_out = fresh_dir("target_forwarding_mt_self");
    let common_out = fresh_dir("target_forwarding_mt_common");
    let sv = inputs.join("TlmForwardingAll.sv");
    let mut source = String::new();
    for file in [
        "tests/dut/TlmReadInitiatorPair.sv",
        "tests/dut/TlmMemory.sv",
        "tests/dut/TlmForwardingTop.sv",
    ] {
        source.push_str(&std::fs::read_to_string(root.join(file)).expect("read forwarding RTL"));
        source.push('\n');
    }
    std::fs::write(&sv, source).expect("write combined forwarding RTL");
    let tb = root.join("tests/fixtures/tlm_target_forwarding_test.harc");

    build_self_contained_mt(&sv, &tb, "TlmForwardingTop", &self_out);
    build_common_suite_mt(&sv, &tb, "TlmForwardingTop", &common_out);
    let capsule = std::fs::read_to_string(
        common_out.join("tlm_target_forwarding_test__test_TlmTargetForwardingTest.cpp"),
    )
    .expect("read MT forwarding capsule");
    let forwarded_call = capsule
        .split("// bus.read tlm_method")
        .nth(1)
        .expect("forwarded target call is rendered");
    let forwarded_call = forwarded_call
        .lines()
        .take(48)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        forwarded_call.contains("co_await harc_rt::wait_cycles(_slot, 1)"),
        "target forwarding must suspend its actor slot, not advance shared time:\n{forwarded_call}"
    );
    assert!(!forwarded_call.contains("tick();"), "{forwarded_call}");

    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmForwardingTop"))
            .args(["--test", "TlmTargetForwardingTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5757")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} MT target forwarding: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} MT target forwarding failed:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "MT target forwarding trace mismatch"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_mt_target_fork_forwarding_routes_reverse_tagged_responses() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_mt_target_fork_forwarding_routes_reverse_tagged_responses: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("target_fork_forwarding_mt_inputs");
    let self_out = fresh_dir("target_fork_forwarding_mt_self");
    let common_out = fresh_dir("target_fork_forwarding_mt_common");
    let sv = inputs.join("TlmForkForwardingAll.sv");
    let mut source = String::new();
    for file in [
        "tests/dut/TlmReadInitiatorPair.sv",
        "tests/dut/TlmMemory.sv",
        "tests/dut/TlmForkForwardingTop.sv",
    ] {
        source
            .push_str(&std::fs::read_to_string(root.join(file)).expect("read fork forwarding RTL"));
        source.push('\n');
    }
    std::fs::write(&sv, source).expect("write combined fork forwarding RTL");
    let tb = root.join("tests/fixtures/tlm_target_fork_forwarding_test.harc");

    build_self_contained_mt(&sv, &tb, "TlmForkForwardingTop", &self_out);
    build_common_suite_mt(&sv, &tb, "TlmForkForwardingTop", &common_out);
    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmForkForwardingTop"))
            .args(["--test", "TlmTargetForkForwardingTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "6060")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} MT fork forwarding: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} MT fork forwarding failed:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "MT target fork-forwarding trace mismatch"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_tagged_target_responder_preserves_reverse_completion() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_tagged_target_responder_preserves_reverse_completion: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmOooReadInitiatorPair.sv");
    let tb = root.join("tests/fixtures/tlm_target_ooo_lanes_test.harc");
    let self_out = fresh_dir("target_ooo_self");
    let common_out = fresh_dir("target_ooo_common");

    build_self_contained(&sv, &tb, "TlmOooReadInitiatorPair", &self_out);
    build_common_suite(&sv, &tb, "TlmOooReadInitiatorPair", &common_out);
    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmOooReadInitiatorPair"))
            .args(["--test", "TlmTargetOooLanesTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5656")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} tagged target: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} tagged target failed:\n{log}"
        );
        assert!(
            log.contains("ALL TESTS PASSED - tlm_target_ooo_lanes_test"),
            "{layout} tagged target:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "tagged target trace mismatch"
    );

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_shared_record_tlm_response_adapter_runs_per_binding() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_shared_record_tlm_response_adapter_runs_per_binding: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("shared_record_tlm_inputs");
    let sv = inputs.join("SharedRecordResponder.sv");
    let tb = inputs.join("shared_record_tlm.harc");
    let self_out = fresh_dir("shared_record_tlm_self");
    let common_out = fresh_dir("shared_record_tlm_common");
    std::fs::write(
        &sv,
        r#"module SharedRecordResponder(
  input logic clk,
  input logic rst,
  input logic a_req_valid,
  input logic [7:0] a_addr,
  output logic a_req_ready,
  output logic a_rsp_valid,
  output logic [7:0] a_rsp_data,
  input logic a_rsp_ready,
  output logic [7:0] a_count,
  input logic b_req_valid,
  input logic [7:0] b_addr,
  output logic b_req_ready,
  output logic b_rsp_valid,
  output logic [7:0] b_rsp_data,
  input logic b_rsp_ready,
  output logic [7:0] b_count
);
  assign a_req_ready = 1'b1;
  assign b_req_ready = 1'b1;
  always_ff @(posedge clk) begin
    if (rst) begin
      a_rsp_valid <= 1'b0;
      a_rsp_data <= '0;
      a_count <= '0;
      b_rsp_valid <= 1'b0;
      b_rsp_data <= '0;
      b_count <= '0;
    end else begin
      if (a_rsp_valid && a_rsp_ready) a_rsp_valid <= 1'b0;
      if (b_rsp_valid && b_rsp_ready) b_rsp_valid <= 1'b0;
      if (a_req_valid && a_req_ready) begin
        a_rsp_valid <= 1'b1;
        a_rsp_data <= a_addr + 8'h10;
        a_count <= a_count + 1'b1;
      end
      if (b_req_valid && b_req_ready) begin
        b_rsp_valid <= 1'b1;
        b_rsp_data <= b_addr + 8'h20;
        b_count <= b_count + 1'b1;
      end
    end
  end
endmodule
"#,
    )
    .expect("write shared record responder DUT");
    std::fs::write(
        &tb,
        r#"struct Reply
    data : uint<8>
end struct Reply

bus RecordBus
    tlm_method read(addr: uint<8>) -> Reply: blocking;
end bus RecordBus

testbench RecordTb
    dut : SharedRecordResponder

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
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
        let reply = read_one(1)
        assert reply.data == 0x11 else fail("A record response ${reply.data}")
        assert dut.a_count == 1 else fail("A count ${dut.a_count}")
        assert dut.b_count == 0 else fail("A touched poisoned B binding")
        log(info, "PASS: shared record TLM A")
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
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
        let reply = read_one(2)
        assert reply.data == 0x22 else fail("B record response ${reply.data}")
        assert dut.b_count == 1 else fail("B count ${dut.b_count}")
        assert dut.a_count == 0 else fail("B touched poisoned A binding")
        log(info, "PASS: shared record TLM B")
    end run
end impl RecordB
"#,
    )
    .expect("write shared record responder test");

    build_self_contained(&sv, &tb, "SharedRecordResponder", &self_out);
    build_common_suite(&sv, &tb, "SharedRecordResponder", &common_out);
    let runtime = std::fs::read_to_string(common_out.join("shared_record_tlm__runtime.cpp"))
        .expect("read shared record runtime");
    assert!(runtime.contains("HarcBusSignalRef<Reply>"), "{runtime}");
    assert!(runtime.contains(".harc_read();"), "{runtime}");
    assert!(
        !runtime.contains("harc_unpack_Reply(_harc_bus_signal_"),
        "a typed record adapter must not be unpacked a second time:\n{runtime}"
    );

    for test in ["RecordA", "RecordB"] {
        for (layout, outdir, tag) in [
            ("self-contained", &self_out, "self"),
            ("common", &common_out, "common"),
        ] {
            let trace = outdir.join(format!("{tag}_{test}.jsonl"));
            let run = Command::new(outdir.join("obj_dir/VSharedRecordResponder"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5656")
                .env("HARC_TRACE", &trace)
                .output()
                .unwrap_or_else(|error| panic!("run {layout} {test}: {error}"));
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
            assert!(run.status.success(), "{layout} {test} failed:\n{log}");
            assert!(
                log.contains(&format!("PASS: shared record TLM {}", &test[6..])),
                "{layout} {test}:\n{log}"
            );
        }
        assert_eq!(
            std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "shared record TLM trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn signed_blocking_and_record_ooo_targets_match_all_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP signed_blocking_and_record_ooo_targets_match_all_layouts: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmSignedRecordOooInitiator.sv");
    let tb = root.join("tests/fixtures/tlm_target_signed_record_ooo_test.harc");
    let v1_out = fresh_dir("target_signed_record_v1");
    let self_out = fresh_dir("target_signed_record_self");
    let common_out = fresh_dir("target_signed_record_common");

    build_v1(&sv, &tb, "TlmSignedRecordOooInitiator", &v1_out);
    build_self_contained(&sv, &tb, "TlmSignedRecordOooInitiator", &self_out);
    build_common_suite(&sv, &tb, "TlmSignedRecordOooInitiator", &common_out);

    let capsule = std::fs::read_to_string(
        common_out.join("tlm_target_signed_record_ooo_test__test_SignedRecordTargetTest.cpp"),
    )
    .expect("read typed target capsule");
    assert!(
        capsule
            .contains("std::array<SignedRecordReply, 2> _target_transform_target_ooo_arg_request"),
        "record request lanes must use the exact IR type:\n{capsule}"
    );
    assert!(
        capsule.contains(
            "std::array<SignedRecordReply, 2> _target_transform_target_ooo_lane_rsp_data"
        ),
        "record response lanes must not collapse to uint64_t:\n{capsule}"
    );
    assert!(
        capsule.contains("harc_sext_u128(static_cast<_harc_u128>(value), 8, 65)"),
        "blocking and record-field signed widening must retain the sign bit:\n{capsule}"
    );

    for (layout, outdir) in [
        ("v1", &v1_out),
        ("self-contained", &self_out),
        ("common", &common_out),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmSignedRecordOooInitiator"))
            .args(["--test", "SignedRecordTargetTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5757")
            .output()
            .unwrap_or_else(|error| panic!("run {layout} signed record target: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} signed record target failed:\n{log}"
        );
        assert!(
            log.contains("ALL TESTS PASSED - tlm_target_signed_record_ooo_test"),
            "{layout} signed record target:\n{log}"
        );
    }

    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_mt_tagged_target_workers_preserve_reverse_completion() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_mt_tagged_target_workers_preserve_reverse_completion: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmOooReadInitiatorPair.sv");
    let tb = root.join("tests/fixtures/tlm_target_ooo_lanes_test.harc");
    let self_out = fresh_dir("target_ooo_mt_self");
    let common_out = fresh_dir("target_ooo_mt_common");

    build_self_contained_mt(&sv, &tb, "TlmOooReadInitiatorPair", &self_out);
    build_common_suite_mt(&sv, &tb, "TlmOooReadInitiatorPair", &common_out);
    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmOooReadInitiatorPair"))
            .args(["--test", "TlmTargetOooLanesTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5858")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} MT tagged target: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} MT tagged target failed:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "MT tagged target trace mismatch"
    );

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_untagged_fork_join_matches_all_layouts() {
    if !verilator_present() {
        eprintln!("SKIP tbir_untagged_fork_join_matches_all_layouts: `verilator` not found");
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sv = root.join("tests/dut/TlmMemory.sv");
    let tb = root.join("tests/fixtures/tlm_method_blocking_fork_bus_test.harc");
    let self_out = fresh_dir("untagged_fork_self");
    let v1_out = fresh_dir("untagged_fork_v1");
    let common_out = fresh_dir("untagged_fork_common");

    build_self_contained(&sv, &tb, "TlmMemory", &self_out);
    build_v1(&sv, &tb, "TlmMemory", &v1_out);
    build_common_suite(&sv, &tb, "TlmMemory", &common_out);
    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("v1", &v1_out, "v1"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmMemory"))
            .args(["--test", "TlmMethodBlockingForkBusTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "5757")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} untagged fork: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} untagged fork failed:\n{log}"
        );
        assert!(
            log.contains("PASS: blocking fork bus tlm_method read"),
            "{layout} untagged fork:\n{log}"
        );
    }
    let self_trace = std::fs::read(self_out.join("self.jsonl")).unwrap();
    assert_eq!(self_trace, std::fs::read(v1_out.join("v1.jsonl")).unwrap());
    assert_eq!(
        self_trace,
        std::fs::read(common_out.join("common.jsonl")).unwrap()
    );

    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_builds_one_model_and_dispatches_distinct_tests() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_builds_one_model_and_dispatches_distinct_tests: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("inputs");
    let outdir = fresh_dir("output");
    let (sv, tb) = write_fixture(&inputs);
    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(&sv)
        .arg(&tb)
        .args(["--top", "TbirCommonReg", "--codegen", "tbir"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .args(["--emit-jobs", "2", "--jobs", "2"])
        .arg("--outdir")
        .arg(&outdir)
        .env("HARC_SEED", "1")
        .output()
        .expect("run TB-IR common build");
    let build_log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "common build failed:\n{build_log}");
    assert_eq!(
        build_log.matches("running: verilator").count(),
        1,
        "expected one Verilator build for the suite:\n{build_log}"
    );

    let expected = vec![
        "tbir_common__artifacts.json".to_string(),
        "tbir_common__registry.cpp".to_string(),
        "tbir_common__runtime.cpp".to_string(),
        "tbir_common__suite_api.hpp".to_string(),
        "tbir_common__test_Common17.cpp".to_string(),
        "tbir_common__test_Common203.cpp".to_string(),
    ];
    let mut actual: Vec<String> = std::fs::read_dir(&outdir)
        .expect("read output directory")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("tbir_common__"))
        .collect();
    actual.sort();
    assert_eq!(actual, expected);

    let manifest =
        std::fs::read_to_string(outdir.join("tbir_common__artifacts.json")).expect("read manifest");
    let manifest: serde_json::Value = serde_json::from_str(&manifest).expect("parse manifest");
    assert_eq!(
        manifest["tests"],
        serde_json::json!(["Common17", "Common203"])
    );
    let artifact_names = manifest["artifacts"]
        .as_array()
        .expect("manifest artifacts")
        .iter()
        .map(|artifact| artifact["filename"].as_str().expect("artifact filename"))
        .collect::<Vec<_>>();
    assert_eq!(
        artifact_names,
        vec![
            "tbir_common__suite_api.hpp",
            "tbir_common__runtime.cpp",
            "tbir_common__test_Common17.cpp",
            "tbir_common__test_Common203.cpp",
            "tbir_common__registry.cpp",
            "harc_thread_rt.h",
            "harc_random_rt.h",
            "harc_queue_rt.h",
            "harc_trace_rt.h",
            "harc_log_rt.h",
            "harc_z3_rt.h"
        ]
    );

    let interface =
        std::fs::read_to_string(outdir.join("tbir_common__suite_api.hpp")).expect("read interface");
    let runtime =
        std::fs::read_to_string(outdir.join("tbir_common__runtime.cpp")).expect("read runtime");
    let capsule17 = std::fs::read_to_string(outdir.join("tbir_common__test_Common17.cpp"))
        .expect("read Common17 capsule");
    let capsule203 = std::fs::read_to_string(outdir.join("tbir_common__test_Common203.cpp"))
        .expect("read Common203 capsule");
    let registry =
        std::fs::read_to_string(outdir.join("tbir_common__registry.cpp")).expect("read registry");
    let all = format!("{interface}\n{runtime}\n{capsule17}\n{capsule203}\n{registry}");
    assert_eq!(all.matches("HarcTestContext ctx;").count(), 1);
    assert_eq!(all.matches("new VTbirCommonReg").count(), 1);
    assert!(capsule17.contains("harc_body_Common17"));
    assert!(!capsule17.contains("Common203"));
    assert!(capsule203.contains("harc_body_Common203"));
    assert!(!capsule203.contains("Common17"));
    assert!(!capsule17.contains("ThreadScheduler scheduler"));
    assert!(!capsule203.contains("ThreadScheduler scheduler"));
    assert!(
        registry.find("harc_test_Common17").unwrap()
            < registry.find("harc_test_Common203").unwrap()
    );

    let binary = outdir.join("obj_dir/VTbirCommonReg");
    assert!(
        binary.exists(),
        "missing suite executable: {}",
        binary.display()
    );
    for (name, marker) in [
        ("Common17", "COMMON_RESULT=17"),
        ("Common203", "COMMON_RESULT=203"),
    ] {
        let run = Command::new(&binary)
            .args(["--test", name])
            .current_dir(&outdir)
            .env("HARC_SEED", "1")
            .env("HARC_SIM_LOG", outdir.join(format!("{name}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run {name}: {error}"));
        let run_log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(run.status.success(), "{name} failed:\n{run_log}");
        assert!(run_log.contains(marker), "missing `{marker}`:\n{run_log}");
        assert!(
            run_log.contains("ALL TESTS PASSED"),
            "{name} did not pass:\n{run_log}"
        );
    }

    let unknown = Command::new(&binary)
        .args(["--test", "NoSuchTest"])
        .current_dir(&outdir)
        .output()
        .expect("run unknown selector");
    let unknown_log = format!(
        "{}{}",
        String::from_utf8_lossy(&unknown.stdout),
        String::from_utf8_lossy(&unknown.stderr)
    );
    assert!(!unknown.status.success());
    assert!(unknown_log.contains("unknown test"), "{unknown_log}");
    assert!(unknown_log.contains("Common17, Common203"), "{unknown_log}");

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(outdir);
}

#[test]
fn tbir_common_shared_types_helpers_and_tseqs_match_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_shared_types_helpers_and_tseqs_match_self_contained: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("shared_inputs");
    let common_out = fresh_dir("shared_common");
    let self_out = fresh_dir("shared_self");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("shared.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(&tb, SHARED_TYPES_AND_CALLABLES_TESTBENCH).expect("write shared HARC fixture");

    build_self_contained(&sv, &tb, "TbirCommonReg", &self_out);
    build_common_suite(&sv, &tb, "TbirCommonReg", &common_out);

    let interface = std::fs::read_to_string(common_out.join("shared__suite_api.hpp"))
        .expect("read shared interface");
    let runtime = std::fs::read_to_string(common_out.join("shared__runtime.cpp"))
        .expect("read shared runtime");
    let capsule_a = std::fs::read_to_string(common_out.join("shared__test_SharedTypesA.cpp"))
        .expect("read A capsule");
    let capsule_b = std::fs::read_to_string(common_out.join("shared__test_SharedTypesB.cpp"))
        .expect("read B capsule");
    let all = format!("{interface}\n{runtime}\n{capsule_a}\n{capsule_b}");
    for definition in [
        "struct InnerValue {",
        "struct OuterValue {",
        "struct SharedScoreboard {",
        "struct LeafState {",
        "struct ParentState {",
        "struct _StatefulTarget_state {",
        "struct StateCov {",
        "struct SharedTypesTb {",
        "harc_helper_widen_plus_one(uint64_t x) {",
        "harc_rt::HarcWide<5> harc_helper_wide_identity(harc_rt::HarcWide<5> value) {",
        "harc_tseq_PureValues(uint64_t seed) {",
        "harc_tseq_ForwardPure(uint64_t seed) {",
        "std::vector<uint64_t> harc_tseq_CopyScalarValues(std::vector<uint64_t> values) {",
        "std::vector<InnerValue> harc_tseq_CopyRecordValues(std::vector<InnerValue> values) {",
        "std::vector<InnerValue> harc_tseq_EchoRecord(InnerValue value) {",
        "std::vector<InnerValue> harc_tseq_RecordValues(uint64_t seed) {",
        "harc_tseq_TimedValues(HarcTestContext& ctx, uint64_t seed) {",
        "harc_tseq_ForwardTimed(HarcTestContext& ctx, uint64_t seed) {",
    ] {
        assert_eq!(
            all.matches(definition).count(),
            1,
            "`{definition}` must have one generated owner"
        );
    }
    assert_eq!(
        all.matches("void StateCov::report(harc_rt::log::HarcLogContext& log_ctx) const {")
            .count(),
        1
    );
    assert!(
        !interface.contains("void StateCov::report(harc_rt::log::HarcLogContext& log_ctx) const {")
    );
    assert!(!interface.contains("harc_helper_widen_plus_one(uint64_t x) {"));
    assert!(!interface.contains("harc_tseq_PureValues(uint64_t seed) {"));
    assert!(!capsule_a.contains("harc_helper_widen_plus_one(uint64_t x) {"));
    assert!(!capsule_b.contains("harc_helper_widen_plus_one(uint64_t x) {"));

    for test in ["SharedTypesA", "SharedTypesB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VTbirCommonReg"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "4242")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let self_run = run(&self_out, &format!("self_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("ALL TESTS PASSED"), "{layout} {test}:\n{log}");
        }
        let self_trace = std::fs::read(self_out.join(format!("self_{test}.jsonl")))
            .expect("read self-contained trace");
        let common_trace = std::fs::read(common_out.join(format!("common_{test}.jsonl")))
            .expect("read common trace");
        assert_eq!(self_trace, common_trace, "trace mismatch for {test}");
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_common_bus_send_recv_backpressure_matches_all_layouts() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_bus_send_recv_backpressure_matches_all_layouts: `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("bus_send_recv_inputs");
    let sv = inputs.join("HandshakeLoop.sv");
    let tb = inputs.join("bus_send_recv.harc");
    let self_out = fresh_dir("bus_send_recv_self");
    let v1_out = fresh_dir("bus_send_recv_v1");
    let common_out = fresh_dir("bus_send_recv_common");
    std::fs::write(
        &sv,
        r#"module HandshakeLoop(
  input logic clk,
  input logic rst,
  input logic [7:0] tx_data,
  input logic tx_valid,
  output logic tx_ready,
  output logic [7:0] rx_data,
  output logic rx_valid,
  input logic rx_ready,
  output logic [7:0] accept_count
);
  logic [2:0] ready_delay;
  assign tx_ready = ready_delay >= 2 && !rx_valid;
  always_ff @(posedge clk) begin
    if (rst) begin
      ready_delay <= 0;
      rx_data <= 0;
      rx_valid <= 0;
      accept_count <= 0;
    end else begin
      if (ready_delay < 2) ready_delay <= ready_delay + 1;
      if (rx_valid && rx_ready) rx_valid <= 0;
      if (tx_valid && tx_ready) begin
        rx_data <= tx_data;
        rx_valid <= 1;
        accept_count <= accept_count + 1;
      end
    end
  end
endmodule
"#,
    )
    .expect("write send/recv DUT");
    std::fs::write(
        &tb,
        r#"bus PingBus
    handshake_channel tx: send kind: valid_ready
        data: uint<8>
    end handshake_channel tx
    handshake_channel rx: receive kind: valid_ready
        data: uint<8>
    end handshake_channel rx
end bus PingBus

testbench PingTb
    dut : HandshakeLoop
end testbench PingTb

impl PingTest for PingTb
    let p : PingBus = bind dut with {
        tx.data: "tx_data", tx.valid: "tx_valid", tx.ready: "tx_ready",
        rx.data: "rx_data", rx.valid: "rx_valid", rx.ready: "rx_ready"
    }
    clock clk = 10ns
    run
        dut.rst = 1
        p.tx.valid = 0
        p.rx.ready = 0
        wait 2 cycles
        dut.rst = 0
        p.tx.send(7)
        let value = p.rx.recv()
        assert value == 7 else fail("recv value ${value}")
        assert dut.accept_count == 1 else fail("request accepted ${dut.accept_count} times")
        log(info, "PASS: bus.send/recv backpressure and single accept")
    end run
end impl PingTest
"#,
    )
    .expect("write send/recv HARC fixture");

    build_self_contained(&sv, &tb, "HandshakeLoop", &self_out);
    build_v1(&sv, &tb, "HandshakeLoop", &v1_out);
    build_common_suite(&sv, &tb, "HandshakeLoop", &common_out);
    for (layout, outdir, tag) in [
        ("self-contained", &self_out, "self"),
        ("v1", &v1_out, "v1"),
        ("common", &common_out, "common"),
    ] {
        let run = Command::new(outdir.join("obj_dir/VHandshakeLoop"))
            .args(["--test", "PingTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "6161")
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} bus send/recv: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(
            run.status.success(),
            "{layout} bus send/recv failed:\n{log}"
        );
        assert!(
            log.contains("PASS: bus.send/recv backpressure and single accept"),
            "{layout}:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).unwrap(),
        std::fs::read(common_out.join("common.jsonl")).unwrap(),
        "bus send/recv common trace mismatch"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_blocking_tlm_timeout_is_bounded_and_diagnostic() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_blocking_tlm_timeout_is_bounded_and_diagnostic: `verilator` not found"
        );
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("blocking_tlm_timeout_inputs");
    let sv = root.join("tests/dut/TlmStallMemory.sv");
    let tb = inputs.join("blocking_tlm_timeout.harc");
    let self_out = fresh_dir("blocking_tlm_timeout_self");
    let v1_out = fresh_dir("blocking_tlm_timeout_v1");
    let common_out = fresh_dir("blocking_tlm_timeout_common");
    std::fs::write(
        &tb,
        r#"bus TlmStallBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmStallBus

testbench TlmStallTimeoutTb
    dut : TlmStallMemory
end testbench TlmStallTimeoutTb

impl TimeoutBuildPass for TlmStallTimeoutTb
    let mem : TlmStallBus = bind dut
    run
        dut.rst = 1
        wait 1 cycle
    end run
end impl TimeoutBuildPass

impl TlmStallTimeoutTest for TlmStallTimeoutTb
    let mem : TlmStallBus = bind dut
    run
        dut.rst = 1
        dut.mem_read_req_valid = 0
        dut.mem_read_rsp_ready = 0
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
        let value = mem.read(5)
        log(info, "timeout returned ${value}")
    end run
end impl TlmStallTimeoutTest
"#,
    )
    .expect("write blocking timeout fixture");

    build_self_contained(&sv, &tb, "TlmStallMemory", &self_out);
    build_v1(&sv, &tb, "TlmStallMemory", &v1_out);
    build_common_suite(&sv, &tb, "TlmStallMemory", &common_out);
    for (layout, outdir) in [
        ("self-contained", &self_out),
        ("v1", &v1_out),
        ("common", &common_out),
    ] {
        let run = Command::new(outdir.join("obj_dir/VTlmStallMemory"))
            .args(["--test", "TlmStallTimeoutTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "6262")
            .env("HARC_SIM_LOG", outdir.join(format!("{layout}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run {layout} blocking timeout: {error}"));
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(!run.status.success(), "{layout} timeout passed:\n{log}");
        assert!(
            log.contains("TLM mem.read request timed out after 16 cycles"),
            "{layout}:\n{log}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_rejects_invalid_clock_metadata_before_verilator_or_publication() {
    let inputs = fresh_dir("preflight_inputs");
    let outdir = fresh_dir("preflight_output");
    let empty_path = fresh_dir("empty_path");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("tbir_common_bad.harc");
    std::fs::write(
        &sv,
        "module TbirCommonReg(\n  output logic clk\n);\nendmodule\n",
    )
    .expect("write DUT fixture");
    std::fs::write(
        &tb,
        "test InvalidClock\n    let dut : TbirCommonReg\n    clock clk = 10ns\n    run\n        wait 1 cycle\n    end run\nend test InvalidClock\n",
    )
    .expect("write unsupported HARC fixture");

    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(&sv)
        .arg(&tb)
        .args(["--top", "TbirCommonReg", "--codegen", "tbir"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .arg("--outdir")
        .arg(&outdir)
        .env("PATH", &empty_path)
        .output()
        .expect("run unsupported common layout");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "invalid clock suite passed unexpectedly"
    );
    assert!(
        log.contains("dut.clk") && log.contains("direction"),
        "wrong diagnostic:\n{log}"
    );
    assert!(
        !log.contains("running: verilator"),
        "Verilator was started:\n{log}"
    );
    assert!(
        std::fs::read_dir(&outdir)
            .expect("read output directory")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with("tbir_common_bad__")),
        "unsupported suite published common artifacts"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(outdir);
    let _ = std::fs::remove_dir_all(empty_path);
}

#[test]
fn tbir_common_bound_transactor_uses_one_body_with_per_capsule_adapters() {
    let inputs = fresh_dir("bound_bus_preflight_inputs");
    let common_out = fresh_dir("bound_bus_common_output");
    let self_out = fresh_dir("bound_bus_self_contained_output");
    let sv = inputs.join("BoundBusTop.sv");
    let tb = inputs.join("bound_bus_conflict.harc");
    std::fs::write(
        &sv,
        r#"module BoundBusTop(
  input logic clk,
  input logic [7:0] first_data,
  input logic first_valid,
  output logic first_ready,
  input logic first_read_req_valid,
  input logic [7:0] first_read_addr,
  output logic first_read_req_ready,
  output logic first_read_rsp_valid,
  output logic [7:0] first_read_rsp_data,
  input logic first_read_rsp_ready,
  input logic [7:0] second_data,
  input logic second_valid,
  output logic second_ready,
  input logic second_read_req_valid,
  input logic [7:0] second_read_addr,
  output logic second_read_req_ready,
  output logic second_read_rsp_valid,
  output logic [7:0] second_read_rsp_data,
  input logic second_read_rsp_ready
);
  assign first_ready = 1'b1;
  assign second_ready = 1'b1;
  assign first_read_req_ready = 1'b1;
  assign first_read_rsp_valid = 1'b1;
  assign first_read_rsp_data = first_read_addr + 1'b1;
  assign second_read_req_ready = 1'b1;
  assign second_read_rsp_valid = 1'b1;
  assign second_read_rsp_data = second_read_addr + 1'b1;
endmodule
"#,
    )
    .expect("write DUT fixture");
    std::fs::write(
        &tb,
        r#"bus TinyBus
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

        function read_back(addr: uint<8>) -> uint<8>
            let result = bus.read(addr)
            return result
        end read_back
    end when
end transactor TinyDriver

transactor TinyEventDriver bound to TinyBus
    when active
        request : in event<uint<8>>

        function drive_event(value: uint<8>)
            bus.req.data = value
            bus.req.valid = 1
            wait 1 cycle
            bus.req.valid = 0
        end drive_event

        on request(value)
            drive_event(value)
        end on
    end when
end transactor TinyEventDriver

testbench BusTbA
    dut : BoundBusTop
    hook_count : uint<8> default 0
end testbench BusTbA
impl BoundBusA for BusTbA
    let first : TinyBus = bind dut with {
        req.data: "first_data", req.valid: "first_valid", req.ready: "first_ready",
        read.req_valid: "first_read_req_valid", read.addr: "first_read_addr",
        read.req_ready: "first_read_req_ready", read.rsp_valid: "first_read_rsp_valid",
        read.rsp_data: "first_read_rsp_data", read.rsp_ready: "first_read_rsp_ready"
    }
    let driver_a : TinyDriver active = bind first
    let event_driver_a : TinyEventDriver active = bind first
    clock clk = 10ns
    on driver_a.drive post
        hook_count = hook_count + 1
        let hook_read = driver_a.read_back(value)
        assert hook_read == value + 1 else fail("first hook TLM remap ${hook_read}")
        log(info, "BOUND_BUS_HOOK=A:${value}")
    end on
    run
        dut.second_data = 0xa5
        driver_a.drive(3)
        let read_value = driver_a.read_back(9)
        assert dut.first_data == 3 else fail("first remap")
        assert read_value == 10 else fail("first TLM remap ${read_value}")
        assert driver_a.calls == 1 else fail("first driver calls ${driver_a.calls}")
        assert hook_count == 1 else fail("first hook count ${hook_count}")
        emit event_driver_a.request(13)
        assert dut.first_data == 13 else fail("first component-call adapter")
        assert dut.second_data == 0xa5 else fail("first component-call poisoned second port")
        log(info, "BOUND_BUS_RESULT=A")
    end run
end impl BoundBusA

testbench BusTbB
    dut : BoundBusTop
    hook_count : uint<8> default 0
end testbench BusTbB
impl BoundBusB for BusTbB
    let second : TinyBus = bind dut with {
        req.data: "second_data", req.valid: "second_valid", req.ready: "second_ready",
        read.req_valid: "second_read_req_valid", read.addr: "second_read_addr",
        read.req_ready: "second_read_req_ready", read.rsp_valid: "second_read_rsp_valid",
        read.rsp_data: "second_read_rsp_data", read.rsp_ready: "second_read_rsp_ready"
    }
    let driver_b : TinyDriver active = bind second
    let event_driver_b : TinyEventDriver active = bind second
    clock clk = 10ns
    on driver_b.drive post
        hook_count = hook_count + 1
        let hook_read = driver_b.read_back(value)
        assert hook_read == value + 1 else fail("second hook TLM remap ${hook_read}")
        log(info, "BOUND_BUS_HOOK=B:${value}")
    end on
    run
        dut.first_data = 0x5a
        driver_b.drive(7)
        let read_value = driver_b.read_back(11)
        assert dut.second_data == 7 else fail("second remap")
        assert read_value == 12 else fail("second TLM remap ${read_value}")
        assert driver_b.calls == 1 else fail("second driver calls ${driver_b.calls}")
        assert hook_count == 1 else fail("second hook count ${hook_count}")
        emit event_driver_b.request(29)
        assert dut.second_data == 29 else fail("second component-call adapter")
        assert dut.first_data == 0x5a else fail("second component-call poisoned first port")
        log(info, "BOUND_BUS_RESULT=B")
    end run
end impl BoundBusB
"#,
    )
    .expect("write bound-bus HARC fixture");

    build_self_contained(&sv, &tb, "BoundBusTop", &self_out);
    let self_cpp = std::fs::read_to_string(self_out.join("bound_bus_conflict.cpp"))
        .expect("read self-contained bound-bus output");
    assert!(self_cpp.contains("dut->first_data"), "{self_cpp}");
    assert!(self_cpp.contains("dut->second_data"), "{self_cpp}");
    assert!(!self_cpp.contains("dut->first_req_data"), "{self_cpp}");
    assert!(!self_cpp.contains("dut->second_req_data"), "{self_cpp}");
    build_common_suite(&sv, &tb, "BoundBusTop", &common_out);
    let runtime = std::fs::read_to_string(common_out.join("bound_bus_conflict__runtime.cpp"))
        .expect("read common runtime");
    assert_eq!(runtime.matches("TinyDriver_drive(").count(), 1, "{runtime}");
    assert_eq!(
        runtime.matches("TinyDriver_drive_raw(").count(),
        2,
        "one declaration-use plus one definition is expected in the runtime: {runtime}"
    );
    assert_eq!(
        runtime.matches("TinyDriver_read_back(").count(),
        1,
        "{runtime}"
    );
    assert_eq!(
        runtime.matches("TinyEventDriver_drive_event(").count(),
        2,
        "one shared definition and one receiver-compatible handler call are expected: {runtime}"
    );
    assert!(!runtime.contains("first_data"), "{runtime}");
    assert!(!runtime.contains("second_data"), "{runtime}");
    let capsule_a =
        std::fs::read_to_string(common_out.join("bound_bus_conflict__test_BoundBusA.cpp"))
            .expect("read first common capsule");
    let capsule_b =
        std::fs::read_to_string(common_out.join("bound_bus_conflict__test_BoundBusB.cpp"))
            .expect("read second common capsule");
    assert!(capsule_a.contains("ctx.dut->first_data"), "{capsule_a}");
    assert!(
        !capsule_a.contains("ctx.dut->second_data = value"),
        "{capsule_a}"
    );
    assert!(capsule_b.contains("ctx.dut->second_data"), "{capsule_b}");
    assert!(
        !capsule_b.contains("ctx.dut->first_data = value"),
        "{capsule_b}"
    );

    for test in ["BoundBusA", "BoundBusB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VBoundBusTop"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5150")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let self_run = run(&self_out, &format!("self_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("BOUND_BUS_RESULT="), "{layout} {test}:\n{log}");
            assert!(log.contains("BOUND_BUS_HOOK="), "{layout} {test}:\n{log}");
        }
        assert_eq!(
            std::fs::read(self_out.join(format!("self_{test}.jsonl"))).unwrap(),
            std::fs::read(common_out.join(format!("common_{test}.jsonl"))).unwrap(),
            "trace mismatch for {test}"
        );
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(common_out);
    let _ = std::fs::remove_dir_all(self_out);
}

#[test]
fn tbir_canonical_testbench_bus_method_resolves_each_impl_remap() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_canonical_testbench_bus_method_resolves_each_impl_remap: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("tb_bus_method_inputs");
    let self_out = fresh_dir("tb_bus_method_self");
    let common_out = fresh_dir("tb_bus_method_common");
    let sv = inputs.join("BoundBusTop.sv");
    let tb = inputs.join("tb_bus_method.harc");
    std::fs::write(
        &sv,
        r#"module BoundBusTop(
  input logic clk,
  input logic [7:0] first_data,
  input logic first_valid,
  output logic first_ready,
  input logic [7:0] second_data,
  input logic second_valid,
  output logic second_ready
);
  assign first_ready = 1'b1;
  assign second_ready = 1'b1;
endmodule
"#,
    )
    .expect("write bound-bus DUT");
    std::fs::write(
        &tb,
        r#"bus TinyBus
    handshake_channel req: send kind: valid_ready
        data: uint<8>
    end handshake_channel req
end bus TinyBus

testbench SharedBusTb
    dut : BoundBusTop

    function drive(value: uint<8>)
        link.req.data = value
        link.req.valid = 1
        wait 1 cycle
        link.req.valid = 0
    end function drive

    function relay(value: uint<8>)
        drive(value)
    end function relay
end testbench SharedBusTb

impl SharedBusA for SharedBusTb
    let link : TinyBus = bind dut with {
        req.data: "first_data", req.valid: "first_valid", req.ready: "first_ready"
    }
    clock clk = 10ns
    run
        dut.second_data = 0xa5
        relay(3)
        assert dut.first_data == 3 else fail("first canonical remap")
        assert dut.second_data == 0xa5 else fail("first adapter touched poisoned second port")
        log(info, "TB_BUS_METHOD=A:${dut.first_data}:${dut.second_data}")
    end run
end impl SharedBusA

impl SharedBusB for SharedBusTb
    let link : TinyBus = bind dut with {
        req.data: "second_data", req.valid: "second_valid", req.ready: "second_ready"
    }
    clock clk = 10ns
    run
        dut.first_data = 0x5a
        relay(7)
        assert dut.second_data == 7 else fail("second canonical remap")
        assert dut.first_data == 0x5a else fail("second adapter touched poisoned first port")
        log(info, "TB_BUS_METHOD=B:${dut.first_data}:${dut.second_data}")
    end run
end impl SharedBusB
"#,
    )
    .expect("write canonical bound-bus fixture");

    build_self_contained(&sv, &tb, "BoundBusTop", &self_out);
    let cpp = std::fs::read_to_string(self_out.join("tb_bus_method.cpp"))
        .expect("read canonical bound-bus source");
    assert!(cpp.contains("dut->first_data"), "{cpp}");
    assert!(cpp.contains("dut->second_data"), "{cpp}");
    assert!(!cpp.contains("dut->link_req_data"), "{cpp}");
    build_common_suite(&sv, &tb, "BoundBusTop", &common_out);
    let runtime = std::fs::read_to_string(common_out.join("tb_bus_method__runtime.cpp"))
        .expect("read common bus runtime");
    let capsule_a = std::fs::read_to_string(common_out.join("tb_bus_method__test_SharedBusA.cpp"))
        .expect("read first common bus capsule");
    let capsule_b = std::fs::read_to_string(common_out.join("tb_bus_method__test_SharedBusB.cpp"))
        .expect("read second common bus capsule");
    assert_eq!(
        runtime.matches("void SharedBusTb_drive(").count(),
        1,
        "{runtime}"
    );
    assert_eq!(
        runtime.matches("void SharedBusTb_relay(").count(),
        1,
        "{runtime}"
    );
    assert!(!runtime.contains("first_data"), "{runtime}");
    assert!(!runtime.contains("second_data"), "{runtime}");
    assert!(capsule_a.contains("ctx.dut->first_data"), "{capsule_a}");
    assert!(
        !capsule_a.contains("ctx.dut->second_data = value"),
        "{capsule_a}"
    );
    assert!(capsule_b.contains("ctx.dut->second_data"), "{capsule_b}");
    assert!(
        !capsule_b.contains("ctx.dut->first_data = value"),
        "{capsule_b}"
    );

    for test in ["SharedBusA", "SharedBusB"] {
        let run = |outdir: &Path, tag: &str| {
            Command::new(outdir.join("obj_dir/VBoundBusTop"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "5150")
                .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
                .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {test} in {tag}: {error}"))
        };
        let self_run = run(&self_out, &format!("self_{test}"));
        let common_run = run(&common_out, &format!("common_{test}"));
        for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("TB_BUS_METHOD="), "{layout} {test}:\n{log}");
        }
        let self_trace = std::fs::read(self_out.join(format!("self_{test}.jsonl")))
            .expect("read self-contained trace");
        let common_trace = std::fs::read(common_out.join(format!("common_{test}.jsonl")))
            .expect("read common trace");
        assert_eq!(self_trace, common_trace, "trace mismatch for {test}");
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_regblock_frontdoor_matches_self_contained() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_regblock_frontdoor_matches_self_contained: `verilator` not found"
        );
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inputs = fresh_dir("regblock_frontdoor_inputs");
    let self_out = fresh_dir("regblock_frontdoor_self");
    let common_out = fresh_dir("regblock_frontdoor_common");
    let sv = inputs.join("AxiLiteRegs.sv");
    let tb = inputs.join("regblock_frontdoor.harc");
    std::fs::copy(root.join("tests/dut/AxiLiteRegs.sv"), &sv).expect("copy RAL DUT");
    let exact_bus = r#"bus BusAxiLite
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
end bus BusAxiLite"#;
    let source = std::fs::read_to_string(root.join("tests/fixtures/regblock_access_test.harc"))
        .expect("read RAL fixture")
        .replace("use BusAxiLite", exact_bus);
    std::fs::write(&tb, source).expect("write exact RAL fixture");

    build_self_contained(&sv, &tb, "AxiLiteRegs", &self_out);
    build_common_suite(&sv, &tb, "AxiLiteRegs", &common_out);
    let run = |outdir: &Path, tag: &str| {
        Command::new(outdir.join("obj_dir/VAxiLiteRegs"))
            .args(["--test", "RegblockAccessTest"])
            .current_dir(outdir)
            .env("HARC_SEED", "909")
            .env("HARC_TRACE", outdir.join(format!("{tag}.jsonl")))
            .env("HARC_SIM_LOG", outdir.join(format!("{tag}.log")))
            .output()
            .unwrap_or_else(|error| panic!("run {tag}: {error}"))
    };
    let self_run = run(&self_out, "self");
    let common_run = run(&common_out, "common");
    for (layout, result) in [("self-contained", &self_run), ("common", &common_run)] {
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.status.success(), "{layout} RAL run failed:\n{log}");
        assert!(
            log.contains("PASS: ro suppresses bus write"),
            "{layout}:\n{log}"
        );
    }
    assert_eq!(
        std::fs::read(self_out.join("self.jsonl")).expect("read self RAL trace"),
        std::fs::read(common_out.join("common.jsonl")).expect("read common RAL trace"),
        "RAL frontdoor trace mismatch"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_regblock_frontdoor_uses_per_capsule_bus_adapters_and_callbacks() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_regblock_frontdoor_uses_per_capsule_bus_adapters_and_callbacks: \
             `verilator` not found"
        );
        return;
    }
    let inputs = fresh_dir("regblock_remap_inputs");
    let self_out = fresh_dir("regblock_remap_self");
    let v1_out = fresh_dir("regblock_remap_v1");
    let common_out = fresh_dir("regblock_remap_common");
    let sv = inputs.join("RalRemapTop.sv");
    let tb = inputs.join("regblock_remap.harc");
    std::fs::write(
        &sv,
        r#"module RalRemapTop(
  input logic clk,
  input logic rst,
  input logic [63:0] first_data,
  input logic first_valid,
  output logic first_ready,
  input logic first_read_req_valid,
  input logic [7:0] first_read_addr,
  output logic first_read_req_ready,
  output logic first_read_rsp_valid,
  output logic [63:0] first_read_rsp_data,
  input logic first_read_rsp_ready,
  input logic [63:0] second_data,
  input logic second_valid,
  output logic second_ready,
  input logic second_read_req_valid,
  input logic [7:0] second_read_addr,
  output logic second_read_req_ready,
  output logic second_read_rsp_valid,
  output logic [63:0] second_read_rsp_data,
  input logic second_read_rsp_ready
);
  assign first_ready = 1'b1;
  assign second_ready = 1'b1;
  assign first_read_req_ready = 1'b1;
  assign first_read_rsp_valid = first_read_req_valid;
  assign first_read_rsp_data = 64'h8000000000001234;
  assign second_read_req_ready = 1'b1;
  assign second_read_rsp_valid = second_read_req_valid;
  assign second_read_rsp_data = 64'h8000000000005678;
endmodule
"#,
    )
    .expect("write RAL remap DUT");
    std::fs::write(
        &tb,
        r#"bus TinyRegBus
    handshake_channel req: send kind: valid_ready
        data: uint<64>
    end handshake_channel req
    tlm_method read(addr: uint<8>) -> uint<64>: blocking;
end bus TinyRegBus

transactor TinyRegHelper bound to TinyRegBus
    when active
        hookable write(addr: uint<8>, data: uint<64>)
            bus.req.data = data
            bus.req.valid = 1
            wait 1 cycle
            bus.req.valid = 0
        end write

        hookable read(addr: uint<8>) -> uint<64>
            let value = bus.read(addr)
            return value
        end read
    end when
end transactor TinyRegHelper

regblock TinyRegs via TinyRegHelper width 64
    register VALUE @ 5 access rw
end regblock TinyRegs

testbench RalRemapTb
    dut : RalRemapTop
    observed : uint<64> default 0
end testbench RalRemapTb

impl RalRemapFirst for RalRemapTb
    let link : TinyRegBus = bind dut with {
        req.data: "first_data", req.valid: "first_valid", req.ready: "first_ready",
        read.req_valid: "first_read_req_valid", read.addr: "first_read_addr",
        read.req_ready: "first_read_req_ready", read.rsp_valid: "first_read_rsp_valid",
        read.rsp_data: "first_read_rsp_data", read.rsp_ready: "first_read_rsp_ready"
    }
    let helper : TinyRegHelper active = bind link
    let regs : TinyRegs = bind helper
    clock clk = 10ns
    on regs.VALUE
        observed = data
    end on
    run
        let high : uint<64> = 1.zext<64>() << 63
        let callback_value : uint<64> = high + 4660
        let write_value : uint<64> = high + 1
        let poison : uint<64> = high + 43690
        dut.rst = 0
        dut.second_data = poison
        regs.record_write(5, callback_value)
        assert observed == callback_value else fail("first callback ${observed:016x}")
        regs.VALUE = write_value
        assert dut.first_data == write_value else fail("first RAL remap")
        assert dut.second_data == poison else fail("first poisoned second port")
        let value = regs.VALUE
        assert value == callback_value else fail("first RAL read ${value:016x}")
        log(info, "RAL_REMAP=FIRST:${observed:016x}:${value:016x}")
    end run
end impl RalRemapFirst

impl RalRemapSecond for RalRemapTb
    let link : TinyRegBus = bind dut with {
        req.data: "second_data", req.valid: "second_valid", req.ready: "second_ready",
        read.req_valid: "second_read_req_valid", read.addr: "second_read_addr",
        read.req_ready: "second_read_req_ready", read.rsp_valid: "second_read_rsp_valid",
        read.rsp_data: "second_read_rsp_data", read.rsp_ready: "second_read_rsp_ready"
    }
    let helper : TinyRegHelper active = bind link
    let regs : TinyRegs = bind helper
    clock clk = 10ns
    on regs.VALUE
        observed = data
    end on
    run
        let high : uint<64> = 1.zext<64>() << 63
        let callback_value : uint<64> = high + 22136
        let write_value : uint<64> = high + 2
        let poison : uint<64> = high + 21845
        dut.rst = 0
        dut.first_data = poison
        regs.record_write(5, callback_value)
        assert observed == callback_value else fail("second callback ${observed:016x}")
        regs.VALUE = write_value
        assert dut.second_data == write_value else fail("second RAL remap")
        assert dut.first_data == poison else fail("second poisoned first port")
        let value = regs.VALUE
        assert value == callback_value else fail("second RAL read ${value:016x}")
        log(info, "RAL_REMAP=SECOND:${observed:016x}:${value:016x}")
    end run
end impl RalRemapSecond
"#,
    )
    .expect("write divergent RAL fixture");

    build_self_contained(&sv, &tb, "RalRemapTop", &self_out);
    build_v1(&sv, &tb, "RalRemapTop", &v1_out);
    build_common_suite(&sv, &tb, "RalRemapTop", &common_out);
    let runtime = std::fs::read_to_string(common_out.join("regblock_remap__runtime.cpp"))
        .expect("read common RAL runtime");
    assert!(!runtime.contains("first_data"), "{runtime}");
    assert!(!runtime.contains("second_data"), "{runtime}");
    let first_capsule =
        std::fs::read_to_string(common_out.join("regblock_remap__test_RalRemapFirst.cpp"))
            .expect("read first RAL capsule");
    let second_capsule =
        std::fs::read_to_string(common_out.join("regblock_remap__test_RalRemapSecond.cpp"))
            .expect("read second RAL capsule");
    assert!(
        first_capsule.contains("ctx.dut->first_data"),
        "{first_capsule}"
    );
    assert!(
        !first_capsule.contains("ctx.dut->second_data = data"),
        "{first_capsule}"
    );
    assert!(
        second_capsule.contains("ctx.dut->second_data"),
        "{second_capsule}"
    );
    assert!(
        !second_capsule.contains("ctx.dut->first_data = data"),
        "{second_capsule}"
    );

    for test in ["RalRemapFirst", "RalRemapSecond"] {
        let mut traces = Vec::new();
        for (layout, outdir) in [
            ("self", &self_out),
            ("v1", &v1_out),
            ("common", &common_out),
        ] {
            let trace = outdir.join(format!("{layout}_{test}.jsonl"));
            let run = Command::new(outdir.join("obj_dir/VRalRemapTop"))
                .args(["--test", test])
                .current_dir(outdir)
                .env("HARC_SEED", "909")
                .env("HARC_TRACE", &trace)
                .env("HARC_SIM_LOG", outdir.join(format!("{layout}_{test}.log")))
                .output()
                .unwrap_or_else(|error| panic!("run {layout} {test}: {error}"));
            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
            assert!(run.status.success(), "{layout} {test} failed:\n{log}");
            assert!(log.contains("RAL_REMAP="), "{layout} {test}:\n{log}");
            traces.push(std::fs::read(trace).expect("read RAL trace"));
        }
        assert_eq!(traces[0], traces[1], "v1 trace mismatch for {test}");
        assert_eq!(traces[0], traces[2], "common trace mismatch for {test}");
    }

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(self_out);
    let _ = std::fs::remove_dir_all(v1_out);
    let _ = std::fs::remove_dir_all(common_out);
}

#[test]
fn tbir_common_rejects_wrong_tseq_arity_before_verilator_or_publication() {
    let inputs = fresh_dir("arity_preflight_inputs");
    let outdir = fresh_dir("arity_preflight_output");
    let empty_path = fresh_dir("arity_empty_path");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("wrong_tseq_arity.harc");
    std::fs::write(&sv, "module TbirCommonReg(input logic clk);\nendmodule\n")
        .expect("write DUT fixture");
    std::fs::write(
        &tb,
        r#"tseq Pair(a: uint<8>, b: uint<8>) -> TSeq<uint<8>>
    yield a
    yield b
end tseq Pair

test WrongArity
    let dut : TbirCommonReg
    clock clk = 10ns
    run
        let values = Pair(1)
    end run
end test WrongArity
"#,
    )
    .expect("write wrong-arity HARC fixture");

    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(&sv)
        .arg(&tb)
        .args(["--top", "TbirCommonReg", "--codegen", "tbir"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .arg("--outdir")
        .arg(&outdir)
        .env("PATH", &empty_path)
        .output()
        .expect("run wrong-arity common layout");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "wrong-arity suite passed");
    assert!(
        log.contains("tseq `Pair` takes 2 argument(s), call passes 1"),
        "wrong diagnostic:\n{log}"
    );
    assert!(
        !log.contains("running: verilator"),
        "Verilator was started:\n{log}"
    );
    assert!(
        std::fs::read_dir(&outdir)
            .expect("read output directory")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with("wrong_tseq_arity__")),
        "wrong-arity suite published common artifacts"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(outdir);
    let _ = std::fs::remove_dir_all(empty_path);
}

#[test]
fn tbir_common_rejects_reusable_callback_registration_before_verilator_or_publication() {
    let inputs = fresh_dir("callback_registration_preflight_inputs");
    let outdir = fresh_dir("callback_registration_preflight_output");
    let empty_path = fresh_dir("callback_registration_empty_path");
    let sv = inputs.join("TbirCommonReg.sv");
    let tb = inputs.join("callback_registration.harc");
    std::fs::write(&sv, DUT).expect("write DUT fixture");
    std::fs::write(
        &tb,
        r#"testbench RegistrationTb
    dut : TbirCommonReg
    function arm()
        on 1 cycles
            log(info, "nested registration")
        end on
    end function arm
end testbench RegistrationTb

impl RegistrationTest for RegistrationTb
    clock clk = 10ns
    run
        arm()
        wait 1 cycle
    end run
end impl RegistrationTest
"#,
    )
    .expect("write callback-registration fixture");

    let output = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(&sv)
        .arg(&tb)
        .args(["--top", "TbirCommonReg", "--codegen", "tbir"])
        .args(["--cpp-split", "tests", "--cpp-split-layout", "common"])
        .arg("--outdir")
        .arg(&outdir)
        .env("PATH", &empty_path)
        .output()
        .expect("run callback-registration common preflight");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "reusable callback registration passed preflight"
    );
    assert!(
        log.contains("outside a test's")
            && log.contains("registration installs a closure that outlives the")
            && log.contains("must run exactly once"),
        "{log}"
    );
    assert!(!log.contains("running: verilator"), "{log}");
    assert!(
        std::fs::read_dir(&outdir)
            .expect("read output directory")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with("callback_registration__")),
        "reusable callback registration published common artifacts"
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(outdir);
    let _ = std::fs::remove_dir_all(empty_path);
}

#[test]
fn tbir_common_abi_anchor_survives_optimized_objects_and_rejects_stale_links() {
    if !verilator_present() {
        eprintln!(
            "SKIP tbir_common_abi_anchor_survives_optimized_objects_and_rejects_stale_links: \
             `verilator` not found"
        );
        return;
    }

    let inputs = fresh_dir("abi_inputs");
    let outdir = fresh_dir("abi_output");
    let (sv, tb) = write_fixture(&inputs);
    let build = build_common_suite(&sv, &tb, "TbirCommonReg", &outdir);
    assert!(
        build.contains("-Os"),
        "ABI objects were not compiled through Verilator's optimized path"
    );

    let prefix = "tbir_common__";
    let abi_a = manifest_abi(&outdir, prefix);
    let anchor_a = format!("harc_suite_abi_{abi_a}");
    let obj_dir = outdir.join("obj_dir");
    let capsule_name = "tbir_common__test_Common17.o";
    let registry_name = "tbir_common__registry.o";
    let capsule = obj_dir.join(capsule_name);
    let registry = obj_dir.join(registry_name);
    for object in [&capsule, &registry] {
        assert!(object.is_file(), "missing object {}", object.display());
    }
    assert!(
        undefined_symbols(&capsule).contains(&anchor_a),
        "optimized capsule object did not retain `{anchor_a}`"
    );
    assert!(
        undefined_symbols(&registry).contains(&anchor_a),
        "optimized registry object did not retain `{anchor_a}`"
    );

    let stale_capsule = std::fs::read(&capsule).expect("save generation-A capsule object");
    let stale_registry = std::fs::read(&registry).expect("save generation-A registry object");
    let capsule_source = outdir.join("tbir_common__test_Common17.cpp");
    let registry_source = outdir.join("tbir_common__registry.cpp");
    let capsule_source_a =
        std::fs::read_to_string(&capsule_source).expect("read generation-A capsule source");
    let registry_source_a =
        std::fs::read_to_string(&registry_source).expect("read generation-A registry source");

    let abi_b = rebind_generated_abi_inputs(
        &outdir,
        prefix,
        &abi_a,
        &[
            "runtime_abi=deliberately-incompatible-layout".to_string(),
            "trace_mode=disabled".to_string(),
        ],
    );
    let anchor_b = format!("harc_suite_abi_{abi_b}");
    let capsule_source_b =
        std::fs::read_to_string(&capsule_source).expect("read generation-B capsule source");
    let registry_source_b =
        std::fs::read_to_string(&registry_source).expect("read generation-B registry source");
    assert_eq!(
        capsule_source_a,
        capsule_source_b.replace(&abi_b, &abi_a),
        "capsule generations must differ only in their ABI symbol"
    );
    assert_eq!(
        registry_source_a,
        registry_source_b.replace(&abi_b, &abi_a),
        "registry generations must differ only in their ABI symbol"
    );

    for filename in [
        "tbir_common__runtime.o",
        "tbir_common__test_Common17.o",
        "tbir_common__test_Common203.o",
        "tbir_common__registry.o",
    ] {
        std::fs::remove_file(obj_dir.join(filename)).expect("remove generation-A object");
    }
    let fresh = relink(&obj_dir, "TbirCommonReg");
    assert!(
        fresh.status.success(),
        "generation-B source set did not build and link:\n{}{}",
        String::from_utf8_lossy(&fresh.stdout),
        String::from_utf8_lossy(&fresh.stderr)
    );

    assert!(
        undefined_symbols(&capsule).contains(&anchor_b),
        "optimized generation-B capsule object did not retain `{anchor_b}`"
    );
    assert!(
        undefined_symbols(&registry).contains(&anchor_b),
        "optimized generation-B registry object did not retain `{anchor_b}`"
    );
    let fresh_capsule = std::fs::read(&capsule).expect("save generation-B capsule object");
    let fresh_registry = std::fs::read(&registry).expect("save generation-B registry object");

    std::fs::write(&capsule, &stale_capsule).expect("install stale capsule object");
    let stale_capsule_link = relink(&obj_dir, "TbirCommonReg");
    let stale_capsule_log = format!(
        "{}{}",
        String::from_utf8_lossy(&stale_capsule_link.stdout),
        String::from_utf8_lossy(&stale_capsule_link.stderr)
    );
    assert!(
        !stale_capsule_link.status.success(),
        "stale capsule linked unexpectedly:\n{stale_capsule_log}"
    );
    assert!(
        stale_capsule_log.contains(&anchor_a),
        "stale-capsule link failure did not name the old ABI anchor:\n{stale_capsule_log}"
    );

    std::fs::write(&capsule, &fresh_capsule).expect("restore fresh capsule object");
    let fresh = relink(&obj_dir, "TbirCommonReg");
    assert!(
        fresh.status.success(),
        "fresh object set did not relink:\n{}{}",
        String::from_utf8_lossy(&fresh.stdout),
        String::from_utf8_lossy(&fresh.stderr)
    );

    std::fs::write(&registry, &stale_registry).expect("install stale registry object");
    let stale_registry_link = relink(&obj_dir, "TbirCommonReg");
    let stale_registry_log = format!(
        "{}{}",
        String::from_utf8_lossy(&stale_registry_link.stdout),
        String::from_utf8_lossy(&stale_registry_link.stderr)
    );
    assert!(
        !stale_registry_link.status.success(),
        "stale registry linked unexpectedly:\n{stale_registry_log}"
    );
    assert!(
        stale_registry_log.contains(&anchor_a),
        "stale-registry link failure did not name the old ABI anchor:\n{stale_registry_log}"
    );

    std::fs::write(&registry, &fresh_registry).expect("restore fresh registry object");
    let fresh = relink(&obj_dir, "TbirCommonReg");
    assert!(
        fresh.status.success(),
        "fresh object set did not relink after registry check:\n{}{}",
        String::from_utf8_lossy(&fresh.stdout),
        String::from_utf8_lossy(&fresh.stderr)
    );

    let _ = std::fs::remove_dir_all(inputs);
    let _ = std::fs::remove_dir_all(outdir);
}
