//! Regressions for the R16 hunt-1 frontend/CLI defects: `extend` closer
//! and qualifier resolution (spec §3.6), newline-terminated postfix
//! `(`/`[` (spec §1), `--check-backends` option forwarding, `HARC_SEED`
//! parsing, path-naming file errors, whole-suite runs without `--test`,
//! and `--verbosity` (spec §7.7).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn harc() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harc"));
    command
        .env("NO_COLOR", "1")
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .env_remove("HARC_SEED")
        .env_remove("HARC_TEST")
        .env_remove("HARC_VERBOSITY");
    command
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_frontend_cli_hunt1_{}_{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, source).expect("write source");
    path
}

fn sv() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/dut/top_counter.sv"))
}

fn arch_dut() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/dut/top_counter.arch"))
}

/// stdout + stderr with miette's line wrapping undone: the box-drawing
/// gutter is dropped and whitespace runs collapse to one space.
fn text(output: &Output) -> String {
    let raw = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    raw.replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// [text] with all whitespace removed, for matching wrapped paths.
fn squashed(output: &Output) -> String {
    text(output).replace(' ', "")
}

fn check(path: &Path) -> Output {
    harc().arg("check").arg(path).output().expect("run harc check")
}

fn verilator_available() -> bool {
    let found = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found && std::env::var_os("HARC_REQUIRE_VERILATOR").is_some() {
        panic!("HARC_REQUIRE_VERILATOR is set but verilator is not on PATH");
    }
    found
}

const TWO_TESTS: &str = "test One
    let dut : Top
    run
        log(info, \"ONE_RAN\")
    end run
end test One

test Two
    let dut : Top
    run
        log(error, \"TWO_FAILS\")
    end run
end test Two
";

// ── parser-1: `end extend` must repeat the opener's full dotted path ──────

#[test]
fn extend_closer_must_match_target_path() {
    let dir = scratch("extend_closer");
    let bad = write(
        &dir,
        "bad.harc",
        "transaction Txn\n    len : uint<8>\nend transaction Txn\n\n\
         extend Txn\n    keep len < 16\nend extend Bogus\n",
    );
    let out = check(&bad);
    assert!(!out.status.success(), "mismatched extend closer accepted:\n{}", text(&out));
    assert!(
        text(&out).contains("mismatched closing name: expected `Txn`, found `Bogus`"),
        "{}",
        text(&out)
    );

    // Partial path in the closer of a qualified opener is a mismatch too.
    let partial = write(
        &dir,
        "partial.harc",
        "package P\n    transaction Txn\n        len : uint<8>\n    end transaction Txn\nend package P\n\n\
         extend P.Txn\n    keep len < 16\nend extend Txn\n",
    );
    let out = check(&partial);
    assert!(
        text(&out).contains("expected `P.Txn`, found `Txn`"),
        "partial closer accepted:\n{}",
        text(&out)
    );

    // The full qualified closer against a package that declares the base.
    let good = write(
        &dir,
        "good.harc",
        "package P\n    transaction Txn\n        len : uint<8>\n    end transaction Txn\nend package P\n\n\
         extend P.Txn\n    keep len < 16\nend extend P.Txn\n",
    );
    let out = check(&good);
    assert!(out.status.success(), "valid qualified extend rejected:\n{}", text(&out));
}

// ── parser-2: depth-1 rule and qualifier resolution ───────────────────────

#[test]
fn extend_of_an_extension_is_rejected_as_depth_two() {
    let dir = scratch("extend_depth2");
    let src = write(
        &dir,
        "depth2.harc",
        "transaction Txn\n    len : uint<8>\nend transaction Txn\n\n\
         package Short\n    extend Txn\n        keep len < 16\n    end extend Txn\nend package Short\n\n\
         package Strict\n    extend Short.Txn\n        keep len < 8\n    end extend Short.Txn\nend package Strict\n",
    );
    let out = check(&src);
    assert!(!out.status.success(), "depth-2 extend accepted:\n{}", text(&out));
    assert!(text(&out).contains("extends an extension"), "{}", text(&out));
    assert!(text(&out).contains("depth-1"), "{}", text(&out));
}

#[test]
fn extend_with_unresolved_qualifier_is_rejected() {
    let dir = scratch("extend_noqual");
    let src = write(
        &dir,
        "noqual.harc",
        "transaction Txn\n    len : uint<8>\nend transaction Txn\n\n\
         extend No.Such.Txn\n    keep len < 8\nend extend No.Such.Txn\n",
    );
    let out = check(&src);
    assert!(!out.status.success(), "unresolved qualifier accepted:\n{}", text(&out));
    assert!(
        text(&out).contains("qualifier `No.Such` does not name a declared package"),
        "{}",
        text(&out)
    );
}

#[test]
fn qualified_test_extend_no_longer_merges_by_last_segment() {
    // Used to merge into `test BaseT` and run (last-segment lookup).
    let dir = scratch("extend_merge");
    let base = write(
        &dir,
        "base.harc",
        "test BaseT\n    let dut : Top\n    run\n        log(info, \"base\")\n    end run\nend test BaseT\n",
    );
    let ext = write(
        &dir,
        "ext.harc",
        "extend Nope.Deeper.BaseT\n    log(info, \"extended\")\nend extend Nope.Deeper.BaseT\n",
    );
    let out = harc()
        .args(["sim", "--emit-only", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&base)
        .arg(&ext)
        .output()
        .expect("run harc sim");
    assert!(!out.status.success(), "unresolved qualified extend merged:\n{}", text(&out));
    assert!(
        text(&out).contains("qualifier `Nope.Deeper` does not name a declared package"),
        "{}",
        text(&out)
    );
}

// ── D8 (parser-3): `;` stays rejected ─────────────────────────────────────

#[test]
fn semicolon_is_not_a_statement_separator() {
    let dir = scratch("semicolon");
    let src = write(
        &dir,
        "apply.harc",
        "package A\nend package A\npackage B\nend package B\n\n\
         test T\n    let dut : Top\n    apply A; apply B\n    run\n        log(info, \"x\")\n    end run\nend test T\n",
    );
    let out = check(&src);
    assert!(!out.status.success(), "`;` accepted:\n{}", text(&out));
    assert!(text(&out).contains("`;` is not a statement separator"), "{}", text(&out));
}

// ── D9 (parser-4): newline ends an expression before postfix `(` / `[` ────

const NEWLINE_PAREN: &str = "function g() -> uint<8>
    return 7
end function g

test T
    let dut : Top
    run
        let a : uint<8> = 3
        let y : uint<8> = a
        (g())
        log(info, \"y=${y}\")
    end run
end test T
";

#[test]
fn newline_before_paren_starts_a_new_statement() {
    let dir = scratch("newline_paren");
    let src = write(&dir, "nl.harc", NEWLINE_PAREN);
    // fmt must keep the two statements on separate lines.
    let out = harc().arg("fmt").arg(&src).output().expect("run harc fmt");
    assert!(out.status.success(), "{}", text(&out));
    let formatted = String::from_utf8_lossy(&out.stdout);
    assert!(
        formatted.contains("let y : uint<8> = a\n") && formatted.contains("\n        (g())\n"),
        "fmt joined the lines:\n{formatted}"
    );
    assert!(!formatted.contains("a(g())"), "fmt glued a call:\n{formatted}");
    // Lowering sees `a` and a separate `(g())`, not a helper call `a(...)`.
    let out = harc().arg("dump-ir").arg(&src).output().expect("run dump-ir");
    assert!(out.status.success(), "dump-ir failed:\n{}", text(&out));
}

#[test]
fn newline_before_bracket_does_not_index() {
    let dir = scratch("newline_bracket");
    let src = write(
        &dir,
        "nlb.harc",
        "test T\n    let dut : Top\n    run\n        let v : uint<8> = 5\n        let y : uint<8> = v\n        [1]\n    end run\nend test T\n",
    );
    // `v` ends at the newline, and the split-off `[1]` line has no effect,
    // so it is rejected instead of indexing `v` or vanishing.
    let out = harc().arg("check").arg("--ast").arg(&src).output().expect("check --ast");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Index {"),
        "newline-separated `[` parsed as an index:\n{}",
        text(&out)
    );
    assert!(!out.status.success(), "stray `[1]` line accepted:\n{}", text(&out));
    assert!(text(&out).contains("expression statement has no effect"), "{}", text(&out));
    // Same line still indexes.
    let same = write(
        &dir,
        "same.harc",
        "test T\n    let dut : Top\n    run\n        let v : uint<8> = 5\n        let y : uint<1> = v[1]\n    end run\nend test T\n",
    );
    let out = harc().arg("check").arg("--ast").arg(&same).output().expect("check --ast");
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Index {"), "{}", text(&out));
}

// ── CLI-1 / CLI-2: --check-backends forwards or rejects sim options ───────

fn check_backends(dir: &Path, extra: &[&str], src: &Path) -> Output {
    harc()
        .args(["sim", "--check-backends", "--top", "Top", "--dut"])
        .arg(arch_dut())
        .arg("--sv")
        .arg(sv())
        .arg("--arch-bin")
        .arg(dir.join("no_such_arch"))
        .arg("--outdir")
        .arg(dir.join("out"))
        .args(extra)
        .arg(src)
        .output()
        .expect("run harc sim --check-backends")
}

#[test]
fn check_backends_honours_emit_only() {
    let dir = scratch("cb_emit_only");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = check_backends(&dir, &["--emit-only"], &src);
    assert!(
        out.status.success(),
        "--emit-only still compiled/ran a backend:\n{}",
        text(&out)
    );
    assert!(!text(&out).contains("no_such_arch sim"), "{}", text(&out));
    assert!(dir.join("out/arch_backend/tw.cpp").is_file());
    assert!(dir.join("out/sv_backend/tw.cpp").is_file());
}

#[test]
fn check_backends_honours_compile_scope() {
    let dir = scratch("cb_scope");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = check_backends(&dir, &["--emit-only", "--compile-scope", "test"], &src);
    assert!(!out.status.success(), "--compile-scope test without --test accepted");
    assert!(
        text(&out).contains("--compile-scope test requires --test <name>"),
        "{}",
        text(&out)
    );
    let out = check_backends(
        &dir,
        &["--emit-only", "--compile-scope", "test", "--test", "One"],
        &src,
    );
    assert!(out.status.success(), "{}", text(&out));
    for leg in ["arch_backend", "sv_backend"] {
        let cpp = fs::read_to_string(dir.join("out").join(leg).join("tw__test_One.cpp"))
            .expect("focused emit");
        assert!(cpp.contains("run_One"), "{leg}: selected test missing");
        assert!(!cpp.contains("run_Two"), "{leg}: --compile-scope test ignored");
    }
}

#[test]
fn check_backends_rejects_options_it_cannot_honour() {
    let dir = scratch("cb_reject");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let trace = dir.join("mine.jsonl");
    let out = check_backends(&dir, &["--record-trace", trace.to_str().unwrap()], &src);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("--record-trace is not supported with --check-backends"),
        "{}",
        text(&out)
    );
    let out = check_backends(&dir, &["--cpp-split", "tests"], &src);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("not supported with --check-backends"),
        "{}",
        text(&out)
    );
}

// ── CLI-5: one HARC_SEED parser ───────────────────────────────────────────

#[cfg(unix)]
#[test]
fn check_backends_parses_hex_harc_seed_like_plain_sim() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("cb_seed");
    let record = dir.join("seed.txt");
    let mock = dir.join("arch");
    fs::write(
        &mock,
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s' \"$HARC_SEED\" \"$HARC_VERBOSITY\" \"$HARC_VERBOSITY_OF\" > '{}'\nexit 1\n",
            record.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o755)).unwrap();
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let _ = harc()
        .env("HARC_SEED", "0x10")
        .args(["sim", "--check-backends", "--top", "Top", "--test", "One"])
        .args(["--verbosity", "high", "--verbosity-of", "AXI_DRV=low", "--dut"])
        .arg(arch_dut())
        .arg("--sv")
        .arg(sv())
        .arg("--arch-bin")
        .arg(&mock)
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim --check-backends");
    assert_eq!(
        fs::read_to_string(&record).expect("mock arch was not invoked"),
        "16|HIGH|AXI_DRV=LOW",
        "HARC_SEED=0x10 must reach the ARCH leg as 16, with the verbosity settings"
    );
}

#[test]
fn unparsable_harc_seed_is_an_error() {
    let dir = scratch("seed_bad");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    for bad in ["+5", "0x+5", "1__0", "_1"] {
        let out = harc()
            .env("HARC_SEED", bad)
            .args(["sim", "--emit-only", "--top", "Top", "--sv"])
            .arg(sv())
            .arg("--outdir")
            .arg(dir.join("out"))
            .arg(&src)
            .output()
            .expect("run harc sim");
        assert!(!out.status.success(), "HARC_SEED={bad} accepted");
    }
    let out = harc()
        .env("HARC_SEED", "banana")
        .args(["sim", "--emit-only", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(!out.status.success(), "HARC_SEED=banana accepted:\n{}", text(&out));
    assert!(text(&out).contains("HARC_SEED=`banana` is not a valid seed"), "{}", text(&out));
}

// ── CLI-7: file errors name the path and the flag ─────────────────────────

#[test]
fn missing_files_are_named_with_their_flag() {
    let dir = scratch("missing_files");
    let out = check(&dir.join("nope.harc"));
    assert!(text(&out).contains("nope.harc"), "{}", text(&out));
    let out = check(&dir);
    assert!(
        squashed(&out).contains(dir.to_str().unwrap()),
        "directory error does not name the path:\n{}",
        text(&out)
    );
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = harc()
        .args(["sim", "--emit-only", "--top", "Top", "--sv"])
        .arg(dir.join("nope.sv"))
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(!out.status.success());
    assert!(
        text(&out).contains("--sv file") && text(&out).contains("nope.sv"),
        "{}",
        text(&out)
    );
}

// ── CLI-4 / CLI-8: help text matches behaviour ────────────────────────────

#[test]
fn help_text_matches_behaviour() {
    let out = harc().arg("--help").output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Pretty-print one HARC source file"),
        "{}",
        text(&out)
    );
    let out = harc().args(["sim", "--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(!help.contains("to `v1` for `--check-backends`"), "{help}");
    assert!(!help.contains("Not combinable with `--mt`"), "{help}");
}

// ── D10 (CLI-6): no --test runs every test ────────────────────────────────

#[test]
fn multi_test_dispatcher_runs_the_whole_suite_by_default() {
    let dir = scratch("suite_emit");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = harc()
        .args(["sim", "--emit-only", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(out.status.success(), "{}", text(&out));
    let cpp = fs::read_to_string(dir.join("out/tw.cpp")).unwrap();
    assert!(cpp.contains("harc_run_suite"), "dispatcher still runs only the first test");
    assert!(!cpp.contains("if (!test_sel) return run_One"), "{cpp}");
}

#[test]
fn whole_suite_run_reports_a_failing_second_test() {
    if !verilator_available() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = scratch("suite_run");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = harc()
        .args(["sim", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    let log = text(&out);
    assert!(!out.status.success(), "failing test Two did not fail the run:\n{log}");
    assert!(log.contains("TWO_FAILS"), "test Two never ran:\n{log}");
    assert!(log.contains("1 TESTS FAILED"), "{log}");
    assert!(!log.contains("ALL TESTS PASSED"), "{log}");
    let sim_log = fs::read_to_string(dir.join("out/sim.log")).unwrap();
    assert!(
        sim_log.contains("ONE_RAN") && sim_log.contains("TWO_FAILS"),
        "sim.log lost a test's output:\n{sim_log}"
    );
}

#[test]
fn cosim_multi_test_suite_requires_test_selection() {
    let dir = scratch("cosim_multi");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = harc()
        .args(["sim", "--cosim", "dpi", "--emit-only", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("select one with --test <name>"), "{}", text(&out));
}

// ── D11 (CLI-9): --verbosity, debug hidden by default ─────────────────────

const DEBUG_LOG: &str = "test D
    let dut : Top
    run
        log(debug, \"DEBUG_SHOWN\")
        log(info, \"INFO_SHOWN\")
    end run
end test D
";

#[test]
fn debug_logs_are_hidden_unless_verbosity_is_high() {
    if !verilator_available() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = scratch("verbosity");
    let src = write(&dir, "v.harc", DEBUG_LOG);
    let run = |extra: &[&str]| {
        harc()
            .args(["sim", "--top", "Top", "--sv"])
            .arg(sv())
            .arg("--outdir")
            .arg(dir.join("out"))
            .args(extra)
            .arg(&src)
            .output()
            .expect("run harc sim")
    };
    let out = run(&[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("INFO_SHOWN"), "{}", text(&out));
    assert!(!text(&out).contains("DEBUG_SHOWN"), "debug printed by default:\n{}", text(&out));
    let out = run(&["--verbosity", "HIGH"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("DEBUG_SHOWN"), "{}", text(&out));
    let out = run(&["--verbosity", "low"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("INFO_SHOWN"), "{}", text(&out));
}

const ATTR_LOGS: &str = "test D
    let dut : Top
    run
        log(info, \"INFO_HIGH\", verbosity=HIGH)
        log(debug, \"DEBUG_LOW\", verbosity=LOW)
        log(info, \"DRV_INFO\", id=\"AXI_DRV\")
        log(debug, \"DRV_DEBUG\", id=\"AXI_DRV\")
        log(debug, \"MON_DEBUG\", id=\"MON\")
    end run
end test D
";

#[test]
fn log_verbosity_and_id_arguments_are_carried_in_the_ir() {
    let dir = scratch("log_attrs_ir");
    let src = write(&dir, "attrs.harc", ATTR_LOGS);
    let out = harc().arg("dump-ir").arg(&src).output().expect("run dump-ir");
    assert!(out.status.success(), "{}", text(&out));
    let ir = text(&out);
    assert!(ir.contains("verbosity=HIGH"), "verbosity= dropped:\n{ir}");
    assert!(ir.contains("verbosity=LOW"), "verbosity= dropped:\n{ir}");
    assert!(ir.contains("id=\"AXI_DRV\""), "id= dropped:\n{ir}");

    for (bad, why) in [
        ("log(info, \"x\", verbosity=LOUD)", "takes a verbosity level"),
        ("log(info, \"x\", verbosity=\"HIGH\")", "takes a verbosity level"),
        ("log(info, \"x\", id=AXI)", "non-interpolated string literal"),
        ("log(info, \"x\", id=\"\")", "non-interpolated string literal"),
        ("log(info, \"x\", verbosity=LOW, verbosity=HIGH)", "more than one `verbosity=`"),
    ] {
        let src = write(
            &dir,
            "bad.harc",
            &format!("test D\n    let dut : Top\n    run\n        {bad}\n    end run\nend test D\n"),
        );
        let out = harc().arg("dump-ir").arg(&src).output().expect("run dump-ir");
        assert!(!out.status.success(), "{bad} accepted:\n{}", text(&out));
        assert!(text(&out).contains(why), "{bad}:\n{}", text(&out));
    }
}

#[test]
fn per_message_verbosity_and_verbosity_of_filter_at_runtime() {
    if !verilator_available() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = scratch("log_attrs_run");
    let src = write(&dir, "attrs.harc", ATTR_LOGS);
    let run = |extra: &[&str]| {
        let out = harc()
            .args(["sim", "--top", "Top", "--sv"])
            .arg(sv())
            .arg("--outdir")
            .arg(dir.join("out"))
            .args(extra)
            .arg(&src)
            .output()
            .expect("run harc sim");
        assert!(out.status.success(), "{}", text(&out));
        let printed = |m: &str| text(&out).contains(&format!("] {m}"));
        ["INFO_HIGH", "DEBUG_LOW", "DRV_INFO", "DRV_DEBUG", "MON_DEBUG"]
            .into_iter()
            .filter(|m| printed(m))
            .collect::<Vec<_>>()
    };
    // Default MEDIUM: verbosity= overrides the severity default both ways.
    assert_eq!(run(&[]), ["DEBUG_LOW", "DRV_INFO"]);
    // --verbosity-of raises only the matching id.
    assert_eq!(
        run(&["--verbosity-of", "AXI_DRV=high"]),
        ["DEBUG_LOW", "DRV_INFO", "DRV_DEBUG"]
    );
    // ...and replaces --verbosity for it, in either direction.
    assert_eq!(
        run(&["--verbosity", "HIGH", "--verbosity-of", "AXI_DRV=LOW"]),
        ["INFO_HIGH", "DEBUG_LOW", "MON_DEBUG"]
    );
    // Hidden messages still reach the semantic trace.
    let trace = dir.join("trace.jsonl");
    let out = harc()
        .args(["sim", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg("--record-trace")
        .arg(&trace)
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(out.status.success(), "{}", text(&out));
    let recorded = fs::read_to_string(&trace).expect("trace written");
    assert!(recorded.contains("MON_DEBUG") && recorded.contains("INFO_HIGH"), "{recorded}");
}

#[test]
fn verbosity_of_rejects_malformed_entries() {
    let dir = scratch("verbosity_of_bad");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    for bad in ["=HIGH", "AXI", "AXI=LOUD", "A,B=HIGH"] {
        let out = harc()
            .args(["sim", "--emit-only", "--top", "Top", "--sv"])
            .arg(sv())
            .arg("--outdir")
            .arg(dir.join("out"))
            .args(["--verbosity-of", bad])
            .arg(&src)
            .output()
            .expect("run harc sim");
        assert!(!out.status.success(), "--verbosity-of {bad} accepted");
        assert!(text(&out).contains("expected ID=LEVEL"), "{}", text(&out));
    }
}

// ── review follow-ups ─────────────────────────────────────────────────────

#[test]
fn extend_qualifier_sees_every_declaration_of_a_reopened_package() {
    let dir = scratch("extend_reopened");
    let src = write(
        &dir,
        "reopen.harc",
        "package P\n    const K : uint<8> = 1\nend package P\n\n\
         package P\n    transaction Txn\n        len : uint<8>\n    end transaction Txn\nend package P\n\n\
         extend P.Txn\n    keep len < 16\nend extend P.Txn\n",
    );
    let out = check(&src);
    assert!(out.status.success(), "reopened package not indexed:\n{}", text(&out));
}

#[test]
fn extend_qualifier_resolves_relative_to_the_enclosing_package_first() {
    let dir = scratch("extend_relative");
    // `Inner` inside `Outer` declares Txn; a top-level `Inner` only extends
    // it. From inside `Outer`, `Inner.Txn` must mean `Outer.Inner.Txn`.
    let src = write(
        &dir,
        "relative.harc",
        "transaction Txn\n    len : uint<8>\nend transaction Txn\n\n\
         package Inner\n    extend Txn\n        keep len < 16\n    end extend Txn\nend package Inner\n\n\
         package Outer\n    package Inner\n        transaction Txn\n            len : uint<8>\n        end transaction Txn\n    end package Inner\n\n\
         \x20   extend Inner.Txn\n        keep len < 8\n    end extend Inner.Txn\nend package Outer\n",
    );
    let out = check(&src);
    assert!(out.status.success(), "relative qualifier not tried first:\n{}", text(&out));
    // Absolute fallback: from inside `Other`, `Inner.Txn` is the top-level
    // `Inner`, which only extends Txn -> depth-2 error.
    let src = write(
        &dir,
        "absolute.harc",
        "transaction Txn\n    len : uint<8>\nend transaction Txn\n\n\
         package Inner\n    extend Txn\n        keep len < 16\n    end extend Txn\nend package Inner\n\n\
         package Other\n    extend Inner.Txn\n        keep len < 8\n    end extend Inner.Txn\nend package Other\n",
    );
    let out = check(&src);
    assert!(text(&out).contains("extends an extension"), "{}", text(&out));
}

#[test]
fn extend_qualifier_error_explains_use_imports_carry_only_buses() {
    let dir = scratch("extend_use");
    let src = write(
        &dir,
        "use.harc",
        "use P\n\ntransaction Txn\n    len : uint<8>\nend transaction Txn\n\n\
         extend P.Q.Txn\n    keep len < 8\nend extend P.Q.Txn\n",
    );
    let out = check(&src);
    assert!(!out.status.success(), "{}", text(&out));
    let msg = text(&out);
    assert!(msg.contains("`use P` does not make package `P` visible"), "{msg}");
    assert!(msg.contains("`use` imports only carry buses"), "{msg}");
    assert!(msg.contains("pass the file declaring package `P`"), "{msg}");
}

#[test]
fn expression_statements_without_effect_are_rejected() {
    let dir = scratch("no_effect");
    for stmt in ["v", "[1]", "(v)", "v + 1", "v[0]"] {
        let src = write(
            &dir,
            "e.harc",
            &format!(
                "test T\n    let dut : Top\n    run\n        let v : uint<8> = 5\n        {stmt}\n    end run\nend test T\n"
            ),
        );
        let out = check(&src);
        assert!(!out.status.success(), "`{stmt}` accepted as a statement");
        assert!(text(&out).contains("expression statement has no effect"), "{}", text(&out));
    }
    // Calls (also parenthesized) and randomize stay legal.
    let ok = write(
        &dir,
        "ok.harc",
        "function g() -> uint<8>\n    return 7\nend function g\n\n\
         test T\n    let dut : Top\n    run\n        g()\n        (g())\n        log(info, \"x\")\n    end run\nend test T\n",
    );
    let out = check(&ok);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn seed_line_is_logged_at_every_verbosity() {
    let dir = scratch("seed_line_emit");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = harc()
        .args(["sim", "--emit-only", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(out.status.success(), "{}", text(&out));
    let cpp = fs::read_to_string(dir.join("out/tw.cpp")).unwrap();
    assert!(
        cpp.contains(
            "HarcLogAttrScope _harc_seed_always(harc_rt::log::HARC_V_LOW, nullptr); \
             sim_log_line(\"INFO\", \"seed=%llu\""
        ),
        "seed line is not pinned to verbosity LOW"
    );
    if !verilator_available() {
        eprintln!("skipping run half: verilator not on PATH");
        return;
    }
    let out = harc()
        .args(["sim", "--top", "Top", "--verbosity", "LOW", "--test", "One", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("run"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("seed=1"), "{}", text(&out));
    assert!(!text(&out).contains("ONE_RAN"), "{}", text(&out));
}

#[test]
fn cosim_multi_test_suite_accepts_harc_test_as_the_selection() {
    let dir = scratch("cosim_harc_test");
    let src = write(&dir, "tw.harc", TWO_TESTS);
    let out = harc()
        .env("HARC_TEST", "One")
        .args(["sim", "--cosim", "dpi", "--emit-only", "--top", "Top", "--sv"])
        .arg(sv())
        .arg("--outdir")
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("run harc sim");
    assert!(out.status.success(), "HARC_TEST not accepted as a selection:\n{}", text(&out));
}

#[test]
fn whole_suite_logf_file_written_only_by_a_later_test_starts_empty() {
    if !verilator_available() {
        eprintln!("skipping: verilator not on PATH");
        return;
    }
    let dir = scratch("suite_logf");
    let src = write(
        &dir,
        "lf.harc",
        "test A\n    let dut : Top\n    run\n        log(info, \"A\")\n    end run\nend test A\n\n\
         test B\n    let dut : Top\n    run\n        logf(\"b.log\", info, \"B_LINE\")\n    end run\nend test B\n",
    );
    for _ in 0..2 {
        let out = harc()
            .args(["sim", "--top", "Top", "--sv"])
            .arg(sv())
            .arg("--outdir")
            .arg(dir.join("out"))
            .arg(&src)
            .output()
            .expect("run harc sim");
        assert!(out.status.success(), "{}", text(&out));
    }
    let b = fs::read_to_string(dir.join("out/b.log")).expect("b.log");
    assert_eq!(b.matches("B_LINE").count(), 1, "b.log appended across invocations:\n{b}");
}

#[test]
fn two_statements_on_one_line_are_rejected() {
    let dir = scratch("same_line");
    for line in [
        "let a : uint<8> = 1 let b : uint<8> = 2",
        "log(info, \"a\") log(info, \"b\")",
        "let a : uint<8> = 1 a = 2",
    ] {
        let src = write(
            &dir,
            "s.harc",
            &format!("test T\n    let dut : Top\n    run\n        {line}\n    end run\nend test T\n"),
        );
        let out = check(&src);
        assert!(!out.status.success(), "`{line}` accepted");
        assert!(
            text(&out).contains("a statement must end at a newline"),
            "{line}:\n{}",
            text(&out)
        );
    }
    // A statement may end at its block's `end` / `else` on the same line,
    // and `;` keeps its own diagnostic.
    let ok = write(
        &dir,
        "ok.harc",
        "test T\n    let dut : Top\n    run\n        let a : uint<8> = 1\n        if a == 1\n            a = 2 else\n            a = 3 end if\n    end run\nend test T\n",
    );
    let out = check(&ok);
    assert!(out.status.success(), "{}", text(&out));
    let semi = write(
        &dir,
        "semi.harc",
        "test T\n    let dut : Top\n    run\n        log(info, \"a\"); log(info, \"b\")\n    end run\nend test T\n",
    );
    assert!(text(&check(&semi)).contains("`;` is not a statement separator"));
}
