//! Harnesses import the production module directly; there is no copied evaluator.
#![allow(dead_code)]
#[path = "../../src/numeric_semantics.rs"]
mod numeric_semantics;
use numeric_semantics::{
    fold_binary, solver_binary_is_signed, BinaryOp::*, ConstraintOperandSignedness as Sign,
    NumericError::*, Scalar,
};

fn scalar() -> Scalar {
    Scalar {
        bits: kani::any(),
        signed: kani::any(),
        boolean: kani::any(),
    }
}
fn integer(bits: u64, signed: bool) -> Scalar {
    Scalar {
        bits,
        signed,
        boolean: false,
    }
}
fn boolean(value: bool) -> Scalar {
    Scalar {
        bits: value as u64,
        signed: true,
        boolean: true,
    }
}
// Compare active fields explicitly. This avoids library aggregate equality
// lowering and keeps the proof obligation visible to the SMT backend.
fn check(
    actual: Result<Scalar, numeric_semantics::NumericError>,
    expected: Result<Scalar, numeric_semantics::NumericError>,
) {
    match (actual, expected) {
        (Ok(a), Ok(b)) => {
            assert!(a.bits == b.bits);
            assert!(a.signed == b.signed);
            assert!(a.boolean == b.boolean);
        }
        (Err(a), Err(b)) => assert!(a as u8 == b as u8),
        _ => panic!("wrong result kind"),
    }
}
fn mathematical(v: Scalar, signed: bool) -> i128 {
    let unsigned = v.bits as i128;
    if signed && v.bits >> 63 != 0 {
        unsigned - (1i128 << 64)
    } else {
        unsigned
    }
}

#[kani::proof]
fn const_add_sub() {
    let (a, b) = (scalar(), scalar());
    let signed = a.signed && b.signed;
    check(
        fold_binary(Add, a, b, None),
        Ok(integer(
            ((a.bits as u128) + (b.bits as u128)) as u64,
            signed,
        )),
    );
    check(
        fold_binary(Sub, a, b, None),
        Ok(integer(
            ((1u128 << 64) + (a.bits as u128) - (b.bits as u128)) as u64,
            signed,
        )),
    );
}
#[kani::proof]
#[kani::solver(z3)]
fn const_mul() {
    let (a, b) = (scalar(), scalar());
    check(
        fold_binary(Mul, a, b, None),
        Ok(integer(
            ((a.bits as u128) * (b.bits as u128)) as u64,
            a.signed && b.signed,
        )),
    );
}
#[kani::proof]
#[kani::solver(z3)]
fn const_wrap() {
    let (a, b) = (scalar(), scalar());
    let width: u32 = kani::any();
    kani::assume(width <= 64);
    let choice: u8 = kani::any();
    kani::assume(choice < 3);
    let (op, raw) = match choice {
        0 => (AddWrap, (a.bits as u128) + (b.bits as u128)),
        1 => (SubWrap, (1u128 << 64) + (a.bits as u128) - (b.bits as u128)),
        _ => (MulWrap, (a.bits as u128) * (b.bits as u128)),
    };
    let residue = (raw & ((1u128 << width) - 1)) as u64;
    check(
        fold_binary(op, a, b, Some(width)),
        Ok(integer(residue, false)),
    );
}
#[kani::proof]
fn const_compare() {
    let (a, b) = (scalar(), scalar());
    let (x, y) = (
        mathematical(a, a.signed && b.signed),
        mathematical(b, a.signed && b.signed),
    );
    check(fold_binary(Lt, a, b, None), Ok(boolean(x < y)));
    check(fold_binary(Le, a, b, None), Ok(boolean(x <= y)));
    check(fold_binary(Gt, a, b, None), Ok(boolean(x > y)));
    check(fold_binary(Ge, a, b, None), Ok(boolean(x >= y)));
    check(fold_binary(Eq, a, b, None), Ok(boolean(a.bits == b.bits)));
    check(fold_binary(Ne, a, b, None), Ok(boolean(a.bits != b.bits)));
}
#[kani::proof]
fn const_shift() {
    let (a, b) = (scalar(), scalar());
    let error = if b.signed && b.bits >> 63 != 0 {
        Some(NegativeShift)
    } else if b.bits >= 64 {
        Some(ShiftOutOfRange)
    } else {
        None
    };
    if let Some(error) = error {
        check(fold_binary(Shl, a, b, None), Err(error));
        check(fold_binary(Shr, a, b, None), Err(error));
    } else {
        let left = ((a.bits as u128) << b.bits) as u64;
        let right = (mathematical(a, a.signed) >> b.bits) as u64;
        check(fold_binary(Shl, a, b, None), Ok(integer(left, a.signed)));
        check(fold_binary(Shr, a, b, None), Ok(integer(right, a.signed)));
    }
}
#[kani::proof]
fn const_bit_logic() {
    let (a, b) = (scalar(), scalar());
    let both = a.boolean && b.boolean;
    for (op, bits) in [
        (BitAnd, a.bits & b.bits),
        (BitOr, a.bits | b.bits),
        (BitXor, a.bits ^ b.bits),
    ] {
        let expected = Scalar {
            bits,
            signed: if both { true } else { a.signed && b.signed },
            boolean: both,
        };
        check(fold_binary(op, a, b, None), Ok(expected));
    }
    for op in [AndAnd, AndKw] {
        check(
            fold_binary(op, a, b, None),
            Ok(boolean(a.bits != 0 && b.bits != 0)),
        );
    }
    for op in [OrOr, OrKw] {
        check(
            fold_binary(op, a, b, None),
            Ok(boolean(a.bits != 0 || b.bits != 0)),
        );
    }
}
#[kani::proof]
#[kani::solver(z3)]
fn const_div_rem() {
    let (a, b) = (scalar(), scalar());
    let signed = a.signed && b.signed;
    let (x, y) = (mathematical(a, signed), mathematical(b, signed));
    kani::assume(y != 0);
    kani::assume(!(signed && x == -(1i128 << 63) && y == -1));
    let quotient = if signed {
        ((a.bits as i64) / (b.bits as i64)) as u64
    } else {
        a.bits / b.bits
    };
    let remainder = if signed {
        ((a.bits as i64) % (b.bits as i64)) as u64
    } else {
        a.bits % b.bits
    };
    check(fold_binary(Div, a, b, None), Ok(integer(quotient, signed)));
    check(fold_binary(Mod, a, b, None), Ok(integer(remainder, signed)));
}

#[kani::proof]
fn const_errors() {
    let (a, b) = (scalar(), scalar());
    for op in [AddWrap, SubWrap, MulWrap] {
        check(fold_binary(op, a, b, None), Err(InvalidWrapWidth));
        let width: u32 = kani::any();
        kani::assume(width > 64);
        check(fold_binary(op, a, b, Some(width)), Err(InvalidWrapWidth));
    }
    let unsupported: u8 = kani::any();
    kani::assume(unsupported < 7);
    let op = match unsupported {
        0 => PipeImplies,
        1 => PipeImpliesNext,
        2 => Throughout,
        3 => Within,
        4 => Intersect,
        5 => In,
        _ => Inside,
    };
    check(fold_binary(op, a, b, None), Err(Unsupported));
    let signed = a.signed && b.signed;
    let (x, y) = (mathematical(a, signed), mathematical(b, signed));
    kani::assume(y == 0 || (signed && x == -(1i128 << 63) && y == -1));
    let error = if y == 0 {
        ZeroDivisor
    } else {
        SignedDivisionOverflow
    };
    check(fold_binary(Div, a, b, None), Err(error));
    check(fold_binary(Mod, a, b, None), Err(error));
}

fn sign() -> Sign {
    let x: u8 = kani::any();
    kani::assume(x < 3);
    match x {
        0 => Sign::Signed,
        1 => Sign::Unsigned,
        _ => Sign::UntypedLiteral,
    }
}
#[kani::proof]
fn solver_promotion() {
    let (left, right) = (sign(), sign());
    let (lw, rw): (u32, u32) = kani::any();
    kani::assume((1..=1024).contains(&lw) && (1..=1024).contains(&rw));
    // Existing solver contract: narrow mixed operands promote to signed
    // int32; otherwise the wider declared signed domain must contain the
    // unsigned domain. This does not prove AST classification or C++ ABI parity.
    let promoted = |s: Sign, w: u32| {
        if s == Sign::UntypedLiteral {
            None
        } else if w < 32 {
            Some((true, 32))
        } else {
            Some((s == Sign::Signed, w))
        }
    };
    let expected = match (left, right) {
        // Contextual literal treatment is HARC's existing solver policy.
        (Sign::UntypedLiteral, Sign::UntypedLiteral) => false,
        (Sign::UntypedLiteral, s) | (s, Sign::UntypedLiteral) => s == Sign::Signed,
        (Sign::Signed, Sign::Signed) => true,
        (Sign::Unsigned, Sign::Unsigned) => false,
        _ => {
            let (ls, lr) = promoted(left, lw).unwrap();
            let (rs, rr) = promoted(right, rw).unwrap();
            if ls == rs {
                ls
            } else if ls {
                lr > rr
            } else {
                rr > lr
            }
        }
    };
    let result = solver_binary_is_signed(left, right, Some(lw), Some(rw));
    assert_eq!(result, expected);
    assert_eq!(
        result,
        solver_binary_is_signed(right, left, Some(rw), Some(lw))
    );
}
#[kani::proof]
fn solver_unknown_widths() {
    let (left, right) = (sign(), sign());
    let (lw, rw): (Option<u32>, Option<u32>) = kani::any();
    let result = solver_binary_is_signed(left, right, lw, rw);
    assert_eq!(result, solver_binary_is_signed(right, left, rw, lw));
    if left == Sign::Signed && right == Sign::Unsigned && rw.is_none() {
        assert!(!result);
    }
    if right == Sign::Signed && left == Sign::Unsigned && lw.is_none() {
        assert!(!result);
    }
}
