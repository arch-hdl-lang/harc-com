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
use std::collections::HashSet;

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
    /// A printf-style interpolation capture must use the scalar formatting
    /// ABI; aggregate values cannot be converted by `harc_printf_ll`.
    BadFormatArg {
        func: FunctionId,
        block: BlockId,
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
            VerifyError::BadFormatArg {
                func,
                block,
                actual,
            } => write!(
                f,
                "fn{}: b{} uses aggregate {:?} as a scalar format argument",
                func.0, block.0, actual
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
        Expr::ComponentIdle { n, .. } | Expr::TransactorIdle { n, .. } => {
            check_temporal_slots(n, n_slots, what, errs)
        }
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

fn cover_expr_type_hint(expr: &Expr) -> Option<IrType> {
    match expr {
        Expr::Literal { ty, .. } => Some(ty.clone()),
        Expr::WideLiteral(words) => Some(IrType::UInt(Some(wide_literal_bits(words)))),
        Expr::Port(port) => Some(IrType::UInt(port.width)),
        Expr::Unary(crate::ir::UnOp::Not, _) => Some(IrType::Bool),
        Expr::Unary(crate::ir::UnOp::BitNotHost, _) => Some(IrType::SInt(None)),
        Expr::Unary(_, inner) => cover_expr_type_hint(inner),
        Expr::Binary(op, lhs, rhs) => match op {
            crate::ir::BinOp::Eq
            | crate::ir::BinOp::Ne
            | crate::ir::BinOp::Lt
            | crate::ir::BinOp::Le
            | crate::ir::BinOp::Gt
            | crate::ir::BinOp::Ge
            | crate::ir::BinOp::And
            | crate::ir::BinOp::Or => Some(IrType::Bool),
            crate::ir::BinOp::Shl | crate::ir::BinOp::Shr => cover_expr_type_hint(lhs),
            _ => cover_common_type_hint(lhs, rhs),
        },
        Expr::Ternary(_, then_expr, else_expr) => cover_common_type_hint(then_expr, else_expr),
        Expr::BitSlice { hi, lo, .. } => Some(IrType::UInt(Some(hi - lo + 1))),
        Expr::BitSliceDyn { .. } => Some(IrType::UInt(None)),
        Expr::WidthCast { kind, width, .. } => Some(match kind {
            crate::ir::WidthCastKind::Sext => IrType::SInt(Some(*width)),
            _ => IrType::UInt(Some(*width)),
        }),
        Expr::Call(CallTarget::Helper { ret, .. } | CallTarget::ExternFn { ret, .. }, _) => {
            Some(ret.clone())
        }
        _ => None,
    }
}

fn cover_common_type_hint(lhs: &Expr, rhs: &Expr) -> Option<IrType> {
    let lhs = cover_expr_type_hint(lhs)?;
    let rhs = cover_expr_type_hint(rhs)?;
    Some(match (lhs, rhs) {
        (IrType::SInt(Some(lhs)), IrType::SInt(Some(rhs))) => IrType::SInt(Some(lhs.max(rhs))),
        (IrType::UInt(Some(lhs)), IrType::UInt(Some(rhs))) => IrType::UInt(Some(lhs.max(rhs))),
        (IrType::SInt(Some(lhs)), IrType::UInt(Some(rhs)))
        | (IrType::UInt(Some(lhs)), IrType::SInt(Some(rhs))) => IrType::UInt(Some(lhs.max(rhs))),
        (IrType::SInt(_), IrType::SInt(_)) => IrType::SInt(None),
        (IrType::Bool, IrType::Bool) => IrType::Bool,
        _ => IrType::UInt(None),
    })
}

fn cover_scalar_type(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown
    )
}

fn helper_abi_type_valid(ty: &IrType, record_count: usize) -> bool {
    match ty {
        IrType::FixedVec { elem, len } if *len != 0 => match elem.as_ref() {
            IrType::Record(record) => record.index() < record_count,
            IrType::FixedVec { .. } => false,
            scalar => fixed_vec_elem_valid(scalar),
        },
        other => cover_scalar_type(other),
    }
}

fn cover_call_compatible(expected: &IrType, actual: &IrType) -> bool {
    if matches!(expected, IrType::FixedVec { .. })
        || matches!(actual, IrType::FixedVec { .. })
    {
        return expected == actual;
    }
    cover_scalar_type(expected)
        && cover_scalar_type(actual)
        && (matches!(expected, IrType::Unknown)
            || matches!(actual, IrType::Unknown)
            || assign_compatible(expected, actual))
}

fn check_cover_expr(
    prog: &TbProgram,
    covgroup: usize,
    what: &str,
    hook_params: &[String],
    expr: &Expr,
    errs: &mut Vec<VerifyError>,
) {
    let bad = |detail: String, errs: &mut Vec<VerifyError>| {
        errs.push(VerifyError::BadProgramRef {
            what: format!("cg{covgroup} {what}: {detail}"),
        });
    };
    let recurse = |child: &Expr, errs: &mut Vec<VerifyError>| {
        check_cover_expr(prog, covgroup, what, hook_params, child, errs)
    };
    match expr {
        Expr::Literal { .. } => {}
        Expr::WideLiteral(words) => {
            if words.len() <= 2 {
                bad(
                    "wide literal must contain more than two 32-bit words".to_string(),
                    errs,
                );
            }
        }
        Expr::Port(port) => {
            if let Some(LaneIndex::Var(index)) = &port.lane {
                recurse(index, errs);
            }
        }
        Expr::Binary(_, lhs, rhs) => {
            recurse(lhs, errs);
            recurse(rhs, errs);
        }
        Expr::Unary(_, inner) => recurse(inner, errs),
        Expr::Ternary(cond, then_expr, else_expr) => {
            recurse(cond, errs);
            recurse(then_expr, errs);
            recurse(else_expr, errs);
        }
        Expr::BitSlice { target, hi, lo } => {
            recurse(target, errs);
            if hi < lo {
                bad(
                    format!("constant bit slice has reversed bounds [{hi}:{lo}]"),
                    errs,
                );
            }
        }
        Expr::BitSliceDyn { target, hi, lo } => {
            recurse(target, errs);
            recurse(hi, errs);
            recurse(lo, errs);
        }
        Expr::WidthCast {
            width,
            src_width,
            inner,
            ..
        } => {
            recurse(inner, errs);
            if *width == 0
                || *width > crate::MAX_WIDTH_METHOD_BITS
                || src_width.is_some_and(|w| w == 0)
            {
                bad(
                    format!("width cast has invalid destination {width} or source {src_width:?}"),
                    errs,
                );
            }
        }
        Expr::CovHookArg { param } | Expr::CovHookParam { param, .. }
            if !hook_params.contains(param) =>
        {
            bad(format!("references unknown hook parameter `{param}`"), errs);
        }
        Expr::CovHookParam {
            index: Some(index), ..
        } => recurse(index, errs),
        Expr::CovHookArg { .. } | Expr::CovHookParam { index: None, .. } => {}
        Expr::Call(CallTarget::Helper { name, ret }, args) => {
            let helper = prog
                .functions
                .iter()
                .find(|f| f.kind == FunctionKind::Helper && f.name == *name);
            let Some(helper) = helper else {
                bad(format!("references missing helper `{name}`"), errs);
                for arg in args {
                    recurse(arg, errs);
                }
                return;
            };
            if helper.params.len() != args.len() {
                bad(
                    format!(
                        "helper `{name}` arity mismatch: function has {}, call carries {}",
                        helper.params.len(),
                        args.len()
                    ),
                    errs,
                );
            }
            for (index, (arg, param)) in args.iter().zip(&helper.params).enumerate() {
                if let Some(actual) = cover_expr_type_hint(arg) {
                    if !cover_call_compatible(&param.ty, &actual) {
                        bad(
                            format!(
                                "helper `{name}` argument {} type mismatch: expected {:?}, got {:?}",
                                index + 1,
                                param.ty,
                                actual
                            ),
                            errs,
                        );
                    }
                }
            }
            let actual_ret = helper
                .ret
                .and_then(|local| helper.locals.get(local.index()))
                .map(|local| &local.ty);
            match actual_ret {
                None => bad(format!("helper `{name}` has no return value"), errs),
                Some(actual) if actual != ret => bad(
                    format!(
                        "helper `{name}` return metadata mismatch: function has {actual:?}, call carries {ret:?}"
                    ),
                    errs,
                ),
                Some(actual) if !helper_abi_type_valid(actual, prog.records.len()) => bad(
                    format!(
                        "helper `{name}` return must use the scalar or fixed-vector ABI, got {actual:?}"
                    ),
                    errs,
                ),
                Some(_) => {}
            }
            for arg in args {
                recurse(arg, errs);
            }
        }
        Expr::Call(CallTarget::ExternFn { .. }, args) => {
            for arg in args {
                recurse(arg, errs);
            }
        }
        Expr::Call(target, _) => {
            bad(
                format!("uses unsupported coverpoint call target {target:?}"),
                errs,
            );
        }
        other => bad(
            format!("contains expression outside the cover subset: {other:?}"),
            errs,
        ),
    }
}

/// A fixed-vector element is valid when it is a nonzero-width
/// `UInt`/`SInt`/`Bool` within the field width policy, or another
/// `FixedVec` (nonzero length) whose element is recursively valid.
/// Mirrors the lowering decoder `fixed_vec_elem_ir_type`, so the
/// verifier accepts exactly the nested shapes lowering produces.
fn fixed_vec_elem_valid(ty: &IrType) -> bool {
    match ty {
        IrType::Bool => true,
        IrType::UInt(Some(w)) | IrType::SInt(Some(w)) if *w > 0 => {
            crate::ir::lower::components::field_scalar_width_ok(ty)
        }
        IrType::FixedVec { elem, len } => *len != 0 && fixed_vec_elem_valid(elem),
        _ => false,
    }
}

fn component_fixed_vec_elem_valid(ty: &IrType, record_count: usize) -> bool {
    match ty {
        IrType::Record(record) => record.index() < record_count,
        IrType::FixedVec { elem, len } => {
            *len != 0 && component_fixed_vec_elem_valid(elem, record_count)
        }
        scalar => fixed_vec_elem_valid(scalar),
    }
}

fn queue_fixed_vec_elem_valid(ty: &IrType, record_count: usize) -> bool {
    match ty {
        IrType::Record(record) => record.index() < record_count,
        IrType::FixedVec { elem, len } => {
            *len != 0 && queue_fixed_vec_elem_valid(elem, record_count)
        }
        scalar => fixed_vec_elem_valid(scalar),
    }
}

fn verify_queue_elem_schema(
    elem: &QueueElem,
    record_count: usize,
    what: String,
    errs: &mut Vec<VerifyError>,
) {
    match elem {
        QueueElem::Record(record) if record.index() >= record_count => {
            errs.push(VerifyError::BadProgramRef {
                what: format!("{what} references missing record r{}", record.0),
            });
        }
        QueueElem::Record(_) => {}
        QueueElem::Scalar { ty } => {
            let valid = matches!(ty, IrType::Bool)
                || matches!(ty, IrType::UInt(Some(width)) | IrType::SInt(Some(width)) if *width > 0);
            if !valid {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "{what} has invalid scalar element type {ty:?}; expected bool or a \
                         resolved, nonzero UInt/SInt"
                    ),
                });
            }
        }
        QueueElem::FixedVec { elem, len } => {
            if *len == 0 || !queue_fixed_vec_elem_valid(elem, record_count) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "{what} has invalid fixed-vector element schema {elem:?} x {len}; \
                         expected a nonempty vector with resolved scalar or record leaves"
                    ),
                });
            }
        }
        QueueElem::List { elem } => {
            let valid = matches!(
                elem.as_ref(),
                IrType::Bool
                    | IrType::UInt(Some(1..=crate::MAX_WIDTH_METHOD_BITS))
                    | IrType::SInt(Some(1..=64))
            ) || matches!(elem.as_ref(), IrType::Record(record) if record.index() < record_count);
            if !valid {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "{what} has invalid dynamic-list element schema {elem:?}; expected a resolved scalar or record list element"
                    ),
                });
            }
        }
    }
}

fn verify_scoreboard_scalar_schema(
    ty: &IrType,
    default: &crate::ir::ScoreboardScalarDefault,
    what: String,
    errs: &mut Vec<VerifyError>,
) {
    // Signed and unsigned share the width ceiling now (harc#657): the
    // emitter routes signed wide operators through the two's-complement
    // helpers, so a `sint` past 64 is a real supported type, not the
    // unsigned-by-magnitude carrier the cap once guarded against.
    let valid = match ty {
        IrType::Bool | IrType::UInt(None) | IrType::SInt(None) => true,
        IrType::UInt(Some(width)) | IrType::SInt(Some(width)) => {
            (1..=crate::MAX_WIDTH_METHOD_BITS).contains(width)
        }
        _ => false,
    };
    if !valid {
        errs.push(VerifyError::BadProgramRef {
            what: format!(
                "{what} has invalid scalar type {ty:?}; expected bool or a scalar of \
                 width 1..={} (signed or unsigned)",
                crate::MAX_WIDTH_METHOD_BITS
            ),
        });
    }
    let default_valid = match default {
        // The default is only ever folded to zero for a wide field
        // (source lowering rejects a non-zero wide default), and zero is
        // in range for every width and sign, so the only bound worth
        // checking is on the narrow signed/unsigned cases below.
        crate::ir::ScoreboardScalarDefault::Narrow(value) => match ty {
            IrType::Bool => *value <= 1,
            IrType::UInt(Some(width)) if *width < 64 => *value < (1u64 << width),
            IrType::UInt(_) => true,
            IrType::SInt(Some(width)) if (1..64).contains(width) => {
                let signed = *value as i64;
                let limit = 1i64 << (*width - 1);
                (-limit..limit).contains(&signed)
            }
            // 64-bit and wider signed, and widthless: a zero default is
            // always representable; a wider non-zero default cannot
            // reach here (lowering folds absent/zero only).
            IrType::SInt(Some(64)) | IrType::SInt(None) => true,
            IrType::SInt(Some(_)) => *value == 0,
            _ => false,
        },
        crate::ir::ScoreboardScalarDefault::Wide(words) => {
            !words.is_empty()
                && words.last().is_some_and(|word| *word != 0)
                && matches!(ty, IrType::UInt(Some(width)) if wide_literal_bits(words) > 64 && wide_literal_bits(words) <= *width)
        }
    };
    if !default_valid {
        errs.push(VerifyError::BadProgramRef {
            what: format!("{what} has invalid scalar default {default:?} for {ty:?}"),
        });
    }
}

pub fn verify_program(prog: &TbProgram) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    for (ri, record) in prog.records.iter().enumerate() {
        let mut names = std::collections::HashSet::new();
        for field in &record.fields {
            let what = format!("record r{ri} `{}` field `{}`", record.name, field.name);
            if !names.insert(field.name.as_str()) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "record r{ri} `{}` repeats field `{}`",
                        record.name, field.name
                    ),
                });
            }
            match &field.ty {
                IrType::Seq(elem) => {
                    let valid_elem = matches!(
                        elem.as_ref(),
                        IrType::Bool
                            | IrType::UInt(Some(1..=crate::MAX_WIDTH_METHOD_BITS))
                            | IrType::SInt(Some(1..=64))
                    );
                    if field.vec_len.is_some() || field.default.is_some() || !valid_elem {
                        errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "{what} has invalid dynamic-list schema {:?}; lists require a scalar element, no fixed length, and no scalar default",
                                field.ty
                            ),
                        });
                    }
                }
                IrType::Record(rid) if rid.index() >= prog.records.len() => {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!("{what} references missing record r{}", rid.0),
                    });
                }
                IrType::FixedVec { .. }
                    if field.vec_len.is_none()
                        || field.vec_len == Some(0)
                        || field.default.is_some()
                        || !fixed_vec_elem_valid(&field.ty) =>
                {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "{what} has invalid nested fixed-vector schema {:?} with outer length {:?}",
                            field.ty, field.vec_len
                        ),
                    });
                }
                _ => {}
            }
        }
    }
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
        let hook_params = match &cg.trigger {
            CovTrigger::Hook { param_names, .. } => param_names.as_slice(),
            CovTrigger::PosedgeDutClk => &[],
        };
        for point in &cg.points {
            check_cover_expr(
                prog,
                ci,
                &format!("point `{}` target", point.name),
                hook_params,
                &point.target,
                &mut errs,
            );
            for bin in &point.bins {
                for value in &bin.values {
                    let mut check_bound = |bound: &CovBinBound| {
                        if let CovBinBound::Runtime(expr) = bound {
                            check_cover_expr(
                                prog,
                                ci,
                                &format!("bin `{}.{}` runtime bound", point.name, bin.name),
                                hook_params,
                                expr,
                                &mut errs,
                            );
                        }
                    };
                    match value {
                        CovBinValue::Eq(bound) => check_bound(bound),
                        CovBinValue::Range { lo, hi } => {
                            if let Some(bound) = lo {
                                check_bound(bound);
                            }
                            if let Some(bound) = hi {
                                check_bound(bound);
                            }
                        }
                    }
                }
            }
        }
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
        drop(check_component_function);
        for method in &component.methods {
            for (param, ty) in method.param_tys.iter().enumerate() {
                if matches!(ty, IrType::FixedVec { .. })
                    && !component_fixed_vec_elem_valid(ty, prog.records.len())
                {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "component c{ci} `{}` method `{}` parameter {param} has invalid fixed-vector schema {ty:?}",
                            component.name, method.name
                        ),
                    });
                }
            }
            if let Some(function) = prog.functions.get(method.function.index()) {
                let actual: Vec<_> = function
                    .params
                    .iter()
                    .map(|param| param.ty.clone())
                    .collect();
                let locals: Vec<_> = function
                    .locals
                    .iter()
                    .take(function.params.len())
                    .map(|local| local.ty.clone())
                    .collect();
                if actual != method.param_tys || locals != actual {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "component c{ci} `{}` method `{}` parameter schema {:?} disagrees with fn{} parameters {:?} or parameter locals {:?}",
                            component.name,
                            method.name,
                            method.param_tys,
                            method.function.0,
                            actual,
                            locals
                        ),
                    });
                }
                let function_ret = function
                    .ret
                    .and_then(|ret| function.locals.get(ret.index()))
                    .map(|local| local.ty.clone());
                if method.has_ret != method.ret_ty.is_some() {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "component c{ci} `{}` method `{}` has_ret={} disagrees with return schema {:?}",
                            component.name,
                            method.name,
                            method.has_ret,
                            method.ret_ty,
                        ),
                    });
                }
                let materialized_return = matches!(
                    method.ret_ty,
                    Some(
                        IrType::Record(_)
                            | IrType::FixedVec { .. }
                            | IrType::UInt(_)
                            | IrType::SInt(_)
                            | IrType::Bool
                    )
                );
                if materialized_return && method.ret_ty != function_ret {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "component c{ci} `{}` method `{}` return schema {:?} disagrees with fn{} return {:?}",
                            component.name,
                            method.name,
                            method.ret_ty,
                            method.function.0,
                            function_ret
                        ),
                    });
                }
                if let Some(ty @ IrType::FixedVec { .. }) = &method.ret_ty {
                    if !component_fixed_vec_elem_valid(ty, prog.records.len()) {
                        errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "component c{ci} `{}` method `{}` has invalid fixed-vector return schema {ty:?}",
                                component.name, method.name
                            ),
                        });
                    }
                }
            }
        }
        for edge in &component.connects {
            if let Err(detail) =
                verify_component_connect(prog, ComponentId(ci as u32), edge)
            {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "component c{ci} `{}` has invalid connect metadata: {detail}",
                        component.name
                    ),
                });
            }
        }
        for handler in &component.cycle_handlers {
            if handler.monitor_channel.is_some()
                && matches!(handler.phase, HandlerPhase::PostEval)
            {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "component c{ci} `{}` has a bound handshake monitor scheduled at \
                         post_eval, which is outside the lowered monitor contract",
                        component.name
                    ),
                });
            }
            if let Some(func) = prog.functions.get(handler.function.index()) {
                let mut checker = Checker {
                    prog,
                    func,
                    fid: func.id,
                    bid: func.entry,
                    errs: &mut errs,
                    temporal_slots_ok: false,
                };
                checker.check_truth_expr(&handler.trigger, true, "component cycle-handler trigger");
            }
        }
        if component.has_active_surface() && !matches!(component.kind, ComponentKindTag::Transactor)
        {
            errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "component c{ci} `{}` is not a transactor but has active-only members",
                    component.name
                ),
            });
        }
        for field in &component.fields {
            if let ComponentFieldKind::Event { payload } = &field.kind {
                if let Err(detail) = verify_event_payload_ref(prog, payload) {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "component c{ci} field `{}` event payload {detail}",
                            field.name
                        ),
                    });
                }
            }
            if let ComponentFieldKind::Queue { elem } = &field.kind {
                verify_queue_elem_schema(
                    elem,
                    prog.records.len(),
                    format!("component c{ci} field `{}`", field.name),
                    &mut errs,
                );
            }
            if let ComponentFieldKind::FixedVec(vec) = &field.kind {
                // The THIRD site of one width policy — the lowering
                // gate and the emitter were the other two. A
                // hardcoded `<= 64` here rejected a program lowering
                // had just accepted, which is an internal error rather
                // than a diagnostic. It asks
                // `components::field_scalar_width_ok` now, the same
                // function the gate uses.
                // Recurses through a nested `FixedVec` element to the
                // scalar leaf, so `Vec<Vec<uint<8>,2>,2>` validates and
                // `Vec<Vec<uint<2048>,2>,2>` (an over-wide leaf) still
                // fails at the same width policy the gate applies.
                let valid_elem = component_fixed_vec_elem_valid(&vec.elem, prog.records.len());
                if vec.len == 0 || !valid_elem {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "component c{ci} field `{}` has invalid fixed-vector schema {:?}",
                            field.name, vec
                        ),
                    });
                }
            }
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
    for (si, scoreboard) in prog.scoreboards.iter().enumerate() {
        for field in &scoreboard.fields {
            if matches!(field.name.as_str(), "_last_in_cycle" | "_last_out_cycle") {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "scoreboard sb{si} field `{}` collides with a generated heartbeat member",
                        field.name
                    ),
                });
            }
            match &field.kind {
                ScoreboardFieldKind::Queue { elem } => verify_queue_elem_schema(
                    elem,
                    prog.records.len(),
                    format!("scoreboard sb{si} field `{}`", field.name),
                    &mut errs,
                ),
                ScoreboardFieldKind::Scalar { ty, default } => verify_scoreboard_scalar_schema(
                    ty,
                    default,
                    format!("scoreboard sb{si} field `{}`", field.name),
                    &mut errs,
                ),
                ScoreboardFieldKind::Record { record } => {
                    if prog.records.get(record.index()).is_none() {
                        errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "scoreboard sb{si} field `{}` references missing record r{}",
                                field.name, record.0
                            ),
                        });
                    }
                }
                ScoreboardFieldKind::List { elem, vec_len } => {
                    // Signed and unsigned share the element width ceiling
                    // (harc#657) — the same lift as scalar fields, so a
                    // `list<sint<128>>` that lowering now accepts is not
                    // rejected here into an internal error. The scoreboard
                    // list decoder routes through the shared
                    // `field_scalar_width_ok`, so the two must agree.
                    let valid_elem =
                        matches!(elem, IrType::Bool | IrType::UInt(None) | IrType::SInt(None))
                            || matches!(
                                elem,
                                IrType::UInt(Some(width)) | IrType::SInt(Some(width))
                                    if (1..=crate::MAX_WIDTH_METHOD_BITS).contains(width)
                            );
                    if !valid_elem || vec_len.is_some_and(|len| len == 0) {
                        errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "scoreboard sb{si} field `{}` has invalid list schema {:?} x {:?}",
                                field.name, elem, vec_len
                            ),
                        });
                    }
                }
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
        for field in &x.state_fields {
            match &field.kind {
                StateFieldKind::Queue { elem } => verify_queue_elem_schema(
                    elem,
                    prog.records.len(),
                    format!("transactor x{xi} state field `{}`", field.name),
                    &mut errs,
                ),
                StateFieldKind::FixedVec { ty } => {
                    let valid = matches!(
                        ty,
                        IrType::FixedVec { elem, len }
                            if *len != 0
                                && component_fixed_vec_elem_valid(elem, prog.records.len())
                    );
                    if !valid {
                        errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "transactor x{xi} state field `{}` has invalid fixed-vector metadata {ty:?}",
                                field.name
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
        for method in &x.methods {
            let Some(function) = prog.functions.get(method.function.index()) else {
                continue;
            };
            let function_param_tys = function
                .params
                .iter()
                .map(|param| param.ty.clone())
                .collect::<Vec<_>>();
            let function_param_local_tys = function
                .locals
                .iter()
                .take(function.params.len())
                .map(|local| local.ty.clone())
                .collect::<Vec<_>>();
            if method.param_names.len() != method.param_tys.len()
                || method.param_tys != function_param_tys
                || method.param_tys != function_param_local_tys
            {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` parameter schema {:?} disagrees with fn{} params {:?} / locals {:?}",
                        method.name,
                        method.param_tys,
                        method.function.0,
                        function_param_tys,
                        function_param_local_tys
                    ),
                });
            }
            for ty in &method.param_tys {
                if matches!(ty, IrType::FixedVec { .. })
                    && !component_fixed_vec_elem_valid(ty, prog.records.len())
                {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "transactor x{xi} method `{}` has invalid fixed-vector parameter type {ty:?}",
                            method.name
                        ),
                    });
                }
            }
            let function_ret = function
                .ret
                .and_then(|ret| function.locals.get(ret.index()))
                .map(|local| local.ty.clone());
            if method.has_ret != method.ret_ty.is_some() || method.ret_ty != function_ret {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` return schema {:?} disagrees with fn{} return {:?}",
                        method.name, method.ret_ty, method.function.0, function_ret
                    ),
                });
            }
            if let Some(ty @ IrType::FixedVec { .. }) = &method.ret_ty {
                if !component_fixed_vec_elem_valid(ty, prog.records.len()) {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "transactor x{xi} method `{}` has invalid fixed-vector return type {ty:?}",
                            method.name
                        ),
                    });
                }
            }
        }
        for method in &x.target_methods {
            let Some(function) = prog.functions.get(method.function.index()) else {
                continue;
            };
            let ret_record = function
                .ret
                .and_then(|ret| function.locals.get(ret.index()))
                .and_then(|local| match local.ty {
                    IrType::Record(record) => Some(record),
                    _ => None,
                });
            if ret_record.is_some_and(|record| record_contains_dynamic_list(prog, record)) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} target method `{}` returns a dynamic-list record over a fixed TLM response wire",
                        method.name
                    ),
                });
            }
            for param in &function.params {
                if matches!(param.ty, IrType::Record(record) if record_contains_dynamic_list(prog, record))
                {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "transactor x{xi} target method `{}` receives a dynamic-list record over a fixed TLM request wire",
                            method.name
                        ),
                    });
                }
            }
        }
        for (name, function) in x
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m.function))
            .chain(
                x.target_methods
                    .iter()
                    .map(|m| (m.name.as_str(), m.function)),
            )
        {
            match prog.functions.get(function.index()) {
                Some(f)
                    if f.kind
                        == (FunctionKind::TransactorBody {
                            transactor: TransactorId(xi as u32),
                        }) => {}
                Some(f) => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` points at fn{} with kind {:?}",
                        name, function.0, f.kind
                    ),
                }),
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` references missing fn{}",
                        name, function.0
                    ),
                }),
            }
        }
    }
    for (ti, tb) in prog.testbenches.iter().enumerate() {
        let mut component_binding_names = std::collections::HashSet::new();
        let mut transactor_binding_names = std::collections::HashSet::new();
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
                TbStateFieldSchema::Queue(field) => {
                    verify_queue_elem_schema(
                        &field.elem,
                        prog.records.len(),
                        format!("tb{ti} queue field `{}`", field.name),
                        &mut errs,
                    );
                    &field.name
                }
            };
            if !state_names.insert(name) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("tb{ti} declares state field `{name}` more than once"),
                });
            }
        }
        let mut regblock_binding_names = HashSet::new();
        for binding in &tb.regblock_bindings {
            if !regblock_binding_names.insert(&binding.field) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} declares regblock binding `{}` more than once",
                        binding.field
                    ),
                });
            }
            let Some(regblock) = prog.regblocks.get(binding.regblock.index()) else {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} regblock binding `{}` references missing rb{}",
                        binding.field, binding.regblock.0
                    ),
                });
                continue;
            };
            let mut callback_registers = HashSet::new();
            for (register, callback) in &binding.callbacks {
                if !callback_registers.insert(register)
                    || !regblock.registers.iter().any(|reg| reg.name == *register)
                {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} regblock binding `{}` has invalid or duplicate callback register `{register}`",
                            binding.field
                        ),
                    });
                }
                match prog.functions.get(callback.index()) {
                    Some(func)
                        if func.kind == FunctionKind::TestHook
                            && func.owner == Some(TestbenchId(ti as u32))
                            && func.params.len() == 1
                            && func.params[0].ty == IrType::UInt(None)
                            && func
                                .locals
                                .first()
                                .is_some_and(|local| local.ty == func.params[0].ty)
                            && func.ret.is_none() => {}
                    Some(func) => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} regblock binding `{}` callback fn{} has invalid kind {:?}, owner {:?}, or signature",
                            binding.field, callback.0, func.kind, func.owner
                        ),
                    }),
                    None => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} regblock binding `{}` callback references missing fn{}",
                            binding.field, callback.0
                        ),
                    }),
                }
            }
        }
        for (field, xid) in &tb.transactor_fields {
            if !transactor_binding_names.insert(field) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("tb{ti} declares transactor field `{field}` more than once"),
                });
            }
            if xid.index() >= prog.transactors.len() {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} transactor field `{field}` references missing x{}",
                        xid.0
                    ),
                });
            }
        }
        let mut actor_names = std::collections::HashSet::new();
        let mut actor_storage_names = std::collections::HashSet::new();
        for actor in &tb.unbound_state_actors {
            if !actor_names.insert(&actor.field) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} declares transactor stamp storage `{}` more than once",
                        actor.field
                    ),
                });
            }
            if actor.storage.is_empty() || !actor_storage_names.insert(&actor.storage) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} transactor field `{}` has empty or duplicate C++ stamp storage `{}`",
                        actor.field, actor.storage
                    ),
                });
            }
            match tb
                .transactor_fields
                .iter()
                .find_map(|(name, bound)| (name == &actor.field).then_some(*bound))
            {
                Some(bound) if bound == actor.transactor => {}
                Some(bound) => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} transactor stamp storage `{}` binds x{} but the field binds x{}",
                        actor.field, actor.transactor.0, bound.0
                    ),
                }),
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} transactor stamp storage `{}` has no matching transactor field",
                        actor.field
                    ),
                }),
            }
            if actor.transactor.index() >= prog.transactors.len() {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} transactor stamp storage `{}` references missing x{}",
                        actor.field, actor.transactor.0
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
                Some(_) => {}
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} component field `{}` references missing c{}",
                        binding.field, binding.component.0
                    ),
                }),
            }
        }
        let mut target_names = std::collections::HashSet::new();
        for actor in &tb.target_tlm_actors {
            if transactor_binding_names.contains(&actor.instance)
                || !target_names.insert(&actor.instance)
            {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} declares target transactor instance `{}` more than once",
                        actor.instance
                    ),
                });
            }
            let Some(schema) = prog.transactors.get(actor.transactor.index()) else {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} target transactor `{}` references missing x{}",
                        actor.instance, actor.transactor.0
                    ),
                });
                continue;
            };
            let binding = tb.bus_bindings.iter().find(|b| b.field == actor.bus_field);
            match binding {
                Some(binding) if schema.bound_bus.as_deref() == Some(binding.bus.as_str()) => {}
                Some(binding) => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} target transactor `{}` uses bus `{}` but x{} binds to {:?}",
                        actor.instance, binding.bus, actor.transactor.0, schema.bound_bus
                    ),
                }),
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} target transactor `{}` references missing bus binding `{}`",
                        actor.instance, actor.bus_field
                    ),
                }),
            }
            if let Some(host) = actor.host_component {
                let Some(component) = prog.components.get(host.index()) else {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} target transactor `{}` references missing host c{}",
                            actor.instance, host.0
                        ),
                    });
                    continue;
                };
                match tb
                    .component_fields
                    .iter()
                    .find(|field| field.field == actor.instance)
                {
                    Some(field)
                        if field.component == host
                            && field.mode
                                == Some(if actor.active {
                                    ComponentInstanceMode::Active
                                } else {
                                    ComponentInstanceMode::Passive
                                }) => {}
                    Some(field) => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} target transactor `{}` stores host c{} / active={} but its component field binds c{} / mode {:?}",
                            actor.instance, host.0, actor.active, field.component.0, field.mode
                        ),
                    }),
                    None => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} target transactor `{}` has host c{} but no same-named component field",
                            actor.instance, host.0
                        ),
                    }),
                }
                if !matches!(component.kind, ComponentKindTag::Transactor)
                    || component.name != schema.name
                    || !target_state_matches_component(schema, component)
                {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} target transactor `{}` host c{} is not state-compatible with x{}",
                            actor.instance, host.0, actor.transactor.0
                        ),
                    });
                }
            } else {
                if actor.active {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} standalone target transactor `{}` is unexpectedly active",
                            actor.instance
                        ),
                    });
                }
                if tb
                    .component_fields
                    .iter()
                    .any(|field| field.field == actor.instance)
                {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "tb{ti} target transactor `{}` aliases a component field but stores no host component",
                            actor.instance
                        ),
                    });
                }
            }
        }
        if let Err(detail) =
            validate_component_binding_modes(&prog.components, &tb.component_fields)
        {
            errs.push(VerifyError::BadProgramRef {
                what: format!("tb{ti} has invalid component instance modes: {detail}"),
            });
        }
        for edge in &tb.connects {
            if let Err(detail) = verify_testbench_connect(prog, tb, edge) {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("tb{ti} has invalid connect metadata: {detail}"),
                });
            }
        }
        // Testbench cycle-service predicates are standalone expressions,
        // not part of the handler body's CFG. Walk them explicitly with
        // the handler function's owner/type context so every expression
        // invariant (including transactor heartbeat field/schema/storage)
        // is checked before codegen renders the registration closure.
        for (si, service) in tb.cycle_services.iter().enumerate() {
            let Some(func) = prog.functions.get(service.function.index()) else {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} cycle service {si} references missing fn{}",
                        service.function.0
                    ),
                });
                continue;
            };
            if func.kind != FunctionKind::TestHook
                || func.owner != Some(TestbenchId(ti as u32))
                || !func.params.is_empty()
            {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} cycle service {si} body fn{} is {:?}, owner {:?}, with {} param(s)",
                        func.id.0,
                        func.kind,
                        func.owner,
                        func.params.len()
                    ),
                });
            }
            let mut checker = Checker {
                prog,
                func,
                fid: func.id,
                bid: func.entry,
                errs: &mut errs,
                temporal_slots_ok: false,
            };
            checker.check_truth_expr(&service.trigger, true, "testbench cycle-service trigger");
        }
    }
    for (i, func) in prog.functions.iter().enumerate() {
        if func.id.index() != i {
            errs.push(VerifyError::BadProgramRef {
                what: format!("fn at index {i} carries id fn{}", func.id.0),
            });
        }
        for (local, schema) in func.locals.iter().enumerate() {
            if let IrType::Event(payload) = &schema.ty {
                if let Err(detail) = verify_event_payload_ref(prog, payload) {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!("fn{} local %{local} event payload {detail}", func.id.0),
                    });
                }
            }
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

fn target_state_matches_component(target: &TransactorSchema, component: &ComponentSchema) -> bool {
    let host_state: Vec<_> = component
        .fields
        .iter()
        .filter(|field| {
            matches!(
                field.kind,
                ComponentFieldKind::Scalar { .. }
                    | ComponentFieldKind::Queue { .. }
                    | ComponentFieldKind::Record { .. }
            )
        })
        .collect();
    if target.state_fields.len() != host_state.len() {
        return false;
    }
    let mut target_names = HashSet::new();
    let mut host_names = HashSet::new();
    if target
        .state_fields
        .iter()
        .any(|field| !target_names.insert(field.name.as_str()))
        || host_state
            .iter()
            .any(|field| !host_names.insert(field.name.as_str()))
    {
        return false;
    }
    target.state_fields.iter().all(|state| {
        host_state.iter().any(|field| {
            if field.name != state.name {
                return false;
            }
            match (&state.kind, &field.kind) {
                (
                    StateFieldKind::Scalar { ty: a, default: ad },
                    ComponentFieldKind::Scalar { ty: b, default: bd },
                ) => a == b && ad == bd,
                (StateFieldKind::Queue { elem: a }, ComponentFieldKind::Queue { elem: b }) => {
                    a == b
                }
                (
                    StateFieldKind::Record { record: a },
                    ComponentFieldKind::Record { record: b },
                ) => a == b,
                (StateFieldKind::FixedVec { ty }, ComponentFieldKind::FixedVec(vec)) => {
                    ty == &IrType::FixedVec {
                        elem: Box::new(vec.elem.clone()),
                        len: vec.len,
                    }
                }
                _ => false,
            }
        })
    })
}

fn verify_testbench_connect(
    prog: &TbProgram,
    tb: &TestbenchSchema,
    edge: &ConnectEdgeSchema,
) -> Result<(), String> {
    let endpoint_mode = |path: &[String]| -> Result<_, String> {
        let (root, tail) = path
            .split_first()
            .ok_or_else(|| "empty component path".to_string())?;
        let binding = tb
            .component_fields
            .iter()
            .find(|field| field.field == *root)
            .ok_or_else(|| format!("root `{root}` is not a testbench component field"))?;
        resolve_component_path_mode(&prog.components, binding.component, binding.mode, tail)
            .map(|resolved| resolved.effective_mode)
            .map_err(|err| err.to_string())
    };
    let source_mode = endpoint_mode(&edge.src_path)?;
    let sink_mode = endpoint_mode(&edge.sink_path)?;
    if !component_mode_includes_activation(source_mode, edge.src_activation)
        || !component_mode_includes_activation(sink_mode, edge.sink_activation)
    {
        return Err("connect edge uses a mode-disabled endpoint".to_string());
    }

    let src_id = resolve_testbench_component_path(prog, tb, &edge.src_path)?;
    let sink_id = resolve_testbench_component_path(prog, tb, &edge.sink_path)?;
    verify_resolved_connect(prog, edge, src_id, sink_id)
}

fn verify_component_connect(
    prog: &TbProgram,
    owner: ComponentId,
    edge: &ConnectEdgeSchema,
) -> Result<(), String> {
    let resolve = |path: &[String]| {
        resolve_component_path_mode(&prog.components, owner, None, path)
            .map_err(|err| err.to_string())
    };
    let source = resolve(&edge.src_path)?;
    let sink = resolve(&edge.sink_path)?;
    // A reusable component schema has no inherited instance mode. Its
    // active-only edge is valid metadata even when one passive binding
    // later omits that wiring; the binding/codegen traversal decides
    // visibility. Here we verify only the schema-relative endpoints and
    // payload contract shared by every instantiation.
    verify_resolved_connect(prog, edge, source.component, sink.component)
}

fn verify_resolved_connect(
    prog: &TbProgram,
    edge: &ConnectEdgeSchema,
    src_id: ComponentId,
    sink_id: ComponentId,
) -> Result<(), String> {
    let src = prog
        .components
        .get(src_id.index())
        .ok_or_else(|| format!("source component c{} does not resolve", src_id.0))?;
    let payload = match src.field(&edge.src_event) {
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Event { payload },
            activation,
            ..
        }) if *activation == edge.src_activation => payload,
        _ => {
            return Err(format!(
                "source `{}.{}` is not an event field",
                edge.src_path.join("."),
                edge.src_event
            ));
        }
    };

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
                return Err(format!(
                    "sink method `{method}` has mismatched activation metadata"
                ));
            }
            if !m.hookable || m.param_names.len() != 1 || m.has_ret || m.param_tys.len() != 1 {
                return Err(format!(
                    "sink method `{method}` is not a one-argument void hookable"
                ));
            }
            if !connect_payload_reaches_param(payload, &m.param_tys[0]) {
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
            }) if connect_payloads_bridge(payload, sink_payload)
                && *activation == edge.sink_activation => {}
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

/// Resolve a persistent transactor-state queue from either a shared
/// transactor/responder body (by function id) or a test-scope instance name.
/// This mirrors the backend's state-receiver resolution so verification can
/// enforce the queue element type before emission indexes the same schema.
fn resolve_transactor_state_queue_elem(
    prog: &TbProgram,
    func: &TbFunction,
    instance: &str,
    field: &str,
) -> Result<QueueElem, String> {
    let by_function = prog.transactors.iter().find(|transactor| {
        transactor
            .methods
            .iter()
            .any(|method| method.function == func.id)
            || transactor
                .target_methods
                .iter()
                .any(|method| method.function == func.id)
    });

    let transactor = if let Some(transactor) = by_function {
        transactor
    } else {
        if instance.is_empty() {
            return Err(format!(
                "fn{} target-state queue `.{field}` has no owning transactor body",
                func.id.0
            ));
        }
        let owner = func
            .owner
            .ok_or_else(|| format!("target-state instance `{instance}` has no owning testbench"))?;
        let tb = prog
            .testbenches
            .get(owner.index())
            .ok_or_else(|| format!("references missing testbench tb{}", owner.0))?;
        let transactor = tb
            .transactor_fields
            .iter()
            .find(|(name, _)| name == instance)
            .map(|(_, transactor)| *transactor)
            .or_else(|| {
                tb.target_tlm_actors
                    .iter()
                    .find(|actor| actor.instance == instance)
                    .map(|actor| actor.transactor)
            })
            .or_else(|| {
                tb.unbound_state_actors
                    .iter()
                    .find(|actor| actor.field == instance)
                    .map(|actor| actor.transactor)
            })
            .ok_or_else(|| {
                format!(
                    "target-state instance `{instance}` does not resolve on tb{}",
                    owner.0
                )
            })?;
        prog.transactors.get(transactor.index()).ok_or_else(|| {
            format!(
                "target-state instance `{instance}` references missing transactor x{}",
                transactor.0
            )
        })?
    };

    transactor
        .state_fields
        .iter()
        .find(|state| state.name == field)
        .ok_or_else(|| {
            format!(
                "transactor `{}` has no state field `{field}`",
                transactor.name
            )
        })
        .and_then(|state| match &state.kind {
            StateFieldKind::Queue { elem } => Ok(elem.clone()),
            _ => Err(format!(
                "transactor `{}` state field `{field}` is not a queue",
                transactor.name
            )),
        })
}

fn resolve_component_queue_elem(
    prog: &TbProgram,
    func: &TbFunction,
    base: &ComponentBase,
    queue: &str,
) -> Result<QueueElem, String> {
    let component = match base {
        ComponentBase::SelfField => match func.kind {
            FunctionKind::ComponentMethod { component } => component,
            _ => {
                return Err(format!(
                    "self-relative component queue `{queue}` appears outside a component method"
                ));
            }
        },
        ComponentBase::Path(path) => {
            let owner = func
                .owner
                .ok_or_else(|| "component queue path has no owning testbench".to_string())?;
            let tb = prog
                .testbenches
                .get(owner.index())
                .ok_or_else(|| format!("references missing testbench tb{}", owner.0))?;
            resolve_testbench_component_path(prog, tb, path)?
        }
        ComponentBase::Local(local) => {
            return Err(format!(
                "component queue `{queue}` uses unsupported local base %{}",
                local.0
            ));
        }
    };
    let schema = prog
        .components
        .get(component.index())
        .ok_or_else(|| format!("references missing component c{}", component.0))?;
    match schema.field(queue) {
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Queue { elem },
            ..
        }) => Ok(elem.clone()),
        _ => Err(format!(
            "component c{} has no queue field `{queue}`",
            component.0
        )),
    }
}

/// The METHOD-sink half of the same rule, and for the same reason.
///
/// Lowering's shape predicate permits every scalar-to-scalar bridge and
/// requires record identity, while its delivery predicate separately
/// prevents storage-width loss. Leaving either rule restated here is
/// exactly the drift this verifier backstop is meant to avoid: a
/// lowering site that forgot the delivery call would emit
/// `std::function<void(harc_rt::HarcWide<32>)>` feeding a `uint64_t`
/// parameter, verify clean, and truncate 960 bits per notification.
/// Measured by deleting that call and watching `dump-ir` exit 0.
fn connect_payload_reaches_param(payload: &EventPayload, ty: &IrType) -> bool {
    if !crate::ir::lower::components::connect_payload_matches_ir_type(payload, ty) {
        return false;
    }
    match payload.scalar_ir_type() {
        Some(src) => crate::ir::lower::components::connect_delivery_is_faithful(&src, ty),
        None => true,
    }
}

/// The verifier's copy of the `connect` payload rule — which is to
/// say, not a copy: it ASKS lowering's two predicates.
///
/// This arm was `*sink_payload == payload`, the exact twin of the
/// `*payload != src_payload` in `components_impl.rs`. `EventPayload`
/// derives `PartialEq`, so when the payload grew a `width` BOTH
/// comparisons silently became width checks. Fixing only the lowering
/// one made this the worse of the two failures: lowering emitted a
/// `ConnectSink::Event` edge for two payloads of different declared
/// widths and the verifier then rejected it, turning a graceful
/// diagnostic into `internal error: TB-IR failed verification after
/// lowering`.
///
/// Asking rather than restating is the point. A verifier that
/// re-derives a rule is a second place for it to be wrong, and this
/// one was wrong in the direction that produces an internal error
/// rather than a refusal.
fn connect_payloads_bridge(src: &EventPayload, sink: &EventPayload) -> bool {
    if !crate::ir::lower::components::event_payloads_agree_in_shape(src, sink) {
        return false;
    }
    match (src.scalar_ir_type(), sink.scalar_ir_type()) {
        (Some(s), Some(k)) => crate::ir::lower::components::connect_delivery_is_faithful(&s, &k),
        _ => true,
    }
}

fn event_payload_accepts_value_type(payload: &EventPayload, ty: &IrType) -> bool {
    match (payload, ty) {
        (_, IrType::Unknown) => true,
        (EventPayload::Scalar { .. }, IrType::UInt(_) | IrType::SInt(_) | IrType::Bool) => true,
        (EventPayload::Record(source), IrType::Record(sink)) => *source == *sink,
        (EventPayload::FixedVec { .. }, IrType::FixedVec { .. }) => {
            payload.value_ir_type() == *ty
        }
        _ => false,
    }
}

fn event_payload_handler_matches_type(payload: &EventPayload, ty: &IrType) -> bool {
    match (payload, ty) {
        (EventPayload::Scalar { signed: true, .. }, IrType::SInt(_)) => true,
        (EventPayload::Scalar { signed: false, .. }, IrType::UInt(_) | IrType::Bool) => true,
        (EventPayload::Record(source), IrType::Record(sink)) => *source == *sink,
        (EventPayload::FixedVec { .. }, IrType::FixedVec { .. }) => {
            payload.value_ir_type() == *ty
        }
        _ => false,
    }
}

fn verify_event_payload_ref(prog: &TbProgram, payload: &EventPayload) -> Result<(), String> {
    match payload {
        EventPayload::Record(record) if record.index() >= prog.records.len() => {
            return Err(format!("references missing record r{}", record.0));
        }
        EventPayload::FixedVec { elem, len } => {
            if *len == 0 {
                return Err("has a zero-length fixed-vector payload".to_string());
            }
            if !component_fixed_vec_elem_valid(&elem, prog.records.len()) {
                return Err(format!("has invalid fixed-vector element metadata {elem:?}"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn verify_component_event_ref(
    prog: &TbProgram,
    func: &TbFunction,
    base: &ComponentBase,
    component: ComponentId,
    event: &str,
    payload: &EventPayload,
) -> Result<(), String> {
    verify_event_payload_ref(prog, payload)?;
    let ComponentBase::Path(path) = base else {
        return Err("component event target must use a component path".to_string());
    };
    let (root, tail) = path
        .split_first()
        .ok_or_else(|| "component event target has an empty path".to_string())?;
    // Match `component_base_id`'s namespace order. A real testbench field
    // wins; otherwise `self.<child>...` is rooted at the component method's
    // owning schema and verifies component-body dotted emits.
    let owner_tb = func
        .owner
        .and_then(|owner| prog.testbenches.get(owner.index()));
    let resolved = if let Some(binding) = owner_tb.and_then(|tb| {
        tb.component_fields
            .iter()
            .find(|binding| binding.field == *root)
    }) {
        resolve_component_path_mode(&prog.components, binding.component, binding.mode, tail)
            .map_err(|err| err.to_string())?
    } else if root == "self" {
        let FunctionKind::ComponentMethod { component: owner } = func.kind else {
            return Err("self-rooted component event outside a component method".to_string());
        };
        resolve_component_path_mode(&prog.components, owner, None, tail)
            .map_err(|err| err.to_string())?
    } else {
        return Err(format!("root `{root}` is not a testbench component field"));
    };
    if resolved.component != component {
        return Err(format!(
            "component path `{}` resolves to c{}, not stored c{}",
            path.join("."),
            resolved.component.0,
            component.0
        ));
    }
    let schema = prog
        .components
        .get(component.index())
        .ok_or_else(|| format!("references missing component c{}", component.0))?;
    match schema.field(event) {
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Event {
                payload: field_payload,
            },
            activation,
            ..
        // The THIRD `EventPayload` struct equality in the tree, and
        // the only one that should stay one. The other two compared a
        // SOURCE payload against a SINK's — two independently declared
        // types that a C++ conversion has to bridge — so widening the
        // struct silently turned each into a width check and refused
        // legal programs (divergences 139-141). This one compares an
        // op's stored payload against THE SAME FIELD it was copied
        // from, so the question is identity, not bridging: a
        // difference means the IR is internally inconsistent, which is
        // exactly what a verifier is for. It stays correct if a fourth
        // field is added, for the same reason the others did not.
        //
        // Swept across `uint`, `bool`, `uint<1|8|64|65|128|160|1024>`
        // and `sint<8|64>` after the payload grew a width: no arm of
        // this match fires on any of them.
        }) if field_payload == payload => {
            if !component_mode_includes_activation(resolved.effective_mode, *activation) {
                return Err(format!(
                    "component event `{}.{event}` is disabled by its instance mode",
                    path.join(".")
                ));
            }
        }
        Some(ComponentFieldSchema {
            kind: ComponentFieldKind::Event { .. },
            ..
        }) => {
            return Err(format!(
                "component event `{}.{event}` has a mismatched payload",
                path.join(".")
            ));
        }
        _ => {
            return Err(format!(
                "`{}.{event}` is not an event field",
                path.join(".")
            ));
        }
    }
    Ok(())
}

fn verify_method_hook_target(
    prog: &TbProgram,
    func: &TbFunction,
    target: &MethodHookTarget,
) -> Result<Vec<IrType>, String> {
    let owner = func
        .owner
        .ok_or_else(|| "method-hook subscription has no owning testbench".to_string())?;
    let tb = prog
        .testbenches
        .get(owner.index())
        .ok_or_else(|| format!("references missing testbench tb{}", owner.0))?;
    match target {
        MethodHookTarget::Transactor {
            field,
            transactor,
            method,
        } => {
            let bound = tb
                .transactor_fields
                .iter()
                .find(|(name, _)| name == field)
                .ok_or_else(|| format!("`{field}` is not a testbench transactor field"))?;
            if bound.1 != *transactor {
                return Err(format!(
                    "transactor field `{field}` resolves to x{}, not stored x{}",
                    bound.1 .0, transactor.0
                ));
            }
            let schema = prog
                .transactors
                .get(transactor.index())
                .ok_or_else(|| format!("references missing transactor x{}", transactor.0))?;
            let method = schema
                .method(method)
                .filter(|method| method.hookable)
                .ok_or_else(|| {
                    format!("`{field}.{method}` does not resolve to a hookable transactor method")
                })?;
            if method.active_only && tb.passive_transactor_fields.contains(field) {
                return Err(format!(
                    "active-only hookable `{field}.{}` is disabled on a passive instance",
                    method.name
                ));
            }
            Ok(method.param_tys.clone())
        }
        MethodHookTarget::Component {
            base,
            component,
            method,
        } => {
            let ComponentBase::Path(path) = base else {
                return Err("component method hook must use a test-scope path".to_string());
            };
            let (root, tail) = path
                .split_first()
                .ok_or_else(|| "component method hook has an empty path".to_string())?;
            let binding = tb
                .component_fields
                .iter()
                .find(|binding| binding.field == *root)
                .ok_or_else(|| format!("root `{root}` is not a testbench component field"))?;
            let resolved = resolve_component_path_mode(
                &prog.components,
                binding.component,
                binding.mode,
                tail,
            )
            .map_err(|err| err.to_string())?;
            if resolved.component != *component {
                return Err(format!(
                    "component path `{}` resolves to c{}, not stored c{}",
                    path.join("."),
                    resolved.component.0,
                    component.0
                ));
            }
            let schema = prog
                .components
                .get(component.index())
                .ok_or_else(|| format!("references missing component c{}", component.0))?;
            let method = schema
                .method(method)
                .filter(|method| method.hookable)
                .ok_or_else(|| {
                    format!(
                        "`{}.{method}` does not resolve to a hookable component method",
                        path.join(".")
                    )
                })?;
            if !component_mode_includes_activation(resolved.effective_mode, method.activation) {
                return Err(format!(
                    "component hookable `{}.{}` is disabled by its instance mode",
                    path.join("."),
                    method.name
                ));
            }
            Ok(method.param_tys.clone())
        }
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

    // Pure helpers emit as file-scope C++ functions whose params and return
    // use the scalar-or-one-dimensional-fixed-vector helper ABI. Internal
    // record locals are permitted, but a pass must not drift a parameter's
    // mirrored local type or route a bare record/nested vector through the
    // signature/return slot.
    if func.kind == FunctionKind::Helper {
        if func.params.len() > func.locals.len() {
            errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} helper `{}` has {} params but only {} locals",
                    fid.0,
                    func.name,
                    func.params.len(),
                    func.locals.len()
                ),
            });
        }
        for (index, param) in func.params.iter().enumerate() {
            match func.locals.get(index) {
                Some(local) if local.ty != param.ty => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} helper `{}` param {} metadata {:?} does not match mirrored local {:?}",
                        fid.0, func.name, index, param.ty, local.ty
                    ),
                }),
                Some(local) if !helper_abi_type_valid(&local.ty, prog.records.len()) => {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} helper `{}` param {} must use the scalar helper ABI or fixed-vector extension, got {:?}",
                            fid.0, func.name, index, local.ty
                        ),
                    });
                }
                _ => {}
            }
        }
        if let Some(ret) = func.ret {
            match func.locals.get(ret.index()) {
                Some(local) if !helper_abi_type_valid(&local.ty, prog.records.len()) => {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} helper `{}` return %{} must use the scalar helper ABI or fixed-vector extension, got {:?}",
                            fid.0, func.name, ret.0, local.ty
                        ),
                    });
                }
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} helper `{}` return %{} does not resolve",
                        fid.0, func.name, ret.0
                    ),
                }),
                _ => {}
            }
        }
    }

    if let FunctionKind::Tseq { elem } = &func.kind {
        let expected = elem.seq_type();
        match func.ret.and_then(|ret| func.locals.get(ret.index())) {
            Some(local) if local.ty == expected => {}
            Some(local) => errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} tseq `{}` element metadata expects return {:?}, got {:?}",
                    fid.0, func.name, expected, local.ty
                ),
            }),
            None => errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} tseq `{}` has no resolvable sequence return local",
                    fid.0, func.name
                ),
            }),
        }
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

    // Backend-critical return provenance: post method hooks and hooked
    // covergroups fire only for the Return blocks listed here. A stale block
    // id (or a non-Return block) would silently suppress or move fan-out.
    let mut seen_implicit_returns = HashSet::new();
    for block in &func.implicit_returns {
        let valid = func
            .blocks
            .get(block.index())
            .is_some_and(|basic| matches!(basic.terminator, Terminator::Return));
        if !valid || !seen_implicit_returns.insert(*block) {
            errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} implicit return b{} must name one distinct Return block",
                    fid.0, block.0
                ),
            });
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
            temporal_slots_ok: false,
        };
        ck.check_block(b);
    }

    check_port_snapshot_definitions(func, fid, &mut errs);

    // Invariant 4 — forward dataflow: a local must be defined on every
    // path from entry before its first read. Params count as defined.
    check_def_before_use(prog, func, fid, &reachable, &mut errs);

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
    /// Concurrent property/cover roots are stored outside the CFG but
    /// execute in the registering function's context. Their temporal
    /// slots are valid only while explicitly walking those side tables.
    temporal_slots_ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WholeCollectionShape {
    FixedVec(usize, String),
    DynamicSeq(String),
}

impl Checker<'_> {
    fn check_truth_expr(&mut self, expr: &Expr, ports_ok: bool, context: &'static str) {
        self.check_expr(expr, ports_ok, context);
        let ty = self.aggregate_assignment_expr_type(expr);
        if matches!(
            ty,
            Some(IrType::Record(_) | IrType::Seq(_) | IrType::FixedVec { .. })
        )
            || self.contains_invalid_record_composition(expr)
        {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} {context} is not a scalar truth value",
                    self.fid.0, self.bid.0
                ),
            });
        }
    }

    fn check_scalar_value_expr(&mut self, expr: &Expr, ports_ok: bool, context: &'static str) {
        self.check_expr(expr, ports_ok, context);
        if matches!(
            self.aggregate_assignment_expr_type(expr),
            Some(IrType::Record(_) | IrType::Seq(_) | IrType::FixedVec { .. })
        ) || self.contains_invalid_record_composition(expr)
        {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} {context} is not a scalar value",
                    self.fid.0, self.bid.0
                ),
            });
        }
    }

    fn check_event_payload_value(
        &mut self,
        payload: EventPayload,
        expr: &Expr,
        context: &'static str,
    ) {
        if matches!(payload, EventPayload::FixedVec { .. }) {
            if self.contains_invalid_record_composition(expr) {
                self.errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} b{} {context} contains an invalid record composition",
                        self.fid.0, self.bid.0
                    ),
                });
            }
            // Event delivery is the one sanctioned whole-value use for a
            // recursively nested component vector. The ordinary whole-vector
            // equality/copy classifier intentionally keeps its existing
            // one-dimensional boundary, so validate this receiver directly
            // against the event's exact aggregate type.
            let actual = if let Expr::ComponentField { base, field } = expr {
                match self.component_field_whole_vec_type(base, field) {
                    Ok(actual) => actual,
                    Err(detail) => {
                        self.report_bad_component_field(detail);
                        None
                    }
                }
            } else {
                self.check_expr_inner(expr, false, context, true);
                self.expr_whole_vec_type(expr).ok().flatten()
            };
            if actual != Some(payload.value_ir_type()) {
                self.errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} b{} {context} has aggregate type {actual:?}, incompatible with payload {payload:?}",
                        self.fid.0, self.bid.0
                    ),
                });
            }
            return;
        }
        self.check_expr(expr, false, context);
        let actual = self
            .aggregate_assignment_expr_type(expr)
            .unwrap_or(IrType::Unknown);
        if !event_payload_accepts_value_type(&payload, &actual) {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} {context} has type {actual:?}, incompatible with payload {payload:?}",
                    self.fid.0, self.bid.0
                ),
            });
        }
    }

    fn check_property_schema(&mut self, id: PropertyCheckId, schema: &PropertyCheckSchema) {
        let saved = self.temporal_slots_ok;
        self.temporal_slots_ok = true;
        match &schema.shape {
            PropertyShape::Implies { ante, cons } | PropertyShape::ImpliesNext { ante, cons } => {
                self.check_truth_expr(ante, true, "property-check antecedent");
                self.check_truth_expr(cons, true, "property-check consequent");
            }
            PropertyShape::Invariant(expr) => {
                self.check_truth_expr(expr, true, "property-check invariant");
            }
        }
        self.temporal_slots_ok = saved;
        for (slot, temporal) in schema.temporals.iter().enumerate() {
            self.check_scalar_value_expr(&temporal.inner, true, "property-check temporal operand");
            check_temporal_slots(
                &temporal.inner,
                0,
                &format!("property check p{} latch operand {slot}", id.0),
                self.errs,
            );
        }
        if let Some(message) = &schema.message {
            self.check_fmt_args(message);
        }
    }

    fn check_cover_schema(&mut self, id: CoverCheckId, schema: &CoverCheckSchema) {
        let saved = self.temporal_slots_ok;
        self.temporal_slots_ok = true;
        self.check_truth_expr(&schema.cond, true, "cover-check condition");
        self.temporal_slots_ok = saved;
        for (slot, temporal) in schema.temporals.iter().enumerate() {
            self.check_scalar_value_expr(&temporal.inner, true, "cover-check temporal operand");
            check_temporal_slots(
                &temporal.inner,
                0,
                &format!("cover check c{} latch operand {slot}", id.0),
                self.errs,
            );
        }
    }

    /// Resolve the component selected by an emitted field-access base.
    /// Lowering stores paths rather than a redundant component id on
    /// `ComponentField`, so the verifier must replay the path walk before
    /// codegen is allowed to trust the member name.
    fn component_base_id(&self, base: &ComponentBase) -> Result<ComponentId, String> {
        match base {
            ComponentBase::SelfField => match self.func.kind {
                FunctionKind::ComponentMethod { component } => self
                    .prog
                    .components
                    .get(component.index())
                    .map(|_| component)
                    .ok_or_else(|| format!("self component c{} does not resolve", component.0)),
                _ => Err("self-relative component field outside a component method".to_string()),
            },
            ComponentBase::Path(path) => {
                let (root, tail) = path
                    .split_first()
                    .ok_or_else(|| "empty component field path".to_string())?;
                // Match lowering's namespace order: a real testbench
                // component field named `self` wins over the synthetic
                // method-relative root of the same spelling.
                let owner_tb = self
                    .func
                    .owner
                    .and_then(|owner| self.prog.testbenches.get(owner.index()));
                if let Some(tb) = owner_tb.filter(|tb| {
                    tb.component_fields
                        .iter()
                        .any(|binding| binding.field == *root)
                }) {
                    resolve_testbench_component_path(self.prog, tb, path)
                } else if root == "self" {
                    let FunctionKind::ComponentMethod { component } = self.func.kind else {
                        return Err(
                            "self-rooted component path outside a component method".to_string()
                        );
                    };
                    let mut cid = component;
                    for segment in tail {
                        let schema = self
                            .prog
                            .components
                            .get(cid.index())
                            .ok_or_else(|| format!("component c{} does not resolve", cid.0))?;
                        cid = match schema.field(segment) {
                            Some(ComponentFieldSchema {
                                kind: ComponentFieldKind::Sub { component, .. },
                                ..
                            }) => *component,
                            _ => {
                                return Err(format!(
                                    "self path segment `{segment}` is not a sub-component"
                                ))
                            }
                        };
                    }
                    self.prog
                        .components
                        .get(cid.index())
                        .map(|_| cid)
                        .ok_or_else(|| format!("component c{} does not resolve", cid.0))
                } else {
                    let owner = self
                        .func
                        .owner
                        .ok_or_else(|| "component path has no owning testbench".to_string())?;
                    let tb = self
                        .prog
                        .testbenches
                        .get(owner.index())
                        .ok_or_else(|| format!("owner tb{} does not resolve", owner.0))?;
                    resolve_testbench_component_path(self.prog, tb, path)
                }
            }
            ComponentBase::Local(local) => Err(format!(
                "component field uses local base %{}; only method calls may use local component values",
                local.0
            )),
        }
    }

    /// Validate the receiver and relative leaf selected by a component idle
    /// predicate. Unlike ordinary component fields, predicates may use a
    /// component-typed parameter as their base.
    fn check_component_idle(&mut self, base: &ComponentBase, subpath: &[String]) {
        let mut component = match base {
            ComponentBase::Local(local) => {
                self.check_local(*local);
                match self.func.locals.get(local.index()).map(|entry| &entry.ty) {
                    Some(IrType::Component(component))
                        if self.prog.components.get(component.index()).is_some() =>
                    {
                        *component
                    }
                    Some(IrType::Component(component)) => {
                        self.report_bad_component_field(format!(
                            "component idle local %{} references missing component c{}",
                            local.0, component.0
                        ));
                        return;
                    }
                    _ => {
                        self.report_bad_component_field(format!(
                            "component idle local %{} is not component-typed",
                            local.0
                        ));
                        return;
                    }
                }
            }
            _ => match self.component_base_id(base) {
                Ok(component) => component,
                Err(detail) => {
                    self.report_bad_component_field(format!(
                        "component idle base does not resolve: {detail}"
                    ));
                    return;
                }
            },
        };

        for (position, segment) in subpath.iter().enumerate() {
            let terminal = position + 1 == subpath.len();
            let Some(schema) = self.prog.components.get(component.index()) else {
                self.report_bad_component_field(format!(
                    "component idle path reaches missing component c{}",
                    component.0
                ));
                return;
            };
            match schema.field(segment).map(|field| &field.kind) {
                Some(ComponentFieldKind::Sub {
                    component: next, ..
                }) => component = *next,
                Some(ComponentFieldKind::ScoreboardSub { scoreboard }) if terminal => {
                    if self.prog.scoreboards.get(scoreboard.index()).is_none() {
                        self.report_bad_component_field(format!(
                            "component idle leaf `{segment}` references missing scoreboard sb{}",
                            scoreboard.0
                        ));
                    }
                    return;
                }
                _ => {
                    self.report_bad_component_field(format!(
                        "component idle path `{}` has invalid segment `{segment}`",
                        subpath.join(".")
                    ));
                    return;
                }
            }
        }
        if self.prog.components.get(component.index()).is_none() {
            self.report_bad_component_field(format!(
                "component idle path reaches missing component c{}",
                component.0
            ));
        }
    }

    fn component_emit_payload(
        &self,
        base: &ComponentBase,
        subpath: &[String],
        event: &str,
    ) -> Result<EventPayload, String> {
        let (mut component, local_effective_mode) = match base {
            ComponentBase::Local(local) => {
                let root = match self.func.locals.get(local.index()).map(|l| &l.ty) {
                    Some(IrType::Component(component)) => *component,
                    _ => return Err(format!("local %{} is not component-typed", local.0)),
                };
                let resolved = resolve_component_path_mode(&self.prog.components, root, None, subpath)
                    .map_err(|err| err.to_string())?;
                (resolved.component, resolved.effective_mode)
            }
            _ => (self.component_base_id(base)?, None),
        };
        for segment in if matches!(base, ComponentBase::Local(_)) {
            &[][..]
        } else {
            subpath
        } {
            let schema = self
                .prog
                .components
                .get(component.index())
                .ok_or_else(|| format!("references missing component c{}", component.0))?;
            component = match schema.field(segment).map(|field| &field.kind) {
                Some(ComponentFieldKind::Sub { component, .. }) => *component,
                _ => {
                    return Err(format!(
                        "`{segment}` is not a sub-component of `{}`",
                        schema.name
                    ));
                }
            };
        }
        let schema = self
            .prog
            .components
            .get(component.index())
            .ok_or_else(|| format!("references missing component c{}", component.0))?;
        let field = schema
            .field(event)
            .ok_or_else(|| format!("component `{}` has no field `{event}`", schema.name))?;
        let ComponentFieldKind::Event { payload } = &field.kind else {
            return Err(format!(
                "component `{}.{event}` is not an event field",
                schema.name
            ));
        };
        let payload = payload.clone();
        match base {
            ComponentBase::Path(_) => {
                verify_component_event_ref(self.prog, self.func, base, component, event, &payload)?;
            }
            ComponentBase::SelfField => {
                let active_context = schema.methods.iter().any(|m| {
                    m.function == self.fid && matches!(m.activation, Activation::ActiveOnly)
                }) || schema.on_handlers.iter().any(|h| {
                    h.function == self.fid && matches!(h.activation, Activation::ActiveOnly)
                }) || schema.periodic_handlers.iter().any(|h| {
                    h.function == self.fid && matches!(h.activation, Activation::ActiveOnly)
                }) || schema.cycle_handlers.iter().any(|h| {
                    h.function == self.fid && matches!(h.activation, Activation::ActiveOnly)
                }) || schema.watchdog.as_ref().is_some_and(|w| {
                    w.function == self.fid && matches!(w.activation, Activation::ActiveOnly)
                });
                if matches!(field.activation, Activation::ActiveOnly) && !active_context {
                    return Err(format!(
                        "always-on component body cannot emit active-only event `{event}`"
                    ));
                }
            }
            // The root binding mode of a component parameter is unknown, but
            // an explicit passive descendant mode is statically decisive.
            ComponentBase::Local(_)
                if matches!(field.activation, Activation::ActiveOnly)
                    && matches!(
                        local_effective_mode,
                        Some(ComponentInstanceMode::Passive)
                    ) =>
            {
                return Err(format!(
                    "active-only component event `{event}` is disabled by a passive \
                     component-parameter descendant"
                ));
            }
            ComponentBase::Local(_) => {}
        }
        Ok(payload)
    }

    /// Validate a component member suffix and return its whole fixed-vector
    /// C++ shape when the suffix names one. `None` is a valid scalar or
    /// whole-record field. Queue/event/sub-component members have dedicated
    /// IR nodes and are invalid here.
    fn component_field_vec_shape(
        &self,
        base: &ComponentBase,
        field: &str,
    ) -> Result<Option<(usize, String)>, String> {
        let cid = self.component_base_id(base)?;
        let component = self
            .prog
            .components
            .get(cid.index())
            .ok_or_else(|| format!("component c{} does not resolve", cid.0))?;
        let mut segments = field.split('.');
        let root = segments
            .next()
            .filter(|segment| !segment.is_empty())
            .ok_or_else(|| "empty component field name".to_string())?;
        let root = component
            .field(root)
            .ok_or_else(|| format!("component `{}` has no field `{root}`", component.name))?;
        match &root.kind {
            ComponentFieldKind::Scalar { .. } => {
                if segments.next().is_some() {
                    Err(format!("scalar component field `{field}` has a subfield"))
                } else {
                    Ok(None)
                }
            }
            ComponentFieldKind::FixedVec(vec) => {
                if segments.next().is_some() {
                    return Err(format!(
                        "fixed-vector component field `{field}` has a subfield"
                    ));
                }
                crate::codegen::cpp_tb::ir_vec_elem_class(&vec.elem)
                    .map(|elem| Some((vec.len, elem)))
                    .ok_or_else(|| {
                        format!(
                            "fixed-vector component field `{field}` has an invalid element type"
                        )
                    })
            }
            ComponentFieldKind::Record { record } => {
                let mut rid = *record;
                let rest: Vec<&str> = segments.collect();
                if rest.is_empty() {
                    return Ok(None);
                }
                for (index, segment) in rest.iter().enumerate() {
                    let schema = self
                        .prog
                        .records
                        .get(rid.index())
                        .ok_or_else(|| format!("record r{} does not resolve", rid.0))?;
                    let member = schema.field(segment).ok_or_else(|| {
                        format!("record `{}` has no field `{segment}`", schema.name)
                    })?;
                    let last = index + 1 == rest.len();
                    if last {
                        return match member.vec_len {
                            Some(len) => crate::codegen::cpp_tb::ir_vec_elem_class(&member.ty)
                                .map(|elem| Some((len, elem)))
                                .ok_or_else(|| {
                                    format!(
                                        "record vector field `{field}` has an invalid element type"
                                    )
                                }),
                            None => Ok(None),
                        };
                    }
                    match member.ty {
                        IrType::Record(next) if member.vec_len.is_none() => rid = next,
                        _ => {
                            return Err(format!(
                                "component record path `{field}` traverses non-record `{segment}`"
                            ))
                        }
                    }
                }
                unreachable!("non-empty record suffix returned from the loop")
            }
            ComponentFieldKind::Queue { .. }
            | ComponentFieldKind::Event { .. }
            | ComponentFieldKind::Sub { .. }
            | ComponentFieldKind::ScoreboardSub { .. }
            | ComponentFieldKind::Dut { .. } => Err(format!(
                "component member `{field}` is not a scalar, record, or fixed-vector field"
            )),
        }
    }

    fn component_field_whole_vec_type(
        &self,
        base: &ComponentBase,
        field: &str,
    ) -> Result<Option<IrType>, String> {
        let cid = self.component_base_id(base)?;
        let component = self
            .prog
            .components
            .get(cid.index())
            .ok_or_else(|| format!("component c{} does not resolve", cid.0))?;
        if field.contains('.') {
            return Ok(None);
        }
        Ok(component.field(field).and_then(|field| match &field.kind {
            ComponentFieldKind::FixedVec(vec) => Some(IrType::FixedVec {
                elem: Box::new(vec.elem.clone()),
                len: vec.len,
            }),
            _ => None,
        }))
    }

    fn component_indexed_field_type(
        &self,
        base: &ComponentBase,
        field: &str,
        index_pos: usize,
    ) -> Result<(IrType, usize), String> {
        let cid = self.component_base_id(base)?;
        let component = self
            .prog
            .components
            .get(cid.index())
            .ok_or_else(|| format!("component c{} does not resolve", cid.0))?;
        let segments: Vec<&str> = field.split('.').collect();
        let root = component
            .field(segments.first().copied().unwrap_or_default())
            .ok_or_else(|| {
                format!(
                    "component `{}` has no field `{}`",
                    component.name,
                    segments.first().copied().unwrap_or_default()
                )
            })?;
        match &root.kind {
            ComponentFieldKind::FixedVec(vec) if segments.len() == 1 && index_pos == 0 => {
                Ok((vec.elem.clone(), vec.len))
            }
            ComponentFieldKind::Record { record } if index_pos > 0 => {
                let mut rid = *record;
                let mut selected_len = None;
                for (offset, segment) in segments[1..].iter().enumerate() {
                    let pos = offset + 1;
                    let schema = self
                        .prog
                        .records
                        .get(rid.index())
                        .ok_or_else(|| format!("record r{} does not resolve", rid.0))?;
                    let member = schema.field(segment).ok_or_else(|| {
                        format!("record `{}` has no field `{segment}`", schema.name)
                    })?;
                    let indexed = pos == index_pos;
                    if indexed {
                        selected_len = Some(member.vec_len.ok_or_else(|| {
                            format!(
                                "indexed component field `{field}` selects non-vector `{segment}`"
                            )
                        })?);
                    }
                    if offset + 1 == segments.len() - 1 {
                        let len = selected_len.ok_or_else(|| {
                            format!(
                                "indexed component field `{field}` has no selection at position {index_pos}"
                            )
                        })?;
                        return Ok((member.ty.clone(), len));
                    }
                    match member.ty {
                        IrType::Record(next) if member.vec_len.is_none() && !indexed => rid = next,
                        IrType::Record(next) if member.vec_len.is_some() && indexed => rid = next,
                        _ => {
                            return Err(format!(
                            "indexed component path `{field}` has invalid selection at `{segment}`"
                        ))
                        }
                    }
                }
                Err(format!(
                    "indexed component path `{field}` has no record leaf"
                ))
            }
            _ => Err(format!(
                "indexed component field `{field}` does not select a fixed vector"
            )),
        }
    }

    fn record_path_vec_shape(
        &self,
        mut record: RecordId,
        segments: &[&str],
        mid_indices: &[usize],
    ) -> Result<Option<(usize, String)>, String> {
        if segments.is_empty() {
            return Ok(None);
        }
        for (position, segment) in segments.iter().enumerate() {
            let schema = self
                .prog
                .records
                .get(record.index())
                .ok_or_else(|| format!("record r{} does not resolve", record.0))?;
            let member = schema
                .field(segment)
                .ok_or_else(|| format!("record `{}` has no field `{segment}`", schema.name))?;
            let last = position + 1 == segments.len();
            if last {
                let mut len = member.vec_len;
                let mut ty = member.ty.clone();
                for _ in mid_indices.iter().filter(|p| **p == position) {
                    let Some(_) = len else {
                        return Err(format!(
                            "record path over-indexes fixed-vector field `{segment}`"
                        ));
                    };
                    match ty {
                        IrType::FixedVec {
                            elem,
                            len: inner_len,
                        } => {
                            len = Some(inner_len);
                            ty = *elem;
                        }
                        _ => len = None,
                    }
                }
                return match len {
                    Some(len) => crate::codegen::cpp_tb::ir_vec_elem_class(&ty)
                        .map(|elem| Some((len, elem)))
                        .ok_or_else(|| {
                            format!("record vector field `{segment}` has an invalid element type")
                        }),
                    None => Ok(None),
                };
            }
            let indexed = mid_indices.contains(&position);
            match member.ty {
                IrType::Record(next) if member.vec_len.is_none() == !indexed => record = next,
                _ => {
                    return Err(format!(
                        "record path traverses non-record or unindexed vector field `{segment}`"
                    ))
                }
            }
        }
        unreachable!("non-empty record path returned from the loop")
    }

    fn record_field_vec_shape(
        &self,
        local: LocalId,
        field: &str,
        path: &[String],
        mid_indices: &[(usize, Expr)],
        index: Option<&Expr>,
    ) -> Result<Option<(usize, String)>, String> {
        let record = match self.func.locals.get(local.index()).map(|local| &local.ty) {
            Some(IrType::Record(record)) => *record,
            _ => return Err(format!("local %{} is not record-typed", local.0)),
        };
        let segments: Vec<&str> = std::iter::once(field)
            .chain(path.iter().map(String::as_str))
            .collect();
        let positions: Vec<usize> = mid_indices.iter().map(|(position, _)| *position).collect();
        // Verify every literal selector against the layer it consumes. The
        // final selector is stored separately from path selectors, but both
        // participate in the same recursive fixed-vector shape.
        let mut rid = record;
        for (position, segment) in segments.iter().enumerate() {
            let member = self
                .prog
                .records
                .get(rid.index())
                .and_then(|schema| schema.field(segment))
                .ok_or_else(|| format!("record path field `{segment}` does not resolve"))?;
            let last = position + 1 == segments.len();
            let selected: Vec<&Expr> = mid_indices
                .iter()
                .filter(|(p, _)| *p == position)
                .map(|(_, expr)| expr)
                .collect();
            if last {
                let mut len = member.vec_len;
                let mut ty = member.ty.clone();
                for selector in selected.into_iter().chain(index.into_iter()) {
                    let selected_len = len.ok_or_else(|| {
                        format!("record path over-indexes fixed-vector field `{segment}`")
                    })?;
                    if matches!(selector, Expr::Literal { value, .. } if *value as usize >= selected_len)
                    {
                        return Err(format!(
                            "record path selects `{segment}` out of bounds for length {selected_len}"
                        ));
                    }
                    match ty {
                        IrType::FixedVec {
                            elem,
                            len: inner_len,
                        } => {
                            len = Some(inner_len);
                            ty = *elem;
                        }
                        _ => len = None,
                    }
                }
            } else {
                if selected.len() > 1 {
                    return Err(format!("record path over-indexes record field `{segment}`"));
                }
                if let Some(selector) = selected.first() {
                    let selected_len = member.vec_len.ok_or_else(|| {
                        format!("record path indexes non-vector field `{segment}`")
                    })?;
                    if matches!(selector, Expr::Literal { value, .. } if *value as usize >= selected_len)
                    {
                        return Err(format!(
                            "record path selects `{segment}` out of bounds for length {selected_len}"
                        ));
                    }
                }
                let indexed = !selected.is_empty();
                match member.ty {
                    IrType::Record(next)
                        if (member.vec_len.is_none() && !indexed)
                            || (member.vec_len.is_some() && indexed) =>
                    {
                        rid = next
                    }
                    _ => return Err(format!("record path cannot traverse `{segment}`")),
                }
            }
        }
        if index.is_some() {
            if matches!(
                self.record_path_leaf_type(record, &segments, &positions),
                Some(IrType::Seq(_))
            ) {
                return Err(
                    "a dynamic-list record leaf cannot carry a fixed-vector element index"
                        .to_string(),
                );
            }
            self.record_path_vec_shape(record, &segments, &positions)?
                .ok_or_else(|| {
                    "an indexed record leaf has no remaining vector layer".to_string()
                })?;
            return Ok(None);
        }
        self.record_path_vec_shape(record, &segments, &positions)
    }

    fn record_field_type(
        &self,
        local: LocalId,
        field: &str,
        path: &[String],
        mid_indices: &[(usize, Expr)],
    ) -> Option<IrType> {
        let IrType::Record(record) = self.func.locals.get(local.index())?.ty else {
            return None;
        };
        let segments: Vec<&str> = std::iter::once(field)
            .chain(path.iter().map(String::as_str))
            .collect();
        let positions: Vec<usize> = mid_indices.iter().map(|(position, _)| *position).collect();
        self.record_path_leaf_type(record, &segments, &positions)
    }

    fn transactor_state_field(
        &self,
        instance: &str,
        field: &str,
    ) -> Result<&StateFieldSchema, String> {
        let body_transactor = match self.func.kind {
            FunctionKind::TransactorBody { transactor } => Some(transactor),
            _ => None,
        };
        let transactor = if let Some(transactor) =
            body_transactor.filter(|_| instance.is_empty() || self.func.owner.is_none())
        {
            // Target responder bodies are schema functions shared by every
            // binding, so they have no owning testbench even after binding
            // fills the emission placeholder with an instance name.
            transactor
        } else if instance.is_empty() {
            return Err(
                "placeholder transactor-state instance outside a transactor body".to_string(),
            );
        } else {
            let owner = self
                .func
                .owner
                .ok_or_else(|| "transactor-state access has no owning testbench".to_string())?;
            let tb = self
                .prog
                .testbenches
                .get(owner.index())
                .ok_or_else(|| format!("owner tb{} does not resolve", owner.0))?;
            tb.target_tlm_actors
                .iter()
                .find(|actor| actor.instance == instance)
                .map(|actor| actor.transactor)
                .or_else(|| {
                    tb.unbound_state_actors
                        .iter()
                        .find(|actor| actor.field == instance)
                        .map(|actor| actor.transactor)
                })
                .ok_or_else(|| format!("transactor state instance `{instance}` does not resolve"))?
        };
        let schema = self
            .prog
            .transactors
            .get(transactor.index())
            .ok_or_else(|| format!("transactor x{} does not resolve", transactor.0))?;
        schema
            .state_fields
            .iter()
            .find(|state| state.name == field)
            .ok_or_else(|| format!("transactor `{}` has no state field `{field}`", schema.name))
    }

    fn transactor_state_record(&self, instance: &str, field: &str) -> Result<RecordId, String> {
        match &self.transactor_state_field(instance, field)?.kind {
            StateFieldKind::Record { record } => Ok(*record),
            _ => Err(format!(
                "transactor state field `{instance}.{field}` is not record-typed"
            )),
        }
    }

    fn record_path_leaf_type(
        &self,
        mut record: RecordId,
        segments: &[&str],
        mid_positions: &[usize],
    ) -> Option<IrType> {
        for (position, segment) in segments.iter().enumerate() {
            let member = self.prog.records.get(record.index())?.field(segment)?;
            if position + 1 == segments.len() {
                let mut ty = member.ty.clone();
                for _ in mid_positions.iter().filter(|p| **p == position) {
                    ty = match ty {
                        IrType::FixedVec { elem, .. } => *elem,
                        _ => return None,
                    };
                }
                return Some(ty);
            }
            let indexed = mid_positions.contains(&position);
            match member.ty {
                IrType::Record(next) if member.vec_len.is_none() == !indexed => record = next,
                _ => return None,
            }
        }
        None
    }

    fn component_field_type(&self, base: &ComponentBase, field: &str) -> Option<IrType> {
        let component = self
            .prog
            .components
            .get(self.component_base_id(base).ok()?.index())?;
        let segments: Vec<&str> = field.split('.').collect();
        let root = component.field(*segments.first()?)?;
        if segments.len() == 1 {
            return match &root.kind {
                ComponentFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                ComponentFieldKind::FixedVec(vec) => Some(vec.elem.clone()),
                ComponentFieldKind::Record { record } => Some(IrType::Record(*record)),
                _ => None,
            };
        }
        let ComponentFieldKind::Record { record } = root.kind else {
            return None;
        };
        self.record_path_leaf_type(record, &segments[1..], &[])
    }

    /// Type record-bearing RHS forms that the general scalar expression
    /// classifier intentionally does not understand. Aggregate destinations
    /// must enforce record identity even after another pass rewrites a valid
    /// local RHS into a component/state/record-field expression.
    fn aggregate_assignment_expr_type(&self, value: &Expr) -> Option<IrType> {
        match value {
            Expr::Ternary(_, then_expr, else_expr) => {
                let then_ty = self.aggregate_assignment_expr_type(then_expr);
                let else_ty = self.aggregate_assignment_expr_type(else_expr);
                match (then_ty, else_ty) {
                    (Some(IrType::Record(lhs)), Some(IrType::Record(rhs))) if lhs == rhs => {
                        Some(IrType::Record(lhs))
                    }
                    // A record mixed with another record identity, a scalar,
                    // or an unclassifiable arm is never a sound record value.
                    (Some(IrType::Record(_)), _) | (_, Some(IrType::Record(_))) => {
                        Some(IrType::Unknown)
                    }
                    (lhs, rhs) => common_scalar_expr_type(lhs, rhs).or(Some(IrType::Unknown)),
                }
            }
            Expr::TbField(field) => self
                .func
                .owner
                .and_then(|owner| self.prog.testbenches.get(owner.index()))
                .and_then(|tb| tb.scalar_fields.iter().find(|f| f.name == *field))
                .map(|field| field.ty.clone())
                .or(Some(IrType::Unknown)),
            Expr::SeqIndex { seq, .. } => match self.func.locals.get(seq.index()).map(|l| &l.ty) {
                Some(IrType::RecordSeq(record)) => Some(IrType::Record(*record)),
                Some(IrType::Seq(scalar)) => Some((**scalar).clone()),
                _ => Some(IrType::Unknown),
            },
            Expr::ComponentValue { base } => self
                .component_base_id(base)
                .ok()
                .map(IrType::Component)
                .or(Some(IrType::Unknown)),
            Expr::ComponentField { base, field } => self
                .component_field_type(base, field)
                .or(Some(IrType::Unknown)),
            Expr::ComponentVecElement {
                base,
                field,
                index_pos,
                inner_index,
                ..
            } => self
                .component_indexed_field_type(base, field, *index_pos)
                .ok()
                .map(|(ty, _)| match (ty, inner_index) {
                    (IrType::FixedVec { elem, .. }, Some(_)) => *elem,
                    (ty, _) => ty,
                })
                .or(Some(IrType::Unknown)),
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                ..
            } => {
                let Some(IrType::Record(record)) = self
                    .func
                    .locals
                    .get(local.index())
                    .map(|local| local.ty.clone())
                else {
                    return Some(IrType::Unknown);
                };
                let segments: Vec<&str> = std::iter::once(field.as_str())
                    .chain(path.iter().map(String::as_str))
                    .collect();
                let positions: Vec<usize> = mid_indices.iter().map(|(pos, _)| *pos).collect();
                self.record_path_leaf_type(record, &segments, &positions)
                    .or(Some(IrType::Unknown))
            }
            Expr::TransactorState { instance, field } => {
                let Ok(state) = self.transactor_state_field(instance, field) else {
                    return Some(IrType::Unknown);
                };
                match &state.kind {
                    StateFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                    StateFieldKind::Record { record } => Some(IrType::Record(*record)),
                    StateFieldKind::FixedVec { ty } => Some(ty.clone()),
                    StateFieldKind::Queue { .. } => Some(IrType::Unknown),
                }
            }
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                ..
            } => self
                .transactor_state_record_field_type(instance, field, path, mid_indices)
                .or(Some(IrType::Unknown)),
            _ => assignment_expr_type(self.prog, self.func, value).or(Some(IrType::Unknown)),
        }
    }

    /// Exact fixed-vector type of a whole-value RHS. Unlike the general
    /// assignment classifier, this never applies scalar common-type rules to
    /// aggregate ternary arms: both arms must independently be the same
    /// fixed-vector type.
    fn exact_fixed_vec_expr_type(&self, value: &Expr) -> Option<IrType> {
        match value {
            Expr::Ternary(cond, then_expr, else_expr) => {
                if !matches!(
                    self.aggregate_assignment_expr_type(cond),
                    Some(IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown)
                ) {
                    return None;
                }
                let then_ty = self.exact_fixed_vec_expr_type(then_expr)?;
                let else_ty = self.exact_fixed_vec_expr_type(else_expr)?;
                (then_ty == else_ty).then_some(then_ty)
            }
            _ => {
                let ty = self.aggregate_assignment_expr_type(value)?;
                if matches!(ty, IrType::FixedVec { .. }) {
                    return Some(ty);
                }
                let Ok(Some((len, _))) = self.expr_whole_vec_shape(value) else {
                    return None;
                };
                Some(IrType::FixedVec {
                    elem: Box::new(ty),
                    len,
                })
            }
        }
    }

    /// Reject record/fixed-vector values consumed by scalar operators, plus
    /// aggregate ternaries whose arms cannot denote one exact aggregate type.
    /// The general scalar classifier intentionally returns `Unknown` for
    /// several of these forms; aggregate destinations must not interpret that
    /// as a wildcard.
    fn contains_invalid_record_composition(&self, value: &Expr) -> bool {
        if let Expr::Ternary(cond, then_expr, else_expr) = value {
            if matches!(
                self.aggregate_assignment_expr_type(cond),
                Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
            ) {
                return true;
            }
            let then_ty = self.aggregate_assignment_expr_type(then_expr);
            let else_ty = self.aggregate_assignment_expr_type(else_expr);
            let aggregate = |ty: &Option<IrType>| {
                matches!(
                    ty,
                    Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
                )
            };
            if (aggregate(&then_ty) || aggregate(&else_ty)) && then_ty != else_ty {
                return true;
            }
        }
        match value {
            Expr::Binary(op, lhs, rhs) => {
                let lhs_ty = self.aggregate_assignment_expr_type(lhs);
                let rhs_ty = self.aggregate_assignment_expr_type(rhs);
                let invalid_operands = if matches!(op, BinOp::Eq | BinOp::Ne) {
                    let aggregate = |ty: &Option<IrType>| {
                        matches!(
                            ty,
                            Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
                        )
                    };
                    (aggregate(&lhs_ty) || aggregate(&rhs_ty)) && lhs_ty != rhs_ty
                } else {
                    matches!(
                        lhs_ty,
                        Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
                    ) || matches!(
                        rhs_ty,
                        Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
                    )
                };
                invalid_operands
                    || self.contains_invalid_record_composition(lhs)
                    || self.contains_invalid_record_composition(rhs)
            }
            Expr::Unary(_, inner) | Expr::BitSlice { target: inner, .. } => {
                matches!(
                    self.aggregate_assignment_expr_type(inner),
                    Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
                ) || self.contains_invalid_record_composition(inner)
            }
            Expr::BitSliceDyn { target, hi, lo } => [target.as_ref(), hi.as_ref(), lo.as_ref()]
                .iter()
                .any(|inner| {
                    matches!(
                        self.aggregate_assignment_expr_type(inner),
                        Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
                    ) || self.contains_invalid_record_composition(inner)
                }),
            Expr::Ternary(cond, then_expr, else_expr) => {
                self.contains_invalid_record_composition(cond)
                    || self.contains_invalid_record_composition(then_expr)
                    || self.contains_invalid_record_composition(else_expr)
            }
            Expr::WidthCast { inner, .. }
            | Expr::ComponentIdle { n: inner, .. }
            | Expr::TransactorIdle { n: inner, .. }
            | Expr::SeqIndex { index: inner, .. } => {
                matches!(
                    self.aggregate_assignment_expr_type(inner),
                    Some(IrType::Record(_) | IrType::FixedVec { .. } | IrType::Seq(_))
                ) || self.contains_invalid_record_composition(inner)
            }
            _ => false,
        }
    }

    fn transactor_state_field_vec_shape(
        &self,
        instance: &str,
        field: &str,
        path: &[String],
        mid_indices: &[(usize, Expr)],
        index: Option<&Expr>,
    ) -> Result<Option<(usize, String)>, String> {
        if path.is_empty() {
            if !mid_indices.is_empty() {
                return Err(format!(
                    "fixed-vector state field `{instance}.{field}` has record-path indices"
                ));
            }
            let StateFieldKind::FixedVec { ty } =
                &self.transactor_state_field(instance, field)?.kind
            else {
                return Err(format!(
                    "transactor state field `{instance}.{field}` has an empty record path"
                ));
            };
            let IrType::FixedVec { elem, len } = ty else {
                return Err(format!(
                    "transactor state field `{instance}.{field}` has malformed vector metadata"
                ));
            };
            let Some(idx) = index else {
                return Err(format!(
                    "fixed-vector state field `{instance}.{field}` empty-path access lacks an index"
                ));
            };
            if !matches!(
                self.aggregate_assignment_expr_type(idx),
                Some(IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown)
            ) {
                return Err(format!(
                    "fixed-vector state field `{instance}.{field}` has a non-scalar index"
                ));
            }
            if matches!(idx, Expr::Literal { value, .. } if *value as usize >= *len) {
                return Err(format!(
                    "fixed-vector state field `{instance}.{field}` index is out of bounds for length {len}"
                ));
            }
            let _ = elem;
            return Ok(None);
        }
        let record = self.transactor_state_record(instance, field)?;
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        if mid_indices.windows(2).any(|pair| pair[0].0 > pair[1].0)
            || mid_indices
                .iter()
                .any(|(position, _)| *position >= segments.len())
        {
            return Err(format!(
                "transactor state path `{instance}.{field}` has malformed index positions"
            ));
        }
        let mut rid = record;
        for (position, segment) in segments.iter().enumerate() {
            let schema = self
                .prog
                .records
                .get(rid.index())
                .ok_or_else(|| format!("record r{} does not resolve", rid.0))?;
            let member = schema
                .field(segment)
                .ok_or_else(|| format!("record `{}` has no field `{segment}`", schema.name))?;
            if position + 1 < segments.len() {
                let selected: Vec<&Expr> = mid_indices
                    .iter()
                    .filter(|(p, _)| *p == position)
                    .map(|(_, e)| e)
                    .collect();
                if selected.len() > 1 {
                    return Err(format!(
                        "indexed state path over-indexes record vector `{segment}`"
                    ));
                }
                let indexed = !selected.is_empty();
                if let Some(idx) = selected.first() {
                    let len = member.vec_len.ok_or_else(|| {
                        format!("indexed state path selects non-vector `{segment}`")
                    })?;
                    if matches!(idx, Expr::Literal { value, .. } if *value as usize >= len) {
                        return Err(format!(
                            "indexed state path selects `{segment}` out of bounds for length {len}"
                        ));
                    }
                }
                match member.ty {
                    IrType::Record(next) if member.vec_len.is_none() && !indexed => rid = next,
                    IrType::Record(next) if member.vec_len.is_some() && indexed => rid = next,
                    _ => return Err(format!("indexed state path cannot traverse `{segment}`")),
                }
            } else {
                let mut len = member.vec_len;
                let mut ty = member.ty.clone();
                for idx in mid_indices
                    .iter()
                    .filter(|(p, _)| *p == position)
                    .map(|(_, e)| e)
                    .chain(index.into_iter())
                {
                    let selected_len = len.ok_or_else(|| {
                        format!("indexed state path selects non-vector `{segment}`")
                    })?;
                    if matches!(idx, Expr::Literal { value, .. } if *value as usize >= selected_len)
                    {
                        return Err(format!(
                            "indexed state path selects `{segment}` out of bounds for length {selected_len}"
                        ));
                    }
                    match ty {
                        IrType::FixedVec {
                            elem,
                            len: inner_len,
                        } => {
                            len = Some(inner_len);
                            ty = *elem;
                        }
                        _ => len = None,
                    }
                }
            }
        }
        let positions: Vec<usize> = mid_indices.iter().map(|(position, _)| *position).collect();
        let shape = self.record_path_vec_shape(record, &segments, &positions)?;
        if index.is_some() {
            Ok(None)
        } else {
            Ok(shape)
        }
    }

    fn transactor_state_record_field_type(
        &self,
        instance: &str,
        field: &str,
        path: &[String],
        mid_indices: &[(usize, Expr)],
    ) -> Option<IrType> {
        if path.is_empty() {
            let StateFieldKind::FixedVec { ty } =
                &self.transactor_state_field(instance, field).ok()?.kind
            else {
                return None;
            };
            let IrType::FixedVec { elem, .. } = ty else {
                return None;
            };
            return Some((**elem).clone());
        }
        let record = self.transactor_state_record(instance, field).ok()?;
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        let positions: Vec<usize> = mid_indices.iter().map(|(position, _)| *position).collect();
        self.record_path_leaf_type(record, &segments, &positions)
    }

    fn expr_whole_vec_shape(&self, expr: &Expr) -> Result<Option<(usize, String)>, String> {
        match expr {
            Expr::ComponentField { base, field } => self.component_field_vec_shape(base, field),
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } => self.record_field_vec_shape(*local, field, path, mid_indices, index.as_deref()),
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index,
            } => self.transactor_state_field_vec_shape(
                instance,
                field,
                path,
                mid_indices,
                index.as_deref(),
            ),
            _ => Ok(None),
        }
    }

    /// Exact whole-vector type for payload compatibility. The shape helper
    /// intentionally collapses scalar widths that share a C++ carrier;
    /// event schemas retain declared element width and signedness.
    fn expr_whole_vec_type(&self, expr: &Expr) -> Result<Option<IrType>, String> {
        if let Expr::Local(local) = expr {
            return Ok(self
                .func
                .locals
                .get(local.index())
                .map(|local| local.ty.clone())
                .filter(|ty| matches!(ty, IrType::FixedVec { .. })));
        }
        if let Expr::TbField(field) = expr {
            return Ok(self
                .tb_scalar_field_ty(field)
                .filter(|ty| matches!(ty, IrType::FixedVec { .. })));
        }
        if let Expr::ComponentField { base, field } = expr {
            if let Some(ty) = self.component_field_whole_vec_type(base, field)? {
                return Ok(Some(ty));
            }
        }
        let Some((len, _)) = self.expr_whole_vec_shape(expr)? else {
            return Ok(None);
        };
        let elem = match expr {
            Expr::ComponentField { base, field } => self.component_field_type(base, field),
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index: None,
            } => self.record_field_type(*local, field, path, mid_indices),
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index: None,
            } => self.transactor_state_record_field_type(instance, field, path, mid_indices),
            _ => None,
        };
        Ok(elem.map(|elem| IrType::FixedVec {
            elem: Box::new(elem),
            len,
        }))
    }

    fn whole_collection_shape(
        &self,
        vec_shape: Result<Option<(usize, String)>, String>,
        ty: Option<IrType>,
    ) -> Result<Option<WholeCollectionShape>, String> {
        match vec_shape? {
            Some((len, elem)) => Ok(Some(WholeCollectionShape::FixedVec(len, elem))),
            None => match ty {
                Some(IrType::FixedVec { elem, len }) => {
                    crate::codegen::cpp_tb::ir_vec_elem_class(&elem)
                        .map(|class| WholeCollectionShape::FixedVec(len, class))
                        .map(Some)
                        .ok_or_else(|| "fixed vector has an invalid element type".to_string())
                }
                Some(IrType::Seq(elem)) => crate::codegen::cpp_tb::ir_vec_elem_class(&elem)
                    .map(WholeCollectionShape::DynamicSeq)
                    .map(Some)
                    .ok_or_else(|| "dynamic list has an invalid element type".to_string()),
                _ => Ok(None),
            },
        }
    }

    fn expr_whole_collection_shape(
        &self,
        expr: &Expr,
    ) -> Result<Option<WholeCollectionShape>, String> {
        self.whole_collection_shape(
            self.expr_whole_vec_shape(expr),
            self.aggregate_assignment_expr_type(expr),
        )
    }

    fn expr_is_record_api_scalar(&self, expr: &Expr) -> bool {
        let is_collection = self
            .expr_whole_collection_shape(expr)
            .is_ok_and(|shape| shape.is_some());
        !is_collection
            && !matches!(
                self.aggregate_assignment_expr_type(expr),
                Some(
                    IrType::Record(_)
                        | IrType::RecordSeq(_)
                        | IrType::Seq(_)
                        | IrType::FixedVec { .. }
                        | IrType::Component(_)
                        | IrType::Event(_)
                )
            )
    }

    fn report_bad_whole_vec_use(&mut self, detail: String) {
        self.errs.push(VerifyError::BadProgramRef {
            what: format!(
                "fn{} b{} whole-vector use: {detail}",
                self.fid.0, self.bid.0
            ),
        });
    }

    fn check_whole_vec_write_value(
        &mut self,
        dst_shape: Result<Option<(usize, String)>, String>,
        dst_ty: Option<IrType>,
        value: &Expr,
        context: &'static str,
    ) {
        let dst_shape = self.whole_collection_shape(dst_shape, dst_ty);
        let rhs_shape = self.expr_whole_collection_shape(value);
        match (dst_shape, rhs_shape) {
            (Ok(Some(dst)), Ok(Some(rhs))) if dst == rhs => {
                self.check_expr_inner(value, false, context, true);
            }
            (Ok(None), Ok(None)) => self.check_expr(value, false, context),
            (Ok(dst), Ok(rhs)) => {
                self.report_bad_whole_vec_use(format!(
                    "write has incompatible destination and value shapes {dst:?} and {rhs:?}"
                ));
                self.check_expr_inner(value, false, context, rhs.is_some());
            }
            (Err(detail), _) | (_, Err(detail)) => {
                self.report_bad_whole_vec_use(detail);
                self.check_expr(value, false, context);
            }
        }
    }

    fn report_bad_component_field(&mut self, detail: String) {
        self.errs.push(VerifyError::BadProgramRef {
            what: format!("fn{} b{} component field: {detail}", self.fid.0, self.bid.0),
        });
    }

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
                        self.check_bus_call_edge(Some(*l), bus_field, method, args);
                        continue;
                    }
                    if let Expr::Call(CallTarget::Tseq(name), args) = e {
                        self.check_tseq_call(*l, name, args);
                        continue;
                    }
                    let fixed_vec_dest = self
                        .func
                        .locals
                        .get(l.index())
                        .and_then(|local| match &local.ty {
                            IrType::FixedVec { elem, len } => {
                                crate::codegen::cpp_tb::ir_vec_elem_class(elem)
                                    .map(|class| (*len, class))
                            }
                            _ => None,
                        });
                    if let Some(shape) = fixed_vec_dest {
                        self.check_whole_vec_write_value(
                            Ok(Some(shape)),
                            self.func.locals.get(l.index()).map(|local| local.ty.clone()),
                            e,
                            "Assign value",
                        );
                    } else {
                        self.check_expr(e, false, "Assign value");
                    }
                    // Invariant 15.
                    if self.func.locals.get(l.index()).is_some() {
                        let expected = &self.func.local(*l).ty;
                        if let Some(actual) = expr_type(self.prog, self.func, e) {
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
                        // The general expression classifier deliberately
                        // leaves aggregate ternaries untyped. For a record
                        // destination, replay both arms so a rewriting pass
                        // cannot hide a different record identity behind arm
                        // order while preserving all historically accepted
                        // non-ternary aggregate assignment forms.
                        let aggregate_actual = self.aggregate_assignment_expr_type(e);
                        let aggregate_incompatible = self.contains_invalid_record_composition(e)
                            || match expected {
                                IrType::Record(_) => {
                                    aggregate_actual.as_ref().is_some_and(|actual| {
                                        *actual != IrType::Unknown
                                            && !aggregate_assignment_compatible(expected, actual)
                                    })
                                }
                                IrType::UInt(_) | IrType::SInt(_) | IrType::Bool => {
                                    matches!(&aggregate_actual, Some(IrType::Record(_)))
                                }
                                _ => false,
                            };
                        if aggregate_incompatible {
                            self.errs.push(VerifyError::TypeMismatch {
                                func: self.fid,
                                block: self.bid,
                                local: *l,
                                expected: expected.clone(),
                                actual: aggregate_actual.unwrap_or(IrType::Unknown),
                            });
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
                Stmt::AggregateInit(l) => {
                    self.check_local(*l);
                    if self
                        .func
                        .locals
                        .get(l.index())
                        .is_some_and(|tl| {
                            !matches!(tl.ty, IrType::FixedVec { .. })
                                || !helper_abi_type_valid(&tl.ty, self.prog.records.len())
                        })
                    {
                        self.errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "fn{} b{} AggregateInit target is not a fixed vector",
                                self.fid.0, self.bid.0
                            ),
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
                    let dst_shape = self.record_field_vec_shape(
                        *local,
                        field,
                        path,
                        mid_indices,
                        index.as_ref(),
                    );
                    let dst_ty = self.record_field_type(*local, field, path, mid_indices);
                    self.check_whole_vec_write_value(
                        dst_shape,
                        dst_ty,
                        value,
                        "RecordFieldWrite value",
                    );
                }
                Stmt::RecordRead {
                    dest,
                    local,
                    regblock,
                    addr,
                } => {
                    self.check_local(*dest);
                    self.check_local(*local);
                    self.check_expr(addr, false, "RecordRead address");
                    let dest_valid = self
                        .func
                        .locals
                        .get(dest.index())
                        .is_some_and(|l| l.ty == IrType::UInt(None));
                    let addr_ty = self.aggregate_assignment_expr_type(addr);
                    let addr_is_collection = self
                        .expr_whole_collection_shape(addr)
                        .is_ok_and(|shape| shape.is_some());
                    let addr_valid = !addr_is_collection
                        && !matches!(
                            addr_ty,
                            Some(
                                IrType::Record(_)
                                    | IrType::RecordSeq(_)
                                    | IrType::Seq(_)
                                    | IrType::FixedVec { .. }
                                    | IrType::Component(_)
                                    | IrType::Event(_)
                            )
                        );
                    let refs_valid = self
                        .prog
                        .regblocks
                        .get(regblock.index())
                        .is_some_and(|rb| {
                            self.func
                                .locals
                                .get(local.index())
                                .is_some_and(|l| l.ty == IrType::Record(rb.record))
                        });
                    if !dest_valid || !addr_valid || !refs_valid {
                        self.errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "invalid RecordRead destination/address/regblock/local in fn{}",
                                self.fid.0
                            ),
                        });
                    }
                }
                Stmt::RecordWrite {
                    local,
                    binding,
                    regblock,
                    addr,
                    value,
                } => {
                    self.check_local(*local);
                    self.check_expr(addr, false, "RecordWrite address");
                    self.check_expr(value, false, "RecordWrite value");
                    let refs_valid = self
                        .prog
                        .regblocks
                        .get(regblock.index())
                        .is_some_and(|rb| {
                            self.func.locals.get(local.index()).is_some_and(|l| {
                                l.name == *binding && l.ty == IrType::Record(rb.record)
                            })
                        });
                    let binding_valid = self
                        .func
                        .owner
                        .and_then(|owner| self.prog.testbenches.get(owner.index()))
                        .is_some_and(|tb| {
                            tb.regblock_bindings.iter().any(|b| {
                                b.field == *binding && b.regblock == *regblock
                            })
                        });
                    if !refs_valid
                        || !binding_valid
                        || !self.expr_is_record_api_scalar(addr)
                        || !self.expr_is_record_api_scalar(value)
                    {
                        self.errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "invalid RecordWrite address/value/binding/regblock/local in fn{}",
                                self.fid.0
                            ),
                        });
                    }
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
                    if let (Some(expected), Some(actual)) = (
                        self.tb_scalar_field_ty(field),
                        // Keep this backstop to expression forms whose type
                        // is explicit in the IR. The assignment typer gives
                        // arithmetic such as `0 - 8` an unsigned width even
                        // when lowering correctly contextualizes it as sint;
                        // treating that approximation as authoritative here
                        // would reject valid signed field writes.
                        expr_type(self.prog, self.func, value),
                    ) {
                        if !assign_compatible(&expected, &actual) {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} writes {:?} into testbench field `{field}` declared {:?}",
                                    self.fid.0, self.bid.0, actual, expected
                                ),
                            });
                        }
                    }
                }
                Stmt::TbQueuePush { field, value } => {
                    self.check_tb_queue(field);
                    self.check_expr(value, false, "TbQueuePush value");
                    if let (Some(elem), Some(actual)) = (
                        self.tb_queue_elem(field),
                        expr_type(self.prog, self.func, value),
                    ) {
                        if !queue_elem_accepts_type(elem, &actual) {
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
                        if !queue_elem_fits_dest(elem, &local.ty) {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} pops testbench queue `{field}` with element {:?} into local %{} declared {:?}",
                                    self.fid.0, self.bid.0, elem, dest.0, local.ty
                                ),
                            });
                        }
                    }
                }
                Stmt::TransactorStateWrite {
                    instance,
                    field,
                    value,
                } => {
                    match self.transactor_state_field(instance, field) {
                        Ok(state) => {
                            let expected = match &state.kind {
                                StateFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                                StateFieldKind::Record { record } => {
                                    Some(IrType::Record(*record))
                                }
                                StateFieldKind::FixedVec { ty } => Some(ty.clone()),
                                StateFieldKind::Queue { .. } => None,
                            };
                            let actual = match &state.kind {
                                StateFieldKind::FixedVec { .. } => {
                                    self.exact_fixed_vec_expr_type(value)
                                }
                                StateFieldKind::Record { .. } => {
                                    self.aggregate_assignment_expr_type(value)
                                }
                                // Mirror TbFieldWrite: arithmetic IR does not
                                // retain the destination context that made
                                // `0 - 8` signed during lowering. Only enforce
                                // scalar types that are explicit in the IR;
                                // otherwise a valid sint state assignment is
                                // misclassified as unsigned here.
                                StateFieldKind::Scalar { .. } => match value {
                                    Expr::Literal {
                                        ty: IrType::Unknown,
                                        ..
                                    } => assignment_expr_type(self.prog, self.func, value),
                                    _ => expr_type(self.prog, self.func, value),
                                },
                                StateFieldKind::Queue { .. } => None,
                            };
                            if let (Some(expected), Some(actual)) = (&expected, actual.as_ref()) {
                                let compatible = if matches!(expected, IrType::FixedVec { .. }) {
                                    fixed_vec_abi_compatible(expected, actual)
                                } else {
                                    aggregate_assignment_compatible(expected, actual)
                                };
                                if !compatible {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} writes transactor state `{instance}.{field}` of type {expected:?} from incompatible type {actual:?}",
                                            self.fid.0, self.bid.0
                                        ),
                                    });
                                }
                            } else if expected.is_none() {
                                self.errs.push(VerifyError::BadProgramRef {
                                    what: format!(
                                        "fn{} b{} writes queue transactor state `{instance}.{field}` as a whole value",
                                        self.fid.0, self.bid.0
                                    ),
                                });
                            } else if matches!(expected, Some(IrType::FixedVec { .. })) {
                                self.errs.push(VerifyError::BadProgramRef {
                                    what: format!(
                                        "fn{} b{} writes fixed-vector transactor state `{instance}.{field}` from a non-matching whole-vector expression",
                                        self.fid.0, self.bid.0
                                    ),
                                });
                            }
                            self.check_expr_inner(
                                value,
                                false,
                                "TransactorStateWrite value",
                                matches!(expected, Some(IrType::FixedVec { .. })),
                            );
                        }
                        Err(detail) => {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} transactor-state write `{instance}.{field}`: {detail}",
                                    self.fid.0, self.bid.0
                                ),
                            });
                            self.check_expr(value, false, "TransactorStateWrite value");
                        }
                    }
                }
                Stmt::TransactorStateRecordFieldWrite {
                    instance,
                    field,
                    path,
                    mid_indices,
                    index,
                    value,
                } => {
                    for (_, idx) in mid_indices {
                        self.check_expr(idx, false, "TransactorStateRecordFieldWrite mid index");
                    }
                    if let Some(idx) = index {
                        self.check_expr(idx, false, "TransactorStateRecordFieldWrite index");
                    }
                    let dst_shape = self.transactor_state_field_vec_shape(
                        instance,
                        field,
                        path,
                        mid_indices,
                        index.as_ref(),
                    );
                    let dst_ty =
                        self.transactor_state_record_field_type(instance, field, path, mid_indices);
                    self.check_whole_vec_write_value(
                        dst_shape,
                        dst_ty.clone(),
                        value,
                        "TransactorStateRecordFieldWrite value",
                    );
                    if path.is_empty() {
                        if let (Some(expected), Some(actual)) =
                            (dst_ty, self.aggregate_assignment_expr_type(value))
                        {
                            if !aggregate_assignment_compatible(&expected, &actual) {
                                self.errs.push(VerifyError::BadProgramRef {
                                    what: format!(
                                        "fn{} b{} writes fixed-vector state element `{instance}.{field}` of type {expected:?} from incompatible type {actual:?}",
                                        self.fid.0, self.bid.0
                                    ),
                                });
                            }
                        }
                    }
                }
                Stmt::TransactorStateQueuePush {
                    instance,
                    field,
                    value,
                } => {
                    // Target-state queue host state — the pushed value
                    // follows the no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "TransactorStateQueuePush value");
                    match resolve_transactor_state_queue_elem(self.prog, self.func, instance, field)
                    {
                        Ok(elem) => {
                            if let Some(actual) = expr_type(self.prog, self.func, value) {
                                if !queue_elem_accepts_type(&elem, &actual) {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} pushes {:?} into target-state queue \
                                             `{instance}.{field}` with element {:?}",
                                            self.fid.0, self.bid.0, actual, elem
                                        ),
                                    });
                                }
                            }
                        }
                        Err(what) => self.errs.push(VerifyError::BadProgramRef { what }),
                    }
                }
                Stmt::TransactorStateQueuePop {
                    instance,
                    field,
                    dest,
                } => {
                    self.check_local(*dest);
                    match resolve_transactor_state_queue_elem(self.prog, self.func, instance, field)
                    {
                        Ok(elem) => {
                            if let Some(local) = self.func.locals.get(dest.index()) {
                                if !queue_elem_fits_dest(&elem, &local.ty) {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} pops target-state queue \
                                             `{instance}.{field}` with element {:?} into local \
                                             %{} declared {:?}",
                                            self.fid.0, self.bid.0, elem, dest.0, local.ty
                                        ),
                                    });
                                }
                            }
                        }
                        Err(what) => self.errs.push(VerifyError::BadProgramRef { what }),
                    }
                }
                Stmt::Log { args, .. } => self.check_fmt_args(args),
                Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail } => {
                    self.check_truth_expr(cond, true, "AssertCheck cond");
                    self.check_fmt_args(on_fail);
                }
                Stmt::CovReport(inst) => self.check_covgroup(inst.covgroup),
                Stmt::PropertyCheck(p) => match self.prog.property_checks.get(p.index()).cloned() {
                    Some(schema) => self.check_property_schema(*p, &schema),
                    None => self.errs.push(VerifyError::BadConcurrentCheck {
                        func: self.fid,
                        block: self.bid,
                        detail: format!("references missing property check p{}", p.0),
                    }),
                },
                Stmt::CoverCheck(c) => match self.prog.cover_checks.get(c.index()).cloned() {
                    Some(schema) => self.check_cover_schema(*c, &schema),
                    None => self.errs.push(VerifyError::BadConcurrentCheck {
                        func: self.fid,
                        block: self.bid,
                        detail: format!("references missing cover check c{}", c.0),
                    }),
                },
                // The handler body is its own function (verified in its
                // own right); here the id must resolve to a one-parameter
                // `TestHook` whose parameter matches the event payload. Both
                // local and component channels are re-resolved so a later IR
                // pass cannot leave stale metadata for emission to trust.
                Stmt::EventSubscribe { event, handler } => {
                    let payload = match event {
                        crate::ir::EventChannelRef::Local(event) => {
                            self.check_local(*event);
                            match self.event_payload(*event) {
                                Some(payload) => {
                                    if let Err(detail) =
                                        verify_event_payload_ref(self.prog, &payload)
                                    {
                                        self.errs.push(VerifyError::BadConcurrentCheck {
                                            func: self.fid,
                                            block: self.bid,
                                            detail: format!(
                                                "EventSubscribe target {} {detail}",
                                                event.0
                                            ),
                                        });
                                    }
                                    Some(payload)
                                }
                                None => {
                                    self.errs.push(VerifyError::BadConcurrentCheck {
                                        func: self.fid,
                                        block: self.bid,
                                        detail: format!(
                                            "EventSubscribe target {} is not an event channel",
                                            event.0
                                        ),
                                    });
                                    None
                                }
                            }
                        }
                        crate::ir::EventChannelRef::Component {
                            base,
                            component,
                            event,
                            payload,
                        } => {
                            if let Err(detail) = verify_component_event_ref(
                                self.prog, self.func, base, *component, event, payload,
                            ) {
                                self.errs.push(VerifyError::BadConcurrentCheck {
                                    func: self.fid,
                                    block: self.bid,
                                    detail: format!("EventSubscribe component target: {detail}"),
                                });
                            }
                            Some(payload.clone())
                        }
                    };
                    match self.prog.functions.get(handler.index()) {
                        Some(f) if f.kind == FunctionKind::TestHook && f.params.len() == 1 => {
                            if f.locals.first().map(|local| &local.ty)
                                != f.params.first().map(|param| &param.ty)
                            {
                                self.errs.push(VerifyError::BadConcurrentCheck {
                                    func: self.fid,
                                    block: self.bid,
                                    detail: format!(
                                        "event subscriber fn{} parameter/local types disagree",
                                        f.id.0
                                    ),
                                });
                            }
                            if let Some(payload) = payload {
                                if !event_payload_handler_matches_type(&payload, &f.params[0].ty) {
                                    self.errs.push(VerifyError::BadConcurrentCheck {
                                        func: self.fid,
                                        block: self.bid,
                                        detail: format!(
                                            "event subscriber fn{} parameter {:?} does not match payload {:?}",
                                            f.id.0, f.params[0].ty, payload
                                        ),
                                    });
                                }
                            }
                            if f.owner != self.func.owner {
                                self.errs.push(VerifyError::BadConcurrentCheck {
                                    func: self.fid,
                                    block: self.bid,
                                    detail: format!(
                                        "event subscriber fn{} belongs to {:?}, expected {:?}",
                                        f.id.0, f.owner, self.func.owner
                                    ),
                                });
                            }
                        }
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
                Stmt::MethodHookSubscribe {
                    target,
                    handler,
                    captures,
                    ..
                } => {
                    for capture in captures {
                        self.check_local(*capture);
                    }
                    let expected = match verify_method_hook_target(self.prog, self.func, target) {
                        Ok(params) => Some(params),
                        Err(detail) => {
                            self.errs.push(VerifyError::BadConcurrentCheck {
                                func: self.fid,
                                block: self.bid,
                                detail: format!("MethodHookSubscribe target: {detail}"),
                            });
                            None
                        }
                    };
                    match self.prog.functions.get(handler.index()) {
                        Some(f) if f.kind == FunctionKind::TestHook => {
                            let actual: Vec<IrType> =
                                f.params.iter().map(|param| param.ty.clone()).collect();
                            let locals: Vec<IrType> = f
                                .locals
                                .iter()
                                .take(f.params.len())
                                .map(|local| local.ty.clone())
                                .collect();
                            if locals != actual {
                                self.errs.push(VerifyError::BadConcurrentCheck {
                                    func: self.fid,
                                    block: self.bid,
                                    detail: format!(
                                        "method-hook subscriber fn{} parameter/local types disagree",
                                        f.id.0
                                    ),
                                });
                            }
                            if let Some(expected) = &expected {
                                let method_count = expected.len();
                                if actual.get(..method_count) != Some(expected.as_slice())
                                    || actual.len() != method_count + captures.len()
                                {
                                    self.errs.push(VerifyError::BadConcurrentCheck {
                                        func: self.fid,
                                        block: self.bid,
                                        detail: format!(
                                            "method-hook subscriber fn{} parameters {:?} do not match target {:?}",
                                            f.id.0, actual, expected
                                        ),
                                    });
                                }
                                for (capture, param_ty) in
                                    captures.iter().zip(actual.iter().skip(method_count))
                                {
                                    if self
                                        .func
                                        .locals
                                        .get(capture.index())
                                        .is_some_and(|local| &local.ty != param_ty)
                                    {
                                        self.errs.push(VerifyError::BadConcurrentCheck {
                                            func: self.fid,
                                            block: self.bid,
                                            detail: format!(
                                                "method-hook capture %{} type does not match handler fn{} parameter {:?}",
                                                capture.0, f.id.0, param_ty
                                            ),
                                        });
                                    }
                                }
                            }
                            if f.owner != self.func.owner {
                                self.errs.push(VerifyError::BadConcurrentCheck {
                                    func: self.fid,
                                    block: self.bid,
                                    detail: format!(
                                        "method-hook subscriber fn{} belongs to {:?}, expected {:?}",
                                        f.id.0, f.owner, self.func.owner
                                    ),
                                });
                            }
                        }
                        Some(f) => self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!(
                                "method-hook subscriber fn{} is {:?}, not a TestHook",
                                f.id.0, f.kind
                            ),
                        }),
                        None => self.errs.push(VerifyError::BadConcurrentCheck {
                            func: self.fid,
                            block: self.bid,
                            detail: format!(
                                "method-hook subscription references missing fn{}",
                                handler.0
                            ),
                        }),
                    }
                }
                Stmt::EventEmit { event, args } => {
                    self.check_local(*event);
                    let payload = self.event_payload(*event);
                    if payload.is_none() {
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
                    if let ([arg], Some(payload)) = (args.as_slice(), payload) {
                        self.check_event_payload_value(payload, arg, "EventEmit arg");
                    } else {
                        for a in args {
                            self.check_expr(a, false, "EventEmit arg");
                        }
                    }
                }
                Stmt::CycleHandler(h) => match self.prog.cycle_handlers.get(h.index()) {
                    None => self.errs.push(VerifyError::BadConcurrentCheck {
                        func: self.fid,
                        block: self.bid,
                        detail: format!("references missing cycle handler h{}", h.0),
                    }),
                    Some(schema) => {
                        if let CycleHandlerKind::Trigger { trigger, .. } = &schema.kind {
                            self.check_truth_expr(trigger, true, "cycle-handler trigger");
                        }
                        match self.prog.functions.get(schema.function.index()) {
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
                            }
                    }
                },
                Stmt::TransactorCall { dest, call } => {
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                    self.check_transactor_call(*dest, call);
                }
                Stmt::TransactorSelfCall { dest, call } => {
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                    self.check_transactor_self_call(*dest, call);
                }
                Stmt::FailDiag { guard, args } => {
                    if let Some(g) = guard {
                        self.check_truth_expr(g, true, "FailDiag guard");
                    }
                    self.check_fmt_args(args);
                }
                Stmt::ScoreboardOp {
                    sb,
                    field,
                    op,
                    nested_path,
                } => {
                    self.check_scoreboard(*sb, field, nested_path.as_deref());
                    match op {
                        crate::ir::ScoreboardOp::QueuePush { queue, value } => {
                            self.check_scoreboard_queue(*sb, queue);
                            self.check_expr(value, false, "ScoreboardOp push value");
                            if let (Some(elem), Some(actual)) = (
                                self.scoreboard_queue_elem(*sb, queue),
                                self.aggregate_assignment_expr_type(value),
                            ) {
                                if self.contains_invalid_record_composition(value)
                                    || !queue_elem_accepts_type(&elem, &actual)
                                {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} pushes {:?} into scoreboard queue `{queue}` with element {:?}",
                                            self.fid.0, self.bid.0, actual, elem
                                        ),
                                    });
                                }
                            }
                        }
                        crate::ir::ScoreboardOp::QueuePop { queue, dest } => {
                            self.check_scoreboard_queue(*sb, queue);
                            self.check_local(*dest);
                            if let (Some(elem), Some(local)) = (
                                self.scoreboard_queue_elem(*sb, queue),
                                self.func.locals.get(dest.index()),
                            ) {
                                if !queue_elem_fits_dest(&elem, &local.ty) {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} pops scoreboard queue `{queue}` with element {:?} into local %{} declared {:?}",
                                            self.fid.0, self.bid.0, elem, dest.0, local.ty
                                        ),
                                    });
                                }
                            }
                        }
                        crate::ir::ScoreboardOp::ScalarWrite { scalar, value } => {
                            self.check_scoreboard_scalar(*sb, scalar);
                            self.check_expr(value, false, "ScoreboardOp scalar value");
                            if let (Some(expected), Some(actual)) = (
                                self.scoreboard_scalar_type(*sb, scalar),
                                self.aggregate_assignment_expr_type(value),
                            ) {
                                if self.contains_invalid_record_composition(value)
                                    || !aggregate_assignment_compatible(&expected, &actual)
                                {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} writes {:?} into scoreboard scalar \
                                             `{scalar}` declared {:?}",
                                            self.fid.0, self.bid.0, actual, expected
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
                Stmt::ComponentFieldWrite { base, field, value } => {
                    // Component host state — the value follows the
                    // no-inline-port rule like any Assign value.
                    //
                    // `component_field_vec_shape` already reports an
                    // unresolvable base, a missing field, and a field
                    // kind that cannot take this write (a scalar with a
                    // subfield, a `Vec` against a non-`Vec` value); it
                    // surfaces them through the `Err` arm of the call
                    // below.
                    let dst_shape = self.component_field_vec_shape(base, field);
                    // `dst_ty` feeds #661's whole-collection shape
                    // (a scalar record list is a collection the raw vec
                    // shape cannot see). Computed before the call so
                    // the scalar-type check below can still ask whether
                    // this destination is a plain, non-collection slot.
                    let dst_ty = self.component_field_type(base, field);
                    let non_vec_dest = matches!(dst_shape, Ok(None));
                    self.check_whole_vec_write_value(
                        dst_shape,
                        dst_ty.clone(),
                        value,
                        "ComponentFieldWrite value",
                    );
                    // What none of that covered: the destination's
                    // TYPE. A `uint<129>` written into a `uint<65>`
                    // field passed the verifier untouched, and both
                    // sides are `_harc_u128` in C++, so nothing
                    // downstream objected either. Lowering rejects it
                    // now (harc#642/#656); this is the IR-level
                    // backstop, so a future lowering path that skips
                    // that check cannot emit the truncation silently.
                    //
                    // Gated on a NON-VEC destination:
                    // `component_field_type` answers a fixed vector
                    // with its ELEMENT type, and a whole-`Vec` copy is
                    // not an element assignment. The collection shapes
                    // are the call above's business.
                    //
                    // Deliberately bounded to a destination past 64
                    // bits, even though the LOWERING check now judges
                    // <=64-bit fields too (harc#658). A verifier is a
                    // backstop, not a second opinion, and judging <=64
                    // here faithfully would require re-deriving whether
                    // the RHS width is DECLARED or MANUFACTURED from a
                    // widthless leaf (`seen + <const>`) — the exact
                    // distinction `check_owner_scalar_field_write`'s
                    // `rhs_width_manufactured` draws in lowering.
                    // Duplicating that here, on the verifier's separate
                    // expr typers, is how the two drift; getting it
                    // wrong turns a program lowering accepts into
                    // `internal error: failed verification after
                    // lowering`. So the backstop stays at >64 — the
                    // silent same-carrier truncation class (#642/#656)
                    // it was built for — and lowering is authoritative
                    // for the <=64 rule.
                    let wide_dest = matches!(
                        dst_ty,
                        Some(IrType::UInt(Some(w)) | IrType::SInt(Some(w))) if w > 64
                    );
                    if non_vec_dest && wide_dest {
                        if let (Some(expected), Some(actual)) =
                            (dst_ty, self.aggregate_assignment_expr_type(value))
                        {
                            if self.contains_invalid_record_composition(value)
                                || !aggregate_assignment_compatible(&expected, &actual)
                            {
                                self.errs.push(VerifyError::BadProgramRef {
                                    what: format!(
                                        "fn{} b{} writes {:?} into component field \
                                         `{field}` declared {:?}",
                                        self.fid.0, self.bid.0, actual, expected
                                    ),
                                });
                            }
                        }
                    }
                }
                Stmt::ComponentVecElementWrite {
                    base,
                    field,
                    index_pos,
                    index,
                    inner_index,
                    value,
                } => {
                    self.check_expr(index, false, "ComponentVecElementWrite index");
                    match self.component_indexed_field_type(base, field, *index_pos) {
                        Ok((expected, len)) => {
                            if matches!(index, Expr::Literal { value, .. } if *value as usize >= len)
                            {
                                self.report_bad_component_field(format!(
                                    "indexed component field `{field}` is out of bounds for length {len}"
                                ));
                            }
                            // A nested write `v[i][j] = x`: `expected`
                            // (the outer element) must be a `FixedVec`;
                            // the value is checked against its scalar
                            // inner element.
                            if let Some(inner) = inner_index {
                                self.check_expr(
                                    inner,
                                    false,
                                    "ComponentVecElementWrite inner index",
                                );
                                let IrType::FixedVec {
                                    elem: inner_elem,
                                    len: inner_len,
                                } = &expected
                                else {
                                    self.report_bad_component_field(format!(
                                        "nested index write on component field `{field}` whose element is not a fixed vector"
                                    ));
                                    continue;
                                };
                                if matches!(inner, Expr::Literal { value, .. } if *value as usize >= *inner_len)
                                {
                                    self.report_bad_component_field(format!(
                                        "nested index into component field `{field}` is out of bounds for length {inner_len}"
                                    ));
                                }
                                self.check_expr(value, false, "ComponentVecElementWrite value");
                                let actual = self.aggregate_assignment_expr_type(value);
                                if let Some(actual) = actual {
                                    let compatible = match (&**inner_elem, &actual) {
                                        (IrType::Record(expected), IrType::Record(actual)) => {
                                            expected == actual
                                        }
                                        (IrType::Record(_), _) | (_, IrType::Record(_)) => false,
                                        _ => true,
                                    };
                                    if !compatible {
                                        self.report_bad_component_field(format!(
                                            "nested component field `{field}` element of type {:?} is written from incompatible type {actual:?}",
                                            inner_elem
                                        ));
                                    }
                                }
                                continue;
                            }
                            if matches!(expected, IrType::Seq(_)) {
                                self.check_whole_vec_write_value(
                                    Ok(None),
                                    Some(expected),
                                    value,
                                    "ComponentVecElementWrite value",
                                );
                                continue;
                            }
                            self.check_expr(value, false, "ComponentVecElementWrite value");
                            let actual = match value {
                                Expr::Literal {
                                    ty: IrType::Unknown,
                                    ..
                                } => Some(IrType::Unknown),
                                _ => self.aggregate_assignment_expr_type(value),
                            };
                            if let Some(actual) = actual {
                                let compatible = match (&expected, &actual) {
                                    (IrType::Record(expected), IrType::Record(actual)) => {
                                        expected == actual
                                    }
                                    (IrType::Record(_), _) | (_, IrType::Record(_)) => false,
                                    // Scalar element coercions are already an established
                                    // lowering contract (including positive literals into
                                    // signed slots). This verifier guard is the aggregate
                                    // boundary: record-vs-scalar and record identity.
                                    _ => true,
                                };
                                if !compatible {
                                    self.report_bad_component_field(format!(
                                        "indexed component field `{field}` of type {expected:?} is written from incompatible type {actual:?}"
                                    ));
                                }
                            }
                        }
                        Err(detail) => self.report_bad_component_field(detail),
                    }
                }
                Stmt::TbFieldVecElementWrite {
                    field,
                    index,
                    inner_index,
                    value,
                } => {
                    self.check_tb_field(field);
                    let vec_ty = self.tb_scalar_field_ty(field);
                    self.check_fixed_vec_element_write(
                        vec_ty,
                        index,
                        inner_index.as_ref(),
                        value,
                        "TbFieldVecElementWrite",
                    );
                }
                Stmt::ComponentEmit {
                    base,
                    subpath,
                    event,
                    args,
                } => {
                    let payload = match self.component_emit_payload(base, subpath, event) {
                        Ok(payload) => Some(payload),
                        Err(detail) => {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} ComponentEmit target `{event}`: {detail}",
                                    self.fid.0, self.bid.0
                                ),
                            });
                            None
                        }
                    };
                    if args.len() != 1 {
                        self.errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "fn{} b{} ComponentEmit carries {} argument(s)",
                                self.fid.0,
                                self.bid.0,
                                args.len()
                            ),
                        });
                    }
                    if let ([arg], Some(payload)) = (args.as_slice(), payload) {
                        self.check_event_payload_value(payload, arg, "ComponentEmit arg");
                    } else {
                        for a in args {
                            self.check_expr(a, false, "ComponentEmit arg");
                        }
                    }
                }
                Stmt::ComponentCall {
                    component,
                    method,
                    args,
                    dest,
                    ..
                } => {
                    for a in args {
                        self.check_expr(a, false, "ComponentCall arg");
                    }
                    if let Some(schema) = self
                        .prog
                        .components
                        .get(component.index())
                        .and_then(|component| component.method(method))
                    {
                        if schema.param_tys.len() != args.len() {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} ComponentCall `{method}` argument count mismatch",
                                    self.fid.0, self.bid.0
                                ),
                            });
                        }
                        for (index, (arg, expected)) in
                            args.iter().zip(&schema.param_tys).enumerate()
                        {
                            let actual = self
                                .aggregate_assignment_expr_type(arg)
                                .unwrap_or(IrType::Unknown);
                            if (matches!(
                                expected,
                                IrType::FixedVec { .. } | IrType::Seq(_) | IrType::RecordSeq(_)
                            ) || matches!(
                                &actual,
                                IrType::FixedVec { .. } | IrType::Seq(_) | IrType::RecordSeq(_)
                            )) && *expected != actual
                            {
                                self.errs.push(VerifyError::BadProgramRef {
                                    what: format!(
                                        "fn{} b{} ComponentCall `{method}` argument {} expects {:?}, got {:?}",
                                        self.fid.0,
                                        self.bid.0,
                                        index + 1,
                                        expected,
                                        actual
                                    ),
                                });
                            }
                        }
                        if let Some(d) = dest {
                            let actual = self.func.locals.get(d.index()).map(|local| &local.ty);
                            match (&schema.ret_ty, actual) {
                                (Some(expected), Some(IrType::Unknown))
                                    if !matches!(expected, IrType::FixedVec { .. }) => {}
                                (Some(expected), Some(actual))
                                    if !matches!(actual, IrType::Unknown)
                                        && assign_compatible(actual, expected) => {}
                                (Some(expected), Some(actual)) => {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} ComponentCall `{method}` returns {expected:?}, but destination is {actual:?}",
                                            self.fid.0, self.bid.0
                                        ),
                                    });
                                }
                                (None, _) => self.errs.push(VerifyError::BadProgramRef {
                                    what: format!(
                                        "fn{} b{} void ComponentCall `{method}` captured into a destination",
                                        self.fid.0, self.bid.0
                                    ),
                                }),
                                _ => {}
                            }
                        }
                    }
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                }
                Stmt::SeqPush { seq, value } => {
                    self.check_local(*seq);
                    let expected = match self.func.locals.get(seq.index()).map(|l| &l.ty) {
                        Some(IrType::RecordSeq(record)) => Some(IrType::Record(*record)),
                        Some(IrType::Seq(elem)) => Some((**elem).clone()),
                        Some(other) => {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} SeqPush accumulator l{} has non-sequence type {:?}",
                                    self.fid.0, self.bid.0, seq.0, other
                                ),
                            });
                            None
                        }
                        None => None,
                    };
                    if let Some(expected) = expected {
                        // A fixed-vector yield is one of the explicit
                        // whole-value copy landings. Preserve the collection
                        // shape instead of classifying its leaf scalar, and
                        // authorize that exact expression in the structural
                        // whole-vector checker.
                        let fixed = matches!(expected, IrType::FixedVec { .. });
                        if fixed {
                            self.check_expr_inner(value, false, "SeqPush value", true);
                        } else {
                            self.check_expr(value, false, "SeqPush value");
                        }
                        let actual = if fixed {
                            self.expr_whole_vec_type(value).ok().flatten()
                        } else {
                            self.aggregate_assignment_expr_type(value)
                        };
                        let compatible = match &expected {
                            IrType::Record(_) => {
                                actual.as_ref() == Some(&expected)
                                    && !self.contains_invalid_record_composition(value)
                            }
                            _ => actual
                                .as_ref()
                                .is_some_and(|actual| assign_compatible(&expected, actual)),
                        };
                        if !compatible {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} SeqPush into l{} expects {:?}, got {:?}",
                                    self.fid.0, self.bid.0, seq.0, expected, actual
                                ),
                            });
                        }
                    }
                }
                Stmt::ComponentQueuePush { base, queue, value } => {
                    // Component-queue host state — the pushed value follows
                    // the no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "ComponentQueuePush value");
                    match resolve_component_queue_elem(self.prog, self.func, base, queue) {
                        Ok(elem) => {
                            if let Some(actual) = expr_type(self.prog, self.func, value) {
                                if !queue_elem_accepts_type(&elem, &actual) {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} pushes {:?} into component queue `{queue}` with element {:?}",
                                            self.fid.0, self.bid.0, actual, elem
                                        ),
                                    });
                                }
                            }
                        }
                        Err(detail) => self.errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "fn{} b{} has invalid component queue push: {detail}",
                                self.fid.0, self.bid.0
                            ),
                        }),
                    }
                }
                Stmt::ComponentQueuePop { base, queue, dest } => {
                    self.check_local(*dest);
                    match resolve_component_queue_elem(self.prog, self.func, base, queue) {
                        Ok(elem) => {
                            if let Some(local) = self.func.locals.get(dest.index()) {
                                if !queue_elem_fits_dest(&elem, &local.ty) {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} pops component queue `{queue}` with element {:?} into local %{} declared {:?}",
                                            self.fid.0, self.bid.0, elem, dest.0, local.ty
                                        ),
                                    });
                                }
                            }
                        }
                        Err(detail) => self.errs.push(VerifyError::BadProgramRef {
                            what: format!(
                                "fn{} b{} has invalid component queue pop: {detail}",
                                self.fid.0, self.bid.0
                            ),
                        }),
                    }
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
                    self.check_bus_call_edge(desc.dest, &desc.bus_field, &desc.method, &desc.args);
                }
                Stmt::TlmJoinAll(pending) => {
                    for p in pending {
                        if let Some(d) = p.dest {
                            self.check_local(d);
                        }
                        self.check_bus_call_edge(p.dest, &p.bus_field, &p.method, &p.args);
                    }
                }
            }
        }
        match &b.terminator {
            Terminator::Branch(c, _, _) => self.check_truth_expr(c, false, "Branch cond"),
            Terminator::WaitCycles(e, _, _) => self.check_expr(e, false, "WaitCycles count"),
            Terminator::WaitCyclesSync(e, _) => self.check_expr(e, false, "WaitCycles count"),
            Terminator::WaitTimePs(..) => {}
            Terminator::WaitUntil { preds, .. } => {
                for p in preds {
                    self.check_truth_expr(&p.expr, true, "WaitUntil pred");
                }
            }
            Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                for p in preds {
                    self.check_truth_expr(&p.expr, true, "WaitUntilTimeout pred");
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
                } else if let Some(local) = self.func.locals.get(target.index()) {
                    if let IrType::Record(record) = local.ty {
                        let site = &self.prog.constraint_sites[constraints.index()];
                        match self.prog.records.get(record.index()) {
                            Some(schema) if schema.name == site.record => {}
                            Some(schema) => self.errs.push(VerifyError::DanglingConstraintRef {
                                func: self.fid,
                                block: self.bid,
                                detail: format!(
                                    "c{} is for record `{}` but target local `{}` has record `{}`",
                                    constraints.0, site.record, local.name, schema.name
                                ),
                            }),
                            None => self.errs.push(VerifyError::DanglingConstraintRef {
                                func: self.fid,
                                block: self.bid,
                                detail: format!(
                                    "target local `{}` references missing record r{}",
                                    local.name, record.0
                                ),
                            }),
                        }
                    }
                }
            }
            Terminator::Fatal(args) => self.check_fmt_args(args),
            Terminator::TbLifecycleCall { function, .. } => {
                // The re-inline target must resolve to a lifecycle function
                // (#619 M4a). `succ` is range-checked with every other
                // successor by the invariant-6 pass above.
                match self.prog.functions.get(function.index()) {
                    Some(f) if matches!(f.kind, FunctionKind::TestbenchLifecycle { .. }) => {}
                    Some(f) => self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "TbLifecycleCall fn{} targets `{}`, not a TestbenchLifecycle function",
                            function.0, f.name
                        ),
                    }),
                    None => self.errs.push(VerifyError::BadProgramRef {
                        what: format!("TbLifecycleCall references missing fn{}", function.0),
                    }),
                }
            }
            Terminator::Jump(_) | Terminator::Return => {}
        }
    }

    fn check_fmt_args(&mut self, args: &FmtArgs) {
        for a in &args.args {
            self.check_expr(&a.expr, true, "format arg");
            if self.contains_invalid_record_composition(&a.expr) {
                self.errs.push(VerifyError::BadFormatArg {
                    func: self.fid,
                    block: self.bid,
                    actual: IrType::Unknown,
                });
            } else if let Some(actual) = self.aggregate_assignment_expr_type(&a.expr) {
                if !matches!(
                    actual,
                    IrType::UInt(_)
                        | IrType::SInt(_)
                        | IrType::Bool
                        | IrType::Unknown
                        | IrType::PortSnapshot
                ) {
                    self.errs.push(VerifyError::BadFormatArg {
                        func: self.fid,
                        block: self.bid,
                        actual,
                    });
                }
            }
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

    /// Declared type of a scalar-kind testbench field. A `Vec<T, N>` host
    /// field is stored as a `Scalar` state field whose `ty` is a
    /// `FixedVec`, so this is how the fixed-vector element checks recover
    /// the receiver's shape. `None` when no scalar field by that name.
    fn tb_scalar_field_ty(&self, field: &str) -> Option<IrType> {
        self.func
            .owner
            .and_then(|tb| self.prog.testbenches.get(tb.index()))
            .and_then(|tb| {
                tb.state_fields.iter().find_map(|state| match state {
                    TbStateFieldSchema::Scalar(scalar) if scalar.name == field => {
                        Some(scalar.ty.clone())
                    }
                    _ => None,
                })
            })
    }

    /// Bounds- and structure-check a fixed-vector element write whose
    /// receiver is a plain `IrType::FixedVec` — a testbench host field
    /// (`_tb.mem[i] = x`). The testbench-field decoder gate admits only
    /// scalar (or nested-`FixedVec`) elements, so there is no
    /// component-field / dotted-path indirection and no record elements to
    /// reject; this mirrors the scalar path of `ComponentVecElementWrite`.
    /// The value's scalar coercion is an established lowering contract, so
    /// (as on the component path) only the index bounds are enforced here
    /// beyond the structural `check_expr` walks.
    fn check_fixed_vec_element_write(
        &mut self,
        vec_ty: Option<IrType>,
        index: &Expr,
        inner_index: Option<&Expr>,
        value: &Expr,
        what: &'static str,
    ) {
        self.check_expr(value, false, what);
        self.check_fixed_vec_element_indices(vec_ty, index, inner_index, false, what, what);
    }

    /// Read counterpart of `check_fixed_vec_element_write` — same bounds and
    /// structural walks, but the index sub-exprs inherit the read's
    /// `ports_ok`/`context` (a read may legally appear in a port-bearing
    /// position where a write cannot).
    fn check_fixed_vec_element_read(
        &mut self,
        vec_ty: Option<IrType>,
        index: &Expr,
        inner_index: Option<&Expr>,
        ports_ok: bool,
        context: &'static str,
        what: &'static str,
    ) {
        self.check_fixed_vec_element_indices(vec_ty, index, inner_index, ports_ok, context, what);
    }

    /// Shared index-walk + literal-bounds check for the fixed-vector
    /// element read/write nodes whose receiver is a plain `IrType::FixedVec`.
    fn check_fixed_vec_element_indices(
        &mut self,
        vec_ty: Option<IrType>,
        index: &Expr,
        inner_index: Option<&Expr>,
        ports_ok: bool,
        context: &'static str,
        what: &'static str,
    ) {
        self.check_expr(index, ports_ok, context);
        if let Some(inner) = inner_index {
            self.check_expr(inner, ports_ok, context);
        }
        let Some(IrType::FixedVec { elem, len }) = vec_ty else {
            return;
        };
        if matches!(index, Expr::Literal { value, .. } if *value as usize >= len) {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} {what}: index out of bounds for fixed vector of length {len}",
                    self.fid.0, self.bid.0
                ),
            });
        }
        if let Some(inner) = inner_index {
            let IrType::FixedVec { len: inner_len, .. } = elem.as_ref() else {
                self.errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} b{} {what}: nested index whose element is not a fixed vector",
                        self.fid.0, self.bid.0
                    ),
                });
                return;
            };
            if matches!(inner, Expr::Literal { value, .. } if *value as usize >= *inner_len) {
                self.errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} b{} {what}: nested index out of bounds for fixed vector of length {inner_len}",
                        self.fid.0, self.bid.0
                    ),
                });
            }
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
    fn check_scoreboard(
        &mut self,
        sb: crate::ir::ScoreboardId,
        field: &str,
        nested_path: Option<&[String]>,
    ) {
        if sb.index() >= self.prog.scoreboards.len() {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard id sb{} does not resolve", sb.0),
            });
            return;
        }
        if let Some(path) = nested_path {
            if let Err(detail) = self.check_nested_scoreboard_path(sb, field, path) {
                self.errs.push(VerifyError::BadScoreboard {
                    func: self.fid,
                    block: self.bid,
                    detail,
                });
            }
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

    /// Replay lowering's env/self component walk for a nested data-only
    /// scoreboard. Codegen renders this path verbatim, so every segment and
    /// the terminal scoreboard identity must remain tied to the schema after
    /// any IR rewrite.
    fn check_nested_scoreboard_path(
        &self,
        sb: crate::ir::ScoreboardId,
        field: &str,
        path: &[String],
    ) -> Result<(), String> {
        let (root, tail) = path
            .split_first()
            .ok_or_else(|| "nested scoreboard has an empty path".to_string())?;
        if tail.is_empty() || path.last().is_none_or(|leaf| leaf != field) {
            return Err(format!(
                "nested scoreboard path `{}` does not terminate at field `{field}`",
                path.join(".")
            ));
        }
        // Match lowering's namespace order: a real testbench component field
        // named `self` wins over the synthetic component-method root.
        let owner_component = self
            .func
            .owner
            .and_then(|owner| self.prog.testbenches.get(owner.index()))
            .and_then(|owner| {
                owner
                    .component_fields
                    .iter()
                    .find(|binding| binding.field == *root)
                    .map(|binding| binding.component)
            });
        let mut component = if let Some(component) = owner_component {
            component
        } else if root == "self" {
            match self.func.kind {
                FunctionKind::ComponentMethod { component } => component,
                _ => {
                    return Err(
                        "self-rooted nested scoreboard outside a component method".to_string()
                    )
                }
            }
        } else {
            return Err(format!("root `{root}` is not a testbench component field"));
        };
        for (position, segment) in tail.iter().enumerate() {
            let schema = self
                .prog
                .components
                .get(component.index())
                .ok_or_else(|| format!("component c{} does not resolve", component.0))?;
            let terminal = position + 1 == tail.len();
            match schema.field(segment).map(|member| &member.kind) {
                Some(ComponentFieldKind::Sub {
                    component: next, ..
                }) if !terminal => component = *next,
                Some(ComponentFieldKind::ScoreboardSub { scoreboard }) if terminal => {
                    if *scoreboard == sb {
                        return Ok(());
                    }
                    return Err(format!(
                        "nested scoreboard path `{}` resolves to sb{}, not sb{}",
                        path.join("."),
                        scoreboard.0,
                        sb.0
                    ));
                }
                _ => {
                    return Err(format!(
                        "nested scoreboard path `{}` has invalid segment `{segment}`",
                        path.join(".")
                    ))
                }
            }
        }
        Err(format!(
            "nested scoreboard path `{}` has no scoreboard leaf",
            path.join(".")
        ))
    }

    fn check_scoreboard_scalar(&mut self, sb: crate::ir::ScoreboardId, scalar: &str) {
        let ok = self
            .prog
            .scoreboards
            .get(sb.index())
            .and_then(|s| s.field(scalar))
            .is_some_and(|f| {
                matches!(
                    f.kind,
                    crate::ir::ScoreboardFieldKind::Scalar { .. }
                        | crate::ir::ScoreboardFieldKind::Record { .. }
                )
            });
        if !ok {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!(
                    "scoreboard sb{} has no scalar/record field `{scalar}`",
                    sb.0
                ),
            });
        }
    }

    fn scoreboard_scalar_type(&self, sb: crate::ir::ScoreboardId, scalar: &str) -> Option<IrType> {
        self.prog
            .scoreboards
            .get(sb.index())
            .and_then(|s| s.field(scalar))
            .and_then(|f| match &f.kind {
                crate::ir::ScoreboardFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                crate::ir::ScoreboardFieldKind::Record { record } => Some(IrType::Record(*record)),
                _ => None,
            })
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

    fn check_scoreboard_container(&mut self, sb: crate::ir::ScoreboardId, field: &str) {
        let ok = self
            .prog
            .scoreboards
            .get(sb.index())
            .and_then(|s| s.field(field))
            .is_some_and(|f| {
                matches!(
                    f.kind,
                    crate::ir::ScoreboardFieldKind::Queue { .. }
                        | crate::ir::ScoreboardFieldKind::List { .. }
                )
            });
        if !ok {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard sb{} has no queue or list field `{field}`", sb.0),
            });
        }
    }

    fn scoreboard_queue_elem(&self, sb: crate::ir::ScoreboardId, queue: &str) -> Option<QueueElem> {
        self.prog
            .scoreboards
            .get(sb.index())
            .and_then(|schema| schema.field(queue))
            .and_then(|field| match &field.kind {
                crate::ir::ScoreboardFieldKind::Queue { elem } => Some(elem.clone()),
                crate::ir::ScoreboardFieldKind::Scalar { .. }
                | crate::ir::ScoreboardFieldKind::Record { .. }
                | crate::ir::ScoreboardFieldKind::List { .. } => None,
            })
    }

    /// A direct transactor heartbeat expression carries both the source
    /// field name and schema id. Verify both halves against the owning
    /// testbench so an IR-mutating pass cannot invent a field, mismatch its
    /// schema, or leave codegen indexing a dangling transactor id.
    fn check_transactor_idle(&mut self, field: &str, transactor: TransactorId, storage: &str) {
        let Some(owner) = self.func.owner else {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} transactor heartbeat `{field}` has no owning testbench",
                    self.fid.0, self.bid.0
                ),
            });
            return;
        };
        let Some(tb) = self.prog.testbenches.get(owner.index()) else {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} transactor heartbeat owner tb{} does not resolve",
                    self.fid.0, self.bid.0, owner.0
                ),
            });
            return;
        };
        let bound = tb
            .transactor_fields
            .iter()
            .find_map(|(name, id)| (name == field).then_some(*id))
            .or_else(|| {
                tb.target_tlm_actors
                    .iter()
                    .find(|actor| actor.instance == field)
                    .map(|actor| actor.transactor)
            });
        match bound {
            None => self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} transactor heartbeat field `{field}` is not bound on testbench `{}`",
                    self.fid.0, self.bid.0, tb.name
                ),
            }),
            Some(actual) if actual != transactor => {
                self.errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} b{} transactor heartbeat field `{field}` binds x{} but expression names x{}",
                        self.fid.0, self.bid.0, actual.0, transactor.0
                    ),
                })
            }
            Some(_) => {}
        }
        if self.prog.transactors.get(transactor.index()).is_none() {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} transactor heartbeat references missing x{}",
                    self.fid.0, self.bid.0, transactor.0
                ),
            });
        }
        let direct_storage = tb
            .unbound_state_actors
            .iter()
            .find(|actor| actor.field == field && actor.transactor == transactor)
            .map(|actor| actor.storage.as_str());
        let target_storage = tb
            .target_tlm_actors
            .iter()
            .find(|actor| actor.instance == field && actor.transactor == transactor)
            .map(|actor| actor.instance.as_str());
        match direct_storage.or(target_storage) {
            Some(expected) if expected == storage => {}
            Some(expected) => self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} transactor heartbeat field `{field}` names stamp storage `{storage}` but schema requires `{expected}`",
                    self.fid.0, self.bid.0
                ),
            }),
            None => {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} transactor heartbeat field `{field}` has no matching stamp storage",
                    self.fid.0, self.bid.0
                ),
            });
            }
        }
    }
    /// `local` must be record-typed and its schema must declare `field`.
    /// `mid_positions` lists the segments (positions in `[field] ++ path`)
    /// that carry path selections. Repeated leaf positions consume nested
    /// fixed-vector layers in order.
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
        // carry nested fixed-vector selections. Fails on an unknown
        // field, a non-record intermediate, or an index/`Vec` mismatch.
        let ok = (|| -> Option<()> {
            if mid_positions.windows(2).any(|pair| pair[0] > pair[1]) {
                return None;
            }
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
                let index_count = mid_positions.iter().filter(|p| **p == i).count();
                let indexed = index_count != 0;
                if i == last {
                    let mut len = fld.vec_len;
                    let mut ty = fld.ty.clone();
                    for _ in 0..index_count {
                        len?;
                        match ty {
                            IrType::FixedVec {
                                elem,
                                len: inner_len,
                            } => {
                                len = Some(inner_len);
                                ty = *elem;
                            }
                            _ => len = None,
                        }
                    }
                    return Some(());
                }
                match fld.ty {
                    IrType::Record(r)
                        if (fld.vec_len.is_none() && !indexed)
                            || (fld.vec_len.is_some() && index_count == 1) =>
                    {
                        rid = r
                    }
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
    fn check_transactor_call(&mut self, dest: Option<LocalId>, call: &Expr) {
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
        let Some(resolved) = schema.method(method) else {
            self.errs.push(bad(format!(
                "transactor `{}` has no method `{method}`",
                schema.name
            )));
            return;
        };
        for (i, arg) in args.iter().enumerate() {
            let expected = resolved.param_tys.get(i);
            self.check_expr_inner(
                arg,
                false,
                "TransactorCall arg",
                matches!(expected, Some(IrType::FixedVec { .. })),
            );
            if let Some(expected) = expected {
                let actual = if matches!(expected, IrType::FixedVec { .. }) {
                    self.expr_whole_vec_type(arg).ok().flatten()
                } else {
                    expr_type(self.prog, self.func, arg)
                };
                match actual {
                    Some(actual) if assign_compatible(expected, &actual) => {}
                    Some(actual) => self.errs.push(bad(format!(
                        "transactor method `{}.{method}` parameter {} expects {expected:?}, got {actual:?}",
                        schema.name,
                        i + 1
                    ))),
                    None if matches!(expected, IrType::FixedVec { .. }) => {
                        self.errs.push(bad(format!(
                            "transactor method `{}.{method}` parameter {} expects {expected:?}, got a non-fixed-vector value",
                            schema.name,
                            i + 1
                        )));
                    }
                    None => {}
                }
            }
        }
        if let Some(dest) = dest {
            let actual = self.func.locals.get(dest.index()).map(|local| &local.ty);
            match (&resolved.ret_ty, actual) {
                (Some(IrType::Unknown), Some(IrType::Unknown)) => {}
                (Some(expected), Some(actual))
                    if !matches!(actual, IrType::Unknown)
                        && assign_compatible(actual, expected) => {}
                (Some(expected), Some(actual)) => self.errs.push(bad(format!(
                    "transactor method `{}.{method}` returns {expected:?}, but destination is {actual:?}",
                    schema.name
                ))),
                (None, _) => self.errs.push(bad(format!(
                    "void transactor method `{}.{method}` captured into a destination",
                    schema.name
                ))),
                _ => {}
            }
        }
    }

    /// The payload of an event-channel local, or `None` when the local
    /// does not resolve or is not event-typed.
    fn event_payload(&self, l: LocalId) -> Option<crate::ir::EventPayload> {
        match self.func.locals.get(l.index()).map(|t| &t.ty) {
            Some(IrType::Event(p)) => Some(p.clone()),
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
        if self.contains_invalid_record_composition(e) {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} {context} contains an invalid record composition",
                    self.fid.0, self.bid.0
                ),
            });
        }
        self.check_expr_inner(e, ports_ok, context, false);
    }

    fn check_expr_inner(
        &mut self,
        e: &Expr,
        ports_ok: bool,
        context: &'static str,
        whole_vec_ok: bool,
    ) {
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
                if !self.temporal_slots_ok {
                    self.errs.push(VerifyError::BadConcurrentCheck {
                        func: self.fid,
                        block: self.bid,
                        detail: format!(
                            "temporal slot {slot} referenced in {context}, outside a \
                             concurrent property/cover body"
                        ),
                    });
                }
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
            Expr::TransactorState { instance, field } => {
                if let Err(detail) = self.transactor_state_field(instance, field) {
                    self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} transactor-state read `{instance}.{field}`: {detail}",
                            self.fid.0, self.bid.0
                        ),
                    });
                }
            }
            Expr::TransactorStateRecordField {
                instance,
                field,
                path,
                mid_indices,
                index,
            } => {
                for (_, idx) in mid_indices {
                    self.check_expr(idx, ports_ok, context);
                }
                if let Some(idx) = index {
                    self.check_expr(idx, ports_ok, context);
                }
                let vec_shape = self.transactor_state_field_vec_shape(
                    instance,
                    field,
                    path,
                    mid_indices,
                    index.as_deref(),
                );
                let ty =
                    self.transactor_state_record_field_type(instance, field, path, mid_indices);
                match self.whole_collection_shape(vec_shape, ty) {
                Ok(Some(shape)) if !whole_vec_ok => self.report_bad_whole_vec_use(format!(
                    "transactor-state collection `{instance}.{field}.{}` with shape {shape:?} appears outside matching equality/copy",
                    path.join(".")
                )),
                Ok(_) => {}
                Err(detail) => self.report_bad_whole_vec_use(detail),
                }
            }
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
            Expr::Binary(op, a, b) => {
                let lhs_shape = self.expr_whole_collection_shape(a);
                let rhs_shape = self.expr_whole_collection_shape(b);
                match (lhs_shape, rhs_shape) {
                    (Ok(Some(lhs)), Ok(Some(rhs)))
                        if matches!(op, BinOp::Eq | BinOp::Ne) && lhs == rhs =>
                    {
                        self.check_expr_inner(a, ports_ok, context, true);
                        self.check_expr_inner(b, ports_ok, context, true);
                    }
                    (Ok(None), Ok(None)) => {
                        self.check_expr(a, ports_ok, context);
                        self.check_expr(b, ports_ok, context);
                    }
                    (Ok(lhs), Ok(rhs)) => {
                        self.report_bad_whole_vec_use(format!(
                            "binary operator {op:?} has incompatible whole-collection operands {lhs:?} and {rhs:?}"
                        ));
                        self.check_expr_inner(a, ports_ok, context, lhs.is_some());
                        self.check_expr_inner(b, ports_ok, context, rhs.is_some());
                    }
                    (Err(detail), _) | (_, Err(detail)) => {
                        self.report_bad_whole_vec_use(detail);
                        self.check_expr(a, ports_ok, context);
                        self.check_expr(b, ports_ok, context);
                    }
                }
            }
            Expr::Unary(_, a) => self.check_expr(a, ports_ok, context),
            Expr::BitSlice { target, .. } => self.check_expr(target, ports_ok, context),
            Expr::BitSliceDyn { target, hi, lo } => {
                self.check_expr(target, ports_ok, context);
                self.check_expr(hi, ports_ok, context);
                self.check_expr(lo, ports_ok, context);
            }
            Expr::PortSnapshotLane {
                snapshot,
                port,
                index,
            } => {
                if context != "Assign value" {
                    self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} sampled-port lane is only valid beneath an Assign value",
                            self.fid.0, self.bid.0
                        ),
                    });
                }
                self.check_local(*snapshot);
                if !matches!(
                    self.func.locals.get(snapshot.index()).map(|l| &l.ty),
                    Some(IrType::PortSnapshot)
                ) {
                    self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} sampled-port lane references a non-snapshot local",
                            self.fid.0, self.bid.0
                        ),
                    });
                }
                if port.lane.is_some() {
                    self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} sampled-port lane carries an already-indexed port",
                            self.fid.0, self.bid.0
                        ),
                    });
                }
                let mut definition_count = 0usize;
                let mut defined_port = None;
                for stmt in self.func.blocks.iter().flat_map(|block| &block.stmts) {
                    match stmt {
                        Stmt::DutRead(dest, defined) if dest == snapshot => {
                            definition_count += 1;
                            defined_port = Some(defined);
                        }
                        Stmt::Assign(dest, _) if dest == snapshot => definition_count += 1,
                        _ => {}
                    }
                }
                let same_port = |defined: &crate::ir::PortRef| {
                    defined.testbench_field == port.testbench_field
                        && defined.port_path == port.port_path
                        && defined.aggregate_path == port.aggregate_path
                        && defined.direction == port.direction
                        && defined.width == port.width
                        && defined.access == port.access
                        && defined.lane.is_none()
                };
                if definition_count != 1 || !defined_port.is_some_and(same_port) {
                    self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} sampled-port lane metadata does not match its unique defining DutRead",
                            self.fid.0, self.bid.0
                        ),
                    });
                }
                self.check_expr(index, ports_ok, context);
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
                let vec_shape = self.record_field_vec_shape(
                    *local,
                    field,
                    path,
                    mid_indices,
                    index.as_deref(),
                );
                let ty = self.record_field_type(*local, field, path, mid_indices);
                match self.whole_collection_shape(vec_shape, ty) {
                    Ok(Some(shape)) if !whole_vec_ok => self.report_bad_whole_vec_use(format!(
                        "record collection `%{}.{}` with shape {shape:?} appears outside matching equality/copy",
                        local.0,
                        std::iter::once(field.as_str())
                            .chain(path.iter().map(String::as_str))
                            .collect::<Vec<_>>()
                            .join(".")
                    )),
                    Ok(_) => {}
                    Err(detail) => self.report_bad_whole_vec_use(detail),
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
            Expr::ComponentField { base, field } => {
                let vec_shape = self.component_field_vec_shape(base, field);
                let ty = self.component_field_type(base, field);
                match self.whole_collection_shape(vec_shape, ty) {
                    Ok(Some(shape)) if !whole_vec_ok => self.report_bad_whole_vec_use(format!(
                        "component collection `{field}` with shape {shape:?} appears outside matching equality/copy"
                    )),
                    Ok(_) => {}
                    Err(detail) => self.report_bad_component_field(detail),
                }
            }
            Expr::ComponentVecElement {
                base,
                field,
                index_pos,
                index,
                inner_index,
            } => {
                self.check_expr(index, ports_ok, context);
                match self.component_indexed_field_type(base, field, *index_pos) {
                    Ok((ty, len)) => {
                        if matches!(index.as_ref(), Expr::Literal { value, .. } if *value as usize >= len)
                        {
                            self.report_bad_component_field(format!(
                                "indexed component field `{field}` is out of bounds for length {len}"
                            ));
                        }
                        // A nested read `v[i][j]`: the outer element
                        // `ty` must be a `FixedVec`; the second index is
                        // bounds-checked against its inner length.
                        if let Some(inner) = inner_index {
                            self.check_expr(inner, ports_ok, context);
                            match &ty {
                                IrType::FixedVec {
                                    len: inner_len, ..
                                } => {
                                    if matches!(inner.as_ref(), Expr::Literal { value, .. } if *value as usize >= *inner_len)
                                    {
                                        self.report_bad_component_field(format!(
                                            "nested index into component field `{field}` is out of bounds for length {inner_len}"
                                        ));
                                    }
                                }
                                _ => self.report_bad_component_field(format!(
                                    "nested index read on component field `{field}` whose element is not a fixed vector"
                                )),
                            }
                        } else if let IrType::Seq(elem) = ty {
                            let shape = crate::codegen::cpp_tb::ir_vec_elem_class(&elem)
                                .map(WholeCollectionShape::DynamicSeq);
                            match shape {
                                Some(shape) if !whole_vec_ok => self.report_bad_whole_vec_use(
                                    format!("indexed component collection `{field}` with shape {shape:?} appears outside matching equality/copy"),
                                ),
                                Some(_) => {}
                                None => self.report_bad_component_field(
                                    "indexed component dynamic list has an invalid element type".to_string(),
                                ),
                            }
                        }
                    }
                    Err(detail) => self.report_bad_component_field(detail),
                }
            }
            Expr::TbFieldVecElement {
                field,
                index,
                inner_index,
            } => {
                self.check_tb_field(field);
                let vec_ty = self.tb_scalar_field_ty(field);
                self.check_fixed_vec_element_read(
                    vec_ty,
                    index,
                    inner_index.as_deref(),
                    ports_ok,
                    context,
                    "TbFieldVecElement",
                );
            }
            // A by-value component passed as a method arg. A `Local` base
            // is a method-param local (verify it is defined); a
            // `SelfField`/`Path` base is resolved at lowering.
            Expr::ComponentValue { base } => match base {
                crate::ir::ComponentBase::Local(l) => {
                    self.check_local(*l);
                    if !matches!(
                        self.func.locals.get(l.index()).map(|local| &local.ty),
                        Some(IrType::Component(_))
                    ) {
                        self.report_bad_component_field(
                            "component value local is not component-typed".to_string(),
                        );
                    }
                }
                _ if self.component_base_id(base).is_err() => self.report_bad_component_field(
                    "component value base does not resolve".to_string(),
                ),
                _ => {}
            },
            // Component-queue size/empty read — host state resolved at
            // lowering against the component schema; nothing to verify.
            Expr::ComponentQueueQuery { .. } => {}
            Expr::DynamicListQuery { target, .. } => {
                match self.expr_whole_collection_shape(target) {
                    Ok(Some(WholeCollectionShape::DynamicSeq(_))) => {
                        self.check_expr_inner(target, ports_ok, context, true);
                    }
                    Ok(shape) => {
                        self.report_bad_whole_vec_use(format!(
                            "dynamic-list query receiver is not a dynamic list: {shape:?}"
                        ));
                        self.check_expr_inner(target, ports_ok, context, shape.is_some());
                    }
                    Err(detail) => {
                        self.report_bad_whole_vec_use(detail);
                        self.check_expr(target, ports_ok, context);
                    }
                }
            }
            Expr::ComponentIdle {
                base, subpath, n, ..
            } => {
                self.check_component_idle(base, subpath);
                self.check_expr(n, ports_ok, context);
            }
            Expr::TransactorIdle {
                field,
                transactor,
                storage,
                n,
                ..
            } => {
                self.check_transactor_idle(field, *transactor, storage);
                self.check_expr(n, ports_ok, context);
            }
            Expr::ScoreboardQuery {
                sb,
                field,
                query,
                nested_path,
            } => {
                self.check_scoreboard(*sb, field, nested_path.as_deref());
                match query {
                    crate::ir::ScoreboardQuery::Scalar { scalar } => {
                        self.check_scoreboard_scalar(*sb, scalar)
                    }
                    crate::ir::ScoreboardQuery::QueueSize { queue }
                    | crate::ir::ScoreboardQuery::QueueEmpty { queue } => {
                        self.check_scoreboard_container(*sb, queue)
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
                if !matches!(
                    self.func.locals.get(seq.index()).map(|local| &local.ty),
                    Some(IrType::RecordSeq(_) | IrType::Seq(_) | IrType::FixedVec { .. })
                ) {
                    self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} sequence/fixed-vector index receiver %{} is not indexable",
                            self.fid.0, self.bid.0, seq.0
                        ),
                    });
                }
                self.check_expr(index, ports_ok, context);
            }
            Expr::Call(target, args) => {
                let helper_schema = if let CallTarget::Helper { name, ret } = target {
                    self.prog
                        .functions
                        .iter()
                        .find(|function| {
                        function.kind == FunctionKind::Helper && function.name == *name
                        })
                        .map(|helper| {
                            let params = helper
                                .params
                                .iter()
                                .map(|param| param.ty.clone())
                                .collect::<Vec<_>>();
                            let actual_ret = helper
                                .ret
                                .and_then(|local| helper.locals.get(local.index()))
                                .map(|local| local.ty.clone());
                            (params, actual_ret)
                        })
                        .or_else(|| {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} helper call references missing helper `{name}`",
                                    self.fid.0, self.bid.0
                                ),
                            });
                            None
                        })
                        .map(|(params, actual_ret)| {
                            if params.len() != args.len() {
                                self.errs.push(VerifyError::BadProgramRef {
                                    what: format!(
                                        "fn{} b{} helper `{name}` arity mismatch: function has {}, call carries {}",
                                        self.fid.0,
                                        self.bid.0,
                                        params.len(),
                                        args.len()
                                    ),
                                });
                            }
                            match actual_ret.as_ref() {
                                Some(actual) if actual != ret => {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} helper `{name}` return metadata mismatch: function has {actual:?}, call carries {ret:?}",
                                            self.fid.0, self.bid.0
                                        ),
                                    });
                                }
                                None if !matches!(ret, IrType::Unknown) => {
                                    self.errs.push(VerifyError::BadProgramRef {
                                        what: format!(
                                            "fn{} b{} void helper `{name}` call carries return type {ret:?}",
                                            self.fid.0, self.bid.0
                                        ),
                                    });
                                }
                                _ => {}
                            }
                            (params, actual_ret)
                        })
                } else {
                    None
                };
                if let (CallTarget::Helper { name, .. }, Some((params, _))) =
                    (target, helper_schema.as_ref())
                {
                    for (index, (arg, param_ty)) in args.iter().zip(params).enumerate() {
                        let actual = if matches!(param_ty, IrType::FixedVec { .. }) {
                                self.expr_whole_vec_type(arg)
                                    .ok()
                                    .flatten()
                                    .unwrap_or(IrType::Unknown)
                        } else {
                            self.aggregate_assignment_expr_type(arg)
                                .unwrap_or(IrType::Unknown)
                        };
                        if (matches!(
                            param_ty,
                            IrType::FixedVec { .. } | IrType::Seq(_) | IrType::RecordSeq(_)
                        ) || matches!(
                            &actual,
                            IrType::FixedVec { .. } | IrType::Seq(_) | IrType::RecordSeq(_)
                        )) && *param_ty != actual
                        {
                            self.errs.push(VerifyError::BadProgramRef {
                                what: format!(
                                    "fn{} b{} helper `{name}` argument {} expects {:?}, got {:?}",
                                    self.fid.0,
                                    self.bid.0,
                                    index + 1,
                                    param_ty,
                                    actual
                                ),
                            });
                        }
                        self.check_expr_inner(
                            arg,
                            ports_ok,
                            context,
                            matches!(param_ty, IrType::FixedVec { .. }),
                        );
                    }
                }
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
                if let CallTarget::Tseq(name) = target {
                    self.errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "fn{} b{} tseq `{name}` call in a disallowed position ({context}); \
                             it must be the entire RHS of an Assign",
                            self.fid.0, self.bid.0
                        ),
                    });
                }
                if helper_schema.is_none() {
                    for a in args {
                        self.check_expr(a, ports_ok, context);
                    }
                }
            }
        }
    }

    fn check_tseq_call(&mut self, dest: LocalId, name: &str, args: &[Expr]) {
        let mut matches = self.prog.functions.iter().filter(|function| {
            function.name == name && matches!(function.kind, FunctionKind::Tseq { .. })
        });
        let Some(target) = matches.next() else {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} tseq call `{name}` resolves to no Tseq function",
                    self.fid.0, self.bid.0
                ),
            });
            return;
        };
        if matches.next().is_some() {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} tseq call `{name}` resolves ambiguously",
                    self.fid.0, self.bid.0
                ),
            });
            return;
        }
        if args.len() != target.params.len() {
            self.errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} b{} tseq call `{name}` takes {} arguments, got {}",
                    self.fid.0,
                    self.bid.0,
                    target.params.len(),
                    args.len()
                ),
            });
        }
        for (index, (arg, param)) in args.iter().zip(&target.params).enumerate() {
            self.check_expr(arg, false, "Tseq argument");
            let actual = self
                .aggregate_assignment_expr_type(arg)
                .unwrap_or(IrType::Unknown);
            if actual != IrType::Unknown && !assign_compatible(&param.ty, &actual) {
                self.errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "fn{} b{} tseq call `{name}` argument {} expects {:?}, got {:?}",
                        self.fid.0,
                        self.bid.0,
                        index + 1,
                        param.ty,
                        actual
                    ),
                });
            }
        }
        let expected = match &target.kind {
            FunctionKind::Tseq { elem } => elem.seq_type(),
            _ => unreachable!("filtered to Tseq functions"),
        };
        if self
            .func
            .locals
            .get(dest.index())
            .is_some_and(|local| local.ty != expected)
        {
            self.errs.push(VerifyError::TypeMismatch {
                func: self.fid,
                block: self.bid,
                local: dest,
                expected,
                actual: self.func.local(dest).ty.clone(),
            });
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
        if args.len() != m.param_names.len() {
            self.errs.push(bad(format!(
                "transactor method `{}.{method}` takes {} argument(s), call passes {}",
                schema.name,
                m.param_names.len(),
                args.len()
            )));
        }
        for (i, arg) in args.iter().enumerate() {
            let expected = m.param_tys.get(i);
            self.check_expr_inner(
                arg,
                false,
                "TransactorSelfCall arg",
                matches!(expected, Some(IrType::FixedVec { .. })),
            );
            if let Some(expected) = expected {
                let actual = if matches!(expected, IrType::FixedVec { .. }) {
                    self.expr_whole_vec_type(arg).ok().flatten()
                } else {
                    expr_type(self.prog, self.func, arg)
                };
                match actual {
                    Some(actual) if assign_compatible(expected, &actual) => {}
                    Some(actual) => self.errs.push(bad(format!(
                        "transactor method `{}.{method}` parameter {} expects {expected:?}, got {actual:?}",
                        schema.name,
                        i + 1
                    ))),
                    None if matches!(expected, IrType::FixedVec { .. }) => {
                        self.errs.push(bad(format!(
                            "transactor method `{}.{method}` parameter {} expects {expected:?}, got a non-fixed-vector value",
                            schema.name,
                            i + 1
                        )));
                    }
                    None => {}
                }
            }
        }
        if let Some(dest) = dest {
            let actual = self.func.locals.get(dest.index()).map(|local| &local.ty);
            match (&m.ret_ty, actual) {
                (Some(IrType::Unknown), Some(IrType::Unknown)) => {}
                (Some(expected), Some(actual))
                    if !matches!(actual, IrType::Unknown)
                        && assign_compatible(actual, expected) => {}
                (Some(expected), Some(actual)) => self.errs.push(bad(format!(
                    "transactor method `{}.{method}` returns {expected:?}, but destination is {actual:?}",
                    schema.name
                ))),
                (None, _) => self.errs.push(bad(format!(
                    "void transactor method `{}.{method}` captured into a destination",
                    schema.name
                ))),
                _ => {}
            }
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
    fn check_bus_call_edge(
        &mut self,
        dest: Option<LocalId>,
        bus_field: &str,
        method: &str,
        args: &[Expr],
    ) {
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
            if self
                .aggregate_assignment_expr_type(a)
                .and_then(|ty| match ty {
                    IrType::Record(record) => Some(record),
                    _ => None,
                })
                .is_some_and(|record| record_contains_dynamic_list(self.prog, record))
            {
                self.bad_transactor(format!(
                    "`{bus_field}.{method}` carries a dynamic-list record over a fixed TLM request wire"
                ));
            }
        }
        if dest
            .and_then(|local| self.func.locals.get(local.index()))
            .and_then(|local| match local.ty {
                IrType::Record(record) => Some(record),
                _ => None,
            })
            .is_some_and(|record| record_contains_dynamic_list(self.prog, record))
        {
            self.bad_transactor(format!(
                "`{bus_field}.{method}` returns a dynamic-list record over a fixed TLM response wire"
            ));
        }
    }
}

fn record_contains_dynamic_list(prog: &TbProgram, record: RecordId) -> bool {
    let mut pending = vec![record];
    let mut seen = std::collections::HashSet::new();
    while let Some(next) = pending.pop() {
        if !seen.insert(next) {
            continue;
        }
        let Some(schema) = prog.records.get(next.index()) else {
            continue;
        };
        for field in &schema.fields {
            match field.ty {
                IrType::Seq(_) => return true,
                IrType::Record(inner) => pending.push(inner),
                _ => {}
            }
        }
    }
    false
}

fn check_port_snapshot_definitions(
    func: &TbFunction,
    fid: FunctionId,
    errs: &mut Vec<VerifyError>,
) {
    for (index, local) in func.locals.iter().enumerate() {
        if !matches!(local.ty, IrType::PortSnapshot) {
            continue;
        }
        let snapshot = LocalId(index as u32);
        let definitions = func
            .blocks
            .iter()
            .flat_map(|block| &block.stmts)
            .filter(|stmt| {
                matches!(stmt, Stmt::DutRead(dest, _) | Stmt::Assign(dest, _) if *dest == snapshot)
            })
            .count();
        if definitions != 1 {
            errs.push(VerifyError::BadProgramRef {
                what: format!(
                    "fn{} ordered snapshot l{} must have exactly one Assign/DutRead definition (found {definitions})",
                    fid.0, snapshot.0
                ),
            });
        }
    }
}

/// Best-effort expression typing for invariant 15. Returns `None` when
/// the expression's type cannot be locally determined.
fn common_scalar_expr_type(lhs: Option<IrType>, rhs: Option<IrType>) -> Option<IrType> {
    let (lhs, rhs) = match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => (lhs, rhs),
        (lhs, rhs) => return lhs.or(rhs),
    };
    if lhs == rhs {
        return Some(lhs);
    }
    if matches!(lhs, IrType::Unknown) {
        return Some(rhs);
    }
    if matches!(rhs, IrType::Unknown) {
        return Some(lhs);
    }
    let scalar = |ty: &IrType| match ty {
        IrType::UInt(width) => Some((*width, false)),
        IrType::SInt(width) => Some((*width, true)),
        IrType::Bool => Some((Some(1), false)),
        _ => None,
    };
    let (Some((lw, ls)), Some((rw, rs))) = (scalar(&lhs), scalar(&rhs)) else {
        return Some(lhs);
    };
    let width = Some(lw.unwrap_or(64).max(rw.unwrap_or(64)));
    Some(if ls && rs {
        IrType::SInt(width)
    } else {
        IrType::UInt(width)
    })
}

fn assignment_expr_type(prog: &TbProgram, func: &TbFunction, e: &Expr) -> Option<IrType> {
    match e {
        Expr::Literal {
            value,
            ty: IrType::Unknown,
        } => Some(IrType::UInt(Some((64 - value.leading_zeros()).max(1)))),
        Expr::Binary(BinOp::BitAnd, lhs, rhs) => {
            let bounded = |e: &Expr| {
                if let Expr::Literal { value, ty } = e {
                    if matches!(ty, IrType::Unknown) {
                        return Some(IrType::UInt(Some((64 - value.leading_zeros()).max(1))));
                    }
                }
                assignment_expr_type(prog, func, e)
            };
            let (lhs, rhs) = (bounded(lhs), bounded(rhs));
            let shape = |ty: IrType| match ty {
                IrType::UInt(width) => Some((width, false)),
                IrType::SInt(width) => Some((width, true)),
                IrType::Bool => Some((Some(1), false)),
                _ => None,
            };
            match (lhs.and_then(&shape), rhs.and_then(shape)) {
                (Some((lhs_width, ls)), Some((rhs_width, rs))) => {
                    let (lhs_abi, rhs_abi) = (lhs_width.unwrap_or(64), rhs_width.unwrap_or(64));
                    let selected = if lhs_abi < rhs_abi && !ls {
                        lhs_abi
                    } else if rhs_abi < lhs_abi && !rs {
                        rhs_abi
                    } else {
                        lhs_abi.max(rhs_abi)
                    };
                    let width = if lhs_width.is_none() && rhs_width.is_none() {
                        None
                    } else {
                        Some(selected)
                    };
                    Some(if ls && rs {
                        IrType::SInt(width)
                    } else {
                        IrType::UInt(width)
                    })
                }
                (Some((width, signed)), None) | (None, Some((width, signed))) => Some(if signed {
                    IrType::SInt(width)
                } else {
                    IrType::UInt(width)
                }),
                _ => None,
            }
        }
        Expr::Binary(BinOp::Shl | BinOp::Shr, lhs, _) => assignment_expr_type(prog, func, lhs),
        Expr::Binary(
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or,
            _,
            _,
        ) => Some(IrType::Bool),
        Expr::Binary(_, lhs, rhs) => common_scalar_expr_type(
            assignment_expr_type(prog, func, lhs),
            assignment_expr_type(prog, func, rhs),
        ),
        Expr::Ternary(_, then_expr, else_expr) => common_scalar_expr_type(
            assignment_expr_type(prog, func, then_expr),
            assignment_expr_type(prog, func, else_expr),
        ),
        Expr::Unary(crate::ir::UnOp::Not, _) => Some(IrType::Bool),
        Expr::Unary(crate::ir::UnOp::BitNotHost, _) => Some(IrType::SInt(None)),
        Expr::Unary(_, inner) => assignment_expr_type(prog, func, inner),
        _ => expr_type(prog, func, e),
    }
}

fn expr_type(prog: &TbProgram, func: &TbFunction, e: &Expr) -> Option<IrType> {
    match e {
        Expr::Literal { ty, .. } => Some(ty.clone()),
        Expr::WideLiteral(words) => Some(IrType::UInt(Some(wide_literal_bits(words)))),
        Expr::Local(l) => func.locals.get(l.index()).map(|t| t.ty.clone()),
        Expr::SeqIndex { seq, .. } => match func.locals.get(seq.index()).map(|local| &local.ty) {
            Some(IrType::RecordSeq(record)) => Some(IrType::Record(*record)),
            Some(IrType::Seq(elem)) => Some((**elem).clone()),
            Some(IrType::FixedVec { elem, .. }) => Some((**elem).clone()),
            _ => None,
        },
        Expr::BitSlice { hi, lo, .. } => Some(IrType::UInt(Some(hi - lo + 1))),
        // Runtime bounds: unsigned, width unknown until the slice runs.
        // `UInt(None)` is invariant 15's widthless wildcard, which is
        // what a `uint64_t` helper return is here.
        Expr::BitSliceDyn { .. } => Some(IrType::UInt(None)),
        Expr::WidthCast { kind, width, .. } => Some(match kind {
            crate::ir::WidthCastKind::Sext => IrType::SInt(Some(*width)),
            _ => IrType::UInt(Some(*width)),
        }),
        Expr::Call(CallTarget::Helper { ret, .. } | CallTarget::ExternFn { ret, .. }, _) => {
            Some(ret.clone())
        }
        Expr::ComponentIdle { .. } | Expr::TransactorIdle { .. } => Some(IrType::Bool),
        Expr::DynamicListQuery {
            query: crate::ir::DynamicListQuery::Size,
            ..
        } => Some(IrType::UInt(None)),
        Expr::DynamicListQuery {
            query: crate::ir::DynamicListQuery::Empty,
            ..
        } => Some(IrType::Bool),
        Expr::ScoreboardQuery {
            sb,
            query: ScoreboardQuery::Scalar { scalar },
            ..
        } => prog
            .scoreboards
            .get(sb.index())
            .and_then(|schema| schema.field(scalar))
            .and_then(|field| match &field.kind {
                ScoreboardFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                ScoreboardFieldKind::Record { record } => Some(IrType::Record(*record)),
                _ => None,
            }),
        Expr::ScoreboardQuery {
            query: ScoreboardQuery::QueueSize { .. },
            ..
        } => Some(IrType::UInt(None)),
        Expr::ScoreboardQuery {
            query: ScoreboardQuery::QueueEmpty { .. },
            ..
        } => Some(IrType::Bool),
        _ => None,
    }
}

/// Whether `actual` can enter the queue's element slot. Unknown expressions
/// remain conservatively accepted; known scalars obey the ordinary assignment
/// direction, including width and signedness.
fn queue_elem_accepts_type(elem: &QueueElem, actual: &IrType) -> bool {
    matches!(actual, IrType::Unknown) || assign_compatible(&elem.ir_type(), actual)
}

/// Whether a value popped from `elem` can enter `dest`. This is the reverse
/// assignment direction from a push: a wider destination is valid, while a
/// narrower destination would lose queue data.
fn queue_elem_fits_dest(elem: &QueueElem, dest: &IrType) -> bool {
    matches!(dest, IrType::Unknown) || assign_compatible(dest, &elem.ir_type())
}

fn assign_compatible(expected: &IrType, actual: &IrType) -> bool {
    // Ordinary integer literals carry `Unknown` until a destination gives
    // them a width. Treat that as the same conservative wildcard used by
    // queue transfers; source lowering has already rejected literals that do
    // not fit their destination.
    if matches!(expected, IrType::Unknown) || matches!(actual, IrType::Unknown) {
        return true;
    }
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

fn aggregate_assignment_compatible(expected: &IrType, actual: &IrType) -> bool {
    match (expected, actual) {
        (IrType::Record(expected), IrType::Record(actual)) => expected == actual,
        (IrType::Record(_), _) | (_, IrType::Record(_)) => false,
        _ => assign_compatible(expected, actual),
    }
}

/// Whole fixed-vector copies follow the same rule as lowering and v1: the
/// arrays must have the same length and the same emitted C++ element carrier.
/// Declared scalar widths that share that carrier remain copy-compatible.
fn fixed_vec_abi_compatible(expected: &IrType, actual: &IrType) -> bool {
    let (
        IrType::FixedVec {
            elem: expected_elem,
            len: expected_len,
        },
        IrType::FixedVec {
            elem: actual_elem,
            len: actual_len,
        },
    ) = (expected, actual)
    else {
        return false;
    };
    let (Some(expected_class), Some(actual_class)) = (
        crate::codegen::cpp_tb::ir_vec_elem_class(expected_elem),
        crate::codegen::cpp_tb::ir_vec_elem_class(actual_elem),
    ) else {
        return false;
    };
    expected_len == actual_len && expected_class == actual_class
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
    prog: &TbProgram,
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
                Stmt::Assign(l, _) | Stmt::DutRead(l, _) | Stmt::RecordInit(l, _)
                | Stmt::RecordRead { dest: l, .. } => {
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
                Stmt::DutRead(l, _) | Stmt::RecordInit(l, _) | Stmt::AggregateInit(l) => {
                    bit_set(&mut defined, l.index());
                }
                Stmt::RecordRead {
                    dest, local, addr, ..
                } => {
                    if local.index() < nlocals && !bit_get(&defined, local.index()) {
                        errs.push(VerifyError::LocalUseBeforeDef {
                            func: fid,
                            block: bid,
                            local: *local,
                        });
                    }
                    check_e(addr, &defined, errs);
                    bit_set(&mut defined, dest.index());
                }
                Stmt::RecordWrite {
                    local, addr, value, ..
                } => {
                    if local.index() < nlocals && !bit_get(&defined, local.index()) {
                        errs.push(VerifyError::LocalUseBeforeDef {
                            func: fid,
                            block: bid,
                            local: *local,
                        });
                    }
                    check_e(addr, &defined, errs);
                    check_e(value, &defined, errs);
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
                Stmt::TransactorStateRecordFieldWrite {
                    mid_indices,
                    index,
                    value,
                    ..
                } => {
                    for (_, idx) in mid_indices {
                        check_e(idx, &defined, errs);
                    }
                    if let Some(idx) = index {
                        check_e(idx, &defined, errs);
                    }
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
                // Concurrent-check schemas live outside the CFG, but
                // their closures capture locals from this registering
                // function. Account for those hidden reads at the exact
                // registration point, just like an inline expression.
                Stmt::PropertyCheck(id) => {
                    if let Some(schema) = prog.property_checks.get(id.index()) {
                        match &schema.shape {
                            PropertyShape::Implies { ante, cons }
                            | PropertyShape::ImpliesNext { ante, cons } => {
                                check_e(ante, &defined, errs);
                                check_e(cons, &defined, errs);
                            }
                            PropertyShape::Invariant(expr) => check_e(expr, &defined, errs),
                        }
                        for temporal in &schema.temporals {
                            check_e(&temporal.inner, &defined, errs);
                        }
                        if let Some(message) = &schema.message {
                            for arg in &message.args {
                                check_e(&arg.expr, &defined, errs);
                            }
                        }
                    }
                }
                Stmt::CoverCheck(id) => {
                    if let Some(schema) = prog.cover_checks.get(id.index()) {
                        check_e(&schema.cond, &defined, errs);
                        for temporal in &schema.temporals {
                            check_e(&temporal.inner, &defined, errs);
                        }
                    }
                }
                Stmt::CycleHandler(id) => {
                    if let Some(CycleHandlerSchema {
                        kind: CycleHandlerKind::Trigger { trigger, .. },
                        ..
                    }) = prog.cycle_handlers.get(id.index())
                    {
                        check_e(trigger, &defined, errs);
                    }
                }
                // The channel local is DEFINED by its declaration (the
                // emitter declares the subscriber vector at the hoisted
                // local site), so subscribing/emitting only reads it —
                // and reading it is not an expression, so there is
                // nothing for `check_e` to walk. The payload args are.
                Stmt::EventSubscribe { .. } => {}
                Stmt::MethodHookSubscribe { captures, .. } => {
                    for capture in captures {
                        if capture.index() < nlocals && !bit_get(&defined, capture.index()) {
                            errs.push(VerifyError::LocalUseBeforeDef {
                                func: fid,
                                block: bid,
                                local: *capture,
                            });
                        }
                    }
                }
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
                Stmt::ComponentVecElementWrite {
                    index,
                    inner_index,
                    value,
                    ..
                } => {
                    check_e(index, &defined, errs);
                    if let Some(inner) = inner_index {
                        check_e(inner, &defined, errs);
                    }
                    check_e(value, &defined, errs);
                }
                // A fixed-vector testbench-field element write reads its
                // index/value exprs (the `_tb` receiver is host state, not
                // a test local).
                Stmt::TbFieldVecElementWrite {
                    index,
                    inner_index,
                    value,
                    ..
                } => {
                    check_e(index, &defined, errs);
                    if let Some(inner) = inner_index {
                        check_e(inner, &defined, errs);
                    }
                    check_e(value, &defined, errs);
                }
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
            // No operands: the re-inlined lifecycle body carries its own
            // locals; nothing is used or defined in the caller frame.
            Terminator::TbLifecycleCall { .. } => {}
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
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::ComponentField { .. }
        | Expr::ScoreboardQuery { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::CovHookArg { .. } => {}
        Expr::TransactorStateRecordField {
            mid_indices, index, ..
        } => {
            for (_, idx) in mid_indices {
                for_each_local(idx, f);
            }
            if let Some(idx) = index {
                for_each_local(idx, f);
            }
        }
        Expr::ComponentVecElement {
            index, inner_index, ..
        }
        | Expr::TbFieldVecElement {
            index, inner_index, ..
        } => {
            for_each_local(index, f);
            if let Some(inner) = inner_index {
                for_each_local(inner, f);
            }
        }
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
        Expr::DynamicListQuery { target, .. } => for_each_local(target, f),
        Expr::BitSlice { target, .. } => for_each_local(target, f),
        Expr::BitSliceDyn { target, hi, lo } => {
            for_each_local(target, f);
            for_each_local(hi, f);
            for_each_local(lo, f);
        }
        Expr::PortSnapshotLane {
            snapshot, index, ..
        } => {
            f(*snapshot);
            for_each_local(index, f);
        }
        Expr::Ternary(c, t, e) => {
            for_each_local(c, f);
            for_each_local(t, f);
            for_each_local(e, f);
        }
        Expr::WidthCast { inner, .. } => for_each_local(inner, f),
        Expr::ComponentIdle { base, n, .. } => {
            if let ComponentBase::Local(local) = base {
                f(*local);
            }
            for_each_local(n, f);
        }
        Expr::TransactorIdle { n, .. } => for_each_local(n, f),
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
