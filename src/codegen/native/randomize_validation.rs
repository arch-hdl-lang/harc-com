//! Validation and field-access helpers shared by native randomization renderers.
//!
//! These routines remain on `Emitter` while the constraint solver renderer is
//! incrementally separated from the monolithic native backend.

use super::*;

impl Emitter {
    pub(super) fn report_runtime_dependent_randomize_field_attrs(&mut self, ty: &str) {
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

    pub(super) fn emit_target_field_access(&mut self, target: &Expr, f: &TxnFieldInfo) {
        self.emit_randomize_target(target);
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

    pub(super) fn validate_randomize_constraint_dependencies(
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
}
