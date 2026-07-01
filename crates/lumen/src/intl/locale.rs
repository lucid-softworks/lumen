//! `Intl.Locale` and the shared locale-support predicate.

use super::{ab, canonicalize_locale_list, coerce_options, def_getter, get_string_option, make_service};
use crate::interpreter::Interp;
use crate::intl::tags;
use crate::value::{set_builtin, Gc, Property, Value};

/// Whether a canonical tag is "supported" by our (minimal) data. We accept any structurally valid
/// tag whose language we have some data for, plus always `en`; locale negotiation elsewhere falls
/// back to `en` regardless.
#[allow(dead_code)]
pub fn is_supported(tag: &str) -> bool {
    // For supportedLocalesOf we report support only for locales we can actually service; today that
    // is the base languages we ship data for. Unknown tags are simply dropped from the result.
    let lang = tag.split('-').next().unwrap_or("");
    matches!(
        lang,
        "en" | "de" | "fr" | "es" | "it" | "pt" | "nl" | "ja" | "zh" | "ko" | "ru" | "ar"
    )
}

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "Locale", 1, locale_construct);

    def_getter(it, &proto, "baseName", |i, this, _| {
        Ok(Value::from_string(field(i, &this, "__locale_basename")))
    });
    def_getter(it, &proto, "language", |i, this, _| {
        Ok(Value::from_string(field(i, &this, "__locale_language")))
    });
    def_getter(it, &proto, "script", |i, this, _| {
        opt_field(i, &this, "__locale_script")
    });
    def_getter(it, &proto, "region", |i, this, _| {
        opt_field(i, &this, "__locale_region")
    });
    def_getter(it, &proto, "calendar", |i, this, _| opt_field(i, &this, "__locale_ca"));
    def_getter(it, &proto, "collation", |i, this, _| opt_field(i, &this, "__locale_co"));
    def_getter(it, &proto, "hourCycle", |i, this, _| opt_field(i, &this, "__locale_hc"));
    def_getter(it, &proto, "caseFirst", |i, this, _| opt_field(i, &this, "__locale_kf"));
    def_getter(it, &proto, "numberingSystem", |i, this, _| {
        opt_field(i, &this, "__locale_nu")
    });
    def_getter(it, &proto, "numeric", |i, this, _| {
        let o = this.as_obj().ok_or_else(|| i.make_error("TypeError", "not a Locale"))?;
        let v = o.borrow().props.get("__locale_kn").map(|p| p.value.clone());
        Ok(Value::Bool(matches!(v, Some(Value::Str(s)) if &*s == "true" || s.is_empty())))
    });

    it.def_method(&proto, "toString", 0, |i, this, _| {
        Ok(Value::from_string(field(i, &this, "__locale_tag")))
    });
    it.def_method(&proto, "maximize", 0, |_i, this, _| Ok(this));
    it.def_method(&proto, "minimize", 0, |_i, this, _| Ok(this));

    let _ = ctor;
}

fn field(i: &mut Interp, this: &Value, slot: &str) -> String {
    match this.as_obj().and_then(|o| o.borrow().props.get(slot).map(|p| p.value.clone())) {
        Some(Value::Str(s)) => s.to_string(),
        _ => {
            let _ = i;
            String::new()
        }
    }
}

fn opt_field(i: &mut Interp, this: &Value, slot: &str) -> Result<Value, Value> {
    let o = this.as_obj().ok_or_else(|| i.make_error("TypeError", "not a Locale"))?;
    match o.borrow().props.get(slot).map(|p| p.value.clone()) {
        Some(Value::Str(s)) if !s.is_empty() => Ok(Value::Str(s)),
        _ => Ok(Value::Undefined),
    }
}

fn locale_construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.Locale requires 'new'"));
    }
    let tag_arg = super::arg(a, 0);
    // The tag may be a string or another Locale.
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
    if !tags::is_structurally_valid_tag(&base) {
        return Err(i.make_error("RangeError", format!("invalid language tag: {base}")));
    }
    base = tags::canonicalize_language_tag(&base)
        .ok_or_else(|| i.make_error("RangeError", format!("invalid language tag: {base}")))?;

    let options = coerce_options(i, &super::arg(a, 1))?;

    // Options override the corresponding fields; here we apply the base-name overrides then the
    // Unicode-extension keyword overrides by rebuilding the tag.
    let language = get_string_option(i, &options, "language", &[], None)?;
    let script = get_string_option(i, &options, "script", &[], None)?;
    let region = get_string_option(i, &options, "region", &[], None)?;
    let ca = get_string_option(i, &options, "calendar", &[], None)?;
    let co = get_string_option(i, &options, "collation", &[], None)?;
    let hc = get_string_option(i, &options, "hourCycle", &["h11", "h12", "h23", "h24"], None)?;
    let kf = get_string_option(i, &options, "caseFirst", &["upper", "lower", "false"], None)?;
    let nu = get_string_option(i, &options, "numberingSystem", &[], None)?;
    let kn = {
        let v = ab(i.get_member(&options, "numeric"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };

    let mut parsed = tags::parse(&base).unwrap();
    if let Some(l) = &language {
        parsed.language = l.to_lowercase();
    }
    if let Some(s) = &script {
        parsed.script = titlecase(s);
    }
    if let Some(r) = &region {
        parsed.region = r.to_uppercase();
    }
    // Rebuild the base tag, then re-canonicalize with the Unicode-extension keywords applied.
    let base_only = {
        let mut t = parsed.clone();
        t.unicode = None;
        t.transform = None;
        t.other_ext.clear();
        t.private.clear();
        tags::render(&t)
    };

    // Merge keyword overrides into the existing -u- keywords.
    let (mut attrs, mut kws) = parsed.unicode.take().unwrap_or_default();
    let mut set_kw = |key: &str, val: Option<String>| {
        if let Some(v) = val {
            kws.retain(|(k, _)| k != key);
            let types: Vec<String> = if v.is_empty() {
                Vec::new()
            } else {
                v.split('-').map(|s| s.to_string()).collect()
            };
            kws.push((key.to_string(), types));
        }
    };
    set_kw("ca", ca.clone());
    set_kw("co", co.clone());
    set_kw("hc", hc.clone());
    set_kw("kf", kf.clone());
    set_kw("nu", nu.clone());
    if let Some(b) = kn {
        set_kw("kn", Some(if b { String::new() } else { "false".to_string() }));
    }
    attrs.sort();
    kws.sort_by(|a, b| a.0.cmp(&b.0));

    let mut full = tags::parse(&base_only).unwrap();
    if !attrs.is_empty() || !kws.is_empty() {
        full.unicode = Some((attrs, kws.clone()));
    }
    let tag = tags::canonicalize_language_tag(&tags::render(&full))
        .ok_or_else(|| i.make_error("RangeError", "invalid locale"))?;

    let obj = i.new_object();
    if let Some(proto) = i.extra_protos.get("Intl.Locale").cloned() {
        obj.borrow_mut().proto = Some(proto);
    }
    let cparsed = tags::parse(&tag).unwrap();
    set_builtin(&obj, "__locale_tag", Value::from_string(tag.clone()));
    set_builtin(&obj, "__locale_basename", Value::from_string(base_name(&cparsed)));
    set_builtin(&obj, "__locale_language", Value::from_string(cparsed.language.clone()));
    set_builtin(&obj, "__locale_script", Value::from_string(cparsed.script.clone()));
    set_builtin(&obj, "__locale_region", Value::from_string(cparsed.region.clone()));
    if let Some((_a, keywords)) = &cparsed.unicode {
        for (k, types) in keywords {
            let slot = format!("__locale_{k}");
            let val = types.join("-");
            set_builtin(&obj, Box::leak(slot.into_boxed_str()), Value::from_string(val));
        }
    }
    let _ = canonicalize_locale_list;
    Ok(Value::Obj(obj))
}

fn base_name(t: &tags::LangTag) -> String {
    let mut b = t.clone();
    b.unicode = None;
    b.transform = None;
    b.other_ext.clear();
    b.private.clear();
    tags::render(&b)
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

// Re-export the Property type usage to satisfy the unused-import lint in some builds.
#[allow(unused)]
type _P = Property;
