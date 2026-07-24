/*
 * Icarus/VPI signal-access adapter.
 *
 * Icarus Verilog has no DPI-C support, so this bridge reaches the same
 * simulator-neutral HARC C ABI (harc_init / harc_on_posedge /
 * harc_finish + harc_dut_get / harc_dut_set) through IEEE 1800 VPI —
 * which is a standard interface every major simulator ships, not an
 * Icarus-only one. Nothing below uses an Icarus extension:
 *
 *   - $harc_init  system task -> resolves signal handles once via
 *     vpi_handle_by_name, then calls harc_init().
 *   - $harc_tick  system task (from the harness's negedge block) ->
 *     calls harc_on_posedge(); on "done" calls harc_finish() and stops
 *     the simulation with vpi_control(vpiFinish).
 *   - harc_dut_get / harc_dut_set -> vpi_get_value / vpi_put_value
 *     (vpiNoDelay, i.e. an immediate blocking assign — same semantics
 *     as the DPI-exported harc_sv_set function).
 *
 * The HARC TB core (harc_cosim_core.cpp) is compiled unchanged into
 * this .vpi module; only this file differs from the Verilator build.
 */

#include <stdint.h>
#include <vpi_user.h>

#include "harc_cosim_sig_ids.h"

/* From harc_cosim_core.cpp (C linkage). */
extern void harc_init(void);
extern int harc_on_posedge(void);
extern void harc_finish(void);

static vpiHandle g_sig[HARC_SIG_COUNT];

static const char* const g_sig_names[HARC_SIG_COUNT] = {
    "HarcCosimTop.rst",        /* HARC_SIG_RST */
    "HarcCosimTop.push_valid", /* HARC_SIG_PUSH_VALID */
    "HarcCosimTop.push_data",  /* HARC_SIG_PUSH_DATA */
    "HarcCosimTop.pop_ready",  /* HARC_SIG_POP_READY */
    "HarcCosimTop.push_ready", /* HARC_SIG_PUSH_READY */
    "HarcCosimTop.pop_valid",  /* HARC_SIG_POP_VALID */
    "HarcCosimTop.pop_data",   /* HARC_SIG_POP_DATA */
    "HarcCosimTop.full",       /* HARC_SIG_FULL */
    "HarcCosimTop.empty",      /* HARC_SIG_EMPTY */
};

uint64_t harc_dut_get(int sig_id) {
    s_vpi_value v;
    v.format = vpiIntVal;
    vpi_get_value(g_sig[sig_id], &v);
    return (uint64_t)(uint32_t)v.value.integer;
}

void harc_dut_set(int sig_id, uint64_t value) {
    s_vpi_value v;
    v.format = vpiIntVal;
    v.value.integer = (PLI_INT32)value;
    vpi_put_value(g_sig[sig_id], &v, NULL, vpiNoDelay);
}

static PLI_INT32 harc_init_tf(PLI_BYTE8* user_data) {
    int i;
    (void)user_data;
    for (i = 0; i < HARC_SIG_COUNT; ++i) {
        g_sig[i] = vpi_handle_by_name((PLI_BYTE8*)g_sig_names[i], NULL);
        if (!g_sig[i]) {
            vpi_printf("harc-cosim: cannot resolve %s\n", g_sig_names[i]);
            vpi_control(vpiFinish, 1);
            return 0;
        }
    }
    harc_init();
    return 0;
}

static PLI_INT32 harc_tick_tf(PLI_BYTE8* user_data) {
    (void)user_data;
    if (harc_on_posedge() != 0) {
        harc_finish();
        vpi_control(vpiFinish, 0);
    }
    return 0;
}

static void harc_register_tasks(void) {
    s_vpi_systf_data tf;

    tf.type = vpiSysTask;
    tf.sysfunctype = 0;
    tf.tfname = "$harc_init";
    tf.calltf = harc_init_tf;
    tf.compiletf = NULL;
    tf.sizetf = NULL;
    tf.user_data = NULL;
    vpi_register_systf(&tf);

    tf.tfname = "$harc_tick";
    tf.calltf = harc_tick_tf;
    vpi_register_systf(&tf);
}

void (*vlog_startup_routines[])(void) = {
    harc_register_tasks,
    0,
};
