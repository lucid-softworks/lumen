//! `Intl.Collator` (a locale-independent case-aware comparison; option surface complete).

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Gc, Value};

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
        let f = i.make_native("", 2, |i, that, a| {
            compare(i, &that, &arg(a, 0), &arg(a, 1))
        });
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
    let collation_opt = get_option(i, &options, "collation", &[], None)?;
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
    let ignore_punct_opt = {
        let v = ab(i.get_member(&options, "ignorePunctuation"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };
    let resolved = resolve_locale(i, &requested, &["co", "kn", "kf"]);
    // ignorePunctuation defaults per locale: the dictionary-ordered locales (Thai) default to true.
    let ignore_punct =
        ignore_punct_opt.unwrap_or_else(|| resolved.locale.split('-').next() == Some("th"));
    let kw = |k: &str| {
        resolved
            .keywords
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };
    let numeric_opt = numeric;
    let case_first_opt = case_first.clone();
    // The option wins over the locale's -u- keyword; a bare -u-kn (empty value) means numeric=true.
    let numeric = numeric_opt.unwrap_or_else(|| match kw("kn") {
        Some(v) => v != "false",
        None => false,
    });
    let case_first = case_first_opt
        .clone()
        .or_else(|| kw("kf"))
        .unwrap_or_else(|| "false".to_string());
    // The `-u-co-` value must be a known collation type (never the reserved standard/search); an
    // unknown one falls back to "default".
    const COLLATIONS: [&str; 15] = [
        "compat", "dict", "emoji", "eor", "phonebk", "phonetic", "pinyin", "reformed", "searchjl",
        "stroke", "trad", "unihan", "zhuyin", "big5han", "gb2312",
    ];
    // A collation is *supported* per locale (phonebk for German; eor broadly). The `collation`
    // option overrides the -u-co extension when supported; an unsupported value is ignored.
    let res_lang = resolved
        .locale
        .split('-')
        .next()
        .unwrap_or("en")
        .to_string();
    let supported = |c: &str| c == "eor" || (c == "phonebk" && res_lang == "de");
    let opt_co = collation_opt
        .filter(|c| COLLATIONS.contains(&c.as_str()) && supported(c))
        .filter(|_| usage == "sort");
    let ext_co = kw("co").filter(|c| COLLATIONS.contains(&c.as_str()) && supported(c));
    let collation = opt_co
        .clone()
        .or_else(|| ext_co.clone())
        .unwrap_or_else(|| "default".to_string());

    // ResolveLocale: reflect the surviving `-u-` keywords in the resolved locale string. A keyword
    // survives when its value came from the locale extension and no differing option overrode it
    // (keys are emitted in alphabetical order: co, kf, kn).
    let mut additions: Vec<(&str, String)> = Vec::new();
    if let Some(co) = ext_co {
        if opt_co.as_ref().is_none_or(|o| *o == co) {
            additions.push(("co", co));
        }
    }
    if let Some(kf) = kw("kf") {
        if ["upper", "lower", "false"].contains(&kf.as_str())
            && case_first_opt.as_deref().is_none_or(|o| o == kf)
        {
            additions.push(("kf", kf));
        }
    }
    if let Some(kn) = kw("kn") {
        let kn_bool = kn != "false";
        if numeric_opt.is_none_or(|o| o == kn_bool) {
            // Canonical form: the `true` value is elided (`-u-kn`), `false` is spelled out.
            additions.push((
                "kn",
                if kn_bool {
                    String::new()
                } else {
                    "false".to_string()
                },
            ));
        }
    }
    let locale = if additions.is_empty() {
        resolved.locale.clone()
    } else {
        let ext: String = additions
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    format!("-{k}")
                } else {
                    format!("-{k}-{v}")
                }
            })
            .collect();
        format!("{}-u{}", resolved.locale, ext)
    };

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.Collator")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__co", Value::Bool(true));
    set_builtin(&obj, "__co_locale", Value::from_string(locale));
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
    let get = |k: &str| match o.borrow().props.get(k).map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    };
    let getb = |k: &str| {
        matches!(
            o.borrow().props.get(k).map(|p| p.value.clone()),
            Some(Value::Bool(true))
        )
    };
    let sa = ab(i.to_string(a))?.to_string();
    let sb = ab(i.to_string(b))?.to_string();
    let opts = CollateOpts {
        sensitivity: get("__co_sensitivity"),
        numeric: getb("__co_numeric"),
        ignore_punct: getb("__co_ignorepunct"),
        upper_first: get("__co_casefirst") == "upper",
        // German ä/ö/ü expand to ae/oe/ue under the phonebook collation and in search usage.
        expand_umlaut: {
            let lang = get("__co_locale");
            let lang = lang.split('-').next().unwrap_or("");
            lang == "de" && (get("__co_collation") == "phonebk" || get("__co_usage") == "search")
        },
    };
    let ord = match collate(&sa, &sb, &opts) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Equal => 0.0,
    };
    Ok(Value::Num(ord))
}

struct CollateOpts {
    sensitivity: String,
    numeric: bool,
    ignore_punct: bool,
    upper_first: bool,
    expand_umlaut: bool,
}

/// Per-primary-slot collation element: a lowercased base code point, its attached marks
/// (secondary), and a case weight (tertiary).
struct El {
    prim: u32,
    marks: Vec<u32>,
    upper: bool,
}

fn elements(s: &str, opts: &CollateOpts) -> Vec<El> {
    let cps = crate::jstr::code_points(s);
    let nfd = crate::unicode_norm_impl::decompose(&cps, false);
    let mut els: Vec<El> = Vec::with_capacity(nfd.len());
    let mut k = 0;
    while k < nfd.len() {
        let cp = nfd[k];
        k += 1;
        if crate::unicode_norm_impl::ccc(cp) != 0 {
            if let Some(last) = els.last_mut() {
                last.marks.push(cp);
                continue;
            }
        }
        if opts.ignore_punct && is_collation_ignorable(cp) {
            continue;
        }
        let ch = char::from_u32(cp);
        let upper = ch.map(|c| c.is_uppercase()).unwrap_or(false);
        let prim = ch
            .and_then(|c| c.to_lowercase().next())
            .map(|c| c as u32)
            .unwrap_or(cp);
        // German phonebook/search expansion: ä → "ae" at the primary level, with the umlaut
        // kept as a secondary difference (so AE < Ä).
        if opts.expand_umlaut && matches!(prim, 0x61 | 0x6F | 0x75) && nfd.get(k) == Some(&0x308) {
            k += 1;
            els.push(El {
                prim,
                marks: vec![0x308],
                upper,
            });
            els.push(El {
                prim: 'e' as u32,
                marks: Vec::new(),
                upper: false,
            });
            continue;
        }
        els.push(El {
            prim,
            marks: Vec::new(),
            upper,
        });
    }
    els
}

fn is_collation_ignorable(cp: u32) -> bool {
    match char::from_u32(cp) {
        Some(c) => c.is_whitespace() || c.is_ascii_punctuation() || (0x2000..=0x206F).contains(&cp),
        None => false,
    }
}

/// A three-level (primary letters / secondary accents / tertiary case) comparison, with an
/// optional numeric mode that compares digit runs by value at the primary level.
fn collate(a: &str, b: &str, opts: &CollateOpts) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ea = elements(a, opts);
    let eb = elements(b, opts);
    // Primary.
    let prim = if opts.numeric {
        cmp_primary_numeric(&ea, &eb)
    } else {
        ea.iter().map(|e| e.prim).cmp(eb.iter().map(|e| e.prim))
    };
    if prim != Ordering::Equal {
        return prim;
    }
    let sens = opts.sensitivity.as_str();
    // Secondary (accents).
    if sens == "accent" || sens == "variant" {
        let sec = ea.iter().map(|e| &e.marks).cmp(eb.iter().map(|e| &e.marks));
        if sec != Ordering::Equal {
            return sec;
        }
    }
    // Tertiary (case): lowercase first unless caseFirst is "upper".
    if sens == "case" || sens == "variant" {
        let w = |u: bool| u != opts.upper_first;
        let ter = ea
            .iter()
            .map(|e| w(e.upper))
            .cmp(eb.iter().map(|e| w(e.upper)));
        if ter != Ordering::Equal {
            return ter;
        }
    }
    Ordering::Equal
}

fn cmp_primary_numeric(a: &[El], b: &[El]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let is_digit = |e: &El| (0x30..=0x39).contains(&e.prim);
    let (mut i, mut j) = (0, 0);
    loop {
        match (a.get(i), b.get(j)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if is_digit(x) && is_digit(y) {
                    // Compare whole digit runs by numeric value, then by run position.
                    let (si, sj) = (i, j);
                    while a.get(i).map(is_digit).unwrap_or(false) {
                        i += 1;
                    }
                    while b.get(j).map(is_digit).unwrap_or(false) {
                        j += 1;
                    }
                    let da: String = a[si..i].iter().map(|e| (e.prim as u8) as char).collect();
                    let db: String = b[sj..j].iter().map(|e| (e.prim as u8) as char).collect();
                    let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                    let c = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                    if c != Ordering::Equal {
                        return c;
                    }
                } else {
                    let c = x.prim.cmp(&y.prim);
                    if c != Ordering::Equal {
                        return c;
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
    }
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__co")?;
    let get = |k: &str| {
        o.borrow()
            .props
            .get(k)
            .map(|p| p.value.clone())
            .unwrap_or(Value::Undefined)
    };
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
