//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_json(it: &mut Interp) {
    let j = it.new_object();
    it.def_method(&j, "stringify", 3, |i, _t, args| {
        let value = arg(args, 0);
        let replacer = arg(args, 1);
        // The replacer is either a function, or an array PropertyList of keys (strings/numbers).
        let opts = if replacer.is_callable() {
            JsonOpts {
                func: Some(replacer),
                keys: None,
            }
        } else if json_is_array(i, &replacer)? {
            let len = match &replacer {
                Value::Obj(o) if proxy_pair(i, &replacer).is_none() => i.array_length(o),
                Value::Obj(o) => ab(i.to_length(o))?,
                _ => 0,
            };
            let mut list: Vec<String> = Vec::new();
            for k in 0..len {
                let item = ab(i.get_member(&replacer, &k.to_string()))?;
                // String/Number primitives and their wrappers contribute a key via ToString.
                let key = match &item {
                    Value::Str(s) => Some(s.to_string()),
                    Value::Num(n) => Some(i.num_to_str(*n)),
                    Value::Obj(o)
                        if matches!(o.borrow().exotic, Exotic::StrWrap(_) | Exotic::NumWrap(_)) =>
                    {
                        Some(ab(i.to_string(&item))?.to_string())
                    }
                    _ => None,
                };
                if let Some(key) = key {
                    if !list.contains(&key) {
                        list.push(key);
                    }
                }
            }
            JsonOpts {
                func: None,
                keys: Some(list),
            }
        } else {
            JsonOpts {
                func: None,
                keys: None,
            }
        };
        // The `space` argument: a Number/String wrapper is unwrapped first; then a Number becomes
        // that many spaces (clamped 0..10), a String its first 10 code units, else no indentation.
        let mut space = arg(args, 2);
        if let Value::Obj(o) = &space {
            let exotic = o.borrow().exotic.clone();
            match exotic {
                Exotic::NumWrap(_) => space = Value::Num(ab(i.to_number(&space))?),
                Exotic::StrWrap(_) => space = Value::Str(ab(i.to_string(&space))?),
                _ => {}
            }
        }
        let gap = match space {
            Value::Num(n) => {
                let n = if n.is_nan() { 0.0 } else { n.trunc() };
                " ".repeat(n.clamp(0.0, 10.0) as usize)
            }
            Value::Str(s) => s.chars().take(10).collect(),
            _ => String::new(),
        };
        // SerializeJSONProperty starts from a wrapper holder `{ "": value }`.
        let wrapper = i.new_object();
        set_data(&wrapper, "", value);
        let mut seen = Vec::new();
        match json_str(i, &Value::Obj(wrapper), "", &opts, &gap, "", &mut seen)? {
            Some(s) => Ok(Value::from_string(s)),
            None => Ok(Value::Undefined),
        }
    });
    it.def_method(&j, "parse", 2, |i, _t, args| {
        let text = ab(i.to_string(&arg(args, 0)))?;
        let chars: Vec<char> = text.chars().collect();
        let mut pos = 0;
        let reviver = arg(args, 1);
        // A callable reviver walks the result via InternalizeJSONProperty from a `{ "": v }` root,
        // recording primitive source spans for its `context` argument.
        if reviver.is_callable() {
            let (v, record) = json_parse_recorded(i, &chars, &mut pos)?;
            json_skip_ws(&chars, &mut pos);
            if pos != chars.len() {
                return Err(i.make_error("SyntaxError", "Unexpected non-whitespace after JSON"));
            }
            let root = i.new_object();
            set_data(&root, "", v);
            return internalize_json_property(i, &Value::Obj(root), "", &reviver, Some(&record));
        }
        let v = json_parse_value(i, &chars, &mut pos)?;
        json_skip_ws(&chars, &mut pos);
        if pos != chars.len() {
            return Err(i.make_error("SyntaxError", "Unexpected non-whitespace after JSON"));
        }
        Ok(v)
    });
    it.def_method(&j, "rawJSON", 1, |i, _t, args| {
        let text = ab(i.to_string(&arg(args, 0)))?.to_string();
        let bytes: Vec<char> = text.chars().collect();
        if bytes.is_empty() {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: empty string"));
        }
        let is_ws = |c: char| matches!(c, '\t' | '\n' | '\r' | ' ');
        if is_ws(bytes[0]) || is_ws(*bytes.last().unwrap()) {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: leading/trailing whitespace"));
        }
        if bytes[0] == '{' || bytes[0] == '[' {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON value must be a primitive"));
        }
        // Validate it is exactly one JSON value.
        let mut pos = 0;
        json_parse_value(i, &bytes, &mut pos)?;
        json_skip_ws(&bytes, &mut pos);
        if pos != bytes.len() {
            return Err(i.make_error("SyntaxError", "JSON.rawJSON: invalid JSON text"));
        }
        let o = i.new_object();
        o.borrow_mut().proto = None;
        set_data(&o, "rawJSON", Value::from_string(text.clone()));
        set_internal(&o, "\u{0}raw_json", Value::from_string(text));
        i.freeze_object(&Value::Obj(o.clone()));
        Ok(Value::Obj(o))
    });
    it.def_method(&j, "isRawJSON", 1, |_i, _t, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Obj(o) if o.borrow().props.contains("\u{0}raw_json")),
        ))
    });
    set_to_string_tag(it, &j, "JSON");
    set_builtin(&it.global, "JSON", Value::Obj(j));
}

fn json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    let mut chars = s.chars().peekable();
    while let Some(mut c) = chars.next() {
        // A smuggled pair round-trips as its real character. If that character itself falls in
        // the smuggle range (a real plane-16 PUA code point), keep the smuggled representation so
        // it isn't re-read as a lone surrogate below.
        if let Some(real) = chars.peek().and_then(|&n| crate::jstr::paired_char(c, n)) {
            if crate::jstr::smuggled(real).is_some() {
                out.push(c);
                out.push(chars.next().unwrap());
                continue;
            }
            chars.next();
            c = real;
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => match crate::jstr::smuggled(c) {
                // Well-formed JSON.stringify: a lone surrogate is written as its \u escape.
                Some(u) => out.push_str(&format!("\\u{u:04x}")),
                None => out.push(c),
            },
        }
    }
    out.push('"');
    out
}

/// JSON.stringify options: an optional function replacer and/or an array PropertyList of keys.
struct JsonOpts {
    func: Option<Value>,
    keys: Option<Vec<String>>,
}

/// [[Delete]] for the JSON reviver: trap-aware, discarding the boolean status (per `Perform ?`).
fn json_delete_prop(i: &mut Interp, holder: &Value, key: &str) -> Result<(), Value> {
    if let Some((target, handler)) = proxy_pair(i, holder) {
        ab(i.proxy_delete(target, handler, key))?;
        return Ok(());
    }
    if let Value::Obj(o) = holder {
        let configurable = o
            .borrow()
            .props
            .get(key)
            .map(|p| p.configurable)
            .unwrap_or(true);
        if configurable {
            o.borrow_mut().props.remove(key);
        }
    }
    Ok(())
}

/// CreateDataProperty for the JSON reviver: a { value, writable, enumerable, configurable: true }
/// data property, trap-aware for proxy holders.
fn json_create_data_prop(i: &mut Interp, holder: &Value, key: &str, v: Value) -> Result<(), Value> {
    // CreateDataProperty defines a fully-permissive data descriptor via [[DefineOwnProperty]], which
    // validates against any existing (e.g. non-configurable) property; a false result is not an error.
    let desc = i.new_object();
    set_data(&desc, "value", v);
    set_data(&desc, "writable", Value::Bool(true));
    set_data(&desc, "enumerable", Value::Bool(true));
    set_data(&desc, "configurable", Value::Bool(true));
    if let Some((target, handler)) = proxy_pair(i, holder) {
        ab(proxy_define_property(
            i,
            &target,
            &handler,
            key,
            &Value::Obj(desc),
        ))?;
        return Ok(());
    }
    if let Value::Obj(o) = holder {
        ab(define_own_property(i, o, key, &Value::Obj(desc)))?;
    }
    Ok(())
}

/// InternalizeJSONProperty: recursively walk the parsed value, applying the reviver bottom-up. The
/// optional record carries primitive source text for the reviver's `context.source`.
fn internalize_json_property(
    i: &mut Interp,
    holder: &Value,
    name: &str,
    reviver: &Value,
    record: Option<&JsonRecord>,
) -> Result<Value, Value> {
    let val = ab(i.get_member(holder, name))?;
    if matches!(val, Value::Obj(_)) {
        if json_is_array(i, &val)? {
            let len = match val.as_obj() {
                Some(o) => ab(i.to_length(o))?,
                None => 0,
            };
            for idx in 0..len {
                let k = idx.to_string();
                let child = match record {
                    Some(JsonRecord::Arr(elems)) => elems.get(idx),
                    _ => None,
                };
                let new_el = internalize_json_property(i, &val, &k, reviver, child)?;
                if matches!(new_el, Value::Undefined) {
                    json_delete_prop(i, &val, &k)?;
                } else {
                    json_create_data_prop(i, &val, &k, new_el)?;
                }
            }
        } else {
            let keys: Vec<String> = if proxy_pair(i, &val).is_some() {
                proxy_enum_string_keys(i, &val)?
                    .iter()
                    .filter_map(|k| match k {
                        Value::Str(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .collect()
            } else if let Some(info) = val.as_obj().and_then(|o| ta_info(i, o)) {
                // A TypedArray holder (a reviver may graft one in) walks its integer indices.
                (0..i.ta_len(&info).unwrap_or(0))
                    .map(|k| k.to_string())
                    .collect()
            } else {
                val.as_obj()
                    .map(|o| ordered_enum_keys(o).iter().map(|k| k.to_string()).collect())
                    .unwrap_or_default()
            };
            for k in keys {
                let child = match record {
                    Some(JsonRecord::Obj(entries)) => {
                        entries.iter().find(|(key, _)| key == &k).map(|(_, r)| r)
                    }
                    _ => None,
                };
                let new_el = internalize_json_property(i, &val, &k, reviver, child)?;
                if matches!(new_el, Value::Undefined) {
                    json_delete_prop(i, &val, &k)?;
                } else {
                    json_create_data_prop(i, &val, &k, new_el)?;
                }
            }
        }
    }
    // The `context` argument: a primitive leaf whose value is still the originally-parsed one (i.e.
    // not forward-modified by the reviver) exposes its exact source text.
    let context = i.new_object();
    if let Some(JsonRecord::Prim(src, parsed)) = record {
        if !matches!(val, Value::Obj(_)) && same_value(&val, parsed) {
            set_data(&context, "source", Value::from_string(src.clone()));
        }
    }
    ab(i.call(
        reviver.clone(),
        holder.clone(),
        &[
            Value::from_string(name.to_string()),
            val,
            Value::Obj(context),
        ],
    ))
}

fn json_str(
    i: &mut Interp,
    holder: &Value,
    key: &str,
    opts: &JsonOpts,
    gap: &str,
    indent: &str,
    seen: &mut Vec<usize>,
) -> Result<Option<String>, Value> {
    // SerializeJSONProperty: fetch the value, then apply toJSON, then the function replacer.
    let mut value = ab(i.get_member(holder, key))?;
    if matches!(value, Value::Obj(_) | Value::BigInt(_)) {
        let tojson = ab(i.get_member(&value, "toJSON"))?;
        if tojson.is_callable() {
            value = ab(i.call(
                tojson,
                value.clone(),
                &[Value::from_string(key.to_string())],
            ))?;
        }
    }
    if let Some(func) = &opts.func {
        value = ab(i.call(
            func.clone(),
            holder.clone(),
            &[Value::from_string(key.to_string()), value],
        ))?;
    }
    // A JSON.rawJSON object serializes as its stored raw text, verbatim.
    if let Value::Obj(o) = &value {
        if let Some(Value::Str(raw)) = o
            .borrow()
            .props
            .get("\u{0}raw_json")
            .map(|p| p.value.clone())
        {
            return Ok(Some(raw.to_string()));
        }
    }
    // A primitive-wrapper object re-coerces through ToNumber/ToString (so an overridden
    // valueOf/toString is observed); booleans read the wrapped datum directly.
    if let Value::Obj(o) = &value {
        let exotic = o.borrow().exotic.clone();
        match exotic {
            Exotic::NumWrap(_) => value = Value::Num(ab(i.to_number(&value))?),
            Exotic::StrWrap(_) => value = Value::Str(ab(i.to_string(&value))?),
            Exotic::BoolWrap(b) => value = Value::Bool(b),
            Exotic::BigIntWrap(_) => {
                return Err(i.make_error("TypeError", "Do not know how to serialize a BigInt"))
            }
            _ => {}
        }
    }
    match &value {
        Value::Undefined | Value::Empty | Value::Sym(_) => Ok(None),
        Value::Null => Ok(Some("null".to_string())),
        Value::Bool(b) => Ok(Some(if *b { "true" } else { "false" }.to_string())),
        Value::Num(n) => Ok(Some(if n.is_finite() {
            i.num_to_str(*n)
        } else {
            "null".to_string()
        })),
        // JSON.stringify of a BigInt throws (matches the spec).
        Value::BigInt(_) => Err(i.make_error("TypeError", "Do not know how to serialize a BigInt")),
        Value::Str(s) => Ok(Some(json_quote(s))),
        Value::Obj(o) => {
            if !matches!(o.borrow().call, Callable::None) {
                return Ok(None); // functions are omitted
            }
            let ptr = Rc::as_ptr(o) as usize;
            if seen.contains(&ptr) {
                return Err(i.make_error("TypeError", "Converting circular structure to JSON"));
            }
            seen.push(ptr);
            let new_indent = format!("{indent}{gap}");
            // IsArray sees through proxies; key enumeration / length use proxy-aware operations.
            let is_array = json_is_array(i, &value)?;
            let is_proxy = proxy_pair(i, &value).is_some();
            let result = if is_array {
                let len = if is_proxy {
                    ab(i.to_length(o))?
                } else {
                    i.array_length(o)
                };
                let mut items = Vec::with_capacity(len);
                for k in 0..len {
                    items.push(
                        json_str(i, &value, &k.to_string(), opts, gap, &new_indent, seen)?
                            .unwrap_or_else(|| "null".to_string()),
                    );
                }
                join_json("[", "]", items, gap, &new_indent, indent)
            } else {
                // An array replacer restricts the keys (in its order); else all enumerable keys.
                let keys: Vec<String> = match &opts.keys {
                    Some(list) => list.clone(),
                    None if is_proxy => proxy_enum_string_keys(i, &value)?
                        .iter()
                        .filter_map(|k| match k {
                            Value::Str(s) => Some(s.to_string()),
                            _ => None,
                        })
                        .collect(),
                    None => ordered_enum_keys(o).iter().map(|k| k.to_string()).collect(),
                };
                let mut parts = Vec::new();
                for k in &keys {
                    if let Some(vs) = json_str(i, &value, k, opts, gap, &new_indent, seen)? {
                        let colon = if gap.is_empty() { ":" } else { ": " };
                        parts.push(format!("{}{colon}{vs}", json_quote(k)));
                    }
                }
                join_json("{", "}", parts, gap, &new_indent, indent)
            };
            seen.pop();
            Ok(Some(result))
        }
    }
}

fn join_json(
    open: &str,
    close: &str,
    parts: Vec<String>,
    gap: &str,
    inner: &str,
    outer: &str,
) -> String {
    if parts.is_empty() {
        format!("{open}{close}")
    } else if gap.is_empty() {
        format!("{open}{}{close}", parts.join(","))
    } else {
        format!(
            "{open}\n{inner}{}\n{outer}{close}",
            parts.join(&format!(",\n{inner}"))
        )
    }
}

fn json_skip_ws(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() && matches!(chars[*pos], ' ' | '\t' | '\n' | '\r') {
        *pos += 1;
    }
}

fn json_parse_value(i: &mut Interp, chars: &[char], pos: &mut usize) -> Result<Value, Value> {
    json_skip_ws(chars, pos);
    let c = *chars
        .get(*pos)
        .ok_or_else(|| i.make_error("SyntaxError", "Unexpected end of JSON input"))?;
    match c {
        '{' => {
            *pos += 1;
            let obj = i.new_object();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&'}') {
                *pos += 1;
                return Ok(Value::Obj(obj));
            }
            loop {
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&'"') {
                    return Err(i.make_error("SyntaxError", "Expected string key in JSON object"));
                }
                let key = json_parse_string(i, chars, pos)?;
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&':') {
                    return Err(i.make_error("SyntaxError", "Expected ':' in JSON object"));
                }
                *pos += 1;
                let v = json_parse_value(i, chars, pos)?;
                set_data(&obj, &key, v);
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => {
                        *pos += 1;
                    }
                    Some('}') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(
                            i.make_error("SyntaxError", "Expected ',' or '}' in JSON object")
                        )
                    }
                }
            }
            Ok(Value::Obj(obj))
        }
        '[' => {
            *pos += 1;
            let mut items = Vec::new();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&']') {
                *pos += 1;
                return Ok(i.make_array(items));
            }
            loop {
                items.push(json_parse_value(i, chars, pos)?);
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => {
                        *pos += 1;
                    }
                    Some(']') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(i.make_error("SyntaxError", "Expected ',' or ']' in JSON array"))
                    }
                }
            }
            Ok(i.make_array(items))
        }
        '"' => Ok(Value::from_string(json_parse_string(i, chars, pos)?)),
        't' => json_parse_lit(i, chars, pos, "true", Value::Bool(true)),
        'f' => json_parse_lit(i, chars, pos, "false", Value::Bool(false)),
        'n' => json_parse_lit(i, chars, pos, "null", Value::Null),
        '-' | '0'..='9' => {
            let start = *pos;
            // Strict JSON number grammar: -?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)? — no leading
            // zeros, a mandatory integer part, and at least one digit after `.` / exponent.
            let err = || i.make_error("SyntaxError", "Invalid number in JSON");
            let at = |p: usize| chars.get(p).copied();
            if at(*pos) == Some('-') {
                *pos += 1;
            }
            match at(*pos) {
                Some('0') => *pos += 1,
                Some('1'..='9') => {
                    while matches!(at(*pos), Some('0'..='9')) {
                        *pos += 1;
                    }
                }
                _ => return Err(err()),
            }
            if at(*pos) == Some('.') {
                *pos += 1;
                if !matches!(at(*pos), Some('0'..='9')) {
                    return Err(err());
                }
                while matches!(at(*pos), Some('0'..='9')) {
                    *pos += 1;
                }
            }
            if matches!(at(*pos), Some('e' | 'E')) {
                *pos += 1;
                if matches!(at(*pos), Some('+' | '-')) {
                    *pos += 1;
                }
                if !matches!(at(*pos), Some('0'..='9')) {
                    return Err(err());
                }
                while matches!(at(*pos), Some('0'..='9')) {
                    *pos += 1;
                }
            }
            let s: String = chars[start..*pos].iter().collect();
            s.parse::<f64>().map(Value::Num).map_err(|_| err())
        }
        _ => Err(i.make_error("SyntaxError", "Unexpected token in JSON")),
    }
}

/// A parallel parse tree recording the source text of every primitive leaf, so the JSON.parse
/// reviver can receive a `context` argument with a `source` property (ES2025 source-text access).
enum JsonRecord {
    Prim(String, Value),
    Arr(Vec<JsonRecord>),
    Obj(Vec<(String, JsonRecord)>),
}

/// Mirror of `json_parse_value` that also returns a `JsonRecord`. Containers are framed inline;
/// primitive leaves delegate to `json_parse_value` and capture their exact source span.
fn json_parse_recorded(
    i: &mut Interp,
    chars: &[char],
    pos: &mut usize,
) -> Result<(Value, JsonRecord), Value> {
    json_skip_ws(chars, pos);
    let c = *chars
        .get(*pos)
        .ok_or_else(|| i.make_error("SyntaxError", "Unexpected end of JSON input"))?;
    match c {
        '{' => {
            *pos += 1;
            let obj = i.new_object();
            let mut rec: Vec<(String, JsonRecord)> = Vec::new();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&'}') {
                *pos += 1;
                return Ok((Value::Obj(obj), JsonRecord::Obj(rec)));
            }
            loop {
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&'"') {
                    return Err(i.make_error("SyntaxError", "Expected string key in JSON object"));
                }
                let key = json_parse_string(i, chars, pos)?;
                json_skip_ws(chars, pos);
                if chars.get(*pos) != Some(&':') {
                    return Err(i.make_error("SyntaxError", "Expected ':' in JSON object"));
                }
                *pos += 1;
                let (v, vr) = json_parse_recorded(i, chars, pos)?;
                set_data(&obj, &key, v);
                rec.retain(|(k, _)| k != &key);
                rec.push((key, vr));
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => *pos += 1,
                    Some('}') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(
                            i.make_error("SyntaxError", "Expected ',' or '}' in JSON object")
                        )
                    }
                }
            }
            Ok((Value::Obj(obj), JsonRecord::Obj(rec)))
        }
        '[' => {
            *pos += 1;
            let mut items = Vec::new();
            let mut rec = Vec::new();
            json_skip_ws(chars, pos);
            if chars.get(*pos) == Some(&']') {
                *pos += 1;
                return Ok((i.make_array(items), JsonRecord::Arr(rec)));
            }
            loop {
                let (v, vr) = json_parse_recorded(i, chars, pos)?;
                items.push(v);
                rec.push(vr);
                json_skip_ws(chars, pos);
                match chars.get(*pos) {
                    Some(',') => *pos += 1,
                    Some(']') => {
                        *pos += 1;
                        break;
                    }
                    _ => {
                        return Err(i.make_error("SyntaxError", "Expected ',' or ']' in JSON array"))
                    }
                }
            }
            Ok((i.make_array(items), JsonRecord::Arr(rec)))
        }
        _ => {
            let start = *pos;
            let v = json_parse_value(i, chars, pos)?;
            let src: String = chars[start..*pos].iter().collect();
            Ok((v.clone(), JsonRecord::Prim(src, v)))
        }
    }
}

fn json_parse_lit(
    i: &mut Interp,
    chars: &[char],
    pos: &mut usize,
    lit: &str,
    val: Value,
) -> Result<Value, Value> {
    for expect in lit.chars() {
        if chars.get(*pos) != Some(&expect) {
            return Err(i.make_error("SyntaxError", "Invalid literal in JSON"));
        }
        *pos += 1;
    }
    Ok(val)
}

fn json_parse_string(i: &mut Interp, chars: &[char], pos: &mut usize) -> Result<String, Value> {
    *pos += 1; // opening quote
    let mut s = String::new();
    loop {
        let c = *chars
            .get(*pos)
            .ok_or_else(|| i.make_error("SyntaxError", "Unterminated JSON string"))?;
        *pos += 1;
        match c {
            '"' => return Ok(s),
            '\\' => {
                let e = *chars
                    .get(*pos)
                    .ok_or_else(|| i.make_error("SyntaxError", "Bad escape in JSON"))?;
                *pos += 1;
                match e {
                    '"' => s.push('"'),
                    '\\' => s.push('\\'),
                    '/' => s.push('/'),
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    'b' => s.push('\u{0008}'),
                    'f' => s.push('\u{000C}'),
                    'u' => {
                        let hex: String = chars[*pos..(*pos + 4).min(chars.len())].iter().collect();
                        *pos += 4;
                        let n = u32::from_str_radix(&hex, 16)
                            .map_err(|_| i.make_error("SyntaxError", "Bad \\u escape in JSON"))?;
                        if (0xD800..0xDC00).contains(&n)
                            && chars.get(*pos) == Some(&'\\')
                            && chars.get(*pos + 1) == Some(&'u')
                        {
                            // A high surrogate followed by \uDCxx forms a pair.
                            let hex2: String = chars
                                [(*pos + 2).min(chars.len())..(*pos + 6).min(chars.len())]
                                .iter()
                                .collect();
                            if let Ok(n2) = u32::from_str_radix(&hex2, 16) {
                                if (0xDC00..0xE000).contains(&n2) {
                                    *pos += 6;
                                    let c = 0x10000 + ((n - 0xD800) << 10) + (n2 - 0xDC00);
                                    s.push(char::from_u32(c).unwrap());
                                    continue;
                                }
                            }
                        }
                        if (0xD800..0xE000).contains(&n) {
                            s.push(crate::jstr::smuggle(n as u16));
                        } else {
                            s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                        }
                    }
                    _ => return Err(i.make_error("SyntaxError", "Bad escape in JSON")),
                }
            }
            // Unescaped control characters (U+0000–U+001F) are not allowed in JSON strings.
            c if (c as u32) < 0x20 => {
                return Err(
                    i.make_error("SyntaxError", "Unescaped control character in JSON string")
                )
            }
            c => s.push(c),
        }
    }
}
