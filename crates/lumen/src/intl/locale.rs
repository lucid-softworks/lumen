//! `Intl.Locale` and the shared locale-support predicate.

use super::{ab, coerce_options, def_getter, make_service};
use crate::interpreter::Interp;
use crate::intl::tags;
use crate::value::{set_builtin, set_data, Gc, Value};

/// Whether a canonical tag is serviced by our (minimal) locale data.
#[allow(dead_code)]
pub fn is_supported(tag: &str) -> bool {
    let lang = tag.split('-').next().unwrap_or("");
    matches!(
        lang,
        "en" | "de" | "fr" | "es" | "it" | "pt" | "nl" | "ja" | "zh" | "ko" | "ru" | "ar"
    )
}

// ---- subtag validators (the option grammar) --------------------------------------------------

fn is_alpha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_alnum(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn is_digit(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}
fn valid_language(s: &str) -> bool {
    ((s.len() >= 2 && s.len() <= 3) || (s.len() >= 5 && s.len() <= 8)) && is_alpha(s)
}
fn valid_script(s: &str) -> bool {
    s.len() == 4 && is_alpha(s)
}
fn valid_region(s: &str) -> bool {
    (s.len() == 2 && is_alpha(s)) || (s.len() == 3 && is_digit(s))
}
fn valid_variant_subtag(s: &str) -> bool {
    (s.len() >= 5 && s.len() <= 8 && is_alnum(s))
        || (s.len() == 4 && s.as_bytes()[0].is_ascii_digit() && is_alnum(s))
}
/// One or more "-"-joined variant subtags.
fn valid_variants(s: &str) -> bool {
    !s.is_empty() && s.split('-').all(valid_variant_subtag)
}
/// A Unicode keyword type: one or more "-"-joined 3..8 alnum subtags (used by ca/co/nu).
fn valid_type(s: &str) -> bool {
    !s.is_empty() && s.split('-').all(|p| p.len() >= 3 && p.len() <= 8 && is_alnum(p))
}

pub fn install(it: &mut Interp, ns: &Gc) {
    let (_ctor, proto) = make_service(it, ns, "Locale", 1, locale_construct);

    def_getter(it, &proto, "baseName", |i, this, _| {
        Ok(Value::from_string(slot(i, &this, "__locale_basename")?))
    });
    def_getter(it, &proto, "language", |i, this, _| {
        Ok(Value::from_string(slot(i, &this, "__locale_language")?))
    });
    def_getter(it, &proto, "script", |i, this, _| opt_slot(i, &this, "__locale_script"));
    def_getter(it, &proto, "region", |i, this, _| opt_slot(i, &this, "__locale_region"));
    def_getter(it, &proto, "variants", |i, this, _| opt_slot(i, &this, "__locale_variants"));
    def_getter(it, &proto, "calendar", |i, this, _| opt_slot(i, &this, "__locale_ca"));
    def_getter(it, &proto, "collation", |i, this, _| opt_slot(i, &this, "__locale_co"));
    def_getter(it, &proto, "hourCycle", |i, this, _| opt_slot(i, &this, "__locale_hc"));
    def_getter(it, &proto, "caseFirst", |i, this, _| opt_slot(i, &this, "__locale_kf"));
    def_getter(it, &proto, "firstDayOfWeek", |i, this, _| opt_slot(i, &this, "__locale_fw"));
    def_getter(it, &proto, "numberingSystem", |i, this, _| opt_slot(i, &this, "__locale_nu"));
    def_getter(it, &proto, "numeric", |i, this, _| {
        let o = this.as_obj().ok_or_else(|| i.make_error("TypeError", "not a Locale"))?;
        if !o.borrow().props.contains("__locale_tag") {
            return Err(i.make_error("TypeError", "receiver is not an Intl.Locale"));
        }
        let present = o.borrow().props.contains("__locale_kn");
        let is_false = matches!(
            o.borrow().props.get("__locale_kn").map(|p| p.value.clone()),
            Some(Value::Str(s)) if &*s == "false"
        );
        Ok(Value::Bool(present && !is_false))
    });

    it.def_method(&proto, "toString", 0, |i, this, _| {
        Ok(Value::from_string(slot(i, &this, "__locale_tag")?))
    });
    it.def_method(&proto, "maximize", 0, |i, this, _| relocale(i, &this, true));
    it.def_method(&proto, "minimize", 0, |i, this, _| relocale(i, &this, false));

    // Intl.Locale-info: getX() return the locale's preferred values (the keyword override, if any,
    // else a data default) as a fresh Array.
    it.def_method(&proto, "getCalendars", 0, |i, this, _| {
        info_list(i, &this, "__locale_ca", &["gregory"])
    });
    it.def_method(&proto, "getCollations", 0, |i, this, _| {
        info_list(i, &this, "__locale_co", &["default"])
    });
    it.def_method(&proto, "getHourCycles", 0, |i, this, _| {
        info_list(i, &this, "__locale_hc", &["h23"])
    });
    it.def_method(&proto, "getNumberingSystems", 0, |i, this, _| {
        info_list(i, &this, "__locale_nu", &["latn"])
    });
    it.def_method(&proto, "getTimeZones", 0, |i, this, _| {
        // Defined only for a locale with a region; otherwise `undefined`.
        let region = {
            let o = this.as_obj().ok_or_else(|| i.make_error("TypeError", "not a Locale"))?;
            if !o.borrow().props.contains("__locale_tag") {
                return Err(i.make_error("TypeError", "receiver is not an Intl.Locale"));
            }
            matches!(o.borrow().props.get("__locale_region").map(|p| p.value.clone()), Some(Value::Str(s)) if !s.is_empty())
        };
        if region {
            Ok(i.make_array(vec![Value::str("UTC")]))
        } else {
            Ok(Value::Undefined)
        }
    });
    it.def_method(&proto, "getTextInfo", 0, |i, this, _| {
        let _ = slot(i, &this, "__locale_tag")?;
        let o = i.new_object();
        set_data(&o, "direction", Value::str("ltr"));
        Ok(Value::Obj(o))
    });
    it.def_method(&proto, "getWeekInfo", 0, |i, this, _| {
        let _ = slot(i, &this, "__locale_tag")?;
        // firstDay comes from the fw keyword (option or -u-fw-), defaulting to Monday.
        let first = match opt_slot(i, &this, "__locale_fw")? {
            Value::Str(s) => fw_to_num(&s).unwrap_or(1.0),
            _ => 1.0,
        };
        let o = i.new_object();
        set_data(&o, "firstDay", Value::Num(first));
        set_data(&o, "weekend", i.make_array(vec![Value::Num(6.0), Value::Num(7.0)]));
        Ok(Value::Obj(o))
    });
}

/// The value list for a `getX` info method: `[keyword]` if the locale carries that keyword, else the
/// data default. Brand-checks the receiver.
fn info_list(i: &mut Interp, this: &Value, slot_name: &str, default: &[&str]) -> Result<Value, Value> {
    let o = this.as_obj().ok_or_else(|| i.make_error("TypeError", "not a Locale"))?;
    if !o.borrow().props.contains("__locale_tag") {
        return Err(i.make_error("TypeError", "receiver is not an Intl.Locale"));
    }
    let kw = match o.borrow().props.get(slot_name).map(|p| p.value.clone()) {
        Some(Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };
    let items: Vec<Value> = match kw {
        Some(v) => vec![Value::from_string(v)],
        None => default.iter().map(|s| Value::str(*s)).collect(),
    };
    Ok(i.make_array(items))
}

fn slot(i: &mut Interp, this: &Value, name: &str) -> Result<String, Value> {
    let o = this.as_obj().ok_or_else(|| i.make_error("TypeError", "not a Locale"))?;
    match o.borrow().props.get(name).map(|p| p.value.clone()) {
        Some(Value::Str(s)) => Ok(s.to_string()),
        _ => Err(i.make_error("TypeError", "receiver is not an Intl.Locale")),
    }
}

/// A getter whose result is the slot value (possibly `""`) when the slot exists, else `undefined`.
fn opt_slot(i: &mut Interp, this: &Value, name: &str) -> Result<Value, Value> {
    let o = this.as_obj().ok_or_else(|| i.make_error("TypeError", "not a Locale"))?;
    if !o.borrow().props.contains("__locale_tag") {
        return Err(i.make_error("TypeError", "receiver is not an Intl.Locale"));
    }
    match o.borrow().props.get(name).map(|p| p.value.clone()) {
        Some(Value::Str(s)) => Ok(Value::Str(s)),
        _ => Ok(Value::Undefined),
    }
}

fn titlecase(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == 0 {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// GetOption(options, prop, "string"): reads + ToStrings the option, or `None` if undefined.
fn get_str(i: &mut Interp, options: &Value, prop: &str) -> Result<Option<String>, Value> {
    let v = ab(i.get_member(options, prop))?;
    if matches!(v, Value::Undefined) {
        return Ok(None);
    }
    Ok(Some(ab(i.to_string(&v))?.to_string()))
}

fn locale_construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.Locale requires 'new'"));
    }
    let tag_arg = super::arg(a, 0);
    let mut base = match &tag_arg {
        Value::Str(s) => s.to_string(),
        Value::Obj(o) if o.borrow().props.contains("__locale_tag") => {
            match o.borrow().props.get("__locale_tag").map(|p| p.value.clone()) {
                Some(Value::Str(s)) => s.to_string(),
                _ => String::new(),
            }
        }
        Value::Obj(_) => ab(i.to_string(&tag_arg))?.to_string(),
        _ => return Err(i.make_error("TypeError", "locale tag must be a string or Locale")),
    };
    // Intl.Locale requires a full unicode_language_id (a language subtag); a private-use-only tag
    // such as "x-foo" has no language and is rejected.
    let has_language = tags::parse(&base).map(|p| !p.language.is_empty()).unwrap_or(false);
    if !tags::is_structurally_valid_tag(&base) || !has_language {
        return Err(i.make_error("RangeError", format!("invalid language tag: {base}")));
    }
    base = tags::canonicalize_language_tag(&base)
        .ok_or_else(|| i.make_error("RangeError", format!("invalid language tag: {base}")))?;

    let options = coerce_options(i, &super::arg(a, 1))?;

    // ---- ApplyOptionsToTag: language / script / region / variants overrides -------------------
    let mut parsed = tags::parse(&base).unwrap();

    if let Some(l) = get_str(i, &options, "language")? {
        if !valid_language(&l) {
            return Err(i.make_error("RangeError", "invalid language option"));
        }
        parsed.language = l.to_lowercase();
    }
    if let Some(s) = get_str(i, &options, "script")? {
        if !valid_script(&s) {
            return Err(i.make_error("RangeError", "invalid script option"));
        }
        parsed.script = titlecase(&s);
    }
    if let Some(r) = get_str(i, &options, "region")? {
        if !valid_region(&r) {
            return Err(i.make_error("RangeError", "invalid region option"));
        }
        parsed.region = r.to_uppercase();
    }
    if let Some(v) = get_str(i, &options, "variants")? {
        if !valid_variants(&v) {
            return Err(i.make_error("RangeError", "invalid variants option"));
        }
        parsed.variants = v.to_lowercase().split('-').map(|s| s.to_string()).collect();
    }

    // ---- keyword options (calendar/collation/hourCycle/caseFirst/numeric/numberingSystem/…) ---
    // Read in the spec's observable order: calendar, collation, firstDayOfWeek, hourCycle,
    // caseFirst, numeric, numberingSystem.
    let ca = keyword_opt(i, &options, "calendar", &[], valid_type)?;
    let co = keyword_opt(i, &options, "collation", &[], valid_type)?;
    let fw = firstday_opt(i, &options)?;
    let hc = keyword_opt(i, &options, "hourCycle", &["h11", "h12", "h23", "h24"], |_| true)?;
    let kf = keyword_opt(i, &options, "caseFirst", &["upper", "lower", "false"], |_| true)?;
    let kn = {
        let v = ab(i.get_member(&options, "numeric"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };
    let nu = keyword_opt(i, &options, "numberingSystem", &[], valid_type)?;

    // Merge keyword overrides into the -u- extension.
    let (mut attrs, mut kws) = parsed.unicode.take().unwrap_or_default();
    let mut set_kw = |key: &str, val: Option<String>| {
        if let Some(v) = val {
            kws.retain(|(k, _)| k != key);
            let types: Vec<String> = if v.is_empty() || v == "true" {
                Vec::new()
            } else {
                v.split('-').map(|s| s.to_string()).collect()
            };
            kws.push((key.to_string(), types));
        }
    };
    set_kw("ca", ca);
    set_kw("co", co);
    set_kw("fw", fw);
    set_kw("hc", hc);
    set_kw("kf", kf);
    set_kw("nu", nu);
    if let Some(b) = kn {
        set_kw("kn", Some(if b { String::new() } else { "false".to_string() }));
    }
    if !attrs.is_empty() || !kws.is_empty() {
        attrs.sort();
        kws.sort_by(|a, b| a.0.cmp(&b.0));
        parsed.unicode = Some((attrs, kws));
    } else {
        parsed.unicode = None;
    }

    let tag = tags::canonicalize_language_tag(&tags::render(&parsed))
        .ok_or_else(|| i.make_error("RangeError", "invalid locale"))?;
    build_locale_object(i, &tag)
}

/// Read a keyword string option, validating it against an explicit value set or a grammar check.
fn keyword_opt(
    i: &mut Interp,
    options: &Value,
    prop: &str,
    values: &[&str],
    valid: fn(&str) -> bool,
) -> Result<Option<String>, Value> {
    let v = ab(i.get_member(options, prop))?;
    if matches!(v, Value::Undefined) {
        return Ok(None);
    }
    let s = ab(i.to_string(&v))?.to_string();
    if !values.is_empty() {
        // Enum-valued options (hourCycle/caseFirst) are matched case-sensitively against the values.
        if !values.contains(&s.as_str()) {
            return Err(i.make_error("RangeError", format!("invalid {prop} option: {s}")));
        }
        return Ok(Some(s));
    }
    let lower = s.to_lowercase();
    if !valid(&lower) {
        return Err(i.make_error("RangeError", format!("invalid {prop} option: {s}")));
    }
    Ok(Some(lower))
}

/// The 1..7 (Mon..Sun) weekday number for an fw keyword value.
fn fw_to_num(fw: &str) -> Option<f64> {
    Some(match fw {
        "mon" => 1.0,
        "tue" => 2.0,
        "wed" => 3.0,
        "thu" => 4.0,
        "fri" => 5.0,
        "sat" => 6.0,
        "sun" => 7.0,
        _ => return None,
    })
}

/// firstDayOfWeek accepts a weekday name (mon…sun) or a number 0-7; canonicalizes to the fw type.
fn firstday_opt(i: &mut Interp, options: &Value) -> Result<Option<String>, Value> {
    let v = ab(i.get_member(options, "firstDayOfWeek"))?;
    if matches!(v, Value::Undefined) {
        return Ok(None);
    }
    let s = ab(i.to_string(&v))?.to_string().to_lowercase();
    // WeekdayToString: 1..7 (and 0) map to weekday abbreviations; any other value is kept verbatim.
    let mapped = match s.as_str() {
        "1" => "mon",
        "2" => "tue",
        "3" => "wed",
        "4" => "thu",
        "5" => "fri",
        "6" => "sat",
        "7" | "0" => "sun",
        other => other,
    }
    .to_string();
    // It must match the Unicode `type` production: one or more 3-8 char alphanumeric subtags.
    let valid = !mapped.is_empty()
        && mapped
            .split('-')
            .all(|p| (3..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()));
    if !valid {
        return Err(i.make_error("RangeError", format!("invalid firstDayOfWeek: {s}")));
    }
    // A keyword value of "true" canonicalizes to the empty value (rendered as bare "-u-fw").
    Ok(Some(if mapped == "true" { String::new() } else { mapped }))
}

fn build_locale_object(i: &mut Interp, tag: &str) -> Result<Value, Value> {
    let obj = i.new_object();
    if let Some(proto) = super::service::instance_proto(i, "Intl.Locale")? {
        obj.borrow_mut().proto = Some(proto);
    }
    let p = tags::parse(tag).unwrap();
    // baseName = language[-script][-region][-variants…] (no extensions).
    let base = {
        let mut b = p.clone();
        b.unicode = None;
        b.transform = None;
        b.other_ext.clear();
        b.private.clear();
        tags::render(&b)
    };
    set_builtin(&obj, "__locale_tag", Value::from_string(tag.to_string()));
    set_builtin(&obj, "__locale_basename", Value::from_string(base));
    set_builtin(&obj, "__locale_language", Value::from_string(p.language.clone()));
    if !p.script.is_empty() {
        set_builtin(&obj, "__locale_script", Value::from_string(p.script.clone()));
    }
    if !p.region.is_empty() {
        set_builtin(&obj, "__locale_region", Value::from_string(p.region.clone()));
    }
    if !p.variants.is_empty() {
        set_builtin(&obj, "__locale_variants", Value::from_string(p.variants.join("-")));
    }
    if let Some((_a, keywords)) = &p.unicode {
        for (k, types) in keywords {
            let slot = format!("__locale_{k}");
            set_builtin(
                &obj,
                Box::leak(slot.into_boxed_str()),
                Value::from_string(types.join("-")),
            );
        }
    }
    Ok(Value::Obj(obj))
}

/// Add Likely Subtags (UTS #35): resolve the most likely (language, script, region) for a partial
/// identifier, keeping any originally-present subtags. Returns None only if nothing matches at all.
fn add_likely(lang: &str, script: &str, region: &str) -> Option<(String, String, String)> {
    use crate::cldr_likely::likely;
    // "und" behaves as an absent language for override purposes.
    let orig_lang = if lang == "und" { "" } else { lang };
    let l = if orig_lang.is_empty() { "und" } else { orig_lang };
    let mut keys: Vec<String> = Vec::new();
    if !script.is_empty() && !region.is_empty() {
        keys.push(format!("{l}-{script}-{region}"));
    }
    if !region.is_empty() {
        keys.push(format!("{l}-{region}"));
    }
    if !script.is_empty() {
        keys.push(format!("{l}-{script}"));
    }
    keys.push(l.to_string());
    if !script.is_empty() {
        keys.push(format!("und-{script}"));
    }
    for k in &keys {
        if let Some(v) = likely(k) {
            let p: Vec<&str> = v.split('-').collect();
            let (ml, ms, mr) = (p[0], p.get(1).copied().unwrap_or(""), p.get(2).copied().unwrap_or(""));
            return Some((
                if orig_lang.is_empty() { ml } else { orig_lang }.to_string(),
                if script.is_empty() { ms } else { script }.to_string(),
                if region.is_empty() { mr } else { region }.to_string(),
            ));
        }
    }
    None
}

/// Remove Likely Subtags (UTS #35): the shortest identifier that maximizes back to the same tag.
fn remove_likely(lang: &str, script: &str, region: &str) -> (String, String, String) {
    let max = match add_likely(lang, script, region) {
        Some(m) => m,
        None => return (lang.to_string(), script.to_string(), region.to_string()),
    };
    let (ml, ms, mr) = (max.0.as_str(), max.1.as_str(), max.2.as_str());
    for cand in [(ml, "", ""), (ml, "", mr), (ml, ms, "")] {
        if add_likely(cand.0, cand.1, cand.2).as_ref() == Some(&max) {
            return (cand.0.to_string(), cand.1.to_string(), cand.2.to_string());
        }
    }
    max
}

/// Intl.Locale.prototype.maximize/minimize via the CLDR likelySubtags data. Only the core
/// (language, script, region) is transformed; variants and extensions are preserved.
fn relocale(i: &mut Interp, this: &Value, maximize: bool) -> Result<Value, Value> {
    let tag = slot(i, this, "__locale_tag")?;
    let mut t = match super::tags::parse(&tag) {
        Some(t) => t,
        None => return build_locale_object(i, &tag),
    };
    let (l, s, r) = if maximize {
        add_likely(&t.language, &t.script, &t.region).unwrap_or((
            t.language.clone(),
            t.script.clone(),
            t.region.clone(),
        ))
    } else {
        remove_likely(&t.language, &t.script, &t.region)
    };
    t.language = l;
    t.script = s;
    t.region = r;
    build_locale_object(i, &super::tags::render(&t))
}
