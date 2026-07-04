//! `Intl.DisplayNames` (English data subset).

use super::service::{
    brand_slot, get_option, install_supported_locales, read_locale_matcher, resolve_locale,
};
use super::{
    ab, arg, canonicalize_locale_list, get_options_object as coerce_options, make_service,
};
use crate::interpreter::Interp;
use crate::intl::tags;
use crate::value::{set_builtin, set_data, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "DisplayNames", 2, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "of", 1, |i, this, a| of(i, &this, &arg(a, 0)));
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.DisplayNames requires 'new'"));
    }
    // OrdinaryCreateFromConstructor first: a poisoned newTarget.prototype getter fires before
    // any locale/options validation.
    let obj = crate::builtins::new_from_ctor(i, "Intl.DisplayNames")?;
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    // options is required and must be an object.
    let opt_arg = arg(a, 1);
    if matches!(opt_arg, Value::Undefined) {
        return Err(i.make_error("TypeError", "options is required for Intl.DisplayNames"));
    }
    let options = coerce_options(i, &opt_arg)?;
    read_locale_matcher(i, &options)?;
    let style = get_option(
        i,
        &options,
        "style",
        &["narrow", "short", "long"],
        Some("long"),
    )?
    .unwrap();
    let kind = get_option(
        i,
        &options,
        "type",
        &[
            "language",
            "region",
            "script",
            "currency",
            "calendar",
            "dateTimeField",
        ],
        None,
    )?;
    let kind = kind.ok_or_else(|| i.make_error("TypeError", "type option is required"))?;
    let fallback = get_option(i, &options, "fallback", &["code", "none"], Some("code"))?.unwrap();
    let language_display = get_option(
        i,
        &options,
        "languageDisplay",
        &["dialect", "standard"],
        Some("dialect"),
    )?
    .unwrap();
    let resolved = resolve_locale(i, &requested, &[]);

    set_builtin(&obj, "__dn", Value::Bool(true));
    set_builtin(&obj, "__dn_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__dn_style", Value::from_string(style));
    set_builtin(&obj, "__dn_type", Value::from_string(kind));
    set_builtin(&obj, "__dn_fallback", Value::from_string(fallback));
    set_builtin(
        &obj,
        "__dn_langdisplay",
        Value::from_string(language_display),
    );
    Ok(Value::Obj(obj))
}

fn of(i: &mut Interp, this: &Value, code: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__dn")?;
    let kind = match o.borrow().props.get("__dn_type").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    };
    let fallback = match o
        .borrow()
        .props
        .get("__dn_fallback")
        .map(|p| p.value.clone())
    {
        Some(Value::Str(s)) => s.to_string(),
        _ => "code".to_string(),
    };
    let s = ab(i.to_string(code))?.to_string();
    // Validate the code per type.
    let canonical = match kind.as_str() {
        "language" => {
            // DisplayNames `language` requires a `unicode_language_id` (language[-script][-region]
            // [-variants]) — a tag carrying extensions or singleton subtags is a RangeError.
            let is_language_id = tags::parse(&s)
                .map(|t| {
                    t.unicode.is_none()
                        && t.transform.is_none()
                        && t.other_ext.is_empty()
                        && t.private.is_empty()
                })
                .unwrap_or(false);
            if !is_language_id {
                return Err(i.make_error("RangeError", format!("invalid language code: {s}")));
            }
            tags::canonicalize_language_tag(&s).unwrap_or(s.clone())
        }
        "region" => {
            if !(s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic())
                || s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
            {
                return Err(i.make_error("RangeError", format!("invalid region code: {s}")));
            }
            s.to_uppercase()
        }
        "script" => {
            if s.len() != 4 || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(i.make_error("RangeError", format!("invalid script code: {s}")));
            }
            let mut c = s.to_lowercase();
            c[..1].make_ascii_uppercase();
            c
        }
        "currency" => {
            if s.len() != 3 || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(i.make_error("RangeError", format!("invalid currency code: {s}")));
            }
            s.to_uppercase()
        }
        "calendar" => {
            let ok = !s.is_empty()
                && s.split('-').all(|p| {
                    p.len() >= 3 && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric())
                });
            if !ok {
                return Err(i.make_error("RangeError", format!("invalid calendar code: {s}")));
            }
            let lc = s.to_lowercase();
            crate::intl::tags::canonical_ca(&lc).unwrap_or(lc)
        }
        "dateTimeField" => {
            const FIELDS: &[&str] = &[
                "era",
                "year",
                "quarter",
                "month",
                "weekOfYear",
                "weekday",
                "day",
                "dayPeriod",
                "hour",
                "minute",
                "second",
                "timeZoneName",
            ];
            if !FIELDS.contains(&s.as_str()) {
                return Err(i.make_error("RangeError", format!("invalid dateTimeField: {s}")));
            }
            s.clone()
        }
        _ => s.clone(),
    };
    let name = display_name(&kind, &canonical);
    match name {
        Some(n) => Ok(Value::str(n)),
        None => {
            if fallback == "code" {
                Ok(Value::from_string(canonical))
            } else {
                Ok(Value::Undefined)
            }
        }
    }
}

fn display_name(kind: &str, code: &str) -> Option<&'static str> {
    match kind {
        "language" => Some(match code {
            "en" => "English",
            "en-GB" => "British English",
            "en-US" => "American English",
            "de" => "German",
            "fr" => "French",
            "es" => "Spanish",
            "it" => "Italian",
            "pt" => "Portuguese",
            "nl" => "Dutch",
            "ja" => "Japanese",
            "zh" => "Chinese",
            "ko" => "Korean",
            "ru" => "Russian",
            "ar" => "Arabic",
            _ => return None,
        }),
        "region" => Some(match code {
            "US" => "United States",
            "GB" => "United Kingdom",
            "DE" => "Germany",
            "FR" => "France",
            "ES" => "Spain",
            "IT" => "Italy",
            "PT" => "Portugal",
            "NL" => "Netherlands",
            "JP" => "Japan",
            "CN" => "China",
            "KR" => "South Korea",
            "RU" => "Russia",
            "BR" => "Brazil",
            "419" => "Latin America",
            _ => return None,
        }),
        "script" => Some(match code {
            "Latn" => "Latin",
            "Cyrl" => "Cyrillic",
            "Hans" => "Simplified Han",
            "Hant" => "Traditional Han",
            "Arab" => "Arabic",
            "Jpan" => "Japanese",
            "Kore" => "Korean",
            _ => return None,
        }),
        "calendar" => Some(match code {
            "buddhist" => "Buddhist Calendar",
            "chinese" => "Chinese Calendar",
            "coptic" => "Coptic Calendar",
            "dangi" => "Dangi Calendar",
            "ethioaa" => "Ethiopic Amete Alem Calendar",
            "ethiopic" => "Ethiopic Calendar",
            "gregory" => "Gregorian Calendar",
            "hebrew" => "Hebrew Calendar",
            "indian" => "Indian National Calendar",
            "islamic" => "Islamic Calendar",
            "islamic-civil" => "Islamic Calendar (tabular, civil epoch)",
            "islamic-rgsa" => "Islamic Calendar (Saudi Arabia, sighting)",
            "islamic-tbla" => "Islamic Calendar (tabular, astronomical epoch)",
            "islamic-umalqura" => "Islamic Calendar (Umm al-Qura)",
            "iso8601" => "ISO-8601 Calendar",
            "japanese" => "Japanese Calendar",
            "persian" => "Persian Calendar",
            "roc" => "Minguo Calendar",
            _ => return None,
        }),
        "currency" => Some(match code {
            "USD" => "US Dollar",
            "EUR" => "Euro",
            "GBP" => "British Pound",
            "JPY" => "Japanese Yen",
            "CNY" => "Chinese Yuan",
            _ => return None,
        }),
        _ => None,
    }
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__dn")?;
    let get = |k: &str| {
        o.borrow()
            .props
            .get(k)
            .map(|p| p.value.clone())
            .unwrap_or(Value::Undefined)
    };
    let res = i.new_object();
    set_data(&res, "locale", get("__dn_locale"));
    set_data(&res, "style", get("__dn_style"));
    set_data(&res, "type", get("__dn_type"));
    set_data(&res, "fallback", get("__dn_fallback"));
    set_data(&res, "languageDisplay", get("__dn_langdisplay"));
    Ok(Value::Obj(res))
}
