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
    Zoned {
        epoch_ns: i128,
        offset_ns: i64,
        tz: Rc<str>,
    },
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

// Temporal's representable range: epoch nanoseconds in ±(8.64e21), with a one-day buffer so any
// wall-clock time on a boundary date stays in range. ISODateTimeWithinLimits checks the instant.
const NS_MAX_INSTANT: i128 = 8_640_000_000_000_000_000_000; // 8.64e21
const NS_PER_DAY: i128 = 86_400_000_000_000;

/// Whether a date (checked at noon, per ISODateWithinLimits) is representable. The coarse year guard
/// also keeps `epoch_days` from overflowing i64 for absurd years.
fn iso_date_within_limits(d: IsoDate) -> bool {
    if d.year < -271_821 || d.year > 275_760 {
        return false;
    }
    let ns = epoch_days(d) as i128 * NS_PER_DAY + NS_PER_DAY / 2;
    ns > -NS_MAX_INSTANT - NS_PER_DAY && ns < NS_MAX_INSTANT + NS_PER_DAY
}
/// ISODateTimeWithinLimits: the actual date+time instant must lie within ±(8.64e21 + one day) ns.
fn iso_datetime_within_limits(d: IsoDate, t: IsoTime) -> bool {
    if d.year < -271_821 || d.year > 275_760 {
        return false;
    }
    let ns = dt_ns(d, t);
    ns > -NS_MAX_INSTANT - NS_PER_DAY && ns < NS_MAX_INSTANT + NS_PER_DAY
}
/// ISOYearMonthWithinLimits: a (year, month) is representable (month-granularity bounds).
fn iso_year_month_within_limits(year: i64, month: i64) -> bool {
    if !(-271_821..=275_760).contains(&year) {
        return false;
    }
    if year == -271_821 && month < 4 {
        return false;
    }
    if year == 275_760 && month > 9 {
        return false;
    }
    true
}

fn check_date(i: &Interp, d: IsoDate) -> Result<IsoDate, Value> {
    if !(1..=12).contains(&d.month) || d.day < 1 || d.day > days_in_month(d.year, d.month) {
        return Err(i.make_error("RangeError", "invalid ISO date"));
    }
    if !iso_date_within_limits(d) {
        return Err(i.make_error("RangeError", "date is outside the supported range"));
    }
    Ok(d)
}
fn check_time(i: &Interp, t: IsoTime) -> Result<IsoTime, Value> {
    if t.hour > 23 || t.minute > 59 || t.second > 59 || t.ms > 999 || t.us > 999 || t.ns > 999 {
        return Err(i.make_error("RangeError", "invalid ISO time"));
    }
    Ok(t)
}
/// Range-check raw (possibly out-of-range) integer date fields *before* narrowing, then validate the
/// ISO date and its representable range. Avoids the silent wrap of casting e.g. day=257 to `1u8`.
fn build_date(i: &Interp, year: i64, month: i64, day: i64) -> Result<IsoDate, Value> {
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month as u8) as i64 {
        return Err(i.make_error("RangeError", "ISO date field is out of range"));
    }
    check_date(
        i,
        IsoDate {
            year,
            month: month as u8,
            day: day as u8,
        },
    )
}
/// Build a date honoring `overflow`: `reject` range-checks (as [`build_date`]); `constrain` clamps
/// month into 1..12 and day into the target month's valid range.
fn build_date_ovf(
    i: &Interp,
    year: i64,
    month: i64,
    day: i64,
    ovf: Overflow,
) -> Result<IsoDate, Value> {
    match ovf {
        Overflow::Reject => build_date(i, year, month, day),
        Overflow::Constrain => {
            let m = month.clamp(1, 12);
            let d = day.clamp(1, days_in_month(year, m as u8) as i64);
            check_date(
                i,
                IsoDate {
                    year,
                    month: m as u8,
                    day: d as u8,
                },
            )
        }
    }
}
/// Range-check raw integer time fields before narrowing (RejectTime semantics).
fn build_time(
    i: &Interp,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
    us: i64,
    ns: i64,
) -> Result<IsoTime, Value> {
    if !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
        || !(0..=999).contains(&ms)
        || !(0..=999).contains(&us)
        || !(0..=999).contains(&ns)
    {
        return Err(i.make_error("RangeError", "ISO time field is out of range"));
    }
    Ok(IsoTime {
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        ms: ms as u16,
        us: us as u16,
        ns: ns as u16,
    })
}
/// Build a time honoring `overflow`: `reject` range-checks (as [`build_time`]); `constrain` clamps
/// each field into its valid range.
#[allow(clippy::too_many_arguments)]
fn build_time_ovf(
    i: &Interp,
    hour: i64,
    minute: i64,
    second: i64,
    ms: i64,
    us: i64,
    ns: i64,
    ovf: Overflow,
) -> Result<IsoTime, Value> {
    match ovf {
        Overflow::Reject => build_time(i, hour, minute, second, ms, us, ns),
        Overflow::Constrain => Ok(IsoTime {
            hour: hour.clamp(0, 23) as u8,
            minute: minute.clamp(0, 59) as u8,
            second: second.clamp(0, 59) as u8,
            ms: ms.clamp(0, 999) as u16,
            us: us.clamp(0, 999) as u16,
            ns: ns.clamp(0, 999) as u16,
        }),
    }
}
/// The recognised BCP-47 calendar identifiers. Date arithmetic uses the proleptic ISO/Gregorian
/// calendar for all of them (exact for iso8601/gregory; the others are accepted so construction and
/// `calendarId` round-trip, even though their field output is not yet calendar-specific).
// The calendars Temporal itself accepts. Note "islamic", "islamic-rgsa", and "islamicc" are NOT
// here: "islamicc" is only an alias (→ islamic-civil), and "islamic"/"islamic-rgsa" are recognized
// solely by Intl.DateTimeFormat, so Temporal rejects them.
const KNOWN_CALENDARS: &[&str] = &[
    "iso8601", "gregory", "buddhist", "chinese", "coptic", "dangi", "ethioaa", "ethiopic",
    "hebrew", "indian", "islamic-umalqura", "islamic-tbla", "islamic-civil", "japanese", "persian",
    "roc",
];

/// Validate a constructor's calendar argument and return its canonical id. `undefined` → "iso8601".
/// A non-string (other than undefined) is a TypeError; an unknown id is a RangeError.
fn check_calendar(i: &mut Interp, v: &Value) -> Result<std::rc::Rc<str>, Value> {
    match v {
        Value::Undefined => Ok(std::rc::Rc::from("iso8601")),
        Value::Str(s) => {
            // A bare (CLDR-aliased) calendar id...
            if let Some(canon) = canon_cal(s) {
                return Ok(std::rc::Rc::from(canon));
            }
            // ...or an ISO date/time whose `[u-ca=...]` annotation (or the implicit "iso8601")
            // supplies the calendar.
            if let Some(parsed) = parse_iso(s) {
                let cal = parsed.calendar.unwrap_or_else(|| "iso8601".to_string());
                if let Some(canon) = canon_cal(&cal) {
                    return Ok(std::rc::Rc::from(canon));
                }
            }
            Err(i.make_error("RangeError", "invalid calendar identifier"))
        }
        _ => Err(i.make_error("TypeError", "calendar must be a string")),
    }
}

/// The (era code, era year) for a proleptic-Gregorian year in a given calendar. Only the era-based
/// calendars (gregory/japanese) expose an era here; others (incl. iso8601) return `(None, None)`.
/// (era, eraYear) for an arithmetic calendar at an ISO date. `None` for calendars without eras.
fn cal_era(cal: &str, d: IsoDate) -> (Option<&'static str>, Option<i64>) {
    match cal {
        "gregory" => {
            if d.year >= 1 {
                (Some("ce"), Some(d.year))
            } else {
                (Some("bce"), Some(1 - d.year))
            }
        }
        "japanese" => japanese_era(d),
        "buddhist" => (Some("be"), Some(d.year + 543)),
        "roc" => {
            let y = d.year - 1911;
            if y >= 1 {
                (Some("roc"), Some(y))
            } else {
                (Some("broc"), Some(1 - y))
            }
        }
        "coptic" => (Some("am"), Some(from_13month(d, COPTIC_EPOCH).0)),
        "ethiopic" => {
            // Amete-mihret for positive years; the amete-alem era (+5500) covers year <= 0.
            let am = from_13month(d, ETHIOPIC_EPOCH).0;
            if am >= 1 {
                (Some("am"), Some(am))
            } else {
                (Some("aa"), Some(am + 5500))
            }
        }
        "ethioaa" => (Some("aa"), Some(from_13month(d, ETHIOPIC_EPOCH).0 + 5500)),
        _ if is_islamic(cal) => {
            let y = isl_from(cal, d).0;
            if y >= 1 {
                (Some("ah"), Some(y))
            } else {
                (Some("bh"), Some(1 - y))
            }
        }
        "indian" => (Some("shaka"), Some(from_indian(d).0)),
        "hebrew" => (Some("am"), Some(hebrew_from_iso(d).0)),
        // Persian has a single proleptic era "ap"; eraYear equals the year even when non-positive.
        "persian" => (Some("ap"), Some(from_persian(d).0)),
        _ => (None, None),
    }
}

/// The Japanese era (and 1-based era year) in effect on an ISO date; pre-Meiji falls back to
/// gregorian ce/bce. Era-start dates per the modern Japanese calendar.
fn japanese_era(d: IsoDate) -> (Option<&'static str>, Option<i64>) {
    let e = epoch_days(d);
    let start = |y, m, day| days_from_civil(y, m, day);
    if e >= start(2019, 5, 1) {
        (Some("reiwa"), Some(d.year - 2019 + 1))
    } else if e >= start(1989, 1, 8) {
        (Some("heisei"), Some(d.year - 1989 + 1))
    } else if e >= start(1926, 12, 25) {
        (Some("showa"), Some(d.year - 1926 + 1))
    } else if e >= start(1912, 7, 30) {
        (Some("taisho"), Some(d.year - 1912 + 1))
    } else if e >= start(1873, 1, 1) {
        // ICU's Japanese calendar represents dates before Meiji 6 (1873, when Japan adopted the
        // Gregorian calendar) with the proleptic "ce"/"bce" eras, not "meiji".
        (Some("meiji"), Some(d.year - 1868 + 1))
    } else if d.year >= 1 {
        (Some("ce"), Some(d.year))
    } else {
        (Some("bce"), Some(1 - d.year))
    }
}

// epoch_days (days from 1970-01-01, proleptic Gregorian) of each 13-month calendar's year 1-1-1.
const COPTIC_EPOCH: i64 = -615558; // Coptic 1-01-01 = Julian 284-08-29
const ETHIOPIC_EPOCH: i64 = -716367; // Ethiopic (amete mihret) 1-01-01 = Julian 8-08-29

fn is_13month(cal: &str) -> bool {
    matches!(cal, "coptic" | "ethiopic" | "ethioaa")
}

// Tabular Islamic ("civil"/Kuwaiti) calendar. epoch_days of AH 1-01-01: civil = Julian 622-07-16,
// astronomical (tbla) = 622-07-15. Observational variants are approximated by the civil algorithm.
const ISLAMIC_EPOCH: i64 = -492148;
fn is_islamic(cal: &str) -> bool {
    cal.starts_with("islamic")
}
fn islamic_epoch(cal: &str) -> i64 {
    if cal == "islamic-tbla" {
        ISLAMIC_EPOCH - 1
    } else {
        ISLAMIC_EPOCH
    }
}
fn islamic_leap(y: i64) -> bool {
    (14 + 11 * y).rem_euclid(30) < 11
}
fn islamic_month_len(m: i64, leap: bool) -> i64 {
    if m % 2 == 1 {
        30
    } else if m == 12 && leap {
        30
    } else {
        29
    }
}
/// Tabular Islamic date from an ISO date: (year, month, day).
fn from_islamic(iso: IsoDate, epoch: i64) -> (i64, i64, i64) {
    let fixed = epoch_days(iso);
    let year = (30 * (fixed - epoch) + 10646).div_euclid(10631);
    let year_start = epoch + (year - 1) * 354 + (3 + 11 * year).div_euclid(30);
    let mut rem = fixed - year_start; // 0-based day of year
    let leap = islamic_leap(year);
    let mut month = 1;
    while month < 12 {
        let ml = islamic_month_len(month, leap);
        if rem < ml {
            break;
        }
        rem -= ml;
        month += 1;
    }
    (year, month, rem + 1)
}
fn to_islamic(y: i64, m: i64, d: i64, epoch: i64) -> IsoDate {
    let fixed = epoch + (y - 1) * 354 + (3 + 11 * y).div_euclid(30) + (m - 1) * 29 + m.div_euclid(2) + (d - 1);
    let (yy, mm, dd) = civil_from_days(fixed);
    IsoDate { year: yy, month: mm, day: dd }
}

// --- Umm al-Qura (islamic-umalqura): table-based for AH 1300-1600 (ICU data), else tabular civil.
fn umalqura_in_range(y: i64) -> bool {
    y >= crate::umalqura::YEAR_START as i64 && y <= crate::umalqura::YEAR_END as i64
}
/// Length (29/30) of Islamic month `m` (1-based) in year `y` under Umm al-Qura.
fn umalqura_month_len(y: i64, m: i64) -> i64 {
    if umalqura_in_range(y) {
        let bits = crate::umalqura::MONTH_LENGTHS[(y - crate::umalqura::YEAR_START as i64) as usize];
        if bits & (1 << (12 - m)) != 0 { 30 } else { 29 }
    } else {
        islamic_month_len(m, islamic_leap(y))
    }
}
/// Epoch-day of the first day of Umm al-Qura year `y`.
fn umalqura_year_start(y: i64) -> i64 {
    if umalqura_in_range(y) {
        crate::umalqura::YEAR_STARTS[(y - crate::umalqura::YEAR_START as i64) as usize] + crate::umalqura::EPOCH_OFFSET
    } else {
        epoch_days(to_islamic(y, 1, 1, ISLAMIC_EPOCH))
    }
}
fn from_umalqura(iso: IsoDate) -> (i64, i64, i64) {
    let ed = epoch_days(iso);
    let mut y = 1300 + ((ed - umalqura_year_start(1300)) as f64 / 354.367) as i64;
    while umalqura_year_start(y) > ed {
        y -= 1;
    }
    while umalqura_year_start(y + 1) <= ed {
        y += 1;
    }
    let mut rem = ed - umalqura_year_start(y);
    let mut m = 1;
    while m < 12 {
        let ml = umalqura_month_len(y, m);
        if rem < ml {
            break;
        }
        rem -= ml;
        m += 1;
    }
    (y, m, rem + 1)
}
fn to_umalqura(y: i64, m: i64, d: i64) -> IsoDate {
    let off: i64 = (1..m).map(|mm| umalqura_month_len(y, mm)).sum::<i64>() + (d - 1);
    let (yy, mm, dd) = civil_from_days(umalqura_year_start(y) + off);
    IsoDate { year: yy, month: mm, day: dd }
}
fn umalqura_year_len(y: i64) -> i64 {
    (1..=12).map(|m| umalqura_month_len(y, m)).sum()
}
/// Calendar-aware Islamic conversions: `islamic-umalqura` uses the ICU table, others are tabular.
fn isl_from(cal: &str, iso: IsoDate) -> (i64, i64, i64) {
    if cal == "islamic-umalqura" {
        from_umalqura(iso)
    } else {
        from_islamic(iso, islamic_epoch(cal))
    }
}
fn isl_to(cal: &str, y: i64, m: i64, d: i64) -> IsoDate {
    if cal == "islamic-umalqura" {
        to_umalqura(y, m, d)
    } else {
        to_islamic(y, m, d, islamic_epoch(cal))
    }
}
fn isl_month_len(cal: &str, y: i64, m: i64) -> i64 {
    if cal == "islamic-umalqura" {
        umalqura_month_len(y, m)
    } else {
        islamic_month_len(m, islamic_leap(y))
    }
}

// ===== Hebrew (lunisolar) calendar ============================================================
// Reingold/Dershowitz arithmetic. Works in RD (days from proleptic-Gregorian 0001-01-01);
// RD = epoch_days + 719163 (epoch_days of 0001-01-01 is -719162).
const RD_OFFSET: i64 = 719163;
const HEBREW_EPOCH: i64 = -1373427; // RD of Hebrew 1-01-01 (Tishri 1, year 1)
fn hebrew_leap(y: i64) -> bool {
    (7 * y + 1).rem_euclid(19) < 7
}
fn hebrew_months_in_year(y: i64) -> i64 {
    if hebrew_leap(y) { 13 } else { 12 }
}
fn hebrew_elapsed_days(y: i64) -> i64 {
    let months = (235 * y - 234).div_euclid(19);
    let parts = 12084 + 13753 * months;
    let day = 29 * months + parts.div_euclid(25920);
    if (3 * (day + 1)).rem_euclid(7) < 3 {
        day + 1
    } else {
        day
    }
}
fn hebrew_new_year(y: i64) -> i64 {
    // RD of Tishri 1, applying the year-length postponements.
    let ed = hebrew_elapsed_days(y);
    let corr = {
        let ny0 = hebrew_elapsed_days(y - 1);
        let ny2 = hebrew_elapsed_days(y + 1);
        if ny2 - ed == 356 {
            2
        } else if ed - ny0 == 382 {
            1
        } else {
            0
        }
    };
    HEBREW_EPOCH + ed + corr
}
fn hebrew_year_len(y: i64) -> i64 {
    hebrew_new_year(y + 1) - hebrew_new_year(y)
}
/// Length of the `ordinal`-th Hebrew month (Temporal numbering: 1=Tishri) in year `y`.
fn hebrew_month_len(y: i64, ordinal: i64) -> i64 {
    let leap = hebrew_leap(y);
    let yl = hebrew_year_len(y);
    match ordinal {
        1 => 30,                                          // Tishri
        2 => if yl % 10 == 5 { 30 } else { 29 },          // Marheshvan (30 iff complete year)
        3 => if yl % 10 == 3 { 29 } else { 30 },          // Kislev (29 iff deficient year)
        4 => 29,                                          // Tevet
        5 => 30,                                          // Shevat
        6 => if leap { 30 } else { 29 },                  // Adar I (leap) / Adar (common)
        7 => if leap { 29 } else { 30 },                  // Adar II (leap) / Nisan (common)
        // From here the alternation continues 30/29; in a leap year everything is shifted by one.
        n => {
            let m = if leap { n - 1 } else { n }; // map to common-year position (7=Nisan..12=Elul)
            if m % 2 == 1 { 30 } else { 29 }
        }
    }
}
fn hebrew_from_iso(iso: IsoDate) -> (i64, i64, i64) {
    let fixed = epoch_days(iso) + RD_OFFSET;
    let mut y = ((fixed - HEBREW_EPOCH) as f64 / 365.25).floor() as i64 + 1;
    while hebrew_new_year(y) > fixed {
        y -= 1;
    }
    while hebrew_new_year(y + 1) <= fixed {
        y += 1;
    }
    let mut rem = fixed - hebrew_new_year(y);
    let mut m = 1;
    while m < hebrew_months_in_year(y) {
        let ml = hebrew_month_len(y, m);
        if rem < ml {
            break;
        }
        rem -= ml;
        m += 1;
    }
    (y, m, rem + 1)
}
fn hebrew_to_iso(y: i64, ordinal: i64, day: i64) -> IsoDate {
    let mut fixed = hebrew_new_year(y) + (day - 1);
    for m in 1..ordinal {
        fixed += hebrew_month_len(y, m);
    }
    let (yy, mm, dd) = civil_from_days(fixed - RD_OFFSET);
    IsoDate { year: yy, month: mm, day: dd }
}
/// The Hebrew monthCode for an ordinal month: the leap month (Adar I, ordinal 6 in a leap year) is
/// "M05L"; later leap-year months shift back by one so codes stay stable across years.
fn hebrew_month_code(y: i64, ordinal: i64) -> String {
    if hebrew_leap(y) {
        if ordinal <= 5 {
            format!("M{ordinal:02}")
        } else if ordinal == 6 {
            "M05L".to_string()
        } else {
            format!("M{:02}", ordinal - 1)
        }
    } else {
        format!("M{ordinal:02}")
    }
}
/// The ordinal Hebrew month for a monthCode in year `y` (None if not valid that year).
fn hebrew_ord_from_code(y: i64, code: &str) -> Option<i64> {
    let leap = hebrew_leap(y);
    if code == "M05L" {
        return if leap { Some(6) } else { None };
    }
    let n: i64 = code.strip_prefix('M')?.parse().ok()?;
    if !(1..=12).contains(&n) {
        return None;
    }
    Some(if leap && n >= 6 { n + 1 } else { n })
}

// ===== Persian (Solar Hijri, astronomical) ====================================================
// The Iranian national calendar: the year begins on the day (Tehran local time) of the March
// equinox — Nowruz is that day if the equinox falls before local noon, else the next day. The
// equinox instant is computed with Meeus' periodic formula (accurate to seconds for 1000–3000 CE)
// and reduced to universal time via ΔT (Espenak–Meeus polynomials).
fn delta_t_seconds(year: i64) -> f64 {
    let y = year as f64;
    if (2005..=2050).contains(&year) {
        let t = y - 2000.0;
        62.92 + 0.32217 * t + 0.005589 * t * t
    } else if (1986..2005).contains(&year) {
        let t = y - 2000.0;
        63.86 + 0.3345 * t - 0.060374 * t.powi(2) + 0.0017275 * t.powi(3)
            + 0.000651814 * t.powi(4) + 0.00002373599 * t.powi(5)
    } else if (1961..1986).contains(&year) {
        let t = y - 1975.0;
        45.45 + 1.067 * t - t * t / 260.0 - t.powi(3) / 718.0
    } else if (1941..1961).contains(&year) {
        let t = y - 1950.0;
        29.07 + 0.407 * t - t * t / 233.0 + t.powi(3) / 2547.0
    } else if (1920..1941).contains(&year) {
        let t = y - 1920.0;
        21.20 + 0.84493 * t - 0.076100 * t * t + 0.0020936 * t.powi(3)
    } else if (1900..1920).contains(&year) {
        let t = y - 1900.0;
        -2.79 + 1.494119 * t - 0.0598939 * t * t + 0.0061966 * t.powi(3) - 0.000197 * t.powi(4)
    } else if (1860..1900).contains(&year) {
        let t = y - 1860.0;
        7.62 + 0.5737 * t - 0.251754 * t * t + 0.01680668 * t.powi(3) - 0.0004473624 * t.powi(4)
            + t.powi(5) / 233174.0
    } else if (1800..1860).contains(&year) {
        let t = y - 1800.0;
        13.72 - 0.332447 * t + 0.0068612 * t * t + 0.0041116 * t.powi(3) - 0.00037436 * t.powi(4)
            + 0.0000121272 * t.powi(5) - 0.0000001699 * t.powi(6) + 0.000000000875 * t.powi(7)
    } else {
        // Coarse fallback (Espenak) for years outside the fitted range.
        let u = (y - 1820.0) / 100.0;
        -20.0 + 32.0 * u * u
    }
}
/// The March-equinox instant of a Gregorian year as a Julian Ephemeris Day (dynamical time).
fn march_equinox_jde(year: i64) -> f64 {
    let yy = (year as f64 - 2000.0) / 1000.0;
    let jde0 = 2451623.80984 + 365242.37404 * yy + 0.05169 * yy.powi(2)
        - 0.00411 * yy.powi(3) - 0.00057 * yy.powi(4);
    let t = (jde0 - 2451545.0) / 36525.0;
    let w = (35999.373 * t - 2.47).to_radians();
    let dl = 1.0 + 0.0334 * w.cos() + 0.0007 * (2.0 * w).cos();
    // Meeus Table 27.A periodic terms (A, B, C); S = Σ A·cos(B + C·T) in degrees.
    const TERMS: [(f64, f64, f64); 24] = [
        (485.0, 324.96, 1934.136), (203.0, 337.23, 32964.467), (199.0, 342.08, 20.186),
        (182.0, 27.85, 445267.112), (156.0, 73.14, 45036.886), (136.0, 171.52, 22518.443),
        (77.0, 222.54, 65928.934), (74.0, 296.72, 3034.906), (70.0, 243.58, 9037.513),
        (58.0, 119.81, 33718.147), (52.0, 297.17, 150.678), (50.0, 21.02, 2281.226),
        (45.0, 247.54, 29929.562), (44.0, 325.15, 31555.956), (29.0, 60.93, 4443.417),
        (18.0, 155.12, 67555.328), (17.0, 288.79, 4562.452), (16.0, 198.04, 62894.029),
        (14.0, 199.76, 31436.921), (12.0, 95.39, 14577.848), (12.0, 287.11, 31931.756),
        (12.0, 320.81, 34777.259), (9.0, 227.73, 1222.114), (8.0, 15.45, 16859.074),
    ];
    let s: f64 = TERMS.iter().map(|(a, b, c)| a * (b + c * t).to_radians().cos()).sum();
    jde0 + (0.00001 * s) / dl
}
/// The epoch-day (days from 1970-01-01) of Nowruz for a Persian year.
fn persian_new_year(py: i64) -> i64 {
    let greg_year = py + 621;
    let jde = march_equinox_jde(greg_year);
    let ut = jde - delta_t_seconds(greg_year) / 86400.0; // JD in universal time
    let tehran = ut + 3.5 / 24.0; // Tehran standard time (UTC+3:30)
    let ed = tehran - 2440587.5; // epoch-days as a real moment
    let day = ed.floor() as i64;
    if ed - day as f64 <= 0.5 {
        day // equinox before local noon → Nowruz today
    } else {
        day + 1
    }
}
fn persian_leap(py: i64) -> bool {
    persian_new_year(py + 1) - persian_new_year(py) == 366
}
fn persian_month_len(py: i64, m: i64) -> i64 {
    if m <= 6 {
        31
    } else if m <= 11 {
        30
    } else if persian_leap(py) {
        30
    } else {
        29
    }
}
fn from_persian(iso: IsoDate) -> (i64, i64, i64) {
    let ed = epoch_days(iso);
    let mut py = iso.year - 621;
    while persian_new_year(py) > ed {
        py -= 1;
    }
    while persian_new_year(py + 1) <= ed {
        py += 1;
    }
    let doy = ed - persian_new_year(py); // 0-based
    let (month, mstart) = if doy < 186 {
        (doy / 31 + 1, (doy / 31) * 31)
    } else {
        let m = (doy - 186) / 30 + 7;
        (m, 186 + (m - 7) * 30)
    };
    (py, month, doy - mstart + 1)
}
fn to_persian(py: i64, m: i64, d: i64) -> IsoDate {
    let offset = if m <= 7 { 31 * (m - 1) } else { 30 * (m - 1) + 6 };
    let (yy, mm, dd) = civil_from_days(persian_new_year(py) + offset + (d - 1));
    IsoDate { year: yy, month: mm, day: dd }
}

// ===== Astronomy (from scratch) for the Chinese/Dangi lunisolar calendars ======================
// Universal-time moments are real numbers of days from 1970-01-01T00:00Z (an "epoch-day moment").
const MEAN_TROPICAL_YEAR: f64 = 365.242189;
const MEAN_SYNODIC_MONTH: f64 = 29.530588861;

fn julian_centuries_tt(ed: f64) -> f64 {
    let (gy, _, _) = civil_from_days(ed.floor() as i64);
    let jd_tt = ed + 2440587.5 + delta_t_seconds(gy) / 86400.0;
    (jd_tt - 2451545.0) / 36525.0
}
/// The sun's apparent longitude (degrees) at a universal-time moment (Meeus/Calendrical-Calc series).
fn solar_longitude(ed: f64) -> f64 {
    let c = julian_centuries_tt(ed);
    const COEF: [f64; 49] = [
        403406., 195207., 119433., 112392., 3891., 2819., 1721., 660., 350., 334., 314., 268.,
        242., 234., 158., 132., 129., 114., 99., 93., 86., 78., 72., 68., 64., 46., 38., 37., 32.,
        29., 28., 27., 27., 25., 24., 21., 21., 20., 18., 17., 14., 13., 13., 13., 12., 10., 10.,
        10., 10.,
    ];
    const MULT: [f64; 49] = [
        0.9287892, 35999.1376958, 35999.4089666, 35998.7287385, 71998.20261, 71998.4403,
        36000.35726, 71997.4812, 32964.4678, -19.4410, 445267.1117, 45036.8840, 3.1008,
        22518.4434, -19.9739, 65928.9345, 9038.0293, 3034.7684, 33718.148, 3034.448, -2280.773,
        29929.992, 31556.493, 149.588, 9037.750, 107997.405, -4444.176, 151.771, 67555.316,
        31556.080, -4561.540, 107996.706, 1221.655, 62894.167, 31437.369, 14578.298, -31931.757,
        34777.243, 1221.999, 62894.511, -4442.039, 107997.909, 119.066, 16859.071, -4.578,
        26895.292, -39.127, 12297.536, 90073.778,
    ];
    const ADD: [f64; 49] = [
        270.54861, 340.19128, 63.91854, 331.26220, 317.843, 86.631, 240.052, 310.26, 247.23,
        260.87, 297.82, 343.14, 166.79, 81.53, 3.50, 132.75, 182.95, 162.03, 29.8, 266.4, 249.2,
        157.6, 257.8, 185.1, 69.9, 8.0, 197.1, 250.4, 65.3, 162.7, 341.5, 291.6, 98.5, 146.7,
        110.0, 5.2, 342.6, 230.9, 256.1, 45.3, 242.9, 115.2, 151.8, 285.3, 53.3, 126.6, 205.7,
        85.9, 146.1,
    ];
    let mut sum = 0.0;
    for i in 0..49 {
        sum += COEF[i] * (ADD[i] + MULT[i] * c).to_radians().sin();
    }
    let lambda = 282.7771834 + 36000.76953744 * c + 0.000005729577951308232 * sum;
    let aberration = 0.0000974 * (177.63 + 35999.01848 * c).to_radians().cos() - 0.005575;
    let a = 124.90 - 1934.134 * c + 0.002063 * c * c;
    let b = 201.11 + 72001.5377 * c + 0.00057 * c * c;
    let nutation = -0.004778 * a.to_radians().sin() - 0.0003667 * b.to_radians().sin();
    (lambda + aberration + nutation).rem_euclid(360.0)
}
/// The universal-time moment of the `n`th new moon after the epoch new moon (Meeus, ch. 49).
fn nth_new_moon(n: i64) -> f64 {
    let k = n as f64;
    let t = k / 1236.85;
    let jde = 2451550.09766 + MEAN_SYNODIC_MONTH * k + 0.00015437 * t * t
        - 0.000000150 * t.powi(3) + 0.00000000073 * t.powi(4);
    let e = 1.0 - 0.002516 * t - 0.0000074 * t * t;
    let m = (2.5534 + 29.10535670 * k - 0.0000014 * t * t - 0.00000011 * t.powi(3)).to_radians();
    let mp = (201.5643 + 385.81693528 * k + 0.0107582 * t * t + 0.00001238 * t.powi(3)
        - 0.000000058 * t.powi(4))
    .to_radians();
    let f = (160.7108 + 390.67050284 * k - 0.0016118 * t * t - 0.00000227 * t.powi(3)
        + 0.000000011 * t.powi(4))
    .to_radians();
    let om = (124.7746 - 1.56375588 * k + 0.0020672 * t * t + 0.00000215 * t.powi(3)).to_radians();
    let corr = -0.40720 * mp.sin() + 0.17241 * e * m.sin() + 0.01608 * (2.0 * mp).sin()
        + 0.01039 * (2.0 * f).sin() + 0.00739 * e * (mp - m).sin() - 0.00514 * e * (mp + m).sin()
        + 0.00208 * e * e * (2.0 * m).sin() - 0.00111 * (mp - 2.0 * f).sin()
        - 0.00057 * (mp + 2.0 * f).sin() + 0.00056 * e * (2.0 * mp + m).sin()
        - 0.00042 * (3.0 * mp).sin() + 0.00042 * e * (m + 2.0 * f).sin()
        + 0.00038 * e * (m - 2.0 * f).sin() - 0.00024 * e * (2.0 * mp - m).sin()
        - 0.00017 * om.sin() - 0.00007 * (mp + 2.0 * m).sin() + 0.00004 * (2.0 * mp - 2.0 * f).sin()
        + 0.00004 * (3.0 * m).sin() + 0.00003 * (mp + m - 2.0 * f).sin()
        + 0.00003 * (2.0 * mp + 2.0 * f).sin() - 0.00003 * (mp + m + 2.0 * f).sin()
        + 0.00003 * (mp - m + 2.0 * f).sin() - 0.00002 * (mp - m - 2.0 * f).sin()
        - 0.00002 * (3.0 * mp + m).sin() + 0.00002 * (4.0 * mp).sin();
    const ADDL: [(f64, f64, f64); 14] = [
        (0.000325, 299.77, 0.107408), (0.000165, 251.88, 0.016321), (0.000164, 251.83, 26.651886),
        (0.000126, 349.42, 36.412478), (0.000110, 84.66, 18.206239), (0.000062, 141.74, 53.303771),
        (0.000060, 207.14, 2.453732), (0.000056, 154.84, 7.306860), (0.000047, 34.52, 27.261239),
        (0.000042, 207.19, 0.121824), (0.000040, 291.34, 1.844379), (0.000037, 161.72, 24.198154),
        (0.000035, 239.56, 25.513099), (0.000023, 331.55, 3.592518),
    ];
    let mut add = 0.0;
    for (idx, (amp, c0, c1)) in ADDL.iter().enumerate() {
        let arg = if idx == 0 { c0 + c1 * k - 0.009173 * t * t } else { c0 + c1 * k };
        add += amp * arg.to_radians().sin();
    }
    let jde_new = jde + corr + add; // dynamical time
    let approx_ed = jde_new - 2440587.5;
    let (gy, _, _) = civil_from_days(approx_ed.floor() as i64);
    jde_new - delta_t_seconds(gy) / 86400.0 - 2440587.5
}
/// The last new moon strictly before universal moment `ed`.
fn new_moon_before(ed: f64) -> f64 {
    let n0 = ((ed + 2440587.5 - 2451550.09766) / MEAN_SYNODIC_MONTH).round() as i64;
    let mut n = n0 + 2;
    while nth_new_moon(n) >= ed {
        n -= 1;
    }
    nth_new_moon(n)
}
/// The first new moon at or after universal moment `ed`.
fn new_moon_at_or_after(ed: f64) -> f64 {
    let n0 = ((ed + 2440587.5 - 2451550.09766) / MEAN_SYNODIC_MONTH).round() as i64;
    let mut n = n0 - 2;
    while nth_new_moon(n) < ed {
        n += 1;
    }
    nth_new_moon(n)
}
fn estimate_prior_solar_longitude(lambda: f64, t: f64) -> f64 {
    let rate = MEAN_TROPICAL_YEAR / 360.0;
    let tau = t - rate * (solar_longitude(t) - lambda).rem_euclid(360.0);
    let delta = (solar_longitude(tau) - lambda + 180.0).rem_euclid(360.0) - 180.0;
    t.min(tau - rate * delta)
}

// --- Chinese/Dangi calendar (Beijing / Seoul local time) ---
/// UTC offset (fraction of a day) for the calendar's reference meridian in a Gregorian year.
fn china_zone(cal: &str, gy: i64) -> f64 {
    if cal == "dangi" {
        // Korea: +8:30 before 1912, +8:00 1912-1954 & 1961-, +8:30 1954-1961 (approx per CLDR).
        if gy < 1908 { 3809.0 / 450.0 / 24.0 } else { 9.0 / 24.0 }
    } else if gy < 1929 {
        1397.0 / 180.0 / 24.0
    } else {
        8.0 / 24.0
    }
}
fn midnight_china(cal: &str, day: i64) -> f64 {
    let (gy, _, _) = civil_from_days(day);
    day as f64 - china_zone(cal, gy)
}
fn china_day_of(cal: &str, ut: f64) -> i64 {
    let (gy, _, _) = civil_from_days(ut.floor() as i64);
    (ut + china_zone(cal, gy)).floor() as i64
}
/// The index (1..12) of the major solar term (zhongqi) in effect at a Beijing day's midnight.
fn current_major_solar_term(cal: &str, day: i64) -> i64 {
    let s = solar_longitude(midnight_china(cal, day));
    (2 + (s / 30.0).floor() as i64 - 1).rem_euclid(12) + 1
}
fn china_new_moon_before(cal: &str, day: i64) -> i64 {
    china_day_of(cal, new_moon_before(midnight_china(cal, day)))
}
fn china_new_moon_on_or_after(cal: &str, day: i64) -> i64 {
    china_day_of(cal, new_moon_at_or_after(midnight_china(cal, day)))
}
/// Whether the month beginning at new-moon day `m` contains no major solar term (→ a leap month).
fn china_no_major_solar_term(cal: &str, m: i64) -> bool {
    current_major_solar_term(cal, m) == current_major_solar_term(cal, china_new_moon_on_or_after(cal, m + 1))
}
fn china_winter_solstice_before(cal: &str, day: i64) -> i64 {
    let approx = estimate_prior_solar_longitude(270.0, midnight_china(cal, day + 1));
    let mut d = approx.floor() as i64 - 1;
    while solar_longitude(midnight_china(cal, d + 1)) <= 270.0 {
        d += 1;
    }
    d
}
fn china_prior_leap_month(cal: &str, m_prime: i64, m: i64) -> bool {
    m >= m_prime
        && (china_no_major_solar_term(cal, m)
            || china_prior_leap_month(cal, m_prime, china_new_moon_before(cal, m)))
}
fn china_new_year_in_sui(cal: &str, day: i64) -> i64 {
    let s1 = china_winter_solstice_before(cal, day);
    let s2 = china_winter_solstice_before(cal, s1 + 370);
    let m12 = china_new_moon_on_or_after(cal, s1 + 1);
    let m13 = china_new_moon_on_or_after(cal, m12 + 1);
    let next_m11 = china_new_moon_before(cal, s2 + 1);
    if ((next_m11 - m12) as f64 / MEAN_SYNODIC_MONTH).round() as i64 == 12
        && (china_no_major_solar_term(cal, m12) || china_no_major_solar_term(cal, m13))
    {
        china_new_moon_on_or_after(cal, m13 + 1)
    } else {
        m13
    }
}
fn china_new_year_before(cal: &str, day: i64) -> i64 {
    let ny = china_new_year_in_sui(cal, day);
    if day >= ny {
        ny
    } else {
        china_new_year_in_sui(cal, day - 180)
    }
}
/// (year, ordinal-month, month-number, leap-month?, day) for an ISO date in Chinese/Dangi.
/// `ordinal` is the 1-based position in the year (1..13); `month-number` is the 1..12 name shared by
/// a leap month with its predecessor (the leap month adds an "L" to its monthCode).
fn china_fields(cal: &str, iso: IsoDate) -> (i64, i64, i64, bool, i64) {
    let day = epoch_days(iso);
    let s1 = china_winter_solstice_before(cal, day);
    let s2 = china_winter_solstice_before(cal, s1 + 370);
    let m12 = china_new_moon_on_or_after(cal, s1 + 1);
    let next_m11 = china_new_moon_before(cal, s2 + 1);
    let leap_year = ((next_m11 - m12) as f64 / MEAN_SYNODIC_MONTH).round() as i64 == 12;
    let m = china_new_moon_before(cal, day + 1);
    let elapsed = ((m - m12) as f64 / MEAN_SYNODIC_MONTH).round() as i64;
    let adj = if leap_year && china_prior_leap_month(cal, m12, m) { 1 } else { 0 };
    let month_num = (elapsed - adj - 1).rem_euclid(12) + 1;
    let leap_month = leap_year
        && china_no_major_solar_term(cal, m)
        && !china_prior_leap_month(cal, m12, china_new_moon_before(cal, m));
    let ny = china_new_year_before(cal, day);
    let ordinal = ((m - ny) as f64 / MEAN_SYNODIC_MONTH).round() as i64 + 1;
    // Temporal reports the Chinese year as the related Gregorian year (the Gregorian year the sui
    // begins in), not the continuous count.
    let (year, _, _) = civil_from_days(ny);
    (year, ordinal, month_num, leap_month, day - m + 1)
}

/// The epoch-day of Chinese New Year for a given Chinese year.
fn china_new_year_of(cal: &str, year: i64) -> i64 {
    // The Chinese year number IS the Gregorian year its CNY falls in.
    china_new_year_before(cal, days_from_civil(year, 12, 31))
}
/// Months in a Chinese year (12, or 13 in a leap year).
fn china_months_in_year(cal: &str, year: i64) -> i64 {
    let ny = china_new_year_of(cal, year);
    let ny2 = china_new_year_of(cal, year + 1);
    ((ny2 - ny) as f64 / MEAN_SYNODIC_MONTH).round() as i64
}
/// The new-moon start-day of the month with (month-number, leap?) in a Chinese year, if it exists.
fn china_month_start(cal: &str, year: i64, num: i64, leap: bool) -> Option<i64> {
    let mut m = china_new_year_of(cal, year);
    for _ in 0..14 {
        let (y, mo, d) = civil_from_days(m);
        let f = china_fields(cal, IsoDate { year: y, month: mo, day: d });
        if f.2 == num && f.3 == leap {
            return Some(m);
        }
        m = china_new_moon_on_or_after(cal, m + 1);
    }
    None
}
/// The length of the Chinese month starting at new-moon day `start`.
fn china_month_len_at(cal: &str, start: i64) -> i64 {
    china_new_moon_on_or_after(cal, start + 1) - start
}
/// The new-moon start-day of the ordinal-th month (1-based) of a Chinese year.
fn china_ord_start(cal: &str, year: i64, ordinal: i64) -> i64 {
    let mut m = china_new_year_of(cal, year);
    for _ in 1..ordinal {
        m = china_new_moon_on_or_after(cal, m + 1);
    }
    m
}
fn china_ord_to_iso(cal: &str, year: i64, ordinal: i64, day: i64) -> IsoDate {
    let (y, mo, d) = civil_from_days(china_ord_start(cal, year, ordinal) + day - 1);
    IsoDate { year: y, month: mo, day: d }
}
fn china_ord_month_len(cal: &str, year: i64, ordinal: i64) -> i64 {
    china_month_len_at(cal, china_ord_start(cal, year, ordinal))
}
/// The ordinal position (1-based) of the month starting at new-moon day `start` in a Chinese year.
fn china_ord_of_start(cal: &str, year: i64, start: i64) -> i64 {
    let ny = china_new_year_of(cal, year);
    ((start - ny) as f64 / MEAN_SYNODIC_MONTH).round() as i64 + 1
}
/// The ordinal of the month (month-number, leap?) in `year`, constraining a missing leap month to
/// its plain counterpart. Returns (leap-existed?, ordinal).
fn china_resolve_month_ord(cal: &str, year: i64, num: i64, leap: bool) -> (bool, i64) {
    if let Some(s) = china_month_start(cal, year, num, leap) {
        (true, china_ord_of_start(cal, year, s))
    } else {
        let s = china_month_start(cal, year, num, false).unwrap();
        (false, china_ord_of_start(cal, year, s))
    }
}
/// Resolve a monthCode to an ordinal month in a leap-month calendar (hebrew/chinese/dangi),
/// constraining an absent leap month to its plain counterpart. Returns (ordinal, leap-existed?).
fn resolve_code_ord(cal: &str, year: i64, code: &str) -> (i64, bool) {
    if cal == "hebrew" {
        if let Some(o) = hebrew_ord_from_code(year, code) {
            return (o, true);
        }
        // The only absent code is "M05L" (Adar I) in a common year — it collapses to Adar, which is
        // "M06" (not "M05" = Shevat).
        (hebrew_ord_from_code(year, "M06").unwrap(), false)
    } else {
        let body = code.strip_prefix('M').unwrap();
        let (digits, leap) = match body.strip_suffix('L') {
            Some(d) => (d, true),
            None => (body, false),
        };
        let num: i64 = digits.parse().unwrap();
        let (ok, ord) = china_resolve_month_ord(cal, year, num, leap);
        (ord, ok)
    }
}
/// Advance an ordinal month by a signed `k` across years of a varying-length calendar.
fn advance_ord(cal: &str, year: i64, ord: i64, k: i64) -> (i64, i64) {
    let (mut y, mut m) = (year, ord);
    if k >= 0 {
        for _ in 0..k {
            m += 1;
            if m > cal_months_in_year(cal, y) {
                m = 1;
                y += 1;
            }
        }
    } else {
        for _ in 0..(-k) {
            m -= 1;
            if m < 1 {
                y -= 1;
                m = cal_months_in_year(cal, y);
            }
        }
    }
    (y, m)
}
/// CalendarDateAdd for leap-month calendars (hebrew/chinese/dangi): add years while preserving the
/// monthCode (constraining an absent leap month), then months by ordinal position, then regulate the
/// day, then weeks/days. `err` is `Some` for reject-overflow (returns a RangeError), `None` for
/// constrain (never errors).
fn leap_cal_add(i: Option<&Interp>, cal: &str, d: IsoDate, dur: IsoDuration, sign: i64) -> Result<IsoDate, Value> {
    let f = cal_fields(cal, d);
    let (year, day_of) = (f.0, f.2);
    let code = cal_month_code(cal, d);
    let ny = year + sign * dur.years;
    let (ord0, leap_ok) = resolve_code_ord(cal, ny, &code);
    if let Some(i) = i {
        if !leap_ok {
            return Err(i.make_error("RangeError", "leap month does not exist in the target year"));
        }
    }
    let (fy, fm) = advance_ord(cal, ny, ord0, sign * dur.months);
    let mlen = cal_month_len(cal, fy, fm);
    if let Some(i) = i {
        if day_of > mlen {
            return Err(i.make_error("RangeError", "day is out of range in the target month"));
        }
    }
    let dd = day_of.min(mlen);
    let iso0 = cal_to_iso(cal, fy, fm, dd);
    let z = epoch_days(iso0) + sign * (dur.weeks * 7 + dur.days);
    let (y, m, day) = civil_from_days(z);
    Ok(IsoDate { year: y, month: m, day })
}
/// The Chinese monthCode for a (month-number, leap?): "M04" or "M04L".
fn china_month_code(num: i64, leap: bool) -> String {
    if leap {
        format!("M{num:02}L")
    } else {
        format!("M{num:02}")
    }
}

/// Whether a calendar has its own month structure (not the Gregorian months), so date arithmetic
/// must add years/months in the calendar's own terms.
fn is_month_structure(cal: &str) -> bool {
    is_13month(cal)
        || is_islamic(cal)
        || cal == "indian"
        || cal == "hebrew"
        || cal == "persian"
        || cal == "chinese"
        || cal == "dangi"
}
/// The ISO date for a month-structure calendar's (year, month, day).
fn cal_to_iso(cal: &str, y: i64, m: i64, d: i64) -> IsoDate {
    if is_13month(cal) {
        to_13month(y, m, d, epoch_13(cal))
    } else if is_islamic(cal) {
        isl_to(cal, y, m, d)
    } else if cal == "hebrew" {
        hebrew_to_iso(y, m, d)
    } else if cal == "persian" {
        to_persian(y, m, d)
    } else if cal == "chinese" || cal == "dangi" {
        china_ord_to_iso(cal, y, m, d)
    } else {
        to_indian(y, m, d)
    }
}
/// The length of month `m` in year `y` of a month-structure calendar.
fn cal_month_len(cal: &str, y: i64, m: i64) -> i64 {
    if is_13month(cal) {
        if m <= 12 {
            30
        } else if y.rem_euclid(4) == 3 {
            6
        } else {
            5
        }
    } else if is_islamic(cal) {
        isl_month_len(cal, y, m)
    } else if cal == "hebrew" {
        hebrew_month_len(y, m)
    } else if cal == "persian" {
        persian_month_len(y, m)
    } else if cal == "chinese" || cal == "dangi" {
        china_ord_month_len(cal, y, m)
    } else {
        indian_month_len(m, is_leap(y + 78))
    }
}
/// CalendarDateAdd for a month-structure calendar: add years/months in the calendar (clamping the
/// day to the target month under `constrain`, or rejecting when it overflows), then weeks/days.
fn cal_add(i: &Interp, cal: &str, d: IsoDate, dur: IsoDuration, sign: i64, ovf: Overflow) -> Result<IsoDate, Value> {
    if matches!(cal, "hebrew" | "chinese" | "dangi") {
        return leap_cal_add(if ovf == Overflow::Reject { Some(i) } else { None }, cal, d, dur, sign);
    }
    if ovf == Overflow::Reject {
        let f = cal_fields(cal, d);
        let (cy, cm, cd, mpy) = (f.0, f.1, f.2, f.4);
        let total = cy * mpy + (cm - 1) + sign * (dur.years * mpy + dur.months);
        let (ny, nm) = (total.div_euclid(mpy), total.rem_euclid(mpy) + 1);
        if cd > cal_month_len(cal, ny, nm) {
            return Err(i.make_error("RangeError", "day is out of range in the target month"));
        }
    }
    Ok(cal_add_c(cal, d, dur, sign))
}
/// Months in year `y` of a month-structure calendar (13 for coptic/ethiopic, 12 or 13 for Hebrew).
fn cal_months_in_year(cal: &str, y: i64) -> i64 {
    if cal == "hebrew" {
        hebrew_months_in_year(y)
    } else if cal == "chinese" || cal == "dangi" {
        china_months_in_year(cal, y)
    } else if is_13month(cal) {
        13
    } else {
        12
    }
}
/// Constrain-only calendar add (clamps the day to the target month).
fn cal_add_c(cal: &str, d: IsoDate, dur: IsoDuration, sign: i64) -> IsoDate {
    if matches!(cal, "hebrew" | "chinese" | "dangi") {
        // Constrain never errors (`None` interpreter → reject-free path).
        return match leap_cal_add(None, cal, d, dur, sign) {
            Ok(v) => v,
            Err(_) => unreachable!(),
        };
    }
    let f = cal_fields(cal, d);
    let (cy, cm, cd, mpy) = (f.0, f.1, f.2, f.4);
    let total = cy * mpy + (cm - 1) + sign * (dur.years * mpy + dur.months);
    let ny = total.div_euclid(mpy);
    let nm = total.rem_euclid(mpy) + 1;
    let iso0 = cal_to_iso(cal, ny, nm, cd.min(cal_month_len(cal, ny, nm)));
    let z = epoch_days(iso0) + sign * (dur.weeks * 7 + dur.days);
    let (yy, mm, dd) = civil_from_days(z);
    IsoDate { year: yy, month: mm, day: dd }
}

// Indian national (Saka) calendar: a solar calendar whose year begins on Chaitra 1 (Gregorian
// Mar 22, or Mar 21 in a Gregorian leap year). Saka year = Gregorian - 78.
fn indian_month_len(m: i64, leap: bool) -> i64 {
    if m == 1 {
        if leap { 31 } else { 30 }
    } else if m <= 6 {
        31
    } else {
        30
    }
}
fn from_indian(iso: IsoDate) -> (i64, i64, i64) {
    let g = iso.year;
    let fixed = epoch_days(iso);
    let chaitra1 = |gy: i64| days_from_civil(gy, 3, if is_leap(gy) { 21 } else { 22 });
    let (s, start) = if fixed >= chaitra1(g) {
        (g - 78, chaitra1(g))
    } else {
        (g - 79, chaitra1(g - 1))
    };
    let leap = is_leap(s + 78);
    let mut rem = fixed - start;
    let mut month = 1;
    while month < 12 {
        let ml = indian_month_len(month, leap);
        if rem < ml {
            break;
        }
        rem -= ml;
        month += 1;
    }
    (s, month, rem + 1)
}
fn to_indian(s: i64, m: i64, d: i64) -> IsoDate {
    let gy = s + 78;
    let leap = is_leap(gy);
    let chaitra1 = days_from_civil(gy, 3, if leap { 21 } else { 22 });
    let doy: i64 = (1..m).map(|mm| indian_month_len(mm, leap)).sum::<i64>() + (d - 1);
    let (yy, mm, dd) = civil_from_days(chaitra1 + doy);
    IsoDate { year: yy, month: mm, day: dd }
}
fn epoch_13(cal: &str) -> i64 {
    if cal == "coptic" { COPTIC_EPOCH } else { ETHIOPIC_EPOCH }
}

/// A 13-month (12×30 days + a 5/6-day epagomenal month) calendar date from an ISO date: returns
/// (year, month, day). `epoch` is the calendar's 1-01-01 in epoch-days.
fn from_13month(iso: IsoDate, epoch: i64) -> (i64, i64, i64) {
    let ed = epoch_days(iso);
    let d0 = ed - epoch; // days since epoch, 0-based
    let year = (4 * d0 + 1463).div_euclid(1461);
    let year_start = epoch + 365 * (year - 1) + year.div_euclid(4);
    let doy = ed - year_start; // 0-based day within the year
    (year, doy.div_euclid(30) + 1, doy.rem_euclid(30) + 1)
}
/// The ISO date for a 13-month calendar (year, month, day).
fn to_13month(year: i64, month: i64, day: i64, epoch: i64) -> IsoDate {
    let ed = epoch + 365 * (year - 1) + year.div_euclid(4) + 30 * (month - 1) + (day - 1);
    let (y, m, d) = civil_from_days(ed);
    IsoDate { year: y, month: m, day: d }
}

/// Full calendar fields for an ISO date: (year, month, day, daysInMonth, monthsInYear, dayOfYear,
/// daysInYear, inLeapYear). ISO/arithmetic calendars keep the Gregorian month/day structure.
fn cal_fields(cal: &str, iso: IsoDate) -> (i64, i64, i64, i64, i64, i64, i64, bool) {
    if is_13month(cal) {
        let (y, m, d) = from_13month(iso, epoch_13(cal));
        let leap = y.rem_euclid(4) == 3;
        let dim = if m <= 12 {
            30
        } else if leap {
            6
        } else {
            5
        };
        let doy = 30 * (m - 1) + d;
        let diy = if leap { 366 } else { 365 };
        (y, m, d, dim, 13, doy, diy, leap)
    } else if is_islamic(cal) {
        let (y, m, d) = isl_from(cal, iso);
        let dim = isl_month_len(cal, y, m);
        let doy = (1..m).map(|mm| isl_month_len(cal, y, mm)).sum::<i64>() + d;
        let diy = if cal == "islamic-umalqura" {
            umalqura_year_len(y)
        } else if islamic_leap(y) {
            355
        } else {
            354
        };
        let leap = diy == 355;
        (y, m, d, dim, 12, doy, diy, leap)
    } else if cal == "indian" {
        let (y, m, d) = from_indian(iso);
        let leap = is_leap(y + 78);
        let dim = indian_month_len(m, leap);
        let doy = (1..m).map(|mm| indian_month_len(mm, leap)).sum::<i64>() + d;
        let diy = if leap { 366 } else { 365 };
        (y, m, d, dim, 12, doy, diy, leap)
    } else if cal == "persian" {
        let (y, m, d) = from_persian(iso);
        let dim = persian_month_len(y, m);
        let leap = persian_leap(y);
        let doy = (1..m).map(|mm| persian_month_len(y, mm)).sum::<i64>() + d;
        (y, m, d, dim, 12, doy, if leap { 366 } else { 365 }, leap)
    } else if cal == "hebrew" {
        let (y, m, d) = hebrew_from_iso(iso);
        let mpy = hebrew_months_in_year(y);
        let dim = hebrew_month_len(y, m);
        let doy = (1..m).map(|mm| hebrew_month_len(y, mm)).sum::<i64>() + d;
        (y, m, d, dim, mpy, doy, hebrew_year_len(y), hebrew_leap(y))
    } else if cal == "chinese" || cal == "dangi" {
        let (y, ord, _num, _leap, d) = china_fields(cal, iso);
        let day = epoch_days(iso);
        let start = china_new_moon_before(cal, day + 1);
        let dim = china_month_len_at(cal, start);
        let mpy = china_months_in_year(cal, y);
        let ny = china_new_year_of(cal, y);
        let doy = day - ny + 1;
        let diy = china_new_year_of(cal, y + 1) - ny;
        (y, ord, d, dim, mpy, doy, diy, mpy == 13)
    } else {
        (
            cal_year_num(cal, iso),
            iso.month as i64,
            iso.day as i64,
            days_in_month(iso.year, iso.month) as i64,
            12,
            iso_day_of_year(iso) as i64,
            if is_leap(iso.year) { 366 } else { 365 },
            is_leap(iso.year),
        )
    }
}

/// The monthCode string for a date. Most calendars derive it from the ordinal month; Hebrew inserts
/// the leap month code "M05L".
fn cal_month_code(cal: &str, iso: IsoDate) -> String {
    if cal == "hebrew" {
        let (y, m, _) = hebrew_from_iso(iso);
        hebrew_month_code(y, m)
    } else if cal == "chinese" || cal == "dangi" {
        let (_, _, num, leap, _) = china_fields(cal, iso);
        china_month_code(num, leap)
    } else {
        month_code(cal_fields(cal, iso).1 as u8).to_string()
    }
}

/// The calendar-year number a receiver's `.year` reports (ISO/gregory/japanese use the ISO year).
fn cal_year_num(cal: &str, d: IsoDate) -> i64 {
    match cal {
        "buddhist" => d.year + 543,
        "roc" => d.year - 1911,
        "coptic" | "ethiopic" => from_13month(d, epoch_13(cal)).0,
        "ethioaa" => from_13month(d, ETHIOPIC_EPOCH).0 + 5500,
        _ if is_islamic(cal) => isl_from(cal, d).0,
        "indian" => from_indian(d).0,
        "hebrew" => hebrew_from_iso(d).0,
        "persian" => from_persian(d).0,
        "chinese" | "dangi" => china_fields(cal, d).0,
        _ => d.year,
    }
}

/// Back-convert a calendar year field to the ISO (proleptic Gregorian) year.
fn cal_year_to_iso(cal: &str, year: i64) -> i64 {
    match cal {
        "buddhist" => year - 543,
        "roc" => year + 1911,
        _ => year,
    }
}

/// The ISO year for an `(era, eraYear)` pair in an arithmetic calendar, if recognized.
fn cal_era_to_iso(cal: &str, era: &str, era_year: i64) -> Option<i64> {
    match (cal, era) {
        ("gregory" | "japanese", "ce" | "gregory" | "ad") => Some(era_year),
        ("gregory" | "japanese", "bce" | "gregory-inverse" | "bc") => Some(1 - era_year),
        ("japanese", "reiwa") => Some(2019 + era_year - 1),
        ("japanese", "heisei") => Some(1989 + era_year - 1),
        ("japanese", "showa") => Some(1926 + era_year - 1),
        ("japanese", "taisho") => Some(1912 + era_year - 1),
        ("japanese", "meiji") => Some(1868 + era_year - 1),
        ("buddhist", "be") => Some(era_year - 543),
        ("roc", "roc" | "minguo") => Some(1911 + era_year),
        ("roc", "broc" | "before-roc") => Some(1912 - era_year),
        _ => None,
    }
}

/// Whether a calendar numbers years via eras (so era/eraYear fields resolve the year).
fn cal_uses_era(cal: &str) -> bool {
    matches!(cal, "gregory" | "japanese" | "buddhist" | "roc") || is_13month(cal) || is_islamic(cal) || cal == "indian" || cal == "hebrew" || cal == "persian"
}

/// Whether `era` (already lowercased) is an era this calendar defines.
fn valid_era(cal: &str, era: &str) -> bool {
    match cal {
        "gregory" => matches!(era, "ce" | "bce" | "gregory" | "gregory-inverse" | "ad" | "bc"),
        "japanese" => matches!(
            era,
            "ce" | "bce" | "gregory" | "gregory-inverse" | "ad" | "bc"
                | "reiwa" | "heisei" | "showa" | "taisho" | "meiji"
        ),
        "buddhist" => era == "be",
        "roc" => matches!(era, "roc" | "roc-inverse" | "broc" | "minguo" | "before-roc"),
        "coptic" => era == "am",
        "ethiopic" => matches!(era, "am" | "aa" | "mundi" | "incar"),
        "ethioaa" => matches!(era, "aa" | "mundi"),
        _ if is_islamic(cal) => matches!(era, "ah" | "bh"),
        "indian" => era == "shaka",
        "hebrew" => era == "am",
        "persian" => era == "ap",
        _ => false,
    }
}

/// The amete-mihret / coptic year for a 13-month calendar from a `year` field or `(era, eraYear)`.
fn thirteen_month_year(cal: &str, year: Option<i64>, era: &Option<String>, era_year: Option<i64>) -> Option<i64> {
    // For `ethioaa` the year/`aa` era counts amete-alem (= amete-mihret + 5500).
    let offset = if cal == "ethioaa" { 5500 } else { 0 };
    if let Some(y) = year {
        return Some(y - offset);
    }
    match (era.as_deref(), era_year) {
        (Some("am"), Some(ey)) => Some(ey),
        (Some("aa"), Some(ey)) => Some(ey - 5500),
        _ => None,
    }
}

/// Make a new Temporal value that inherits `src`'s calendar (operations like add/with/round preserve
/// the receiver's calendar).
fn make_like(i: &mut Interp, src: &Value, kind: &str, t: Temporal) -> Value {
    let cal = cal_of(i, src);
    let v = make(i, kind, t);
    set_cal(i, &v, cal);
    v
}

/// Record the calendar id on a just-created Temporal object.
fn set_cal(i: &mut Interp, v: &Value, cal: std::rc::Rc<str>) {
    if let Value::Obj(o) = v {
        i.temporal_cal.insert(Rc::as_ptr(o) as usize, cal);
    }
}

/// The calendar id of a Temporal receiver (default "iso8601").
fn cal_of(i: &Interp, this: &Value) -> std::rc::Rc<str> {
    match this {
        Value::Obj(o) => i
            .temporal_cal
            .get(&(Rc::as_ptr(o) as usize))
            .cloned()
            .unwrap_or_else(|| std::rc::Rc::from("iso8601")),
        _ => std::rc::Rc::from("iso8601"),
    }
}
/// ToIntegerIfIntegral: ToNumber, then reject non-finite and any fractional part (Duration fields).
fn to_int_integral(i: &mut Interp, v: &Value) -> Result<i64, Value> {
    let n = i.to_number(v).map_err(unab)?;
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(i.make_error("RangeError", "value must be an integer"));
    }
    Ok(n as i64)
}
/// Read a Duration property-bag field via ToIntegerIfIntegral, defaulting when absent.
/// Read a positional Duration constructor argument via ToIntegerIfIntegral, defaulting to 0.
fn dur_arg(i: &mut Interp, v: &Value) -> Result<i64, Value> {
    match v {
        Value::Undefined => Ok(0),
        _ => to_int_integral(i, v),
    }
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
    let smallest_raw = opt_str(i, opts, "smallestUnit", "")?;
    let smallest = smallest_raw.strip_suffix('s').unwrap_or(&smallest_raw);
    // A present smallestUnit must be a time unit; the roundingMode option is validated even when the
    // default is used (a non-string / out-of-range value is a RangeError).
    if !smallest.is_empty()
        && !matches!(smallest, "minute" | "second" | "millisecond" | "microsecond" | "nanosecond")
    {
        return Err(i.make_error("RangeError", "smallestUnit must be a time unit"));
    }
    let mode = opt_str(i, opts, "roundingMode", "trunc")?;
    check_mode(i, &mode)?;
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
fn cal_suffix(i: &mut Interp, opts: &Value, cal: &str) -> Result<String, Value> {
    match opt_enum(i, opts, "calendarName", &["auto", "always", "never", "critical"], "auto")?.as_str() {
        "never" => Ok(String::new()),
        "always" => Ok(format!("[u-ca={cal}]")),
        "critical" => Ok(format!("[!u-ca={cal}]")),
        // "auto": show the annotation only for a non-ISO calendar.
        _ => Ok(if cal == "iso8601" {
            String::new()
        } else {
            format!("[u-ca={cal}]")
        }),
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
    for v in [
        d.years, d.months, d.weeks, d.days, d.hours, d.minutes, d.seconds, d.ms, d.us, d.ns,
    ] {
        if v != 0 {
            return if v < 0 { -1 } else { 1 };
        }
    }
    0
}

// ----- ISO-8601 string parsing ----------------------------------------------------------------

/// A parsed UTC-offset designator.
#[derive(Clone, Copy, PartialEq)]
enum Off {
    None,
    Z,
    Num(i64), // offset in nanoseconds
}

/// The result of parsing an ISO date-time string (one of the date / date-time / time productions).
struct Parsed {
    date: Option<IsoDate>, // a full calendar date (with day) when present
    time: Option<IsoTime>,
    designator: bool, // a leading time designator `T`/`t` was present (bare-time form)
    offset: Off,
    calendar: Option<String>, // the first `u-ca` annotation value (lowercased)
    tz: Option<String>,       // a time-zone annotation `[..]`, if any
}

/// A byte cursor over an ISO string. ISO strings are pure ASCII; any non-ASCII byte simply fails to
/// match and aborts the parse (so the Unicode minus `U+2212` is rejected).
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Cur<'a> {
    fn new(s: &'a str) -> Self {
        Cur {
            b: s.as_bytes(),
            i: 0,
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn at(&self, off: usize) -> Option<u8> {
        self.b.get(self.i + off).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_any(&mut self, cs: &[u8]) -> bool {
        match self.peek() {
            Some(c) if cs.contains(&c) => {
                self.i += 1;
                true
            }
            _ => false,
        }
    }
    fn digit_at(&self, off: usize) -> bool {
        matches!(self.at(off), Some(c) if c.is_ascii_digit())
    }
    /// Consume exactly `n` digits, returning their numeric value.
    fn num(&mut self, n: usize) -> Option<i64> {
        let mut v = 0i64;
        for k in 0..n {
            let c = self.at(k)?;
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        self.i += n;
        Some(v)
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
}

/// `DateYear`: a 4-digit year, or a sign followed by exactly 6 digits (expanded year). A negative
/// zero expanded year is invalid.
fn p_year(c: &mut Cur) -> Option<i64> {
    match c.peek() {
        Some(b'+') => {
            c.i += 1;
            c.num(6)
        }
        Some(b'-') => {
            c.i += 1;
            let v = c.num(6)?;
            if v == 0 {
                None
            } else {
                Some(-v)
            }
        }
        _ => c.num(4),
    }
}

/// A full calendar date `Year(-?)Month(-?)Day` with consistent separators.
fn p_date(c: &mut Cur) -> Option<IsoDate> {
    let year = p_year(c)?;
    let (month, day) = if c.eat(b'-') {
        let m = c.num(2)?;
        if !c.eat(b'-') {
            return None;
        }
        (m, c.num(2)?)
    } else {
        (c.num(2)?, c.num(2)?)
    };
    if !(1..=12).contains(&month) {
        return None;
    }
    let m = month as u8;
    if day < 1 || day as u8 > days_in_month(year, m) {
        return None;
    }
    Some(IsoDate {
        year,
        month: m,
        day: day as u8,
    })
}

/// A fractional second/offset: `.`/`,` then 1..9 digits. Returns `(ms, us, ns)`; `None` only on a
/// malformed fraction (dangling separator or more than 9 digits). Absence yields `(0, 0, 0)`.
fn p_fraction(c: &mut Cur) -> Option<(u16, u16, u16)> {
    if c.peek() != Some(b'.') && c.peek() != Some(b',') {
        return Some((0, 0, 0));
    }
    if !c.digit_at(1) {
        return None;
    }
    c.i += 1;
    let mut digits = 0usize;
    let mut val: u32 = 0;
    while c.digit_at(0) {
        if digits < 9 {
            val = val * 10 + (c.peek().unwrap() - b'0') as u32;
        }
        digits += 1;
        c.i += 1;
    }
    if digits > 9 {
        return None;
    }
    for _ in digits..9 {
        val *= 10;
    }
    Some((
        (val / 1_000_000) as u16,
        ((val / 1000) % 1000) as u16,
        (val % 1000) as u16,
    ))
}

/// `TimeSpec`: `HH`, `HH:MM`, `HH:MM:SS[.fff]` (extended) or the colon-less basic equivalents.
/// Separators must be used consistently. A `:60` second is constrained to `59` (leap second).
fn p_time(c: &mut Cur) -> Option<IsoTime> {
    let hour = c.num(2)?;
    if hour > 23 {
        return None;
    }
    let (mut minute, mut second, mut had_sec) = (0i64, 0i64, false);
    if c.eat(b':') {
        minute = c.num(2)?;
        if c.eat(b':') {
            second = c.num(2)?;
            had_sec = true;
        }
    } else if c.digit_at(0) && c.digit_at(1) {
        minute = c.num(2)?;
        if c.digit_at(0) && c.digit_at(1) {
            second = c.num(2)?;
            had_sec = true;
        }
    }
    if minute > 59 || second > 60 {
        return None;
    }
    let (ms, us, ns) = if had_sec { p_fraction(c)? } else { (0, 0, 0) };
    let second = if second == 60 { 59 } else { second };
    Some(IsoTime {
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        ms,
        us,
        ns,
    })
}

/// `DateTimeUTCOffset`: `Z`/`z`, or `±HH[:MM[:SS[.fff]]]` / colon-less basic. Returns `Off::None`
/// when no offset is present, `None` on a malformed offset.
fn p_offset(c: &mut Cur) -> Option<Off> {
    let sign = match c.peek() {
        Some(b'Z') | Some(b'z') => {
            c.i += 1;
            return Some(Off::Z);
        }
        Some(b'+') => 1i64,
        Some(b'-') => -1,
        _ => return Some(Off::None),
    };
    c.i += 1;
    let hour = c.num(2)?;
    if hour > 23 {
        return None;
    }
    let (mut minute, mut second, mut had_sec) = (0i64, 0i64, false);
    if c.eat(b':') {
        minute = c.num(2)?;
        if c.eat(b':') {
            second = c.num(2)?;
            had_sec = true;
        }
    } else if c.digit_at(0) && c.digit_at(1) {
        minute = c.num(2)?;
        if c.digit_at(0) && c.digit_at(1) {
            second = c.num(2)?;
            had_sec = true;
        }
    }
    if minute > 59 || second > 59 {
        return None;
    }
    let (ms, us, ns) = if had_sec { p_fraction(c)? } else { (0, 0, 0) };
    let total = (hour * 3600 + minute * 60 + second) * 1_000_000_000
        + ms as i64 * 1_000_000
        + us as i64 * 1000
        + ns as i64;
    Some(Off::Num(sign * total))
}

/// An annotation key: lowercase, starting with `a-z`/`_`.
fn valid_key(k: &str) -> bool {
    let mut bytes = k.bytes();
    match bytes.next() {
        Some(c) if c.is_ascii_lowercase() || c == b'_' => {}
        _ => return false,
    }
    bytes.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

/// An annotation value: `-`-separated runs of ASCII alphanumerics.
fn valid_value(v: &str) -> bool {
    !v.is_empty()
        && v.split('-')
            .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric()))
}

/// A time-zone identifier: a numeric offset, or an IANA-style name.
fn valid_tz(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if matches!(s.as_bytes()[0], b'+' | b'-') {
        let mut c = Cur::new(s);
        return matches!(p_offset(&mut c), Some(Off::Num(_))) && c.done();
    }
    s.split('/').all(|comp| {
        let bytes = comp.as_bytes();
        match bytes.first() {
            Some(&c0) if c0.is_ascii_alphabetic() || c0 == b'.' || c0 == b'_' => {}
            _ => return false,
        }
        bytes
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'+'))
    })
}

/// Parse the bracketed annotation suffix (`[tz]` then `[key=value]`...), returning the first
/// `u-ca` value and the time-zone annotation. Enforces key/value syntax, single time-zone
/// (before any key=value annotation), critical-unknown rejection, and the "multiple calendars
/// with a critical flag" rejection.
fn p_annotations(c: &mut Cur) -> Option<(Option<String>, Option<String>)> {
    let (mut cal, mut tz): (Option<String>, Option<String>) = (None, None);
    let (mut seen_kv, mut cal_count, mut cal_critical) = (false, 0u32, false);
    while c.peek() == Some(b'[') {
        c.i += 1;
        let critical = c.eat(b'!');
        let start = c.i;
        while !matches!(c.peek(), Some(b']') | None) {
            c.i += 1;
        }
        if c.peek() != Some(b']') {
            return None;
        }
        let inner = std::str::from_utf8(&c.b[start..c.i]).ok()?;
        c.i += 1;
        if let Some(eq) = inner.find('=') {
            let (key, value) = (&inner[..eq], &inner[eq + 1..]);
            if !valid_key(key) || !valid_value(value) {
                return None;
            }
            seen_kv = true;
            if key == "u-ca" {
                cal_count += 1;
                cal_critical |= critical;
                if cal.is_none() {
                    cal = Some(value.to_ascii_lowercase());
                }
            } else if critical {
                return None;
            }
        } else {
            if seen_kv || tz.is_some() || !valid_tz(inner) {
                return None;
            }
            tz = Some(inner.to_string());
        }
    }
    if cal_count > 1 && cal_critical {
        return None;
    }
    Some((cal, tz))
}

/// Parse a full ISO string as a date / date-time / time, trying the date-first production then a
/// bare time. The whole string (annotations included) must be consumed.
fn parse_iso(s: &str) -> Option<Parsed> {
    parse_branch(s, true).or_else(|| parse_branch(s, false))
}
fn parse_branch(s: &str, date_first: bool) -> Option<Parsed> {
    let mut c = Cur::new(s);
    let (mut date, mut time, mut offset, mut designator) = (None, None, Off::None, false);
    if date_first {
        date = Some(p_date(&mut c)?);
        if c.eat_any(b"Tt ") {
            time = Some(p_time(&mut c)?);
            offset = p_offset(&mut c)?;
        }
    } else {
        designator = c.eat_any(b"Tt");
        time = Some(p_time(&mut c)?);
        offset = p_offset(&mut c)?;
    }
    let (calendar, tz) = p_annotations(&mut c)?;
    if !c.done() {
        return None;
    }
    Some(Parsed {
        date,
        time,
        designator,
        offset,
        calendar,
        tz,
    })
}

/// The portion of a string before any annotation (used for ambiguity checks).
fn iso_core(s: &str) -> &str {
    &s[..s.find('[').unwrap_or(s.len())]
}

/// Whether `core` is exactly a valid `DateSpecYearMonth` (`Year(-?)Month`).
fn matches_year_month(core: &str) -> bool {
    let mut c = Cur::new(core);
    if p_year(&mut c).is_none() {
        return false;
    }
    c.eat(b'-');
    match c.num(2) {
        Some(m) => c.done() && (1..=12).contains(&m),
        None => false,
    }
}

/// Whether `core` is exactly a valid `DateSpecMonthDay` (`(--)?Month(-?)Day`).
fn matches_month_day(core: &str) -> bool {
    let mut c = Cur::new(core);
    if c.peek() == Some(b'-') {
        if c.at(1) != Some(b'-') {
            return false;
        }
        c.i += 2;
    }
    let m = match c.num(2) {
        Some(m) => m,
        None => return false,
    };
    c.eat(b'-');
    let d = match c.num(2) {
        Some(d) => d,
        None => return false,
    };
    c.done() && (1..=12).contains(&m) && d >= 1 && d as u8 <= days_in_month(1972, m as u8)
}

/// Whether an effective calendar annotation is acceptable (only the ISO 8601 calendar is supported).
/// Canonicalize a calendar identifier (applying Temporal's aliases) to a known id, or `None` if it
/// is not a calendar Temporal accepts.
fn canon_cal(name: &str) -> Option<&'static str> {
    let lc = name.to_lowercase();
    let mapped = match lc.as_str() {
        "islamicc" => "islamic-civil",
        "ethiopic-amete-alem" => "ethioaa",
        "gregorian" => "gregory",
        other => other,
    };
    KNOWN_CALENDARS.iter().copied().find(|&k| k == mapped)
}
fn cal_ok(cal: &Option<String>) -> bool {
    match cal {
        Some(c) => canon_cal(c).is_some(),
        None => true,
    }
}

/// Whether an ISO date lies within the representable range (`-271821-04-19`..`+275760-09-13`).
fn date_in_range(d: IsoDate) -> bool {
    let ed = epoch_days(d);
    ed >= days_from_civil(-271821, 4, 19) && ed <= days_from_civil(275760, 9, 13)
}

/// Parse a `Temporal.PlainYearMonth` string (a year-month, or a full date-time taking year+month).
fn parse_year_month(s: &str) -> Option<IsoDate> {
    if let Some(p) = parse_iso(s) {
        if let Some(d) = p.date {
            if p.offset == Off::Z || !cal_ok(&p.calendar) || !ym_in_range(d.year, d.month) {
                return None;
            }
            return Some(IsoDate {
                year: d.year,
                month: d.month,
                day: 1,
            });
        }
        // a bare time falls through to the year-month grammar below
    }
    let mut c = Cur::new(s);
    let year = p_year(&mut c)?;
    c.eat(b'-');
    let month = c.num(2)?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let (cal, _tz) = p_annotations(&mut c)?;
    if !c.done() || !cal_ok(&cal) || !ym_in_range(year, month as u8) {
        return None;
    }
    Some(IsoDate {
        year,
        month: month as u8,
        day: 1,
    })
}

/// Whether a year-month is representable (its first or last day lies within range).
fn ym_in_range(year: i64, month: u8) -> bool {
    date_in_range(IsoDate {
        year,
        month,
        day: 1,
    }) || date_in_range(IsoDate {
        year,
        month,
        day: days_in_month(year, month),
    })
}

/// Parse a `Temporal.PlainMonthDay` string (a month-day, or a full date taking month+day). The
/// year is irrelevant, so out-of-range years are accepted.
fn parse_month_day(s: &str) -> Option<IsoDate> {
    if let Some(p) = parse_iso(s) {
        if let Some(d) = p.date {
            if p.offset == Off::Z || !cal_ok(&p.calendar) {
                return None;
            }
            return Some(IsoDate {
                year: 1972,
                month: d.month,
                day: d.day,
            });
        }
    }
    let mut c = Cur::new(s);
    if c.peek() == Some(b'-') {
        if c.at(1) != Some(b'-') {
            return None;
        }
        c.i += 2;
    }
    let month = c.num(2)?;
    c.eat(b'-');
    let day = c.num(2)?;
    if !(1..=12).contains(&month) || day < 1 || day as u8 > days_in_month(1972, month as u8) {
        return None;
    }
    let (cal, _tz) = p_annotations(&mut c)?;
    if !c.done() || !cal_ok(&cal) {
        return None;
    }
    Some(IsoDate {
        year: 1972,
        month: month as u8,
        day: day as u8,
    })
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
    // Duration keeps its plain toString for toLocaleString (Intl.DurationFormat is separate); the
    // date/time types format through Intl.DateTimeFormat, which understands Temporal receivers.
    for name in ["Duration"] {
        if let Some(proto) = it.extra_protos.get(format!("Temporal.{name}").as_str()).cloned() {
            let ts = proto.borrow().props.get("toString").cloned();
            if let Some(p) = ts {
                proto.borrow_mut().props.insert("toLocaleString", p);
            }
        }
    }
    for name in ["PlainDate", "PlainTime", "PlainDateTime", "PlainYearMonth", "PlainMonthDay", "Instant"] {
        if let Some(proto) = it.extra_protos.get(format!("Temporal.{name}").as_str()).cloned() {
            it.def_method(&proto, "toLocaleString", 0, |i, this, a| {
                let intl = i.get_member(&Value::Obj(i.global.clone()), "Intl").map_err(unab)?;
                let ctor = i.get_member(&intl, "DateTimeFormat").map_err(unab)?;
                let locales = a.first().cloned().unwrap_or(Value::Undefined);
                let options = a.get(1).cloned().unwrap_or(Value::Undefined);
                let dtf = i.construct(ctor, &[locales, options]).map_err(unab)?;
                let fmt = i.get_member(&dtf, "format").map_err(unab)?;
                i.call(fmt, dtf, &[this]).map_err(unab)
            });
        }
    }
    // Temporal.ZonedDateTime.prototype.toLocaleString: the formatter's time zone is forced to the
    // instance's (a `timeZone` option is disallowed), sensible date+time+zone-name defaults are
    // applied, and the underlying instant is formatted through Intl.DateTimeFormat.
    if let Some(proto) = it.extra_protos.get("Temporal.ZonedDateTime").cloned() {
        it.def_method(&proto, "toLocaleString", 0, zoned_to_locale_string);
    }
    it.global
        .borrow_mut()
        .props
        .insert("Temporal", Property::builtin(Value::Obj(ns)));
}

/// Temporal.ZonedDateTime.prototype.toLocaleString(locales, options): the instance's time zone is
/// forced onto the formatter (a `timeZone` option is a TypeError), the instance's calendar must be
/// ISO or match the formatter's, sensible date+time+zone-name defaults apply when nothing was asked
/// for, and the underlying instant is then formatted through Intl.DateTimeFormat.
fn zoned_to_locale_string(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    use crate::value::set_data;
    let obj = match this.as_obj() {
        Some(o) => o.clone(),
        None => return Err(i.make_error("TypeError", "not an object")),
    };
    let ptr = Rc::as_ptr(&obj) as usize;
    let (epoch_ns, tz) = match i.temporal.get(&ptr) {
        Some(Temporal::Zoned { epoch_ns, tz, .. }) => (*epoch_ns, tz.clone()),
        _ => {
            return Err(i.make_error(
                "TypeError",
                "Temporal.ZonedDateTime.prototype.toLocaleString called on an incompatible receiver",
            ))
        }
    };
    let zcal = i.temporal_cal.get(&ptr).map(|c| c.to_string()).unwrap_or_else(|| "iso8601".to_string());

    let locales = a.first().cloned().unwrap_or(Value::Undefined);
    let user_opts = a.get(1).cloned().unwrap_or(Value::Undefined);
    let user_obj = match &user_opts {
        Value::Undefined => None,
        Value::Obj(o) => Some(o.clone()),
        _ => return Err(i.make_error("TypeError", "options must be an object")),
    };

    // Effective options inherit from the user's so getters still fire; own props shadow.
    let opts = i.new_object();
    let mut any_comp = false;
    if let Some(uo) = &user_obj {
        // A timeZone option is disallowed (the instance's zone always wins).
        if !matches!(i.get_member(&user_opts, "timeZone").map_err(unab)?, Value::Undefined) {
            return Err(i.make_error(
                "TypeError",
                "the timeZone option is not allowed for Temporal.ZonedDateTime.prototype.toLocaleString",
            ));
        }
        opts.borrow_mut().proto = Some(uo.clone());
        for k in [
            "weekday", "era", "year", "month", "day", "dayPeriod", "hour", "minute", "second",
            "fractionalSecondDigits", "timeZoneName", "dateStyle", "timeStyle",
        ] {
            if !matches!(i.get_member(&user_opts, k).map_err(unab)?, Value::Undefined) {
                any_comp = true;
                break;
            }
        }
    }
    // No components requested: default to a full date+time plus the zone name.
    if !any_comp {
        for (k, v) in [
            ("year", "numeric"), ("month", "numeric"), ("day", "numeric"),
            ("hour", "numeric"), ("minute", "numeric"), ("second", "numeric"),
            ("timeZoneName", "short"),
        ] {
            set_data(&opts, k, Value::str(v));
        }
    }
    // Intl.DateTimeFormat canonicalizes the zone (Asia/Calcutta -> Asia/Kolkata); do it here so the
    // formatted zone name reflects the canonical identifier.
    let tz_canon = crate::tz::canonicalize(&tz).map(|s| s.to_string()).unwrap_or_else(|| tz.to_string());
    set_data(&opts, "timeZone", Value::from_string(tz_canon));

    let intl = i.get_member(&Value::Obj(i.global.clone()), "Intl").map_err(unab)?;
    let ctor = i.get_member(&intl, "DateTimeFormat").map_err(unab)?;
    let dtf = i.construct(ctor, &[locales, Value::Obj(opts)]).map_err(unab)?;

    // A non-ISO instance calendar must match the formatter's resolved calendar.
    let ropts_fn = i.get_member(&dtf, "resolvedOptions").map_err(unab)?;
    let ropts = i.call(ropts_fn, dtf.clone(), &[]).map_err(unab)?;
    let dcal = match i.get_member(&ropts, "calendar").map_err(unab)? {
        Value::Str(s) => s.to_string(),
        _ => "iso8601".to_string(),
    };
    if zcal != "iso8601" && zcal != dcal {
        return Err(i.make_error("RangeError", format!("calendar mismatch: {zcal} vs {dcal}")));
    }

    let ms = (epoch_ns / 1_000_000) as f64;
    let fmt = i.get_member(&dtf, "format").map_err(unab)?;
    i.call(fmt, dtf, &[Value::Num(ms)]).map_err(unab)
}

fn add_ctor(
    it: &mut Interp,
    ns: &Gc,
    name: &'static str,
    len: usize,
    proto: Gc,
    f: NativeFn,
) -> Gc {
    let ctor = it.make_native(name, len, f);
    ctor.borrow_mut().is_constructor = true;
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    if let Some(key) = crate::builtins::to_string_tag_key(it) {
        proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str(format!("Temporal.{name}")), false, false, true),
        );
    }
    ns.borrow_mut()
        .props
        .insert(name, Property::builtin(Value::Obj(ctor.clone())));
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

    def_getter(it, &proto, "year", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(cal_year_num(&cal_of(i, &t), d) as f64))
    });
    def_getter(it, &proto, "era", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(cal_era(&cal_of(i, &t), d).0.map(Value::str).unwrap_or(Value::Undefined))
    });
    def_getter(it, &proto, "eraYear", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(cal_era(&cal_of(i, &t), d).1.map(|e| Value::Num(e as f64)).unwrap_or(Value::Undefined))
    });
    def_getter(it, &proto, "month", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).1 as f64))
    });
    def_getter(it, &proto, "day", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).2 as f64))
    });
    def_getter(it, &proto, "monthCode", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::from_string(cal_month_code(&cal_of(i, &t), d)))
    });
    def_getter(it, &proto, "calendarId", |i, t, _| Ok(Value::from_string(cal_of(i, &t).to_string())));
    def_getter(it, &proto, "dayOfWeek", |i, t, _| {
        Ok(Value::Num(iso_day_of_week(as_date(i, &t)?) as f64))
    });
    def_getter(it, &proto, "dayOfYear", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).5 as f64))
    });
    def_getter(it, &proto, "weekOfYear", |i, t, _| {
        Ok(Value::Num(iso_week(as_date(i, &t)?).0 as f64))
    });
    def_getter(it, &proto, "yearOfWeek", |i, t, _| {
        Ok(Value::Num(iso_week(as_date(i, &t)?).1 as f64))
    });
    def_getter(it, &proto, "daysInWeek", |i, t, _| {
        as_date(i, &t)?;
        Ok(Value::Num(7.0))
    });
    def_getter(it, &proto, "daysInMonth", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).3 as f64))
    });
    def_getter(it, &proto, "daysInYear", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).6 as f64))
    });
    def_getter(it, &proto, "monthsInYear", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).4 as f64))
    });
    def_getter(it, &proto, "inLeapYear", |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(Value::Bool(cal_fields(&cal_of(i, &t), d).7))
    });

    it.def_method(&proto, "toString", 0, |i, t, a| {
        let d = as_date(i, &t)?;
        Ok(Value::str(format!(
            "{}{}",
            fmt_date(d),
            cal_suffix(i, &arg(a, 0), &cal_of(i, &t))?
        )))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        Ok(Value::str(fmt_date(as_date(i, &t)?)))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error(
            "TypeError",
            "Temporal.PlainDate has no valueOf; use compare",
        ))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let o = to_date(i, &arg(a, 0), &Value::Undefined)?;
        Ok(Value::Bool(
            d.year == o.year && d.month == o.month && d.day == o.day,
        ))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "with() argument must be an object"));
        }
        let cal = cal_of(i, &t);
        let ovf = to_overflow(i, &arg(a, 1))?;
        let nd = if &*cal == "iso8601" {
            let year = field_int(i, &f, "year", d.year)?;
            let month = field_int(i, &f, "month", d.month as i64)?;
            let day = field_int(i, &f, "day", d.day as i64)?;
            build_date_ovf(i, year, month, day, ovf)?
        } else {
            with_cal_date(i, &cal, d, &f, ovf)?
        };
        Ok(make_like(i, &t, "Temporal.PlainDate", Temporal::Date(nd)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal_id = cal_of(i, &t);
        let nd = add_to_date(i, d, dur, 1, ovf, &cal_id)?;
        Ok(make_like(i, &t, "Temporal.PlainDate", Temporal::Date(nd)))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal_id = cal_of(i, &t);
        let nd = add_to_date(i, d, dur, -1, ovf, &cal_id)?;
        Ok(make_like(i, &t, "Temporal.PlainDate", Temporal::Date(nd)))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let o = to_date(i, &arg(a, 0), &Value::Undefined)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        let (largest, smallest, incr, mode) = read_date_diff(i, &arg(a, 1))?;
        Ok(make(
            i,
            "Temporal.Duration",
            Temporal::Duration(diff_date_rounded(&cal, d, o, &largest, &smallest, incr, &mode)),
        ))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let o = to_date(i, &arg(a, 0), &Value::Undefined)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        // `since` mirrors `until` with a negated rounding mode, then negates the result.
        let (largest, smallest, incr, mode) = read_date_diff(i, &arg(a, 1))?;
        let dur = diff_date_rounded(&cal, d, o, &largest, &smallest, incr, negate_mode(&mode));
        Ok(make(i, "Temporal.Duration", Temporal::Duration(neg_duration(dur))))
    });
    it.def_method(&proto, "toPlainDateTime", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let time = match arg(a, 0) {
            Value::Undefined => IsoTime {
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                us: 0,
                ns: 0,
            },
            v => to_time(i, &v, &Value::Undefined)?,
        };
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(d, time)))
    });
    it.def_method(&proto, "toPlainYearMonth", 0, |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(make_like(i, &t, "Temporal.PlainYearMonth", Temporal::YearMonth(d)))
    });
    it.def_method(&proto, "toPlainMonthDay", 0, |i, t, _| {
        let d = as_date(i, &t)?;
        Ok(make_like(i, &t, "Temporal.PlainMonthDay", Temporal::MonthDay(d)))
    });
    it.def_method(&proto, "withCalendar", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let cal = check_calendar(i, &arg(a, 0))?;
        let v = make(i, "Temporal.PlainDate", Temporal::Date(d));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&proto, "toZonedDateTime", 1, |i, t, a| {
        let d = as_date(i, &t)?;
        let item = arg(a, 0);
        let (tzv, timev) = match &item {
            Value::Obj(_) => (getm(i, &item, "timeZone")?, getm(i, &item, "plainTime")?),
            other => (other.clone(), Value::Undefined),
        };
        let tz_raw: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let tz = normalize_tz(i, &tz_raw)?;
        let time = match timev {
            Value::Undefined => IsoTime {
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                us: 0,
                ns: 0,
            },
            v => to_time(i, &v, &Value::Undefined)?,
        };
        let local = dt_ns(d, time);
        let offset = offset_for_local(&tz, local);
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: local - offset as i128, offset_ns: offset, tz, }))
    });

    let ctor = add_ctor(it, ns, "PlainDate", 3, proto, |i, _t, a| {
        require_new(i)?;
        let year = to_int(i, &arg(a, 0))?;
        let month = to_int(i, &arg(a, 1))?;
        let day = to_int(i, &arg(a, 2))?;
        let cal = check_calendar(i, &arg(a, 3))?;
        let d = build_date(i, year, month, day)?;
        let v = make(i, "Temporal.PlainDate", Temporal::Date(d));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let (d, cal) = to_date_cal(i, &arg(a, 0), &arg(a, 1))?;
        let v = make(i, "Temporal.PlainDate", Temporal::Date(d));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_date(i, &arg(a, 0), &Value::Undefined)?;
        let y = to_date(i, &arg(a, 1), &Value::Undefined)?;
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
/// round()/total() accept a bare string as the `smallestUnit`/`unit` shorthand; otherwise the
/// argument is an options object. Returns (options-object-or-undefined, shorthand-unit).
fn round_opts(arg0: &Value) -> (Value, Option<String>) {
    if let Value::Str(s) = arg0 {
        (Value::Undefined, Some(s.to_string()))
    } else {
        (arg0.clone(), None)
    }
}

/// GetOption(string) validated against an explicit value list (RangeError on a value not in it).
fn opt_enum(i: &mut Interp, opts: &Value, key: &str, values: &[&str], default: &str) -> Result<String, Value> {
    let s = opt_str(i, opts, key, default)?;
    if !values.contains(&s.as_str()) {
        return Err(i.make_error("RangeError", format!("invalid {key}: {s}")));
    }
    Ok(s)
}
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
/// Nanoseconds per time unit, or None for calendar units. Accepts singular and plural unit names.
fn unit_ns(u: &str) -> Option<i128> {
    Some(match sing(u) {
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
        // Calendar units (year/month/week/day) have no divisibility ceiling — any positive integer
        // increment is valid (the value is simply rounded to that multiple).
        _ => return Ok(()),
    };
    if incr >= max || max % incr != 0 {
        return Err(i.make_error("RangeError", "roundingIncrement out of range"));
    }
    Ok(())
}
/// Validate a `roundingMode` option, else RangeError.
fn check_mode(i: &Interp, m: &str) -> Result<(), Value> {
    const MODES: [&str; 9] = [
        "ceil",
        "floor",
        "expand",
        "trunc",
        "halfCeil",
        "halfFloor",
        "halfExpand",
        "halfTrunc",
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
    let q = value.div_euclid(inc); // floor
    let r = value.rem_euclid(inc); // always >= 0
    if r == 0 {
        return value;
    }
    let floor = q * inc; // toward -inf
    let ceil = floor + inc; // toward +inf
                            // `ceil`/`expand` and `floor`/`trunc` differ for negative values; half-modes break ties.
    let to_ceil = match mode {
        "ceil" => true,
        "floor" => false,
        "trunc" => value < 0,
        "expand" => value >= 0,
        _ => match (r * 2).cmp(&inc) {
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => match mode {
                "halfCeil" => true,
                "halfFloor" => false,
                "halfTrunc" => value < 0,
                "halfEven" => q.rem_euclid(2) != 0,
                _ => value >= 0, // halfExpand (default)
            },
        },
    };
    if to_ceil {
        ceil
    } else {
        floor
    }
}

/// Canonical singular unit name (strip a trailing plural `s`).
fn sing(u: &str) -> &str {
    u.strip_suffix('s').unwrap_or(u)
}
/// Rank of a pure *time* unit (hour=5 … nanosecond=0), or None if it isn't one.
fn time_unit_rank(u: &str) -> Option<i32> {
    match unit_rank(u) {
        Some(r) if r <= 5 => Some(r),
        _ => None,
    }
}
/// Rank of a *date* unit (year=9 … day=6), or None if it isn't one.
fn date_unit_rank(u: &str) -> Option<i32> {
    match unit_rank(u) {
        Some(r) if r >= 6 => Some(r),
        _ => None,
    }
}
/// Read & validate the largestUnit for a date-only `until`/`since`: a date unit (year/month/week/day)
/// or "auto" (⇒ the larger of day and smallestUnit). RangeError if a unit isn't a date unit or
/// largestUnit is narrower than smallestUnit.
/// The unit name for a rank (inverse of [`unit_rank`]).
fn rank_unit(r: i32) -> &'static str {
    match r {
        9 => "year",
        8 => "month",
        7 => "week",
        6 => "day",
        5 => "hour",
        4 => "minute",
        3 => "second",
        2 => "millisecond",
        1 => "microsecond",
        _ => "nanosecond",
    }
}
/// Negate a roundingMode (for `since`, which mirrors `until`): ceil↔floor, halfCeil↔halfFloor.
fn negate_mode(mode: &str) -> &str {
    match mode {
        "ceil" => "floor",
        "floor" => "ceil",
        "halfCeil" => "halfFloor",
        "halfFloor" => "halfCeil",
        other => other,
    }
}

/// Resolve `until`/`since` options for a pure-time difference and produce the rounded, balanced
/// duration. `diff_ns` is `other - this` in nanoseconds; `since` negates the rounding mode and the
/// result. Validates that both units are time units with `largestUnit >= smallestUnit`.
fn time_diff(
    i: &mut Interp,
    diff_ns: i128,
    opts: &Value,
    since: bool,
    auto_rank: i32,
) -> Result<IsoDuration, Value> {
    let largest_raw = opt_str(i, opts, "smallestUnit", "nanosecond")?;
    let smallest = sing(&largest_raw).to_string();
    let srank = time_unit_rank(&smallest)
        .ok_or_else(|| i.make_error("RangeError", "smallestUnit must be a time unit"))?;
    let largest_opt = opt_str(i, opts, "largestUnit", "auto")?;
    let largest_name = sing(&largest_opt).to_string();
    let lrank = if largest_name == "auto" {
        srank.max(auto_rank) // auto ⇒ the type default, but never narrower than smallestUnit
    } else {
        time_unit_rank(&largest_name)
            .ok_or_else(|| i.make_error("RangeError", "largestUnit must be a time unit"))?
    };
    if lrank < srank {
        return Err(i.make_error(
            "RangeError",
            "largestUnit cannot be smaller than smallestUnit",
        ));
    }
    let incr = opt_num(i, opts, "roundingIncrement", 1)?;
    check_increment(i, &smallest, incr)?;
    let mode = opt_str(i, opts, "roundingMode", "trunc")?;
    check_mode(i, &mode)?;
    let mode = if since { negate_mode(&mode) } else { &mode }.to_string();
    let unit_size = unit_ns(&smallest).unwrap();
    let rounded = round_ns(diff_ns, unit_size * incr as i128, &mode);
    let bal = balance_ns(rounded, rank_unit(lrank));
    Ok(if since { neg_duration(bal) } else { bal })
}

/// Rank of a temporal unit (years highest), or None if not a unit name.
fn unit_rank(u: &str) -> Option<i32> {
    Some(match sing(u) {
        "year" => 9,
        "month" => 8,
        "week" => 7,
        "day" => 6,
        "hour" => 5,
        "minute" => 4,
        "second" => 3,
        "millisecond" => 2,
        "microsecond" => 1,
        "nanosecond" => 0,
        _ => return None,
    })
}
/// The largest non-zero unit of a duration (singular), defaulting to nanosecond.
fn default_largest(d: &IsoDuration) -> &'static str {
    if d.years != 0 {
        "year"
    } else if d.months != 0 {
        "month"
    } else if d.weeks != 0 {
        "week"
    } else if d.days != 0 {
        "day"
    } else if d.hours != 0 {
        "hour"
    } else if d.minutes != 0 {
        "minute"
    } else if d.seconds != 0 {
        "second"
    } else if d.ms != 0 {
        "millisecond"
    } else if d.us != 0 {
        "microsecond"
    } else {
        "nanosecond"
    }
}
/// Round `num`/`den` (den > 0) to the nearest integer using a rounding mode.
fn round_div(num: i128, den: i128, mode: &str) -> i128 {
    let q = num.div_euclid(den);
    let r = num.rem_euclid(den);
    if r == 0 {
        return q;
    }
    let up = match mode {
        "ceil" => true,
        "floor" => false,
        "trunc" => num < 0,
        "expand" => num > 0,
        _ => match (r * 2).cmp(&den) {
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => match mode {
                "halfCeil" => true,
                "halfFloor" => false,
                "halfTrunc" => num < 0,
                "halfEven" => q.rem_euclid(2) != 0,
                _ => num > 0, // halfExpand (default)
            },
        },
    };
    if up {
        q + 1
    } else {
        q
    }
}
/// Correctly-rounded conversion of an exact rational `num`/`den` to the nearest f64.
fn ratio_to_f64(num: i128, den: i128) -> f64 {
    if num == 0 {
        return 0.0;
    }
    let neg = (num < 0) ^ (den < 0);
    let mut n = num.unsigned_abs();
    let mut d = den.unsigned_abs();
    let mut exp: i32 = 0;
    let lo = 1u128 << 52;
    let hi = 1u128 << 53;
    while n / d < lo {
        n <<= 1;
        exp -= 1;
    }
    while n / d >= hi {
        d <<= 1;
        exp += 1;
    }
    let q = n / d;
    let r = n % d;
    let mut m = q;
    let two_r = r * 2;
    if two_r > d || (two_r == d && (q & 1) == 1) {
        m += 1;
    }
    let val = (m as f64) * (exp as f64).exp2();
    if neg {
        -val
    } else {
        val
    }
}
/// Round a calendar duration to a calendar `unit` (year/month/week/day) relative to `rel`.
/// `dest` is the duration's nanosecond span from `rel`; `bal` is its date part balanced to the
/// target largest unit. Returns the rounded date duration (time fields zero); does not bubble up.
fn round_calendar_unit(
    rel: IsoDate,
    dest: i128,
    bal: &IsoDuration,
    unit: &str,
    incr: i128,
    sign: i64,
    mode: &str,
) -> IsoDuration {
    let day_ns = 86_400_000_000_000i128;
    let comp = match unit {
        "year" => bal.years,
        "month" => bal.months,
        "week" => bal.weeks,
        _ => bal.days,
    };
    let incr = incr as i64;
    let j1 = comp / incr; // truncate toward zero
    let mk = |c: i64| match unit {
        "year" => IsoDuration {
            years: c,
            ..Default::default()
        },
        "month" => IsoDuration {
            years: bal.years,
            months: c,
            ..Default::default()
        },
        "week" => IsoDuration {
            years: bal.years,
            months: bal.months,
            weeks: c,
            ..Default::default()
        },
        _ => IsoDuration {
            years: bal.years,
            months: bal.months,
            weeks: bal.weeks,
            days: c,
            ..Default::default()
        },
    };
    let sd = add_date_dur(rel, mk(j1 * incr));
    let ed = add_date_dur(rel, mk((j1 + sign) * incr));
    let start_ns = (epoch_days(sd) - epoch_days(rel)) as i128 * day_ns;
    let end_ns = (epoch_days(ed) - epoch_days(rel)) as i128 * day_ns;
    let den_p = end_ns - start_ns;
    if den_p == 0 {
        return mk(j1 * incr);
    }
    let num_p = dest - start_ns;
    let mut vn = j1 as i128 * den_p + num_p * sign as i128;
    let mut vd = den_p;
    if vd < 0 {
        vn = -vn;
        vd = -vd;
    }
    let j = round_div(vn, vd, mode) as i64;
    mk(j * incr)
}

/// Difference between two ISO dates as a calendar duration honoring `largest`
/// (years/months/weeks/days). Assumes nothing about ordering; the result carries the sign.
fn diff_date(a: IsoDate, b: IsoDate, largest: &str) -> IsoDuration {
    let largest = largest.strip_suffix('s').unwrap_or(largest); // accept plural unit names
    let c = cmp_date(a, b);
    let mut out = IsoDuration::default();
    if c == 0 {
        return out;
    }
    match largest {
        "year" | "month" => {
            // `a`-anchored field difference (per Temporal's DifferenceISODate). `sign` is the sign of
            // the result (b - a). A month only counts if `a`'s day fits in the target month: reaching
            // b's month solely by day-clamping (e.g. Jan 29 → Feb 28) does NOT count as a whole month.
            let sign: i64 = if c < 0 { 1 } else { -1 };
            let mut years = if largest == "year" { b.year - a.year } else { 0 };
            let mut months = if largest == "year" {
                b.month as i64 - a.month as i64
            } else {
                (b.year - a.year) * 12 + (b.month as i64 - a.month as i64)
            };
            loop {
                let mid = constrain_add_ym(a, years * 12 + months);
                let cc = cmp_date(mid, b);
                let mid_sign = if cc < 0 { 1 } else if cc > 0 { -1 } else { 0 };
                let clamped = a.day as i64 > days_in_month(mid.year, mid.month) as i64;
                if mid_sign == -sign || (mid_sign == 0 && clamped) {
                    months -= sign; // overshot, or landed on b only via clamping
                } else {
                    break;
                }
            }
            // Rebalance a month component that borrowed against the year total.
            if largest == "year" && months * sign < 0 {
                years -= sign;
                months += sign * 12;
            }
            let mid = constrain_add_ym(a, years * 12 + months);
            let days = if mid.year == b.year && mid.month == b.month {
                b.day as i64 - mid.day as i64
            } else if sign < 0 {
                -(mid.day as i64) - (days_in_month(b.year, b.month) as i64 - b.day as i64)
            } else {
                b.day as i64 + (days_in_month(mid.year, mid.month) as i64 - mid.day as i64)
            };
            if largest == "month" {
                months += years * 12;
                years = 0;
            }
            out.years = years;
            out.months = months;
            out.days = days;
        }
        "week" => {
            let total = epoch_days(b) - epoch_days(a);
            out.weeks = total / 7;
            out.days = total % 7;
        }
        _ => {
            out.days = epoch_days(b) - epoch_days(a);
        }
    }
    out
}

/// Whether to round the magnitude up given the discarded `fraction` in [0,1), the `mode`, the sign
/// of the signed result (`positive`), and the parity of the retained value (for `halfEven`).
fn round_up_magnitude(mode: &str, fraction: f64, positive: bool, low_even: bool) -> bool {
    match mode {
        "trunc" => false,
        "expand" => fraction > 0.0,
        "ceil" => positive && fraction > 0.0,
        "floor" => !positive && fraction > 0.0,
        "halfExpand" => fraction >= 0.5,
        "halfTrunc" => fraction > 0.5,
        "halfCeil" => {
            if positive { fraction >= 0.5 } else { fraction > 0.5 }
        }
        "halfFloor" => {
            if positive { fraction > 0.5 } else { fraction >= 0.5 }
        }
        "halfEven" => fraction > 0.5 || (fraction == 0.5 && !low_even),
        _ => false,
    }
}

/// A PlainDate difference (`a` → `b`) balanced to `largest`, then rounded to `smallest` with
/// `increment`/`mode`. `mode` is oriented for the caller (`since` passes a negated mode). The
/// fractional part of the smallest unit is measured by interpolating the target date between the
/// two candidate boundary dates (calendar-accurate for the ISO calendar).
/// Calendar-aware date difference for month-structure calendars: years/months are counted in the
/// calendar's own months. For day/week largest units (calendar-independent) it defers to `diff_date`.
/// Calendar-aware date difference for month-structure calendars, anchored at `a` and moving toward
/// `b` (like `diff_date`): years/months are counted with the calendar's own constrain-add, so a
/// backward difference borrows month lengths near `a`. Day/week largest units defer to `diff_date`.
fn diff_date_cal(cal: &str, a: IsoDate, b: IsoDate, largest: &str) -> IsoDuration {
    let largest = largest.strip_suffix('s').unwrap_or(largest);
    if !is_month_structure(cal) || matches!(largest, "week" | "day") {
        return diff_date(a, b, largest);
    }
    let sign = cmp_date(a, b);
    if sign == 0 {
        return IsoDuration::default();
    }
    let dir = if sign < 0 { 1 } else { -1 };
    let addc = |y: i64, m: i64| {
        cal_add_c(cal, a, IsoDuration { years: y, months: m, ..Default::default() }, dir)
    };
    let a_dom = cal_fields(cal, a).2;
    let a_code = cal_month_code(cal, a);
    let passed = |x: IsoDate| if dir > 0 { cmp_date(x, b) > 0 } else { cmp_date(x, b) < 0 };
    // A whole month reached exactly at `b` only because `a`'s day was clamped to a shorter target
    // month is not complete when moving *forward* (backward it counts, per ICU4X).
    let clamped_at = |m: IsoDate| dir > 0 && cmp_date(m, b) == 0 && cal_fields(cal, m).2 < a_dom;
    // The year step additionally must preserve the monthCode: landing on `b` only after a leap
    // monthCode was constrained to its plain form (M04L → M04 in a non-leap year) is not a whole year.
    // A year also fails to complete when a leap monthCode had to collapse to its plain form of the
    // SAME number (Chinese M04L → M04). A leap month that maps to a differently-numbered plain month
    // (Hebrew Adar I "M05L" → Adar "M06") is still a whole year.
    let year_clamped = |m: IsoDate| {
        clamped_at(m)
            || (dir > 0
                && cmp_date(m, b) == 0
                && a_code.ends_with('L')
                && a_code.trim_end_matches('L') == cal_month_code(cal, m))
    };
    // Largest year magnitude that has not passed `b`, backing off a clamped exact landing.
    let mut years = 0i64;
    if largest == "year" {
        while !passed(addc(years + 1, 0)) {
            years += 1;
        }
        if years > 0 && year_clamped(addc(years, 0)) {
            years -= 1;
        }
    }
    // Then the largest month magnitude on top of those years, again backing off a clamped landing.
    let mut months = 0i64;
    while !passed(addc(years, months + 1)) {
        months += 1;
    }
    if months > 0 && clamped_at(addc(years, months)) {
        months -= 1;
    }
    let mid = addc(years, months);
    let days = epoch_days(b) - epoch_days(mid);
    IsoDuration { years: dir * years, months: dir * months, days, ..Default::default() }
}

fn diff_date_rounded(cal: &str, a: IsoDate, b: IsoDate, largest: &str, smallest: &str, increment: i64, mode: &str) -> IsoDuration {
    let base = diff_date_cal(cal, a, b, largest);
    let smallest = smallest.strip_suffix('s').unwrap_or(smallest);
    if smallest == "day" && increment <= 1 {
        return base; // day differences are already whole
    }
    let sign = cmp_date(a, b);
    if sign == 0 {
        return base;
    }
    let positive = sign < 0; // a → b is positive when a precedes b
    let (lo, hi) = if positive { (a, b) } else { (b, a) };
    // Unsigned lo → hi difference, anchored at `lo`. (Not `neg_duration(base)`: because a difference
    // is anchored at its start date, `a.until(b)` negated is not `b.until(a)` when a month clamps.)
    let mag = diff_date_cal(cal, lo, hi, largest);

    // The candidate durations: truncated at `smallest`, and one increment above.
    let (low, field, base_units) = match smallest {
        "year" => (IsoDuration { years: mag.years, ..Default::default() }, 0u8, mag.years),
        "month" => (
            IsoDuration { years: mag.years, months: mag.months, ..Default::default() },
            1,
            mag.months,
        ),
        "week" => (
            IsoDuration { years: mag.years, months: mag.months, weeks: mag.weeks, ..Default::default() },
            2,
            mag.weeks,
        ),
        _ => {
            let d = mag.days - mag.days.rem_euclid(increment);
            (
                IsoDuration { years: mag.years, months: mag.months, weeks: mag.weeks, days: d, ..Default::default() },
                3,
                d / increment,
            )
        }
    };
    let mut high = low;
    match field {
        0 => high.years += increment,
        1 => high.months += increment,
        2 => high.weeks += increment,
        _ => high.days += increment,
    }
    // The boundary dates must be produced with the calendar's own month arithmetic (ISO
    // `add_date_dur` would place month boundaries on the wrong days for a non-ISO calendar).
    let cadd = |dur: IsoDuration| {
        if is_month_structure(cal) {
            cal_add_c(cal, lo, dur, 1)
        } else {
            add_date_dur(lo, dur)
        }
    };
    let dl = epoch_days(cadd(low));
    let dh = epoch_days(cadd(high));
    let dt = epoch_days(hi);
    let denom = (dh - dl) as f64;
    let fraction = if denom == 0.0 { 0.0 } else { (dt - dl) as f64 / denom };
    let up = round_up_magnitude(mode, fraction, positive, base_units % 2 == 0);
    let chosen = if up { high } else { low };
    // Re-balance the rounded date back to `largest`. This uses the *greedy* difference (a clamped
    // month counts as whole), NOT DifferenceDate's clamp-backoff — the rounded duration is exact and
    // must be preserved (e.g. relativeTo + 11 months rounded to months stays 11, not 10mo+30d).
    let result = diff_date_greedy(cal, lo, cadd(chosen), largest);
    if positive { result } else { neg_duration(result) }
}

/// A date difference (`a` → `b`, either direction) that greedily maximizes the larger units: a month
/// reached only by day-clamping still counts as a whole month. This is the balancing semantics used
/// by Duration rounding/totalling — unlike `diff_date`, which drops a clamped month for
/// `until`/`since`.
fn diff_date_greedy(cal: &str, a: IsoDate, b: IsoDate, largest: &str) -> IsoDuration {
    let largest = largest.strip_suffix('s').unwrap_or(largest);
    let mut out = IsoDuration::default();
    let c = cmp_date(a, b);
    if c == 0 {
        return out;
    }
    let dir = if c < 0 { 1 } else { -1 };
    let passed = |x: IsoDate| if dir > 0 { cmp_date(x, b) > 0 } else { cmp_date(x, b) < 0 };
    match largest {
        "week" => {
            let t = epoch_days(b) - epoch_days(a);
            out.weeks = t / 7;
            out.days = t % 7;
        }
        "day" => out.days = epoch_days(b) - epoch_days(a),
        _ if is_month_structure(cal) => {
            let ym = |y: i64, m: i64| IsoDuration { years: y, months: m, ..Default::default() };
            let mut years = 0i64;
            if largest == "year" {
                while !passed(cal_add_c(cal, a, ym(years + 1, 0), dir)) {
                    years += 1;
                }
            }
            let mut months = 0i64;
            while !passed(cal_add_c(cal, a, ym(years, months + 1), dir)) {
                months += 1;
            }
            let mid = cal_add_c(cal, a, ym(years, months), dir);
            out.years = dir * years;
            out.months = dir * months;
            out.days = epoch_days(b) - epoch_days(mid);
        }
        _ => {
            let mut tm = 0i64;
            while !passed(constrain_add_ym(a, dir * (tm + 1))) {
                tm += 1;
            }
            let mid = constrain_add_ym(a, dir * tm);
            if largest == "year" {
                out.years = dir * (tm / 12);
                out.months = dir * (tm % 12);
            } else {
                out.months = dir * tm;
            }
            out.days = epoch_days(b) - epoch_days(mid);
        }
    }
    out
}

/// Add a full duration (date + time parts) to a datetime, carrying time overflow into days.
fn add_dt_dur(d: IsoDate, t: IsoTime, dur: &IsoDuration) -> (IsoDate, IsoTime) {
    let date_only = IsoDuration {
        years: dur.years,
        months: dur.months,
        weeks: dur.weeks,
        days: dur.days,
        ..Default::default()
    };
    let nd = add_date_dur(d, date_only);
    let time_ns = dur.hours as i128 * 3_600_000_000_000
        + dur.minutes as i128 * 60_000_000_000
        + dur.seconds as i128 * 1_000_000_000
        + dur.ms as i128 * 1_000_000
        + dur.us as i128 * 1_000
        + dur.ns as i128;
    let total = time_to_ns(t) as i128 + time_ns;
    let carry = total.div_euclid(86_400_000_000_000);
    let rem = total.rem_euclid(86_400_000_000_000);
    let (y, m, da) = civil_from_days(epoch_days(nd) + carry as i64);
    (IsoDate { year: y, month: m, day: da }, ns_to_time(rem))
}

/// Like `add_dt_dur`, but adds the year/month/day part with calendar `cal`'s own arithmetic (for a
/// non-ISO PlainDateTime), carrying time overflow into the calendar day count.
fn add_dt_dur_cal(cal: &str, d: IsoDate, t: IsoTime, dur: &IsoDuration) -> (IsoDate, IsoTime) {
    if !is_month_structure(cal) {
        return add_dt_dur(d, t, dur);
    }
    let date_only = IsoDuration {
        years: dur.years,
        months: dur.months,
        weeks: dur.weeks,
        days: dur.days,
        ..Default::default()
    };
    let nd = cal_add_c(cal, d, date_only, 1);
    let time_ns = dur.hours as i128 * 3_600_000_000_000
        + dur.minutes as i128 * 60_000_000_000
        + dur.seconds as i128 * 1_000_000_000
        + dur.ms as i128 * 1_000_000
        + dur.us as i128 * 1_000
        + dur.ns as i128;
    let total = time_to_ns(t) as i128 + time_ns;
    let carry = total.div_euclid(86_400_000_000_000);
    let rem = total.rem_euclid(86_400_000_000_000);
    let (y, m, da) = civil_from_days(epoch_days(nd) + carry as i64);
    (IsoDate { year: y, month: m, day: da }, ns_to_time(rem))
}

/// Zero every duration field strictly smaller than the unit of rank `srank` (year=9 … ns=0).
fn zero_below(m: &mut IsoDuration, srank: i32) {
    if srank > 0 { m.ns = 0; }
    if srank > 1 { m.us = 0; }
    if srank > 2 { m.ms = 0; }
    if srank > 3 { m.seconds = 0; }
    if srank > 4 { m.minutes = 0; }
    if srank > 5 { m.hours = 0; }
    if srank > 6 { m.days = 0; }
    if srank > 7 { m.weeks = 0; }
    if srank > 8 { m.months = 0; }
}
/// The value of the duration field named by `unit`.
fn dur_field_val(m: &IsoDuration, unit: &str) -> i64 {
    match unit {
        "year" => m.years,
        "month" => m.months,
        "week" => m.weeks,
        "day" => m.days,
        "hour" => m.hours,
        "minute" => m.minutes,
        "second" => m.seconds,
        "millisecond" => m.ms,
        "microsecond" => m.us,
        _ => m.ns,
    }
}
/// Add `delta` to the duration field named by `unit`.
fn dur_field_add(m: &mut IsoDuration, unit: &str, delta: i64) {
    match unit {
        "year" => m.years += delta,
        "month" => m.months += delta,
        "week" => m.weeks += delta,
        "day" => m.days += delta,
        "hour" => m.hours += delta,
        "minute" => m.minutes += delta,
        "second" => m.seconds += delta,
        "millisecond" => m.ms += delta,
        "microsecond" => m.us += delta,
        _ => m.ns += delta,
    }
}

/// A PlainDateTime difference (`d1t1` → `d2t2`) balanced to `largest`, then rounded to `smallest`
/// with `increment`/`mode`. The fraction of the smallest unit is measured on the absolute-ns line
/// between the two candidate boundary datetimes.
#[allow(clippy::too_many_arguments)]
fn diff_datetime_rounded(
    cal: &str,
    d1: IsoDate, t1: IsoTime, d2: IsoDate, t2: IsoTime,
    largest: &str, smallest: &str, increment: i64, mode: &str,
) -> IsoDuration {
    let smallest = sing(smallest);
    let is_cal = matches!(largest, "year" | "month" | "week");
    let base = if is_cal {
        diff_datetime(cal, d1, t1, d2, t2, largest)
    } else {
        balance_ns(dt_ns(d2, t2) - dt_ns(d1, t1), largest)
    };
    let srank = unit_rank(smallest).unwrap_or(0);
    if srank == 0 && increment <= 1 {
        return base;
    }
    let a_ns = dt_ns(d1, t1);
    let b_ns = dt_ns(d2, t2);
    if a_ns == b_ns {
        return base;
    }
    let positive = a_ns < b_ns;
    let (lo_d, lo_t, hi_d, hi_t) = if positive { (d1, t1, d2, t2) } else { (d2, t2, d1, t1) };
    let mag = if positive { base } else { neg_duration(base) };
    let mut low = mag;
    zero_below(&mut low, srank);
    if increment > 1 {
        let v = dur_field_val(&low, smallest);
        dur_field_add(&mut low, smallest, -v.rem_euclid(increment));
    }
    let base_units = dur_field_val(&low, smallest) / increment.max(1);
    let mut high = low;
    dur_field_add(&mut high, smallest, increment);
    let (ld, lt) = add_dt_dur_cal(cal, lo_d, lo_t, &low);
    let (hd, ht) = add_dt_dur_cal(cal, lo_d, lo_t, &high);
    let low_ns = dt_ns(ld, lt);
    let high_ns = dt_ns(hd, ht);
    let target_ns = dt_ns(hi_d, hi_t);
    let denom = (high_ns - low_ns) as f64;
    let fraction = if denom == 0.0 { 0.0 } else { (target_ns - low_ns) as f64 / denom };
    let up = round_up_magnitude(mode, fraction, positive, base_units % 2 == 0);
    let chosen = if up { high } else { low };
    let (rd, rt) = add_dt_dur_cal(cal, lo_d, lo_t, &chosen);
    let result = if is_cal {
        diff_datetime(cal, lo_d, lo_t, rd, rt, largest)
    } else {
        balance_ns(dt_ns(rd, rt) - dt_ns(lo_d, lo_t), largest)
    };
    if positive { result } else { neg_duration(result) }
}

/// Read `until`/`since` options for a PlainDateTime difference: (largest, smallest, increment, mode).
/// DifferenceZonedDateTime: the difference between two instants in `tz`/`cal`. Time-only largest
/// units use a rounded nanosecond span; date units count whole calendar days in the time zone (a day
/// may be 23/25 hours across a DST transition) and balance the remaining time of day.
fn diff_zoned(
    _i: &Interp,
    e1: i128,
    o1: i64,
    e2: i128,
    tz: &str,
    cal: &str,
    largest: &str,
    smallest: &str,
    incr: i64,
    mode: &str,
) -> IsoDuration {
    if e1 == e2 {
        return IsoDuration::default();
    }
    if !matches!(largest, "year" | "month" | "week" | "day") {
        let un = unit_ns(smallest).unwrap_or(1);
        let rounded = round_ns(e2 - e1, un * incr as i128, mode);
        return balance_ns(rounded, largest);
    }
    let (d1, t1) = zoned_local(e1, o1);
    let sign = if e1 <= e2 { 1i64 } else { -1 };
    let add_days_ns = |days: i64| -> i128 {
        let (y, mo, da) = civil_from_days(epoch_days(d1) + days);
        let local = dt_ns(IsoDate { year: y, month: mo, day: da }, t1);
        local - offset_for_local(tz, local) as i128
    };
    let mut days = ((e2 - e1) / 86_400_000_000_000) as i64;
    for _ in 0..4 {
        let ns = add_days_ns(days);
        if sign > 0 {
            if ns > e2 {
                days -= 1;
            } else if add_days_ns(days + 1) <= e2 {
                days += 1;
            } else {
                break;
            }
        } else if ns < e2 {
            days += 1;
        } else if add_days_ns(days - 1) >= e2 {
            days -= 1;
        } else {
            break;
        }
    }
    let mid_ns = add_days_ns(days);
    let (my, mmo, mda) = civil_from_days(epoch_days(d1) + days);
    let mut out = diff_date_cal(cal, d1, IsoDate { year: my, month: mmo, day: mda }, largest);
    let tb = balance_ns(e2 - mid_ns, "hour");
    out.hours = tb.hours;
    out.minutes = tb.minutes;
    out.seconds = tb.seconds;
    out.ms = tb.ms;
    out.us = tb.us;
    out.ns = tb.ns;
    out
}

fn read_datetime_diff(i: &mut Interp, opts: &Value) -> Result<(String, String, i64, String), Value> {
    let largest_raw = sing(&opt_str(i, opts, "largestUnit", "auto")?).to_string();
    let incr = opt_num(i, opts, "roundingIncrement", 1)?;
    let mode = opt_str(i, opts, "roundingMode", "trunc")?;
    check_mode(i, &mode)?;
    let smallest = sing(&opt_str(i, opts, "smallestUnit", "nanosecond")?).to_string();
    let srank = unit_rank(&smallest).ok_or_else(|| i.make_error("RangeError", "invalid smallestUnit"))?;
    let lrank = if largest_raw == "auto" {
        srank.max(6)
    } else {
        unit_rank(&largest_raw).ok_or_else(|| i.make_error("RangeError", "invalid largestUnit"))?
    };
    if lrank < srank {
        return Err(i.make_error("RangeError", "largestUnit cannot be smaller than smallestUnit"));
    }
    check_increment(i, &smallest, incr)?;
    Ok((rank_unit(lrank).to_string(), smallest, incr, mode))
}

/// The ISO date of the first day of `iso`'s month in calendar `cal` — the reference date a
/// PlainYearMonth stores.
fn ym_ref_of(cal: &str, iso: IsoDate) -> IsoDate {
    if cal == "iso8601" {
        return IsoDate { year: iso.year, month: iso.month, day: 1 };
    }
    let day_of = cal_fields(cal, iso).2;
    let (y, m, d) = civil_from_days(epoch_days(iso) - (day_of - 1));
    IsoDate { year: y, month: m, day: d }
}
/// Add/subtract a duration to a PlainYearMonth in its calendar: anchored at the month's first day
/// (or last day when moving backwards), then reduced back to the resulting year-month.
fn ym_add(i: &mut Interp, cal: &str, d: IsoDate, dur: IsoDuration, sign: i64, ovf: Overflow) -> Result<IsoDate, Value> {
    // Anchor at day 1: it fits every month, so the synthetic day never triggers a day-overflow reject,
    // while a `reject` overflow still fires for a leap month absent in the target year (an independent
    // check in the calendar add).
    let result = add_to_date(i, ym_ref_of(cal, d), dur, sign, ovf, cal)?;
    Ok(ym_ref_of(cal, result))
}

/// Read `until`/`since` options for a PlainYearMonth difference: (largest, smallest, mode). Only
/// `year`/`month` units are allowed and `roundingIncrement` must be 1.
fn read_ym_diff(i: &mut Interp, opts: &Value) -> Result<(String, String, String), Value> {
    let smallest = sing(&opt_str(i, opts, "smallestUnit", "month")?).to_string();
    if !matches!(smallest.as_str(), "year" | "month") {
        return Err(i.make_error("RangeError", "smallestUnit must be year or month"));
    }
    let largest_raw = sing(&opt_str(i, opts, "largestUnit", "auto")?).to_string();
    let largest = if largest_raw == "auto" {
        if smallest == "year" { "year" } else { "year" }.to_string()
    } else {
        if !matches!(largest_raw.as_str(), "year" | "month") {
            return Err(i.make_error("RangeError", "largestUnit must be year or month"));
        }
        largest_raw
    };
    // largest (year=9) must not be narrower than smallest.
    if largest == "month" && smallest == "year" {
        return Err(i.make_error("RangeError", "largestUnit cannot be smaller than smallestUnit"));
    }
    let mode = opt_str(i, opts, "roundingMode", "trunc")?;
    check_mode(i, &mode)?;
    let incr = opt_num(i, opts, "roundingIncrement", 1)?;
    check_increment(i, &smallest, incr)?;
    Ok((largest, smallest, mode))
}

/// Read `until`/`since` options for a PlainDate difference: (largest, smallest, increment, mode).
/// GetDifferenceSettings reads the options in the order largestUnit, roundingIncrement,
/// roundingMode, smallestUnit, then validates the unit relationship.
fn read_date_diff(i: &mut Interp, opts: &Value) -> Result<(String, String, i64, String), Value> {
    let largest_raw = sing(&opt_str(i, opts, "largestUnit", "auto")?).to_string();
    let incr = opt_num(i, opts, "roundingIncrement", 1)?;
    let mode = opt_str(i, opts, "roundingMode", "trunc")?;
    check_mode(i, &mode)?;
    let smallest = sing(&opt_str(i, opts, "smallestUnit", "day")?).to_string();
    let srank = date_unit_rank(&smallest)
        .ok_or_else(|| i.make_error("RangeError", "smallestUnit must be a date unit"))?;
    let lrank = if largest_raw == "auto" {
        srank.max(6)
    } else {
        date_unit_rank(&largest_raw)
            .ok_or_else(|| i.make_error("RangeError", "largestUnit must be a date unit"))?
    };
    if lrank < srank {
        return Err(i.make_error("RangeError", "largestUnit cannot be smaller than smallestUnit"));
    }
    check_increment(i, &smallest, incr)?;
    Ok((rank_unit(lrank).to_string(), smallest, incr, mode))
}

/// Difference between two datetimes honoring a calendar `largest` unit (year/month/week/day) for the
/// date part and balancing the remaining time-of-day, with a borrow when the end time is earlier.
fn diff_datetime(cal: &str, d1: IsoDate, t1: IsoTime, d2: IsoDate, t2: IsoTime, largest: &str) -> IsoDuration {
    let a = dt_ns(d1, t1);
    let b = dt_ns(d2, t2);
    if a == b {
        return IsoDuration::default();
    }
    // Anchor the difference at (d1, t1) and move toward (d2, t2), matching `diff_date`. The time part
    // carries a whole-day borrow (in the direction of travel) when the end time-of-day would overshoot.
    let day_ns = 86_400_000_000_000i64;
    let sign = if a < b { 1 } else { -1 };
    let mut tdiff = time_to_ns(t2) - time_to_ns(t1);
    let mut end_date = d2;
    if sign > 0 && tdiff < 0 {
        tdiff += day_ns;
        let (y, m, da) = civil_from_days(epoch_days(d2) - 1);
        end_date = IsoDate { year: y, month: m, day: da };
    } else if sign < 0 && tdiff > 0 {
        tdiff -= day_ns;
        let (y, m, da) = civil_from_days(epoch_days(d2) + 1);
        end_date = IsoDate { year: y, month: m, day: da };
    }
    let mut out = diff_date_cal(cal, d1, end_date, largest); // signed (anchored at d1)
    let time = balance_ns(tdiff as i128, "hour");
    out.hours = time.hours;
    out.minutes = time.minutes;
    out.seconds = time.seconds;
    out.ms = time.ms;
    out.us = time.us;
    out.ns = time.ns;
    out
}

/// Add `months` months to a date, clamping the day to the resulting month's length.
fn constrain_add_ym(d: IsoDate, months: i64) -> IsoDate {
    let total = d.year * 12 + (d.month as i64 - 1) + months;
    let y = total.div_euclid(12);
    let m = (total.rem_euclid(12) + 1) as u8;
    let day = (d.day).min(days_in_month(y, m));
    IsoDate {
        year: y,
        month: m,
        day,
    }
}

/// Overflow handling mode for out-of-range date/time components.
#[derive(Clone, Copy, PartialEq)]
enum Overflow {
    Constrain,
    Reject,
}

/// GetOptionsObject + GetTemporalOverflowOption: validate the options arg type (a non-object,
/// non-undefined primitive is a TypeError) and read/validate the `overflow` option.
fn to_overflow(i: &mut Interp, opts: &Value) -> Result<Overflow, Value> {
    match opt_str(i, opts, "overflow", "constrain")?.as_str() {
        "constrain" => Ok(Overflow::Constrain),
        "reject" => Ok(Overflow::Reject),
        other => Err(i.make_error("RangeError", format!("invalid overflow ({other})"))),
    }
}
/// Regulate a time component to `[lo, hi]`: clamp under `constrain`, throw under `reject`.
fn regulate(
    i: &Interp,
    val: i64,
    lo: i64,
    hi: i64,
    ovf: Overflow,
    what: &str,
) -> Result<i64, Value> {
    if ovf == Overflow::Reject && (val < lo || val > hi) {
        return Err(i.make_error("RangeError", format!("{what} out of range")));
    }
    Ok(val.clamp(lo, hi))
}
/// Regulate a month/day component: the `>= 1` floor (from ToPositiveIntegerWithTruncation) always
/// applies; the calendar ceiling `hi` is governed by `overflow`.
fn regulate_high(i: &Interp, val: i64, hi: i64, ovf: Overflow, what: &str) -> Result<i64, Value> {
    if val < 1 {
        return Err(i.make_error("RangeError", format!("{what} out of range")));
    }
    if val > hi {
        if ovf == Overflow::Reject {
            return Err(i.make_error("RangeError", format!("{what} out of range")));
        }
        return Ok(hi);
    }
    Ok(val)
}
/// CanonicalizeCalendar of a calendar id string: only the ISO calendar is supported. A bare id is
/// matched case-insensitively against "iso8601"; otherwise the id may be a full ISO date/datetime
/// string whose optional `[u-ca=...]` annotation must itself resolve to the ISO calendar.
fn canon_calendar(i: &Interp, s: &str) -> Result<(), Value> {
    if canon_cal(s).is_some() {
        return Ok(());
    }
    if parse_iso(s).is_some() {
        return check_str_calendar(i, s);
    }
    Err(i.make_error("RangeError", format!("unknown calendar: {s}")))
}
/// GetTemporalCalendarIdentifierWithISODefault: read & validate a property bag's `calendar` field.
fn read_calendar(i: &mut Interp, o: &Value) -> Result<(), Value> {
    let c = getm(i, o, "calendar")?;
    match &c {
        Value::Undefined => Ok(()),
        Value::Str(s) => canon_calendar(i, s),
        Value::Obj(_) => match get(i, &c) {
            Some(Temporal::Date(_))
            | Some(Temporal::DateTime(_, _))
            | Some(Temporal::YearMonth(_))
            | Some(Temporal::MonthDay(_))
            | Some(Temporal::Zoned { .. }) => Ok(()),
            _ => Err(i.make_error("TypeError", "calendar is not a string")),
        },
        _ => Err(i.make_error("TypeError", "calendar is not a string")),
    }
}
/// The first `[u-ca=...]` annotation value in an ISO string, if any.
fn calendar_annotation(s: &str) -> Option<String> {
    let mut rest = s;
    while let Some(start) = rest.find('[') {
        let end = rest[start..].find(']')? + start;
        let inner = &rest[start + 1..end];
        let body = inner.strip_prefix('!').unwrap_or(inner);
        if let Some(v) = body.strip_prefix("u-ca=") {
            return Some(v.to_string());
        }
        rest = &rest[end + 1..];
    }
    None
}
/// Validate the (optional) calendar annotation of an ISO string used to build a calendared type:
/// the annotation, if present, must resolve to the ISO calendar (case-insensitive).
fn check_str_calendar(i: &Interp, s: &str) -> Result<(), Value> {
    match calendar_annotation(s) {
        Some(cal) if canon_cal(&cal).is_none() => {
            Err(i.make_error("RangeError", format!("unknown calendar: {cal}")))
        }
        _ => Ok(()),
    }
}
/// Copy the six date fields out of `v` (reading each once, in `read_date_raw_cal`'s field order) into
/// a plain object, so the calendar resolution can be deferred past the overflow option without
/// re-triggering the source's getters.
fn snapshot_date_fields(i: &mut Interp, v: &Value) -> Result<Value, Value> {
    let snap = i.new_object();
    for k in ["day", "era", "eraYear", "month", "monthCode", "year"] {
        let fv = getm(i, v, k)?;
        if !matches!(fv, Value::Undefined) {
            setm(&snap, k, fv);
        }
    }
    Ok(Value::Obj(snap))
}
/// Clamp `val` into `[1, hi]`, or (under `overflow: reject`) raise a RangeError if it was out of range.
fn clamp_or(i: &Interp, val: i64, hi: i64, ovf: Overflow, what: &str) -> Result<i64, Value> {
    if ovf == Overflow::Reject && (val < 1 || val > hi) {
        return Err(i.make_error("RangeError", format!("{what} is out of range")));
    }
    Ok(val.clamp(1, hi))
}
/// Read raw (year, month, day) from a property bag, resolving `era`/`eraYear` to an ISO year for
/// the era-based calendars (gregory/japanese use CE=`ce`, BCE=`bce`). Out-of-range calendar fields
/// are clamped under `overflow: constrain` and rejected under `overflow: reject`.
fn read_date_raw_cal(i: &mut Interp, v: &Value, cal: &str, ovf: Overflow) -> Result<(i64, i64, i64), Value> {
    // PrepareTemporalFields reads (and coerces) fields in alphabetical order, calling each field's
    // valueOf/toString as it is read: day, [era, eraYear], month, monthCode, year. Required-field
    // errors are only raised after every field has been read.
    let uses_era = cal_uses_era(cal);
    let day_f = getm(i, v, "day")?;
    let day = match day_f {
        Value::Undefined => None,
        _ => Some(to_int(i, &day_f)?),
    };
    let (era, era_year) = if uses_era {
        let e = getm(i, v, "era")?;
        let e = match e {
            Value::Undefined => None,
            _ => Some(i.to_string(&e).map_err(unab)?.to_lowercase()),
        };
        let ey = getm(i, v, "eraYear")?;
        let ey = match ey {
            Value::Undefined => None,
            _ => Some(to_int(i, &ey)?),
        };
        (e, ey)
    } else {
        (None, None)
    };
    let month_f = getm(i, v, "month")?;
    let month = match month_f {
        Value::Undefined => None,
        _ => {
            let n = to_int(i, &month_f)?;
            if n < 1 {
                return Err(i.make_error("RangeError", "month must be positive"));
            }
            Some(n)
        }
    };
    let mc_f = getm(i, v, "monthCode")?;
    let month_code = match mc_f {
        Value::Undefined => None,
        _ => Some(i.to_string(&mc_f).map_err(unab)?.to_string()),
    };
    let year_f = getm(i, v, "year")?;
    let year_opt = match year_f {
        Value::Undefined => None,
        _ => Some(to_int(i, &year_f)?),
    };

    // An `era` that isn't one this calendar defines is a RangeError (raised before the required-field
    // TypeErrors of the resolution below).
    if let Some(e) = &era {
        if !valid_era(cal, e) {
            return Err(i.make_error("RangeError", format!("{e} is not a valid era in calendar {cal}")));
        }
    }

    // Hebrew: the year field (or era "am") is the Hebrew year; the month is an ordinal, or a
    // monthCode (which may be the leap "M05L"). Resolve directly to ISO.
    if cal == "hebrew" {
        let hy = if let Some(y) = year_opt {
            y
        } else if era.as_deref() == Some("am") {
            era_year.ok_or_else(|| i.make_error("TypeError", "year is required"))?
        } else {
            return Err(i.make_error("TypeError", "year is required"));
        };
        let ord = if let Some(code) = &month_code {
            let o = match hebrew_ord_from_code(hy, code) {
                Some(o) => o,
                // Adar I ("M05L") is the only code that legitimately doesn't occur (a common year):
                // under constrain it collapses to Adar ("M06"), under reject it errors. Any other
                // absent code is simply invalid.
                None if code == "M05L" => {
                    if ovf == Overflow::Reject {
                        return Err(i.make_error("RangeError", "leap month does not exist in this year"));
                    }
                    hebrew_ord_from_code(hy, "M06").unwrap()
                }
                None => return Err(i.make_error("RangeError", "invalid monthCode for this calendar")),
            };
            if let Some(m) = month {
                if m != o {
                    return Err(i.make_error("RangeError", "month and monthCode disagree"));
                }
            }
            o
        } else {
            month.ok_or_else(|| i.make_error("TypeError", "month or monthCode is required"))?
        };
        let day = day.ok_or_else(|| i.make_error("TypeError", "day is required"))?;
        let ord = clamp_or(i, ord, hebrew_months_in_year(hy), ovf, "month")?;
        let day = clamp_or(i, day, hebrew_month_len(hy, ord), ovf, "day")?;
        let iso = hebrew_to_iso(hy, ord, day);
        return Ok((iso.year, iso.month as i64, iso.day as i64));
    }
    // Chinese/Dangi: year is a plain number (no era); the month is an ordinal (1..13) or a monthCode
    // "M01".."M12" with an optional leap "L" suffix. Resolve to the month's new-moon start day.
    if cal == "chinese" || cal == "dangi" {
        let cy = year_opt.ok_or_else(|| i.make_error("TypeError", "year is required"))?;
        let day = day.ok_or_else(|| i.make_error("TypeError", "day is required"))?;
        let start = if let Some(code) = &month_code {
            let body = code
                .strip_prefix('M')
                .ok_or_else(|| i.make_error("RangeError", "invalid monthCode"))?;
            let (digits, leap) = match body.strip_suffix('L') {
                Some(d) => (d, true),
                None => (body, false),
            };
            if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(i.make_error("RangeError", "invalid monthCode"));
            }
            let num: i64 = digits.parse().unwrap();
            if num < 1 || num > 12 {
                return Err(i.make_error("RangeError", "invalid monthCode for this calendar"));
            }
            let s = match china_month_start(cal, cy, num, leap) {
                Some(s) => s,
                // A requested leap month that does not occur this year constrains to the plain
                // month (or is rejected under `overflow: reject`).
                None if leap && ovf != Overflow::Reject => china_month_start(cal, cy, num, false)
                    .ok_or_else(|| i.make_error("RangeError", "invalid monthCode for this calendar"))?,
                None => return Err(i.make_error("RangeError", "invalid monthCode for this calendar")),
            };
            // A supplied ordinal `month` must agree with the monthCode.
            if let Some(ord) = month {
                if china_ord_start(cal, cy, ord.clamp(1, china_months_in_year(cal, cy))) != s {
                    return Err(i.make_error("RangeError", "month and monthCode disagree"));
                }
            }
            s
        } else {
            let ord = month.ok_or_else(|| i.make_error("TypeError", "month or monthCode is required"))?;
            let ord = clamp_or(i, ord, china_months_in_year(cal, cy), ovf, "month")?;
            china_ord_start(cal, cy, ord)
        };
        let dim = china_month_len_at(cal, start);
        let day = clamp_or(i, day, dim, ovf, "day")?;
        let (y, m, dd) = civil_from_days(start + day - 1);
        let iso = IsoDate { year: y, month: m, day: dd };
        return Ok((iso.year, iso.month as i64, iso.day as i64));
    }
    // Reconcile month / monthCode ("M" + 2 digits + optional leap "L"; no leap month in ISO/Gregory).
    let mc_num = match &month_code {
        None => None,
        Some(s) => {
            let body = s
                .strip_prefix('M')
                .ok_or_else(|| i.make_error("RangeError", "invalid monthCode"))?;
            let (digits, leap) = match body.strip_suffix('L') {
                Some(d) => (d, true),
                None => (body, false),
            };
            if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(i.make_error("RangeError", "invalid monthCode"));
            }
            let n: i64 = digits.parse().unwrap();
            // 13-month calendars allow M13; all others cap at M12. Leap months (the "L" suffix) only
            // exist in Hebrew/Chinese, which are resolved before this generic parse.
            let max_month = if is_13month(cal) { 13 } else { 12 };
            if n < 1 || n > max_month || leap {
                return Err(i.make_error("RangeError", "invalid monthCode for this calendar"));
            }
            Some(n)
        }
    };
    // A 13-month (Coptic/Ethiopic) calendar converts its own (year, month, day) directly to ISO.
    if is_13month(cal) {
        let cy = thirteen_month_year(cal, year_opt, &era, era_year)
            .ok_or_else(|| i.make_error("TypeError", "year is required"))?;
        let month = match (month, mc_num) {
            (Some(a), Some(b)) if a != b => {
                return Err(i.make_error("RangeError", "month and monthCode disagree"))
            }
            (Some(a), _) => a,
            (None, Some(b)) => b,
            (None, None) => return Err(i.make_error("TypeError", "month or monthCode is required")),
        };
        let day = day.ok_or_else(|| i.make_error("TypeError", "day is required"))?;
        // Constrain to the calendar's ranges (13 months; the epagomenal month has 5 or 6 days).
        let month = clamp_or(i, month, 13, ovf, "month")?;
        let leap = cy.rem_euclid(4) == 3;
        let dim = if month <= 12 {
            30
        } else if leap {
            6
        } else {
            5
        };
        let day = clamp_or(i, day, dim, ovf, "day")?;
        let iso = to_13month(cy, month, day, epoch_13(cal));
        return Ok((iso.year, iso.month as i64, iso.day as i64));
    }
    // Tabular Islamic: convert (year|era "ah", month, day) to ISO.
    if is_islamic(cal) {
        let iy = if let Some(y) = year_opt {
            y
        } else {
            match (era.as_deref(), era_year) {
                (Some("ah"), Some(ey)) => ey,
                (Some("bh"), Some(ey)) => 1 - ey,
                _ => return Err(i.make_error("TypeError", "year is required")),
            }
        };
        let month = match (month, mc_num) {
            (Some(a), Some(b)) if a != b => {
                return Err(i.make_error("RangeError", "month and monthCode disagree"))
            }
            (Some(a), _) => a,
            (None, Some(b)) => b,
            (None, None) => return Err(i.make_error("TypeError", "month or monthCode is required")),
        };
        let day = day.ok_or_else(|| i.make_error("TypeError", "day is required"))?;
        let month = clamp_or(i, month, 12, ovf, "month")?;
        let day = clamp_or(i, day, isl_month_len(cal, iy, month), ovf, "day")?;
        let iso = isl_to(cal, iy, month, day);
        return Ok((iso.year, iso.month as i64, iso.day as i64));
    }
    // Indian (Saka): convert (year | era "shaka", month, day) to ISO.
    if cal == "indian" {
        let sy = if let Some(y) = year_opt {
            y
        } else if era.as_deref() == Some("shaka") {
            era_year.ok_or_else(|| i.make_error("TypeError", "year is required"))?
        } else {
            return Err(i.make_error("TypeError", "year is required"));
        };
        let month = match (month, mc_num) {
            (Some(a), Some(b)) if a != b => {
                return Err(i.make_error("RangeError", "month and monthCode disagree"))
            }
            (Some(a), _) => a,
            (None, Some(b)) => b,
            (None, None) => return Err(i.make_error("TypeError", "month or monthCode is required")),
        };
        let day = day.ok_or_else(|| i.make_error("TypeError", "day is required"))?;
        let month = clamp_or(i, month, 12, ovf, "month")?;
        let day = clamp_or(i, day, indian_month_len(month, is_leap(sy + 78)), ovf, "day")?;
        let iso = to_indian(sy, month, day);
        return Ok((iso.year, iso.month as i64, iso.day as i64));
    }
    // Persian (Solar Hijri): (year | era ap/bp, month, day) → ISO.
    if cal == "persian" {
        let py = if let Some(y) = year_opt {
            y
        } else {
            match (era.as_deref(), era_year) {
                (Some("ap"), Some(ey)) => ey,
                _ => return Err(i.make_error("TypeError", "year is required")),
            }
        };
        let month = match (month, mc_num) {
            (Some(a), Some(b)) if a != b => {
                return Err(i.make_error("RangeError", "month and monthCode disagree"))
            }
            (Some(a), _) => a,
            (None, Some(b)) => b,
            (None, None) => return Err(i.make_error("TypeError", "month or monthCode is required")),
        };
        let day = day.ok_or_else(|| i.make_error("TypeError", "day is required"))?;
        let month = clamp_or(i, month, 12, ovf, "month")?;
        let day = clamp_or(i, day, persian_month_len(py, month), ovf, "day")?;
        let iso = to_persian(py, month, day);
        return Ok((iso.year, iso.month as i64, iso.day as i64));
    }
    // Reconcile year / era+eraYear BEFORE month (a missing year is a TypeError that the spec raises
    // before a month/monthCode conflict). An era, when present, must be valid for the calendar even
    // if a `year` field also supplies the value.
    let era_iso = if uses_era {
        match (&era, era_year) {
            (Some(e), Some(ey)) => Some(
                cal_era_to_iso(cal, e, ey)
                    .ok_or_else(|| i.make_error("RangeError", format!("invalid era: {e}")))?,
            ),
            (Some(_), None) | (None, Some(_)) => {
                return Err(i.make_error("TypeError", "era and eraYear must be provided together"))
            }
            (None, None) => None,
        }
    } else {
        None
    };
    let year = if let Some(y) = year_opt {
        cal_year_to_iso(cal, y)
    } else if let Some(ei) = era_iso {
        ei
    } else {
        return Err(i.make_error("TypeError", "year is required"));
    };
    // Every missing-required-field TypeError precedes a month/monthCode-conflict RangeError, so the
    // presence checks come before the conflict resolution.
    if month.is_none() && mc_num.is_none() {
        return Err(i.make_error("TypeError", "month or monthCode is required"));
    }
    let day = day.ok_or_else(|| i.make_error("TypeError", "day is required"))?;
    let month = match (month, mc_num) {
        (Some(a), Some(b)) => {
            if a != b {
                return Err(i.make_error("RangeError", "month and monthCode disagree"));
            }
            a
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => unreachable!(),
    };
    Ok((year, month, day))
}
fn setm(o: &Gc, k: &str, v: Value) {
    o.borrow_mut()
        .props
        .insert(k.to_string(), Property::data(v, true, true, true));
}

/// `with()` for a non-ISO calendar: merge the receiver's calendar fields with the partial input
/// (CalendarMergeFields — a provided year/era or month/monthCode replaces the receiver's whole
/// group), then resolve back to an ISO date.
fn with_cal_date(i: &mut Interp, cal: &str, d: IsoDate, f: &Value, ovf: Overflow) -> Result<IsoDate, Value> {
    let (_, cm, cd, ..) = cal_fields(cal, d);
    // The `year` field is the calendar's reported year number (e.g. ethioaa's amete-alem = amete-mihret
    // + 5500), which is not always `cal_fields().0`.
    let cy = cal_year_num(cal, d);
    let (cera, cery) = cal_era(cal, d);
    let present = |i: &mut Interp, k: &str| -> Result<bool, Value> {
        Ok(!matches!(getm(i, f, k)?, Value::Undefined))
    };
    let has_year = present(i, "year")? || present(i, "era")? || present(i, "eraYear")?;
    let has_month = present(i, "month")? || present(i, "monthCode")?;
    let has_day = present(i, "day")?;
    let merged = i.new_object();
    if !has_year {
        setm(&merged, "year", Value::Num(cy as f64));
        if let Some(e) = cera {
            setm(&merged, "era", Value::str(e));
        }
        if let Some(ey) = cery {
            setm(&merged, "eraYear", Value::Num(ey as f64));
        }
    }
    if !has_month {
        // For leap-month calendars the month's identity is its monthCode (an ordinal shifts between
        // leap and common years); carrying it over lets a year change reject/constrain correctly.
        if matches!(cal, "hebrew" | "chinese" | "dangi") {
            setm(&merged, "monthCode", Value::str(cal_month_code(cal, d).as_str()));
        } else {
            setm(&merged, "month", Value::Num(cm as f64));
        }
    }
    if !has_day {
        setm(&merged, "day", Value::Num(cd as f64));
    }
    // Overlay every field the caller actually supplied.
    for k in ["year", "era", "eraYear", "month", "monthCode", "day"] {
        let v = getm(i, f, k)?;
        if !matches!(v, Value::Undefined) {
            setm(&merged, k, v);
        }
    }
    let raw = read_date_raw_cal(i, &Value::Obj(merged), cal, ovf)?;
    regulate_date(i, raw, ovf)
}

/// Regulate raw year/month/day into a valid ISO date per `overflow`.
fn regulate_date(
    i: &Interp,
    (year, month, day): (i64, i64, i64),
    ovf: Overflow,
) -> Result<IsoDate, Value> {
    let month = regulate_high(i, month, 12, ovf, "month")? as u8;
    let day = regulate_high(i, day, days_in_month(year, month) as i64, ovf, "day")?;
    Ok(IsoDate {
        year,
        month,
        day: day as u8,
    })
}
/// Read the six raw time components from a bag; returns the values and whether any were present.
fn read_time_raw(i: &mut Interp, v: &Value) -> Result<([i64; 6], bool), Value> {
    let keys = [
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
    ];
    let mut vals = [0i64; 6];
    let mut any = false;
    for (k, slot) in keys.iter().zip(vals.iter_mut()) {
        let fv = getm(i, v, k)?;
        if !matches!(fv, Value::Undefined) {
            any = true;
            *slot = to_int(i, &fv)?;
        }
    }
    Ok((vals, any))
}
/// Regulate raw time components into a valid ISO time per `overflow`.
fn regulate_time(i: &Interp, v: [i64; 6], ovf: Overflow) -> Result<IsoTime, Value> {
    Ok(IsoTime {
        hour: regulate(i, v[0], 0, 23, ovf, "hour")? as u8,
        minute: regulate(i, v[1], 0, 59, ovf, "minute")? as u8,
        second: regulate(i, v[2], 0, 59, ovf, "second")? as u8,
        ms: regulate(i, v[3], 0, 999, ovf, "millisecond")? as u16,
        us: regulate(i, v[4], 0, 999, ovf, "microsecond")? as u16,
        ns: regulate(i, v[5], 0, 999, ovf, "nanosecond")? as u16,
    })
}

/// ToTemporalDate: accept a PlainDate/PlainDateTime, a fields object, or an ISO string. `opts`
/// supplies the `overflow` option (validated as an options object).
/// The receiver's calendar, requiring the `other` operand (of `until`/`since`) to share it (else a
/// RangeError, as calendar arithmetic between two calendars is undefined).
fn same_calendar(i: &mut Interp, t: &Value, other: &Value) -> Result<std::rc::Rc<str>, Value> {
    let cal = cal_of(i, t);
    let ocal = input_cal(i, other)?;
    if cal != ocal {
        return Err(i.make_error("RangeError", "operating between two calendars is not supported"));
    }
    Ok(cal)
}
/// Extract the calendar id from a Temporal-like input (Temporal object, ISO string, or property bag).
fn input_cal(i: &mut Interp, v: &Value) -> Result<std::rc::Rc<str>, Value> {
    if get(i, v).is_some() {
        return Ok(cal_of(i, v));
    }
    Ok(match v {
        Value::Str(s) => match parse_iso(s).and_then(|p| p.calendar) {
            Some(c) => std::rc::Rc::from(canon_cal(&c).unwrap_or("iso8601")),
            None => std::rc::Rc::from("iso8601"),
        },
        Value::Obj(_) => {
            let c = getm(i, v, "calendar")?;
            match &c {
                Value::Str(_) => check_calendar(i, &c)?,
                _ => std::rc::Rc::from("iso8601"),
            }
        }
        _ => std::rc::Rc::from("iso8601"),
    })
}

fn datetime_cal(i: &mut Interp, v: &Value) -> Result<std::rc::Rc<str>, Value> {
    input_cal(i, v)
}

/// Like [`to_date`], but also returns the resolved calendar id.
fn to_date_cal(i: &mut Interp, v: &Value, opts: &Value) -> Result<(IsoDate, std::rc::Rc<str>), Value> {
    let cal = input_cal(i, v)?;
    let d = to_date(i, v, opts)?;
    Ok((d, cal))
}

fn to_date(i: &mut Interp, v: &Value, opts: &Value) -> Result<IsoDate, Value> {
    match get(i, v) {
        Some(Temporal::Date(d)) | Some(Temporal::DateTime(d, _)) => {
            to_overflow(i, opts)?;
            return Ok(d);
        }
        _ => {}
    }
    let d = match v {
        Value::Str(s) => {
            let p =
                parse_iso(s).ok_or_else(|| i.make_error("RangeError", "invalid date string"))?;
            if p.offset == Off::Z {
                return Err(i.make_error("RangeError", "UTC designator not valid for PlainDate"));
            }
            let d = p
                .date
                .ok_or_else(|| i.make_error("RangeError", "no date in PlainDate string"))?;
            if !cal_ok(&p.calendar) {
                return Err(i.make_error("RangeError", "unsupported calendar"));
            }
            if !date_in_range(d) {
                return Err(i.make_error("RangeError", "date outside representable range"));
            }
            to_overflow(i, opts)?;
            d
        }
        Value::Obj(_) => {
            read_calendar(i, v)?;
            let cal = input_cal(i, v)?;
            let ovf = to_overflow(i, opts)?;
            let raw = read_date_raw_cal(i, v, &cal, ovf)?;
            regulate_date(i, raw, ovf)?
        }
        _ => return Err(i.make_error("TypeError", "cannot convert to Temporal.PlainDate")),
    };
    if !iso_date_within_limits(d) {
        return Err(i.make_error("RangeError", "date is outside the supported range"));
    }
    Ok(d)
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
    let month = if matches!(m, Value::Undefined) {
        None
    } else {
        let n = to_int(i, &m)?;
        if n < 1 {
            return Err(i.make_error("RangeError", "month must be positive"));
        }
        Some(n)
    };
    // Parse a monthCode "M" + 2 digits + optional "L" (leap). ISO/Gregorian have no leap months.
    let mc = getm(i, o, "monthCode")?;
    let mc_num = match &mc {
        Value::Undefined => None,
        Value::Str(s) => {
            let body = s
                .strip_prefix('M')
                .ok_or_else(|| i.make_error("RangeError", "invalid monthCode"))?;
            let (digits, leap) = match body.strip_suffix('L') {
                Some(d) => (d, true),
                None => (body, false),
            };
            if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(i.make_error("RangeError", "invalid monthCode"));
            }
            let n: i64 = digits.parse().unwrap();
            if n < 1 || leap {
                // No leap months, and M00 is invalid, in the ISO/Gregorian calendar.
                return Err(i.make_error("RangeError", "invalid monthCode for this calendar"));
            }
            Some(n)
        }
        _ => {
            let s = i.to_string(&mc).map_err(unab)?;
            return field_month_from_str(i, &s, month);
        }
    };
    match (month, mc_num) {
        (Some(a), Some(b)) => {
            if a != b {
                return Err(i.make_error("RangeError", "month and monthCode disagree"));
            }
            Ok(a)
        }
        (Some(a), None) => Ok(a),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(i.make_error("TypeError", "missing 'month' or 'monthCode'")),
    }
}

/// monthCode was a non-string coercible value: ToString then re-validate against an optional month.
fn field_month_from_str(i: &mut Interp, s: &str, month: Option<i64>) -> Result<i64, Value> {
    let body = s
        .strip_prefix('M')
        .ok_or_else(|| i.make_error("RangeError", "invalid monthCode"))?;
    let (digits, leap) = match body.strip_suffix('L') {
        Some(d) => (d, true),
        None => (body, false),
    };
    if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(i.make_error("RangeError", "invalid monthCode"));
    }
    let n: i64 = digits.parse().unwrap();
    if n < 1 || leap {
        return Err(i.make_error("RangeError", "invalid monthCode for this calendar"));
    }
    match month {
        Some(a) if a != n => Err(i.make_error("RangeError", "month and monthCode disagree")),
        _ => Ok(n),
    }
}

fn add_to_date(
    i: &mut Interp,
    d: IsoDate,
    dur: IsoDuration,
    sign: i64,
    ovf: Overflow,
    cal: &str,
) -> Result<IsoDate, Value> {
    // Month-structure calendars (Coptic/Ethiopic/Islamic/Indian) add in their own months.
    if is_month_structure(cal) {
        return check_date(i, cal_add(i, cal, d, dur, sign, ovf)?);
    }
    // Add years & months first (constraining the day), then weeks & days.
    let total_months = d.year * 12 + (d.month as i64 - 1) + sign * (dur.years * 12 + dur.months);
    let (y, m) = balance_year_month(total_months / 12, total_months % 12 + 1);
    let dim = days_in_month(y, m);
    // `reject` overflow throws when the source day doesn't exist in the target month.
    if ovf == Overflow::Reject && d.day as i64 > dim as i64 {
        return Err(i.make_error("RangeError", "date day out of range"));
    }
    let day = (d.day as i64).min(dim as i64);
    let z = days_from_civil(y, m as i64, day) + sign * (dur.weeks * 7 + dur.days);
    let (ny, nm, nd) = civil_from_days(z);
    check_date(
        i,
        IsoDate {
            year: ny,
            month: nm,
            day: nd,
        },
    )
}

/// Add a duration's date part (years/months/weeks/days) to a date, clamping the day.
fn add_date_dur(start: IsoDate, d: IsoDuration) -> IsoDate {
    let total_months = start.year * 12 + (start.month as i64 - 1) + d.years * 12 + d.months;
    let (y, m) = balance_year_month(total_months / 12, total_months % 12 + 1);
    let day = start.day.min(days_in_month(y, m));
    let z = days_from_civil(y, m as i64, day as i64) + d.weeks * 7 + d.days;
    let (ny, nm, nd) = civil_from_days(z);
    IsoDate {
        year: ny,
        month: nm,
        day: nd,
    }
}
/// Add a full duration (date + time) to a midnight-anchored start date.
fn add_full_duration(start: IsoDate, d: IsoDuration) -> (IsoDate, IsoTime) {
    let nd = add_date_dur(start, d);
    let tns = duration_time_ns(d);
    let carry = tns.div_euclid(86_400_000_000_000);
    let rem = tns.rem_euclid(86_400_000_000_000);
    let z = epoch_days(nd) as i128 + carry;
    let (y, m, da) = civil_from_days(z as i64);
    (
        IsoDate {
            year: y,
            month: m,
            day: da,
        },
        ns_to_time(rem),
    )
}
/// Read the `relativeTo` option as an anchor date, if present.
fn read_relative_to(i: &mut Interp, opts: &Value) -> Result<Option<IsoDate>, Value> {
    if matches!(opts, Value::Undefined | Value::Str(_)) {
        return Ok(None);
    }
    let v = getm(i, opts, "relativeTo")?;
    match get(i, &v) {
        Some(Temporal::Date(d)) | Some(Temporal::DateTime(d, _)) => return Ok(Some(d)),
        Some(Temporal::Zoned {
            epoch_ns,
            offset_ns,
            ..
        }) => return Ok(Some(zoned_local(epoch_ns, offset_ns).0)),
        _ => {}
    }
    match v {
        Value::Undefined => Ok(None),
        Value::Obj(_) => {
            // A `timeZone` field, if a string, must be a valid time-zone identifier (offset, named
            // zone, or an ISO string carrying a `[...]` annotation — a bare date-time string is not).
            let tzv = getm(i, &v, "timeZone")?;
            match &tzv {
                Value::Undefined => {}
                Value::Str(s) => validate_tz_string(i, s)?,
                // Only a Temporal object (which carries its own zone) is a valid non-string zone;
                // a plain object or other primitive is a TypeError.
                Value::Obj(_) if get(i, &tzv).is_some() => {}
                _ => return Err(i.make_error("TypeError", "timeZone must be a string or Temporal object")),
            }
            Ok(Some(to_date(i, &v, &Value::Undefined)?))
        }
        _ => Ok(Some(to_date(i, &v, &Value::Undefined)?)),
    }
}

/// Validate a time-zone identifier string per ToTemporalTimeZoneIdentifier: an offset, a named zone,
/// or an ISO date/time string whose `[...]` annotation is itself a valid zone. RangeError otherwise.
fn validate_tz_string(i: &Interp, s: &str) -> Result<(), Value> {
    let t = s.trim();
    if is_pure_offset(t) {
        return normalize_tz(i, t).map(|_| ());
    }
    if crate::tz::canonicalize(t).is_some() {
        return Ok(());
    }
    if let Some(p) = parse_iso(t) {
        if let Some(tz) = p.tz {
            return validate_tz_string(i, &tz);
        }
    }
    Err(i.make_error("RangeError", "invalid time zone identifier"))
}

/// Add a duration to a date+time, carrying the time overflow into the date.
fn dt_add(
    i: &mut Interp,
    d: IsoDate,
    t: IsoTime,
    dur: IsoDuration,
    sign: i64,
    ovf: Overflow,
    cal: &str,
) -> Result<(IsoDate, IsoTime), Value> {
    let nd = add_to_date(i, d, dur, sign, ovf, cal)?;
    let total = time_to_ns(t) as i128 + sign as i128 * duration_time_ns(dur);
    let carry = total.div_euclid(86_400_000_000_000);
    let tns = total.rem_euclid(86_400_000_000_000);
    let z = epoch_days(nd) as i128 + carry;
    let (ny, nm, nday) = civil_from_days(z as i64);
    let ndate = check_date(
        i,
        IsoDate {
            year: ny,
            month: nm,
            day: nday,
        },
    )?;
    Ok((ndate, ns_to_time(tns)))
}

// ===== PlainTime ==============================================================================

fn install_plain_time(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.PlainTime", proto.clone());

    def_getter(it, &proto, "hour", |i, t, _| {
        Ok(Value::Num(as_time(i, &t)?.hour as f64))
    });
    def_getter(it, &proto, "minute", |i, t, _| {
        Ok(Value::Num(as_time(i, &t)?.minute as f64))
    });
    def_getter(it, &proto, "second", |i, t, _| {
        Ok(Value::Num(as_time(i, &t)?.second as f64))
    });
    def_getter(it, &proto, "millisecond", |i, t, _| {
        Ok(Value::Num(as_time(i, &t)?.ms as f64))
    });
    def_getter(it, &proto, "microsecond", |i, t, _| {
        Ok(Value::Num(as_time(i, &t)?.us as f64))
    });
    def_getter(it, &proto, "nanosecond", |i, t, _| {
        Ok(Value::Num(as_time(i, &t)?.ns as f64))
    });

    it.def_method(&proto, "toString", 0, |i, t, a| {
        let x = as_time(i, &t)?;
        Ok(Value::str(fmt_time_opts(i, x, &arg(a, 0))?))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        Ok(Value::str(fmt_time(as_time(i, &t)?)))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error(
            "TypeError",
            "Temporal.PlainTime has no valueOf; use compare",
        ))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let y = to_time(i, &arg(a, 0), &Value::Undefined)?;
        Ok(Value::Bool(time_to_ns(x) == time_to_ns(y)))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let f = arg(a, 0);
        let hour = field_int(i, &f, "hour", x.hour as i64)?;
        let minute = field_int(i, &f, "minute", x.minute as i64)?;
        let second = field_int(i, &f, "second", x.second as i64)?;
        let ms = field_int(i, &f, "millisecond", x.ms as i64)?;
        let us = field_int(i, &f, "microsecond", x.us as i64)?;
        let ns = field_int(i, &f, "nanosecond", x.ns as i64)?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let nt = build_time_ovf(i, hour, minute, second, ms, us, ns, ovf)?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(nt)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let total = (time_to_ns(x) as i128 + duration_time_ns(dur)).rem_euclid(86_400_000_000_000);
        Ok(make(
            i,
            "Temporal.PlainTime",
            Temporal::Time(ns_to_time(total)),
        ))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let total = (time_to_ns(x) as i128 - duration_time_ns(dur)).rem_euclid(86_400_000_000_000);
        Ok(make(
            i,
            "Temporal.PlainTime",
            Temporal::Time(ns_to_time(total)),
        ))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let y = to_time(i, &arg(a, 0), &Value::Undefined)?;
        let diff = (time_to_ns(y) - time_to_ns(x)) as i128;
        let dur = time_diff(i, diff, &arg(a, 1), false, 5)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let y = to_time(i, &arg(a, 0), &Value::Undefined)?;
        let diff = (time_to_ns(y) - time_to_ns(x)) as i128;
        let dur = time_diff(i, diff, &arg(a, 1), true, 5)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let x = as_time(i, &t)?;
        let (o, shorthand) = round_opts(&arg(a, 0));
        let smallest = match shorthand {
            Some(s) => s,
            None => opt_str(i, &o, "smallestUnit", "")?,
        };
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
        let hour = to_int_default(i, &arg(a, 0), 0)?;
        let minute = to_int_default(i, &arg(a, 1), 0)?;
        let second = to_int_default(i, &arg(a, 2), 0)?;
        let ms = to_int_default(i, &arg(a, 3), 0)?;
        let us = to_int_default(i, &arg(a, 4), 0)?;
        let ns = to_int_default(i, &arg(a, 5), 0)?;
        let t = build_time(i, hour, minute, second, ms, us, ns)?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(t)))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let t = to_time(i, &arg(a, 0), &arg(a, 1))?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(t)))
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_time(i, &arg(a, 0), &Value::Undefined)?;
        let y = to_time(i, &arg(a, 1), &Value::Undefined)?;
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
    let mut out = IsoDuration {
        ms: millis,
        us: micros,
        ns: nanos,
        ..Default::default()
    };
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
fn to_time(i: &mut Interp, v: &Value, opts: &Value) -> Result<IsoTime, Value> {
    match get(i, v) {
        Some(Temporal::Time(t)) | Some(Temporal::DateTime(_, t)) => {
            to_overflow(i, opts)?;
            return Ok(t);
        }
        _ => {}
    }
    match v {
        Value::Str(s) => {
            let p = parse_iso(s)
                .ok_or_else(|| i.make_error("RangeError", "invalid PlainTime string"))?;
            // A PlainTime string may not carry a UTC designator.
            if p.offset == Off::Z {
                return Err(i.make_error("RangeError", "UTC designator not valid for PlainTime"));
            }
            let t = p
                .time
                .ok_or_else(|| i.make_error("RangeError", "no time in PlainTime string"))?;
            // A bare time that could also be read as a year-month or month-day needs a `T` prefix.
            if !p.designator && p.date.is_none() {
                let core = iso_core(s);
                if matches_year_month(core) || matches_month_day(core) {
                    return Err(i.make_error(
                        "RangeError",
                        "ambiguous time string requires a T designator",
                    ));
                }
            }
            Ok(t)
        }
        Value::Obj(_) => {
            // At least one time field must be present.
            let (vals, any) = read_time_raw(i, v)?;
            if !any {
                return Err(i.make_error("TypeError", "object has no time fields"));
            }
            let ovf = to_overflow(i, opts)?;
            regulate_time(i, vals, ovf)
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainTime")),
    }
}

// ===== PlainDateTime ==========================================================================

fn install_plain_datetime(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos
        .insert("Temporal.PlainDateTime", proto.clone());

    def_getter(it, &proto, "year", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(cal_year_num(&cal_of(i, &t), d) as f64))
    });
    def_getter(it, &proto, "era", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(cal_era(&cal_of(i, &t), d).0.map(Value::str).unwrap_or(Value::Undefined))
    });
    def_getter(it, &proto, "eraYear", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(cal_era(&cal_of(i, &t), d).1.map(|e| Value::Num(e as f64)).unwrap_or(Value::Undefined))
    });
    def_getter(it, &proto, "month", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).1 as f64))
    });
    def_getter(it, &proto, "day", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).2 as f64))
    });
    def_getter(it, &proto, "monthCode", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::from_string(cal_month_code(&cal_of(i, &t), d)))
    });
    def_getter(it, &proto, "calendarId", |i, t, _| Ok(Value::from_string(cal_of(i, &t).to_string())));
    def_getter(it, &proto, "hour", |i, t, _| {
        Ok(Value::Num(as_datetime(i, &t)?.1.hour as f64))
    });
    def_getter(it, &proto, "minute", |i, t, _| {
        Ok(Value::Num(as_datetime(i, &t)?.1.minute as f64))
    });
    def_getter(it, &proto, "second", |i, t, _| {
        Ok(Value::Num(as_datetime(i, &t)?.1.second as f64))
    });
    def_getter(it, &proto, "millisecond", |i, t, _| {
        Ok(Value::Num(as_datetime(i, &t)?.1.ms as f64))
    });
    def_getter(it, &proto, "microsecond", |i, t, _| {
        Ok(Value::Num(as_datetime(i, &t)?.1.us as f64))
    });
    def_getter(it, &proto, "nanosecond", |i, t, _| {
        Ok(Value::Num(as_datetime(i, &t)?.1.ns as f64))
    });
    def_getter(it, &proto, "dayOfWeek", |i, t, _| {
        Ok(Value::Num(iso_day_of_week(as_datetime(i, &t)?.0) as f64))
    });
    def_getter(it, &proto, "dayOfYear", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).5 as f64))
    });
    def_getter(it, &proto, "daysInMonth", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).3 as f64))
    });
    def_getter(it, &proto, "daysInYear", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).6 as f64))
    });
    def_getter(it, &proto, "monthsInYear", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).4 as f64))
    });
    def_getter(it, &proto, "inLeapYear", |i, t, _| {
        let d = as_datetime(i, &t)?.0;
        Ok(Value::Bool(cal_fields(&cal_of(i, &t), d).7))
    });

    it.def_method(&proto, "toString", 0, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let ts = fmt_time_opts(i, tm, &arg(a, 0))?;
        Ok(Value::str(format!(
            "{}T{}{}",
            fmt_date(d),
            ts,
            cal_suffix(i, &arg(a, 0), &cal_of(i, &t))?
        )))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let (d, tm) = as_datetime(i, &t)?;
        Ok(Value::str(format!("{}T{}", fmt_date(d), fmt_time(tm))))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error(
            "TypeError",
            "Temporal.PlainDateTime has no valueOf; use compare",
        ))
    });
    it.def_method(&proto, "toPlainDate", 0, |i, t, _| {
        let (d, _) = as_datetime(i, &t)?;
        Ok(make_like(i, &t, "Temporal.PlainDate", Temporal::Date(d)))
    });
    it.def_method(&proto, "toPlainTime", 0, |i, t, _| {
        let (_, tm) = as_datetime(i, &t)?;
        Ok(make(i, "Temporal.PlainTime", Temporal::Time(tm)))
    });
    it.def_method(&proto, "withPlainTime", 1, |i, t, a| {
        let (d, _) = as_datetime(i, &t)?;
        let nt = match arg(a, 0) {
            Value::Undefined => IsoTime {
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                us: 0,
                ns: 0,
            },
            v => to_time(i, &v, &Value::Undefined)?,
        };
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(d, nt)))
    });
    it.def_method(&proto, "withPlainDate", 1, |i, t, a| {
        let (_, tm) = as_datetime(i, &t)?;
        let nd = to_date(i, &arg(a, 0), &Value::Undefined)?;
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(nd, tm)))
    });
    it.def_method(&proto, "withCalendar", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let cal = check_calendar(i, &arg(a, 0))?;
        let v = make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, tm));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&proto, "toZonedDateTime", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let tzv = arg(a, 0);
        let tz_raw: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let tz = normalize_tz(i, &tz_raw)?;
        let disamb = opt_str(i, &arg(a, 1), "disambiguation", "compatible")?;
        let local = dt_ns(d, tm);
        let epoch = local_to_epoch(i, &tz, local, &disamb)?;
        let offset = (local - epoch) as i64;
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: offset, tz, }))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (od, otm) = to_datetime(i, &arg(a, 0), &Value::Undefined)?;
        Ok(Value::Bool(
            cmp_date(d, od) == 0 && time_to_ns(tm) == time_to_ns(otm),
        ))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal_id = cal_of(i, &t);
        let (nd, ntm) = dt_add(i, d, tm, dur, 1, ovf, &cal_id)?;
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(nd, ntm)))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal_id = cal_of(i, &t);
        let (nd, ntm) = dt_add(i, d, tm, dur, -1, ovf, &cal_id)?;
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(nd, ntm)))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "with() argument must be an object"));
        }
        let hour = field_int(i, &f, "hour", tm.hour as i64)?;
        let minute = field_int(i, &f, "minute", tm.minute as i64)?;
        let second = field_int(i, &f, "second", tm.second as i64)?;
        let ms = field_int(i, &f, "millisecond", tm.ms as i64)?;
        let us = field_int(i, &f, "microsecond", tm.us as i64)?;
        let nsf = field_int(i, &f, "nanosecond", tm.ns as i64)?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal = cal_of(i, &t);
        let nd = if &*cal == "iso8601" {
            let year = field_int(i, &f, "year", d.year)?;
            let month = field_int(i, &f, "month", d.month as i64)?;
            let day = field_int(i, &f, "day", d.day as i64)?;
            build_date_ovf(i, year, month, day, ovf)?
        } else {
            with_cal_date(i, &cal, d, &f, ovf)?
        };
        let nt = build_time_ovf(i, hour, minute, second, ms, us, nsf, ovf)?;
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(nd, nt)))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (o, shorthand) = round_opts(&arg(a, 0));
        let smallest = match shorthand {
            Some(s) => s,
            None => opt_str(i, &o, "smallestUnit", "")?,
        };
        let unit = if smallest == "day" {
            86_400_000_000_000
        } else {
            unit_ns(&smallest)
                .ok_or_else(|| i.make_error("RangeError", "smallestUnit is required"))?
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
        let nd = check_date(
            i,
            IsoDate {
                year: y,
                month: mo,
                day: da,
            },
        )?;
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(nd, ns_to_time(rem))))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (od, otm) = to_datetime(i, &arg(a, 0), &Value::Undefined)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        let (largest, smallest, incr, mode) = read_datetime_diff(i, &arg(a, 1))?;
        let dur = diff_datetime_rounded(&cal, d, tm, od, otm, &largest, &smallest, incr, &mode);
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let (d, tm) = as_datetime(i, &t)?;
        let (od, otm) = to_datetime(i, &arg(a, 0), &Value::Undefined)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        let (largest, smallest, incr, mode) = read_datetime_diff(i, &arg(a, 1))?;
        let dur = diff_datetime_rounded(&cal, d, tm, od, otm, &largest, &smallest, incr, negate_mode(&mode));
        Ok(make(i, "Temporal.Duration", Temporal::Duration(neg_duration(dur))))
    });

    let ctor = add_ctor(it, ns, "PlainDateTime", 3, proto, |i, _t, a| {
        require_new(i)?;
        let year = to_int(i, &arg(a, 0))?;
        let month = to_int(i, &arg(a, 1))?;
        let day = to_int(i, &arg(a, 2))?;
        let hour = to_int_default(i, &arg(a, 3), 0)?;
        let minute = to_int_default(i, &arg(a, 4), 0)?;
        let second = to_int_default(i, &arg(a, 5), 0)?;
        let ms = to_int_default(i, &arg(a, 6), 0)?;
        let us = to_int_default(i, &arg(a, 7), 0)?;
        let ns = to_int_default(i, &arg(a, 8), 0)?;
        let cal = check_calendar(i, &arg(a, 9))?;
        let d = build_date(i, year, month, day)?;
        let tm = build_time(i, hour, minute, second, ms, us, ns)?;
        let v = make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, tm));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let (d, tm) = to_datetime(i, &arg(a, 0), &arg(a, 1))?;
        let cal = datetime_cal(i, &arg(a, 0))?;
        let v = make(i, "Temporal.PlainDateTime", Temporal::DateTime(d, tm));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let (xd, xt) = to_datetime(i, &arg(a, 0), &Value::Undefined)?;
        let (yd, yt) = to_datetime(i, &arg(a, 1), &Value::Undefined)?;
        let c = cmp_date(xd, yd);
        Ok(Value::Num(if c != 0 {
            c
        } else {
            time_to_ns(xt).cmp(&time_to_ns(yt)) as i64
        } as f64))
    });
}

fn to_datetime(i: &mut Interp, v: &Value, opts: &Value) -> Result<(IsoDate, IsoTime), Value> {
    let midnight = IsoTime {
        hour: 0,
        minute: 0,
        second: 0,
        ms: 0,
        us: 0,
        ns: 0,
    };
    match get(i, v) {
        Some(Temporal::DateTime(d, t)) => {
            to_overflow(i, opts)?;
            return Ok((d, t));
        }
        Some(Temporal::Date(d)) => {
            to_overflow(i, opts)?;
            return Ok((d, midnight));
        }
        _ => {}
    }
    match v {
        Value::Str(s) => {
            let p = parse_iso(s).ok_or_else(|| i.make_error("RangeError", "invalid datetime"))?;
            if p.offset == Off::Z {
                return Err(
                    i.make_error("RangeError", "UTC designator not valid for PlainDateTime")
                );
            }
            let d = p
                .date
                .ok_or_else(|| i.make_error("RangeError", "no date in PlainDateTime string"))?;
            if !cal_ok(&p.calendar) {
                return Err(i.make_error("RangeError", "unsupported calendar"));
            }
            let t = p.time.unwrap_or(IsoTime {
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                us: 0,
                ns: 0,
            });
            if !iso_datetime_within_limits(d, t) {
                return Err(i.make_error("RangeError", "date-time outside representable range"));
            }
            Ok((d, t))
        }
        Value::Obj(_) => {
            read_calendar(i, v)?;
            let cal = input_cal(i, v)?;
            // Snapshot the date fields (side-effecting reads, in order), then read time fields, then
            // the overflow option — matching the spec order — and only then resolve the calendar with
            // the real overflow (so reject rejects an out-of-range calendar day).
            let snap = snapshot_date_fields(i, v)?;
            let (traw, _) = read_time_raw(i, v)?;
            let ovf = to_overflow(i, opts)?;
            let draw = read_date_raw_cal(i, &snap, &cal, ovf)?;
            let d = regulate_date(i, draw, ovf)?;
            let t = regulate_time(i, traw, ovf)?;
            if !iso_datetime_within_limits(d, t) {
                return Err(i.make_error("RangeError", "date-time outside representable range"));
            }
            Ok((d, t))
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainDateTime")),
    }
}

// ===== PlainYearMonth / PlainMonthDay =========================================================

fn install_year_month(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos
        .insert("Temporal.PlainYearMonth", proto.clone());
    def_getter(it, &proto, "year", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::Num(cal_year_num(&cal_of(i, &t), d) as f64))
    });
    def_getter(it, &proto, "era", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(cal_era(&cal_of(i, &t), d).0.map(Value::str).unwrap_or(Value::Undefined))
    });
    def_getter(it, &proto, "eraYear", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(cal_era(&cal_of(i, &t), d).1.map(|e| Value::Num(e as f64)).unwrap_or(Value::Undefined))
    });
    def_getter(it, &proto, "month", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).1 as f64))
    });
    def_getter(it, &proto, "monthCode", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::from_string(cal_month_code(&cal_of(i, &t), d)))
    });
    def_getter(it, &proto, "calendarId", |i, t, _| Ok(Value::from_string(cal_of(i, &t).to_string())));
    it.def_method(&proto, "toPlainDate", 1, |i, t, a| {
        let ym = as_yearmonth(i, &t)?;
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "toPlainDate() argument must be an object"));
        }
        // The day is required to complete a date from a year-month.
        let dv = getm(i, &f, "day")?;
        if matches!(dv, Value::Undefined) {
            return Err(i.make_error("TypeError", "toPlainDate() requires a day"));
        }
        let day = to_int(i, &dv)?;
        let cal = cal_of(i, &t);
        let d = if &*cal == "iso8601" {
            build_date(i, ym.year, ym.month as i64, day)?
        } else {
            // Combine the calendar's (year, monthCode) with the day, resolving through the calendar.
            let merged = i.new_object();
            setm(&merged, "year", Value::Num(cal_year_num(&cal, ym) as f64));
            setm(&merged, "monthCode", Value::str(cal_month_code(&cal, ym).as_str()));
            setm(&merged, "day", Value::Num(day as f64));
            let raw = read_date_raw_cal(i, &Value::Obj(merged), &cal, Overflow::Constrain)?;
            regulate_date(i, raw, Overflow::Constrain)?
        };
        Ok(make_like(i, &t, "Temporal.PlainDate", Temporal::Date(d)))
    });
    def_getter(it, &proto, "daysInMonth", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).3 as f64))
    });
    def_getter(it, &proto, "daysInYear", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).6 as f64))
    });
    def_getter(it, &proto, "monthsInYear", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).4 as f64))
    });
    def_getter(it, &proto, "inLeapYear", |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::Bool(cal_fields(&cal_of(i, &t), d).7))
    });
    it.def_method(&proto, "toString", 0, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let suffix = cal_suffix(i, &arg(a, 0), &cal_of(i, &t))?;
        // When the calendar is shown, a PlainYearMonth includes its reference ISO day (`-DD`).
        let s = if suffix.is_empty() {
            format!("{}-{:02}", pad_year(d.year), d.month)
        } else {
            format!("{}-{:02}-{:02}{}", pad_year(d.year), d.month, d.day, suffix)
        };
        Ok(Value::str(s))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let d = as_yearmonth(i, &t)?;
        Ok(Value::str(format!("{}-{:02}", pad_year(d.year), d.month)))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let o = to_yearmonth(i, &arg(a, 0), &Value::Undefined)?;
        Ok(Value::Bool(d.year == o.year && d.month == o.month))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "with() argument must be an object"));
        }
        let cal = cal_of(i, &t);
        if &*cal != "iso8601" {
            // Merge the calendar (year, monthCode) with the partial input (day 1), then reduce back to
            // the resulting year-month.
            let present = |i: &mut Interp, k: &str| -> Result<bool, Value> {
                Ok(!matches!(getm(i, &f, k)?, Value::Undefined))
            };
            let has_year = present(i, "year")? || present(i, "era")? || present(i, "eraYear")?;
            let has_month = present(i, "month")? || present(i, "monthCode")?;
            let merged = i.new_object();
            if !has_year {
                setm(&merged, "year", Value::Num(cal_year_num(&cal, d) as f64));
                let (era, ery) = cal_era(&cal, d);
                if let Some(e) = era {
                    setm(&merged, "era", Value::str(e));
                }
                if let Some(ey) = ery {
                    setm(&merged, "eraYear", Value::Num(ey as f64));
                }
            }
            if !has_month {
                setm(&merged, "monthCode", Value::str(cal_month_code(&cal, d).as_str()));
            }
            for k in ["year", "era", "eraYear", "month", "monthCode"] {
                let v = getm(i, &f, k)?;
                if !matches!(v, Value::Undefined) {
                    setm(&merged, k, v);
                }
            }
            setm(&merged, "day", Value::Num(1.0));
            let ovf = to_overflow(i, &arg(a, 1))?;
            let raw = read_date_raw_cal(i, &Value::Obj(merged), &cal, ovf)?;
            let iso = regulate_date(i, raw, ovf)?;
            return Ok(make_like(i, &t, "Temporal.PlainYearMonth", Temporal::YearMonth(ym_ref_of(&cal, iso))));
        }
        let year = field_int(i, &f, "year", d.year)?;
        let month_raw = field_int(i, &f, "month", d.month as i64)?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let month = match ovf {
            Overflow::Constrain => month_raw.clamp(1, 12),
            Overflow::Reject => month_raw,
        };
        if !(1..=12).contains(&month) || !iso_year_month_within_limits(year, month) {
            return Err(i.make_error("RangeError", "invalid year-month"));
        }
        Ok(make_like(i, &t, "Temporal.PlainYearMonth", Temporal::YearMonth(IsoDate { year, month: month as u8, day: 1, })))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal = cal_of(i, &t);
        let r = ym_add(i, &cal, d, dur, 1, ovf)?;
        Ok(make_like(i, &t, "Temporal.PlainYearMonth", Temporal::YearMonth(r)))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal = cal_of(i, &t);
        let r = ym_add(i, &cal, d, dur, -1, ovf)?;
        Ok(make_like(i, &t, "Temporal.PlainYearMonth", Temporal::YearMonth(r)))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let o = to_yearmonth(i, &arg(a, 0), &Value::Undefined)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        let (largest, smallest, mode) = read_ym_diff(i, &arg(a, 1))?;
        let (d1, o1) = (d, o); // stored references are the calendar month's first day
        let mut dur = diff_date_rounded(&cal, d1, o1, &largest, &smallest, 1, &mode);
        dur.weeks = 0;
        dur.days = 0; // a year-month difference has no day component
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let d = as_yearmonth(i, &t)?;
        let o = to_yearmonth(i, &arg(a, 0), &Value::Undefined)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        let (largest, smallest, mode) = read_ym_diff(i, &arg(a, 1))?;
        let (d1, o1) = (d, o); // stored references are the calendar month's first day
        let mut dur = diff_date_rounded(&cal, d1, o1, &largest, &smallest, 1, negate_mode(&mode));
        dur.weeks = 0;
        dur.days = 0; // a year-month difference has no day component
        Ok(make(i, "Temporal.Duration", Temporal::Duration(neg_duration(dur))))
    });
    let ctor = add_ctor(it, ns, "PlainYearMonth", 2, proto, |i, _t, a| {
        require_new(i)?;
        let year = to_int(i, &arg(a, 0))?;
        let month = to_int(i, &arg(a, 1))?;
        let cal = check_calendar(i, &arg(a, 2))?;
        let day = to_int_default(i, &arg(a, 3), 1)?;
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month as u8) as i64 {
            return Err(i.make_error("RangeError", "invalid year-month"));
        }
        if !iso_year_month_within_limits(year, month) {
            return Err(i.make_error("RangeError", "year-month is outside the supported range"));
        }
        let v = make(
            i,
            "Temporal.PlainYearMonth",
            Temporal::YearMonth(IsoDate {
                year,
                month: month as u8,
                day: day as u8,
            }),
        );
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let cal = input_cal(i, &arg(a, 0))?;
        let d = to_yearmonth(i, &arg(a, 0), &arg(a, 1))?;
        let v = make(i, "Temporal.PlainYearMonth", Temporal::YearMonth(d));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_yearmonth(i, &arg(a, 0), &Value::Undefined)?;
        let y = to_yearmonth(i, &arg(a, 1), &Value::Undefined)?;
        let xk = x.year * 12 + x.month as i64;
        let yk = y.year * 12 + y.month as i64;
        Ok(Value::Num(xk.cmp(&yk) as i64 as f64))
    });
}
fn to_yearmonth(i: &mut Interp, v: &Value, opts: &Value) -> Result<IsoDate, Value> {
    if let Some(Temporal::YearMonth(d)) = get(i, v) {
        to_overflow(i, opts)?;
        return Ok(d);
    }
    let d = match v {
        Value::Str(s) => {
            parse_year_month(s).ok_or_else(|| i.make_error("RangeError", "invalid year-month"))?
        }
        Value::Obj(_) => {
            read_calendar(i, v)?;
            let cal = input_cal(i, v)?;
            if &*cal == "iso8601" {
                let year = field_req(i, v, "year")?;
                let month = field_month(i, v)?;
                let ovf = to_overflow(i, opts)?;
                let month = regulate_high(i, month, 12, ovf, "month")? as u8;
                IsoDate { year, month, day: 1 }
            } else {
                // Resolve via the calendar (year/era/month/monthCode) with a reference day of 1.
                let merged = i.new_object();
                setm(&merged, "day", Value::Num(1.0));
                for k in ["year", "era", "eraYear", "month", "monthCode"] {
                    let fv = getm(i, v, k)?;
                    if !matches!(fv, Value::Undefined) {
                        setm(&merged, k, fv);
                    }
                }
                let ovf = to_overflow(i, opts)?;
                let raw = read_date_raw_cal(i, &Value::Obj(merged), &cal, ovf)?;
                regulate_date(i, raw, ovf)?
            }
        }
        _ => return Err(i.make_error("TypeError", "cannot convert to Temporal.PlainYearMonth")),
    };
    if !iso_year_month_within_limits(d.year, d.month as i64) {
        return Err(i.make_error("RangeError", "year-month is outside the supported range"));
    }
    Ok(d)
}

fn install_month_day(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos
        .insert("Temporal.PlainMonthDay", proto.clone());
    def_getter(it, &proto, "monthCode", |i, t, _| {
        let d = as_monthday(i, &t)?;
        Ok(Value::from_string(cal_month_code(&cal_of(i, &t), d)))
    });
    def_getter(it, &proto, "day", |i, t, _| {
        let d = as_monthday(i, &t)?;
        Ok(Value::Num(cal_fields(&cal_of(i, &t), d).2 as f64))
    });
    def_getter(it, &proto, "calendarId", |i, t, _| Ok(Value::from_string(cal_of(i, &t).to_string())));
    it.def_method(&proto, "toString", 0, |i, t, a| {
        let d = as_monthday(i, &t)?;
        let suffix = cal_suffix(i, &arg(a, 0), &cal_of(i, &t))?;
        // When the calendar is shown, a PlainMonthDay includes its reference ISO year (`YYYY-`).
        let s = if suffix.is_empty() {
            format!("{:02}-{:02}", d.month, d.day)
        } else {
            format!("{}-{:02}-{:02}{}", pad_year(d.year), d.month, d.day, suffix)
        };
        Ok(Value::str(s))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let d = as_monthday(i, &t)?;
        Ok(Value::str(format!("{:02}-{:02}", d.month, d.day)))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        let d = as_monthday(i, &t)?;
        let o = to_monthday(i, &arg(a, 0), &Value::Undefined)?;
        Ok(Value::Bool(d.month == o.month && d.day == o.day))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let md = as_monthday(i, &t)?;
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "with() argument must be an object"));
        }
        let month = field_int(i, &f, "month", md.month as i64)?;
        let day = field_int(i, &f, "day", md.day as i64)?;
        let ovf = to_overflow(i, &arg(a, 1))?;
        // Keep the reference ISO year; regulate the merged month/day.
        let d = build_date_ovf(i, md.year, month, day, ovf)?;
        let v = make_like(i, &t, "Temporal.PlainMonthDay", Temporal::MonthDay(d));
        set_cal(i, &v, cal_of(i, &t));
        Ok(v)
    });
    it.def_method(&proto, "toPlainDate", 1, |i, t, a| {
        let md = as_monthday(i, &t)?;
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "toPlainDate() argument must be an object"));
        }
        // The year is required to complete a date from a month-day.
        let yv = getm(i, &f, "year")?;
        if matches!(yv, Value::Undefined) {
            return Err(i.make_error("TypeError", "toPlainDate() requires a year"));
        }
        let year = to_int(i, &yv)?;
        let cal = cal_of(i, &t);
        let d = if &*cal == "iso8601" {
            build_date(i, year, md.month as i64, md.day as i64)?
        } else {
            // Combine the month-day's calendar (monthCode, day) with the supplied year.
            let merged = i.new_object();
            setm(&merged, "year", Value::Num(year as f64));
            setm(&merged, "monthCode", Value::str(cal_month_code(&cal, md).as_str()));
            setm(&merged, "day", Value::Num(md.day as f64));
            let raw = read_date_raw_cal(i, &Value::Obj(merged), &cal, Overflow::Constrain)?;
            regulate_date(i, raw, Overflow::Constrain)?
        };
        Ok(make_like(i, &t, "Temporal.PlainDate", Temporal::Date(d)))
    });
    let ctor = add_ctor(it, ns, "PlainMonthDay", 2, proto, |i, _t, a| {
        require_new(i)?;
        let month = to_int(i, &arg(a, 0))?;
        let day = to_int(i, &arg(a, 1))?;
        let cal = check_calendar(i, &arg(a, 2))?;
        let year = to_int_default(i, &arg(a, 3), 1972)?;
        let d = build_date(i, year, month, day)?;
        let v = make(i, "Temporal.PlainMonthDay", Temporal::MonthDay(d));
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let cal = input_cal(i, &arg(a, 0))?;
        let d = to_monthday(i, &arg(a, 0), &arg(a, 1))?;
        let v = make(i, "Temporal.PlainMonthDay", Temporal::MonthDay(d));
        set_cal(i, &v, cal);
        Ok(v)
    });
}
/// CalendarMonthDayToISOReferenceDate for a non-ISO calendar: the reference ISO date is the latest
/// date at or before 1972-12-31 (in the calendar) whose monthCode and day match the request, so a
/// leap monthCode anchors to a year that actually has it. `snap` holds the already-read fields.
fn cal_month_day_reference(i: &mut Interp, cal: &str, snap: &Gc, has_year: bool, ovf: Overflow) -> Result<IsoDate, Value> {
    // A bare ordinal `month` (no monthCode) can't be interpreted without a year in a non-ISO calendar,
    // so a year is required (a TypeError raised before any month/monthCode-conflict RangeError).
    let has_month = !matches!(getm(i, &Value::Obj(snap.clone()), "month")?, Value::Undefined);
    if has_month && !has_year {
        return Err(i.make_error("TypeError", "year is required to interpret an ordinal month"));
    }
    let ref_iso = IsoDate { year: 1972, month: 12, day: 31 };
    let start_cy = cal_year_num(cal, ref_iso);
    // Resolve the request in a candidate calendar year (day/month/monthCode from `snap`, year = `cy`).
    let resolve = |i: &mut Interp, cy: i64| -> Result<IsoDate, Value> {
        let merged = i.new_object();
        for k in ["day", "month", "monthCode"] {
            let fv = getm(i, &Value::Obj(snap.clone()), k)?;
            if !matches!(fv, Value::Undefined) {
                setm(&merged, k, fv);
            }
        }
        setm(&merged, "year", Value::Num(cy as f64));
        let raw = read_date_raw_cal(i, &Value::Obj(merged), cal, ovf)?;
        regulate_date(i, raw, ovf)
    };
    // If a year was supplied, resolve there (validating a leap month under reject); otherwise anchor
    // at the reference year. Either way the request's monthCode is what we then search for.
    let anchor = if has_year {
        let merged = i.new_object();
        for k in ["day", "era", "eraYear", "month", "monthCode", "year"] {
            let fv = getm(i, &Value::Obj(snap.clone()), k)?;
            if !matches!(fv, Value::Undefined) {
                setm(&merged, k, fv);
            }
        }
        let raw = read_date_raw_cal(i, &Value::Obj(merged), cal, ovf)?;
        regulate_date(i, raw, ovf)?
    } else {
        resolve(i, start_cy)?
    };
    // The wanted monthCode is the caller's verbatim (preserving a leap "L"); with an ordinal `month`
    // it comes from the anchor. Try the exact code first, then (for a leap month that never occurs in
    // range) its plain form — and within each, prefer a year where the day fits un-clamped.
    let want = match getm(i, &Value::Obj(snap.clone()), "monthCode")? {
        Value::Undefined => cal_month_code(cal, anchor),
        mc => i.to_string(&mc).map_err(unab)?.to_string(),
    };
    let want_day = match getm(i, &Value::Obj(snap.clone()), "day")? {
        Value::Undefined => 0,
        dv => to_int(i, &dv)?,
    };
    let plain = want.trim_end_matches('L').to_string();
    let candidates: Vec<&str> = if want.ends_with('L') {
        vec![&want, &plain]
    } else {
        vec![&want]
    };
    for cand in &candidates {
        for require_day in [true, false] {
            for cy in (start_cy - 40..=start_cy).rev() {
                if let Ok(iso) = resolve(i, cy) {
                    if cal_month_code(cal, iso) == *cand
                        && (!require_day || cal_fields(cal, iso).2 == want_day)
                        && epoch_days(iso) <= epoch_days(ref_iso)
                    {
                        return Ok(iso);
                    }
                }
            }
        }
    }
    // Fallback: the anchor itself (should be unreachable for representable month-days).
    Ok(anchor)
}
fn to_monthday(i: &mut Interp, v: &Value, opts: &Value) -> Result<IsoDate, Value> {
    if let Some(Temporal::MonthDay(d)) = get(i, v) {
        to_overflow(i, opts)?;
        return Ok(d);
    }
    match v {
        Value::Str(s) => {
            parse_month_day(s).ok_or_else(|| i.make_error("RangeError", "invalid month-day"))
        }
        Value::Obj(_) => {
            read_calendar(i, v)?;
            let cal = input_cal(i, v)?;
            if &*cal == "iso8601" {
                // The day ceiling is computed against the provided year (or the ISO reference year
                // 1972, also a leap year) but the stored reference year is always 1972.
                let year = field_int(i, v, "year", 1972)?;
                let month = field_month(i, v)?;
                let day = field_req(i, v, "day")?;
                let ovf = to_overflow(i, opts)?;
                let month = regulate_high(i, month, 12, ovf, "month")? as u8;
                let day = regulate_high(i, day, days_in_month(year, month) as i64, ovf, "day")? as u8;
                Ok(IsoDate { year: 1972, month, day })
            } else {
                // Snapshot the caller's fields once (in the observable order), then resolve the
                // reference date via the calendar-specific search.
                let snap = i.new_object();
                let mut has_year = false;
                for k in ["day", "era", "eraYear", "month", "monthCode", "year"] {
                    let fv = getm(i, v, k)?;
                    if !matches!(fv, Value::Undefined) {
                        if matches!(k, "year" | "era" | "eraYear") {
                            has_year = true;
                        }
                        setm(&snap, k, fv);
                    }
                }
                let ovf = to_overflow(i, opts)?;
                cal_month_day_reference(i, &cal, &snap, has_year, ovf)
            }
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.PlainMonthDay")),
    }
}

// ===== Duration ===============================================================================

fn install_duration(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Temporal.Duration", proto.clone());
    def_getter(it, &proto, "years", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.years as f64))
    });
    def_getter(it, &proto, "months", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.months as f64))
    });
    def_getter(it, &proto, "weeks", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.weeks as f64))
    });
    def_getter(it, &proto, "days", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.days as f64))
    });
    def_getter(it, &proto, "hours", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.hours as f64))
    });
    def_getter(it, &proto, "minutes", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.minutes as f64))
    });
    def_getter(it, &proto, "seconds", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.seconds as f64))
    });
    def_getter(it, &proto, "milliseconds", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.ms as f64))
    });
    def_getter(it, &proto, "microseconds", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.us as f64))
    });
    def_getter(it, &proto, "nanoseconds", |i, t, _| {
        Ok(Value::Num(as_duration(i, &t)?.ns as f64))
    });
    def_getter(it, &proto, "sign", |i, t, _| {
        Ok(Value::Num(duration_sign(as_duration(i, &t)?) as f64))
    });
    def_getter(it, &proto, "blank", |i, t, _| {
        Ok(Value::Bool(duration_sign(as_duration(i, &t)?) == 0))
    });

    it.def_method(&proto, "toString", 0, |i, t, a| {
        let d = as_duration(i, &t)?;
        // Validate the options: smallestUnit (if present) must be second or smaller, and
        // roundingMode / fractionalSecondDigits must be well-formed.
        let opts = arg(a, 0);
        let smallest = opt_str(i, &opts, "smallestUnit", "")?;
        let su = smallest.strip_suffix('s').unwrap_or(&smallest);
        if !su.is_empty()
            && !matches!(su, "second" | "millisecond" | "microsecond" | "nanosecond")
        {
            return Err(i.make_error("RangeError", "smallestUnit must be second or smaller"));
        }
        let mode = opt_str(i, &opts, "roundingMode", "trunc")?;
        check_mode(i, &mode)?;
        read_frac_digits(i, &opts)?;
        Ok(Value::str(fmt_duration(d)))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        Ok(Value::str(fmt_duration(as_duration(i, &t)?)))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error("TypeError", "Temporal.Duration has no valueOf; use compare"))
    });
    it.def_method(&proto, "negated", 0, |i, t, _| {
        let d = as_duration(i, &t)?;
        Ok(make(
            i,
            "Temporal.Duration",
            Temporal::Duration(neg_duration(d)),
        ))
    });
    it.def_method(&proto, "abs", 0, |i, t, _| {
        let d = as_duration(i, &t)?;
        let d = if duration_sign(d) < 0 {
            neg_duration(d)
        } else {
            d
        };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(d)))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "with() argument must be an object"));
        }
        // ToTemporalPartialDurationRecord: read alphabetically; at least one field must be present.
        let mut any = false;
        let mut read = |i: &mut Interp, key: &str, cur: i64| -> Result<i64, Value> {
            let fv = getm(i, &f, key)?;
            if matches!(fv, Value::Undefined) {
                Ok(cur)
            } else {
                any = true;
                to_int_integral(i, &fv)
            }
        };
        let days = read(i, "days", d.days)?;
        let hours = read(i, "hours", d.hours)?;
        let us = read(i, "microseconds", d.us)?;
        let ms = read(i, "milliseconds", d.ms)?;
        let minutes = read(i, "minutes", d.minutes)?;
        let months = read(i, "months", d.months)?;
        let ns = read(i, "nanoseconds", d.ns)?;
        let seconds = read(i, "seconds", d.seconds)?;
        let weeks = read(i, "weeks", d.weeks)?;
        let years = read(i, "years", d.years)?;
        if !any {
            return Err(i.make_error("TypeError", "with() requires at least one duration field"));
        }
        let nd = IsoDuration { years, months, weeks, days, hours, minutes, seconds, ms, us, ns };
        validate_duration(i, nd)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(nd)))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let o = to_duration(i, &arg(a, 0))?;
        let r = add_duration(d, o, 1);
        validate_duration(i, r)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(r)))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let o = to_duration(i, &arg(a, 0))?;
        let r = add_duration(d, o, -1);
        validate_duration(i, r)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(r)))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let arg0 = arg(a, 0);
        if matches!(arg0, Value::Undefined) {
            return Err(i.make_error("TypeError", "round() requires an options argument"));
        }
        let (o, shorthand) = round_opts(&arg0);
        let smallest_raw = match &shorthand {
            Some(s) => s.clone(),
            None => opt_str(i, &o, "smallestUnit", "")?,
        };
        let largest_raw = opt_str(i, &o, "largestUnit", "")?;
        let incr_raw = opt_num(i, &o, "roundingIncrement", 1)?;
        let mode = opt_str(i, &o, "roundingMode", "halfExpand")?;
        let rel = read_relative_to(i, &o)?;
        check_mode(i, &mode)?;

        if smallest_raw.is_empty() && largest_raw.is_empty() {
            return Err(i.make_error("RangeError", "round() requires smallestUnit or largestUnit"));
        }
        // Resolve smallestUnit (default nanosecond) and validate.
        let smallest: String = if smallest_raw.is_empty() {
            "nanosecond".into()
        } else {
            match unit_rank(&smallest_raw) {
                Some(_) => sing(&smallest_raw).into(),
                None => return Err(i.make_error("RangeError", "invalid smallestUnit")),
            }
        };
        // Resolve largestUnit ("auto" -> larger of existing-largest and smallestUnit).
        let largest: String = if largest_raw.is_empty() || largest_raw == "auto" {
            let ex = default_largest(&d);
            if unit_rank(ex) >= unit_rank(&smallest) {
                ex.into()
            } else {
                smallest.clone()
            }
        } else {
            match unit_rank(&largest_raw) {
                Some(_) => sing(&largest_raw).into(),
                None => return Err(i.make_error("RangeError", "invalid largestUnit")),
            }
        };
        let srank = unit_rank(&smallest).unwrap();
        let lrank = unit_rank(&largest).unwrap();
        if srank > lrank {
            return Err(i.make_error("RangeError", "smallestUnit is larger than largestUnit"));
        }
        // Validate roundingIncrement.
        let scal = matches!(smallest.as_str(), "year" | "month" | "week" | "day");
        if scal {
            if incr_raw < 1 {
                return Err(i.make_error("RangeError", "roundingIncrement out of range"));
            }
            if incr_raw > 1 && smallest != largest {
                return Err(i.make_error(
                    "RangeError",
                    "cannot round to an increment > 1 while balancing",
                ));
            }
        } else {
            check_increment(i, &smallest, incr_raw)?;
        }
        let incr = incr_raw as i128;
        let day_ns = 86_400_000_000_000i128;
        let sign = duration_sign(d);
        // A reference point is required for calendar units (years/months/weeks).
        let need_rel = d.years != 0
            || d.months != 0
            || d.weeks != 0
            || matches!(smallest.as_str(), "year" | "month" | "week")
            || matches!(largest.as_str(), "year" | "month" | "week");

        let result = if let Some(rel) = rel {
            let (ed, et) = add_full_duration(rel, d);
            let dest = (epoch_days(ed) - epoch_days(rel)) as i128 * day_ns + time_to_ns(et) as i128;
            if scal {
                let bal = diff_date_greedy("iso8601", rel, ed, &largest);
                let rounded = round_calendar_unit(rel, dest, &bal, &smallest, incr, sign, &mode);
                // Bubble the rounded date up to the largest unit (weeks are never re-balanced away).
                if smallest == "week" {
                    rounded
                } else {
                    diff_date_greedy("iso8601", rel, add_date_dur(rel, rounded), &largest)
                }
            } else {
                let un = unit_ns(&smallest).unwrap();
                let rounded = round_ns(dest, un * incr, &mode);
                if lrank >= 6 {
                    // largestUnit is a date unit: keep whole days, balance sub-day time.
                    let rdays = rounded / day_ns;
                    let sub = rounded % day_ns;
                    let (y, m, da) = civil_from_days(epoch_days(rel) + rdays as i64);
                    let mut out = diff_date_greedy(
                        "iso8601",
                        rel,
                        IsoDate {
                            year: y,
                            month: m,
                            day: da,
                        },
                        &largest,
                    );
                    let tb = balance_ns(sub, "hour");
                    out.hours = tb.hours;
                    out.minutes = tb.minutes;
                    out.seconds = tb.seconds;
                    out.ms = tb.ms;
                    out.us = tb.us;
                    out.ns = tb.ns;
                    out
                } else {
                    balance_ns(rounded, &largest)
                }
            }
        } else {
            if need_rel {
                return Err(
                    i.make_error("RangeError", "rounding calendar units requires relativeTo")
                );
            }
            // Without a reference point, days are fixed 24-hour spans.
            let total = d.days as i128 * day_ns + duration_time_ns(d);
            let un = if smallest == "day" {
                day_ns
            } else {
                unit_ns(&smallest).unwrap()
            };
            let rounded = round_ns(total, un * incr, &mode);
            balance_ns(rounded, &largest)
        };
        Ok(make(i, "Temporal.Duration", Temporal::Duration(result)))
    });
    it.def_method(&proto, "total", 1, |i, t, a| {
        let d = as_duration(i, &t)?;
        let (o, shorthand) = round_opts(&arg(a, 0));
        let unit_raw = match shorthand {
            Some(s) => s,
            None => opt_str(i, &o, "unit", "")?,
        };
        if unit_raw.is_empty() {
            return Err(i.make_error("RangeError", "unit is required"));
        }
        if unit_rank(&unit_raw).is_none() {
            return Err(i.make_error("RangeError", "invalid unit"));
        }
        let unit = sing(&unit_raw);
        let rel = read_relative_to(i, &o)?;
        let day_ns = 86_400_000_000_000i128;
        let sign = duration_sign(d);
        let need_rel = d.years != 0
            || d.months != 0
            || d.weeks != 0
            || matches!(unit, "year" | "month" | "week");
        if need_rel && rel.is_none() {
            return Err(i.make_error(
                "RangeError",
                "total of a calendar duration requires relativeTo",
            ));
        }
        // dest is the duration's nanosecond span from the reference (or from a 24h-day origin).
        let (rel_o, dest) = match rel {
            Some(rel) => {
                let (ed, et) = add_full_duration(rel, d);
                (
                    Some((rel, ed)),
                    (epoch_days(ed) - epoch_days(rel)) as i128 * day_ns + time_to_ns(et) as i128,
                )
            }
            None => (None, d.days as i128 * day_ns + duration_time_ns(d)),
        };
        let value = if matches!(unit, "year" | "month" | "week") {
            let (rel, ed) = rel_o.unwrap();
            let bal = diff_date_greedy("iso8601", rel, ed, unit);
            let comp = match unit {
                "year" => bal.years,
                "month" => bal.months,
                _ => bal.weeks,
            };
            let mk = |c: i64| match unit {
                "year" => IsoDuration {
                    years: c,
                    ..Default::default()
                },
                "month" => IsoDuration {
                    years: bal.years,
                    months: c,
                    ..Default::default()
                },
                _ => IsoDuration {
                    years: bal.years,
                    months: bal.months,
                    weeks: c,
                    ..Default::default()
                },
            };
            let sd = add_date_dur(rel, mk(comp));
            let ed2 = add_date_dur(rel, mk(comp + sign));
            let start_ns = (epoch_days(sd) - epoch_days(rel)) as i128 * day_ns;
            let den_p = (epoch_days(ed2) - epoch_days(rel)) as i128 * day_ns - start_ns;
            // total = comp + (dest - start)/den_p * sign
            let mut num = comp as i128 * den_p + (dest - start_ns) * sign as i128;
            let mut den = den_p;
            if den < 0 {
                num = -num;
                den = -den;
            }
            ratio_to_f64(num, den)
        } else {
            let u = if unit == "day" {
                day_ns
            } else {
                unit_ns(unit).unwrap()
            };
            ratio_to_f64(dest, u)
        };
        Ok(Value::Num(value))
    });

    let ctor = add_ctor(it, ns, "Duration", 0, proto, |i, _t, a| {
        require_new(i)?;
        let d = IsoDuration {
            years: dur_arg(i, &arg(a, 0))?,
            months: dur_arg(i, &arg(a, 1))?,
            weeks: dur_arg(i, &arg(a, 2))?,
            days: dur_arg(i, &arg(a, 3))?,
            hours: dur_arg(i, &arg(a, 4))?,
            minutes: dur_arg(i, &arg(a, 5))?,
            seconds: dur_arg(i, &arg(a, 6))?,
            ms: dur_arg(i, &arg(a, 7))?,
            us: dur_arg(i, &arg(a, 8))?,
            ns: dur_arg(i, &arg(a, 9))?,
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
        let has_cal = x.years != 0
            || x.months != 0
            || x.weeks != 0
            || y.years != 0
            || y.months != 0
            || y.weeks != 0;
        let (xn, yn) = if has_cal {
            let start = read_relative_to(i, &arg(a, 2))?.ok_or_else(|| {
                i.make_error(
                    "RangeError",
                    "comparing calendar durations requires relativeTo",
                )
            })?;
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
/// Whole-nanosecond magnitude of a duration's day-and-below portion (used for the range bound).
fn duration_total_ns(d: IsoDuration) -> i128 {
    d.days as i128 * 86_400_000_000_000
        + d.hours as i128 * 3_600_000_000_000
        + d.minutes as i128 * 60_000_000_000
        + d.seconds as i128 * 1_000_000_000
        + d.ms as i128 * 1_000_000
        + d.us as i128 * 1_000
        + d.ns as i128
}
/// IsValidDuration: every non-zero field shares one sign; years/months/weeks have magnitude < 2^32;
/// and the combined days-and-below seconds total has magnitude < 2^53. (Finiteness/integrality is
/// already enforced when the fields are read.)
fn validate_duration(i: &Interp, d: IsoDuration) -> Result<(), Value> {
    let mut sign = 0i64;
    for v in [
        d.years, d.months, d.weeks, d.days, d.hours, d.minutes, d.seconds, d.ms, d.us, d.ns,
    ] {
        if v != 0 {
            let s = if v < 0 { -1 } else { 1 };
            if sign != 0 && sign != s {
                return Err(i.make_error("RangeError", "mixed-sign duration"));
            }
            sign = s;
        }
    }
    const MAX_YMW: i64 = 4_294_967_296; // 2^32
    let oob = |v: i64| v >= MAX_YMW || v <= -MAX_YMW;
    if oob(d.years) || oob(d.months) || oob(d.weeks) {
        return Err(i.make_error("RangeError", "duration field is outside the valid range"));
    }
    const MAX_TOTAL_NS: i128 = 9_007_199_254_740_992_000_000_000; // 2^53 seconds, in nanoseconds
    let total = duration_total_ns(d);
    if total >= MAX_TOTAL_NS || total <= -MAX_TOTAL_NS {
        return Err(i.make_error("RangeError", "duration is outside the valid range"));
    }
    Ok(())
}
fn to_duration(i: &mut Interp, v: &Value) -> Result<IsoDuration, Value> {
    if let Some(Temporal::Duration(d)) = get(i, v) {
        return Ok(d);
    }
    match v {
        Value::Str(s) => {
            let d = parse_duration_str(s)
                .ok_or_else(|| i.make_error("RangeError", "invalid duration"))?;
            validate_duration(i, d)?;
            Ok(d)
        }
        Value::Obj(_) => {
            // ToTemporalPartialDurationRecord: read the fields in alphabetical order; at least one
            // recognized field must be present (an empty object / array / singular-name is a TypeError).
            let mut any = false;
            let mut read = |i: &mut Interp, key: &str| -> Result<i64, Value> {
                let fv = getm(i, v, key)?;
                if matches!(fv, Value::Undefined) {
                    Ok(0)
                } else {
                    any = true;
                    to_int_integral(i, &fv)
                }
            };
            let days = read(i, "days")?;
            let hours = read(i, "hours")?;
            let us = read(i, "microseconds")?;
            let ms = read(i, "milliseconds")?;
            let minutes = read(i, "minutes")?;
            let months = read(i, "months")?;
            let ns = read(i, "nanoseconds")?;
            let seconds = read(i, "seconds")?;
            let weeks = read(i, "weeks")?;
            let years = read(i, "years")?;
            if !any {
                return Err(i.make_error("TypeError", "invalid Temporal.Duration-like: no recognized fields"));
            }
            let d = IsoDuration { years, months, weeks, days, hours, minutes, seconds, ms, us, ns };
            validate_duration(i, d)?;
            Ok(d)
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.Duration")),
    }
}
fn parse_duration_str(s: &str) -> Option<IsoDuration> {
    let s = s.trim();
    let (neg, s) = match s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+').map(|_| s))
    {
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
        Ok(Value::Num(
            (as_instant(i, &t)?.div_euclid(1_000_000)) as f64,
        ))
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
        Ok(Value::str(format!(
            "{}T{}Z",
            fmt_date(IsoDate {
                year: y,
                month: mo,
                day: da
            }),
            ts
        )))
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
            fmt_date(IsoDate {
                year: y,
                month: mo,
                day: da
            }),
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
        let tz_raw: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let tz = normalize_tz(i, &tz_raw)?;
        let offset = zone_offset(&tz, e);
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: e, offset_ns: offset, tz, }))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        if dur.years != 0 || dur.months != 0 || dur.weeks != 0 || dur.days != 0 {
            return Err(i.make_error("RangeError", "Instant.add does not accept calendar units"));
        }
        let r = check_instant(i, x + duration_time_ns(dur))?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(r)))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        if dur.years != 0 || dur.months != 0 || dur.weeks != 0 || dur.days != 0 {
            return Err(i.make_error(
                "RangeError",
                "Instant.subtract does not accept calendar units",
            ));
        }
        let r = check_instant(i, x - duration_time_ns(dur))?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(r)))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let (o, shorthand) = round_opts(&arg(a, 0));
        let smallest = match shorthand {
            Some(s) => s,
            None => opt_str(i, &o, "smallestUnit", "")?,
        };
        let unit = unit_ns(&smallest)
            .ok_or_else(|| i.make_error("RangeError", "smallestUnit is required"))?;
        let incr_raw = opt_num(i, &o, "roundingIncrement", 1)?;
        let mode = opt_str(i, &o, "roundingMode", "halfExpand")?;
        check_mode(i, &mode)?;
        // Instant rounding: the increment times the unit must evenly divide a 24-hour solar day
        // (inclusive — a full day is allowed).
        let max = (86_400_000_000_000i128 / unit) as i64;
        if incr_raw < 1 || incr_raw > max || max % incr_raw != 0 {
            return Err(i.make_error("RangeError", "roundingIncrement out of range"));
        }
        let incr = incr_raw as i128;
        Ok(make(
            i,
            "Temporal.Instant",
            Temporal::Instant(round_ns(x, unit * incr, &mode)),
        ))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let y = to_instant(i, &arg(a, 0))?;
        let dur = time_diff(i, y - x, &arg(a, 1), false, 3)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let x = as_instant(i, &t)?;
        let y = to_instant(i, &arg(a, 0))?;
        let dur = time_diff(i, y - x, &arg(a, 1), true, 3)?;
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    let ctor = add_ctor(it, ns, "Instant", 1, proto, |i, _t, a| {
        require_new(i)?;
        let ns = to_epoch_bigint(i, &arg(a, 0))?;
        let ns = check_instant(i, ns)?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(ns)))
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let n = to_instant(i, &arg(a, 0))?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(n)))
    });
    it.def_method(&ctor, "fromEpochMilliseconds", 1, |i, _t, a| {
        // ToNumber then require an integer in the representable range.
        let n = i.to_number(&arg(a, 0)).map_err(unab)?;
        if !n.is_finite() || n.fract() != 0.0 {
            return Err(i.make_error("RangeError", "epochMilliseconds must be an integer"));
        }
        let ns = check_instant(i, n as i128 * 1_000_000)?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(ns)))
    });
    it.def_method(&ctor, "fromEpochNanoseconds", 1, |i, _t, a| {
        let ns = to_epoch_bigint(i, &arg(a, 0))?;
        let ns = check_instant(i, ns)?;
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
    // ToTemporalInstant: a non-Temporal value is coerced to a string (ToPrimitive, string hint) and
    // parsed as an ISO instant.
    match v {
        Value::BigInt(n) => Ok(*n),
        _ => {
            let s = i.to_string(v).map_err(unab)?;
            parse_instant(&s).ok_or_else(|| i.make_error("RangeError", "invalid Instant string"))
        }
    }
}
/// ToBigInt for an epoch-nanosecond argument: BigInt/boolean/numeric-string only; number, null,
/// undefined, symbol, and object are a TypeError.
fn to_epoch_bigint(i: &mut Interp, v: &Value) -> Result<i128, Value> {
    match v {
        Value::BigInt(n) => Ok(*n),
        Value::Bool(b) => Ok(*b as i128),
        Value::Str(s) => s
            .trim()
            .parse::<i128>()
            .map_err(|_| i.make_error("SyntaxError", "cannot convert string to a BigInt")),
        _ => Err(i.make_error("TypeError", "epochNanoseconds must be a BigInt")),
    }
}

/// Reject an epoch-nanosecond value outside Temporal's representable instant range (±8.64e21).
fn check_instant(i: &Interp, ns: i128) -> Result<i128, Value> {
    if ns.abs() > 8_640_000_000_000_000_000_000 {
        Err(i.make_error("RangeError", "instant is outside the representable range"))
    } else {
        Ok(ns)
    }
}

/// Parse an ISO instant string (must carry a `Z` or `±HH:MM` offset).
fn parse_instant(s: &str) -> Option<i128> {
    let p = parse_iso(s)?;
    let date = p.date?; // an instant needs a full date-time...
    let time = p.time?;
    let offset = match p.offset {
        Off::Z => 0,
        Off::Num(n) => n,
        Off::None => return None, // ...with an absolute reference (Z or numeric offset)
    };
    let ns = dt_ns(date, time) - offset as i128;
    if ns.abs() > 8_640_000_000_000_000_000_000 {
        return None; // outside the representable instant range
    }
    Some(ns)
}

// ===== ZonedDateTime ==========================================================================

/// A named-zone rule: standard offset, optional DST offset + transition rules. Transition rules are
/// `(month, week, weekday, hour)` where week 5 = "last"; weekday 0 = Sunday. `utc_rule` means the
/// transition hour is in UTC (EU style) rather than local wall time (US style).
/// The UTC offset (ns) of zone `tz` at instant `epoch_ns`.
fn zone_offset(tz: &str, epoch_ns: i128) -> i64 {
    if let Some(off) = parse_fixed_offset(tz) {
        return off;
    }
    // The generated IANA transition tables (seconds-resolution) cover named zones.
    let epoch_sec = epoch_ns.div_euclid(1_000_000_000) as i64;
    if let Some(off_s) = crate::tz::offset_at(tz, epoch_sec) {
        return off_s as i64 * 1_000_000_000;
    }
    0
}
/// The epoch instants a local wall-clock time maps to in `tz`: 0 in a spring-forward gap, 2 in a
/// fall-back overlap, else 1 (GetPossibleInstantsFor), sorted ascending.
fn possible_epochs(tz: &str, local_ns: i128) -> Vec<i128> {
    let day = 86_400_000_000_000i128;
    let mut v: Vec<i128> = Vec::new();
    for off in [zone_offset(tz, local_ns - day), zone_offset(tz, local_ns + day)] {
        let inst = local_ns - off as i128;
        if zone_offset(tz, inst) == off && !v.contains(&inst) {
            v.push(inst);
        }
    }
    v.sort_unstable();
    v
}
/// DisambiguatePossibleInstants: the epoch instant a local time maps to under `disamb`
/// (compatible/earlier/later/reject). Returns a RangeError for `reject` on an ambiguous/skipped time.
fn local_to_epoch(i: &Interp, tz: &str, local_ns: i128, disamb: &str) -> Result<i128, Value> {
    let poss = possible_epochs(tz, local_ns);
    match poss.len() {
        1 => Ok(poss[0]),
        0 => {
            if disamb == "reject" {
                return Err(i.make_error("RangeError", "no such local time (skipped by a DST gap)"));
            }
            let day = 86_400_000_000_000i128;
            let gap = (zone_offset(tz, local_ns + day) - zone_offset(tz, local_ns - day)) as i128;
            if disamb == "earlier" {
                Ok(*possible_epochs(tz, local_ns - gap).first().unwrap_or(&(local_ns - gap)))
            } else {
                Ok(*possible_epochs(tz, local_ns + gap).last().unwrap_or(&(local_ns + gap)))
            }
        }
        n => match disamb {
            "later" => Ok(poss[n - 1]),
            "reject" => Err(i.make_error("RangeError", "ambiguous local time (DST overlap)")),
            _ => Ok(poss[0]), // compatible / earlier
        },
    }
}
/// InterpretISODateTimeOffset: resolve a local wall-clock time (+ optional supplied UTC offset) to an
/// (epoch_ns, offset_ns) pair under the `offset` (use/ignore/prefer/reject) and `disambiguation`
/// options.
fn interpret_offset(
    i: &Interp,
    tz: &str,
    local: i128,
    has_offset: bool,
    offset_ns: i64,
    offset_opt: &str,
    disamb: &str,
) -> Result<(i128, i64), Value> {
    if !has_offset || offset_opt == "ignore" {
        let epoch = local_to_epoch(i, tz, local, disamb)?;
        return Ok((epoch, (local - epoch) as i64));
    }
    if offset_opt == "use" {
        return Ok((local - offset_ns as i128, offset_ns));
    }
    // "prefer"/"reject": use the supplied offset if it is valid for one of the possible instants.
    for inst in possible_epochs(tz, local) {
        if (local - inst) as i64 == offset_ns {
            return Ok((inst, offset_ns));
        }
    }
    if offset_opt == "reject" {
        return Err(i.make_error("RangeError", "offset does not match the time zone"));
    }
    let epoch = local_to_epoch(i, tz, local, disamb)?;
    Ok((epoch, (local - epoch) as i64))
}
/// The offset (ns) for interpreting a local wall-clock time under the default "compatible"
/// disambiguation.
fn offset_for_local(tz: &str, local_ns: i128) -> i64 {
    let poss = possible_epochs(tz, local_ns);
    let inst = match poss.len() {
        0 => {
            // Spring-forward gap: shift the wall time forward by the gap (compatible), then take the
            // later instant.
            let day = 86_400_000_000_000i128;
            let gap = (zone_offset(tz, local_ns + day) - zone_offset(tz, local_ns - day)) as i128;
            *possible_epochs(tz, local_ns + gap).last().unwrap_or(&(local_ns - zone_offset(tz, local_ns) as i128))
        }
        _ => poss[0],
    };
    (local_ns - inst) as i64
}

/// Parse a fixed-offset id (`UTC`/`Z`/`±HH:MM[:SS]`) to ns, or None for a named zone.
/// Validate and canonicalize a time-zone identifier: a UTC-offset form (`±HH:MM[:SS]`) normalizes to
/// its canonical string, a named IANA zone canonicalizes to its registry name, and anything else is
/// a RangeError.
fn normalize_tz(i: &Interp, s: &str) -> Result<Rc<str>, Value> {
    let t = s.trim();
    if is_pure_offset(t) {
        return Ok(Rc::from(offset_string(tz_offset_ns(t)).as_str()));
    }
    // A named zone keeps its identifier as given (only case-normalized) — Temporal does not
    // canonicalize aliases on construction; `equals`/`compare` do that.
    if let Some(name) = crate::tz::registry_name(t) {
        return Ok(Rc::from(name));
    }
    // A full ISO date/time string: its `[tz]` annotation names the zone; otherwise a `Z`/offset does.
    if let Some(p) = parse_iso(t) {
        if let Some(tzname) = p.tz {
            if let Some(name) = crate::tz::registry_name(&tzname) {
                return Ok(Rc::from(name));
            }
            if is_pure_offset(&tzname) {
                return Ok(Rc::from(offset_string(tz_offset_ns(&tzname)).as_str()));
            }
        } else {
            match p.offset {
                Off::Z => return Ok(Rc::from("UTC")),
                Off::Num(n) => return Ok(Rc::from(offset_string(n).as_str())),
                Off::None => {}
            }
        }
    }
    Err(i.make_error("RangeError", format!("unknown time zone: {t}")))
}

/// Whether `s` is a minute-precision UTC-offset identifier: `±HH`, `±HHMM`, or `±HH:MM`. Sub-minute
/// (seconds/fraction) offsets are not valid time-zone identifiers, and a longer string such as a
/// negative-year ISO date-time (which also starts with `-`) is not an offset.
fn is_pure_offset(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || (b[0] != b'+' && b[0] != b'-') {
        return false;
    }
    let rest = &s[1..];
    let all_digits = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
    match rest.split_once(':') {
        Some((h, m)) => h.len() == 2 && m.len() == 2 && all_digits(h) && all_digits(m),
        None => (rest.len() == 2 || rest.len() == 4) && all_digits(rest),
    }
}

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
    (
        IsoDate {
            year: y,
            month: mo,
            day: da,
        },
        t,
    )
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
/// ToTemporalZonedDateTime: a ZonedDateTime, an ISO string with `[timeZone]`, or a fields object
/// carrying `timeZone`.
fn to_zoned(i: &mut Interp, v: &Value, opts: &Value) -> Result<(i128, i64, Rc<str>), Value> {
    if let Some(Temporal::Zoned {
        epoch_ns,
        offset_ns,
        tz,
    }) = get(i, v)
    {
        to_overflow(i, opts)?;
        return Ok((epoch_ns, offset_ns, tz));
    }
    match v {
        Value::Str(s) => {
            let p =
                parse_iso(s).ok_or_else(|| i.make_error("RangeError", "invalid ZonedDateTime"))?;
            let date = p
                .date
                .ok_or_else(|| i.make_error("RangeError", "invalid ZonedDateTime"))?;
            let tz: Rc<str> = match p.tz {
                Some(t) => normalize_tz(i, &t)?,
                None => return Err(i.make_error("RangeError", "missing time zone")),
            };
            let time = p.time.unwrap_or(IsoTime {
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                us: 0,
                ns: 0,
            });
            let local = dt_ns(date, time);
            let disamb = opt_str(i, opts, "disambiguation", "compatible")?;
            // A string with an offset defaults to `offset: reject`; the fixed-offset zone case ("Z" or
            // a numeric-offset id) always uses the annotation's own offset.
            let offset_opt = opt_str(i, opts, "offset", "reject")?;
            let (has_off, off_ns) = match p.offset {
                Off::Z => (true, 0),
                Off::Num(n) => (true, n),
                Off::None => (false, 0),
            };
            if parse_fixed_offset(&tz).is_some() {
                let off = zone_offset(&tz, local) ; // fixed offset zone
                return Ok((local - off as i128, off, tz));
            }
            let (epoch, off) = interpret_offset(i, &tz, local, has_off, off_ns, &offset_opt, &disamb)?;
            Ok((epoch, off, tz))
        }
        Value::Obj(_) => {
            read_calendar(i, v)?;
            let tzv = getm(i, v, "timeZone")?;
            if matches!(tzv, Value::Undefined) {
                return Err(i.make_error("TypeError", "missing timeZone"));
            }
            let tz_raw: Rc<str> = match &tzv {
                Value::Str(s) => s.clone(),
                _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
            };
            let tz = normalize_tz(i, &tz_raw)?;
            let zcal = input_cal(i, v)?;
            // The bag's own `offset` field (a "+HH:MM" string), if present.
            let offv = getm(i, v, "offset")?;
            let (has_off, off_ns) = match &offv {
                Value::Undefined => (false, 0),
                _ => {
                    let s = i.to_string(&offv).map_err(unab)?;
                    if !(s.starts_with('+') || s.starts_with('-')) {
                        return Err(i.make_error("RangeError", "invalid offset string"));
                    }
                    (true, tz_offset_ns(&s))
                }
            };
            let snap = snapshot_date_fields(i, v)?;
            let (traw, _) = read_time_raw(i, v)?;
            let disamb = opt_str(i, opts, "disambiguation", "compatible")?;
            let offset_opt = opt_str(i, opts, "offset", "reject")?;
            let ovf = to_overflow(i, opts)?;
            let draw = read_date_raw_cal(i, &snap, &zcal, ovf)?;
            let date = regulate_date(i, draw, ovf)?;
            let time = regulate_time(i, traw, ovf)?;
            let local = dt_ns(date, time);
            if parse_fixed_offset(&tz).is_some() {
                let off = zone_offset(&tz, local);
                return Ok((local - off as i128, off, tz));
            }
            let (epoch, off) = interpret_offset(i, &tz, local, has_off, off_ns, &offset_opt, &disamb)?;
            Ok((epoch, off, tz))
        }
        _ => Err(i.make_error("TypeError", "cannot convert to Temporal.ZonedDateTime")),
    }
}

fn install_zoned(it: &mut Interp, ns: &Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos
        .insert("Temporal.ZonedDateTime", proto.clone());

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
    // Calendar-aware field getter: `$f` receives the full `cal_fields` tuple.
    macro_rules! calf_get {
        ($name:literal, $f:expr) => {
            def_getter(it, &proto, $name, |i, t, _| {
                let (e, o, _) = as_zoned(i, &t)?;
                Ok($f(cal_fields(&cal_of(i, &t), zoned_local(e, o).0)))
            });
        };
    }
    def_getter(it, &proto, "year", |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(Value::Num(cal_year_num(&cal_of(i, &t), zoned_local(e, o).0) as f64))
    });
    def_getter(it, &proto, "era", |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(cal_era(&cal_of(i, &t), zoned_local(e, o).0).0.map(Value::str).unwrap_or(Value::Undefined))
    });
    def_getter(it, &proto, "eraYear", |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(cal_era(&cal_of(i, &t), zoned_local(e, o).0).1.map(|v| Value::Num(v as f64)).unwrap_or(Value::Undefined))
    });
    type CF = (i64, i64, i64, i64, i64, i64, i64, bool);
    calf_get!("month", |f: CF| Value::Num(f.1 as f64));
    calf_get!("day", |f: CF| Value::Num(f.2 as f64));
    def_getter(it, &proto, "monthCode", |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(Value::from_string(cal_month_code(&cal_of(i, &t), zoned_local(e, o).0)))
    });
    date_get!("dayOfWeek", |d: IsoDate| Value::Num(
        iso_day_of_week(d) as f64
    ));
    calf_get!("dayOfYear", |f: CF| Value::Num(f.5 as f64));
    calf_get!("daysInMonth", |f: CF| Value::Num(f.3 as f64));
    calf_get!("daysInYear", |f: CF| Value::Num(f.6 as f64));
    calf_get!("inLeapYear", |f: CF| Value::Bool(f.7));
    date_get!("weekOfYear", |d: IsoDate| Value::Num(iso_week(d).0 as f64));
    date_get!("yearOfWeek", |d: IsoDate| Value::Num(iso_week(d).1 as f64));
    date_get!("daysInWeek", |_d: IsoDate| Value::Num(7.0));
    calf_get!("monthsInYear", |f: CF| Value::Num(f.4 as f64));
    def_getter(it, &proto, "hoursInDay", |i, t, _| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, _) = zoned_local(e, o);
        let midnight = IsoTime {
            hour: 0,
            minute: 0,
            second: 0,
            ms: 0,
            us: 0,
            ns: 0,
        };
        let today_local = dt_ns(d, midnight);
        let today = today_local - offset_for_local(&tz, today_local) as i128;
        let (ty, tm, td) = civil_from_days(epoch_days(d) + 1);
        let tomorrow_local = dt_ns(
            IsoDate {
                year: ty,
                month: tm,
                day: td,
            },
            midnight,
        );
        let tomorrow = tomorrow_local - offset_for_local(&tz, tomorrow_local) as i128;
        Ok(Value::Num((tomorrow - today) as f64 / 3_600_000_000_000.0))
    });
    def_getter(it, &proto, "epochSeconds", |i, t, _| {
        Ok(Value::Num(
            as_zoned(i, &t)?.0.div_euclid(1_000_000_000) as f64
        ))
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
    def_getter(it, &proto, "calendarId", |i, t, _| Ok(Value::from_string(cal_of(i, &t).to_string())));
    def_getter(it, &proto, "epochMilliseconds", |i, t, _| {
        Ok(Value::Num(as_zoned(i, &t)?.0.div_euclid(1_000_000) as f64))
    });
    def_getter(it, &proto, "epochNanoseconds", |i, t, _| {
        Ok(Value::BigInt(as_zoned(i, &t)?.0))
    });
    def_getter(it, &proto, "offsetNanoseconds", |i, t, _| {
        Ok(Value::Num(as_zoned(i, &t)?.1 as f64))
    });
    def_getter(it, &proto, "offset", |i, t, _| {
        Ok(Value::str(offset_string(as_zoned(i, &t)?.1)))
    });
    def_getter(it, &proto, "timeZoneId", |i, t, _| {
        Ok(Value::Str(as_zoned(i, &t)?.2))
    });

    it.def_method(&proto, "toInstant", 0, |i, t, _| {
        let (e, _, _) = as_zoned(i, &t)?;
        Ok(make(i, "Temporal.Instant", Temporal::Instant(e)))
    });
    it.def_method(&proto, "toPlainDate", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(
            i,
            "Temporal.PlainDate",
            Temporal::Date(zoned_local(e, o).0),
        ))
    });
    it.def_method(&proto, "toPlainTime", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(
            i,
            "Temporal.PlainTime",
            Temporal::Time(zoned_local(e, o).1),
        ))
    });
    it.def_method(&proto, "toPlainDateTime", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        Ok(make_like(i, &t, "Temporal.PlainDateTime", Temporal::DateTime(d, tm)))
    });
    it.def_method(&proto, "toPlainYearMonth", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(
            i,
            "Temporal.PlainYearMonth",
            Temporal::YearMonth(zoned_local(e, o).0),
        ))
    });
    it.def_method(&proto, "toPlainMonthDay", 0, |i, t, _| {
        let (e, o, _) = as_zoned(i, &t)?;
        Ok(make(
            i,
            "Temporal.PlainMonthDay",
            Temporal::MonthDay(zoned_local(e, o).0),
        ))
    });
    it.def_method(&proto, "startOfDay", 0, |i, t, _| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, _) = zoned_local(e, o);
        let midnight = IsoTime {
            hour: 0,
            minute: 0,
            second: 0,
            ms: 0,
            us: 0,
            ns: 0,
        };
        let local = dt_ns(d, midnight);
        let off = offset_for_local(&tz, local);
        let epoch = local - off as i128;
        Ok(make(
            i,
            "Temporal.ZonedDateTime",
            Temporal::Zoned {
                epoch_ns: epoch,
                offset_ns: off,
                tz,
            },
        ))
    });
    it.def_method(&proto, "equals", 1, |i, t, a| {
        // Two ZonedDateTimes are equal iff same instant, same *canonicalized* time zone, same calendar.
        let (e, _, tz) = as_zoned(i, &t)?;
        let tcal = cal_of(i, &t);
        let (oe, _, otz) = to_zoned(i, &arg(a, 0), &Value::Undefined)?;
        let ocal = input_cal(i, &arg(a, 0))?;
        let same_tz = crate::tz::canonicalize(&tz) == crate::tz::canonicalize(&otz);
        Ok(Value::Bool(e == oe && same_tz && tcal == ocal))
    });
    it.def_method(&proto, "valueOf", 0, |i, _t, _| {
        Err(i.make_error(
            "TypeError",
            "Temporal.ZonedDateTime has no valueOf; use compare",
        ))
    });
    it.def_method(&proto, "toJSON", 0, |i, t, _| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        Ok(Value::str(format!(
            "{}T{}{}[{}]",
            fmt_date(d),
            fmt_time(tm),
            offset_string(o),
            tz
        )))
    });
    it.def_method(&proto, "toString", 0, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        let ts = fmt_time_opts(i, tm, &arg(a, 0))?;
        Ok(Value::str(format!(
            "{}T{}{}[{}]{}",
            fmt_date(d),
            ts,
            offset_string(o),
            tz,
            cal_suffix(i, &arg(a, 0), &cal_of(i, &t))?
        )))
    });
    it.def_method(&proto, "add", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let (d, tm) = zoned_local(e, o);
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal_id = cal_of(i, &t);
        let (nd, ntm) = dt_add(i, d, tm, dur, 1, ovf, &cal_id)?;
        let local = dt_ns(nd, ntm);
        let off = offset_for_local(&tz, local);
        let epoch = local - off as i128;
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz, }))
    });
    it.def_method(&proto, "subtract", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let dur = to_duration(i, &arg(a, 0))?;
        let (d, tm) = zoned_local(e, o);
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal_id = cal_of(i, &t);
        let (nd, ntm) = dt_add(i, d, tm, dur, -1, ovf, &cal_id)?;
        let local = dt_ns(nd, ntm);
        let off = offset_for_local(&tz, local);
        let epoch = local - off as i128;
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz, }))
    });
    it.def_method(&proto, "with", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, tm) = zoned_local(e, o);
        let f = arg(a, 0);
        if !matches!(f, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "with() argument must be an object"));
        }
        let hour = field_int(i, &f, "hour", tm.hour as i64)? as u8;
        let minute = field_int(i, &f, "minute", tm.minute as i64)? as u8;
        let second = field_int(i, &f, "second", tm.second as i64)? as u8;
        let ms = field_int(i, &f, "millisecond", tm.ms as i64)? as u16;
        let us = field_int(i, &f, "microsecond", tm.us as i64)? as u16;
        let nsf = field_int(i, &f, "nanosecond", tm.ns as i64)? as u16;
        let ovf = to_overflow(i, &arg(a, 1))?;
        let cal = cal_of(i, &t);
        let nd = if &*cal == "iso8601" {
            let year = field_int(i, &f, "year", d.year)?;
            let month = field_int(i, &f, "month", d.month as i64)? as u8;
            let day = field_int(i, &f, "day", d.day as i64)? as u8;
            check_date(i, IsoDate { year, month, day })?
        } else {
            with_cal_date(i, &cal, d, &f, ovf)?
        };
        let nt = check_time(
            i,
            IsoTime {
                hour,
                minute,
                second,
                ms,
                us,
                ns: nsf,
            },
        )?;
        let local = dt_ns(nd, nt);
        let off = offset_for_local(&tz, local);
        let epoch = local - off as i128;
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz, }))
    });
    it.def_method(&proto, "withTimeZone", 1, |i, t, a| {
        let (e, _, _) = as_zoned(i, &t)?;
        let s = i.to_string(&arg(a, 0)).map_err(unab)?;
        let tz = normalize_tz(i, &s)?;
        let off = zone_offset(&tz, e);
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: e, offset_ns: off, tz }))
    });
    it.def_method(&proto, "withCalendar", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let cal = check_calendar(i, &arg(a, 0))?;
        let v = make(
            i,
            "Temporal.ZonedDateTime",
            Temporal::Zoned { epoch_ns: e, offset_ns: o, tz },
        );
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&proto, "getTimeZoneTransition", 1, |i, t, a| {
        let (e, _, tz) = as_zoned(i, &t)?;
        let dir = match arg(a, 0) {
            Value::Str(s) => s.to_string(),
            Value::Obj(_) => opt_str(i, &arg(a, 0), "direction", "")?,
            Value::Undefined => return Err(i.make_error("TypeError", "direction is required")),
            v => i.to_string(&v).map_err(unab)?.to_string(),
        };
        if dir != "next" && dir != "previous" {
            return Err(i.make_error("RangeError", "direction must be 'next' or 'previous'"));
        }
        let epoch_sec = (e.div_euclid(1_000_000_000)) as i64;
        match crate::tz::next_transition(&tz, epoch_sec, dir == "next") {
            Some(ts) => {
                let (off, _) = (zone_offset(&tz, ts as i128 * 1_000_000_000), ());
                Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: ts as i128 * 1_000_000_000, offset_ns: off, tz }))
            }
            None => Ok(Value::Null),
        }
    });
    it.def_method(&proto, "withPlainTime", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (d, _) = zoned_local(e, o);
        let nt = match arg(a, 0) {
            Value::Undefined => IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 },
            v => to_time(i, &v, &Value::Undefined)?,
        };
        let local = dt_ns(d, nt);
        let off = offset_for_local(&tz, local);
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: local - off as i128, offset_ns: off, tz }))
    });
    it.def_method(&proto, "withPlainDate", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let (_, tm) = zoned_local(e, o);
        let nd = to_date(i, &arg(a, 0), &Value::Undefined)?;
        let local = dt_ns(nd, tm);
        let off = offset_for_local(&tz, local);
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: local - off as i128, offset_ns: off, tz }))
    });
    it.def_method(&proto, "until", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        let (oe, _, _) = to_zoned(i, &arg(a, 0), &Value::Undefined)?;
        let (largest, smallest, incr, mode) = read_datetime_diff(i, &arg(a, 1))?;
        let dur = diff_zoned(i, e, o, oe, &tz, &cal, &largest, &smallest, incr, &mode);
        Ok(make(i, "Temporal.Duration", Temporal::Duration(dur)))
    });
    it.def_method(&proto, "since", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let cal = same_calendar(i, &t, &arg(a, 0))?;
        let (oe, _, _) = to_zoned(i, &arg(a, 0), &Value::Undefined)?;
        let (largest, smallest, incr, mode) = read_datetime_diff(i, &arg(a, 1))?;
        let dur = diff_zoned(i, e, o, oe, &tz, &cal, &largest, &smallest, incr, &negate_mode(&mode));
        Ok(make(i, "Temporal.Duration", Temporal::Duration(neg_duration(dur))))
    });
    it.def_method(&proto, "round", 1, |i, t, a| {
        let (e, o, tz) = as_zoned(i, &t)?;
        let opts = arg(a, 0);
        let smallest = sing(&opt_str(i, &opts, "smallestUnit", "")?).to_string();
        let incr_raw = opt_num(i, &opts, "roundingIncrement", 1)?;
        let mode = opt_str(i, &opts, "roundingMode", "halfExpand")?;
        check_mode(i, &mode)?;
        check_increment(i, &smallest, incr_raw)?;
        let incr = incr_raw as i128;
        if smallest == "day" {
            // Round to the nearest local day boundary; a DST day is 23/25h long, not a fixed 24h.
            let (d, _) = zoned_local(e, o);
            let sod = |dd: IsoDate| -> i128 {
                let mid = dt_ns(dd, IsoTime { hour: 0, minute: 0, second: 0, ms: 0, us: 0, ns: 0 });
                mid - offset_for_local(&tz, mid) as i128
            };
            let today = sod(d);
            let (ny, nm, nda) = civil_from_days(epoch_days(d) + 1);
            let tomorrow = sod(IsoDate { year: ny, month: nm, day: nda });
            let denom = (tomorrow - today) as f64;
            let frac = if denom == 0.0 { 0.0 } else { (e - today) as f64 / denom };
            let up = round_up_magnitude(&mode, frac, true, false);
            let epoch = if up { tomorrow } else { today };
            let off = zone_offset(&tz, epoch);
            return Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: epoch, offset_ns: off, tz }));
        }
        let unit = unit_ns(&smallest).ok_or_else(|| i.make_error("RangeError", "smallestUnit is required"))?;
        let local = e + o as i128;
        let rounded = round_ns(local, unit * incr, &mode);
        // Re-resolve the offset for the rounded wall-clock time (it may cross a DST transition).
        let off = offset_for_local(&tz, rounded);
        Ok(make_like(i, &t, "Temporal.ZonedDateTime", Temporal::Zoned { epoch_ns: rounded - off as i128, offset_ns: off, tz }))
    });

    let ctor = add_ctor(it, ns, "ZonedDateTime", 2, proto, |i, _t, a| {
        require_new(i)?;
        let epoch_ns = match arg(a, 0) {
            Value::BigInt(n) => n,
            _ => return Err(i.make_error("TypeError", "epochNanoseconds must be a BigInt")),
        };
        let tzv = arg(a, 1);
        let tz_raw: Rc<str> = match &tzv {
            Value::Str(s) => s.clone(),
            Value::Undefined => return Err(i.make_error("TypeError", "missing timeZone")),
            _ => Rc::from(i.to_string(&tzv).map_err(unab)?.as_ref()),
        };
        let tz = normalize_tz(i, &tz_raw)?;
        let cal = check_calendar(i, &arg(a, 2))?;
        let offset_ns = zone_offset(&tz, epoch_ns);
        let v = make(
            i,
            "Temporal.ZonedDateTime",
            Temporal::Zoned {
                epoch_ns,
                offset_ns,
                tz,
            },
        );
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let cal = input_cal(i, &arg(a, 0))?;
        let (epoch_ns, offset_ns, tz) = to_zoned(i, &arg(a, 0), &arg(a, 1))?;
        let v = make(
            i,
            "Temporal.ZonedDateTime",
            Temporal::Zoned {
                epoch_ns,
                offset_ns,
                tz,
            },
        );
        set_cal(i, &v, cal);
        Ok(v)
    });
    it.def_method(&ctor, "compare", 2, |i, _t, a| {
        let x = to_zoned(i, &arg(a, 0), &Value::Undefined)?.0;
        let y = to_zoned(i, &arg(a, 1), &Value::Undefined)?.0;
        Ok(Value::Num(x.cmp(&y) as i64 as f64))
    });
}

// ===== Now ====================================================================================

/// Validate a `Now.*ISO` time-zone argument (a string identifier, or undefined for the default).
fn now_validate_zone(i: &mut Interp, v: &Value) -> Result<(), Value> {
    match v {
        Value::Undefined => Ok(()),
        Value::Str(s) => validate_tz_string(i, s),
        _ => {
            let s = i.to_string(v).map_err(unab)?;
            validate_tz_string(i, &s)
        }
    }
}

fn install_now(it: &mut Interp, ns: &Gc) {
    let now = Object::new(Some(it.object_proto.clone()));
    // lumen has no real clock; the epoch is fixed at 1970-01-01T00:00:00Z. Structure/type tests
    // pass even though absolute-time tests do not.
    it.def_method(&now, "instant", 0, |i, _t, _| {
        Ok(make(i, "Temporal.Instant", Temporal::Instant(0)))
    });
    it.def_method(&now, "timeZoneId", 0, |_i, _t, _| Ok(Value::str("UTC")));
    it.def_method(&now, "zonedDateTimeISO", 0, |i, _t, a| {
        // The system zone (default UTC) at the fixed epoch.
        let tz = match arg(a, 0) {
            Value::Undefined => Rc::from("UTC"),
            v => {
                let s = i.to_string(&v).map_err(unab)?;
                normalize_tz(i, &s)?
            }
        };
        let off = zone_offset(&tz, 0);
        Ok(make(
            i,
            "Temporal.ZonedDateTime",
            Temporal::Zoned { epoch_ns: 0, offset_ns: off, tz },
        ))
    });
    it.def_method(&now, "plainDateISO", 0, |i, _t, a| {
        now_validate_zone(i, &arg(a, 0))?;
        Ok(make(
            i,
            "Temporal.PlainDate",
            Temporal::Date(IsoDate {
                year: 1970,
                month: 1,
                day: 1,
            }),
        ))
    });
    it.def_method(&now, "plainTimeISO", 0, |i, _t, a| {
        now_validate_zone(i, &arg(a, 0))?;
        Ok(make(
            i,
            "Temporal.PlainTime",
            Temporal::Time(IsoTime {
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                us: 0,
                ns: 0,
            }),
        ))
    });
    it.def_method(&now, "plainDateTimeISO", 0, |i, _t, a| {
        now_validate_zone(i, &arg(a, 0))?;
        Ok(make(
            i,
            "Temporal.PlainDateTime",
            Temporal::DateTime(
                IsoDate {
                    year: 1970,
                    month: 1,
                    day: 1,
                },
                IsoTime {
                    hour: 0,
                    minute: 0,
                    second: 0,
                    ms: 0,
                    us: 0,
                    ns: 0,
                },
            ),
        ))
    });
    ns.borrow_mut()
        .props
        .insert("Now", Property::builtin(Value::Obj(now)));
}
