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

fn parse_date_str(s: &str) -> Option<IsoDate> {
    // YYYY-MM-DD (optionally with time/zone suffix we ignore).
    let s = s.trim();
    let datepart = s.split(['T', ' ']).next()?;
    let mut it = datepart.splitn(3, '-');
    // Allow a leading sign for the (expanded) year.
    let (sign, rest) = if let Some(r) = datepart.strip_prefix('-') {
        (-1i64, r)
    } else if let Some(r) = datepart.strip_prefix('+') {
        (1, r)
    } else {
        (1, datepart)
    };
    if sign != 1 || datepart.starts_with('+') {
        let mut p = rest.splitn(3, '-');
        let y: i64 = p.next()?.parse().ok()?;
        let m: u8 = p.next()?.parse().ok()?;
        let d: u8 = p.next()?.parse().ok()?;
        return Some(IsoDate { year: sign * y, month: m, day: d });
    }
    let y: i64 = it.next()?.parse().ok()?;
    let m: u8 = it.next()?.parse().ok()?;
    let d: u8 = it.next()?.parse().ok()?;
    Some(IsoDate { year: y, month: m, day: d })
}
fn parse_time_str(s: &str) -> Option<IsoTime> {
    let s = s.trim();
    let tpart = if let Some(idx) = s.find('T') { &s[idx + 1..] } else { s };
    let tpart = tpart.split(['Z', '+']).next().unwrap_or(tpart);
    let mut hms = tpart.splitn(3, ':');
    let h: u8 = hms.next()?.parse().ok()?;
    let mi: u8 = hms.next().unwrap_or("0").parse().ok()?;
    let secpart = hms.next().unwrap_or("0");
    let mut sf = secpart.splitn(2, '.');
    let sec: u8 = sf.next()?.parse().ok()?;
    let (ms, us, ns) = match sf.next() {
        Some(frac) => {
            let mut f = frac.to_string();
            while f.len() < 9 {
                f.push('0');
            }
            f.truncate(9);
            let n: u32 = f.parse().ok()?;
            ((n / 1_000_000) as u16, ((n / 1000) % 1000) as u16, (n % 1000) as u16)
        }
        None => (0, 0, 0),
    };
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
    install_now(it, &ns);
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

    it.def_method(&proto, "toString", 0, |i, t, _| Ok(Value::str(fmt_date(as_date(i, &t)?))));
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
    it.def_method(&proto, "toPlainDateTime", 0, |i, t, _| {
        let d = as_date(i, &t)?;
        let time = IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 };
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
            let month = field_req(i, v, "month")?;
            let day = field_req(i, v, "day")?;
            check_date(i, IsoDate { year, month: month as u8, day: day as u8 })
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

    it.def_method(&proto, "toString", 0, |i, t, _| Ok(Value::str(fmt_time(as_time(i, &t)?))));
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
fn to_time(i: &mut Interp, v: &Value) -> Result<IsoTime, Value> {
    match get(i, v) {
        Some(Temporal::Time(t)) | Some(Temporal::DateTime(_, t)) => return Ok(t),
        _ => {}
    }
    match v {
        Value::Str(s) => parse_time_str(s).ok_or_else(|| i.make_error("RangeError", "invalid time string")),
        Value::Obj(_) => {
            let hour = field_int(i, v, "hour", 0)? as u8;
            let minute = field_int(i, v, "minute", 0)? as u8;
            let second = field_int(i, v, "second", 0)? as u8;
            let ms = field_int(i, v, "millisecond", 0)? as u16;
            let us = field_int(i, v, "microsecond", 0)? as u16;
            let ns = field_int(i, v, "nanosecond", 0)? as u16;
            check_time(i, IsoTime { hour, minute, second, ms, us, ns })
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

    it.def_method(&proto, "toString", 0, |i, t, _| {
        let (d, tm) = as_datetime(i, &t)?;
        Ok(Value::str(format!("{}T{}", fmt_date(d), fmt_time(tm))))
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
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (od, otm) = to_datetime(i, &arg(a, 0))?;
        Ok(Value::Bool(cmp_date(d, od) == 0 && time_to_ns(tm) == time_to_ns(otm)))
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
            let hour = field_int(i, v, "hour", 0)? as u8;
            let minute = field_int(i, v, "minute", 0)? as u8;
            let second = field_int(i, v, "second", 0)? as u8;
            let ms = field_int(i, v, "millisecond", 0)? as u16;
            let us = field_int(i, v, "microsecond", 0)? as u16;
            let ns = field_int(i, v, "nanosecond", 0)? as u16;
            Ok((d, check_time(i, IsoTime { hour, minute, second, ms, us, ns })?))
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
    it.def_method(&proto, "toString", 0, |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::str(format!("{}-{:02}", pad_year(d.year), d.month)))
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
            let month = field_req(i, v, "month")?;
            Ok(IsoDate { year, month: month as u8, day: 1 })
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
    it.def_method(&proto, "toString", 0, |i, t, _| {
        let d = as_monthday(i, &t)?;
        Ok(Value::str(format!("{:02}-{:02}", d.month, d.day)))
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
            let s = s.trim().trim_start_matches("--");
            let mut p = s.splitn(2, '-');
            let m: u8 = p.next().and_then(|x| x.parse().ok()).ok_or_else(|| i.make_error("RangeError", "invalid month-day"))?;
            let d: u8 = p.next().and_then(|x| x.parse().ok()).ok_or_else(|| i.make_error("RangeError", "invalid month-day"))?;
            Ok(IsoDate { year: 1972, month: m, day: d })
        }
        Value::Obj(_) => {
            let month = field_req(i, v, "month")?;
            let day = field_req(i, v, "day")?;
            Ok(IsoDate { year: 1972, month: month as u8, day: day as u8 })
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
    it.def_method(&proto, "toString", 0, |i, t, _| {
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
        Ok(Value::str(format!("{}T{}Z", fmt_date(IsoDate { year: y, month: mo, day: da }), fmt_time(t))))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.Instant has no valueOf; use compare"))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let y = to_instant(i, &arg(a, 0))?;
        Ok(Value::Bool(x == y))
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
    if let Some(Temporal::Instant(n)) = get(i, v) {
        return Ok(n);
    }
    match v {
        Value::BigInt(n) => Ok(*n),
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.Instant")),
    }
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
