use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .expect("read fixture")
}

fn fatal_actor_fixture() -> String {
    fixture("tlm_target_thread_test.harc").replacen(
        "    run\n",
        "    run\n        log(fatal, \"stop with run and actor frames suspended\")\n        wait 1 cycle\n",
        1,
    )
}

fn verilator_present() -> bool {
    Command::new("verilator")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn assert_cleanup_order(cpp: &str, backend: &str) {
    let joined = cpp.find("for (auto& _w : _workers) _w.join();").unwrap();
    let reports = cpp
        .find("for (auto& _r : _auto_cov_reports) _r();")
        .unwrap();
    let callbacks_cleared = cpp.find("_checkers.clear();").unwrap_or_else(|| {
        panic!("{backend} does not clear callback registries before teardown:\n{cpp}")
    });
    let first_destroy = cpp
        .find("harc_rt::harc_destroy_scheduler_threads(")
        .unwrap_or_else(|| {
            panic!("{backend} does not explicitly destroy scheduler frames:\n{cpp}")
        });
    let dut_final = cpp.find("dut->final();").unwrap();

    assert!(
        joined < reports
            && reports < callbacks_cleared
            && callbacks_cleared < first_destroy
            && first_destroy < dut_final,
        "{backend} teardown order is not worker join -> reports -> callback clear -> frame \
         destruction -> DUT final"
    );
    assert!(
        cpp.matches("harc_rt::harc_destroy_scheduler_threads(")
            .count()
            >= 2,
        "{backend} must destroy both the run/cooperative scheduler and the MT actor scheduler"
    );
}

#[test]
fn self_contained_backends_destroy_all_frames_before_dut_teardown() {
    let fixture = fatal_actor_fixture();
    let source = parse_source(&fixture).expect("fixture parses");
    let merged = merge::merge_for_sim(vec![source], None).expect("fixture merges");
    let opts = cpp_tb::EmitOpts {
        mt: true,
        ..Default::default()
    };

    let v1 = cpp_tb::emit_with_opts(&merged, opts.clone()).expect("v1 emits");
    assert_cleanup_order(&v1, "v1");

    let program = lower::lower_program(&merged).expect("fixture lowers");
    verify::verify_program(&program).expect("fixture verifies");
    let tbir = tbir::emit(&program, &merged, &opts).expect("TBIR emits");
    assert_cleanup_order(&tbir, "TBIR");
}

#[test]
fn fatal_suspended_mt_actors_exit_cleanly_in_both_self_contained_backends() {
    if !verilator_present() {
        eprintln!("SKIP fatal_suspended_mt_actors_exit_cleanly: `verilator` not found");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scratch =
        std::env::temp_dir().join(format!("harc_fatal_actor_cleanup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("create test directory");
    let source = scratch.join("fatal_actor.harc");
    std::fs::write(&source, fatal_actor_fixture()).expect("write HARC fixture");

    for backend in ["v1", "tbir"] {
        let outdir = scratch.join(backend);
        let build = Command::new(env!("CARGO_BIN_EXE_harc"))
            .arg("sim")
            .arg("--sv")
            .arg(root.join("tests/dut/TlmReadInitiator.sv"))
            .arg(&source)
            .args(["--top", "TlmReadInitiator", "--codegen", backend, "--mt"])
            .arg("--outdir")
            .arg(&outdir)
            .output()
            .expect("build fatal actor fixture");
        let binary = outdir.join("obj_dir/VTlmReadInitiator");
        assert!(
            binary.is_file(),
            "{backend} fatal actor build did not produce an executable:\n{}{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );

        let run = Command::new(binary)
            .current_dir(&outdir)
            .env("HARC_SEED", "3")
            .env("HARC_SIM_LOG", outdir.join("fatal.log"))
            .output()
            .expect("run fatal actor fixture");
        assert_eq!(
            run.status.code(),
            Some(1),
            "{backend} fatal actor run did not unwind cleanly:\n{}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        let log = std::fs::read_to_string(outdir.join("fatal.log")).expect("read sim log");
        assert!(log.contains("stop with run and actor frames suspended"));
        assert!(!log.contains("PASS: HARC target-side TLM thread served SV initiator"));
    }

    let _ = std::fs::remove_dir_all(scratch);
}
