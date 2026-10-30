//! End-to-end regressions for R16 hunt-1 value-semantics defects (TB-IR).
//!
//! Each assertion below failed on the pre-fix compiler:
//!
//! * runtime `<<` / `>>` by a count >= 64 on a <= 64-bit value was a bare C++
//!   shift (UB; arm64 gave `1 << 64 == 1` and `-8 >> 64 == -8`);
//! * a narrow `<<` kept bits past its left operand's declared width W
//!   (`uint<4>(9) << 8 == 2304`; the result keeps W bits, so it is 0);
//! * `~` on a `sint<N>` (N < 64) produced the zero-extended pattern
//!   (`~sint<8>(5) == 250`);
//! * `${x}` of a `uint<64>` at or above 2^63 printed a negative number, and
//!   `${s:x}` of a negative `sint<16>` printed sixteen hex digits;
//! * record `==` compared raw carriers, so equal `sint<70>` / `sint<300>`
//!   fields with different padding compared unequal;
//! * `x as sint<8>` / `s as uint<16>` relabelled the 64-bit carrier instead of
//!   producing the 8/16-bit value (`uint<8>(0xFC) as sint<8> == 252`,
//!   `sint<8>(-1) as uint<16> == -1`);
//! * a width-erased `0 - i` stored into a `sint<70>` zero-extended from bit
//!   63 (`2^64 - 4`, not -4);
//! * runtime `/` and `%` by zero, and signed MIN / -1, were silent (wide
//!   helpers: `x / 0 = 0`, `x % 0 = x`) or C++ UB (<= 64 bits).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn verilator_available() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // A skipped end-to-end test reports `ok`; CI sets this so a missing
    // Verilator cannot silently turn these into passes.
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH: this \
         end-to-end test would have skipped itself and reported success"
    );
    present
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harc_r16_values_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    dir
}

fn sim(dir: &Path, name: &str, src: &str) -> Output {
    let source = dir.join(format!("{name}.harc"));
    std::fs::write(&source, src).expect("write fixture");
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--outdir"])
        .arg(dir.join(format!("out_{name}")))
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run harc sim")
}

const VALUES: &str = r#"
struct P
    s : sint<70>
end struct P
struct Q
    t : sint<300>
end struct Q
struct V
    v : Vec<sint<8>, 2>
end struct V

test Values
    let dut : Top
    run
        // Shift saturation on the 64-bit carrier tier.
        let n : uint<8> = 64
        let n200 : uint<8> = 200
        let one : uint<64> = 1
        let s64 : sint<64> = 0 - 8
        let p64 : sint<64> = 8
        let top : uint<64> = 0x8000000000000000
        let r1 : uint<64> = one << n
        let r2 : sint<64> = s64 >> n
        let r3 : sint<64> = p64 >> n200
        let r4 : uint<64> = top >> n
        assert r1 == 0 else fail("uint<64> 1 << 64 = ${r1}")
        assert r2 == -1 else fail("sint<64> -8 >> 64 = ${r2}")
        assert r3 == 0 else fail("sint<64> 8 >> 200 = ${r3}")
        assert r4 == 0 else fail("uint<64> 2^63 >> 64 = ${r4}")
        let s8 : sint<8> = 0 - 8
        let r5 : sint<8> = s8 >> n
        assert r5 == -1 else fail("sint<8> -8 >> 64 = ${r5}")
        // A shift keeps its left operand's declared width W: `<<` drops the
        // bits shifted past W and a count >= W shifts everything out.
        let h : uint<4> = 9
        let h1 : uint<4> = h << 1
        assert h1 == 2 else fail("uint<4> 9 << 1 = ${h1}")
        let h8 = h << 8
        assert h8 == 0 else fail("uint<4> 9 << 8 = ${h8}")
        let n3 : uint<8> = 3
        let hv = h << n3
        assert hv == 8 else fail("uint<4> 9 << 3 = ${hv}")
        let hand = (h & h) << 8
        assert hand == 0 else fail("(uint<4> & uint<4>) << 8 = ${hand}")
        let hnot = (~h) << 8
        assert hnot == 0 else fail("(~uint<4>) << 8 = ${hnot}")
        let hnest = (h << 1) << 1
        assert hnest == 4 else fail("(uint<4> 9 << 1) << 1 = ${hnest}")
        let bcd : uint<12> = (h.zext<12>() << 8) | 3
        assert bcd == 0x903 else fail("uint<4>.zext<12>() 9 << 8 = ${bcd}")
        let sone : sint<8> = 1
        let n7 : uint<8> = 7
        let smin = sone << n7
        assert smin == -128 else fail("sint<8> 1 << 7 = ${smin}")
        let sneg : sint<8> = 0 - 128
        let n9 : uint<8> = 9
        let sfill = sneg >> n9
        assert sfill == -1 else fail("sint<8> -128 >> 9 = ${sfill}")
        let u8v : uint<8> = 0x80
        let ushr = u8v >> n9
        assert ushr == 0 else fail("uint<8> 0x80 >> 9 = ${ushr}")
        // A width-erased value shifts in its 64-bit carrier.
        let wide1 = 1 << 40
        assert wide1 == 0x10000000000 else fail("1 << 40")
        // The 65+ tiers are unchanged: they saturate at the declared width.
        let w65 : uint<65> = 1
        let n65 : uint<8> = 65
        let r6 : uint<65> = w65 << n65
        assert r6 == 0 else fail("uint<65> 1 << 65")

        // `~` on a narrow signed value is the signed complement.
        let b8 : sint<8> = 5
        let nb8 : sint<8> = ~b8
        assert nb8 == -6 else fail("~sint<8>(5) = ${nb8}")
        assert nb8 < 0 else fail("~sint<8>(5) < 0")
        let z1 : sint<1> = 0
        let nz1 = ~z1
        assert nz1 < 0 else fail("~sint<1>(0) < 0")
        let z63 : sint<63> = 0
        let nz63 = ~z63
        assert nz63 == -1 else fail("~sint<63>(0) = ${nz63}")

        // Record equality ignores carrier padding.
        let m : sint<4> = 0 - 1
        let a : P
        let b : P
        a.s = m.sext<70>()
        b.s = 0 - 1
        assert a == b else fail("sint<70> records with equal fields compare unequal")
        let c : Q
        let d : Q
        c.t = m.sext<300>()
        d.t = 0 - 1
        assert c == d else fail("sint<300> records with equal fields compare unequal")
        let e : V
        let f : V
        e.v[0] = m.sext<8>()
        f.v[0] = 0 - 1
        assert e == f else fail("Vec<sint<8>> records with equal lanes compare unequal")

        // `as` produces the target-width value.
        let uf : uint<8> = 0xFC
        let cs : sint<8> = uf as sint<8>
        assert cs == -4 else fail("uint<8>(0xFC) as sint<8> = ${cs}")
        assert cs < 0 else fail("uint<8>(0xFC) as sint<8> < 0")
        let ss : sint<8> = 0 - 1
        let su : uint<16> = ss as uint<16>
        assert su == 0xFFFF else fail("sint<8>(-1) as uint<16> = ${su}")
        let wide : uint<16> = 0x1FC
        let narrow : sint<8> = wide as sint<8>
        assert narrow == -4 else fail("uint<16>(0x1FC) as sint<8> = ${narrow}")
        let sw : sint<16> = ss as sint<16>
        assert sw == -1 else fail("sint<8>(-1) as sint<16> = ${sw}")

        // A width-erased 64-bit value sign-extends into a wide signed slot.
        let g : P
        for i in 4 .. 4
            let zz : sint<70> = 0 - i
            assert zz == (0 - 4) else fail("sint<70> local = 0 - i")
            assert zz < 0 else fail("sint<70> local = 0 - i < 0")
            g.s = 0 - i
        end for
        assert g.s == (0 - 4) else fail("sint<70> field = 0 - i")

        // Printing.
        let umax : uint<64> = 0xFFFFFFFFFFFFFFFF
        let neg16 : sint<16> = 0 - 1
        log(info, "PRINT umax=${umax} top=${top} hex=${umax:x} neg16=${neg16} neg16x=${neg16:x} neg16pad=${neg16:08x} neg16o=${neg16:o}")
        log(info, "VALUES_OK")
    end run
end test Values
"#;

#[test]
fn r16_runtime_value_semantics() {
    if !verilator_available() {
        return;
    }
    let dir = fresh_dir("values");
    let output = sim(&dir, "values", VALUES);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED"),
        "value-semantics fixture failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("VALUES_OK"), "missing marker:\n{stdout}");
    assert!(
        stdout.contains(
            "PRINT umax=18446744073709551615 top=9223372036854775808 hex=ffffffffffffffff \
             neg16=-1 neg16x=ffff neg16pad=0000ffff neg16o=177777"
        ),
        "interpolation printed the wrong text:\n{stdout}"
    );
}

/// One fault per test: after the first runtime arithmetic FATAL a run is
/// already failing and later faults are not re-reported, so each tier is
/// exercised in its own test of the same source (run with `--test`, which
/// reuses the Verilator build).
const DIV_FAULTS: &str = r#"
test NoFault
    let dut : Top
    run
        let a8 : uint<8> = 200
        let ok8 = a8 / 7
        assert ok8 == 28 else fail("200 / 7")
        // A width-erased dividend divides in the 64-bit carrier: -128 / -1
        // is 128 there, not a sint<8> overflow.
        let x : sint<8> = 0 - 100
        let y : sint<8> = 0 - 28
        let m1 : sint<8> = 0 - 1
        let q1 = (x + y) / m1
        assert q1 == 128 else fail("(x + y) / m1 = ${q1}")
        let x2 : sint<8> = 0 - 128
        let q2 : sint<16> = (x2 - 0) / m1
        assert q2 == 128 else fail("(x2 - 0) / m1 = ${q2}")
        let r2 = (x2 - 0) % m1
        assert r2 == 0 else fail("(x2 - 0) % m1 = ${r2}")
        // An exact dividend widened by a wider exact divisor: no overflow.
        let m16 : sint<16> = 0 - 1
        let q3 = x2 / m16
        assert q3 == 128 else fail("sint<8> / sint<16> = ${q3}")
        log(info, "NOFAULT_OK")
    end run
end test NoFault

test UZero8
    let dut : Top
    run
        // `count_out` is 0 at cycle 0: a runtime zero the folder cannot see.
        let z8 : uint<8> = dut.count_out
        let a8 : uint<8> = 200
        let q = a8 / z8
        log(info, "AFTER_FAULT ${q}")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test UZero8

test UMod8
    let dut : Top
    run
        let z8 : uint<8> = dut.count_out
        let a8 : uint<8> = 200
        let q = a8 % z8
        log(info, "AFTER_FAULT ${q}")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test UMod8

test SMin8
    let dut : Top
    run
        let smin8 : sint<8> = 0 - 128
        let sm1 : sint<8> = 0 - 1
        let q = smin8 / sm1
        log(info, "AFTER_FAULT ${q}")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test SMin8

test UZero65
    let dut : Top
    run
        let z65 : uint<65> = 0
        let a65 : uint<65> = 200
        let q = a65 / z65
        log(info, "AFTER_FAULT ${q == 0}")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test UZero65

test SMin100
    let dut : Top
    run
        let one100 : sint<100> = 1
        let smin100 : sint<100> = one100 << 99
        let sm100 : sint<100> = 0 - 1
        let q = smin100 % sm100
        log(info, "AFTER_FAULT ${q == 0}")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test SMin100

test UZero200
    let dut : Top
    run
        let z200 : uint<200> = 0
        let a200 : uint<200> = 200
        let q = a200 / z200
        log(info, "AFTER_FAULT ${q == 0}")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test UZero200

test SZero300
    let dut : Top
    run
        let sz300 : sint<300> = 0
        let sa300 : sint<300> = 0 - 7
        let q = sa300 % sz300
        log(info, "AFTER_FAULT ${q == 0}")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test SZero300

test Repeated
    let dut : Top
    run
        let z8 : uint<8> = dut.count_out
        for i in 0 .. 3
            let q = 7 / z8
        end for
        log(info, "AFTER_FAULT repeated")
        wait 1 cycle
        log(info, "UNREACHABLE_AFTER_FATAL")
    end run
end test Repeated
"#;

fn sim_test(dir: &Path, name: &str, src: &str, test_name: &str) -> Output {
    let source = dir.join(format!("{name}.harc"));
    std::fs::write(&source, src).expect("write fixture");
    Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--jobs", "2", "--test", test_name, "--outdir"])
        .arg(dir.join(format!("out_{name}")))
        .env("HARC_NO_LEARN", "1")
        .output()
        .expect("run harc sim")
}

#[test]
fn r16_runtime_division_faults_are_fatal_at_every_width() {
    if !verilator_available() {
        return;
    }
    let dir = fresh_dir("divfaults");
    let output = sim_test(&dir, "div_faults", DIV_FAULTS, "NoFault");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success() && stdout.contains("NOFAULT_OK") && !stdout.contains("FATAL]"),
        "a non-faulting division faulted:\n{stdout}"
    );

    let div0 = "runtime arithmetic error: division by zero";
    let mod0 = "runtime arithmetic error: modulo by zero";
    let divov = "runtime arithmetic error: signed division overflow (MIN / -1)";
    let modov = "runtime arithmetic error: signed modulo overflow (MIN % -1)";
    for (test_name, message, after) in [
        ("UZero8", div0, "AFTER_FAULT 0"),
        ("UMod8", mod0, "AFTER_FAULT 0"),
        ("SMin8", divov, "AFTER_FAULT -128"),
        ("UZero65", div0, "AFTER_FAULT 1"),
        ("SMin100", modov, "AFTER_FAULT 1"),
        ("UZero200", div0, "AFTER_FAULT 1"),
        ("SZero300", mod0, "AFTER_FAULT 1"),
        ("Repeated", div0, "AFTER_FAULT repeated"),
    ] {
        let output = sim_test(&dir, "div_faults", DIV_FAULTS, test_name);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            !output.status.success() && !stdout.contains("ALL TESTS PASSED"),
            "{test_name}: the fault did not fail the run:\n{stdout}"
        );
        let fatal_lines: Vec<&str> = stdout.lines().filter(|l| l.contains("FATAL]")).collect();
        // Exactly one report, even when the fault repeats before the yield.
        assert_eq!(fatal_lines.len(), 1, "{test_name}:\n{stdout}");
        assert!(fatal_lines[0].ends_with(message), "{test_name}:\n{stdout}");
        // The faulting expression yields a deterministic value and the run
        // stops at its next yield, like `log(fatal, ...)`.
        assert!(stdout.contains(after), "{test_name}:\n{stdout}");
        assert!(!stdout.contains("UNREACHABLE_AFTER_FATAL"), "{test_name}:\n{stdout}");
    }
}

/// The constant folder gives `as` and `<<` the runtime meaning.
#[test]
fn r16_const_cast_and_shift_match_runtime() {
    let dir = fresh_dir("constas");
    let source = dir.join("const_as.harc");
    std::fs::write(
        &source,
        r#"
const KU : uint<8> = 0xFC
const KS : sint<8> = KU as sint<8>
const KN : sint<8> = 0 - 1
const KW : uint<16> = KN as uint<16>
const KM : uint<8> = (0 - 4) as uint<8>
const KH : uint<4> = 9
const KSH : uint<16> = KH << 8
const KCAST : uint<16> = (KH as uint<12>) << 8
const KAND : uint<16> = (KH & KH) << 8
const KNOT : uint<16> = (~KH) << 8
const KNEST : uint<16> = (KH << 1) << 1
test ConstAs
    let dut : Top
    run
        let a : sint<8> = KS
        let b : uint<16> = KW
        let c : uint<8> = KM
        let d : uint<16> = KSH
        let e : uint<16> = KCAST
        let f : uint<16> = KAND
        let g : uint<16> = KNOT
        let h : uint<16> = KNEST
        log(info, "${a} ${b} ${c} ${d} ${e} ${f} ${g} ${h}")
    end run
end test ConstAs
"#,
    )
    .expect("write fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("dump-ir")
        .arg(&source)
        .output()
        .expect("run harc dump-ir");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "dump-ir failed:\n{stdout}\n{stderr}");
    // `dump-ir` prints a literal's 64-bit pattern: -4.
    assert!(stdout.contains("Assign(%a, 18446744073709551612)"), "{stdout}\n{stderr}");
    assert!(stdout.contains("Assign(%b, 65535)"), "{stdout}\n{stderr}");
    assert!(stdout.contains("Assign(%c, 252)"), "{stdout}\n{stderr}");
    assert!(stdout.contains("Assign(%d, 0)"), "{stdout}\n{stderr}");
    assert!(stdout.contains("Assign(%e, 2304)"), "{stdout}\n{stderr}");
    assert!(stdout.contains("Assign(%f, 0)"), "{stdout}\n{stderr}");
    assert!(stdout.contains("Assign(%g, 0)"), "{stdout}\n{stderr}");
    // 9 << 1 keeps 4 bits (2), << 1 again is 4.
    assert!(stdout.contains("Assign(%h, 4)"), "{stdout}\n{stderr}");
}

/// Fixed-vector diagnostics name the real source and say what differs.
#[test]
fn r16_vec_diagnostics_name_the_source_and_the_difference() {
    let dir = fresh_dir("vecdiag");
    let cases = [
        (
            "let b : Vec<uint<8>, 2> = a",
            "let a : Vec<uint<16>, 2>",
            "`let b` fixed-vector annotation does not match its initializer: declared \
             `Vec<uint<8>, 2>` but the initializer is `Vec<uint<16>, 2>` (element types \
             differ: uint<8> vs uint<16>)",
        ),
        (
            "let b : Vec<uint<8>, 2> = a",
            "let a : Vec<uint<8>, 3>",
            "(lengths differ: 2 vs 3)",
        ),
        (
            "a[2] = 1",
            "let a : Vec<uint<8>, 2>",
            "element index 2 is out of range for `Vec` `a` of length 2 (valid indices are 0..=1)",
        ),
    ];
    for (index, (stmt, decl, expected)) in cases.iter().enumerate() {
        let source = dir.join(format!("vec_{index}.harc"));
        std::fs::write(
            &source,
            format!(
                "test G\n    let dut : Top\n    run\n        {decl}\n        {stmt}\n    end run\nend test G\n"
            ),
        )
        .expect("write fixture");
        let output = Command::new(env!("CARGO_BIN_EXE_harc"))
            .arg("dump-ir")
            .arg(&source)
            .env("NO_COLOR", "1")
            .output()
            .expect("run harc dump-ir");
        assert!(!output.status.success(), "case {index} was accepted");
        // miette wraps long messages; compare with whitespace collapsed.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let flat = stderr
            .lines()
            .map(|line| line.trim_start_matches(|c: char| c == ' ' || c == '│' || c == '×'))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(flat.contains(expected), "case {index}: {stderr}");
    }
}
