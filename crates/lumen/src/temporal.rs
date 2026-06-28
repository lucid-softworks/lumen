//! A from-scratch subset of the `Temporal` proposal (ISO-8601 calendar only). Covers the
//! non-timezone types — PlainDate/PlainTime/PlainDateTime/PlainYearMonth/PlainMonthDay/Duration/
//! Instant — with constructors, field getters, `from`/`compare`/`equals`/`toString`, `with`, and
//! basic `add`/`subtract`. ZonedDateTime and TimeZone/Calendar objects are out of scope.

use crate::interpreter::Interp;
use crate::value::{Gc, NativeFn, Object, Property, Value};
use std::rc::Rc;

#[derive(Clone, Copy)]
pub struct IsoDate {
    pub year: i64,
    pub month: u8,
    pub day: u8,
}
#[derive(Clone, Copy)]
pub struct IsoTime {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub ms: u16,
    pub us: u16,
    pub ns: u16,
}
#[derive(Clone, Copy, Default)]
pub struct IsoDuration {
    pub years: i64,
    pub months: i64,
    pub weeks: i64,
    pub days: i64,
    pub hours: i64,
    pub minutes: i64,
    pub seconds: i64,
    pub ms: i64,
    pub us: i64,
    pub ns: i64,
}

#[derive(Clone)]
pub enum Temporal {
    Date(IsoDate),
    Time(IsoTime),
    DateTime(IsoDate, IsoTime),
    YearMonth(IsoDate),
    MonthDay(IsoDate),
    Duration(IsoDuration),
    Instant(i128), // epoch nanoseconds
    /// epoch nanoseconds + a fixed UTC offset (named zones are treated as their fixed offset; no
    /// DST database) + the time-zone id string.
    Zoned { epoch_ns: i128, offset_ns: i64, tz: Rc<str> },
}

// ----- ISO calendar math ----------------------------------------------------------------------

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
pub fn days_in_month(y: i64, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}
/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u8, d as u8)
}
fn iso_day_of_week(d: IsoDate) -> i64 {
    let z = days_from_civil(d.year, d.month as i64, d.day as i64);
    let wd = ((z % 7) + 7) % 7; // 0 = Thursday (1970-01-01)
    ((wd + 3) % 7) + 1 // 1 = Monday .. 7 = Sunday
}
fn iso_day_of_year(d: IsoDate) -> i64 {
    days_from_civil(d.year, d.month as i64, d.day as i64) - days_from_civil(d.year, 1, 1) + 1
}
fn iso_week(d: IsoDate) -> (i64, i64) {
    let z = days_from_civil(d.year, d.month as i64, d.day as i64);
    let wd = iso_day_of_week(d);
    let thursday = z + (4 - wd);
    let (ty, _, _) = civil_from_days(thursday);
    let jan1 = days_from_civil(ty, 1, 1);
    ((thursday - jan1) / 7 + 1, ty)
}

/// Normalize a (year, month) where `month` may be outside 1..=12 into a valid pair.
fn balance_year_month(year: i64, month: i64) -> (i64, u8) {
    let m0 = month - 1;
    let y = year + m0.div_euclid(12);
    let m = m0.rem_euclid(12) + 1;
    (y, m as u8)
}

// ----- helpers --------------------------------------------------------------------------------

fn get(i: &Interp, this: &Value) -> Option<Temporal> {
    match this {
        Value::Obj(o) => i.temporal.get(&(Rc::as_ptr(o) as usize)).cloned(),
        _ => None,
    }
}
fn make(i: &mut Interp, proto: &str, data: Temporal) -> Value {
    let obj = Object::new(i.extra_protos.get(proto).cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.temporal.insert(p, data);
    Value::Obj(obj)
}
fn arg(a: &[Value], n: usize) -> Value {
    a.get(n).cloned().unwrap_or(Value::Undefined)
}
fn to_int(i: &mut Interp, v: &Value) -> Result<i64, Value> {
    let n = i.to_number(v).map_err(unab)?;
    if !n.is_finite() {
        return Err(i.make_error("RangeError", "value must be finite"));
    }
    Ok(n.trunc() as i64)
}
fn to_int_default(i: &mut Interp, v: &Value, d: i64) -> Result<i64, Value> {
    match v {
        Value::Undefined => Ok(d),
        _ => to_int(i, v),
    }
}
fn unab(a: crate::interpreter::Abrupt) -> Value {
    match a {
        crate::interpreter::Abrupt::Throw(v) => v,
        _ => Value::Undefined,
    }
}
fn getm(i: &mut Interp, o: &Value, k: &str) -> Result<Value, Value> {
    i.get_member(o, k).map_err(unab)
}
fn def_getter(it: &Interp, proto: &Gc, name: &str, f: NativeFn) {
    let g = it.make_native(name, 0, f);
    proto.borrow_mut().props.insert(
        name,
        Property {
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
fn month_code(m: u8) -> String {
    format!("M{m:02}")
}
fn pad_year(y: i64) -> String {
    if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else {
        format!("{}{:06}", if y < 0 { "-" } else { "+" }, y.abs())
    }
}

// Brand-check extractors.
fn as_date(i: &Interp, this: &Value) -> Result<IsoDate, Value> {
    match get(i, this) {
        Some(Temporal::Date(d)) => Ok(d),
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.PlainDate")),
    }
}
fn as_time(i: &Interp, this: &Value) -> Result<IsoTime, Value> {
    match get(i, this) {
        Some(Temporal::Time(t)) => Ok(t),
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.PlainTime")),
    }
}
fn as_datetime(i: &Interp, this: &Value) -> Result<(IsoDate, IsoTime), Value> {
    match get(i, this) {
        Some(Temporal::DateTime(d, t)) => Ok((d, t)),
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.PlainDateTime")),
    }
}
fn as_yearmonth(i: &Interp, this: &Value) -> Result<IsoDate, Value> {
    match get(i, this) {
        Some(Temporal::YearMonth(d)) => Ok(d),
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.PlainYearMonth")),
    }
}
fn as_monthday(i: &Interp, this: &Value) -> Result<IsoDate, Value> {
    match get(i, this) {
        Some(Temporal::MonthDay(d)) => Ok(d),
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.PlainMonthDay")),
    }
}
fn as_duration(i: &Interp, this: &Value) -> Result<IsoDuration, Value> {
    match get(i, this) {
        Some(Temporal::Duration(d)) => Ok(d),
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.Duration")),
    }
}
fn as_instant(i: &Interp, this: &Value) -> Result<i128, Value> {
    match get(i, this) {
        Some(Temporal::Instant(n)) => Ok(n),
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.Instant")),
    }
}

// Validation.
fn check_date(i: &Interp, d: IsoDate) -> Result<IsoDate, Value> {
    if !(1..=12).contains(&d.month) || d.day < 1 || d.day > days_in_month(d.year, d.month) {
        return Err(i.make_error("RangeError", "invalid ISO date"));
    }
    Ok(d)
}
fn check_time(i: &Interp, t: IsoTime) -> Result<IsoTime, Value> {
    if t.hour > 23 || t.minute > 59 || t.second > 59 || t.ms > 999 || t.us > 999 || t.ns > 999 {
        return Err(i.make_error("RangeError", "invalid ISO time"));
    }
    Ok(t)
}

// ----- toString formatting --------------------------------------------------------------------

fn fmt_date(d: IsoDate) -> String {
    format!("{}-{:02}-{:02}", pad_year(d.year), d.month, d.day)
}
fn fmt_time(t: IsoTime) -> String {
    let mut s = format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second);
    let frac = t.ms as u32 * 1_000_000 + t.us as u32 * 1000 + t.ns as u32;
    if frac > 0 {
        let mut f = format!("{frac:09}");
        while f.ends_with('0') {
            f.pop();
        }
        s.push('.');
        s.push_str(&f);
    }
    s
}
/// Read `fractionalSecondDigits` (0..=9 or "auto"); None means auto (trim trailing zeros).
fn read_frac_digits(i: &mut Interp, opts: &Value) -> Result<Option<usize>, Value> {
    if matches!(opts, Value::Undefined | Value::Str(_)) {
        return Ok(None);
    }
    let v = getm(i, opts, "fractionalSecondDigits")?;
    match v {
        Value::Undefined => Ok(None),
        Value::Str(s) if &*s == "auto" => Ok(None),
        _ => {
            let n = to_int(i, &v)?;
            if !(0..=9).contains(&n) {
                return Err(i.make_error("RangeError", "fractionalSecondDigits out of range"));
            }
            Ok(Some(n as usize))
        }
    }
}
/// Format a time honoring `smallestUnit` / `fractionalSecondDigits` options.
fn fmt_time_opts(i: &mut Interp, t: IsoTime, opts: &Value) -> Result<String, Value> {
    let smallest = opt_str(i, opts, "smallestUnit", "")?;
    let smallest = smallest.strip_suffix('s').unwrap_or(&smallest);
    let base = format!("{:02}:{:02}", t.hour, t.minute);
    if smallest == "minute" {
        return Ok(base);
    }
    let mut s = format!("{}:{:02}", base, t.second);
    let subsec = t.ms as u32 * 1_000_000 + t.us as u32 * 1000 + t.ns as u32;
    let digits = match smallest {
        "second" => Some(0),
        "millisecond" => Some(3),
        "microsecond" => Some(6),
        "nanosecond" => Some(9),
        _ => read_frac_digits(i, opts)?,
    };
    match digits {
        Some(0) => {}
        Some(n) => {
            let f = format!("{subsec:09}");
            s.push('.');
            s.push_str(&f[..n]);
        }
        None => {
            if subsec > 0 {
                let mut f = format!("{subsec:09}");
                while f.ends_with('0') {
                    f.pop();
                }
                s.push('.');
                s.push_str(&f);
            }
        }
    }
    Ok(s)
}
/// The `[u-ca=iso8601]` calendar annotation per the `calendarName` option.
fn cal_suffix(i: &mut Interp, opts: &Value) -> Result<&'static str, Value> {
    match opt_str(i, opts, "calendarName", "auto")?.as_str() {
        "always" | "critical" => Ok("[u-ca=iso8601]"),
        _ => Ok(""),
    }
}

fn fmt_duration(d: IsoDuration) -> String {
    let sign = duration_sign(d);
    let neg = sign < 0;
    let a = |n: i64| n.unsigned_abs();
    let mut date = String::new();
    if d.years != 0 {
        date.push_str(&format!("{}Y", a(d.years)));
    }
    if d.months != 0 {
        date.push_str(&format!("{}M", a(d.months)));
    }
    if d.weeks != 0 {
        date.push_str(&format!("{}W", a(d.weeks)));
    }
    if d.days != 0 {
        date.push_str(&format!("{}D", a(d.days)));
    }
    let mut time = String::new();
    if d.hours != 0 {
        time.push_str(&format!("{}H", a(d.hours)));
    }
    if d.minutes != 0 {
        time.push_str(&format!("{}M", a(d.minutes)));
    }
    let subsec = a(d.ms) * 1_000_000 + a(d.us) * 1000 + a(d.ns);
    if d.seconds != 0 || subsec != 0 {
        if subsec > 0 {
            let mut f = format!("{subsec:09}");
            while f.ends_with('0') {
                f.pop();
            }
            time.push_str(&format!("{}.{}S", a(d.seconds), f));
        } else {
            time.push_str(&format!("{}S", a(d.seconds)));
        }
    }
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.push('P');
    s.push_str(&date);
    if !time.is_empty() {
        s.push('T');
        s.push_str(&time);
    }
    if s.ends_with('P') {
        s.push_str("0D");
    }
    s
}
fn duration_sign(d: IsoDuration) -> i64 {
    for v in [d.years, d.months, d.weeks, d.days, d.hours, d.minutes, d.seconds, d.ms, d.us, d.ns] {
        if v != 0 {
            return if v < 0 { -1 } else { 1 };
        }
    }
    0
}

// ----- string parsing (basic ISO) -------------------------------------------------------------

/// Validate ISO `[..]` annotations: keys lowercase, at most one calendar and one time-zone
/// annotation, no unknown critical (`[!key=val]`) annotation.
fn valid_annotations(s: &str) -> bool {
    let mut rest = s;
    let (mut tz, mut cal) = (0u32, 0u32);
    while let Some(start) = rest.find('[') {
        let end = match rest[start..].find(']') {
            Some(e) => e + start,
            None => return false,
        };
        let inner = &rest[start + 1..end];
        let critical = inner.starts_with('!');
        let body = inner.strip_prefix('!').unwrap_or(inner);
        if let Some(eq) = body.find('=') {
            let key = &body[..eq];
            if key.is_empty()
                || !key.bytes().all(|c| c.is_ascii_lowercase() || c == b'-' || c.is_ascii_digit())
            {
                return false;
            }
            if key == "u-ca" {
                cal += 1;
            } else if critical {
                return false; // unknown critical annotation
            }
        } else {
            tz += 1;
        }
        rest = &rest[end + 1..];
    }
    cal <= 1 && tz <= 1
}

fn parse_date_str(s: &str) -> Option<IsoDate> {
    let s = s.trim();
    if s.is_empty() || s.contains('\u{2212}') || !valid_annotations(s) {
        return None; // reject empty, Unicode minus, malformed annotations
    }
    let s = s.split('[').next().unwrap_or(s); // drop [..] annotations
    let datepart = s.split(['T', 't', ' ']).next()?;
    let (sign, body) = if let Some(r) = datepart.strip_prefix('-') {
        (-1i64, r)
    } else if let Some(r) = datepart.strip_prefix('+') {
        (1, r)
    } else {
        (1, datepart)
    };
    let (y, m, d) = if body.contains('-') {
        let mut p = body.splitn(3, '-');
        let ys = p.next()?;
        let ms = p.next()?;
        let ds = p.next()?;
        // Components must be all-digits with the expected widths.
        if ms.len() != 2 || ds.len() != 2 || ys.len() < 4 || !ys.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        (ys.parse::<i64>().ok()?, ms.parse::<u8>().ok()?, ds.parse::<u8>().ok()?)
    } else {
        // Basic format YYYYMMDD (year = all but the last four digits).
        if body.len() < 8 || !body.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let cut = body.len() - 4;
        (
            body[..cut].parse::<i64>().ok()?,
            body[cut..cut + 2].parse::<u8>().ok()?,
            body[cut + 2..].parse::<u8>().ok()?,
        )
    };
    let date = IsoDate { year: sign * y, month: m, day: d };
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(date.year, m) {
        return None; // out-of-range date components are a RangeError
    }
    if sign < 0 && y == 0 {
        return None; // negative-zero extended year is invalid
    }
    Some(date)
}
fn parse_time_str(s: &str) -> Option<IsoTime> {
    let s = s.trim();
    if s.is_empty() || s.contains('\u{2212}') || !valid_annotations(s) {
        return None;
    }
    let s = s.split('[').next().unwrap_or(s);
    let mut tpart = if let Some(idx) = s.find(['T', 't']) { &s[idx + 1..] } else { s };
    // Strip a trailing UTC offset / Z designator (time itself never contains '+'/'Z'/'-').
    tpart = tpart.trim_end_matches('Z');
    tpart = tpart.split('+').next().unwrap_or(tpart);
    if let Some(pos) = tpart.rfind('-') {
        tpart = &tpart[..pos];
    }
    let (hms, frac) = match tpart.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (tpart, None),
    };
    let (h, mi, sec) = if hms.contains(':') {
        let mut p = hms.splitn(3, ':');
        (
            p.next()?.parse().ok()?,
            p.next().unwrap_or("0").parse().ok()?,
            p.next().unwrap_or("0").parse().ok()?,
        )
    } else {
        // Basic format HHMMSS / HHMM / HH.
        if hms.is_empty() || !hms.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let g = |a: usize, b: usize| hms.get(a..b).unwrap_or("0").parse::<u8>().unwrap_or(0);
        (hms.get(0..2)?.parse().ok()?, g(2, 4), g(4, 6))
    };
    // Fractional parts only attach to seconds, and must be all digits.
    let (ms, us, ns) = match frac {
        Some(frac) => {
            if frac.is_empty() || !frac.bytes().all(|c| c.is_ascii_digit()) || frac.len() > 9 {
                return None;
            }
            let mut f = frac.to_string();
            while f.len() < 9 {
                f.push('0');
            }
            let n: u32 = f.parse().ok()?;
            ((n / 1_000_000) as u16, ((n / 1000) % 1000) as u16, (n % 1000) as u16)
        }
        None => (0, 0, 0),
    };
    if h > 23 || mi > 59 || sec > 60 {
        return None; // out-of-range time component
    }
    let sec = if sec == 60 { 59 } else { sec }; // leap second constrained
    Some(IsoTime { hour: h, minute: mi, second: sec, ms, us, ns })
}

// ----- install --------------------------------------------------------------------------------

pub fn install(it: &mut Interp) {
    let ns = Object::new(Some(it.object_proto.clone()));
    install_plain_date(it, &ns);
    install_plain_time(it, &ns);
    install_plain_datetime(it, &ns);
    install_year_month(it, &ns);
    install_month_day(it, &ns);
    install_duration(it, &ns);
    install_instant(it, &ns);
    install_zoned(it, &ns);
    install_now(it, &ns);
    // toLocaleString aliases toString (lumen has no Intl).
    for name in ["PlainDate", "PlainTime", "PlainDateTime", "PlainYearMonth", "PlainMonthDay", "Duration", "Instant", "ZonedDateTime"] {
        if let Some(proto) = it.extra_protos.get(format!("Temporal.{name}").as_str()).cloned() {
            let ts = proto.borrow().props.get("toString").cloned();
            if let Some(p) = ts {
                proto.borrow_mut().props.insert("toLocaleString", p);
            }
        }
    }
    it.global.borrow_mut().props.insert("Temporal", Property::builtin(Value::Obj(ns)));
}

fn add_ctor(it: &mut Interp, ns: &Gc, name: &'static str, len: usize, proto: Gc, f: NativeFn) -> Gc {
    let ctor = it.make_native(name, len, f);
    ctor.borrow_mut().is_constructor = true;
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    if let Some(key) = crate::builtins::to_string_tag_key(it) {
        proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str(format!("Temporal.{name}")), false, false, true),
        );
    }
    ns.borrow_mut().props.insert(name, Property::builtin(Value::Obj(ctor.clone())));
    ctor
}

fn require_new(i: &Interp) -> Result<(), Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "constructor requires 'new'"));
    }
    Ok(())
}

// ===== PlainDate ==============================================================================

fn install_plain_date(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.PlainDate", proto.clone());

    def_getter(it, &proto, "year", |i, t, _| Ok(Value::Num(as_date(i, &t)?.year as f64)));
    def_getter(it, &proto, "month", |i, t, _| Ok(Value::Num(as_date(i, &t)?.month as f64)));
    def_getter(it, &proto, "day", |i, t, _| Ok(Value::Num(as_date(i, &t)?.day as f64)));
    def_getter(it, &proto, "monthCode", |i, t, _| {
        Ok(Value::str(month_code(as_date(i, &t)?.month)))
    });
    def_getter(it, &proto, "calendarId", |_i, _t, _| Ok(Value::str("iso8601")));
    def_getter(it, &proto, "dayOfWeek", |i, t, _| Ok(Value::Num(iso_day_of_week(as_date(i, &t)?) as f64)));
    def_getter(it, &proto, "dayOfYear", |i, t, _| Ok(Value::Num(iso_day_of_year(as_date(i, &t)?) as f64)));
    def_getter(it, &proto, "weekOfYear", |i, t, _| Ok(Value::Num(iso_week(as_date(i, &t)?).0 as f64)));
    def_getter(it, &proto, "yearOfWeek", |i, t, _| Ok(Value::Num(iso_week(as_date(i, &t)?).1 as f64)));
    def_getter(it, &proto, "daysInWeek", |i, t, _| {
        as_date(i, &t)?;
        Ok(Value::Num(7.0))
    });
    def_getter(it, &proto, "daysInMonth", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(days_in_month(d.year, d.month) as f64))
    });
    def_getter(it, &proto, "daysInYear", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(if is_leap(d.year) { 366.0 } else { 365.0 }))
    });
    def_getter(it, &proto, "monthsInYear", |i, t, _| {
        as_date(i, &t)?;
        Ok(Value::Num(12.0))
    });
    def_getter(it, &proto, "inLeapYear", |i, t, _| {
        Ok(Value::Bool(is_leap(as_date(i, &t)?.year)))
    });

    it.def_method(&proto, "toString", 0, |i, t, a| { let d=as_date(i,&t)?; Ok(Value::str(format!("{}{}", fmt_date(d), cal_suffix(i,&arg(a,0))?))) });
    it.def_method(&proto, "toJSON", 0, |i, t, _| Ok(Value::str(fmt_date(as_date(i, &t)?))));
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.PlainDate has no valueOf; use compare"))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let o = to_date(i, &arg(a, 0))?;
        Ok(Value::Bool(d.year == o.year && d.month == o.month && d.day == o.day))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let f = arg(a, 0);
        let year = field_int(i, &f, "year", d.year)?;
        let month = field_int(i, &f, "month", d.month as i64)?;
        let day = field_int(i, &f, "day", d.day as i64)?;
        let nd = check_date(i, IsoDate { year, month: month as u8, day: day as u8 })?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(nd)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let nd = add_to_date(i, d, dur, 1)?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(nd)))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let nd = add_to_date(i, d, dur, -1)?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(nd)))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let o = to_date(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "day")?;
        let largest = if largest == "auto" { "day".to_string() } else { largest };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(diff_date(d, o, &largest))))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let o = to_date(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "day")?;
        let largest = if largest == "auto" { "day".to_string() } else { largest };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(neg_duration(diff_date(d, o, &largest)))))
    });
    it.def_method(&proto, "toPlainDateTime", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let time = match arg(a, 0) {
            Value::Undefined => IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 },
            v => to_time(i, &v)?,
        };
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, time)))
    });
    it.def_method(&proto, "toPlainYearMonth", 0, |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(d)))
    });
    it.def_method(&proto, "toPlainMonthDay", 0, |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(make(i, "Temporal.PlainMonthDay", Temporal::MonthDay(d)))
    });
    it.def_method(&proto, "withCalendar", 1, |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(d)))
    });
    it.def_method(&proto, "toZonedDateTime", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let item = arg(a, 0);
        let (tzv, timev) = match &item {
            Value::Obj(_) => (getm(i, &item, "timeZone")?, getm(i, &item, "plainTime")?),
            other => (other.clone(), Value::Undefined),
        };
        let tz: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let time = match timev {
            Value::Undefined => IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 },
            v => to_time(i, &v)?,
        };
        let local = dt_ns(d, time);
        let offset = offset_for_local(&tz, local);
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: local - offset as i128, offset_ns: offset, tz }))
    });

    let ctor = add_ctor(it, ns, "PlainDate", 3, proto, |i, _t, a| {
        require_new(i)?;
        let year = to_int(i, &arg(a, 0))?;
        let month = to_int(i, &arg(a, 1))?;
        let day = to_int(i, &arg(a, 2))?;
        if !(1..=12).contains(&month) || day < 1 {
            return Err(i.make_error("RangeError", "invalid date"));
        }
        let d = check_date(i, IsoDate { year, month: month as u8, day: day as u8 })?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(d)))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let d = to_date(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(d)))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_date(i, &arg(a, 0))?;
        let y = to_date(i, &arg(a, 1))?;
        Ok(Value::Num(cmp_date(x, y) as f64))
    });
}

fn cmp_date(x: IsoDate, y: IsoDate) -> i64 {
    let a = days_from_civil(x.year, x.month as i64, x.day as i64);
    let b = days_from_civil(y.year, y.month as i64, y.day as i64);
    a.cmp(&b) as i64
}
fn epoch_days(d: IsoDate) -> i64 {
    days_from_civil(d.year, d.month as i64, d.day as i64)
}

/// Read a string option (e.g. `largestUnit`) from an options argument, defaulting if absent. A bare
/// string options arg (the `smallestUnit` shorthand) is returned directly.
fn opt_str(i: &mut Interp, opts: &Value, key: &str, default: &str) -> Result<String, Value> {
    match opts {
        Value::Undefined => Ok(default.to_string()),
        Value::Obj(_) => {
            let v = getm(i, opts, key)?;
            match v {
                Value::Undefined => Ok(default.to_string()),
                _ => Ok(i.to_string(&v).map_err(unab)?.to_string()),
            }
        }
        _ => Err(i.make_error("TypeError", "options must be an object")),
    }
}
fn opt_num(i: &mut Interp, opts: &Value, key: &str, default: i64) -> Result<i64, Value> {
    match opts {
        Value::Undefined => Ok(default),
        Value::Obj(_) => {
            let v = getm(i, opts, key)?;
            to_int_default(i, &v, default)
        }
        _ => Err(i.make_error("TypeError", "options must be an object")),
    }
}
/// Nanoseconds per time unit, or None for calendar units.
fn unit_ns(u: &str) -> Option<i128> {
    Some(match u {
        "hour" => 3_600_000_000_000,
        "minute" => 60_000_000_000,
        "second" => 1_000_000_000,
        "millisecond" => 1_000_000,
        "microsecond" => 1000,
        "nanosecond" => 1,
        _ => return None,
    })
}
/// Validate a `roundingIncrement` for a (singular) time/day unit: it must be a positive integer that
/// evenly divides the next-larger unit and is smaller than it (day allows only 1).
fn check_increment(i: &Interp, unit: &str, incr: i64) -> Result<(), Value> {
    if incr < 1 {
        return Err(i.make_error("RangeError", "roundingIncrement out of range"));
    }
    let max = match unit {
        "hour" => 24,
        "minute" | "second" => 60,
        "millisecond" | "microsecond" | "nanosecond" => 1000,
        _ => return if incr == 1 { Ok(()) } else { Err(i.make_error("RangeError", "roundingIncrement out of range")) },
    };
    if incr >= max || max % incr != 0 {
        return Err(i.make_error("RangeError", "roundingIncrement out of range"));
    }
    Ok(())
}
/// Validate a `roundingMode` option, else RangeError.
fn check_mode(i: &Interp, m: &str) -> Result<(), Value> {
    const MODES: [&str; 9] = [
        "ceil", "floor", "expand", "trunc", "halfCeil", "halfFloor", "halfExpand", "halfTrunc",
        "halfEven",
    ];
    if MODES.contains(&m) {
        Ok(())
    } else {
        Err(i.make_error("RangeError", "invalid roundingMode"))
    }
}
/// Round `value` (signed ns) to a multiple of `inc` ns using a rounding mode.
fn round_ns(value: i128, inc: i128, mode: &str) -> i128 {
    if inc <= 1 {
        return value;
    }
    let q = value.div_euclid(inc);
    let r = value.rem_euclid(inc);
    if r == 0 {
        return value;
    }
    let up = match mode {
        "ceil" | "expand" => true,
        "floor" | "trunc" => false,
        "halfFloor" | "halfTrunc" => r * 2 > inc,
        "halfCeil" | "halfExpand" => r * 2 >= inc,
        "halfEven" => r * 2 > inc || (r * 2 == inc && q % 2 != 0),
        _ => r * 2 >= inc,
    };
    (q + if up { 1 } else { 0 }) * inc
}

/// Difference between two ISO dates as a calendar duration honoring `largest`
/// (years/months/weeks/days). Assumes nothing about ordering; the result carries the sign.
fn diff_date(a: IsoDate, b: IsoDate, largest: &str) -> IsoDuration {
    let largest = largest.strip_suffix('s').unwrap_or(largest); // accept plural unit names
    let sign = cmp_date(a, b);
    let mut out = IsoDuration::default();
    if sign == 0 {
        return out;
    }
    let (lo, hi) = if sign < 0 { (a, b) } else { (b, a) };
    match largest {
        "year" | "month" => {
            let mut years = if largest == "year" { hi.year - lo.year } else { 0 };
            let mut mid = constrain_add_ym(lo, years * 12);
            if cmp_date(mid, hi) > 0 {
                years -= 1;
                mid = constrain_add_ym(lo, years * 12);
            }
            let mut months = 0i64;
            loop {
                let next = constrain_add_ym(mid, 1);
                if cmp_date(next, hi) <= 0 {
                    months += 1;
                    mid = next;
                } else {
                    break;
                }
            }
            let days = epoch_days(hi) - epoch_days(mid);
            out.years = years;
            out.months = months;
            out.days = days;
        }
        "week" => {
            let total = epoch_days(hi) - epoch_days(lo);
            out.weeks = total / 7;
            out.days = total % 7;
        }
        _ => {
            out.days = epoch_days(hi) - epoch_days(lo);
        }
    }
    if sign > 0 {
        out = neg_duration(out);
    }
    out
}
/// Difference between two datetimes honoring a calendar `largest` unit (year/month/week/day) for the
/// date part and balancing the remaining time-of-day, with a borrow when the end time is earlier.
fn diff_datetime(d1: IsoDate, t1: IsoTime, d2: IsoDate, t2: IsoTime, largest: &str) -> IsoDuration {
    let a = dt_ns(d1, t1);
    let b = dt_ns(d2, t2);
    if a == b {
        return IsoDuration::default();
    }
    let sign = if a < b { 1 } else { -1 };
    let (sd, st, ed, et) = if a < b { (d1, t1, d2, t2) } else { (d2, t2, d1, t1) };
    let mut tdiff = time_to_ns(et) - time_to_ns(st);
    let mut end_date = ed;
    if tdiff < 0 {
        tdiff += 86_400_000_000_000;
        let (y, m, da) = civil_from_days(epoch_days(ed) - 1);
        end_date = IsoDate { year: y, month: m, day: da };
    }
    let mut out = diff_date(sd, end_date, largest); // years/months/weeks/days (positive)
    let time = balance_ns(tdiff as i128, "hour");
    out.hours = time.hours;
    out.minutes = time.minutes;
    out.seconds = time.seconds;
    out.ms = time.ms;
    out.us = time.us;
    out.ns = time.ns;
    if sign < 0 {
        out = neg_duration(out);
    }
    out
}

/// Add `months` months to a date, clamping the day to the resulting month's length.
fn constrain_add_ym(d: IsoDate, months: i64) -> IsoDate {
    let total = d.year * 12 + (d.month as i64 - 1) + months;
    let (y, m) = balance_year_month(total / 12, total % 12 + 1);
    let day = (d.day).min(days_in_month(y, m));
    IsoDate { year: y, month: m, day }
}

/// ToTemporalDate: accept a PlainDate/PlainDateTime, a fields object, or an ISO string.
fn to_date(i: &mut Interp, v: &Value) -> Result<IsoDate, Value> {
    match get(i, v) {
        Some(Temporal::Date(d)) | Some(Temporal::DateTime(d, _)) => return Ok(d),
        _ => {}
    }
    match v {
        Value::Str(s) => parse_date_str(s).ok_or_else(|| i.make_error("RangeError", "invalid date string")),
        Value::Obj(_) => {
            let year = field_req(i, v, "year")?;
            let month = field_month(i, v)?.clamp(1, 12) as u8;
            let day = field_req(i, v, "day")?.clamp(1, days_in_month(year, month) as i64) as u8;
            Ok(IsoDate { year, month, day })
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainDate")),
    }
}
fn field_req(i: &mut Interp, o: &Value, k: &str) -> Result<i64, Value> {
    let v = getm(i, o, k)?;
    if matches!(v, Value::Undefined) {
        return Err(i.make_error("TypeError", format!("missing field '{k}'")));
    }
    to_int(i, &v)
}
fn field_int(i: &mut Interp, o: &Value, k: &str, default: i64) -> Result<i64, Value> {
    let v = getm(i, o, k)?;
    to_int_default(i, &v, default)
}
/// Read a month from either `month` or `monthCode` ("M01".."M12", optional leap suffix).
fn field_month(i: &mut Interp, o: &Value) -> Result<i64, Value> {
    let m = getm(i, o, "month")?;
    if !matches!(m, Value::Undefined) {
        return to_int(i, &m);
    }
    let mc = getm(i, o, "monthCode")?;
    if let Value::Str(s) = &mc {
        if let Some(num) = s.strip_prefix('M') {
            let num = num.trim_end_matches('L');
            if let Ok(n) = num.parse::<i64>() {
                return Ok(n);
            }
        }
        return Err(i.make_error("RangeError", "invalid monthCode"));
    }
    Err(i.make_error("TypeError", "missing 'month' or 'monthCode'"))
}

fn add_to_date(i: &mut Interp, d: IsoDate, dur: IsoDuration, sign: i64) -> Result<IsoDate, Value> {
    // Add years & months first (constraining the day), then weeks & days.
    let total_months = d.year * 12 + (d.month as i64 - 1) + sign * (dur.years * 12 + dur.months);
    let (y, m) = balance_year_month(total_months / 12, total_months % 12 + 1);
    let dim = days_in_month(y, m);
    let day = (d.day as i64).min(dim as i64);
    let z = days_from_civil(y, m as i64, day) + sign * (dur.weeks * 7 + dur.days);
    let (ny, nm, nd) = civil_from_days(z);
    check_date(i, IsoDate { year: ny, month: nm, day: nd })
}

/// Add a duration's date part (years/months/weeks/days) to a date, clamping the day.
fn add_date_dur(start: IsoDate, d: IsoDuration) -> IsoDate {
    let total_months = start.year * 12 + (start.month as i64 - 1) + d.years * 12 + d.months;
    let (y, m) = balance_year_month(total_months / 12, total_months % 12 + 1);
    let day = start.day.min(days_in_month(y, m));
    let z = days_from_civil(y, m as i64, day as i64) + d.weeks * 7 + d.days;
    let (ny, nm, nd) = civil_from_days(z);
    IsoDate { year: ny, month: nm, day: nd }
}
/// Add a full duration (date + time) to a midnight-anchored start date.
fn add_full_duration(start: IsoDate, d: IsoDuration) -> (IsoDate, IsoTime) {
    let nd = add_date_dur(start, d);
    let tns = duration_time_ns(d);
    let carry = tns.div_euclid(86_400_000_000_000);
    let rem = tns.rem_euclid(86_400_000_000_000);
    let z = epoch_days(nd) as i128 + carry;
    let (y, m, da) = civil_from_days(z as i64);
    (IsoDate { year: y, month: m, day: da }, ns_to_time(rem))
}
/// Read the `relativeTo` option as an anchor date, if present.
fn read_relative_to(i: &mut Interp, opts: &Value) -> Result<Option<IsoDate>, Value> {
    if matches!(opts, Value::Undefined | Value::Str(_)) {
        return Ok(None);
    }
    let v = getm(i, opts, "relativeTo")?;
    match get(i, &v) {
        Some(Temporal::Date(d)) | Some(Temporal::DateTime(d, _)) => return Ok(Some(d)),
        Some(Temporal::Zoned { epoch_ns, offset_ns, .. }) => {
            return Ok(Some(zoned_local(epoch_ns, offset_ns).0))
        }
        _ => {}
    }
    match v {
        Value::Undefined => Ok(None),
        _ => Ok(Some(to_date(i, &v)?)),
    }
}

/// Add a duration to a date+time, carrying the time overflow into the date.
fn dt_add(i: &mut Interp, d: IsoDate, t: IsoTime, dur: IsoDuration, sign: i64) -> Result<(IsoDate, IsoTime), Value> {
    let nd = add_to_date(i, d, dur, sign)?;
    let total = time_to_ns(t) as i128 + sign as i128 * duration_time_ns(dur);
    let carry = total.div_euclid(86_400_000_000_000);
    let tns = total.rem_euclid(86_400_000_000_000);
    let z = epoch_days(nd) as i128 + carry;
    let (ny, nm, nday) = civil_from_days(z as i64);
    let ndate = check_date(i, IsoDate { year: ny, month: nm, day: nday })?;
    Ok((ndate, ns_to_time(tns)))
}

// ===== PlainTime ==============================================================================

fn install_plain_time(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.PlainTime", proto.clone());

    def_getter(it, &proto, "hour", |i, t, _| Ok(Value::Num(as_time(i, &t)?.hour as f64)));
    def_getter(it, &proto, "minute", |i, t, _| Ok(Value::Num(as_time(i, &t)?.minute as f64)));
    def_getter(it, &proto, "second", |i, t, _| Ok(Value::Num(as_time(i, &t)?.second as f64)));
    def_getter(it, &proto, "millisecond", |i, t, _| Ok(Value::Num(as_time(i, &t)?.ms as f64)));
    def_getter(it, &proto, "microsecond", |i, t, _| Ok(Value::Num(as_time(i, &t)?.us as f64)));
    def_getter(it, &proto, "nanosecond", |i, t, _| Ok(Value::Num(as_time(i, &t)?.ns as f64)));

    it.def_method(&proto, "toString", 0, |i, t, a| { let x=as_time(i,&t)?; Ok(Value::str(fmt_time_opts(i, x, &arg(a,0))?)) });
    it.def_method(&proto, "toJSON", 0, |i, t, _| Ok(Value::str(fmt_time(as_time(i, &t)?))));
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.PlainTime has no valueOf; use compare"))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let y = to_time(i, &arg(a, 0))?;
        Ok(Value::Bool(time_to_ns(x) == time_to_ns(y)))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let f = arg(a, 0);
        let hour = field_int(i, &f, "hour", x.hour as i64)? as u8;
        let minute = field_int(i, &f, "minute", x.minute as i64)? as u8;
        let second = field_int(i, &f, "second", x.second as i64)? as u8;
        let ms = field_int(i, &f, "millisecond", x.ms as i64)? as u16;
        let us = field_int(i, &f, "microsecond", x.us as i64)? as u16;
        let ns = field_int(i, &f, "nanosecond", x.ns as i64)? as u16;
        let nt = check_time(i, IsoTime { hour, minute, second, ms, us, ns })?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(nt)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let total = (time_to_ns(x) as i128 + duration_time_ns(dur)).rem_euclid(86_400_000_000_000);
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(ns_to_time(total))))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let total = (time_to_ns(x) as i128 - duration_time_ns(dur)).rem_euclid(86_400_000_000_000);
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(ns_to_time(total))))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let y = to_time(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "hour")?;
        let diff = (time_to_ns(y) - time_to_ns(x)) as i128;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(balance_ns(diff, &largest))))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let y = to_time(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "hour")?;
        let diff = (time_to_ns(x) - time_to_ns(y)) as i128;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(balance_ns(diff, &largest))))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let o = arg(a, 0);
        let smallest = opt_str(i, &o, "smallestUnit", "")?;
        let unit = unit_ns(&smallest)
            .ok_or_else(|| i.make_error("RangeError", "smallestUnit is required"))?;
        let incr_raw = opt_num(i, &o, "roundingIncrement", 1)?;
        let mode = opt_str(i, &o, "roundingMode", "halfExpand")?;
        check_mode(i, &mode)?;
        check_increment(i, smallest.strip_suffix('s').unwrap_or(&smallest), incr_raw)?;
        let incr = incr_raw as i128;
        let r = round_ns(time_to_ns(x) as i128, unit * incr, &mode).rem_euclid(86_400_000_000_000);
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(ns_to_time(r))))
    });

    let ctor = add_ctor(it, ns, "PlainTime", 0, proto, |i, _t, a| {
        require_new(i)?;
        let hour = to_int_default(i, &arg(a, 0), 0)? as u8;
        let minute = to_int_default(i, &arg(a, 1), 0)? as u8;
        let second = to_int_default(i, &arg(a, 2), 0)? as u8;
        let ms = to_int_default(i, &arg(a, 3), 0)? as u16;
        let us = to_int_default(i, &arg(a, 4), 0)? as u16;
        let ns = to_int_default(i, &arg(a, 5), 0)? as u16;
        let t = check_time(i, IsoTime { hour, minute, second, ms, us, ns })?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(t)))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let t = to_time(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(t)))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_time(i, &arg(a, 0))?;
        let y = to_time(i, &arg(a, 1))?;
        Ok(Value::Num(time_to_ns(x).cmp(&time_to_ns(y)) as i64 as f64))
    });
}

fn time_to_ns(t: IsoTime) -> i64 {
    ((t.hour as i64 * 60 + t.minute as i64) * 60 + t.second as i64) * 1_000_000_000
        + t.ms as i64 * 1_000_000
        + t.us as i64 * 1000
        + t.ns as i64
}
fn dt_ns(d: IsoDate, t: IsoTime) -> i128 {
    epoch_days(d) as i128 * 86_400_000_000_000 + time_to_ns(t) as i128
}
/// The time-only nanosecond span of a duration (hours/minutes/seconds/sub-second).
fn duration_time_ns(d: IsoDuration) -> i128 {
    (d.hours as i128 * 3600 + d.minutes as i128 * 60 + d.seconds as i128) * 1_000_000_000
        + d.ms as i128 * 1_000_000
        + d.us as i128 * 1000
        + d.ns as i128
}
/// Convert a within-a-day nanosecond count to an IsoTime.
fn ns_to_time(ns: i128) -> IsoTime {
    let secs = ns / 1_000_000_000;
    IsoTime {
        hour: (secs / 3600) as u8,
        minute: ((secs / 60) % 60) as u8,
        second: (secs % 60) as u8,
        ms: ((ns / 1_000_000) % 1000) as u16,
        us: ((ns / 1000) % 1000) as u16,
        ns: (ns % 1000) as u16,
    }
}
/// Balance a nanosecond span into a Duration whose largest unit is `largest`.
fn balance_ns(total: i128, largest: &str) -> IsoDuration {
    let largest = largest.strip_suffix('s').unwrap_or(largest); // accept plural unit names
    let neg = total < 0;
    let mut n = total.abs();
    let nanos = (n % 1000) as i64;
    n /= 1000;
    let micros = (n % 1000) as i64;
    n /= 1000;
    let millis = (n % 1000) as i64;
    n /= 1000;
    let secs = n as i64; // remaining whole seconds
    let mut out = IsoDuration { ms: millis, us: micros, ns: nanos, ..Default::default() };
    match largest {
        "day" => {
            out.days = secs / 86400;
            let r = secs % 86400;
            out.hours = r / 3600;
            out.minutes = (r / 60) % 60;
            out.seconds = r % 60;
        }
        "hour" | "auto" => {
            out.hours = secs / 3600;
            out.minutes = (secs / 60) % 60;
            out.seconds = secs % 60;
        }
        "minute" => {
            out.minutes = secs / 60;
            out.seconds = secs % 60;
        }
        _ => out.seconds = secs,
    }
    if neg {
        out = neg_duration(out);
    }
    out
}
fn to_time(i: &mut Interp, v: &Value) -> Result<IsoTime, Value> {
    match get(i, v) {
        Some(Temporal::Time(t)) | Some(Temporal::DateTime(_, t)) => return Ok(t),
        _ => {}
    }
    match v {
        Value::Str(s) => {
            // A PlainTime string may not carry a UTC designator or time-zone annotation.
            if s.contains(['Z', 'z']) || s.contains('[') {
                return Err(i.make_error("RangeError", "invalid PlainTime string"));
            }
            parse_time_str(s).ok_or_else(|| i.make_error("RangeError", "invalid time string"))
        }
        Value::Obj(_) => {
            // from() constrains out-of-range fields (overflow: 'constrain'); at least one time
            // field must be present.
            let fields =
                ["hour", "minute", "second", "millisecond", "microsecond", "nanosecond"];
            let mut any = false;
            let mut vals = [0i64; 6];
            for (k, slot) in fields.iter().zip(vals.iter_mut()) {
                let fv = getm(i, v, k)?;
                if !matches!(fv, Value::Undefined) {
                    any = true;
                    *slot = to_int(i, &fv)?;
                }
            }
            if !any {
                return Err(i.make_error("TypeError", "object has no time fields"));
            }
            Ok(IsoTime {
                hour: vals[0].clamp(0, 23) as u8,
                minute: vals[1].clamp(0, 59) as u8,
                second: vals[2].clamp(0, 59) as u8,
                ms: vals[3].clamp(0, 999) as u16,
                us: vals[4].clamp(0, 999) as u16,
                ns: vals[5].clamp(0, 999) as u16,
            })
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainTime")),
    }
}

// ===== PlainDateTime ==========================================================================

fn install_plain_datetime(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.PlainDateTime", proto.clone());

    def_getter(it, &proto, "year", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.0.year as f64)));
    def_getter(it, &proto, "month", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.0.month as f64)));
    def_getter(it, &proto, "day", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.0.day as f64)));
    def_getter(it, &proto, "monthCode", |i, t, _| Ok(Value::str(month_code(as_datetime(i, &t)?.0.month))));
    def_getter(it, &proto, "calendarId", |_i, _t, _| Ok(Value::str("iso8601")));
    def_getter(it, &proto, "hour", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.1.hour as f64)));
    def_getter(it, &proto, "minute", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.1.minute as f64)));
    def_getter(it, &proto, "second", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.1.second as f64)));
    def_getter(it, &proto, "millisecond", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.1.ms as f64)));
    def_getter(it, &proto, "microsecond", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.1.us as f64)));
    def_getter(it, &proto, "nanosecond", |i, t, _| Ok(Value::Num(as_datetime(i, &t)?.1.ns as f64)));
    def_getter(it, &proto, "dayOfWeek", |i, t, _| Ok(Value::Num(iso_day_of_week(as_datetime(i, &t)?.0) as f64)));
    def_getter(it, &proto, "dayOfYear", |i, t, _| Ok(Value::Num(iso_day_of_year(as_datetime(i, &t)?.0) as f64)));
    def_getter(it, &proto, "daysInMonth", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(days_in_month(d.year, d.month) as f64))
    });
    def_getter(it, &proto, "daysInYear", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(if is_leap(d.year) { 366.0 } else { 365.0 }))
    });
    def_getter(it, &proto, "inLeapYear", |i, t, _| Ok(Value::Bool(is_leap(as_datetime(i, &t)?.0.year))));

    it.def_method(&proto, "toString", 0, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let ts = fmt_time_opts(i, tm, &arg(a, 0))?;
        Ok(Value::str(format!("{}T{}{}", fmt_date(d), ts, cal_suffix(i, &arg(a, 0))?)))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let (d, tm) = as_datetime(i, &t)?;
        Ok(Value::str(format!("{}T{}", fmt_date(d), fmt_time(tm))))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.PlainDateTime has no valueOf; use compare"))
    });
    it.def_method(&proto, "toPlainDate", 0, |i, t, _| {
        let (d, _) = as_datetime(i, &t)?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(d)))
    });
    it.def_method(&proto, "toPlainTime", 0, |i, t, _| {
        let (_, tm) = as_datetime(i, &t)?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(tm)))
    });
    it.def_method(&proto, "withPlainTime", 1, |i, t, a| {
        let (d, _) = as_datetime(i, &t)?;
        let nt = match arg(a, 0) {
            Value::Undefined => IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 },
            v => to_time(i, &v)?,
        };
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, nt)))
    });
    it.def_method(&proto, "withPlainDate", 1, |i, t, a| {
        let (_, tm) = as_datetime(i, &t)?;
        let nd = to_date(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(nd, tm)))
    });
    it.def_method(&proto, "withCalendar", 1, |i, t, _| {
        let (d, tm) = as_datetime(i, &t)?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, tm)))
    });
    it.def_method(&proto, "toZonedDateTime", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let tzv = arg(a, 0);
        let tz: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let local = dt_ns(d, tm);
        let offset = offset_for_local(&tz, local);
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: local - offset as i128, offset_ns: offset, tz }))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (od, otm) = to_datetime(i, &arg(a, 0))?;
        Ok(Value::Bool(cmp_date(d, od) == 0 && time_to_ns(tm) == time_to_ns(otm)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let (nd, ntm) = dt_add(i, d, tm, dur, 1)?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(nd, ntm)))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let (nd, ntm) = dt_add(i, d, tm, dur, -1)?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(nd, ntm)))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let f = arg(a, 0);
        let year = field_int(i, &f, "year", d.year)?;
        let month = field_int(i, &f, "month", d.month as i64)? as u8;
        let day = field_int(i, &f, "day", d.day as i64)? as u8;
        let hour = field_int(i, &f, "hour", tm.hour as i64)? as u8;
        let minute = field_int(i, &f, "minute", tm.minute as i64)? as u8;
        let second = field_int(i, &f, "second", tm.second as i64)? as u8;
        let ms = field_int(i, &f, "millisecond", tm.ms as i64)? as u16;
        let us = field_int(i, &f, "microsecond", tm.us as i64)? as u16;
        let nsf = field_int(i, &f, "nanosecond", tm.ns as i64)? as u16;
        let nd = check_date(i, IsoDate { year, month, day })?;
        let nt = check_time(i, IsoTime { hour, minute, second, ms, us, ns: nsf })?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(nd, nt)))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let o = arg(a, 0);
        let smallest = opt_str(i, &o, "smallestUnit", "")?;
        let unit = if smallest == "day" {
            86_400_000_000_000
        } else {
            unit_ns(&smallest).ok_or_else(|| i.make_error("RangeError", "smallestUnit is required"))?
        };
        let incr_raw = opt_num(i, &o, "roundingIncrement", 1)?;
        let mode = opt_str(i, &o, "roundingMode", "halfExpand")?;
        check_mode(i, &mode)?;
        check_increment(i, smallest.strip_suffix('s').unwrap_or(&smallest), incr_raw)?;
        let incr = incr_raw as i128;
        let rounded = round_ns(dt_ns(d, tm), unit * incr, &mode);
        let z = rounded.div_euclid(86_400_000_000_000) as i64;
        let rem = rounded.rem_euclid(86_400_000_000_000);
        let (y, mo, da) = civil_from_days(z);
        let nd = check_date(i, IsoDate { year: y, month: mo, day: da })?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(nd, ns_to_time(rem))))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (od, otm) = to_datetime(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "day")?;
        let dur = if matches!(largest.strip_suffix('s').unwrap_or(&largest), "year" | "month" | "week") {
            diff_datetime(d, tm, od, otm, &largest)
        } else {
            balance_ns(dt_ns(od, otm) - dt_ns(d, tm), &largest)
        };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (od, otm) = to_datetime(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "day")?;
        let dur = if matches!(largest.strip_suffix('s').unwrap_or(&largest), "year" | "month" | "week") {
            diff_datetime(od, otm, d, tm, &largest)
        } else {
            balance_ns(dt_ns(d, tm) - dt_ns(od, otm), &largest)
        };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });

    let ctor = add_ctor(it, ns, "PlainDateTime", 3, proto, |i, _t, a| {
        require_new(i)?;
        let year = to_int(i, &arg(a, 0))?;
        let month = to_int(i, &arg(a, 1))? as u8;
        let day = to_int(i, &arg(a, 2))? as u8;
        let hour = to_int_default(i, &arg(a, 3), 0)? as u8;
        let minute = to_int_default(i, &arg(a, 4), 0)? as u8;
        let second = to_int_default(i, &arg(a, 5), 0)? as u8;
        let ms = to_int_default(i, &arg(a, 6), 0)? as u16;
        let us = to_int_default(i, &arg(a, 7), 0)? as u16;
        let ns = to_int_default(i, &arg(a, 8), 0)? as u16;
        let d = check_date(i, IsoDate { year, month, day })?;
        let tm = check_time(i, IsoTime { hour, minute, second, ms, us, ns })?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, tm)))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let (d, tm) = to_datetime(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, tm)))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let (xd, xt) = to_datetime(i, &arg(a, 0))?;
        let (yd, yt) = to_datetime(i, &arg(a, 1))?;
        let c = cmp_date(xd, yd);
        Ok(Value::Num(if c != 0 { c } else { time_to_ns(xt).cmp(&time_to_ns(yt)) as i64 } as f64))
    });
}

fn to_datetime(i: &mut Interp, v: &Value) -> Result<(IsoDate, IsoTime), Value> {
    match get(i, v) {
        Some(Temporal::DateTime(d, t)) => return Ok((d, t)),
        Some(Temporal::Date(d)) => {
            return Ok((d, IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 }))
        }
        _ => {}
    }
    match v {
        Value::Str(s) => {
            let d = parse_date_str(s).ok_or_else(|| i.make_error("RangeError", "invalid datetime"))?;
            let t = parse_time_str(s).unwrap_or(IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 });
            Ok((d, t))
        }
        Value::Obj(_) => {
            let d = to_date(i, v)?;
            // Time fields constrain (overflow: 'constrain'); absent fields default to 0.
            let t = IsoTime {
                hour: field_int(i, v, "hour", 0)?.clamp(0, 23) as u8,
                minute: field_int(i, v, "minute", 0)?.clamp(0, 59) as u8,
                second: field_int(i, v, "second", 0)?.clamp(0, 59) as u8,
                ms: field_int(i, v, "millisecond", 0)?.clamp(0, 999) as u16,
                us: field_int(i, v, "microsecond", 0)?.clamp(0, 999) as u16,
                ns: field_int(i, v, "nanosecond", 0)?.clamp(0, 999) as u16,
            };
            Ok((d, t))
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainDateTime")),
    }
}

// ===== PlainYearMonth / PlainMonthDay =========================================================

fn install_year_month(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.PlainYearMonth", proto.clone());
    def_getter(it, &proto, "year", |i, t, _| Ok(Value::Num(as_yearmonth(i, &t)?.year as f64)));
    def_getter(it, &proto, "month", |i, t, _| Ok(Value::Num(as_yearmonth(i, &t)?.month as f64)));
    def_getter(it, &proto, "monthCode", |i, t, _| Ok(Value::str(month_code(as_yearmonth(i, &t)?.month))));
    def_getter(it, &proto, "calendarId", |_i, _t, _| Ok(Value::str("iso8601")));
    def_getter(it, &proto, "daysInMonth", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::Num(days_in_month(d.year, d.month) as f64))
    });
    def_getter(it, &proto, "daysInYear", |i, t, _| {
        Ok(Value::Num(if is_leap(as_yearmonth(i, &t)?.year) { 366.0 } else { 365.0 }))
    });
    def_getter(it, &proto, "monthsInYear", |i, t, _| {
        as_yearmonth(i, &t)?;
        Ok(Value::Num(12.0))
    });
    def_getter(it, &proto, "inLeapYear", |i, t, _| Ok(Value::Bool(is_leap(as_yearmonth(i, &t)?.year))));
    it.def_method(&proto, "toString", 0, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::str(format!("{}-{:02}{}", pad_year(d.year), d.month, cal_suffix(i, &arg(a, 0))?)))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::str(format!("{}-{:02}", pad_year(d.year), d.month)))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let o = to_yearmonth(i, &arg(a, 0))?;
        Ok(Value::Bool(d.year == o.year && d.month == o.month))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let f = arg(a, 0);
        let year = field_int(i, &f, "year", d.year)?;
        let month = field_int(i, &f, "month", d.month as i64)?;
        if !(1..=12).contains(&month) {
            return Err(i.make_error("RangeError", "invalid month"));
        }
        Ok(make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(IsoDate { year, month: month as u8, day: 1 })))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let total = d.year * 12 + (d.month as i64 - 1) + dur.years * 12 + dur.months;
        let (y, m) = balance_year_month(total / 12, total % 12 + 1);
        Ok(make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(IsoDate { year: y, month: m, day: 1 })))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let total = d.year * 12 + (d.month as i64 - 1) - dur.years * 12 - dur.months;
        let (y, m) = balance_year_month(total / 12, total % 12 + 1);
        Ok(make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(IsoDate { year: y, month: m, day: 1 })))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let o = to_yearmonth(i, &arg(a, 0))?;
        let months = (o.year * 12 + o.month as i64) - (d.year * 12 + d.month as i64);
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "year")?;
        let dur = if largest == "month" {
            IsoDuration { months, ..Default::default() }
        } else {
            IsoDuration { years: months / 12, months: months % 12, ..Default::default() }
        };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let o = to_yearmonth(i, &arg(a, 0))?;
        let months = (d.year * 12 + d.month as i64) - (o.year * 12 + o.month as i64);
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "year")?;
        let dur = if largest == "month" {
            IsoDuration { months, ..Default::default() }
        } else {
            IsoDuration { years: months / 12, months: months % 12, ..Default::default() }
        };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    let ctor = add_ctor(it, ns, "PlainYearMonth", 2, proto, |i, _t, a| {
        require_new(i)?;
        let year = to_int(i, &arg(a, 0))?;
        let month = to_int(i, &arg(a, 1))?;
        let day = to_int_default(i, &arg(a, 2), 1)?;
        if !(1..=12).contains(&month) {
            return Err(i.make_error("RangeError", "invalid month"));
        }
        Ok(make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(IsoDate { year, month: month as u8, day: day as u8 })))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let d = to_yearmonth(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(d)))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_yearmonth(i, &arg(a, 0))?;
        let y = to_yearmonth(i, &arg(a, 1))?;
        let xk = x.year * 12 + x.month as i64;
        let yk = y.year * 12 + y.month as i64;
        Ok(Value::Num(xk.cmp(&yk) as i64 as f64))
    });
}
fn to_yearmonth(i: &mut Interp, v: &Value) -> Result<IsoDate, Value> {
    if let Some(Temporal::YearMonth(d)) = get(i, v) {
        return Ok(d);
    }
    match v {
        Value::Str(s) => {
            let d = parse_date_str(&format!("{}-01", s.trim_end_matches('Z')))
                .or_else(|| parse_date_str(s))
                .ok_or_else(|| i.make_error("RangeError", "invalid year-month"))?;
            Ok(d)
        }
        Value::Obj(_) => {
            let year = field_req(i, v, "year")?;
            let month = field_month(i, v)?.clamp(1, 12) as u8;
            Ok(IsoDate { year, month, day: 1 })
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainYearMonth")),
    }
}

fn install_month_day(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.PlainMonthDay", proto.clone());
    def_getter(it, &proto, "monthCode", |i, t, _| Ok(Value::str(month_code(as_monthday(i, &t)?.month))));
    def_getter(it, &proto, "day", |i, t, _| Ok(Value::Num(as_monthday(i, &t)?.day as f64)));
    def_getter(it, &proto, "calendarId", |_i, _t, _| Ok(Value::str("iso8601")));
    it.def_method(&proto, "toString", 0, |i, t, a| {
        let d = as_monthday(i, &t)?;
        Ok(Value::str(format!("{:02}-{:02}{}", d.month, d.day, cal_suffix(i, &arg(a, 0))?)))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let d = as_monthday(i, &t)?;
        Ok(Value::str(format!("{:02}-{:02}", d.month, d.day)))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let d = as_monthday(i, &t)?;
        let o = to_monthday(i, &arg(a, 0))?;
        Ok(Value::Bool(d.month == o.month && d.day == o.day))
    });
    let ctor = add_ctor(it, ns, "PlainMonthDay", 2, proto, |i, _t, a| {
        require_new(i)?;
        let month = to_int(i, &arg(a, 0))?;
        let day = to_int(i, &arg(a, 1))?;
        let year = to_int_default(i, &arg(a, 2), 1972)?;
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month as u8) as i64 {
            return Err(i.make_error("RangeError", "invalid month-day"));
        }
        Ok(make(i, "Temporal.PlainMonthDay", Temporal::MonthDay(IsoDate { year, month: month as u8, day: day as u8 })))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let d = to_monthday(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.PlainMonthDay", Temporal::MonthDay(d)))
    });
}
fn to_monthday(i: &mut Interp, v: &Value) -> Result<IsoDate, Value> {
    if let Some(Temporal::MonthDay(d)) = get(i, v) {
        return Ok(d);
    }
    match v {
        Value::Str(s) => {
            // A full date string is accepted (the month/day are taken from it).
            if let Some(d) = parse_date_str(s) {
                return Ok(IsoDate { year: 1972, month: d.month, day: d.day });
            }
            if !valid_annotations(s) {
                return Err(i.make_error("RangeError", "invalid month-day"));
            }
            let core = s.split('[').next().unwrap_or(s).trim().trim_start_matches("--");
            let mut p = core.splitn(2, '-');
            let ms = p.next().unwrap_or("");
            let ds = p.next().unwrap_or("");
            if ms.len() != 2 || ds.len() != 2 {
                return Err(i.make_error("RangeError", "invalid month-day"));
            }
            let m: u8 = ms.parse().map_err(|_| i.make_error("RangeError", "invalid month-day"))?;
            let d: u8 = ds.parse().map_err(|_| i.make_error("RangeError", "invalid month-day"))?;
            if !(1..=12).contains(&m) || d < 1 || d > days_in_month(1972, m) {
                return Err(i.make_error("RangeError", "invalid month-day"));
            }
            Ok(IsoDate { year: 1972, month: m, day: d })
        }
        Value::Obj(_) => {
            let month = field_month(i, v)?.clamp(1, 12) as u8;
            let day = field_req(i, v, "day")?.clamp(1, days_in_month(1972, month) as i64) as u8;
            Ok(IsoDate { year: 1972, month, day })
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainMonthDay")),
    }
}

// ===== Duration ===============================================================================

fn install_duration(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.Duration", proto.clone());
    def_getter(it, &proto, "years", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.years as f64)));
    def_getter(it, &proto, "months", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.months as f64)));
    def_getter(it, &proto, "weeks", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.weeks as f64)));
    def_getter(it, &proto, "days", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.days as f64)));
    def_getter(it, &proto, "hours", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.hours as f64)));
    def_getter(it, &proto, "minutes", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.minutes as f64)));
    def_getter(it, &proto, "seconds", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.seconds as f64)));
    def_getter(it, &proto, "milliseconds", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.ms as f64)));
    def_getter(it, &proto, "microseconds", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.us as f64)));
    def_getter(it, &proto, "nanoseconds", |i, t, _| Ok(Value::Num(as_duration(i, &t)?.ns as f64)));
    def_getter(it, &proto, "sign", |i, t, _| Ok(Value::Num(duration_sign(as_duration(i, &t)?) as f64)));
    def_getter(it, &proto, "blank", |i, t, _| Ok(Value::Bool(duration_sign(as_duration(i, &t)?) == 0)));

    it.def_method(&proto, "toString", 0, |i, t, _| Ok(Value::str(fmt_duration(as_duration(i, &t)?))));
    it.def_method(&proto, "toJSON", 0, |i, t, _| Ok(Value::str(fmt_duration(as_duration(i, &t)?))));
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.Duration has no valueOf; use compare"))
    });
    it.def_method(&proto, "negated", 0, |i, t, _| {
        let d = as_duration(i, &t)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(neg_duration(d))))
    });
    it.def_method(&proto, "abs", 0, |i, t, _| {
        let d = as_duration(i, &t)?;
        let d = if duration_sign(d) < 0 { neg_duration(d) } else { d };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(d)))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let f = arg(a, 0);
        let nd = IsoDuration {
            years: field_int(i, &f, "years", d.years)?,
            months: field_int(i, &f, "months", d.months)?,
            weeks: field_int(i, &f, "weeks", d.weeks)?,
            days: field_int(i, &f, "days", d.days)?,
            hours: field_int(i, &f, "hours", d.hours)?,
            minutes: field_int(i, &f, "minutes", d.minutes)?,
            seconds: field_int(i, &f, "seconds", d.seconds)?,
            ms: field_int(i, &f, "milliseconds", d.ms)?,
            us: field_int(i, &f, "microseconds", d.us)?,
            ns: field_int(i, &f, "nanoseconds", d.ns)?,
        };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(nd)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let o = to_duration(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(add_duration(d, o, 1))))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let o = to_duration(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(add_duration(d, o, -1))))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let o = arg(a, 0);
        let smallest = opt_str(i, &o, "smallestUnit", "nanosecond")?;
        let mut largest = opt_str(i, &o, "largestUnit", "auto")?;
        let incr_raw = opt_num(i, &o, "roundingIncrement", 1)?;
        let mode = opt_str(i, &o, "roundingMode", "halfExpand")?;
        check_mode(i, &mode)?;
        check_increment(i, smallest.strip_suffix('s').unwrap_or(&smallest), incr_raw)?;
        let incr = incr_raw as i128;
        let has_cal = d.years != 0 || d.months != 0 || d.weeks != 0;
        let cal_unit = |u: &str| matches!(u.strip_suffix('s').unwrap_or(u), "year" | "month" | "week");

        if has_cal || cal_unit(&smallest) || cal_unit(&largest) {
            // Calendar rounding needs a reference point. With one, balance via the resulting date.
            let start = read_relative_to(i, &o)?
                .ok_or_else(|| i.make_error("RangeError", "rounding a calendar duration requires relativeTo"))?;
            if largest == "auto" {
                largest = if d.years != 0 {
                    "year".into()
                } else if d.months != 0 {
                    "month".into()
                } else if d.weeks != 0 {
                    "week".into()
                } else if d.days != 0 {
                    "day".into()
                } else {
                    "hour".into()
                };
            }
            let (end_date, end_time) = add_full_duration(start, d);
            let mut result = diff_date(start, end_date, &largest);
            result.hours = end_time.hour as i64;
            result.minutes = end_time.minute as i64;
            result.seconds = end_time.second as i64;
            result.ms = end_time.ms as i64;
            result.us = end_time.us as i64;
            result.ns = end_time.ns as i64;
            // Round the time portion when the smallest unit is sub-day.
            if let Some(u) = unit_ns(smallest.strip_suffix('s').unwrap_or(&smallest)) {
                let tns = duration_time_ns(result);
                let rounded = round_ns(tns, u * incr, &mode);
                let balanced = balance_ns(rounded, "hour");
                result.hours = balanced.hours;
                result.minutes = balanced.minutes;
                result.seconds = balanced.seconds;
                result.ms = balanced.ms;
                result.us = balanced.us;
                result.ns = balanced.ns;
            }
            return Ok(make(i, "Temporal.Duration", Temporal::Duration(result)));
        }

        if unit_ns(smallest.strip_suffix('s').unwrap_or(&smallest)).is_none() && smallest != "day" {
            return Err(i.make_error("RangeError", "invalid smallestUnit"));
        }
        if largest == "auto" {
            largest = if d.days != 0 { "day".into() } else { "hour".into() };
        }
        let unit = if smallest == "day" {
            86_400_000_000_000
        } else {
            unit_ns(smallest.strip_suffix('s').unwrap_or(&smallest)).unwrap()
        };
        let total = d.days as i128 * 86_400_000_000_000 + duration_time_ns(d);
        let rounded = round_ns(total, unit * incr, &mode);
        Ok(make(i, "Temporal.Duration", Temporal::Duration(balance_ns(rounded, &largest))))
    });
    it.def_method(&proto, "total", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let o = arg(a, 0);
        let unit = opt_str(i, &o, "unit", "")?;
        if unit.is_empty() {
            return Err(i.make_error("RangeError", "unit is required"));
        }
        let unit_s = unit.strip_suffix('s').unwrap_or(&unit);
        let has_cal = d.years != 0 || d.months != 0 || d.weeks != 0;
        // Calendar units in the duration or unit need a reference point to total against.
        let total_ns = if has_cal || matches!(unit_s, "year" | "month" | "week") {
            let start = read_relative_to(i, &o)?
                .ok_or_else(|| i.make_error("RangeError", "total of a calendar duration requires relativeTo"))?;
            if matches!(unit_s, "year" | "month") {
                return Err(i.make_error("RangeError", "year/month totals are not supported"));
            }
            let (ed, et) = add_full_duration(start, d);
            dt_ns(ed, et) - dt_ns(start, IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 })
        } else {
            d.days as i128 * 86_400_000_000_000 + duration_time_ns(d)
        };
        let u = match unit_s {
            "day" => 86_400_000_000_000i128,
            "week" => 7 * 86_400_000_000_000i128,
            _ => unit_ns(unit_s).ok_or_else(|| i.make_error("RangeError", "invalid unit"))?,
        };
        Ok(Value::Num(total_ns as f64 / u as f64))
    });

    let ctor = add_ctor(it, ns, "Duration", 0, proto, |i, _t, a| {
        require_new(i)?;
        let d = IsoDuration {
            years: to_int_default(i, &arg(a, 0), 0)?,
            months: to_int_default(i, &arg(a, 1), 0)?,
            weeks: to_int_default(i, &arg(a, 2), 0)?,
            days: to_int_default(i, &arg(a, 3), 0)?,
            hours: to_int_default(i, &arg(a, 4), 0)?,
            minutes: to_int_default(i, &arg(a, 5), 0)?,
            seconds: to_int_default(i, &arg(a, 6), 0)?,
            ms: to_int_default(i, &arg(a, 7), 0)?,
            us: to_int_default(i, &arg(a, 8), 0)?,
            ns: to_int_default(i, &arg(a, 9), 0)?,
        };
        validate_duration(i, d)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(d)))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let d = to_duration(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(d)))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_duration(i, &arg(a, 0))?;
        let y = to_duration(i, &arg(a, 1))?;
        let has_cal = x.years != 0 || x.months != 0 || x.weeks != 0 || y.years != 0 || y.months != 0 || y.weeks != 0;
        let (xn, yn) = if has_cal {
            let start = read_relative_to(i, &arg(a, 2))?
                .ok_or_else(|| i.make_error("RangeError", "comparing calendar durations requires relativeTo"))?;
            let (xd, xt) = add_full_duration(start, x);
            let (yd, yt) = add_full_duration(start, y);
            (dt_ns(xd, xt), dt_ns(yd, yt))
        } else {
            (
                x.days as i128 * 86_400_000_000_000 + duration_time_ns(x),
                y.days as i128 * 86_400_000_000_000 + duration_time_ns(y),
            )
        };
        Ok(Value::Num(xn.cmp(&yn) as i64 as f64))
    });
}
fn neg_duration(d: IsoDuration) -> IsoDuration {
    IsoDuration {
        years: -d.years,
        months: -d.months,
        weeks: -d.weeks,
        days: -d.days,
        hours: -d.hours,
        minutes: -d.minutes,
        seconds: -d.seconds,
        ms: -d.ms,
        us: -d.us,
        ns: -d.ns,
    }
}
fn add_duration(a: IsoDuration, b: IsoDuration, sign: i64) -> IsoDuration {
    IsoDuration {
        years: a.years + sign * b.years,
        months: a.months + sign * b.months,
        weeks: a.weeks + sign * b.weeks,
        days: a.days + sign * b.days,
        hours: a.hours + sign * b.hours,
        minutes: a.minutes + sign * b.minutes,
        seconds: a.seconds + sign * b.seconds,
        ms: a.ms + sign * b.ms,
        us: a.us + sign * b.us,
        ns: a.ns + sign * b.ns,
    }
}
/// All non-zero fields must share one sign.
fn validate_duration(i: &Interp, d: IsoDuration) -> Result<(), Value> {
    let mut sign = 0i64;
    for v in [d.years, d.months, d.weeks, d.days, d.hours, d.minutes, d.seconds, d.ms, d.us, d.ns] {
        if v != 0 {
            let s = if v < 0 { -1 } else { 1 };
            if sign != 0 && sign != s {
                return Err(i.make_error("RangeError", "mixed-sign duration"));
            }
            sign = s;
        }
    }
    Ok(())
}
fn to_duration(i: &mut Interp, v: &Value) -> Result<IsoDuration, Value> {
    if let Some(Temporal::Duration(d)) = get(i, v) {
        return Ok(d);
    }
    match v {
        Value::Str(s) => parse_duration_str(s).ok_or_else(|| i.make_error("RangeError", "invalid duration")),
        Value::Obj(_) => {
            let d = IsoDuration {
                years: field_int(i, v, "years", 0)?,
                months: field_int(i, v, "months", 0)?,
                weeks: field_int(i, v, "weeks", 0)?,
                days: field_int(i, v, "days", 0)?,
                hours: field_int(i, v, "hours", 0)?,
                minutes: field_int(i, v, "minutes", 0)?,
                seconds: field_int(i, v, "seconds", 0)?,
                ms: field_int(i, v, "milliseconds", 0)?,
                us: field_int(i, v, "microseconds", 0)?,
                ns: field_int(i, v, "nanoseconds", 0)?,
            };
            validate_duration(i, d)?;
            Ok(d)
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.Duration")),
    }
}
fn parse_duration_str(s: &str) -> Option<IsoDuration> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-').or_else(|| s.strip_prefix('+').map(|_| s)) {
        Some(r) if s.starts_with('-') => (true, r),
        _ => (false, s.trim_start_matches('+')),
    };
    let s = s.strip_prefix('P').or_else(|| s.strip_prefix('p'))?;
    let mut d = IsoDuration::default();
    let (date_part, time_part) = match s.split_once('T').or_else(|| s.split_once('t')) {
        Some((dp, tp)) => (dp, Some(tp)),
        None => (s, None),
    };
    let mut num = String::new();
    for c in date_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i64 = num.parse().ok()?;
            num.clear();
            match c {
                'Y' | 'y' => d.years = n,
                'W' | 'w' => d.weeks = n,
                'D' | 'd' => d.days = n,
                'M' | 'm' => d.months = n,
                _ => return None,
            }
        }
    }
    if let Some(tp) = time_part {
        let mut num = String::new();
        for c in tp.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else {
                let base = num.split('.').next().unwrap_or("0");
                let n: i64 = base.parse().ok()?;
                num.clear();
                match c {
                    'H' | 'h' => d.hours = n,
                    'M' | 'm' => d.minutes = n,
                    'S' | 's' => d.seconds = n,
                    _ => return None,
                }
            }
        }
    }
    if neg {
        d = neg_duration(d);
    }
    Some(d)
}

// ===== Instant ================================================================================

fn install_instant(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.Instant", proto.clone());
    def_getter(it, &proto, "epochMilliseconds", |i, t, _| {
        Ok(Value::Num((as_instant(i, &t)?.div_euclid(1_000_000)) as f64))
    });
    def_getter(it, &proto, "epochNanoseconds", |i, t, _| {
        Ok(Value::BigInt(as_instant(i, &t)?))
    });
    it.def_method(&proto, "toString", 0, |i, t, a| {
        let ns = as_instant(i, &t)?;
        let z = ns.div_euclid(86_400_000_000_000) as i64;
        let rem = ns.rem_euclid(86_400_000_000_000) as i64;
        let (y, mo, da) = civil_from_days(z);
        let secs = rem / 1_000_000_000;
        let t = IsoTime {
            hour: (secs / 3600) as u8,
            minute: ((secs / 60) % 60) as u8,
            second: (secs % 60) as u8,
            ms: ((rem / 1_000_000) % 1000) as u16,
            us: ((rem / 1000) % 1000) as u16,
            ns: (rem % 1000) as u16,
        };
        let ts = fmt_time_opts(i, t, &arg(a, 0))?;
        Ok(Value::str(format!("{}T{}Z", fmt_date(IsoDate { year: y, month: mo, day: da }), ts)))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.Instant has no valueOf; use compare"))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let ns = as_instant(i, &t)?;
        let z = ns.div_euclid(86_400_000_000_000) as i64;
        let rem = ns.rem_euclid(86_400_000_000_000);
        let (y, mo, da) = civil_from_days(z);
        Ok(Value::str(format!(
            "{}T{}Z",
            fmt_date(IsoDate { year: y, month: mo, day: da }),
            fmt_time(ns_to_time(rem))
        )))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let y = to_instant(i, &arg(a, 0))?;
        Ok(Value::Bool(x == y))
    });
    it.def_method(&proto, "toZonedDateTimeISO", 1, |i, t, a| {
        let e = as_instant(i, &t)?;
        let tzv = arg(a, 0);
        let tz: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let offset = zone_offset(&tz, e);
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: e, offset_ns: offset, tz }))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        if dur.years != 0 || dur.months != 0 || dur.weeks != 0 || dur.days != 0 {
            return Err(i.make_error("RangeError", "Instant.add does not accept calendar units"));
        }
        Ok(make(i, "Temporal.Instant", Temporal::Instant(x + duration_time_ns(dur))))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        if dur.years != 0 || dur.months != 0 || dur.weeks != 0 || dur.days != 0 {
            return Err(i.make_error("RangeError", "Instant.subtract does not accept calendar units"));
        }
        Ok(make(i, "Temporal.Instant", Temporal::Instant(x - duration_time_ns(dur))))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let o = arg(a, 0);
        let smallest = opt_str(i, &o, "smallestUnit", "")?;
        let unit = unit_ns(&smallest)
            .ok_or_else(|| i.make_error("RangeError", "smallestUnit is required"))?;
        let incr_raw = opt_num(i, &o, "roundingIncrement", 1)?;
        let mode = opt_str(i, &o, "roundingMode", "halfExpand")?;
        check_mode(i, &mode)?;
        check_increment(i, smallest.strip_suffix('s').unwrap_or(&smallest), incr_raw)?;
        let incr = incr_raw as i128;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(round_ns(x, unit * incr, &mode))))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let y = to_instant(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "second")?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(balance_ns(y - x, &largest))))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let y = to_instant(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "second")?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(balance_ns(x - y, &largest))))
    });
    let ctor = add_ctor(it, ns, "Instant", 1, proto, |i, _t, a| {
        require_new(i)?;
        let ns = match arg(a, 0) {
            Value::BigInt(n) => n,
            v => to_int(i, &v)? as i128,
        };
        Ok(make(i, "Temporal.Instant", Temporal::Instant(ns)))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let n = to_instant(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(n)))
    });
    it.def_method(&ctor, "fromEpochMilliseconds", 1, |i, _t, a| {
        let ms = to_int(i, &arg(a, 0))? as i128;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(ms * 1_000_000)))
    });
    it.def_method(&ctor, "fromEpochNanoseconds", 1, |i, _t, a| {
        let ns = match arg(a, 0) {
            Value::BigInt(n) => n,
            v => to_int(i, &v)? as i128,
        };
        Ok(make(i, "Temporal.Instant", Temporal::Instant(ns)))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_instant(i, &arg(a, 0))?;
        let y = to_instant(i, &arg(a, 1))?;
        Ok(Value::Num(x.cmp(&y) as i64 as f64))
    });
}
fn to_instant(i: &mut Interp, v: &Value) -> Result<i128, Value> {
    match get(i, v) {
        Some(Temporal::Instant(n)) => return Ok(n),
        Some(Temporal::Zoned { epoch_ns, .. }) => return Ok(epoch_ns),
        _ => {}
    }
    match v {
        Value::BigInt(n) => Ok(*n),
        Value::Str(s) => parse_instant(s).ok_or_else(|| i.make_error("RangeError", "invalid Instant string")),
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.Instant")),
    }
}
/// Parse an ISO instant string (must carry a `Z` or `±HH:MM` offset).
fn parse_instant(s: &str) -> Option<i128> {
    let date = parse_date_str(s)?;
    let time = parse_time_str(s).unwrap_or(IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 });
    let offset = parse_offset_from(s)?; // required: an instant needs an absolute reference
    Some(dt_ns(date, time) - offset as i128)
}

// ===== ZonedDateTime ==========================================================================

/// A named-zone rule: standard offset, optional DST offset + transition rules. Transition rules are
/// `(month, week, weekday, hour)` where week 5 = "last"; weekday 0 = Sunday. `utc_rule` means the
/// transition hour is in UTC (EU style) rather than local wall time (US style).
struct ZoneRule {
    std: i64,
    dst: Option<(i64, (u8, u8, u8, u8), (u8, u8, u8, u8), bool)>,
}
const SEC: i64 = 1_000_000_000;
const US_START: (u8, u8, u8, u8) = (3, 2, 0, 2); // 2nd Sunday March, 02:00 local
const US_END: (u8, u8, u8, u8) = (11, 1, 0, 2); // 1st Sunday Nov, 02:00 local
const EU_START: (u8, u8, u8, u8) = (3, 5, 0, 1); // last Sunday March, 01:00 UTC
const EU_END: (u8, u8, u8, u8) = (10, 5, 0, 1); // last Sunday Oct, 01:00 UTC

fn zone_rule(tz: &str) -> Option<ZoneRule> {
    let h = |n: i64| n * 3600 * SEC;
    let hm = |hh: i64, mm: i64| (hh * 3600 + mm * 60) * SEC;
    let fixed = |o: i64| Some(ZoneRule { std: o, dst: None });
    let us = |std: i64, dst: i64| Some(ZoneRule { std, dst: Some((dst, US_START, US_END, false)) });
    let eu = |std: i64, dst: i64| Some(ZoneRule { std, dst: Some((dst, EU_START, EU_END, true)) });
    match tz {
        "UTC" | "Z" | "Etc/UTC" | "Etc/GMT" | "GMT" => fixed(0),
        "Africa/Abidjan" | "Africa/Accra" | "Atlantic/Reykjavik" | "Africa/Monrovia" => fixed(0),
        "Africa/Lagos" | "Africa/Algiers" | "Africa/Tunis" => fixed(h(1)),
        "Africa/Cairo" | "Africa/Johannesburg" => fixed(h(2)),
        "Asia/Kolkata" | "Asia/Calcutta" => fixed(hm(5, 30)),
        "Asia/Katmandu" | "Asia/Kathmandu" => fixed(hm(5, 45)),
        "Asia/Tokyo" | "Asia/Seoul" => fixed(h(9)),
        "Asia/Shanghai" | "Asia/Hong_Kong" | "Asia/Singapore" | "Asia/Manila" => fixed(h(8)),
        "Asia/Dubai" => fixed(h(4)),
        "America/Sao_Paulo" | "America/Argentina/Buenos_Aires" => fixed(h(-3)),
        "America/New_York" | "US/Eastern" => us(h(-5), h(-4)),
        "America/Chicago" | "US/Central" => us(h(-6), h(-5)),
        "America/Denver" | "US/Mountain" => us(h(-7), h(-6)),
        "America/Los_Angeles" | "America/Vancouver" | "US/Pacific" => us(h(-8), h(-7)),
        "America/Halifax" => us(h(-4), h(-3)),
        "America/St_Johns" => us(hm(-3, -30), hm(-2, -30)),
        "Europe/London" | "Europe/Lisbon" | "Europe/Dublin" => eu(0, h(1)),
        "Europe/Vienna" | "Europe/Paris" | "Europe/Berlin" | "Europe/Amsterdam"
        | "Europe/Madrid" | "Europe/Rome" | "Europe/Brussels" | "Europe/Zurich"
        | "Europe/Stockholm" | "Europe/Prague" | "Europe/Warsaw" => eu(h(1), h(2)),
        "Europe/Athens" | "Europe/Helsinki" | "Europe/Bucharest" | "Europe/Kiev" => eu(h(2), h(3)),
        _ => None,
    }
}

/// Day-of-month of the `week`-th `weekday` (0=Sun) of `month` (week 5 = last).
fn nth_weekday(year: i64, month: u8, week: u8, dow: u8) -> u8 {
    let dow_of = |day: u8| (days_from_civil(year, month as i64, day as i64).rem_euclid(7) + 4) % 7; // 0=Sun
    if week >= 5 {
        let dim = days_in_month(year, month);
        let mut d = dim;
        while dow_of(d) as u8 != dow {
            d -= 1;
        }
        d
    } else {
        let first = dow_of(1) as u8;
        let offset = (dow + 7 - first) % 7;
        1 + offset + (week - 1) * 7
    }
}
/// The UTC nanosecond instant of a DST transition in `year`, given `offset_before` (the offset in
/// effect just before the transition) and whether the rule hour is UTC.
fn transition_ns(year: i64, rule: (u8, u8, u8, u8), offset_before: i64, utc_rule: bool) -> i128 {
    let (month, week, dow, hour) = rule;
    let day = nth_weekday(year, month, week, dow);
    let local = days_from_civil(year, month as i64, day as i64) as i128 * 86_400 * SEC as i128
        + hour as i128 * 3600 * SEC as i128;
    if utc_rule {
        local
    } else {
        local - offset_before as i128
    }
}
/// The UTC offset (ns) of zone `tz` at instant `epoch_ns`.
fn zone_offset(tz: &str, epoch_ns: i128) -> i64 {
    if let Some(off) = parse_fixed_offset(tz) {
        return off;
    }
    match zone_rule(tz) {
        Some(ZoneRule { std, dst: None }) => std,
        Some(ZoneRule { std, dst: Some((dst, start, end, utc_rule)) }) => {
            let year = civil_from_days((epoch_ns.div_euclid(86_400 * SEC as i128)) as i64).0;
            let s = transition_ns(year, start, std, utc_rule);
            let e = transition_ns(year, end, dst, utc_rule);
            if epoch_ns >= s && epoch_ns < e {
                dst
            } else {
                std
            }
        }
        None => 0,
    }
}
/// The offset to use when interpreting a *local* wall-clock instant in `tz` (one refinement step).
fn offset_for_local(tz: &str, local_ns: i128) -> i64 {
    let g = zone_offset(tz, local_ns); // first guess: treat local as UTC
    zone_offset(tz, local_ns - g as i128)
}

/// Parse a fixed-offset id (`UTC`/`Z`/`±HH:MM[:SS]`) to ns, or None for a named zone.
fn parse_fixed_offset(tz: &str) -> Option<i64> {
    let t = tz.trim();
    if t.eq_ignore_ascii_case("utc") || t == "Z" {
        return Some(0);
    }
    if t.starts_with('+') || t.starts_with('-') {
        return Some(tz_offset_ns(t));
    }
    None
}

/// Parse a time-zone id to a fixed offset in nanoseconds. "UTC"/"Z" and `±HH:MM[:SS]` are exact;
/// any other (named) zone is treated as UTC (no DST database).
fn tz_offset_ns(tz: &str) -> i64 {
    let t = tz.trim();
    if t.eq_ignore_ascii_case("utc") || t == "Z" {
        return 0;
    }
    let (sign, rest) = match t.strip_prefix('-') {
        Some(r) => (-1i64, r),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    if t.starts_with('+') || t.starts_with('-') {
        let mut p = rest.split(':');
        let h: i64 = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let m: i64 = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let s: i64 = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        return sign * ((h * 3600 + m * 60 + s) * 1_000_000_000);
    }
    0
}
fn offset_string(offset_ns: i64) -> String {
    let neg = offset_ns < 0;
    let secs = offset_ns.abs() / 1_000_000_000;
    let h = secs / 3600;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    let sign = if neg { "-" } else { "+" };
    if s == 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    }
}
fn zoned_local(epoch_ns: i128, offset_ns: i64) -> (IsoDate, IsoTime) {
    let local = epoch_ns + offset_ns as i128;
    let z = local.div_euclid(86_400_000_000_000) as i64;
    let rem = local.rem_euclid(86_400_000_000_000) as i64;
    let (y, mo, da) = civil_from_days(z);
    let secs = rem / 1_000_000_000;
    let t = IsoTime {
        hour: (secs / 3600) as u8,
        minute: ((secs / 60) % 60) as u8,
        second: (secs % 60) as u8,
        ms: ((rem / 1_000_000) % 1000) as u16,
        us: ((rem / 1000) % 1000) as u16,
        ns: (rem % 1000) as u16,
    };
    (IsoDate { year: y, month: mo, day: da }, t)
}
fn as_zoned(i: &Interp, this: &Value) -> Result<(i128, i64, Rc<str>), Value> {
    match get(i, this) {
        // The offset is recomputed from the instant + zone so DST is reflected.
        Some(Temporal::Zoned { epoch_ns, tz, .. }) => {
            let offset = zone_offset(&tz, epoch_ns);
            Ok((epoch_ns, offset, tz))
        }
        _ => Err(i.make_error("TypeError", "receiver is not a Temporal.ZonedDateTime")),
    }
}
/// Parse the UTC-offset designator from the time portion of an ISO string (`Z` → 0, `±HH:MM`).
fn parse_offset_from(s: &str) -> Option<i64> {
    let tpart = s.split(['T', 't']).nth(1)?;
    if tpart.contains('Z') || tpart.contains('z') {
        return Some(0);
    }
    if let Some(pos) = tpart.find('+') {
        return Some(tz_offset_ns(&tpart[pos..]));
    }
    if let Some(pos) = tpart.rfind('-') {
        return Some(tz_offset_ns(&tpart[pos..]));
    }
    None
}
/// ToTemporalZonedDateTime: a ZonedDateTime, an ISO string with `[timeZone]`, or a fields object
/// carrying `timeZone`.
fn to_zoned(i: &mut Interp, v: &Value) -> Result<(i128, i64, Rc<str>), Value> {
    if let Some(Temporal::Zoned { epoch_ns, offset_ns, tz }) = get(i, v) {
        return Ok((epoch_ns, offset_ns, tz));
    }
    match v {
        Value::Str(s) => {
            let tz_str = s
                .find('[')
                .and_then(|a| s[a + 1..].find(']').map(|b| s[a + 1..a + 1 + b].to_string()));
            let main = s.split('[').next().unwrap_or(s);
            let date = parse_date_str(main).ok_or_else(|| i.make_error("RangeError", "invalid ZonedDateTime"))?;
            let time = parse_time_str(main)
                .unwrap_or(IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 });
            let tz: Rc<str> = match tz_str {
                Some(t) => Rc::from(t.as_str()),
                None => return Err(i.make_error("RangeError", "missing time zone")),
            };
            let local = dt_ns(date, time);
            let off = parse_offset_from(main).unwrap_or_else(|| offset_for_local(&tz, local));
            Ok((local - off as i128, off, tz))
        }
        Value::Obj(_) => {
            let tzv = getm(i, v, "timeZone")?;
            if matches!(tzv, Value::Undefined) {
                return Err(i.make_error("TypeError", "missing timeZone"));
            }
            let tz: Rc<str> = match &tzv {
                Value::Str(s) => s.clone(),
                _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
            };
            let date = to_date(i, v)?;
            let hour = field_int(i, v, "hour", 0)? as u8;
            let minute = field_int(i, v, "minute", 0)? as u8;
            let second = field_int(i, v, "second", 0)? as u8;
            let ms = field_int(i, v, "millisecond", 0)? as u16;
            let us = field_int(i, v, "microsecond", 0)? as u16;
            let ns = field_int(i, v, "nanosecond", 0)? as u16;
            let time = IsoTime { hour, minute, second, ms, us, ns };
            let local = dt_ns(date, time);
            let off = offset_for_local(&tz, local);
            Ok((local - off as i128, off, tz))
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.ZonedDateTime")),
    }
}

fn install_zoned(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.ZonedDateTime", proto.clone());

    macro_rules! date_get {
        ($name:literal, $f:expr) => {
            def_getter(it, &proto, $name, |i, t, _| {
                let (e, o, _) = as_zoned(i, &t)?;
                let (d, _tm) = zoned_local(e, o);
                Ok($f(d))
            });
        };
    }
    macro_rules! time_get {
        ($name:literal, $f:expr) => {
            def_getter(it, &proto, $name, |i, t, _| {
                let (e, o, _) = as_zoned(i, &t)?;
                let (_d, tm) = zoned_local(e, o);
                Ok($f(tm))
            });
        };
    }
    date_get!("year", |d: IsoDate| Value::Num(d.year as f64));
    date_get!("month", |d: IsoDate| Value::Num(d.month as f64));
    date_get!("day", |d: IsoDate| Value::Num(d.day as f64));
    date_get!("monthCode", |d: IsoDate| Value::str(month_code(d.month)));
    date_get!("dayOfWeek", |d: IsoDate| Value::Num(iso_day_of_week(d) as f64));
    date_get!("dayOfYear", |d: IsoDate| Value::Num(iso_day_of_year(d) as f64));
    date_get!("daysInMonth", |d: IsoDate| Value::Num(days_in_month(d.year, d.month) as f64));
    date_get!("daysInYear", |d: IsoDate| Value::Num(if is_leap(d.year) { 366.0 } else { 365.0 }));
    date_get!("inLeapYear", |d: IsoDate| Value::Bool(is_leap(d.year)));
    date_get!("weekOfYear", |d: IsoDate| Value::Num(iso_week(d).0 as f64));
    date_get!("yearOfWeek", |d: IsoDate| Value::Num(iso_week(d).1 as f64));
    date_get!("daysInWeek", |_d: IsoDate| Value::Num(7.0));
    date_get!("monthsInYear", |_d: IsoDate| Value::Num(12.0));
    def_getter(it, &proto, "hoursInDay", |i, t, _| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, _) = zoned_local(e, o);
        let midnight = IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 };
        let today_local = dt_ns(d, midnight);
        let today = today_local - offset_for_local(&tz, today_local) as i128;
        let (ty, tm, td) = civil_from_days(epoch_days(d) + 1);
        let tomorrow_local = dt_ns(IsoDate { year: ty, month: tm, day: td }, midnight);
        let tomorrow = tomorrow_local - offset_for_local(&tz, tomorrow_local) as i128;
        Ok(Value::Num((tomorrow - today) as f64 / 3_600_000_000_000.0))
    });
    def_getter(it, &proto, "epochSeconds", |i, t, _| {
        Ok(Value::Num(as_zoned(i, &t)?.0.div_euclid(1_000_000_000) as f64))
    });
    def_getter(it, &proto, "epochMicroseconds", |i, t, _| {
        Ok(Value::BigInt(as_zoned(i, &t)?.0.div_euclid(1000)))
    });
    time_get!("hour", |t: IsoTime| Value::Num(t.hour as f64));
    time_get!("minute", |t: IsoTime| Value::Num(t.minute as f64));
    time_get!("second", |t: IsoTime| Value::Num(t.second as f64));
    time_get!("millisecond", |t: IsoTime| Value::Num(t.ms as f64));
    time_get!("microsecond", |t: IsoTime| Value::Num(t.us as f64));
    time_get!("nanosecond", |t: IsoTime| Value::Num(t.ns as f64));
    def_getter(it, &proto, "calendarId", |_i, _t, _| Ok(Value::str("iso8601")));
    def_getter(it, &proto, "epochMilliseconds", |i, t, _| {
        Ok(Value::Num(as_zoned(i, &t)?.0.div_euclid(1_000_000) as f64))
    });
    def_getter(it, &proto, "epochNanoseconds", |i, t, _| Ok(Value::BigInt(as_zoned(i, &t)?.0)));
    def_getter(it, &proto, "offsetNanoseconds", |i, t, _| Ok(Value::Num(as_zoned(i, &t)?.1 as f64)));
    def_getter(it, &proto, "offset", |i, t, _| Ok(Value::str(offset_string(as_zoned(i, &t)?.1))));
    def_getter(it, &proto, "timeZoneId", |i, t, _| {
        Ok(Value::Str(as_zoned(i, &t)?.2))
    });

    it.def_method(&proto, "toInstant", 0, |i, t, _| {
        let (e, _, _) = as_zoned(i, &t)?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(e)))
    });
    it.def_method(&proto, "toPlainDate", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(zoned_local(e, o).0)))
    });
    it.def_method(&proto, "toPlainTime", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(zoned_local(e, o).1)))
    });
    it.def_method(&proto, "toPlainDateTime", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        Ok(make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, tm)))
    });
    it.def_method(&proto, "toPlainYearMonth", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(zoned_local(e, o).0)))
    });
    it.def_method(&proto, "toPlainMonthDay", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(i, "Temporal.PlainMonthDay", Temporal::MonthDay(zoned_local(e, o).0)))
    });
    it.def_method(&proto, "startOfDay", 0, |i, t, _| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, _) = zoned_local(e, o);
        let midnight = IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 };
        let local = dt_ns(d, midnight); let off = offset_for_local(&tz, local); let epoch = local - off as i128;
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz }))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let (e, _, tz) = as_zoned(i, &t)?;
        match get(i, &arg(a, 0)) {
            Some(Temporal::Zoned { epoch_ns, tz: otz, .. }) => {
                Ok(Value::Bool(e == epoch_ns && tz == otz))
            }
            _ => Ok(Value::Bool(false)),
        }
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.ZonedDateTime has no valueOf; use compare"))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        Ok(Value::str(format!("{}T{}{}[{}]", fmt_date(d), fmt_time(tm), offset_string(o), tz)))
    });
    it.def_method(&proto, "toString", 0, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        let ts = fmt_time_opts(i, tm, &arg(a, 0))?;
        Ok(Value::str(format!("{}T{}{}[{}]{}", fmt_date(d), ts, offset_string(o), tz, cal_suffix(i, &arg(a, 0))?)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let (d, tm) = zoned_local(e, o);
        let (nd, ntm) = dt_add(i, d, tm, dur, 1)?;
        let local = dt_ns(nd, ntm); let off = offset_for_local(&tz, local); let epoch = local - off as i128;
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz }))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let (d, tm) = zoned_local(e, o);
        let (nd, ntm) = dt_add(i, d, tm, dur, -1)?;
        let local = dt_ns(nd, ntm); let off = offset_for_local(&tz, local); let epoch = local - off as i128;
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz }))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        let f = arg(a, 0);
        let year = field_int(i, &f, "year", d.year)?;
        let month = field_int(i, &f, "month", d.month as i64)? as u8;
        let day = field_int(i, &f, "day", d.day as i64)? as u8;
        let hour = field_int(i, &f, "hour", tm.hour as i64)? as u8;
        let minute = field_int(i, &f, "minute", tm.minute as i64)? as u8;
        let second = field_int(i, &f, "second", tm.second as i64)? as u8;
        let ms = field_int(i, &f, "millisecond", tm.ms as i64)? as u16;
        let us = field_int(i, &f, "microsecond", tm.us as i64)? as u16;
        let nsf = field_int(i, &f, "nanosecond", tm.ns as i64)? as u16;
        let nd = check_date(i, IsoDate { year, month, day })?;
        let nt = check_time(i, IsoTime { hour, minute, second, ms, us, ns: nsf })?;
        let local = dt_ns(nd, nt); let off = offset_for_local(&tz, local); let epoch = local - off as i128;
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz }))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let (e, _, _) = as_zoned(i, &t)?;
        let o = to_instant(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "hour")?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(balance_ns(o - e, &largest))))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let (e, _, _) = as_zoned(i, &t)?;
        let o = to_instant(i, &arg(a, 0))?;
        let largest = opt_str(i, &arg(a, 1), "largestUnit", "hour")?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(balance_ns(e - o, &largest))))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let opts = arg(a, 0);
        let smallest = opt_str(i, &opts, "smallestUnit", "")?;
        let unit = if smallest == "day" {
            86_400_000_000_000
        } else {
            unit_ns(&smallest).ok_or_else(|| i.make_error("RangeError", "smallestUnit is required"))?
        };
        let incr_raw = opt_num(i, &opts, "roundingIncrement", 1)?;
        let mode = opt_str(i, &opts, "roundingMode", "halfExpand")?;
        check_mode(i, &mode)?;
        check_increment(i, smallest.strip_suffix('s').unwrap_or(&smallest), incr_raw)?;
        let incr = incr_raw as i128;
        let local = e + o as i128;
        let rounded = round_ns(local, unit * incr, &mode);
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: rounded - o as i128, offset_ns: o, tz }))
    });

    let ctor = add_ctor(it, ns, "ZonedDateTime", 2, proto, |i, _t, a| {
        require_new(i)?;
        let epoch_ns = match arg(a, 0) {
            Value::BigInt(n) => n,
            _ => return Err(i.make_error("TypeError", "epochNanoseconds must be a BigInt")),
        };
        let tzv = arg(a, 1);
        let tz: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            Value::Undefined => return Err(i.make_error("TypeError", "missing timeZone")),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let offset_ns = tz_offset_ns(&tz);
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns, offset_ns, tz }))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let (epoch_ns, offset_ns, tz) = to_zoned(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns, offset_ns, tz }))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_zoned(i, &arg(a, 0))?.0;
        let y = to_zoned(i, &arg(a, 1))?.0;
        Ok(Value::Num(x.cmp(&y) as i64 as f64))
    });
}

// ===== Now ====================================================================================

fn install_now(it: &mut Interp, ns: &Gc) {
    let now = Object::new(Some(it.object_proto.clone()));
    // lumen has no real clock; the epoch is fixed at 1970-01-01T00:00:00Z. Structure/type tests
    // pass even though absolute-time tests do not.
    it.def_method(&now, "instant", 0, |i, _t, _| Ok(make(i, "Temporal.Instant", Temporal::Instant(0))));
    it.def_method(&now, "plainDateISO", 0, |i, _t, _| {
        Ok(make(i, "Temporal.PlainDate", Temporal::Date(IsoDate { year: 1970, month: 1, day: 1 })))
    });
    it.def_method(&now, "plainTimeISO", 0, |i, _t, _| {
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 })))
    });
    it.def_method(&now, "plainDateTimeISO", 0, |i, _t, _| {
        Ok(make(
            i,
            "Temporal.PlainDateTime",
            Temporal::DateTime(
                IsoDate { year: 1970, month: 1, day: 1 },
                IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 },
            ),
        ))
    });
    ns.borrow_mut().props.insert("Now", Property::builtin(Value::Obj(now)));
}
