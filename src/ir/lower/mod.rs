//! AST → TB-IR lowering (docs/tb-ir-design.md §"AST → IR lowering rules").
//!
//! MVP scope: classic-form and impl-for testbench-bound tests with a
//! single DUT, declared clocks, and the core statement subset (DUT
//! port read/write, `let`/assign, `log`/`logf`, inline `assert ...
//! else fail`, `fail`, `wait N cycles`, `wait until` in its single
//! and `all of` forms with optional `timeout`, `if`/`for`/`while`/
//! `repeat`/`loop`/`break`/`continue`). Everything else is rejected
//! with
//! `LowerError::Unsupported` naming the construct — lowering NEVER
//! silently mis-lowers. Re-run with `--codegen v1` to use the legacy
//! direct AST → C++ path for unsupported constructs.

mod addrmap;
mod bus;
mod components;
mod control;
mod covergroups;
mod exprs;
mod helpers;
mod records;
mod regblock;
mod scoreboards;
mod stmts;
mod transactors;
mod tseqs;

use crate::ast::{
    AddrmapDecl, Block, BuiltinTy, BusDecl, ClockDecl, ComponentDecl, ComponentItem, ExprKind,
    HookSide, HookableMethod, Item, OnPhase, ScopeDecl, SourceFile, Stmt as AstStmt, StmtKind,
    TestDecl, TestItem, TransactorMode, TypeExpr,
};
use crate::ir::{
    self, BasicBlock, BlockId, ClockSpec, ComponentSchema, ConstraintRef, ConstraintSite,
    CovgroupId, CovgroupSchema, FunctionId, FunctionKind, IrType, LocalId, RecordId, RecordSchema,
    RegblockId, ScoreboardId, ScoreboardSchema, TbFunction, TbProgram, Terminator, TestSchema,
    TestbenchId, TestbenchSchema, TransactorId, TransactorSchema, TseqElem, TypedLocal, TypedParam,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub enum LowerError {
    /// The construct is outside the TB-IR MVP subset.
    Unsupported { construct: String, detail: String },
    /// Structurally invalid input — a program error under every
    /// backend (v1 either rejects it too or silently mis-evaluates
    /// it), never a TB-IR subset gap, so no `--codegen v1` suggestion.
    Invalid(String),
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LowerError::Unsupported { construct, detail } => {
                write!(f, "TB-IR lowering does not support {construct} yet")?;
                if !detail.is_empty() {
                    write!(f, " ({detail})")?;
                }
                write!(f, "; re-run with `--codegen v1`")
            }
            LowerError::Invalid(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LowerError {}

pub(crate) fn unsupported(construct: &str, detail: impl Into<String>) -> LowerError {
    LowerError::Unsupported {
        construct: construct.to_string(),
        detail: detail.into(),
    }
}

/// A folded file-scope constant: 64-bit two's-complement bit pattern
/// plus the signedness later expressions must evaluate it under. The
/// signedness is the *declared* type's (`sint<N>` → signed), matching
/// v1, which stores every ≤64-bit const as `uint64_t`/`int64_t` per
/// `c_type_for` — the bit pattern is backend-identical and signedness
/// only changes the value of `>>`, `/`, `%`, and ordered comparisons.
#[derive(Clone, Copy)]
struct ConstVal {
    bits: u64,
    signed: bool,
}

impl ConstVal {
    fn as_i64(self) -> i64 {
        self.bits as i64
    }
    fn is_negative(self) -> bool {
        self.signed && (self.bits as i64) < 0
    }
}

/// A const-initializer fold failure (issue #521): `Unsupported` is a
/// construct outside the constant-expression subset (surfaced as
/// `LowerError::Unsupported`, which suggests `--codegen v1`), while
/// `Invalid` is a well-formed constant expression whose evaluation is
/// illegal — division by zero, an out-of-range shift, an unknown or
/// cyclic constant reference, a value that violates the declared
/// width. Those are program errors under every backend (v1 would hit
/// C++ UB or silently mis-evaluate), so they surface as
/// `LowerError::Invalid` with a precise diagnostic instead.
enum ConstFoldErr {
    Unsupported(String),
    Invalid(String),
}

use ConstFoldErr::Invalid as FoldInvalid;

/// Const-fold a file-scope `const` initializer expression, resolving
/// identifiers against `consts` (earlier `const` values and enum-
/// variant indices, both in source order; `self_name` is the constant
/// being defined, for cycle diagnostics). Evaluation is 64-bit two's-
/// complement with per-node signedness — integer literals are 64-bit
/// (HARC semantics; unlike C++'s 32-bit `int` literals), references
/// carry their declared signedness, and a binary op is signed only
/// when both operands are (C++'s usual-arithmetic-conversion rule at
/// rank 64, which is what v1's emitted `constexpr` initializers
/// evaluate under). Supports: integer literals, booleans, identifiers
/// (const/enum names), parentheses, `as uint/sint/bits<≤64>` relabel
/// casts, unary `-`/`~`/`!`/`not`, and the binary arithmetic
/// (`+ - * / %`), shift (`<< >>`), bitwise (`& | ^`), comparison, and
/// logical operators. The wrapping `+% -% *%` spellings are rejected —
/// their §2.4 mask depends on static operand widths the fold does not
/// model.
fn fold_const(
    e: &crate::ast::Expr,
    consts: &HashMap<String, ConstVal>,
    self_name: &str,
) -> Result<ConstVal, ConstFoldErr> {
    use crate::ast::{BinaryOp, TypeExpr, UnaryOp};
    let boolean = |x: bool| {
        // Comparison / logical results promote like C++ `bool` → `int`.
        Ok(ConstVal {
            bits: x as u64,
            signed: true,
        })
    };
    match &*e.kind {
        ExprKind::Paren(inner) => fold_const(inner, consts, self_name),
        ExprKind::Int(s) => match exprs::parse_int_literal(s) {
            // Decimal literals above i64::MAX are unsigned, like C++.
            Some(v) => Ok(ConstVal {
                bits: v,
                signed: v <= i64::MAX as u64,
            }),
            None => Err(ConstFoldErr::Unsupported(format!(
                "the integer literal `{s}` does not fit the 64-bit constant-evaluation domain"
            ))),
        },
        ExprKind::Bool(b) => boolean(*b),
        ExprKind::Ident(id) => match consts.get(&id.name) {
            Some(v) => Ok(*v),
            None if id.name == self_name => Err(FoldInvalid(format!(
                "references itself — `const {self_name}` is a dependency cycle",
            ))),
            None => Err(FoldInvalid(format!(
                "references `{}`, which is not an earlier `const` or enum variant \
                 (constants resolve in declaration order, so forward or cyclic \
                 references cannot be evaluated)",
                id.name
            ))),
        },
        // `expr as uint<W>` / `as sint<W>` / `as bits<W>` (W ≤ 64) is a
        // signedness relabel with the value unchanged — the same
        // semantics the runtime lowering gives casts (`cast_relabel_width`)
        // and v1's `((uint64_t)(expr))` emission at 64-bit rank.
        ExprKind::Cast { expr, ty } => {
            // The runtime relabel helper accepts widths up to 128, but
            // the const fold's value domain is 64-bit — gate at 64 so
            // the accepted subset matches the documented one.
            if !exprs::cast_relabel_width(ty).is_some_and(|w| w <= 64) {
                return Err(ConstFoldErr::Unsupported(
                    "`as` casts outside scalar uint/sint/bits (≤ 64 bits) in a \
                     constant expression"
                        .into(),
                ));
            }
            let v = fold_const(expr, consts, self_name)?;
            let signed = matches!(
                ty,
                TypeExpr::Builtin {
                    name: crate::ast::BuiltinTy::SInt | crate::ast::BuiltinTy::SIntCap,
                    ..
                }
            );
            Ok(ConstVal {
                bits: v.bits,
                signed,
            })
        }
        ExprKind::Unary { op, expr } => {
            let v = fold_const(expr, consts, self_name)?;
            match op {
                UnaryOp::Neg => Ok(ConstVal {
                    bits: v.bits.wrapping_neg(),
                    // C++: negating an unsigned value stays unsigned.
                    signed: v.signed,
                }),
                UnaryOp::Not | UnaryOp::NotKw => boolean(v.bits == 0),
                UnaryOp::BitNot => Ok(ConstVal {
                    bits: !v.bits,
                    signed: v.signed,
                }),
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let a = fold_const(lhs, consts, self_name)?;
            let b = fold_const(rhs, consts, self_name)?;
            // C++ usual arithmetic conversions at rank 64: the result
            // (and the comparison/division domain) is signed only when
            // both operands are.
            let signed = a.signed && b.signed;
            let arith = |bits: u64| Ok(ConstVal { bits, signed });
            // Shift-amount validation shared by `<<`/`>>`. v1 forwards
            // the raw expression to C++, where an out-of-range amount
            // is UB — reject loudly instead of silently masking.
            let shift_amount = |b: ConstVal| -> Result<u32, ConstFoldErr> {
                if b.is_negative() {
                    return Err(FoldInvalid(format!(
                        "negative shift amount ({})",
                        b.as_i64()
                    )));
                }
                if b.bits >= 64 {
                    return Err(FoldInvalid(format!(
                        "shift amount {} is out of range (constant evaluation \
                         is 64-bit; shift amounts must be 0..=63)",
                        b.bits
                    )));
                }
                Ok(b.bits as u32)
            };
            match op {
                BinaryOp::Add => arith(a.bits.wrapping_add(b.bits)),
                BinaryOp::Sub => arith(a.bits.wrapping_sub(b.bits)),
                BinaryOp::Mul => arith(a.bits.wrapping_mul(b.bits)),
                // `+% -% *%` mask to max(W(a), W(b)) per spec §2.4, and
                // the 64-bit fold domain does not model per-operand
                // widths — folding them as plain ops would silently
                // change their meaning. This is `Invalid`, not
                // `Unsupported`: v1 emits wrap ops UNMASKED into the
                // constexpr initializer (`255 +% 1` at `uint<8>` gives
                // 256 under v1), so steering users to `--codegen v1`
                // would recommend a backend that silently mis-evaluates.
                BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap => {
                    Err(FoldInvalid(
                        "the wrapping `+% -% *%` operators are not evaluable in a \
                         constant initializer (their §2.4 mask needs static operand \
                         widths); use the plain operator and spell the intended \
                         wrapped value or mask explicitly"
                            .into(),
                    ))
                }
                BinaryOp::Div | BinaryOp::Mod => {
                    let is_div = matches!(op, BinaryOp::Div);
                    let label = if is_div { "division" } else { "modulo" };
                    if b.bits == 0 {
                        return Err(FoldInvalid(format!("{label} by zero")));
                    }
                    if signed {
                        if a.as_i64() == i64::MIN && b.as_i64() == -1 {
                            return Err(FoldInvalid(format!(
                                "signed {label} overflow ({} / -1 exceeds the \
                                 64-bit constant-evaluation domain)",
                                i64::MIN
                            )));
                        }
                        let r = if is_div {
                            a.as_i64().wrapping_div(b.as_i64())
                        } else {
                            a.as_i64().wrapping_rem(b.as_i64())
                        };
                        arith(r as u64)
                    } else if is_div {
                        arith(a.bits / b.bits)
                    } else {
                        arith(a.bits % b.bits)
                    }
                }
                BinaryOp::Shl => {
                    let n = shift_amount(b)?;
                    // C++: the result's signedness is the (promoted)
                    // left operand's, not the pair's.
                    Ok(ConstVal {
                        bits: a.bits << n,
                        signed: a.signed,
                    })
                }
                BinaryOp::Shr => {
                    let n = shift_amount(b)?;
                    let bits = if a.signed {
                        // Arithmetic shift for signed operands — what
                        // v1's `int64_t` constexpr evaluation does.
                        (a.as_i64() >> n) as u64
                    } else {
                        a.bits >> n
                    };
                    Ok(ConstVal {
                        bits,
                        signed: a.signed,
                    })
                }
                BinaryOp::BitAnd => arith(a.bits & b.bits),
                BinaryOp::BitOr => arith(a.bits | b.bits),
                BinaryOp::BitXor => arith(a.bits ^ b.bits),
                BinaryOp::Eq => boolean(a.bits == b.bits),
                BinaryOp::Ne => boolean(a.bits != b.bits),
                BinaryOp::Lt => boolean(if signed {
                    a.as_i64() < b.as_i64()
                } else {
                    a.bits < b.bits
                }),
                BinaryOp::Le => boolean(if signed {
                    a.as_i64() <= b.as_i64()
                } else {
                    a.bits <= b.bits
                }),
                BinaryOp::Gt => boolean(if signed {
                    a.as_i64() > b.as_i64()
                } else {
                    a.bits > b.bits
                }),
                BinaryOp::Ge => boolean(if signed {
                    a.as_i64() >= b.as_i64()
                } else {
                    a.bits >= b.bits
                }),
                BinaryOp::AndAnd | BinaryOp::AndKw => boolean(a.bits != 0 && b.bits != 0),
                BinaryOp::OrOr | BinaryOp::OrKw => boolean(a.bits != 0 || b.bits != 0),
                _ => Err(ConstFoldErr::Unsupported(format!(
                    "the `{op:?}` operator in a constant expression"
                ))),
            }
        }
        _ => Err(ConstFoldErr::Unsupported(
            "does not fold to a compile-time integer constant (only integer \
             literals, earlier `const`/enum-variant names, `as` relabel casts, \
             and the arithmetic/bitwise/shift operators are supported)"
            .into(),
        )),
    }
}

/// Check the folded value against the declared type and pin the
/// stored signedness to the declaration (issue #521 acceptance
/// criterion 2). Widths below 64 are *validated*, not truncated: v1
/// stores every ≤64-bit const in a 64-bit C type, so silently masking
/// here would diverge from the legacy backend, and silently keeping an
/// out-of-range value would make the declared width a lie. A value
/// that does not fit the declared `uint<W>` / `sint<W>` is a precise
/// compile-time error instead. Widths ≥ 64 (and missing/unknown
/// types) pass through unchecked — the 64-bit fold domain cannot
/// overflow them.
fn check_const_decl_type(
    ty: Option<&crate::ast::TypeExpr>,
    v: ConstVal,
) -> Result<ConstVal, String> {
    use crate::ast::{BuiltinTy, TypeArg, TypeExpr};
    let Some(TypeExpr::Builtin { name, args, .. }) = ty else {
        // Untyped `const NAME = expr` — v1 stores it as `int64_t`.
        return Ok(ConstVal {
            bits: v.bits,
            signed: ty.is_none() || v.signed,
        });
    };
    let width: Option<u32> = args.first().and_then(|a| match a {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    });
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
            if let Some(w) = width.filter(|w| (1..64).contains(w)) {
                if v.is_negative() {
                    return Err(format!(
                        "value {} does not fit `{}<{w}>` — negative values \
                         cannot initialize an unsigned constant (spell the \
                         intended {w}-bit value explicitly, e.g. as a hex \
                         literal)",
                        v.as_i64(),
                        type_keyword(name),
                    ));
                }
                if v.bits >> w != 0 {
                    return Err(format!(
                        "value {} does not fit `{}<{w}>` (max {})",
                        v.bits,
                        type_keyword(name),
                        (1u64 << w) - 1
                    ));
                }
            }
            Ok(ConstVal {
                bits: v.bits,
                signed: false,
            })
        }
        BuiltinTy::SInt | BuiltinTy::SIntCap => {
            if let Some(w) = width.filter(|w| (1..64).contains(w)) {
                // The bit pattern is reinterpreted as signed 64-bit two's
                // complement regardless of the expression's signedness —
                // the mod-2^64 conversion C++20 defines and v1 performs
                // (`sint<63> = 0xFFFF_FFFF_FFFF_FFFF` is -1 under v1, and
                // `sint<64>` already accepted the same pattern here).
                let s = v.bits as i64;
                let min = -(1i64 << (w - 1));
                let max = (1i64 << (w - 1)) - 1;
                if s < min || s > max {
                    return Err(format!(
                        "value {s} does not fit `sint<{w}>` (range {min}..={max})"
                    ));
                }
            }
            Ok(ConstVal {
                bits: v.bits,
                signed: true,
            })
        }
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Ok(ConstVal {
            // v1 emits `static constexpr bool` — C++ bool conversion.
            bits: (v.bits != 0) as u64,
            signed: true,
        }),
        _ => Ok(v),
    }
}

/// Surface keyword for an unsigned builtin scalar, for diagnostics.
fn type_keyword(name: &crate::ast::BuiltinTy) -> &'static str {
    use crate::ast::BuiltinTy;
    match name {
        BuiltinTy::UIntCap => "UInt",
        BuiltinTy::Bits => "bits",
        BuiltinTy::Int => "int",
        _ => "uint",
    }
}

/// Lower a merged source file (post `merge_for_sim`) into a verified-
/// shape `TbProgram`. Callers should run `verify::verify_program` on
/// the result before emission.
pub fn lower_program(file: &SourceFile) -> Result<TbProgram, LowerError> {
    // Capture impl-form testbench bindings BEFORE desugaring clears
    // `for_testbench`.
    let mut tb_of_test: HashMap<String, String> = HashMap::new();
    for it in &file.items {
        if let Item::Test(t) = it {
            if let Some(tb) = &t.for_testbench {
                tb_of_test.insert(t.name.name.clone(), tb.name.clone());
            }
        }
    }

    // Reuse v1's impl-for desugaring so both codegens see the exact
    // same classic-form AST (synthesized `let dut` / `let _tb`, merged
    // lifecycle blocks, `_tb.<field>` rewrites).
    let file = crate::codegen::cpp_tb::desugar_impl_for_test_in_file(file);

    // Domain table: `domain D freq_mhz: N` → period_ps = 1_000_000 / N.
    let mut domains: HashMap<String, i64> = HashMap::new();
    for it in &file.items {
        if let Item::Domain(d) = it {
            for f in &d.fields {
                if f.name.name == "freq_mhz" {
                    if let ExprKind::Int(s) = &*f.value.kind {
                        if let Ok(n) = s.replace('_', "").parse::<i64>() {
                            if n > 0 {
                                domains.insert(d.name.name.clone(), 1_000_000 / n);
                            }
                        }
                    }
                }
            }
        }
    }

    // Testbench components referenced by impl-form tests.
    let mut components: HashMap<String, &ComponentDecl> = HashMap::new();
    for it in &file.items {
        // Scoreboards are NOT in this map — they lower to their own
        // schema table (`scoreboard_ids`) and bind as testbench fields,
        // not as `impl ... for`-bound composite components.
        if let Item::Env(c) | Item::Agent(c) | Item::Sequencer(c) = it {
            components.insert(c.name.name.clone(), c);
        }
    }

    // Bus declarations (inline or `use`-imported — the import resolver
    // appended the parsed stdlib file before merge_for_sim, so both
    // arrive here as plain items). Declarations are inert until a test
    // binds them; unsupported bus features are rejected at the bind /
    // access site, not here.
    let mut buses: HashMap<String, &BusDecl> = HashMap::new();
    for it in &file.items {
        if let Item::Bus(b) = it {
            buses.insert(b.name.name.clone(), b);
        }
    }
    // Names requested via `use Name;` that never resolved to a live `bus`
    // declaration — either the search in `resolve_use_imports` (main.rs)
    // found no `Name.arch`/`Name.harc`, or `use` targeted something that
    // was never a bus. `use` can only ever bring in `Item::Bus` items (see
    // `resolve_use_imports`'s doc comment), so "not in `buses`" is a
    // reliable "this use never resolved" signal. Threaded into
    // `lower_test` so a downstream `let x: Name = bind ...` can name the
    // failed import directly instead of falling through to the generic
    // "let with a bind" rejection (see issue #493).
    let mut unresolved_use_names: HashSet<String> = HashSet::new();
    for it in &file.items {
        if let Item::Use(u) = it {
            if let Some(last) = u.path.segments.last() {
                if !buses.contains_key(&last.name) {
                    unresolved_use_names.insert(last.name.clone());
                }
            }
        }
    }
    let used_tbs: HashSet<&String> = tb_of_test.values().collect();

    // Pre-scan of test-scope bus bindings (`let <name> : <Bus> = bind
    // ...`) across every (desugared) test, keyed by binding name. A
    // bound-to target responder body may re-issue a downstream blocking
    // TLM call (`let raw = back.read(addr)` — nested forwarding) against
    // a bus binding that the test, not the transactor, declares. The
    // transactor is lowered before any test (so its responder bodies are
    // ready when a test binds the actor), so the downstream binding's
    // bus type is not yet in scope at responder-lowering time. This
    // pre-scan makes those bindings visible to the responder body so the
    // downstream call lowers to a `TransactorMethod` call edge (resolved
    // against the test's `bus_bindings` at emit), instead of falling
    // through to the generic transactor-method rejection.
    //
    // First binding name wins on a cross-test collision (the responder
    // body is shared, so its downstream bus type must be unambiguous; a
    // genuine type clash surfaces as a wire-resolution error at emit).
    let mut downstream_bus_binds: HashMap<String, BusDecl> = HashMap::new();
    for it in &file.items {
        let Item::Test(t) = it else { continue };
        for ti in &t.items {
            let TestItem::Let(l) = ti else { continue };
            if !l.bind {
                continue;
            }
            let Some(bus_name) = type_simple_name(l.ty.as_ref()) else {
                continue;
            };
            let Some(decl) = buses.get(bus_name) else {
                continue;
            };
            downstream_bus_binds
                .entry(l.name.name.clone())
                .or_insert_with(|| (*decl).clone());
        }
    }

    // File-scope named integer constants: `const NAME : Ty = <expr>`
    // (v1: `static constexpr <cty> NAME = <expr>;`) and `enum Color {
    // RED, ... }` variant names (v1: variant index, first definition
    // wins). Both substitute as plain integer literals at use sites —
    // observably identical to v1's constexpr/index emission. The
    // initializer expression is const-folded in the 64-bit two's-
    // complement domain with declared-type signedness (issue #521; see
    // `fold_const`), validated against the declared width, and may
    // reference earlier `const` names and enum-variant names (both
    // live in the same map, processed in source order). An initializer
    // outside the constant-expression subset is rejected structurally;
    // an illegal evaluation (division by zero, out-of-range shift,
    // unknown/cyclic reference, width violation) gets a precise
    // `Invalid` diagnostic.
    let mut const_vals: HashMap<String, ConstVal> = HashMap::new();
    for it in &file.items {
        match it {
            Item::Const(c) => {
                let folded = fold_const(&c.value, &const_vals, &c.name.name).and_then(|v| {
                    check_const_decl_type(c.ty.as_ref(), v).map_err(FoldInvalid)
                });
                let v = match folded {
                    Ok(v) => v,
                    Err(ConstFoldErr::Unsupported(detail)) => {
                        return Err(unsupported(
                            &format!("`const {}` initializer", c.name.name),
                            detail,
                        ));
                    }
                    Err(ConstFoldErr::Invalid(detail)) => {
                        return Err(LowerError::Invalid(format!(
                            "`const {}` initializer: {detail}",
                            c.name.name
                        )));
                    }
                };
                const_vals.insert(c.name.name.clone(), v);
            }
            Item::Enum(e) => {
                for (i, v) in e.variants.iter().enumerate() {
                    // First definition wins across enums — v1's
                    // `enum_variants.entry(..).or_insert(i)`. Variant
                    // indices are small non-negative values; v1 emits
                    // them as plain (signed) `int` literals.
                    const_vals.entry(v.name.clone()).or_insert(ConstVal {
                        bits: i as u64,
                        signed: true,
                    });
                }
            }
            _ => {}
        }
    }
    // The lowering contexts only need the substituted bit pattern —
    // use sites emit the 64-bit literal either way.
    let consts: HashMap<String, u64> = const_vals
        .iter()
        .map(|(k, v)| (k.clone(), v.bits))
        .collect();
    let const_signed: HashMap<String, bool> = const_vals
        .iter()
        .map(|(k, v)| (k.clone(), v.signed))
        .collect();
    // `extern function name(...) -> ret` (spec §9) names — calls to
    // these lower to `CallTarget::ExternFn`; the file-scope `extern "C"`
    // forward declarations are emitted by `emit_extern_fn_decls`. Shared
    // across every lowering context (an extern fn is a pure scalar C
    // call, callable wherever a pure helper is).
    let extern_fn_decls: HashMap<String, &crate::ast::ExternFnDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::ExternFn(f) => Some((f.name.name.clone(), f)),
            _ => None,
        })
        .collect();
    let extern_fns: HashSet<String> = extern_fn_decls.keys().cloned().collect();

    // Enum names, so transaction fields of enum type lower as scalars
    // (v1 flattens them to `int64_t` members with index values).
    let enum_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Enum(e) => Some(e.name.name.clone()),
            _ => None,
        })
        .collect();

    // Helper functions: categorize pure vs impure, reject recursion.
    // Covergroup schemas need this early so hook-triggered coverpoints can
    // sample pure helper calls over hook parameters.
    let helper_registry = helpers::HelperRegistry::build(&file)?;

    // Covergroup schemas, in file order. All declarations lower (even
    // unreferenced ones — v1 emits a struct for each), so unsupported
    // covergroup features are rejected here rather than dropped.
    let mut covgroup_ids: HashMap<String, CovgroupId> = HashMap::new();
    let mut covgroups: Vec<CovgroupSchema> = Vec::new();
    for it in &file.items {
        if let Item::Covergroup(g) = it {
            let schema =
                covergroups::lower_covergroup(g, &helper_registry, &extern_fn_decls, &consts)?;
            covgroup_ids.insert(g.name.name.clone(), CovgroupId(covgroups.len() as u32));
            covgroups.push(schema);
        }
    }
    // Record schemas (`transaction` declarations), in file order. All
    // declarations lower (even unreferenced ones — v1 emits a struct
    // for each), so unsupported transaction shapes are rejected here
    // rather than dropped.
    let mut record_ids: HashMap<String, RecordId> = HashMap::new();
    // PRE-SCAN: assign a `RecordId` to every transaction (file order) then
    // every struct (file order) BEFORE lowering any field body. A struct
    // field may name another struct declared later (forward reference) or a
    // transaction, so the full name→id map must exist before `field_ir_type`
    // resolves a nested-record field type. Ids are stable: the second pass
    // pushes schemas in exactly this order, so `RecordId(k)` indexes
    // `record_schemas[k]`. A struct sharing a name with a transaction (or
    // another struct) resolves ambiguously, so reject the collision.
    let mut record_order: Vec<&Item> = Vec::new();
    for it in &file.items {
        if let Item::Transaction(t) = it {
            record_ids.insert(t.name.name.clone(), RecordId(record_order.len() as u32));
            record_order.push(it);
        }
    }
    for it in &file.items {
        if let Item::Struct(s) = it {
            let name = &s.name.name;
            if record_ids.contains_key(name) {
                return Err(LowerError::Invalid(format!(
                    "struct `{name}` collides with a transaction or struct of the same name"
                )));
            }
            record_ids.insert(name.clone(), RecordId(record_order.len() as u32));
            record_order.push(it);
        }
    }
    // Second pass: lower each transaction/struct body's fields with the full
    // `record_ids` map in scope, so a nested-record field type resolves to
    // `IrType::Record(rid)` (native nested records — v1 parity). A struct is
    // the shared value-record shape (v1's `emit_struct_record` routes through
    // `emit_record_struct`, exactly as transactions do), so a `let r : S`
    // resolves `S` via `record_ids` and every record-local op (`RecordInit`
    // / `RecordFieldWrite` / `Expr::RecordField`) works for free.
    let mut record_schemas: Vec<RecordSchema> = Vec::new();
    for it in record_order {
        let schema = match it {
            Item::Transaction(t) => records::lower_transaction(t, &enum_names, &record_ids)?,
            Item::Struct(s) => records::lower_struct(s, &enum_names, &record_ids)?,
            _ => unreachable!("record_order holds only transactions and structs"),
        };
        record_schemas.push(schema);
    }
    // Reject recursive / mutually-recursive nested records: an outer struct
    // that transitively contains itself would emit an infinite C++ struct.
    records::check_no_record_cycles(&record_schemas)?;
    // Regblock schemas (`regblock` declarations), in file order. The
    // mirror is a synthetic value-record (one scalar field per
    // register), pushed into the records table right after the
    // transactions so its `RecordId` is stable; the `RegblockSchema`
    // carries the offset/width/access metadata access lowering needs.
    // The regblock name doubles as the mirror record's name, so a
    // `let regs : R` resolves `R` to the synthetic record via
    // `record_ids` exactly like a transaction local.
    let mut regblock_ids: HashMap<String, RegblockId> = HashMap::new();
    let mut regblock_schemas: Vec<ir::RegblockSchema> = Vec::new();
    for it in &file.items {
        if let Item::Regblock(r) = it {
            let name = &r.name.name;
            if record_ids.contains_key(name) {
                return Err(LowerError::Invalid(format!(
                    "regblock `{name}` collides with a transaction or struct of the same name"
                )));
            }
            let rec_id = RecordId(record_schemas.len() as u32);
            let (rec, schema) = regblock::lower_regblock(r, rec_id)?;
            record_ids.insert(name.clone(), rec_id);
            record_schemas.push(rec);
            regblock_ids.insert(name.clone(), RegblockId(regblock_schemas.len() as u32));
            regblock_schemas.push(schema);
        }
    }

    // Addrmap declarations (`addrmap A via H { instance ... }`), in file
    // order. Resolved per-binding at the `let chip : A = bind helper`
    // site (each instance becomes its own shifted-offset mirror local).
    // No `TbProgram`-level schema: the binding context carries everything
    // access lowering and the Run-function mirror inits need, and dump-ir
    // surfaces it through the regblock binding list. See
    // `src/ir/lower/addrmap.rs`.
    let mut addrmap_decls: HashMap<String, &AddrmapDecl> = HashMap::new();
    for it in &file.items {
        if let Item::Addrmap(a) = it {
            let name = &a.name.name;
            if regblock_ids.contains_key(name) || record_ids.contains_key(name) {
                return Err(LowerError::Invalid(format!(
                    "addrmap `{name}` collides with a regblock, transaction, or struct of \
                     the same name"
                )));
            }
            if addrmap_decls.insert(name.clone(), a).is_some() {
                return Err(LowerError::Invalid(format!(
                    "addrmap `{name}` is declared more than once"
                )));
            }
        }
    }

    // `tseq` (transaction-sequence) declarations: name → element record
    // type. Validated up front (the element type must be a declared
    // record); the bodies lower to `FunctionKind::Tseq` functions after
    // the pure helpers (so FunctionIds stay sequential). Threaded into
    // every `LowerCtx` so a `let txns = Name(...)` resolves the call edge
    // and a `for t in txns` resolves the iteration.
    let tseq_records = tseqs::collect_tseq_records(&file, &record_ids)?;

    // Type names referenced as a by-value sub-component FIELD of some
    // `env`/`agent` declaration. A purely-structural DUT-poking BFM
    // transactor routes to the composite-component table ONLY when it
    // appears here (an env must hold it by value); standalone it stays a
    // `TransactorSchema`. (Other component-routed transactor shapes —
    // event-driven / reactive-monitor — route regardless of placement, so
    // this set only changes the BFM trailing arm of
    // `transactor_is_component`.) Computed once and reused at every
    // `transactor_is_component(t, env_held)` site below.
    let env_held_type_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            // NB: a `testbench` parses as `Item::Env` with
            // `ComponentKind::Testbench` — exclude it. A testbench FIELD
            // (`xt : Xt active`) is a top-level instance binding, NOT an
            // env holding the transactor by value; the BFM there stays a
            // `TransactorSchema`. Only true `env`/`agent` sub-fields force
            // the component routing.
            Item::Env(c)
                if matches!(
                    c.kind,
                    crate::ast::ComponentKind::Env | crate::ast::ComponentKind::Agent
                ) =>
            {
                Some(c)
            }
            Item::Agent(c) => Some(c),
            _ => None,
        })
        .flat_map(|c| c.items.iter())
        .filter_map(|ci| match ci {
            crate::ast::ComponentItem::Field(f) => match &f.ty {
                TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let env_held = |t: &crate::ast::TransactorDecl| env_held_type_names.contains(&t.name.name);

    // Transactor names, for the file gate + testbench-field validation
    // (schemas lower after pure helpers so FunctionIds line up).
    let mut transactor_ids: HashMap<String, TransactorId> = HashMap::new();
    let mut n_transactors = 0u32;
    for it in &file.items {
        if let Item::Transactor(t) = it {
            // A pure analysis-source transactor (event port + no DUT
            // field) routes to the composite-component table instead of
            // the DUT-poking `TransactorSchema` (classified below).
            if components::transactor_is_component(t, env_held(t)) {
                continue;
            }
            if transactor_ids
                .insert(t.name.name.clone(), TransactorId(n_transactors))
                .is_some()
            {
                return Err(LowerError::Invalid(format!(
                    "duplicate transactor declaration `{}`",
                    t.name.name
                )));
            }
            n_transactors += 1;
        }
    }

    // Scoreboard schemas (`scoreboard` declarations), in file order. All
    // declarations lower (even unreferenced ones — v1 emits a struct for
    // each), so unsupported scoreboard shapes are rejected here rather
    // than dropped. The ids feed the testbench-field validation below.
    let mut scoreboard_ids: HashMap<String, ScoreboardId> = HashMap::new();
    let mut scoreboard_schemas: Vec<ScoreboardSchema> = Vec::new();
    for it in &file.items {
        if let Item::Scoreboard(c) = it {
            // A method-bearing scoreboard needs per-instance state, so it
            // routes to the composite-component table instead of the
            // data-only `ScoreboardSchema` (classified below).
            if components::scoreboard_is_component(c) {
                continue;
            }
            let schema = scoreboards::lower_scoreboard(c, &record_ids)?;
            if scoreboard_ids
                .insert(
                    c.name.name.clone(),
                    ScoreboardId(scoreboard_schemas.len() as u32),
                )
                .is_some()
            {
                return Err(LowerError::Invalid(format!(
                    "duplicate scoreboard declaration `{}`",
                    c.name.name
                )));
            }
            scoreboard_schemas.push(schema);
        }
    }

    // The set of every composite-component type that lowers to a
    // `ComponentSchema` (env, method-bearing scoreboard, analysis-source
    // transactor, agent). A field of one of these types is a
    // composite-component binding — valid both as a test-scope `let` and
    // (since the testbench-field-binding slice) as a `testbench` field.
    // Must stay in lockstep with the `comp_sources` classification below.
    let component_type_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Scoreboard(c) if components::scoreboard_is_component(c) => {
                Some(c.name.name.clone())
            }
            Item::Transactor(t) if components::transactor_is_component(t, env_held(t)) => {
                Some(t.name.name.clone())
            }
            Item::Env(c) if matches!(c.kind, crate::ast::ComponentKind::Env) => {
                Some(c.name.name.clone())
            }
            Item::Agent(c) if matches!(c.kind, crate::ast::ComponentKind::Agent) => {
                Some(c.name.name.clone())
            }
            Item::Sequencer(c) => Some(c.name.name.clone()),
            _ => None,
        })
        .collect();

    // The subset of composite-component types that are event-driven
    // *transactors* (`in event<T>` + `on <ev>` consumer BFM). They route
    // to a `ComponentSchema` but, being transactors, accept an
    // `active`/`passive` instance mode at a binding site (the mode just
    // selects whether the `when active` body is included — always, in
    // this subset, so `active` is required and `passive` rejected).
    let event_driven_transactor_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) if components::transactor_is_event_driven(t) => {
                Some(t.name.name.clone())
            }
            _ => None,
        })
        .collect();
    // Reactive monitor / checker transactors (cycle-trigger / periodic
    // `on` handlers, no `in event` consumer pipe). These route to the
    // composite-component table AND accept a `passive` instance mode (the
    // observation half is always-on, with no `when active` registration to
    // suppress). A subset of `event_driven_transactor_names`.
    let reactive_monitor_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) if components::transactor_is_reactive_monitor(t) => {
                Some(t.name.name.clone())
            }
            _ => None,
        })
        .collect();
    // DUT-poking hookable BFM transactors (hookable methods + a DUT
    // handle, no `on`/event/bound). They route to a `ComponentSchema`
    // (so an `env` can hold one by value as a sub-component) but remain
    // transactors at a binding site: their methods live under `when
    // active`, so an instance requires an explicit `active` mode (same as
    // `event_driven_transactor_names`; a `passive` instance has no
    // methods). A disjoint subset of `component_type_names`.
    let dut_poking_bfm_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) if components::transactor_is_dut_poking_bfm(t, env_held(t)) => {
                Some(t.name.name.clone())
            }
            _ => None,
        })
        .collect();

    // Function-library transactors (pure methods, no DUT/event/`on`). They
    // route to a `ComponentSchema` and, being transactors, tolerate an
    // `active`/`passive` mode at a binding site — the mode is inert (a
    // function library has no `when active` registration to gate), so any
    // mode (or none) is accepted, exactly as a reactive monitor is.
    let function_library_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) if components::transactor_is_function_library(t) => {
                Some(t.name.name.clone())
            }
            _ => None,
        })
        .collect();
    // DUT-attached passive helper / monitor transactors. Their hookables
    // live in the always-on body rather than `when active`, so `passive`
    // instances keep the same callable monitor surface.
    let passive_helper_names: HashSet<String> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Transactor(t) if components::transactor_is_passive_helper(t) => {
                Some(t.name.name.clone())
            }
            _ => None,
        })
        .collect();

    // File-level construct gate: anything outside the MVP subset is an
    // explicit Unsupported, never silently dropped.
    for it in &file.items {
        match it {
            Item::Use(_)
            | Item::Domain(_)
            | Item::Test(_)
            | Item::Covergroup(_)
            | Item::Function(_)
            | Item::Transaction(_)
            | Item::Const(_)
            | Item::Enum(_)
            // Struct declarations already lowered to record schemas above
            // (with their own Unsupported rejections); inert here.
            | Item::Struct(_)
            | Item::Bus(_)
            // Free-standing `relation` declarations (spec §4.2) are
            // reusable constraint sets. They are inert at this gate: a
            // relation only contributes constraints when a
            // `randomize(t) with R(t)` call inlines its body, and that
            // inlining happens entirely in the typed constraint backend
            // (`constraints::typed_lower` expands relation calls — block
            // and alias forms, recursively — before Z3). The TB-IR
            // randomize lowering keys off the solver problem table
            // (`randomize_problem_ids`), which is built from the same
            // expanded constraints v1 sees, so the relation declaration
            // itself needs no IR shape — only acceptance here.
            | Item::Relation(_)
            // SVA-style `property NAME ... end property` declarations
            // (spec). A property is inert until referenced by an
            // `assert property NAME` / `cover property NAME` site; v1
            // builds a name→body table up front and only emits an
            // executable concurrent check at the reference site. The
            // TB-IR test-body lowering rejects `assert property` and
            // named-property `assert` (see `stmts.rs`), so an accepted
            // property declaration can only be observed via a reference
            // that is itself still rejected — i.e. an unreferenced
            // property is observably inert under both codegens. Accept
            // the declaration here; the reference gate stays closed.
            | Item::Property(_)
            // `extern function name(...) -> ret` (spec §9) — a C
            // reference model linked via `--ref-src`. Inert at this
            // gate: the file-scope `extern "C"` forward declaration is
            // emitted by `emit_extern_fn_decls`, and call sites resolve
            // through `extern_fns` to a `CallTarget::ExternFn` (raw
            // symbol name). The declaration itself carries no IR shape.
            | Item::ExternFn(_)
            | Item::Transactor(_)
            // Scoreboard declarations already lowered to schemas above
            // (with their own Unsupported rejections); they are inert
            // until a testbench binds one as a field.
            | Item::Scoreboard(_)
            | Item::Regblock(_)
            // Addrmap declarations are collected into `addrmap_decls`
            // above and resolved per `let chip : A = bind helper`
            // binding (each instance becomes its own shifted-offset
            // mirror local); inert at this gate.
            | Item::Addrmap(_) => {}
            // `extend` was already folded by merge_for_sim; a survivor
            // means the merge didn't apply (e.g. dump-ir on a lone
            // extension file).
            Item::Extend(_) => {
                return Err(unsupported(
                    "an unmerged `extend` block",
                    "pass the base test file alongside the extension",
                ));
            }
            // A `sequencer` is a composite component (analysis-source
            // shape: `out event<T>` + hookable `emit` methods). It is
            // lowered through the component-cluster path below and binds
            // only as a test-scope component or an env/agent sub-component
            // (a sequencer testbench field is rejected via
            // `component_type_names`); inert at this gate.
            Item::Sequencer(_) => {}
            // `tseq` declarations are validated by `collect_tseq_records`
            // (element type must be a declared record) and lowered to
            // `FunctionKind::Tseq` functions below — inert at this gate.
            Item::Tseq(_) => {}
            Item::Env(c) | Item::Agent(c) => {
                if used_tbs.contains(&c.name.name) {
                    validate_testbench_component(
                        c,
                        &components,
                        &covgroup_ids,
                        &record_ids,
                        &transactor_ids,
                        &scoreboard_ids,
                        &component_type_names,
                        &event_driven_transactor_names,
                        &reactive_monitor_names,
                        &dut_poking_bfm_names,
                        &function_library_names,
                        &passive_helper_names,
                    )?;
                } else if matches!(
                    c.kind,
                    crate::ast::ComponentKind::Env | crate::ast::ComponentKind::Agent
                ) {
                    // A composite `env`/`agent` used as a test-scope
                    // component (`let env : AnalysisEnv` / `let tagger :
                    // Tagger`) is lowered through the component-cluster
                    // path below — inert at this gate (its items are
                    // validated by the component schema builder).
                } else {
                    return Err(unsupported(
                        &format!("env/component `{}`", c.name.name),
                        "only testbench components bound via `impl ... for` are lowered",
                    ));
                }
            }
            other => {
                return Err(unsupported(
                    &format!("the `{}` construct", item_label(other)),
                    "",
                ));
            }
        }
    }

    let mut prog = TbProgram {
        covgroups,
        records: record_schemas,
        scoreboards: scoreboard_schemas,
        regblocks: regblock_schemas,
        ..TbProgram::default()
    };

    // Program-wide constraint-site accumulator. Shared by reference
    // across every function lowered below so a `Terminator::Randomize`'s
    // `ConstraintRef` is a globally-unique index into one table. Drained
    // into `prog.constraint_sites` once all functions are lowered.
    let constraint_sites: RefCell<Vec<ConstraintSite>> = RefCell::new(Vec::new());
    // Typed solver problem table (constraint-IR layer) — the source of
    // the per-site `problem_id` handle. Built from the SAME desugared
    // `file` v1 uses (`cpp_tb` desugars, then builds the table), so the
    // randomize-target spans this table is keyed by match the spans the
    // lowering sees. Drives `ConstraintSite::problem_id`.
    let solver_table = crate::solver::problem_table::build_typed_solver_problem_table(&file);

    // Randomize-target span → problem-id, keyed exactly like v1's
    // `runtime_randomize_problem_ids` (only Z3-ready sites populate).
    let mut randomize_problem_ids: HashMap<(usize, usize), u32> = HashMap::new();
    for entry in &solver_table.entries {
        let crate::solver::problem_table::TypedSolverProblemSource::RandomizeSite { span, .. } =
            entry.source
        else {
            continue;
        };
        if let crate::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. } =
            &entry.build
        {
            randomize_problem_ids.insert((span.start, span.end), typed.problem_id.0);
        }
    }

    // Transaction-level `keep` clauses as AST exprs, by transaction
    // name. Spec §4: these are part of every `randomize(t)` of that
    // type, merged ahead of any call-site `with {...}` body (v1's
    // `txn_keeps` merge in `StmtKind::Randomize`).
    let mut txn_keeps: HashMap<String, Vec<crate::ast::Expr>> = HashMap::new();
    for it in &file.items {
        if let Item::Transaction(t) = it {
            let keeps: Vec<crate::ast::Expr> = t
                .body
                .iter()
                .filter_map(|item| match item {
                    crate::ast::TxnBodyItem::Keep(k) => Some(k.expr.clone()),
                    _ => None,
                })
                .collect();
            if !keeps.is_empty() {
                txn_keeps.insert(t.name.name.clone(), keeps);
            }
        }
    }

    // Eagerly lower pure helpers (declaration order) so call sites can
    // stay `ir::Expr::Call` and backends emit them as plain C++ functions.
    // Records are visible (for precise rejection messages), but pure
    // helpers cannot hold record locals — see `lower_let`.
    let helper_ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        allow_scheduler_time_waits: false,
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        // Deliberately empty: bus bindings and transactor fields are
        // test-scope, so a pure helper body can never resolve one —
        // which structurally enforces the design seam rule that
        // `TransactorMethod` call edges never appear in pure-helper
        // bodies.
        bus_bindings: HashMap::new(),
        bus_remaps: HashMap::new(),
        transactor_fields: HashMap::new(),
        passive_transactor_fields: HashSet::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: consts.clone(),
        const_signed: const_signed.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        // Pure helpers cannot hold record locals, so `randomize` can
        // never fire in one — these maps stay inert here.
        txn_keeps: HashMap::new(),
        randomize_problem_ids: HashMap::new(),
        tseqs: HashMap::new(),
        // Pure helpers never access the DUT (probes are test-scope only).
        probes: HashMap::new(),
        // Extern fns are pure scalar C calls — callable from a pure
        // helper body.
        extern_fns: extern_fns.clone(),
    };
    for it in &file.items {
        let Item::Function(fd) = it else { continue };
        let Some(entry) = helper_registry.get(&fd.name.name) else {
            continue;
        };
        // On duplicate names the registry keeps the last declaration;
        // only lower that one (single definition per name downstream).
        if !entry.pure || !std::ptr::eq(entry.decl, fd) {
            continue;
        }
        let id = FunctionId(prog.functions.len() as u32);
        let f =
            helpers::lower_pure_helper(id, fd, &helper_registry, &helper_ctx, &constraint_sites)?;
        prog.functions.push(f);
    }

    // `tseq` bodies, in file order: one `FunctionKind::Tseq` function
    // each, recorded as `name → FunctionId` so the test body can resolve
    // a `let txns = Name(...)` call edge. A tseq body holds record locals
    // and `randomize`, so its ctx carries the records / keep / problem-id
    // / tseq tables — but no test-scope bindings (a tseq cannot poke the
    // DUT, a bus, or a transactor field; those are test-scope only).
    let tseq_ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        allow_scheduler_time_waits: false,
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        bus_bindings: HashMap::new(),
        bus_remaps: HashMap::new(),
        transactor_fields: HashMap::new(),
        passive_transactor_fields: HashSet::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        scoreboards: Vec::new(),
        consts: consts.clone(),
        const_signed: const_signed.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: Vec::new(),
        component_fields: HashMap::new(),
        txn_keeps: txn_keeps.clone(),
        randomize_problem_ids: randomize_problem_ids.clone(),
        tseqs: tseq_records.clone(),
        // tseq generator bodies never access the DUT.
        probes: HashMap::new(),
        extern_fns: extern_fns.clone(),
    };
    for it in &file.items {
        let Item::Tseq(decl) = it else { continue };
        let elem = tseq_records[&decl.name.name].clone();
        let id = FunctionId(prog.functions.len() as u32);
        let f = tseqs::lower_tseq(
            id,
            decl,
            elem,
            &tseq_ctx,
            &helper_registry,
            &constraint_sites,
        )?;
        prog.functions.push(f);
    }

    // Transactor declarations, in file order: one schema each plus one
    // `TbFunction` (kind `TransactorBody`) per method. All declarations
    // lower (even unreferenced ones), so unsupported transactor shapes
    // are rejected here rather than dropped.
    for it in &file.items {
        let Item::Transactor(t) = it else { continue };
        if components::transactor_is_component(t, env_held(t)) {
            continue;
        }
        let id = TransactorId(prog.transactors.len() as u32);
        debug_assert_eq!(Some(&id), transactor_ids.get(&t.name.name));
        let (schema, funcs) = transactors::lower_transactor(
            id,
            t,
            FunctionId(prog.functions.len() as u32),
            &helper_registry,
            &helper_ctx,
            &buses,
            &downstream_bus_binds,
            &constraint_sites,
        )?;
        prog.transactors.push(schema);
        prog.functions.extend(funcs);
    }

    // Composite-component declarations (env/agent cluster, flat-struct
    // subset): method-bearing scoreboards, analysis-source transactors,
    // and the `env`s that compose them. Classified in file order; an
    // `env`'s sub-component fields and `connect` edges resolve against
    // the component ids assigned here.
    //
    // Two passes, like transactors: (1) build schemas (fields + method
    // signatures, reserving FunctionIds), (2) lower method bodies with a
    // ctx that knows the component table. `agent`/`sequencer` decls are
    // rejected precisely (they need the event-handler / sequencer slices,
    // out of this subset).
    let mut comp_sources: Vec<components::CompSource<'_>> = Vec::new();
    let mut component_ids: HashMap<String, ir::ComponentId> = HashMap::new();
    for it in &file.items {
        let (name, src) = match it {
            Item::Scoreboard(c) if components::scoreboard_is_component(c) => {
                (&c.name.name, components::CompSource::Scoreboard(c))
            }
            Item::Transactor(t) if components::transactor_is_component(t, env_held(t)) => {
                (&t.name.name, components::CompSource::Transactor(t))
            }
            Item::Env(c) if matches!(c.kind, crate::ast::ComponentKind::Env) => {
                (&c.name.name, components::CompSource::Env(c))
            }
            Item::Agent(c) if matches!(c.kind, crate::ast::ComponentKind::Agent) => {
                (&c.name.name, components::CompSource::Agent(c))
            }
            Item::Sequencer(c) => (&c.name.name, components::CompSource::Sequencer(c)),
            _ => continue,
        };
        let cid = ir::ComponentId(comp_sources.len() as u32);
        if component_ids.insert(name.clone(), cid).is_some() {
            return Err(LowerError::Invalid(format!(
                "duplicate component declaration `{name}`"
            )));
        }
        comp_sources.push(src);
    }
    // Pass 1: schemas. FunctionIds count up from the current function
    // count (after pure helpers + transactor methods).
    let mut next_fn = prog.functions.len() as u32;
    for src in &comp_sources {
        let schema = components::lower_component_schema(
            src,
            &component_ids,
            &scoreboard_ids,
            &record_ids,
            &mut next_fn,
        )?;
        prog.components.push(schema);
    }
    // Pass 1b: resolve `connect` edges (env + agent components — both carry
    // a `connect` block wiring their sub-components), now that every
    // component schema (fields + methods) exists. An agent's
    // `sequencer.dispatched -> drv.req` bridge is the canonical case.
    let comp_snapshot = prog.components.clone();
    for (i, src) in comp_sources.iter().enumerate() {
        if let components::CompSource::Env(decl) | components::CompSource::Agent(decl) = src {
            let connects = components::resolve_connects(decl, &comp_snapshot[i], &comp_snapshot)?;
            prog.components[i].connects = connects;
        }
    }
    // Pass 2: method bodies. A method ctx knows the component table (for
    // self-relative field access + sub-component method resolution) but
    // no test-scope fields. Bodies are lowered into placeholder
    // FunctionIds reserved in pass 1, then sorted into the functions
    // table (their ids are already contiguous from `start_fn`).
    let start_fn = prog.functions.len();
    let method_ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: None,
        cov_fields: HashMap::new(),
        covgroups: Vec::new(),
        clock_names: Vec::new(),
        allow_scheduler_time_waits: true,
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        bus_bindings: HashMap::new(),
        bus_remaps: HashMap::new(),
        transactor_fields: HashMap::new(),
        passive_transactor_fields: HashSet::new(),
        transactors: Vec::new(),
        scoreboard_fields: HashMap::new(),
        // A transactor body that pokes its own sub-scoreboard (`sb.writes =
        // ...` inside a cycle-trigger / on-handler) validates the scalar
        // field against the scoreboard schema, so the table must be visible
        // here even though method bodies are not bound at testbench scope.
        scoreboards: prog.scoreboards.clone(),
        consts: consts.clone(),
        const_signed: const_signed.clone(),
        tb_scalar_fields: HashSet::new(),
        tb_record_fields: Vec::new(),
        regblock_callbacks: HashMap::new(),
        tb_methods: HashMap::new(),
        test_scope_lets: HashSet::new(),
        regblock_bindings: HashMap::new(),
        regblock_init_order: Vec::new(),
        addrmap_bindings: HashMap::new(),
        addrmap_init_order: Vec::new(),
        bare_transactor_fields: HashSet::new(),
        target_state: HashMap::new(),
        components: prog.components.clone(),
        component_fields: HashMap::new(),
        // Component method bodies are not cataloged in the constraint-IR
        // problem table; a `randomize` inside one lowers with no
        // problem-id (v1's nullptr-descriptor fallback).
        txn_keeps: HashMap::new(),
        randomize_problem_ids: HashMap::new(),
        // Component methods cannot call a tseq generator (test-scope only).
        tseqs: HashMap::new(),
        // Component/transactor method bodies access the bound DUT but
        // never test-scope probes (probes live on `let dut`).
        probes: HashMap::new(),
        extern_fns: extern_fns.clone(),
    };
    let mut method_funcs: Vec<TbFunction> = Vec::new();
    for (i, src) in comp_sources.iter().enumerate() {
        let cid = ir::ComponentId(i as u32);
        let schema = prog.components[i].clone();
        // A bound-bus event-driven transactor's `on <ev>` handler bodies
        // drive the bound bus's handshake channels (`bus.<ch>.send/recv`,
        // `bus.<ch>.<sig>`). They lower exactly like the bound-initiator
        // BFM: the bound `BusDecl` is visible under the placeholder prefix
        // (`transactors::INITIATOR_BUS_PLACEHOLDER`), filled with the real
        // binding name at test-binding time. Inject a per-component ctx
        // carrying that binding; everything else mirrors `method_ctx`.
        let bound_ctx;
        let body_ctx: &LowerCtx = if let Some(bus_name) = schema.bound_bus.as_deref() {
            let Some(bus) = buses.get(bus_name) else {
                return Err(LowerError::Invalid(format!(
                    "event-driven transactor `{}` is bound to `{bus_name}`, which is not a \
                     `bus` declaration",
                    schema.name
                )));
            };
            let mut bb = method_ctx.clone();
            bb.bus_bindings.insert(
                transactors::INITIATOR_BUS_PLACEHOLDER.to_string(),
                (*bus).clone(),
            );
            bound_ctx = bb;
            &bound_ctx
        } else {
            &method_ctx
        };
        let bodies = components::lower_component_bodies(
            src,
            cid,
            &schema,
            body_ctx,
            &helper_registry,
            &constraint_sites,
        )?;
        // Patch the schema's pass-1 clause placeholders with the
        // resolved period/max_idle expressions (they could only lower
        // once a body context existed).
        debug_assert_eq!(
            bodies.periodic_periods.len(),
            prog.components[i].periodic_handlers.len()
        );
        for (ph, period) in prog.components[i]
            .periodic_handlers
            .iter_mut()
            .zip(bodies.periodic_periods)
        {
            ph.period = period;
        }
        debug_assert_eq!(
            bodies.cycle_triggers.len(),
            prog.components[i].cycle_handlers.len()
        );
        for (ch, trigger) in prog.components[i]
            .cycle_handlers
            .iter_mut()
            .zip(bodies.cycle_triggers)
        {
            ch.trigger = trigger;
        }
        if let (Some(ws), Some((period, max_idle))) = (
            prog.components[i].watchdog.as_mut(),
            bodies.watchdog_clauses,
        ) {
            ws.period = period;
            ws.max_idle = max_idle;
        }
        method_funcs.extend(bodies.funcs);
    }
    // The reserved FunctionIds must be contiguous from `start_fn`.
    debug_assert_eq!(
        method_funcs.first().map(|f| f.id.0),
        if method_funcs.is_empty() {
            None
        } else {
            Some(start_fn as u32)
        }
    );
    let _ = start_fn;
    prog.functions.extend(method_funcs);

    for it in &file.items {
        let Item::Test(t) = it else { continue };
        lower_test(
            t,
            &tb_of_test,
            &components,
            &component_ids,
            &domains,
            &covgroup_ids,
            &record_ids,
            &regblock_ids,
            &addrmap_decls,
            &buses,
            &unresolved_use_names,
            &consts,
            &const_signed,
            &extern_fns,
            &helper_registry,
            &txn_keeps,
            &randomize_problem_ids,
            &tseq_records,
            &constraint_sites,
            &dut_poking_bfm_names,
            &mut prog,
        )?;
    }

    if prog.tests.is_empty() {
        return Err(LowerError::Invalid(
            "no `test` declaration found".to_string(),
        ));
    }
    prog.constraint_sites = constraint_sites.into_inner();
    reject_recursive_transactor_methods(&prog)?;
    // Resolve hook-triggered covergroups (`covergroup G @(drv.send(t) post)`)
    // now that transactor method tables exist; records the subscription
    // on the target method's `cov_hook_subs`.
    crate::ir::passes::covergroup_hooks::run(&mut prog)?;
    Ok(prog)
}

/// Reject recursive transactor-method call cycles.
///
/// A sibling call (`m()` inside another method of the same DUT-poking
/// transactor) lowers to a `Stmt::TransactorSelfCall` and emits as a
/// *synchronous* `<Transactor>_<method>(args)` invocation inside the
/// enclosing method's `std::function` lambda (see `codegen::tbir::func`,
/// "v1 hookables run synchronously — their waits advance the clock
/// directly instead of yielding to the scheduler"). A direct (`a -> a`)
/// or mutual (`a -> b -> a`) cycle therefore has no base case: each call
/// pushes a new C++ stack frame, so the program recurses one frame per
/// simulated cycle until the stack overflows — a runtime segfault with
/// no diagnostic.
///
/// The legacy v1 emitter declares method lambdas with `auto`, so a
/// self-reference fails to compile at the C++ level (a name used in its
/// own initializer has an incomplete type). The TB-IR emitter
/// predeclares each method as a `std::function` slot before assigning
/// the lambda (so forward sibling references compile) — which also lets
/// a recursive cycle compile cleanly and then crash. Catch the cycle
/// here, at lowering, mirroring the phase-call recursion guard in
/// `expand_phase_calls`.
fn reject_recursive_transactor_methods(prog: &TbProgram) -> Result<(), LowerError> {
    for x in &prog.transactors {
        let methods: Vec<(&str, FunctionId)> = x
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m.function))
            .collect();
        reject_recursive_method_cycle_set(prog, &x.name, &methods)?;
    }
    // Env-held / function-library transactors route through the component
    // path, where sibling method calls appear as `ComponentCall {
    // base: SelfField, .. }`. Walk those method sets too, or a recursive
    // `env.drv.ping()` chain slips past the guard and recreates the
    // original stack-overflow-at-runtime bug.
    for c in &prog.components {
        let methods: Vec<(&str, FunctionId)> = c
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m.function))
            .collect();
        if methods
            .iter()
            .any(|(_, f)| !sibling_callees(prog.function(*f)).is_empty())
        {
            reject_recursive_method_cycle_set(prog, &c.name, &methods)?;
        }
    }
    Ok(())
}

fn reject_recursive_method_cycle_set(
    prog: &TbProgram,
    owner_name: &str,
    methods: &[(&str, FunctionId)],
) -> Result<(), LowerError> {
    let names: Vec<&str> = methods.iter().map(|(name, _)| *name).collect();
    // Adjacency by method index: method -> sibling methods it calls.
    // Unknown callees (already rejected upstream by
    // `verify::check_transactor_self_call`) are skipped here.
    let adj: Vec<Vec<usize>> = methods
        .iter()
        .map(|(_, f)| {
            sibling_callees(prog.function(*f))
                .iter()
                .filter_map(|callee| names.iter().position(|&n| n == callee))
                .collect()
        })
        .collect();
    // DFS with white/gray/black coloring; a gray back-edge is a cycle.
    let mut color = vec![0u8; names.len()];
    let mut path: Vec<usize> = Vec::new();
    for start in 0..names.len() {
        if color[start] != 0 {
            continue;
        }
        if let Some(cycle) = find_method_cycle(start, &adj, &mut color, &mut path) {
            let rendered = cycle
                .iter()
                .map(|&i| names[i])
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(LowerError::Invalid(format!(
                "transactor `{owner_name}` has a recursive method-call cycle: {rendered}; \
                 transactor methods lower to synchronous calls and cannot recurse \
                 (it would overflow the C++ stack at runtime)"
            )));
        }
    }
    Ok(())
}

/// Sibling-method names called from a method body. DUT-poking transactor
/// methods use `Stmt::TransactorSelfCall`; component-path transactors use
/// `Stmt::ComponentCall { base: SelfField, .. }`. Both are synchronous
/// sibling-call edges, so scanning block statements is sufficient — no
/// nested-expression walk is needed.
fn sibling_callees(func: &TbFunction) -> Vec<String> {
    let mut out = Vec::new();
    for b in &func.blocks {
        for s in &b.stmts {
            match s {
                ir::Stmt::TransactorSelfCall { call, .. } => {
                    if let ir::Expr::Call(ir::CallTarget::TransactorSelfMethod { method, .. }, _) =
                        call
                    {
                        out.push(method.clone());
                    }
                }
                ir::Stmt::ComponentCall {
                    base: ir::ComponentBase::SelfField,
                    method,
                    ..
                } => out.push(method.clone()),
                _ => {}
            }
        }
    }
    out
}

/// DFS back-edge search. On a cycle, returns the node indices on the
/// cycle with the closing node repeated at the end (e.g. `[a, b, a]`,
/// or `[a, a]` for a direct self-call), so the caller can render the
/// loop. Method counts per transactor are tiny, so recursion is fine.
fn find_method_cycle(
    node: usize,
    adj: &[Vec<usize>],
    color: &mut [u8],
    path: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    color[node] = 1; // gray (on the current DFS path)
    path.push(node);
    for &next in &adj[node] {
        if color[next] == 1 {
            let pos = path.iter().position(|&p| p == next).unwrap();
            let mut cyc = path[pos..].to_vec();
            cyc.push(next);
            return Some(cyc);
        }
        if color[next] == 0 {
            if let Some(c) = find_method_cycle(next, adj, color, path) {
                return Some(c);
            }
        }
    }
    path.pop();
    color[node] = 2; // black (fully explored)
    None
}

/// MVP testbench-component validation: every field must be the DUT
/// field (a non-HARC type, i.e. a Verilator module type), a covergroup
/// instance, an `active` transactor instance, or a scalar
/// (uint/sint/bits/bool) member — run/check-shared host state on the
/// `_tb` struct. Scoreboard / agent / env fields are post-MVP.
fn validate_testbench_component(
    c: &ComponentDecl,
    components: &HashMap<String, &ComponentDecl>,
    covgroup_ids: &HashMap<String, CovgroupId>,
    record_ids: &HashMap<String, RecordId>,
    transactor_ids: &HashMap<String, TransactorId>,
    scoreboard_ids: &HashMap<String, ScoreboardId>,
    component_type_names: &HashSet<String>,
    event_driven_transactor_names: &HashSet<String>,
    reactive_monitor_names: &HashSet<String>,
    dut_poking_bfm_names: &HashSet<String>,
    function_library_names: &HashSet<String>,
    passive_helper_names: &HashSet<String>,
) -> Result<(), LowerError> {
    for ci in &c.items {
        match ci {
            ComponentItem::Field(f) => {
                if let TypeExpr::Named { name, mode, .. } = &f.ty {
                    let simple = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                    if covgroup_ids.contains_key(simple) {
                        continue;
                    }
                    // A reactive monitor / checker transactor field (`mon :
                    // MemXactor passive`) routes to a `ComponentSchema`;
                    // its always-on cycle-trigger / periodic handlers
                    // register regardless of mode, so BOTH `active` and
                    // `passive` are accepted (and an unannotated instance
                    // defaults to the observation-only behavior). Checked
                    // before the event-driven gate (a monitor is a subset).
                    if reactive_monitor_names.contains(simple) {
                        continue;
                    }
                    // A function-library transactor field (`model :
                    // ProtocolModel active`) routes to a `ComponentSchema`
                    // but is still a transactor: it tolerates an
                    // `active`/`passive` mode (inert — no `when active`
                    // registration to gate), so any mode (or none) is
                    // accepted. Checked before the composite-component gate,
                    // which otherwise rejects a mode on a component field.
                    if function_library_names.contains(simple) {
                        continue;
                    }
                    // A passive helper / monitor transactor has a DUT
                    // handle but no `when active` body, so its always-on
                    // hookables exist on passive instances.
                    if passive_helper_names.contains(simple) {
                        continue;
                    }
                    // An event-driven transactor field (`drv : SeqXactor
                    // active`) routes to a `ComponentSchema` but is still a
                    // transactor: it requires an explicit `active` mode (a
                    // `passive` instance has no `when active` body — its
                    // `on` handler never registers, so it can't consume).
                    if event_driven_transactor_names.contains(simple) {
                        match mode {
                            Some(TransactorMode::Active) => continue,
                            Some(TransactorMode::Passive) => {
                                return Err(unsupported(
                                    &format!(
                                        "a passive event-driven transactor field `{}.{} : \
                                         {simple} passive`",
                                        c.name.name, f.name.name
                                    ),
                                    "the consumer's `on` handler only registers on an \
                                     `active` instance",
                                ));
                            }
                            None => {
                                return Err(unsupported(
                                    &format!(
                                        "an event-driven transactor field `{}.{} : {simple}` \
                                         without an `active`/`passive` mode",
                                        c.name.name, f.name.name
                                    ),
                                    "annotate the instance `active`",
                                ));
                            }
                        }
                    }
                    // A DUT-poking hookable BFM transactor field (`drv :
                    // CounterDriver active`). It routes to a
                    // `ComponentSchema` (so an env can hold one by value)
                    // but is still a transactor: its methods live under
                    // `when active`, so it requires an explicit `active`
                    // mode — exactly like the event-driven gate above. A
                    // `passive` instance structurally lacks every method.
                    // Checked before the composite-component gate (which
                    // rejects any mode), since a BFM IS in
                    // `component_type_names`.
                    if dut_poking_bfm_names.contains(simple) {
                        match mode {
                            Some(TransactorMode::Active) => continue,
                            Some(TransactorMode::Passive) => {
                                return Err(unsupported(
                                    &format!(
                                        "a passive DUT-poking transactor field `{}.{} : \
                                         {simple} passive`",
                                        c.name.name, f.name.name
                                    ),
                                    "methods inside `when active` do not exist on a passive \
                                     instance",
                                ));
                            }
                            None => {
                                return Err(unsupported(
                                    &format!(
                                        "a DUT-poking transactor field `{}.{} : {simple}` \
                                         without an `active`/`passive` mode",
                                        c.name.name, f.name.name
                                    ),
                                    "annotate the instance `active`",
                                ));
                            }
                        }
                    }
                    // A composite-component type (method-bearing
                    // scoreboard, analysis-source transactor, env, or
                    // agent) bound as a testbench field. Accepted by the
                    // testbench-field-binding slice: the field routes to a
                    // `ComponentSchema` instance just like a test-scope
                    // `let env : <Env>` does. A `mode` (active/passive) is
                    // meaningless on a composite component (that keyword is
                    // a transactor concept), so reject it rather than
                    // silently drop it.
                    if component_type_names.contains(simple) {
                        if mode.is_some() {
                            return Err(unsupported(
                                &format!(
                                    "an `active`/`passive` mode on composite-component \
                                     testbench field `{}.{} : {simple}`",
                                    c.name.name, f.name.name
                                ),
                                "the mode keyword applies to transactor instances, not \
                                 envs/agents/scoreboards",
                            ));
                        }
                        continue;
                    }
                    if scoreboard_ids.contains_key(simple) {
                        // A scoreboard testbench field — data-only host
                        // state. Always accepted (no mode); the schema's
                        // own lowering already rejected unsupported field
                        // shapes.
                        continue;
                    }
                    if transactor_ids.contains_key(simple) {
                        // Mode rules mirror v1: a mode-less transactor
                        // field has nothing to inherit from at testbench
                        // scope. An `active` instance exposes its `when
                        // active` methods; a `passive` instance exposes
                        // only its passive surface — persistent state
                        // fields (kept, with their `default` initializer)
                        // and any always-on `on` handlers. The `when
                        // active` methods structurally do not exist on a
                        // passive instance (they are simply never callable
                        // — a call site would fail to resolve), so v1
                        // lowers a passive instance by keeping the state
                        // and omitting the active methods (#494 P0a/P1b).
                        // Multiple passive instances each get their own
                        // per-instance state struct (see `lower_test`).
                        match mode {
                            Some(TransactorMode::Active) | Some(TransactorMode::Passive) => continue,
                            None => {
                                return Err(unsupported(
                                    &format!(
                                        "transactor field `{}.{} : {simple}` without an \
                                         `active`/`passive` mode",
                                        c.name.name, f.name.name
                                    ),
                                    "",
                                ));
                            }
                        }
                    }
                    // Transaction/struct-typed testbench fields are
                    // shared host record state; validation only accepts
                    // the field type here. The concrete field list is
                    // collected in `lower_test`, where the per-testbench
                    // schema is available.
                    if record_ids.contains_key(simple) {
                        continue;
                    }
                    // env/agent component types are accepted by the
                    // `component_type_names` gate above; the only remaining
                    // entry in `components` here is a `sequencer`, which is
                    // out of the lowered subset entirely.
                    if components.contains_key(simple) {
                        return Err(unsupported(
                            &format!(
                                "testbench field `{}` of component type `{}`",
                                f.name.name, simple
                            ),
                            "the `sequencer` construct is not in this subset",
                        ));
                    }
                } else if tb_scalar_field_ir_type(&f.ty).is_none() {
                    return Err(unsupported(
                        &format!(
                            "testbench field `{}` with a non-scalar, non-named type",
                            f.name.name
                        ),
                        "only uint/sint/bits/bool fields up to 64 bits are lowered",
                    ));
                }
            }
            // Lifecycle blocks were folded into the test's scope by the
            // impl-for desugaring; the declaration itself is inert here.
            ComponentItem::Lifecycle(..) => {}
            // Helper methods are inert unless called; calls surface as
            // `Unsupported` at the call site during body lowering.
            ComponentItem::Hookable(_) => {}
            // Testbench-scoped `on ... end on` handler (issues #485, #494).
            // The periodic form (`on <N> cycles [phase post_eval]`) and the
            // cycle-trigger form (`on <bool-expr> [phase post_eval]`) both
            // lower to flow-owned services (see `lower_test`). The
            // event-subscription form (`on <ev>(arg)`) and pre/post method
            // hooks are not lowered at testbench scope — reject them by
            // their exact kind so the diagnostic points at the real gap.
            ComponentItem::OnHandler(h) => {
                if h.hook.is_some() {
                    return Err(unsupported(
                        &format!(
                            "a testbench-scoped `on <obj>.<method>` pre/post method hook in `{}`",
                            c.name.name
                        ),
                        "only periodic `on <N> cycles` and cycle-trigger `on <bool-expr>` \
                         handlers are lowered at testbench scope",
                    ));
                }
                // An event-subscription / handshake-monitor form (`on
                // ev(arg)`) is a `Call` trigger; testbench scope has no
                // event fields to subscribe to, so reject it distinctly.
                if !h.periodic && matches!(&*h.event.kind, ExprKind::Call { .. }) {
                    return Err(unsupported(
                        &format!(
                            "a testbench-scoped event / handshake-monitor `on` handler in `{}`",
                            c.name.name
                        ),
                        "only periodic `on <N> cycles` and cycle-trigger `on <bool-expr>` \
                         handlers are lowered at testbench scope; move an event / handshake \
                         handler onto a component field",
                    ));
                }
                // A periodic handler (`on <N> cycles`) or a cycle-trigger
                // handler (`on <bool-expr>`) is accepted; its period /
                // predicate is validated when `lower_test` lowers it.
            }
            _ => {
                return Err(unsupported(
                    &format!(
                        "a `{}` item in testbench `{}`",
                        item_component_label(ci),
                        c.name.name
                    ),
                    "only fields, lifecycle phases, helper methods, and periodic `on <N> cycles` \
                     handlers are lowered at testbench scope",
                ));
            }
        }
    }
    Ok(())
}

fn item_label(it: &Item) -> &'static str {
    match it {
        Item::Use(_) => "use",
        Item::Package(_) => "package",
        Item::Const(_) => "const",
        Item::Domain(_) => "domain",
        Item::Struct(_) => "struct",
        Item::Enum(_) => "enum",
        Item::Transaction(_) => "transaction",
        Item::Relation(_) => "relation",
        Item::Tseq(_) => "tseq",
        Item::Agent(_) => "agent",
        Item::Env(_) => "env",
        Item::Scoreboard(_) => "scoreboard",
        Item::Sequencer(_) => "sequencer",
        Item::Test(_) => "test",
        Item::Extend(_) => "extend",
        Item::Covergroup(_) => "covergroup",
        Item::Property(_) => "property",
        Item::Pseq(_) => "pseq",
        Item::CoverSequence(_) => "cover sequence",
        Item::ExternalModule(_) => "external module",
        Item::Function(_) => "function",
        Item::ExternFn(_) => "extern fn",
        Item::Apply(_) => "apply",
        Item::Bus(_) => "bus",
        Item::Transactor(_) => "transactor",
        Item::Regblock(_) => "regblock",
        Item::Addrmap(_) => "addrmap",
    }
}

/// Human-readable kind for a `ComponentItem`, for testbench-scope
/// diagnostics (issue #485).
fn item_component_label(it: &ComponentItem) -> &'static str {
    match it {
        ComponentItem::Field(_) => "field",
        ComponentItem::Connect(_) => "connect",
        ComponentItem::OnHandler(_) => "on-handler",
        ComponentItem::TargetTlmThread(_) => "target thread",
        ComponentItem::Hookable(_) => "method",
        ComponentItem::Lifecycle(..) => "lifecycle phase",
        ComponentItem::Apply(_) => "apply",
        ComponentItem::Watchdog(_) => "watchdog",
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_test(
    t: &TestDecl,
    tb_of_test: &HashMap<String, String>,
    components: &HashMap<String, &ComponentDecl>,
    component_ids: &HashMap<String, ir::ComponentId>,
    domains: &HashMap<String, i64>,
    covgroup_ids: &HashMap<String, CovgroupId>,
    record_ids: &HashMap<String, RecordId>,
    regblock_ids: &HashMap<String, RegblockId>,
    addrmap_decls: &HashMap<String, &AddrmapDecl>,
    buses: &HashMap<String, &BusDecl>,
    unresolved_use_names: &HashSet<String>,
    consts: &HashMap<String, u64>,
    const_signed: &HashMap<String, bool>,
    extern_fns: &HashSet<String>,
    helpers: &helpers::HelperRegistry<'_>,
    txn_keeps: &HashMap<String, Vec<crate::ast::Expr>>,
    randomize_problem_ids: &HashMap<(usize, usize), u32>,
    tseq_records: &HashMap<String, TseqElem>,
    constraint_sites: &RefCell<Vec<ConstraintSite>>,
    dut_poking_bfm_names: &HashSet<String>,
    prog: &mut TbProgram,
) -> Result<(), LowerError> {
    if !t.params.is_empty() {
        return Err(unsupported("test parameters", ""));
    }

    let mut dut_type: Option<String> = None;
    // DUT-internal probe declarations on `let dut` (name → metadata).
    // Threaded into `LowerCtx::probes` so `dut.<probe>` accesses lower to
    // a `Probe`/`Force` `PortRef`. See docs/probe-signals.md.
    let mut probes: HashMap<String, ProbeMeta> = HashMap::new();
    let mut clocks: Vec<&ClockDecl> = Vec::new();
    let mut scope: Option<&ScopeDecl> = None;
    let mut bare_stmts: Vec<&AstStmt> = Vec::new();
    // Count of bare statements collected *before* the `scope` block was
    // seen, in test-item order. v1 emits the whole test (bare stmts +
    // scope phases) into one coroutine in source order, so a bare
    // statement that precedes the `scope` runs before setup/run, and one
    // that follows it runs after teardown. The run/check IR split (Run =
    // setup+run, Check = check+teardown, emitted back-to-back) preserves
    // that ordering by routing the pre-scope bare stmts to the front of
    // the run list and the post-scope ones to the tail of the check list.
    let mut n_bare_before_scope: usize = 0;
    // Test-scope `on <obj>.<method> pre/post` method hooks, in source
    // order. Resolved to a transactor field + method and lowered as
    // `FunctionKind::TestHook` bodies once the test ctx is built.
    let mut method_hook_asts: Vec<(HookSide, &crate::ast::OnHandler)> = Vec::new();
    // Candidate `on regs.REG` per-register write callbacks (`on
    // <ident>.<name>`, no hook side / period), carried as whole
    // statements so a non-regblock candidate can fall back to the
    // bare-statement path. Resolved against the regblock bindings +
    // lowered as `FunctionKind::TestHook` callbacks.
    let mut reg_cb_asts: Vec<&AstStmt> = Vec::new();
    // Named `phase <name> ... end phase <name>` blocks (spec §7.2). Each
    // is callable by `<name>()` from the run/check body; the call site is
    // INLINED with the phase block's statements (v1 emits a `[&]() ->
    // void` lambda + a plain call — identical observable behavior, since
    // the body runs at the call site inside the run coroutine). Collected
    // in declaration order so a redeclaration is rejected.
    let mut phases: HashMap<String, &Block> = HashMap::new();
    let mut bus_bindings: Vec<ir::BusBindingSchema> = Vec::new();
    let mut bus_binding_decls: HashMap<String, BusDecl> = HashMap::new();
    // Regblock bindings (`let regs : R = bind <helper>`), collected here
    // and validated after the testbench's transactor fields are known
    // (the `<helper>` must be an active transactor field). Each tuple is
    // (binding name, regblock id, helper field name).
    let mut regblock_binds: Vec<(String, RegblockId, String)> = Vec::new();
    // Addrmap bindings (`let chip : A = bind <helper>`), collected here
    // and resolved after the testbench's transactor fields are known.
    // Each tuple is (binding name, addrmap type name, helper field name).
    let mut addrmap_binds: Vec<(String, String, String)> = Vec::new();
    // Test-scope unbound-transactor instances (`let h : Xactor active`),
    // accessed by bare name. Merged into `transactor_fields` after the
    // testbench-field walk; collected here as (name, transactor id).
    let mut test_scope_xactors: Vec<(String, TransactorId)> = Vec::new();
    // Test-scope composite-component instances (`let env : AnalysisEnv`),
    // collected as (name, component id). Emitted as plain run-scope
    // locals + their `connect` push_backs.
    let mut test_scope_components: Vec<(String, ir::ComponentId)> = Vec::new();
    // Bound-to target-side TLM responder instances (`let target : X
    // passive = bind <busbinding>`), collected as (instance, transactor
    // id, bus-binding field). Validated after the bus bindings are known.
    let mut target_tlm_binds: Vec<(String, TransactorId, String)> = Vec::new();
    // Bound-to initiator-side BFM instances (`let helper : H active =
    // bind <busbinding>`), collected as (instance, transactor id, bus-
    // binding field). The helper's `hookable` methods drive the bound
    // bus's channels; it is registered as a transactor field so the
    // regblock `via` frontdoor and bare `helper.method(...)` calls
    // resolve. Validated after the bus bindings are known.
    let mut initiator_bfm_binds: Vec<(String, TransactorId, String)> = Vec::new();
    // Bound-to event-driven component instances (`let xact : X active =
    // bind <busbinding>`), collected as (instance, component id, bus-
    // binding field). The component's `on <ev>` handler bodies drive the
    // bound bus's channels; the placeholder bus prefix in those bodies is
    // filled with the real binding name (like the initiator-BFM path), and
    // the instance is registered as a component field. Validated after the
    // bus bindings are known.
    // `(instance, component, bus_field, active)` — `active` distinguishes
    // the `on <ev>` driver instance (re-lowered into a queue-fed worker
    // coroutine under `--mt`) from a `passive` monitor-only instance.
    let mut bound_event_component_binds: Vec<(String, ir::ComponentId, String, bool)> = Vec::new();
    // Test-scope `let`s (beyond `dut`/`_tb`/bus binds), hoisted to the
    // head of the run body in item order — v1 hoists them to `main`
    // scope before the coroutine, initialized once, and the coroutine
    // captures by reference; the IR lowers them as run-function locals
    // initialized at entry. Names are recorded so a check-phase
    // reference gets a precise rejection (run and check are separate
    // IR functions, so the shared-state form is not representable).
    let mut test_let_stmts: Vec<AstStmt> = Vec::new();
    let mut test_let_names: HashSet<String> = HashSet::new();
    let tb_name = tb_of_test.get(&t.name.name).cloned();

    for it in &t.items {
        match it {
            TestItem::Let(l) if l.name.name == "dut" => {
                if !l.bind_remap.is_empty() {
                    return Err(unsupported("bind remaps on `let dut`", ""));
                }
                // Collect DUT-internal probe declarations. Each becomes a
                // `Probe`/`Force` access class in `LowerCtx::probes`,
                // consulted when lowering `dut.<probe>` reads/writes and
                // `release dut.<probe>`. The probe type must be a scalar
                // (uint<N>/sint<N>/bits<N>/bit/bool) — the SV bind stub
                // only surfaces scalar logic; reject aggregates precisely
                // (v1's `sv_type_decl` errors at SV-emit time).
                for p in &l.probes {
                    let width = probe_scalar_width(&p.ty).ok_or_else(|| {
                        unsupported(
                            &format!("probe `{}` of non-scalar type", p.name.name),
                            "probe types must be uint<N>/sint<N>/bits<N>/bit/bool",
                        )
                    })?;
                    if probes
                        .insert(
                            p.name.name.clone(),
                            ProbeMeta {
                                force: p.force,
                                width: Some(width),
                            },
                        )
                        .is_some()
                    {
                        return Err(LowerError::Invalid(format!(
                            "duplicate probe `{}` on `let dut`",
                            p.name.name
                        )));
                    }
                }
                let simple = match l.ty.as_ref() {
                    Some(TypeExpr::Named { name, .. }) => {
                        name.segments.last().map(|s| s.name.clone())
                    }
                    _ => None,
                };
                dut_type = Some(simple.ok_or_else(|| {
                    LowerError::Invalid(
                        "`let dut : <Type>` must use a simple named type".to_string(),
                    )
                })?);
            }
            // The desugared impl-form synthesizes `let _tb : <TbType>`;
            // the TB instance is scaffolding-owned in the IR backend.
            TestItem::Let(l) if l.name.name == "_tb" => {}
            // Bus binding: `let axil : BusAxiLite = bind dut`.
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref()).is_some_and(|n| buses.contains_key(n)) =>
            {
                let bus_name = type_simple_name(l.ty.as_ref()).unwrap();
                let decl = buses[bus_name];
                let (schema, owned) = bus::lower_bus_binding(l, decl)?;
                if bus_binding_decls.contains_key(&l.name.name) {
                    // Two bindings with one name would resolve
                    // ambiguously (v1's map silently keeps the last;
                    // the IR schema would keep both) — reject.
                    return Err(LowerError::Invalid(format!(
                        "duplicate bus binding `{}` in test `{}`",
                        l.name.name, t.name.name
                    )));
                }
                bus_bindings.push(schema);
                bus_binding_decls.insert(l.name.name.clone(), owned);
            }
            // Regblock binding: `let regs : DmaRegs = bind <helper>`.
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| regblock_ids.contains_key(n)) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported("probe declarations on a regblock binding", ""));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported("bind remaps on a regblock binding", ""));
                }
                let rb_name = type_simple_name(l.ty.as_ref()).unwrap();
                let rbid = regblock_ids[rb_name];
                // RHS must be a bare helper-instance identifier (the
                // transactor field the frontdoor routes through).
                let helper_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "regblock binding `{}` to a non-identifier helper",
                                l.name.name
                            ),
                            "only `= bind <helper>` (a transactor instance) is lowered",
                        ));
                    }
                };
                regblock_binds.push((l.name.name.clone(), rbid, helper_field));
            }
            // Addrmap binding: `let chip : Soc = bind <helper>`.
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| addrmap_decls.contains_key(n)) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported("probe declarations on an addrmap binding", ""));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported("bind remaps on an addrmap binding", ""));
                }
                let amap_name = type_simple_name(l.ty.as_ref()).unwrap().to_string();
                // RHS must be a bare helper-instance identifier.
                let helper_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "addrmap binding `{}` to a non-identifier helper",
                                l.name.name
                            ),
                            "only `= bind <helper>` (a transactor instance) is lowered",
                        ));
                    }
                };
                addrmap_binds.push((l.name.name.clone(), amap_name, helper_field));
            }
            // Bound-to initiator-side BFM: `let helper : AxilHelper
            // active = bind <busbinding>`. The helper's `hookable`
            // methods drive the bound bus's handshake channels; it is
            // registered as an active transactor field so the regblock
            // `via <helper>` frontdoor (#369) and bare
            // `helper.method(...)` calls resolve through the same
            // `CallTarget::TransactorMethod` dispatch. The bind RHS must
            // be a bus binding declared earlier in the test, of the bus
            // the transactor is `bound to` (validated after the binding
            // walk). Distinguished from the target-responder form below
            // by carrying `methods` (initiator) rather than
            // `target_methods` (responder).
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref()).is_some_and(|n| {
                        prog.transactors
                            .iter()
                            .any(|x| x.name == n && x.bound_bus.is_some() && !x.methods.is_empty())
                    }) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on an initiator-BFM instance",
                        "",
                    ));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported(
                        "bind remaps on an initiator-BFM instance",
                        "the default `<binding>_<ch>_<sig>` wire convention is lowered; \
                         custom signal remaps are a follow-up slice",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                // The BFM host must be `active` — its methods are
                // test-called (via the regblock frontdoor or directly).
                match l.ty.as_ref() {
                    Some(TypeExpr::Named {
                        mode: Some(TransactorMode::Active),
                        ..
                    }) => {}
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "initiator-BFM instance `let {} : {simple}` must be declared \
                                 `active`",
                                l.name.name
                            ),
                            "its hookable methods are test-called, not request-served",
                        ));
                    }
                }
                // RHS must be a bare bus-binding identifier.
                let bus_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "initiator-BFM instance `{}` bound to a non-identifier",
                                l.name.name
                            ),
                            "only `= bind <bus-binding>` is lowered",
                        ));
                    }
                };
                let xid = ir::TransactorId(
                    prog.transactors
                        .iter()
                        .position(|x| x.name == simple)
                        .unwrap() as u32,
                );
                initiator_bfm_binds.push((l.name.name.clone(), xid, bus_field));
            }
            // Bound-to event-driven component instance: `let xact :
            // AxilXactor active = bind axil`. The transactor routes to a
            // `ComponentSchema` (event-driven consumer BFM) AND carries a
            // `bound_bus`; its `on <ev>` handler bodies drive the bound
            // bus's handshake channels. The bind RHS must be a bus binding
            // declared earlier in the test (validated after the walk). A
            // mode is required: the `on req` handler lives under `when
            // active`, so a `passive` instance has no driver. (The passive
            // monitor-only form — `on bus.<ch>.handshake` observers — is a
            // follow-up slice.)
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref()).is_some_and(|n| {
                        component_ids
                            .get(n)
                            .is_some_and(|cid| prog.components[cid.index()].bound_bus.is_some())
                    }) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on a bound-to event-driven transactor instance",
                        "",
                    ));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported(
                        "bind remaps on a bound-to event-driven transactor instance",
                        "the default `<binding>_<ch>_<sig>` wire convention is lowered; custom \
                         signal remaps are a follow-up slice",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                let cid_probe = component_ids[simple];
                let has_monitor = prog.components[cid_probe.index()]
                    .cycle_handlers
                    .iter()
                    .any(|ch| ch.monitor_channel.is_some());
                // Mode rules:
                //   * `active`  — the `on <ev>` driver (under `when active`)
                //     fires on `emit <inst>.<ev>`. Always permitted.
                //   * `passive` — no driver; only the always-on
                //     `on bus.<ch>.handshake` monitor observers fire. Valid
                //     only when the transactor declares monitor handlers (a
                //     pure-driver transactor with no monitor half has nothing
                //     for a passive instance to do).
                let instance_active = match l.ty.as_ref() {
                    Some(TypeExpr::Named {
                        mode: Some(TransactorMode::Active),
                        ..
                    }) => true,
                    Some(TypeExpr::Named {
                        mode: Some(TransactorMode::Passive),
                        ..
                    }) => {
                        if !has_monitor {
                            return Err(unsupported(
                                &format!(
                                    "passive bound-to event-driven transactor instance `let {} : \
                                     {simple} passive` with no monitor half",
                                    l.name.name
                                ),
                                "a `passive` instance only runs the always-on \
                                 `on bus.<ch>.handshake(...)` observers; this transactor declares \
                                 none, so a passive instance is inert — annotate it `active`",
                            ));
                        }
                        false
                    }
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "bound-to event-driven transactor instance `let {} : {simple}` \
                                 without an `active`/`passive` mode",
                                l.name.name
                            ),
                            "annotate the instance `active` (driver) or `passive` (monitor)",
                        ));
                    }
                };
                // RHS must be a bare bus-binding identifier.
                let bus_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "bound-to event-driven transactor `{}` bound to a non-identifier",
                                l.name.name
                            ),
                            "only `= bind <bus-binding>` is lowered",
                        ));
                    }
                };
                let cid = component_ids[simple];
                bound_event_component_binds.push((
                    l.name.name.clone(),
                    cid,
                    bus_field,
                    instance_active,
                ));
            }
            // Bound-to target-side TLM responder: `let target : MemTarget
            // passive = bind <busbinding>`. The instance is a passive
            // responder host for a `transactor X bound to <Bus>`; its
            // `thread bus.<m>(...)` bodies serve the bound bus binding's
            // req/rsp wires. The bind RHS must be a bus binding declared
            // earlier in the test (validated after the binding walk).
            TestItem::Let(l)
                if l.bind
                    && type_simple_name(l.ty.as_ref()).is_some_and(|n| {
                        prog.transactors
                            .iter()
                            .any(|x| x.name == n && x.bound_bus.is_some())
                    }) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on a target-TLM responder instance",
                        "",
                    ));
                }
                if !l.bind_remap.is_empty() {
                    return Err(unsupported(
                        "bind remaps on a target-TLM responder instance",
                        "the default `<binding>_<method>_<sig>` wire convention is lowered; \
                         custom signal remaps are a follow-up slice",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                // The responder host must be `passive` — its methods are
                // request-served, never test-called.
                match l.ty.as_ref() {
                    Some(TypeExpr::Named {
                        mode: Some(TransactorMode::Passive),
                        ..
                    }) => {}
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "target-TLM responder instance `let {} : {simple}` must be \
                                 declared `passive`",
                                l.name.name
                            ),
                            "the responder serves bus requests; its methods are not test-called",
                        ));
                    }
                }
                // RHS must be a bare bus-binding identifier.
                let bus_field = match l.value.as_ref().map(|v| &*v.kind) {
                    Some(ExprKind::Ident(id)) => id.name.clone(),
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "target-TLM responder `{}` bound to a non-identifier",
                                l.name.name
                            ),
                            "only `= bind <bus-binding>` is lowered",
                        ));
                    }
                };
                let xid = ir::TransactorId(
                    prog.transactors
                        .iter()
                        .position(|x| x.name == simple)
                        .unwrap() as u32,
                );
                target_tlm_binds.push((l.name.name.clone(), xid, bus_field));
            }
            // Test-scope composite-component instance: `let env :
            // AnalysisEnv`. Emitted as a plain run-scope local (v1's
            // `AnalysisEnv env;`), default-constructed, with its env
            // `connect` push_backs wired right after. Accessed by bare
            // dotted path (`env.source.publish(..)`, `env.sb.count`).
            TestItem::Let(l)
                if !l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| component_ids.contains_key(n)) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on a component instance",
                        "",
                    ));
                }
                if l.value.is_some() {
                    return Err(unsupported(
                        &format!(
                            "component instance `let {}` with an initializer",
                            l.name.name
                        ),
                        "components default-construct",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                // A DUT-poking hookable BFM transactor routes to a
                // `ComponentSchema`, so it lands in this component-let
                // branch — but it is still a transactor: its methods live
                // under `when active`, so a test-scope `let drv : X active`
                // requires an explicit `active` mode (a `passive` instance
                // has no methods). A genuine composite component (env /
                // agent / scoreboard / sequencer) takes no mode.
                if dut_poking_bfm_names.contains(simple) {
                    match l.ty.as_ref() {
                        Some(TypeExpr::Named {
                            mode: Some(TransactorMode::Active),
                            ..
                        }) => {}
                        Some(TypeExpr::Named {
                            mode: Some(TransactorMode::Passive),
                            ..
                        }) => {
                            return Err(unsupported(
                                &format!(
                                    "passive DUT-poking transactor instance `let {} : \
                                     {simple} passive`",
                                    l.name.name
                                ),
                                "methods inside `when active` do not exist on a passive instance",
                            ));
                        }
                        _ => {
                            return Err(unsupported(
                                &format!(
                                    "DUT-poking transactor instance `let {} : {simple}` without \
                                     an `active`/`passive` mode",
                                    l.name.name
                                ),
                                "annotate the instance `active`",
                            ));
                        }
                    }
                }
                let cid = component_ids[simple];
                test_scope_components.push((l.name.name.clone(), cid));
            }
            // Test-scope unbound-transactor instance: `let h : MemHelper
            // active` (no `= bind`). v1 routes regblock frontdoor calls
            // and bare `h.method(...)` through a test-scope-let helper
            // (it lives in v1's `let_types`), so the IR registers it as a
            // transactor instance accessed by its BARE name (not
            // `_tb.h` — test-scope lets aren't rewritten by the impl-for
            // desugaring). The DUT bind `h.dut = dut` and method calls
            // resolve through the same `transactor_fields` machinery as a
            // testbench-field instance.
            TestItem::Let(l)
                if !l.bind
                    && type_simple_name(l.ty.as_ref())
                        .is_some_and(|n| prog.transactors.iter().any(|x| x.name == n)) =>
            {
                if !l.probes.is_empty() {
                    return Err(unsupported(
                        "probe declarations on a transactor instance",
                        "",
                    ));
                }
                if l.value.is_some() {
                    return Err(unsupported(
                        &format!(
                            "transactor instance `let {}` with an initializer",
                            l.name.name
                        ),
                        "transactor instances default-construct; bind the DUT with \
                         `{}.dut = dut` in the body",
                    ));
                }
                let simple = type_simple_name(l.ty.as_ref()).unwrap();
                // Require an explicit `active` mode (matching the
                // testbench-field rule: every method lives in `when
                // active`, so a passive instance has none).
                match l.ty.as_ref() {
                    Some(TypeExpr::Named {
                        mode: Some(TransactorMode::Active),
                        ..
                    }) => {}
                    Some(TypeExpr::Named {
                        mode: Some(TransactorMode::Passive),
                        ..
                    }) => {
                        return Err(unsupported(
                            &format!(
                                "passive transactor instance `let {} : {simple} passive`",
                                l.name.name
                            ),
                            "methods inside `when active` do not exist on a passive instance",
                        ));
                    }
                    _ => {
                        return Err(unsupported(
                            &format!(
                                "transactor instance `let {} : {simple}` without an \
                                 `active`/`passive` mode",
                                l.name.name
                            ),
                            "",
                        ));
                    }
                }
                let xid = ir::TransactorId(
                    prog.transactors
                        .iter()
                        .position(|x| x.name == simple)
                        .unwrap() as u32,
                );
                let xdut = &prog.transactors[xid.index()].dut_type;
                if let Some(dt) = &dut_type {
                    if xdut != dt {
                        return Err(unsupported(
                            &format!(
                                "transactor instance `{}` whose DUT field type `{xdut}` \
                                 differs from the test DUT type `{dt}`",
                                l.name.name
                            ),
                            "",
                        ));
                    }
                }
                test_scope_xactors.push((l.name.name.clone(), xid));
            }
            TestItem::Let(l) => {
                if !l.probes.is_empty() || l.bind {
                    // A bind whose type name matches a `use` that never
                    // resolved gets a targeted diagnostic instead of the
                    // generic "let with a bind" rejection below — the type
                    // is not unsupported, it's simply missing (see #493).
                    if l.bind {
                        if let Some(name) = type_simple_name(l.ty.as_ref()) {
                            if unresolved_use_names.contains(name) {
                                return Err(LowerError::Invalid(format!(
                                    "test-scope `let {} : {name} = bind ...` references type \
                                     `{name}`, but `use {name};` never resolved — no \
                                     `{name}.arch` or `{name}.harc` was found in \
                                     $HARC_LIB_PATH, <input dir>/stdlib/, ./stdlib/, or \
                                     ../arch-com/{{stdlib,examples}}/. Check the import name/path, \
                                     or declare `bus {name} ... end bus {name}` locally.",
                                    l.name.name
                                )));
                            }
                        }
                    }
                    return Err(unsupported(
                        &format!("test-scope `let {}` with probes or a bind", l.name.name),
                        "only plain `let <name> [: <Ty>] = <expr>` test-scope lets are lowered",
                    ));
                }
                test_let_names.insert(l.name.name.clone());
                test_let_stmts.push(AstStmt {
                    kind: StmtKind::Let(l.clone()),
                    span: l.span,
                });
            }
            TestItem::Clock(c) => clocks.push(c),
            TestItem::Scope(s) => {
                if scope.is_some() {
                    return Err(unsupported("multiple `scope` blocks in one test", ""));
                }
                scope = Some(s);
                // Everything in `bare_stmts` so far precedes the scope.
                n_bare_before_scope = bare_stmts.len();
            }
            TestItem::Stmt(s) => {
                // Test-scope pre/post method hook (`on drv.send pre ...
                // end on`): a hookable-method hook registration. Collected
                // here and lowered as a `FunctionKind::TestHook` body after
                // the test ctx is built (host-state promotion lets the
                // function-per-CFG IR express v1's `[&]`-capturing hook
                // closure — captured run-scope `let`s become `_tb` fields).
                // Routing it out of `bare_stmts` also pre-empts the generic
                // bare-statement/scope mixing error below.
                if let StmtKind::On(h) = &s.kind {
                    if let Some(side) = h.hook {
                        if h.phase == OnPhase::PostEval {
                            return Err(unsupported(
                                "a test-scope `on <obj>.<method> phase post_eval` hook",
                                "only `pre`/`post` method hooks are lowered",
                            ));
                        }
                        method_hook_asts.push((side, h));
                        continue;
                    }
                    // Candidate per-register `on regs.REG` write callback
                    // (`on <ident>.<name>`, no hook side / period): collect
                    // for resolution once regblock bindings are known. A
                    // non-regblock `<ident>.<name>` cycle-trigger falls back
                    // to bare_stmts (it never matches a regblock binding).
                    if h.hook.is_none() && !h.periodic {
                        if let ExprKind::Field { target, .. } = &*h.event.kind {
                            if matches!(&*target.kind, ExprKind::Ident(_)) {
                                reg_cb_asts.push(s);
                                continue;
                            }
                        }
                    }
                }
                bare_stmts.push(s)
            }
            TestItem::Phase(name, body) => {
                if phases.insert(name.name.clone(), body).is_some() {
                    return Err(LowerError::Invalid(format!(
                        "test `{}` declares phase `{}` more than once",
                        t.name.name, name.name
                    )));
                }
            }
            TestItem::Apply(_) => return Err(unsupported("apply", "")),
            TestItem::Use(_) => {}
        }
    }

    let dut_type = dut_type.ok_or_else(|| {
        LowerError::Invalid(format!(
            "test `{}` has no `let dut : <Type>` declaration",
            t.name.name
        ))
    })?;

    // Resolve clocks.
    let mut clock_specs = Vec::new();
    for c in &clocks {
        let (period_ps, domain) = match &*c.period.kind {
            ExprKind::Time(s) => (time_literal_to_ps(s).map_err(LowerError::Invalid)?, None),
            ExprKind::Ident(id) => {
                let p = domains.get(&id.name).ok_or_else(|| {
                    LowerError::Invalid(format!(
                        "clock {} references domain `{}` but no `domain {}` declaration was found",
                        c.name.name, id.name, id.name
                    ))
                })?;
                (*p, Some(id.name.clone()))
            }
            _ => {
                return Err(LowerError::Invalid(format!(
                    "clock {} period must be a time literal or a domain name",
                    c.name.name
                )));
            }
        };
        clock_specs.push(ClockSpec {
            name: c.name.name.clone(),
            period_ps,
            domain,
        });
    }

    // Testbench schema.
    let synthetic = tb_name.is_none();
    let tb_schema_name = tb_name
        .clone()
        .unwrap_or_else(|| format!("{}_tb", t.name.name));
    if let Some(tbn) = &tb_name {
        // Validated at the file gate; double-check it resolved.
        if !components.contains_key(tbn) {
            return Err(LowerError::Invalid(format!(
                "test `{}` is bound to unknown testbench `{tbn}`",
                t.name.name
            )));
        }
    }
    // Covergroup- and transactor-typed testbench fields, in declaration
    // order — cov order is the sampler registration order (must match
    // v1); transactor order is the method-lambda emission order. Scalar
    // member fields and helper methods (`function`/`hookable`) are
    // collected in the same walk.
    let mut cov_fields: Vec<(String, CovgroupId)> = Vec::new();
    let mut transactor_fields: Vec<(String, TransactorId)> = Vec::new();
    // Testbench-field transactor instances declared `passive` (`a :
    // Poker passive`). A passive instance exposes only its passive
    // surface (persistent state + always-on `on` handlers); its `when
    // active` methods are never callable, so the shared per-type method
    // bodies are NOT filled with a passive instance name. That is what
    // lets two passive instances of one stateful type coexist with
    // independent state (#494 P0a/P1b) — the per-instance state struct is
    // then the only per-instance piece. Keyed by field name (a subset of
    // `transactor_fields` keys).
    let mut passive_transactor_fields: HashSet<String> = HashSet::new();
    let mut scoreboard_fields: Vec<(String, ScoreboardId)> = Vec::new();
    let mut scalar_fields: Vec<ir::TbScalarFieldSchema> = Vec::new();
    let mut record_fields: Vec<(String, RecordId)> = Vec::new();
    let mut tb_methods: HashMap<String, HookableMethod> = HashMap::new();
    // Testbench-scoped `on <N> cycles [phase post_eval]` periodic
    // handlers (issue #485). Collected here in declaration order; their
    // bodies are lowered as flow-owned `TestHook` functions after the
    // test ctx is built (alongside run/check), then registered on the
    // testbench schema.
    let mut tb_periodic_asts: Vec<&crate::ast::OnHandler> = Vec::new();
    // Testbench-scoped `on <bool-expr> [phase post_eval]` cycle-trigger
    // handlers (issue #494 P2b). Same treatment as the periodic form: the
    // body lowers to a flow-owned `TestHook` function and the predicate is
    // re-evaluated every cycle in a registration closure.
    let mut tb_cycle_asts: Vec<&crate::ast::OnHandler> = Vec::new();
    if let Some(tbn) = &tb_name {
        if let Some(c) = components.get(tbn) {
            for ci in &c.items {
                match ci {
                    ComponentItem::Field(f) => {
                        if let TypeExpr::Named { name, mode, .. } = &f.ty {
                            let simple =
                                name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                            if let Some(id) = covgroup_ids.get(simple) {
                                cov_fields.push((f.name.name.clone(), *id));
                            }
                            if let Some(idx) =
                                prog.scoreboards.iter().position(|s| s.name == simple)
                            {
                                scoreboard_fields
                                    .push((f.name.name.clone(), ScoreboardId(idx as u32)));
                            }
                            if let Some(idx) =
                                prog.transactors.iter().position(|t| t.name == simple)
                            {
                                let xid = TransactorId(idx as u32);
                                // The transactor's module-typed field must
                                // drive the same DUT type this test
                                // instantiates — the IR's single-DUT model
                                // makes the bind static.
                                let xdut = &prog.transactors[idx].dut_type;
                                if *xdut != dut_type {
                                    return Err(unsupported(
                                        &format!(
                                            "transactor field `{tbn}.{} : {simple}` whose \
                                             `{}` field type `{xdut}` differs from the test \
                                             DUT type `{dut_type}`",
                                            f.name.name, prog.transactors[idx].dut_field
                                        ),
                                        "",
                                    ));
                                }
                                transactor_fields.push((f.name.name.clone(), xid));
                                if matches!(mode, Some(TransactorMode::Passive)) {
                                    passive_transactor_fields.insert(f.name.name.clone());
                                }
                            }
                            if let Some(&rid) = record_ids.get(simple) {
                                record_fields.push((f.name.name.clone(), rid));
                            }
                            // Composite-component testbench field (`prod :
                            // Producer` / `top : HeartbeatEnv`). Routes to
                            // the SAME `test_scope_components` collector a
                            // test-scope `let env : <Env>` uses — the IR
                            // models both identically (a default-constructed
                            // run-scope instance plus its `connect`/`on`
                            // wiring). v1 instead holds the field on the
                            // `_tb` struct (`_tb.prod`); this is a recorded
                            // C++-shape divergence with identical trace
                            // behavior (docs/tbir-mvp.md). A method-bearing
                            // scoreboard / analysis-source transactor is in
                            // `component_ids` (NOT `prog.scoreboards` /
                            // `prog.transactors`), so it lands here and not
                            // in the data-only routes above.
                            if let Some(cid) = component_ids.get(simple) {
                                test_scope_components.push((f.name.name.clone(), *cid));
                            }
                        } else if let Some(ty) = tb_scalar_field_ir_type(&f.ty) {
                            let default = match &f.default {
                                None => 0,
                                Some(d) => match &*d.kind {
                                    ExprKind::Int(s) => {
                                        exprs::parse_int_literal(s).ok_or_else(|| {
                                            unsupported(
                                                &format!(
                                                    "testbench field default `{} default {s}`",
                                                    f.name.name
                                                ),
                                                "not a plain integer literal",
                                            )
                                        })?
                                    }
                                    ExprKind::Bool(b) => *b as u64,
                                    _ => {
                                        return Err(unsupported(
                                            &format!(
                                                "a non-literal default on testbench field `{}`",
                                                f.name.name
                                            ),
                                            "",
                                        ));
                                    }
                                },
                            };
                            scalar_fields.push(ir::TbScalarFieldSchema {
                                name: f.name.name.clone(),
                                ty,
                                default,
                            });
                        }
                    }
                    ComponentItem::Hookable(h) => {
                        // Rewrite the method body's bare references to
                        // testbench fields/siblings into `_tb.<name>` form,
                        // the same shape the impl-for desugaring gives the
                        // bound test body — so a helper that touches a
                        // testbench field (`_tb.count = ...`) or calls a
                        // sibling (`_tb.other()`) resolves when CFG-inlined
                        // (issue #485). `dut` and the method's own params are
                        // shadowed (left bare). A method touching only `dut`
                        // (the pre-#485 corpus) is unaffected by the rewrite.
                        let mut hm = h.clone();
                        let params: HashSet<String> =
                            h.params.iter().map(|p| p.name.name.clone()).collect();
                        crate::codegen::cpp_tb::rewrite_testbench_scope_body(
                            &mut hm.body,
                            c,
                            &params,
                        );
                        tb_methods.insert(h.name.name.clone(), hm);
                    }
                    // Testbench-scoped periodic `on <N> cycles` handler
                    // (issue #485). Validated by `validate_testbench_component`
                    // to be the periodic form; collect it here for body
                    // lowering below. Non-periodic testbench on-handlers are
                    // rejected there, so anything reaching this arm that is
                    // NOT periodic is a bug in the validator — skip it
                    // defensively rather than mis-lower.
                    ComponentItem::OnHandler(h) if h.periodic => tb_periodic_asts.push(h),
                    // Testbench-scoped cycle-trigger `on <bool-expr>` handler
                    // (issue #494 P2b). The validator accepts the non-
                    // periodic, non-call (non-event/handshake), non-hook form;
                    // collect it here for body + predicate lowering below.
                    ComponentItem::OnHandler(h)
                        if !h.periodic
                            && h.hook.is_none()
                            && !matches!(&*h.event.kind, ExprKind::Call { .. }) =>
                    {
                        tb_cycle_asts.push(h)
                    }
                    _ => {}
                }
            }
        }
    }
    // Merge test-scope-let transactor instances into the transactor
    // field set so the call/bind/regblock machinery resolves them
    // uniformly. They are accessed by their BARE name (test-scope lets
    // aren't `_tb`-prefixed by the desugaring), recorded separately so
    // resolution knows which access shape to expect.
    let mut bare_transactor_fields: HashSet<String> =
        test_scope_xactors.iter().map(|(n, _)| n.clone()).collect();
    for (name, xid) in &test_scope_xactors {
        if transactor_fields.iter().any(|(f, _)| f == name) {
            return Err(LowerError::Invalid(format!(
                "name `{name}` is both a testbench transactor field and a test-scope \
                 transactor instance in test `{}`",
                t.name.name
            )));
        }
        transactor_fields.push((name.clone(), *xid));
    }
    // Bound-to initiator-side BFM instances (`let helper : H active =
    // bind axil`). Validate the bound bus binding matches the
    // transactor's `bound to` bus, fill the placeholder bus prefix in
    // the (TYPE-shared) method bodies with the real binding name, and
    // register the helper as a bare-name active transactor field so the
    // regblock `via` frontdoor and direct method calls resolve. The
    // method bodies are shared per transactor TYPE, so the subset is one
    // bound instance per type per file — a second bind to a different
    // bus binding would clobber the first's filled prefix, so reject it.
    for (instance, xid, bus_field) in &initiator_bfm_binds {
        if transactor_fields.iter().any(|(f, _)| f == instance) {
            return Err(LowerError::Invalid(format!(
                "name `{instance}` is bound more than once in test `{}`",
                t.name.name
            )));
        }
        // The bound bus binding must be a `let <bus_field> : <Bus> =
        // bind dut` declared in this test, of the bound bus.
        let Some(binding) = bus_bindings.iter().find(|b| &b.field == bus_field) else {
            return Err(LowerError::Invalid(format!(
                "initiator-BFM `{instance}` is bound to `{bus_field}`, which is not a bus \
                 binding in test `{}`",
                t.name.name
            )));
        };
        let xschema = &prog.transactors[xid.index()];
        if xschema.bound_bus.as_deref() != Some(binding.bus.as_str()) {
            return Err(LowerError::Invalid(format!(
                "initiator-BFM `{instance} : {}` is bound to bus binding `{bus_field}` of bus \
                 `{}`, but the transactor is `bound to {}`",
                xschema.name,
                binding.bus,
                xschema.bound_bus.as_deref().unwrap_or("<none>"),
            )));
        }
        // Fill the placeholder bus prefix in the method bodies with the
        // real binding name. The bodies are shared per type; a second
        // bind to a different binding name is rejected (one instance per
        // type per file).
        let method_fns: Vec<usize> = xschema.methods.iter().map(|m| m.function.index()).collect();
        let xname = xschema.name.clone();
        let remap = binding.remap.clone();
        for fidx in method_fns {
            if let Err(prev) =
                fill_initiator_bus_prefix(&mut prog.functions[fidx], bus_field, &remap)
            {
                return Err(unsupported(
                    &format!(
                        "initiator-BFM transactor `{xname}` bound to more than one bus \
                         binding (`{prev}`, `{bus_field}`)"
                    ),
                    "the initiator-BFM subset shares one method body per transactor type; \
                     multiple instances need per-instance bodies",
                ));
            }
        }
        transactor_fields.push((instance.clone(), *xid));
        bare_transactor_fields.insert(instance.clone());
    }
    // Bound-to event-driven component instances (`let xact : X active =
    // bind axil`). Validate the bound bus binding matches the component's
    // `bound_bus`, fill the placeholder bus prefix in the (TYPE-shared)
    // on-handler bodies with the real binding name, and register the
    // instance as a composite-component field so `emit xact.req(t)` and
    // `xact.<state>` read-backs resolve through the component machinery.
    // The handler bodies are shared per component TYPE, so the subset is
    // one bound instance per type per file — a second bind to a different
    // binding would clobber the first's filled prefix, so reject it.
    // Instance names of `active` bound event-driven transactors — their
    // `on <ev>` driver re-lowers into a queue-fed worker coroutine under
    // `--mt`. Carried onto each `ComponentFieldBinding` below.
    let mut active_bound_instances: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for (instance, cid, bus_field, active) in &bound_event_component_binds {
        if *active {
            active_bound_instances.insert(instance.clone());
        }
        // The bound bus binding must be a `let <bus_field> : <Bus> =
        // bind dut` declared in this test, of the component's bound bus.
        let Some(binding) = bus_bindings.iter().find(|b| &b.field == bus_field) else {
            return Err(LowerError::Invalid(format!(
                "bound-to event-driven transactor `{instance}` is bound to `{bus_field}`, which \
                 is not a bus binding in test `{}`",
                t.name.name
            )));
        };
        let cschema = &prog.components[cid.index()];
        if cschema.bound_bus.as_deref() != Some(binding.bus.as_str()) {
            return Err(LowerError::Invalid(format!(
                "bound-to event-driven transactor `{instance} : {}` is bound to bus binding \
                 `{bus_field}` of bus `{}`, but the transactor is `bound to {}`",
                cschema.name,
                binding.bus,
                cschema.bound_bus.as_deref().unwrap_or("<none>"),
            )));
        }
        // Fill the placeholder bus prefix in every handler body: the
        // `on req` driver (on-handlers), the `on bus.<ch>.handshake`
        // monitor bodies (cycle-handlers — their payload-capture DutReads
        // carry the placeholder prefix), and any methods. The fill is a
        // no-op when a body carries no placeholder ref (an agent-mode
        // `dut.<sig>` cycle handler, a non-bus method).
        let body_fns: Vec<usize> = cschema
            .on_handlers
            .iter()
            .map(|h| h.function.index())
            .chain(cschema.cycle_handlers.iter().map(|c| c.function.index()))
            .chain(cschema.methods.iter().map(|m| m.function.index()))
            .collect();
        let cname = cschema.name.clone();
        let remap = binding.remap.clone();
        for fidx in body_fns {
            if let Err(prev) =
                fill_initiator_bus_prefix(&mut prog.functions[fidx], bus_field, &remap)
            {
                return Err(unsupported(
                    &format!(
                        "bound-to event-driven transactor `{cname}` bound to more than one bus \
                         binding (`{prev}`, `{bus_field}`)"
                    ),
                    "the bound event-driven subset shares one handler body per transactor type; \
                     multiple instances need per-instance bodies",
                ));
            }
        }
        // Fill the monitor cycle-handlers' synthesized `valid && ready`
        // triggers too — they live on the schema (rendered standalone in
        // the per-instance `_checkers` closure), not in the function body,
        // so the body fill above does not reach them.
        for ch in prog.components[cid.index()].cycle_handlers.iter_mut() {
            if ch.monitor_channel.is_some() {
                if let Err(prev) =
                    fill_initiator_bus_prefix_expr(&mut ch.trigger, bus_field, &remap)
                {
                    return Err(unsupported(
                        &format!(
                            "bound-to event-driven transactor `{cname}` bound to more than one \
                             bus binding (`{prev}`, `{bus_field}`)"
                        ),
                        "the bound event-driven subset shares one handler body per transactor \
                         type; multiple instances need per-instance bodies",
                    ));
                }
            }
        }
        // Register as a composite-component instance (same machinery as
        // `let env : AnalysisEnv`): `emit xact.req(t)` fires the handler,
        // `xact.<state>` reads the per-instance state.
        test_scope_components.push((instance.clone(), *cid));
    }
    // Composite-component instances → schema bindings (with the env's
    // resolved `connect` edges). A name collision with another binding
    // class would resolve ambiguously, so reject it.
    let mut component_field_map: HashMap<String, ir::ComponentId> = HashMap::new();
    let mut component_field_bindings: Vec<ir::ComponentFieldBinding> = Vec::new();
    for (field, cid) in &test_scope_components {
        if transactor_fields.iter().any(|(f, _)| f == field)
            || bus_binding_decls.contains_key(field)
            || component_field_map.contains_key(field)
        {
            return Err(LowerError::Invalid(format!(
                "name `{field}` is bound more than once in test `{}`",
                t.name.name
            )));
        }
        component_field_map.insert(field.clone(), *cid);
        component_field_bindings.push(ir::ComponentFieldBinding {
            field: field.clone(),
            component: *cid,
            connects: prog.components[cid.index()].connects.clone(),
            active: active_bound_instances.contains(field),
        });
    }

    // Bus bindings (test-scope `let`s) and transactor fields (testbench
    // component fields) share the `CallTarget::TransactorMethod`
    // bus_field namespace; a name living in both would dispatch
    // ambiguously, so reject it here rather than shadow.
    for (field, _) in &transactor_fields {
        if bus_binding_decls.contains_key(field) {
            return Err(LowerError::Invalid(format!(
                "name `{field}` is both a bus binding and a transactor field in test `{}` — \
                 rename one; method calls through it would be ambiguous",
                t.name.name
            )));
        }
    }

    // Resolve regblock bindings now the transactor fields are known: the
    // `via <helper>` instance must be an active transactor field, and
    // that transactor must declare the frontdoor `write(addr, data)` /
    // `read(addr) -> data` methods. Build the per-binding access context
    // (carried in `LowerCtx`) plus the schema (carried for dump-ir).
    let transactor_field_ids: HashMap<&str, ir::TransactorId> = transactor_fields
        .iter()
        .map(|(f, x)| (f.as_str(), *x))
        .collect();
    let mut regblock_bindings_map: HashMap<String, regblock::RegblockBindingCtx> = HashMap::new();
    let mut regblock_binding_schemas: Vec<ir::RegblockBinding> = Vec::new();
    let mut regblock_init_order: Vec<String> = Vec::new();
    for (binding, rbid, helper_field) in &regblock_binds {
        if regblock_bindings_map.contains_key(binding) {
            return Err(LowerError::Invalid(format!(
                "duplicate regblock binding `{binding}` in test `{}`",
                t.name.name
            )));
        }
        // The binding name doubles as the mirror local; a name shared
        // with a bus binding, a transactor field, or a plain test-scope
        // let would shadow ambiguously at the access site. Reject.
        if bus_binding_decls.contains_key(binding)
            || transactor_fields.iter().any(|(f, _)| f == binding)
            || test_let_names.contains(binding)
        {
            return Err(LowerError::Invalid(format!(
                "name `{binding}` is a regblock binding and also a bus binding, transactor \
                 instance, or test-scope let in test `{}` — rename one",
                t.name.name
            )));
        }
        let Some(&xid) = transactor_field_ids.get(helper_field.as_str()) else {
            return Err(unsupported(
                &format!(
                    "regblock binding `{binding}` via `{helper_field}` (not an active \
                     transactor field of the testbench)"
                ),
                "the `via` helper must be a `transactor` instance that pokes the DUT; the \
                 bus-bound helper form is a follow-up slice",
            ));
        };
        // Validate the frontdoor methods exist at the expected arity so
        // a malformed helper fails at lowering, not at C++ compile.
        let xschema = &prog.transactors[xid.index()];
        for (m, n) in [("write", 2usize), ("read", 1usize)] {
            match xschema.method(m) {
                Some(ms) if ms.n_params == n => {}
                Some(ms) => {
                    return Err(LowerError::Invalid(format!(
                        "regblock `via` helper `{}` method `{m}` takes {} argument(s), \
                         the frontdoor needs {n}",
                        xschema.name, ms.n_params
                    )));
                }
                None => {
                    return Err(LowerError::Invalid(format!(
                        "regblock `via` helper `{}` has no `{m}(addr, data)`-style method",
                        xschema.name
                    )));
                }
            }
        }
        let rb = &prog.regblocks[rbid.index()];
        regblock_bindings_map.insert(
            binding.clone(),
            regblock::RegblockBindingCtx {
                record: rb.record,
                helper_field: helper_field.clone(),
                registers: rb.registers.clone(),
            },
        );
        regblock_binding_schemas.push(ir::RegblockBinding {
            field: binding.clone(),
            regblock: *rbid,
            helper_field: helper_field.clone(),
            // Per-register `on regs.REG` write callbacks are lowered after
            // the run/check bodies (their bodies share the test ctx) and
            // back-patched into this entry; empty until then.
            callbacks: Vec::new(),
        });
        regblock_init_order.push(binding.clone());
    }

    // Resolve addrmap bindings: build one shifted-offset mirror local per
    // (non-aliased) instance, sharing the addrmap's `via` helper. The
    // helper validation mirrors the regblock path (must be an active
    // transactor field declaring `write(addr,data)` / `read(addr)`).
    let mut addrmap_bindings_map: HashMap<String, addrmap::AddrmapBindingCtx> = HashMap::new();
    let mut addrmap_init_order: Vec<(String, RecordId)> = Vec::new();
    // Helper maps for instance resolution: regblock type → mirror record
    // id and register table.
    let regblock_record_of: HashMap<String, RecordId> = regblock_ids
        .keys()
        .map(|n| (n.clone(), record_ids[n]))
        .collect();
    let regblock_registers: HashMap<String, Vec<ir::RegRegisterSchema>> = regblock_ids
        .iter()
        .map(|(n, rbid)| (n.clone(), prog.regblocks[rbid.index()].registers.clone()))
        .collect();
    for (binding, amap_name, helper_field) in &addrmap_binds {
        if regblock_bindings_map.contains_key(binding) || addrmap_bindings_map.contains_key(binding)
        {
            return Err(LowerError::Invalid(format!(
                "duplicate regblock/addrmap binding `{binding}` in test `{}`",
                t.name.name
            )));
        }
        if bus_binding_decls.contains_key(binding)
            || transactor_fields.iter().any(|(f, _)| f == binding)
            || test_let_names.contains(binding)
        {
            return Err(LowerError::Invalid(format!(
                "name `{binding}` is an addrmap binding and also a bus binding, transactor \
                 instance, or test-scope let in test `{}` — rename one",
                t.name.name
            )));
        }
        let Some(&xid) = transactor_field_ids.get(helper_field.as_str()) else {
            return Err(unsupported(
                &format!(
                    "addrmap binding `{binding}` via `{helper_field}` (not an active \
                     transactor field of the testbench)"
                ),
                "the `via` helper must be a `transactor` instance that pokes the DUT",
            ));
        };
        let xschema = &prog.transactors[xid.index()];
        for (m, n) in [("write", 2usize), ("read", 1usize)] {
            match xschema.method(m) {
                Some(ms) if ms.n_params == n => {}
                Some(ms) => {
                    return Err(LowerError::Invalid(format!(
                        "addrmap `via` helper `{}` method `{m}` takes {} argument(s), \
                         the frontdoor needs {n}",
                        xschema.name, ms.n_params
                    )));
                }
                None => {
                    return Err(LowerError::Invalid(format!(
                        "addrmap `via` helper `{}` has no `{m}(addr, data)`-style method",
                        xschema.name
                    )));
                }
            }
        }
        let decl = addrmap_decls[amap_name];
        let actx = addrmap::build_binding_ctx(
            binding,
            decl,
            helper_field,
            &regblock_record_of,
            &regblock_registers,
        )?;
        for (key, rec) in &actx.mirror_inits {
            addrmap_init_order.push((key.clone(), *rec));
        }
        addrmap_bindings_map.insert(binding.clone(), actx);
    }

    // Resolve bound-to target-TLM responder binds: the bound bus binding
    // must exist in this test, its bus type must match the transactor's
    // `bound to` bus, and the instance name must be unique. Build the
    // per-instance state map (for test-scope `target.<field>` access) and
    // the actor schemas, and substitute the instance name into the
    // responder bodies' `TransactorState` placeholders (lowered with an
    // empty instance at transactor-decl time, before the bind was known).
    let mut target_tlm_actors: Vec<ir::TargetTlmActorSchema> = Vec::new();
    let mut target_state: HashMap<String, HashMap<String, crate::ir::StateFieldKind>> =
        HashMap::new();
    for (instance, xid, bus_field) in &target_tlm_binds {
        if target_state.contains_key(instance) {
            return Err(LowerError::Invalid(format!(
                "duplicate target-TLM responder instance `{instance}` in test `{}`",
                t.name.name
            )));
        }
        // The bound bus binding must be a `let <bus_field> : <Bus> = bind
        // dut` declared in this test.
        let Some(binding) = bus_bindings.iter().find(|b| &b.field == bus_field) else {
            return Err(LowerError::Invalid(format!(
                "target-TLM responder `{instance}` is bound to `{bus_field}`, which is not a \
                 bus binding in test `{}`",
                t.name.name
            )));
        };
        let xschema = &prog.transactors[xid.index()];
        if xschema.bound_bus.as_deref() != Some(binding.bus.as_str()) {
            return Err(LowerError::Invalid(format!(
                "target-TLM responder `{instance} : {}` is bound to bus binding `{bus_field}` \
                 of bus `{}`, but the transactor is `bound to {}`",
                xschema.name,
                binding.bus,
                xschema.bound_bus.as_deref().unwrap_or("<none>"),
            )));
        }
        // State map for test-scope `target.<field>` access.
        target_state.insert(
            instance.clone(),
            xschema
                .state_fields
                .iter()
                .map(|f| (f.name.clone(), f.kind.clone()))
                .collect(),
        );
        // Fill the instance into the responder bodies' state-access
        // placeholders. The responder `TbFunction`s are shared per
        // transactor TYPE across the whole file, so a second test binding
        // the same transactor to a DIFFERENT instance name would clobber
        // the first test's already-filled bodies. The subset is one
        // passive instance per bound transactor — reject the multi-
        // instance case loudly rather than silently mis-emit.
        let methods: Vec<usize> = xschema
            .target_methods
            .iter()
            .map(|m| m.function.index())
            .collect();
        let xname = xschema.name.clone();
        for fidx in methods {
            if let Err(prev) = fill_transactor_state_instance(&mut prog.functions[fidx], instance) {
                return Err(unsupported(
                    &format!(
                        "bound-to transactor `{xname}` bound to more than one instance \
                         (`{prev}`, `{instance}`)"
                    ),
                    "the target-side TLM subset materializes one passive instance per bound \
                     transactor; multiple instances need per-instance responder bodies",
                ));
            }
        }
        target_tlm_actors.push(ir::TargetTlmActorSchema {
            instance: instance.clone(),
            bus_field: bus_field.clone(),
            transactor: *xid,
        });
    }

    // Resolve persistent state for the unbound DUT-poking transactor
    // instances (`drv : SeqXactor active` where `SeqXactor` declares a
    // `last_read : uint<32>` field). Same per-instance state map as the
    // bound-to target form (for test-scope `drv.last_read` reads).
    //
    // Per-instance state uses an EXPLICIT STATE RECEIVER (#494 P1b): the
    // type-shared method lambda takes a leading `<Type>_state& self_state`
    // parameter and its `TransactorState`/`TransactorStateWrite` nodes
    // stay as empty-instance placeholders — codegen renders an empty
    // instance as `self_state.<field>`. Each call site passes the calling
    // instance's own state struct (`Drv_go(a)` / `Drv_go(b)`), so one
    // shared body serves any number of instances with independent state.
    // This replaces the old fill-the-instance-name-into-the-body scheme,
    // which shared one baked-in name per TYPE and therefore rejected a
    // second active instance (a fill would clobber the first's name).
    //
    // Bound-to TARGET responders (`target_tlm_actors`, handled above) and
    // bound-to INITIATOR BFMs keep their own name-fill path — see the
    // fill loop below, gated on `bound_bus`.
    let mut unbound_state_actors: Vec<(String, ir::TransactorId)> = Vec::new();
    for (field, xid) in &transactor_fields {
        let xschema = &prog.transactors[xid.index()];
        // Bound-to TARGET instances are handled above (they appear in
        // `target_state` via `target_tlm_binds`, NOT in
        // `transactor_fields`); a stateless transactor has no per-instance
        // struct. The only `bound_bus.is_some()` entries here are bound-to
        // INITIATOR BFMs (added to `transactor_fields` alongside the bus-
        // prefix fill), so they share this per-instance state machinery
        // with the unbound DUT-poking form.
        if xschema.state_fields.is_empty() {
            continue;
        }
        let xname = xschema.name.clone();
        let is_passive = passive_transactor_fields.contains(field);
        let is_bound = xschema.bound_bus.is_some();
        if target_state.contains_key(field) {
            return Err(LowerError::Invalid(format!(
                "name `{field}` is both a stateful transactor instance and a target-TLM \
                 responder in test `{}`",
                t.name.name
            )));
        }
        target_state.insert(
            field.clone(),
            xschema
                .state_fields
                .iter()
                .map(|f| (f.name.clone(), f.kind.clone()))
                .collect(),
        );
        // Bound-to INITIATOR BFMs still bake the instance name into their
        // (type-shared) method bodies — those bodies are not invoked
        // through the state-receiver method lambdas, and a bound bus pins
        // one instance per type. The unbound DUT-poking form leaves the
        // placeholders EMPTY so codegen renders them against the per-call
        // `self_state` receiver, letting multiple active instances of one
        // type coexist (#494 P1b). A PASSIVE instance's `when active`
        // methods are never callable, so its bodies are never filled and
        // never receive a receiver either.
        if is_bound && !is_passive {
            let method_fns: Vec<usize> =
                xschema.methods.iter().map(|m| m.function.index()).collect();
            for fidx in method_fns {
                if let Err(prev) = fill_transactor_state_instance(&mut prog.functions[fidx], field) {
                    return Err(unsupported(
                        &format!(
                            "bound-to initiator transactor `{xname}` instantiated more than \
                             once (`{prev}`, `{field}`)"
                        ),
                        "a bound-to initiator BFM pins one instance per transactor type",
                    ));
                }
            }
        }
        unbound_state_actors.push((field.clone(), *xid));
    }

    // ── Closure-hook cluster: `on <obj>.<method> pre/post` method hooks ──
    //
    // Resolve each collected hook to its transactor field + method, then
    // promote the test-scope `let`s the hook bodies capture by reference
    // into `_tb` scalar host fields (host-state promotion). The hook
    // bodies are lowered as `FunctionKind::TestHook` functions AFTER the
    // ctx is built (below, alongside run/check) so they share the same
    // resolution (`_tb.<field>` host state, the firing transactor's
    // `_tb.drv.last_read` state, the method's by-value params). v1 emits
    // these as `<Type>_<method>_pre/_post` `[&]`-capturing closures; the
    // promotion is what lets the function-per-CFG IR express them.
    let transactor_field_map: HashMap<String, TransactorId> =
        transactor_fields.iter().cloned().collect();
    // Resolved hooks: (transactor, method, side, &OnHandler).
    let mut resolved_hooks: Vec<(TransactorId, String, HookSide, &crate::ast::OnHandler)> =
        Vec::new();
    let mut promoted_lets: HashSet<String> = HashSet::new();
    for (side, h) in &method_hook_asts {
        let (xfield, method) = resolve_method_hook_target(&h.event).ok_or_else(|| {
            unsupported(
                "an `on <obj>.<method> pre/post` hook whose `<obj>.<method>` is not \
                 a `<transactor-field>.<method>` access",
                "the hook target must resolve to a method on a transactor testbench field",
            )
        })?;
        let xid = transactor_field_map.get(&xfield).copied().ok_or_else(|| {
            LowerError::Invalid(format!(
                "`on {xfield}.{method}` hook: `{xfield}` is not a transactor testbench field"
            ))
        })?;
        let xschema = prog.transactor(xid);
        if xschema.method(&method).is_none() {
            return Err(LowerError::Invalid(format!(
                "`on {xfield}.{method}` hook: transactor `{}` declares no method `{method}`",
                xschema.name
            )));
        }
        // Promote a captured run-scope `let` read (bare, un-shadowed) in the
        // hook body. Scope-aware (issue #458, same class as #452): an inner
        // same-named `let` shadows only its own lexical scope — the read-site
        // lowering resolves an in-scope local first — so a nested shadow must
        // not suppress a genuine top-level capture. The hook also sees the
        // firing method's args by the same names, so seed the scope with the
        // method's param names (a test-let sharing a param name resolves to
        // the param, not the promoted cell).
        let mut hook_scope = HashSet::new();
        if let Some(m) = xschema.method(&method) {
            for p in &prog.function(m.function).params {
                hook_scope.insert(p.name.clone());
            }
        }
        collect_promotable_check_reads(&h.body, &test_let_names, &hook_scope, &mut promoted_lets);
        resolved_hooks.push((xid, method, *side, h));
    }
    // Resolve `on regs.REG` per-register write callbacks against the
    // regblock bindings. A candidate `on <ident>.<name>` whose `<ident>`
    // is not a regblock binding is NOT a callback — push it back to
    // bare_stmts (it is a cycle-trigger handler, handled there).
    // Resolved callbacks: (binding, register, &OnHandler).
    let mut resolved_reg_cbs: Vec<(String, String, &crate::ast::OnHandler)> = Vec::new();
    for s in &reg_cb_asts {
        let StmtKind::On(h) = &s.kind else {
            unreachable!("collected only StmtKind::On candidates");
        };
        let (binding, reg) = match &*h.event.kind {
            ExprKind::Field { target, name } => match &*target.kind {
                ExprKind::Ident(id) => (id.name.clone(), name.name.clone()),
                _ => unreachable!("collected only `on <ident>.<name>` shapes"),
            },
            _ => unreachable!("collected only `on <ident>.<name>` shapes"),
        };
        let Some(bctx) = regblock_bindings_map.get(&binding) else {
            // Not a regblock binding → a cycle-trigger handler; leave it
            // for the bare-statement path.
            bare_stmts.push(s);
            continue;
        };
        if !bctx.registers.iter().any(|r| r.name == reg) {
            return Err(LowerError::Invalid(format!(
                "`on {binding}.{reg}`: regblock binding `{binding}` declares no register `{reg}`"
            )));
        }
        // A callback body may capture run-scope lets too (host-state
        // promotion, same as method hooks). Scope-aware (issue #458): a
        // nested same-named `let` must not suppress a genuine top-level
        // capture. The callback sees a single `data` param (the observed
        // write value), so seed the scope with it.
        let mut cb_scope = HashSet::new();
        cb_scope.insert("data".to_string());
        collect_promotable_check_reads(&h.body, &test_let_names, &cb_scope, &mut promoted_lets);
        resolved_reg_cbs.push((binding, reg, h));
    }
    // Check-phase host-state promotion: a test-scope `let` that is read
    // (bare) in the `check`/`teardown` body. v1 hoists every test-scope
    // let to `main`-scope so run AND check capture it by reference; the
    // IR splits run and check into separate functions, so a let written
    // in run and read in check needs a shared cell. Promote it to the
    // SAME `_tb` scalar host field the closure-hook path uses (reads →
    // `Expr::TbField`, writes → `Stmt::TbFieldWrite`), so the value
    // persists across the run→check boundary inside the shared `_tb`
    // struct — trace-equivalent, since a `_tb` field write emits no
    // observable trace event, exactly like a v1 shared-scope local
    // mutation. The constant-initializer requirement (enforced below) is
    // a precise narrowing of v1: a non-constant-init check-read let is
    // rejected loudly rather than miscompiled.
    if let Some(s) = scope {
        // Promote a test-scope `let` when the check/teardown phase has at
        // least one *un-shadowed* bare read of it. A same-named `let`
        // declared inside the check body shadows the test-scope let only at
        // read sites within that inner decl's lexical scope — and the
        // read-site lowering (`exprs.rs`) already resolves an in-scope local
        // before the promotion/rejection paths, so a per-site-shadowed read
        // is handled correctly without any promotion. The decision must
        // therefore be scope-aware: the old flat "any nested decl of this
        // name suppresses promotion" rule over-rejected the common case of a
        // top-level check read alongside an unrelated nested shadow (issue
        // #452). Conversely we must NOT promote a name read only where it is
        // shadowed, since promotion drops the run-scope let and would then
        // require a compile-time-constant initializer.
        let empty = HashSet::new();
        if let Some(b) = &s.check {
            collect_promotable_check_reads(b, &test_let_names, &empty, &mut promoted_lets);
        }
        if let Some(b) = &s.teardown {
            collect_promotable_check_reads(b, &test_let_names, &empty, &mut promoted_lets);
        }
    }
    // Promote each captured let: drop its `let` declaration (it becomes a
    // `_tb` field with a default) and register it as a scalar host field.
    // The init must be a compile-time constant (the field's `default`);
    // v1's captured lets in these fixtures all init to literals.
    if !promoted_lets.is_empty() {
        // Register one `_tb` scalar field per promoted let, in declaration
        // order (deterministic schema order).
        for s in &test_let_stmts {
            let StmtKind::Let(l) = &s.kind else { continue };
            if !promoted_lets.contains(&l.name.name) {
                continue;
            }
            let inferred_ty = match l.value.as_ref().map(|v| &*v.kind) {
                Some(ExprKind::Bool(_)) => Some(IrType::Bool),
                _ => None,
            };
            let ty =
                l.ty.as_ref()
                    .and_then(tb_scalar_field_ir_type)
                    .or(inferred_ty)
                    .unwrap_or(IrType::UInt(None));
            let default = match l.value.as_ref().map(|v| &*v.kind) {
                Some(ExprKind::Int(s)) => s.replace('_', "").parse::<u64>().map_err(|_| {
                    LowerError::Invalid(format!(
                        "promoted `let {}` has a non-integer initializer",
                        l.name.name
                    ))
                })?,
                Some(ExprKind::Bool(b)) => *b as u64,
                None => 0,
                _ => {
                    return Err(unsupported(
                        &format!(
                            "a promoted test-scope `let {}` with a non-constant initializer",
                            l.name.name
                        ),
                        "a test-scope let captured by a closure hook OR read in the check phase \
                         is promoted to a `_tb` host field whose default must be a compile-time \
                         constant; assign the computed value in the run body instead",
                    ));
                }
            };
            scalar_fields.push(ir::TbScalarFieldSchema {
                name: l.name.name.clone(),
                ty,
                default,
            });
        }
        // Drop the promoted lets from the hoisted-let list — they are now
        // `_tb` host fields, not run-function locals.
        test_let_stmts.retain(|s| match &s.kind {
            StmtKind::Let(l) => !promoted_lets.contains(&l.name.name),
            _ => true,
        });
    }

    let tb_id = TestbenchId(prog.testbenches.len() as u32);
    prog.testbenches.push(TestbenchSchema {
        name: tb_schema_name,
        dut_field: "dut".to_string(),
        dut_type,
        cov_fields: cov_fields.clone(),
        scalar_fields: scalar_fields.clone(),
        record_fields: record_fields.clone(),
        bus_bindings: bus_bindings.clone(),
        transactor_fields: transactor_fields.clone(),
        passive_transactor_fields: passive_transactor_fields.clone(),
        scoreboard_fields: scoreboard_fields.clone(),
        regblock_bindings: regblock_binding_schemas,
        target_tlm_actors,
        component_fields: component_field_bindings,
        unbound_state_actors,
        synthetic,
        // Back-patched after the handler bodies are lowered (below).
        periodic_services: Vec::new(),
        cycle_services: Vec::new(),
    });

    // Assemble run/check statement lists. Mirrors v1: the coroutine
    // executes setup → run → check → teardown sequentially; the IR
    // splits them into a Run function (setup+run) and a Check function
    // (check+teardown) that backends emit back-to-back.
    // Hoisted test-scope lets first (v1 evaluates them at `main` scope
    // before the coroutine bootstraps — i.e. before any body statement
    // and before the first clock edge), then the body in scope order.
    let mut run_stmts: Vec<&AstStmt> = test_let_stmts.iter().collect();
    let n_hoisted_lets = run_stmts.len();
    let mut check_stmts: Vec<&AstStmt> = Vec::new();
    // Bare statements that precede the `scope` block run before its
    // setup/run (matching v1's single-coroutine item-order emission);
    // those that follow it run after teardown.
    let (bare_before_scope, bare_after_scope) = bare_stmts.split_at(n_bare_before_scope);
    if scope.is_some() {
        run_stmts.extend(bare_before_scope.iter().copied());
    }
    if let Some(s) = scope {
        if let Some(b) = &s.setup {
            collect_stmts(b, !synthetic, &mut run_stmts);
        }
        if let Some(b) = &s.run {
            collect_stmts(b, false, &mut run_stmts);
        }
        if let Some(b) = &s.check {
            collect_stmts(b, false, &mut check_stmts);
        }
        if let Some(b) = &s.teardown {
            collect_stmts(b, false, &mut check_stmts);
        }
        check_stmts.extend(bare_after_scope.iter().copied());
    }
    // Precise rejection for the per-register `on regs.REG ... end on`
    // write callback residual at test scope. This is out of the TB-IR
    // subset (see `regblock::detect_regblock_residual`); without this
    // pass a `record_test`-shaped test trips the generic bare-statement/
    // scope mixing error below, which buries the real reason. The passive
    // `record_write`/`record_read` API itself IS lowered. Scan both bare
    // statements and the scope blocks' bodies.
    {
        let binding_names: std::collections::HashSet<&str> =
            regblock_bindings_map.keys().map(|s| s.as_str()).collect();
        let mut scan: Vec<&AstStmt> = bare_stmts.clone();
        scan.extend(run_stmts.iter().copied().skip(n_hoisted_lets));
        scan.extend(check_stmts.iter().copied());
        for s in &scan {
            if let Some(detail) = regblock::detect_regblock_residual(s, &binding_names) {
                return Err(unsupported(
                    &detail,
                    "a per-register `on regs.REG` write callback lowers to a \
                     reference-capturing closure over run-scope state fired from \
                     inside `record_write`, which the function-per-CFG IR cannot \
                     express (the same blocker as the `axilite_hooks` pre/post \
                     method hooks); the passive `record_write`/`record_read` API, \
                     register-level frontdoor reads/writes, and `bitbash(regs)` ARE \
                     supported",
                ));
            }
        }
    }
    if scope.is_some() && !bare_stmts.is_empty() {
        // A bare `cover` mixed with a `scope` block stays rejected: v1
        // itself emits non-compiling C++ for that case (its `_cov_*_hits`
        // counter is declared inside the per-scope cover-summary block,
        // so a bare cover references an out-of-scope symbol). There is no
        // correct v1 behavior to mirror, so refuse rather than emit
        // something that diverges. General bare statements (e.g. a bare
        // `log(...)`) are routed by item order above and are fine.
        if bare_stmts
            .iter()
            .any(|s| matches!(&s.kind, StmtKind::Cover(_)))
        {
            return Err(unsupported(
                "mixing a bare `cover` statement with a `scope`/`run` block in one test",
                "move the `cover` into the scope's `check` block (a bare cover \
                 alongside a scope has no well-defined lowering — v1 emits \
                 non-compiling C++ for it)",
            ));
        }
        // Other bare statements were already routed into the run/check
        // lists by item order (pre-scope → run front, post-scope → check
        // tail); nothing more to append here.
    } else {
        // No scope: every bare statement is the run body, in order.
        run_stmts.extend(bare_stmts.iter().copied());
    }
    if run_stmts.len() == n_hoisted_lets && check_stmts.is_empty() {
        return Err(LowerError::Invalid(format!(
            "test `{}` has no body — add a `scope sim`, `run`, or bare statements",
            t.name.name
        )));
    }

    // Inline `<phase>()` call sites with the phase block's statements. v1
    // emits each phase as a captured `[&]() -> void` lambda + a plain
    // call; the IR splices the body at the call site (observably identical
    // — the phase body runs in the run/check coroutine context). Recurses
    // so a phase may call another; a cycle is rejected.
    if !phases.is_empty() {
        let mut expanded_run = Vec::with_capacity(run_stmts.len());
        for s in &run_stmts {
            expand_phase_calls(s, &phases, &mut Vec::new(), &mut expanded_run, &t.name.name)?;
        }
        run_stmts = expanded_run;
        let mut expanded_check = Vec::with_capacity(check_stmts.len());
        for s in &check_stmts {
            expand_phase_calls(
                s,
                &phases,
                &mut Vec::new(),
                &mut expanded_check,
                &t.name.name,
            )?;
        }
        check_stmts = expanded_check;
    }

    // Reserve `FunctionId`s for the `on regs.REG` write callbacks so
    // `try_lower_record_write` (run during run/check/callback lowering)
    // can reference the matching callback that hasn't been lowered yet.
    // Pushes after the ctx, in this order: samplers, run, check?, method
    // hooks, then reg callbacks — so the callback base is fixed now.
    let mut regblock_callbacks: HashMap<String, Vec<(String, FunctionId)>> = HashMap::new();
    {
        let n_check = if check_stmts.is_empty() { 0 } else { 1 };
        let cb_base = prog.functions.len()
            + cov_fields.len()
            + 1 // run
            + n_check
            + resolved_hooks.len();
        for (i, (binding, reg, _)) in resolved_reg_cbs.iter().enumerate() {
            regblock_callbacks
                .entry(binding.clone())
                .or_default()
                .push((reg.clone(), FunctionId((cb_base + i) as u32)));
        }
    }

    let ctx = LowerCtx {
        dut_field: "dut".to_string(),
        tb_field: if synthetic {
            None
        } else {
            Some("_tb".to_string())
        },
        cov_fields: cov_fields.iter().cloned().collect(),
        covgroups: prog.covgroups.clone(),
        clock_names: clock_specs.iter().map(|c| c.name.clone()).collect(),
        allow_scheduler_time_waits: true,
        record_ids: record_ids.clone(),
        records: prog.records.clone(),
        bus_bindings: bus_binding_decls,
        bus_remaps: bus_bindings
            .iter()
            .filter(|b| !b.remap.is_empty())
            .map(|b| (b.field.clone(), b.remap.clone()))
            .collect(),
        transactor_fields: transactor_fields.iter().cloned().collect(),
        passive_transactor_fields: passive_transactor_fields.clone(),
        transactors: prog.transactors.clone(),
        scoreboard_fields: scoreboard_fields.iter().cloned().collect(),
        scoreboards: prog.scoreboards.clone(),
        consts: consts.clone(),
        const_signed: const_signed.clone(),
        tb_scalar_fields: scalar_fields.iter().map(|f| f.name.clone()).collect(),
        tb_record_fields: record_fields.clone(),
        regblock_callbacks: regblock_callbacks.clone(),
        tb_methods,
        test_scope_lets: test_let_names,
        regblock_bindings: regblock_bindings_map,
        regblock_init_order,
        addrmap_bindings: addrmap_bindings_map,
        addrmap_init_order,
        bare_transactor_fields,
        target_state,
        components: prog.components.clone(),
        component_fields: component_field_map,
        txn_keeps: txn_keeps.clone(),
        randomize_problem_ids: randomize_problem_ids.clone(),
        tseqs: tseq_records.clone(),
        probes: probes.clone(),
        extern_fns: extern_fns.clone(),
    };

    // Synthesized auto-sampler functions, one per covergroup field, in
    // declaration order. Sampling is schema-driven (the bin counters
    // live in the emitted covergroup struct, outside IR locals), so
    // the function body is empty — the function records the
    // registration slot and covgroup binding for backends.
    for (field, cg) in &cov_fields {
        let id = FunctionId(prog.functions.len() as u32);
        prog.functions.push(TbFunction {
            id,
            name: format!("sample_{}_{}", t.name.name, field),
            kind: FunctionKind::SamplerAuto { covgroup: *cg },
            params: Vec::new(),
            locals: Vec::new(),
            blocks: vec![BasicBlock {
                stmts: Vec::new(),
                terminator: Terminator::Return,
            }],
            entry: BlockId(0),
            owner: Some(tb_id),
            ret: None,
        });
    }

    let run_id = FunctionId(prog.functions.len() as u32);
    let run_fn = lower_function(
        run_id,
        format!("run_{}", t.name.name),
        FunctionKind::Run,
        Some(tb_id),
        &run_stmts,
        &ctx,
        helpers,
        constraint_sites,
    )?;
    prog.functions.push(run_fn);

    let check = if check_stmts.is_empty() {
        None
    } else {
        let check_id = FunctionId(prog.functions.len() as u32);
        let check_fn = lower_function(
            check_id,
            format!("check_{}", t.name.name),
            FunctionKind::Check,
            Some(tb_id),
            &check_stmts,
            &ctx,
            helpers,
            constraint_sites,
        )?;
        prog.functions.push(check_fn);
        Some(check_id)
    };

    // Lower each `on <obj>.<method> pre/post` hook body as a
    // `FunctionKind::TestHook` function sharing the method's parameter
    // signature, then back-patch its FunctionId onto the target method's
    // `pre_hooks`/`post_hooks` (fired at the `emit_method` call site).
    for (xid, method, side, h) in &resolved_hooks {
        // Snapshot the method's typed params (names + IR types) from its
        // lowered body — the hook sees the same args by the same names.
        let mparams: Vec<TypedParam> = {
            let xschema = prog.transactor(*xid);
            let m = xschema.method(method).expect("hook method resolved above");
            prog.function(m.function).params.clone()
        };
        let hook_id = FunctionId(prog.functions.len() as u32);
        let hook_fn = lower_method_hook_body(
            hook_id,
            format!(
                "{}_{method}_{}",
                prog.transactor(*xid).name,
                match side {
                    HookSide::Pre => "pre",
                    HookSide::Post => "post",
                }
            ),
            Some(tb_id),
            &mparams,
            &h.body,
            &ctx,
            helpers,
            constraint_sites,
        )?;
        prog.functions.push(hook_fn);
        // Back-patch onto the method schema.
        let xschema = &mut prog.transactors[xid.index()];
        let m = xschema
            .methods
            .iter_mut()
            .find(|m| &m.name == method)
            .expect("hook method resolved above");
        match side {
            HookSide::Pre => m.pre_hooks.push(hook_id),
            HookSide::Post => m.post_hooks.push(hook_id),
        }
    }

    // Lower each `on regs.REG` per-register write callback as a
    // `FunctionKind::TestHook` function (single `data` param), then
    // back-patch its FunctionId onto the binding's `callbacks` table.
    // `record_write` dispatches the matching callback after the mirror
    // update, guarded by a per-binding recursion-depth counter.
    for (binding, reg, h) in &resolved_reg_cbs {
        let cb_id = FunctionId(prog.functions.len() as u32);
        // Must match the id reserved into `ctx.regblock_callbacks` so the
        // `RecordWriteCb` firing sites reference this exact callback.
        debug_assert!(
            regblock_callbacks
                .get(binding)
                .is_some_and(|v| v.iter().any(|(r, f)| r == reg && *f == cb_id)),
            "reg callback id drift for {binding}.{reg}"
        );
        let cb_fn = lower_reg_cb_body(
            cb_id,
            format!("{}_{binding}_{reg}_cb", t.name.name),
            Some(tb_id),
            &h.body,
            &ctx,
            helpers,
            constraint_sites,
        )?;
        prog.functions.push(cb_fn);
        let tb = &mut prog.testbenches[tb_id.index()];
        let b = tb
            .regblock_bindings
            .iter_mut()
            .find(|b| &b.field == binding)
            .expect("regblock binding resolved above");
        if b.callbacks.iter().any(|(r, _)| r == reg) {
            return Err(LowerError::Invalid(format!(
                "`on {binding}.{reg}`: register `{reg}` already has a write callback"
            )));
        }
        b.callbacks.push((reg.clone(), cb_id));
    }

    // Lower each testbench-scoped `on <N> cycles [phase post_eval]`
    // periodic handler (issue #485). The body is a zero-arg
    // `FunctionKind::TestHook` function (emitted as a free `[&]`-capturing
    // lambda alongside method hooks); the backend registers it into the
    // per-cycle `_checkers` / `_post_eval_services` vector with a
    // last-fire stamp gated on `period`. The period must be a positive
    // integer literal in this subset.
    if !tb_periodic_asts.is_empty() {
        // The testbench decl is needed to rewrite bare field/method refs
        // in the handler body; re-fetch it (the item-walk borrow ended).
        let tb_decl = tb_name
            .as_ref()
            .and_then(|tbn| components.get(tbn).copied())
            .expect("tb_periodic_asts is only populated for an impl-bound testbench");
        let mut periodic_services: Vec<ir::TbPeriodicServiceSchema> = Vec::new();
        for h in &tb_periodic_asts {
            let period = tb_periodic_literal(&h.event).ok_or_else(|| {
                unsupported(
                    &format!(
                        "a testbench-scoped `on <N> cycles` handler in `{}` with a \
                         non-literal period",
                        t.name.name
                    ),
                    "the TB-IR backend requires a positive integer-literal cycle count \
                     (e.g. `on 1 cycles`); a field-backed period is not yet lowered",
                )
            })?;
            let fid = FunctionId(prog.functions.len() as u32);
            let f = lower_tb_periodic_service_body(
                fid,
                format!("{}_tb_periodic_{}", t.name.name, fid.0),
                Some(tb_id),
                tb_decl,
                h,
                &ctx,
                helpers,
                constraint_sites,
            )?;
            prog.functions.push(f);
            periodic_services.push(ir::TbPeriodicServiceSchema {
                period,
                function: fid,
                phase: ir::HandlerPhase::from_ast(h.phase),
            });
        }
        prog.testbenches[tb_id.index()].periodic_services = periodic_services;
    }

    // Lower each testbench-scoped `on <bool-expr> [phase post_eval]`
    // cycle-trigger handler (issue #494 P2b). The body lowers to a zero-arg
    // `FunctionKind::TestHook` function (like the periodic form); the
    // trigger predicate lowers alongside it as a standalone `ir::Expr` that
    // the backend re-evaluates every cycle in a `_checkers` /
    // `_post_eval_services` registration closure, gated on the recorded
    // edge mode. Mirrors v1's `emit_cycle_trigger`.
    if !tb_cycle_asts.is_empty() {
        let tb_decl = tb_name
            .as_ref()
            .and_then(|tbn| components.get(tbn).copied())
            .expect("tb_cycle_asts is only populated for an impl-bound testbench");
        let mut cycle_services: Vec<ir::TbCycleServiceSchema> = Vec::new();
        for h in &tb_cycle_asts {
            let fid = FunctionId(prog.functions.len() as u32);
            let (f, trigger) = lower_tb_cycle_service_body(
                fid,
                format!("{}_tb_cycle_{}", t.name.name, fid.0),
                Some(tb_id),
                tb_decl,
                h,
                &ctx,
                helpers,
                constraint_sites,
            )?;
            prog.functions.push(f);
            cycle_services.push(ir::TbCycleServiceSchema {
                trigger,
                edge: ir::CycleEdge::from_ast(h.edge),
                function: fid,
                phase: ir::HandlerPhase::from_ast(h.phase),
            });
        }
        prog.testbenches[tb_id.index()].cycle_services = cycle_services;
    }

    prog.tests.push(TestSchema {
        name: t.name.name.clone(),
        testbench: tb_id,
        run: run_id,
        check,
        clock_domain: clock_specs.first().and_then(|c| c.domain.clone()),
        clocks: clock_specs,
    });
    Ok(())
}

/// Parse the cycle count of a testbench-scoped `on <N> cycles` periodic
/// handler (issue #485). The parser stashes the period expression in
/// `OnHandler::event`; this subset requires a positive integer literal.
/// Returns `None` for a non-literal or non-positive period.
fn tb_periodic_literal(event: &crate::ast::Expr) -> Option<u64> {
    let v = exprs::parse_int_literal_expr(event)?;
    (v > 0).then_some(v)
}

/// Recognize a bare phase-call statement `<name>()` — `StmtKind::Expr`
/// of a zero-arg `Call` whose callee is a plain identifier naming a
/// declared `phase`. Returns the phase name on a match.
fn phase_call_name<'a>(s: &AstStmt, phases: &HashMap<String, &'a Block>) -> Option<String> {
    let StmtKind::Expr(e) = &s.kind else {
        return None;
    };
    let ExprKind::Call { callee, args } = &*e.kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let ExprKind::Ident(id) = &*callee.kind else {
        return None;
    };
    phases.contains_key(&id.name).then(|| id.name.clone())
}

/// Inline a single statement into `out`, expanding any `<phase>()` call
/// site with the phase block's statements (recursively, so a phase may
/// call another). `active` tracks the phase-expansion stack to reject a
/// recursive cycle. A non-phase-call statement is pushed unchanged.
fn expand_phase_calls<'a>(
    s: &'a AstStmt,
    phases: &HashMap<String, &'a Block>,
    active: &mut Vec<String>,
    out: &mut Vec<&'a AstStmt>,
    test_name: &str,
) -> Result<(), LowerError> {
    if let Some(name) = phase_call_name(s, phases) {
        if active.contains(&name) {
            return Err(LowerError::Invalid(format!(
                "phase `{name}` is called recursively in test `{test_name}`"
            )));
        }
        active.push(name.clone());
        let body = phases[&name];
        for inner in &body.stmts {
            expand_phase_calls(inner, phases, active, out, test_name)?;
        }
        active.pop();
        return Ok(());
    }
    out.push(s);
    Ok(())
}

/// Flatten a block's statements, optionally skipping the synthesized
/// `_tb.dut = dut` wire (it is scaffolding-owned in the IR backend —
/// the emitter wires the TB struct's DUT pointer before the body runs).
fn collect_stmts<'a>(b: &'a Block, skip_tb_wire: bool, out: &mut Vec<&'a AstStmt>) {
    for s in &b.stmts {
        if skip_tb_wire && is_tb_dut_wire(s) {
            continue;
        }
        out.push(s);
    }
}

/// Resolve an `on <obj>.<method> pre/post` hook target expression to
/// `(transactor-field, method)`. The impl-form desugarer has already
/// rewritten the transactor testbench field to `_tb.<field>`, so the
/// event is either `_tb.<field>.<method>` (impl form) or `<field>.<method>`
/// (classic form). Returns `None` for any other shape.
fn resolve_method_hook_target(event: &crate::ast::Expr) -> Option<(String, String)> {
    let ExprKind::Field {
        target,
        name: method,
    } = &*event.kind
    else {
        return None;
    };
    match &*target.kind {
        // Classic form: `<field>.<method>`.
        ExprKind::Ident(field) => Some((field.name.clone(), method.name.clone())),
        // Impl form: `_tb.<field>.<method>`.
        ExprKind::Field {
            target: root,
            name: field,
        } => {
            let ExprKind::Ident(root) = &*root.kind else {
                return None;
            };
            if root.name != "_tb" {
                return None;
            }
            Some((field.name.clone(), method.name.clone()))
        }
        _ => None,
    }
}

/// Scope-aware reader-collection for check-phase / closure-hook
/// host-state promotion (issues #452 and #458). Collects the test-scope
/// `let` names (those in `test_let_names`) that have at least one bare read
/// in this block which is NOT lexically shadowed by an inner same-named
/// `let`.
///
/// `in_scope` carries the names declared in enclosing scopes (and, for a
/// nested block, the enclosing block's earlier decls); a fresh copy is
/// extended as we walk this block's statements in order, so a `let` takes
/// effect only for reads that follow it (point-of-declaration), e.g. the
/// RHS of `let acc = acc + 1` still reads the outer `acc`.
///
/// Statement coverage mirrors the AST read positions (the same set a flat
/// ident-collector would visit), but each read is checked against the live
/// scope rather than flattening every read and every decl into two
/// order-free sets — the old approach conflated a top-level outer read with
/// an unrelated nested shadow.
fn collect_promotable_check_reads(
    b: &Block,
    test_let_names: &HashSet<String>,
    in_scope: &HashSet<String>,
    out: &mut HashSet<String>,
) {
    let mut scope = in_scope.clone();
    for s in &b.stmts {
        collect_promotable_check_reads_in_stmt(s, test_let_names, &mut scope, out);
    }
}

/// Mark every test-scope `let` read in `e` that is not currently in scope.
/// Within a single expression there are no binding forms, so all idents are
/// reads evaluated in the same `scope`.
fn note_promotable_reads_in_expr(
    e: &crate::ast::Expr,
    test_let_names: &HashSet<String>,
    scope: &HashSet<String>,
    out: &mut HashSet<String>,
) {
    let mut idents = HashSet::new();
    collect_idents_in_expr(e, &mut idents);
    for name in idents {
        if test_let_names.contains(&name) && !scope.contains(&name) {
            out.insert(name);
        }
    }
}

fn collect_promotable_check_reads_in_stmt(
    s: &AstStmt,
    test_let_names: &HashSet<String>,
    scope: &mut HashSet<String>,
    out: &mut HashSet<String>,
) {
    use crate::ast::CallArg;
    match &s.kind {
        StmtKind::Let(l) => {
            // The initializer is evaluated before the new binding takes
            // effect, so its reads still see the enclosing `scope`.
            if let Some(v) = &l.value {
                note_promotable_reads_in_expr(v, test_let_names, scope, out);
            }
            scope.insert(l.name.name.clone());
        }
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            note_promotable_reads_in_expr(target, test_let_names, scope, out);
            note_promotable_reads_in_expr(value, test_let_names, scope, out);
        }
        StmtKind::Expr(e) => note_promotable_reads_in_expr(e, test_let_names, scope, out),
        StmtKind::For(f) => {
            note_promotable_reads_in_expr(&f.iter, test_let_names, scope, out);
            let mut body_scope = scope.clone();
            body_scope.insert(f.var.name.clone());
            collect_promotable_check_reads(&f.body, test_let_names, &body_scope, out);
        }
        StmtKind::Repeat(r) => {
            note_promotable_reads_in_expr(&r.count, test_let_names, scope, out);
            collect_promotable_check_reads(&r.body, test_let_names, scope, out);
        }
        StmtKind::Loop(b) => collect_promotable_check_reads(b, test_let_names, scope, out),
        StmtKind::While { cond, body, .. } => {
            note_promotable_reads_in_expr(cond, test_let_names, scope, out);
            collect_promotable_check_reads(body, test_let_names, scope, out);
        }
        StmtKind::If(ifs) => {
            note_promotable_reads_in_expr(&ifs.cond, test_let_names, scope, out);
            collect_promotable_check_reads(&ifs.then_block, test_let_names, scope, out);
            for (c, b) in &ifs.elsifs {
                note_promotable_reads_in_expr(c, test_let_names, scope, out);
                collect_promotable_check_reads(b, test_let_names, scope, out);
            }
            if let Some(b) = &ifs.else_block {
                collect_promotable_check_reads(b, test_let_names, scope, out);
            }
        }
        StmtKind::On(h) => {
            note_promotable_reads_in_expr(&h.event, test_let_names, scope, out);
            collect_promotable_check_reads(&h.body, test_let_names, scope, out);
        }
        StmtKind::Assert(v) | StmtKind::Assume(v) | StmtKind::Cover(v) => {
            if let Some(e) = &v.expr {
                note_promotable_reads_in_expr(e, test_let_names, scope, out);
            }
            if let Some(e) = &v.else_fail {
                note_promotable_reads_in_expr(e, test_let_names, scope, out);
            }
        }
        StmtKind::Log { args, .. } | StmtKind::LogF { args, .. } => {
            for a in args {
                match a {
                    CallArg::Expr(e) | CallArg::Named { value: e, .. } => {
                        note_promotable_reads_in_expr(e, test_let_names, scope, out)
                    }
                }
            }
        }
        StmtKind::Return(opt) => {
            if let Some(e) = opt {
                note_promotable_reads_in_expr(e, test_let_names, scope, out);
            }
        }
        StmtKind::Yield(e) | StmtKind::Release(e) => {
            note_promotable_reads_in_expr(e, test_let_names, scope, out)
        }
        StmtKind::Wait { duration, .. } => {
            note_promotable_reads_in_expr(duration, test_let_names, scope, out)
        }
        StmtKind::WaitUntil { conditions, .. } => {
            for c in conditions {
                note_promotable_reads_in_expr(c, test_let_names, scope, out);
            }
        }
        // Any statement shape not handled above is not a promotion read
        // site in this subset (fork/parallel/schedule/select/randomize/
        // emit). A future shape that becomes one must be added here.
        _ => {}
    }
}

fn collect_idents_in_expr(e: &crate::ast::Expr, out: &mut HashSet<String>) {
    use crate::ast::CallArg;
    match &*e.kind {
        ExprKind::Ident(id) => {
            out.insert(id.name.clone());
        }
        ExprKind::Field { target, .. } => collect_idents_in_expr(target, out),
        ExprKind::Index { target, index } => {
            collect_idents_in_expr(target, out);
            collect_idents_in_expr(index, out);
        }
        ExprKind::BitSlice { target, hi, lo } => {
            collect_idents_in_expr(target, out);
            collect_idents_in_expr(hi, out);
            collect_idents_in_expr(lo, out);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_idents_in_expr(lhs, out);
            collect_idents_in_expr(rhs, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => {
            collect_idents_in_expr(expr, out)
        }
        ExprKind::Paren(inner) => collect_idents_in_expr(inner, out),
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_idents_in_expr(cond, out);
            collect_idents_in_expr(then_branch, out);
            collect_idents_in_expr(else_branch, out);
        }
        ExprKind::Call { callee, args } => {
            collect_idents_in_expr(callee, out);
            for a in args {
                match a {
                    CallArg::Expr(x) | CallArg::Named { value: x, .. } => {
                        collect_idents_in_expr(x, out)
                    }
                }
            }
        }
        ExprKind::Send { target, value } => {
            collect_idents_in_expr(target, out);
            collect_idents_in_expr(value, out);
        }
        ExprKind::ForkCall { call } => collect_idents_in_expr(call, out),
        // Literals / time / other leaf or out-of-subset forms carry no
        // capturable run-scope ident in a closure-hook body.
        _ => {}
    }
}

/// Scalar IR type of a testbench member field (`expected : uint<32>`),
/// or `None` when the type is outside the scalar subset. Mirrors v1's
/// `component_field_c_type` → `txn_field_c_type` C-type choice for
/// the ≤64-bit subset.
pub(super) fn tb_scalar_field_ir_type(t: &TypeExpr) -> Option<IrType> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    let width = match args.first() {
        Some(crate::ast::TypeArg::Expr(e)) => match &*e.kind {
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
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => Some(IrType::UInt(width)),
        BuiltinTy::SInt | BuiltinTy::SIntCap => Some(IrType::SInt(width)),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(IrType::Bool),
        _ => None,
    }
}

/// Simple (last-segment) name of a `Named` type expression, if any.
fn type_simple_name(t: Option<&TypeExpr>) -> Option<&str> {
    match t? {
        TypeExpr::Named { name, .. } => name.segments.last().map(|s| s.name.as_str()),
        _ => None,
    }
}

fn is_tb_dut_wire(s: &AstStmt) -> bool {
    let StmtKind::Assign { target, value } = &s.kind else {
        return false;
    };
    let ExprKind::Field { target: ft, name } = &*target.kind else {
        return false;
    };
    let (ExprKind::Ident(root), ExprKind::Ident(v)) = (&*ft.kind, &*value.kind) else {
        return false;
    };
    root.name == "_tb" && name.name == "dut" && v.name == "dut"
}

pub(crate) fn time_literal_to_ps(s: &str) -> Result<i64, String> {
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

/// Bit width of a probe's scalar type. Probes surface a single SV
/// `logic`/`logic [W-1:0]` through the bind stub, so only scalar types
/// are accepted: `uint<N>`/`sint<N>`/`bits<N>` (width = N), `bit`/`bool`
/// (width = 1). Returns `None` for any aggregate / named type. Mirrors
/// `crate::codegen::sv_stub::sv_type_decl`'s accepted set.
fn probe_scalar_width(t: &TypeExpr) -> Option<u32> {
    let TypeExpr::Builtin { name, args, .. } = t else {
        return None;
    };
    use crate::ast::BuiltinTy;
    match name {
        BuiltinTy::Bit | BuiltinTy::Bool | BuiltinTy::BoolLower => Some(1),
        BuiltinTy::UInt | BuiltinTy::SInt | BuiltinTy::Bits => match args.first()? {
            crate::ast::TypeArg::Expr(e) => match &*e.kind {
                ExprKind::Int(s) => s.replace('_', "").parse::<u32>().ok(),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

// ── Function builder ─────────────────────────────────────────────────

/// Per-test lowering context shared by all of the test's functions.
#[derive(Clone)]
pub(crate) struct LowerCtx {
    /// Test-scope DUT field name (`"dut"`).
    pub dut_field: String,
    /// `Some("_tb")` for impl-form tests (testbench-bound).
    pub tb_field: Option<String>,
    /// Covergroup-typed testbench fields (`cov` → covgroup id).
    pub cov_fields: HashMap<String, ir::CovgroupId>,
    /// Snapshot of the program's covgroup schemas, for point/bin
    /// validation at `cov.<point>.<bin>` lowering sites.
    pub covgroups: Vec<CovgroupSchema>,
    /// Declared clock names in declaration order (index == the
    /// `TestSchema::clocks` / runtime scheduler index). Consulted by
    /// `wait N cycles on <clock>` lowering; empty for clockless tests
    /// and for the file-level pure-helper context (a pure helper can
    /// never contain a wait — waits make a helper impure, so it is
    /// CFG-inlined under the calling test's context instead).
    pub clock_names: Vec<String>,
    /// Method bodies are lowered before binding to a concrete test clock
    /// list, but TBIR emits them as lambdas inside the scheduled test body
    /// where `now_ps` and `eval_clocks_until` are available.
    pub allow_scheduler_time_waits: bool,
    /// Transaction record names → ids, for `let t : TxnType`
    /// resolution (the IR mirror of v1's `transactions` set seeding
    /// let-type resolution).
    pub record_ids: HashMap<String, RecordId>,
    /// Snapshot of the program's record schemas, for field validation
    /// at `t.<field>` lowering sites.
    pub records: Vec<RecordSchema>,
    /// Test-scope bus bindings: binding name → bus declaration (with
    /// any unsupported features already rejected at the bind site).
    /// Empty for the file-level pure-helper context, so a pure helper
    /// can never resolve a bus access — which structurally keeps
    /// `TransactorMethod` call edges out of pure-helper bodies (design
    /// seam rule).
    pub bus_bindings: HashMap<String, crate::ast::BusDecl>,
    /// Per-binding `bind ... with { ch.sig: "port" }` signal remaps
    /// (binding name → v1's `(channel, signal) → flat_port` table).
    /// Consulted when lowering a `<binding>.<channel>.<signal>`
    /// handshake access so the emitted flat name honors the override
    /// instead of the `<binding>_<channel>_<signal>` convention. Empty
    /// for bindings without a `with { ... }` clause and for every
    /// non-test context (helper/method bodies, which carry the
    /// placeholder bus prefix — those are remapped at bind time by
    /// `fill_initiator_bus_prefix`).
    pub bus_remaps: HashMap<String, Vec<((String, String), String)>>,
    /// Transactor-typed testbench fields (`xact` → transactor id), for
    /// `xact.method(...)` call resolution and the `xact.dut = dut`
    /// bind. Disjoint from `bus_bindings` (collision rejected at
    /// testbench-schema construction). Empty for synthetic testbenches,
    /// helper contexts, and transactor method bodies.
    pub transactor_fields: HashMap<String, ir::TransactorId>,
    /// The subset of `transactor_fields` declared `passive`. A call to an
    /// active-only method (`when active`) on a passive instance is
    /// rejected at the call site (a passive instance has no such method),
    /// mirroring v1's "`<m>` is declared inside `when active`" diagnostic.
    pub passive_transactor_fields: HashSet<String>,
    /// Snapshot of the program's transactor schemas, for method
    /// validation at call sites.
    pub transactors: Vec<TransactorSchema>,
    /// Scoreboard-typed testbench fields (`sb` → scoreboard id), for
    /// `sb.<field>` / `sb.<queue>.push(...)` resolution. Empty for
    /// synthetic testbenches (no `_tb` to hold the instance), helper
    /// contexts, and transactor method bodies.
    pub scoreboard_fields: HashMap<String, ScoreboardId>,
    /// Snapshot of the program's scoreboard schemas, for field
    /// validation at op/query sites.
    pub scoreboards: Vec<ScoreboardSchema>,
    /// File-scope named integer constants: `const` declarations plus
    /// enum variant names (variant index, first definition wins —
    /// v1's `enum_variants` rule). Substituted as literals at use
    /// sites; locals shadow (lookup order: local, then const — same
    /// effective shadowing as v1's C++ scoping).
    pub consts: HashMap<String, u64>,
    /// Signedness of file-scope constants, retained alongside the
    /// substituted bit patterns so TB-IR preserves signed operators at
    /// use sites (`const NEG : sint<8> = -1; NEG >> 1`).
    pub const_signed: HashMap<String, bool>,
    /// Scalar testbench field names (`TestbenchSchema::scalar_fields`),
    /// for `_tb.<field>` access lowering.
    pub tb_scalar_fields: HashSet<String>,
    /// Transaction/struct-typed testbench field names and record ids.
    /// Each owning function declares a synthetic record local with the
    /// same name so existing record-field lowering and verification apply;
    /// the backend skips that local declaration/init and binds the name
    /// to one shared test-scope C++ object instead.
    pub tb_record_fields: Vec<(String, RecordId)>,
    /// Per-register `on regs.REG` write callbacks, keyed by regblock
    /// binding name → `(register, callback-FunctionId)`. Consulted by
    /// `try_lower_record_write`: when a `record_write` targets a binding
    /// in this map it lowers to `Stmt::RecordWriteCb` (mirror update +
    /// recursion-depth guard + callback dispatch) instead of a plain
    /// `RecordFieldWrite`. The callback FunctionIds are reserved before
    /// the run/check/callback bodies are lowered so the firing sites can
    /// reference them; the bodies are lowered (and the schema patched)
    /// afterward at the matching reserved ids.
    pub regblock_callbacks: HashMap<String, Vec<(String, FunctionId)>>,
    /// Testbench helper methods (`function`/`hookable` declared inside
    /// the bound testbench), CFG-inlined at `_tb.<m>(...)` call sites
    /// like impure helpers — v1 emits them as `[&]`-capturing lambdas
    /// whose waits tick the same scheduler.
    pub tb_methods: HashMap<String, HookableMethod>,
    /// Test-scope let names (hoisted into the run function). Used for
    /// a precise rejection when the check phase references one — run
    /// and check are separate IR functions, so v1's shared-capture
    /// scoping is not representable.
    pub test_scope_lets: HashSet<String>,
    /// Register-block bindings (`let regs : R = bind <helper>`) →
    /// per-binding access context (mirror record, helper field,
    /// registers). Empty for helper/method/synthetic contexts.
    pub regblock_bindings: HashMap<String, regblock::RegblockBindingCtx>,
    /// Regblock binding names in declaration order. The Run function
    /// declares + `RecordInit`s each mirror local at entry, in this
    /// order — v1 declares the `<Name>_Mirror` struct once at the
    /// hoisted-let site. The mirror is run-scoped (a check-phase access
    /// is a precise rejection, like a test-scope let).
    pub regblock_init_order: Vec<String>,
    /// Addrmap bindings (`let chip : A = bind <helper>`) → per-binding
    /// access context (per-instance mirror locals + shifted register
    /// tables + alias sharing). Empty for helper/method/synthetic
    /// contexts.
    pub addrmap_bindings: HashMap<String, addrmap::AddrmapBindingCtx>,
    /// Distinct addrmap mirror locals to declare + `RecordInit` at the
    /// head of the Run function, in declaration order: `(mangled local
    /// name, mirror record id)`. Aliased instances are absent (they
    /// share their target's local).
    pub addrmap_init_order: Vec<(String, RecordId)>,
    /// Transactor instances declared as test-scope lets (`let h :
    /// Xactor active`) rather than testbench fields. Accessed by their
    /// BARE name (`h.method(...)`, `h.dut = dut`) — the impl-for
    /// desugaring rewrites testbench-field access to `_tb.<field>` but
    /// leaves test-scope lets unqualified, so resolution must accept
    /// both shapes. A subset of `transactor_fields` keys.
    pub bare_transactor_fields: HashSet<String>,
    /// Bound-to target-transactor instances → their persistent state
    /// fields (name → kind). Populated at test binding for `passive`
    /// instances of `transactor X bound to <Bus>` transactors. Resolves
    /// test-scope reads/writes `target.<field>` to `ir::Expr::
    /// TransactorState` / `ir::Stmt::TransactorStateWrite` (scalar) and
    /// `target.<queue>.size()`/`.empty()`/`.pop()`/`.push()` to the
    /// state-queue ops. Empty everywhere else.
    pub target_state: HashMap<String, HashMap<String, crate::ir::StateFieldKind>>,
    /// Snapshot of the program's component schemas, for path/field/method
    /// resolution at access sites.
    pub components: Vec<ComponentSchema>,
    /// Test-scope composite-component instances (`let env : AnalysisEnv`)
    /// → `ComponentId`. A bare access whose head segment is in this map
    /// resolves through the component path machinery (`env.source.publish`,
    /// `env.sb.count`). Empty in helper/method/transactor contexts.
    pub component_fields: HashMap<String, ir::ComponentId>,
    /// Per-transaction `keep` constraint clauses as AST expressions, by
    /// transaction name. Merged ahead of a `randomize(t)` call-site
    /// `with {...}` body (v1's spec-§4 merge) when building the
    /// `ConstraintSite`. Empty for keep-free transactions and for
    /// contexts that cannot host a `randomize` (pure helpers).
    pub txn_keeps: HashMap<String, Vec<crate::ast::Expr>>,
    /// Randomize-target span → typed constraint-problem id. The handle
    /// (`ConstraintProblemId.0`) the constraint-IR layer assigned to the
    /// site, keyed exactly like v1's `runtime_randomize_problem_ids`.
    /// `None` at a site means no Z3-ready problem (lower/backend error).
    pub randomize_problem_ids: HashMap<(usize, usize), u32>,
    /// `tseq` name → element type (`TseqElem::Record`/`TseqElem::Scalar`).
    /// A `let txns = Name(args)` whose callee is in this map lowers to a
    /// `CallTarget::Tseq` whose result types the local as the element's
    /// `RecordSeq`/`Seq` (`TseqElem::seq_type`), and a `for t in txns` over
    /// such a local lowers to a counted loop over `txns`.
    pub tseqs: HashMap<String, TseqElem>,
    /// DUT-internal `probe` declarations on `let dut` (probe name →
    /// metadata). A `dut.<name>` access whose head is the DUT and whose
    /// segment is a probe name lowers to a `PortRef` with
    /// `access = Probe` (read-only) or `Force` (force-capable) instead of
    /// the default `Port`. Empty for probe-less tests and every non-test
    /// context (helpers, methods). See docs/probe-signals.md.
    pub probes: HashMap<String, ProbeMeta>,
    /// `extern function name(...) -> ret` (spec §9) names. A call whose
    /// callee is in this set lowers to `CallTarget::ExternFn` and emits
    /// with the RAW symbol name (resolved at link via `--ref-src`); the
    /// forward declaration is emitted file-scope by
    /// `emit_extern_fn_decls`. Visible in EVERY context (test bodies,
    /// helpers, methods) — an extern fn is a pure scalar C function, so
    /// it is callable wherever a pure helper is. Empty when the program
    /// declares no extern fns.
    pub extern_fns: HashSet<String>,
}

/// Lowered metadata for one `probe` / `probe force` declaration on
/// `let dut`. `force` selects `PortAccess::Force` (write-capable via the
/// SV procedural-force stub) vs `PortAccess::Probe` (read-only);
/// `width` is the declared probe type's bit width (for the `PortRef`).
#[derive(Debug, Clone)]
pub(crate) struct ProbeMeta {
    pub force: bool,
    pub width: Option<u32>,
}

pub(crate) struct LoopFrame {
    pub continue_to: BlockId,
    pub break_to: BlockId,
}

/// One in-flight helper inline (innermost last). While a frame is
/// active, name lookup is fenced to scopes opened inside the frame
/// (helpers are free functions — they do not capture caller locals),
/// `break`/`continue` cannot bind to caller loops, and `return e`
/// becomes `Assign(ret_dest, e); Jump(ret_cont)`.
pub(crate) struct InlineFrame {
    /// Helper name, for recursion detection.
    pub(crate) name: String,
    /// True for `_tb.<method>` frames. Unlike free helpers, testbench
    /// methods capture testbench-owned host state.
    pub(crate) is_testbench_method: bool,
    /// Param names bound to the caller's DUT — `as_port_ref` resolves
    /// `<alias>.<port>` exactly like `dut.<port>`.
    pub(crate) dut_aliases: HashSet<String>,
    pub(crate) ret_dest: LocalId,
    pub(crate) ret_cont: BlockId,
    /// `scopes.len()` at frame entry — lookup floor.
    pub(crate) scope_floor: usize,
    /// `loop_stack.len()` at frame entry — break/continue floor.
    pub(crate) loop_floor: usize,
}

struct BlockInProgress {
    stmts: Vec<ir::Stmt>,
    term: Option<Terminator>,
}

pub(crate) struct FuncBuilder<'a> {
    pub(crate) ctx: &'a LowerCtx,
    pub(crate) helpers: &'a helpers::HelperRegistry<'a>,
    locals: Vec<TypedLocal>,
    local_names: HashSet<String>,
    scopes: Vec<HashMap<String, LocalId>>,
    blocks: Vec<BlockInProgress>,
    current: usize,
    pub(crate) loop_stack: Vec<LoopFrame>,
    temp_counter: u32,
    pub(crate) inline_frames: Vec<InlineFrame>,
    /// Synthetic locals for transaction/struct-typed testbench fields.
    /// These are declared in every owning function so record-field IR can
    /// type-check, but codegen binds the names to shared test-scope C++
    /// objects. Kept separate from ordinary lexical scopes so `_tb.cur`
    /// can still name the field when a method parameter/local shadows
    /// bare `cur`.
    tb_record_locals: HashMap<String, LocalId>,
    /// Return slot when lowering a standalone pure-helper body.
    pub(crate) helper_ret: Option<LocalId>,
    /// True while lowering a standalone pure-helper body — record
    /// locals are rejected there (pure helpers emit as file-scope
    /// uint64-only C++ functions in the tbir backend).
    pub(crate) in_pure_helper: bool,
    /// True while lowering `${...}` captures of a log/fail message —
    /// impure helper calls cannot inline there (messages evaluate
    /// lazily at the failure site).
    pub(crate) in_fmt_args: bool,
    /// True while lowering a transactor method body. Methods keep v1's
    /// synchronous hookable semantics (waits emit as `tick()` loops),
    /// so the constructs whose sync emission is out of this slice —
    /// clock-qualified waits and timed `wait until` — are rejected
    /// here.
    pub(crate) in_transactor_method: bool,
    /// Name of the DUT-poking transactor whose method body is currently
    /// being lowered. Used to resolve bare sibling method calls like
    /// `idle()` inside `write()`.
    pub(crate) self_transactor: Option<String>,
    /// Full sibling method signature table for the current transactor,
    /// including methods declared later in source order.
    pub(crate) self_transactor_methods: HashMap<String, (usize, bool, bool)>,
    /// True while lowering a transactor method declared under
    /// `when active`. Used to reject an always-on method that would
    /// backdoor-call an active-only sibling.
    pub(crate) self_transactor_method_active_only: bool,
    /// Name of the function/method body currently being lowered when a
    /// diagnostic wants to cite it directly.
    pub(crate) current_body_name: Option<String>,
    /// State fields visible to a bound-to target-responder body
    /// (`thread bus.<m>(...)`), mapping each field name to its kind
    /// (scalar / queue). A bare ident that hits this map lowers to the
    /// matching state op — a scalar reads/writes `ir::Expr::
    /// TransactorState`/`ir::Stmt::TransactorStateWrite`; a `queue<T>`
    /// field routes `.push`/`.pop`/`.size`/`.empty` to the state-queue
    /// ops — all with an empty `instance` placeholder that the test-
    /// binding stage fills once the passive transactor field is resolved.
    /// Empty in every non-responder context, so the resolution path is
    /// inert.
    pub(crate) target_state_fields: HashMap<String, crate::ir::StateFieldKind>,
    /// True while lowering a Check-kind function — used for the
    /// precise test-scope-let rejection (see `LowerCtx::
    /// test_scope_lets`).
    pub(crate) in_check: bool,
    /// Best-effort bit widths of locals with an explicit scalar type
    /// annotation (`let s64 : uint<64> = ...`). Consulted only by the
    /// width-method receiver inference (v1's `let_widths`); the
    /// declared `TypedLocal::ty` deliberately stays `Unknown` (see
    /// docs/tbir-mvp.md divergence 4).
    pub(crate) let_widths: HashMap<LocalId, u32>,
    /// `Some(component)` while lowering a `ComponentMethod` body: a bare
    /// field name that names a field of that component resolves self-
    /// relatively (`Expr::ComponentField { base: SelfField }`), and
    /// `emit <ev>(...)` resolves against the body's `out event` fields.
    pub(crate) self_component: Option<ir::ComponentId>,
    /// Program-wide constraint-site table, shared across every function
    /// lowered for one program so `ConstraintRef` indices are globally
    /// unique. A `randomize` site appends here and the resulting index
    /// becomes the terminator's `ConstraintRef`. `lower_program` drains
    /// it into `TbProgram::constraint_sites` after all functions lower.
    pub(crate) constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
    /// Payload-field bindings for `recv()`-captured locals: `let r =
    /// bus.<ch>.recv()` records `r → [(field, captured-local)]` so a
    /// later `r.<field>` read resolves to the per-field captured local
    /// (v1 captures the whole payload struct; the IR captures each
    /// payload signal into its own local). The bare local (`r`) still
    /// holds the FIRST payload signal — preserving scalar `recv()`
    /// reads (`let v = bus.r.recv(); assert v == ...`) — so this map is
    /// consulted only for the dotted `r.<field>` form.
    pub(crate) recv_payloads: HashMap<LocalId, Vec<(String, LocalId)>>,

    /// The `RecordSeq` accumulator local of the `tseq` body currently
    /// being lowered (`Some` only inside a `FunctionKind::Tseq` body). A
    /// `yield t` lowers to `Stmt::SeqPush { seq: this, value: t }`; a
    /// `yield` reaching lowering with `None` is rejected (yield outside a
    /// tseq), matching v1's "`yield` outside a `tseq` body" error.
    pub(crate) tseq_result: Option<LocalId>,

    /// `fork bus.<method>(...)` descriptors issued since the last
    /// `join_all`, in issue order — drained into the next
    /// `Stmt::TlmJoinAll` (v1's `pending_tlm_forks`). Empty between
    /// join_alls.
    pub(crate) pending_tlm_forks: Vec<crate::ir::TlmForkDesc>,
    /// Next OOO request tag per `(bus_field, method)`, allocated when a
    /// fork issues against an `out_of_order tags N` method (v1's
    /// `next_tlm_fork_tag`). Function-scoped — a fresh builder starts at
    /// 0, matching v1's per-test reset.
    pub(crate) next_tlm_fork_tag: HashMap<(String, String), u64>,
}

#[allow(clippy::too_many_arguments)]
fn lower_function<'a>(
    id: FunctionId,
    name: String,
    kind: FunctionKind,
    owner: Option<TestbenchId>,
    stmts: &[&AstStmt],
    ctx: &'a LowerCtx,
    helpers: &'a helpers::HelperRegistry<'a>,
    constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.in_check = kind == FunctionKind::Check;
    declare_tb_record_fields(&mut b, ctx);
    // Regblock mirror locals: declared + default-constructed (to their
    // reset values) at the head of the Run function, mirroring v1's
    // single `<Name>_Mirror regs;` declaration at the hoisted-let site.
    // Run-scoped — a check-phase regblock access fails the binding
    // lookup and is rejected precisely (like a test-scope let).
    if kind == FunctionKind::Run {
        for binding in &ctx.regblock_init_order {
            let rec = ctx.regblock_bindings[binding].record;
            let id = b.declare(binding);
            b.set_local_type(id, IrType::Record(rec));
            b.push(ir::Stmt::RecordInit(id, rec));
        }
        // Addrmap per-instance mirror locals: one default-constructed
        // record local per non-aliased instance (alias instances share
        // their target's cell). Declared with the mangled
        // `__addrmap_<chip>_<inst>` name the access resolution computes.
        for (key, rec) in &ctx.addrmap_init_order {
            let id = b.declare(key);
            b.set_local_type(id, IrType::Record(*rec));
            b.push(ir::Stmt::RecordInit(id, *rec));
        }
    }
    for s in stmts {
        b.lower_stmt(s)?;
    }
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    b.finish(id, name, kind, owner)
}

/// Lower one closure-hook body (`on <obj>.<method> pre/post`) as a
/// `FunctionKind::TestHook` function. The hook sees the firing method's
/// args by the same names, so `params` are pre-declared as locals before
/// the body; everything else resolves through the TEST scope's `ctx`
/// (promoted `_tb` host fields, the firing transactor's state via the
/// desugared `_tb.<inst>.<field>` form, regblock bindings). Mirrors v1's
/// `<Type>_<method>_pre/_post` `[&]`-capturing closure body.
#[allow(clippy::too_many_arguments)]
fn lower_method_hook_body<'a>(
    id: FunctionId,
    name: String,
    owner: Option<TestbenchId>,
    params: &[TypedParam],
    body: &Block,
    ctx: &'a LowerCtx,
    helpers: &'a helpers::HelperRegistry<'a>,
    constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    reserve_tb_record_names(&mut b, ctx);
    for p in params {
        let local = b.declare(&p.name);
        b.set_local_type(local, p.ty.clone());
    }
    declare_tb_record_fields(&mut b, ctx);
    b.lower_block_stmts(body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let mut f = b.finish(id, name, FunctionKind::TestHook, owner)?;
    f.params = params.to_vec();
    Ok(f)
}

/// Lower one testbench-scoped `on <N> cycles ... end on` periodic-handler
/// body (issue #485) as a zero-param `FunctionKind::TestHook` function.
/// The body is first rewritten (bare testbench field/method references →
/// `_tb.<name>`) so it resolves through the ordinary TEST-scope `ctx`
/// exactly like the bound test body — `_tb.<field>` host state, `dut`
/// pokes/reads, and `_tb.<m>()` helper inlining. The firing cadence
/// (`period`) and the phase are recorded on the schema, not here; the
/// registration closure the backend emits gates on `cycle_count`.
#[allow(clippy::too_many_arguments)]
fn lower_tb_periodic_service_body<'a>(
    id: FunctionId,
    name: String,
    owner: Option<TestbenchId>,
    tb: &ComponentDecl,
    h: &crate::ast::OnHandler,
    ctx: &'a LowerCtx,
    helpers: &'a helpers::HelperRegistry<'a>,
    constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut body = h.body.clone();
    crate::codegen::cpp_tb::rewrite_testbench_scope_body(&mut body, tb, &HashSet::new());
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    b.lower_block_stmts(&body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    b.finish(id, name, FunctionKind::TestHook, owner)
}

/// Lower one testbench-scoped `on <bool-expr> ... end on` cycle-trigger
/// handler (issue #494 P2b) as a zero-param `FunctionKind::TestHook`
/// function PLUS its standalone trigger predicate. Both the body and the
/// trigger are first rewritten (bare testbench field/method refs →
/// `_tb.<name>`) so they resolve through the ordinary TEST-scope `ctx`
/// exactly like the bound test body — `_tb.<field>` host state and `dut`
/// reads. The trigger uses `lower_expr` (NOT `_no_ports`): it renders
/// standalone in the backend's registration closure, never appended to
/// this body, so it must not hoist a port read into a body-only temp local
/// (that local would dangle in the closure). The firing cadence (`edge`,
/// `phase`) is recorded on the schema, not here.
#[allow(clippy::too_many_arguments)]
fn lower_tb_cycle_service_body<'a>(
    id: FunctionId,
    name: String,
    owner: Option<TestbenchId>,
    tb: &ComponentDecl,
    h: &crate::ast::OnHandler,
    ctx: &'a LowerCtx,
    helpers: &'a helpers::HelperRegistry<'a>,
    constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
) -> Result<(TbFunction, ir::Expr), LowerError> {
    // Rewrite the trigger predicate the same way the body is rewritten:
    // wrap it in a throwaway one-statement block so the shared
    // `rewrite_testbench_scope_body` walker maps bare field refs to
    // `_tb.<field>` and leaves `dut.<sig>` / `_tb` alone.
    let mut trigger_expr = h.event.clone();
    let mut trigger_wrapper = Block {
        stmts: vec![crate::ast::Stmt {
            kind: StmtKind::Expr(trigger_expr),
            span: h.event.span,
        }],
        span: h.event.span,
    };
    crate::codegen::cpp_tb::rewrite_testbench_scope_body(&mut trigger_wrapper, tb, &HashSet::new());
    let StmtKind::Expr(rewritten) = trigger_wrapper.stmts.remove(0).kind else {
        unreachable!("trigger wrapper holds exactly the Expr statement we inserted");
    };
    trigger_expr = rewritten;

    let mut body = h.body.clone();
    crate::codegen::cpp_tb::rewrite_testbench_scope_body(&mut body, tb, &HashSet::new());
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    let trigger = b.lower_expr(&trigger_expr)?;
    b.lower_block_stmts(&body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let f = b.finish(id, name, FunctionKind::TestHook, owner)?;
    Ok((f, trigger))
}

/// Lower one `on regs.REG` per-register write callback body as a
/// `FunctionKind::TestHook` function. The callback sees the observed
/// value as a single `data` param (v1's `[&](uint64_t data)` closure).
/// The body calls the passive `record_write`/`record_read` API, so the
/// regblock mirror locals are declared at entry (same as the Run
/// function) — the backend declares the mirror struct + recursion-depth
/// counter ONCE at test scope (shared by `[&]` capture between the run
/// coroutine and every callback), so these per-function mirror locals
/// resolve to that one captured struct by name.
#[allow(clippy::too_many_arguments)]
fn lower_reg_cb_body<'a>(
    id: FunctionId,
    name: String,
    owner: Option<TestbenchId>,
    body: &Block,
    ctx: &'a LowerCtx,
    helpers: &'a helpers::HelperRegistry<'a>,
    constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, constraint_sites);
    reserve_tb_record_names(&mut b, ctx);
    // The callback param `data` — the observed write value. Declared
    // FIRST so it is param index 0.
    let data = b.declare("data");
    b.set_local_type(data, IrType::UInt(None));
    declare_tb_record_fields(&mut b, ctx);
    // Regblock + addrmap mirror locals, same as the Run function — the
    // callback body's `record_write`/`record_read` resolve through them,
    // and the RecordInit defines the local for the def/use verifier. The
    // BACKEND skips emitting both the declaration and the RecordInit for a
    // callback-bearing (shared) mirror: it is declared + default-
    // constructed ONCE at test scope and captured, so re-initializing it
    // on every callback entry would wipe the mirror mid-run.
    for binding in &ctx.regblock_init_order {
        let rec = ctx.regblock_bindings[binding].record;
        let id = b.declare(binding);
        b.set_local_type(id, IrType::Record(rec));
        b.push(ir::Stmt::RecordInit(id, rec));
    }
    for (key, rec) in &ctx.addrmap_init_order {
        let id = b.declare(key);
        b.set_local_type(id, IrType::Record(*rec));
        b.push(ir::Stmt::RecordInit(id, *rec));
    }
    b.lower_block_stmts(body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let mut f = b.finish(id, name, FunctionKind::TestHook, owner)?;
    f.params = vec![TypedParam {
        name: "data".to_string(),
        ty: IrType::UInt(None),
    }];
    Ok(f)
}

fn declare_tb_record_fields(b: &mut FuncBuilder<'_>, ctx: &LowerCtx) {
    for (name, rec) in &ctx.tb_record_fields {
        let id = b.declare_tb_record_field(name, *rec);
        b.push(ir::Stmt::RecordInit(id, *rec));
    }
}

fn reserve_tb_record_names(b: &mut FuncBuilder<'_>, ctx: &LowerCtx) {
    for (name, _) in &ctx.tb_record_fields {
        b.reserve_local_name(name);
    }
}

impl<'a> FuncBuilder<'a> {
    pub(crate) fn new(
        ctx: &'a LowerCtx,
        helpers: &'a helpers::HelperRegistry<'a>,
        constraint_sites: &'a RefCell<Vec<ConstraintSite>>,
    ) -> Self {
        FuncBuilder {
            ctx,
            helpers,
            locals: Vec::new(),
            local_names: HashSet::new(),
            scopes: vec![HashMap::new()],
            blocks: vec![BlockInProgress {
                stmts: Vec::new(),
                term: None,
            }],
            current: 0,
            loop_stack: Vec::new(),
            temp_counter: 0,
            inline_frames: Vec::new(),
            tb_record_locals: HashMap::new(),
            helper_ret: None,
            in_pure_helper: false,
            in_fmt_args: false,
            in_transactor_method: false,
            self_transactor: None,
            self_transactor_methods: HashMap::new(),
            self_transactor_method_active_only: false,
            current_body_name: None,
            target_state_fields: HashMap::new(),
            in_check: false,
            let_widths: HashMap::new(),
            self_component: None,
            constraint_sites,
            recv_payloads: HashMap::new(),
            tseq_result: None,
            pending_tlm_forks: Vec::new(),
            next_tlm_fork_tag: HashMap::new(),
        }
    }

    /// Mark the `RecordSeq` accumulator of the tseq body being lowered, so
    /// `yield` knows its push target.
    pub(crate) fn set_tseq_result(&mut self, acc: LocalId) {
        self.tseq_result = Some(acc);
    }
}

impl FuncBuilder<'_> {
    pub(crate) fn new_block(&mut self) -> BlockId {
        self.blocks.push(BlockInProgress {
            stmts: Vec::new(),
            term: None,
        });
        BlockId((self.blocks.len() - 1) as u32)
    }

    /// Make `b` the current insertion block. The block must not be
    /// terminated yet.
    pub(crate) fn start_block(&mut self, b: BlockId) {
        debug_assert!(self.blocks[b.index()].term.is_none());
        self.current = b.index();
    }

    pub(crate) fn push(&mut self, s: ir::Stmt) {
        debug_assert!(self.blocks[self.current].term.is_none());
        self.blocks[self.current].stmts.push(s);
    }

    pub(crate) fn terminate(&mut self, t: Terminator) {
        debug_assert!(self.blocks[self.current].term.is_none());
        self.blocks[self.current].term = Some(t);
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.blocks[self.current].term.is_some()
    }

    /// After a `break`/`continue`/`return` mid-block, trailing source
    /// statements are dead code; they lower into a fresh block that the
    /// reachability prune in `finish` removes.
    pub(crate) fn ensure_open_block(&mut self) {
        if self.is_terminated() {
            let b = self.new_block();
            self.start_block(b);
        }
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn scope_depth(&self) -> usize {
        self.scopes.len()
    }

    /// True when `name` refers to the caller's DUT in the current
    /// context: the test-scope DUT field itself (helpers see it the
    /// way v1's `[&]`-capturing lambdas do), or — inside an inline
    /// frame — a helper parameter bound to the DUT at the call site.
    pub(crate) fn is_dut_name(&self, name: &str) -> bool {
        if let Some(f) = self.inline_frames.last() {
            if f.dut_aliases.contains(name) {
                return true;
            }
        }
        name == self.ctx.dut_field
    }

    /// Declare a new local for a source name. Shadowed / reused names
    /// get a deduplicated stored name so backends can emit them as
    /// identifiers directly.
    pub(crate) fn declare(&mut self, source_name: &str) -> LocalId {
        let base = if source_name == "_" {
            "_anon".to_string()
        } else {
            source_name.to_string()
        };
        let mut candidate = base.clone();
        let mut n = 2;
        while !self.local_names.insert(candidate.clone()) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(TypedLocal {
            name: candidate,
            ty: IrType::Unknown,
        });
        self.scopes
            .last_mut()
            .expect("scope stack never empty")
            .insert(source_name.to_string(), id);
        id
    }

    pub(crate) fn declare_tb_record_field(&mut self, source_name: &str, rec: RecordId) -> LocalId {
        self.local_names.insert(source_name.to_string());
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(TypedLocal {
            name: source_name.to_string(),
            ty: IrType::Record(rec),
        });
        if self.lookup(source_name).is_none() {
            self.scopes
                .last_mut()
                .expect("scope stack never empty")
                .insert(source_name.to_string(), id);
        }
        self.tb_record_locals.insert(source_name.to_string(), id);
        id
    }

    pub(crate) fn reserve_local_name(&mut self, source_name: &str) {
        self.local_names.insert(source_name.to_string());
    }

    pub(crate) fn fresh_temp(&mut self) -> LocalId {
        let name = loop {
            let candidate = format!("__t{}", self.temp_counter);
            self.temp_counter += 1;
            if self.local_names.insert(candidate.clone()) {
                break candidate;
            }
        };
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(TypedLocal {
            name,
            ty: IrType::Unknown,
        });
        id
    }

    /// Name resolution, fenced at the innermost inline frame: helpers
    /// are free functions, so an inlined body must not see the caller's
    /// locals — only scopes opened inside the frame (its params and its
    /// own `let`s) resolve.
    pub(crate) fn lookup(&self, name: &str) -> Option<LocalId> {
        let floor = self
            .inline_frames
            .last()
            .map_or(0, |f| f.scope_floor.min(self.scopes.len()));
        for scope in self.scopes[floor..].iter().rev() {
            if let Some(id) = scope.get(name) {
                return Some(*id);
            }
        }
        None
    }

    /// Lookup for transaction/struct-typed testbench fields captured by
    /// `_tb.<method>` bodies. Ordinary `lookup` intentionally fences
    /// inlined helper bodies from caller locals; testbench record fields
    /// are different because they are declared as shared test-scope host
    /// state and must remain visible inside captured testbench methods.
    /// A local declared inside the method body still shadows BARE access
    /// because callers try `lookup` first, while `_tb.<field>` uses this
    /// table directly.
    pub(crate) fn lookup_tb_record_field(&self, name: &str) -> Option<LocalId> {
        self.tb_record_locals.get(name).copied()
    }

    pub(crate) fn in_testbench_method_frame(&self) -> bool {
        self.inline_frames
            .last()
            .map_or(false, |f| f.is_testbench_method)
    }

    /// Lookup shared transaction/struct-typed testbench fields only from
    /// contexts that model v1's testbench capture: the run/check/hook body
    /// itself, or an inlined `_tb.<method>` body. Free helpers remain
    /// fenced from caller/testbench locals.
    pub(crate) fn lookup_tb_record_field_in_capture_scope(&self, name: &str) -> Option<LocalId> {
        let can_capture = self.inline_frames.is_empty() || self.in_testbench_method_frame();
        can_capture
            .then(|| self.lookup_tb_record_field(name))
            .flatten()
    }

    pub(crate) fn tb_scalar_field_in_capture_scope(&self, name: &str) -> Option<String> {
        let can_capture = self.inline_frames.is_empty() || self.in_testbench_method_frame();
        (can_capture && self.ctx.tb_scalar_fields.contains(name)).then(|| name.to_string())
    }

    pub(crate) fn set_local_type(&mut self, l: LocalId, ty: IrType) {
        self.locals[l.index()].ty = ty;
    }

    pub(crate) fn local_type(&self, l: LocalId) -> &IrType {
        &self.locals[l.index()].ty
    }

    /// `Some(record)` when the local is record-typed (`let t : Txn`).
    pub(crate) fn record_of_local(&self, l: LocalId) -> Option<ir::RecordId> {
        match self.locals[l.index()].ty {
            IrType::Record(r) => Some(r),
            _ => None,
        }
    }

    /// `Some(component)` when the local is a component value
    /// (a component-typed method parameter, `IrType::Component`).
    pub(crate) fn component_of_local(&self, l: LocalId) -> Option<ir::ComponentId> {
        match self.locals[l.index()].ty {
            IrType::Component(c) => Some(c),
            _ => None,
        }
    }

    /// `Some(element type)` when the local is a transaction-sequence
    /// (`let txns = SomeTseq(...)`, typed `RecordSeq`/`Seq`). The element
    /// type is what a `for x in <seq>` loop variable binds to: a record
    /// (`IrType::Record`) for a `RecordSeq`, or the boxed scalar for a `Seq`.
    pub(crate) fn seq_of_local(&self, l: LocalId) -> Option<IrType> {
        match &self.locals[l.index()].ty {
            IrType::RecordSeq(r) => Some(IrType::Record(*r)),
            IrType::Seq(elem) => Some((**elem).clone()),
            _ => None,
        }
    }

    /// Append a constraint site to the program-wide table and return its
    /// `ConstraintRef` handle (the index). Used by `randomize` lowering.
    pub(crate) fn push_constraint_site(&self, site: ConstraintSite) -> ConstraintRef {
        let mut sites = self.constraint_sites.borrow_mut();
        let id = ConstraintRef(sites.len() as u32);
        sites.push(site);
        id
    }

    /// Seal all blocks, prune the ones unreachable from the entry
    /// (block 0), and remap successor ids.
    fn finish(
        self,
        id: FunctionId,
        name: String,
        kind: FunctionKind,
        owner: Option<TestbenchId>,
    ) -> Result<TbFunction, LowerError> {
        // A `fork bus.<m>(...)` with no matching `join_all` would leave
        // its request side hanging forever (the response is never
        // drained). v1 silently drops un-joined forks at test end; the
        // IR rejects precisely instead of mis-lowering.
        if let Some(p) = self.pending_tlm_forks.first() {
            return Err(LowerError::Invalid(format!(
                "`fork {}.{}(...)` has no matching `join_all` before the end of `{name}`",
                p.bus_field, p.method
            )));
        }
        let sealed: Vec<BasicBlock> = self
            .blocks
            .into_iter()
            .map(|b| BasicBlock {
                stmts: b.stmts,
                terminator: b.term.unwrap_or(Terminator::Return),
            })
            .collect();

        // Reachability from block 0.
        let mut reachable = vec![false; sealed.len()];
        let mut work = vec![0usize];
        while let Some(i) = work.pop() {
            if std::mem::replace(&mut reachable[i], true) {
                continue;
            }
            for s in sealed[i].terminator.successors() {
                work.push(s.index());
            }
        }
        let mut remap = vec![BlockId(0); sealed.len()];
        let mut kept = Vec::new();
        for (i, b) in sealed.into_iter().enumerate() {
            if reachable[i] {
                remap[i] = BlockId(kept.len() as u32);
                kept.push(b);
            }
        }
        for b in &mut kept {
            remap_terminator(&mut b.terminator, &remap);
        }

        Ok(TbFunction {
            id,
            name,
            kind,
            params: Vec::<TypedParam>::new(),
            locals: self.locals,
            blocks: kept,
            entry: BlockId(0),
            owner,
            ret: self.helper_ret,
        })
    }
}

fn remap_terminator(t: &mut Terminator, remap: &[BlockId]) {
    let m = |b: &mut BlockId| *b = remap[b.index()];
    match t {
        Terminator::Jump(b) => m(b),
        Terminator::Branch(_, a, b) => {
            m(a);
            m(b);
        }
        Terminator::WaitCycles(_, _, b) => m(b),
        Terminator::WaitCyclesSync(_, b) => m(b),
        Terminator::WaitTimePs(_, b) => m(b),
        Terminator::WaitUntil { succ, .. } => m(succ),
        Terminator::WaitUntilTimeout {
            on_fire,
            on_timeout,
            ..
        } => {
            m(on_fire);
            m(on_timeout);
        }
        Terminator::Randomize { succ, .. } => m(succ),
        Terminator::Return | Terminator::Fatal(_) => {}
    }
}

/// Fill the bound responder instance name into a target-method body's
/// `TransactorState` / `TransactorStateWrite` placeholders (lowered with
/// an empty instance at transactor-decl time). The responder bodies are
/// shared per transactor TYPE across the file, so this is idempotent for
/// the same instance but `Err(prev)` when a state node was already filled
/// with a DIFFERENT instance (a second test binding the same transactor
/// to another name) — the caller turns that into an `Unsupported`. The
/// scan-then-fill split keeps the body un-mutated on the error path.
fn fill_transactor_state_instance(func: &mut TbFunction, instance: &str) -> Result<(), String> {
    if let Some(prev) = existing_state_instance(func) {
        if prev != instance {
            return Err(prev);
        }
    }
    fill_transactor_state_instance_unchecked(func, instance);
    Ok(())
}

/// First non-empty instance name already present on any `TransactorState`
/// / `TransactorStateWrite` node in the body, or `None` (all empty
/// placeholders, the common single-bind case).
fn existing_state_instance(func: &TbFunction) -> Option<String> {
    fn in_expr(e: &ir::Expr) -> Option<String> {
        match e {
            ir::Expr::TransactorState { instance, .. }
            | ir::Expr::TransactorStateRecordField { instance, .. }
            | ir::Expr::TransactorStateQueueQuery { instance, .. }
                if !instance.is_empty() =>
            {
                Some(instance.clone())
            }
            ir::Expr::Binary(_, a, b) => in_expr(a).or_else(|| in_expr(b)),
            ir::Expr::Unary(_, a) | ir::Expr::WidthCast { inner: a, .. } => in_expr(a),
            ir::Expr::BitSlice { target, .. } => in_expr(target),
            ir::Expr::Ternary(c, t, f) => in_expr(c).or_else(|| in_expr(t)).or_else(|| in_expr(f)),
            ir::Expr::Call(_, args) => args.iter().find_map(in_expr),
            // Component fields never carry a transactor-state instance.
            ir::Expr::ComponentField { .. } => None,
            _ => None,
        }
    }
    for block in &func.blocks {
        for s in &block.stmts {
            let found = match s {
                ir::Stmt::TransactorStateWrite {
                    instance, value, ..
                }
                | ir::Stmt::TransactorStateRecordFieldWrite {
                    instance, value, ..
                } => {
                    if !instance.is_empty() {
                        Some(instance.clone())
                    } else {
                        in_expr(value)
                    }
                }
                ir::Stmt::TransactorStateQueuePush {
                    instance, value, ..
                } => {
                    if !instance.is_empty() {
                        Some(instance.clone())
                    } else {
                        in_expr(value)
                    }
                }
                ir::Stmt::TransactorStateQueuePop { instance, .. } => {
                    (!instance.is_empty()).then(|| instance.clone())
                }
                ir::Stmt::Assign(_, e) | ir::Stmt::DutWrite(_, e) => in_expr(e),
                ir::Stmt::RecordFieldWrite { value, .. }
                | ir::Stmt::RecordWriteCb { value, .. }
                | ir::Stmt::TbFieldWrite { value, .. } => in_expr(value),
                ir::Stmt::AssertCheck { cond, on_fail } => {
                    in_expr(cond).or_else(|| on_fail.args.iter().find_map(|a| in_expr(&a.expr)))
                }
                ir::Stmt::Log { args, .. } | ir::Stmt::FailDiag { args, .. } => {
                    args.args.iter().find_map(|a| in_expr(&a.expr))
                }
                ir::Stmt::TransactorCall { call, .. }
                | ir::Stmt::TransactorSelfCall { call, .. } => in_expr(call),
                ir::Stmt::ScoreboardOp { op, .. } => match op {
                    ir::ScoreboardOp::QueuePush { value, .. }
                    | ir::ScoreboardOp::ScalarWrite { value, .. } => in_expr(value),
                    ir::ScoreboardOp::QueuePop { .. } => None,
                },
                // Component-method bodies never reach this TLM target-
                // state filler (they are not bound-to target responders);
                // any expr they carry holds no transactor-state node.
                ir::Stmt::ComponentFieldWrite { value, .. } => in_expr(value),
                ir::Stmt::ComponentEmit { args, .. } => args.iter().find_map(in_expr),
                ir::Stmt::ComponentCall { args, .. } => args.iter().find_map(in_expr),
                // tseq bodies never appear in a bound-to responder body
                // (transactor-method randomize / tseq is out of subset),
                // so the yielded value holds no transactor-state node.
                ir::Stmt::SeqPush { value, .. } | ir::Stmt::ComponentQueuePush { value, .. } => {
                    in_expr(value)
                }
                ir::Stmt::ComponentQueuePop { .. } | ir::Stmt::ComponentSubAssign { .. } => None,
                // Fork/join descriptors carry their request payload exprs;
                // a responder body never forks, but scan for completeness.
                ir::Stmt::TlmFork(desc) => desc.args.iter().find_map(in_expr),
                ir::Stmt::TlmJoinAll(pending) => {
                    pending.iter().find_map(|p| p.args.iter().find_map(in_expr))
                }
                ir::Stmt::DutRead(_, _)
                | ir::Stmt::RecordInit(_, _)
                | ir::Stmt::CovReport(_)
                | ir::Stmt::ProbeRelease(_) => None,
            };
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

fn fill_transactor_state_instance_unchecked(func: &mut TbFunction, instance: &str) {
    fn fill_expr(e: &mut ir::Expr, instance: &str) {
        match e {
            ir::Expr::TransactorState { instance: i, .. }
            | ir::Expr::TransactorStateRecordField { instance: i, .. }
            | ir::Expr::TransactorStateQueueQuery { instance: i, .. } => {
                debug_assert!(
                    i.is_empty() || i == instance,
                    "target-state instance already filled with a different name"
                );
                *i = instance.to_string();
            }
            ir::Expr::Binary(_, a, b) => {
                fill_expr(a, instance);
                fill_expr(b, instance);
            }
            ir::Expr::Unary(_, a) => fill_expr(a, instance),
            ir::Expr::BitSlice { target, .. } => fill_expr(target, instance),
            ir::Expr::Ternary(c, t, f) => {
                fill_expr(c, instance);
                fill_expr(t, instance);
                fill_expr(f, instance);
            }
            ir::Expr::WidthCast { inner, .. } => fill_expr(inner, instance),
            ir::Expr::ComponentIdle { n, .. } => fill_expr(n, instance),
            ir::Expr::SeqIndex { index, .. } => fill_expr(index, instance),
            ir::Expr::RecordField {
                index: Some(idx), ..
            } => fill_expr(idx, instance),
            ir::Expr::CovHookParam {
                index: Some(idx), ..
            } => fill_expr(idx, instance),
            ir::Expr::Call(_, args) => {
                for a in args {
                    fill_expr(a, instance);
                }
            }
            ir::Expr::Literal { .. }
            | ir::Expr::WideLiteral(_)
            | ir::Expr::Local(_)
            | ir::Expr::CycleCount
            | ir::Expr::ErrorCount
            | ir::Expr::Port(_)
            | ir::Expr::RecordField { index: None, .. }
            | ir::Expr::CovHookParam { index: None, .. }
            | ir::Expr::CovHookArg { .. }
            | ir::Expr::TbField(_)
            | ir::Expr::ComponentField { .. }
            | ir::Expr::ComponentValue { .. }
            | ir::Expr::ScoreboardQuery { .. }
            | ir::Expr::ComponentQueueQuery { .. }
            | ir::Expr::SeqLen(_)
            | ir::Expr::RegRead { .. }
            | ir::Expr::CovBin { .. } => {}
        }
    }
    fn fill_term(t: &mut Terminator, instance: &str) {
        match t {
            Terminator::Branch(c, _, _) => fill_expr(c, instance),
            Terminator::WaitCycles(n, _, _) | Terminator::WaitCyclesSync(n, _) => {
                fill_expr(n, instance)
            }
            Terminator::WaitUntil { preds, .. } => {
                for p in preds {
                    fill_expr(&mut p.expr, instance);
                }
            }
            Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                for p in preds {
                    fill_expr(&mut p.expr, instance);
                }
                fill_expr(cycles, instance);
            }
            Terminator::Fatal(args) => {
                for a in &mut args.args {
                    fill_expr(&mut a.expr, instance);
                }
            }
            // Randomize carries no `TransactorState` placeholders, and
            // never appears in a responder body (transactor-method
            // randomize is out of subset) — nothing to fill.
            Terminator::Randomize { .. }
            | Terminator::Jump(_)
            | Terminator::WaitTimePs(_, _)
            | Terminator::Return => {}
        }
    }
    for block in &mut func.blocks {
        for s in &mut block.stmts {
            match s {
                ir::Stmt::TransactorStateWrite {
                    instance: i, value, ..
                }
                | ir::Stmt::TransactorStateRecordFieldWrite {
                    instance: i, value, ..
                }
                | ir::Stmt::TransactorStateQueuePush {
                    instance: i, value, ..
                } => {
                    debug_assert!(
                        i.is_empty() || i == instance,
                        "target-state-write instance already filled with a different name"
                    );
                    *i = instance.to_string();
                    fill_expr(value, instance);
                }
                ir::Stmt::TransactorStateQueuePop { instance: i, .. } => {
                    debug_assert!(
                        i.is_empty() || i == instance,
                        "target-state-pop instance already filled with a different name"
                    );
                    *i = instance.to_string();
                }
                ir::Stmt::Assign(_, e) | ir::Stmt::DutWrite(_, e) => fill_expr(e, instance),
                ir::Stmt::RecordFieldWrite { value, .. }
                | ir::Stmt::RecordWriteCb { value, .. }
                | ir::Stmt::TbFieldWrite { value, .. } => fill_expr(value, instance),
                ir::Stmt::AssertCheck { cond, on_fail } => {
                    fill_expr(cond, instance);
                    for a in &mut on_fail.args {
                        fill_expr(&mut a.expr, instance);
                    }
                }
                ir::Stmt::Log { args, .. } | ir::Stmt::FailDiag { args, .. } => {
                    for a in &mut args.args {
                        fill_expr(&mut a.expr, instance);
                    }
                }
                ir::Stmt::TransactorCall { call, .. }
                | ir::Stmt::TransactorSelfCall { call, .. } => fill_expr(call, instance),
                ir::Stmt::ScoreboardOp { op, .. } => match op {
                    ir::ScoreboardOp::QueuePush { value, .. }
                    | ir::ScoreboardOp::ScalarWrite { value, .. } => fill_expr(value, instance),
                    ir::ScoreboardOp::QueuePop { .. } => {}
                },
                ir::Stmt::ComponentFieldWrite { value, .. } => fill_expr(value, instance),
                ir::Stmt::ComponentEmit { args, .. } => {
                    for a in args {
                        fill_expr(a, instance);
                    }
                }
                ir::Stmt::ComponentCall { args, .. } => {
                    for a in args {
                        fill_expr(a, instance);
                    }
                }
                ir::Stmt::SeqPush { value, .. } => fill_expr(value, instance),
                ir::Stmt::ComponentQueuePush { value, .. } => fill_expr(value, instance),
                ir::Stmt::ComponentQueuePop { .. } | ir::Stmt::ComponentSubAssign { .. } => {}
                ir::Stmt::TlmFork(desc) => {
                    for a in &mut desc.args {
                        fill_expr(a, instance);
                    }
                }
                ir::Stmt::TlmJoinAll(pending) => {
                    for p in pending {
                        for a in &mut p.args {
                            fill_expr(a, instance);
                        }
                    }
                }
                ir::Stmt::DutRead(_, _)
                | ir::Stmt::RecordInit(_, _)
                | ir::Stmt::CovReport(_)
                | ir::Stmt::ProbeRelease(_) => {}
            }
        }
        fill_term(&mut block.terminator, instance);
    }
}

/// Fill the placeholder bus-binding prefix
/// (`transactors::INITIATOR_BUS_PLACEHOLDER`) in an initiator-side BFM
/// method body with the real bus binding name (the arch-com §19.6 flat
/// prefix). The body was lowered before the test's `let helper = bind
/// <binding>` named the binding, so every `bus.<ch>.<sig>` access carries
/// a `PortRef` whose first path segment is the placeholder.
///
/// Idempotent under the same binding (re-filling with the identical name
/// is a no-op). The method bodies are shared per transactor TYPE, so a
/// second bind to a DIFFERENT binding returns the previously filled name
/// (`Err`) — the one-instance-per-type subset gate, mirroring
/// `fill_transactor_state_instance`.
/// Rewrite (or check, when `rewrite == false`) the placeholder bus prefix
/// of a single `PortRef`. Shared by `fill_initiator_bus_prefix` (function
/// bodies) and `fill_initiator_bus_prefix_expr` (schema-resident exprs
/// like a monitor cycle-handler's synthesized trigger). Returns the first
/// conflicting (already-rewritten-to-a-different-binding) prefix it sees
/// via `conflict`.
fn fill_visit_port(
    p: &mut crate::ir::PortRef,
    placeholder: &str,
    binding: &str,
    remap: &[((String, String), String)],
    rewrite: bool,
    conflict: &mut Option<String>,
) {
    match p.port_path.first() {
        Some(seg) if seg == placeholder => {
            if rewrite {
                // A 3-segment `[placeholder, channel, signal]` handshake
                // path collapses to the `bind ... with { ch.sig: "port" }`
                // override — a single-segment full flat port name — when
                // `(channel, signal)` is mapped; otherwise the placeholder
                // is rewritten to the binding name (the canonical
                // `<bind>_<ch>_<sig>` convention). Mirrors v1's
                // `bus_signal_name`, which remaps the channel form only.
                if p.port_path.len() == 3 {
                    if let Some((_, port)) = remap
                        .iter()
                        .find(|((rch, rsig), _)| rch == &p.port_path[1] && rsig == &p.port_path[2])
                    {
                        p.port_path = vec![port.clone()];
                        return;
                    }
                }
                p.port_path[0] = binding.to_string();
            }
        }
        // A non-placeholder, non-`binding` prefix means a prior bind
        // already rewrote this shared body to a different name.
        Some(seg) if seg != binding && conflict.is_none() => {
            *conflict = Some(seg.clone());
        }
        _ => {}
    }
}

/// Recursively fill/check the placeholder bus prefix of every `PortRef`
/// inside an `Expr`. See `fill_visit_port`.
fn fill_visit_expr(
    e: &mut crate::ir::Expr,
    placeholder: &str,
    binding: &str,
    remap: &[((String, String), String)],
    rewrite: bool,
    conflict: &mut Option<String>,
) {
    use crate::ir::Expr;
    match e {
        Expr::Port(p) => fill_visit_port(p, placeholder, binding, remap, rewrite, conflict),
        Expr::Binary(_, a, b) => {
            fill_visit_expr(a, placeholder, binding, remap, rewrite, conflict);
            fill_visit_expr(b, placeholder, binding, remap, rewrite, conflict);
        }
        Expr::Unary(_, a)
        | Expr::BitSlice { target: a, .. }
        | Expr::WidthCast { inner: a, .. }
        | Expr::ComponentIdle { n: a, .. } => {
            fill_visit_expr(a, placeholder, binding, remap, rewrite, conflict)
        }
        Expr::Ternary(c, t, f) => {
            fill_visit_expr(c, placeholder, binding, remap, rewrite, conflict);
            fill_visit_expr(t, placeholder, binding, remap, rewrite, conflict);
            fill_visit_expr(f, placeholder, binding, remap, rewrite, conflict);
        }
        Expr::Call(_, args) => {
            for a in args {
                fill_visit_expr(a, placeholder, binding, remap, rewrite, conflict);
            }
        }
        Expr::SeqIndex { index, .. } => {
            fill_visit_expr(index, placeholder, binding, remap, rewrite, conflict)
        }
        Expr::RecordField {
            index: Some(idx), ..
        } => fill_visit_expr(idx, placeholder, binding, remap, rewrite, conflict),
        Expr::CovHookParam {
            index: Some(idx), ..
        } => fill_visit_expr(idx, placeholder, binding, remap, rewrite, conflict),
        Expr::Literal { .. }
        | Expr::WideLiteral(_)
        | Expr::Local(_)
        | Expr::CycleCount
        | Expr::ErrorCount
        | Expr::RecordField { index: None, .. }
        | Expr::CovHookParam { index: None, .. }
        | Expr::CovHookArg { .. }
        | Expr::TbField(_)
        | Expr::TransactorState { .. }
        | Expr::TransactorStateRecordField { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::ComponentField { .. }
        | Expr::ComponentValue { .. }
        | Expr::ScoreboardQuery { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::SeqLen(_)
        | Expr::RegRead { .. }
        | Expr::CovBin { .. } => {}
    }
}

/// Fill the placeholder bus prefix in a single schema-resident expression
/// (a monitor cycle-handler's synthesized `valid && ready` trigger, which
/// lives on the schema rather than in a function body, so the body-walking
/// `fill_initiator_bus_prefix` does not reach it). Same one-instance-per-
/// type conflict gate.
fn fill_initiator_bus_prefix_expr(
    e: &mut crate::ir::Expr,
    binding: &str,
    remap: &[((String, String), String)],
) -> Result<(), String> {
    let placeholder = transactors::INITIATOR_BUS_PLACEHOLDER;
    let mut conflict = None;
    fill_visit_expr(e, placeholder, binding, remap, false, &mut conflict);
    if let Some(prev) = conflict {
        return Err(prev);
    }
    fill_visit_expr(e, placeholder, binding, remap, true, &mut conflict);
    Ok(())
}

fn fill_initiator_bus_prefix(
    func: &mut TbFunction,
    binding: &str,
    remap: &[((String, String), String)],
) -> Result<(), String> {
    use crate::ir::Stmt;
    let placeholder = transactors::INITIATOR_BUS_PLACEHOLDER;

    // Every PortRef carried by the body, whether at statement level
    // (`DutWrite`/`DutRead`), inside an expression (`Expr::Port` — bus
    // signal reads in wait predicates / assert conditions / format args
    // / DutWrite values), or in a terminator (a `Branch`/`WaitUntil`
    // condition reading a bus signal), prefixes its flat path with the
    // placeholder. A faithful fill must reach all of them — partial
    // coverage would leave a `bus_<ch>_<sig>` name with the wrong (still-
    // placeholder) prefix in the emitted C++.
    //
    // `run` walks every PortRef the body carries (statements + expressions
    // + terminators) via the shared `fill_visit_*` helpers, over both a
    // check pass (detect a prior fill to a DIFFERENT binding → the one-
    // instance-per-type gate) and the rewrite pass.
    let visit_port = fill_visit_port;
    let visit_expr = fill_visit_expr;
    let mut run = |rewrite: bool| -> Option<String> {
        let mut conflict = None;
        for block in &mut func.blocks {
            for s in &mut block.stmts {
                match s {
                    Stmt::DutWrite(p, e) => {
                        visit_port(p, placeholder, binding, remap, rewrite, &mut conflict);
                        visit_expr(e, placeholder, binding, remap, rewrite, &mut conflict);
                    }
                    Stmt::DutRead(_, p) | Stmt::ProbeRelease(p) => {
                        visit_port(p, placeholder, binding, remap, rewrite, &mut conflict)
                    }
                    Stmt::Assign(_, e)
                    | Stmt::RecordFieldWrite { value: e, .. }
                    | Stmt::RecordWriteCb { value: e, .. }
                    | Stmt::TbFieldWrite { value: e, .. }
                    | Stmt::TransactorStateWrite { value: e, .. }
                    | Stmt::TransactorStateRecordFieldWrite { value: e, .. }
                    | Stmt::ComponentFieldWrite { value: e, .. } => {
                        visit_expr(e, placeholder, binding, remap, rewrite, &mut conflict)
                    }
                    Stmt::AssertCheck { cond, on_fail } => {
                        visit_expr(cond, placeholder, binding, remap, rewrite, &mut conflict);
                        for a in &mut on_fail.args {
                            visit_expr(
                                &mut a.expr,
                                placeholder,
                                binding,
                                remap,
                                rewrite,
                                &mut conflict,
                            );
                        }
                    }
                    Stmt::Log { args, .. } | Stmt::FailDiag { args, .. } => {
                        for a in &mut args.args {
                            visit_expr(
                                &mut a.expr,
                                placeholder,
                                binding,
                                remap,
                                rewrite,
                                &mut conflict,
                            );
                        }
                    }
                    Stmt::TransactorCall { call, .. } | Stmt::TransactorSelfCall { call, .. } => {
                        visit_expr(call, placeholder, binding, remap, rewrite, &mut conflict)
                    }
                    Stmt::ScoreboardOp { op, .. } => match op {
                        ir::ScoreboardOp::QueuePush { value, .. }
                        | ir::ScoreboardOp::ScalarWrite { value, .. } => {
                            visit_expr(value, placeholder, binding, remap, rewrite, &mut conflict)
                        }
                        ir::ScoreboardOp::QueuePop { .. } => {}
                    },
                    Stmt::ComponentEmit { args, .. } | Stmt::ComponentCall { args, .. } => {
                        for a in args {
                            visit_expr(a, placeholder, binding, remap, rewrite, &mut conflict);
                        }
                    }
                    Stmt::SeqPush { value, .. }
                    | Stmt::ComponentQueuePush { value, .. }
                    | Stmt::TransactorStateQueuePush { value, .. } => {
                        visit_expr(value, placeholder, binding, remap, rewrite, &mut conflict)
                    }
                    Stmt::ComponentQueuePop { .. }
                    | Stmt::ComponentSubAssign { .. }
                    | Stmt::TransactorStateQueuePop { .. } => {}
                    Stmt::TlmFork(desc) => {
                        for a in &mut desc.args {
                            visit_expr(a, placeholder, binding, remap, rewrite, &mut conflict);
                        }
                    }
                    Stmt::TlmJoinAll(pending) => {
                        for p in pending {
                            for a in &mut p.args {
                                visit_expr(a, placeholder, binding, remap, rewrite, &mut conflict);
                            }
                        }
                    }
                    Stmt::RecordInit(_, _) | Stmt::CovReport(_) => {}
                }
            }
            match &mut block.terminator {
                Terminator::Branch(c, _, _) => {
                    visit_expr(c, placeholder, binding, remap, rewrite, &mut conflict)
                }
                Terminator::WaitCycles(n, _, _) | Terminator::WaitCyclesSync(n, _) => {
                    visit_expr(n, placeholder, binding, remap, rewrite, &mut conflict)
                }
                Terminator::WaitUntil { preds, .. } => {
                    for p in preds {
                        visit_expr(
                            &mut p.expr,
                            placeholder,
                            binding,
                            remap,
                            rewrite,
                            &mut conflict,
                        );
                    }
                }
                Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                    for p in preds {
                        visit_expr(
                            &mut p.expr,
                            placeholder,
                            binding,
                            remap,
                            rewrite,
                            &mut conflict,
                        );
                    }
                    visit_expr(cycles, placeholder, binding, remap, rewrite, &mut conflict);
                }
                Terminator::Fatal(args) => {
                    for a in &mut args.args {
                        visit_expr(
                            &mut a.expr,
                            placeholder,
                            binding,
                            remap,
                            rewrite,
                            &mut conflict,
                        );
                    }
                }
                Terminator::Randomize { .. }
                | Terminator::Jump(_)
                | Terminator::WaitTimePs(_, _)
                | Terminator::Return => {}
            }
        }
        conflict
    };
    // Check pass: a prior fill to a different binding is the multi-
    // instance gate. (Idempotent re-fill with the same binding is fine.)
    if let Some(prev) = run(false) {
        return Err(prev);
    }
    // Rewrite pass.
    run(true);
    Ok(())
}

#[cfg(test)]
mod time_literal_tests {
    use super::time_literal_to_ps;

    /// Largest value per unit whose picosecond conversion still fits in
    /// i64 (i64::MAX / factor), and the smallest rejected (one above).
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
