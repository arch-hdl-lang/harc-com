//! Minimal HARC → Verilator C++ TB codegen.
//!
//! Scope (tracking spec §14 "Phase 0" / pre-1a):
//! - One `test T` with one `scope sim` and one `run` block becomes a `main()`
//!   that drives a Verilated DUT.
//! - The conventional name `dut` (per spec §2) marks the DUT instance — the
//!   field-access expression `dut.signal` lowers to `dut->signal`, and
//!   `let dut : <Type>` becomes `V<Type>* dut = new V<Type>;`. The same
//!   pointer-shape rule applies to any `Named`-typed local or function
//!   parameter, not just the literal name `dut`.
//! - **Assignment operators** match ARCH's discipline:
//!     - `=` — blocking assignment (locals, raw DUT signal drives).
//!     - `<-` — channel send (event / buffer / state per spec §17.2; future).
//!     - `<=` — non-blocking assignment, future, valid only inside `seq` /
//!       `thread` blocks (parser disambiguation will mirror ARCH's
//!       `no_lteq` flag once those constructs land in codegen).
//!   Both `=` and `<-` lower to the same `dut->x = v;` C++ today.
//! - `after N cycles ... end after` lowers to `for (int _=0; _<N; _++) tick();`
//!   where `tick()` toggles the clock and calls `eval()` twice.
//! - `assert expr [else fail("msg")]` lowers to a runtime check that
//!   increments an `errors` counter; per-test failure reporting and exit
//!   code mirror the hand-written `examples/counter_tb.cpp` shape.
//! - `log(<severity>, "...")` lowers to a `sim_log_line(<SEV>, ...)` call
//!   that writes `[cycle:N <SEV>] <msg>` to both stdout and `sim.log`.
//!   Test-result semantics from §7.7 are honored:
//!     - `error` increments the `errors` counter (test fails at end of run).
//!     - `fatal` increments + sets `_fatal`, and the main simulation loop
//!       exits at end of the current cycle (this test instance aborts).
//!     - `warn` / `info` / `debug` have no test-result effect.
//!   Verbosity flags, component IDs, and per-component overrides are
//!   spec'd in §7.7 but deferred — the runtime currently prints all
//!   severities unconditionally.
//!
//! Out of scope here:
//! - `tseq`, randomization, properties, coverage groups, fork/join, env/
//!   agent/monitor/scoreboard composition, multi-clock domains.
//! - Type checking — we trust the user's HARC and let the C++ compiler
//!   surface signal-name typos against Verilator's generated header.

use crate::ast::*;
use crate::lexer::Span;
use std::fmt::Write;

/// HARC's coroutine runtime header. Baked into the binary at build
/// time via `include_str!` and dropped into the test build dir by
/// `harc sim` so the emitted `.cpp` can `#include "harc_thread_rt.h"`
/// without a separate file dependency.
pub const THREAD_RT_HEADER: &str = include_str!("../../runtime/harc_thread_rt.h");
pub const RANDOM_RT_HEADER: &str = include_str!("../../runtime/harc_random_rt.h");
pub const QUEUE_RT_HEADER: &str = include_str!("../../runtime/harc_queue_rt.h");
pub const TRACE_RT_HEADER: &str = include_str!("../../runtime/harc_trace_rt.h");
pub const LOG_RT_HEADER: &str = include_str!("../../runtime/harc_log_rt.h");
pub const Z3_RT_HEADER: &str = include_str!("../../runtime/harc_z3_rt.h");
pub const COSIM_RT_HEADER: &str = include_str!("../../runtime/harc_cosim_rt.h");

const INDENT: &str = "    ";

#[derive(Debug)]
pub struct EmitError(pub String);

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for EmitError {}

/// Walk the file for a single `test T { let dut : <Type> }` decl and return
/// the simple name of `<Type>` — the SystemVerilog top module that the C++
/// codegen will reference as `V<Type>`. Used by the harc-sim driver to set
/// Verilator's `--top-module` when running against a pre-built SV DUT.
pub fn dut_type_name(file: &SourceFile) -> Option<String> {
    let test = file.items.iter().find_map(|it| match it {
        Item::Test(t) => Some(t),
        _ => None,
    })?;
    for it in &test.items {
        if let TestItem::Let(l) = it {
            if l.name.name == "dut" {
                return type_simple_name(l.ty.as_ref()).map(|s| s.to_string());
            }
        }
    }
    None
}

/// Scan a generated SystemVerilog DUT for the `--top` module and return a
/// map of `port-name → per-lane bit-width` for every port that flattens
/// to a PACKED 2-D / multi-lane vector — i.e. the SV shapes ARCH's
/// `arch build` emits for `Vec<Bus, N>` (and other multi-lane bus) ports:
///
///   input  logic [2:0]                  m_ar_valid   → lane width 1
///   input  logic [2:0][MASTER_ID_W-1:0] m_ar_id      → lane width MASTER_ID_W
///   output logic [3:0][SLAVE_ID_W-1:0]  s_ar_id      → lane width SLAVE_ID_W
///
/// Lane `i` of such a port occupies bits `[i*W +: W]` of the packed
/// scalar Verilator exposes. The TB codegen consults this map so a HARC
/// `dut.<port>[i]` index lowers to a bit-extract / bit-deposit on the
/// `--sv` backend instead of a (broken) C++ array subscript.
///
/// This is a deliberately small, tolerant textual scan — NOT a full SV
/// parser. It resolves `parameter int NAME = <int>;` defaults declared in
/// the same module header so width references like `[MASTER_ID_W-1:0]`
/// fold to a concrete `W`. Anything it can't fold concretely is skipped
/// (the port simply isn't added to the map; the codegen then falls back
/// to the legacy direct-index path, preserving prior behavior).
///
/// Note: a *single*-dimension port (`logic [2:0] foo`) is ambiguous in
/// pure SV — it could be a 3-lane 1-bit Vec or a plain 3-bit scalar — but
/// for a 1-bit lane width the two interpretations coincide (`(foo>>i)&1`),
/// so recording W=1 is always correct for an indexed access. Multi-bit
/// lanes are only ever produced by the 2-D shape, which is unambiguous.
pub fn vec_lane_widths_from_sv(
    sv_sources: &[std::path::PathBuf],
    top: &str,
) -> std::collections::HashMap<String, u32> {
    let mut out = std::collections::HashMap::new();
    for path in sv_sources {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some(map) = scan_sv_module_lane_widths(&src, top) {
            // First module to define `top` wins; merge any others
            // defensively without overwriting concrete entries.
            for (k, v) in map {
                out.entry(k).or_insert(v);
            }
        }
    }
    out
}

/// Core of `vec_lane_widths_from_sv` over a single SV source string.
/// Returns `None` if `top` isn't declared in this source.
fn scan_sv_module_lane_widths(
    src: &str,
    top: &str,
) -> Option<std::collections::HashMap<String, u32>> {
    let lines: Vec<&str> = src.lines().collect();
    // Find `module <top>` — match the bare module keyword + name, not a
    // substring of some other identifier.
    let start = lines.iter().position(|l| {
        let t = l.trim_start();
        if let Some(rest) = t.strip_prefix("module ") {
            rest.trim_start().strip_prefix(top).is_some_and(|after| {
                after
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_')
            })
        } else {
            false
        }
    })?;

    // Collect the module body up to the matching `endmodule`.
    let body = &lines[start..];
    let end_rel = body
        .iter()
        .position(|l| l.trim_start().starts_with("endmodule"))
        .unwrap_or(body.len());
    let body = &body[..end_rel];

    // Resolve `parameter int NAME = <int>;` (and `localparam`) defaults so
    // width expressions like `[MASTER_ID_W-1:0]` fold to a number.
    let mut params: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for l in body {
        let t = l.trim();
        for kw in [
            "parameter int ",
            "parameter ",
            "localparam int ",
            "localparam ",
        ] {
            if let Some(rest) = t.strip_prefix(kw) {
                if let Some(eq) = rest.find('=') {
                    let name = rest[..eq]
                        .trim()
                        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
                    let name = name.split_whitespace().last().unwrap_or(name);
                    let val_str: String = rest[eq + 1..]
                        .trim()
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '-')
                        .collect();
                    if let Ok(v) = val_str.parse::<i64>() {
                        params.insert(name.to_string(), v);
                    }
                }
                break;
            }
        }
    }

    let mut out = std::collections::HashMap::new();
    for l in body {
        if let Some((name, w)) = parse_sv_port_lane_width(l, &params) {
            out.insert(name, w);
        }
    }
    Some(out)
}

/// Parse one SV port declaration line into `(port_name, lane_width)` when
/// it has the flattened multi-lane shape, else `None`. Handles:
///   `input  logic [N-1:0] name,`               → lane width 1
///   `input  logic [N-1:0][W-1:0] name,`        → lane width W
///   `output logic signed [N-1:0][W-1:0] name,` → lane width W
/// Width expressions fold against `params`. A range `[hi:lo]` yields
/// width `hi-lo+1`.
fn parse_sv_port_lane_width(
    line: &str,
    params: &std::collections::HashMap<String, i64>,
) -> Option<(String, u32)> {
    let t = line.trim();
    let after_dir = t
        .strip_prefix("input ")
        .or_else(|| t.strip_prefix("output "))
        .or_else(|| t.strip_prefix("inout "))?;
    // Drop optional `logic`/`wire`/`reg`/`signed`/`unsigned` qualifiers.
    let mut rest = after_dir.trim();
    for q in ["logic", "wire", "reg", "bit", "signed", "unsigned"] {
        if let Some(r) = rest.strip_prefix(q) {
            // Only strip when followed by whitespace/`[` (a real qualifier,
            // not the start of an identifier).
            if r.starts_with(|c: char| c.is_whitespace() || c == '[') {
                rest = r.trim_start();
            }
        }
    }
    // Collect leading `[..]` dimension groups.
    let mut dims: Vec<String> = Vec::new();
    let mut cur = rest;
    while cur.starts_with('[') {
        let close = cur.find(']')?;
        dims.push(cur[1..close].to_string());
        cur = cur[close + 1..].trim_start();
    }
    if dims.is_empty() {
        return None; // scalar 1-bit port — not indexable as a lane vector
    }
    // Remaining text is the port name (strip trailing `,`/`)`/`;`).
    let name: String = cur
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    // Reject ports with a *trailing* (unpacked) dimension after the name,
    // e.g. `logic [7:0] foo [N]`. Those are genuine SystemVerilog unpacked
    // arrays — Verilator keeps them as C++ arrays (`VlUnpacked`), so the
    // HARC `dut.foo[i]` index must stay a real element subscript, NOT a
    // bit-extract. Excluding them from the map leaves the legacy direct-
    // index path in place (correct on both backends).
    let after_name = cur[name.len()..].trim_start();
    if after_name.starts_with('[') {
        return None;
    }
    // Pre-name dims only: the outer dim is the lane count (unused here);
    // the inner dim, if present, is the per-lane width. A single packed
    // dim ⇒ 1-bit lanes (also correct as a bit-select on a plain scalar).
    let lane_w = if dims.len() >= 2 {
        sv_range_width(&dims[dims.len() - 1], params)?
    } else {
        1
    };
    if lane_w == 0 {
        return None;
    }
    Some((name, lane_w))
}

/// Fold an SV packed range `"hi:lo"` (e.g. `"MASTER_ID_W-1:0"`, `"7:0"`)
/// to its bit width `hi-lo+1`, resolving param names and simple `-1`
/// offsets against `params`. Returns `None` if it can't fold concretely.
fn sv_range_width(range: &str, params: &std::collections::HashMap<String, i64>) -> Option<u32> {
    let (hi_s, lo_s) = range.split_once(':')?;
    let hi = eval_sv_index_expr(hi_s.trim(), params)?;
    let lo = eval_sv_index_expr(lo_s.trim(), params)?;
    if hi < lo {
        return None;
    }
    Some((hi - lo + 1) as u32)
}

/// Evaluate a tiny SV index expression: an integer, a param name, or
/// `<param-or-int> ± <int>` (covers `W-1`, `8`, `MASTER_ID_W`). Returns
/// `None` for anything more complex (e.g. multiplications) — the port is
/// then conservatively skipped.
fn eval_sv_index_expr(e: &str, params: &std::collections::HashMap<String, i64>) -> Option<i64> {
    let e = e.trim();
    if let Ok(v) = e.parse::<i64>() {
        return Some(v);
    }
    if let Some(&v) = params.get(e) {
        return Some(v);
    }
    for (op, sign) in [('+', 1i64), ('-', -1i64)] {
        if let Some(idx) = e.rfind(op) {
            // Avoid splitting a leading unary minus / empty lhs.
            if idx == 0 {
                continue;
            }
            let lhs = e[..idx].trim();
            let rhs = e[idx + 1..].trim();
            let lv = eval_sv_index_expr(lhs, params)?;
            let rv: i64 = rhs.parse().ok().or_else(|| params.get(rhs).copied())?;
            return Some(lv + sign * rv);
        }
    }
    None
}

/// If the test's `let dut : T` carries `probe ... at <path>` declarations,
/// return `(T's simple name, &probes)`. `None` for tests without probes —
/// callers skip the SV bind-stub emission entirely in that case.
pub fn dut_probes(file: &SourceFile) -> Option<(String, Vec<Probe>)> {
    let lowered = desugar_impl_for_test_in_file(file);
    let test = lowered.items.iter().find_map(|it| match it {
        Item::Test(t) => Some(t),
        _ => None,
    })?;
    for it in &test.items {
        if let TestItem::Let(l) = it {
            if l.name.name == "dut" && !l.probes.is_empty() {
                let ty = type_simple_name(l.ty.as_ref())?.to_string();
                return Some((ty, l.probes.clone()));
            }
        }
    }
    None
}

/// Emit a C++ Verilator TB for the single `test` declaration in this file.
/// Per-emit options. Currently just the `--mt` opt-in for Phase 3a's
/// per-actor OS thread topology; defaults to cooperative
/// (single-OS-thread, faster on real fixtures).
#[derive(Default, Clone)]
pub struct EmitOpts {
    /// Spawn one `std::thread` per bound coroutine actor with dual-
    /// barrier sync per posedge. Off → all coroutines share `sched`
    /// and tick cooperatively in the main thread.
    pub mt: bool,
    /// Map of DUT top-module port-name → per-lane bit-width for ports
    /// that flatten to a PACKED SystemVerilog vector (a `Vec<Bus, N>`
    /// or any multi-lane bus port). Populated from the `--sv` DUT port
    /// table (see `vec_lane_widths_from_sv`). When a HARC `dut.port[i]`
    /// index targets a port in this map, the codegen routes through
    /// `harc_rt::harc_vec_lane_{read,write}<W>` so the lane access works
    /// against Verilator's packed scalar (bit-extract) as well as the
    /// ARCH native sim's C++ array (direct index). Empty on the `--dut`
    /// path — there the existing direct `port[i]` array indexing is
    /// already correct.
    pub vec_lane_widths: std::collections::HashMap<String, u32>,
    /// DUT-port-level bus param overrides, keyed by DUT port name (which by
    /// convention equals the HARC `let <name> : Bus = bind dut` bind name and
    /// the flattened SV signal prefix). Inner map is `param-name → folded i64`.
    ///
    /// Sourced by parsing the DUT's `.arch`/`.archi` interface (see
    /// `dut_bus_port_overrides_from_files`). When a DUT module declares
    /// `port s: target BusRw<WRITE=0>`, `arch build` flattens `s` *without* the
    /// WRITE-gated channels, so a harc bus-bind on `s` must model the same port
    /// set. These overrides are layered onto the bus defaults (and any HARC-TB
    /// bind-site generic) at the `bus_param_envs` population sites — see
    /// `bus_param_env_with_port_override`. The DUT port's own override is
    /// authoritative for that port (it reflects what `arch build` actually
    /// synthesized), so it wins over a bind-site generic if both name the same
    /// param. Empty when no DUT interface is available, in which case the
    /// pre-existing conservative defaults-only behavior is unchanged.
    pub dut_bus_port_overrides:
        std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    /// `harc sim --cosim dpi` (spec §10 DPI-C co-sim pilot). When set,
    /// the emitted TB is a passive DPI-C runtime instead of a
    /// self-driving binary: `run_<Test>` becomes a driver coroutine
    /// that `co_yield`s time requests, `main()` is replaced by the
    /// `harc_cosim_init` / `harc_cosim_step` entrypoints, and DUT port
    /// access routes through the id-keyed accessor shim (see
    /// `runtime/harc_cosim_rt.h`) rather than Verilated member access.
    /// `None` (the default) leaves the direct-backend emission
    /// byte-identical.
    pub cosim: Option<CosimOpts>,
}

/// One DUT top-module port for co-sim accessor generation, discovered by
/// `cosim_ports_from_sv`. `sig_id` positions are shared between the
/// generated SV harness's accessor `case` tables and the TB shim's
/// `SigProxy<ID>` members — both are generated from the same vector.
#[derive(Debug, Clone)]
pub struct CosimPort {
    pub name: String,
    /// Total packed width; for unpacked-array ports, the ELEMENT width.
    pub width_bits: u32,
    pub is_input: bool,
    /// `Some(N)` for an unpacked-array port `p [N]` (single unpacked
    /// dimension, element width <= 64). Access goes through the
    /// element accessors instead of the scalar/word ones.
    pub unpacked_elems: Option<u32>,
}

/// One `probe` declaration routed through the co-sim accessors. The
/// probe's signals live in the bound `__harc_probe_<T>` stub (the same
/// stub the direct backend uses); the harness reaches them
/// hierarchically as `dut.harc_probes.<name>` and, for force probes,
/// `dut.harc_probes.<name>_drv` / `<name>_en`.
#[derive(Debug, Clone)]
pub struct CosimProbe {
    pub name: String,
    pub width_bits: u32,
    pub force: bool,
}

/// Options for `--cosim dpi` emission, shared by the TB emitter (shim +
/// entrypoints) and the harness generator in `main.rs`.
#[derive(Debug, Clone)]
pub struct CosimOpts {
    pub ports: Vec<CosimPort>,
    pub probes: Vec<CosimProbe>,
    /// Half period of the implicit TB clock in picoseconds. The direct
    /// backend's clockless drive loop has no physical time; co-sim needs
    /// one because the simulator owns a real timeline.
    pub half_period_ps: u64,
}

impl CosimOpts {
    /// Accessor ids for each probe, continuing after the port ids —
    /// `(read_id, Some((drv_id, en_id)))` for force probes. The harness
    /// case tables and the shim's `rootp` proxy members are both
    /// generated from this one assignment so they can never skew.
    pub fn probe_ids(&self) -> Vec<(usize, Option<(usize, usize)>)> {
        let mut next = self.ports.len();
        self.probes
            .iter()
            .map(|p| {
                let read = next;
                next += 1;
                let force = if p.force {
                    let drv = next;
                    let en = next + 1;
                    next += 2;
                    Some((drv, en))
                } else {
                    None
                };
                (read, force)
            })
            .collect()
    }
}

/// Scan the `--sv` sources for the `--top` module and return its full
/// port table (name, direction, total packed width). Same deliberately
/// tolerant line-based scan as `vec_lane_widths_from_sv` — ANSI-style
/// headers with foldable widths. Returns `None` when the top module
/// isn't found in any source. Ports whose width can't be folded, or
/// with unpacked (post-name) dimensions, are skipped — a TB referencing
/// a skipped port fails C++ compilation with a missing-member error.
pub fn cosim_ports_from_sv(
    sv_sources: &[std::path::PathBuf],
    top: &str,
) -> Option<Vec<CosimPort>> {
    for path in sv_sources {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some(ports) = scan_sv_module_ports(&src, top) {
            return Some(ports);
        }
    }
    None
}

/// Core of `cosim_ports_from_sv` over one source string:
///   - comment-strips each line (`//` tails and single-line `/* */`
///     spans) before any matching, so commented-out text can neither
///     declare ports nor fake the header-closing `);`;
///   - collects `parameter`/`localparam` values ONLY within the top
///     module's extent (a helper module in the same file re-declaring
///     the same parameter name must not re-size the top's ports);
///     typedefs are collected file-wide (file-scope typedefs above the
///     module are legal and used by the DUT corpus);
///   - folds values with a small const evaluator (`+ - * / ( )`,
///     `$clog2`), handles multiple declarators per line
///     (`parameter W = 8, D = 16`), resolves `typedef logic [..] T` /
///     `typedef struct packed {…} T` / `parameter type T = …`;
///   - scans port declarations only inside the ANSI header (module
///     line to the header-closing `);`), with a whole-body fallback
///     for non-ANSI headers, and reports every skipped
///     `input`/`output` line on stderr — a silently absent port is the
///     failure mode this scanner must never have.
fn scan_sv_module_ports(src: &str, top: &str) -> Option<Vec<CosimPort>> {
    let raw_lines: Vec<&str> = src.lines().collect();
    let lines: Vec<String> = raw_lines.iter().map(|l| strip_sv_line_comments(l)).collect();
    let start = lines.iter().position(|l| {
        let t = l.trim_start();
        if let Some(rest) = t.strip_prefix("module ") {
            rest.trim_start().strip_prefix(top).is_some_and(|after| {
                after
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_')
            })
        } else {
            false
        }
    })?;
    let body_end = lines[start..]
        .iter()
        .position(|l| l.trim_start().starts_with("endmodule"))
        .map(|i| start + i)
        .unwrap_or(lines.len());
    let body = &lines[start..body_end];

    // ANSI header extent: everything up to (and including) the first
    // line whose (comment-stripped) content contains the closing `);`.
    let header_end = body
        .iter()
        .position(|l| l.contains(");"))
        .map(|i| i + 1)
        .unwrap_or(body.len());

    // Pass 1a (top module extent only): parameters. `parameter type`
    // goes to the typedef table; everything else through the const
    // evaluator, one or more declarators per line.
    let mut params: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut typedefs: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for l in body {
        let t = l.trim();
        for kw in [
            "parameter type ",
            "localparam type ",
            "parameter int ",
            "parameter ",
            "localparam int ",
            "localparam ",
        ] {
            let Some(rest) = t.strip_prefix(kw) else {
                continue;
            };
            if kw.contains("type") {
                if let Some(eq) = rest.find('=') {
                    let name = rest[..eq].trim();
                    let name = name
                        .split_whitespace()
                        .last()
                        .unwrap_or(name)
                        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
                    if let Some((_, w)) =
                        sv_typedef_width(&format!("{} {name};", rest[eq + 1..].trim()), &params)
                    {
                        typedefs.insert(name.to_string(), w);
                    }
                }
            } else {
                // One or more `NAME = <expr>` declarators, comma-
                // separated at paren depth zero.
                for decl in split_sv_top_level_commas(rest) {
                    let Some(eq) = decl.find('=') else { continue };
                    let name = decl[..eq].trim();
                    let name = name
                        .split_whitespace()
                        .last()
                        .unwrap_or(name)
                        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
                    let mut val = decl[eq + 1..].trim();
                    if let Some(cut) = val.find(';') {
                        val = &val[..cut];
                    }
                    let val = val.trim();
                    // The last parameter before the header's `) (` can
                    // carry the closing paren on the same line — retry
                    // with it stripped when the raw text doesn't
                    // evaluate.
                    let v = eval_sv_const_expr(val, &params).or_else(|| {
                        eval_sv_const_expr(val.trim_end_matches(')'), &params)
                    });
                    if let Some(v) = v {
                        params.insert(name.to_string(), v);
                    }
                }
            }
            break;
        }
    }

    // Pass 1b (whole file): typedefs, including multi-line
    // `typedef struct packed { ... } Name;` blocks (member widths sum).
    let mut struct_accum: Option<u64> = None;
    for l in &lines {
        let t = l.trim();
        if let Some(acc) = struct_accum {
            if let Some(rest) = t.strip_prefix('}') {
                let name: String = rest
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() && acc > 0 {
                    typedefs.insert(name, acc.min(u32::MAX as u64) as u32);
                }
                struct_accum = None;
            } else if let Some((_, w)) = sv_typedef_width(t, &params) {
                struct_accum = Some(acc + w as u64);
            } else {
                // Unparseable member — poison the accumulated width so
                // the struct never gets a wrong size.
                struct_accum = Some(0);
            }
            continue;
        }
        if t.starts_with("typedef struct packed") {
            struct_accum = Some(0);
            continue;
        }
        if let Some(rest) = t.strip_prefix("typedef ") {
            if let Some((name, w)) = sv_typedef_width(rest, &params) {
                typedefs.insert(name, w);
            }
        }
    }

    // Pass 2: port declarations (header first, whole body as fallback).
    // Every `input`/`output` line that yields no port is recorded — a
    // silently absent port surfaces later only as a confusing
    // missing-member C++ error (or not at all).
    let scan_ports = |range: &[String]| -> (Vec<CosimPort>, Vec<String>) {
        let mut out = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for l in range {
            let t = l.trim();
            let (is_input, after_dir) = if let Some(r) = t.strip_prefix("input ") {
                (true, r)
            } else if let Some(r) = t.strip_prefix("output ") {
                (false, r)
            } else {
                if t.starts_with("inout ") {
                    skipped.push(format!("inout port unsupported: `{t}`"));
                }
                continue; // non-port line
            };
            // Strip net/var/base-type qualifiers to a fixpoint so
            // `input wire logic x` and `output var logic y` parse. A
            // sized builtin base type contributes a default width.
            let mut rest = after_dir.trim();
            let mut base_width: Option<u32> = None;
            loop {
                let mut stripped_any = false;
                for (q, w) in [
                    ("logic", Some(1u32)),
                    ("wire", Some(1)),
                    ("reg", Some(1)),
                    ("bit", Some(1)),
                    ("var", None),
                    ("signed", None),
                    ("unsigned", None),
                    ("integer", Some(32)),
                    ("int", Some(32)),
                    ("longint", Some(64)),
                    ("shortint", Some(16)),
                    ("byte", Some(8)),
                ] {
                    if let Some(r) = rest.strip_prefix(q) {
                        if r.starts_with(|c: char| c.is_whitespace() || c == '[')
                            || r.is_empty()
                        {
                            if let Some(w) = w {
                                base_width = Some(base_width.map_or(w, |b| b.max(w)));
                            }
                            rest = r.trim_start();
                            stripped_any = true;
                        }
                    }
                }
                if !stripped_any {
                    break;
                }
            }
            // A leading identifier now is either the port name itself
            // (`input clk,`) or a user type (`input DATA name`). It is
            // a type only when another identifier follows (past any
            // packed dims): resolve it through the typedef table, or
            // skip the port when the type's width is unknown.
            let mut type_width: Option<u32> = None;
            if rest.starts_with(|c: char| c.is_alphabetic() || c == '_') {
                let tok: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let after_tok = rest[tok.len()..].trim_start();
                let mut peek = after_tok;
                while peek.starts_with('[') {
                    match peek.find(']') {
                        Some(c) => peek = peek[c + 1..].trim_start(),
                        None => break,
                    }
                }
                if peek.starts_with(|c: char| c.is_alphabetic() || c == '_') {
                    match typedefs.get(&tok) {
                        Some(&w) => {
                            type_width = Some(w);
                            rest = after_tok;
                        }
                        None => {
                            skipped.push(format!(
                                "unresolved port type `{tok}`: `{t}`"
                            ));
                            continue;
                        }
                    }
                }
            }
            // Total width = base width (typedef, sized builtin, or the
            // 1-bit scalar default) × the product of packed dims. For
            // the common `logic [hi:lo]` shape that is 1 × (hi-lo+1);
            // for a typedef'd port with dims it is the typedef width
            // per lane × lane count.
            let base: u64 = type_width.or(base_width).unwrap_or(1) as u64;
            let mut dims_product: u64 = 1;
            let mut cur = rest;
            let mut foldable = true;
            while cur.starts_with('[') {
                let Some(close) = cur.find(']') else {
                    foldable = false;
                    break;
                };
                match sv_const_range_width(&cur[1..close], &params) {
                    Some(w) => dims_product = dims_product.saturating_mul(w as u64),
                    None => foldable = false,
                }
                cur = cur[close + 1..].trim_start();
            }
            if !foldable {
                skipped.push(format!("unfoldable port width: `{t}`"));
                continue;
            }
            let width: u64 = base.saturating_mul(dims_product);
            // One or more comma-separated names, each with an optional
            // unpacked dimension (`input logic a, b, c` / `... x [N]`).
            for name_decl in split_sv_top_level_commas(cur) {
                let nd = name_decl
                    .trim()
                    .trim_end_matches([';', ')'])
                    .trim();
                if nd.is_empty() {
                    continue;
                }
                let name: String = nd
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() || !nd.starts_with(|c: char| c.is_alphabetic() || c == '_')
                {
                    skipped.push(format!("unparsed port declarator `{nd}` in: `{t}`"));
                    continue;
                }
                let after_name = nd[name.len()..].trim_start();
                let mut unpacked_elems = None;
                if after_name.starts_with('[') {
                    // Single unpacked dimension, `[N]` or `[a:b]`
                    // (either order), elements <= 64 bits.
                    let Some(close) = after_name.find(']') else {
                        skipped.push(format!("unterminated unpacked dim: `{t}`"));
                        continue;
                    };
                    let dim = &after_name[1..close];
                    let elems = match dim.split_once(':') {
                        Some(_) => sv_const_range_width(dim, &params),
                        None => eval_sv_const_expr(dim.trim(), &params)
                            .and_then(|v| u32::try_from(v).ok()),
                    };
                    let Some(elems) = elems else {
                        skipped.push(format!("unfoldable unpacked dim `{dim}`: `{t}`"));
                        continue;
                    };
                    if elems == 0
                        || width > 64
                        || after_name[close + 1..].trim_start().starts_with('[')
                    {
                        skipped.push(format!(
                            "unsupported unpacked-array shape (multi-dim or >64-bit \
                             elements): `{t}`"
                        ));
                        continue;
                    }
                    unpacked_elems = Some(elems);
                } else if !after_name.is_empty() {
                    skipped.push(format!("unparsed port declarator tail `{after_name}`: `{t}`"));
                    continue;
                }
                out.push(CosimPort {
                    name,
                    width_bits: width.min(u32::MAX as u64) as u32,
                    is_input,
                    unpacked_elems,
                });
            }
        }
        (out, skipped)
    };

    let (mut out, mut skipped) = scan_ports(&body[..header_end]);
    if out.is_empty() {
        (out, skipped) = scan_ports(body);
    }
    if out.is_empty() {
        return None;
    }
    // A silently absent port is this scanner's worst failure mode:
    // surface every skip loudly on stderr (the TB may still build if it
    // never touches the port; if it does, the C++ error names it).
    for w in &skipped {
        eprintln!("harc: --cosim dpi: module `{top}`: skipped port — {w}");
    }
    Some(out)
}

/// Strip `//` line comments and single-line `/* ... */` block comments.
/// A block comment left OPEN on the line truncates the line at the
/// opener (the continuation is unparseable line-by-line anyway; the
/// affected declarations then skip loudly rather than mis-parse).
fn strip_sv_line_comments(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            break;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            match line[i + 2..].find("*/") {
                Some(close) => {
                    out.push(' ');
                    i = i + 2 + close + 2;
                    continue;
                }
                None => break,
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// Split on commas at paren/bracket depth zero — used for
/// multi-declarator parameter lines and multi-name port declarations.
fn split_sv_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth <= 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// `logic [hi:lo]... Name` (the tail of a typedef or a `parameter type`
/// value) → `(Name, total packed width)`.
fn sv_typedef_width(
    decl: &str,
    params: &std::collections::HashMap<String, i64>,
) -> Option<(String, u32)> {
    let mut rest = decl.trim();
    let mut saw_base = false;
    for q in ["logic", "wire", "reg", "bit", "signed", "unsigned"] {
        if let Some(r) = rest.strip_prefix(q) {
            if r.starts_with(|c: char| c.is_whitespace() || c == '[') || r.starts_with(';') {
                rest = r.trim_start();
                saw_base = true;
            }
        }
    }
    if !saw_base {
        return None;
    }
    let mut width: u64 = 1;
    while rest.starts_with('[') {
        let close = rest.find(']')?;
        width = width.saturating_mul(sv_const_range_width(&rest[1..close], params)? as u64);
        rest = rest[close + 1..].trim_start();
    }
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    Some((name, width.min(u32::MAX as u64) as u32))
}

/// `[hi:lo]` width via the richer const evaluator.
fn sv_const_range_width(
    range: &str,
    params: &std::collections::HashMap<String, i64>,
) -> Option<u32> {
    let (a_s, b_s) = range.split_once(':')?;
    let a = eval_sv_const_expr(a_s.trim(), params)?;
    let b = eval_sv_const_expr(b_s.trim(), params)?;
    // Order-agnostic: `[7:0]` and `[0:7]` are both 8 wide (ascending
    // ranges are the common style for unpacked dims).
    Some((a - b).unsigned_abs().saturating_add(1).min(u32::MAX as u64) as u32)
}

/// Small recursive-descent evaluator for SV constant expressions in
/// module headers: integers, parameter names, `+ - * /`, parentheses,
/// and `$clog2(...)`. Anything else folds to `None` (the port is then
/// skipped, never mis-sized).
fn eval_sv_const_expr(e: &str, params: &std::collections::HashMap<String, i64>) -> Option<i64> {
    struct P<'a> {
        s: &'a [u8],
        i: usize,
        params: &'a std::collections::HashMap<String, i64>,
    }
    impl<'a> P<'a> {
        fn ws(&mut self) {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }
        fn expr(&mut self) -> Option<i64> {
            let mut v = self.term()?;
            loop {
                self.ws();
                match self.s.get(self.i) {
                    Some(b'+') => {
                        self.i += 1;
                        v += self.term()?;
                    }
                    Some(b'-') => {
                        self.i += 1;
                        v -= self.term()?;
                    }
                    _ => return Some(v),
                }
            }
        }
        fn term(&mut self) -> Option<i64> {
            let mut v = self.atom()?;
            loop {
                self.ws();
                match self.s.get(self.i) {
                    Some(b'*') => {
                        self.i += 1;
                        v *= self.atom()?;
                    }
                    Some(b'/') => {
                        self.i += 1;
                        let d = self.atom()?;
                        if d == 0 {
                            return None;
                        }
                        v /= d;
                    }
                    _ => return Some(v),
                }
            }
        }
        fn atom(&mut self) -> Option<i64> {
            self.ws();
            match self.s.get(self.i)? {
                b'(' => {
                    self.i += 1;
                    let v = self.expr()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b')') {
                        return None;
                    }
                    self.i += 1;
                    Some(v)
                }
                b'-' => {
                    self.i += 1;
                    Some(-self.atom()?)
                }
                b'$' => {
                    let rest = &self.s[self.i..];
                    if !rest.starts_with(b"$clog2") {
                        return None;
                    }
                    self.i += 6;
                    self.ws();
                    if self.s.get(self.i) != Some(&b'(') {
                        return None;
                    }
                    self.i += 1;
                    let v = self.expr()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b')') {
                        return None;
                    }
                    self.i += 1;
                    if v <= 0 {
                        return Some(0);
                    }
                    Some(64 - ((v - 1) as u64).leading_zeros() as i64)
                }
                c if c.is_ascii_digit() => {
                    let mut v: i64 = 0;
                    while let Some(c) = self.s.get(self.i) {
                        if c.is_ascii_digit() {
                            v = v.checked_mul(10)?.checked_add((c - b'0') as i64)?;
                            self.i += 1;
                        } else if *c == b'\'' {
                            // Sized literal (`32'h...`): too rare in
                            // header widths to justify parsing.
                            return None;
                        } else {
                            break;
                        }
                    }
                    Some(v)
                }
                c if c.is_ascii_alphabetic() || *c == b'_' => {
                    let start = self.i;
                    while let Some(c) = self.s.get(self.i) {
                        if c.is_ascii_alphanumeric() || *c == b'_' {
                            self.i += 1;
                        } else {
                            break;
                        }
                    }
                    let name = std::str::from_utf8(&self.s[start..self.i]).ok()?;
                    self.params.get(name).copied()
                }
                _ => None,
            }
        }
    }
    let mut p = P {
        s: e.as_bytes(),
        i: 0,
        params,
    };
    let v = p.expr()?;
    p.ws();
    if p.i != p.s.len() {
        return None;
    }
    Some(v)
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

pub fn emit(file: &SourceFile) -> Result<String, EmitError> {
    emit_with_opts(file, EmitOpts::default())
}

/// Emit a dispatcher plus one or more generated C++ translation units for
/// the tests, so Verilator/Make can compile and recompile them
/// independently.
///
/// Each shard is a self-contained translation unit: `emit_with_opts` over a
/// source filtered to that shard's tests, with the dispatcher `main()`
/// stripped off. The full file-scope scaffolding (the `HarcTestContext`
/// struct, `static`/template helpers, `harc_rng`, …) is re-emitted into every
/// shard, but it all has internal linkage, so the shards link cleanly
/// alongside the generated `main.cpp` dispatcher — the only external symbols
/// are the per-test `run_<TestName>` functions (each unique to one shard) and
/// `main`.
///
/// Incremental granularity comes from `write_if_changed` at the call site
/// (`src/main.rs`): a shard's emitted bytes depend only on the tests it
/// contains, so editing one test leaves every other shard byte-identical and
/// Make skips their objects. `group_size` (default 4) trades that granularity
/// against the per-translation-unit cost of re-parsing the shared
/// scaffolding: `group_size == 1` emits one `test_<name>.cpp` per test (finest
/// granularity); larger groups bundle `group_size` tests per `shard<N>.cpp`.
pub fn emit_split_tests(file: &SourceFile, opts: EmitOpts) -> Result<SplitCppOutput, EmitError> {
    emit_split_tests_with_file_prefix(file, opts, "", 1)
}

pub fn emit_split_tests_with_file_prefix(
    file: &SourceFile,
    opts: EmitOpts,
    file_prefix: &str,
    group_size: usize,
) -> Result<SplitCppOutput, EmitError> {
    let group_size = group_size.max(1);
    let lowered = desugar_impl_for_test_in_file(file);
    let test_names: Vec<String> = lowered
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Test(t) => Some(t.name.name.clone()),
            _ => None,
        })
        .collect();
    if test_names.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }
    validate_split_tests_share_dut(&lowered, &test_names)?;

    let shard_count = test_names.len().div_ceil(group_size);
    let mut files = Vec::with_capacity(shard_count + 1);
    files.push(GeneratedCppFile {
        filename: format!("{file_prefix}main.cpp"),
        contents: emit_split_dispatcher(&test_names),
    });

    for (shard_idx, shard_names) in test_names.chunks(group_size).enumerate() {
        let shard_file = filter_source_to_tests(&lowered, shard_names);
        let cpp = emit_with_opts(&shard_file, opts.clone())?;
        // One test per shard reads better as `test_<name>.cpp`; a bundled
        // shard has no single owning test, so it gets `shard<N>.cpp`.
        let filename = if group_size == 1 {
            format!(
                "{file_prefix}test_{}.cpp",
                sanitize_file_component(&shard_names[0])
            )
        } else {
            format!("{file_prefix}shard{}.cpp", shard_idx + 1)
        };
        files.push(GeneratedCppFile {
            filename,
            contents: strip_generated_dispatcher(&cpp)?,
        });
    }

    Ok(SplitCppOutput { files, test_names })
}

fn filter_source_to_tests(file: &SourceFile, test_names: &[String]) -> SourceFile {
    let items = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Test(t) if test_names.iter().any(|name| name == &t.name.name) => {
                Some(item.clone())
            }
            Item::Test(_) => None,
            _ => Some(item.clone()),
        })
        .collect();
    SourceFile {
        items,
        inner_doc: file.inner_doc.clone(),
        frontmatter: file.frontmatter.clone(),
    }
}

fn strip_generated_dispatcher(cpp: &str) -> Result<String, EmitError> {
    let marker = "\nint main(int argc, char** argv) {";
    let Some(idx) = cpp.rfind(marker) else {
        return Err(EmitError(
            "internal error: generated single-test C++ did not contain dispatcher main()".into(),
        ));
    };
    let mut out = cpp[..idx].trim_end().to_string();
    out.push('\n');
    Ok(out)
}

fn emit_split_dispatcher(test_names: &[String]) -> String {
    let mut out = String::new();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(out, "// HARC split-test dispatcher.").ok();
    writeln!(out).ok();
    writeln!(out, "#include <cstring>").ok();
    writeln!(out, "#include \"harc_log_rt.h\"").ok();
    writeln!(out).ok();
    for name in test_names {
        writeln!(out, "extern int run_{name}(int argc, char** argv);").ok();
    }
    writeln!(out).ok();
    writeln!(out, "int main(int argc, char** argv) {{").ok();
    writeln!(
        out,
        "{INDENT}const char* test_sel = harc_rt::log::harc_select_test(argc, argv);"
    )
    .ok();
    let first_test = &test_names[0];
    writeln!(
        out,
        "{INDENT}if (!test_sel) return run_{first_test}(argc, argv);"
    )
    .ok();
    for name in test_names {
        writeln!(
            out,
            "{INDENT}if (std::strcmp(test_sel, \"{name}\") == 0) return run_{name}(argc, argv);"
        )
        .ok();
    }
    let avail_csv = test_names.join(", ");
    writeln!(
        out,
        "{INDENT}harc_rt::log::harc_report_unknown_test(test_sel, \"{avail_csv}\");"
    )
    .ok();
    writeln!(out, "{INDENT}return 1;").ok();
    writeln!(out, "}}").ok();
    out
}

fn validate_split_tests_share_dut(
    file: &SourceFile,
    test_names: &[String],
) -> Result<(), EmitError> {
    let mut shared_dut_type: Option<&str> = None;
    for item in &file.items {
        let Item::Test(test) = item else { continue };
        if !test_names.iter().any(|name| name == &test.name.name) {
            continue;
        }
        for test_item in &test.items {
            let TestItem::Let(l) = test_item else {
                continue;
            };
            if l.name.name != "dut" {
                continue;
            }
            let ty_name = type_simple_name(l.ty.as_ref()).ok_or_else(|| {
                EmitError("`let dut : <Type>` must use a simple named type".into())
            })?;
            match shared_dut_type {
                Some(prev) if prev != ty_name => {
                    return Err(EmitError(format!(
                        "multi-DUT tests in one split binary are out of scope; \
                         test `{}` uses `{}`, but a previous test used `{}`",
                        test.name.name, ty_name, prev,
                    )));
                }
                _ => shared_dut_type = Some(ty_name),
            }
        }
    }
    if shared_dut_type.is_none() {
        return Err(EmitError(
            "expected `let dut : <Type>` declaration in test body".into(),
        ));
    }
    Ok(())
}

pub(crate) fn sanitize_file_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "test".to_string()
    } else {
        out
    }
}

/// Build a *focused* `Emitter` for the TB-IR backend's randomize seam.
///
/// The tbir backend (`codegen/tbir`) lowers `randomize` through
/// `Terminator::Randomize`, but the actual Z3-solve emission is v1's
/// (`emit_constraint_solver_block` & friends) — "the constraint runtime
/// is shared; only the call site moves to the IR backend". This builds
/// the minimal Emitter those methods read from: transaction field
/// metadata, enum domains, relation declarations, and the runtime
/// problem-id table. Everything else is empty — the randomize emission
/// touches no other Emitter state. (Listing every field explicitly is
/// deliberate: a future field addition fails to compile here, forcing a
/// reviewer to decide whether the randomize path needs it.)
/// Normalize `Vec<Entry, N>` element type-args on struct/transaction
/// FIELD declarations (harc#522). The parser's type-arg heuristic parses
/// a bare NAMED element as `TypeArg::Expr(Ident)` (only builtin heads
/// parse as `TypeArg::Type`), a shape `fixed_vec_type_args` — and with it
/// the member-type, default, packed-width, and pack-helper walks — cannot
/// see: the field silently fell through to the scalar `uint64_t` mapping
/// and every real use miscompiled in clang. Rewriting the element to
/// `TypeArg::Type(TypeExpr::Named)` when the ident names a
/// struct/transaction in the file routes a record-element `Vec` field
/// through the SAME emission path as a scalar-element one
/// (`std::array<Entry, N>` member, `{}` default, per-element
/// `harc_pack_Entry` packing). Scoped to record field declarations —
/// every other type position keeps the parser's shape.
fn normalize_vec_record_elem_fields(file: &SourceFile) -> SourceFile {
    let record_names: std::collections::HashSet<&str> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Struct(s) => Some(s.name.name.as_str()),
            Item::Transaction(t) => Some(t.name.name.as_str()),
            _ => None,
        })
        .collect();
    fn normalize_ty(ty: &mut TypeExpr, record_names: &std::collections::HashSet<&str>) {
        let TypeExpr::Builtin {
            name: BuiltinTy::Vec,
            args,
            ..
        } = ty
        else {
            return;
        };
        let Some(first) = args.first_mut() else {
            return;
        };
        let TypeArg::Expr(e) = first else { return };
        let ExprKind::Ident(id) = &*e.kind else {
            return;
        };
        if !record_names.contains(id.name.as_str()) {
            return;
        }
        *first = TypeArg::Type(TypeExpr::Named {
            name: Path {
                segments: vec![id.clone()],
                span: e.span,
            },
            generics: Vec::new(),
            mode: None,
            span: e.span,
        });
    }
    let mut out = file.clone();
    for it in &mut out.items {
        match it {
            Item::Struct(s) => {
                for f in &mut s.fields {
                    normalize_ty(&mut f.ty, &record_names);
                }
                for b in &mut s.body {
                    if let TxnBodyItem::Field(f) = b {
                        normalize_ty(&mut f.ty, &record_names);
                    }
                }
            }
            Item::Transaction(t) => {
                for b in &mut t.body {
                    if let TxnBodyItem::Field(f) = b {
                        normalize_ty(&mut f.ty, &record_names);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn build_randomize_emitter(file: &SourceFile, opts: &EmitOpts) -> Emitter {
    let file = &normalize_vec_record_elem_fields(file);
    // The tbir backend already desugars impl-for before lowering, so the
    // `file` handed here is classic-form; desugar again is idempotent and
    // keeps the problem-table spans aligned with v1.
    let file = desugar_impl_for_test_in_file(file);
    let file = &file;

    let typed_solver_problem_table =
        crate::solver::problem_table::build_typed_solver_problem_table(file);
    let mut runtime_randomize_problem_ids = std::collections::HashMap::new();
    for entry in &typed_solver_problem_table.entries {
        let crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite { span, .. } =
            entry.source
        else {
            continue;
        };
        if let crate::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. } =
            &entry.build
        {
            runtime_randomize_problem_ids.insert((span.start, span.end), typed.problem_id.0);
        }
    }

    // Record-field metadata: same collection the main emission path runs
    // (record_fields/record_bodies → flatten_*_field_infos), restricted
    // to the inputs the constraint solver block consumes.
    let mut record_fields: std::collections::HashMap<String, Vec<Field>> =
        std::collections::HashMap::new();
    let mut enum_domains: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut enums: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut enum_variants: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut consts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut relations: std::collections::HashMap<String, RelationDecl> =
        std::collections::HashMap::new();
    let mut const_widths: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for it in &file.items {
        match it {
            Item::Const(c) => {
                if let ExprKind::Int(text) = &*c.value.kind {
                    consts.insert(c.name.name.clone(), text.clone());
                    if let Some(w) =
                        c.ty.as_ref()
                            .and_then(declared_type_bit_width)
                            .or_else(|| literal_operand_bit_width(text))
                    {
                        const_widths.insert(c.name.name.clone(), w);
                    }
                }
            }
            Item::Enum(e) => {
                enums.insert(e.name.name.clone(), e.variants.len());
                enum_domains.insert(
                    e.name.name.clone(),
                    e.variants.iter().map(|v| v.name.clone()).collect(),
                );
                for (i, v) in e.variants.iter().enumerate() {
                    enum_variants.entry(v.name.clone()).or_insert(i as i64);
                }
            }
            Item::Relation(r) => {
                relations.insert(r.name.name.clone(), r.clone());
            }
            _ => {}
        }
    }
    for it in &file.items {
        match it {
            Item::Struct(s) => {
                record_fields.insert(s.name.name.clone(), s.fields.clone());
            }
            Item::Transaction(t) => {
                record_fields.insert(t.name.name.clone(), txn_direct_fields(&t.body));
            }
            _ => {}
        }
    }
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    for it in &file.items {
        match it {
            Item::Transaction(t) => {
                txn_fields.insert(
                    t.name.name.clone(),
                    flatten_txn_body_field_infos(&t.body, &record_fields, &enums, &enum_domains),
                );
            }
            Item::Struct(s) => {
                txn_fields.insert(
                    s.name.name.clone(),
                    flatten_record_field_infos(&s.fields, &record_fields, &enums, &enum_domains),
                );
            }
            _ => {}
        }
    }

    Emitter {
        out: String::new(),
        errors: Vec::new(),
        txn_fields,
        record_fields,
        txn_keeps: std::collections::HashMap::new(),
        probes: std::collections::HashMap::new(),
        probe_widths: std::collections::HashMap::new(),
        shadowed_lets: std::collections::HashSet::new(),
        regblocks: std::collections::HashMap::new(),
        addrmaps: std::collections::HashMap::new(),
        let_helper: std::collections::HashMap::new(),
        runtime_randomize_problem_ids,
        relations,
        pointer_vars: std::collections::HashSet::new(),
        let_types: std::collections::HashMap::new(),
        let_widths: std::collections::HashMap::new(),
        vec_lane_widths: opts.vec_lane_widths.clone(),
        let_modes: std::collections::HashMap::new(),
        transactions: std::collections::HashSet::new(),
        structs: std::collections::HashSet::new(),
        scoreboards: std::collections::HashSet::new(),
        covergroups: std::collections::HashMap::new(),
        covers: Vec::new(),
        field_subs: std::collections::HashMap::new(),
        enums,
        enum_variants,
        consts,
        const_widths,
        properties: std::collections::HashMap::new(),
        prop_subs: std::collections::HashMap::new(),
        event_types: std::collections::HashMap::new(),
        clock_names: Vec::new(),
        current_yield_target: None,
        tseq_names: std::collections::HashSet::new(),
        components: std::collections::HashMap::new(),
        buses: std::collections::HashMap::new(),
        bus_bindings: std::collections::HashMap::new(),
        bus_param_envs: std::collections::HashMap::new(),
        dut_bus_port_overrides: opts.dut_bus_port_overrides.clone(),
        bus_remap: std::collections::HashMap::new(),
        pending_tlm_forks: Vec::new(),
        next_tlm_fork_tag: std::collections::HashMap::new(),
        in_coroutine: false,
        actor_threads: Vec::new(),
        mt: opts.mt,
        driver_bus_for_hookables: std::collections::HashMap::new(),
        transactors: std::collections::HashMap::new(),
        current_component_instance: None,
        current_component_method: None,
    }
}

/// Emit the v1 Z3-solve C++ snippet for each TB-IR `ConstraintSite`, in
/// table order. The returned `Vec` is indexed by `ConstraintRef.0`, so a
/// `Terminator::Randomize { constraints: ConstraintRef(i), .. }` splices
/// in `snippets[i]`. Each snippet is emitted at `depth` indentation
/// levels and is byte-identical to what v1 emits at the same site (the
/// constraints arrive pre-merged from lowering — transaction keeps ahead
/// of the `with {...}` body — so this path runs v1's dispatch without
/// re-doing the merge). `Err` if any site reports an emission error.
pub fn emit_randomize_snippets(
    file: &SourceFile,
    opts: &EmitOpts,
    sites: &[crate::ir::ConstraintSite],
    depth: usize,
) -> Result<Vec<String>, EmitError> {
    if sites.is_empty() {
        return Ok(Vec::new());
    }
    let mut e = build_randomize_emitter(file, opts);
    let mut out = Vec::with_capacity(sites.len());
    for site in sites {
        e.out.clear();
        e.errors.clear();
        e.emit_randomize_for_site(site, depth);
        if let Some(err) = e.errors.first() {
            return Err(EmitError(format!("tbir randomize({}): {err}", site.record)));
        }
        out.push(std::mem::take(&mut e.out));
    }
    Ok(out)
}

pub fn emit_with_opts(file: &SourceFile, opts: EmitOpts) -> Result<String, EmitError> {
    // Desugar `impl <name> for <TbType>` tests into the classic
    // `test <name>` form before any other emission work runs. The
    // testbench-bound form (docs/test-ergonomics.md §3.3) folds a
    // testbench's fields + helper methods into the bound test's
    // scope; desugaring synthesizes the equivalent `let dut : ...` /
    // `let _tb : <TbType>` declarations at test scope and rewrites
    // bare-name references inside run/setup/check/teardown bodies
    // to `_tb.<field>` / `<TbType>_<method>(_tb, ...)`. Once
    // desugared, the test looks identical to a classic-form test
    // and threads through the existing pipeline unchanged.
    let file = desugar_impl_for_test_in_file(file);
    // Route `Vec<Record, N>` fields through the scalar-`Vec` emission
    // path (see `normalize_vec_record_elem_fields`, harc#522).
    let file = normalize_vec_record_elem_fields(&file);
    let file = &file;
    let typed_solver_problem_table =
        crate::solver::problem_table::build_typed_solver_problem_table(file);
    let mut runtime_randomize_problem_ids = std::collections::HashMap::new();
    for entry in &typed_solver_problem_table.entries {
        let crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite { span, .. } =
            entry.source
        else {
            continue;
        };
        if let crate::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. } =
            &entry.build
        {
            runtime_randomize_problem_ids.insert((span.start, span.end), typed.problem_id.0);
        }
    }
    let runtime_problem_table =
        crate::solver::runtime::RuntimeProblemTable::from_typed_solver_table(
            &typed_solver_problem_table,
        );

    // Collect ALL `test` items — every one becomes a `run_<TestName>`
    // function in the emitted binary, and the dispatcher `main()` at
    // the end picks one based on `--test <name>` or the `HARC_TEST`
    // env var (Phase 1b of docs/separate-compilation-plan.md).
    // Multi-test in one binary lets `harc sim --test foo` then
    // `harc sim --test bar` share Verilator output (the .cpp is
    // byte-identical across selectors).
    let tests: Vec<&TestDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Test(t) => Some(t),
            _ => None,
        })
        .collect();
    if tests.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }

    // Validate: all tests in one binary must agree on the DUT type
    // (Verilator builds one V<top> per build; multi-DUT requires
    // separate builds, out of scope for v0). Aggregate the probe
    // accessors across tests so the root-header include gate sees
    // every test's probes — without this, a test with probes that
    // wasn't the first one in source order would compile against a
    // V<Top>.h that didn't pull in V<Top>___024root.h.
    let mut shared_dut_type: Option<&str> = None;
    let mut aggregated_probes: std::collections::HashMap<String, ProbeAccessor> =
        std::collections::HashMap::new();
    for t in &tests {
        for it in &t.items {
            if let TestItem::Let(l) = it {
                if l.name.name == "dut" {
                    let ty_name = type_simple_name(l.ty.as_ref()).ok_or_else(|| {
                        EmitError("`let dut : <Type>` must use a simple named type".into())
                    })?;
                    match shared_dut_type {
                        Some(prev) if prev != ty_name => {
                            return Err(EmitError(format!(
                                "multi-DUT tests in one binary are out of scope for v0; \
                                 test `{}` uses `{}`, but a previous test used `{}`",
                                t.name.name, ty_name, prev,
                            )));
                        }
                        _ => {
                            shared_dut_type = Some(ty_name);
                        }
                    }
                    for p in &l.probes {
                        aggregated_probes
                            .entry(p.name.name.clone())
                            .or_insert_with(|| ProbeAccessor {
                                read: crate::codegen::sv_stub::mangled_accessor(
                                    ty_name,
                                    &p.name.name,
                                ),
                                force: p.force,
                            });
                    }
                }
            }
        }
    }
    let dut_type: &str = shared_dut_type
        .ok_or_else(|| EmitError("expected `let dut : <Type>` declaration in test body".into()))?;

    // Per-test metadata (custom_phases, other_lets, ...) is derived
    // inside the per-test emission loop further down. The few
    // file-scope code paths that previously referenced `test_decl`
    // (e.g. when computing tseq/funcs/etc.) keep working off any of
    // the tests — those collections walk `file.items`, not
    // `test.items`. We bind `test_decl` to the alphabetically-first
    // test only so leftover single-test references compile during
    // the staged migration.
    let _test_decl = tests[0];

    // Collect top-level functions — emitted as lambdas inside main() so they
    // can capture `dut` and `tick` lexically.
    let funcs: Vec<&FunctionDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Function(f) => Some(f),
            _ => None,
        })
        .collect();

    // Collect `tseq` declarations — same hoisting strategy as functions,
    // emitted as `std::function`-shaped lambdas returning `std::vector<T>`
    // built up via `yield`.
    let tseqs: Vec<&TseqDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Tseq(t) => Some(t),
            _ => None,
        })
        .collect();

    // Index `domain Foo freq_mhz: N end domain Foo` decls so a `clock X =
    // Foo` reference can resolve N to a wall-clock period (1/N µs → ps).
    let mut domains: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Domain(d) = it {
            for f in &d.fields {
                if f.name.name == "freq_mhz" {
                    if let ExprKind::Int(s) = &*f.value.kind {
                        if let Ok(n) = s.replace('_', "").parse::<i64>() {
                            // Period in ps = 1_000_000 / freq_mhz.
                            if n > 0 {
                                domains.insert(d.name.name.clone(), 1_000_000 / n);
                            }
                        }
                    }
                }
            }
        }
    }

    // Per-test metadata (dut_type for V<top>* construction, other_lets,
    // bare_stmts, explicit_run, clocks, probe_accessors) is derived
    // inside the per-test emission loop further down (search for
    // `for test in &tests`). The shared `dut_type` resolved above is
    // used by file-scope code paths that need the DUT module name.

    // Collect records + enums + scoreboards + monitors for typed-let
    // emission. Transactions and structs both lower as value-records;
    // transaction-specific behavior (keeps, transactor-facing metadata)
    // is layered on top.
    let mut structs = std::collections::HashSet::new();
    let mut transactions = std::collections::HashSet::new();
    let mut scoreboards = std::collections::HashSet::new();
    let mut components: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    let mut covergroups: std::collections::HashMap<String, CovergroupDecl> =
        std::collections::HashMap::new();
    let mut buses: std::collections::HashMap<String, BusDecl> = std::collections::HashMap::new();
    let mut transactors: std::collections::HashMap<String, TransactorDecl> =
        std::collections::HashMap::new();
    // RAL regblocks indexed by name. Populated in the pre-pass below;
    // consulted at codegen time to (a) emit one POD mirror struct +
    // constexpr address table per regblock at file scope, and (b)
    // lower `regs.NAME` field accesses against the bound helper.
    let mut regblocks: std::collections::HashMap<String, RegblockDecl> =
        std::collections::HashMap::new();
    // RAL addrmaps indexed by name. Chip-level container of regblock
    // instances at distinct base addresses. See docs/ral-support.md §4.
    let mut addrmaps: std::collections::HashMap<String, AddrmapDecl> =
        std::collections::HashMap::new();
    let mut txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>> =
        std::collections::HashMap::new();
    let mut record_fields: std::collections::HashMap<String, Vec<Field>> =
        std::collections::HashMap::new();
    let mut record_bodies: std::collections::HashMap<String, Vec<TxnBodyItem>> =
        std::collections::HashMap::new();
    // Per-transaction `keep` constraints (spec §4 — "constraints are
    // relations, not directives"). Collected here so `randomize(t)`
    // and `randomize(t) with …` both route them through the Z3 solver
    // path. Without this collection the keep blocks parse but emit
    // zero C++, a silent footgun: `keep len in [1..256]` would
    // randomize freely up to the field's full width.
    let mut txn_keeps: std::collections::HashMap<String, Vec<Expr>> =
        std::collections::HashMap::new();
    let mut enums = std::collections::HashMap::new();
    let mut enum_domains: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // Global variant-name → index map. Used by the Z3 constraint
    // translator to resolve bare `WRAP` / `INCR` / etc. into their
    // numeric encoding. v0 assumes variant names are globally
    // unique; collisions take the first-declared mapping (warned
    // about in the parser if we add that). Maps to i64 so negative
    // signed values fit naturally; the solver widens at use site.
    let mut enum_variants: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut consts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // Relation declarations indexed by name (spec §4.2). At constraint-
    // emit time, any `Call(Ident(R), args)` whose name is in this map
    // is inlined: the formal parameters substitute into R's body and
    // each expression becomes its own constraint added to the Z3
    // solver block. Recursive expansion handles relation-aliases-of-
    // relations (`relation A(t) = B(t) && t.x == 0`).
    let mut relations: std::collections::HashMap<String, RelationDecl> =
        std::collections::HashMap::new();
    let mut const_widths: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for it in &file.items {
        match it {
            Item::Const(c) => {
                if let ExprKind::Int(text) = &*c.value.kind {
                    consts.insert(c.name.name.clone(), text.clone());
                    if let Some(w) =
                        c.ty.as_ref()
                            .and_then(declared_type_bit_width)
                            .or_else(|| literal_operand_bit_width(text))
                    {
                        const_widths.insert(c.name.name.clone(), w);
                    }
                }
            }
            Item::Relation(r) => {
                relations.insert(r.name.name.clone(), r.clone());
            }
            _ => {}
        }
    }
    for it in &file.items {
        if let Item::Enum(e) = it {
            enums.insert(e.name.name.clone(), e.variants.len());
            enum_domains.insert(
                e.name.name.clone(),
                e.variants.iter().map(|v| v.name.clone()).collect(),
            );
            for (i, v) in e.variants.iter().enumerate() {
                enum_variants.entry(v.name.clone()).or_insert(i as i64);
            }
        }
    }
    for it in &file.items {
        match it {
            Item::Struct(s) => {
                record_fields.insert(s.name.name.clone(), s.fields.clone());
                record_bodies.insert(s.name.name.clone(), s.body.clone());
            }
            Item::Transaction(t) => {
                record_fields.insert(t.name.name.clone(), txn_direct_fields(&t.body));
                record_bodies.insert(t.name.name.clone(), t.body.clone());
            }
            Item::Scoreboard(c) => {
                scoreboards.insert(c.name.name.clone());
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) => {
                components.insert(c.name.name.clone(), c.clone());
            }
            Item::Covergroup(g) => {
                covergroups.insert(g.name.name.clone(), g.clone());
            }
            Item::Bus(b) => {
                buses.insert(b.name.name.clone(), b.clone());
            }
            Item::Transactor(t) => {
                transactors.insert(t.name.name.clone(), t.clone());
            }
            Item::Regblock(r) => {
                regblocks.insert(r.name.name.clone(), r.clone());
            }
            Item::Addrmap(a) => {
                addrmaps.insert(a.name.name.clone(), a.clone());
            }
            _ => {}
        }
    }

    // Validate addrmap structure: alias targets resolve, no chained
    // aliases, instance windows don't overlap (for pairs with size,
    // skipping any pair where one side aliases the other or both
    // alias the same target).
    for a in addrmaps.values() {
        if let Some(err) = check_addrmap_aliases(a) {
            return Err(EmitError(err));
        }
        if let Some(err) = check_addrmap_overlap(a) {
            return Err(EmitError(err));
        }
    }

    // Enforce: a transactor's always-on body cannot drive DUT signals.
    // Drive-side hookables / on-handlers must live inside `when active`.
    // Without this check, a `passive` instance — which has its
    // `when_active` body elided — would still emit the drive code and
    // could wire an observer to the bus (block-level→chip-level TB
    // reuse footgun). See `check_transactor_no_drive_in_always_on_body`
    // for the full rationale and detection model.
    for t in transactors.values() {
        if let Some(err) = check_transactor_no_drive_in_always_on_body(
            t,
            &transactors,
            &components,
            &scoreboards,
            &covergroups,
            &buses,
            &regblocks,
            &addrmaps,
        ) {
            return Err(EmitError(err));
        }
    }

    for it in &file.items {
        match it {
            Item::Transaction(t) => {
                transactions.insert(t.name.name.clone());
                let fields =
                    flatten_txn_body_field_infos(&t.body, &record_fields, &enums, &enum_domains);
                txn_fields.insert(t.name.name.clone(), fields);
                // Collect transaction-level `keep` constraints. Keeps nested
                // inside `when` subtype bodies lower as guarded implications:
                // `when G { keep K }` contributes `(!G) || K`, so the
                // constraint participates only when the discriminator selects
                // that subtype. Full tagged-ADT solver modeling remains a
                // future backend step; this keeps the current flat solver path
                // semantically honest for keeps over already-visible fields.
                let keeps = collect_record_keeps(&t.body, &record_bodies, &record_fields);
                if !keeps.is_empty() {
                    txn_keeps.insert(t.name.name.clone(), keeps);
                }
            }
            Item::Struct(s) => {
                structs.insert(s.name.name.clone());
                let fields =
                    flatten_record_field_infos(&s.fields, &record_fields, &enums, &enum_domains);
                txn_fields.insert(s.name.name.clone(), fields);
                let keeps = collect_record_keeps(&s.body, &record_bodies, &record_fields);
                if !keeps.is_empty() {
                    txn_keeps.insert(s.name.name.clone(), keeps);
                }
            }
            Item::Enum(e) => {
                enums.insert(e.name.name.clone(), e.variants.len());
            }
            _ => {}
        }
    }

    // Seed let_types from test-level `let` decls so `randomize(t)` works
    // when the let appears at test scope (above any scope sim / bare stmts).
    // Also seed let_modes from any `: T active`/`: T passive` annotation;
    // the call-site passive-mode check (further down, in emit_expr) walks
    // this map to determine the effective mode of `<env>.<sub>...<xact>`
    // paths whose root resolves to a let-bound name.
    // Build the property name → body table.
    let mut properties = std::collections::HashMap::new();
    for it in &file.items {
        if let Item::Property(p) = it {
            properties.insert(p.name.name.clone(), p.body.clone());
        }
    }

    // Construct the emitter with all FILE-SCOPE shared state. Per-test
    // fields (let_types, let_modes, pointer_vars, probes, clock_names,
    // bus_bindings, bus_remap, covers, actor_threads, field_subs,
    // driver_bus_for_hookables, let_helper) are intentionally left
    // empty here — they're reset+populated inside the per-test
    // emission loop below.
    let mut e = Emitter {
        out: String::new(),
        errors: Vec::new(),
        pointer_vars: std::collections::HashSet::new(),
        let_types: std::collections::HashMap::new(),
        let_widths: std::collections::HashMap::new(),
        vec_lane_widths: opts.vec_lane_widths.clone(),
        let_modes: std::collections::HashMap::new(),
        transactions,
        structs,
        scoreboards,
        components,
        covergroups,
        txn_fields,
        record_fields,
        enums,
        enum_variants,
        consts,
        const_widths,
        properties,
        prop_subs: std::collections::HashMap::new(),
        event_types: std::collections::HashMap::new(),
        field_subs: std::collections::HashMap::new(),
        covers: Vec::new(),
        clock_names: Vec::new(),
        current_yield_target: None,
        tseq_names: tseqs.iter().map(|t| t.name.name.clone()).collect(),
        buses,
        bus_bindings: std::collections::HashMap::new(),
        bus_param_envs: std::collections::HashMap::new(),
        dut_bus_port_overrides: opts.dut_bus_port_overrides.clone(),
        bus_remap: std::collections::HashMap::new(),
        pending_tlm_forks: Vec::new(),
        next_tlm_fork_tag: std::collections::HashMap::new(),
        in_coroutine: false,
        actor_threads: Vec::new(),
        mt: opts.mt,
        driver_bus_for_hookables: std::collections::HashMap::new(),
        transactors,
        current_component_instance: None,
        current_component_method: None,
        txn_keeps,
        relations,
        probes: std::collections::HashMap::new(),
        probe_widths: std::collections::HashMap::new(),
        shadowed_lets: std::collections::HashSet::new(),
        regblocks,
        addrmaps,
        let_helper: std::collections::HashMap::new(),
        runtime_randomize_problem_ids,
    };

    // Header.
    writeln!(e.out, "// Auto-generated by harc — do not edit.").ok();
    if tests.len() == 1 {
        writeln!(e.out, "// HARC test: {}", tests[0].name.name).ok();
    } else {
        let names: Vec<&str> = tests.iter().map(|t| t.name.name.as_str()).collect();
        writeln!(
            e.out,
            "// HARC tests ({}): {}",
            tests.len(),
            names.join(", ")
        )
        .ok();
    }
    writeln!(e.out, "").ok();
    // Disable clang optimization for this file. clang 17+ on both
    // Apple Silicon and Linux x86_64 mis-optimizes our `[&]`-
    // capturing C++20 lambda coroutines at `-Os` / `-O2`: closure
    // reference members fold against the original (freed) stack
    // frame after a suspension, causing SEGV on resume. Per-file
    // pragma keeps verilator-generated DUT `.cpp` files at `-Os`
    // for fast simulation.
    //
    // GCC has the same class of miscompile but `#pragma optimize`
    // doesn't propagate through C++20 coroutine codegen there.
    // The structural fix (2026-06-22): every coroutine is stored
    // in a named lambda variable (`auto _foo_lambda = [&](){...};
    // slot.thread = _foo_lambda(&slot);`) so the closure object
    // lives for the full duration of `run_<Test>`, not as a
    // temporary freed at the IIFE semicolon. This fixes GCC on
    // Linux without requiring HARC_CXX=clang++. The pragma remains
    // as a redundant defence for clang.
    writeln!(e.out, "#ifdef __clang__").ok();
    writeln!(e.out, "#pragma clang optimize off").ok();
    writeln!(e.out, "#endif").ok();
    writeln!(e.out, "").ok();
    writeln!(e.out, "#include \"V{dut_type}.h\"").ok();
    // Probe access needs the root struct's full definition (the
    // `rootp` field on V<Top> is a forward-declared pointer in
    // `V<Top>.h`). When the test declares one or more probes, also
    // include the root header so `dut->rootp-><mangled>` compiles.
    if !aggregated_probes.is_empty() {
        writeln!(e.out, "#include \"V{dut_type}___024root.h\"").ok();
    }
    writeln!(e.out, "#include \"verilated.h\"").ok();
    writeln!(e.out, "#if VM_COVERAGE").ok();
    writeln!(e.out, "#include \"verilated_cov.h\"").ok();
    writeln!(e.out, "#endif").ok();
    // Waveform trace headers (issue #209). Always emitted but gated
    // on `-DHARC_TRACE_VCD` / `-DHARC_TRACE_FST`, both supplied by
    // `harc sim --waves` at Verilator compile time. Non-trace builds
    // never include either header, so the no-waves build cost is
    // exactly zero.
    writeln!(e.out, "#if defined(HARC_TRACE_VCD)").ok();
    writeln!(e.out, "#include \"verilated_vcd_c.h\"").ok();
    writeln!(e.out, "#define HARC_TRACE_ENABLED 1").ok();
    writeln!(e.out, "using HarcTraceC = VerilatedVcdC;").ok();
    writeln!(e.out, "#elif defined(HARC_TRACE_FST)").ok();
    writeln!(e.out, "#include \"verilated_fst_c.h\"").ok();
    writeln!(e.out, "#define HARC_TRACE_ENABLED 1").ok();
    writeln!(e.out, "using HarcTraceC = VerilatedFstC;").ok();
    writeln!(e.out, "#else").ok();
    writeln!(e.out, "#define HARC_TRACE_ENABLED 0").ok();
    writeln!(e.out, "#endif").ok();
    writeln!(e.out, "#include <cstdio>").ok();
    writeln!(e.out, "#include <cstdint>").ok();
    writeln!(e.out, "#include <cstdlib>").ok();
    writeln!(e.out, "#include <cstdarg>").ok();
    writeln!(e.out, "#include <cstring>").ok();
    writeln!(e.out, "#include <string>").ok();
    writeln!(e.out, "#include <array>").ok();
    writeln!(e.out, "#include <vector>").ok();
    writeln!(e.out, "#include <deque>").ok();
    writeln!(e.out, "#include <functional>").ok();
    // Phase 3a: per-actor OS threads + barrier sync. Pulled in
    // unconditionally — `<atomic>` is also indirectly available via
    // `harc_thread_rt.h` but the explicit include keeps intent clear.
    writeln!(e.out, "#include <thread>").ok();
    writeln!(e.out, "#include <atomic>").ok();
    // HARC's coroutine runtime — drives the test's `run` block as a
    // C++20 coroutine via `harc_rt::ThreadScheduler`. Hookable methods
    // and `on`-handler closures still run synchronously between the
    // run coroutine's co_awaits; only the run body itself yields.
    // Multi-actor parallelism (driver + monitor coroutines on the same
    // bus) lands in Phase 2 on top of the same runtime.
    writeln!(e.out, "#include \"harc_thread_rt.h\"").ok();
    writeln!(e.out, "#include \"harc_random_rt.h\"").ok();
    writeln!(e.out, "#include \"harc_queue_rt.h\"").ok();
    writeln!(e.out, "#include \"harc_trace_rt.h\"").ok();
    writeln!(e.out, "#include \"harc_log_rt.h\"").ok();
    let uses_solver = uses_constraint_solver(file);
    if uses_solver {
        writeln!(
            e.out,
            "#include \"harc_z3_rt.h\"   // randomize(t) with <constraints>"
        )
        .ok();
    }
    writeln!(e.out, "").ok();
    // Template helpers that gate clock toggling on whether the DUT exposes a
    // `clk` member. `if constexpr (requires {{ ... }})` only suppresses the
    // discarded branch inside a template body; moving the `dut->clk` write
    // into a template function means calling it from `main()` or a lambda
    // instantiates the right specialisation and the ill-formed branch for
    // combinational DUTs is never compiled.
    writeln!(e.out, "template<typename DUT>").ok();
    writeln!(e.out, "static void _harc_eval_negedge(DUT* dut) {{").ok();
    writeln!(
        e.out,
        "{INDENT}if constexpr (requires {{ dut->clk; }}) {{ dut->clk = 0; }}"
    )
    .ok();
    writeln!(e.out, "{INDENT}dut->eval();").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "template<typename DUT>").ok();
    writeln!(e.out, "static void _harc_eval_posedge(DUT* dut) {{").ok();
    writeln!(
        e.out,
        "{INDENT}if constexpr (requires {{ dut->clk; }}) {{ dut->clk = 1; }}"
    )
    .ok();
    writeln!(e.out, "{INDENT}dut->eval();").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "").ok();

    if !runtime_problem_table.problems.is_empty() {
        e.out.push_str(
            &runtime_problem_table.render_cpp_table("_harc_runtime_random_problem_table"),
        );
    }

    // ── PRNG runtime ──────────────────────────────────────────────────────
    // Seed loaded from HARC_SEED through the random runtime helper.
    writeln!(e.out, "static harc_rt::random::HarcRng harc_rng;").ok();
    writeln!(e.out, "static inline uint64_t harc_rng_next() {{").ok();
    writeln!(e.out, "{INDENT}return harc_rng.next();").ok();
    writeln!(e.out, "}}").ok();
    writeln!(e.out, "").ok();

    // ── Shared HVL value records ────────────────────────────────────────
    // Structs and transactions share the C++ record/equality/randomize
    // lowering. Transactions layer keeps and protocol-facing semantics at
    // the randomize call site and in transactors.
    for it in &file.items {
        if let Item::Struct(s) = it {
            e.emit_struct_record(s);
        }
    }
    for it in &file.items {
        if let Item::Transaction(t) = it {
            e.emit_transaction(t);
        }
    }

    // ── Scoreboard / component / transactor structs ─────────────────────
    // Emitted in field-dependency order so a transactor / component
    // field whose type is another transactor / component declared
    // later in the source list still finds a complete C++ type at
    // its declaration site. See `topo_sort_component_indices` for
    // the dependency rule and cycle-recovery behaviour, and issue
    // #301 for the symptom this prevents (transactor field forward
    // reference passed `harc check` but emitted undeclared C++).
    //
    // Scoreboards emit before regular components / transactors in
    // the source-order pass below ONLY when they have no incoming
    // edges from a transactor's fields — the topo sort handles the
    // mixed case correctly. Covergroups still emit earlier (above)
    // because covergroups are leaf observables that never name a
    // component or transactor.
    let component_order = topo_sort_component_indices(file);
    for &idx in &component_order {
        if let Item::Scoreboard(s) = &file.items[idx] {
            e.emit_scoreboard(s);
        }
    }

    // ── Per-channel payload structs ──────────────────────────────────────
    // `bus.<ch>.recv()` and `on bus.<ch>.handshake(arg)` capture the
    // channel's full payload — for multi-payload channels (e.g. AXI's
    // `r` carrying both `data` and `resp`) users need `arg.data` /
    // `arg.resp`. We emit one struct per `handshake_channel`, named
    // `<BusName>_<chan>_payload`, with one field per payload signal.
    //
    // The struct also exposes `operator uint64_t() const` returning
    // the first field — backward compatible with the previous v0
    // behaviour (`recv()` returning a scalar). Existing fixtures that
    // do `assert val == 0xCAFEBABE` keep working without change;
    // multi-payload access just becomes available.
    for it in &file.items {
        if let Item::Bus(b) = it {
            for h in &b.handshakes {
                if h.payload.is_empty() {
                    continue;
                }
                let struct_name = format!("{}_{}_payload", b.name.name, h.name.name);
                writeln!(e.out, "struct {struct_name} {{").ok();
                for sig in &h.payload {
                    let cty = txn_field_c_type(&sig.ty);
                    writeln!(e.out, "{INDENT}{cty} {};", sig.name.name).ok();
                }
                // Implicit conversion to the first payload field — keeps
                // single-field-style usage (`val == N`, `last_read = val`)
                // compiling against the new struct type.
                let first_sig = &h.payload[0];
                let first_cty = txn_field_c_type(&first_sig.ty);
                writeln!(
                    e.out,
                    "{INDENT}operator {first_cty}() const {{ return {}; }}",
                    first_sig.name.name,
                )
                .ok();
                writeln!(e.out, "}};").ok();
                writeln!(e.out, "").ok();
            }
        }
    }

    // ── RAL regblock mirror structs + address tables ────────────────────
    // One POD `struct <Name>_Mirror { uint<W>_t REG; ... };` per
    // declared `regblock`, plus a `constexpr` address table giving
    // each register's byte offset. Accessor calls (`regs.NAME = v` /
    // `let x = regs.NAME`) lower to mirror update + helper.write/read
    // — see docs/ral-support.md §7.4 (frontdoor lowering). Phase 1a
    // keeps the mirror flat — no nested addrmap composition yet.
    let any_regblock = file.items.iter().any(|it| matches!(it, Item::Regblock(_)));
    if any_regblock {
        // Recursion-guard depth limit for `regs.record_write` callbacks.
        // A callback body invoking `record_write` re-enters the same
        // decode block synchronously; without a bound, a self-write
        // (`on regs.A { regs.record_write(0x00, ...) }`) grows the
        // stack unboundedly. 16 leaves plenty of room for realistic
        // nested CSR cascades while catching runaway recursion fast.
        // See docs/ral-support.md §3.2.
        writeln!(e.out, "#ifndef HARC_RAL_CB_MAX_DEPTH").ok();
        writeln!(
            e.out,
            "static constexpr uint32_t HARC_RAL_CB_MAX_DEPTH = 16;"
        )
        .ok();
        writeln!(e.out, "#endif").ok();
        writeln!(e.out, "").ok();
    }
    for it in &file.items {
        if let Item::Regblock(r) = it {
            let default_w = r.default_width.unwrap_or(32);
            writeln!(e.out, "struct {}_Mirror {{", r.name.name).ok();
            for reg in &r.registers {
                let w = reg.width.unwrap_or(default_w);
                let cty = mirror_field_c_type(w);
                let init = match &reg.reset {
                    Some(rv) => format!(" = {}", c_int_literal_from(&rv.kind)),
                    None => " = 0".to_string(),
                };
                writeln!(e.out, "{INDENT}{cty} {}{};", reg.name.name, init).ok();
            }
            writeln!(e.out, "}};").ok();
            writeln!(e.out, "").ok();

            // constexpr address table — one entry per register, indexed
            // by C++ identifier matching the source register name. Used
            // by future bitbash() lowering; reads/writes inline the
            // offset literal directly for simplicity in Phase 1a.
            writeln!(
                e.out,
                "struct {}_AddrEntry {{ const char* name; uint64_t offset; uint32_t width; }};",
                r.name.name,
            )
            .ok();
            writeln!(
                e.out,
                "static constexpr {}_AddrEntry {}_AddrTable[] = {{",
                r.name.name, r.name.name,
            )
            .ok();
            for reg in &r.registers {
                let w = reg.width.unwrap_or(default_w);
                let off = c_int_literal_from(&reg.offset.kind);
                writeln!(
                    e.out,
                    "{INDENT}{{ \"{name}\", {off}, {w} }},",
                    name = reg.name.name,
                )
                .ok();
            }
            writeln!(e.out, "}};").ok();
            writeln!(e.out, "").ok();

            // ── RAL per-register write callbacks + passive record API ──
            // `<Name>_Callbacks` holds one optional `void(uint64_t)`
            // closure per register. `on regs.REG ... end on` populates a
            // slot; `regs.record_write(addr, data)` fires the matching
            // slot after updating the mirror. `<Name>_record_read(m, addr)`
            // is the passive read counterpart — it decodes the address to
            // the mirror cell with no bus traffic. Both own the address
            // decode at codegen time so a checker observing bus traffic
            // never hand-writes an `if/elsif` address ladder.
            // See docs/ral-support.md §3.2.
            writeln!(e.out, "struct {}_Callbacks {{", r.name.name).ok();
            for reg in &r.registers {
                writeln!(
                    e.out,
                    "{INDENT}std::function<void(uint64_t)> {};",
                    reg.name.name,
                )
                .ok();
            }
            writeln!(e.out, "}};").ok();
            writeln!(e.out, "").ok();

            writeln!(
                e.out,
                "static inline uint64_t {name}_record_read(const {name}_Mirror& m, uint64_t addr) {{",
                name = r.name.name,
            )
            .ok();
            for reg in &r.registers {
                let off = c_int_literal_from(&reg.offset.kind);
                writeln!(
                    e.out,
                    "{INDENT}if (addr == (uint64_t)({off})) return (uint64_t)m.{};",
                    reg.name.name,
                )
                .ok();
            }
            writeln!(e.out, "{INDENT}return 0;").ok();
            writeln!(e.out, "}}").ok();
            writeln!(e.out, "").ok();
        }
    }

    // ── RAL addrmap mirror structs ──────────────────────────────────────
    // Chip-level container. Each `addrmap A { instance inst : R @ B }`
    // lowers to `struct A_Mirror { R_Mirror inst; ... };` — one
    // member per instance, of the corresponding regblock's Mirror
    // type. Bus addresses for `chip.inst.REG` are computed
    // `BASE_inst + offset(REG)` at codegen time. See docs/ral-support.md §4.
    for it in &file.items {
        if let Item::Addrmap(a) = it {
            writeln!(e.out, "struct {}_Mirror {{", a.name.name).ok();
            for inst in &a.instances {
                // Aliased instances share mirror storage with their
                // target — no separate field. Access through the
                // alias rewrites to the target's mirror path at
                // codegen time. See docs/ral-support.md §4.
                if inst.alias_of.is_some() {
                    writeln!(
                        e.out,
                        "{INDENT}// {name}: alias of {target} — shares mirror",
                        name = inst.name.name,
                        target = inst.alias_of.as_ref().unwrap().name,
                    )
                    .ok();
                    continue;
                }
                writeln!(
                    e.out,
                    "{INDENT}{ty}_Mirror {name};",
                    ty = inst.regblock_ty.name,
                    name = inst.name.name,
                )
                .ok();
            }
            writeln!(e.out, "}};").ok();
            writeln!(e.out, "").ok();
        }
    }

    // ── Covergroup structs (per-bin counters + sample() + report()) ─────
    // Emitted BEFORE component structs so a `testbench Tb { cov : Cg }`
    // body can name `Cg` as a field type without a forward-decl. The
    // testbench/env composition is the only direction the dependency
    // runs — covergroups are leaf observables, they never name a
    // component or transactor.
    for it in &file.items {
        if let Item::Covergroup(g) = it {
            e.emit_covergroup_struct(g);
        }
    }

    // ── Monitor structs ──────────────────────────────────────────────────
    // Same shape as scoreboards; output `event<T>` fields lower to
    // ── Component structs (agent / env / sequencer).
    // Scoreboards have their own dedicated path above; the rest are
    // plain field-bearing structs. `hookable` methods are emitted
    // separately as free `[&]`-capturing lambdas (below) so the
    // method body sees `dut` / `tick` / `_checkers` from the test scope.
    //
    // Emission order: dependency-sorted (`component_order` computed
    // above). A transactor whose field references another transactor
    // / component declared later in the source list emits after its
    // dependency — fixes #301.
    for &idx in &component_order {
        match &file.items[idx] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) => {
                e.emit_component_struct(c);
            }
            Item::Transactor(t) => {
                // Compose a synthetic ComponentDecl with the union of
                // always-present body items + active body items, and
                // emit a single struct. Both modes get the same C++
                // class layout — passive instances simply don't
                // subscribe to / spawn actors against the active
                // fields. Mode-specific elision lives in the lowering
                // at instantiation, not in the struct shape.
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                e.emit_component_struct(&synth);
            }
            _ => {}
        }
    }

    // ── Top-level `const` items ─────────────────────────────────────────
    // `const NAME : Ty = expr` lowers to a file-scope
    // `static constexpr <c_type> NAME = <expr>;` so the value is
    // available everywhere — inside `main()`, hookable lambdas (which
    // can't capture across translation-unit boundaries but can use
    // file-scope constants directly), `tseq` lambdas, and on-handler
    // closures. constexpr also folds into expressions used at struct
    // field defaults if any future fixture goes there.
    let mut emitted_const = false;
    for it in &file.items {
        if let Item::Const(c) = it {
            let cty =
                c.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or_else(|| "int64_t".to_string());
            write!(e.out, "static constexpr {cty} {} = ", c.name.name).ok();
            e.emit_expr(&c.value);
            writeln!(e.out, ";").ok();
            emitted_const = true;
        }
    }
    if emitted_const {
        writeln!(e.out, "").ok();
    }

    // `extern function name(params) -> ret` (spec §9) — emit C-linkage
    // forward declarations at file scope so the user's `--ref-src
    // <file>.cpp` can satisfy the linker without HARC needing to know
    // its implementation. Shared with the TB-IR codegen so both emit
    // byte-identical `extern "C"` blocks (`emit_extern_fn_decls`).
    emit_extern_fn_decls(&mut e.out, file);

    // Shared per-run state. This is the first step toward the phase-2
    // member/context refactor: generated run bodies still use the
    // historical local names (`dut`, `_checkers`, `errors`, ...), but the
    // run prologue now binds those names as references into this common
    // context object. Future shared helper functions can take
    // `HarcTestContext& ctx` instead of relying on `[&]` captures.
    writeln!(e.out, "struct HarcTestContext {{").ok();
    writeln!(e.out, "{INDENT}V{dut_type}* dut = nullptr;").ok();
    writeln!(e.out, "#if HARC_TRACE_ENABLED").ok();
    writeln!(e.out, "{INDENT}HarcTraceC* tfp = nullptr;").ok();
    writeln!(e.out, "{INDENT}std::string _wave_path;").ok();
    writeln!(e.out, "#endif").ok();
    writeln!(e.out, "{INDENT}uint64_t _trace_time = 0;").ok();
    writeln!(e.out, "{INDENT}int errors = 0;").ok();
    writeln!(e.out, "{INDENT}bool _fatal = false;").ok();
    writeln!(e.out, "{INDENT}int cycle_count = 0;").ok();
    writeln!(e.out, "{INDENT}harc_rt::trace::HarcTraceWriter trace;").ok();
    writeln!(e.out, "{INDENT}harc_rt::log::HarcLogContext log_ctx;").ok();
    writeln!(
        e.out,
        "{INDENT}std::vector<std::function<void()>> _checkers;"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}std::vector<std::function<void()>> _post_eval_services;"
    )
    .ok();
    writeln!(
        e.out,
        "{INDENT}std::vector<std::function<void()>> _auto_cov_reports;"
    )
    .ok();
    writeln!(e.out, "}};").ok();
    writeln!(e.out, "").ok();

    // Per-test entry points. Each `Item::Test` in source gets its
    // own `int run_<TestName>(argc, argv)` function. The dispatcher
    // `int main()` below picks one based on `--test <name>` /
    // `HARC_TEST` env. Each run function owns its DUT pointer,
    // scheduler, `_checkers` vector — tests run against fresh
    // Verilator instances, sequentially (one binary, dispatched).
    // See docs/separate-compilation-plan.md for the future per-test
    // `.o` direction (Phase 2).
    for test in &tests {
        // ── Reset Emitter per-test state ────────────────────────────
        e.let_types.clear();
        e.let_modes.clear();
        e.let_widths.clear();
        e.pointer_vars.clear();
        e.pointer_vars.insert("dut".to_string());
        e.bus_bindings.clear();
        e.bus_param_envs.clear();
        e.bus_remap.clear();
        e.pending_tlm_forks.clear();
        e.next_tlm_fork_tag.clear();
        e.probes.clear();
        e.probe_widths.clear();
        e.shadowed_lets.clear();
        e.clock_names.clear();
        e.covers.clear();
        e.actor_threads.clear();
        e.field_subs.clear();
        e.driver_bus_for_hookables.clear();
        e.let_helper.clear();
        e.in_coroutine = false;
        e.current_yield_target = None;
        e.current_component_instance = None;

        // ── Derive per-test metadata ───────────────────────────────
        let custom_phases: Vec<(Ident, Block)> = test
            .items
            .iter()
            .filter_map(|it| match it {
                TestItem::Phase(name, body) => Some((name.clone(), body.clone())),
                _ => None,
            })
            .collect();
        let mut other_lets: Vec<&LetStmt> = Vec::new();
        let mut bare_stmts: Vec<&Stmt> = Vec::new();
        let mut explicit_run: Option<&Block> = None;
        let mut clocks: Vec<&ClockDecl> = Vec::new();
        for it in &test.items {
            match it {
                TestItem::Let(l) => {
                    if l.name.name == "dut" {
                        for p in &l.probes {
                            e.probes.insert(
                                p.name.name.clone(),
                                ProbeAccessor {
                                    read: crate::codegen::sv_stub::mangled_accessor(
                                        dut_type,
                                        &p.name.name,
                                    ),
                                    force: p.force,
                                },
                            );
                            // Mirrors TB-IR's `probe_scalar_width`: a
                            // `Bit`/`Bool` probe is one bit wide, not
                            // width-less. Omitting those made v1 reject a
                            // `dut.<bit probe> +% 1` that TB-IR wraps.
                            if let TypeExpr::Builtin { name, args, .. } = &p.ty {
                                let w = match name {
                                    BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => {
                                        Some(1)
                                    }
                                    BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits => {
                                        type_arg_width(args)
                                    }
                                    _ => None,
                                };
                                if let Some(w) = w {
                                    e.probe_widths.insert(p.name.name.clone(), w);
                                }
                            }
                        }
                    } else {
                        other_lets.push(l);
                    }
                }
                TestItem::Scope(s) => {
                    if let Some(r) = &s.run {
                        explicit_run = Some(r);
                    }
                }
                TestItem::Stmt(s) => bare_stmts.push(s),
                TestItem::Clock(c) => clocks.push(c),
                _ => {}
            }
        }
        if explicit_run.is_none() && bare_stmts.is_empty() {
            return Err(EmitError(format!(
                "test `{}` has no body — add a `scope sim`, `run`, or bare statements",
                test.name.name,
            )));
        }
        let _ = (&explicit_run, &bare_stmts);

        // ── Seed Emitter per-test state ────────────────────────────
        for l in &other_lets {
            if let Some(s) = type_simple_name(l.ty.as_ref()) {
                e.let_types.insert(l.name.name.clone(), s.to_string());
            }
            if let Some(TypeExpr::Named { mode: Some(m), .. }) = l.ty.as_ref() {
                e.let_modes.insert(l.name.name.clone(), *m);
            }
            // Track bit-widths for uint<W> / sint<W> / bits<W> lets so
            // the width-method intrinsics (`.trunc<N>()` / `.sext<N>()`
            // etc.) can statically check that the requested width
            // direction matches the source width, and so sext can emit
            // its shift-fill from the right MSB position.
            if let Some(TypeExpr::Builtin { name, args, .. }) = l.ty.as_ref() {
                if matches!(name, BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits) {
                    if let Some(w) = type_arg_width(args) {
                        if e.let_widths.insert(l.name.name.clone(), w).is_some() {
                            e.shadowed_lets.insert(l.name.name.clone());
                        }
                    }
                }
            }
            if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                if e.transactions.contains(simple)
                    || e.scoreboards.contains(simple)
                    || e.components.contains_key(simple)
                    || e.covergroups.contains_key(simple)
                    || e.buses.contains_key(simple)
                    || e.transactors.contains_key(simple)
                    || e.regblocks.contains_key(simple)
                    || e.addrmaps.contains_key(simple)
                {
                    continue;
                }
            }
            if matches!(&l.ty, Some(TypeExpr::Named { .. })) {
                e.pointer_vars.insert(l.name.name.clone());
            }
        }
        // Seed `let_types` for the bound testbench's fields (impl-for
        // form). After desugaring, a `testbench`-form test carries
        // `let _tb : <TbType>` and its fields (`drv`, `cov`, ...) live on
        // the `_tb` struct rather than as run-scope lets, so they never
        // hit the `other_lets` loop above. Covergroup HOOK-trigger
        // resolution (`covergroup G @(drv.method(t) post)`) resolves the
        // receiver field via `let_types`, so without this seed it cannot
        // find `drv` under the impl-for form (only the classic `test`
        // form, where the fields ARE run-scope lets, worked). Field names
        // are test-scoped; seeding them by their declared type lets the
        // hook resolver reach the transactor/component method table.
        if let Some(tb_ty) = e.let_types.get("_tb").cloned() {
            if let Some(tb_comp) = e.components.get(&tb_ty).cloned() {
                for ci in &tb_comp.items {
                    if let ComponentItem::Field(f) = ci {
                        if let Some(simple) = type_simple_name(Some(&f.ty)) {
                            e.let_types
                                .entry(f.name.name.clone())
                                .or_insert_with(|| simple.to_string());
                        }
                    }
                }
            }
        }
        e.clock_names = clocks.iter().map(|c| c.name.name.clone()).collect();

        writeln!(
            e.out,
            "int run_{}(int argc, char** argv) {{",
            test.name.name
        )
        .ok();
        writeln!(e.out, "{INDENT}Verilated::commandArgs(argc, argv);").ok();
        writeln!(e.out, "{INDENT}HarcTestContext ctx;").ok();
        writeln!(e.out, "{INDENT}ctx.dut = new V{dut_type};").ok();
        writeln!(e.out, "{INDENT}auto* dut = ctx.dut;").ok();
        writeln!(e.out, "#if HARC_TRACE_ENABLED").ok();
        writeln!(e.out, "{INDENT}auto* tfp = ctx.tfp;").ok();
        writeln!(e.out, "{INDENT}auto& _wave_path = ctx._wave_path;").ok();
        writeln!(e.out, "#endif").ok();
        writeln!(e.out, "{INDENT}auto& _trace_time = ctx._trace_time;").ok();
        writeln!(e.out, "{INDENT}auto& errors = ctx.errors;").ok();
        writeln!(e.out, "{INDENT}auto& _fatal = ctx._fatal;").ok();
        writeln!(e.out, "{INDENT}auto& cycle_count = ctx.cycle_count;").ok();
        writeln!(e.out, "{INDENT}auto& trace = ctx.trace;").ok();
        writeln!(e.out, "{INDENT}auto& log_ctx = ctx.log_ctx;").ok();
        writeln!(e.out, "{INDENT}auto& _checkers = ctx._checkers;").ok();
        writeln!(
            e.out,
            "{INDENT}auto& _post_eval_services = ctx._post_eval_services;"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}auto& _auto_cov_reports = ctx._auto_cov_reports;"
        )
        .ok();
        // Waveform tracer setup (issue #209). Only compiled in when
        // `harc sim --waves` defined `HARC_TRACE_VCD` or
        // `HARC_TRACE_FST`. `HARC_WAVE_FILE` (set by `harc sim`)
        // selects the output path; `HARC_TRACE_DEPTH` selects the
        // hierarchy depth passed to `dut->trace()`. `_trace_time` is
        // a monotonically increasing dump cursor — its absolute
        // units do not matter, only that successive dump helper calls
        // receive strictly-increasing values so GTKWave /
        // surfer can order events.
        writeln!(e.out, "#if HARC_TRACE_ENABLED").ok();
        writeln!(e.out, "{INDENT}Verilated::traceEverOn(true);").ok();
        writeln!(e.out, "{INDENT}tfp = new HarcTraceC;").ok();
        writeln!(
            e.out,
            "{INDENT}_wave_path = harc_rt::log::harc_open_wave_trace(dut, tfp, harc_rt::log::harc_wave_default_name());"
        )
        .ok();
        writeln!(e.out, "#endif").ok();
        // Per spec §7.7: `log(fatal, ...)` aborts this test instance at
        // the end of the current cycle. The flag is checked by the
        // main simulation-loop guard below.
        writeln!(e.out, "").ok();
        // Seed PRNG from HARC_SEED env (or 1 if unset). Logged after sim_log_line
        // is defined so it lands in sim.log along with normal test output.
        writeln!(e.out, "{INDENT}harc_rng.seed_from_env();").ok();
        writeln!(
            e.out,
            "{INDENT}harc_rt::trace::harc_start_trace(trace, harc_rng.state, \"{dut_type}\", \"{}\", cycle_count);",
            test.name.name
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}auto _harc_trace_dump_next = [&](const char* clock, uint64_t clock_cycle) {{"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}uint64_t t = _trace_time++;").ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}trace.set_timing(t, clock, clock_cycle);"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}HARC_RT_DUMP_WAVE_TRACE(tfp, t);").ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(
            e.out,
            "{INDENT}auto _harc_trace_dump_at = [&](uint64_t t, const char* clock, uint64_t clock_cycle) {{"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}trace.set_timing(t, clock, clock_cycle);"
        )
        .ok();
        writeln!(e.out, "{INDENT}{INDENT}HARC_RT_DUMP_WAVE_TRACE(tfp, t);").ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(e.out, "").ok();
        // sim.log captures every log()/assert/fail line with cycle + severity
        // prefix. Path is configurable via the HARC_SIM_LOG env var (so the
        // outer harness can put it in the build dir); default `sim.log` in cwd.
        // Echo the active waveform path into sim.log so post-mortem
        // log inspection links to the matching VCD/FST without
        // grepping stderr. No-op in non-trace builds.
        writeln!(
            e.out,
            "{INDENT}HARC_RT_LOG_WAVE_FILE(log_ctx.sim_log, _wave_path);"
        )
        .ok();
        writeln!(e.out, "").ok();
        // Concurrent assertion hook — every `assert property <expr>` /
        // `assert property NAME` registers a closure here; tick() invokes the
        // whole list after each `eval()`. Same-cycle (`|->`) and one-cycle
        // (`|=>`) properties run on every primary-clock edge.
        writeln!(e.out, "").ok();

        if clocks.is_empty() {
            // Single-clock backward-compat path: drives `dut->clk` when the DUT
            // has that member (clocked modules). Purely combinational DUTs have no
            // `clk` port, so `_harc_eval_{negedge,posedge}` silently skip the
            // assignment via `if constexpr (requires { dut->clk; })`.
            // cycle_count increments once per tick.
            writeln!(e.out, "{INDENT}auto tick = [&]() {{").ok();
            writeln!(e.out, "{INDENT}{INDENT}_harc_eval_negedge(dut);").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}_harc_eval_posedge(dut);").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)(cycle_count + 1));"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}cycle_count++;").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}for (auto& _svc : _post_eval_services) _svc();"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}if (!_post_eval_services.empty()) dut->eval();"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}if (!_post_eval_services.empty()) _harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}};").ok();
            writeln!(e.out, "").ok();
        } else {
            // Multi-clock scheduler: every declared clock keeps its own next-edge
            // timestamp; we advance simulation time to the earliest pending edge,
            // toggle that clock, call eval(). cycle_count tracks rising edges of
            // the primary clock (first-declared) so existing log lines remain
            // meaningful.
            writeln!(e.out, "{INDENT}long long now_ps = 0;").ok();
            writeln!(e.out, "{INDENT}struct ClockState {{ const char* name; long long half_period_ps; long long next_edge_ps; int level; long long rising_count; }};").ok();
            writeln!(e.out, "{INDENT}std::vector<ClockState> clocks_;").ok();
            for c in &clocks {
                // Period source: time literal `5ns` OR domain reference `FastDomain`
                // (looked up in the domain table → derived from freq_mhz).
                let period_ps = match &*c.period.kind {
                ExprKind::Time(s) => time_literal_to_ps(s).map_err(EmitError)?,
                ExprKind::Ident(id) => *domains.get(&id.name).ok_or_else(|| EmitError(format!(
                    "clock {} references domain `{}` but no `domain {}` declaration was found in any input file",
                    c.name.name, id.name, id.name
                )))?,
                _ => return Err(EmitError(format!(
                    "clock {} period must be a time literal (e.g. 5ns) or a domain name (e.g. FastDomain)",
                    c.name.name
                ))),
            };
                let half = period_ps / 2;
                // First edge fires at half_period (rising) so initial state is 0.
                writeln!(
                    e.out,
                    "{INDENT}clocks_.push_back(ClockState{{\"{}\", {half}, {half}, 0, 0}});",
                    c.name.name
                )
                .ok();
                writeln!(e.out, "{INDENT}dut->{} = 0;", c.name.name).ok();
            }
            writeln!(e.out, "").ok();
            writeln!(
                e.out,
                "{INDENT}auto eval_clocks_until = [&](long long t_ps) {{"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}while (now_ps < t_ps) {{").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}long long next = t_ps;").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}for (auto& c : clocks_) if (c.next_edge_ps < next) next = c.next_edge_ps;").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}now_ps = next;").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}bool _primary_rising = false;"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}const char* _last_edge_clock = \"\";"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}uint64_t _last_edge_cycle = 0;"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}for (size_t i = 0; i < clocks_.size(); i++) {{"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}auto& c = clocks_[i];"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}if (c.next_edge_ps == now_ps) {{"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.level = !c.level;"
            )
            .ok();
            // Per-clock signal write — done by name lookup.
            for (idx, c) in clocks.iter().enumerate() {
                writeln!(
                    e.out,
                    "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == {idx}) dut->{} = c.level;",
                    c.name.name
                )
                .ok();
            }
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}c.next_edge_ps += c.half_period_ps;"
            )
            .ok();
            // Per-clock rising-edge count (consumed by `wait N cycles on <clock>`).
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (c.level == 1) c.rising_count++;"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}_last_edge_clock = c.name;"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}_last_edge_cycle = (uint64_t)c.rising_count;"
            )
            .ok();
            // Primary clock rising edge bumps cycle_count.
            writeln!(
            e.out,
            "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (i == 0 && c.level == 1) {{ cycle_count++; _primary_rising = true; }}"
        )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}{INDENT}}}").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}}}").ok();
            writeln!(e.out, "{INDENT}{INDENT}{INDENT}dut->eval();").ok();
            // Set semantic trace timing for this edge *before* post_eval
            // services (so their trace events carry the right time), but
            // defer the waveform dump until after those services and the
            // follow-up eval settle. VCD allows only one dump per physical
            // timestamp, so we dump exactly once per `now_ps` (issue #477).
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}trace.set_timing((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}if (_primary_rising) {{ for (auto& _svc : _post_eval_services) _svc(); if (!_post_eval_services.empty()) dut->eval(); }}"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}{INDENT}_harc_trace_dump_at((uint64_t)now_ps, _last_edge_clock, _last_edge_cycle);"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}}}").ok();
            writeln!(e.out, "{INDENT}}};").ok();
            writeln!(e.out, "").ok();
            // `tick()` advances by one full primary clock period (one rising
            // edge). Other clocks tick at their natural rate during this span.
            writeln!(e.out, "{INDENT}auto tick = [&]() {{").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}long long target = now_ps + clocks_[0].half_period_ps * 2;"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}eval_clocks_until(target);").ok();
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}};").ok();
            writeln!(e.out, "").ok();
        }
        // Variadic so log()/assert/fail callers can pass printf-style args
        // produced by `${expr}` string-interpolation lowering. The generated
        // lambdas keep the varargs ABI; runtime helpers own the sinks.
        // Per-file log handles, opened on first reference, closed at exit.
        // Relative paths are anchored to HARC_LOG_DIR by the runtime helper.
        writeln!(
            e.out,
            "{INDENT}auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);"
        )
        .ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(e.out, "").ok();
        // After sim_log_line below is defined, emit the seed line so it lands
        // in sim.log on every run — required for reproducing failures.
        let log_seed = true;

        writeln!(
            e.out,
            "{INDENT}auto sim_log_line = [&](const char* sev, const char* fmt, ...) {{"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}{INDENT}HARC_RT_LOG_PRINTF(log_ctx.sim_log, &trace, cycle_count, sev, fmt);"
        )
        .ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(e.out, "").ok();

        if log_seed {
            writeln!(
                e.out,
                "{INDENT}sim_log_line(\"INFO\", \"seed=%llu\", (long long)harc_rng.state);"
            )
            .ok();
            writeln!(e.out, "").ok();
        }

        // User-defined functions become lambdas. Emitted before the test body so
        // the body can call them. Capture-all (`[&]`) so they see `dut`/`tick`/
        // any test-level let bindings.
        for f in &funcs {
            e.emit_function(f, 1);
        }
        if !funcs.is_empty() {
            writeln!(e.out, "").ok();
        }
        // Tseqs lower to lambdas returning `std::vector<T>`; emitted alongside
        // functions so the run-block can invoke them and consume the result.
        for t in &tseqs {
            e.emit_tseq(t, 1);
        }
        if !tseqs.is_empty() {
            writeln!(e.out, "").ok();
        }
        // Hookable methods on driver / agent / env / sequencer / scoreboard
        // become free `[&]`-capturing lambdas named `<Type>_<method>`. The
        // method-call site rewrites `obj.method(args)` to
        // `<Type>_<method>(obj, args)` so the body sees `tick` / `_checkers`
        // / etc. from the test scope.
        //
        // Pre/post hook vectors emit FIRST so the method bodies (and the
        // test-scope `on obj.method pre/post` registrations) can `[&]`-
        // capture them. Empty vectors are no-ops; the wrap is unconditional.
        let mut emitted_any_method = false;
        // Hook vectors emit in dependency order so that a method
        // body which calls into another transactor / component's
        // method finds the corresponding `<Type>_<method>_pre/_post`
        // vector already declared at its capture site. The vectors
        // themselves don't reference each other, but the method
        // lambdas below — which `[&]`-capture these vectors — do
        // call across component boundaries; co-locating hook
        // vectors and methods in the same order keeps the capture
        // graph acyclic. See `topo_sort_component_indices` (#301).
        for &idx in &component_order {
            match &file.items[idx] {
                Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                    for ci in &c.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_hook_vectors(c, h, 1);
                        }
                    }
                }
                Item::Transactor(t) => {
                    let synth = synth_component_from_transactor(t, /*include_active*/ true);
                    for ci in &synth.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_hook_vectors(&synth, h, 1);
                        }
                    }
                }
                _ => {}
            }
        }
        // Watchdog hook vectors must be emitted in the same forward-decl
        // pass as the hookable hook vectors — `on <Type>.watchdog pre/post`
        // captures them. The `emit_watchdog` helper below emits BOTH the
        // hook vectors AND the synthetic method body in one go, since the
        // method body refers to those vectors. So we forward-declare the
        // method via the same pass that emits hookable method lambdas
        // below; here, no separate forward decl is needed because the
        // method lambda + its hook vectors are emitted together at that
        // point.
        // Pre-scan: register test-scope bus bindings (`let axil :
        // BusAxiLite = bind dut`) and driver-type → binding mappings
        // BEFORE hookable methods emit. Hookables on `bound to BusType`
        // drivers need the binding active so `bus.<ch>.send/recv` and
        // `bus.<ch>.<sig>` resolve correctly. emit_let later re-registers
        // the bus bindings (idempotent — same key, same value).
        for it in &test.items {
            if let TestItem::Let(l) = it {
                if l.bind {
                    if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                        if let Some(bus_decl) = e.buses.get(simple).cloned() {
                            if let Some(v) = &l.value {
                                let mut buf = String::new();
                                std::mem::swap(&mut e.out, &mut buf);
                                e.emit_expr(v);
                                std::mem::swap(&mut e.out, &mut buf);
                                let prefix = l.name.name.clone();
                                // Effective param env for `generate_if` gate
                                // evaluation: bus defaults overlaid with the
                                // bind-site generic overrides (`BusAxi4#(READ=0)`)
                                // and then the DUT port's own override
                                // (`port s: target BusAxi4<WRITE=0>`), which is
                                // authoritative for which gated channels
                                // `arch build` actually flattened. The bind name
                                // equals the DUT port name by convention.
                                let env = bus_param_env_with_port_override(
                                    &bus_decl,
                                    l.ty.as_ref(),
                                    e.dut_bus_port_overrides.get(&l.name.name),
                                );
                                e.bus_param_envs.insert(l.name.name.clone(), env);
                                e.bus_bindings
                                    .insert(l.name.name.clone(), (bus_decl, buf, prefix.clone()));
                                // Populate the per-bind signal remap so hookable-
                                // method emission (which precedes `emit_let`) can
                                // resolve `bus.<ch>.<sig>` against the override
                                // table. Without this pre-pass, transactor
                                // bodies use only the prefix-convention name and
                                // the override never fires for indirectly-routed
                                // accesses.
                                if !l.bind_remap.is_empty() {
                                    let mut map: std::collections::HashMap<
                                        (String, String),
                                        String,
                                    > = std::collections::HashMap::new();
                                    for entry in &l.bind_remap {
                                        if entry.path.len() == 2 {
                                            map.insert(
                                                (
                                                    entry.path[0].name.clone(),
                                                    entry.path[1].name.clone(),
                                                ),
                                                entry.port.clone(),
                                            );
                                        }
                                    }
                                    e.bus_remap.insert(prefix, map);
                                }
                            }
                        }
                    }
                }
            }
        }
        for it in &test.items {
            if let TestItem::Let(l) = it {
                if l.bind {
                    if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                        let bound_decl_to_bus = e
                            .components
                            .get(simple)
                            .and_then(|c| c.bound_to.as_ref().map(|_| ()))
                            .is_some()
                            || e.transactors
                                .get(simple)
                                .and_then(|t| t.bound_to.as_ref().map(|_| ()))
                                .is_some();
                        if bound_decl_to_bus {
                            if let Some(v) = &l.value {
                                if let ExprKind::Ident(rhs) = &*v.kind {
                                    if let Some(binding) = e.bus_bindings.get(&rhs.name).cloned() {
                                        // First binding wins; multi-instance is deferred.
                                        e.driver_bus_for_hookables
                                            .entry(simple.to_string())
                                            .or_insert(binding);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Hookable methods on regular components (agent / env /
        // sequencer / scoreboard) plus transactors. Transactor hookables
        // are emitted via the synth ComponentDecl so the existing
        // emit_component_method path finds the same struct shape.
        //
        // Emission order: dependency-sorted (`component_order` from
        // above) — a method body that calls `field.method(...)`
        // lowers to `<FieldType>_<method>(self.field, ...)`, so the
        // referenced lambda must be declared before the calling
        // lambda's `[&]` capture. Source-order emission breaks for
        // a transactor whose field type appears later in the file
        // (issue #301); the topo sort guarantees the callee's
        // lambda is in scope at the caller's capture.
        for &idx in &component_order {
            match &file.items[idx] {
                Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                    for ci in &c.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_component_method(c, h, 1);
                            emitted_any_method = true;
                        }
                        if let ComponentItem::Watchdog(w) = ci {
                            e.emit_watchdog(c, w, 1);
                            emitted_any_method = true;
                        }
                    }
                }
                Item::Transactor(t) => {
                    // include_active = true so any hookable inside `when
                    // active` is also emitted. Active-only hookables on a
                    // passive instance still compile but won't be invoked
                    // at runtime (no input event firing).
                    let synth = synth_component_from_transactor(t, /*include_active*/ true);
                    for ci in &synth.items {
                        if let ComponentItem::Hookable(h) = ci {
                            e.emit_component_method(&synth, h, 1);
                            emitted_any_method = true;
                        }
                        if let ComponentItem::Watchdog(w) = ci {
                            e.emit_watchdog(&synth, w, 1);
                            emitted_any_method = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if emitted_any_method {
            writeln!(e.out, "").ok();
        }

        // ── Scheduler — declared before hoisted lets so bound coroutine
        // drivers (Phase 2b) can register their slots inline at the
        // hoisted-let site. The run coroutine's slot is added below
        // alongside its body.
        writeln!(e.out, "{INDENT}harc_rt::ThreadScheduler sched;").ok();

        // Other lets get hoisted up front. Bound coroutine drivers also
        // emit their slot + per-driver transaction queue + actor
        // coroutine here (see the `let drv : Drv = bind axil` arm in
        // emit_let).
        for l in &other_lets {
            e.emit_let(l, 1);
        }

        // Custom `phase <name> ... end phase <name>` blocks from the test
        // lifecycle body (spec §7.2). Emitted after hoisted lets so
        // phase bodies can reference test-level env/agent/scoreboard
        // instances. Calls of the form `<name>()` from inside `run` lower as
        // plain C++ function calls; `wait` inside the phase takes the sync
        // `tick()` path because the lambda body emits with `in_coroutine =
        // false`.
        if !custom_phases.is_empty() {
            for (name, body) in &custom_phases {
                e.pad(1);
                writeln!(e.out, "auto {} = [&]() -> void {{", name.name).ok();
                e.emit_block(body, 2);
                e.pad(1);
                writeln!(e.out, "}};").ok();
            }
            writeln!(e.out, "").ok();
        }

        // ── Coroutine wrap for the test body ───────────────────────────────
        //
        // The whole test body (bare stmts + scope sim/{setup,run,check,
        // teardown}) becomes a single C++20 coroutine driven by
        // `harc_rt::ThreadScheduler`. Setting `in_coroutine = true` flips
        // the wait/tick lowering inside this scope to emit
        // `co_await harc_rt::wait_cycles(_slot, N)` instead of the
        // synchronous `for (...) tick();` form.
        //
        // The lambda captures by reference (`[&]`) so it sees `dut`,
        // `tick`, `cycle_count`, `_checkers`, hookable-method lambdas,
        // and any hoisted lets defined above in `main`'s scope.
        //
        // After the coroutine is constructed, `sched.bootstrap()` resumes
        // every initially-Ready slot once (the run setup statements,
        // plus any bound-driver actors' first wait_until on their queue).
        // The main loop then drives the clock until the run coroutine is
        // `Done` — driver coroutines may still be parked in WaitUntil
        // (queue empty) at that point; abandoning them is intentional
        // since the test is over.
        writeln!(e.out, "{INDENT}harc_rt::ThreadSlot _run_slot;").ok();
        writeln!(e.out, "{INDENT}sched.slots.push_back(&_run_slot);").ok();
        writeln!(
            e.out,
            "{INDENT}auto _run_slot_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{"
        )
        .ok();

        e.in_coroutine = true;
        for it in &test.items {
            match it {
                TestItem::Stmt(s) => e.emit_stmt(s, 2),
                TestItem::Scope(s) => {
                    if let Some(b) = &s.setup {
                        e.emit_block(b, 2);
                    }
                    if let Some(b) = &s.run {
                        e.emit_block(b, 2);
                    }
                    if let Some(b) = &s.check {
                        e.emit_block(b, 2);
                    }
                    if let Some(b) = &s.teardown {
                        e.emit_block(b, 2);
                    }
                }
                _ => {}
            }
        }
        e.in_coroutine = false;

        writeln!(e.out, "{INDENT}{INDENT}co_return;").ok();
        writeln!(e.out, "{INDENT}}};").ok();
        writeln!(
            e.out,
            "{INDENT}_run_slot.thread = _run_slot_lambda(&_run_slot);"
        )
        .ok();
        writeln!(e.out, "").ok();

        // `actor_threads` is populated only when `--mt` is set (cooperative
        // mode pushes actor slots into the global `sched` instead). So
        // `mt` here means "we have per-actor schedulers needing barrier
        // sync"; cooperative mode skips the worker spawn / barrier dance
        // entirely even when actors are present.
        let n_actors = e.actor_threads.len();
        let mt = n_actors > 0;
        debug_assert!(mt == (e.mt && !e.actor_threads.is_empty()));
        let _ = e.mt; // suppress unused warning when no actors

        // ── Bootstrap ──────────────────────────────────────────────────────
        // Single-threaded — workers haven't started yet. Each scheduler
        // (main + per-actor) runs its initially-Ready slots once until
        // they hit their first co_await.
        writeln!(
            e.out,
            "{INDENT}// Resume each coroutine once so initial-setup statements run"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// before the first clock edge. Single-threaded — workers"
        )
        .ok();
        writeln!(e.out, "{INDENT}// haven't been spawned yet.").ok();
        writeln!(e.out, "{INDENT}sched.bootstrap();").ok();
        if mt {
            for (sched_var, _) in &e.actor_threads {
                writeln!(e.out, "{INDENT}{sched_var}.bootstrap();").ok();
            }
        }
        writeln!(e.out, "").ok();

        if mt {
            // ── Phase 3a multi-thread topology ──────────────────────────
            // Each actor coroutine runs on its own `std::thread`. Per
            // posedge: main runs the run-coroutine, two atomic-spin
            // barriers synchronize main with N worker threads, each
            // worker runs `_<n>_sched.tick()` once. Then main does
            // `dut->eval()` (single-threaded — Verilator-generated DUT
            // code is not MT-safe) and runs `_checkers`. The dual
            // barrier mirrors arch-com's Phase 3 design (`Barrier` class
            // shared in `harc_thread_rt.h`); cycle batching to amortize
            // barrier cost is Phase 3b.
            writeln!(
                e.out,
                "{INDENT}// Phase 3a: per-actor OS threads with dual barrier sync."
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// {} actor(s) → {} barrier participants (main + workers).",
                n_actors,
                n_actors + 1
            )
            .ok();
            writeln!(e.out, "{INDENT}std::atomic<bool> _shutdown{{false}};").ok();
            writeln!(
                e.out,
                "{INDENT}harc_rt::Barrier _start_barrier({});",
                n_actors + 1
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}harc_rt::Barrier _end_barrier({});",
                n_actors + 1
            )
            .ok();
            writeln!(e.out, "{INDENT}std::vector<std::thread> _workers;").ok();
            for (sched_var, _) in &e.actor_threads {
                writeln!(e.out, "{INDENT}_workers.emplace_back([&]() {{").ok();
                writeln!(e.out, "{INDENT}{INDENT}while (true) {{").ok();
                writeln!(e.out, "{INDENT}{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(
                    e.out,
                    "{INDENT}{INDENT}{INDENT}if (_shutdown.load(std::memory_order_acquire)) break;"
                )
                .ok();
                writeln!(e.out, "{INDENT}{INDENT}{INDENT}{sched_var}.tick();").ok();
                writeln!(e.out, "{INDENT}{INDENT}{INDENT}_end_barrier.wait();").ok();
                writeln!(e.out, "{INDENT}{INDENT}}}").ok();
                writeln!(e.out, "{INDENT}}});").ok();
            }
            writeln!(e.out, "").ok();
        }

        writeln!(
            e.out,
            "{INDENT}// Drive the clock until the run coroutine completes."
        )
        .ok();
        writeln!(e.out, "{INDENT}//").ok();
        writeln!(
            e.out,
            "{INDENT}// `wait N cycles` matches Verilog's `@(posedge clk)` semantic:"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// values set in the segment BEFORE the wait are sampled at the"
        )
        .ok();
        writeln!(e.out, "{INDENT}// next posedge. Per loop iteration:").ok();
        writeln!(
            e.out,
            "{INDENT}//   1. Posedge (clk 0→1, eval) — DUT FFs latch the current input"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}//      values (set in the previous segment, or in bootstrap on"
        )
        .ok();
        writeln!(e.out, "{INDENT}//      the first iteration).").ok();
        writeln!(
            e.out,
            "{INDENT}//   2. `sched.tick()` — advance the run coroutine to its next"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}//      wait, setting the inputs for the NEXT cycle's posedge."
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}//   3. Falling edge (clk 1→0, eval) — comb re-settles with the"
        )
        .ok();
        writeln!(e.out, "{INDENT}//      newly-set inputs.").ok();
        writeln!(e.out, "{INDENT}//   4. Cycle counter + checkers.").ok();
        writeln!(
            e.out,
            "{INDENT}// One initial `eval(clk=0)` before the loop settles combinational"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// logic with the bootstrap inputs — same role as `initial`-block"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// settle in Verilog. Each `wait 1 cycle` then maps to exactly"
        )
        .ok();
        writeln!(
            e.out,
            "{INDENT}// one posedge that observes the just-set values."
        )
        .ok();
        if mt {
            writeln!(e.out, "{INDENT}//").ok();
            writeln!(
                e.out,
                "{INDENT}// MT mode: workers run between tick() and the falling edge,"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// gated by _start_barrier / _end_barrier. Run-coroutine writes"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// complete BEFORE workers wake → no race on shared queues."
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}// Workers' DUT-input writes complete BEFORE the falling-edge"
            )
            .ok();
            writeln!(e.out, "{INDENT}// eval → no race on signal state.").ok();
        }
        if clocks.is_empty() {
            // Initial comb settle — bootstrap's inputs propagate through
            // combinational logic before the first posedge.
            writeln!(e.out, "{INDENT}_harc_eval_negedge(dut);").ok();
            writeln!(
                e.out,
                "{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {{"
            )
            .ok();
            // Posedge first — latches current input values.
            writeln!(e.out, "{INDENT}{INDENT}_harc_eval_posedge(dut);").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)(cycle_count + 1));"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}cycle_count++;").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}for (auto& _svc : _post_eval_services) _svc();"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}if (!_post_eval_services.empty()) dut->eval();"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}if (!_post_eval_services.empty()) _harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
            )
            .ok();
            // Then advance the run coroutine for the next cycle's inputs.
            writeln!(e.out, "{INDENT}{INDENT}sched.tick();").ok();
            if mt {
                writeln!(e.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(e.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
            }
            // Falling edge + comb resettle with the new inputs.
            writeln!(e.out, "{INDENT}{INDENT}_harc_eval_negedge(dut);").ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}_harc_trace_dump_next(\"clk\", (uint64_t)cycle_count);"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}}").ok();
        } else {
            // Multi-clock: initial bare eval() to settle combinational
            // logic with bootstrap inputs (no clock advancement). The
            // loop's eval_clocks_until then advances time by one full
            // primary-clock period per iteration. Posedge-vs-tick ordering
            // is constrained by eval_clocks_until's atomic per-edge eval
            // — we tick AFTER eval_clocks_until so the run coroutine
            // observes the just-completed cycle's outputs and sets the
            // next cycle's inputs in time for the following iteration's
            // first edge. Same effect as the single-clock branch, just
            // with the clock toggling factored into eval_clocks_until.
            writeln!(e.out, "{INDENT}dut->eval();").ok();
            writeln!(
                e.out,
                "{INDENT}_harc_trace_dump_at((uint64_t)now_ps, \"\", 0);"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}while (_run_slot.kind != harc_rt::WaitKind::Done && !_fatal) {{"
            )
            .ok();
            writeln!(
                e.out,
                "{INDENT}{INDENT}long long _target = now_ps + clocks_[0].half_period_ps * 2;"
            )
            .ok();
            writeln!(e.out, "{INDENT}{INDENT}eval_clocks_until(_target);").ok();
            writeln!(e.out, "{INDENT}{INDENT}sched.tick();").ok();
            if mt {
                writeln!(e.out, "{INDENT}{INDENT}_start_barrier.wait();").ok();
                writeln!(e.out, "{INDENT}{INDENT}_end_barrier.wait();").ok();
            }
            writeln!(e.out, "{INDENT}{INDENT}for (auto& _c : _checkers) _c();").ok();
            writeln!(e.out, "{INDENT}}}").ok();
        }

        if mt {
            // Shutdown sequence: workers are blocked on _start_barrier
            // (their next iteration). Set _shutdown, wake them via the
            // start barrier; they observe the flag and break out of their
            // loop without reaching _end_barrier. Then join.
            writeln!(e.out, "").ok();
            writeln!(
                e.out,
                "{INDENT}_shutdown.store(true, std::memory_order_release);"
            )
            .ok();
            writeln!(e.out, "{INDENT}_start_barrier.wait();").ok();
            writeln!(e.out, "{INDENT}for (auto& _w : _workers) _w.join();").ok();
        }
        writeln!(e.out, "").ok();

        // Final + return.
        writeln!(e.out, "").ok();
        writeln!(e.out, "{INDENT}for (auto& _r : _auto_cov_reports) _r();").ok();
        // Property-cover summary (ARCH-style: header line + per-point lines
        // with `*NOT HIT*` marker; stdout destination — see covergroup
        // report() for the rationale on stdout vs stderr).
        if !e.covers.is_empty() {
            let n_covers = e.covers.len();
            writeln!(e.out, "{INDENT}{{").ok();
            writeln!(e.out, "{INDENT}{INDENT}uint64_t _cov_total = {n_covers};").ok();
            writeln!(e.out, "{INDENT}{INDENT}uint64_t _cov_hit = 0;").ok();
            let covers_clone = e.covers.clone();
            for c in &covers_clone {
                writeln!(
                    e.out,
                    "{INDENT}{INDENT}if (_cov_{tag}_hits > 0) _cov_hit++;",
                    tag = c.tag
                )
                .ok();
            }
            writeln!(
                e.out,
                "{INDENT}{INDENT}harc_rt::log::harc_print_cover_summary(_cov_hit, _cov_total);"
            )
            .ok();
            for c in &covers_clone {
                writeln!(e.out,
                "{INDENT}{INDENT}harc_rt::log::harc_print_cover_point(\"{label}\", _cov_{tag}_hits);",
                tag = c.tag, label = escape_c(&c.label)).ok();
            }
            writeln!(e.out, "{INDENT}}}").ok();
        }
        writeln!(e.out, "{INDENT}dut->final();").ok();
        // Verilator coverage write — the runtime macro is a no-op unless the
        // TB was built with `harc sim --coverage` (which sets `--coverage` on
        // verilator → defines `VM_COVERAGE=1`).
        writeln!(
            e.out,
            "{INDENT}HARC_RT_WRITE_COVERAGE(Verilated::threadContextp()->coveragep());"
        )
        .ok();
        // Waveform tracer teardown (issue #209). Must precede
        // `delete dut` because `tfp->close()` writes the
        // end-of-trace marker via the trace dispatcher held by the
        // DUT root. Skipped via the same compile-time gate as
        // tracer construction.
        writeln!(e.out, "{INDENT}HARC_RT_CLOSE_WAVE_TRACE(tfp);").ok();
        writeln!(e.out, "{INDENT}delete dut;").ok();
        writeln!(e.out, "").ok();
        writeln!(
            e.out,
            "{INDENT}return harc_rt::log::harc_finish_sim_run(log_ctx, trace, cycle_count, errors);"
        )
        .ok();
        writeln!(e.out, "}}").ok();
        writeln!(e.out, "").ok();
    } // end of `for test in &tests`

    // Dispatcher `main()` (Phase 1b). One branch per test: the
    // dispatcher reads `--test <name>` from argv (preferred) or the
    // `HARC_TEST` env var, then `return run_<TestName>(argc, argv)`.
    // When unset, dispatches to the alphabetically-first test
    // (deterministic — matches the sort key in merge_for_sim).
    writeln!(e.out, "int main(int argc, char** argv) {{").ok();
    writeln!(
        e.out,
        "{INDENT}const char* test_sel = harc_rt::log::harc_select_test(argc, argv);"
    )
    .ok();
    // Default (no --test, no HARC_TEST) → run the first test.
    let first_test = &tests[0].name.name;
    writeln!(
        e.out,
        "{INDENT}if (!test_sel) return run_{first_test}(argc, argv);"
    )
    .ok();
    // Branches for every test.
    for t in &tests {
        let n = &t.name.name;
        writeln!(
            e.out,
            "{INDENT}if (std::strcmp(test_sel, \"{n}\") == 0) return run_{n}(argc, argv);"
        )
        .ok();
    }
    // Build the human-readable "available tests" list for the
    // unknown-test error message.
    let avail: Vec<&str> = tests.iter().map(|t| t.name.name.as_str()).collect();
    let avail_csv = avail.join(", ");
    writeln!(
        e.out,
        "{INDENT}harc_rt::log::harc_report_unknown_test(test_sel, \"{avail_csv}\");"
    )
    .ok();
    writeln!(e.out, "{INDENT}return 1;").ok();
    writeln!(e.out, "}}").ok();

    if !e.errors.is_empty() {
        return Err(EmitError(e.errors.join("\n")));
    }
    Ok(e.out)
}

/// Build a synthetic `ComponentDecl` view of a transactor for codegen
/// reuse. The transactor's always-present `items` + (optionally) its
/// `when_active` items become a single component-shaped item list that
/// existing emit paths (`emit_component_struct`,
/// `emit_bound_monitor_actors`, `try_emit_bound_driver_actor`) can
/// consume without modification. `include_active = false` produces the
/// passive view: only the always-present body, no `when active` fields
/// or handlers — which is what gets used for `let xact : T passive`
/// instantiations so passive instances don't even attempt to spawn the
/// active driver actor.
///
/// `kind: ComponentKind::Driver` is a placeholder — the existing
/// component codegen paths only consult `kind` for the tag prefix in
/// diagnostic strings; correctness doesn't depend on the choice.
fn synth_component_from_transactor(t: &TransactorDecl, include_active: bool) -> ComponentDecl {
    let mut items = t.items.clone();
    if include_active {
        if let Some(active) = &t.when_active {
            items.extend(active.clone());
        }
    }
    ComponentDecl {
        kind: ComponentKind::Transactor,
        name: t.name.clone(),
        params: t.params.clone(),
        bound_to: t.bound_to.clone(),
        items,
        span: t.span,
        doc: t.doc.clone(),
        inner_doc: t.inner_doc.clone(),
    }
}

fn type_simple_name(t: Option<&TypeExpr>) -> Option<&str> {
    match t? {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

/// Compute a bus's effective const-param environment at a bind site.
///
/// Starts from the bus declaration's param defaults (`BusDecl::params`) and
/// applies any named generic overrides from the bind-site type expression
/// (`BusAxi4#(READ=0, WRITE=1)` → {READ:0, WRITE:1}). This is the env the
/// `generate_if` gate is evaluated against, so HARC's notion of which gated
/// signals are present matches `arch build`'s flatten exactly.
///
/// Only const-foldable params land in the env (literals / arithmetic over
/// other params). A param whose value can't be folded is omitted; a gate
/// referencing it then fails to evaluate and the signal is conservatively
/// treated as PRESENT (see `gate_passes`) — never silently dropped.
fn bus_param_env(
    bus: &BusDecl,
    bind_ty: Option<&TypeExpr>,
) -> std::collections::HashMap<String, i64> {
    let mut env: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    // Defaults first, in declaration order (later params may reference earlier
    // ones, e.g. `STRB_W = DATA_W/8`).
    for p in &bus.params {
        if let Some(d) = &p.default {
            if let Some(v) = eval_const_i64(d, &env) {
                env.insert(p.name.name.clone(), v);
            }
        }
    }
    // Bind-site named overrides win over defaults.
    if let Some(TypeExpr::Named { generics, .. }) = bind_ty {
        for g in generics {
            if let TypeArg::Named { name, value } = g {
                if let Some(v) = eval_const_i64(value, &env) {
                    env.insert(name.name.clone(), v);
                }
            }
        }
    }
    env
}

/// Like `bus_param_env`, but additionally layers a DUT-port-level override
/// (`port s: target BusRw<WRITE=0>` in the DUT's `.arch`/`.archi`) onto the env.
///
/// Precedence, lowest to highest:
///   1. bus param defaults,
///   2. HARC-TB bind-site generic (`let s : BusRw<...> = bind dut`, the `...`),
///   3. DUT-port override (the port's own `<...>`).
///
/// The DUT port's override is authoritative because it reflects which
/// `generate_if`-gated channels `arch build` *actually* flattened into the SV
/// port set for that port. In the realistic case the DUT carries the override
/// and the harc TB does not restate it, so (2) is empty and (3) is the only
/// non-default layer; when both name the same param, (3) wins so harc agrees
/// with `arch build` rather than with a stale TB restatement. Overrides that
/// can't be folded are simply not inserted, leaving the gate conservatively
/// PRESENT (same fallback as `gate_passes`).
pub(crate) fn bus_param_env_with_port_override(
    bus: &BusDecl,
    bind_ty: Option<&TypeExpr>,
    port_override: Option<&std::collections::HashMap<String, i64>>,
) -> std::collections::HashMap<String, i64> {
    let mut env = bus_param_env(bus, bind_ty);
    if let Some(ov) = port_override {
        for (k, v) in ov {
            env.insert(k.clone(), *v);
        }
    }
    env
}

/// Parse DUT `.arch`/`.archi` interface files and extract per-port bus param
/// overrides, keyed by DUT port name. A port declaration of the form
///
///   port <name>: <initiator|target> <BusName>[<P=v, ...>] [as Vec<...>];
///   port <name>: <initiator|target> Vec<BusName<P=v, ...>, N>;
///
/// contributes `{ <name>: { P: v, ... } }`. The structural prefix
/// (`port <name>: <persp>`) is matched line-wise; the type tail
/// (`BusName<...>` / `Vec<BusName<...>, N>`) is handed to the real type-expr
/// parser (`parse_type_expr_fragment`) so the `<P=v>` and `Vec<>` forms are
/// parsed by the same grammar harc#344/#345 already rely on, not a regex.
///
/// `.archi` is preferred when a sibling exists next to a `.arch` input, since
/// it is the post-elaboration authoritative interface (#567). When only the
/// `.arch` source is present, its port declaration carries the identical
/// override line, so scanning it directly is equally correct.
///
/// Only named value params that fold to an i64 are recorded — anything that
/// can't be folded is dropped, leaving the gate conservatively PRESENT.
pub fn dut_bus_port_overrides_from_files(
    dut_files: &[std::path::PathBuf],
) -> std::collections::HashMap<String, std::collections::HashMap<String, i64>> {
    let mut out: std::collections::HashMap<String, std::collections::HashMap<String, i64>> =
        std::collections::HashMap::new();
    // Resolve each `.arch` input to its sibling `.archi` if present.
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for f in dut_files {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if f.extension().and_then(|e| e.to_str()) == Some("arch") {
            let archi = f.with_extension("archi");
            if archi.exists() {
                candidates.push(archi);
            }
        }
        candidates.push(f.clone());
        for cand in candidates {
            if !seen.insert(cand.clone()) {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&cand) else {
                continue;
            };
            collect_port_overrides_from_src(&src, &mut out);
            // First successfully-read candidate (prefer .archi) is enough for
            // this input; the .arch fallback only matters when no .archi.
            break;
        }
    }
    out
}

/// Scan one interface source string for `port <name>: <persp> <bus-ty>;` lines
/// and fold any bus param overrides into `out`. Defined separately so the
/// unit tests can drive it with an in-memory string.
fn collect_port_overrides_from_src(
    src: &str,
    out: &mut std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
) {
    for raw in src.lines() {
        // Strip line comments and trim.
        let line = match raw.find("//") {
            Some(i) => &raw[..i],
            None => raw,
        };
        let line = line.trim();
        let Some(rest) = line.strip_prefix("port ") else {
            continue;
        };
        // `<name>: <persp> <ty>;`
        let Some(colon) = rest.find(':') else {
            continue;
        };
        let name = rest[..colon].trim();
        if name.is_empty() || !is_simple_ident(name) {
            continue;
        }
        let after_colon = rest[colon + 1..].trim();
        // Perspective keyword distinguishes a bus port from a scalar/vec port.
        let ty_tail = if let Some(t) = after_colon.strip_prefix("initiator ") {
            t
        } else if let Some(t) = after_colon.strip_prefix("target ") {
            t
        } else {
            continue;
        };
        // Drop the trailing `;` and anything after it; also drop a trailing
        // `as Vec<...>` / `comb_dep_on(...)` decoration — only the bus type
        // (and its `<P=v>`) carries the override.
        let ty_tail = ty_tail.trim();
        let ty_tail = ty_tail.split(';').next().unwrap_or(ty_tail).trim();
        if ty_tail.is_empty() {
            continue;
        }
        // Unwrap a `Vec<ELEM, N>` array-of-bus form down to its element bus
        // type. harc's type-arg grammar does not accept a nested
        // `BusName<P=v>` as a Vec element (it parses the element as a value
        // expression), so we strip the `Vec<` / trailing `, N>` wrapper
        // textually and hand the element string to the real type-expr parser —
        // which DOES parse a top-level `BusName<P=v>` correctly. The structural
        // unwrap is trivial; the `<P=v>` parse stays with the parser.
        let elem_str = vec_element_type_str(ty_tail).unwrap_or_else(|| ty_tail.to_string());
        let Ok(te) = crate::parser::parse_type_expr_fragment(&elem_str) else {
            continue;
        };
        let generics = bus_port_generics(&te);
        let Some(generics) = generics else {
            continue;
        };
        let mut env: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        // Fold in declaration order so a later param may reference an earlier
        // one (mirrors `bus_param_env`'s default-folding pass).
        for g in generics {
            if let TypeArg::Named { name: pn, value } = g {
                if let Some(v) = eval_const_i64(value, &env) {
                    env.insert(pn.name.clone(), v);
                }
            }
        }
        if !env.is_empty() {
            out.entry(name.to_string()).or_default().extend(env);
        }
    }
}

/// Extract the bus element's generic-arg list from a parsed port type:
/// `BusName<...>` → its generics; `Vec<BusName<...>, N>` → the element's
/// generics. Returns `None` for non-bus/non-generic forms.
fn bus_port_generics(te: &TypeExpr) -> Option<&[TypeArg]> {
    match te {
        TypeExpr::Named { generics, .. } => Some(generics.as_slice()),
        TypeExpr::Builtin {
            name: BuiltinTy::Vec,
            args,
            ..
        } => {
            // First type-arg is the element type; descend into it.
            args.iter().find_map(|a| match a {
                TypeArg::Type(inner) => bus_port_generics(inner),
                _ => None,
            })
        }
        _ => None,
    }
}

/// If `ty` is a `Vec<ELEM, N>` array-of-bus form, return the `ELEM` substring
/// (the element bus type, e.g. `BusAxi4<WRITE=0>`). The element is everything
/// inside the outer `Vec<...>` up to the LAST top-level comma (the `, N` count).
/// Angle-bracket depth is tracked so a nested `BusName<P=v>` is not split.
/// Returns `None` if `ty` is not a `Vec<...>` form (a bare `BusName<...>` port
/// type passes straight through to the parser unchanged).
fn vec_element_type_str(ty: &str) -> Option<String> {
    let inner = ty.strip_prefix("Vec")?.trim_start();
    let inner = inner.strip_prefix('<')?;
    let inner = inner.strip_suffix('>')?;
    // Find the last top-level (depth-0) comma — separates ELEM from the count.
    let mut depth = 0i32;
    let mut last_comma: Option<usize> = None;
    for (i, c) in inner.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => last_comma = Some(i),
            _ => {}
        }
    }
    let cut = last_comma?;
    Some(inner[..cut].trim().to_string())
}

/// `true` iff `s` is a single ARCH identifier (no whitespace / punctuation).
fn is_simple_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Decide whether a gated bus signal is present under an effective param env.
///
/// `None` gate ⇒ ungated ⇒ always present. `Some(cond)` ⇒ present iff `cond`
/// folds to a non-zero value. If the condition can't be folded (references a
/// param not in the env, or uses an operator the mini-evaluator doesn't
/// handle), we return `true` (present) — the same conservative default the
/// pre-fix behavior had, so we never regress a working corpus design into a
/// missing-port error. The common ARCH forms (`generate_if READ`,
/// `generate_if WRITE`, `generate_if READ > 0`) all fold.
pub(crate) fn gate_passes(
    gate: Option<&Expr>,
    env: &std::collections::HashMap<String, i64>,
) -> bool {
    match gate {
        None => true,
        Some(cond) => match eval_const_i64(cond, env) {
            Some(v) => v != 0,
            None => true,
        },
    }
}

/// Minimal const-expression evaluator over an i64 param env. Handles the
/// literal/identifier/paren/unary/binary forms that appear in bus param
/// defaults and `generate_if` conditions. Returns `None` for anything it
/// can't fold (unknown ident, unsupported op, division by zero) so callers
/// fall back to their conservative default.
fn eval_const_i64(e: &Expr, env: &std::collections::HashMap<String, i64>) -> Option<i64> {
    match &*e.kind {
        ExprKind::Paren(inner) => eval_const_i64(inner, env),
        ExprKind::Ident(id) => env.get(&id.name).copied(),
        ExprKind::Int(s) => {
            let stripped = s.replace('_', "");
            if let Some(rest) = stripped
                .strip_prefix("0x")
                .or_else(|| stripped.strip_prefix("0X"))
            {
                i64::from_str_radix(rest, 16).ok()
            } else if let Some(rest) = stripped
                .strip_prefix("0b")
                .or_else(|| stripped.strip_prefix("0B"))
            {
                i64::from_str_radix(rest, 2).ok()
            } else {
                stripped.parse::<i64>().ok()
            }
        }
        ExprKind::Bool(b) => Some(if *b { 1 } else { 0 }),
        ExprKind::Unary { op, expr } => {
            let v = eval_const_i64(expr, env)?;
            match op {
                UnaryOp::Neg => Some(v.wrapping_neg()),
                UnaryOp::Not | UnaryOp::NotKw => Some(if v == 0 { 1 } else { 0 }),
                UnaryOp::BitNot => Some(!v),
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let a = eval_const_i64(lhs, env)?;
            let b = eval_const_i64(rhs, env)?;
            let bool_i = |x: bool| Some(if x { 1 } else { 0 });
            match op {
                BinaryOp::Add | BinaryOp::AddWrap => Some(a.wrapping_add(b)),
                BinaryOp::Sub | BinaryOp::SubWrap => Some(a.wrapping_sub(b)),
                BinaryOp::Mul | BinaryOp::MulWrap => Some(a.wrapping_mul(b)),
                BinaryOp::Div => (b != 0).then(|| a / b),
                BinaryOp::Mod => (b != 0).then(|| a % b),
                BinaryOp::Shl => Some(a.wrapping_shl(b as u32)),
                BinaryOp::Shr => Some(a.wrapping_shr(b as u32)),
                BinaryOp::BitAnd => Some(a & b),
                BinaryOp::BitOr => Some(a | b),
                BinaryOp::BitXor => Some(a ^ b),
                BinaryOp::Eq => bool_i(a == b),
                BinaryOp::Ne => bool_i(a != b),
                BinaryOp::Lt => bool_i(a < b),
                BinaryOp::Le => bool_i(a <= b),
                BinaryOp::Gt => bool_i(a > b),
                BinaryOp::Ge => bool_i(a >= b),
                BinaryOp::AndAnd | BinaryOp::AndKw => bool_i(a != 0 && b != 0),
                BinaryOp::OrOr | BinaryOp::OrKw => bool_i(a != 0 || b != 0),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Collect every method-name string that appears as the callee of
/// a `Call { callee: Field { name, .. }, .. }` expression anywhere
/// inside the given block (recursively). Used by
/// `topo_sort_component_indices` to discover cross-component
/// hookable calls that don't go through a field-typed receiver — see
/// arch-com#447 §8.
fn collect_called_method_names(block: &Block, out: &mut std::collections::HashSet<String>) {
    fn visit_expr(e: &Expr, out: &mut std::collections::HashSet<String>) {
        if let ExprKind::Call { callee, args } = &*e.kind {
            if let ExprKind::Field { name, .. } = &*callee.kind {
                out.insert(name.name.clone());
            }
            visit_expr(callee, out);
            for a in args {
                let arg_expr = match a {
                    CallArg::Expr(ex) => ex,
                    CallArg::Named { value, .. } => value,
                };
                visit_expr(arg_expr, out);
            }
            return;
        }
        match &*e.kind {
            ExprKind::Field { target, .. }
            | ExprKind::Cast { expr: target, .. }
            | ExprKind::Unary { expr: target, .. }
            | ExprKind::HashHash { expr: target, .. }
            | ExprKind::SeqRepeat { expr: target, .. }
            | ExprKind::ForkCall { call: target } => {
                visit_expr(target, out);
            }
            ExprKind::Index { target, index } => {
                visit_expr(target, out);
                visit_expr(index, out);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                visit_expr(target, out);
                visit_expr(hi, out);
                visit_expr(lo, out);
            }
            ExprKind::Send { target, value } => {
                visit_expr(target, out);
                visit_expr(value, out);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                visit_expr(lhs, out);
                visit_expr(rhs, out);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                visit_expr(cond, out);
                visit_expr(then_branch, out);
                visit_expr(else_branch, out);
            }
            ExprKind::RangeLit { lo, hi } => {
                if let Some(e) = lo {
                    visit_expr(e, out);
                }
                if let Some(e) = hi {
                    visit_expr(e, out);
                }
            }
            // Leaf / unsupported-for-our-purposes kinds — no nested
            // `Expr` children that could host a call we'd care about,
            // or non-call shapes (literals, idents, etc.).
            _ => {}
        }
    }
    fn visit_block(b: &Block, out: &mut std::collections::HashSet<String>) {
        for s in &b.stmts {
            visit_stmt(s, out);
        }
    }
    fn visit_stmt(s: &Stmt, out: &mut std::collections::HashSet<String>) {
        match &s.kind {
            StmtKind::Let(l) => {
                if let Some(e) = &l.value {
                    visit_expr(e, out);
                }
            }
            StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
                visit_expr(target, out);
                visit_expr(value, out);
            }
            StmtKind::For(f) => {
                visit_expr(&f.iter, out);
                visit_block(&f.body, out);
            }
            StmtKind::Repeat(r) => {
                visit_expr(&r.count, out);
                visit_block(&r.body, out);
            }
            StmtKind::Loop(b) => visit_block(b, out),
            StmtKind::While { cond, body, .. } => {
                visit_expr(cond, out);
                visit_block(body, out);
            }
            StmtKind::If(i) => {
                visit_expr(&i.cond, out);
                visit_block(&i.then_block, out);
                for (c, b) in &i.elsifs {
                    visit_expr(c, out);
                    visit_block(b, out);
                }
                if let Some(b) = &i.else_block {
                    visit_block(b, out);
                }
            }
            StmtKind::Fork(f) => {
                for b in &f.branches {
                    visit_block(b, out);
                }
            }
            StmtKind::Parallel(bs) | StmtKind::Schedule(bs) => {
                for b in bs {
                    visit_block(b, out);
                }
            }
            StmtKind::Select(arms) => {
                for arm in arms {
                    visit_expr(&arm.event, out);
                    visit_block(&arm.action, out);
                }
            }
            StmtKind::Emit { args, .. }
            | StmtKind::Log { args, .. }
            | StmtKind::LogF { args, .. } => {
                for a in args {
                    let e = match a {
                        CallArg::Expr(ex) => ex,
                        CallArg::Named { value, .. } => value,
                    };
                    visit_expr(e, out);
                }
            }
            StmtKind::Yield(e) | StmtKind::Release(e) => visit_expr(e, out),
            StmtKind::Return(opt) => {
                if let Some(e) = opt {
                    visit_expr(e, out);
                }
            }
            StmtKind::Randomize {
                target, with_body, ..
            } => {
                visit_expr(target, out);
                for e in with_body {
                    visit_expr(e, out);
                }
            }
            StmtKind::Expr(e) => visit_expr(e, out),
            StmtKind::After { duration, body, .. } => {
                visit_expr(duration, out);
                visit_block(body, out);
            }
            StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
                if let Some(e) = &v.expr {
                    visit_expr(e, out);
                }
            }
            StmtKind::On(h) => {
                visit_block(&h.body, out);
            }
            // `JoinAll`, `Break`, `Continue`, `Apply` have no Expr/Block
            // children we need to visit for call discovery; ditto for
            // any future variant — when a new variant lands the topo
            // sort safely under-covers it (existing field-type edges
            // still apply).
            _ => {}
        }
    }
    visit_block(block, out);
}

/// Topologically sort the indices of `file.items` that hold a
/// component-shaped declaration (`Agent` / `Env` / `Sequencer` /
/// `Scoreboard` / `Transactor`) so that every by-value field-type
/// reference between two items in the set names a type emitted
/// earlier in the returned order. Items whose types are not part of
/// the set (DUT modules, transactions, structs, enums, covergroups)
/// are ignored for dependency purposes — those have their own emit
/// ordering before this set lands. Non-matching items are dropped
/// entirely from the result; the caller iterates and pattern-matches
/// to dispatch (consumers do `match` on `Item::Agent(_) | Item::Env(_)
/// | Item::Sequencer(_) | Item::Scoreboard(_) | Item::Transactor(_)`).
///
/// Ordering rules (Kahn-style with source-order tie-breaking):
/// 1. By-value field types: an item appears before another item iff
///    the second's field list references the first's type by simple
///    name.
/// 2. Hookable-call targets (arch-com#447 §8): if an item's body
///    contains a call `<recv>.<method>()` and exactly one *other*
///    eligible item declares a `hookable` named `<method>`, an edge
///    from that item to the caller is added. This catches call
///    dependencies that don't manifest as field-type references —
///    e.g. a transactor body resolving the receiver via an outer
///    scope or a future cross-scope binding. Rule (1) already
///    transitively covers the field-rooted call chains; rule (2) is
///    the safety net for the non-field case the existing graph
///    didn't model.
///
/// Source order breaks ties so fixtures with no cross-dependencies
/// still emit in the order users wrote them — diffs against the
/// pre-sort emitter stay minimal.
///
/// Cycle handling: a by-value cycle (`A { b : B }` ↔ `B { a : A }`)
/// is structurally invalid C++ (infinite size) and rejected by the
/// language-level check elsewhere; if one slips through, the
/// remaining nodes fall through in source order so codegen still
/// emits something and the C++ compiler surfaces the cycle as a
/// downstream error rather than us silently dropping items.
///
/// This sort fixes issue #301: a transactor field whose type is
/// another transactor defined later in the source list. Prior to
/// this, both the C++ struct definition and the hookable-method
/// lambda for the owning transactor referenced the later type as
/// an undeclared symbol.
pub fn topo_sort_component_indices(file: &SourceFile) -> Vec<usize> {
    // First pass: enumerate eligible items and build the name → index
    // map. Eligibility = anything we emit a C++ `struct` and/or
    // hookable-method lambdas for.
    let mut name_to_idx: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut eligible: Vec<usize> = Vec::new();
    for (i, it) in file.items.iter().enumerate() {
        let name = match it {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                c.name.name.clone()
            }
            Item::Transactor(t) => t.name.name.clone(),
            _ => continue,
        };
        name_to_idx.insert(name, i);
        eligible.push(i);
    }
    // Build `method_owners`: hookable-method-name → set of eligible
    // item indices that declare a `hookable` (or `function` —
    // `is_hookable=false`) with that name. Consulted by the
    // call-target edge rule below. Includes `when_active` items for
    // transactors so an active-only hookable still counts.
    let mut method_owners: std::collections::HashMap<String, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    for &i in &eligible {
        let items: &[ComponentItem] = match &file.items[i] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => &c.items,
            Item::Transactor(t) => &t.items,
            _ => continue,
        };
        let when_active_items: Option<&[ComponentItem]> = match &file.items[i] {
            Item::Transactor(t) => t.when_active.as_deref(),
            _ => None,
        };
        for ci in items.iter().chain(when_active_items.into_iter().flatten()) {
            if let ComponentItem::Hookable(h) = ci {
                method_owners
                    .entry(h.name.name.clone())
                    .or_default()
                    .insert(i);
            }
        }
    }
    // Second pass: collect outgoing edges. Two rules:
    //  (1) field-type rule — i depends on j when item i has a field
    //      whose simple type name matches item j's name. This is the
    //      structural cause: a by-value field needs the callee
    //      `struct` complete at the use site.
    //  (2) call-target rule (arch-com#447 §8) — i depends on j when
    //      i's body contains a call `<recv>.<method>()` and exactly
    //      one *other* eligible item j declares a `hookable` named
    //      `<method>`. This catches cross-item call edges that don't
    //      manifest as field-type references — without it, source
    //      order is the only thing keeping the C++ lambdas in
    //      dependency order. Ambiguous methods (multiple owners)
    //      are skipped, since which owner the call actually resolves
    //      to is decided at emit time by `resolve_component_method_call`
    //      against the current scope, and we don't have scope here.
    //      An ambiguous case will still resolve correctly if the
    //      receiver's type is reachable through fields (rule 1).
    let mut deps: std::collections::HashMap<usize, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    for &i in &eligible {
        let items: &[ComponentItem] = match &file.items[i] {
            Item::Agent(c) | Item::Env(c) | Item::Sequencer(c) | Item::Scoreboard(c) => &c.items,
            Item::Transactor(t) => &t.items,
            _ => continue,
        };
        // Transactors also have an active-only body; field references
        // there matter too (e.g. an `active` driver might hold a
        // helper transactor instance).
        let when_active_items: Option<&[ComponentItem]> = match &file.items[i] {
            Item::Transactor(t) => t.when_active.as_deref(),
            _ => None,
        };
        let mut entry = std::collections::HashSet::new();
        let record = |t: &TypeExpr, entry: &mut std::collections::HashSet<usize>| {
            if let Some(n) = type_simple_name(Some(t)) {
                if let Some(&j) = name_to_idx.get(n) {
                    if j != i {
                        entry.insert(j);
                    }
                }
            }
        };
        let mut called_methods: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for ci in items.iter().chain(when_active_items.into_iter().flatten()) {
            match ci {
                ComponentItem::Field(f) => {
                    record(&f.ty, &mut entry);
                }
                ComponentItem::Hookable(h) => {
                    collect_called_method_names(&h.body, &mut called_methods);
                }
                ComponentItem::OnHandler(h) => {
                    collect_called_method_names(&h.body, &mut called_methods);
                }
                ComponentItem::TargetTlmThread(t) => {
                    collect_called_method_names(&t.body, &mut called_methods);
                }
                ComponentItem::Lifecycle(_phase, body) => {
                    collect_called_method_names(body, &mut called_methods);
                }
                ComponentItem::Watchdog(w) => {
                    collect_called_method_names(&w.body, &mut called_methods);
                }
                ComponentItem::Connect(_) | ComponentItem::Apply(_) => {}
            }
        }
        // Apply rule (2): for each method called, if exactly one
        // *other* component owns a hookable by that name, add the
        // edge. Self-edges (same i) are skipped — same-component
        // calls are intra-item and don't reorder structs.
        //
        // TODO(arch-com#463 §8): the ambiguous-owner branch
        // (`externals.len() > 1`) currently drops the edge
        // silently. This is conservative-correct today because
        // `resolve_component_method_call` picks the receiver-
        // type-matching owner at emit time and the field rule
        // will have already added an edge to that owner — provided
        // the receiver's type is field-reachable. If the receiver
        // is NOT field-reachable (e.g. obtained via another
        // hookable call), the dependency degrades to source order.
        // Two design choices on the table:
        //   (a) widen rule 2 to resolve the receiver's type at
        //       graph-build time and add the resolved owner's
        //       edge, OR
        //   (b) emit a compile error on ambiguous unrooted calls.
        // Pinned by `transactor_topo_sort_skips_ambiguous_hookable_call_edges`
        // in `tests/codegen.rs`; pick a direction before any
        // codegen feature lands that legitimately wants the
        // edge added.
        for m in &called_methods {
            if let Some(owners) = method_owners.get(m) {
                let externals: Vec<usize> = owners.iter().copied().filter(|&j| j != i).collect();
                if externals.len() == 1 {
                    entry.insert(externals[0]);
                }
            }
        }
        deps.insert(i, entry);
    }
    // Kahn's algorithm with source-order tie-breaking. Indegrees are
    // taken from the reverse of `deps` (an edge j → i means j must
    // be emitted before i). We seed the work-list with indegree-0
    // nodes in source order; popping always takes the smallest-index
    // ready node so unrelated items keep their source position.
    let mut indegree: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &i in &eligible {
        indegree.insert(i, 0);
    }
    for &i in &eligible {
        if let Some(d) = deps.get(&i) {
            // `deps[i]` is the set of items i depends on (i must be
            // emitted after each j ∈ deps[i]). Treating those as edges
            // j → i, i's indegree is exactly |deps[i]|.
            *indegree.entry(i).or_insert(0) += d.len();
        }
    }
    // Build the reverse adjacency: for each j, which i's depend on j?
    let mut reverse: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for &i in &eligible {
        if let Some(d) = deps.get(&i) {
            for &j in d {
                reverse.entry(j).or_default().push(i);
            }
        }
    }
    let mut ready: Vec<usize> = eligible
        .iter()
        .copied()
        .filter(|i| indegree.get(i).copied().unwrap_or(0) == 0)
        .collect();
    ready.sort();
    let mut order: Vec<usize> = Vec::with_capacity(eligible.len());
    while let Some(pos) = ready
        .iter()
        .enumerate()
        .min_by_key(|(_, &idx)| idx)
        .map(|(p, _)| p)
    {
        let i = ready.remove(pos);
        order.push(i);
        if let Some(children) = reverse.get(&i) {
            for &c in children {
                if let Some(deg) = indegree.get_mut(&c) {
                    if *deg > 0 {
                        *deg -= 1;
                        if *deg == 0 {
                            ready.push(c);
                        }
                    }
                }
            }
        }
    }
    if order.len() < eligible.len() {
        // Cycle in the field-type graph. Recover by appending any
        // still-unprocessed items in source order so emission isn't
        // silently truncated; the resulting C++ will surface the
        // cycle as an "incomplete type" error at the offending field
        // — which is the structurally correct diagnostic for a
        // by-value cycle.
        for &i in &eligible {
            if !order.contains(&i) {
                order.push(i);
            }
        }
    }
    order
}

/// One registered cover point — what gets reported at end of test.
#[derive(Debug, Clone)]
struct CoverInfo {
    tag: String,
    label: String,
}

struct ConnectHookableSink {
    comp_ty: String,
    instance: String,
    method: String,
}

#[derive(Debug, Clone)]
struct PendingTlmFork {
    root: String,
    component: String,
    bus: String,
    method: String,
    sig_prefix: String,
    ret_var: Option<String>,
    ret_ty: Option<TypeExpr>,
    tag: Option<u64>,
}

fn stmt_contains_return(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(_) => true,
        StmtKind::For(s) => s.body.stmts.iter().any(stmt_contains_return),
        StmtKind::Repeat(s) => s.body.stmts.iter().any(stmt_contains_return),
        StmtKind::Loop(b) => b.stmts.iter().any(stmt_contains_return),
        StmtKind::While { body, .. } => body.stmts.iter().any(stmt_contains_return),
        StmtKind::If(i) => {
            i.then_block.stmts.iter().any(stmt_contains_return)
                || i.elsifs
                    .iter()
                    .any(|(_, b)| b.stmts.iter().any(stmt_contains_return))
                || i.else_block
                    .as_ref()
                    .is_some_and(|b| b.stmts.iter().any(stmt_contains_return))
        }
        StmtKind::Fork(f) => f
            .branches
            .iter()
            .any(|b| b.stmts.iter().any(stmt_contains_return)),
        StmtKind::Parallel(branches) | StmtKind::Schedule(branches) => branches
            .iter()
            .any(|b| b.stmts.iter().any(stmt_contains_return)),
        StmtKind::Select(arms) => arms
            .iter()
            .any(|a| a.action.stmts.iter().any(stmt_contains_return)),
        StmtKind::On(h) => h.body.stmts.iter().any(stmt_contains_return),
        StmtKind::After { body, .. } => body.stmts.iter().any(stmt_contains_return),
        _ => false,
    }
}

enum WaitCondition<'a> {
    Expr(&'a Expr),
    Idle { instance: String, cycles: &'a Expr },
}

impl<'a> WaitCondition<'a> {
    fn label(&self) -> String {
        match self {
            WaitCondition::Expr(e) => expr_source_str(e),
            WaitCondition::Idle { instance, cycles } => {
                format!("{instance}.idle({})", expr_source_str(cycles))
            }
        }
    }

    fn emit(&self, e: &mut Emitter) {
        match self {
            WaitCondition::Expr(expr) => e.emit_expr(expr),
            WaitCondition::Idle { instance, cycles } => {
                e.emit_idle_predicate(instance, "idle", cycles);
            }
        }
    }
}

/// Width-typed transaction field info used by the Z3 solver lowering.
#[derive(Debug, Clone)]
struct TxnFieldInfo {
    name: String,
    path: Vec<String>,
    width: u32,
    signed: bool,
    enum_variants: Option<usize>,
    enum_variant_labels: Option<Vec<String>>,
    list: Option<ListFieldInfo>,
    /// `!` prefix on a transaction field — pinned to the current value during
    /// solver-backed randomize and skipped during model assignment.
    non_random: bool,
    attrs: Vec<Attr>,
    when_guard: Option<Expr>,
}

#[derive(Debug, Clone)]
struct ListFieldInfo {
    declared_max_len: Option<usize>,
    elem_width: u32,
    elem_signed: bool,
}

fn txn_field_info_from_field_path(
    f: &Field,
    path: Vec<String>,
    when_guard: Option<Expr>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> TxnFieldInfo {
    let list = list_field_info(&f.ty);
    let (width, signed, enum_variants, enum_variant_labels) = match &f.ty {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::Bits | BuiltinTy::UIntCap => {
                (type_arg_width(args).unwrap_or(64), false, None, None)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => {
                (type_arg_width(args).unwrap_or(64), true, None, None)
            }
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => (1, false, None, None),
            BuiltinTy::Int => (32, true, None, None),
            _ => (64, false, None, None),
        },
        TypeExpr::Named { name, .. } => {
            let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            if let Some(&n) = enums.get(last) {
                (
                    enum_width(n),
                    false,
                    Some(n),
                    enum_domains.get(last).cloned(),
                )
            } else {
                (0, false, None, None)
            }
        }
    };
    TxnFieldInfo {
        name: path.join("."),
        path,
        width,
        signed,
        enum_variants,
        enum_variant_labels,
        list,
        non_random: f.non_random,
        attrs: f.attrs.clone(),
        when_guard,
    }
}

fn record_type_name(t: &TypeExpr) -> Option<&str> {
    let TypeExpr::Named { name, .. } = t else {
        return None;
    };
    name.segments.last().map(|segment| segment.name.as_str())
}

fn txn_direct_fields(items: &[TxnBodyItem]) -> Vec<Field> {
    items
        .iter()
        .filter_map(|item| match item {
            TxnBodyItem::Field(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn txn_all_fields(items: &[TxnBodyItem]) -> Vec<Field> {
    let mut out = Vec::new();
    collect_txn_all_fields(items, &mut out);
    out
}

fn collect_txn_all_fields(items: &[TxnBodyItem], out: &mut Vec<Field>) {
    for item in items {
        match item {
            TxnBodyItem::Field(f) => out.push(f.clone()),
            TxnBodyItem::When(w) => collect_txn_all_fields(&w.items, out),
            TxnBodyItem::Keep(_) => {}
        }
    }
}

fn flatten_txn_body_field_infos(
    items: &[TxnBodyItem],
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<TxnFieldInfo> {
    let mut out = Vec::new();
    flatten_txn_body_field_infos_inner(
        items,
        Vec::new(),
        None,
        record_fields,
        enums,
        enum_domains,
        &mut out,
    );
    out
}

fn flatten_txn_body_field_infos_inner(
    items: &[TxnBodyItem],
    prefix: Vec<String>,
    when_guard: Option<Expr>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    for item in items {
        match item {
            TxnBodyItem::Field(f) => flatten_record_field_info_one(
                f,
                prefix.clone(),
                when_guard.clone(),
                record_fields,
                enums,
                enum_domains,
                out,
            ),
            TxnBodyItem::When(w) => {
                let next_guard = match &when_guard {
                    Some(g) => and_join(&[g.clone(), w.discriminant.clone()], w.span),
                    None => w.discriminant.clone(),
                };
                flatten_txn_body_field_infos_inner(
                    &w.items,
                    prefix.clone(),
                    Some(next_guard),
                    record_fields,
                    enums,
                    enum_domains,
                    out,
                );
            }
            TxnBodyItem::Keep(_) => {}
        }
    }
}

fn flatten_record_field_infos(
    fields: &[Field],
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<TxnFieldInfo> {
    let mut out = Vec::new();
    flatten_record_field_infos_inner(
        fields,
        Vec::new(),
        record_fields,
        enums,
        enum_domains,
        &mut out,
    );
    out
}

fn flatten_record_field_infos_inner(
    fields: &[Field],
    prefix: Vec<String>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    for f in fields {
        flatten_record_field_info_one(
            f,
            prefix.clone(),
            None,
            record_fields,
            enums,
            enum_domains,
            out,
        );
    }
}

fn flatten_record_field_info_one(
    f: &Field,
    prefix: Vec<String>,
    when_guard: Option<Expr>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
    enums: &std::collections::HashMap<String, usize>,
    enum_domains: &std::collections::HashMap<String, Vec<String>>,
    out: &mut Vec<TxnFieldInfo>,
) {
    let mut path = prefix;
    path.push(f.name.name.clone());
    if list_field_info(&f.ty).is_none() {
        if let Some(record) = record_type_name(&f.ty).and_then(|name| record_fields.get(name)) {
            if !enums.contains_key(record_type_name(&f.ty).unwrap_or_default()) {
                flatten_record_field_infos_inner(
                    record,
                    path,
                    record_fields,
                    enums,
                    enum_domains,
                    out,
                );
                return;
            }
        }
    }
    out.push(txn_field_info_from_field_path(
        f,
        path,
        when_guard,
        enums,
        enum_domains,
    ));
}

#[derive(Debug, Clone)]
struct AutoCoverageGoal {
    field: String,
    c_field: String,
    values: Vec<AutoCoverageValue>,
}

#[derive(Debug, Clone)]
struct AutoCoverageValue {
    label: String,
    c_expr: String,
    words: Vec<u32>,
}

impl AutoCoverageValue {
    fn unsigned(value: u64) -> Self {
        Self {
            label: value.to_string(),
            c_expr: format!("{value}ULL"),
            words: vec![value as u32, (value >> 32) as u32],
        }
    }

    fn signed(value: i64) -> Self {
        Self {
            label: value.to_string(),
            c_expr: if value == i64::MIN {
                "INT64_MIN".to_string()
            } else if value < 0 {
                format!("{value}LL")
            } else {
                format!("{value}LL")
            },
            words: vec![value as u32, ((value as u64) >> 32) as u32],
        }
    }

    fn from_words(label: String, words: Vec<u32>) -> Self {
        let c_expr = if words.len() <= 2 {
            let lo = words.first().copied().unwrap_or(0) as u64;
            let hi = words.get(1).copied().unwrap_or(0) as u64;
            format!("{}ULL", lo | (hi << 32))
        } else if words.len() <= 4 {
            let mut terms = Vec::new();
            for (idx, word) in words.iter().enumerate() {
                if *word == 0 {
                    continue;
                }
                terms.push(format!("((_harc_u128)0x{word:08x}ULL << {})", idx * 32));
            }
            if terms.is_empty() {
                "(_harc_u128)0".to_string()
            } else {
                format!("({})", terms.join(" | "))
            }
        } else {
            format!(
                "harc_rt::HarcWide<{}>({{{}}})",
                words.len(),
                words
                    .iter()
                    .map(|w| format!("0x{w:08x}u"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Self {
            label,
            c_expr,
            words,
        }
    }
}

const COVERGROUP_AUTO_CROSS_BIN_CAP: usize = 64;
const COVERGROUP_CROSS_MISSING_DETAIL_LIMIT: usize = 16;
const AUTO_COVERAGE_WALKING_BIT_CAP: usize = 16;

struct DeclaredCoverCross<'a> {
    storage: String,
    label: String,
    points: Vec<&'a CoverPoint>,
    total_bins: usize,
}

fn covergroup_binned_points(g: &CovergroupDecl) -> Vec<&CoverPoint> {
    g.items
        .iter()
        .filter_map(|it| match it {
            CoverItem::Point(p) if !p.bins.is_empty() => Some(p),
            _ => None,
        })
        .collect()
}

fn covergroup_auto_crosses(g: &CovergroupDecl) -> Vec<(&CoverPoint, &CoverPoint)> {
    let points = covergroup_binned_points(g);
    let declared_pairs: std::collections::BTreeSet<(String, String)> = g
        .items
        .iter()
        .filter_map(|it| match it {
            CoverItem::Cross(c) if c.points.len() == 2 => {
                let mut names = [c.points[0].name.clone(), c.points[1].name.clone()];
                names.sort();
                Some((names[0].clone(), names[1].clone()))
            }
            _ => None,
        })
        .collect();
    let mut crosses = Vec::new();
    for i in 0..points.len() {
        for j in (i + 1)..points.len() {
            let mut names = [points[i].name.name.clone(), points[j].name.name.clone()];
            names.sort();
            if declared_pairs.contains(&(names[0].clone(), names[1].clone())) {
                continue;
            }
            let bins = points[i].bins.len() * points[j].bins.len();
            if bins <= COVERGROUP_AUTO_CROSS_BIN_CAP {
                crosses.push((points[i], points[j]));
            }
        }
    }
    crosses
}

fn covergroup_declared_crosses(g: &CovergroupDecl) -> Result<Vec<DeclaredCoverCross<'_>>, String> {
    let mut crosses = Vec::new();
    for (cross_idx, item) in g.items.iter().enumerate() {
        let CoverItem::Cross(cross) = item else {
            continue;
        };
        if cross.points.len() < 2 {
            return Err(format!(
                "covergroup `{}` cross must name at least two coverpoints",
                g.name.name
            ));
        }
        let mut points = Vec::new();
        for ident in &cross.points {
            let point = g.items.iter().find_map(|item| match item {
                CoverItem::Point(p) if p.name.name == ident.name => Some(p),
                _ => None,
            });
            let Some(point) = point else {
                return Err(format!(
                    "covergroup `{}` cross references unknown coverpoint `{}`",
                    g.name.name, ident.name
                ));
            };
            if point.bins.is_empty() {
                return Err(format!(
                    "covergroup `{}` cross references coverpoint `{}` with no bins",
                    g.name.name, point.name.name
                ));
            }
            points.push(point);
        }
        let mut total_bins = 1usize;
        for p in &points {
            total_bins = total_bins.checked_mul(p.bins.len()).ok_or_else(|| {
                format!(
                    "covergroup `{}` cross `{}` has too many bins",
                    g.name.name,
                    cross
                        .points
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" x ")
                )
            })?;
        }
        let label = points
            .iter()
            .map(|p| p.name.name.as_str())
            .collect::<Vec<_>>()
            .join(" x ");
        let storage = format!(
            "_cross_{}_{}",
            cross_idx,
            points
                .iter()
                .map(|p| p.name.name.as_str())
                .collect::<Vec<_>>()
                .join("__")
        );
        crosses.push(DeclaredCoverCross {
            storage,
            label,
            points,
            total_bins,
        });
    }
    Ok(crosses)
}

fn declared_cross_index_expr(cross: &DeclaredCoverCross<'_>) -> String {
    let mut expr = "_i0".to_string();
    for (idx, point) in cross.points.iter().enumerate().skip(1) {
        expr = format!("({expr} * {} + _i{idx})", point.bins.len());
    }
    expr
}

fn declared_cross_bin_labels(cross: &DeclaredCoverCross<'_>) -> Vec<(usize, String)> {
    fn walk(
        cross: &DeclaredCoverCross<'_>,
        depth: usize,
        index: usize,
        labels: &mut Vec<String>,
        out: &mut Vec<(usize, String)>,
    ) {
        if depth == cross.points.len() {
            out.push((index, labels.join(" x ")));
            return;
        }
        let point = cross.points[depth];
        for (bin_idx, bin) in point.bins.iter().enumerate() {
            labels.push(format!("{}.{}", point.name.name, bin.name.name));
            walk(
                cross,
                depth + 1,
                index * point.bins.len() + bin_idx,
                labels,
                out,
            );
            labels.pop();
        }
    }

    let mut out = Vec::new();
    walk(cross, 0, 0, &mut Vec::new(), &mut out);
    out
}

fn field_attr_range(f: &TxnFieldInfo) -> Option<(&Expr, &Expr)> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "range" || a.args.len() < 2 {
            return None;
        }
        match (&a.args[0], &a.args[1]) {
            (AttrArg::Expr(lo), AttrArg::Expr(hi)) => Some((lo, hi)),
            _ => None,
        }
    })
}

fn field_attr_dist_entries(f: &TxnFieldInfo) -> Option<&Vec<DistEntry>> {
    f.attrs.iter().find_map(|a| {
        if a.name.name != "dist" {
            return None;
        }
        a.args.iter().find_map(|arg| match arg {
            AttrArg::Dist(entries) => Some(entries),
            _ => None,
        })
    })
}

fn field_attr_unique(f: &TxnFieldInfo) -> bool {
    f.attrs.iter().any(|a| a.name.name == "unique")
}

fn field_attr_unique_scope(f: &TxnFieldInfo) -> &str {
    f.attrs
        .iter()
        .find(|a| a.name.name == "unique")
        .and_then(|a| {
            a.args.iter().find_map(|arg| match arg {
                AttrArg::WithinScope(scope) => Some(scope.name.as_str()),
                _ => None,
            })
        })
        .unwrap_or("test")
}

fn c_scope_ident(scope: &str) -> String {
    scope
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn push_auto_coverage_value_unique(values: &mut Vec<AutoCoverageValue>, value: AutoCoverageValue) {
    if !values.iter().any(|v| v.words == value.words) {
        values.push(value);
    }
}

fn walking_auto_coverage_bit_positions(width: u32) -> Vec<u32> {
    if width == 0 {
        return Vec::new();
    }
    let width_usize = width as usize;
    let count = width_usize.min(AUTO_COVERAGE_WALKING_BIT_CAP);
    if count == width_usize {
        return (0..width).collect();
    }

    let mut positions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let max_pos = width_usize - 1;
    let max_idx = count - 1;
    for i in 0..count {
        let pos = (i * max_pos + (max_idx / 2)) / max_idx;
        if seen.insert(pos) {
            positions.push(pos as u32);
        }
    }
    positions
}

fn unsigned_max_words(width: u32) -> Vec<u32> {
    let word_count = (width as usize).div_ceil(32).max(1);
    let mut words = vec![0xffff_ffffu32; word_count];
    let rem = width % 32;
    if rem != 0 {
        if let Some(last) = words.last_mut() {
            *last = (1u32 << rem) - 1;
        }
    }
    words
}

fn solver_unsigned_mask_expr(width: u32) -> String {
    let words = unsigned_max_words(width);
    if words.len() <= 2 {
        let lo = words.first().copied().unwrap_or(0) as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        format!("(uint64_t)0x{:016x}ULL", lo | (hi << 32))
    } else if words.len() <= 4 {
        let mut terms = Vec::new();
        for (idx, word) in words.iter().enumerate() {
            if *word != 0 {
                terms.push(format!("((_harc_u128)0x{word:08x}ULL << {})", idx * 32));
            }
        }
        if terms.is_empty() {
            "(_harc_u128)0".to_string()
        } else {
            format!("({})", terms.join(" | "))
        }
    } else {
        format!(
            "harc_rt::HarcWide<{}>({{{}}})",
            words.len(),
            words
                .iter()
                .map(|w| format!("0x{w:08x}u"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn signed_min_words(width: u32) -> Vec<u32> {
    let mut words = vec![0u32; (width as usize).div_ceil(32).max(1)];
    let sign_bit = width.saturating_sub(1);
    words[(sign_bit / 32) as usize] = 1u32 << (sign_bit % 32);
    words
}

fn signed_max_words(width: u32) -> Vec<u32> {
    let mut words = unsigned_max_words(width);
    let sign_bit = width.saturating_sub(1);
    words[(sign_bit / 32) as usize] &= !(1u32 << (sign_bit % 32));
    words
}

fn decimal_label_for_auto_words(width: u32, words: &[u32]) -> String {
    if width <= 64 {
        let lo = words.first().copied().unwrap_or(0) as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        return (lo | (hi << 32)).to_string();
    }
    if words.iter().all(|w| *w == 0) {
        return "0".to_string();
    }
    if words.iter().enumerate().all(|(idx, w)| {
        let max = unsigned_max_words(width);
        max.get(idx).copied().unwrap_or(0) == *w
    }) {
        return format!("2^{}-1", width);
    }
    if let Some((idx, word)) = words
        .iter()
        .enumerate()
        .find(|(_, word)| **word != 0 && word.count_ones() == 1)
    {
        if words
            .iter()
            .enumerate()
            .all(|(other_idx, other)| other_idx == idx || *other == 0)
        {
            let bit = idx as u32 * 32 + word.trailing_zeros();
            return format!("2^{bit}");
        }
    }
    if words.iter().enumerate().all(|(idx, word)| {
        let mut max = unsigned_max_words(width);
        let diff = max.get_mut(idx).map(|m| {
            let d = *m ^ *word;
            d != 0 && d.count_ones() == 1
        });
        let this_is_diff = diff.unwrap_or(false);
        this_is_diff
            || max
                .get(idx)
                .copied()
                .map(|m| m == *word)
                .unwrap_or(*word == 0)
    }) {
        let max = unsigned_max_words(width);
        for (idx, (a, b)) in max.iter().zip(words.iter()).enumerate() {
            let diff = *a ^ *b;
            if diff != 0 && diff.count_ones() == 1 {
                let bit = idx as u32 * 32 + diff.trailing_zeros();
                return format!("2^{}-1-2^{bit}", width);
            }
        }
    }
    "wide".to_string()
}

fn auto_coverage_unsigned_word(value_words: Vec<u32>, width: u32) -> AutoCoverageValue {
    let label = decimal_label_for_auto_words(width, &value_words);
    AutoCoverageValue::from_words(label, value_words)
}

fn auto_coverage_signed_word(value_words: Vec<u32>, width: u32, is_min: bool) -> AutoCoverageValue {
    let label = if is_min {
        format!("-2^{}", width.saturating_sub(1))
    } else {
        format!("2^{}-1", width.saturating_sub(1))
    };
    AutoCoverageValue::from_words(label, value_words)
}

fn signed_domain_bound_exprs(width: u32) -> (String, String) {
    if width == 64 {
        ("INT64_MIN".to_string(), "INT64_MAX".to_string())
    } else if width < 64 {
        (
            format!("(int64_t)(-(1LL << {}))", width.saturating_sub(1)),
            format!("(int64_t)((1LL << {}) - 1)", width.saturating_sub(1)),
        )
    } else {
        (
            AutoCoverageValue::from_words(String::new(), signed_min_words(width)).c_expr,
            AutoCoverageValue::from_words(String::new(), signed_max_words(width)).c_expr,
        )
    }
}

fn solver_bv_value_call(value: &str, signed: bool, value_width: u32, solver_width: u32) -> String {
    if signed {
        format!("harc_z3_bv_signed_value(_ctx, {value}, {value_width}, {solver_width})")
    } else {
        format!("harc_z3_bv_value(_ctx, {value}, {solver_width})")
    }
}

fn natural_auto_coverage_endpoints(f: &TxnFieldInfo) -> Vec<AutoCoverageValue> {
    if f.width == 0 {
        return Vec::new();
    }

    if f.signed {
        if f.width > 64 {
            return vec![
                auto_coverage_signed_word(signed_min_words(f.width), f.width, true),
                auto_coverage_signed_word(signed_max_words(f.width), f.width, false),
            ];
        }
        let shift = f.width.saturating_sub(1);
        let lo = if shift >= 63 {
            i64::MIN
        } else {
            -(1i64 << shift)
        };
        let hi = if shift >= 63 {
            i64::MAX
        } else {
            (1i64 << shift) - 1
        };
        let mut values = vec![AutoCoverageValue::signed(lo)];
        if hi != lo {
            values.push(AutoCoverageValue::signed(hi));
        }
        values
    } else {
        let hi_words = unsigned_max_words(f.width);
        let mut values = Vec::new();
        push_auto_coverage_value_unique(
            &mut values,
            auto_coverage_unsigned_word(vec![0; hi_words.len()], f.width),
        );
        push_auto_coverage_value_unique(
            &mut values,
            auto_coverage_unsigned_word(hi_words.clone(), f.width),
        );
        for bit in walking_auto_coverage_bit_positions(f.width) {
            let mut one = vec![0u32; hi_words.len()];
            one[(bit / 32) as usize] = 1u32 << (bit % 32);
            push_auto_coverage_value_unique(&mut values, auto_coverage_unsigned_word(one, f.width));
            let mut inv = hi_words.clone();
            inv[(bit / 32) as usize] ^= 1u32 << (bit % 32);
            push_auto_coverage_value_unique(&mut values, auto_coverage_unsigned_word(inv, f.width));
        }
        values
    }
}

fn auto_coverage_values(f: &TxnFieldInfo) -> Vec<AutoCoverageValue> {
    let mut values = Vec::new();
    if let Some(n) = f.enum_variants {
        let labels = f.enum_variant_labels.as_deref();
        for i in 0..n {
            if let Some(label) = labels.and_then(|labels| labels.get(i)) {
                values.push(AutoCoverageValue {
                    label: label.clone(),
                    c_expr: format!("{i}ULL"),
                    words: vec![i as u32, 0],
                });
            } else {
                values.push(AutoCoverageValue::unsigned(i as u64));
            }
        }
    } else if f.width == 1 && !f.signed {
        values.extend([0, 1].map(AutoCoverageValue::unsigned));
    } else if let Some((lo, hi)) = field_attr_range(f) {
        if f.signed {
            if let (Some(lo), Some(hi)) = (fold_signed_int_literal(lo), fold_signed_int_literal(hi))
            {
                values.push(AutoCoverageValue::signed(lo));
                if hi != lo {
                    values.push(AutoCoverageValue::signed(hi));
                }
            }
        } else if let (Some(lo), Some(hi)) = (fold_int_literal(lo), fold_int_literal(hi)) {
            values.push(AutoCoverageValue::unsigned(lo));
            if hi != lo {
                values.push(AutoCoverageValue::unsigned(hi));
            }
        }
    } else {
        values.extend(natural_auto_coverage_endpoints(f));
    }
    values
}

/// Bits needed to hold `v` — a literal is self-sized, and `0` is one bit.
fn value_bit_width(v: u64) -> u32 {
    if v == 0 {
        1
    } else {
        64 - v.leading_zeros()
    }
}

/// Self-width of a literal operand in a constraint, for the §2.4 wrap
/// mask. Everything unsized is sized by value through the statement
/// path's own `parse_int_literal`, so the same literal sizes identically
/// in both positions.
///
/// An ARCH sized literal states its width outright, and that width is
/// what §2.4 masks at — the same declared-width rule a `const` follows,
/// so the two agree about a token like `8'd300` that can be spelled
/// either way. The digits are deliberately NOT consulted: nothing in the
/// lexer, parser or either backend truncates an overwide literal
/// (`c_int_literal` emits `4'hFF` as `0xFF`, harc#565), and letting the
/// digits widen the mask turned `keep len +% 4'hFF == 15` on a `uint<4>`
/// field from solvable into unsatisfiable. Not parsing them also keeps a
/// literal whose value overflows `u64` (`128'hFFFF...`) resolvable, where
/// its declared width is all the mask ever needed.
fn literal_operand_bit_width(s: &str) -> Option<u32> {
    let Some(idx) = s.find('\'') else {
        return crate::ir::lower::parse_int_literal(s).map(value_bit_width);
    };
    let declared: u32 = s[..idx].replace('_', "").parse().ok()?;
    // `0'h0` lexes. A zero-bit value can only be zero, which this
    // module sizes as one bit; rejecting it instead would fail a build
    // that succeeds today.
    Some(declared.max(1))
}

/// Raw declared width of a width-carrying builtin type, for a `const`'s
/// wrap-operand width. Deliberately NOT `cast_relabel_width`: that helper
/// caps at 128 and returns `None` both for "not a width type" and for
/// "out of range", and a caller that falls back on `None` cannot tell
/// those apart — `const K : uint<200> = 10` would silently take the
/// initializer's 4-bit value width instead of being rejected as
/// unrepresentable. A non-width type still returns `None`, which is a
/// real "no declared width" and correctly falls back.
fn declared_type_bit_width(t: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    match name {
        BuiltinTy::UInt
        | BuiltinTy::UIntCap
        | BuiltinTy::SInt
        | BuiltinTy::SIntCap
        | BuiltinTy::Bits => Some(type_arg_width(args).unwrap_or(64)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(1),
        BuiltinTy::Int => Some(32),
        _ => None,
    }
}

fn txn_field_solver_width(f: &TxnFieldInfo) -> u32 {
    f.list
        .as_ref()
        .map(|list| list.elem_width)
        .unwrap_or(f.width)
}

fn txn_field_solver_c_type(f: &TxnFieldInfo) -> String {
    if f.signed {
        cpp_sint_for_width(Some(f.width))
    } else {
        cpp_uint_for_width(Some(f.width))
    }
}

fn solver_scalar_c_type(width: u32, signed: bool) -> String {
    if signed {
        cpp_sint_for_width(Some(width))
    } else {
        cpp_uint_for_width(Some(width))
    }
}

fn auto_value_array_type(
    goal: &AutoCoverageGoal,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
) -> String {
    field_info
        .get(&goal.field)
        .map(txn_field_solver_c_type)
        .unwrap_or_else(|| "uint64_t".to_string())
}

fn auto_value_initializer(values: &[AutoCoverageValue]) -> String {
    values
        .iter()
        .map(|v| v.c_expr.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn emit_random_pref_expr(f: &TxnFieldInfo, salt: usize) -> String {
    if f.width <= 128 {
        format!(
            "harc_rt::random::harc_prefer_u128(_harc_rt_seed, {}, {})",
            salt, f.width
        )
    } else {
        let words = f.width.div_ceil(32);
        format!(
            "harc_rt::random::harc_prefer_wide<{}>(_harc_rt_seed, {}, {})",
            words, salt, f.width
        )
    }
}

fn emit_random_unsigned_expr(width: u32) -> String {
    if width <= 64 {
        format!("harc_rt::random::harc_rng_uint(harc_rng_next, {width})")
    } else if width <= 128 {
        format!("harc_rt::random::harc_rng_u128(harc_rng_next, {width})")
    } else {
        let words = width.div_ceil(32);
        format!(
            "harc_rt::random::harc_rng_wide<{}>(harc_rng_next, {width})",
            words
        )
    }
}

fn c_solver_int_literal(s: &str) -> String {
    let value = c_value_literal(s);
    if value.starts_with("harc_rt::HarcWide<") || value.contains("_harc_u128") {
        value
    } else {
        format!("(int64_t)({})", c_int_literal(s))
    }
}

fn c_ident(s: &str) -> String {
    s.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn randomize_target_field_name(
    target: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let field = expr_field_path(target, target_root)?;
    field_info.contains_key(&field).then_some(field)
}

fn randomize_target_ident(target: &Expr) -> Option<&str> {
    match &*target.kind {
        ExprKind::Ident(id) => Some(id.name.as_str()),
        ExprKind::Paren(inner) => randomize_target_ident(inner),
        _ => None,
    }
}

fn list_field_name_from_expr(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let field = expr_field_path(e, target_root)?;
    field_info
        .get(&field)
        .and_then(|info| info.list.as_ref().map(|_| field))
}

fn expr_field_path(e: &Expr, target_root: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    collect_expr_field_path(e, &mut parts)?;
    if parts.is_empty() {
        return None;
    }
    if let Some(root) = target_root {
        if parts.first().is_some_and(|part| part == root) {
            parts.remove(0);
        }
    }
    Some(parts.join("."))
}

fn expr_field_root(e: &Expr) -> Option<String> {
    let mut parts = Vec::new();
    collect_expr_field_path(e, &mut parts)?;
    parts.first().cloned()
}

fn collect_expr_field_path(e: &Expr, out: &mut Vec<String>) -> Option<()> {
    match &*e.kind {
        ExprKind::Ident(id) => out.push(id.name.clone()),
        ExprKind::ImplicitSelf => {}
        ExprKind::Field { target, name } => {
            collect_expr_field_path(target, out)?;
            out.push(name.name.clone());
        }
        ExprKind::Paren(inner) => collect_expr_field_path(inner, out)?,
        _ => return None,
    }
    Some(())
}

fn list_len_call_field_name(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<String> {
    let ExprKind::Call { callee, args } = &*e.kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let ExprKind::Field { target, name } = &*callee.kind else {
        return None;
    };
    if name.name != "len" {
        return None;
    }
    list_field_name_from_expr(target, field_info, target_root)
}

fn const_usize_from_constraint_expr(e: &Expr) -> Option<usize> {
    const_usize_expr(e)
}

fn list_len_upper_bound_from_expr(
    e: &Expr,
    field: &str,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> Option<usize> {
    match &*e.kind {
        ExprKind::Paren(inner) => {
            list_len_upper_bound_from_expr(inner, field, field_info, target_root)
        }
        ExprKind::Membership { expr, set } => {
            if list_len_call_field_name(expr, field_info, target_root).as_deref() != Some(field) {
                return None;
            }
            match &*set.kind {
                ExprKind::RangeLit { hi: Some(hi), .. } => const_usize_from_constraint_expr(hi),
                ExprKind::SetLit(items) => items
                    .iter()
                    .filter_map(|item| match &*item.kind {
                        ExprKind::RangeLit { hi: Some(hi), .. } => {
                            const_usize_from_constraint_expr(hi)
                        }
                        _ => const_usize_from_constraint_expr(item),
                    })
                    .max(),
                _ => const_usize_from_constraint_expr(set),
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            if matches!(op, BinaryOp::OrOr) && guarded_constraint_guard(e).is_some() {
                return list_len_upper_bound_from_expr(rhs, field, field_info, target_root);
            }
            let lhs_is_len =
                list_len_call_field_name(lhs, field_info, target_root).as_deref() == Some(field);
            let rhs_is_len =
                list_len_call_field_name(rhs, field_info, target_root).as_deref() == Some(field);
            match op {
                BinaryOp::Le if lhs_is_len => const_usize_from_constraint_expr(rhs),
                BinaryOp::Lt if lhs_is_len => {
                    const_usize_from_constraint_expr(rhs).and_then(|v| v.checked_sub(1))
                }
                BinaryOp::Eq if lhs_is_len => const_usize_from_constraint_expr(rhs),
                BinaryOp::Ge if rhs_is_len => const_usize_from_constraint_expr(lhs),
                BinaryOp::Gt if rhs_is_len => {
                    const_usize_from_constraint_expr(lhs).and_then(|v| v.checked_sub(1))
                }
                BinaryOp::Eq if rhs_is_len => const_usize_from_constraint_expr(lhs),
                BinaryOp::AndAnd | BinaryOp::AndKw => {
                    let a = list_len_upper_bound_from_expr(lhs, field, field_info, target_root);
                    let b = list_len_upper_bound_from_expr(rhs, field, field_info, target_root);
                    match (a, b) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn infer_list_unroll_bounds(
    fields: &[TxnFieldInfo],
    hard_constraints: &[&Expr],
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
) -> std::collections::HashMap<String, usize> {
    let mut out = std::collections::HashMap::new();
    for f in fields {
        let Some(list) = &f.list else {
            continue;
        };
        let mut bound = list.declared_max_len;
        for c in hard_constraints {
            if let Some(next) = list_len_upper_bound_from_expr(c, &f.name, field_info, target_root)
            {
                bound = Some(bound.map_or(next, |old| old.min(next)));
            }
        }
        if let Some(bound) = bound {
            out.insert(f.name.clone(), bound);
        }
    }
    out
}

fn collect_randomize_target_field_refs(
    e: &Expr,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
    out: &mut std::collections::HashSet<String>,
) {
    match &*e.kind {
        ExprKind::Ident(id) => {
            if field_info.contains_key(&id.name) {
                out.insert(id.name.clone());
            }
        }
        ExprKind::Field { target, .. } => {
            if let Some(field) = expr_field_path(e, target_root) {
                if field_info.contains_key(&field) {
                    out.insert(field);
                    return;
                }
            }
            collect_randomize_target_field_refs(target, field_info, target_root, out);
        }
        ExprKind::Index { target, index } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(index, field_info, target_root, out);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(hi, field_info, target_root, out);
            collect_randomize_target_field_refs(lo, field_info, target_root, out);
        }
        ExprKind::Call { callee, args } => {
            collect_randomize_target_field_refs(callee, field_info, target_root, out);
            for arg in args {
                match arg {
                    CallArg::Expr(e) | CallArg::Named { value: e, .. } => {
                        collect_randomize_target_field_refs(e, field_info, target_root, out)
                    }
                }
            }
        }
        ExprKind::ForEachConstraint { iter, body, .. } => {
            collect_randomize_target_field_refs(iter, field_info, target_root, out);
            for clause in body {
                collect_randomize_target_field_refs(clause, field_info, target_root, out);
            }
        }
        ExprKind::Cast { expr, .. }
        | ExprKind::Unary { expr, .. }
        | ExprKind::Paren(expr)
        | ExprKind::ForkCall { call: expr }
        | ExprKind::HashHash { expr, .. }
        | ExprKind::SeqRepeat { expr, .. } => {
            collect_randomize_target_field_refs(expr, field_info, target_root, out);
        }
        ExprKind::Membership { expr, set } => {
            collect_randomize_target_field_refs(expr, field_info, target_root, out);
            collect_randomize_target_field_refs(set, field_info, target_root, out);
        }
        ExprKind::Send { target, value }
        | ExprKind::Binary {
            lhs: target,
            rhs: value,
            ..
        } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            collect_randomize_target_field_refs(value, field_info, target_root, out);
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_randomize_target_field_refs(cond, field_info, target_root, out);
            collect_randomize_target_field_refs(then_branch, field_info, target_root, out);
            collect_randomize_target_field_refs(else_branch, field_info, target_root, out);
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo {
                collect_randomize_target_field_refs(lo, field_info, target_root, out);
            }
            if let Some(hi) = hi {
                collect_randomize_target_field_refs(hi, field_info, target_root, out);
            }
        }
        ExprKind::SetLit(items) => {
            for item in items {
                collect_randomize_target_field_refs(item, field_info, target_root, out);
            }
        }
        ExprKind::DistLit(entries) => {
            for entry in entries {
                collect_randomize_target_field_refs(&entry.value, field_info, target_root, out);
                collect_randomize_target_field_refs(&entry.weight, field_info, target_root, out);
            }
        }
        ExprKind::SystemCall { args, .. }
        | ExprKind::Randomize {
            with_body: args, ..
        } => {
            for arg in args {
                collect_randomize_target_field_refs(arg, field_info, target_root, out);
            }
        }
        ExprKind::SoftConstraint(sc) => {
            collect_randomize_target_field_refs(&sc.expr, field_info, target_root, out);
            if let Some(weight) = &sc.weight {
                collect_randomize_target_field_refs(weight, field_info, target_root, out);
            }
        }
        ExprKind::DistDirective { target, entries } => {
            collect_randomize_target_field_refs(target, field_info, target_root, out);
            for entry in entries {
                collect_randomize_target_field_refs(&entry.value, field_info, target_root, out);
                collect_randomize_target_field_refs(&entry.weight, field_info, target_root, out);
            }
        }
        ExprKind::NamedArg { value, .. } => {
            collect_randomize_target_field_refs(value, field_info, target_root, out);
        }
        ExprKind::StructLit { fields, .. } => {
            for field in fields {
                collect_randomize_target_field_refs(&field.value, field_info, target_root, out);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, count } => {
            collect_randomize_target_field_refs(lhs, field_info, target_root, out);
            collect_randomize_target_field_refs(rhs, field_info, target_root, out);
            if let Some(count) = count {
                collect_hash_count_field_refs(count, field_info, target_root, out);
            }
        }
        ExprKind::SolveOrder { args } => {
            for arg in args {
                collect_randomize_target_field_refs(arg, field_info, target_root, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::String(_)
        | ExprKind::Bool(_)
        | ExprKind::ImplicitSelf => {}
    }
}

fn collect_hash_count_field_refs(
    count: &HashCount,
    field_info: &std::collections::HashMap<String, TxnFieldInfo>,
    target_root: Option<&str>,
    out: &mut std::collections::HashSet<String>,
) {
    match count {
        HashCount::Const(e) => collect_randomize_target_field_refs(e, field_info, target_root, out),
        HashCount::Range { lo, hi } => {
            collect_randomize_target_field_refs(lo, field_info, target_root, out);
            collect_randomize_target_field_refs(hi, field_info, target_root, out);
        }
    }
}

/// Per-probe codegen state. The mangled read accessor is the bare
/// path `<TopModule>__DOT__harc_probes__DOT__<name>`. The `_drv`
/// and `_en` siblings (force probes only) are derived by appending
/// the suffix; see `ProbeAccessor::drive` / `enable`.
#[derive(Clone, Debug)]
struct ProbeAccessor {
    read: String,
    force: bool,
}

impl ProbeAccessor {
    fn drive(&self) -> String {
        format!("{}_drv", self.read)
    }
    fn enable(&self) -> String {
        format!("{}_en", self.read)
    }
}

struct Emitter {
    out: String,
    errors: Vec<String>,
    /// Transaction name → field metadata. Populated alongside `transactions`
    /// before main-body emission so the solver block can declare Z3 vars
    /// of the right widths and walk only the fields it should write back.
    txn_fields: std::collections::HashMap<String, Vec<TxnFieldInfo>>,
    /// Struct/transaction name → direct declared fields. Used for value-record
    /// C++ type lowering and packed TLM bridge helpers.
    record_fields: std::collections::HashMap<String, Vec<Field>>,
    /// Per-transaction `keep` constraints (spec §4). `randomize(t)`
    /// — bare or with `with …` — merges these into the Z3 solver
    /// block so the constraints are enforced. Empty entry / missing
    /// key means the transaction has no `keep`s; bare `randomize(t)`
    /// stays on the fast PRNG path.
    txn_keeps: std::collections::HashMap<String, Vec<Expr>>,
    /// Map from `probe_name` to the mangled accessor string used after
    /// `dut->rootp->`. Populated from the test's `let dut : T { probe ... }`
    /// block at emit start; consulted when lowering `dut.<X>` so that
    /// probe reads/writes route through the Verilator-bind stub instead of
    /// the (non-existent) top-level signal of the original DUT. Empty for
    /// probe-less tests — current 76 fixtures all sit in this bucket.
    probes: std::collections::HashMap<String, ProbeAccessor>,
    /// Declared bit-width of each `probe <name> : uint<W>` on the test's
    /// `let dut`. A probe read is the one `dut.<field>` shape that carries
    /// a static width, so it is the one wrapping-operator operand that
    /// resolves — TB-IR reads the same width off `PortRef::width`, and
    /// without this v1 rejected `dut.<probe> +% 1` as unknown-width while
    /// TB-IR wrapped it. Plain top-level ports stay width-erased in both.
    probe_widths: std::collections::HashMap<String, u32>,
    /// Names that more than one `let` in the current test declares.
    /// `let_widths` is keyed by bare source name with no scoping, so an
    /// inner shadow permanently clobbers the outer name's width. The
    /// direction checks have always lived with that; the narrowing check
    /// must not, or a legal `let b : uint<8> = a` after an inner
    /// `let a : uint<64>` is rejected on a width `a` never had at that
    /// point. Consulted only to suppress the narrowing check.
    shadowed_lets: std::collections::HashSet<String>,
    /// RAL regblock declarations indexed by type name. See ast.rs::
    /// `RegblockDecl`. Used to emit one POD mirror struct +
    /// `constexpr` address table per declared regblock at file scope,
    /// and to lower `regs.NAME` field accesses against the bound
    /// helper transactor at call sites.
    regblocks: std::collections::HashMap<String, RegblockDecl>,
    /// RAL addrmap declarations indexed by type name. Chip-level
    /// container; each instance owns one regblock at a distinct base
    /// address. See docs/ral-support.md §4.
    addrmaps: std::collections::HashMap<String, AddrmapDecl>,
    /// Per-test map from a `let regs : <RegblockType> = bind <helper>`
    /// variable name to its helper transactor variable name. Populated
    /// when emitting the test's `let`s; consulted when lowering
    /// `regs.<reg>` accessors so the emitted call resolves to
    /// `<helper>.write(addr, val)` / `<helper>.read(addr)`.
    let_helper: std::collections::HashMap<String, String>,
    /// Runtime problem id per concrete `randomize(...)` target span.
    /// Emitted as metadata touches only during the scaffold phase; current
    /// behavior still uses the existing PRNG / inline Z3 solve paths.
    runtime_randomize_problem_ids: std::collections::HashMap<(usize, usize), u32>,
    /// Free-standing `relation` declarations indexed by name (spec
    /// §4.2). `Call(Ident(R), args)` inside a constraint expression
    /// expands to R's body with formal parameters substituted by the
    /// call arguments. Block-body relations contribute one
    /// constraint per body expression; alias-body relations
    /// contribute one. Expansion is recursive so a relation that
    /// calls another relation flattens fully before reaching Z3.
    relations: std::collections::HashMap<String, RelationDecl>,
    /// Identifiers that are emitted as Verilator-class pointers (`VFoo*`).
    /// Field access on these uses `->` instead of `.`. Populated from
    /// `let x : <NamedType>` declarations and from function parameters
    /// whose type is a Named user type. Function-scoped: entries added by
    /// a function's params are popped when the function body finishes.
    pointer_vars: std::collections::HashSet<String>,
    /// Identifier → simple type name, for `randomize(t)` to resolve which
    /// `randomize_T()` function to call. Populated by `let t : T` and
    /// function parameters.
    let_types: std::collections::HashMap<String, String>,
    /// Per-test bit-widths for `let X : uint<W>` (and sint/bits)
    /// declarations. Used by the width-method intrinsics
    /// (`.trunc<N>()` / `.zext<N>()` / `.sext<N>()` / `.resize<N>()`)
    /// to (a) reject wrong-direction casts at codegen time and (b)
    /// give sext the correct source-width for its shift-fill. Lets
    /// without an explicit type (`let x = expr`) don't populate this
    /// map — width-direction checks then fall back to best-effort
    /// inference on the RHS expression.
    let_widths: std::collections::HashMap<String, u32>,
    /// DUT top-module port-name → per-lane bit-width for ports that
    /// flatten to a PACKED SystemVerilog vector (`Vec<Bus, N>` /
    /// multi-lane bus ports). Forwarded from `EmitOpts::vec_lane_widths`
    /// (built from the `--sv` port table). A `dut.port[i]` index whose
    /// port is in this map lowers via `harc_rt::harc_vec_lane_*<W>` so
    /// the lane access bit-extracts against Verilator's packed scalar
    /// while still indexing the ARCH native sim's C++ array.
    vec_lane_widths: std::collections::HashMap<String, u32>,
    /// Identifier → transactor mode, for let-bindings that carry a mode
    /// annotation (`let x : T active`, `let env : E passive`). The mode
    /// is recorded for the root let-name and forms the inheritance root
    /// for nested sub-component / sub-transactor fields; resolution at
    /// any path walks down field-by-field, with field-explicit modes
    /// overriding the inherited mode (same model as
    /// `emit_subcomponent_handler_registrations`). Used by the
    /// call-site passive-mode check (spec §8.1): a method call that
    /// resolves to a hookable inside `T.when_active` on a passive
    /// instance is a HARC-level error.
    let_modes: std::collections::HashMap<String, TransactorMode>,
    /// Set of transaction type names emitted in this file — guards against
    /// `randomize(t)` against a non-transaction type.
    transactions: std::collections::HashSet<String>,
    /// Set of plain struct record type names emitted in this file.
    /// Structs share the value-record C++ lowering with transactions;
    /// transactions add HVL/protocol semantics on top.
    structs: std::collections::HashSet<String>,
    /// Set of scoreboard type names emitted in this file. `let sb : X`
    /// where X is in this set lowers to `X sb;` (default-constructed)
    /// rather than the int64_t fallback.
    scoreboards: std::collections::HashSet<String>,
    /// Covergroup declarations indexed by name. `let cov : G` instantiates
    /// the struct and registers its `sample()` as a rising-edge `_checkers`
    /// closure on the primary clock. Bin counters live as `uint64_t` fields
    /// inside the struct; user-side access is `cov.cp_name.bin_name`.
    covergroups: std::collections::HashMap<String, CovergroupDecl>,
    /// Per-test list of registered `cover <expr>` points. Each entry pairs
    /// a unique tag (used to name the static hit counter in C++) with a
    /// human label for the report. Aggregated end-of-main report iterates
    /// the list and prints `[cover] H/T hit (X.X%)` plus per-point lines.
    covers: Vec<CoverInfo>,
    /// In-flight bare-identifier substitutions during monitor-body
    /// emission. Empty outside the monitor's handler emission. Lets the
    /// body reference its own fields by short name (`emit write_e(...)`)
    /// while the codegen prefixes with the let-instance name.
    field_subs: std::collections::HashMap<String, String>,
    /// Enum name → variant count; used when randomizing enum-typed fields.
    enums: std::collections::HashMap<String, usize>,
    /// Global enum-variant-name → numeric index. Used by the Z3
    /// constraint translator to resolve bare references to enum
    /// variants (e.g. `keep op != WRAP`) into their numeric encoding.
    enum_variants: std::collections::HashMap<String, i64>,
    /// File-scope integer constants. Randomize constraints may reference
    /// these by source name; emission lowers them to literal Z3 values.
    consts: std::collections::HashMap<String, String>,
    /// Declared bit-width of a `const`, kept beside `consts` because that
    /// map holds only the initializer text. §2.4 masks a wrap at the
    /// operands' declared widths, so sizing a `const` by the value of its
    /// initializer (`const BUMP : uint<16> = 10` → 4 bits) masks too
    /// narrowly and solves to a value the source constraint rejects.
    const_widths: std::collections::HashMap<String, u32>,
    /// Property name → body expression. Populated up-front from
    /// `Item::Property` declarations so `assert property NAME` can be
    /// resolved at the call site without a separate pass.
    properties: std::collections::HashMap<String, Expr>,
    /// In-flight substitutions for temporal SystemCall expressions during
    /// property emission. Keyed by AST span so the rewrite finds the right
    /// occurrence. Set/cleared around each `emit_property_check` call.
    prop_subs: std::collections::HashMap<(usize, usize), String>,
    /// Event name → C++ inner type (e.g. `uint64_t`). Populated when
    /// emitting `let e : event<T>`; consulted when emitting `emit e(arg)`
    /// and `on e(arg) ... end on` so the lambda gets the right param type.
    event_types: std::collections::HashMap<String, String>,
    /// Names of declared clocks in declaration order. Empty under
    /// single-clock backward-compat. Used by `wait N cycles on <clock>`
    /// to look up the clock's index in the runtime `clocks_` vector.
    clock_names: Vec<String>,
    /// Name of the C++ `std::vector` accumulator collecting `yield`-ed
    /// values inside the current `tseq` body. `Some("_result")` while
    /// emitting a tseq; `None` everywhere else (so a stray `yield`
    /// outside a tseq surfaces as a compile error rather than silently
    /// pushing into nothing).
    current_yield_target: Option<String>,
    /// Set of declared tseq names. Used by `let x = TseqName(...)` to
    /// pick `auto` over `int64_t` for the local's type — a tseq call
    /// returns `std::vector<T>`, not an integer.
    tseq_names: std::collections::HashSet<String>,
    /// Component declarations indexed by name. Covers `driver`,
    /// `agent`, `env`, `sequencer`, `scoreboard` — anything with
    /// fields + `hookable` methods. Scoreboards are also registered
    /// here (in addition to the legacy `scoreboards` set) so method
    /// dispatch and field-substitution work uniformly.
    components: std::collections::HashMap<String, ComponentDecl>,
    /// Bus declarations indexed by name. Populated from inline
    /// `bus Name { ... }` items + (future) `use Name` extern import.
    /// Consulted at let-time for `let var : BusName = bind <dut-expr>`
    /// to track the bus binding, and at expression-emit time for
    /// `var.signal` and `var.channel.signal` flat-naming.
    buses: std::collections::HashMap<String, BusDecl>,
    /// Active bus bindings in the current emit context. Each entry
    /// maps a source-level identifier to `(bus_decl, dut_root_expr,
    /// signal_prefix)`:
    ///   * `bus_decl`     — bus declaration whose `signals` /
    ///                      `handshakes` constrain typed access
    ///   * `dut_root_expr` — C++ expression text for the DUT root
    ///                       (typically `"dut"`)
    ///   * `signal_prefix` — name used for flat signal lookup on the
    ///                       DUT (e.g. `"axil"` for `dut->axil_aw_addr`)
    ///
    /// For the test-scope `let axil : BusAxiLite = bind dut`, the
    /// identifier and prefix are both `"axil"`. For the driver-bound
    /// alias `let drv = bind axil` (Phase 2), inside the driver's
    /// on-handler body the bare identifier `"bus"` maps to the same
    /// `(bus_decl, root, prefix)` tuple as `"axil"` — the prefix stays
    /// `"axil"` so signal access still resolves to the DUT-level flat
    /// names. Distinguishing the lookup key from the prefix is what
    /// makes the alias work.
    bus_bindings: std::collections::HashMap<String, (BusDecl, String, String)>,
    /// Effective const-param environment per bus bind, keyed by the same
    /// binding name as `bus_bindings`. Computed once at the real bind site
    /// from the bus's param defaults overlaid with the bind-site generic
    /// overrides (`BusAxi4#(READ=0)`). Consulted when resolving a bus signal
    /// access to evaluate the signal's `generate_if` gate — a gated-OFF
    /// signal is absent (matches `arch build`'s flatten). Missing entry ⇒
    /// fall back to the bus's defaults via `bus_param_env(&bus, None)`.
    bus_param_envs: std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    /// DUT-port-level bus param overrides, keyed by DUT port name (== bind
    /// name == SV signal prefix). Forwarded from `EmitOpts::dut_bus_port_overrides`,
    /// which is filled by parsing the DUT's `.arch`/`.archi` interface. Layered
    /// onto the bus defaults + bind-site generic when computing `bus_param_envs`
    /// for a bind whose name matches a DUT port — the port's own override is
    /// authoritative for which `generate_if`-gated channels `arch build`
    /// actually flattened. See `bus_param_env_with_port_override`.
    dut_bus_port_overrides:
        std::collections::HashMap<String, std::collections::HashMap<String, i64>>,
    /// Per-bind per-signal name override table populated from
    /// `bind <dut> with { ch.sig: "port" }` clauses. Outer key is
    /// the bind variable name (matching the `sig_prefix` field of
    /// `bus_bindings`); inner key is `(channel, signal)` tuple.
    /// Lookups consult this map first; missing entries fall through
    /// to the `<prefix>_<channel>_<signal>` convention. Empty for
    /// binds without a `with { ... }` clause — most fixtures sit
    /// in this bucket.
    bus_remap:
        std::collections::HashMap<String, std::collections::HashMap<(String, String), String>>,
    /// Pending RHS-fork TLM calls in the current run/method body. Populated
    /// by `let x = fork bus.method(...)` or discarded `fork bus.method(...)`
    /// and drained by `join_all`.
    pending_tlm_forks: Vec<PendingTlmFork>,
    /// Per `(bus prefix, method)` tag counter for RHS-fork calls to
    /// `tlm_method ... : out_of_order tags N`.
    next_tlm_fork_tag: std::collections::HashMap<(String, String), u64>,
    /// True while emitting statements *directly inside the test's run
    /// coroutine body* (the `scope sim/run` block plus bare test-level
    /// stmts). When set, `wait N cycles`, bare `tick()` (the bus
    /// handshake spin loops), and bus.send/recv lowerings emit
    /// `co_await harc_rt::wait_cycles(_slot, ...)` instead of the
    /// synchronous `for (...) tick();` form.
    ///
    /// Component method bodies, `on`-event-handler closures, tseq
    /// lambdas, and free functions all run synchronously between
    /// coroutine yields — they keep the sync `tick()` lowering. This
    /// works because they only execute while the run coroutine is
    /// "running" (between its co_awaits), so a sync `tick()` from
    /// inside a method does not race the scheduler.
    in_coroutine: bool,
    /// Phase 3a: list of `(scheduler_var, slot_var)` pairs for every
    /// actor coroutine that should run on its own OS thread. Each
    /// entry corresponds to a bound-driver or bound-monitor actor.
    /// At main-loop emission time, if non-empty, we wire up
    /// `_start_barrier` / `_end_barrier` (sized to `1 + len`) plus
    /// one `std::thread` per actor running the actor's
    /// per-os-thread `ThreadScheduler::tick()` between the barriers.
    /// Only populated when `mt = true`; in cooperative mode actor
    /// slots are pushed directly into the global `sched`.
    actor_threads: Vec<(String, String)>,
    /// True when `--mt` opt-in is set: emit per-actor schedulers,
    /// dual barriers, and worker thread spawns. False (default):
    /// actors join the global `sched` and tick cooperatively on
    /// the main thread. The cooperative path is faster on typical
    /// fixtures (per-cycle barrier sync exceeds per-cycle actor
    /// work on Apple Silicon — see the bench measurements in PR
    /// #16's commit message).
    mt: bool,
    /// Driver/agent type name → bus binding to use when emitting
    /// hookable method bodies on that type. Populated by a pre-scan
    /// of the test's `let X : Drv = bind axil` statements before
    /// hookable emission, so `bus.<ch>.send/recv` inside a hookable
    /// body resolves to the parent driver's bound bus. Single-
    /// instance per driver type (the first binding encountered);
    /// multi-instance support requires per-instance hookable
    /// emission and is deferred.
    driver_bus_for_hookables: std::collections::HashMap<String, (BusDecl, String, String)>,
    /// Transactor declarations indexed by name (spec §8.1). Looked
    /// up at `let xact : T mode = bind axil` time; codegen
    /// composes a synthetic ComponentDecl from `T.items` plus
    /// `T.when_active` (only when mode == Active) and reuses the
    /// existing bound-driver-actor + bound-monitor-actor lowering.
    transactors: std::collections::HashMap<String, TransactorDecl>,
    /// While we're emitting statements *inside a component instance's
    /// own body* — an `on <event>` subscriber, a bound-driver actor
    /// coroutine, a bound-monitor handshake actor — this names the
    /// fully-qualified instance path (`"ag"`, `"topenv.ag"`, etc.).
    /// `None` outside such bodies (test run code, free functions,
    /// hookable method bodies which use `self` instead).
    ///
    /// Used by the activity-tracking lowering to bump
    /// `<instance>._last_in_cycle` / `_last_out_cycle` at the sites
    /// where the framework knows an in/out has just happened:
    /// `emit ev(arg)`, `bus.<ch>.send(...)`, `bus.<ch>.recv()`. The
    /// in-handler entry bump uses the static `instance` parameter
    /// directly (already in scope), but `emit`/`send`/`recv` can
    /// appear nested inside arbitrary expressions and need this
    /// context to know whose heartbeat to bump.
    current_component_instance: Option<String>,
    /// While emitting a component method body, this records the HARC
    /// component type, C++ receiver expression, method name, and
    /// whether the method lives in `when active`, for bare sibling
    /// method calls such as `helper()` -> `Type_helper(self)`.
    current_component_method: Option<(String, String, String, bool)>,
}

/// RAII guard that temporarily overrides a single
/// `Option<(String, String, String, bool)>` slot — specifically
/// `Emitter::current_component_method` — and restores the prior
/// value on drop. Makes the install/restore invariant
/// compiler-enforced: any control-flow exit from the scope holding
/// the guard — `continue`, `return`, panic, `?`, fall-through —
/// runs the restore.
///
/// Used in `emit_component_handler_registrations_bound` where each
/// per-handler iteration has multiple exit paths (the
/// event-subscription `continue` arm and the fall-through
/// cycle-trigger arm), so that a future maintainer adding a third
/// exit doesn't have to remember a manual restore line.
///
/// The guard owns `&mut Emitter` and exposes it via
/// `Deref`/`DerefMut`, so the caller can keep using all of
/// `Emitter`'s API for the lifetime of the override. The per-handler
/// emit work is factored into a sibling helper (`emit_single_on_handler_bound`)
/// that takes `&mut self`; the caller drives that helper through the
/// guard, and restoration happens on drop after the helper returns
/// (or panics, or `?`s).
struct CurrentMethodGuard<'a> {
    emitter: &'a mut Emitter,
    saved: Option<(String, String, String, bool)>,
}

impl<'a> CurrentMethodGuard<'a> {
    fn new(emitter: &'a mut Emitter, new_value: Option<(String, String, String, bool)>) -> Self {
        let saved = std::mem::replace(&mut emitter.current_component_method, new_value);
        Self { emitter, saved }
    }
}

impl<'a> std::ops::Deref for CurrentMethodGuard<'a> {
    type Target = Emitter;
    fn deref(&self) -> &Emitter {
        self.emitter
    }
}

impl<'a> std::ops::DerefMut for CurrentMethodGuard<'a> {
    fn deref_mut(&mut self) -> &mut Emitter {
        self.emitter
    }
}

impl<'a> Drop for CurrentMethodGuard<'a> {
    fn drop(&mut self) {
        self.emitter.current_component_method = self.saved.take();
    }
}

impl Emitter {
    fn pad(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str(INDENT);
        }
    }

    /// Emit one TB-IR `ConstraintSite` as the v1 randomize C++ snippet.
    ///
    /// Mirrors v1's `StmtKind::Randomize` dispatch *after* the
    /// transaction-keep merge (the IR lowering already merged keeps ahead
    /// of the `with {...}` body and stored the result in `site`). So this
    /// runs the same `report_runtime_dependent_randomize_field_attrs`
    /// validation, the same solver-policy-field check, and routes to the
    /// same unconstrained-PRNG-shell or `emit_constraint_solver_block`
    /// path — producing byte-identical output to v1 for the site.
    pub(crate) fn emit_randomize_for_site(
        &mut self,
        site: &crate::ir::ConstraintSite,
        depth: usize,
    ) {
        let ty = site.record.clone();
        let target = &site.target;
        let combined = &site.constraints;
        self.report_runtime_dependent_randomize_field_attrs(&ty);
        let has_solver_policy_fields = self.txn_fields.get(&ty).is_some_and(|fields| {
            fields
                .iter()
                .any(|f| field_attr_unique(f) || !auto_coverage_values(f).is_empty())
        });
        if combined.is_empty() && !has_solver_policy_fields {
            if !self.emit_runtime_unconstrained_randomize_call(&ty, target, depth) {
                self.pad(depth);
                write!(self.out, "randomize_{ty}(&").ok();
                self.emit_expr(target);
                writeln!(self.out, ");").ok();
            }
            self.emit_randomize_trace_event(&ty, target, depth);
        } else {
            self.emit_constraint_solver_block(&ty, target, combined, site.blocking, depth);
        }
    }

    /// Emit `assert <expr> [else fail("msg")]` as an immediate, point-in-
    /// time check (existing behaviour). Compare with `emit_property_check`
    /// which schedules the check on every primary-clock edge.
    fn emit_inline_assert(&mut self, v: &Verify, depth: usize) {
        self.pad(depth);
        let expr = v.expr.as_ref().expect("assert without expr");
        write!(self.out, "if (!(").ok();
        self.emit_expr(expr);
        writeln!(self.out, ")) {{").ok();
        self.pad(depth + 1);
        let msg = v
            .else_fail
            .as_ref()
            .and_then(|e| match &*e.kind {
                ExprKind::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "assertion failed".to_string());
        let (fmt, caps) = process_interp(&msg);
        write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
        for c in &caps {
            self.emit_interp_arg(c);
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(self.out, "ctx.errors++;").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    fn runtime_randomize_problem_id(&self, target: &Expr) -> Option<u32> {
        self.runtime_randomize_problem_ids
            .get(&(target.span.start, target.span.end))
            .copied()
    }

    fn emit_runtime_unconstrained_randomize_call(
        &mut self,
        ty: &str,
        target: &Expr,
        depth: usize,
    ) -> bool {
        let Some(problem_id) = self.runtime_randomize_problem_id(target) else {
            return false;
        };
        self.pad(depth);
        writeln!(
            self.out,
            "{{   // runtime queued randomize scaffold for unconstrained record"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "auto _harc_rt_call = _harc_runtime_random_problem_table_prepare_call({problem_id}, harc_rng.state, 0);"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "auto _harc_rt_seed = _harc_rt_call.seed;").ok();
        self.pad(depth + 1);
        write!(
            self.out,
            "auto _harc_rt_status = harc_rt::random::harc_solve_queued("
        )
        .ok();
        self.emit_expr(target);
        writeln!(
            self.out,
            ", _harc_rt_call.problem_id, _harc_rt_seed, randomize_{ty});"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "(void)harc_rt::random::harc_handle_solve_status(_harc_rt_status);"
        )
        .ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
        true
    }

    /// Emit a `wait until …` statement (spec §7.9). Four shape axes:
    ///
    /// - **Single / AllOf / AnyOf** — the predicate is one expression
    ///   or the conjunction / disjunction of multiple sub-expressions.
    /// - **With or without `timeout`** — without, the loop is an
    ///   unbounded `co_await wait_until(_slot, [&]{ … })` (or sync
    ///   `while (!cond) tick();`). With timeout, a bounded polling
    ///   loop measured against `cycle_count` plus per-predicate
    ///   diagnostic logging on expiration.
    ///
    /// Diagnostic on `all of` timeout: lists each sub-predicate still
    /// false, identified by its pretty-printed source text (so the
    /// log mentions `env.agent.idle(100)` rather than a synthetic
    /// index). For `any of`, the breakdown lists every sub-predicate
    /// since none became true.
    fn emit_wait_until(
        &mut self,
        mode: &WaitUntilMode,
        conditions: &[Expr],
        timeout: Option<&WaitTimeout>,
        depth: usize,
    ) {
        if conditions.is_empty() {
            self.errors
                .push("wait until: at least one condition required".into());
            return;
        }
        let expanded_conditions = self.expand_wait_conditions(conditions);
        // Combine conditions into one predicate expression in C++.
        // `Single` uses the bare expression; `AllOf` / `AnyOf` are
        // emitted as parenthesized `&&` / `||` chains.
        let joiner = match mode {
            WaitUntilMode::Single | WaitUntilMode::AllOf => "&&",
            WaitUntilMode::AnyOf => "||",
        };
        let emit_overall_cond = |this: &mut Self| {
            if expanded_conditions.len() == 1 {
                expanded_conditions[0].emit(this);
                return;
            }
            for (i, c) in expanded_conditions.iter().enumerate() {
                if i > 0 {
                    write!(this.out, " {joiner} ").ok();
                }
                write!(this.out, "(").ok();
                c.emit(this);
                write!(this.out, ")").ok();
            }
        };

        match timeout {
            None => {
                // Untimed wait. Coroutine context: yield to the
                // scheduler; sync context: a synchronous polling loop.
                self.pad(depth);
                if self.in_coroutine {
                    write!(
                        self.out,
                        "co_await harc_rt::wait_until(_slot, [&]{{ return "
                    )
                    .ok();
                    emit_overall_cond(self);
                    writeln!(self.out, "; }});").ok();
                } else {
                    write!(self.out, "while (!(").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, ")) tick();").ok();
                }
            }
            Some(to) => {
                // Timed wait — open a brace block so the budget /
                // start variables don't leak.
                //
                // Coroutine context uses the runtime's
                // `wait_until_timeout` awaiter (one scheduler
                // round-trip): the scheduler evaluates the predicate
                // each cycle AND decrements a per-slot countdown,
                // resuming the coroutine when EITHER pred fires OR
                // the budget hits 0. The awaiter returns `true` for
                // pred-fired, `false` for timed-out — drives the
                // diagnostic emission below.
                //
                // Synchronous context (hookable body, etc.) has no
                // coroutine to suspend, so it stays on the explicit
                // polling loop (the only mechanism available there).
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                write!(self.out, "int64_t _wu_budget = (int64_t)(").ok();
                self.emit_expr(&to.cycles);
                writeln!(self.out, ");").ok();
                if self.in_coroutine {
                    // Single co_await — runtime handles the wait + countdown.
                    self.pad(depth + 1);
                    write!(self.out, "bool _wu_satisfied = co_await harc_rt::wait_until_timeout(_slot, [&]{{ return ").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, "; }}, (uint32_t)_wu_budget);").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "if (!_wu_satisfied) {{").ok();
                } else {
                    // Sync context: keep the explicit polling loop —
                    // no scheduler to defer the wait to.
                    self.pad(depth + 1);
                    writeln!(self.out, "int64_t _wu_start = (int64_t)cycle_count;").ok();
                    self.pad(depth + 1);
                    write!(self.out, "while (!(").ok();
                    emit_overall_cond(self);
                    writeln!(
                        self.out,
                        ") && ((int64_t)cycle_count - _wu_start) < _wu_budget) {{"
                    )
                    .ok();
                    self.pad(depth + 2);
                    writeln!(self.out, "tick();").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "}}").ok();
                    // Final check: re-evaluate the predicate now that
                    // the loop has ended (either pred fired or budget
                    // expired).
                    self.pad(depth + 1);
                    write!(self.out, "if (!(").ok();
                    emit_overall_cond(self);
                    writeln!(self.out, ")) {{").ok();
                }
                // Header line — user-supplied message or default.
                let header = to.message.as_ref().and_then(|e| match &*e.kind {
                    ExprKind::String(s) => Some(s.clone()),
                    _ => None,
                });
                self.pad(depth + 2);
                if let Some(raw) = header {
                    let (fmt, caps) = process_interp(&raw);
                    write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
                    for c in &caps {
                        self.emit_interp_arg(c);
                    }
                    writeln!(self.out, ");").ok();
                } else {
                    let label = match mode {
                        WaitUntilMode::Single => "wait until",
                        WaitUntilMode::AllOf => "wait until all of",
                        WaitUntilMode::AnyOf => "wait until any of",
                    };
                    writeln!(self.out,
                        "sim_log_line(\"FAIL\", \"{label} timed out after %lld cycles\", (long long)_wu_budget);"
                    ).ok();
                }
                // Per-sub-predicate breakdown.
                match mode {
                    WaitUntilMode::Single | WaitUntilMode::AllOf => {
                        for c in &expanded_conditions {
                            let escaped = escape_c(&c.label());
                            self.pad(depth + 2);
                            write!(self.out, "if (!(").ok();
                            c.emit(self);
                            writeln!(
                                self.out,
                                ")) sim_log_line(\"FAIL\", \"  not yet true: {escaped}\");"
                            )
                            .ok();
                        }
                    }
                    WaitUntilMode::AnyOf => {
                        // None became true — list everything that was
                        // being waited on so the user can spot the
                        // expected-firing condition that never fired.
                        let mut joined = String::new();
                        for (i, c) in expanded_conditions.iter().enumerate() {
                            if i > 0 {
                                joined.push_str(", ");
                            }
                            joined.push_str(&c.label());
                        }
                        let escaped = escape_c(&joined);
                        self.pad(depth + 2);
                        writeln!(
                            self.out,
                            "sim_log_line(\"FAIL\", \"  none of: {escaped}\");"
                        )
                        .ok();
                    }
                }
                self.pad(depth + 2);
                writeln!(self.out, "ctx.errors++;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
    }

    /// `assume <expr>` (immediate form) — same as inline assert but logged
    /// as ASSUME so the user can grep separately. No `errors++` because in
    /// sim, assumes are warnings (spec §5.2).
    fn emit_inline_assume(&mut self, v: &Verify, depth: usize) {
        self.pad(depth);
        let expr = v.expr.as_ref().expect("assume without expr");
        write!(self.out, "if (!(").ok();
        self.emit_expr(expr);
        writeln!(
            self.out,
            ")) sim_log_line(\"ASSUME\", \"assumption failed\");"
        )
        .ok();
    }

    /// Register a concurrent (every-primary-clock-edge) property check.
    /// Resolves a bare `Ident` to a declared property body; otherwise uses
    /// the inline expression directly. Translates the top-level temporal
    /// shape (`a |-> b`, `a |=> b`, or a plain bool) to a stateful closure
    /// pushed into `_checkers`.
    fn emit_property_check(&mut self, severity: &str, v: &Verify, depth: usize) {
        let raw = v.expr.as_ref().expect("assert property without body");
        let body: Expr = match &*raw.kind {
            ExprKind::Ident(id) => match self.properties.get(&id.name).cloned() {
                Some(b) => b,
                None => {
                    self.errors.push(format!(
                        "assert property `{}`: no property declaration with that name",
                        id.name
                    ));
                    return;
                }
            },
            _ => raw.clone(),
        };

        // Generate a unique tag from source span for the static state.
        let tag = format!("_p_{}_{}", raw.span.start, raw.span.end);
        let label = property_label(v, raw);

        // Pre-walk the body for `$past`/`$rose`/`$fell`/`$stable` occurrences.
        // Each gets a static delay slot + a per-call current-value local;
        // emit_expr substitutes references into the closure's predicate.
        let temporals = collect_temporal_occurrences(&body);

        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        // Static slots for delay state — survive across closure calls.
        for (i, _t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            writeln!(self.out, "static int64_t {tag}_ps{i} = 0;").ok();
        }
        // Current-value locals + populate prop_subs.
        for (i, t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            write!(self.out, "int64_t {tag}_cur{i} = (int64_t)(").ok();
            self.emit_expr(&t.inner);
            writeln!(self.out, ");").ok();
            let sub = match t.kind {
                SystemFn::Past => format!("{tag}_ps{i}"),
                SystemFn::Rose => format!("(!{tag}_ps{i} && {tag}_cur{i})"),
                SystemFn::Fell => format!("({tag}_ps{i} && !{tag}_cur{i})"),
                SystemFn::Stable => format!("({tag}_ps{i} == {tag}_cur{i})"),
                SystemFn::Clog2 => continue, // not temporal — skip
            };
            self.prop_subs
                .insert((t.call_span.start, t.call_span.end), sub);
        }
        match &*body.kind {
            // a |=> b — one-cycle-delayed implication. State: prev_a.
            ExprKind::Binary {
                op: BinaryOp::PipeImpliesNext,
                lhs,
                rhs,
            } => {
                self.pad(depth + 1);
                writeln!(self.out, "static bool {tag}_prev = false;").ok();
                self.pad(depth + 1);
                write!(self.out, "bool _curr_a = (bool)(").ok();
                self.emit_expr(lhs);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                write!(self.out, "bool _curr_b = (bool)(").ok();
                self.emit_expr(rhs);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                writeln!(self.out, "if ({tag}_prev && !_curr_b) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed (|=>)\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    writeln!(self.out, "ctx.errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{tag}_prev = _curr_a;").ok();
            }
            // a |-> b — same-cycle implication.
            ExprKind::Binary {
                op: BinaryOp::PipeImplies,
                lhs,
                rhs,
            } => {
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                self.emit_expr(lhs);
                write!(self.out, ") && !(bool)(").ok();
                self.emit_expr(rhs);
                writeln!(self.out, ")) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed (|->)\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    writeln!(self.out, "ctx.errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            // Plain bool — same-cycle invariant.
            _ => {
                self.pad(depth + 1);
                write!(self.out, "if (!(").ok();
                self.emit_expr(&body);
                writeln!(self.out, ")) {{").ok();
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "sim_log_line(\"{severity}\", \"property `{}` failed\");",
                    escape_c(&label)
                )
                .ok();
                if severity == "FAIL" {
                    self.pad(depth + 2);
                    writeln!(self.out, "ctx.errors++;").ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
        }
        // Update delay slots from current values for next-cycle reads.
        for (i, _t) in temporals.iter().enumerate() {
            self.pad(depth + 1);
            writeln!(self.out, "{tag}_ps{i} = {tag}_cur{i};").ok();
        }
        // Clear substitutions so they don't leak into other emissions.
        self.prop_subs.clear();

        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    /// Emit a cycle-trigger `on <bool-expr>` handler — registers a closure
    /// that fires per the handler's `edge` mode (rising / falling / level).
    /// Used by both the test-scope `on` form and monitor body handlers; the
    /// `prefix` distinguishes the static-state tags so concurrent handlers
    /// at the same span don't collide.
    ///
    /// Also handles the periodic form `on <N> cycles ... end on` (spec
    /// §7.10) — fires the body once every `<N>` primary-clock cycles.
    /// `N` is re-read each cycle so per-test overrides via component-
    /// field initialization (or a free `int wdog_period = 5000`)
    /// flow through naturally without rebuilding.
    fn emit_cycle_trigger(&mut self, h: &OnHandler, depth: usize, prefix: &str) {
        let tag = format!("{prefix}{}_{}", h.event.span.start, h.event.span.end);
        let target_vec = match h.phase {
            OnPhase::Checker => "_checkers",
            OnPhase::PostEval => "_post_eval_services",
        };
        self.pad(depth);
        writeln!(self.out, "{target_vec}.push_back([&]() {{").ok();
        if h.periodic {
            // Periodic: fire body when cycle_count - last_fired >= N.
            // Initial state: last_fired = 0 means first firing is at
            // cycle N (NOT cycle 0 — the user didn't ask for an
            // immediate-fire-on-start; they asked for "every N cycles").
            self.pad(depth + 1);
            writeln!(self.out, "static int64_t {tag}_last = 0;").ok();
            self.pad(depth + 1);
            write!(self.out, "int64_t {tag}_period = (int64_t)(").ok();
            self.emit_expr(&h.event);
            writeln!(self.out, ");").ok();
            // Guard against period <= 0 — a misconfigured period would
            // otherwise spin-fire every cycle (negative) or every cycle
            // forever (zero). Treat as no-op.
            self.pad(depth + 1);
            writeln!(
                self.out,
                "if ({tag}_period > 0 && (int64_t)cycle_count - {tag}_last >= {tag}_period) {{"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "{tag}_last = (int64_t)cycle_count;").ok();
            self.emit_block(&h.body, depth + 2);
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            writeln!(self.out, "}});").ok();
            return;
        }
        match h.edge {
            EdgeMode::Level => {
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                self.emit_expr(&h.event);
                writeln!(self.out, ")) {{").ok();
                self.emit_block(&h.body, depth + 2);
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            EdgeMode::Rising | EdgeMode::Falling => {
                self.pad(depth + 1);
                writeln!(self.out, "static bool {tag}_prev = false;").ok();
                self.pad(depth + 1);
                write!(self.out, "bool {tag}_curr = (bool)(").ok();
                self.emit_expr(&h.event);
                writeln!(self.out, ");").ok();
                self.pad(depth + 1);
                let cond = match h.edge {
                    EdgeMode::Rising => format!("!{tag}_prev && {tag}_curr"),
                    EdgeMode::Falling => format!("{tag}_prev && !{tag}_curr"),
                    EdgeMode::Level => unreachable!(),
                };
                writeln!(self.out, "if ({cond}) {{").ok();
                self.emit_block(&h.body, depth + 2);
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{tag}_prev = {tag}_curr;").ok();
            }
        }
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    /// Generic on-handler registration for any component (agent /
    /// transactor / sequencer / scoreboard). Dispatches each handler
    /// by the shape of its trigger expression:
    ///
    /// - `on event_field(arg) ... end on` (a `Call` expression) →
    ///   register a `[&]`-capturing closure into the corresponding
    ///   event vector. Used for drivers' `on req(t)`, sequencers'
    ///   subscriptions, etc.
    /// - `on <bool-expr> ... end on` (any other shape) → register a
    ///   per-cycle checker closure with the requested edge mode
    ///   (rising/falling/level). Used for monitors' `on dut.x && y`.
    ///
    /// `tag_prefix` distinguishes the static cycle-trigger state
    /// across invocations so concurrent components at the same source
    /// span don't collide. Inside both paths, body bare-references to
    /// component fields rewrite to `<instance>.<field>` via
    /// `field_subs`.
    /// Emit `on`-handler subscriber closures for a component instance.
    ///
    /// `bound_bus = Some((BusDecl, root))` is set when the component is
    /// declared `bound to BusType` and the user instantiated it via
    /// `let drv : Drv = bind <bus_binding>`. While each handler body is
    /// emitted, `bus_bindings["bus"]` is temporarily set to this binding
    /// so `bus.<ch>.send(t.addr, …)` and `bus.<ch>.<sig>` inside the
    /// body resolve to flat DUT signals through the existing bus-typing
    /// lowerers. The temporary entry is popped after each body so it
    /// doesn't leak across components.
    fn emit_component_handler_registrations(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
    ) {
        self.emit_component_handler_registrations_bound(comp, instance, depth, tag_prefix, None);
    }

    fn field_type_in_component_type(&self, ty: &str, field: &str) -> Option<String> {
        let lookup = |items: &[ComponentItem]| -> Option<String> {
            items.iter().find_map(|it| {
                if let ComponentItem::Field(f) = it {
                    if f.name.name == field {
                        return type_simple_name(Some(&f.ty)).map(String::from);
                    }
                }
                None
            })
        };
        if let Some(comp) = self.components.get(ty) {
            lookup(&comp.items)
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            lookup(&synth.items)
        } else {
            None
        }
    }

    fn hookable_method_info(&self, ty: &str, method: &str) -> Option<(usize, bool)> {
        let lookup = |items: &[ComponentItem]| -> Option<(usize, bool)> {
            items.iter().find_map(|it| {
                if let ComponentItem::Hookable(h) = it {
                    if h.is_hookable && h.name.name == method {
                        return Some((h.params.len(), h.return_ty.is_some()));
                    }
                }
                None
            })
        };
        if let Some(comp) = self.components.get(ty) {
            lookup(&comp.items)
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            lookup(&synth.items)
        } else {
            None
        }
    }

    fn resolve_connect_hookable_sink(
        &mut self,
        owner: &ComponentDecl,
        instance: &str,
        to_path: &str,
    ) -> Option<ConnectHookableSink> {
        let parts: Vec<&str> = to_path.split('.').collect();
        let method = parts.last()?;
        let mut cur_ty = owner.name.name.clone();
        let mut target_instance = instance.to_string();
        for seg in &parts[..parts.len().saturating_sub(1)] {
            let Some(next_ty) = self.field_type_in_component_type(&cur_ty, seg) else {
                return None;
            };
            target_instance.push('.');
            target_instance.push_str(seg);
            cur_ty = next_ty;
        }
        let Some((param_count, has_return)) = self.hookable_method_info(&cur_ty, method) else {
            return None;
        };
        let mut sink_path: Vec<String> = instance.split('.').map(String::from).collect();
        sink_path.extend(
            parts[..parts.len().saturating_sub(1)]
                .iter()
                .map(|seg| (*seg).to_string()),
        );
        if self.method_lives_in_when_active(&cur_ty, method) {
            if let Some(TransactorMode::Passive) = self.resolve_path_mode(&sink_path) {
                self.errors.push(format!(
                    "connect: hookable sink `{}.{}` lives inside `when active` of transactor `{}`, \
                     so it does not exist on a passive instance. Change the let-binding or field to `{} active`, \
                     or remove the connect edge. See spec §8.1.",
                    target_instance, method, cur_ty, cur_ty,
                ));
                return Some(ConnectHookableSink {
                    comp_ty: cur_ty,
                    instance: target_instance,
                    method: (*method).to_string(),
                });
            }
        }
        if param_count != 1 {
            self.errors.push(format!(
                "connect: hookable sink `{to_path}` must take exactly one payload argument, got {param_count}"
            ));
            return Some(ConnectHookableSink {
                comp_ty: cur_ty,
                instance: target_instance,
                method: (*method).to_string(),
            });
        }
        if has_return {
            self.errors.push(format!(
                "connect: hookable sink `{to_path}` must return void"
            ));
            return Some(ConnectHookableSink {
                comp_ty: cur_ty,
                instance: target_instance,
                method: (*method).to_string(),
            });
        }
        Some(ConnectHookableSink {
            comp_ty: cur_ty,
            instance: target_instance,
            method: (*method).to_string(),
        })
    }

    fn emit_connect_edge(
        &mut self,
        owner: &ComponentDecl,
        instance: &str,
        edge: &ConnectEdge,
        depth: usize,
    ) {
        let from = expr_path_str(&edge.from);
        let to = expr_path_str(&edge.to);
        let (Some(from), Some(to)) = (from, to) else {
            self.errors
                .push("connect: edge endpoints must be plain field paths in v0 cpp_tb".into());
            return;
        };
        self.pad(depth);
        if let Some(sink) = self.resolve_connect_hookable_sink(owner, instance, &to) {
            writeln!(
                self.out,
                "{}.{}.push_back([&](auto _t) {{ {}_{}({}, _t); }});",
                instance, from, sink.comp_ty, sink.method, sink.instance,
            )
            .ok();
        } else {
            writeln!(
                self.out,
                "{}.{}.push_back([&](auto _t) {{ for (auto& _s : {}.{}) _s(_t); }});",
                instance, from, instance, to,
            )
            .ok();
        }
    }

    /// Recursively walk a component's `Field`s and register handlers
    /// for each sub-component or sub-transactor. Mode flows down the
    /// inheritance chain via `inherited_mode`:
    ///
    /// ```text
    ///   let topenv : OuterEnv passive
    ///                ^^^^^^^^^^^^^^^^^
    ///                root inherited_mode = Some(Passive)
    ///
    ///   env OuterEnv { ag : MyAgent }      → ag inherits Passive
    ///   agent MyAgent { drv : T }          → drv inherits Passive
    /// ```
    ///
    /// At each sub-field, the **field-level explicit mode** (e.g. `drv
    /// : T active`) wins; only when the field has no mode does the
    /// inherited mode apply. If a transactor sub-field ends up with no
    /// mode (neither field-level nor inherited), it's a clear error.
    ///
    /// Sub-component connect edges are emitted here too — paths like
    /// `sequencer.dispatched -> drv.req` inside an agent get prefixed
    /// with the agent's instance path so they wire correctly when the
    /// agent is composed into a parent env.
    fn emit_subcomponent_handler_registrations(
        &mut self,
        comp: &ComponentDecl,
        instance_path: &str,
        inherited_mode: Option<TransactorMode>,
        depth: usize,
    ) {
        for ci in &comp.items {
            let f = match ci {
                ComponentItem::Field(f) => f,
                _ => continue,
            };
            let field_ty = match type_simple_name(Some(&f.ty)) {
                Some(t) => t,
                None => continue,
            };
            // Resolve effective mode: field-explicit wins over inherited.
            let field_mode = match &f.ty {
                TypeExpr::Named { mode: Some(m), .. } => Some(*m),
                _ => None,
            };
            let effective_mode = field_mode.or(inherited_mode);

            // Sub-component (driver/agent/env/sequencer/scoreboard).
            if let Some(sub_comp) = self.components.get(field_ty).cloned() {
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                let sub_tag = format!("_{}_{}_", sub_comp.kind.keyword(), f.name.name);
                self.emit_component_handler_registrations(&sub_comp, &sub_inst, depth, &sub_tag);
                // Sub-component watchdog (spec §8.6) — install the
                // periodic checker prefixed with the sub-instance path
                // so each composed agent inside an env gets its own
                // independent watchdog firing.
                for sub_ci in &sub_comp.items {
                    if let ComponentItem::Watchdog(w) = sub_ci {
                        self.emit_watchdog_checker(&sub_comp, w, &sub_inst, depth);
                    }
                }
                // Recurse into the sub-component's own sub-fields.
                self.emit_subcomponent_handler_registrations(
                    &sub_comp,
                    &sub_inst,
                    effective_mode,
                    depth,
                );
                // Emit the sub-component's connect edges, prefixed
                // with its instance path. Without this, an agent's
                // `connect sequencer.dispatched -> drv.req` would
                // never wire when the agent is composed into a parent
                // env.
                for sub_ci in &sub_comp.items {
                    if let ComponentItem::Connect(cb) = sub_ci {
                        for edge in &cb.edges {
                            self.emit_connect_edge(&sub_comp, &sub_inst, edge, depth);
                        }
                    }
                }
                continue;
            }

            // Covergroup sub-field. Register the per-cycle sample
            // closure prefixed with the sub-instance path so the
            // testbench-bound form (`testbench Tb { cov : Cg }`)
            // gets its sample firing for free. Without this, a
            // covergroup declared as a testbench field never
            // samples (the test-scope `let cov : Cg` registration
            // is the only path otherwise — and the desugarer doesn't
            // mint a parallel let at test scope).
            if let Some(g) = self.covergroups.get(field_ty).cloned() {
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                self.emit_covergroup_sample_registration(&g, &sub_inst, depth);
                continue;
            }

            // Transactor sub-field. Resolve mode and emit handlers
            // through the synth ComponentDecl path.
            if let Some(t) = self.transactors.get(field_ty).cloned() {
                if t.bound_to.is_some() {
                    self.errors.push(format!(
                        "transactor field `{}.{} : {}` has a `bound to` clause; bound sub-components inside an env/agent are out of v0 scope",
                        instance_path, f.name.name, field_ty,
                    ));
                    continue;
                }
                let mode = match effective_mode {
                    Some(m) => m,
                    None => {
                        self.errors.push(format!(
                            "transactor field `{}.{} : {}` has no mode and no parent specifies one; annotate the field or one of its parent let/field sites with `active`/`passive`",
                            instance_path, f.name.name, field_ty,
                        ));
                        continue;
                    }
                };
                let include_active = matches!(mode, TransactorMode::Active);
                let synth = synth_component_from_transactor(&t, include_active);
                let sub_inst = format!("{}.{}", instance_path, f.name.name);
                let sub_tag = format!("_xactor_{}_", sub_inst.replace('.', "_"));
                self.emit_component_handler_registrations(&synth, &sub_inst, depth, &sub_tag);
            }
        }
    }

    /// Phase 2c: emit each `on bus.<ch>.handshake(arg)` handler in a
    /// `bound to BusType` monitor as an independent coroutine actor:
    ///
    /// ```text
    ///     while (true) {
    ///         co_await wait_until(_slot, [&]{ return valid && ready; });
    ///         auto <arg> = <first payload signal>;
    ///         <body — bus.<ch>.<sig> resolves through the bus binding>
    ///         co_await wait_cycles(_slot, 1);   // skip past this handshake
    ///     }
    /// ```
    ///
    /// Non-handshake on-handlers in the monitor (event subscribers,
    /// cycle triggers on bool expressions) fall through to the
    /// existing sync `_checkers`-based path. A monitor can mix both —
    /// each handler is independently classified.
    ///
    /// The trailing `wait_cycles(1)` matters: a handshake completes
    /// in exactly one cycle (valid && ready both high at the posedge),
    /// but the producer/consumer may keep the signals high across
    /// multiple cycles for back-to-back handshakes. Skipping forward
    /// one cycle ensures we don't re-fire on the same handshake.
    /// Back-to-back handshakes with no idle gap will fire on every
    /// cycle, which is exactly what arch §19 specifies.
    fn emit_bound_monitor_actors(
        &mut self,
        mon: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) {
        // Build field substitution map (same shape as
        // emit_component_handler_registrations) so bare names inside
        // the handler body resolve to `instance.field`.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        let mut local_field_types: Vec<(String, String)> = Vec::new();
        for it in &mon.items {
            if let ComponentItem::Field(f) = it {
                let field_ref = if self.is_dut_pointer_field_type(&f.ty)
                    && self.pointer_vars.contains(&f.name.name)
                {
                    f.name.name.clone()
                } else {
                    format!("{instance}.{}", f.name.name)
                };
                subs.insert(f.name.name.clone(), field_ref);
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    local_field_types.push((f.name.name.clone(), ty.to_string()));
                }
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        // Walk handlers, classify each as handshake-actor vs sync.
        let mut sync_handlers: Vec<&OnHandler> = Vec::new();
        for it in &mon.items {
            if let ComponentItem::OnHandler(h) = it {
                if let Some((ch_name, arg_name)) = extract_bus_handshake_event(&h.event, "bus") {
                    // Resolve the channel in the bound bus.
                    let channel = match binding
                        .0
                        .handshakes
                        .iter()
                        .find(|hs| hs.name.name == ch_name)
                    {
                        Some(c) => c.clone(),
                        None => {
                            self.errors.push(format!(
                                "monitor {instance}: bus has no channel `{ch_name}`"
                            ));
                            continue;
                        }
                    };
                    if channel.payload.is_empty() {
                        self.errors.push(format!(
                            "monitor {instance}: channel `{ch_name}` has no payload signals to capture"
                        ));
                        continue;
                    }
                    let (bus_decl, root, sig_prefix) = binding;
                    let slot_var = format!("_{instance}_{ch_name}_slot");
                    let sched_var = format!("_{instance}_{ch_name}_sched");

                    if self.mt {
                        self.pad(depth);
                        writeln!(self.out, "harc_rt::ThreadScheduler {sched_var};").ok();
                    }
                    self.pad(depth);
                    writeln!(self.out, "harc_rt::ThreadSlot {slot_var};").ok();
                    self.pad(depth);
                    if self.mt {
                        writeln!(self.out, "{sched_var}.slots.push_back(&{slot_var});").ok();
                        self.actor_threads
                            .push((sched_var.clone(), slot_var.clone()));
                    } else {
                        writeln!(self.out, "sched.slots.push_back(&{slot_var});").ok();
                    }
                    self.pad(depth);
                    writeln!(
                        self.out,
                        "auto {slot_var}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
                    ).ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "while (true) {{").ok();
                    let valid_port = self.bus_signal_name(&sig_prefix, &ch_name, "valid");
                    let ready_port = self.bus_signal_name(&sig_prefix, &ch_name, "ready");
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{valid_port} && {root}->{ready_port}; }});",
                    ).ok();
                    // Bind `arg` to the per-channel payload struct so the
                    // body can use `arg.data`, `arg.resp`, etc. The
                    // struct's implicit-conversion-to-first-field
                    // operator means scalar use (`sb.queue.push(arg)`)
                    // also keeps working — push receives the first
                    // payload value, matching pre-multi-payload behaviour.
                    self.pad(depth + 2);
                    let struct_name = format!("{}_{}_payload", bus_decl.name.name, ch_name);
                    write!(self.out, "{struct_name} {arg_name} = {{").ok();
                    let mut first = true;
                    for sig in &channel.payload {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        let sig_port = self.bus_signal_name(&sig_prefix, &ch_name, &sig.name.name);
                        write!(self.out, "{root}->{sig_port}").ok();
                    }
                    writeln!(self.out, "}};").ok();
                    // Activity tracking (spec §7.x): an observed bus
                    // handshake counts as an "in" for this monitor
                    // instance.
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "{instance}._last_in_cycle = (uint64_t)cycle_count;"
                    )
                    .ok();

                    // Body: install field subs + bus binding, mark
                    // coroutine context, emit, restore state.
                    let prev_subs = std::mem::replace(&mut self.field_subs, subs.clone());
                    let mut saved_field_types = Vec::new();
                    for (name, ty) in &local_field_types {
                        let prev = self.let_types.insert(name.clone(), ty.clone());
                        saved_field_types.push((name.clone(), prev));
                    }
                    let mut added_events = Vec::new();
                    for (name, ty) in &local_event_types {
                        if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                            added_events.push(name.clone());
                        }
                    }
                    let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
                    let prior_corout = self.in_coroutine;
                    self.in_coroutine = true;
                    let prior_inst = std::mem::replace(
                        &mut self.current_component_instance,
                        Some(instance.to_string()),
                    );

                    self.emit_block(&h.body, depth + 2);

                    self.current_component_instance = prior_inst;
                    self.in_coroutine = prior_corout;
                    match prior_bus {
                        Some(prev) => {
                            self.bus_bindings.insert("bus".into(), prev);
                        }
                        None => {
                            self.bus_bindings.remove("bus");
                        }
                    }
                    for n in added_events {
                        self.event_types.remove(&n);
                    }
                    for (name, prev) in saved_field_types {
                        match prev {
                            Some(ty) => {
                                self.let_types.insert(name, ty);
                            }
                            None => {
                                self.let_types.remove(&name);
                            }
                        }
                    }
                    self.field_subs = prev_subs;

                    // Skip past this handshake before re-arming.
                    self.pad(depth + 2);
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "}}").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "co_return;").ok();
                    self.pad(depth);
                    writeln!(self.out, "}};").ok();
                    self.pad(depth);
                    writeln!(
                        self.out,
                        "{slot_var}.thread = {slot_var}_lambda(&{slot_var});"
                    )
                    .ok();
                } else {
                    sync_handlers.push(h);
                }
            }
        }

        // Other on-handlers fall through to the existing sync
        // registration. We do this by emitting a temporary clone of
        // the monitor whose `items` contain only the leftover
        // handlers + the original fields, so existing field-sub /
        // event-subscription paths run unchanged.
        if !sync_handlers.is_empty() {
            let mut sync_view = mon.clone();
            sync_view.items.retain(|it| match it {
                ComponentItem::OnHandler(h) => {
                    extract_bus_handshake_event(&h.event, "bus").is_none()
                }
                _ => true,
            });
            self.emit_component_handler_registrations_bound(
                &sync_view,
                instance,
                depth,
                "_m_",
                Some(binding.clone()),
            );
        }
    }

    /// Emit a target-side TLM body with source-level `return` lowered into a
    /// response temp plus a returned flag. This keeps responder actors in the
    /// same coroutine after nested/early returns: remaining statements are
    /// skipped, loops break, then the common rsp_valid/rsp_ready epilogue runs.
    fn emit_target_tlm_body(
        &mut self,
        block: &Block,
        rsp_var: Option<&str>,
        returned_var: &str,
        depth: usize,
    ) {
        for stmt in &block.stmts {
            if matches!(stmt.kind, StmtKind::Let(_)) {
                self.emit_target_tlm_stmt(stmt, rsp_var, returned_var, depth);
                continue;
            }
            self.pad(depth);
            writeln!(self.out, "if (!{returned_var}) {{").ok();
            self.emit_target_tlm_stmt(stmt, rsp_var, returned_var, depth + 1);
            self.pad(depth);
            writeln!(self.out, "}}").ok();
        }
    }

    fn emit_target_tlm_stmt(
        &mut self,
        stmt: &Stmt,
        rsp_var: Option<&str>,
        returned_var: &str,
        depth: usize,
    ) {
        match &stmt.kind {
            StmtKind::Return(opt) => {
                self.pad(depth);
                match (rsp_var, opt) {
                    (Some(dst), Some(expr)) => {
                        write!(self.out, "harc_rt::harc_assign({dst}, ").ok();
                        self.emit_expr(expr);
                        writeln!(self.out, ");").ok();
                    }
                    (Some(_), None) => {
                        self.errors
                            .push("target TLM value-returning method uses bare `return`".into());
                    }
                    (None, Some(_)) => {
                        self.errors
                            .push("target TLM void method must not return a value".into());
                    }
                    (None, None) => {}
                }
                self.pad(depth);
                writeln!(self.out, "{returned_var} = true;").ok();
            }
            StmtKind::If(i) => {
                self.pad(depth);
                write!(self.out, "if (").ok();
                self.emit_expr(&i.cond);
                writeln!(self.out, ") {{").ok();
                self.emit_target_tlm_body(&i.then_block, rsp_var, returned_var, depth + 1);
                for (cond, block) in &i.elsifs {
                    self.pad(depth);
                    write!(self.out, "}} else if (").ok();
                    self.emit_expr(cond);
                    writeln!(self.out, ") {{").ok();
                    self.emit_target_tlm_body(block, rsp_var, returned_var, depth + 1);
                }
                if let Some(else_block) = &i.else_block {
                    self.pad(depth);
                    writeln!(self.out, "}} else {{").ok();
                    self.emit_target_tlm_body(else_block, rsp_var, returned_var, depth + 1);
                }
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::For(f) => {
                self.pad(depth);
                let var = &f.var.name;
                if let ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } = &*f.iter.kind
                {
                    write!(self.out, "for (int64_t {var} = ").ok();
                    self.emit_expr(lo);
                    // `for i in lo .. hi` is INCLUSIVE of `hi` (matches ARCH);
                    // emit `<=`, not `<`. Keep in lockstep with the tbir
                    // lowering in `ir::lower::control::lower_for`.
                    write!(self.out, "; {var} <= ").ok();
                    self.emit_expr(hi);
                    writeln!(self.out, "; {var}++) {{").ok();
                    self.emit_target_tlm_body(&f.body, rsp_var, returned_var, depth + 1);
                    self.pad(depth + 1);
                    writeln!(self.out, "if ({returned_var}) break;").ok();
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                } else {
                    write!(self.out, "for (auto& {var} : ").ok();
                    self.emit_expr(&f.iter);
                    writeln!(self.out, ") {{").ok();
                    self.emit_target_tlm_body(&f.body, rsp_var, returned_var, depth + 1);
                    self.pad(depth + 1);
                    writeln!(self.out, "if ({returned_var}) break;").ok();
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                }
            }
            StmtKind::Repeat(r) => {
                self.pad(depth);
                write!(self.out, "for (int64_t _r = 0; _r < ").ok();
                self.emit_expr(&r.count);
                writeln!(self.out, "; _r++) {{").ok();
                self.emit_target_tlm_body(&r.body, rsp_var, returned_var, depth + 1);
                self.pad(depth + 1);
                writeln!(self.out, "if ({returned_var}) break;").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::While { cond, body, .. } => {
                self.pad(depth);
                write!(self.out, "while (!{returned_var} && (").ok();
                self.emit_expr(cond);
                writeln!(self.out, ")) {{").ok();
                self.emit_target_tlm_body(body, rsp_var, returned_var, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Loop(body) => {
                self.pad(depth);
                writeln!(self.out, "while (!{returned_var}) {{").ok();
                self.emit_target_tlm_body(body, rsp_var, returned_var, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            _ => {
                self.emit_stmt(stmt, depth);
            }
        }
    }

    /// Emit `thread bus.method(args) ... return expr end thread` items in a
    /// bound transactor as target-side TLM responder actors. The actor owns
    /// the method's target handshake:
    ///
    /// 1. Assert `<method>_req_ready`.
    /// 2. Wait for a request handshake and capture args (plus tag for OOO).
    /// 3. Run the HARC body in coroutine context.
    /// 4. Drive optional response payload/tag and hold `rsp_valid` until
    ///    the DUT initiator asserts `rsp_ready`.
    fn emit_bound_tlm_target_actors(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) {
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        let mut local_field_types: Vec<(String, String)> = Vec::new();
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                let field_ref = if self.is_dut_pointer_field_type(&f.ty)
                    && self.pointer_vars.contains(&f.name.name)
                {
                    f.name.name.clone()
                } else {
                    format!("{instance}.{}", f.name.name)
                };
                subs.insert(f.name.name.clone(), field_ref);
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    local_field_types.push((f.name.name.clone(), ty.to_string()));
                }
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        for it in &comp.items {
            let ComponentItem::TargetTlmThread(t) = it else {
                continue;
            };
            let Some(bus_alias) = t.method.segments.first().map(|s| s.name.as_str()) else {
                continue;
            };
            if bus_alias != "bus" {
                self.errors.push(format!(
                    "target TLM thread in transactor `{}` must target `bus.<method>`, got `{}`",
                    comp.name.name,
                    t.method
                        .segments
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".")
                ));
                continue;
            }
            if t.method.segments.len() != 2 {
                self.errors.push(format!(
                    "target TLM thread in transactor `{}` must use `thread bus.<method>(...)`",
                    comp.name.name
                ));
                continue;
            }
            let method_name = &t.method.segments[1].name;
            let Some(method) = binding
                .0
                .tlm_methods
                .iter()
                .find(|m| &m.name.name == method_name)
                .cloned()
            else {
                self.errors.push(format!(
                    "target TLM thread `{}`: bus `{}` has no tlm_method `{}`",
                    instance, binding.0.name.name, method_name
                ));
                continue;
            };
            if t.params.len() != method.args.len() {
                self.errors.push(format!(
                    "target TLM thread bus.{}: expected {} arg(s), got {}",
                    method.name.name,
                    method.args.len(),
                    t.params.len(),
                ));
                continue;
            }

            if method.ret.is_some() && !t.body.stmts.iter().any(stmt_contains_return) {
                self.errors.push(format!(
                    "target TLM thread bus.{} must end with `return <expr>` because the method returns a value",
                    method.name.name
                ));
                continue;
            }

            let (_, root, sig_prefix) = binding;
            let inst_tag = instance.replace('.', "_");
            let method_tag = method.name.name.replace('.', "_");
            let slot_var = format!("_{inst_tag}_{method_tag}_target_slot");
            let sched_var = format!("_{inst_tag}_{method_tag}_target_sched");
            let req_valid = self.bus_signal_name(sig_prefix, &method.name.name, "req_valid");
            let req_ready = self.bus_signal_name(sig_prefix, &method.name.name, "req_ready");
            let rsp_valid = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_valid");
            let rsp_ready = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_ready");
            let rsp_data = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_data");
            let req_tag = self.bus_signal_name(sig_prefix, &method.name.name, "req_tag");
            let rsp_tag = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_tag");
            let tagged = method.mode.name == "out_of_order";
            if method.mode.name != "blocking" && !tagged {
                self.errors.push(format!(
                    "target TLM thread bus.{} supports `blocking` and `out_of_order tags N`, not `{}`",
                    method.name.name, method.mode.name,
                ));
                continue;
            }
            if tagged {
                self.emit_bound_tagged_tlm_target_actors(
                    t,
                    &method,
                    instance,
                    depth,
                    binding,
                    &subs,
                    &local_event_types,
                    &local_field_types,
                );
                continue;
            }

            if self.mt {
                self.pad(depth);
                writeln!(self.out, "harc_rt::ThreadScheduler {sched_var};").ok();
            }
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadSlot {slot_var};").ok();
            self.pad(depth);
            if self.mt {
                writeln!(self.out, "{sched_var}.slots.push_back(&{slot_var});").ok();
                self.actor_threads
                    .push((sched_var.clone(), slot_var.clone()));
            } else {
                writeln!(self.out, "sched.slots.push_back(&{slot_var});").ok();
            }
            self.pad(depth);
            writeln!(
                self.out,
                "auto {slot_var}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "{root}->{req_ready} = 0;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "while (true) {{").ok();
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{req_ready} = 1;").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{req_valid} && {root}->{req_ready}; }});",
            )
            .ok();
            for (param, (arg_name, arg_ty)) in t.params.iter().zip(method.args.iter()) {
                let local_name = &param.name.name;
                let local_ty = param.ty.as_ref().unwrap_or(arg_ty);
                let cty = self.c_type_for_param(local_ty);
                let arg_port = self.bus_signal_name(sig_prefix, &method.name.name, &arg_name.name);
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "{cty} {local_name} = ({cty})harc_rt::harc_read({root}->{arg_port});"
                )
                .ok();
            }
            if tagged {
                self.pad(depth + 2);
                writeln!(self.out, "auto _tlm_req_tag = {root}->{req_tag};").ok();
            }
            self.pad(depth + 2);
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{req_ready} = 0;").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "{instance}._last_in_cycle = (uint64_t)cycle_count;"
            )
            .ok();
            self.emit_tlm_call_trace_event(
                instance,
                "bus",
                &method.name.name,
                "request",
                "target",
                None,
                depth + 2,
            );

            let prev_subs = std::mem::replace(&mut self.field_subs, subs.clone());
            let mut saved_field_types = Vec::new();
            for (name, ty) in &local_field_types {
                let prev = self.let_types.insert(name.clone(), ty.clone());
                saved_field_types.push((name.clone(), prev));
            }
            let mut added_events = Vec::new();
            for (name, ty) in &local_event_types {
                if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                    added_events.push(name.clone());
                }
            }
            let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
            let prior_corout = self.in_coroutine;
            self.in_coroutine = true;
            let prior_inst = std::mem::replace(
                &mut self.current_component_instance,
                Some(instance.to_string()),
            );
            if let Some(ret_ty) = &method.ret {
                let cty = self.c_type_for_param(ret_ty);
                self.pad(depth + 2);
                writeln!(self.out, "{cty} _tlm_rsp_value{{}};").ok();
            }
            self.pad(depth + 2);
            writeln!(self.out, "bool _tlm_returned = false;").ok();
            self.emit_target_tlm_body(
                &t.body,
                method.ret.as_ref().map(|_| "_tlm_rsp_value"),
                "_tlm_returned",
                depth + 2,
            );
            if method.ret.is_some() {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "if (!_tlm_returned) {{ sim_log_line(\"FAIL\", \"target TLM thread bus.{} completed without return\"); ctx.errors++; }}",
                    method.name.name
                )
                .ok();
                let rsp_stmt = method
                    .ret
                    .as_ref()
                    .map(|ret| {
                        self.record_drive_stmt(
                            ret,
                            &format!("{root}->{rsp_data}"),
                            "_tlm_rsp_value",
                        )
                    })
                    .unwrap_or_else(|| {
                        format!("harc_rt::harc_assign({root}->{rsp_data}, _tlm_rsp_value);")
                    });
                self.pad(depth + 2);
                writeln!(self.out, "{rsp_stmt}").ok();
            }
            self.current_component_instance = prior_inst;
            self.in_coroutine = prior_corout;
            match prior_bus {
                Some(prev) => {
                    self.bus_bindings.insert("bus".into(), prev);
                }
                None => {
                    self.bus_bindings.remove("bus");
                }
            }
            for n in added_events {
                self.event_types.remove(&n);
            }
            for (name, prev) in saved_field_types {
                match prev {
                    Some(ty) => {
                        self.let_types.insert(name, ty);
                    }
                    None => {
                        self.let_types.remove(&name);
                    }
                }
            }
            self.field_subs = prev_subs;

            if tagged {
                self.pad(depth + 2);
                writeln!(self.out, "{root}->{rsp_tag} = _tlm_req_tag;").ok();
            }
            self.emit_tlm_call_trace_event(
                instance,
                "bus",
                &method.name.name,
                "response",
                "target",
                None,
                depth + 2,
            );
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{rsp_valid} = 1;").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "if (!{root}->{rsp_ready}) co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{rsp_ready}; }});",
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            self.pad(depth + 2);
            writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "{instance}._last_out_cycle = (uint64_t)cycle_count;"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth + 1);
            writeln!(self.out, "co_return;").ok();
            self.pad(depth);
            writeln!(self.out, "}};").ok();
            self.pad(depth);
            writeln!(
                self.out,
                "{slot_var}.thread = {slot_var}_lambda(&{slot_var});"
            )
            .ok();
        }
    }

    fn emit_bound_tagged_tlm_target_actors(
        &mut self,
        t: &TargetTlmThread,
        method: &TlmMethod,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
        subs: &std::collections::HashMap<String, String>,
        local_event_types: &[(String, String)],
        local_field_types: &[(String, String)],
    ) {
        let Some(tags_expr) = &method.out_of_order_tags else {
            self.errors.push(format!(
                "target TLM thread bus.{} is out_of_order but has no tag count",
                method.name.name
            ));
            return;
        };
        let Some(tag_count_u64) = fold_int_literal(tags_expr) else {
            self.errors.push(format!(
                "target TLM thread bus.{} requires a literal `out_of_order tags N` count for responder-lane lowering",
                method.name.name
            ));
            return;
        };
        if tag_count_u64 == 0 || tag_count_u64 > 64 {
            self.errors.push(format!(
                "target TLM thread bus.{} supports 1..64 out_of_order target tags, got {}",
                method.name.name, tag_count_u64
            ));
            return;
        }
        let tag_count = tag_count_u64 as usize;

        let (_, root, sig_prefix) = binding;
        let inst_tag = instance.replace('.', "_");
        let method_tag = method.name.name.replace('.', "_");
        let prefix = format!("_{inst_tag}_{method_tag}_target_ooo");
        let req_valid = self.bus_signal_name(sig_prefix, &method.name.name, "req_valid");
        let req_ready = self.bus_signal_name(sig_prefix, &method.name.name, "req_ready");
        let rsp_valid = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_valid");
        let rsp_ready = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_ready");
        let rsp_data = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_data");
        let req_tag = self.bus_signal_name(sig_prefix, &method.name.name, "req_tag");
        let rsp_tag = self.bus_signal_name(sig_prefix, &method.name.name, "rsp_tag");

        let dispatcher_slot = format!("{prefix}_dispatcher_slot");
        let arbiter_slot = format!("{prefix}_arbiter_slot");
        let dispatcher_sched = format!("{prefix}_dispatcher_sched");
        let arbiter_sched = format!("{prefix}_arbiter_sched");
        let lane_busy = format!("{prefix}_lane_busy");
        let lane_req_valid = format!("{prefix}_lane_req_valid");
        let lane_rsp_valid = format!("{prefix}_lane_rsp_valid");
        let lane_rsp_data = format!("{prefix}_lane_rsp_data");

        self.pad(depth);
        writeln!(
            self.out,
            "std::array<std::atomic<bool>, {tag_count}> {lane_busy}{{}};"
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::array<std::atomic<bool>, {tag_count}> {lane_req_valid}{{}};"
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::array<std::atomic<bool>, {tag_count}> {lane_rsp_valid}{{}};"
        )
        .ok();
        for (arg_name, arg_ty) in &method.args {
            let cty = self.c_type_for_param(arg_ty);
            let arr = format!("{prefix}_arg_{}", arg_name.name);
            self.pad(depth);
            writeln!(self.out, "std::array<{cty}, {tag_count}> {arr}{{}};").ok();
        }
        if let Some(ret_ty) = &method.ret {
            let cty = self.c_type_for_param(ret_ty);
            self.pad(depth);
            writeln!(
                self.out,
                "std::array<{cty}, {tag_count}> {lane_rsp_data}{{}};"
            )
            .ok();
        }

        self.pad(depth);
        writeln!(self.out, "_post_eval_services.push_back([&]() {{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "bool _tlm_ready = false;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "if ({root}->{req_valid}) {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "auto _tag = (size_t){root}->{req_tag};").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "_tlm_ready = _tag < {tag_count} && !{lane_busy}[_tag].load() && !{lane_req_valid}[_tag].load();"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "}} else {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "for (size_t i = 0; i < {tag_count}; ++i) if (!{lane_busy}[i].load() && !{lane_req_valid}[i].load()) {{ _tlm_ready = true; break; }}"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "{root}->{req_ready} = _tlm_ready;").ok();
        self.pad(depth);
        writeln!(self.out, "}});").ok();

        if self.mt {
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadScheduler {dispatcher_sched};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "harc_rt::ThreadSlot {dispatcher_slot};").ok();
        self.pad(depth);
        if self.mt {
            writeln!(
                self.out,
                "{dispatcher_sched}.slots.push_back(&{dispatcher_slot});"
            )
            .ok();
            self.actor_threads
                .push((dispatcher_sched.clone(), dispatcher_slot.clone()));
        } else {
            writeln!(self.out, "sched.slots.push_back(&{dispatcher_slot});").ok();
        }
        self.pad(depth);
        writeln!(
            self.out,
            "auto {dispatcher_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "{root}->{req_ready} = 0;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{req_valid} && {root}->{req_ready}; }});"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 3);
        writeln!(self.out, "auto _tag = (size_t){root}->{req_tag};").ok();
        for (arg_name, _) in &method.args {
            let arg_port = self.bus_signal_name(sig_prefix, &method.name.name, &arg_name.name);
            let arr = format!("{prefix}_arg_{}", arg_name.name);
            self.pad(depth + 3);
            writeln!(
                self.out,
                "{arr}[_tag] = harc_rt::harc_read({root}->{arg_port});"
            )
            .ok();
        }
        self.pad(depth + 3);
        writeln!(self.out, "{lane_busy}[_tag].store(true);").ok();
        self.pad(depth + 3);
        writeln!(self.out, "{lane_req_valid}[_tag].store(true);").ok();
        self.pad(depth + 3);
        writeln!(
            self.out,
            "{instance}._last_in_cycle = (uint64_t)cycle_count;"
        )
        .ok();
        self.emit_tlm_call_trace_event(
            instance,
            "bus",
            &method.name.name,
            "request",
            "target",
            Some("_tag"),
            depth + 3,
        );
        self.pad(depth + 2);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 2);
        writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "co_return;").ok();
        self.pad(depth);
        writeln!(self.out, "}};").ok();
        self.pad(depth);
        writeln!(
            self.out,
            "{dispatcher_slot}.thread = {dispatcher_slot}_lambda(&{dispatcher_slot});"
        )
        .ok();

        for lane in 0..tag_count {
            let lane_slot = format!("{prefix}_lane{lane}_slot");
            let lane_sched = format!("{prefix}_lane{lane}_sched");
            if self.mt {
                self.pad(depth);
                writeln!(self.out, "harc_rt::ThreadScheduler {lane_sched};").ok();
            }
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadSlot {lane_slot};").ok();
            self.pad(depth);
            if self.mt {
                writeln!(self.out, "{lane_sched}.slots.push_back(&{lane_slot});").ok();
                self.actor_threads
                    .push((lane_sched.clone(), lane_slot.clone()));
            } else {
                writeln!(self.out, "sched.slots.push_back(&{lane_slot});").ok();
            }
            self.pad(depth);
            writeln!(
                self.out,
                "auto {lane_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "while (true) {{").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "co_await harc_rt::wait_until(_slot, [&]{{ return {lane_req_valid}[{lane}].load(); }});"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "{lane_req_valid}[{lane}].store(false);").ok();
            for (param, (arg_name, arg_ty)) in t.params.iter().zip(method.args.iter()) {
                let local_name = &param.name.name;
                let local_ty = param.ty.as_ref().unwrap_or(arg_ty);
                let cty = self.c_type_for_param(local_ty);
                let arr = format!("{prefix}_arg_{}", arg_name.name);
                self.pad(depth + 2);
                writeln!(self.out, "{cty} {local_name} = {arr}[{lane}];").ok();
            }

            let prev_subs = std::mem::replace(&mut self.field_subs, subs.clone());
            let mut saved_field_types = Vec::new();
            for (name, ty) in local_field_types {
                let prev = self.let_types.insert(name.clone(), ty.clone());
                saved_field_types.push((name.clone(), prev));
            }
            let mut added_events = Vec::new();
            for (name, ty) in local_event_types {
                if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                    added_events.push(name.clone());
                }
            }
            let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
            let prior_corout = self.in_coroutine;
            self.in_coroutine = true;
            let prior_inst = std::mem::replace(
                &mut self.current_component_instance,
                Some(instance.to_string()),
            );
            if let Some(ret_ty) = &method.ret {
                let cty = self.c_type_for_param(ret_ty);
                self.pad(depth + 2);
                writeln!(self.out, "{cty} _tlm_rsp_value{{}};").ok();
            }
            self.pad(depth + 2);
            writeln!(self.out, "bool _tlm_returned = false;").ok();
            self.emit_target_tlm_body(
                &t.body,
                method.ret.as_ref().map(|_| "_tlm_rsp_value"),
                "_tlm_returned",
                depth + 2,
            );
            if method.ret.is_some() {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "if (!_tlm_returned) {{ sim_log_line(\"FAIL\", \"target TLM thread bus.{} completed without return\"); ctx.errors++; }}",
                    method.name.name
                )
                .ok();
                self.pad(depth + 2);
                writeln!(self.out, "{lane_rsp_data}[{lane}] = _tlm_rsp_value;").ok();
            }
            self.current_component_instance = prior_inst;
            self.in_coroutine = prior_corout;
            match prior_bus {
                Some(prev) => {
                    self.bus_bindings.insert("bus".into(), prev);
                }
                None => {
                    self.bus_bindings.remove("bus");
                }
            }
            for n in added_events {
                self.event_types.remove(&n);
            }
            for (name, prev) in saved_field_types {
                match prev {
                    Some(ty) => {
                        self.let_types.insert(name, ty);
                    }
                    None => {
                        self.let_types.remove(&name);
                    }
                }
            }
            self.field_subs = prev_subs;

            self.pad(depth + 2);
            writeln!(self.out, "{lane_rsp_valid}[{lane}].store(true);").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "co_await harc_rt::wait_until(_slot, [&]{{ return !{lane_rsp_valid}[{lane}].load(); }});"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(self.out, "{lane_busy}[{lane}].store(false);").ok();
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth + 1);
            writeln!(self.out, "co_return;").ok();
            self.pad(depth);
            writeln!(self.out, "}};").ok();
            self.pad(depth);
            writeln!(
                self.out,
                "{lane_slot}.thread = {lane_slot}_lambda(&{lane_slot});"
            )
            .ok();
        }

        if self.mt {
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadScheduler {arbiter_sched};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "harc_rt::ThreadSlot {arbiter_slot};").ok();
        self.pad(depth);
        if self.mt {
            writeln!(
                self.out,
                "{arbiter_sched}.slots.push_back(&{arbiter_slot});"
            )
            .ok();
            self.actor_threads
                .push((arbiter_sched.clone(), arbiter_slot.clone()));
        } else {
            writeln!(self.out, "sched.slots.push_back(&{arbiter_slot});").ok();
        }
        self.pad(depth);
        writeln!(
            self.out,
            "auto {arbiter_slot}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ for (size_t i = 0; i < {tag_count}; ++i) if ({lane_rsp_valid}[i].load()) return true; return false; }});"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "int _sel = -1;").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "for (int i = {tag_count} - 1; i >= 0; --i) if ({lane_rsp_valid}[(size_t)i].load()) {{ _sel = i; break; }}"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "if (_sel >= 0) {{").ok();
        if method.ret.is_some() {
            let rsp_stmt = method
                .ret
                .as_ref()
                .map(|ret| {
                    self.record_drive_stmt(
                        ret,
                        &format!("{root}->{rsp_data}"),
                        &format!("{lane_rsp_data}[(size_t)_sel]"),
                    )
                })
                .unwrap_or_else(|| {
                    format!(
                        "harc_rt::harc_assign({root}->{rsp_data}, {lane_rsp_data}[(size_t)_sel]);"
                    )
                });
            self.pad(depth + 3);
            writeln!(self.out, "{rsp_stmt}").ok();
        }
        self.pad(depth + 3);
        writeln!(self.out, "{root}->{rsp_tag} = _sel;").ok();
        self.emit_tlm_call_trace_event(
            instance,
            "bus",
            &method.name.name,
            "response",
            "target",
            Some("_sel"),
            depth + 3,
        );
        self.pad(depth + 3);
        writeln!(self.out, "{root}->{rsp_valid} = 1;").ok();
        self.pad(depth + 3);
        writeln!(
            self.out,
            "if (!{root}->{rsp_ready}) co_await harc_rt::wait_until(_slot, [&]{{ return {root}->{rsp_ready}; }});"
        )
        .ok();
        self.pad(depth + 3);
        writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
        self.pad(depth + 3);
        writeln!(self.out, "{root}->{rsp_valid} = 0;").ok();
        self.pad(depth + 3);
        writeln!(self.out, "{lane_rsp_valid}[(size_t)_sel].store(false);").ok();
        self.pad(depth + 3);
        writeln!(
            self.out,
            "{instance}._last_out_cycle = (uint64_t)cycle_count;"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "co_return;").ok();
        self.pad(depth);
        writeln!(self.out, "}};").ok();
        self.pad(depth);
        writeln!(
            self.out,
            "{arbiter_slot}.thread = {arbiter_slot}_lambda(&{arbiter_slot});"
        )
        .ok();
    }

    /// Phase 2b: if `comp` is a `bound to BusType` driver/agent with
    /// exactly one `in event<T>` field plus a matching
    /// `on <event_name>(t)` handler, emit it as an independent
    /// coroutine actor:
    ///
    /// 1. A per-instance transaction queue (`std::deque<T>`).
    /// 2. A subscriber on the input event that *pushes* incoming
    ///    transactions onto the queue (no longer runs the body
    ///    inline — the actor body lives in the coroutine).
    /// 3. A `ThreadSlot` registered with `sched`.
    /// 4. The actor coroutine: loop forever, `co_await wait_until`
    ///    until the queue is non-empty, pop a transaction, run the
    ///    `on T t` handler body in coroutine context (so internal
    ///    `wait N cycles` and `bus.<ch>.send/recv` lower to `co_await`),
    ///    repeat. The actor never returns; the main loop terminates
    ///    when the *run* coroutine finishes, abandoning auxiliary
    ///    actors mid-WaitUntil.
    ///
    /// Returns `true` on success — caller should NOT also emit the
    /// sync subscriber path. Returns `false` to fall through.
    fn try_emit_bound_driver_actor(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        binding: &(BusDecl, String, String),
    ) -> bool {
        // Only `agent` and `transactor` (synth ComponentDecl from a
        // bound active transactor) are eligible — envs / sequencers /
        // scoreboards aren't actor-shaped at all.
        if !matches!(comp.kind, ComponentKind::Agent | ComponentKind::Transactor) {
            return false;
        }

        // Find a single `in event<T>` field. Multi-input drivers stay
        // sync for now (each input would need its own queue + the
        // coroutine would have to multi-way wait — separate PR).
        let mut input_event: Option<(String, String)> = None;
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                if matches!(f.direction, Some(Direction::In)) {
                    if let TypeExpr::Builtin {
                        name: BuiltinTy::Event,
                        args,
                        ..
                    } = &f.ty
                    {
                        let payload = self.payload_type_for_arg(args.first());
                        if input_event.is_some() {
                            return false; // multi-input — bail out
                        }
                        input_event = Some((f.name.name.clone(), payload));
                    }
                }
            }
        }
        let Some((event_name, payload_ty)) = input_event else {
            return false; // no input event — nothing to actor-ify
        };

        // Find the matching on-handler. If absent or there's more
        // than one matching, fall back. (Other handlers on different
        // events are fine — they just don't get coroutine-ified by
        // this PR; the sync registration still runs for them.)
        let mut matched_handler: Option<&OnHandler> = None;
        let mut other_handlers: Vec<&OnHandler> = Vec::new();
        for it in &comp.items {
            if let ComponentItem::OnHandler(h) = it {
                if let Some((ev, _arg)) = extract_event_subscription(&h.event) {
                    if ev == event_name {
                        if matched_handler.is_some() {
                            return false; // ambiguous multi-handler — bail
                        }
                        matched_handler = Some(h);
                    } else {
                        other_handlers.push(h);
                    }
                }
            }
        }
        let Some(handler) = matched_handler else {
            return false;
        };
        let arg_name = match extract_event_subscription(&handler.event) {
            Some((_, a)) => a,
            None => return false,
        };

        // ── Emit the actor topology ─────────────────────────────────
        // Cooperative mode (default): all actor slots go into the
        // single `sched` and tick together with `_run_slot` on the
        // main thread. This is faster than MT on typical fixtures
        // because there's no barrier overhead.
        //
        // MT mode (`--mt`, Phase 3a): each actor gets its own
        // `ThreadScheduler` and lives on a dedicated OS thread,
        // synchronized via dual barriers per posedge. `tick()`
        // itself is not MT-safe so per-actor schedulers avoid locks.
        let queue_var = format!("_{instance}_q");
        let slot_var = format!("_{instance}_slot");
        let sched_var = format!("_{instance}_sched");

        self.pad(depth);
        writeln!(self.out, "std::deque<{payload_ty}> {queue_var};").ok();
        if self.mt {
            self.pad(depth);
            writeln!(self.out, "harc_rt::ThreadScheduler {sched_var};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "harc_rt::ThreadSlot {slot_var};").ok();
        self.pad(depth);
        if self.mt {
            writeln!(self.out, "{sched_var}.slots.push_back(&{slot_var});").ok();
            self.actor_threads
                .push((sched_var.clone(), slot_var.clone()));
        } else {
            writeln!(self.out, "sched.slots.push_back(&{slot_var});").ok();
        }

        // Pusher subscriber on the input event. Replaces the sync
        // body-callback; emit/connect bridges that fan out to this
        // event now feed the actor's queue.
        self.pad(depth);
        writeln!(
            self.out,
            "{instance}.{event_name}.push_back([&]({payload_ty} {arg_name}) {{ {queue_var}.push_back({arg_name}); }});",
        ).ok();

        // Spawn the coroutine. Body emits the on-handler body in
        // coroutine context with the bus binding active for typed
        // signal access.
        self.pad(depth);
        writeln!(
            self.out,
            "auto {slot_var}_lambda = [&](harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread {{",
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (true) {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "co_await harc_rt::wait_until(_slot, [&]{{ return !{queue_var}.empty(); }});",
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "auto {arg_name} = {queue_var}.front();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "{queue_var}.pop_front();").ok();
        // Activity tracking (spec §7.x): popping a transaction off
        // the per-actor queue counts as an "in" for this instance.
        self.pad(depth + 2);
        writeln!(
            self.out,
            "{instance}._last_in_cycle = (uint64_t)cycle_count;"
        )
        .ok();

        // Build field-name substitution map: bare names inside the
        // handler body resolve to `instance.field`. Event-typed
        // fields also register their payload types so any nested
        // `emit <ev>(arg)` finds the right param shape.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types: Vec<(String, String)> = Vec::new();
        let mut saved_field_types = Vec::new();
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                let field_ref = if self.is_dut_pointer_field_type(&f.ty)
                    && self.pointer_vars.contains(&f.name.name)
                {
                    f.name.name.clone()
                } else {
                    format!("{instance}.{}", f.name.name)
                };
                subs.insert(f.name.name.clone(), field_ref);
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    let name = f.name.name.clone();
                    let prev = self.let_types.insert(name.clone(), ty.to_string());
                    saved_field_types.push((name, prev));
                }
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);
        let mut added_events = Vec::new();
        for (name, ty) in &local_event_types {
            if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                added_events.push(name.clone());
            }
        }

        // Push the bus binding under the alias `"bus"` so
        // `bus.<ch>.send/recv` and `bus.<ch>.<sig>` inside the actor
        // body resolve to flat DUT signals via the bus-typing
        // lowerers — same mechanism Phase 2a's sync path uses.
        let prior_bus = self.bus_bindings.insert("bus".into(), binding.clone());
        // Mark coroutine context so wait/tick lower to co_await.
        let prior_corout = self.in_coroutine;
        self.in_coroutine = true;
        // Set current-component scope so `emit`/`bus.<ch>.send/recv`
        // nested in the actor body bump this instance's heartbeat
        // fields.
        let prior_inst = std::mem::replace(
            &mut self.current_component_instance,
            Some(instance.to_string()),
        );

        self.emit_block(&handler.body, depth + 2);

        self.current_component_instance = prior_inst;
        self.in_coroutine = prior_corout;
        match prior_bus {
            Some(prev) => {
                self.bus_bindings.insert("bus".into(), prev);
            }
            None => {
                self.bus_bindings.remove("bus");
            }
        }
        self.field_subs = prev_subs;
        for (name, prev) in saved_field_types {
            match prev {
                Some(ty) => {
                    self.let_types.insert(name, ty);
                }
                None => {
                    self.let_types.remove(&name);
                }
            }
        }
        for n in added_events {
            self.event_types.remove(&n);
        }

        self.pad(depth + 1);
        writeln!(self.out, "}}").ok(); // close while(true)
        self.pad(depth + 1);
        writeln!(self.out, "co_return;").ok(); // unreachable but required
        self.pad(depth);
        writeln!(self.out, "}};").ok();
        self.pad(depth);
        writeln!(
            self.out,
            "{slot_var}.thread = {slot_var}_lambda(&{slot_var});"
        )
        .ok();

        // Other on-handlers (on different events) still register
        // sync — they don't go through the actor's queue and may
        // legitimately want sync dispatch.
        if !other_handlers.is_empty() {
            let tag = format!("_{}_", comp.kind.keyword());
            // Only register the non-matching handlers via the sync
            // path. We do that by emitting them one-at-a-time using
            // the existing helper after temporarily replacing comp's
            // items list. Simpler: filter manually here.
            let mut filtered = comp.clone();
            filtered.items.retain(|it| match it {
                ComponentItem::OnHandler(h) => extract_event_subscription(&h.event)
                    .map(|(ev, _)| ev != event_name)
                    .unwrap_or(true),
                _ => true,
            });
            self.emit_component_handler_registrations_bound(
                &filtered,
                instance,
                depth,
                &tag,
                Some(binding.clone()),
            );
        }

        true
    }

    fn emit_component_handler_registrations_bound(
        &mut self,
        comp: &ComponentDecl,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
        bound_bus: Option<(BusDecl, String, String)>,
    ) {
        // Build field-name substitution map for the body. Component
        // fields visible by bare name inside the handler body get
        // prefixed with the instance path.
        let mut subs = std::collections::HashMap::new();
        let mut local_event_types = Vec::new();
        let mut saved_field_types = Vec::new();
        let mut event_field_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for it in &comp.items {
            if let ComponentItem::Field(f) = it {
                let field_ref = if self.is_dut_pointer_field_type(&f.ty)
                    && self.pointer_vars.contains(&f.name.name)
                {
                    f.name.name.clone()
                } else {
                    format!("{instance}.{}", f.name.name)
                };
                subs.insert(f.name.name.clone(), field_ref);
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    let name = f.name.name.clone();
                    let prev = self.let_types.insert(name.clone(), ty.to_string());
                    saved_field_types.push((name, prev));
                }
                if let TypeExpr::Builtin {
                    name: BuiltinTy::Event,
                    args,
                    ..
                } = &f.ty
                {
                    event_field_names.insert(f.name.name.clone());
                    let inner = self.payload_type_for_arg(args.first());
                    local_event_types.push((f.name.name.clone(), inner));
                }
            }
        }

        let prev_subs = std::mem::replace(&mut self.field_subs, subs);
        let mut added_events = Vec::new();
        for (name, ty) in &local_event_types {
            if self.event_types.insert(name.clone(), ty.clone()).is_none() {
                added_events.push(name.clone());
            }
        }

        let comp_ty = comp.name.name.clone();
        for it in &comp.items {
            if let ComponentItem::OnHandler(h) = it {
                // While emitting a transactor's `on`-handler body, expose
                // the same `current_component_method` context that
                // ordinary methods do — receiver = instance path —
                // so a bare-ident call to a transactor-local
                // `hookable` rewrites to `<Type>_<method>(<instance>,
                // …)` (issue #300). Without this, the call emits as a
                // raw identifier and the generated C++ fails to
                // compile because the helper symbol isn't in scope.
                //
                // The override is installed via `CurrentMethodGuard`
                // so restoration happens on drop — any exit path
                // from `emit_single_on_handler_bound` (early
                // `return`, `?`, panic, fall-through) cannot leak
                // the override into the next iteration. A future
                // maintainer adding a new exit gets the restore for
                // free, no manual line to remember.
                let handler_active = self.handler_lives_in_when_active(&comp_ty, h);
                let mut method_guard = CurrentMethodGuard::new(
                    self,
                    Some((
                        comp_ty.clone(),
                        instance.to_string(),
                        format!("on_{}_{}", h.span.start, h.span.end),
                        handler_active,
                    )),
                );
                method_guard.emit_single_on_handler_bound(
                    h,
                    instance,
                    depth,
                    tag_prefix,
                    &event_field_names,
                    bound_bus.as_ref(),
                );
            }
        }

        self.field_subs = prev_subs;
        for (name, prev) in saved_field_types {
            match prev {
                Some(ty) => {
                    self.let_types.insert(name, ty);
                }
                None => {
                    self.let_types.remove(&name);
                }
            }
        }
        for n in added_events {
            self.event_types.remove(&n);
        }
    }

    /// Emit a single `on`-handler registration for a bound component
    /// instance. Picks one of two shapes based on the trigger:
    ///   * event subscription (`on field { ... }`) — push a closure
    ///     into the field's subscriber list.
    ///   * bool-expression cycle trigger / periodic `on N cycles`
    ///     trigger (monitors, post_eval/checker) — fall through to
    ///     `emit_cycle_trigger`.
    ///
    /// Caller is responsible for installing
    /// `current_component_method` for this handler (via
    /// `CurrentMethodGuard`) before invoking; this helper only
    /// manages the `current_component_instance` scope local to its
    /// own arms.
    fn emit_single_on_handler_bound(
        &mut self,
        h: &OnHandler,
        instance: &str,
        depth: usize,
        tag_prefix: &str,
        event_field_names: &std::collections::HashSet<String>,
        bound_bus: Option<&(BusDecl, String, String)>,
    ) {
        if let Some((event_name, arg_name)) = extract_event_subscription(&h.event) {
            if event_field_names.contains(&event_name) {
                // Subscriber to a component event field.
                let arg_ty = self
                    .event_types
                    .get(&event_name)
                    .cloned()
                    .unwrap_or_else(|| "int64_t".into());
                self.pad(depth);
                writeln!(
                    self.out,
                    "{instance}.{event_name}.push_back([&]({arg_ty} {arg_name}) {{",
                )
                .ok();
                // Activity tracking (spec §7.x): an incoming event
                // counts as an "in" for the component instance —
                // bump _last_in_cycle to the current cycle. The
                // current-component-instance scope is also set
                // so any `emit`/`bus.send`/`bus.recv` *inside*
                // this body bumps the same instance's
                // _last_out_cycle (those sites can't see the
                // static `instance` parameter directly).
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "{instance}._last_in_cycle = (uint64_t)cycle_count;"
                )
                .ok();
                let prior_inst = std::mem::replace(
                    &mut self.current_component_instance,
                    Some(instance.to_string()),
                );
                // For `bound to BusType` components, expose the
                // driver's bus binding inside the handler body
                // as the bare identifier `bus`. Same root +
                // BusDecl as the test-scope binding it was
                // bound from, so `bus.<ch>.send/recv` and
                // `bus.<ch>.<sig>` lower through the existing
                // bus_handshake / bus_field_access paths.
                let prior_bus = bound_bus
                    .as_ref()
                    .and_then(|b| self.bus_bindings.insert("bus".into(), (*b).clone()));
                self.emit_block(&h.body, depth + 1);
                if bound_bus.is_some() {
                    match prior_bus {
                        Some(prev) => {
                            self.bus_bindings.insert("bus".into(), prev);
                        }
                        None => {
                            self.bus_bindings.remove("bus");
                        }
                    }
                }
                self.current_component_instance = prior_inst;
                self.pad(depth);
                writeln!(self.out, "}});").ok();
                return;
            }
        }
        // Fallback: bool-expression cycle trigger (monitors) or
        // periodic `on N cycles` trigger (post_eval / checker).
        let prior_inst = std::mem::replace(
            &mut self.current_component_instance,
            Some(instance.to_string()),
        );
        self.emit_cycle_trigger(h, depth, tag_prefix);
        self.current_component_instance = prior_inst;
    }

    /// Emit the C++ boolean expression for "is `_v` in this bin?".
    ///   `{x}`        — `(_v == x)`
    ///   `{a, b, c}`  — `(_v == a || _v == b || _v == c)`
    ///   `[a..b]`     — `(_v >= a && _v <= b)`
    ///   `(inner)`    — recurse
    ///   anything else — falls back to `(_v == <expr>)` (e.g. a single Int).
    fn emit_bin_membership(&mut self, spec: &Expr) {
        match &*spec.kind {
            ExprKind::SetLit(items) => {
                write!(self.out, "(").ok();
                if items.is_empty() {
                    write!(self.out, "false").ok();
                } else {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            write!(self.out, " || ").ok();
                        }
                        // Recurse — set-of-ranges (`{[1..3], 7}`) works.
                        self.emit_bin_membership(it);
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::RangeLit { lo, hi } => {
                write!(self.out, "(").ok();
                if let Some(l) = lo {
                    write!(self.out, "_v >= ").ok();
                    self.emit_expr(l);
                    if hi.is_some() {
                        write!(self.out, " && ").ok();
                    }
                }
                if let Some(h) = hi {
                    write!(self.out, "_v <= ").ok();
                    self.emit_expr(h);
                }
                if lo.is_none() && hi.is_none() {
                    write!(self.out, "true").ok();
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => self.emit_bin_membership(inner),
            // Default: equality test against the expression. Covers the
            // single-value form (`{1}` parses as a SetLit but `1` alone
            // would land here).
            _ => {
                write!(self.out, "(_v == ").ok();
                self.emit_expr(spec);
                write!(self.out, ")").ok();
            }
        }
    }

    /// Emit a covergroup as a C++ struct: one nested struct per cover-point
    /// holding `uint64_t` bin counters plus sample-local cross counters.
    /// Crosses are updated only from bins hit in the same sample invocation;
    /// this is deliberately a semantic sample record, not a post-hoc mix of
    /// bins hit at unrelated times.
    fn emit_covergroup_struct(&mut self, g: &CovergroupDecl) {
        let auto_crosses = covergroup_auto_crosses(g);
        let declared_crosses = match covergroup_declared_crosses(g) {
            Ok(crosses) => crosses,
            Err(err) => {
                self.errors.push(err);
                Vec::new()
            }
        };
        writeln!(self.out, "struct {} {{", g.name.name).ok();
        // Per-cover-point nested struct of bin counters. The struct is
        // pure data; the sample() logic is emitted at `let cov : G` time
        // as a `_checkers` closure (so `dut` is in scope via `[&]`
        // capture). report() lives here because it only reads its own
        // counters.
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                writeln!(self.out, "{INDENT}struct {{").ok();
                for b in &p.bins {
                    writeln!(self.out, "{INDENT}{INDENT}uint64_t {} = 0;", b.name.name).ok();
                }
                writeln!(self.out, "{INDENT}}} {};", p.name.name).ok();
            }
        }
        for (a, b) in &auto_crosses {
            writeln!(
                self.out,
                "{INDENT}uint64_t _auto_cross_{}__{}[{}][{}] = {{}};",
                a.name.name,
                b.name.name,
                a.bins.len(),
                b.bins.len()
            )
            .ok();
        }
        for cross in &declared_crosses {
            writeln!(
                self.out,
                "{INDENT}uint64_t {}[{}] = {{}};",
                cross.storage, cross.total_bins
            )
            .ok();
        }
        writeln!(self.out, "").ok();
        // report() — ARCH-format coverage dump (header line + per-bin
        // lines + `*NOT HIT*` marker). Mirrors arch-com's
        // sim_codegen/fsm.rs `_arch_cov_dump` shape, but writes to stdout
        // (not stderr): in a HARC TB the program's stdout already IS
        // the framework log (everything the user wrote via log() goes
        // there too). ARCH writes to stderr because in ARCH-sim, stdout
        // belongs to the user's test program — that distinction doesn't
        // apply to a HARC-emitted TB.
        let total_bins: usize = g
            .items
            .iter()
            .map(|it| match it {
                CoverItem::Point(p) => p.bins.len(),
                _ => 0,
            })
            .sum();
        writeln!(self.out, "{INDENT}void report() const {{").ok();
        self.pad(2);
        writeln!(
            self.out,
            "uint64_t _total = {total_bins}; uint64_t _hit = 0;"
        )
        .ok();
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                for b in &p.bins {
                    self.pad(2);
                    writeln!(self.out, "if ({}.{} > 0) _hit++;", p.name.name, b.name.name).ok();
                }
            }
        }
        self.pad(2);
        writeln!(
            self.out,
            "harc_rt::log::harc_print_covergroup_summary(\"{}\", _hit, _total);",
            g.name.name
        )
        .ok();
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                for b in &p.bins {
                    self.pad(2);
                    writeln!(
                        self.out,
                        "harc_rt::log::harc_print_covergroup_bin(\"{0}\", \"{1}\", {0}.{1});",
                        p.name.name, b.name.name
                    )
                    .ok();
                }
            }
        }
        for cross in &declared_crosses {
            self.pad(2);
            writeln!(self.out, "{{").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_hit = 0;").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_missing = 0;").ok();
            self.pad(3);
            writeln!(
                self.out,
                "for (size_t _i = 0; _i < {}; ++_i) if ({}[_i] > 0) _cross_hit++;",
                cross.total_bins, cross.storage
            )
            .ok();
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_cross_summary(\"{}\", \"cross\", \"{}\", _cross_hit, {});",
                g.name.name,
                escape_c(&cross.label),
                cross.total_bins
            )
            .ok();
            for (idx, label) in declared_cross_bin_labels(cross) {
                self.pad(3);
                writeln!(
                    self.out,
                    "if ({}[{}] == 0) {{ if (_cross_missing < {}) harc_rt::log::harc_print_covergroup_missing_bin(\"{}\"); _cross_missing++; }}",
                    cross.storage,
                    idx,
                    COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
                    escape_c(&label)
                )
                .ok();
            }
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, {}, \"cross\");",
                COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
            )
            .ok();
            self.pad(2);
            writeln!(self.out, "}}").ok();
        }
        for (a, b) in &auto_crosses {
            self.pad(2);
            writeln!(self.out, "{{").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_hit = 0;").ok();
            self.pad(3);
            writeln!(self.out, "uint64_t _cross_missing = 0;").ok();
            self.pad(3);
            writeln!(
                self.out,
                "for (size_t _i = 0; _i < {}; ++_i) for (size_t _j = 0; _j < {}; ++_j) if (_auto_cross_{}__{}[_i][_j] > 0) _cross_hit++;",
                a.bins.len(),
                b.bins.len(),
                a.name.name,
                b.name.name
            )
            .ok();
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_cross_summary(\"{}\", \"auto_cross\", \"{} x {}\", _cross_hit, {});",
                escape_c(&g.name.name),
                escape_c(&a.name.name),
                escape_c(&b.name.name),
                a.bins.len() * b.bins.len()
            )
            .ok();
            for (i, ab) in a.bins.iter().enumerate() {
                for (j, bb) in b.bins.iter().enumerate() {
                    self.pad(3);
                    writeln!(
                        self.out,
                        "if (_auto_cross_{}__{}[{}][{}] == 0) {{ if (_cross_missing < {}) harc_rt::log::harc_print_covergroup_missing_bin(\"{}.{} x {}.{}\"); _cross_missing++; }}",
                        a.name.name,
                        b.name.name,
                        i,
                        j,
                        COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
                        a.name.name,
                        ab.name.name,
                        b.name.name,
                        bb.name.name
                    )
                    .ok();
                }
            }
            self.pad(3);
            writeln!(
                self.out,
                "harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, {}, \"auto-cross\");",
                COVERGROUP_CROSS_MISSING_DETAIL_LIMIT,
            )
            .ok();
            self.pad(2);
            writeln!(self.out, "}}").ok();
        }
        writeln!(self.out, "{INDENT}}}").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// At a `let cov : G` site, register the covergroup's sample logic.
    /// Clock/no-trigger covergroups use the legacy per-cycle `_checkers`
    /// path; hook-triggered covergroups subscribe to the existing
    /// `<Type>_<method>_pre/post` hook vectors so sampling occurs at the
    /// semantic hook invocation point rather than every cycle.
    fn emit_covergroup_sample_registration(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        depth: usize,
    ) {
        match &g.trigger {
            Some(CoverTrigger::Hook { call, side }) => {
                self.emit_covergroup_hook_sample_registration(g, instance, call, *side, depth);
            }
            _ => self.emit_covergroup_clock_sample_registration(g, instance, depth),
        }
    }

    fn emit_covergroup_clock_sample_registration(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        depth: usize,
    ) {
        let binned_points = covergroup_binned_points(g);
        let auto_crosses = covergroup_auto_crosses(g);
        let declared_crosses = match covergroup_declared_crosses(g) {
            Ok(crosses) => crosses,
            Err(_) => Vec::new(),
        };
        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        self.emit_covergroup_sample_body(
            g,
            instance,
            &binned_points,
            &auto_crosses,
            &declared_crosses,
            depth + 1,
        );
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    fn emit_covergroup_hook_sample_registration(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        call: &Expr,
        side: HookSide,
        depth: usize,
    ) {
        let ExprKind::Call { callee, args } = &*call.kind else {
            self.errors.push(format!(
                "covergroup `{}` hook trigger must be a method call",
                g.name.name
            ));
            return;
        };
        let Some((comp_ty, method_name, params)) = self.resolve_component_hookable(callee) else {
            self.errors.push(format!(
                "covergroup `{}` hook trigger must resolve to a `hookable` on a known component type",
                g.name.name
            ));
            return;
        };
        if args.len() != params.len() {
            self.errors.push(format!(
                "covergroup `{}` hook trigger `{method_name}` expects {} argument(s), got {}",
                g.name.name,
                params.len(),
                args.len()
            ));
            return;
        }
        for (arg, param) in args.iter().zip(params.iter()) {
            let CallArg::Expr(arg_expr) = arg else {
                self.errors.push(format!(
                    "covergroup `{}` hook trigger arguments must be identifiers",
                    g.name.name
                ));
                return;
            };
            let ExprKind::Ident(arg_name) = &*arg_expr.kind else {
                self.errors.push(format!(
                    "covergroup `{}` hook trigger arguments must be identifiers",
                    g.name.name
                ));
                return;
            };
            if arg_name.name != param.name.name || arg_name.name == "_" {
                self.errors.push(format!(
                    "covergroup `{}` hook trigger argument `{}` must match hook parameter `{}`",
                    g.name.name, arg_name.name, param.name.name
                ));
                return;
            }
        }

        let side_str = match side {
            HookSide::Pre => "pre",
            HookSide::Post => "post",
        };
        let param_names = cpp_param_names(&params);
        let arg_decls: Vec<String> = params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let ty =
                    p.ty.as_ref()
                        .map(|t| self.c_type_for_param(t))
                        .unwrap_or_else(|| "int64_t".to_string());
                format!("{ty} {}", param_names[i])
            })
            .collect();
        let binned_points = covergroup_binned_points(g);
        let auto_crosses = covergroup_auto_crosses(g);
        let declared_crosses = match covergroup_declared_crosses(g) {
            Ok(crosses) => crosses,
            Err(_) => Vec::new(),
        };
        self.pad(depth);
        writeln!(
            self.out,
            "{comp_ty}_{method_name}_{side_str}.push_back([&]({}) {{",
            arg_decls.join(", "),
        )
        .ok();
        self.emit_covergroup_sample_body(
            g,
            instance,
            &binned_points,
            &auto_crosses,
            &declared_crosses,
            depth + 1,
        );
        self.pad(depth);
        writeln!(self.out, "}});").ok();
    }

    fn emit_covergroup_sample_body(
        &mut self,
        g: &CovergroupDecl,
        instance: &str,
        binned_points: &[&CoverPoint],
        auto_crosses: &[(&CoverPoint, &CoverPoint)],
        declared_crosses: &[DeclaredCoverCross<'_>],
        depth: usize,
    ) {
        if !auto_crosses.is_empty() || !declared_crosses.is_empty() {
            for p in binned_points {
                self.pad(depth);
                writeln!(
                    self.out,
                    "bool _cg_hit_{}[{}] = {{}};",
                    p.name.name,
                    p.bins.len()
                )
                .ok();
            }
        }
        for it in &g.items {
            if let CoverItem::Point(p) = it {
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                write!(self.out, "uint64_t _v = (uint64_t)(").ok();
                self.emit_expr(&p.target);
                writeln!(self.out, ");").ok();
                for (bin_idx, b) in p.bins.iter().enumerate() {
                    self.pad(depth + 1);
                    write!(self.out, "if (").ok();
                    self.emit_bin_membership(&b.spec);
                    if auto_crosses.is_empty() && declared_crosses.is_empty() {
                        writeln!(self.out, ") {instance}.{}.{}++;", p.name.name, b.name.name).ok();
                    } else {
                        writeln!(
                            self.out,
                            ") {{ {instance}.{}.{}++; _cg_hit_{}[{}] = true; }}",
                            p.name.name, b.name.name, p.name.name, bin_idx
                        )
                        .ok();
                    }
                }
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
        for (a, b) in auto_crosses {
            self.pad(depth);
            writeln!(
                self.out,
                "for (size_t _i = 0; _i < {}; ++_i) {{",
                a.bins.len()
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (size_t _j = 0; _j < {}; ++_j) {{",
                b.bins.len()
            )
            .ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "if (_cg_hit_{}[_i] && _cg_hit_{}[_j]) {instance}._auto_cross_{}__{}[_i][_j]++;",
                a.name.name, b.name.name, a.name.name, b.name.name
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            writeln!(self.out, "}}").ok();
        }
        for cross in declared_crosses {
            self.emit_declared_cover_cross_sample_update(instance, cross, depth);
        }
    }

    fn emit_declared_cover_cross_sample_update(
        &mut self,
        instance: &str,
        cross: &DeclaredCoverCross<'_>,
        depth: usize,
    ) {
        for (idx, point) in cross.points.iter().enumerate() {
            self.pad(depth + idx);
            writeln!(
                self.out,
                "for (size_t _i{idx} = 0; _i{idx} < {}; ++_i{idx}) {{",
                point.bins.len()
            )
            .ok();
        }
        self.pad(depth + cross.points.len());
        let hit_cond = cross
            .points
            .iter()
            .enumerate()
            .map(|(idx, point)| format!("_cg_hit_{}[_i{idx}]", point.name.name))
            .collect::<Vec<_>>()
            .join(" && ");
        writeln!(self.out, "if ({hit_cond}) {{").ok();
        self.pad(depth + cross.points.len() + 1);
        writeln!(
            self.out,
            "{instance}.{}[{}]++;",
            cross.storage,
            declared_cross_index_expr(cross)
        )
        .ok();
        self.pad(depth + cross.points.len());
        writeln!(self.out, "}}").ok();
        for idx in (0..cross.points.len()).rev() {
            self.pad(depth + idx);
            writeln!(self.out, "}}").ok();
        }
    }

    /// Emit a scoreboard declaration as a C++ struct. Only the field list
    /// is lowered for v0 — `connect` clauses and `on` handlers in the
    /// scoreboard body would need event registration that's not in scope.
    /// Each field default-initialises (queues to empty, ints to 0). The
    /// user drives push/pop/etc. directly from test code.
    /// Resolve a bus-bound field access. Returns `Some(c++_string)`
    /// when `target.name` (or `target.ch.name`) names a signal under
    /// a known bus binding; `None` otherwise (caller falls back to
    /// generic field-access lowering).
    ///
    /// Naming convention mirrors arch-com §19.6: each plain bus
    /// signal flattens to `<binding>_<signal>` on the DUT pointer,
    /// each handshake_channel signal flattens to
    /// `<binding>_<channel>_<signal>` (and the implicit `valid` /
    /// `ready` signals flatten the same way).
    /// Detect `<bus>.<method>(args)` for a `tlm_method` declared on a bus
    /// binding and emit ARCH-compatible blocking req/rsp wire protocol:
    /// `<prefix>_<method>_req_valid`, `<prefix>_<method>_<arg>`,
    /// `<prefix>_<method>_req_ready`, `<prefix>_<method>_rsp_valid`,
    /// optional `<prefix>_<method>_rsp_data`, and
    /// `<prefix>_<method>_rsp_ready`.
    fn try_emit_bus_tlm_method(&mut self, e: &Expr, let_name: Option<&str>, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Field {
            target,
            name: method_name,
        } = &*callee.kind
        else {
            return false;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            return false;
        };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else {
            return false;
        };
        let Some(method) = bus
            .tlm_methods
            .iter()
            .find(|m| m.name.name == method_name.name)
            .cloned()
        else {
            return false;
        };
        if method.mode.name != "blocking" {
            self.errors.push(format!(
                "bus.{}: HARC direct-Verilator lowering currently supports only `blocking` tlm_method calls; `{}` is parsed for ARCH compatibility but not lowered here",
                method.name.name, method.mode.name
            ));
            return true;
        }
        if args.len() != method.args.len() {
            self.errors.push(format!(
                "bus.{}: expected {} arg(s), got {}",
                method.name.name,
                method.args.len(),
                args.len(),
            ));
            return true;
        }
        if let_name.is_some() && method.ret.is_none() {
            self.errors.push(format!(
                "bus.{} returns no value; use it as a statement",
                method.name.name,
            ));
            return true;
        }

        let req_valid = self.bus_signal_name(&sig_prefix, &method.name.name, "req_valid");
        let req_ready = self.bus_signal_name(&sig_prefix, &method.name.name, "req_ready");
        let rsp_valid = self.bus_signal_name(&sig_prefix, &method.name.name, "rsp_valid");
        let rsp_ready = self.bus_signal_name(&sig_prefix, &method.name.name, "rsp_ready");
        let rsp_data = self.bus_signal_name(&sig_prefix, &method.name.name, "rsp_data");
        let component = self.trace_component_context();

        self.pad(depth);
        writeln!(self.out, "// bus.{} tlm_method", method.name.name).ok();
        for ((arg_name, _), arg) in method.args.iter().zip(args.iter()) {
            let sig_port = self.bus_signal_name(&sig_prefix, &method.name.name, &arg_name.name);
            self.pad(depth);
            write!(self.out, "harc_rt::harc_assign({root}->{sig_port}, ").ok();
            match arg {
                CallArg::Expr(e) => self.emit_expr(e),
                CallArg::Named { value, .. } => self.emit_expr(value),
            }
            writeln!(self.out, ");").ok();
        }
        self.emit_tlm_call_trace_event(
            &component,
            &id.name,
            &method.name.name,
            "request",
            "initiator",
            None,
            depth,
        );
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 1;").ok();
        let advance = if self.in_coroutine {
            "co_await harc_rt::wait_cycles(_slot, 1)"
        } else {
            "tick()"
        };
        self.pad(depth);
        writeln!(
            self.out,
            "{}",
            crate::codegen::bounded_handshake_wait(
                &format!("{root}->{req_ready}"),
                crate::codegen::TLM_WAIT_BOUND,
                advance,
                &format!("TLM {}.{} request", id.name, method.name.name),
            )
        )
        .ok();
        self.pad(depth);
        writeln!(self.out, "{advance};").ok();
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 0;").ok();

        self.pad(depth);
        writeln!(self.out, "{root}->{rsp_ready} = 1;").ok();
        if let Some(name) = let_name {
            if let Some(ret) = &method.ret {
                self.pad(depth);
                let cty = self.record_field_c_type(ret);
                writeln!(self.out, "{cty} {name} = {{}};").ok();
            }
        }
        self.pad(depth);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "bool _rsp_ok = true;").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "{}",
            crate::codegen::bounded_handshake_wait_into(
                &format!("{root}->{rsp_valid}"),
                crate::codegen::TLM_WAIT_BOUND,
                advance,
                &format!("TLM {}.{} response", id.name, method.name.name),
                "_rsp_ok",
            )
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "if (_rsp_ok) {{").ok();
        self.emit_tlm_call_trace_event(
            &component,
            &id.name,
            &method.name.name,
            "response",
            "initiator",
            None,
            depth + 2,
        );
        if let Some(name) = let_name {
            if let Some(ret) = &method.ret {
                self.pad(depth + 2);
                let read_expr = self.record_unpack_expr(ret, &format!("{root}->{rsp_data}"));
                writeln!(self.out, "{name} = {read_expr};").ok();
            }
        }
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        if self.in_coroutine {
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
        } else {
            writeln!(self.out, "tick();").ok();
        }
        self.pad(depth);
        writeln!(self.out, "{root}->{rsp_ready} = 0;").ok();

        if let Some(inst) = self.current_component_instance.clone() {
            self.pad(depth);
            writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
            self.pad(depth);
            writeln!(self.out, "{inst}._last_in_cycle = (uint64_t)cycle_count;").ok();
        }
        true
    }

    /// Detect `fork bus.<method>(args)` and emit only the request side of a
    /// bus-level `tlm_method` call. A later `join_all` drains the response
    /// side and assigns any captured return values.
    fn try_emit_bus_tlm_fork(&mut self, e: &Expr, let_name: Option<&str>, depth: usize) -> bool {
        let ExprKind::ForkCall { call } = &*e.kind else {
            return false;
        };
        let ExprKind::Call { callee, args } = &*call.kind else {
            self.errors
                .push("`fork` RHS currently requires a direct bus tlm_method call".into());
            return true;
        };
        let ExprKind::Field {
            target,
            name: method_name,
        } = &*callee.kind
        else {
            self.errors
                .push("`fork` RHS currently requires a direct bus tlm_method call".into());
            return true;
        };
        let ExprKind::Ident(id) = &*target.kind else {
            self.errors
                .push("`fork` RHS currently requires `bus.method(args)`".into());
            return true;
        };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else {
            self.errors.push(format!(
                "`fork {}.{}(...)`: `{}` is not a known bus binding",
                id.name, method_name.name, id.name
            ));
            return true;
        };
        let Some(method) = bus
            .tlm_methods
            .iter()
            .find(|m| m.name.name == method_name.name)
            .cloned()
        else {
            self.errors.push(format!(
                "bus `{}` has no tlm_method `{}`",
                bus.name.name, method_name.name
            ));
            return true;
        };
        if args.len() != method.args.len() {
            self.errors.push(format!(
                "bus.{}: expected {} arg(s), got {}",
                method.name.name,
                method.args.len(),
                args.len(),
            ));
            return true;
        }

        let ret_type = method.ret.as_ref().map(|t| self.record_field_c_type(t));
        let component = self.trace_component_context();
        if let Some(name) = let_name {
            let cty = ret_type.clone().unwrap_or_else(|| "uint64_t".into());
            self.pad(depth);
            writeln!(self.out, "{cty} {name} = {{}};").ok();
        } else if method.ret.is_some() {
            self.pad(depth);
            writeln!(
                self.out,
                "// fork bus.{} result intentionally discarded",
                method.name.name
            )
            .ok();
        }

        let req_valid = self.bus_signal_name(&sig_prefix, &method.name.name, "req_valid");
        let req_ready = self.bus_signal_name(&sig_prefix, &method.name.name, "req_ready");
        let req_tag = self.bus_signal_name(&sig_prefix, &method.name.name, "req_tag");
        let tag = if method.mode.name == "out_of_order" {
            let key = (sig_prefix.clone(), method.name.name.clone());
            let next = self.next_tlm_fork_tag.entry(key).or_insert(0);
            let tag = *next;
            *next += 1;
            Some(tag)
        } else if method.mode.name == "blocking" {
            None
        } else {
            self.errors.push(format!(
                "bus.{}: HARC RHS-fork lowering supports `blocking` and `out_of_order tags N`, not `{}`",
                method.name.name, method.mode.name
            ));
            return true;
        };

        self.pad(depth);
        writeln!(
            self.out,
            "// fork bus.{} tlm_method issue",
            method.name.name
        )
        .ok();
        for ((arg_name, _), arg) in method.args.iter().zip(args.iter()) {
            let sig_port = self.bus_signal_name(&sig_prefix, &method.name.name, &arg_name.name);
            self.pad(depth);
            write!(self.out, "harc_rt::harc_assign({root}->{sig_port}, ").ok();
            match arg {
                CallArg::Expr(e) => self.emit_expr(e),
                CallArg::Named { value, .. } => self.emit_expr(value),
            }
            writeln!(self.out, ");").ok();
        }
        if let Some(tag) = tag {
            self.pad(depth);
            writeln!(self.out, "{root}->{req_tag} = {tag};").ok();
        }
        let req_tag_expr = tag.map(|_| format!("{root}->{req_tag}"));
        self.emit_tlm_call_trace_event(
            &component,
            &id.name,
            &method.name.name,
            "request",
            "initiator",
            req_tag_expr.as_deref(),
            depth,
        );
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 1;").ok();
        self.pad(depth);
        if tag.is_some() {
            // Present the request (valid + tag + payload) for exactly the
            // acceptance cycle, then deassert. An out-of-order target accepts
            // combinationally — `req_ready` is high while the addressed tag's
            // slot is idle and drops the same posedge the request is latched.
            // We must NOT spin on `req_ready` before advancing: at this point
            // the DUT mirror has not re-evaluated with the just-written
            // `req_tag`, so `req_ready` still reflects the *previous* tag
            // (whose slot may now be busy) — a stale 0 that would send the
            // initiator into a multi-cycle hold, leaving `req_valid` asserted
            // through a `valid && !ready` window and dropping it while the
            // slot is busy. That trips the DUT's `_auto_tlm_*_req_stable`
            // handshake assertion (a real protocol violation, not a false
            // positive). Advancing exactly one cycle spans the accept edge
            // with the new tag/payload held stable; deasserting the next
            // cycle keeps the request out of any stalled window. (The
            // response is collected later at join_all.)
            if self.in_coroutine {
                writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            } else {
                writeln!(self.out, "tick();").ok();
            }
        } else {
            // Blocking forks still need the legacy ready-wait: unlike tagged
            // OOO lanes, a blocking target may legitimately hold req_ready low
            // for multiple cycles before accepting the request.
            let advance = if self.in_coroutine {
                "co_await harc_rt::wait_cycles(_slot, 1)"
            } else {
                "tick()"
            };
            // `self.pad(depth)` already emitted above (shared with the OOO
            // branch), so the loop line is not re-padded.
            writeln!(
                self.out,
                "{}",
                crate::codegen::bounded_handshake_wait(
                    &format!("{root}->{req_ready}"),
                    crate::codegen::TLM_WAIT_BOUND,
                    advance,
                    &format!("TLM {}.{} fork request", id.name, method.name.name),
                )
            )
            .ok();
            self.pad(depth);
            writeln!(self.out, "{advance};").ok();
        }
        self.pad(depth);
        writeln!(self.out, "{root}->{req_valid} = 0;").ok();

        self.pending_tlm_forks.push(PendingTlmFork {
            root,
            component,
            bus: id.name.clone(),
            method: method.name.name,
            sig_prefix,
            ret_var: let_name.map(|s| s.to_string()),
            ret_ty: method.ret.clone(),
            tag,
        });
        true
    }

    fn emit_tlm_join_all(&mut self, depth: usize) {
        if self.pending_tlm_forks.is_empty() {
            self.pad(depth);
            writeln!(self.out, "// join_all: no pending forked TLM calls").ok();
            return;
        }
        let pending = std::mem::take(&mut self.pending_tlm_forks);
        let tagged = pending.iter().any(|p| p.tag.is_some());
        if tagged {
            self.emit_tagged_tlm_join_all(&pending, depth);
        } else {
            self.emit_ordered_tlm_join_all(&pending, depth);
        }
    }

    fn emit_ordered_tlm_join_all(&mut self, pending: &[PendingTlmFork], depth: usize) {
        for p in pending {
            let rsp_valid = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_valid");
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            let rsp_data = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_data");
            self.pad(depth);
            writeln!(self.out, "// join_all bus.{} response", p.method).ok();
            self.pad(depth);
            writeln!(self.out, "{}->{} = 1;", p.root, rsp_ready).ok();
            self.pad(depth);
            writeln!(self.out, "{{").ok();
            self.pad(depth + 1);
            writeln!(self.out, "bool _rsp_ok = true;").ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "{}",
                crate::codegen::bounded_handshake_wait_into(
                    &format!("{}->{}", p.root, rsp_valid),
                    crate::codegen::TLM_JOIN_DRAIN_BOUND,
                    if self.in_coroutine {
                        "co_await harc_rt::wait_cycles(_slot, 1)"
                    } else {
                        "tick()"
                    },
                    &format!("TLM {}.{} fork response", p.bus, p.method),
                    "_rsp_ok",
                )
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "if (_rsp_ok) {{").ok();
            if let Some(var) = &p.ret_var {
                let read_expr = p
                    .ret_ty
                    .as_ref()
                    .map(|ty| self.record_unpack_expr(ty, &format!("{}->{}", p.root, rsp_data)))
                    .unwrap_or_else(|| format!("harc_rt::harc_read({}->{})", p.root, rsp_data));
                self.pad(depth + 2);
                writeln!(self.out, "{var} = {read_expr};").ok();
            }
            self.emit_tlm_call_trace_event(
                &p.component,
                &p.bus,
                &p.method,
                "response",
                "initiator",
                None,
                depth + 2,
            );
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            writeln!(self.out, "}}").ok();
            self.pad(depth);
            if self.in_coroutine {
                writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
            } else {
                writeln!(self.out, "tick();").ok();
            }
            self.pad(depth);
            writeln!(self.out, "{}->{} = 0;", p.root, rsp_ready).ok();
        }
    }

    fn emit_tagged_tlm_join_all(&mut self, pending: &[PendingTlmFork], depth: usize) {
        self.pad(depth);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "int _tlm_pending = {};", pending.len()).ok();
        for (idx, _) in pending.iter().enumerate() {
            self.pad(depth + 1);
            writeln!(self.out, "bool _tlm_seen_{idx} = false;").ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "int _tlm_budget = 256;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "while (_tlm_pending > 0 && _tlm_budget > 0) {{").ok();
        for p in pending {
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            self.pad(depth + 2);
            writeln!(self.out, "{}->{} = 0;", p.root, rsp_ready).ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "bool _tlm_accept = false;").ok();
        for (idx, p) in pending.iter().enumerate() {
            let Some(tag) = p.tag else {
                self.errors.push(
                    "cannot mix tagged and untagged RHS-fork TLM calls before one join_all".into(),
                );
                continue;
            };
            let rsp_valid = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_valid");
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            let rsp_data = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_data");
            let rsp_tag = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_tag");
            self.pad(depth + 2);
            writeln!(
                self.out,
                "if (!_tlm_seen_{idx} && {root}->{rsp_valid} && {root}->{rsp_tag} == {tag}) {{",
                root = p.root
            )
            .ok();
            if let Some(var) = &p.ret_var {
                let read_expr = p
                    .ret_ty
                    .as_ref()
                    .map(|ty| self.record_unpack_expr(ty, &format!("{}->{}", p.root, rsp_data)))
                    .unwrap_or_else(|| format!("harc_rt::harc_read({}->{})", p.root, rsp_data));
                self.pad(depth + 3);
                writeln!(self.out, "{var} = {read_expr};").ok();
            }
            let tag_expr = tag.to_string();
            self.emit_tlm_call_trace_event(
                &p.component,
                &p.bus,
                &p.method,
                "response",
                "initiator",
                Some(&tag_expr),
                depth + 3,
            );
            self.pad(depth + 3);
            writeln!(self.out, "{}->{} = 1;", p.root, rsp_ready).ok();
            self.pad(depth + 3);
            writeln!(self.out, "_tlm_seen_{idx} = true;").ok();
            self.pad(depth + 3);
            writeln!(self.out, "_tlm_pending--;").ok();
            self.pad(depth + 3);
            writeln!(self.out, "_tlm_accept = true;").ok();
            self.pad(depth + 2);
            writeln!(self.out, "}}").ok();
        }
        self.pad(depth + 2);
        if self.in_coroutine {
            writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
        } else {
            writeln!(self.out, "tick();").ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "if (!_tlm_accept) _tlm_budget--;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        for p in pending {
            let rsp_ready = self.bus_signal_name(&p.sig_prefix, &p.method, "rsp_ready");
            self.pad(depth + 1);
            writeln!(self.out, "{}->{} = 0;", p.root, rsp_ready).ok();
        }
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Detect `<bus>.<channel>.send(args)` / `<bus>.<channel>.recv()`
    /// calls and emit the auto-generated valid/ready handshake. The
    /// channel must be a `handshake_channel` on the bus the binding
    /// names; the call's arity (for `send`) must match the channel's
    /// payload signal count.
    ///
    /// `let_name = Some("x")` for the let-rhs form (`let x = bus.r.recv()`):
    /// the captured payload is assigned into `auto x = ...;`. With
    /// `None` the call is a discarded statement (drives the dance,
    /// throws away any received data).
    ///
    /// Returns `true` when the call was a recognized bus handshake
    /// and was emitted; `false` to fall through to plain Call lowering.
    fn try_emit_bus_handshake(&mut self, e: &Expr, let_name: Option<&str>, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return false;
        };
        let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        else {
            return false;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return false;
        };
        let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() else {
            return false;
        };
        let Some(h) = bus
            .handshakes
            .iter()
            .find(|h| h.name.name == ch.name)
            .cloned()
        else {
            return false;
        };
        let valid_port = self.bus_signal_name(&sig_prefix, &ch.name, "valid");
        let ready_port = self.bus_signal_name(&sig_prefix, &ch.name, "ready");

        match method.name.as_str() {
            "send" => {
                if args.len() != h.payload.len() {
                    self.errors.push(format!(
                        "bus.{}.send: expected {} payload arg(s), got {}",
                        ch.name,
                        h.payload.len(),
                        args.len(),
                    ));
                    return true;
                }
                if let_name.is_some() {
                    self.errors.push(format!(
                        "bus.{}.send returns no value; use it as a statement",
                        ch.name,
                    ));
                    return true;
                }
                self.pad(depth);
                writeln!(self.out, "// bus.{}.send", ch.name).ok();
                // Drive payload signals.
                for (sig, arg) in h.payload.iter().zip(args.iter()) {
                    let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &sig.name.name);
                    self.pad(depth);
                    write!(self.out, "{root}->{sig_port} = ").ok();
                    match arg {
                        CallArg::Expr(e) => self.emit_expr(e),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                    writeln!(self.out, ";").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{valid_port} = 1;").ok();
                // Wait until ready=1 (bounded). The bound matches the sync
                // 16-cycle budget so a stuck DUT terminates the test (now
                // with a FAIL diagnostic) rather than hanging.
                let advance = if self.in_coroutine {
                    "co_await harc_rt::wait_cycles(_slot, 1)"
                } else {
                    "tick()"
                };
                self.pad(depth);
                writeln!(
                    self.out,
                    "{}",
                    crate::codegen::bounded_handshake_wait(
                        &format!("{root}->{ready_port}"),
                        crate::codegen::TLM_WAIT_BOUND,
                        advance,
                        &format!("handshake {}.send ready", ch.name),
                    )
                )
                .ok();
                self.pad(depth);
                writeln!(self.out, "{advance};").ok();
                self.pad(depth);
                writeln!(self.out, "{root}->{valid_port} = 0;").ok();
                // Activity tracking (spec §7.x): a completed
                // bus.<ch>.send counts as an "out" for the surrounding
                // component instance, if any. Bus calls inside
                // free/test-run code (no enclosing component) skip the
                // bump.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
                }
                true
            }
            "recv" => {
                if !args.is_empty() {
                    self.errors.push(format!(
                        "bus.{}.recv: expected 0 args, got {}",
                        ch.name,
                        args.len(),
                    ));
                    return true;
                }
                if h.payload.is_empty() {
                    self.errors.push(format!(
                        "bus.{}.recv: channel has no payload signals to receive",
                        ch.name,
                    ));
                    return true;
                }
                // Multi-payload receive captures the full payload into
                // a per-channel struct (`<BusName>_<chan>_payload`),
                // emitted at file scope. The struct exposes an implicit
                // conversion to the first payload field's C++ type so
                // pre-existing scalar usage (`assert val == 0xCAFE`,
                // `field = val`) keeps compiling without change. Users
                // who want named field access write `val.data`,
                // `val.resp`, etc.
                self.pad(depth);
                writeln!(self.out, "// bus.{}.recv", ch.name).ok();
                self.pad(depth);
                writeln!(self.out, "{root}->{ready_port} = 1;").ok();
                self.pad(depth);
                writeln!(
                    self.out,
                    "{}",
                    crate::codegen::bounded_handshake_wait(
                        &format!("{root}->{valid_port}"),
                        crate::codegen::TLM_WAIT_BOUND,
                        if self.in_coroutine {
                            "co_await harc_rt::wait_cycles(_slot, 1)"
                        } else {
                            "tick()"
                        },
                        &format!("handshake {}.recv valid", ch.name),
                    )
                )
                .ok();
                // Capture BEFORE the trailing tick: the destination signals
                // are valid in the same cycle as `valid` is high.
                if let Some(name) = let_name {
                    self.pad(depth);
                    let struct_name = format!("{}_{}_payload", bus.name.name, ch.name);
                    write!(self.out, "{struct_name} {name} = {{").ok();
                    let mut first = true;
                    for sig in &h.payload {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &sig.name.name);
                        write!(self.out, "{root}->{sig_port}").ok();
                    }
                    writeln!(self.out, "}};").ok();
                }
                self.pad(depth);
                if self.in_coroutine {
                    writeln!(self.out, "co_await harc_rt::wait_cycles(_slot, 1);").ok();
                } else {
                    writeln!(self.out, "tick();").ok();
                }
                self.pad(depth);
                writeln!(self.out, "{root}->{ready_port} = 0;").ok();
                // Activity tracking (spec §7.x): a completed
                // bus.<ch>.recv counts as an "in" for the surrounding
                // component instance, if any.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_in_cycle = (uint64_t)cycle_count;").ok();
                }
                true
            }
            _ => false,
        }
    }

    /// Intrinsic width-method lowering: `.trunc<N>()`, `.zext<N>()`,
    /// `.sext<N>()`, `.resize<N>()`. Ported from arch-com's
    /// `cpp_method_call` (src/sim_codegen/mod.rs:2688). The parser
    /// emits these as `Call { callee: Field{recv, name},
    /// args: [width_const] }`. We dispatch on `name`, evaluate the
    /// width as a compile-time constant, and emit the C++ narrow or
    /// extend. Returns `true` when the call matched and was emitted;
    /// `false` otherwise (caller falls through to normal call
    /// emission).
    ///
    /// Behavior summary (matches arch-com's semantics):
    /// - `.trunc<N>()`  — narrow to N bits: `((uintN_t)((expr) & MASK))`
    ///                    where MASK = (1 << N) - 1. Errors if N >=
    ///                    source width when source width is known
    ///                    (would widen, wrong direction).
    /// - `.zext<N>()`   — zero-extend to N bits: `((uintN_t)(expr))`.
    /// - `.sext<N>()`   — sign-extend to N bits: shift-fill trick.
    /// - `.resize<N>()` — direction-agnostic: narrow if N < source
    ///                    width, zero-extend otherwise.
    fn try_emit_width_method(&mut self, callee: &Expr, args: &[CallArg]) -> bool {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return false;
        };
        let kind = match name.name.as_str() {
            "trunc" => "trunc",
            "zext" => "zext",
            "sext" => "sext",
            "resize" => "resize",
            _ => return false,
        };
        let width_expr = match args.first() {
            Some(CallArg::Expr(e)) => e,
            _ => {
                self.errors
                    .push(format!("`.{kind}<N>()` requires a constant width argument",));
                return true;
            }
        };
        let width = match eval_const_width(width_expr) {
            Some(w) => w,
            None => {
                self.errors.push(format!(
                    "`.{kind}<N>()` requires a constant integer width \
                     (saw a non-const expression); span at parse-time \
                     should pin the offending token",
                ));
                return true;
            }
        };
        if width == 0 {
            self.errors.push(format!(
                "`.{kind}<{width}>()`: width must be greater than zero",
            ));
            return true;
        }
        if width > crate::MAX_WIDTH_METHOD_BITS {
            self.errors.push(format!(
                "`.{kind}<{width}>()`: destination width exceeds the {}-bit language limit",
                crate::MAX_WIDTH_METHOD_BITS
            ));
            return true;
        }
        // Best-effort source-width inference for the type-direction check.
        // We can detect width when the receiver is a literal-typed `let`
        // or an explicit `as uint<W>` / `as sint<W>` cast. Otherwise the
        // width is unknown and we skip the type-direction check.
        let source_width = self.infer_expr_width_best_effort(target);
        if let Some(sw) = source_width {
            match kind {
                "trunc" if width >= sw => {
                    self.errors.push(format!(
                        "`.trunc<{width}>()` on a {sw}-bit value: width \
                         must be strictly less than the source width \
                         (otherwise it's a no-op or wrong-direction). \
                         Use `.zext<{width}>()` to widen, or remove the \
                         cast if you meant a no-op.",
                    ));
                    return true;
                }
                "zext" | "sext" if width < sw => {
                    self.errors.push(format!(
                        "`.{kind}<{width}>()` on a {sw}-bit value: width \
                         must be ≥ the source width (otherwise it \
                         narrows, wrong direction). Use `.trunc<{width}>()` \
                         to narrow.",
                    ));
                    return true;
                }
                _ => {}
            }
        }
        // The direction checks above want the raw inferred width; a zero
        // width is not usable emission metadata (it would reach the sext
        // shift-fill as `64 - 0`, UB), so drop to the unknown-source
        // shapes. Matches TB-IR's `lower_width_method`.
        let source_width = source_width.filter(|w| *w > 0);
        // Emit. Pattern follows arch-com's cast_to_bits / shift-fill
        // for sext. Sub-64 narrowing (`trunc`, `resize` down) narrows to
        // `uint64_t` *before* the mask: a `HarcWide<N>` receiver converts
        // implicitly to both `uint64_t` and `_harc_u128`, so masking it
        // directly is an ambiguous `operator&`. Narrowing first is
        // value-identical for the `uint64_t` / `_harc_u128` receivers,
        // since `width < 64` keeps the kept bits inside the low word.
        let c_unsigned = cpp_uint_for_width(Some(width));
        // `sext` with nothing to extend (source already ≥ the destination,
        // or unknown) is still a *signed* relabel: the result feeds signed
        // comparison, division, and shifts. Casting it through `uint64_t`
        // silently made `s.sext<64>() > 0` true for a negative `sint<64>`
        // — TB-IR emits `((int64_t)(...))` here and the two backends
        // returned opposite verdicts for the same source.
        let c_sext_plain = if width <= 64 {
            "int64_t".to_string()
        } else {
            c_unsigned.clone()
        };
        // Narrow to `uint64_t` before that signed relabel: a `HarcWide<N>`
        // receiver converts implicitly to both `uint64_t` and `_harc_u128`,
        // so a bare `(int64_t)` on one is an ambiguous conversion. The
        // scalar receivers reinterpret the same low 64 bits either way.
        let (sext_narrow, sext_narrow_close) = if width <= 64 {
            ("(uint64_t)(", ")")
        } else {
            ("", "")
        };
        match kind {
            "trunc" => {
                if width > 128 {
                    let words = width.div_ceil(32);
                    write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                    self.emit_expr(target);
                    write!(self.out, ", {width})").ok();
                } else if width > 64 {
                    write!(self.out, "harc_rt::harc_trunc_u128((_harc_u128)(").ok();
                    self.emit_expr(target);
                    write!(self.out, "), {width})").ok();
                } else if width == 64 {
                    write!(self.out, "(({c_unsigned})(").ok();
                    self.emit_expr(target);
                    write!(self.out, "))").ok();
                } else {
                    let mask = (1u64 << width) - 1;
                    write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
                    self.emit_expr(target);
                    write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                }
            }
            "zext" => {
                // Mask to the source width before widening so any high bits
                // in the underlying int64_t storage (from a prior no-op cast)
                // are dropped. When source width is unknown we conservatively
                // assume the receiver already fits in N bits — same shape
                // as arch-com.
                if width > 128 {
                    let words = width.div_ceil(32);
                    write!(self.out, "harc_rt::harc_wide_zext<{words}>(").ok();
                    self.emit_expr(target);
                    if let Some(sw) = source_width {
                        write!(self.out, ", {sw}").ok();
                    }
                    write!(self.out, ")").ok();
                } else {
                    write!(self.out, "(({c_unsigned})(").ok();
                    self.emit_expr(target);
                    write!(self.out, "))").ok();
                }
            }
            "sext" => {
                // Sign-extend from source width to dest width. Strategy:
                // shift source-width MSB into bit 63 of an int64_t, then
                // arithmetic-shift right back (the standard idiom).
                // The result is masked to the dest width via the C++
                // cast `((cpp_uint_for_width(width))<expr>)`. When source
                // width is unknown, fall back to a plain widening cast
                // — the receiver almost certainly already has the right
                // sign-bit pattern in its underlying storage.
                if let Some(sw) = source_width {
                    if sw < width {
                        if width > 64 {
                            if width > 128 {
                                let words = width.div_ceil(32);
                                write!(self.out, "harc_rt::harc_wide_sext<{words}>(").ok();
                                self.emit_expr(target);
                                write!(self.out, ", {sw}, {width})").ok();
                            } else {
                                write!(self.out, "harc_rt::harc_sext_u128((_harc_u128)(").ok();
                                self.emit_expr(target);
                                write!(self.out, "), {sw}, {width})").ok();
                            }
                        } else {
                            let shift = 64 - sw;
                            if width == 64 {
                                // Want full 64-bit signed-extended view.
                                // `int64_t`, not `uint64_t`: this binds to
                                // `auto` at the use site, and deducing
                                // unsigned here made `p[7:0].sext<64>() > 0`
                                // true under v1 and false under TB-IR, whose
                                // local for a sext is `int64_t`.
                                write!(self.out, "((int64_t)(((int64_t)((uint64_t)(").ok();
                                self.emit_expr(target);
                                write!(self.out, ") << {shift})) >> {shift}))").ok();
                            } else {
                                let mask = (1u64 << width) - 1;
                                write!(self.out, "(({c_unsigned})(((int64_t)((uint64_t)(").ok();
                                self.emit_expr(target);
                                write!(self.out, ") << {shift})) >> {shift}) & 0x{mask:X}ULL)")
                                    .ok();
                            }
                        }
                    } else {
                        if width > 128 {
                            let words = width.div_ceil(32);
                            write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                            self.emit_expr(target);
                            write!(self.out, ", {sw})").ok();
                        } else {
                            write!(self.out, "(({c_sext_plain})({sext_narrow}").ok();
                            self.emit_expr(target);
                            write!(self.out, "){sext_narrow_close})").ok();
                        }
                    }
                } else {
                    if width > 128 {
                        let words = width.div_ceil(32);
                        write!(self.out, "harc_rt::harc_wide_zext<{words}>(").ok();
                        self.emit_expr(target);
                        write!(self.out, ")").ok();
                    } else {
                        write!(self.out, "(({c_sext_plain})({sext_narrow}").ok();
                        self.emit_expr(target);
                        write!(self.out, "){sext_narrow_close})").ok();
                    }
                }
            }
            "resize" => {
                // Direction-agnostic: narrow with mask when narrowing,
                // plain cast otherwise.
                if let Some(sw) = source_width {
                    if width < sw {
                        // Narrowing — mask + cast.
                        if width > 128 {
                            let words = width.div_ceil(32);
                            write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                            self.emit_expr(target);
                            write!(self.out, ", {width})").ok();
                        } else if width > 64 {
                            write!(self.out, "harc_rt::harc_trunc_u128((_harc_u128)(").ok();
                            self.emit_expr(target);
                            write!(self.out, "), {width})").ok();
                        } else if width == 64 {
                            write!(self.out, "(({c_unsigned})(").ok();
                            self.emit_expr(target);
                            write!(self.out, "))").ok();
                        } else {
                            let mask = (1u64 << width) - 1;
                            write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
                            self.emit_expr(target);
                            write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                        }
                    } else {
                        // Widening or same width — plain cast.
                        if width > 128 {
                            let words = width.div_ceil(32);
                            write!(self.out, "harc_rt::harc_wide_zext<{words}>(").ok();
                            self.emit_expr(target);
                            write!(self.out, ", {sw})").ok();
                        } else {
                            write!(self.out, "(({c_unsigned})(").ok();
                            self.emit_expr(target);
                            write!(self.out, "))").ok();
                        }
                    }
                } else {
                    // Unknown source width — default to mask-narrow,
                    // since `.resize<N>()` with `N <= 64` always wants
                    // a value bounded to N bits regardless of source.
                    if width > 128 {
                        let words = width.div_ceil(32);
                        write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                        self.emit_expr(target);
                        write!(self.out, ", {width})").ok();
                    } else if width > 64 {
                        write!(self.out, "harc_rt::harc_trunc_u128((_harc_u128)(").ok();
                        self.emit_expr(target);
                        write!(self.out, "), {width})").ok();
                    } else if width == 64 {
                        write!(self.out, "(({c_unsigned})(").ok();
                        self.emit_expr(target);
                        write!(self.out, "))").ok();
                    } else {
                        let mask = (1u64 << width) - 1;
                        write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
                        self.emit_expr(target);
                        write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                    }
                }
            }
            _ => unreachable!(),
        }
        true
    }

    /// Best-effort source-width inference for the width-method type
    /// checks. Returns the width in bits when it can be statically
    /// determined; `None` otherwise (callers skip the type-direction
    /// check in that case). Recognized shapes:
    ///   - Parenthesized expression → recurse into inner.
    ///   - `<expr> as uint<W>` / `<expr> as sint<W>` → W.
    ///   - `<expr>[hi:lo]` → `hi - lo + 1` for constant bounds.
    ///   - `<expr>.trunc<W>()` / `.zext<W>()` / `.sext<W>()` / `.resize<W>()`
    ///     → W (the prior width-method's target width).
    ///   - Bare integer literal → minimum unsigned bit-width of the
    ///     literal value (cheap heuristic: `64 - leading_zeros(v)` for
    ///     positive values; `None` for negatives).
    ///
    /// The result can be `Some(0)` for a `uint<0>` receiver. Callers
    /// emitting a cast shape must filter that out (it would reach the
    /// sext shift-fill as `64 - 0`, UB); the direction checks want the
    /// raw value.
    ///
    /// Mirrors TB-IR's `infer_expr_width` on TB-IR's whole reachable
    /// domain, but NOT identically for casts: TB-IR caps a cast relabel
    /// at 128 bits, and this returns the raw declared width above that.
    /// The two agree in practice only because TB-IR rejects every cast
    /// outside 1..=128 before inference sees it. Do not "re-unify" them
    /// by delegating this arm — that is what turned
    /// `(big as uint<200>).sext<300>()` into a zero-extension.
    fn infer_expr_width_best_effort(&self, e: &Expr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.infer_expr_width_best_effort(inner),
            // RAW width, deliberately not `cast_relabel_width`: that
            // helper caps at 128 and rejects 0, which is right for a wrap
            // operand (see `wrap_operand_width`) but wrong here. The
            // direction checks and the `sext` emission shape need the
            // declared width even above 128 — losing it turned
            // `(big as uint<200>).sext<300>()` into a zero-extension,
            // silently, and dropped the wrong-direction rejection.
            //
            // The arms below still mirror `cast_relabel_width`'s *shape*
            // rules: the capitalised ARCH spellings count, an absent
            // width argument is 64 bits, and a width argument that is not
            // an integer literal is unknown rather than 64.
            ExprKind::Cast { ty, .. } => match ty {
                TypeExpr::Builtin {
                    name:
                        BuiltinTy::UInt
                        | BuiltinTy::UIntCap
                        | BuiltinTy::SInt
                        | BuiltinTy::SIntCap
                        | BuiltinTy::Bits,
                    args,
                    ..
                } => match args.first() {
                    None => Some(64),
                    Some(TypeArg::Expr(e)) => match &*e.kind {
                        ExprKind::Int(s) => s.replace('_', "").parse().ok(),
                        _ => None,
                    },
                    Some(_) => None,
                },
                _ => None,
            },
            ExprKind::BitSlice { hi, lo, .. } => {
                let hi = eval_const_width(hi)?;
                let lo = eval_const_width(lo)?;
                (hi >= lo).then_some(hi - lo + 1)
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { name, .. } = &*callee.kind {
                    if Emitter::is_width_method_name(&name.name) {
                        if let Some(CallArg::Expr(w)) = args.first() {
                            return eval_const_width(w);
                        }
                    }
                }
                None
            }
            ExprKind::Int(s) => {
                // Parse the literal text to a u64 and use bit-width.
                let stripped = s.replace('_', "");
                let v: Option<u64> = if let Some(rest) = stripped
                    .strip_prefix("0x")
                    .or_else(|| stripped.strip_prefix("0X"))
                {
                    u64::from_str_radix(rest, 16).ok()
                } else if let Some(rest) = stripped
                    .strip_prefix("0b")
                    .or_else(|| stripped.strip_prefix("0B"))
                {
                    u64::from_str_radix(rest, 2).ok()
                } else {
                    stripped.parse::<u64>().ok()
                };
                v.map(|v| if v == 0 { 1 } else { 64 - v.leading_zeros() })
            }
            ExprKind::Ident(id) => self.let_widths.get(&id.name).copied(),
            _ => None,
        }
    }

    fn is_width_method_name(name: &str) -> bool {
        matches!(name, "trunc" | "zext" | "sext" | "resize")
    }

    /// A wrap's residue is unsigned (spec §2.4), so binding one to a
    /// signed local is a signedness mismatch — TB-IR rejects it, v1
    /// accepted it silently. Unwraps parentheses: `(a +% b)` is the same
    /// wrap, and not seeing through them left the divergence reachable.
    fn check_signed_wrap_destination(&mut self, name: &str, dw: u32, value: &Expr) {
        fn unwrap_parens(v: &Expr) -> &Expr {
            match &*v.kind {
                ExprKind::Paren(inner) => unwrap_parens(inner),
                _ => v,
            }
        }
        fn is_wrap(v: &Expr) -> bool {
            match &*v.kind {
                ExprKind::Paren(inner) => is_wrap(inner),
                ExprKind::Binary { op, .. } => matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ),
                _ => false,
            }
        }
        if is_wrap(value) {
            // Report the VALUE's width, as TB-IR does — the destination's
            // width is a different number whenever the two disagree.
            // If the wrap itself is invalid (unknown or >64-bit operands)
            // its own error already fires and says more; adding a
            // signedness message with a fabricated width is just noise.
            let Some(aw) = (match &*unwrap_parens(value).kind {
                ExprKind::Binary { op, lhs, rhs } => self.wrap_result_width(*op, lhs, rhs).ok(),
                _ => None,
            }) else {
                return;
            };
            self.errors.push(format!(
                "assignment of an unsigned {aw}-bit value to `{name}`, declared \
                 signed {dw} bits. Signedness must match — relabel the value \
                 explicitly with `as sint<{dw}>`."
            ));
        }
    }

    /// Reject a narrowing scalar assignment into a declared-width local.
    /// TB-IR's `check_scalar_assign_width` is the counterpart; v1 used to
    /// emit `HarcWide<7> b = a;` for a 256-bit `a` (which does not
    /// compile) and silently accept the ≤64-bit cases.
    fn check_scalar_assign_width(&mut self, name: &str, value: &Expr) {
        let Some(dw) = self.let_widths.get(name).copied() else {
            return;
        };
        if dw == 0 {
            return;
        }
        let Some(aw) = narrowing_source_width(self, value) else {
            return;
        };
        if aw > dw {
            self.errors.push(format!(
                "assignment of a {aw}-bit value to `{name}`, declared {dw} bits, \
                 narrows. Widths must not shrink implicitly — use `.trunc<{dw}>()` \
                 to narrow explicitly, or widen the declaration to {aw} bits."
            ));
        }
    }

    /// Mask width for a wrapping operator: `max(W(lhs), W(rhs))`, the
    /// wider operand's width with no widening (harc#473). Mirrors TB-IR's
    /// `wrap_to_operand_width` / `infer_wrap_operand_width`, including its
    /// two rejections, so both backends accept and reject the same
    /// programs. `Err` carries the user-facing diagnostic.
    fn wrap_result_width(&self, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Result<u32, String> {
        let sym = match op {
            BinaryOp::SubWrap => "-%",
            BinaryOp::MulWrap => "*%",
            _ => "+%",
        };
        let wl = self.wrap_operand_width(lhs);
        let wr = self.wrap_operand_width(rhs);
        let (Some(wl), Some(wr)) = (wl, wr) else {
            return Err(format!(
                "wrapping operator `{sym}` needs both operands to have a statically \
                 known bit-width so the wrap width `max(W(lhs), W(rhs))` is defined \
                 (left is {}, right is {}). Give the operand(s) a scalar type \
                 (`let x : uint<N>`), a cast (`x as uint<N>`), or a width method.",
                if wl.is_some() { "known" } else { "unknown" },
                if wr.is_some() { "known" } else { "unknown" },
            ));
        };
        let width = wl.max(wr);
        if width > 64 {
            return Err(format!(
                "wrapping operator `{sym}` at width {width} (> 64 bits) is not \
                 supported: wrapping arithmetic is lowered for operand widths up \
                 to 64 bits; wider datapaths need the `HarcWide<N>` model, which \
                 is not wired through the wrapping mask yet"
            ));
        }
        Ok(width)
    }

    /// Operand bit-width for a wrapping-op mask: the receiver-width
    /// inference, plus composition through a nested wrap so a chain
    /// (`(a +% b) *% c`) masks at each step's own operand width.
    fn wrap_operand_width(&self, e: &Expr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.wrap_operand_width(inner),
            ExprKind::Binary {
                op: BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap,
                lhs,
                rhs,
            } => {
                let l = self.wrap_operand_width(lhs)?;
                let r = self.wrap_operand_width(rhs)?;
                Some(l.max(r))
            }
            // A cast operand uses TB-IR's own `cast_relabel_width`, which
            // caps at 128 and rejects 0 — TB-IR's wrap inference routes
            // through the same helper, so sharing it keeps the two
            // backends' accepted operand sets aligned by construction.
            // This is the one consumer that wants the capped rule; the
            // direction checks and emission shapes read the raw declared
            // width from `infer_expr_width_best_effort` instead.
            ExprKind::Cast { ty, .. } => crate::ir::lower::cast_relabel_width(ty),
            // `dut.<probe>` carries the probe's declared width — the same
            // width TB-IR reads off `PortRef::width`. A plain top-level
            // port is width-erased and still reports `None` in both.
            //
            // The base MUST be checked, not just the field name: this map
            // is keyed by bare probe name, so an unguarded lookup masked a
            // same-named record field (`t.count +% 10` on a `uint<32>`
            // field, with a `probe count : uint<8>` in scope) at the
            // probe's width — a silently wrong value. Mirrors the
            // read-only-probe guard on the write path.
            ExprKind::Field { target, name } => match &*target.kind {
                ExprKind::Ident(id) if id.name == "dut" => {
                    self.probe_widths.get(&name.name).copied()
                }
                _ => None,
            },
            _ => self.infer_expr_width_best_effort(e),
        }
    }

    /// `((uint64_t)(((uint64_t)((lhs OP rhs)) & 0xMASK)))` — the same
    /// narrow-to-`width` shape the `.trunc<N>()` intrinsic emits, so the
    /// two spellings of a wrap are byte-identical, and identical to what
    /// TB-IR emits for the `WidthCast::Trunc` its lowering inserts.
    fn emit_wrapping_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, width: u32) {
        let c_unsigned = cpp_uint_for_width(Some(width));
        let s = c_binary_op(op);
        if width == 64 {
            write!(self.out, "(({c_unsigned})(").ok();
        } else {
            write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
        }
        write!(self.out, "(").ok();
        self.emit_expr(lhs);
        write!(self.out, " {s} ").ok();
        self.emit_expr(rhs);
        write!(self.out, ")").ok();
        if width == 64 {
            write!(self.out, "))").ok();
        } else {
            let mask = (1u64 << width) - 1;
            write!(self.out, ") & 0x{mask:X}ULL)))").ok();
        }
    }

    /// Is a plain bus signal present (not gated OFF) at this bind site?
    ///
    /// Looks up the named signal on the bus; if it carries a `generate_if`
    /// gate, evaluates it against the bind's effective param env (falling back
    /// to the bus's param defaults if no explicit env was recorded). Returns
    /// `false` only when the signal exists but its gate is provably false —
    /// i.e. `arch build` would have omitted the port. Unknown signal ⇒ `false`
    /// (let the caller's not-found path fire). Unfoldable gate ⇒ `true`
    /// (conservative: present, never a silent drop).
    fn bus_signal_present(&self, binding_key: &str, bus: &BusDecl, sig_name: &str) -> bool {
        let Some(sig) = bus.signals.iter().find(|s| s.name.name == sig_name) else {
            return false;
        };
        let owned_env;
        let env = match self.bus_param_envs.get(binding_key) {
            Some(e) => e,
            None => {
                owned_env = bus_param_env(bus, None);
                &owned_env
            }
        };
        gate_passes(sig.gate.as_ref(), env)
    }

    fn try_emit_bus_field_access(&mut self, target: &Expr, name: &Ident) -> Option<String> {
        // <binding>.<signal>
        if let ExprKind::Ident(id) = &*target.kind {
            if let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() {
                // Honor the signal's `generate_if` gate: a signal gated OFF
                // under this bind's effective params is absent (the DUT built
                // by `arch build` has no such port), so it falls through to
                // the not-found error below rather than resolving to a phantom
                // port — which would silently diverge from the SV backend.
                if self.bus_signal_present(&id.name, &bus, &name.name) {
                    return Some(format!("{root}->{}_{}", sig_prefix, name.name));
                }
                // Already-flattened `<chan>_<sig>` form (or `<chan>_valid`
                // / `<chan>_ready`).
                for h in &bus.handshakes {
                    let chprefix = format!("{}_", h.name.name);
                    if name.name.starts_with(&chprefix) {
                        let tail = &name.name[chprefix.len()..];
                        if tail == "valid"
                            || tail == "ready"
                            || h.payload.iter().any(|s| s.name.name == tail)
                        {
                            return Some(format!("{root}->{}_{}", sig_prefix, name.name));
                        }
                    }
                }
                // Channel-name itself (used as a leaf in the two-level
                // form `bus.ch.sig`) — defer to the field-access path.
                if bus.handshakes.iter().any(|h| h.name.name == name.name) {
                    return None;
                }
                // Distinguish "gated OFF" from "never declared": a gated-OFF
                // access is a real TB bug (it drives a channel the DUT was
                // built without), so name it precisely.
                if bus.signals.iter().any(|s| s.name.name == name.name) {
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) signal `{}` is gated OFF by its \
                         `generate_if` condition under the bind's params — `arch build` \
                         omits this port, so the testbench must not access it",
                        bus.name.name, id.name, name.name,
                    ));
                } else {
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) has no signal or channel named `{}`",
                        bus.name.name, id.name, name.name,
                    ));
                }
                return Some(format!("/* unresolved: {}.{} */ 0", id.name, name.name));
            }
        }
        // <binding>.<channel>.<signal>
        if let ExprKind::Field {
            target: outer,
            name: ch,
        } = &*target.kind
        {
            if let ExprKind::Ident(id) = &*outer.kind {
                if let Some((bus, root, sig_prefix)) = self.bus_bindings.get(&id.name).cloned() {
                    if let Some(h) = bus.handshakes.iter().find(|h| h.name.name == ch.name) {
                        if name.name == "valid"
                            || name.name == "ready"
                            || h.payload.iter().any(|s| s.name.name == name.name)
                        {
                            let sig_port = self.bus_signal_name(&sig_prefix, &ch.name, &name.name);
                            return Some(format!("{root}->{sig_port}"));
                        }
                        let valid_options: Vec<&str> = std::iter::once("valid")
                            .chain(std::iter::once("ready"))
                            .chain(h.payload.iter().map(|s| s.name.name.as_str()))
                            .collect();
                        self.errors.push(format!(
                            "bus `{}` channel `{}` has no signal `{}` (valid: {})",
                            bus.name.name,
                            ch.name,
                            name.name,
                            valid_options.join(", "),
                        ));
                        return Some(format!(
                            "/* unresolved: {}.{}.{} */ 0",
                            id.name, ch.name, name.name
                        ));
                    }
                    self.errors.push(format!(
                        "bus `{}` (binding `{}`) has no channel `{}`",
                        bus.name.name, id.name, ch.name,
                    ));
                    return Some(format!(
                        "/* unresolved: {}.{}.{} */ 0",
                        id.name, ch.name, name.name
                    ));
                }
            }
        }
        None
    }

    /// If `event` is the trigger of an `on obj.method pre/post`
    /// handler, resolve `obj.method` to a known hookable on a known
    /// component type. Returns `(component_type_name, method_name,
    /// params)`. Walks one or two levels of field-access chain
    /// (covers `<var>.<method>` and `<env>.<sub>.<method>`).
    fn resolve_component_hookable(&self, event: &Expr) -> Option<(String, String, Vec<Param>)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*event.kind
        else {
            return None;
        };
        // Walk the field-access chain, collecting path segments. Same
        // shape as `resolve_component_method_call` — see that function
        // for the rationale (env→agent→transactor.method requires
        // arbitrary-depth chain resolution, not just two levels).
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        // Resolve type at each step.
        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }

        // Find the hookable on cur_ty (component or transactor).
        let find_method = |items: &[ComponentItem], m: &str| -> Option<Vec<Param>> {
            for it in items {
                if let ComponentItem::Hookable(h) = it {
                    if h.is_hookable && h.name.name == m {
                        return Some(h.params.clone());
                    }
                }
            }
            None
        };
        let params = if let Some(comp) = self.components.get(&cur_ty) {
            find_method(&comp.items, &method.name)?
        } else if let Some(t) = self.transactors.get(&cur_ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            find_method(&synth.items, &method.name)?
        } else {
            return None;
        };
        Some((cur_ty, method.name.clone(), params))
    }

    /// If `callee` is a method call on a known component, return the
    /// triple `(<component_type>, <self_expr>, <method>)` so the caller
    /// can lower to `<component_type>_<method>(<self_expr>, args)`.
    /// Returns `None` when the call is not a method-on-component (so the
    /// generic plain-call path runs).
    ///
    /// Handles two shapes:
    /// - `<var>.<method>` — `var` is a let-bound component instance.
    /// - `<env>.<sub>.<method>` — `env` is a let-bound env-style
    ///   component, `<sub>` is one of its sub-component fields, and
    ///   `<method>` is a `hookable` on the sub-component's type.
    /// Detect built-in activity-tracking predicates `idle(N)`,
    /// `idle_in(N)`, `idle_out(N)` on a component-typed binding. Walks
    /// the field-access chain (same shape as `resolve_component_method_call`,
    /// but doesn't require a `hookable` method — these predicates read
    /// the auto-injected `_last_in_cycle` / `_last_out_cycle` fields
    /// instead). Returns `Some((instance_path, predicate_kind))` on
    /// match, where `predicate_kind` is `"idle"`, `"idle_in"`, or
    /// `"idle_out"`.
    ///
    /// **User-defined hookables win.** If the resolved component type
    /// already declares a `hookable <same-name>` method, we return
    /// `None` so the call falls through to the normal hookable-dispatch
    /// path. That keeps pre-existing fixtures (e.g. `buf_mgr_test`,
    /// which has a `hookable idle(n)` that holds bus valids low for
    /// `n` cycles) compiling unchanged. The built-in predicate is
    /// effectively a *default* — users override by declaring the
    /// method themselves.
    fn resolve_component_idle_predicate(&self, callee: &Expr) -> Option<(String, String)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        match method.name.as_str() {
            "idle" | "idle_in" | "idle_out" => {}
            _ => return None,
        }
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }
        // The target must resolve to a known component or transactor
        // type — those are what get the auto-injected heartbeat fields.
        let has_hookable_override = |items: &[ComponentItem]| -> bool {
            items.iter().any(|it| {
                matches!(
                    it, ComponentItem::Hookable(h) if h.name.name == method.name
                )
            })
        };
        let user_overrides = if let Some(comp) = self.components.get(&cur_ty) {
            has_hookable_override(&comp.items)
        } else if let Some(t) = self.transactors.get(&cur_ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            has_hookable_override(&synth.items)
        } else {
            return None;
        };
        if user_overrides {
            // User has a hookable with this name — defer to the
            // normal `Type_method(obj, args)` dispatch path.
            return None;
        }
        Some((path.join("."), method.name.clone()))
    }

    fn resolve_component_path(&self, target: &Expr) -> Option<(Vec<String>, String)> {
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }

        if self.components.contains_key(&cur_ty) || self.transactors.contains_key(&cur_ty) {
            Some((path, cur_ty))
        } else {
            None
        }
    }

    fn component_has_hookable(&self, ty: &str, method: &str) -> bool {
        let has = |items: &[ComponentItem]| -> bool {
            items.iter().any(|it| {
                matches!(
                    it, ComponentItem::Hookable(h) if h.name.name == method
                )
            })
        };
        if let Some(comp) = self.components.get(ty) {
            has(&comp.items)
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            has(&synth.items)
        } else {
            false
        }
    }

    fn resolve_bare_sibling_method_call(
        &self,
        callee: &Expr,
    ) -> Option<(String, String, String, String, bool)> {
        let ExprKind::Ident(id) = &*callee.kind else {
            return None;
        };
        let (comp_ty, receiver, current_method, current_method_active) =
            self.current_component_method.as_ref()?;
        if self.component_has_hookable(comp_ty, &id.name) {
            return Some((
                comp_ty.clone(),
                receiver.clone(),
                id.name.clone(),
                current_method.clone(),
                *current_method_active,
            ));
        }
        None
    }

    fn collect_quiesced_paths(
        &self,
        ty: &str,
        instance: &str,
        stack: &mut std::collections::HashSet<String>,
        out: &mut Vec<String>,
    ) {
        if !stack.insert(ty.to_string()) {
            out.push(instance.to_string());
            return;
        }

        let mut found_subcomponent = false;
        if let Some(comp) = self.components.get(ty) {
            for it in &comp.items {
                if let ComponentItem::Field(f) = it {
                    let Some(field_ty) = type_simple_name(Some(&f.ty)) else {
                        continue;
                    };
                    if self.components.contains_key(field_ty)
                        || self.transactors.contains_key(field_ty)
                    {
                        found_subcomponent = true;
                        let sub_instance = format!("{instance}.{}", f.name.name);
                        self.collect_quiesced_paths(field_ty, &sub_instance, stack, out);
                    }
                }
            }
        } else if let Some(t) = self.transactors.get(ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            for it in &synth.items {
                if let ComponentItem::Field(f) = it {
                    let Some(field_ty) = type_simple_name(Some(&f.ty)) else {
                        continue;
                    };
                    if self.components.contains_key(field_ty)
                        || self.transactors.contains_key(field_ty)
                    {
                        found_subcomponent = true;
                        let sub_instance = format!("{instance}.{}", f.name.name);
                        self.collect_quiesced_paths(field_ty, &sub_instance, stack, out);
                    }
                }
            }
        }

        if !found_subcomponent {
            out.push(instance.to_string());
        }
        stack.remove(ty);
    }

    fn resolve_component_quiesced_predicate<'a>(
        &self,
        callee: &Expr,
        args: &'a [CallArg],
    ) -> Option<(Vec<String>, &'a Expr)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        if method.name != "quiesced" {
            return None;
        }
        let (path, ty) = self.resolve_component_path(target)?;
        if self.component_has_hookable(&ty, "quiesced") {
            return None;
        }
        if args.len() != 1 {
            return None;
        }
        let n_expr = match &args[0] {
            CallArg::Expr(e) => e,
            CallArg::Named { value, .. } => value,
        };
        let mut paths = Vec::new();
        self.collect_quiesced_paths(
            &ty,
            &path.join("."),
            &mut std::collections::HashSet::new(),
            &mut paths,
        );
        Some((paths, n_expr))
    }

    fn emit_idle_predicate(&mut self, instance: &str, kind: &str, n_expr: &Expr) {
        match kind {
            "idle_in" => {
                write!(
                    self.out,
                    "(((uint64_t)cycle_count - {instance}._last_in_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(self.out, "))").ok();
            }
            "idle_out" => {
                write!(
                    self.out,
                    "(((uint64_t)cycle_count - {instance}._last_out_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(self.out, "))").ok();
            }
            "idle" => {
                write!(
                    self.out,
                    "((((uint64_t)cycle_count - {instance}._last_in_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(
                    self.out,
                    ")) && (((uint64_t)cycle_count - {instance}._last_out_cycle) >= (uint64_t)("
                )
                .ok();
                self.emit_expr(n_expr);
                write!(self.out, ")))").ok();
            }
            _ => unreachable!(),
        }
    }

    fn expand_wait_conditions<'a>(&self, conditions: &'a [Expr]) -> Vec<WaitCondition<'a>> {
        let mut out = Vec::new();
        for c in conditions {
            if let ExprKind::Call { callee, args } = &*c.kind {
                if let Some((paths, cycles)) =
                    self.resolve_component_quiesced_predicate(callee, args)
                {
                    out.extend(
                        paths
                            .into_iter()
                            .map(|instance| WaitCondition::Idle { instance, cycles }),
                    );
                    continue;
                }
            }
            out.push(WaitCondition::Expr(c));
        }
        out
    }

    fn resolve_component_method_call(&self, callee: &Expr) -> Option<(String, String, String)> {
        let ExprKind::Field {
            target,
            name: method,
        } = &*callee.kind
        else {
            return None;
        };
        // Walk the field-access chain to its root, collecting the path
        // segments. `topenv.ag.sequencer.dispatch` produces
        // path = ["topenv", "ag", "sequencer"] with method = "dispatch".
        // The root must be an `Ident` (a let-bound name); inner
        // segments are field names we resolve through the type chain.
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();

        // Resolve the type chain. Start from let_types[root]; walk
        // remaining path segments looking each up as a Field on the
        // current type.
        let root = path.first()?;
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let next_ty = if let Some(comp) = self.components.get(&cur_ty) {
                comp.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                synth.items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            return type_simple_name(Some(&f.ty)).map(String::from);
                        }
                    }
                    None
                })
            } else {
                None
            };
            cur_ty = next_ty?;
        }

        // Does cur_ty (component or transactor) have a `hookable
        // <method>`?
        let has_method = |items: &[ComponentItem]| -> bool {
            items.iter().any(|it| {
                matches!(
                    it, ComponentItem::Hookable(h) if h.name.name == method.name
                )
            })
        };
        let found = if let Some(comp) = self.components.get(&cur_ty) {
            has_method(&comp.items)
        } else if let Some(t) = self.transactors.get(&cur_ty) {
            let synth = synth_component_from_transactor(t, /*include_active*/ true);
            has_method(&synth.items)
        } else {
            false
        };
        if !found {
            return None;
        }

        let instance = if let Some(root_sub) = self.field_subs.get(root) {
            if path.len() == 1 {
                root_sub.clone()
            } else {
                format!("{}.{}", root_sub, path[1..].join("."))
            }
        } else {
            path.join(".")
        };

        Some((cur_ty, instance, method.name.clone()))
    }

    /// Walk the field-access chain rooted at `callee` (which must be a
    /// `Field { target, name }` call target) and return the path of
    /// segment names from the root let-binding down to (but excluding)
    /// the method name. Mirrors the path-extraction half of
    /// `resolve_component_method_call`; factored out so the passive-mode
    /// check can re-use the same path without duplicating the walker.
    /// Returns `None` if the callee isn't a field-access chain rooted
    /// at an `Ident`.
    fn extract_method_call_path(callee: &Expr) -> Option<Vec<String>> {
        let ExprKind::Field { target, name: _ } = &*callee.kind else {
            return None;
        };
        let mut path: Vec<String> = Vec::new();
        let mut cur: &Expr = target;
        loop {
            match &*cur.kind {
                ExprKind::Field {
                    target: inner,
                    name,
                } => {
                    path.push(name.name.clone());
                    cur = inner;
                }
                ExprKind::Ident(id) => {
                    path.push(id.name.clone());
                    break;
                }
                _ => return None,
            }
        }
        path.reverse();
        Some(path)
    }

    /// Resolve the effective transactor mode at the end of a
    /// field-access path. The root segment must be a let-bound name
    /// whose mode is in `self.let_modes` (the inheritance root); each
    /// subsequent segment can override via its field-level mode
    /// annotation, otherwise it inherits. Same model as
    /// `emit_subcomponent_handler_registrations` — kept here as a pure
    /// `&self` resolver so the call-site check can run during
    /// `emit_expr` without conflicting with the mutable emitter.
    ///
    /// Returns `None` when the root has no recorded mode (e.g. the
    /// path roots at a non-transactor non-env let), or when an
    /// intermediate type doesn't resolve cleanly. Both cases mean "no
    /// passive-mode constraint to enforce at this site."
    fn resolve_path_mode(&self, path: &[String]) -> Option<TransactorMode> {
        let root = path.first()?;
        let mut effective: Option<TransactorMode> = self.let_modes.get(root).copied();
        let mut cur_ty: String = self.let_types.get(root)?.clone();
        for seg in path.iter().skip(1) {
            let lookup = |items: &[ComponentItem]| -> Option<(String, Option<TransactorMode>)> {
                items.iter().find_map(|it| {
                    if let ComponentItem::Field(f) = it {
                        if &f.name.name == seg {
                            let ty_name = type_simple_name(Some(&f.ty)).map(String::from)?;
                            let mode = match &f.ty {
                                TypeExpr::Named { mode: Some(m), .. } => Some(*m),
                                _ => None,
                            };
                            return Some((ty_name, mode));
                        }
                    }
                    None
                })
            };
            let next = if let Some(comp) = self.components.get(&cur_ty) {
                lookup(&comp.items)
            } else if let Some(t) = self.transactors.get(&cur_ty) {
                let synth = synth_component_from_transactor(t, /*include_active*/ true);
                lookup(&synth.items)
            } else {
                None
            };
            let (ty, mode_opt) = next?;
            cur_ty = ty;
            if let Some(m) = mode_opt {
                effective = Some(m);
            }
        }
        effective
    }

    /// True iff `method_name` is declared as a `hookable` inside
    /// `transactor_name`'s `when active { ... }` block (and is NOT also
    /// shadowed in the always-on body — though that's a parse error in
    /// practice).
    fn method_lives_in_when_active(&self, transactor_name: &str, method_name: &str) -> bool {
        let Some(t) = self.transactors.get(transactor_name) else {
            return false;
        };
        let Some(when_active) = &t.when_active else {
            return false;
        };
        when_active
            .iter()
            .any(|it| matches!(it, ComponentItem::Hookable(h) if h.name.name == method_name))
    }

    /// True iff the given `on`-handler is declared inside
    /// `transactor_name`'s `when active { ... }` block. Identified by
    /// span equality against the original transactor decl so the check
    /// works after `synth_component_from_transactor` flattens the items
    /// list. Returns `false` for non-transactor components, which never
    /// have a `when active` block.
    fn handler_lives_in_when_active(&self, comp_ty: &str, handler: &OnHandler) -> bool {
        let Some(t) = self.transactors.get(comp_ty) else {
            return false;
        };
        let Some(when_active) = &t.when_active else {
            return false;
        };
        when_active
            .iter()
            .any(|it| matches!(it, ComponentItem::OnHandler(h) if h.span == handler.span))
    }

    /// Emit a single `hookable` method on a component as a free
    /// `[&]`-capturing lambda named `<Type>_<method>`. The first
    /// parameter is `<Type>& self`; subsequent parameters mirror the
    /// HARC declaration. Inside the body, bare references to component
    /// fields are rewritten to `self.<field>` via `field_subs`. DUT
    /// pointer fields stay arrow-accessed: `self.dut->aw_addr = ...`.
    /// Component-aware C++ type rendering for a HARC `TypeExpr`.
    /// Wraps `c_type_for` with awareness of declared transactions /
    /// enums / sub-components: those Named types lower to their bare
    /// struct/enum name rather than the default `V<Name>*` pointer.
    /// Used by hookable-method param types and other component-body
    /// contexts where DUT vs. value-type ambiguity would otherwise
    /// produce wrong code.
    fn c_type_for_param(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last() {
                let n = &last.name;
                if self.transactions.contains(n)
                    || self.structs.contains(n)
                    || self.enums.contains_key(n)
                    || self.components.contains_key(n)
                    || self.scoreboards.contains(n)
                    || self.transactors.contains_key(n)
                    || self.covergroups.contains_key(n)
                {
                    return n.clone();
                }
            }
        }
        c_type_for(t)
    }

    fn c_type_for_value(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last() {
                let n = &last.name;
                if self.is_record_type(n)
                    || self.enums.contains_key(n)
                    || self.components.contains_key(n)
                    || self.scoreboards.contains(n)
                    || self.transactors.contains_key(n)
                    || self.covergroups.contains_key(n)
                {
                    return n.clone();
                }
            }
        }
        c_type_for(t)
    }

    /// Emit the two `<Type>_<method>_pre` and `<Type>_<method>_post`
    /// `std::vector<std::function<void(args)>>` declarations that
    /// hold the registered hook subscribers for one hookable method.
    /// Empty by default; users push closures via `on obj.method pre`
    /// / `on obj.method post` at test scope.
    fn emit_hook_vectors(&mut self, c: &ComponentDecl, h: &HookableMethod, depth: usize) {
        // `function` methods (testbench / env helpers, docs/test-
        // ergonomics.md §3.2) carry `is_hookable = false`: no pre/post
        // vectors emitted, and the corresponding fan-out in
        // `emit_component_method` is suppressed. The method itself
        // still emits as a free `<Type>_<method>` lambda.
        if !h.is_hookable {
            return;
        }
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let arg_tys: Vec<String> = h
            .params
            .iter()
            .map(|p| {
                p.ty.as_ref()
                    .map(|t| self.c_type_for_param(t))
                    .unwrap_or_else(|| "int64_t".to_string())
            })
            .collect();
        let arg_csv = arg_tys.join(", ");
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_pre;",
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void({arg_csv})>> {comp_ty}_{m_name}_post;",
        )
        .ok();
    }

    fn emit_component_method(&mut self, c: &ComponentDecl, h: &HookableMethod, depth: usize) {
        let comp_ty = &c.name.name;
        let m_name = &h.name.name;
        let param_names = cpp_param_names(&h.params);
        let ret = h
            .return_ty
            .as_ref()
            .map(|t| self.c_type_for_value(t))
            .unwrap_or_else(|| "void".to_string());
        self.pad(depth);
        write!(self.out, "auto {comp_ty}_{m_name} = [&]({comp_ty}& self").ok();
        // Track Named-typed params as pointers so dut.field rewrites
        // properly in the body. Restore on exit. Transaction / enum /
        // sub-component params are by-value (not pointer-shaped).
        let mut added: Vec<String> = Vec::new();
        let mut saved_param_types = Vec::new();
        for (i, p) in h.params.iter().enumerate() {
            let pty =
                p.ty.as_ref()
                    .map(|t| self.c_type_for_param(t))
                    .unwrap_or_else(|| "int64_t".to_string());
            write!(self.out, ", {pty} {}", param_names[i]).ok();
            if p.name.name != "_" {
                if let Some(ty) = type_simple_name(p.ty.as_ref()) {
                    if self.components.contains_key(ty) || self.transactors.contains_key(ty) {
                        let name = p.name.name.clone();
                        let prev = self.let_types.insert(name.clone(), ty.to_string());
                        saved_param_types.push((name, prev));
                    }
                }
            }
            if matches!(&p.ty, Some(TypeExpr::Named { .. }))
                && self.is_dut_pointer_field_type(p.ty.as_ref().unwrap())
                && p.name.name != "_"
            {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        writeln!(self.out, ") -> {ret} {{").ok();
        // Build field-name substitution: bare `count` inside the body
        // resolves to `self.count`. Dut-pointer fields also get a
        // pointer_vars entry so `dut.field` lowers to `dut->field`.
        let mut subs = std::collections::HashMap::new();
        let mut added_pointer_fields: Vec<String> = Vec::new();
        let mut saved_field_types = Vec::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("self.{}", f.name.name));
                if let Some(ty) = type_simple_name(Some(&f.ty)) {
                    let name = f.name.name.clone();
                    let prev = self.let_types.insert(name.clone(), ty.to_string());
                    saved_field_types.push((name, prev));
                }
                if self.is_dut_pointer_field_type(&f.ty) {
                    if self.pointer_vars.insert(f.name.name.clone()) {
                        added_pointer_fields.push(f.name.name.clone());
                    }
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        // For a `bound to BusType` driver, the parent's bus binding
        // (resolved at codegen time from the test's `let drv : Drv =
        // bind axil` statement) propagates into hookable method
        // bodies. Inside the body, the bare identifier `bus` resolves
        // to the same `(BusDecl, root, prefix)` tuple as the
        // test-scope binding, so `bus.<ch>.send(...)`, `bus.<ch>.recv()`,
        // and `bus.<ch>.<sig>` all work identically to the patterns
        // available inside `on T t` handlers.
        //
        // Restriction (single-instance): we use the FIRST binding
        // discovered for this driver type. If the test instantiates
        // two `let A : Drv = bind X` and `let B : Drv = bind Y` with
        // different buses, both share A's binding inside the
        // hookable body. v0 doesn't have a use case for
        // multi-instance bound drivers; per-instance hookable
        // emission is a follow-up.
        let pushed_bus = self.driver_bus_for_hookables.get(comp_ty).cloned();
        let prior_bus = pushed_bus
            .as_ref()
            .and_then(|b| self.bus_bindings.insert("bus".into(), b.clone()));

        // Pre-hooks: fire `<Type>_<method>_pre` subscribers before the
        // body. The hook closures see the same args as the method —
        // empty vectors are a no-op so the wrap is always safe to
        // emit.
        let arg_csv = param_names.join(", ");
        // Pre/post hook fan-out is skipped for non-hookable `function`
        // methods (docs/test-ergonomics.md §3.2): no vectors were
        // emitted for them, so the fan-out would reference undeclared
        // symbols.
        if h.is_hookable {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (auto& _h : {comp_ty}_{m_name}_pre) _h({arg_csv});"
            )
            .ok();
        }

        let current_method_active = self.method_lives_in_when_active(comp_ty, m_name);
        let prior_component_method = std::mem::replace(
            &mut self.current_component_method,
            Some((
                comp_ty.clone(),
                "self".to_string(),
                m_name.clone(),
                current_method_active,
            )),
        );
        self.emit_block(&h.body, depth + 1);
        self.current_component_method = prior_component_method;

        if h.is_hookable {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "for (auto& _h : {comp_ty}_{m_name}_post) _h({arg_csv});"
            )
            .ok();
        }
        let _ = arg_csv;
        // Restore state.
        if pushed_bus.is_some() {
            match prior_bus {
                Some(prev) => {
                    self.bus_bindings.insert("bus".into(), prev);
                }
                None => {
                    self.bus_bindings.remove("bus");
                }
            }
        }
        self.field_subs = prev_subs;
        for (name, prev) in saved_field_types {
            match prev {
                Some(ty) => {
                    self.let_types.insert(name, ty);
                }
                None => {
                    self.let_types.remove(&name);
                }
            }
        }
        for (name, prev) in saved_param_types {
            match prev {
                Some(ty) => {
                    self.let_types.insert(name, ty);
                }
                None => {
                    self.let_types.remove(&name);
                }
            }
        }
        for k in added_pointer_fields {
            self.pointer_vars.remove(&k);
        }
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    /// Default watchdog period (cycles) when the user writes `watchdog`
    /// without an explicit `period` clause. Spec §8.6.
    const WATCHDOG_DEFAULT_PERIOD: i64 = 1000;
    /// Default watchdog idle threshold (cycles) when the user writes
    /// `watchdog` without an explicit `max_idle` clause.
    const WATCHDOG_DEFAULT_MAX_IDLE: i64 = 10000;

    /// Emit hook-vector declarations + the synthetic `<Type>_watchdog`
    /// method for a component's `watchdog` block (spec §8.6). The
    /// method body, in order:
    ///   1. Pre-hooks (`<Type>_watchdog_pre`) — for `on <Type>.watchdog
    ///      pre` aspect attachments.
    ///   2. User-supplied body statements (typically `log(info, …)`
    ///      debug prints). Field references rewrite to `self.<field>`
    ///      via the same `field_subs` mechanism used for hookable
    ///      methods.
    ///   3. The idle check: if BOTH `_last_in_cycle` AND
    ///      `_last_out_cycle` are ≥ `max_idle` cycles behind
    ///      `cycle_count`, log `FAIL` with a watchdog-specific message
    ///      and bump `errors`.
    ///   4. Post-hooks (`<Type>_watchdog_post`).
    ///
    /// `disabled` watchdogs emit nothing — no hook vectors, no method.
    /// External `on <Type>.watchdog pre/post` referencing a disabled
    /// component's watchdog will surface as a missing-symbol C++
    /// compile error, which is the intended signal that the aspect
    /// has no target. (Reasonable people who turned the watchdog
    /// off will know to remove their hooks.)
    fn emit_watchdog(&mut self, c: &ComponentDecl, w: &WatchdogDecl, depth: usize) {
        if w.disabled {
            return;
        }
        let comp_ty = &c.name.name;
        // Hook vectors — `watchdog` takes no args, so `void()` signature.
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void()>> {comp_ty}_watchdog_pre;"
        )
        .ok();
        self.pad(depth);
        writeln!(
            self.out,
            "std::vector<std::function<void()>> {comp_ty}_watchdog_post;"
        )
        .ok();
        // The method itself: a `[&]`-capturing lambda parallelling the
        // shape of `emit_component_method` so the hookable-dispatch
        // path (`<Type>_<method>(obj, args)`) finds the same symbol.
        self.pad(depth);
        writeln!(
            self.out,
            "auto {comp_ty}_watchdog = [&]({comp_ty}& self) -> void {{"
        )
        .ok();
        // Field substitution: bare `wdog_max_idle` inside the user body
        // resolves to `self.wdog_max_idle`. Same shape as
        // emit_component_method's `subs` setup.
        let mut subs = std::collections::HashMap::new();
        let mut added_pointer_fields: Vec<String> = Vec::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("self.{}", f.name.name));
                if self.is_dut_pointer_field_type(&f.ty) {
                    if self.pointer_vars.insert(f.name.name.clone()) {
                        added_pointer_fields.push(f.name.name.clone());
                    }
                }
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        // Pre-hooks.
        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_watchdog_pre) _h();").ok();

        // User body (typically debug logging).
        self.emit_block(&w.body, depth + 1);

        // Idle check. We emit the C++ directly (not through the HARC
        // `idle()` predicate dispatch) because the receiver is `self`,
        // not a let-bound variable, and `resolve_component_idle_predicate`
        // expects to resolve through `let_types`. Direct C++ keeps
        // the synthetic method self-contained and avoids polluting
        // let_types with the synthetic name `self`.
        self.pad(depth + 1);
        write!(self.out, "int64_t _wdog_max_idle = (int64_t)(").ok();
        if let Some(m) = &w.max_idle {
            self.emit_expr(m);
        } else {
            write!(self.out, "{}", Self::WATCHDOG_DEFAULT_MAX_IDLE).ok();
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (_wdog_max_idle > 0 \
             && (int64_t)((uint64_t)cycle_count - self._last_in_cycle) >= _wdog_max_idle \
             && (int64_t)((uint64_t)cycle_count - self._last_out_cycle) >= _wdog_max_idle) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "sim_log_line(\"FAIL\", \"watchdog: {comp_ty} has been idle for >= %lld cycles\", \
             (long long)_wdog_max_idle);"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "ctx.errors++;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        // Post-hooks.
        self.pad(depth + 1);
        writeln!(self.out, "for (auto& _h : {comp_ty}_watchdog_post) _h();").ok();

        // Restore state.
        self.field_subs = prev_subs;
        for k in added_pointer_fields {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    /// Emit a periodic `_checkers` closure that calls `<Type>_watchdog(<inst>)`
    /// every `period` cycles. Installed at every `let foo : Agent` site
    /// (including sub-component composition). The closure re-reads
    /// the period expression each cycle so per-test overrides via
    /// field assignment work without re-installation.
    fn emit_watchdog_checker(
        &mut self,
        c: &ComponentDecl,
        w: &WatchdogDecl,
        instance: &str,
        depth: usize,
    ) {
        if w.disabled {
            return;
        }
        let comp_ty = &c.name.name;
        // The instance path may contain dots (`env.agent`); the static
        // tag needs to be a valid C++ identifier, so flatten dots to
        // underscores.
        let inst_tag = instance.replace('.', "_");
        // Field substitution: a period expression like `wdog_period`
        // resolves to `<instance>.wdog_period` (parallels how field
        // refs inside `on event(t)` bodies work at let-time).
        let mut subs = std::collections::HashMap::new();
        for ci in &c.items {
            if let ComponentItem::Field(f) = ci {
                subs.insert(f.name.name.clone(), format!("{instance}.{}", f.name.name));
            }
        }
        let prev_subs = std::mem::replace(&mut self.field_subs, subs);

        self.pad(depth);
        writeln!(self.out, "_checkers.push_back([&]() {{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "static int64_t _wdog_{inst_tag}_last = 0;").ok();
        self.pad(depth + 1);
        write!(self.out, "int64_t _wdog_{inst_tag}_period = (int64_t)(").ok();
        if let Some(p) = &w.period {
            self.emit_expr(p);
        } else {
            write!(self.out, "{}", Self::WATCHDOG_DEFAULT_PERIOD).ok();
        }
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (_wdog_{inst_tag}_period > 0 \
             && (int64_t)cycle_count - _wdog_{inst_tag}_last >= _wdog_{inst_tag}_period) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "_wdog_{inst_tag}_last = (int64_t)cycle_count;").ok();
        self.pad(depth + 2);
        writeln!(self.out, "{comp_ty}_watchdog({instance});").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        writeln!(self.out, "}});").ok();

        self.field_subs = prev_subs;
    }

    /// Emit a `driver` / `agent` / `env` / `sequencer` body as a C++
    /// struct. Field types are resolved through `component_field_c_type`,
    /// which recognizes:
    ///
    /// - `event<T>` → `std::vector<std::function<void(T)>>`
    /// - `queue<T>` → `harc_rt::HarcQueue<T>` (when scoreboards are also present)
    /// - Named DUT module → `V<Name>*` pointer (the same shape the
    ///   test-level `let dut : T` already uses)
    /// - Named sub-component → that struct directly (for `env` composing
    ///   `agent` / `driver` / etc. by-value)
    /// - Builtins → uint64_t / int64_t / bool
    ///
    /// `hookable` methods are emitted separately as free `[&]`-capturing
    /// lambdas (see `emit_component_methods`); the struct body holds
    /// data only.
    fn emit_component_struct(&mut self, c: &ComponentDecl) {
        writeln!(self.out, "struct {} {{", c.name.name).ok();
        for it in &c.items {
            if let ComponentItem::Field(f) = it {
                let cty = self.component_field_c_type(&f.ty);
                let init = if let Some(d) = &f.default {
                    format!(" = {}", format_simple_expr(d))
                } else if matches!(&f.ty, TypeExpr::Named { .. })
                    && self.is_dut_pointer_field_type(&f.ty)
                {
                    // Pointer fields default to nullptr — caller assigns
                    // via `drv.dut = dut` after construction.
                    " = nullptr".into()
                } else {
                    "".into()
                };
                writeln!(self.out, "{INDENT}{cty} {}{};", f.name.name, init).ok();
            }
        }
        // Auto-injected activity-tracking fields (spec §7.x). Bumped by
        // codegen at every place the framework knows an in/out has just
        // happened — `on <ev>` handler body entry, bus handshake actor
        // body entry, `bus.<ch>.send/recv`, `emit ev(arg)` — and read
        // by the `obj.idle(N)` / `idle_in(N)` / `idle_out(N)` predicate
        // lowering. Initial value 0 means "no activity yet"; the
        // predicate `idle(N)` correctly reports false until at least N
        // cycles have elapsed since the last bump.
        writeln!(self.out, "{INDENT}uint64_t _last_in_cycle = 0;").ok();
        writeln!(self.out, "{INDENT}uint64_t _last_out_cycle = 0;").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    /// True if a Named-type field on a component refers to a DUT module
    /// (i.e. nothing the codegen knows about other than that it's a
    /// Verilator-compiled module type). Sub-components / scoreboards /
    /// transactors / covergroups are excluded — they're held by-value.
    fn is_dut_pointer_field_type(&self, t: &TypeExpr) -> bool {
        if let Some(name) = type_simple_name(Some(t)) {
            // Every known value-shaped Named type is excluded; only an
            // unrecognized Named type (the DUT module) is pointer-shaped.
            // `structs` matters: a struct-typed method param is a by-value
            // record exactly like a transaction-typed one — classifying it
            // as a pointer emitted `r->field` on a by-value `Result r`.
            return !self.transactions.contains(name)
                && !self.structs.contains(name)
                && !self.scoreboards.contains(name)
                && !self.covergroups.contains_key(name)
                && !self.components.contains_key(name)
                && !self.transactors.contains_key(name)
                && !self.enums.contains_key(name);
        }
        false
    }

    /// Field-type lowering for `driver`/`agent`/`env`/`sequencer` bodies.
    fn component_field_c_type(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Builtin {
                name: BuiltinTy::Event,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("std::vector<std::function<void({inner})>>")
            }
            TypeExpr::Builtin {
                name: BuiltinTy::Queue,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("harc_rt::HarcQueue<{inner}>")
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if self.is_dut_pointer_field_type(t) {
                    // DUT module → Verilator pointer.
                    format!("V{last}*")
                } else {
                    // Sub-component / scoreboard / transaction → by-value.
                    last.to_string()
                }
            }
            _ => self.record_field_c_type(t),
        }
    }

    fn scoreboard_field_c_type(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Builtin {
                name: BuiltinTy::Queue,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("harc_rt::HarcQueue<{inner}>")
            }
            _ => self.record_field_c_type(t),
        }
    }

    /// Resolve a `<T>` payload arg (the inside of `event<T>`,
    /// `queue<T>`, etc.) to a C++ type name. User-declared
    /// transactions and enums get their bare name (so payloads
    /// round-trip through C++ as the struct/enum directly);
    /// everything else falls back to `txn_field_c_type` which
    /// widens narrow ints into `int64_t`/`uint64_t`.
    fn payload_type_for_arg(&self, arg: Option<&TypeArg>) -> String {
        match arg {
            Some(TypeArg::Type(ty)) => {
                if let Some(name) = type_simple_name(Some(ty)) {
                    if self.is_record_type(name) || self.enums.contains_key(name) {
                        return name.to_string();
                    }
                }
                self.record_field_c_type(ty)
            }
            Some(TypeArg::Expr(e)) => {
                // `event<RegOp>` parses as TypeArg::Expr(Ident) at the
                // type-arg layer — the parser doesn't always know the
                // arg's a type until the user actually references it.
                if let ExprKind::Ident(id) = &*e.kind {
                    if self.is_record_type(&id.name) || self.enums.contains_key(&id.name) {
                        return id.name.clone();
                    }
                }
                "uint64_t".into()
            }
            _ => "uint64_t".into(),
        }
    }

    fn emit_scoreboard(&mut self, s: &ComponentDecl) {
        writeln!(self.out, "struct {} {{", s.name.name).ok();
        for it in &s.items {
            if let ComponentItem::Field(f) = it {
                let cty = self.scoreboard_field_c_type(&f.ty);
                let init = if let Some(d) = &f.default {
                    format!(" = {}", format_simple_expr(d))
                } else {
                    "".into()
                };
                writeln!(self.out, "{INDENT}{cty} {}{};", f.name.name, init).ok();
            }
        }
        // Auto-injected activity-tracking fields (spec §7.x). Same shape
        // as for driver/agent/env/sequencer in `emit_component_struct`
        // — scoreboards also count as components, so `sb.idle(N)` is
        // a valid watchdog predicate.
        writeln!(self.out, "{INDENT}uint64_t _last_in_cycle = 0;").ok();
        writeln!(self.out, "{INDENT}uint64_t _last_out_cycle = 0;").ok();
        writeln!(self.out, "}};").ok();
        writeln!(self.out, "").ok();
    }

    fn is_record_type(&self, name: &str) -> bool {
        self.transactions.contains(name) || self.structs.contains(name)
    }

    fn record_field_c_type(&self, t: &TypeExpr) -> String {
        if is_list_type(t) {
            let inner = list_elem_type(t)
                .map(|ty| self.record_field_c_type(ty))
                .unwrap_or_else(|| "uint64_t".into());
            return format!("std::vector<{inner}>");
        }
        if let Some((elem, len)) = fixed_vec_type_args(t) {
            let inner = self.record_field_c_type(elem);
            return format!("std::array<{inner}, {len}>");
        }
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last().map(|s| s.name.as_str()) {
                if self.is_record_type(last) {
                    return last.to_string();
                }
            }
        }
        txn_field_c_type(t)
    }

    fn packed_record_name(&self, t: &TypeExpr) -> Option<String> {
        let TypeExpr::Named { name, .. } = t else {
            return None;
        };
        let last = name.segments.last()?.name.clone();
        self.is_record_type(&last).then_some(last)
    }

    fn packed_width(&self, t: &TypeExpr) -> Option<usize> {
        if let Some((elem, len)) = fixed_vec_type_args(t) {
            return self.packed_width(elem).map(|w| w * len);
        }
        match t {
            TypeExpr::Builtin { name, args, .. } => match name {
                BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                    Some(int_width_from_args(args).unwrap_or(64) as usize)
                }
                BuiltinTy::SInt | BuiltinTy::SIntCap => {
                    Some(int_width_from_args(args).unwrap_or(64) as usize)
                }
                BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(1),
                BuiltinTy::Int => Some(32),
                _ => None,
            },
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last()?.name.as_str();
                let fields = self.record_fields.get(last)?;
                fields
                    .iter()
                    .try_fold(0usize, |acc, f| self.packed_width(&f.ty).map(|w| acc + w))
            }
        }
    }

    fn record_drive_stmt(&self, ty: &TypeExpr, dst_expr: &str, value_expr: &str) -> String {
        if let Some(name) = self.packed_record_name(ty) {
            format!("harc_drive_{name}({dst_expr}, {value_expr});")
        } else {
            format!("harc_rt::harc_assign({dst_expr}, {value_expr});")
        }
    }

    fn record_unpack_expr(&self, ty: &TypeExpr, raw_expr: &str) -> String {
        if let Some(name) = self.packed_record_name(ty) {
            format!("harc_unpack_{name}({raw_expr})")
        } else {
            format!("harc_rt::harc_read({raw_expr})")
        }
    }

    fn emit_pack_bits(&mut self, ty: &TypeExpr, value_expr: &str, offset: usize, depth: usize) {
        if let Some((elem, len)) = fixed_vec_type_args(ty) {
            let Some(elem_w) = self.packed_width(elem) else {
                return;
            };
            for i in 0..len {
                self.emit_pack_bits(
                    elem,
                    &format!("{value_expr}[{i}]"),
                    offset + i * elem_w,
                    depth,
                );
            }
            return;
        }
        if let Some(name) = self.packed_record_name(ty) {
            let Some(width) = self.packed_width(ty) else {
                return;
            };
            self.pad(depth);
            writeln!(
                self.out,
                "harc_rt::harc_wide_write_bits(_packed, {offset}, {width}, harc_pack_{name}({value_expr}));"
            )
            .ok();
            return;
        }
        let Some(width) = self.packed_width(ty) else {
            return;
        };
        self.pad(depth);
        writeln!(
            self.out,
            "harc_rt::harc_wide_write_bits(_packed, {offset}, {width}, {value_expr});"
        )
        .ok();
    }

    fn emit_unpack_bits(&mut self, ty: &TypeExpr, target_expr: &str, offset: usize, depth: usize) {
        if let Some((elem, len)) = fixed_vec_type_args(ty) {
            let Some(elem_w) = self.packed_width(elem) else {
                return;
            };
            for i in 0..len {
                self.emit_unpack_bits(
                    elem,
                    &format!("{target_expr}[{i}]"),
                    offset + i * elem_w,
                    depth,
                );
            }
            return;
        }
        let Some(width) = self.packed_width(ty) else {
            return;
        };
        let cty = self.record_field_c_type(ty);
        self.pad(depth);
        writeln!(
            self.out,
            "{target_expr} = ({cty})harc_rt::harc_bits(_packed, {hi}, {offset});",
            hi = offset + width - 1
        )
        .ok();
    }

    fn emit_record_pack_helpers(&mut self, name: &str, fields: &[Field]) {
        let Some(width) = fields
            .iter()
            .try_fold(0usize, |acc, f| self.packed_width(&f.ty).map(|w| acc + w))
        else {
            return;
        };
        let words = width.div_ceil(32).max(1);
        writeln!(
            self.out,
            "static harc_rt::HarcWide<{words}> harc_pack_{name}(const {name}& value) {{"
        )
        .ok();
        self.pad(1);
        writeln!(self.out, "harc_rt::HarcWide<{words}> _packed{{}};").ok();
        let mut offset = 0usize;
        for f in fields.iter().rev() {
            self.emit_pack_bits(&f.ty, &format!("value.{}", f.name.name), offset, 1);
            offset += self.packed_width(&f.ty).unwrap_or(0);
        }
        self.pad(1);
        writeln!(self.out, "return _packed;").ok();
        writeln!(self.out, "}}").ok();
        writeln!(
            self.out,
            "template<typename Raw> static {name} harc_unpack_{name}(const Raw& raw) {{"
        )
        .ok();
        let raw_checks: Vec<String> = fields
            .iter()
            .map(|f| format!("raw.{}", f.name.name))
            .collect();
        if !raw_checks.is_empty() {
            self.pad(1);
            writeln!(
                self.out,
                "if constexpr (requires {{ {}; }}) {{",
                raw_checks.join("; ")
            )
            .ok();
            self.pad(2);
            writeln!(self.out, "{name} value{{}};").ok();
            for f in fields {
                if let Some((_, len)) = fixed_vec_type_args(&f.ty) {
                    for i in 0..len {
                        self.pad(2);
                        writeln!(self.out, "value.{0}[{i}] = raw.{0}[{i}];", f.name.name).ok();
                    }
                } else {
                    self.pad(2);
                    writeln!(self.out, "value.{0} = raw.{0};", f.name.name).ok();
                }
            }
            self.pad(2);
            writeln!(self.out, "return value;").ok();
            self.pad(1);
            writeln!(self.out, "}} else {{").ok();
        }
        self.pad(1);
        writeln!(
            self.out,
            "auto _packed = harc_rt::harc_wide_zext<{words}>(harc_rt::harc_read(raw));"
        )
        .ok();
        self.pad(1);
        writeln!(self.out, "{name} value{{}};").ok();
        let mut offset = 0usize;
        for f in fields.iter().rev() {
            self.emit_unpack_bits(&f.ty, &format!("value.{}", f.name.name), offset, 1);
            offset += self.packed_width(&f.ty).unwrap_or(0);
        }
        self.pad(1);
        writeln!(self.out, "return value;").ok();
        if !raw_checks.is_empty() {
            self.pad(1);
            writeln!(self.out, "}}").ok();
        }
        writeln!(self.out, "}}").ok();
        writeln!(
            self.out,
            "template<typename Sig> static void harc_drive_{name}(Sig& sig, const {name}& value) {{"
        )
        .ok();
        if !raw_checks.is_empty() {
            self.pad(1);
            writeln!(
                self.out,
                "if constexpr (requires {{ {}; }}) {{",
                raw_checks
                    .iter()
                    .map(|s| s.replacen("raw.", "sig.", 1))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
            .ok();
            for f in fields {
                if let Some((_, len)) = fixed_vec_type_args(&f.ty) {
                    for i in 0..len {
                        self.pad(2);
                        writeln!(self.out, "sig.{0}[{i}] = value.{0}[{i}];", f.name.name).ok();
                    }
                } else {
                    self.pad(2);
                    writeln!(self.out, "sig.{0} = value.{0};", f.name.name).ok();
                }
            }
            self.pad(1);
            writeln!(self.out, "}} else {{").ok();
            self.pad(2);
            writeln!(
                self.out,
                "harc_rt::harc_assign(sig, harc_pack_{name}(value));"
            )
            .ok();
            self.pad(1);
            writeln!(self.out, "}}").ok();
        } else {
            self.pad(1);
            writeln!(
                self.out,
                "harc_rt::harc_assign(sig, harc_pack_{name}(value));"
            )
            .ok();
        }
        writeln!(self.out, "}}").ok();
        writeln!(self.out, "").ok();
    }

    fn local_value_c_type(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last().map(|s| s.name.as_str()) {
                if self.is_record_type(last) {
                    return last.to_string();
                }
            }
            return c_type_for(t);
        }
        self.record_field_c_type(t)
    }

    fn record_field_default(&self, f: &Field) -> String {
        if let Some(d) = &f.default {
            return format_simple_expr(d);
        }
        if is_list_type(&f.ty) {
            return "{}".into();
        }
        if let TypeExpr::Named { name, .. } = &f.ty {
            if name
                .segments
                .last()
                .is_some_and(|s| self.is_record_type(&s.name))
            {
                return "{}".into();
            }
        }
        field_default(f)
    }

    fn emit_record_struct(&mut self, name: &str, fields: &[Field]) {
        writeln!(self.out, "struct {name} {{").ok();
        for f in fields {
            let cty = self.record_field_c_type(&f.ty);
            let init = self.record_field_default(f);
            writeln!(self.out, "{INDENT}{cty} {} = {init};", f.name.name).ok();
        }
        writeln!(self.out, "}};").ok();

        let field_names: Vec<&str> = fields.iter().map(|f| f.name.name.as_str()).collect();
        if field_names.is_empty() {
            writeln!(
                self.out,
                "inline bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
                name
            )
            .ok();
        } else {
            write!(
                self.out,
                "inline bool operator==(const {0}& a, const {0}& b) {{ return ",
                name
            )
            .ok();
            for (i, fname) in field_names.iter().enumerate() {
                if i > 0 {
                    write!(self.out, " && ").ok();
                }
                write!(self.out, "a.{fname} == b.{fname}").ok();
            }
            writeln!(self.out, "; }}").ok();
        }
        writeln!(
            self.out,
            "inline bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
            name
        )
        .ok();
        writeln!(self.out, "").ok();
        self.emit_record_pack_helpers(name, fields);
    }

    fn emit_record_randomize_fn(&mut self, name: &str, fields: &[Field]) {
        writeln!(self.out, "static void randomize_{name}({name}* t) {{").ok();
        for f in fields {
            if f.non_random {
                if let Some(d) = &f.default {
                    write!(self.out, "{INDENT}t->{} = ", f.name.name).ok();
                    self.emit_expr(d);
                    writeln!(self.out, ";").ok();
                }
                continue;
            }
            self.emit_field_random(f);
        }
        writeln!(self.out, "}}").ok();
        writeln!(self.out, "").ok();
    }

    /// Emit a plain `struct` as the shared value-record shape. It gets
    /// the same PRNG helper as a transaction; transaction-only features
    /// such as top-level keeps stay layered in the randomize call path.
    fn emit_struct_record(&mut self, s: &StructDecl) {
        self.emit_record_struct(&s.name.name, &s.fields);
        self.emit_record_randomize_fn(&s.name.name, &s.fields);
    }

    /// Emit a transaction as the shared value-record shape plus a
    /// `randomize_T(&t)` function. Transaction keeps are not emitted here;
    /// the call site merges them into the solver path.
    fn emit_transaction(&mut self, t: &TransactionDecl) {
        let fields = txn_all_fields(&t.body);
        self.emit_record_struct(&t.name.name, &fields);
        self.emit_record_randomize_fn(&t.name.name, &fields);
    }

    fn emit_randomize_trace_event(&mut self, ty: &str, target: &Expr, depth: usize) {
        let Some(fields) = self.txn_fields.get(ty).cloned() else {
            return;
        };
        self.pad(depth);
        writeln!(self.out, "if (trace.enabled) {{").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "std::string _trace_fields = \"\\\"txn_type\\\":\\\"{}\\\",\\\"fields\\\":{{\";",
            escape_c(ty)
        )
        .ok();
        let mut emitted = 0usize;
        for f in &fields {
            if f.width == 0 && f.list.is_none() {
                continue;
            }
            self.pad(depth + 1);
            let prefix = if emitted == 0 {
                format!("\\\"{}\\\":", escape_c(&f.name))
            } else {
                format!(",\\\"{}\\\":", escape_c(&f.name))
            };
            emitted += 1;
            write!(
                self.out,
                "_trace_fields += \"{prefix}\" + std::to_string((unsigned long long)("
            )
            .ok();
            self.emit_expr(target);
            if f.list.is_some() {
                writeln!(self.out, ".{}.size()));", f.name).ok();
            } else {
                writeln!(self.out, ".{}));", f.name).ok();
            }
        }
        self.pad(depth + 1);
        writeln!(self.out, "_trace_fields += \"}}\";").ok();
        self.pad(depth + 1);
        writeln!(self.out, "trace.randomize(cycle_count, _trace_fields);").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    fn trace_component_context(&self) -> String {
        self.current_component_instance.clone().unwrap_or_default()
    }

    fn emit_tlm_call_trace_event(
        &mut self,
        component: &str,
        bus: &str,
        method: &str,
        phase: &str,
        direction: &str,
        tag: Option<&str>,
        depth: usize,
    ) {
        self.pad(depth);
        write!(
            self.out,
            "trace.tlm_call(cycle_count, \"{}\", \"{}\", \"{}\", \"{}\", \"{}\"",
            escape_c(component),
            escape_c(bus),
            escape_c(method),
            escape_c(phase),
            escape_c(direction),
        )
        .ok();
        if let Some(tag) = tag {
            write!(self.out, ", (int64_t)({tag})").ok();
        }
        writeln!(self.out, ");").ok();
    }

    fn report_runtime_dependent_randomize_field_attrs(&mut self, ty: &str) {
        let Some(fields) = self.txn_fields.get(ty).cloned() else {
            return;
        };
        for f in &fields {
            for a in &f.attrs {
                match a.name.name.as_str() {
                    "range" => {
                        for arg in &a.args {
                            if let AttrArg::Expr(e) = arg {
                                self.validate_field_attr_static_expr(ty, &f.name, "range", e);
                            }
                        }
                    }
                    "dist" => {
                        for arg in &a.args {
                            if let AttrArg::Dist(entries) = arg {
                                for entry in entries {
                                    self.validate_field_attr_static_expr(
                                        ty,
                                        &f.name,
                                        "dist",
                                        &entry.value,
                                    );
                                    self.validate_field_attr_static_expr(
                                        ty,
                                        &f.name,
                                        "dist",
                                        &entry.weight,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn emit_target_field_access(&mut self, target: &Expr, f: &TxnFieldInfo) {
        self.emit_expr(target);
        for part in &f.path {
            write!(self.out, ".{part}").ok();
        }
    }

    fn validate_field_attr_static_expr(&mut self, ty: &str, field: &str, attr: &str, expr: &Expr) {
        match &*expr.kind {
            ExprKind::Field { target, name } => {
                if let ExprKind::Ident(root) = &*target.kind {
                    self.errors.push(format!(
                        "randomize({ty}): field attribute `[{attr}]` on `{ty}.{field}` references runtime state `{}.{}`; move runtime-dependent constraints into `blocking randomize(...) with`",
                        root.name, name.name,
                    ));
                }
                self.validate_field_attr_static_expr(ty, field, attr, target);
            }
            ExprKind::Ident(id) if self.let_widths.contains_key(&id.name) => {
                self.errors.push(format!(
                    "randomize({ty}): field attribute `[{attr}]` on `{ty}.{field}` references runtime state `{}`; move runtime-dependent constraints into `blocking randomize(...) with`",
                    id.name,
                ));
            }
            ExprKind::Index { target, index } => {
                self.validate_field_attr_static_expr(ty, field, attr, target);
                self.validate_field_attr_static_expr(ty, field, attr, index);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                for e in [target, hi, lo] {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::Call { callee, args } => {
                self.validate_field_attr_static_expr(ty, field, attr, callee);
                for arg in args {
                    let value = match arg {
                        CallArg::Expr(e) => e,
                        CallArg::Named { value, .. } => value,
                    };
                    self.validate_field_attr_static_expr(ty, field, attr, value);
                }
            }
            ExprKind::ForEachConstraint { iter, body, .. } => {
                self.validate_field_attr_static_expr(ty, field, attr, iter);
                for clause in body {
                    self.validate_field_attr_static_expr(ty, field, attr, clause);
                }
            }
            ExprKind::Cast { expr, .. }
            | ExprKind::Unary { expr, .. }
            | ExprKind::HashHash { expr, .. }
            | ExprKind::SeqRepeat { expr, .. }
            | ExprKind::Paren(expr) => {
                self.validate_field_attr_static_expr(ty, field, attr, expr);
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Membership {
                expr: lhs,
                set: rhs,
            }
            | ExprKind::CoverArrow { lhs, rhs, .. } => {
                self.validate_field_attr_static_expr(ty, field, attr, lhs);
                self.validate_field_attr_static_expr(ty, field, attr, rhs);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                for e in [cond, then_branch, else_branch] {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::RangeLit { lo, hi } => {
                for e in lo.iter().chain(hi.iter()) {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::SetLit(items) => {
                for e in items {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::DistLit(entries) | ExprKind::DistDirective { entries, .. } => {
                for entry in entries {
                    self.validate_field_attr_static_expr(ty, field, attr, &entry.value);
                    self.validate_field_attr_static_expr(ty, field, attr, &entry.weight);
                }
            }
            ExprKind::SystemCall { args, .. } | ExprKind::SolveOrder { args } => {
                for e in args {
                    self.validate_field_attr_static_expr(ty, field, attr, e);
                }
            }
            ExprKind::NamedArg { value, .. } => {
                self.validate_field_attr_static_expr(ty, field, attr, value);
            }
            ExprKind::StructLit { fields, .. } => {
                for f in fields {
                    self.validate_field_attr_static_expr(ty, field, attr, &f.value);
                }
            }
            _ => {}
        }
    }

    fn validate_randomize_constraint_dependencies(
        &mut self,
        ty: &str,
        expr: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
    ) {
        match &*expr.kind {
            ExprKind::Field { target, name } => {
                if expr_field_path(expr, target_root)
                    .is_some_and(|field| field_info.contains_key(&field))
                {
                    return;
                }
                if let ExprKind::Ident(root) = &*target.kind {
                    if !blocking && target_root.is_some_and(|expected| root.name != expected) {
                        let path = format!("{}.{}", root.name, name.name);
                        self.errors.push(format!(
                            "queued randomize({ty}) constraint references runtime state `{path}`; use `blocking randomize` once runtime-dependent constraint lowering is supported"
                        ));
                    }
                }
                self.validate_randomize_constraint_dependencies(
                    ty,
                    target,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::Ident(id) => {
                if !blocking
                    && !field_info.contains_key(&id.name)
                    && !self.enum_variants.contains_key(&id.name)
                    && self.let_widths.contains_key(&id.name)
                {
                    self.errors.push(format!(
                        "queued randomize({ty}) constraint references runtime state `{}`; use `blocking randomize` once runtime-dependent constraint lowering is supported",
                        id.name
                    ));
                }
            }
            ExprKind::Index { target, index } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    target,
                    field_info,
                    target_root,
                    blocking,
                );
                self.validate_randomize_constraint_dependencies(
                    ty,
                    index,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::BitSlice { target, hi, lo } => {
                for e in [target, hi, lo] {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::Call { callee, args } => {
                if list_len_call_field_name(expr, field_info, target_root).is_some() {
                    return;
                }
                self.validate_randomize_constraint_dependencies(
                    ty,
                    callee,
                    field_info,
                    target_root,
                    blocking,
                );
                for arg in args {
                    let value = match arg {
                        CallArg::Expr(e) => e,
                        CallArg::Named { value, .. } => value,
                    };
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        value,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::ForEachConstraint { iter, body, .. } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    iter,
                    field_info,
                    target_root,
                    blocking,
                );
                for clause in body {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        clause,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::Cast { expr, .. }
            | ExprKind::Unary { expr, .. }
            | ExprKind::HashHash { expr, .. }
            | ExprKind::SeqRepeat { expr, .. }
            | ExprKind::Paren(expr) => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    expr,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Membership {
                expr: lhs,
                set: rhs,
            }
            | ExprKind::CoverArrow { lhs, rhs, .. } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    lhs,
                    field_info,
                    target_root,
                    blocking,
                );
                self.validate_randomize_constraint_dependencies(
                    ty,
                    rhs,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                for e in [cond, then_branch, else_branch] {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::RangeLit { lo, hi } => {
                for e in lo.iter().chain(hi.iter()) {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::SetLit(items) => {
                for e in items {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::DistLit(entries) | ExprKind::DistDirective { entries, .. } => {
                for entry in entries {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &entry.value,
                        field_info,
                        target_root,
                        blocking,
                    );
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &entry.weight,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::SoftConstraint(sc) => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    &sc.expr,
                    field_info,
                    target_root,
                    blocking,
                );
                if let Some(weight) = &sc.weight {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        weight,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::SystemCall { args, .. } | ExprKind::SolveOrder { args } => {
                for e in args {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        e,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            ExprKind::NamedArg { value, .. } => {
                self.validate_randomize_constraint_dependencies(
                    ty,
                    value,
                    field_info,
                    target_root,
                    blocking,
                );
            }
            ExprKind::StructLit { fields, .. } => {
                for field in fields {
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &field.value,
                        field_info,
                        target_root,
                        blocking,
                    );
                }
            }
            _ => {}
        }
    }

    /// Emit one random-field assignment honouring `[range(...)]` and
    /// `[dist {...}]` attributes. Falls back to a uniform sample over the
    /// declared type's value range.
    fn emit_field_random(&mut self, f: &Field) {
        if let Some(info) = list_field_info(&f.ty) {
            let max_len = info.declared_max_len.unwrap_or(0);
            writeln!(
                self.out,
                "{INDENT}t->{}.resize((size_t)harc_rt::random::harc_rng_range(harc_rng_next, 0, {}));",
                f.name.name, max_len
            )
            .ok();
            writeln!(
                self.out,
                "{INDENT}for (size_t _i = 0; _i < t->{}.size(); ++_i) {{",
                f.name.name
            )
            .ok();
            let elem_ty = list_elem_type(&f.ty);
            match elem_ty {
                Some(TypeExpr::Builtin { name, args, .. }) => match name {
                    BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                        let w = type_arg_width(args).unwrap_or(32);
                        writeln!(
                            self.out,
                            "{INDENT}{INDENT}t->{}[_i] = {};",
                            f.name.name,
                            emit_random_unsigned_expr(w)
                        )
                        .ok();
                    }
                    BuiltinTy::SInt | BuiltinTy::SIntCap => {
                        let w = type_arg_width(args).unwrap_or(32);
                        if w < 63 {
                            writeln!(
                                self.out,
                                "{INDENT}{INDENT}t->{}[_i] = harc_rt::random::harc_rng_range(harc_rng_next, -(1LL << {}), (1LL << {}) - 1);",
                                f.name.name,
                                w.saturating_sub(1),
                                w.saturating_sub(1)
                            )
                            .ok();
                        } else {
                            writeln!(
                                self.out,
                                "{INDENT}{INDENT}t->{}[_i] = {};",
                                f.name.name,
                                emit_random_unsigned_expr(w)
                            )
                            .ok();
                        }
                    }
                    BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                        writeln!(
                            self.out,
                            "{INDENT}{INDENT}t->{}[_i] = harc_rt::random::harc_rng_range(harc_rng_next, 0, 1);",
                            f.name.name
                        )
                        .ok();
                    }
                    _ => {
                        writeln!(self.out, "{INDENT}{INDENT}t->{}[_i] = 0;", f.name.name).ok();
                    }
                },
                _ => {
                    writeln!(self.out, "{INDENT}{INDENT}t->{}[_i] = 0;", f.name.name).ok();
                }
            }
            writeln!(self.out, "{INDENT}}}").ok();
            return;
        }

        // Look for a `[range(lo, hi)]` or `[dist {...}]` attribute.
        let mut handled = false;
        for a in &f.attrs {
            match a.name.name.as_str() {
                "range" => {
                    if a.args.len() >= 2 {
                        if let (AttrArg::Expr(lo), AttrArg::Expr(hi)) = (&a.args[0], &a.args[1]) {
                            write!(
                                self.out,
                                "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, ",
                                f.name.name
                            )
                            .ok();
                            self.emit_expr(lo);
                            write!(self.out, ", ").ok();
                            self.emit_expr(hi);
                            writeln!(self.out, ");").ok();
                            handled = true;
                        }
                    }
                }
                "dist" => {
                    let dist_args: Option<&Vec<DistEntry>> = a.args.iter().find_map(|x| match x {
                        AttrArg::Dist(d) => Some(d),
                        _ => None,
                    });
                    if let Some(entries) = dist_args {
                        write!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_dist(harc_rng_next, {{",
                            f.name.name
                        )
                        .ok();
                        self.emit_rng_dist_entries(entries);
                        writeln!(self.out, "}});").ok();
                        handled = true;
                    }
                }
                _ => {}
            }
            if handled {
                break;
            }
        }
        if handled {
            return;
        }

        // Fallback: type-driven uniform sampling.
        match &f.ty {
            TypeExpr::Builtin { name, args, .. } => {
                let width = type_arg_width(args);
                match name {
                    BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                        let w = width.unwrap_or(32);
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = {};",
                            f.name.name,
                            emit_random_unsigned_expr(w)
                        )
                        .ok();
                    }
                    BuiltinTy::SInt | BuiltinTy::SIntCap => {
                        let w = width.unwrap_or(32);
                        if w < 63 {
                            writeln!(
                                self.out,
                                "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, -(1LL << {}), (1LL << {}) - 1);",
                                f.name.name,
                                w - 1,
                                w - 1
                            )
                            .ok();
                        } else {
                            writeln!(
                                self.out,
                                "{INDENT}t->{} = {};",
                                f.name.name,
                                emit_random_unsigned_expr(w)
                            )
                            .ok();
                        }
                    }
                    BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, 1);",
                            f.name.name
                        )
                        .ok();
                    }
                    BuiltinTy::Int => {
                        writeln!(
                            self.out,
                            "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, 0x7FFFFFFF);",
                            f.name.name
                        )
                        .ok();
                    }
                    _ => {
                        writeln!(
                            self.out,
                            "{INDENT}// {} : <unsupported type for v0 randomize>",
                            f.name.name
                        )
                        .ok();
                    }
                }
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if let Some(&n) = self.enums.get(last) {
                    let hi = if n == 0 { 0 } else { (n - 1) as i64 };
                    writeln!(
                        self.out,
                        "{INDENT}t->{} = harc_rt::random::harc_rng_range(harc_rng_next, 0, {});",
                        f.name.name, hi
                    )
                    .ok();
                } else if self.is_record_type(last) {
                    writeln!(self.out, "{INDENT}randomize_{last}(&t->{});", f.name.name).ok();
                } else {
                    writeln!(
                        self.out,
                        "{INDENT}// {} : {} (named, not yet supported)",
                        f.name.name, last
                    )
                    .ok();
                }
            }
        }
    }

    fn emit_rng_dist_entries(&mut self, entries: &[DistEntry]) {
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            // Each dist entry: (lo, hi, weight). The `value` expression is
            // either a RangeLit (use lo/hi) or a scalar (use as both lo/hi).
            match &*e.value.kind {
                ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(lo);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(hi);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
                _ => {
                    write!(self.out, "{{(int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.value);
                    write!(self.out, "), (int64_t)(").ok();
                    self.emit_expr(&e.weight);
                    write!(self.out, ")}}").ok();
                }
            }
        }
    }

    fn emit_solver_range_constraint(
        &mut self,
        f: &TxnFieldInfo,
        lo: &Expr,
        hi: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
        solver_width: u32,
        depth: usize,
    ) {
        self.pad(depth);
        write!(self.out, "_s.add(").ok();
        self.emit_solver_range_constraint_expr(
            f,
            lo,
            hi,
            field_info,
            target_root,
            blocking,
            solver_width,
            &std::collections::HashMap::new(),
        );
        writeln!(self.out, ");").ok();
    }

    fn emit_solver_range_constraint_expr(
        &mut self,
        f: &TxnFieldInfo,
        lo: &Expr,
        hi: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
        solver_width: u32,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        let c_name = c_ident(&f.name);
        if f.signed {
            write!(self.out, "_z_{} >= ", c_name).ok();
            self.emit_constraint_expr_w(
                lo,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, " && _z_{} <= ", c_name).ok();
            self.emit_constraint_expr_w(
                hi,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
        } else {
            write!(self.out, "z3::uge(_z_{}, ", c_name).ok();
            self.emit_constraint_expr_w(
                lo,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, ") && z3::ule(_z_{}, ", c_name).ok();
            self.emit_constraint_expr_w(
                hi,
                field_info,
                solver_width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, ")").ok();
        }
    }

    /// Inline a list of top-level constraint expressions, expanding
    /// any `Call(Ident(R), args)` whose name resolves to a known
    /// `relation R(params) … end relation` (spec §4.2). Each call:
    ///   * Looks up R in `self.relations`.
    ///   * Builds a substitution map `formal_param → actual_arg`.
    ///   * For `RelationBody::Block(exprs)`, returns each expr with
    ///     substitution applied — each becomes its own constraint at
    ///     top level (`_s.add(...)` per body expr).
    ///   * For `RelationBody::Alias(expr)`, returns a 1-element Vec
    ///     of the substituted expr.
    ///
    /// Non-call expressions are also walked with `expand_relation_subtree`
    /// so a nested relation call (e.g. inside a `Binary &&`) expands too.
    /// Recursion handles relations of relations.
    fn expand_relation_calls(&self, exprs: &[Expr]) -> Vec<Expr> {
        let mut out: Vec<Expr> = Vec::with_capacity(exprs.len());
        for e in exprs {
            // Top-level Call: expand to potentially multiple constraints.
            if let Some(expanded) = self.try_expand_top_level_call(e) {
                // Each body expr is itself walked so block-form
                // relations that contain nested calls flatten too.
                for be in &expanded {
                    out.extend(self.expand_relation_calls(&[be.clone()]));
                }
                continue;
            }
            // Non-call (or unknown-name call): walk the subtree so any
            // nested R(args) inside Binary / Unary / Paren / etc.
            // expands inline.
            out.push(self.expand_relation_subtree(e));
        }
        out
    }

    /// Try to expand `e` as a top-level relation call. Returns the
    /// substituted body expressions if `e` is `Call(Ident(R), args)`
    /// for a known relation; `None` otherwise (caller falls back to
    /// subtree walking). Arity-mismatched calls return `None` so the
    /// downstream translator surfaces a "constraint expression not
    /// supported" error with a useful span — better than swallowing.
    fn try_expand_top_level_call(&self, e: &Expr) -> Option<Vec<Expr>> {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return None;
        };
        let ExprKind::Ident(id) = &*callee.kind else {
            return None;
        };
        let rel = self.relations.get(&id.name)?;
        if rel.params.len() != args.len() {
            return None;
        }
        let mut subst: std::collections::HashMap<String, Expr> = std::collections::HashMap::new();
        for (p, a) in rel.params.iter().zip(args.iter()) {
            let arg_expr = match a {
                CallArg::Expr(ex) => ex.clone(),
                CallArg::Named { value, .. } => value.clone(),
            };
            subst.insert(p.name.name.clone(), arg_expr);
        }
        let body_exprs: Vec<Expr> = match &rel.body {
            RelationBody::Block(exprs) => {
                exprs.iter().map(|x| substitute_idents(x, &subst)).collect()
            }
            RelationBody::Alias(expr) => vec![substitute_idents(expr, &subst)],
        };
        Some(body_exprs)
    }

    /// Walk a constraint expression, replacing any nested
    /// `Call(Ident(R), args)` (for a known relation R) with R's body.
    /// Block-form bodies collapse to a `&&`-chain so they fit where a
    /// single expression is expected (e.g. inside a `Binary &&`); use
    /// `expand_relation_calls` at the top level to get one constraint
    /// per body expression.
    fn expand_relation_subtree(&self, expr: &Expr) -> Expr {
        let span = expr.span;
        // Recognize a relation Call anywhere in the tree. For block-form
        // bodies, build the AND-of-all-body-exprs expression so the
        // call site (which expected one Expr) gets one Expr back.
        if let Some(body_exprs) = self.try_expand_top_level_call(expr) {
            let exprs: Vec<Expr> = body_exprs
                .iter()
                .map(|x| self.expand_relation_subtree(x))
                .collect();
            return and_join(&exprs, span);
        }
        let new_kind: ExprKind = match &*expr.kind {
            ExprKind::Field { target, name } => ExprKind::Field {
                target: self.expand_relation_subtree(target),
                name: name.clone(),
            },
            ExprKind::Index { target, index } => ExprKind::Index {
                target: self.expand_relation_subtree(target),
                index: self.expand_relation_subtree(index),
            },
            ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
                target: self.expand_relation_subtree(target),
                hi: self.expand_relation_subtree(hi),
                lo: self.expand_relation_subtree(lo),
            },
            ExprKind::Call { callee, args } => ExprKind::Call {
                callee: self.expand_relation_subtree(callee),
                args: args
                    .iter()
                    .map(|a| match a {
                        CallArg::Expr(e) => CallArg::Expr(self.expand_relation_subtree(e)),
                        CallArg::Named { name, value } => CallArg::Named {
                            name: name.clone(),
                            value: self.expand_relation_subtree(value),
                        },
                    })
                    .collect(),
            },
            ExprKind::ForEachConstraint { var, iter, body } => ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: self.expand_relation_subtree(iter),
                body: self.expand_relation_calls(body),
            },
            ExprKind::SoftConstraint(sc) => ExprKind::SoftConstraint(SoftConstraint {
                expr: self.expand_relation_subtree(&sc.expr),
                weight: sc
                    .weight
                    .as_ref()
                    .map(|weight| self.expand_relation_subtree(weight)),
            }),
            ExprKind::Cast { expr, ty } => ExprKind::Cast {
                expr: self.expand_relation_subtree(expr),
                ty: ty.clone(),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op: *op,
                expr: self.expand_relation_subtree(expr),
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: *op,
                lhs: self.expand_relation_subtree(lhs),
                rhs: self.expand_relation_subtree(rhs),
            },
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => ExprKind::Ternary {
                cond: self.expand_relation_subtree(cond),
                then_branch: self.expand_relation_subtree(then_branch),
                else_branch: self.expand_relation_subtree(else_branch),
            },
            ExprKind::Paren(inner) => ExprKind::Paren(self.expand_relation_subtree(inner)),
            ExprKind::Membership { expr, set } => ExprKind::Membership {
                expr: self.expand_relation_subtree(expr),
                set: self.expand_relation_subtree(set),
            },
            ExprKind::SetLit(items) => ExprKind::SetLit(
                items
                    .iter()
                    .map(|x| self.expand_relation_subtree(x))
                    .collect(),
            ),
            ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
                lo: lo.as_ref().map(|x| self.expand_relation_subtree(x)),
                hi: hi.as_ref().map(|x| self.expand_relation_subtree(x)),
            },
            other => other.clone(),
        };
        Expr::new(new_kind, span)
    }

    fn enum_variants_as_ints(&self, expr: &Expr) -> Expr {
        let span = expr.span;
        if let ExprKind::Ident(id) = &*expr.kind {
            if let Some(text) = self.consts.get(&id.name) {
                return Expr::new(ExprKind::Int(text.clone()), span);
            }
            if let Some(idx) = self.enum_variants.get(&id.name).copied() {
                return Expr::new(ExprKind::Int(idx.to_string()), span);
            }
        }
        let kind = match &*expr.kind {
            ExprKind::Field { target, name } => ExprKind::Field {
                target: self.enum_variants_as_ints(target),
                name: name.clone(),
            },
            ExprKind::Index { target, index } => ExprKind::Index {
                target: self.enum_variants_as_ints(target),
                index: self.enum_variants_as_ints(index),
            },
            ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
                target: self.enum_variants_as_ints(target),
                hi: self.enum_variants_as_ints(hi),
                lo: self.enum_variants_as_ints(lo),
            },
            ExprKind::Call { callee, args } => ExprKind::Call {
                callee: self.enum_variants_as_ints(callee),
                args: args
                    .iter()
                    .map(|arg| match arg {
                        CallArg::Expr(e) => CallArg::Expr(self.enum_variants_as_ints(e)),
                        CallArg::Named { name, value } => CallArg::Named {
                            name: name.clone(),
                            value: self.enum_variants_as_ints(value),
                        },
                    })
                    .collect(),
            },
            ExprKind::Cast { expr, ty } => ExprKind::Cast {
                expr: self.enum_variants_as_ints(expr),
                ty: ty.clone(),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op: *op,
                expr: self.enum_variants_as_ints(expr),
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: *op,
                lhs: self.enum_variants_as_ints(lhs),
                rhs: self.enum_variants_as_ints(rhs),
            },
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => ExprKind::Ternary {
                cond: self.enum_variants_as_ints(cond),
                then_branch: self.enum_variants_as_ints(then_branch),
                else_branch: self.enum_variants_as_ints(else_branch),
            },
            ExprKind::Paren(inner) => ExprKind::Paren(self.enum_variants_as_ints(inner)),
            ExprKind::Membership { expr, set } => ExprKind::Membership {
                expr: self.enum_variants_as_ints(expr),
                set: self.enum_variants_as_ints(set),
            },
            ExprKind::SoftConstraint(sc) => ExprKind::SoftConstraint(SoftConstraint {
                expr: self.enum_variants_as_ints(&sc.expr),
                weight: sc
                    .weight
                    .as_ref()
                    .map(|weight| self.enum_variants_as_ints(weight)),
            }),
            ExprKind::SetLit(items) => ExprKind::SetLit(
                items
                    .iter()
                    .map(|item| self.enum_variants_as_ints(item))
                    .collect(),
            ),
            ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
                lo: lo.as_ref().map(|e| self.enum_variants_as_ints(e)),
                hi: hi.as_ref().map(|e| self.enum_variants_as_ints(e)),
            },
            other => other.clone(),
        };
        Expr::new(kind, span)
    }

    /// Emit an inline Z3 solver block for `randomize(t) with { ... }`.
    /// Each call builds a fresh Z3 context, declares one bitvector variable
    /// per field at its declared width, translates the constraint
    /// expressions, sets `random_seed` from the runtime call-site seed so
    /// `--seed` flows through, then assigns the satisfying model back into `t`. UNSAT
    /// raises a FAIL log line and increments errors.
    fn emit_constraint_solver_block(
        &mut self,
        ty: &str,
        target: &Expr,
        with_body: &[Expr],
        blocking: bool,
        depth: usize,
    ) {
        // Expand relation calls into their bodies up-front so the
        // rest of the function sees a flat list of constraints. Both
        // the user's `with` body AND the transaction's keeps reach
        // this function via the merge in StmtKind::Randomize.
        let with_body_owned: Vec<Expr> = self.expand_relation_calls(with_body);
        let with_body: &[Expr] = &with_body_owned;
        let fields = match self.txn_fields.get(ty).cloned() {
            Some(f) => f,
            None => {
                self.errors
                    .push(format!("internal: no field info for transaction `{ty}`"));
                return;
            }
        };
        // Tracking for the constraint translator: field names plus their
        // signedness/domain metadata. Z3 vars remain 64-bit in this migration
        // step, but operator selection and domain constraints now honor the
        // source field type.
        let field_info: std::collections::HashMap<String, TxnFieldInfo> =
            fields.iter().map(|f| (f.name.clone(), f.clone())).collect();
        let target_root = randomize_target_ident(target);
        let mut dist_directives: std::collections::HashMap<String, Vec<DistEntry>> =
            std::collections::HashMap::new();
        let mut solve_order_directives: Vec<Vec<String>> = Vec::new();
        let mut hard_constraints: Vec<&Expr> = Vec::new();
        let mut soft_constraints: Vec<(&Expr, u32)> = Vec::new();
        for c in with_body {
            match &*c.kind {
                ExprKind::SoftConstraint(sc) => {
                    if matches!(
                        &*sc.expr.kind,
                        ExprKind::DistDirective { .. } | ExprKind::SolveOrder { .. }
                    ) {
                        self.errors.push(format!(
                            "randomize({ty}) with `soft`: expected a boolean constraint expression"
                        ));
                        continue;
                    }
                    let weight = match sc
                        .weight
                        .as_ref()
                        .map_or(Some(1), |weight| fold_int_literal(weight))
                        .and_then(|weight| u32::try_from(weight).ok())
                    {
                        Some(0) | None => {
                            self.errors.push(format!(
                                "randomize({ty}) with `soft`: weight must be a positive integer literal"
                            ));
                            1
                        }
                        Some(weight) => weight,
                    };
                    self.validate_randomize_constraint_dependencies(
                        ty,
                        &sc.expr,
                        &field_info,
                        target_root,
                        blocking,
                    );
                    soft_constraints.push((&sc.expr, weight));
                    continue;
                }
                ExprKind::DistDirective { target, entries } => {
                    if let Some(field) =
                        randomize_target_field_name(target, &field_info, target_root)
                    {
                        if let Some(info) = field_info.get(&field) {
                            if info.non_random || info.list.is_some() || info.width == 0 {
                                self.errors.push(format!(
                                    "randomize({ty}) with `dist`: target `{field}` must be a random scalar field of transaction `{ty}`"
                                ));
                                continue;
                            }
                        }
                        for entry in entries {
                            self.validate_randomize_constraint_dependencies(
                                ty,
                                &entry.value,
                                &field_info,
                                target_root,
                                blocking,
                            );
                            self.validate_randomize_constraint_dependencies(
                                ty,
                                &entry.weight,
                                &field_info,
                                target_root,
                                blocking,
                            );
                        }
                        dist_directives.insert(field, entries.clone());
                    } else {
                        self.errors.push(format!(
                            "randomize({ty}) with `dist`: target must be a field of transaction `{ty}`"
                        ));
                    }
                    continue;
                }
                ExprKind::SolveOrder { args } => {
                    if args.len() < 2 {
                        self.errors.push(format!(
                            "randomize({ty}) with `solve_order` expects at least two target fields",
                        ));
                        continue;
                    }
                    let mut fields = Vec::new();
                    let mut valid = true;
                    for arg in args {
                        if let Some(field) =
                            randomize_target_field_name(arg, &field_info, target_root)
                        {
                            if let Some(info) = field_info.get(&field) {
                                if info.non_random {
                                    self.errors.push(format!(
                                        "randomize({ty}) with `solve_order`: field `{field}` is non-random and cannot be ordered"
                                    ));
                                    valid = false;
                                    continue;
                                }
                                if info.list.is_some() || info.width == 0 {
                                    self.errors.push(format!(
                                        "randomize({ty}) with `solve_order`: field `{field}` must be a scalar random field"
                                    ));
                                    valid = false;
                                    continue;
                                }
                            }
                            fields.push(field);
                        } else {
                            self.errors.push(format!(
                                "randomize({ty}) with `solve_order`: arguments must be fields of transaction `{ty}`",
                            ));
                            valid = false;
                        }
                    }
                    if valid {
                        solve_order_directives.push(fields);
                    }
                    continue;
                }
                _ => {}
            }
            hard_constraints.push(c);
        }
        for c in &hard_constraints {
            self.validate_randomize_constraint_dependencies(
                ty,
                c,
                &field_info,
                target_root,
                blocking,
            );
        }
        let list_unroll_bounds =
            infer_list_unroll_bounds(&fields, &hard_constraints, &field_info, target_root);
        for f in &fields {
            if f.list.is_some() && !list_unroll_bounds.contains_key(&f.name) {
                self.errors.push(format!(
                    "randomize({ty}): list field `{}` needs a bounded length constraint such as `{}.len() <= N`",
                    f.name, f.name
                ));
            }
        }
        let solver_width = fields
            .iter()
            .map(txn_field_solver_width)
            .max()
            .unwrap_or(64)
            .max(64);
        if solver_width > 1024 {
            self.errors.push(format!(
                "randomize({ty}): constraint solver supports vector fields up to 1024 bits, got {} bits",
                solver_width
            ));
        }

        self.pad(depth);
        writeln!(
            self.out,
            "{{   // {} randomize(t) with — runtime constrained solve callback",
            if blocking { "blocking" } else { "queued" }
        )
        .ok();

        if let Some(problem_id) = self.runtime_randomize_problem_id(target) {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "auto _harc_rt_call = _harc_runtime_random_problem_table_prepare_call({problem_id}, harc_rng.state, harc_rng_next());"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "auto* _harc_rt_problem = _harc_rt_call.problem;").ok();
            self.pad(depth + 1);
            writeln!(self.out, "auto _harc_rt_seed = _harc_rt_call.seed;").ok();
        } else {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "const harc_rt::random::HarcRuntimeProblemDescriptor* _harc_rt_problem = nullptr;"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "auto _harc_rt_seed = harc_rng_next();").ok();
        }
        self.pad(depth + 1);
        writeln!(
            self.out,
            "auto _harc_rt_generated_solver = [&]() -> harc_rt::random::HarcSolveStatus {{"
        )
        .ok();

        // Context + solver. We use `z3::solver` (not `optimize`) so UNSAT
        // is reported faithfully — `optimize` can return a "best partial"
        // model when soft+hard constraints conflict, hiding real UNSAT.
        // Ordinary diversity comes from deterministic seeded candidate
        // preferences; persistent no-repeat history is reserved for explicit
        // `[unique]` fields.
        self.pad(depth + 1);
        writeln!(self.out, "z3::context _ctx;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "z3::solver _s(_ctx);").ok();
        self.pad(depth + 1);
        writeln!(self.out, "z3::params _p(_ctx);").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "_p.set(\"random_seed\", static_cast<unsigned>(_harc_rt_seed & 0x7fffffffU));"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(self.out, "_s.set(_p);").ok();

        // All Z3 vars in this solve use one common bit-vector width so
        // binops with literals and across fields don't trip Z3's sort
        // compatibility checks. The common width grows past 64 when a
        // wide field participates; each narrower field then gets a range
        // constraint enforcing its declared width.
        let mut guarded_field_ranges = Vec::new();
        for f in &fields {
            let c_name = c_ident(&f.name);
            if let Some(list) = &f.list {
                let max_len = list_unroll_bounds.get(&f.name).copied().unwrap_or(0);
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "z3::expr _z_{}_len = _ctx.bv_const(\"{}_len\", {});",
                    c_name, c_name, solver_width
                )
                .ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ule(_z_{}_len, _ctx.bv_val((uint64_t){}, {})));",
                    c_name, max_len, solver_width
                )
                .ok();
                if f.non_random {
                    self.pad(depth + 1);
                    write!(
                        self.out,
                        "_s.add(_z_{}_len == _ctx.bv_val((uint64_t)(",
                        c_name
                    )
                    .ok();
                    self.emit_target_field_access(target, f);
                    writeln!(self.out, ".size()), {}));", solver_width).ok();
                }
                for i in 0..max_len {
                    self.pad(depth + 1);
                    writeln!(
                        self.out,
                        "z3::expr _z_{}_{i} = _ctx.bv_const(\"{}_{i}\", {});",
                        c_name, c_name, solver_width
                    )
                    .ok();
                    if list.elem_signed && list.elem_width < solver_width {
                        let (lo_expr, hi_expr) = signed_domain_bound_exprs(list.elem_width);
                        self.pad(depth + 1);
                        writeln!(
                            self.out,
                            "_s.add(_z_{}_{i} >= {});",
                            c_name,
                            solver_bv_value_call(&lo_expr, true, list.elem_width, solver_width)
                        )
                        .ok();
                        self.pad(depth + 1);
                        writeln!(
                            self.out,
                            "_s.add(_z_{}_{i} <= {});",
                            c_name,
                            solver_bv_value_call(&hi_expr, true, list.elem_width, solver_width)
                        )
                        .ok();
                    } else if !list.elem_signed && list.elem_width < solver_width {
                        self.pad(depth + 1);
                        writeln!(
                            self.out,
                            "_s.add(z3::ult(_z_{}_{i}, z3::shl(_ctx.bv_val((uint64_t)1, {}), _ctx.bv_val((uint64_t){}, {}))));",
                            c_name, solver_width, list.elem_width, solver_width
                        )
                        .ok();
                    }
                    if f.non_random {
                        self.pad(depth + 1);
                        write!(self.out, "if (").ok();
                        self.emit_target_field_access(target, f);
                        if list.elem_width <= 64 {
                            write!(self.out, ".size() > {i}) _s.add(_z_{}_{i} == ", c_name).ok();
                            if list.elem_signed {
                                write!(self.out, "harc_z3_bv_signed_value(_ctx, ").ok();
                            } else {
                                write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                            }
                            self.emit_target_field_access(target, f);
                            if list.elem_signed {
                                writeln!(
                                    self.out,
                                    "[{i}], {}, {}));",
                                    list.elem_width, solver_width
                                )
                                .ok();
                            } else {
                                writeln!(self.out, "[{i}], {}));", solver_width).ok();
                            }
                        } else {
                            write!(self.out, ".size() > {i}) _s.add(_z_{}_{i} == ", c_name).ok();
                            if list.elem_signed {
                                write!(self.out, "harc_z3_bv_signed_value(_ctx, ").ok();
                            } else {
                                write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                            }
                            self.emit_target_field_access(target, f);
                            if list.elem_signed {
                                writeln!(
                                    self.out,
                                    "[{i}], {}, {}));",
                                    list.elem_width, solver_width
                                )
                                .ok();
                            } else {
                                writeln!(self.out, "[{i}], {}));", solver_width).ok();
                            }
                        }
                    }
                }
                continue;
            }
            if f.width == 0 {
                continue;
            }
            self.pad(depth + 1);
            writeln!(
                self.out,
                "z3::expr _z_{} = _ctx.bv_const(\"{}\", {});",
                c_name, c_name, solver_width
            )
            .ok();
            if let Some(n) = f.enum_variants {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ule(_z_{}, _ctx.bv_val((uint64_t){}, {})));",
                    c_name,
                    n.saturating_sub(1),
                    solver_width
                )
                .ok();
            } else if f.signed && f.width < solver_width {
                let (lo_expr, hi_expr) = signed_domain_bound_exprs(f.width);
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(_z_{} >= {});",
                    c_name,
                    solver_bv_value_call(&lo_expr, true, f.width, solver_width)
                )
                .ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(_z_{} <= {});",
                    c_name,
                    solver_bv_value_call(&hi_expr, true, f.width, solver_width)
                )
                .ok();
            } else if !f.signed && f.width < solver_width {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "_s.add(z3::ult(_z_{}, z3::shl(_ctx.bv_val((uint64_t)1, {}), _ctx.bv_val((uint64_t){}, {}))));",
                    c_name, solver_width, f.width, solver_width
                )
                .ok();
            }
            if f.non_random {
                self.pad(depth + 1);
                if f.width <= 64 {
                    if f.signed {
                        write!(
                            self.out,
                            "_s.add(_z_{} == harc_z3_bv_signed_value(_ctx, ",
                            c_name
                        )
                        .ok();
                    } else {
                        write!(self.out, "_s.add(_z_{} == harc_z3_bv_value(_ctx, ", c_name).ok();
                    }
                    self.emit_target_field_access(target, f);
                    if f.signed {
                        writeln!(self.out, ", {}, {}));", f.width, solver_width).ok();
                    } else {
                        writeln!(self.out, ", {}));", solver_width).ok();
                    }
                } else {
                    if f.signed {
                        write!(
                            self.out,
                            "_s.add(_z_{} == harc_z3_bv_signed_value(_ctx, ",
                            c_name
                        )
                        .ok();
                    } else {
                        write!(self.out, "_s.add(_z_{} == harc_z3_bv_value(_ctx, ", c_name).ok();
                    }
                    self.emit_target_field_access(target, f);
                    if f.signed {
                        writeln!(self.out, ", {}, {}));", f.width, solver_width).ok();
                    } else {
                        writeln!(self.out, ", {}));", solver_width).ok();
                    }
                }
            }
            if let Some((lo, hi)) = field_attr_range(f) {
                if f.when_guard.is_some() {
                    guarded_field_ranges.push((f, lo, hi));
                    continue;
                }
                self.emit_solver_range_constraint(
                    f,
                    lo,
                    hi,
                    &field_info,
                    target_root,
                    blocking,
                    solver_width,
                    depth + 1,
                );
            }
        }

        let (guarded_branch_constraints, base_hard_constraints): (Vec<&Expr>, Vec<&Expr>) =
            hard_constraints
                .iter()
                .copied()
                .partition(|c| guarded_constraint_guard(c).is_some());

        // Translated base constraints. Guarded `when` branch constraints
        // are added after a discriminator-only solve below.
        for c in &base_hard_constraints {
            if let ExprKind::ForEachConstraint { var, iter, body } = &*c.kind {
                self.emit_foreach_constraint_clauses(
                    ty,
                    var,
                    iter,
                    body,
                    &field_info,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                    solver_width,
                    depth + 1,
                );
                continue;
            }
            self.pad(depth + 1);
            write!(self.out, "_s.add(").ok();
            self.emit_constraint_expr_with_list_bounds(
                c,
                &field_info,
                solver_width,
                target_root,
                blocking,
                &list_unroll_bounds,
            );
            writeln!(self.out, ");").ok();
        }
        if !guarded_branch_constraints.is_empty() || !guarded_field_ranges.is_empty() {
            let mut discriminator_fields = std::collections::HashSet::new();
            for c in &guarded_branch_constraints {
                if let Some(guard) = guarded_constraint_guard(c) {
                    collect_randomize_target_field_refs(
                        guard,
                        &field_info,
                        target_root,
                        &mut discriminator_fields,
                    );
                }
            }
            for (f, _, _) in &guarded_field_ranges {
                if let Some(guard) = &f.when_guard {
                    collect_randomize_target_field_refs(
                        guard,
                        &field_info,
                        target_root,
                        &mut discriminator_fields,
                    );
                }
            }
            if !discriminator_fields.is_empty() {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "// discriminator-first when-subtype solve: pick guard fields before branch-local constraints"
                )
                .ok();
                self.pad(depth + 1);
                writeln!(self.out, "auto _when_r = _s.check();").ok();
                self.pad(depth + 1);
                writeln!(self.out, "if (_when_r == z3::sat) {{").ok();
                self.pad(depth + 2);
                writeln!(self.out, "z3::model _when_m = _s.get_model();").ok();
                let mut discriminator_fields: Vec<String> =
                    discriminator_fields.into_iter().collect();
                discriminator_fields.sort();
                for field in discriminator_fields {
                    let Some(info) = field_info.get(&field) else {
                        continue;
                    };
                    if info.list.is_some() || info.width == 0 {
                        continue;
                    }
                    let c_name = c_ident(&field);
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "_s.add(_z_{} == _when_m.eval(_z_{}, true));",
                        c_name, c_name
                    )
                    .ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
            }
            for c in &guarded_branch_constraints {
                if let ExprKind::ForEachConstraint { var, iter, body } = &*c.kind {
                    self.emit_foreach_constraint_clauses(
                        ty,
                        var,
                        iter,
                        body,
                        &field_info,
                        target_root,
                        blocking,
                        &list_unroll_bounds,
                        solver_width,
                        depth + 1,
                    );
                    continue;
                }
                self.pad(depth + 1);
                write!(self.out, "_s.add(").ok();
                self.emit_constraint_expr_with_list_bounds(
                    c,
                    &field_info,
                    solver_width,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                );
                writeln!(self.out, ");").ok();
            }
            for (f, lo, hi) in &guarded_field_ranges {
                let Some(guard) = &f.when_guard else {
                    continue;
                };
                self.pad(depth + 1);
                write!(self.out, "_s.add(!(").ok();
                self.emit_constraint_bool_expr_w(
                    guard,
                    &field_info,
                    solver_width,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                );
                write!(self.out, ") || ").ok();
                self.emit_solver_range_constraint_expr(
                    f,
                    lo,
                    hi,
                    &field_info,
                    target_root,
                    blocking,
                    solver_width,
                    &list_unroll_bounds,
                );
                writeln!(self.out, ");").ok();
            }
        }
        for fields in &solve_order_directives {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "// solve_order({}) accepted as solver scheduling metadata",
                fields.join(", ")
            )
            .ok();
        }
        let mut ordered_soft_constraints: Vec<(usize, &Expr, u32)> = soft_constraints
            .iter()
            .enumerate()
            .map(|(idx, (expr, weight))| (idx, *expr, *weight))
            .collect();
        ordered_soft_constraints.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        for (idx, c, weight) in ordered_soft_constraints {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "_s.push();   // soft constraint weight {}: {}",
                weight,
                escape_c(&expr_source_str(c))
            )
            .ok();
            if let ExprKind::ForEachConstraint { var, iter, body } = &*c.kind {
                self.emit_foreach_constraint_clauses(
                    ty,
                    var,
                    iter,
                    body,
                    &field_info,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                    solver_width,
                    depth + 1,
                );
            } else {
                self.pad(depth + 1);
                write!(self.out, "_s.add(").ok();
                self.emit_constraint_expr_with_list_bounds(
                    c,
                    &field_info,
                    solver_width,
                    target_root,
                    blocking,
                    &list_unroll_bounds,
                );
                writeln!(self.out, ");").ok();
            }
            self.pad(depth + 1);
            writeln!(self.out, "auto _soft_r_{idx} = _s.check();").ok();
            self.pad(depth + 1);
            writeln!(self.out, "if (_soft_r_{idx} != z3::sat) {{").ok();
            self.pad(depth + 2);
            writeln!(self.out, "_s.pop();").ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "sim_log_line(\"INFO\", \"randomize(t) with: dropped unsatisfiable soft constraint `{}` (weight {})\");",
                escape_c(&expr_source_str(c)),
                weight
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "}}").ok();
        }

        // Detect fields the user has equality-pinned (e.g. `t.addr == 24`).
        // Those fields have only one satisfying value, so they are removed
        // from free-field preference sampling and uniqueness history.
        let pinned: std::collections::HashSet<String> = hard_constraints
            .iter()
            .copied()
            .filter_map(|e| {
                if let ExprKind::Binary {
                    op: BinaryOp::Eq,
                    lhs,
                    rhs,
                } = &*e.kind
                {
                    let pin_from = |side: &Expr, other: &Expr| -> Option<String> {
                        if matches!(&*other.kind, ExprKind::Int(_)) {
                            let field = expr_field_path(side, target_root)?;
                            if field_info.contains_key(&field) {
                                return Some(field);
                            }
                        }
                        None
                    };
                    pin_from(lhs, rhs).or_else(|| pin_from(rhs, lhs))
                } else {
                    None
                }
            })
            .collect();
        for ordered_fields in &solve_order_directives {
            for field in ordered_fields {
                if pinned.contains(field) {
                    self.errors.push(format!(
                        "randomize({ty}) with `solve_order`: field `{field}` is equality-pinned and cannot affect sampling order"
                    ));
                }
            }
        }
        let mut constrained_fields = std::collections::HashSet::new();
        for c in &hard_constraints {
            collect_randomize_target_field_refs(
                c,
                &field_info,
                target_root,
                &mut constrained_fields,
            );
        }

        let cache_tag = format!("_solver_site_{}", target.span.start);
        let mut free_fields: Vec<&TxnFieldInfo> = fields
            .iter()
            .filter(|f| {
                f.list.is_none()
                    && f.width > 0
                    && !f.non_random
                    && !pinned.contains(&f.name)
                    && f.when_guard.is_none()
            })
            .collect();
        let free_field_names: std::collections::HashSet<String> =
            free_fields.iter().map(|f| f.name.clone()).collect();
        if !solve_order_directives.is_empty() {
            let mut before: std::collections::HashMap<String, std::collections::HashSet<String>> =
                std::collections::HashMap::new();
            let mut indegree: std::collections::HashMap<String, usize> = free_fields
                .iter()
                .map(|f| (f.name.clone(), 0usize))
                .collect();
            for ordered_fields in &solve_order_directives {
                for i in 0..ordered_fields.len() {
                    for j in (i + 1)..ordered_fields.len() {
                        let (src, dst) = (&ordered_fields[i], &ordered_fields[j]);
                        if !free_field_names.contains(src) || !free_field_names.contains(dst) {
                            continue;
                        }
                        let entry = before.entry(src.clone()).or_default();
                        if entry.insert(dst.clone()) {
                            *indegree.entry(dst.clone()).or_insert(0) += 1;
                        }
                    }
                }
            }

            let original_order: Vec<String> = free_fields.iter().map(|f| f.name.clone()).collect();
            let mut ordered_names = Vec::new();
            let mut ready: Vec<String> = original_order
                .iter()
                .filter(|name| indegree.get(*name).copied().unwrap_or(0) == 0)
                .cloned()
                .collect();
            while let Some(name) = ready.first().cloned() {
                ready.remove(0);
                ordered_names.push(name.clone());
                if let Some(nexts) = before.get(&name) {
                    for candidate in &original_order {
                        if !nexts.contains(candidate) {
                            continue;
                        }
                        if let Some(n) = indegree.get_mut(candidate) {
                            *n = n.saturating_sub(1);
                            if *n == 0 {
                                ready.push(candidate.clone());
                            }
                        }
                    }
                }
            }

            if ordered_names.len() != free_fields.len() {
                self.errors.push(format!(
                    "randomize({ty}) solve-order hints form a cycle among free fields"
                ));
            } else {
                let rank: std::collections::HashMap<String, usize> = ordered_names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (name.clone(), i))
                    .collect();
                free_fields.sort_by_key(|f| rank.get(&f.name).copied().unwrap_or(usize::MAX));
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "// solve-order sampling order: {}",
                    ordered_names.join(", ")
                )
                .ok();
            }
        }
        let unique_fields: std::collections::HashSet<String> = free_fields
            .iter()
            .filter(|f| field_attr_unique(f) && !constrained_fields.contains(&f.name))
            .map(|f| f.name.clone())
            .collect();
        let auto_goals: Vec<AutoCoverageGoal> = free_fields
            .iter()
            .filter(|f| !constrained_fields.contains(&f.name))
            .filter_map(|f| {
                let values = auto_coverage_values(f);
                (!values.is_empty()).then(|| AutoCoverageGoal {
                    field: f.name.clone(),
                    c_field: c_ident(&f.name),
                    values,
                })
            })
            .collect();
        let mut auto_crosses: Vec<(AutoCoverageGoal, AutoCoverageGoal)> = Vec::new();
        for i in 0..auto_goals.len() {
            for j in (i + 1)..auto_goals.len() {
                let cross_bins = auto_goals[i].values.len() * auto_goals[j].values.len();
                if cross_bins <= 64 && auto_crosses.len() < 16 {
                    auto_crosses.push((auto_goals[i].clone(), auto_goals[j].clone()));
                }
            }
        }

        // History is a semantic policy now, not the default source of
        // diversity. Ordinary random fields are sampled by seeded
        // preferences; only explicit `[unique]` fields keep no-repeat state
        // across calls.
        for f in free_fields
            .iter()
            .filter(|f| unique_fields.contains(&f.name))
        {
            let c_name = c_ident(&f.name);
            let scope = c_scope_ident(field_attr_unique_scope(f));
            let value_ty = txn_field_solver_c_type(f);
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static harc_rt::random::HarcUniqueHistory<{}> {cache_tag}_unique_{}_{};",
                value_ty, scope, c_name
            )
            .ok();
        }
        if !auto_goals.is_empty() {
            for goal in &auto_goals {
                self.pad(depth + 1);
                write!(
                    self.out,
                    "static const char* _auto_cov_labels_{cache_tag}_{}[] = {{",
                    goal.c_field
                )
                .ok();
                for (idx, value) in goal.values.iter().enumerate() {
                    if idx > 0 {
                        write!(self.out, ", ").ok();
                    }
                    write!(
                        self.out,
                        "\"{}.{}={}\"",
                        escape_c(ty),
                        escape_c(&goal.field),
                        escape_c(&value.label)
                    )
                    .ok();
                }
                writeln!(self.out, "}};").ok();
            }
            for (a, b) in &auto_crosses {
                self.pad(depth + 1);
                write!(
                    self.out,
                    "static const char* _auto_cross_labels_{cache_tag}_{}__{}[] = {{",
                    a.c_field, b.c_field
                )
                .ok();
                let mut first = true;
                for av in &a.values {
                    for bv in &b.values {
                        if !first {
                            write!(self.out, ", ").ok();
                        }
                        first = false;
                        write!(
                            self.out,
                            "\"{}.{}={} x {}.{}={}\"",
                            escape_c(ty),
                            escape_c(&a.field),
                            escape_c(&av.label),
                            escape_c(ty),
                            escape_c(&b.field),
                            escape_c(&bv.label)
                        )
                        .ok();
                    }
                }
                writeln!(self.out, "}};").ok();
            }
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static const harc_rt::random::HarcAutoCovPointMeta _auto_cov_points_{cache_tag}[] = {{"
            )
            .ok();
            for goal in &auto_goals {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "{{_auto_cov_labels_{cache_tag}_{}, {}}},",
                    goal.c_field,
                    goal.values.len()
                )
                .ok();
            }
            self.pad(depth + 1);
            writeln!(self.out, "}};").ok();
            if !auto_crosses.is_empty() {
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "static const harc_rt::random::HarcAutoCovCrossMeta _auto_cov_crosses_{cache_tag}[] = {{"
                )
                .ok();
                for (a, b) in &auto_crosses {
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "{{_auto_cross_labels_{cache_tag}_{}__{}, {}, {}}},",
                        a.c_field,
                        b.c_field,
                        a.values.len(),
                        b.values.len()
                    )
                    .ok();
                }
                self.pad(depth + 1);
                writeln!(self.out, "}};").ok();
            }
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static const harc_rt::random::HarcAutoCovPlan _auto_cov_plan_{cache_tag} = {{\"{}\", {}, _auto_cov_points_{cache_tag}, {}, {}, {}}};",
                escape_c(ty),
                target.span.start,
                auto_goals.len(),
                if auto_crosses.is_empty() {
                    "nullptr".to_string()
                } else {
                    format!("_auto_cov_crosses_{cache_tag}")
                },
                auto_crosses.len()
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static harc_rt::random::HarcAutoCovState _auto_cov_state_{cache_tag};"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_init(_auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag});"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "static bool _auto_cov_report_registered_{cache_tag} = false;"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_register_report(_auto_cov_report_registered_{cache_tag}, _auto_cov_reports, [&]() {{"
            )
            .ok();
            self.pad(depth + 2);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_report(_auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag});"
            )
            .ok();
            self.pad(depth + 1);
            writeln!(self.out, "}});").ok();
        }

        // Seeded preference values. These are hard clauses only for the first
        // check; if the preferred tuple is incompatible with the user's
        // constraints, we drop the preference stack and fall back to the
        // base solve. This makes solver-backed randomize
        // consume the HARC seed without turning preferences into false UNSATs.
        for (pref_idx, f) in free_fields.iter().enumerate() {
            let c_name = c_ident(&f.name);
            self.pad(depth + 1);
            let dist_entries = dist_directives
                .get(&f.name)
                .map(Vec::as_slice)
                .or_else(|| field_attr_dist_entries(f).map(Vec::as_slice));
            if let Some(entries) = dist_entries {
                if f.width > 64 {
                    self.errors.push(format!(
                        "randomize({ty}) [dist] on >64-bit field `{}` is not supported yet",
                        f.name
                    ));
                }
                write!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = (uint64_t)harc_rt::random::harc_prefer_dist(_harc_rt_seed, {}, {{",
                    c_name, pref_idx
                )
                .ok();
                self.emit_rng_dist_entries(entries);
                writeln!(self.out, "}});").ok();
            } else if let Some(n) = f.enum_variants {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = (uint64_t)harc_rt::random::harc_prefer_range(_harc_rt_seed, {}, 0, {});",
                    c_name,
                    pref_idx,
                    n.saturating_sub(1)
                )
                .ok();
            } else if f.signed && f.width > 0 && f.width < 63 {
                writeln!(
                    self.out,
                    "int64_t _pref_{cache_tag}_{} = harc_rt::random::harc_prefer_sint(_harc_rt_seed, {}, {});",
                    c_name,
                    pref_idx,
                    f.width
                )
                .ok();
            } else if f.width <= 64 {
                writeln!(
                    self.out,
                    "uint64_t _pref_{cache_tag}_{} = harc_rt::random::harc_prefer_uint(_harc_rt_seed, {}, {});",
                    c_name, pref_idx, f.width
                )
                .ok();
            } else {
                let value_ty = txn_field_solver_c_type(f);
                let expr = emit_random_pref_expr(f, pref_idx);
                writeln!(
                    self.out,
                    "{} _pref_{cache_tag}_{} = {};",
                    value_ty, c_name, expr
                )
                .ok();
            }
        }
        if !auto_goals.is_empty() {
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::HarcAutoCovSelection _auto_cov_selection_{cache_tag};   // auto coverage preference"
            )
            .ok();
        }
        for (group, (a, b)) in auto_crosses.iter().enumerate() {
            self.pad(depth + 1);
            let a_ty = auto_value_array_type(a, &field_info);
            writeln!(
                self.out,
                "static const {} _auto_cross_vals_{cache_tag}_{}__{}_{}[] = {{{}}};",
                a_ty,
                a.c_field,
                b.c_field,
                a.c_field,
                auto_value_initializer(&a.values)
            )
            .ok();
            self.pad(depth + 1);
            let b_ty = auto_value_array_type(b, &field_info);
            writeln!(
                self.out,
                "static const {} _auto_cross_vals_{cache_tag}_{}__{}_{}[] = {{{}}};",
                b_ty,
                a.c_field,
                b.c_field,
                b.c_field,
                auto_value_initializer(&b.values)
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_apply_cross_preference(_auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag}, _auto_cov_selection_{cache_tag}, {group}, _auto_cross_vals_{cache_tag}_{}__{}_{}, _auto_cross_vals_{cache_tag}_{}__{}_{}, _pref_{cache_tag}_{}, _pref_{cache_tag}_{});",
                a.c_field,
                b.c_field,
                a.c_field,
                a.c_field,
                b.c_field,
                b.c_field,
                a.c_field,
                b.c_field
            )
            .ok();
        }
        for (group, goal) in auto_goals.iter().enumerate() {
            self.pad(depth + 1);
            let value_ty = auto_value_array_type(goal, &field_info);
            writeln!(
                self.out,
                "static const {} _auto_point_vals_{cache_tag}_{}[] = {{{}}};",
                value_ty,
                goal.c_field,
                auto_value_initializer(&goal.values)
            )
            .ok();
            self.pad(depth + 1);
            writeln!(
                self.out,
                "harc_rt::random::harc_auto_cov_apply_point_preference(_auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag}, _auto_cov_selection_{cache_tag}, {group}, _auto_point_vals_{cache_tag}_{}, _pref_{cache_tag}_{});",
                goal.c_field, goal.c_field
            )
            .ok();
        }
        self.pad(depth + 1);
        writeln!(
            self.out,
            "harc_rt::random::HarcSolverRetryPolicy _harc_rt_retry_policy;"
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "_s.push();   // seeded candidate preferences (free fields only)"
        )
        .ok();
        for f in &free_fields {
            let c_name = c_ident(&f.name);
            self.pad(depth + 1);
            let pref_expr = format!("_pref_{cache_tag}_{c_name}");
            writeln!(
                self.out,
                "_s.add(_z_{} == {});",
                c_name,
                solver_bv_value_call(&pref_expr, f.signed, f.width, solver_width)
            )
            .ok();
        }
        self.pad(depth + 1);
        writeln!(self.out, "auto _r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (harc_rt::random::harc_retry_without_preferences(_harc_rt_retry_policy, _r == z3::sat)) {{"
        )
        .ok();
        if !auto_goals.is_empty() {
            for (group, (_a, _b)) in auto_crosses.iter().enumerate() {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "harc_rt::random::harc_auto_cov_mark_selected_cross_blocked(_auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag}, _auto_cov_selection_{cache_tag}, {group});"
                )
                .ok();
            }
            for (group, _goal) in auto_goals.iter().enumerate() {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "harc_rt::random::harc_auto_cov_mark_selected_point_blocked(_auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag}, _auto_cov_selection_{cache_tag}, {group});"
                )
                .ok();
            }
        }
        self.pad(depth + 2);
        writeln!(self.out, "_s.pop();").ok();
        self.pad(depth + 2);
        writeln!(self.out, "_s.push();   // retry without seeded preferences").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        for f in free_fields
            .iter()
            .filter(|f| unique_fields.contains(&f.name))
        {
            let c_name = c_ident(&f.name);
            let scope = c_scope_ident(field_attr_unique_scope(f));
            self.pad(depth + 1);
            let v_expr = solver_bv_value_call("_v", f.signed, f.width, solver_width);
            writeln!(
                self.out,
                "for (auto _v : harc_rt::random::harc_unique_values({cache_tag}_unique_{scope}_{})) _s.add(_z_{} != {});   // [unique within {}] policy: no repeat until exhausted",
                c_name,
                c_name,
                v_expr,
                escape_c(field_attr_unique_scope(f))
            )
            .ok();
        }
        // Final check: seeded preferences have already been dropped if they
        // conflict. If `[unique]` history saturates the satisfiable space,
        // clear that history and retry under the original hard constraints.
        self.pad(depth + 1);
        writeln!(self.out, "_r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if (harc_rt::random::harc_retry_without_unique_history(_harc_rt_retry_policy, _r == z3::sat)) {{"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "_s.pop();").ok();
        for f in free_fields
            .iter()
            .filter(|f| unique_fields.contains(&f.name))
        {
            let c_name = c_ident(&f.name);
            let scope = c_scope_ident(field_attr_unique_scope(f));
            self.pad(depth + 2);
            writeln!(
                self.out,
                "harc_rt::random::harc_unique_clear({cache_tag}_unique_{scope}_{});",
                c_name
            )
            .ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "_r = _s.check();").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();

        // Assign model values back into t.
        self.pad(depth + 1);
        writeln!(self.out, "if (_r == z3::sat) {{").ok();
        self.pad(depth + 2);
        writeln!(self.out, "z3::model _m = _s.get_model();").ok();
        let field_root_names: std::collections::HashSet<String> = field_info
            .keys()
            .filter_map(|name| name.split('.').next().map(str::to_string))
            .collect();
        for f in &fields {
            let c_name = c_ident(&f.name);
            if f.non_random {
                continue;
            }
            if let Some(list) = &f.list {
                let max_len = list_unroll_bounds.get(&f.name).copied().unwrap_or(0);
                let guard_expr = f.when_guard.as_ref().map(|guard| {
                    self.enum_variants_as_ints(&prefix_record_keep_expr(
                        guard,
                        target,
                        &field_root_names,
                    ))
                });
                let list_depth = if guard_expr.is_some() {
                    depth + 3
                } else {
                    depth + 2
                };
                if let Some(guard) = &guard_expr {
                    self.pad(depth + 2);
                    write!(self.out, "if (").ok();
                    self.emit_expr(guard);
                    writeln!(self.out, ") {{   // active when-subtype field {}", f.name).ok();
                }
                self.pad(list_depth);
                writeln!(
                    self.out,
                    "z3::expr _eval_{}_len = _m.eval(_z_{}_len, true).simplify();",
                    c_name, c_name
                )
                .ok();
                self.pad(list_depth);
                writeln!(self.out, "uint64_t _raw_{}_len = 0;", c_name).ok();
                self.pad(list_depth);
                writeln!(
                    self.out,
                    "if (!_eval_{}_len.is_numeral_u64(_raw_{}_len)) {{",
                    c_name, c_name
                )
                .ok();
                self.pad(list_depth + 1);
                writeln!(
                    self.out,
                    "sim_log_line(\"FAIL\", \"randomize(t) with: solver model for field '{}.len' is not a uint64 numeral\");",
                    escape_c(&f.name)
                )
                .ok();
                self.pad(list_depth + 1);
                writeln!(self.out, "ctx.errors++;").ok();
                self.pad(list_depth);
                writeln!(self.out, "}}").ok();
                self.pad(list_depth);
                writeln!(
                    self.out,
                    "if (_raw_{}_len > {}) _raw_{}_len = {};",
                    c_name, max_len, c_name, max_len
                )
                .ok();
                self.pad(list_depth);
                self.emit_target_field_access(target, f);
                writeln!(self.out, ".resize((size_t)_raw_{}_len);", c_name).ok();
                for i in 0..max_len {
                    self.pad(list_depth);
                    writeln!(self.out, "if (_raw_{}_len > {i}) {{", c_name).ok();
                    self.pad(list_depth + 1);
                    writeln!(
                        self.out,
                        "z3::expr _eval_{}_{i} = _m.eval(_z_{}_{i}, true).simplify();",
                        c_name, c_name
                    )
                    .ok();
                    self.pad(list_depth + 1);
                    if list.elem_width <= 64 {
                        let val_ty = solver_scalar_c_type(list.elem_width, list.elem_signed);
                        writeln!(
                            self.out,
                            "uint64_t _raw_{}_{i} = harc_z3_bv_low_u64(_ctx, _eval_{}_{i});",
                            c_name, c_name
                        )
                        .ok();
                        self.pad(list_depth + 1);
                        self.emit_target_field_access(target, f);
                        writeln!(self.out, "[{i}] = ({})_raw_{}_{i};", val_ty, c_name).ok();
                    } else {
                        let val_ty = solver_scalar_c_type(list.elem_width, list.elem_signed);
                        let words = list.elem_width.div_ceil(32);
                        writeln!(
                            self.out,
                            "const char* _bin_{}_{i} = Z3_get_numeral_binary_string(_ctx, _eval_{}_{i});",
                            c_name, c_name
                        )
                        .ok();
                        self.pad(list_depth + 1);
                        writeln!(self.out, "if (_bin_{}_{i} == nullptr) {{", c_name).ok();
                        self.pad(list_depth + 2);
                        writeln!(
                            self.out,
                            "sim_log_line(\"FAIL\", \"randomize(t) with: solver model for field '{}[{i}]' is not a binary numeral\");",
                            escape_c(&f.name)
                        )
                        .ok();
                        self.pad(list_depth + 2);
                        writeln!(self.out, "ctx.errors++;").ok();
                        self.pad(list_depth + 1);
                        writeln!(self.out, "}}").ok();
                        self.pad(list_depth + 1);
                        self.emit_target_field_access(target, f);
                        writeln!(
                            self.out,
                            "[{i}] = ({})harc_rt::harc_wide_from_binary<{}>(_bin_{}_{i});",
                            val_ty, words, c_name
                        )
                        .ok();
                    }
                    self.pad(list_depth);
                    writeln!(self.out, "}}").ok();
                }
                if guard_expr.is_some() {
                    self.pad(depth + 2);
                    writeln!(self.out, "}}").ok();
                }
                continue;
            }
            if f.width == 0 {
                continue;
            }
            let guard_expr = f.when_guard.as_ref().map(|guard| {
                self.enum_variants_as_ints(&prefix_record_keep_expr(
                    guard,
                    target,
                    &field_root_names,
                ))
            });
            let scalar_depth = if guard_expr.is_some() {
                depth + 3
            } else {
                depth + 2
            };
            if let Some(guard) = &guard_expr {
                self.pad(depth + 2);
                write!(self.out, "if (").ok();
                self.emit_expr(guard);
                writeln!(self.out, ") {{   // active when-subtype field {}", f.name).ok();
            }
            // Every declared field is assigned from the model — equality-
            // pinned fields take their constrained value; free fields take
            // a Z3-chosen satisfying value. Only explicit `[unique]` fields
            // get pushed into persistent no-repeat history.
            self.pad(scalar_depth);
            writeln!(
                self.out,
                "z3::expr _eval_{} = _m.eval(_z_{}, true).simplify();",
                c_name, c_name
            )
            .ok();
            if f.width <= 64 {
                let val_ty = if f.signed { "int64_t" } else { "uint64_t" };
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "uint64_t _raw_{} = harc_z3_bv_low_u64(_ctx, _eval_{});",
                    c_name, c_name
                )
                .ok();
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "{} _val_{} = ({})_raw_{};",
                    val_ty, c_name, val_ty, c_name
                )
                .ok();
            } else {
                let val_ty = txn_field_solver_c_type(f);
                let words = f.width.div_ceil(32);
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "const char* _bin_{} = Z3_get_numeral_binary_string(_ctx, _eval_{});",
                    c_name, c_name
                )
                .ok();
                self.pad(scalar_depth);
                writeln!(self.out, "if (_bin_{} == nullptr) {{", c_name).ok();
                self.pad(scalar_depth + 1);
                writeln!(
                    self.out,
                    "sim_log_line(\"FAIL\", \"randomize(t) with: solver model for field '{}' is not a binary numeral\");",
                    escape_c(&f.name)
                )
                .ok();
                self.pad(scalar_depth + 1);
                writeln!(self.out, "ctx.errors++;").ok();
                self.pad(scalar_depth);
                writeln!(self.out, "}}").ok();
                self.pad(scalar_depth);
                writeln!(
                    self.out,
                    "{} _val_{} = ({})harc_rt::harc_wide_from_binary<{}>(_bin_{});",
                    val_ty, c_name, val_ty, words, c_name
                )
                .ok();
            }
            self.pad(scalar_depth);
            write!(self.out, "").ok();
            self.emit_target_field_access(target, f);
            writeln!(self.out, " = _val_{};", c_name).ok();
            if !pinned.contains(&f.name) {
                if f.when_guard.is_none() && unique_fields.contains(&f.name) {
                    let scope = c_scope_ident(field_attr_unique_scope(f));
                    self.pad(scalar_depth);
                    writeln!(
                        self.out,
                        "harc_rt::random::harc_unique_remember({cache_tag}_unique_{scope}_{}, _val_{});",
                        c_name, c_name
                    )
                    .ok();
                }
            }
            if guard_expr.is_some() {
                self.pad(depth + 2);
                writeln!(self.out, "}}").ok();
            }
        }
        for (group, goal) in auto_goals.iter().enumerate() {
            for (idx, value) in goal.values.iter().enumerate() {
                self.pad(depth + 2);
                writeln!(
                    self.out,
                    "harc_rt::random::harc_auto_cov_mark_value_hit(_val_{}, {}, _auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag}, {}, {});",
                    goal.c_field, value.c_expr, group, idx
                )
                .ok();
            }
        }
        for (group, (a, b)) in auto_crosses.iter().enumerate() {
            for (i, av) in a.values.iter().enumerate() {
                for (j, bv) in b.values.iter().enumerate() {
                    self.pad(depth + 2);
                    writeln!(
                        self.out,
                        "harc_rt::random::harc_auto_cov_mark_cross_hit(_val_{}, {}, _val_{}, {}, _auto_cov_plan_{cache_tag}, _auto_cov_state_{cache_tag}, {}, {}, {});",
                        a.c_field, av.c_expr, b.c_field, bv.c_expr, group, i, j
                    )
                    .ok();
                }
            }
        }
        self.emit_randomize_trace_event(ty, target, depth + 2);
        self.pad(depth + 2);
        writeln!(self.out, "return harc_rt::random::harc_solve_status_ok();").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}} else {{").ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "auto _harc_rt_status = harc_rt::random::harc_solve_status_unsat(_harc_rt_problem ? _harc_rt_problem->id : 0, _harc_rt_seed);"
        )
        .ok();
        self.pad(depth + 2);
        writeln!(
            self.out,
            "sim_log_line(\"FAIL\", \"%s\", _harc_rt_status.message ? _harc_rt_status.message : \"randomize(t) with: constraint UNSAT\");"
        )
        .ok();
        let mut constraint_origins: Vec<String> = hard_constraints
            .iter()
            .map(|expr| format!("constraint `{}`", expr_source_str(expr)))
            .collect();
        for f in &fields {
            if field_attr_range(f).is_some() {
                constraint_origins.push(format!("field attribute `[range]` on `{ty}.{}`", f.name));
            }
        }
        constraint_origins.sort();
        constraint_origins.dedup();
        for origin in constraint_origins {
            self.pad(depth + 2);
            writeln!(
                self.out,
                "sim_log_line(\"FAIL\", \"randomize(t) with: {} participated in the solve\");",
                escape_c(&origin)
            )
            .ok();
        }
        let mut when_guards: Vec<String> = fields
            .iter()
            .filter_map(|f| f.when_guard.as_ref().map(expr_source_str))
            .collect();
        when_guards.extend(
            hard_constraints
                .iter()
                .filter_map(|expr| guarded_constraint_guard(expr).map(expr_source_str)),
        );
        when_guards.sort();
        when_guards.dedup();
        for guard in when_guards {
            self.pad(depth + 2);
            writeln!(
                self.out,
                "sim_log_line(\"FAIL\", \"randomize(t) with: active when subtype guard `{}` participated in the solve\");",
                escape_c(&guard)
            )
            .ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "ctx.errors++;").ok();
        self.pad(depth + 2);
        writeln!(self.out, "return _harc_rt_status;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}};").ok();
        self.pad(depth + 1);
        write!(
            self.out,
            "auto _harc_rt_solve_status = harc_rt::random::harc_solve_constrained("
        )
        .ok();
        self.emit_expr(target);
        writeln!(
            self.out,
            ", _harc_rt_problem ? _harc_rt_problem->id : 0, _harc_rt_seed, harc_rt::random::HarcSolveMode::{}, _harc_rt_generated_solver);",
            if blocking { "Blocking" } else { "Queued" }
        )
        .ok();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "(void)harc_rt::random::harc_handle_solve_status(_harc_rt_solve_status);"
        )
        .ok();

        self.pad(depth);
        writeln!(self.out, "}}").ok();
    }

    /// Operand bit-width of a constraint expression, for the `+% -% *%`
    /// mask. `None` when no operand carries a width — the same "wrap width
    /// undefined" condition both emitters reject at the statement level.
    ///
    /// Each arm mirrors the shape the *emitter* resolves at that position,
    /// including the `Ident` fallback chain (field, then `const`, then enum
    /// variant, then a blocking `let`). Two ways to get this wrong, both
    /// found in review:
    ///
    /// - Resolving a `Field` by its leaf `name` finds an unrelated
    ///   top-level field of the same name, so `hdr.len : uint<16>` next to
    ///   a `len : uint<8>` masks to 8 bits. The solver then returns a value
    ///   that does not satisfy the source constraint — silent, and worse
    ///   than the unsatisfiable report this mask exists to prevent. Dotted
    ///   paths go through `expr_field_path`, exactly as the emitter does.
    /// - Resolving *less* than the emitter turns a constraint the emitter
    ///   would have emitted into a hard build error.
    ///
    /// `sum(...)` and `.len()` are deliberately absent: the emitter turns
    /// them into solver-internal variables with no declared source width,
    /// so there is no honest mask to apply and rejecting is the only
    /// correct answer.
    fn constraint_expr_width(
        &self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
    ) -> Option<u32> {
        let recur = |inner| self.constraint_expr_width(inner, field_info, target_root);
        match &*e.kind {
            // A bare ident is its own whole path — resolve it the way the
            // emitter's `Ident` arm does, NOT through `expr_field_path`,
            // whose `target_root` strip would erase a field named after
            // the randomize target (`randomize(t) with t +% 10 == 5`).
            ExprKind::Ident(id) => {
                if let Some(info) = field_info.get(&id.name) {
                    return Some(txn_field_solver_width(info));
                }
                if let Some(width) = self.const_widths.get(&id.name) {
                    return Some(*width);
                }
                if let Some(idx) = self.enum_variants.get(&id.name) {
                    return u64::try_from(*idx).ok().map(value_bit_width);
                }
                None
            }
            ExprKind::Field { .. } => expr_field_path(e, target_root)
                .and_then(|field| field_info.get(&field))
                .map(txn_field_solver_width),
            // `items[i]` — a list element has the list's element width.
            // A `foreach` clause is unrolled into exactly this shape, so
            // without it every `foreach` constraint containing a wrap
            // would be rejected for an unknown width. A bit-select on a
            // scalar field is one bit.
            ExprKind::Index { target, .. } => {
                if let Some(field) = list_field_name_from_expr(target, field_info, target_root) {
                    return field_info.get(&field).map(txn_field_solver_width);
                }
                expr_field_path(target, target_root)
                    .and_then(|field| field_info.get(&field))
                    .filter(|info| info.list.is_none() && info.width != 0)
                    .map(|_| 1)
            }
            ExprKind::BitSlice { hi, lo, .. } => {
                let hi = const_usize_expr(hi)?;
                let lo = const_usize_expr(lo)?;
                (hi >= lo).then(|| (hi - lo + 1) as u32)
            }
            ExprKind::Paren(inner) => recur(inner),
            ExprKind::Cast { ty, .. } => crate::ir::lower::cast_relabel_width(ty),
            ExprKind::Int(s) => literal_operand_bit_width(s),
            ExprKind::Binary {
                op: BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap,
                lhs,
                rhs,
            } => Some(recur(lhs)?.max(recur(rhs)?)),
            _ => None,
        }
    }

    /// Signedness of a constraint expression, picking `<` vs `z3::ult`,
    /// `/` vs `z3::udiv` and `%` vs `z3::urem`.
    ///
    /// Resolves *transaction fields* the way `constraint_expr_width` does
    /// — dotted paths through `expr_field_path` — and for the same reason:
    /// these are different Z3 predicates over the *same* variable, so a
    /// wrong hit is a wrong solved value, not a cosmetic difference. A
    /// field whose width equals `solver_width` carries no
    /// `ult(_z_x, 1<<W)` range assumption at all (that bound is only
    /// emitted when the field is narrower), so under `bvslt` the solver is
    /// free to return a value the source constraint forbids.
    ///
    /// It does NOT have the width oracle's `const`/enum/`let` fallback
    /// chain. A `const` and an enum variant are always emitted as
    /// non-negative `uint64_t`, so `false` is right for those. A signed
    /// `let` under `blocking randomize` is a real gap — `t.x < s` on a
    /// `sint<8>` compares unsigned — but it predates this path and the
    /// fix needs a signedness table for locals, which was removed once
    /// already for poisoning unrelated functions (harc#550). Tracked in
    /// harc#563; do not paper over it with a fourth flat side table.
    fn constraint_expr_is_signed(
        &self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
    ) -> bool {
        let recur = |inner| self.constraint_expr_is_signed(inner, field_info, target_root);
        match &*e.kind {
            ExprKind::Ident(id) => field_info.get(&id.name).map(|f| f.signed).unwrap_or(false),
            ExprKind::Field { .. } => expr_field_path(e, target_root)
                .and_then(|field| field_info.get(&field))
                .map(|f| f.signed)
                .unwrap_or(false),
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => recur(inner),
            ExprKind::Binary { lhs, rhs, .. } => recur(lhs) || recur(rhs),
            ExprKind::Membership { expr, .. } => recur(expr),
            _ => false,
        }
    }

    /// Translate a HARC expression to a z3++ C++ expression. Field accesses
    /// `t.<name>` resolve to the per-field `_z_<name>` Z3 var declared in
    /// the surrounding solver block. Integer literals become `_ctx.bv_val(N, W)`
    /// at the field's width inferred from context. v0 is permissive — any
    /// untranslatable form falls back to a comment + `_ctx.bool_val(true)`.
    fn emit_constraint_expr_with_list_bounds(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        self.emit_constraint_expr_w(
            e,
            field_info,
            width,
            target_root,
            blocking,
            &list_unroll_bounds,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_foreach_constraint_clauses(
        &mut self,
        ty: &str,
        var: &Ident,
        iter: &Expr,
        body: &[Expr],
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
        solver_width: u32,
        depth: usize,
    ) {
        let Some(field) = list_field_name_from_expr(iter, field_info, target_root) else {
            self.errors.push(format!(
                "randomize({ty}) foreach constraint must iterate a list field of transaction `{ty}`"
            ));
            return;
        };
        let Some(max_len) = list_unroll_bounds.get(&field).copied() else {
            self.errors.push(format!(
                "randomize({ty}) foreach constraint over `{field}` needs a bounded length constraint such as `{field}.len() <= N`"
            ));
            return;
        };
        let c_field = c_ident(&field);
        for i in 0..max_len {
            let item = Expr::new(
                ExprKind::Index {
                    target: iter.clone(),
                    index: Expr::new(ExprKind::Int(i.to_string()), var.span),
                },
                iter.span,
            );
            let mut subst = std::collections::HashMap::new();
            subst.insert(var.name.clone(), item);
            for clause in body {
                let lowered = substitute_idents(clause, &subst);
                self.pad(depth);
                write!(
                    self.out,
                    "_s.add(z3::ule(_z_{}_len, _ctx.bv_val((uint64_t){i}, {})) || (",
                    c_field, solver_width
                )
                .ok();
                self.emit_constraint_expr_with_list_bounds(
                    &lowered,
                    field_info,
                    solver_width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                writeln!(self.out, "));").ok();
            }
        }
    }

    fn try_emit_constraint_list_call(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) -> bool {
        if let Some(field) = list_len_call_field_name(e, field_info, target_root) {
            write!(self.out, "_z_{}_len", c_ident(&field)).ok();
            return true;
        }

        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Ident(name) = &*callee.kind else {
            return false;
        };
        if name.name != "sum" || args.len() != 1 {
            return false;
        }
        let CallArg::Expr(arg) = &args[0] else {
            return false;
        };

        let (field, lo, hi) = match &*arg.kind {
            ExprKind::Index { target, index } => {
                let Some(field) = list_field_name_from_expr(target, field_info, target_root) else {
                    return false;
                };
                match &*index.kind {
                    ExprKind::RangeLit { lo, hi } => (field, lo.as_ref(), hi.as_ref()),
                    _ => {
                        self.errors.push(
                            "`sum(list[index])` constraints require a range slice like `sum(items[0..items.len()])`".into(),
                        );
                        write!(self.out, "_ctx.bool_val(true)").ok();
                        return true;
                    }
                }
            }
            _ => {
                let Some(field) = list_field_name_from_expr(arg, field_info, target_root) else {
                    return false;
                };
                (field, None, None)
            }
        };
        self.emit_constraint_list_sum(
            &field,
            lo,
            hi,
            field_info,
            width,
            target_root,
            list_unroll_bounds,
        );
        true
    }

    fn emit_constraint_list_sum(
        &mut self,
        field: &str,
        lo: Option<&Expr>,
        hi: Option<&Expr>,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        let Some(_) = field_info.get(field).and_then(|f| f.list.as_ref()) else {
            write!(self.out, "_ctx.bv_val((uint64_t)0, {width})").ok();
            return;
        };
        let max_len = list_unroll_bounds.get(field).copied().unwrap_or(0);
        let c_field = c_ident(field);
        let lo_const = lo.and_then(const_usize_expr).unwrap_or(0);
        let hi_const = hi.and_then(const_usize_expr);
        let hi_is_len = hi
            .and_then(|h| list_len_call_field_name(h, field_info, target_root))
            .as_deref()
            == Some(field);
        write!(self.out, "(").ok();
        let mut emitted = false;
        for i in 0..max_len {
            if i < lo_const {
                continue;
            }
            if let Some(hi) = hi_const {
                if i >= hi {
                    continue;
                }
            }
            if emitted {
                write!(self.out, " + ").ok();
            }
            if hi_is_len || hi.is_none() {
                write!(
                    self.out,
                    "z3::ite(z3::ugt(_z_{}_len, _ctx.bv_val((uint64_t){i}, {})), _z_{}_{i}, _ctx.bv_val((uint64_t)0, {}))",
                    c_field, width, c_field, width
                )
                .ok();
            } else {
                write!(self.out, "_z_{}_{i}", c_field).ok();
            }
            emitted = true;
        }
        if !emitted {
            write!(self.out, "_ctx.bv_val((uint64_t)0, {width})").ok();
        }
        write!(self.out, ")").ok();
    }

    fn emit_constraint_bool_expr_w(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        match &*e.kind {
            ExprKind::Bool(b) => {
                write!(
                    self.out,
                    "_ctx.bool_val({})",
                    if *b { "true" } else { "false" }
                )
                .ok();
                return;
            }
            ExprKind::Ident(id) => {
                if field_info.get(&id.name).is_some_and(|f| {
                    f.list.is_none() && f.enum_variants.is_none() && f.width == 1 && !f.signed
                }) {
                    write!(
                        self.out,
                        "_z_{} != _ctx.bv_val((uint64_t)0, {})",
                        c_ident(&id.name),
                        width
                    )
                    .ok();
                    return;
                }
            }
            ExprKind::Field { .. } => {
                if let Some(field) = expr_field_path(e, target_root) {
                    if field_info.get(&field).is_some_and(|f| {
                        f.list.is_none() && f.enum_variants.is_none() && f.width == 1 && !f.signed
                    }) {
                        write!(
                            self.out,
                            "_z_{} != _ctx.bv_val((uint64_t)0, {})",
                            c_ident(&field),
                            width
                        )
                        .ok();
                        return;
                    }
                }
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_constraint_bool_expr_w(
                    inner,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, ")").ok();
                return;
            }
            ExprKind::Unary {
                op: UnaryOp::Not | UnaryOp::NotKw,
                expr,
            } => {
                write!(self.out, "!(").ok();
                self.emit_constraint_bool_expr_w(
                    expr,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, ")").ok();
                return;
            }
            ExprKind::Binary {
                op: BinaryOp::AndAnd | BinaryOp::AndKw,
                lhs,
                rhs,
            } => {
                self.emit_constraint_bool_expr_w(
                    lhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, " && ").ok();
                self.emit_constraint_bool_expr_w(
                    rhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                return;
            }
            ExprKind::Binary {
                op: BinaryOp::OrOr | BinaryOp::OrKw,
                lhs,
                rhs,
            } => {
                self.emit_constraint_bool_expr_w(
                    lhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, " || ").ok();
                self.emit_constraint_bool_expr_w(
                    rhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                return;
            }
            _ => {}
        }
        self.emit_constraint_expr_w(
            e,
            field_info,
            width,
            target_root,
            blocking,
            list_unroll_bounds,
        );
    }

    fn emit_constraint_bit_slice_expr(
        &mut self,
        target: &Expr,
        hi: usize,
        lo: usize,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        let slice_width = (hi - lo + 1) as u32;
        let mask = solver_unsigned_mask_expr(slice_width);
        write!(self.out, "(").ok();
        if lo == 0 {
            self.emit_constraint_expr_w(
                target,
                field_info,
                width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
        } else {
            write!(self.out, "z3::lshr(").ok();
            self.emit_constraint_expr_w(
                target,
                field_info,
                width,
                target_root,
                blocking,
                list_unroll_bounds,
            );
            write!(self.out, ", _ctx.bv_val((uint64_t){lo}, {width}))").ok();
        }
        write!(self.out, " & harc_z3_bv_value(_ctx, {mask}, {width}))").ok();
    }

    fn emit_constraint_expr_w(
        &mut self,
        e: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        match &*e.kind {
            ExprKind::ForEachConstraint { .. } => {
                self.errors.push(
                    "foreach constraints are only supported as top-level `randomize ... with` clauses".into(),
                );
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
            ExprKind::Call { .. } => {
                if self.try_emit_constraint_list_call(
                    e,
                    field_info,
                    width,
                    target_root,
                    list_unroll_bounds,
                ) {
                    return;
                }
                self.errors
                    .push("constraint function call not supported in v0 solver path".into());
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
            ExprKind::Index { target, index } => {
                if let Some(field) = list_field_name_from_expr(target, field_info, target_root) {
                    if let Some(i) = const_usize_expr(index) {
                        if field_info
                            .get(&field)
                            .and_then(|f| f.list.as_ref())
                            .is_some()
                        {
                            if i < list_unroll_bounds.get(&field).copied().unwrap_or(0) {
                                write!(self.out, "_z_{}_{i}", c_ident(&field)).ok();
                                return;
                            }
                        }
                    }
                    self.errors.push(
                        "constraint list indexing requires a constant index within the list max length".into(),
                    );
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                }
                if let Some(field) = expr_field_path(target, target_root) {
                    if let Some(info) = field_info.get(&field) {
                        if info.list.is_none() && info.width != 0 {
                            let Some(i) = const_usize_expr(index) else {
                                self.errors
                                    .push("constraint bit-select index must be constant".into());
                                write!(self.out, "_ctx.bool_val(true)").ok();
                                return;
                            };
                            if i as u32 >= info.width {
                                self.errors.push(format!(
                                    "constraint bit-select index {i} out of range for field `{field}`"
                                ));
                                write!(self.out, "_ctx.bool_val(true)").ok();
                                return;
                            }
                            self.emit_constraint_bit_slice_expr(
                                target,
                                i,
                                i,
                                field_info,
                                width,
                                target_root,
                                blocking,
                                list_unroll_bounds,
                            );
                            return;
                        }
                    }
                }
                self.errors
                    .push("constraint index expression not supported in v0 solver path".into());
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
            ExprKind::BitSlice { target, hi, lo } => {
                let Some(hi) = const_usize_expr(hi) else {
                    self.errors
                        .push("constraint bit-slice high bound must be constant".into());
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                };
                let Some(lo) = const_usize_expr(lo) else {
                    self.errors
                        .push("constraint bit-slice low bound must be constant".into());
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                };
                if hi < lo {
                    self.errors
                        .push("constraint bit-slice high bound must be >= low bound".into());
                    write!(self.out, "_ctx.bool_val(true)").ok();
                    return;
                }
                self.emit_constraint_bit_slice_expr(
                    target,
                    hi,
                    lo,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
            }
            // `t.<name>` → _z_<name>. Strip the `t.` prefix.
            ExprKind::Field { .. } => {
                if let Some(field) = expr_field_path(e, target_root) {
                    if let Some(info) = field_info.get(&field) {
                        if info.list.is_some() {
                            self.errors.push(format!(
                                "constraint references list field `{}` as a scalar; use `{}.len()`, `{}[index]`, or `sum({}[0..{}.len()])`",
                                field, field, field, field, field
                            ));
                            write!(self.out, "_ctx.bool_val(true)").ok();
                        } else if info.width == 0 {
                            self.errors.push(format!(
                                "constraint references composite field `{}` as a scalar; nested record constraint flattening is not supported yet",
                                field
                            ));
                            write!(self.out, "_ctx.bool_val(true)").ok();
                        } else {
                            write!(self.out, "_z_{}", c_ident(&field)).ok();
                        }
                    } else if blocking
                        && target_root
                            .zip(expr_field_root(e).as_deref())
                            .is_some_and(|(target, root)| root != target)
                    {
                        if width <= 64 {
                            write!(self.out, "_ctx.bv_val((uint64_t)(").ok();
                            self.emit_expr(e);
                            write!(self.out, "), {width})").ok();
                        } else {
                            write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                            self.emit_expr(e);
                            write!(self.out, ", {width})").ok();
                        }
                    } else {
                        self.errors.push(format!(
                            "constraint references unknown field `{}` (only fields of the randomize target are supported)",
                            field
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                    }
                } else if blocking {
                    if width <= 64 {
                        write!(self.out, "_ctx.bv_val((uint64_t)(").ok();
                        self.emit_expr(e);
                        write!(self.out, "), {width})").ok();
                    } else {
                        write!(self.out, "harc_z3_bv_value(_ctx, ").ok();
                        self.emit_expr(e);
                        write!(self.out, ", {width})").ok();
                    }
                } else {
                    self.errors.push(
                        "constraint references unsupported field path (only fields of the randomize target are supported)".into(),
                    );
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
            }
            ExprKind::Ident(id) => {
                // Bare ident: try fields first, then enum variants.
                // Lets a constraint like `keep op != WRAP` work — the
                // RHS isn't a field, it's the enum variant `WRAP`.
                if field_info.contains_key(&id.name) {
                    if field_info
                        .get(&id.name)
                        .and_then(|f| f.list.as_ref())
                        .is_some()
                    {
                        self.errors.push(format!(
                            "constraint references list field `{}` as a scalar; use `{}.len()`, `{}[index]`, or `sum({}[0..{}.len()])`",
                            id.name, id.name, id.name, id.name, id.name
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                    } else if field_info.get(&id.name).is_some_and(|f| f.width == 0) {
                        self.errors.push(format!(
                            "constraint references composite field `{}` as a scalar; nested record constraint flattening is not supported yet",
                            id.name
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                    } else {
                        write!(self.out, "_z_{}", c_ident(&id.name)).ok();
                    }
                } else if let Some(text) = self.consts.get(&id.name).cloned() {
                    if width <= 64 {
                        write!(
                            self.out,
                            "_ctx.bv_val((uint64_t){}, {})",
                            c_int_literal(&text),
                            width
                        )
                        .ok();
                    } else {
                        write!(
                            self.out,
                            "harc_z3_bv_value(_ctx, {}, {})",
                            c_int_literal(&text),
                            width
                        )
                        .ok();
                    }
                } else if let Some(idx) = self.enum_variants.get(&id.name).copied() {
                    if width <= 64 {
                        write!(self.out, "_ctx.bv_val((uint64_t){}, {})", idx, width).ok();
                    } else {
                        write!(
                            self.out,
                            "harc_z3_bv_value(_ctx, (uint64_t){}, {})",
                            idx, width
                        )
                        .ok();
                    }
                } else if blocking && self.let_widths.contains_key(&id.name) {
                    if width <= 64 {
                        write!(self.out, "_ctx.bv_val((uint64_t)({}), {})", id.name, width).ok();
                    } else {
                        write!(self.out, "harc_z3_bv_value(_ctx, {}, {})", id.name, width).ok();
                    }
                } else {
                    self.errors
                        .push(format!("constraint references unknown name `{}`", id.name));
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
            }
            ExprKind::Int(s) => {
                if width <= 64 {
                    write!(
                        self.out,
                        "_ctx.bv_val((uint64_t){}, {})",
                        c_int_literal(s),
                        width
                    )
                    .ok();
                } else {
                    write!(
                        self.out,
                        "harc_z3_bv_value(_ctx, {}, {})",
                        c_solver_int_literal(s),
                        width
                    )
                    .ok();
                }
            }
            ExprKind::Bool(b) => {
                write!(
                    self.out,
                    "_ctx.bv_val((uint64_t){}, {})",
                    if *b { 1 } else { 0 },
                    width
                )
                .ok();
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_constraint_expr_w(
                    inner,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, ")").ok();
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not | UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                if matches!(op, UnaryOp::Not | UnaryOp::NotKw) {
                    write!(self.out, "!(").ok();
                    self.emit_constraint_bool_expr_w(
                        expr,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, ")").ok();
                } else {
                    write!(self.out, "{s}").ok();
                    self.emit_constraint_expr_w(
                        expr,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                }
            }
            // `e in <range-or-set>` parses as ExprKind::Membership
            // (not Binary(In, …) — the `in` keyword is lexed as
            // TokenKind::In and lowered structurally). Expand to an
            // OR-chain of equality / range-comparison sub-expressions
            // the solver handles natively.
            ExprKind::Membership { expr, set } => {
                self.emit_constraint_membership(
                    expr,
                    set,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
            }
            ExprKind::Binary { op, lhs, rhs } => {
                use BinaryOp::*;
                // Defensive: BinaryOp::In/Inside isn't produced by the
                // current parser (Membership is used instead) but the
                // op variants exist, so keep the handler in case
                // future parser paths emit them.
                if matches!(op, In | Inside) {
                    self.emit_constraint_membership(
                        lhs,
                        rhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    return;
                }
                // Z3's operator overloads on `z3::expr` default to
                // *signed* semantics for `<`, `>`, `<=`, `>=`, `/`,
                // `%`. HARC fields are unsigned (uint<N> / bits<N>),
                // so we explicitly route those through the
                // unsigned-named functions: ult/ule/ugt/uge for
                // comparisons, udiv for division, urem for modulus.
                // C++ has no `%%` infix — the old `" %% "` mapping was
                // a half-finished placeholder. Equality (`==`, `!=`),
                // logical / bitwise / shift use the natural overloads.
                let signed = self.constraint_expr_is_signed(lhs, field_info, target_root)
                    || self.constraint_expr_is_signed(rhs, field_info, target_root);
                // `+% -% *%` mask the result to `max(W(lhs), W(rhs))`
                // (spec §2.4). The solver variable is a 64-bit bitvector
                // with the field's width carried as a separate range
                // assumption, so the wrap does NOT fall out of the
                // bitvector semantics — it has to be applied explicitly.
                // Without it `keep len +% 10 == 5` on a `uint<8>` field
                // was solved as `len + 10 == 5 && len < 256`, reported
                // unsatisfiable where `len = 251` is a solution.
                let wrap_mask = match op {
                    AddWrap | SubWrap | MulWrap => {
                        let spelling = match op {
                            SubWrap => "-%",
                            MulWrap => "*%",
                            _ => "+%",
                        };
                        match self.constraint_expr_width(e, field_info, target_root) {
                            // A wrap exactly as wide as the solver bitvector
                            // needs no mask. That bitvector is `solver_width`
                            // — `max(field widths).max(64)`, so it is >= 64,
                            // NOT always 64: a transaction with any field
                            // wider than 64 bits solves every constraint at
                            // that wider rank, and a 32-bit wrap there still
                            // needs its mask.
                            Some(w) if w == width => None,
                            // Wider than the solver bitvector: the residue is
                            // not representable, so there is no mask to
                            // apply. Rejected rather than silently emitted
                            // unmasked. Reachable two ways — a bit-slice
                            // (`emit_constraint_bit_slice_expr` does not
                            // bound `hi` by the field width, unlike the
                            // bit-select path) and a sized literal that
                            // declares more bits than the solver has
                            // (`128'h1` where every field fits in 64).
                            Some(w) => {
                                if w > width {
                                    self.errors.push(format!(
                                        "wrapping operator `{spelling}` in a constraint has a \
                                         {w}-bit result, wider than the {width}-bit solver \
                                         bitvector, so the §2.4 mask is not representable"
                                    ));
                                    None
                                } else {
                                    Some(solver_unsigned_mask_expr(w))
                                }
                            }
                            None => {
                                self.errors.push(format!(
                                    "wrapping operator `{spelling}` in a constraint needs both \
                                     operands to have a statically known bit-width so the §2.4 \
                                     mask is defined; use a transaction field, a `const`, an \
                                     enum variant, a sized literal (`8'hAB`) or a plain integer \
                                     literal. `sum(...)` and `.len()` have no declared width \
                                     and are not accepted here"
                                ));
                                None
                            }
                        }
                    }
                    _ => None,
                };
                if wrap_mask.is_some() {
                    // TWO parens: the inner one groups the arithmetic, the
                    // outer one guards the whole masked value. C++ binds
                    // `&` looser than `==`, so `(a + b) & mask == 5` would
                    // parse as `(a + b) & (mask == 5)`.
                    write!(self.out, "((").ok();
                }
                let (sep, fname) = match op {
                    Add | AddWrap => (" + ", None),
                    Sub | SubWrap => (" - ", None),
                    Mul | MulWrap => (" * ", None),
                    Div if signed => (" / ", None),
                    Div => ("", Some("udiv")),
                    Mod if signed => (" % ", None),
                    Mod => ("", Some("urem")),
                    Eq => (" == ", None),
                    Ne => (" != ", None),
                    Lt if signed => (" < ", None),
                    Lt => ("", Some("ult")),
                    Le if signed => (" <= ", None),
                    Le => ("", Some("ule")),
                    Gt if signed => (" > ", None),
                    Gt => ("", Some("ugt")),
                    Ge if signed => (" >= ", None),
                    Ge => ("", Some("uge")),
                    AndAnd | AndKw => {
                        self.emit_constraint_bool_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " && ").ok();
                        self.emit_constraint_bool_expr_w(
                            rhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        return;
                    }
                    OrOr | OrKw => {
                        self.emit_constraint_bool_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " || ").ok();
                        self.emit_constraint_bool_expr_w(
                            rhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        return;
                    }
                    BitAnd => (" & ", None),
                    BitOr => (" | ", None),
                    BitXor => (" ^ ", None),
                    Shl => (" << ", None),
                    Shr => (" >> ", None),
                    _ => {
                        self.errors.push(format!(
                            "constraint operator `{:?}` not supported in v0 solver path",
                            op
                        ));
                        write!(self.out, "_ctx.bool_val(true)").ok();
                        return;
                    }
                };
                if let Some(fn_name) = fname {
                    // Unsigned op via z3::<fn>(lhs, rhs).
                    write!(self.out, "z3::{fn_name}(").ok();
                    self.emit_constraint_expr_w(
                        lhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, ", ").ok();
                    self.emit_constraint_expr_w(
                        rhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, ")").ok();
                } else {
                    self.emit_constraint_expr_w(
                        lhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                    write!(self.out, "{sep}").ok();
                    self.emit_constraint_expr_w(
                        rhs,
                        field_info,
                        width,
                        target_root,
                        blocking,
                        list_unroll_bounds,
                    );
                }
                if let Some(mask) = wrap_mask {
                    // `bvand` with the low-W mask, at the solver's width.
                    // `harc_z3_bv_value` (not `bv_val`) because at a solver
                    // width above 64 the mask does not fit a `uint64_t` —
                    // same pairing the bit-slice emitter already uses.
                    write!(self.out, ") & harc_z3_bv_value(_ctx, {mask}, {width}))").ok();
                }
            }
            _ => {
                self.errors.push(format!(
                    "constraint expression not supported in v0 solver path"
                ));
                write!(self.out, "_ctx.bool_val(true)").ok();
            }
        }
    }

    /// Translate `<lhs> in <rhs>` (and `inside`) for the Z3 constraint
    /// path. Rhs shapes:
    ///   * `[lo..hi]` (RangeLit) → `lhs >= lo && lhs <= hi`
    ///   * `{a, b, c}` (SetLit)  → `lhs == a || lhs == b || lhs == c`
    ///   * Anything else         → falls back to `lhs == rhs`
    ///                              (treats rhs as a singleton).
    ///
    /// Open and unbounded sides on RangeLit (`[..hi]`, `[lo..]`)
    /// collapse the missing comparison. Set elements that are
    /// themselves RangeLits recurse (so `{[0..3], 7}` expands to
    /// `(_z>=0 && _z<=3) || _z==7`).
    fn emit_constraint_membership(
        &mut self,
        lhs: &Expr,
        rhs: &Expr,
        field_info: &std::collections::HashMap<String, TxnFieldInfo>,
        width: u32,
        target_root: Option<&str>,
        blocking: bool,
        list_unroll_bounds: &std::collections::HashMap<String, usize>,
    ) {
        match &*rhs.kind {
            ExprKind::RangeLit { lo, hi } => {
                write!(self.out, "(").ok();
                let mut has_any = false;
                let signed = self.constraint_expr_is_signed(lhs, field_info, target_root);
                if let Some(l) = lo {
                    if signed {
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " >= ").ok();
                        self.emit_constraint_expr_w(
                            l,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                    } else {
                        write!(self.out, "z3::uge(").ok();
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ", ").ok();
                        self.emit_constraint_expr_w(
                            l,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ")").ok();
                    }
                    has_any = true;
                }
                if let Some(h) = hi {
                    if has_any {
                        write!(self.out, " && ").ok();
                    }
                    if signed {
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, " <= ").ok();
                        self.emit_constraint_expr_w(
                            h,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                    } else {
                        write!(self.out, "z3::ule(").ok();
                        self.emit_constraint_expr_w(
                            lhs,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ", ").ok();
                        self.emit_constraint_expr_w(
                            h,
                            field_info,
                            width,
                            target_root,
                            blocking,
                            list_unroll_bounds,
                        );
                        write!(self.out, ")").ok();
                    }
                    has_any = true;
                }
                if !has_any {
                    // Open-ended on both sides — vacuously true.
                    write!(self.out, "_ctx.bool_val(true)").ok();
                }
                write!(self.out, ")").ok();
            }
            ExprKind::SetLit(items) => {
                write!(self.out, "(").ok();
                if items.is_empty() {
                    write!(self.out, "_ctx.bool_val(false)").ok();
                } else {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            write!(self.out, " || ").ok();
                        }
                        // Recurse so `{[0..3], 7}` expands correctly.
                        match &*it.kind {
                            ExprKind::RangeLit { .. } | ExprKind::SetLit(_) => {
                                self.emit_constraint_membership(
                                    lhs,
                                    it,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                            }
                            ExprKind::Paren(inner) => {
                                self.emit_constraint_membership(
                                    lhs,
                                    inner,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                            }
                            _ => {
                                // Singleton element — equality test.
                                self.emit_constraint_expr_w(
                                    lhs,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                                write!(self.out, " == ").ok();
                                self.emit_constraint_expr_w(
                                    it,
                                    field_info,
                                    width,
                                    target_root,
                                    blocking,
                                    list_unroll_bounds,
                                );
                            }
                        }
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => {
                self.emit_constraint_membership(
                    lhs,
                    inner,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
            }
            _ => {
                // Fallback: treat rhs as a singleton — `a in b` becomes
                // `a == b`. Same shape `emit_bin_membership` uses.
                self.emit_constraint_expr_w(
                    lhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
                write!(self.out, " == ").ok();
                self.emit_constraint_expr_w(
                    rhs,
                    field_info,
                    width,
                    target_root,
                    blocking,
                    list_unroll_bounds,
                );
            }
        }
    }

    /// Emit a `log` or `logf` call. When `file_path` is `Some`, lower to
    /// `sim_logf_line(log_ctx.file(path), sev, fmt, args)`; otherwise lower
    /// to `sim_log_line(sev, fmt, args)`. Severity / message extraction
    /// matches `log()`'s rules (first ident is severity; first string is
    /// the message).
    fn emit_log(&mut self, args: &[CallArg], file_path: Option<String>, depth: usize) {
        let sev = args
            .iter()
            .find_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::Ident(id) => Some(id.name.to_uppercase()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| "INFO".to_string());
        let msg = args
            .iter()
            .find_map(|a| match a {
                CallArg::Expr(e) => match &*e.kind {
                    ExprKind::String(s) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_else(|| "".to_string());
        let (fmt, caps) = process_interp(&msg);
        self.pad(depth);
        match file_path {
            Some(p) => {
                write!(
                    self.out,
                    "sim_logf_line(log_ctx.file(\"{}\"), \"{}\", \"{}\"",
                    escape_c(&p),
                    sev,
                    escape_c(&fmt)
                )
                .ok();
            }
            None => {
                write!(self.out, "sim_log_line(\"{}\", \"{}\"", sev, escape_c(&fmt)).ok();
            }
        }
        for c in &caps {
            self.emit_interp_arg(c);
        }
        writeln!(self.out, ");").ok();

        // Spec §7.7 test-result semantics:
        //   `log(error, ...)` increments the failure counter; the test
        //                     fails at end of run if any error logged.
        //   `log(fatal, ...)` increments + sets `_fatal` so the main
        //                     simulation loop exits at end of the
        //                     current cycle (this test instance aborts;
        //                     others in a regression continue).
        // `warn` / `info` / `debug` have no test-result effect.
        match sev.as_str() {
            "ERROR" => {
                self.pad(depth);
                writeln!(self.out, "ctx.errors++;").ok();
            }
            "FATAL" => {
                self.pad(depth);
                writeln!(self.out, "ctx.errors++; _fatal = true;").ok();
            }
            _ => {}
        }
    }

    /// Emit one captured `${expr}` value as a printf argument. Routes the
    /// captured source text through the parser so `dut.x` (a member access
    /// on a pointer) gets emitted as `dut->x` via the normal field-rewrite
    /// machinery, rather than the literal source text. Falls back to raw
    /// text if the fragment doesn't parse — preserves whatever the user
    /// wrote so they get a meaningful C++ error rather than a hidden one.
    fn emit_interp_arg(&mut self, cap: &InterpCap) {
        write!(self.out, ", ").ok();
        match cap.wide_hex {
            Some((width, upper)) => {
                let upper_str = if upper { "true" } else { "false" };
                if width > 32 {
                    write!(self.out, "(const char*)harc_rt::HarcHexBufWide(").ok();
                } else {
                    write!(self.out, "(const char*)harc_rt::HarcHexBuf128(").ok();
                }
                match crate::parser::parse_expr_fragment(&cap.expr) {
                    Ok(e) => self.emit_expr(&e),
                    Err(_) => {
                        write!(self.out, "{}", cap.expr).ok();
                    }
                }
                write!(self.out, ", {width}, {upper_str})").ok();
            }
            None => {
                // Narrow / decimal path: preserve the legacy long-long
                // printf ABI. HarcWide<N> has both uint64_t and _harc_u128
                // conversions, so route through a helper to avoid ambiguous
                // C-style casts when a wide DUT signal is printed with a
                // narrow format such as `${sig:08x}`.
                write!(self.out, "harc_rt::harc_printf_ll(").ok();
                match crate::parser::parse_expr_fragment(&cap.expr) {
                    Ok(e) => self.emit_expr(&e),
                    Err(_) => {
                        write!(self.out, "{}", cap.expr).ok();
                    }
                }
                write!(self.out, ")").ok();
            }
        }
    }

    /// Emit a top-level `function` as a `[&]`-captured lambda. Named-typed
    /// parameters are recognised as DUT pointers (`VName*`); their field
    /// access in the body uses `->`. Capture-all (`[&]`) so the body can
    /// still see `tick` and any test-level state.
    /// Emit a `tseq` declaration as a `[&]`-capturing lambda that returns
    /// a `std::vector<T>` filled in by `yield` statements. The inner type
    /// `T` is the argument of `TSeq<T>` in the return-type slot.
    fn emit_tseq(&mut self, t: &TseqDecl, depth: usize) {
        // Inner type: pull T out of `TSeq<T>`. Default to `int64_t` if
        // the user wrote a bare `tseq`-as-block without a return clause.
        let inner = t
            .return_ty
            .as_ref()
            .and_then(tseq_inner_type)
            .unwrap_or_else(|| "int64_t".to_string());
        self.pad(depth);
        write!(self.out, "auto {} = [&](", t.name.name).ok();
        let mut added: Vec<String> = Vec::new();
        let param_names = cpp_param_names(&t.params);
        for (i, p) in t.params.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", param_names[i]).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) && p.name.name != "_" {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        writeln!(self.out, ") -> std::vector<{inner}> {{").ok();
        self.pad(depth + 1);
        writeln!(self.out, "std::vector<{inner}> _result;").ok();
        let prev = self.current_yield_target.replace("_result".to_string());
        self.emit_block(&t.body, depth + 1);
        self.current_yield_target = prev;
        self.pad(depth + 1);
        writeln!(self.out, "return _result;").ok();
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    fn emit_function(&mut self, f: &FunctionDecl, depth: usize) {
        self.pad(depth);
        let ret = f
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or("void".to_string());
        write!(self.out, "auto {} = [&](", f.name.name).ok();
        // Track which params are Named-typed (pointer-shaped). Add to
        // `pointer_vars` while emitting the body, then remove on exit so
        // siblings don't leak each other's params.
        let mut added: Vec<String> = Vec::new();
        let param_names = cpp_param_names(&f.params);
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                write!(self.out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or("int64_t".to_string());
            write!(self.out, "{pty} {}", param_names[i]).ok();
            if matches!(&p.ty, Some(TypeExpr::Named { .. })) && p.name.name != "_" {
                if self.pointer_vars.insert(p.name.name.clone()) {
                    added.push(p.name.name.clone());
                }
            }
        }
        writeln!(self.out, ") -> {ret} {{").ok();
        self.emit_block(&f.body, depth + 1);
        for k in added {
            self.pointer_vars.remove(&k);
        }
        self.pad(depth);
        writeln!(self.out, "}};").ok();
    }

    fn emit_block(&mut self, b: &Block, depth: usize) {
        for s in &b.stmts {
            self.emit_stmt(s, depth);
        }
    }

    fn emit_stmt(&mut self, s: &Stmt, depth: usize) {
        match &s.kind {
            StmtKind::Let(l) => self.emit_let(l, depth),
            StmtKind::Send { target, value } => {
                self.pad(depth);
                self.emit_signal_assignment(target, value, depth);
            }
            StmtKind::Assign { target, value } => {
                // Same narrowing rejection as the typed-`let` initializer:
                // reassigning a wider value into a declared-width local is
                // the identical defect (`b = a` for a 256-bit `a` into a
                // 200-bit `b`), and TB-IR rejects both.
                if let ExprKind::Ident(id) = &*target.kind {
                    self.check_scalar_assign_width(&id.name, value);
                }
                self.pad(depth);
                self.emit_signal_assignment(target, value, depth);
            }
            StmtKind::For(f) => {
                self.pad(depth);
                let var = &f.var.name;
                if let ExprKind::RangeLit {
                    lo: Some(lo),
                    hi: Some(hi),
                } = &*f.iter.kind
                {
                    // `for i in lo .. hi` — emit as an indexed C++ for loop.
                    // The range is INCLUSIVE of `hi` (matches ARCH), so the
                    // bound is `<=`, not `<`. Keep in lockstep with the tbir
                    // lowering in `ir::lower::control::lower_for`.
                    write!(self.out, "for (int64_t {var} = ").ok();
                    self.emit_expr(lo);
                    write!(self.out, "; {var} <= ").ok();
                    self.emit_expr(hi);
                    writeln!(self.out, "; {var}++) {{").ok();
                    self.emit_block(&f.body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                } else {
                    // `for x in <seq-expression>` — assume the rhs evaluates
                    // to something C++ can range-iterate over (e.g. a
                    // `std::vector<T>` returned by a `tseq`). The bound
                    // variable is `auto&` so transactions don't get copied
                    // on each iteration.
                    write!(self.out, "for (auto& {var} : ").ok();
                    self.emit_expr(&f.iter);
                    writeln!(self.out, ") {{").ok();
                    self.emit_block(&f.body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                }
            }
            StmtKind::Repeat(r) => {
                self.pad(depth);
                write!(self.out, "for (int64_t _r = 0; _r < ").ok();
                self.emit_expr(&r.count);
                writeln!(self.out, "; _r++) {{").ok();
                self.emit_block(&r.body, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Loop(b) => {
                self.pad(depth);
                writeln!(self.out, "while (true) {{").ok();
                self.emit_block(b, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::While { cond, body, .. } => {
                self.pad(depth);
                write!(self.out, "while (").ok();
                self.emit_expr(cond);
                writeln!(self.out, ") {{").ok();
                self.emit_block(body, depth + 1);
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::Break { .. } => {
                self.pad(depth);
                writeln!(self.out, "break;").ok();
            }
            StmtKind::Continue { .. } => {
                self.pad(depth);
                writeln!(self.out, "continue;").ok();
            }
            StmtKind::If(i) => {
                self.pad(depth);
                write!(self.out, "if (").ok();
                self.emit_expr(&i.cond);
                writeln!(self.out, ") {{").ok();
                self.emit_block(&i.then_block, depth + 1);
                for (c, b) in &i.elsifs {
                    self.pad(depth);
                    write!(self.out, "}} else if (").ok();
                    self.emit_expr(c);
                    writeln!(self.out, ") {{").ok();
                    self.emit_block(b, depth + 1);
                }
                if let Some(eb) = &i.else_block {
                    self.pad(depth);
                    writeln!(self.out, "}} else {{").ok();
                    self.emit_block(eb, depth + 1);
                }
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
            StmtKind::After { duration, body, .. } => {
                self.pad(depth);
                if self.in_coroutine {
                    // Coroutine path: yield to the scheduler so other
                    // coroutines can advance during the wait.
                    write!(self.out, "co_await harc_rt::wait_cycles(_slot, (uint32_t)(").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "));").ok();
                } else {
                    write!(self.out, "for (int _ck = 0; _ck < ").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "; _ck++) tick();").ok();
                }
                if !body.stmts.is_empty() {
                    self.pad(depth);
                    writeln!(self.out, "{{").ok();
                    self.emit_block(body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}}").ok();
                }
            }
            StmtKind::Wait {
                duration, clock, ..
            } => {
                self.pad(depth);
                // `wait N cycles on <clock>` — advance simulated time
                // until the named clock has seen N more rising edges.
                // Other clocks continue ticking at their natural rate.
                // Useful for cycle-relative reasoning in multi-clock
                // tests (e.g. "after 2 dst_clk cycles, X should hold").
                if let Some(c) = clock {
                    let idx = match self.clock_names.iter().position(|n| n == &c.name) {
                        Some(i) => i,
                        None => {
                            self.errors.push(format!(
                                "wait ... on {}: no clock named `{}` declared in this test",
                                c.name, c.name
                            ));
                            return;
                        }
                    };
                    // `wait N cycles on <named-clock>` always lowers to
                    // an inline `eval_clocks_until` loop, regardless of
                    // coroutine context. The main loop's
                    // `eval_clocks_until(next_primary_posedge)` advances
                    // by a FULL primary period per iteration, which is
                    // too coarse when the named clock runs faster than
                    // the primary (e.g. `wait 1 cycle on dst_clk` where
                    // dst is 2× src would skip past the target). The
                    // sync path advances to whichever clock's next edge
                    // is sooner, preserving sub-primary-cycle precision.
                    //
                    // Phase 1 is single-actor anyway (only the run
                    // coroutine exists), so not yielding to the
                    // scheduler during this wait is harmless. When
                    // multi-actor lands and named-clock waits need to
                    // cooperate with other coroutines, the main loop
                    // gets reworked to advance by next-edge-of-any-clock
                    // and the coroutine path resumes.
                    write!(
                        self.out,
                        "{{ long long _target = clocks_[{idx}].rising_count + (long long)("
                    )
                    .ok();
                    self.emit_expr(duration);
                    writeln!(
                        self.out,
                        "); while (clocks_[{idx}].rising_count < _target) {{"
                    )
                    .ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "long long _next = clocks_[0].next_edge_ps;").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "for (auto& _ck : clocks_) if (_ck.next_edge_ps < _next) _next = _ck.next_edge_ps;").ok();
                    self.pad(depth + 1);
                    writeln!(self.out, "eval_clocks_until(_next);").ok();
                    self.pad(depth);
                    writeln!(self.out, "}} for (auto& _c : _checkers) _c(); }}").ok();
                    return;
                }
                // Wall-clock duration (e.g. `wait 100ns`) advances absolute
                // time when multi-clock; under single-clock backward compat
                // we don't have absolute time, so fall back to a comment +
                // a single tick (rare path; the documented form for single
                // clock is `wait N cycles`).
                if let ExprKind::Time(s) = &*duration.kind {
                    match time_literal_to_ps(s) {
                        Ok(ps) => {
                            writeln!(self.out, "eval_clocks_until(now_ps + {ps});").ok();
                        }
                        Err(e) => self.errors.push(e),
                    }
                } else if self.in_coroutine {
                    write!(self.out, "co_await harc_rt::wait_cycles(_slot, (uint32_t)(").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "));").ok();
                } else {
                    write!(self.out, "for (int _w = 0; _w < ").ok();
                    self.emit_expr(duration);
                    writeln!(self.out, "; _w++) tick();").ok();
                }
            }
            StmtKind::WaitUntil {
                mode,
                conditions,
                timeout,
                ..
            } => {
                self.emit_wait_until(mode, conditions, timeout.as_ref(), depth);
            }
            StmtKind::Assert(v) => {
                // Spec §2 LL(1) table: bare IDENT → named property
                // reference (concurrent); expression with temporal ops →
                // concurrent inline; plain bool expression → immediate
                // point-in-time check. The legacy `property` keyword still
                // parses (back-compat) but is no longer required and no
                // longer changes dispatch.
                if let Some(expr) = &v.expr {
                    if is_concurrent_assertion(expr, &self.properties) {
                        self.emit_property_check("FAIL", v, depth);
                    } else {
                        self.emit_inline_assert(v, depth);
                    }
                }
            }
            StmtKind::Fail { msg, .. } => {
                // Standalone `fail("...")` — unconditional failure log
                // + error counter bump. Same emission as the failure
                // arm of an inline assert, just without the surrounding
                // `if (!cond)` guard.
                let raw = match &*msg.kind {
                    ExprKind::String(s) => s.clone(),
                    _ => "fail() with non-string arg".to_string(),
                };
                let (fmt, caps) = process_interp(&raw);
                self.pad(depth);
                write!(self.out, "sim_log_line(\"FAIL\", \"{}\"", escape_c(&fmt)).ok();
                for c in &caps {
                    self.emit_interp_arg(c);
                }
                writeln!(self.out, ");").ok();
                self.pad(depth);
                writeln!(self.out, "ctx.errors++;").ok();
            }
            StmtKind::Assume(v) => {
                if let Some(expr) = &v.expr {
                    if is_concurrent_assertion(expr, &self.properties) {
                        self.emit_property_check("ASSUME-FAIL", v, depth);
                    } else {
                        self.emit_inline_assume(v, depth);
                    }
                }
            }
            StmtKind::Cover(v) => {
                // Concurrent cover (spec §5): a witness counter — every
                // primary-clock edge where the expression is true bumps
                // the hit count. End-of-test report aggregates all
                // registered covers (similar to a covergroup, but flat).
                // `cover NAME` resolves a declared property; otherwise
                // the inline expression is used directly.
                let expr = match &v.expr {
                    Some(e) => e,
                    None => return,
                };
                let (label, body) = if let ExprKind::Ident(id) = &*expr.kind {
                    if let Some(b) = self.properties.get(&id.name).cloned() {
                        (id.name.clone(), b)
                    } else {
                        (id.name.clone(), expr.clone())
                    }
                } else {
                    (
                        format!("cov_{}_{}", expr.span.start, expr.span.end),
                        expr.clone(),
                    )
                };
                let tag = format!("c_{}_{}", expr.span.start, expr.span.end);
                self.covers.push(CoverInfo {
                    tag: tag.clone(),
                    label,
                });
                self.pad(depth);
                writeln!(self.out, "static uint64_t _cov_{tag}_hits = 0;").ok();
                self.pad(depth);
                writeln!(self.out, "_checkers.push_back([&]() {{").ok();
                self.pad(depth + 1);
                write!(self.out, "if ((bool)(").ok();
                // Note: temporal ops (|->, |=>, past/rose/etc.) inside a
                // cover body aren't translated for v0 — the same
                // limitation as inline-temporal asserts before the
                // property-decl path was added. Use a `property` decl
                // and `cover <name>` for temporal patterns, or stick to
                // same-cycle bool expressions inline.
                self.emit_expr(&body);
                writeln!(self.out, ")) _cov_{tag}_hits++;").ok();
                self.pad(depth);
                writeln!(self.out, "}});").ok();
            }
            StmtKind::Log { args, .. } => self.emit_log(args, None, depth),
            StmtKind::LogF { args, .. } => {
                // First positional string literal is the file path; the
                // remaining args follow `log` semantics (severity ident +
                // message string).
                let (path, rest): (Option<String>, Vec<&CallArg>) = {
                    let mut path = None;
                    let mut rest = Vec::new();
                    for a in args {
                        if path.is_none() {
                            if let CallArg::Expr(e) = a {
                                if let ExprKind::String(s) = &*e.kind {
                                    path = Some(s.clone());
                                    continue;
                                }
                            }
                        }
                        rest.push(a);
                    }
                    (path, rest)
                };
                if path.is_none() {
                    self.errors
                        .push("logf requires a string-literal file path as the first arg".into());
                    return;
                }
                let collected: Vec<CallArg> = rest.into_iter().cloned().collect();
                self.emit_log(&collected, path, depth);
            }
            StmtKind::Expr(e) => {
                // `fork bus.<method>(args)` issues a TLM request without
                // waiting for the response; `join_all` drains responses.
                if self.try_emit_bus_tlm_fork(e, None, depth) {
                    return;
                }
                // `bus.<method>(args)` for bus-level `tlm_method`
                // declarations expands into ARCH-compatible req/rsp
                // protocol wires.
                if self.try_emit_bus_tlm_method(e, None, depth) {
                    return;
                }
                // `bus.<channel>.send(args)` and `bus.<channel>.recv()`
                // expand into a multi-statement v/r handshake. When
                // it's a discarded-result `recv()`, just run the dance.
                if self.try_emit_bus_handshake(e, None, depth) {
                    return;
                }
                // `bitbash(regs)` — RFC §7.7 compile-time-unrolled
                // walk-all over each RW register in the regblock.
                // Emits write(all-ones) + read + assert + write(0) +
                // read + assert for every RW register. RO/WO are
                // skipped with a comment. See docs/ral-support.md.
                if self.try_emit_bitbash(e, depth) {
                    return;
                }
                // `regs.record_write(addr, data)` — passive mirror update
                // keyed by address, with per-register write-callback
                // dispatch. Codegen owns the address decode.
                if self.try_emit_record_write(e, depth) {
                    return;
                }
                self.pad(depth);
                self.emit_expr(e);
                writeln!(self.out, ";").ok();
            }
            StmtKind::JoinAll { .. } => {
                self.emit_tlm_join_all(depth);
            }
            StmtKind::Release(e) => {
                // `release <expr>` — disable the active SV procedural
                // force on a `probe force` signal. Lowers to a single
                // store: `<name>_en = 0`. Read-only probes (no `force`
                // modifier) error.
                if let Some(probe) = self.resolve_force_probe(e) {
                    self.pad(depth);
                    writeln!(self.out, "dut->rootp->{} = 0;", probe.enable()).ok();
                } else {
                    self.errors.push(
                        "`release` target must be `dut.<probe_name>` where the named probe was declared with `probe force`".into(),
                    );
                }
            }
            StmtKind::Return(opt) => {
                self.pad(depth);
                if let Some(e) = opt {
                    write!(self.out, "return ").ok();
                    self.emit_expr(e);
                    writeln!(self.out, ";").ok();
                } else {
                    writeln!(self.out, "return;").ok();
                }
            }
            StmtKind::Yield(e) => {
                self.pad(depth);
                if let Some(target) = self.current_yield_target.clone() {
                    write!(self.out, "{target}.push_back(").ok();
                    self.emit_expr(e);
                    writeln!(self.out, ");").ok();
                } else {
                    self.errors
                        .push("`yield` outside a `tseq` body is not supported in v0 cpp_tb".into());
                }
            }
            StmtKind::Emit { name, args, .. } => {
                // `emit e(v)` → call every subscriber.
                //   - Multi-segment path (`emit drv.req(t)`) → emit
                //     verbatim as `drv.req`.
                //   - Single segment (`emit write_seen(...)` inside a
                //     monitor body) → check field_subs to resolve the
                //     bare name to the instance-qualified field
                //     (`mon.write_seen`).
                let event_name = if name.segments.len() > 1 {
                    name.segments
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".")
                } else {
                    let raw = name
                        .segments
                        .last()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.field_subs.get(&raw).cloned().unwrap_or(raw)
                };
                let arg = args.first();
                self.pad(depth);
                write!(self.out, "for (auto& _s : {event_name}) _s(").ok();
                if let Some(a) = arg {
                    match a {
                        CallArg::Expr(e) => self.emit_expr(e),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                writeln!(self.out, ");").ok();
                // Activity tracking (spec §7.x): an `emit` counts as an
                // "out" for the surrounding component instance. Emits
                // outside a component (test-run / free function) don't
                // attribute to any instance.
                if let Some(inst) = self.current_component_instance.clone() {
                    self.pad(depth);
                    writeln!(self.out, "{inst}._last_out_cycle = (uint64_t)cycle_count;").ok();
                }
            }
            StmtKind::On(h) => {
                // Three shapes:
                //   `on obj.method pre/post ... end on` — pre/post hook
                //       on a hookable method. Resolves obj to a known
                //       component-typed binding, then pushes the body
                //       closure into the global `<Type>_<method>_<side>`
                //       vector. The method's body wrap fires hooks
                //       around the body each call.
                //   `on event_name(arg) ... end on` — event subscription.
                //   `on <bool-expr> ... end on`     — cycle trigger.
                if let Some(side) = h.hook {
                    if h.phase == OnPhase::PostEval {
                        self.errors.push(
                            "on <obj>.<method> phase post_eval is not supported; use `pre`/`post` method hooks or a cycle-trigger `on <expr> phase post_eval`".into()
                        );
                        return;
                    }
                    if let Some((comp_ty, method_name, params)) =
                        self.resolve_component_hookable(&h.event)
                    {
                        let side_str = match side {
                            HookSide::Pre => "pre",
                            HookSide::Post => "post",
                        };
                        let param_names = cpp_param_names(&params);
                        let arg_decls: Vec<String> = params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                let ty =
                                    p.ty.as_ref()
                                        .map(|t| self.c_type_for_param(t))
                                        .unwrap_or_else(|| "int64_t".to_string());
                                format!("{ty} {}", param_names[i])
                            })
                            .collect();
                        self.pad(depth);
                        writeln!(
                            self.out,
                            "{comp_ty}_{method_name}_{side_str}.push_back([&]({}) {{",
                            arg_decls.join(", "),
                        )
                        .ok();
                        self.emit_block(&h.body, depth + 1);
                        self.pad(depth);
                        writeln!(self.out, "}});").ok();
                        return;
                    } else {
                        self.errors.push(
                            "on <obj>.<method> pre/post: obj.method must resolve to a `hookable` on a known component type".into()
                        );
                        return;
                    }
                }
                // RAL per-register write callback: `on regs.REG ... end on`
                // where `regs` is a regblock binding and REG is one of its
                // registers. Lowers to a `void(uint64_t data)` closure
                // stored in `<regs>_cbs.REG`; `record_write` fires it after
                // updating the mirror cell. The body sees the written value
                // as the local `data`. No hook side / period. Recognized
                // before the cycle-trigger fallback (a regblock register
                // has no meaningful boolean-trigger reading — that would
                // issue a bus read every cycle). See docs/ral-support.md §3.2.
                if h.hook.is_none() && !h.periodic {
                    if let ExprKind::Field {
                        target,
                        name: reg_name,
                    } = &*h.event.kind
                    {
                        if let ExprKind::Ident(regs_id) = &*target.kind {
                            let is_reg = self
                                .let_types
                                .get(&regs_id.name)
                                .and_then(|ty| self.regblocks.get(ty))
                                .map(|b| b.registers.iter().any(|r| r.name.name == reg_name.name))
                                .unwrap_or(false);
                            if is_reg {
                                self.pad(depth);
                                writeln!(
                                    self.out,
                                    "{}_cbs.{} = [&](uint64_t data) {{",
                                    regs_id.name, reg_name.name,
                                )
                                .ok();
                                self.emit_block(&h.body, depth + 1);
                                self.pad(depth);
                                writeln!(self.out, "}};").ok();
                                return;
                            }
                        }
                    }
                }
                if let ExprKind::Call { callee, args } = &*h.event.kind {
                    if h.phase == OnPhase::PostEval {
                        self.errors.push(
                            "on <event>(arg) phase post_eval is not supported; post_eval is only for cycle-trigger handlers".into()
                        );
                        return;
                    }
                    // Event-subscription path.
                    let raw = match &*callee.kind {
                        ExprKind::Ident(id) => id.name.clone(),
                        ExprKind::Field { target: _, name } => {
                            // `mon.write_e` → emit raw "mon.write_e".
                            // Use a string buffer to capture the qualified name.
                            let mut tmp = String::new();
                            std::mem::swap(&mut self.out, &mut tmp);
                            self.emit_expr(callee);
                            std::mem::swap(&mut self.out, &mut tmp);
                            let _ = name;
                            tmp
                        }
                        _ => {
                            self.errors
                                .push("on <event>(arg): event must be `name` or `obj.name`".into());
                            return;
                        }
                    };
                    let event_ref = self.field_subs.get(&raw).cloned().unwrap_or(raw.clone());
                    let arg = match args.first() {
                        Some(CallArg::Expr(e)) => match &*e.kind {
                            ExprKind::Ident(id) => id.name.clone(),
                            _ => "_v".into(),
                        },
                        _ => "_v".into(),
                    };
                    let arg_ty = self
                        .event_types
                        .get(&raw)
                        .cloned()
                        .unwrap_or_else(|| "int64_t".into());
                    self.pad(depth);
                    writeln!(self.out, "{event_ref}.push_back([&]({arg_ty} {arg}) {{").ok();
                    self.emit_block(&h.body, depth + 1);
                    self.pad(depth);
                    writeln!(self.out, "}});").ok();
                } else {
                    // Cycle-trigger path. Edge mode comes from the parser
                    // (`rising` / `falling` / `level`). Default is rising
                    // — matches typical handshake protocols.
                    self.emit_cycle_trigger(h, depth, "_t_");
                }
            }
            StmtKind::Randomize {
                blocking,
                target,
                with_body,
            } => {
                let ty = match &*target.kind {
                    ExprKind::Ident(id) => self.let_types.get(&id.name).cloned(),
                    _ => None,
                };
                let ty = match ty {
                    Some(t) if self.is_record_type(&t) => t,
                    Some(t) => {
                        self.errors.push(format!(
                            "randomize(t): t has type `{t}` but no `transaction {t}` or `struct {t}` is declared in this file"
                        ));
                        return;
                    }
                    None => {
                        self.errors.push(
                            "randomize(t): could not resolve t's type — declare with `let t : <Transaction>`".into()
                        );
                        return;
                    }
                };
                // Spec §4: transaction-level `keep` constraints are
                // semantically part of every `randomize(t)` of that
                // type. Merge them with any user-supplied `with …`
                // body before handing to the solver. Without this
                // merge, `keep` would silently parse but emit zero
                // C++ — a correctness footgun (caught while auditing
                // §4 implementation status).
                let txn_keeps = self.txn_keeps.get(&ty).cloned().unwrap_or_default();
                let mut combined: Vec<Expr> = Vec::with_capacity(txn_keeps.len() + with_body.len());
                combined.extend(txn_keeps);
                combined.extend(with_body.iter().cloned());
                self.report_runtime_dependent_randomize_field_attrs(&ty);
                let has_solver_policy_fields = self.txn_fields.get(&ty).is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|f| field_attr_unique(f) || !auto_coverage_values(f).is_empty())
                });
                if combined.is_empty() && !has_solver_policy_fields {
                    // No constraints anywhere: route the former direct PRNG
                    // call through the runtime shell. The shell currently
                    // delegates to `randomize_T`, preserving behavior while
                    // exercising the new boundary.
                    if !self.emit_runtime_unconstrained_randomize_call(&ty, target, depth) {
                        self.pad(depth);
                        write!(self.out, "randomize_{ty}(&").ok();
                        self.emit_expr(target);
                        writeln!(self.out, ");").ok();
                    }
                    self.emit_randomize_trace_event(&ty, target, depth);
                } else {
                    // Constraint-solving path via Z3.
                    self.emit_constraint_solver_block(&ty, target, &combined, *blocking, depth);
                }
            }
            other => {
                self.errors.push(format!(
                    "statement not supported in v0 cpp_tb: {:?}",
                    std::mem::discriminant(other)
                ));
            }
        }
    }

    fn emit_let(&mut self, l: &LetStmt, depth: usize) {
        if l.name.name == "_" {
            self.pad(depth);
            if let Some(v) = &l.value {
                write!(self.out, "(void)(").ok();
                self.emit_expr(v);
                writeln!(self.out, ");").ok();
            } else {
                writeln!(self.out, "// let _ (discard)").ok();
            }
            return;
        }
        // Track let-binding type for randomize(t) resolution. Done first
        // (before the dut shortcut) so even nested lets register.
        if let Some(s) = type_simple_name(l.ty.as_ref()) {
            self.let_types.insert(l.name.name.clone(), s.to_string());
        }
        // Track mode annotation for the call-site passive-mode check.
        // Same gate as the pre-pass at lowering entry — only typed lets
        // with an explicit `active`/`passive` populate; inheritance is
        // resolved at lookup time via `resolve_path_mode`.
        if let Some(TypeExpr::Named { mode: Some(m), .. }) = l.ty.as_ref() {
            self.let_modes.insert(l.name.name.clone(), *m);
        }
        // Track explicit bit-width for width-method intrinsics
        // (`.trunc<N>()` etc.). Same logic as the per-test loop seed
        // in `emit_with_opts` — typed lets only; bare `let x = expr`
        // falls back to RHS inference.
        if let Some(TypeExpr::Builtin { name, args, .. }) = l.ty.as_ref() {
            // Signedness gate FIRST, and outside the `UInt | SInt | Bits`
            // match below: that match excludes `SIntCap`, so a check nested
            // inside it could never fire for `let s : SInt<8> = …`.
            if matches!(name, BuiltinTy::SInt | BuiltinTy::SIntCap) {
                if let (Some(w), Some(v)) = (type_arg_width(args), l.value.as_ref()) {
                    self.check_signed_wrap_destination(&l.name.name, w, v);
                }
            }
            if matches!(name, BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits) {
                if let Some(w) = type_arg_width(args) {
                    // Narrowing initializer: reject before emitting. v1
                    // used to emit `HarcWide<7> b = a;` for a 256-bit `a`,
                    // which does not compile, and silently accepted the
                    // ≤64-bit cases TB-IR rejects. Same diagnostic and same
                    // accepted set as TB-IR's `check_scalar_assign_width`.
                    let dw = w as u32;
                    let aw = l
                        .value
                        .as_ref()
                        .filter(|_| dw > 0)
                        .and_then(|v| narrowing_source_width(self, v));
                    if let Some(aw) = aw {
                        if aw > dw {
                            self.errors.push(format!(
                                "assignment of a {aw}-bit value to `{}`, declared {dw} \
                                 bits, narrows. Widths must not shrink implicitly — use \
                                 `.trunc<{dw}>()` to narrow explicitly, or widen the \
                                 declaration to {aw} bits.",
                                l.name.name
                            ));
                        }
                    }
                    if self.let_widths.insert(l.name.name.clone(), dw).is_some() {
                        self.shadowed_lets.insert(l.name.name.clone());
                    }
                }
            }
        }
        // Bus binding: `let axil : BusAxiLite = bind dut`. Track the
        // (bus, dut-root) pair so subsequent `axil.signal` accesses
        // can lower to flat DUT signals (`dut->axil_signal`). The
        // binding itself emits no C++ — it's purely a typing
        // assertion and a name-prefix declaration.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if let Some(bus) = self.buses.get(simple).cloned() {
                if let Some(v) = &l.value {
                    // Capture the bind expression as a string. For the
                    // common case of `bind dut`, that's just "dut".
                    let mut buf = String::new();
                    std::mem::swap(&mut self.out, &mut buf);
                    self.emit_expr(v);
                    std::mem::swap(&mut self.out, &mut buf);
                    // Test-scope binding: identifier and prefix are
                    // the same. Aliases (e.g. driver's `bus`) reuse
                    // this prefix when registering — see
                    // `emit_component_handler_registrations_bound`.
                    let prefix = l.name.name.clone();
                    // Effective param env for `generate_if` gate evaluation:
                    // bus defaults overlaid with bind-site generic overrides and
                    // then the DUT port's own override (authoritative for which
                    // gated channels `arch build` flattened — bind name == DUT
                    // port name by convention).
                    let env = bus_param_env_with_port_override(
                        &bus,
                        l.ty.as_ref(),
                        self.dut_bus_port_overrides.get(&l.name.name),
                    );
                    self.bus_param_envs.insert(l.name.name.clone(), env);
                    self.bus_bindings
                        .insert(l.name.name.clone(), (bus, buf, prefix.clone()));
                    // Translate the AST's bind_remap entries into a
                    // (channel, signal) → port_name lookup table. The
                    // path is required to be exactly `<channel>.<signal>`
                    // (two segments) — single-segment paths and 3+
                    // levels error out cleanly.
                    if !l.bind_remap.is_empty() {
                        let mut map: std::collections::HashMap<(String, String), String> =
                            std::collections::HashMap::new();
                        for entry in &l.bind_remap {
                            if entry.path.len() != 2 {
                                self.errors.push(format!(
                                    "bind {} with: signal path `{}` must be exactly `<channel>.<signal>` (2 segments, got {})",
                                    l.name.name,
                                    entry.path.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join("."),
                                    entry.path.len(),
                                ));
                                continue;
                            }
                            map.insert(
                                (entry.path[0].name.clone(), entry.path[1].name.clone()),
                                entry.port.clone(),
                            );
                        }
                        self.bus_remap.insert(prefix, map);
                    }
                    return;
                }
            }
        }
        // RAL addrmap binding: `let chip : <AddrmapType> = bind <helper>`.
        // Same shape as regblock binding — emits a value-typed mirror
        // struct and records the helper.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if self.addrmaps.contains_key(simple) {
                if l.bind {
                    if let Some(v) = &l.value {
                        if let ExprKind::Ident(rhs) = &*v.kind {
                            self.let_helper
                                .insert(l.name.name.clone(), rhs.name.clone());
                        } else {
                            self.errors.push(format!(
                                "let {} : {simple} = bind <expr>: addrmap binding RHS must be a helper transactor identifier",
                                l.name.name,
                            ));
                        }
                    }
                } else {
                    self.errors.push(format!(
                        "let {} : {simple}: addrmap instantiation requires `= bind <helper>`",
                        l.name.name,
                    ));
                }
                self.pad(depth);
                writeln!(self.out, "{simple}_Mirror {};", l.name.name).ok();
                return;
            }
        }
        // RAL regblock binding: `let regs : <RegblockType> = bind <helper>`.
        // Emit `<T>_Mirror regs;` (reset values populate from the struct
        // default initializers emitted at file scope) and record the
        // helper variable name so accessor lowering can resolve it.
        // See docs/ral-support.md.
        if let Some(simple) = type_simple_name(l.ty.as_ref()) {
            if self.regblocks.contains_key(simple) {
                if l.bind {
                    if let Some(v) = &l.value {
                        if let ExprKind::Ident(rhs) = &*v.kind {
                            self.let_helper
                                .insert(l.name.name.clone(), rhs.name.clone());
                        } else {
                            self.errors.push(format!(
                                "let {} : {simple} = bind <expr>: regblock binding RHS must be a helper transactor identifier",
                                l.name.name,
                            ));
                        }
                    }
                } else {
                    self.errors.push(format!(
                        "let {} : {simple}: regblock instantiation requires `= bind <helper>` (a transactor with write/read methods)",
                        l.name.name,
                    ));
                }
                self.pad(depth);
                writeln!(self.out, "{simple}_Mirror {};", l.name.name).ok();
                // Per-register write-callback holder; slots are filled by
                // `on regs.REG ... end on` and fired from `record_write`.
                self.pad(depth);
                writeln!(self.out, "{simple}_Callbacks {}_cbs;", l.name.name).ok();
                // Recursion-depth counter for `record_write` -> callback ->
                // `record_write` cascades. Each entry into the decode block
                // bumps this, and we abort if it exceeds
                // `HARC_RAL_CB_MAX_DEPTH`. See docs/ral-support.md §3.2.
                self.pad(depth);
                writeln!(self.out, "uint32_t {}_cb_depth = 0;", l.name.name).ok();
                return;
            }
        }
        // Component binding: `let drv : AxilDrv = bind axil` for a
        // `driver` / `agent` / `monitor` declared `bound to BusType`.
        // The binding ties this driver instance to the named bus
        // binding so its on-handlers can use the bare `bus` identifier
        // for typed signal/handshake access. Emits the struct exactly
        // as the no-value path would, plus on-handler registrations
        // with the bus binding pushed for the duration of each body.
        if l.bind {
            if let Some(simple) = type_simple_name(l.ty.as_ref()) {
                // Bound transactor: `let xact : T mode = bind axil`
                // for a `transactor T bound to BusType`. Compose a
                // synthetic ComponentDecl from the transactor body —
                // including or excluding the `when active` items
                // based on the instantiation's mode — and route
                // through the existing bound-driver-actor +
                // bound-monitor-actor paths. (Spec §8.1, T-2 in the
                // implementation roadmap.)
                if let Some(t) = self.transactors.get(simple).cloned() {
                    // Mode is mandatory at the binding site for
                    // transactors. We read it from the let's
                    // TypeExpr::Named.mode field, populated by
                    // parse_let_stmt.
                    let mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => *m,
                        _ => {
                            self.errors.push(format!(
                                "let {}: transactor instantiation requires a mode annotation (`{} active` or `{} passive`)",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                    };
                    let include_active = matches!(mode, TransactorMode::Active);
                    let synth = synth_component_from_transactor(&t, include_active);

                    // Four shapes:
                    //   bound transactor + `= bind axil`  → bus-actor path
                    //   bound transactor + no value       → error
                    //   unbound transactor + no value     → handler-registration path (same as plain driver/agent)
                    //   unbound transactor + `= bind axil`→ error
                    match (&t.bound_to, &l.value) {
                        (Some(bus_ty), Some(v)) => {
                            if let ExprKind::Ident(rhs) = &*v.kind {
                                if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned() {
                                    let want = type_simple_name(Some(bus_ty));
                                    if want != Some(binding.0.name.name.as_str()) {
                                        self.errors.push(format!(
                                            "let {} : {} = bind {}: transactor is bound to `{}`, but `{}` is a `{}`",
                                            l.name.name, simple, rhs.name,
                                            want.unwrap_or("?"),
                                            rhs.name, binding.0.name.name,
                                        ));
                                    }
                                    self.pad(depth);
                                    writeln!(self.out, "{simple} {};", l.name.name).ok();

                                    // Driver-actor path only fires for
                                    // active mode (passive synth has no
                                    // input event field, so it would
                                    // bail anyway — but skipping the
                                    // call is cleaner).
                                    if include_active {
                                        self.try_emit_bound_driver_actor(
                                            &synth,
                                            &l.name.name,
                                            depth,
                                            &binding,
                                        );
                                    }
                                    // Monitor-actor path: handshake
                                    // handlers in the always-present
                                    // body always fire, regardless of
                                    // mode. This is the observation
                                    // half of the transactor.
                                    self.emit_bound_monitor_actors(
                                        &synth,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    );
                                    self.emit_bound_tlm_target_actors(
                                        &synth,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    );
                                    return;
                                } else {
                                    self.errors.push(format!(
                                        "let {} : {} = bind {}: `{}` is not a known bus binding",
                                        l.name.name, simple, rhs.name, rhs.name,
                                    ));
                                    return;
                                }
                            }
                        }
                        (None, None) => {
                            // Unbound transactor: default-construct
                            // the struct, then walk the synth
                            // ComponentDecl to wire up `on`-handler
                            // subscribers. Same path as plain
                            // `driver`/`agent` instantiation. No bus
                            // actor coroutines spawn — the active
                            // half drives raw DUT signals (or
                            // sideband), reacting to `emit
                            // xact.req(t)` events from the test
                            // scope. The passive half observes via
                            // event-typed `on` handlers if present.
                            self.pad(depth);
                            writeln!(self.out, "{simple} {};", l.name.name).ok();
                            let tag = format!("_xactor_{}_", l.name.name);
                            self.emit_component_handler_registrations(
                                &synth,
                                &l.name.name,
                                depth,
                                &tag,
                            );
                            return;
                        }
                        (Some(_), None) => {
                            self.errors.push(format!(
                                "let {} : {}: transactor `{}` has a `bound to` clause; instantiation requires `= bind <bus>`",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                        (None, Some(_)) => {
                            self.errors.push(format!(
                                "let {} : {} = bind ...: transactor `{}` has no `bound to` clause; remove the `= bind` clause",
                                l.name.name, simple, simple,
                            ));
                            return;
                        }
                    }
                    self.errors.push(format!(
                        "let {} : {} = bind <expr>: rhs must be a bare bus-binding name in v0",
                        l.name.name, simple,
                    ));
                    return;
                }
                // (Legacy bound `monitor MonName` instantiation path
                // is removed — `monitor` construct retired in PR-B.
                // Observation handlers live in transactor always-on
                // bodies, lowered via the synth ComponentDecl path
                // above.)
                if let Some(comp) = self.components.get(simple).cloned() {
                    if let Some(bus_ty) = &comp.bound_to {
                        // The rhs must name an existing bus binding (a
                        // previous `let X : BusType = bind dut`). Plain
                        // ident only in v0; richer expressions deferred.
                        if let Some(v) = &l.value {
                            if let ExprKind::Ident(rhs) = &*v.kind {
                                if let Some(binding) = self.bus_bindings.get(&rhs.name).cloned() {
                                    // Type-check: the bus binding's
                                    // BusDecl name must match the
                                    // driver's `bound to <BusType>`.
                                    let want = type_simple_name(Some(bus_ty));
                                    if want != Some(binding.0.name.name.as_str()) {
                                        self.errors.push(format!(
                                            "let {} : {} = bind {}: driver is bound to `{}`, but `{}` is a `{}`",
                                            l.name.name, simple, rhs.name,
                                            want.unwrap_or("?"),
                                            rhs.name, binding.0.name.name,
                                        ));
                                    }
                                    self.pad(depth);
                                    writeln!(self.out, "{simple} {};", l.name.name).ok();
                                    // Try the Phase 2b coroutine actor
                                    // path first: if the driver has a
                                    // single input event with a
                                    // matching `on <ev>(t)` handler,
                                    // spawn an independent coroutine
                                    // that loops over a transaction
                                    // queue. Falls through to the
                                    // sync subscriber path when the
                                    // driver isn't actor-shaped (no
                                    // input event, or multiple
                                    // handlers — the latter stays sync
                                    // for now to keep the model
                                    // tractable).
                                    if self.try_emit_bound_driver_actor(
                                        &comp,
                                        &l.name.name,
                                        depth,
                                        &binding,
                                    ) {
                                        return;
                                    }
                                    let tag = format!("_{}_", comp.kind.keyword());
                                    self.emit_component_handler_registrations_bound(
                                        &comp,
                                        &l.name.name,
                                        depth,
                                        &tag,
                                        Some(binding),
                                    );
                                    return;
                                } else {
                                    self.errors.push(format!(
                                        "let {} : {} = bind {}: `{}` is not a known bus binding (declare it first via `let {} : <BusType> = bind dut`)",
                                        l.name.name, simple, rhs.name, rhs.name, rhs.name,
                                    ));
                                    return;
                                }
                            }
                        }
                        self.errors.push(format!(
                            "let {} : {} = bind <expr>: rhs must be a bare bus-binding name in v0",
                            l.name.name, simple,
                        ));
                        return;
                    }
                }
            }
        }
        // Skip the `let dut : T` decl — already emitted at main() prelude.
        if l.name.name == "dut" {
            return;
        }
        // `let e : event<T>` — pub/sub primitive. Lower to a vector of
        // std::function<void(T)>; `on e(arg) body end on` registers a
        // closure; `emit e(v)` calls every subscriber. (Spec §3.4 + §7.3.)
        if let Some(TypeExpr::Builtin {
            name: BuiltinTy::Event,
            args,
            ..
        }) = &l.ty
        {
            let inner = args
                .first()
                .map(|a| match a {
                    TypeArg::Type(t) => self.record_field_c_type(t),
                    _ => "uint64_t".into(),
                })
                .unwrap_or_else(|| "uint64_t".into());
            self.pad(depth);
            writeln!(
                self.out,
                "std::vector<std::function<void({inner})>> {};",
                l.name.name
            )
            .ok();
            self.event_types.insert(l.name.name.clone(), inner);
            return;
        }
        if let Some(v) = &l.value {
            // `let x = fork bus.<method>(args)` issues the request now
            // and captures the response into x at a later `join_all`.
            if self.try_emit_bus_tlm_fork(v, Some(&l.name.name), depth) {
                return;
            }
            // `bus.<method>(args)` on the rhs expands the req/rsp
            // transaction-method protocol and captures the response
            // payload into the let when the method returns a value.
            if self.try_emit_bus_tlm_method(v, Some(&l.name.name), depth) {
                return;
            }
            // `bus.<ch>.recv()` on the rhs expands the handshake +
            // captures the payload signal directly into the let.
            // Defer pad emission so the handshake helper can write its
            // own indentation.
            if self.try_emit_bus_handshake(v, Some(&l.name.name), depth) {
                return;
            }
            self.pad(depth);
            // Explicitly typed initialized locals must keep their
            // declared scalar width. In particular, `let x : uint<75> =
            // ...` needs `_harc_u128`, not the historical `int64_t`
            // fallback used for untyped integer-shaped lets.
            let ty = if let Some(t) = &l.ty {
                self.local_value_c_type(t)
            } else if rhs_wants_auto(v, &self.tseq_names) {
                "auto".into()
            } else {
                // Default to `int64_t` for untyped integer-shaped lets
                // so 32-bit DUT signals zero-extend on assignment
                // (matters for the `assert got == expected` pattern
                // when comparing widened C++ ints against narrow
                // Verilator outputs). Switch to `auto` when the rhs is
                // a call — function/tseq/method returns can be
                // `std::vector<T>` or a transaction value, neither of
                // which fit in int64_t.
                "int64_t".into()
            };
            write!(self.out, "{ty} {} = ", l.name.name).ok();
            self.emit_expr(v);
            writeln!(self.out, ";").ok();
        } else if let Some(t) = &l.ty {
            self.pad(depth);
            // No initializer. Transactions / scoreboards default-
            // construct (their struct field defaults run). Transactors
            // additionally register `on`-handlers (see the unbound-
            // transactor arm below).
            let simple = type_simple_name(Some(t));
            if let Some(name) = simple {
                if self.is_record_type(name) || self.scoreboards.contains(name) {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    return;
                }
                // Unbound transactor: `let xact : T mode` for a
                // `transactor T` declared without `bound to BusType`.
                // Same lowering as plain `driver`/`agent` instantiation —
                // default-construct the struct, then walk the synth
                // ComponentDecl to register `on`-handler subscribers.
                // The active half drives raw DUT signals (or sideband)
                // reacting to `emit xact.req(t)`; the passive half (if
                // present) observes via event-typed `on` handlers.
                if let Some(t) = self.transactors.get(name).cloned() {
                    let mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => *m,
                        _ => {
                            self.errors.push(format!(
                                "let {}: transactor instantiation requires a mode annotation (`{} active` or `{} passive`)",
                                l.name.name, name, name,
                            ));
                            return;
                        }
                    };
                    if t.bound_to.is_some() {
                        self.errors.push(format!(
                            "let {} : {}: transactor `{}` has a `bound to` clause; instantiation requires `= bind <bus>`",
                            l.name.name, name, name,
                        ));
                        return;
                    }
                    let include_active = matches!(mode, TransactorMode::Active);
                    let synth = synth_component_from_transactor(&t, include_active);
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    let tag = format!("_xactor_{}_", l.name.name);
                    self.emit_component_handler_registrations(&synth, &l.name.name, depth, &tag);
                    return;
                }
                if let Some(comp) = self.components.get(name).cloned() {
                    // driver / agent / env / sequencer → default-construct
                    // the struct. The user assigns DUT pointers and other
                    // field values explicitly afterward (`drv.dut = dut;`).
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    // Register on-handlers in the body the same way
                    // monitors do — events get subscriber closures,
                    // bool exprs get cycle-trigger checkers. Tag prefix
                    // is the kind keyword so concurrent components at
                    // the same source span don't collide.
                    let tag = format!("_{}_", comp.kind.keyword());
                    self.emit_component_handler_registrations(&comp, &l.name.name, depth, &tag);
                    // Recurse through sub-component / sub-transactor
                    // fields with mode propagation. The let-instance's
                    // mode is the root of the inheritance chain:
                    //
                    //   let topenv : OuterEnv passive
                    //                ^^^^^^^^^^^^^^^^^
                    //   propagates →  env's agent fields         (no mode → passive)
                    //   propagates →    agent's transactor fields (no mode → passive)
                    //
                    // At each level a field-level explicit mode wins
                    // over the inherited mode. See
                    // `emit_subcomponent_handler_registrations`.
                    let root_mode = match l.ty.as_ref() {
                        Some(TypeExpr::Named { mode: Some(m), .. }) => Some(*m),
                        _ => None,
                    };
                    self.emit_subcomponent_handler_registrations(
                        &comp,
                        &l.name.name,
                        root_mode,
                        depth,
                    );
                    // Top-level watchdog (spec §8.6). Install a periodic
                    // `_checkers` closure that fires `<Type>_watchdog(<inst>)`
                    // every `period` cycles. Sub-component watchdogs are
                    // installed by `emit_subcomponent_handler_registrations`.
                    for ci in &comp.items {
                        if let ComponentItem::Watchdog(w) = ci {
                            self.emit_watchdog_checker(&comp, w, &l.name.name, depth);
                        }
                    }
                    // Top-level connect edges (the let's own component's
                    // connect block, if any). Nested sub-component
                    // connects are emitted inside the recursive helper
                    // above so they're prefixed by the sub-instance path.
                    for ci in &comp.items {
                        if let ComponentItem::Connect(cb) = ci {
                            for edge in &cb.edges {
                                self.emit_connect_edge(&comp, &l.name.name, edge, depth);
                            }
                        }
                    }
                    return;
                }
                if let Some(g) = self.covergroups.get(name).cloned() {
                    writeln!(self.out, "{name} {};", l.name.name).ok();
                    // Register an inline sample closure (per-cycle level
                    // trigger). Spec §6 says sampling fires on the
                    // covergroup's clocking expression — for our v0
                    // single-clock primary-clock model, every tick is a
                    // sample. Multi-clock would tie this to the named
                    // clock instead.
                    self.emit_covergroup_sample_registration(&g, &l.name.name, depth);
                    return;
                }
            }
            writeln!(self.out, "int64_t {} = 0;", l.name.name).ok();
        } else {
            self.pad(depth);
            writeln!(self.out, "// let {} (no type / no value)", l.name.name).ok();
        }
    }

    /// Lvalue side of `<-` / `=` — `dut.x` becomes `dut->x`.
    fn emit_lvalue(&mut self, e: &Expr) {
        self.emit_expr_with_arrow(e, true);
    }

    fn emit_expr(&mut self, e: &Expr) {
        self.emit_expr_with_arrow(e, false);
    }

    /// Emit a single-line `<lhs> = <rhs>` style statement, dispatching
    /// on three orthogonal axes:
    ///   1. Pointer-rooted signal target → wrap with the
    ///      `harc_rt::harc_*` template helpers so `VlWide<N>` ports
    ///      compile against integer values.
    ///   2. Plain local target → bare `lhs = rhs;`.
    ///   3. RHS is a > 128-bit hex literal → route through
    ///      `harc_rt::harc_assign_words` with an
    ///      `std::initializer_list<uint32_t>`. Without this, the
    ///      literal would clamp at 128 bits in `c_int_literal`'s
    ///      composite path and silently lose the high bits.
    /// If `target` is a `regs.NAME` field access where `regs` is a
    /// RAL regblock binding, return `(regs_var, helper_var, helper_ty,
    /// offset_literal, register-access-policy)`. Used by
    /// `emit_signal_assignment` (write side) and `emit_expr_with_arrow`
    /// (read side).
    /// Compile-time-unrolled bitbash walk. Detects a bare statement
    /// `bitbash(<regs_ident>)` where the argument is a `let regs : R =
    /// bind helper` binding, and emits one write-all-ones / read-back
    /// + write-zero / read-back pair per RW register in the regblock.
    /// RO/WO registers are skipped (RO can't accept the write; WO
    /// can't be read meaningfully). Failures bump the `errors`
    /// counter and log via `sim_log_line` so the existing test-result
    /// machinery picks them up.
    fn try_emit_bitbash(&mut self, e: &Expr, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Ident(name) = &*callee.kind else {
            return false;
        };
        if name.name != "bitbash" || args.len() != 1 {
            return false;
        }
        let arg = match &args[0] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let ExprKind::Ident(regs_id) = &*arg.kind else {
            return false;
        };
        let Some(regs_ty) = self.let_types.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(block) = self.regblocks.get(&regs_ty).cloned() else {
            return false;
        };
        let Some(helper_var) = self.let_helper.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(helper_ty) = self.let_types.get(&helper_var).cloned() else {
            return false;
        };

        let default_w = block.default_width.unwrap_or(32);
        self.pad(depth);
        writeln!(
            self.out,
            "// bitbash({}) — RAL walk-all over RW regs of {}",
            regs_id.name, regs_ty
        )
        .ok();
        for reg in &block.registers {
            let w = reg.width.unwrap_or(default_w);
            let mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let off = c_int_literal_from(&reg.offset.kind);
            let regname = &reg.name.name;
            if !reg.access.writes_to_bus() || !reg.access.reads_from_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// bitbash: skipping {} (access {})",
                    regname,
                    reg.access.keyword()
                )
                .ok();
                continue;
            }
            // Two patterns: all-ones (masked to register width), then
            // zero. Each pair: write → read → compare → bump errors
            // if mismatch + sim_log.
            for (pat_label, pat) in [("ones", mask), ("zero", 0u64)] {
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                writeln!(self.out, "uint64_t _bb_pat = 0x{pat:x}ull;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{helper_ty}_write({helper_var}, {off}, _bb_pat);").ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "uint64_t _bb_got = {helper_ty}_read({helper_var}, {off});"
                )
                .ok();
                self.pad(depth + 1);
                writeln!(self.out, "if (_bb_got != _bb_pat) {{").ok();
                self.pad(depth + 2);
                writeln!(self.out,
                    "sim_log_line(\"FAIL\", \"bitbash {regname} {pat_label}: wrote 0x%llx, got 0x%llx\", (long long)_bb_pat, (long long)_bb_got);").ok();
                self.pad(depth + 2);
                writeln!(self.out, "ctx.errors++;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
        true
    }

    /// `regs.record_write(addr, data)` — **PASSIVE PATH** mirror-only
    /// record of an observed bus write, keyed by address. Decodes
    /// `addr` against the regblock's register offsets at codegen time
    /// (no bus traffic — the checker already saw the transaction),
    /// updates the matching mirror cell masked to the register width,
    /// **then fires `<regs>_cbs.REG(data)` if a per-register callback
    /// is registered**.
    ///
    /// This is the only emission site that dispatches RAL callbacks.
    /// The two ACTIVE frontdoor write paths above (`regs.NAME = expr`
    /// and `regs.REG.FIELD = expr`) intentionally do NOT dispatch —
    /// see their comments and `docs/ral-support.md` §3.2 for why.
    /// The asymmetry is locked from silent drift by the
    /// `regblock_active_frontdoor_write_does_not_dispatch_callback`
    /// test in `tests/codegen.rs`.
    ///
    /// Returns false (no-op) if the call shape isn't a record_write on
    /// a regblock binding.
    fn try_emit_record_write(&mut self, e: &Expr, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Field { target, name } = &*callee.kind else {
            return false;
        };
        if name.name != "record_write" || args.len() != 2 {
            return false;
        }
        let ExprKind::Ident(regs_id) = &*target.kind else {
            return false;
        };
        let Some(regs_ty) = self.let_types.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(block) = self.regblocks.get(&regs_ty).cloned() else {
            return false;
        };
        let addr = match &args[0] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let data = match &args[1] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let regs_var = &regs_id.name;
        let default_w = block.default_width.unwrap_or(32);
        self.pad(depth);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 1);
        write!(self.out, "uint64_t _rec_addr = (uint64_t)(").ok();
        self.emit_expr(addr);
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        write!(self.out, "uint64_t _rec_data = (uint64_t)(").ok();
        self.emit_expr(data);
        writeln!(self.out, ");").ok();
        // Recursion guard: a callback body can call `record_write` again
        // (legitimately, to model a CSR side-effect that writes another
        // register, or accidentally, to write the same register). Bump
        // a per-binding depth counter on entry and abort if it crosses
        // `HARC_RAL_CB_MAX_DEPTH` — without the bound a self-write
        // would blow the stack. See docs/ral-support.md §3.2.
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if ({regs_var}_cb_depth >= HARC_RAL_CB_MAX_DEPTH) {{ sim_log_line(\"FATAL\", \"RAL record_write callback recursion exceeded HARC_RAL_CB_MAX_DEPTH (%u) on binding `{regs_var}` at addr 0x%llx\", (unsigned)HARC_RAL_CB_MAX_DEPTH, (unsigned long long)_rec_addr); ctx.errors++; _fatal = true; }} else {{",
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "{regs_var}_cb_depth++;").ok();
        // Emit an if/else-if chain so a single addr matches at most one
        // register. Register offsets *should* be unique, but the regblock
        // parser doesn't enforce uniqueness here, so chaining keeps
        // record_write semantics deterministic if a duplicate slips
        // through (the first-declared register wins, matching
        // first-match dispatch elsewhere in cpp_tb).
        for (i, reg) in block.registers.iter().enumerate() {
            let w = reg.width.unwrap_or(default_w);
            let mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let cty = mirror_field_c_type(w);
            let off = c_int_literal_from(&reg.offset.kind);
            let regname = &reg.name.name;
            self.pad(depth + 2);
            let kw = if i == 0 { "if" } else { "else if" };
            writeln!(
                self.out,
                "{kw} (_rec_addr == (uint64_t)({off})) {{ {regs_var}.{regname} = ({cty})(_rec_data & 0x{mask:x}ull); if ({regs_var}_cbs.{regname}) {regs_var}_cbs.{regname}(_rec_data); }}",
            )
            .ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "{regs_var}_cb_depth--;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
        true
    }

    /// 3-level `chip.inst.REG` addrmap access. Returns
    /// `(chip_var, instance_path, helper_var, helper_ty,
    ///   effective_offset_expr, register-access-policy)`.
    /// 3-level `chip.inst.REG` addrmap access. If `inst` is aliased
    /// to another instance via `alias of`, the returned
    /// `instance_path` points at the TARGET's mirror cell (one
    /// storage shared across windows) while the effective bus
    /// offset still uses this instance's own base.
    fn resolve_addrmap_register_lookup(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, String, RegAccess)> {
        let ExprKind::Field {
            target: mid,
            name: reg_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: inst_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip_id) = &*outer.kind else {
            return None;
        };
        let chip_ty = self.let_types.get(&chip_id.name)?;
        let amap = self.addrmaps.get(chip_ty)?;
        let inst = amap
            .instances
            .iter()
            .find(|i| i.name.name == inst_name.name)?;
        let block = self.regblocks.get(&inst.regblock_ty.name)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let helper_var = self.let_helper.get(&chip_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let base = c_int_literal_from(&inst.base_addr.kind);
        let off = c_int_literal_from(&reg.offset.kind);
        let effective = format!("({base} + {off})");
        let mirror_inst_name = inst
            .alias_of
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| inst.name.name.clone());
        let instance_path = format!("{}.{}", chip_id.name, mirror_inst_name);
        Some((
            chip_id.name.clone(),
            instance_path,
            helper_var,
            helper_ty,
            effective,
            reg.access,
        ))
    }

    /// 4-level `chip.inst.REG.FIELD` addrmap+field access. Alias-
    /// aware like its 3-level sibling.
    fn resolve_addrmap_subfield_lookup(
        &self,
        target: &Expr,
    ) -> Option<(
        String,
        String,
        String,
        String,
        String,
        String,
        &'static str,
        u32,
        u32,
        RegAccess,
    )> {
        let ExprKind::Field {
            target: lvl3,
            name: fld_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl2,
            name: reg_name,
        } = &*lvl3.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl1,
            name: inst_name,
        } = &*lvl2.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip_id) = &*lvl1.kind else {
            return None;
        };
        let chip_ty = self.let_types.get(&chip_id.name)?;
        let amap = self.addrmaps.get(chip_ty)?;
        let inst = amap
            .instances
            .iter()
            .find(|i| i.name.name == inst_name.name)?;
        let block = self.regblocks.get(&inst.regblock_ty.name)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name.name == fld_name.name)?;
        let helper_var = self.let_helper.get(&chip_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let base = c_int_literal_from(&inst.base_addr.kind);
        let off = c_int_literal_from(&reg.offset.kind);
        let effective = format!("({base} + {off})");
        let reg_width = reg.width.unwrap_or(block.default_width.unwrap_or(32));
        let reg_c_type = mirror_field_c_type(reg_width);
        let bit_width = field_bit_width(&fld.ty);
        let mirror_inst_name = inst
            .alias_of
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| inst.name.name.clone());
        let instance_path = format!("{}.{}", chip_id.name, mirror_inst_name);
        Some((
            chip_id.name.clone(),
            instance_path,
            reg.name.name.clone(),
            helper_var,
            helper_ty,
            effective,
            reg_c_type,
            fld.bit_pos,
            bit_width,
            fld.access,
        ))
    }

    fn resolve_regblock_field_lookup(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, RegAccess)> {
        let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(regs_id) = &*outer.kind else {
            return None;
        };
        let regs_ty = self.let_types.get(&regs_id.name)?;
        let block = self.regblocks.get(regs_ty)?;
        let reg = block.registers.iter().find(|r| r.name.name == name.name)?;
        let helper_var = self.let_helper.get(&regs_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let offset_lit = c_int_literal_from(&reg.offset.kind);
        Some((
            regs_id.name.clone(),
            helper_var,
            helper_ty,
            offset_lit,
            reg.access,
        ))
    }

    fn resolve_regblock_field_write(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, RegAccess)> {
        self.resolve_regblock_field_lookup(target)
    }

    /// If `target` is a 3-level `regs.REG.FIELD` access where `regs`
    /// is a RAL regblock binding and FIELD is declared inside REG,
    /// return all metadata needed to emit a masked write / shifted
    /// read: `(regs_var, helper_var, helper_ty, offset_lit, reg_name,
    /// reg_c_type, bit_pos, bit_width, field-access-policy)`.
    fn resolve_regblock_subfield_lookup(
        &self,
        target: &Expr,
    ) -> Option<(
        String,
        String,
        String,
        String,
        String,
        &'static str,
        u32,
        u32,
        RegAccess,
    )> {
        let ExprKind::Field {
            target: mid,
            name: fld_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: reg_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(regs_id) = &*outer.kind else {
            return None;
        };
        let regs_ty = self.let_types.get(&regs_id.name)?;
        let block = self.regblocks.get(regs_ty)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name.name == fld_name.name)?;
        let helper_var = self.let_helper.get(&regs_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let offset_lit = c_int_literal_from(&reg.offset.kind);
        let reg_width = reg.width.unwrap_or(block.default_width.unwrap_or(32));
        let reg_c_type = mirror_field_c_type(reg_width);
        let bit_width = field_bit_width(&fld.ty);
        Some((
            regs_id.name.clone(),
            helper_var,
            helper_ty,
            offset_lit,
            reg.name.name.clone(),
            reg_c_type,
            fld.bit_pos,
            bit_width,
            fld.access,
        ))
    }

    /// Resolve the SV port name for `<bus>.<channel>.<signal>` access.
    /// Checks the `bind ... with { ch.sig: "port" }` remap first;
    /// falls back to the `<prefix>_<channel>_<signal>` convention.
    /// `prefix` is typically the bind variable's name (same as the
    /// `sig_prefix` field stored in `bus_bindings`).
    fn bus_signal_name(&self, prefix: &str, channel: &str, signal: &str) -> String {
        if let Some(map) = self.bus_remap.get(prefix) {
            if let Some(port) = map.get(&(channel.to_string(), signal.to_string())) {
                return port.clone();
            }
        }
        format!("{prefix}_{channel}_{signal}")
    }

    /// If `target` is `dut.<probe>` where the probe was declared with
    /// the `force` modifier, return its `ProbeAccessor`. Used by
    /// `emit_signal_assignment` and the `release` statement to emit
    /// the two-store (drv + en) lowering.
    fn resolve_force_probe(&self, target: &Expr) -> Option<ProbeAccessor> {
        let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return None;
        };
        if id.name != "dut" {
            return None;
        }
        let probe = self.probes.get(&name.name)?;
        if probe.force {
            Some(probe.clone())
        } else {
            None
        }
    }

    /// If `e` is an index into a DUT port that flattens to a packed
    /// multi-lane SV vector (i.e. `dut.<port>[i]` with `<port>` recorded
    /// in `vec_lane_widths`), return `(port-name, lane-width, &index)`.
    /// Used to route the lane access through the backend-agnostic
    /// `harc_rt::harc_vec_lane_*` helpers instead of a raw C++ subscript
    /// (which only works on the ARCH native sim's array port, not on
    /// Verilator's packed scalar). Returns `None` for any other shape —
    /// including indexing a true unpacked-`Vec` port (those aren't in the
    /// map; their direct `port[i]` array index is correct on both
    /// backends).
    fn dut_packed_lane<'e>(&self, e: &'e Expr) -> Option<(String, String, u32, &'e Expr)> {
        let ExprKind::Index { target, index } = &*e.kind else {
            return None;
        };
        let ExprKind::Field {
            target: root,
            name: port,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(root_id) = &*root.kind else {
            return None;
        };
        if !self.pointer_vars.contains(&root_id.name) {
            return None;
        }
        // The map is keyed by top-module DUT port names (from the `--sv`
        // port table); only the DUT pointer's ports can match.
        let w = *self.vec_lane_widths.get(&port.name)?;
        Some((root_id.name.clone(), port.name.clone(), w, index))
    }

    fn emit_signal_assignment(&mut self, target: &Expr, value: &Expr, depth: usize) {
        // Packed multi-lane `Vec<Bus>` port lane write: `dut.<port>[i] =
        // expr`. On the `--sv` (Verilator) backend `<port>` is a single
        // packed scalar, so a raw `dut-><port>[i] = …` subscript won't
        // compile; route through `harc_vec_lane_write<W>` which bit-
        // deposits the lane (and still array-indexes the ARCH native sim
        // port via `if constexpr`). See `vec_lane_widths_from_sv`.
        if let Some((root, port, w, index)) = self.dut_packed_lane(target) {
            write!(
                self.out,
                "harc_rt::harc_vec_lane_write<{w}>({root}->{port}, (std::size_t)("
            )
            .ok();
            self.emit_expr(index);
            write!(self.out, "), ").ok();
            self.emit_expr(value);
            writeln!(self.out, ");").ok();
            return;
        }
        // `probe force <name>` write: lower to a paired store of
        // `<name>_drv = expr` and `<name>_en = 1`. See
        // docs/probe-signals.md. The bound SV stub's `always_comb`
        // picks up `_en=1` next cycle and procedurally forces the
        // target path with the latched `_drv` value.
        if let Some(probe) = self.resolve_force_probe(target) {
            write!(self.out, "dut->rootp->{} = ", probe.drive()).ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            self.pad(depth);
            writeln!(self.out, "dut->rootp->{} = 1;", probe.enable()).ok();
            return;
        }
        // Plain-probe write: not allowed. Surface a clear codegen
        // error rather than emit invalid C++. Read-only probes
        // can't drive their target signal — declare with `force`
        // for fault injection.
        if let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        {
            if let ExprKind::Ident(id) = &*outer.kind {
                if id.name == "dut" && self.probes.contains_key(&name.name) {
                    self.errors.push(format!(
                        "write to `dut.{}`: read-only probe — declare with `probe force` to enable fault injection",
                        name.name,
                    ));
                    return;
                }
            }
        }

        // RAL addrmap subfield write: `chip.inst.REG.FIELD = expr`.
        // Identical lowering to the regblock subfield path, but the
        // bus offset is `base(inst) + offset(REG)` and the mirror path
        // walks through the instance: `chip.inst.REG`.
        if let Some((
            _chip_var,
            inst_path,
            reg_name,
            helper_var,
            helper_ty,
            effective_off,
            reg_c_type,
            bit_pos,
            bit_width,
            access,
        )) = self.resolve_addrmap_subfield_lookup(target)
        {
            let mask = field_mask_literal(bit_width);
            write!(
                self.out,
                "{inst_path}.{reg_name} = ({inst_path}.{reg_name} & ~(({reg_c_type})0x{mask:x}u << {bit_pos})) | (((({reg_c_type})(",
            ).ok();
            self.emit_expr(value);
            writeln!(self.out, ")) & 0x{mask:x}u) << {bit_pos});",).ok();
            if access.writes_to_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {effective_off}, {inst_path}.{reg_name});",
                )
                .ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO field — write to bus suppressed (chip mirror still updated)"
                )
                .ok();
            }
            return;
        }

        // RAL addrmap register write: `chip.inst.REG = expr`.
        if let Some((_chip_var, inst_path, helper_var, helper_ty, effective_off, access)) =
            self.resolve_addrmap_register_lookup(target)
        {
            // The outer Field's `name` is the register name itself.
            write!(self.out, "{inst_path}.").ok();
            if let ExprKind::Field { name, .. } = &*target.kind {
                write!(self.out, "{}", name.name).ok();
            }
            write!(self.out, " = ").ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            if access.writes_to_bus() {
                self.pad(depth);
                write!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {effective_off}, "
                )
                .ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO register — write to bus suppressed (mirror updated)"
                )
                .ok();
            }
            return;
        }

        // RAL frontdoor field-level write: `regs.REG.FIELD = expr`.
        // Lowers to a read-modify-write on the mirror (mask out the
        // FIELD bits, OR in the new value shifted into POS) + a bus
        // write of the full register word. Checked before the
        // register-level path because it's strictly more specific
        // (3-level Field expr vs 2-level).
        //
        // **ACTIVE PATH — no callback dispatch.** `on regs.REG`
        // callbacks fire only on the PASSIVE `record_write` decode
        // (see `try_emit_record_write` below). The active frontdoor
        // is intentionally silent: callbacks are for observing
        // externally-driven mirror updates, not the test's own
        // writes. See `docs/ral-support.md` §3.2 (active-vs-passive
        // asymmetry). If you add callback dispatch here you'll
        // double-fire on any test that mixes both paths.
        if let Some((
            regs_var,
            helper_var,
            helper_ty,
            offset_lit,
            reg_name,
            reg_c_type,
            bit_pos,
            bit_width,
            access,
        )) = self.resolve_regblock_subfield_lookup(target)
        {
            let mask = field_mask_literal(bit_width);
            // Mirror update always happens; even RO mirrors might be
            // useful for the read-predict side. For RO we then DROP
            // the bus write — matches the RFC `ro` semantics.
            write!(
                self.out,
                "{regs_var}.{reg_name} = ({regs_var}.{reg_name} & ~(({reg_c_type})0x{mask:x}u << {bit_pos})) | (((({reg_c_type})(",
            ).ok();
            self.emit_expr(value);
            writeln!(self.out, ")) & 0x{mask:x}u) << {bit_pos});",).ok();
            if access.writes_to_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {offset_lit}, {regs_var}.{reg_name});",
                )
                .ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO field — write to bus suppressed (regs.{reg_name}.<field> mirror still updated)",
                ).ok();
            }
            return;
        }

        // RAL frontdoor write: `regs.NAME = expr` where `regs` is a
        // `let regs : <Regblock> = bind <helper>` instantiation. Lowers
        // to mirror update + `<HelperType>_write(helper, OFFSET, expr)`.
        //
        // **ACTIVE PATH — no callback dispatch.** Same asymmetry as
        // the field-level frontdoor above: `on regs.NAME` callbacks
        // are passive-only, fired by `record_write` decode. Adding
        // dispatch here would double-fire and break the documented
        // contract — see `docs/ral-support.md` §3.2.
        if let Some((regs_var, helper_var, helper_ty, offset_lit, access)) =
            self.resolve_regblock_field_write(target)
        {
            write!(self.out, "{regs_var}.").ok();
            if let ExprKind::Field { name, .. } = &*target.kind {
                write!(self.out, "{}", name.name).ok();
            }
            write!(self.out, " = ").ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            if access.writes_to_bus() {
                self.pad(depth);
                write!(self.out, "{helper_ty}_write({helper_var}, {offset_lit}, ").ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO register — write to bus suppressed (mirror updated)"
                )
                .ok();
            }
            return;
        }

        let pointer_rooted = self.is_pointer_rooted_signal_lvalue(target);
        let wide_words = is_wide_int_literal(value);

        match (pointer_rooted, wide_words) {
            (true, Some(words)) => {
                // > 128-bit literal into a wide signal — word-list path.
                // Route through the *checked* helper, parameterized by the
                // number of 32-bit words the literal's value actually needs
                // (`significant_word_count`). The helper `static_assert`s
                // that this fits the target signal's word capacity, turning
                // a previously-silent over-width truncation (high words
                // dropped / message misaligned) into a hard, named C++
                // compile error. Leading-zero high words don't count, so a
                // value that fits a wider-than-necessary literal is fine.
                let req_words = significant_word_count(&words);
                write!(self.out, "harc_rt::harc_assign_words_checked<{req_words}>(").ok();
                self.emit_lvalue(target);
                write!(self.out, ", {{").ok();
                for (i, w) in words.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ").ok();
                    }
                    write!(self.out, "{w}").ok();
                }
                writeln!(self.out, "}});").ok();
            }
            (true, None) => {
                // Pointer-rooted signal, ≤128b RHS — uniform helper path.
                write!(self.out, "harc_rt::harc_assign(").ok();
                self.emit_lvalue(target);
                write!(self.out, ", ").ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            }
            (false, _) => {
                // Plain local — bare assignment. (If RHS happens to be
                // a > 128-bit literal here it'll clamp via
                // `c_int_literal`'s fallback; locals aren't wide
                // today so this is a corner that doesn't show up.)
                self.emit_lvalue(target);
                write!(self.out, " = ").ok();
                self.emit_expr(value);
                writeln!(self.out, ";").ok();
            }
        }
    }

    /// Returns true if `e` is an L-value that ultimately lowers to a
    /// `<root>-><field>...` access chain (where `<root>` is a DUT pointer
    /// or bus-binding root). These are the cases where Verilator may
    /// have given the leaf signal a wide type (`VlWide<N>` for >64-bit
    /// ports), so plain `lhs = rhs` won't compile and we instead emit
    /// a `harc_rt::harc_assign(lhs, rhs);` call.
    fn is_pointer_rooted_signal_lvalue(&self, e: &Expr) -> bool {
        // Walk down field-access chain; root must be Ident in either
        // pointer_vars (e.g. `dut`) or bus_bindings (e.g. `axil`).
        let mut cur: &Expr = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, .. } => cur = target,
                ExprKind::Ident(id) => {
                    return self.pointer_vars.contains(&id.name)
                        || self.bus_bindings.contains_key(&id.name);
                }
                _ => return false,
            }
        }
    }

    fn emit_expr_with_arrow(&mut self, e: &Expr, lvalue: bool) {
        // Property-time substitutions for temporal SystemCall expressions.
        // `emit_property_check` populates `prop_subs` with span → snippet for
        // each `$past`/`$rose`/`$fell`/`$stable` occurrence; if we encounter
        // one here, emit the snippet instead of recursing into its inner.
        if !self.prop_subs.is_empty() {
            if let Some(s) = self.prop_subs.get(&(e.span.start, e.span.end)) {
                write!(self.out, "{s}").ok();
                return;
            }
        }
        match &*e.kind {
            ExprKind::Int(s) => {
                write!(self.out, "{}", c_value_literal(s)).ok();
            }
            ExprKind::Float(s) => {
                write!(self.out, "{s}").ok();
            }
            ExprKind::Time(s) => {
                // Strip the unit suffix and emit the numeric portion.
                let n: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_')
                    .collect();
                write!(self.out, "{n}").ok();
            }
            ExprKind::String(s) => {
                write!(self.out, "\"{}\"", escape_c(s)).ok();
            }
            ExprKind::Bool(b) => {
                write!(self.out, "{}", if *b { "true" } else { "false" }).ok();
            }
            ExprKind::Ident(id) => {
                // Monitor-body bare-name rewrite — see field_subs.
                if let Some(s) = self.field_subs.get(&id.name) {
                    write!(self.out, "{s}").ok();
                } else {
                    write!(self.out, "{}", id.name).ok();
                }
            }
            ExprKind::ImplicitSelf => {}
            ExprKind::Field { target, name } => {
                // RAL addrmap subfield read: `chip.inst.REG.FIELD`.
                // Mirror path is `chip.inst.REG`; offset is base+reg_off.
                if !lvalue {
                    if let Some((
                        _chip_var,
                        inst_path,
                        reg_name,
                        helper_var,
                        helper_ty,
                        effective_off,
                        _reg_c_type,
                        bit_pos,
                        bit_width,
                        access,
                    )) = self.resolve_addrmap_subfield_lookup(e)
                    {
                        let mask = field_mask_literal(bit_width);
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "((({inst_path}.{reg_name} = {helper_ty}_read({helper_var}, {effective_off})) >> {bit_pos}) & 0x{mask:x}u)",
                            ).ok();
                        } else {
                            write!(
                                self.out,
                                "(({inst_path}.{reg_name} >> {bit_pos}) & 0x{mask:x}u)",
                            )
                            .ok();
                        }
                        return;
                    }
                }
                // RAL addrmap register read: `chip.inst.REG`.
                if !lvalue {
                    if let Some((
                        _chip_var,
                        inst_path,
                        helper_var,
                        helper_ty,
                        effective_off,
                        access,
                    )) = self.resolve_addrmap_register_lookup(e)
                    {
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "({inst_path}.{} = {helper_ty}_read({helper_var}, {effective_off}))",
                                name.name,
                            ).ok();
                        } else {
                            write!(self.out, "{inst_path}.{}", name.name).ok();
                        }
                        return;
                    }
                }
                // RAL frontdoor field-level read: `regs.REG.FIELD`.
                // For RW/RO: `((regs.REG = <H>_read(helper, OFFSET)) >> POS) & MASK`
                // — bus read updates the mirror (read-side predict) AND
                // returns the value, then bit-extract.
                // For WO: `((regs.REG >> POS) & MASK)` — the bus would
                // return garbage on a WO register, so we serve from the
                // mirror.
                if !lvalue {
                    if let Some((
                        regs_var,
                        helper_var,
                        helper_ty,
                        offset_lit,
                        reg_name,
                        _reg_c_type,
                        bit_pos,
                        bit_width,
                        access,
                    )) = self.resolve_regblock_subfield_lookup(e)
                    {
                        let mask = field_mask_literal(bit_width);
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "((({regs_var}.{reg_name} = {helper_ty}_read({helper_var}, {offset_lit})) >> {bit_pos}) & 0x{mask:x}u)",
                            ).ok();
                        } else {
                            // WO — serve from mirror only.
                            write!(
                                self.out,
                                "(({regs_var}.{reg_name} >> {bit_pos}) & 0x{mask:x}u)",
                            )
                            .ok();
                        }
                        return;
                    }
                }
                // RAL frontdoor register-level read: `regs.NAME` rvalue.
                // RW/RO: `(regs.NAME = <H>_read(helper, OFFSET))` —
                // assignment-expression form so the mirror updates AND
                // the expression yields the value. WO: serve from
                // mirror only.
                if !lvalue {
                    if let Some((regs_var, helper_var, helper_ty, offset_lit, access)) =
                        self.resolve_regblock_field_lookup(e)
                    {
                        // The `name` of the outer Field expr is the
                        // register name itself.
                        if access.reads_from_bus() {
                            write!(
                                self.out,
                                "({regs_var}.{} = {helper_ty}_read({helper_var}, {offset_lit}))",
                                name.name,
                            )
                            .ok();
                        } else {
                            write!(self.out, "{regs_var}.{}", name.name).ok();
                        }
                        return;
                    }
                }
                // Probe lowering. `dut.<name>` where <name> was declared
                // as a `probe` on the `let dut : T` decl resolves to
                // `dut->rootp-><mangled>`, not the (non-existent) top-
                // level `dut-><name>`. Reads still wrap with harc_read
                // so wide signals downcast cleanly. See
                // docs/probe-signals.md.
                if let ExprKind::Ident(id) = &*target.kind {
                    if id.name == "dut" {
                        if let Some(probe) = self.probes.get(&name.name).cloned() {
                            // Reads always come from the read-side
                            // accessor regardless of `force`. Writes
                            // are handled elsewhere (emit_signal_
                            // assignment) since they expand to a
                            // two-statement drv+en pair.
                            if lvalue {
                                write!(self.out, "dut->rootp->{}", probe.read).ok();
                            } else {
                                write!(self.out, "harc_rt::harc_read(dut->rootp->{})", probe.read)
                                    .ok();
                            }
                            return;
                        }
                    }
                }
                // Bus-bound binding lowering. If `target` is an Ident
                // bound to a bus (`let axil : BusAxiLite = bind dut`),
                // resolve `axil.signal` to `<root>-><axil>_<signal>`.
                // Two-level form (`axil.aw.addr`) walks into the bus's
                // handshake_channel groupings and emits
                // `<root>-><axil>_aw_addr`.
                if let Some(s) = self.try_emit_bus_field_access(target, name) {
                    if lvalue {
                        write!(self.out, "{s}").ok();
                    } else {
                        // Wrap reads with harc_rt::harc_read so wide
                        // signals (Verilator's VlWide<N> for >64-bit
                        // ports) implicitly convert to uint64_t. For
                        // narrow signals it's a no-op cast.
                        write!(self.out, "harc_rt::harc_read({s})").ok();
                    }
                    return;
                }
                // Pointer-typed identifiers (DUTs, threaded as Named-type
                // params) use `->`. Everything else uses `.`. Tracked in
                // `pointer_vars` — populated from `let x : <NamedType>` and
                // function params with a Named type (see emit_function).
                let is_pointer_root = matches!(&*target.kind,
                    ExprKind::Ident(id) if self.pointer_vars.contains(&id.name)
                );
                let wrap_read = is_pointer_root && !lvalue;
                if wrap_read {
                    write!(self.out, "harc_rt::harc_read(").ok();
                }
                self.emit_expr(target);
                if is_pointer_root {
                    write!(self.out, "->").ok();
                } else {
                    write!(self.out, ".").ok();
                }
                write!(self.out, "{}", name.name).ok();
                if wrap_read {
                    write!(self.out, ")").ok();
                }
            }
            ExprKind::Index { target, index } => {
                // Packed multi-lane `Vec<Bus>` port lane read:
                // `dut.<port>[i]`. On Verilator `<port>` is a packed
                // scalar, so a raw `dut-><port>[i]` subscript won't
                // compile; route the lane read through
                // `harc_vec_lane_read<W>` (bit-extract on the packed
                // scalar, direct index on the ARCH native sim array).
                // Only fires for ports recorded in `vec_lane_widths`
                // (built from the `--sv` port table) — genuine
                // unpacked-`Vec` ports and wide-signal `VlWide` word
                // indexing fall through to the legacy subscript below.
                // Read (R-value) only: `harc_vec_lane_read` is not an
                // l-value, so the write side is handled separately in
                // `emit_signal_assignment` (via `harc_vec_lane_write`).
                // Guarding on `!lvalue` keeps any l-value request on the
                // legacy subscript path rather than emitting a
                // non-assignable call.
                if !lvalue {
                    if let Some((root, port, w, idx)) = self.dut_packed_lane(e) {
                        write!(
                            self.out,
                            "harc_rt::harc_vec_lane_read<{w}>({root}->{port}, (std::size_t)("
                        )
                        .ok();
                        self.emit_expr(idx);
                        write!(self.out, "))").ok();
                        return;
                    }
                }
                // Pass `lvalue=true` to suppress the `harc_rt::harc_read`
                // wrap on a pointer-rooted Field target. Indexing a
                // wide signal (VlWide<N>) needs the raw `dut->field`
                // expression so VlWide's `operator[]` is reachable;
                // wrapping with `harc_read` would yield a `uint64_t`
                // which can't be indexed. Narrow signals shouldn't be
                // indexed in the first place — that path errors at
                // C++ compile, which is the desired outcome.
                self.emit_expr_with_arrow(target, /*lvalue*/ true);
                write!(self.out, "[").ok();
                self.emit_expr(index);
                write!(self.out, "]").ok();
            }
            ExprKind::BitSlice { target, hi, lo } => {
                write!(self.out, "harc_rt::harc_bits(").ok();
                self.emit_expr(target);
                write!(self.out, ", (uint32_t)(").ok();
                self.emit_expr(hi);
                write!(self.out, "), (uint32_t)(").ok();
                self.emit_expr(lo);
                write!(self.out, "))").ok();
            }
            ExprKind::Call { callee, args } => {
                if args.is_empty() {
                    if let ExprKind::Field { target, name } = &*callee.kind {
                        if name.name == "len" {
                            self.emit_expr(target);
                            write!(self.out, ".size()").ok();
                            return;
                        }
                    }
                }
                // RAL passive read: `regs.record_read(addr)` — decode the
                // address to the mirror cell with no bus traffic. Lowers
                // to the generated `<Regblock>_record_read` free function.
                if args.len() == 1 {
                    if let ExprKind::Field { target, name } = &*callee.kind {
                        if name.name == "record_read" {
                            if let ExprKind::Ident(regs_id) = &*target.kind {
                                if let Some(regs_ty) = self.let_types.get(&regs_id.name).cloned() {
                                    if self.regblocks.contains_key(&regs_ty) {
                                        let arg = match &args[0] {
                                            CallArg::Expr(ex) => ex,
                                            CallArg::Named { value, .. } => value,
                                        };
                                        write!(
                                            self.out,
                                            "{regs_ty}_record_read({}, (uint64_t)(",
                                            regs_id.name,
                                        )
                                        .ok();
                                        self.emit_expr(arg);
                                        write!(self.out, "))").ok();
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some((comp_ty, receiver, method, current_method, current_method_active)) =
                    self.resolve_bare_sibling_method_call(callee)
                {
                    if self.transactors.contains_key(&comp_ty)
                        && self.method_lives_in_when_active(&comp_ty, &method)
                        && !current_method_active
                    {
                        self.errors.push(format!(
                            "method `{comp_ty}.{current_method}(...)`: bare sibling call `{method}(...)` targets a hookable inside `when active` of transactor `{comp_ty}`. Move `{current_method}` into `when active`, or call `{method}` only from active-only code."
                        ));
                        write!(self.out, "/* unsupported bare active sibling call */ 0").ok();
                        return;
                    }
                    write!(self.out, "{comp_ty}_{method}({receiver}").ok();
                    for a in args.iter() {
                        write!(self.out, ", ").ok();
                        match a {
                            CallArg::Expr(ex) => self.emit_expr(ex),
                            CallArg::Named { value, .. } => self.emit_expr(value),
                        }
                    }
                    write!(self.out, ")").ok();
                    return;
                }
                // Env/test-level quiescence helper: `env.quiesced(N)`
                // aggregates the built-in `idle(N)` predicate over all
                // nested component fields registered under that component.
                // Timed `wait until` expands the same helper into per-leaf
                // predicates so timeout diagnostics name the blocking
                // sub-component.
                if let Some((instances, n_expr)) =
                    self.resolve_component_quiesced_predicate(callee, args)
                {
                    if instances.is_empty() {
                        write!(self.out, "true").ok();
                    } else if instances.len() == 1 {
                        self.emit_idle_predicate(&instances[0], "idle", n_expr);
                    } else {
                        for (i, instance) in instances.iter().enumerate() {
                            if i > 0 {
                                write!(self.out, " && ").ok();
                            }
                            write!(self.out, "(").ok();
                            self.emit_idle_predicate(instance, "idle", n_expr);
                            write!(self.out, ")").ok();
                        }
                    }
                    return;
                }
                // Built-in activity-tracking predicates: `obj.idle(N)`,
                // `obj.idle_in(N)`, `obj.idle_out(N)` lower directly to
                // arithmetic on the auto-injected heartbeat fields.
                // Recognized BEFORE hookable method dispatch so a user
                // can't shadow them. (Spec §7.x — activity tracking.)
                if let Some((instance, kind)) = self.resolve_component_idle_predicate(callee) {
                    if args.len() != 1 {
                        self.errors.push(format!(
                            "{instance}.{kind}: expected 1 cycle-count arg, got {}",
                            args.len(),
                        ));
                        write!(self.out, "false").ok();
                        return;
                    }
                    let n_expr = match &args[0] {
                        CallArg::Expr(e) => e,
                        CallArg::Named { value, .. } => value,
                    };
                    self.emit_idle_predicate(&instance, &kind, n_expr);
                    return;
                }
                // Width-method intrinsics: `.trunc<N>()` / `.zext<N>()` /
                // `.sext<N>()` / `.resize<N>()`. Mirrors arch-com's
                // sim_codegen lowering (src/sim_codegen/mod.rs:2688).
                // Parser emits these as `Call { callee: Field{recv, name},
                // args: [width_expr] }`. The width_expr resolves to a
                // const integer at emit time (literal or const path).
                if self.try_emit_width_method(callee, args) {
                    return;
                }

                // Method-call rewrite: `obj.method(args)` where `obj`'s
                // type is a known component with a `hookable method`
                // lowers to `<Type>_method(obj, args)`. Falls through to
                // the generic call shape when no method match is found,
                // so plain free-function calls keep working.
                if let Some((comp_ty, instance, method)) =
                    self.resolve_component_method_call(callee)
                {
                    // Passive-mode call-site check (spec §8.1):
                    // a hookable declared inside `T.when_active` is
                    // structurally absent from passive instances (the
                    // `when_active` block is elided by
                    // `synth_component_from_transactor` for passive).
                    // Today both halves still emit as free C++ functions
                    // — the actor coroutine is the only thing the
                    // passive mode suppresses — so a `passive_instance
                    // .when_active_method(...)` call would otherwise
                    // silently dispatch into orphan code. Surface it.
                    if self.method_lives_in_when_active(&comp_ty, &method) {
                        let path = Self::extract_method_call_path(callee)
                            .unwrap_or_else(|| vec![instance.clone()]);
                        if let Some(TransactorMode::Passive) = self.resolve_path_mode(&path) {
                            self.errors.push(format!(
                                "method `{}.{}(...)`: hookable `{}` lives inside `when active` of transactor `{}`, \
                                 so it does not exist on a passive instance. Change the let-binding to `{} active`, \
                                 or remove the call. See spec §8.1.",
                                instance, method, method, comp_ty, comp_ty,
                            ));
                            return;
                        }
                    }
                    write!(self.out, "{comp_ty}_{method}({instance}").ok();
                    for a in args.iter() {
                        write!(self.out, ", ").ok();
                        match a {
                            CallArg::Expr(ex) => self.emit_expr(ex),
                            CallArg::Named { value, .. } => self.emit_expr(value),
                        }
                    }
                    write!(self.out, ")").ok();
                    return;
                }
                self.emit_expr(callee);
                write!(self.out, "(").ok();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ").ok();
                    }
                    match a {
                        CallArg::Expr(ex) => self.emit_expr(ex),
                        CallArg::Named { value, .. } => self.emit_expr(value),
                    }
                }
                write!(self.out, ")").ok();
            }
            ExprKind::Cast { expr, ty } => {
                // Emit `((<c_type>)(<inner>))` when the target type
                // maps to a known C++ integer type — mirrors arch-com's
                // postfix `expr as Type` lowering. Width-widening
                // casts MATTER in C++: `1 << 31` against a `int`
                // literal is undefined behavior, while
                // `((uint64_t)1) << 31` is well-defined. Bool↔UInt
                // narrowing also flows through here, with C++'s
                // implicit conversions handling the no-op cases.
                //
                // For non-Builtin target types (struct, named type),
                // drop the cast and emit just the inner expression —
                // those are usually identity casts at the type-system
                // level that don't need a C++ representation.
                if matches!(ty, TypeExpr::Builtin { .. }) {
                    let cty = c_type_for(ty);
                    write!(self.out, "(({cty})(").ok();
                    self.emit_expr(expr);
                    write!(self.out, "))").ok();
                } else {
                    self.emit_expr(expr);
                }
            }
            ExprKind::Unary { op, expr } => {
                let s = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "!",
                    UnaryOp::NotKw => "!",
                    UnaryOp::BitNot => "~",
                };
                write!(self.out, "{s}").ok();
                self.emit_expr(expr);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // Wrapping arithmetic (`+% / -% / *%`, harc#473) masks the
                // result to `max(W(lhs), W(rhs))` bits. v1 used to treat
                // these as pass-through sugar for `+ - *`, so an
                // overflowing `uint<8>` add produced 300 where TB-IR
                // produced 44 — the same source, two answers.
                if matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ) {
                    match self.wrap_result_width(*op, lhs, rhs) {
                        Ok(width) => {
                            self.emit_wrapping_binary(*op, lhs, rhs, width);
                            return;
                        }
                        Err(msg) => {
                            self.errors.push(msg);
                            return;
                        }
                    }
                }
                // Wide-literal == / != routing: when either operand is
                // a > 128-bit hex literal, route through
                // `harc_rt::harc_eq_words(sig, {w0, w1, ...})` so the
                // comparison happens word-by-word instead of through
                // `_harc_u128` (which would silently truncate the
                // literal to 128 bits and produce wrong matches).
                if matches!(op, BinaryOp::Eq | BinaryOp::Ne) {
                    let lhs_words = is_wide_int_literal(lhs);
                    let rhs_is_wide = is_wide_int_literal(rhs).is_some();
                    let words = lhs_words.or_else(|| is_wide_int_literal(rhs));
                    if let Some(words) = words {
                        // The signal side is whichever operand is not the
                        // wide literal. Prefer rhs if both happen to be
                        // (which won't normally happen — rejected by
                        // typechecking — but keeps codegen total).
                        let sig_side = if rhs_is_wide { lhs } else { rhs };
                        if matches!(op, BinaryOp::Ne) {
                            write!(self.out, "(!").ok();
                        }
                        write!(self.out, "harc_rt::harc_eq_words(").ok();
                        // The signal side: pass as L-value (no harc_read
                        // wrap) so the helper sees the raw VlWide<N>.
                        self.emit_expr_with_arrow(sig_side, /*lvalue*/ true);
                        write!(self.out, ", {{").ok();
                        for (i, w) in words.iter().enumerate() {
                            if i > 0 {
                                write!(self.out, ", ").ok();
                            }
                            write!(self.out, "{w}").ok();
                        }
                        write!(self.out, "}})").ok();
                        if matches!(op, BinaryOp::Ne) {
                            write!(self.out, ")").ok();
                        }
                        return;
                    }
                }
                self.emit_expr(lhs);
                let s = c_binary_op(*op);
                write!(self.out, " {s} ").ok();
                self.emit_expr(rhs);
            }
            ExprKind::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                // Wrap in parens so the ternary doesn't bind into a
                // surrounding higher-precedence operator on the C++
                // side (e.g. `a + (cond ? x : y)` lowers correctly).
                write!(self.out, "(").ok();
                self.emit_expr(cond);
                write!(self.out, " ? ").ok();
                self.emit_expr(then_branch);
                write!(self.out, " : ").ok();
                self.emit_expr(else_branch);
                write!(self.out, ")").ok();
            }
            ExprKind::Paren(inner) => {
                write!(self.out, "(").ok();
                self.emit_expr(inner);
                write!(self.out, ")").ok();
            }
            ExprKind::RangeLit { lo, hi } => {
                // Bare ranges outside of `for`/`in` aren't representable in C++.
                // Emit a comment so the user sees the issue without a hard fail.
                write!(self.out, "/* range ").ok();
                if let Some(l) = lo {
                    self.emit_expr(l);
                }
                write!(self.out, "..").ok();
                if let Some(h) = hi {
                    self.emit_expr(h);
                }
                write!(self.out, " */ 0").ok();
            }
            other => {
                self.errors.push(format!(
                    "expression not supported in v0 cpp_tb: {:?}",
                    std::mem::discriminant(other)
                ));
                write!(self.out, "/* unsupported */ 0").ok();
            }
        }
    }
}

/// True if any code path will emit the Z3 solver block. Drives the
/// `harc_z3_rt.h` include + Z3 link flags. The check needs to mirror the
/// codegen's actual decision:
///   * `randomize(t) with <body>` — always solver (user wrote constraints)
///   * bare `randomize(t)` where `t`'s transaction has `keep` items,
///     `[unique]` fields, or auto coverage goals — also solver (keeps and
///     policy preferences are call-site constraints)
pub fn uses_constraint_solver(file: &SourceFile) -> bool {
    let enum_names: std::collections::HashSet<&str> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Enum(e) => Some(e.name.name.as_str()),
            _ => None,
        })
        .collect();
    // First pass: collect names of transactions whose bare `randomize(t)`
    // needs the solver path. False positives are cheap (an unused include);
    // false negatives are compile failures.
    let solver_bearing: std::collections::HashSet<&str> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transaction(t) => {
                if txn_body_has_keep(&t.body) || txn_body_has_solver_policy(&t.body, &enum_names) {
                    Some(t.name.name.as_str())
                } else {
                    None
                }
            }
            Item::Struct(s) => {
                if txn_body_has_keep(&s.body) || fields_have_solver_policy(&s.fields, &enum_names) {
                    Some(s.name.name.as_str())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    fn block(b: &Block, solver_bearing: &std::collections::HashSet<&str>) -> bool {
        b.stmts.iter().any(|s| stmt(s, solver_bearing))
    }
    fn component_item(
        item: &ComponentItem,
        solver_bearing: &std::collections::HashSet<&str>,
    ) -> bool {
        match item {
            ComponentItem::OnHandler(h) => block(&h.body, solver_bearing),
            ComponentItem::Hookable(h) => block(&h.body, solver_bearing),
            ComponentItem::Watchdog(w) => block(&w.body, solver_bearing),
            _ => false,
        }
    }
    fn component_items(
        items: &[ComponentItem],
        solver_bearing: &std::collections::HashSet<&str>,
    ) -> bool {
        items
            .iter()
            .any(|item| component_item(item, solver_bearing))
    }
    fn stmt(s: &Stmt, solver_bearing: &std::collections::HashSet<&str>) -> bool {
        match &s.kind {
            StmtKind::Randomize { with_body, .. } => {
                !with_body.is_empty() || !solver_bearing.is_empty()
            }
            StmtKind::For(f) => block(&f.body, solver_bearing),
            StmtKind::Repeat(r) => block(&r.body, solver_bearing),
            StmtKind::Loop(b) => block(b, solver_bearing),
            StmtKind::While { body, .. } => block(body, solver_bearing),
            StmtKind::If(i) => {
                block(&i.then_block, solver_bearing)
                    || i.else_block
                        .as_ref()
                        .map_or(false, |b| block(b, solver_bearing))
                    || i.elsifs.iter().any(|(_, b)| block(b, solver_bearing))
            }
            StmtKind::Fork(f) => f.branches.iter().any(|b| block(b, solver_bearing)),
            StmtKind::Parallel(bs) | StmtKind::Schedule(bs) => {
                bs.iter().any(|b| block(b, solver_bearing))
            }
            StmtKind::Select(arms) => arms.iter().any(|a| block(&a.action, solver_bearing)),
            StmtKind::On(h) => block(&h.body, solver_bearing),
            StmtKind::After { body, .. } => block(body, solver_bearing),
            _ => false,
        }
    }
    file.items.iter().any(|it| match it {
        Item::Function(f) => block(&f.body, &solver_bearing),
        Item::Tseq(t) => block(&t.body, &solver_bearing),
        Item::Agent(c) | Item::Env(c) | Item::Scoreboard(c) | Item::Sequencer(c) => {
            component_items(&c.items, &solver_bearing)
        }
        Item::Transactor(t) => {
            component_items(&t.items, &solver_bearing)
                || t.when_active
                    .as_ref()
                    .map_or(false, |items| component_items(items, &solver_bearing))
        }
        Item::Test(t) => t.items.iter().any(|ti| match ti {
            TestItem::Stmt(s) => stmt(s, &solver_bearing),
            TestItem::Scope(sc) => {
                sc.setup
                    .as_ref()
                    .map_or(false, |b| block(b, &solver_bearing))
                    || sc.run.as_ref().map_or(false, |b| block(b, &solver_bearing))
                    || sc
                        .check
                        .as_ref()
                        .map_or(false, |b| block(b, &solver_bearing))
                    || sc
                        .teardown
                        .as_ref()
                        .map_or(false, |b| block(b, &solver_bearing))
            }
            _ => false,
        }),
        _ => false,
    })
}

fn named_type_last_segment(t: &TypeExpr) -> Option<&str> {
    match t {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

fn is_list_type(t: &TypeExpr) -> bool {
    matches!(named_type_last_segment(t), Some("list") | Some("List"))
}

fn list_type_args(t: &TypeExpr) -> Option<&[TypeArg]> {
    match t {
        TypeExpr::Named { generics, .. } if is_list_type(t) => Some(generics.as_slice()),
        _ => None,
    }
}

fn list_elem_type(t: &TypeExpr) -> Option<&TypeExpr> {
    list_type_args(t)?.first().and_then(|arg| match arg {
        TypeArg::Type(ty) => Some(ty),
        _ => None,
    })
}

fn fixed_vec_type_args(t: &TypeExpr) -> Option<(&TypeExpr, usize)> {
    let TypeExpr::Builtin {
        name: BuiltinTy::Vec,
        args,
        ..
    } = t
    else {
        return None;
    };
    let elem = match args.first()? {
        TypeArg::Type(ty) => ty,
        _ => return None,
    };
    let len = args.get(1).and_then(type_arg_const_usize)?;
    Some((elem, len))
}

fn type_arg_const_usize(arg: &TypeArg) -> Option<usize> {
    match arg {
        TypeArg::Expr(e) => const_usize_expr(e),
        TypeArg::Named { value, .. } => const_usize_expr(value),
        _ => None,
    }
}

fn const_usize_expr(e: &Expr) -> Option<usize> {
    match &*e.kind {
        ExprKind::Paren(inner) => const_usize_expr(inner),
        ExprKind::Int(s) => {
            let stripped = s.replace('_', "");
            if let Some(rest) = stripped
                .strip_prefix("0x")
                .or_else(|| stripped.strip_prefix("0X"))
            {
                usize::from_str_radix(rest, 16).ok()
            } else if let Some(rest) = stripped
                .strip_prefix("0b")
                .or_else(|| stripped.strip_prefix("0B"))
            {
                usize::from_str_radix(rest, 2).ok()
            } else {
                stripped.parse::<usize>().ok()
            }
        }
        _ => None,
    }
}

fn list_max_len(t: &TypeExpr) -> Option<usize> {
    let args = list_type_args(t)?;
    for arg in args {
        if let TypeArg::Named { name, value } = arg {
            if name.name == "max" {
                return const_usize_expr(value);
            }
        }
    }
    args.get(1).and_then(type_arg_const_usize)
}

fn scalar_field_shape(t: &TypeExpr) -> (u32, bool) {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::Bits | BuiltinTy::UIntCap => {
                (type_arg_width(args).unwrap_or(64), false)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => (type_arg_width(args).unwrap_or(64), true),
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => (1, false),
            BuiltinTy::Int => (32, true),
            _ => (64, false),
        },
        _ => (64, false),
    }
}

fn list_field_info(t: &TypeExpr) -> Option<ListFieldInfo> {
    if !is_list_type(t) {
        return None;
    }
    let elem = list_elem_type(t)?;
    let declared_max_len = list_max_len(t);
    let (elem_width, elem_signed) = scalar_field_shape(elem);
    Some(ListFieldInfo {
        declared_max_len,
        elem_width,
        elem_signed,
    })
}

/// Pick a C++ representation for a transaction field's HARC type. Conservative
/// — small ints get widened to `uint64_t`/`int64_t`; bool stays bool; named
/// types get the bare name (likely an enum which is `int64_t` in v0).
fn txn_field_c_type(t: &TypeExpr) -> String {
    if is_list_type(t) {
        let inner = list_elem_type(t)
            .map(txn_field_c_type)
            .unwrap_or_else(|| "uint64_t".into());
        return format!("std::vector<{inner}>");
    }
    if let Some((elem, len)) = fixed_vec_type_args(t) {
        let inner = txn_field_c_type(elem);
        return format!("std::array<{inner}, {len}>");
    }
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
                cpp_uint_for_width(int_width_from_args(args))
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => cpp_sint_for_width(int_width_from_args(args)),
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "bool".into(),
            _ => "uint64_t".into(),
        },
        // Enums (and other named user types) lower to int64_t. Real type
        // emission lands when we have a proper type system.
        TypeExpr::Named { .. } => "int64_t".into(),
    }
}

/// Default initialiser for a transaction field. Uses the user's `default
/// <expr>` clause when given; falls back to type-appropriate zero / false.
/// Default exprs are restricted to simple forms (int / bool / ident / float)
/// in v0 — anything more complex falls back to `0`.
fn field_default(f: &Field) -> String {
    if let Some(d) = &f.default {
        return format_simple_expr(d);
    }
    if is_list_type(&f.ty) || fixed_vec_type_args(&f.ty).is_some() {
        return "{}".into();
    }
    match &f.ty {
        TypeExpr::Builtin { name, .. } => match name {
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "false".into(),
            _ => "0".into(),
        },
        _ => "0".into(),
    }
}

fn format_simple_expr(e: &Expr) -> String {
    match &*e.kind {
        ExprKind::Int(s) => c_int_literal(s),
        ExprKind::Ident(id) => id.name.clone(),
        ExprKind::Bool(b) => (if *b { "true" } else { "false" }).into(),
        ExprKind::Float(s) => s.clone(),
        _ => "0".into(),
    }
}

/// Extract a width from a builtin's first type-arg if it's a literal (e.g.
/// `uint<32>` → Some(32)). Returns None if the arg isn't a parseable int.
fn type_arg_width(args: &[TypeArg]) -> Option<u32> {
    let arg = args.first()?;
    match arg {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn enum_width(variant_count: usize) -> u32 {
    if variant_count <= 1 {
        1
    } else {
        usize::BITS - (variant_count - 1).leading_zeros()
    }
}

/// Translate a HARC type expression to a C++ type for function parameters
/// and return types. Best-effort and conservative — integer-shaped HARC
/// types collapse to `uint64_t` / `int64_t`, since the actual width on the
/// Verilator side is determined by the DUT port type and we just shovel
/// values through. Bool stays bool. Unknown named types emit the bare
/// identifier (likely won't compile — surfaces the gap to the user).
/// Render a field-access chain like `seq.dispatched` or just `req`
/// as a dot-separated string. Returns `None` for shapes the connect
/// codegen can't handle in v0 (anything other than nested
/// `Field { Field { Ident, _ }, _ }` chains rooted at a bare ident).
fn expr_path_str(e: &Expr) -> Option<String> {
    match &*e.kind {
        ExprKind::Ident(id) => Some(id.name.clone()),
        ExprKind::Field { target, name } => {
            let head = expr_path_str(target)?;
            Some(format!("{head}.{}", name.name))
        }
        _ => None,
    }
}

/// If `event` is the trigger of an `on event_name(arg)` handler,
/// returns `(event_name, arg_binding_name)`. Anything else (a bool
/// expression like `on dut.x && y`, or a malformed call) returns
/// `None`. The binding name falls back to `_v` when the user wrote
/// `on event(_)` or omitted the arg.
/// Detect the `<bus>.<channel>.handshake(<arg>)` form used by bound
/// monitors (Phase 2c). Returns `Some((channel_name, arg_name))`
/// when the expression matches AND the outer `<bus>` ident matches
/// the expected binding name (typically `"bus"`); `None` otherwise.
///
/// The expression is a 3-level tree:
///   ExprKind::Call {
///     callee: Field { target: Field { target: Ident(bus), name: ch },
///                     name: "handshake" },
///     args:   [Expr::Ident(arg)] }
fn extract_bus_handshake_event(event: &Expr, bus_ident: &str) -> Option<(String, String)> {
    let ExprKind::Call { callee, args } = &*event.kind else {
        return None;
    };
    let ExprKind::Field {
        target,
        name: method,
    } = &*callee.kind
    else {
        return None;
    };
    if method.name != "handshake" {
        return None;
    }
    let ExprKind::Field {
        target: outer,
        name: ch,
    } = &*target.kind
    else {
        return None;
    };
    let ExprKind::Ident(id) = &*outer.kind else {
        return None;
    };
    if id.name != bus_ident {
        return None;
    }
    let arg_name = match args.first() {
        Some(CallArg::Expr(e)) => match &*e.kind {
            ExprKind::Ident(id) => id.name.clone(),
            _ => "_v".into(),
        },
        _ => "_v".into(),
    };
    Some((ch.name.clone(), arg_name))
}

fn extract_event_subscription(event: &Expr) -> Option<(String, String)> {
    let ExprKind::Call { callee, args } = &*event.kind else {
        return None;
    };
    let event_name = match &*callee.kind {
        ExprKind::Ident(id) => id.name.clone(),
        _ => return None,
    };
    let arg_name = match args.first() {
        Some(CallArg::Expr(e)) => match &*e.kind {
            ExprKind::Ident(id) => id.name.clone(),
            _ => "_v".into(),
        },
        _ => "_v".into(),
    };
    Some((event_name, arg_name))
}

/// True when `let x = EXPR` should use `auto` rather than `int64_t`
/// for the local's declared type. Picks `auto` whenever the rhs is a
/// call (which can return `std::vector<T>` from a tseq, or a
/// transaction by value from `queue<T>::pop()`, etc.) and falls back
/// to `int64_t` for everything else (DUT signal reads, plain numeric
/// expressions — `int64_t` zero-extends them safely for comparisons
/// against int literals). The `tseq_names` arg is kept for future use
/// but the current decision is broader than tseq alone.
fn rhs_wants_auto(e: &Expr, _tseq_names: &std::collections::HashSet<String>) -> bool {
    match &*e.kind {
        ExprKind::Call { .. } => true,
        // A wrapping operator's residue is UNSIGNED (spec §2.4), and the
        // emitted mask is already spelled `((uint64_t)(…))`. Defaulting
        // the local to `int64_t` reinterpreted that residue as signed, so
        // `let y = a -% 1` on a `uint<64>` gave `y > 0` false under v1 and
        // true under TB-IR, whose local is `uint64_t`. Letting `auto`
        // deduce from the mask puts the two backends on the same type.
        ExprKind::Binary { op, .. } => matches!(
            op,
            BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
        ),
        // `(a -% 1)` is the same wrap: without this the width-64
        // signedness divergence survived a single pair of parentheses.
        ExprKind::Paren(inner) => rhs_wants_auto(inner, _tseq_names),
        _ => false,
    }
}

/// Extract `T` from `TSeq<T>`. Returns the C++ rendering of the inner
/// type. The TSeq builtin always carries exactly one type-arg in
/// well-formed source; if missing or malformed, returns `None` so the
/// caller can fall back to a sentinel type (the user already wrote
/// something well-formed in practice — this guards against synth from
/// degenerate ASTs).
fn tseq_inner_type(t: &TypeExpr) -> Option<String> {
    if let TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    } = t
    {
        if let Some(TypeArg::Type(inner)) = args.first() {
            return Some(c_type_for(inner));
        }
        if let Some(TypeArg::Expr(e)) = args.first() {
            // `TSeq<MyTxn>` parses as Expr(Ident) at the type level — if
            // the path is a single identifier, treat it as a Named type.
            if let ExprKind::Ident(id) = &*e.kind {
                return Some(id.name.clone());
            }
        }
    }
    None
}

/// Pull the bit-width literal out of a builtin integer's args list
/// (`uint<N>`, `bits<N>`, etc.). Returns `None` if the args don't have
/// a recognizable integer literal — caller falls back to a default
/// (typically 64).
fn int_width_from_args(args: &[TypeArg]) -> Option<u32> {
    args.first().and_then(|a| match a {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    })
}

/// Evaluate a HARC expression as a compile-time integer width. Used by
/// the width-method intrinsics (`.trunc<N>()` / `.zext<N>()` /
/// `.sext<N>()` / `.resize<N>()`) to extract `N`. Today recognizes only
/// integer literals (the common case); a future pass could fold `const
/// NAME : <Ty> = <expr>` references too.
fn eval_const_width(e: &Expr) -> Option<u32> {
    match &*e.kind {
        ExprKind::Paren(inner) => eval_const_width(inner),
        ExprKind::Int(s) => {
            let stripped = s.replace('_', "");
            if let Some(rest) = stripped
                .strip_prefix("0x")
                .or_else(|| stripped.strip_prefix("0X"))
            {
                u32::from_str_radix(rest, 16).ok()
            } else if let Some(rest) = stripped
                .strip_prefix("0b")
                .or_else(|| stripped.strip_prefix("0B"))
            {
                u32::from_str_radix(rest, 2).ok()
            } else {
                stripped.parse::<u32>().ok()
            }
        }
        _ => None,
    }
}

/// Pick the C++ value-type for a HARC unsigned integer of the given
/// bit-width:
///   1..64    → uint64_t       (every narrow case widens; native ops)
///   65..128  → _harc_u128     (typedef of __uint128_t; mirrors arch-com)
///   >128     → _harc_u128     (truncates to low 128; whole-signal access
///                              beyond 128b is rare; per-word indexing
///                              via `dut.field[i]` covers the gap)
fn cpp_uint_for_width(w: Option<u32>) -> String {
    match w {
        Some(n) if n > 128 => format!("harc_rt::HarcWide<{}>", n.div_ceil(32)),
        Some(n) if n > 64 => "_harc_u128".into(),
        _ => "uint64_t".into(),
    }
}

fn cpp_sint_for_width(w: Option<u32>) -> String {
    match w {
        // No native __int128 signed in C++ portably — but unsigned
        // ops bit-wise emulate signed for the common ops we care
        // about. Cast at use sites if signedness matters above 64b.
        Some(n) if n > 128 => format!("harc_rt::HarcWide<{}>", n.div_ceil(32)),
        Some(n) if n > 64 => "_harc_u128".into(),
        _ => "int64_t".into(),
    }
}

/// Emit the file-scope `extern "C" { … }` forward-declaration block for
/// every `extern function name(params) -> ret` (spec §9) in `file`, so a
/// user-supplied `--ref-src <file>.cpp` resolves at link time. Shared by
/// the v1 (`cpp_tb`) and TB-IR codegens so both produce byte-identical
/// declarations. Writes nothing when the program declares no extern fns.
pub(crate) fn emit_extern_fn_decls(out: &mut String, file: &SourceFile) {
    let extern_fns: Vec<&ExternFnDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::ExternFn(f) => Some(f),
            _ => None,
        })
        .collect();
    if extern_fns.is_empty() {
        return;
    }
    writeln!(
        out,
        "// extern reference functions (spec §9) — implementations"
    )
    .ok();
    writeln!(
        out,
        "// supplied via `harc sim --ref-src <file>` and linked into the"
    )
    .ok();
    writeln!(out, "// verilator-built binary.").ok();
    writeln!(out, "extern \"C\" {{").ok();
    for f in &extern_fns {
        let param_names = cpp_param_names(&f.params);
        let ret = f
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or_else(|| "void".to_string());
        write!(out, "{INDENT}{ret} {}(", f.name.name).ok();
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                write!(out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or_else(|| "int64_t".to_string());
            write!(out, "{pty} {}", param_names[i]).ok();
        }
        writeln!(out, ");").ok();
    }
    writeln!(out, "}}").ok();
    writeln!(out, "").ok();
}

fn c_type_for(t: &TypeExpr) -> String {
    if is_list_type(t) || fixed_vec_type_args(t).is_some() {
        return txn_field_c_type(t);
    }
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
                cpp_uint_for_width(int_width_from_args(args))
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => cpp_sint_for_width(int_width_from_args(args)),
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => "bool".into(),
            BuiltinTy::String => "const char*".into(),
            BuiltinTy::Time => "uint64_t".into(),
            BuiltinTy::TSeq => {
                // `TSeq<T>` → `const std::vector<T>&` for params (pass-
                // by-reference avoids copying every transaction across
                // method calls). Matches the tseq lowering — a tseq
                // emits a `std::vector<T>` accumulator filled by
                // `yield`. Outside param context this is also a sane
                // local-variable type.
                let inner = tseq_inner_type(t).unwrap_or_else(|| "uint64_t".into());
                format!("const std::vector<{inner}>&")
            }
            BuiltinTy::Queue => {
                // `queue<T>` as a function param — pass by reference to
                // avoid copying the runtime queue. Mirrors the field-type
                // lowering.
                let inner = args
                    .first()
                    .map(|a| match a {
                        TypeArg::Type(ty) => txn_field_c_type(ty),
                        TypeArg::Expr(e) => match &*e.kind {
                            ExprKind::Ident(id) => id.name.clone(),
                            _ => "uint64_t".into(),
                        },
                        _ => "uint64_t".into(),
                    })
                    .unwrap_or_else(|| "uint64_t".into());
                format!("harc_rt::HarcQueue<{inner}>&")
            }
            // Aggregates / verification-only types fall back to the spelling
            // — caller will get a compile error pointing at the gap.
            _ => format!("/* TODO: type {:?} */ uint64_t", name),
        },
        TypeExpr::Named { name, .. } => {
            // User-defined module types lower to Verilator pointers (`VFoo*`).
            // Matches the `let dut : AxiLiteRegs` → `VAxiLiteRegs* dut` rule
            // already used for the test-level DUT decl.
            let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            format!("V{last}*")
        }
    }
}

fn cpp_param_names(params: &[Param]) -> Vec<String> {
    let mut discard_idx = 0usize;
    params
        .iter()
        .map(|p| {
            if p.name.name == "_" {
                let name = format!("_discard{discard_idx}");
                discard_idx += 1;
                name
            } else {
                p.name.name.clone()
            }
        })
        .collect()
}

/// Whether an expression reads a name that more than one `let` declares.
/// Such a name has no single recorded width (see `Emitter::shadowed_lets`),
/// so any width derived from it is not trustworthy for a rejection.
fn references_shadowed_let(e: &Emitter, v: &Expr) -> bool {
    match &*v.kind {
        ExprKind::Ident(id) => e.shadowed_lets.contains(&id.name),
        ExprKind::Paren(inner) => references_shadowed_let(e, inner),
        ExprKind::Binary { lhs, rhs, .. } => {
            references_shadowed_let(e, lhs) || references_shadowed_let(e, rhs)
        }
        ExprKind::Unary { expr, .. } => references_shadowed_let(e, expr),
        // A CAST is where the recursion stops: `wrap_operand_width` takes
        // a cast operand's width from the cast target, not from whatever
        // is inside it, so a shadowed name under one contributes nothing
        // to the width and must not suppress the check. Recursing here
        // let `let b : uint<4> = (a as uint<8>) +% 1` through under v1
        // while TB-IR rejected it.
        ExprKind::Cast { .. } => false,
        _ => false,
    }
}

/// Width of an initializer for the narrowing check, or `None` when the
/// shape carries no width worth comparing.
///
/// A literal that fits in 64 bits is exempt, matching TB-IR, which types
/// one as widthless (`UInt(None)`) and treats that as an assignment
/// wildcard — counting its minimum *value* width would make v1 reject
/// `let b : uint<8> = 300` alone. A literal that does NOT fit is a
/// different case: TB-IR types it as `WideLiteral` and does check it, and
/// v1 was silently storing the low 64 bits of an 80-bit value into a
/// local the user declared 8 bits wide.
fn narrowing_source_width(e: &Emitter, value: &Expr) -> Option<u32> {
    match &*value.kind {
        ExprKind::Paren(inner) => narrowing_source_width(e, inner),
        ExprKind::Int(s) => wide_literal_bit_width(s),
        // A name several `let`s declare has no single recorded width —
        // `let_widths` keeps the last one seen, which may belong to an
        // inner scope that has already closed. Decline rather than
        // reject on a width the value may never have had.
        ExprKind::Ident(id) if e.shadowed_lets.contains(&id.name) => None,
        // A wrap's residue is `max(W(lhs), W(rhs))` bits wide, so it is a
        // width-carrying source: `let x : uint<4> = a +% 1` on an 8-bit
        // `a` narrows. TB-IR sees this (the wrap's implicit mask is a
        // `WidthCast`, which its check covers); v1 accepted it and stored
        // the unmasked 8-bit residue in a 4-bit-declared local.
        ExprKind::Binary { op, lhs, rhs }
            if matches!(
                op,
                BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
            ) =>
        {
            // The operands go through `let_widths` too, so the
            // shadowed-name guard above has to cover them or it is simply
            // bypassed: `let b : uint<8> = a +% 1` after an inner
            // `let a : uint<64>` was rejected on a width `a` never had.
            if references_shadowed_let(e, lhs) || references_shadowed_let(e, rhs) {
                return None;
            }
            e.wrap_result_width(*op, lhs, rhs).ok()
        }
        _ => e.infer_expr_width_best_effort(value),
    }
}

/// `Some(bits)` when an integer literal's text denotes a value wider than
/// 64 bits, using the same bit count TB-IR's `wide_literal_bits` reports
/// so the two backends quote the same number in the same diagnostic.
///
/// `None` means "not a narrowing source": the value fits in 64 bits, or
/// the text is a form this function does not decode. Never guess a width
/// for text that failed to parse — an earlier version returned a
/// hard-coded 129 for any parse failure, which rejected every ARCH sized
/// literal (`8'hFF`, `4'b1010`), a form v1 fully supports and TB-IR
/// explicitly delegates to v1.
fn wide_literal_bit_width(s: &str) -> Option<u32> {
    let t = s.replace('_', "");
    // Sized ARCH literals (`8'hFF`) carry their own width and are v1-only;
    // exempt them exactly as a bare literal that fits is exempt.
    if t.contains('\'') {
        return None;
    }
    let (digits, bits_per_digit) =
        if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            (r, 4)
        } else if let Some(r) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
            (r, 1)
        } else {
            // Decimal: no digit-count shortcut, so bound it with u128 and
            // decline to guess beyond that rather than invent a width.
            return match t.parse::<u128>() {
                Ok(v) if v > u64::MAX as u128 => Some(128 - v.leading_zeros()),
                _ => None,
            };
        };
    // Radix-aligned: the exact width is derivable from the digit count, so
    // this stays correct past 128 bits where `u128` would overflow.
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_digit(1 << bits_per_digit)) {
        return None;
    }
    let lead = u32::from_str_radix(&trimmed[..1], 1 << bits_per_digit).ok()?;
    let lead_bits = 32 - lead.leading_zeros();
    let bits = (trimmed.len() as u32 - 1) * bits_per_digit + lead_bits;
    (bits > 64).then_some(bits)
}

fn c_binary_op(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add | AddWrap => "+",
        Sub | SubWrap => "-",
        Mul | MulWrap => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        AndAnd | AndKw => "&&",
        OrOr | OrKw => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        // Temporal / membership operators have no direct C++ equivalent — they
        // shouldn't appear in v0 cpp_tb input. Emit a placeholder rather than
        // fail hard so the whole emit doesn't abort on one stray operator.
        PipeImplies | PipeImpliesNext | Throughout | Within | Intersect | In | Inside => {
            "/* unsupported-op */ ,"
        }
    }
}

/// Constant-fold a base/size expression to `u64` for the addrmap
/// overlap check. Handles plain integer literals only (hex/decimal/
/// underscored). Non-literal expressions fall back to `None` — the
/// overlap check skips pairs whose bounds aren't computable, which
/// keeps the static analysis honest without surfacing false
/// positives.
fn fold_int_literal(e: &Expr) -> Option<u64> {
    match &*e.kind {
        ExprKind::Int(s) => parse_int_str(s),
        _ => None,
    }
}

fn fold_signed_int_literal(e: &Expr) -> Option<i64> {
    match &*e.kind {
        ExprKind::Int(s) => i64::try_from(parse_int_str(s)?).ok(),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => i64::try_from(fold_int_literal(expr)?).ok().map(|v| -v),
        ExprKind::Paren(inner) => fold_signed_int_literal(inner),
        _ => None,
    }
}

fn parse_int_str(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    if let Some(idx) = cleaned.find('\'') {
        let (_, tail) = cleaned.split_at(idx + 1);
        let kind = tail.chars().next()?;
        let digits: &str = &tail[1..];
        match kind {
            'h' | 'H' => u64::from_str_radix(digits, 16).ok(),
            'b' | 'B' => u64::from_str_radix(digits, 2).ok(),
            'o' | 'O' => u64::from_str_radix(digits, 8).ok(),
            _ => digits.parse::<u64>().ok(),
        }
    } else if let Some(hex) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        u64::from_str_radix(bin, 2).ok()
    } else {
        cleaned.parse::<u64>().ok()
    }
}

/// Validates `alias of` targets: target instance must exist in the
/// same addrmap, must not itself be aliased (chained aliases are
/// out of scope for Phase 1g), and must have the same regblock
/// type as the alias (otherwise the shared mirror would be a
/// type error).
fn check_addrmap_aliases(a: &AddrmapDecl) -> Option<String> {
    for inst in &a.instances {
        let Some(target_name) = &inst.alias_of else {
            continue;
        };
        let target = a.instances.iter().find(|i| i.name.name == target_name.name);
        let Some(target) = target else {
            return Some(format!(
                "addrmap `{}`: instance `{}` aliases `{}`, but no such instance exists in this addrmap",
                a.name.name, inst.name.name, target_name.name,
            ));
        };
        if target.alias_of.is_some() {
            return Some(format!(
                "addrmap `{}`: instance `{}` aliases `{}`, which is itself an alias — chained aliases are not supported",
                a.name.name, inst.name.name, target_name.name,
            ));
        }
        if target.regblock_ty.name != inst.regblock_ty.name {
            return Some(format!(
                "addrmap `{}`: instance `{}` (type `{}`) aliases `{}` (type `{}`) — alias target must share the regblock type",
                a.name.name, inst.name.name, inst.regblock_ty.name,
                target_name.name, target.regblock_ty.name,
            ));
        }
    }
    None
}

/// Walks an addrmap's instance list and returns an error message
/// when any two sized instances have overlapping windows. Windows
/// are half-open `[base, base + size)`. Skips pairs where either
/// instance lacks `size`, or where either instance aliases the
/// other (or both alias the same target — RFC §4 explicitly
/// permits this).
fn pair_is_aliased(a: &InstanceDecl, b: &InstanceDecl) -> bool {
    if let Some(t) = &a.alias_of {
        if t.name == b.name.name {
            return true;
        }
    }
    if let Some(t) = &b.alias_of {
        if t.name == a.name.name {
            return true;
        }
    }
    // Both alias the same third instance.
    match (&a.alias_of, &b.alias_of) {
        (Some(ta), Some(tb)) if ta.name == tb.name => true,
        _ => false,
    }
}

fn check_addrmap_overlap(a: &AddrmapDecl) -> Option<String> {
    let sized: Vec<(usize, &InstanceDecl, u64, u64)> = a
        .instances
        .iter()
        .enumerate()
        .filter_map(|(i, inst)| {
            let base = fold_int_literal(&inst.base_addr)?;
            let size = fold_int_literal(inst.size.as_ref()?)?;
            Some((i, inst, base, base.saturating_add(size)))
        })
        .collect();
    for w in 0..sized.len() {
        for x in (w + 1)..sized.len() {
            let (_i_a, a_inst, a_lo, a_hi) = &sized[w];
            let (_i_b, b_inst, b_lo, b_hi) = &sized[x];
            if pair_is_aliased(a_inst, b_inst) {
                continue;
            }
            // Half-open overlap test.
            if *a_lo < *b_hi && *b_lo < *a_hi {
                return Some(format!(
                    "addrmap `{}`: instance `{}` [0x{:x}, 0x{:x}) overlaps instance `{}` [0x{:x}, 0x{:x})",
                    a.name.name,
                    a_inst.name.name, a_lo, a_hi,
                    b_inst.name.name, b_lo, b_hi,
                ));
            }
        }
    }
    None
}

/// Desugar `impl <name> for <TbType>` tests into the classic
/// `test <name>` form (docs/test-ergonomics.md §3.3). For each test
/// with `for_testbench: Some(tb)`:
///
/// 1. Find the bound testbench's declaration (in `file.items`).
/// 2. Classify its fields:
///    - First named-type field whose type isn't a known HARC
///      construct → the DUT (e.g. `dut : Top`). Synthesize a test-
///      scope `let dut : <SVType>` so the existing Verilator-init
///      path picks up the allocation. (Other DUT-typed fields are
///      currently out of scope — multi-DUT testbenches will need a
///      separate plumbing pass.)
///    - All fields participate in the bare-name-rewrite set.
/// 3. Synthesize a test-scope `let _tb : <TbType>` so the existing
///    component-instantiation path constructs the testbench struct.
/// 4. Synthesize `_tb.dut = dut` at the start of the run block so
///    the testbench's DUT pointer is wired to the test-scope let.
/// 5. Walk every Stmt / Expr in the test items and rewrite bare
///    `Ident(name)` references where `name` matches a testbench
///    field (other than `dut`) or a testbench helper method to
///    `_tb.<name>` field-access or `_tb.<name>(...)` call,
///    respectively. `dut` keeps its bare form — it refers to the
///    synthesized test-scope let, which is the same underlying
///    VTop instance as `_tb.dut` (one allocation, two pointers).
///
/// Tests with `for_testbench: None` (classic form) pass through
/// unchanged.
pub(crate) fn desugar_impl_for_test_in_file(file: &SourceFile) -> SourceFile {
    // Index components by name so the desugarer can resolve the
    // bound testbench's field list without re-walking the file.
    let mut components: std::collections::HashMap<String, ComponentDecl> =
        std::collections::HashMap::new();
    for it in &file.items {
        match it {
            Item::Env(c) | Item::Agent(c) | Item::Sequencer(c) | Item::Scoreboard(c) => {
                components.insert(c.name.name.clone(), c.clone());
            }
            _ => {}
        }
    }
    let transactors: std::collections::HashMap<String, TransactorDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) => Some((t.name.name.clone(), t.clone())),
            _ => None,
        })
        .collect();
    let scoreboards: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Scoreboard(c) => Some(c.name.name.clone()),
            _ => None,
        })
        .collect();
    let buses: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Bus(b) => Some(b.name.name.clone()),
            _ => None,
        })
        .collect();
    let covergroups: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Covergroup(g) => Some(g.name.name.clone()),
            _ => None,
        })
        .collect();
    let regblocks: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Regblock(r) => Some(r.name.name.clone()),
            _ => None,
        })
        .collect();
    let addrmaps: std::collections::HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Addrmap(a) => Some(a.name.name.clone()),
            _ => None,
        })
        .collect();

    let mut out = file.clone();
    for it in out.items.iter_mut() {
        let Item::Test(t) = it else { continue };
        let Some(tb_ident) = t.for_testbench.clone() else {
            continue;
        };
        let Some(tb) = components.get(&tb_ident.name) else {
            // Bound testbench not found — leave as is; the main
            // pipeline will surface a sensible error when nothing
            // resolves `_tb`'s type.
            continue;
        };

        // Classify testbench fields.
        let mut dut_field: Option<ComponentField> = None;
        let mut field_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut field_types: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut field_is_pointer: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for ci in &tb.items {
            if let ComponentItem::Field(f) = ci {
                field_names.insert(f.name.name.clone());
                if let TypeExpr::Named { name, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    field_types.insert(f.name.name.clone(), simple.to_string());
                    let is_harc = components.contains_key(simple)
                        || transactors.contains_key(simple)
                        || scoreboards.contains(simple)
                        || buses.contains(simple)
                        || covergroups.contains(simple)
                        || regblocks.contains(simple)
                        || addrmaps.contains(simple);
                    if !is_harc {
                        // Treat as a DUT pointer (Verilator-named SV
                        // module type). First wins for the synthesized
                        // `let dut : ...`.
                        if dut_field.is_none() {
                            dut_field = Some(f.clone());
                        }
                        field_is_pointer.insert(f.name.name.clone());
                    }
                }
            }
        }

        // Method names — both `hookable` and `function` items.
        let mut method_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for ci in &tb.items {
            if let ComponentItem::Hookable(h) = ci {
                method_names.insert(h.name.name.clone());
            }
        }

        let mut tb_lifecycle = ScopeDecl {
            name: Ident {
                name: "sim".into(),
                span: tb_ident.span,
            },
            setup: None,
            run: None,
            check: None,
            teardown: None,
            span: tb_ident.span,
        };
        // Aggregate phase bodies into the synthetic tb_lifecycle
        // ScopeDecl the rest of cpp_tb's lifecycle machinery
        // consumes. With the §7 cleanup each `Lifecycle(phase, body)`
        // node carries exactly one phase, so this loop just routes by
        // phase tag — no field-of-ScopeDecl inspection needed.
        for ci in &tb.items {
            if let ComponentItem::Lifecycle(phase, body) = ci {
                match phase {
                    LifecyclePhase::Setup => tb_lifecycle.setup = Some(body.clone()),
                    LifecyclePhase::Check => tb_lifecycle.check = Some(body.clone()),
                    LifecyclePhase::Teardown => tb_lifecycle.teardown = Some(body.clone()),
                }
                tb_lifecycle.span = body.span;
            }
        }

        // Build the rewriter's "skip set" — names that must NOT be
        // rewritten because they shadow testbench fields at test
        // scope. `dut` always shadows (we synthesize `let dut : ...`
        // for it). Any user-declared `let X` at test scope also
        // shadows — capture them up-front.
        let mut shadow: std::collections::HashSet<String> = std::collections::HashSet::new();
        shadow.insert("dut".into());
        shadow.insert("_tb".into());
        let mut regblock_bindings: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for ti in &t.items {
            if let TestItem::Let(l) = ti {
                shadow.insert(l.name.name.clone());
                if l.bind {
                    if let Some(TypeExpr::Named { name, .. }) = l.ty.as_ref() {
                        if let Some(simple) = name.segments.last().map(|s| s.name.as_str()) {
                            if regblocks.contains(simple) {
                                regblock_bindings.insert(l.name.name.clone());
                            }
                        }
                    }
                }
            }
        }

        // Rewrite every Stmt / Expr in the test body. Bare-statement
        // items (the `impl ... for Tb` form with no `setup`/`run`/
        // `check`/`teardown` scopes) are routed through the same
        // `rewrite_stmts_for_impl` helper as scoped blocks below, so
        // any future rewrite behavior applies to both paths by
        // construction (PR address: code-review finding B on PR
        // arch-hdl-lang/harc-com#306).
        for ti in t.items.iter_mut() {
            match ti {
                TestItem::Stmt(s) => rewrite_stmts_for_impl(
                    std::slice::from_mut(s),
                    &field_names,
                    &method_names,
                    &field_is_pointer,
                    &shadow,
                    &field_types,
                    &transactors,
                    &regblock_bindings,
                ),
                TestItem::Scope(sc) => {
                    if let Some(b) = sc.run.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                    if let Some(b) = sc.setup.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                    if let Some(b) = sc.check.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                    if let Some(b) = sc.teardown.as_mut() {
                        rewrite_block_for_impl(
                            b,
                            &field_names,
                            &method_names,
                            &field_is_pointer,
                            &shadow,
                            &field_types,
                            &transactors,
                            &regblock_bindings,
                        );
                    }
                }
                TestItem::Phase(_, b) => {
                    rewrite_block_for_impl(
                        b,
                        &field_names,
                        &method_names,
                        &field_is_pointer,
                        &shadow,
                        &field_types,
                        &transactors,
                        &regblock_bindings,
                    );
                }
                _ => {}
            }
        }
        if let Some(b) = tb_lifecycle.setup.as_mut() {
            rewrite_block_for_impl(
                b,
                &field_names,
                &method_names,
                &field_is_pointer,
                &shadow,
                &field_types,
                &transactors,
                &regblock_bindings,
            );
        }
        if let Some(b) = tb_lifecycle.check.as_mut() {
            rewrite_block_for_impl(
                b,
                &field_names,
                &method_names,
                &field_is_pointer,
                &shadow,
                &field_types,
                &transactors,
                &regblock_bindings,
            );
        }
        if let Some(b) = tb_lifecycle.teardown.as_mut() {
            rewrite_block_for_impl(
                b,
                &field_names,
                &method_names,
                &field_is_pointer,
                &shadow,
                &field_types,
                &transactors,
                &regblock_bindings,
            );
        }

        // Prepend synthesized lets. Order: `let dut : Top` (so the
        // Verilator-init path sees it as a top-level DUT pointer),
        // then `let _tb : TopTb`. Inserted at the head of items so
        // they win over any user-declared lets that happen to shadow
        // (a defensive choice; today shadowing is also a HARC error
        // via duplicate-let detection).
        let mut prefix: Vec<TestItem> = Vec::new();
        if let Some(dut) = &dut_field {
            // Synthesize: `let dut : <SVType>`
            prefix.push(TestItem::Let(LetStmt {
                name: Ident {
                    name: "dut".into(),
                    span: tb_ident.span,
                },
                ty: Some(dut.ty.clone()),
                value: None,
                bind: false,
                probes: dut.probes.clone(),
                bind_remap: dut.bind_remap.clone(),
                span: tb_ident.span,
            }));
        }
        // Synthesize: `let _tb : <TbType>` — default-constructed
        // through the existing component-let path.
        prefix.push(TestItem::Let(LetStmt {
            name: Ident {
                name: "_tb".into(),
                span: tb_ident.span,
            },
            ty: Some(TypeExpr::Named {
                name: Path {
                    segments: vec![tb_ident.clone()],
                    span: tb_ident.span,
                },
                generics: Vec::new(),
                mode: None,
                span: tb_ident.span,
            }),
            value: None,
            bind: false,
            probes: Vec::new(),
            bind_remap: Vec::new(),
            span: tb_ident.span,
        }));

        // Splice synthesized lets at the head, preserving the rest
        // of the items in original order.
        let original: Vec<TestItem> = std::mem::take(&mut t.items);
        let has_scope = original.iter().any(|ti| matches!(ti, TestItem::Scope(_)));
        let mut bare_run_stmts: Vec<Stmt> = Vec::new();
        t.items = prefix;
        for ti in original {
            if !has_scope {
                if let TestItem::Stmt(s) = &ti {
                    bare_run_stmts.push(s.clone());
                    continue;
                }
            }
            if let TestItem::Scope(sc) = &ti {
                let mut sc = sc.clone();
                // INVARIANT (load-bearing): the synthesized
                // `_tb.dut = dut` wire MUST precede the first read of
                // `_tb.dut.*` anywhere in the lowered test (any
                // lifecycle block or `run`). We achieve this by
                // always materializing a `setup` block to host the
                // wire as its very first statement — even if neither
                // the testbench nor the impl declared a setup. Setup
                // is by spec the earliest user-visible phase, so the
                // wire is guaranteed to run before any other phase
                // body that could dereference `_tb.dut.*`.
                //
                // If a new phase is later added that runs BEFORE
                // setup, the wire site MUST move with it (or the
                // new phase must be threaded through this same
                // merge). Conditional emission ("only inject if
                // setup already exists") was the previous shape;
                // that made the invariant implicit and order-
                // sensitive — silent zero-reads on regression.
                let wire_stmt = dut_field
                    .as_ref()
                    .map(|_| make_wire_dut_stmt(tb_ident.span));

                sc.setup = merge_lifecycle_blocks(
                    tb_lifecycle.setup.clone(),
                    sc.setup.clone(),
                    wire_stmt,
                    tb_ident.span,
                );
                sc.check = merge_lifecycle_blocks(
                    tb_lifecycle.check.clone(),
                    sc.check.clone(),
                    None,
                    tb_ident.span,
                );
                sc.teardown = merge_lifecycle_blocks(
                    sc.teardown.clone(),
                    tb_lifecycle.teardown.clone(),
                    None,
                    tb_ident.span,
                );
                t.items.push(TestItem::Scope(sc));
                continue;
            }
            t.items.push(ti);
        }
        if !bare_run_stmts.is_empty() {
            // Same invariant as the scoped branch: always materialize
            // a setup block to host the `_tb.dut = dut` wire as its
            // first statement. See the scoped-branch comment above
            // for the load-bearing rationale.
            let wire_stmt = dut_field
                .as_ref()
                .map(|_| make_wire_dut_stmt(tb_ident.span));
            let setup =
                merge_lifecycle_blocks(tb_lifecycle.setup.clone(), None, wire_stmt, tb_ident.span);
            let run_stmts = bare_run_stmts;
            t.items.push(TestItem::Scope(ScopeDecl {
                name: Ident {
                    name: "sim".into(),
                    span: tb_ident.span,
                },
                setup,
                run: Some(Block {
                    stmts: run_stmts,
                    span: tb_ident.span,
                }),
                check: tb_lifecycle.check.clone(),
                teardown: tb_lifecycle.teardown.clone(),
                span: tb_ident.span,
            }));
        }

        // Mark as desugared so any downstream consumer (pretty-
        // printer for diagnostics, etc.) sees the classic shape.
        t.for_testbench = None;
    }
    out
}

fn merge_lifecycle_blocks(
    first: Option<Block>,
    second: Option<Block>,
    prefix_stmt: Option<Stmt>,
    fallback_span: Span,
) -> Option<Block> {
    if first.is_none() && second.is_none() && prefix_stmt.is_none() {
        return None;
    }
    let mut stmts = Vec::new();
    if let Some(s) = prefix_stmt {
        stmts.push(s);
    }
    let mut span = fallback_span;
    if let Some(b) = first {
        span = b.span;
        stmts.extend(b.stmts);
    }
    if let Some(b) = second {
        span = b.span;
        stmts.extend(b.stmts);
    }
    Some(Block { stmts, span })
}

/// Build the synthetic `_tb.dut = dut` statement that wires the
/// testbench's DUT pointer to the test-scope `let dut : ...`. Same
/// shape as a user-written assignment, so it threads through
/// `emit_stmt` unchanged.
fn make_wire_dut_stmt(span: Span) -> Stmt {
    let _tb = Expr {
        kind: Box::new(ExprKind::Ident(Ident {
            name: "_tb".into(),
            span,
        })),
        span,
    };
    let _tb_dut = Expr {
        kind: Box::new(ExprKind::Field {
            target: _tb,
            name: Ident {
                name: "dut".into(),
                span,
            },
        }),
        span,
    };
    let dut = Expr {
        kind: Box::new(ExprKind::Ident(Ident {
            name: "dut".into(),
            span,
        })),
        span,
    };
    Stmt {
        kind: StmtKind::Assign {
            target: _tb_dut,
            value: dut,
        },
        span,
    }
}

/// Walk a block and rewrite bare-ident references that match a
/// testbench field or method to `_tb.<name>`. See
/// `desugar_impl_for_test_in_file` for the rewrite rules.
fn rewrite_block_for_impl(
    b: &mut Block,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) {
    rewrite_stmts_for_impl(
        &mut b.stmts,
        fields,
        methods,
        pointers,
        shadow,
        field_types,
        transactors,
        regblock_bindings,
    );
}

/// Rewrite a testbench-scoped body (an `on ... end on` handler body or a
/// testbench helper-method body) so bare references to the testbench's
/// own fields/methods become `_tb.<name>`, matching the impl-for
/// desugaring already applied to the bound test body. Used by the TB-IR
/// lowering (issue #485): unlike v1 it has no testbench component to
/// resolve bare names against, so it reuses this AST rewrite before
/// lowering the body with the ordinary test-scope context. `extra_shadow`
/// names (e.g. a helper method's own parameters, or the handler's own
/// locals) are never rewritten; `dut` and `_tb` are always shadowed.
pub(crate) fn rewrite_testbench_scope_body(
    body: &mut Block,
    tb: &ComponentDecl,
    extra_shadow: &std::collections::HashSet<String>,
) {
    let mut fields: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut methods: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ci in &tb.items {
        match ci {
            ComponentItem::Field(f) => {
                fields.insert(f.name.name.clone());
            }
            ComponentItem::Hookable(h) => {
                methods.insert(h.name.name.clone());
            }
            _ => {}
        }
    }
    let mut shadow: std::collections::HashSet<String> =
        ["dut".to_string(), "_tb".to_string()].into_iter().collect();
    shadow.extend(extra_shadow.iter().cloned());
    // The `pointers` set only affected a legacy DUT-deref path that the
    // expr rewriter no longer consults; pass an empty set.
    let pointers = std::collections::HashSet::new();
    let field_types = std::collections::HashMap::new();
    let transactors = std::collections::HashMap::new();
    let regblock_bindings = std::collections::HashSet::new();
    rewrite_block_for_impl(
        body,
        &fields,
        &methods,
        &pointers,
        &shadow,
        &field_types,
        &transactors,
        &regblock_bindings,
    );
}

/// Statement-list variant of `rewrite_block_for_impl`. Both the
/// scoped form (per-`Block` walk) and the bare-statement form
/// (`impl ... for Tb` with a flat `Stmt` list, no `setup`/`run`/
/// `check`/`teardown` scopes) share this helper so that bare bodies
/// can never structurally diverge from scoped bodies. New rewrite
/// behavior added here applies to both paths by construction.
fn rewrite_stmts_for_impl(
    stmts: &mut [Stmt],
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) {
    for s in stmts.iter_mut() {
        rewrite_stmt_for_impl(
            s,
            fields,
            methods,
            pointers,
            shadow,
            field_types,
            transactors,
            regblock_bindings,
        );
    }
}

fn rewrite_stmt_for_impl(
    s: &mut Stmt,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) {
    match &mut s.kind {
        StmtKind::Let(l) => {
            if let Some(v) = l.value.as_mut() {
                rewrite_expr_for_impl(v, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            rewrite_expr_for_impl(target, fields, methods, pointers, shadow);
            rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
        }
        StmtKind::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
        StmtKind::For(f) => {
            rewrite_expr_for_impl(&mut f.iter, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut f.body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::Repeat(r) => {
            rewrite_expr_for_impl(&mut r.count, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut r.body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::Loop(b) => rewrite_block_for_impl(
            b,
            fields,
            methods,
            pointers,
            shadow,
            field_types,
            transactors,
            regblock_bindings,
        ),
        StmtKind::While { cond, body, .. } => {
            rewrite_expr_for_impl(cond, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::If(ifs) => {
            rewrite_expr_for_impl(&mut ifs.cond, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut ifs.then_block,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
            for (c, b) in ifs.elsifs.iter_mut() {
                rewrite_expr_for_impl(c, fields, methods, pointers, shadow);
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
            if let Some(b) = ifs.else_block.as_mut() {
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::Fork(fk) => {
            for b in fk.branches.iter_mut() {
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
            for b in blocks.iter_mut() {
                rewrite_block_for_impl(
                    b,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::Select(arms) => {
            for a in arms.iter_mut() {
                rewrite_expr_for_impl(&mut a.event, fields, methods, pointers, shadow);
                rewrite_block_for_impl(
                    &mut a.action,
                    fields,
                    methods,
                    pointers,
                    shadow,
                    field_types,
                    transactors,
                    regblock_bindings,
                );
            }
        }
        StmtKind::On(h) => {
            let mut body_shadow = shadow.clone();
            if let Some(params) =
                impl_on_handler_param_names(h, field_types, transactors, regblock_bindings)
            {
                body_shadow.extend(params);
            }
            rewrite_expr_for_impl(&mut h.event, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                &mut h.body,
                fields,
                methods,
                pointers,
                &body_shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::After { duration, body, .. } => {
            rewrite_expr_for_impl(duration, fields, methods, pointers, shadow);
            rewrite_block_for_impl(
                body,
                fields,
                methods,
                pointers,
                shadow,
                field_types,
                transactors,
                regblock_bindings,
            );
        }
        StmtKind::Wait { duration, .. } => {
            rewrite_expr_for_impl(duration, fields, methods, pointers, shadow);
        }
        StmtKind::WaitUntil {
            conditions,
            timeout,
            ..
        } => {
            for c in conditions.iter_mut() {
                rewrite_expr_for_impl(c, fields, methods, pointers, shadow);
            }
            if let Some(t) = timeout.as_mut() {
                rewrite_expr_for_impl(&mut t.cycles, fields, methods, pointers, shadow);
                if let Some(m) = t.message.as_mut() {
                    rewrite_expr_for_impl(m, fields, methods, pointers, shadow);
                }
            }
        }
        StmtKind::Yield(e) | StmtKind::Release(e) => {
            rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
        }
        StmtKind::Return(opt) => {
            if let Some(e) = opt.as_mut() {
                rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
            if let Some(ex) = v.expr.as_mut() {
                rewrite_expr_for_impl(ex, fields, methods, pointers, shadow);
            }
            if let Some(else_fail) = v.else_fail.as_mut() {
                rewrite_expr_for_impl(else_fail, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Randomize {
            target, with_body, ..
        } => {
            rewrite_expr_for_impl(target, fields, methods, pointers, shadow);
            for e in with_body.iter_mut() {
                rewrite_expr_for_impl(e, fields, methods, pointers, shadow);
            }
        }
        StmtKind::Log { args, .. } | StmtKind::LogF { args, .. } => {
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
                    }
                }
            }
        }
        StmtKind::Emit { name, args, .. } => {
            // Multi-segment `emit X.Y(t)` lowers verbatim as
            // `X.Y(...)` in the codegen at StmtKind::Emit handling
            // (see lower_emit). If the head segment is a testbench
            // field, prepend `_tb` so the lowered C++ resolves
            // through the testbench struct.
            if let Some(head) = name.segments.first() {
                if fields.contains(&head.name) && !shadow.contains(&head.name) {
                    name.segments.insert(
                        0,
                        Ident {
                            name: "_tb".into(),
                            span: head.span,
                        },
                    );
                }
            }
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(e) => rewrite_expr_for_impl(e, fields, methods, pointers, shadow),
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, pointers, shadow);
                    }
                }
            }
        }
        StmtKind::Fail { msg, .. } => {
            rewrite_expr_for_impl(msg, fields, methods, pointers, shadow);
        }
        StmtKind::JoinAll { .. }
        | StmtKind::Apply(_)
        | StmtKind::Break { .. }
        | StmtKind::Continue { .. } => {}
    }
}

fn impl_on_handler_param_names(
    h: &OnHandler,
    field_types: &std::collections::HashMap<String, String>,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    regblock_bindings: &std::collections::HashSet<String>,
) -> Option<Vec<String>> {
    if h.hook.is_some() {
        let ExprKind::Field {
            target,
            name: method,
        } = &*h.event.kind
        else {
            return None;
        };
        let field = match &*target.kind {
            ExprKind::Ident(id) => id.name.as_str(),
            ExprKind::Field { target, name } => {
                let ExprKind::Ident(root) = &*target.kind else {
                    return None;
                };
                if root.name != "_tb" {
                    return None;
                }
                name.name.as_str()
            }
            _ => return None,
        };
        let transactor_ty = field_types.get(field)?;
        let transactor = transactors.get(transactor_ty)?;
        let method_decl = transactor
            .items
            .iter()
            .chain(transactor.when_active.iter().flatten())
            .find_map(|item| match item {
                ComponentItem::Hookable(m) if m.name.name == method.name => Some(m),
                _ => None,
            })?;
        return Some(
            method_decl
                .params
                .iter()
                .map(|p| p.name.name.clone())
                .collect(),
        );
    }

    // Per-register RAL callbacks (`on regs.REG`) see an implicit scalar
    // parameter named `data`. Match the lowering collector's resolved
    // subset by requiring the root ident to be a known test-scope
    // regblock binding; ordinary cycle triggers like `on dut.ready`
    // must not suppress testbench-field rewriting for a field named
    // `data`.
    if !h.periodic {
        if let ExprKind::Field { target, .. } = &*h.event.kind {
            if let ExprKind::Ident(binding) = &*target.kind {
                if !regblock_bindings.contains(&binding.name) {
                    return None;
                }
                return Some(vec!["data".to_string()]);
            }
        }
    }
    None
}

fn rewrite_expr_for_impl(
    e: &mut Expr,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    _pointers: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) {
    match e.kind.as_mut() {
        ExprKind::Ident(id) => {
            // Bare ident matching a testbench field (non-shadowed)
            // → `_tb.<id>`.
            if fields.contains(&id.name) && !shadow.contains(&id.name) {
                let new_id = id.clone();
                let span = e.span;
                let inner = Expr {
                    kind: Box::new(ExprKind::Ident(Ident {
                        name: "_tb".into(),
                        span,
                    })),
                    span,
                };
                *e.kind = ExprKind::Field {
                    target: inner,
                    name: new_id,
                };
            }
        }
        ExprKind::Call { callee, args } => {
            // Bare-name method call `<m>(args)` where `<m>` is a
            // testbench helper (function/hookable) and not shadowed
            // → rewrite callee to `_tb.<m>` so the existing
            // `resolve_component_method_call` dispatcher picks it up.
            if let ExprKind::Ident(id) = callee.kind.as_ref() {
                if methods.contains(&id.name) && !shadow.contains(&id.name) {
                    let new_id = id.clone();
                    let span = callee.span;
                    let inner = Expr {
                        kind: Box::new(ExprKind::Ident(Ident {
                            name: "_tb".into(),
                            span,
                        })),
                        span,
                    };
                    *callee.kind = ExprKind::Field {
                        target: inner,
                        name: new_id,
                    };
                }
            } else {
                rewrite_expr_for_impl(callee, fields, methods, _pointers, shadow);
            }
            for a in args.iter_mut() {
                match a {
                    CallArg::Expr(x) => {
                        rewrite_expr_for_impl(x, fields, methods, _pointers, shadow)
                    }
                    CallArg::Named { value, .. } => {
                        rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
                    }
                }
            }
        }
        ExprKind::Field { target, .. } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
        }
        ExprKind::Index { target, index } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(index, fields, methods, _pointers, shadow);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(hi, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(lo, fields, methods, _pointers, shadow);
        }
        ExprKind::Cast { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::Send { target, value } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
        }
        ExprKind::Unary { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::ForkCall { call } => {
            rewrite_expr_for_impl(call, fields, methods, _pointers, shadow)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            rewrite_expr_for_impl(lhs, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(rhs, fields, methods, _pointers, shadow);
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            rewrite_expr_for_impl(cond, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(then_branch, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(else_branch, fields, methods, _pointers, shadow);
        }
        ExprKind::HashHash { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::SeqRepeat { expr, .. } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow)
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(lo) = lo.as_mut() {
                rewrite_expr_for_impl(lo, fields, methods, _pointers, shadow);
            }
            if let Some(hi) = hi.as_mut() {
                rewrite_expr_for_impl(hi, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::SetLit(es) => {
            for x in es.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::SystemCall { args, .. } => {
            for x in args.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::Randomize {
            target, with_body, ..
        } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
            for x in with_body.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::ForEachConstraint { var, iter, body } => {
            rewrite_expr_for_impl(iter, fields, methods, _pointers, shadow);
            let mut inner_shadow = shadow.clone();
            inner_shadow.insert(var.name.clone());
            for x in body.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, &inner_shadow);
            }
        }
        ExprKind::SoftConstraint(sc) => {
            rewrite_expr_for_impl(&mut sc.expr, fields, methods, _pointers, shadow);
            if let Some(weight) = sc.weight.as_mut() {
                rewrite_expr_for_impl(weight, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::DistDirective { target, .. } => {
            rewrite_expr_for_impl(target, fields, methods, _pointers, shadow);
        }
        ExprKind::Paren(x) => rewrite_expr_for_impl(x, fields, methods, _pointers, shadow),
        ExprKind::NamedArg { value, .. } => {
            rewrite_expr_for_impl(value, fields, methods, _pointers, shadow);
        }
        ExprKind::StructLit { fields: nfs, .. } => {
            for nf in nfs.iter_mut() {
                rewrite_expr_for_impl(&mut nf.value, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::CoverArrow { lhs, rhs, .. } => {
            rewrite_expr_for_impl(lhs, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(rhs, fields, methods, _pointers, shadow);
        }
        ExprKind::SolveOrder { args } => {
            for x in args.iter_mut() {
                rewrite_expr_for_impl(x, fields, methods, _pointers, shadow);
            }
        }
        ExprKind::Membership { expr, set } => {
            rewrite_expr_for_impl(expr, fields, methods, _pointers, shadow);
            rewrite_expr_for_impl(set, fields, methods, _pointers, shadow);
        }
        ExprKind::String(s) => {
            // Rewrite testbench-field references inside `${...}`
            // interpolations. Without this, `fail("count = ${env.sb}")`
            // emits raw `env.sb` after the desugarer has already
            // rewritten the rest of the AST, producing a compile-time
            // "undeclared identifier" in C++. Textual rewrite is
            // safe — `${...}` spans don't nest in v0.
            *s = rewrite_interp_in_string(s, fields, methods, shadow);
        }
        ExprKind::DistLit(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::Bool(_)
        | ExprKind::ImplicitSelf => {}
    }
}

/// Scan a string literal for `${...}` interpolation segments and
/// prepend `_tb.` to any segment whose leading identifier matches
/// a testbench field (or method that's been folded into scope) and
/// isn't shadowed. Operates textually so it can't get confused by
/// quote-escaping nuances inside the expression — `${...}` doesn't
/// nest in v0.
fn rewrite_interp_in_string(
    s: &str,
    fields: &std::collections::HashSet<String>,
    methods: &std::collections::HashSet<String>,
    shadow: &std::collections::HashSet<String>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for the start of a `${` interpolation (not preceded by
        // a backslash). v0 doesn't ship a fancier escape grammar, so
        // a bare `${` is always an interpolation.
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            out.push_str("${");
            i += 2;
            // Capture up to the matching `}`.
            let start = i;
            while i < bytes.len() && bytes[i] != b'}' {
                i += 1;
            }
            let segment = &s[start..i];
            // Extract the leading identifier (before any `.`, `:`,
            // `[`, or whitespace). The format-spec suffix `:<fmt>` is
            // preserved unchanged so widthhex / decimal hints
            // continue to work.
            let head_end = segment
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(segment.len());
            let head = &segment[..head_end];
            if !head.is_empty()
                && (fields.contains(head) || methods.contains(head))
                && !shadow.contains(head)
            {
                out.push_str("_tb.");
            }
            out.push_str(segment);
            // Consume the closing `}` (if present — malformed
            // interpolations pass through untouched).
            if i < bytes.len() {
                out.push('}');
                i += 1;
            }
        } else {
            out.push(s.as_bytes()[i] as char);
            i += 1;
        }
    }
    out
}

/// Enforce: a transactor's always-on body (anything not under
/// `when active { ... }`) must not drive DUT signals. Drive-capable
/// hookables / on-handlers must live inside `when active`.
///
/// Rationale (spec §8.1): a `passive` instance literally has its
/// `when active` body elided at codegen (see
/// `synth_component_from_transactor`). If drive code lived in the
/// always-on portion, a passive instance would still emit that code
/// and could end up wiring an observer to the bus — exactly the
/// footgun this check prevents. The common block-level→chip-level
/// TB reuse pattern (passive instance at chip level monitoring a bus
/// the chip already drives) silently miscompiles otherwise.
///
/// "Drive" is:
/// - assignment whose LHS is a field-access chain rooted at a
///   non-HARC-inner-typed field (treated as a DUT pointer), or
///   rooted at the implicit `bus` alias of a `bound to BusType`
///   transactor.
/// - call to `<bus>.<ch>.send(...)` or `<bus>.<ch>.recv()` — both
///   drive the channel's valid (send) or ready (recv) line.
/// - `release <expr>` — pairs with `probe force` writes.
/// - `<lhs> <- <rhs>` channel-send statement on a DUT-rooted target.
///
/// Returns an error message string on the first violation (caller
/// converts to `EmitError`).
fn check_transactor_no_drive_in_always_on_body(
    t: &TransactorDecl,
    transactors: &std::collections::HashMap<String, TransactorDecl>,
    components: &std::collections::HashMap<String, ComponentDecl>,
    scoreboards: &std::collections::HashSet<String>,
    covergroups: &std::collections::HashMap<String, CovergroupDecl>,
    buses: &std::collections::HashMap<String, BusDecl>,
    regblocks: &std::collections::HashMap<String, RegblockDecl>,
    addrmaps: &std::collections::HashMap<String, AddrmapDecl>,
) -> Option<String> {
    // 1) Decide which of T's fields are HARC inner constructs (and so
    //    safe to read/write at the field level) versus DUT pointers
    //    (member-write counts as a drive). Builtins (uint/sint/bool/
    //    queue/event/...) are excluded — they're plain inner state.
    let mut dut_field_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for it in &t.items {
        if let ComponentItem::Field(f) = it {
            let TypeExpr::Named { name, .. } = &f.ty else {
                continue; // builtin / event / queue → not a DUT pointer
            };
            let Some(simple) = name.segments.last().map(|s| s.name.as_str()) else {
                continue;
            };
            let is_harc_inner = transactors.contains_key(simple)
                || components.contains_key(simple)
                || scoreboards.contains(simple)
                || covergroups.contains_key(simple)
                || regblocks.contains_key(simple)
                || addrmaps.contains_key(simple)
                || buses.contains_key(simple);
            if !is_harc_inner {
                dut_field_names.insert(f.name.name.clone());
            }
        }
    }
    let is_bound_to_bus = t.bound_to.is_some();

    // 2) Walk every hookable + on-handler in the always-on body
    //    (skipping `t.when_active`, which is the legitimate home for
    //    drive code).
    for it in &t.items {
        match it {
            ComponentItem::Hookable(h) => {
                if let Some(violation) =
                    find_drive_in_block(&h.body, &dut_field_names, is_bound_to_bus)
                {
                    return Some(format!(
                        "transactor `{}` hookable `{}` drives a DUT signal ({}) from the always-on body. \
                         Passive instances would still execute this code. Move the hookable into a \
                         `when active ... end when` block, or remove the drive. See spec §8.1.",
                        t.name.name, h.name.name, violation,
                    ));
                }
            }
            ComponentItem::OnHandler(h) => {
                if let Some(violation) =
                    find_drive_in_block(&h.body, &dut_field_names, is_bound_to_bus)
                {
                    return Some(format!(
                        "transactor `{}` `on`-handler drives a DUT signal ({}) from the always-on body. \
                         Passive instances would still execute this code. Move the handler into a \
                         `when active ... end when` block, or remove the drive. See spec §8.1.",
                        t.name.name, violation,
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// Returns the offending source snippet describing the first drive
/// statement found in `block`, or `None` if the block is drive-free.
fn find_drive_in_block(
    block: &Block,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    for s in &block.stmts {
        if let Some(v) = find_drive_in_stmt(s, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    None
}

fn find_drive_in_stmt(
    s: &Stmt,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    match &s.kind {
        StmtKind::Assign { target, value: _ } | StmtKind::Send { target, value: _ } => {
            if let Some(root) = expr_root_ident(target) {
                if has_field_access(target) {
                    if dut_fields.contains(&root) {
                        return Some(format!("`{}` = ...", expr_source_str(target)));
                    }
                    if bound_to_bus && root == "bus" {
                        return Some(format!("`{}` = ...", expr_source_str(target)));
                    }
                }
            }
            None
        }
        StmtKind::Release(e) => Some(format!("release {}", expr_source_str(e))),
        StmtKind::Expr(e) => find_drive_in_expr(e, dut_fields, bound_to_bus),
        StmtKind::Let(l) => l
            .value
            .as_ref()
            .and_then(|v| find_drive_in_expr(v, dut_fields, bound_to_bus)),
        StmtKind::For(f) => find_drive_in_block(&f.body, dut_fields, bound_to_bus),
        StmtKind::Repeat(r) => find_drive_in_block(&r.body, dut_fields, bound_to_bus),
        StmtKind::Loop(b) => find_drive_in_block(b, dut_fields, bound_to_bus),
        StmtKind::While { body, .. } => find_drive_in_block(body, dut_fields, bound_to_bus),
        StmtKind::If(ifs) => find_drive_in_if(ifs, dut_fields, bound_to_bus),
        StmtKind::Fork(fk) => fk
            .branches
            .iter()
            .find_map(|b| find_drive_in_block(b, dut_fields, bound_to_bus)),
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => blocks
            .iter()
            .find_map(|b| find_drive_in_block(b, dut_fields, bound_to_bus)),
        StmtKind::Select(arms) => arms
            .iter()
            .find_map(|a| find_drive_in_block(&a.action, dut_fields, bound_to_bus)),
        StmtKind::On(h) => find_drive_in_block(&h.body, dut_fields, bound_to_bus),
        StmtKind::After { body, .. } => find_drive_in_block(body, dut_fields, bound_to_bus),
        _ => None,
    }
}

fn find_drive_in_if(
    ifs: &IfStmt,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    if let Some(v) = find_drive_in_block(&ifs.then_block, dut_fields, bound_to_bus) {
        return Some(v);
    }
    for (_cond, body) in &ifs.elsifs {
        if let Some(v) = find_drive_in_block(body, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    if let Some(else_b) = &ifs.else_block {
        if let Some(v) = find_drive_in_block(else_b, dut_fields, bound_to_bus) {
            return Some(v);
        }
    }
    None
}

fn find_drive_in_expr(
    e: &Expr,
    dut_fields: &std::collections::HashSet<String>,
    bound_to_bus: bool,
) -> Option<String> {
    // The only expression-position drive we flag is a method call
    // `<bus>.<ch>.send(...)` or `<bus>.<ch>.recv()` — both drive a
    // handshake line.
    if let ExprKind::Call { callee, args } = &*e.kind {
        if let ExprKind::Field { target, name } = &*callee.kind {
            if (name.name == "send" || name.name == "recv") && bound_to_bus {
                if let ExprKind::Field {
                    target: inner,
                    name: _,
                } = &*target.kind
                {
                    if let ExprKind::Ident(id) = &*inner.kind {
                        if id.name == "bus" {
                            return Some(format!("{}(...)", expr_source_str(callee)));
                        }
                    }
                }
            }
        }
        // Recurse into args in case a deeply-nested call drives.
        for a in args {
            let arg_expr = match a {
                CallArg::Expr(x) => x,
                CallArg::Named { value, .. } => value,
            };
            if let Some(v) = find_drive_in_expr(arg_expr, dut_fields, bound_to_bus) {
                return Some(v);
            }
        }
    }
    None
}

fn expr_root_ident(e: &Expr) -> Option<String> {
    let mut cur = e;
    loop {
        match &*cur.kind {
            ExprKind::Ident(id) => return Some(id.name.clone()),
            ExprKind::Field { target, .. } => cur = target,
            ExprKind::Index { target, .. } => cur = target,
            ExprKind::Paren(inner) => cur = inner,
            _ => return None,
        }
    }
}

fn has_field_access(e: &Expr) -> bool {
    matches!(&*e.kind, ExprKind::Field { .. })
        || matches!(&*e.kind, ExprKind::Index { target, .. } if has_field_access(target))
        || matches!(&*e.kind, ExprKind::Paren(inner) if has_field_access(inner))
}

/// Width in bits of a RAL field declared as `field name : <ty>`.
/// `bit` / `bool` collapse to 1; `uint<N>` / `sint<N>` / `bits<N>` /
/// `UInt<N>` / `SInt<N>` use the type argument. Falls back to 1 for
/// shapes the parser shouldn't have accepted (caller reports a clean
/// error path).
fn field_bit_width(t: &TypeExpr) -> u32 {
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => 1,
            BuiltinTy::UInt
            | BuiltinTy::SInt
            | BuiltinTy::Bits
            | BuiltinTy::UIntCap
            | BuiltinTy::SIntCap => type_arg_width(args).unwrap_or(1),
            _ => 1,
        },
        _ => 1,
    }
}

/// Right-aligned bit mask for a `width`-bit field, suitable for emit
/// as `0x{mask:x}u`. Clamped at 32 bits because Phase 1b fields cap at
/// register width 32 (mirror is `uint32_t`); wider fields are a
/// downstream extension along with the wider mirror types.
fn field_mask_literal(width: u32) -> u64 {
    if width >= 32 {
        0xFFFFFFFFu64
    } else {
        (1u64 << width) - 1
    }
}

/// C++ unsigned integer type wide enough to hold a register of `width`
/// bits. Phase 1a caps at 64; wider registers (e.g. AES blocks) would
/// need the `harc_rt::harc_u128` story used elsewhere in this file.
fn mirror_field_c_type(width: u32) -> &'static str {
    match width {
        1..=8 => "uint8_t",
        9..=16 => "uint16_t",
        17..=32 => "uint32_t",
        _ => "uint64_t",
    }
}

/// Render an integer-literal `ExprKind` as a C++ integer literal.
/// Non-integer expressions fall back to `0` so the emitted code still
/// compiles; the parser path that calls this only fires on `Int(...)`
/// today because reset values and addresses are parsed with
/// `parse_expr` and the codegen surfaces dynamic expressions through
/// the main emit path. Phase 1a callers always supply Int kinds.
fn c_int_literal_from(k: &ExprKind) -> String {
    match k {
        ExprKind::Int(s) => c_int_literal(s),
        _ => "0".to_string(),
    }
}

fn c_int_literal(s: &str) -> String {
    // ARCH-style sized literals (`8'hAB`) → C++ `0xAB`. Then the
    // unsized-literal post-pass below catches any >64-bit cases.
    let normalized = if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            'b' | 'B' => format!("0b{digits}"),
            _ => digits,
        }
    } else {
        s.replace('_', "")
    };

    // Hex literals wider than 64 bits (>16 hex digits) overflow C++'s
    // unsigned long long. Lower as a composite `_harc_u128` shifted-OR
    // so the value flows through `harc_assign` / `harc_read` and 128-
    // bit comparisons. Two halves are sufficient for the v0 cap of
    // 128 bits; literals above that are clamped to the low 128 bits.
    let (prefix, digits) = if let Some(d) = normalized.strip_prefix("0x") {
        ("0x", d)
    } else if let Some(d) = normalized.strip_prefix("0X") {
        ("0x", d)
    } else {
        ("", normalized.as_str())
    };
    if !prefix.is_empty() && digits.len() > 16 && digits.len() <= 32 {
        // 65..128 bits — fits in `_harc_u128`. Split into two 64-bit
        // halves and emit the composite shifted-OR.
        let hex_lo_start = digits.len() - 16;
        let lo = &digits[hex_lo_start..];
        let hi = &digits[..hex_lo_start];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    // > 128 bits: handled by `c_wide_lit_words` at the assign / equality
    // call sites — they emit `harc_assign_words` / `harc_eq_words` with
    // an `std::initializer_list<uint32_t>` instead. If we got here with
    // a > 128b literal it means the literal escaped into a context that
    // can't take a word-list (e.g. used as an arithmetic operand);
    // fall back to a clamped composite to keep the output compilable
    // with a clear narrowing warning rather than a crash.
    if !prefix.is_empty() && digits.len() > 32 {
        let lo = &digits[digits.len() - 16..];
        let hi = &digits[digits.len() - 32..digits.len() - 16];
        return format!("(((_harc_u128)0x{hi}ULL << 64) | (_harc_u128)0x{lo}ULL)");
    }
    normalized
}

fn c_value_literal(s: &str) -> String {
    if let Some(words) = c_wide_lit_words(s) {
        let mut out = format!("harc_rt::HarcWide<{}>({{", words.len());
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(w);
        }
        out.push_str("})");
        return out;
    }
    c_int_literal(s)
}

/// If the integer literal `s` is hex with > 32 hex digits (i.e. >
/// 128 bits), return its decomposition into 32-bit words in LSB-
/// first order — `vec!["0x...", "0x...", ...]`. Each string is a
/// `uint32_t` hex literal. Returns `None` for narrower or non-hex
/// literals; callers fall back to `c_int_literal` for those.
///
/// Used by the assign and equality lowering paths: a literal that
/// can't fit in `_harc_u128` flows through `harc_assign_words` /
/// `harc_eq_words` instead, which take `std::initializer_list<uint32_t>`.
fn c_wide_lit_words(s: &str) -> Option<Vec<String>> {
    let normalized = if let Some(idx) = s.find('\'') {
        let (_, tail) = s.split_at(idx + 1);
        let kind = tail.chars().next().unwrap_or('d');
        let digits: String = tail[1..].chars().filter(|c| *c != '_').collect();
        match kind {
            'h' | 'H' => format!("0x{digits}"),
            _ => return None,
        }
    } else {
        s.replace('_', "")
    };
    let hex = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))?;
    if hex.len() <= 32 {
        return None;
    }
    // Split into 8-hex-digit (32-bit) chunks, LSB-first.
    let mut words = Vec::new();
    let mut remaining = hex.len();
    while remaining > 0 {
        let start = remaining.saturating_sub(8);
        let chunk = &hex[start..remaining];
        words.push(format!("0x{chunk}u"));
        remaining = start;
    }
    Some(words)
}

/// Number of 32-bit words the *value* of a `c_wide_lit_words` word list
/// actually needs — i.e. one past the index of the highest non-zero
/// word (minimum 1). This is the value-based "required width" used by
/// the over-width assignment guard: leading-zero high words (a literal
/// written wider than necessary) do NOT inflate the count, so a value
/// that fits the port is never flagged, while a value with set bits
/// above the port width is.
///
/// `words` are LSB-first `uint32_t` hex literals like `"0x1800u"`, as
/// produced by `c_wide_lit_words`.
fn significant_word_count(words: &[String]) -> usize {
    for (i, w) in words.iter().enumerate().rev() {
        let digits = w
            .trim_end_matches(['u', 'U'])
            .trim_start_matches("0x")
            .trim_start_matches("0X");
        if u32::from_str_radix(digits, 16).unwrap_or(0) != 0 {
            return i + 1;
        }
    }
    1
}

/// Returns true if `e` is an `Int` literal whose value is wider than
/// 128 bits — i.e. the assign / equality call site should route
/// through `harc_assign_words` / `harc_eq_words` with the word-list
/// from `c_wide_lit_words` rather than the `_harc_u128` composite
/// from `c_int_literal`.
fn is_wide_int_literal(e: &Expr) -> Option<Vec<String>> {
    if let ExprKind::Int(s) = &*e.kind {
        c_wide_lit_words(s)
    } else {
        None
    }
}

/// Parse a HARC-style interpolated string into a printf format string and
/// the list of capture expressions in source form.
///
/// Syntax:
///   `${expr}`         — default decimal: `%lld`
///   `${expr:d}`       — explicit decimal: `%lld`
///   `${expr:x}`       — lowercase hex: `%llx`
///   `${expr:X}`       — uppercase hex: `%llX`
///   `${expr:o}`       — octal: `%llo`
///   `${expr:08x}`     — width + zero-pad: `%08llx` (Python/Rust f-string)
///   `${expr:8d}`      — width + space-pad
///
/// Plain `%` is escaped to `%%`. Every captured expression is routed through
/// `harc_rt::harc_printf_ll` at the call site for the non-wide-hex path. v0
/// limitations: bit-slice `a[7:0]` cannot appear inside `${...}` (the format
/// separator is `:`); strings and chars are not yet supported as interpolation
/// targets — hoist into a let.
/// One captured `${expr:spec}` from a HARC interpolated string.
/// `wide_hex` is `Some((width_hex_digits, upper_case))` when the spec
/// is `:WWx` or `:WWX` with WW > 16 — those route through the
/// `HarcHexBuf128` runtime helper at codegen time so values up to 128
/// bits print in full instead of being truncated to a `long long`.
/// All other specs use the legacy long-long printf ABI via
/// `harc_rt::harc_printf_ll(...)`.
pub(crate) struct InterpCap {
    pub(crate) expr: String,
    pub(crate) wide_hex: Option<(usize, bool)>,
}

pub(crate) fn process_interp(s: &str) -> (String, Vec<InterpCap>) {
    let mut fmt = String::with_capacity(s.len());
    let mut captures: Vec<InterpCap> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Walk to matching `}` (no nested braces in v0).
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            } // unmatched — bail and keep what we have
            let inner = std::str::from_utf8(&bytes[i + 2..j]).unwrap_or("").trim();
            // Split on the last `:` to extract an optional format spec.
            let (expr_src, spec) = match inner.rfind(':') {
                Some(idx) => (inner[..idx].trim(), inner[idx + 1..].trim()),
                None => (inner, ""),
            };
            let (fmt_token, wide_hex) = translate_fmt_spec(spec);
            captures.push(InterpCap {
                expr: expr_src.to_string(),
                wide_hex,
            });
            fmt.push_str(&fmt_token);
            i = j + 1;
        } else if bytes[i] == b'%' {
            fmt.push_str("%%");
            i += 1;
        } else {
            fmt.push(bytes[i] as char);
            i += 1;
        }
    }
    (fmt, captures)
}

/// Translate a HARC format spec (Python/Rust f-string subset) to a
/// printf conversion specifier. Returns `(token, wide_hex_info)`:
/// - `token` is the printf format substring (e.g. `%08llx` or `%s`).
/// - `wide_hex_info` is `Some((width, upper))` when the spec is hex
///   wider than 16 digits — the caller must emit the value via
///   `HarcHexBuf128` so it prints in full 128-bit precision.
pub(crate) fn translate_fmt_spec(spec: &str) -> (String, Option<(usize, bool)>) {
    if spec.is_empty() {
        return ("%lld".to_string(), None);
    }
    let last = spec.chars().last().unwrap();
    let prefix = &spec[..spec.len() - last.len_utf8()];

    // Pull the leading width digits (after an optional `0` flag) so we
    // can decide whether to route through the wide-hex helper. Examples:
    //   "032x" → width 32, hex. "8d" → width 8, decimal. "x" → width 0.
    let width: usize = {
        let trimmed = prefix.trim_start_matches('0');
        trimmed
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<usize>()
            .unwrap_or(0)
    };

    match last {
        'd' => (format!("%{prefix}lld"), None),
        'o' => (format!("%{prefix}llo"), None),
        'x' | 'X' if width > 16 => {
            // Wide-hex: use `%s` and let the runtime helper format
            // into a stack buffer. Width-pad and zero-pad both come
            // from the helper itself.
            ("%s".to_string(), Some((width, last == 'X')))
        }
        'x' => (format!("%{prefix}llx"), None),
        'X' => (format!("%{prefix}llX"), None),
        _ => ("%lld".to_string(), None),
    }
}

/// Convert a HARC time literal (`5ns`, `20ns`, `100ps`, `2us`, `1ms`, `1s`)
/// to picoseconds. Returns `Err(msg)` on unrecognised units. The `cycles`
/// suffix is rejected here — clock periods must be wall-clock time.
fn time_literal_to_ps(s: &str) -> Result<i64, String> {
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .collect();
    let unit: String = s.chars().skip(digits.len()).collect();
    let factor: i64 = match unit.as_str() {
        "ps" => 1,
        "ns" => 1_000,
        "us" => 1_000_000,
        "ms" => 1_000_000_000,
        "s" => 1_000_000_000_000,
        other => {
            return Err(format!(
                "unsupported time unit `{other}` in `{s}` (expected ps/ns/us/ms/s)"
            ));
        }
    };
    // i64 picoseconds caps each unit at i64::MAX / factor (e.g. 9_223_372 s).
    let overflow = || {
        format!(
            "time literal `{s}` overflows the picosecond range (max {}{unit})",
            i64::MAX / factor
        )
    };
    let n: i64 = digits.replace('_', "").parse().map_err(|e| {
        if matches!(
            std::num::ParseIntError::kind(&e),
            std::num::IntErrorKind::PosOverflow
        ) {
            overflow()
        } else {
            format!("bad number in time literal `{s}`")
        }
    })?;
    n.checked_mul(factor).ok_or_else(overflow)
}

/// Recognise a temporal helper call in either form — bare `Call` AST with
/// callee `Ident("past"|"rose"|"fell"|"stable")` (the ARCH-aligned form,
/// what the parser produces today) or the legacy `SystemCall` AST (pre-
/// `$past`-removal). Returns `(kind, inner_arg)` on match.
fn match_temporal_call(e: &Expr) -> Option<(SystemFn, &Expr)> {
    match &*e.kind {
        ExprKind::SystemCall { name, args } => {
            let kind = match name {
                SystemFn::Past => SystemFn::Past,
                SystemFn::Rose => SystemFn::Rose,
                SystemFn::Fell => SystemFn::Fell,
                SystemFn::Stable => SystemFn::Stable,
                _ => return None,
            };
            args.first().map(|a| (kind, a))
        }
        ExprKind::Call { callee, args } => {
            let id = match &*callee.kind {
                ExprKind::Ident(id) => id,
                _ => return None,
            };
            let kind = match id.name.as_str() {
                "past" => SystemFn::Past,
                "rose" => SystemFn::Rose,
                "fell" => SystemFn::Fell,
                "stable" => SystemFn::Stable,
                _ => return None,
            };
            match args.first()? {
                CallArg::Expr(inner) => Some((kind, inner)),
                CallArg::Named { value, .. } => Some((kind, value)),
            }
        }
        _ => None,
    }
}

/// True if an `assert <expr>` should lower to a concurrent (every-cycle)
/// check rather than a one-shot inline check. Per spec §2 LL(1) table:
/// a bare ident referencing a declared property, or any expression
/// containing temporal operators (`|->`, `|=>`, `##N`, `throughout`,
/// `within`, `intersect`) or temporal system calls (`$past`, `$rose`,
/// `$fell`, `$stable`), runs as a concurrent property. Everything else —
/// plain boolean / arithmetic / comparison — is an immediate check.
fn is_concurrent_assertion(
    expr: &Expr,
    properties: &std::collections::HashMap<String, Expr>,
) -> bool {
    if let ExprKind::Ident(id) = &*expr.kind {
        if properties.contains_key(&id.name) {
            return true;
        }
    }
    contains_temporal(expr)
}

fn contains_temporal(e: &Expr) -> bool {
    if match_temporal_call(e).is_some() {
        return true;
    }
    match &*e.kind {
        ExprKind::Binary { op, lhs, rhs } => {
            matches!(
                op,
                BinaryOp::PipeImplies
                    | BinaryOp::PipeImpliesNext
                    | BinaryOp::Throughout
                    | BinaryOp::Within
                    | BinaryOp::Intersect
            ) || contains_temporal(lhs)
                || contains_temporal(rhs)
        }
        ExprKind::SystemCall { args, .. } => args.iter().any(contains_temporal),
        ExprKind::HashHash { .. } | ExprKind::SeqRepeat { .. } => true,
        ExprKind::Field { target, .. } => contains_temporal(target),
        ExprKind::Index { target, index } => contains_temporal(target) || contains_temporal(index),
        ExprKind::BitSlice { target, hi, lo } => {
            contains_temporal(target) || contains_temporal(hi) || contains_temporal(lo)
        }
        ExprKind::Call { callee, args } => {
            contains_temporal(callee)
                || args.iter().any(|a| match a {
                    CallArg::Expr(x) => contains_temporal(x),
                    CallArg::Named { value, .. } => contains_temporal(value),
                })
        }
        ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => contains_temporal(expr),
        ExprKind::Paren(inner) => contains_temporal(inner),
        ExprKind::Send { target, value } => contains_temporal(target) || contains_temporal(value),
        ExprKind::SetLit(items) => items.iter().any(contains_temporal),
        _ => false,
    }
}

/// One temporal SystemCall occurrence ($past/$rose/$fell/$stable) collected
/// from a property body. The codegen allocates a static delay slot per
/// occurrence; references to the call get substituted to the slot.
struct Temporal {
    /// Span of the SystemCall expression itself — used as the substitution
    /// key in `prop_subs`.
    call_span: Span,
    /// Which kind of system call this is.
    kind: SystemFn,
    /// The argument expression — captured each cycle into a `_curN` local.
    inner: Expr,
}

/// Pre-walk a property body and return one `Temporal` entry per
/// $past/$rose/$fell/$stable call. Order is left-to-right depth-first.
fn collect_temporal_occurrences(body: &Expr) -> Vec<Temporal> {
    fn walk(e: &Expr, out: &mut Vec<Temporal>) {
        if let Some((kind, inner)) = match_temporal_call(e) {
            out.push(Temporal {
                call_span: e.span,
                kind,
                inner: inner.clone(),
            });
            // Don't recurse into the argument — nested `past(past(x))` not
            // supported in v0 and would need slot-of-slot accounting.
            return;
        }
        match &*e.kind {
            ExprKind::Field { target, .. } => walk(target, out),
            ExprKind::Index { target, index } => {
                walk(target, out);
                walk(index, out);
            }
            ExprKind::BitSlice { target, hi, lo } => {
                walk(target, out);
                walk(hi, out);
                walk(lo, out);
            }
            ExprKind::Call { callee, args } => {
                walk(callee, out);
                for a in args {
                    if let CallArg::Expr(e) = a {
                        walk(e, out);
                    } else if let CallArg::Named { value, .. } = a {
                        walk(value, out);
                    }
                }
            }
            ExprKind::Cast { expr, .. } => walk(expr, out),
            ExprKind::Send { target, value } => {
                walk(target, out);
                walk(value, out);
            }
            ExprKind::Unary { expr, .. } => walk(expr, out),
            ExprKind::Binary { lhs, rhs, .. } => {
                walk(lhs, out);
                walk(rhs, out);
            }
            ExprKind::Paren(inner) => walk(inner, out),
            ExprKind::SystemCall { args, .. } => {
                for a in args {
                    walk(a, out);
                }
            }
            ExprKind::HashHash { expr, .. } => walk(expr, out),
            ExprKind::SeqRepeat { expr, .. } => walk(expr, out),
            ExprKind::SetLit(items) => {
                for i in items {
                    walk(i, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(body, &mut out);
    out
}

/// Best-effort human label for a property assertion — used in FAIL log
/// lines so the user can identify which check tripped. Prefers the named
/// form (`assert property foo` → "foo"), then falls back to "<inline>".
fn property_label(v: &Verify, raw: &Expr) -> String {
    if let Some(n) = &v.named {
        return n.name.clone();
    }
    if let ExprKind::Ident(id) = &*raw.kind {
        return id.name.clone();
    }
    "<inline>".to_string()
}

fn escape_c(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Render a HARC `Expr` back to source-level text via the pretty
/// printer. Used by `wait until` codegen (v1 emission and TB-IR
/// lowering's `PredSrc::src_text`) to label each sub-predicate in the
/// timeout diagnostic with the user's original expression
/// (e.g. `env.agent.idle(100)` rather than a synthetic index).
pub(crate) fn expr_source_str(e: &Expr) -> String {
    let mut buf = String::new();
    crate::pretty::print_expr(&mut buf, e);
    buf
}

fn txn_body_has_keep(items: &[TxnBodyItem]) -> bool {
    items.iter().any(|item| match item {
        TxnBodyItem::Keep(_) => true,
        TxnBodyItem::When(w) => txn_body_has_keep(&w.items),
        TxnBodyItem::Field(_) => false,
    })
}

fn txn_body_has_solver_policy(
    items: &[TxnBodyItem],
    enum_names: &std::collections::HashSet<&str>,
) -> bool {
    items.iter().any(|item| match item {
        TxnBodyItem::Field(f) => txn_field_has_solver_policy(f, enum_names),
        TxnBodyItem::When(w) => txn_body_has_solver_policy(&w.items, enum_names),
        TxnBodyItem::Keep(_) => false,
    })
}

fn fields_have_solver_policy(
    fields: &[Field],
    enum_names: &std::collections::HashSet<&str>,
) -> bool {
    fields
        .iter()
        .any(|field| txn_field_has_solver_policy(field, enum_names))
}

fn txn_field_has_solver_policy(f: &Field, enum_names: &std::collections::HashSet<&str>) -> bool {
    if f.attrs
        .iter()
        .any(|a| a.name.name == "unique" || a.name.name == "range")
    {
        return true;
    }
    match &f.ty {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => true,
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                type_arg_width(args).is_some_and(|w| w <= 1024)
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => type_arg_width(args).is_some_and(|w| w <= 1024),
            _ => false,
        },
        TypeExpr::Named { name, .. } => name
            .segments
            .last()
            .is_some_and(|segment| enum_names.contains(segment.name.as_str())),
    }
}

fn collect_txn_keeps(items: &[TxnBodyItem]) -> Vec<Expr> {
    let mut out = Vec::new();
    collect_txn_keeps_with_guard(items, None, &mut out);
    out
}

fn collect_record_keeps(
    items: &[TxnBodyItem],
    record_bodies: &std::collections::HashMap<String, Vec<TxnBodyItem>>,
    record_fields: &std::collections::HashMap<String, Vec<Field>>,
) -> Vec<Expr> {
    let mut out = collect_txn_keeps(items);
    for f in txn_direct_fields(items) {
        let Some(child_ty) = record_type_name(&f.ty) else {
            continue;
        };
        if !record_fields.contains_key(child_ty) {
            continue;
        }
        let Some(child_body) = record_bodies.get(child_ty) else {
            continue;
        };
        let child_keeps = collect_record_keeps(child_body, record_bodies, record_fields);
        if child_keeps.is_empty() {
            continue;
        }
        let prefix = Expr::new(ExprKind::Ident(f.name.clone()), f.name.span);
        let child_fields: std::collections::HashSet<String> = record_fields
            .get(child_ty)
            .into_iter()
            .flat_map(|fields| fields.iter().map(|field| field.name.name.clone()))
            .collect();
        for keep in child_keeps {
            out.push(prefix_record_keep_expr(&keep, &prefix, &child_fields));
        }
    }
    out
}

fn prefix_record_keep_expr(
    expr: &Expr,
    prefix: &Expr,
    field_names: &std::collections::HashSet<String>,
) -> Expr {
    let kind = match &*expr.kind {
        ExprKind::Ident(id) if field_names.contains(&id.name) => ExprKind::Field {
            target: prefix.clone(),
            name: id.clone(),
        },
        ExprKind::Field { target, name } => ExprKind::Field {
            target: prefix_record_keep_expr(target, prefix, field_names),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: prefix_record_keep_expr(target, prefix, field_names),
            index: prefix_record_keep_expr(index, prefix, field_names),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: prefix_record_keep_expr(target, prefix, field_names),
            hi: prefix_record_keep_expr(hi, prefix, field_names),
            lo: prefix_record_keep_expr(lo, prefix, field_names),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: prefix_record_keep_expr(callee, prefix, field_names),
            args: args
                .iter()
                .map(|arg| match arg {
                    CallArg::Expr(e) => {
                        CallArg::Expr(prefix_record_keep_expr(e, prefix, field_names))
                    }
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: prefix_record_keep_expr(value, prefix, field_names),
                    },
                })
                .collect(),
        },
        ExprKind::ForEachConstraint { var, iter, body } => ExprKind::ForEachConstraint {
            var: var.clone(),
            iter: prefix_record_keep_expr(iter, prefix, field_names),
            body: body
                .iter()
                .map(|e| {
                    if field_names.contains(&var.name) {
                        e.clone()
                    } else {
                        prefix_record_keep_expr(e, prefix, field_names)
                    }
                })
                .collect(),
        },
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: prefix_record_keep_expr(expr, prefix, field_names),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: prefix_record_keep_expr(expr, prefix, field_names),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: prefix_record_keep_expr(lhs, prefix, field_names),
            rhs: prefix_record_keep_expr(rhs, prefix, field_names),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: prefix_record_keep_expr(cond, prefix, field_names),
            then_branch: prefix_record_keep_expr(then_branch, prefix, field_names),
            else_branch: prefix_record_keep_expr(else_branch, prefix, field_names),
        },
        ExprKind::Paren(inner) => {
            ExprKind::Paren(prefix_record_keep_expr(inner, prefix, field_names))
        }
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: prefix_record_keep_expr(expr, prefix, field_names),
            set: prefix_record_keep_expr(set, prefix, field_names),
        },
        ExprKind::SetLit(items) => ExprKind::SetLit(
            items
                .iter()
                .map(|e| prefix_record_keep_expr(e, prefix, field_names))
                .collect(),
        ),
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo
                .as_ref()
                .map(|e| prefix_record_keep_expr(e, prefix, field_names)),
            hi: hi
                .as_ref()
                .map(|e| prefix_record_keep_expr(e, prefix, field_names)),
        },
        _ => return expr.clone(),
    };
    Expr::new(kind, expr.span)
}

fn collect_txn_keeps_with_guard(items: &[TxnBodyItem], guard: Option<Expr>, out: &mut Vec<Expr>) {
    for item in items {
        match item {
            TxnBodyItem::Keep(k) => {
                let expr = match &guard {
                    Some(g) => guarded_keep_expr(g.clone(), k.expr.clone(), k.span),
                    None => k.expr.clone(),
                };
                out.push(expr);
            }
            TxnBodyItem::When(w) => {
                let next_guard = match &guard {
                    Some(g) => and_join(&[g.clone(), w.discriminant.clone()], w.span),
                    None => w.discriminant.clone(),
                };
                collect_txn_keeps_with_guard(&w.items, Some(next_guard), out);
            }
            TxnBodyItem::Field(_) => {}
        }
    }
}

fn guarded_keep_expr(guard: Expr, keep: Expr, span: Span) -> Expr {
    if let ExprKind::ForEachConstraint { var, iter, body } = &*keep.kind {
        return Expr::new(
            ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: iter.clone(),
                body: body
                    .iter()
                    .map(|clause| guarded_keep_expr(guard.clone(), clause.clone(), clause.span))
                    .collect(),
            },
            span,
        );
    }
    let not_guard = Expr::new(
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr: Expr::new(ExprKind::Paren(guard), span),
        },
        span,
    );
    Expr::new(
        ExprKind::Binary {
            op: BinaryOp::OrOr,
            lhs: not_guard,
            rhs: keep,
        },
        span,
    )
}

fn guarded_constraint_guard(expr: &Expr) -> Option<&Expr> {
    let ExprKind::Binary {
        op: BinaryOp::OrOr,
        lhs,
        ..
    } = &*expr.kind
    else {
        return None;
    };
    let ExprKind::Unary {
        op: UnaryOp::Not,
        expr,
    } = &*lhs.kind
    else {
        return None;
    };
    match &*expr.kind {
        ExprKind::Paren(inner) => Some(inner),
        _ => Some(expr),
    }
}

/// Combine a list of constraint expressions into one
/// `Binary(AndAnd, …)` chain. Used by `expand_relation_subtree` when a
/// block-form relation appears in a position that expects a single
/// expression (e.g. nested inside another `Binary &&`). Empty input
/// returns a literal `true`. Single-element input returns the element
/// unchanged.
fn and_join(exprs: &[Expr], span: Span) -> Expr {
    if exprs.is_empty() {
        return Expr::new(ExprKind::Bool(true), span);
    }
    let mut iter = exprs.iter().cloned();
    let mut acc = iter.next().unwrap();
    for next in iter {
        let s = acc.span.merge(next.span);
        acc = Expr::new(
            ExprKind::Binary {
                op: BinaryOp::AndAnd,
                lhs: acc,
                rhs: next,
            },
            s,
        );
    }
    acc
}

/// Walk `expr` and replace each `ExprKind::Ident(name)` whose name is
/// in `subst` with the corresponding substitute expression (cloned in
/// place). Used by `expand_relation_calls` to bind each relation's
/// formal parameter to the actual call argument before the body is
/// added to the Z3 solver block.
///
/// The substitute can be any expression — a bare ident (the common
/// case: `R(pkt)` → `pkt`), a field access (`R(env.agent.txn)`), or
/// a deeper subtree. Field-access names (`Field { name, .. }`) are
/// attribute references, not bindings, so they're never substituted.
fn substitute_idents(expr: &Expr, subst: &std::collections::HashMap<String, Expr>) -> Expr {
    let span = expr.span;
    let new_kind: ExprKind = match &*expr.kind {
        ExprKind::Ident(id) => {
            if let Some(replacement) = subst.get(&id.name) {
                // Splice the replacement's *kind* in at the current
                // span. Keeping the original ident's span preserves
                // source attribution in any future diagnostic.
                return Expr::new((*replacement.kind).clone(), span);
            }
            ExprKind::Ident(id.clone())
        }
        ExprKind::Field { target, name } => ExprKind::Field {
            target: substitute_idents(target, subst),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: substitute_idents(target, subst),
            index: substitute_idents(index, subst),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: substitute_idents(target, subst),
            hi: substitute_idents(hi, subst),
            lo: substitute_idents(lo, subst),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: substitute_idents(callee, subst),
            args: args
                .iter()
                .map(|a| match a {
                    CallArg::Expr(e) => CallArg::Expr(substitute_idents(e, subst)),
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: substitute_idents(value, subst),
                    },
                })
                .collect(),
        },
        ExprKind::ForEachConstraint { var, iter, body } => {
            let mut scoped = subst.clone();
            scoped.remove(&var.name);
            ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: substitute_idents(iter, subst),
                body: body.iter().map(|e| substitute_idents(e, &scoped)).collect(),
            }
        }
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: substitute_idents(expr, subst),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: substitute_idents(expr, subst),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: substitute_idents(lhs, subst),
            rhs: substitute_idents(rhs, subst),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: substitute_idents(cond, subst),
            then_branch: substitute_idents(then_branch, subst),
            else_branch: substitute_idents(else_branch, subst),
        },
        ExprKind::Paren(inner) => ExprKind::Paren(substitute_idents(inner, subst)),
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: substitute_idents(expr, subst),
            set: substitute_idents(set, subst),
        },
        ExprKind::SetLit(items) => {
            ExprKind::SetLit(items.iter().map(|x| substitute_idents(x, subst)).collect())
        }
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo.as_ref().map(|x| substitute_idents(x, subst)),
            hi: hi.as_ref().map(|x| substitute_idents(x, subst)),
        },
        // Literals, ImplicitSelf, and the spec-§5 / §6 / §8 forms
        // (HashHash, SeqRepeat, DistLit, etc.) don't bind names and
        // aren't expected inside constraint bodies; pass through
        // verbatim. If a relation body happens to contain one of these
        // shapes, the downstream constraint translator will reject it
        // with a clear "not supported" error, same as it would for an
        // un-inlined body.
        other => other.clone(),
    };
    Expr::new(new_kind, span)
}

#[cfg(test)]
mod bus_gate_tests {
    use super::{bus_param_env, gate_passes};
    use crate::ast::{Item, TypeExpr};
    use crate::parser::parse_source;

    fn bus_of(src: &str) -> crate::ast::BusDecl {
        let f = parse_source(src).expect("parse");
        match f.items.into_iter().next().expect("one item") {
            Item::Bus(b) => b,
            other => panic!("expected bus, got {other:?}"),
        }
    }

    fn bind_ty(src: &str) -> TypeExpr {
        // Parse a component field `x : <Type>` so the generic args land in a
        // TypeExpr::Named we can hand to bus_param_env (mirrors the bind-site
        // type the real codegen reads off a `let`/field). `testbench` parses to
        // Item::Env (a ComponentDecl); its fields are ComponentItem::Field.
        let full = format!("testbench Tb\n  x : {src}\nend testbench Tb");
        let f = parse_source(&full).expect("parse tb");
        for it in &f.items {
            if let Item::Env(c) = it {
                for ci in &c.items {
                    if let crate::ast::ComponentItem::Field(fd) = ci {
                        return fd.ty.clone();
                    }
                }
            }
        }
        panic!("no field type found");
    }

    const GATED_BUS: &str = r#"bus BusAxiG
  param ADDR_W: const = 32;
  param READ: const = 1;
  param WRITE: const = 1;
  generate_if READ
    ar_valid: out Bool;
    r_valid:  in  Bool;
  end generate_if
  generate_if WRITE
    aw_valid: out Bool;
  end generate_if
  hready: in Bool;
end bus BusAxiG"#;

    fn gate_for(bus: &crate::ast::BusDecl, sig: &str) -> Option<crate::ast::Expr> {
        bus.signals
            .iter()
            .find(|s| s.name.name == sig)
            .and_then(|s| s.gate.clone())
    }

    #[test]
    fn defaults_keep_both_channels_present() {
        let bus = bus_of(GATED_BUS);
        let env = bus_param_env(&bus, None);
        assert_eq!(env.get("READ"), Some(&1));
        assert_eq!(env.get("WRITE"), Some(&1));
        // With READ=WRITE=1 defaults, every gated signal is present.
        assert!(gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        // Ungated signal is always present.
        assert!(gate_passes(gate_for(&bus, "hready").as_ref(), &env));
    }

    #[test]
    fn read_zero_override_drops_read_channel() {
        let bus = bus_of(GATED_BUS);
        let ty = bind_ty("BusAxiG#(READ=0)");
        let env = bus_param_env(&bus, Some(&ty));
        assert_eq!(env.get("READ"), Some(&0));
        // READ=0 ⇒ the AR/R channel is gated OFF (absent), matching arch build.
        assert!(!gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
        assert!(!gate_passes(gate_for(&bus, "r_valid").as_ref(), &env));
        // WRITE still defaults to 1 ⇒ AW channel present.
        assert!(gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        // Ungated signal unaffected.
        assert!(gate_passes(gate_for(&bus, "hready").as_ref(), &env));
    }

    #[test]
    fn write_zero_override_drops_write_channel() {
        let bus = bus_of(GATED_BUS);
        let ty = bind_ty("BusAxiG#(WRITE=0)");
        let env = bus_param_env(&bus, Some(&ty));
        assert!(!gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
    }

    #[test]
    fn default_off_param_drops_channel_with_no_override() {
        // A bus whose default gates a channel OFF must model it absent even
        // without any bind-site override.
        let bus = bus_of(
            r#"bus B
  param EN: const = 0;
  generate_if EN
    x: out Bool;
  end generate_if
  y: in Bool;
end bus B"#,
        );
        let env = bus_param_env(&bus, None);
        assert!(!gate_passes(gate_for(&bus, "x").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "y").as_ref(), &env));
    }

    #[test]
    fn unfoldable_gate_is_conservatively_present() {
        // A gate referencing an unknown param can't fold; we keep the signal
        // present rather than silently dropping it.
        let bus = bus_of(
            r#"bus B
  generate_if UNKNOWN
    x: out Bool;
  end generate_if
end bus B"#,
        );
        let env = bus_param_env(&bus, None); // no UNKNOWN binding
        assert!(gate_passes(gate_for(&bus, "x").as_ref(), &env));
    }

    // ── DUT-port-level override ingestion (fix/bus-port-override-from-archi) ──

    use super::{
        bus_param_env_with_port_override, collect_port_overrides_from_src,
        dut_bus_port_overrides_from_files,
    };

    /// The realistic case: the DUT module's own port carries the override
    /// (`port s: target BusAxiG<WRITE=0>`, as recorded in its `.arch`/`.archi`),
    /// and the harc TB does NOT restate it at the bind site. The override must
    /// be ingested from the interface line and drop the WRITE-gated channel from
    /// the modeled port set, agreeing with `arch build`'s flatten.
    #[test]
    fn dut_port_override_from_archi_drops_gated_channel_no_tb_generic() {
        let bus = bus_of(GATED_BUS);
        // `.archi` interface line shape emitted by arch#567.
        let archi = "module RwTarget\n  port clk: in Clock<SysDomain>;\n  port s: target BusAxiG<WRITE=0>;\nend module RwTarget\n";
        let mut overrides = std::collections::HashMap::new();
        collect_port_overrides_from_src(archi, &mut overrides);
        // Override keyed by DUT port name `s`.
        let s_ov = overrides.get("s").expect("port s override ingested");
        assert_eq!(s_ov.get("WRITE"), Some(&0));

        // No harc-TB bind-site generic (bind_ty = None) — the DUT port override
        // is the ONLY non-default layer.
        let env = bus_param_env_with_port_override(&bus, None, Some(s_ov));
        assert_eq!(env.get("WRITE"), Some(&0));
        // WRITE=0 ⇒ AW channel gated OFF (absent), matching `arch build`.
        assert!(!gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
        // READ still defaults to 1 ⇒ AR/R channel present.
        assert!(gate_passes(gate_for(&bus, "ar_valid").as_ref(), &env));
        assert!(gate_passes(gate_for(&bus, "r_valid").as_ref(), &env));
        // Ungated signal unaffected.
        assert!(gate_passes(gate_for(&bus, "hready").as_ref(), &env));
    }

    /// Precedence: when the DUT port override AND a harc-TB bind-site generic
    /// name the same param, the DUT port's override wins (it reflects what
    /// `arch build` actually synthesized).
    #[test]
    fn dut_port_override_wins_over_tb_bind_generic() {
        let bus = bus_of(GATED_BUS);
        // TB restates WRITE=1, but the DUT port was built WRITE=0.
        let tb_ty = bind_ty("BusAxiG#(WRITE=1)");
        let mut ov = std::collections::HashMap::new();
        ov.insert("WRITE".to_string(), 0i64);
        let env = bus_param_env_with_port_override(&bus, Some(&tb_ty), Some(&ov));
        assert_eq!(env.get("WRITE"), Some(&0));
        assert!(!gate_passes(gate_for(&bus, "aw_valid").as_ref(), &env));
    }

    /// `Vec<BusName<P=v>, N>` array-of-bus port form (arch#567) ingests the
    /// element bus's override.
    #[test]
    fn vec_of_bus_port_override_is_ingested() {
        let archi = "module M\n  port outs: initiator Vec<BusAxiG<READ=0>, 4>;\nend module M\n";
        let mut overrides = std::collections::HashMap::new();
        collect_port_overrides_from_src(archi, &mut overrides);
        let ov = overrides.get("outs").expect("vec-of-bus override ingested");
        assert_eq!(ov.get("READ"), Some(&0));
    }

    /// A port with no override contributes nothing (no spurious entry).
    #[test]
    fn plain_bus_port_contributes_no_override() {
        let archi = "module M\n  port s: target BusAxiG;\n  port busy: out Bool;\nend module M\n";
        let mut overrides = std::collections::HashMap::new();
        collect_port_overrides_from_src(archi, &mut overrides);
        assert!(overrides.is_empty());
    }

    /// A `.arch` source on disk (no sibling `.archi`) is scanned directly — the
    /// source port decl carries the identical override line.
    #[test]
    fn override_read_from_arch_source_on_disk() {
        let dir =
            std::env::temp_dir().join(format!("harc_archi_override_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let arch = dir.join("RwTarget.arch");
        std::fs::write(
            &arch,
            "module RwTarget\n  port clk: in Clock<SysDomain>;\n  port s: target BusAxiG<WRITE=0>;\nend module RwTarget\n",
        )
        .unwrap();
        let overrides = dut_bus_port_overrides_from_files(&[arch.clone()]);
        assert_eq!(overrides.get("s").and_then(|m| m.get("WRITE")), Some(&0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `.archi` sibling is preferred over the `.arch` source when both exist.
    #[test]
    fn archi_sibling_preferred_over_arch_source() {
        let dir = std::env::temp_dir().join(format!("harc_archi_pref_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let arch = dir.join("M.arch");
        // .arch says WRITE=1; .archi (authoritative, post-elaboration) says WRITE=0.
        std::fs::write(
            &arch,
            "module M\n  port s: target BusAxiG<WRITE=1>;\nend module M\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("M.archi"),
            "module M\n  port s: target BusAxiG<WRITE=0>;\nend module M\n",
        )
        .unwrap();
        let overrides = dut_bus_port_overrides_from_files(&[arch]);
        assert_eq!(
            overrides.get("s").and_then(|m| m.get("WRITE")),
            Some(&0),
            "the .archi sibling override should win"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod vec_lane_sv_scan_tests {
    use super::scan_sv_module_lane_widths;

    fn scan(src: &str, top: &str) -> std::collections::HashMap<String, u32> {
        scan_sv_module_lane_widths(src, top).unwrap_or_default()
    }

    #[test]
    fn packed_one_dim_lane_is_width_one() {
        let sv = "module M (\n  input logic [2:0] valid,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("valid"), Some(&1));
    }

    #[test]
    fn packed_two_dim_lane_resolves_inner_width() {
        let sv = "module M (\n  input logic [2:0] [7:0] data,\n  input logic [3:0] [1:0] resp,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("data"), Some(&8));
        assert_eq!(m.get("resp"), Some(&2));
    }

    #[test]
    fn lane_width_folds_against_module_params() {
        let sv = "module M #(\n  parameter int W = 5\n) (\n  input logic [2:0] [W-1:0] id,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("id"), Some(&5));
    }

    #[test]
    fn unpacked_array_port_is_excluded() {
        // `logic [7:0] uvec [N]` — packed [7:0] before the name, UNPACKED
        // [N] after the name. This is a real SV unpacked array and must
        // NOT be recorded as a packed lane vector.
        let sv = "module M #(\n  parameter int N = 3\n) (\n  input logic [7:0] uvec [N],\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("uvec"), None);
    }

    #[test]
    fn plain_clk_rst_scalars_are_excluded() {
        let sv = "module M (\n  input logic clk,\n  input logic rst,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert!(m.is_empty());
    }

    #[test]
    fn only_named_top_module_is_scanned() {
        let sv = "module Other (\n  input logic [2:0] [7:0] data,\n);\nendmodule\nmodule M (\n  input logic [3:0] flag,\n);\nendmodule\n";
        let m = scan(sv, "M");
        assert_eq!(m.get("flag"), Some(&1));
        // `data` belongs to `Other`, not `M`.
        assert_eq!(m.get("data"), None);
    }
}

#[cfg(test)]
mod time_literal_tests {
    use super::time_literal_to_ps;

    /// Largest value per unit whose picosecond conversion still fits in
    /// i64 (i64::MAX / factor), and the smallest rejected (one above).
    /// Mirrors `ir::lower::time_literal_tests` — the two emitters must
    /// accept/reject identically.
    #[test]
    fn boundary_per_unit() {
        let cases = [
            ("ps", 9_223_372_036_854_775_807i64, 1i64),
            ("ns", 9_223_372_036_854_775, 1_000),
            ("us", 9_223_372_036_854, 1_000_000),
            ("ms", 9_223_372_036, 1_000_000_000),
            ("s", 9_223_372, 1_000_000_000_000),
        ];
        for (unit, max, factor) in cases {
            let ok = format!("{max}{unit}");
            assert_eq!(
                time_literal_to_ps(&ok),
                Ok(max.checked_mul(factor).unwrap()),
                "largest accepted value for {unit}"
            );
            // Smallest rejected: max+1 (for ps this overflows i64 itself,
            // so build the digit string from u64 instead).
            let over = format!("{}{unit}", (max as u64) + 1);
            let err = time_literal_to_ps(&over).expect_err("must reject");
            assert_eq!(
                err,
                format!("time literal `{over}` overflows the picosecond range (max {max}{unit})")
            );
        }
    }

    #[test]
    fn review_finding_repro_9300000s() {
        let err = time_literal_to_ps("9300000s").expect_err("must reject");
        assert_eq!(
            err,
            "time literal `9300000s` overflows the picosecond range (max 9223372s)"
        );
    }

    #[test]
    fn underscores_and_units_still_accepted() {
        assert_eq!(time_literal_to_ps("1_000ns"), Ok(1_000_000));
        assert_eq!(time_literal_to_ps("5ns"), Ok(5_000));
        assert!(time_literal_to_ps("5cycles").is_err());
    }
}
