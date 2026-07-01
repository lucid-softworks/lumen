//! `Intl.RelativeTimeFormat` (English data; `numeric: "always"` and a small `"auto"` subset).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, get_options_object as coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Value};

const UNITS: &[&str] = &[
    "year", "years", "quarter", "quarters", "month", "months", "week", "weeks", "day", "days",
    "hour", "hours", "minute", "minutes", "second", "seconds",
];

pub fn install(it: &mut Interp, ns: &crate::value::Gc) {
    let (ctor, proto) = make_service(it, ns, "RelativeTimeFormat", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "format", 2, |i, this, a| {
        format(i, &this, &arg(a, 0), &arg(a, 1), false)
    });
    it.def_method(&proto, "formatToParts", 2, |i, this, a| {
        format(i, &this, &arg(a, 0), &arg(a, 1), true)
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.RelativeTimeFormat requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let numeric = get_option(i, &options, "numeric", &["always", "auto"], Some("always"))?.unwrap();
    let style = get_option(i, &options, "style", &["long", "short", "narrow"], Some("long"))?
        .unwrap();
    let resolved = resolve_locale(i, &requested, &["nu"]);

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.RelativeTimeFormat") {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__rtf", Value::Bool(true));
    set_builtin(&obj, "__rtf_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__rtf_numeric", Value::from_string(numeric));
    set_builtin(&obj, "__rtf_style", Value::from_string(style));
    set_builtin(&obj, "__rtf_nu", Value::str("latn"));
    Ok(Value::Obj(obj))
}

/// Singular unit name from an accepted unit string (strips a trailing plural "s").
fn singular(unit: &str) -> Option<&'static str> {
    match unit {
        "year" | "years" => Some("year"),
        "quarter" | "quarters" => Some("quarter"),
        "month" | "months" => Some("month"),
        "week" | "weeks" => Some("week"),
        "day" | "days" => Some("day"),
        "hour" | "hours" => Some("hour"),
        "minute" | "minutes" => Some("minute"),
        "second" | "seconds" => Some("second"),
        _ => None,
    }
}

/// The English "auto" phrasing for the common near values, if any.
fn auto_phrase(unit: &str, v: f64) -> Option<&'static str> {
    match (unit, v as i64) {
        ("day", 0) => Some("today"),
        ("day", 1) => Some("tomorrow"),
        ("day", -1) => Some("yesterday"),
        ("week", 0) => Some("this week"),
        ("week", 1) => Some("next week"),
        ("week", -1) => Some("last week"),
        ("month", 0) => Some("this month"),
        ("month", 1) => Some("next month"),
        ("month", -1) => Some("last month"),
        ("year", 0) => Some("this year"),
        ("year", 1) => Some("next year"),
        ("year", -1) => Some("last year"),
        ("quarter", 0) => Some("this quarter"),
        ("quarter", 1) => Some("next quarter"),
        ("quarter", -1) => Some("last quarter"),
        ("hour", 0) => Some("this hour"),
        ("minute", 0) => Some("this minute"),
        ("second", 0) => Some("now"),
        _ => None,
    }
}

fn format(
    i: &mut Interp,
    this: &Value,
    value: &Value,
    unit: &Value,
    to_parts: bool,
) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__rtf")?;
    let numeric = match o.borrow().props.get("__rtf_numeric").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "always".to_string(),
    };
    let v = ab(i.to_number(value))?;
    if !v.is_finite() {
        return Err(i.make_error("RangeError", "value must be finite"));
    }
    let unit_s = ab(i.to_string(unit))?.to_string();
    let sing = singular(&unit_s)
        .ok_or_else(|| i.make_error("RangeError", format!("invalid unit: {unit_s}")))?;

    if numeric == "auto" {
        if let Some(phrase) = auto_phrase(sing, v) {
            if to_parts {
                let ob = i.new_object();
                set_data(&ob, "type", Value::str("literal"));
                set_data(&ob, "value", Value::str(phrase));
                return Ok(i.make_array(vec![Value::Obj(ob)]));
            }
            return Ok(Value::str(phrase));
        }
    }

    // Numeric phrasing: "<n> <unit>[s] ago" (past) / "in <n> <unit>[s]" (future).
    let past = v < 0.0 || (v == 0.0 && v.is_sign_negative());
    let n = v.abs();
    let plural = n != 1.0;
    let unit_word = if plural {
        format!("{sing}s")
    } else {
        sing.to_string()
    };
    let num_str = crate::intl::relativetimeformat::fmt_num(n);
    if to_parts {
        let mut arr: Vec<Value> = Vec::new();
        let push_lit = |i: &mut Interp, arr: &mut Vec<Value>, s: &str| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str("literal"));
            set_data(&ob, "value", Value::from_string(s.to_string()));
            arr.push(Value::Obj(ob));
        };
        let push_num = |i: &mut Interp, arr: &mut Vec<Value>, s: &str, unit: &str| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str("integer"));
            set_data(&ob, "value", Value::from_string(s.to_string()));
            set_data(&ob, "unit", Value::from_string(format!("{unit}")));
            arr.push(Value::Obj(ob));
        };
        if past {
            push_num(i, &mut arr, &num_str, sing);
            push_lit(i, &mut arr, &format!(" {unit_word} ago"));
        } else {
            push_lit(i, &mut arr, "in ");
            push_num(i, &mut arr, &num_str, sing);
            push_lit(i, &mut arr, &format!(" {unit_word}"));
        }
        return Ok(i.make_array(arr));
    }
    let out = if past {
        format!("{num_str} {unit_word} ago")
    } else {
        format!("in {num_str} {unit_word}")
    };
    Ok(Value::from_string(out))
}

/// Format a non-negative number the way our minimal number formatter would (integer or decimal).
pub(crate) fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n}");
        s
    }
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__rtf")?;
    let get = |k: &str| o.borrow().props.get(k).map(|p| p.value.clone()).unwrap_or(Value::Undefined);
    let res = i.new_object();
    set_data(&res, "locale", get("__rtf_locale"));
    set_data(&res, "style", get("__rtf_style"));
    set_data(&res, "numeric", get("__rtf_numeric"));
    set_data(&res, "numberingSystem", get("__rtf_nu"));
    Ok(Value::Obj(res))
}

// Keep UNITS referenced (used by the validity table above conceptually).
#[allow(dead_code)]
fn _units() -> &'static [&'static str] {
    UNITS
}
