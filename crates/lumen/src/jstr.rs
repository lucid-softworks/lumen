//! UTF-16 string semantics over Rust strings.
//!
//! JS strings are sequences of UTF-16 code units, including lone surrogates, which a Rust `str`
//! cannot hold. Lone surrogates are *smuggled* as the plane-16 private-use scalars
//! `U+10F800 + (unit - 0xD800)` (the same trick as Python's surrogateescape), so any unit
//! sequence round-trips through `Value::Str` while remaining a valid `str`. Everything that
//! speaks spec semantics — `length`, indexing, `charCodeAt`, JSON escapes, relational
//! comparison — goes through this module. A *real* character in the smuggle range
//! (U+10F800..U+10FFFF) is itself represented as its smuggled surrogate PAIR, so the encoding is
//! total: adjacent smuggled high+low is the canonical form of those two code points' character,
//! and a solitary smuggled scalar is always a lone surrogate.

/// First smuggled scalar: encodes the lone surrogate U+D800.
pub const SMUGGLE_BASE: u32 = 0x10F800;

/// If `c` is a smuggled lone surrogate, the surrogate code unit it encodes.
#[inline]
pub fn smuggled(c: char) -> Option<u16> {
    let v = c as u32;
    if (SMUGGLE_BASE..SMUGGLE_BASE + 0x800).contains(&v) {
        Some((v - SMUGGLE_BASE + 0xD800) as u16)
    } else {
        None
    }
}

/// Smuggle a surrogate code unit (0xD800..=0xDFFF) into its private-use scalar.
#[inline]
pub fn smuggle(unit: u16) -> char {
    debug_assert!((0xD800..0xE000).contains(&(unit as u32)));
    char::from_u32(SMUGGLE_BASE + (unit as u32 - 0xD800)).unwrap()
}

/// The UTF-16 code units of `s` (smuggled scalars decode to their lone surrogates).
pub fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match smuggled(c) {
            Some(u) => out.push(u),
            None => {
                let mut buf = [0u16; 2];
                out.extend_from_slice(c.encode_utf16(&mut buf));
            }
        }
    }
    out
}

/// The UTF-16 length of `s` without materializing the units.
pub fn unit_len(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if smuggled(c).is_some() {
                1
            } else {
                c.len_utf16()
            }
        })
        .sum()
}

/// Rebuild a string from code units: valid surrogate pairs combine into their code point, lone
/// surrogates are smuggled.
pub fn from_units(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i] as u32;
        if (0xD800..0xDC00).contains(&u)
            && i + 1 < units.len()
            && (0xDC00..0xE000).contains(&(units[i + 1] as u32))
        {
            let c = 0x10000 + ((u - 0xD800) << 10) + (units[i + 1] as u32 - 0xDC00);
            if c < SMUGGLE_BASE {
                out.push(char::from_u32(c).unwrap());
            } else {
                // A real character in the smuggle range is stored as its smuggled pair.
                out.push(smuggle(units[i]));
                out.push(smuggle(units[i + 1]));
            }
            i += 2;
            continue;
        }
        if (0xD800..0xE000).contains(&u) {
            out.push(smuggle(units[i]));
        } else {
            out.push(char::from_u32(u).unwrap());
        }
        i += 1;
    }
    out
}

/// The single-unit string for one code unit.
pub fn unit_str(unit: u16) -> String {
    from_units(&[unit])
}

/// The spec's code-unit-wise string comparison (differs from `str` byte order for strings mixing
/// supplementary-plane characters with U+E000..U+FFFF, and for smuggled surrogates).
pub fn cmp_units(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ia = UnitIter::new(a);
    let mut ib = UnitIter::new(b);
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
            }
        }
    }
}

/// Iterator over `s`'s UTF-16 code units.
pub struct UnitIter<'a> {
    chars: std::str::Chars<'a>,
    trail: Option<u16>,
}

impl<'a> UnitIter<'a> {
    pub fn new(s: &'a str) -> Self {
        UnitIter {
            chars: s.chars(),
            trail: None,
        }
    }
}

impl Iterator for UnitIter<'_> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        if let Some(t) = self.trail.take() {
            return Some(t);
        }
        let c = self.chars.next()?;
        if let Some(u) = smuggled(c) {
            return Some(u);
        }
        let mut buf = [0u16; 2];
        let enc = c.encode_utf16(&mut buf);
        if enc.len() == 2 {
            self.trail = Some(enc[1]);
        }
        Some(enc[0])
    }
}

/// Whether `s` contains a lone surrogate: a smuggled scalar NOT part of an adjacent
/// high+low pair (which canonically encodes a real smuggle-range character).
pub fn has_lone_surrogate(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if smuggled_high(c).is_some() {
            if chars.peek().copied().and_then(smuggled_low).is_some() {
                chars.next();
                continue;
            }
            return true;
        }
        if smuggled(c).is_some() {
            return true;
        }
    }
    false
}

/// If `a` and `b` are a smuggled high+low pair, the real character they encode.
pub fn paired_char(a: char, b: char) -> Option<char> {
    let hi = smuggled_high(a)?;
    let lo = smuggled_low(b)?;
    char::from_u32(0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00))
}

/// Whether `c` is a smuggled HIGH surrogate (U+D800..U+DBFF).
#[inline]
fn smuggled_high(c: char) -> Option<u16> {
    smuggled(c).filter(|u| (0xD800..0xDC00).contains(&(*u as u32)))
}

/// Whether `c` is a smuggled LOW surrogate (U+DC00..U+DFFF).
#[inline]
fn smuggled_low(c: char) -> Option<u16> {
    smuggled(c).filter(|u| (0xDC00..0xE000).contains(&(*u as u32)))
}

/// String concatenation with the canonical-form fix-up: a smuggled high surrogate at the end of
/// `a` followed by a smuggled low surrogate at the start of `b` combines into the astral scalar,
/// so `"\uD834" + "\uDF06"` equals the literal `"𝌆"`.
pub fn concat(a: &str, b: &str) -> String {
    if let (Some(last), Some(first)) = (a.chars().next_back(), b.chars().next()) {
        if let (Some(hi), Some(lo)) = (smuggled_high(last), smuggled_low(first)) {
            let cp = 0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00);
            // Adjacent smuggled high+low IS the canonical form for smuggle-range characters.
            if cp < SMUGGLE_BASE {
                let mut out = String::with_capacity(a.len() + b.len());
                out.push_str(&a[..a.len() - last.len_utf8()]);
                out.push(char::from_u32(cp).unwrap());
                out.push_str(&b[first.len_utf8()..]);
                return out;
            }
        }
    }
    let mut out = String::with_capacity(a.len() + b.len());
    out.push_str(a);
    out.push_str(b);
    out
}

/// Canonicalize any adjacent smuggled high+low surrogate pairs inside `s` (needed after building
/// a string from independently produced pieces, e.g. `join` or `repeat`).
pub fn canonicalize(s: &str) -> Option<String> {
    let mut prev_high = false;
    for c in s.chars() {
        if prev_high && smuggled_low(c).is_some() {
            // Slow path: rebuild through the unit round-trip.
            return Some(from_units(&units(s)));
        }
        prev_high = smuggled_high(c).is_some();
    }
    None
}

/// The code points of `s`, with lone surrogates as their surrogate values (paired smuggled
/// high+low decode to the real character they canonically encode — see module docs).
pub fn code_points(s: &str) -> Vec<u32> {
    let units = units(s);
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i] as u32;
        if (0xD800..0xDC00).contains(&u)
            && i + 1 < units.len()
            && (0xDC00..0xE000).contains(&(units[i + 1] as u32))
        {
            out.push(0x10000 + ((u - 0xD800) << 10) + (units[i + 1] as u32 - 0xDC00));
            i += 2;
        } else {
            out.push(u);
            i += 1;
        }
    }
    out
}

/// Rebuild a string from code points (surrogate values become lone surrogates).
pub fn from_code_points(cps: &[u32]) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(cps.len());
    for &cp in cps {
        if cp < 0x10000 {
            units.push(cp as u16);
        } else {
            let v = cp - 0x10000;
            units.push(0xD800 + (v >> 10) as u16);
            units.push(0xDC00 + (v & 0x3FF) as u16);
        }
    }
    from_units(&units)
}
