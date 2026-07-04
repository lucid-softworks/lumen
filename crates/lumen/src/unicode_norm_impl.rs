//! Unicode normalization (NFC/NFD/NFKC/NFKD) over the generated `unicode_norm` tables.
//! Operates on code points; Hangul syllables decompose/compose algorithmically (UAX #15).

use crate::unicode_norm::{CANON_DECOMP, CCC, COMPAT_DECOMP, COMPOSE};

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The canonical combining class of `cp` (0 for starters).
pub fn ccc(cp: u32) -> u8 {
    match CCC.binary_search_by_key(&cp, |&(c, _)| c) {
        Ok(k) => CCC[k].1,
        Err(_) => 0,
    }
}

fn canon_decomp(cp: u32) -> Option<(u32, u32)> {
    CANON_DECOMP
        .binary_search_by_key(&cp, |&(c, _, _)| c)
        .ok()
        .map(|k| (CANON_DECOMP[k].1, CANON_DECOMP[k].2))
}

fn compat_decomp(cp: u32) -> Option<&'static [u32]> {
    COMPAT_DECOMP
        .binary_search_by_key(&cp, |&(c, _)| c)
        .ok()
        .map(|k| COMPAT_DECOMP[k].1)
}

fn compose_pair(a: u32, b: u32) -> Option<u32> {
    // Hangul LV / LVT composition.
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return Some(S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a)
        && (a - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&b)
    {
        return Some(a + (b - T_BASE));
    }
    COMPOSE
        .binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b)))
        .ok()
        .map(|k| COMPOSE[k].2)
}

/// Recursively push the canonical (and optionally compatibility) decomposition of `cp`.
fn push_decomp(cp: u32, compat: bool, out: &mut Vec<u32>) {
    // Hangul syllable → L V (T).
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let s = cp - S_BASE;
        out.push(L_BASE + s / N_COUNT);
        out.push(V_BASE + (s % N_COUNT) / T_COUNT);
        if !s.is_multiple_of(T_COUNT) {
            out.push(T_BASE + s % T_COUNT);
        }
        return;
    }
    if let Some((a, b)) = canon_decomp(cp) {
        push_decomp(a, compat, out);
        if b != 0 {
            push_decomp(b, compat, out);
        }
        return;
    }
    if compat {
        if let Some(parts) = compat_decomp(cp) {
            // COMPAT_DECOMP is already fully NFKD-expanded.
            out.extend_from_slice(parts);
            return;
        }
    }
    out.push(cp);
}

/// Canonical ordering: stable-sort sequences of nonzero-class code points by combining class.
fn canonical_order(cps: &mut [u32]) {
    let mut i = 1;
    while i < cps.len() {
        let cc = ccc(cps[i]);
        if cc != 0 {
            let mut j = i;
            while j > 0 && ccc(cps[j - 1]) > cc {
                cps.swap(j - 1, j);
                j -= 1;
            }
        }
        i += 1;
    }
}

/// NFD (or NFKD with `compat`) of a code-point sequence.
pub fn decompose(cps: &[u32], compat: bool) -> Vec<u32> {
    let mut out = Vec::with_capacity(cps.len());
    for &cp in cps {
        push_decomp(cp, compat, &mut out);
    }
    canonical_order(&mut out);
    out
}

/// Canonical composition (UAX #15): recombine each starter with following unblocked marks
/// (a mark is blocked when a character of equal-or-higher class — or another starter — sits
/// between it and the starter).
pub fn compose(cps: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::with_capacity(cps.len());
    let mut starter: Option<usize> = None;
    let mut last_cc: i32 = -1;
    for &cp in cps {
        let cc = ccc(cp) as i32;
        if let Some(si) = starter {
            if last_cc < cc {
                if let Some(c) = compose_pair(out[si], cp) {
                    out[si] = c;
                    continue;
                }
            }
        }
        out.push(cp);
        if cc == 0 {
            starter = Some(out.len() - 1);
            last_cc = -1;
        } else {
            last_cc = cc;
        }
    }
    out
}

/// Normalize `cps` to the requested form ("NFC" | "NFD" | "NFKC" | "NFKD").
pub fn normalize(cps: &[u32], form: &str) -> Vec<u32> {
    let compat = form.starts_with("NFK");
    let d = decompose(cps, compat);
    if form.ends_with('C') {
        compose(&d)
    } else {
        d
    }
}
