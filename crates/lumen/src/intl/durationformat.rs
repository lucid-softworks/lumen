//! `Intl.DurationFormat` (English digital/short subset).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Gc, Value};

const UNITS: &[(&str, &str)] = &[
    ("years", "year"),
    ("months", "month"),
    ("weeks", "week"),
    ("days", "day"),
    ("hours", "hour"),
    ("minutes", "minute"),
    ("seconds", "second"),
    ("milliseconds", "millisecond"),
    ("microseconds", "microsecond"),
    ("nanoseconds", "nanosecond"),
];

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "DurationFormat", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "format", 1, |i, this, a| format(i, &this, &arg(a, 0)));
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        let s = format_string(i, &this, &arg(a, 0))?;
        let ob = i.new_object();
        set_data(&ob, "type", Value::str("literal"));
        set_data(&ob, "value", Value::from_string(s));
        Ok(i.make_array(vec![Value::Obj(ob)]))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.DurationFormat requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let style = get_option(i, &options, "style", &["long", "short", "narrow", "digital"], Some("short"))?
        .unwrap();
    let resolved = resolve_locale(i, &requested, &["nu"]);
    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.DurationFormat") {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__df", Value::Bool(true));
    set_builtin(&obj, "__df_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__df_style", Value::from_string(style));
    set_builtin(&obj, "__df_nu", Value::str("latn"));
    Ok(Value::Obj(obj))
}

/// ToDurationRecord: read the numeric duration fields off an object; a non-object or a bad field is
/// a TypeError/RangeError.
fn read_duration(i: &mut Interp, v: &Value) -> Result<Vec<(&'static str, f64)>, Value> {
    if !matches!(v, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "duration must be an object"));
    }
    let mut out = Vec::new();
    let mut any = false;
    for (plural, _sing) in UNITS {
        let fv = ab(i.get_member(v, plural))?;
        if matches!(fv, Value::Undefined) {
            continue;
        }
        let n = ab(i.to_number(&fv))?;
        if !n.is_finite() || n.fract() != 0.0 {
            return Err(i.make_error("RangeError", format!("invalid duration field {plural}")));
        }
        any = true;
        if n != 0.0 {
            out.push((*plural, n));
        }
    }
    let _ = any;
    Ok(out)
}

fn unit_label(sing: &str, plural: &str, n: f64, style: &str) -> String {
    if style == "long" {
        let word = if n == 1.0 { sing.to_string() } else { plural.to_string() };
        format!("{n} {word}")
    } else {
        // short/narrow abbreviations.
        let abbr = match sing {
            "year" => "yr",
            "month" => "mth",
            "week" => "wk",
            "day" => "day",
            "hour" => "hr",
            "minute" => "min",
            "second" => "sec",
            "millisecond" => "ms",
            "microsecond" => "μs",
            "nanosecond" => "ns",
            _ => sing,
        };
        format!("{n} {abbr}")
    }
}

fn format_string(i: &mut Interp, this: &Value, dur: &Value) -> Result<String, Value> {
    let o = brand_slot(i, this, "__df")?;
    let style = match o.borrow().props.get("__df_style").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "short".to_string(),
    };
    let fields = read_duration(i, dur)?;
    let parts: Vec<String> = fields
        .iter()
        .map(|(plural, n)| {
            let sing = UNITS.iter().find(|(p, _)| p == plural).map(|(_, s)| *s).unwrap_or(plural);
            unit_label(sing, plural, *n, &style)
        })
        .collect();
    Ok(parts.join(", "))
}

fn format(i: &mut Interp, this: &Value, dur: &Value) -> Result<Value, Value> {
    Ok(Value::from_string(format_string(i, this, dur)?))
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__df")?;
    let get = |k: &str| o.borrow().props.get(k).map(|p| p.value.clone()).unwrap_or(Value::Undefined);
    let res = i.new_object();
    set_data(&res, "locale", get("__df_locale"));
    set_data(&res, "numberingSystem", get("__df_nu"));
    set_data(&res, "style", get("__df_style"));
    Ok(Value::Obj(res))
}
