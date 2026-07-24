// Verilator/DPI-C signal-access adapter: forwards the simulator-neutral
// harc_dut_get / harc_dut_set ABI to the SV functions the harness
// exports over DPI-C.
//
// The exported functions are declared by hand rather than by including
// Verilator's generated V<top>__Dpi.h, so this file states the exact
// boundary the spike depends on: two C-linkage functions with
// IEEE 1800 DPI type mapping (SV longint <-> C long long,
// SV int <-> C int). The same declarations link unchanged against
// VCS/Xcelium/Questa export stubs — nothing here is Verilator-specific.
//
// Calls into DPI exports are only legal while the simulator is inside a
// `context` import (harc_init / harc_on_posedge / harc_finish), which is
// the only time the HARC runtime runs — contract point 4 in spec §10.

#include <cstdint>

extern "C" {

// Exported from harness.sv via `export "DPI-C"`.
long long harc_sv_get(int sig_id);
void harc_sv_set(int sig_id, long long value);

uint64_t harc_dut_get(int sig_id) {
    return static_cast<uint64_t>(harc_sv_get(sig_id));
}

void harc_dut_set(int sig_id, uint64_t value) {
    harc_sv_set(sig_id, static_cast<long long>(value));
}

} // extern "C"
