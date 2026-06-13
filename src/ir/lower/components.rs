//! Composite-component lowering for the env/agent cluster's
//! flat-struct subset (docs/tbir-mvp.md §env/agent). Three source shapes
//! lower into one `ComponentSchema`, mirroring v1's uniform
//! `emit_component_struct` + `emit_component_method` treatment:
//!
//!   - a `scoreboard` carrying `hookable`/`function` **methods** (a
//!     data-only, method-less board stays on `ScoreboardSchema`);
//!   - a `transactor` used as a pure **analysis source** — `out event<T>`
//!     port(s) + methods that `emit` on them, and NO module-typed DUT
//!     field (the DUT-poking BFM form stays on `TransactorSchema`);
//!   - an `env` that composes the two as by-value sub-component fields
//!     and `connect`s a source event to a sink method.
//!
//! Classification (`is_component_*`) runs BEFORE the scoreboard /
//! transactor schema loops in `lower/mod.rs`, so each decl routes to
//! exactly one schema table.
//!
//! Out of subset, rejected (never mis-lowered): `agent` declarations and
//! `on <ev>` event handlers, watchdog/phase orchestration, generics,
//! `bound to` source transactors, and non-scalar event payloads. Those
//! gate on the agent/sequencer/event slices.

use super::{FuncBuilder, LowerCtx, LowerError, helpers, unsupported};
use crate::ast::{
    BuiltinTy, ComponentDecl, ComponentField, ComponentItem, ConnectEdge, Direction, ExprKind,
    HookableMethod, TransactorDecl, TypeArg, TypeExpr,
};
use crate::ir::{
    ComponentFieldKind, ComponentFieldSchema, ComponentId, ComponentKindTag,
    ComponentMethodSchema, ComponentSchema, ConnectEdgeSchema, FunctionId, FunctionKind, IrType,
    TbFunction, Terminator, TypedParam,
};
use std::collections::HashMap;

/// True when a `scoreboard` declaration carries a `hookable`/`function`
/// method, so it must lower as a component (per-instance state) rather
/// than the data-only `ScoreboardSchema`.
pub(crate) fn scoreboard_is_component(c: &ComponentDecl) -> bool {
    c.items
        .iter()
        .any(|it| matches!(it, ComponentItem::Hookable(_)))
}

/// True when a `transactor` declaration is a pure analysis source: it
/// has at least one `out event<T>` field and NO module-typed DUT field.
/// (A DUT-poking BFM has exactly one module-typed field and no event
/// port; a `bound to` target responder is excluded too.)
pub(crate) fn transactor_is_component(t: &TransactorDecl) -> bool {
    if t.bound_to.is_some() {
        return false;
    }
    let mut has_event = false;
    let mut has_module_field = false;
    for it in t.items.iter().chain(t.when_active.iter().flatten()) {
        if let ComponentItem::Field(f) = it {
            if is_event_field(f) {
                has_event = true;
            } else if matches!(&f.ty, TypeExpr::Named { .. }) {
                has_module_field = true;
            }
        }
    }
    has_event && !has_module_field
}

fn is_event_field(f: &ComponentField) -> bool {
    matches!(
        &f.ty,
        TypeExpr::Builtin {
            name: BuiltinTy::Event,
            ..
        }
    )
}

/// One unit of component-lowering work, in file order: the source decl
/// plus its assigned id. Built by the caller's classification pass.
pub(crate) enum CompSource<'a> {
    Env(&'a ComponentDecl),
    Scoreboard(&'a ComponentDecl),
    Transactor(&'a TransactorDecl),
}

/// Lower one component's STRUCTURE (fields + method signatures), without
/// method bodies. `ids` maps already-classified component names to their
/// ids so a `Sub` field / `connect` edge can resolve nested components.
/// Method `FunctionId`s are assigned from `next_fn` upward; bodies are
/// lowered in the second pass (`lower_component_bodies`).
pub(crate) fn lower_component_schema(
    src: &CompSource<'_>,
    ids: &HashMap<String, ComponentId>,
    next_fn: &mut u32,
) -> Result<ComponentSchema, LowerError> {
    let (name, kind, items, when_active): (
        &str,
        ComponentKindTag,
        &[ComponentItem],
        Option<&[ComponentItem]>,
    ) = match src {
        CompSource::Env(c) => (&c.name.name, ComponentKindTag::Env, &c.items, None),
        CompSource::Scoreboard(c) => {
            (&c.name.name, ComponentKindTag::Scoreboard, &c.items, None)
        }
        CompSource::Transactor(t) => (
            &t.name.name,
            ComponentKindTag::Transactor,
            &t.items,
            t.when_active.as_deref(),
        ),
    };
    if let CompSource::Transactor(t) = src {
        if !t.params.is_empty() {
            return Err(unsupported(
                &format!("generic parameters on analysis-source `{name}`"),
                "",
            ));
        }
    }
    if let CompSource::Env(c) | CompSource::Scoreboard(c) = src {
        if !c.params.is_empty() {
            return Err(unsupported(&format!("parameters on `{name}`"), ""));
        }
        if c.bound_to.is_some() {
            return Err(unsupported(&format!("a `bound to` clause on `{name}`"), ""));
        }
    }

    let mut fields: Vec<ComponentFieldSchema> = Vec::new();
    let mut methods: Vec<ComponentMethodSchema> = Vec::new();
    for it in items.iter().chain(when_active.into_iter().flatten()) {
        match it {
            ComponentItem::Field(f) => {
                let fk = lower_field(name, f, ids)?;
                if fields.iter().any(|x| x.name == f.name.name) {
                    return Err(LowerError::Invalid(format!(
                        "component `{name}` declares field `{}` more than once",
                        f.name.name
                    )));
                }
                fields.push(ComponentFieldSchema {
                    name: f.name.name.clone(),
                    kind: fk,
                });
            }
            ComponentItem::Hookable(h) => {
                let n_params = h.params.len();
                let has_ret = h.return_ty.is_some();
                let fid = FunctionId(*next_fn);
                *next_fn += 1;
                methods.push(ComponentMethodSchema {
                    name: h.name.name.clone(),
                    function: fid,
                    n_params,
                    has_ret,
                    hookable: h.is_hookable,
                });
            }
            // Connect blocks are resolved separately (env-binding stage).
            ComponentItem::Connect(_) => {}
            ComponentItem::Lifecycle(..) => {}
            ComponentItem::OnHandler(_) => {
                return Err(unsupported(
                    &format!("`on <ev>` event handlers on `{name}`"),
                    "event handlers gate on the agent slice",
                ));
            }
            ComponentItem::Watchdog(_) => {
                return Err(unsupported(
                    &format!("a `watchdog` on `{name}`"),
                    "watchdog/phase orchestration gates on the agent slice",
                ));
            }
            ComponentItem::TargetTlmThread(_) | ComponentItem::Apply(_) => {
                return Err(unsupported(
                    &format!("an unsupported item in component `{name}`"),
                    "",
                ));
            }
        }
    }

    Ok(ComponentSchema {
        name: name.to_string(),
        kind,
        fields,
        methods,
        // Connects resolved in a third pass once all schemas exist.
        connects: Vec::new(),
    })
}

fn lower_field(
    comp: &str,
    f: &ComponentField,
    ids: &HashMap<String, ComponentId>,
) -> Result<ComponentFieldKind, LowerError> {
    let fname = &f.name.name;
    if f.bound_to.is_some() {
        return Err(unsupported(
            &format!("a `bound to` clause on field `{comp}.{fname}`"),
            "",
        ));
    }
    match &f.ty {
        // `observed : out event<T>` analysis port.
        TypeExpr::Builtin {
            name: BuiltinTy::Event,
            args,
            ..
        } => {
            if f.direction != Some(Direction::Out) {
                return Err(unsupported(
                    &format!("a non-`out` event field `{comp}.{fname}`"),
                    "only `out event<T>` analysis ports are lowered",
                ));
            }
            let signed = match args.first() {
                Some(TypeArg::Type(ty)) => match scalar_ir_type(ty) {
                    Some(IrType::SInt(_)) => true,
                    Some(IrType::UInt(_)) | Some(IrType::Bool) => false,
                    _ => {
                        return Err(unsupported(
                            &format!("a non-scalar event payload on `{comp}.{fname}`"),
                            "only event<scalar ≤ 64 bits> ports are lowered",
                        ));
                    }
                },
                _ => false,
            };
            Ok(ComponentFieldKind::Event { signed })
        }
        // `expected : queue<T>` FIFO.
        TypeExpr::Builtin {
            name: BuiltinTy::Queue,
            args,
            ..
        } => {
            if f.direction.is_some() {
                return Err(unsupported(
                    &format!("a directional queue field `{comp}.{fname}`"),
                    "",
                ));
            }
            let signed = match args.first() {
                Some(TypeArg::Type(ty)) => match scalar_ir_type(ty) {
                    Some(IrType::SInt(_)) => true,
                    Some(IrType::UInt(_)) | Some(IrType::Bool) => false,
                    _ => {
                        return Err(unsupported(
                            &format!("a non-scalar queue element on `{comp}.{fname}`"),
                            "",
                        ));
                    }
                },
                _ => false,
            };
            Ok(ComponentFieldKind::Queue { signed })
        }
        // Scalar counter, or a nested sub-component (`source :
        // AnalysisSource passive` / `sb : AnalysisSb`).
        TypeExpr::Builtin { .. } => {
            if f.direction.is_some() {
                return Err(unsupported(
                    &format!("a directional scalar field `{comp}.{fname}`"),
                    "",
                ));
            }
            let ty = scalar_ir_type(&f.ty).ok_or_else(|| {
                unsupported(
                    &format!("scalar field `{comp}.{fname}` of an unsupported type"),
                    "only uint/sint/bits/bool fields up to 64 bits are lowered",
                )
            })?;
            let default = scalar_default(&f.default, comp, fname)?;
            Ok(ComponentFieldKind::Scalar { ty, default })
        }
        TypeExpr::Named { name, .. } => {
            let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
            let cid = *ids.get(simple).ok_or_else(|| {
                unsupported(
                    &format!("sub-component field `{comp}.{fname}` of type `{simple}`"),
                    "only env/method-scoreboard/analysis-source sub-components are lowered",
                )
            })?;
            Ok(ComponentFieldKind::Sub { component: cid })
        }
    }
}

/// Lower one component's method BODIES, returning the lowered
/// `TbFunction`s in declaration order (their ids match the schema's
/// `function` slots, assigned in pass 1). The body context resolves bare
/// field names self-relatively and `emit` against the component's event
/// fields.
pub(crate) fn lower_component_bodies(
    src: &CompSource<'_>,
    cid: ComponentId,
    schema: &ComponentSchema,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
) -> Result<Vec<TbFunction>, LowerError> {
    let (items, when_active): (&[ComponentItem], Option<&[ComponentItem]>) = match src {
        CompSource::Env(c) | CompSource::Scoreboard(c) => (&c.items, None),
        CompSource::Transactor(t) => (&t.items, t.when_active.as_deref()),
    };
    let mut funcs = Vec::new();
    let mut method_idx = 0usize;
    for it in items.iter().chain(when_active.into_iter().flatten()) {
        let ComponentItem::Hookable(h) = it else {
            continue;
        };
        let m = &schema.methods[method_idx];
        method_idx += 1;
        let f = lower_method_body(h, m.function, cid, ctx, helpers, constraint_sites)?;
        funcs.push(f);
    }
    Ok(funcs)
}

fn lower_method_body(
    h: &HookableMethod,
    fid: FunctionId,
    cid: ComponentId,
    ctx: &LowerCtx,
    helpers: &helpers::HelperRegistry<'_>,
    constraint_sites: &std::cell::RefCell<Vec<crate::ir::ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.self_component = Some(cid);
    // Bind parameters as the first locals (the run/check convention: a
    // LocalId < params.len() *is* the i-th param). Same shape as a
    // transactor method body.
    let mut params = Vec::with_capacity(h.params.len());
    for p in &h.params {
        let ty = helpers::ir_type_of(p.ty.as_ref());
        let local = b.declare(&p.name.name);
        b.set_local_type(local, ty.clone());
        if let Some(t) = &p.ty {
            if let Some(w) = scalar_width(t) {
                b.let_widths.insert(local, w);
            }
        }
        params.push(TypedParam {
            name: p.name.name.clone(),
            ty,
        });
    }
    if h.return_ty.is_some() {
        let ret = b.declare("__ret");
        b.helper_ret = Some(ret);
    }
    b.lower_block_stmts(&h.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let mut f = b.finish(
        fid,
        format!("comp_method_{}", fid.0),
        FunctionKind::ComponentMethod { component: cid },
        None,
    )?;
    f.params = params;
    Ok(f)
}

/// Resolve an env's `connect` block into `ConnectEdgeSchema`s. The env
/// is `env_schema` (id `env_cid`); the test reaches it through the
/// test-field local. `components` is the program snapshot for resolving
/// sink sub-component types. Paths are relative to the env field (the
/// first segment is the env local; here we record the sub-component path
/// from the env down, e.g. `["source"]`).
pub(crate) fn resolve_connects(
    env: &ComponentDecl,
    env_schema: &ComponentSchema,
    components: &[ComponentSchema],
) -> Result<Vec<ConnectEdgeSchema>, LowerError> {
    let mut edges = Vec::new();
    for it in &env.items {
        let ComponentItem::Connect(block) = it else {
            continue;
        };
        for e in &block.edges {
            edges.push(resolve_one_connect(env, env_schema, components, e)?);
        }
    }
    Ok(edges)
}

fn resolve_one_connect(
    env: &ComponentDecl,
    env_schema: &ComponentSchema,
    components: &[ComponentSchema],
    edge: &ConnectEdge,
) -> Result<ConnectEdgeSchema, LowerError> {
    // `source.observed -> sb.write_obs`: both sides are dotted paths
    // rooted at an env sub-component field.
    let from = dotted_path(&edge.from).ok_or_else(|| {
        unsupported(
            &format!("a non-path `connect` source in env `{}`", env.name.name),
            "",
        )
    })?;
    let to = dotted_path(&edge.to).ok_or_else(|| {
        unsupported(
            &format!("a non-path `connect` sink in env `{}`", env.name.name),
            "",
        )
    })?;
    // The source path is `<subcomp>.<event>` (final segment is the event
    // port on the source sub-component).
    if from.len() < 2 {
        return Err(unsupported(
            &format!("a `connect` source `{}` without an event field", from.join(".")),
            "",
        ));
    }
    let (src_path, src_event) = from.split_at(from.len() - 1);
    let src_event = src_event[0].clone();
    // The sink path is `<subcomp>.<method>` (final segment is the
    // hookable method on the sink sub-component).
    if to.len() < 2 {
        return Err(unsupported(
            &format!("a `connect` sink `{}` without a method", to.join(".")),
            "",
        ));
    }
    let (sink_path, sink_method) = to.split_at(to.len() - 1);
    let sink_method = sink_method[0].clone();

    // Resolve the source sub-component and verify it exposes `src_event`.
    let src_cid = resolve_sub_path(env_schema, components, src_path)?;
    let src_comp = &components[src_cid.index()];
    match src_comp.field(&src_event) {
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Event { .. },
            ..
        }) => {}
        _ => {
            return Err(unsupported(
                &format!(
                    "a `connect` source `{}.{src_event}` that is not an `out event` port",
                    src_path.join(".")
                ),
                "",
            ));
        }
    }

    // Resolve the sink sub-component and verify it exposes the method.
    let sink_cid = resolve_sub_path(env_schema, components, sink_path)?;
    let sink_comp = &components[sink_cid.index()];
    let sm = sink_comp.method(&sink_method).ok_or_else(|| {
        unsupported(
            &format!(
                "a `connect` sink method `{}.{sink_method}` that does not exist",
                sink_path.join(".")
            ),
            "",
        )
    })?;
    if sm.n_params != 1 {
        return Err(unsupported(
            &format!(
                "a `connect` sink method `{sink_method}` with {} parameters",
                sm.n_params
            ),
            "analysis sinks take exactly one payload parameter",
        ));
    }

    Ok(ConnectEdgeSchema {
        src_path: src_path.to_vec(),
        src_event,
        sink_path: sink_path.to_vec(),
        sink_component: sink_cid,
        sink_method,
    })
}

/// Walk a sub-component path (relative to the env) to the ComponentId it
/// names. `["source"]` → the env field `source`'s component.
fn resolve_sub_path(
    env_schema: &ComponentSchema,
    components: &[ComponentSchema],
    path: &[String],
) -> Result<ComponentId, LowerError> {
    let mut cur = env_schema;
    let mut cid = None;
    for seg in path {
        let f = cur.field(seg).ok_or_else(|| {
            unsupported(
                &format!("a `connect` path segment `{seg}` that is not a sub-component field"),
                "",
            )
        })?;
        match &f.kind {
            ComponentFieldKind::Sub { component } => {
                cid = Some(*component);
                cur = &components[component.index()];
            }
            _ => {
                return Err(unsupported(
                    &format!("a `connect` path segment `{seg}` that is not a sub-component"),
                    "",
                ));
            }
        }
    }
    cid.ok_or_else(|| unsupported("an empty `connect` sub-component path", ""))
}

/// Flatten `a.b.c` (Field nodes over an Ident root) into a dotted path,
/// or `None` for anything else.
fn dotted_path(e: &crate::ast::Expr) -> Option<Vec<String>> {
    match &*e.kind {
        ExprKind::Ident(id) => Some(vec![id.name.clone()]),
        ExprKind::Field { target, name } => {
            let mut p = dotted_path(target)?;
            p.push(name.name.clone());
            Some(p)
        }
        ExprKind::Paren(inner) => dotted_path(inner),
        _ => None,
    }
}

// --- shared scalar-field helpers (mirroring scoreboards.rs) ---

fn scalar_ir_type(t: &TypeExpr) -> Option<IrType> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    let width = match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => Some(s.replace('_', "").parse::<u32>().ok()?),
            _ => return None,
        },
        Some(_) => return None,
        None => None,
    };
    if width.is_some_and(|w| w == 0 || w > 64) {
        return None;
    }
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
            Some(IrType::UInt(width))
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(width)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        _ => None,
    }
}

fn scalar_width(t: &TypeExpr) -> Option<u32> {
    match scalar_ir_type(t) {
        Some(IrType::UInt(Some(w))) | Some(IrType::SInt(Some(w))) => Some(w),
        Some(IrType::Bool) => Some(1),
        _ => None,
    }
}

fn scalar_default(
    default: &Option<crate::ast::Expr>,
    comp: &str,
    fname: &str,
) -> Result<u64, LowerError> {
    match default {
        None => Ok(0),
        Some(d) => match &*d.kind {
            ExprKind::Int(s) => super::exprs::parse_int_literal(s).ok_or_else(|| {
                unsupported(
                    &format!("component field default `{comp}.{fname} default {s}`"),
                    "not a plain integer literal",
                )
            }),
            ExprKind::Bool(b) => Ok(*b as u64),
            _ => Err(unsupported(
                &format!("a non-literal default on component field `{comp}.{fname}`"),
                "",
            )),
        },
    }
}

// --- access resolution on FuncBuilder (test-body + method-body) ---

use crate::ast::{CallArg, Expr as AstExpr};
use crate::ir::{ComponentBase, Expr as IrExpr, Stmt as IrStmt};

impl super::FuncBuilder<'_> {
    /// Resolve a component-method call callee: `env.source.publish` (a
    /// dotted path rooted at a test-scope component local) or a bare
    /// `publish` self-call inside a method body. Returns
    /// `(base, component, method)` when the receiver resolves to a
    /// component and `method` is one of its methods; `None` otherwise.
    /// An error is reserved for a malformed path that clearly INTENDED a
    /// component (head segment is a component local) but does not resolve.
    pub(crate) fn as_component_method_call(
        &self,
        callee: &AstExpr,
    ) -> Result<Option<(ComponentBase, ComponentId, String)>, LowerError> {
        // Path form: `<path...>.<method>`.
        if let Some(path) = dotted_path(callee) {
            if path.len() >= 2 {
                if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
                    let (recv, method) = path.split_at(path.len() - 1);
                    let method = method[0].clone();
                    let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                    let comp = &self.ctx.components[cid.index()];
                    if comp.method(&method).is_none() {
                        return Err(unsupported(
                            &format!(
                                "component `{}` has no method `{method}` (in `{}`)",
                                comp.name,
                                path.join(".")
                            ),
                            "",
                        ));
                    }
                    return Ok(Some((ComponentBase::Path(recv.to_vec()), cid, method)));
                }
            }
        }
        // Self-call form: bare `publish` inside a method body.
        if let ExprKind::Ident(id) = &*callee.kind {
            if let Some(cid) = self.self_component {
                let comp = &self.ctx.components[cid.index()];
                if comp.method(&id.name).is_some() {
                    return Ok(Some((ComponentBase::SelfField, cid, id.name.clone())));
                }
            }
        }
        Ok(None)
    }

    /// Resolve a component scalar-field assignment target: a bare
    /// `count` (self-relative, inside a method body) or a dotted path
    /// `env.sb.errors` (test scope). Returns `(base, field)` when it
    /// resolves to a scalar field of a component.
    pub(crate) fn as_component_field_target(
        &self,
        target: &AstExpr,
    ) -> Result<Option<(ComponentBase, String)>, LowerError> {
        // Self-relative bare field (only inside a method body, and only
        // when the name is NOT a shadowing local).
        if let ExprKind::Ident(id) = &*target.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if matches!(
                        comp.field(&id.name).map(|f| &f.kind),
                        Some(ComponentFieldKind::Scalar { .. })
                    ) {
                        return Ok(Some((ComponentBase::SelfField, id.name.clone())));
                    }
                }
            }
        }
        // Dotted path `env.sb.errors`.
        if let Some(path) = dotted_path(target) {
            if path.len() >= 2 {
                if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
                    let (recv, field) = path.split_at(path.len() - 1);
                    let field = field[0].clone();
                    let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                    let comp = &self.ctx.components[cid.index()];
                    match comp.field(&field).map(|f| &f.kind) {
                        Some(ComponentFieldKind::Scalar { .. }) => {
                            return Ok(Some((ComponentBase::Path(recv.to_vec()), field)));
                        }
                        _ => {
                            return Err(unsupported(
                                &format!(
                                    "write to `{}` — not a scalar component field",
                                    path.join(".")
                                ),
                                "",
                            ));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Resolve a component scalar-field READ as an `Expr::ComponentField`:
    /// bare `count` (self) or path `env.sb.count`.
    pub(crate) fn as_component_field_read(
        &self,
        e: &AstExpr,
    ) -> Result<Option<IrExpr>, LowerError> {
        if let ExprKind::Ident(id) = &*e.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(cid) = self.self_component {
                    let comp = &self.ctx.components[cid.index()];
                    if matches!(
                        comp.field(&id.name).map(|f| &f.kind),
                        Some(ComponentFieldKind::Scalar { .. })
                    ) {
                        return Ok(Some(IrExpr::ComponentField {
                            base: ComponentBase::SelfField,
                            field: id.name.clone(),
                        }));
                    }
                }
            }
        }
        if let Some(path) = dotted_path(e) {
            if path.len() >= 2 {
                if let Some(&head_cid) = self.ctx.component_fields.get(&path[0]) {
                    let (recv, field) = path.split_at(path.len() - 1);
                    let field = field[0].clone();
                    let cid = self.resolve_component_recv(head_cid, &recv[1..])?;
                    let comp = &self.ctx.components[cid.index()];
                    if matches!(
                        comp.field(&field).map(|f| &f.kind),
                        Some(ComponentFieldKind::Scalar { .. })
                    ) {
                        return Ok(Some(IrExpr::ComponentField {
                            base: ComponentBase::Path(recv.to_vec()),
                            field,
                        }));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Walk a sub-component receiver path from a head component down
    /// `segs` (each must name a `Sub` field), returning the final
    /// ComponentId. `segs` is the path AFTER the head local segment.
    fn resolve_component_recv(
        &self,
        head: ComponentId,
        segs: &[String],
    ) -> Result<ComponentId, LowerError> {
        let mut cid = head;
        for seg in segs {
            let comp = &self.ctx.components[cid.index()];
            match comp.field(seg).map(|f| &f.kind) {
                Some(ComponentFieldKind::Sub { component }) => cid = *component,
                _ => {
                    return Err(unsupported(
                        &format!("`{seg}` is not a sub-component of `{}`", comp.name),
                        "",
                    ));
                }
            }
        }
        Ok(cid)
    }

    /// Lower the args of a component method call (port-hoisted, like any
    /// host-side call).
    pub(crate) fn lower_component_call_args(
        &mut self,
        args: &[CallArg],
    ) -> Result<Vec<IrExpr>, LowerError> {
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            let CallArg::Expr(e) = a else {
                return Err(unsupported("named arguments in a component method call", ""));
            };
            out.push(self.lower_expr_no_ports(e)?);
        }
        Ok(out)
    }

    /// Lower `emit <event>(args)` inside a component method body to a
    /// `Stmt::ComponentEmit` (self-relative analysis-port fan-out).
    pub(crate) fn lower_emit(
        &mut self,
        name: &crate::ast::Path,
        args: &[CallArg],
    ) -> Result<(), LowerError> {
        let cid = self.self_component.ok_or_else(|| {
            unsupported(
                "`emit` outside a component method body",
                "only analysis-source `out event` ports support `emit` in this subset",
            )
        })?;
        // The event must be a single bare segment naming an `out event`
        // field of the body's component. `emit top.prod.in_ev(t)` (a
        // path to a nested component's event) is the agent-slice form and
        // is rejected precisely.
        if name.segments.len() != 1 {
            return Err(unsupported(
                &format!("`emit {}` to a dotted event path", path_str(name)),
                "only self-relative `emit <event>(...)` is lowered",
            ));
        }
        let event = name.segments[0].name.clone();
        let comp = &self.ctx.components[cid.index()];
        match comp.field(&event).map(|f| &f.kind) {
            Some(ComponentFieldKind::Event { .. }) => {}
            _ => {
                return Err(unsupported(
                    &format!("`emit {event}` — not an `out event` field of `{}`", comp.name),
                    "",
                ));
            }
        }
        let lowered = self.lower_component_call_args(args)?;
        self.push(IrStmt::ComponentEmit {
            event,
            args: lowered,
        });
        Ok(())
    }
}

fn path_str(p: &crate::ast::Path) -> String {
    p.segments
        .iter()
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        .join(".")
}
