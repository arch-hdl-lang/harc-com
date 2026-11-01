//! Arbitrary-precision integers for constant evaluation.
//!
//! Constant initializers are evaluated exactly at their ARCH result width
//! (spec: result widths of binary operators), so `uint<64>(0) -
//! uint<64>(15)` is the 65-bit value `2**65 - 15` in a `const` exactly as
//! it is at runtime. Values are sign-magnitude; bitwise operations and
//! width normalization use infinite two's-complement semantics.

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ExactInt {
    negative: bool,
    /// Magnitude, little-endian 32-bit limbs, no trailing zero limbs.
    mag: Vec<u32>,
}

fn trim(mut mag: Vec<u32>) -> Vec<u32> {
    while mag.last() == Some(&0) {
        mag.pop();
    }
    mag
}

fn mag_cmp(a: &[u32], b: &[u32]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for (x, y) in a.iter().rev().zip(b.iter().rev()) {
        if x != y {
            return x.cmp(y);
        }
    }
    Ordering::Equal
}

fn mag_add(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for i in 0..a.len().max(b.len()) {
        let s = *a.get(i).unwrap_or(&0) as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    trim(out)
}

/// `a - b` for `a >= b`.
fn mag_sub(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, x) in a.iter().enumerate() {
        let mut d = *x as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        borrow = 0;
        if d < 0 {
            d += 1 << 32;
            borrow = 1;
        }
        out.push(d as u32);
    }
    debug_assert_eq!(borrow, 0, "mag_sub underflow");
    trim(out)
}

fn mag_mul(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, y) in b.iter().enumerate() {
            let t = out[i + j] as u64 + (*x as u64) * (*y as u64) + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        let mut k = i + b.len();
        while carry != 0 {
            let t = out[k] as u64 + carry;
            out[k] = t as u32;
            carry = t >> 32;
            k += 1;
        }
    }
    trim(out)
}

fn mag_bit_len(a: &[u32]) -> u32 {
    match a.last() {
        None => 0,
        Some(top) => (a.len() as u32 - 1) * 32 + (32 - top.leading_zeros()),
    }
}

fn mag_bit(a: &[u32], bit: u32) -> bool {
    a.get((bit / 32) as usize)
        .is_some_and(|w| (w >> (bit % 32)) & 1 == 1)
}

fn mag_shl(a: &[u32], n: u32) -> Vec<u32> {
    if a.is_empty() {
        return Vec::new();
    }
    let (words, bits) = ((n / 32) as usize, n % 32);
    let mut out = vec![0u32; words];
    let mut carry = 0u32;
    for x in a {
        if bits == 0 {
            out.push(*x);
        } else {
            out.push((x << bits) | carry);
            carry = x >> (32 - bits);
        }
    }
    if carry != 0 {
        out.push(carry);
    }
    trim(out)
}

fn mag_shr(a: &[u32], n: u32) -> Vec<u32> {
    let (words, bits) = ((n / 32) as usize, n % 32);
    if words >= a.len() {
        return Vec::new();
    }
    let src = &a[words..];
    let mut out = Vec::with_capacity(src.len());
    for i in 0..src.len() {
        let lo = src[i] >> bits;
        let hi = if bits == 0 {
            0
        } else {
            src.get(i + 1).map_or(0, |h| h << (32 - bits))
        };
        out.push(lo | hi);
    }
    trim(out)
}

/// The low `w` bits of a magnitude.
fn mag_low_bits(a: &[u32], w: u32) -> Vec<u32> {
    let words = w.div_ceil(32) as usize;
    let mut out: Vec<u32> = a.iter().copied().take(words).collect();
    if out.len() == words && w % 32 != 0 {
        let last = out.last_mut().expect("nonempty");
        *last &= (1u32 << (w % 32)) - 1;
    }
    trim(out)
}

/// Truncated magnitude division (shift-subtract; constant operands are
/// small enough that this is never a bottleneck).
fn mag_divrem(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    assert!(!b.is_empty(), "division by zero magnitude");
    if mag_cmp(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    let mut q = vec![0u32; a.len()];
    let mut r: Vec<u32> = Vec::new();
    for bit in (0..mag_bit_len(a)).rev() {
        r = mag_shl(&r, 1);
        if mag_bit(a, bit) {
            if r.is_empty() {
                r.push(1);
            } else {
                r[0] |= 1;
            }
        }
        if mag_cmp(&r, b) != Ordering::Less {
            r = mag_sub(&r, b);
            q[(bit / 32) as usize] |= 1 << (bit % 32);
        }
    }
    (trim(q), r)
}

impl ExactInt {
    #[cfg(test)]
    pub(crate) fn zero() -> Self {
        Self::default()
    }

    fn from_parts(negative: bool, mag: Vec<u32>) -> Self {
        let mag = trim(mag);
        Self {
            negative: negative && !mag.is_empty(),
            mag,
        }
    }

    pub(crate) fn from_u128(v: u128) -> Self {
        Self::from_parts(false, (0..4).map(|i| (v >> (32 * i)) as u32).collect())
    }

    pub(crate) fn from_i128(v: i128) -> Self {
        let mut out = Self::from_u128(v.unsigned_abs());
        out.negative = v < 0;
        out
    }

    /// A 64-bit carrier pattern read as signed (two's complement) or unsigned.
    pub(crate) fn from_bits(bits: u64, signed: bool) -> Self {
        if signed {
            Self::from_i128(bits as i64 as i128)
        } else {
            Self::from_u128(bits as u128)
        }
    }

    /// LSB-first 32-bit words of a non-negative value.
    pub(crate) fn from_words(words: &[u32]) -> Self {
        Self::from_parts(false, words.to_vec())
    }

    pub(crate) fn pow2(n: u32) -> Self {
        Self::from_u128(1).shl(n)
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub(crate) fn is_negative(&self) -> bool {
        self.negative
    }

    /// Bits needed for the magnitude (0 for zero).
    pub(crate) fn magnitude_bits(&self) -> u32 {
        mag_bit_len(&self.mag)
    }

    /// Width of the value as an untyped literal: its significant bits, and
    /// one more for the sign of a negative value (`-1` is 1 bit + sign).
    pub(crate) fn literal_width(&self) -> u32 {
        if self.negative {
            // Two's-complement width of -m: bits of (m - 1) plus a sign bit.
            self.clone().add(&Self::from_u128(1)).magnitude_bits() + 1
        } else {
            self.magnitude_bits().max(1)
        }
    }

    pub(crate) fn to_i128(&self) -> Option<i128> {
        if self.magnitude_bits() > 127 {
            return (self.negative
                && self.magnitude_bits() == 128
                && self.mag == [0, 0, 0, 1 << 31])
            .then_some(i128::MIN);
        }
        let mut v: u128 = 0;
        for (i, w) in self.mag.iter().enumerate() {
            v |= (*w as u128) << (32 * i);
        }
        Some(if self.negative {
            -(v as i128)
        } else {
            v as i128
        })
    }

    /// Whether the value fits a 64-bit carrier as signed (`i64`) or as
    /// unsigned (`u64`).
    pub(crate) fn fits_i64(&self) -> bool {
        self.to_i128().is_some_and(|v| i64::try_from(v).is_ok())
    }
    pub(crate) fn fits_u64(&self) -> bool {
        self.to_i128().is_some_and(|v| u64::try_from(v).is_ok())
    }

    /// The low 64 bits of the two's-complement pattern.
    pub(crate) fn low_u64(&self) -> u64 {
        let m = self.mod_pow2(64);
        m.mag.first().copied().unwrap_or(0) as u64
            | (m.mag.get(1).copied().unwrap_or(0) as u64) << 32
    }

    /// LSB-first 32-bit words of the `w`-bit two's-complement pattern.
    pub(crate) fn pattern_words(&self, w: u32) -> Vec<u32> {
        let m = self.mod_pow2(w);
        let mut words = m.mag;
        words.resize(w.div_ceil(32) as usize, 0);
        words
    }

    pub(crate) fn neg(&self) -> Self {
        Self::from_parts(!self.negative, self.mag.clone())
    }

    pub(crate) fn add(&self, other: &Self) -> Self {
        if self.negative == other.negative {
            return Self::from_parts(self.negative, mag_add(&self.mag, &other.mag));
        }
        match mag_cmp(&self.mag, &other.mag) {
            Ordering::Less => Self::from_parts(other.negative, mag_sub(&other.mag, &self.mag)),
            _ => Self::from_parts(self.negative, mag_sub(&self.mag, &other.mag)),
        }
    }

    pub(crate) fn sub(&self, other: &Self) -> Self {
        self.add(&other.neg())
    }

    pub(crate) fn mul(&self, other: &Self) -> Self {
        Self::from_parts(
            self.negative != other.negative,
            mag_mul(&self.mag, &other.mag),
        )
    }

    /// Division truncated toward zero, and its remainder (sign of the
    /// dividend). `None` for a zero divisor.
    pub(crate) fn div_rem(&self, other: &Self) -> Option<(Self, Self)> {
        if other.is_zero() {
            return None;
        }
        let (q, r) = mag_divrem(&self.mag, &other.mag);
        Some((
            Self::from_parts(self.negative != other.negative, q),
            Self::from_parts(self.negative, r),
        ))
    }

    pub(crate) fn shl(&self, n: u32) -> Self {
        Self::from_parts(self.negative, mag_shl(&self.mag, n))
    }

    /// Arithmetic (floor) right shift.
    pub(crate) fn shr(&self, n: u32) -> Self {
        if !self.negative {
            return Self::from_parts(false, mag_shr(&self.mag, n));
        }
        // floor(-m / 2^n) = -(((m - 1) >> n) + 1)
        let m1 = mag_sub(&self.mag, &[1]);
        Self::from_parts(true, mag_add(&mag_shr(&m1, n), &[1]))
    }

    /// The value modulo `2**w`, in `0 .. 2**w` (the low `w` bits of its
    /// two's-complement pattern, read unsigned).
    pub(crate) fn mod_pow2(&self, w: u32) -> Self {
        let low = mag_low_bits(&self.mag, w);
        if !self.negative || low.is_empty() {
            return Self::from_parts(false, low);
        }
        Self::from_parts(false, mag_sub(&Self::pow2(w).mag, &low))
    }

    /// The low `w` bits read as a `w`-bit two's-complement value.
    pub(crate) fn sext(&self, w: u32) -> Self {
        let u = self.mod_pow2(w);
        if w > 0 && mag_bit(&u.mag, w - 1) {
            u.sub(&Self::pow2(w))
        } else {
            u
        }
    }

    /// Normalize to a `w`-bit value: unsigned keeps the low `w` bits,
    /// signed also sign-extends from bit `w - 1`.
    pub(crate) fn normalize(&self, w: u32, signed: bool) -> Self {
        if signed {
            self.sext(w)
        } else {
            self.mod_pow2(w)
        }
    }

    /// Whether the value is representable as `uint<w>` / `sint<w>`.
    pub(crate) fn fits(&self, w: u32, signed: bool) -> bool {
        &self.normalize(w, signed) == self
    }

    pub(crate) fn not(&self) -> Self {
        self.neg().sub(&Self::from_u128(1))
    }

    fn bitwise(&self, other: &Self, f: impl Fn(u32, u32) -> u32) -> Self {
        let w = self.literal_width().max(other.literal_width()) + 1;
        let (a, b) = (self.pattern_words(w), other.pattern_words(w));
        let words: Vec<u32> = a.iter().zip(&b).map(|(x, y)| f(*x, *y)).collect();
        Self::from_words(&words).sext(w)
    }

    pub(crate) fn and(&self, other: &Self) -> Self {
        self.bitwise(other, |x, y| x & y)
    }
    pub(crate) fn or(&self, other: &Self) -> Self {
        self.bitwise(other, |x, y| x | y)
    }
    pub(crate) fn xor(&self, other: &Self) -> Self {
        self.bitwise(other, |x, y| x ^ y)
    }

    pub(crate) fn to_u32(&self) -> Option<u32> {
        self.to_i128().and_then(|v| u32::try_from(v).ok())
    }
}

impl PartialOrd for ExactInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ExactInt {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_cmp(&self.mag, &other.mag),
            (true, true) => mag_cmp(&other.mag, &self.mag),
        }
    }
}

impl std::fmt::Display for ExactInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_zero() {
            return write!(f, "0");
        }
        let mut digits = Vec::new();
        let mut m = self.mag.clone();
        let billion = [1_000_000_000u32];
        while !m.is_empty() {
            let (q, r) = mag_divrem(&m, &billion);
            digits.push(r.first().copied().unwrap_or(0));
            m = q;
        }
        if self.negative {
            write!(f, "-")?;
        }
        write!(f, "{}", digits.pop().expect("nonzero"))?;
        for d in digits.iter().rev() {
            write!(f, "{d:09}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ExactInt as E;

    fn i(v: i128) -> E {
        E::from_i128(v)
    }

    #[test]
    fn arithmetic_matches_i128() {
        let samples = [
            0i128,
            1,
            -1,
            7,
            -7,
            255,
            -256,
            i64::MAX as i128,
            i64::MIN as i128,
            u64::MAX as i128,
            (1i128 << 100) + 12345,
            -(1i128 << 90) - 3,
        ];
        for &a in &samples {
            for &b in &samples {
                assert_eq!(i(a).add(&i(b)).to_i128(), a.checked_add(b), "{a}+{b}");
                assert_eq!(i(a).sub(&i(b)).to_i128(), a.checked_sub(b), "{a}-{b}");
                if let Some(p) = a.checked_mul(b) {
                    assert_eq!(i(a).mul(&i(b)).to_i128(), Some(p), "{a}*{b}");
                }
                if b != 0 {
                    let (q, r) = i(a).div_rem(&i(b)).unwrap();
                    assert_eq!(
                        (q.to_i128(), r.to_i128()),
                        (Some(a / b), Some(a % b)),
                        "{a}/{b}"
                    );
                }
                assert_eq!(i(a).and(&i(b)).to_i128(), Some(a & b), "{a}&{b}");
                assert_eq!(i(a).or(&i(b)).to_i128(), Some(a | b), "{a}|{b}");
                assert_eq!(i(a).xor(&i(b)).to_i128(), Some(a ^ b), "{a}^{b}");
                assert_eq!(i(a).cmp(&i(b)), a.cmp(&b), "{a} cmp {b}");
            }
            for n in [0u32, 1, 5, 31, 32, 33, 63] {
                assert_eq!(i(a).shr(n).to_i128(), Some(a >> n), "{a}>>{n}");
            }
            assert_eq!(i(a).not().to_i128(), Some(!a));
            assert_eq!(i(a).to_string(), a.to_string());
        }
    }

    #[test]
    fn widths_and_patterns() {
        assert_eq!(i(-1).mod_pow2(8).to_i128(), Some(255));
        assert_eq!(i(255).sext(8).to_i128(), Some(-1));
        assert_eq!(i(-15).mod_pow2(65).to_i128(), Some((1i128 << 65) - 15));
        assert!(i(127).fits(8, true) && !i(128).fits(8, true) && i(-128).fits(8, true));
        assert!(i(255).fits(8, false) && !i(-1).fits(8, false));
        assert_eq!(i(-1).low_u64(), u64::MAX);
        assert_eq!(i(-1).literal_width(), 1);
        assert_eq!(i(-2).literal_width(), 2);
        assert_eq!(i(5).literal_width(), 3);
        let big = E::pow2(200).sub(&i(1));
        assert_eq!(big.magnitude_bits(), 200);
        // (2**200 - 1)**2 == 2**400 - 2**201 + 1
        assert_eq!(big.mul(&big), E::pow2(400).sub(&E::pow2(201)).add(&i(1)));
        let (q, r) = big.mul(&big).div_rem(&big).unwrap();
        assert_eq!((q, r), (big.clone(), E::zero()));
        assert_eq!(i(-60).pattern_words(129).len(), 5);
        assert_eq!(
            E::pow2(129).sub(&i(60)).to_string(),
            "680564733841876926926749214863536422852"
        );
    }
}
