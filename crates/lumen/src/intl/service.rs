//! Shared machinery for the `Intl.*` formatting services: locale resolution, the `localeMatcher`
//! option, `supportedLocalesOf`, and small option readers.

use super::{ab, arg, canonicalize_locale_list};
use crate::interpreter::Interp;
use crate::intl::tags;
use crate::value::{Gc, Property, Value};

/// The languages we ship formatting data for (plus common ones the conformance tests negotiate).
/// Unknown languages resolve to `en`.
pub fn supported_language(lang: &str) -> bool {
    matches!(
        lang,
        "en" | "de"
            | "fr"
            | "es"
            | "it"
            | "pt"
            | "nl"
            | "ja"
            | "zh"
            | "ko"
            | "ru"
            | "ar"
            | "sr"
            | "th"
            | "gv"
            | "sl"
            | "pl"
            | "si"
            | "ln"
            | "sv"
            | "hi"
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

/// Whether `n` is a numbering system this engine can render with.
pub fn is_supported_nu(n: &str) -> bool {
    n == "latn" || crate::numbering::NUMBERING.iter().any(|(id, _)| *id == n)
}

/// ResolveLocale specialised to the single `nu` key: returns the resolved locale string (the base
/// name plus a `-u-nu-<value>` addition when it survives) and the chosen numbering system. The
/// `-u-nu-` addition is kept only when the value came from the requested locale's extension; an
/// options value that differs from it drops the addition (per ResolveLocale steps for `nu`).
pub fn resolve_locale_nu(requested: &[String], option: Option<&str>) -> (String, String) {
    let mut base = "en".to_string();
    let mut ext_value: Option<String> = None;
    for tag in requested {
        if let Some(parsed) = tags::parse(tag) {
            if supported_language(&parsed.language) {
                let mut b = parsed.clone();
                b.unicode = None;
                b.transform = None;
                b.other_ext.clear();
                b.private.clear();
                base = tags::render(&b);
                if let Some((_a, kws)) = &parsed.unicode {
                    for (k, types) in kws {
                        if k == "nu" {
                            ext_value = Some(types.join("-"));
                        }
                    }
                }
                break;
            }
        }
    }
    // The locale's default numbering system (most are latn; Arabic uses arab, Persian arabext).
    let default_nu = {
        let lang = base.split('-').next().unwrap_or("");
        let region = base
            .split('-')
            .find(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_uppercase()))
            .unwrap_or("");
        match lang {
            "ar" if !matches!(region, "DZ" | "MA" | "TN" | "LY" | "EH" | "MR") => "arab",
            "fa" | "ps" => "arabext",
            _ => "latn",
        }
    };
    let mut value = default_nu.to_string();
    let mut addition: Option<String> = None;
    if let Some(ev) = &ext_value {
        if is_supported_nu(ev) {
            value = ev.clone();
            addition = Some(ev.clone());
        }
    }
    if let Some(opt) = option {
        if is_supported_nu(opt) && opt != value {
            value = opt.to_string();
            addition = None;
        }
    }
    let locale = match addition {
        Some(a) => format!("{base}-u-nu-{a}"),
        None => base,
    };
    (locale, value)
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
/// OrdinaryCreateFromConstructor's proto lookup: `Get(newTarget, "prototype")` (propagating a
/// poisoned getter's error), falling back to the intrinsic prototype when it isn't an object.
pub fn instance_proto(i: &mut Interp, intrinsic: &str) -> Result<Option<Gc>, Value> {
    if let Value::Obj(nt) = &i.new_target {
        let nt = nt.clone();
        match i.get_member(&Value::Obj(nt.clone()), "prototype") {
            Ok(Value::Obj(p)) => return Ok(Some(p)),
            // GetPrototypeFromConstructor: when newTarget.prototype is not an object, use the
            // *newTarget's realm* intrinsic (found by matching its prototype chain's Function.prototype
            // against each realm), not necessarily the current realm's.
            Ok(_) => {
                // GetFunctionRealm throws for a proxy revoked by its own prototype getter.
                ab(i.get_function_realm_global(&nt))?;
                if let Some(protos) = realm_protos_of(i, &nt) {
                    return Ok(protos.get(intrinsic).cloned());
                }
            }
            Err(a) => return Err(ab::<()>(Err(a)).unwrap_err()),
        }
    }
    Ok(i.extra_protos.get(intrinsic).cloned())
}

/// The `extra_protos` map of the realm that owns `func`, located by walking `func`'s prototype chain
/// and matching a realm's `Function.prototype`. Returns None (→ current realm) if no match.
fn realm_protos_of<'a>(
    i: &'a Interp,
    func: &Gc,
) -> Option<&'a std::collections::HashMap<&'static str, Gc>> {
    let mut cur = func.borrow().proto.clone();
    while let Some(p) = cur {
        if std::rc::Rc::ptr_eq(&p, &i.function_proto) {
            return Some(&i.extra_protos);
        }
        for rs in i.realms.values() {
            if std::rc::Rc::ptr_eq(&p, &rs.function_proto) {
                return Some(&rs.extra_protos);
            }
        }
        cur = p.borrow().proto.clone();
    }
    None
}
