use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;
use std::path::PathBuf;
use std::process::Command;

const SOURCE: &str = r#"
domain SysDomain
  freq_mhz: 100
end domain SysDomain

transactor LocalModel
    value : uint<8> default 7

    hookable read() -> uint<8>
        return value
    end read

    hookable set(v : uint<8>)
        value = v
    end set
end transactor LocalModel

transactor ActiveModel
    value : uint<8> default 3

    hookable read() -> uint<8>
        return value
    end read

    when active
        hookable set(v : uint<8>)
            value = v
        end set
    end when
end transactor ActiveModel

test ComponentLocalInitTest
    let dut : Top
    clock clk = SysDomain

    run
        let model : LocalModel passive
        assert model.read() == 7 else fail("outer model did not default-construct")
        model.set(21)
        repeat 2
            let model : LocalModel passive
            assert model.read() == 7 else fail("loop-local model did not reinitialize")
            model.set(99)
        end repeat
        assert model.read() == 21 else fail("shadowing local changed outer model")

        let active_model : ActiveModel active
        assert active_model.read() == 3 else fail("active model did not default-construct")
        active_model.set(44)
        assert active_model.read() == 44 else fail("active model method did not update state")
        log(info, "ALL TESTS PASSED - component locals default construct")
        wait 1 cycle
    end run
end test ComponentLocalInitTest
"#;

fn merged_source(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("source parses");
    merge::merge_for_sim(vec![parsed], None).expect("source merges")
}

fn lower_source(src: &str) -> Result<ir::TbProgram, lower::LowerError> {
    lower::lower_program(&merged_source(src))
}

fn statements(function: &ir::TbFunction) -> impl Iterator<Item = &ir::Stmt> {
    function.blocks.iter().flat_map(|block| &block.stmts)
}

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

fn runtime_outdir(codegen: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "harc_{codegen}_component_local_init_{}",
        std::process::id(),
    ))
}

#[test]
fn v1_and_tbir_default_construct_typed_component_locals() {
    let merged = merged_source(SOURCE);
    let v1 = cpp_tb::emit(&merged).expect("v1 supports default-constructed component locals");
    assert_eq!(v1.matches("LocalModel model;").count(), 2, "{v1}");
    assert!(v1.contains("ActiveModel active_model;"), "{v1}");

    let prog = lower::lower_program(&merged).expect("TB-IR lowers component locals");
    verify::verify_program(&prog).expect("component locals verify");
    let run = prog.function(prog.tests[0].run);
    let inits: Vec<_> = statements(run)
        .filter_map(|stmt| match stmt {
            ir::Stmt::ComponentInit {
                local,
                component,
                mode,
            } => Some((run.local(*local).name.as_str(), *component, *mode)),
            _ => None,
        })
        .collect();
    assert_eq!(inits.len(), 3, "{inits:?}");
    assert_eq!(inits[0].0, "model");
    assert_eq!(inits[0].2, Some(ir::ComponentInstanceMode::Passive));
    assert_eq!(inits[1].0, "model_2");
    assert_eq!(inits[1].2, Some(ir::ComponentInstanceMode::Passive));
    assert_eq!(inits[2].0, "active_model");
    assert_eq!(inits[2].2, Some(ir::ComponentInstanceMode::Active));

    let cpp = tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default())
        .expect("TB-IR emits component locals");
    for expected in [
        "LocalModel model{};",
        "LocalModel model_2{};",
        "ActiveModel active_model{};",
        "model = decltype(model){};",
        "model_2 = decltype(model_2){};",
        "active_model = decltype(active_model){};",
    ] {
        assert!(cpp.contains(expected), "missing `{expected}` in:\n{cpp}");
    }
}

#[test]
fn component_local_modes_gate_active_only_methods() {
    lower_source(SOURCE).expect("active instance may call an active-only method");

    let passive = SOURCE.replace(
        "let active_model : ActiveModel active",
        "let active_model : ActiveModel passive",
    );
    let err = lower_source(&passive)
        .expect_err("a passive component local cannot call an active-only method");
    let message = err.to_string();
    assert!(message.contains("active-only method `set`"), "{message}");
    assert!(
        message.contains("passive transactor `active_model`"),
        "{message}"
    );

    let modeless = SOURCE.replace(
        "let active_model : ActiveModel active",
        "let active_model : ActiveModel",
    );
    let err = lower_source(&modeless)
        .expect_err("a mode-sensitive transactor local needs an instance mode");
    let message = err.to_string();
    assert!(message.contains("active_model"), "{message}");
    assert!(message.contains("active/passive mode"), "{message}");
}

#[test]
fn runtime_managed_component_locals_are_rejected_instead_of_misemitted() {
    let source = r#"
agent Listener
    input : event<uint<8>>
    seen : uint<8> default 0

    on input(value)
        seen = value
    end on
end agent Listener

test LocalListenerTest
    let dut : Top
    run
        let listener : Listener
    end run
end test LocalListenerTest
"#;
    cpp_tb::emit(&merged_source(source)).expect("v1 installs local component handlers");
    let err = lower_source(source)
        .expect_err("TB-IR must not silently omit local component runtime setup");
    assert!(
        matches!(err, lower::LowerError::Unsupported { .. }),
        "{err:?}"
    );
    let message = err.to_string();
    assert!(message.contains("listener"), "{message}");
    assert!(message.contains("event-handler registration"), "{message}");
}

#[test]
fn verifier_rejects_corrupt_component_local_initialization() {
    let prog = lower_source(SOURCE).expect("component locals lower");
    verify::verify_program(&prog).expect("control verifies");

    let local_model = prog
        .components
        .iter()
        .position(|component| component.name == "LocalModel")
        .map(|index| ir::ComponentId(index as u32))
        .expect("LocalModel component");

    let mutate_active_init = |mut candidate: ir::TbProgram,
                              mutate: &dyn Fn(
        &mut ir::LocalId,
        &mut ir::ComponentId,
        &mut Option<ir::ComponentInstanceMode>,
    )| {
        let run_id = candidate.tests[0].run;
        let active_local = candidate.functions[run_id.index()]
            .locals
            .iter()
            .position(|local| local.name == "active_model")
            .map(|index| ir::LocalId(index as u32))
            .expect("active-model local");
        let init = candidate.functions[run_id.index()]
            .blocks
            .iter_mut()
            .flat_map(|block| &mut block.stmts)
            .find_map(|stmt| match stmt {
                ir::Stmt::ComponentInit {
                    local,
                    component,
                    mode,
                } if *local == active_local => Some((local, component, mode)),
                _ => None,
            })
            .expect("active-model initialization");
        mutate(init.0, init.1, init.2);
        verify::verify_program(&candidate)
            .expect_err("corrupt component initialization must not verify")
    };

    let errors = mutate_active_init(prog.clone(), &|_, _, mode| {
        *mode = Some(ir::ComponentInstanceMode::Passive)
    });
    let message = format!("{errors:?}");
    assert!(
        message.contains("ComponentCall active-only method `set`"),
        "{message}"
    );

    let errors = mutate_active_init(prog.clone(), &|_, component, _| *component = local_model);
    assert!(format!("{errors:?}").contains("ComponentInit"));

    let errors = mutate_active_init(prog.clone(), &|_, _, mode| *mode = None);
    let message = format!("{errors:?}");
    assert!(message.contains("ComponentInit"), "{message}");
    assert!(message.contains("active/passive mode"), "{message}");

    let mut missing_init = prog;
    let run_id = missing_init.tests[0].run;
    let model_local = missing_init.functions[run_id.index()]
        .locals
        .iter()
        .position(|local| local.name == "model")
        .map(|index| ir::LocalId(index as u32))
        .expect("outer model local");
    for block in &mut missing_init.functions[run_id.index()].blocks {
        block.stmts.retain(|stmt| {
            !matches!(
                stmt,
                ir::Stmt::ComponentInit { local, .. }
                    if *local == model_local
            )
        });
    }
    let errors = verify::verify_program(&missing_init)
        .expect_err("using a component local without its initializer must not verify");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, verify::VerifyError::LocalUseBeforeDef { .. })),
        "{errors:?}"
    );
}

#[test]
fn component_locals_default_construct_at_runtime_in_v1_and_tbir() {
    if !verilator_present() {
        eprintln!("skipping component-local initialization runtime test: verilator not found");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for codegen in ["v1", "tbir"] {
        let outdir = runtime_outdir(codegen);
        let _ = std::fs::remove_dir_all(&outdir);
        std::fs::create_dir_all(&outdir).expect("create component-init runtime directory");
        let source = outdir.join("component_local_init.harc");
        std::fs::write(&source, SOURCE).expect("write component-init source");

        let output = Command::new(env!("CARGO_BIN_EXE_harc"))
            .arg("sim")
            .arg("--codegen")
            .arg(codegen)
            .arg("--sv")
            .arg(root.join("tests/dut/top_counter.sv"))
            .arg(&source)
            .arg("--top")
            .arg("Top")
            .arg("--outdir")
            .arg(&outdir)
            .output()
            .unwrap_or_else(|error| panic!("spawn {codegen} component-local simulation: {error}"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{codegen} component-local simulation failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        assert!(
            stdout.contains("ALL TESTS PASSED - component locals default construct")
                || stderr.contains("ALL TESTS PASSED - component locals default construct"),
            "{codegen} component-local simulation did not reach its success marker:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        std::fs::remove_dir_all(&outdir).expect("remove component-init runtime directory");
    }
}
