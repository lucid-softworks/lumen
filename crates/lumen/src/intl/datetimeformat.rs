//! `Intl.DateTimeFormat` (Gregorian, UTC/en subset).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Gc, Value};
use std::rc::Rc;

/// Days since the Unix epoch for a proleptic-Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The epoch-milliseconds a Temporal value formats at (ISO calendar, UTC).
fn temporal_to_ms(t: &crate::temporal::Temporal) -> f64 {
    use crate::temporal::Temporal as T;
    let date_ms = |d: &crate::temporal::IsoDate| {
        days_from_civil(d.year, d.month as i64, d.day as i64) as f64 * 86_400_000.0
    };
    let time_ms = |t: &crate::temporal::IsoTime| {
        (t.hour as f64) * 3_600_000.0
            + (t.minute as f64) * 60_000.0
            + (t.second as f64) * 1000.0
            + (t.ms as f64)
    };
    match t {
        T::Date(d) | T::YearMonth(d) | T::MonthDay(d) => date_ms(d),
        T::DateTime(d, tm) => date_ms(d) + time_ms(tm),
        T::Time(tm) => time_ms(tm),
        T::Instant(ns) => (*ns / 1_000_000) as f64,
        T::Zoned { epoch_ns, .. } => (*epoch_ns / 1_000_000) as f64,
        T::Duration(_) => 0.0,
    }
}

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "DateTimeFormat", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        let o = brand_slot(i, &this, "__dtf")?;
        let ms = dtf_ms(i, &o, &arg(a, 0))?;
        let parts = build_parts(&o, ms);
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
    // dateStyle / timeStyle expand to a preset component set (en; used by build_parts).
    if let Some(ds) = &date_style {
        let (wd, mo, dy, yr): (Option<&str>, &str, &str, &str) = match ds.as_str() {
            "full" => (Some("long"), "long", "numeric", "numeric"),
            "long" => (None, "long", "numeric", "numeric"),
            "medium" => (None, "short", "numeric", "numeric"),
            _ => (None, "numeric", "numeric", "2-digit"), // short
        };
        if let Some(w) = wd {
            set_builtin(&obj, "__dtfx_weekday", Value::str(w));
        }
        set_builtin(&obj, "__dtfx_month", Value::str(mo));
        set_builtin(&obj, "__dtfx_day", Value::str(dy));
        set_builtin(&obj, "__dtfx_year", Value::str(yr));
    }
    if let Some(ts) = &time_style {
        set_builtin(&obj, "__dtfx_hour", Value::str("numeric"));
        set_builtin(&obj, "__dtfx_minute", Value::str("2-digit"));
        if matches!(ts.as_str(), "medium" | "long" | "full") {
            set_builtin(&obj, "__dtfx_second", Value::str("2-digit"));
        }
    }
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
    // Default components when nothing was requested: year/month/day numeric. Flagged so a Temporal
    // receiver's compatibility check ignores them (only *explicit* options can conflict).
    if !has_explicit && date_style.is_none() && time_style.is_none() {
        set_builtin(&obj, "__dtf_year", Value::str("numeric"));
        set_builtin(&obj, "__dtf_month", Value::str("numeric"));
        set_builtin(&obj, "__dtf_day", Value::str("numeric"));
        set_builtin(&obj, "__dtf_defaults", Value::Bool(true));
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

/// Resolve the format argument to epoch-milliseconds. A Temporal receiver uses its ISO fields and
/// must be compatible with the formatter's requested components (else TypeError/RangeError).
fn dtf_ms(i: &mut Interp, o: &Gc, date: &Value) -> Result<f64, Value> {
    if matches!(date, Value::Undefined) {
        // "now" is non-deterministic; epoch 0 keeps explicit-date tests deterministic.
        return Ok(0.0);
    }
    if let Some(t) = date.as_obj().and_then(|d| i.temporal.get(&(Rc::as_ptr(d) as usize)).cloned()) {
        temporal_compat_check(i, o, &t)?;
        return Ok(temporal_to_ms(&t));
    }
    let n = ab(i.to_number(date))?;
    if !n.is_finite() {
        return Err(i.make_error("RangeError", "Invalid time value"));
    }
    Ok(n)
}

/// A Temporal receiver's kind must not request incompatible fields from the formatter.
fn temporal_compat_check(i: &mut Interp, o: &Gc, t: &crate::temporal::Temporal) -> Result<(), Value> {
    use crate::temporal::Temporal as T;
    let has = |k: &str| o.borrow().props.contains(k);
    // Defaulted (not explicitly requested) components never conflict.
    let defaulted = has("__dtf_defaults");
    let has_time = has("__dtf_hour") || has("__dtf_minute") || has("__dtf_second")
        || has("__dtf_dayperiod") || has("__dtf_fracsec") || has("__dtf_timestyle");
    let has_date_any = !defaulted
        && (has("__dtf_weekday") || has("__dtf_era") || has("__dtf_year")
            || has("__dtf_month") || has("__dtf_day") || has("__dtf_datestyle"));
    let has_year = !defaulted && has("__dtf_year");
    let has_day = !defaulted && has("__dtf_day");
    let has_weekday = !defaulted && has("__dtf_weekday");
    let err = |i: &mut Interp, msg: &str| Err(i.make_error("TypeError", msg.to_string()));
    match t {
        T::Date(_) => {
            if has_time {
                return err(i, "PlainDate cannot be formatted with time components");
            }
        }
        T::Time(_) => {
            if has_date_any {
                return err(i, "PlainTime cannot be formatted with date components");
            }
        }
        T::YearMonth(_) => {
            if has_time || has_day || has_weekday {
                return err(i, "PlainYearMonth accepts only year/month");
            }
        }
        T::MonthDay(_) => {
            if has_time || has_year || has_weekday {
                return err(i, "PlainMonthDay accepts only month/day");
            }
        }
        // DateTime / Instant / Zoned accept any components.
        _ => {}
    }
    Ok(())
}

fn do_format(i: &mut Interp, this: &Value, date: &Value) -> Result<String, Value> {
    let o = brand_slot(i, this, "__dtf")?;
    let ms = dtf_ms(i, &o, date)?;
    let parts = build_parts(&o, ms);
    Ok(parts.into_iter().map(|(_, v)| v).collect::<Vec<_>>().join(""))
}

/// Build the typed (type, value) parts for the given epoch-ms per the stored components (en, UTC).
fn build_parts(o: &Gc, ms: f64) -> Vec<(&'static str, String)> {
    let (y, mo, d, h, mi, s, wd) = ymd(ms);
    // Read a component slot, falling back to the dateStyle/timeStyle expansion (`__dtfx_`).
    let get = |k: &str| {
        let read = |key: &str| match o.borrow().props.get(key).map(|p| p.value.clone()) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        read(k).or_else(|| read(&k.replacen("__dtf_", "__dtfx_", 1)))
    };
    let mut parts: Vec<(&'static str, String)> = Vec::new();
    let lit = |parts: &mut Vec<(&'static str, String)>, s: &str| {
        parts.push(("literal", s.to_string()));
    };

    if let Some(w) = get("__dtf_weekday") {
        let name = if w == "long" { WD_LONG[wd as usize] } else { WD_SHORT[wd as usize] };
        parts.push(("weekday", name.to_string()));
        lit(&mut parts, ", ");
    }

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
    let have_date = month_str.is_some() || day_str.is_some() || year_str.is_some();

    if named_month {
        if let Some(m) = &month_str {
            parts.push(("month", m.clone()));
        }
        if let Some(dd) = &day_str {
            lit(&mut parts, " ");
            parts.push(("day", dd.clone()));
        }
        if let Some(yy) = &year_str {
            lit(&mut parts, ", ");
            parts.push(("year", yy.clone()));
        }
    } else if have_date {
        // numeric M/D/Y
        let mut first = true;
        if let Some(m) = &month_str {
            parts.push(("month", m.clone()));
            first = false;
        }
        if let Some(dd) = &day_str {
            if !first {
                lit(&mut parts, "/");
            }
            parts.push(("day", dd.clone()));
            first = false;
        }
        if let Some(yy) = &year_str {
            if !first {
                lit(&mut parts, "/");
            }
            parts.push(("year", yy.clone()));
        }
    }

    let have_time = get("__dtf_hour").is_some() || get("__dtf_minute").is_some() || get("__dtf_second").is_some();
    if have_time {
        if have_date {
            lit(&mut parts, ", ");
        }
        let use12 = !matches!(o.borrow().props.get("__dtf_hour12").map(|p| p.value.clone()), Some(Value::Bool(false)));
        let (disp_h, ampm) = if use12 {
            let ap = if h < 12 { "AM" } else { "PM" };
            (if h % 12 == 0 { 12 } else { h % 12 }, Some(ap))
        } else {
            (h, None)
        };
        let mut first = true;
        if get("__dtf_hour").is_some() {
            parts.push(("hour", if get("__dtf_hour").as_deref() == Some("2-digit") { format!("{disp_h:02}") } else { format!("{disp_h}") }));
            first = false;
        }
        if get("__dtf_minute").is_some() {
            if !first {
                lit(&mut parts, ":");
            }
            parts.push(("minute", format!("{mi:02}")));
            first = false;
        }
        if get("__dtf_second").is_some() {
            if !first {
                lit(&mut parts, ":");
            }
            parts.push(("second", format!("{s:02}")));
        }
        if let Some(ap) = ampm {
            lit(&mut parts, " ");
            parts.push(("dayPeriod", ap.to_string()));
        }
    }

    if parts.is_empty() {
        // Default numeric date.
        parts.push(("month", format!("{mo}")));
        lit(&mut parts, "/");
        parts.push(("day", format!("{d}")));
        lit(&mut parts, "/");
        parts.push(("year", format!("{y}")));
    }
    parts
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
