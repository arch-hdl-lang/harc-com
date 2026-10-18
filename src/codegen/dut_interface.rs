//! Backend-neutral DUT interface and co-simulation contracts.

#[derive(Debug, Clone)]
pub struct CosimPort {
    pub name: String,
    pub width_bits: u32,
    pub is_input: bool,
    pub is_signed: bool,
    pub packed_lane_width: Option<u32>,
    pub unpacked_elems: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CosimProbe {
    pub name: String,
    pub width_bits: u32,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct CosimOpts {
    pub ports: Vec<CosimPort>,
    pub probes: Vec<CosimProbe>,
    pub half_period_ps: u64,
}

impl CosimOpts {
    pub fn probe_ids(&self) -> Vec<(usize, Option<(usize, usize)>)> {
        let mut next = self.ports.len();
        self.probes
            .iter()
            .map(|probe| {
                let read = next;
                next += 1;
                let force = probe.force.then(|| {
                    let ids = (next, next + 1);
                    next += 2;
                    ids
                });
                (read, force)
            })
            .collect()
    }
}

// The scanners still share the mature parser implementation with the v1
// emitter. Keeping their public entry points here prevents callers from
// depending on that emitter while the parser is split into its own module.
pub use crate::codegen::native::{
    add_arch_scalar_bit_lanes, cosim_ports_from_sv, dut_bus_port_overrides_from_files,
    dut_interface_catalog, dut_interface_catalog_with_parameter_overrides,
    dut_port_widths_from_files, dut_port_widths_from_sv, dut_probes, dut_type_name,
    vec_lane_widths_from_sv,
};
