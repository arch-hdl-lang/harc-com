//! Generates the SystemVerilog `bind` stub for HARC `probe` declarations.
//!
//! When a test's `let dut : T` carries a probe block, this emitter
//! produces `__harc_probe_<T>.sv` containing:
//!
//!   1. A module `__harc_probe_<T>` with one `logic` per declared probe,
//!      each annotated `/* verilator public_flat_rd */` so Verilator
//!      keeps it through optimization and exposes it as a top-level
//!      flat-readable member.
//!   2. `assign <probe> = <path>;` lines using upward references — the
//!      bound stub sits inside every instance of `T`, so unqualified
//!      paths like `alu0.a` resolve in that scope.
//!   3. A `bind T __harc_probe_<T> harc_probes ();` directive.
//!
//! Bind instance name is `harc_probes` (no leading underscore) so the
//! Verilator-mangled accessor is the clean form
//! `<T>__DOT__harc_probes__DOT__<probe_name>`, accessed via
//! `dut->rootp-><mangled>`. The header
//! `V<T>___024root.h` must be included in the emitted TB.

use crate::ast::*;
use crate::ir::passes::dut_access::DutAccessPlan;
use crate::ir::ProbeScalarType;
use crate::lexer::Span;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

/// Canonical DUT/probe metadata for one emitted test suite. Component
/// declarations are lowered once for the whole suite, so their probe access
/// classification and the generated bind stub must be derived from the same
/// catalog rather than from an arbitrary individual test.
#[derive(Debug, Clone)]
pub struct DutProbeCatalog {
    pub dut_type: Option<String>,
    /// Union used by the single suite-wide bind stub.
    pub probes: Vec<Probe>,
    /// Exact source-level scalar kind for every probe in `probes`, keyed by
    /// probe name. Width-only lowering is insufficient for signed reads and
    /// loses the intentional distinction between `bits<N>` and `uint<N>`.
    pub probe_types: HashMap<String, ProbeScalarType>,
    /// Probes declared identically by every test. Only this intersection is
    /// visible while lowering shared component bodies: otherwise one test's
    /// declaration could silently grant another test access it never opted
    /// into merely because both tests share a generated binary.
    pub shared_component_probes: Vec<Probe>,
    /// Names present in the stub union but absent from at least one test.
    /// Shared component/transactor bodies must reject these names instead of
    /// silently reclassifying them as ordinary DUT ports.
    pub partial_component_probe_names: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct DutProbeCatalogError {
    message: String,
    source_id: SourceId,
    span: Span,
    related: Option<SourceSite>,
    unsupported_type: bool,
}

impl DutProbeCatalogError {
    pub fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn span(&self) -> Span {
        self.span
    }

    pub fn is_unsupported_type(&self) -> bool {
        self.unsupported_type
    }

    pub fn related_site(&self) -> Option<SourceSite> {
        self.related
    }
}

impl std::fmt::Display for DutProbeCatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DutProbeCatalogError {}

pub fn probe_scalar_type(ty: &TypeExpr) -> Result<ProbeScalarType, String> {
    let TypeExpr::Builtin { name, args, .. } = ty else {
        if matches!(
            ty,
            TypeExpr::Named { name, generics, .. }
                if generics.is_empty()
                    && name.segments.len() == 1
                    && name.segments[0].name == "bit"
        ) {
            return Ok(ProbeScalarType::Bit);
        }
        return Err("use uint<N>, sint<N>, bits<N>, bit, or bool".to_string());
    };
    let width = match name {
        BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => 1,
        BuiltinTy::UInt
        | BuiltinTy::SInt
        | BuiltinTy::Bits
        | BuiltinTy::UIntCap
        | BuiltinTy::SIntCap => args
            .first()
            .and_then(int_arg)
            .ok_or_else(|| format!("probe type {name:?} needs a single integer width argument"))?,
        _ => return Err("use uint<N>, sint<N>, bits<N>, bit, or bool".to_string()),
    };
    if width == 0 {
        return Err("probe scalar widths must be greater than zero".to_string());
    }
    let ty = match name {
        BuiltinTy::Bit => ProbeScalarType::Bit,
        BuiltinTy::Bool | BuiltinTy::BoolLower => ProbeScalarType::Bool,
        BuiltinTy::UInt | BuiltinTy::UIntCap => ProbeScalarType::UInt(width),
        BuiltinTy::SInt | BuiltinTy::SIntCap => ProbeScalarType::SInt(width),
        BuiltinTy::Bits => ProbeScalarType::Bits(width),
        _ => return Err("use uint<N>, sint<N>, bits<N>, bit, or bool".to_string()),
    };
    Ok(ty)
}

fn probe_type_label(ty: ProbeScalarType) -> String {
    match ty {
        ProbeScalarType::Bit => "bit".to_string(),
        ProbeScalarType::Bool => "bool".to_string(),
        ProbeScalarType::UInt(width) => format!("uint<{width}>"),
        ProbeScalarType::SInt(width) => format!("sint<{width}>"),
        ProbeScalarType::Bits(width) => format!("bits<{width}>"),
    }
}

/// Collect and validate the probes declared by every desugared test in a
/// suite. Identical declarations are deduplicated in first-seen order;
/// conflicting declarations cannot safely share one generated bind module
/// and are rejected before either lowering or emission proceeds.
pub fn collect_suite_probes(file: &SourceFile) -> Result<DutProbeCatalog, DutProbeCatalogError> {
    let mut dut_type: Option<(String, SourceSite, String)> = None;
    let mut probes = Vec::new();
    let mut probe_types = HashMap::new();
    let mut by_name: HashMap<String, (ProbeScalarType, String, bool, SourceSite, String)> =
        HashMap::new();
    let mut generated_symbols: HashMap<String, (String, SourceSite)> = HashMap::new();
    let mut force_paths: HashMap<String, (String, SourceSite)> = HashMap::new();
    let mut probe_names_by_test = Vec::new();

    for (item_index, item) in file.items.iter().enumerate() {
        let Item::Test(test) = item else { continue };
        let test_source = file.item_source(item_index);
        let mut names_in_test: HashMap<String, SourceSite> = HashMap::new();
        for (test_item_index, test_item) in test.items.iter().enumerate() {
            let TestItem::Let(let_stmt) = test_item else {
                continue;
            };
            if let_stmt.name.name != "dut" {
                continue;
            }
            let item_source = test.item_source(test_item_index);
            let source_id = if item_source.is_known() {
                item_source
            } else {
                test_source
            };
            if !let_stmt.probes.is_empty() {
                let current_dut = match let_stmt.ty.as_ref() {
                    Some(TypeExpr::Named { name, .. }) => {
                        name.segments.last().map(|segment| segment.name.clone())
                    }
                    _ => None,
                }
                .ok_or_else(|| DutProbeCatalogError {
                    message: "`let dut : <Type>` must use a simple named type".to_string(),
                    source_id,
                    span: let_stmt.span,
                    related: None,
                    unsupported_type: false,
                })?;
                let current_site = SourceSite::new(source_id, let_stmt.span);
                if let Some((previous, previous_site, previous_test)) = dut_type.as_ref() {
                    if previous != &current_dut {
                        return Err(DutProbeCatalogError {
                            message: format!(
                                "probe declarations in one suite cannot target different DUT types; test `{}` uses `{}` at {}, but test `{previous_test}` first used `{previous}` at {}",
                                test.name.name,
                                current_dut,
                                source_site_label(file, current_site),
                                source_site_label(file, *previous_site),
                            ),
                            source_id,
                            span: let_stmt.span,
                            related: Some(*previous_site),
                            unsupported_type: false,
                        });
                    }
                } else {
                    dut_type = Some((current_dut, current_site, test.name.name.clone()));
                }
            }

            for probe in &let_stmt.probes {
                let site = SourceSite::new(source_id, probe.span);
                if let Some(previous_site) = names_in_test.insert(probe.name.name.clone(), site) {
                    return Err(DutProbeCatalogError {
                        message: format!(
                            "duplicate probe `{}` on `let dut` in test `{}`: first declared at {}, repeated at {}",
                            probe.name.name,
                            test.name.name,
                            source_site_label(file, previous_site),
                            source_site_label(file, site),
                        ),
                        source_id,
                        span: probe.span,
                        related: Some(previous_site),
                        unsupported_type: false,
                    });
                }
                let ty = probe_scalar_type(&probe.ty).map_err(|detail| DutProbeCatalogError {
                    message: format!(
                        "probe `{}` has an unsupported scalar type: {detail}",
                        probe.name.name
                    ),
                    source_id,
                    span: probe.span,
                    related: None,
                    unsupported_type: true,
                })?;
                if let Some((
                    previous_ty,
                    previous_path,
                    previous_force,
                    previous_site,
                    previous_test,
                )) = by_name.get(&probe.name.name)
                {
                    if *previous_ty != ty
                        || previous_path != &probe.path
                        || *previous_force != probe.force
                    {
                        return Err(DutProbeCatalogError {
                            message: format!(
                                "conflicting declarations for suite probe `{}`: test `{previous_test}` first declared `{}` at `{}` ({}) at {}; test `{}` declares `{}` at `{}` ({}) at {}",
                                probe.name.name,
                                probe_type_label(*previous_ty),
                                previous_path,
                                if *previous_force { "force-capable" } else { "read-only" },
                                source_site_label(file, *previous_site),
                                test.name.name,
                                probe_type_label(ty),
                                probe.path,
                                if probe.force { "force-capable" } else { "read-only" },
                                source_site_label(file, site),
                            ),
                            source_id,
                            span: probe.span,
                            related: Some(*previous_site),
                            unsupported_type: false,
                        });
                    }
                    continue;
                }

                let mut symbols = vec![probe.name.name.clone()];
                if probe.force {
                    symbols.push(format!("{}_drv", probe.name.name));
                    symbols.push(format!("{}_en", probe.name.name));
                }
                for symbol in symbols {
                    if let Some((owner, previous_site)) = generated_symbols.get(&symbol) {
                        return Err(DutProbeCatalogError {
                            message: format!(
                                "probe `{}` at {} collides with generated bind-stub signal `{symbol}` owned by probe `{owner}` declared at {}",
                                probe.name.name,
                                source_site_label(file, site),
                                source_site_label(file, *previous_site),
                            ),
                            source_id,
                            span: probe.span,
                            related: Some(*previous_site),
                            unsupported_type: false,
                        });
                    }
                    generated_symbols.insert(symbol, (probe.name.name.clone(), site));
                }
                if probe.force {
                    if let Some((previous_path, (owner, previous_site))) = force_paths
                        .iter()
                        .find(|(path, _)| probe_paths_overlap(path, &probe.path))
                    {
                        return Err(DutProbeCatalogError {
                            message: format!(
                                "force probe `{owner}` declared at {} and probe `{}` declared at {} target overlapping SV paths `{previous_path}` and `{}` and would emit competing force controllers",
                                source_site_label(file, *previous_site),
                                probe.name.name,
                                source_site_label(file, site),
                                probe.path,
                            ),
                            source_id,
                            span: probe.span,
                            related: Some(*previous_site),
                            unsupported_type: false,
                        });
                    }
                    force_paths.insert(probe.path.clone(), (probe.name.name.clone(), site));
                }
                by_name.insert(
                    probe.name.name.clone(),
                    (
                        ty,
                        probe.path.clone(),
                        probe.force,
                        site,
                        test.name.name.clone(),
                    ),
                );
                probe_types.insert(probe.name.name.clone(), ty);
                probes.push(probe.clone());
            }
        }
        probe_names_by_test.push(names_in_test.into_keys().collect::<HashSet<_>>());
    }

    let shared_component_probes: Vec<Probe> = probes
        .iter()
        .filter(|probe| {
            probe_names_by_test
                .iter()
                .all(|names| names.contains(&probe.name.name))
        })
        .cloned()
        .collect();
    let shared_names: HashSet<&str> = shared_component_probes
        .iter()
        .map(|probe| probe.name.name.as_str())
        .collect();
    let partial_component_probe_names = probes
        .iter()
        .filter(|probe| !shared_names.contains(probe.name.name.as_str()))
        .map(|probe| probe.name.name.clone())
        .collect();

    Ok(DutProbeCatalog {
        dut_type: dut_type.map(|(name, _, _)| name),
        probes,
        probe_types,
        shared_component_probes,
        partial_component_probe_names,
    })
}

fn source_site_label(file: &SourceFile, site: SourceSite) -> String {
    let Some(source) = file.source_for_id(site.source_id) else {
        return format!(
            "source#{} bytes {}..{}",
            site.source_id.0, site.span.start, site.span.end
        );
    };
    let offset = (site.span.start as usize).min(source.text.len());
    let prefix = &source.text[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.chars().count() + 1, |(_, tail)| {
            tail.chars().count() + 1
        });
    format!("{}:{line}:{column}", source.name)
}

fn probe_paths_overlap(a: &str, b: &str) -> bool {
    fn is_path_prefix(prefix: &str, path: &str) -> bool {
        path.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with('.') || suffix.starts_with('[')
        })
    }
    is_path_prefix(a, b) || is_path_prefix(b, a)
}

/// Render verified scalar metadata as the SystemVerilog declarator between
/// `logic` and the probe signal name. One-bit unsigned kinds are scalar.
fn sv_type_decl_from_scalar(scalar: ProbeScalarType) -> Result<String, String> {
    let signed = matches!(scalar, ProbeScalarType::SInt(_));
    let width = scalar.width();
    match (signed, width) {
        (false, 1) => Ok(String::new()),
        (true, 1) => Ok(" signed".to_string()),
        (false, width) => Ok(format!(" [{}:0]", width - 1)),
        (true, width) => Ok(format!(" signed [{}:0]", width - 1)),
    }
}

#[derive(Clone, Copy)]
struct StubProbe<'a> {
    name: &'a str,
    path: &'a str,
    ty: ProbeScalarType,
    force: bool,
}

fn int_arg(a: &TypeArg) -> Option<u32> {
    match a {
        TypeArg::Expr(e) | TypeArg::Named { value: e, .. } => {
            if let ExprKind::Int(s) = &*e.kind {
                s.replace('_', "").parse::<u32>().ok()
            } else {
                None
            }
        }
        TypeArg::Type(_) => None,
    }
}

/// Emit the full stub file contents.
///
/// Layout per probe:
/// - Read-only probe (`probe N : T at PATH`): one `logic` annotated
///   `public_flat_rd`, plus an `assign N = PATH;`.
/// - Force probe (`probe force N : T at PATH`): the same read-side
///   accessor, PLUS an `<N>_drv` + `<N>_en` pair annotated
///   `public_flat_rw`, plus an `always_comb` that procedurally
///   forces PATH from `<N>_drv` when `<N>_en` is high and releases
///   it otherwise. The HARC TB writes `<N>_drv` + `<N>_en` for
///   force; `release dut.<N>` lowers to just clearing `<N>_en`.
pub fn emit_stub(dut_type: &str, probes: &[Probe]) -> Result<String, String> {
    let probes = probes
        .iter()
        .map(|probe| {
            Ok(StubProbe {
                name: &probe.name.name,
                path: &probe.path,
                ty: probe_scalar_type(&probe.ty)?,
                force: probe.force,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    emit_stub_specs(dut_type, &probes)
}

/// Emit the probe stub from the immutable verified-IR DUT access plan.
///
/// Common-object publication uses this entry point so the C++ accessors and
/// the manifest-owned SystemVerilog stub are rendered from one resolved plan.
pub fn emit_stub_from_plan(plan: &DutAccessPlan) -> Result<String, String> {
    let probes = plan
        .probes()
        .iter()
        .map(|probe| StubProbe {
            name: probe.name(),
            path: probe.sv_path(),
            ty: probe.ty(),
            force: probe.force_capable(),
        })
        .collect::<Vec<_>>();
    emit_stub_specs(plan.dut_type(), &probes)
}

fn emit_stub_specs(dut_type: &str, probes: &[StubProbe<'_>]) -> Result<String, String> {
    let mut out = String::new();
    writeln!(out, "// Generated by harc — do not edit.").ok();
    writeln!(
        out,
        "// SystemVerilog `bind` stub for HARC probe declarations"
    )
    .ok();
    writeln!(
        out,
        "// on `let dut : {dut_type}`. See docs/probe-signals.md."
    )
    .ok();
    writeln!(out).ok();
    writeln!(out, "module __harc_probe_{dut_type};").ok();
    // Read-side declarations — one per probe, regardless of force.
    for p in probes {
        let decl = sv_type_decl_from_scalar(p.ty)?;
        writeln!(
            out,
            "    logic{decl} {name} /* verilator public_flat_rd */;",
            name = p.name
        )
        .ok();
    }
    // Force-only drive + enable pair.
    for p in probes {
        if !p.force {
            continue;
        }
        let decl = sv_type_decl_from_scalar(p.ty)?;
        writeln!(
            out,
            "    logic{decl} {name}_drv /* verilator public_flat_rw */;",
            name = p.name
        )
        .ok();
        writeln!(
            out,
            "    logic {name}_en  /* verilator public_flat_rw */;",
            name = p.name
        )
        .ok();
    }
    // Read-side continuous assigns. Targets are qualified with the
    // bound module's name (`<DutType>.<path>`) — Verilator's bind-scope
    // upward reference resolves unqualified sub-instance ports (e.g.
    // `alu0.a`) but trips on local `logic`s of the bound module
    // itself (e.g. `decode_rs1_val`). Qualifying with the module
    // name makes both cases resolve uniformly.
    for p in probes {
        writeln!(
            out,
            "    assign {name} = {dut_type}.{path};",
            name = p.name,
            path = p.path,
        )
        .ok();
    }
    // Force-side procedural drive. Targets are qualified with the
    // bound module's name (`<DutType>.<path>`) — Verilator needs the
    // fully-qualified reference inside the bind stub to resolve the
    // force/release target. Unqualified upward references work for
    // read-side `assign` but trip the optimizer for `force` because
    // the signal would otherwise be eliminated.
    for p in probes {
        if !p.force {
            continue;
        }
        writeln!(out, "    always_comb begin").ok();
        writeln!(out, "        if ({name}_en) begin", name = p.name).ok();
        writeln!(
            out,
            "            force {dut_type}.{path} = {name}_drv;",
            path = p.path,
            name = p.name,
        )
        .ok();
        writeln!(out, "        end else begin").ok();
        writeln!(out, "            release {dut_type}.{path};", path = p.path).ok();
        writeln!(out, "        end").ok();
        writeln!(out, "    end").ok();
    }
    writeln!(out, "endmodule").ok();
    writeln!(out).ok();
    writeln!(
        out,
        "bind {dut_type} __harc_probe_{dut_type} harc_probes ();"
    )
    .ok();
    Ok(out)
}

/// Bit width of a probe's declared HARC type (`uint<N>`/`sint<N>`/
/// `bits<N>` → N; `bit`/`bool` → 1). `None` for unsupported types —
/// `sv_type_decl` rejects those with a proper diagnostic at stub
/// emission, so co-sim callers can treat `None` as already-reported.
pub fn probe_width_bits(ty: &TypeExpr) -> Option<u32> {
    probe_scalar_type(ty).ok().map(ProbeScalarType::width)
}

/// Mangled C++ accessor for a probe signal under `dut->rootp->...`.
/// Matches the Verilator 5.x encoding for instance/signal paths:
/// `<TopModule>__DOT__harc_probes__DOT__<signal>`.
pub fn mangled_accessor(dut_type: &str, probe_name: &str) -> String {
    format!("{dut_type}__DOT__harc_probes__DOT__{probe_name}")
}
