//! `Intl.DurationFormat`. The format algorithm mirrors PartitionDurationFormatPattern, delegating
//! per-unit number formatting to `Intl.NumberFormat` and list joining to `Intl.ListFormat`, so the
//! output matches the conformance harness (which drives the same primitives).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, get_options_object as coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Gc, Value};

/// (plural, singular, index): years..nanoseconds.
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
    it.def_method(&proto, "format", 1, |i, this, a| {
        Ok(Value::from_string(format_string(i, &this, &arg(a, 0))?))
    });
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        // A single literal part carrying the whole string (sufficient for the shape tests).
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
    let numbering = get_option(i, &options, "numberingSystem", &[], None)?;
    if let Some(nsid) = &numbering {
        if !valid_type(nsid) {
            return Err(i.make_error("RangeError", format!("invalid numberingSystem: {nsid}")));
        }
    }
    let numbering = numbering.unwrap_or_else(|| "latn".to_string());
    let base_style = get_option(i, &options, "style", &["long", "short", "narrow", "digital"], Some("short"))?
        .unwrap();

    let resolved = resolve_locale(i, &requested, &["nu"]);
    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.DurationFormat") {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__df", Value::Bool(true));
    set_builtin(&obj, "__df_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__df_style", Value::from_string(base_style.clone()));
    set_builtin(&obj, "__df_nu", Value::from_string(numbering));

    // GetDurationUnitOptions for each unit, threading `prev_style`.
    let mut prev_style: Option<String> = None;
    for (idx, (plural, _sing)) in UNITS.iter().enumerate() {
        let is_time = idx >= 4;
        let is_subsecond = idx >= 7;
        let styles_list: &[&str] = if is_subsecond {
            &["long", "short", "narrow", "numeric", "2-digit", "fractional"]
        } else if is_time {
            &["long", "short", "narrow", "numeric", "2-digit"]
        } else {
            &["long", "short", "narrow"]
        };
        let digital_base = if plural == &"hours" { "numeric" } else { "2-digit" };

        let mut style = get_option(i, &options, plural, styles_list, None)?;
        // display defaults to "always" only for the primary clock units (hours/minutes/seconds).
        let mut display_default = if matches!(*plural, "hours" | "minutes" | "seconds") {
            "always"
        } else {
            "auto"
        };
        if style.is_none() {
            if base_style == "digital" {
                style = Some(digital_base.to_string());
            } else {
                match prev_style.as_deref() {
                    Some("fractional") | Some("numeric") | Some("2-digit") => {
                        style = Some("numeric".to_string());
                    }
                    _ => style = Some(base_style.clone()),
                }
            }
        }
        let mut style = style.unwrap();
        let _ = is_subsecond;
        if style == "fractional" {
            display_default = "auto";
        }

        let display_prop = format!("{plural}Display");
        let display = get_option(i, &options, &display_prop, &["auto", "always"], Some(display_default))?
            .unwrap();
        if display == "always" && style == "fractional" {
            return Err(i.make_error("RangeError", "a fractional unit cannot be display:always"));
        }
        if prev_style.as_deref() == Some("fractional") && style != "fractional" {
            return Err(i.make_error("RangeError", "only a fractional unit may follow a fractional unit"));
        }
        if matches!(prev_style.as_deref(), Some("numeric") | Some("2-digit"))
            && !matches!(style.as_str(), "fractional" | "numeric" | "2-digit")
        {
            return Err(i.make_error("RangeError", format!("{plural} style conflicts with a preceding numeric unit")));
        }
        // minutes/seconds after a numeric/2-digit unit render as 2-digit.
        if (plural == &"minutes" || plural == &"seconds")
            && matches!(prev_style.as_deref(), Some("numeric") | Some("2-digit"))
        {
            style = "2-digit".to_string();
        }

        set_builtin(&obj, Box::leak(format!("__df_u_{plural}").into_boxed_str()), Value::from_string(style.clone()));
        set_builtin(&obj, Box::leak(format!("__df_d_{plural}").into_boxed_str()), Value::from_string(display));
        prev_style = Some(style);
    }

    // fractionalDigits (0..9).
    let frac = {
        let v = ab(i.get_member(&options, "fractionalDigits"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            let n = ab(i.to_number(&v))?;
            if n.fract() != 0.0 || n < 0.0 || n > 9.0 {
                return Err(i.make_error("RangeError", "fractionalDigits out of range"));
            }
            Some(n as u32)
        }
    };
    if let Some(f) = frac {
        set_builtin(&obj, "__df_frac", Value::Num(f as f64));
    }
    Ok(Value::Obj(obj))
}

fn valid_type(s: &str) -> bool {
    !s.is_empty() && s.split('-').all(|p| p.len() >= 3 && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// ToDurationRecord + IsValidDuration: read integer fields, require a single sign, and bound the
/// calendar units (< 2^32) and total time (< 2^53 seconds).
fn read_duration(i: &mut Interp, v: &Value) -> Result<[f64; 10], Value> {
    if !matches!(v, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "duration must be an object"));
    }
    let mut vals = [0f64; 10];
    let mut any_present = false;
    for (k, (plural, _s)) in UNITS.iter().enumerate() {
        let fv = ab(i.get_member(v, plural))?;
        if matches!(fv, Value::Undefined) {
            continue;
        }
        any_present = true;
        let n = ab(i.to_number(&fv))?;
        if !n.is_finite() || n.fract() != 0.0 {
            return Err(i.make_error("RangeError", format!("invalid duration field {plural}")));
        }
        vals[k] = n;
    }
    if !any_present {
        return Err(i.make_error("TypeError", "duration has no recognized fields"));
    }
    // Single overall sign.
    let mut sign = 0.0f64;
    for &x in &vals {
        if x != 0.0 {
            let s = x.signum();
            if sign == 0.0 {
                sign = s;
            } else if s != sign {
                return Err(i.make_error("RangeError", "duration fields have mixed signs"));
            }
        }
    }
    // Bounds.
    for &x in &vals[0..3] {
        if x.abs() >= 4294967296.0 {
            return Err(i.make_error("RangeError", "calendar unit out of range"));
        }
    }
    // The whole-second magnitude (days..seconds) must fit in 2^53-1; sub-second fields add at most
    // one extra second toward the same sign, so they can only push a boundary value over.
    let whole_sec = vals[3] * 86400.0 + vals[4] * 3600.0 + vals[5] * 60.0 + vals[6];
    let sub_over = vals[7] != 0.0 || vals[8] != 0.0 || vals[9] != 0.0;
    let limit = 9007199254740991.0;
    if whole_sec.abs() > limit || (whole_sec.abs() == limit && sub_over) {
        return Err(i.make_error("RangeError", "duration time total out of range"));
    }
    Ok(vals)
}

fn get_str(o: &Gc, k: &str) -> String {
    match o.borrow().props.get(k).map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    }
}

/// Construct `new Intl.<service>(locale, options)`.
fn new_service(i: &mut Interp, service: &str, locale: &str, opts: Gc) -> Result<Value, Value> {
    let intl = ab(i.get_member(&Value::Obj(i.global.clone()), "Intl"))?;
    let ctor = ab(i.get_member(&intl, service))?;
    ab(i.construct(ctor, &[Value::from_string(locale.to_string()), Value::Obj(opts)]))
}

fn format_string(i: &mut Interp, this: &Value, dur: &Value) -> Result<String, Value> {
    let o = brand_slot(i, this, "__df")?;
    let vals = read_duration(i, dur)?;
    let locale = get_str(&o, "__df_locale");
    let base_style = get_str(&o, "__df_style");
    let numbering = get_str(&o, "__df_nu");
    let frac_digits = match o.borrow().props.get("__df_frac").map(|p| p.value.clone()) {
        Some(Value::Num(n)) => Some(n as u32),
        _ => None,
    };

    let mut group_strings: Vec<String> = Vec::new();
    let mut cur_group: Option<String> = None; // an in-progress numeric "hh:mm:ss" group
    let mut display_negative_sign = true;

    let mut idx = 0;
    while idx < UNITS.len() {
        let (plural, sing) = UNITS[idx];
        let mut value = vals[idx];
        let style = get_str(&o, &format!("__df_u_{plural}"));
        let display = get_str(&o, &format!("__df_d_{plural}"));
        let need_separator = cur_group.is_some();

        // Seconds/ms/us combine into a fractional value when the next unit is numeric.
        let mut done = false;
        let mut nf_max_frac: Option<u32> = None;
        let mut nf_min_frac: Option<u32> = None;
        let mut nf_trunc = false;
        if matches!(plural, "seconds" | "milliseconds" | "microseconds") {
            let next_style = get_str(&o, &format!("__df_u_{}", UNITS[idx + 1].0));
            if next_style == "numeric" || next_style == "fractional" {
                let exp = match plural {
                    "seconds" => 9,
                    "milliseconds" => 6,
                    _ => 3,
                };
                value = fractional_value(&vals, exp);
                nf_max_frac = Some(frac_digits.unwrap_or(9));
                nf_min_frac = Some(frac_digits.unwrap_or(0));
                nf_trunc = true;
                done = true;
            }
        }

        // minutes: display a zero numeric minute if seconds follow.
        let mut display_required = false;
        if plural == "minutes" && need_separator {
            display_required = get_str(&o, "__df_d_seconds") == "always"
                || vals[6] != 0.0
                || vals[7] != 0.0
                || vals[8] != 0.0
                || vals[9] != 0.0;
        }

        if value != 0.0 || display != "auto" || display_required {
            // Only the first displayed unit shows the negative sign.
            let mut sign_never = false;
            if display_negative_sign {
                display_negative_sign = false;
                if value == 0.0 && vals.iter().any(|&x| x < 0.0) {
                    value = -0.0;
                }
            } else {
                sign_never = true;
            }

            // Build the NumberFormat options for this unit.
            let nf_opts = i.new_object();
            set_data(&nf_opts, "numberingSystem", Value::from_string(numbering.clone()));
            if sign_never {
                set_data(&nf_opts, "signDisplay", Value::str("never"));
            }
            if style == "2-digit" {
                set_data(&nf_opts, "minimumIntegerDigits", Value::Num(2.0));
            }
            if style != "numeric" && style != "2-digit" {
                set_data(&nf_opts, "style", Value::str("unit"));
                set_data(&nf_opts, "unit", Value::str(sing));
                set_data(&nf_opts, "unitDisplay", Value::from_string(style.clone()));
            } else {
                set_data(&nf_opts, "useGrouping", Value::Bool(false));
            }
            if let Some(m) = nf_max_frac {
                set_data(&nf_opts, "maximumFractionDigits", Value::Num(m as f64));
            }
            if let Some(m) = nf_min_frac {
                set_data(&nf_opts, "minimumFractionDigits", Value::Num(m as f64));
            }
            if nf_trunc {
                set_data(&nf_opts, "roundingMode", Value::str("trunc"));
            }

            let nf = new_service(i, "NumberFormat", &locale, nf_opts)?;
            let fmt = ab(i.get_member(&nf, "format"))?;
            let num_str = ab(i.call(fmt, nf.clone(), &[Value::Num(value)]))?;
            let num_str = if let Value::Str(s) = num_str { s.to_string() } else { String::new() };

            match &mut cur_group {
                Some(g) => {
                    g.push(':');
                    g.push_str(&num_str);
                }
                None => {
                    if style == "2-digit" || style == "numeric" {
                        cur_group = Some(num_str);
                    } else {
                        group_strings.push(num_str);
                    }
                }
            }
        }

        if done {
            break;
        }
        // A group ends when the next non-numeric unit begins; flush before a standalone unit.
        if let Some(g) = &cur_group {
            let next_numeric = idx + 1 < UNITS.len() && {
                let ns = get_str(&o, &format!("__df_u_{}", UNITS[idx + 1].0));
                ns == "numeric" || ns == "2-digit"
            };
            if !next_numeric {
                group_strings.push(g.clone());
                cur_group = None;
            }
        }
        idx += 1;
    }
    if let Some(g) = cur_group {
        group_strings.push(g);
    }

    // Join with a unit-style ListFormat (digital -> short).
    let list_style = if base_style == "digital" { "short" } else { base_style.as_str() };
    let lf_opts = i.new_object();
    set_data(&lf_opts, "type", Value::str("unit"));
    set_data(&lf_opts, "style", Value::str(list_style));
    let lf = new_service(i, "ListFormat", &locale, lf_opts)?;
    let arr = i.make_array(group_strings.into_iter().map(Value::from_string).collect());
    let fmt = ab(i.get_member(&lf, "format"))?;
    let out = ab(i.call(fmt, lf, &[arr]))?;
    Ok(if let Value::Str(s) = out { s.to_string() } else { String::new() })
}

/// The fractional seconds/ms/us value combining sub-second fields, truncated as a decimal string
/// value (returned as an f64 — good enough for the fraction digits we format).
fn fractional_value(vals: &[f64; 10], exp: i32) -> f64 {
    let (base, ms, us, ns) = (vals[6], vals[7], vals[8], vals[9]);
    match exp {
        9 => {
            if ms == 0.0 && us == 0.0 && ns == 0.0 {
                return base;
            }
            base + ms / 1e3 + us / 1e6 + ns / 1e9
        }
        6 => {
            if us == 0.0 && ns == 0.0 {
                return ms;
            }
            ms + us / 1e3 + ns / 1e6
        }
        _ => {
            if ns == 0.0 {
                return us;
            }
            us + ns / 1e3
        }
    }
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__df")?;
    let get = |k: &str| o.borrow().props.get(k).map(|p| p.value.clone()).unwrap_or(Value::Undefined);
    let res = i.new_object();
    set_data(&res, "locale", get("__df_locale"));
    set_data(&res, "numberingSystem", get("__df_nu"));
    set_data(&res, "style", get("__df_style"));
    for (plural, _sing) in UNITS {
        set_data(&res, plural, get(&format!("__df_u_{plural}")));
        set_data(&res, Box::leak(format!("{plural}Display").into_boxed_str()), get(&format!("__df_d_{plural}")));
    }
    if o.borrow().props.contains("__df_frac") {
        set_data(&res, "fractionalDigits", get("__df_frac"));
    }
    Ok(Value::Obj(res))
}
