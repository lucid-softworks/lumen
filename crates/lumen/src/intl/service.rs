//! Shared machinery for the `Intl.*` formatting services: locale resolution, the `localeMatcher`
//! option, `supportedLocalesOf`, and small option readers.

use super::{ab, arg, canonicalize_locale_list};
use crate::interpreter::Interp;
use crate::intl::tags;
use crate::value::{Gc, Property, Value};

/// The languages we ship formatting data for. Unknown languages resolve to `en`.
pub fn supported_language(lang: &str) -> bool {
    matches!(
        lang,
        "en" | "de" | "fr" | "es" | "it" | "pt" | "nl" | "ja" | "zh" | "ko" | "ru" | "ar" | "und"
    )
}

/// The result of ResolveLocale: the chosen locale's base name plus the Unicode `-u-` keywords it
/// carried (only those relevant to the service are kept by the caller).
pub struct ResolvedLocale {
    pub locale: String,
    #[allow(dead_code)]
    pub keywords: Vec<(String, String)>,
}

/// A minimal ResolveLocale (lookup matcher): the first requested locale whose language we service
/// wins, preserving its script/region; otherwise `en`. Relevant `-u-` keywords are extracted.
pub fn resolve_locale(
    _i: &mut Interp,
    requested: &[String],
    relevant_keys: &[&str],
) -> ResolvedLocale {
    for tag in requested {
        if let Some(parsed) = tags::parse(tag) {
            if supported_language(&parsed.language) {
                // base name (language-script-region-variants), no extensions.
                let base = {
                    let mut b = parsed.clone();
                    b.unicode = None;
                    b.transform = None;
                    b.other_ext.clear();
                    b.private.clear();
                    tags::render(&b)
                };
                let mut keywords = Vec::new();
                if let Some((_a, kws)) = &parsed.unicode {
                    for (k, types) in kws {
                        if relevant_keys.contains(&k.as_str()) {
                            keywords.push((k.clone(), types.join("-")));
                        }
                    }
                }
                return ResolvedLocale {
                    locale: base,
                    keywords,
                };
            }
        }
    }
    ResolvedLocale {
        locale: "en".to_string(),
        keywords: Vec::new(),
    }
}

/// GetOption(options, "localeMatcher", string, «"lookup","best fit"», "best fit"). Validated then
/// discarded (both matchers behave identically here).
pub fn read_locale_matcher(i: &mut Interp, options: &Value) -> Result<(), Value> {
    let v = ab(i.get_member(options, "localeMatcher"))?;
    if matches!(v, Value::Undefined) {
        return Ok(());
    }
    let s = ab(i.to_string(&v))?.to_string();
    if s != "lookup" && s != "best fit" {
        return Err(i.make_error("RangeError", format!("invalid localeMatcher: {s}")));
    }
    Ok(())
}

/// GetOption string with an explicit enum + fallback.
pub fn get_option(
    i: &mut Interp,
    options: &Value,
    prop: &str,
    values: &[&str],
    fallback: Option<&str>,
) -> Result<Option<String>, Value> {
    let v = ab(i.get_member(options, prop))?;
    if matches!(v, Value::Undefined) {
        return Ok(fallback.map(|s| s.to_string()));
    }
    let s = ab(i.to_string(&v))?.to_string();
    if !values.is_empty() && !values.contains(&s.as_str()) {
        return Err(i.make_error("RangeError", format!("invalid {prop}: {s}")));
    }
    Ok(Some(s))
}

/// Install `supportedLocalesOf` on a service constructor.
pub fn install_supported_locales(it: &mut Interp, ctor: &Gc) {
    let f = it.make_native("supportedLocalesOf", 1, |i, _t, a| {
        let requested = canonicalize_locale_list(i, &arg(a, 0))?;
        // Validate the options bag's localeMatcher (per spec) then filter by supported language.
        let options = arg(a, 1);
        if !matches!(options, Value::Undefined) {
            read_locale_matcher(i, &options)?;
        }
        let out: Vec<Value> = requested
            .into_iter()
            .filter(|t| {
                tags::parse(t)
                    .map(|p| supported_language(&p.language))
                    .unwrap_or(false)
            })
            .map(Value::from_string)
            .collect();
        Ok(i.make_array(out))
    });
    ctor.borrow_mut()
        .props
        .insert("supportedLocalesOf", Property::builtin(Value::Obj(f)));
}

/// Brand-check helper: fetch the internal slot object of a service instance, or throw TypeError.
pub fn brand_slot(i: &mut Interp, this: &Value, marker: &str) -> Result<Gc, Value> {
    match this.as_obj() {
        Some(o) if o.borrow().props.contains(marker) => Ok(o.clone()),
        _ => Err(i.make_error("TypeError", "method called on an incompatible receiver")),
    }
}

/// OrdinaryCreateFromConstructor prototype: `new.target.prototype` when it is an object, else the
/// intrinsic prototype for this service.
pub fn instance_proto(i: &mut Interp, intrinsic: &str) -> Option<Gc> {
    if let Value::Obj(nt) = &i.new_target {
        if let Ok(Value::Obj(p)) = i.get_member(&Value::Obj(nt.clone()), "prototype") {
            return Some(p);
        }
    }
    i.extra_protos.get(intrinsic).cloned()
}

