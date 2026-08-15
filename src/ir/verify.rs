//! TB-IR structural verifier — design-doc invariants 1-8, 10, 15 plus
//! the port-position rule (an `Expr::Port` may appear only in wait
//! predicates, format-arg expressions, `DutRead`/`DutWrite` operands,
//! `AssertCheck` condition subtrees, and `FailDiag` guards — which
//! re-evaluate a wait predicate after the wait timed out) and the
//! transactor-call seam rule (below).
//!
//! **Transactor-call seam rule.** A `CallTarget::TransactorMethod`
//! call edge is deliberately never inlined at the IR level — the
//! sequence→transactor boundary is the placement cut every split
//! backend needs (design doc §CallTarget). The verifier pins the edge
//! to the one position backends expand: the ENTIRE right-hand side of
//! a `Stmt::Assign` in a `Run`/`Check` function, with a `bus_field`/
//! `method` pair that resolves against the owning testbench's
//! `bus_bindings` at the declared arity. Anywhere else — nested in an
//! expression, in a format arg or wait predicate, or inside a
//! `Helper`/`SamplerAuto` body (pure helpers must stay suspension-
//! free and placement-neutral) — is a lowering bug. The edge is also
//! the sanctioned exception to "no statement may suspend": its
//! suspension lives behind the call boundary, which placement
//! classifies as timing-tolerant by construction.
//!
//! Violations are programmer errors (lowering bugs or pass bugs), not
//! user errors — user errors are rejected earlier by the lowering pass.
//!
//! Two deliberate deviations from the doc's literal text:
//! - Invariant 5 ("exactly one terminator") and invariant 7 ("no
//!   suspending Stmt") hold by construction — `BasicBlock` has one
//!   `terminator` field and `Stmt` has no suspending variant — so no
//!   runtime check is needed.
//! - Invariant 8 permits empty blocks terminated by `Branch` or a
//!   suspension (`WaitCycles`/`WaitUntil`/`WaitUntilTimeout`) in
//!   addition to `Return`/`Jump`: loop headers are empty-by-design
//!   branch blocks (see the doc's own worked example 2, `b_header`),
//!   and a loop body whose first statement is `wait N cycles` lowers
//!   to an empty block whose terminator IS the content. Only an empty
//!   `Fatal` block remains flagged — the design synthesizes the fail
//!   action into that block's statements, so emptiness there means the
//!   synthesis dropped its body.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// Invariant 1: entry resolves to a block.
    BadEntry { func: FunctionId, entry: BlockId },
    /// Invariant 2: every block reachable from entry.
    UnreachableBlock { func: FunctionId, block: BlockId },
    /// Invariant 3: every LocalId resolves.
    BadLocal {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
    },
    /// Invariant 4: defs dominate uses.
    LocalUseBeforeDef {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
    },
    /// Invariant 6: terminator successors resolve.
    BadSuccessor {
        func: FunctionId,
        block: BlockId,
        succ: BlockId,
    },
    /// Invariant 8 (amended): empty `Fatal` block — the synthesized
    /// fail action went missing.
    EmptyBlock { func: FunctionId, block: BlockId },
    /// Invariant 10: covgroup references resolve.
    BadCovgroup {
        func: FunctionId,
        block: BlockId,
        covgroup: CovgroupId,
    },
    /// A concurrent-check reference (`Stmt::PropertyCheck` /
    /// `Stmt::CoverCheck`) does not resolve, or an `Expr::TemporalSlot`
    /// appears where no latch cells exist (outside a check body) or
    /// names a slot the check does not declare.
    BadConcurrentCheck {
        func: FunctionId,
        block: BlockId,
        detail: String,
    },
    /// Invariant 15: Assign type matches the local's declared type
    /// (only checked when both sides are known).
    TypeMismatch {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
        expected: IrType,
        actual: IrType,
    },
    /// WidthCast nodes carry language-level bit widths even when they are
    /// constructed by a compiler pass rather than source lowering.
    BadWidthCast {
        func: FunctionId,
        block: BlockId,
        width: u32,
        src_width: Option<u32>,
    },
    /// Record references resolve: the `RecordId` indexes the records
    /// table and record-typed locals carry the matching `IrType`.
    BadRecord {
        func: FunctionId,
        block: BlockId,
        record: RecordId,
    },
    /// A record field access names a field the schema does not have,
    /// or targets a local that is not record-typed.
    BadRecordField {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
        field: String,
    },
    /// A `TbField`/`TbFieldWrite` names a scalar field the owning
    /// testbench schema does not declare (or the function has no
    /// owning testbench at all — helpers cannot touch TB state).
    BadTbField {
        func: FunctionId,
        block: BlockId,
        field: String,
    },
    /// Port-position rule: `Expr::Port` outside an allowed position.
    PortInDisallowedPosition {
        func: FunctionId,
        block: BlockId,
        context: &'static str,
    },
    /// Transactor-call seam rule (module docs). A
    /// `CallTarget::TransactorMethod` edge must resolve in exactly one
    /// namespace of the owning testbench, in its sanctioned position:
    /// bus binding → the entire `Assign` RHS of a Run/Check function;
    /// transactor field → the payload of a `Stmt::TransactorCall`.
    /// Violations: an edge nested in expression position (it can
    /// advance simulated time — never an expression value), a
    /// `Stmt::TransactorCall` payload that is not a call edge, or an
    /// edge that resolves in neither/the wrong namespace.
    BadTransactorCall {
        func: FunctionId,
        block: BlockId,
        detail: String,
    },
    /// A scoreboard op/query references a scoreboard id, testbench field,
    /// or scoreboard field that does not resolve (or names a queue where
    /// a scalar is expected, or vice versa).
    BadScoreboard {
        func: FunctionId,
        block: BlockId,
        detail: String,
    },
    /// Cross-IR: a test's run/check FunctionId or TestbenchId resolves.
    BadProgramRef { what: String },
    /// Invariant 9: a `Terminator::Randomize`'s `ConstraintRef` must
    /// index `TbProgram::constraint_sites`, and its `target` local must
    /// be record-typed (the solver writes record fields back into it).
    DanglingConstraintRef {
        func: FunctionId,
        block: BlockId,
        detail: String,
    },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::BadEntry { func, entry } => {
                write!(f, "fn{}: entry b{} does not resolve", func.0, entry.0)
            }
            VerifyError::UnreachableBlock { func, block } => {
                write!(f, "fn{}: block b{} unreachable from entry", func.0, block.0)
            }
            VerifyError::BadLocal { func, block, local } => write!(
                f,
                "fn{}: b{} references undeclared local %{}",
                func.0, block.0, local.0
            ),
            VerifyError::LocalUseBeforeDef { func, block, local } => write!(
                f,
                "fn{}: b{} reads local %{} before any definition dominates it",
                func.0, block.0, local.0
            ),
            VerifyError::BadSuccessor { func, block, succ } => write!(
                f,
                "fn{}: b{} terminator targets missing block b{}",
                func.0, block.0, succ.0
            ),
            VerifyError::EmptyBlock { func, block } => write!(
                f,
                "fn{}: b{} is an empty Fatal block (synthesized fail action missing)",
                func.0, block.0
            ),
            VerifyError::BadCovgroup {
                func,
                block,
                covgroup,
            } => write!(
                f,
                "fn{}: b{} references missing covgroup cg{}",
                func.0, block.0, covgroup.0
            ),
            VerifyError::BadConcurrentCheck {
                func,
                block,
                detail,
            } => write!(f, "fn{}: b{}: {detail}", func.0, block.0),
            VerifyError::TypeMismatch {
                func,
                block,
                local,
                expected,
                actual,
            } => write!(
                f,
                "fn{}: b{} assigns {:?} into local %{} declared {:?}",
                func.0, block.0, actual, local.0, expected
            ),
            VerifyError::BadWidthCast {
                func,
                block,
                width,
                src_width,
            } => write!(
                f,
                "fn{}: b{} has invalid width cast destination {} and source {:?}",
                func.0, block.0, width, src_width
            ),
            VerifyError::BadRecord {
                func,
                block,
                record,
            } => write!(
                f,
                "fn{}: b{} references missing or mismatched record r{}",
                func.0, block.0, record.0
            ),
            VerifyError::BadRecordField {
                func,
                block,
                local,
                field,
            } => write!(
                f,
                "fn{}: b{} accesses field `{field}` on local %{} (not a record-typed local or no such field)",
                func.0, block.0, local.0
            ),
            VerifyError::BadTbField { func, block, field } => write!(
                f,
                "fn{}: b{} accesses testbench scalar field `{field}` that the owning \
                 testbench does not declare",
                func.0, block.0
            ),
            VerifyError::PortInDisallowedPosition {
                func,
                block,
                context,
            } => write!(
                f,
                "fn{}: b{} contains a DUT port read in a disallowed position ({context})",
                func.0, block.0
            ),
            VerifyError::BadTransactorCall {
                func,
                block,
                detail,
            } => write!(
                f,
                "fn{}: b{} transactor-call seam violation: {detail}",
                func.0, block.0
            ),
            VerifyError::BadScoreboard { func, block, detail } => write!(
                f,
                "fn{}: b{} scoreboard reference error: {detail}",
                func.0, block.0
            ),
            VerifyError::BadProgramRef { what } => write!(f, "program: {what}"),
            VerifyError::DanglingConstraintRef {
                func,
                block,
                detail,
            } => write!(
                f,
                "f{} b{}: dangling Randomize constraint ref ({detail})",
                func.0, block.0
            ),
        }
    }
}

/// Walk a concurrent-check body and report every `Expr::TemporalSlot`
/// whose index is at or beyond `n_slots`. Passing `n_slots == 0` asserts
/// the expression carries no slot reading at all — used for latch
/// operands, where a nested reading would need slot-of-slot accounting
/// the model deliberately does not carry.
fn check_temporal_slots(e: &Expr, n_slots: usize, what: &str, errs: &mut Vec<VerifyError>) {
    match e {
        Expr::TemporalSlot { slot, .. } => {
            if (*slot as usize) >= n_slots {
                errs.push(VerifyError::BadProgramRef {
                    what: if n_slots == 0 {
                        format!("{what} nests a temporal reading inside a latch operand")
                    } else {
                        format!(
                            "{what} references temporal slot {slot} but declares only \
                             {n_slots} latch(es)"
                        )
                    },
                });
            }
        }
        Expr::Binary(_, a, b) => {
            check_temporal_slots(a, n_slots, what, errs);
            check_temporal_slots(b, n_slots, what, errs);
        }
        Expr::Unary(_, a) => check_temporal_slots(a, n_slots, what, errs),
        Expr::BitSlice { target, .. } => check_temporal_slots(target, n_slots, what, errs),
        Expr::BitSliceDyn { target, hi, lo } => {
            check_temporal_slots(target, n_slots, what, errs);
            check_temporal_slots(hi, n_slots, what, errs);
            check_temporal_slots(lo, n_slots, what, errs);
        }
        Expr::Ternary(c, t, f) => {
            check_temporal_slots(c, n_slots, what, errs);
            check_temporal_slots(t, n_slots, what, errs);
            check_temporal_slots(f, n_slots, what, errs);
        }
        Expr::WidthCast { inner, .. } => check_temporal_slots(inner, n_slots, what, errs),
        Expr::ComponentIdle { n, .. } => check_temporal_slots(n, n_slots, what, errs),
        Expr::SeqIndex { index, .. } => check_temporal_slots(index, n_slots, what, errs),
        Expr::Call(_, args) => {
            for a in args {
                check_temporal_slots(a, n_slots, what, errs);
            }
        }
        Expr::RecordField {
            mid_indices, index, ..
        } => {
            for (_, idx) in mid_indices {
                check_temporal_slots(idx, n_slots, what, errs);
            }
            if let Some(idx) = index {
                check_temporal_slots(idx, n_slots, what, errs);
            }
        }
        _ => {}
    }
}

pub fn verify_program(prog: &TbProgram) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    for t in &prog.tests {
        if t.testbench.index() >= prog.testbenches.len() {
            errs.push(VerifyError::BadProgramRef {
                what: format!("test {} references missing tb{}", t.name, t.testbench.0),
            });
        }
        if t.run.index() >= prog.functions.len() {
            errs.push(VerifyError::BadProgramRef {
                what: format!("test {} references missing run fn{}", t.name, t.run.0),
            });
        }
        if let Some(c) = t.check {
            if c.index() >= prog.functions.len() {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("test {} references missing check fn{}", t.name, c.0),
                });
            }
        }
        // Cross-IR: every clock-qualified WaitCycles in the test's
        // functions must name a clock the test actually declares
        // (index in range AND name agreement — lowering resolves both
        // together, so disagreement means a pass corrupted the IR).
        // Codegen indexes the runtime clock vector with `index`
        // unchecked; this is the net that keeps that sound.
        for fid in [Some(t.run), t.check].into_iter().flatten() {
            let Some(func) = prog.functions.get(fid.index()) else {
                continue; // missing fn already reported above
            };
            for (bi, b) in func.blocks.iter().enumerate() {
                let Terminator::WaitCycles(_, Some(wc), _) = &b.terminator else {
                    continue;
                };
                match t.clocks.get(wc.index) {
                    Some(spec) if spec.name == wc.name => {}
                    Some(spec) => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "test {}: fn{} b{bi} waits on clock `{}` at index {} but \
                             that slot is `{}`",
                            t.name, fid.0, wc.name, wc.index, spec.name
                        ),
                    }),
                    None => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "test {}: fn{} b{bi} waits on clock `{}` at index {} but \
                             only {} clock(s) are declared",
                            t.name,
                            fid.0,
                            wc.name,
                            wc.index,
                            t.clocks.len()
                        ),
                    }),
                }
            }
        }
    }
    // Covergroup schemas: declared crosses must reference 2+ existing
    // points, all binned (lowering validates this; a pass that edits
    // schemas must not break it — emission indexes `points` directly).
    for (ci, cg) in prog.covgroups.iter().enumerate() {
        for cross in &cg.crosses {
            if cross.point_indices.len() < 2 {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("cg{ci} cross has fewer than two points"),
                });
            }
            for &pi in &cross.point_indices {
                if pi >= cg.points.len() {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!("cg{ci} cross references missing point index {pi}"),
                    });
                } else if cg.points[pi].bins.is_empty() {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "cg{ci} cross references binless point `{}`",
                            cg.points[pi].name
                        ),
                    });
                }
            }
        }
    }
    // Component-mode metadata is consumed directly by lowering and codegen:
    // preserve the invariant that only transactors have an active surface,
    // and that a nested mode override names a transactor child.
    for (ci, component) in prog.components.iter().enumerate() {
        let mut component_functions = std::collections::HashSet::new();
        let mut check_component_function = |what: &str, function: FunctionId| {
            if !component_functions.insert(function) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "component c{ci} `{}` uses fn{} for more than one {what}",
                        component.name, function.0
                    ),
                });
            }
            match prog.functions.get(function.index()) {
                Some(f)
                    if f.kind
                        == (FunctionKind::ComponentMethod {
                            component: ComponentId(ci as u32),
                        }) => {}
                Some(f) => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "component c{ci} `{}` {what} points at fn{} with kind {:?}",
                        component.name, function.0, f.kind
                    ),
                }),
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "component c{ci} `{}` {what} references missing fn{}",
                        component.name, function.0
                    ),
                }),
            }
        };
        for method in &component.methods {
            check_component_function("method", method.function);
        }
        for handler in &component.on_handlers {
            check_component_function("on handler", handler.function);
        }
        for handler in &component.periodic_handlers {
            check_component_function("periodic handler", handler.function);
        }
        for handler in &component.cycle_handlers {
            check_component_function("cycle handler", handler.function);
        }
        if let Some(handler) = &component.watchdog {
            check_component_function("watchdog", handler.function);
        }
        let active_member = component
            .fields
            .iter()
            .any(|field| matches!(field.activation, Activation::ActiveOnly))
            || component
                .methods
                .iter()
                .any(|method| matches!(method.activation, Activation::ActiveOnly))
            || component
                .on_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || component
                .periodic_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || component
                .cycle_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || component
                .watchdog
                .as_ref()
                .is_some_and(|handler| matches!(handler.activation, Activation::ActiveOnly));
        if active_member && !matches!(component.kind, ComponentKindTag::Transactor) {
            errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "component c{ci} `{}` is not a transactor but has active-only members",
                    component.name
                ),
            });
        }
        for field in &component.fields {
            let ComponentFieldKind::Sub {
                component: child,
                mode: Some(_),
            } = &field.kind
            else {
                continue;
            };
            match prog.components.get(child.index()) {
                Some(child_schema) if matches!(child_schema.kind, ComponentKindTag::Transactor) => {
                }
                Some(child_schema) => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "component c{ci} field `{}` declares a transactor mode on {} `{}`",
                        field.name,
                        child_schema.kind.keyword(),
                        child_schema.name
                    ),
                }),
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "component c{ci} field `{}` references missing child c{}",
                        field.name, child.0
                    ),
                }),
            }
        }
    }
    // Concurrent-check schemas: every `Expr::TemporalSlot` in a check
    // body must name a slot the check declares, and a latch's own
    // operand must not itself be a slot reading (no `past(past(x))` —
    // slot-of-slot accounting is deliberately out of the model, matching
    // v1's non-recursing occurrence walk). Emission indexes `temporals`
    // directly, so this is the net that keeps that sound.
    for (pi, p) in prog.property_checks.iter().enumerate() {
        let n = p.temporals.len();
        let exprs: Vec<&Expr> = match &p.shape {
            crate::ir::PropertyShape::Implies { ante, cons }
            | crate::ir::PropertyShape::ImpliesNext { ante, cons } => vec![ante, cons],
            crate::ir::PropertyShape::Invariant(e) => vec![e],
        };
        for e in exprs {
            check_temporal_slots(e, n, &format!("property check p{pi}"), &mut errs);
        }
        // The `else fail(...)` message renders inside the same closure
        // but is lowered with no slot map, so it can hold no
        // `Expr::TemporalSlot` at all — 0 slots, not `n`. A slot
        // appearing here means the message picked up an occurrence by
        // span collision, which is exactly the bug the empty map
        // prevents; the check keeps that guarantee testable.
        for a in p.message.iter().flat_map(|m| &m.args) {
            check_temporal_slots(
                &a.expr,
                0,
                &format!("property check p{pi} message"),
                &mut errs,
            );
        }
        for (si, slot) in p.temporals.iter().enumerate() {
            check_temporal_slots(
                &slot.inner,
                0,
                &format!("property check p{pi} latch operand {si}"),
                &mut errs,
            );
        }
    }
    for (ci, c) in prog.cover_checks.iter().enumerate() {
        check_temporal_slots(
            &c.cond,
            c.temporals.len(),
            &format!("cover check c{ci}"),
            &mut errs,
        );
        for (si, slot) in c.temporals.iter().enumerate() {
            check_temporal_slots(
                &slot.inner,
                0,
                &format!("cover check c{ci} latch operand {si}"),
                &mut errs,
            );
        }
    }
    // Every test's reported cover list must resolve into the table.
    for t in &prog.tests {
        for c in &t.cover_checks {
            if c.index() >= prog.cover_checks.len() {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("test {} reports missing cover check c{}", t.name, c.0),
                });
            }
        }
    }
    // Transactor schemas: every method's FunctionId resolves to a
    // function tagged `TransactorBody` for that transactor, and every
    // testbench transactor field resolves. Emission indexes both
    // tables directly off these links.
    for (xi, x) in prog.transactors.iter().enumerate() {
        for m in &x.methods {
            match prog.functions.get(m.function.index()) {
                Some(f)
                    if f.kind
                        == (FunctionKind::TransactorBody {
                            transactor: TransactorId(xi as u32),
                        }) => {}
                Some(f) => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` points at fn{} with kind {:?}",
                        m.name, m.function.0, f.kind
                    ),
                }),
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` references missing fn{}",
                        m.name, m.function.0
                    ),
                }),
            }
        }
    }
    for (ti, tb) in prog.testbenches.iter().enumerate() {
        let mut component_binding_names = std::collections::HashSet::new();
        let state_scalars: Vec<_> = tb
            .state_fields
            .iter()
            .filter_map(|field| match field {
                TbStateFieldSchema::Scalar(scalar) => Some(scalar.clone()),
                TbStateFieldSchema::Queue(_) => None,
            })
            .collect();
        let state_queues: Vec<_> = tb
            .state_fields
            .iter()
            .filter_map(|field| match field {
                TbStateFieldSchema::Scalar(_) => None,
                TbStateFieldSchema::Queue(queue) => Some(queue.clone()),
            })
            .collect();
        if state_scalars != tb.scalar_fields || state_queues != tb.queue_fields {
            errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "tb{ti} state_fields does not exactly project to scalar_fields and queue_fields"
                ),
            });
        }
        let mut state_names = std::collections::HashSet::new();
        for field in &tb.state_fields {
            let name = match field {
                TbStateFieldSchema::Scalar(field) => &field.name,
                TbStateFieldSchema::Queue(field) => &field.name,
            };
            if !state_names.insert(name) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("tb{ti} declares state field `{name}` more than once"),
                });
            }
        }
        for (field, xid) in &tb.transactor_fields {
            if xid.index() >= prog.transactors.len() {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} transactor field `{field}` references missing x{}",
                        xid.0
                    ),
                });
            }
        }
        for binding in &tb.component_fields {
            if !component_binding_names.insert(&binding.field) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} declares component field `{}` more than once",
                        binding.field
                    ),
                });
            }
            match prog.components.get(binding.component.index()) {
                Some(component) if matches!(component.kind, ComponentKindTag::Transactor) => {
                    if binding.mode.is_none() {
                        errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "tb{ti} transactor component field `{}` has no active/passive mode",
                                binding.field
                            ),
                        });
                    }
                }
                Some(_) => {}
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} component field `{}` references missing c{}",
                        binding.field, binding.component.0
                    ),
                }),
            }
        }
        for edge in &tb.connects {
            if let Err(detail) = verify_testbench_connect(prog, tb, edge) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("tb{ti} has invalid connect metadata: {detail}"),
                });
            }
        }
    }
    for (i, func) in prog.functions.iter().enumerate() {
        if func.id.index() != i {
            errs.push(VerifyError::BadProgramRef {
                what: format!("fn at index {i} carries id fn{}", func.id.0),
            });
        }
        if let Err(mut e) = verify_function(prog, func) {
            errs.append(&mut e);
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

fn verify_testbench_connect(
    prog: &TbProgram,
    tb: &TestbenchSchema,
    edge: &ConnectEdgeSchema,
) -> Result<(), String> {
    let src_id = resolve_testbench_component_path(prog, tb, &edge.src_path)?;
    let src = prog
        .components
        .get(src_id.index())
        .ok_or_else(|| format!("source component c{} does not resolve", src_id.0))?;
    let payload = match src.field(&edge.src_event) {
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Event { payload }, activation,
            ..
        }) if *activation == edge.src_activation => *payload,
        _ => {
            return Err(format!(
                "source `{}.{}` is not an event field",
                edge.src_path.join("."),
                edge.src_event
            ));
        }
    };

    let sink_id = resolve_testbench_component_path(prog, tb, &edge.sink_path)?;
    if sink_id != edge.sink_component {
        return Err(format!(
            "sink path `{}` resolves to c{} but edge stores c{}",
            edge.sink_path.join("."),
            sink_id.0,
            edge.sink_component.0
        ));
    }
    let sink = prog
        .components
        .get(sink_id.index())
        .ok_or_else(|| format!("sink component c{} does not resolve", sink_id.0))?;
    match &edge.sink {
        ConnectSink::Method { method } => {
            let Some(m) = sink.method(method) else {
                return Err(format!("sink method `{method}` does not resolve"));
            };
            if m.activation != edge.sink_activation {
                return Err(format!("sink method `{method}` has mismatched activation metadata"));
            }
            if !m.hookable || m.n_params != 1 || m.has_ret || m.param_tys.len() != 1 {
                return Err(format!(
                    "sink method `{method}` is not a one-argument void hookable"
                ));
            }
            if !event_payload_matches_type(payload, &m.param_tys[0]) {
                return Err(format!(
                    "sink method `{method}` has an incompatible payload type"
                ));
            }
        }
        ConnectSink::Event { event } => match sink.field(event) {
            Some(ComponentFieldSchema {
                kind:
                    ComponentFieldKind::Event {
                        payload: sink_payload,
                    },
                activation,
                ..
            }) if *sink_payload == payload && *activation == edge.sink_activation => {}
            _ => {
                return Err(format!(
                    "sink event `{event}` does not resolve or has a mismatched payload"
                ))
            }
        },
    }
    Ok(())
}

fn resolve_testbench_component_path(
    prog: &TbProgram,
    tb: &TestbenchSchema,
    path: &[String],
) -> Result<ComponentId, String> {
    let Some((root, tail)) = path.split_first() else {
        return Err("empty component path".to_string());
    };
    let mut cid = tb
        .component_fields
        .iter()
        .find(|field| field.field == *root)
        .map(|field| field.component)
        .ok_or_else(|| format!("root `{root}` is not a testbench component field"))?;
    for segment in tail {
        let component = prog
            .components
            .get(cid.index())
            .ok_or_else(|| format!("component c{} does not resolve", cid.0))?;
        cid = match component.field(segment) {
            Some(ComponentFieldSchema {
                kind: ComponentFieldKind::Sub { component, .. },
                ..
            }) => *component,
            _ => return Err(format!("path segment `{segment}` is not a sub-component")),
        };
    }
    Ok(cid)
}

fn event_payload_matches_type(payload: EventPayload, ty: &IrType) -> bool {
    match (payload, ty) {
        (_, IrType::Unknown) => true,
        (EventPayload::Scalar { signed: true }, IrType::SInt(_)) => true,
        (EventPayload::Scalar { signed: false }, IrType::UInt(_) | IrType::Bool) => true,
        (EventPayload::Record(source), IrType::Record(sink)) => source == *sink,
        _ => false,
    }
}

pub fn verify_function(prog: &TbProgram, func: &TbFunction) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    let nblocks = func.blocks.len();
    let fid = func.id;

    // Invariant 1.
    if func.entry.index() >= nblocks {
        errs.push(VerifyError::BadEntry {
            func: fid,
            entry: func.entry,
        });
        return Err(errs); // nothing else is meaningful
    }

    // Invariant 6 — successors resolve (checked before reachability so
    // the walk below can't index out of bounds).
    for (bi, b) in func.blocks.iter().enumerate() {
        for s in b.terminator.successors() {
            if s.index() >= nblocks {
                errs.push(VerifyError::BadSuccessor {
                    func: fid,
                    block: BlockId(bi as u32),
                    succ: s,
                });
            }
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }

    // Invariant 2 — reachability.
    let mut reachable = vec![false; nblocks];
    let mut work = vec![func.entry];
    while let Some(b) = work.pop() {
        if std::mem::replace(&mut reachable[b.index()], true) {
            continue;
        }
        work.extend(func.block(b).terminator.successors());
    }
    for (bi, r) in reachable.iter().enumerate() {
        if !r {
            errs.push(VerifyError::UnreachableBlock {
                func: fid,
                block: BlockId(bi as u32),
            });
        }
    }

    // Invariant 8 (amended — see module docs).
    for (bi, b) in func.blocks.iter().enumerate() {
        if b.stmts.is_empty() && matches!(b.terminator, Terminator::Fatal(_)) {
            errs.push(VerifyError::EmptyBlock {
                func: fid,
                block: BlockId(bi as u32),
            });
        }
    }

    // Invariants 3, 10, 15 + port positions, per block.
    for (bi, b) in func.blocks.iter().enumerate() {
        let bid = BlockId(bi as u32);
        let mut ck = Checker {
            prog,
            func,
            fid,
            bid,
            errs: &mut errs,
        };
        ck.check_block(b);
    }

    // Invariant 4 — forward dataflow: a local must be defined on every
    // path from entry before its first read. Params count as defined.
    check_def_before_use(func, fid, &reachable, &mut errs);

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

struct Checker<'a> {
    prog: &'a TbProgram,
    func: &'a TbFunction,
    fid: FunctionId,
    bid: BlockId,
    errs: &'a mut Vec<VerifyError>,
}

impl Checker<'_> {
    fn check_block(&mut self, b: &BasicBlock) {
        for s in &b.stmts {
            match s {
                Stmt::Assign(l, e) => {
                    self.check_local(*l);
                    // Transactor-call seam rule: the one sanctioned
                    // position for a TransactorMethod call edge is the
                    // entire Assign RHS of a Run/Check function. Args
                    // are checked individually (no ports, no nesting);
                    // `check_expr` rejects the target everywhere else.
                    if let Expr::Call(CallTarget::TransactorMethod { bus_field, method }, args) = e
                    {
                        self.check_bus_call_edge(bus_field, method, args);
                        continue;
                    }
                    self.check_expr(e, false, "Assign value");
                    // Invariant 15.
                    if self.func.locals.get(l.index()).is_some() {
                        let expected = &self.func.local(*l).ty;
                        if let Some(actual) = expr_type(self.func, e) {
                            if *expected != IrType::Unknown
                                && actual != IrType::Unknown
                                && !assign_compatible(expected, &actual)
                            {
                                self.errs.push(VerifyError::TypeMismatch {
                                    func: self.fid,
                                    block: self.bid,
                                    local: *l,
                                    expected: expected.clone(),
                                    actual,
                                });
                            }
                        }
                    }
                }
                Stmt::DutWrite(_, e) => self.check_expr(e, true, "DutWrite value"),
                Stmt::DutRead(l, _) => self.check_local(*l),
                // `release dut.<probe>` carries no value and no local;
                // the PortRef's access class is validated at lowering.
                Stmt::ProbeRelease(_) => {}
                Stmt::RecordInit(l, r) => {
                    self.check_local(*l);
                    if r.index() >= self.prog.records.len()
                        || self
                            .func
                            .locals
                            .get(l.index())
                            .is_some_and(|tl| tl.ty != IrType::Record(*r))
                    {
                        self.errs.push(VerifyError::BadRecord {
                            func: self.fid,
                            block: self.bid,
                            record: *r,
                        });
                    }
                }
                Stmt::RecordFieldWrite {
                    local,
                    field,
                    path,
                    mid_indices,
                    index,
                    value,
                } => {
                    self.check_local(*local);
                    let mid_positions: Vec<usize> = mid_indices.iter().map(|(p, _)| *p).collect();
                    self.check_record_field(*local, field, path, &mid_positions);
                    for (_, idx) in mid_indices {
                        self.check_expr(idx, false, "RecordFieldWrite mid index");
                    }
                    if let Some(idx) = index {
                        self.check_expr(idx, false, "RecordFieldWrite index");
                    }
                    self.check_expr(value, false, "RecordFieldWrite value");
                }
                Stmt::RecordWriteCb {
                    local,
                    field,
                    value,
                    ..
                } => {
                    self.check_local(*local);
                    self.check_record_field(*local, field, &[], &[]);
                    self.check_expr(value, false, "RecordWriteCb value");
                }
                Stmt::TbFieldWrite { field, value } => {
                    self.check_tb_field(field);
                    self.check_expr(value, false, "TbFieldWrite value");
                }
                Stmt::TbQueuePush { field, value } => {
                    self.check_tb_queue(field);
                    self.check_expr(value, false, "TbQueuePush value");
                    if let (Some(elem), Some(actual)) =
                        (self.tb_queue_elem(field), expr_type(self.func, value))
                    {
                        if !queue_elem_matches_type(elem, &actual) {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} pushes {:?} into testbench queue `{field}` with element {:?}",
                                    self.fid.0, self.bid.0, actual, elem
                                ),
                            });
                        }
                    }
                }
                Stmt::TbQueuePop { field, dest } => {
                    self.check_tb_queue(field);
                    self.check_local(*dest);
                    if let (Some(elem), Some(local)) = (
                        self.tb_queue_elem(field),
                        self.func.locals.get(dest.index()),
                    ) {
                        if !queue_elem_matches_type(elem, &local.ty) {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} pops testbench queue `{field}` with element {:?} into local %{} declared {:?}",
                                    self.fid.0, self.bid.0, elem, dest.0, local.ty
                                ),
                            });
                        }
                    }
                }
                Stmt::TransactorStateWrite { value, .. } => {
                    // Instance/field resolution is a lowering concern
                    // (the verifier has no transactor-binding context);
                    // just hold the value to the no-inline-port rule.
                    self.check_expr(value, false, "TransactorStateWrite value");
                }
                Stmt::TransactorStateRecordFieldWrite { value, .. } => {
                    self.check_expr(value, false, "TransactorStateRecordFieldWrite value");
                }
                Stmt::TransactorStateQueuePush { value, .. } => {
                    // Target-state queue host state — the pushed value
                    // follows the no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "TransactorStateQueuePush value");
                }
                Stmt::TransactorStateQueuePop { dest, .. } => {
                    self.check_local(*dest);
                }
                Stmt::Log { args, .. } => self.check_fmt_args(args),
                Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail } => {
                    self.check_expr(cond, true, "AssertCheck cond");
                    self.check_fmt_args(on_fail);
                }
                Stmt::CovReport(inst) => self.check_covgroup(inst.covgroup),
                // Registration statements carry only a table index; the
                // bodies themselves are verified once per schema by
                // `check_concurrent_checks` (they are not part of any
                // function's local scope, so walking them here would
                // report every reference as an undefined local).
                Stmt::PropertyCheck(p) => {
                    if p.index() >= self.prog.property_checks.len() {
                        self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!("references missing property check p{}", p.0),
                        });
                    }
                }
                Stmt::CoverCheck(c) => {
                    if c.index() >= self.prog.cover_checks.len() {
                        self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!("references missing cover check c{}", c.0),
                        });
                    }
                }
                // The handler body is its own function (verified in its
                // own right); here only the link is checked — the id
                // resolves and points at a zero-parameter `TestHook`,
                // which is what the registration closure can call.
                // The channel local must resolve and be event-typed, and
                // a subscriber body must be a one-parameter `TestHook`
                // whose parameter matches the channel's payload — emission
                // pushes it into a `std::function<void(payload)>` vector.
                Stmt::EventSubscribe { event, handler } => {
                    self.check_local(*event);
                    let payload = self.event_payload(*event);
                    if payload.is_none() {
                        self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!(
                                "EventSubscribe target {} is not an event channel",
                                event.0
                            ),
                        });
                    }
                    match self.prog.functions.get(handler.index()) {
                        Some(f) if f.kind == FunctionKind::TestHook && f.params.len() == 1 => {}
                        Some(f) => self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!(
                                "event subscriber fn{} is {:?} with {} param(s), not a \
                                 one-parameter TestHook",
                                f.id.0,
                                f.kind,
                                f.params.len()
                            ),
                        }),
                        None => self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!("event subscriber references missing fn{}", handler.0),
                        }),
                    }
                }
                Stmt::EventEmit { event, args } => {
                    self.check_local(*event);
                    if self.event_payload(*event).is_none() {
                        self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!("EventEmit target {} is not an event channel", event.0),
                        });
                    }
                    if args.len() != 1 {
                        self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!(
                                "EventEmit carries {} argument(s); an event payload is exactly one",
                                args.len()
                            ),
                        });
                    }
                    for a in args {
                        self.check_expr(a, false, "EventEmit arg");
                    }
                }
                Stmt::CycleHandler(h) => {
                    match self.prog.cycle_handlers.get(h.index()) {
                        None => self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!("references missing cycle handler h{}", h.0),
                        }),
                        Some(schema) => match self.prog.functions.get(schema.function.index()) {
                            Some(f)
                                if f.kind == FunctionKind::TestHook && f.params.is_empty() => {}
                            Some(f) => self.errs.push(VerifyError::BadConcurrentCheck {
                                func: self.fid,
                                block: self.bid,
                                detail: format!(
                                    "cycle handler h{} body fn{} is {:?} with {} param(s),                                      not a zero-parameter TestHook",
                                    h.0,
                                    f.id.0,
                                    f.kind,
                                    f.params.len()
                                ),
                            }),
                            None => self.errs.push(VerifyError::BadConcurrentCheck {
                                func: self.fid,
                                block: self.bid,
                                detail: format!(
                                    "cycle handler h{} references missing fn{}",
                                    h.0, schema.function.0
                                ),
                            }),
                        },
                    }
                }
                Stmt::TransactorCall { dest, call } => {
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                    self.check_transactor_call(call);
                }
                Stmt::TransactorSelfCall { dest, call } => {
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                    self.check_transactor_self_call(*dest, call);
                }
                Stmt::FailDiag { guard, args } => {
                    if let Some(g) = guard {
                        self.check_expr(g, true, "FailDiag guard");
                    }
                    self.check_fmt_args(args);
                }
                Stmt::ScoreboardOp {
                    sb,
                    field,
                    op,
                    nested_path,
                } => {
                    self.check_scoreboard(*sb, field, nested_path.is_some());
                    match op {
                        crate::ir::ScoreboardOp::QueuePush { queue, value } => {
                            self.check_scoreboard_queue(*sb, queue);
                            self.check_expr(value, false, "ScoreboardOp push value");
                        }
                        crate::ir::ScoreboardOp::QueuePop { queue, dest } => {
                            self.check_scoreboard_queue(*sb, queue);
                            self.check_local(*dest);
                        }
                        crate::ir::ScoreboardOp::ScalarWrite { scalar, value } => {
                            self.check_scoreboard_scalar(*sb, scalar);
                            self.check_expr(value, false, "ScoreboardOp scalar value");
                        }
                    }
                }
                Stmt::ComponentFieldWrite { value, .. } => {
                    // Component host state — the value follows the
                    // no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "ComponentFieldWrite value");
                }
                Stmt::ComponentEmit { args, .. } => {
                    for a in args {
                        self.check_expr(a, false, "ComponentEmit arg");
                    }
                }
                Stmt::ComponentCall { args, dest, .. } => {
                    for a in args {
                        self.check_expr(a, false, "ComponentCall arg");
                    }
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                }
                Stmt::SeqPush { seq, value } => {
                    self.check_local(*seq);
                    // The yielded value (a record `Local`) follows the
                    // no-inline-port rule like any host-state assignment.
                    self.check_expr(value, false, "SeqPush value");
                }
                Stmt::ComponentQueuePush { value, .. } => {
                    // Component-queue host state — the pushed value follows
                    // the no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "ComponentQueuePush value");
                }
                Stmt::ComponentQueuePop { dest, .. } => {
                    self.check_local(*dest);
                }
                // Whole sub-component value copy — receiver/source resolved
                // at lowering against the component schema; nothing to
                // verify structurally (no local/port dependency).
                Stmt::ComponentSubAssign { .. } => {}
                Stmt::TlmFork(desc) => {
                    if let Some(d) = desc.dest {
                        self.check_local(d);
                    }
                    // A fork is a bus-bound TLM seam, same resolution rules
                    // as a blocking Assign-RHS edge (Run/Check only, binding
                    // resolves on the owner tb, method exists, arg arity +
                    // purity). The args are no-inline-port.
                    self.check_bus_call_edge(&desc.bus_field, &desc.method, &desc.args);
                }
                Stmt::TlmJoinAll(pending) => {
                    for p in pending {
                        if let Some(d) = p.dest {
                            self.check_local(d);
                        }
                        self.check_bus_call_edge(&p.bus_field, &p.method, &p.args);
                    }
                }
            }
        }
        match &b.terminator {
            Terminator::Branch(c, _, _) => self.check_expr(c, false, "Branch cond"),
            Terminator::WaitCycles(e, _, _) => self.check_expr(e, false, "WaitCycles count"),
            Terminator::WaitCyclesSync(e, _) => self.check_expr(e, false, "WaitCycles count"),
            Terminator::WaitTimePs(..) => {}
            Terminator::WaitUntil { preds, .. } => {
                for p in preds {
                    self.check_expr(&p.expr, true, "WaitUntil pred");
                }
            }
            Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                for p in preds {
                    self.check_expr(&p.expr, true, "WaitUntilTimeout pred");
                }
                self.check_expr(cycles, false, "WaitUntilTimeout cycles");
            }
            Terminator::Randomize {
                target,
                constraints,
                ..
            } => {
                self.check_local(*target);
                // Target must be record-typed: the solver writes the
                // record's fields back into it.
                if let Some(l) = self.func.locals.get(target.index()) {
                    if !matches!(l.ty, IrType::Record(_)) {
                        self.errs.push(VerifyError::DanglingConstraintRef {
                            func: self.fid,
                            block: self.bid,
                            detail: format!("target local `{}` is not record-typed", l.name),
                        });
                    }
                }
                // Invariant 9: the ConstraintRef resolves.
                if constraints.index() >= self.prog.constraint_sites.len() {
                    self.errs.push(VerifyError::DanglingConstraintRef {
                        func: self.fid,
                        block: self.bid,
                        detail: format!("c{} out of range", constraints.0),
                    });
                }
            }
            Terminator::Fatal(args) => self.check_fmt_args(args),
            Terminator::Jump(_) | Terminator::Return => {}
        }
    }

    fn check_fmt_args(&mut self, args: &FmtArgs) {
        for a in &args.args {
            self.check_expr(&a.expr, true, "format arg");
        }
    }

    fn check_local(&mut self, l: LocalId) {
        if l.index() >= self.func.locals.len() {
            self.errs.push(VerifyError::BadLocal {
                func: self.fid,
                block: self.bid,
                local: l,
            });
        }
    }

    /// The owning testbench must declare scalar field `field`.
    fn check_tb_field(&mut self, field: &str) {
        let ok = self
            .func
            .owner
            .and_then(|tb| self.prog.testbenches.get(tb.index()))
            .is_some_and(|tb| {
                tb.state_fields.iter().any(|state| {
                    matches!(state, TbStateFieldSchema::Scalar(scalar) if scalar.name == field)
                })
            });
        if !ok {
            self.errs.push(VerifyError::BadTbField {
                func: self.fid,
                block: self.bid,
                field: field.to_string(),
            });
        }
    }

    /// The owning testbench must declare queue field `field`.
    fn check_tb_queue(&mut self, field: &str) {
        if self.tb_queue_elem(field).is_none() {
            self.errs.push(VerifyError::BadTbField {
                func: self.fid,
                block: self.bid,
                field: field.to_string(),
            });
        }
    }

    fn tb_queue_elem(&self, field: &str) -> Option<&QueueElem> {
        self.func
            .owner
            .and_then(|tb| self.prog.testbenches.get(tb.index()))
            .and_then(|tb| {
                tb.state_fields.iter().find_map(|state| match state {
                    TbStateFieldSchema::Queue(queue) if queue.name == field => Some(&queue.elem),
                    _ => None,
                })
            })
    }

    /// The scoreboard id must resolve and `field` must be a
    /// scoreboard-typed field of the owning testbench bound to it.
    fn check_scoreboard(&mut self, sb: crate::ir::ScoreboardId, field: &str, nested: bool) {
        if sb.index() >= self.prog.scoreboards.len() {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard id sb{} does not resolve", sb.0),
            });
            return;
        }
        // An env-nested data scoreboard (`top.sb`) is a sub-component of
        // the env local, not a testbench field — the binding check below
        // only applies to the `_tb.<field>` form. The sb id already
        // resolved above; that is sufficient for the nested form.
        if nested {
            return;
        }
        let bound = self
            .func
            .owner
            .and_then(|tb| self.prog.testbenches.get(tb.index()))
            .is_some_and(|tb| {
                tb.scoreboard_fields
                    .iter()
                    .any(|(f, id)| f == field && *id == sb)
            });
        if !bound {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!(
                    "field `{field}` is not bound to scoreboard sb{} on the owning testbench",
                    sb.0
                ),
            });
        }
    }

    fn check_scoreboard_scalar(&mut self, sb: crate::ir::ScoreboardId, scalar: &str) {
        let ok = self
            .prog
            .scoreboards
            .get(sb.index())
            .and_then(|s| s.field(scalar))
            .is_some_and(|f| matches!(f.kind, crate::ir::ScoreboardFieldKind::Scalar { .. }));
        if !ok {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard sb{} has no scalar field `{scalar}`", sb.0),
            });
        }
    }

    fn check_scoreboard_queue(&mut self, sb: crate::ir::ScoreboardId, queue: &str) {
        let ok = self
            .prog
            .scoreboards
            .get(sb.index())
            .and_then(|s| s.field(queue))
            .is_some_and(|f| matches!(f.kind, crate::ir::ScoreboardFieldKind::Queue { .. }));
        if !ok {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard sb{} has no queue field `{queue}`", sb.0),
            });
        }
    }

    /// `local` must be record-typed and its schema must declare `field`.
    /// `mid_positions` lists the segments (positions in `[field] ++ path`)
    /// that carry a `Vec<Record, N>` element selection.
    fn check_record_field(
        &mut self,
        local: LocalId,
        field: &str,
        path: &[String],
        mid_positions: &[usize],
    ) {
        // Resolve `field` then each `path` component against the nested
        // record schemas: a non-leaf component must reach a nested record
        // to descend into — a plain nested-record field (unindexed), or
        // one element of a `Vec<Record, N>` field (indexed); the leaf may
        // be any field but never carries a mid index. Fails on an unknown
        // field, a non-record intermediate, or an index/`Vec` mismatch.
        let ok = (|| -> Option<()> {
            let tl = self.func.locals.get(local.index())?;
            let mut rid = match tl.ty {
                IrType::Record(r) => r,
                _ => return None,
            };
            let segs: Vec<&str> = std::iter::once(field)
                .chain(path.iter().map(String::as_str))
                .collect();
            let last = segs.len() - 1;
            for (i, seg) in segs.iter().enumerate() {
                let fld = self.prog.records.get(rid.index())?.field(seg)?;
                let indexed = mid_positions.contains(&i);
                if i == last {
                    return (!indexed).then_some(());
                }
                match fld.ty {
                    IrType::Record(r) if fld.vec_len.is_none() == !indexed => rid = r,
                    _ => return None,
                }
            }
            Some(())
        })();
        if ok.is_none() {
            let mut dotted = field.to_string();
            for p in path {
                dotted.push('.');
                dotted.push_str(p);
            }
            self.errs.push(VerifyError::BadRecordField {
                func: self.fid,
                block: self.bid,
                local,
                field: dotted,
            });
        }
    }

    /// The `Stmt::TransactorCall` payload: must be a `TransactorMethod`
    /// call edge whose `bus_field`/`method` resolve through the owner
    /// testbench's transactor fields. Args follow the no-inline-ports
    /// rule (they are hoisted at lowering, like `Assign` values).
    fn check_transactor_call(&mut self, call: &Expr) {
        let (fid, bid) = (self.fid, self.bid);
        let bad = move |detail: String| VerifyError::BadTransactorCall {
            func: fid,
            block: bid,
            detail,
        };
        let Expr::Call(CallTarget::TransactorMethod { bus_field, method }, args) = call else {
            self.errs.push(bad(
                "payload is not a TransactorMethod call edge".to_string()
            ));
            return;
        };
        for a in args {
            self.check_expr(a, false, "TransactorCall arg");
        }
        let Some(owner) = self.func.owner else {
            self.errs.push(bad(format!(
                "`{bus_field}.{method}` called from a function with no owner testbench"
            )));
            return;
        };
        let Some(tb) = self.prog.testbenches.get(owner.index()) else {
            self.errs
                .push(bad(format!("owner tb{} does not resolve", owner.0)));
            return;
        };
        let Some((_, xid)) = tb.transactor_fields.iter().find(|(f, _)| f == bus_field) else {
            if tb.bus_bindings.iter().any(|b| &b.field == bus_field) {
                self.errs.push(bad(format!(
                    "`{bus_field}.{method}` names a bus binding but rides a \
                     Stmt::TransactorCall — bus-bound edges must be the entire \
                     RHS of an Assign"
                )));
            } else {
                self.errs.push(bad(format!(
                    "testbench `{}` has no transactor field `{bus_field}`",
                    tb.name
                )));
            }
            return;
        };
        let Some(schema) = self.prog.transactors.get(xid.index()) else {
            self.errs
                .push(bad(format!("transactor x{} does not resolve", xid.0)));
            return;
        };
        if schema.method(method).is_none() {
            self.errs.push(bad(format!(
                "transactor `{}` has no method `{method}`",
                schema.name
            )));
        }
    }

    /// The payload of an event-channel local, or `None` when the local
    /// does not resolve or is not event-typed.
    fn event_payload(&self, l: LocalId) -> Option<crate::ir::EventPayload> {
        match self.func.locals.get(l.index()).map(|t| &t.ty) {
            Some(IrType::Event(p)) => Some(*p),
            _ => None,
        }
    }

    fn check_covgroup(&mut self, c: CovgroupId) {
        if c.index() >= self.prog.covgroups.len() {
            self.errs.push(VerifyError::BadCovgroup {
                func: self.fid,
                block: self.bid,
                covgroup: c,
            });
        }
    }

    fn check_expr(&mut self, e: &Expr, ports_ok: bool, context: &'static str) {
        match e {
            Expr::Literal { .. } | Expr::WideLiteral(_) => {}
            // The global cycle counter — a framework value, no
            // local/port dependency to verify.
            Expr::CycleCount | Expr::ErrorCount => {}
            Expr::Local(l) => self.check_local(*l),
            Expr::TbField(field) => self.check_tb_field(field),
            // A temporal latch reading only has meaning inside the
            // per-cycle closure that owns the `static` latch cells —
            // i.e. inside a `PropertyCheckSchema`/`CoverCheckSchema`
            // body, which is verified by `check_concurrent_checks`
            // rather than walked as part of a function body.
            Expr::TemporalSlot { slot, .. } => {
                self.errs.push(VerifyError::BadConcurrentCheck {
                    func: self.fid,
                    block: self.bid,
                    detail: format!(
                        "temporal slot {slot} referenced in {context}, outside a \
                         concurrent property/cover body"
                    ),
                });
            }
            Expr::TbQueueQuery { field, query } => {
                self.check_tb_queue(field);
                match query {
                    ScoreboardQuery::QueueSize { queue }
                    | ScoreboardQuery::QueueEmpty { queue }
                        if queue == field => {}
                    _ => self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} has malformed query metadata for testbench queue `{field}`",
                            self.fid.0, self.bid.0
                        ),
                    }),
                }
            }
            // Transactor-instance state — host state, resolved at
            // lowering against the bound instance; nothing to verify
            // structurally here (no local/port dependency).
            Expr::TransactorState { .. } => {}
            Expr::TransactorStateRecordField { .. } => {}
            Expr::TransactorStateQueueQuery { .. } => {}
            Expr::Port(_) => {
                if !ports_ok {
                    self.errs.push(VerifyError::PortInDisallowedPosition {
                        func: self.fid,
                        block: self.bid,
                        context,
                    });
                }
            }
            Expr::Binary(_, a, b) => {
                self.check_expr(a, ports_ok, context);
                self.check_expr(b, ports_ok, context);
            }
            Expr::Unary(_, a) => self.check_expr(a, ports_ok, context),
            Expr::BitSlice { target, .. } => self.check_expr(target, ports_ok, context),
            Expr::BitSliceDyn { target, hi, lo } => {
                self.check_expr(target, ports_ok, context);
                self.check_expr(hi, ports_ok, context);
                self.check_expr(lo, ports_ok, context);
            }
            Expr::Ternary(c, t, e2) => {
                self.check_expr(c, ports_ok, context);
                self.check_expr(t, ports_ok, context);
                self.check_expr(e2, ports_ok, context);
            }
            Expr::WidthCast {
                width,
                src_width,
                inner,
                ..
            } => {
                // `width` is the cast *destination* and carries the
                // width-method language limit lowering enforces. `src_width`
                // is best-effort receiver metadata read off a declared type,
                // and declared widths are not bounded by that limit —
                // `let big : uint<2048>` narrowed by `big.trunc<64>()` is a
                // legal program — so only a zero-width source is malformed
                // here (lowering reports an unusable declared width as
                // `None`, never `Some(0)`).
                if *width == 0
                    || *width > crate::MAX_WIDTH_METHOD_BITS
                    || src_width.is_some_and(|w| w == 0)
                {
                    self.errs.push(VerifyError::BadWidthCast {
                        func: self.fid,
                        block: self.bid,
                        width: *width,
                        src_width: *src_width,
                    });
                }
                self.check_expr(inner, ports_ok, context);
            }
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } => {
                self.check_local(*local);
                let mid_positions: Vec<usize> = mid_indices.iter().map(|(p, _)| *p).collect();
                self.check_record_field(*local, field, path, &mid_positions);
                for (_, idx) in mid_indices {
                    self.check_expr(idx, ports_ok, context);
                }
                if let Some(idx) = index {
                    self.check_expr(idx, ports_ok, context);
                }
            }
            // Register-level frontdoor read in expression position. The
            // mirror is a record local; the register name must be one of
            // its fields. The helper read is a plain lambda call (not the
            // TLM seam), so it is a legitimate sub-expression value —
            // nothing in the seam rule forbids it here.
            Expr::RegRead { mirror, field, .. } => {
                self.check_local(*mirror);
                self.check_record_field(*mirror, field, &[], &[]);
            }
            Expr::CovBin { inst, .. } => self.check_covgroup(inst.covgroup),
            // A hook-param cover target carries the parameter NAME (no
            // resolvable local before the transactor pass); only its
            // optional index sub-expression needs checking.
            Expr::CovHookParam { index, .. } => {
                if let Some(i) = index {
                    self.check_expr(i, ports_ok, context);
                }
            }
            Expr::CovHookArg { .. } => {}
            // Component host state — resolved at lowering against the
            // component schema; no local/port dependency to verify here.
            Expr::ComponentField { .. } => {}
            // A by-value component passed as a method arg. A `Local` base
            // is a method-param local (verify it is defined); a
            // `SelfField`/`Path` base is resolved at lowering.
            Expr::ComponentValue { base } => {
                if let crate::ir::ComponentBase::Local(l) = base {
                    self.check_local(*l);
                }
            }
            // Component-queue size/empty read — host state resolved at
            // lowering against the component schema; nothing to verify.
            Expr::ComponentQueueQuery { .. } => {}
            // Idle predicate: the base/kind are resolved at lowering; only
            // the threshold sub-expression carries verifiable structure.
            Expr::ComponentIdle { n, .. } => self.check_expr(n, ports_ok, context),
            Expr::ScoreboardQuery {
                sb,
                field,
                query,
                nested_path,
            } => {
                self.check_scoreboard(*sb, field, nested_path.is_some());
                match query {
                    crate::ir::ScoreboardQuery::Scalar { scalar } => {
                        self.check_scoreboard_scalar(*sb, scalar)
                    }
                    crate::ir::ScoreboardQuery::QueueSize { queue }
                    | crate::ir::ScoreboardQuery::QueueEmpty { queue } => {
                        self.check_scoreboard_queue(*sb, queue)
                    }
                }
            }
            // Sequence length: the seq local must resolve. Host state —
            // no port/value dependency beyond the local.
            Expr::SeqLen(seq) => self.check_local(*seq),
            // Sequence element read (`seq[i]`): the seq local must resolve;
            // the index follows the same port rules as the surrounding
            // context.
            Expr::SeqIndex { seq, index } => {
                self.check_local(*seq);
                self.check_expr(index, ports_ok, context);
            }
            Expr::Call(target, args) => {
                // Seam rule: a call edge is never an expression VALUE.
                // It reaches the verifier only as the top-level Assign
                // RHS (bus) or the root payload of `Stmt::TransactorCall`
                // (transactor) — both consumed by `check_block` before
                // recursing. Reaching one here means it is nested or in
                // a disallowed statement position.
                if let CallTarget::TransactorMethod { bus_field, method } = target {
                    self.errs.push(VerifyError::BadTransactorCall {
                        func: self.fid,
                        block: self.bid,
                        detail: format!(
                            "`{bus_field}.{method}` call edge in a disallowed position \
                             ({context}) — must be the entire RHS of an Assign (bus) \
                             or the payload of a Stmt::TransactorCall (transactor)"
                        ),
                    });
                }
                if let CallTarget::TransactorSelfMethod { transactor, method } = target {
                    self.errs.push(VerifyError::BadTransactorCall {
                        func: self.fid,
                        block: self.bid,
                        detail: format!(
                            "`{transactor}.{method}` sibling call in a disallowed position \
                             ({context}) — lowering must hoist it into a \
                             Stmt::TransactorSelfCall"
                        ),
                    });
                }
                for a in args {
                    self.check_expr(a, ports_ok, context);
                }
            }
        }
    }

    /// Validate one sibling method call inside a DUT-poking transactor
    /// method body. These calls are synchronous lambda calls, not
    /// testbench-field call edges, so they are only legal in a
    /// `TransactorBody` and resolve against that body's transactor
    /// schema.
    fn check_transactor_self_call(&mut self, dest: Option<LocalId>, call: &Expr) {
        let (fid, bid) = (self.fid, self.bid);
        let bad = move |detail: String| VerifyError::BadTransactorCall {
            func: fid,
            block: bid,
            detail,
        };
        let Expr::Call(CallTarget::TransactorSelfMethod { transactor, method }, args) = call else {
            self.errs
                .push(bad("payload is not a TransactorSelfMethod call".to_string()));
            return;
        };
        for a in args {
            self.check_expr(a, false, "TransactorSelfCall arg");
        }
        let FunctionKind::TransactorBody { transactor: xid } = self.func.kind else {
            self.errs.push(bad(format!(
                "`{transactor}.{method}` sibling call outside a transactor method body"
            )));
            return;
        };
        let Some(schema) = self.prog.transactors.get(xid.index()) else {
            self.errs
                .push(bad(format!("transactor t{} does not resolve", xid.0)));
            return;
        };
        if schema.name != *transactor {
            self.errs.push(bad(format!(
                "sibling call names transactor `{transactor}` from `{}` body",
                schema.name
            )));
            return;
        }
        let Some(m) = schema.method(method) else {
            self.errs.push(bad(format!(
                "transactor `{}` has no sibling method `{method}`",
                schema.name
            )));
            return;
        };
        if args.len() != m.n_params {
            self.errs.push(bad(format!(
                "transactor method `{}.{method}` takes {} argument(s), call passes {}",
                schema.name,
                m.n_params,
                args.len()
            )));
        }
        if dest.is_some() && !m.has_ret {
            self.errs.push(bad(format!(
                "void transactor method `{}.{method}` captured into a destination",
                schema.name
            )));
        }
    }

    fn bad_transactor(&mut self, detail: String) {
        self.errs.push(VerifyError::BadTransactorCall {
            func: self.fid,
            block: self.bid,
            detail,
        });
    }

    /// Validate one sanctioned bus-bound `TransactorMethod` call edge
    /// (Assign-RHS position): function kind, bus-binding resolution on
    /// the owning testbench, method existence, arity, and argument
    /// purity (no ports, no nesting). Transactor-field edges never take
    /// this position — they ride `Stmt::TransactorCall` and are checked
    /// by `check_transactor_call`.
    fn check_bus_call_edge(&mut self, bus_field: &str, method: &str, args: &[Expr]) {
        // A `TransactorBody` function may carry a downstream blocking
        // bus-call edge when it is a bound-to target responder
        // re-issuing a TLM call (nested forwarding). The responder body
        // is lowered standalone (no owner testbench), so the binding's
        // wire names cannot be resolved here — emission resolves the edge
        // against the binding testbench's `bus_bindings` (raising an
        // EmitError if the downstream binding is absent). Only argument
        // purity is checked here (below); the Run/Check resolution arm is
        // skipped for the owner-less responder case.
        if matches!(self.func.kind, FunctionKind::TransactorBody { .. })
            && self.func.owner.is_none()
        {
            // Downstream forwarding edge — defer wire resolution to emit.
        } else if !matches!(self.func.kind, FunctionKind::Run | FunctionKind::Check) {
            self.bad_transactor(format!(
                "`{bus_field}.{method}` call edge in a {:?}-kind function \
                 (allowed only in Run/Check bodies or a bound-to responder \
                 forwarding a downstream call)",
                self.func.kind
            ));
        } else {
            let owner_tb = self
                .func
                .owner
                .and_then(|tb| self.prog.testbenches.get(tb.index()));
            let binding =
                owner_tb.and_then(|tb| tb.bus_bindings.iter().find(|b| b.field == bus_field));
            let diag = match binding {
                None if owner_tb
                    .is_some_and(|tb| tb.transactor_fields.iter().any(|(f, _)| f == bus_field)) =>
                {
                    Some(format!(
                        "`{bus_field}.{method}` names a transactor field but rides an \
                         Assign RHS — transactor-bound edges must be a \
                         Stmt::TransactorCall payload"
                    ))
                }
                None => Some(format!(
                    "`{bus_field}.{method}` does not resolve: owning testbench has no \
                     bus binding `{bus_field}`"
                )),
                Some(b) => match b.methods.iter().find(|m| m.name == method) {
                    None => Some(format!(
                        "bus `{}` (binding `{bus_field}`) has no tlm_method `{method}`",
                        b.bus
                    )),
                    Some(m) if m.args.len() != args.len() => Some(format!(
                        "`{bus_field}.{method}` arity mismatch: schema declares {} \
                         arg(s), call carries {}",
                        m.args.len(),
                        args.len()
                    )),
                    Some(_) => None,
                },
            };
            if let Some(what) = diag {
                self.bad_transactor(what);
            }
        }
        for a in args {
            self.check_expr(a, false, "TransactorMethod arg");
        }
    }
}

/// Best-effort expression typing for invariant 15. Returns `None` when
/// the expression's type cannot be locally determined.
fn expr_type(func: &TbFunction, e: &Expr) -> Option<IrType> {
    match e {
        Expr::Literal { ty, .. } => Some(ty.clone()),
        Expr::WideLiteral(words) => Some(IrType::UInt(Some(wide_literal_bits(words)))),
        Expr::Local(l) => func.locals.get(l.index()).map(|t| t.ty.clone()),
        Expr::BitSlice { hi, lo, .. } => Some(IrType::UInt(Some(hi - lo + 1))),
        // Runtime bounds: unsigned, width unknown until the slice runs.
        // `UInt(None)` is invariant 15's widthless wildcard, which is
        // what a `uint64_t` helper return is here.
        Expr::BitSliceDyn { .. } => Some(IrType::UInt(None)),
        Expr::WidthCast { kind, width, .. } => Some(match kind {
            crate::ir::WidthCastKind::Sext => IrType::SInt(Some(*width)),
            _ => IrType::UInt(Some(*width)),
        }),
        _ => None,
    }
}

/// Queue elements erase scalar widths but retain signedness and record
/// identity. `Unknown` is the inferred type of an unannotated scalar pop,
/// which is emitted as the queue's runtime scalar representation.
fn queue_elem_matches_type(elem: &QueueElem, ty: &IrType) -> bool {
    match (elem, ty) {
        (_, IrType::Unknown) => true,
        (QueueElem::Scalar { signed: true }, IrType::SInt(_)) => true,
        (QueueElem::Scalar { signed: false }, IrType::UInt(_) | IrType::Bool) => true,
        (QueueElem::Record(expected), IrType::Record(actual)) => expected == actual,
        _ => false,
    }
}

fn assign_compatible(expected: &IrType, actual: &IrType) -> bool {
    if expected == actual {
        return true;
    }
    // A widthless scalar (`UInt(None)` / `SInt(None)`) is signedness
    // metadata on a 64-bit value with no declared width — file-scope
    // const / enum-variant substitution emits these (#525). For width
    // compatibility it is the same wildcard `Unknown` was before the
    // substitution carried signedness: assignable into (and from) any
    // scalar local, exactly the pre-#525 accepted set.
    let widthless = |t: &IrType| matches!(t, IrType::UInt(None) | IrType::SInt(None));
    if widthless(expected) || widthless(actual) {
        return true;
    }
    match (expected, actual) {
        (IrType::UInt(Some(ew)), IrType::UInt(Some(aw)))
        | (IrType::SInt(Some(ew)), IrType::SInt(Some(aw))) => aw <= ew,
        (IrType::UInt(Some(ew)), IrType::Bool) | (IrType::SInt(Some(ew)), IrType::Bool) => *ew >= 1,
        _ => false,
    }
}

fn wide_literal_bits(words: &[u32]) -> u32 {
    let Some((idx, word)) = words.iter().enumerate().rev().find(|(_, w)| **w != 0) else {
        return 1;
    };
    (idx as u32) * 32 + (32 - word.leading_zeros())
}

fn bit_words(nbits: usize) -> usize {
    nbits.div_ceil(64)
}

fn full_bits(nbits: usize) -> Vec<u64> {
    let words = bit_words(nbits);
    let mut bits = vec![!0u64; words];
    let rem = nbits % 64;
    if rem != 0 {
        if let Some(last) = bits.last_mut() {
            *last = (1u64 << rem) - 1;
        }
    }
    bits
}

fn zero_bits(nbits: usize) -> Vec<u64> {
    vec![0u64; bit_words(nbits)]
}

fn bit_get(bits: &[u64], idx: usize) -> bool {
    bits.get(idx / 64)
        .is_some_and(|word| (word & (1u64 << (idx % 64))) != 0)
}

fn bit_set(bits: &mut [u64], idx: usize) {
    if let Some(word) = bits.get_mut(idx / 64) {
        *word |= 1u64 << (idx % 64);
    }
}

fn bit_or_assign(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d |= *s;
    }
}

fn bit_and_assign(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d &= *s;
    }
}

/// Invariant 4 — iterative forward dataflow over "definitely defined"
/// local sets. `defined_in[b]` = intersection of predecessors' outs;
/// a read inside the block must be covered by the running defined set.
fn check_def_before_use(
    func: &TbFunction,
    fid: FunctionId,
    reachable: &[bool],
    errs: &mut Vec<VerifyError>,
) {
    let nlocals = func.locals.len();
    let nblocks = func.blocks.len();
    if nblocks == 0 {
        return;
    }
    let full = full_bits(nlocals);
    // Params count as defined at entry: by convention the first
    // `params.len()` locals mirror the function's parameters.
    let mut entry_in = zero_bits(nlocals);
    for i in 0..func.params.len().min(nlocals) {
        bit_set(&mut entry_in, i);
    }
    // A `RecordSeq` accumulator (the tseq `ret` slot) is always
    // default-constructed by the backend at function top — `declare_locals`
    // emits `std::vector<Record> r{};` — so it is live from entry. Mark it
    // defined so the `yield`/`SeqPush` accumulator read never trips
    // use-before-def.
    for (i, l) in func.locals.iter().enumerate() {
        if matches!(l.ty, IrType::RecordSeq(_) | IrType::Seq(_)) {
            bit_set(&mut entry_in, i);
        }
    }
    let mut ins: Vec<Vec<u64>> = vec![full.clone(); nblocks];
    ins[func.entry.index()] = entry_in.clone();

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nblocks];
    for (bi, b) in func.blocks.iter().enumerate() {
        for s in b.terminator.successors() {
            preds[s.index()].push(bi);
        }
    }

    let mut gens = vec![zero_bits(nlocals); nblocks];
    for (bi, b) in func.blocks.iter().enumerate() {
        for s in &b.stmts {
            match s {
                Stmt::Assign(l, _) | Stmt::DutRead(l, _) | Stmt::RecordInit(l, _) => {
                    bit_set(&mut gens[bi], l.index());
                }
                Stmt::TransactorCall { dest: Some(l), .. } => {
                    bit_set(&mut gens[bi], l.index());
                }
                Stmt::TransactorSelfCall { dest: Some(l), .. } => {
                    bit_set(&mut gens[bi], l.index());
                }
                Stmt::ScoreboardOp {
                    op: crate::ir::ScoreboardOp::QueuePop { dest: l, .. },
                    ..
                }
                | Stmt::ComponentCall { dest: Some(l), .. }
                | Stmt::ComponentQueuePop { dest: l, .. }
                | Stmt::TransactorStateQueuePop { dest: l, .. }
                | Stmt::TbQueuePop { dest: l, .. } => {
                    bit_set(&mut gens[bi], l.index());
                }
                _ => {}
            }
        }
    }

    // Fixpoint.
    loop {
        let mut changed = false;
        for bi in 0..nblocks {
            if !reachable[bi] {
                continue;
            }
            let new_in = if bi == func.entry.index() {
                entry_in.clone()
            } else {
                let mut acc = full.clone();
                let mut out = zero_bits(nlocals);
                let mut any = false;
                for &p in &preds[bi] {
                    if !reachable[p] {
                        continue;
                    }
                    any = true;
                    out.clone_from(&ins[p]);
                    bit_or_assign(&mut out, &gens[p]);
                    bit_and_assign(&mut acc, &out);
                }
                if !any {
                    zero_bits(nlocals)
                } else {
                    acc
                }
            };
            if new_in != ins[bi] {
                ins[bi] = new_in;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Walk each reachable block statement-by-statement, reporting reads
    // not covered by the running defined set.
    for (bi, b) in func.blocks.iter().enumerate() {
        if !reachable[bi] {
            continue;
        }
        let bid = BlockId(bi as u32);
        let mut defined = ins[bi].clone();
        let check_e = |e: &Expr, defined: &[u64], errs: &mut Vec<VerifyError>| {
            for_each_local(e, &mut |l| {
                if l.index() < nlocals && !bit_get(defined, l.index()) {
                    errs.push(VerifyError::LocalUseBeforeDef {
                        func: fid,
                        block: bid,
                        local: l,
                    });
                }
            });
        };
        for s in &b.stmts {
            match s {
                Stmt::Assign(l, e) => {
                    check_e(e, &defined, errs);
                    bit_set(&mut defined, l.index());
                }
                Stmt::DutRead(l, _) | Stmt::RecordInit(l, _) => {
                    bit_set(&mut defined, l.index());
                }
                Stmt::RecordFieldWrite { local, value, .. }
                | Stmt::RecordWriteCb { local, value, .. } => {
                    // Writing a field READS the record local (it must
                    // be initialized first — RecordInit defines it).
                    if local.index() < nlocals && !bit_get(&defined, local.index()) {
                        errs.push(VerifyError::LocalUseBeforeDef {
                            func: fid,
                            block: bid,
                            local: *local,
                        });
                    }
                    check_e(value, &defined, errs);
                }
                Stmt::TbFieldWrite { value, .. } | Stmt::TbQueuePush { value, .. } => {
                    check_e(value, &defined, errs)
                }
                Stmt::TbQueuePop { dest, .. } => bit_set(&mut defined, dest.index()),
                Stmt::TransactorStateWrite { value, .. } => check_e(value, &defined, errs),
                Stmt::TransactorStateRecordFieldWrite { value, .. } => {
                    check_e(value, &defined, errs)
                }
                Stmt::TransactorStateQueuePush { value, .. } => check_e(value, &defined, errs),
                Stmt::TransactorStateQueuePop { dest, .. } => {
                    // Pop defines the destination local.
                    bit_set(&mut defined, dest.index());
                }
                Stmt::DutWrite(_, e) => check_e(e, &defined, errs),
                Stmt::TransactorCall { dest, call } => {
                    check_e(call, &defined, errs);
                    if let Some(l) = dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::TransactorSelfCall { dest, call } => {
                    check_e(call, &defined, errs);
                    if let Some(l) = dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::Log { args, .. } => {
                    for a in &args.args {
                        check_e(&a.expr, &defined, errs);
                    }
                }
                Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail } => {
                    check_e(cond, &defined, errs);
                    for a in &on_fail.args {
                        check_e(&a.expr, &defined, errs);
                    }
                }
                Stmt::CovReport(_) => {}
                // Concurrent-check bodies reference DUT ports and host
                // state, never function locals — nothing to def-check.
                Stmt::PropertyCheck(_) | Stmt::CoverCheck(_) | Stmt::CycleHandler(_) => {}
                // The channel local is DEFINED by its declaration (the
                // emitter declares the subscriber vector at the hoisted
                // local site), so subscribing/emitting only reads it —
                // and reading it is not an expression, so there is
                // nothing for `check_e` to walk. The payload args are.
                Stmt::EventSubscribe { .. } => {}
                Stmt::EventEmit { args, .. } => {
                    for a in args {
                        check_e(a, &defined, errs);
                    }
                }
                Stmt::ProbeRelease(_) => {}
                Stmt::FailDiag { guard, args } => {
                    if let Some(g) = guard {
                        check_e(g, &defined, errs);
                    }
                    for a in &args.args {
                        check_e(&a.expr, &defined, errs);
                    }
                }
                Stmt::ScoreboardOp { op, .. } => match op {
                    crate::ir::ScoreboardOp::QueuePush { value, .. } => {
                        check_e(value, &defined, errs)
                    }
                    crate::ir::ScoreboardOp::ScalarWrite { value, .. } => {
                        check_e(value, &defined, errs)
                    }
                    crate::ir::ScoreboardOp::QueuePop { dest, .. } => {
                        bit_set(&mut defined, dest.index());
                    }
                },
                Stmt::ComponentFieldWrite { value, .. } => check_e(value, &defined, errs),
                Stmt::ComponentEmit { args, .. } => {
                    for a in args {
                        check_e(a, &defined, errs);
                    }
                }
                Stmt::ComponentCall { args, dest, .. } => {
                    for a in args {
                        check_e(a, &defined, errs);
                    }
                    if let Some(l) = dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::SeqPush { seq, value } => {
                    // `yield t` reads both the accumulator (defined at the
                    // tseq function entry) and the yielded value.
                    if seq.index() < nlocals && !bit_get(&defined, seq.index()) {
                        errs.push(VerifyError::LocalUseBeforeDef {
                            func: fid,
                            block: bid,
                            local: *seq,
                        });
                    }
                    check_e(value, &defined, errs);
                }
                Stmt::ComponentQueuePush { value, .. } => check_e(value, &defined, errs),
                Stmt::ComponentQueuePop { dest, .. } => {
                    // Pop defines the destination local.
                    bit_set(&mut defined, dest.index());
                }
                // Whole sub-component copy — no local def/use (both ends
                // are component values, not test locals).
                Stmt::ComponentSubAssign { .. } => {}
                Stmt::TlmFork(desc) => {
                    // Args read at the fork site; the dest is defined here
                    // (v1 declares + zero-inits `T x = {};` at the fork,
                    // so reads between fork and join_all see a defined
                    // local), and re-assigned at the matching join_all.
                    for a in &desc.args {
                        check_e(a, &defined, errs);
                    }
                    if let Some(l) = desc.dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::TlmJoinAll(pending) => {
                    for p in pending {
                        if let Some(l) = p.dest {
                            bit_set(&mut defined, l.index());
                        }
                    }
                }
            }
        }
        match &b.terminator {
            Terminator::Branch(c, _, _) => check_e(c, &defined, errs),
            Terminator::WaitCycles(e, _, _) => check_e(e, &defined, errs),
            Terminator::WaitCyclesSync(e, _) => check_e(e, &defined, errs),
            Terminator::WaitTimePs(..) => {}
            Terminator::WaitUntil { preds, .. } => {
                for p in preds {
                    check_e(&p.expr, &defined, errs);
                }
            }
            Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                for p in preds {
                    check_e(&p.expr, &defined, errs);
                }
                check_e(cycles, &defined, errs);
            }
            Terminator::Fatal(args) => {
                for a in &args.args {
                    check_e(&a.expr, &defined, errs);
                }
            }
            Terminator::Randomize { target, .. } => {
                // The solver writes the record fields back into `target`;
                // it is a def, not a use (the record local was already
                // defined at its `let` RecordInit site).
                bit_set(&mut defined, target.index());
            }
            Terminator::Jump(_) | Terminator::Return => {}
        }
    }
}

fn for_each_local(e: &Expr, f: &mut impl FnMut(LocalId)) {
    match e {
        Expr::Literal { .. }
        | Expr::WideLiteral(_)
        | Expr::CycleCount
        | Expr::ErrorCount
        | Expr::Port(_)
        | Expr::TbField(_)
        | Expr::TemporalSlot { .. }
        | Expr::TbQueueQuery { .. }
        | Expr::TransactorState { .. }
        | Expr::TransactorStateRecordField { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::ComponentField { .. }
        | Expr::ScoreboardQuery { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::CovHookArg { .. } => {}
        Expr::ComponentValue { base } => {
            if let crate::ir::ComponentBase::Local(l) = base {
                f(*l);
            }
        }
        Expr::Local(l) => f(*l),
        Expr::RecordField {
            local,
            mid_indices,
            index,
            ..
        } => {
            f(*local);
            for (_, idx) in mid_indices {
                for_each_local(idx, f);
            }
            if let Some(idx) = index {
                for_each_local(idx, f);
            }
        }
        // The mirror record local is both used (read) and written (the
        // inline assignment-expression predict), but it was defined at
        // its `let` RecordInit site upstream — record it as a use.
        Expr::RegRead { mirror, .. } => f(*mirror),
        Expr::Binary(_, a, b) => {
            for_each_local(a, f);
            for_each_local(b, f);
        }
        Expr::Unary(_, a) => for_each_local(a, f),
        Expr::BitSlice { target, .. } => for_each_local(target, f),
        Expr::BitSliceDyn { target, hi, lo } => {
            for_each_local(target, f);
            for_each_local(hi, f);
            for_each_local(lo, f);
        }
        Expr::Ternary(c, t, e) => {
            for_each_local(c, f);
            for_each_local(t, f);
            for_each_local(e, f);
        }
        Expr::WidthCast { inner, .. } => for_each_local(inner, f),
        Expr::ComponentIdle { n, .. } => for_each_local(n, f),
        Expr::CovBin { .. } => {}
        Expr::CovHookParam { index, .. } => {
            if let Some(i) = index {
                for_each_local(i, f);
            }
        }
        Expr::SeqLen(l) => f(*l),
        Expr::SeqIndex { seq, index } => {
            f(*seq);
            for_each_local(index, f);
        }
        Expr::Call(_, args) => {
            for a in args {
                for_each_local(a, f);
            }
        }
    }
}
