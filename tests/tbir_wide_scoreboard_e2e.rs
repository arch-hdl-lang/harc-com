//! End-to-end compile/run gate for mixed-carrier wide scoreboard expressions.

use std::path::PathBuf;
use std::process::Command;

const TEST: &str = r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

const NEG : sint<8> = -1
const POS : sint<8> = 1
const UPOS : sint = 1

scoreboard Sb
    wide : uint<256> default 240
    mid : uint<128> default 0x10000000000000000
    narrow : uint<8>
    scalar : uint<64> default 1
    odd : uint<200>
    narrowq : queue<uint<8>>
end scoreboard Sb

testbench Tb
    dut : Top
    sb : Sb

    on sb.wide rising
        log(info, "wide lifecycle trigger")
    end on
end testbench Tb

impl WideScoreboardTest for Tb
    clock clk = SysDomain
    run
        sb.wide = 0x100000000000000000000000000000000000000000000000000
        let forward = sb.mid + sb.wide
        let reverse = sb.wide + sb.mid
        assert forward == reverse else fail("mixed-width inference depends on operand order")
        assert forward[200:200] else fail("mixed-width inferred local truncated")
        let widthless_mix = sb.wide + UPOS
        assert widthless_mix[200:200] else fail("widthless signed mix inferred too narrow")
        let signed_mask_shift = (sb.wide & NEG) >> 1
        assert signed_mask_shift[199:199] else fail("signed mask falsely narrowed wide shift")
        let chosen = false ? sb.mid : sb.wide
        assert chosen[200:200] else fail("ternary inferred local truncated")
        let flag = !sb.wide
        assert !flag else fail("logical-not inferred local is not bool")
        sb.wide = 240
        sb.odd = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF
        sb.odd = sb.odd + 1
        assert sb.odd == 0 else fail("non-word-aligned wide add did not wrap")
        sb.narrow = 1
        sb.narrow = (sb.wide & 0xFF) >> 4
        sb.narrowq.push((sb.wide & 0xFF) >> 4)
        let queued = sb.narrowq.pop()
        assert sb.narrow == 15 else fail("masked scalar=${sb.narrow}")
        assert queued == 15 else fail("masked queue=${queued}")
        sb.wide = sb.wide | sb.mid
        sb.wide = (sb.mid + sb.wide) + 1
        assert sb.wide[7:0] == 241 else fail("wide add failed")
        assert sb.wide[71:64] == 2 else fail("reverse operand promotion failed")
        sb.wide = sb.wide + -1
        sb.wide = sb.wide + NEG
        assert sb.wide[7:0] == 239 else fail("negative operand extension failed")
        sb.wide = 0
        sb.wide = sb.wide + POS
        assert sb.wide == 1 else fail("typed positive signed operand extension failed")
        sb.wide = 0
        sb.wide = sb.wide + UPOS
        assert sb.wide == 1 else fail("widthless positive signed operand extension failed")
        sb.wide = 0
        sb.wide = sb.wide + !0
        assert sb.wide == 1 else fail("logical-not operand sign-extended")
        sb.wide = 0
        sb.wide = sb.wide + (sb.wide == 0)
        assert sb.wide == 1 else fail("comparison bool operand was not scalarized")
        sb.wide = 0
        sb.wide = sb.wide + (true && true)
        assert sb.wide == 1 else fail("logical bool operand was not scalarized")
        sb.wide = sb.mid + 1
        assert sb.wide > sb.mid else fail("wide ordered comparison failed")
        sb.wide = true ? sb.wide : sb.mid
        assert sb.wide > sb.mid else fail("wide ternary true branch failed")
        sb.wide = false ? sb.wide : sb.mid
        assert sb.wide == sb.mid else fail("wide ternary false branch failed")
        assert sb.wide && true else fail("wide logical conversion failed")
        assert !(!sb.wide) else fail("wide unary truthiness failed")
        assert (true ? sb.mid : sb.wide) && true
            else fail("reverse ternary truthiness failed")
        assert !(!(true ? sb.mid : sb.wide))
            else fail("nested reverse ternary truthiness failed")
        assert (true ? sb.mid : sb.wide)
            else fail("bare reverse ternary truthiness failed")
        sb.wide = sb.wide >> (sb.wide & 0x7)
        sb.narrow = 1 << (sb.wide & 0x7)
        assert sb.narrow == 1 else fail("wide shift count conversion failed")
        sb.wide = 3
        sb.narrow = 1 << sb.wide
        assert sb.narrow == 8 else fail("wide literal-lhs shift count clamped")
        sb.mid = 3
        sb.narrow = 1 << sb.mid
        assert sb.narrow == 8 else fail("u128 literal-lhs shift count clamped")
        sb.wide = 65
        sb.scalar = sb.scalar << sb.wide
        assert sb.scalar == 0 else fail("wide count scalar shift was undefined")
        sb.scalar = 1 << 0x100000000000000000000
        assert sb.scalar == 0 else fail("wide literal shift count was undefined")
        assert 0x100000000000000000000000000000000
            else fail("wide literal truthiness failed")
        sb.wide = -1
        assert sb.wide[255:192] == 0xFFFFFFFFFFFFFFFF
            else fail("negative direct write did not sign-extend")
        sb.wide = 1
        assert sb.wide[255:64] == 0 else fail("positive direct write sign-extended")
        let negated = -sb.wide
        assert negated[255:192] == 0xFFFFFFFFFFFFFFFF
            else fail("wide unary negation local did not compile or sign-fill")
        sb.wide = -sb.wide
        assert sb.wide[255:192] == 0xFFFFFFFFFFFFFFFF
            else fail("wide unary negation assignment did not compile or sign-fill")
        sb.wide = 1
        wait sb.wide cycles
        wait until true timeout sb.wide cycles
        cover past(sb.wide)
        cover stable(sb.wide)
        wait 2 cycles
    end run
end impl WideScoreboardTest
"#;

fn verilator_present() -> bool {
    Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn tbir_mixed_wide_scoreboard_expressions_compile_and_run() {
    if !verilator_present() {
        eprintln!("skipping tbir_wide_scoreboard_e2e: verilator not found on PATH");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let outdir = std::env::temp_dir().join(format!(
        "harc_tbir_wide_scoreboard_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create temp outdir");
    let source = outdir.join("wide_scoreboard.harc");
    std::fs::write(&source, TEST).expect("write HARC probe");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("sim")
        .arg("--codegen")
        .arg("tbir")
        .arg("--sv")
        .arg(root.join("tests/dut/top_counter.sv"))
        .arg(&source)
        .arg("--top")
        .arg("Top")
        .arg("--outdir")
        .arg(&outdir)
        .output()
        .expect("spawn harc sim");
    assert!(
        output.status.success(),
        "TBIR wide-scoreboard simulation failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&outdir);
}
