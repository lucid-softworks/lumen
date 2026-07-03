//! `Intl.ListFormat`.

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
    resolve_locale,
};
use super::{
    ab, arg, canonicalize_locale_list, data, def_getter, get_options_object as coerce_options,
    make_service,
};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Value};

pub fn install(it: &mut Interp, ns: &crate::value::Gc) {
    let (ctor, proto) = make_service(it, ns, "ListFormat", 0, construct);
    install_supported_locales(it, &ctor);

    it.def_method(&proto, "format", 1, |i, this, a| {
        format(i, &this, &arg(a, 0), false)
    });
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
    let style = get_option(
        i,
        &options,
        "style",
        &["long", "short", "narrow"],
        Some("long"),
    )?
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
                let err =
                    i.make_error("TypeError", "Intl.ListFormat list elements must be strings");
                i.iterator_close(&iter);
                return Err(err);
            }
        }
    }
    Ok(out)
}

/// The literal between `{0}` and `{1}` in a two-placeholder list pattern.
fn pat_sep(pat: &str) -> &str {
    pat.trim_start_matches("{0}").trim_end_matches("{1}")
}

/// CreatePartsFromList: alternating element/literal segments (`true` = element).
fn assemble_segments(parts: &[String], pats: [&'static str; 4]) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let n = parts.len();
    for (idx, p) in parts.iter().enumerate() {
        if idx > 0 {
            let sep = match (n, idx) {
                (2, _) => pat_sep(pats[0]),               // pair
                (_, 1) => pat_sep(pats[1]),               // start
                (_, i) if i == n - 1 => pat_sep(pats[3]), // end
                _ => pat_sep(pats[2]),                    // middle
            };
            if !sep.is_empty() {
                out.push((false, sep.to_string()));
            }
        }
        out.push((true, p.clone()));
    }
    out
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
    let segments = assemble_segments(&parts, pats);
    if !to_parts {
        return Ok(Value::from_string(
            segments.iter().map(|(_, s)| s.as_str()).collect::<String>(),
        ));
    }
    let arr: Vec<Value> = segments
        .into_iter()
        .map(|(is_elem, v)| {
            let ob = i.new_object();
            let t = if is_elem { "element" } else { "literal" };
            set_data(&ob, "type", Value::from_string(t.to_string()));
            set_data(&ob, "value", Value::from_string(v));
            Value::Obj(ob)
        })
        .collect();
    Ok(i.make_array(arr))
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__lf")?;
    let get = |k: &str| {
        o.borrow()
            .props
            .get(k)
            .map(|p| p.value.clone())
            .unwrap_or(Value::Undefined)
    };
    let res = i.new_object();
    set_data(&res, "locale", get("__lf_locale"));
    set_data(&res, "type", get("__lf_type"));
    set_data(&res, "style", get("__lf_style"));
    Ok(Value::Obj(res))
}
