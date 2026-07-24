// Signal IDs shared between the co-sim TB core, the SV harness, and the
// per-simulator signal-access adapters (DPI on Verilator, VPI on Icarus).
//
// In the eventual compiler integration these IDs — and the SV accessor
// functions keyed on them — are generated from the DUT port list, the
// same source of truth `let dut : T` binding uses today. For the spike
// they're hand-written for tests/dut/sync_fifo.sv (TxQueue).
//
// The harness mirrors these values as localparams; keep both in sync.
#ifndef HARC_COSIM_SIG_IDS_H
#define HARC_COSIM_SIG_IDS_H

// TB-driven DUT inputs.
#define HARC_SIG_RST 0
#define HARC_SIG_PUSH_VALID 1
#define HARC_SIG_PUSH_DATA 2
#define HARC_SIG_POP_READY 3
// TB-observed DUT outputs.
#define HARC_SIG_PUSH_READY 4
#define HARC_SIG_POP_VALID 5
#define HARC_SIG_POP_DATA 6
#define HARC_SIG_FULL 7
#define HARC_SIG_EMPTY 8

#define HARC_SIG_COUNT 9

#endif // HARC_COSIM_SIG_IDS_H
