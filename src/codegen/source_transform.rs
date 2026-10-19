//! Source normalization shared by lowering and native emitters.

use crate::ast::*;
use crate::lexer::Span;

pub(crate) type SharedLifecycleBodies = std::collections::HashMap<(String, LifecyclePhase), Block>;

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub(crate) enum RecordLeafFate {
    Models,
    Flattens,
    Uncompilable,
}

pub(crate) struct Temporal {
    pub(crate) call_span: Span,
    pub(crate) kind: SystemFn,
    pub(crate) inner: Expr,
}

pub(crate) fn named_type_last_segment(t: &TypeExpr) -> Option<&str> {
    match t {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

pub(crate) fn is_list_type(t: &TypeExpr) -> bool {
    matches!(named_type_last_segment(t), Some("list") | Some("List"))
}

pub(crate) fn list_type_args(t: &TypeExpr) -> Option<&[TypeArg]> {
    match t {
        TypeExpr::Named { generics, .. } if is_list_type(t) => Some(generics.as_slice()),
        _ => None,
    }
}

pub(crate) fn list_elem_type(t: &TypeExpr) -> Option<&TypeExpr> {
    list_type_args(t)?.first().and_then(|arg| match arg {
        TypeArg::Type(ty) => Some(ty),
        _ => None,
    })
}

pub(crate) fn type_arg_const_usize(arg: &TypeArg) -> Option<usize> {
    match arg {
        TypeArg::Expr(e) => const_usize_expr(e),
        TypeArg::Named { value, .. } => const_usize_expr(value),
        _ => None,
    }
}

pub(crate) fn const_usize_expr(e: &Expr) -> Option<usize> {
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

#[derive(Clone, Copy, Debug)]
pub(crate) enum ScalarLeafKind {
    Unsigned,
    Signed,
    Bool,
}

pub(crate) fn scalar_leaf_kind(name: &BuiltinTy) -> Option<ScalarLeafKind> {
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
            Some(ScalarLeafKind::Unsigned)
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(ScalarLeafKind::Signed),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(ScalarLeafKind::Bool),
        _ => None,
    }
}

pub(crate) fn list_elem_random_kind(elem: &TypeExpr) -> Option<ScalarLeafKind> {
    let TypeExpr::Builtin { name, .. } = elem else {
        return None;
    };
    match name {
        BuiltinTy::Int => None,
        _ => scalar_leaf_kind(name),
    }
}

pub(crate) fn record_type_name(t: &TypeExpr) -> Option<&str> {
    match t {
        TypeExpr::Named { name, .. } => name.segments.last().map(|segment| segment.name.as_str()),
        _ => None,
    }
}

pub(crate) fn txn_direct_fields(items: &[TxnBodyItem]) -> Vec<Field> {
    items
        .iter()
        .filter_map(|item| match item {
            TxnBodyItem::Field(field) => Some(field.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn txn_all_fields(items: &[TxnBodyItem]) -> Vec<Field> {
    fn collect(items: &[TxnBodyItem], out: &mut Vec<Field>) {
        for item in items {
            match item {
                TxnBodyItem::Field(field) => out.push(field.clone()),
                TxnBodyItem::When(when) => collect(&when.items, out),
                TxnBodyItem::Keep(_) => {}
            }
        }
    }

    let mut out = Vec::new();
    collect(items, &mut out);
    out
}

/// Normalize `Vec<Record, N>` element type arguments on record field
/// declarations. The parser represents a bare named element as an expression,
/// while downstream record-layout and randomization code expects a type.
pub(crate) fn normalize_vec_record_elem_fields(
    file: &SourceFile,
) -> std::borrow::Cow<'_, SourceFile> {
    let record_names: std::collections::HashSet<&str> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Struct(record) => Some(record.name.name.as_str()),
            Item::Transaction(record) => Some(record.name.name.as_str()),
            _ => None,
        })
        .collect();

    fn vec_record_elem(ty: &TypeExpr, record_names: &std::collections::HashSet<&str>) -> bool {
        let TypeExpr::Builtin {
            name: BuiltinTy::Vec,
            args,
            ..
        } = ty
        else {
            return false;
        };
        let Some(TypeArg::Expr(expr)) = args.first() else {
            return false;
        };
        let ExprKind::Ident(ident) = &*expr.kind else {
            return false;
        };
        record_names.contains(ident.name.as_str())
    }

    fn normalize_ty(ty: &mut TypeExpr, record_names: &std::collections::HashSet<&str>) {
        if !vec_record_elem(ty, record_names) {
            return;
        }
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
        let TypeArg::Expr(expr) = first else { return };
        let ExprKind::Ident(ident) = &*expr.kind else {
            return;
        };
        *first = TypeArg::Type(TypeExpr::Named {
            name: Path {
                segments: vec![ident.clone()],
                span: expr.span,
            },
            generics: Vec::new(),
            mode: None,
            span: expr.span,
        });
    }

    let needs_rewrite = file.items.iter().any(|item| match item {
        Item::Struct(record) => {
            record
                .fields
                .iter()
                .any(|field| vec_record_elem(&field.ty, &record_names))
                || record.body.iter().any(|body_item| match body_item {
                    TxnBodyItem::Field(field) => vec_record_elem(&field.ty, &record_names),
                    _ => false,
                })
        }
        Item::Transaction(record) => record.body.iter().any(|body_item| match body_item {
            TxnBodyItem::Field(field) => vec_record_elem(&field.ty, &record_names),
            _ => false,
        }),
        _ => false,
    });
    if !needs_rewrite {
        return std::borrow::Cow::Borrowed(file);
    }

    let mut normalized = file.clone();
    for item in &mut normalized.items {
        match item {
            Item::Struct(record) => {
                for field in &mut record.fields {
                    normalize_ty(&mut field.ty, &record_names);
                }
                for body_item in &mut record.body {
                    if let TxnBodyItem::Field(field) = body_item {
                        normalize_ty(&mut field.ty, &record_names);
                    }
                }
            }
            Item::Transaction(record) => {
                for body_item in &mut record.body {
                    if let TxnBodyItem::Field(field) = body_item {
                        normalize_ty(&mut field.ty, &record_names);
                    }
                }
            }
            _ => {}
        }
    }
    std::borrow::Cow::Owned(normalized)
}

fn collect_txn_keeps(items: &[TxnBodyItem]) -> Vec<Expr> {
    let mut out = Vec::new();
    collect_txn_keeps_with_guard(items, None, &mut out);
    out
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

pub(crate) fn and_join(exprs: &[Expr], span: Span) -> Expr {
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

pub(crate) fn prefix_record_keep_expr(
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

pub(crate) fn fixed_vec_type_args(t: &TypeExpr) -> Option<(&TypeExpr, usize)> {
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

#[derive(Clone, Copy, Debug)]
pub(crate) enum NativeContainerKind<'a> {
    List(Option<&'a TypeExpr>),
    FixedVec { elem: &'a TypeExpr, len: usize },
}

pub(crate) fn native_container_kind(t: &TypeExpr) -> Option<NativeContainerKind<'_>> {
    if is_list_type(t) {
        return Some(NativeContainerKind::List(list_elem_type(t)));
    }
    fixed_vec_type_args(t).map(|(elem, len)| NativeContainerKind::FixedVec { elem, len })
}

/// What this backend does with a record-field leaf — the whole basis on
/// which the TB-IR lowering decides whether its rejection may suggest
/// `--codegen v1`.
///
/// TB-IR lowers a narrower record subset than this backend does, so a
/// rejection for a leaf outside that subset has to say which of three
/// things is true, and none of them is a property of the type NAME:
///
///   * `list<T>` → `std::vector<T'>`, and `emit_field_random` draws the
///     element only for a scalar `T`. A `T` that models as a CONTAINER
///     keeps its `std::vector<std::array<…>>` member and then gets
///     `[_i] = 0`, which does not compile; any other undrawn `T` gets a
///     `uint64_t` element, which does.
///   * `Vec<T, N>` → `std::array<T', N>`, but only when
///     `fixed_vec_type_args` reads it — a non-literal `N` or an element
///     it cannot type collapses the whole member. There is no
///     per-element assignment on this path, so it cannot go
///     uncompilable.
///   * a scalar → a width-correct type at ANY width, including
///     `_harc_u128` above 64 bits and `HarcWide<n>` above 128.
///   * everything else (`queue`, `string`, `event`, `object`, an
///     unresolved name) → a bare scalar.
///
/// `is_record` answers `Emitter::is_record_type` — the extra layer
/// `record_field_c_type` puts on top of `txn_field_c_type`, and the one
/// that actually serves scoreboard and record members. Asking only the
/// free function missed it (divergence 103).
///
/// Consulted by the TB-IR lowering (`ir::lower::records` and
/// `ir::lower::scoreboards`). Writing a second copy of these rules there
/// is what produced divergence 97's first version, which called a
/// 128-bit field a flattening and a `list<Inner>` a working escape
/// hatch — both backwards.
pub(crate) fn record_leaf_fate(t: &TypeExpr, is_record: &dyn Fn(&str) -> bool) -> RecordLeafFate {
    match native_container_kind(t) {
        Some(NativeContainerKind::List(elem)) => {
            let Some(elem) = elem else {
                return RecordLeafFate::Flattens;
            };
            if list_elem_random_kind(elem).is_some() {
                return RecordLeafFate::Models;
            }
            // No draw: the loop body is `t->f[_i] = 0;`. Whether that
            // compiles is decided by the element type this backend picked,
            // so ask the same representation classifier the emitter uses.
            if native_container_kind(elem).is_some() {
                return RecordLeafFate::Uncompilable;
            }
            return RecordLeafFate::Flattens;
        }
        Some(NativeContainerKind::FixedVec { elem, .. }) => {
            return match record_leaf_fate(elem, is_record) {
                RecordLeafFate::Models => RecordLeafFate::Models,
                _ => RecordLeafFate::Flattens,
            };
        }
        None => {}
    }
    // The layer `record_field_c_type` adds on top of `txn_field_c_type`:
    // a named type that IS a declared record gets the record's own name
    // as its member type, not `int64_t`. Asking only `txn_field_c_type`
    // missed it and called `scoreboard Sb { l : Inner }` a flattening,
    // when v1 emits `Inner l;` and compiles.
    if let TypeExpr::Named { name, .. } = t {
        if name.segments.last().is_some_and(|seg| is_record(&seg.name)) {
            return RecordLeafFate::Models;
        }
    }
    match t {
        TypeExpr::Builtin { name, .. } if scalar_leaf_kind(name).is_some() => {
            RecordLeafFate::Models
        }
        _ => RecordLeafFate::Flattens,
    }
}

/// `Some(name)` when `t` renders as a Verilated module handle
/// (`V<name>*`) rather than a real C++ type — the `Named` fall-through
/// of `c_type_for` below, AFTER its `list`/`Vec` guard.
///
/// Exposed because TB-IR lowering has to refuse an `extern function`
/// whose signature names such a type: only `V<dut_type>.h` is ever
/// included, so any other `V<name>` is undeclared and the generated C++
/// does not compile. Lowering asking this function, rather than
/// restating the rule, is deliberate — the first version restated it as
/// "the `Named` arm is unconditional" and missed the guard on the line
/// above, which turned a `struct List` parameter (rendered
/// `std::vector<uint64_t>`, and compiling) into a rejection.
pub(crate) fn verilated_handle_name(t: &TypeExpr) -> Option<&str> {
    if is_list_type(t) || fixed_vec_type_args(t).is_some() {
        return None;
    }
    match t {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

/// The HARC name that `c_type_for` pastes as the ELEMENT of a
/// `TSeq<T>` / `queue<T>` — `const std::vector<T>&`,
/// `harc_rt::HarcQueue<T>&`. `None` when the element renders as a real
/// C++ type (a scalar) or the type is neither.
///
/// A record element is fine: transactions and structs are emitted as C++
/// structs of the same name, so `TSeq<Beat>` compiles. Anything else —
/// an enum, a module, an undeclared name — is pasted verbatim with no
/// declaration behind it, which is the same failure the `Named`
/// fall-through has, one level in.
pub(crate) fn element_type_name(t: &TypeExpr) -> Option<&str> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    if !matches!(name, BuiltinTy::TSeq | BuiltinTy::Queue) {
        return None;
    }
    match args.first()? {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Ident(id) => Some(id.name.as_str()),
            _ => None,
        },
        TypeArg::Type(inner) => verilated_handle_name(inner),
        _ => None,
    }
}

/// Reverse of [`tb_lifecycle_marker_name`]: the lifecycle phase a bare
/// call name denotes, or `None` if it is not a reserved marker. TB-IR
/// lowering uses this to recognize a `__harc_tb_lifecycle_<phase>()`
/// marker call and lower it to a `Terminator::TbLifecycleCall` (#619 M4a).
pub(crate) fn tb_lifecycle_marker_phase(name: &str) -> Option<LifecyclePhase> {
    match name {
        "__harc_tb_lifecycle_setup" => Some(LifecyclePhase::Setup),
        "__harc_tb_lifecycle_check" => Some(LifecyclePhase::Check),
        "__harc_tb_lifecycle_teardown" => Some(LifecyclePhase::Teardown),
        _ => None,
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

/// Recognise a temporal helper call in either form — bare `Call` AST with
/// callee `Ident("past"|"rose"|"fell"|"stable")` (the ARCH-aligned form,
/// what the parser produces today) or the legacy `SystemCall` AST (pre-
/// `$past`-removal). Returns `(kind, inner_arg)` on match.
pub(crate) fn match_temporal_call(e: &Expr) -> Option<(SystemFn, &Expr)> {
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
pub(crate) fn is_concurrent_assertion(
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

pub(crate) fn contains_temporal(e: &Expr) -> bool {
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
/// Pre-walk a property body and return one `Temporal` entry per
/// $past/$rose/$fell/$stable call. Order is left-to-right depth-first.
pub(crate) fn collect_temporal_occurrences(body: &Expr) -> Vec<Temporal> {
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
pub(crate) fn property_label(v: &Verify, raw: &Expr) -> String {
    if let Some(n) = &v.named {
        return n.name.clone();
    }
    if let ExprKind::Ident(id) = &*raw.kind {
        return id.name.clone();
    }
    "<inline>".to_string()
}

/// Render the lexer's raw string-token payload as a C++ value literal.
/// Message literals use the interpolation path instead; this function is
/// only for first-class `String` values and decodes recognized HARC escapes
/// exactly once. Unknown escapes retain their leading backslash.
pub(crate) fn string_value_literal_cpp(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            match ch {
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\0' => out.push_str("\\000"),
                other => out.push(other),
            }
            continue;
        }
        match chars.next() {
            Some('0') => out.push_str("\\000"),
            Some(next @ ('\\' | '"' | 'n' | 'r' | 't')) => {
                out.push('\\');
                out.push(next);
            }
            Some(other) => {
                out.push_str("\\\\");
                out.push(other);
            }
            None => out.push_str("\\\\"),
        }
    }
    out.push('"');
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

/// Collect a record's direct keeps plus recursively-prefixed keeps from
/// nested record fields. Shared with TB-IR lowering so both emitters build
/// identical call-site constraint expressions.
pub(crate) fn collect_record_keeps(
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
