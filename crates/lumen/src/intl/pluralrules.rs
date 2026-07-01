//! `Intl.PluralRules` (a subset: default digit options, cardinal + a coarse ordinal).

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
    resolve_locale,
};
use super::{
    ab, arg, canonicalize_locale_list, data, get_options_object as coerce_options, make_service,
};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Value};

pub fn install(it: &mut Interp, ns: &crate::value::Gc) {
    let (ctor, proto) = make_service(it, ns, "PluralRules", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "select", 1, |i, this, a| {
        select(i, &this, &arg(a, 0))
    });
    it.def_method(&proto, "selectRange", 2, |i, this, a| {
        select_range(i, &this, &arg(a, 0), &arg(a, 1))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.PluralRules requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let kind = get_option(
        i,
        &options,
        "type",
        &["cardinal", "ordinal"],
        Some("cardinal"),
    )?
    .unwrap();
    // Read order: type, notation, compactDisplay, then the digit + rounding options.
    let notation = get_option(
        i,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?
    .unwrap();
    let compact_display = get_option(
        i,
        &options,
        "compactDisplay",
        &["short", "long"],
        Some("short"),
    )?
    .unwrap();
    let min_int = read_digits(i, &options, "minimumIntegerDigits", 1, 21, 1)?;
    let min_frac = read_digits(i, &options, "minimumFractionDigits", 0, 100, 0)?;
    let max_frac_default = min_frac.max(3);
    let max_frac = read_digits(
        i,
        &options,
        "maximumFractionDigits",
        min_frac,
        100,
        max_frac_default,
    )?;
    let mnsd = read_digits_opt(i, &options, "minimumSignificantDigits", 1, 21)?;
    let mxsd = read_digits_opt(i, &options, "maximumSignificantDigits", 1, 21)?;
    // When either significant-digit bound is present, both are resolved (min->1, max->21).
    let (min_sig, max_sig) = if mnsd.is_some() || mxsd.is_some() {
        (Some(mnsd.unwrap_or(1)), Some(mxsd.unwrap_or(21)))
    } else {
        (None, None)
    };
    let _rinc = {
        let v = ab(i.get_member(&options, "roundingIncrement"))?;
        if matches!(v, Value::Undefined) {
            1.0
        } else {
            ab(i.to_number(&v))?
        }
    };
    let rounding_mode = get_option(
        i,
        &options,
        "roundingMode",
        &[
            "ceil",
            "floor",
            "expand",
            "trunc",
            "halfCeil",
            "halfFloor",
            "halfExpand",
            "halfTrunc",
            "halfEven",
        ],
        Some("halfExpand"),
    )?
    .unwrap();
    let _rprio = get_option(
        i,
        &options,
        "roundingPriority",
        &["auto", "morePrecision", "lessPrecision"],
        Some("auto"),
    )?;
    let _tzd = get_option(
        i,
        &options,
        "trailingZeroDisplay",
        &["auto", "stripIfInteger"],
        Some("auto"),
    )?;
    let resolved = resolve_locale(i, &requested, &[]);

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.PluralRules")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__pr", Value::Bool(true));
    set_builtin(&obj, "__pr_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__pr_type", Value::from_string(kind));
    set_builtin(&obj, "__pr_minint", Value::Num(min_int as f64));
    set_builtin(&obj, "__pr_minfrac", Value::Num(min_frac as f64));
    set_builtin(&obj, "__pr_maxfrac", Value::Num(max_frac as f64));
    set_builtin(&obj, "__pr_notation", Value::from_string(notation.clone()));
    set_builtin(
        &obj,
        "__pr_compactdisplay",
        Value::from_string(compact_display),
    );
    set_builtin(&obj, "__pr_roundingmode", Value::from_string(rounding_mode));
    if let (Some(mn), Some(mx)) = (min_sig, max_sig) {
        set_builtin(&obj, "__pr_minsig", Value::Num(mn as f64));
        set_builtin(&obj, "__pr_maxsig", Value::Num(mx as f64));
    }
    Ok(Value::Obj(obj))
}

fn read_digits(
    i: &mut Interp,
    options: &Value,
    prop: &str,
    lo: u32,
    hi: u32,
    fallback: u32,
) -> Result<u32, Value> {
    let v = ab(i.get_member(options, prop))?;
    if matches!(v, Value::Undefined) {
        return Ok(fallback);
    }
    let n = ab(i.to_number(&v))?;
    if n.is_nan() || n < lo as f64 || n > hi as f64 {
        return Err(i.make_error("RangeError", format!("{prop} out of range")));
    }
    Ok(n.floor() as u32)
}

fn read_digits_opt(
    i: &mut Interp,
    options: &Value,
    prop: &str,
    lo: u32,
    hi: u32,
) -> Result<Option<u32>, Value> {
    let v = ab(i.get_member(options, prop))?;
    if matches!(v, Value::Undefined) {
        return Ok(None);
    }
    let n = ab(i.to_number(&v))?;
    if n.is_nan() || n < lo as f64 || n > hi as f64 {
        return Err(i.make_error("RangeError", format!("{prop} out of range")));
    }
    Ok(Some(n.floor() as u32))
}

fn select(i: &mut Interp, this: &Value, n: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__pr")?;
    let locale = match o.borrow().props.get("__pr_locale").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "en".to_string(),
    };
    let x = ab(i.to_number(n))?;
    let compact = matches!(
        o.borrow().props.get("__pr_notation").map(|p| p.value.clone()),
        Some(Value::Str(s)) if &*s == "compact"
    );
    let lang = locale.split('-').next().unwrap_or("en");
    let cat = if x.is_nan() || x.is_infinite() {
        "other"
    } else {
        let ax = x.abs();
        let int = ax.trunc() as u64;
        let has_fraction = ax.fract() != 0.0;
        // The compact exponent operand `e`: the largest 10^(3k) tier not exceeding the magnitude.
        let e = if compact {
            let mut v = ax;
            let mut ex = 0i32;
            while v >= 1000.0 {
                v /= 1000.0;
                ex += 3;
            }
            ex
        } else {
            0
        };
        data::plural_cardinal(lang, int, has_fraction, e)
    };
    Ok(Value::str(cat))
}

/// PluralRules.prototype.selectRange: both endpoints are required and must be numbers (NaN throws
/// RangeError). We resolve the category of the end value (CLDR range rules collapse to the end
/// category for the locales we ship).
fn select_range(i: &mut Interp, this: &Value, start: &Value, end: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__pr")?;
    if matches!(start, Value::Undefined) || matches!(end, Value::Undefined) {
        return Err(i.make_error("TypeError", "selectRange requires two numbers"));
    }
    let x = ab(i.to_number(start))?;
    let y = ab(i.to_number(end))?;
    if x.is_nan() || y.is_nan() {
        return Err(i.make_error("RangeError", "selectRange arguments must not be NaN"));
    }
    let locale = match o.borrow().props.get("__pr_locale").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "en".to_string(),
    };
    let lang = locale.split('-').next().unwrap_or("en");
    let cat = if y.is_infinite() {
        "other"
    } else {
        let ay = y.abs();
        data::plural_cardinal(lang, ay.trunc() as u64, ay.fract() != 0.0, 0)
    };
    Ok(Value::str(cat))
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__pr")?;
    let get = |k: &str| {
        o.borrow()
            .props
            .get(k)
            .map(|p| p.value.clone())
            .unwrap_or(Value::Undefined)
    };
    let res = i.new_object();
    set_data(&res, "locale", get("__pr_locale"));
    set_data(&res, "type", get("__pr_type"));
    set_data(&res, "notation", get("__pr_notation"));
    // compactDisplay is present only for compact notation.
    if matches!(get("__pr_notation"), Value::Str(s) if &*s == "compact") {
        set_data(&res, "compactDisplay", get("__pr_compactdisplay"));
    }
    set_data(&res, "minimumIntegerDigits", get("__pr_minint"));
    set_data(&res, "minimumFractionDigits", get("__pr_minfrac"));
    set_data(&res, "maximumFractionDigits", get("__pr_maxfrac"));
    // Significant-digit bounds, when significant-digit rounding is in effect, precede pluralCategories.
    if !matches!(get("__pr_minsig"), Value::Undefined) {
        set_data(&res, "minimumSignificantDigits", get("__pr_minsig"));
        set_data(&res, "maximumSignificantDigits", get("__pr_maxsig"));
    }
    let locale = match get("__pr_locale") {
        Value::Str(s) => s.to_string(),
        _ => "en".to_string(),
    };
    let lang = locale.split('-').next().unwrap_or("en").to_string();
    let cats: Vec<Value> = data::plural_categories(&lang)
        .iter()
        .map(|s| Value::str(*s))
        .collect();
    // pluralCategories precedes roundingMode in the resolvedOptions key order.
    set_data(&res, "pluralCategories", i.make_array(cats));
    set_data(&res, "roundingMode", get("__pr_roundingmode"));
    Ok(Value::Obj(res))
}
