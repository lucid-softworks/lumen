//! `Intl.DurationFormat`. The format algorithm mirrors PartitionDurationFormatPattern, delegating
//! per-unit number formatting to `Intl.NumberFormat` and list joining to `Intl.ListFormat`, so the
//! output matches the conformance harness (which drives the same primitives).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
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
        format_to_parts(i, &this, &arg(a, 0))
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
    let base_style = get_option(i, &options, "style", &["long", "short", "narrow", "digital"], Some("short"))?
        .unwrap();

    let (resolved_locale, numbering) =
        super::service::resolve_locale_nu(&requested, numbering.as_deref());
    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.DurationFormat")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__df", Value::Bool(true));
    set_builtin(&obj, "__df_locale", Value::from_string(resolved_locale));
    set_builtin(&obj, "__df_style", Value::from_string(base_style.clone()));
    set_builtin(&obj, "__df_nu", Value::from_string(numbering));

    // GetDurationUnitOptions for each unit, threading `prev_style`.
    let mut prev_style: Option<String> = None;
    for (idx, (plural, _sing)) in UNITS.iter().enumerate() {
        let is_time = idx >= 4;
        let is_subsecond = idx >= 7;
        let styles_list: &[&str] = if is_subsecond {
            &["long", "short", "narrow", "numeric"]
        } else if is_time {
            &["long", "short", "narrow", "numeric", "2-digit"]
        } else {
            &["long", "short", "narrow"]
        };
        // GetDurationUnitOptions digitalBase: "short" for the calendar units, "numeric" for the
        // clock/sub-second units (per Table 3).
        let digital_base = if is_time { "numeric" } else { "short" };

        // Step 1-3: resolve the style and its display default.
        let style_opt = get_option(i, &options, plural, styles_list, None)?;
        let mut display_default = "always";
        let is_frac_second = matches!(*plural, "milliseconds" | "microseconds" | "nanoseconds");
        let style = match style_opt {
            Some(s) => s,
            None => {
                if base_style == "digital" {
                    if !matches!(*plural, "hours" | "minutes" | "seconds") {
                        display_default = "auto";
                    }
                    digital_base.to_string()
                } else {
                    display_default = "auto";
                    match prev_style.as_deref() {
                        Some("fractional") | Some("numeric") | Some("2-digit") => "numeric".to_string(),
                        _ => base_style.clone(),
                    }
                }
            }
        };
        // Step 4 folds a numeric sub-second unit into a "fractional" one; that style is only ever
        // observable as "numeric" (resolvedOptions maps it back, and the formatter folds it into the
        // preceding second), so we keep it "numeric" and merely mirror the displayDefault->"auto".
        let mut style = style;
        if is_frac_second && style == "numeric" {
            display_default = "auto";
        }
        let _ = is_subsecond;

        let display_prop = format!("{plural}Display");
        let display = get_option(i, &options, &display_prop, &["auto", "always"], Some(display_default))?
            .unwrap();
        if display == "always" && style == "fractional" {
            return Err(i.make_error("RangeError", "a fractional unit cannot be display:always"));
        }
        if prev_style.as_deref() == Some("fractional") && style != "fractional" {
            return Err(i.make_error("RangeError", "only a fractional unit may follow a fractional unit"));
        }
        // Step 6: a unit following a numeric/2-digit one must itself be numeric-ish; minutes/seconds
        // then render as 2-digit.
        if matches!(prev_style.as_deref(), Some("numeric") | Some("2-digit")) {
            if !matches!(style.as_str(), "fractional" | "numeric" | "2-digit") {
                return Err(i.make_error("RangeError", format!("{plural} style conflicts with a preceding numeric unit")));
            }
            if matches!(*plural, "minutes" | "seconds") {
                style = "2-digit".to_string();
            }
        }

        set_builtin(&obj, Box::leak(format!("__df_u_{plural}").into_boxed_str()), Value::from_string(style.clone()));
        set_builtin(&obj, Box::leak(format!("__df_d_{plural}").into_boxed_str()), Value::from_string(display));
        // prevStyle is updated only for hours..microseconds (nanoseconds and the calendar units do
        // not propagate).
        if matches!(*plural, "hours" | "minutes" | "seconds" | "milliseconds" | "microseconds") {
            prev_style = Some(style);
        }
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

/// Parse an ISO 8601 duration string (`±PnYnMnWnDTnHnMnS`) into the 10-field values array. A
/// fractional final time component distributes into the sub-second fields; calendar/whole units must
/// be integers. Returns `None` on any grammar violation.
fn parse_iso_duration(s: &str) -> Option<[f64; 10]> {
    let mut rest = s;
    let mut sign = 1.0;
    if let Some(r) = rest.strip_prefix('+') {
        rest = r;
    } else if let Some(r) = rest.strip_prefix('-').or_else(|| rest.strip_prefix('\u{2212}')) {
        rest = r;
        sign = -1.0;
    }
    rest = rest.strip_prefix(['P', 'p'])?;
    let (date_part, time_part) = match rest.find(['T', 't']) {
        Some(p) => (&rest[..p], Some(&rest[p + 1..])),
        None => (rest, None),
    };
    let mut vals = [0f64; 10];
    let mut any = false;
    // Consume `<number><designator>` pairs; `designators` maps a letter to its field index and
    // whether that field may carry a fraction (only the seconds field, which spills into ms/us/ns).
    let consume = |section: &str, designators: &[(char, usize)], vals: &mut [f64; 10], any: &mut bool| -> Option<()> {
        let mut buf = String::new();
        for c in section.chars() {
            if c.is_ascii_digit() {
                buf.push(c);
            } else if c == '.' || c == ',' {
                buf.push('.');
            } else {
                let (_, idx) = designators.iter().find(|(d, _)| *d == c.to_ascii_uppercase())?;
                if buf.is_empty() {
                    return None;
                }
                let frac = buf.contains('.');
                let num: f64 = buf.parse().ok()?;
                if frac {
                    // Only the seconds field accepts a fraction; distribute to ms/us/ns.
                    if *idx != 6 {
                        return None;
                    }
                    vals[6] = num.trunc();
                    let sub = ((num.fract() * 1e9).round()) as i64;
                    vals[7] = (sub / 1_000_000) as f64;
                    vals[8] = ((sub / 1000) % 1000) as f64;
                    vals[9] = (sub % 1000) as f64;
                } else {
                    vals[*idx] = num;
                }
                *any = true;
                buf.clear();
            }
        }
        // A trailing number with no designator is invalid.
        if buf.is_empty() { Some(()) } else { None }
    };
    consume(date_part, &[('Y', 0), ('M', 1), ('W', 2), ('D', 3)], &mut vals, &mut any)?;
    if let Some(tp) = time_part {
        if tp.is_empty() {
            return None; // a lone "T" with no time components
        }
        consume(tp, &[('H', 4), ('M', 5), ('S', 6)], &mut vals, &mut any)?;
    }
    if !any {
        return None;
    }
    for v in &mut vals {
        *v *= sign;
    }
    Some(vals)
}

fn valid_type(s: &str) -> bool {
    !s.is_empty() && s.split('-').all(|p| p.len() >= 3 && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// ToDurationRecord + IsValidDuration: read integer fields, require a single sign, and bound the
/// calendar units (< 2^32) and total time (< 2^53 seconds).
fn read_duration(i: &mut Interp, v: &Value) -> Result<[f64; 10], Value> {
    // A string is parsed as an ISO 8601 duration; a bad string is a RangeError, not a TypeError.
    if let Value::Str(s) = v {
        return parse_iso_duration(s).ok_or_else(|| i.make_error("RangeError", "invalid ISO 8601 duration string"));
    }
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
        // ToIntegerIfIntegral yields a mathematical value, so -0 becomes +0.
        vals[k] = n + 0.0;
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
    // IsValidDurationRecord: abs(normalizedSeconds) must be < 2^53. Evaluate exactly in i128 as a
    // total nanosecond count so the boundary (values crafted just under 2^53) is decided precisely;
    // grossly-oversized fields saturate on the `as i128` cast, which still lands past the limit.
    let ns_total: i128 = (vals[3] as i128) * 86_400_000_000_000
        + (vals[4] as i128) * 3_600_000_000_000
        + (vals[5] as i128) * 60_000_000_000
        + (vals[6] as i128) * 1_000_000_000
        + (vals[7] as i128) * 1_000_000
        + (vals[8] as i128) * 1_000
        + (vals[9] as i128);
    let limit: i128 = 9_007_199_254_740_992 * 1_000_000_000; // 2^53 seconds, in nanoseconds
    if ns_total.abs() >= limit {
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

/// One formatted piece: NumberFormat part `type`/`value`, plus the singular `unit` it belongs to
/// (`None` for a `:` time separator literal).
type DurPart = (String, String, Option<String>);

fn format_string(i: &mut Interp, this: &Value, dur: &Value) -> Result<String, Value> {
    let (groups, list_style, locale) = partition(i, this, dur)?;
    let strings: Vec<String> = groups
        .iter()
        .map(|g| g.iter().map(|(_, v, _)| v.as_str()).collect::<String>())
        .collect();
    let lf_opts = i.new_object();
    set_data(&lf_opts, "type", Value::str("unit"));
    set_data(&lf_opts, "style", Value::from_string(list_style));
    let lf = new_service(i, "ListFormat", &locale, lf_opts)?;
    let arr = i.make_array(strings.into_iter().map(Value::from_string).collect());
    let fmt = ab(i.get_member(&lf, "format"))?;
    let out = ab(i.call(fmt, lf, &[arr]))?;
    Ok(if let Value::Str(s) = out { s.to_string() } else { String::new() })
}

/// PartitionDurationFormatPattern producing the formatted list, and each group's typed parts, ready
/// to be either joined (format) or substituted into ListFormat's element parts (formatToParts).
fn partition(i: &mut Interp, this: &Value, dur: &Value) -> Result<(Vec<Vec<DurPart>>, String, String), Value> {
    let o = brand_slot(i, this, "__df")?;
    let vals = read_duration(i, dur)?;
    let locale = get_str(&o, "__df_locale");
    let base_style = get_str(&o, "__df_style");
    let numbering = get_str(&o, "__df_nu");
    let frac_digits = match o.borrow().props.get("__df_frac").map(|p| p.value.clone()) {
        Some(Value::Num(n)) => Some(n as u32),
        _ => None,
    };

    let mut groups: Vec<Vec<DurPart>> = Vec::new();
    let mut cur_group: Option<Vec<DurPart>> = None; // an in-progress numeric "hh:mm:ss" group
    let mut display_negative_sign = true;

    let mut idx = 0;
    while idx < UNITS.len() {
        let (plural, sing) = UNITS[idx];
        let mut value = vals[idx];
        let style = get_str(&o, &format!("__df_u_{plural}"));
        let display = get_str(&o, &format!("__df_d_{plural}"));
        let need_separator = cur_group.is_some();

        // Seconds/ms/us absorb the smaller sub-second units into a single fractional value when the
        // next unit is numeric (formatting then stops — the fold consumes everything smaller).
        let mut done = false;
        let mut nf_max_frac: Option<u32> = None;
        let mut nf_min_frac: Option<u32> = None;
        let mut nf_trunc = false;
        if matches!(plural, "seconds" | "milliseconds" | "microseconds") {
            let next_style = get_str(&o, &format!("__df_u_{}", UNITS[idx + 1].0));
            if next_style == "numeric" {
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
            let ftp = ab(i.get_member(&nf, "formatToParts"))?;
            let parts_arr = ab(i.call(ftp, nf.clone(), &[Value::Num(value)]))?;
            // Read the {type,value} objects into (type,value,unit=singular) parts.
            let len = ab(i.get_member(&parts_arr, "length"))?;
            let len = if let Value::Num(n) = len { n as usize } else { 0 };
            let mut parts: Vec<DurPart> = Vec::with_capacity(len);
            for k in 0..len {
                let el = ab(i.get_member(&parts_arr, &k.to_string()))?;
                let ty = match ab(i.get_member(&el, "type"))? {
                    Value::Str(s) => s.to_string(),
                    _ => String::new(),
                };
                let va = match ab(i.get_member(&el, "value"))? {
                    Value::Str(s) => s.to_string(),
                    _ => String::new(),
                };
                parts.push((ty, va, Some(sing.to_string())));
            }

            match &mut cur_group {
                Some(g) => {
                    g.push(("literal".to_string(), ":".to_string(), None));
                    g.extend(parts);
                }
                None => {
                    if style == "2-digit" || style == "numeric" {
                        cur_group = Some(parts);
                    } else {
                        groups.push(parts);
                    }
                }
            }
        }

        if done {
            break;
        }
        // A group ends when the next non-numeric unit begins; flush before a standalone unit.
        if cur_group.is_some() {
            let next_numeric = idx + 1 < UNITS.len() && {
                let ns = get_str(&o, &format!("__df_u_{}", UNITS[idx + 1].0));
                ns == "numeric" || ns == "2-digit"
            };
            if !next_numeric {
                groups.push(cur_group.take().unwrap());
            }
        }
        idx += 1;
    }
    if let Some(g) = cur_group {
        groups.push(g);
    }

    let list_style = if base_style == "digital" { "short".to_string() } else { base_style };
    Ok((groups, list_style, locale))
}

fn format_to_parts(i: &mut Interp, this: &Value, dur: &Value) -> Result<Value, Value> {
    let (mut groups, list_style, locale) = partition(i, this, dur)?;
    let strings: Vec<String> = groups
        .iter()
        .map(|g| g.iter().map(|(_, v, _)| v.as_str()).collect::<String>())
        .collect();
    // Run the group strings through ListFormat.formatToParts; each "element" part expands to that
    // group's typed sub-parts, while list "literal" parts are kept verbatim.
    let lf_opts = i.new_object();
    set_data(&lf_opts, "type", Value::str("unit"));
    set_data(&lf_opts, "style", Value::from_string(list_style));
    let lf = new_service(i, "ListFormat", &locale, lf_opts)?;
    let arr = i.make_array(strings.into_iter().map(Value::from_string).collect());
    let ftp = ab(i.get_member(&lf, "formatToParts"))?;
    let list_parts = ab(i.call(ftp, lf, &[arr]))?;
    let len = match ab(i.get_member(&list_parts, "length"))? {
        Value::Num(n) => n as usize,
        _ => 0,
    };
    let mut out: Vec<Value> = Vec::new();
    let mut giter = groups.drain(..);
    for k in 0..len {
        let el = ab(i.get_member(&list_parts, &k.to_string()))?;
        let ty = match ab(i.get_member(&el, "type"))? {
            Value::Str(s) => s.to_string(),
            _ => String::new(),
        };
        if ty == "element" {
            if let Some(group) = giter.next() {
                for (pty, pval, unit) in group {
                    let ob = i.new_object();
                    set_data(&ob, "type", Value::from_string(pty));
                    set_data(&ob, "value", Value::from_string(pval));
                    if let Some(u) = unit {
                        set_data(&ob, "unit", Value::from_string(u));
                    }
                    out.push(Value::Obj(ob));
                }
            }
        } else {
            let va = ab(i.get_member(&el, "value"))?;
            let ob = i.new_object();
            set_data(&ob, "type", Value::from_string(ty));
            set_data(&ob, "value", va);
            out.push(Value::Obj(ob));
        }
    }
    Ok(i.make_array(out))
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
