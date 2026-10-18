//! Runtime assets shared by every native testbench backend.

use crate::codegen::common_artifacts;

pub const THREAD_RT_HEADER: &str = include_str!("../../runtime/harc_thread_rt.h");
pub const RANDOM_RT_HEADER: &str = include_str!("../../runtime/harc_random_rt.h");
pub const QUEUE_RT_HEADER: &str = include_str!("../../runtime/harc_queue_rt.h");
pub const TRACE_RT_HEADER: &str = include_str!("../../runtime/harc_trace_rt.h");
pub const LOG_RT_HEADER: &str = include_str!("../../runtime/harc_log_rt.h");
pub const Z3_RT_HEADER: &str = include_str!("../../runtime/harc_z3_rt.h");
pub const COSIM_RT_HEADER: &str = include_str!("../../runtime/harc_cosim_rt.h");

pub const RUNTIME_HEADERS: [(&str, &str); 6] = [
    (
        common_artifacts::RUNTIME_HEADER_FILENAMES[0],
        THREAD_RT_HEADER,
    ),
    (
        common_artifacts::RUNTIME_HEADER_FILENAMES[1],
        RANDOM_RT_HEADER,
    ),
    (
        common_artifacts::RUNTIME_HEADER_FILENAMES[2],
        QUEUE_RT_HEADER,
    ),
    (
        common_artifacts::RUNTIME_HEADER_FILENAMES[3],
        TRACE_RT_HEADER,
    ),
    (common_artifacts::RUNTIME_HEADER_FILENAMES[4], LOG_RT_HEADER),
    (common_artifacts::RUNTIME_HEADER_FILENAMES[5], Z3_RT_HEADER),
];

pub fn common_runtime_abi_fingerprint() -> String {
    let mut canonical = String::from("harc-common-runtime-abi-v1\n");
    for (filename, contents) in RUNTIME_HEADERS {
        canonical.push_str(filename);
        canonical.push('=');
        canonical.push_str(&common_artifacts::stable_hash_hex(contents.as_bytes()));
        canonical.push('\n');
    }
    common_artifacts::stable_hash_hex(canonical.as_bytes())
}

pub(crate) fn complete_common_abi_inputs(inputs: &[String]) -> Vec<String> {
    let mut complete = inputs
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    complete.insert(format!("runtime_abi={}", common_runtime_abi_fingerprint()));
    complete.into_iter().collect()
}
