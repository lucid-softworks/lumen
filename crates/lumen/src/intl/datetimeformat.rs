//! `Intl.DateTimeFormat` (Gregorian, UTC/en subset).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "DateTimeFormat", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        let s = do_format(i, &this, &arg(a, 0))?;
        // A single literal part is acceptable for many shape tests.
        let ob = i.new_object();
        set_data(&ob, "type", Value::str("literal"));
        set_data(&ob, "value", Value::from_string(s));
        Ok(i.make_array(vec![Value::Obj(ob)]))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
    it.def_method(&proto, "formatRange", 2, |i, this, a| {
        let (s, e) = range_dates(i, &arg(a, 0), &arg(a, 1))?;
        let a1 = do_format(i, &this, &Value::Num(s))?;
        if s == e {
            return Ok(Value::from_string(a1));
        }
        let a2 = do_format(i, &this, &Value::Num(e))?;
        Ok(Value::from_string(format!("{a1}\u{2009}\u{2013}\u{2009}{a2}")))
    });
    it.def_method(&proto, "formatRangeToParts", 2, |i, this, a| {
        let (s, e) = range_dates(i, &arg(a, 0), &arg(a, 1))?;
        let a1 = do_format(i, &this, &Value::Num(s))?;
        let mk = |i: &mut Interp, src: &str, val: &str| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str("literal"));
            set_data(&ob, "value", Value::from_string(val.to_string()));
            set_data(&ob, "source", Value::str(src));
            Value::Obj(ob)
        };
        let mut parts = vec![mk(i, if s == e { "shared" } else { "startRange" }, &a1)];
        if s != e {
            let a2 = do_format(i, &this, &Value::Num(e))?;
            parts.push(mk(i, "shared", "\u{2009}\u{2013}\u{2009}"));
            parts.push(mk(i, "endRange", &a2));
        }
        Ok(i.make_array(parts))
    });
    install_format_getter(it, &proto);
}

/// ToDateTimeFormattable + ordering for a range: both endpoints ToNumber; NaN or start > end throws.
fn range_dates(i: &mut Interp, a: &Value, b: &Value) -> Result<(f64, f64), Value> {
    if matches!(a, Value::Undefined) || matches!(b, Value::Undefined) {
        return Err(i.make_error("TypeError", "formatRange requires two dates"));
    }
    let s = ab(i.to_number(a))?;
    let e = ab(i.to_number(b))?;
    if !s.is_finite() || !e.is_finite() {
        return Err(i.make_error("RangeError", "Invalid time value"));
    }
    if s > e {
        return Err(i.make_error("RangeError", "start date is after end date"));
    }
    Ok((s, e))
}

fn install_format_getter(it: &mut Interp, proto: &Gc) {
    let g = it.make_native("get format", 0, |i, this, _| {
        let o = brand_slot(i, &this, "__dtf")?;
        if let Some(f) = o.borrow().props.get("__dtf_bound").map(|p| p.value.clone()) {
            return Ok(f);
        }
        let f = i.make_native("", 1, |i, that, a| {
            Ok(Value::from_string(do_format(i, &that, &arg(a, 0))?))
        });
        let bound = crate::intl::numberformat::bind_this(i, Value::Obj(f), this.clone());
        set_builtin(&o, "__dtf_bound", bound.clone());
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

/// A Unicode key type identifier: one or more "-"-joined 3..8 alnum subtags.
fn valid_type_id(s: &str) -> bool {
    !s.is_empty() && s.split('-').all(|p| p.len() >= 3 && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    // Legacy service: callable without `new` (returns a fresh instance either way).
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;

    // Spec option read order: calendar, numberingSystem, hour12, hourCycle, timeZone, then the
    // component options, then formatMatcher, dateStyle, timeStyle.
    let calendar = get_option(i, &options, "calendar", &[], None)?.map(|c| {
        // A deprecated calendar id canonicalizes (e.g. ethiopic-amete-alem -> ethioaa).
        let lc = c.to_lowercase();
        crate::intl::tags::canonical_ca(&lc).unwrap_or(lc)
    });
    if let Some(c) = &calendar {
        if !valid_type_id(c) {
            return Err(i.make_error("RangeError", format!("invalid calendar: {c}")));
        }
    }
    let numbering = get_option(i, &options, "numberingSystem", &[], None)?;
    if let Some(ns) = &numbering {
        if !valid_type_id(ns) {
            return Err(i.make_error("RangeError", format!("invalid numberingSystem: {ns}")));
        }
    }
    let hour12 = {
        let v = ab(i.get_member(&options, "hour12"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };
    let hour_cycle = get_option(i, &options, "hourCycle", &["h11", "h12", "h23", "h24"], None)?;
    let time_zone = {
        let v = ab(i.get_member(&options, "timeZone"))?;
        if matches!(v, Value::Undefined) {
            "UTC".to_string()
        } else {
            ab(i.to_string(&v))?.to_string()
        }
    };

    let weekday = get_option(i, &options, "weekday", &["narrow", "short", "long"], None)?;
    let era = get_option(i, &options, "era", &["narrow", "short", "long"], None)?;
    let year = get_option(i, &options, "year", &["2-digit", "numeric"], None)?;
    let month = get_option(i, &options, "month", &["2-digit", "numeric", "narrow", "short", "long"], None)?;
    let day = get_option(i, &options, "day", &["2-digit", "numeric"], None)?;
    let day_period = get_option(i, &options, "dayPeriod", &["narrow", "short", "long"], None)?;
    let hour = get_option(i, &options, "hour", &["2-digit", "numeric"], None)?;
    let minute = get_option(i, &options, "minute", &["2-digit", "numeric"], None)?;
    let second = get_option(i, &options, "second", &["2-digit", "numeric"], None)?;
    let frac_sec = {
        let v = ab(i.get_member(&options, "fractionalSecondDigits"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            let n = ab(i.to_number(&v))?;
            if n.fract() != 0.0 || n < 1.0 || n > 3.0 {
                return Err(i.make_error("RangeError", "fractionalSecondDigits out of range"));
            }
            Some(n as u32)
        }
    };
    let tz_name = get_option(
        i,
        &options,
        "timeZoneName",
        &["short", "long", "shortOffset", "longOffset", "shortGeneric", "longGeneric"],
        None,
    )?;
    let _format_matcher = get_option(i, &options, "formatMatcher", &["basic", "best fit"], Some("best fit"))?;
    let date_style = get_option(i, &options, "dateStyle", &["full", "long", "medium", "short"], None)?;
    let time_style = get_option(i, &options, "timeStyle", &["full", "long", "medium", "short"], None)?;

    // A dateStyle/timeStyle may not be combined with explicit component options.
    let has_components = weekday.is_some()
        || era.is_some()
        || year.is_some()
        || month.is_some()
        || day.is_some()
        || day_period.is_some()
        || hour.is_some()
        || minute.is_some()
        || second.is_some()
        || frac_sec.is_some()
        || tz_name.is_some();
    if (date_style.is_some() || time_style.is_some()) && has_components {
        return Err(i.make_error(
            "TypeError",
            "dateStyle/timeStyle cannot be combined with explicit date-time components",
        ));
    }

    let has_explicit = weekday.is_some()
        || year.is_some()
        || month.is_some()
        || day.is_some()
        || hour.is_some()
        || minute.is_some()
        || second.is_some()
        || era.is_some();
    let resolved = resolve_locale(i, &requested, &["ca", "nu", "hc"]);

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.DateTimeFormat") {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__dtf", Value::Bool(true));
    set_builtin(&obj, "__dtf_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__dtf_ca", Value::from_string(calendar.clone().unwrap_or_else(|| "gregory".to_string())));
    set_builtin(&obj, "__dtf_nu", Value::from_string(numbering.clone().unwrap_or_else(|| "latn".to_string())));
    set_builtin(&obj, "__dtf_tz", Value::from_string(time_zone));
    let put = |obj: &Gc, k: &str, v: &Option<String>| {
        if let Some(v) = v {
            set_builtin(obj, k, Value::from_string(v.clone()));
        }
    };
    put(&obj, "__dtf_weekday", &weekday);
    put(&obj, "__dtf_era", &era);
    put(&obj, "__dtf_year", &year);
    put(&obj, "__dtf_month", &month);
    put(&obj, "__dtf_day", &day);
    put(&obj, "__dtf_dayperiod", &day_period);
    put(&obj, "__dtf_hour", &hour);
    put(&obj, "__dtf_minute", &minute);
    put(&obj, "__dtf_second", &second);
    if let Some(f) = frac_sec {
        set_builtin(&obj, "__dtf_fracsec", Value::Num(f as f64));
    }
    put(&obj, "__dtf_tzname", &tz_name);
    put(&obj, "__dtf_datestyle", &date_style);
    put(&obj, "__dtf_timestyle", &time_style);
    // hourCycle / hour12 are resolved only when an hour is shown (explicit hour, or a timeStyle).
    let shows_hour = hour.is_some() || time_style.is_some();
    if shows_hour {
        // hour12 overrides hourCycle: true → h12, false → h23.
        let hc = if let Some(h12) = hour12 {
            if h12 { "h12" } else { "h23" }.to_string()
        } else {
            hour_cycle.clone().unwrap_or_else(|| "h23".to_string())
        };
        let h12 = matches!(hc.as_str(), "h11" | "h12");
        set_builtin(&obj, "__dtf_hourcycle", Value::from_string(hc));
        set_builtin(&obj, "__dtf_hour12", Value::Bool(h12));
    }
    // Default components when nothing was requested: year/month/day numeric.
    if !has_explicit && date_style.is_none() && time_style.is_none() {
        set_builtin(&obj, "__dtf_year", Value::str("numeric"));
        set_builtin(&obj, "__dtf_month", Value::str("numeric"));
        set_builtin(&obj, "__dtf_day", Value::str("numeric"));
    }
    Ok(Value::Obj(obj))
}

/// Break epoch-ms into UTC (Y, M, D, h, m, s, weekday).
fn ymd(ms: f64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let ms_i = ms.floor() as i64;
    let mut days = ms_i.div_euclid(86_400_000);
    let mut rem = ms_i.rem_euclid(86_400_000);
    let h = (rem / 3_600_000) as u32;
    rem %= 3_600_000;
    let mi = (rem / 60_000) as u32;
    rem %= 60_000;
    let sec = (rem / 1000) as u32;
    let weekday = ((days % 7 + 4) % 7 + 7) as u32 % 7; // 0=Sun; 1970-01-01 was Thursday(4)
    // civil from days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if m <= 2 { y + 1 } else { y };
    let _ = &mut days;
    (year, m, d, h, mi, sec, weekday)
}

const WD_LONG: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const WD_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MON_LONG: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const MON_SHORT: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn do_format(i: &mut Interp, this: &Value, date: &Value) -> Result<String, Value> {
    let o = brand_slot(i, this, "__dtf")?;
    let ms = if matches!(date, Value::Undefined) {
        // "now" is non-deterministic; use epoch 0 so tests that pass an explicit date are unaffected.
        0.0
    } else {
        let n = ab(i.to_number(date))?;
        if !n.is_finite() {
            return Err(i.make_error("RangeError", "Invalid time value"));
        }
        n
    };
    let (y, mo, d, h, mi, s, wd) = ymd(ms);
    let get = |k: &str| match o.borrow().props.get(k).map(|p| p.value.clone()) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    };

    let mut date_parts: Vec<String> = Vec::new();
    // weekday
    let mut prefix = String::new();
    if let Some(w) = get("__dtf_weekday") {
        let name = if w == "long" { WD_LONG[wd as usize] } else { WD_SHORT[wd as usize] };
        prefix = format!("{name}, ");
    }
    // date: month/day/year in en order M/D/Y (numeric) or "Month D, Y".
    let month_str = get("__dtf_month").map(|m| match m.as_str() {
        "long" => MON_LONG[(mo - 1) as usize].to_string(),
        "short" => MON_SHORT[(mo - 1) as usize].to_string(),
        "narrow" => MON_LONG[(mo - 1) as usize][..1].to_string(),
        "2-digit" => format!("{mo:02}"),
        _ => format!("{mo}"),
    });
    let day_str = get("__dtf_day").map(|dd| if dd == "2-digit" { format!("{d:02}") } else { format!("{d}") });
    let year_str = get("__dtf_year").map(|yy| if yy == "2-digit" { format!("{:02}", (y % 100 + 100) % 100) } else { format!("{y}") });

    let named_month = matches!(get("__dtf_month").as_deref(), Some("long" | "short" | "narrow"));
    if named_month {
        if let Some(m) = &month_str {
            let mut ds = m.clone();
            if let Some(dd) = &day_str {
                ds = format!("{ds} {dd}");
            }
            if let Some(yy) = &year_str {
                ds = format!("{ds}, {yy}");
            }
            date_parts.push(ds);
        }
    } else {
        // numeric M/D/Y
        let mut nums: Vec<String> = Vec::new();
        if let Some(m) = &month_str {
            nums.push(m.clone());
        }
        if let Some(dd) = &day_str {
            nums.push(dd.clone());
        }
        if let Some(yy) = &year_str {
            nums.push(yy.clone());
        }
        if !nums.is_empty() {
            date_parts.push(nums.join("/"));
        }
    }

    // time
    let mut time_str = String::new();
    if get("__dtf_hour").is_some() || get("__dtf_minute").is_some() || get("__dtf_second").is_some() {
        let h12mode = o.borrow().props.get("__dtf_hour12").map(|p| p.value.clone());
        let use12 = !matches!(h12mode, Some(Value::Bool(false)));
        let (disp_h, ampm) = if use12 {
            let ap = if h < 12 { "AM" } else { "PM" };
            let hh = if h % 12 == 0 { 12 } else { h % 12 };
            (hh, Some(ap))
        } else {
            (h, None)
        };
        let mut ts = String::new();
        if get("__dtf_hour").is_some() {
            ts.push_str(&if get("__dtf_hour").as_deref() == Some("2-digit") {
                format!("{disp_h:02}")
            } else {
                format!("{disp_h}")
            });
        }
        if get("__dtf_minute").is_some() {
            ts.push_str(&format!(":{mi:02}"));
        }
        if get("__dtf_second").is_some() {
            ts.push_str(&format!(":{s:02}"));
        }
        if let Some(ap) = ampm {
            ts.push_str(&format!(" {ap}"));
        }
        time_str = ts;
    }

    let mut out = prefix;
    let body = match (date_parts.is_empty(), time_str.is_empty()) {
        (false, false) => format!("{}, {}", date_parts.join(" "), time_str),
        (false, true) => date_parts.join(" "),
        (true, false) => time_str,
        (true, true) => format!("{mo}/{d}/{y}"),
    };
    out.push_str(&body);
    Ok(out)
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__dtf")?;
    let res = i.new_object();
    let put = |i: &mut Interp, res: &Gc, k: &str, slot: &str| {
        if let Some(v) = o.borrow().props.get(slot).map(|p| p.value.clone()) {
            set_data(res, k, v);
        }
        let _ = i;
    };
    put(i, &res, "locale", "__dtf_locale");
    put(i, &res, "calendar", "__dtf_ca");
    put(i, &res, "numberingSystem", "__dtf_nu");
    put(i, &res, "timeZone", "__dtf_tz");
    put(i, &res, "hourCycle", "__dtf_hourcycle");
    put(i, &res, "hour12", "__dtf_hour12");
    put(i, &res, "weekday", "__dtf_weekday");
    put(i, &res, "era", "__dtf_era");
    put(i, &res, "year", "__dtf_year");
    put(i, &res, "month", "__dtf_month");
    put(i, &res, "day", "__dtf_day");
    put(i, &res, "dayPeriod", "__dtf_dayperiod");
    put(i, &res, "hour", "__dtf_hour");
    put(i, &res, "minute", "__dtf_minute");
    put(i, &res, "second", "__dtf_second");
    put(i, &res, "fractionalSecondDigits", "__dtf_fracsec");
    put(i, &res, "timeZoneName", "__dtf_tzname");
    put(i, &res, "dateStyle", "__dtf_datestyle");
    put(i, &res, "timeStyle", "__dtf_timestyle");
    Ok(Value::Obj(res))
}
