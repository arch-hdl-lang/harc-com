//! Native component-path, hookable-method, and activity-predicate resolution.
//!
//! These helpers resolve nested component calls and event hooks while keeping
//! built-in idle/quiesced behavior aligned with transactor activation modes.

use super::*;

impl Emitter {
    /// If `event` is the trigger of an `on obj.method pre/post`
    /// handler, resolve `obj.method` to a known hookable on a known
    /// component type. Returns `(component_type_name, method_name,
    /// params)`. Walks one or two levels of field-access chain
    /// (covers `<var>.<method>` and `<env>.<sub>.<method>`).
    pub(super) fn resolve_component_hookable(
        &self,
        event: &Expr,
    ) -> Option<(String, String, Vec<Param>)> {
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
    pub(super) fn resolve_component_idle_predicate(
        &self,
        callee: &Expr,
    ) -> Option<(String, String)> {
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

    pub(super) fn resolve_bare_sibling_method_call(
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

    pub(super) fn resolve_component_quiesced_predicate<'a>(
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

    pub(super) fn emit_idle_predicate(&mut self, instance: &str, kind: &str, n_expr: &Expr) {
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

    pub(super) fn expand_wait_conditions<'a>(
        &self,
        conditions: &'a [Expr],
    ) -> Vec<WaitCondition<'a>> {
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

    pub(super) fn resolve_component_method_call(
        &self,
        callee: &Expr,
    ) -> Option<(String, String, String)> {
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
    pub(super) fn extract_method_call_path(callee: &Expr) -> Option<Vec<String>> {
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
    pub(super) fn resolve_path_mode(&self, path: &[String]) -> Option<TransactorMode> {
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
    pub(super) fn method_lives_in_when_active(
        &self,
        transactor_name: &str,
        method_name: &str,
    ) -> bool {
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
    pub(super) fn handler_lives_in_when_active(&self, comp_ty: &str, handler: &OnHandler) -> bool {
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
}
