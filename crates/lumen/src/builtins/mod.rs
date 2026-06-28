//! The built-in objects and global functions. This is the realm: a freshly-constructed [`Interp`]
//! calls [`install`] to populate `globalThis`, the standard constructors/prototypes, `Math`, and
//! the global functions. The set grows as the test262 score climbs — it is intentionally a subset.

use crate::interpreter::{Abrupt, Interp, MAX_ARRAY_OP_LEN, MAX_STR_LEN};
use crate::value::*;
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
    install_array(it);
    install_string(it);
    install_number(it);
    install_boolean(it);
    install_math(it);
    install_errors(it);
    install_globals(it);
    install_console(it);
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
    it.def_method(&op, "toString", 0, |_i, _this, _args| Ok(Value::str("[object Object]")));
    it.def_method(&op, "valueOf", 0, |_i, this, _args| Ok(this));

    let ctor = it.make_native("Object", 1, |i, _this, args| {
        Ok(match arg(args, 0) {
            Value::Undefined | Value::Null => Value::Obj(i.new_object()),
            Value::Obj(o) => Value::Obj(o),
            other => other, // primitive wrappers are a TODO; returning the primitive is harmless here
        })
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(op.clone()), false, false, false));
    op.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));

    it.def_method(&ctor, "keys", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Object.keys called on non-object")),
        };
        let keys: Vec<Value> = o
            .borrow()
            .props
            .iter()
            .filter(|(_, p)| p.enumerable)
            .map(|(k, _)| Value::Str(k.clone()))
            .collect();
        Ok(i.make_array(keys))
    });
    it.def_method(&ctor, "getOwnPropertyNames", 1, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "called on non-object")),
        };
        let keys: Vec<Value> = o.borrow().props.keys().into_iter().map(Value::Str).collect();
        Ok(i.make_array(keys))
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
    set_builtin(&it.global, "Array", Value::Obj(ctor));
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

    let ctor = it.make_native("String", 1, |i, _this, args| {
        match args.first() {
            None => Ok(Value::str("")),
            Some(v) => Ok(Value::Str(ab(i.to_string(v))?)),
        }
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
    set_builtin(&it.global, "String", Value::Obj(ctor));
}

fn this_number(i: &mut Interp, this: &Value) -> Result<f64, Value> {
    match this {
        Value::Num(n) => Ok(*n),
        Value::Obj(o) => match &o.borrow().exotic {
            Exotic::NumWrap(n) => Ok(*n),
            _ => ab(i.to_number(this)),
        },
        _ => ab(i.to_number(this)),
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
        match args.first() {
            None => Ok(Value::Num(0.0)),
            Some(v) => Ok(Value::Num(ab(i.to_number(v))?)),
        }
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
    it.def_method(&bp, "toString", 0, |_i, this, _| {
        Ok(Value::str(if truthy_bool(&this) { "true" } else { "false" }))
    });
    it.def_method(&bp, "valueOf", 0, |_i, this, _| Ok(Value::Bool(truthy_bool(&this))));
    let ctor = it.make_native("Boolean", 1, |i, _this, args| {
        Ok(Value::Bool(i.to_boolean(&arg(args, 0))))
    });
    ctor.borrow_mut().props.insert("prototype", Property::data(Value::Obj(bp.clone()), false, false, false));
    bp.borrow_mut().props.insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Boolean", Value::Obj(ctor));
}

fn truthy_bool(this: &Value) -> bool {
    match this {
        Value::Bool(b) => *b,
        Value::Obj(o) => matches!(o.borrow().exotic, Exotic::BoolWrap(true)),
        _ => false,
    }
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
    unary!("sign", f64::signum);
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
    global_fn(it, "isNaN", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_nan()))
    });
    global_fn(it, "isFinite", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_finite()))
    });
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
