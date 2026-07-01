//! `Intl.Segmenter` (grapheme = per code point; word/sentence = coarse boundaries).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, get_options_object as coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "Segmenter", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "segment", 1, |i, this, a| segment(i, &this, &arg(a, 0)));
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.Segmenter requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let granularity =
        get_option(i, &options, "granularity", &["grapheme", "word", "sentence"], Some("grapheme"))?
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
fn boundaries(s: &[u16], granularity: &str) -> Vec<(usize, bool)> {
    // Returns (start, isWordLike) for each segment.
    let n = s.len();
    if n == 0 {
        return Vec::new();
    }
    match granularity {
        "grapheme" => {
            // Per code point (surrogate pairs kept together).
            let mut out = Vec::new();
            let mut idx = 0;
            while idx < n {
                out.push((idx, false));
                let is_high = (0xD800..=0xDBFF).contains(&s[idx]);
                idx += if is_high && idx + 1 < n { 2 } else { 1 };
            }
            out
        }
        "word" => {
            // Runs of "word" characters (letters/digits) vs. non-word, with UAX #29 infix handling:
            // a MidNum/MidLetter/MidNumLet (e.g. "." or ",") between two word characters does not
            // break, so "1.23" and "3,000" stay whole.
            let mut out = Vec::new();
            let mut idx = 0;
            while idx < n {
                let start = idx;
                let word = is_word_cu(s[idx]);
                if word {
                    idx += 1;
                    while idx < n {
                        if is_word_cu(s[idx]) {
                            idx += 1;
                        } else if idx + 1 < n && mid_joins(s[idx - 1], s[idx], s[idx + 1]) {
                            idx += 2;
                        } else {
                            break;
                        }
                    }
                } else {
                    idx += 1;
                    while idx < n && !is_word_cu(s[idx]) {
                        idx += 1;
                    }
                }
                out.push((start, word));
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

fn is_num_cu(c: u16) -> bool {
    (0x30..=0x39).contains(&c)
}
fn is_alpha_cu(c: u16) -> bool {
    matches!(c, 0x41..=0x5A | 0x61..=0x7A) || c >= 0x80
}

/// Whether a MidNum/MidLetter/MidNumLet character `mid` joins its neighbours (UAX #29 WB6/7/11/12):
/// MidNumLet (. ' ’ ⁄) joins two numbers or two letters; MidNum (, ;) only two numbers; MidLetter
/// (: ·) only two letters.
fn mid_joins(prev: u16, mid: u16, next: u16) -> bool {
    let both_num = is_num_cu(prev) && is_num_cu(next);
    let both_alpha = is_alpha_cu(prev) && is_alpha_cu(next);
    match mid {
        0x2E | 0x27 | 0x2019 | 0x2044 => both_num || both_alpha,
        0x2C | 0x3B => both_num,
        0x3A | 0x00B7 => both_alpha,
        _ => false,
    }
}

fn is_word_cu(c: u16) -> bool {
    let c = c as u32;
    (c >= 0x30 && c <= 0x39)
        || (c >= 0x41 && c <= 0x5A)
        || (c >= 0x61 && c <= 0x7A)
        || c >= 0x80 // treat most non-ASCII as word-like (coarse)
}

fn segment(i: &mut Interp, this: &Value, input: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__sg")?;
    let granularity = match o.borrow().props.get("__sg_granularity").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "grapheme".to_string(),
    };
    let s = ab(i.to_string(input))?.to_string();
    let units: Vec<u16> = s.encode_utf16().collect();
    let bnds = boundaries(&units, &granularity);

    // Build an array-like "Segments" object that is iterable and has a `containing` method. We
    // pre-materialise the segment records.
    let segments = i.new_object();
    let mut records: Vec<Value> = Vec::new();
    for (k, &(start, wordlike)) in bnds.iter().enumerate() {
        let end = bnds.get(k + 1).map(|&(e, _)| e).unwrap_or(units.len());
        let seg: String = String::from_utf16_lossy(&units[start..end]);
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
        segments
            .borrow_mut()
            .props
            .insert(crate::interpreter::Interp::sym_key(&sym), crate::value::Property::builtin(Value::Obj(f)));
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
                s.encode_utf16().count() as i64
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
    let get = |k: &str| o.borrow().props.get(k).map(|p| p.value.clone()).unwrap_or(Value::Undefined);
    let res = i.new_object();
    set_data(&res, "locale", get("__sg_locale"));
    set_data(&res, "granularity", get("__sg_granularity"));
    Ok(Value::Obj(res))
}
