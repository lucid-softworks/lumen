//! `Intl.NumberFormat` (standard notation; decimal/percent/currency/unit; English grouping).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "NumberFormat", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "format", 1, |i, this, a| {
        // `format` is a bound-ish getter in the spec; here it's a plain method (sufficient for most
        // tests) — but many tests read `.format` then call it, so return a stable per-instance fn.
        format_number(i, &this, &arg(a, 0))
    });
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        format_to_parts(i, &this, &arg(a, 0))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
    it.def_method(&proto, "formatRange", 2, |i, this, a| {
        format_range(i, &this, &arg(a, 0), &arg(a, 1))
    });
    it.def_method(&proto, "formatRangeToParts", 2, |i, this, a| {
        format_range_to_parts(i, &this, &arg(a, 0), &arg(a, 1))
    });
    // A `format` accessor that returns a bound function is what the spec mandates; provide it.
    install_format_getter(it, &proto);
}

/// The two range endpoints as intl mathematical values, rejecting NaN/undefined per
/// FormatNumericRange step 1 (`start`/`end` must not be undefined; NaN throws RangeError).
fn range_endpoints(i: &mut Interp, this: &Value, x: &Value, y: &Value) -> Result<(Gc, f64, f64), Value> {
    let o = instance(i, this)?;
    if matches!(x, Value::Undefined) || matches!(y, Value::Undefined) {
        return Err(i.make_error("TypeError", "formatRange requires two arguments"));
    }
    let a = to_intl_number(i, x)?;
    let b = to_intl_number(i, y)?;
    if a.is_nan() || b.is_nan() {
        return Err(i.make_error("RangeError", "formatRange arguments must not be NaN"));
    }
    Ok((o, a, b))
}

fn format_range(i: &mut Interp, this: &Value, x: &Value, y: &Value) -> Result<Value, Value> {
    let (o, a, b) = range_endpoints(i, this, x, y)?;
    let sa = assemble_number(i, &o, a);
    if a == b {
        return Ok(Value::from_string(sa));
    }
    let sb = assemble_number(i, &o, b);
    // The `en` range pattern joins with an en-dash (U+2013), no surrounding spaces.
    Ok(Value::from_string(format!("{sa}\u{2013}{sb}")))
}

fn format_range_to_parts(i: &mut Interp, this: &Value, x: &Value, y: &Value) -> Result<Value, Value> {
    let (o, a, b) = range_endpoints(i, this, x, y)?;
    let mut out: Vec<Value> = Vec::new();
    let mut push_parts = |i: &mut Interp, whole: &str, source: &str, out: &mut Vec<Value>| {
        for (t, v) in decompose_parts(whole) {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str(t));
            set_data(&ob, "value", Value::from_string(v));
            set_data(&ob, "source", Value::str(source));
            out.push(Value::Obj(ob));
        }
    };
    let sa = assemble_number(i, &o, a);
    if a == b {
        push_parts(i, &sa, "shared", &mut out);
        return Ok(i.make_array(out));
    }
    let sb = assemble_number(i, &o, b);
    push_parts(i, &sa, "startRange", &mut out);
    let lit = i.new_object();
    set_data(&lit, "type", Value::str("literal"));
    set_data(&lit, "value", Value::str("\u{2013}"));
    set_data(&lit, "source", Value::str("shared"));
    out.push(Value::Obj(lit));
    push_parts(i, &sb, "endRange", &mut out);
    Ok(i.make_array(out))
}

fn install_format_getter(it: &mut Interp, proto: &Gc) {
    let g = it.make_native("get format", 0, |i, this, _| {
        let o = brand_slot(i, &this, "__nf")?;
        // Cache a bound function on the instance so repeated reads return the same object.
        if let Some(f) = o.borrow().props.get("__nf_boundformat").map(|p| p.value.clone()) {
            return Ok(f);
        }
        let f = i.make_native("", 1, |i, that, a| format_number(i, &that, &arg(a, 0)));
        // Bind `this` = the NumberFormat instance.
        let bound = crate::intl::numberformat::bind_this(i, Value::Obj(f), this.clone());
        set_builtin(&o, "__nf_boundformat", bound.clone());
        Ok(bound)
    });
    proto.borrow_mut().props.insert(
        "format",
        crate::value::Property {
            value: Value::Undefined,
            get: Some(Value::Obj(g)),
            set: None,
            accessor: true,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
}

/// Bind `this_arg` onto `target` via Function.prototype.bind, then normalise the result to match the
/// spec's `format`/`compare` bound functions: name `""`, length 1, and not a constructor.
pub(crate) fn bind_this(i: &mut Interp, target: Value, this_arg: Value) -> Value {
    if let Ok(bindfn) = i.get_member(&target, "bind") {
        if let Ok(bound) = i.call(bindfn, target.clone(), &[this_arg]) {
            if let Value::Obj(o) = &bound {
                o.borrow_mut().is_constructor = false;
                o.borrow_mut().props.insert(
                    "name",
                    crate::value::Property::data(Value::str(""), false, false, true),
                );
            }
            return bound;
        }
    }
    target
}

struct DigitOpts {
    min_int: u32,
    min_frac: u32,
    max_frac: u32,
    min_sig: Option<u32>,
    max_sig: Option<u32>,
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    // Legacy service: callable without `new` (returns a fresh instance either way).
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    // numberingSystem is read right after localeMatcher, and must be a valid type identifier.
    let numbering = get_option(i, &options, "numberingSystem", &[], None)?;
    if let Some(ns) = &numbering {
        if !ns.split('-').all(|p| p.len() >= 3 && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric())) {
            return Err(i.make_error("RangeError", format!("invalid numberingSystem: {ns}")));
        }
    }
    let resolved = resolve_locale(i, &requested, &["nu"]);
    let numbering = numbering.unwrap_or_else(|| "latn".to_string());

    let style = get_option(
        i,
        &options,
        "style",
        &["decimal", "percent", "currency", "unit"],
        Some("decimal"),
    )?
    .unwrap();

    // currency
    let currency = get_option(i, &options, "currency", &[], None)?;
    if style == "currency" && currency.is_none() {
        return Err(i.make_error("TypeError", "currency is required for currency style"));
    }
    if let Some(c) = &currency {
        if !is_well_formed_currency(c) {
            return Err(i.make_error("RangeError", format!("invalid currency: {c}")));
        }
    }
    let currency_display = get_option(
        i,
        &options,
        "currencyDisplay",
        &["code", "symbol", "narrowSymbol", "name"],
        Some("symbol"),
    )?
    .unwrap();
    let currency_sign = get_option(i, &options, "currencySign", &["standard", "accounting"], Some("standard"))?
        .unwrap();

    // unit
    let unit = get_option(i, &options, "unit", &[], None)?;
    if style == "unit" && unit.is_none() {
        return Err(i.make_error("TypeError", "unit is required for unit style"));
    }
    if let Some(u) = &unit {
        if !is_well_formed_unit(u) {
            return Err(i.make_error("RangeError", format!("invalid unit: {u}")));
        }
    }
    let unit_display = get_option(i, &options, "unitDisplay", &["short", "narrow", "long"], Some("short"))?
        .unwrap();

    // notation is read before the digit options.
    let notation = get_option(
        i,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?
    .unwrap();

    // digit options (minInt, min/maxFrac, min/maxSig), then the rounding options.
    let cur_digits = currency
        .as_deref()
        .map(|c| currency_fraction_digits(&c.to_uppercase()))
        .unwrap_or(2);
    let digits = read_digit_options(i, &options, &style, cur_digits)?;

    let rounding_increment = {
        let v = ab(i.get_member(&options, "roundingIncrement"))?;
        if matches!(v, Value::Undefined) {
            1u32
        } else {
            let n = ab(i.to_number(&v))?;
            let allowed = [1u32, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000, 2500, 5000];
            if n.fract() != 0.0 || !allowed.contains(&(n as u32)) {
                return Err(i.make_error("RangeError", "invalid roundingIncrement"));
            }
            n as u32
        }
    };
    let rounding_mode = get_option(
        i,
        &options,
        "roundingMode",
        &[
            "ceil", "floor", "expand", "trunc", "halfCeil", "halfFloor", "halfExpand",
            "halfTrunc", "halfEven",
        ],
        Some("halfExpand"),
    )?
    .unwrap();
    let rounding_priority = get_option(
        i,
        &options,
        "roundingPriority",
        &["auto", "morePrecision", "lessPrecision"],
        Some("auto"),
    )?
    .unwrap();
    let trailing_zero = get_option(
        i,
        &options,
        "trailingZeroDisplay",
        &["auto", "stripIfInteger"],
        Some("auto"),
    )?
    .unwrap();

    let compact_display = get_option(i, &options, "compactDisplay", &["short", "long"], Some("short"))?
        .unwrap();
    let use_grouping = read_use_grouping(i, &options, &notation)?;
    let sign_display = get_option(
        i,
        &options,
        "signDisplay",
        &["auto", "never", "always", "exceptZero", "negative"],
        Some("auto"),
    )?
    .unwrap();

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.NumberFormat") {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__nf", Value::Bool(true));
    set_builtin(&obj, "__nf_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__nf_nu", Value::from_string(numbering));
    set_builtin(&obj, "__nf_roundingincrement", Value::Num(rounding_increment as f64));
    set_builtin(&obj, "__nf_roundingpriority", Value::from_string(rounding_priority));
    set_builtin(&obj, "__nf_trailingzero", Value::from_string(trailing_zero));
    set_builtin(&obj, "__nf_style", Value::from_string(style));
    if let Some(c) = currency {
        set_builtin(&obj, "__nf_currency", Value::from_string(c.to_uppercase()));
        set_builtin(&obj, "__nf_currencydisplay", Value::from_string(currency_display));
        set_builtin(&obj, "__nf_currencysign", Value::from_string(currency_sign));
    }
    if let Some(u) = unit {
        set_builtin(&obj, "__nf_unit", Value::from_string(u));
        set_builtin(&obj, "__nf_unitdisplay", Value::from_string(unit_display));
    }
    set_builtin(&obj, "__nf_minint", Value::Num(digits.min_int as f64));
    set_builtin(&obj, "__nf_minfrac", Value::Num(digits.min_frac as f64));
    set_builtin(&obj, "__nf_maxfrac", Value::Num(digits.max_frac as f64));
    if let Some(v) = digits.min_sig {
        set_builtin(&obj, "__nf_minsig", Value::Num(v as f64));
    }
    if let Some(v) = digits.max_sig {
        set_builtin(&obj, "__nf_maxsig", Value::Num(v as f64));
    }
    set_builtin(&obj, "__nf_notation", Value::from_string(notation));
    set_builtin(&obj, "__nf_compactdisplay", Value::from_string(compact_display));
    set_builtin(&obj, "__nf_grouping", use_grouping);
    set_builtin(&obj, "__nf_signdisplay", Value::from_string(sign_display));
    set_builtin(&obj, "__nf_roundingmode", Value::from_string(rounding_mode));
    Ok(Value::Obj(obj))
}

/// The CLDR default fraction-digit count for a currency (ISO 4217 minor units).
fn currency_fraction_digits(code: &str) -> u32 {
    match code {
        "JPY" | "KRW" | "CLP" | "ISK" | "HUF" | "VND" | "TWD" | "UGX" | "XOF" | "XAF" | "XPF"
        | "PYG" | "RWF" | "DJF" | "GNF" | "KMF" | "VUV" => 0,
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
        _ => 2,
    }
}

fn read_digit_options(i: &mut Interp, options: &Value, style: &str, cur_digits: u32) -> Result<DigitOpts, Value> {
    // Read order: minInt, minFrac, maxFrac, minSig, maxSig.
    let min_int = read_range(i, options, "minimumIntegerDigits", 1, 21, 1)?;
    let mnfd = read_range_opt(i, options, "minimumFractionDigits", 0, 100)?;
    let mxfd = read_range_opt(i, options, "maximumFractionDigits", 0, 100)?;
    let min_sig = read_range_opt(i, options, "minimumSignificantDigits", 1, 21)?;
    let max_sig = read_range_opt(i, options, "maximumSignificantDigits", 1, 21)?;

    let (default_min_frac, default_max_frac) = if style == "currency" {
        (cur_digits, cur_digits)
    } else if style == "percent" {
        (0, 0)
    } else {
        (0, 3)
    };

    let (min_frac, max_frac) = if min_sig.is_some() || max_sig.is_some() {
        (0, 0)
    } else {
        let mnfd = mnfd.unwrap_or(default_min_frac);
        let mxfd = mxfd.unwrap_or(default_max_frac.max(mnfd));
        if mnfd > mxfd {
            return Err(i.make_error("RangeError", "minimumFractionDigits > maximumFractionDigits"));
        }
        (mnfd, mxfd)
    };
    if let (Some(a), Some(b)) = (min_sig, max_sig) {
        if a > b {
            return Err(i.make_error(
                "RangeError",
                "minimumSignificantDigits > maximumSignificantDigits",
            ));
        }
    }
    Ok(DigitOpts {
        min_int,
        min_frac,
        max_frac,
        min_sig,
        max_sig,
    })
}

fn read_range(i: &mut Interp, options: &Value, prop: &str, lo: u32, hi: u32, fallback: u32) -> Result<u32, Value> {
    Ok(read_range_opt(i, options, prop, lo, hi)?.unwrap_or(fallback))
}
fn read_range_opt(i: &mut Interp, options: &Value, prop: &str, lo: u32, hi: u32) -> Result<Option<u32>, Value> {
    let v = ab(i.get_member(options, prop))?;
    if matches!(v, Value::Undefined) {
        return Ok(None);
    }
    let n = ab(i.to_number(&v))?;
    if n.is_nan() {
        return Err(i.make_error("RangeError", format!("{prop} is NaN")));
    }
    let f = n.floor();
    if f < lo as f64 || f > hi as f64 {
        return Err(i.make_error("RangeError", format!("{prop} out of range")));
    }
    Ok(Some(f as u32))
}

fn read_use_grouping(i: &mut Interp, options: &Value, notation: &str) -> Result<Value, Value> {
    let v = ab(i.get_member(options, "useGrouping"))?;
    // Default: "min2" for compact notation? No — default "auto".
    let default = Value::str(if notation == "compact" { "min2" } else { "auto" });
    match v {
        Value::Undefined => Ok(default),
        Value::Bool(true) => Ok(Value::str("always")),
        Value::Bool(false) => Ok(Value::Bool(false)),
        _ => {
            let s = ab(i.to_string(&v))?.to_string();
            if s == "true" {
                return Ok(Value::str("always"));
            }
            if s == "false" {
                return Ok(Value::Bool(false));
            }
            if !["always", "auto", "min2"].contains(&s.as_str()) {
                return Err(i.make_error("RangeError", format!("invalid useGrouping: {s}")));
            }
            Ok(Value::from_string(s))
        }
    }
}

fn is_well_formed_currency(c: &str) -> bool {
    c.len() == 3 && c.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_well_formed_unit(u: &str) -> bool {
    // "unit" or "unit-per-unit"; each a 3-8 (roughly) identifier of alpha/-.
    let simple = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-');
    match u.split_once("-per-") {
        Some((a, b)) => simple(a) && simple(b),
        None => simple(u),
    }
}

// ---- formatting ------------------------------------------------------------------------------

fn instance(i: &mut Interp, this: &Value) -> Result<Gc, Value> {
    brand_slot(i, this, "__nf")
}

fn get_str(o: &Gc, k: &str) -> String {
    match o.borrow().props.get(k).map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    }
}
fn get_num(o: &Gc, k: &str) -> Option<u32> {
    match o.borrow().props.get(k).map(|p| p.value.clone()) {
        Some(Value::Num(n)) => Some(n as u32),
        _ => None,
    }
}

/// Produce (sign_is_negative, digit-string) for |x| per digit options.
fn format_magnitude(x: f64, o: &Gc) -> String {
    let min_int = get_num(o, "__nf_minint").unwrap_or(1);
    let min_frac = get_num(o, "__nf_minfrac").unwrap_or(0);
    let max_frac = get_num(o, "__nf_maxfrac").unwrap_or(0);
    let min_sig = get_num(o, "__nf_minsig");
    let max_sig = get_num(o, "__nf_maxsig");

    let increment = get_num(o, "__nf_roundingincrement").unwrap_or(1);
    let mode = get_str(o, "__nf_roundingmode");
    let mode = if mode.is_empty() { "halfExpand".to_string() } else { mode };
    let mut s = if let Some(msig) = max_sig {
        round_significant_dec(x, msig, min_sig.unwrap_or(1), &mode)
    } else {
        round_fraction_dec(x, min_frac, max_frac, increment, &mode)
    };
    // Pad integer digits to min_int.
    {
        let (int_part, frac_part) = match s.split_once('.') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (s.clone(), None),
        };
        let int_digits = int_part.trim_start_matches('0');
        let int_len = int_digits.len().max(1);
        let padded_int = if (int_len as u32) < min_int {
            format!("{:0>width$}", int_digits.max(""), width = min_int as usize)
        } else if int_digits.is_empty() {
            "0".to_string()
        } else {
            int_digits.to_string()
        };
        s = match frac_part {
            Some(f) => format!("{padded_int}.{f}"),
            None => padded_int,
        };
    }
    s
}

/// The shortest round-trip decimal digits of `|x|` as (integer_digits, fraction_digits). Rounding is
/// performed on this decimal expansion (not the binary f64) so `1.015` rounds like the source `1.015`.
fn decimal_digits(x: f64) -> (String, String) {
    let s = format!("{}", x.abs());
    match s.split_once('.') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (s, String::new()),
    }
}

/// Round the decimal `int_str.frac_str` to `keep` fraction digits (may be negative, rounding integer
/// places), snapping to a multiple of `inc` at that scale, per the ECMA-402 `mode`. Returns the exact
/// decimal string (unsigned, `keep` fraction digits when `keep > 0`, no min-frac padding).
fn round_decimal(int_str: &str, frac_str: &str, keep: i32, inc: u32, mode: &str, negative: bool) -> String {
    let mut digits: Vec<u8> = int_str.bytes().chain(frac_str.bytes()).map(|b| b - b'0').collect();
    let point = int_str.len() as i32;
    let cut = point + keep; // number of leading digits retained as the coefficient
    if cut <= 0 {
        // Everything is discarded; the only possible non-zero result is a single rounded-up unit.
        let any = digits.iter().any(|&d| d != 0);
        let up = round_up_decision(mode, negative, /*rem*/ 0.0, /*frac*/ if any { 0.5001 } else { 0.0 }, inc);
        let val = if up { inc as u128 } else { 0 };
        return place_decimal(&val.to_string(), keep);
    }
    let cut = cut as usize;
    if digits.len() < cut {
        digits.resize(cut, 0);
    }
    let retained: String = digits[..cut].iter().map(|d| (d + b'0') as char).collect();
    let discarded = &digits[cut..];
    // The discarded tail as a fraction of one retained unit (short string → exact enough to compare).
    let frac = if discarded.is_empty() {
        0.0
    } else {
        let s: String = discarded.iter().map(|d| (d + b'0') as char).collect();
        format!("0.{s}").parse::<f64>().unwrap_or(0.0)
    };
    let Ok(c) = retained.parse::<u128>() else {
        // Coefficient too large for exact arithmetic; emit unrounded (huge integers seldom round).
        return place_decimal(&retained, keep);
    };
    let rem = (c % inc.max(1) as u128) as f64;
    let up = round_up_decision(mode, negative, rem, frac, inc);
    let base = c - (c % inc.max(1) as u128);
    let result = if up { base + inc as u128 } else { base };
    place_decimal(&result.to_string(), keep)
}

/// Reconstruct a decimal string from an integer coefficient at scale `10^-keep`.
fn place_decimal(coeff: &str, keep: i32) -> String {
    if keep <= 0 {
        return format!("{coeff}{}", "0".repeat((-keep) as usize));
    }
    let keep = keep as usize;
    let padded = if coeff.len() <= keep {
        format!("{:0>width$}", coeff, width = keep + 1)
    } else {
        coeff.to_string()
    };
    let split = padded.len() - keep;
    format!("{}.{}", &padded[..split], &padded[split..])
}

/// Whether to round the coefficient up by one increment, given the discarded position `rem + frac`
/// (in increment units, `rem` an integer remainder and `frac` in [0,1)) under ECMA-402 `mode`.
fn round_up_decision(mode: &str, negative: bool, rem: f64, frac: f64, inc: u32) -> bool {
    let position = rem + frac;
    let half = inc as f64 / 2.0;
    let some = position > 0.0;
    match mode {
        "trunc" => false,
        "expand" => some,
        "ceil" => some && !negative,
        "floor" => some && negative,
        "halfTrunc" => position > half,
        "halfExpand" => position >= half,
        "halfCeil" => position > half || (position == half && !negative),
        "halfFloor" => position > half || (position == half && negative),
        "halfEven" => {
            // Tie → round toward the multiple whose quotient is even.
            position > half || (position == half && (((rem / inc as f64).round() as i64) % 2 != 0))
        }
        _ => position >= half, // halfExpand default
    }
}

/// Round `x` to at most `max_frac` fraction digits, snapping to `increment`, per `mode`; pad to
/// `min_frac`.
fn round_fraction_dec(x: f64, min_frac: u32, max_frac: u32, increment: u32, mode: &str) -> String {
    let (int_str, frac_str) = decimal_digits(x);
    let negative = x.is_sign_negative();
    let mut s = round_decimal(&int_str, &frac_str, max_frac as i32, increment.max(1), mode, negative);
    // Trim trailing zeros beyond min_frac (only meaningful without an increment > 1).
    if increment <= 1 && max_frac > min_frac && s.contains('.') {
        while s.ends_with('0') {
            let frac_len = s.split('.').nth(1).map(|f| f.len()).unwrap_or(0);
            if frac_len as u32 <= min_frac {
                break;
            }
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Round `x` to `max_sig` significant digits (per `mode`), keeping at least `min_sig`.
fn round_significant_dec(x: f64, max_sig: u32, min_sig: u32, mode: &str) -> String {
    if x == 0.0 {
        if min_sig > 1 {
            return format!("0.{}", "0".repeat((min_sig - 1) as usize));
        }
        return "0".to_string();
    }
    let (int_str, frac_str) = decimal_digits(x);
    let negative = x.is_sign_negative();
    // Position of the most-significant digit (0 = ones place, positive = 10^k, negative = 10^-k).
    let int_trim = int_str.trim_start_matches('0');
    let msd: i32 = if !int_trim.is_empty() {
        int_trim.len() as i32 - 1
    } else {
        // Leading zeros in the fraction push the first significant digit right.
        -(1 + frac_str.bytes().take_while(|&b| b == b'0').count() as i32)
    };
    let keep = max_sig as i32 - 1 - msd; // fraction digits to retain
    let mut s = round_decimal(&int_str, &frac_str, keep, 1, mode, negative);
    // Trim trailing fractional zeros down to min_sig significant digits.
    if s.contains('.') {
        let significant = |t: &str| t.chars().filter(|c| c.is_ascii_digit()).skip_while(|c| *c == '0').count();
        while s.ends_with('0') && significant(&s) as u32 > min_sig {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}


fn group_integer(int_part: &str, grouping: &Value, sep: &str, sizes: (usize, usize)) -> String {
    let enabled = !matches!(grouping, Value::Bool(false));
    let min2 = matches!(grouping, Value::Str(s) if &**s == "min2");
    let (primary, secondary) = sizes;
    if !enabled || int_part.len() <= primary || (min2 && int_part.len() <= primary + 1) {
        return int_part.to_string();
    }
    // Insert separators right-to-left: the first (rightmost) group is `primary` digits, the rest are
    // `secondary` digits (Indian-style 3;2 when they differ).
    let digits: Vec<char> = int_part.chars().collect();
    let n = digits.len();
    let mut out = String::new();
    for (idx, c) in digits.iter().enumerate() {
        let from_right = n - idx;
        let boundary = idx > 0
            && from_right >= primary
            && (from_right - primary) % secondary == 0;
        if boundary {
            out.push_str(sep);
        }
        out.push(*c);
    }
    out
}

fn assemble_number(i: &mut Interp, o: &Gc, x: f64) -> String {
    let style = get_str(o, "__nf_style");
    let mut value = x;
    if style == "percent" {
        value *= 100.0;
    }
    // Negative zero counts as negative for sign display (so -0 formats as "-0").
    let negative = value.is_sign_negative() && !value.is_nan();
    // scientific / engineering notation: mantissa in [1,10) or [1,1000), plus an exponent.
    let notation = get_str(o, "__nf_notation");
    let mut exponent: Option<i32> = None;
    if (notation == "scientific" || notation == "engineering") && value != 0.0 && value.is_finite() {
        let mut e = value.abs().log10().floor() as i32;
        if notation == "engineering" {
            e -= e.rem_euclid(3);
        }
        value /= 10f64.powi(e);
        // Guard against log10 rounding pushing the mantissa to 10.
        if value.abs() >= if notation == "engineering" { 1000.0 } else { 10.0 } {
            value /= 10.0;
            e += 1;
        }
        exponent = Some(e);
    }
    // compact notation: divide into a K/M/B/T tier and append the compact suffix (English data).
    let mut compact_suffix = String::new();
    let mut compact = false;
    if notation == "compact" && value.is_finite() && value.abs() >= 1000.0 {
        let long = get_str(o, "__nf_compactdisplay") == "long";
        let (div, short, longw) = if value.abs() >= 1e12 {
            (1e12, "T", " trillion")
        } else if value.abs() >= 1e9 {
            (1e9, "B", " billion")
        } else if value.abs() >= 1e6 {
            (1e6, "M", " million")
        } else {
            (1e3, "K", " thousand")
        };
        value /= div;
        compact_suffix = if long { longw.to_string() } else { short.to_string() };
        compact = true;
    }
    let (int_part, frac_part) = if value.is_nan() {
        ("NaN".to_string(), None)
    } else if value.is_infinite() {
        ("\u{221e}".to_string(), None)
    } else {
        // Compact notation rounds with the default "morePrecision" of 2 significant / 0 fraction
        // digits: keep max(0, 2 - integerDigits) fraction digits (unless digit options were given).
        let has_sig = o.borrow().props.contains("__nf_minsig");
        let mag = if notation == "compact" && !has_sig {
            // roundingPriority "morePrecision" over (max 0 fraction) and (max 2 significant): pick
            // whichever shows more fraction digits (ties keep the integer/fraction result).
            let s_frac = round_fraction_dec(value.abs(), 0, 0, 1, "halfExpand");
            let s_sig = round_significant_dec(value.abs(), 2, 1, "halfExpand");
            let fd = |s: &str| s.split('.').nth(1).map(|f| f.len()).unwrap_or(0);
            if fd(&s_sig) > fd(&s_frac) {
                s_sig
            } else {
                s_frac
            }
        } else {
            format_magnitude(value.abs(), o)
        };
        match mag.split_once('.') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (mag.clone(), None),
        }
    };
    // A value that rounds to zero (e.g. -0.0001 with the default 3 fraction digits) is "zero" for
    // the purpose of the `exceptZero`/`negative` sign rules, even though its sign bit is negative.
    let rounded_zero = value.is_finite()
        && int_part.chars().all(|c| c == '0')
        && frac_part.as_deref().unwrap_or("").chars().all(|c| c == '0');
    let grouping = o.borrow().props.get("__nf_grouping").map(|p| p.value.clone()).unwrap_or(Value::str("auto"));
    // Locale number symbols (decimal separator, grouping separator + sizes).
    let locale = get_str(o, "__nf_locale");
    let mut lparts = locale.split('-');
    let lang = lparts.next().unwrap_or("en");
    let region = lparts.find(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_uppercase())).unwrap_or("");
    let (dec_sep, grp_sep, sizes) = crate::intl::data::number_symbols(lang, region);
    // Grouping is suppressed in scientific/engineering notation.
    let grouped = if exponent.is_some() || !value.is_finite() {
        int_part.clone()
    } else {
        group_integer(&int_part, &grouping, grp_sep, sizes)
    };
    let mut num = match frac_part {
        Some(f) => format!("{grouped}{dec_sep}{f}"),
        None => grouped,
    };
    if let Some(e) = exponent {
        // CLDR: "E" then the exponent with its sign ("E-6", "E6").
        num = format!("{num}E{e}");
    }
    if compact {
        num.push_str(&compact_suffix);
    }

    // Sign display. `auto`/`always` key off the sign bit (so -0 and values rounding to zero still
    // show "-0"); `exceptZero`/`negative` suppress the sign when the displayed value is zero or NaN.
    let sign_display = get_str(o, "__nf_signdisplay");
    let zeroish = rounded_zero || value.is_nan();
    let sign = match sign_display.as_str() {
        "never" => "",
        "always" => {
            if negative {
                "-"
            } else {
                "+"
            }
        }
        "exceptZero" => {
            if zeroish {
                ""
            } else if negative {
                "-"
            } else {
                "+"
            }
        }
        "negative" => {
            if negative && !zeroish {
                "-"
            } else {
                ""
            }
        }
        _ => {
            if negative {
                "-"
            } else {
                ""
            }
        }
    };

    // Style wrapping.
    match style.as_str() {
        "percent" => {
            num = format!("{sign}{num}%");
        }
        "currency" => {
            let code = get_str(o, "__nf_currency");
            let disp = get_str(o, "__nf_currencydisplay");
            let sym = currency_symbol(&code, &disp);
            // Accounting notation wraps a negative amount in parentheses instead of a minus sign.
            let accounting = get_str(o, "__nf_currencysign") == "accounting";
            if accounting && negative && !zeroish {
                num = format!("({sym}{num})");
            } else {
                num = format!("{sign}{sym}{num}");
            }
        }
        "unit" => {
            let unit = get_str(o, "__nf_unit");
            let disp = get_str(o, "__nf_unitdisplay");
            num = format!("{sign}{}", unit_wrap(&num, &unit, &disp));
        }
        _ => {
            num = format!("{sign}{num}");
        }
    }
    let _ = i;
    num
}

fn currency_symbol(code: &str, display: &str) -> String {
    if display == "code" {
        return format!("{code}\u{00a0}");
    }
    let sym = match code {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CNY" => "CN¥",
        "AUD" => "A$",
        "CAD" => "CA$",
        _ => return format!("{code}\u{00a0}"),
    };
    sym.to_string()
}

fn unit_short_name(u: &str) -> Option<&'static str> {
    Some(match u {
        "kilometer-per-hour" => "km/h",
        "meter" => "m",
        "kilometer" => "km",
        "centimeter" => "cm",
        "percent" => "%",
        "liter" => "L",
        "kilobyte" => "kB",
        "megabyte" => "MB",
        "celsius" => "°C",
        "fahrenheit" => "°F",
        _ => return None,
    })
}

fn unit_wrap(num: &str, unit: &str, display: &str) -> String {
    if display == "long" {
        return format!("{num} {unit}");
    }
    match unit_short_name(unit) {
        Some(n) => format!("{num} {n}"),
        None => format!("{num} {unit}"),
    }
}

fn format_number(i: &mut Interp, this: &Value, x: &Value) -> Result<Value, Value> {
    let o = instance(i, this)?;
    let n = to_intl_number(i, x)?;
    Ok(Value::from_string(assemble_number(i, &o, n)))
}

fn to_intl_number(i: &mut Interp, x: &Value) -> Result<f64, Value> {
    // ToIntlMathematicalValue — we approximate with ToNumber (BigInt handled as its value).
    match x {
        Value::BigInt(b) => Ok(*b as f64),
        _ => ab(i.to_number(x)),
    }
}

fn format_to_parts(i: &mut Interp, this: &Value, x: &Value) -> Result<Value, Value> {
    let o = instance(i, this)?;
    let n = to_intl_number(i, x)?;
    let whole = assemble_number(i, &o, n);
    let parts = decompose_parts(&whole);
    let arr: Vec<Value> = parts
        .into_iter()
        .map(|(t, v)| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str(t));
            set_data(&ob, "value", Value::from_string(v));
            Value::Obj(ob)
        })
        .collect();
    Ok(i.make_array(arr))
}

/// Break an assembled number string into typed parts (minusSign/plusSign, currency/percentSign/
/// literal affixes, grouped integer, decimal, fraction, and the exponent group).
fn decompose_parts(s: &str) -> Vec<(&'static str, String)> {
    let mut parts: Vec<(&'static str, String)> = Vec::new();
    // Split off the exponent tail, if any.
    let (main, exp) = match s.split_once('E') {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    let bytes: Vec<char> = main.chars().collect();
    let mut idx = 0;
    // Leading affix: a sign, then any non-digit prefix (currency symbol).
    if idx < bytes.len() && (bytes[idx] == '-' || bytes[idx] == '+') {
        parts.push((if bytes[idx] == '-' { "minusSign" } else { "plusSign" }, bytes[idx].to_string()));
        idx += 1;
    }
    // The number body is either digits, the infinity glyph, or "NaN"; the prefix loop stops there.
    let is_nan_at = |b: &[char], k: usize| b[k..].starts_with(&['N', 'a', 'N']);
    let mut prefix = String::new();
    while idx < bytes.len()
        && !bytes[idx].is_ascii_digit()
        && bytes[idx] != '.'
        && bytes[idx] != '\u{221e}'
        && !is_nan_at(&bytes, idx)
    {
        prefix.push(bytes[idx]);
        idx += 1;
    }
    if !prefix.is_empty() {
        // A leading currency symbol vs. a stray literal — classify a $ / letters as currency.
        let t = if prefix.chars().all(|c| c == ' ' || c == '\u{00a0}') { "literal" } else { "currency" };
        parts.push((t, prefix));
    }
    // Non-finite body: emit a single infinity/nan part, then fall through to any trailing affix.
    if idx < bytes.len() && bytes[idx] == '\u{221e}' {
        parts.push(("infinity", "\u{221e}".to_string()));
        idx += 1;
    } else if is_nan_at(&bytes, idx) {
        parts.push(("nan", "NaN".to_string()));
        idx += 3;
    }
    // Integer digits (with grouping commas).
    let int_start = idx;
    while idx < bytes.len() && (bytes[idx].is_ascii_digit() || bytes[idx] == ',') {
        idx += 1;
    }
    let int_str: String = bytes[int_start..idx].iter().collect();
    for seg in split_grouped(&int_str) {
        parts.push(seg);
    }
    // Decimal + fraction.
    if idx < bytes.len() && bytes[idx] == '.' {
        parts.push(("decimal", ".".to_string()));
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        parts.push(("fraction", bytes[frac_start..idx].iter().collect()));
    }
    // Trailing affix (percent sign, unit, or literal).
    let suffix: String = bytes[idx..].iter().collect();
    if !suffix.is_empty() {
        let t = if suffix.contains('%') { "percentSign" } else { "literal" };
        parts.push((t, suffix));
    }
    // Exponent group.
    if let Some(e) = exp {
        parts.push(("exponentSeparator", "E".to_string()));
        let mut ec = e.chars().peekable();
        if ec.peek() == Some(&'-') {
            parts.push(("exponentMinusSign", "-".to_string()));
            ec.next();
        }
        let digits: String = ec.collect();
        parts.push(("exponentInteger", digits));
    }
    parts
}

/// Split a grouped integer like "12,345" into integer/group parts.
fn split_grouped(int_str: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in int_str.chars() {
        if c == ',' {
            if !cur.is_empty() {
                out.push(("integer", std::mem::take(&mut cur)));
            }
            out.push(("group", ",".to_string()));
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(("integer", cur));
    }
    out
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = instance(i, &this)?;
    let res = i.new_object();
    let put = |i: &mut Interp, res: &Gc, k: &str, slot: &str| {
        if let Some(v) = o.borrow().props.get(slot).map(|p| p.value.clone()) {
            set_data(res, k, v);
        }
        let _ = i;
    };
    put(i, &res, "locale", "__nf_locale");
    put(i, &res, "numberingSystem", "__nf_nu");
    put(i, &res, "style", "__nf_style");
    put(i, &res, "currency", "__nf_currency");
    put(i, &res, "currencyDisplay", "__nf_currencydisplay");
    put(i, &res, "currencySign", "__nf_currencysign");
    put(i, &res, "unit", "__nf_unit");
    put(i, &res, "unitDisplay", "__nf_unitdisplay");
    put(i, &res, "minimumIntegerDigits", "__nf_minint");
    if o.borrow().props.contains("__nf_minsig") {
        put(i, &res, "minimumSignificantDigits", "__nf_minsig");
        put(i, &res, "maximumSignificantDigits", "__nf_maxsig");
    } else {
        put(i, &res, "minimumFractionDigits", "__nf_minfrac");
        put(i, &res, "maximumFractionDigits", "__nf_maxfrac");
    }
    put(i, &res, "roundingIncrement", "__nf_roundingincrement");
    put(i, &res, "roundingMode", "__nf_roundingmode");
    put(i, &res, "roundingPriority", "__nf_roundingpriority");
    put(i, &res, "trailingZeroDisplay", "__nf_trailingzero");
    set_data(&res, "useGrouping", o.borrow().props.get("__nf_grouping").map(|p| p.value.clone()).unwrap_or(Value::str("auto")));
    put(i, &res, "notation", "__nf_notation");
    // compactDisplay only appears when notation is compact.
    if matches!(o.borrow().props.get("__nf_notation").map(|p| p.value.clone()), Some(Value::Str(s)) if &*s == "compact") {
        put(i, &res, "compactDisplay", "__nf_compactdisplay");
    }
    put(i, &res, "signDisplay", "__nf_signdisplay");
    Ok(Value::Obj(res))
}
