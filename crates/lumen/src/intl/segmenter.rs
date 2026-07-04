//! `Intl.Segmenter` (grapheme = per code point; word/sentence = coarse boundaries).

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
    resolve_locale,
};
use super::{
    ab, arg, canonicalize_locale_list, get_options_object as coerce_options, make_service,
};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "Segmenter", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "segment", 1, |i, this, a| {
        segment(i, &this, &arg(a, 0))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.Segmenter requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let granularity = get_option(
        i,
        &options,
        "granularity",
        &["grapheme", "word", "sentence"],
        Some("grapheme"),
    )?
    .unwrap();
    let resolved = resolve_locale(i, &requested, &[]);
    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.Segmenter")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__sg", Value::Bool(true));
    set_builtin(&obj, "__sg_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__sg_granularity", Value::from_string(granularity));
    Ok(Value::Obj(obj))
}

/// Boundaries (UTF-16 code-unit offsets) between segments of `s` at the given granularity.
/// A UAX #29 Grapheme_Cluster_Break class (plus Extended_Pictographic for GB11).
#[derive(Clone, Copy, PartialEq)]
enum Gcb {
    Other,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    Ri,
    SpacingMark,
    L,
    V,
    T,
    Lv,
    Lvt,
    ExtPict,
}

fn in_ranges(r: Option<&'static [(u32, u32)]>, cp: u32) -> bool {
    match r {
        Some(ranges) => ranges
            .binary_search_by(|&(lo, hi)| {
                if cp < lo {
                    std::cmp::Ordering::Greater
                } else if cp > hi {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok(),
        None => false,
    }
}

fn gcb_class(cp: u32) -> Gcb {
    use crate::unicode_props::lookup;
    match cp {
        0x0D => return Gcb::Cr,
        0x0A => return Gcb::Lf,
        0x200D => return Gcb::Zwj,
        // Emoji modifiers (skin tones) have Grapheme_Cluster_Break=Extend (UAX #29, GB9).
        0x1F3FB..=0x1F3FF => return Gcb::Extend,
        // Hangul Jamo (fixed blocks) + conjoining Hangul syllables.
        0x1100..=0x115F | 0xA960..=0xA97C => return Gcb::L,
        0x1160..=0x11A7 | 0xD7B0..=0xD7C6 => return Gcb::V,
        0x11A8..=0x11FF | 0xD7CB..=0xD7FB => return Gcb::T,
        0xAC00..=0xD7A3 => {
            return if (cp - 0xAC00).is_multiple_of(28) {
                Gcb::Lv
            } else {
                Gcb::Lvt
            };
        }
        _ => {}
    }
    if in_ranges(lookup("regionalindicator", None), cp) {
        return Gcb::Ri;
    }
    if in_ranges(lookup("graphemeextend", None), cp) {
        return Gcb::Extend;
    }
    if in_ranges(lookup("spacingmark", None), cp) {
        return Gcb::SpacingMark;
    }
    if in_ranges(lookup("extendedpictographic", None), cp) {
        return Gcb::ExtPict;
    }
    if in_ranges(lookup("gc", Some("cc")), cp)
        || in_ranges(lookup("gc", Some("cf")), cp)
        || in_ranges(lookup("gc", Some("zl")), cp)
        || in_ranges(lookup("gc", Some("zp")), cp)
    {
        return Gcb::Control;
    }
    Gcb::Other
}

/// UAX #29 grapheme-cluster boundaries as `(start_offset, false)` in UTF-16 code units.
// The GB3-13 rules deliberately map to separate `else if` arms (several returning the same bool) so
// each stays traceable to its spec rule; collapsing them would obscure that mapping.
#[allow(clippy::if_same_then_else)]
fn grapheme_boundaries(s: &[u16]) -> Vec<(usize, bool)> {
    // Decode to (utf16 offset, code point), keeping surrogate pairs together.
    let n = s.len();
    let mut cps: Vec<(usize, u32)> = Vec::new();
    let mut idx = 0;
    while idx < n {
        let hi = s[idx];
        if (0xD800..=0xDBFF).contains(&hi) && idx + 1 < n && (0xDC00..=0xDFFF).contains(&s[idx + 1])
        {
            let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (s[idx + 1] as u32 - 0xDC00));
            cps.push((idx, cp));
            idx += 2;
        } else {
            cps.push((idx, hi as u32));
            idx += 1;
        }
    }
    if cps.is_empty() {
        return Vec::new();
    }
    let cls: Vec<Gcb> = cps.iter().map(|&(_, cp)| gcb_class(cp)).collect();
    let mut out = vec![(0usize, false)];
    // State over the prefix ending at k-1: RI run length and the GB11 "ExtPict Extend* ZWJ" tracker.
    let mut ri_run: usize = if cls[0] == Gcb::Ri { 1 } else { 0 };
    let mut pict_active = cls[0] == Gcb::ExtPict;
    let mut zwj_seen = false;
    for k in 1..cps.len() {
        let a = cls[k - 1];
        let b = cls[k];
        let no_break = if a == Gcb::Cr && b == Gcb::Lf {
            true // GB3
        } else if matches!(a, Gcb::Control | Gcb::Cr | Gcb::Lf)
            || matches!(b, Gcb::Control | Gcb::Cr | Gcb::Lf)
        {
            false // GB4 / GB5
        } else if a == Gcb::L && matches!(b, Gcb::L | Gcb::V | Gcb::Lv | Gcb::Lvt) {
            true // GB6
        } else if matches!(a, Gcb::Lv | Gcb::V) && matches!(b, Gcb::V | Gcb::T) {
            true // GB7
        } else if matches!(a, Gcb::Lvt | Gcb::T) && b == Gcb::T {
            true // GB8
        } else if matches!(b, Gcb::Extend | Gcb::Zwj) {
            true // GB9
        } else if b == Gcb::SpacingMark {
            true // GB9a
        } else if pict_active && zwj_seen && b == Gcb::ExtPict {
            true // GB11
        } else {
            a == Gcb::Ri && b == Gcb::Ri && ri_run % 2 == 1 // GB12/13
        };
        if !no_break {
            out.push((cps[k].0, false));
        }
        // Fold cls[k] into the running state.
        ri_run = if b == Gcb::Ri {
            if no_break {
                ri_run + 1
            } else {
                1
            }
        } else {
            0
        };
        match b {
            Gcb::ExtPict => {
                pict_active = true;
                zwj_seen = false;
            }
            Gcb::Extend => zwj_seen = false,
            Gcb::Zwj => {
                if pict_active {
                    zwj_seen = true;
                }
            }
            _ => {
                pict_active = false;
                zwj_seen = false;
            }
        }
    }
    out
}

fn boundaries(s: &[u16], granularity: &str) -> Vec<(usize, bool)> {
    // Returns (start, isWordLike) for each segment.
    let n = s.len();
    if n == 0 {
        return Vec::new();
    }
    match granularity {
        "grapheme" => grapheme_boundaries(s),
        "word" => {
            // Decode to (offset, code point), pairing valid surrogates so an astral character is
            // one word character; a lone surrogate stays as its own (non-word) unit.
            let mut cps: Vec<(usize, u32)> = Vec::new();
            let mut k = 0;
            while k < n {
                let u = s[k] as u32;
                if (0xD800..0xDC00).contains(&u)
                    && k + 1 < n
                    && (0xDC00..0xE000).contains(&(s[k + 1] as u32))
                {
                    cps.push((
                        k,
                        0x10000 + ((u - 0xD800) << 10) + (s[k + 1] as u32 - 0xDC00),
                    ));
                    k += 2;
                } else {
                    cps.push((k, u));
                    k += 1;
                }
            }
            // Runs of "word" characters (letters/digits) vs. non-word, with UAX #29 infix handling:
            // a MidNum/MidLetter/MidNumLet (e.g. "." or ",") between two word characters does not
            // break, so "1.23" and "3,000" stay whole.
            let m = cps.len();
            let mut out = Vec::new();
            let mut idx = 0;
            while idx < m {
                let start = idx;
                let word = is_word_cp(cps[idx].1);
                if word {
                    idx += 1;
                    while idx < m {
                        if is_word_cp(cps[idx].1) {
                            idx += 1;
                        } else if idx + 1 < m
                            && mid_joins(cps[idx - 1].1, cps[idx].1, cps[idx + 1].1)
                        {
                            idx += 2;
                        } else {
                            break;
                        }
                    }
                } else {
                    idx += 1;
                    // WB3d: runs of spaces stay together; any other non-word character
                    // (punctuation, lone surrogate, ...) is its own segment (WB999).
                    if cps[start].1 == 0x20 {
                        while idx < m && cps[idx].1 == 0x20 {
                            idx += 1;
                        }
                    }
                }
                out.push((cps[start].0, word));
            }
            out
        }
        _ => {
            // sentence: split after a run ending in . ! ? followed by whitespace.
            let mut out = vec![(0usize, false)];
            let mut idx = 0;
            while idx < n {
                let c = s[idx];
                if c == b'.' as u16 || c == b'!' as u16 || c == b'?' as u16 {
                    // include trailing spaces in this sentence
                    let mut j = idx + 1;
                    while j < n && (s[j] == b' ' as u16 || s[j] == b'\n' as u16) {
                        j += 1;
                    }
                    if j < n {
                        out.push((j, false));
                    }
                    idx = j;
                } else {
                    idx += 1;
                }
            }
            out
        }
    }
}

fn is_num_cp(c: u32) -> bool {
    (0x30..=0x39).contains(&c)
}
fn is_alpha_cp(c: u32) -> bool {
    matches!(c, 0x41..=0x5A | 0x61..=0x7A) || (c >= 0x80 && !(0xD800..0xE000).contains(&c))
}

/// Whether a MidNum/MidLetter/MidNumLet character `mid` joins its neighbours (UAX #29 WB6/7/11/12):
/// MidNumLet (. ' ’ ⁄) joins two numbers or two letters; MidNum (, ;) only two numbers; MidLetter
/// (: ·) only two letters.
fn mid_joins(prev: u32, mid: u32, next: u32) -> bool {
    let both_num = is_num_cp(prev) && is_num_cp(next);
    let both_alpha = is_alpha_cp(prev) && is_alpha_cp(next);
    match mid {
        0x2E | 0x27 | 0x2019 | 0x2044 => both_num || both_alpha,
        0x2C | 0x3B => both_num,
        0x3A | 0x00B7 => both_alpha,
        _ => false,
    }
}

fn is_word_cp(c: u32) -> bool {
    (0x30..=0x39).contains(&c)
        || (0x41..=0x5A).contains(&c)
        || (0x61..=0x7A).contains(&c)
        || (c >= 0x80 && !(0xD800..0xE000).contains(&c))
    // treat most non-ASCII as word-like (coarse); unpaired surrogates are not word characters
}

fn segment(i: &mut Interp, this: &Value, input: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__sg")?;
    let granularity = match o
        .borrow()
        .props
        .get("__sg_granularity")
        .map(|p| p.value.clone())
    {
        Some(Value::Str(s)) => s.to_string(),
        _ => "grapheme".to_string(),
    };
    let s = ab(i.to_string(input))?.to_string();
    // jstr units: lone surrogates must round-trip (each is its own single-unit segment).
    let units: Vec<u16> = crate::jstr::units(&s);
    let bnds = boundaries(&units, &granularity);

    // Build an array-like "Segments" object that is iterable and has a `containing` method. We
    // pre-materialise the segment records.
    let segments = i.new_object();
    let mut records: Vec<Value> = Vec::new();
    for (k, &(start, wordlike)) in bnds.iter().enumerate() {
        let end = bnds.get(k + 1).map(|&(e, _)| e).unwrap_or(units.len());
        let seg: String = crate::jstr::from_units(&units[start..end]);
        let rec = i.new_object();
        set_data(&rec, "segment", Value::from_string(seg));
        set_data(&rec, "index", Value::Num(start as f64));
        set_data(&rec, "input", Value::from_string(s.clone()));
        if granularity == "word" {
            set_data(&rec, "isWordLike", Value::Bool(wordlike));
        }
        records.push(Value::Obj(rec));
    }
    // Make `segments` iterable by giving it a @@iterator returning an array iterator over records.
    let arr = i.make_array(records);
    set_builtin(&segments, "__seg_records", arr.clone());
    if let Some(sym) = i.iterator_sym.clone() {
        let f = i.make_native("[Symbol.iterator]", 0, |i, this, _| {
            let recs = ab(i.get_member(&this, "__seg_records"))?;
            let itf = ab(i.get_member(&recs, "values"))?;
            ab(i.call(itf, recs, &[]))
        });
        segments.borrow_mut().props.insert(
            crate::interpreter::Interp::sym_key(&sym),
            crate::value::Property::builtin(Value::Obj(f)),
        );
    }
    it_containing(i, &segments);
    Ok(Value::Obj(segments))
}

fn it_containing(i: &mut Interp, segments: &Gc) {
    let f = i.make_native("containing", 1, |i, this, a| {
        let idx = ab(i.to_number(&arg(a, 0)))? as i64;
        let recs = ab(i.get_member(&this, "__seg_records"))?;
        let len = ab(i.get_member(&recs, "length"))?;
        let len = ab(i.to_number(&len))? as usize;
        for k in 0..len {
            let rec = ab(i.get_member(&recs, &k.to_string()))?;
            let idxv = ab(i.get_member(&rec, "index"))?;
            let start = ab(i.to_number(&idxv))? as i64;
            let seg = ab(i.get_member(&rec, "segment"))?;
            let slen = if let Value::Str(s) = &seg {
                crate::jstr::unit_len(s) as i64
            } else {
                0
            };
            if idx >= start && idx < start + slen {
                return Ok(rec);
            }
        }
        Ok(Value::Undefined)
    });
    set_builtin(segments, "containing", Value::Obj(f));
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__sg")?;
    let get = |k: &str| {
        o.borrow()
            .props
            .get(k)
            .map(|p| p.value.clone())
            .unwrap_or(Value::Undefined)
    };
    let res = i.new_object();
    set_data(&res, "locale", get("__sg_locale"));
    set_data(&res, "granularity", get("__sg_granularity"));
    Ok(Value::Obj(res))
}
