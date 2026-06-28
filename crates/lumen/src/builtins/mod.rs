//! The built-in objects and global functions. This is the realm: a freshly-constructed [`Interp`]
//! calls [`install`] to populate `globalThis`, the standard constructors/prototypes, `Math`, and
//! the global functions. The set grows as the test262 score climbs — it is intentionally a subset.

use crate::interpreter::{Abrupt, Interp, MAX_ARRAY_OP_LEN, MAX_STR_LEN};
use crate::value::*;
use std::cmp::Ordering;
use std::rc::Rc;

/// `args[i]` or `undefined`.
fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

/// Map an `Abrupt` (which, from inside a native function, can only be a `Throw`) to its value so it
/// fits the native `Result<_, Value>` contract.
fn ab<T>(r: Result<T, Abrupt>) -> Result<T, Value> {
    r.map_err(|a| match a {
        Abrupt::Throw(v) => v,
        _ => Value::Undefined,
    })
}

fn this_obj(this: &Value) -> Option<Gc> {
    this.as_obj().cloned()
}

pub fn install(it: &mut Interp) {
    // Primitive globals.
    let g = it.global.clone();
    set_builtin(&g, "undefined", Value::Undefined);
    set_data(&g, "NaN", Value::Num(f64::NAN));
    set_data(&g, "Infinity", Value::Num(f64::INFINITY));
    g.borrow_mut().props.get_mut("NaN").unwrap().writable = false;
    g.borrow_mut().props.get_mut("Infinity").unwrap().writable = false;
    set_builtin(&g, "globalThis", Value::Obj(g.clone()));

    install_function_proto(it);
    install_object(it);
    // Symbol before Array/String so `Symbol.iterator` exists when they define `@@iterator`.
    install_symbol(it);
    install_iterator(it);
    install_array(it);
    install_string(it);
    install_number(it);
    install_boolean(it);
    install_bigint(it);
    install_math(it);
    install_errors(it);
    install_reflect(it);
    install_proxy(it);
    install_promise(it);
    install_json(it);
    install_collections(it);
    install_date(it);
    install_typed_arrays(it);
    install_dataview(it);
    install_shared_array_buffer(it);
    install_regexp(it);
    install_globals(it);
    install_console(it);
    install_host(it);
    install_atomics(it);
    install_weak_refs(it);
    crate::temporal::install(it);
}

/// WeakRef / FinalizationRegistry. lumen's collector never observably reclaims during a test, so
/// WeakRef holds its target (deref always returns it) and FinalizationRegistry callbacks never fire.
fn install_weak_refs(it: &mut Interp) {
    let wr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&wr_proto, "deref", 0, |i, this, _| ab(i.get_member(&this, "__target")));
    let wr_ctor = it.make_native("WeakRef", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "WeakRef requires 'new'"));
        }
        let target = arg(a, 0);
        if !matches!(target, Value::Obj(_) | Value::Sym(_)) {
            return Err(i.make_error("TypeError", "WeakRef target must be an object or symbol"));
        }
        let obj = Object::new(i.extra_protos.get("WeakRef").cloned());
        set_internal(&obj, "__target", target);
        Ok(Value::Obj(obj))
    });
    it.extra_protos.insert("WeakRef", wr_proto.clone());
    wr_ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(wr_proto.clone()), false, false, false));
    wr_proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(wr_ctor.clone())));
    set_builtin(&it.global, "WeakRef", Value::Obj(wr_ctor));

    let fr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&fr_proto, "register", 2, |i, _this, a| {
        if matches!(arg(a, 0), Value::Undefined) {
            return Err(i.make_error("TypeError", "target must be an object"));
        }
        Ok(Value::Undefined)
    });
    it.def_method(&fr_proto, "unregister", 1, |_i, _this, _a| Ok(Value::Bool(false)));
    let fr_ctor = it.make_native("FinalizationRegistry", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "FinalizationRegistry requires 'new'"));
        }
        if !arg(a, 0).is_callable() {
            return Err(i.make_error("TypeError", "cleanup callback must be callable"));
        }
        Ok(Value::Obj(Object::new(i.extra_protos.get("FinalizationRegistry").cloned())))
    });
    it.extra_protos.insert("FinalizationRegistry", fr_proto.clone());
    fr_ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(fr_proto.clone()), false, false, false));
    fr_proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(fr_ctor.clone())));
    set_builtin(&it.global, "FinalizationRegistry", Value::Obj(fr_ctor));
}

/// `Atomics` over integer TypedArrays. lumen is single-threaded, so the read-modify-write ops are
/// plain operations; `wait`/`notify` are no-ops.
fn install_atomics(it: &mut Interp) {
    let atomics = Object::new(Some(it.object_proto.clone()));

    fn target(i: &mut Interp, args: &[Value]) -> Result<(TaInfo, usize), Value> {
        let ptr = map_ptr(&arg(args, 0))
            .ok_or_else(|| i.make_error("TypeError", "Atomics: not an integer TypedArray"))?;
        let info = *i
            .typed_arrays
            .get(&ptr)
            .ok_or_else(|| i.make_error("TypeError", "Atomics: not an integer TypedArray"))?;
        if matches!(info.kind, TaKind::F32 | TaKind::F64 | TaKind::U8Clamped) {
            return Err(i.make_error("TypeError", "Atomics requires an integer TypedArray"));
        }
        let idx = ab(i.to_number(&arg(args, 1)))?;
        if !idx.is_finite() || idx.fract() != 0.0 || idx < 0.0 || idx as usize >= info.len {
            return Err(i.make_error("RangeError", "Atomics: index out of range"));
        }
        Ok((info, idx as usize))
    }
    fn operand(i: &mut Interp, info: &TaInfo, v: &Value) -> Result<i128, Value> {
        if info.kind.is_bigint() {
            ab(i.to_bigint(v))
        } else {
            let n = ab(i.to_number(v))?;
            Ok(if n.is_finite() { n.trunc() as i128 } else { 0 })
        }
    }
    fn read_i128(i: &Interp, info: &TaInfo, idx: usize) -> i128 {
        match i.ta_read(info, idx) {
            Value::Num(n) => n as i128,
            Value::BigInt(n) => n,
            _ => 0,
        }
    }
    fn write_i128(i: &mut Interp, info: &TaInfo, idx: usize, n: i128) {
        if info.kind.is_bigint() {
            i.ta_write_bigint(info, idx, n);
        } else {
            i.ta_write(info, idx, n as f64);
        }
    }
    fn wrap(i: &mut Interp, info: &TaInfo, idx: usize, n: i128) -> Value {
        // Read back so the value reflects the element type's wrapping.
        write_i128(i, info, idx, n);
        i.ta_read(info, idx)
    }
    fn rmw(i: &mut Interp, args: &[Value], f: fn(i128, i128) -> i128) -> Result<Value, Value> {
        let (info, idx) = target(i, args)?;
        let val = operand(i, &info, &arg(args, 2))?;
        let old = read_i128(i, &info, idx);
        let old_val = i.ta_read(&info, idx);
        write_i128(i, &info, idx, f(old, val));
        Ok(old_val)
    }

    it.def_method(&atomics, "add", 3, |i, _t, a| rmw(i, a, |o, v| o + v));
    it.def_method(&atomics, "sub", 3, |i, _t, a| rmw(i, a, |o, v| o - v));
    it.def_method(&atomics, "and", 3, |i, _t, a| rmw(i, a, |o, v| o & v));
    it.def_method(&atomics, "or", 3, |i, _t, a| rmw(i, a, |o, v| o | v));
    it.def_method(&atomics, "xor", 3, |i, _t, a| rmw(i, a, |o, v| o ^ v));
    it.def_method(&atomics, "exchange", 3, |i, _t, a| rmw(i, a, |_o, v| v));
    it.def_method(&atomics, "load", 2, |i, _t, a| {
        let (info, idx) = target(i, a)?;
        Ok(i.ta_read(&info, idx))
    });
    it.def_method(&atomics, "store", 3, |i, _t, a| {
        let (info, idx) = target(i, a)?;
        let val = operand(i, &info, &arg(a, 2))?;
        Ok(wrap(i, &info, idx, val))
    });
    it.def_method(&atomics, "compareExchange", 4, |i, _t, a| {
        let (info, idx) = target(i, a)?;
        let expected = operand(i, &info, &arg(a, 2))?;
        let replacement = operand(i, &info, &arg(a, 3))?;
        let old = read_i128(i, &info, idx);
        let old_val = i.ta_read(&info, idx);
        if old == expected {
            write_i128(i, &info, idx, replacement);
        }
        Ok(old_val)
    });
    it.def_method(&atomics, "isLockFree", 1, |i, _t, a| {
        let n = ab(i.to_number(&arg(a, 0)))?;
        Ok(Value::Bool(matches!(n as i64, 1 | 2 | 4 | 8)))
    });
    it.def_method(&atomics, "wait", 4, |i, _t, a| {
        let (info, idx) = target(i, a)?;
        let expected = operand(i, &info, &arg(a, 2))?;
        // Single-threaded: never blocks; report whether the value already differs.
        Ok(Value::str(if read_i128(i, &info, idx) == expected { "timed-out" } else { "not-equal" }))
    });
    it.def_method(&atomics, "notify", 3, |i, _t, a| {
        target(i, a)?;
        Ok(Value::Num(0.0)) // no agents are waiting
    });
    set_builtin(&it.global, "Atomics", Value::Obj(atomics));
}

/// The test262 `$262` host object. Only the portions lumen can support are provided (`global`,
/// `gc`, `evalScript`, best-effort `detachArrayBuffer`); `agent`/`createRealm` are omitted.
fn install_host(it: &mut Interp) {
    let host = Object::new(Some(it.object_proto.clone()));
    set_builtin(&host, "global", Value::Obj(it.global.clone()));
    it.def_method(&host, "gc", 0, |i, _t, _a| {
        i.gc_collect();
        Ok(Value::Undefined)
    });
    it.def_method(&host, "evalScript", 1, |i, _t, args| {
        let code = match arg(args, 0) {
            Value::Str(s) => s,
            other => return Ok(other),
        };
        let body = crate::parser::parse_script(&code, false)
            .map_err(|e| i.make_error("SyntaxError", e.message))?;
        let env = i.global_env.clone();
        ab(i.eval_in_scope(&body, &env))
    });
    it.def_method(&host, "detachArrayBuffer", 1, |i, _t, args| {
        if let Value::Obj(o) = arg(args, 0) {
            let p = Rc::as_ptr(&o) as usize;
            if let Some(buf) = i.array_buffers.get_mut(&p) {
                buf.clear();
            }
            set_internal(&o, "byteLength", Value::Num(0.0));
        }
        Ok(Value::Undefined)
    });
    set_builtin(&it.global, "$262", Value::Obj(host));
}

fn dv_get(i: &mut Interp, this: &Value, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len) = *i.data_views.get(&ptr).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = ab(i.to_number(&arg(args, 0)))? as usize;
    let little = i.to_boolean(&arg(args, 1));
    let es = kind.elsize();
    if byte_off + es > len {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let start = off + byte_off;
    let mut b = match i.array_buffers.get(&buf) {
        Some(buf) if start + es <= buf.len() => buf[start..start + es].to_vec(),
        _ => return Err(i.make_error("TypeError", "detached buffer")),
    };
    // `kind.read` is little-endian; DataView defaults to big-endian, so reverse unless little.
    if !little {
        b.reverse();
    }
    Ok(Value::Num(kind.read(&b)))
}

fn dv_set(i: &mut Interp, this: &Value, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len) = *i.data_views.get(&ptr).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = ab(i.to_number(&arg(args, 0)))? as usize;
    let value = ab(i.to_number(&arg(args, 1)))?;
    let little = i.to_boolean(&arg(args, 2));
    let es = kind.elsize();
    if byte_off + es > len {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let mut bytes = kind.write(value);
    if !little {
        bytes.reverse();
    }
    let start = off + byte_off;
    if let Some(b) = i.array_buffers.get_mut(&buf) {
        if start + es <= b.len() {
            b[start..start + es].copy_from_slice(&bytes);
        }
    }
    Ok(Value::Undefined)
}

fn install_dataview(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("DataView", proto.clone());
    macro_rules! dvm {
        ($getname:expr, $setname:expr, $kind:expr) => {
            it.def_method(&proto, $getname, 1, |i, this, a| dv_get(i, &this, a, $kind));
            it.def_method(&proto, $setname, 2, |i, this, a| dv_set(i, &this, a, $kind));
        };
    }
    dvm!("getInt8", "setInt8", TaKind::I8);
    dvm!("getUint8", "setUint8", TaKind::U8);
    dvm!("getInt16", "setInt16", TaKind::I16);
    dvm!("getUint16", "setUint16", TaKind::U16);
    dvm!("getInt32", "setInt32", TaKind::I32);
    dvm!("getUint32", "setUint32", TaKind::U32);
    dvm!("getFloat32", "setFloat32", TaKind::F32);
    dvm!("getFloat64", "setFloat64", TaKind::F64);

    let ctor = it.make_native("DataView", 1, |i, _t, a| {
        let bp = match arg(a, 0) {
            Value::Obj(o) if i.array_buffers.contains_key(&(Rc::as_ptr(&o) as usize)) => {
                Rc::as_ptr(&o) as usize
            }
            _ => return Err(i.make_error("TypeError", "DataView requires an ArrayBuffer")),
        };
        let buflen = i.array_buffers[&bp].len();
        let offset = match arg(a, 1) {
            Value::Undefined => 0,
            v => ab(i.to_number(&v))? as usize,
        };
        let len = match arg(a, 2) {
            Value::Undefined => buflen.saturating_sub(offset),
            v => ab(i.to_number(&v))? as usize,
        };
        let obj = Object::new(i.extra_protos.get("DataView").cloned());
        let p = Rc::as_ptr(&obj) as usize;
        i.data_views.insert(p, (bp, offset, len));
        set_internal(&obj, "buffer", arg(a, 0));
        set_internal(&obj, "byteOffset", Value::Num(offset as f64));
        set_internal(&obj, "byteLength", Value::Num(len as f64));
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "DataView", Value::Obj(ctor));
}

fn install_regexp(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("RegExp", proto.clone());
    it.def_method(&proto, "exec", 1, regexp_exec);
    it.def_method(&proto, "test", 1, |i, this, a| {
        Ok(Value::Bool(!matches!(regexp_exec(i, this, a)?, Value::Null)))
    });
    it.def_method(&proto, "toString", 0, |i, this, _| {
        let src_v = ab(i.get_member(&this, "source"))?;
        let src = ab(i.to_string(&src_v))?;
        let flags_v = ab(i.get_member(&this, "flags"))?;
        let flags = ab(i.to_string(&flags_v))?;
        Ok(Value::from_string(format!("/{src}/{flags}")))
    });
    let ctor = it.make_native("RegExp", 2, |i, _t, a| {
        let (source, flags) = match arg(a, 0) {
            Value::Obj(o) if i.regexps.contains_key(&(Rc::as_ptr(&o) as usize)) => {
                let re = i.regexps[&(Rc::as_ptr(&o) as usize)].clone();
                let fl = match arg(a, 1) {
                    Value::Undefined => re.flags.clone(),
                    v => ab(i.to_string(&v))?.to_string(),
                };
                (re.source.clone(), fl)
            }
            Value::Undefined => (
                String::new(),
                match arg(a, 1) {
                    Value::Undefined => String::new(),
                    v => ab(i.to_string(&v))?.to_string(),
                },
            ),
            v => {
                let src = ab(i.to_string(&v))?.to_string();
                let fl = match arg(a, 1) {
                    Value::Undefined => String::new(),
                    v => ab(i.to_string(&v))?.to_string(),
                };
                (src, fl)
            }
        };
        ab(i.make_regexp(&source, &flags))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "RegExp", Value::Obj(ctor));
}

/// `RegExp.prototype.exec`: returns the match array (with `index`/`input`) or `null`, advancing
/// `lastIndex` for global/sticky regexes.
fn regexp_exec(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "exec on non-RegExp"))?;
    let re = i.regexps.get(&ptr).cloned().ok_or_else(|| i.make_error("TypeError", "exec on non-RegExp"))?;
    let input = ab(i.to_string(&arg(args, 0)))?;
    let chars: Vec<char> = input.chars().collect();
    let use_last = re.global || re.sticky;
    let last = if use_last {
        let li = ab(i.get_member(&this, "lastIndex"))?;
        ab(i.to_number(&li))?.max(0.0) as usize
    } else {
        0
    };
    if last > chars.len() {
        if use_last {
            ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
        }
        return Ok(Value::Null);
    }
    match re.exec_at(&chars, last) {
        None => {
            if use_last {
                ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
            }
            Ok(Value::Null)
        }
        Some(caps) => {
            let (start, end) = caps[0].unwrap();
            if use_last {
                ab(i.set_member(&this, "lastIndex", Value::Num(end as f64)))?;
            }
            let mut items = vec![Value::from_string(chars[start..end].iter().collect::<String>())];
            for g in 1..=re.ngroups {
                items.push(match caps[g] {
                    Some((a, b)) => Value::from_string(chars[a..b].iter().collect::<String>()),
                    None => Value::Undefined,
                });
            }
            let result = i.make_array(items);
            if let Value::Obj(o) = &result {
                set_data(o, "index", Value::Num(start as f64));
                set_data(o, "input", Value::Str(input));
            }
            Ok(result)
        }
    }
}

/// Coerce `v` to a RegExp object (returning it unchanged if already one).
/// `$&` / `$$` / `$1`..`$99` / `` $` `` / `$'` substitution in a string replacement template.
fn expand_dollar(template: &str, caps: &[Option<(usize, usize)>], chars: &[char]) -> String {
    let (ms, me) = caps[0].unwrap();
    let group = |g: usize| -> String {
        caps.get(g).and_then(|c| *c).map(|(a, b)| chars[a..b].iter().collect()).unwrap_or_default()
    };
    let t: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < t.len() {
        if t[i] == '$' && i + 1 < t.len() {
            match t[i + 1] {
                '$' => {
                    out.push('$');
                    i += 2;
                }
                '&' => {
                    out.extend(chars[ms..me].iter());
                    i += 2;
                }
                '`' => {
                    out.extend(chars[..ms].iter());
                    i += 2;
                }
                '\'' => {
                    out.extend(chars[me..].iter());
                    i += 2;
                }
                d if d.is_ascii_digit() => {
                    // One or two digits (prefer two if that group exists).
                    let mut g = d.to_digit(10).unwrap() as usize;
                    let mut consumed = 2;
                    if i + 2 < t.len() && t[i + 2].is_ascii_digit() {
                        let two = g * 10 + t[i + 2].to_digit(10).unwrap() as usize;
                        if two < caps.len() {
                            g = two;
                            consumed = 3;
                        }
                    }
                    if g >= 1 && g < caps.len() {
                        out.push_str(&group(g));
                        i += consumed;
                    } else {
                        out.push('$');
                        i += 1;
                    }
                }
                _ => {
                    out.push('$');
                    i += 1;
                }
            }
        } else {
            out.push(t[i]);
            i += 1;
        }
    }
    out
}

fn regex_replace(i: &mut Interp, s: &str, re_obj: &Value, repl: &Value) -> Result<Value, Value> {
    let re = i.regexps[&map_ptr(re_obj).unwrap()].clone();
    let chars: Vec<char> = s.chars().collect();
    let matches = if re.global {
        regex_find_all(&re, &chars)
    } else {
        match re.exec_at(&chars, 0) {
            Some(c) => vec![c],
            None => Vec::new(),
        }
    };
    let mut out = String::new();
    let mut last = 0;
    for caps in &matches {
        let (a, b) = caps[0].unwrap();
        out.extend(chars[last..a].iter());
        if repl.is_callable() {
            let matched: String = chars[a..b].iter().collect();
            let mut cbargs = vec![Value::from_string(matched)];
            for g in 1..=re.ngroups {
                cbargs.push(match caps[g] {
                    Some((x, y)) => Value::from_string(chars[x..y].iter().collect::<String>()),
                    None => Value::Undefined,
                });
            }
            cbargs.push(Value::Num(a as f64));
            cbargs.push(Value::from_string(s.to_string()));
            let r = ab(i.call(repl.clone(), Value::Undefined, &cbargs))?;
            out.push_str(&ab(i.to_string(&r))?);
        } else {
            let template = ab(i.to_string(repl))?;
            out.push_str(&expand_dollar(&template, caps, &chars));
        }
        last = b;
    }
    out.extend(chars[last..].iter());
    Ok(Value::from_string(out))
}

/// Coerce `v` to a RegExp object (returning it unchanged if already one).
fn coerce_regexp(i: &mut Interp, v: Value) -> Result<Value, Value> {
    match &v {
        Value::Obj(o) if i.regexps.contains_key(&(Rc::as_ptr(o) as usize)) => Ok(v),
        Value::Undefined => ab(i.make_regexp("", "")),
        _ => {
            let s = ab(i.to_string(&v))?.to_string();
            ab(i.make_regexp(&s, ""))
        }
    }
}

/// All non-overlapping matches of `re` in `chars`, each as capture spans.
fn regex_find_all(re: &crate::regex::Regex, chars: &[char]) -> Vec<Vec<Option<(usize, usize)>>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= chars.len() {
        match re.exec_at(chars, pos) {
            None => break,
            Some(caps) => {
                let (a, b) = caps[0].unwrap();
                pos = if b > a { b } else { b + 1 };
                out.push(caps);
                if out.len() > MAX_ARRAY_OP_LEN {
                    break;
                }
            }
        }
    }
    out
}

fn install_shared_array_buffer(it: &mut Interp) {
    // Modeled as a plain ArrayBuffer (no real sharing) — enough for tests that just need the type.
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("SharedArrayBuffer", proto.clone());
    it.def_method(&proto, "slice", 2, |i, this, a| {
        let ptr = this.as_obj().map(|o| Rc::as_ptr(o) as usize);
        let bytes = ptr.and_then(|p| i.array_buffers.get(&p)).cloned().unwrap_or_default();
        let len = bytes.len() as i64;
        let begin = norm_index(ab(i.to_number(&arg(a, 0)))?, len, 0);
        let end = match arg(a, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let slice = if begin < end { bytes[begin as usize..end as usize].to_vec() } else { Vec::new() };
        let (bv, bp) = make_array_buffer(i, slice.len());
        if let Some(buf) = i.array_buffers.get_mut(&bp) {
            buf.copy_from_slice(&slice);
        }
        Ok(bv)
    });
    let ctor = it.make_native("SharedArrayBuffer", 1, |i, _t, a| {
        let len = ab(i.to_number(&arg(a, 0)))?.max(0.0) as usize;
        if len > MAX_ARRAY_OP_LEN {
            return Err(i.make_error("RangeError", "Invalid SharedArrayBuffer length"));
        }
        // Stamp the SharedArrayBuffer prototype rather than ArrayBuffer's.
        let (bv, _) = make_array_buffer(i, len);
        if let Value::Obj(o) = &bv {
            o.borrow_mut().proto = i.extra_protos.get("SharedArrayBuffer").cloned();
        }
        Ok(bv)
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "SharedArrayBuffer", Value::Obj(ctor));
}

fn uri_encode(s: &str, keep: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) || keep.contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn uri_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------------------------
// ArrayBuffer + TypedArrays. Backing bytes live in `Interp::array_buffers`; each view's state in
// `Interp::typed_arrays`. Integer-index get/set is wired in `get_member`/`set_member`; the named
// metadata (length/byteLength/byteOffset/buffer/BYTES_PER_ELEMENT) is stored as real own props.
// ---------------------------------------------------------------------------------------------

fn make_array_buffer(i: &mut Interp, byte_len: usize) -> (Value, usize) {
    let obj = Object::new(i.extra_protos.get("ArrayBuffer").cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.array_buffers.insert(p, vec![0u8; byte_len]);
    set_internal(&obj, "byteLength", Value::Num(byte_len as f64));
    set_internal(&obj, "maxByteLength", Value::Num(byte_len as f64));
    set_internal(&obj, "resizable", Value::Bool(false));
    set_internal(&obj, "detached", Value::Bool(false));
    (Value::Obj(obj), p)
}

fn install_array_buffer(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("ArrayBuffer", proto.clone());
    it.def_method(&proto, "slice", 2, |i, this, a| {
        let ptr = this.as_obj().map(|o| Rc::as_ptr(o) as usize);
        let bytes = ptr.and_then(|p| i.array_buffers.get(&p)).cloned().unwrap_or_default();
        let len = bytes.len() as i64;
        let begin = norm_index(ab(i.to_number(&arg(a, 0)))?, len, 0);
        let end = match arg(a, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let slice = if begin < end { bytes[begin as usize..end as usize].to_vec() } else { Vec::new() };
        let (bv, bp) = make_array_buffer(i, slice.len());
        if let Some(buf) = i.array_buffers.get_mut(&bp) {
            buf.copy_from_slice(&slice);
        }
        Ok(bv)
    });
    it.def_method(&proto, "resize", 1, |i, this, a| {
        let o = this.as_obj().cloned().ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
        let rv = ab(i.get_member(&this, "resizable"))?;
        if !i.to_boolean(&rv) {
            return Err(i.make_error("TypeError", "ArrayBuffer is not resizable"));
        }
        let mv = ab(i.get_member(&this, "maxByteLength"))?;
        let max = ab(i.to_number(&mv))? as usize;
        let new_len = ab(i.to_number(&arg(a, 0)))?;
        if !new_len.is_finite() || new_len < 0.0 || new_len as usize > max {
            return Err(i.make_error("RangeError", "ArrayBuffer resize out of range"));
        }
        let n = new_len as usize;
        if let Some(buf) = i.array_buffers.get_mut(&(Rc::as_ptr(&o) as usize)) {
            buf.resize(n, 0);
        }
        set_internal(&o, "byteLength", Value::Num(n as f64));
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "transfer", 1, |i, this, a| {
        let o = this.as_obj().cloned().ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
        let bytes = i.array_buffers.get(&(Rc::as_ptr(&o) as usize)).cloned().unwrap_or_default();
        let new_len = match arg(a, 0) {
            Value::Undefined => bytes.len(),
            v => ab(i.to_number(&v))?.max(0.0) as usize,
        };
        let (bv, bp) = make_array_buffer(i, new_len);
        if let Some(buf) = i.array_buffers.get_mut(&bp) {
            let n = bytes.len().min(new_len);
            buf[..n].copy_from_slice(&bytes[..n]);
        }
        // Detach the source.
        i.array_buffers.insert(Rc::as_ptr(&o) as usize, Vec::new());
        set_internal(&o, "byteLength", Value::Num(0.0));
        set_internal(&o, "detached", Value::Bool(true));
        Ok(bv)
    });
    let ctor = it.make_native("ArrayBuffer", 1, |i, _t, a| {
        let len = ab(i.to_number(&arg(a, 0)))?.max(0.0) as usize;
        if len > MAX_ARRAY_OP_LEN {
            return Err(i.make_error("RangeError", "Invalid ArrayBuffer length"));
        }
        let (bv, _) = make_array_buffer(i, len);
        // The options bag's maxByteLength makes the buffer resizable.
        if let Value::Obj(_) = arg(a, 1) {
            let mbl = ab(i.get_member(&arg(a, 1), "maxByteLength"))?;
            if !matches!(mbl, Value::Undefined) {
                let m = ab(i.to_number(&mbl))?;
                if !m.is_finite() || (m as usize) < len || m as usize > MAX_ARRAY_OP_LEN {
                    return Err(i.make_error("RangeError", "Invalid maxByteLength"));
                }
                if let Value::Obj(o) = &bv {
                    set_internal(o, "maxByteLength", Value::Num(m));
                    set_internal(o, "resizable", Value::Bool(true));
                }
            }
        }
        Ok(bv)
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "isView", 1, |i, _t, a| {
        let is_view = match arg(a, 0) {
            Value::Obj(o) => i.typed_arrays.contains_key(&(Rc::as_ptr(&o) as usize)),
            _ => false,
        };
        Ok(Value::Bool(is_view))
    });
    set_builtin(&it.global, "ArrayBuffer", Value::Obj(ctor));
}

/// A %TypedArray%.prototype method: brand-check the receiver, then delegate to the like-named
/// Array.prototype method.
fn ta_delegate(i: &mut Interp, this: &Value, method: &str, args: &[Value]) -> Result<Value, Value> {
    let ok = map_ptr(this).map(|p| i.typed_arrays.contains_key(&p)).unwrap_or(false);
    if !ok {
        return Err(i.make_error("TypeError", "method called on a non-TypedArray receiver"));
    }
    let f = it_array_method(i, method);
    ab(i.call(f, this.clone(), args))
}
fn it_array_method(i: &Interp, method: &str) -> Value {
    i.array_proto.borrow().props.get(method).map(|p| p.value.clone()).unwrap_or(Value::Undefined)
}
macro_rules! ta_methods {
    ($($name:literal => $fname:ident),* $(,)?) => {
        $(fn $fname(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
            ta_delegate(i, &this, $name, a)
        })*
        const TA_METHODS: &[(&str, NativeFn)] = &[$(($name, $fname)),*];
    };
}
ta_methods! {
    "forEach" => tad_foreach, "map" => tad_map, "filter" => tad_filter, "reduce" => tad_reduce,
    "reduceRight" => tad_reduceright, "some" => tad_some, "every" => tad_every, "find" => tad_find,
    "findIndex" => tad_findindex, "findLast" => tad_findlast, "findLastIndex" => tad_findlastindex,
    "indexOf" => tad_indexof, "lastIndexOf" => tad_lastindexof, "includes" => tad_includes,
    "join" => tad_join, "fill" => tad_fill, "reverse" => tad_reverse, "sort" => tad_sort,
    "slice" => tad_slice, "at" => tad_at, "keys" => tad_keys, "values" => tad_values,
    "entries" => tad_entries, "toString" => tad_tostring, "copyWithin" => tad_copywithin,
    "toReversed" => tad_toreversed, "toSorted" => tad_tosorted, "with" => tad_with,
}

fn ta_construct(i: &mut Interp, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    let es = kind.elsize();
    let (buf_val, buf_ptr, offset, len) = match args.first() {
        None => {
            let (bv, bp) = make_array_buffer(i, 0);
            (bv, bp, 0, 0)
        }
        Some(Value::Num(n)) => {
            let len = n.max(0.0) as usize;
            if len > MAX_ARRAY_OP_LEN {
                return Err(i.make_error("RangeError", "Invalid typed array length"));
            }
            let (bv, bp) = make_array_buffer(i, len * es);
            (bv, bp, 0, len)
        }
        Some(Value::Obj(o)) if i.array_buffers.contains_key(&(Rc::as_ptr(o) as usize)) => {
            let bp = Rc::as_ptr(o) as usize;
            let bv = Value::Obj(o.clone());
            let offset = match args.get(1) {
                Some(v) if !matches!(v, Value::Undefined) => ab(i.to_number(v))? as usize,
                _ => 0,
            };
            let buflen = i.array_buffers[&bp].len();
            let len = match args.get(2) {
                Some(v) if !matches!(v, Value::Undefined) => ab(i.to_number(v))? as usize,
                _ => buflen.saturating_sub(offset) / es,
            };
            (bv, bp, offset, len)
        }
        Some(other) => {
            // TypedArray / array-like / iterable: copy element values into a fresh buffer.
            let items = if matches!(other, Value::Str(_)) || i.has_iterator(other) {
                ab(i.iterate(other))?
            } else if matches!(other, Value::Obj(_)) {
                let lenv = ab(i.get_member(other, "length"))?;
                let n = ab(i.to_number(&lenv))?.max(0.0) as usize;
                if n > MAX_ARRAY_OP_LEN {
                    return Err(i.make_error("RangeError", "Invalid typed array length"));
                }
                let mut v = Vec::with_capacity(n.min(1024));
                for k in 0..n {
                    v.push(ab(i.get_member(other, &k.to_string()))?);
                }
                v
            } else {
                Vec::new()
            };
            let len = items.len();
            let (bv, bp) = make_array_buffer(i, len * es);
            let info = TaInfo { buffer: bp, offset: 0, len, kind };
            for (idx, item) in items.iter().enumerate() {
                ab(i.ta_store(&info, idx, item))?;
            }
            (bv, bp, 0, len)
        }
    };
    let obj = Object::new(i.extra_protos.get(kind.name()).cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.typed_arrays.insert(p, TaInfo { buffer: buf_ptr, offset, len, kind });
    set_internal(&obj, "length", Value::Num(len as f64));
    set_internal(&obj, "byteLength", Value::Num((len * es) as f64));
    set_internal(&obj, "byteOffset", Value::Num(offset as f64));
    set_internal(&obj, "buffer", buf_val);
    set_internal(&obj, "BYTES_PER_ELEMENT", Value::Num(es as f64));
    Ok(Value::Obj(obj))
}

fn ta_set(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "set on non-TypedArray"))?;
    let info = *i.typed_arrays.get(&ptr).ok_or_else(|| i.make_error("TypeError", "set on non-TypedArray"))?;
    let offset = match arg(args, 1) {
        Value::Undefined => 0,
        v => ab(i.to_number(&v))? as usize,
    };
    let source = arg(args, 0);
    let items = if matches!(source, Value::Str(_)) || i.has_iterator(&source) {
        ab(i.iterate(&source))?
    } else if matches!(source, Value::Obj(_)) {
        let lenv = ab(i.get_member(&source, "length"))?;
        let n = ab(i.to_number(&lenv))?.max(0.0) as usize;
        let mut v = Vec::with_capacity(n.min(1024));
        for k in 0..n {
            v.push(ab(i.get_member(&source, &k.to_string()))?);
        }
        v
    } else {
        Vec::new()
    };
    for (k, item) in items.iter().enumerate() {
        ab(i.ta_store(&info, offset + k, item))?;
    }
    Ok(Value::Undefined)
}

fn ta_subarray(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "subarray on non-TypedArray"))?;
    let info = *i.typed_arrays.get(&ptr).ok_or_else(|| i.make_error("TypeError", "subarray on non-TypedArray"))?;
    let es = info.kind.elsize();
    let len = info.len as i64;
    let begin = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
    let end = match arg(args, 1) {
        Value::Undefined => len,
        v => norm_index(ab(i.to_number(&v))?, len, len),
    };
    let new_len = (end - begin).max(0) as usize;
    let new_offset = info.offset + begin as usize * es;
    let buf_val = ab(i.get_member(&this, "buffer"))?;
    let obj = Object::new(i.extra_protos.get(info.kind.name()).cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.typed_arrays.insert(p, TaInfo { buffer: info.buffer, offset: new_offset, len: new_len, kind: info.kind });
    set_internal(&obj, "length", Value::Num(new_len as f64));
    set_internal(&obj, "byteLength", Value::Num((new_len * es) as f64));
    set_internal(&obj, "byteOffset", Value::Num(new_offset as f64));
    set_internal(&obj, "buffer", buf_val);
    set_internal(&obj, "BYTES_PER_ELEMENT", Value::Num(es as f64));
    Ok(Value::Obj(obj))
}

macro_rules! ta_ctor {
    ($name:ident, $kind:expr) => {
        fn $name(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
            ta_construct(i, a, $kind)
        }
    };
}
ta_ctor!(ta_ctor_i8, TaKind::I8);
ta_ctor!(ta_ctor_u8, TaKind::U8);
ta_ctor!(ta_ctor_u8c, TaKind::U8Clamped);
ta_ctor!(ta_ctor_i16, TaKind::I16);
ta_ctor!(ta_ctor_u16, TaKind::U16);
ta_ctor!(ta_ctor_i32, TaKind::I32);
ta_ctor!(ta_ctor_u32, TaKind::U32);
ta_ctor!(ta_ctor_f32, TaKind::F32);
ta_ctor!(ta_ctor_f64, TaKind::F64);
ta_ctor!(ta_ctor_i64, TaKind::I64);
ta_ctor!(ta_ctor_u64, TaKind::U64);

fn install_typed_arrays(it: &mut Interp) {
    install_array_buffer(it);

    // Shared %TypedArray% prototype. Each method brand-checks the receiver is a TypedArray, then
    // delegates to the generic Array method (which works through get_member/array_length/set_member).
    let ta_proto = Object::new(Some(it.object_proto.clone()));
    for (name, f) in TA_METHODS {
        it.def_method(&ta_proto, name, 1, *f);
    }
    if let Some(sym) = it.iterator_sym.clone() {
        let k = Interp::sym_key(&sym);
        if let Some(p) = it.array_proto.borrow().props.get(&k).cloned() {
            ta_proto.borrow_mut().props.insert(k, p);
        }
    }
    it.def_method(&ta_proto, "set", 1, ta_set);
    it.def_method(&ta_proto, "subarray", 2, ta_subarray);

    let kinds: [(TaKind, NativeFn); 11] = [
        (TaKind::I8, ta_ctor_i8),
        (TaKind::U8, ta_ctor_u8),
        (TaKind::U8Clamped, ta_ctor_u8c),
        (TaKind::I16, ta_ctor_i16),
        (TaKind::U16, ta_ctor_u16),
        (TaKind::I32, ta_ctor_i32),
        (TaKind::U32, ta_ctor_u32),
        (TaKind::F32, ta_ctor_f32),
        (TaKind::F64, ta_ctor_f64),
        (TaKind::I64, ta_ctor_i64),
        (TaKind::U64, ta_ctor_u64),
    ];
    for (kind, ctor_fn) in kinds {
        let proto = Object::new(Some(ta_proto.clone()));
        it.extra_protos.insert(kind.name(), proto.clone());
        set_builtin(&proto, "BYTES_PER_ELEMENT", Value::Num(kind.elsize() as f64));
        let ctor = it.make_native(kind.name(), 3, ctor_fn);
        ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
        proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
        set_builtin(&ctor, "BYTES_PER_ELEMENT", Value::Num(kind.elsize() as f64));
        set_builtin(&it.global, kind.name(), Value::Obj(ctor));
    }
}

// ---------------------------------------------------------------------------------------------
// Date  (treated entirely as UTC — getTimezoneOffset is 0 — which is enough for most test262 Date
// tests, which use explicit timestamps. Calendar math uses the days-from-civil algorithm.)
// ---------------------------------------------------------------------------------------------

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month0, day, hour, minute, second, millisecond, weekday[0=Sun]).
fn ms_to_parts(t: f64) -> (i64, i64, i64, i64, i64, i64, i64, i64) {
    let ms = t as i64;
    let days = ms.div_euclid(86_400_000);
    let mut rem = ms.rem_euclid(86_400_000);
    let milli = rem % 1000;
    rem /= 1000;
    let sec = rem % 60;
    rem /= 60;
    let min = rem % 60;
    rem /= 60;
    let hour = rem;
    let (y, m, d) = civil_from_days(days);
    let weekday = (days.rem_euclid(7) + 4) % 7; // 1970-01-01 was a Thursday (4)
    (y, m - 1, d, hour, min, sec, milli, weekday)
}

#[allow(clippy::too_many_arguments)]
fn parts_to_ms(y: i64, mo0: i64, d: i64, h: i64, mi: i64, s: i64, ml: i64) -> f64 {
    // Normalize the month into 0..11 with a year carry so e.g. month 13 rolls over.
    let y = y + mo0.div_euclid(12);
    let mo = mo0.rem_euclid(12);
    let days = days_from_civil(y, mo + 1, d);
    (days * 86_400_000 + h * 3_600_000 + mi * 60_000 + s * 1000 + ml) as f64
}

fn set_internal(obj: &Gc, key: &str, v: Value) {
    obj.borrow_mut().props.insert(key, Property::data(v, true, false, false));
}

fn date_ms(i: &mut Interp, this: &Value) -> Result<f64, Value> {
    // thisTimeValue: the receiver must be a Date (carry the internal time slot), else TypeError.
    match this {
        Value::Obj(o) if o.borrow().props.contains("__date_ms") => {}
        _ => return Err(i.make_error("TypeError", "this is not a Date object")),
    }
    Ok(match ab(i.get_member(this, "__date_ms"))? {
        Value::Num(n) => n,
        _ => f64::NAN,
    })
}

fn date_get(i: &mut Interp, this: &Value, sel: u8) -> Result<Value, Value> {
    let t = date_ms(i, this)?;
    if t.is_nan() {
        return Ok(Value::Num(f64::NAN));
    }
    let (y, mo, d, h, mi, s, ml, wd) = ms_to_parts(t);
    let v = match sel {
        0 => y,
        1 => mo,
        2 => d,
        3 => wd,
        4 => h,
        5 => mi,
        6 => s,
        _ => ml,
    };
    Ok(Value::Num(v as f64))
}

fn date_set(i: &mut Interp, this: &Value, sel: u8, nv: f64) -> Result<Value, Value> {
    let t = date_ms(i, this)?;
    let (mut y, mut mo, mut d, mut h, mut mi, mut s, mut ml, _) =
        if t.is_nan() { (1970, 0, 1, 0, 0, 0, 0, 0) } else { ms_to_parts(t) };
    let n = nv as i64;
    match sel {
        0 => y = n,
        1 => mo = n,
        2 => d = n,
        4 => h = n,
        5 => mi = n,
        6 => s = n,
        _ => ml = n,
    }
    let ms = if nv.is_nan() { f64::NAN } else { parts_to_ms(y, mo, d, h, mi, s, ml) };
    if let Value::Obj(o) = this {
        set_internal(o, "__date_ms", Value::Num(ms));
    }
    Ok(Value::Num(ms))
}

/// Minimal ISO-8601 parser: `YYYY[-MM[-DD]][THH:mm[:ss[.sss]]][Z]`. Returns NaN on anything else.
fn parse_iso(s: &str) -> f64 {
    let s = s.trim();
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dp = date_part.splitn(3, '-');
    let y: i64 = match dp.next().and_then(|x| x.parse().ok()) {
        Some(v) => v,
        None => return f64::NAN,
    };
    let mo: i64 = dp.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    let d: i64 = dp.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    let (mut h, mut mi, mut s, mut ml) = (0i64, 0i64, 0i64, 0i64);
    if let Some(tp) = time_part {
        let tp = tp.trim_end_matches('Z');
        let (hms, frac) = match tp.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (tp, None),
        };
        let mut parts = hms.split(':');
        h = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        mi = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        s = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        if let Some(f) = frac {
            let f3: String = f.chars().take(3).chain(std::iter::repeat('0')).take(3).collect();
            ml = f3.parse().unwrap_or(0);
        }
    }
    parts_to_ms(y, mo - 1, d, h, mi, s, ml)
}

fn date_ctor(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let ms = match args.len() {
        0 => now_ms(),
        1 => match &args[0] {
            Value::Str(s) => parse_iso(s),
            v => ab(i.to_number(v))?.trunc(),
        },
        _ => {
            let mut y = ab(i.to_number(&args[0]))? as i64;
            if (0..=99).contains(&y) {
                y += 1900;
            }
            let mo = ab(i.to_number(&arg(args, 1)))? as i64;
            let d = if args.len() > 2 { ab(i.to_number(&args[2]))? as i64 } else { 1 };
            let h = if args.len() > 3 { ab(i.to_number(&args[3]))? as i64 } else { 0 };
            let mi = if args.len() > 4 { ab(i.to_number(&args[4]))? as i64 } else { 0 };
            let s = if args.len() > 5 { ab(i.to_number(&args[5]))? as i64 } else { 0 };
            let ml = if args.len() > 6 { ab(i.to_number(&args[6]))? as i64 } else { 0 };
            parts_to_ms(y, mo, d, h, mi, s, ml)
        }
    };
    let obj = Object::new(i.extra_protos.get("Date").cloned());
    set_internal(&obj, "__date_ms", Value::Num(ms));
    Ok(Value::Obj(obj))
}

fn iso_string(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (y, mo, d, h, mi, s, ml, _) = ms_to_parts(t);
    Some(format!("{y:04}-{:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ml:03}Z", mo + 1))
}

fn install_date(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Date", proto.clone());

    it.def_method(&proto, "getTime", 0, |i, this, _| Ok(Value::Num(date_ms(i, &this)?)));
    it.def_method(&proto, "valueOf", 0, |i, this, _| Ok(Value::Num(date_ms(i, &this)?)));
    it.def_method(&proto, "setTime", 1, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?.trunc();
        if let Value::Obj(o) = &this {
            set_internal(o, "__date_ms", Value::Num(v));
        }
        Ok(Value::Num(v))
    });
    it.def_method(&proto, "getTimezoneOffset", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::Num(if t.is_nan() { f64::NAN } else { 0.0 }))
    });
    // Local and UTC accessors are identical (offset 0).
    for (name, sel) in [
        ("getFullYear", 0u8),
        ("getMonth", 1),
        ("getDate", 2),
        ("getDay", 3),
        ("getHours", 4),
        ("getMinutes", 5),
        ("getSeconds", 6),
        ("getMilliseconds", 7),
    ] {
        let utc = format!("getUTC{}", &name[3..]);
        match sel {
            0 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 0));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 0));
            }
            1 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 1));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 1));
            }
            2 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 2));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 2));
            }
            3 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 3));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 3));
            }
            4 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 4));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 4));
            }
            5 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 5));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 5));
            }
            6 => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 6));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 6));
            }
            _ => {
                it.def_method(&proto, name, 0, |i, this, _| date_get(i, &this, 7));
                it.def_method(&proto, &utc, 0, |i, this, _| date_get(i, &this, 7));
            }
        }
    }
    it.def_method(&proto, "setFullYear", 3, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 0, v)
    });
    it.def_method(&proto, "setMonth", 2, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 1, v)
    });
    it.def_method(&proto, "setDate", 1, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 2, v)
    });
    it.def_method(&proto, "setHours", 4, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 4, v)
    });
    it.def_method(&proto, "setMinutes", 3, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 5, v)
    });
    it.def_method(&proto, "setSeconds", 2, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 6, v)
    });
    it.def_method(&proto, "setMilliseconds", 1, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 7, v)
    });
    // UTC setters mirror the local ones (offset 0).
    it.def_method(&proto, "setUTCFullYear", 3, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 0, v)
    });
    it.def_method(&proto, "setUTCMonth", 2, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 1, v)
    });
    it.def_method(&proto, "setUTCDate", 1, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 2, v)
    });
    it.def_method(&proto, "setUTCHours", 4, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 4, v)
    });
    it.def_method(&proto, "setUTCMinutes", 3, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 5, v)
    });
    it.def_method(&proto, "setUTCSeconds", 2, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 6, v)
    });
    it.def_method(&proto, "setUTCMilliseconds", 1, |i, this, a| {
        let v = ab(i.to_number(&arg(a, 0)))?;
        date_set(i, &this, 7, v)
    });
    it.def_method(&proto, "toISOString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        match iso_string(t) {
            Some(s) => Ok(Value::from_string(s)),
            None => Err(i.make_error("RangeError", "Invalid time value")),
        }
    });
    it.def_method(&proto, "toJSON", 1, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(iso_string(t).map(Value::from_string).unwrap_or(Value::Null))
    });
    it.def_method(&proto, "toString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(iso_string(t).unwrap_or_else(|| "Invalid Date".to_string())))
    });

    let ctor = it.make_native("Date", 7, date_ctor);
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "now", 0, |_i, _t, _a| Ok(Value::Num(now_ms())));
    it.def_method(&ctor, "parse", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        Ok(Value::Num(parse_iso(&s)))
    });
    it.def_method(&ctor, "UTC", 7, |i, _t, a| {
        let mut y = ab(i.to_number(&arg(a, 0)))? as i64;
        if (0..=99).contains(&y) {
            y += 1900;
        }
        let mo = if a.len() > 1 { ab(i.to_number(&a[1]))? as i64 } else { 0 };
        let d = if a.len() > 2 { ab(i.to_number(&a[2]))? as i64 } else { 1 };
        let h = if a.len() > 3 { ab(i.to_number(&a[3]))? as i64 } else { 0 };
        let mi = if a.len() > 4 { ab(i.to_number(&a[4]))? as i64 } else { 0 };
        let s = if a.len() > 5 { ab(i.to_number(&a[5]))? as i64 } else { 0 };
        let ml = if a.len() > 6 { ab(i.to_number(&a[6]))? as i64 } else { 0 };
        Ok(Value::Num(parts_to_ms(y, mo, d, h, mi, s, ml)))
    });
    set_builtin(&it.global, "Date", Value::Obj(ctor));
}

fn map_ptr(this: &Value) -> Option<usize> {
    this.as_obj().map(|o| Rc::as_ptr(o) as usize)
}

/// The property key for `@@toStringTag` (`Symbol.toStringTag`), if Symbol is installed.
pub(crate) fn to_string_tag_key(i: &Interp) -> Option<String> {
    let sym = i.global.borrow().props.get("Symbol").map(|p| p.value.clone())?;
    if let Value::Obj(o) = sym {
        if let Some(p) = o.borrow().props.get("toStringTag") {
            if let Value::Sym(d) = &p.value {
                return Some(Interp::sym_key(d));
            }
        }
    }
    None
}

/// The Object.prototype.toString builtin tag for `this` (the `[object <tag>]` body without an
/// overriding `@@toStringTag`).
fn builtin_tag(i: &Interp, this: &Value) -> &'static str {
    match this {
        Value::Num(_) => "Number",
        Value::Str(_) => "String",
        Value::Bool(_) => "Boolean",
        Value::Obj(o) => {
            let b = o.borrow();
            if matches!(b.exotic, Exotic::Array) {
                "Array"
            } else if !matches!(b.call, Callable::None) {
                "Function"
            } else if matches!(b.exotic, Exotic::Error) {
                "Error"
            } else if matches!(b.exotic, Exotic::BoolWrap(_)) {
                "Boolean"
            } else if matches!(b.exotic, Exotic::NumWrap(_)) {
                "Number"
            } else if matches!(b.exotic, Exotic::StrWrap(_)) {
                "String"
            } else if b.props.contains("__date_ms") {
                "Date"
            } else if i.regexps.contains_key(&(Rc::as_ptr(o) as usize)) {
                "RegExp"
            } else {
                "Object"
            }
        }
        _ => "Object",
    }
}

/// Brand-check a Map/Set receiver: it must be an object carrying a collection data slot (every
/// Map/Set/WeakMap/WeakSet gets one at construction), else TypeError.
fn coll_ptr(i: &Interp, this: &Value) -> Result<usize, Value> {
    match this.as_obj() {
        Some(o) => {
            let ptr = Rc::as_ptr(o) as usize;
            if i.map_data.contains_key(&ptr) {
                Ok(ptr)
            } else {
                Err(i.make_error("TypeError", "method called on an incompatible receiver"))
            }
        }
        None => Err(i.make_error("TypeError", "method called on an incompatible receiver")),
    }
}

/// Build an iterator over a Map/Set's snapshot. `kind`: 0 = values, 1 = keys, 2 = [key,value].
fn collection_iter(i: &mut Interp, this: &Value, kind: u8) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a Map or Set"))?;
    let snap = i.map_data.get(&ptr).cloned().unwrap_or_default();
    let mut arr = Vec::with_capacity(snap.len());
    for (k, v) in snap {
        arr.push(match kind {
            1 => k,
            2 => i.make_array(vec![k, v]),
            _ => v,
        });
    }
    let arrv = i.make_array(arr);
    Ok(make_array_iterator(i, arrv, 0))
}

fn install_collections(it: &mut Interp) {
    install_map_like(it, "Map", false, map_ctor);
    install_map_like(it, "Set", true, set_ctor);
    install_weak(it, "WeakMap", false, weakmap_ctor);
    install_weak(it, "WeakSet", true, weakset_ctor);
}

// Non-capturing constructor entry points (native fns must be bare `fn` pointers).
fn map_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "Map", false)
}
fn set_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "Set", true)
}
fn weakmap_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "WeakMap", false)
}
fn weakset_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "WeakSet", true)
}

fn collection_ctor(i: &mut Interp, args: &[Value], name: &str, is_set: bool) -> Result<Value, Value> {
    let proto = i.extra_protos.get(name).cloned();
    let obj = Object::new(proto);
    let ptr = Rc::as_ptr(&obj) as usize;
    i.map_data.insert(ptr, Vec::new());
    let mv = Value::Obj(obj);
    if let Some(src) = args.first() {
        if !matches!(src, Value::Undefined | Value::Null) {
            let add_fn = ab(i.get_member(&mv, if is_set { "add" } else { "set" }))?;
            let items = ab(i.iterate(src))?;
            for item in items {
                if is_set {
                    ab(i.call(add_fn.clone(), mv.clone(), &[item]))?;
                } else {
                    let k = ab(i.get_member(&item, "0"))?;
                    let v = ab(i.get_member(&item, "1"))?;
                    ab(i.call(add_fn.clone(), mv.clone(), &[k, v]))?;
                }
            }
        }
    }
    Ok(mv)
}

/// Map and Set share almost everything; `is_set` flips key/value handling and method names.
fn install_map_like(it: &mut Interp, name: &'static str, is_set: bool, ctor_fn: NativeFn) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());

    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr(i, &this)?;
            let key = arg(a, 0);
            let e = i.map_data.entry(ptr).or_default();
            if !e.iter().any(|(k, _)| same_value_zero(k, &key)) {
                e.push((key.clone(), key));
            }
            Ok(this)
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr(i, &this)?;
            let (key, val) = (arg(a, 0), arg(a, 1));
            let e = i.map_data.entry(ptr).or_default();
            if let Some(slot) = e.iter_mut().find(|(k, _)| same_value_zero(k, &key)) {
                slot.1 = val;
            } else {
                e.push((key, val));
            }
            Ok(this)
        }
    };
    it.def_method(&proto, if is_set { "add" } else { "set" }, if is_set { 1 } else { 2 }, adder);
    if !is_set {
        it.def_method(&proto, "get", 1, |i, this, a| {
            let ptr = coll_ptr(i, &this)?;
            let key = arg(a, 0);
            Ok(i.map_data
                .get(&ptr)
                .and_then(|e| e.iter().find(|(k, _)| same_value_zero(k, &key)).map(|(_, v)| v.clone()))
                .unwrap_or(Value::Undefined))
        });
    }
    it.def_method(&proto, "has", 1, |i, this, a| {
        let ptr = coll_ptr(i, &this)?;
        let key = arg(a, 0);
        Ok(Value::Bool(
            i.map_data.get(&ptr).map(|e| e.iter().any(|(k, _)| same_value_zero(k, &key))).unwrap_or(false),
        ))
    });
    it.def_method(&proto, "delete", 1, |i, this, a| {
        let ptr = coll_ptr(i, &this)?;
        let key = arg(a, 0);
        let mut removed = false;
        if let Some(e) = i.map_data.get_mut(&ptr) {
            let before = e.len();
            e.retain(|(k, _)| !same_value_zero(k, &key));
            removed = e.len() < before;
        }
        Ok(Value::Bool(removed))
    });
    it.def_method(&proto, "clear", 0, |i, this, _| {
        if let Some(ptr) = map_ptr(&this) {
            if let Some(e) = i.map_data.get_mut(&ptr) {
                e.clear();
            }
        }
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "forEach", 1, |i, this, a| {
        let ptr = coll_ptr(i, &this)?;
        let cb = arg(a, 0);
        let cb_this = arg(a, 1);
        let snap = i.map_data.get(&ptr).cloned().unwrap_or_default();
        for (k, v) in snap {
            ab(i.call(cb.clone(), cb_this.clone(), &[v, k, this.clone()]))?;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "values", 0, |i, this, _| collection_iter(i, &this, 0));
    it.def_method(&proto, "keys", 0, |i, this, _| collection_iter(i, &this, 1));
    it.def_method(&proto, "entries", 0, |i, this, _| collection_iter(i, &this, 2));

    // `size` accessor.
    let size_getter = it.make_native("get size", 0, |i, this, _| {
        let ptr = coll_ptr(i, &this)?;
        Ok(Value::Num(i.map_data.get(&ptr).map(|e| e.len()).unwrap_or(0) as f64))
    });
    proto.borrow_mut().props.insert(
        "size",
        Property {
            value: Value::Undefined,
            get: Some(Value::Obj(size_getter)),
            set: None,
            accessor: true,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    // @@iterator: Set -> values, Map -> entries.
    if let Some(sym) = it.iterator_sym.clone() {
        let default = if is_set { "values" } else { "entries" };
        let f = proto.borrow().props.get(default).map(|p| p.value.clone()).unwrap();
        proto.borrow_mut().props.insert(Interp::sym_key(&sym), Property::builtin(f));
    }

    let ctor = it.make_native(name, 0, ctor_fn);
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    if !is_set {
        // Map.groupBy(items, cb) -> a Map of key -> [items...].
        it.def_method(&ctor, "groupBy", 2, |i, _t, a| {
            let cb = arg(a, 1);
            if !cb.is_callable() {
                return Err(i.make_error("TypeError", "Map.groupBy callback is not callable"));
            }
            let elems = ab(i.iterate(&arg(a, 0)))?;
            let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
            for (idx, el) in elems.into_iter().enumerate() {
                let key = ab(i.call(cb.clone(), Value::Undefined, &[el.clone(), Value::Num(idx as f64)]))?;
                match groups.iter_mut().find(|(k, _)| same_value_zero(k, &key)) {
                    Some(g) => g.1.push(el),
                    None => groups.push((key, vec![el])),
                }
            }
            let m = Object::new(i.extra_protos.get("Map").cloned());
            let ptr = Rc::as_ptr(&m) as usize;
            let entries: Vec<(Value, Value)> =
                groups.into_iter().map(|(k, v)| (k, i.make_array(v))).collect();
            i.map_data.insert(ptr, entries);
            Ok(Value::Obj(m))
        });
    }
    set_builtin(&it.global, name, Value::Obj(ctor));
}

/// WeakMap/WeakSet: like Map/Set but keys must be objects and there is no iteration/size (we do not
/// model weakness — entries simply persist, which is unobservable to non-GC tests).
fn install_weak(it: &mut Interp, name: &'static str, is_set: bool, ctor_fn: NativeFn) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());
    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "add on non-WeakSet"))?;
            let key = arg(a, 0);
            if !matches!(key, Value::Obj(_)) {
                return Err(i.make_error("TypeError", "Invalid value used in weak set"));
            }
            let e = i.map_data.entry(ptr).or_default();
            if !e.iter().any(|(k, _)| same_value_zero(k, &key)) {
                e.push((key.clone(), key));
            }
            Ok(this)
        }
    } else {
        |i, this, a| {
            let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "set on non-WeakMap"))?;
            let (key, val) = (arg(a, 0), arg(a, 1));
            if !matches!(key, Value::Obj(_)) {
                return Err(i.make_error("TypeError", "Invalid value used as weak map key"));
            }
            let e = i.map_data.entry(ptr).or_default();
            if let Some(slot) = e.iter_mut().find(|(k, _)| same_value_zero(k, &key)) {
                slot.1 = val;
            } else {
                e.push((key, val));
            }
            Ok(this)
        }
    };
    it.def_method(&proto, if is_set { "add" } else { "set" }, if is_set { 1 } else { 2 }, adder);
    if !is_set {
        it.def_method(&proto, "get", 1, |i, this, a| {
            let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "get on non-WeakMap"))?;
            let key = arg(a, 0);
            Ok(i.map_data
                .get(&ptr)
                .and_then(|e| e.iter().find(|(k, _)| same_value_zero(k, &key)).map(|(_, v)| v.clone()))
                .unwrap_or(Value::Undefined))
        });
    }
    it.def_method(&proto, "has", 1, |i, this, a| {
        let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "has on non-weak"))?;
        let key = arg(a, 0);
        Ok(Value::Bool(
            i.map_data.get(&ptr).map(|e| e.iter().any(|(k, _)| same_value_zero(k, &key))).unwrap_or(false),
        ))
    });
    it.def_method(&proto, "delete", 1, |i, this, a| {
        let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "delete on non-weak"))?;
        let key = arg(a, 0);
        let mut removed = false;
        if let Some(e) = i.map_data.get_mut(&ptr) {
            let before = e.len();
            e.retain(|(k, _)| !same_value_zero(k, &key));
            removed = e.len() < before;
        }
        Ok(Value::Bool(removed))
    });
    let ctor = it.make_native(name, 0, ctor_fn);
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, name, Value::Obj(ctor));
}

fn install_reflect(it: &mut Interp) {
    let r = it.new_object();
    it.def_method(&r, "get", 2, |i, _t, a| {
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        ab(i.get_member(&arg(a, 0), &key))
    });
    it.def_method(&r, "set", 3, |i, _t, a| {
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        ab(i.set_member(&arg(a, 0), &key, arg(a, 2)))?;
        Ok(Value::Bool(true))
    });
    it.def_method(&r, "has", 2, |i, _t, a| {
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        match arg(a, 0) {
            Value::Obj(o) => Ok(Value::Bool(i.has_property(&o, &key))),
            _ => Err(i.make_error("TypeError", "Reflect.has called on non-object")),
        }
    });
    it.def_method(&r, "deleteProperty", 2, |i, _t, a| {
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        if let Value::Obj(o) = arg(a, 0) {
            let configurable = o.borrow().props.get(&key).map(|p| p.configurable).unwrap_or(true);
            if configurable {
                o.borrow_mut().props.remove(&key);
                return Ok(Value::Bool(true));
            }
            return Ok(Value::Bool(false));
        }
        Err(i.make_error("TypeError", "Reflect.deleteProperty called on non-object"))
    });
    it.def_method(&r, "ownKeys", 1, |i, _t, a| {
        let o = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Reflect.ownKeys called on non-object")),
        };
        // String keys first, then symbols (per spec ordering, simplified).
        let mut out: Vec<Value> = o
            .borrow()
            .props
            .keys()
            .into_iter()
            .filter(|k| !Interp::is_sym_key(k))
            .map(Value::Str)
            .collect();
        let syms: Vec<Value> = o
            .borrow()
            .props
            .keys()
            .into_iter()
            .filter(|k| Interp::is_sym_key(k))
            .filter_map(|k| i.sym_from_key(&k))
            .collect();
        out.extend(syms);
        Ok(i.make_array(out))
    });
    it.def_method(&r, "getPrototypeOf", 1, |i, _t, a| match arg(a, 0) {
        Value::Obj(o) => Ok(o.borrow().proto.clone().map(Value::Obj).unwrap_or(Value::Null)),
        _ => Err(i.make_error("TypeError", "Reflect.getPrototypeOf called on non-object")),
    });
    it.def_method(&r, "setPrototypeOf", 2, |_i, _t, a| {
        if let Value::Obj(o) = arg(a, 0) {
            o.borrow_mut().proto = match arg(a, 1) {
                Value::Obj(p) => Some(p),
                _ => None,
            };
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&r, "defineProperty", 3, |i, _t, a| {
        let o = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Reflect.defineProperty on non-object")),
        };
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        let prop = ab(build_descriptor(i, &arg(a, 2)))?;
        o.borrow_mut().props.insert(key, prop);
        Ok(Value::Bool(true))
    });
    it.def_method(&r, "apply", 3, |i, _t, a| {
        let args = match arg(a, 2) {
            Value::Undefined | Value::Null => Vec::new(),
            list => ab(i.iterate(&list))?,
        };
        ab(i.call(arg(a, 0), arg(a, 1), &args))
    });
    it.def_method(&r, "construct", 2, |i, _t, a| {
        let args = match arg(a, 1) {
            Value::Undefined | Value::Null => Vec::new(),
            list => ab(i.iterate(&list))?,
        };
        ab(i.construct(arg(a, 0), &args))
    });
    it.def_method(&r, "isExtensible", 1, |_i, _t, a| {
        Ok(Value::Bool(matches!(arg(a, 0), Value::Obj(o) if o.borrow().extensible)))
    });
    it.def_method(&r, "preventExtensions", 1, |_i, _t, a| {
        if let Value::Obj(o) = arg(a, 0) {
            o.borrow_mut().extensible = false;
        }
        Ok(Value::Bool(true))
    });
    set_builtin(&it.global, "Reflect", Value::Obj(r));
}

/// Sentinel call slot for a function-targeted proxy, so `is_callable()` is true; the actual
/// dispatch happens in `call_inner`/`construct_inner` via the proxies table (this never runs).
fn proxy_uncallable(i: &mut Interp, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    Err(i.make_error("TypeError", "proxy call dispatch error"))
}

/// A bound handler `(target, this=Undefined, [bound...])` used to thread per-element state into a
/// `Promise.all` reaction without closures.
fn make_bound(i: &Interp, target: NativeFn, bound_args: Vec<Value>) -> Value {
    let t = i.make_native("", 1, target);
    let obj = Object::new(Some(i.function_proto.clone()));
    obj.borrow_mut().call = Callable::Bound { target: t, this: Value::Undefined, args: bound_args };
    Value::Obj(obj)
}

/// `Promise.all` per-element fulfill reaction. `args = [resultPromise, index, value]`.
fn promise_all_element(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let result = arg(args, 0);
    let idx = ab(i.to_number(&arg(args, 1)))? as usize;
    let value = arg(args, 2);
    let results = ab(i.get_member(&result, "__results"))?;
    ab(i.set_member(&results, &idx.to_string(), value))?;
    let rem_v = ab(i.get_member(&result, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&result, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        i.resolve_promise(&result, results);
    }
    Ok(Value::Undefined)
}

fn install_promise(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Promise", proto.clone());
    it.def_method(&proto, "then", 2, |i, this, a| Ok(i.promise_then(&this, arg(a, 0), arg(a, 1))));
    it.def_method(&proto, "catch", 1, |i, this, a| {
        Ok(i.promise_then(&this, Value::Undefined, arg(a, 0)))
    });
    it.def_method(&proto, "finally", 1, |i, this, a| {
        // Approximation: run the callback on both settlement paths (does not perfectly pass the
        // original value/reason through, but covers the common case).
        let cb = arg(a, 0);
        Ok(i.promise_then(&this, cb.clone(), cb))
    });

    let ctor = it.make_native("Promise", 1, |i, _t, a| {
        let executor = arg(a, 0);
        if !executor.is_callable() {
            return Err(i.make_error("TypeError", "Promise resolver is not a function"));
        }
        let promise = i.new_promise();
        let res = i.make_resolver(&promise, true);
        let rej = i.make_resolver(&promise, false);
        if let Err(Abrupt::Throw(e)) = i.call(executor, Value::Undefined, &[res, rej]) {
            i.reject_promise(&promise, e);
        }
        Ok(promise)
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "withResolvers", 0, |i, _t, _a| {
        let promise = i.new_promise();
        let resolve = i.make_resolver(&promise, true);
        let reject = i.make_resolver(&promise, false);
        let obj = i.new_object();
        set_data(&obj, "promise", promise);
        set_data(&obj, "resolve", resolve);
        set_data(&obj, "reject", reject);
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "resolve", 1, |i, _t, a| {
        let v = arg(a, 0);
        if let Value::Obj(o) = &v {
            if i.promises.contains_key(&(Rc::as_ptr(o) as usize)) {
                return Ok(v);
            }
        }
        let p = i.new_promise();
        i.resolve_promise(&p, v);
        Ok(p)
    });
    it.def_method(&ctor, "reject", 1, |i, _t, a| {
        let p = i.new_promise();
        i.reject_promise(&p, arg(a, 0));
        Ok(p)
    });
    it.def_method(&ctor, "all", 1, |i, _t, a| {
        let items = ab(i.iterate(&arg(a, 0)))?;
        let result = i.new_promise();
        let n = items.len();
        let results = i.make_array(vec![Value::Undefined; n]);
        set_internal_obj(&result, "__results", results.clone());
        set_internal_obj(&result, "__remaining", Value::Num(n as f64));
        if n == 0 {
            i.resolve_promise(&result, results);
            return Ok(result);
        }
        for (idx, item) in items.into_iter().enumerate() {
            let p = promise_resolve_value(i, item);
            let on_f = make_bound(i, promise_all_element, vec![result.clone(), Value::Num(idx as f64)]);
            let on_r = i.make_resolver(&result, false);
            i.promise_then(&p, on_f, on_r);
        }
        Ok(result)
    });
    it.def_method(&ctor, "race", 1, |i, _t, a| {
        let items = ab(i.iterate(&arg(a, 0)))?;
        let result = i.new_promise();
        for item in items {
            let p = promise_resolve_value(i, item);
            let on_f = i.make_resolver(&result, true);
            let on_r = i.make_resolver(&result, false);
            i.promise_then(&p, on_f, on_r);
        }
        Ok(result)
    });
    set_builtin(&it.global, "Promise", Value::Obj(ctor));
}

/// `Promise.resolve(x)` as a value helper (returns existing promises unchanged).
fn promise_resolve_value(i: &mut Interp, v: Value) -> Value {
    if let Value::Obj(o) = &v {
        if i.promises.contains_key(&(Rc::as_ptr(o) as usize)) {
            return v;
        }
    }
    let p = i.new_promise();
    i.resolve_promise(&p, v);
    p
}

fn set_internal_obj(target: &Value, key: &str, v: Value) {
    if let Value::Obj(o) = target {
        o.borrow_mut().props.insert(key, Property::data(v, true, false, false));
    }
}

fn install_proxy(it: &mut Interp) {
    let ctor = it.make_native("Proxy", 2, |i, _t, a| {
        let target = arg(a, 0);
        let handler = arg(a, 1);
        if !matches!(target, Value::Obj(_)) || !matches!(handler, Value::Obj(_)) {
            return Err(i.make_error(
                "TypeError",
                "Cannot create proxy with a non-object as target or handler",
            ));
        }
        let proto = match &target {
            Value::Obj(o) => o.borrow().proto.clone(),
            _ => None,
        };
        let obj = Object::new(proto);
        if target.is_callable() {
            obj.borrow_mut().call = Callable::Native(proxy_uncallable);
            obj.borrow_mut().is_constructor = true;
        }
        let p = Rc::as_ptr(&obj) as usize;
        i.proxies.insert(p, (target, handler));
        Ok(Value::Obj(obj))
    });
    set_builtin(&it.global, "Proxy", Value::Obj(ctor));
}

fn install_json(it: &mut Interp) {
    let j = it.new_object();
    it.def_method(&j, "stringify", 3, |i, _t, args| {
        let value = arg(args, 0);
        let gap = match arg(args, 2) {
            Value::Num(n) => " ".repeat((n.max(0.0) as usize).min(10)),
            Value::Str(s) => s.chars().take(10).collect(),
            _ => String::new(),
        };
        let mut seen = Vec::new();
        match json_str(i, &value, &gap, "", &mut seen)? {
            Some(s) => Ok(Value::from_string(s)),
            None => Ok(Value::Undefined),
        }
    });
    it.def_method(&j, "parse", 2, |i, _t, args| {
        let text = ab(i.to_string(&arg(args, 0)))?;
        let chars: Vec<char> = text.chars().collect();
        let mut pos = 0;
        let v = json_parse_value(i, &chars, &mut pos)?;
        json_skip_ws(&chars, &mut pos);
        if pos != chars.len() {
            return Err(i.make_error("SyntaxError", "Unexpected non-whitespace after JSON"));
        }
        Ok(v)
    });
    set_builtin(&it.global, "JSON", Value::Obj(j));
}

fn json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_str(
    i: &mut Interp,
    value: &Value,
    gap: &str,
    indent: &str,
    seen: &mut Vec<usize>,
) -> Result<Option<String>, Value> {
    // Honor a `toJSON` method.
    let value = if matches!(value, Value::Obj(_)) {
        let tojson = ab(i.get_member(value, "toJSON"))?;
        if tojson.is_callable() {
            ab(i.call(tojson, value.clone(), &[]))?
        } else {
            value.clone()
        }
    } else {
        value.clone()
    };
    match &value {
        Value::Undefined | Value::Sym(_) => Ok(None),
        Value::Null => Ok(Some("null".to_string())),
        Value::Bool(b) => Ok(Some(if *b { "true" } else { "false" }.to_string())),
        Value::Num(n) => Ok(Some(if n.is_finite() { i.num_to_str(*n) } else { "null".to_string() })),
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
            let is_array = matches!(o.borrow().exotic, Exotic::Array);
            let result = if is_array {
                let len = i.array_length(o);
                let mut items = Vec::with_capacity(len);
                for k in 0..len {
                    let elem = ab(i.get_member(&value, &k.to_string()))?;
                    items.push(json_str(i, &elem, gap, &new_indent, seen)?.unwrap_or_else(|| "null".to_string()));
                }
                join_json("[", "]", items, gap, &new_indent, indent)
            } else {
                let keys: Vec<Rc<str>> = o
                    .borrow()
                    .props
                    .iter()
                    .filter(|(k, p)| p.enumerable && !Interp::is_sym_key(k))
                    .map(|(k, _)| k.clone())
                    .collect();
                let mut parts = Vec::new();
                for k in keys {
                    let v = ab(i.get_member(&value, &k))?;
                    if let Some(vs) = json_str(i, &v, gap, &new_indent, seen)? {
                        let colon = if gap.is_empty() { ":" } else { ": " };
                        parts.push(format!("{}{colon}{vs}", json_quote(&k)));
                    }
                }
                join_json("{", "}", parts, gap, &new_indent, indent)
            };
            seen.pop();
            Ok(Some(result))
        }
    }
}

fn join_json(open: &str, close: &str, parts: Vec<String>, gap: &str, inner: &str, outer: &str) -> String {
    if parts.is_empty() {
        format!("{open}{close}")
    } else if gap.is_empty() {
        format!("{open}{}{close}", parts.join(","))
    } else {
        format!("{open}\n{inner}{}\n{outer}{close}", parts.join(&format!(",\n{inner}")))
    }
}

fn json_skip_ws(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() && matches!(chars[*pos], ' ' | '\t' | '\n' | '\r') {
        *pos += 1;
    }
}

fn json_parse_value(i: &mut Interp, chars: &[char], pos: &mut usize) -> Result<Value, Value> {
    json_skip_ws(chars, pos);
    let c = *chars.get(*pos).ok_or_else(|| i.make_error("SyntaxError", "Unexpected end of JSON input"))?;
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
                    _ => return Err(i.make_error("SyntaxError", "Expected ',' or '}' in JSON object")),
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
                    _ => return Err(i.make_error("SyntaxError", "Expected ',' or ']' in JSON array")),
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
            if chars[*pos] == '-' {
                *pos += 1;
            }
            while *pos < chars.len() && matches!(chars[*pos], '0'..='9' | '.' | 'e' | 'E' | '+' | '-') {
                *pos += 1;
            }
            let s: String = chars[start..*pos].iter().collect();
            s.parse::<f64>()
                .map(Value::Num)
                .map_err(|_| i.make_error("SyntaxError", "Invalid number in JSON"))
        }
        _ => Err(i.make_error("SyntaxError", "Unexpected token in JSON")),
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
        let c = *chars.get(*pos).ok_or_else(|| i.make_error("SyntaxError", "Unterminated JSON string"))?;
        *pos += 1;
        match c {
            '"' => return Ok(s),
            '\\' => {
                let e = *chars.get(*pos).ok_or_else(|| i.make_error("SyntaxError", "Bad escape in JSON"))?;
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
                        s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                    }
                    _ => return Err(i.make_error("SyntaxError", "Bad escape in JSON")),
                }
            }
            c => s.push(c),
        }
    }
}

fn global_fn(it: &Interp, name: &str, len: usize, f: NativeFn) {
    let func = it.make_native(name, len, f);
    set_builtin(&it.global, name, Value::Obj(func));
}

// ---------------------------------------------------------------------------------------------
// Function.prototype
// ---------------------------------------------------------------------------------------------

fn install_function_proto(it: &mut Interp) {
    let fp = it.function_proto.clone();
    it.def_method(&fp, "call", 1, |i, this, args| {
        let this_arg = arg(args, 0);
        let rest = if args.is_empty() { &[][..] } else { &args[1..] };
        ab(i.call(this, this_arg, rest))
    });
    it.def_method(&fp, "apply", 2, |i, this, args| {
        let this_arg = arg(args, 0);
        let list = match arg(args, 1) {
            Value::Undefined | Value::Null => Vec::new(),
            Value::Obj(o) => {
                let len = ab(i.checked_array_len(&o))?;
                let mut v = Vec::with_capacity(len);
                for k in 0..len {
                    v.push(ab(i.get_member(&Value::Obj(o.clone()), &k.to_string()))?);
                }
                v
            }
            _ => return Err(i.make_error("TypeError", "apply: argument list must be array-like")),
        };
        ab(i.call(this, this_arg, &list))
    });
    it.def_method(&fp, "bind", 1, |i, this, args| {
        let target = match &this {
            Value::Obj(o) if !matches!(o.borrow().call, Callable::None) => o.clone(),
            _ => return Err(i.make_error("TypeError", "bind must be called on a function")),
        };
        let bound_this = arg(args, 0);
        let bound_args = if args.is_empty() { Vec::new() } else { args[1..].to_vec() };
        let obj = Object::new(Some(i.function_proto.clone()));
        obj.borrow_mut().call =
            Callable::Bound { target, this: bound_this, args: bound_args };
        obj.borrow_mut().is_constructor = true;
        set_builtin(&obj, "name", Value::str("bound"));
        Ok(Value::Obj(obj))
    });
    it.def_method(&fp, "toString", 0, |_i, _this, _args| {
        Ok(Value::str("function () { [native code] }"))
    });

    // The `Function` constructor: `Function(p1, p2, ..., body)` compiles a new function in the
    // global scope. We synthesize source and reuse the in-crate parser (no eval engine needed).
    let ctor = it.make_native("Function", 1, |i, _this, args| {
        let (params, body) = if args.is_empty() {
            (String::new(), String::new())
        } else {
            let body = ab(i.to_string(args.last().unwrap()))?.to_string();
            let mut ps = Vec::new();
            for a in &args[..args.len() - 1] {
                ps.push(ab(i.to_string(a))?.to_string());
            }
            (ps.join(","), body)
        };
        let src = format!("function anonymous({params}\n) {{\n{body}\n}}");
        let program = crate::parser::parse_script(&src, false)
            .map_err(|e| i.make_error("SyntaxError", e.message))?;
        match program.into_iter().next() {
            Some(crate::ast::Stmt::FuncDecl(f)) => {
                let env = i.global_env.clone();
                Ok(i.make_function(f, env))
            }
            _ => Err(i.make_error("SyntaxError", "Function constructor: invalid body")),
        }
    });
    // `Function.prototype` is the shared function prototype, so `f instanceof Function` holds for
    // every function (their [[Prototype]] is `function_proto`).
    ctor.borrow_mut().proto = Some(fp.clone());
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(fp.clone()), false, false, false));
    fp.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Function", Value::Obj(ctor));
}

// ---------------------------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------------------------

fn install_object(it: &mut Interp) {
    let op = it.object_proto.clone();
    it.def_method(&op, "hasOwnProperty", 1, |i, this, args| {
        let key = ab(i.to_property_key(&arg(args, 0)))?;
        let has = this_obj(&this).map(|o| o.borrow().props.contains(&key)).unwrap_or(false);
        Ok(Value::Bool(has))
    });
    it.def_method(&op, "isPrototypeOf", 1, |_i, this, args| {
        let target = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Ok(Value::Bool(false)),
        };
        let me = match &this {
            Value::Obj(o) => o.clone(),
            _ => return Ok(Value::Bool(false)),
        };
        let mut cur = target.borrow().proto.clone();
        while let Some(o) = cur {
            if Rc::ptr_eq(&o, &me) {
                return Ok(Value::Bool(true));
            }
            cur = o.borrow().proto.clone();
        }
        Ok(Value::Bool(false))
    });
    it.def_method(&op, "propertyIsEnumerable", 1, |i, this, args| {
        let key = ab(i.to_property_key(&arg(args, 0)))?;
        let e = this_obj(&this)
            .and_then(|o| o.borrow().props.get(&key).map(|p| p.enumerable))
            .unwrap_or(false);
        Ok(Value::Bool(e))
    });
    it.def_method(&op, "toString", 0, |i, this, _args| {
        if matches!(this, Value::Undefined) {
            return Ok(Value::str("[object Undefined]"));
        }
        if matches!(this, Value::Null) {
            return Ok(Value::str("[object Null]"));
        }
        let builtin = builtin_tag(i, &this);
        let tag = match to_string_tag_key(i) {
            Some(key) => match ab(i.get_member(&this, &key))? {
                Value::Str(s) => s.to_string(),
                _ => builtin.to_string(),
            },
            None => builtin.to_string(),
        };
        Ok(Value::str(format!("[object {tag}]")))
    });
    it.def_method(&op, "valueOf", 0, |_i, this, _args| Ok(this));

    let ctor = it.make_native("Object", 1, |i, _this, args| {
        Ok(match arg(args, 0) {
            Value::Undefined | Value::Null => Value::Obj(i.new_object()),
            Value::Obj(o) => Value::Obj(o),
            // ToObject of a primitive yields its wrapper object.
            other => box_primitive(i, other),
        })
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(op.clone()), false, false, false));
    op.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));

    it.def_method(&ctor, "hasOwn", 2, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Object.hasOwn called on non-object")),
        };
        let key = ab(i.to_property_key(&arg(args, 1)))?;
        let has = o.borrow().props.contains(&key);
        Ok(Value::Bool(has))
    });
    it.def_method(&ctor, "groupBy", 2, |i, _this, args| {
        let cb = arg(args, 1);
        if !cb.is_callable() {
            return Err(i.make_error("TypeError", "Object.groupBy callback is not callable"));
        }
        let elems = ab(i.iterate(&arg(args, 0)))?;
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for (idx, el) in elems.into_iter().enumerate() {
            let key_v = ab(i.call(cb.clone(), Value::Undefined, &[el.clone(), Value::Num(idx as f64)]))?;
            let key = ab(i.to_property_key(&key_v))?;
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some(g) => g.1.push(el),
                None => groups.push((key, vec![el])),
            }
        }
        let result = i.new_object();
        result.borrow_mut().proto = None; // groupBy returns a null-prototype object
        for (k, v) in groups {
            let arr = i.make_array(v);
            result.borrow_mut().props.insert(k, Property::plain(arr));
        }
        Ok(Value::Obj(result))
    });
    it.def_method(&ctor, "keys", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Object.keys called on non-object")),
        };
        let keys: Vec<Value> = o
            .borrow()
            .props
            .iter()
            .filter(|(k, p)| p.enumerable && !Interp::is_sym_key(k))
            .map(|(k, _)| Value::Str(k.clone()))
            .collect();
        Ok(i.make_array(keys))
    });
    it.def_method(&ctor, "getOwnPropertyNames", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "called on non-object")),
        };
        let keys: Vec<Value> = o
            .borrow()
            .props
            .keys()
            .into_iter()
            .filter(|k| !Interp::is_sym_key(k))
            .map(Value::Str)
            .collect();
        Ok(i.make_array(keys))
    });
    it.def_method(&ctor, "getOwnPropertySymbols", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "called on non-object")),
        };
        let syms: Vec<Value> = o
            .borrow()
            .props
            .keys()
            .into_iter()
            .filter(|k| Interp::is_sym_key(k))
            .filter_map(|k| i.sym_from_key(&k))
            .collect();
        Ok(i.make_array(syms))
    });
    it.def_method(&ctor, "getPrototypeOf", 1, |i, _this, args| {
        match arg(args, 0) {
            Value::Obj(o) => Ok(o.borrow().proto.clone().map(Value::Obj).unwrap_or(Value::Null)),
            _ => Err(i.make_error("TypeError", "called on non-object")),
        }
    });
    it.def_method(&ctor, "setPrototypeOf", 2, |_i, _this, args| {
        if let Value::Obj(o) = arg(args, 0) {
            o.borrow_mut().proto = match arg(args, 1) {
                Value::Obj(p) => Some(p),
                _ => None,
            };
        }
        Ok(arg(args, 0))
    });
    it.def_method(&ctor, "create", 2, |i, _this, args| {
        let proto = match arg(args, 0) {
            Value::Obj(o) => Some(o),
            Value::Null => None,
            _ => return Err(i.make_error("TypeError", "Object.create proto must be object or null")),
        };
        let obj = Object::new(proto);
        if let Value::Obj(descs) = arg(args, 1) {
            for k in descs.borrow().props.keys() {
                let d = ab(i.get_member(&Value::Obj(descs.clone()), &k))?;
                let prop = ab(build_descriptor(i, &d))?;
                obj.borrow_mut().props.insert(k, prop);
            }
        }
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "defineProperty", 3, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Object.defineProperty called on non-object")),
        };
        let key = ab(i.to_property_key(&arg(args, 1)))?;
        let prop = ab(build_descriptor(i, &arg(args, 2)))?;
        o.borrow_mut().props.insert(key, prop);
        Ok(Value::Obj(o))
    });
    it.def_method(&ctor, "getOwnPropertyDescriptor", 2, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "called on non-object")),
        };
        let key = ab(i.to_property_key(&arg(args, 1)))?;
        let prop = o.borrow().props.get(&key).cloned();
        match prop {
            None => Ok(Value::Undefined),
            Some(p) => {
                let d = i.new_object();
                if p.accessor {
                    set_data(&d, "get", p.get.unwrap_or(Value::Undefined));
                    set_data(&d, "set", p.set.unwrap_or(Value::Undefined));
                } else {
                    set_data(&d, "value", p.value);
                    set_data(&d, "writable", Value::Bool(p.writable));
                }
                set_data(&d, "enumerable", Value::Bool(p.enumerable));
                set_data(&d, "configurable", Value::Bool(p.configurable));
                Ok(Value::Obj(d))
            }
        }
    });
    it.def_method(&ctor, "getOwnPropertyDescriptors", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "called on non-object")),
        };
        let result = i.new_object();
        let keys = o.borrow().props.keys();
        for key in keys {
            let prop = o.borrow().props.get(&key).cloned();
            if let Some(p) = prop {
                let d = i.new_object();
                if p.accessor {
                    set_data(&d, "get", p.get.unwrap_or(Value::Undefined));
                    set_data(&d, "set", p.set.unwrap_or(Value::Undefined));
                } else {
                    set_data(&d, "value", p.value);
                    set_data(&d, "writable", Value::Bool(p.writable));
                }
                set_data(&d, "enumerable", Value::Bool(p.enumerable));
                set_data(&d, "configurable", Value::Bool(p.configurable));
                result.borrow_mut().props.insert(key, Property::plain(Value::Obj(d)));
            }
        }
        Ok(Value::Obj(result))
    });
    it.def_method(&ctor, "freeze", 1, |_i, _this, args| {
        if let Value::Obj(o) = arg(args, 0) {
            o.borrow_mut().extensible = false;
            for k in o.borrow().props.keys() {
                if let Some(p) = o.borrow_mut().props.get_mut(&k) {
                    p.writable = false;
                    p.configurable = false;
                }
            }
        }
        Ok(arg(args, 0))
    });
    it.def_method(&ctor, "preventExtensions", 1, |_i, _this, args| {
        if let Value::Obj(o) = arg(args, 0) {
            o.borrow_mut().extensible = false;
        }
        Ok(arg(args, 0))
    });
    it.def_method(&ctor, "isExtensible", 1, |_i, _this, args| {
        Ok(Value::Bool(matches!(arg(args, 0), Value::Obj(o) if o.borrow().extensible)))
    });
    it.def_method(&ctor, "assign", 2, |i, _this, args| {
        let target = arg(args, 0);
        for src in &args[1.min(args.len())..] {
            if let Value::Obj(o) = src {
                for k in o.borrow().props.iter().filter(|(_, p)| p.enumerable).map(|(k, _)| k.clone()).collect::<Vec<_>>() {
                    let v = ab(i.get_member(src, &k))?;
                    ab(i.set_member(&target, &k, v))?;
                }
            }
        }
        Ok(target)
    });
    it.def_method(&ctor, "is", 2, |_i, _this, args| {
        Ok(Value::Bool(same_value(&arg(args, 0), &arg(args, 1))))
    });
    it.def_method(&ctor, "values", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Object.values called on non-object")),
        };
        let keys: Vec<Rc<str>> = o
            .borrow()
            .props
            .iter()
            .filter(|(k, p)| p.enumerable && !Interp::is_sym_key(k))
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            out.push(ab(i.get_member(&arg(args, 0), &k))?);
        }
        Ok(i.make_array(out))
    });
    it.def_method(&ctor, "entries", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Object.entries called on non-object")),
        };
        let keys: Vec<Rc<str>> = o
            .borrow()
            .props
            .iter()
            .filter(|(k, p)| p.enumerable && !Interp::is_sym_key(k))
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            let v = ab(i.get_member(&arg(args, 0), &k))?;
            out.push(i.make_array(vec![Value::Str(k), v]));
        }
        Ok(i.make_array(out))
    });
    it.def_method(&ctor, "fromEntries", 1, |i, _this, args| {
        let pairs = ab(i.iterate(&arg(args, 0)))?;
        let obj = i.new_object();
        for pair in pairs {
            let k = ab(i.get_member(&pair, "0"))?;
            let v = ab(i.get_member(&pair, "1"))?;
            let key = ab(i.to_property_key(&k))?;
            ab(i.set_member(&Value::Obj(obj.clone()), &key, v))?;
        }
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "defineProperties", 2, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Object.defineProperties on non-object")),
        };
        if let Value::Obj(descs) = arg(args, 1) {
            for k in descs.borrow().props.keys() {
                let d = ab(i.get_member(&Value::Obj(descs.clone()), &k))?;
                let prop = ab(build_descriptor(i, &d))?;
                o.borrow_mut().props.insert(k, prop);
            }
        }
        Ok(Value::Obj(o))
    });
    it.def_method(&ctor, "seal", 1, |_i, _this, args| {
        if let Value::Obj(o) = arg(args, 0) {
            o.borrow_mut().extensible = false;
            for k in o.borrow().props.keys() {
                if let Some(p) = o.borrow_mut().props.get_mut(&k) {
                    p.configurable = false;
                }
            }
        }
        Ok(arg(args, 0))
    });
    it.def_method(&ctor, "isSealed", 1, |_i, _this, args| {
        let sealed = match arg(args, 0) {
            Value::Obj(o) => {
                !o.borrow().extensible && o.borrow().props.iter().all(|(_, p)| !p.configurable)
            }
            _ => true,
        };
        Ok(Value::Bool(sealed))
    });
    it.def_method(&ctor, "isFrozen", 1, |_i, _this, args| {
        let frozen = match arg(args, 0) {
            Value::Obj(o) => {
                !o.borrow().extensible
                    && o.borrow().props.iter().all(|(_, p)| !p.configurable && (p.accessor || !p.writable))
            }
            _ => true,
        };
        Ok(Value::Bool(frozen))
    });

    set_builtin(&it.global, "Object", Value::Obj(ctor));
}

/// Build a property descriptor from a JS descriptor object.
fn build_descriptor(i: &mut Interp, desc: &Value) -> Result<Property, Abrupt> {
    let o = match desc {
        Value::Obj(o) => o.clone(),
        _ => return Err(i.throw("TypeError", "property descriptor must be an object")),
    };
    let has = |k: &str| o.borrow().props.contains(k);
    let base = Value::Obj(o.clone());

    // Resolve each present field into a local first (avoids overlapping borrows of `i`).
    let bool_field = |i: &mut Interp, k: &str| -> Result<bool, Abrupt> {
        if has(k) {
            let v = i.get_member(&base, k)?;
            Ok(i.to_boolean(&v))
        } else {
            Ok(false)
        }
    };
    let enumerable = bool_field(i, "enumerable")?;
    let configurable = bool_field(i, "configurable")?;

    if has("get") || has("set") {
        let get = if has("get") { Some(i.get_member(&base, "get")?) } else { None };
        let set = if has("set") { Some(i.get_member(&base, "set")?) } else { None };
        Ok(Property {
            value: Value::Undefined,
            get,
            set,
            accessor: true,
            writable: false,
            enumerable,
            configurable,
        })
    } else {
        let writable = bool_field(i, "writable")?;
        let value = if has("value") { i.get_member(&base, "value")? } else { Value::Undefined };
        Ok(Property {
            value,
            get: None,
            set: None,
            accessor: false,
            writable,
            enumerable,
            configurable,
        })
    }
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => {
            if x.is_nan() && y.is_nan() {
                return true;
            }
            if *x == 0.0 && *y == 0.0 {
                return x.is_sign_negative() == y.is_sign_negative();
            }
            x == y
        }
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Array
// ---------------------------------------------------------------------------------------------

fn install_array(it: &mut Interp) {
    let ap = it.array_proto.clone();
    ap.borrow_mut().exotic = Exotic::Array;
    ap.borrow_mut().props.insert("length", Property::data(Value::Num(0.0), true, false, false));

    it.def_method(&ap, "push", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "push on non-object"))?;
        let mut len = ab(i.checked_array_len(&o))?;
        for a in args {
            ab(i.set_member(&this, &len.to_string(), a.clone()))?;
            len += 1;
        }
        Ok(Value::Num(len as f64))
    });
    it.def_method(&ap, "pop", 0, |i, this, _args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "pop on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        if len == 0 {
            return Ok(Value::Undefined);
        }
        let last = ab(i.get_member(&this, &(len - 1).to_string()))?;
        o.borrow_mut().props.remove(&(len - 1).to_string());
        ab(i.set_member(&this, "length", Value::Num((len - 1) as f64)))?;
        Ok(last)
    });
    it.def_method(&ap, "shift", 0, |i, this, _args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "shift on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        if len == 0 {
            return Ok(Value::Undefined);
        }
        let first = ab(i.get_member(&this, "0"))?;
        for k in 1..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            ab(i.set_member(&this, &(k - 1).to_string(), v))?;
        }
        o.borrow_mut().props.remove(&(len - 1).to_string());
        ab(i.set_member(&this, "length", Value::Num((len - 1) as f64)))?;
        Ok(first)
    });
    it.def_method(&ap, "unshift", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "unshift on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let n = args.len();
        for k in (0..len).rev() {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            ab(i.set_member(&this, &(k + n).to_string(), v))?;
        }
        for (idx, a) in args.iter().enumerate() {
            ab(i.set_member(&this, &idx.to_string(), a.clone()))?;
        }
        ab(i.set_member(&this, "length", Value::Num((len + n) as f64)))?;
        Ok(Value::Num((len + n) as f64))
    });
    it.def_method(&ap, "slice", 2, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "slice on non-object"))?;
        let len = ab(i.checked_array_len(&o))? as i64;
        let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
        let end = match arg(args, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let mut out = Vec::new();
        let mut k = start;
        while k < end {
            out.push(ab(i.get_member(&this, &k.to_string()))?);
            k += 1;
        }
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "indexOf", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "indexOf on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let target = arg(args, 0);
        for k in 0..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            if i.strict_equals(&v, &target) {
                return Ok(Value::Num(k as f64));
            }
        }
        Ok(Value::Num(-1.0))
    });
    it.def_method(&ap, "includes", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "includes on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let target = arg(args, 0);
        for k in 0..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            if same_value_zero(&v, &target) {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });
    it.def_method(&ap, "join", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "join on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let sep = match arg(args, 0) {
            Value::Undefined => ",".to_string(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        let mut parts = Vec::with_capacity(len);
        for k in 0..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            parts.push(match v {
                Value::Undefined | Value::Null => String::new(),
                other => ab(i.to_string(&other))?.to_string(),
            });
        }
        Ok(Value::from_string(parts.join(&sep)))
    });
    it.def_method(&ap, "concat", 1, |i, this, args| {
        let mut out = Vec::new();
        let mut push_all = |i: &mut Interp, v: &Value| -> Result<(), Value> {
            if let Value::Obj(o) = v {
                if matches!(o.borrow().exotic, Exotic::Array) {
                    let len = ab(i.checked_array_len(o))?;
                    for k in 0..len {
                        out.push(ab(i.get_member(v, &k.to_string()))?);
                    }
                    return Ok(());
                }
            }
            out.push(v.clone());
            Ok(())
        };
        push_all(i, &this)?;
        for a in args {
            push_all(i, a)?;
        }
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "forEach", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "forEach on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        let cb_this = arg(args, 1);
        for k in 0..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            ab(i.call(cb.clone(), cb_this.clone(), &[v, Value::Num(k as f64), this.clone()]))?;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&ap, "map", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "map on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        let cb_this = arg(args, 1);
        let mut out = Vec::with_capacity(len);
        for k in 0..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            out.push(ab(i.call(cb.clone(), cb_this.clone(), &[v, Value::Num(k as f64), this.clone()]))?);
        }
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "filter", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "filter on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        let cb_this = arg(args, 1);
        let mut out = Vec::new();
        for k in 0..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            let keep = ab(i.call(cb.clone(), cb_this.clone(), &[v.clone(), Value::Num(k as f64), this.clone()]))?;
            if i.to_boolean(&keep) {
                out.push(v);
            }
        }
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "reduce", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "reduce on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        let mut acc;
        let mut start = 0;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            if len == 0 {
                return Err(i.make_error("TypeError", "reduce of empty array with no initial value"));
            }
            acc = ab(i.get_member(&this, "0"))?;
            start = 1;
        }
        for k in start..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            acc = ab(i.call(cb.clone(), Value::Undefined, &[acc, v, Value::Num(k as f64), this.clone()]))?;
        }
        Ok(acc)
    });
    it.def_method(&ap, "reverse", 0, |i, this, _args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "reverse on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        for k in 0..len / 2 {
            let a = ab(i.get_member(&this, &k.to_string()))?;
            let b = ab(i.get_member(&this, &(len - 1 - k).to_string()))?;
            ab(i.set_member(&this, &k.to_string(), b))?;
            ab(i.set_member(&this, &(len - 1 - k).to_string(), a))?;
        }
        Ok(this)
    });
    it.def_method(&ap, "toString", 0, |i, this, _args| {
        let join = ab(i.get_member(&this, "join"))?;
        if join.is_callable() {
            ab(i.call(join, this, &[]))
        } else {
            Ok(Value::str("[object Array]"))
        }
    });
    it.def_method(&ap, "at", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "at on non-object"))?;
        let len = ab(i.checked_array_len(&o))? as i64;
        let mut idx = ab(i.to_number(&arg(args, 0)))? as i64;
        if idx < 0 {
            idx += len;
        }
        if idx < 0 || idx >= len {
            return Ok(Value::Undefined);
        }
        ab(i.get_member(&this, &idx.to_string()))
    });
    it.def_method(&ap, "find", 1, |i, this, args| array_find(i, this, args, true, false));
    it.def_method(&ap, "findIndex", 1, |i, this, args| array_find(i, this, args, false, false));
    it.def_method(&ap, "findLast", 1, |i, this, args| array_find(i, this, args, true, true));
    it.def_method(&ap, "findLastIndex", 1, |i, this, args| array_find(i, this, args, false, true));
    it.def_method(&ap, "some", 1, |i, this, args| array_some_every(i, this, args, false));
    it.def_method(&ap, "every", 1, |i, this, args| array_some_every(i, this, args, true));
    it.def_method(&ap, "fill", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "fill on non-object"))?;
        let len = ab(i.checked_array_len(&o))? as i64;
        let v = arg(args, 0);
        let start = norm_index(ab(i.to_number(&arg(args, 1)))?, len, 0);
        let end = match arg(args, 2) {
            Value::Undefined => len,
            x => norm_index(ab(i.to_number(&x))?, len, len),
        };
        for k in start..end {
            ab(i.set_member(&this, &k.to_string(), v.clone()))?;
        }
        Ok(this)
    });
    it.def_method(&ap, "lastIndexOf", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "lastIndexOf"))?;
        let len = ab(i.checked_array_len(&o))?;
        let target = arg(args, 0);
        for k in (0..len).rev() {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            if i.strict_equals(&v, &target) {
                return Ok(Value::Num(k as f64));
            }
        }
        Ok(Value::Num(-1.0))
    });
    it.def_method(&ap, "flat", 0, |i, this, args| {
        let depth = match arg(args, 0) {
            Value::Undefined => 1.0,
            v => ab(i.to_number(&v))?,
        };
        let mut out = Vec::new();
        array_flatten(i, &this, depth as i64, &mut out)?;
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "flatMap", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "flatMap"))?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        let cb_this = arg(args, 1);
        let mut mapped = Vec::with_capacity(len);
        for k in 0..len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            mapped.push(ab(i.call(cb.clone(), cb_this.clone(), &[v, Value::Num(k as f64), this.clone()]))?);
        }
        let arr = i.make_array(mapped);
        let mut out = Vec::new();
        array_flatten(i, &arr, 1, &mut out)?;
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "splice", 2, array_splice);
    it.def_method(&ap, "sort", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "sort on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let mut items = Vec::with_capacity(len);
        for k in 0..len {
            items.push(ab(i.get_member(&this, &k.to_string()))?);
        }
        let cmp = arg(args, 0);
        merge_sort(i, &mut items, &cmp)?;
        for (k, v) in items.into_iter().enumerate() {
            ab(i.set_member(&this, &k.to_string(), v))?;
        }
        Ok(this)
    });
    // ----- change-array-by-copy (return a new Array, leave the receiver untouched) -----
    fn collect_items(i: &mut Interp, this: &Value) -> Result<Vec<Value>, Value> {
        let o = this_obj(this).ok_or_else(|| i.make_error("TypeError", "called on non-object"))?;
        let len = ab(i.checked_array_len(&o))?;
        let mut items = Vec::with_capacity(len);
        for k in 0..len {
            items.push(ab(i.get_member(this, &k.to_string()))?);
        }
        Ok(items)
    }
    it.def_method(&ap, "toReversed", 0, |i, this, _| {
        let mut items = collect_items(i, &this)?;
        items.reverse();
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "toSorted", 1, |i, this, args| {
        let mut items = collect_items(i, &this)?;
        let cmp = arg(args, 0);
        if !matches!(cmp, Value::Undefined) && !cmp.is_callable() {
            return Err(i.make_error("TypeError", "comparator is not callable"));
        }
        merge_sort(i, &mut items, &cmp)?;
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "with", 2, |i, this, args| {
        let items = collect_items(i, &this)?;
        let len = items.len() as i64;
        let rel = ab(i.to_number(&arg(args, 0)))?;
        let idx = if rel < 0.0 { len + rel as i64 } else { rel as i64 };
        if idx < 0 || idx >= len {
            return Err(i.make_error("RangeError", "invalid index"));
        }
        let mut items = items;
        items[idx as usize] = arg(args, 1);
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "toSpliced", 2, |i, this, args| {
        let mut items = collect_items(i, &this)?;
        let len = items.len() as i64;
        let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0) as usize;
        let del = if args.len() < 2 {
            items.len() - start
        } else {
            (ab(i.to_number(&arg(args, 1)))?.max(0.0) as usize).min(items.len() - start)
        };
        let inserts: Vec<Value> = args.iter().skip(2).cloned().collect();
        items.splice(start..start + del, inserts);
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "reduceRight", 1, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "reduceRight"))?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        let mut acc;
        let mut k = len as i64 - 1;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            if len == 0 {
                return Err(i.make_error("TypeError", "reduce of empty array with no initial value"));
            }
            acc = ab(i.get_member(&this, &k.to_string()))?;
            k -= 1;
        }
        while k >= 0 {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            acc = ab(i.call(cb.clone(), Value::Undefined, &[acc, v, Value::Num(k as f64), this.clone()]))?;
            k -= 1;
        }
        Ok(acc)
    });
    it.def_method(&ap, "copyWithin", 2, |i, this, args| {
        let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "copyWithin"))?;
        let len = ab(i.checked_array_len(&o))? as i64;
        let target = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
        let start = norm_index(ab(i.to_number(&arg(args, 1)))?, len, 0);
        let end = match arg(args, 2) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let snapshot: Vec<Value> = {
            let mut s = Vec::new();
            let mut k = start;
            while k < end {
                s.push(ab(i.get_member(&this, &k.to_string()))?);
                k += 1;
            }
            s
        };
        for (off, v) in snapshot.into_iter().enumerate() {
            if target + off as i64 >= len {
                break;
            }
            ab(i.set_member(&this, &(target + off as i64).to_string(), v))?;
        }
        Ok(this)
    });
    it.def_method(&ap, "values", 0, |i, this, _| Ok(make_array_iterator(i, this, 0)));
    it.def_method(&ap, "keys", 0, |i, this, _| Ok(make_array_iterator(i, this, 1)));
    it.def_method(&ap, "entries", 0, |i, this, _| Ok(make_array_iterator(i, this, 2)));
    // `arr[Symbol.iterator]` is `Array.prototype.values`.
    if let Some(sym) = it.iterator_sym.clone() {
        let values_fn = ap.borrow().props.get("values").map(|p| p.value.clone()).unwrap();
        ap.borrow_mut().props.insert(Interp::sym_key(&sym), Property::builtin(values_fn));
    }

    let ctor = it.make_native("Array", 1, |i, _this, args| {
        if args.len() == 1 {
            if let Value::Num(n) = args[0] {
                // `new Array(len)` sets length without materializing elements; the length setter
                // validates that it is a valid uint32 (else RangeError: Invalid array length).
                let a = i.make_array(Vec::new());
                ab(i.set_member(&a, "length", Value::Num(n)))?;
                return Ok(a);
            }
        }
        Ok(i.make_array(args.to_vec()))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(ap.clone()), false, false, false));
    ap.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "isArray", 1, |_i, _this, args| {
        Ok(Value::Bool(matches!(arg(args, 0), Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array))))
    });
    it.def_method(&ctor, "of", 0, |i, _this, args| Ok(i.make_array(args.to_vec())));
    it.def_method(&ctor, "from", 1, |i, _this, args| {
        let source = arg(args, 0);
        let mapfn = arg(args, 1);
        let items = match &source {
            Value::Str(_) => ab(i.iterate(&source))?,
            Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array) => ab(i.iterate(&source))?,
            Value::Obj(_) if i.has_iterator(&source) => ab(i.iterate(&source))?,
            Value::Obj(_) => {
                // Array-like: read `length` then indexed elements.
                let lenv = ab(i.get_member(&source, "length"))?;
                let len = ab(i.to_number(&lenv))?.max(0.0) as usize;
                if len > MAX_ARRAY_OP_LEN {
                    return Err(i.make_error("RangeError", "array length exceeds engine limit"));
                }
                let mut v = Vec::with_capacity(len);
                for k in 0..len {
                    v.push(ab(i.get_member(&source, &k.to_string()))?);
                }
                v
            }
            _ => return Err(i.make_error("TypeError", "Array.from requires an iterable or array-like")),
        };
        if mapfn.is_callable() {
            let mut out = Vec::with_capacity(items.len());
            for (k, v) in items.into_iter().enumerate() {
                out.push(ab(i.call(mapfn.clone(), Value::Undefined, &[v, Value::Num(k as f64)]))?);
            }
            Ok(i.make_array(out))
        } else {
            Ok(i.make_array(items))
        }
    });
    set_builtin(&it.global, "Array", Value::Obj(ctor));
}

fn array_find(
    i: &mut Interp,
    this: Value,
    args: &[Value],
    want_value: bool,
    from_last: bool,
) -> Result<Value, Value> {
    let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "find on non-object"))?;
    let len = ab(i.checked_array_len(&o))?;
    let cb = arg(args, 0);
    let cb_this = arg(args, 1);
    let order: Vec<usize> = if from_last { (0..len).rev().collect() } else { (0..len).collect() };
    for k in order {
        let v = ab(i.get_member(&this, &k.to_string()))?;
        let r = ab(i.call(cb.clone(), cb_this.clone(), &[v.clone(), Value::Num(k as f64), this.clone()]))?;
        if i.to_boolean(&r) {
            return Ok(if want_value { v } else { Value::Num(k as f64) });
        }
    }
    Ok(if want_value { Value::Undefined } else { Value::Num(-1.0) })
}

fn array_some_every(i: &mut Interp, this: Value, args: &[Value], every: bool) -> Result<Value, Value> {
    let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "some/every on non-object"))?;
    let len = ab(i.checked_array_len(&o))?;
    let cb = arg(args, 0);
    let cb_this = arg(args, 1);
    for k in 0..len {
        let v = ab(i.get_member(&this, &k.to_string()))?;
        let r = ab(i.call(cb.clone(), cb_this.clone(), &[v, Value::Num(k as f64), this.clone()]))?;
        let b = i.to_boolean(&r);
        if every && !b {
            return Ok(Value::Bool(false));
        }
        if !every && b {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(every))
}

fn array_flatten(i: &mut Interp, arr: &Value, depth: i64, out: &mut Vec<Value>) -> Result<(), Value> {
    let o = match arr {
        Value::Obj(o) => o.clone(),
        _ => return Ok(()),
    };
    let len = ab(i.checked_array_len(&o))?;
    for k in 0..len {
        let v = ab(i.get_member(arr, &k.to_string()))?;
        let is_arr = matches!(&v, Value::Obj(vo) if matches!(vo.borrow().exotic, Exotic::Array));
        if depth > 0 && is_arr {
            array_flatten(i, &v, depth - 1, out)?;
        } else {
            out.push(v);
        }
    }
    Ok(())
}

fn array_splice(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = this_obj(&this).ok_or_else(|| i.make_error("TypeError", "splice on non-object"))?;
    let len = ab(i.checked_array_len(&o))? as i64;
    let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
    let delete_count = if args.len() < 2 {
        len - start
    } else {
        (ab(i.to_number(&arg(args, 1)))? as i64).clamp(0, len - start)
    };
    let items: Vec<Value> = if args.len() > 2 { args[2..].to_vec() } else { Vec::new() };
    let mut removed = Vec::with_capacity(delete_count.max(0) as usize);
    for k in 0..delete_count {
        removed.push(ab(i.get_member(&this, &(start + k).to_string()))?);
    }
    let mut tail = Vec::new();
    for k in (start + delete_count)..len {
        tail.push(ab(i.get_member(&this, &k.to_string()))?);
    }
    let mut idx = start;
    for v in items.iter().chain(tail.iter()) {
        ab(i.set_member(&this, &idx.to_string(), v.clone()))?;
        idx += 1;
    }
    ab(i.set_member(&this, "length", Value::Num(idx as f64)))?;
    Ok(i.make_array(removed))
}

fn merge_sort(i: &mut Interp, items: &mut [Value], cmp: &Value) -> Result<(), Value> {
    let n = items.len();
    if n <= 1 {
        return Ok(());
    }
    let mid = n / 2;
    let mut left = items[..mid].to_vec();
    let mut right = items[mid..].to_vec();
    merge_sort(i, &mut left, cmp)?;
    merge_sort(i, &mut right, cmp)?;
    let (mut a, mut b, mut k) = (0, 0, 0);
    while a < left.len() && b < right.len() {
        if compare_values(i, cmp, &left[a], &right[b])? != Ordering::Greater {
            items[k] = left[a].clone();
            a += 1;
        } else {
            items[k] = right[b].clone();
            b += 1;
        }
        k += 1;
    }
    while a < left.len() {
        items[k] = left[a].clone();
        a += 1;
        k += 1;
    }
    while b < right.len() {
        items[k] = right[b].clone();
        b += 1;
        k += 1;
    }
    Ok(())
}

fn compare_values(i: &mut Interp, cmp: &Value, a: &Value, b: &Value) -> Result<Ordering, Value> {
    // `undefined` always sorts to the end.
    match (matches!(a, Value::Undefined), matches!(b, Value::Undefined)) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Greater),
        (false, true) => return Ok(Ordering::Less),
        _ => {}
    }
    if cmp.is_callable() {
        let r = ab(i.call(cmp.clone(), Value::Undefined, &[a.clone(), b.clone()]))?;
        let n = ab(i.to_number(&r))?;
        Ok(if n < 0.0 {
            Ordering::Less
        } else if n > 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        })
    } else {
        let sa = ab(i.to_string(a))?;
        let sb = ab(i.to_string(b))?;
        Ok(sa.as_ref().cmp(sb.as_ref()))
    }
}

/// A native that returns its `this` — the `@@iterator` of an iterator object is itself.
fn return_this(_i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(this)
}

fn install_iterator(it: &mut Interp) {
    // %IteratorPrototype%: the common prototype of all built-in iterators; `[@@iterator]()` is the
    // identity function so an iterator is itself iterable.
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("%IteratorPrototype%", proto.clone());
    if let Some(sym) = it.iterator_sym.clone() {
        let f = it.make_native("[Symbol.iterator]", 0, return_this);
        proto.borrow_mut().props.insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(f)));
    }
    let ctor = it.make_native("Iterator", 0, |i, _t, _a| {
        Err(i.make_error("TypeError", "Abstract class Iterator not directly constructable"))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Iterator", Value::Obj(ctor));
}

/// Build an Array Iterator over `target`. `kind`: 0 = values, 1 = keys, 2 = [key, value] entries.
/// State lives in non-enumerable internal slots so `next` can advance it.
fn make_array_iterator(i: &mut Interp, target: Value, kind: u8) -> Value {
    let proto = i.extra_protos.get("%IteratorPrototype%").cloned().or_else(|| Some(i.object_proto.clone()));
    let obj = Object::new(proto);
    set_builtin(&obj, "__ai_target", target);
    set_builtin(&obj, "__ai_index", Value::Num(0.0));
    set_builtin(&obj, "__ai_kind", Value::Num(kind as f64));
    i.def_method(&obj, "next", 0, array_iter_next);
    if let Some(sym) = i.iterator_sym.clone() {
        let f = i.make_native("[Symbol.iterator]", 0, return_this);
        obj.borrow_mut().props.insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(f)));
    }
    Value::Obj(obj)
}

/// `next()` for a generator object built by `make_generator`: walk the buffered values, then throw
/// any stored error, then return `{ value: <return>, done: true }`.
pub(crate) fn generator_next(i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    let arr = ab(i.get_member(&this, "__gen_arr"))?;
    let idx_v = ab(i.get_member(&this, "__gen_idx"))?;
    let idx = ab(i.to_number(&idx_v))? as usize;
    let len = match &arr {
        Value::Obj(o) => i.array_length(o),
        _ => 0,
    };
    let result = i.new_object();
    if idx < len {
        let v = ab(i.get_member(&arr, &idx.to_string()))?;
        ab(i.set_member(&this, "__gen_idx", Value::Num((idx + 1) as f64)))?;
        set_data(&result, "value", v);
        set_data(&result, "done", Value::Bool(false));
        return Ok(Value::Obj(result));
    }
    let has_err = ab(i.get_member(&this, "__gen_haserr"))?;
    if i.to_boolean(&has_err) {
        ab(i.set_member(&this, "__gen_haserr", Value::Bool(false)))?;
        return Err(ab(i.get_member(&this, "__gen_err"))?);
    }
    let ret = ab(i.get_member(&this, "__gen_ret"))?;
    set_data(&result, "value", ret);
    set_data(&result, "done", Value::Bool(true));
    Ok(Value::Obj(result))
}

fn array_iter_next(i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    let target = ab(i.get_member(&this, "__ai_target"))?;
    let idx_v = ab(i.get_member(&this, "__ai_index"))?;
    let idx = ab(i.to_number(&idx_v))? as usize;
    let kind_v = ab(i.get_member(&this, "__ai_kind"))?;
    let kind = ab(i.to_number(&kind_v))? as u8;
    let len = match &target {
        Value::Obj(o) => i.array_length(o),
        Value::Str(s) => s.chars().count(),
        _ => 0,
    };
    let result = i.new_object();
    if idx >= len {
        set_data(&result, "value", Value::Undefined);
        set_data(&result, "done", Value::Bool(true));
        return Ok(Value::Obj(result));
    }
    ab(i.set_member(&this, "__ai_index", Value::Num((idx + 1) as f64)))?;
    let elem = ab(i.get_member(&target, &idx.to_string()))?;
    let value = match kind {
        1 => Value::Num(idx as f64),
        2 => i.make_array(vec![Value::Num(idx as f64), elem]),
        _ => elem,
    };
    set_data(&result, "value", value);
    set_data(&result, "done", Value::Bool(false));
    Ok(Value::Obj(result))
}

fn norm_index(n: f64, len: i64, default: i64) -> i64 {
    if n.is_nan() {
        return default;
    }
    let i = n as i64;
    if i < 0 {
        (len + i).max(0)
    } else {
        i.min(len)
    }
}

fn same_value_zero(a: &Value, b: &Value) -> bool {
    if let (Value::Num(x), Value::Num(y)) = (a, b) {
        if x.is_nan() && y.is_nan() {
            return true;
        }
    }
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x == y,
        _ => same_value(a, b),
    }
}

// ---------------------------------------------------------------------------------------------
// String / Number / Boolean / Math / errors / globals
// ---------------------------------------------------------------------------------------------

fn this_string(i: &mut Interp, this: &Value) -> Result<Rc<str>, Value> {
    match this {
        Value::Str(s) => Ok(s.clone()),
        Value::Obj(o) => match &o.borrow().exotic {
            Exotic::StrWrap(s) => Ok(s.clone()),
            _ => ab(i.to_string(this)),
        },
        // String.prototype methods RequireObjectCoercible(this): null/undefined → TypeError.
        Value::Undefined | Value::Null => {
            Err(i.make_error("TypeError", "String.prototype method called on null or undefined"))
        }
        _ => ab(i.to_string(this)),
    }
}

fn install_string(it: &mut Interp) {
    let sp = it.string_proto.clone();
    it.def_method(&sp, "toString", 0, |i, this, _| Ok(Value::Str(this_string(i, &this)?)));
    it.def_method(&sp, "valueOf", 0, |i, this, _| Ok(Value::Str(this_string(i, &this)?)));
    it.def_method(&sp, "charAt", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let idx = ab(i.to_number(&arg(args, 0)))? as i64;
        Ok(match s.chars().nth(idx.max(0) as usize) {
            Some(c) => Value::from_string(c.to_string()),
            None => Value::str(""),
        })
    });
    it.def_method(&sp, "charCodeAt", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let idx = ab(i.to_number(&arg(args, 0)))? as i64;
        Ok(match s.chars().nth(idx.max(0) as usize) {
            Some(c) => Value::Num(c as u32 as f64),
            None => Value::Num(f64::NAN),
        })
    });
    it.def_method(&sp, "indexOf", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let needle = ab(i.to_string(&arg(args, 0)))?;
        Ok(match s.find(needle.as_ref()) {
            Some(byte) => Value::Num(s[..byte].chars().count() as f64),
            None => Value::Num(-1.0),
        })
    });
    it.def_method(&sp, "includes", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let needle = ab(i.to_string(&arg(args, 0)))?;
        Ok(Value::Bool(s.contains(needle.as_ref())))
    });
    it.def_method(&sp, "startsWith", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let needle = ab(i.to_string(&arg(args, 0)))?;
        Ok(Value::Bool(s.starts_with(needle.as_ref())))
    });
    it.def_method(&sp, "endsWith", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let needle = ab(i.to_string(&arg(args, 0)))?;
        Ok(Value::Bool(s.ends_with(needle.as_ref())))
    });
    it.def_method(&sp, "slice", 2, |i, this, args| {
        let s = this_string(i, &this)?;
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len() as i64;
        let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
        let end = match arg(args, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let out: String = if start < end {
            chars[start as usize..end as usize].iter().collect()
        } else {
            String::new()
        };
        Ok(Value::from_string(out))
    });
    it.def_method(&sp, "substring", 2, |i, this, args| {
        let s = this_string(i, &this)?;
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len() as i64;
        let mut a = (ab(i.to_number(&arg(args, 0)))? as i64).clamp(0, len);
        let mut b = match arg(args, 1) {
            Value::Undefined => len,
            v => (ab(i.to_number(&v))? as i64).clamp(0, len),
        };
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        Ok(Value::from_string(chars[a as usize..b as usize].iter().collect::<String>()))
    });
    it.def_method(&sp, "toUpperCase", 0, |i, this, _| {
        Ok(Value::from_string(this_string(i, &this)?.to_uppercase()))
    });
    it.def_method(&sp, "toLowerCase", 0, |i, this, _| {
        Ok(Value::from_string(this_string(i, &this)?.to_lowercase()))
    });
    // lumen strings are valid UTF-8, so they're always well-formed.
    it.def_method(&sp, "isWellFormed", 0, |i, this, _| {
        this_string(i, &this)?;
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "toWellFormed", 0, |i, this, _| Ok(Value::Str(this_string(i, &this)?)));
    it.def_method(&sp, "trim", 0, |i, this, _| {
        Ok(Value::from_string(this_string(i, &this)?.trim().to_string()))
    });
    it.def_method(&sp, "concat", 1, |i, this, args| {
        let mut s = this_string(i, &this)?.to_string();
        for a in args {
            s.push_str(&ab(i.to_string(a))?);
            if s.len() > MAX_STR_LEN {
                return Err(i.make_error("RangeError", "Invalid string length"));
            }
        }
        Ok(Value::from_string(s))
    });
    it.def_method(&sp, "repeat", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let n = ab(i.to_number(&arg(args, 0)))?;
        if n < 0.0 || n.is_infinite() {
            return Err(i.make_error("RangeError", "invalid count value"));
        }
        let count = n as usize;
        if s.len().saturating_mul(count) > MAX_STR_LEN {
            return Err(i.make_error("RangeError", "Invalid string length"));
        }
        Ok(Value::from_string(s.repeat(count)))
    });
    it.def_method(&sp, "split", 2, |i, this, args| {
        let s = this_string(i, &this)?;
        if s.len() > MAX_ARRAY_OP_LEN {
            return Err(i.make_error("RangeError", "string too large to split in this engine"));
        }
        // Regex separator: split on each match (group captures are inserted between pieces).
        if let Value::Obj(o) = &arg(args, 0) {
            if i.regexps.contains_key(&(Rc::as_ptr(o) as usize)) {
                let re = i.regexps[&(Rc::as_ptr(o) as usize)].clone();
                let chars: Vec<char> = s.chars().collect();
                let mut parts = Vec::new();
                let mut last = 0;
                for caps in regex_find_all(&re, &chars) {
                    let (a, b) = caps[0].unwrap();
                    if b == 0 {
                        continue; // skip a zero-width match at the start
                    }
                    parts.push(Value::from_string(chars[last..a].iter().collect::<String>()));
                    for g in 1..=re.ngroups {
                        parts.push(match caps[g] {
                            Some((x, y)) => Value::from_string(chars[x..y].iter().collect::<String>()),
                            None => Value::Undefined,
                        });
                    }
                    last = b;
                }
                parts.push(Value::from_string(chars[last..].iter().collect::<String>()));
                return Ok(i.make_array(parts));
            }
        }
        match arg(args, 0) {
            Value::Undefined => Ok(i.make_array(vec![Value::Str(s)])),
            sep => {
                let sep = ab(i.to_string(&sep))?;
                let parts: Vec<Value> = if sep.is_empty() {
                    s.chars().map(|c| Value::from_string(c.to_string())).collect()
                } else {
                    s.split(sep.as_ref()).map(|p| Value::from_string(p.to_string())).collect()
                };
                Ok(i.make_array(parts))
            }
        }
    });
    it.def_method(&sp, "at", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len() as i64;
        let mut idx = ab(i.to_number(&arg(args, 0)))? as i64;
        if idx < 0 {
            idx += len;
        }
        Ok(if idx < 0 || idx >= len {
            Value::Undefined
        } else {
            Value::from_string(chars[idx as usize].to_string())
        })
    });
    it.def_method(&sp, "codePointAt", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let idx = ab(i.to_number(&arg(args, 0)))? as usize;
        Ok(match s.chars().nth(idx) {
            Some(c) => Value::Num(c as u32 as f64),
            None => Value::Undefined,
        })
    });
    it.def_method(&sp, "trimStart", 0, |i, this, _| {
        Ok(Value::from_string(this_string(i, &this)?.trim_start().to_string()))
    });
    it.def_method(&sp, "trimEnd", 0, |i, this, _| {
        Ok(Value::from_string(this_string(i, &this)?.trim_end().to_string()))
    });
    it.def_method(&sp, "padStart", 1, |i, this, args| string_pad(i, this, args, true));
    it.def_method(&sp, "padEnd", 1, |i, this, args| string_pad(i, this, args, false));
    it.def_method(&sp, "match", 1, |i, this, a| {
        let s = this_string(i, &this)?;
        let re_obj = coerce_regexp(i, arg(a, 0))?;
        let re = i.regexps[&map_ptr(&re_obj).unwrap()].clone();
        let chars: Vec<char> = s.chars().collect();
        if re.global {
            ab(i.set_member(&re_obj, "lastIndex", Value::Num(0.0)))?;
            let all = regex_find_all(&re, &chars);
            if all.is_empty() {
                return Ok(Value::Null);
            }
            let items: Vec<Value> = all
                .iter()
                .map(|c| {
                    let (x, y) = c[0].unwrap();
                    Value::from_string(chars[x..y].iter().collect::<String>())
                })
                .collect();
            Ok(i.make_array(items))
        } else {
            regexp_exec(i, re_obj, &[Value::Str(s)])
        }
    });
    it.def_method(&sp, "search", 1, |i, this, a| {
        let s = this_string(i, &this)?;
        let re_obj = coerce_regexp(i, arg(a, 0))?;
        let re = i.regexps[&map_ptr(&re_obj).unwrap()].clone();
        let chars: Vec<char> = s.chars().collect();
        Ok(match re.exec_at(&chars, 0) {
            Some(c) => Value::Num(c[0].unwrap().0 as f64),
            None => Value::Num(-1.0),
        })
    });
    it.def_method(&sp, "matchAll", 1, |i, this, a| {
        let s = this_string(i, &this)?;
        let re_obj = coerce_regexp(i, arg(a, 0))?;
        let re = i.regexps[&map_ptr(&re_obj).unwrap()].clone();
        let chars: Vec<char> = s.chars().collect();
        let all = regex_find_all(&re, &chars);
        let mut results = Vec::new();
        for caps in all {
            let (x, y) = caps[0].unwrap();
            let mut items = vec![Value::from_string(chars[x..y].iter().collect::<String>())];
            for g in 1..=re.ngroups {
                items.push(match caps[g] {
                    Some((aa, bb)) => Value::from_string(chars[aa..bb].iter().collect::<String>()),
                    None => Value::Undefined,
                });
            }
            let m = i.make_array(items);
            if let Value::Obj(o) = &m {
                set_data(o, "index", Value::Num(x as f64));
                set_data(o, "input", Value::Str(s.clone()));
            }
            results.push(m);
        }
        let arr = i.make_array(results);
        Ok(make_array_iterator(i, arr, 0))
    });
    it.def_method(&sp, "replace", 2, |i, this, args| {
        let s = this_string(i, &this)?.to_string();
        // Regex pattern: replace first (or all, if global) matches with $-substitution / fn replacer.
        if let Value::Obj(o) = &arg(args, 0) {
            if i.regexps.contains_key(&(Rc::as_ptr(o) as usize)) {
                return regex_replace(i, &s, &arg(args, 0), &arg(args, 1));
            }
        }
        let pat = ab(i.to_string(&arg(args, 0)))?;
        let repl = arg(args, 1);
        match s.find(pat.as_ref()) {
            None => Ok(Value::from_string(s)),
            Some(pos) => {
                let matched = &s[pos..pos + pat.len()];
                let rep = string_replacement(i, &repl, matched, &s, pos)?;
                Ok(Value::from_string(format!("{}{}{}", &s[..pos], rep, &s[pos + pat.len()..])))
            }
        }
    });
    it.def_method(&sp, "replaceAll", 2, |i, this, args| {
        let s = this_string(i, &this)?.to_string();
        let pat = ab(i.to_string(&arg(args, 0)))?;
        let repl = arg(args, 1);
        if pat.is_empty() {
            return Ok(Value::from_string(s));
        }
        let mut out = String::new();
        let mut rest = s.as_str();
        let mut base = 0usize;
        while let Some(pos) = rest.find(pat.as_ref()) {
            out.push_str(&rest[..pos]);
            let rep = string_replacement(i, &repl, pat.as_ref(), &s, base + pos)?;
            out.push_str(&rep);
            rest = &rest[pos + pat.len()..];
            base += pos + pat.len();
        }
        out.push_str(rest);
        Ok(Value::from_string(out))
    });

    let ctor = it.make_native("String", 1, |i, _this, args| {
        let s = match args.first() {
            None => Value::str(""),
            // `String(sym)` stringifies a symbol to its descriptive string; `new String(sym)`
            // instead throws (via ToString below).
            Some(Value::Sym(s)) if !i.constructing => {
                Value::from_string(format!("Symbol({})", s.description.as_deref().unwrap_or("")))
            }
            Some(v) => Value::Str(ab(i.to_string(v))?),
        };
        Ok(maybe_box(i, s))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(sp.clone()), false, false, false));
    sp.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "fromCharCode", 1, |i, _this, args| {
        let mut s = String::new();
        for a in args {
            let n = ab(i.to_number(a))? as u32;
            s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
        }
        Ok(Value::from_string(s))
    });
    it.def_method(&ctor, "fromCodePoint", 1, |i, _this, args| {
        let mut s = String::new();
        for a in args {
            let n = ab(i.to_number(a))?;
            // Each argument must be an integer code point in [0, 0x10FFFF].
            if !n.is_finite() || n.fract() != 0.0 || n < 0.0 || n > 0x10FFFF as f64 {
                return Err(i.make_error("RangeError", "Invalid code point"));
            }
            // Lone surrogates are valid arguments but can't be a Rust char; substitute U+FFFD.
            s.push(char::from_u32(n as u32).unwrap_or('\u{FFFD}'));
        }
        Ok(Value::from_string(s))
    });
    set_builtin(&it.global, "String", Value::Obj(ctor));
}

/// Box a Number/String/Boolean primitive into a wrapper object (right prototype + exotic). Other
/// values pass through unchanged (Symbol/BigInt wrappers are not modeled yet).
fn box_primitive(i: &mut Interp, v: Value) -> Value {
    let (proto, exotic) = match &v {
        Value::Num(n) => (i.number_proto.clone(), Exotic::NumWrap(*n)),
        Value::Bool(b) => (i.boolean_proto.clone(), Exotic::BoolWrap(*b)),
        Value::Str(s) => (i.string_proto.clone(), Exotic::StrWrap(s.clone())),
        _ => return v,
    };
    let obj = Object::new(Some(proto));
    obj.borrow_mut().exotic = exotic;
    Value::Obj(obj)
}

/// Box only when a wrapper constructor is invoked via `new` (`new Number(x)` boxes, `Number(x)` does
/// not).
fn maybe_box(i: &mut Interp, v: Value) -> Value {
    if i.constructing {
        box_primitive(i, v)
    } else {
        v
    }
}

fn string_pad(i: &mut Interp, this: Value, args: &[Value], at_start: bool) -> Result<Value, Value> {
    let s = this_string(i, &this)?.to_string();
    let target = ab(i.to_number(&arg(args, 0)))? as usize;
    let cur = s.chars().count();
    if cur >= target {
        return Ok(Value::from_string(s));
    }
    let pad = match arg(args, 1) {
        Value::Undefined => " ".to_string(),
        v => ab(i.to_string(&v))?.to_string(),
    };
    if pad.is_empty() {
        return Ok(Value::from_string(s));
    }
    let need = target - cur;
    let mut fill = String::new();
    for c in pad.chars().cycle() {
        if fill.chars().count() >= need {
            break;
        }
        fill.push(c);
    }
    let fill: String = fill.chars().take(need).collect();
    Ok(Value::from_string(if at_start { format!("{fill}{s}") } else { format!("{s}{fill}") }))
}

/// Compute the replacement text for String.prototype.replace/replaceAll. A function replacer is
/// called with (match, position, whole-string); otherwise `$&` etc. patterns are honored minimally.
fn string_replacement(
    i: &mut Interp,
    repl: &Value,
    matched: &str,
    whole: &str,
    pos: usize,
) -> Result<String, Value> {
    if repl.is_callable() {
        let r = ab(i.call(
            repl.clone(),
            Value::Undefined,
            &[Value::from_string(matched.to_string()), Value::Num(pos as f64), Value::from_string(whole.to_string())],
        ))?;
        Ok(ab(i.to_string(&r))?.to_string())
    } else {
        let template = ab(i.to_string(repl))?;
        // Support the common `$&` (matched substring) and `$$` (literal $) patterns.
        Ok(template.replace("$&", matched).replace("$$", "$"))
    }
}

fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    n.trunc().rem_euclid(4294967296.0) as u32
}

/// A small deterministic PRNG for `Math.random` (lumen has no entropy source; tests only check the
/// `[0, 1)` range, not distribution).
fn next_random() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0x2545_F491_4F6C_DD1D);
    let mut x = STATE.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    (x >> 11) as f64 / (1u64 << 53) as f64
}

fn this_number(i: &mut Interp, this: &Value) -> Result<f64, Value> {
    // thisNumberValue: only a Number primitive or Number wrapper is acceptable.
    match this {
        Value::Num(n) => Ok(*n),
        Value::Obj(o) => match o.borrow().exotic {
            Exotic::NumWrap(n) => Ok(n),
            _ => Err(i.make_error("TypeError", "Number method called on incompatible receiver")),
        },
        _ => Err(i.make_error("TypeError", "Number method called on incompatible receiver")),
    }
}

fn install_number(it: &mut Interp) {
    let np = it.number_proto.clone();
    it.def_method(&np, "toString", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        let radix = match arg(args, 0) {
            Value::Undefined => 10.0,
            v => ab(i.to_number(&v))?,
        };
        if radix == 10.0 {
            Ok(Value::from_string(i.num_to_str(n)))
        } else {
            Ok(Value::from_string(to_radix_string(n, radix as u32)))
        }
    });
    it.def_method(&np, "valueOf", 0, |i, this, _| Ok(Value::Num(this_number(i, &this)?)));
    it.def_method(&np, "toExponential", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        let s = match arg(args, 0) {
            Value::Undefined => format!("{n:e}"),
            v => {
                let d = ab(i.to_number(&v))? as usize;
                format!("{n:.d$e}")
            }
        };
        // Rust prints `1e2`; JS wants `1e+2`.
        Ok(Value::from_string(s.replace('e', "e+").replace("e+-", "e-")))
    });
    it.def_method(&np, "toPrecision", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        match arg(args, 0) {
            Value::Undefined => Ok(Value::from_string(i.num_to_str(n))),
            v => {
                let p = ab(i.to_number(&v))? as usize;
                Ok(Value::from_string(format!("{n:.*}", p.saturating_sub(1).min(100))))
            }
        }
    });
    it.def_method(&np, "toFixed", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        let d = ab(i.to_number(&arg(args, 0)))?;
        // Spec: fractionDigits in 0..=100, else RangeError (also guards a giant `format!`).
        if !(0.0..=100.0).contains(&d) {
            return Err(i.make_error("RangeError", "toFixed() digits argument must be between 0 and 100"));
        }
        let digits = d as usize;
        Ok(Value::from_string(format!("{n:.digits$}")))
    });

    let ctor = it.make_native("Number", 1, |i, _this, args| {
        let n = match args.first() {
            None => 0.0,
            // Number(bigint) explicitly converts (only *implicit* ToNumber of a BigInt throws).
            Some(Value::BigInt(n)) => *n as f64,
            Some(v) => ab(i.to_number(v))?,
        };
        Ok(maybe_box(i, Value::Num(n)))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(np.clone()), false, false, false));
    np.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&ctor, "MAX_SAFE_INTEGER", Value::Num(9007199254740991.0));
    set_builtin(&ctor, "MIN_SAFE_INTEGER", Value::Num(-9007199254740991.0));
    set_builtin(&ctor, "MAX_VALUE", Value::Num(f64::MAX));
    set_builtin(&ctor, "MIN_VALUE", Value::Num(f64::MIN_POSITIVE));
    set_builtin(&ctor, "POSITIVE_INFINITY", Value::Num(f64::INFINITY));
    set_builtin(&ctor, "NEGATIVE_INFINITY", Value::Num(f64::NEG_INFINITY));
    set_builtin(&ctor, "NaN", Value::Num(f64::NAN));
    set_builtin(&ctor, "EPSILON", Value::Num(f64::EPSILON));
    it.def_method(&ctor, "isNaN", 1, |_i, _this, args| {
        Ok(Value::Bool(matches!(arg(args, 0), Value::Num(n) if n.is_nan())))
    });
    it.def_method(&ctor, "isFinite", 1, |_i, _this, args| {
        Ok(Value::Bool(matches!(arg(args, 0), Value::Num(n) if n.is_finite())))
    });
    it.def_method(&ctor, "isSafeInteger", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9007199254740991.0),
        ))
    });
    it.def_method(&ctor, "isInteger", 1, |_i, _this, args| {
        Ok(Value::Bool(matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0)))
    });
    set_builtin(&it.global, "Number", Value::Obj(ctor));
}

fn to_radix_string(n: f64, radix: u32) -> String {
    if !(2..=36).contains(&radix) {
        return "NaN".to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    let neg = n < 0.0;
    let mut int = n.abs().trunc() as u64;
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    if int == 0 {
        out.push(b'0');
    }
    while int > 0 {
        out.push(digits[(int % radix as u64) as usize]);
        int /= radix as u64;
    }
    if neg {
        out.push(b'-');
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

fn install_boolean(it: &mut Interp) {
    let bp = it.boolean_proto.clone();
    it.def_method(&bp, "toString", 0, |i, this, _| {
        Ok(Value::str(if this_boolean(i, &this)? { "true" } else { "false" }))
    });
    it.def_method(&bp, "valueOf", 0, |i, this, _| Ok(Value::Bool(this_boolean(i, &this)?)));
    let ctor = it.make_native("Boolean", 1, |i, _this, args| {
        let b = Value::Bool(i.to_boolean(&arg(args, 0)));
        Ok(maybe_box(i, b))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(bp.clone()), false, false, false));
    bp.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Boolean", Value::Obj(ctor));
}

/// thisBooleanValue: a Boolean primitive or Boolean wrapper, else TypeError.
fn this_boolean(i: &mut Interp, this: &Value) -> Result<bool, Value> {
    match this {
        Value::Bool(b) => Ok(*b),
        Value::Obj(o) => match o.borrow().exotic {
            Exotic::BoolWrap(b) => Ok(b),
            _ => Err(i.make_error("TypeError", "Boolean method called on incompatible receiver")),
        },
        _ => Err(i.make_error("TypeError", "Boolean method called on incompatible receiver")),
    }
}

fn install_symbol(it: &mut Interp) {
    let sp = it.symbol_proto.clone();
    it.def_method(&sp, "toString", 0, |i, this, _| match &this {
        Value::Sym(s) => {
            Ok(Value::from_string(format!("Symbol({})", s.description.as_deref().unwrap_or(""))))
        }
        _ => Err(i.make_error("TypeError", "Symbol.prototype.toString requires a symbol")),
    });
    it.def_method(&sp, "valueOf", 0, |i, this, _| match this {
        Value::Sym(_) => Ok(this),
        _ => Err(i.make_error("TypeError", "Symbol.prototype.valueOf requires a symbol")),
    });

    let ctor = it.make_native("Symbol", 0, |i, _this, args| {
        if i.constructing {
            return Err(i.make_error("TypeError", "Symbol is not a constructor"));
        }
        let desc = match arg(args, 0) {
            Value::Undefined => None,
            v => Some(ab(i.to_string(&v))?),
        };
        Ok(i.new_symbol(desc))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(sp.clone()), false, false, false));
    sp.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));

    // Well-known symbols (each a unique, frozen instance on the constructor).
    for name in [
        "iterator", "asyncIterator", "hasInstance", "isConcatSpreadable", "match", "matchAll",
        "replace", "search", "species", "split", "toPrimitive", "toStringTag", "unscopables",
    ] {
        let sym = it.new_symbol(Some(Rc::from(format!("Symbol.{name}").as_str())));
        if name == "iterator" {
            if let Value::Sym(d) = &sym {
                it.iterator_sym = Some(d.clone());
            }
        }
        ctor.borrow_mut().props.insert(name, Property::data(sym, false, false, false));
    }

    it.def_method(&ctor, "for", 1, |i, _this, args| {
        let key = ab(i.to_string(&arg(args, 0)))?.to_string();
        if let Some(d) = i.sym_for.get(&key).cloned() {
            return Ok(Value::Sym(d));
        }
        let sym = i.new_symbol(Some(Rc::from(key.as_str())));
        if let Value::Sym(d) = &sym {
            i.sym_for.insert(key, d.clone());
        }
        Ok(sym)
    });
    it.def_method(&ctor, "keyFor", 1, |i, _this, args| {
        if let Value::Sym(s) = arg(args, 0) {
            for (k, d) in &i.sym_for {
                if d.id == s.id {
                    return Ok(Value::from_string(k.clone()));
                }
            }
        }
        Ok(Value::Undefined)
    });
    set_builtin(&it.global, "Symbol", Value::Obj(ctor));
}

fn bigint_to_radix(mut n: i128, radix: u32) -> String {
    if radix == 10 || !(2..=36).contains(&radix) {
        return n.to_string();
    }
    if n == 0 {
        return "0".to_string();
    }
    let neg = n < 0;
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let r = radix as i128;
    n = n.abs();
    while n > 0 {
        out.push(digits[(n % r) as usize]);
        n /= r;
    }
    if neg {
        out.push(b'-');
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

fn install_bigint(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("BigInt", proto.clone());
    it.def_method(&proto, "toString", 1, |i, this, a| {
        let n = match this {
            Value::BigInt(n) => n,
            _ => return Err(i.make_error("TypeError", "BigInt.prototype.toString requires a BigInt")),
        };
        let radix = match arg(a, 0) {
            Value::Undefined => 10,
            v => ab(i.to_number(&v))? as u32,
        };
        Ok(Value::from_string(bigint_to_radix(n, radix)))
    });
    it.def_method(&proto, "valueOf", 0, |i, this, _| match this {
        Value::BigInt(_) => Ok(this),
        _ => Err(i.make_error("TypeError", "BigInt.prototype.valueOf requires a BigInt")),
    });
    let ctor = it.make_native("BigInt", 1, |i, _t, a| {
        if i.constructing {
            return Err(i.make_error("TypeError", "BigInt is not a constructor"));
        }
        match arg(a, 0) {
        Value::BigInt(n) => Ok(Value::BigInt(n)),
        Value::Num(n) => {
            if n.is_finite() && n.fract() == 0.0 {
                Ok(Value::BigInt(n as i128))
            } else {
                Err(i.make_error("RangeError", "The number is not a safe integer"))
            }
        }
        Value::Bool(b) => Ok(Value::BigInt(if b { 1 } else { 0 })),
        Value::Str(s) => {
            let t = s.trim();
            let t = if t.is_empty() { "0" } else { t };
            t.parse::<i128>()
                .map(Value::BigInt)
                .map_err(|_| i.make_error("SyntaxError", "Cannot convert string to a BigInt"))
        }
        _ => Err(i.make_error("TypeError", "Cannot convert value to a BigInt")),
        }
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
    proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "asIntN", 2, |i, _t, a| {
        let bits = ab(i.to_number(&arg(a, 0)))? as u32;
        let n = match arg(a, 1) {
            Value::BigInt(n) => n,
            _ => return Err(i.make_error("TypeError", "asIntN requires a BigInt")),
        };
        if bits == 0 {
            return Ok(Value::BigInt(0));
        }
        if bits >= 128 {
            return Ok(Value::BigInt(n));
        }
        let m = 1i128 << bits;
        let mut r = n.rem_euclid(m);
        if r >= m / 2 {
            r -= m;
        }
        Ok(Value::BigInt(r))
    });
    it.def_method(&ctor, "asUintN", 2, |i, _t, a| {
        let bits = ab(i.to_number(&arg(a, 0)))? as u32;
        let n = match arg(a, 1) {
            Value::BigInt(n) => n,
            _ => return Err(i.make_error("TypeError", "asUintN requires a BigInt")),
        };
        if bits == 0 {
            return Ok(Value::BigInt(0));
        }
        if bits >= 127 {
            return Ok(Value::BigInt(n));
        }
        Ok(Value::BigInt(n.rem_euclid(1i128 << bits)))
    });
    set_builtin(&it.global, "BigInt", Value::Obj(ctor));
}

fn install_math(it: &mut Interp) {
    let math = it.new_object();
    set_builtin(&math, "PI", Value::Num(std::f64::consts::PI));
    set_builtin(&math, "E", Value::Num(std::f64::consts::E));
    set_builtin(&math, "LN2", Value::Num(std::f64::consts::LN_2));
    set_builtin(&math, "LN10", Value::Num(std::f64::consts::LN_10));
    set_builtin(&math, "SQRT2", Value::Num(std::f64::consts::SQRT_2));
    macro_rules! unary {
        ($name:expr, $f:expr) => {
            it.def_method(&math, $name, 1, |i, _t, a| {
                let x = ab(i.to_number(&arg(a, 0)))?;
                Ok(Value::Num($f(x)))
            });
        };
    }
    unary!("abs", f64::abs);
    unary!("floor", f64::floor);
    unary!("ceil", f64::ceil);
    unary!("round", |x: f64| (x + 0.5).floor());
    unary!("trunc", f64::trunc);
    unary!("sqrt", f64::sqrt);
    unary!("cbrt", f64::cbrt);
    unary!("sign", |x: f64| if x.is_nan() || x == 0.0 { x } else { x.signum() });
    unary!("expm1", f64::exp_m1);
    unary!("log1p", f64::ln_1p);
    unary!("sinh", f64::sinh);
    unary!("cosh", f64::cosh);
    unary!("tanh", f64::tanh);
    unary!("asinh", f64::asinh);
    unary!("acosh", f64::acosh);
    unary!("atanh", f64::atanh);
    unary!("fround", |x: f64| x as f32 as f64);
    unary!("clz32", |x: f64| (to_uint32(x)).leading_zeros() as f64);
    it.def_method(&math, "hypot", 2, |i, _t, a| {
        let mut sum = 0.0;
        for v in a {
            let n = ab(i.to_number(v))?;
            sum += n * n;
        }
        Ok(Value::Num(sum.sqrt()))
    });
    it.def_method(&math, "imul", 2, |i, _t, a| {
        let x = to_uint32(ab(i.to_number(&arg(a, 0)))?) as i32;
        let y = to_uint32(ab(i.to_number(&arg(a, 1)))?) as i32;
        Ok(Value::Num(x.wrapping_mul(y) as f64))
    });
    it.def_method(&math, "random", 0, |_i, _t, _a| Ok(Value::Num(next_random())));
    unary!("log", f64::ln);
    unary!("log2", f64::log2);
    unary!("log10", f64::log10);
    unary!("exp", f64::exp);
    unary!("sin", f64::sin);
    unary!("cos", f64::cos);
    unary!("tan", f64::tan);
    unary!("atan", f64::atan);
    unary!("asin", f64::asin);
    unary!("acos", f64::acos);
    it.def_method(&math, "pow", 2, |i, _t, a| {
        Ok(Value::Num(ab(i.to_number(&arg(a, 0)))?.powf(ab(i.to_number(&arg(a, 1)))?)))
    });
    it.def_method(&math, "atan2", 2, |i, _t, a| {
        Ok(Value::Num(ab(i.to_number(&arg(a, 0)))?.atan2(ab(i.to_number(&arg(a, 1)))?)))
    });
    it.def_method(&math, "max", 2, |i, _t, a| {
        let mut m = f64::NEG_INFINITY;
        for v in a {
            let n = ab(i.to_number(v))?;
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n > m {
                m = n;
            }
        }
        Ok(Value::Num(m))
    });
    it.def_method(&math, "min", 2, |i, _t, a| {
        let mut m = f64::INFINITY;
        for v in a {
            let n = ab(i.to_number(v))?;
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n < m {
                m = n;
            }
        }
        Ok(Value::Num(m))
    });
    set_builtin(&it.global, "Math", Value::Obj(math));
}

fn install_errors(it: &mut Interp) {
    // Base Error first (its prototype's proto is Object.prototype).
    let names = ["Error", "TypeError", "RangeError", "ReferenceError", "SyntaxError", "EvalError", "URIError"];
    // Create Error.prototype.
    let error_proto = Object::new(Some(it.object_proto.clone()));
    set_builtin(&error_proto, "name", Value::str("Error"));
    set_builtin(&error_proto, "message", Value::str(""));
    it.def_method(&error_proto, "toString", 0, |i, this, _| {
        let name = match ab(i.get_member(&this, "name"))? {
            Value::Undefined => "Error".to_string(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        let msg = match ab(i.get_member(&this, "message"))? {
            Value::Undefined => String::new(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        Ok(Value::from_string(if msg.is_empty() {
            name
        } else if name.is_empty() {
            msg
        } else {
            format!("{name}: {msg}")
        }))
    });
    it.error_protos.insert("Error", error_proto.clone());

    for name in names {
        let proto = if name == "Error" {
            error_proto.clone()
        } else {
            let p = Object::new(Some(error_proto.clone()));
            set_builtin(&p, "name", Value::str(name));
            set_builtin(&p, "message", Value::str(""));
            it.error_protos.insert(name, p.clone());
            p
        };
        // A distinct native constructor per error kind (fn pointers can't capture the name).
        let ctor_fn: NativeFn = match name {
            "Error" => |i, _t, a| Ok(make_err(i, "Error", a)),
            "TypeError" => |i, _t, a| Ok(make_err(i, "TypeError", a)),
            "RangeError" => |i, _t, a| Ok(make_err(i, "RangeError", a)),
            "ReferenceError" => |i, _t, a| Ok(make_err(i, "ReferenceError", a)),
            "SyntaxError" => |i, _t, a| Ok(make_err(i, "SyntaxError", a)),
            "EvalError" => |i, _t, a| Ok(make_err(i, "EvalError", a)),
            "URIError" => |i, _t, a| Ok(make_err(i, "URIError", a)),
            _ => unreachable!(),
        };
        let ctor = it.make_native(name, 1, ctor_fn);
        ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(proto.clone()), false, false, false));
        proto.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
        set_builtin(&it.global, name, Value::Obj(ctor));
    }
}

fn make_err(i: &mut Interp, kind: &str, args: &[Value]) -> Value {
    let err = i.make_error(kind, "");
    if let Some(msg) = args.first() {
        if !matches!(msg, Value::Undefined) {
            if let Ok(s) = i.to_string(msg) {
                let _ = i.set_member(&err, "message", Value::Str(s));
            }
        }
    }
    err
}

fn install_globals(it: &mut Interp) {
    global_fn(it, "parseInt", 2, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        let radix = match arg(a, 1) {
            Value::Undefined => 0,
            v => ab(i.to_number(&v))? as u32,
        };
        Ok(Value::Num(parse_int(&s, radix)))
    });
    global_fn(it, "parseFloat", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        Ok(Value::Num(parse_float(&s)))
    });
    // Number.parseInt / Number.parseFloat are the same functions as the globals.
    if let Some(num) = it.global.borrow().props.get("Number").map(|p| p.value.clone()) {
        let pi = it.global.borrow().props.get("parseInt").map(|p| p.value.clone());
        let pf = it.global.borrow().props.get("parseFloat").map(|p| p.value.clone());
        if let (Value::Obj(n), Some(pi), Some(pf)) = (num, pi, pf) {
            n.borrow_mut().props.insert("parseInt", Property::builtin(pi));
            n.borrow_mut().props.insert("parseFloat", Property::builtin(pf));
        }
    }
    global_fn(it, "isNaN", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_nan()))
    });
    global_fn(it, "isFinite", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_finite()))
    });
    global_fn(it, "encodeURIComponent", 1, |i, _t, a| {
        Ok(Value::from_string(uri_encode(&ab(i.to_string(&arg(a, 0)))?, "")))
    });
    global_fn(it, "encodeURI", 1, |i, _t, a| {
        Ok(Value::from_string(uri_encode(&ab(i.to_string(&arg(a, 0)))?, ";,/?:@&=+$#")))
    });
    global_fn(it, "decodeURIComponent", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_decode(&s)
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });
    global_fn(it, "decodeURI", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_decode(&s)
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });

    // Indirect eval: runs in the global scope. (A *direct* `eval(...)` call is intercepted in
    // `eval_call` and run in the caller's scope; both share this same function object.)
    let eval_fn = it.make_native("eval", 1, |i, _this, args| {
        let code = match arg(args, 0) {
            Value::Str(s) => s,
            other => return Ok(other),
        };
        let body = crate::parser::parse_script(&code, false)
            .map_err(|e| i.make_error("SyntaxError", e.message))?;
        let env = i.global_env.clone();
        ab(i.eval_in_scope(&body, &env))
    });
    set_builtin(&it.global, "eval", Value::Obj(eval_fn.clone()));
    it.eval_fn = Some(eval_fn);
}

fn install_console(it: &mut Interp) {
    let console = it.new_object();
    let log: NativeFn = |i, _t, a| {
        let parts: Result<Vec<String>, Value> =
            a.iter().map(|v| ab(i.to_string(v)).map(|s| s.to_string())).collect();
        i.console.push(parts?.join(" "));
        Ok(Value::Undefined)
    };
    for name in ["log", "info", "warn", "error", "debug"] {
        it.def_method(&console, name, 0, log);
    }
    set_builtin(&it.global, "console", Value::Obj(console));
}

fn parse_int(s: &str, mut radix: u32) -> f64 {
    let t = s.trim();
    let (neg, mut body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if radix == 0 {
        if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            radix = 16;
            body = rest;
        } else {
            radix = 10;
        }
    } else if radix == 16 {
        if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            body = rest;
        }
    }
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let mut acc = 0.0;
    let mut any = false;
    for c in body.chars() {
        match c.to_digit(radix) {
            Some(d) => {
                acc = acc * radix as f64 + d as f64;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return f64::NAN;
    }
    if neg {
        -acc
    } else {
        acc
    }
}

fn parse_float(s: &str) -> f64 {
    let t = s.trim();
    // Take the longest leading prefix that parses as a float.
    let mut end = 0;
    let bytes = t.as_bytes();
    let mut seen_dot = false;
    let mut seen_e = false;
    while end < bytes.len() {
        let c = bytes[end] as char;
        if c.is_ascii_digit() {
        } else if c == '.' && !seen_dot && !seen_e {
            seen_dot = true;
        } else if (c == 'e' || c == 'E') && !seen_e && end > 0 {
            seen_e = true;
        } else if (c == '+' || c == '-') && (end == 0 || matches!(bytes[end - 1] as char, 'e' | 'E')) {
        } else {
            break;
        }
        end += 1;
    }
    t[..end].parse::<f64>().unwrap_or(f64::NAN)
}
