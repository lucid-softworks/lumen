//! `Intl.PluralRules` (a subset: default digit options, cardinal + a coarse ordinal).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, data, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Value};

pub fn install(it: &mut Interp, ns: &crate::value::Gc) {
    let (ctor, proto) = make_service(it, ns, "PluralRules", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "select", 1, |i, this, a| select(i, &this, &arg(a, 0)));
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.PluralRules requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let kind = get_option(i, &options, "type", &["cardinal", "ordinal"], Some("cardinal"))?.unwrap();
    // Read order: type, notation, compactDisplay, then the digit + rounding options.
    let notation = get_option(
        i,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?
    .unwrap();
    let compact_display =
        get_option(i, &options, "compactDisplay", &["short", "long"], Some("short"))?.unwrap();
    let min_int = read_digits(i, &options, "minimumIntegerDigits", 1, 21, 1)?;
    let min_frac = read_digits(i, &options, "minimumFractionDigits", 0, 100, 0)?;
    let max_frac_default = min_frac.max(3);
    let max_frac = read_digits(i, &options, "maximumFractionDigits", min_frac, 100, max_frac_default)?;
    let _min_sig = read_digits_opt(i, &options, "minimumSignificantDigits", 1, 21)?;
    let _max_sig = read_digits_opt(i, &options, "maximumSignificantDigits", 1, 21)?;
    let _rinc = {
        let v = ab(i.get_member(&options, "roundingIncrement"))?;
        if matches!(v, Value::Undefined) { 1.0 } else { ab(i.to_number(&v))? }
    };
    let rounding_mode = get_option(
        i,
        &options,
        "roundingMode",
        &["ceil", "floor", "expand", "trunc", "halfCeil", "halfFloor", "halfExpand", "halfTrunc", "halfEven"],
        Some("halfExpand"),
    )?
    .unwrap();
    let _rprio = get_option(i, &options, "roundingPriority", &["auto", "morePrecision", "lessPrecision"], Some("auto"))?;
    let _tzd = get_option(i, &options, "trailingZeroDisplay", &["auto", "stripIfInteger"], Some("auto"))?;
    let resolved = resolve_locale(i, &requested, &[]);

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.PluralRules") {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__pr", Value::Bool(true));
    set_builtin(&obj, "__pr_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__pr_type", Value::from_string(kind));
    set_builtin(&obj, "__pr_minint", Value::Num(min_int as f64));
    set_builtin(&obj, "__pr_minfrac", Value::Num(min_frac as f64));
    set_builtin(&obj, "__pr_maxfrac", Value::Num(max_frac as f64));
    set_builtin(&obj, "__pr_notation", Value::from_string(notation));
    set_builtin(&obj, "__pr_compactdisplay", Value::from_string(compact_display));
    set_builtin(&obj, "__pr_roundingmode", Value::from_string(rounding_mode));
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

fn read_digits_opt(i: &mut Interp, options: &Value, prop: &str, lo: u32, hi: u32) -> Result<Option<u32>, Value> {
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
    let lang = locale.split('-').next().unwrap_or("en");
    let cat = if x.is_nan() || x.is_infinite() {
        "other"
    } else {
        let ax = x.abs();
        let int = ax.trunc() as u64;
        let has_fraction = ax.fract() != 0.0;
        data::plural_cardinal(lang, int, has_fraction)
    };
    Ok(Value::str(cat))
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__pr")?;
    let get = |k: &str| o.borrow().props.get(k).map(|p| p.value.clone()).unwrap_or(Value::Undefined);
    let res = i.new_object();
    set_data(&res, "locale", get("__pr_locale"));
    set_data(&res, "type", get("__pr_type"));
    set_data(&res, "notation", get("__pr_notation"));
    set_data(&res, "minimumIntegerDigits", get("__pr_minint"));
    set_data(&res, "minimumFractionDigits", get("__pr_minfrac"));
    set_data(&res, "maximumFractionDigits", get("__pr_maxfrac"));
    set_data(&res, "roundingMode", get("__pr_roundingmode"));
    let locale = match get("__pr_locale") {
        Value::Str(s) => s.to_string(),
        _ => "en".to_string(),
    };
    let lang = locale.split('-').next().unwrap_or("en").to_string();
    let cats: Vec<Value> = data::plural_categories(&lang)
        .iter()
        .map(|s| Value::str(*s))
        .collect();
    set_data(&res, "pluralCategories", i.make_array(cats));
    Ok(Value::Obj(res))
}
