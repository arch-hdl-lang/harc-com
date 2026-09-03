use super::{BinOp, Expr, IrType, UnOp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarValueEvidence {
    Bool,
    Integer {
        width: Option<u32>,
        signed: Option<bool>,
        exact: Option<i128>,
        unsigned_bound: bool,
    },
}

pub(crate) fn contextual_value_bits(value: i128, signed: bool) -> Option<u32> {
    if signed {
        Some(signed_value_bits(value))
    } else if value < 0 {
        None
    } else {
        Some(nonnegative_value_bits(value))
    }
}

pub(crate) fn scalar_value_evidence(
    value: &Expr,
    leaf_type: &impl Fn(&Expr) -> Option<IrType>,
) -> Option<ScalarValueEvidence> {
    let integer = |width, signed, exact, unsigned_bound| ScalarValueEvidence::Integer {
        width,
        signed,
        exact,
        unsigned_bound,
    };
    let from_type = |ty: IrType| match ty {
        IrType::Bool => Some(ScalarValueEvidence::Bool),
        IrType::UInt(width) => Some(integer(width, Some(false), None, width.is_some())),
        IrType::SInt(width) => Some(integer(width, Some(true), None, false)),
        _ => None,
    };
    let as_integer = |evidence: ScalarValueEvidence| match evidence {
        ScalarValueEvidence::Bool => (Some(1), Some(false), None, true),
        ScalarValueEvidence::Integer {
            width,
            signed,
            exact,
            unsigned_bound,
        } => (width, signed, exact, unsigned_bound),
    };
    match value {
        Expr::Literal { value, ty } => match ty {
            IrType::Bool => Some(ScalarValueEvidence::Bool),
            IrType::Unknown | IrType::UInt(None) => Some(integer(
                Some(unsigned_value_bits(*value)),
                None,
                Some(*value as i128),
                true,
            )),
            IrType::SInt(None) => {
                let exact = (*value as i64) as i128;
                Some(integer(
                    contextual_value_bits(exact, exact < 0),
                    None,
                    Some(exact),
                    exact >= 0,
                ))
            }
            _ => from_type(ty.clone()),
        },
        Expr::WideLiteral(words) => Some(integer(
            Some(wide_literal_bits(words)),
            Some(false),
            None,
            true,
        )),
        Expr::Binary(op, lhs, rhs) => {
            if matches!(
                op,
                BinOp::Eq
                    | BinOp::Ne
                    | BinOp::Lt
                    | BinOp::Le
                    | BinOp::Gt
                    | BinOp::Ge
                    | BinOp::And
                    | BinOp::Or
            ) {
                return Some(ScalarValueEvidence::Bool);
            }
            let lhs = scalar_value_evidence(lhs, leaf_type)?;
            let rhs = scalar_value_evidence(rhs, leaf_type)?;
            let (lw, ls, le, lub) = as_integer(lhs);
            let (rw, rs, re, rub) = as_integer(rhs);
            if matches!(op, BinOp::Shl | BinOp::Shr) {
                let shift = re.and_then(|shift| u32::try_from(shift).ok());
                let (width, exact) = match (op, ls, le, shift) {
                    (BinOp::Shl, None, Some(value), Some(shift)) => {
                        let width = if value == 0 {
                            Some(1)
                        } else {
                            contextual_value_bits(value, value < 0)
                                .and_then(|width| width.checked_add(shift))
                        };
                        let exact = width
                            .filter(|width| *width <= if value < 0 { 128 } else { 127 })
                            .and_then(|_| value.checked_shl(shift));
                        (width, exact)
                    }
                    (BinOp::Shr, None, Some(value), Some(shift)) if shift < 128 => {
                        let exact = value.checked_shr(shift);
                        (
                            exact.and_then(|value| contextual_value_bits(value, value < 0)),
                            exact,
                        )
                    }
                    _ => (lw, None),
                };
                return Some(integer(width, ls, exact, lub));
            }
            let exact = match (op, le, re) {
                (BinOp::Add, Some(a), Some(b)) => a.checked_add(b),
                (BinOp::Sub, Some(a), Some(b)) => a.checked_sub(b),
                (BinOp::Mul, Some(a), Some(b)) => a.checked_mul(b),
                (BinOp::Div, Some(a), Some(b)) if b != 0 => a.checked_div(b),
                (BinOp::Mod, Some(a), Some(b)) if b != 0 => a.checked_rem(b),
                (BinOp::BitAnd, Some(a), Some(b)) => Some(a & b),
                (BinOp::BitOr, Some(a), Some(b)) => Some(a | b),
                (BinOp::BitXor, Some(a), Some(b)) => Some(a ^ b),
                _ => None,
            };
            if matches!(op, BinOp::BitAnd) {
                let width = [(lw, lub), (rw, rub)]
                    .into_iter()
                    .filter_map(|(width, bound)| bound.then_some(width).flatten())
                    .min()
                    .or_else(|| common_evidence_width(lw, rw));
                let unsigned_bound = width.is_some() && (lub || rub);
                return Some(integer(
                    width,
                    unsigned_bound
                        .then_some(false)
                        .or_else(|| common_evidence_sign(ls, rs)),
                    exact,
                    unsigned_bound,
                ));
            }
            let signed = common_evidence_sign(ls, rs);
            let result_signed = signed.unwrap_or(false);
            let width = exact
                .and_then(|value| contextual_value_bits(value, value < 0))
                .filter(|_| signed.is_none())
                .or_else(|| {
                    common_evidence_width(
                        evidence_width_for_sign(lw, ls, le, result_signed),
                        evidence_width_for_sign(rw, rs, re, result_signed),
                    )
                });
            Some(integer(
                width,
                signed,
                exact,
                signed == Some(false) && width.is_some(),
            ))
        }
        Expr::Ternary(_, then_value, else_value) => {
            let then_value = scalar_value_evidence(then_value, leaf_type)?;
            let else_value = scalar_value_evidence(else_value, leaf_type)?;
            match (then_value, else_value) {
                (ScalarValueEvidence::Bool, ScalarValueEvidence::Bool) => {
                    Some(ScalarValueEvidence::Bool)
                }
                (then_value, else_value) => {
                    let (tw, ts, te, tub) = as_integer(then_value);
                    let (ew, es, ee, eub) = as_integer(else_value);
                    let signed = common_evidence_sign(ts, es);
                    let result_signed = signed.unwrap_or(false);
                    Some(integer(
                        common_evidence_width(
                            evidence_width_for_sign(tw, ts, te, result_signed),
                            evidence_width_for_sign(ew, es, ee, result_signed),
                        ),
                        signed,
                        (te == ee).then_some(te).flatten(),
                        tub && eub,
                    ))
                }
            }
        }
        Expr::Unary(UnOp::Not, _) => Some(ScalarValueEvidence::Bool),
        Expr::Unary(op, inner) => {
            let evidence = scalar_value_evidence(inner, leaf_type)?;
            let ScalarValueEvidence::Integer {
                mut width,
                signed,
                exact,
                unsigned_bound,
            } = evidence
            else {
                return None;
            };
            let exact = match (op, exact) {
                (UnOp::Neg, Some(value)) => value.checked_neg(),
                _ => None,
            };
            if signed.is_none() {
                if let Some(value) = exact {
                    width = contextual_value_bits(value, value < 0);
                }
            }
            Some(integer(
                width,
                signed,
                exact,
                unsigned_bound && !matches!(op, UnOp::Neg),
            ))
        }
        _ => leaf_type(value).and_then(from_type),
    }
}

fn unsigned_value_bits(value: u64) -> u32 {
    (64 - value.leading_zeros()).max(1)
}

fn nonnegative_value_bits(value: i128) -> u32 {
    debug_assert!(value >= 0);
    (128 - (value as u128).leading_zeros()).max(1)
}

pub(crate) fn signed_value_bits(value: i128) -> u32 {
    if value >= 0 {
        if value == 0 {
            1
        } else {
            nonnegative_value_bits(value).saturating_add(1)
        }
    } else {
        let magnitude_below_zero = (!value) as u128;
        (128 - magnitude_below_zero.leading_zeros()) + 1
    }
}

fn common_evidence_sign(lhs: Option<bool>, rhs: Option<bool>) -> Option<bool> {
    match (lhs, rhs) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        (Some(sign), None) | (None, Some(sign)) => Some(sign),
        (None, None) => None,
    }
}

fn common_evidence_width(lhs: Option<u32>, rhs: Option<u32>) -> Option<u32> {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Some(lhs.max(rhs)),
        (width, None) | (None, width) => width,
    }
}

fn evidence_width_for_sign(
    width: Option<u32>,
    signed: Option<bool>,
    exact: Option<i128>,
    result_signed: bool,
) -> Option<u32> {
    if signed.is_none() {
        exact
            .and_then(|value| contextual_value_bits(value, result_signed))
            .or(width)
    } else {
        width
    }
}

fn wide_literal_bits(words: &[u32]) -> u32 {
    let Some((index, word)) = words.iter().enumerate().rev().find(|(_, word)| **word != 0) else {
        return 1;
    };
    (index as u32) * 32 + (32 - word.leading_zeros())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_shift_evidence_tracks_the_shifted_value() {
        let value = Expr::Binary(
            BinOp::Shl,
            Box::new(Expr::Literal {
                value: 1,
                ty: IrType::Unknown,
            }),
            Box::new(Expr::Literal {
                value: 8,
                ty: IrType::Unknown,
            }),
        );

        let overflow = Expr::Binary(
            BinOp::Shl,
            Box::new(Expr::Literal {
                value: 1,
                ty: IrType::Unknown,
            }),
            Box::new(Expr::Literal {
                value: 127,
                ty: IrType::Unknown,
            }),
        );
        assert_eq!(
            scalar_value_evidence(&overflow, &|_| None),
            Some(ScalarValueEvidence::Integer {
                width: Some(128),
                signed: None,
                exact: None,
                unsigned_bound: true,
            })
        );
        assert_eq!(
            scalar_value_evidence(&value, &|_| None),
            Some(ScalarValueEvidence::Integer {
                width: Some(9),
                signed: None,
                exact: Some(256),
                unsigned_bound: true,
            })
        );
    }

    #[test]
    fn signed_width_handles_the_i128_minimum() {
        assert_eq!(signed_value_bits(i128::MIN), 128);
    }
}
