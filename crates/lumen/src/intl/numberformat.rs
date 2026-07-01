//! `Intl.NumberFormat` (standard notation; decimal/percent/currency/unit; English grouping).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
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
    let stype = suffix_type_of(&o);
    let (dec, grp) = loc_seps(&o);
    let mut out: Vec<Value> = Vec::new();
    let push_parts = |i: &mut Interp, whole: &str, source: &str, out: &mut Vec<Value>| {
        for (t, v) in decompose_parts(whole, &stype, dec, grp) {
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
    mxfd_set: bool,
    /// "significant" | "fraction" | "morePrecision" | "lessPrecision".
    rounding_type: &'static str,
}

/// The raw significant/fraction-digit options, read in spec order before the rounding options.
struct RawDigits {
    min_int: u32,
    mnfd: Option<u32>,
    mxfd: Option<u32>,
    mnsd: Option<u32>,
    mxsd: Option<u32>,
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
    let (resolved_locale, numbering) =
        super::service::resolve_locale_nu(&requested, numbering.as_deref());

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
    let raw_digits = read_raw_digits(i, &options)?;

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
    let digits = interpret_digits(
        i,
        &raw_digits,
        &style,
        &notation,
        &rounding_priority,
        cur_digits,
        rounding_increment,
    )?;
    // A roundingIncrement other than 1 requires fraction-digit rounding (TypeError otherwise), and
    // then maximumFractionDigits must equal minimumFractionDigits (RangeError otherwise).
    if rounding_increment != 1 {
        if digits.rounding_type != "fraction" {
            return Err(i.make_error(
                "TypeError",
                "roundingIncrement is only supported with fractionDigits rounding",
            ));
        }
        if digits.min_frac != digits.max_frac {
            return Err(i.make_error("RangeError", "maximumFractionDigits must equal minimumFractionDigits with roundingIncrement"));
        }
    }
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
    if let Some(proto) = instance_proto(i, "Intl.NumberFormat")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__nf", Value::Bool(true));
    set_builtin(&obj, "__nf_locale", Value::from_string(resolved_locale));
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
    set_builtin(&obj, "__nf_roundingtype", Value::str(digits.rounding_type));
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

/// SetNumberFormatDigitOptions, phase 1: read the raw digit options (spec order: minInt, minFrac,
/// maxFrac, minSig, maxSig). The rounding options are read by the caller afterwards; interpretation
/// happens in `interpret_digits` once the rounding priority is known.
fn read_raw_digits(i: &mut Interp, options: &Value) -> Result<RawDigits, Value> {
    let min_int = read_range(i, options, "minimumIntegerDigits", 1, 21, 1)?;
    let mnfd = read_range_opt(i, options, "minimumFractionDigits", 0, 100)?;
    let mxfd = read_range_opt(i, options, "maximumFractionDigits", 0, 100)?;
    let mnsd = read_range_opt(i, options, "minimumSignificantDigits", 1, 21)?;
    let mxsd = read_range_opt(i, options, "maximumSignificantDigits", 1, 21)?;
    Ok(RawDigits { min_int, mnfd, mxfd, mnsd, mxsd })
}

/// SetNumberFormatDigitOptions, phase 2: derive the effective digit bounds and rounding type from the
/// raw options, the style/notation defaults, and the rounding priority.
fn interpret_digits(
    i: &mut Interp,
    raw: &RawDigits,
    style: &str,
    notation: &str,
    priority: &str,
    cur_digits: u32,
    rounding_increment: u32,
) -> Result<DigitOpts, Value> {
    let (mnfd_default, mut mxfd_default) = if style == "currency" {
        (cur_digits, cur_digits)
    } else if style == "percent" {
        (0, 0)
    } else {
        (0, 3)
    };
    if rounding_increment != 1 {
        mxfd_default = mnfd_default;
    }
    let has_sd = raw.mnsd.is_some() || raw.mxsd.is_some();
    let has_fd = raw.mnfd.is_some() || raw.mxfd.is_some();
    let mut need_sd = true;
    let mut need_fd = true;
    if priority == "auto" {
        need_sd = has_sd;
        if need_sd || (!has_fd && notation == "compact") {
            need_fd = false;
        }
    }

    let (mut min_sig, mut max_sig) = (None, None);
    if need_sd {
        if has_sd {
            let mn = raw.mnsd.unwrap_or(1);
            let mx = raw.mxsd.unwrap_or(21);
            if mn > mx {
                return Err(i.make_error("RangeError", "minimumSignificantDigits > maximumSignificantDigits"));
            }
            min_sig = Some(mn);
            max_sig = Some(mx);
        } else {
            min_sig = Some(1);
            max_sig = Some(21);
        }
    }

    let (mut min_frac, mut max_frac) = (0u32, 0u32);
    if need_fd {
        let (mn, mx) = if has_fd {
            match (raw.mnfd, raw.mxfd) {
                (None, Some(mx)) => (mnfd_default.min(mx), mx),
                (Some(mn), None) => (mn, mxfd_default.max(mn)),
                (Some(mn), Some(mx)) => {
                    if mn > mx {
                        return Err(i.make_error("RangeError", "minimumFractionDigits > maximumFractionDigits"));
                    }
                    (mn, mx)
                }
                (None, None) => (mnfd_default, mxfd_default),
            }
        } else {
            (mnfd_default, mxfd_default)
        };
        min_frac = mn;
        max_frac = mx;
    }

    let rounding_type = if !need_sd && !need_fd {
        // Neither range requested (compact, auto): the default rounding is morePrecision over 0
        // fraction and (1..2) significant digits.
        min_frac = 0;
        max_frac = 0;
        min_sig = Some(1);
        max_sig = Some(2);
        "morePrecision"
    } else if priority == "morePrecision" {
        "morePrecision"
    } else if priority == "lessPrecision" {
        "lessPrecision"
    } else if has_sd {
        "significant"
    } else {
        "fraction"
    };

    Ok(DigitOpts {
        min_int: raw.min_int,
        min_frac,
        max_frac,
        min_sig,
        max_sig,
        mxfd_set: raw.mxfd.is_some(),
        rounding_type,
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
    // GetStringOrBooleanOption: undefined → fallback; `true` → "always"; a falsy value → false;
    // otherwise ToString and validate against ["min2","auto","always"] (so the string "true" is a
    // RangeError, not "always"). The fallback is "auto" (or "min2" for compact notation).
    let v = ab(i.get_member(options, "useGrouping"))?;
    let default = Value::str(if notation == "compact" { "min2" } else { "auto" });
    if matches!(v, Value::Undefined) {
        return Ok(default);
    }
    if matches!(v, Value::Bool(true)) {
        return Ok(Value::str("always"));
    }
    if !i.to_boolean(&v) {
        return Ok(Value::Bool(false));
    }
    let s = ab(i.to_string(&v))?.to_string();
    if !["always", "auto", "min2"].contains(&s.as_str()) {
        return Err(i.make_error("RangeError", format!("invalid useGrouping: {s}")));
    }
    Ok(Value::from_string(s))
}

fn is_well_formed_currency(c: &str) -> bool {
    c.len() == 3 && c.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_well_formed_unit(u: &str) -> bool {
    // A sanctioned single unit, or "X-per-Y" with both X and Y sanctioned (IsWellFormedUnitIdentifier).
    let sanctioned = |s: &str| crate::units::SANCTIONED_UNITS.contains(&s);
    match u.split_once("-per-") {
        Some((a, b)) => sanctioned(a) && sanctioned(b),
        None => sanctioned(u),
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
    let rtype = get_str(o, "__nf_roundingtype");
    let mut s = match rtype.as_str() {
        "significant" => round_significant_dec(x, max_sig.unwrap_or(21), min_sig.unwrap_or(1), &mode),
        "morePrecision" | "lessPrecision" => round_priority(
            x,
            min_sig.unwrap_or(1),
            max_sig.unwrap_or(21),
            min_frac,
            max_frac,
            increment,
            &mode,
            rtype == "morePrecision",
        ),
        "fraction" => round_fraction_dec(x, min_frac, max_frac, increment, &mode),
        // Fallback for formatters created without a stored rounding type.
        _ if max_sig.is_some() => round_significant_dec(x, max_sig.unwrap(), min_sig.unwrap_or(1), &mode),
        _ => round_fraction_dec(x, min_frac, max_frac, increment, &mode),
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
        let up = round_up_decision(mode, negative, /*rem*/ 0.0, /*frac*/ if any { 0.5001 } else { 0.0 }, inc, 0);
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
    let quotient = c / inc.max(1) as u128;
    let up = round_up_decision(mode, negative, rem, frac, inc, quotient);
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
fn round_up_decision(mode: &str, negative: bool, rem: f64, frac: f64, inc: u32, quotient: u128) -> bool {
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
            // Tie → round toward the multiple whose quotient is even (parity of the retained value).
            position > half || (position == half && quotient % 2 == 1)
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

/// The base-10 exponent of the most-significant digit of `x` (0 for the ones place). Zero maps to 0,
/// matching ToRawPrecision's handling.
fn msd_exponent(x: f64) -> i32 {
    if x == 0.0 {
        return 0;
    }
    let (int_str, frac_str) = decimal_digits(x);
    let int_trim = int_str.trim_start_matches('0');
    if !int_trim.is_empty() {
        int_trim.len() as i32 - 1
    } else {
        -(1 + frac_str.bytes().take_while(|&b| b == b'0').count() as i32)
    }
}

/// PartitionNumberPattern's morePrecision/lessPrecision disambiguation: round `x` both ways and pick
/// the significant-digit or fraction-digit result. The rounding magnitude uses the *maximum*
/// settings (ToRawPrecision: `e - maxSig + 1`; ToRawFixed: `-maxFrac`).
#[allow(clippy::too_many_arguments)]
fn round_priority(
    x: f64,
    min_sig: u32,
    max_sig: u32,
    min_frac: u32,
    max_frac: u32,
    increment: u32,
    mode: &str,
    more: bool,
) -> String {
    let s_result = round_significant_dec(x, max_sig, min_sig, mode);
    let f_result = round_fraction_dec(x, min_frac, max_frac, increment.max(1), mode);
    let s_mag = msd_exponent(x) - max_sig as i32 + 1;
    let f_mag = -(max_frac as i32);
    let fixed_is_more_precise = f_mag < s_mag;
    if (more && fixed_is_more_precise) || (!more && !fixed_is_more_precise) {
        f_result
    } else {
        s_result
    }
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
    if notation == "compact" && value.is_finite() {
        let long = get_str(o, "__nf_compactdisplay") == "long";
        let loc = get_str(o, "__nf_locale");
        let mut lp = loc.split('-');
        let clang = lp.next().unwrap_or("en").to_string();
        let cregion = lp.find(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_uppercase())).unwrap_or("").to_string();
        // Pick the largest compact tier whose divisor does not exceed the value; below the smallest
        // tier the number is formatted normally (locales differ: de starts at 1e6, CJK at 1e4, ko 1e3).
        if let Some(&(div, suffix)) = compact_tiers(&clang, &cregion, long).iter().find(|(t, _)| value.abs() >= *t) {
            value /= div;
            compact_suffix = suffix.to_string();
            compact = true;
        }
    }
    let (int_part, frac_part) = if value.is_nan() {
        let loc = get_str(o, "__nf_locale");
        let mut lp = loc.split('-');
        let lang = lp.next().unwrap_or("en");
        let region = lp.find(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_uppercase())).unwrap_or("");
        (crate::intl::data::nan_symbol(lang, region).to_string(), None)
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
            // Pass the signed value so the sign-sensitive rounding modes (ceil/floor and their
            // half-variants) see the true sign; the returned magnitude is unsigned.
            format_magnitude(value, o)
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
            let loc = get_str(o, "__nf_locale");
            let lang = loc.split('-').next().unwrap_or("en");
            let sym = currency_symbol_loc(lang, &code, &disp);
            // Symbol placement is locale-specific (de puts it after with a NBSP).
            let after = curr_symbol_after(lang);
            let body = if after { format!("{num}\u{a0}{sym}") } else { format!("{sym}{num}") };
            // Accounting wraps a negative in parentheses in symbol-before locales; symbol-after
            // locales (de) keep the minus sign.
            let accounting = get_str(o, "__nf_currencysign") == "accounting";
            if accounting && sign == "-" && !after {
                num = format!("({body})");
            } else {
                num = format!("{sign}{body}");
            }
        }
        "unit" => {
            let signed = format!("{sign}{num}");
            num = match unit_pattern_of(o, value) {
                Some(p) => p.replace("{0}", &signed),
                None => {
                    let unit = get_str(o, "__nf_unit");
                    let disp = get_str(o, "__nf_unitdisplay");
                    let cat = crate::intl::data::plural_cardinal(o_lang(o), value.abs().trunc() as u64, value.fract() != 0.0);
                    format!("{sign}{}", unit_wrap(&num, &unit, &disp, cat != "one"))
                }
            };
        }
        _ => {
            num = format!("{sign}{num}");
        }
    }
    let _ = i;
    num
}

/// Whether the currency symbol follows the amount (with a NBSP) in this locale.
fn curr_symbol_after(lang: &str) -> bool {
    matches!(lang, "de" | "fr" | "fi" | "sv" | "cs" | "sk" | "hu" | "pl" | "ru" | "pt")
}

/// The locale-aware currency symbol. For USD the default "symbol" form is "$" in en/de but "US$"
/// elsewhere; "narrowSymbol" is always the plain "$".
fn currency_symbol_loc(lang: &str, code: &str, display: &str) -> String {
    if code == "USD" && (display == "symbol" || display.is_empty()) {
        return if matches!(lang, "en" | "de" | "ja") { "$" } else { "US$" }.to_string();
    }
    if code == "USD" && display == "narrowSymbol" {
        return "$".to_string();
    }
    currency_symbol(code, display)
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

/// The compact-notation tiers (threshold divisor, suffix) for a locale, largest first. The suffix
/// includes any leading spacing. Below the smallest tier the number is not compacted.
fn compact_tiers(lang: &str, region: &str, long: bool) -> &'static [(f64, &'static str)] {
    match lang {
        "ja" => &[(1e12, "兆"), (1e8, "億"), (1e4, "万")],
        "zh" => {
            if matches!(region, "TW" | "HK" | "MO") {
                &[(1e12, "兆"), (1e8, "億"), (1e4, "萬")]
            } else {
                &[(1e12, "兆"), (1e8, "亿"), (1e4, "万")]
            }
        }
        "ko" => &[(1e12, "조"), (1e8, "억"), (1e4, "만"), (1e3, "천")],
        "de" => {
            if long {
                &[(1e12, " Billionen"), (1e9, " Milliarden"), (1e6, " Millionen"), (1e3, " Tausend")]
            } else {
                &[(1e12, "\u{a0}Bio."), (1e9, "\u{a0}Mrd."), (1e6, "\u{a0}Mio.")]
            }
        }
        // Indian English uses the lakh/crore system in short notation.
        "en" if region == "IN" && !long => &[(1e7, "Cr"), (1e5, "L"), (1e3, "K")],
        _ => {
            if long {
                &[(1e12, " trillion"), (1e9, " billion"), (1e6, " million"), (1e3, " thousand")]
            } else {
                &[(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "K")]
            }
        }
    }
}

fn unit_wrap(num: &str, unit: &str, display: &str, plural: bool) -> String {
    if display == "long" {
        return format!("{num} {}", unit_long_en(unit, plural));
    }
    let name = unit_short_name(unit).unwrap_or(unit);
    // The narrow style attaches the unit with no space ("987km/h"); short keeps a space.
    if display == "narrow" {
        format!("{num}{name}")
    } else {
        format!("{num} {name}")
    }
}

/// The English long display name of a sanctioned unit (regular +s plural; "X-per-Y" pluralizes the
/// numerator and keeps a singular denominator).
fn unit_long_en(unit: &str, plural: bool) -> String {
    if let Some((a, b)) = unit.split_once("-per-") {
        return format!("{} per {}", unit_long_en(a, plural), unit_long_en(b, false));
    }
    let name = unit.replace('-', " ");
    if plural {
        format!("{name}s")
    } else {
        name
    }
}

/// Replace the ASCII digits of a formatted number with the glyphs of numbering system `nu` (a no-op
/// for `latn` or an unknown system).
pub(crate) fn xlate_digits(s: &str, nu: &str) -> String {
    if nu == "latn" {
        return s.to_string();
    }
    match crate::numbering::NUMBERING.iter().find(|(id, _)| *id == nu) {
        Some((_, glyphs)) => s
            .chars()
            .map(|c| if c.is_ascii_digit() { glyphs[c as usize - '0' as usize] } else { c })
            .collect(),
        None => s.to_string(),
    }
}

fn format_number(i: &mut Interp, this: &Value, x: &Value) -> Result<Value, Value> {
    let o = instance(i, this)?;
    let n = to_intl_number(i, x)?;
    let s = assemble_number(i, &o, n);
    Ok(Value::from_string(xlate_digits(&s, &get_str(&o, "__nf_nu"))))
}

/// The trailing-affix classification for this formatter's parts (compact suffix vs plain).
fn suffix_type_of(o: &Gc) -> String {
    if get_str(o, "__nf_style") == "unit" {
        "unit".to_string()
    } else if get_str(o, "__nf_notation") == "compact" {
        "compact".to_string()
    } else {
        "literal".to_string()
    }
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
    let nu = get_str(&o, "__nf_nu");
    let (dec, grp) = loc_seps(&o);
    // Unit style: rebuild from the CLDR pattern so a unit prefix/suffix (e.g. ko "시속 {0}킬로미터")
    // is tagged as unit/literal around the number's own parts.
    let parts = if get_str(&o, "__nf_style") == "unit" && n.is_finite() {
        if let Some(pat) = unit_pattern_of(&o, n) {
            let (pre, post) = pat.split_once("{0}").unwrap_or(("", ""));
            let num_only = whole
                .strip_prefix(pre)
                .and_then(|s| s.strip_suffix(post))
                .unwrap_or(&whole);
            let mut p = unit_affix_parts(pre, true);
            p.extend(decompose_parts(num_only, "literal", dec, grp));
            p.extend(unit_affix_parts(post, false));
            p
        } else {
            decompose_parts(&whole, &suffix_type_of(&o), dec, grp)
        }
    } else {
        decompose_parts(&whole, &suffix_type_of(&o), dec, grp)
    };
    let arr: Vec<Value> = parts
        .into_iter()
        .map(|(t, v)| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str(t));
            // Localize the digits of numeric parts to the numbering system.
            let v = if matches!(t, "integer" | "fraction") { xlate_digits(&v, &nu) } else { v };
            set_data(&ob, "value", Value::from_string(v));
            Value::Obj(ob)
        })
        .collect();
    Ok(i.make_array(arr))
}

/// Break an assembled number string into typed parts (minusSign/plusSign, currency/percentSign/
/// literal affixes, grouped integer, decimal, fraction, and the exponent group). `suffix_type`
/// classifies the trailing affix ("compact" for compact notation, else percent/literal by content).
/// The formatter locale's primary language subtag.
fn o_lang(o: &Gc) -> &'static str {
    let loc = get_str(o, "__nf_locale");
    match loc.split('-').next().unwrap_or("en") {
        "de" => "de", "fr" => "fr", "es" => "es", "it" => "it", "pt" => "pt", "nl" => "nl",
        "ja" => "ja", "ko" => "ko", "zh" => "zh", "ru" => "ru", "ar" => "ar", "pl" => "pl",
        _ => "en",
    }
}

/// The CLDR unit-display pattern ("{0} km/h") for a formatter and value (plural category from the
/// value; zh split by script, en-IN region-specific).
fn unit_pattern_of(o: &Gc, value: f64) -> Option<String> {
    let unit = get_str(o, "__nf_unit");
    let disp = get_str(o, "__nf_unitdisplay");
    let style = if disp.is_empty() { "short" } else { disp.as_str() };
    let loc = get_str(o, "__nf_locale");
    let mut lp = loc.split('-');
    let lang = lp.next().unwrap_or("en");
    let region = lp.find(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_uppercase())).unwrap_or("");
    let cldr_loc = match (lang, region) {
        ("zh", "TW" | "HK" | "MO") => "zh-Hant",
        ("zh", _) => "zh-Hans",
        ("en", "IN") => "en-IN",
        _ => lang,
    };
    let cat = crate::intl::data::plural_cardinal(o_lang(o), value.abs().trunc() as u64, value.fract() != 0.0);
    crate::cldr_units::unit_pattern(cldr_loc, &unit, style, cat)
        .or_else(|| crate::cldr_units::unit_pattern(cldr_loc, &unit, style, "other"))
        .or_else(|| crate::cldr_units::unit_pattern(lang, &unit, style, "other"))
        .map(|s| s.to_string())
}

/// Tokenize a unit pattern's pre/post affix into (unit, literal) parts: the unit name is the
/// non-space run, adjoining spaces are literals (a `pre` affix ends with the separator, a `post`
/// affix begins with it).
fn unit_affix_parts(text: &str, is_pre: bool) -> Vec<(&'static str, String)> {
    if text.is_empty() {
        return Vec::new();
    }
    let sp = |c: char| c == ' ' || c == '\u{a0}';
    let mut v = Vec::new();
    if is_pre {
        let unit = text.trim_end_matches(sp);
        if !unit.is_empty() {
            v.push(("unit", unit.to_string()));
        }
        let tail = &text[unit.len()..];
        if !tail.is_empty() {
            v.push(("literal", tail.to_string()));
        }
    } else {
        let unit = text.trim_start_matches(sp);
        let lead = &text[..text.len() - unit.len()];
        if !lead.is_empty() {
            v.push(("literal", lead.to_string()));
        }
        if !unit.is_empty() {
            v.push(("unit", unit.to_string()));
        }
    }
    v
}

/// The (decimal, group) separator chars for a formatter's locale.
fn loc_seps(o: &Gc) -> (char, char) {
    let loc = get_str(o, "__nf_locale");
    let mut lp = loc.split('-');
    let lang = lp.next().unwrap_or("en");
    let region = lp.find(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_uppercase())).unwrap_or("");
    let (dec, grp, _) = crate::intl::data::number_symbols(lang, region);
    (dec.chars().next().unwrap_or('.'), grp.chars().next().unwrap_or(','))
}

fn decompose_parts(s: &str, suffix_type: &str, dec: char, grp: char) -> Vec<(&'static str, String)> {
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
    // Accounting notation's opening parenthesis is its own literal part.
    if idx < bytes.len() && bytes[idx] == '(' {
        parts.push(("literal", "(".to_string()));
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
    while idx < bytes.len() && (bytes[idx].is_ascii_digit() || bytes[idx] == grp) {
        idx += 1;
    }
    let int_str: String = bytes[int_start..idx].iter().collect();
    for seg in split_grouped(&int_str, grp) {
        parts.push(seg);
    }
    // Decimal + fraction.
    if idx < bytes.len() && bytes[idx] == dec {
        parts.push(("decimal", dec.to_string()));
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        parts.push(("fraction", bytes[frac_start..idx].iter().collect()));
    }
    // Trailing affix (percent sign, unit, compact suffix, or literal).
    let suffix: String = bytes[idx..].iter().collect();
    if !suffix.is_empty() {
        if suffix_type == "unit" {
            // The leading separator space(s) are a literal; the rest is one unit part (internal
            // spaces, e.g. "kilometers per hour", stay within the unit).
            let trimmed = suffix.trim_start_matches([' ', '\u{a0}']);
            let lead = &suffix[..suffix.len() - trimmed.len()];
            if !lead.is_empty() {
                parts.push(("literal", lead.to_string()));
            }
            if !trimmed.is_empty() {
                parts.push(("unit", trimmed.to_string()));
            }
        } else if suffix.contains('%') {
            parts.push(("percentSign", suffix));
        } else if suffix_type == "compact" {
            // A leading space separates the number from the compact word; it is its own literal.
            let trimmed = suffix.trim_start_matches(' ');
            if trimmed.len() < suffix.len() {
                parts.push(("literal", " ".to_string()));
            }
            if trimmed == ")" {
                parts.push(("literal", ")".to_string()));
            } else if !trimmed.is_empty() {
                parts.push(("compact", trimmed.to_string()));
            }
        } else {
            parts.push(("literal", suffix));
        }
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

/// Split a grouped integer like "12,345" into integer/group parts (group char is locale-specific).
fn split_grouped(int_str: &str, grp: char) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in int_str.chars() {
        if c == grp {
            if !cur.is_empty() {
                out.push(("integer", std::mem::take(&mut cur)));
            }
            out.push(("group", grp.to_string()));
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
    // useGrouping/notation/compactDisplay/signDisplay precede the rounding options in key order.
    set_data(&res, "useGrouping", o.borrow().props.get("__nf_grouping").map(|p| p.value.clone()).unwrap_or(Value::str("auto")));
    put(i, &res, "notation", "__nf_notation");
    // compactDisplay only appears when notation is compact.
    if matches!(o.borrow().props.get("__nf_notation").map(|p| p.value.clone()), Some(Value::Str(s)) if &*s == "compact") {
        put(i, &res, "compactDisplay", "__nf_compactdisplay");
    }
    put(i, &res, "signDisplay", "__nf_signdisplay");
    put(i, &res, "roundingIncrement", "__nf_roundingincrement");
    put(i, &res, "roundingMode", "__nf_roundingmode");
    put(i, &res, "roundingPriority", "__nf_roundingpriority");
    put(i, &res, "trailingZeroDisplay", "__nf_trailingzero");
    Ok(Value::Obj(res))
}
