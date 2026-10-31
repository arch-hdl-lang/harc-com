//! Pure numeric decisions shared with the formal proof harnesses.
//! AST traversal, diagnostics and emitter state stay with their callers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    // arithmetic
    Add,
    AddWrap,
    Sub,
    SubWrap,
    Mul,
    MulWrap,
    Div,
    Mod,
    // comparison
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    // logical
    AndAnd,
    OrOr,
    AndKw,
    OrKw,
    // bitwise
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    // temporal SVA
    PipeImplies,     // |->
    PipeImpliesNext, // |=>
    Throughout,
    Within,
    Intersect,
    // membership
    In,
    Inside,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConstraintOperandSignedness {
    Signed,
    Unsigned,
    UntypedLiteral,
}

/// Declared-width signedness policy used by both native solver renderers.
/// Unknown widths deliberately keep the previous conservative defaults.
pub(crate) fn solver_binary_is_signed(
    left: ConstraintOperandSignedness,
    right: ConstraintOperandSignedness,
    left_width: Option<u32>,
    right_width: Option<u32>,
) -> bool {
    use self::ConstraintOperandSignedness::{Signed, Unsigned};
    if left == Unsigned || right == Unsigned {
        let (signed_width, unsigned_width) = match (left, right) {
            (Signed, Unsigned) => (left_width, right_width),
            (Unsigned, Signed) => (right_width, left_width),
            _ => return false,
        };
        let signed_width = signed_width.unwrap_or(0);
        let unsigned_width = unsigned_width.unwrap_or(u32::MAX);
        return (signed_width < 32 && unsigned_width < 32) || signed_width > unsigned_width;
    }
    left == Signed || right == Signed
}

// The 64-bit constant kernel below is what proofs/arithmetic/kani.rs
// proves. Constant folding now evaluates exactly at ARCH result widths
// (`crate::exact_int`), so the compiler no longer calls it; re-targeting
// the proofs is a follow-up.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Scalar {
    pub bits: u64,
    pub signed: bool,
    pub boolean: bool,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumericError {
    ZeroDivisor,
    SignedDivisionOverflow,
    NegativeShift,
    ShiftOutOfRange,
    InvalidWrapWidth,
    Unsupported,
}

/// One node of the 64-bit constant evaluator. Width is needed only for
/// wrapping operators. Relabel casts and AST width inference are outside
/// this kernel. Boolean inputs may carry noncanonical bits after bit-not.
#[allow(dead_code)]
pub(crate) fn fold_binary(
    op: BinaryOp,
    a: Scalar,
    b: Scalar,
    wrap_width: Option<u32>,
) -> Result<Scalar, NumericError> {
    use self::BinaryOp::*;
    use self::NumericError::*;
    let signed = a.signed && b.signed;
    let arith = |bits| Scalar {
        bits,
        signed,
        boolean: false,
    };
    let boolean = |value: bool| Scalar {
        bits: value as u64,
        signed: true,
        boolean: true,
    };
    Ok(match op {
        Add => arith(a.bits.wrapping_add(b.bits)),
        Sub => arith(a.bits.wrapping_sub(b.bits)),
        Mul => arith(a.bits.wrapping_mul(b.bits)),
        AddWrap | SubWrap | MulWrap => {
            let width = wrap_width.ok_or(InvalidWrapWidth)?;
            if width > 64 {
                return Err(InvalidWrapWidth);
            }
            let raw = match op {
                SubWrap => a.bits.wrapping_sub(b.bits),
                MulWrap => a.bits.wrapping_mul(b.bits),
                _ => a.bits.wrapping_add(b.bits),
            };
            Scalar {
                bits: if width == 64 {
                    raw
                } else {
                    raw & ((1u64 << width) - 1)
                },
                signed: false,
                boolean: false,
            }
        }
        Div | Mod => {
            if b.bits == 0 {
                return Err(ZeroDivisor);
            }
            if signed {
                let (x, y) = (a.bits as i64, b.bits as i64);
                if x == i64::MIN && y == -1 {
                    return Err(SignedDivisionOverflow);
                }
                arith(if op == Div { x / y } else { x % y } as u64)
            } else {
                arith(if op == Div {
                    a.bits / b.bits
                } else {
                    a.bits % b.bits
                })
            }
        }
        Shl | Shr => {
            if b.signed && (b.bits as i64) < 0 {
                return Err(NegativeShift);
            }
            if b.bits >= 64 {
                return Err(ShiftOutOfRange);
            }
            let bits = if op == Shl {
                a.bits << b.bits
            } else if a.signed {
                ((a.bits as i64) >> b.bits) as u64
            } else {
                a.bits >> b.bits
            };
            Scalar {
                bits,
                signed: a.signed,
                boolean: false,
            }
        }
        BitAnd | BitOr | BitXor => {
            let bits = match op {
                BitAnd => a.bits & b.bits,
                BitOr => a.bits | b.bits,
                _ => a.bits ^ b.bits,
            };
            // Boolean provenance remains attached to the result for shape and
            // dimension validation, but bitwise operators are still 64-bit
            // arithmetic on the operand bit patterns. In particular, `~true`
            // carries the noncanonical all-but-low-bit pattern and must not be
            // collapsed back to the Boolean value 1 here.
            let boolean = a.boolean && b.boolean;
            Scalar {
                bits,
                // Preserve the existing integral-promotion signedness for
                // Boolean-only operations, even when typed Boolean carriers
                // enter the kernel with `signed: false`.
                signed: if boolean { true } else { signed },
                boolean,
            }
        }
        Eq => boolean(a.bits == b.bits),
        Ne => boolean(a.bits != b.bits),
        Lt => boolean(if signed {
            (a.bits as i64) < (b.bits as i64)
        } else {
            a.bits < b.bits
        }),
        Le => boolean(if signed {
            (a.bits as i64) <= (b.bits as i64)
        } else {
            a.bits <= b.bits
        }),
        Gt => boolean(if signed {
            (a.bits as i64) > (b.bits as i64)
        } else {
            a.bits > b.bits
        }),
        Ge => boolean(if signed {
            (a.bits as i64) >= (b.bits as i64)
        } else {
            a.bits >= b.bits
        }),
        AndAnd | AndKw => boolean(a.bits != 0 && b.bits != 0),
        OrOr | OrKw => boolean(a.bits != 0 || b.bits != 0),
        _ => return Err(Unsupported),
    })
}
