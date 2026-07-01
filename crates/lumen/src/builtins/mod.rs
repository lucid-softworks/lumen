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
    // The global value properties are { writable:false, enumerable:false, configurable:false }.
    for (name, val) in [
        ("undefined", Value::Undefined),
        ("NaN", Value::Num(f64::NAN)),
        ("Infinity", Value::Num(f64::INFINITY)),
    ] {
        g.borrow_mut()
            .props
            .insert(name, Property::data(val, false, false, false));
    }
    set_builtin(&g, "globalThis", Value::Obj(g.clone()));

    install_function_proto(it);
    install_object(it);
    // Symbol before Array/String so `Symbol.iterator` exists when they define `@@iterator`.
    install_symbol(it);
    // Function.prototype[@@hasInstance] (default OrdinaryHasInstance) — installed after Symbol so the
    // well-known key exists; non-writable/non-configurable.
    if let Some(key) = well_known_key(it, "hasInstance") {
        let f = it.make_native("[Symbol.hasInstance]", 1, |i, this, a| {
            Ok(Value::Bool(ab(i.ordinary_has_instance(&this, &arg(a, 0)))?))
        });
        it.function_proto
            .borrow_mut()
            .props
            .insert(key, Property::data(Value::Obj(f), false, false, false));
    }
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
    install_disposable_stack(it);
    install_shadow_realm(it);
    crate::temporal::install(it);
}

/// `DisposableStack` (explicit resource management). Disposers are stored as `[fn, thisArg]` pairs
/// in an internal array and run last-in-first-out on `dispose()`.
fn ds_list(i: &mut Interp, this: &Value) -> Result<Value, Value> {
    let o = this_obj(this).ok_or_else(|| i.make_error("TypeError", "not a DisposableStack"))?;
    if !o.borrow().props.contains("__ds") {
        return Err(i.make_error("TypeError", "not a DisposableStack"));
    }
    ab(i.get_member(this, "__ds"))
}
fn ds_disposed(i: &mut Interp, this: &Value) -> Result<bool, Value> {
    let v = ab(i.get_member(this, "__ds_disposed"))?;
    Ok(i.to_boolean(&v))
}
fn ds_push(i: &mut Interp, this: &Value, entry: Value) -> Result<(), Value> {
    let list = ds_list(i, this)?;
    let push = ab(i.get_member(&list, "push"))?;
    ab(i.call(push, list, &[entry]))?;
    Ok(())
}

fn install_disposable_stack(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("DisposableStack", proto.clone());

    it.def_method(&proto, "use", 1, |i, this, a| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "DisposableStack already disposed"));
        }
        let v = arg(a, 0);
        if !matches!(v, Value::Undefined | Value::Null) {
            let key = well_known_key(i, "dispose").unwrap_or_default();
            let disp = ab(i.get_member(&v, &key))?;
            if !disp.is_callable() {
                return Err(i.make_error("TypeError", "value is not disposable"));
            }
            let entry = i.make_array(vec![disp, v.clone()]);
            ds_push(i, &this, entry)?;
        }
        Ok(v)
    });
    it.def_method(&proto, "adopt", 2, |i, this, a| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "DisposableStack already disposed"));
        }
        let v = arg(a, 0);
        let on = arg(a, 1);
        if !on.is_callable() {
            return Err(i.make_error("TypeError", "onDispose is not callable"));
        }
        let entry = i.make_array(vec![on, Value::Undefined, v.clone()]);
        ds_push(i, &this, entry)?;
        Ok(v)
    });
    it.def_method(&proto, "defer", 1, |i, this, a| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "DisposableStack already disposed"));
        }
        let on = arg(a, 0);
        if !on.is_callable() {
            return Err(i.make_error("TypeError", "onDispose is not callable"));
        }
        let entry = i.make_array(vec![on, Value::Undefined]);
        ds_push(i, &this, entry)?;
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "dispose", 0, |i, this, _| {
        if ds_disposed(i, &this)? {
            return Ok(Value::Undefined);
        }
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        let list = ds_list(i, &this)?;
        let len = ab(i.get_member(&list, "length"))?;
        let n = ab(i.to_number(&len))? as i64;
        for idx in (0..n).rev() {
            let entry = ab(i.get_member(&list, &idx.to_string()))?;
            let f = ab(i.get_member(&entry, "0"))?;
            let t = ab(i.get_member(&entry, "1"))?;
            let has_arg = ab(i.get_member(&entry, "length"))?;
            let args = if ab(i.to_number(&has_arg))? >= 3.0 {
                vec![ab(i.get_member(&entry, "2"))?]
            } else {
                Vec::new()
            };
            ab(i.call(f, t, &args))?;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "move", 0, |i, this, _| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "DisposableStack already disposed"));
        }
        let proto = i.extra_protos.get("DisposableStack").cloned();
        let fresh = Object::new(proto);
        let list = ds_list(i, &this)?;
        set_internal(&fresh, "__ds", list);
        set_internal(&fresh, "__ds_disposed", Value::Bool(false));
        let empty = i.make_array(Vec::new());
        set_internal(this.as_obj().unwrap(), "__ds", empty);
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        Ok(Value::Obj(fresh))
    });
    // `disposed` accessor + `[Symbol.dispose]` alias for `dispose`.
    let disposed_getter = it.make_native("get disposed", 0, |i, this, _| {
        Ok(Value::Bool(ds_disposed(i, &this)?))
    });
    proto.borrow_mut().props.insert(
        "disposed",
        Property {
            value: Value::Undefined,
            get: Some(Value::Obj(disposed_getter)),
            set: None,
            accessor: true,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    if let Some(key) = well_known_key(it, "dispose") {
        let dispose = proto.borrow().props.get("dispose").map(|p| p.value.clone());
        if let Some(d) = dispose {
            proto.borrow_mut().props.insert(key, Property::builtin(d));
        }
    }

    let ctor = it.make_native("DisposableStack", 0, |i, _t, _a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "Constructor DisposableStack requires 'new'"));
        }
        let obj = Object::new(i.extra_protos.get("DisposableStack").cloned());
        let list = i.make_array(Vec::new());
        set_internal(&obj, "__ds", list);
        set_internal(&obj, "__ds_disposed", Value::Bool(false));
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "DisposableStack", Value::Obj(ctor));

    install_async_disposable_stack(it);
}

/// `AsyncDisposableStack`: like DisposableStack but disposers may be async; `disposeAsync` returns a
/// promise and awaits each disposer (lumen runs them synchronously and settles the promise).
fn install_async_disposable_stack(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos
        .insert("AsyncDisposableStack", proto.clone());

    it.def_method(&proto, "use", 1, |i, this, a| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "AsyncDisposableStack already disposed"));
        }
        let v = arg(a, 0);
        if !matches!(v, Value::Undefined | Value::Null) {
            // Prefer @@asyncDispose, falling back to @@dispose.
            let akey = well_known_key(i, "asyncDispose").unwrap_or_default();
            let mut disp = ab(i.get_member(&v, &akey))?;
            if !disp.is_callable() {
                let dkey = well_known_key(i, "dispose").unwrap_or_default();
                disp = ab(i.get_member(&v, &dkey))?;
            }
            if !disp.is_callable() {
                return Err(i.make_error("TypeError", "value is not async-disposable"));
            }
            let entry = i.make_array(vec![disp, v.clone()]);
            ds_push(i, &this, entry)?;
        }
        Ok(v)
    });
    it.def_method(&proto, "adopt", 2, |i, this, a| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "AsyncDisposableStack already disposed"));
        }
        let v = arg(a, 0);
        let on = arg(a, 1);
        if !on.is_callable() {
            return Err(i.make_error("TypeError", "onDisposeAsync is not callable"));
        }
        let entry = i.make_array(vec![on, Value::Undefined, v.clone()]);
        ds_push(i, &this, entry)?;
        Ok(v)
    });
    it.def_method(&proto, "defer", 1, |i, this, a| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "AsyncDisposableStack already disposed"));
        }
        let on = arg(a, 0);
        if !on.is_callable() {
            return Err(i.make_error("TypeError", "onDisposeAsync is not callable"));
        }
        let entry = i.make_array(vec![on, Value::Undefined]);
        ds_push(i, &this, entry)?;
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "disposeAsync", 0, |i, this, _| {
        let result = i.new_promise();
        if ds_disposed(i, &this).unwrap_or(true) {
            i.resolve_promise(&result, Value::Undefined);
            return Ok(result);
        }
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        // Run disposers LIFO, awaiting any returned promise; reject on the first failure.
        let outcome = (|| -> Result<(), Value> {
            let list = ds_list(i, &this)?;
            let len = ab(i.get_member(&list, "length"))?;
            let n = ab(i.to_number(&len))? as i64;
            for idx in (0..n).rev() {
                let entry = ab(i.get_member(&list, &idx.to_string()))?;
                let f = ab(i.get_member(&entry, "0"))?;
                let t = ab(i.get_member(&entry, "1"))?;
                let has_arg = ab(i.get_member(&entry, "length"))?;
                let args = if ab(i.to_number(&has_arg))? >= 3.0 {
                    vec![ab(i.get_member(&entry, "2"))?]
                } else {
                    Vec::new()
                };
                let r = ab(i.call(f, t, &args))?;
                ab(i.await_value(r))?;
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => i.resolve_promise(&result, Value::Undefined),
            Err(e) => i.reject_promise(&result, e),
        }
        Ok(result)
    });
    it.def_method(&proto, "move", 0, |i, this, _| {
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "AsyncDisposableStack already disposed"));
        }
        let fresh = Object::new(i.extra_protos.get("AsyncDisposableStack").cloned());
        let list = ds_list(i, &this)?;
        set_internal(&fresh, "__ds", list);
        set_internal(&fresh, "__ds_disposed", Value::Bool(false));
        let empty = i.make_array(Vec::new());
        set_internal(this.as_obj().unwrap(), "__ds", empty);
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        Ok(Value::Obj(fresh))
    });
    let disposed_getter = it.make_native("get disposed", 0, |i, this, _| {
        Ok(Value::Bool(ds_disposed(i, &this)?))
    });
    proto.borrow_mut().props.insert(
        "disposed",
        Property {
            value: Value::Undefined,
            get: Some(Value::Obj(disposed_getter)),
            set: None,
            accessor: true,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    if let Some(key) = well_known_key(it, "asyncDispose") {
        let d = proto
            .borrow()
            .props
            .get("disposeAsync")
            .map(|p| p.value.clone());
        if let Some(d) = d {
            proto.borrow_mut().props.insert(key, Property::builtin(d));
        }
    }

    let ctor = it.make_native("AsyncDisposableStack", 0, |i, _t, _a| {
        if !i.constructing {
            return Err(i.make_error(
                "TypeError",
                "Constructor AsyncDisposableStack requires 'new'",
            ));
        }
        let obj = Object::new(i.extra_protos.get("AsyncDisposableStack").cloned());
        let list = i.make_array(Vec::new());
        set_internal(&obj, "__ds", list);
        set_internal(&obj, "__ds_disposed", Value::Bool(false));
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "AsyncDisposableStack", Value::Obj(ctor));
}

/// WeakRef / FinalizationRegistry. lumen's collector never observably reclaims during a test, so
/// WeakRef holds its target (deref always returns it) and FinalizationRegistry callbacks never fire.
fn install_weak_refs(it: &mut Interp) {
    let wr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&wr_proto, "deref", 0, |i, this, _| {
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("__target")) {
            return Err(i.make_error("TypeError", "deref called on a non-WeakRef"));
        }
        ab(i.get_member(&this, "__target"))
    });
    let wr_ctor = it.make_native("WeakRef", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "WeakRef requires 'new'"));
        }
        let target = arg(a, 0);
        if !can_be_held_weakly(i, &target) {
            return Err(i.make_error("TypeError", "WeakRef target must be an object or symbol"));
        }
        let obj = new_from_ctor(i, "WeakRef")?;
        set_internal(&obj, "__target", target);
        Ok(Value::Obj(obj))
    });
    it.extra_protos.insert("WeakRef", wr_proto.clone());
    wr_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(wr_proto.clone()), false, false, false),
    );
    wr_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(wr_ctor.clone())),
    );
    if let Some(key) = well_known_key(it, "toStringTag") {
        wr_proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("WeakRef"), false, false, true),
        );
    }
    set_builtin(&it.global, "WeakRef", Value::Obj(wr_ctor));

    let fr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&fr_proto, "register", 2, |i, this, a| {
        // Brand check, then: target must be registerable, distinct from its held value, and any
        // unregister token must itself be registerable.
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("__fr")) {
            return Err(i.make_error("TypeError", "register called on a non-FinalizationRegistry"));
        }
        let target = arg(a, 0);
        if !can_be_held_weakly(i, &target) {
            return Err(i.make_error("TypeError", "target cannot be held weakly"));
        }
        if same_value(&target, &arg(a, 1)) {
            return Err(i.make_error("TypeError", "target and held value must not be the same"));
        }
        let token = arg(a, 2);
        if !matches!(token, Value::Undefined) && !can_be_held_weakly(i, &token) {
            return Err(i.make_error("TypeError", "unregister token cannot be held weakly"));
        }
        Ok(Value::Undefined)
    });
    it.def_method(&fr_proto, "unregister", 1, |i, this, a| {
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("__fr")) {
            return Err(i.make_error(
                "TypeError",
                "unregister called on a non-FinalizationRegistry",
            ));
        }
        if !can_be_held_weakly(i, &arg(a, 0)) {
            return Err(i.make_error("TypeError", "unregister token cannot be held weakly"));
        }
        Ok(Value::Bool(false))
    });
    let fr_ctor = it.make_native("FinalizationRegistry", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "FinalizationRegistry requires 'new'"));
        }
        if !arg(a, 0).is_callable() {
            return Err(i.make_error("TypeError", "cleanup callback must be callable"));
        }
        let obj = new_from_ctor(i, "FinalizationRegistry")
            .unwrap_or_else(|_| Object::new(i.extra_protos.get("FinalizationRegistry").cloned()));
        set_internal(&obj, "__fr", Value::Bool(true));
        Ok(Value::Obj(obj))
    });
    it.extra_protos
        .insert("FinalizationRegistry", fr_proto.clone());
    fr_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(fr_proto.clone()), false, false, false),
    );
    fr_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(fr_ctor.clone())),
    );
    if let Some(key) = well_known_key(it, "toStringTag") {
        fr_proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("FinalizationRegistry"), false, false, true),
        );
    }
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
        if matches!(
            info.kind,
            TaKind::F16 | TaKind::F32 | TaKind::F64 | TaKind::U8Clamped
        ) {
            return Err(i.make_error("TypeError", "Atomics requires an integer TypedArray"));
        }
        // ValidateAtomicAccess: ToIndex truncates toward zero (NaN→0); a negative or out-of-bounds
        // result is a RangeError, but a fractional request index is simply truncated.
        let raw = ab(i.to_number(&arg(args, 1)))?;
        let idx = if raw.is_nan() { 0.0 } else { raw.trunc() };
        if idx < 0.0 || !idx.is_finite() || idx as usize >= info.len {
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
        write_i128(i, &info, idx, val);
        // store returns the coerced value itself, not the (possibly wrapped) stored representation.
        Ok(if info.kind.is_bigint() {
            Value::BigInt(val)
        } else {
            Value::Num(val as f64)
        })
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
        // Waitable types are only Int32Array and BigInt64Array, over a *shared* buffer.
        if !matches!(info.kind, TaKind::I32 | TaKind::I64) {
            return Err(i.make_error(
                "TypeError",
                "Atomics.wait requires an Int32 or BigInt64 array",
            ));
        }
        let id = match i.shared_buffers.get(&info.buffer) {
            Some(&id) => id,
            None => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.wait requires a SharedArrayBuffer-backed array",
                ))
            }
        };
        let expected = operand(i, &info, &arg(a, 2))?;
        // timeout: ToNumber (NaN → +Infinity), clamped at 0.
        let q = ab(i.to_number(&arg(a, 3)))?;
        let timeout = if q.is_nan() || q == f64::INFINITY {
            None
        } else {
            Some(std::time::Duration::from_millis(q.max(0.0) as u64))
        };
        // Only an agent with [[CanBlock]] may suspend (the main agent cannot).
        if !i.can_block {
            return Err(i.make_error(
                "TypeError",
                "Atomics.wait: the calling agent cannot be suspended",
            ));
        }
        if read_i128(i, &info, idx) != expected {
            return Ok(Value::str("not-equal"));
        }
        let byte_index = info.offset + idx * info.kind.elsize();
        let woken = crate::interpreter::futex_wait(id, byte_index, timeout);
        Ok(Value::str(if woken { "ok" } else { "timed-out" }))
    });
    it.def_method(&atomics, "notify", 3, |i, _t, a| {
        let (info, idx) = target(i, a)?;
        if !matches!(info.kind, TaKind::I32 | TaKind::I64) {
            return Err(i.make_error(
                "TypeError",
                "Atomics.notify requires an Int32 or BigInt64 array",
            ));
        }
        // count = ToIntegerOrInfinity(arg2), default +Infinity (= all); negative clamps to 0.
        let count: i64 = match arg(a, 2) {
            Value::Undefined => -1,
            v => {
                let n = ab(i.to_number(&v))?;
                if n.is_nan() || n < 0.0 {
                    0
                } else if n == f64::INFINITY {
                    -1
                } else {
                    n as i64
                }
            }
        };
        // notify is a no-op (returns 0) on a non-shared buffer.
        match i.shared_buffers.get(&info.buffer) {
            Some(&id) => {
                let byte_index = info.offset + idx * info.kind.elsize();
                Ok(Value::Num(
                    crate::interpreter::futex_notify(id, byte_index, count) as f64,
                ))
            }
            None => Ok(Value::Num(0.0)),
        }
    });
    it.def_method(&atomics, "waitAsync", 4, |i, _t, a| {
        let (info, idx) = target(i, a)?;
        if !matches!(info.kind, TaKind::I32 | TaKind::I64) {
            return Err(i.make_error(
                "TypeError",
                "Atomics.waitAsync requires an Int32 or BigInt64 array",
            ));
        }
        let id = match i.shared_buffers.get(&info.buffer) {
            Some(&id) => id,
            None => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.waitAsync requires a SharedArrayBuffer-backed array",
                ))
            }
        };
        let expected = operand(i, &info, &arg(a, 2))?;
        // timeout: ToNumber (NaN → +Infinity); None means "no timeout".
        let q = ab(i.to_number(&arg(a, 3)))?;
        let timeout = if q.is_nan() || q == f64::INFINITY {
            None
        } else {
            Some(std::time::Duration::from_millis(q.max(0.0) as u64))
        };
        let result = i.new_object();
        // The value already differs ⇒ resolved synchronously (not async).
        if read_i128(i, &info, idx) != expected {
            set_data(&result, "async", Value::Bool(false));
            set_data(&result, "value", Value::str("not-equal"));
            return Ok(Value::Obj(result));
        }
        // A zero timeout times out immediately (still synchronous).
        if matches!(timeout, Some(d) if d.is_zero()) {
            set_data(&result, "async", Value::Bool(false));
            set_data(&result, "value", Value::str("timed-out"));
            return Ok(Value::Obj(result));
        }
        // Otherwise wait asynchronously: a waiter thread reports the outcome, which the event loop
        // uses to resolve the returned promise.
        let byte_index = info.offset + idx * info.kind.elsize();
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        std::thread::spawn(move || {
            let woken = crate::interpreter::futex_wait(id, byte_index, timeout);
            let _ = tx.send(if woken { "ok" } else { "timed-out" });
        });
        let promise = i.new_promise();
        i.pending_async_waits.push((promise.clone(), rx));
        set_data(&result, "async", Value::Bool(true));
        set_data(&result, "value", promise);
        Ok(Value::Obj(result))
    });
    it.def_method(&atomics, "pause", 0, |i, _t, a| {
        // The optional iterationNumber, if present, must be an integral Number.
        match arg(a, 0) {
            Value::Undefined => {}
            Value::Num(n) if n.fract() == 0.0 && n.is_finite() => {}
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.pause iterationNumber must be an integer",
                ))
            }
        }
        Ok(Value::Undefined)
    });
    set_to_string_tag(it, &atomics, "Atomics");
    set_builtin(&it.global, "Atomics", Value::Obj(atomics));
}

/// The test262 `$262` host object. Only the portions lumen can support are provided (`global`,
/// `gc`, `evalScript`, best-effort `detachArrayBuffer`); `agent`/`createRealm` are omitted.
fn install_host(it: &mut Interp) {
    let host = make_262(it, None);
    set_builtin(&it.global, "$262", host);
}

/// Build a `$262` host object. `realm_global` is the global of the realm this `$262` controls
/// (None for the main realm, in which case the live `it.global` is used and `evalScript` runs in the
/// current realm).
fn make_262(it: &mut Interp, realm_global: Option<Value>) -> Value {
    let host = Object::new(Some(it.object_proto.clone()));
    let global = realm_global
        .clone()
        .unwrap_or_else(|| Value::Obj(it.global.clone()));
    set_builtin(&host, "global", global.clone());
    it.def_method(&host, "gc", 0, |i, _t, _a| {
        i.gc_collect();
        Ok(Value::Undefined)
    });
    it.def_method(&host, "createRealm", 0, |i, _t, _a| {
        let g = i.create_realm();
        Ok(make_262(i, Some(g)))
    });
    // evalScript runs in this $262's realm (the main realm's $262 keeps using the current global).
    let rg = realm_global.clone();
    if let Some(rg) = rg {
        set_internal(&host, "__realm_global", rg);
    }
    it.def_method(&host, "evalScript", 1, |i, this, args| {
        let code = match arg(args, 0) {
            Value::Str(s) => s,
            other => return Ok(other),
        };
        let rg = ab(i.get_member(&this, "__realm_global"))?;
        if let Value::Obj(_) = &rg {
            return ab(i.eval_in_realm(&rg, &code));
        }
        let body = crate::parser::parse_script(&code, false)
            .map_err(|e| i.make_error("SyntaxError", e.message))?;
        let env = i.global_env.clone();
        ab(i.eval_in_scope(&body, &env))
    });
    it.def_method(&host, "detachArrayBuffer", 1, |i, _t, args| {
        if let Value::Obj(o) = arg(args, 0) {
            let p = Rc::as_ptr(&o) as usize;
            // Truly detach: drop the backing store (so views see it as detached) and zero the views.
            i.array_buffers.remove(&p);
            let views: Vec<usize> = i
                .typed_arrays
                .iter()
                .filter(|(_, info)| info.buffer == p)
                .map(|(k, _)| *k)
                .collect();
            for vp in views {
                if let Some(info) = i.typed_arrays.get_mut(&vp) {
                    info.len = 0;
                }
            }
        }
        Ok(Value::Undefined)
    });
    install_agent(it, &host);
    Value::Obj(host)
}

/// Reconstruct a SharedArrayBuffer object in this agent that aliases the global shared block `id`.
fn agent_make_shared(i: &mut Interp, id: u64, len: usize) -> Value {
    let obj = Object::new(i.extra_protos.get("SharedArrayBuffer").cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.array_buffers.insert(p, vec![0u8; len]); // length placeholder; bytes live in the registry
    set_internal(&obj, "__abMaxByteLength", Value::Num(len as f64));
    set_internal(&obj, "__abResizable", Value::Bool(false));
    set_internal(&obj, "__sab_id", Value::Num(id as f64));
    i.shared_buffers.insert(p, id);
    Value::Obj(obj)
}

/// `$262.agent`: the multi-agent harness (real OS threads sharing SharedArrayBuffer memory).
fn install_agent(it: &mut Interp, host: &Gc) {
    let agent = Object::new(Some(it.object_proto.clone()));
    it.def_method(&agent, "start", 1, |i, _t, a| {
        let src = ab(i.to_string(&arg(a, 0)))?.to_string();
        // Lazily create the main agent's report channel.
        if i.agent.is_none() {
            let (report_tx, report_rx) = std::sync::mpsc::channel();
            i.agent = Some(Box::new(crate::interpreter::AgentChannels {
                agent_broadcast_txs: Vec::new(),
                report_rx: Some(report_rx),
                report_tx,
                broadcast_rx: None,
            }));
        }
        let (bcast_tx, bcast_rx) = std::sync::mpsc::channel();
        let report_tx = i.agent.as_ref().unwrap().report_tx.clone();
        i.agent.as_mut().unwrap().agent_broadcast_txs.push(bcast_tx);
        std::thread::spawn(move || {
            let mut eng = crate::Engine::new();
            eng.run_as_agent(&src, bcast_rx, report_tx);
        });
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "broadcast", 2, |i, _t, a| {
        let sab = arg(a, 0);
        // Accept a SharedArrayBuffer directly, or a TypedArray view over one.
        let p = match sab.as_obj() {
            Some(o) => {
                let p = Rc::as_ptr(o) as usize;
                if i.shared_buffers.contains_key(&p) {
                    p
                } else if let Some(info) = i.typed_arrays.get(&p) {
                    info.buffer
                } else {
                    return Err(i.make_error("TypeError", "broadcast requires a SharedArrayBuffer"));
                }
            }
            None => return Err(i.make_error("TypeError", "broadcast requires a SharedArrayBuffer")),
        };
        let id = *i
            .shared_buffers
            .get(&p)
            .ok_or_else(|| i.make_error("TypeError", "broadcast requires a SharedArrayBuffer"))?;
        let len = i.array_buffers.get(&p).map(|b| b.len()).unwrap_or(0);
        if let Some(ag) = &i.agent {
            for tx in &ag.agent_broadcast_txs {
                let _ = tx.send((id, len));
            }
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "getReport", 0, |i, _t, _a| {
        // Block briefly for the next report (the producing agent typically reports very soon).
        if let Some(ag) = &i.agent {
            if let Some(rx) = &ag.report_rx {
                return Ok(match rx.recv_timeout(std::time::Duration::from_secs(4)) {
                    Ok(s) => Value::from_string(s),
                    Err(_) => Value::Null,
                });
            }
        }
        Ok(Value::Null)
    });
    it.def_method(&agent, "sleep", 1, |i, _t, a| {
        let ms = ab(i.to_number(&arg(a, 0)))?;
        if ms.is_finite() && ms > 0.0 {
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "monotonicNow", 0, |_i, _t, _a| {
        Ok(Value::Num(crate::interpreter::monotonic_now_ms()))
    });
    it.def_method(&agent, "receiveBroadcast", 1, |i, _t, a| {
        let cb = arg(a, 0);
        let received = {
            match i.agent.as_ref().and_then(|ag| ag.broadcast_rx.as_ref()) {
                Some(rx) => rx.recv().ok(),
                None => None,
            }
        };
        if let Some((id, len)) = received {
            let sab = agent_make_shared(i, id, len);
            ab(i.call(cb, Value::Undefined, &[sab]))?;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "report", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?.to_string();
        if let Some(ag) = &i.agent {
            let _ = ag.report_tx.send(s);
        }
        Ok(Value::Undefined)
    });
    it.def_method(&agent, "leaving", 0, |_i, _t, _a| Ok(Value::Undefined));
    set_data(host, "agent", Value::Obj(agent));
}

fn dv_info(i: &mut Interp, this: &Value) -> Result<(usize, usize, usize), Value> {
    let ptr =
        map_ptr(this).ok_or_else(|| i.make_error("TypeError", "receiver is not a DataView"))?;
    i.data_views
        .get(&ptr)
        .copied()
        .ok_or_else(|| i.make_error("TypeError", "receiver is not a DataView"))
}
fn dv_buffer_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    dv_info(i, &this)?;
    ab(i.get_member(&this, "__dv_buffer"))
}
fn dv_bytelength_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (buf, _off, len) = dv_info(i, &this)?;
    if !i.array_buffers.contains_key(&buf) {
        return Err(i.make_error("TypeError", "DataView's buffer is detached"));
    }
    Ok(Value::Num(len as f64))
}
fn dv_byteoffset_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (buf, off, _len) = dv_info(i, &this)?;
    if !i.array_buffers.contains_key(&buf) {
        return Err(i.make_error("TypeError", "DataView's buffer is detached"));
    }
    Ok(Value::Num(off as f64))
}

fn dv_get(i: &mut Interp, this: &Value, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let little = i.to_boolean(&arg(args, 1));
    let es = kind.elsize();
    if byte_off.checked_add(es).is_none_or(|e| e > len) {
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
    let (buf, off, len) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let value = ab(i.to_number(&arg(args, 1)))?;
    let little = i.to_boolean(&arg(args, 2));
    // Coercing the index/value can detach the buffer.
    if !i.array_buffers.contains_key(&buf) {
        return Err(i.make_error("TypeError", "DataView's buffer is detached"));
    }
    let es = kind.elsize();
    if byte_off.checked_add(es).is_none_or(|e| e > len) {
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

/// ToIndex: a non-negative integer in [0, 2^53-1]; anything else is a RangeError.
fn to_index(i: &mut Interp, v: &Value) -> Result<usize, Value> {
    let n = ab(i.to_number(v))?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    if !(0.0..=9007199254740991.0).contains(&n) {
        return Err(i.make_error("RangeError", "index out of range"));
    }
    Ok(n as usize)
}

fn dv_get_big(i: &mut Interp, this: &Value, args: &[Value], signed: bool) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let little = i.to_boolean(&arg(args, 1));
    if byte_off.checked_add(8).is_none_or(|e| e > len) {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let start = off + byte_off;
    let mut b = match i.array_buffers.get(&buf) {
        Some(buf) if start + 8 <= buf.len() => buf[start..start + 8].to_vec(),
        _ => return Err(i.make_error("TypeError", "detached buffer")),
    };
    if !little {
        b.reverse();
    }
    let raw = u64::from_le_bytes(b.try_into().unwrap());
    Ok(Value::BigInt(if signed {
        raw as i64 as i128
    } else {
        raw as i128
    }))
}

fn dv_set_big(i: &mut Interp, this: &Value, args: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let value = ab(i.to_bigint(&arg(args, 1)))?;
    let little = i.to_boolean(&arg(args, 2));
    if !i.array_buffers.contains_key(&buf) {
        return Err(i.make_error("TypeError", "DataView's buffer is detached"));
    }
    if byte_off.checked_add(8).is_none_or(|e| e > len) {
        return Err(i.make_error("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let mut bytes = (value as u64).to_le_bytes().to_vec();
    if !little {
        bytes.reverse();
    }
    let start = off + byte_off;
    if let Some(b) = i.array_buffers.get_mut(&buf) {
        if start + 8 <= b.len() {
            b[start..start + 8].copy_from_slice(&bytes);
        }
    }
    Ok(Value::Undefined)
}

fn install_dataview(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("DataView", proto.clone());
    // buffer / byteLength / byteOffset are brand-checked accessor getters; byteLength/byteOffset
    // additionally throw if the backing buffer has been detached.
    for (name, getter) in [
        (
            "buffer",
            dv_buffer_get as fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>,
        ),
        ("byteLength", dv_bytelength_get),
        ("byteOffset", dv_byteoffset_get),
    ] {
        let g = it.make_native(name, 0, getter);
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
    dvm!("getFloat16", "setFloat16", TaKind::F16);
    dvm!("getFloat32", "setFloat32", TaKind::F32);
    dvm!("getFloat64", "setFloat64", TaKind::F64);
    it.def_method(&proto, "getBigInt64", 1, |i, this, a| {
        dv_get_big(i, &this, a, true)
    });
    it.def_method(&proto, "getBigUint64", 1, |i, this, a| {
        dv_get_big(i, &this, a, false)
    });
    it.def_method(&proto, "setBigInt64", 2, |i, this, a| {
        dv_set_big(i, &this, a)
    });
    it.def_method(&proto, "setBigUint64", 2, |i, this, a| {
        dv_set_big(i, &this, a)
    });

    let ctor = it.make_native("DataView", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "DataView constructor requires 'new'"));
        }
        let bp = match arg(a, 0) {
            Value::Obj(o) if i.array_buffers.contains_key(&(Rc::as_ptr(&o) as usize)) => {
                Rc::as_ptr(&o) as usize
            }
            _ => return Err(i.make_error("TypeError", "DataView requires an ArrayBuffer")),
        };
        let buflen = i.array_buffers[&bp].len();
        let off_n = match arg(a, 1) {
            Value::Undefined => 0.0,
            v => ab(i.to_number(&v))?,
        };
        let off_n = if off_n.is_nan() { 0.0 } else { off_n.trunc() };
        if off_n < 0.0 || !off_n.is_finite() {
            return Err(i.make_error("RangeError", "invalid DataView byteOffset"));
        }
        let offset = off_n as usize;
        if offset > buflen {
            return Err(i.make_error("RangeError", "DataView byteOffset is out of bounds"));
        }
        let len = match arg(a, 2) {
            Value::Undefined => buflen - offset,
            v => {
                let n = ab(i.to_number(&v))?;
                let n = if n.is_nan() { 0.0 } else { n.trunc() };
                if n < 0.0 || !n.is_finite() {
                    return Err(i.make_error("RangeError", "invalid DataView byteLength"));
                }
                let l = n as usize;
                if offset + l > buflen {
                    return Err(i.make_error("RangeError", "DataView byteLength is out of bounds"));
                }
                l
            }
        };
        let obj = new_from_ctor(i, "DataView")?;
        let p = Rc::as_ptr(&obj) as usize;
        i.data_views.insert(p, (bp, offset, len));
        // buffer/byteOffset/byteLength are accessor getters on the prototype, not own properties;
        // only the buffer object itself is kept (hidden) for the `buffer` getter.
        set_internal(&obj, "__dv_buffer", arg(a, 0));
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "DataView", Value::Obj(ctor));
}

/// A RegExp prototype getter: a flag boolean (`Some(char)`), or the special source/flags string.
fn re_flag_get(i: &Interp, this: &Value, flag: Option<char>) -> Result<Value, Value> {
    if let Some(ptr) = map_ptr(this) {
        if let Some(re) = i.regexps.get(&ptr) {
            return Ok(match flag {
                Some(c) => Value::Bool(re.flags.contains(c)),
                None => Value::from_string(re.flags.clone()),
            });
        }
        // The %RegExp.prototype% object itself has default values rather than throwing.
        if i.extra_protos.get("RegExp").map(|p| Rc::as_ptr(p) as usize) == Some(ptr) {
            return Ok(match flag {
                Some(_) => Value::Undefined,
                None => Value::str(""),
            });
        }
    }
    Err(i.make_error(
        "TypeError",
        "RegExp.prototype getter called on a non-RegExp",
    ))
}
fn re_source_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    if let Some(ptr) = map_ptr(&this) {
        if let Some(re) = i.regexps.get(&ptr) {
            return Ok(Value::from_string(re.source.clone()));
        }
        if i.extra_protos.get("RegExp").map(|p| Rc::as_ptr(p) as usize) == Some(ptr) {
            return Ok(Value::str("(?:)"));
        }
    }
    Err(i.make_error(
        "TypeError",
        "RegExp.prototype.source called on a non-RegExp",
    ))
}

/// The second (flags) argument to RegExp / RegExp.prototype.compile: undefined → "", else ToString.
fn regexp_flags_arg(i: &mut Interp, a: &[Value]) -> Result<String, Value> {
    match arg(a, 1) {
        Value::Undefined => Ok(String::new()),
        v => Ok(ab(i.to_string(&v))?.to_string()),
    }
}

fn install_regexp(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("RegExp", proto.clone());
    // source/flags/global/... accessor getters (computed from the matcher).
    let add_getter = |it: &mut Interp, proto: &Gc, name: &str, f: NativeFn| {
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
    };
    add_getter(it, &proto, "source", re_source_get);
    // `get flags` is generic: it reads each component flag accessor via [[Get]] on the receiver.
    add_getter(it, &proto, "flags", |i, t, _| {
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error(
                "TypeError",
                "RegExp.prototype.flags getter called on non-object",
            ));
        }
        let mut out = String::new();
        for (prop, ch) in [
            ("hasIndices", 'd'),
            ("global", 'g'),
            ("ignoreCase", 'i'),
            ("multiline", 'm'),
            ("dotAll", 's'),
            ("unicode", 'u'),
            ("unicodeSets", 'v'),
            ("sticky", 'y'),
        ] {
            let v = ab(i.get_member(&t, prop))?;
            if i.to_boolean(&v) {
                out.push(ch);
            }
        }
        Ok(Value::from_string(out))
    });
    add_getter(it, &proto, "global", |i, t, _| {
        re_flag_get(i, &t, Some('g'))
    });
    add_getter(it, &proto, "ignoreCase", |i, t, _| {
        re_flag_get(i, &t, Some('i'))
    });
    add_getter(it, &proto, "multiline", |i, t, _| {
        re_flag_get(i, &t, Some('m'))
    });
    add_getter(it, &proto, "dotAll", |i, t, _| {
        re_flag_get(i, &t, Some('s'))
    });
    add_getter(it, &proto, "sticky", |i, t, _| {
        re_flag_get(i, &t, Some('y'))
    });
    add_getter(it, &proto, "unicode", |i, t, _| {
        re_flag_get(i, &t, Some('u'))
    });
    add_getter(it, &proto, "hasIndices", |i, t, _| {
        re_flag_get(i, &t, Some('d'))
    });
    add_getter(it, &proto, "unicodeSets", |i, t, _| {
        re_flag_get(i, &t, Some('v'))
    });
    // Annex B B.2.5 RegExp.prototype.compile(pattern, flags): recompile this regex in place.
    it.def_method(&proto, "compile", 2, |i, this, a| {
        let ptr = map_ptr(&this).filter(|p| i.regexps.contains_key(p));
        let ptr = ptr.ok_or_else(|| i.make_error("TypeError", "compile called on non-RegExp"))?;
        let (source, flags) = match arg(a, 0) {
            Value::Obj(o) if i.regexps.contains_key(&(Rc::as_ptr(&o) as usize)) => {
                // A RegExp pattern copies its source/flags; a second flags argument is then an error.
                if !matches!(arg(a, 1), Value::Undefined) {
                    return Err(
                        i.make_error("TypeError", "cannot supply flags when compiling a RegExp")
                    );
                }
                let re = i.regexps[&(Rc::as_ptr(&o) as usize)].clone();
                (re.source.clone(), re.flags.clone())
            }
            Value::Undefined => (String::new(), regexp_flags_arg(i, a)?),
            v => {
                let src = ab(i.to_string(&v))?.to_string();
                (src, regexp_flags_arg(i, a)?)
            }
        };
        let re = crate::regex::Regex::new(&source, &flags)
            .map_err(|e| i.make_error("SyntaxError", &e))?;
        i.regexps.insert(ptr, Rc::new(re));
        ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
        Ok(this)
    });
    it.def_method(&proto, "exec", 1, regexp_exec);
    it.def_method(&proto, "test", 1, |i, this, a| {
        Ok(Value::Bool(!matches!(
            regexp_exec(i, this, a)?,
            Value::Null
        )))
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
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    install_species(it, &ctor);

    // The @@match/@@replace/@@search/@@split/@@matchAll methods on RegExp.prototype (what
    // String.prototype.{match,replace,...} dispatch to).
    let methods: [(&str, NativeFn); 5] = [
        ("match", re_sym_match),
        ("replace", re_sym_replace),
        ("search", re_sym_search),
        ("split", re_sym_split),
        ("matchAll", re_sym_matchall),
    ];
    for (sym, f) in methods {
        if let Some(key) = well_known_key(it, sym) {
            let m = it.make_native(&format!("[Symbol.{sym}]"), 1, f);
            proto
                .borrow_mut()
                .props
                .insert(key, Property::builtin(Value::Obj(m)));
        }
    }
    it.def_method(&ctor, "escape", 1, |i, _t, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "RegExp.escape requires a string")),
        };
        let mut out = String::new();
        for (idx, c) in s.chars().enumerate() {
            out.push_str(&regexp_escape_char(c, idx == 0));
        }
        Ok(Value::from_string(out))
    });
    set_builtin(&it.global, "RegExp", Value::Obj(ctor));
}

/// EncodeForRegExpEscape: escape one code point for `RegExp.escape`.
fn regexp_escape_char(c: char, first: bool) -> String {
    let cp = c as u32;
    // The first character, if alphanumeric, is hex-escaped so the result can't start an identifier.
    if first && c.is_ascii_alphanumeric() {
        return format!("\\x{cp:02x}");
    }
    if "^$\\.*+?()[]{}|/".contains(c) {
        return format!("\\{c}");
    }
    match c {
        '\t' => "\\t".into(),
        '\n' => "\\n".into(),
        '\u{0b}' => "\\v".into(),
        '\u{0c}' => "\\f".into(),
        '\r' => "\\r".into(),
        _ if ",-=<>#&!%:;@~'`\"".contains(c) || c.is_whitespace() || c.is_control() => {
            if cp <= 0xff {
                format!("\\x{cp:02x}")
            } else if cp <= 0xffff {
                format!("\\u{cp:04x}")
            } else {
                format!("\\u{{{cp:x}}}")
            }
        }
        _ => c.to_string(),
    }
}

fn re_sym_match(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).filter(|p| i.regexps.contains_key(p));
    if ptr.is_none() {
        return Err(i.make_error("TypeError", "[Symbol.match] called on non-RegExp"));
    }
    let s = ab(i.to_string(&arg(a, 0)))?;
    let re = i.regexps[&ptr.unwrap()].clone();
    let chars: Vec<char> = s.chars().collect();
    if re.global {
        ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
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
        regexp_exec(i, this, &[Value::Str(s)])
    }
}
fn re_sym_replace(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    if map_ptr(&this).map(|p| i.regexps.contains_key(&p)) != Some(true) {
        return Err(i.make_error("TypeError", "[Symbol.replace] called on non-RegExp"));
    }
    let s = ab(i.to_string(&arg(a, 0)))?.to_string();
    regex_replace(i, &s, &this, &arg(a, 1))
}
fn re_sym_search(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).filter(|p| i.regexps.contains_key(p));
    if ptr.is_none() {
        return Err(i.make_error("TypeError", "[Symbol.search] called on non-RegExp"));
    }
    let s = ab(i.to_string(&arg(a, 0)))?;
    let re = i.regexps[&ptr.unwrap()].clone();
    let chars: Vec<char> = s.chars().collect();
    Ok(match re.exec_at(&chars, 0) {
        Some(c) => Value::Num(c[0].unwrap().0 as f64),
        None => Value::Num(-1.0),
    })
}
fn re_sym_matchall(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).filter(|p| i.regexps.contains_key(p));
    if ptr.is_none() {
        return Err(i.make_error("TypeError", "[Symbol.matchAll] called on non-RegExp"));
    }
    let s = ab(i.to_string(&arg(a, 0)))?;
    let re = i.regexps[&ptr.unwrap()].clone();
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
}
fn re_sym_split(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).filter(|p| i.regexps.contains_key(p));
    if ptr.is_none() {
        return Err(i.make_error("TypeError", "[Symbol.split] called on non-RegExp"));
    }
    let s = ab(i.to_string(&arg(a, 0)))?;
    let re = i.regexps[&ptr.unwrap()].clone();
    let chars: Vec<char> = s.chars().collect();
    let limit = match arg(a, 1) {
        Value::Undefined => usize::MAX,
        v => ab(i.to_number(&v))? as usize,
    };
    let mut out = Vec::new();
    let mut last = 0;
    let mut pos = 0;
    while pos <= chars.len() && out.len() < limit {
        match re.exec_at(&chars, pos) {
            Some(caps) => {
                let (mstart, mend) = caps[0].unwrap();
                if mend == last && mstart == last {
                    pos += 1;
                    continue;
                }
                if mstart >= chars.len() {
                    break;
                }
                out.push(Value::from_string(
                    chars[last..mstart].iter().collect::<String>(),
                ));
                for g in 1..=re.ngroups {
                    out.push(match caps[g] {
                        Some((x, y)) => Value::from_string(chars[x..y].iter().collect::<String>()),
                        None => Value::Undefined,
                    });
                }
                last = mend;
                pos = if mend > mstart { mend } else { mend + 1 };
            }
            None => break,
        }
    }
    if out.len() < limit {
        out.push(Value::from_string(chars[last..].iter().collect::<String>()));
    }
    Ok(i.make_array(out))
}

/// `RegExp.prototype.exec`: returns the match array (with `index`/`input`) or `null`, advancing
/// `lastIndex` for global/sticky regexes.
fn regexp_exec(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "exec on non-RegExp"))?;
    let re = i
        .regexps
        .get(&ptr)
        .cloned()
        .ok_or_else(|| i.make_error("TypeError", "exec on non-RegExp"))?;
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
            let mut items = vec![Value::from_string(
                chars[start..end].iter().collect::<String>(),
            )];
            for g in 1..=re.ngroups {
                items.push(match caps[g] {
                    Some((a, b)) => Value::from_string(chars[a..b].iter().collect::<String>()),
                    None => Value::Undefined,
                });
            }
            let result = i.make_array(items);
            // `groups`: a null-prototype object of named captures, or undefined if there are none.
            let groups = if re.names.is_empty() {
                Value::Undefined
            } else {
                let g = i.new_object();
                g.borrow_mut().proto = None;
                // One property per name, in first-occurrence order; for duplicate names the value
                // comes from whichever same-named group actually matched (else undefined).
                let mut seen: Vec<&str> = Vec::new();
                for (name, _) in &re.names {
                    if seen.contains(&name.as_str()) {
                        continue;
                    }
                    seen.push(name.as_str());
                    let v = re
                        .names
                        .iter()
                        .filter(|(n, _)| n == name)
                        .find_map(|(_, idx)| caps.get(*idx).copied().flatten())
                        .map(|(a, b)| Value::from_string(chars[a..b].iter().collect::<String>()))
                        .unwrap_or(Value::Undefined);
                    set_data(&g, name, v);
                }
                Value::Obj(g)
            };
            if let Value::Obj(o) = &result {
                set_data(o, "index", Value::Num(start as f64));
                set_data(o, "input", Value::Str(input));
                set_data(o, "groups", groups);
            }
            Ok(result)
        }
    }
}

/// Coerce `v` to a RegExp object (returning it unchanged if already one).
/// `$&` / `$$` / `$1`..`$99` / `` $` `` / `$'` substitution in a string replacement template.
fn expand_dollar(
    template: &str,
    caps: &[Option<(usize, usize)>],
    chars: &[char],
    names: &[(String, usize)],
) -> String {
    let (ms, me) = caps[0].unwrap();
    let group = |g: usize| -> String {
        caps.get(g)
            .and_then(|c| *c)
            .map(|(a, b)| chars[a..b].iter().collect())
            .unwrap_or_default()
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
                // `$<name>` — a named-group reference.
                '<' if !names.is_empty() => {
                    let mut j = i + 2;
                    let mut name = String::new();
                    while j < t.len() && t[j] != '>' {
                        name.push(t[j]);
                        j += 1;
                    }
                    if j < t.len() {
                        if let Some((_, idx)) = names.iter().find(|(n, _)| *n == name) {
                            out.push_str(&group(*idx));
                        }
                        i = j + 1;
                    } else {
                        out.push('$');
                        i += 1;
                    }
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
            out.push_str(&expand_dollar(&template, caps, &chars, &re.names));
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
    // byteLength/maxByteLength/growable accessor getters on the prototype.
    let sab_getter = |it: &mut Interp, proto: &Gc, name: &str, f: NativeFn| {
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
    };
    sab_getter(it, &proto, "byteLength", |i, this, _| {
        let p = this
            .as_obj()
            .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
            .map(|o| Rc::as_ptr(o) as usize)
            .ok_or_else(|| i.make_error("TypeError", "not a SharedArrayBuffer"))?;
        Ok(Value::Num(
            i.array_buffers.get(&p).map(|b| b.len()).unwrap_or(0) as f64,
        ))
    });
    sab_getter(it, &proto, "maxByteLength", |i, this, _| {
        this.as_obj()
            .and_then(|o| {
                o.borrow()
                    .props
                    .get("__abMaxByteLength")
                    .map(|p| p.value.clone())
            })
            .ok_or_else(|| i.make_error("TypeError", "not a SharedArrayBuffer"))
    });
    sab_getter(it, &proto, "growable", |i, this, _| {
        this.as_obj()
            .and_then(|o| {
                o.borrow()
                    .props
                    .get("__abResizable")
                    .map(|p| p.value.clone())
            })
            .ok_or_else(|| i.make_error("TypeError", "not a SharedArrayBuffer"))
    });
    // grow(newLength): only allowed for a growable buffer and only to a larger size.
    it.def_method(&proto, "grow", 1, |i, this, a| {
        let o = this
            .as_obj()
            .cloned()
            .ok_or_else(|| i.make_error("TypeError", "not a SharedArrayBuffer"))?;
        let growable = ab(i.get_member(&this, "growable"))?;
        if !i.to_boolean(&growable) {
            return Err(i.make_error("TypeError", "SharedArrayBuffer is not growable"));
        }
        let mv = ab(i.get_member(&this, "maxByteLength"))?;
        let max = ab(i.to_number(&mv))? as usize;
        let new_len = ab(i.to_number(&arg(a, 0)))?;
        let ptr = Rc::as_ptr(&o) as usize;
        let cur = i.array_buffers.get(&ptr).map(|b| b.len()).unwrap_or(0);
        if !new_len.is_finite() || new_len < cur as f64 || new_len as usize > max {
            return Err(i.make_error("RangeError", "SharedArrayBuffer grow out of range"));
        }
        if let Some(buf) = i.array_buffers.get_mut(&ptr) {
            buf.resize(new_len as usize, 0);
        }
        Ok(Value::Undefined)
    });
    if let Some(key) = well_known_key(it, "toStringTag") {
        proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("SharedArrayBuffer"), false, false, true),
        );
    }
    it.def_method(&proto, "slice", 2, |i, this, a| {
        let ptr = this.as_obj().map(|o| Rc::as_ptr(o) as usize);
        let bytes = ptr
            .and_then(|p| i.array_buffers.get(&p))
            .cloned()
            .unwrap_or_default();
        let len = bytes.len() as i64;
        let begin = norm_index(ab(i.to_number(&arg(a, 0)))?, len, 0);
        let end = match arg(a, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let slice = if begin < end {
            bytes[begin as usize..end as usize].to_vec()
        } else {
            Vec::new()
        };
        let (bv, bp) = make_array_buffer(i, slice.len());
        if let Some(buf) = i.array_buffers.get_mut(&bp) {
            buf.copy_from_slice(&slice);
        }
        Ok(bv)
    });
    let ctor = it.make_native("SharedArrayBuffer", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "SharedArrayBuffer constructor requires 'new'"));
        }
        let nraw = ab(i.to_number(&arg(a, 0)))?;
        let n = if nraw.is_nan() { 0.0 } else { nraw.trunc() };
        if n < 0.0 || !n.is_finite() || n as usize > MAX_ARRAY_OP_LEN {
            return Err(i.make_error("RangeError", "Invalid SharedArrayBuffer length"));
        }
        let len = n as usize;
        // Stamp the SharedArrayBuffer prototype rather than ArrayBuffer's, and back it with a
        // process-global shared-memory block (so agent threads see the same bytes).
        let (bv, bp) = make_array_buffer(i, len);
        if let Value::Obj(o) = &bv {
            o.borrow_mut().proto = i.extra_protos.get("SharedArrayBuffer").cloned();
        }
        let id = crate::interpreter::alloc_shared_mem(len);
        i.shared_buffers.insert(bp, id);
        set_internal(
            &bv.as_obj().cloned().unwrap(),
            "__sab_id",
            Value::Num(id as f64),
        );
        // The options bag's maxByteLength makes the buffer growable.
        if let Value::Obj(_) = arg(a, 1) {
            let mbl = ab(i.get_member(&arg(a, 1), "maxByteLength"))?;
            if !matches!(mbl, Value::Undefined) {
                let m = ab(i.to_number(&mbl))?;
                if !m.is_finite() || (m as usize) < len || m as usize > MAX_ARRAY_OP_LEN {
                    return Err(i.make_error("RangeError", "Invalid maxByteLength"));
                }
                if let Value::Obj(o) = &bv {
                    set_internal(o, "__abMaxByteLength", Value::Num(m));
                    set_internal(o, "__abResizable", Value::Bool(true));
                }
            }
        }
        Ok(bv)
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
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

/// ArrayBuffer.prototype.transfer / transferToFixedLength: move the bytes into a fresh fixed-length
/// buffer and detach the source (a TypeError if it's already detached).
fn ab_transfer(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    let o = this
        .as_obj()
        .cloned()
        .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
    let ptr = Rc::as_ptr(&o) as usize;
    if !i.array_buffers.contains_key(&ptr) {
        return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
    }
    let new_len = match arg(a, 0) {
        Value::Undefined => i.array_buffers[&ptr].len(),
        v => {
            let n = ab(i.to_number(&v))?;
            if n.is_nan() || n < 0.0 || !n.is_finite() || n as usize > MAX_ARRAY_OP_LEN {
                return Err(i.make_error("RangeError", "invalid transfer length"));
            }
            n as usize
        }
    };
    let bytes = i.array_buffers[&ptr].clone();
    let (bv, bp) = make_array_buffer(i, new_len);
    if let Some(buf) = i.array_buffers.get_mut(&bp) {
        let n = bytes.len().min(new_len);
        buf[..n].copy_from_slice(&bytes[..n]);
    }
    // Detach the source (drop its backing store; detached/byteLength derive from the side table).
    i.array_buffers.remove(&ptr);
    let _ = &o;
    Ok(bv)
}

fn make_array_buffer(i: &mut Interp, byte_len: usize) -> (Value, usize) {
    let obj = Object::new(i.extra_protos.get("ArrayBuffer").cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.array_buffers.insert(p, vec![0u8; byte_len]);
    // byteLength/detached derive from the side table; only max/resizable need stored slots, hidden
    // behind the `__ab*` prefix and surfaced through prototype accessor getters.
    set_internal(&obj, "__abMaxByteLength", Value::Num(byte_len as f64));
    set_internal(&obj, "__abResizable", Value::Bool(false));
    (Value::Obj(obj), p)
}

fn install_array_buffer(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("ArrayBuffer", proto.clone());
    // byteLength/maxByteLength/resizable/detached are accessor getters on the prototype.
    let ab_getter = |it: &mut Interp, proto: &Gc, name: &str, f: NativeFn| {
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
    };
    ab_getter(it, &proto, "byteLength", |i, this, _| {
        let p = this
            .as_obj()
            .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
            .map(|o| Rc::as_ptr(o) as usize)
            .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
        // Detached (absent from the side table) → 0.
        Ok(Value::Num(
            i.array_buffers.get(&p).map(|b| b.len()).unwrap_or(0) as f64,
        ))
    });
    ab_getter(it, &proto, "maxByteLength", |i, this, _| {
        match this.as_obj().and_then(|o| {
            o.borrow()
                .props
                .get("__abMaxByteLength")
                .map(|pr| pr.value.clone())
        }) {
            Some(v) => {
                // A detached buffer reports 0.
                let detached = this
                    .as_obj()
                    .map(|o| !i.array_buffers.contains_key(&(Rc::as_ptr(o) as usize)))
                    .unwrap_or(true);
                Ok(if detached { Value::Num(0.0) } else { v })
            }
            None => Err(i.make_error("TypeError", "not an ArrayBuffer")),
        }
    });
    ab_getter(it, &proto, "resizable", |i, this, _| {
        match this.as_obj().and_then(|o| {
            o.borrow()
                .props
                .get("__abResizable")
                .map(|pr| pr.value.clone())
        }) {
            Some(v) => Ok(v),
            None => Err(i.make_error("TypeError", "not an ArrayBuffer")),
        }
    });
    ab_getter(it, &proto, "detached", |i, this, _| {
        let o = this
            .as_obj()
            .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
            .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
        Ok(Value::Bool(
            !i.array_buffers.contains_key(&(Rc::as_ptr(o) as usize)),
        ))
    });
    it.def_method(&proto, "slice", 2, |i, this, a| {
        let ptr = this.as_obj().map(|o| Rc::as_ptr(o) as usize);
        let bytes = ptr
            .and_then(|p| i.array_buffers.get(&p))
            .cloned()
            .unwrap_or_default();
        let len = bytes.len() as i64;
        let begin = norm_index(ab(i.to_number(&arg(a, 0)))?, len, 0);
        let end = match arg(a, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let slice = if begin < end {
            bytes[begin as usize..end as usize].to_vec()
        } else {
            Vec::new()
        };
        let (bv, bp) = make_array_buffer(i, slice.len());
        if let Some(buf) = i.array_buffers.get_mut(&bp) {
            buf.copy_from_slice(&slice);
        }
        Ok(bv)
    });
    it.def_method(&proto, "resize", 1, |i, this, a| {
        let o = this
            .as_obj()
            .cloned()
            .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
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
        // byteLength derives from the backing store length, which the resize above updated.
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "transfer", 1, ab_transfer);
    it.def_method(&proto, "transferToFixedLength", 1, ab_transfer);
    let ctor = it.make_native("ArrayBuffer", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "ArrayBuffer constructor requires 'new'"));
        }
        // ToIndex: NaN → 0, truncate, and a negative or too-large length is a RangeError.
        let n = ab(i.to_number(&arg(a, 0)))?;
        let n = if n.is_nan() { 0.0 } else { n.trunc() };
        if n < 0.0 || !n.is_finite() || n as usize > MAX_ARRAY_OP_LEN {
            return Err(i.make_error("RangeError", "Invalid ArrayBuffer length"));
        }
        let len = n as usize;
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
                    set_internal(o, "__abMaxByteLength", Value::Num(m));
                    set_internal(o, "__abResizable", Value::Bool(true));
                }
            }
        }
        Ok(bv)
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
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
    let info = map_ptr(this).and_then(|p| i.typed_arrays.get(&p).copied());
    let info = match info {
        Some(info) => info,
        None => return Err(i.make_error("TypeError", "method called on a non-TypedArray receiver")),
    };
    // ValidateTypedArray: a detached (or out-of-bounds, after a resizable buffer shrank) backing
    // buffer makes the operation throw.
    if i.ta_len(&info).is_none() {
        return Err(i.make_error(
            "TypeError",
            "Cannot perform operation on a detached or out-of-bounds TypedArray",
        ));
    }
    // %TypedArray%.prototype.with has TypedArray-specific semantics (value coerced to the element
    // type *before* the index bounds check) that the generic Array delegation can't reproduce.
    if method == "with" {
        let len = i.ta_len(&info).unwrap_or(0);
        let rel = ab(i.to_number(&arg(args, 0)))?;
        let rel = if rel.is_nan() { 0.0 } else { rel.trunc() };
        let actual = if rel >= 0.0 { rel } else { len as f64 + rel };
        // Coerce the value to the element type (this can run user code).
        let coerced = if info.kind.is_bigint() {
            Value::BigInt(ab(i.to_bigint(&arg(args, 1)))?)
        } else {
            Value::Num(ab(i.to_number(&arg(args, 1)))?)
        };
        if actual < 0.0 || actual >= len as f64 {
            return Err(i.make_error("RangeError", "invalid TypedArray index"));
        }
        let actual = actual as usize;
        let new_ta = ta_species_create(i, this, info.kind, &[Value::Num(len as f64)])?;
        let new_info = map_ptr(&new_ta)
            .and_then(|p| i.typed_arrays.get(&p).copied())
            .ok_or_else(|| {
                i.make_error("TypeError", "with: species did not return a TypedArray")
            })?;
        for k in 0..len {
            let val = if k == actual {
                coerced.clone()
            } else {
                i.ta_read(&info, k)
            };
            ab(i.ta_store(&new_info, k, &val))?;
        }
        return Ok(new_ta);
    }
    let f = it_array_method(i, method);
    let result = ab(i.call(f, this.clone(), args))?;
    // Methods that produce a new collection return a TypedArray built via TypedArraySpeciesCreate
    // (the receiver's `constructor[@@species]`, falling back to the kind's intrinsic constructor).
    if matches!(
        method,
        "map" | "filter" | "slice" | "toReversed" | "toSorted" | "with"
    ) {
        let len = ab(i.get_member(&result, "length"))?;
        let len = ab(i.to_number(&len))? as usize;
        let new_ta = ta_species_create(i, this, info.kind, &[Value::Num(len as f64)])?;
        // SpeciesConstructor access can run user code that detaches the source — re-validate.
        if !i.array_buffers.contains_key(&info.buffer) {
            return Err(i.make_error(
                "TypeError",
                "Cannot perform operation on a detached ArrayBuffer",
            ));
        }
        for k in 0..len {
            let v = ab(i.get_member(&result, &k.to_string()))?;
            ab(i.set_member(&new_ta, &k.to_string(), v))?;
        }
        return Ok(new_ta);
    }
    Ok(result)
}

/// TypedArraySpeciesCreate(O, args): construct a new TypedArray using `O.constructor[@@species]`,
/// falling back to the receiver kind's intrinsic constructor. Validates the result is a TypedArray.
fn ta_species_create(
    i: &mut Interp,
    this: &Value,
    kind: TaKind,
    args: &[Value],
) -> Result<Value, Value> {
    let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), kind.name()))?;
    let ctor = ab(i.get_member(this, "constructor"))?;
    let chosen = if matches!(ctor, Value::Undefined) {
        default_ctor
    } else if matches!(ctor, Value::Obj(_)) {
        let species_key = well_known_key(i, "species").unwrap_or_default();
        let species = ab(i.get_member(&ctor, &species_key))?;
        if matches!(species, Value::Undefined | Value::Null) {
            default_ctor
        } else {
            species
        }
    } else {
        return Err(i.make_error(
            "TypeError",
            "TypedArray constructor property is not an object",
        ));
    };
    if !chosen.is_callable() {
        return Err(i.make_error("TypeError", "TypedArray species is not a constructor"));
    }
    let result = ab(i.construct(chosen, args))?;
    if map_ptr(&result)
        .and_then(|p| i.typed_arrays.get(&p))
        .is_none()
    {
        return Err(i.make_error(
            "TypeError",
            "TypedArray species constructor did not return a TypedArray",
        ));
    }
    Ok(result)
}
fn it_array_method(i: &Interp, method: &str) -> Value {
    i.array_proto
        .borrow()
        .props
        .get(method)
        .map(|p| p.value.clone())
        .unwrap_or(Value::Undefined)
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

/// OrdinaryCreateFromConstructor: a new object whose [[Prototype]] is `new.target.prototype` when
/// that's an object (subclassing / Reflect.construct), else the named intrinsic prototype. Safe to
/// call from any native constructor: outside a `new`, `new_target` is Undefined so the default wins.
/// CanBeHeldWeakly: an object, or a non-registered (collectable) symbol.
fn can_be_held_weakly(i: &Interp, v: &Value) -> bool {
    match v {
        Value::Obj(_) => true,
        Value::Sym(s) => !i.sym_for.values().any(|r| Rc::ptr_eq(r, s)),
        _ => false,
    }
}

/// CreateListFromArrayLike: the object must be array-like (TypeError otherwise); reads `length`
/// (ToLength) then the indexed elements in order. Used by Reflect.apply/construct.
fn create_list_from_array_like(i: &mut Interp, v: &Value) -> Result<Vec<Value>, Value> {
    let o = match v {
        Value::Obj(o) => o.clone(),
        _ => {
            return Err(i.make_error(
                "TypeError",
                "CreateListFromArrayLike called on a non-object",
            ))
        }
    };
    let len = ab(i.to_length(&o))?;
    let mut out = Vec::with_capacity(len.min(1024));
    for k in 0..len {
        out.push(ab(i.get_member(v, &k.to_string()))?);
    }
    Ok(out)
}

/// ObjectDefineProperties(O, Properties): ToObject(Properties), then for each *enumerable* own key
/// (string or symbol, proxy-aware) collect its descriptor object, then DefinePropertyOrThrow each on
/// O (which may itself be a proxy).
fn object_define_properties(
    i: &mut Interp,
    o: &Value,
    props_arg: Value,
    who: &str,
) -> Result<(), Value> {
    let props = Value::Obj(to_object_arg(i, props_arg, who)?);
    let keys: Vec<String> = if let Some((t, h)) = proxy_pair(i, &props) {
        let mut ks = Vec::new();
        for k in proxy_own_keys(i, &t, &h)? {
            ks.push(ab(i.to_property_key(&k))?);
        }
        ks
    } else {
        props
            .as_obj()
            .unwrap()
            .borrow()
            .props
            .ordered_keys()
            .iter()
            .filter(|k| !k.starts_with('#'))
            .map(|k| k.to_string())
            .collect()
    };
    // Collect descriptor objects for enumerable own keys first (so all Gets precede any Defines).
    let mut descs: Vec<(String, Value)> = Vec::new();
    for key in keys {
        let enumerable = if let Some((t, h)) = proxy_pair(i, &props) {
            let d = proxy_gopd_value(i, &t, &h, &key)?;
            match &d {
                Value::Obj(_) => {
                    let e = ab(i.get_member(&d, "enumerable"))?;
                    i.to_boolean(&e)
                }
                _ => false,
            }
        } else {
            props
                .as_obj()
                .unwrap()
                .borrow()
                .props
                .get(&key)
                .map(|p| p.enumerable)
                .unwrap_or(false)
        };
        if enumerable {
            let desc_obj = ab(i.get_member(&props, &key))?;
            descs.push((key, desc_obj));
        }
    }
    for (key, desc_obj) in descs {
        let ok = if let Some((t, h)) = proxy_pair(i, o) {
            ab(proxy_define_property(i, &t, &h, &key, &desc_obj))?
        } else {
            ab(define_own_property(i, o.as_obj().unwrap(), &key, &desc_obj))?
        };
        if !ok {
            return Err(i.make_error("TypeError", "cannot define property"));
        }
    }
    Ok(())
}

/// SetIntegrityLevel(obj, frozen?): PreventExtensions then make every own property non-configurable
/// (and, when freezing, non-writable for data properties), all through trap-aware operations.
fn set_integrity_level(i: &mut Interp, obj: &Value, freeze: bool) -> Result<bool, Value> {
    if !matches!(obj, Value::Obj(_)) {
        return Ok(true);
    }
    if !js_prevent_extensions(i, obj)? {
        return Ok(false);
    }
    let proxy = proxy_pair(i, obj);
    let keys: Vec<String> = if let Some((t, h)) = &proxy {
        let mut ks = Vec::new();
        for k in proxy_own_keys(i, t, h)? {
            ks.push(ab(i.to_property_key(&k))?);
        }
        ks
    } else {
        obj.as_obj()
            .unwrap()
            .borrow()
            .props
            .ordered_keys()
            .iter()
            .filter(|k| !k.starts_with('#'))
            .map(|k| k.to_string())
            .collect()
    };
    for key in keys {
        let is_accessor = if let Some((t, h)) = &proxy {
            let d = proxy_gopd_value(i, t, h, &key)?;
            matches!(&d, Value::Obj(o) if o.borrow().props.contains("get") || o.borrow().props.contains("set"))
        } else {
            obj.as_obj()
                .unwrap()
                .borrow()
                .props
                .get(&key)
                .map(|p| p.accessor)
                .unwrap_or(false)
        };
        let desc = i.new_object();
        set_data(&desc, "configurable", Value::Bool(false));
        if freeze && !is_accessor {
            set_data(&desc, "writable", Value::Bool(false));
        }
        let ok = if let Some((t, h)) = &proxy {
            ab(proxy_define_property(i, t, h, &key, &Value::Obj(desc)))?
        } else {
            ab(define_own_property(
                i,
                obj.as_obj().unwrap(),
                &key,
                &Value::Obj(desc),
            ))?
        };
        if !ok {
            return Err(i.make_error("TypeError", "could not set the object's integrity level"));
        }
    }
    Ok(true)
}

/// GetFunctionRealm-style lookup: when `new.target`'s `prototype` isn't an object, the instance's
/// [[Prototype]] is `new.target`'s realm's intrinsic. The realm is identified by which registered
/// realm's `Function.prototype` lies on `new.target`'s own prototype chain.
fn ctor_realm_proto(i: &Interp, nt: &Value, default_proto: &str) -> Option<Gc> {
    let ntobj = nt.as_obj()?;
    let mut chain: Vec<Gc> = Vec::new();
    let mut cur = ntobj.borrow().proto.clone();
    while let Some(p) = cur {
        let next = p.borrow().proto.clone();
        chain.push(p);
        if chain.len() > 64 {
            break;
        }
        cur = next;
    }
    for p in &chain {
        for rs in i.realms.values() {
            if Rc::ptr_eq(&rs.function_proto, p) {
                // Core intrinsics live in named fields; errors in error_protos; the rest in extra_protos.
                return match default_proto {
                    "Object" => Some(rs.object_proto.clone()),
                    "Array" => Some(rs.array_proto.clone()),
                    "Function" => Some(rs.function_proto.clone()),
                    "String" => Some(rs.string_proto.clone()),
                    "Number" => Some(rs.number_proto.clone()),
                    "Boolean" => Some(rs.boolean_proto.clone()),
                    "Symbol" => Some(rs.symbol_proto.clone()),
                    other => rs
                        .error_protos
                        .get(other)
                        .cloned()
                        .or_else(|| rs.extra_protos.get(other).cloned()),
                };
            }
        }
    }
    None
}

fn new_from_ctor(i: &mut Interp, default_proto: &str) -> Result<Gc, Value> {
    let proto = match &i.new_target {
        nt @ Value::Obj(_) => match ab(i.get_member(&nt.clone(), "prototype"))? {
            Value::Obj(p) => Some(p),
            // The constructor's `prototype` isn't an object — use its realm's intrinsic.
            _ => ctor_realm_proto(i, &i.new_target.clone(), default_proto)
                .or_else(|| i.extra_protos.get(default_proto).cloned()),
        },
        _ => i.extra_protos.get(default_proto).cloned(),
    };
    Ok(Object::new(proto))
}

fn ta_construct(i: &mut Interp, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "TypedArray constructor requires 'new'"));
    }
    let es = kind.elsize();
    let (buf_val, buf_ptr, offset, len, track) = match args.first() {
        None => {
            let (bv, bp) = make_array_buffer(i, 0);
            (bv, bp, 0, 0, false)
        }
        Some(Value::Obj(o)) if i.array_buffers.contains_key(&(Rc::as_ptr(o) as usize)) => {
            let bp = Rc::as_ptr(o) as usize;
            let bv = Value::Obj(o.clone());
            // byteOffset is a ToIndex value and must be a multiple of the element size.
            let offset = match args.get(1) {
                Some(v) if !matches!(v, Value::Undefined) => to_index(i, v)?,
                _ => 0,
            };
            if offset % es != 0 {
                return Err(i.make_error(
                    "RangeError",
                    "byteOffset is not aligned to the element size",
                ));
            }
            let explicit = matches!(args.get(2), Some(v) if !matches!(v, Value::Undefined));
            let len_arg = match args.get(2) {
                Some(v) if !matches!(v, Value::Undefined) => Some(to_index(i, v)?),
                _ => None,
            };
            // Coercing byteOffset/length above may have detached the buffer.
            if !i.array_buffers.contains_key(&bp) {
                return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
            }
            let buflen = i.array_buffers[&bp].len();
            let rv = ab(i.get_member(&bv, "resizable"))?;
            let resizable = i.to_boolean(&rv);
            let len = match len_arg {
                Some(l) => {
                    if offset + l * es > buflen {
                        return Err(i.make_error("RangeError", "invalid typed array length"));
                    }
                    l
                }
                None => {
                    // The "not a multiple of element size" rule only applies to a fixed-length
                    // buffer; over a resizable buffer the view is length-tracking (auto length).
                    if !resizable && !buflen.is_multiple_of(es) {
                        return Err(i.make_error(
                            "RangeError",
                            "buffer length is not a multiple of the element size",
                        ));
                    }
                    if offset > buflen {
                        return Err(i.make_error("RangeError", "byteOffset is out of bounds"));
                    }
                    buflen.saturating_sub(offset) / es
                }
            };
            (bv, bp, offset, len, !explicit && resizable)
        }
        // A non-object first argument is a length (ToIndex): NaN→0, negative/too-large→RangeError,
        // a Symbol/BigInt → TypeError via ToNumber.
        Some(v) if !matches!(v, Value::Obj(_)) => {
            let len = to_index(i, v)?;
            if len > MAX_ARRAY_OP_LEN {
                return Err(i.make_error("RangeError", "Invalid typed array length"));
            }
            let (bv, bp) = make_array_buffer(i, len * es);
            (bv, bp, 0, len, false)
        }
        Some(other) => {
            // TypedArray / array-like / iterable object: copy element values into a fresh buffer.
            let items = if i.has_iterator(other) {
                ab(i.iterate(other))?
            } else {
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
            };
            let len = items.len();
            let (bv, bp) = make_array_buffer(i, len * es);
            let info = TaInfo {
                buffer: bp,
                offset: 0,
                len,
                kind,
                track: false,
            };
            for (idx, item) in items.iter().enumerate() {
                ab(i.ta_store(&info, idx, item))?;
            }
            (bv, bp, 0, len, false)
        }
    };
    // The instance prototype comes from new.target.prototype when it's an object (subclassing /
    // Reflect.construct), else the intrinsic %TypedArray.prototype% for this element type.
    let proto = match &i.new_target {
        nt @ Value::Obj(_) => match ab(i.get_member(&nt.clone(), "prototype"))? {
            Value::Obj(p) => Some(p),
            _ => i.extra_protos.get(kind.name()).cloned(),
        },
        _ => i.extra_protos.get(kind.name()).cloned(),
    };
    let obj = Object::new(proto);
    let p = Rc::as_ptr(&obj) as usize;
    i.typed_arrays.insert(
        p,
        TaInfo {
            buffer: buf_ptr,
            offset,
            len,
            kind,
            track,
        },
    );
    // length / byteLength / byteOffset / buffer / BYTES_PER_ELEMENT are inherited accessors+constants,
    // not own properties: length/byteLength/byteOffset are computed in get_member, the buffer object
    // is kept in a side table, and BYTES_PER_ELEMENT lives on the prototype.
    let _ = es;
    i.ta_buffer.insert(p, buf_val);
    Ok(Value::Obj(obj))
}

fn ta_set(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "set on non-TypedArray"))?;
    let info = *i
        .typed_arrays
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "set on non-TypedArray"))?;
    // targetOffset = ToIntegerOrInfinity(offset); a negative offset is a RangeError.
    let offset_n = match arg(args, 1) {
        Value::Undefined => 0.0,
        v => {
            let n = ab(i.to_number(&v))?;
            if n.is_nan() {
                0.0
            } else {
                n.trunc()
            }
        }
    };
    if offset_n < 0.0 {
        return Err(i.make_error("RangeError", "TypedArray.prototype.set offset is negative"));
    }
    // Coercing the offset can run user code that detaches the target — re-validate.
    if !i.array_buffers.contains_key(&info.buffer) {
        return Err(i.make_error("TypeError", "detached buffer"));
    }
    let offset = offset_n as usize;
    let source = arg(args, 0);
    // The source is array-like (or another TypedArray): read its length, bounds-check, then copy
    // element by element (so a throwing getter leaves earlier elements written).
    let src_len = match &source {
        Value::Obj(_) => {
            let lenv = ab(i.get_member(&source, "length"))?;
            let n = ab(i.to_number(&lenv))?;
            if n.is_nan() || n < 0.0 {
                0
            } else {
                n.min(9007199254740991.0) as usize
            }
        }
        _ => 0,
    };
    // Compare in f64 so an Infinity offset can't overflow the usize addition.
    if offset_n + src_len as f64 > i.ta_len(&info).unwrap_or(0) as f64 {
        return Err(i.make_error("RangeError", "source is too large for the target at offset"));
    }
    for k in 0..src_len {
        let item = ab(i.get_member(&source, &k.to_string()))?;
        ab(i.ta_store(&info, offset + k, &item))?;
    }
    Ok(Value::Undefined)
}

fn ta_subarray(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let ptr =
        map_ptr(&this).ok_or_else(|| i.make_error("TypeError", "subarray on non-TypedArray"))?;
    let info = *i
        .typed_arrays
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "subarray on non-TypedArray"))?;
    let es = info.kind.elsize();
    let len = i.ta_len(&info).unwrap_or(0) as i64;
    let begin = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
    let end = match arg(args, 1) {
        Value::Undefined => len,
        v => norm_index(ab(i.to_number(&v))?, len, len),
    };
    let new_len = (end - begin).max(0) as usize;
    let new_offset = info.offset + begin as usize * es;
    let buf_val = ab(i.get_member(&this, "buffer"))?;
    // subarray builds the result via TypedArraySpeciesCreate(O, «buffer, byteOffset, length»), so a
    // subclass `@@species` (or a detaching `constructor` getter) is honored.
    ta_species_create(
        i,
        &this,
        info.kind,
        &[
            buf_val,
            Value::Num(new_offset as f64),
            Value::Num(new_len as f64),
        ],
    )
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
ta_ctor!(ta_ctor_f16, TaKind::F16);
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
        // Each method's `length` matches its required-parameter count.
        let len = match *name {
            "copyWithin" | "slice" | "subarray" | "with" => 2,
            "keys" | "values" | "entries" | "toReversed" => 0,
            _ => 1,
        };
        it.def_method(&ta_proto, name, len, *f);
    }
    if let Some(sym) = it.iterator_sym.clone() {
        let k = Interp::sym_key(&sym);
        if let Some(p) = it.array_proto.borrow().props.get(&k).cloned() {
            ta_proto.borrow_mut().props.insert(k, p);
        }
    }
    it.def_method(&ta_proto, "set", 1, ta_set);
    it.def_method(&ta_proto, "subarray", 2, ta_subarray);
    it.def_method(&ta_proto, "toLocaleString", 0, |i, this, _a| {
        // Per spec: call `toLocaleString` on each element and join with ",".
        let info = map_ptr(&this).and_then(|p| i.typed_arrays.get(&p).copied());
        let info = info.ok_or_else(|| i.make_error("TypeError", "not a TypedArray"))?;
        if !i.array_buffers.contains_key(&info.buffer) {
            return Err(i.make_error("TypeError", "detached buffer"));
        }
        let mut out = String::new();
        for k in 0..i.ta_len(&info).unwrap_or(0) {
            if k > 0 {
                out.push(',');
            }
            let v = ab(i.get_member(&this, &k.to_string()))?;
            let tls = ab(i.get_member(&v, "toLocaleString"))?;
            if !tls.is_callable() {
                return Err(i.make_error("TypeError", "toLocaleString is not a function"));
            }
            let s = ab(i.call(tls, v, &[]))?;
            out.push_str(&ab(i.to_string(&s))?);
        }
        Ok(Value::from_string(out))
    });
    // length / byteLength / byteOffset / buffer are accessor getters on %TypedArray.prototype% that
    // brand-check the receiver (calling one on a non-TypedArray is a TypeError).
    for (name, getter) in [
        (
            "length",
            ta_length_get as fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>,
        ),
        ("byteLength", ta_bytelength_get),
        ("byteOffset", ta_byteoffset_get),
        ("buffer", ta_buffer_get),
    ] {
        let g = it.make_native(name, 0, getter);
        ta_proto.borrow_mut().props.insert(
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
    // %TypedArray.prototype%[@@toStringTag]: a getter returning the kind name (e.g. "Int8Array")
    // for a TypedArray receiver, else undefined (no throw, no setter).
    if let Some(key) = well_known_key(it, "toStringTag") {
        let g = it.make_native("get [Symbol.toStringTag]", 0, |i, this, _| {
            Ok(match map_ptr(&this).and_then(|p| i.typed_arrays.get(&p)) {
                Some(info) => Value::str(info.kind.name()),
                None => Value::Undefined,
            })
        });
        ta_proto.borrow_mut().props.insert(
            key,
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

    // The abstract %TypedArray% intrinsic: each concrete TypedArray constructor inherits from it,
    // and its `.prototype` is the shared `ta_proto`. Tests reach it via Object.getPrototypeOf(Int8Array).
    let ta_ctor = it.make_native("TypedArray", 0, |i, t, _a| {
        // Abstract: a direct `new %TypedArray%()` throws; subclass `super()` (this is set) is allowed.
        if matches!(t, Value::Undefined) {
            return Err(i.make_error(
                "TypeError",
                "Abstract class TypedArray not directly constructable",
            ));
        }
        Ok(t)
    });
    ta_ctor.borrow_mut().is_constructor = true;
    ta_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(ta_proto.clone()), false, false, false),
    );
    ta_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(ta_ctor.clone())),
    );
    it.def_method(&ta_ctor, "of", 0, ta_of);
    it.def_method(&ta_ctor, "from", 1, ta_from);
    install_species(it, &ta_ctor);

    let kinds: [(TaKind, NativeFn); 12] = [
        (TaKind::I8, ta_ctor_i8),
        (TaKind::U8, ta_ctor_u8),
        (TaKind::U8Clamped, ta_ctor_u8c),
        (TaKind::I16, ta_ctor_i16),
        (TaKind::U16, ta_ctor_u16),
        (TaKind::I32, ta_ctor_i32),
        (TaKind::U32, ta_ctor_u32),
        (TaKind::F16, ta_ctor_f16),
        (TaKind::F32, ta_ctor_f32),
        (TaKind::F64, ta_ctor_f64),
        (TaKind::I64, ta_ctor_i64),
        (TaKind::U64, ta_ctor_u64),
    ];
    for (kind, ctor_fn) in kinds {
        let proto = Object::new(Some(ta_proto.clone()));
        it.extra_protos.insert(kind.name(), proto.clone());
        set_builtin(
            &proto,
            "BYTES_PER_ELEMENT",
            Value::Num(kind.elsize() as f64),
        );
        let ctor = it.make_native(kind.name(), 3, ctor_fn);
        ctor.borrow_mut().proto = Some(ta_ctor.clone()); // [[Prototype]] is %TypedArray%
        ctor.borrow_mut().props.insert(
            "prototype",
            Property::data(Value::Obj(proto.clone()), false, false, false),
        );
        proto
            .borrow_mut()
            .props
            .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
        set_builtin(&ctor, "BYTES_PER_ELEMENT", Value::Num(kind.elsize() as f64));
        // from/of are inherited from %TypedArray% (they construct through `this`), not own here.
        set_builtin(&it.global, kind.name(), Value::Obj(ctor));
    }
    install_uint8_base64(it);
}

// --- Uint8Array base64 / hex (the Stage-3 proposal) -------------------------------------------

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode(bytes: &[u8], url: bool, pad: bool) -> String {
    let alpha = if url { B64_URL } else { B64_STD };
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alpha[(n >> 18 & 63) as usize] as char);
        out.push(alpha[(n >> 12 & 63) as usize] as char);
        match chunk.len() {
            1 => {
                if pad {
                    out.push_str("==");
                }
            }
            2 => {
                out.push(alpha[(n >> 6 & 63) as usize] as char);
                if pad {
                    out.push('=');
                }
            }
            _ => {
                out.push(alpha[(n >> 6 & 63) as usize] as char);
                out.push(alpha[(n & 63) as usize] as char);
            }
        }
    }
    out
}
fn b64_decode(s: &str, url: bool) -> Option<Vec<u8>> {
    let alpha = if url { B64_URL } else { B64_STD };
    let mut vals: Vec<u32> = Vec::new();
    for c in s.chars() {
        if c == '=' {
            break;
        }
        if c.is_whitespace() {
            continue;
        }
        let pos = alpha.iter().position(|&a| a as char == c)?;
        vals.push(pos as u32);
    }
    let mut out = Vec::new();
    for grp in vals.chunks(4) {
        if grp.len() == 1 {
            return None; // a lone 6-bit group is not decodable
        }
        let mut n = 0u32;
        for (idx, &v) in grp.iter().enumerate() {
            n |= v << (18 - 6 * idx);
        }
        out.push((n >> 16) as u8);
        if grp.len() >= 3 {
            out.push((n >> 8) as u8);
        }
        if grp.len() >= 4 {
            out.push(n as u8);
        }
    }
    Some(out)
}
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    for pair in chars.chunks(2) {
        let hi = pair[0].to_digit(16)?;
        let lo = pair[1].to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Read a Uint8Array receiver's bytes; errors if `this` isn't a (non-detached) Uint8Array.
fn u8_bytes(i: &mut Interp, this: &Value) -> Result<Vec<u8>, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a Uint8Array"))?;
    let info = *i
        .typed_arrays
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a Uint8Array"))?;
    if !matches!(info.kind, TaKind::U8) {
        return Err(i.make_error("TypeError", "method requires a Uint8Array"));
    }
    let buf = i
        .array_buffers
        .get(&info.buffer)
        .ok_or_else(|| i.make_error("TypeError", "detached buffer"))?;
    Ok(buf[info.offset..info.offset + info.len].to_vec())
}
fn make_u8array(i: &mut Interp, bytes: Vec<u8>) -> Result<Value, Value> {
    let ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "Uint8Array"))?;
    let ta = ab(i.construct(ctor, &[Value::Num(bytes.len() as f64)]))?;
    if let Some(ptr) = map_ptr(&ta) {
        if let Some(info) = i.typed_arrays.get(&ptr).copied() {
            if let Some(buf) = i.array_buffers.get_mut(&info.buffer) {
                buf[info.offset..info.offset + bytes.len()].copy_from_slice(&bytes);
            }
        }
    }
    Ok(ta)
}
fn b64_option_url(i: &mut Interp, opts: &Value) -> Result<bool, Value> {
    if let Value::Obj(_) = opts {
        let a = ab(i.get_member(opts, "alphabet"))?;
        match a {
            Value::Undefined => Ok(false),
            Value::Str(s) if &*s == "base64" => Ok(false),
            Value::Str(s) if &*s == "base64url" => Ok(true),
            _ => Err(i.make_error("TypeError", "alphabet must be 'base64' or 'base64url'")),
        }
    } else if matches!(opts, Value::Undefined) {
        Ok(false)
    } else {
        Err(i.make_error("TypeError", "options must be an object"))
    }
}

fn install_uint8_base64(it: &mut Interp) {
    let proto = it.extra_protos.get("Uint8Array").cloned().unwrap();
    let ctor = match it
        .global
        .borrow()
        .props
        .get("Uint8Array")
        .map(|p| p.value.clone())
    {
        Some(Value::Obj(c)) => c,
        _ => return,
    };
    it.def_method(&proto, "toHex", 0, |i, this, _| {
        let bytes = u8_bytes(i, &this)?;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        Ok(Value::from_string(s))
    });
    it.def_method(&proto, "toBase64", 0, |i, this, a| {
        let bytes = u8_bytes(i, &this)?;
        let url = b64_option_url(i, &arg(a, 0))?;
        let omit_padding = if let Value::Obj(_) = arg(a, 0) {
            let op = ab(i.get_member(&arg(a, 0), "omitPadding"))?;
            i.to_boolean(&op)
        } else {
            false
        };
        Ok(Value::from_string(b64_encode(&bytes, url, !omit_padding)))
    });
    it.def_method(&ctor, "fromHex", 1, |i, _t, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "fromHex requires a string")),
        };
        let bytes =
            hex_decode(&s).ok_or_else(|| i.make_error("SyntaxError", "invalid hex string"))?;
        make_u8array(i, bytes)
    });
    it.def_method(&ctor, "fromBase64", 1, |i, _t, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "fromBase64 requires a string")),
        };
        let url = b64_option_url(i, &arg(a, 1))?;
        let bytes = b64_decode(&s, url)
            .ok_or_else(|| i.make_error("SyntaxError", "invalid base64 string"))?;
        make_u8array(i, bytes)
    });
    it.def_method(&proto, "setFromHex", 1, |i, this, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "setFromHex requires a string")),
        };
        let bytes =
            hex_decode(&s).ok_or_else(|| i.make_error("SyntaxError", "invalid hex string"))?;
        u8_set_bytes(i, &this, &bytes, s.len())
    });
    it.def_method(&proto, "setFromBase64", 1, |i, this, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "setFromBase64 requires a string")),
        };
        let url = b64_option_url(i, &arg(a, 1))?;
        let bytes = b64_decode(&s, url)
            .ok_or_else(|| i.make_error("SyntaxError", "invalid base64 string"))?;
        u8_set_bytes(i, &this, &bytes, s.len())
    });
}

/// Write decoded `bytes` into the Uint8Array receiver (truncating to its length); returns
/// `{read, written}`.
fn u8_set_bytes(
    i: &mut Interp,
    this: &Value,
    bytes: &[u8],
    src_len: usize,
) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a Uint8Array"))?;
    let info = *i
        .typed_arrays
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a Uint8Array"))?;
    if !matches!(info.kind, TaKind::U8) {
        return Err(i.make_error("TypeError", "requires a Uint8Array"));
    }
    let written = bytes.len().min(info.len);
    if let Some(buf) = i.array_buffers.get_mut(&info.buffer) {
        buf[info.offset..info.offset + written].copy_from_slice(&bytes[..written]);
    }
    let result = i.new_object();
    set_data(&result, "read", Value::Num(src_len as f64));
    set_data(&result, "written", Value::Num(written as f64));
    Ok(Value::Obj(result))
}

fn ta_of(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    if !is_constructor_value(&this) {
        return Err(i.make_error("TypeError", "TypedArray.of requires a constructor receiver"));
    }
    let ta = ab(i.construct(this, &[Value::Num(args.len() as f64)]))?;
    for (k, v) in args.iter().enumerate() {
        ab(i.set_member(&ta, &k.to_string(), v.clone()))?;
    }
    Ok(ta)
}
fn ta_from(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    if !is_constructor_value(&this) {
        return Err(i.make_error(
            "TypeError",
            "TypedArray.from requires a constructor receiver",
        ));
    }
    let mapfn = arg(args, 1);
    if !matches!(mapfn, Value::Undefined) && !mapfn.is_callable() {
        return Err(i.make_error("TypeError", "mapfn is not callable"));
    }
    let this_arg = arg(args, 2);
    // A source with an @@iterator is iterated; otherwise it is treated as an array-like.
    let source = arg(args, 0);
    let items: Vec<Value> = match &source {
        Value::Str(_) => ab(i.iterate(&source))?,
        Value::Obj(_) if i.has_iterator(&source) => ab(i.iterate(&source))?,
        Value::Obj(_) => {
            let lenv = ab(i.get_member(&source, "length"))?;
            let n = ab(i.to_number(&lenv))?;
            let len = if n.is_nan() || n < 0.0 {
                0
            } else {
                n.min(9007199254740991.0) as usize
            };
            let mut v = Vec::with_capacity(len.min(1 << 20));
            for k in 0..len {
                v.push(ab(i.get_member(&source, &k.to_string()))?);
            }
            v
        }
        Value::Undefined | Value::Null => {
            return Err(i.make_error("TypeError", "TypedArray.from source is not iterable"))
        }
        _ => Vec::new(),
    };
    let ta = ab(i.construct(this, &[Value::Num(items.len() as f64)]))?;
    for (k, v) in items.into_iter().enumerate() {
        let val = if mapfn.is_callable() {
            ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k as f64)]))?
        } else {
            v
        };
        ab(i.set_member(&ta, &k.to_string(), val))?;
    }
    Ok(ta)
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
const WDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn date_str_part(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (y, mo, d, _, _, _, _, wd) = ms_to_parts(t);
    Some(format!(
        "{} {} {:02} {:04}",
        WDAYS[wd as usize], MONTHS[mo as usize], d, y
    ))
}
fn time_str_part(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (_, _, _, h, mi, s, _, _) = ms_to_parts(t);
    Some(format!(
        "{h:02}:{mi:02}:{s:02} GMT+0000 (Coordinated Universal Time)"
    ))
}
fn utc_string(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (y, mo, d, h, mi, s, _, wd) = ms_to_parts(t);
    Some(format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WDAYS[wd as usize], d, MONTHS[mo as usize], y, h, mi, s
    ))
}

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
    obj.borrow_mut()
        .props
        .insert(key, Property::data(v, true, false, false));
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

/// A multi-argument Date setter (setHours, setFullYear, …). Per spec, ALL provided arguments are
/// ToNumber-coerced first, in order, before any field is applied; `start_sel` is the field of the
/// first argument (the field order skips the unused selector 3). `setFullYear`/`setUTCFullYear`
/// (start 0) treat a NaN receiver as the epoch; the time setters leave it NaN.
fn date_set_multi(
    i: &mut Interp,
    this: &Value,
    start_sel: u8,
    args: &[Value],
    n_max: usize,
) -> Result<Value, Value> {
    const ORDER: [u8; 7] = [0, 1, 2, 4, 5, 6, 7];
    let start_idx = ORDER.iter().position(|&f| f == start_sel).unwrap();
    let count = args.len().clamp(1, n_max);
    // thisTimeValue validation precedes argument coercion (a non-Date receiver throws before any
    // argument's valueOf runs); then coerce every read argument up front, in order.
    let t = date_ms(i, this)?;
    let mut vals = Vec::with_capacity(count);
    for k in 0..count {
        vals.push(ab(i.to_number(&arg(args, k)))?);
    }
    let nan_to_zero = start_sel == 0;
    // A NaN stored time (for setters that don't zero it) yields NaN and leaves [[DateValue]]
    // untouched — so an argument's valueOf side-effect on the receiver persists.
    if t.is_nan() && !nan_to_zero {
        return Ok(Value::Num(f64::NAN));
    }
    let mut any_nan = t.is_nan() && !nan_to_zero;
    let base = if t.is_nan() { 0.0 } else { t };
    let (mut y, mut mo, mut d, mut h, mut mi, mut s, mut ml, _) = ms_to_parts(base);
    for (k, &v) in vals.iter().enumerate() {
        if !v.is_finite() {
            any_nan = true;
        }
        let n = v as i64;
        match ORDER[start_idx + k] {
            0 => y = n,
            1 => mo = n,
            2 => d = n,
            4 => h = n,
            5 => mi = n,
            6 => s = n,
            _ => ml = n,
        }
    }
    let ms = if any_nan {
        f64::NAN
    } else {
        parts_to_ms(y, mo, d, h, mi, s, ml)
    };
    if let Value::Obj(o) = this {
        set_internal(o, "__date_ms", Value::Num(ms));
    }
    Ok(Value::Num(ms))
}

/// Minimal ISO-8601 parser: `YYYY[-MM[-DD]][THH:mm[:ss[.sss]]][Z]`. Returns NaN on anything else.
/// Best-effort parse of the RFC-2822-ish / `toString`/`toUTCString`/`toDateString` formats (e.g.
/// "Thu, 01 Jan 1970 00:00:00 GMT", "Wed Jul 28 1993 14:39:07 GMT-0600 (…)").
fn parse_rfc(s: &str) -> f64 {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let (mut year, mut month, mut day): (Option<i64>, Option<i64>, Option<i64>) =
        (None, None, None);
    let (mut hh, mut mm, mut ss) = (0i64, 0i64, 0i64);
    let mut offset: i64 = 0; // minutes east of UTC
    let mut got_time = false;
    for tok in s.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')')) {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let low = tok.to_lowercase();
        if let Some(idx) = MONTHS.iter().position(|m| low.starts_with(m)) {
            month = Some(idx as i64);
        } else if tok.contains(':') && !got_time {
            let mut p = tok.split(':');
            hh = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            mm = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            ss = p.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            got_time = true;
        } else if let Ok(n) = tok.parse::<i64>() {
            if tok.len() >= 4 || n > 31 {
                year = Some(n);
            } else if day.is_none() {
                day = Some(n);
            } else if year.is_none() {
                year = Some(n);
            }
        } else if low.starts_with("gmt") || low.starts_with('+') || low.starts_with('-') {
            let rest = low.trim_start_matches("gmt");
            let sign = rest.chars().next();
            let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 4 {
                let oh: i64 = digits[..2].parse().unwrap_or(0);
                let om: i64 = digits[2..4].parse().unwrap_or(0);
                let mag = oh * 60 + om;
                offset = if sign == Some('-') { -mag } else { mag };
            }
        }
    }
    match (year, month, day) {
        (Some(y), Some(mo), Some(d)) => {
            parts_to_ms(y, mo, d, hh, mm, ss, 0) - (offset as f64) * 60000.0
        }
        _ => f64::NAN,
    }
}

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
            let f3: String = f
                .chars()
                .take(3)
                .chain(std::iter::repeat('0'))
                .take(3)
                .collect();
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
            let d = if args.len() > 2 {
                ab(i.to_number(&args[2]))? as i64
            } else {
                1
            };
            let h = if args.len() > 3 {
                ab(i.to_number(&args[3]))? as i64
            } else {
                0
            };
            let mi = if args.len() > 4 {
                ab(i.to_number(&args[4]))? as i64
            } else {
                0
            };
            let s = if args.len() > 5 {
                ab(i.to_number(&args[5]))? as i64
            } else {
                0
            };
            let ml = if args.len() > 6 {
                ab(i.to_number(&args[6]))? as i64
            } else {
                0
            };
            parts_to_ms(y, mo, d, h, mi, s, ml)
        }
    };
    let obj = new_from_ctor(i, "Date")?;
    set_internal(&obj, "__date_ms", Value::Num(ms));
    Ok(Value::Obj(obj))
}

fn iso_string(t: f64) -> Option<String> {
    if !t.is_finite() {
        return None;
    }
    let (y, mo, d, h, mi, s, ml, _) = ms_to_parts(t);
    Some(format!(
        "{y:04}-{:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ml:03}Z",
        mo + 1
    ))
}

fn install_date(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Date", proto.clone());

    it.def_method(&proto, "getTime", 0, |i, this, _| {
        Ok(Value::Num(date_ms(i, &this)?))
    });
    it.def_method(&proto, "valueOf", 0, |i, this, _| {
        Ok(Value::Num(date_ms(i, &this)?))
    });
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
        date_set_multi(i, &this, 0, a, 3)
    });
    // Annex B legacy getYear/setYear (years offset from 1900).
    it.def_method(&proto, "getYear", 0, |i, this, _| {
        let f = ab(i.get_member(&this, "getFullYear"))?;
        let yv = ab(i.call(f, this.clone(), &[]))?;
        let y = ab(i.to_number(&yv))?;
        Ok(Value::Num(if y.is_nan() { f64::NAN } else { y - 1900.0 }))
    });
    it.def_method(&proto, "setYear", 1, |i, this, a| {
        let y = ab(i.to_number(&arg(a, 0)))?;
        let full = if y.is_nan() {
            f64::NAN
        } else {
            let yi = y.trunc() as i64;
            if (0..=99).contains(&yi) {
                1900.0 + yi as f64
            } else {
                y
            }
        };
        date_set_multi(i, &this, 0, &[Value::Num(full)], 1)
    });
    it.def_method(&proto, "setMonth", 2, |i, this, a| {
        date_set_multi(i, &this, 1, a, 2)
    });
    it.def_method(&proto, "setDate", 1, |i, this, a| {
        date_set_multi(i, &this, 2, a, 1)
    });
    it.def_method(&proto, "setHours", 4, |i, this, a| {
        date_set_multi(i, &this, 4, a, 4)
    });
    it.def_method(&proto, "setMinutes", 3, |i, this, a| {
        date_set_multi(i, &this, 5, a, 3)
    });
    it.def_method(&proto, "setSeconds", 2, |i, this, a| {
        date_set_multi(i, &this, 6, a, 2)
    });
    it.def_method(&proto, "setMilliseconds", 1, |i, this, a| {
        date_set_multi(i, &this, 7, a, 1)
    });
    // UTC setters mirror the local ones (offset 0).
    it.def_method(&proto, "setUTCFullYear", 3, |i, this, a| {
        date_set_multi(i, &this, 0, a, 3)
    });
    it.def_method(&proto, "setUTCMonth", 2, |i, this, a| {
        date_set_multi(i, &this, 1, a, 2)
    });
    it.def_method(&proto, "setUTCDate", 1, |i, this, a| {
        date_set_multi(i, &this, 2, a, 1)
    });
    it.def_method(&proto, "setUTCHours", 4, |i, this, a| {
        date_set_multi(i, &this, 4, a, 4)
    });
    it.def_method(&proto, "setUTCMinutes", 3, |i, this, a| {
        date_set_multi(i, &this, 5, a, 3)
    });
    it.def_method(&proto, "setUTCSeconds", 2, |i, this, a| {
        date_set_multi(i, &this, 6, a, 2)
    });
    it.def_method(&proto, "setUTCMilliseconds", 1, |i, this, a| {
        date_set_multi(i, &this, 7, a, 1)
    });
    it.def_method(&proto, "toISOString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        match iso_string(t) {
            Some(s) => Ok(Value::from_string(s)),
            None => Err(i.make_error("RangeError", "Invalid time value")),
        }
    });
    it.def_method(&proto, "toJSON", 1, |i, this, _| {
        // Generic: ToObject, ToPrimitive(number); a non-finite time is null; else Invoke toISOString.
        let o = to_object_arg(i, this.clone(), "Date.prototype.toJSON")?;
        let ov = Value::Obj(o);
        let tv = ab(i.to_primitive(&ov, crate::eval::Hint::Number))?;
        if let Value::Num(n) = &tv {
            if !n.is_finite() {
                return Ok(Value::Null);
            }
        }
        let iso = ab(i.get_member(&ov, "toISOString"))?;
        if !iso.is_callable() {
            return Err(i.make_error("TypeError", "toISOString is not callable"));
        }
        ab(i.call(iso, ov, &[]))
    });
    it.def_method(&proto, "toString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            iso_string(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toDateString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            date_str_part(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toTimeString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            time_str_part(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toUTCString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            utc_string(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toGMTString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            utc_string(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    // No Intl: toLocale* return a stable implementation-defined string.
    it.def_method(&proto, "toLocaleString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            iso_string(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toLocaleDateString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            date_str_part(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });
    it.def_method(&proto, "toLocaleTimeString", 0, |i, this, _| {
        let t = date_ms(i, &this)?;
        Ok(Value::from_string(
            time_str_part(t).unwrap_or_else(|| "Invalid Date".to_string()),
        ))
    });

    let ctor = it.make_native("Date", 7, date_ctor);
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "now", 0, |_i, _t, _a| Ok(Value::Num(now_ms())));
    it.def_method(&ctor, "parse", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        let v = parse_iso(&s);
        Ok(Value::Num(if v.is_nan() { parse_rfc(&s) } else { v }))
    });
    it.def_method(&ctor, "UTC", 7, |i, _t, a| {
        // The year is always read; later components only if supplied. Coerce all reads first; any
        // non-finite component makes the whole result NaN.
        let count = a.len().clamp(1, 7);
        let defaults = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let mut vals = defaults;
        let mut nan = false;
        for k in 0..count {
            let v = ab(i.to_number(&arg(a, k)))?;
            if !v.is_finite() {
                nan = true;
            } else {
                vals[k] = v.trunc();
            }
        }
        if nan {
            return Ok(Value::Num(f64::NAN));
        }
        let mut y = vals[0] as i64;
        if (0..=99).contains(&y) {
            y += 1900;
        }
        Ok(Value::Num(parts_to_ms(
            y,
            vals[1] as i64,
            vals[2] as i64,
            vals[3] as i64,
            vals[4] as i64,
            vals[5] as i64,
            vals[6] as i64,
        )))
    });
    // Date.prototype[@@toPrimitive]: "number" hint uses valueOf, "string"/"default" use toString.
    let prim = it.make_native("[Symbol.toPrimitive]", 1, |i, this, a| {
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error(
                "TypeError",
                "Date.prototype[Symbol.toPrimitive] on non-object",
            ));
        }
        // The hint must be a primitive String; OrdinaryToPrimitive then tries the two methods in
        // order, returning the first non-object result.
        let hint = match arg(a, 0) {
            Value::Str(s) => s.to_string(),
            _ => return Err(i.make_error("TypeError", "invalid Symbol.toPrimitive hint")),
        };
        let methods: &[&str] = match hint.as_str() {
            "number" => &["valueOf", "toString"],
            "string" | "default" => &["toString", "valueOf"],
            _ => return Err(i.make_error("TypeError", "invalid Symbol.toPrimitive hint")),
        };
        for m in methods {
            let f = ab(i.get_member(&this, m))?;
            if f.is_callable() {
                let r = ab(i.call(f, this.clone(), &[]))?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        Err(i.make_error("TypeError", "cannot convert Date to a primitive value"))
    });
    if let Some(key) = well_known_key(it, "toPrimitive") {
        proto
            .borrow_mut()
            .props
            .insert(key, Property::builtin(Value::Obj(prim)));
    }
    set_builtin(&it.global, "Date", Value::Obj(ctor));
}

/// Whether a value is a constructor: a callable with an own `prototype` (built-in ctors / user
/// non-arrow functions, which also set is_constructor) — matching the `new` constructability rule.
fn is_constructor_value(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if {
        let b = o.borrow();
        !matches!(b.call, Callable::None) && (b.is_constructor || b.props.contains("prototype"))
    })
}

/// Install the `get [Symbol.species]` accessor (returns the receiver `this`) on a constructor.
fn install_species(it: &Interp, ctor: &Gc) {
    if let Some(key) = well_known_key(it, "species") {
        let getter = it.make_native("get [Symbol.species]", 0, |_i, this, _| Ok(this));
        ctor.borrow_mut().props.insert(
            key,
            Property {
                value: Value::Undefined,
                get: Some(Value::Obj(getter)),
                set: None,
                accessor: true,
                writable: false,
                enumerable: false,
                configurable: true,
            },
        );
    }
}

/// The internal property key for a well-known `Symbol.<name>`.
pub(crate) fn well_known_key(it: &Interp, name: &str) -> Option<String> {
    let sym = it
        .global
        .borrow()
        .props
        .get("Symbol")
        .map(|p| p.value.clone())?;
    if let Value::Obj(o) = sym {
        if let Some(p) = o.borrow().props.get(name) {
            if let Value::Sym(d) = &p.value {
                return Some(Interp::sym_key(d));
            }
        }
    }
    None
}

fn map_ptr(this: &Value) -> Option<usize> {
    this.as_obj().map(|o| Rc::as_ptr(o) as usize)
}

/// ArraySpeciesCreate(originalArray, length): build the result array for a method like map/filter,
/// honoring `this.constructor[@@species]`; for an ordinary array (or no species) it's a plain array.
fn make_sparse_array(i: &mut Interp, len: usize) -> Value {
    let arr = i.make_array(Vec::new());
    if let Value::Obj(o) = &arr {
        o.borrow_mut().props.insert(
            "length",
            crate::value::Property::data(Value::Num(len as f64), true, false, false),
        );
    }
    arr
}

fn array_species_create(i: &mut Interp, original: &Value, len: usize) -> Result<Value, Value> {
    let is_array = matches!(original, Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array));
    if !is_array {
        return Ok(make_sparse_array(i, len));
    }
    let ctor = ab(i.get_member(original, "constructor"))?;
    let mut species = Value::Undefined;
    if matches!(&ctor, Value::Obj(_)) {
        if let Some(key) = well_known_key(i, "species") {
            species = ab(i.get_member(&ctor, &key))?;
        }
    }
    if matches!(species, Value::Undefined | Value::Null) {
        return Ok(make_sparse_array(i, len));
    }
    let array_ctor = i
        .global
        .borrow()
        .props
        .get("Array")
        .map(|p| p.value.clone());
    if let (Value::Obj(s), Some(Value::Obj(ac))) = (&species, &array_ctor) {
        if Rc::ptr_eq(s, ac) {
            return Ok(make_sparse_array(i, len));
        }
    }
    if !species.is_callable() {
        return Err(i.make_error("TypeError", "Array @@species is not a constructor"));
    }
    ab(i.construct(species, &[Value::Num(len as f64)]))
}

/// Own enumerable string keys in spec [[OwnPropertyKeys]] order (array-index ascending first).
fn ordered_enum_keys(o: &Gc) -> Vec<Rc<str>> {
    let b = o.borrow();
    b.props
        .ordered_keys()
        .into_iter()
        .filter(|k| !Interp::is_sym_key(k) && b.props.get(k).map(|p| p.enumerable).unwrap_or(false))
        .collect()
}

/// The property key for `@@toStringTag` (`Symbol.toStringTag`), if Symbol is installed.
pub(crate) fn to_string_tag_key(i: &Interp) -> Option<String> {
    let sym = i
        .global
        .borrow()
        .props
        .get("Symbol")
        .map(|p| p.value.clone())?;
    if let Value::Obj(o) = sym {
        if let Some(p) = o.borrow().props.get("toStringTag") {
            if let Value::Sym(d) = &p.value {
                return Some(Interp::sym_key(d));
            }
        }
    }
    None
}

/// Install a non-enumerable, configurable `@@toStringTag` data property.
pub(crate) fn set_to_string_tag(i: &Interp, obj: &Gc, tag: &str) {
    if let Some(key) = to_string_tag_key(i) {
        obj.borrow_mut().props.insert(
            key,
            Property::data(Value::from_string(tag.to_string()), false, false, true),
        );
    }
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
    coll_ptr_kind(i, this, None)
}

/// Resolve a Map/Set backing pointer with a brand check. `want = Some("Map"|"Set")` requires that
/// exact kind; `None` accepts either strong collection but still rejects WeakMap/WeakSet and plain
/// objects (which lack a strong `[[MapData]]`/`[[SetData]]` slot).
fn coll_ptr_kind(i: &Interp, this: &Value, want: Option<&str>) -> Result<usize, Value> {
    let err = || i.make_error("TypeError", "method called on an incompatible receiver");
    let o = this.as_obj().ok_or_else(err)?;
    let ptr = Rc::as_ptr(o) as usize;
    if !i.map_data.contains_key(&ptr) {
        return Err(err());
    }
    let kind = o.borrow().props.get("__ck").map(|p| p.value.clone());
    let ok = match (&kind, want) {
        (Some(Value::Str(s)), Some(w)) => &**s == w,
        (Some(Value::Str(s)), None) => &**s == "Map" || &**s == "Set",
        _ => false,
    };
    if !ok {
        return Err(err());
    }
    Ok(ptr)
}

/// Build an iterator over a Map/Set's snapshot. `kind`: 0 = values, 1 = keys, 2 = [key,value].
fn collection_iter(i: &mut Interp, this: &Value, kind: u8) -> Result<Value, Value> {
    let ptr = coll_ptr(i, this)?;
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
    install_set_methods(it);
    install_map_methods(it);
}

fn install_map_methods(it: &mut Interp) {
    let mp = it.extra_protos.get("Map").cloned().unwrap();
    // getOrInsert(key, value): return the existing value, or insert and return `value`.
    it.def_method(&mp, "getOrInsert", 2, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
        let key = arg(a, 0);
        if let Some((_, v)) = i.map_data[&ptr]
            .iter()
            .find(|(k, _)| same_value_zero(k, &key))
        {
            return Ok(v.clone());
        }
        let value = arg(a, 1);
        i.map_data
            .entry(ptr)
            .or_default()
            .push((key, value.clone()));
        Ok(value)
    });
    it.def_method(&mp, "getOrInsertComputed", 2, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
        let key = arg(a, 0);
        let cb = arg(a, 1);
        if !cb.is_callable() {
            return Err(i.make_error("TypeError", "callback is not callable"));
        }
        if let Some((_, v)) = i.map_data[&ptr]
            .iter()
            .find(|(k, _)| same_value_zero(k, &key))
        {
            return Ok(v.clone());
        }
        let value = ab(i.call(cb, Value::Undefined, std::slice::from_ref(&key)))?;
        // Re-check (the callback may have mutated the map), then insert.
        if let Some((_, v)) = i.map_data[&ptr]
            .iter()
            .find(|(k, _)| same_value_zero(k, &key))
        {
            return Ok(v.clone());
        }
        i.map_data
            .entry(ptr)
            .or_default()
            .push((key, value.clone()));
        Ok(value)
    });
}

/// The receiver Set's values (deduped insertion order). Errors if `this` isn't a Set.
fn set_values(i: &mut Interp, this: &Value) -> Result<Vec<Value>, Value> {
    let ptr = map_ptr(this).filter(|p| i.map_data.contains_key(p));
    match ptr {
        Some(p) => Ok(i.map_data[&p].iter().map(|(k, _)| k.clone()).collect()),
        None => Err(i.make_error("TypeError", "method called on an incompatible receiver")),
    }
}
/// Build a fresh Set from `values` (deduped via SameValueZero).
fn new_set(i: &mut Interp, values: Vec<Value>) -> Value {
    let obj =
        new_from_ctor(i, "Set").unwrap_or_else(|_| Object::new(i.extra_protos.get("Set").cloned()));
    let ptr = Rc::as_ptr(&obj) as usize;
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for v in values {
        if !entries.iter().any(|(k, _)| same_value_zero(k, &v)) {
            entries.push((v.clone(), v));
        }
    }
    i.map_data.insert(ptr, entries);
    set_internal(&obj, "__ck", Value::str("Set"));
    Value::Obj(obj)
}
/// GetSetRecord: a set-like `other` exposes a numeric `size`, and callable `has` and `keys`.
fn set_record(i: &mut Interp, other: &Value) -> Result<(Value, Value, f64), Value> {
    if !matches!(other, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "argument is not an object"));
    }
    // GetSetRecord: size → ToNumber (NaN throws TypeError), ToIntegerOrInfinity (negative throws
    // RangeError); then `has` and `keys` must be callable.
    let size_v = ab(i.get_member(other, "size"))?;
    let size = ab(i.to_number(&size_v))?;
    if size.is_nan() {
        return Err(i.make_error("TypeError", "set-like size is NaN"));
    }
    let int_size = if size.is_infinite() {
        size
    } else {
        size.trunc()
    };
    if int_size < 0.0 {
        return Err(i.make_error("RangeError", "set-like size is negative"));
    }
    let has = ab(i.get_member(other, "has"))?;
    if !has.is_callable() {
        return Err(i.make_error("TypeError", "set-like has is not callable"));
    }
    let keys = ab(i.get_member(other, "keys"))?;
    if !keys.is_callable() {
        return Err(i.make_error("TypeError", "set-like keys is not callable"));
    }
    Ok((has, keys, int_size))
}
fn set_like_has(i: &mut Interp, has: &Value, other: &Value, v: &Value) -> Result<bool, Value> {
    let r = ab(i.call(has.clone(), other.clone(), std::slice::from_ref(v)))?;
    Ok(i.to_boolean(&r))
}
fn set_like_keys(i: &mut Interp, keys: &Value, other: &Value) -> Result<Vec<Value>, Value> {
    // `keys` returns an iterator *record*: step its `next` directly rather than calling GetIterator
    // (the result need not be iterable itself).
    let iter = ab(i.call(keys.clone(), other.clone(), &[]))?;
    let next = ab(i.get_member(&iter, "next"))?;
    if !next.is_callable() {
        return Err(i.make_error("TypeError", "set-like keys iterator has no next method"));
    }
    let mut out = Vec::new();
    loop {
        let r = ab(i.call(next.clone(), iter.clone(), &[]))?;
        if !matches!(r, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "iterator result is not an object"));
        }
        let done = ab(i.get_member(&r, "done"))?;
        if i.to_boolean(&done) {
            break;
        }
        out.push(ab(i.get_member(&r, "value"))?);
    }
    Ok(out)
}

fn install_set_methods(it: &mut Interp) {
    let sp = it.extra_protos.get("Set").cloned().unwrap();
    it.def_method(&sp, "union", 1, |i, this, a| {
        let mut vals = set_values(i, &this)?;
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        for k in set_like_keys(i, &keys, &arg(a, 0))? {
            if !vals.iter().any(|v| same_value_zero(v, &k)) {
                vals.push(k);
            }
        }
        Ok(new_set(i, vals))
    });
    it.def_method(&sp, "intersection", 1, |i, this, a| {
        let vals = set_values(i, &this)?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let mut out = Vec::new();
        if (vals.len() as f64) <= other_size {
            // Iterate this Set, probing the other's `has`.
            for v in vals {
                if set_like_has(i, &has, &arg(a, 0), &v)?
                    && !out.iter().any(|o| same_value_zero(o, &v))
                {
                    out.push(v);
                }
            }
        } else {
            // Iterate the other's keys, probing this Set directly (no `has` calls on the other).
            for k in set_like_keys(i, &keys, &arg(a, 0))? {
                if vals.iter().any(|v| same_value_zero(v, &k))
                    && !out.iter().any(|o| same_value_zero(o, &k))
                {
                    out.push(k);
                }
            }
        }
        Ok(new_set(i, out))
    });
    it.def_method(&sp, "difference", 1, |i, this, a| {
        let vals = set_values(i, &this)?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        if (vals.len() as f64) <= other_size {
            // Iterate this Set, dropping elements the other's `has` reports.
            let mut out = Vec::new();
            for v in vals {
                if !set_like_has(i, &has, &arg(a, 0), &v)? {
                    out.push(v);
                }
            }
            Ok(new_set(i, out))
        } else {
            // Start from this Set and remove each of the other's keys.
            let mut out = vals;
            for k in set_like_keys(i, &keys, &arg(a, 0))? {
                out.retain(|v| !same_value_zero(v, &k));
            }
            Ok(new_set(i, out))
        }
    });
    it.def_method(&sp, "symmetricDifference", 1, |i, this, a| {
        let vals = set_values(i, &this)?;
        // Start from this Set; toggle each of the other's keys (remove if present, else append).
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        let mut out = vals;
        let mut seen_other: Vec<Value> = Vec::new();
        for k in set_like_keys(i, &keys, &arg(a, 0))? {
            if seen_other.iter().any(|v| same_value_zero(v, &k)) {
                continue;
            }
            seen_other.push(k.clone());
            if let Some(pos) = out.iter().position(|v| same_value_zero(v, &k)) {
                out.remove(pos);
            } else {
                out.push(k);
            }
        }
        Ok(new_set(i, out))
    });
    it.def_method(&sp, "isSubsetOf", 1, |i, this, a| {
        let vals = set_values(i, &this)?;
        let (has, _keys, other_size) = set_record(i, &arg(a, 0))?;
        // A larger set cannot be a subset; otherwise every element must be in the other.
        if (vals.len() as f64) > other_size {
            return Ok(Value::Bool(false));
        }
        for v in vals {
            if !set_like_has(i, &has, &arg(a, 0), &v)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "isSupersetOf", 1, |i, this, a| {
        let vals = set_values(i, &this)?;
        let (_has, keys, other_size) = set_record(i, &arg(a, 0))?;
        // A smaller set cannot be a superset; otherwise every other key must be in this.
        if (vals.len() as f64) < other_size {
            return Ok(Value::Bool(false));
        }
        for k in set_like_keys(i, &keys, &arg(a, 0))? {
            if !vals.iter().any(|v| same_value_zero(v, &k)) {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "isDisjointFrom", 1, |i, this, a| {
        let vals = set_values(i, &this)?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        if (vals.len() as f64) <= other_size {
            // Iterate this Set, probing the other's `has`.
            for v in vals {
                if set_like_has(i, &has, &arg(a, 0), &v)? {
                    return Ok(Value::Bool(false));
                }
            }
        } else {
            // Iterate the other's keys, probing this Set directly.
            for k in set_like_keys(i, &keys, &arg(a, 0))? {
                if vals.iter().any(|v| same_value_zero(v, &k)) {
                    return Ok(Value::Bool(false));
                }
            }
        }
        Ok(Value::Bool(true))
    });
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

fn collection_ctor(
    i: &mut Interp,
    args: &[Value],
    name: &str,
    is_set: bool,
) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Constructor requires 'new'"));
    }
    let obj = new_from_ctor(i, name)?;
    let ptr = Rc::as_ptr(&obj) as usize;
    i.map_data.insert(ptr, Vec::new());
    // Brand the instance so prototype methods can reject cross-collection receivers.
    set_internal(&obj, "__ck", Value::str(name));
    let mv = Value::Obj(obj);
    if let Some(src) = args.first() {
        if !matches!(src, Value::Undefined | Value::Null) {
            let add_fn = ab(i.get_member(&mv, if is_set { "add" } else { "set" }))?;
            if !add_fn.is_callable() {
                return Err(i.make_error("TypeError", "adder is not callable"));
            }
            // Step the source lazily: an error while processing an entry closes the iterator.
            let (iter, next) = ab(i.get_iterator(src))?;
            loop {
                let item = match step_iter_with(i, &iter, &next)? {
                    Some(v) => v,
                    None => break,
                };
                let step = if is_set {
                    i.call(add_fn.clone(), mv.clone(), &[item])
                } else if !matches!(item, Value::Obj(_)) {
                    Err(crate::interpreter::Abrupt::Throw(i.make_error(
                        "TypeError",
                        "iterator value is not an entry object",
                    )))
                } else {
                    i.get_member(&item, "0")
                        .and_then(|k| i.get_member(&item, "1").map(|v| (k, v)))
                        .and_then(|(k, v)| i.call(add_fn.clone(), mv.clone(), &[k, v]))
                };
                if let Err(e) = step {
                    i.iterator_close(&iter);
                    return Err(crate::interpreter::abrupt_value(e));
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
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = arg(a, 0);
            let e = i.map_data.entry(ptr).or_default();
            if !e.iter().any(|(k, _)| same_value_zero(k, &key)) {
                e.push((key.clone(), key));
            }
            Ok(this)
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
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
    it.def_method(
        &proto,
        if is_set { "add" } else { "set" },
        if is_set { 1 } else { 2 },
        adder,
    );
    if !is_set {
        it.def_method(&proto, "get", 1, |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = arg(a, 0);
            Ok(i.map_data
                .get(&ptr)
                .and_then(|e| {
                    e.iter()
                        .find(|(k, _)| same_value_zero(k, &key))
                        .map(|(_, v)| v.clone())
                })
                .unwrap_or(Value::Undefined))
        });
    }
    // has/delete are shared but brand-check the exact kind via kind-specific fn pointers.
    let has_fn: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = arg(a, 0);
            Ok(Value::Bool(
                i.map_data
                    .get(&ptr)
                    .map(|e| e.iter().any(|(k, _)| same_value_zero(k, &key)))
                    .unwrap_or(false),
            ))
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = arg(a, 0);
            Ok(Value::Bool(
                i.map_data
                    .get(&ptr)
                    .map(|e| e.iter().any(|(k, _)| same_value_zero(k, &key)))
                    .unwrap_or(false),
            ))
        }
    };
    it.def_method(&proto, "has", 1, has_fn);
    let delete_fn: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = arg(a, 0);
            let mut removed = false;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                let before = e.len();
                e.retain(|(k, _)| !same_value_zero(k, &key));
                removed = e.len() < before;
            }
            Ok(Value::Bool(removed))
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = arg(a, 0);
            let mut removed = false;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                let before = e.len();
                e.retain(|(k, _)| !same_value_zero(k, &key));
                removed = e.len() < before;
            }
            Ok(Value::Bool(removed))
        }
    };
    it.def_method(&proto, "delete", 1, delete_fn);
    it.def_method(&proto, "clear", 0, |i, this, _| {
        let ptr = coll_ptr(i, &this)?;
        if let Some(e) = i.map_data.get_mut(&ptr) {
            e.clear();
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
    it.def_method(&proto, "values", 0, |i, this, _| {
        collection_iter(i, &this, 0)
    });
    it.def_method(&proto, "keys", 0, |i, this, _| collection_iter(i, &this, 1));
    it.def_method(&proto, "entries", 0, |i, this, _| {
        collection_iter(i, &this, 2)
    });

    // `size` accessor.
    let size_getter = it.make_native("get size", 0, |i, this, _| {
        let ptr = coll_ptr(i, &this)?;
        Ok(Value::Num(
            i.map_data.get(&ptr).map(|e| e.len()).unwrap_or(0) as f64,
        ))
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
        let f = proto
            .borrow()
            .props
            .get(default)
            .map(|p| p.value.clone())
            .unwrap();
        proto
            .borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(f));
    }

    let ctor = it.make_native(name, 0, ctor_fn);
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
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
                let key = ab(i.call(
                    cb.clone(),
                    Value::Undefined,
                    &[el.clone(), Value::Num(idx as f64)],
                ))?;
                match groups.iter_mut().find(|(k, _)| same_value_zero(k, &key)) {
                    Some(g) => g.1.push(el),
                    None => groups.push((key, vec![el])),
                }
            }
            let m = Object::new(i.extra_protos.get("Map").cloned());
            let ptr = Rc::as_ptr(&m) as usize;
            let entries: Vec<(Value, Value)> = groups
                .into_iter()
                .map(|(k, v)| (k, i.make_array(v)))
                .collect();
            i.map_data.insert(ptr, entries);
            set_internal(&m, "__ck", Value::str("Map"));
            Ok(Value::Obj(m))
        });
    }
    install_species(it, &ctor); // Map/Set carry @@species
    set_to_string_tag(it, &proto, name);
    set_builtin(&it.global, name, Value::Obj(ctor));
}

/// WeakMap/WeakSet: like Map/Set but keys must be objects and there is no iteration/size (we do not
/// model weakness — entries simply persist, which is unobservable to non-GC tests).
/// Resolve the backing-store pointer for a weak-collection receiver, enforcing its brand: `want` is
/// the exact kind ("WeakMap"/"WeakSet") for kind-specific methods, or "Weak" to accept either for
/// the methods (has/delete) shared by both.
fn weak_brand_ptr(i: &mut Interp, this: &Value, want: &str) -> Result<usize, Value> {
    let ptr = map_ptr(this)
        .filter(|p| i.map_data.contains_key(p))
        .ok_or_else(|| i.make_error("TypeError", "method called on incompatible receiver"))?;
    let kind = this
        .as_obj()
        .and_then(|o| o.borrow().props.get("__ck").map(|p| p.value.clone()));
    let ok = match &kind {
        Some(Value::Str(s)) if want == "Weak" => s.starts_with("Weak"),
        Some(Value::Str(s)) => &**s == want,
        _ => false,
    };
    if !ok {
        return Err(i.make_error("TypeError", "method called on incompatible receiver"));
    }
    Ok(ptr)
}

fn install_weak(it: &mut Interp, name: &'static str, is_set: bool, ctor_fn: NativeFn) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());
    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakSet")?;
            let key = arg(a, 0);
            if !can_be_held_weakly(i, &key) {
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
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let (key, val) = (arg(a, 0), arg(a, 1));
            if !can_be_held_weakly(i, &key) {
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
    it.def_method(
        &proto,
        if is_set { "add" } else { "set" },
        if is_set { 1 } else { 2 },
        adder,
    );
    if !is_set {
        it.def_method(&proto, "get", 1, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let key = arg(a, 0);
            Ok(i.map_data
                .get(&ptr)
                .and_then(|e| {
                    e.iter()
                        .find(|(k, _)| same_value_zero(k, &key))
                        .map(|(_, v)| v.clone())
                })
                .unwrap_or(Value::Undefined))
        });
        // Upsert proposal: getOrInsert(key, value) / getOrInsertComputed(key, callbackfn).
        it.def_method(&proto, "getOrInsert", 2, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let key = arg(a, 0);
            if !can_be_held_weakly(i, &key) {
                return Err(i.make_error("TypeError", "Invalid value used as weak map key"));
            }
            if let Some((_, v)) = i.map_data[&ptr]
                .iter()
                .find(|(k, _)| same_value_zero(k, &key))
            {
                return Ok(v.clone());
            }
            let value = arg(a, 1);
            i.map_data
                .entry(ptr)
                .or_default()
                .push((key, value.clone()));
            Ok(value)
        });
        it.def_method(&proto, "getOrInsertComputed", 2, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let key = arg(a, 0);
            if !can_be_held_weakly(i, &key) {
                return Err(i.make_error("TypeError", "Invalid value used as weak map key"));
            }
            let cb = arg(a, 1);
            if !cb.is_callable() {
                return Err(i.make_error("TypeError", "callback is not callable"));
            }
            if let Some((_, v)) = i.map_data[&ptr]
                .iter()
                .find(|(k, _)| same_value_zero(k, &key))
            {
                return Ok(v.clone());
            }
            let value = ab(i.call(cb, Value::Undefined, std::slice::from_ref(&key)))?;
            // The callback may have mutated the map; re-check before inserting.
            if let Some((_, v)) = i.map_data[&ptr]
                .iter()
                .find(|(k, _)| same_value_zero(k, &key))
            {
                return Ok(v.clone());
            }
            i.map_data
                .entry(ptr)
                .or_default()
                .push((key, value.clone()));
            Ok(value)
        });
    }
    it.def_method(&proto, "has", 1, |i, this, a| {
        let ptr = weak_brand_ptr(i, &this, "Weak")?;
        let key = arg(a, 0);
        Ok(Value::Bool(
            i.map_data
                .get(&ptr)
                .map(|e| e.iter().any(|(k, _)| same_value_zero(k, &key)))
                .unwrap_or(false),
        ))
    });
    it.def_method(&proto, "delete", 1, |i, this, a| {
        let ptr = weak_brand_ptr(i, &this, "Weak")?;
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
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_to_string_tag(it, &proto, name);
    set_builtin(&it.global, name, Value::Obj(ctor));
}

fn install_reflect(it: &mut Interp) {
    let r = it.new_object();
    it.def_method(&r, "get", 2, |i, _t, a| {
        let target = arg(a, 0);
        if !matches!(target, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Reflect.get called on non-object"));
        }
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        // Reflect.get(target, key, receiver) — the receiver is the third argument.
        let receiver = if a.len() > 2 {
            arg(a, 2)
        } else {
            target.clone()
        };
        reflect_ordinary_get(i, &target, &key, &receiver)
    });
    it.def_method(&r, "set", 3, |i, _t, a| {
        let target = arg(a, 0);
        if !matches!(target, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Reflect.set called on non-object"));
        }
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        let value = arg(a, 2);
        // A proxy's [[Set]] returns the ToBoolean of its trap result; surface that as Reflect.set's
        // own boolean rather than always reporting success.
        if let Some((ptarget, handler)) = proxy_pair(i, &target) {
            if matches!(handler, Value::Null) {
                return Err(i.make_error("TypeError", "proxy is revoked"));
            }
            let trap = ab(i.get_member(&handler, "set"))?;
            if trap.is_callable() {
                let receiver = if a.len() > 3 {
                    arg(a, 3)
                } else {
                    target.clone()
                };
                let res = ab(i.call(
                    trap,
                    handler,
                    &[
                        ptarget.clone(),
                        Value::from_string(key.clone()),
                        value.clone(),
                        receiver,
                    ],
                ))?;
                let ok = i.to_boolean(&res);
                if ok {
                    ab(i.proxy_set_invariant(&ptarget, &key, &value))?;
                }
                return Ok(Value::Bool(ok));
            }
            ab(i.set_member(&ptarget, &key, value))?;
            return Ok(Value::Bool(true));
        }
        let receiver = if a.len() > 3 {
            arg(a, 3)
        } else {
            target.clone()
        };
        Ok(Value::Bool(reflect_ordinary_set(
            i, &target, &key, value, &receiver,
        )?))
    });
    it.def_method(&r, "has", 2, |i, _t, a| {
        let target = arg(a, 0);
        if !matches!(target, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Reflect.has called on non-object"));
        }
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        // Trap-aware [[HasProperty]] so a proxy's `has` trap (and its throws) are honored.
        Ok(Value::Bool(ab(i.js_has_property(&target, &key))?))
    });
    it.def_method(&r, "getOwnPropertyDescriptor", 2, |i, _t, a| {
        let o = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "Reflect.getOwnPropertyDescriptor on non-object",
                ))
            }
        };
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        if key.starts_with('#') {
            return Ok(Value::Undefined); // private-name slot is not an own property
        }
        // A proxy's [[GetOwnProperty]] goes through its getOwnPropertyDescriptor trap.
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            return proxy_gopd_value(i, &target, &handler, &key);
        }
        let prop = o.borrow().props.get(&key).cloned();
        Ok(prop
            .map(|p| descriptor_from_prop(i, p))
            .unwrap_or(Value::Undefined))
    });
    it.def_method(&r, "deleteProperty", 2, |i, _t, a| {
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        if let Value::Obj(o) = arg(a, 0) {
            if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
                return Ok(Value::Bool(ab(i.proxy_delete(target, handler, &key))?));
            }
            // A TypedArray integer index can't be deleted; a canonical-numeric non-index reports true.
            if let Some(info) = ta_info(i, &o) {
                match i.ta_index_kind(&info, &key) {
                    crate::value::TaIndex::Element(_) => return Ok(Value::Bool(false)),
                    crate::value::TaIndex::Exotic => return Ok(Value::Bool(true)),
                    crate::value::TaIndex::Ordinary => {}
                }
            }
            let configurable = o
                .borrow()
                .props
                .get(&key)
                .map(|p| p.configurable)
                .unwrap_or(true);
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
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            let keys = proxy_own_keys(i, &target, &handler)?;
            return Ok(i.make_array(keys));
        }
        // A TypedArray's integer indices come first (ascending), then string keys, then symbols.
        let mut out: Vec<Value> = if let Some(info) = ta_info(i, &o) {
            (0..i.ta_len(&info).unwrap_or(0))
                .map(|k| Value::from_string(k.to_string()))
                .collect()
        } else {
            Vec::new()
        };
        // Spec [[OwnPropertyKeys]] order: array-index keys ascending, then string keys (insertion
        // order), then symbol keys (insertion order) — exactly what `ordered_keys` produces.
        let ordered = o.borrow().props.ordered_keys();
        for k in ordered {
            if Interp::is_sym_key(&k) {
                if let Some(s) = i.sym_from_key(&k) {
                    out.push(s);
                }
            } else {
                out.push(Value::Str(k));
            }
        }
        Ok(i.make_array(out))
    });
    it.def_method(&r, "getPrototypeOf", 1, |i, _t, a| match arg(a, 0) {
        Value::Obj(o) => {
            if proxy_pair(i, &Value::Obj(o.clone())).is_some() {
                return js_get_prototype_of(i, &Value::Obj(o.clone()));
            }
            Ok(o.borrow()
                .proto
                .clone()
                .map(Value::Obj)
                .unwrap_or(Value::Null))
        }
        _ => Err(i.make_error("TypeError", "Reflect.getPrototypeOf called on non-object")),
    });
    it.def_method(&r, "setPrototypeOf", 2, |i, _t, a| {
        let obj = arg(a, 0);
        if !matches!(obj, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Reflect.setPrototypeOf called on non-object"));
        }
        let proto = arg(a, 1);
        if !matches!(proto, Value::Obj(_) | Value::Null) {
            return Err(i.make_error("TypeError", "prototype must be an object or null"));
        }
        Ok(Value::Bool(js_set_prototype_of(i, &obj, &proto)?))
    });
    it.def_method(&r, "defineProperty", 3, |i, _t, a| {
        let o = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => return Err(i.make_error("TypeError", "Reflect.defineProperty on non-object")),
        };
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            return Ok(Value::Bool(ab(proxy_define_property(
                i,
                &target,
                &handler,
                &key,
                &arg(a, 2),
            ))?));
        }
        let ok = ab(define_own_property(i, &o, &key, &arg(a, 2)))?;
        Ok(Value::Bool(ok))
    });
    it.def_method(&r, "apply", 3, |i, _t, a| {
        let args = create_list_from_array_like(i, &arg(a, 2))?;
        ab(i.call(arg(a, 0), arg(a, 1), &args))
    });
    it.def_method(&r, "construct", 2, |i, _t, a| {
        let target = arg(a, 0);
        if !is_constructor_value(&target) {
            return Err(i.make_error("TypeError", "Reflect.construct target is not a constructor"));
        }
        // The optional newTarget (3rd arg) must also be a constructor.
        if a.len() >= 3 && !is_constructor_value(&arg(a, 2)) {
            return Err(i.make_error(
                "TypeError",
                "Reflect.construct newTarget is not a constructor",
            ));
        }
        let args = create_list_from_array_like(i, &arg(a, 1))?;
        let new_target = if a.len() >= 3 {
            arg(a, 2)
        } else {
            target.clone()
        };
        ab(i.construct_nt(target, &args, new_target))
    });
    it.def_method(&r, "isExtensible", 1, |i, _t, a| {
        let obj = arg(a, 0);
        if !matches!(obj, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Reflect.isExtensible called on non-object"));
        }
        Ok(Value::Bool(js_is_extensible(i, &obj)?))
    });
    it.def_method(&r, "preventExtensions", 1, |i, _t, a| {
        let obj = arg(a, 0);
        if !matches!(obj, Value::Obj(_)) {
            return Err(i.make_error(
                "TypeError",
                "Reflect.preventExtensions called on non-object",
            ));
        }
        Ok(Value::Bool(js_prevent_extensions(i, &obj)?))
    });
    set_to_string_tag(it, &r, "Reflect");
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
    obj.borrow_mut().call = Callable::Bound {
        target: t,
        this: Value::Undefined,
        args: bound_args,
    };
    Value::Obj(obj)
}

/// The executor passed to `new C(executor)` inside NewPromiseCapability: capture the resolve/reject
/// functions (and reject a second invocation). `args = [cap, resolve, reject]`.
fn capability_executor(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let cap = arg(args, 0);
    let already = !matches!(ab(i.get_member(&cap, "__resolve"))?, Value::Undefined)
        || !matches!(ab(i.get_member(&cap, "__reject"))?, Value::Undefined);
    if already {
        return Err(i.make_error("TypeError", "promise capability executor already invoked"));
    }
    set_internal_obj(&cap, "__resolve", arg(args, 1));
    set_internal_obj(&cap, "__reject", arg(args, 2));
    Ok(Value::Undefined)
}

/// `Promise.prototype.finally` helpers: the value/reason pass-through thunks and the then/catch
/// reactions that run `onFinally` then forward the original settlement.
fn pf_return(_i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    Ok(arg(a, 0))
}
fn pf_throw(_i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    Err(arg(a, 0))
}
fn pf_then_finally(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    // bound args [onFinally, C]; called with (value)
    let (on_finally, c, value) = (arg(a, 0), arg(a, 1), arg(a, 2));
    let result = ab(i.call(on_finally, Value::Undefined, &[]))?;
    let resolve = ab(i.get_member(&c, "resolve"))?;
    let p = ab(i.call(resolve, c.clone(), &[result]))?;
    let thunk = make_bound(i, pf_return, vec![value]);
    let then = ab(i.get_member(&p, "then"))?;
    ab(i.call(then, p, &[thunk]))
}
fn pf_catch_finally(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let (on_finally, c, reason) = (arg(a, 0), arg(a, 1), arg(a, 2));
    let result = ab(i.call(on_finally, Value::Undefined, &[]))?;
    let resolve = ab(i.get_member(&c, "resolve"))?;
    let p = ab(i.call(resolve, c.clone(), &[result]))?;
    let thrower = make_bound(i, pf_throw, vec![reason]);
    let then = ab(i.get_member(&p, "then"))?;
    ab(i.call(then, p, &[thrower]))
}

/// SpeciesConstructor(O, defaultCtor): O.constructor[@@species], or the default.
fn species_constructor(i: &mut Interp, obj: &Value, default_ctor: &Value) -> Result<Value, Value> {
    let c = ab(i.get_member(obj, "constructor"))?;
    if matches!(c, Value::Undefined) {
        return Ok(default_ctor.clone());
    }
    if !matches!(c, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "constructor is not an object"));
    }
    let species = match well_known_key(i, "species") {
        Some(k) => ab(i.get_member(&c, &k))?,
        None => Value::Undefined,
    };
    if matches!(species, Value::Undefined | Value::Null) {
        return Ok(default_ctor.clone());
    }
    if !is_constructor_value(&species) {
        return Err(i.make_error("TypeError", "@@species is not a constructor"));
    }
    Ok(species)
}

/// GetPromiseResolve(C): read `C.resolve` once, requiring it to be callable.
fn get_promise_resolve(i: &mut Interp, ctor: &Value) -> Result<Value, Value> {
    let r = ab(i.get_member(ctor, "resolve"))?;
    if !r.is_callable() {
        return Err(i.make_error(
            "TypeError",
            "Promise combinator: this.resolve is not callable",
        ));
    }
    Ok(r)
}

/// NewPromiseCapability(C): `new C(executor)`, capturing the resolve/reject functions the executor is
/// called with (and validating they're callable). Returns the result promise built by `C`.
fn new_promise_capability(i: &mut Interp, ctor: &Value) -> Result<Value, Value> {
    if !ctor.is_callable() {
        return Err(i.make_error(
            "TypeError",
            "NewPromiseCapability: receiver is not a constructor",
        ));
    }
    let cap = i.new_object();
    let executor = make_bound(i, capability_executor, vec![Value::Obj(cap.clone())]);
    let promise = ab(i.construct(ctor.clone(), &[executor]))?;
    let resolve = ab(i.get_member(&Value::Obj(cap.clone()), "__resolve"))?;
    let reject = ab(i.get_member(&Value::Obj(cap.clone()), "__reject"))?;
    if !resolve.is_callable() || !reject.is_callable() {
        return Err(i.make_error("TypeError", "promise capability functions are not callable"));
    }
    Ok(promise)
}

/// `Promise.all` per-element fulfill reaction. `args = [resultPromise, index, value]`.
fn promise_all_element(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let result = arg(args, 0);
    let idx = ab(i.to_number(&arg(args, 1)))? as usize;
    let value = arg(args, 2);
    let results = ab(i.get_member(&result, "__results"))?;
    // CreateDataProperty: a direct own data property, so Array.prototype index setters aren't invoked.
    if let Value::Obj(o) = &results {
        crate::value::set_data(o, &idx.to_string(), value);
    }
    let rem_v = ab(i.get_member(&result, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&result, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        i.resolve_promise(&result, results);
    }
    Ok(Value::Undefined)
}

/// Subscribe a Promise combinator's element handlers via the resolved item's user-visible `.then`
/// (per spec). A throwing `.then` getter/call rejects the combinator's result promise. Returns
/// `false` if the combinator should bail out (already rejected).
fn combinator_then(i: &mut Interp, result: &Value, next: Value, on_f: Value, on_r: Value) -> bool {
    let then = match i.get_member(&next, "then") {
        Ok(t) => t,
        Err(e) => {
            let r = crate::interpreter::abrupt_value(e);
            i.reject_promise(result, r);
            return false;
        }
    };
    match i.call(then, next, &[on_f, on_r]) {
        Ok(_) => true,
        Err(e) => {
            let r = crate::interpreter::abrupt_value(e);
            i.reject_promise(result, r);
            false
        }
    }
}

/// Element handler for Promise.allKeyed: store `value` under its key in the result object.
fn promise_keyed_element(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let result = arg(args, 0);
    let key = ab(i.to_string(&arg(args, 1)))?;
    let value = arg(args, 2);
    let results = ab(i.get_member(&result, "__results"))?;
    if let Value::Obj(o) = &results {
        o.borrow_mut()
            .props
            .insert(&*key, Property::data(value, true, true, true));
    }
    let rem_v = ab(i.get_member(&result, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&result, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        i.resolve_promise(&result, results);
    }
    Ok(Value::Undefined)
}
fn promise_keyed_settle(i: &mut Interp, args: &[Value], fulfilled: bool) -> Result<Value, Value> {
    let result = arg(args, 0);
    let key = ab(i.to_string(&arg(args, 1)))?;
    let value = arg(args, 2);
    let status = i.new_object();
    set_data(
        &status,
        "status",
        Value::str(if fulfilled { "fulfilled" } else { "rejected" }),
    );
    set_data(&status, if fulfilled { "value" } else { "reason" }, value);
    let results = ab(i.get_member(&result, "__results"))?;
    if let Value::Obj(o) = &results {
        o.borrow_mut()
            .props
            .insert(&*key, Property::data(Value::Obj(status), true, true, true));
    }
    let rem_v = ab(i.get_member(&result, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&result, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        i.resolve_promise(&result, results);
    }
    Ok(Value::Undefined)
}
fn promise_keyed_settle_f(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    promise_keyed_settle(i, a, true)
}
fn promise_keyed_settle_r(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    promise_keyed_settle(i, a, false)
}

fn promise_settled(i: &mut Interp, args: &[Value], fulfilled: bool) -> Result<Value, Value> {
    let result = arg(args, 0);
    let idx = ab(i.to_number(&arg(args, 1)))? as usize;
    let value = arg(args, 2);
    let status = i.new_object();
    set_data(
        &status,
        "status",
        Value::str(if fulfilled { "fulfilled" } else { "rejected" }),
    );
    set_data(&status, if fulfilled { "value" } else { "reason" }, value);
    let results = ab(i.get_member(&result, "__results"))?;
    if let Value::Obj(o) = &results {
        crate::value::set_data(o, &idx.to_string(), Value::Obj(status));
    }
    let rem_v = ab(i.get_member(&result, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&result, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        i.resolve_promise(&result, results);
    }
    Ok(Value::Undefined)
}
fn promise_settled_fulfill(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    promise_settled(i, a, true)
}
fn promise_settled_reject(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    promise_settled(i, a, false)
}
fn promise_any_reject(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let result = arg(a, 0);
    let idx = ab(i.to_number(&arg(a, 1)))? as usize;
    let reason = arg(a, 2);
    let errors = ab(i.get_member(&result, "__errors"))?;
    ab(i.set_member(&errors, &idx.to_string(), reason))?;
    let rem_v = ab(i.get_member(&result, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&result, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        let agg = make_aggregate_error(i, errors)?;
        i.reject_promise(&result, agg);
    }
    Ok(Value::Undefined)
}
fn make_aggregate_error(i: &mut Interp, errors: Value) -> Result<Value, Value> {
    let ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "AggregateError"))?;
    ab(i.construct(ctor, &[errors]))
}

fn install_promise(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Promise", proto.clone());
    it.def_method(&proto, "then", 2, |i, this, a| {
        if map_ptr(&this).map(|p| i.promises.contains_key(&p)) != Some(true) {
            return Err(i.make_error(
                "TypeError",
                "Promise.prototype.then called on a non-Promise",
            ));
        }
        // The result promise is built by SpeciesConstructor(this, %Promise%) via NewPromiseCapability.
        let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "Promise"))?;
        let c = species_constructor(i, &this, &default_ctor)?;
        let result = new_promise_capability(i, &c)?;
        i.promise_then_into(&this, arg(a, 0), arg(a, 1), result.clone());
        Ok(result)
    });
    it.def_method(&proto, "catch", 1, |i, this, a| {
        // Catch is `this.then(undefined, onRejected)` through the actual then method.
        let then = ab(i.get_member(&this, "then"))?;
        ab(i.call(then, this.clone(), &[Value::Undefined, arg(a, 0)]))
    });
    it.def_method(&proto, "finally", 1, |i, this, a| {
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.prototype.finally on non-object"));
        }
        let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "Promise"))?;
        let c = species_constructor(i, &this, &default_ctor)?;
        let then = ab(i.get_member(&this, "then"))?;
        if !then.is_callable() {
            return Err(i.make_error("TypeError", "then is not callable"));
        }
        let on_finally = arg(a, 0);
        // A non-callable onFinally is passed to `then` on both paths unchanged.
        if !on_finally.is_callable() {
            return ab(i.call(then, this.clone(), &[on_finally.clone(), on_finally]));
        }
        // Otherwise wrap it so the original value/reason passes through after onFinally runs.
        let then_finally = make_bound(i, pf_then_finally, vec![on_finally.clone(), c.clone()]);
        let catch_finally = make_bound(i, pf_catch_finally, vec![on_finally, c]);
        ab(i.call(then, this.clone(), &[then_finally, catch_finally]))
    });

    let ctor = it.make_native("Promise", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "Promise constructor requires 'new'"));
        }
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
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "withResolvers", 0, |i, t, _a| {
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.withResolvers called on a non-object"));
        }
        let promise = new_promise_capability(i, &t)?;
        let resolve = i.make_resolver(&promise, true);
        let reject = i.make_resolver(&promise, false);
        let obj = i.new_object();
        set_data(&obj, "promise", promise);
        set_data(&obj, "resolve", resolve);
        set_data(&obj, "reject", reject);
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "resolve", 1, |i, t, a| {
        // `this` must be a constructor object; PromiseResolve(this, v).
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.resolve called on a non-object"));
        }
        let v = arg(a, 0);
        // A promise whose own `constructor` is `this` is returned unchanged.
        if let Value::Obj(o) = &v {
            if i.promises.contains_key(&(Rc::as_ptr(o) as usize)) {
                let c = ab(i.get_member(&v, "constructor"))?;
                if same_value(&c, &t) {
                    return Ok(v);
                }
            }
        }
        let cap = new_promise_capability(i, &t)?;
        i.resolve_promise(&cap, v);
        Ok(cap)
    });
    it.def_method(&ctor, "reject", 1, |i, t, a| {
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.reject called on a non-object"));
        }
        let cap = new_promise_capability(i, &t)?;
        i.reject_promise(&cap, arg(a, 0));
        Ok(cap)
    });
    it.def_method(&ctor, "all", 1, |i, t, a| {
        let result = match new_promise_capability(i, &t) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        // GetPromiseResolve and the iteration may throw — those reject the result promise.
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                i.reject_promise(&result, e);
                return Ok(result);
            }
        };
        let items = match i.iterate(&arg(a, 0)) {
            Ok(items) => items,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                i.reject_promise(&result, reason);
                return Ok(result);
            }
        };
        let n = items.len();
        let results = i.make_array(vec![Value::Undefined; n]);
        set_internal_obj(&result, "__results", results.clone());
        set_internal_obj(&result, "__remaining", Value::Num(n as f64));
        if n == 0 {
            i.resolve_promise(&result, results);
            return Ok(result);
        }
        for (idx, item) in items.into_iter().enumerate() {
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.reject_promise(&result, crate::interpreter::abrupt_value(e));
                    return Ok(result);
                }
            };
            let on_f = make_bound(
                i,
                promise_all_element,
                vec![result.clone(), Value::Num(idx as f64)],
            );
            let on_r = i.make_resolver(&result, false);
            if !combinator_then(i, &result, p, on_f, on_r) {
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "race", 1, |i, t, a| {
        let result = match new_promise_capability(i, &t) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                i.reject_promise(&result, e);
                return Ok(result);
            }
        };
        let items = match i.iterate(&arg(a, 0)) {
            Ok(items) => items,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                i.reject_promise(&result, reason);
                return Ok(result);
            }
        };
        for item in items {
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.reject_promise(&result, crate::interpreter::abrupt_value(e));
                    return Ok(result);
                }
            };
            let on_f = i.make_resolver(&result, true);
            let on_r = i.make_resolver(&result, false);
            if !combinator_then(i, &result, p, on_f, on_r) {
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "allSettled", 1, |i, t, a| {
        let result = match new_promise_capability(i, &t) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                i.reject_promise(&result, e);
                return Ok(result);
            }
        };
        let items = match i.iterate(&arg(a, 0)) {
            Ok(items) => items,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                i.reject_promise(&result, reason);
                return Ok(result);
            }
        };
        let n = items.len();
        let results = i.make_array(vec![Value::Undefined; n]);
        set_internal_obj(&result, "__results", results.clone());
        set_internal_obj(&result, "__remaining", Value::Num(n as f64));
        if n == 0 {
            i.resolve_promise(&result, results);
            return Ok(result);
        }
        for (idx, item) in items.into_iter().enumerate() {
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.reject_promise(&result, crate::interpreter::abrupt_value(e));
                    return Ok(result);
                }
            };
            let on_f = make_bound(
                i,
                promise_settled_fulfill,
                vec![result.clone(), Value::Num(idx as f64)],
            );
            let on_r = make_bound(
                i,
                promise_settled_reject,
                vec![result.clone(), Value::Num(idx as f64)],
            );
            if !combinator_then(i, &result, p, on_f, on_r) {
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "any", 1, |i, t, a| {
        let result = match new_promise_capability(i, &t) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                i.reject_promise(&result, e);
                return Ok(result);
            }
        };
        let items = match i.iterate(&arg(a, 0)) {
            Ok(items) => items,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                i.reject_promise(&result, reason);
                return Ok(result);
            }
        };
        let n = items.len();
        let errors = i.make_array(vec![Value::Undefined; n]);
        set_internal_obj(&result, "__errors", errors.clone());
        set_internal_obj(&result, "__remaining", Value::Num(n as f64));
        if n == 0 {
            let agg = make_aggregate_error(i, errors)?;
            i.reject_promise(&result, agg);
            return Ok(result);
        }
        for (idx, item) in items.into_iter().enumerate() {
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.reject_promise(&result, crate::interpreter::abrupt_value(e));
                    return Ok(result);
                }
            };
            let on_f = i.make_resolver(&result, true);
            let on_r = make_bound(
                i,
                promise_any_reject,
                vec![result.clone(), Value::Num(idx as f64)],
            );
            if !combinator_then(i, &result, p, on_f, on_r) {
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "allKeyed", 1, |i, t, a| {
        let result = match new_promise_capability(i, &t) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let dict = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => {
                let e = i.make_error("TypeError", "Promise.allKeyed argument must be an object");
                i.reject_promise(&result, e);
                return Ok(result);
            }
        };
        let keys: Vec<Rc<str>> = dict
            .borrow()
            .props
            .iter()
            .filter(|(_, p)| p.enumerable)
            .map(|(k, _)| k.clone())
            .collect();
        let results = i.new_object();
        results.borrow_mut().proto = None;
        set_internal(
            &result.as_obj().unwrap().clone(),
            "__results",
            Value::Obj(results.clone()),
        );
        set_internal(
            &result.as_obj().unwrap().clone(),
            "__remaining",
            Value::Num(keys.len() as f64),
        );
        if keys.is_empty() {
            i.resolve_promise(&result, Value::Obj(results));
            return Ok(result);
        }
        for k in keys {
            let item = ab(i.get_member(&Value::Obj(dict.clone()), &k))?;
            let p = promise_resolve_value(i, item);
            let on_f = make_bound(
                i,
                promise_keyed_element,
                vec![result.clone(), Value::str(&*k)],
            );
            let on_r = i.make_resolver(&result, false);
            if !combinator_then(i, &result, p, on_f, on_r) {
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "allSettledKeyed", 1, |i, t, a| {
        let result = match new_promise_capability(i, &t) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let dict = match arg(a, 0) {
            Value::Obj(o) => o,
            _ => {
                let e = i.make_error(
                    "TypeError",
                    "Promise.allSettledKeyed argument must be an object",
                );
                i.reject_promise(&result, e);
                return Ok(result);
            }
        };
        let keys: Vec<Rc<str>> = dict
            .borrow()
            .props
            .iter()
            .filter(|(_, p)| p.enumerable)
            .map(|(k, _)| k.clone())
            .collect();
        let results = i.new_object();
        results.borrow_mut().proto = None;
        set_internal(
            &result.as_obj().unwrap().clone(),
            "__results",
            Value::Obj(results.clone()),
        );
        set_internal(
            &result.as_obj().unwrap().clone(),
            "__remaining",
            Value::Num(keys.len() as f64),
        );
        if keys.is_empty() {
            i.resolve_promise(&result, Value::Obj(results));
            return Ok(result);
        }
        for k in keys {
            let item = ab(i.get_member(&Value::Obj(dict.clone()), &k))?;
            let p = promise_resolve_value(i, item);
            let on_f = make_bound(
                i,
                promise_keyed_settle_f,
                vec![result.clone(), Value::str(&*k)],
            );
            let on_r = make_bound(
                i,
                promise_keyed_settle_r,
                vec![result.clone(), Value::str(&*k)],
            );
            if !combinator_then(i, &result, p, on_f, on_r) {
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "try", 1, |i, _t, a| {
        // Promise.try(fn, ...args): call fn synchronously, settling a promise with its result/throw.
        let result = i.new_promise();
        let func = arg(a, 0);
        let rest: Vec<Value> = a.iter().skip(1).cloned().collect();
        match ab(i.call(func, Value::Undefined, &rest)) {
            Ok(v) => i.resolve_promise(&result, v),
            Err(e) => i.reject_promise(&result, e),
        }
        Ok(result)
    });
    install_species(it, &ctor);
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
        o.borrow_mut()
            .props
            .insert(key, Property::data(v, true, false, false));
    }
}

fn make_proxy(i: &mut Interp, target: Value, handler: Value) -> Result<Value, Value> {
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
}

fn revoke_proxy(i: &mut Interp, _this: Value, a: &[Value]) -> Result<Value, Value> {
    if let Value::Obj(o) = arg(a, 0) {
        let ptr = Rc::as_ptr(&o) as usize;
        // Keep the entry but null the handler, so the object stays a (revoked) Proxy and every
        // operation throws a TypeError rather than silently acting like a plain object.
        if let Some((target, _)) = i.proxies.get(&ptr).cloned() {
            i.proxies.insert(ptr, (target, Value::Null));
        }
    }
    Ok(Value::Undefined)
}

fn install_proxy(it: &mut Interp) {
    let ctor = it.make_native("Proxy", 2, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "Constructor Proxy requires 'new'"));
        }
        make_proxy(i, arg(a, 0), arg(a, 1))
    });
    ctor.borrow_mut().is_constructor = true; // Proxy is a constructor but has no `.prototype`
    it.def_method(&ctor, "revocable", 2, |i, _t, a| {
        let proxy = make_proxy(i, arg(a, 0), arg(a, 1))?;
        let revoke = make_bound(i, revoke_proxy, vec![proxy.clone()]);
        // The revocation function has own `length` (0) then `name` ("") like a built-in function.
        if let Value::Obj(ro) = &revoke {
            ro.borrow_mut().props.insert(
                "length",
                Property::data(Value::Num(0.0), false, false, true),
            );
            ro.borrow_mut()
                .props
                .insert("name", Property::data(Value::str(""), false, false, true));
        }
        let result = i.new_object();
        set_data(&result, "proxy", proxy);
        set_data(&result, "revoke", revoke);
        Ok(Value::Obj(result))
    });
    set_builtin(&it.global, "Proxy", Value::Obj(ctor));
}

fn install_json(it: &mut Interp) {
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
        set_internal(&o, "__raw_json", Value::from_string(text));
        i.freeze_object(&Value::Obj(o.clone()));
        Ok(Value::Obj(o))
    });
    it.def_method(&j, "isRawJSON", 1, |_i, _t, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Obj(o) if o.borrow().props.contains("__raw_json")),
        ))
    });
    set_to_string_tag(it, &j, "JSON");
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

/// Set(to, key, value) with Throw=true, for Object.assign: a non-writable data property or a
/// setter-less accessor (own or inherited along the chain) makes the assignment throw a TypeError.
fn assign_set(i: &mut Interp, to: &Value, key: &str, value: Value) -> Result<(), Value> {
    if let Value::Obj(o) = to {
        if proxy_pair(i, to).is_none() && ta_info(i, o).is_none() {
            let mut cur = Some(o.clone());
            while let Some(c) = cur {
                let blocking = {
                    let b = c.borrow();
                    b.props.get(key).map(|p| {
                        if p.accessor {
                            (true, p.set.is_none())
                        } else {
                            (true, !p.writable)
                        }
                    })
                };
                match blocking {
                    Some((_, true)) => {
                        return Err(i.make_error(
                            "TypeError",
                            format!("Cannot assign to read-only property '{key}'"),
                        ))
                    }
                    Some((_, false)) => break,
                    None => {
                        let next = c.borrow().proto.clone();
                        cur = next;
                    }
                }
            }
        }
    }
    ab(i.set_member(to, key, value))
}

/// OrdinaryGet(target, key, receiver): walk target's chain; a data property returns its value, an
/// accessor invokes its getter with `receiver` as `this`. Proxies on the chain fall back to the
/// receiver-less [[Get]].
fn reflect_ordinary_get(
    i: &mut Interp,
    target: &Value,
    key: &str,
    receiver: &Value,
) -> Result<Value, Value> {
    let mut current = target.clone();
    loop {
        if proxy_pair(i, &current).is_some() {
            return ab(i.get_member(&current, key));
        }
        let obj = match &current {
            Value::Obj(o) => o.clone(),
            _ => return Ok(Value::Undefined),
        };
        let own = obj.borrow().props.get(key).cloned();
        match own {
            Some(p) if p.accessor => {
                return match p.get {
                    Some(g) if g.is_callable() => ab(i.call(g, receiver.clone(), &[])),
                    _ => Ok(Value::Undefined),
                };
            }
            Some(p) => return Ok(p.value),
            None => {
                let proto = obj.borrow().proto.clone();
                match proto {
                    Some(pp) => current = Value::Obj(pp),
                    None => return Ok(Value::Undefined),
                }
            }
        }
    }
}

/// OrdinarySet(target, key, value, receiver): walks target's prototype chain to find the controlling
/// descriptor, then applies the result on `receiver` (which may differ from `target`). Returns the
/// success boolean. Proxies on the chain fall back to the receiver-less [[Set]].
fn reflect_ordinary_set(
    i: &mut Interp,
    target: &Value,
    key: &str,
    value: Value,
    receiver: &Value,
) -> Result<bool, Value> {
    let mut current = target.clone();
    loop {
        if proxy_pair(i, &current).is_some() {
            ab(i.set_member(&current, key, value))?;
            return Ok(true);
        }
        let obj = match &current {
            Value::Obj(o) => o.clone(),
            _ => return Ok(false),
        };
        let own = obj.borrow().props.get(key).cloned();
        match own {
            Some(p) if p.accessor => {
                return match p.set {
                    Some(setter) if setter.is_callable() => {
                        ab(i.call(setter, receiver.clone(), &[value]))?;
                        Ok(true)
                    }
                    _ => Ok(false),
                };
            }
            Some(p) => {
                if !p.writable {
                    return Ok(false);
                }
                return reflect_define_on_receiver(i, receiver, key, value);
            }
            None => {
                let proto = obj.borrow().proto.clone();
                match proto {
                    Some(pp) => current = Value::Obj(pp),
                    None => return reflect_define_on_receiver(i, receiver, key, value),
                }
            }
        }
    }
}

/// Apply a writable data assignment to `receiver`: update an existing writable data property, or
/// CreateDataProperty when absent (respecting extensibility); accessor/non-writable existing → false.
fn reflect_define_on_receiver(
    i: &mut Interp,
    receiver: &Value,
    key: &str,
    value: Value,
) -> Result<bool, Value> {
    if proxy_pair(i, receiver).is_some() {
        let (target, handler) = proxy_pair(i, receiver).unwrap();
        let desc = i.new_object();
        set_data(&desc, "value", value);
        set_data(&desc, "writable", Value::Bool(true));
        set_data(&desc, "enumerable", Value::Bool(true));
        set_data(&desc, "configurable", Value::Bool(true));
        return ab(proxy_define_property(
            i,
            &target,
            &handler,
            key,
            &Value::Obj(desc),
        ));
    }
    let ro = match receiver {
        Value::Obj(o) => o.clone(),
        _ => return Ok(false),
    };
    let existing = ro.borrow().props.get(key).cloned();
    match existing {
        Some(ep) if ep.accessor || !ep.writable => Ok(false),
        Some(_) => {
            ro.borrow_mut().props.get_mut(key).unwrap().value = value;
            Ok(true)
        }
        None => {
            if !ro.borrow().extensible {
                return Ok(false);
            }
            ro.borrow_mut()
                .props
                .insert(key, Property::data(value, true, true, true));
            Ok(true)
        }
    }
}

/// EnumerableOwnProperties for Object.values/entries: for each own string key (in order), do
/// [[GetOwnProperty]] then — if enumerable — [[Get]] the value, interleaved per key (matters for the
/// observable trap order on proxies). `entries` controls value vs `[key, value]` output.
fn enumerable_own_value_list(i: &mut Interp, o: &Value, entries: bool) -> Result<Value, Value> {
    let mut out = Vec::new();
    if let Some((t, h)) = proxy_pair(i, o) {
        for k in proxy_own_keys(i, &t, &h)? {
            if let Value::Str(ks) = &k {
                if proxy_key_enumerable(i, &t, &h, ks)? {
                    let v = ab(i.get_member(o, ks))?;
                    out.push(if entries {
                        i.make_array(vec![Value::Str(ks.clone()), v])
                    } else {
                        v
                    });
                }
            }
        }
    } else {
        let keys: Vec<Rc<str>> = o.as_obj().map(ordered_enum_keys).unwrap_or_default();
        for k in keys {
            let v = ab(i.get_member(o, &k))?;
            out.push(if entries {
                i.make_array(vec![Value::Str(k), v])
            } else {
                v
            });
        }
    }
    Ok(i.make_array(out))
}

/// IsArray, seeing through proxies (a Proxy whose target is an Array is itself an Array).
fn json_is_array(i: &mut Interp, v: &Value) -> Result<bool, Value> {
    if let Some((target, handler)) = proxy_pair(i, v) {
        if matches!(handler, Value::Null) {
            return Err(i.make_error("TypeError", "Cannot perform IsArray on a revoked Proxy"));
        }
        return json_is_array(i, &target);
    }
    Ok(matches!(v, Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array)))
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
        if let Some(Value::Str(raw)) = o.borrow().props.get("__raw_json").map(|p| p.value.clone()) {
            return Ok(Some(raw.to_string()));
        }
    }
    // A primitive-wrapper object is unwrapped to its primitive (Number/String/Boolean/BigInt data).
    if let Value::Obj(o) = &value {
        let exotic = o.borrow().exotic.clone();
        match exotic {
            Exotic::NumWrap(n) => value = Value::Num(n),
            Exotic::StrWrap(s) => value = Value::Str(s),
            Exotic::BoolWrap(b) => value = Value::Bool(b),
            Exotic::BigIntWrap(_) => {
                return Err(i.make_error("TypeError", "Do not know how to serialize a BigInt"))
            }
            _ => {}
        }
    }
    match &value {
        Value::Undefined | Value::Sym(_) => Ok(None),
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
            if chars[*pos] == '-' {
                *pos += 1;
            }
            while *pos < chars.len()
                && matches!(chars[*pos], '0'..='9' | '.' | 'e' | 'E' | '+' | '-')
            {
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
                        s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
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
        let bound_args = if args.is_empty() {
            Vec::new()
        } else {
            args[1..].to_vec()
        };
        // length = max(0, ToInteger(target.length) - boundArgs); name = "bound " + target.name.
        let target_len = ab(i.get_member(&this, "length"))?;
        let l = match target_len {
            Value::Num(n) if n.is_finite() => (n.trunc() as i64 - bound_args.len() as i64).max(0),
            _ => 0,
        };
        let target_name = ab(i.get_member(&this, "name"))?;
        let name = match &target_name {
            Value::Str(s) => format!("bound {s}"),
            _ => "bound ".to_string(),
        };
        let obj = Object::new(Some(i.function_proto.clone()));
        obj.borrow_mut().call = Callable::Bound {
            target,
            this: bound_this,
            args: bound_args,
        };
        obj.borrow_mut().is_constructor = true;
        obj.borrow_mut().props.insert(
            "length",
            Property::data(Value::Num(l as f64), false, false, true),
        );
        obj.borrow_mut().props.insert(
            "name",
            Property::data(Value::from_string(name), false, false, true),
        );
        Ok(Value::Obj(obj))
    });
    it.def_method(&fp, "toString", 0, |_i, _this, _args| {
        Ok(Value::str("function () { [native code] }"))
    });

    // The %ThrowTypeError% poison pill: `caller`/`arguments` accessors on Function.prototype that
    // throw on get or set (CallerArguments restriction).
    let throw_type_error = it.make_native("", 0, |i, _t, _a| {
        Err(i.make_error(
            "TypeError",
            "'caller', 'callee', and 'arguments' may not be accessed on strict mode functions",
        ))
    });
    for name in ["caller", "arguments"] {
        fp.borrow_mut().props.insert(
            name,
            Property {
                value: Value::Undefined,
                get: Some(Value::Obj(throw_type_error.clone())),
                set: Some(Value::Obj(throw_type_error.clone())),
                accessor: true,
                writable: false,
                enumerable: false,
                configurable: true,
            },
        );
    }

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
                let func = i.make_function(f, env);
                // GetPrototypeFromConstructor(new.target, %Function.prototype%): a cross-realm
                // `new other.Function()` gets other realm's Function.prototype as its [[Prototype]].
                let nt = i.new_target.clone();
                if matches!(nt, Value::Obj(_)) {
                    if let Value::Obj(p) = ab(i.get_member(&nt, "prototype"))? {
                        if let Value::Obj(fo) = &func {
                            fo.borrow_mut().proto = Some(p);
                        }
                    }
                }
                Ok(func)
            }
            _ => Err(i.make_error("SyntaxError", "Function constructor: invalid body")),
        }
    });
    // `Function.prototype` is the shared function prototype, so `f instanceof Function` holds for
    // every function (their [[Prototype]] is `function_proto`).
    ctor.borrow_mut().proto = Some(fp.clone());
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(fp.clone()), false, false, false),
    );
    fp.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Function", Value::Obj(ctor));
}

// ---------------------------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------------------------

/// TypedArray info for `o`, if it is one.
fn ta_info(i: &Interp, o: &Gc) -> Option<crate::value::TaInfo> {
    i.typed_arrays.get(&(Rc::as_ptr(o) as usize)).copied()
}

/// ToObject for an `Object.*` argument: an object is returned as-is, a primitive is boxed to its
/// wrapper object, and null/undefined throw a TypeError.
fn to_object_arg(i: &mut Interp, v: Value, method: &str) -> Result<Gc, Value> {
    match v {
        Value::Obj(o) => Ok(o),
        Value::Undefined | Value::Null => {
            Err(i.make_error("TypeError", format!("{method} called on null or undefined")))
        }
        other => match box_primitive(i, other) {
            Value::Obj(o) => Ok(o),
            _ => Err(i.make_error("TypeError", "ToObject failed")),
        },
    }
}

/// A Promise combinator (`Promise.all`/`race`/…) requires `this` to be a constructor.
/// ToObject for a generic `Array.prototype` method receiver: primitives are boxed (so the method
/// reads the inherited array-like properties), null/undefined throw.
fn arr_to_object(i: &mut Interp, this: &Value) -> Result<Gc, Value> {
    to_object_arg(i, this.clone(), "Array.prototype method")
}

/// RequireObjectCoercible for an `Array.prototype` method receiver (null/undefined → TypeError).
fn arr_require_coercible(i: &mut Interp, this: &Value) -> Result<(), Value> {
    if matches!(this, Value::Undefined | Value::Null) {
        return Err(i.make_error(
            "TypeError",
            "Array.prototype method called on null or undefined",
        ));
    }
    Ok(())
}

/// `ShadowRealm`: each instance owns a fully isolated sub-interpreter. `evaluate` runs source in it and
/// only lets primitive completion values cross back (callables are wrapped; objects are a TypeError).
fn install_shadow_realm(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("ShadowRealm", proto.clone());
    set_to_string_tag(it, &proto, "ShadowRealm");
    it.def_method(&proto, "evaluate", 1, shadow_evaluate);

    let ctor = it.make_native("ShadowRealm", 0, |i, _t, _a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "ShadowRealm constructor requires 'new'"));
        }
        let obj = Object::new(i.extra_protos.get("ShadowRealm").cloned());
        let p = Rc::as_ptr(&obj) as usize;
        i.shadow_realms.insert(p, Box::new(Interp::new()));
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.global
        .borrow_mut()
        .props
        .insert("ShadowRealm", Property::builtin(Value::Obj(ctor)));
}

fn shadow_evaluate(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    let ptr = map_ptr(&this)
        .filter(|p| i.shadow_realms.contains_key(p))
        .ok_or_else(|| {
            i.make_error(
                "TypeError",
                "ShadowRealm.prototype.evaluate called on a non-ShadowRealm",
            )
        })?;
    let src = match arg(a, 0) {
        Value::Str(s) => s.to_string(),
        _ => {
            return Err(i.make_error(
                "TypeError",
                "ShadowRealm.prototype.evaluate expects a string",
            ))
        }
    };
    // A parse failure is a SyntaxError of the *calling* realm (not wrapped).
    let body = match crate::parser::parse_script(&src, false) {
        Ok(b) => b,
        Err(e) => return Err(i.make_error("SyntaxError", e.message)),
    };
    let mut sub = i.shadow_realms.remove(&ptr).unwrap();
    let result = sub.run_body(&body, false);
    i.shadow_realms.insert(ptr, sub);
    match result {
        // Primitive values (number/string/bool/null/undefined/bigint/symbol) are self-contained and
        // cross the realm boundary directly.
        Ok(v) if !matches!(v, Value::Obj(_)) => Ok(v),
        Ok(v) if v.is_callable() => Ok(i.make_wrapped_shadow(ptr, v)),
        Ok(_) => Err(i.make_error(
            "TypeError",
            "ShadowRealm.prototype.evaluate result must be a primitive",
        )),
        // An error thrown inside the shadow realm is re-thrown as a TypeError of the calling realm.
        Err(_) => Err(i.make_error(
            "TypeError",
            "ShadowRealm evaluate: the provided source threw an error",
        )),
    }
}

/// `(info, detached)` for a TypedArray receiver, or a TypeError (brand check for the meta getters).
fn ta_receiver(i: &mut Interp, this: &Value) -> Result<(crate::value::TaInfo, bool), Value> {
    let ptr = map_ptr(this).filter(|p| i.typed_arrays.contains_key(p));
    match ptr {
        Some(p) => {
            let info = i.typed_arrays[&p];
            Ok((info, !i.array_buffers.contains_key(&info.buffer)))
        }
        None => Err(i.make_error(
            "TypeError",
            "TypedArray.prototype accessor called on a non-TypedArray",
        )),
    }
}
fn ta_length_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (info, _) = ta_receiver(i, &this)?;
    Ok(Value::Num(i.ta_len(&info).unwrap_or(0) as f64))
}
fn ta_bytelength_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (info, _) = ta_receiver(i, &this)?;
    Ok(Value::Num(
        (i.ta_len(&info).unwrap_or(0) * info.kind.elsize()) as f64,
    ))
}
fn ta_byteoffset_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (info, _) = ta_receiver(i, &this)?;
    Ok(Value::Num(if i.ta_len(&info).is_none() {
        0.0
    } else {
        info.offset as f64
    }))
}
fn ta_buffer_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let _ = ta_receiver(i, &this)?;
    let ptr = map_ptr(&this).unwrap();
    Ok(i.ta_buffer.get(&ptr).cloned().unwrap_or(Value::Undefined))
}
const TA_META_KEYS: [&str; 5] = [
    "length",
    "byteLength",
    "byteOffset",
    "buffer",
    "BYTES_PER_ELEMENT",
];

/// If `v` is a Proxy, its (target, handler) pair.
fn proxy_pair(i: &Interp, v: &Value) -> Option<(Value, Value)> {
    if let Value::Obj(o) = v {
        i.proxies.get(&(Rc::as_ptr(o) as usize)).cloned()
    } else {
        None
    }
}
/// Proxy `[[GetPrototypeOf]]`: call the trap or forward to the target.
/// `[[IsExtensible]]`, proxy-aware (recurses into a proxy target, enforcing the trap invariant).
fn js_is_extensible(i: &mut Interp, obj: &Value) -> Result<bool, Value> {
    if let Some((target, handler)) = proxy_pair(i, obj) {
        if matches!(handler, Value::Null) {
            return Err(i.make_error("TypeError", "proxy is revoked"));
        }
        let trap = ab(i.get_member(&handler, "isExtensible"))?;
        if matches!(trap, Value::Undefined | Value::Null) {
            return js_is_extensible(i, &target);
        }
        if !trap.is_callable() {
            return Err(i.make_error("TypeError", "proxy 'isExtensible' trap is not callable"));
        }
        let res = ab(i.call(trap, handler, std::slice::from_ref(&target)))?;
        let result = i.to_boolean(&res);
        if result != js_is_extensible(i, &target)? {
            return Err(i.make_error("TypeError", "proxy 'isExtensible' must match the target"));
        }
        return Ok(result);
    }
    Ok(matches!(obj, Value::Obj(o) if o.borrow().extensible))
}

/// `[[GetPrototypeOf]]`, proxy-aware.
pub(crate) fn js_get_prototype_of(i: &mut Interp, obj: &Value) -> Result<Value, Value> {
    if let Some((target, handler)) = proxy_pair(i, obj) {
        if matches!(handler, Value::Null) {
            return Err(i.make_error("TypeError", "proxy is revoked"));
        }
        return proxy_get_prototype(i, &target, &handler);
    }
    Ok(match obj {
        Value::Obj(o) => o
            .borrow()
            .proto
            .clone()
            .map(Value::Obj)
            .unwrap_or(Value::Null),
        _ => Value::Null,
    })
}

/// `[[SetPrototypeOf]]`, proxy-aware. Returns whether the operation succeeded (Reflect-style).
fn js_set_prototype_of(i: &mut Interp, obj: &Value, proto: &Value) -> Result<bool, Value> {
    let o = match obj {
        Value::Obj(o) => o.clone(),
        _ => return Ok(true),
    };
    if let Some((target, handler)) = proxy_pair(i, obj) {
        if matches!(handler, Value::Null) {
            return Err(i.make_error("TypeError", "proxy is revoked"));
        }
        let trap = ab(i.get_member(&handler, "setPrototypeOf"))?;
        if matches!(trap, Value::Undefined | Value::Null) {
            return js_set_prototype_of(i, &target, proto);
        }
        if !trap.is_callable() {
            return Err(i.make_error("TypeError", "proxy 'setPrototypeOf' trap is not callable"));
        }
        let res = ab(i.call(trap, handler, &[target.clone(), proto.clone()]))?;
        if !i.to_boolean(&res) {
            return Ok(false);
        }
        // Invariant: a non-extensible target's prototype can't be changed.
        if !js_is_extensible(i, &target)? {
            let cur = js_get_prototype_of(i, &target)?;
            if !i.strict_equals(proto, &cur) {
                return Err(i.make_error(
                    "TypeError",
                    "proxy 'setPrototypeOf' changed a non-extensible target's prototype",
                ));
            }
        }
        return Ok(true);
    }
    // Ordinary [[SetPrototypeOf]].
    let cur = o
        .borrow()
        .proto
        .clone()
        .map(Value::Obj)
        .unwrap_or(Value::Null);
    if i.strict_equals(proto, &cur) {
        return Ok(true);
    }
    if !o.borrow().extensible {
        return Ok(false);
    }
    // Cycle check: refuse if `o` already lies on the candidate prototype's chain (walking stops at
    // any proxy, whose [[GetPrototypeOf]] may be non-deterministic).
    let mut p = proto.clone();
    while let Value::Obj(po) = &p {
        if Rc::ptr_eq(po, &o) {
            return Ok(false);
        }
        if proxy_pair(i, &p).is_some() {
            break;
        }
        let next = po.borrow().proto.clone();
        p = match next {
            Some(n) => Value::Obj(n),
            None => break,
        };
    }
    o.borrow_mut().proto = match proto {
        Value::Obj(p) => Some(p.clone()),
        _ => None,
    };
    Ok(true)
}

/// `[[PreventExtensions]]`, proxy-aware. Returns whether it succeeded.
fn js_prevent_extensions(i: &mut Interp, obj: &Value) -> Result<bool, Value> {
    if let Some((target, handler)) = proxy_pair(i, obj) {
        if matches!(handler, Value::Null) {
            return Err(i.make_error("TypeError", "proxy is revoked"));
        }
        let trap = ab(i.get_member(&handler, "preventExtensions"))?;
        if matches!(trap, Value::Undefined | Value::Null) {
            return js_prevent_extensions(i, &target);
        }
        if !trap.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "proxy 'preventExtensions' trap is not callable",
            ));
        }
        let res = ab(i.call(trap, handler, std::slice::from_ref(&target)))?;
        let ok = i.to_boolean(&res);
        // Invariant: a true result requires the target to actually be non-extensible.
        if ok && js_is_extensible(i, &target)? {
            return Err(i.make_error(
                "TypeError",
                "proxy 'preventExtensions' reported success but the target is extensible",
            ));
        }
        return Ok(ok);
    }
    if let Value::Obj(o) = obj {
        o.borrow_mut().extensible = false;
    }
    Ok(true)
}

fn proxy_get_prototype(i: &mut Interp, target: &Value, handler: &Value) -> Result<Value, Value> {
    let trap = ab(i.get_member(handler, "getPrototypeOf"))?;
    if matches!(trap, Value::Undefined | Value::Null) {
        // Forward to the target's [[GetPrototypeOf]] (recursing for a proxy target).
        return js_get_prototype_of(i, target);
    }
    if !trap.is_callable() {
        return Err(i.make_error("TypeError", "proxy 'getPrototypeOf' trap is not callable"));
    }
    if trap.is_callable() {
        let res = ab(i.call(trap, handler.clone(), std::slice::from_ref(target)))?;
        if !matches!(res, Value::Obj(_) | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "getPrototypeOf trap must return an object or null",
            ));
        }
        // Invariant: for a non-extensible target the reported prototype must be the real one.
        if let Value::Obj(t) = target {
            if !t.borrow().extensible {
                let actual = t
                    .borrow()
                    .proto
                    .clone()
                    .map(Value::Obj)
                    .unwrap_or(Value::Null);
                if !i.strict_equals(&res, &actual) {
                    return Err(i.make_error(
                        "TypeError",
                        "getPrototypeOf trap result differs from a non-extensible target's prototype",
                    ));
                }
            }
        }
        Ok(res)
    } else if let Value::Obj(t) = target {
        Ok(t.borrow()
            .proto
            .clone()
            .map(Value::Obj)
            .unwrap_or(Value::Null))
    } else {
        Ok(Value::Null)
    }
}
/// Proxy `[[OwnPropertyKeys]]`: the trap result (must be a list of strings/symbols) or the target's
/// own keys.
fn proxy_own_keys(i: &mut Interp, target: &Value, handler: &Value) -> Result<Vec<Value>, Value> {
    let trap = ab(i.get_member(handler, "ownKeys"))?;
    if matches!(trap, Value::Undefined | Value::Null) {
        // Forward to the target's [[OwnPropertyKeys]] (recursing for a proxy target).
        if let Some((t2, h2)) = proxy_pair(i, target) {
            return proxy_own_keys(i, &t2, &h2);
        }
        return Ok(match target {
            Value::Obj(t) => t
                .borrow()
                .props
                .keys()
                .into_iter()
                .map(Value::Str)
                .collect(),
            _ => Vec::new(),
        });
    }
    if !trap.is_callable() {
        return Err(i.make_error("TypeError", "proxy 'ownKeys' trap is not callable"));
    }
    if trap.is_callable() {
        let res = ab(i.call(trap, handler.clone(), std::slice::from_ref(target)))?;
        if !matches!(res, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "ownKeys trap must return an array-like object"));
        }
        let keys = ab(i.create_list_from_arraylike(&res))?;
        let mut key_strs: Vec<String> = Vec::with_capacity(keys.len());
        for k in &keys {
            if !matches!(k, Value::Str(_) | Value::Sym(_)) {
                return Err(i.make_error(
                    "TypeError",
                    "ownKeys trap result must contain only strings and symbols",
                ));
            }
            key_strs.push(ab(i.to_property_key(k))?);
        }
        // No duplicate keys.
        let result_set: std::collections::HashSet<&str> =
            key_strs.iter().map(|s| s.as_str()).collect();
        if result_set.len() != key_strs.len() {
            return Err(i.make_error("TypeError", "ownKeys trap result has duplicate keys"));
        }
        // Invariants relative to the target's own keys / extensibility.
        if let Value::Obj(t) = target {
            let extensible = t.borrow().extensible;
            let target_keys: Vec<(String, bool)> = t
                .borrow()
                .props
                .keys()
                .into_iter()
                .map(|k| {
                    let conf = t
                        .borrow()
                        .props
                        .get(&k)
                        .map(|p| p.configurable)
                        .unwrap_or(true);
                    (k.to_string(), conf)
                })
                .collect();
            for (tk, conf) in &target_keys {
                if !conf && !result_set.contains(tk.as_str()) {
                    return Err(i.make_error(
                        "TypeError",
                        "ownKeys trap omitted a non-configurable target key",
                    ));
                }
            }
            if !extensible {
                for (tk, _) in &target_keys {
                    if !result_set.contains(tk.as_str()) {
                        return Err(i.make_error(
                            "TypeError",
                            "ownKeys trap omitted a key of a non-extensible target",
                        ));
                    }
                }
                if key_strs.len() != target_keys.len() {
                    return Err(i.make_error(
                        "TypeError",
                        "ownKeys trap added a key to a non-extensible target",
                    ));
                }
            }
        }
        Ok(keys)
    } else if let Value::Obj(t) = target {
        Ok(t.borrow()
            .props
            .keys()
            .into_iter()
            .map(Value::Str)
            .collect())
    } else {
        Ok(Vec::new())
    }
}

/// Proxy `[[DefineOwnProperty]]`: call the trap (ToBoolean its result) or forward to the target.
/// Proxy `[[GetOwnProperty]]`: the trap result as a descriptor object (or undefined), enforcing the
/// absent-property invariant; a missing trap forwards to the target (recursing for a proxy target).
fn proxy_gopd_value(
    i: &mut Interp,
    target: &Value,
    handler: &Value,
    key: &str,
) -> Result<Value, Value> {
    if matches!(handler, Value::Null) {
        return Err(i.make_error("TypeError", "proxy is revoked"));
    }
    let trap = ab(i.get_member(handler, "getOwnPropertyDescriptor"))?;
    if matches!(trap, Value::Undefined | Value::Null) {
        if let Some((t2, h2)) = proxy_pair(i, target) {
            return proxy_gopd_value(i, &t2, &h2, key);
        }
        if let Value::Obj(t) = target {
            let prop = t.borrow().props.get(key).cloned();
            return Ok(prop
                .map(|p| descriptor_from_prop(i, p))
                .unwrap_or(Value::Undefined));
        }
        return Ok(Value::Undefined);
    }
    if !trap.is_callable() {
        return Err(i.make_error(
            "TypeError",
            "proxy 'getOwnPropertyDescriptor' trap is not callable",
        ));
    }
    let key_val = i
        .sym_from_key(key)
        .unwrap_or_else(|| Value::from_string(key.to_string()));
    let res = ab(i.call(trap, handler.clone(), &[target.clone(), key_val]))?;
    if matches!(res, Value::Undefined) {
        // The trap may report a property absent only if the target permits it.
        if let Value::Obj(t) = target {
            let tprop = t.borrow().props.get(key).cloned();
            if let Some(p) = tprop {
                if !p.configurable {
                    return Err(i.make_error(
                        "TypeError",
                        "gOPD trap reported undefined for a non-configurable property",
                    ));
                }
                if !t.borrow().extensible {
                    return Err(i.make_error(
                        "TypeError",
                        "gOPD trap reported undefined for a non-extensible target's property",
                    ));
                }
            }
        }
        return Ok(Value::Undefined);
    }
    if !matches!(res, Value::Obj(_)) {
        return Err(i.make_error(
            "TypeError",
            "getOwnPropertyDescriptor trap must return an object or undefined",
        ));
    }
    let pd = ab(build_partial(i, &res))?;
    // A reported non-configurable descriptor must be backed by the target.
    if matches!(pd.configurable, Some(false)) {
        if let Value::Obj(t) = target {
            let tprop = t.borrow().props.get(key).cloned();
            match tprop {
                None => {
                    return Err(i.make_error(
                        "TypeError",
                        "gOPD trap reported a non-configurable descriptor the target lacks",
                    ));
                }
                Some(p) => {
                    if p.configurable {
                        return Err(i.make_error(
                            "TypeError",
                            "gOPD trap reported non-configurable for a configurable target property",
                        ));
                    }
                    if matches!(pd.writable, Some(false)) && !p.accessor && p.writable {
                        return Err(i.make_error(
                            "TypeError",
                            "gOPD trap reported non-writable for a writable target property",
                        ));
                    }
                }
            }
        }
    }
    Ok(descriptor_from_prop(i, complete_descriptor(pd)))
}

fn proxy_define_property(
    i: &mut Interp,
    target: &Value,
    handler: &Value,
    key: &str,
    desc: &Value,
) -> Result<bool, Abrupt> {
    let trap = i.get_member(handler, "defineProperty")?;
    if matches!(trap, Value::Undefined | Value::Null) {
        if let Some((t2, h2)) = proxy_pair(i, target) {
            return proxy_define_property(i, &t2, &h2, key, desc);
        }
        return if let Value::Obj(t) = target {
            define_own_property(i, t, key, desc)
        } else {
            Ok(false)
        };
    }
    if !trap.is_callable() {
        return Err(i.throw("TypeError", "proxy 'defineProperty' trap is not callable"));
    }
    let key_val = i
        .sym_from_key(key)
        .unwrap_or_else(|| Value::from_string(key.to_string()));
    let res = i.call(
        trap,
        handler.clone(),
        &[target.clone(), key_val, desc.clone()],
    )?;
    if !i.to_boolean(&res) {
        return Ok(false);
    }
    // Invariants relative to the target's existing property and extensibility.
    let pd = build_partial(i, desc)?;
    let setting_config_false = matches!(pd.configurable, Some(false));
    if let Value::Obj(t) = target {
        let tprop = t.borrow().props.get(key).cloned();
        let extensible = t.borrow().extensible;
        match tprop {
            None => {
                if !extensible {
                    return Err(i.throw(
                        "TypeError",
                        "proxy 'defineProperty' added a property to a non-extensible target",
                    ));
                }
                if setting_config_false {
                    return Err(i.throw(
                        "TypeError",
                        "proxy 'defineProperty' defined a non-configurable property the target lacks",
                    ));
                }
            }
            Some(p) => {
                if setting_config_false && p.configurable {
                    return Err(i.throw(
                        "TypeError",
                        "proxy 'defineProperty' made a configurable target property non-configurable",
                    ));
                }
                if !p.configurable && !p.accessor && !p.writable {
                    if matches!(pd.writable, Some(true)) {
                        return Err(i.throw(
                            "TypeError",
                            "proxy 'defineProperty' made a non-writable property writable",
                        ));
                    }
                    // A non-configurable, non-writable data property's value can't be changed.
                    if let Some(v) = &pd.value {
                        if !i.strict_equals(v, &p.value) {
                            return Err(i.throw(
                                "TypeError",
                                "proxy 'defineProperty' changed a non-configurable non-writable value",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(true)
}

/// A proxy's own enumerable string keys (for Object.keys/values/entries): the ownKeys trap result
/// filtered by each key's [[GetOwnProperty]] enumerable flag.
fn proxy_enum_string_keys(i: &mut Interp, proxy: &Value) -> Result<Vec<Value>, Value> {
    let (target, handler) = proxy_pair(i, proxy).unwrap();
    let keys = proxy_own_keys(i, &target, &handler)?;
    let mut out = Vec::new();
    for k in keys {
        if let Value::Str(ks) = &k {
            if proxy_key_enumerable(i, &target, &handler, ks)? {
                out.push(k);
            }
        }
    }
    Ok(out)
}
fn proxy_key_enumerable(
    i: &mut Interp,
    target: &Value,
    handler: &Value,
    key: &str,
) -> Result<bool, Value> {
    let trap = ab(i.get_member(handler, "getOwnPropertyDescriptor"))?;
    if trap.is_callable() {
        let kv = Value::from_string(key.to_string());
        let res = ab(i.call(trap, handler.clone(), &[target.clone(), kv]))?;
        if matches!(res, Value::Undefined) {
            return Ok(false);
        }
        let enum_v = ab(i.get_member(&res, "enumerable"))?;
        Ok(i.to_boolean(&enum_v))
    } else if let Value::Obj(t) = target {
        Ok(t.borrow()
            .props
            .get(key)
            .map(|p| p.enumerable)
            .unwrap_or(false))
    } else {
        Ok(false)
    }
}

fn install_object(it: &mut Interp) {
    let op = it.object_proto.clone();
    it.def_method(&op, "hasOwnProperty", 1, |i, this, args| {
        let key = ab(i.to_property_key(&arg(args, 0)))?;
        // A private-name slot (`#x`) is never an observable own property.
        if key.starts_with('#') {
            return Ok(Value::Bool(false));
        }
        let o = match this_obj(&this) {
            Some(o) => o,
            None => return Ok(Value::Bool(false)),
        };
        // A TypedArray index in range is an own property even though it isn't in the property map;
        // a canonical-numeric non-index is never an own property.
        if let Some(info) = ta_info(i, &o) {
            match i.ta_index_kind(&info, &key) {
                crate::value::TaIndex::Element(_) => return Ok(Value::Bool(true)),
                crate::value::TaIndex::Exotic => return Ok(Value::Bool(false)),
                crate::value::TaIndex::Ordinary => {}
            }
        }
        let has = o.borrow().props.contains(&key);
        Ok(Value::Bool(has))
    });
    // Annex B __defineGetter__/__defineSetter__/__lookupGetter__/__lookupSetter__.
    fn define_accessor(
        i: &mut Interp,
        this: &Value,
        args: &[Value],
        is_get: bool,
    ) -> Result<Value, Value> {
        let o = this_obj(this).ok_or_else(|| i.make_error("TypeError", "called on non-object"))?;
        let f = arg(args, 1);
        if !f.is_callable() {
            return Err(i.make_error("TypeError", "accessor must be a function"));
        }
        let key = ab(i.to_property_key(&arg(args, 0)))?;
        let mut existing = o
            .borrow()
            .props
            .get(&key)
            .cloned()
            .filter(|p| p.accessor)
            .unwrap_or(Property {
                value: Value::Undefined,
                get: None,
                set: None,
                accessor: true,
                writable: false,
                enumerable: true,
                configurable: true,
            });
        if is_get {
            existing.get = Some(f);
        } else {
            existing.set = Some(f);
        }
        o.borrow_mut().props.insert(key, existing);
        Ok(Value::Undefined)
    }
    fn lookup_accessor(
        i: &mut Interp,
        this: &Value,
        args: &[Value],
        is_get: bool,
    ) -> Result<Value, Value> {
        let mut cur = this_obj(this);
        let key = ab(i.to_property_key(&arg(args, 0)))?;
        while let Some(o) = cur {
            if let Some(p) = o.borrow().props.get(&key) {
                if p.accessor {
                    let f = if is_get { p.get.clone() } else { p.set.clone() };
                    return Ok(f.unwrap_or(Value::Undefined));
                }
                return Ok(Value::Undefined);
            }
            cur = o.borrow().proto.clone();
        }
        Ok(Value::Undefined)
    }
    it.def_method(&op, "__defineGetter__", 2, |i, this, a| {
        define_accessor(i, &this, a, true)
    });
    it.def_method(&op, "__defineSetter__", 2, |i, this, a| {
        define_accessor(i, &this, a, false)
    });
    it.def_method(&op, "__lookupGetter__", 1, |i, this, a| {
        lookup_accessor(i, &this, a, true)
    });
    it.def_method(&op, "__lookupSetter__", 1, |i, this, a| {
        lookup_accessor(i, &this, a, false)
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
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(op.clone()), false, false, false),
    );
    op.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));

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
            let key_v = ab(i.call(
                cb.clone(),
                Value::Undefined,
                &[el.clone(), Value::Num(idx as f64)],
            ))?;
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
        let o = to_object_arg(i, arg(args, 0), "Object.keys")?;
        if proxy_pair(i, &Value::Obj(o.clone())).is_some() {
            let keys = proxy_enum_string_keys(i, &Value::Obj(o.clone()))?;
            return Ok(i.make_array(keys));
        }
        let keys: Vec<Value> = ordered_enum_keys(&o).into_iter().map(Value::Str).collect();
        Ok(i.make_array(keys))
    });
    it.def_method(&ctor, "getOwnPropertyNames", 1, |i, _this, args| {
        let o = to_object_arg(i, arg(args, 0), "Object.getOwnPropertyNames")?;
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            let keys = proxy_own_keys(i, &target, &handler)?;
            let strs: Vec<Value> = keys
                .into_iter()
                .filter(|k| matches!(k, Value::Str(_)))
                .collect();
            return Ok(i.make_array(strs));
        }
        // A TypedArray's own keys are its integer indices, then any string expandos (the
        // length/buffer/... metadata are inherited, not own).
        if let Some(info) = ta_info(i, &o) {
            let n = i.ta_len(&info).unwrap_or(0);
            let mut keys: Vec<Value> = (0..n).map(|k| Value::from_string(k.to_string())).collect();
            for k in o.borrow().props.keys() {
                if !Interp::is_sym_key(&k)
                    && k.parse::<usize>().is_err()
                    && !TA_META_KEYS.contains(&&*k)
                {
                    keys.push(Value::Str(k));
                }
            }
            return Ok(i.make_array(keys));
        }
        // Spec order: array-index keys ascending, then other string keys in insertion order.
        let keys: Vec<Value> = o
            .borrow()
            .props
            .ordered_keys()
            .into_iter()
            .filter(|k| !Interp::is_sym_key(k) && !k.starts_with('#'))
            .map(Value::Str)
            .collect();
        Ok(i.make_array(keys))
    });
    it.def_method(&ctor, "getOwnPropertySymbols", 1, |i, _this, args| {
        // ToObject coerces primitives (and throws for null/undefined).
        let o = to_object_arg(i, arg(args, 0), "Object.getOwnPropertySymbols")?;
        let ov = Value::Obj(o.clone());
        // A proxy's symbol keys come from its [[OwnPropertyKeys]] trap.
        if let Some((target, handler)) = proxy_pair(i, &ov) {
            let syms: Vec<Value> = proxy_own_keys(i, &target, &handler)?
                .into_iter()
                .filter(|k| matches!(k, Value::Sym(_)))
                .collect();
            return Ok(i.make_array(syms));
        }
        let syms: Vec<Value> = o
            .borrow()
            .props
            .ordered_keys()
            .into_iter()
            .filter(|k| Interp::is_sym_key(k))
            .filter_map(|k| i.sym_from_key(&k))
            .collect();
        Ok(i.make_array(syms))
    });
    it.def_method(&ctor, "getPrototypeOf", 1, |i, _this, args| {
        match arg(args, 0) {
            Value::Obj(o) => {
                if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
                    return proxy_get_prototype(i, &target, &handler);
                }
                Ok(o.borrow()
                    .proto
                    .clone()
                    .map(Value::Obj)
                    .unwrap_or(Value::Null))
            }
            // ToObject coerces a primitive (Object.getPrototypeOf('') → String.prototype).
            Value::Undefined | Value::Null => {
                Err(i.make_error("TypeError", "called on null or undefined"))
            }
            Value::Str(_) => Ok(Value::Obj(i.string_proto.clone())),
            Value::Num(_) => Ok(Value::Obj(i.number_proto.clone())),
            Value::Bool(_) => Ok(Value::Obj(i.boolean_proto.clone())),
            Value::Sym(_) => Ok(Value::Obj(i.symbol_proto.clone())),
            Value::BigInt(_) => Ok(i
                .extra_protos
                .get("BigInt")
                .cloned()
                .map(Value::Obj)
                .unwrap_or(Value::Null)),
        }
    });
    it.def_method(&ctor, "setPrototypeOf", 2, |i, _this, args| {
        let proto = arg(args, 1);
        if !matches!(proto, Value::Obj(_) | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "Object prototype may only be an Object or null",
            ));
        }
        if matches!(arg(args, 0), Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "Object.setPrototypeOf called on null or undefined",
            ));
        }
        let obj = arg(args, 0);
        // Object.setPrototypeOf throws if [[SetPrototypeOf]] returns false.
        if !js_set_prototype_of(i, &obj, &proto)? {
            return Err(i.make_error("TypeError", "could not set prototype"));
        }
        Ok(obj)
    });
    it.def_method(&ctor, "create", 2, |i, _this, args| {
        let proto = match arg(args, 0) {
            Value::Obj(o) => Some(o),
            Value::Null => None,
            _ => {
                return Err(i.make_error("TypeError", "Object.create proto must be object or null"))
            }
        };
        let obj = Object::new(proto);
        // Object.create(O, Properties): when Properties is not undefined, ObjectDefineProperties.
        let props_arg = arg(args, 1);
        if !matches!(props_arg, Value::Undefined) {
            object_define_properties(i, &Value::Obj(obj.clone()), props_arg, "Object.create")?;
        }
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "defineProperty", 3, |i, _this, args| {
        let o = match arg(args, 0) {
            Value::Obj(o) => o,
            _ => {
                return Err(i.make_error("TypeError", "Object.defineProperty called on non-object"))
            }
        };
        let key = ab(i.to_property_key(&arg(args, 1)))?;
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            if !ab(proxy_define_property(
                i,
                &target,
                &handler,
                &key,
                &arg(args, 2),
            ))? {
                return Err(
                    i.make_error("TypeError", "proxy defineProperty returned a falsish value")
                );
            }
            return Ok(Value::Obj(o));
        }
        if !ab(define_own_property(i, &o, &key, &arg(args, 2)))? {
            return Err(i.make_error("TypeError", "Cannot redefine property"));
        }
        Ok(Value::Obj(o))
    });
    it.def_method(&ctor, "getOwnPropertyDescriptor", 2, |i, _this, args| {
        // ToObject coerces a primitive target (and throws for null/undefined).
        let o = to_object_arg(i, arg(args, 0), "Object.getOwnPropertyDescriptor")?;
        let key = ab(i.to_property_key(&arg(args, 1)))?;
        if key.starts_with('#') {
            return Ok(Value::Undefined); // private-name slot is not an own property
        }
        // A TypedArray integer index is an own data property reading from the buffer.
        if let Some(info) = ta_info(i, &o) {
            if let Ok(idx) = key.parse::<usize>() {
                if idx >= i.ta_len(&info).unwrap_or(0) {
                    return Ok(Value::Undefined);
                }
                let val = i.ta_read(&info, idx);
                return Ok(descriptor_from_prop(
                    i,
                    Property::data(val, true, true, true),
                ));
            }
        }
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            return proxy_gopd_value(i, &target, &handler, &key);
        }
        let prop = o.borrow().props.get(&key).cloned();
        match prop {
            None => Ok(Value::Undefined),
            Some(p) => Ok(descriptor_from_prop(i, p)),
        }
    });
    it.def_method(&ctor, "getOwnPropertyDescriptors", 1, |i, _this, args| {
        // ToObject coerces primitives (and throws for null/undefined).
        let o = to_object_arg(i, arg(args, 0), "Object.getOwnPropertyDescriptors")?;
        let ov = Value::Obj(o.clone());
        let result = i.new_object();
        // A proxy goes through its [[OwnPropertyKeys]]/[[GetOwnProperty]] traps, in order.
        if let Some((target, handler)) = proxy_pair(i, &ov) {
            for k in proxy_own_keys(i, &target, &handler)? {
                let key = ab(i.to_property_key(&k))?;
                let desc = proxy_gopd_value(i, &target, &handler, &key)?;
                if !matches!(desc, Value::Undefined) {
                    result
                        .borrow_mut()
                        .props
                        .insert(key.as_str(), Property::plain(desc));
                }
            }
            return Ok(Value::Obj(result));
        }
        for key in o.borrow().props.ordered_keys() {
            if key.starts_with('#') {
                continue;
            }
            let prop = o.borrow().props.get(&key).cloned();
            if let Some(p) = prop {
                let d = descriptor_from_prop(i, p);
                result
                    .borrow_mut()
                    .props
                    .insert(key.as_ref(), Property::plain(d));
            }
        }
        Ok(Value::Obj(result))
    });
    it.def_method(&ctor, "freeze", 1, |i, _this, args| {
        let o = arg(args, 0);
        if matches!(o, Value::Obj(_)) && !set_integrity_level(i, &o, true)? {
            return Err(i.make_error("TypeError", "Object.freeze could not freeze the object"));
        }
        Ok(o)
    });
    it.def_method(&ctor, "preventExtensions", 1, |i, _this, args| {
        let obj = arg(args, 0);
        if matches!(obj, Value::Obj(_)) && !js_prevent_extensions(i, &obj)? {
            return Err(i.make_error("TypeError", "could not prevent extensions"));
        }
        Ok(obj)
    });
    it.def_method(&ctor, "isExtensible", 1, |i, _this, args| {
        let obj = arg(args, 0);
        // Object.isExtensible returns false for a non-object (ES2015+).
        if !matches!(obj, Value::Obj(_)) {
            return Ok(Value::Bool(false));
        }
        Ok(Value::Bool(js_is_extensible(i, &obj)?))
    });
    it.def_method(&ctor, "assign", 2, |i, _this, args| {
        // ToObject(target) — throws a TypeError for null/undefined.
        let to = to_object_arg(i, arg(args, 0), "Object.assign")?;
        let to_val = Value::Obj(to);
        for src in args.iter().skip(1) {
            if matches!(src, Value::Undefined | Value::Null) {
                continue;
            }
            let from = Value::Obj(to_object_arg(i, src.clone(), "Object.assign")?);
            if let Some((t, h)) = proxy_pair(i, &from) {
                // Proxy source: enumerate all own keys (string + symbol) via the traps, copying each
                // enumerable one.
                for key in proxy_own_keys(i, &t, &h)? {
                    let pk = ab(i.to_property_key(&key))?;
                    if proxy_key_enumerable(i, &t, &h, &pk)? {
                        let v = ab(i.get_member(&from, &pk))?;
                        assign_set(i, &to_val, &pk, v)?;
                    }
                }
                continue;
            }
            let o = from.as_obj().unwrap();
            let keys: Vec<Rc<str>> = {
                let b = o.borrow();
                b.props
                    .ordered_keys()
                    .into_iter()
                    .filter(|k| b.props.get(k).map(|p| p.enumerable).unwrap_or(false))
                    .collect()
            };
            for k in keys {
                let v = ab(i.get_member(&from, &k))?;
                assign_set(i, &to_val, &k, v)?;
            }
        }
        Ok(to_val)
    });
    it.def_method(&ctor, "is", 2, |_i, _this, args| {
        Ok(Value::Bool(same_value(&arg(args, 0), &arg(args, 1))))
    });
    it.def_method(&ctor, "values", 1, |i, _this, args| {
        let o = to_object_arg(i, arg(args, 0), "Object.values")?;
        enumerable_own_value_list(i, &Value::Obj(o), false)
    });
    it.def_method(&ctor, "entries", 1, |i, _this, args| {
        let o = to_object_arg(i, arg(args, 0), "Object.entries")?;
        enumerable_own_value_list(i, &Value::Obj(o), true)
    });
    it.def_method(&ctor, "fromEntries", 1, |i, _this, args| {
        // RequireObjectCoercible(iterable).
        let iterable = arg(args, 0);
        if matches!(iterable, Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "Object.fromEntries called on null or undefined",
            ));
        }
        let obj = i.new_object();
        // Lazily step the iterator; an abrupt completion while processing an entry closes it.
        let (iter, next) = ab(i.get_iterator(&iterable))?;
        loop {
            let entry = match ab(i.iterator_step(&iter, &next))? {
                Some(v) => v,
                None => break,
            };
            let processed = (|i: &mut Interp| -> Result<(), Value> {
                // Each entry must be an Object; keys/values are added via CreateDataPropertyOnObject,
                // which defines a plain data property and never invokes inherited setters.
                if !matches!(entry, Value::Obj(_)) {
                    return Err(
                        i.make_error("TypeError", "Object.fromEntries entry is not an object")
                    );
                }
                let k = ab(i.get_member(&entry, "0"))?;
                let v = ab(i.get_member(&entry, "1"))?;
                let key = ab(i.to_property_key(&k))?;
                obj.borrow_mut()
                    .props
                    .insert(key.as_str(), Property::data(v, true, true, true));
                Ok(())
            })(i);
            if let Err(e) = processed {
                i.iterator_close(&iter);
                return Err(e);
            }
        }
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "defineProperties", 2, |i, _this, args| {
        let o = arg(args, 0);
        if !matches!(o, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Object.defineProperties on non-object"));
        }
        object_define_properties(i, &o, arg(args, 1), "Object.defineProperties")?;
        Ok(o)
    });
    it.def_method(&ctor, "seal", 1, |i, _this, args| {
        let o = arg(args, 0);
        if matches!(o, Value::Obj(_)) && !set_integrity_level(i, &o, false)? {
            return Err(i.make_error("TypeError", "Object.seal could not seal the object"));
        }
        Ok(o)
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
                    && o.borrow()
                        .props
                        .iter()
                        .all(|(_, p)| !p.configurable && (p.accessor || !p.writable))
            }
            _ => true,
        };
        Ok(Value::Bool(frozen))
    });

    set_builtin(&it.global, "Object", Value::Obj(ctor));
}

/// Build a property descriptor from a JS descriptor object.
/// Build a descriptor object (`{value, writable, enumerable, configurable}` or `{get, set, ...}`).
fn descriptor_from_prop(i: &mut Interp, p: Property) -> Value {
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
    Value::Obj(d)
}

/// A property descriptor with only the explicitly-present fields populated.
#[derive(Default)]
struct PartialDesc {
    value: Option<Value>,
    get: Option<Value>,
    set: Option<Value>,
    writable: Option<bool>,
    enumerable: Option<bool>,
    configurable: Option<bool>,
}
/// CompletePropertyDescriptor: fill in default attributes for a partial descriptor.
fn complete_descriptor(pd: PartialDesc) -> Property {
    if pd.is_accessor() {
        Property {
            value: Value::Undefined,
            get: Some(pd.get.unwrap_or(Value::Undefined)),
            set: Some(pd.set.unwrap_or(Value::Undefined)),
            accessor: true,
            writable: false,
            enumerable: pd.enumerable.unwrap_or(false),
            configurable: pd.configurable.unwrap_or(false),
        }
    } else {
        Property {
            value: pd.value.unwrap_or(Value::Undefined),
            get: None,
            set: None,
            accessor: false,
            writable: pd.writable.unwrap_or(false),
            enumerable: pd.enumerable.unwrap_or(false),
            configurable: pd.configurable.unwrap_or(false),
        }
    }
}
impl PartialDesc {
    fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }
    fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }
}

/// Read + validate a descriptor object into a PartialDesc (ToPropertyDescriptor).
fn build_partial(i: &mut Interp, desc: &Value) -> Result<PartialDesc, Abrupt> {
    let o = match desc {
        Value::Obj(o) => o.clone(),
        _ => return Err(i.throw("TypeError", "Property description must be an object")),
    };
    // ToPropertyDescriptor reads each field with HasProperty/Get, so inherited fields count.
    let base = Value::Obj(o.clone());
    let bool_field = |i: &mut Interp, k: &str| -> Result<Option<bool>, Abrupt> {
        if i.has_property(&o, k) {
            let v = i.get_member(&base, k)?;
            Ok(Some(i.to_boolean(&v)))
        } else {
            Ok(None)
        }
    };
    let enumerable = bool_field(i, "enumerable")?;
    let configurable = bool_field(i, "configurable")?;
    let writable = bool_field(i, "writable")?;
    let value = if i.has_property(&o, "value") {
        Some(i.get_member(&base, "value")?)
    } else {
        None
    };
    let get = if i.has_property(&o, "get") {
        Some(i.get_member(&base, "get")?)
    } else {
        None
    };
    let set = if i.has_property(&o, "set") {
        Some(i.get_member(&base, "set")?)
    } else {
        None
    };
    if (get.is_some() || set.is_some()) && (value.is_some() || writable.is_some()) {
        return Err(i.throw(
            "TypeError",
            "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute",
        ));
    }
    if let Some(g) = &get {
        if !matches!(g, Value::Undefined) && !g.is_callable() {
            return Err(i.throw("TypeError", "Getter must be a function"));
        }
    }
    if let Some(s) = &set {
        if !matches!(s, Value::Undefined) && !s.is_callable() {
            return Err(i.throw("TypeError", "Setter must be a function"));
        }
    }
    Ok(PartialDesc {
        value,
        get,
        set,
        writable,
        enumerable,
        configurable,
    })
}

/// `Number.prototype.toPrecision(p)`: `p` significant digits, fixed or exponential per the exponent.
fn to_precision(n: f64, p: usize) -> String {
    if n == 0.0 {
        return if p == 1 {
            "0".to_string()
        } else {
            format!("0.{}", "0".repeat(p - 1))
        };
    }
    let neg = n < 0.0;
    // `p` significant digits via scientific notation (`d.ddde±E`, the mantissa has exactly `p` digits).
    let sci = format!("{:.*e}", p - 1, n.abs());
    let (mantissa, exp_str) = sci.split_once('e').unwrap();
    let e: i32 = exp_str.parse().unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let body = if e < -6 || e >= p as i32 {
        let sign = if e >= 0 { "+" } else { "-" };
        if p == 1 {
            format!("{}e{}{}", digits, sign, e.abs())
        } else {
            format!("{}.{}e{}{}", &digits[..1], &digits[1..], sign, e.abs())
        }
    } else if e >= 0 {
        let ip = (e + 1) as usize;
        if ip >= p {
            digits
        } else {
            format!("{}.{}", &digits[..ip], &digits[ip..])
        }
    } else {
        format!("0.{}{}", "0".repeat((-e - 1) as usize), digits)
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

fn opt_norm(v: Option<Value>) -> Option<Value> {
    v.filter(|x| !matches!(x, Value::Undefined))
}

/// OrdinaryDefineOwnProperty with the non-configurable / non-extensible invariant checks.
fn define_own_property(i: &mut Interp, o: &Gc, key: &str, desc: &Value) -> Result<bool, Abrupt> {
    let d = build_partial(i, desc)?;
    // TypedArray integer-index [[DefineOwnProperty]]: a valid in-range index accepts only a
    // configurable+enumerable+writable data descriptor and writes the value; a canonical-numeric
    // non-index can't be defined; both never touch the property map.
    if let Some(info) = ta_info(i, o) {
        match i.ta_index_kind(&info, key) {
            crate::value::TaIndex::Element(idx) => {
                if d.is_accessor()
                    || d.configurable == Some(false)
                    || d.enumerable == Some(false)
                    || d.writable == Some(false)
                {
                    return Ok(false);
                }
                if let Some(v) = d.value.clone() {
                    i.ta_store(&info, idx, &v)?;
                }
                return Ok(true);
            }
            crate::value::TaIndex::Exotic => return Ok(false),
            crate::value::TaIndex::Ordinary => {}
        }
    }
    let is_array = matches!(o.borrow().exotic, crate::value::Exotic::Array);
    // Array exotic [[DefineOwnProperty]]: `length` and array indices have special rules.
    if is_array && key == "length" {
        return array_set_length(i, o, &d);
    }
    let array_index = if is_array {
        key.parse::<u32>().ok().filter(|&n| n < 4294967295)
    } else {
        None
    };
    if let Some(idx) = array_index {
        // Adding an index at or past a non-writable `length` is rejected.
        let len = i.array_length(o);
        if idx as usize >= len
            && !o
                .borrow()
                .props
                .get("length")
                .map(|p| p.writable)
                .unwrap_or(true)
        {
            return Ok(false);
        }
    }
    let existing = o.borrow().props.get(key).cloned();

    let mut cur = match existing {
        None => {
            if !o.borrow().extensible {
                return Ok(false);
            }
            let prop = if d.is_accessor() {
                Property {
                    value: Value::Undefined,
                    get: opt_norm(d.get),
                    set: opt_norm(d.set),
                    accessor: true,
                    writable: false,
                    enumerable: d.enumerable.unwrap_or(false),
                    configurable: d.configurable.unwrap_or(false),
                }
            } else {
                Property {
                    value: d.value.unwrap_or(Value::Undefined),
                    get: None,
                    set: None,
                    accessor: false,
                    writable: d.writable.unwrap_or(false),
                    enumerable: d.enumerable.unwrap_or(false),
                    configurable: d.configurable.unwrap_or(false),
                }
            };
            o.borrow_mut().props.insert(key, prop);
            grow_array_length(i, o, array_index);
            return Ok(true);
        }
        Some(p) => p,
    };
    // Redefining an existing property: a non-configurable property restricts what may change.
    if !cur.configurable {
        if d.configurable == Some(true) {
            return Ok(false);
        }
        if let Some(e) = d.enumerable {
            if e != cur.enumerable {
                return Ok(false);
            }
        }
        if d.is_accessor() != cur.accessor && (d.is_accessor() || d.is_data()) {
            return Ok(false);
        }
        if cur.accessor {
            if let Some(g) = &d.get {
                if !same_value(g, cur.get.as_ref().unwrap_or(&Value::Undefined)) {
                    return Ok(false);
                }
            }
            if let Some(s) = &d.set {
                if !same_value(s, cur.set.as_ref().unwrap_or(&Value::Undefined)) {
                    return Ok(false);
                }
            }
        } else if !cur.writable {
            if d.writable == Some(true) {
                return Ok(false);
            }
            if let Some(v) = &d.value {
                if !same_value(v, &cur.value) {
                    return Ok(false);
                }
            }
        }
    }
    // Apply the present fields.
    if d.is_accessor() {
        cur.accessor = true;
        cur.value = Value::Undefined;
        cur.writable = false;
        if let Some(g) = d.get {
            cur.get = opt_norm(Some(g));
        }
        if let Some(s) = d.set {
            cur.set = opt_norm(Some(s));
        }
    } else if d.is_data() || !cur.accessor {
        if cur.accessor {
            cur.get = None;
            cur.set = None;
            cur.accessor = false;
            cur.value = Value::Undefined;
            cur.writable = false;
        }
        if let Some(v) = d.value {
            cur.value = v;
        }
        if let Some(w) = d.writable {
            cur.writable = w;
        }
    }
    if let Some(e) = d.enumerable {
        cur.enumerable = e;
    }
    if let Some(c) = d.configurable {
        cur.configurable = c;
    }
    o.borrow_mut().props.insert(key, cur);
    grow_array_length(i, o, array_index);
    Ok(true)
}

/// After defining an array index, grow `length` to `index + 1` if the index reached past the end.
fn grow_array_length(i: &mut Interp, o: &Gc, array_index: Option<u32>) {
    if let Some(idx) = array_index {
        if idx as usize >= i.array_length(o) {
            let writable = o
                .borrow()
                .props
                .get("length")
                .map(|p| p.writable)
                .unwrap_or(true);
            o.borrow_mut().props.insert(
                "length",
                Property::data(Value::Num((idx as f64) + 1.0), writable, false, false),
            );
        }
    }
}

/// Array exotic `length` define: validate the new length is a valid uint32, honor a non-writable
/// `length`, and drop the now-out-of-range index properties.
fn array_set_length(i: &mut Interp, o: &Gc, d: &PartialDesc) -> Result<bool, Abrupt> {
    let len_writable = o
        .borrow()
        .props
        .get("length")
        .map(|p| p.writable)
        .unwrap_or(true);
    let new_len = match &d.value {
        None => {
            // No value: only a writable change (length is non-configurable, non-enumerable).
            if d.configurable == Some(true) || d.enumerable == Some(true) {
                return Ok(false);
            }
            if let Some(w) = d.writable {
                if !len_writable && w {
                    return Ok(false); // can't make a non-writable length writable again
                }
                let cur = i.array_length(o) as f64;
                o.borrow_mut()
                    .props
                    .insert("length", Property::data(Value::Num(cur), w, false, false));
            }
            return Ok(true);
        }
        Some(v) => {
            let n = i.to_number(v)?;
            let u = if n.is_finite() && n >= 0.0 {
                n as u64
            } else {
                u64::MAX
            };
            if u > 4294967295 || (u as f64) != n {
                return Err(i.throw("RangeError", "Invalid array length"));
            }
            u as usize
        }
    };
    let old_len = i.array_length(o);
    if !len_writable && new_len != old_len {
        return Ok(false);
    }
    if new_len < old_len {
        o.borrow_mut()
            .props
            .retain(|k| k.parse::<usize>().map(|idx| idx < new_len).unwrap_or(true));
    }
    let writable = d.writable.unwrap_or(len_writable);
    o.borrow_mut().props.insert(
        "length",
        Property::data(Value::Num(new_len as f64), writable, false, false),
    );
    Ok(true)
}

pub(crate) fn same_value_pub(a: &Value, b: &Value) -> bool {
    same_value(a, b)
}
/// ToObject on a primitive (for sloppy-mode `this` coercion). Objects pass through.
pub(crate) fn box_primitive_pub(i: &mut Interp, v: Value) -> Value {
    box_primitive(i, v)
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
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        (Value::Sym(x), Value::Sym(y)) => Rc::ptr_eq(x, y),
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
    ap.borrow_mut().props.insert(
        "length",
        Property::data(Value::Num(0.0), true, false, false),
    );

    it.def_method(&ap, "push", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let mut len = ab(i.checked_array_len(&o))? as u64;
        // The resulting length may not exceed 2^53-1.
        if len + args.len() as u64 > 9007199254740991 {
            return Err(i.make_error("TypeError", "push would exceed the maximum array length"));
        }
        for a in args {
            ab(i.set_member(&this, &len.to_string(), a.clone()))?;
            len += 1;
        }
        // Generic objects don't auto-track length the way arrays do, so set it explicitly.
        ab(i.set_member(&this, "length", Value::Num(len as f64)))?;
        Ok(Value::Num(len as f64))
    });
    it.def_method(&ap, "pop", 0, |i, this, _args| {
        let o = arr_to_object(i, &this)?;
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
        let o = arr_to_object(i, &this)?;
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
        let o = arr_to_object(i, &this)?;
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
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))? as i64;
        let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
        let end = match arg(args, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        let count = (end - start).max(0) as usize;
        let result = array_species_create(i, &this, count)?;
        let mut k = start;
        let mut to = 0usize;
        while k < end {
            let key = k.to_string();
            // Preserve holes: only copy indices the source actually has (HasProperty).
            if i.has_property(&o, &key) {
                let v = ab(i.get_member(&this, &key))?;
                ab(i.set_member(&result, &to.to_string(), v))?;
            }
            k += 1;
            to += 1;
        }
        Ok(result)
    });
    it.def_method(&ap, "indexOf", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        let target = arg(args, 0);
        let from = match arg(args, 1) {
            Value::Undefined => 0usize,
            v => {
                let n = ab(i.to_number(&v))?;
                if n >= 0.0 {
                    n as usize
                } else {
                    (len as f64 + n).max(0.0) as usize
                }
            }
        };
        for k in from..len {
            if !i.has_property(&o, &k.to_string()) {
                continue; // indexOf skips holes
            }
            let v = ab(i.get_member(&this, &k.to_string()))?;
            if i.strict_equals(&v, &target) {
                return Ok(Value::Num(k as f64));
            }
        }
        Ok(Value::Num(-1.0))
    });
    it.def_method(&ap, "includes", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))? as i64;
        let target = arg(args, 0);
        let mut k = match arg(args, 1) {
            Value::Undefined => 0i64,
            v => {
                let n = ab(i.to_number(&v))?;
                if n == f64::INFINITY {
                    return Ok(Value::Bool(false));
                }
                if n == f64::NEG_INFINITY || n.is_nan() {
                    0
                } else if n >= 0.0 {
                    n.trunc() as i64
                } else {
                    (len + n.trunc() as i64).max(0)
                }
            }
        };
        while k < len {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            if same_value_zero(&v, &target) {
                return Ok(Value::Bool(true));
            }
            k += 1;
        }
        Ok(Value::Bool(false))
    });
    it.def_method(&ap, "join", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
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
        arr_require_coercible(i, &this)?;
        let result = array_species_create(i, &this, 0)?;
        let mut n = 0usize;
        let items: Vec<Value> = std::iter::once(this.clone())
            .chain(args.iter().cloned())
            .collect();
        for v in &items {
            // IsConcatSpreadable: @@isConcatSpreadable if defined, else IsArray.
            let spreadable = if let Value::Obj(_) = v {
                let key = well_known_key(i, "isConcatSpreadable");
                let flag = match &key {
                    Some(k) => ab(i.get_member(v, k))?,
                    None => Value::Undefined,
                };
                match flag {
                    Value::Undefined => json_is_array(i, v)?,
                    other => i.to_boolean(&other),
                }
            } else {
                false
            };
            if spreadable {
                let len = ab(i.checked_array_len(&match v {
                    Value::Obj(o) => o.clone(),
                    _ => unreachable!(),
                }))?;
                for k in 0..len {
                    let key = k.to_string();
                    if i.has_property(
                        &match v {
                            Value::Obj(o) => o.clone(),
                            _ => unreachable!(),
                        },
                        &key,
                    ) {
                        let elem = ab(i.get_member(v, &key))?;
                        json_create_data_prop(i, &result, &n.to_string(), elem)?;
                    }
                    n += 1; // increment for holes too, preserving their position
                }
            } else {
                json_create_data_prop(i, &result, &n.to_string(), v.clone())?;
                n += 1;
            }
        }
        ab(i.set_member(&result, "length", Value::Num(n as f64)))?;
        Ok(result)
    });
    it.def_method(&ap, "forEach", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.forEach callback is not callable",
            ));
        }
        let cb_this = arg(args, 1);
        for k in 0..len {
            if !i.has_property(&o, &k.to_string()) {
                continue; // skip array holes
            }
            let v = ab(i.get_member(&this, &k.to_string()))?;
            ab(i.call(
                cb.clone(),
                cb_this.clone(),
                &[v, Value::Num(k as f64), this.clone()],
            ))?;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&ap, "map", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error("TypeError", "Array.prototype.map callback is not callable"));
        }
        let cb_this = arg(args, 1);
        let result = array_species_create(i, &this, len)?;
        for k in 0..len {
            if !i.has_property(&o, &k.to_string()) {
                continue; // holes stay holes in the result
            }
            let v = ab(i.get_member(&this, &k.to_string()))?;
            let mapped = ab(i.call(
                cb.clone(),
                cb_this.clone(),
                &[v, Value::Num(k as f64), this.clone()],
            ))?;
            ab(i.set_member(&result, &k.to_string(), mapped))?;
        }
        Ok(result)
    });
    it.def_method(&ap, "filter", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.filter callback is not callable",
            ));
        }
        let cb_this = arg(args, 1);
        let result = array_species_create(i, &this, 0)?;
        let mut to = 0usize;
        for k in 0..len {
            if !i.has_property(&o, &k.to_string()) {
                continue;
            }
            let v = ab(i.get_member(&this, &k.to_string()))?;
            let keep = ab(i.call(
                cb.clone(),
                cb_this.clone(),
                &[v.clone(), Value::Num(k as f64), this.clone()],
            ))?;
            if i.to_boolean(&keep) {
                ab(i.set_member(&result, &to.to_string(), v))?;
                to += 1;
            }
        }
        Ok(result)
    });
    it.def_method(&ap, "reduce", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.reduce callback is not callable",
            ));
        }
        let mut k = 0;
        let mut acc;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            // Seed with the first present element (holes are skipped).
            loop {
                if k >= len {
                    return Err(
                        i.make_error("TypeError", "Reduce of empty array with no initial value")
                    );
                }
                if i.has_property(&o, &k.to_string()) {
                    acc = ab(i.get_member(&this, &k.to_string()))?;
                    k += 1;
                    break;
                }
                k += 1;
            }
        }
        while k < len {
            if i.has_property(&o, &k.to_string()) {
                let v = ab(i.get_member(&this, &k.to_string()))?;
                acc = ab(i.call(
                    cb.clone(),
                    Value::Undefined,
                    &[acc, v, Value::Num(k as f64), this.clone()],
                ))?;
            }
            k += 1;
        }
        Ok(acc)
    });
    it.def_method(&ap, "reverse", 0, |i, this, _args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        for k in 0..len / 2 {
            let lower = k.to_string();
            let upper = (len - 1 - k).to_string();
            // HasProperty/Get the two ends, then swap — preserving holes (a hole moves as a delete).
            let lower_exists = i.has_property(&o, &lower);
            let lower_val = if lower_exists {
                Some(ab(i.get_member(&this, &lower))?)
            } else {
                None
            };
            let upper_exists = i.has_property(&o, &upper);
            let upper_val = if upper_exists {
                Some(ab(i.get_member(&this, &upper))?)
            } else {
                None
            };
            match (lower_val, upper_val) {
                (Some(lv), Some(uv)) => {
                    ab(i.set_member(&this, &lower, uv))?;
                    ab(i.set_member(&this, &upper, lv))?;
                }
                (None, Some(uv)) => {
                    ab(i.set_member(&this, &lower, uv))?;
                    json_delete_prop(i, &this, &upper)?;
                }
                (Some(lv), None) => {
                    json_delete_prop(i, &this, &lower)?;
                    ab(i.set_member(&this, &upper, lv))?;
                }
                (None, None) => {}
            }
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
    it.def_method(&ap, "toLocaleString", 0, |i, this, _args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        let mut out = String::new();
        for k in 0..len {
            if k > 0 {
                out.push(',');
            }
            let el = ab(i.get_member(&this, &k.to_string()))?;
            if !matches!(el, Value::Undefined | Value::Null) {
                // ToString(? Invoke(element, "toLocaleString")).
                let tls = ab(i.get_member(&el, "toLocaleString"))?;
                if !tls.is_callable() {
                    return Err(i.make_error("TypeError", "toLocaleString is not a function"));
                }
                let s = ab(i.call(tls, el, &[]))?;
                out.push_str(&ab(i.to_string(&s))?);
            }
        }
        Ok(Value::from_string(out))
    });
    it.def_method(&ap, "at", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
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
    it.def_method(&ap, "find", 1, |i, this, args| {
        array_find(i, this, args, true, false)
    });
    it.def_method(&ap, "findIndex", 1, |i, this, args| {
        array_find(i, this, args, false, false)
    });
    it.def_method(&ap, "findLast", 1, |i, this, args| {
        array_find(i, this, args, true, true)
    });
    it.def_method(&ap, "findLastIndex", 1, |i, this, args| {
        array_find(i, this, args, false, true)
    });
    it.def_method(&ap, "some", 1, |i, this, args| {
        array_some_every(i, this, args, false)
    });
    it.def_method(&ap, "every", 1, |i, this, args| {
        array_some_every(i, this, args, true)
    });
    it.def_method(&ap, "fill", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
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
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))? as i64;
        if len == 0 {
            return Ok(Value::Num(-1.0));
        }
        let target = arg(args, 0);
        // fromIndex (default len-1): the highest index to search from, going backward.
        let mut k = if args.len() > 1 {
            let n = ab(i.to_number(&arg(args, 1)))?;
            if n == f64::NEG_INFINITY {
                return Ok(Value::Num(-1.0));
            }
            let n = if n.is_nan() { 0 } else { n.trunc() as i64 };
            if n >= 0 {
                n.min(len - 1)
            } else {
                len + n
            }
        } else {
            len - 1
        };
        while k >= 0 {
            let v = ab(i.get_member(&this, &k.to_string()))?;
            if i.strict_equals(&v, &target) {
                return Ok(Value::Num(k as f64));
            }
            k -= 1;
        }
        Ok(Value::Num(-1.0))
    });
    it.def_method(&ap, "flat", 0, |i, this, args| {
        arr_require_coercible(i, &this)?;
        let depth = match arg(args, 0) {
            Value::Undefined => 1.0,
            v => ab(i.to_number(&v))?,
        };
        let mut out = Vec::new();
        array_flatten(i, &this, depth as i64, &mut out)?;
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "flatMap", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.flatMap mapper is not callable",
            ));
        }
        let cb_this = arg(args, 1);
        // The mapper runs only on present indices; its result is then flattened one level.
        let mut out = Vec::new();
        for k in 0..len {
            if !ab(i.js_has_property(&this, &k.to_string()))? {
                continue;
            }
            let v = ab(i.get_member(&this, &k.to_string()))?;
            let mapped = ab(i.call(
                cb.clone(),
                cb_this.clone(),
                &[v, Value::Num(k as f64), this.clone()],
            ))?;
            if json_is_array(i, &mapped)? {
                array_flatten(i, &mapped, 0, &mut out)?;
            } else {
                out.push(mapped);
            }
        }
        Ok(i.make_array(out))
    });
    it.def_method(&ap, "splice", 2, array_splice);
    it.def_method(&ap, "sort", 1, |i, this, args| {
        let cmp = arg(args, 0);
        if !matches!(cmp, Value::Undefined) && !cmp.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "the comparator must be a function or undefined",
            ));
        }
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        // SortIndexedProperties: read only the present indices (holes are skipped, not read).
        let mut items = Vec::new();
        for k in 0..len {
            if ab(i.js_has_property(&this, &k.to_string()))? {
                items.push(ab(i.get_member(&this, &k.to_string()))?);
            }
        }
        let item_count = items.len();
        merge_sort(i, &mut items, &cmp)?;
        for (k, v) in items.into_iter().enumerate() {
            ab(i.set_member(&this, &k.to_string(), v))?;
        }
        // Vacated trailing indices (originally holes, or beyond the present count) are deleted.
        for k in item_count..len {
            o.borrow_mut().props.remove(k.to_string().as_str());
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
        let idx = if rel < 0.0 {
            len + rel as i64
        } else {
            rel as i64
        };
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
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.reduceRight callback is not callable",
            ));
        }
        let mut acc;
        let mut k = len as i64 - 1;
        if args.len() >= 2 {
            acc = arg(args, 1);
        } else {
            // Seed with the last present element (holes are skipped).
            loop {
                if k < 0 {
                    return Err(
                        i.make_error("TypeError", "Reduce of empty array with no initial value")
                    );
                }
                if i.has_property(&o, &k.to_string()) {
                    acc = ab(i.get_member(&this, &k.to_string()))?;
                    k -= 1;
                    break;
                }
                k -= 1;
            }
        }
        while k >= 0 {
            if i.has_property(&o, &k.to_string()) {
                let v = ab(i.get_member(&this, &k.to_string()))?;
                acc = ab(i.call(
                    cb.clone(),
                    Value::Undefined,
                    &[acc, v, Value::Num(k as f64), this.clone()],
                ))?;
            }
            k -= 1;
        }
        Ok(acc)
    });
    it.def_method(&ap, "copyWithin", 2, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.checked_array_len(&o))? as i64;
        let target = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
        let start = norm_index(ab(i.to_number(&arg(args, 1)))?, len, 0);
        let end = match arg(args, 2) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len, len),
        };
        // count = min(end - start, len - target); a source hole deletes the target index.
        let count = (end - start).min(len - target).max(0);
        let mut snapshot: Vec<Option<Value>> = Vec::with_capacity(count as usize);
        for off in 0..count {
            let k = (start + off).to_string();
            if i.has_property(&o, &k) {
                snapshot.push(Some(ab(i.get_member(&this, &k))?));
            } else {
                snapshot.push(None);
            }
        }
        for (off, slot) in snapshot.into_iter().enumerate() {
            let ti = (target + off as i64).to_string();
            match slot {
                Some(v) => ab(i.set_member(&this, &ti, v))?,
                None => {
                    o.borrow_mut().props.remove(ti.as_str());
                }
            }
        }
        Ok(this)
    });
    it.def_method(&ap, "values", 0, |i, this, _| {
        arr_require_coercible(i, &this)?;
        Ok(make_array_iterator(i, this, 0))
    });
    it.def_method(&ap, "keys", 0, |i, this, _| {
        arr_require_coercible(i, &this)?;
        Ok(make_array_iterator(i, this, 1))
    });
    it.def_method(&ap, "entries", 0, |i, this, _| {
        arr_require_coercible(i, &this)?;
        Ok(make_array_iterator(i, this, 2))
    });
    // `arr[Symbol.iterator]` is `Array.prototype.values`.
    if let Some(sym) = it.iterator_sym.clone() {
        let values_fn = ap
            .borrow()
            .props
            .get("values")
            .map(|p| p.value.clone())
            .unwrap();
        ap.borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(values_fn));
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
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(ap.clone()), false, false, false),
    );
    ap.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "isArray", 1, |i, _this, args| {
        // IsArray unwraps Proxies: a proxy of an array is an array (a revoked proxy throws).
        let mut v = arg(args, 0);
        loop {
            match &v {
                Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array) => {
                    return Ok(Value::Bool(true))
                }
                Value::Obj(_) => match proxy_pair(i, &v) {
                    Some((target, _)) => v = target,
                    None => return Ok(Value::Bool(false)),
                },
                _ => return Ok(Value::Bool(false)),
            }
        }
    });
    it.def_method(&ctor, "of", 0, |i, this, args| {
        // If `this` is a constructor, build the result via `new this(len)`; else a plain Array.
        let len = args.len();
        let arr = if is_constructor_value(&this) {
            ab(i.construct(this.clone(), &[Value::Num(len as f64)]))?
        } else {
            i.make_array(Vec::new())
        };
        for (k, v) in args.iter().enumerate() {
            // CreateDataPropertyOrThrow(A, k, v).
            if let Value::Obj(o) = &arr {
                let desc = i.new_object();
                set_data(&desc, "value", v.clone());
                set_data(&desc, "writable", Value::Bool(true));
                set_data(&desc, "enumerable", Value::Bool(true));
                set_data(&desc, "configurable", Value::Bool(true));
                if !ab(define_own_property(i, o, &k.to_string(), &Value::Obj(desc)))? {
                    return Err(i.make_error("TypeError", "Array.of: cannot define property"));
                }
            }
        }
        ab(i.set_member(&arr, "length", Value::Num(len as f64)))?;
        Ok(arr)
    });
    it.def_method(&ctor, "from", 1, |i, this, args| {
        let source = arg(args, 0);
        let mapfn = arg(args, 1);
        let this_arg = arg(args, 2);
        if !matches!(mapfn, Value::Undefined) && !mapfn.is_callable() {
            return Err(i.make_error("TypeError", "Array.from: mapFn is not callable"));
        }
        let mut from_iterable = true;
        let items = match &source {
            Value::Str(_) => ab(i.iterate(&source))?,
            Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Array) => ab(i.iterate(&source))?,
            Value::Obj(_) if i.has_iterator(&source) => ab(i.iterate(&source))?,
            Value::Obj(_) => {
                from_iterable = false;
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
        let mut out = Vec::with_capacity(items.len());
        for (k, v) in items.into_iter().enumerate() {
            let mv = if mapfn.is_callable() {
                ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k as f64)]))?
            } else {
                v
            };
            out.push(mv);
        }
        // `Array.from.call(C, …)` builds the result via the constructor `C`; the plain Array
        // constructor (or a non-callable receiver) makes an ordinary array.
        let array_ctor = i.global.borrow().props.get("Array").map(|p| p.value.clone());
        let is_array_ctor = matches!((&this, &array_ctor), (Value::Obj(a), Some(Value::Obj(b))) if Rc::ptr_eq(a, b));
        if this.is_callable() && !is_array_ctor {
            let len = out.len();
            // Iterable source constructs with no args; array-like forwards the length.
            let ctor_args: &[Value] = if from_iterable { &[] } else { &[Value::Num(len as f64)] };
            let res = ab(i.construct(this, ctor_args))?;
            for (k, v) in out.into_iter().enumerate() {
                // CreateDataPropertyOrThrow: a non-configurable existing index makes this throw.
                if let Value::Obj(o) = &res {
                    let desc = i.new_object();
                    set_data(&desc, "value", v);
                    set_data(&desc, "writable", Value::Bool(true));
                    set_data(&desc, "enumerable", Value::Bool(true));
                    set_data(&desc, "configurable", Value::Bool(true));
                    if !ab(define_own_property(i, o, &k.to_string(), &Value::Obj(desc)))? {
                        return Err(i.make_error("TypeError", "Array.from: cannot define property"));
                    }
                }
            }
            ab(i.set_member(&res, "length", Value::Num(len as f64)))?;
            Ok(res)
        } else {
            Ok(i.make_array(out))
        }
    });
    it.def_method(&ctor, "fromAsync", 1, array_from_async);
    install_species(it, &ctor);
    set_builtin(&it.global, "Array", Value::Obj(ctor));
}

fn array_find(
    i: &mut Interp,
    this: Value,
    args: &[Value],
    want_value: bool,
    from_last: bool,
) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = ab(i.to_length(&o))?;
    let cb = arg(args, 0);
    let cb_this = arg(args, 1);
    for step in 0..len {
        let k = if from_last { len - 1 - step } else { step };
        let v = ab(i.get_member(&this, &k.to_string()))?;
        let r = ab(i.call(
            cb.clone(),
            cb_this.clone(),
            &[v.clone(), Value::Num(k as f64), this.clone()],
        ))?;
        if i.to_boolean(&r) {
            return Ok(if want_value { v } else { Value::Num(k as f64) });
        }
    }
    Ok(if want_value {
        Value::Undefined
    } else {
        Value::Num(-1.0)
    })
}

fn array_some_every(
    i: &mut Interp,
    this: Value,
    args: &[Value],
    every: bool,
) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = ab(i.to_length(&o))?;
    let cb = arg(args, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "predicate is not callable"));
    }
    let cb_this = arg(args, 1);
    for k in 0..len {
        if !i.has_property(&o, &k.to_string()) {
            continue; // skip holes
        }
        let v = ab(i.get_member(&this, &k.to_string()))?;
        let r = ab(i.call(
            cb.clone(),
            cb_this.clone(),
            &[v, Value::Num(k as f64), this.clone()],
        ))?;
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

fn array_flatten(
    i: &mut Interp,
    arr: &Value,
    depth: i64,
    out: &mut Vec<Value>,
) -> Result<(), Value> {
    let o = match arr {
        Value::Obj(o) => o.clone(),
        _ => return Ok(()),
    };
    let len = ab(i.checked_array_len(&o))?;
    for k in 0..len {
        // FlattenIntoArray skips holes (HasProperty), recursing into nested arrays.
        if !ab(i.js_has_property(arr, &k.to_string()))? {
            continue;
        }
        let v = ab(i.get_member(arr, &k.to_string()))?;
        let is_arr = json_is_array(i, &v)?;
        if depth > 0 && is_arr {
            array_flatten(i, &v, depth - 1, out)?;
        } else {
            out.push(v);
        }
    }
    Ok(())
}

fn array_splice(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = ab(i.checked_array_len(&o))? as i64;
    let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len, 0);
    let delete_count = if args.is_empty() {
        0
    } else if args.len() < 2 {
        len - start
    } else {
        (ab(i.to_number(&arg(args, 1)))? as i64).clamp(0, len - start)
    };
    let items: Vec<Value> = if args.len() > 2 {
        args[2..].to_vec()
    } else {
        Vec::new()
    };
    // The removed array (ArraySpeciesCreate) preserves holes via HasProperty.
    let removed = array_species_create(i, &this, delete_count.max(0) as usize)?;
    for k in 0..delete_count {
        let from = (start + k).to_string();
        if i.has_property(&o, &from) {
            let v = ab(i.get_member(&this, &from))?;
            json_create_data_prop(i, &removed, &k.to_string(), v)?;
        }
    }
    ab(i.set_member(&removed, "length", Value::Num(delete_count as f64)))?;
    let item_count = items.len() as i64;
    // Shift the trailing elements (preserving holes) to open or close the gap.
    if item_count < delete_count {
        for k in start..(len - delete_count) {
            let from = (k + delete_count).to_string();
            let to = (k + item_count).to_string();
            if i.has_property(&o, &from) {
                let v = ab(i.get_member(&this, &from))?;
                ab(i.set_member(&this, &to, v))?;
            } else {
                json_delete_prop(i, &this, &to)?;
            }
        }
        for k in ((len - delete_count + item_count)..len).rev() {
            json_delete_prop(i, &this, &k.to_string())?;
        }
    } else if item_count > delete_count {
        for k in ((start + 1)..=(len - delete_count)).rev() {
            let from = (k + delete_count - 1).to_string();
            let to = (k + item_count - 1).to_string();
            if i.has_property(&o, &from) {
                let v = ab(i.get_member(&this, &from))?;
                ab(i.set_member(&this, &to, v))?;
            } else {
                json_delete_prop(i, &this, &to)?;
            }
        }
    }
    for (off, v) in items.iter().enumerate() {
        ab(i.set_member(&this, &(start + off as i64).to_string(), v.clone()))?;
    }
    ab(i.set_member(
        &this,
        "length",
        Value::Num((len - delete_count + item_count) as f64),
    ))?;
    Ok(removed)
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
        proto
            .borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(f)));
    }
    // Iterator-helper methods on %IteratorPrototype%.
    it.def_method(&proto, "map", 1, |i, t, a| {
        make_iter_helper(i, t, "map", arg(a, 0))
    });
    it.def_method(&proto, "filter", 1, |i, t, a| {
        make_iter_helper(i, t, "filter", arg(a, 0))
    });
    it.def_method(&proto, "take", 1, |i, t, a| {
        make_iter_helper(i, t, "take", arg(a, 0))
    });
    it.def_method(&proto, "drop", 1, |i, t, a| {
        make_iter_helper(i, t, "drop", arg(a, 0))
    });
    it.def_method(&proto, "flatMap", 1, |i, t, a| {
        make_iter_helper(i, t, "flatMap", arg(a, 0))
    });
    it.def_method(&proto, "toArray", 0, |i, this, _| {
        let mut out = Vec::new();
        while let Some(v) = step_iter(i, &this)? {
            out.push(v);
        }
        Ok(i.make_array(out))
    });
    it.def_method(&proto, "forEach", 1, |i, this, a| {
        let f = arg(a, 0);
        if !f.is_callable() {
            i.iterator_close(&this);
            return Err(i.make_error(
                "TypeError",
                "Iterator.prototype.forEach argument is not callable",
            ));
        }
        let mut k = 0.0;
        while let Some(v) = step_iter(i, &this)? {
            if let Err(e) = i.call(f.clone(), Value::Undefined, &[v, Value::Num(k)]) {
                i.iterator_close(&this);
                return Err(crate::interpreter::abrupt_value(e));
            }
            k += 1.0;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "reduce", 1, |i, this, a| {
        let f = arg(a, 0);
        if !f.is_callable() {
            i.iterator_close(&this);
            return Err(i.make_error("TypeError", "reducer is not callable"));
        }
        let mut acc;
        let mut k = 0.0;
        if a.len() >= 2 {
            acc = arg(a, 1);
        } else {
            acc = match step_iter(i, &this)? {
                Some(v) => v,
                None => {
                    return Err(i.make_error(
                        "TypeError",
                        "Reduce of empty iterator with no initial value",
                    ))
                }
            };
            k = 1.0;
        }
        while let Some(v) = step_iter(i, &this)? {
            acc = match i.call(f.clone(), Value::Undefined, &[acc, v, Value::Num(k)]) {
                Ok(r) => r,
                Err(e) => {
                    i.iterator_close(&this);
                    return Err(crate::interpreter::abrupt_value(e));
                }
            };
            k += 1.0;
        }
        Ok(acc)
    });
    it.def_method(&proto, "some", 1, |i, t, a| iter_some_every(i, t, a, true));
    it.def_method(&proto, "every", 1, |i, t, a| {
        iter_some_every(i, t, a, false)
    });
    it.def_method(&proto, "find", 1, |i, this, a| {
        let f = arg(a, 0);
        if !f.is_callable() {
            i.iterator_close(&this);
            return Err(i.make_error("TypeError", "predicate is not callable"));
        }
        let mut k = 0.0;
        while let Some(v) = step_iter(i, &this)? {
            let r = match i.call(f.clone(), Value::Undefined, &[v.clone(), Value::Num(k)]) {
                Ok(r) => r,
                Err(e) => {
                    i.iterator_close(&this);
                    return Err(crate::interpreter::abrupt_value(e));
                }
            };
            if i.to_boolean(&r) {
                ab(i.iterator_close_normal(&this))?;
                return Ok(v);
            }
            k += 1.0;
        }
        Ok(Value::Undefined)
    });

    // Iterator.prototype[@@dispose]: close the iterator (calls its return method).
    if let Some(key) = well_known_key(it, "dispose") {
        let disp = it.make_native("[Symbol.dispose]", 0, |i, this, _a| {
            i.iterator_close(&this);
            Ok(Value::Undefined)
        });
        proto
            .borrow_mut()
            .props
            .insert(key, Property::builtin(Value::Obj(disp)));
    }

    let ctor = it.make_native("Iterator", 0, |i, t, _a| {
        // Abstract: `new Iterator()` (this === undefined) throws, but `super()` from a subclass
        // (this is the instance) is allowed.
        if matches!(t, Value::Undefined) {
            return Err(i.make_error(
                "TypeError",
                "Abstract class Iterator not directly constructable",
            ));
        }
        Ok(t)
    });
    ctor.borrow_mut().is_constructor = true;
    it.def_method(&ctor, "from", 1, |i, _t, a| {
        let v = arg(a, 0);
        // GetIteratorFlattenable (strings allowed): a string/iterable via @@iterator, or an iterator
        // used directly. If the result already inherits %Iterator.prototype%, return it; else wrap it.
        let iter = get_iterator_flattenable(i, &v, true)?;
        let iter_proto = i.extra_protos.get("%IteratorPrototype%").cloned();
        let inherits = matches!(&iter, Value::Obj(o) if {
            let mut p = o.borrow().proto.clone();
            let mut found = false;
            while let Some(pp) = p {
                if iter_proto.as_ref().is_some_and(|ip| Rc::ptr_eq(&pp, ip)) { found = true; break; }
                p = pp.borrow().proto.clone();
            }
            found
        });
        if inherits {
            return Ok(iter);
        }
        // Wrap: a fresh iterator-helper-bearing object that forwards next/return to `iter`.
        let next = ab(i.get_member(&iter, "next"))?;
        let obj = Object::new(i.extra_protos.get("%IteratorPrototype%").cloned());
        set_builtin(&obj, "__wrap_iter", iter);
        set_builtin(&obj, "__wrap_next", next);
        i.def_method(&obj, "next", 0, |i, this, _a| {
            let it = ab(i.get_member(&this, "__wrap_iter"))?;
            let nx = ab(i.get_member(&this, "__wrap_next"))?;
            let res = ab(i.call(nx, it, &[]))?;
            Ok(res)
        });
        i.def_method(&obj, "return", 0, |i, this, _a| {
            let it = ab(i.get_member(&this, "__wrap_iter"))?;
            ab(i.iterator_close_normal(&it))?;
            Ok(iter_result(i, Value::Undefined, true))
        });
        Ok(Value::Obj(obj))
    });
    it.def_method(&ctor, "zip", 1, |i, _t, a| iterator_zip(i, a, false));
    it.def_method(&ctor, "zipKeyed", 1, |i, _t, a| iterator_zip(i, a, true));
    it.def_method(&ctor, "concat", 0, |i, _t, a| {
        // Each argument must be an object; capture its @@iterator method now, in order.
        let iter_key = i
            .iterator_sym
            .clone()
            .map(|s| Interp::sym_key(&s))
            .unwrap_or_default();
        let mut items = Vec::new();
        let mut methods = Vec::new();
        for v in a {
            if !matches!(v, Value::Obj(_)) {
                return Err(i.make_error("TypeError", "Iterator.concat argument is not an object"));
            }
            let m = ab(i.get_member(v, &iter_key))?;
            if !m.is_callable() {
                return Err(i.make_error("TypeError", "Iterator.concat argument is not iterable"));
            }
            items.push(v.clone());
            methods.push(m);
        }
        let obj = Object::new(i.extra_protos.get("%IteratorPrototype%").cloned());
        let items_arr = i.make_array(items);
        let methods_arr = i.make_array(methods);
        set_builtin(&obj, "__cc_items", items_arr);
        set_builtin(&obj, "__cc_methods", methods_arr);
        set_builtin(&obj, "__cc_idx", Value::Num(0.0));
        set_builtin(&obj, "__cc_cur", Value::Undefined);
        set_builtin(&obj, "__cc_curnext", Value::Undefined);
        set_builtin(&obj, "__cc_done", Value::Bool(false));
        i.def_method(&obj, "next", 0, concat_next);
        // return() closes the currently-open inner iterator (once), then reports done.
        i.def_method(&obj, "return", 0, |i, this, _a| {
            let done = ab(i.get_member(&this, "__cc_done"))?;
            if !i.to_boolean(&done) {
                set_internal(this.as_obj().unwrap(), "__cc_done", Value::Bool(true));
                let cur = ab(i.get_member(&this, "__cc_cur"))?;
                if matches!(cur, Value::Obj(_)) {
                    ab(i.iterator_close_normal(&cur))?;
                }
            }
            Ok(iter_result(i, Value::Undefined, true))
        });
        if let Some(sym) = i.iterator_sym.clone() {
            let itf = i.make_native("[Symbol.iterator]", 0, return_this);
            obj.borrow_mut()
                .props
                .insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(itf)));
        }
        Ok(Value::Obj(obj))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Iterator", Value::Obj(ctor));

    // %ArrayIteratorPrototype%: the intermediate prototype of Array iterators (its own [[Prototype]]
    // is %IteratorPrototype%), so getPrototypeOf(getPrototypeOf(arrIter)) lands on %IteratorPrototype%.
    let arr_iter_proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
    set_to_string_tag(it, &arr_iter_proto, "Array Iterator");
    it.extra_protos
        .insert("%ArrayIteratorPrototype%", arr_iter_proto);
}

/// `Array.fromAsync(source, mapFn?, thisArg?)`: build an array from a sync/async iterable or an
/// array-like, awaiting each element, and return a promise of the result. lumen drains microtasks
/// synchronously, so the whole thing runs eagerly and settles the returned promise.
fn array_from_async(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let promise = i.new_promise();
    let source = arg(a, 0);
    let mapfn = arg(a, 1);
    let this_arg = arg(a, 2);
    let outcome = (|| -> Result<Value, Value> {
        if !matches!(mapfn, Value::Undefined) && !mapfn.is_callable() {
            return Err(i.make_error("TypeError", "Array.fromAsync: mapFn is not callable"));
        }
        let mut out: Vec<Value> = Vec::new();
        // Prefer @@asyncIterator, then a sync iterator; otherwise treat `source` as array-like.
        let async_it = match well_known_key(i, "asyncIterator") {
            Some(k) => ab(i.get_member(&source, &k))?,
            None => Value::Undefined,
        };
        if async_it.is_callable() {
            let iter = ab(i.call(async_it, source.clone(), &[]))?;
            let next = ab(i.get_member(&iter, "next"))?;
            let mut k = 0.0;
            loop {
                let res = ab(i.call(next.clone(), iter.clone(), &[]))?;
                let res = ab(i.await_value(res))?;
                let done = ab(i.get_member(&res, "done"))?;
                if i.to_boolean(&done) {
                    break;
                }
                let raw = ab(i.get_member(&res, "value"))?;
                let mut v = ab(i.await_value(raw))?;
                if mapfn.is_callable() {
                    let mapped = ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k)]))?;
                    v = ab(i.await_value(mapped))?;
                }
                out.push(v);
                k += 1.0;
            }
        } else if matches!(source, Value::Str(_)) || i.has_iterator(&source) {
            for (k, raw) in ab(i.iterate(&source))?.into_iter().enumerate() {
                let mut v = ab(i.await_value(raw))?;
                if mapfn.is_callable() {
                    let mapped =
                        ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k as f64)]))?;
                    v = ab(i.await_value(mapped))?;
                }
                out.push(v);
            }
        } else if let Value::Obj(o) = &source {
            let len = ab(i.to_length(&o.clone()))?;
            for k in 0..len {
                let raw = ab(i.get_member(&source, &k.to_string()))?;
                let mut v = ab(i.await_value(raw))?;
                if mapfn.is_callable() {
                    let mapped =
                        ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k as f64)]))?;
                    v = ab(i.await_value(mapped))?;
                }
                out.push(v);
            }
        } else {
            return Err(i.make_error(
                "TypeError",
                "Array.fromAsync requires an iterable or array-like",
            ));
        }
        Ok(i.make_array(out))
    })();
    match outcome {
        Ok(arr) => i.resolve_promise(&promise, arr),
        Err(e) => i.reject_promise(&promise, e),
    }
    Ok(promise)
}

fn iter_some_every(i: &mut Interp, this: Value, a: &[Value], want: bool) -> Result<Value, Value> {
    let f = arg(a, 0);
    if !f.is_callable() {
        // A non-callable predicate still closes the underlying iterator.
        i.iterator_close(&this);
        return Err(i.make_error("TypeError", "predicate is not callable"));
    }
    let mut k = 0.0;
    while let Some(v) = step_iter(i, &this)? {
        let r = match i.call(f.clone(), Value::Undefined, &[v, Value::Num(k)]) {
            Ok(r) => r,
            Err(e) => {
                i.iterator_close(&this);
                return Err(crate::interpreter::abrupt_value(e));
            }
        };
        if i.to_boolean(&r) == want {
            ab(i.iterator_close_normal(&this))?;
            return Ok(Value::Bool(want));
        }
        k += 1.0;
    }
    Ok(Value::Bool(!want))
}

/// Step an iterator object (`this`) once: `Some(value)` or `None` when done.
fn step_iter(i: &mut Interp, src: &Value) -> Result<Option<Value>, Value> {
    let next = ab(i.get_member(src, "next"))?;
    step_iter_with(i, src, &next)
}

/// Advance an iterator using an already-captured `next` method (iterator helpers read `next` exactly
/// once at creation per GetIteratorDirect, then reuse it).
fn step_iter_with(i: &mut Interp, src: &Value, next: &Value) -> Result<Option<Value>, Value> {
    if !next.is_callable() {
        return Err(i.make_error("TypeError", "iterator.next is not a function"));
    }
    let res = ab(i.call(next.clone(), src.clone(), &[]))?;
    if !matches!(res, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "iterator result is not an object"));
    }
    let done = ab(i.get_member(&res, "done"))?;
    if i.to_boolean(&done) {
        Ok(None)
    } else {
        Ok(Some(ab(i.get_member(&res, "value"))?))
    }
}

/// Build a lazy iterator-helper (map/filter/take/drop/flatMap) wrapping `source`.
fn make_iter_helper(i: &mut Interp, source: Value, kind: &str, f: Value) -> Result<Value, Value> {
    if matches!(kind, "map" | "filter" | "flatMap") && !f.is_callable() {
        // A non-callable mapper/predicate still closes the underlying iterator.
        i.iterator_close(&source);
        return Err(i.make_error("TypeError", "Iterator helper argument is not callable"));
    }
    // take/drop validate the limit (ToNumber → NaN or negative is a RangeError) BEFORE
    // GetIteratorDirect, per the spec's operation order.
    let limit = if matches!(kind, "take" | "drop") {
        let raw = ab(i.to_number(&f))?;
        if raw.is_nan() || raw < 0.0 {
            return Err(i.make_error("RangeError", "limit must be a non-negative number"));
        }
        Some(raw.trunc())
    } else {
        None
    };
    let proto = i.extra_protos.get("%IteratorPrototype%").cloned();
    let obj = Object::new(proto);
    // GetIteratorDirect: read the source's `next` method exactly once, now.
    let next = ab(i.get_member(&source, "next"))?;
    set_builtin(&obj, "__ih_next", next);
    set_builtin(&obj, "__ih_src", source);
    set_builtin(&obj, "__ih_kind", Value::str(kind));
    set_builtin(&obj, "__ih_fn", f.clone());
    if let Some(n) = limit {
        set_builtin(&obj, "__ih_n", Value::Num(n));
        set_builtin(&obj, "__ih_started", Value::Bool(false));
    }
    set_builtin(&obj, "__ih_count", Value::Num(0.0));
    set_builtin(&obj, "__ih_done", Value::Bool(false));
    i.def_method(&obj, "next", 0, iter_helper_next);
    // The helper's `return` closes the underlying iterator once (IteratorHelperPrototype %return%).
    i.def_method(&obj, "return", 0, |i, this, _a| {
        let done = ab(i.get_member(&this, "__ih_done"))?;
        if !i.to_boolean(&done) {
            set_internal(this.as_obj().unwrap(), "__ih_done", Value::Bool(true));
            let src = ab(i.get_member(&this, "__ih_src"))?;
            // A normal return() propagates an error from the source's return method.
            ab(i.iterator_close_normal(&src))?;
        }
        Ok(iter_result(i, Value::Undefined, true))
    });
    if let Some(sym) = i.iterator_sym.clone() {
        let itf = i.make_native("[Symbol.iterator]", 0, return_this);
        obj.borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(itf)));
    }
    Ok(Value::Obj(obj))
}

fn iter_result(i: &mut Interp, value: Value, done: bool) -> Value {
    let o = i.new_object();
    set_data(&o, "value", value);
    set_data(&o, "done", Value::Bool(done));
    Value::Obj(o)
}

/// `Iterator.zip` (keyed = `Iterator.zipKeyed`): open every input iterator eagerly and step them in
/// lockstep, combining the values per the `mode` (shortest/longest/strict) and `padding`.
fn iterator_zip(i: &mut Interp, a: &[Value], keyed: bool) -> Result<Value, Value> {
    let input = arg(a, 0);
    if !matches!(input, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "Iterator.zip input is not an object"));
    }
    let options = arg(a, 1);
    let mode = match &options {
        Value::Undefined => "shortest".to_string(),
        Value::Obj(_) => {
            let m = ab(i.get_member(&options, "mode"))?;
            match m {
                Value::Undefined => "shortest".to_string(),
                _ => ab(i.to_string(&m))?.to_string(),
            }
        }
        _ => return Err(i.make_error("TypeError", "Iterator.zip options is not an object")),
    };
    if !matches!(mode.as_str(), "shortest" | "longest" | "strict") {
        return Err(i.make_error("RangeError", "invalid Iterator.zip mode"));
    }
    // The padding option is read (but not yet iterated) before the inputs are opened.
    let padding_value = if mode == "longest" && matches!(&options, Value::Obj(_)) {
        ab(i.get_member(&options, "padding"))?
    } else {
        Value::Undefined
    };
    // Open the inputs in order, GetIteratorFlattenable each as it is read. An error opening one
    // closes the input iterator and every iterator already opened.
    let mut keys: Vec<String> = Vec::new();
    let mut iters: Vec<Value> = Vec::new();
    let mut nexts: Vec<Value> = Vec::new();
    let open_one = |i: &mut Interp,
                    v: &Value,
                    iters: &mut Vec<Value>,
                    nexts: &mut Vec<Value>|
     -> Result<(), Value> {
        let iter = get_iterator_flattenable(i, v, false)?;
        let next = ab(i.get_member(&iter, "next"))?;
        iters.push(iter);
        nexts.push(next);
        Ok(())
    };
    if keyed {
        // zipKeyed: each own enumerable key's value is an input iterable.
        if let Value::Obj(o) = &input {
            for k in ordered_enum_keys(o) {
                let v = ab(i.get_member(&input, &k))?;
                if let Err(e) = open_one(i, &v, &mut iters, &mut nexts) {
                    for it in &iters {
                        i.iterator_close(it);
                    }
                    return Err(e);
                }
                keys.push(k.to_string());
            }
        }
    } else {
        // zip: step the iterable-of-iterables lazily, opening each input as it is produced.
        let (input_iter, input_next) = ab(i.get_iterator(&input))?;
        loop {
            match step_iter_with(i, &input_iter, &input_next) {
                Ok(None) => break,
                Ok(Some(v)) => {
                    if let Err(e) = open_one(i, &v, &mut iters, &mut nexts) {
                        i.iterator_close(&input_iter);
                        for it in &iters {
                            i.iterator_close(it);
                        }
                        return Err(e);
                    }
                }
                Err(e) => {
                    for it in &iters {
                        i.iterator_close(it);
                    }
                    return Err(e);
                }
            }
        }
    }
    // `longest` padding: an array aligned with the iterators, filled from the pre-read padding value.
    let padding = if mode == "longest" {
        let mut pad = vec![Value::Undefined; iters.len()];
        if !matches!(padding_value, Value::Undefined) {
            let vals = ab(i.iterate(&padding_value))?;
            for (j, v) in vals.into_iter().enumerate().take(iters.len()) {
                pad[j] = v;
            }
        }
        pad
    } else {
        Vec::new()
    };

    let n_iters = iters.len();
    let obj = Object::new(i.extra_protos.get("%IteratorPrototype%").cloned());
    set_builtin(&obj, "__zip_iters", i.make_array(iters));
    set_builtin(&obj, "__zip_nexts", i.make_array(nexts));
    set_builtin(
        &obj,
        "__zip_done",
        i.make_array(vec![Value::Bool(false); n_iters]),
    );
    set_builtin(&obj, "__zip_mode", Value::from_string(mode.clone()));
    set_builtin(&obj, "__zip_pad", i.make_array(padding));
    set_builtin(&obj, "__zip_finished", Value::Bool(false));
    if keyed {
        let karr = i.make_array(keys.into_iter().map(Value::from_string).collect());
        set_builtin(&obj, "__zip_keys", karr);
    }
    i.def_method(&obj, "next", 0, zip_next);
    // The zip iterator's return() closes every still-open underlying iterator (first error wins).
    i.def_method(&obj, "return", 0, |i, this, _a| {
        let finished = ab(i.get_member(&this, "__zip_finished"))?;
        if !i.to_boolean(&finished) {
            set_internal(this.as_obj().unwrap(), "__zip_finished", Value::Bool(true));
            let iters = ab(i.get_member(&this, "__zip_iters"))?;
            let done = ab(i.get_member(&this, "__zip_done"))?;
            let n = match &iters {
                Value::Obj(o) => i.array_length(o),
                _ => 0,
            };
            zip_close_others(i, &iters, &done, n, usize::MAX, true)?;
        }
        Ok(iter_result(i, Value::Undefined, true))
    });
    if let Some(sym) = i.iterator_sym.clone() {
        let itf = i.make_native("[Symbol.iterator]", 0, return_this);
        obj.borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(itf)));
    }
    Ok(Value::Obj(obj))
}

/// Close every still-open zip iterator except `except`, marking them done (used when the zip finishes
/// or errors mid-round).
fn zip_close_others(
    i: &mut Interp,
    iters: &Value,
    done: &Value,
    n: usize,
    except: usize,
    propagate: bool,
) -> Result<(), Value> {
    for k in 0..n {
        if k == except {
            continue;
        }
        let dk = ab(i.get_member(done, &k.to_string()))?;
        if !i.to_boolean(&dk) {
            let it = ab(i.get_member(iters, &k.to_string()))?;
            ab(i.set_member(done, &k.to_string(), Value::Bool(true)))?;
            // On a normal completion the first close error propagates; on a throw completion (an
            // earlier error already pending) close errors are swallowed.
            if propagate {
                ab(i.iterator_close_normal(&it))?;
            } else {
                i.iterator_close(&it);
            }
        }
    }
    Ok(())
}

fn zip_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let finished = ab(i.get_member(&this, "__zip_finished"))?;
    if i.to_boolean(&finished) {
        return Ok(iter_result(i, Value::Undefined, true));
    }
    let iters = ab(i.get_member(&this, "__zip_iters"))?;
    let nexts = ab(i.get_member(&this, "__zip_nexts"))?;
    let done = ab(i.get_member(&this, "__zip_done"))?;
    let pad = ab(i.get_member(&this, "__zip_pad"))?;
    let mode_v = ab(i.get_member(&this, "__zip_mode"))?;
    let mode = ab(i.to_string(&mode_v))?.to_string();
    let n = match &iters {
        Value::Obj(o) => i.array_length(o),
        _ => 0,
    };
    if n == 0 {
        set_internal(this.as_obj().unwrap(), "__zip_finished", Value::Bool(true));
        return Ok(iter_result(i, Value::Undefined, true));
    }
    let mut values = vec![Value::Undefined; n];
    let mut all_done = true;
    // `first_live` records the first iterator's outcome (for strict-mode mismatch detection).
    let mut first_live: Option<bool> = None;
    for j in 0..n {
        let dj = ab(i.get_member(&done, &j.to_string()))?;
        if i.to_boolean(&dj) {
            values[j] = ab(i.get_member(&pad, &j.to_string()))?;
            continue;
        }
        let it = ab(i.get_member(&iters, &j.to_string()))?;
        let nx = ab(i.get_member(&nexts, &j.to_string()))?;
        let stepped = step_iter_with(i, &it, &nx);
        match stepped {
            Ok(Some(v)) => {
                all_done = false;
                values[j] = v;
                if first_live.is_none() {
                    first_live = Some(true);
                }
                // strict: a live value after a finished iterator is a length mismatch.
                if mode == "strict" && first_live == Some(false) {
                    zip_close_others(i, &iters, &done, n, j, true)?;
                    set_internal(this.as_obj().unwrap(), "__zip_finished", Value::Bool(true));
                    return Err(i.make_error("TypeError", "Iterator.zip strict: length mismatch"));
                }
            }
            Ok(None) => {
                ab(i.set_member(&done, &j.to_string(), Value::Bool(true)))?;
                if first_live.is_none() {
                    first_live = Some(false);
                }
                match mode.as_str() {
                    "shortest" => {
                        zip_close_others(i, &iters, &done, n, j, true)?;
                        set_internal(this.as_obj().unwrap(), "__zip_finished", Value::Bool(true));
                        return Ok(iter_result(i, Value::Undefined, true));
                    }
                    "strict" => {
                        // A finished iterator after a live one is a mismatch.
                        if first_live == Some(true) {
                            zip_close_others(i, &iters, &done, n, j, true)?;
                            set_internal(
                                this.as_obj().unwrap(),
                                "__zip_finished",
                                Value::Bool(true),
                            );
                            return Err(
                                i.make_error("TypeError", "Iterator.zip strict: length mismatch")
                            );
                        }
                        values[j] = ab(i.get_member(&pad, &j.to_string()))?;
                    }
                    _ => {
                        values[j] = ab(i.get_member(&pad, &j.to_string()))?;
                    }
                }
            }
            Err(e) => {
                zip_close_others(i, &iters, &done, n, j, false)?;
                set_internal(this.as_obj().unwrap(), "__zip_finished", Value::Bool(true));
                return Err(e);
            }
        }
    }
    // In longest/strict, the round is over only when every iterator is exhausted.
    if all_done {
        set_internal(this.as_obj().unwrap(), "__zip_finished", Value::Bool(true));
        return Ok(iter_result(i, Value::Undefined, true));
    }
    let result = if ab(i.get_member(&this, "__zip_keys")).is_ok()
        && !matches!(ab(i.get_member(&this, "__zip_keys"))?, Value::Undefined)
    {
        let keys = ab(i.get_member(&this, "__zip_keys"))?;
        let o = i.new_object();
        for j in 0..n {
            let k = ab(i.get_member(&keys, &j.to_string()))?;
            let k = ab(i.to_string(&k))?.to_string();
            set_data(&o, &k, values[j].clone());
        }
        Value::Obj(o)
    } else {
        i.make_array(values)
    };
    Ok(iter_result(i, result, false))
}

/// `Iterator.concat`'s iterator: opens each captured iterable in order and yields its values.
fn concat_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let done = ab(i.get_member(&this, "__cc_done"))?;
    if i.to_boolean(&done) {
        return Ok(iter_result(i, Value::Undefined, true));
    }
    loop {
        let cur = ab(i.get_member(&this, "__cc_cur"))?;
        if matches!(cur, Value::Undefined) {
            // Open the next item's iterator (or finish).
            let idx = ab(i.get_member(&this, "__cc_idx"))?;
            let idx = ab(i.to_number(&idx))? as usize;
            let methods = ab(i.get_member(&this, "__cc_methods"))?;
            let mlen = match &methods {
                Value::Obj(o) => i.array_length(o),
                _ => 0,
            };
            if idx >= mlen {
                return Ok(iter_result(i, Value::Undefined, true));
            }
            let items = ab(i.get_member(&this, "__cc_items"))?;
            let item = ab(i.get_member(&items, &idx.to_string()))?;
            let method = ab(i.get_member(&methods, &idx.to_string()))?;
            let iter = ab(i.call(method, item, &[]))?;
            if !matches!(iter, Value::Obj(_)) {
                return Err(i.make_error("TypeError", "@@iterator did not return an object"));
            }
            let next = ab(i.get_member(&iter, "next"))?;
            set_internal(this.as_obj().unwrap(), "__cc_cur", iter);
            set_internal(this.as_obj().unwrap(), "__cc_curnext", next);
            set_internal(
                this.as_obj().unwrap(),
                "__cc_idx",
                Value::Num((idx + 1) as f64),
            );
        }
        let cur = ab(i.get_member(&this, "__cc_cur"))?;
        let next = ab(i.get_member(&this, "__cc_curnext"))?;
        match step_iter_with(i, &cur, &next)? {
            Some(v) => return Ok(iter_result(i, v, false)),
            None => set_internal(this.as_obj().unwrap(), "__cc_cur", Value::Undefined),
        }
    }
}

/// GetIteratorFlattenable returning the iterator object: a string (when allowed) or an object's
/// @@iterator, or the object itself when it has no @@iterator (it's already an iterator).
fn get_iterator_flattenable(
    i: &mut Interp,
    v: &Value,
    allow_strings: bool,
) -> Result<Value, Value> {
    if !(matches!(v, Value::Obj(_)) || (allow_strings && matches!(v, Value::Str(_)))) {
        return Err(i.make_error("TypeError", "value is not iterable"));
    }
    let iter_key = i
        .iterator_sym
        .clone()
        .map(|s| Interp::sym_key(&s))
        .unwrap_or_default();
    let itf = ab(i.get_member(v, &iter_key))?;
    let iter = if matches!(itf, Value::Undefined | Value::Null) {
        v.clone()
    } else if itf.is_callable() {
        ab(i.call(itf, v.clone(), &[]))?
    } else {
        return Err(i.make_error("TypeError", "@@iterator is not callable"));
    };
    if !matches!(iter, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "iterator is not an object"));
    }
    Ok(iter)
}

/// GetIteratorFlattenable(obj, reject-primitives) then drain it: a primitive is a TypeError; an
/// object's @@iterator is used if present, else the object itself is treated as the iterator.
fn flatmap_flatten(i: &mut Interp, mapped: &Value) -> Result<Vec<Value>, Value> {
    if !matches!(mapped, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "flatMap mapper must return an object"));
    }
    let iter_key = i
        .iterator_sym
        .clone()
        .map(|s| Interp::sym_key(&s))
        .unwrap_or_default();
    let itf = ab(i.get_member(mapped, &iter_key))?;
    let iter = if matches!(itf, Value::Undefined | Value::Null) {
        mapped.clone() // already an iterator
    } else if itf.is_callable() {
        ab(i.call(itf, mapped.clone(), &[]))?
    } else {
        return Err(i.make_error("TypeError", "@@iterator is not callable"));
    };
    let next = ab(i.get_member(&iter, "next"))?;
    if !next.is_callable() {
        return Err(i.make_error("TypeError", "iterator has no next method"));
    }
    let mut out = Vec::new();
    while let Some(v) = step_iter_with(i, &iter, &next)? {
        out.push(v);
    }
    Ok(out)
}

fn iter_helper_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    // A helper that has already finished stays done (and never re-touches the source).
    let done = ab(i.get_member(&this, "__ih_done"))?;
    if i.to_boolean(&done) {
        return Ok(iter_result(i, Value::Undefined, true));
    }
    let src = ab(i.get_member(&this, "__ih_src"))?;
    let inext = ab(i.get_member(&this, "__ih_next"))?;
    let kind_v = ab(i.get_member(&this, "__ih_kind"))?;
    let kind = ab(i.to_string(&kind_v))?;
    let f = ab(i.get_member(&this, "__ih_fn"))?;
    let count_v = ab(i.get_member(&this, "__ih_count"))?;
    let count = ab(i.to_number(&count_v))?;
    match &*kind {
        "map" => match step_iter_with(i, &src, &inext)? {
            None => Ok(iter_result(i, Value::Undefined, true)),
            Some(v) => {
                let mv = match i.call(f, Value::Undefined, &[v, Value::Num(count)]) {
                    Ok(r) => r,
                    Err(e) => {
                        i.iterator_close(&src);
                        return Err(crate::interpreter::abrupt_value(e));
                    }
                };
                set_internal(
                    this.as_obj().unwrap(),
                    "__ih_count",
                    Value::Num(count + 1.0),
                );
                Ok(iter_result(i, mv, false))
            }
        },
        "filter" => {
            let mut k = count;
            loop {
                match step_iter_with(i, &src, &inext)? {
                    None => return Ok(iter_result(i, Value::Undefined, true)),
                    Some(v) => {
                        let r = match i.call(
                            f.clone(),
                            Value::Undefined,
                            &[v.clone(), Value::Num(k)],
                        ) {
                            Ok(r) => r,
                            Err(e) => {
                                i.iterator_close(&src);
                                return Err(crate::interpreter::abrupt_value(e));
                            }
                        };
                        k += 1.0;
                        if i.to_boolean(&r) {
                            set_internal(this.as_obj().unwrap(), "__ih_count", Value::Num(k));
                            return Ok(iter_result(i, v, false));
                        }
                    }
                }
            }
        }
        "take" => {
            let nv = ab(i.get_member(&this, "__ih_n"))?;
            let n = ab(i.to_number(&nv))?;
            if count >= n {
                set_internal(this.as_obj().unwrap(), "__ih_done", Value::Bool(true));
                ab(i.iterator_close_normal(&src))?;
                return Ok(iter_result(i, Value::Undefined, true));
            }
            set_internal(
                this.as_obj().unwrap(),
                "__ih_count",
                Value::Num(count + 1.0),
            );
            match step_iter_with(i, &src, &inext)? {
                None => Ok(iter_result(i, Value::Undefined, true)),
                Some(v) => Ok(iter_result(i, v, false)),
            }
        }
        "drop" => {
            let started_v = ab(i.get_member(&this, "__ih_started"))?;
            let started = i.to_boolean(&started_v);
            if !started {
                let nv = ab(i.get_member(&this, "__ih_n"))?;
                let n = ab(i.to_number(&nv))? as usize;
                for _ in 0..n {
                    if step_iter_with(i, &src, &inext)?.is_none() {
                        break;
                    }
                }
                set_internal(this.as_obj().unwrap(), "__ih_started", Value::Bool(true));
            }
            match step_iter_with(i, &src, &inext)? {
                None => Ok(iter_result(i, Value::Undefined, true)),
                Some(v) => Ok(iter_result(i, v, false)),
            }
        }
        "flatMap" => {
            let mut c = count;
            loop {
                // Drain the current inner buffer first.
                let buf = ab(i.get_member(&this, "__ih_buf"))?;
                if matches!(buf, Value::Obj(_)) {
                    let bi_v = ab(i.get_member(&this, "__ih_bi"))?;
                    let bi = ab(i.to_number(&bi_v))? as usize;
                    let len_v = ab(i.get_member(&buf, "length"))?;
                    let len = ab(i.to_number(&len_v))? as usize;
                    if bi < len {
                        let v = ab(i.get_member(&buf, &bi.to_string()))?;
                        set_internal(
                            this.as_obj().unwrap(),
                            "__ih_bi",
                            Value::Num((bi + 1) as f64),
                        );
                        return Ok(iter_result(i, v, false));
                    }
                }
                // Refill from the source: map a value to an iterable and flatten it.
                match step_iter_with(i, &src, &inext)? {
                    None => return Ok(iter_result(i, Value::Undefined, true)),
                    Some(v) => {
                        let mapped = match i.call(f.clone(), Value::Undefined, &[v, Value::Num(c)])
                        {
                            Ok(m) => m,
                            Err(e) => {
                                i.iterator_close(&src);
                                return Err(crate::interpreter::abrupt_value(e));
                            }
                        };
                        c += 1.0;
                        set_internal(this.as_obj().unwrap(), "__ih_count", Value::Num(c));
                        // GetIteratorFlattenable (reject primitives): an @@iterator-bearing iterable,
                        // or the object itself if it's already an iterator.
                        let inner = match flatmap_flatten(i, &mapped) {
                            Ok(v) => v,
                            Err(e) => {
                                i.iterator_close(&src);
                                return Err(e);
                            }
                        };
                        let arr = i.make_array(inner);
                        set_internal(this.as_obj().unwrap(), "__ih_buf", arr);
                        set_internal(this.as_obj().unwrap(), "__ih_bi", Value::Num(0.0));
                    }
                }
            }
        }
        _ => Ok(iter_result(i, Value::Undefined, true)),
    }
}

/// Build an Array Iterator over `target`. `kind`: 0 = values, 1 = keys, 2 = [key, value] entries.
/// State lives in non-enumerable internal slots so `next` can advance it.
fn make_array_iterator(i: &mut Interp, target: Value, kind: u8) -> Value {
    let proto = i
        .extra_protos
        .get("%ArrayIteratorPrototype%")
        .cloned()
        .or_else(|| i.extra_protos.get("%IteratorPrototype%").cloned());
    let obj = Object::new(proto);
    set_builtin(&obj, "__ai_target", target);
    set_builtin(&obj, "__ai_index", Value::Num(0.0));
    set_builtin(&obj, "__ai_kind", Value::Num(kind as f64));
    i.def_method(&obj, "next", 0, array_iter_next);
    if let Some(sym) = i.iterator_sym.clone() {
        let f = i.make_native("[Symbol.iterator]", 0, return_this);
        obj.borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(f)));
    }
    Value::Obj(obj)
}

/// `next()` for a generator object built by `make_generator`: walk the buffered values, then throw
/// any stored error, then return `{ value: <return>, done: true }`.
/// Drive a generator's coroutine with a resume signal, returning its `{value, done}` (or propagating
/// a throw). The generators map doubles as the brand check.
fn drive_generator(
    i: &mut Interp,
    this: &Value,
    signal: crate::coroutine::Resume,
) -> Result<Value, Value> {
    use crate::coroutine::{Resume, Suspend};
    let key = match this {
        Value::Obj(o) => Rc::as_ptr(o) as usize,
        _ => return Err(i.make_error("TypeError", "Generator method called on a non-generator")),
    };
    let mut coro = match i.generators.remove(&key) {
        Some(c) => c,
        None => return Err(i.make_error("TypeError", "Generator method called on a non-generator")),
    };
    if coro.done {
        i.generators.insert(key, coro);
        return match signal {
            Resume::Throw(e) => Err(e),
            Resume::Return(v) => Ok(iter_result(i, v, true)),
            Resume::Next(_) => Ok(iter_result(i, Value::Undefined, true)),
        };
    }
    let suspend = coro.resume(i, signal);
    i.generators.insert(key, coro);
    match suspend {
        Suspend::Yield(v) => Ok(iter_result(i, v, false)),
        Suspend::Done(v) => Ok(iter_result(i, v, true)),
        Suspend::Throw(e) => Err(e),
        Suspend::Await(_) => Err(i.make_error("TypeError", "await in a non-async generator")),
    }
}

/// Microtask reaction: the awaited promise fulfilled with `args[1]`; resume the async coroutine
/// (whose result promise is `args[0]`, also the coroutine key).
pub(crate) fn async_react_fulfil(
    i: &mut Interp,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    if let Value::Obj(o) = arg(args, 0) {
        i.drive_async(
            Rc::as_ptr(&o) as usize,
            arg(args, 0),
            crate::coroutine::Resume::Next(arg(args, 1)),
        );
    }
    Ok(Value::Undefined)
}
/// Microtask reaction: the awaited promise rejected with `args[1]`; throw it at the suspended await.
pub(crate) fn async_react_reject(
    i: &mut Interp,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    if let Value::Obj(o) = arg(args, 0) {
        i.drive_async(
            Rc::as_ptr(&o) as usize,
            arg(args, 0),
            crate::coroutine::Resume::Throw(arg(args, 1)),
        );
    }
    Ok(Value::Undefined)
}

pub(crate) fn generator_next(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    drive_generator(i, &this, crate::coroutine::Resume::Next(arg(args, 0)))
}

/// `gen.return(v)`: inject a return completion at the suspended `yield` (runs any `finally`).
pub(crate) fn generator_return(
    i: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    drive_generator(i, &this, crate::coroutine::Resume::Return(arg(args, 0)))
}

/// `gen.throw(e)`: inject a throw at the suspended `yield`.
pub(crate) fn generator_throw(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    drive_generator(i, &this, crate::coroutine::Resume::Throw(arg(args, 0)))
}
fn async_gen_drive(
    i: &mut Interp,
    this: &Value,
    signal: crate::coroutine::Resume,
) -> Result<Value, Value> {
    let r = i.new_promise();
    match this {
        Value::Obj(o) => i.drive_async_gen(Rc::as_ptr(o) as usize, r.clone(), signal),
        _ => {
            let e = i.make_error("TypeError", "AsyncGenerator method called on a non-object");
            i.reject_promise(&r, e);
        }
    }
    Ok(r)
}
pub(crate) fn async_generator_next(
    i: &mut Interp,
    this: Value,
    a: &[Value],
) -> Result<Value, Value> {
    async_gen_drive(i, &this, crate::coroutine::Resume::Next(arg(a, 0)))
}
pub(crate) fn async_generator_return(
    i: &mut Interp,
    this: Value,
    a: &[Value],
) -> Result<Value, Value> {
    async_gen_drive(i, &this, crate::coroutine::Resume::Return(arg(a, 0)))
}
pub(crate) fn async_generator_throw(
    i: &mut Interp,
    this: Value,
    a: &[Value],
) -> Result<Value, Value> {
    async_gen_drive(i, &this, crate::coroutine::Resume::Throw(arg(a, 0)))
}
/// Microtask reactions that re-drive an async generator when an awaited value settles. `args` is
/// `[keyMarker, resultPromise, settledValue]`.
pub(crate) fn async_gen_react_fulfil(
    i: &mut Interp,
    _t: Value,
    a: &[Value],
) -> Result<Value, Value> {
    let key = match arg(a, 0) {
        Value::Num(n) => n as usize,
        _ => return Ok(Value::Undefined),
    };
    i.drive_async_gen(key, arg(a, 1), crate::coroutine::Resume::Next(arg(a, 2)));
    Ok(Value::Undefined)
}
pub(crate) fn async_gen_react_reject(
    i: &mut Interp,
    _t: Value,
    a: &[Value],
) -> Result<Value, Value> {
    let key = match arg(a, 0) {
        Value::Num(n) => n as usize,
        _ => return Ok(Value::Undefined),
    };
    i.drive_async_gen(key, arg(a, 1), crate::coroutine::Resume::Throw(arg(a, 2)));
    Ok(Value::Undefined)
}
pub(crate) fn return_this_pub(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    return_this(i, this, a)
}
pub(crate) fn async_iterator_key(i: &Interp) -> Option<String> {
    well_known_key(i, "asyncIterator")
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
        Value::Undefined | Value::Null => Err(i.make_error(
            "TypeError",
            "String.prototype method called on null or undefined",
        )),
        _ => ab(i.to_string(this)),
    }
}

/// JS WhiteSpace + LineTerminator (includes U+FEFF, which Rust's char::is_whitespace omits).
fn is_js_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

/// IsRegExp(arg): true if it has a truthy `@@match`, or (fallback) is a compiled RegExp object.
fn arg_is_regexp(i: &mut Interp, v: &Value) -> Result<bool, Value> {
    if let Value::Obj(o) = v {
        if let Some(k) = well_known_key(i, "match") {
            let m = ab(i.get_member(v, &k))?;
            if !matches!(m, Value::Undefined | Value::Null) {
                return Ok(i.to_boolean(&m));
            }
        }
        return Ok(i.regexps.contains_key(&(Rc::as_ptr(o) as usize)));
    }
    Ok(false)
}

/// ToIntegerOrInfinity of an optional position argument, clamped to `[0, len]`.
fn str_clamp_pos(i: &mut Interp, v: Option<&Value>, len: i64) -> Result<usize, Value> {
    let n = match v {
        Some(v) if !matches!(v, Value::Undefined) => ab(i.to_number(v))?,
        _ => 0.0,
    };
    let n = if n.is_nan() { 0 } else { n.trunc() as i64 };
    Ok(n.clamp(0, len) as usize)
}

/// CreateHTML (Annex B): wrap the coerced `this` string in `<tag attr="value">…</tag>`.
fn create_html(
    i: &mut Interp,
    this: Value,
    tag: &str,
    attr: &str,
    value: &Value,
) -> Result<Value, Value> {
    let s = this_string(i, &this)?;
    let mut p = format!("<{tag}");
    if !attr.is_empty() {
        let v = ab(i.to_string(value))?;
        p.push_str(&format!(" {attr}=\"{}\"", v.replace('"', "&quot;")));
    }
    Ok(Value::from_string(format!("{p}>{s}</{tag}>")))
}

fn install_string(it: &mut Interp) {
    let sp = it.string_proto.clone();
    // String.prototype[@@iterator]: iterate by code point (via an array iterator over the chars).
    if let Some(sym) = it.iterator_sym.clone() {
        let f = it.make_native("[Symbol.iterator]", 0, |i, this, _| {
            let s = this_string(i, &this)?;
            let chars: Vec<Value> = s
                .chars()
                .map(|c| Value::from_string(c.to_string()))
                .collect();
            let arr = i.make_array(chars);
            let key = Interp::sym_key(i.iterator_sym.as_ref().unwrap());
            let itfn = ab(i.get_member(&arr, &key))?;
            ab(i.call(itfn, arr, &[]))
        });
        sp.borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(Value::Obj(f)));
    }
    it.def_method(&sp, "toString", 0, |i, this, _| {
        Ok(Value::Str(this_string(i, &this)?))
    });
    it.def_method(&sp, "valueOf", 0, |i, this, _| {
        Ok(Value::Str(this_string(i, &this)?))
    });
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
        let chars: Vec<char> = s.chars().collect();
        let nchars: Vec<char> = needle.chars().collect();
        let len = chars.len() as i64;
        let pos = str_clamp_pos(i, args.get(1), len)?;
        let nlen = nchars.len();
        let result = (pos..=chars.len())
            .find(|&start| start + nlen <= chars.len() && chars[start..start + nlen] == nchars[..]);
        Ok(Value::Num(result.map(|r| r as f64).unwrap_or(-1.0)))
    });
    it.def_method(&sp, "lastIndexOf", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let needle = ab(i.to_string(&arg(args, 0)))?;
        // Optional `position`: search for the last occurrence starting at or before it.
        let chars: Vec<char> = s.chars().collect();
        let nchars: Vec<char> = needle.chars().collect();
        let limit = match arg(args, 1) {
            Value::Undefined => chars.len(),
            v => {
                let p = ab(i.to_number(&v))?;
                if p.is_nan() {
                    chars.len()
                } else {
                    (p.max(0.0) as usize).min(chars.len())
                }
            }
        };
        let mut found = -1.0;
        let mut start = 0;
        while start + nchars.len() <= chars.len() && start <= limit {
            if chars[start..start + nchars.len()] == nchars[..] {
                found = start as f64;
            }
            start += 1;
        }
        Ok(Value::Num(found))
    });
    it.def_method(&sp, "toLocaleLowerCase", 0, |i, this, _| {
        Ok(Value::from_string(this_string(i, &this)?.to_lowercase()))
    });
    it.def_method(&sp, "toLocaleUpperCase", 0, |i, this, _| {
        Ok(Value::from_string(this_string(i, &this)?.to_uppercase()))
    });
    it.def_method(&sp, "includes", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        if arg_is_regexp(i, &arg(args, 0))? {
            return Err(i.make_error("TypeError", "argument must not be a regular expression"));
        }
        let needle = ab(i.to_string(&arg(args, 0)))?;
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len() as i64;
        let pos = str_clamp_pos(i, args.get(1), len)?;
        let hay: String = chars[pos..].iter().collect();
        Ok(Value::Bool(hay.contains(needle.as_ref())))
    });
    it.def_method(&sp, "startsWith", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        if arg_is_regexp(i, &arg(args, 0))? {
            return Err(i.make_error("TypeError", "argument must not be a regular expression"));
        }
        let needle = ab(i.to_string(&arg(args, 0)))?;
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len() as i64;
        let pos = str_clamp_pos(i, args.get(1), len)?;
        let hay: String = chars[pos..].iter().collect();
        Ok(Value::Bool(hay.starts_with(needle.as_ref())))
    });
    it.def_method(&sp, "endsWith", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        if arg_is_regexp(i, &arg(args, 0))? {
            return Err(i.make_error("TypeError", "argument must not be a regular expression"));
        }
        let needle = ab(i.to_string(&arg(args, 0)))?;
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len() as i64;
        // endsWith's optional argument is the END position (default = length).
        let end = match args.get(1) {
            Some(v) if !matches!(v, Value::Undefined) => str_clamp_pos(i, Some(v), len)?,
            _ => len as usize,
        };
        let hay: String = chars[..end].iter().collect();
        Ok(Value::Bool(hay.ends_with(needle.as_ref())))
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
        Ok(Value::from_string(
            chars[a as usize..b as usize].iter().collect::<String>(),
        ))
    });
    // Annex B B.2.3.1 String.prototype.substr(start, length).
    it.def_method(&sp, "substr", 2, |i, this, args| {
        let s = this_string(i, &this)?;
        let chars: Vec<char> = s.chars().collect();
        let size = chars.len() as i64;
        let n = ab(i.to_number(&arg(args, 0)))?;
        let mut start = if n.is_nan() {
            0
        } else if n < 0.0 {
            (size + n as i64).max(0)
        } else {
            (n as i64).min(size)
        };
        let len = match arg(args, 1) {
            Value::Undefined => size,
            v => {
                let l = ab(i.to_number(&v))?;
                if l.is_nan() {
                    0
                } else {
                    (l as i64).max(0)
                }
            }
        };
        let count = len.min(size - start).max(0);
        if count <= 0 {
            return Ok(Value::from_string(String::new()));
        }
        if start < 0 {
            start = 0;
        }
        Ok(Value::from_string(
            chars[start as usize..(start + count) as usize]
                .iter()
                .collect::<String>(),
        ))
    });
    // Annex B B.2.3 HTML-wrapper methods (CreateHTML): each wraps the string in a tag.
    it.def_method(&sp, "anchor", 1, |i, t, a| {
        create_html(i, t, "a", "name", &arg(a, 0))
    });
    it.def_method(&sp, "link", 1, |i, t, a| {
        create_html(i, t, "a", "href", &arg(a, 0))
    });
    it.def_method(&sp, "fontcolor", 1, |i, t, a| {
        create_html(i, t, "font", "color", &arg(a, 0))
    });
    it.def_method(&sp, "fontsize", 1, |i, t, a| {
        create_html(i, t, "font", "size", &arg(a, 0))
    });
    it.def_method(&sp, "big", 0, |i, t, _| {
        create_html(i, t, "big", "", &Value::Undefined)
    });
    it.def_method(&sp, "blink", 0, |i, t, _| {
        create_html(i, t, "blink", "", &Value::Undefined)
    });
    it.def_method(&sp, "bold", 0, |i, t, _| {
        create_html(i, t, "b", "", &Value::Undefined)
    });
    it.def_method(&sp, "fixed", 0, |i, t, _| {
        create_html(i, t, "tt", "", &Value::Undefined)
    });
    it.def_method(&sp, "italics", 0, |i, t, _| {
        create_html(i, t, "i", "", &Value::Undefined)
    });
    it.def_method(&sp, "small", 0, |i, t, _| {
        create_html(i, t, "small", "", &Value::Undefined)
    });
    it.def_method(&sp, "strike", 0, |i, t, _| {
        create_html(i, t, "strike", "", &Value::Undefined)
    });
    it.def_method(&sp, "sub", 0, |i, t, _| {
        create_html(i, t, "sub", "", &Value::Undefined)
    });
    it.def_method(&sp, "sup", 0, |i, t, _| {
        create_html(i, t, "sup", "", &Value::Undefined)
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
    it.def_method(&sp, "toWellFormed", 0, |i, this, _| {
        Ok(Value::Str(this_string(i, &this)?))
    });
    it.def_method(&sp, "trim", 0, |i, this, _| {
        Ok(Value::from_string(
            this_string(i, &this)?.trim_matches(is_js_ws).to_string(),
        ))
    });
    it.def_method(&sp, "localeCompare", 1, |i, this, args| {
        let a = this_string(i, &this)?;
        let b = ab(i.to_string(&arg(args, 0)))?;
        Ok(Value::Num(match (*a).cmp(&*b) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }))
    });
    it.def_method(&sp, "toLocaleString", 0, |i, this, _| {
        Ok(Value::Str(this_string(i, &this)?))
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
        // `limit` (ToUint32) caps the number of pieces; 0 → empty result.
        let limit = match arg(args, 1) {
            Value::Undefined => u32::MAX as usize,
            v => {
                let n = ab(i.to_number(&v))?;
                (if n.is_finite() { n as i64 as u32 } else { 0 }) as usize
            }
        };
        if limit == 0 {
            return Ok(i.make_array(Vec::new()));
        }
        // Regex separator: split on each match (group captures are inserted between pieces).
        if let Value::Obj(o) = &arg(args, 0) {
            if i.regexps.contains_key(&(Rc::as_ptr(o) as usize)) {
                let re = i.regexps[&(Rc::as_ptr(o) as usize)].clone();
                let chars: Vec<char> = s.chars().collect();
                let mut parts = Vec::new();
                let mut last = 0;
                'outer: for caps in regex_find_all(&re, &chars) {
                    let (a, b) = caps[0].unwrap();
                    // Skip a zero-width match at the very start or end of the string.
                    if a == b && (b == 0 || a >= chars.len()) {
                        continue;
                    }
                    parts.push(Value::from_string(
                        chars[last..a].iter().collect::<String>(),
                    ));
                    if parts.len() >= limit {
                        break;
                    }
                    for g in 1..=re.ngroups {
                        parts.push(match caps[g] {
                            Some((x, y)) => {
                                Value::from_string(chars[x..y].iter().collect::<String>())
                            }
                            None => Value::Undefined,
                        });
                        if parts.len() >= limit {
                            break 'outer;
                        }
                    }
                    last = b;
                }
                if parts.len() < limit {
                    parts.push(Value::from_string(chars[last..].iter().collect::<String>()));
                }
                parts.truncate(limit);
                return Ok(i.make_array(parts));
            }
        }
        match arg(args, 0) {
            Value::Undefined => Ok(i.make_array(vec![Value::Str(s)])),
            sep => {
                let sep = ab(i.to_string(&sep))?;
                let mut parts: Vec<Value> = if sep.is_empty() {
                    s.chars()
                        .map(|c| Value::from_string(c.to_string()))
                        .collect()
                } else {
                    s.split(sep.as_ref())
                        .map(|p| Value::from_string(p.to_string()))
                        .collect()
                };
                parts.truncate(limit);
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
        Ok(Value::from_string(
            this_string(i, &this)?
                .trim_start_matches(is_js_ws)
                .to_string(),
        ))
    });
    it.def_method(&sp, "trimEnd", 0, |i, this, _| {
        Ok(Value::from_string(
            this_string(i, &this)?
                .trim_end_matches(is_js_ws)
                .to_string(),
        ))
    });
    it.def_method(&sp, "padStart", 1, |i, this, args| {
        string_pad(i, this, args, true)
    });
    it.def_method(&sp, "padEnd", 1, |i, this, args| {
        string_pad(i, this, args, false)
    });
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
        // If the argument is a RegExp, its flags must include "g" (else TypeError).
        if let Value::Obj(o) = &arg(a, 0) {
            if let Some(re) = i.regexps.get(&(Rc::as_ptr(o) as usize)) {
                if !re.flags.contains('g') {
                    return Err(i.make_error(
                        "TypeError",
                        "String.prototype.matchAll called with a non-global RegExp",
                    ));
                }
            }
        }
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
                Ok(Value::from_string(format!(
                    "{}{}{}",
                    &s[..pos],
                    rep,
                    &s[pos + pat.len()..]
                )))
            }
        }
    });
    it.def_method(&sp, "replaceAll", 2, |i, this, args| {
        let s = this_string(i, &this)?.to_string();
        let search = arg(args, 0);
        if let Value::Obj(_) = &search {
            // A RegExp search value must be global.
            if let Value::Obj(o) = &search {
                if let Some(re) = i.regexps.get(&(Rc::as_ptr(o) as usize)) {
                    if !re.global {
                        return Err(i.make_error(
                            "TypeError",
                            "replaceAll must be called with a global RegExp",
                        ));
                    }
                }
            }
            // Delegate to the search value's @@replace method when present (handles custom objects).
            if let Some(key) = well_known_key(i, "replace") {
                let replacer = ab(i.get_member(&search, &key))?;
                if replacer.is_callable() {
                    return ab(i.call(
                        replacer,
                        search.clone(),
                        &[Value::from_string(s.clone()), arg(args, 1)],
                    ));
                }
            }
        }
        let pat = ab(i.to_string(&arg(args, 0)))?;
        let repl = arg(args, 1);
        if pat.is_empty() {
            // An empty search matches at every position: insert the replacement between each char.
            let mut out = String::new();
            let mut byte = 0usize;
            for ch in s.chars() {
                out.push_str(&string_replacement(i, &repl, "", &s, byte)?);
                out.push(ch);
                byte += ch.len_utf8();
            }
            out.push_str(&string_replacement(i, &repl, "", &s, byte)?);
            return Ok(Value::from_string(out));
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
            Some(Value::Sym(s)) if !i.constructing => Value::from_string(format!(
                "Symbol({})",
                s.description.as_deref().unwrap_or("")
            )),
            Some(v) => Value::Str(ab(i.to_string(v))?),
        };
        Ok(maybe_box(i, s))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(sp.clone()), false, false, false),
    );
    sp.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "fromCharCode", 1, |i, _this, args| {
        let mut s = String::new();
        for a in args {
            let n = ab(i.to_number(a))? as u32;
            s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
        }
        Ok(Value::from_string(s))
    });
    it.def_method(&ctor, "raw", 1, |i, _this, args| {
        let template = arg(args, 0);
        let raw = ab(i.get_member(&template, "raw"))?;
        let raw_obj =
            this_obj(&raw).ok_or_else(|| i.make_error("TypeError", "raw is not an object"))?;
        let len = ab(i.to_length(&raw_obj))?;
        let mut out = String::new();
        for k in 0..len {
            let seg = ab(i.get_member(&raw, &k.to_string()))?;
            out.push_str(&ab(i.to_string(&seg))?);
            if k + 1 < len {
                if let Some(sub) = args.get(k + 1) {
                    out.push_str(&ab(i.to_string(sub))?);
                }
            }
        }
        Ok(Value::from_string(out))
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
        Value::Sym(s) => (i.symbol_proto.clone(), Exotic::SymWrap(s.clone())),
        Value::BigInt(n) => match i.extra_protos.get("BigInt").cloned() {
            Some(p) => (p, Exotic::BigIntWrap(*n)),
            None => return v,
        },
        _ => return v,
    };
    let obj = Object::new(Some(proto));
    // A String exotic object exposes each character as an own, enumerable, non-writable,
    // non-configurable index property, plus a non-enumerable `length`.
    if let Exotic::StrWrap(s) = &exotic {
        let chars: Vec<char> = s.chars().collect();
        let mut b = obj.borrow_mut();
        for (idx, ch) in chars.iter().enumerate() {
            b.props.insert(
                idx.to_string().as_str(),
                Property::data(Value::from_string(ch.to_string()), false, true, false),
            );
        }
        b.props.insert(
            "length",
            Property::data(Value::Num(chars.len() as f64), false, false, false),
        );
    }
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
    Ok(Value::from_string(if at_start {
        format!("{fill}{s}")
    } else {
        format!("{s}{fill}")
    }))
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
            &[
                Value::from_string(matched.to_string()),
                Value::Num(pos as f64),
                Value::from_string(whole.to_string()),
            ],
        ))?;
        Ok(ab(i.to_string(&r))?.to_string())
    } else {
        let template = ab(i.to_string(repl))?;
        // GetSubstitution for a string match: $$ → $, $& → match, $` → preceding, $' → following.
        // (No captures, so $n and $<name> stay literal.)
        let before = &whole[..pos.min(whole.len())];
        let after = whole.get(pos + matched.len()..).unwrap_or("");
        let tchars: Vec<char> = template.chars().collect();
        let mut out = String::new();
        let mut k = 0;
        while k < tchars.len() {
            if tchars[k] == '$' && k + 1 < tchars.len() {
                match tchars[k + 1] {
                    '$' => {
                        out.push('$');
                        k += 2;
                        continue;
                    }
                    '&' => {
                        out.push_str(matched);
                        k += 2;
                        continue;
                    }
                    '`' => {
                        out.push_str(before);
                        k += 2;
                        continue;
                    }
                    '\'' => {
                        out.push_str(after);
                        k += 2;
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(tchars[k]);
            k += 1;
        }
        Ok(out)
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
    it.def_method(&np, "toLocaleString", 0, |i, this, _| {
        let n = this_number(i, &this)?;
        Ok(Value::from_string(i.num_to_str(n)))
    });
    it.def_method(&np, "toString", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        let radix = match arg(args, 0) {
            Value::Undefined => 10.0,
            v => {
                let r = ab(i.to_number(&v))?;
                if r.is_nan() {
                    0.0
                } else {
                    r.trunc()
                }
            }
        };
        if !(2.0..=36.0).contains(&radix) {
            return Err(i.make_error("RangeError", "toString() radix must be between 2 and 36"));
        }
        if radix == 10.0 {
            Ok(Value::from_string(i.num_to_str(n)))
        } else {
            Ok(Value::from_string(to_radix_string(n, radix as u32)))
        }
    });
    it.def_method(&np, "valueOf", 0, |i, this, _| {
        Ok(Value::Num(this_number(i, &this)?))
    });
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
        Ok(Value::from_string(
            s.replace('e', "e+").replace("e+-", "e-"),
        ))
    });
    it.def_method(&np, "toPrecision", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        if matches!(arg(args, 0), Value::Undefined) {
            return Ok(Value::from_string(i.num_to_str(n)));
        }
        let p = ab(i.to_number(&arg(args, 0)))?;
        if n.is_nan() {
            return Ok(Value::str("NaN"));
        }
        if n.is_infinite() {
            return Ok(Value::from_string(i.num_to_str(n)));
        }
        if !(1.0..=100.0).contains(&p) {
            return Err(i.make_error(
                "RangeError",
                "toPrecision() argument must be between 1 and 100",
            ));
        }
        Ok(Value::from_string(to_precision(n, p as usize)))
    });
    it.def_method(&np, "toFixed", 1, |i, this, args| {
        let n = this_number(i, &this)?;
        // ToIntegerOrInfinity(fractionDigits): undefined/NaN → 0, otherwise truncate toward zero.
        let raw = ab(i.to_number(&arg(args, 0)))?;
        let d = if raw.is_nan() { 0.0 } else { raw.trunc() };
        // Spec: fractionDigits in 0..=100, else RangeError (also guards a giant `format!`).
        if !(0.0..=100.0).contains(&d) {
            return Err(i.make_error(
                "RangeError",
                "toFixed() digits argument must be between 0 and 100",
            ));
        }
        if n.is_nan() {
            return Ok(Value::str("NaN"));
        }
        // For magnitudes ≥ 1e21 toFixed falls back to Number::toString.
        if n.abs() >= 1e21 {
            return Ok(Value::from_string(i.num_to_str(n)));
        }
        let digits = d as usize;
        // The sign is `-` only for a strictly-negative value (not -0), and the magnitude is rounded.
        let body = format!("{:.*}", digits, n.abs());
        Ok(Value::from_string(if n < 0.0 {
            format!("-{body}")
        } else {
            body
        }))
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
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(np.clone()), false, false, false),
    );
    np.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    // The numeric constants are { writable:false, enumerable:false, configurable:false }.
    for (name, val) in [
        ("MAX_SAFE_INTEGER", 9007199254740991.0),
        ("MIN_SAFE_INTEGER", -9007199254740991.0),
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", f64::MIN_POSITIVE),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
        ("EPSILON", f64::EPSILON),
    ] {
        ctor.borrow_mut()
            .props
            .insert(name, Property::data(Value::Num(val), false, false, false));
    }
    it.def_method(&ctor, "isNaN", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_nan()),
        ))
    });
    it.def_method(&ctor, "isFinite", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite()),
        ))
    });
    it.def_method(&ctor, "isSafeInteger", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9007199254740991.0),
        ))
    });
    it.def_method(&ctor, "isInteger", 1, |_i, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0),
        ))
    });
    set_builtin(&it.global, "Number", Value::Obj(ctor));
}

fn to_radix_string(n: f64, radix: u32) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let neg = n < 0.0;
    let x = n.abs();
    let mut int = x.trunc() as u64;
    // Integer part (most-significant digit first after reversal).
    let mut ipart = Vec::new();
    if int == 0 {
        ipart.push(b'0');
    }
    while int > 0 {
        ipart.push(digits[(int % radix as u64) as usize]);
        int /= radix as u64;
    }
    ipart.reverse();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&ipart).unwrap());
    // Fractional part: repeatedly multiply by the radix, emitting the integer digit each step.
    let mut frac = x.fract();
    if frac > 0.0 {
        out.push('.');
        let mut count = 0;
        while frac > 0.0 && count < 52 {
            frac *= radix as f64;
            let d = frac.trunc() as usize;
            out.push(digits[d] as char);
            frac -= d as f64;
            count += 1;
        }
    }
    out
}

fn install_boolean(it: &mut Interp) {
    let bp = it.boolean_proto.clone();
    it.def_method(&bp, "toString", 0, |i, this, _| {
        Ok(Value::str(if this_boolean(i, &this)? {
            "true"
        } else {
            "false"
        }))
    });
    it.def_method(&bp, "valueOf", 0, |i, this, _| {
        Ok(Value::Bool(this_boolean(i, &this)?))
    });
    let ctor = it.make_native("Boolean", 1, |i, _this, args| {
        let b = Value::Bool(i.to_boolean(&arg(args, 0)));
        Ok(maybe_box(i, b))
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(bp.clone()), false, false, false),
    );
    bp.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_builtin(&it.global, "Boolean", Value::Obj(ctor));
}

/// thisBooleanValue: a Boolean primitive or Boolean wrapper, else TypeError.
fn this_boolean(i: &mut Interp, this: &Value) -> Result<bool, Value> {
    match this {
        Value::Bool(b) => Ok(*b),
        Value::Obj(o) => match o.borrow().exotic {
            Exotic::BoolWrap(b) => Ok(b),
            _ => Err(i.make_error(
                "TypeError",
                "Boolean method called on incompatible receiver",
            )),
        },
        _ => Err(i.make_error(
            "TypeError",
            "Boolean method called on incompatible receiver",
        )),
    }
}

/// thisSymbolValue: a Symbol primitive or Symbol wrapper object, else TypeError.
fn this_symbol(i: &mut Interp, this: &Value) -> Result<Rc<SymbolData>, Value> {
    match this {
        Value::Sym(s) => Ok(s.clone()),
        Value::Obj(o) => match &o.borrow().exotic {
            Exotic::SymWrap(s) => Ok(s.clone()),
            _ => Err(i.make_error("TypeError", "Symbol method called on incompatible receiver")),
        },
        _ => Err(i.make_error("TypeError", "Symbol method called on incompatible receiver")),
    }
}

fn install_symbol(it: &mut Interp) {
    let sp = it.symbol_proto.clone();
    it.def_method(&sp, "toString", 0, |i, this, _| {
        match this_symbol(i, &this) {
            Ok(s) => Ok(Value::from_string(format!(
                "Symbol({})",
                s.description.as_deref().unwrap_or("")
            ))),
            Err(e) => Err(e),
        }
    });
    it.def_method(&sp, "valueOf", 0, |i, this, _| {
        match this_symbol(i, &this) {
            Ok(s) => Ok(Value::Sym(s)),
            Err(e) => Err(e),
        }
    });

    let desc_getter = it.make_native("get description", 0, |i, this, _| {
        match this_symbol(i, &this) {
            Ok(s) => Ok(s
                .description
                .as_deref()
                .map(|d| Value::from_string(d.to_string()))
                .unwrap_or(Value::Undefined)),
            _ => Err(i.make_error(
                "TypeError",
                "Symbol.prototype.description requires a symbol",
            )),
        }
    });
    sp.borrow_mut().props.insert(
        "description",
        Property {
            value: Value::Undefined,
            get: Some(Value::Obj(desc_getter)),
            set: None,
            accessor: true,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );

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
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(sp.clone()), false, false, false),
    );
    sp.borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));

    // Well-known symbols (each a unique, frozen instance on the constructor).
    for name in [
        "iterator",
        "asyncIterator",
        "hasInstance",
        "isConcatSpreadable",
        "match",
        "matchAll",
        "replace",
        "search",
        "species",
        "split",
        "toPrimitive",
        "toStringTag",
        "unscopables",
        "dispose",
        "asyncDispose",
        "metadata",
    ] {
        let sym = it.new_symbol(Some(Rc::from(format!("Symbol.{name}").as_str())));
        if name == "iterator" {
            if let Value::Sym(d) = &sym {
                it.iterator_sym = Some(d.clone());
            }
        }
        ctor.borrow_mut()
            .props
            .insert(name, Property::data(sym, false, false, false));
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

    // These need the well-known symbols, which are only reachable via the global Symbol now installed.
    // Symbol.prototype[@@toPrimitive](hint) returns thisSymbolValue; { writable:false }.
    let to_prim = it.make_native("[Symbol.toPrimitive]", 1, |i, this, _| {
        Ok(Value::Sym(this_symbol(i, &this)?))
    });
    if let Some(key) = well_known_key(it, "toPrimitive") {
        sp.borrow_mut()
            .props
            .insert(key, Property::data(Value::Obj(to_prim), false, false, true));
    }
    // Symbol.prototype[@@toStringTag] = "Symbol"; { writable:false }.
    if let Some(key) = well_known_key(it, "toStringTag") {
        sp.borrow_mut().props.insert(
            key,
            Property::data(Value::from_string("Symbol".to_string()), false, false, true),
        );
    }
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

/// StringToBigInt: parse a StringIntegerLiteral (decimal with optional sign, or 0x/0o/0b radix
/// prefixes), with leading/trailing whitespace trimmed. Returns None when the string is not a valid
/// literal (the caller raises SyntaxError).
fn string_to_bigint(s: &str) -> Option<i128> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
    if t.is_empty() {
        return Some(0);
    }
    let (radix, body) = if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, r)
    } else if let Some(r) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        (8, r)
    } else if let Some(r) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        (2, r)
    } else {
        // Decimal, with an optional single leading sign.
        let (neg, digits) = match t.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v = digits.parse::<i128>().ok()?;
        return Some(if neg { -v } else { v });
    };
    if body.is_empty() || !body.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    i128::from_str_radix(body, radix).ok()
}

/// ToBigInt: coerce a value to a BigInt primitive (i128), following the spec's allowed conversions.
fn to_bigint(i: &mut Interp, v: &Value) -> Result<i128, Value> {
    let prim = ab(i.to_primitive(v, crate::eval::Hint::Number))?;
    match prim {
        Value::BigInt(n) => Ok(n),
        Value::Bool(b) => Ok(if b { 1 } else { 0 }),
        Value::Str(ref s) => string_to_bigint(s)
            .ok_or_else(|| i.make_error("SyntaxError", "Cannot convert string to a BigInt")),
        Value::Num(_) => Err(i.make_error("TypeError", "Cannot convert a Number to a BigInt")),
        Value::Sym(_) => Err(i.make_error("TypeError", "Cannot convert a Symbol to a BigInt")),
        _ => Err(i.make_error("TypeError", "Cannot convert value to a BigInt")),
    }
}

/// thisBigIntValue: a BigInt primitive or BigInt wrapper object, else TypeError.
fn this_bigint(i: &mut Interp, this: &Value) -> Result<i128, Value> {
    match this {
        Value::BigInt(n) => Ok(*n),
        Value::Obj(o) => match o.borrow().exotic {
            Exotic::BigIntWrap(n) => Ok(n),
            _ => Err(i.make_error("TypeError", "BigInt method called on incompatible receiver")),
        },
        _ => Err(i.make_error("TypeError", "BigInt method called on incompatible receiver")),
    }
}

fn install_bigint(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("BigInt", proto.clone());
    it.def_method(&proto, "toString", 1, |i, this, a| {
        let n = this_bigint(i, &this)?;
        let radix = match arg(a, 0) {
            Value::Undefined => 10,
            v => ab(i.to_number(&v))? as u32,
        };
        Ok(Value::from_string(bigint_to_radix(n, radix)))
    });
    it.def_method(&proto, "valueOf", 0, |i, this, _| {
        Ok(Value::BigInt(this_bigint(i, &this)?))
    });
    it.def_method(&proto, "toLocaleString", 0, |i, this, _| {
        Ok(Value::from_string(bigint_to_radix(
            this_bigint(i, &this)?,
            10,
        )))
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
            Value::Str(s) => string_to_bigint(&s)
                .map(Value::BigInt)
                .ok_or_else(|| i.make_error("SyntaxError", "Cannot convert string to a BigInt")),
            // BigInt(obj): ToPrimitive(number) then ToBigInt of the primitive.
            v @ Value::Obj(_) => Ok(Value::BigInt(to_bigint(i, &v)?)),
            _ => Err(i.make_error("TypeError", "Cannot convert value to a BigInt")),
        }
    });
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    it.def_method(&ctor, "asIntN", 2, |i, _t, a| {
        let bits = to_index(i, &arg(a, 0))? as u32;
        let n = to_bigint(i, &arg(a, 1))?;
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
        let bits = to_index(i, &arg(a, 0))? as u32;
        let n = to_bigint(i, &arg(a, 1))?;
        if bits == 0 {
            return Ok(Value::BigInt(0));
        }
        if bits >= 127 {
            return Ok(Value::BigInt(n));
        }
        Ok(Value::BigInt(n.rem_euclid(1i128 << bits)))
    });
    set_builtin(&it.global, "BigInt", Value::Obj(ctor));

    // BigInt.prototype[@@toStringTag] = "BigInt"; { writable:false, enumerable:false, configurable:true }.
    if let Some(key) = well_known_key(it, "toStringTag") {
        proto.borrow_mut().props.insert(
            key,
            Property::data(Value::from_string("BigInt".to_string()), false, false, true),
        );
    }
}

/// Correctly-rounded sum of finite f64s, via Shewchuk's nonoverlapping-partials algorithm with
/// CPython's final round-half-to-even step (the `math.fsum` algorithm).
fn fsum_exact(values: &[f64]) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    for &xi in values {
        let mut x = xi;
        let mut i = 0;
        for j in 0..partials.len() {
            let mut y = partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        partials.truncate(i);
        partials.push(x);
    }
    let n = partials.len();
    if n == 0 {
        return 0.0;
    }
    let mut hi = partials[n - 1];
    let mut lo = 0.0;
    let mut idx = n - 1;
    while idx > 0 {
        idx -= 1;
        let x = hi;
        let y = partials[idx];
        hi = x + y;
        lo = y - (hi - x);
        if lo != 0.0 {
            break;
        }
    }
    // Round half to even: nudge when the residual and the next partial agree in sign.
    if idx > 0 && ((lo < 0.0 && partials[idx - 1] < 0.0) || (lo > 0.0 && partials[idx - 1] > 0.0)) {
        let y = lo * 2.0;
        let x = hi + y;
        if y == x - hi {
            hi = x;
        }
    }
    hi
}

fn install_math(it: &mut Interp) {
    let math = it.new_object();
    // The Math constants are { writable:false, enumerable:false, configurable:false }.
    for (name, val) in [
        ("E", std::f64::consts::E),
        ("LN10", std::f64::consts::LN_10),
        ("LN2", std::f64::consts::LN_2),
        ("LOG10E", std::f64::consts::LOG10_E),
        ("LOG2E", std::f64::consts::LOG2_E),
        ("PI", std::f64::consts::PI),
        ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
        ("SQRT2", std::f64::consts::SQRT_2),
    ] {
        math.borrow_mut()
            .props
            .insert(name, Property::data(Value::Num(val), false, false, false));
    }
    // Math[@@toStringTag] = "Math" (non-writable, non-enumerable, configurable).
    if let Some(key) = well_known_key(it, "toStringTag") {
        math.borrow_mut()
            .props
            .insert(key, Property::data(Value::str("Math"), false, false, true));
    }
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
    unary!("sign", |x: f64| if x.is_nan() || x == 0.0 {
        x
    } else {
        x.signum()
    });
    unary!("expm1", f64::exp_m1);
    unary!("log1p", f64::ln_1p);
    unary!("sinh", f64::sinh);
    unary!("cosh", f64::cosh);
    unary!("tanh", f64::tanh);
    unary!("asinh", f64::asinh);
    unary!("acosh", f64::acosh);
    unary!("atanh", f64::atanh);
    unary!("fround", |x: f64| x as f32 as f64);
    unary!(
        "f16round",
        |x: f64| crate::value::f16_to_f32(crate::value::f32_to_f16(x as f32)) as f64
    );
    unary!("clz32", |x: f64| (to_uint32(x)).leading_zeros() as f64);
    it.def_method(&math, "sumPrecise", 1, |i, _t, a| {
        // Iterate the argument, requiring every element to be a Number; compute the correctly
        // rounded sum. Infinities dominate (mixed signs → NaN), any NaN → NaN, empty → -0.
        let (iter, next) = ab(i.get_iterator(&arg(a, 0)))?;
        let mut finite: Vec<f64> = Vec::new();
        let (mut pos_inf, mut neg_inf, mut nan) = (false, false, false);
        loop {
            let item = match ab(i.iterator_step(&iter, &next))? {
                Some(v) => v,
                None => break,
            };
            match item {
                Value::Num(n) => {
                    if n.is_nan() {
                        nan = true;
                    } else if n.is_infinite() {
                        if n > 0.0 {
                            pos_inf = true;
                        } else {
                            neg_inf = true;
                        }
                    } else {
                        finite.push(n);
                    }
                }
                _ => {
                    i.iterator_close(&iter);
                    return Err(i.make_error("TypeError", "Math.sumPrecise: not a number"));
                }
            }
        }
        let result = if pos_inf && neg_inf || nan {
            f64::NAN
        } else if pos_inf {
            f64::INFINITY
        } else if neg_inf {
            f64::NEG_INFINITY
        } else if finite.is_empty() {
            -0.0
        } else {
            fsum_exact(&finite)
        };
        Ok(Value::Num(result))
    });
    it.def_method(&math, "hypot", 2, |i, _t, a| {
        // Coerce every argument (in order), then: any infinite operand yields +Infinity (even
        // alongside a NaN), otherwise any NaN yields NaN, otherwise the Euclidean norm.
        let mut sum = 0.0;
        let mut any_inf = false;
        let mut any_nan = false;
        for v in a {
            let n = ab(i.to_number(v))?;
            if n.is_infinite() {
                any_inf = true;
            } else if n.is_nan() {
                any_nan = true;
            } else {
                sum += n * n;
            }
        }
        Ok(Value::Num(if any_inf {
            f64::INFINITY
        } else if any_nan {
            f64::NAN
        } else {
            sum.sqrt()
        }))
    });
    it.def_method(&math, "imul", 2, |i, _t, a| {
        let x = to_uint32(ab(i.to_number(&arg(a, 0)))?) as i32;
        let y = to_uint32(ab(i.to_number(&arg(a, 1)))?) as i32;
        Ok(Value::Num(x.wrapping_mul(y) as f64))
    });
    it.def_method(&math, "random", 0, |_i, _t, _a| {
        Ok(Value::Num(next_random()))
    });
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
        Ok(Value::Num(
            ab(i.to_number(&arg(a, 0)))?.powf(ab(i.to_number(&arg(a, 1)))?),
        ))
    });
    it.def_method(&math, "atan2", 2, |i, _t, a| {
        Ok(Value::Num(
            ab(i.to_number(&arg(a, 0)))?.atan2(ab(i.to_number(&arg(a, 1)))?),
        ))
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
    set_to_string_tag(it, &math, "Math");
    set_builtin(&it.global, "Math", Value::Obj(math));
}

fn install_errors(it: &mut Interp) {
    // Base Error first (its prototype's proto is Object.prototype).
    let names = [
        "Error",
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "EvalError",
        "URIError",
    ];
    // Create Error.prototype.
    let error_proto = Object::new(Some(it.object_proto.clone()));
    set_builtin(&error_proto, "name", Value::str("Error"));
    set_builtin(&error_proto, "message", Value::str(""));
    it.def_method(&error_proto, "toString", 0, |i, this, _| {
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error(
                "TypeError",
                "Error.prototype.toString called on a non-object",
            ));
        }
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
    // Error.prototype.stack accessor (error-stack-accessor proposal). lumen captures no stack trace,
    // so the getter yields "" for an Error receiver (undefined otherwise); the setter shadows it.
    // get stack: non-object → TypeError; an object without [[ErrorData]] → undefined; an Error
    // instance → an implementation-defined stack string (lumen captures no trace, so "").
    let get_stack = it.make_native("get stack", 0, |i, this, _| match &this {
        Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Error) => Ok(Value::str("")),
        Value::Obj(_) => Ok(Value::Undefined),
        _ => Err(i.make_error(
            "TypeError",
            "Error.prototype.stack getter called on a non-object",
        )),
    });
    // set stack: SetterThatIgnoresPrototypeProperties(this, %Error.prototype%, "stack", v).
    let set_stack = it.make_native("set stack", 1, |i, this, a| {
        let o = match &this {
            Value::Obj(o) => o.clone(),
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "Error.prototype.stack setter requires an object",
                ))
            }
        };
        // The value must be a String.
        let v = arg(a, 0);
        if !matches!(v, Value::Str(_)) {
            return Err(i.make_error(
                "TypeError",
                "Error.prototype.stack setter requires a string value",
            ));
        }
        // Setting on %Error.prototype% itself throws (it emulates a non-writable home property).
        if Rc::ptr_eq(&o, &i.error_protos["Error"]) {
            return Err(i.make_error("TypeError", "cannot set stack on %Error.prototype%"));
        }
        // [[GetOwnProperty]]("stack") (proxy-aware): does the receiver already have its own stack?
        let has_own = if let Some((t, h)) = proxy_pair(i, &this) {
            !matches!(proxy_gopd_value(i, &t, &h, "stack")?, Value::Undefined)
        } else {
            o.borrow().props.contains("stack")
        };
        if has_own {
            // Set(this, "stack", v) with Throw=true.
            if let Some((t, h)) = proxy_pair(i, &this) {
                // Surface the proxy [[Set]] result: a falsy trap return throws under Throw=true.
                let trap = ab(i.get_member(&h, "set"))?;
                if trap.is_callable() {
                    let res = ab(i.call(
                        trap,
                        h.clone(),
                        &[t.clone(), Value::str("stack"), v.clone(), this.clone()],
                    ))?;
                    if !i.to_boolean(&res) {
                        return Err(
                            i.make_error("TypeError", "proxy set of 'stack' returned false")
                        );
                    }
                } else {
                    ab(i.set_member(&t, "stack", v))?;
                }
            } else {
                assign_set(i, &this, "stack", v)?;
            }
        } else {
            // CreateDataPropertyOrThrow(this, "stack", v).
            let desc = i.new_object();
            set_data(&desc, "value", v);
            set_data(&desc, "writable", Value::Bool(true));
            set_data(&desc, "enumerable", Value::Bool(true));
            set_data(&desc, "configurable", Value::Bool(true));
            let ok = if let Some((t, h)) = proxy_pair(i, &this) {
                ab(proxy_define_property(i, &t, &h, "stack", &Value::Obj(desc)))?
            } else {
                ab(define_own_property(i, &o, "stack", &Value::Obj(desc)))?
            };
            if !ok {
                return Err(i.make_error("TypeError", "cannot define stack"));
            }
        }
        Ok(Value::Undefined)
    });
    error_proto.borrow_mut().props.insert(
        "stack",
        Property {
            value: Value::Undefined,
            get: Some(Value::Obj(get_stack)),
            set: Some(Value::Obj(set_stack)),
            accessor: true,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    it.error_protos.insert("Error", error_proto.clone());

    let mut error_ctor: Option<Gc> = None;
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
            "Error" => |i, _t, a| make_err(i, "Error", a),
            "TypeError" => |i, _t, a| make_err(i, "TypeError", a),
            "RangeError" => |i, _t, a| make_err(i, "RangeError", a),
            "ReferenceError" => |i, _t, a| make_err(i, "ReferenceError", a),
            "SyntaxError" => |i, _t, a| make_err(i, "SyntaxError", a),
            "EvalError" => |i, _t, a| make_err(i, "EvalError", a),
            "URIError" => |i, _t, a| make_err(i, "URIError", a),
            _ => unreachable!(),
        };
        let ctor = it.make_native(name, 1, ctor_fn);
        if name == "Error" {
            it.def_method(&ctor, "isError", 1, |_i, _t, a| {
                Ok(Value::Bool(
                    matches!(arg(a, 0), Value::Obj(o) if matches!(o.borrow().exotic, Exotic::Error)),
                ))
            });
            error_ctor = Some(ctor.clone());
        } else if let Some(ec) = &error_ctor {
            // A native error subtype's [[Prototype]] is the Error constructor (it subclasses Error).
            ctor.borrow_mut().proto = Some(ec.clone());
        }
        ctor.borrow_mut().props.insert(
            "prototype",
            Property::data(Value::Obj(proto.clone()), false, false, false),
        );
        proto
            .borrow_mut()
            .props
            .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
        set_builtin(&it.global, name, Value::Obj(ctor));
    }

    // AggregateError(errors, message): an Error subclass carrying an `errors` array.
    let agg_proto = Object::new(Some(error_proto.clone()));
    set_builtin(&agg_proto, "name", Value::str("AggregateError"));
    set_builtin(&agg_proto, "message", Value::str(""));
    it.error_protos.insert("AggregateError", agg_proto.clone());
    let agg_ctor = it.make_native("AggregateError", 2, |i, _t, a| {
        let err = i.make_error("AggregateError", "");
        // OrdinaryCreateFromConstructor: prototype from new.target (cross-realm aware).
        if matches!(i.new_target, Value::Obj(_)) {
            let nt = i.new_target.clone();
            let proto = match i.get_member(&nt, "prototype") {
                Ok(Value::Obj(p)) => Some(p),
                _ => ctor_realm_proto(i, &nt, "AggregateError")
                    .or_else(|| i.error_protos.get("AggregateError").cloned()),
            };
            if let (Value::Obj(o), Some(p)) = (&err, proto) {
                o.borrow_mut().proto = Some(p);
            }
        }
        // The spec order is: message, then cause, then the (iterated) errors list — last.
        if !matches!(arg(a, 1), Value::Undefined) {
            let s = ab(i.to_string(&arg(a, 1)))?;
            if let Some(o) = err.as_obj() {
                o.borrow_mut()
                    .props
                    .insert("message", Property::builtin(Value::Str(s)));
            }
        }
        install_error_cause(i, &err, a.get(2))?;
        let errors = ab(i.iterate(&arg(a, 0)))?;
        let arr = i.make_array(errors);
        // `errors` is { writable:true, enumerable:false, configurable:true }.
        if let Some(o) = err.as_obj() {
            o.borrow_mut()
                .props
                .insert("errors", Property::data(arr, true, false, true));
        }
        Ok(err)
    });
    if let Some(ec) = &error_ctor {
        agg_ctor.borrow_mut().proto = Some(ec.clone());
    }
    agg_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(agg_proto.clone()), false, false, false),
    );
    agg_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(agg_ctor.clone())),
    );
    set_builtin(&it.global, "AggregateError", Value::Obj(agg_ctor));

    // SuppressedError(error, suppressed, message): an Error subclass carrying `error` and
    // `suppressed` (the error thrown during disposal and the one it superseded).
    let sup_proto = Object::new(Some(error_proto.clone()));
    set_builtin(&sup_proto, "name", Value::str("SuppressedError"));
    set_builtin(&sup_proto, "message", Value::str(""));
    it.error_protos.insert("SuppressedError", sup_proto.clone());
    let sup_ctor = it.make_native("SuppressedError", 3, |i, _t, a| {
        let err = i.make_error("SuppressedError", "");
        // OrdinaryCreateFromConstructor: prototype from new.target (cross-realm aware).
        if matches!(i.new_target, Value::Obj(_)) {
            let nt = i.new_target.clone();
            let proto = match i.get_member(&nt, "prototype") {
                Ok(Value::Obj(p)) => Some(p),
                _ => ctor_realm_proto(i, &nt, "SuppressedError")
                    .or_else(|| i.error_protos.get("SuppressedError").cloned()),
            };
            if let (Value::Obj(o), Some(p)) = (&err, proto) {
                o.borrow_mut().proto = Some(p);
            }
        }
        if !matches!(arg(a, 2), Value::Undefined) {
            let s = ab(i.to_string(&arg(a, 2)))?;
            if let Some(o) = err.as_obj() {
                o.borrow_mut()
                    .props
                    .insert("message", Property::builtin(Value::Str(s)));
            }
        }
        // `error` and `suppressed` are non-enumerable, writable, configurable data properties.
        if let Some(o) = err.as_obj() {
            o.borrow_mut()
                .props
                .insert("error", Property::builtin(arg(a, 0)));
            o.borrow_mut()
                .props
                .insert("suppressed", Property::builtin(arg(a, 1)));
        }
        install_error_cause(i, &err, a.get(3))?;
        Ok(err)
    });
    sup_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(sup_proto.clone()), false, false, false),
    );
    sup_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(sup_ctor.clone())),
    );
    if let Some(ec) = &error_ctor {
        sup_ctor.borrow_mut().proto = Some(ec.clone());
    }
    set_builtin(&it.global, "SuppressedError", Value::Obj(sup_ctor));
}

fn make_err(i: &mut Interp, kind: &str, args: &[Value]) -> Result<Value, Value> {
    let err = i.make_error(kind, "");
    // OrdinaryCreateFromConstructor: a subclass / Reflect.construct new.target sets the prototype.
    if matches!(i.new_target, Value::Obj(_)) {
        let nt = i.new_target.clone();
        let proto = match i.get_member(&nt, "prototype") {
            Ok(Value::Obj(p)) => Some(p),
            _ => ctor_realm_proto(i, &nt, kind).or_else(|| i.error_protos.get(kind).cloned()),
        };
        if let (Value::Obj(o), Some(p)) = (&err, proto) {
            o.borrow_mut().proto = Some(p);
        }
    }
    if let Some(msg) = args.first() {
        if !matches!(msg, Value::Undefined) {
            // ToString(message) may throw (e.g. a Symbol, or a throwing toString) — propagate it.
            let s = ab(i.to_string(msg))?;
            // The own `message` is { writable:true, enumerable:false, configurable:true }.
            if let Some(e) = err.as_obj() {
                e.borrow_mut()
                    .props
                    .insert("message", Property::builtin(Value::Str(s)));
            }
        }
    }
    install_error_cause(i, &err, args.get(1))?;
    Ok(err)
}

/// InstallErrorCause: when `options` is an object with a `cause` property, copy it to a
/// non-enumerable own `cause` data property. Both HasProperty and Get are observable (proxy traps)
/// and propagate their throws.
fn install_error_cause(i: &mut Interp, err: &Value, options: Option<&Value>) -> Result<(), Value> {
    if let Some(opts @ Value::Obj(_)) = options {
        if ab(i.js_has_property(opts, "cause"))? {
            let cause = ab(i.get_member(opts, "cause"))?;
            if let Some(e) = err.as_obj() {
                e.borrow_mut()
                    .props
                    .insert("cause", Property::data(cause, true, false, true));
            }
        }
    }
    Ok(())
}

fn install_globals(it: &mut Interp) {
    // The test262 async harness ($DONE) reports completion via `print`; route it to the console.
    global_fn(it, "print", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        i.console.push(s.to_string());
        Ok(Value::Undefined)
    });
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
    if let Some(num) = it
        .global
        .borrow()
        .props
        .get("Number")
        .map(|p| p.value.clone())
    {
        let pi = it
            .global
            .borrow()
            .props
            .get("parseInt")
            .map(|p| p.value.clone());
        let pf = it
            .global
            .borrow()
            .props
            .get("parseFloat")
            .map(|p| p.value.clone());
        if let (Value::Obj(n), Some(pi), Some(pf)) = (num, pi, pf) {
            n.borrow_mut()
                .props
                .insert("parseInt", Property::builtin(pi));
            n.borrow_mut()
                .props
                .insert("parseFloat", Property::builtin(pf));
        }
    }
    global_fn(it, "isNaN", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_nan()))
    });
    global_fn(it, "isFinite", 1, |i, _t, a| {
        Ok(Value::Bool(ab(i.to_number(&arg(a, 0)))?.is_finite()))
    });
    // Annex B escape/unescape.
    global_fn(it, "escape", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        let mut out = String::new();
        for c in s.encode_utf16() {
            let ch = c as u32;
            let keep = c < 128 && {
                let a = ch as u8 as char;
                a.is_ascii_alphanumeric() || "@*_+-./".contains(a)
            };
            if keep {
                out.push(ch as u8 as char);
            } else if ch < 256 {
                out.push_str(&format!("%{ch:02X}"));
            } else {
                out.push_str(&format!("%u{ch:04X}"));
            }
        }
        Ok(Value::from_string(out))
    });
    global_fn(it, "unescape", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        let chars: Vec<char> = s.chars().collect();
        let mut units: Vec<u16> = Vec::new();
        let mut k = 0;
        while k < chars.len() {
            if chars[k] == '%' {
                if k + 5 < chars.len() + 1 && chars.get(k + 1) == Some(&'u') {
                    if let Some(h) = chars
                        .get(k + 2..k + 6)
                        .and_then(|s| u16::from_str_radix(&s.iter().collect::<String>(), 16).ok())
                    {
                        units.push(h);
                        k += 6;
                        continue;
                    }
                } else if let Some(h) = chars
                    .get(k + 1..k + 3)
                    .and_then(|s| u16::from_str_radix(&s.iter().collect::<String>(), 16).ok())
                {
                    units.push(h);
                    k += 3;
                    continue;
                }
            }
            units.push(chars[k] as u16);
            k += 1;
        }
        Ok(Value::from_string(String::from_utf16_lossy(&units)))
    });
    global_fn(it, "encodeURIComponent", 1, |i, _t, a| {
        Ok(Value::from_string(uri_encode(
            &ab(i.to_string(&arg(a, 0)))?,
            "",
        )))
    });
    global_fn(it, "encodeURI", 1, |i, _t, a| {
        Ok(Value::from_string(uri_encode(
            &ab(i.to_string(&arg(a, 0)))?,
            ";,/?:@&=+$#",
        )))
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
        let parts: Result<Vec<String>, Value> = a
            .iter()
            .map(|v| ab(i.to_string(v)).map(|s| s.to_string()))
            .collect();
        i.console.push(parts?.join(" "));
        Ok(Value::Undefined)
    };
    for name in ["log", "info", "warn", "error", "debug"] {
        it.def_method(&console, name, 0, log);
    }
    set_builtin(&it.global, "console", Value::Obj(console));
}

fn parse_int(s: &str, mut radix: u32) -> f64 {
    let t = s.trim_matches(is_js_ws);
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
    let t = s.trim_matches(is_js_ws);
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
        } else if (c == '+' || c == '-')
            && (end == 0 || matches!(bytes[end - 1] as char, 'e' | 'E'))
        {
        } else {
            break;
        }
        end += 1;
    }
    t[..end].parse::<f64>().unwrap_or(f64::NAN)
}
