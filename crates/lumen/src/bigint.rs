//! Arbitrary-precision BigInt, from scratch: sign + little-endian u64 magnitude behind an `Rc`
//! (clones are cheap; every operation allocates a fresh value). Conformance needs exactness far
//! past 128 bits (e.g. comparing a 1024-bit literal with `Number.MAX_VALUE`), not speed.

use std::cmp::Ordering;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub struct JsBigInt {
    /// True for negative values. Zero is always non-negative with an empty magnitude.
    neg: bool,
    /// Little-endian base-2^64 digits, no trailing zero limbs.
    mag: Rc<Vec<u64>>,
}

fn trim(mut mag: Vec<u64>) -> Vec<u64> {
    while mag.last() == Some(&0) {
        mag.pop();
    }
    mag
}

fn mag_cmp(a: &[u64], b: &[u64]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        match a[i].cmp(&b[i]) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

fn mag_add(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u128;
    for i in 0..a.len().max(b.len()) {
        let s = carry + *a.get(i).unwrap_or(&0) as u128 + *b.get(i).unwrap_or(&0) as u128;
        out.push(s as u64);
        carry = s >> 64;
    }
    if carry != 0 {
        out.push(carry as u64);
    }
    out
}

/// `a - b` for `a >= b`.
fn mag_sub(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i128;
    for i in 0..a.len() {
        let d = a[i] as i128 - *b.get(i).unwrap_or(&0) as i128 - borrow;
        if d < 0 {
            out.push((d + (1i128 << 64)) as u64);
            borrow = 1;
        } else {
            out.push(d as u64);
            borrow = 0;
        }
    }
    trim(out)
}

fn mag_mul(a: &[u64], b: &[u64]) -> Vec<u64> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u64; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u128;
        for (j, &y) in b.iter().enumerate() {
            let cur = out[i + j] as u128 + x as u128 * y as u128 + carry;
            out[i + j] = cur as u64;
            carry = cur >> 64;
        }
        let mut k = i + b.len();
        while carry != 0 {
            let cur = out[k] as u128 + carry;
            out[k] = cur as u64;
            carry = cur >> 64;
            k += 1;
        }
    }
    trim(out)
}

/// Schoolbook division: `(quotient, remainder)` of `a / b` with `b` non-zero.
fn mag_divmod(a: &[u64], b: &[u64]) -> (Vec<u64>, Vec<u64>) {
    if mag_cmp(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let d = b[0] as u128;
        let mut q = vec![0u64; a.len()];
        let mut rem = 0u128;
        for i in (0..a.len()).rev() {
            let cur = (rem << 64) | a[i] as u128;
            q[i] = (cur / d) as u64;
            rem = cur % d;
        }
        return (trim(q), trim(vec![rem as u64]));
    }
    // Bit-by-bit long division (slow but simple; conformance sizes are small).
    let bits = a.len() * 64;
    let mut q = vec![0u64; a.len()];
    let mut rem: Vec<u64> = Vec::new();
    for i in (0..bits).rev() {
        // rem = rem << 1 | bit(a, i)
        let mut carry = (a[i / 64] >> (i % 64)) & 1;
        for limb in rem.iter_mut() {
            let hi = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = hi;
        }
        if carry != 0 {
            rem.push(carry);
        }
        if mag_cmp(&rem, b) != Ordering::Less {
            rem = mag_sub(&rem, b);
            q[i / 64] |= 1 << (i % 64);
        }
    }
    (trim(q), rem)
}

impl JsBigInt {
    pub fn zero() -> Self {
        JsBigInt {
            neg: false,
            mag: Rc::new(Vec::new()),
        }
    }
    fn make(neg: bool, mag: Vec<u64>) -> Self {
        let mag = trim(mag);
        JsBigInt {
            neg: neg && !mag.is_empty(),
            mag: Rc::new(mag),
        }
    }
    pub fn from_i128(v: i128) -> Self {
        let neg = v < 0;
        let u = v.unsigned_abs();
        Self::make(neg, vec![u as u64, (u >> 64) as u64])
    }
    pub fn from_u64(v: u64) -> Self {
        Self::make(false, vec![v])
    }
    /// The value as an i128, if it fits.
    pub fn to_i128(&self) -> Option<i128> {
        if self.mag.len() > 2 {
            return None;
        }
        let lo = *self.mag.first().unwrap_or(&0) as u128;
        let hi = *self.mag.get(1).unwrap_or(&0) as u128;
        let u = (hi << 64) | lo;
        if self.neg {
            if u > 1u128 << 127 {
                return None;
            }
            Some((u as i128).wrapping_neg())
        } else {
            if u >= 1u128 << 127 {
                return None;
            }
            Some(u as i128)
        }
    }
    /// The low 128 bits, two's-complement wrapped (for fixed-width storage like BigInt64Array).
    pub fn to_i128_wrapping(&self) -> i128 {
        let lo = *self.mag.first().unwrap_or(&0) as u128;
        let hi = *self.mag.get(1).unwrap_or(&0) as u128;
        let u = (hi << 64) | lo;
        let v = u as i128;
        if self.neg {
            v.wrapping_neg()
        } else {
            v
        }
    }
    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }
    pub fn is_negative(&self) -> bool {
        self.neg
    }
    pub fn bit_len(&self) -> usize {
        match self.mag.last() {
            None => 0,
            Some(&top) => (self.mag.len() - 1) * 64 + (64 - top.leading_zeros() as usize),
        }
    }

    pub fn neg(&self) -> Self {
        Self::make(!self.neg, self.mag.as_ref().clone())
    }
    pub fn add(&self, o: &Self) -> Self {
        if self.neg == o.neg {
            Self::make(self.neg, mag_add(&self.mag, &o.mag))
        } else {
            match mag_cmp(&self.mag, &o.mag) {
                Ordering::Equal => Self::zero(),
                Ordering::Greater => Self::make(self.neg, mag_sub(&self.mag, &o.mag)),
                Ordering::Less => Self::make(o.neg, mag_sub(&o.mag, &self.mag)),
            }
        }
    }
    pub fn sub(&self, o: &Self) -> Self {
        self.add(&o.neg())
    }
    pub fn mul(&self, o: &Self) -> Self {
        Self::make(self.neg != o.neg, mag_mul(&self.mag, &o.mag))
    }
    /// Truncating division; `None` on division by zero.
    pub fn div(&self, o: &Self) -> Option<Self> {
        if o.is_zero() {
            return None;
        }
        let (q, _) = mag_divmod(&self.mag, &o.mag);
        Some(Self::make(self.neg != o.neg, q))
    }
    /// Remainder with the dividend's sign; `None` on division by zero.
    pub fn rem(&self, o: &Self) -> Option<Self> {
        if o.is_zero() {
            return None;
        }
        let (_, r) = mag_divmod(&self.mag, &o.mag);
        Some(Self::make(self.neg, r))
    }
    /// Exponentiation; `None` for a negative exponent.
    pub fn pow(&self, o: &Self) -> Option<Self> {
        if o.neg {
            return None;
        }
        let mut e = o.to_i128().unwrap_or(i128::MAX) as u128;
        let mut base = self.clone();
        let mut acc = Self::from_u64(1);
        while e > 0 {
            if e & 1 == 1 {
                acc = acc.mul(&base);
            }
            e >>= 1;
            if e > 0 {
                base = base.mul(&base);
            }
        }
        Some(acc)
    }

    pub fn shl(&self, count: u64) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        let limbs = (count / 64) as usize;
        let bits = (count % 64) as u32;
        let mut mag = vec![0u64; limbs];
        let mut carry = 0u64;
        for &d in self.mag.iter() {
            if bits == 0 {
                mag.push(d);
            } else {
                mag.push((d << bits) | carry);
                carry = d >> (64 - bits);
            }
        }
        if carry != 0 {
            mag.push(carry);
        }
        Self::make(self.neg, mag)
    }
    /// Arithmetic right shift (floors toward negative infinity).
    pub fn shr(&self, count: u64) -> Self {
        let limbs = (count / 64) as usize;
        let bits = (count % 64) as u32;
        if limbs >= self.mag.len() {
            return if self.neg {
                Self::from_i128(-1)
            } else {
                Self::zero()
            };
        }
        let mut mag: Vec<u64> = self.mag[limbs..].to_vec();
        let mut lost = self.mag[..limbs].iter().any(|&d| d != 0);
        if bits != 0 {
            let mut prev = 0u64;
            if mag.first().map(|d| d & ((1 << bits) - 1) != 0) == Some(true) {
                lost = true;
            }
            for d in mag.iter_mut().rev() {
                let cur = *d;
                *d = (cur >> bits) | (prev << (64 - bits));
                prev = cur;
            }
        }
        let out = Self::make(self.neg, mag);
        // Negative values floor: any lost bit rounds away from zero.
        if self.neg && lost {
            out.sub(&Self::from_u64(1))
        } else {
            out
        }
    }

    pub fn bitand(&self, o: &Self) -> Self {
        self.bitop(o, |a, b| a & b)
    }
    pub fn bitor(&self, o: &Self) -> Self {
        self.bitop(o, |a, b| a | b)
    }
    pub fn bitxor(&self, o: &Self) -> Self {
        self.bitop(o, |a, b| a ^ b)
    }
    pub fn not(&self) -> Self {
        // ~x == -x - 1
        self.neg().sub(&Self::from_u64(1))
    }
    /// Bitwise op over two's-complement representations of arbitrary width.
    fn bitop(&self, o: &Self, f: fn(u64, u64) -> u64) -> Self {
        let n = self.mag.len().max(o.mag.len()) + 1;
        let a = self.twos(n);
        let b = o.twos(n);
        let out: Vec<u64> = (0..n).map(|i| f(a[i], b[i])).collect();
        Self::from_twos(out)
    }
    fn twos(&self, n: usize) -> Vec<u64> {
        let mut v = vec![0u64; n];
        for (i, &d) in self.mag.iter().enumerate() {
            v[i] = d;
        }
        if self.neg {
            for d in v.iter_mut() {
                *d = !*d;
            }
            let mut carry = 1u128;
            for d in v.iter_mut() {
                let s = *d as u128 + carry;
                *d = s as u64;
                carry = s >> 64;
                if carry == 0 {
                    break;
                }
            }
        }
        v
    }
    fn from_twos(mut v: Vec<u64>) -> Self {
        let neg = v.last().map(|&d| d >> 63 == 1).unwrap_or(false);
        if neg {
            // Negate: invert every limb, then add one.
            for d in v.iter_mut() {
                *d = !*d;
            }
            let mut carry = 1u128;
            for d in v.iter_mut() {
                let s = *d as u128 + carry;
                *d = s as u64;
                carry = s >> 64;
                if carry == 0 {
                    break;
                }
            }
        }
        Self::make(neg, v)
    }

    pub fn cmp(&self, o: &Self) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_cmp(&self.mag, &o.mag),
            (true, true) => mag_cmp(&o.mag, &self.mag),
        }
    }
    /// Exact comparison with a finite f64 (NaN/±∞ handled by the caller).
    pub fn cmp_f64(&self, n: f64) -> Option<Ordering> {
        if n.is_nan() {
            return None;
        }
        if n == f64::INFINITY {
            return Some(Ordering::Less);
        }
        if n == f64::NEG_INFINITY {
            return Some(Ordering::Greater);
        }
        // Compare with the integer part, then break ties on the fraction.
        let trunc = Self::from_f64(n.trunc()).expect("finite trunc");
        match self.cmp(&trunc) {
            Ordering::Equal => {
                let frac = n - n.trunc();
                if frac > 0.0 {
                    Some(Ordering::Less)
                } else if frac < 0.0 {
                    Some(Ordering::Greater)
                } else {
                    Some(Ordering::Equal)
                }
            }
            o => Some(o),
        }
    }
    pub fn eq_f64(&self, n: f64) -> bool {
        n.is_finite() && n.fract() == 0.0 && self.cmp_f64(n) == Some(Ordering::Equal)
    }
    /// Exact conversion from an integral finite f64.
    pub fn from_f64(n: f64) -> Option<Self> {
        if !n.is_finite() || n.fract() != 0.0 {
            return None;
        }
        if n == 0.0 {
            return Some(Self::zero());
        }
        let neg = n < 0.0;
        let a = n.abs();
        let bits = a.to_bits();
        let exp = ((bits >> 52) & 0x7FF) as i64 - 1075;
        let frac = if (bits >> 52) & 0x7FF == 0 {
            bits & ((1u64 << 52) - 1)
        } else {
            (bits & ((1u64 << 52) - 1)) | (1u64 << 52)
        };
        let base = Self::make(neg, vec![frac]);
        Some(if exp >= 0 {
            base.shl(exp as u64)
        } else {
            base.shr((-exp) as u64)
        })
    }
    /// Correctly-rounded conversion to f64 (round-to-nearest, ties to even).
    pub fn to_f64(&self) -> f64 {
        let bl = self.bit_len();
        if bl == 0 {
            return 0.0;
        }
        let sign = if self.neg { -1.0 } else { 1.0 };
        if bl <= 64 {
            return self.mag[0] as f64 * sign;
        }
        // Work on the magnitude (`shr`/`shl` are arithmetic and would skew a negative value).
        let mag = Self {
            neg: false,
            mag: self.mag.clone(),
        };
        // Take the top 54 bits; round to 53 with a sticky bit for the rest.
        let shift = (bl - 54) as u64;
        let head_big = mag.shr(shift);
        let head = *head_big.mag.first().unwrap_or(&0); // 54 bits
        let sticky = head_big.shl(shift).cmp(&mag) != std::cmp::Ordering::Equal;
        let q = head >> 1;
        let round = head & 1 == 1;
        let up = round && (sticky || q & 1 == 1);
        let m = q + up as u64;
        m as f64 * 2f64.powi(shift as i32 + 1) * sign
    }

    /// Parse from digits (no sign) in the given radix.
    pub fn parse_radix(text: &str, radix: u32) -> Option<Self> {
        let mut acc = Self::zero();
        let r = Self::from_u64(radix as u64);
        let mut any = false;
        for c in text.chars() {
            let d = c.to_digit(radix)?;
            acc = acc.mul(&r).add(&Self::from_u64(d as u64));
            any = true;
        }
        if any {
            Some(acc)
        } else {
            None
        }
    }
    /// Decimal digits (used by StringToBigInt with an optional leading sign handled outside).
    pub fn parse_dec(text: &str) -> Option<Self> {
        if !text.chars().all(|c| c.is_ascii_digit()) || text.is_empty() {
            return None;
        }
        Self::parse_radix(text, 10)
    }

    pub fn to_string_radix(&self, radix: u32) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        let mut digits = Vec::new();
        let mut cur = self.mag.as_ref().clone();
        let r = vec![radix as u64];
        while !cur.is_empty() {
            let (q, rem) = mag_divmod(&cur, &r);
            let d = *rem.first().unwrap_or(&0) as u32;
            digits.push(std::char::from_digit(d, radix).unwrap());
            cur = q;
        }
        if self.neg {
            digits.push('-');
        }
        digits.iter().rev().collect()
    }
}

impl std::fmt::Display for JsBigInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_radix(10))
    }
}

impl PartialEq for JsBigInt {
    fn eq(&self, other: &Self) -> bool {
        self.neg == other.neg && self.mag == other.mag
    }
}
impl Eq for JsBigInt {}

impl std::hash::Hash for JsBigInt {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.neg.hash(state);
        self.mag.hash(state);
    }
}

impl From<i128> for JsBigInt {
    fn from(v: i128) -> Self {
        Self::from_i128(v)
    }
}
impl From<i64> for JsBigInt {
    fn from(v: i64) -> Self {
        Self::from_i128(v as i128)
    }
}
impl From<u64> for JsBigInt {
    fn from(v: u64) -> Self {
        Self::from_u64(v)
    }
}

#[cfg(test)]
mod tests {
    use super::JsBigInt;
    use std::cmp::Ordering;

    fn big(s: &str) -> JsBigInt {
        let (neg, digits) = match s.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, s),
        };
        let v = JsBigInt::parse_dec(digits).unwrap();
        if neg {
            v.neg()
        } else {
            v
        }
    }

    #[test]
    fn parse_and_display_round_trip() {
        for s in [
            "0",
            "1",
            "-1",
            "18446744073709551616",
            "-340282366920938463463374607431768211456",
            "123456789012345678901234567890123456789012345678901234567890",
        ] {
            assert_eq!(big(s).to_string(), s);
        }
        assert_eq!(JsBigInt::parse_radix("ff", 16).unwrap().to_string(), "255");
        assert_eq!(JsBigInt::parse_radix("", 10), None);
        assert_eq!(JsBigInt::parse_radix("1_0", 10), None);
    }

    #[test]
    fn arithmetic_signs_and_carries() {
        let a = big("18446744073709551615"); // 2^64 - 1
        assert_eq!(
            a.add(&JsBigInt::from_u64(1)).to_string(),
            "18446744073709551616"
        );
        assert_eq!(a.sub(&a), JsBigInt::zero());
        assert_eq!(big("5").sub(&big("7")).to_string(), "-2");
        assert_eq!(big("-5").mul(&big("-7")).to_string(), "35");
        assert_eq!(
            a.mul(&a).to_string(),
            "340282366920938463426481119284349108225"
        );
    }

    #[test]
    fn division_truncates_toward_zero() {
        // BigInt / and % truncate (like Rust integer division), unlike shr which floors.
        assert_eq!(big("7").div(&big("2")).unwrap().to_string(), "3");
        assert_eq!(big("-7").div(&big("2")).unwrap().to_string(), "-3");
        assert_eq!(big("7").rem(&big("-2")).unwrap().to_string(), "1");
        assert_eq!(big("-7").rem(&big("2")).unwrap().to_string(), "-1");
        assert_eq!(big("7").div(&JsBigInt::zero()), None);
        assert_eq!(big("7").rem(&JsBigInt::zero()), None);
    }

    #[test]
    fn pow_and_negative_exponent() {
        assert_eq!(
            big("2").pow(&big("128")).unwrap().to_string(),
            "340282366920938463463374607431768211456"
        );
        assert_eq!(big("-3").pow(&big("3")).unwrap().to_string(), "-27");
        assert_eq!(big("2").pow(&big("-1")), None);
        assert_eq!(big("7").pow(&JsBigInt::zero()).unwrap().to_string(), "1");
    }

    #[test]
    fn shifts_floor_toward_negative_infinity() {
        assert_eq!(
            big("1").shl(130).to_string(),
            "1361129467683753853853498429727072845824"
        );
        assert_eq!(big("1").shl(130).shr(130).to_string(), "1");
        // Arithmetic right shift floors: -1 >> anything is -1, -5 >> 1 is -3.
        assert_eq!(big("-1").shr(200).to_string(), "-1");
        assert_eq!(big("-5").shr(1).to_string(), "-3");
        assert_eq!(big("5").shr(1).to_string(), "2");
        assert_eq!(big("4").shr(70), JsBigInt::zero());
    }

    #[test]
    fn twos_complement_bitwise() {
        assert_eq!(big("-1").bitand(&big("255")).to_string(), "255");
        assert_eq!(big("-2").bitor(&big("1")).to_string(), "-1");
        assert_eq!(big("-1").bitxor(&big("-1")), JsBigInt::zero());
        assert_eq!(big("0").not().to_string(), "-1");
        assert_eq!(
            big("18446744073709551616").not().to_string(),
            "-18446744073709551617"
        );
        // n & (2^64 - 1) is n mod 2^64 (asUintN's masking identity).
        let mask = big("18446744073709551615");
        assert_eq!(big("-1").bitand(&mask), mask);
    }

    #[test]
    fn exact_f64_comparison_at_extremes() {
        // 2^53 and 2^53 + 1: f64 can't tell them apart, the exact comparison must.
        let n = 9007199254740992f64; // 2^53
        assert_eq!(big("9007199254740993").cmp_f64(n), Some(Ordering::Greater));
        assert!(big("9007199254740992").eq_f64(n));
        assert!(!big("9007199254740993").eq_f64(n));
        assert_eq!(big("1").cmp_f64(1.5), Some(Ordering::Less));
        assert_eq!(big("2").cmp_f64(1.5), Some(Ordering::Greater));
        assert_eq!(
            big("-1").cmp_f64(f64::NEG_INFINITY),
            Some(Ordering::Greater)
        );
        assert_eq!(big("1").cmp_f64(f64::NAN), None);
        // Far beyond i128: the magnitude still orders correctly.
        assert_eq!(
            big("2").pow(&big("200")).unwrap().cmp_f64(1e60),
            Some(Ordering::Greater)
        );
        assert_eq!(
            big("2").pow(&big("200")).unwrap().cmp_f64(1e61),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn f64_conversions() {
        assert_eq!(JsBigInt::from_f64(0.5), None);
        assert_eq!(JsBigInt::from_f64(f64::NAN), None);
        assert_eq!(
            JsBigInt::from_f64(1e21).unwrap().to_string(),
            "1000000000000000000000"
        );
        assert_eq!(big("-9007199254740992").to_f64(), -9007199254740992.0);
        assert_eq!(big("2").pow(&big("100")).unwrap().to_f64(), 2f64.powi(100));
    }

    #[test]
    fn i128_round_trips_and_wrapping() {
        assert_eq!(JsBigInt::from_i128(i128::MIN).to_i128(), Some(i128::MIN));
        assert_eq!(JsBigInt::from_i128(i128::MAX).to_i128(), Some(i128::MAX));
        let over = JsBigInt::from_i128(i128::MAX).add(&JsBigInt::from_u64(1));
        assert_eq!(over.to_i128(), None);
        assert_eq!(over.to_i128_wrapping(), i128::MIN);
    }
}
