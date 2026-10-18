//! Small renderer-neutral helpers shared by native backends.

pub(crate) use crate::codegen::native::{
    bus_param_env_with_port_override, component_unique_registry_name, emit_extern_fn_decls,
    gate_passes,
};

#[cfg(test)]
pub(crate) use crate::codegen::native::ir_vec_elem_class;
