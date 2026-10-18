//! Backend-neutral native-emission options and output contracts.

use crate::codegen::dut_interface::CosimOpts;

#[derive(Debug)]
pub struct EmitError(pub String);

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EmitError {}

#[derive(Default, Clone)]
pub struct EmitOpts {
    pub mt: bool,
    pub vec_lane_widths: std::collections::HashMap<String, u32>,
    pub dut_port_widths: std::collections::HashMap<String, u32>,
    pub dut_interface: Option<crate::ir::passes::dut_access::DutInterfaceCatalog>,
    pub program_verified: bool,
    pub build_profile_inputs: Vec<String>,
    pub common_abi_inputs: Vec<String>,
    pub dut_bus_port_overrides:
        std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    pub cosim: Option<CosimOpts>,
}

impl EmitOpts {
    pub(crate) fn effective_vec_lane_widths(&self) -> std::collections::HashMap<String, u32> {
        let mut widths = self.vec_lane_widths.clone();
        if let Some(interface) = &self.dut_interface {
            for port in interface.ports() {
                if let Some(width) = port.packed_lane_width() {
                    widths.insert(port.name().to_string(), width);
                }
            }
        }
        widths
    }
}

#[derive(Debug, Clone)]
pub struct GeneratedCppFile {
    pub filename: String,
    pub contents: String,
}

#[derive(Debug, Clone)]
pub struct SplitCppOutput {
    pub files: Vec<GeneratedCppFile>,
    pub test_names: Vec<String>,
}
