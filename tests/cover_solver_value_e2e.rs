//! End-to-end regressions for coverpoint sampling and solver values that
//! only show up once the generated C++ is built and run:
//!
//! * `.zext<N>()` / `.resize<N>()` / `~` over hook parameters and their
//!   record fields sample the declared-width value (zero-extended from the
//!   source width, sign-preserving for a signed `.resize`, complemented
//!   within the operand width);
//! * coverpoints wider than 128 bits sample at full width (they used to
//!   observe the low 64 bits, and a signed one did not compile);
//! * `dist` on a signed field (directive and `[dist]` attribute) builds and
//!   samples signed values;
//! * a queued `randomize ... with` captures a scalar `let` snapshot at the
//!   call (spec §4.4).
//!
//! Each program asserts its own expectations in its `check`/`run` block, so a
//! wrong bin count or value fails the simulation rather than a text match.

use std::path::{Path, PathBuf};
use std::process::Command;

fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH"
    );
    present
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_cover_solver_value_e2e_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Simulate `source` against `tests/dut/top_counter.sv` and require a clean
/// pass. Returns stdout for extra checks.
fn simulate(name: &str, source: &str) -> Option<String> {
    if !verilator_present() {
        eprintln!("skipping {name}: verilator not found on PATH");
        return None;
    }
    let dir = scratch(name);
    let file = dir.join(format!("{name}.harc"));
    std::fs::write(&file, source).expect("write program");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("sim")
        .arg("--sv")
        .arg(root().join("tests/dut/top_counter.sv"))
        .arg(&file)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(dir.join("out"))
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("spawn harc sim");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED"),
        "{name} failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
    Some(stdout)
}

fn check(name: &str, source: &str) -> std::process::Output {
    let dir = scratch(name);
    let file = dir.join(format!("{name}.harc"));
    std::fs::write(&file, source).expect("write program");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("check")
        .arg(Path::new(&file))
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("spawn harc check");
    let _ = std::fs::remove_dir_all(&dir);
    output
}

#[test]
fn coverpoint_zext_zero_extends_from_the_hook_param_width() {
    simulate(
        "cov_zext_hook",
        r#"transaction Txn
    f : sint<8>
end transaction Txn

scoreboard Sink
    calls : uint<32> default 0
    hookable accept(t: Txn, v: sint<8>, u: uint<8>)
        calls = calls + 1
    end accept
end scoreboard Sink

covergroup Cov @(sink.accept(t, v, u) post)
    cp_param : cover v.zext<16>()
        bins
            ff = {0xFF}
            ffff = {0xFFFF}
        end bins
    cp_field : cover t.f.zext<16>()
        bins
            ff = {0xFF}
            ffff = {0xFFFF}
        end bins
    cp_sext_unsigned : cover u.sext<16>()
        bins
            neg_one = {-1}
            raw = {0xFF}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    sink : Sink
    cov : Cov
end testbench Tb

impl Test for Tb
    run
        let t : Txn
        t.f = -1
        let a : sint<8> = -1
        let b : uint<8> = 0xFF
        assert a.zext<16>() == 0xFF else fail("procedural zext reference")
        sink.accept(t, a, b)
        wait 1 cycle
    end run
    check
        assert cov.cp_param.ff == 1 else fail("sint<8> -1 .zext<16>() must sample 0xFF")
        assert cov.cp_param.ffff == 0 else fail("hook-param zext must not sample the host carrier")
        assert cov.cp_field.ff == 1 else fail("record-field zext must sample 0xFF")
        assert cov.cp_field.ffff == 0 else fail("record-field zext must not sample 0xFFFF")
        assert cov.cp_sext_unsigned.neg_one == 1 else fail("uint<8> 0xFF .sext<16>() must sample -1")
        assert cov.cp_sext_unsigned.raw == 0 else fail("uint<8> .sext<16>() must sign-extend")
    end check
end impl Test
"#,
    );
}

#[test]
fn coverpoint_resize_keeps_sint_hook_params_and_fields_signed() {
    simulate(
        "cov_resize_hook",
        r#"transaction Txn
    f : sint<8>
    g : sint<16>
end transaction Txn

scoreboard Sink
    calls : uint<32> default 0
    hookable accept(t: Txn, v: sint<8>)
        calls = calls + 1
    end accept
end scoreboard Sink

covergroup Cov @(sink.accept(t, v) post)
    cp_field_up : cover t.f.resize<16>()
        bins
            neg = {-1}
            raw = {0xFFFF}
        end bins
    cp_field_dn : cover t.g.resize<8>()
        bins
            neg = {-128}
            raw = {0x80}
        end bins
    cp_param_up : cover v.resize<16>()
        bins
            neg = {-1}
            raw = {0xFFFF}
        end bins
    cp_as_uint : cover v as uint<16>
        bins
            neg = {-1}
            raw = {0xFFFF}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    sink : Sink
    cov : Cov
end testbench Tb

impl Test for Tb
    run
        let t : Txn
        t.f = -1
        t.g = 0x0180
        let a : sint<8> = -1
        sink.accept(t, a)
        wait 1 cycle
    end run
    check
        assert cov.cp_field_up.neg == 1 else fail("sint<8> -1 .resize<16>() must sample -1")
        assert cov.cp_field_up.raw == 0 else fail("signed resize must not sample the raw pattern")
        assert cov.cp_field_dn.neg == 1 else fail("sint<16> 0x0180 .resize<8>() must sample -128")
        assert cov.cp_field_dn.raw == 0 else fail("signed narrowing resize must not sample 0x80")
        assert cov.cp_param_up.neg == 1 else fail("hook param .resize<16>() must sample -1")
        assert cov.cp_param_up.raw == 0 else fail("hook param resize must stay signed")
        assert cov.cp_as_uint.raw == 1 else fail("`as uint<16>` stays an unsigned 16-bit value")
        assert cov.cp_as_uint.neg == 0 else fail("`as uint<16>` must not become signed")
    end check
end impl Test
"#,
    );
}

#[test]
fn coverpoint_bitnot_complements_within_the_operand_width() {
    simulate(
        "cov_bitnot",
        r#"scoreboard Sink
    calls : uint<32> default 0
    hookable accept(u: uint<8>, s: sint<8>)
        calls = calls + 1
    end accept
end scoreboard Sink

covergroup Cov @(sink.accept(u, s) post)
    cp_not : cover ~u
        bins
            ff = {0xFF}
            big = [0x100..0xFFFFFFFF]
        end bins
    cp_snot : cover ~s
        bins
            minus_six = {-6}
            raw = {0xFA}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    sink : Sink
    cov : Cov
end testbench Tb

impl Test for Tb
    run
        let b : uint<8> = 0
        let c : sint<8> = 5
        sink.accept(b, c)
        wait 1 cycle
    end run
    check
        assert cov.cp_not.ff == 1 else fail("~uint<8> 0 must sample 0xFF")
        assert cov.cp_not.big == 0 else fail("~uint<8> must not sample the 64-bit complement")
        assert cov.cp_snot.minus_six == 1 else fail("~sint<8> 5 must sample -6")
        assert cov.cp_snot.raw == 0 else fail("~sint<8> must stay signed")
    end check
end impl Test
"#,
    );
}

#[test]
fn coverpoints_wider_than_128_bits_sample_at_full_width() {
    simulate(
        "cov_wide_sample",
        r#"scoreboard Sink
    calls : uint<32> default 0
    hookable accept(u: uint<200>, s: sint<200>, n: sint<8>)
        calls = calls + 1
    end accept
end scoreboard Sink

covergroup Cov @(sink.accept(u, s, n) post)
    cp_u : cover u
        bins
            one = {1}
            low = [0..100]
            big = [0x100..u]
        end bins
    cp_s : cover s
        bins
            neg_one = {-1}
            neg = [-10..-1]
            pos = [1..10]
            same_n = {n}
            below_n = [-100..n]
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    sink : Sink
    cov : Cov
end testbench Tb

impl Test for Tb
    run
        let u : uint<200> = 1
        u = (u << 150) + 1
        let s : sint<200> = 0
        s = s - 1
        let n : sint<8> = -1
        sink.accept(u, s, n)
        wait 1 cycle
    end run
    check
        assert cov.cp_u.one == 0 else fail("2^150+1 must not hit {1}")
        assert cov.cp_u.low == 0 else fail("2^150+1 must not hit [0..100]")
        assert cov.cp_u.big == 1 else fail("2^150+1 must hit [0x100..u]")
        assert cov.cp_s.neg_one == 1 else fail("sint<200> -1 must hit {-1}")
        assert cov.cp_s.neg == 1 else fail("sint<200> -1 must hit [-10..-1]")
        assert cov.cp_s.pos == 0 else fail("sint<200> -1 must not hit [1..10]")
        assert cov.cp_s.same_n == 1 else fail("sint<200> -1 must equal runtime sint<8> -1")
        assert cov.cp_s.below_n == 1 else fail("sint<200> -1 must lie in [-100..n]")
    end check
end impl Test
"#,
    );
}

#[test]
fn duplicate_coverpoint_and_bin_names_are_compile_errors() {
    let dup_point = check(
        "dup_point",
        r#"covergroup Cov @(posedge dut.clk)
    a : cover dut.count_out
        bins
            x = {1}
        end bins
    a : cover dut.count_out
        bins
            y = {2}
        end bins
end covergroup Cov
"#,
    );
    let stderr = String::from_utf8_lossy(&dup_point.stderr);
    assert!(
        !dup_point.status.success()
            && stderr.contains("duplicate coverpoint `a` in covergroup `Cov`"),
        "duplicate coverpoint must be rejected by `harc check`:\n{stderr}"
    );

    let dup_bin = check(
        "dup_bin",
        r#"covergroup Cov @(posedge dut.clk)
    a : cover dut.count_out
        bins
            x = {1}
            x = {2}
        end bins
end covergroup Cov
"#,
    );
    let stderr = String::from_utf8_lossy(&dup_bin.stderr);
    assert!(
        !dup_bin.status.success() && stderr.contains("duplicate bin `x` in coverpoint `a`"),
        "duplicate bin must be rejected by `harc check`:\n{stderr}"
    );

    // Same bin name in two different coverpoints is fine.
    let distinct = check(
        "distinct_scopes",
        r#"covergroup Cov @(posedge dut.clk)
    a : cover dut.count_out
        bins
            x = {1}
        end bins
    b : cover dut.count_out
        bins
            x = {2}
        end bins
end covergroup Cov
"#,
    );
    assert!(
        distinct.status.success(),
        "bin names are scoped to their coverpoint:\n{}",
        String::from_utf8_lossy(&distinct.stderr)
    );
}

#[test]
fn dist_on_signed_fields_builds_and_samples_signed_values() {
    simulate(
        "signed_dist",
        r#"transaction T
    s : sint<4>
end transaction T
transaction A
    s : sint<4> with [dist {-3 :/ 100}]
end transaction A
test SignedDist
    let dut : Top
    run
        let hits : uint<32> = 0
        let ahits : uint<32> = 0
        repeat 20
            let r : T
            randomize(r) with
                r.s dist { -2 :/ 100 }
            end randomize
            if r.s == -2
                hits = hits + 1
            end if
            let a : A
            randomize(a)
            if a.s == -3
                ahits = ahits + 1
            end if
        end repeat
        log(info, "hits=${hits} ahits=${ahits}")
        assert hits >= 15 else fail("dist directive on sint<4> did not prefer -2")
        assert ahits >= 15 else fail("[dist] attribute on sint<4> did not prefer -3")
    end run
end test SignedDist
"#,
    );
}

#[test]
fn queued_randomize_captures_scalar_let_snapshots_at_each_call() {
    simulate(
        "queued_let_snapshot",
        r#"transaction T
    u : uint<8>
    b : bool
end transaction T
test QueuedSnapshot
    let dut : Top
    run
        let snap : uint<8> = 7
        let flag : bool = true
        let r : T
        randomize(r) with
            r.u == snap
            r.b == flag
        end randomize
        assert r.u == 7 else fail("first queued randomize did not use snap=7")
        assert r.b else fail("first queued randomize did not use flag=true")
        snap = 42
        flag = false
        randomize(r) with
            r.u == snap
            r.b == flag
        end randomize
        assert r.u == 42 else fail("second queued randomize did not capture snap=42")
        assert !r.b else fail("second queued randomize did not capture flag=false")
        let live : uint<8> = dut.count_out
        randomize(r) with
            r.u == live
        end randomize
        assert r.u == live else fail("queued randomize did not capture the DUT snapshot")
    end run
end test QueuedSnapshot
"#,
    );
}

/// Snapshot capture into a solver vector wider than 64 bits (the record has
/// a `uint<100>` field): bool, unsigned and signed scalar locals, queued and
/// blocking, plus a method parameter as the snapshot.
#[test]
fn snapshot_capture_into_a_wide_solver_vector() {
    simulate(
        "wide_vector_snapshot",
        r#"transaction T
    b : bool
    u : uint<8>
    s : sint<16>
    w : uint<100>
end transaction T
scoreboard Gen
    last : uint<8> default 0
    hookable make(want: uint<8>)
        let r : T
        randomize(r) with
            r.u == want
        end randomize
        last = r.u
    end make
end scoreboard Gen
testbench Tb
    dut : Top
    g : Gen
end testbench Tb
impl WideSnapshot for Tb
    run
        let flag : bool = true
        let lo : uint<8> = 10
        let neg : sint<16> = -300
        let r : T
        randomize(r) with
            r.b == flag
            r.u == lo
            r.s == neg
        end randomize
        assert r.b else fail("queued bool snapshot")
        assert r.u == 10 else fail("queued unsigned snapshot")
        assert r.s == -300 else fail("queued signed snapshot")
        flag = false
        neg = -7
        blocking randomize(r) with
            r.b == flag
            r.s == neg
        end randomize
        assert !r.b else fail("blocking bool snapshot")
        assert r.s == -7 else fail("blocking signed snapshot")
        g.make(33)
        assert g.last == 33 else fail("queued parameter snapshot")
        g.make(44)
        assert g.last == 44 else fail("queued parameter snapshot, second call")
    end run
end impl WideSnapshot
"#,
    );
}

/// spec §4.4: scalar fields of a local record `let` are snapshots — a queued
/// randomize reads them when the call executes (record wider than 64 bits).
#[test]
fn queued_randomize_captures_record_let_fields_at_each_call() {
    simulate(
        "record_let_snapshot",
        r#"transaction T
    b : bool
    u : uint<8>
    s : sint<16>
    w : uint<100>
end transaction T
test RecSnap
    let dut : Top
    run
        let other : T
        other.b = true
        other.u = 9
        other.s = -300
        let r : T
        randomize(r) with
            r.b == other.b
            r.u == other.u
            r.s == other.s
        end randomize
        assert r.b else fail("queued record bool snapshot")
        assert r.u == 9 else fail("queued record unsigned snapshot")
        assert r.s == -300 else fail("queued record signed snapshot")
        other.b = false
        other.u = 200
        other.s = -5
        randomize(r) with
            r.b == other.b
            r.u == other.u
            r.s == other.s
        end randomize
        assert !r.b else fail("second call must see other.b=false")
        assert r.u == 200 else fail("second call must see other.u=200")
        assert r.s == -5 else fail("second call must see other.s=-5")
        other.b = true
        randomize(r) with
            other.b
            r.u > other.u
        end randomize
        assert r.u > 200 else fail("record flag and ordering snapshot")
    end run
end test RecSnap
"#,
    );
}

/// Width-method and `~` samples at the carrier boundaries (63/64/65/128/129
/// bits) on signed hook parameters.
#[test]
fn coverpoint_width_methods_at_carrier_boundaries() {
    simulate(
        "cov_width_boundaries",
        r#"scoreboard Sink
    calls : uint<32> default 0
    hookable accept(a: sint<64>, b: sint<63>, c: sint<128>, d: sint<129>, e: sint<65>, r65: uint<65>, r129: uint<129>, s128: sint<128>, s129: sint<129>, u63: uint<63>)
        calls = calls + 1
    end accept
end scoreboard Sink

covergroup Cov @(sink.accept(a, b, c, d, e, r65, r129, s128, s129, u63) post)
    z64_65 : cover a.zext<65>()
        bins
            ok = {0xFFFFFFFFFFFFFFFF}
        end bins
    z63_64 : cover b.zext<64>()
        bins
            ok = {0x7FFFFFFFFFFFFFFF}
        end bins
    z128_129 : cover c.zext<129>()
        bins
            ok = {r129}
            neg = {-1}
        end bins
    r64_65 : cover a.resize<65>()
        bins
            ok = {-1}
            raw = {r65}
        end bins
    r65_64 : cover e.resize<64>()
        bins
            ok = {-1}
        end bins
    r129_128 : cover d.resize<128>()
        bins
            ok = {-1}
            okr = {s128}
        end bins
    r128_129 : cover c.resize<129>()
        bins
            ok = {-1}
            okr = {s129}
        end bins
    r64_200 : cover a.resize<200>()
        bins
            ok = {-1}
        end bins
    s129_id : cover d
        bins
            ok = {-1}
            neg = [-5..-1]
            okr = {s129}
        end bins
    n63 : cover ~u63
        bins
            ok = {0x7FFFFFFFFFFFFFFF}
        end bins
    sn63 : cover ~b
        bins
            zero = {0}
        end bins
end covergroup Cov

testbench Tb
    dut : Top
    sink : Sink
    cov : Cov
end testbench Tb

impl Test for Tb
    run
        let a : sint<64> = -1
        let b : sint<63> = -1
        let c : sint<128> = -1
        let d : sint<129> = -1
        let e : sint<65> = -1
        let r65 : uint<65> = 0xFFFFFFFFFFFFFFFF
        let r129 : uint<129> = 1
        r129 = (r129 << 128) - 1
        let s128 : sint<128> = -1
        let s129 : sint<129> = -1
        let u63 : uint<63> = 0
        sink.accept(a, b, c, d, e, r65, r129, s128, s129, u63)
        wait 1 cycle
    end run
    check
        assert cov.z64_65.ok == 1 else fail("sint<64> -1 .zext<65>() is 2^64-1")
        assert cov.z63_64.ok == 1 else fail("sint<63> -1 .zext<64>() is 2^63-1")
        assert cov.z128_129.ok == 1 else fail("sint<128> -1 .zext<129>() is 2^128-1")
        assert cov.z128_129.neg == 0 else fail("zext must not sign-extend")
        assert cov.r64_65.ok == 1 else fail("sint<64> -1 .resize<65>() is -1")
        assert cov.r64_65.raw == 0 else fail("signed resize must not zero-extend")
        assert cov.r65_64.ok == 1 else fail("sint<65> -1 .resize<64>() is -1")
        assert cov.r129_128.ok == 1 else fail("sint<129> -1 .resize<128>() is -1")
        assert cov.r129_128.okr == 1 else fail("sint<129> -1 .resize<128>() equals sint<128> -1")
        assert cov.r128_129.ok == 1 else fail("sint<128> -1 .resize<129>() is -1")
        assert cov.r128_129.okr == 1 else fail("sint<128> -1 .resize<129>() equals sint<129> -1")
        assert cov.r64_200.ok == 1 else fail("sint<64> -1 .resize<200>() is -1")
        assert cov.s129_id.ok == 1 else fail("sint<129> -1 hits {-1}")
        assert cov.s129_id.neg == 1 else fail("sint<129> -1 hits [-5..-1]")
        assert cov.s129_id.okr == 1 else fail("sint<129> -1 equals runtime sint<129> -1")
        assert cov.n63.ok == 1 else fail("~uint<63> 0 is 2^63-1")
        assert cov.sn63.zero == 1 else fail("~sint<63> -1 is 0")
    end check
end impl Test
"#,
    );
}
