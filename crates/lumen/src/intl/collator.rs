//! `Intl.Collator` (a locale-independent case-aware comparison; option surface complete).

use super::service::{
    brand_slot, get_option, instance_proto, install_supported_locales, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, get_options_object as coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_data, set_builtin, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "Collator", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
    install_compare_getter(it, &proto);
}

fn install_compare_getter(it: &mut Interp, proto: &Gc) {
    let g = it.make_native("get compare", 0, |i, this, _| {
        let o = brand_slot(i, &this, "__co")?;
        if let Some(f) = o.borrow().props.get("__co_bound").map(|p| p.value.clone()) {
            return Ok(f);
        }
        let f = i.make_native("", 2, |i, that, a| compare(i, &that, &arg(a, 0), &arg(a, 1)));
        let bound = crate::intl::numberformat::bind_this(i, Value::Obj(f), this.clone());
        set_builtin(&o, "__co_bound", bound.clone());
        Ok(bound)
    });
    proto.borrow_mut().props.insert(
        "compare",
        crate::value::Property {
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

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    // Legacy service: callable without `new` (returns a fresh instance either way).
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    let usage = get_option(i, &options, "usage", &["sort", "search"], Some("sort"))?.unwrap();
    read_locale_matcher(i, &options)?;
    let _ = get_option(i, &options, "collation", &[], None)?;
    let numeric = {
        let v = ab(i.get_member(&options, "numeric"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };
    let case_first = get_option(i, &options, "caseFirst", &["upper", "lower", "false"], None)?;
    let sensitivity = get_option(
        i,
        &options,
        "sensitivity",
        &["base", "accent", "case", "variant"],
        Some("variant"),
    )?
    .unwrap();
    let ignore_punct = {
        let v = ab(i.get_member(&options, "ignorePunctuation"))?;
        if matches!(v, Value::Undefined) {
            false
        } else {
            i.to_boolean(&v)
        }
    };
    let resolved = resolve_locale(i, &requested, &["co", "kn", "kf"]);
    let kw = |k: &str| resolved.keywords.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    // The option wins over the locale's -u- keyword; a bare -u-kn (empty value) means numeric=true.
    let numeric = numeric.unwrap_or_else(|| match kw("kn") {
        Some(v) => v != "false",
        None => false,
    });
    let case_first = case_first.or_else(|| kw("kf")).unwrap_or_else(|| "false".to_string());
    let collation = kw("co")
        .filter(|c| c != "standard" && c != "search")
        .unwrap_or_else(|| "default".to_string());

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.Collator") {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__co", Value::Bool(true));
    set_builtin(&obj, "__co_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__co_usage", Value::from_string(usage));
    set_builtin(&obj, "__co_sensitivity", Value::from_string(sensitivity));
    set_builtin(&obj, "__co_ignorepunct", Value::Bool(ignore_punct));
    set_builtin(&obj, "__co_numeric", Value::Bool(numeric));
    set_builtin(&obj, "__co_collation", Value::from_string(collation));
    set_builtin(&obj, "__co_casefirst", Value::from_string(case_first));
    Ok(Value::Obj(obj))
}

fn compare(i: &mut Interp, this: &Value, a: &Value, b: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__co")?;
    let sensitivity = match o.borrow().props.get("__co_sensitivity").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "variant".to_string(),
    };
    let sa = ab(i.to_string(a))?.to_string();
    let sb = ab(i.to_string(b))?.to_string();
    // base/accent sensitivity ignores case; a full collator would fold diacritics too.
    let (ka, kb) = if sensitivity == "base" || sensitivity == "accent" {
        (sa.to_lowercase(), sb.to_lowercase())
    } else {
        (sa.clone(), sb.clone())
    };
    let ord = match ka.cmp(&kb) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Equal => 0.0,
    };
    Ok(Value::Num(ord))
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__co")?;
    let get = |k: &str| o.borrow().props.get(k).map(|p| p.value.clone()).unwrap_or(Value::Undefined);
    let res = i.new_object();
    set_data(&res, "locale", get("__co_locale"));
    set_data(&res, "usage", get("__co_usage"));
    set_data(&res, "sensitivity", get("__co_sensitivity"));
    set_data(&res, "ignorePunctuation", get("__co_ignorepunct"));
    set_data(&res, "collation", get("__co_collation"));
    set_data(&res, "numeric", get("__co_numeric"));
    set_data(&res, "caseFirst", get("__co_casefirst"));
    Ok(Value::Obj(res))
}
