//! A from-scratch subset of ECMA-402 (`Intl`). No external ICU/CLDR dependency: the locale data
//! (aliases, number symbols, date patterns, …) is embedded as small Rust tables, grown as the
//! intl402 test262 score climbs. The structural machinery — BCP-47 tag parsing/canonicalization,
//! option coercion, locale negotiation — is complete; the per-locale data is a deliberate subset.

use crate::interpreter::Interp;
use crate::value::{set_builtin, Gc, Object, Property, Value};
use std::rc::Rc;

mod data;
mod listformat;
mod locale;
mod numberformat;
mod pluralrules;
mod relativetimeformat;
mod service;
mod tags;

pub use tags::{canonicalize_language_tag, is_structurally_valid_tag};
pub(crate) use service::{resolve_locale, ResolvedLocale};

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

/// Install `Intl` and its services on the global object.
pub fn install(it: &mut Interp) {
    let intl = Object::new(Some(it.object_proto.clone()));

    // Intl[@@toStringTag] = "Intl" { writable:false, enumerable:false, configurable:true }.
    if let Some(key) = crate::builtins::to_string_tag_key(it) {
        intl.borrow_mut()
            .props
            .insert(key, Property::data(Value::str("Intl"), false, false, true));
    }

    // Intl.getCanonicalLocales(locales) — returns a fresh Array of canonicalized tags.
    let f = it.make_native("getCanonicalLocales", 1, |i, _t, a| {
        let list = canonicalize_locale_list(i, &arg(a, 0))?;
        Ok(i.make_array(list.into_iter().map(Value::from_string).collect()))
    });
    set_builtin(&intl, "getCanonicalLocales", Value::Obj(f));

    // Intl.supportedValuesOf(key).
    let f = it.make_native("supportedValuesOf", 1, |i, _t, a| supported_values_of(i, &arg(a, 0)));
    set_builtin(&intl, "supportedValuesOf", Value::Obj(f));

    locale::install(it, &intl);
    listformat::install(it, &intl);
    pluralrules::install(it, &intl);
    relativetimeformat::install(it, &intl);
    numberformat::install(it, &intl);

    it.global
        .borrow_mut()
        .props
        .insert("Intl", Property::builtin(Value::Obj(intl)));
}

/// CanonicalizeLocaleList(locales): `undefined` → empty; a String is treated as a single-element
/// list; otherwise an array-like whose elements must be Strings or Locale objects. Returns the
/// deduplicated list of canonical tags (throwing RangeError on a structurally invalid tag).
pub(crate) fn canonicalize_locale_list(
    i: &mut Interp,
    locales: &Value,
) -> Result<Vec<String>, Value> {
    let mut seen: Vec<String> = Vec::new();
    if matches!(locales, Value::Undefined) {
        return Ok(seen);
    }
    // A String or Locale value is a single-element list; otherwise iterate the array-like, reading
    // and canonicalizing each element in turn (an element's ToString may mutate later indices).
    match locales {
        Value::Str(_) => process_locale_item(i, locales, &mut seen)?,
        Value::Obj(o) if o.borrow().props.contains("__locale_tag") => {
            process_locale_item(i, locales, &mut seen)?
        }
        Value::Null => return Err(i.make_error("TypeError", "Cannot convert null to object")),
        _ => {
            let lenv = ab(i.get_member(locales, "length"))?;
            let len = to_length(i, &lenv)?;
            for k in 0..len {
                let key = k.to_string();
                if ab(i.js_has_property(locales, &key))? {
                    let item = ab(i.get_member(locales, &key))?;
                    process_locale_item(i, &item, &mut seen)?;
                }
            }
        }
    }
    Ok(seen)
}

/// Canonicalize one locale-list element (a String, Locale, or Object) and append it (deduplicated).
fn process_locale_item(i: &mut Interp, item: &Value, seen: &mut Vec<String>) -> Result<(), Value> {
    let tag = match item {
        Value::Obj(o) if o.borrow().props.contains("__locale_tag") => {
            match o.borrow().props.get("__locale_tag").map(|p| p.value.clone()) {
                Some(Value::Str(s)) => s.to_string(),
                _ => String::new(),
            }
        }
        Value::Str(_) | Value::Obj(_) => {
            let s = ab(i.to_string(item))?;
            if !is_structurally_valid_tag(&s) {
                return Err(i.make_error(
                    "RangeError",
                    format!("Incorrect locale information provided: {s}"),
                ));
            }
            match canonicalize_language_tag(&s) {
                Some(c) => c,
                None => {
                    return Err(i.make_error(
                        "RangeError",
                        format!("Incorrect locale information provided: {s}"),
                    ))
                }
            }
        }
        _ => return Err(i.make_error("TypeError", "locale must be a string or object")),
    };
    if !seen.contains(&tag) {
        seen.push(tag);
    }
    Ok(())
}

fn supported_values_of(i: &mut Interp, key: &Value) -> Result<Value, Value> {
    let k = ab(i.to_string(key))?;
    let vals: &[&str] = match &*k {
        "calendar" => &["buddhist", "chinese", "coptic", "dangi", "ethioaa", "ethiopic", "gregory", "hebrew", "indian", "islamic", "islamic-umalqura", "islamic-tbla", "islamic-civil", "islamic-rgsa", "iso8601", "japanese", "persian", "roc"],
        "collation" => &["compat", "dict", "emoji", "eor", "phonebk", "pinyin", "searchjl", "stroke", "trad", "unihan", "zhuyin"],
        "currency" => &["USD", "EUR", "GBP", "JPY", "CNY"],
        "numberingSystem" => &["adlm", "ahom", "arab", "arabext", "bali", "beng", "deva", "fullwide", "gujr", "guru", "hanidec", "khmr", "knda", "laoo", "latn", "mlym", "mymr", "orya", "tamldec", "telu", "thai", "tibt"],
        "timeZone" => &["UTC", "America/New_York", "Europe/London", "Asia/Tokyo"],
        "unit" => &["acre", "bit", "byte", "celsius", "centimeter", "day", "degree", "fahrenheit", "gigabyte", "gram", "hour", "kilogram", "kilometer", "liter", "megabyte", "meter", "mile", "milliliter", "millimeter", "millisecond", "minute", "month", "ounce", "percent", "petabyte", "pound", "second", "terabyte", "week", "yard", "year"],
        _ => return Err(i.make_error("RangeError", format!("invalid key: {k}"))),
    };
    Ok(i.make_array(vals.iter().map(|s| Value::str(*s)).collect()))
}

// ----- shared helpers used across the intl services --------------------------------------------

/// Map an interpreter `Abrupt` to the native `Result<_, Value>` contract.
pub(crate) fn ab<T>(r: Result<T, crate::interpreter::Abrupt>) -> Result<T, Value> {
    r.map_err(|a| match a {
        crate::interpreter::Abrupt::Throw(v) => v,
        _ => Value::Undefined,
    })
}

/// Build a constructor/prototype pair with the given `@@toStringTag`, install it on `ns`.
pub(crate) fn make_service(
    it: &mut Interp,
    ns: &Gc,
    name: &'static str,
    len: usize,
    ctor_fn: crate::value::NativeFn,
) -> (Gc, Gc) {
    let proto = Object::new(Some(it.object_proto.clone()));
    let ctor = it.make_native(name, len, ctor_fn);
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
            Property::data(Value::str(format!("Intl.{name}")), false, false, true),
        );
    }
    it.extra_protos.insert(
        Box::leak(format!("Intl.{name}").into_boxed_str()),
        proto.clone(),
    );
    set_builtin(ns, name, Value::Obj(ctor.clone()));
    (ctor, proto)
}

// (get_string_option removed; per-service option readers live in each module)

/// GetOption(options, property, "boolean", …).
#[allow(dead_code)]
pub(crate) fn get_boolean_option(
    i: &mut Interp,
    options: &Value,
    property: &str,
    fallback: bool,
) -> Result<bool, Value> {
    let v = ab(i.get_member(options, property))?;
    if matches!(v, Value::Undefined) {
        return Ok(fallback);
    }
    Ok(i.to_boolean(&v))
}

/// CoerceOptionsToObject: `undefined` → a fresh ordinary object; an object is used as-is; `null`
/// throws; other primitives yield an empty object (their reads fall through to option defaults).
pub(crate) fn coerce_options(i: &mut Interp, options: &Value) -> Result<Value, Value> {
    match options {
        Value::Undefined => Ok(Value::Obj(i.new_object())),
        Value::Obj(_) => Ok(options.clone()),
        Value::Null => Err(i.make_error("TypeError", "options cannot be null")),
        _ => Ok(Value::Obj(i.new_object())),
    }
}

/// ToLength on an already-evaluated value.
pub(crate) fn to_length(i: &mut Interp, v: &Value) -> Result<usize, Value> {
    let n = ab(i.to_number(v))?;
    if n.is_nan() || n <= 0.0 {
        return Ok(0);
    }
    Ok(n.min(9007199254740991.0) as usize)
}

/// Install a getter-only accessor on `proto`.
pub(crate) fn def_getter(it: &mut Interp, proto: &Gc, name: &str, f: crate::value::NativeFn) {
    let g = it.make_native(&format!("get {name}"), 0, f);
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

#[allow(dead_code)]
pub(crate) fn install_service_common(it: &mut Interp, ctor: &Gc, proto: &Gc) {
    // Intl.<Service>.supportedLocalesOf(locales, options).
    let f = it.make_native("supportedLocalesOf", 1, |i, _t, a| {
        let requested = canonicalize_locale_list(i, &arg(a, 0))?;
        // A minimal (best-fit == lookup) matcher: `en` and any exactly-supported tag pass through.
        let out: Vec<Value> = requested
            .into_iter()
            .filter(|t| locale::is_supported(t))
            .map(Value::from_string)
            .collect();
        Ok(i.make_array(out))
    });
    ctor.borrow_mut()
        .props
        .insert("supportedLocalesOf", Property::builtin(Value::Obj(f)));

    // resolvedOptions / the per-service format methods are attached by each service; here we just
    // ensure `prototype` is non-configurable already (done in make_service).
    let _ = proto;
}

#[allow(dead_code)]
pub(crate) fn tag_of_str(v: &Value) -> Option<Rc<str>> {
    match v {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}
