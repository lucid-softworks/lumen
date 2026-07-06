//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

fn now_ms() -> f64 {
    if let Some(ms) = crate::host_now_ms() {
        return ms.trunc();
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month0, day, hour, minute, second, millisecond, weekday[0=Sun]).
const WDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn year_str(y: i64) -> String {
    if y < 0 {
        format!("-{:04}", -y)
    } else {
        format!("{y:04}")
    }
}

fn date_str_part(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (y, mo, d, _, _, _, _, wd) = ms_to_parts(t);
    Some(format!(
        "{} {} {:02} {}",
        WDAYS[wd as usize],
        MONTHS[mo as usize],
        d,
        year_str(y)
    ))
}
fn time_str_part(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (_, _, _, h, mi, s, _, _) = ms_to_parts(t);
    Some(format!(
        "{h:02}:{mi:02}:{s:02} GMT+0000 (Coordinated Universal Time)"
    ))
}
fn utc_string(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (y, mo, d, h, mi, s, _, wd) = ms_to_parts(t);
    Some(format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WDAYS[wd as usize],
        d,
        MONTHS[mo as usize],
        year_str(y),
        h,
        mi,
        s
    ))
}

fn ms_to_parts(t: f64) -> (i64, i64, i64, i64, i64, i64, i64, i64) {
    let ms = t as i64;
    let days = ms.div_euclid(86_400_000);
    let mut rem = ms.rem_euclid(86_400_000);
    let milli = rem % 1000;
    rem /= 1000;
    let sec = rem % 60;
    rem /= 60;
    let min = rem % 60;
    rem /= 60;
    let hour = rem;
    let (y, m, d) = civil_from_days(days);
    let weekday = (days.rem_euclid(7) + 4) % 7; // 1970-01-01 was a Thursday (4)
    (y, m - 1, d, hour, min, sec, milli, weekday)
}

#[allow(clippy::too_many_arguments)]
/// MakeTime (spec-ordered f64 arithmetic, so precision matches the standard exactly).
fn make_time(h: f64, m: f64, s: f64, ms: f64) -> f64 {
    h.trunc() * 3_600_000.0 + m.trunc() * 60_000.0 + s.trunc() * 1000.0 + ms.trunc()
}

/// MakeDay: the day number for (year, month, date), with month overflow carried into the year.
fn make_day(y: f64, m: f64, d: f64) -> f64 {
    if !y.is_finite() || !m.is_finite() || !d.is_finite() {
        return f64::NAN;
    }
    let (y, m, d) = (y.trunc(), m.trunc(), d.trunc());
    let ym = y + (m / 12.0).floor();
    let mn = m.rem_euclid(12.0);
    // Outside this range no finite time value can exist anyway (the clip is ±8.64e15 ms).
    if !(-300_000.0..=300_000.0).contains(&ym) {
        return f64::NAN;
    }
    days_from_civil(ym as i64, mn as i64 + 1, 1) as f64 + (d - 1.0)
}

fn make_date(day: f64, time: f64) -> f64 {
    let t = day * 86_400_000.0 + time;
    if t.is_finite() {
        t
    } else {
        f64::NAN
    }
}

/// TimeClip: NaN outside ±8.64e15 ms; -0 normalizes to +0.
fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8.64e15 {
        f64::NAN
    } else {
        t.trunc() + 0.0
    }
}

fn parts_to_ms(y: i64, mo0: i64, d: i64, h: i64, mi: i64, s: i64, ml: i64) -> f64 {
    // Normalize the month into 0..11 with a year carry so e.g. month 13 rolls over.
    let y = y + mo0.div_euclid(12);
    let mo = mo0.rem_euclid(12);
    let days = days_from_civil(y, mo + 1, d);
    (days * 86_400_000 + h * 3_600_000 + mi * 60_000 + s * 1000 + ml) as f64
}

fn date_ms(i: &mut Interp, this: &Value) -> Result<f64, Value> {
    // thisTimeValue: the receiver must be a Date (carry the internal time slot), else TypeError.
    match this {
        Value::Obj(o) if o.borrow().props.contains("__date_ms") => {}
        _ => return Err(i.make_error("TypeError", "this is not a Date object")),
    }
    Ok(match ab(i.get_member(this, "__date_ms"))? {
        Value::Num(n) => n,
        _ => f64::NAN,
    })
}

fn date_get(i: &mut Interp, this: &Value, sel: u8) -> Result<Value, Value> {
    let t = date_ms(i, this)?;
    if t.is_nan() {
        return Ok(Value::Num(f64::NAN));
    }
    let (y, mo, d, h, mi, s, ml, wd) = ms_to_parts(t);
    let v = match sel {
        0 => y,
        1 => mo,
        2 => d,
        3 => wd,
        4 => h,
        5 => mi,
        6 => s,
        _ => ml,
    };
    Ok(Value::Num(v as f64))
}

/// A multi-argument Date setter (setHours, setFullYear, …). Per spec, ALL provided arguments are
/// ToNumber-coerced first, in order, before any field is applied; `start_sel` is the field of the
/// first argument (the field order skips the unused selector 3). `setFullYear`/`setUTCFullYear`
/// (start 0) treat a NaN receiver as the epoch; the time setters leave it NaN.
fn date_set_multi(
    i: &mut Interp,
    this: &Value,
    start_sel: u8,
    args: &[Value],
    n_max: usize,
) -> Result<Value, Value> {
    const ORDER: [u8; 7] = [0, 1, 2, 4, 5, 6, 7];
    let start_idx = ORDER.iter().position(|&f| f == start_sel).unwrap();
    let count = args.len().clamp(1, n_max);
    // thisTimeValue validation precedes argument coercion (a non-Date receiver throws before any
    // argument's valueOf runs); then coerce every read argument up front, in order.
    let t = date_ms(i, this)?;
    let mut vals = Vec::with_capacity(count);
    for k in 0..count {
        vals.push(ab(i.to_number(&arg(args, k)))?);
    }
    let nan_to_zero = start_sel == 0;
    // A NaN stored time (for setters that don't zero it) yields NaN and leaves [[DateValue]]
    // untouched — so an argument's valueOf side-effect on the receiver persists.
    if t.is_nan() && !nan_to_zero {
        return Ok(Value::Num(f64::NAN));
    }
    let mut any_nan = t.is_nan() && !nan_to_zero;
    let base = if t.is_nan() { 0.0 } else { t };
    let (py, pmo, pd, ph, pmi, ps, pml, _) = ms_to_parts(base);
    // Fields default to their current value, held as f64 so an out-of-range assignment overflows
    // to a non-finite MakeDay/MakeTime intermediate (NaN) rather than wrapping an i64.
    let mut f = [
        py as f64, pmo as f64, pd as f64, ph as f64, pmi as f64, ps as f64, pml as f64,
    ];
    for (k, &v) in vals.iter().enumerate() {
        if !v.is_finite() {
            any_nan = true;
        }
        let slot = match ORDER[start_idx + k] {
            0 => 0,
            1 => 1,
            2 => 2,
            4 => 3,
            5 => 4,
            6 => 5,
            _ => 6,
        };
        f[slot] = v.trunc();
    }
    let day = make_day(f[0], f[1], f[2]);
    let time = f[3] * 3_600_000.0 + f[4] * 60_000.0 + f[5] * 1000.0 + f[6];
    let ms = if any_nan {
        f64::NAN
    } else {
        time_clip(make_date(day, time))
    };
    if let Value::Obj(o) = this {
        set_internal(o, "__date_ms", Value::Num(ms));
    }
    Ok(Value::Num(ms))
}

/// Minimal ISO-8601 parser: `YYYY[-MM[-DD]][THH:mm[:ss[.sss]]][Z]`. Returns NaN on anything else.
/// Best-effort parse of the RFC-2822-ish / `toString`/`toUTCString`/`toDateString` formats (e.g.
/// "Thu, 01 Jan 1970 00:00:00 GMT", "Wed Jul 28 1993 14:39:07 GMT-0600 (…)").
fn parse_rfc(s: &str) -> f64 {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let (mut year, mut month, mut day): (Option<i64>, Option<i64>, Option<i64>) =
        (None, None, None);
    let (mut hh, mut mm, mut ss) = (0i64, 0i64, 0i64);
    let mut offset: i64 = 0; // minutes east of UTC
    let mut got_time = false;
    for tok in s.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')')) {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let low = tok.to_lowercase();
        if let Some(idx) = MONTHS.iter().position(|m| low.starts_with(m)) {
            month = Some(idx as i64);
        } else if tok.contains(':') && !got_time {
            let mut p = tok.split(':');
            hh = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            mm = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            ss = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            got_time = true;
        } else if let Ok(n) = tok.parse::<i64>() {
            if tok.len() >= 4 || n > 31 {
                year = Some(n);
            } else if day.is_none() {
                day = Some(n);
            } else if year.is_none() {
                year = Some(n);
            }
        } else if low.starts_with("gmt") || low.starts_with('+') || low.starts_with('-') {
            let rest = low.trim_start_matches("gmt");
            let sign = rest.chars().next();
            let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 4 {
                let oh: i64 = digits[..2].parse().unwrap_or(0);
                let om: i64 = digits[2..4].parse().unwrap_or(0);
                let mag = oh * 60 + om;
                offset = if sign == Some('-') { -mag } else { mag };
            }
        }
    }
    match (year, month, day) {
        (Some(y), Some(mo), Some(d)) => {
            parts_to_ms(y, mo, d, hh, mm, ss, 0) - (offset as f64) * 60000.0
        }
        _ => f64::NAN,
    }
}

fn parse_iso(s: &str) -> f64 {
    let s = s.trim();
    // The 'T' separator demands strict ISO component widths; the web-reality space separator
    // (SpiderMonkey's NOTE-datetime extension) is lenient (1-2 digit fields, `±hh` offsets).
    let (date_part, time_part, strict) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t), true),
        None => match s.split_once(' ') {
            Some((d, t)) => (d, Some(t), false),
            None => (s, None, true),
        },
    };
    // Extended years carry an explicit sign and six digits (`+275760-…`); "-000000" is rejected.
    let (neg_year, signed_year, date_part) = match date_part.strip_prefix('-') {
        Some(r) => (true, true, r),
        None => match date_part.strip_prefix('+') {
            Some(r) => (false, true, r),
            None => (false, false, date_part),
        },
    };
    let mut dp = date_part.splitn(3, '-');
    let ystr = dp.next().unwrap_or("");
    if !ystr.bytes().all(|b| b.is_ascii_digit()) || ystr.is_empty() {
        return f64::NAN;
    }
    if strict {
        let want = if signed_year { 6 } else { 4 };
        if ystr.len() != want {
            return f64::NAN;
        }
    }
    if neg_year && ystr.bytes().all(|b| b == b'0') {
        return f64::NAN;
    }
    let y: i64 = match ystr.parse::<i64>() {
        Ok(v) => {
            if neg_year {
                -v
            } else {
                v
            }
        }
        Err(_) => return f64::NAN,
    };
    let field = |txt: Option<&str>, strict: bool| -> Option<i64> {
        let t = txt?;
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if strict && t.len() != 2 {
            return None;
        }
        if !strict && t.len() > 2 {
            return None;
        }
        t.parse().ok()
    };
    let (mo_txt, d_txt) = (dp.next(), dp.next());
    let mo = match mo_txt {
        None => 1,
        some => match field(some, strict) {
            Some(v) => v,
            None => return f64::NAN,
        },
    };
    let d = match d_txt {
        None => 1,
        some => match field(some, strict) {
            Some(v) => v,
            None => return f64::NAN,
        },
    };
    let (mut h, mut mi, mut sec, mut ml) = (0i64, 0i64, 0i64, 0i64);
    let mut offset_min: Option<i64> = None;
    if let Some(tp) = time_part {
        if tp.is_empty() {
            return f64::NAN;
        }
        // Split a trailing zone: 'Z', or ±hh[:mm] / ±hhmm (strict requires ±hh:mm).
        let (hms_frac, zone) = if let Some(rest) = tp.strip_suffix('Z') {
            (rest, Some(0i64))
        } else if let Some(pos) = tp.rfind(['+', '-']) {
            let (a, z) = tp.split_at(pos);
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let z = &z[1..];
            let (zh, zm) = if let Some((zh, zm)) = z.split_once(':') {
                (zh, zm)
            } else if z.len() == 4 {
                if strict {
                    return f64::NAN;
                }
                z.split_at(2)
            } else if z.len() == 2 {
                if strict {
                    return f64::NAN;
                }
                (z, "")
            } else {
                return f64::NAN;
            };
            if zh.len() != 2
                || !(zm.is_empty() || zm.len() == 2)
                || !zh.bytes().all(|b| b.is_ascii_digit())
                || !zm.bytes().all(|b| b.is_ascii_digit())
            {
                return f64::NAN;
            }
            let mins = zh.parse::<i64>().unwrap_or(0) * 60 + zm.parse::<i64>().unwrap_or(0);
            (a, Some(sign * mins))
        } else {
            (tp, None)
        };
        let (hms, frac) = match hms_frac.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (hms_frac, None),
        };
        let mut parts = hms.splitn(3, ':');
        h = match field(parts.next(), strict) {
            Some(v) => v,
            None => return f64::NAN,
        };
        mi = match parts.next() {
            // A lone hour ("T11" / "1997-03-08 11") parses as HH:00 in both forms.
            None => 0,
            some => match field(some, strict) {
                Some(v) => v,
                None => return f64::NAN,
            },
        };
        sec = match parts.next() {
            None => 0,
            some => match field(some, strict) {
                Some(v) => v,
                None => return f64::NAN,
            },
        };
        if let Some(f) = frac {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return f64::NAN;
            }
            let f3: String = f
                .chars()
                .take(3)
                .chain(std::iter::repeat('0'))
                .take(3)
                .collect();
            ml = f3.parse().unwrap_or(0);
        }
        offset_min = zone;
    }
    let base = parts_to_ms(y, mo - 1, d, h, mi, sec, ml);
    let adjusted = match offset_min {
        Some(mins) => base - (mins as f64) * 60_000.0,
        None => base,
    };
    time_clip(adjusted)
}

fn date_to_string(t: f64) -> String {
    match (date_str_part(t), time_str_part(t)) {
        (Some(d), Some(tm)) => format!("{d} {tm}"),
        _ => "Invalid Date".to_string(),
    }
}

fn date_ctor(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    // Called as a function (no `new`), Date ignores its arguments and returns the
    // current time as a string.
    if !matches!(i.new_target, Value::Obj(_)) {
        return Ok(Value::from_string(date_to_string(now_ms())));
    }
    let ms = match args.len() {
        0 => now_ms(),
        1 => match &args[0] {
            // A Date argument clones its time value directly (no valueOf call).
            Value::Obj(o) if o.borrow().props.contains("__date_ms") => {
                match o.borrow().props.get("__date_ms").map(|p| p.value.clone()) {
                    Some(Value::Num(n)) => n,
                    _ => f64::NAN,
                }
            }
            v => {
                let prim = ab(i.to_primitive(v, crate::eval::Hint::Default))?;
                match prim {
                    Value::Str(s) => parse_iso(&s),
                    p => time_clip(ab(i.to_number(&p))?),
                }
            }
        },
        _ => {
            // All components coerce as f64 (NaN propagates through MakeDay/MakeTime).
            let mut y = ab(i.to_number(&args[0]))?;
            if !y.is_nan() && (0.0..=99.0).contains(&y.trunc()) {
                y = 1900.0 + y.trunc();
            }
            let read = |i: &mut Interp, k: usize, dflt: f64| -> Result<f64, Value> {
                if args.len() > k {
                    ab(i.to_number(&args[k]))
                } else {
                    Ok(dflt)
                }
            };
            let mo = read(i, 1, 0.0)?;
            let d = read(i, 2, 1.0)?;
            let h = read(i, 3, 0.0)?;
            let mi = read(i, 4, 0.0)?;
            let sec = read(i, 5, 0.0)?;
            let ml = read(i, 6, 0.0)?;
            time_clip(make_date(make_day(y, mo, d), make_time(h, mi, sec, ml)))
        }
    };
    let obj = new_from_ctor(i, "Date")?;
    set_internal(&obj, "__date_ms", Value::Num(ms));
    Ok(Value::Obj(obj))
}

fn iso_string(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (y, mo, d, h, mi, s, ml, _) = ms_to_parts(t);
    // Years outside 0..=9999 use the signed six-digit extended form.
    let ys = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    Some(format!(
        "{ys}-{:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ml:03}Z",
        mo + 1
    ))
}

pub(super) fn install_date(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Date", proto.clone());

    it.def_method(&proto, "getTime", 0, |i, this, _| {
        Ok(Value::Num(date_ms(i, &this)?))
    });
    it.def_method(&proto, "valueOf", 0, |i, this, _| {
        Ok(Value::Num(date_ms(i, &this)?))
    });
    it.def_method(&proto, "setTime", 1, |i, this, a| {
        date_ms(i, &this)?; // thisTimeValue brand check
        let v = time_clip(ab(i.to_number(&arg(a, 0)))?);
        if let Value::Obj(o) = &this {
            set_internal(o, "__date_ms", Value::Num(v));
        }
        Ok(Value::Num(v))
    });
    it.def_method(&proto, "getTimezoneOffset", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::Num(if t.is_nan() { f64::NAN } else { 0.0 }))
    });
    it.def_method(&proto, "toTemporalInstant", 0, |i, this, _| {
        // RequireInternalSlot([[DateValue]]) then a Temporal.Instant at ms×10^6 ns.
        let ms = date_ms(i, &this)?;
        crate::temporal::instant_from_epoch_ms(i, ms)
    });
    // Local and UTC accessors are identical (offset 0).
    for (name, sel) in [
        ("getFullYear", 0u8),
        ("getMonth", 1),
        ("getDate", 2),
        ("getDay", 3),
        ("getHours", 4),
        ("getMinutes", 5),
        ("getSeconds", 6),
        ("getMilliseconds", 7),
    ] {
        let utc = format!("getUTC{}", &name[3..]);
        match sel {
            0 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 0));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 0));
            }
            1 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 1));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 1));
            }
            2 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 2));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 2));
            }
            3 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 3));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 3));
            }
            4 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 4));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 4));
            }
            5 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 5));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 5));
            }
            6 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 6));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 6));
            }
            _ => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 7));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 7));
            }
        }
    }
    it.def_method(&proto, "setFullYear", 3, |i, this, a| {
        date_set_multi(i, &this, 0, a, 3)
    });
    // Annex B legacy getYear/setYear (years offset from 1900).
    it.def_method(&proto, "getYear", 0, |i, this, _| {
        let f = ab(i.get_member(&this, "getFullYear"))?;
        let yv = ab(i.call(f, this.clone(), &[]))?;
        let y = ab(i.to_number(&yv))?;
        Ok(Value::Num(if y.is_nan() { f64::NAN } else { y - 1900.0 }))
    });
    it.def_method(&proto, "setYear", 1, |i, this, a| {
        let y = ab(i.to_number(&arg(a, 0)))?;
        let full = if y.is_nan() {
            f64::NAN
        } else {
            let yi = y.trunc() as i64;
            if (0..=99).contains(&yi) {
                1900.0 + yi as f64
            } else {
                y
            }
        };
        date_set_multi(i, &this, 0, &[Value::Num(full)], 1)?;
        // TimeClip: setYear reports the stored (possibly NaN) time value.
        date_ms(i, &this).map(Value::Num)
    });
    it.def_method(&proto, "setMonth", 2, |i, this, a| {
        date_set_multi(i, &this, 1, a, 2)
    });
    it.def_method(&proto, "setDate", 1, |i, this, a| {
        date_set_multi(i, &this, 2, a, 1)
    });
    it.def_method(&proto, "setHours", 4, |i, this, a| {
        date_set_multi(i, &this, 4, a, 4)
    });
    it.def_method(&proto, "setMinutes", 3, |i, this, a| {
        date_set_multi(i, &this, 5, a, 3)
    });
    it.def_method(&proto, "setSeconds", 2, |i, this, a| {
        date_set_multi(i, &this, 6, a, 2)
    });
    it.def_method(&proto, "setMilliseconds", 1, |i, this, a| {
        date_set_multi(i, &this, 7, a, 1)
    });
    // UTC setters mirror the local ones (offset 0).
    it.def_method(&proto, "setUTCFullYear", 3, |i, this, a| {
        date_set_multi(i, &this, 0, a, 3)
    });
    it.def_method(&proto, "setUTCMonth", 2, |i, this, a| {
        date_set_multi(i, &this, 1, a, 2)
    });
    it.def_method(&proto, "setUTCDate", 1, |i, this, a| {
        date_set_multi(i, &this, 2, a, 1)
    });
    it.def_method(&proto, "setUTCHours", 4, |i, this, a| {
        date_set_multi(i, &this, 4, a, 4)
    });
    it.def_method(&proto, "setUTCMinutes", 3, |i, this, a| {
        date_set_multi(i, &this, 5, a, 3)
    });
    it.def_method(&proto, "setUTCSeconds", 2, |i, this, a| {
        date_set_multi(i, &this, 6, a, 2)
    });
    it.def_method(&proto, "setUTCMilliseconds", 1, |i, this, a| {
        date_set_multi(i, &this, 7, a, 1)
    });
    it.def_method(&proto, "toISOString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        match iso_string(t) {
            Some(s) => Ok(Value::from_string(s)),
            None => Err(i.make_error("RangeError", "Invalid time value")),
        }
    });
    it.def_method(&proto, "toJSON", 1, |i, this, _| {
        // Generic: ToObject, ToPrimitive(number); a non-finite time is null; else Invoke toISOString.
        let o = to_object_arg(i, this.clone(), "Date.prototype.toJSON")?;
        let ov = Value::Obj(o);
        let tv = ab(i.to_primitive(&ov, crate::eval::Hint::Number))?;
        if let Value::Num(n) = &tv {
            if !n.is_finite() {
                return Ok(Value::Null);
            }
        }
        let iso = ab(i.get_member(&ov, "toISOString"))?;
        if !iso.is_callable() {
            return Err(i.make_error("TypeError", "toISOString is not callable"));
        }
        ab(i.call(iso, ov, &[]))
    });
    it.def_method(&proto, "toString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(date_to_string(t)))
    });
    it.def_method(&proto, "toDateString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            date_str_part(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toTimeString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            time_str_part(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toUTCString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            utc_string(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    // toGMTString IS toUTCString (the very same function object).
    let utc = proto.borrow().props.get("toUTCString").cloned();
    if let Some(p) = utc {
        proto.borrow_mut().props.insert("toGMTString", p);
    }
    // toLocale* route through Intl.DateTimeFormat (which exists now).
    it.def_method(&proto, "toLocaleString", 0, |i, this, args| {
        let t = date_ms(i, &this)?;
        if !t.is_finite() {
            return Ok(Value::str("Invalid Date"));
        }
        // ToDateTimeOptions(options, "any", "all"): default to date AND time unless the caller
        // already requested a date or time component (or a dateStyle/timeStyle).
        let opts = date_all_default(i, &arg(args, 1))?;
        intl_delegate(
            i,
            "DateTimeFormat",
            arg(args, 0),
            opts,
            "format",
            &[Value::Num(t)],
        )
    });
    it.def_method(&proto, "toLocaleDateString", 0, |i, this, args| {
        let t = date_ms(i, &this)?;
        if !t.is_finite() {
            return Ok(Value::str("Invalid Date"));
        }
        let opts = date_style_default(i, &arg(args, 1), true)?;
        intl_delegate(
            i,
            "DateTimeFormat",
            arg(args, 0),
            opts,
            "format",
            &[Value::Num(t)],
        )
    });
    it.def_method(&proto, "toLocaleTimeString", 0, |i, this, args| {
        let t = date_ms(i, &this)?;
        if !t.is_finite() {
            return Ok(Value::str("Invalid Date"));
        }
        let opts = date_style_default(i, &arg(args, 1), false)?;
        intl_delegate(
            i,
            "DateTimeFormat",
            arg(args, 0),
            opts,
            "format",
            &[Value::Num(t)],
        )
    });

    let ctor = it.make_native("Date", 7, date_ctor);
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "now", 0, |_i, _t, _a| Ok(Value::Num(now_ms())));
    it.def_method(&ctor, "parse", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        let v = parse_iso(&s);
        Ok(Value::Num(if v.is_nan() { parse_rfc(&s) } else { v }))
    });
    it.def_method(&ctor, "UTC", 7, |i, _t, a| {
        // The year is always read; later components only if supplied. Coerce all reads first; any
        // non-finite component makes the whole result NaN.
        let count = a.len().clamp(1, 7);
        let defaults = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let mut vals = defaults;
        let mut nan = false;
        for k in 0..count {
            let v = ab(i.to_number(&arg(a, k)))?;
            if !v.is_finite() {
                nan = true;
            }
            vals[k] = v;
        }
        let _ = nan;
        let mut y = vals[0];
        if !y.is_nan() && (0.0..=99.0).contains(&y.trunc()) {
            y = 1900.0 + y.trunc();
        }
        Ok(Value::Num(time_clip(make_date(
            make_day(y, vals[1], vals[2]),
            make_time(vals[3], vals[4], vals[5], vals[6]),
        ))))
    });
    // Date.prototype[@@toPrimitive]: "number" hint uses valueOf, "string"/"default" use toString.
    let prim = it.make_native("[Symbol.toPrimitive]", 1, |i, this, a| {
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error(
                "TypeError",
                "Date.prototype[Symbol.toPrimitive] on non-object",
            ));
        }
        // The hint must be a primitive String; OrdinaryToPrimitive then tries the two methods in
        // order, returning the first non-object result.
        let hint = match arg(a, 0) {
            Value::Str(s) => s.to_string(),
            _ => return Err(i.make_error("TypeError", "invalid Symbol.toPrimitive hint")),
        };
        let methods: &[&str] = match hint.as_str() {
            "number" => &["valueOf", "toString"],
            "string" | "default" => &["toString", "valueOf"],
            _ => return Err(i.make_error("TypeError", "invalid Symbol.toPrimitive hint")),
        };
        for m in methods {
            let f = ab(i.get_member(&this, m))?;
            if f.is_callable() {
                let r = ab(i.call(f, this.clone(), &[]))?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        Err(i.make_error("TypeError", "cannot convert Date to a primitive value"))
    });
    if let Some(key) = well_known_key(it, "toPrimitive") {
        // @@toPrimitive is non-writable (unlike ordinary builtin methods).
        proto
            .borrow_mut()
            .props
            .insert(key, Property::data(Value::Obj(prim), false, false, true));
    }
    set_builtin(&it.global, "Date", Value::Obj(ctor));
}
