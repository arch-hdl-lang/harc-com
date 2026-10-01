use harc::codegen::{cpp_tb, tbir};
use harc::ir::{IrType, PortDirection};

#[test]
fn native_arch_vec_ports_preserve_shape_and_emit_indexed_access() {
    let dir = std::env::temp_dir().join(format!("harc-native-vec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("VecTop.arch");
    std::fs::write(
        &path,
        r#"module VecTop
  param WIDTH: const = 32;
  param LANES: const = 2;
  port clk: in Clock<SysDomain>;
  port data: in Vec<UInt<WIDTH>, LANES>;
  port result: out Vec<SInt<16>, 2>;
  port flags: in Vec<Bool, 2>;
  port nested: in Vec<Vec<UInt<8>, 2>, 2>;
end module VecTop
"#,
    )
    .unwrap();
    for (overrides, width, lanes) in [
        (vec![], 32, 2),
        (vec!["WIDTH=8".into(), "LANES=4".into()], 8, 4),
    ] {
        let catalog = cpp_tb::dut_interface_catalog_with_parameter_overrides(
            &[],
            &[path.clone()],
            "VecTop",
            &Default::default(),
            &overrides,
        )
        .unwrap()
        .unwrap();
        let input = catalog
            .port("data")
            .expect("native Vec input must be catalogued");
        assert_eq!(input.direction(), PortDirection::In);
        assert_eq!(input.resolved_width(), Some(width));
        assert_eq!(input.value_type(), &IrType::UInt(Some(width)));
        assert_eq!(input.unpacked_elements(), Some(lanes));
        assert_eq!(input.packed_lane_width(), None);
        assert!(
            catalog.port("nested").is_none(),
            "nested arrays must not be flattened"
        );
        let flags = catalog.port("flags").expect("boolean Vec input");
        assert_eq!(flags.value_type(), &IrType::Bool);
        assert_eq!(flags.unpacked_elements(), Some(2));
        let output = catalog
            .port("result")
            .expect("native Vec output must be catalogued");
        assert_eq!(output.direction(), PortDirection::Out);
        assert_eq!(output.value_type(), &IrType::SInt(Some(16)));
        assert_eq!(output.unpacked_elements(), Some(2));
        let source = harc::parser::parse_source(
            r#"test VectorPorts
    let dut : VecTop
    run
        dut.data[0] = 7
        let idx : uint<8> = 1
        dut.data[idx] = 9
        assert dut.result[0] == 0 else fail("constant lane")
        assert dut.result[idx] == 0 else fail("dynamic lane")
    end run
end test VectorPorts
"#,
        )
        .unwrap();
        let program = harc::ir::lower::lower_program(&source).unwrap();
        let opts = cpp_tb::EmitOpts {
            dut_interface: Some(catalog),
            ..Default::default()
        };
        tbir::emit(&program, &source, &opts).expect("TBIR must resolve indexed native Vec ports");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
