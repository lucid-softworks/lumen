//! `Intl.ListFormat`.

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, get_options_object as coerce_options, data, def_getter, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Value};

pub fn install(it: &mut Interp, ns: &crate::value::Gc) {
    let (ctor, proto) = make_service(it, ns, "ListFormat", 0, construct);
    install_supported_locales(it, &ctor);

    it.def_method(&proto, "format", 1, |i, this, a| format(i, &this, &arg(a, 0), false));
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        format(i, &this, &arg(a, 0), true)
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
    let _ = def_getter;
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.ListFormat requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let kind = get_option(
        i,
        &options,
        "type",
        &["conjunction", "disjunction", "unit"],
        Some("conjunction"),
    )?
    .unwrap();
    let style = get_option(i, &options, "style", &["long", "short", "narrow"], Some("long"))?
        .unwrap();
    let resolved = resolve_locale(i, &requested, &[]);

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.ListFormat")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__lf", Value::Bool(true));
    set_builtin(&obj, "__lf_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__lf_type", Value::from_string(kind));
    set_builtin(&obj, "__lf_style", Value::from_string(style));
    Ok(Value::Obj(obj))
}

/// StringListFromIterable: iterate the argument, requiring every element to be a String.
fn string_list(i: &mut Interp, list: &Value) -> Result<Vec<String>, Value> {
    if matches!(list, Value::Undefined) {
        return Ok(Vec::new());
    }
    // Iterate LAZILY so a non-string element throws immediately (stopping the iteration), rather than
    // eagerly draining the iterable first.
    let (iter, next) = ab(i.get_iterator(list))?;
    let mut out = Vec::new();
    loop {
        match ab(i.iterator_step(&iter, &next))? {
            None => break,
            Some(Value::Str(s)) => out.push(s.to_string()),
            Some(_) => {
                let err = i.make_error("TypeError", "Intl.ListFormat list elements must be strings");
                i.iterator_close(&iter);
                return Err(err);
            }
        }
    }
    Ok(out)
}

fn assemble(parts: &[String], pats: [&'static str; 4]) -> String {
    let apply = |pat: &str, a: &str, b: &str| pat.replace("{0}", a).replace("{1}", b);
    match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        2 => apply(pats[0], &parts[0], &parts[1]),
        n => {
            let mut result = parts[n - 1].clone();
            result = apply(pats[3], &parts[n - 2], &result); // end
            for idx in (1..n - 2).rev() {
                result = apply(pats[2], &parts[idx], &result); // middle
            }
            apply(pats[1], &parts[0], &result) // start
        }
    }
}

fn format(i: &mut Interp, this: &Value, list: &Value, to_parts: bool) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__lf")?;
    let get = |k: &str| match o.borrow().props.get(k).map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    };
    let (locale, kind, style) = (get("__lf_locale"), get("__lf_type"), get("__lf_style"));
    let lang = locale.split('-').next().unwrap_or("en");
    let parts = string_list(i, list)?;
    let pats = data::list_patterns(lang, &kind, &style);
    if !to_parts {
        return Ok(Value::from_string(assemble(&parts, pats)));
    }
    // formatToParts: emit "element"/"literal" segments. We reconstruct by diffing the assembled
    // string against the elements (sufficient for the common patterns).
    let whole = assemble(&parts, pats);
    let mut segments: Vec<(String, String)> = Vec::new();
    let mut rest = whole.as_str();
    for (idx, p) in parts.iter().enumerate() {
        if let Some(pos) = rest.find(p.as_str()) {
            if pos > 0 {
                segments.push(("literal".to_string(), rest[..pos].to_string()));
            }
            segments.push(("element".to_string(), p.clone()));
            rest = &rest[pos + p.len()..];
        }
        let _ = idx;
    }
    if !rest.is_empty() {
        segments.push(("literal".to_string(), rest.to_string()));
    }
    let arr: Vec<Value> = segments
        .into_iter()
        .map(|(t, v)| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::from_string(t));
            set_data(&ob, "value", Value::from_string(v));
            Value::Obj(ob)
        })
        .collect();
    Ok(i.make_array(arr))
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__lf")?;
    let get = |k: &str| o.borrow().props.get(k).map(|p| p.value.clone()).unwrap_or(Value::Undefined);
    let res = i.new_object();
    set_data(&res, "locale", get("__lf_locale"));
    set_data(&res, "type", get("__lf_type"));
    set_data(&res, "style", get("__lf_style"));
    Ok(Value::Obj(res))
}
