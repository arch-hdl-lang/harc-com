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

use crate::ast::*;
pub use crate::codegen::probe_catalog::dut_probes;

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

/// Top-module DUT port name → total packed width. This reuses the same
/// tolerant ANSI/non-ANSI scanner as the DPI accessor path. TBIR cover
/// emission needs the total width to reject untyped C++ composition over a
/// raw `HarcWide<N>` port once the SV interface is available.
pub fn dut_port_widths_from_sv(
    sv_sources: &[std::path::PathBuf],
    top: &str,
) -> std::collections::HashMap<String, u32> {
    cosim_ports_from_sv(sv_sources, top)
        .unwrap_or_default()
        .into_iter()
        .map(|port| (port.name, port.width_bits))
        .collect()
}

/// Top-module DUT scalar port name → declared packed width from native
/// `.archi`/`.arch` interfaces. This complements [`dut_port_widths_from_sv`]
/// on the `--dut` path, where no generated SystemVerilog is available to
/// scan. Only concrete scalar widths are retained; parameter-dependent widths
/// live in the typed interface catalog as explicit unresolved metadata.
/// Aggregate bus widths are discovered from the flattened SV interface.
pub fn dut_port_widths_from_files(
    dut_files: &[std::path::PathBuf],
    top: &str,
) -> std::collections::HashMap<String, u32> {
    let mut out = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for f in dut_files {
        let mut candidates = Vec::new();
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
            if collect_port_widths_from_src(&src, top, &mut out) {
                break;
            }
        }
    }
    out
}

/// Add scalar ARCH DUT ports to the packed-lane table as one-bit lanes.
///
/// The native ARCH model exposes `UInt<N>`/`SInt<N>` ports as C++ integer
/// scalars, so a HARC bit-select must use `harc_vec_lane_{read,write}<1>`
/// rather than raw `operator[]`. `scalar_port_widths` comes from
/// [`dut_port_widths_from_files`], which deliberately excludes unpacked Vec
/// and bus ports; those must keep their existing direct-array lowering.
pub fn add_arch_scalar_bit_lanes(
    lane_widths: &mut std::collections::HashMap<String, u32>,
    scalar_port_widths: &std::collections::HashMap<String, u32>,
) {
    for name in scalar_port_widths.keys() {
        lane_widths.entry(name.clone()).or_insert(1);
    }
}

pub(crate) fn collect_port_widths_from_src(
    src: &str,
    top: &str,
    out: &mut std::collections::HashMap<String, u32>,
) -> bool {
    let mut in_top = false;
    let mut found_top = false;
    let mut pending_port = String::new();
    for raw in src.lines() {
        let line = raw.split("//").next().unwrap_or(raw).trim();
        if let Some(rest) = line.strip_prefix("module ") {
            in_top = rest.split_whitespace().next() == Some(top);
            found_top |= in_top;
            pending_port.clear();
            continue;
        }
        if line.starts_with("end module") {
            if in_top && !pending_port.is_empty() {
                collect_scalar_port_width_from_decl(&pending_port, out);
            }
            pending_port.clear();
            in_top = false;
            continue;
        }
        if !in_top {
            continue;
        }
        if pending_port.is_empty() {
            if !line.starts_with("port ") {
                continue;
            }
            pending_port.push_str(line);
        } else if !line.is_empty() {
            pending_port.push(' ');
            pending_port.push_str(line);
        }
        while let Some(end) = pending_port.find(';') {
            let decl = pending_port[..=end].to_string();
            let tail = pending_port[end + 1..].trim_start().to_string();
            collect_scalar_port_width_from_decl(&decl, out);
            pending_port = tail;
            // A physical line may contain several valid `port ...;`
            // declarations. Anything else after a completed declaration is
            // module body text, not a continuation of port metadata.
            if !pending_port.is_empty() && !pending_port.starts_with("port ") {
                pending_port.clear();
                break;
            }
        }
    }
    found_top
}

fn collect_scalar_port_width_from_decl(
    decl: &str,
    out: &mut std::collections::HashMap<String, u32>,
) {
    let Some(rest) = decl.strip_prefix("port ") else {
        return;
    };
    // Legacy registered outputs spell the declaration `port reg q:`.
    // `reg` is storage metadata, not part of the port name.
    let rest = rest.strip_prefix("reg ").unwrap_or(rest);
    let Some(colon) = rest.find(':') else {
        return;
    };
    let name = rest[..colon].trim();
    if name.is_empty() || !is_simple_ident(name) {
        return;
    }
    let after_colon = rest[colon + 1..].trim();
    let ty_tail = if let Some(t) = after_colon.strip_prefix("in ") {
        t
    } else if let Some(t) = after_colon.strip_prefix("out ") {
        t
    } else {
        // Bus perspectives flatten to multiple physical signals; their
        // widths come from the generated SV table when that path is used.
        return;
    };
    let ty_tail = ty_tail.split(';').next().unwrap_or(ty_tail).trim();
    let ty_str = leading_arch_type_str(ty_tail);
    let ty_str = outer_element_type_str(&ty_str, "pipe_reg").unwrap_or(ty_str);
    let Ok(ty) = crate::parser::parse_type_expr_fragment(&ty_str) else {
        return;
    };
    let width = concrete_port_type_bit_width(&ty)
        .or_else(|| scalar_port_type_has_unresolved_width(&ty).then_some(u32::MAX));
    if let Some(width) = width {
        out.entry(name.to_string()).or_insert(width);
    }
}

fn leading_arch_type_str(ty_tail: &str) -> String {
    let mut depth = 0i32;
    for (i, c) in ty_tail.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            c if c.is_whitespace() && depth == 0 => return ty_tail[..i].trim().to_string(),
            _ => {}
        }
    }
    ty_tail.trim().to_string()
}

fn concrete_port_type_bit_width(ty: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = ty else {
        return None;
    };
    match name {
        BuiltinTy::UInt
        | BuiltinTy::UIntCap
        | BuiltinTy::SInt
        | BuiltinTy::SIntCap
        | BuiltinTy::Bits => {
            if args.is_empty() {
                Some(64)
            } else {
                type_arg_width(args)
            }
        }
        BuiltinTy::Bool
        | BuiltinTy::BoolLower
        | BuiltinTy::Bit
        | BuiltinTy::Clock
        | BuiltinTy::Reset => Some(1),
        BuiltinTy::Int => Some(32),
        _ => None,
    }
}

fn concrete_port_ir_type_with_params(
    ty: &TypeExpr,
    params: &std::collections::HashMap<String, i64>,
) -> Option<crate::ir::IrType> {
    let TypeExpr::Builtin { name, args, .. } = ty else {
        return None;
    };
    let width = || {
        if args.is_empty() {
            Some(64)
        } else {
            args.first().and_then(|arg| match arg {
                TypeArg::Expr(expr) => eval_const_i64(expr, params)
                    .and_then(|value| u32::try_from(value).ok())
                    .filter(|width| *width > 0),
                _ => None,
            })
        }
    };
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
            Some(crate::ir::IrType::UInt(Some(width()?)))
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(crate::ir::IrType::SInt(Some(width()?))),
        BuiltinTy::Bool | BuiltinTy::BoolLower => Some(crate::ir::IrType::Bool),
        BuiltinTy::Bit | BuiltinTy::Clock | BuiltinTy::Reset => {
            Some(crate::ir::IrType::UInt(Some(1)))
        }
        BuiltinTy::Int => Some(crate::ir::IrType::SInt(Some(32))),
        _ => None,
    }
}

fn scalar_port_type_has_unresolved_width(ty: &TypeExpr) -> bool {
    matches!(
        ty,
        TypeExpr::Builtin {
            name: BuiltinTy::UInt
                | BuiltinTy::UIntCap
                | BuiltinTy::SInt
                | BuiltinTy::SIntCap
                | BuiltinTy::Bits,
            args,
            ..
        } if !args.is_empty() && type_arg_width(args).is_none()
    )
}

/// Core of `vec_lane_widths_from_sv` over a single SV source string.
/// Returns `None` if `top` isn't declared in this source.
pub(crate) fn scan_sv_module_lane_widths(
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

fn scalar_parameter_overrides(
    overrides: &[String],
) -> (
    std::collections::HashMap<String, i64>,
    std::collections::HashSet<String>,
) {
    let mut values = std::collections::HashMap::new();
    let mut names = std::collections::HashSet::new();
    for override_value in overrides {
        let Some((name, value)) = override_value.split_once('=') else {
            continue;
        };
        let name = name.trim();
        names.insert(name.to_string());
        if let Some(value) = eval_sv_const_expr(value.trim(), &values) {
            values.insert(name.to_string(), value);
        }
    }
    (values, names)
}

pub fn dut_interface_catalog(
    sv_sources: &[std::path::PathBuf],
    dut_files: &[std::path::PathBuf],
    top: &str,
    packed_lane_widths: &std::collections::HashMap<String, u32>,
) -> Result<Option<crate::ir::passes::dut_access::DutInterfaceCatalog>, String> {
    dut_interface_catalog_with_parameter_overrides(
        sv_sources,
        dut_files,
        top,
        packed_lane_widths,
        &[],
    )
}

pub fn dut_interface_catalog_with_parameter_overrides(
    sv_sources: &[std::path::PathBuf],
    dut_files: &[std::path::PathBuf],
    top: &str,
    packed_lane_widths: &std::collections::HashMap<String, u32>,
    parameter_overrides: &[String],
) -> Result<Option<crate::ir::passes::dut_access::DutInterfaceCatalog>, String> {
    use crate::ir::passes::dut_access::{DutInterfaceCatalog, DutInterfacePort};
    use crate::ir::PortDirection;

    let (override_values, overridden_names) = scalar_parameter_overrides(parameter_overrides);
    let mut ports = std::collections::BTreeMap::<String, DutInterfacePort>::new();
    let mut found_sv_top = false;
    let mut found_native_top = false;
    if let Some(discovered) = cosim_ports_from_sv_with_parameter_overrides(
        sv_sources,
        top,
        &override_values,
        &overridden_names,
    ) {
        found_sv_top = true;
        for port in discovered {
            let packed_lane_width = if port.unpacked_elems.is_none() {
                port.packed_lane_width
                    .or_else(|| packed_lane_widths.get(&port.name).copied())
            } else {
                None
            };
            ports.insert(
                port.name.clone(),
                DutInterfacePort::new_typed(
                    port.name,
                    if port.is_input {
                        PortDirection::In
                    } else {
                        PortDirection::Out
                    },
                    port.width_bits,
                    if port.is_signed {
                        crate::ir::IrType::SInt(Some(port.width_bits))
                    } else {
                        crate::ir::IrType::UInt(Some(port.width_bits))
                    },
                    packed_lane_width,
                    port.unpacked_elems,
                ),
            );
        }
    }

    let mut seen = std::collections::HashSet::new();
    for file in dut_files {
        let mut candidates = Vec::new();
        if file.extension().and_then(|extension| extension.to_str()) == Some("arch") {
            let archi = file.with_extension("archi");
            if archi.exists() {
                candidates.push(archi);
            }
        }
        candidates.push(file.clone());
        for candidate in candidates {
            if !seen.insert(candidate.clone()) {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&candidate) else {
                continue;
            };
            let mut discovered = Vec::new();
            if collect_port_catalog_from_src(
                &source,
                top,
                &override_values,
                &overridden_names,
                &mut discovered,
            ) {
                found_native_top = true;
                for port in discovered {
                    if let Some(previous) = ports.get(port.name()) {
                        let merged = merge_dut_interface_port(previous, &port)?;
                        ports.insert(port.name().to_string(), merged);
                    } else {
                        ports.insert(port.name().to_string(), port);
                    }
                }
                break;
            }
        }
    }

    if ports.is_empty() {
        if found_sv_top || found_native_top {
            return Err(format!(
                "DUT interface `{top}` was found, but none of its ports have supported scalar metadata"
            ));
        }
        return Ok(None);
    }
    DutInterfaceCatalog::new(top, ports.into_values().collect())
        .map(Some)
        .map_err(|error| error.to_string())
}

fn merge_dut_interface_port(
    lhs: &crate::ir::passes::dut_access::DutInterfacePort,
    rhs: &crate::ir::passes::dut_access::DutInterfacePort,
) -> Result<crate::ir::passes::dut_access::DutInterfacePort, String> {
    use crate::ir::passes::dut_access::DutInterfacePort;
    use crate::ir::IrType;

    let conflict = || {
        Err(format!(
            "DUT interface port `{}` has conflicting resolved metadata between HDL/interface inputs: {:?} versus {:?}",
            rhs.name(), lhs, rhs
        ))
    };
    if lhs.name() != rhs.name() || lhs.direction() != rhs.direction() {
        return conflict();
    }
    let width = match (lhs.resolved_width(), rhs.resolved_width()) {
        (Some(lhs), Some(rhs)) if lhs != rhs => return conflict(),
        (Some(width), _) | (_, Some(width)) => Some(width),
        (None, None) => None,
    };
    let scalar_kind = |ty: &IrType| match ty {
        IrType::Bool => Some(0u8),
        IrType::UInt(_) => Some(1),
        IrType::SInt(_) => Some(2),
        _ => None,
    };
    let lhs_kind = scalar_kind(lhs.value_type()).ok_or_else(|| conflict().unwrap_err())?;
    let rhs_kind = scalar_kind(rhs.value_type()).ok_or_else(|| conflict().unwrap_err())?;
    let compatible_bool = width == Some(1) && [lhs_kind, rhs_kind].contains(&0);
    if lhs_kind != rhs_kind && !(compatible_bool && [lhs_kind, rhs_kind].contains(&1)) {
        return conflict();
    }
    let value_type = match (width, lhs_kind, rhs_kind) {
        (Some(1), 0, _) | (Some(1), _, 0) => IrType::Bool,
        (width, 2, 2) => IrType::SInt(width),
        (width, 1, 1) => IrType::UInt(width),
        _ => return conflict(),
    };
    let packed_lane_width = match (lhs.packed_lane_width(), rhs.packed_lane_width()) {
        (Some(lhs), Some(rhs)) if lhs != rhs => return conflict(),
        (Some(width), _) | (_, Some(width)) => Some(width),
        (None, None) => None,
    };
    let packed_lane_type = match (lhs.packed_lane_type(), rhs.packed_lane_type()) {
        (Some(lhs), Some(rhs)) if lhs == rhs => Some(lhs.clone()),
        (Some(IrType::Bool), Some(IrType::UInt(Some(1))))
        | (Some(IrType::UInt(Some(1))), Some(IrType::Bool)) => Some(IrType::Bool),
        (Some(_), Some(_)) => return conflict(),
        (Some(ty), None) | (None, Some(ty)) => Some(ty.clone()),
        (None, None) => None,
    };
    let unpacked_elements = match (lhs.unpacked_elements(), rhs.unpacked_elements()) {
        (Some(lhs), Some(rhs)) if lhs != rhs => return conflict(),
        (Some(elements), _) | (_, Some(elements)) => Some(elements),
        (None, None) => None,
    };
    let merged = match width {
        Some(width) => DutInterfacePort::new_typed(
            rhs.name(),
            rhs.direction(),
            width,
            value_type,
            packed_lane_width,
            unpacked_elements,
        ),
        None => DutInterfacePort::new_unresolved_with_shape(
            rhs.name(),
            rhs.direction(),
            value_type,
            packed_lane_width,
            unpacked_elements,
        ),
    };
    Ok(match packed_lane_type {
        Some(lane_type) => merged.with_packed_lane_type(lane_type),
        None => merged,
    })
}

fn collect_port_catalog_from_src(
    src: &str,
    top: &str,
    override_values: &std::collections::HashMap<String, i64>,
    overridden_names: &std::collections::HashSet<String>,
    out: &mut Vec<crate::ir::passes::dut_access::DutInterfacePort>,
) -> bool {
    use crate::ir::passes::dut_access::DutInterfacePort;
    use crate::ir::PortDirection;

    #[derive(Clone, Copy)]
    enum GroupShape {
        Scalar,
        Packed(Option<u32>),
    }

    let mut in_top = false;
    let mut found_top = false;
    let mut group = None::<(String, GroupShape)>;
    let mut type_params = std::collections::HashMap::<String, TypeExpr>::new();
    let mut const_params = override_values.clone();
    for raw in src.lines() {
        let line = raw.split("//").next().unwrap_or(raw).trim();
        if let Some(rest) = ["module ", "fifo ", "ram ", "arbiter "]
            .into_iter()
            .find_map(|keyword| line.strip_prefix(keyword))
        {
            in_top = rest.split_whitespace().next() == Some(top);
            found_top |= in_top;
            group = None;
            type_params.clear();
            const_params.clone_from(override_values);
            continue;
        }
        if ["end module", "end fifo", "end ram", "end arbiter"]
            .into_iter()
            .any(|keyword| line.starts_with(keyword))
        {
            in_top = false;
            group = None;
            continue;
        }
        if !in_top {
            continue;
        }
        // `local param` (ARCH `.archi` emits it for module-local constants,
        // e.g. `local param NUM_WORDS: const = 32;`) sizes ports like a
        // `param` does, but cannot be overridden from the command line.
        let (param_rest, is_local) = match line.strip_prefix("local param ") {
            Some(rest) => (Some(rest), true),
            None => (line.strip_prefix("param "), false),
        };
        if let Some(rest) = param_rest {
            if let Some((name, declaration)) = rest.split_once(':') {
                let declaration = declaration.trim();
                if let Some(value) = declaration.strip_prefix("type") {
                    if let Some((_, value)) = value.split_once('=') {
                        let value = value.split(';').next().unwrap_or(value).trim();
                        if let Ok(ty) = crate::parser::parse_type_expr_fragment(value) {
                            type_params.insert(name.trim().to_string(), ty);
                        }
                    }
                } else if let Some(value) = declaration.strip_prefix("const") {
                    if let Some((_, value)) = value.split_once('=') {
                        let value = value.split(';').next().unwrap_or(value).trim();
                        if is_local || !overridden_names.contains(name.trim()) {
                            match eval_sv_const_expr(value, &const_params) {
                                Some(value) => {
                                    const_params.insert(name.trim().to_string(), value);
                                }
                                // `const_params` starts from the overrides: when a
                                // local param's own expression does not evaluate,
                                // drop a same-named override so it cannot size ports.
                                None if is_local => {
                                    const_params.remove(name.trim());
                                }
                                None => {}
                            }
                        }
                    }
                }
            }
            continue;
        }
        if line.starts_with("end ports") {
            group = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("ports[") {
            let Some((count, rest)) = rest.split_once(']') else {
                group = None;
                continue;
            };
            let name = rest
                .trim_start()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches('{');
            let elements = eval_sv_const_expr(count.trim(), &const_params)
                .and_then(|count| u32::try_from(count).ok())
                .filter(|count| *count > 0);
            group = (!name.is_empty()).then(|| (name.to_string(), GroupShape::Packed(elements)));
            continue;
        }
        if let Some(rest) = line.strip_prefix("ports ") {
            let declarator = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches('{');
            let name = declarator.split('[').next().unwrap_or_default();
            let count = declarator
                .strip_prefix(name)
                .and_then(|suffix| suffix.strip_prefix('['))
                .and_then(|suffix| suffix.split_once(']').map(|(count, _)| count));
            let shape = count.map_or(GroupShape::Scalar, |count| {
                GroupShape::Packed(
                    eval_sv_const_expr(count.trim(), &const_params)
                        .and_then(|count| u32::try_from(count).ok())
                        .filter(|count| *count > 0),
                )
            });
            group = (!name.is_empty()).then(|| (name.to_string(), shape));
            continue;
        }
        if let Some(rest) = line.strip_prefix("handshake_channel ") {
            if let Some((declarator, tail)) = rest.split_once(':') {
                let declarator = declarator.trim();
                let name = declarator.split('[').next().unwrap_or_default().trim();
                let count_expr = declarator
                    .strip_prefix(name)
                    .and_then(|suffix| suffix.trim().strip_prefix('['))
                    .and_then(|suffix| suffix.split_once(']').map(|(count, _)| count));
                let elements = match count_expr {
                    Some(count) => eval_sv_const_expr(count.trim(), &const_params)
                        .and_then(|count| u32::try_from(count).ok())
                        .filter(|count| *count > 0),
                    None => Some(1),
                };
                let perspective = tail.split_whitespace().next().unwrap_or_default();
                let (valid_direction, ready_direction) = match perspective {
                    "receive" => (PortDirection::In, PortDirection::Out),
                    "send" => (PortDirection::Out, PortDirection::In),
                    _ => continue,
                };
                for (suffix, direction) in [("valid", valid_direction), ("ready", ready_direction)]
                {
                    let port_name = format!("{name}_{suffix}");
                    match elements {
                        Some(1) => out.push(DutInterfacePort::new_typed(
                            port_name,
                            direction,
                            1,
                            crate::ir::IrType::Bool,
                            None,
                            None,
                        )),
                        Some(elements) => out.push(
                            DutInterfacePort::new_typed(
                                port_name,
                                direction,
                                elements,
                                crate::ir::IrType::UInt(Some(elements)),
                                Some(1),
                                None,
                            )
                            .with_packed_lane_type(crate::ir::IrType::Bool),
                        ),
                        None => out.push(
                            DutInterfacePort::new_unresolved_with_shape(
                                port_name,
                                direction,
                                crate::ir::IrType::UInt(None),
                                Some(1),
                                None,
                            )
                            .with_packed_lane_type(crate::ir::IrType::Bool),
                        ),
                    }
                }
            }
            continue;
        }
        let rest = if let Some(rest) = line.strip_prefix("port ") {
            rest
        } else if group.is_some() {
            line
        } else {
            continue;
        };
        let Some(colon) = rest.find(':') else {
            continue;
        };
        let leaf_name = rest[..colon].trim();
        let name = group
            .as_ref()
            .map(|(group, _)| format!("{group}_{leaf_name}"))
            .unwrap_or_else(|| leaf_name.to_string());
        if name.is_empty() || !is_simple_ident(&name) {
            continue;
        }
        let after_colon = rest[colon + 1..].trim();
        let (direction, ty_tail) = if let Some(tail) = after_colon.strip_prefix("in ") {
            (PortDirection::In, tail)
        } else if let Some(tail) = after_colon.strip_prefix("out ") {
            (PortDirection::Out, tail)
        } else {
            continue;
        };
        let ty_tail = ty_tail.split(';').next().unwrap_or(ty_tail).trim();
        let ty_str = leading_arch_type_str(ty_tail);
        let ty_str = outer_element_type_str(&ty_str, "pipe_reg").unwrap_or(ty_str);
        let Ok(ty) = crate::parser::parse_type_expr_fragment(&ty_str) else {
            continue;
        };
        let resolved_ty = resolve_arch_port_type(&ty, &type_params).unwrap_or(&ty);
        // Native ARCH Vec ports are unpacked C++ arrays. Keep the element
        // width/type and array length separate, as for SV unpacked ports.
        if let TypeExpr::Builtin {
            name: BuiltinTy::Vec,
            args,
            ..
        } = resolved_ty
        {
            // An array inside a packed port group needs a multidimensional
            // shape, which the current interface catalog cannot represent.
            if matches!(group, Some((_, GroupShape::Packed(_)))) {
                continue;
            }
            let [TypeArg::Type(element), TypeArg::Expr(count)] = args.as_slice() else {
                continue;
            };
            let Some(count) = eval_const_i64(count, &const_params)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|count| *count > 0)
            else {
                continue;
            };
            let element = resolve_arch_port_type(element, &type_params).unwrap_or(element);
            let Some(value_type) = concrete_port_ir_type_with_params(element, &const_params) else {
                continue;
            };
            let width = match value_type {
                crate::ir::IrType::Bool => 1,
                crate::ir::IrType::UInt(Some(width)) | crate::ir::IrType::SInt(Some(width)) => {
                    width
                }
                _ => unreachable!("concrete scalar element has a concrete width"),
            };
            out.push(DutInterfacePort::new_typed(
                name,
                direction,
                width,
                value_type,
                None,
                Some(count),
            ));
            continue;
        }
        if let Some(mut value_type) = concrete_port_ir_type_with_params(resolved_ty, &const_params)
        {
            let lane_value_type = value_type.clone();
            let lane_width = match &value_type {
                crate::ir::IrType::Bool => 1,
                crate::ir::IrType::UInt(Some(width)) | crate::ir::IrType::SInt(Some(width)) => {
                    *width
                }
                _ => unreachable!("concrete scalar port type has a concrete width"),
            };
            let (width, packed_lane_width) = match group.as_ref().map(|(_, shape)| *shape) {
                Some(GroupShape::Packed(Some(elements))) => {
                    let Some(width) = lane_width.checked_mul(elements) else {
                        continue;
                    };
                    value_type = match value_type {
                        crate::ir::IrType::SInt(_) => crate::ir::IrType::SInt(Some(width)),
                        _ => crate::ir::IrType::UInt(Some(width)),
                    };
                    (width, Some(lane_width))
                }
                Some(GroupShape::Packed(None)) => {
                    let value_type = match value_type {
                        crate::ir::IrType::SInt(_) => crate::ir::IrType::SInt(None),
                        _ => crate::ir::IrType::UInt(None),
                    };
                    out.push(
                        DutInterfacePort::new_unresolved_with_shape(
                            name,
                            direction,
                            value_type,
                            Some(lane_width),
                            None,
                        )
                        .with_packed_lane_type(lane_value_type),
                    );
                    continue;
                }
                _ => (lane_width, None),
            };
            let port = DutInterfacePort::new_typed(
                name,
                direction,
                width,
                value_type,
                packed_lane_width,
                None,
            );
            out.push(match packed_lane_width {
                Some(_) => port.with_packed_lane_type(lane_value_type),
                None => port,
            });
        } else if scalar_port_type_has_unresolved_width(resolved_ty) {
            let value_type = if matches!(
                resolved_ty,
                TypeExpr::Builtin {
                    name: BuiltinTy::SInt | BuiltinTy::SIntCap,
                    ..
                }
            ) {
                crate::ir::IrType::SInt(None)
            } else {
                crate::ir::IrType::UInt(None)
            };
            out.push(DutInterfacePort::new_unresolved_with_shape(
                name, direction, value_type, None, None,
            ));
        }
    }
    found_top
}

fn resolve_arch_port_type<'a>(
    ty: &'a TypeExpr,
    aliases: &'a std::collections::HashMap<String, TypeExpr>,
) -> Option<&'a TypeExpr> {
    let mut current = ty;
    for _ in 0..aliases.len().saturating_add(1) {
        let TypeExpr::Named { name, generics, .. } = current else {
            return Some(current);
        };
        if !generics.is_empty() || name.segments.len() != 1 {
            return None;
        }
        current = aliases.get(&name.segments[0].name)?;
    }
    None
}

/// Scan the `--sv` sources for the `--top` module and return its full
/// port table (name, direction, total packed width). The scanner accepts ANSI
/// headers on one or multiple lines and foldable widths. Returns `None` when
/// the top module isn't found in any source. Ports whose width can't be
/// folded, or with unpacked (post-name) dimensions, are skipped — a TB
/// referencing a skipped port fails C++ compilation with a missing-member error.
pub fn cosim_ports_from_sv(sv_sources: &[std::path::PathBuf], top: &str) -> Option<Vec<CosimPort>> {
    cosim_ports_from_sv_with_parameter_overrides(
        sv_sources,
        top,
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
    )
}

fn cosim_ports_from_sv_with_parameter_overrides(
    sv_sources: &[std::path::PathBuf],
    top: &str,
    override_values: &std::collections::HashMap<String, i64>,
    overridden_names: &std::collections::HashSet<String>,
) -> Option<Vec<CosimPort>> {
    let sources = sv_sources
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect::<Vec<_>>();
    let packages = collect_sv_package_symbols(&sources);
    for src in &sources {
        if let Some(ports) = scan_sv_module_ports_with_package_symbols(
            src,
            top,
            override_values,
            overridden_names,
            &packages,
        ) {
            return Some(ports);
        }
    }
    None
}

fn collect_sv_typedefs(
    lines: &[String],
    params: &std::collections::HashMap<String, i64>,
    typedefs: &mut std::collections::HashMap<String, SvResolvedType>,
) {
    let mut struct_accum: Option<(u64, bool)> = None;
    for line in lines {
        let text = line.trim();
        if let Some((acc, signed)) = struct_accum {
            if let Some(rest) = text.strip_prefix('}') {
                let name: String = rest
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() && acc > 0 {
                    typedefs.insert(
                        name,
                        SvResolvedType {
                            width: acc.min(u32::MAX as u64) as u32,
                            signed,
                        },
                    );
                }
                struct_accum = None;
            } else if let Some((_, ty)) = sv_typedef_type(text, params, typedefs) {
                struct_accum = Some((acc + ty.width as u64, signed));
            } else {
                struct_accum = Some((0, signed));
            }
            continue;
        }
        if text.starts_with("typedef struct packed") {
            struct_accum = Some((0, text.starts_with("typedef struct packed signed")));
            continue;
        }
        if let Some(rest) = text.strip_prefix("typedef ") {
            if let Some((name, ty)) = sv_typedef_type(rest, params, typedefs) {
                typedefs.insert(name, ty);
            }
        }
    }
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
///   - normalizes the ANSI header (including same-line declarations) into
///     complete direction-bearing declarations, with a whole-body fallback
///     for non-ANSI headers, and reports every skipped
///     `input`/`output` line on stderr — a silently absent port is the
///     failure mode this scanner must never have.
#[cfg(test)]
pub(crate) fn scan_sv_module_ports(src: &str, top: &str) -> Option<Vec<CosimPort>> {
    scan_sv_module_ports_with_parameter_overrides(
        src,
        top,
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
    )
}

#[cfg(test)]
fn scan_sv_module_ports_with_parameter_overrides(
    src: &str,
    top: &str,
    override_values: &std::collections::HashMap<String, i64>,
    overridden_names: &std::collections::HashSet<String>,
) -> Option<Vec<CosimPort>> {
    scan_sv_module_ports_with_package_symbols(
        src,
        top,
        override_values,
        overridden_names,
        &std::collections::HashMap::new(),
    )
}

fn scan_sv_module_ports_with_package_symbols(
    src: &str,
    top: &str,
    override_values: &std::collections::HashMap<String, i64>,
    overridden_names: &std::collections::HashSet<String>,
    packages: &std::collections::HashMap<String, SvPackageSymbols>,
) -> Option<Vec<CosimPort>> {
    let raw_lines: Vec<&str> = src.lines().collect();
    let lines: Vec<String> = raw_lines
        .iter()
        .map(|l| strip_sv_line_comments(l))
        .collect();
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

    // File-scope typedefs preceding the top are visible in its parameter list.
    // Typedefs inside unrelated modules are not.
    let mut file_scope = Vec::new();
    let mut in_other_module = false;
    for line in &lines[..start] {
        let text = line.trim_start();
        if text.starts_with("module ") {
            in_other_module = true;
            continue;
        }
        if text.starts_with("endmodule") {
            in_other_module = false;
            continue;
        }
        if !in_other_module {
            file_scope.push(line.clone());
        }
    }

    // Pass 1a (top module extent only): parameters. `parameter type`
    // goes to the typedef table; everything else through the const
    // evaluator, one or more declarators per line.
    let mut params = std::collections::HashMap::new();
    let mut typedefs: std::collections::HashMap<String, SvResolvedType> =
        std::collections::HashMap::new();
    for (package_name, symbols) in packages {
        for (name, value) in &symbols.params {
            params.insert(format!("{package_name}::{name}"), *value);
        }
        for (name, value_type) in &symbols.typedefs {
            typedefs.insert(format!("{package_name}::{name}"), *value_type);
        }
    }
    for package_name in imported_sv_packages(&lines[..start], body) {
        let Some(symbols) = packages.get(&package_name) else {
            continue;
        };
        params.extend(
            symbols
                .params
                .iter()
                .map(|(name, value)| (name.clone(), *value)),
        );
        typedefs.extend(
            symbols
                .typedefs
                .iter()
                .map(|(name, value_type)| (name.clone(), *value_type)),
        );
    }
    params.extend(
        override_values
            .iter()
            .map(|(name, value)| (name.clone(), *value)),
    );
    collect_sv_typedefs(&file_scope, &params, &mut typedefs);
    let header_parameters = ansi_header_parameter_declarations(&body[..header_end]);
    for t in header_parameters
        .iter()
        .map(String::as_str)
        .chain(body.iter().map(|line| line.trim()))
    {
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
                    if let Some((_, ty)) = sv_typedef_type(
                        &format!("{} {name};", rest[eq + 1..].trim()),
                        &params,
                        &typedefs,
                    ) {
                        typedefs.insert(name.to_string(), ty);
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
                    let v = eval_sv_const_expr(val, &params)
                        .or_else(|| eval_sv_const_expr(val.trim_end_matches(')'), &params));
                    if kw.starts_with("localparam") {
                        // A `localparam` cannot be overridden, and `params` starts
                        // from the overrides: take its own value, or forget a
                        // same-named override when the expression does not evaluate.
                        match v {
                            Some(v) => {
                                params.insert(name.to_string(), v);
                            }
                            None => {
                                params.remove(name);
                            }
                        }
                    } else if let Some(v) = v.filter(|_| !overridden_names.contains(name)) {
                        params.insert(name.to_string(), v);
                    }
                }
            }
            break;
        }
    }

    // Pass 1b: declarations in the selected module can type non-ANSI ports.
    collect_sv_typedefs(body, &params, &mut typedefs);

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
            let mut is_signed = false;
            let mut explicit_signed = None;
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
                        if r.starts_with(|c: char| c.is_whitespace() || c == '[') || r.is_empty() {
                            if let Some(w) = w {
                                base_width = Some(base_width.map_or(w, |b| b.max(w)));
                            }
                            if q == "signed"
                                || matches!(q, "integer" | "int" | "longint" | "shortint" | "byte")
                            {
                                if q == "signed" {
                                    explicit_signed = Some(true);
                                    is_signed = true;
                                } else if explicit_signed.is_none() {
                                    is_signed = true;
                                }
                            } else if q == "unsigned" {
                                explicit_signed = Some(false);
                                is_signed = false;
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
                let token_len = sv_scoped_identifier_len(rest);
                let token = &rest[..token_len];
                let after_tok = rest[token_len..].trim_start();
                let mut peek = after_tok;
                while peek.starts_with('[') {
                    match peek.find(']') {
                        Some(c) => peek = peek[c + 1..].trim_start(),
                        None => break,
                    }
                }
                if peek.starts_with(|c: char| c.is_alphabetic() || c == '_') {
                    match typedefs.get(token) {
                        Some(ty) => {
                            type_width = Some(ty.width);
                            if explicit_signed.is_none() {
                                is_signed = ty.signed;
                            }
                            rest = after_tok;
                        }
                        None => {
                            skipped.push(format!("unresolved port type `{token}`: `{t}`"));
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
            let mut packed_dims = Vec::new();
            let mut cur = rest;
            let mut foldable = true;
            while cur.starts_with('[') {
                let Some(close) = cur.find(']') else {
                    foldable = false;
                    break;
                };
                match sv_const_range_width(&cur[1..close], &params) {
                    Some(w) => {
                        packed_dims.push(w);
                        dims_product = dims_product.saturating_mul(w as u64);
                    }
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
                let nd = name_decl.trim().trim_end_matches([';', ')']).trim();
                if nd.is_empty() {
                    continue;
                }
                let name: String = nd
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() || !nd.starts_with(|c: char| c.is_alphabetic() || c == '_') {
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
                    skipped.push(format!(
                        "unparsed port declarator tail `{after_name}`: `{t}`"
                    ));
                    continue;
                }
                out.push(CosimPort {
                    name,
                    width_bits: width.min(u32::MAX as u64) as u32,
                    is_input,
                    is_signed,
                    packed_lane_width: (!packed_dims.is_empty()).then(|| {
                        packed_dims
                            .iter()
                            .skip(1)
                            .fold(base, |width, dim| width.saturating_mul(u64::from(*dim)))
                            .min(u64::from(u32::MAX)) as u32
                    }),
                    unpacked_elems,
                });
            }
        }
        (out, skipped)
    };

    let header_declarations = ansi_header_port_declarations(&body[..header_end]);
    let (mut out, mut skipped) = scan_ports(&header_declarations);
    if out.is_empty() {
        (out, skipped) = scan_ports(body);
    }
    // A silently absent port is this scanner's worst failure mode:
    // surface every skip loudly on stderr (the TB may still build if it
    // never touches the port; if it does, the C++ error names it).
    for w in &skipped {
        eprintln!("harc: module `{top}`: skipped port — {w}");
    }
    Some(out)
}

fn ansi_header_parameter_declarations(header: &[String]) -> Vec<String> {
    let text = header.join("\n");
    let Some(module) = text.find("module ") else {
        return Vec::new();
    };
    let mut rest = text[module + "module ".len()..].trim_start();
    let name_len = rest
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .map(char::len_utf8)
        .sum::<usize>();
    rest = rest[name_len..].trim_start();
    let Some(after_hash) = rest.strip_prefix('#') else {
        return Vec::new();
    };
    let Some((parameters, _)) = split_balanced_parentheses(after_hash.trim_start()) else {
        return Vec::new();
    };

    let prefixes = [
        "parameter type ",
        "localparam type ",
        "parameter int ",
        "localparam int ",
        "parameter ",
        "localparam ",
    ];
    let mut inherited_prefix = None;
    split_sv_top_level_commas(parameters)
        .into_iter()
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            if let Some(prefix) = prefixes.iter().find(|prefix| part.starts_with(**prefix)) {
                inherited_prefix = Some(*prefix);
                Some(part.to_string())
            } else {
                inherited_prefix.map(|prefix| format!("{prefix}{part}"))
            }
        })
        .collect()
}

fn ansi_header_port_declarations(header: &[String]) -> Vec<String> {
    let text = header.join("\n");
    let Some(module) = text.find("module ") else {
        return header.to_vec();
    };
    let mut rest = text[module + "module ".len()..].trim_start();
    let name_len = rest
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .map(char::len_utf8)
        .sum::<usize>();
    rest = rest[name_len..].trim_start();
    if let Some(after_hash) = rest.strip_prefix('#') {
        let Some((_, after_params)) = split_balanced_parentheses(after_hash.trim_start()) else {
            return header.to_vec();
        };
        rest = after_params.trim_start();
    }
    let Some((ports, _)) = split_balanced_parentheses(rest) else {
        return header.to_vec();
    };

    let mut declarations = Vec::new();
    let mut current = String::new();
    for part in split_sv_top_level_commas(ports) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let starts_declaration = ["input ", "output ", "inout "]
            .iter()
            .any(|prefix| part.starts_with(prefix));
        if starts_declaration && !current.is_empty() {
            declarations.push(std::mem::take(&mut current));
        } else if !current.is_empty() {
            current.push_str(", ");
        }
        current.push_str(part);
    }
    if !current.is_empty() {
        declarations.push(current);
    }
    declarations
}

fn split_balanced_parentheses(input: &str) -> Option<(&str, &str)> {
    let input = input.strip_prefix('(')?;
    let mut depth = 1u32;
    for (index, character) in input.char_indices() {
        match character {
            '(' => depth = depth.checked_add(1)?,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((&input[..index], &input[index + character.len_utf8()..]));
                }
            }
            _ => {}
        }
    }
    None
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SvResolvedType {
    width: u32,
    signed: bool,
}

#[derive(Clone, Debug, Default)]
struct SvPackageSymbols {
    params: std::collections::HashMap<String, i64>,
    typedefs: std::collections::HashMap<String, SvResolvedType>,
}

fn collect_sv_package_symbols(
    sources: &[String],
) -> std::collections::HashMap<String, SvPackageSymbols> {
    let mut packages = std::collections::HashMap::new();
    for source in sources {
        let lines = source
            .lines()
            .map(strip_sv_line_comments)
            .collect::<Vec<_>>();
        let mut index = 0;
        while index < lines.len() {
            let text = lines[index].trim();
            let Some(mut package_tail) = text.strip_prefix("package ") else {
                index += 1;
                continue;
            };
            package_tail = package_tail
                .strip_prefix("automatic ")
                .unwrap_or(package_tail)
                .trim_start();
            let package_name = package_tail
                .chars()
                .take_while(|character| character.is_alphanumeric() || *character == '_')
                .collect::<String>();
            if package_name.is_empty() {
                index += 1;
                continue;
            }
            let end = lines[index + 1..]
                .iter()
                .position(|line| line.trim_start().starts_with("endpackage"))
                .map(|offset| index + 1 + offset)
                .unwrap_or(lines.len());
            let body = &lines[index + 1..end];
            let mut symbols = SvPackageSymbols::default();
            loop {
                let before = symbols.params.clone();
                collect_sv_constant_declarations(body, &mut symbols.params);
                if symbols.params == before {
                    break;
                }
            }
            collect_sv_typedefs(body, &symbols.params, &mut symbols.typedefs);
            packages.insert(package_name, symbols);
            index = end.saturating_add(1);
        }
    }
    packages
}

fn collect_sv_constant_declarations(
    lines: &[String],
    params: &mut std::collections::HashMap<String, i64>,
) {
    for line in lines {
        let text = line.trim();
        let declaration = [
            "parameter int ",
            "localparam int ",
            "parameter integer ",
            "localparam integer ",
            "parameter ",
            "localparam ",
        ]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix));
        let Some(declaration) = declaration else {
            continue;
        };
        for declarator in split_sv_top_level_commas(declaration) {
            let Some(equal) = declarator.find('=') else {
                continue;
            };
            let name = declarator[..equal]
                .trim()
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .trim_end_matches(|character: char| {
                    !character.is_alphanumeric() && character != '_'
                });
            if name.is_empty() {
                continue;
            }
            let value = declarator[equal + 1..]
                .trim()
                .trim_end_matches([';', ','])
                .trim();
            if let Some(value) = eval_sv_const_expr(value, params) {
                params.insert(name.to_string(), value);
            }
        }
    }
}

fn imported_sv_packages(file_scope: &[String], body: &[String]) -> Vec<String> {
    let mut packages = std::collections::BTreeSet::new();
    for line in file_scope.iter().chain(body) {
        let Some(imports) = line.trim().strip_prefix("import ") else {
            continue;
        };
        for import in split_sv_top_level_commas(imports) {
            let import = import.trim().trim_end_matches(';').trim();
            if let Some(package) = import.strip_suffix("::*") {
                if !package.is_empty() {
                    packages.insert(package.to_string());
                }
            }
        }
    }
    packages.into_iter().collect()
}

fn sv_scoped_identifier_len(text: &str) -> usize {
    text.chars()
        .take_while(|character| {
            character.is_alphanumeric() || *character == '_' || *character == ':'
        })
        .map(char::len_utf8)
        .sum()
}

/// `logic [hi:lo]... Name` (the tail of a typedef or a `parameter type`
/// value) → `(Name, packed scalar metadata)`.
fn sv_typedef_type(
    decl: &str,
    params: &std::collections::HashMap<String, i64>,
    aliases: &std::collections::HashMap<String, SvResolvedType>,
) -> Option<(String, SvResolvedType)> {
    let mut rest = decl.trim();
    let mut saw_base = false;
    let mut signed = false;
    let mut explicit_signed = None;
    let mut width: u64 = 1;
    loop {
        let mut stripped = false;
        for (q, builtin) in [
            ("logic", Some((1u32, false))),
            ("wire", Some((1, false))),
            ("reg", Some((1, false))),
            ("bit", Some((1, false))),
            ("integer", Some((32, true))),
            ("int", Some((32, true))),
            ("longint", Some((64, true))),
            ("shortint", Some((16, true))),
            ("byte", Some((8, true))),
            ("signed", None),
            ("unsigned", None),
        ] {
            if let Some(r) = rest.strip_prefix(q) {
                if r.starts_with(|c: char| c.is_whitespace() || c == '[') || r.starts_with(';') {
                    rest = r.trim_start();
                    saw_base = true;
                    stripped = true;
                    if q == "signed" {
                        explicit_signed = Some(true);
                        signed = true;
                    } else if q == "unsigned" {
                        explicit_signed = Some(false);
                        signed = false;
                    } else if let Some((builtin_width, builtin_signed)) = builtin {
                        width = builtin_width.into();
                        if explicit_signed.is_none() {
                            signed = builtin_signed;
                        }
                    }
                    break;
                }
            }
        }
        if !stripped {
            break;
        }
    }
    if !saw_base {
        let alias_len = sv_scoped_identifier_len(rest);
        let alias = &rest[..alias_len];
        let ty = aliases.get(alias)?;
        width = ty.width.into();
        signed = ty.signed;
        rest = rest[alias_len..].trim_start();
    }
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
    Some((
        name,
        SvResolvedType {
            width: width.min(u32::MAX as u64) as u32,
            signed,
        },
    ))
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
    Some(
        (a - b)
            .unsigned_abs()
            .saturating_add(1)
            .min(u32::MAX as u64) as u32,
    )
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
                        if c.is_ascii_alphanumeric() || *c == b'_' || *c == b':' {
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
pub(crate) fn collect_port_overrides_from_src(
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
    outer_element_type_str(ty, "Vec")
}

fn type_simple_name(t: Option<&TypeExpr>) -> Option<&str> {
    match t? {
        TypeExpr::Named { name, .. } => name.segments.last().map(|segment| segment.name.as_str()),
        _ => None,
    }
}

fn type_arg_width(args: &[TypeArg]) -> Option<u32> {
    let TypeArg::Expr(expr) = args.first()? else {
        return None;
    };
    let ExprKind::Int(value) = &*expr.kind else {
        return None;
    };
    value.replace('_', "").parse().ok()
}

pub(crate) fn outer_element_type_str(ty: &str, wrapper: &str) -> Option<String> {
    let inner = ty.strip_prefix(wrapper)?.trim_start();
    let inner = inner.strip_prefix('<')?;
    let inner = inner.strip_suffix('>')?;
    let mut depth = 0i32;
    let mut last_comma = None;
    for (index, character) in inner.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => last_comma = Some(index),
            _ => {}
        }
    }
    Some(inner[..last_comma?].trim().to_string())
}

pub(crate) fn is_simple_ident(value: &str) -> bool {
    let mut characters = value.chars();
    match characters.next() {
        Some(character) if character.is_ascii_alphabetic() || character == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

pub(crate) fn eval_const_i64(
    expr: &Expr,
    env: &std::collections::HashMap<String, i64>,
) -> Option<i64> {
    match &*expr.kind {
        ExprKind::Paren(inner) => eval_const_i64(inner, env),
        ExprKind::Ident(ident) => env.get(&ident.name).copied(),
        ExprKind::Int(value) => {
            let stripped = value.replace('_', "");
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
        ExprKind::Bool(value) => Some(if *value { 1 } else { 0 }),
        ExprKind::Unary { op, expr } => {
            let value = eval_const_i64(expr, env)?;
            match op {
                UnaryOp::Neg => Some(value.wrapping_neg()),
                UnaryOp::Not | UnaryOp::NotKw => Some(if value == 0 { 1 } else { 0 }),
                UnaryOp::BitNot => Some(!value),
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs = eval_const_i64(lhs, env)?;
            let rhs = eval_const_i64(rhs, env)?;
            let bool_i = |value: bool| Some(if value { 1 } else { 0 });
            match op {
                BinaryOp::Add | BinaryOp::AddWrap => Some(lhs.wrapping_add(rhs)),
                BinaryOp::Sub | BinaryOp::SubWrap => Some(lhs.wrapping_sub(rhs)),
                BinaryOp::Mul | BinaryOp::MulWrap => Some(lhs.wrapping_mul(rhs)),
                BinaryOp::Div => (rhs != 0).then(|| lhs / rhs),
                BinaryOp::Mod => (rhs != 0).then(|| lhs % rhs),
                BinaryOp::Shl => Some(lhs.wrapping_shl(rhs as u32)),
                BinaryOp::Shr => Some(lhs.wrapping_shr(rhs as u32)),
                BinaryOp::BitAnd => Some(lhs & rhs),
                BinaryOp::BitOr => Some(lhs | rhs),
                BinaryOp::BitXor => Some(lhs ^ rhs),
                BinaryOp::Eq => bool_i(lhs == rhs),
                BinaryOp::Ne => bool_i(lhs != rhs),
                BinaryOp::Lt => bool_i(lhs < rhs),
                BinaryOp::Le => bool_i(lhs <= rhs),
                BinaryOp::Gt => bool_i(lhs > rhs),
                BinaryOp::Ge => bool_i(lhs >= rhs),
                BinaryOp::AndAnd | BinaryOp::AndKw => bool_i(lhs != 0 && rhs != 0),
                BinaryOp::OrOr | BinaryOp::OrKw => bool_i(lhs != 0 || rhs != 0),
                _ => None,
            }
        }
        _ => None,
    }
}
