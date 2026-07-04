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

/// Set(O, P, V, true): perform [[Set]] and throw a TypeError if it returns false, matching the
/// spec's `Set(..., Throw=true)` used by Array mutators regardless of the surrounding strict mode.
fn set_throw(i: &mut Interp, base: &Value, key: &str, value: Value) -> Result<(), Value> {
    let ok = ab(i.set_member_recv(base, key, value, base.clone()))?;
    if !ok {
        return Err(i.make_error(
            "TypeError",
            format!("Cannot assign to read only property '{key}'"),
        ));
    }
    Ok(())
}

/// ToDateTimeOptions default for toLocaleDateString/toLocaleTimeString: when `options` is undefined,
/// supply the date-only or time-only component defaults; otherwise pass the options through.
fn date_style_default(i: &mut Interp, options: &Value, date: bool) -> Result<Value, Value> {
    // ToDateTimeOptions(options, required, defaults) for toLocaleDateString(date)/toLocaleTimeString(time):
    // add the dimension's numeric defaults unless a component of that dimension (or a dateStyle/
    // timeStyle) is already present. The user's other-dimension components are kept (via the proto chain).
    let user_obj = match options {
        Value::Undefined => None,
        Value::Obj(o) => Some(o.clone()),
        _ => return Err(i.make_error("TypeError", "options must be an object")),
    };
    let dim: &[&str] = if date {
        &["weekday", "year", "month", "day"]
    } else {
        &[
            "dayPeriod",
            "hour",
            "minute",
            "second",
            "fractionalSecondDigits",
        ]
    };
    let mut need = true;
    if user_obj.is_some() {
        for k in dim.iter().chain(["dateStyle", "timeStyle"].iter()) {
            if !matches!(ab(i.get_member(options, k))?, Value::Undefined) {
                need = false;
                break;
            }
        }
    }
    let o = i.new_object();
    if let Some(uo) = &user_obj {
        o.borrow_mut().proto = Some(uo.clone());
    }
    if need {
        let defs: &[&str] = if date {
            &["year", "month", "day"]
        } else {
            &["hour", "minute", "second"]
        };
        for k in defs {
            set_data(&o, k, Value::str("numeric"));
        }
    }
    Ok(Value::Obj(o))
}

/// ToDateTimeOptions(options, "any", "all") for toLocaleString: add both date and time numeric
/// defaults unless the caller already requested a date/time component or a dateStyle/timeStyle.
fn date_all_default(i: &mut Interp, options: &Value) -> Result<Value, Value> {
    let user_obj = match options {
        Value::Undefined => None,
        Value::Obj(o) => Some(o.clone()),
        _ => return Err(i.make_error("TypeError", "options must be an object")),
    };
    let mut need = true;
    if user_obj.is_some() {
        for k in [
            "weekday",
            "year",
            "month",
            "day",
            "dayPeriod",
            "hour",
            "minute",
            "second",
            "fractionalSecondDigits",
            "dateStyle",
            "timeStyle",
        ] {
            if !matches!(ab(i.get_member(options, k))?, Value::Undefined) {
                need = false;
                break;
            }
        }
    }
    let o = i.new_object();
    if let Some(uo) = &user_obj {
        o.borrow_mut().proto = Some(uo.clone());
    }
    if need {
        for k in ["year", "month", "day", "hour", "minute", "second"] {
            set_data(&o, k, Value::str("numeric"));
        }
    }
    Ok(Value::Obj(o))
}

/// Construct `Intl.<service>(locales, options)` and invoke `method(args…)` on it. Used to route the
/// `toLocale*`/`localeCompare` methods through the Intl services now that they exist.
fn intl_delegate(
    i: &mut Interp,
    service: &str,
    locales: Value,
    options: Value,
    method: &str,
    call_args: &[Value],
) -> Result<Value, Value> {
    // Use the cached intrinsic (immune to tainted Intl globals), falling back to the global.
    let ctor = match i.extra_protos.get(format!("%Intl.{service}%").as_str()) {
        Some(c) => Value::Obj(c.clone()),
        None => {
            let intl = ab(i.get_member(&Value::Obj(i.global.clone()), "Intl"))?;
            ab(i.get_member(&intl, service))?
        }
    };
    let inst = ab(i.construct(ctor, &[locales, options]))?;
    let f = ab(i.get_member(&inst, method))?;
    ab(i.call(f, inst, call_args))
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
    // After Symbol so the intrinsics' @@toStringTag resolves.
    install_generator_function_ctors(it);
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
    crate::intl::install(it);
}

/// `DisposableStack` / `AsyncDisposableStack` (explicit resource management). Disposers are
/// stored as `[fn, thisArg(, adoptedValue)]` entries in an internal array and run LIFO on
/// `dispose()` / `disposeAsync()`; an async sentinel entry `[undefined]` awaits once.
/// The `__ds_kind` marker ("sync"/"async") is the brand.
fn ds_brand(i: &mut Interp, this: &Value, async_kind: bool) -> Result<Gc, Value> {
    let want = if async_kind { "async" } else { "sync" };
    let name = if async_kind {
        "AsyncDisposableStack"
    } else {
        "DisposableStack"
    };
    let o = this_obj(this)
        .ok_or_else(|| i.make_error("TypeError", format!("receiver is not a {name}")))?;
    let kind = o.borrow().props.get("__ds_kind").map(|p| p.value.clone());
    match kind {
        Some(Value::Str(k)) if &*k == want => Ok(o),
        _ => Err(i.make_error("TypeError", format!("receiver is not a {name}"))),
    }
}
fn ds_list(i: &mut Interp, this: &Value) -> Result<Value, Value> {
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

/// DisposeResources error folding: a later error suppresses the pending one.
fn ds_combine_error(i: &mut Interp, pending: Option<Value>, new: Value) -> Value {
    match pending {
        None => new,
        Some(prev) => {
            let ctor = i
                .global
                .borrow()
                .props
                .get("SuppressedError")
                .map(|p| p.value.clone())
                .unwrap_or(Value::Undefined);
            i.construct(ctor, &[new.clone(), prev]).unwrap_or(new)
        }
    }
}

/// One `[fn, thisArg(, adopted)]` entry's call. `Ok(None)` marks the async await-only sentinel.
fn ds_call_entry(i: &mut Interp, list: &Value, idx: i64) -> Result<Option<Value>, Value> {
    let entry = ab(i.get_member(list, &idx.to_string()))?;
    let f = ab(i.get_member(&entry, "0"))?;
    if !f.is_callable() {
        return Ok(None);
    }
    let t = ab(i.get_member(&entry, "1"))?;
    let len = ab(i.get_member(&entry, "length"))?;
    let args = if ab(i.to_number(&len))? >= 3.0 {
        vec![ab(i.get_member(&entry, "2"))?]
    } else {
        Vec::new()
    };
    ab(i.call(f, t, &args)).map(Some)
}

/// disposeAsync's chained state machine: run entries from `idx` down, awaiting each result via a
/// real microtask so job interleaving matches Await semantics.
fn adisp_run(i: &mut Interp, list: Value, mut idx: i64, result: Value, mut err: Option<Value>) {
    loop {
        if idx < 0 {
            match err {
                None => i.resolve_promise(&result, Value::Undefined),
                Some(e) => i.reject_promise(&result, e),
            }
            return;
        }
        // A sync throw skips the await and folds into the pending error.
        let awaited = match ds_call_entry(i, &list, idx) {
            Ok(Some(r)) => r,
            Ok(None) => Value::Undefined, // sentinel: Await(undefined)
            Err(e) => {
                err = Some(ds_combine_error(i, err, e));
                idx -= 1;
                continue;
            }
        };
        let px = match i.promise_resolve_checked(awaited) {
            Ok(p) => p,
            Err(e) => {
                err = Some(ds_combine_error(i, err, e));
                idx -= 1;
                continue;
            }
        };
        let on_f = adisp_reaction(i, &list, idx - 1, &result, &err, true);
        let on_r = adisp_reaction(i, &list, idx - 1, &result, &err, false);
        i.promise_then(&px, on_f, on_r);
        return;
    }
}

fn adisp_reaction(
    i: &mut Interp,
    list: &Value,
    idx: i64,
    result: &Value,
    err: &Option<Value>,
    fulfil: bool,
) -> Value {
    let target = i.make_native(
        "",
        1,
        if fulfil {
            adisp_react_fulfil
        } else {
            adisp_react_reject
        },
    );
    let bound = Object::new(Some(i.function_proto.clone()));
    bound.borrow_mut().call = Callable::Bound {
        target,
        this: Value::Undefined,
        args: vec![
            list.clone(),
            Value::Num(idx as f64),
            result.clone(),
            Value::Bool(err.is_some()),
            err.clone().unwrap_or(Value::Undefined),
        ],
    };
    Value::Obj(bound)
}
fn adisp_react_fulfil(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let idx = match arg(a, 1) {
        Value::Num(n) => n as i64,
        _ => return Ok(Value::Undefined),
    };
    let err = matches!(arg(a, 3), Value::Bool(true)).then(|| arg(a, 4));
    adisp_run(i, arg(a, 0), idx, arg(a, 2), err);
    Ok(Value::Undefined)
}
fn adisp_react_reject(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let idx = match arg(a, 1) {
        Value::Num(n) => n as i64,
        _ => return Ok(Value::Undefined),
    };
    let prev = matches!(arg(a, 3), Value::Bool(true)).then(|| arg(a, 4));
    let err = Some(ds_combine_error(i, prev, arg(a, 5)));
    adisp_run(i, arg(a, 0), idx, arg(a, 2), err);
    Ok(Value::Undefined)
}

/// Calls a wrapped sync `@@dispose` (bound args `[method, thisArg]`), discarding its result.
fn ds_sync_dispose_wrapper(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    ab(i.call(arg(a, 0), arg(a, 1), &[]))?;
    Ok(Value::Undefined)
}

fn install_disposable_stack(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("DisposableStack", proto.clone());
    set_to_string_tag(it, &proto, "DisposableStack");

    it.def_method(&proto, "use", 1, |i, this, a| {
        ds_brand(i, &this, false)?;
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
        ds_brand(i, &this, false)?;
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
        ds_brand(i, &this, false)?;
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
        ds_brand(i, &this, false)?;
        if ds_disposed(i, &this)? {
            return Ok(Value::Undefined);
        }
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        let list = ds_list(i, &this)?;
        let len = ab(i.get_member(&list, "length"))?;
        let n = ab(i.to_number(&len))? as i64;
        // Every disposer runs; a later error suppresses the pending one (SuppressedError).
        let mut err: Option<Value> = None;
        for idx in (0..n).rev() {
            if let Err(e) = ds_call_entry(i, &list, idx) {
                err = Some(ds_combine_error(i, err, e));
            }
        }
        match err {
            None => Ok(Value::Undefined),
            Some(e) => Err(e),
        }
    });
    it.def_method(&proto, "move", 0, |i, this, _| {
        ds_brand(i, &this, false)?;
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "DisposableStack already disposed"));
        }
        let proto = i.extra_protos.get("DisposableStack").cloned();
        let fresh = Object::new(proto);
        let list = ds_list(i, &this)?;
        set_internal(&fresh, "__ds", list);
        set_internal(&fresh, "__ds_kind", Value::str("sync"));
        set_internal(&fresh, "__ds_disposed", Value::Bool(false));
        let empty = i.make_array(Vec::new());
        set_internal(this.as_obj().unwrap(), "__ds", empty);
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        Ok(Value::Obj(fresh))
    });
    // `disposed` accessor + `[Symbol.dispose]` alias for `dispose`.
    let disposed_getter = it.make_native("get disposed", 0, |i, this, _| {
        ds_brand(i, &this, false)?;
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
        let obj = new_from_ctor(i, "DisposableStack")?;
        let list = i.make_array(Vec::new());
        set_internal(&obj, "__ds", list);
        set_internal(&obj, "__ds_kind", Value::str("sync"));
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

/// `AsyncDisposableStack`: like DisposableStack but disposers may be async; `disposeAsync` returns
/// a promise, awaiting each disposer result through real microtasks.
fn install_async_disposable_stack(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos
        .insert("AsyncDisposableStack", proto.clone());
    set_to_string_tag(it, &proto, "AsyncDisposableStack");

    it.def_method(&proto, "use", 1, |i, this, a| {
        ds_brand(i, &this, true)?;
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "AsyncDisposableStack already disposed"));
        }
        let v = arg(a, 0);
        if matches!(v, Value::Undefined | Value::Null) {
            // AddDisposableResource records an await-only sentinel for a nullish async resource.
            let entry = i.make_array(vec![Value::Undefined]);
            ds_push(i, &this, entry)?;
            return Ok(v);
        }
        // Prefer @@asyncDispose, falling back to @@dispose.
        let akey = well_known_key(i, "asyncDispose").unwrap_or_default();
        let mut disp = ab(i.get_member(&v, &akey))?;
        let from_async = disp.is_callable();
        if !from_async {
            let dkey = well_known_key(i, "dispose").unwrap_or_default();
            disp = ab(i.get_member(&v, &dkey))?;
        }
        if !disp.is_callable() {
            return Err(i.make_error("TypeError", "value is not async-disposable"));
        }
        // A sync @@dispose fallback has its result discarded — only Await(undefined) follows —
        // so wrap it rather than exposing the raw method to the awaiting disposal loop.
        let entry = if from_async {
            i.make_array(vec![disp, v.clone()])
        } else {
            let target = i.make_native("", 0, ds_sync_dispose_wrapper);
            let bound = Object::new(Some(i.function_proto.clone()));
            bound.borrow_mut().call = Callable::Bound {
                target,
                this: Value::Undefined,
                args: vec![disp, v.clone()],
            };
            i.make_array(vec![Value::Obj(bound), Value::Undefined])
        };
        ds_push(i, &this, entry)?;
        Ok(v)
    });
    it.def_method(&proto, "adopt", 2, |i, this, a| {
        ds_brand(i, &this, true)?;
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
        ds_brand(i, &this, true)?;
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
        // Brand failures reject the returned promise rather than throwing.
        if let Err(e) = ds_brand(i, &this, true) {
            i.reject_promise(&result, e);
            return Ok(result);
        }
        if ds_disposed(i, &this)? {
            i.resolve_promise(&result, Value::Undefined);
            return Ok(result);
        }
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        let list = ds_list(i, &this)?;
        let len = ab(i.get_member(&list, "length"))?;
        let n = ab(i.to_number(&len))? as i64;
        adisp_run(i, list, n - 1, result.clone(), None);
        Ok(result)
    });
    it.def_method(&proto, "move", 0, |i, this, _| {
        ds_brand(i, &this, true)?;
        if ds_disposed(i, &this)? {
            return Err(i.make_error("ReferenceError", "AsyncDisposableStack already disposed"));
        }
        let fresh = Object::new(i.extra_protos.get("AsyncDisposableStack").cloned());
        let list = ds_list(i, &this)?;
        set_internal(&fresh, "__ds", list);
        set_internal(&fresh, "__ds_kind", Value::str("async"));
        set_internal(&fresh, "__ds_disposed", Value::Bool(false));
        let empty = i.make_array(Vec::new());
        set_internal(this.as_obj().unwrap(), "__ds", empty);
        set_internal(this.as_obj().unwrap(), "__ds_disposed", Value::Bool(true));
        Ok(Value::Obj(fresh))
    });
    let disposed_getter = it.make_native("get disposed", 0, |i, this, _| {
        ds_brand(i, &this, true)?;
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
        let obj = new_from_ctor(i, "AsyncDisposableStack")?;
        let list = i.make_array(Vec::new());
        set_internal(&obj, "__ds", list);
        set_internal(&obj, "__ds_kind", Value::str("async"));
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
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("\u{0}weakref-target")) {
            return Err(i.make_error("TypeError", "deref called on a non-WeakRef"));
        }
        Ok(this
            .as_obj()
            .and_then(|o| {
                o.borrow()
                    .props
                    .get("\u{0}weakref-target")
                    .map(|p| p.value.clone())
            })
            .unwrap_or(Value::Undefined))
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
        // The key is \0-prefixed so it is invisible to every own-property enumeration path.
        set_internal(&obj, "\u{0}weakref-target", target);
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
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("\u{0}fr")) {
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
        // Track the registration so unregister can report whether anything matched. (Cleanup
        // callbacks never fire — nothing is collected during a run — but the cell bookkeeping
        // is still observable.)
        if !matches!(token, Value::Undefined) {
            if let Value::Obj(o) = &this {
                i.fr_tokens
                    .entry(Rc::as_ptr(o) as usize)
                    .or_default()
                    .push(token);
            }
        }
        Ok(Value::Undefined)
    });
    it.def_method(&fr_proto, "unregister", 1, |i, this, a| {
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("\u{0}fr")) {
            return Err(i.make_error(
                "TypeError",
                "unregister called on a non-FinalizationRegistry",
            ));
        }
        let token = arg(a, 0);
        if !can_be_held_weakly(i, &token) {
            return Err(i.make_error("TypeError", "unregister token cannot be held weakly"));
        }
        let mut removed = false;
        if let Value::Obj(o) = &this {
            if let Some(tokens) = i.fr_tokens.get_mut(&(Rc::as_ptr(o) as usize)) {
                let before = tokens.len();
                tokens.retain(|t| !same_value(t, &token));
                removed = tokens.len() != before;
            }
        }
        Ok(Value::Bool(removed))
    });
    let fr_ctor = it.make_native("FinalizationRegistry", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "FinalizationRegistry requires 'new'"));
        }
        if !arg(a, 0).is_callable() {
            return Err(i.make_error("TypeError", "cleanup callback must be callable"));
        }
        let obj = new_from_ctor(i, "FinalizationRegistry")?;
        set_internal(&obj, "\u{0}fr", Value::Bool(true));
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
        target_rw(i, args, false)
    }
    fn target_rw(i: &mut Interp, args: &[Value], write: bool) -> Result<(TaInfo, usize), Value> {
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
        if write && i.immutable_buffers.contains(&info.buffer) {
            return Err(i.make_error(
                "TypeError",
                "Atomics: cannot write into a view over an immutable ArrayBuffer",
            ));
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
    /// Truncate `v` to the element type's bit width (unsigned domain) for raw comparison.
    fn wrap_bits(kind: TaKind, v: i128) -> u128 {
        let bits = kind.elsize() * 8;
        (v as u128) & (u128::MAX >> (128 - bits))
    }
    fn rmw(i: &mut Interp, args: &[Value], f: fn(i128, i128) -> i128) -> Result<Value, Value> {
        let (info, idx) = target_rw(i, args, true)?;
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
        let (info, idx) = target_rw(i, a, true)?;
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
        let (info, idx) = target_rw(i, a, true)?;
        let expected = operand(i, &info, &arg(a, 2))?;
        let replacement = operand(i, &info, &arg(a, 3))?;
        let old = read_i128(i, &info, idx);
        // The comparison is on the element's raw byte representation, so the expected value
        // wraps to the element type first (e.g. 68547 matches an Int16 2979).
        let old_val = i.ta_read(&info, idx);
        if wrap_bits(info.kind, old) == wrap_bits(info.kind, expected) {
            write_i128(i, &info, idx, replacement);
        }
        Ok(old_val)
    });
    it.def_method(&atomics, "isLockFree", 1, |i, _t, a| {
        let n = ab(i.to_number(&arg(a, 0)))?;
        Ok(Value::Bool(matches!(n as i64, 1 | 2 | 4 | 8)))
    });
    // ValidateIntegerTypedArray(ta, waitable=true): only a non-detached Int32Array / BigInt64Array,
    // checked BEFORE any index/value coercion (so a poisoned index doesn't run first).
    fn require_waitable(i: &mut Interp, ta: &Value) -> Result<TaInfo, Value> {
        let err = || {
            i.make_error(
                "TypeError",
                "Atomics.wait/notify requires an Int32Array or BigInt64Array",
            )
        };
        let info = map_ptr(ta)
            .and_then(|p| i.typed_arrays.get(&p).copied())
            .ok_or_else(err)?;
        if !matches!(info.kind, TaKind::I32 | TaKind::I64) {
            return Err(err());
        }
        // A detached buffer (regular buffer removed from the store) has no [[ArrayBufferData]].
        if !i.array_buffers.contains_key(&info.buffer)
            && !i.shared_buffers.contains_key(&info.buffer)
        {
            return Err(i.make_error("TypeError", "Atomics.wait/notify: buffer is detached"));
        }
        Ok(info)
    }
    it.def_method(&atomics, "wait", 4, |i, _t, a| {
        let winfo = require_waitable(i, &arg(a, 0))?;
        // Atomics.wait needs a *shared* buffer — checked before ValidateAtomicAccess (index coercion).
        let id = match i.shared_buffers.get(&winfo.buffer) {
            Some(&id) => id,
            None => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.wait requires a SharedArrayBuffer-backed array",
                ))
            }
        };
        let (info, idx) = target(i, a)?;
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
        require_waitable(i, &arg(a, 0))?;
        let (info, idx) = target(i, a)?;
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
        // ValidateIntegerTypedArray(waitable) runs before any index/value coercion.
        let winfo = require_waitable(i, &arg(a, 0))?;
        let id = match i.shared_buffers.get(&winfo.buffer) {
            Some(&id) => id,
            None => {
                return Err(i.make_error(
                    "TypeError",
                    "Atomics.waitAsync requires a SharedArrayBuffer-backed array",
                ))
            }
        };
        let (info, idx) = target(i, a)?;
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
        // A script runs with full GlobalDeclarationInstantiation (clash checks, global-object
        // own properties for var/function declarations).
        i.run_program(&body)
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
    set_builtin(
        &host,
        "AbstractModuleSource",
        make_abstract_module_source(it),
    );
    Value::Obj(host)
}

/// The %AbstractModuleSource% intrinsic exposed as `$262.AbstractModuleSource`: an abstract
/// constructor (throws when called), whose `.prototype` carries the `@@toStringTag` getter used by
/// module-source objects. lumen has no concrete module-source objects, so the getter always yields
/// `undefined`.
fn make_abstract_module_source(it: &mut Interp) -> Value {
    let ctor = it.make_native("AbstractModuleSource", 0, |i, _t, _a| {
        Err(i.make_error(
            "TypeError",
            "Abstract class AbstractModuleSource not directly constructable",
        ))
    });
    let proto = Object::new(Some(it.object_proto.clone()));
    // `@@toStringTag` getter: returns the source's name for a real module-source object, else undefined.
    if let Some(key) = well_known_key(it, "toStringTag") {
        let getter = it.make_native("get [Symbol.toStringTag]", 0, |_i, _t, _a| {
            Ok(Value::Undefined)
        });
        proto.borrow_mut().props.insert(
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
    proto.borrow_mut().props.insert(
        "constructor",
        Property::data(Value::Obj(ctor.clone()), true, false, true),
    );
    // `.prototype` is non-writable, non-enumerable, non-configurable — and the prototype of every
    // source-phase ModuleSource object (see Interp::module_source_of).
    it.extra_protos
        .insert("%AbstractModuleSourceProto%", proto.clone());
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto), false, false, false),
    );
    Value::Obj(ctor)
}

/// Reconstruct a SharedArrayBuffer object in this agent that aliases the global shared block `id`.
fn agent_make_shared(i: &mut Interp, id: u64, len: usize) -> Value {
    let obj = Object::new(i.extra_protos.get("SharedArrayBuffer").cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
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
    it.def_method(&agent, "setTimeout", 2, |i, _t, a| {
        let f = arg(a, 0);
        if !f.is_callable() {
            return Err(i.make_error("TypeError", "setTimeout requires a callable"));
        }
        let ms = ab(i.to_number(&arg(a, 1)))?.max(0.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms as u64);
        i.pending_timers.push((f, deadline));
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

fn dv_info(i: &mut Interp, this: &Value) -> Result<(usize, usize, usize, bool), Value> {
    let ptr =
        map_ptr(this).ok_or_else(|| i.make_error("TypeError", "receiver is not a DataView"))?;
    i.data_views
        .get(&ptr)
        .copied()
        .ok_or_else(|| i.make_error("TypeError", "receiver is not a DataView"))
}

/// The DataView's current byte length, or `None` when its buffer is detached or (over a resizable
/// buffer) the view is now out of bounds.
fn dv_view_len(i: &Interp, buf: usize, off: usize, len: usize, track: bool) -> Option<usize> {
    let blen = i.array_buffers.get(&buf)?.len();
    if track {
        if off > blen {
            None
        } else {
            Some(blen - off)
        }
    } else if off + len > blen {
        None
    } else {
        Some(len)
    }
}
fn dv_buffer_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    dv_info(i, &this)?;
    ab(i.get_member(&this, "__dv_buffer"))
}
fn dv_bytelength_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (buf, off, len, track) = dv_info(i, &this)?;
    match dv_view_len(i, buf, off, len, track) {
        Some(l) => Ok(Value::Num(l as f64)),
        None => Err(i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )),
    }
}
fn dv_byteoffset_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let (buf, off, len, track) = dv_info(i, &this)?;
    if dv_view_len(i, buf, off, len, track).is_none() {
        return Err(i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        ));
    }
    Ok(Value::Num(off as f64))
}

fn dv_get(i: &mut Interp, this: &Value, args: &[Value], kind: TaKind) -> Result<Value, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let little = i.to_boolean(&arg(args, 1));
    let es = kind.elsize();
    // GetViewByteLength: re-derived after coercion (a detached/out-of-bounds view throws TypeError).
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    if byte_off.checked_add(es).is_none_or(|e| e > vlen) {
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
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    // IsImmutableBuffer is checked before ToIndex(byteOffset)/ToNumber(value) read any arguments.
    if i.immutable_buffers.contains(&buf) {
        return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
    }
    let byte_off = to_index(i, &arg(args, 0))?;
    let value = ab(i.to_number(&arg(args, 1)))?;
    let little = i.to_boolean(&arg(args, 2));
    // Coercing the index/value can detach or resize the buffer — re-derive the view length.
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    let es = kind.elsize();
    if byte_off.checked_add(es).is_none_or(|e| e > vlen) {
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
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    let byte_off = to_index(i, &arg(args, 0))?;
    let little = i.to_boolean(&arg(args, 1));
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    if byte_off.checked_add(8).is_none_or(|e| e > vlen) {
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
    let (buf, off, len, track) = *i
        .data_views
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a DataView"))?;
    if i.immutable_buffers.contains(&buf) {
        return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
    }
    let byte_off = to_index(i, &arg(args, 0))?;
    let value = ab(i.to_bigint(&arg(args, 1)))?;
    let little = i.to_boolean(&arg(args, 2));
    let vlen = dv_view_len(i, buf, off, len, track).ok_or_else(|| {
        i.make_error(
            "TypeError",
            "DataView's buffer is detached or out of bounds",
        )
    })?;
    if byte_off.checked_add(8).is_none_or(|e| e > vlen) {
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
        let g = it.make_native(&format!("get {name}"), 0, getter);
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
    // DataView.prototype[@@toStringTag] = "DataView" (non-writable, non-enumerable, configurable).
    set_to_string_tag(it, &proto, "DataView");
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
        // An ArrayBuffer object is identified by its [[ArrayBufferData]] slot (the internal
        // `__abMaxByteLength` marker), which survives detachment — a detached buffer is still an
        // ArrayBuffer, so ToNumber(byteOffset) must run before the detached check throws.
        let (bv, bp) = match arg(a, 0) {
            Value::Obj(o) if o.borrow().props.contains("__abMaxByteLength") => {
                (Value::Obj(o.clone()), Rc::as_ptr(&o) as usize)
            }
            _ => return Err(i.make_error("TypeError", "DataView requires an ArrayBuffer")),
        };
        // ToIndex(byteOffset) may run user code that detaches/resizes the buffer.
        let offset = match arg(a, 1) {
            Value::Undefined => 0,
            v => to_index(i, &v)?,
        };
        let has_len = !matches!(arg(a, 2), Value::Undefined);
        let len_arg = if has_len {
            Some(to_index(i, &arg(a, 2))?)
        } else {
            None
        };
        // Re-read the (possibly mutated) buffer state after all coercions.
        if !i.array_buffers.contains_key(&bp) {
            return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
        }
        let buflen = i.array_buffers[&bp].len();
        if offset > buflen {
            return Err(i.make_error("RangeError", "DataView byteOffset is out of bounds"));
        }
        let rv = ab(i.get_member(&bv, "resizable"))?;
        let resizable = i.to_boolean(&rv);
        if let Some(l) = len_arg {
            if offset + l > buflen {
                return Err(i.make_error("RangeError", "DataView byteLength is out of bounds"));
            }
        }
        // OrdinaryCreateFromConstructor does Get(newTarget, "prototype"), which can run a custom
        // proto getter that detaches or resizes the buffer — re-validate everything afterwards.
        let obj = new_from_ctor(i, "DataView")?;
        if !i.array_buffers.contains_key(&bp) {
            return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
        }
        let buflen = i.array_buffers[&bp].len();
        if offset > buflen {
            return Err(i.make_error("RangeError", "DataView byteOffset is out of bounds"));
        }
        let len = match len_arg {
            Some(l) => {
                if offset + l > buflen {
                    return Err(i.make_error("RangeError", "DataView byteLength is out of bounds"));
                }
                l
            }
            None => buflen - offset,
        };
        // A length-tracking DataView (no explicit byteLength) over a resizable buffer follows the
        // buffer's current length; its stored `len` is only the initial snapshot.
        let track = !has_len && resizable;
        let p = Rc::as_ptr(&obj) as usize;
        i.gc_pin(&obj);
        i.data_views.insert(p, (bp, offset, len, track));
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
        if let Value::Obj(o) = &this {
            i.gc_pin(o);
        }
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

/// `this` must be an Object for the generic `RegExp.prototype[@@…]` methods (they read flags and
/// call `exec` through ordinary property access rather than the internal slots).
fn require_regexp_this(i: &mut Interp, this: &Value, name: &str) -> Result<(), Value> {
    match this {
        Value::Obj(_) => Ok(()),
        _ => Err(i.make_error("TypeError", format!("{name} called on a non-object"))),
    }
}

/// RegExpExec(R, S): call `R.exec` if it is callable (validating the result is an Object or null),
/// otherwise fall back to the built-in `RegExp.prototype.exec` (which requires a real RegExp).
fn regexp_exec_abstract(i: &mut Interp, r: &Value, s: Rc<str>) -> Result<Value, Value> {
    let exec = ab(i.get_member(r, "exec"))?;
    if exec.is_callable() {
        let res = ab(i.call(exec, r.clone(), &[Value::Str(s)]))?;
        if !matches!(res, Value::Obj(_) | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "RegExp exec method returned something other than an Object or null",
            ));
        }
        return Ok(res);
    }
    if map_ptr(r).map(|p| i.regexps.contains_key(&p)) != Some(true) {
        return Err(i.make_error("TypeError", "exec called on a non-RegExp object"));
    }
    regexp_exec(i, r.clone(), &[Value::Str(s)])
}

/// AdvanceStringIndex. lumen strings are sequences of code points (not UTF-16 units), so a step is
/// always one code point — the `fullUnicode` distinction (surrogate pairs) doesn't apply here.
fn advance_string_index(index: usize, _s: &str, _unicode: bool) -> usize {
    index + 1
}

fn re_sym_match(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.match]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let flags = ab(i.get_member(&this, "flags"))?;
    let flags = ab(i.to_string(&flags))?;
    if !flags.contains('g') {
        return regexp_exec_abstract(i, &this, s);
    }
    let unicode = flags.contains('u') || flags.contains('v');
    ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_abstract(i, &this, s.clone())?;
        if matches!(result, Value::Null) {
            break;
        }
        let m0 = ab(i.get_member(&result, "0"))?;
        let match_str = ab(i.to_string(&m0))?;
        results.push(Value::Str(match_str.clone()));
        if match_str.is_empty() {
            let li = ab(i.get_member(&this, "lastIndex"))?;
            let li = ab(i.to_number(&li))?.max(0.0) as usize;
            let next = advance_string_index(li, &s, unicode);
            ab(i.set_member(&this, "lastIndex", Value::Num(next as f64)))?;
        }
    }
    if results.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(i.make_array(results))
    }
}
fn re_sym_search(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.search]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let prev = ab(i.get_member(&this, "lastIndex"))?;
    if !same_value(&prev, &Value::Num(0.0)) {
        ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
    }
    let result = regexp_exec_abstract(i, &this, s)?;
    let cur = ab(i.get_member(&this, "lastIndex"))?;
    if !same_value(&cur, &prev) {
        ab(i.set_member(&this, "lastIndex", prev))?;
    }
    match result {
        Value::Null => Ok(Value::Num(-1.0)),
        _ => ab(i.get_member(&result, "index")),
    }
}
fn re_sym_replace(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.replace]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let schars: Vec<char> = s.chars().collect();
    let size = schars.len();
    let repl = arg(a, 1);
    let functional = repl.is_callable();
    let repl_str = if functional {
        Rc::from("")
    } else {
        ab(i.to_string(&repl))?
    };
    let global_v = ab(i.get_member(&this, "global"))?;
    let global = i.to_boolean(&global_v);
    let unicode = if global {
        let unicode_v = ab(i.get_member(&this, "unicode"))?;
        let u = i.to_boolean(&unicode_v);
        ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
        u
    } else {
        false
    };
    // Collect every match (RegExpExec advances lastIndex; empty matches step forward manually).
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_abstract(i, &this, s.clone())?;
        if matches!(result, Value::Null) {
            break;
        }
        results.push(result.clone());
        if !global {
            break;
        }
        let m0_v = ab(i.get_member(&result, "0"))?;
        let m0 = ab(i.to_string(&m0_v))?;
        if m0.is_empty() {
            let li_v = ab(i.get_member(&this, "lastIndex"))?;
            let li = ab(i.to_number(&li_v))?.max(0.0) as usize;
            let next = advance_string_index(li, &s, unicode);
            ab(i.set_member(&this, "lastIndex", Value::Num(next as f64)))?;
        }
    }
    let mut accumulated = String::new();
    let mut next_pos = 0usize;
    for result in &results {
        let length_v = ab(i.get_member(result, "length"))?;
        let length = to_length_val(i, &length_v)?;
        let ncaptures = length.saturating_sub(1);
        let matched_v = ab(i.get_member(result, "0"))?;
        let matched = ab(i.to_string(&matched_v))?;
        let match_len = matched.chars().count();
        let index_v = ab(i.get_member(result, "index"))?;
        let pos_raw = ab(i.to_number(&index_v))?;
        let position = if pos_raw.is_nan() || pos_raw < 0.0 {
            0
        } else {
            (pos_raw as usize).min(size)
        };
        let mut captures: Vec<Value> = Vec::new();
        for n in 1..=ncaptures {
            let cap = ab(i.get_member(result, &n.to_string()))?;
            captures.push(if matches!(cap, Value::Undefined) {
                Value::Undefined
            } else {
                Value::Str(ab(i.to_string(&cap))?)
            });
        }
        let named = ab(i.get_member(result, "groups"))?;
        let replacement = if functional {
            let mut cbargs = vec![Value::Str(matched.clone())];
            cbargs.extend(captures.iter().cloned());
            cbargs.push(Value::Num(position as f64));
            cbargs.push(Value::Str(s.clone()));
            if !matches!(named, Value::Undefined) {
                cbargs.push(named.clone());
            }
            let r = ab(i.call(repl.clone(), Value::Undefined, &cbargs))?;
            ab(i.to_string(&r))?.to_string()
        } else {
            get_substitution(i, &matched, &schars, position, &captures, &named, &repl_str)?
        };
        if position >= next_pos {
            accumulated.extend(schars[next_pos..position].iter());
            accumulated.push_str(&replacement);
            next_pos = position + match_len;
        }
    }
    if next_pos < size {
        accumulated.extend(schars[next_pos..].iter());
    }
    Ok(Value::from_string(accumulated))
}

/// ToLength of a value (clamped to a non-negative integer).
fn to_length_val(i: &mut Interp, v: &Value) -> Result<usize, Value> {
    let n = ab(i.to_number(v))?;
    Ok(if n.is_nan() || n < 0.0 {
        0
    } else {
        n.min(9_007_199_254_740_991.0) as usize
    })
}

/// GetSubstitution: expand a `$`-template against a match. `captures` are already `Value::Str` /
/// `Value::Undefined`; `named` is the match's `groups` object (or `undefined`).
fn get_substitution(
    i: &mut Interp,
    matched: &str,
    schars: &[char],
    position: usize,
    captures: &[Value],
    named: &Value,
    template: &str,
) -> Result<String, Value> {
    let tail = (position + matched.chars().count()).min(schars.len());
    let push_cap = |out: &mut String, cap: &Value| {
        if let Value::Str(s) = cap {
            out.push_str(s);
        }
    };
    let t: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut k = 0;
    while k < t.len() {
        if t[k] != '$' || k + 1 >= t.len() {
            out.push(t[k]);
            k += 1;
            continue;
        }
        match t[k + 1] {
            '$' => {
                out.push('$');
                k += 2;
            }
            '&' => {
                out.push_str(matched);
                k += 2;
            }
            '`' => {
                out.extend(schars[..position].iter());
                k += 2;
            }
            '\'' => {
                out.extend(schars[tail..].iter());
                k += 2;
            }
            '<' if !matches!(named, Value::Undefined) => {
                if let Some(rel) = t[k + 2..].iter().position(|&c| c == '>') {
                    let name: String = t[k + 2..k + 2 + rel].iter().collect();
                    let v = ab(i.get_member(named, &name))?;
                    if !matches!(v, Value::Undefined) {
                        out.push_str(&ab(i.to_string(&v))?);
                    }
                    k = k + 2 + rel + 1;
                } else {
                    out.push('$');
                    k += 1;
                }
            }
            d if d.is_ascii_digit() => {
                let one = d.to_digit(10).unwrap() as usize;
                let two = if k + 2 < t.len() && t[k + 2].is_ascii_digit() {
                    Some(one * 10 + t[k + 2].to_digit(10).unwrap() as usize)
                } else {
                    None
                };
                // Prefer a two-digit group reference when it is in range.
                if let Some(tw) = two {
                    if tw >= 1 && tw <= captures.len() {
                        push_cap(&mut out, &captures[tw - 1]);
                        k += 3;
                        continue;
                    }
                }
                if one >= 1 && one <= captures.len() {
                    push_cap(&mut out, &captures[one - 1]);
                    k += 2;
                } else {
                    out.push('$');
                    k += 1;
                }
            }
            _ => {
                out.push('$');
                k += 1;
            }
        }
    }
    Ok(out)
}
fn re_sym_matchall(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.matchAll]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "RegExp"))?;
    let c = species_constructor(i, &this, &default_ctor)?;
    let flags_v = ab(i.get_member(&this, "flags"))?;
    let flags = ab(i.to_string(&flags_v))?;
    let matcher = ab(i.construct(c, &[this.clone(), Value::Str(flags.clone())]))?;
    let li_v = ab(i.get_member(&this, "lastIndex"))?;
    let last = to_length_val(i, &li_v)?;
    ab(i.set_member(&matcher, "lastIndex", Value::Num(last as f64)))?;
    let global = flags.contains('g');
    let unicode = flags.contains('u') || flags.contains('v');
    Ok(create_regexp_string_iterator(
        i, matcher, s, global, unicode,
    ))
}
fn re_sym_split(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.split]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let schars: Vec<char> = s.chars().collect();
    let size = schars.len();
    // SpeciesConstructor(rx, %RegExp%), then a sticky-flagged splitter constructed from it.
    let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "RegExp"))?;
    let c = species_constructor(i, &this, &default_ctor)?;
    let flags_v = ab(i.get_member(&this, "flags"))?;
    let flags = ab(i.to_string(&flags_v))?;
    let unicode = flags.contains('u') || flags.contains('v');
    let new_flags = if flags.contains('y') {
        flags.to_string()
    } else {
        format!("{flags}y")
    };
    let splitter = ab(i.construct(c, &[this.clone(), Value::from_string(new_flags)]))?;
    let limit = match arg(a, 1) {
        Value::Undefined => u32::MAX as usize,
        v => {
            let n = ab(i.to_number(&v))?;
            if n.is_nan() || n <= 0.0 {
                0
            } else {
                (n as u64 % (1u64 << 32)) as usize
            }
        }
    };
    let mut out: Vec<Value> = Vec::new();
    if limit == 0 {
        return Ok(i.make_array(out));
    }
    if size == 0 {
        let z = regexp_exec_abstract(i, &splitter, s.clone())?;
        if !matches!(z, Value::Null) {
            return Ok(i.make_array(out));
        }
        out.push(Value::Str(s));
        return Ok(i.make_array(out));
    }
    let mut p = 0usize;
    let mut q = 0usize;
    while q < size {
        ab(i.set_member(&splitter, "lastIndex", Value::Num(q as f64)))?;
        let z = regexp_exec_abstract(i, &splitter, s.clone())?;
        if matches!(z, Value::Null) {
            q = advance_string_index(q, &s, unicode);
            continue;
        }
        let li_v = ab(i.get_member(&splitter, "lastIndex"))?;
        let e = to_length_val(i, &li_v)?.min(size);
        if e == p {
            q = advance_string_index(q, &s, unicode);
            continue;
        }
        out.push(Value::from_string(schars[p..q].iter().collect::<String>()));
        if out.len() == limit {
            return Ok(i.make_array(out));
        }
        p = e;
        let len_v = ab(i.get_member(&z, "length"))?;
        let ncaptures = to_length_val(i, &len_v)?.saturating_sub(1);
        for n in 1..=ncaptures {
            let cap = ab(i.get_member(&z, &n.to_string()))?;
            out.push(cap);
            if out.len() == limit {
                return Ok(i.make_array(out));
            }
        }
        q = p;
    }
    out.push(Value::from_string(
        schars[p..size].iter().collect::<String>(),
    ));
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
            // `indices` (the `d` flag): [start, end] pairs per capture (undefined if unmatched),
            // plus a null-prototype `groups` of the same for named captures.
            let indices = if re.flags.contains('d') {
                let pair = |i: &mut Interp, span: Option<(usize, usize)>| match span {
                    Some((a, b)) => i.make_array(vec![Value::Num(a as f64), Value::Num(b as f64)]),
                    None => Value::Undefined,
                };
                let mut idx_items = Vec::with_capacity(re.ngroups + 1);
                for g in 0..=re.ngroups {
                    let span = caps.get(g).copied().flatten();
                    idx_items.push(pair(i, span));
                }
                let arr = i.make_array(idx_items);
                let igroups = if re.names.is_empty() {
                    Value::Undefined
                } else {
                    let g = i.new_object();
                    g.borrow_mut().proto = None;
                    let mut seen: Vec<String> = Vec::new();
                    for (name, _) in &re.names {
                        if seen.iter().any(|s| s == name) {
                            continue;
                        }
                        seen.push(name.clone());
                        let span = re
                            .names
                            .iter()
                            .filter(|(n, _)| n == name)
                            .find_map(|(_, idx)| caps.get(*idx).copied().flatten());
                        let v = pair(i, span);
                        set_data(&g, name, v);
                    }
                    Value::Obj(g)
                };
                if let Value::Obj(o) = &arr {
                    set_data(o, "groups", igroups);
                }
                Some(arr)
            } else {
                None
            };
            if let Value::Obj(o) = &result {
                set_data(o, "index", Value::Num(start as f64));
                set_data(o, "input", Value::Str(input));
                set_data(o, "groups", groups);
                if let Some(ind) = indices {
                    set_data(o, "indices", ind);
                }
            }
            Ok(result)
        }
    }
}

/// Coerce `v` to a RegExp object (returning it unchanged if already one).
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
    // SharedArrayBuffer.prototype accessors require a shared buffer: reject a plain ArrayBuffer
    // `this` with a TypeError (the shared side table is the discriminator).
    fn require_shared_buffer(i: &Interp, this: &Value) -> Result<(), Value> {
        let shared = matches!(this, Value::Obj(o) if i.shared_buffers.contains_key(&(Rc::as_ptr(o) as usize)));
        if !shared {
            return Err(i.make_error("TypeError", "requires a SharedArrayBuffer"));
        }
        Ok(())
    }
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
        require_shared_buffer(i, &this)?;
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
        require_shared_buffer(i, &this)?;
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
        require_shared_buffer(i, &this)?;
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
        // RequireInternalSlot + IsSharedArrayBuffer(O).
        let o = this
            .as_obj()
            .filter(|o| i.shared_buffers.contains_key(&(Rc::as_ptr(o) as usize)))
            .ok_or_else(|| {
                i.make_error(
                    "TypeError",
                    "SharedArrayBuffer.prototype.slice requires a SharedArrayBuffer",
                )
            })?;
        let ptr = Rc::as_ptr(o) as usize;
        let len = i.array_buffers.get(&ptr).map(|b| b.len()).unwrap_or(0) as i64;
        let begin = norm_index(ab(i.to_number(&arg(a, 0)))?, len);
        let end = match arg(a, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len),
        };
        let new_len = (end - begin).max(0) as usize;
        // SpeciesConstructor(O, %SharedArrayBuffer%) makes the result, validated as a distinct,
        // large-enough SharedArrayBuffer.
        let sab_ctor = i
            .global
            .borrow()
            .props
            .get("SharedArrayBuffer")
            .map(|p| p.value.clone())
            .unwrap_or(Value::Undefined);
        let ctor = species_constructor(i, &this, &sab_ctor)?;
        let new_buf = ab(i.construct(ctor, &[Value::Num(new_len as f64)]))?;
        let nptr = match &new_buf {
            Value::Obj(no) if i.shared_buffers.contains_key(&(Rc::as_ptr(no) as usize)) => {
                Rc::as_ptr(no) as usize
            }
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "slice species did not create a SharedArrayBuffer",
                ))
            }
        };
        if nptr == ptr {
            return Err(i.make_error(
                "TypeError",
                "slice species returned the same SharedArrayBuffer",
            ));
        }
        // The new buffer must be at least `new_len` bytes.
        let dst_byte_len = i.array_buffers.get(&nptr).map(|b| b.len()).unwrap_or(0);
        if dst_byte_len < new_len {
            return Err(i.make_error("TypeError", "slice species buffer is too small"));
        }
        // A SharedArrayBuffer's bytes live in the process-global shared block keyed by `__sab_id`.
        let src_id = match i.get_member(&this, "__sab_id") {
            Ok(Value::Num(n)) => n as u64,
            _ => return Err(i.make_error("TypeError", "not a SharedArrayBuffer")),
        };
        let src = crate::interpreter::shared_mem_get(src_id)
            .map(|m| m.lock().unwrap().clone())
            .unwrap_or_default();
        if begin < end && (end as usize) <= src.len() {
            let slice = src[begin as usize..end as usize].to_vec();
            if let Ok(Value::Num(dst_id)) = i.get_member(&new_buf, "__sab_id") {
                if let Some(m) = crate::interpreter::shared_mem_get(dst_id as u64) {
                    let mut buf = m.lock().unwrap();
                    let n = slice.len().min(buf.len());
                    buf[..n].copy_from_slice(&slice[..n]);
                }
            }
        }
        Ok(new_buf)
    });
    let ctor = it.make_native("SharedArrayBuffer", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "SharedArrayBuffer constructor requires 'new'"));
        }
        // ToIndex(length), then the options bag's maxByteLength — both *before* object creation
        // (a poisoned newTarget.prototype getter fires next), with allocation limits checked last.
        let nraw = ab(i.to_number(&arg(a, 0)))?;
        let n = if nraw.is_nan() { 0.0 } else { nraw.trunc() };
        if !(0.0..=9007199254740991.0).contains(&n) {
            return Err(i.make_error("RangeError", "Invalid SharedArrayBuffer length"));
        }
        let mut max: Option<f64> = None;
        if let Value::Obj(_) = arg(a, 1) {
            let mbl = ab(i.get_member(&arg(a, 1), "maxByteLength"))?;
            if !matches!(mbl, Value::Undefined) {
                let m = ab(i.to_number(&mbl))?;
                let m = if m.is_nan() { 0.0 } else { m.trunc() };
                if !(0.0..=9007199254740991.0).contains(&m) || m < n {
                    return Err(i.make_error("RangeError", "Invalid maxByteLength"));
                }
                max = Some(m);
            }
        }
        // OrdinaryCreateFromConstructor: the prototype comes from newTarget (abrupt propagates).
        let obj = new_from_ctor(i, "SharedArrayBuffer")?;
        // Data allocation happens after the object exists — an oversized request fails here.
        let len = n as usize;
        if n as u128 > MAX_ARRAY_OP_LEN as u128
            || max.is_some_and(|m| m as u128 > MAX_ARRAY_OP_LEN as u128)
        {
            return Err(i.make_error("RangeError", "SharedArrayBuffer allocation too large"));
        }
        let bp = Rc::as_ptr(&obj) as usize;
        i.gc_pin(&obj);
        i.array_buffers.insert(bp, vec![0u8; len]);
        set_internal(&obj, "__abMaxByteLength", Value::Num(max.unwrap_or(n)));
        set_internal(&obj, "__abResizable", Value::Bool(max.is_some()));
        let id = crate::interpreter::alloc_shared_mem(len);
        i.shared_buffers.insert(bp, id);
        set_internal(&obj, "__sab_id", Value::Num(id as f64));
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
    install_species(it, &ctor); // SharedArrayBuffer[@@species] returns `this`
    set_builtin(&it.global, "SharedArrayBuffer", Value::Obj(ctor));
}

/// Encode (sec-encodeuri-encode): per character; a lone (smuggled) surrogate is a URIError
/// (`None`).
fn uri_encode(s: &str, keep: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(mut c) = chars.next() {
        if crate::jstr::smuggled(c).is_some() {
            // A smuggled pair encodes a real smuggle-range character; a lone one is malformed.
            match chars.peek().and_then(|&n| crate::jstr::paired_char(c, n)) {
                Some(real) => {
                    chars.next();
                    c = real;
                }
                None => return None,
            }
        }
        if c.is_ascii()
            && (c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) || keep.contains(c))
        {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Some(out)
}

/// Decode (sec-decodeuri-decode): percent-escapes become bytes, validated as strict UTF-8 per
/// sequence (a bad hex digit, truncated escape, stray continuation byte, overlong form, encoded
/// surrogate, or > U+10FFFF is a URIError -> None). An escape whose decoded octet is an ASCII
/// character in `preserve` (decodeURI's reservedSet) keeps its original `%XX` text instead.
fn uri_decode(s: &str, preserve: &str) -> Option<String> {
    fn hex_at(bytes: &[u8], i: usize) -> Option<u8> {
        if i + 2 >= bytes.len() || bytes[i] != b'%' {
            return None;
        }
        let h = (bytes[i + 1] as char).to_digit(16)?;
        let l = (bytes[i + 2] as char).to_digit(16)?;
        Some((h * 16 + l) as u8)
    }
    let bytes = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            let c = s[i..].chars().next()?;
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let start = i;
        let b0 = hex_at(bytes, i)?;
        i += 3;
        if b0 < 0x80 {
            let c = b0 as char;
            if preserve.contains(c) {
                out.push_str(&s[start..i]);
            } else {
                out.push(c);
            }
            continue;
        }
        let cont = match b0 {
            0xC2..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF4 => 3,
            _ => return None,
        };
        let mut buf = [b0, 0, 0, 0];
        for k in 0..cont {
            let bc = hex_at(bytes, i)?;
            if bc & 0xC0 != 0x80 {
                return None;
            }
            buf[k + 1] = bc;
            i += 3;
        }
        out.push_str(std::str::from_utf8(&buf[..cont + 1]).ok()?);
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// ArrayBuffer + TypedArrays. Backing bytes live in `Interp::array_buffers`; each view's state in
// `Interp::typed_arrays`. Integer-index get/set is wired in `get_member`/`set_member`; the named
// metadata (length/byteLength/byteOffset/buffer/BYTES_PER_ELEMENT) is stored as real own props.
// ---------------------------------------------------------------------------------------------

/// ArrayBuffer.prototype.transfer / transferToFixedLength: move the bytes into a fresh fixed-length
/// buffer and detach the source (a TypeError if it's already detached).
fn ab_transfer(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    ab_transfer_impl(i, this, a, false)
}
fn ab_transfer_fixed(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    ab_transfer_impl(i, this, a, true)
}
/// ArrayBuffer.prototype.transfer / transferToFixedLength: copy into a new buffer of `newLength` and
/// detach the source. `transfer` preserves the source's resizability (its maxByteLength);
/// `transferToFixedLength` always produces a fixed-length buffer.
fn ab_transfer_impl(i: &mut Interp, this: Value, a: &[Value], fixed: bool) -> Result<Value, Value> {
    let o = this
        .as_obj()
        .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
        .cloned()
        .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
    let ptr = Rc::as_ptr(&o) as usize;
    if i.shared_buffers.contains_key(&ptr) {
        return Err(i.make_error("TypeError", "transfer requires a non-shared ArrayBuffer"));
    }
    // The newLength argument is read (observably) before detachability is verified.
    let new_len = match arg(a, 0) {
        Value::Undefined => None,
        v => {
            let n = ab(i.to_number(&v))?;
            let n = if n.is_nan() { 0.0 } else { n.trunc() };
            if n < 0.0 || !n.is_finite() || n as usize > MAX_ARRAY_OP_LEN {
                return Err(i.make_error("RangeError", "invalid transfer length"));
            }
            Some(n as usize)
        }
    };
    if i.immutable_buffers.contains(&ptr) {
        return Err(i.make_error("TypeError", "an immutable ArrayBuffer is not detachable"));
    }
    if !i.array_buffers.contains_key(&ptr) {
        return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
    }
    let new_len = new_len.unwrap_or_else(|| i.array_buffers[&ptr].len());
    // transfer() preserves the source's resizability; transferToFixedLength() is always fixed.
    let src_resizable = matches!(i.get_member(&this, "resizable"), Ok(Value::Bool(true)));
    let src_max = match i.get_member(&this, "maxByteLength") {
        Ok(Value::Num(n)) => n as usize,
        _ => new_len,
    };
    let bytes = i.array_buffers[&ptr].clone();
    let (bv, bp) = make_array_buffer(i, new_len);
    if let Some(buf) = i.array_buffers.get_mut(&bp) {
        let n = bytes.len().min(new_len);
        buf[..n].copy_from_slice(&bytes[..n]);
    }
    if !fixed && src_resizable {
        if let Value::Obj(nb) = &bv {
            set_internal(nb, "__abMaxByteLength", Value::Num(src_max as f64));
            set_internal(nb, "__abResizable", Value::Bool(true));
        }
    }
    // Detach the source (drop its backing store; detached/byteLength derive from the side table).
    i.array_buffers.remove(&ptr);
    Ok(bv)
}

fn make_array_buffer(i: &mut Interp, byte_len: usize) -> (Value, usize) {
    let obj = Object::new(i.extra_protos.get("ArrayBuffer").cloned());
    let p = Rc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
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
    set_to_string_tag(it, &proto, "ArrayBuffer");
    // byteLength/maxByteLength/resizable/detached are accessor getters on the prototype.
    // ArrayBuffer.prototype accessors/methods require a non-shared buffer: reject a SharedArrayBuffer
    // `this` with a TypeError (both buffer kinds carry `__abMaxByteLength`, so brand alone isn't enough).
    fn reject_shared_buffer(i: &Interp, this: &Value) -> Result<(), Value> {
        if let Value::Obj(o) = this {
            if i.shared_buffers.contains_key(&(Rc::as_ptr(o) as usize)) {
                return Err(i.make_error("TypeError", "requires a non-shared ArrayBuffer"));
            }
        }
        Ok(())
    }
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
        reject_shared_buffer(i, &this)?;
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
        reject_shared_buffer(i, &this)?;
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
        reject_shared_buffer(i, &this)?;
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
        reject_shared_buffer(i, &this)?;
        let o = this
            .as_obj()
            .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
            .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
        Ok(Value::Bool(
            !i.array_buffers.contains_key(&(Rc::as_ptr(o) as usize)),
        ))
    });
    ab_getter(it, &proto, "immutable", |i, this, _| {
        reject_shared_buffer(i, &this)?;
        let o = this
            .as_obj()
            .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
            .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
        Ok(Value::Bool(
            i.immutable_buffers.contains(&(Rc::as_ptr(o) as usize)),
        ))
    });
    it.def_method(&proto, "slice", 2, |i, this, a| {
        // RequireInternalSlot([[ArrayBufferData]]), reject a shared buffer, then a detached one.
        reject_shared_buffer(i, &this)?;
        let o = this
            .as_obj()
            .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
            .ok_or_else(|| {
                i.make_error(
                    "TypeError",
                    "ArrayBuffer.prototype.slice requires an ArrayBuffer",
                )
            })?;
        let ptr = Rc::as_ptr(o) as usize;
        if !i.array_buffers.contains_key(&ptr) {
            return Err(i.make_error("TypeError", "Cannot slice a detached ArrayBuffer"));
        }
        let len = i.array_buffers[&ptr].len() as i64;
        let begin = norm_index(ab(i.to_number(&arg(a, 0)))?, len);
        let end = match arg(a, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len),
        };
        let new_len = (end - begin).max(0) as usize;
        // SpeciesConstructor(O, %ArrayBuffer%) creates the result buffer, then it is validated.
        let ab_ctor = i
            .global
            .borrow()
            .props
            .get("ArrayBuffer")
            .map(|p| p.value.clone())
            .unwrap_or(Value::Undefined);
        let ctor = species_constructor(i, &this, &ab_ctor)?;
        let new_buf = ab(i.construct(ctor, &[Value::Num(new_len as f64)]))?;
        let nptr = match &new_buf {
            Value::Obj(no) if no.borrow().props.contains("__abMaxByteLength") => {
                Rc::as_ptr(no) as usize
            }
            _ => {
                return Err(i.make_error("TypeError", "slice species did not create an ArrayBuffer"))
            }
        };
        if i.shared_buffers.contains_key(&nptr) {
            return Err(i.make_error("TypeError", "slice species returned a SharedArrayBuffer"));
        }
        if !i.array_buffers.contains_key(&nptr) {
            return Err(i.make_error("TypeError", "slice species returned a detached ArrayBuffer"));
        }
        if nptr == ptr {
            return Err(i.make_error("TypeError", "slice species returned the same ArrayBuffer"));
        }
        if i.immutable_buffers.contains(&nptr) {
            return Err(i.make_error("TypeError", "slice species returned an immutable buffer"));
        }
        if i.array_buffers[&nptr].len() < new_len {
            return Err(i.make_error("TypeError", "slice species buffer is too small"));
        }
        // The species constructor may have detached or shrunk the source.
        if !i.array_buffers.contains_key(&ptr) {
            return Err(i.make_error("TypeError", "source ArrayBuffer was detached during slice"));
        }
        let src = i.array_buffers[&ptr].clone();
        let (begin, end) = (begin.min(src.len() as i64), end.min(src.len() as i64));
        if begin < end {
            let slice = src[begin as usize..end as usize].to_vec();
            if let Some(buf) = i.array_buffers.get_mut(&nptr) {
                buf[..slice.len()].copy_from_slice(&slice);
            }
        }
        Ok(new_buf)
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
        let new_len = if new_len.is_nan() {
            0.0
        } else {
            new_len.trunc()
        };
        if !new_len.is_finite() || new_len < 0.0 {
            return Err(i.make_error("RangeError", "ArrayBuffer resize out of range"));
        }
        // The coercion may have detached the buffer — that is a TypeError, checked before the
        // max-length RangeError.
        if !i.array_buffers.contains_key(&(Rc::as_ptr(&o) as usize)) {
            return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
        }
        if new_len as usize > max {
            return Err(i.make_error("RangeError", "ArrayBuffer resize out of range"));
        }
        let n = new_len as usize;
        if i.immutable_buffers.contains(&(Rc::as_ptr(&o) as usize)) {
            return Err(i.make_error("TypeError", "ArrayBuffer is immutable"));
        }
        if let Some(buf) = i.array_buffers.get_mut(&(Rc::as_ptr(&o) as usize)) {
            buf.resize(n, 0);
        }
        // byteLength derives from the backing store length, which the resize above updated.
        Ok(Value::Undefined)
    });
    it.def_method(&proto, "transfer", 0, ab_transfer);
    it.def_method(&proto, "transferToFixedLength", 0, ab_transfer_fixed);
    // Immutable-ArrayBuffer proposal: move the bytes into a fresh immutable (always fixed-length)
    // buffer, detaching the source; or copy a range into an immutable buffer (source intact).
    it.def_method(&proto, "transferToImmutable", 0, |i, this, a| {
        let bv = ab_transfer_fixed(i, this, a)?;
        if let Value::Obj(o) = &bv {
            i.immutable_buffers.insert(Rc::as_ptr(o) as usize);
        }
        Ok(bv)
    });
    it.def_method(&proto, "sliceToImmutable", 2, |i, this, a| {
        reject_shared_buffer(i, &this)?;
        let ptr = this
            .as_obj()
            .filter(|o| o.borrow().props.contains("__abMaxByteLength"))
            .map(|o| Rc::as_ptr(o) as usize)
            .ok_or_else(|| i.make_error("TypeError", "not an ArrayBuffer"))?;
        if !i.array_buffers.contains_key(&ptr) {
            return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
        }
        let len = i.array_buffers[&ptr].len() as i64;
        let begin = norm_index(ab(i.to_number(&arg(a, 0)))?, len);
        let end = match arg(a, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len),
        };
        // Coercing start/end may have detached or shrunk the source buffer: detachment is a
        // TypeError, and a resolved range past the current length is a RangeError.
        let bytes = i
            .array_buffers
            .get(&ptr)
            .ok_or_else(|| i.make_error("TypeError", "ArrayBuffer is detached"))?;
        let cur = bytes.len() as i64;
        if begin < end && end > cur {
            return Err(i.make_error(
                "RangeError",
                "source ArrayBuffer shrank below the resolved range",
            ));
        }
        let slice = if begin < end {
            bytes[begin as usize..end as usize].to_vec()
        } else {
            Vec::new()
        };
        let (bv, bp) = make_array_buffer(i, slice.len());
        if let Some(buf) = i.array_buffers.get_mut(&bp) {
            buf.copy_from_slice(&slice);
        }
        i.immutable_buffers.insert(bp);
        Ok(bv)
    });
    let ctor = it.make_native("ArrayBuffer", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "ArrayBuffer constructor requires 'new'"));
        }
        // ToIndex(length): NaN → 0, truncate; negative or non-finite is a RangeError.
        let n = ab(i.to_number(&arg(a, 0)))?;
        let n = if n.is_nan() { 0.0 } else { n.trunc() };
        if n < 0.0 || !n.is_finite() {
            return Err(i.make_error("RangeError", "Invalid ArrayBuffer length"));
        }
        // GetArrayBufferMaxByteLengthOption: coerced before the object is created.
        let max: Option<f64> = if let Value::Obj(_) = arg(a, 1) {
            let mbl = ab(i.get_member(&arg(a, 1), "maxByteLength"))?;
            if matches!(mbl, Value::Undefined) {
                None
            } else {
                let m = ab(i.to_number(&mbl))?;
                let m = if m.is_nan() { 0.0 } else { m.trunc() };
                if m < 0.0 || !m.is_finite() {
                    return Err(i.make_error("RangeError", "Invalid maxByteLength"));
                }
                Some(m)
            }
        } else {
            None
        };
        if let Some(m) = max {
            if m < n {
                return Err(i.make_error("RangeError", "maxByteLength is below the length"));
            }
        }
        // OrdinaryCreateFromConstructor: new.target's prototype is read (observably) before the
        // data block is allocated, so a poisoned getter beats an allocation RangeError.
        let obj = new_from_ctor(i, "ArrayBuffer")?;
        if n as usize > MAX_ARRAY_OP_LEN || max.is_some_and(|m| m as usize > MAX_ARRAY_OP_LEN) {
            return Err(i.make_error("RangeError", "ArrayBuffer allocation too large"));
        }
        let len = n as usize;
        let p = Rc::as_ptr(&obj) as usize;
        i.gc_pin(&obj);
        i.array_buffers.insert(p, vec![0u8; len]);
        set_internal(&obj, "__abMaxByteLength", Value::Num(max.unwrap_or(n)));
        set_internal(&obj, "__abResizable", Value::Bool(max.is_some()));
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
    it.def_method(&ctor, "isView", 1, |i, _t, a| {
        // A view is a TypedArray or a DataView (identified by its `__dv_buffer` internal slot).
        let is_view = match arg(a, 0) {
            Value::Obj(o) => {
                i.typed_arrays.contains_key(&(Rc::as_ptr(&o) as usize))
                    || o.borrow().props.contains("__dv_buffer")
            }
            _ => false,
        };
        Ok(Value::Bool(is_view))
    });
    install_species(it, &ctor); // ArrayBuffer[@@species] returns `this`
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
        // IsValidIntegerIndex is re-checked against the *current* length: coercing the value may
        // have grown or shrunk a resizable backing buffer.
        let cur_len = i.ta_len(&info).unwrap_or(0);
        if actual < 0.0 || actual >= cur_len as f64 {
            return Err(i.make_error("RangeError", "invalid TypedArray index"));
        }
        let actual = actual as usize;
        // `with` uses TypedArrayCreateSameType — it ignores `@@species`.
        let (new_ta, new_info) = ta_create_same(i, info.kind, len)?;
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
    // Most iteration / search / mutation methods have TypedArray-specific semantics the generic
    // Array delegation can't reproduce: the length is captured once up front, out-of-bounds reads
    // (after a mid-operation detach or resizable-buffer shrink) yield `undefined` instead of
    // stopping early, and the mutators re-validate the buffer after argument coercion.
    if let Some(r) = ta_native(i, this, &info, method, args) {
        return r;
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
        let new_ta = ta_species_create(i, this, info.kind, &[Value::Num(len as f64)], true)?;
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

/// Construct a fresh TypedArray of the same element kind via its intrinsic constructor (ignoring
/// `@@species`), as `TypedArrayCreateSameType` does for toSorted/toReversed/with.
fn ta_create_same(i: &mut Interp, kind: TaKind, len: usize) -> Result<(Value, TaInfo), Value> {
    let ctor = ab(i.get_member(&Value::Obj(i.global.clone()), kind.name()))?;
    let r = ab(i.construct(ctor, &[Value::Num(len as f64)]))?;
    let info = map_ptr(&r)
        .and_then(|p| i.typed_arrays.get(&p).copied())
        .ok_or_else(|| i.make_error("TypeError", "TypedArray constructor did not return a view"))?;
    Ok((r, info))
}

/// CompareTypedArrayElements default (no comparefn): a total order over numbers where NaN sorts last
/// and -0 precedes +0.
fn ta_default_cmp_num(a: f64, b: f64) -> i32 {
    if a.is_nan() {
        return if b.is_nan() { 0 } else { 1 };
    }
    if b.is_nan() {
        return -1;
    }
    if a < b {
        -1
    } else if a > b {
        1
    } else {
        // Equal magnitude: distinguish -0 (< +0).
        match (a.is_sign_negative(), b.is_sign_negative()) {
            (true, false) => -1,
            (false, true) => 1,
            _ => 0,
        }
    }
}

/// SortCompare for a TypedArray element pair: a user comparefn (result ToNumber, NaN treated as 0)
/// or the default numeric order.
fn ta_sort_compare(
    i: &mut Interp,
    a: &Value,
    b: &Value,
    cmp: &Value,
    is_bigint: bool,
) -> Result<i32, Value> {
    if cmp.is_callable() {
        let r = ab(i.call(cmp.clone(), Value::Undefined, &[a.clone(), b.clone()]))?;
        let n = ab(i.to_number(&r))?;
        return Ok(if n.is_nan() {
            0
        } else if n < 0.0 {
            -1
        } else if n > 0.0 {
            1
        } else {
            0
        });
    }
    if is_bigint {
        let (x, y) = match (a, b) {
            (Value::BigInt(x), Value::BigInt(y)) => (*x, *y),
            _ => (0, 0),
        };
        Ok(x.cmp(&y) as i32)
    } else {
        let x = if let Value::Num(x) = a { *x } else { f64::NAN };
        let y = if let Value::Num(y) = b { *y } else { f64::NAN };
        Ok(ta_default_cmp_num(x, y))
    }
}

/// A stable merge sort that propagates a throwing comparefn.
fn ta_merge_sort(
    i: &mut Interp,
    vals: &mut [Value],
    cmp: &Value,
    is_bigint: bool,
) -> Result<(), Value> {
    let n = vals.len();
    if n < 2 {
        return Ok(());
    }
    let mid = n / 2;
    let mut left = vals[..mid].to_vec();
    let mut right = vals[mid..].to_vec();
    ta_merge_sort(i, &mut left, cmp, is_bigint)?;
    ta_merge_sort(i, &mut right, cmp, is_bigint)?;
    let (mut li, mut ri, mut k) = (0usize, 0usize, 0usize);
    while li < left.len() && ri < right.len() {
        if ta_sort_compare(i, &left[li], &right[ri], cmp, is_bigint)? <= 0 {
            vals[k] = left[li].clone();
            li += 1;
        } else {
            vals[k] = right[ri].clone();
            ri += 1;
        }
        k += 1;
    }
    while li < left.len() {
        vals[k] = left[li].clone();
        li += 1;
        k += 1;
    }
    while ri < right.len() {
        vals[k] = right[ri].clone();
        ri += 1;
        k += 1;
    }
    Ok(())
}

/// ToIntegerOrInfinity then clamp to a relative index in `[0, len]` (negatives count from the end).
fn rel_index(n: f64, len: usize) -> usize {
    if n.is_nan() {
        return 0;
    }
    let n = if n.is_infinite() {
        if n > 0.0 {
            len as f64
        } else {
            0.0
        }
    } else {
        n.trunc()
    };
    if n < 0.0 {
        (len as f64 + n).max(0.0) as usize
    } else {
        n.min(len as f64) as usize
    }
}

/// TypedArray-specific implementations of the iteration/search/mutation prototype methods. Returns
/// `None` for methods handled by the generic Array delegation. `len` semantics: captured once here,
/// so out-of-bounds element reads (after a detach/shrink during a callback) surface as `undefined`.
fn ta_native(
    i: &mut Interp,
    this: &Value,
    info: &TaInfo,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    let info = *info;
    let len = i.ta_len(&info).unwrap_or(0);
    let cb = arg(args, 0);
    let this_arg = arg(args, 1);
    // A predicate/callback method requires its first argument to be callable.
    let require_cb = |i: &mut Interp| {
        if cb.is_callable() {
            Ok(())
        } else {
            Err(i.make_error("TypeError", "callback is not a function"))
        }
    };
    match method {
        "forEach" => Some((|| {
            require_cb(i)?;
            for k in 0..len {
                let v = i.ta_read(&info, k);
                ab(i.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Num(k as f64), this.clone()],
                ))?;
            }
            Ok(Value::Undefined)
        })()),
        "every" => Some((|| {
            require_cb(i)?;
            for k in 0..len {
                let v = i.ta_read(&info, k);
                let r = ab(i.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Num(k as f64), this.clone()],
                ))?;
                if !i.to_boolean(&r) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        })()),
        "some" => Some((|| {
            require_cb(i)?;
            for k in 0..len {
                let v = i.ta_read(&info, k);
                let r = ab(i.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Num(k as f64), this.clone()],
                ))?;
                if i.to_boolean(&r) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        })()),
        "find" | "findIndex" | "findLast" | "findLastIndex" => Some((|| {
            require_cb(i)?;
            let want_value = method == "find" || method == "findLast";
            let reverse = method == "findLast" || method == "findLastIndex";
            let order: Vec<usize> = if reverse {
                (0..len).rev().collect()
            } else {
                (0..len).collect()
            };
            for k in order {
                let v = i.ta_read(&info, k);
                let r = ab(i.call(
                    cb.clone(),
                    this_arg.clone(),
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
        })()),
        "map" => Some((|| {
            require_cb(i)?;
            let new_ta = ta_species_create(i, this, info.kind, &[Value::Num(len as f64)], true)?;
            let new_info = map_ptr(&new_ta)
                .and_then(|p| i.typed_arrays.get(&p).copied())
                .ok_or_else(|| {
                    i.make_error("TypeError", "map: species did not return a TypedArray")
                })?;
            for k in 0..len {
                let v = i.ta_read(&info, k);
                let mapped = ab(i.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v, Value::Num(k as f64), this.clone()],
                ))?;
                ab(i.ta_store(&new_info, k, &mapped))?;
            }
            Ok(new_ta)
        })()),
        "filter" => Some((|| {
            require_cb(i)?;
            let mut kept: Vec<Value> = Vec::new();
            for k in 0..len {
                let v = i.ta_read(&info, k);
                let r = ab(i.call(
                    cb.clone(),
                    this_arg.clone(),
                    &[v.clone(), Value::Num(k as f64), this.clone()],
                ))?;
                if i.to_boolean(&r) {
                    kept.push(v);
                }
            }
            let new_ta =
                ta_species_create(i, this, info.kind, &[Value::Num(kept.len() as f64)], true)?;
            let new_info = map_ptr(&new_ta)
                .and_then(|p| i.typed_arrays.get(&p).copied())
                .ok_or_else(|| {
                    i.make_error("TypeError", "filter: species did not return a TypedArray")
                })?;
            for (n, v) in kept.iter().enumerate() {
                ab(i.ta_store(&new_info, n, v))?;
            }
            Ok(new_ta)
        })()),
        "reduce" | "reduceRight" => Some((|| {
            require_cb(i)?;
            let right = method == "reduceRight";
            let order: Vec<usize> = if right {
                (0..len).rev().collect()
            } else {
                (0..len).collect()
            };
            let mut acc: Value;
            let mut start = 0;
            if args.len() >= 2 {
                acc = arg(args, 1);
            } else {
                if len == 0 {
                    return Err(
                        i.make_error("TypeError", "Reduce of empty array with no initial value")
                    );
                }
                acc = i.ta_read(&info, order[0]);
                start = 1;
            }
            for &k in &order[start..] {
                let v = i.ta_read(&info, k);
                acc = ab(i.call(
                    cb.clone(),
                    Value::Undefined,
                    &[acc, v, Value::Num(k as f64), this.clone()],
                ))?;
            }
            Ok(acc)
        })()),
        "indexOf" | "lastIndexOf" => Some((|| {
            let search = arg(args, 0);
            if len == 0 {
                return Ok(Value::Num(-1.0));
            }
            let last = method == "lastIndexOf";
            // fromIndex: ToIntegerOrInfinity when *present* (even if explicitly `undefined`, which
            // coerces to 0); the default when absent is 0 (indexOf) / len-1 (lastIndexOf).
            let from = if args.len() >= 2 {
                let n = ab(i.to_number(&arg(args, 1)))?;
                if n.is_nan() {
                    0.0
                } else {
                    n.trunc()
                }
            } else if last {
                (len - 1) as f64
            } else {
                0.0
            };
            // Argument coercion may have detached/resized the buffer; re-derive the live length
            // (indexOf/lastIndexOf only inspect indices that still HasProperty, i.e. in-bounds).
            let curlen = match i.ta_len(&info) {
                Some(l) => l,
                None => return Ok(Value::Num(-1.0)),
            };
            let order: Vec<i64> = if last {
                let k = if from >= 0.0 {
                    from.min((len - 1) as f64) as i64
                } else {
                    (len as f64 + from) as i64
                };
                (0..=k).rev().collect()
            } else {
                let k = if from >= 0.0 {
                    from as i64
                } else {
                    (len as f64 + from).max(0.0) as i64
                };
                (k..len as i64).collect()
            };
            for k in order {
                if k < 0 || k as usize >= curlen {
                    continue;
                }
                let v = i.ta_read(&info, k as usize);
                if i.strict_equals(&v, &search) {
                    return Ok(Value::Num(k as f64));
                }
            }
            Ok(Value::Num(-1.0))
        })()),
        "includes" => Some((|| {
            let search = arg(args, 0);
            // A zero-length array is false before ToIntegerOrInfinity(fromIndex) runs any user code.
            if len == 0 {
                return Ok(Value::Bool(false));
            }
            let from = match args.get(1) {
                Some(v) if !matches!(v, Value::Undefined) => {
                    let n = ab(i.to_number(v))?;
                    if n.is_nan() {
                        0.0
                    } else {
                        n.trunc()
                    }
                }
                _ => 0.0,
            };
            // includes iterates the *captured* length and reads out-of-bounds indices as `undefined`
            // (so `includes(undefined)` is true after a mid-coercion detach/shrink).
            let lo = if from < 0.0 {
                (len as f64 + from).max(0.0) as usize
            } else {
                (from as usize).min(len)
            };
            for k in lo..len {
                let v = i.ta_read(&info, k);
                if same_value_zero(&v, &search) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        })()),
        "fill" => Some((|| {
            if i.immutable_buffers.contains(&info.buffer) {
                return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
            }
            // The fill value is coerced to the element type exactly once.
            let value = if info.kind.is_bigint() {
                Value::BigInt(ab(i.to_bigint(&arg(args, 0)))?)
            } else {
                Value::Num(ab(i.to_number(&arg(args, 0)))?)
            };
            let start = rel_index(ab(i.to_number(&arg(args, 1)))?, len);
            let end = match args.get(2) {
                Some(v) if !matches!(v, Value::Undefined) => rel_index(ab(i.to_number(v))?, len),
                _ => len,
            };
            // Re-validate after coercions: a detach/shrink that put the view out of bounds throws.
            let curlen = i
                .ta_len(&info)
                .ok_or_else(|| i.make_error("TypeError", "TypedArray is out of bounds"))?;
            let start = start.min(curlen);
            let end = end.min(curlen);
            for k in start..end {
                ab(i.ta_store(&info, k, &value))?;
            }
            Ok(this.clone())
        })()),
        "copyWithin" => Some((|| {
            if i.immutable_buffers.contains(&info.buffer) {
                return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
            }
            let to = rel_index(ab(i.to_number(&arg(args, 0)))?, len);
            let from = rel_index(ab(i.to_number(&arg(args, 1)))?, len);
            let final_ = match args.get(2) {
                Some(v) if !matches!(v, Value::Undefined) => rel_index(ab(i.to_number(v))?, len),
                _ => len,
            };
            let count = (final_ as i64 - from as i64).min(len as i64 - to as i64);
            // Re-validate after coercions.
            let curlen = i
                .ta_len(&info)
                .ok_or_else(|| i.make_error("TypeError", "TypedArray is out of bounds"))?;
            let to = to.min(curlen);
            let from = from.min(curlen);
            let count = count
                .min(curlen as i64 - to as i64)
                .min(curlen as i64 - from as i64);
            if count > 0 {
                let snap: Vec<Value> = (0..count as usize)
                    .map(|j| i.ta_read(&info, from + j))
                    .collect();
                for (j, v) in snap.iter().enumerate() {
                    ab(i.ta_store(&info, to + j, v))?;
                }
            }
            Ok(this.clone())
        })()),
        "sort" | "toSorted" => Some((|| {
            let cmp = arg(args, 0);
            if !matches!(cmp, Value::Undefined) && !cmp.is_callable() {
                return Err(i.make_error("TypeError", "comparefn must be a function"));
            }
            let in_place = method == "sort";
            if in_place && i.immutable_buffers.contains(&info.buffer) {
                return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
            }
            let mut vals: Vec<Value> = (0..len).map(|k| i.ta_read(&info, k)).collect();
            ta_merge_sort(i, &mut vals, &cmp, info.kind.is_bigint())?;
            if in_place {
                for (k, v) in vals.iter().enumerate() {
                    ab(i.ta_store(&info, k, v))?;
                }
                Ok(this.clone())
            } else {
                let (new_ta, new_info) = ta_create_same(i, info.kind, len)?;
                for (k, v) in vals.iter().enumerate() {
                    ab(i.ta_store(&new_info, k, v))?;
                }
                Ok(new_ta)
            }
        })()),
        "toReversed" => Some((|| {
            let (new_ta, new_info) = ta_create_same(i, info.kind, len)?;
            for k in 0..len {
                let v = i.ta_read(&info, len - 1 - k);
                ab(i.ta_store(&new_info, k, &v))?;
            }
            Ok(new_ta)
        })()),
        "reverse" => Some((|| {
            if i.immutable_buffers.contains(&info.buffer) {
                return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
            }
            for k in 0..len / 2 {
                let a = i.ta_read(&info, k);
                let b = i.ta_read(&info, len - 1 - k);
                ab(i.ta_store(&info, k, &b))?;
                ab(i.ta_store(&info, len - 1 - k, &a))?;
            }
            Ok(this.clone())
        })()),
        "slice" => Some((|| {
            let start = rel_index(ab(i.to_number(&arg(args, 0)))?, len);
            let end = match args.get(1) {
                Some(v) if !matches!(v, Value::Undefined) => rel_index(ab(i.to_number(v))?, len),
                _ => len,
            };
            let count = end.saturating_sub(start);
            let new_ta = ta_species_create(i, this, info.kind, &[Value::Num(count as f64)], true)?;
            let new_info = map_ptr(&new_ta)
                .and_then(|p| i.typed_arrays.get(&p).copied())
                .ok_or_else(|| i.make_error("TypeError", "slice: species did not return a view"))?;
            if count > 0 {
                // The species constructor ran user code; re-validate the source view.
                let curlen = i.ta_len(&info).ok_or_else(|| {
                    i.make_error("TypeError", "TypedArray source is out of bounds")
                })?;
                for j in 0..count {
                    let src_idx = start + j;
                    // Indices past the (possibly shrunk) source stay zero-initialised in the result.
                    if src_idx < curlen {
                        let v = i.ta_read(&info, src_idx);
                        ab(i.ta_store(&new_info, j, &v))?;
                    }
                }
            }
            Ok(new_ta)
        })()),
        "join" => Some((|| {
            let sep = match args.first() {
                Some(v) if !matches!(v, Value::Undefined) => ab(i.to_string(v))?.to_string(),
                _ => ",".to_string(),
            };
            let mut out = String::new();
            for k in 0..len {
                if k > 0 {
                    out.push_str(&sep);
                }
                let v = i.ta_read(&info, k);
                if !matches!(v, Value::Undefined | Value::Null) {
                    out.push_str(&ab(i.to_string(&v))?);
                }
            }
            Ok(Value::from_string(out))
        })()),
        _ => None,
    }
}

/// TypedArraySpeciesCreate(O, args): construct a new TypedArray using `O.constructor[@@species]`,
/// falling back to the receiver kind's intrinsic constructor. Validates the result is a TypedArray.
fn ta_species_create(
    i: &mut Interp,
    this: &Value,
    kind: TaKind,
    args: &[Value],
    write: bool,
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
    ta_validate_created(i, result, args, write)
}

/// TypedArrayCreate validation of a just-constructed result: it must be a TypedArray, in bounds
/// (not detached/shrunk), and — for a single-Number argument list — at least that long. When
/// `write` is set (the result will be written into), an immutable backing buffer is also rejected.
fn ta_validate_created(
    i: &mut Interp,
    result: Value,
    args: &[Value],
    write: bool,
) -> Result<Value, Value> {
    let new_info = match map_ptr(&result).and_then(|p| i.typed_arrays.get(&p).copied()) {
        Some(info) => info,
        None => {
            return Err(i.make_error(
                "TypeError",
                "TypedArray constructor did not return a TypedArray",
            ))
        }
    };
    let actual_len = match i.ta_len(&new_info) {
        Some(l) => l,
        None => {
            return Err(i.make_error(
                "TypeError",
                "TypedArray destination is detached or out of bounds",
            ))
        }
    };
    if write && i.immutable_buffers.contains(&new_info.buffer) {
        return Err(i.make_error(
            "TypeError",
            "TypedArray destination is backed by an immutable buffer",
        ));
    }
    if args.len() == 1 {
        if let Value::Num(n) = args[0] {
            if (actual_len as f64) < n {
                return Err(i.make_error("TypeError", "TypedArray destination is too small"));
            }
        }
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
    "entries" => tad_entries, "copyWithin" => tad_copywithin,
    "toReversed" => tad_toreversed, "toSorted" => tad_tosorted, "with" => tad_with,
}

/// OrdinaryCreateFromConstructor: a new object whose [[Prototype]] is `new.target.prototype` when
/// that's an object (subclassing / Reflect.construct), else the named intrinsic prototype. Safe to
/// call from any native constructor: outside a `new`, `new_target` is Undefined so the default wins.
/// CanBeHeldWeakly: an object, or a non-registered (collectable) symbol.
fn can_be_held_weakly(i: &Interp, v: &Value) -> bool {
    match v {
        Value::Obj(_) => true,
        Value::Sym(s) => {
            let _ = i;
            !crate::interpreter::sym_for_contains(s)
        }
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
    // TypedArray elements can never be made non-configurable (or non-writable), so sealing or
    // freezing any view that currently has elements fails at the first DefinePropertyOrThrow.
    if let Value::Obj(o) = obj {
        if let Some(info) = ta_info(i, o) {
            if i.ta_len(&info).unwrap_or(0) > 0 {
                return Ok(false);
            }
        }
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
    let mut ntobj: Gc = nt.as_obj()?.clone();
    // GetFunctionRealm unwraps bound functions and (unrevoked) proxies to their targets.
    for _ in 0..64 {
        if let Some((target, handler)) = i.proxies.get(&(Rc::as_ptr(&ntobj) as usize)) {
            if matches!(handler, Value::Null) {
                return None;
            }
            match target {
                Value::Obj(t) => {
                    ntobj = t.clone();
                    continue;
                }
                _ => return None,
            }
        }
        let bound = match &ntobj.borrow().call {
            Callable::Bound { target, .. } => Some(target.clone()),
            _ => None,
        };
        match bound {
            Some(t) => ntobj = t,
            None => break,
        }
    }
    let g = i.callee_realm_global(&ntobj)?;
    let rs = i.realms.get(&g)?;
    // Core intrinsics live in named fields; errors in error_protos; the rest in extra_protos.
    match default_proto {
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
    }
}

pub(crate) fn new_from_ctor(i: &mut Interp, default_proto: &str) -> Result<Gc, Value> {
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
        // An ArrayBuffer (or SharedArrayBuffer) backing store: identified by the live side table, or
        // by the [[ArrayBufferData]] marker for a detached buffer (still an ArrayBuffer, so it can't
        // fall through to the array-like path — using a detached buffer is a TypeError).
        Some(Value::Obj(o))
            if i.array_buffers.contains_key(&(Rc::as_ptr(o) as usize))
                || o.borrow().props.contains("__abMaxByteLength") =>
        {
            let bp = Rc::as_ptr(o) as usize;
            if !i.array_buffers.contains_key(&bp) {
                return Err(i.make_error("TypeError", "ArrayBuffer is detached"));
            }
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
            // A resizable ArrayBuffer or a growable SharedArrayBuffer makes an auto-length view
            // length-tracking.
            let resizable = matches!(
                o.borrow().props.get("__abResizable").map(|p| &p.value),
                Some(Value::Bool(true))
            );
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
            // GetMethod(@@iterator): a non-callable non-nullish value is a TypeError, and a
            // callable one is used (so a patched Array.prototype[@@iterator] is honored).
            let iter_key = well_known_key(i, "iterator");
            let iter_method = match &iter_key {
                Some(k) => ab(i.get_member(other, k))?,
                None => Value::Undefined,
            };
            if !matches!(iter_method, Value::Undefined | Value::Null) && !iter_method.is_callable()
            {
                return Err(i.make_error("TypeError", "@@iterator is not callable"));
            }
            let items = if iter_method.is_callable() {
                ab(i.iterate_with(other, iter_method))?
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
    // Reflect.construct), else the intrinsic %TypedArray.prototype% for this element type in
    // new.target's realm (GetPrototypeFromConstructor).
    let proto = match &i.new_target {
        nt @ Value::Obj(_) => match ab(i.get_member(&nt.clone(), "prototype"))? {
            Value::Obj(p) => Some(p),
            _ => ctor_realm_proto(i, &i.new_target.clone(), kind.name())
                .or_else(|| i.extra_protos.get(kind.name()).cloned()),
        },
        _ => i.extra_protos.get(kind.name()).cloned(),
    };
    let obj = Object::new(proto);
    let p = Rc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
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
    // An immutable target buffer can't be written — verified before any argument is coerced.
    if i.immutable_buffers.contains(&info.buffer) {
        return Err(i.make_error("TypeError", "Cannot write to an immutable ArrayBuffer"));
    }
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
    // Re-validate the target after offset coercion: an out-of-bounds (detached or shrunk-resizable)
    // target is a TypeError, and an immutable target buffer can't be written.
    let target_len = i
        .ta_len(&info)
        .ok_or_else(|| i.make_error("TypeError", "TypedArray target is out of bounds"))?;
    let source = arg(args, 0);
    let src_info = source
        .as_obj()
        .and_then(|o| i.typed_arrays.get(&(Rc::as_ptr(o) as usize)).copied());
    if let Some(src_info) = src_info {
        // SetTypedArrayFromTypedArray: the source view must be in bounds, the target long enough,
        // and the content types must match (mixing BigInt and Number is a TypeError).
        let src_len = i
            .ta_len(&src_info)
            .ok_or_else(|| i.make_error("TypeError", "TypedArray source is out of bounds"))?;
        if offset_n + src_len as f64 > target_len as f64 {
            return Err(i.make_error("RangeError", "source is too large for the target at offset"));
        }
        if src_info.kind.is_bigint() != info.kind.is_bigint() {
            return Err(i.make_error(
                "TypeError",
                "TypedArray content types (BigInt vs Number) differ",
            ));
        }
        let offset = offset_n as usize;
        // Snapshot the source first so an overlapping same-buffer copy reads pre-write values.
        let vals: Vec<Value> = (0..src_len).map(|k| i.ta_read(&src_info, k)).collect();
        for (k, v) in vals.iter().enumerate() {
            ab(i.ta_store(&info, offset + k, v))?;
        }
        return Ok(Value::Undefined);
    }
    // SetTypedArrayFromArrayLike: ToObject(source), read its length, bounds-check, then copy element
    // by element (coercing each value; a throwing getter leaves earlier elements written).
    let src = to_object_arg(i, source, "TypedArray.prototype.set")?;
    let src_len = ab(i.to_length(&src))?;
    if offset_n + src_len as f64 > target_len as f64 {
        return Err(i.make_error("RangeError", "source is too large for the target at offset"));
    }
    let offset = offset_n as usize;
    let src = Value::Obj(src);
    for k in 0..src_len {
        let item = ab(i.get_member(&src, &k.to_string()))?;
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
    // srcLength is the length at entry (0 if the view is already out of bounds); the begin/end
    // coercions below clamp against it even if they resize the buffer.
    let len = i.ta_len(&info).unwrap_or(0);
    let begin = rel_index(ab(i.to_number(&arg(args, 0)))?, len);
    let new_offset = info.offset + begin * es;
    let buf_val = ab(i.get_member(&this, "buffer"))?;
    // A length-tracking source subarrayed with no explicit end stays length-tracking: pass only
    // «buffer, byteOffset» so the result auto-sizes to the buffer (TypedArraySpeciesCreate honors
    // a subclass `@@species`).
    let end_absent = matches!(args.get(1), None | Some(Value::Undefined));
    if info.track && end_absent {
        return ta_species_create(
            i,
            &this,
            info.kind,
            &[buf_val, Value::Num(new_offset as f64)],
            false,
        );
    }
    let end = match arg(args, 1) {
        Value::Undefined => len,
        v => rel_index(ab(i.to_number(&v))?, len),
    };
    let new_len = end.saturating_sub(begin);
    ta_species_create(
        i,
        &this,
        info.kind,
        &[
            buf_val,
            Value::Num(new_offset as f64),
            Value::Num(new_len as f64),
        ],
        false,
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
            "keys" | "values" | "entries" | "toReversed" | "reverse" => 0,
            _ => 1,
        };
        it.def_method(&ta_proto, name, len, *f);
    }
    // %TypedArray%.prototype.toString is the very same function object as %Array.prototype.toString%.
    if let Some(p) = it.array_proto.borrow().props.get("toString").cloned() {
        ta_proto.borrow_mut().props.insert("toString", p);
    }
    // %TypedArray%.prototype[@@iterator] is the same function object as its own `values`.
    if let Some(sym) = it.iterator_sym.clone() {
        let k = Interp::sym_key(&sym);
        let values_prop = ta_proto.borrow().props.get("values").cloned();
        if let Some(p) = values_prop {
            ta_proto.borrow_mut().props.insert(k, p);
        }
    }
    it.def_method(&ta_proto, "set", 1, ta_set);
    it.def_method(&ta_proto, "subarray", 2, ta_subarray);
    it.def_method(&ta_proto, "toLocaleString", 0, |i, this, a| {
        // ValidateTypedArray then Invoke `toLocaleString` on each element, joining with ",". The
        // length is captured once; an out-of-bounds/detached receiver throws, but an element that
        // reads back `undefined` after a mid-iteration shrink is skipped.
        let info = map_ptr(&this).and_then(|p| i.typed_arrays.get(&p).copied());
        let info = info.ok_or_else(|| i.make_error("TypeError", "not a TypedArray"))?;
        let len = i
            .ta_len(&info)
            .ok_or_else(|| i.make_error("TypeError", "TypedArray is out of bounds"))?;
        let mut out = String::new();
        for k in 0..len {
            if k > 0 {
                out.push(',');
            }
            let v = i.ta_read(&info, k);
            if matches!(v, Value::Undefined | Value::Null) {
                continue;
            }
            let tls = ab(i.get_member(&v, "toLocaleString"))?;
            if !tls.is_callable() {
                return Err(i.make_error("TypeError", "toLocaleString is not a function"));
            }
            let s = ab(i.call(tls, v, &[arg(a, 0), arg(a, 1)]))?;
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
        let g = it.make_native(&format!("get {name}"), 0, getter);
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
        // BYTES_PER_ELEMENT is a non-writable, non-enumerable, non-configurable constant.
        proto.borrow_mut().props.insert(
            "BYTES_PER_ELEMENT",
            Property::data(Value::Num(kind.elsize() as f64), false, false, false),
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
        ctor.borrow_mut().props.insert(
            "BYTES_PER_ELEMENT",
            Property::data(Value::Num(kind.elsize() as f64), false, false, false),
        );
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
/// FromBase64: decode at most `max_len` bytes honoring `handling` (loose / strict /
/// stop-before-partial). Returns `(read, bytes)` where `read` is the code units consumed through
/// the last fully decoded chunk; `Err(())` is a syntax error.
fn b64_decode_spec(s: &str, url: bool, handling: &str, max_len: usize) -> (usize, Vec<u8>, bool) {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let is_ws = |c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ');
    let decode_chunk = |chunk: &[u8], throw_extra: bool, bytes: &mut Vec<u8>| -> Result<(), ()> {
        let mut n = 0u32;
        for (idx, &v) in chunk.iter().enumerate() {
            n |= (v as u32) << (18 - 6 * idx);
        }
        match chunk.len() {
            2 => {
                if throw_extra && n & 0xFFFF != 0 {
                    return Err(());
                }
                bytes.push((n >> 16) as u8);
            }
            3 => {
                if throw_extra && n & 0xFF != 0 {
                    return Err(());
                }
                bytes.extend([(n >> 16) as u8, (n >> 8) as u8]);
            }
            _ => bytes.extend([(n >> 16) as u8, (n >> 8) as u8, n as u8]),
        }
        Ok(())
    };
    let mut bytes = Vec::new();
    if max_len == 0 {
        return (0, bytes, false);
    }
    let (mut read, mut index) = (0usize, 0usize);
    let mut chunk: Vec<u8> = Vec::new();
    loop {
        while index < len && is_ws(chars[index]) {
            index += 1;
        }
        if index == len {
            if !chunk.is_empty() {
                match handling {
                    "stop-before-partial" => return (read, bytes, false),
                    "loose" => {
                        if chunk.len() == 1 {
                            return (read, bytes, true);
                        }
                        if decode_chunk(&chunk, false, &mut bytes).is_err() {
                            return (read, bytes, true);
                        }
                    }
                    _ => return (read, bytes, true), // strict: a partial chunk must be padded
                }
            }
            return (len, bytes, false);
        }
        let mut c = chars[index];
        index += 1;
        if c == '=' {
            if chunk.len() < 2 {
                return (read, bytes, true);
            }
            while index < len && is_ws(chars[index]) {
                index += 1;
            }
            if chunk.len() == 2 {
                if index == len {
                    if handling == "stop-before-partial" {
                        return (read, bytes, false);
                    }
                    return (read, bytes, true);
                }
                if chars[index] != '=' {
                    return (read, bytes, true);
                }
                index += 1;
                while index < len && is_ws(chars[index]) {
                    index += 1;
                }
            }
            if index < len {
                return (read, bytes, true); // trailing characters after padding
            }
            if decode_chunk(&chunk, handling == "strict", &mut bytes).is_err() {
                return (read, bytes, true);
            }
            return (len, bytes, false);
        }
        if url {
            c = match c {
                '+' | '/' => return (read, bytes, true),
                '-' => '+',
                '_' => '/',
                other => other,
            };
        }
        let Some(v) = B64_STD.iter().position(|&a| a as char == c) else {
            return (read, bytes, true);
        };
        let remaining = max_len - bytes.len();
        if (remaining == 1 && chunk.len() == 2) || (remaining == 2 && chunk.len() == 3) {
            return (read, bytes, false); // the next chunk wouldn't fit: stop before it
        }
        chunk.push(v as u8);
        if chunk.len() == 4 {
            if decode_chunk(&chunk, false, &mut bytes).is_err() {
                return (read, bytes, true);
            }
            chunk.clear();
            read = index;
            if bytes.len() == max_len {
                return (read, bytes, false);
            }
        }
    }
}
/// FromHex: decode at most `max_len` bytes; `read` is the number of code units consumed.
fn hex_decode_spec(s: &str, max_len: usize) -> (usize, Vec<u8>, bool) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    if !chars.len().is_multiple_of(2) {
        return (0, out, true);
    }
    let mut index = 0;
    while index < chars.len() && out.len() < max_len {
        let (Some(hi), Some(lo)) = (chars[index].to_digit(16), chars[index + 1].to_digit(16))
        else {
            return (index, out, true);
        };
        out.push((hi * 16 + lo) as u8);
        index += 2;
    }
    (index, out, false)
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

/// Read `{alphabet, lastChunkHandling}` for the base64 decode methods.
fn b64_decode_options(i: &mut Interp, opts: &Value) -> Result<(bool, Rc<str>), Value> {
    let url = b64_option_url(i, opts)?;
    let handling = if let Value::Obj(_) = opts {
        match ab(i.get_member(opts, "lastChunkHandling"))? {
            Value::Undefined => Rc::from("loose"),
            Value::Str(s) if matches!(&*s, "loose" | "strict" | "stop-before-partial") => s,
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "lastChunkHandling must be 'loose', 'strict', or 'stop-before-partial'",
                ))
            }
        }
    } else {
        Rc::from("loose")
    };
    Ok((url, handling))
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
        // Receiver validation, then options, then the (detachment-sensitive) byte read.
        u8_validate(i, &this)?;
        let url = b64_option_url(i, &arg(a, 0))?;
        let omit_padding = if let Value::Obj(_) = arg(a, 0) {
            let op = ab(i.get_member(&arg(a, 0), "omitPadding"))?;
            i.to_boolean(&op)
        } else {
            false
        };
        let bytes = u8_bytes(i, &this)?;
        Ok(Value::from_string(b64_encode(&bytes, url, !omit_padding)))
    });
    it.def_method(&ctor, "fromHex", 1, |i, _t, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "fromHex requires a string")),
        };
        let (_, bytes, err) = hex_decode_spec(&s, usize::MAX);
        if err {
            return Err(i.make_error("SyntaxError", "invalid hex string"));
        }
        make_u8array(i, bytes)
    });
    it.def_method(&ctor, "fromBase64", 1, |i, _t, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "fromBase64 requires a string")),
        };
        let (url, handling) = b64_decode_options(i, &arg(a, 1))?;
        let (_, bytes, err) = b64_decode_spec(&s, url, &handling, usize::MAX);
        if err {
            return Err(i.make_error("SyntaxError", "invalid base64 string"));
        }
        make_u8array(i, bytes)
    });
    it.def_method(&proto, "setFromHex", 1, |i, this, a| {
        let info = u8_validate_writable(i, &this)?;
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "setFromHex requires a string")),
        };
        let max = i
            .ta_len(&info)
            .ok_or_else(|| i.make_error("TypeError", "detached buffer"))?;
        // Valid chunks decoded before an error are still written, then the SyntaxError surfaces.
        let (read, bytes, err) = hex_decode_spec(&s, max);
        let result = u8_set_bytes(i, &info, read, &bytes)?;
        if err {
            return Err(i.make_error("SyntaxError", "invalid hex string"));
        }
        Ok(result)
    });
    it.def_method(&proto, "setFromBase64", 1, |i, this, a| {
        let info = u8_validate_writable(i, &this)?;
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "setFromBase64 requires a string")),
        };
        let (url, handling) = b64_decode_options(i, &arg(a, 1))?;
        let max = i
            .ta_len(&info)
            .ok_or_else(|| i.make_error("TypeError", "detached buffer"))?;
        // Valid chunks decoded before an error are still written, then the SyntaxError surfaces.
        let (read, bytes, err) = b64_decode_spec(&s, url, &handling, max);
        let result = u8_set_bytes(i, &info, read, &bytes)?;
        if err {
            return Err(i.make_error("SyntaxError", "invalid base64 string"));
        }
        Ok(result)
    });
}

/// ValidateUint8Array for the mutating methods: also rejects an immutable backing buffer
/// (before any argument is read).
fn u8_validate_writable(i: &mut Interp, this: &Value) -> Result<TaInfo, Value> {
    let info = u8_validate(i, this)?;
    if i.immutable_buffers.contains(&info.buffer) {
        return Err(i.make_error(
            "TypeError",
            "cannot write into a view over an immutable ArrayBuffer",
        ));
    }
    Ok(info)
}

/// ValidateUint8Array: the receiver must be a Uint8Array (detachment is checked separately).
fn u8_validate(i: &mut Interp, this: &Value) -> Result<TaInfo, Value> {
    let ptr = map_ptr(this).ok_or_else(|| i.make_error("TypeError", "not a Uint8Array"))?;
    let info = *i
        .typed_arrays
        .get(&ptr)
        .ok_or_else(|| i.make_error("TypeError", "not a Uint8Array"))?;
    if !matches!(info.kind, TaKind::U8) {
        return Err(i.make_error("TypeError", "requires a Uint8Array"));
    }
    Ok(info)
}

/// SetUint8ArrayBytes + result object: write decoded `bytes` into the view, report `{read, written}`.
fn u8_set_bytes(i: &mut Interp, info: &TaInfo, read: usize, bytes: &[u8]) -> Result<Value, Value> {
    if !bytes.is_empty() && i.immutable_buffers.contains(&info.buffer) {
        return Err(i.make_error(
            "TypeError",
            "cannot write into a view over an immutable ArrayBuffer",
        ));
    }
    if let Some(buf) = i.array_buffers.get_mut(&info.buffer) {
        buf[info.offset..info.offset + bytes.len()].copy_from_slice(bytes);
    }
    let result = i.new_object();
    set_data(&result, "read", Value::Num(read as f64));
    set_data(&result, "written", Value::Num(bytes.len() as f64));
    Ok(Value::Obj(result))
}

fn ta_of(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    if !is_constructor_value(&this) {
        return Err(i.make_error("TypeError", "TypedArray.of requires a constructor receiver"));
    }
    let len_args = [Value::Num(args.len() as f64)];
    let ta = ab(i.construct(this, &len_args))?;
    let ta = ta_validate_created(i, ta, &len_args, true)?;
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
    let source = arg(args, 0);
    // GetMethod(source, @@iterator): reading @@iterator off undefined/null throws, and a throwing
    // getter propagates. A callable result means the source is iterated; otherwise it is array-like.
    let iter_key = well_known_key(i, "iterator");
    let using_iter = match &iter_key {
        Some(k) => ab(i.get_member(&source, k))?,
        None => Value::Undefined,
    };
    if !matches!(using_iter, Value::Undefined | Value::Null) && !using_iter.is_callable() {
        return Err(i.make_error("TypeError", "@@iterator is not callable"));
    }
    if using_iter.is_callable() {
        // Iterable source: collect all values (reusing the already-fetched @@iterator), then
        // construct the target and fill it.
        let items = ab(i.iterate_with(&source, using_iter))?;
        let len_args = [Value::Num(items.len() as f64)];
        let ta = ab(i.construct(this, &len_args))?;
        let ta = ta_validate_created(i, ta, &len_args, true)?;
        for (k, v) in items.into_iter().enumerate() {
            let val = if mapfn.is_callable() {
                ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k as f64)]))?
            } else {
                v
            };
            ab(i.set_member(&ta, &k.to_string(), val))?;
        }
        return Ok(ta);
    }
    // Array-like source: ToObject, read its length, construct the target *before* visiting any
    // element, then read and set each element in turn.
    let src = Value::Obj(to_object_arg(i, source, "TypedArray.from")?);
    let len = ab(i.to_length(src.as_obj().unwrap()))?;
    let len_args = [Value::Num(len as f64)];
    let ta = ab(i.construct(this, &len_args))?;
    let ta = ta_validate_created(i, ta, &len_args, true)?;
    for k in 0..len {
        let v = ab(i.get_member(&src, &k.to_string()))?;
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

fn date_to_string(t: f64) -> String {
    match (date_str_part(t), time_str_part(t)) {
        (Some(d), Some(tm)) => format!("{d} {tm}"),
        _ => "Invalid Date".to_string(),
    }
}

fn date_ctor(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    // Called as a function (no `new`), Date ignores its arguments and returns the
    // current time as a string.
    if !matches!(i.new_target, Value::Obj(_)) {
        return Ok(Value::from_string(date_to_string(now_ms())));
    }
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
    it.def_method(&proto, "toTemporalInstant", 0, |i, this, _| {
        // RequireInternalSlot([[DateValue]]) then a Temporal.Instant at ms×10^6 ns.
        let ms = date_ms(i, &this)?;
        crate::temporal::instant_from_epoch_ms(i, ms)
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
        Ok(Value::from_string(date_to_string(t)))
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
    // toLocale* route through Intl.DateTimeFormat (which exists now).
    it.def_method(&proto, "toLocaleString", 0, |i, this, args| {
        let t = date_ms(i, &this)?;
        if !t.is_finite() {
            return Ok(Value::str("Invalid Date"));
        }
        // ToDateTimeOptions(options, "any", "all"): default to date AND time unless the caller
        // already requested a date or time component (or a dateStyle/timeStyle).
        let opts = date_all_default(i, &arg(args, 1))?;
        intl_delegate(
            i,
            "DateTimeFormat",
            arg(args, 0),
            opts,
            "format",
            &[Value::Num(t)],
        )
    });
    it.def_method(&proto, "toLocaleDateString", 0, |i, this, args| {
        let t = date_ms(i, &this)?;
        if !t.is_finite() {
            return Ok(Value::str("Invalid Date"));
        }
        let opts = date_style_default(i, &arg(args, 1), true)?;
        intl_delegate(
            i,
            "DateTimeFormat",
            arg(args, 0),
            opts,
            "format",
            &[Value::Num(t)],
        )
    });
    it.def_method(&proto, "toLocaleTimeString", 0, |i, this, args| {
        let t = date_ms(i, &this)?;
        if !t.is_finite() {
            return Ok(Value::str("Invalid Date"));
        }
        let opts = date_style_default(i, &arg(args, 1), false)?;
        intl_delegate(
            i,
            "DateTimeFormat",
            arg(args, 0),
            opts,
            "format",
            &[Value::Num(t)],
        )
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
    // The intrinsic %Symbol% (immune to globalThis.Symbol tampering), else the global binding.
    let sym = it
        .extra_protos
        .get("%SymbolCtor%")
        .map(|o| Value::Obj(o.clone()))
        .or_else(|| {
            it.global
                .borrow()
                .props
                .get("Symbol")
                .map(|p| p.value.clone())
        })?;
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
fn make_sparse_array(i: &mut Interp, len: usize) -> Result<Value, Value> {
    // ArrayCreate: a length past 2^32-1 is a RangeError.
    if len > 4294967295 {
        return Err(i.make_error("RangeError", "invalid array length"));
    }
    let arr = i.make_array(Vec::new());
    if let Value::Obj(o) = &arr {
        o.borrow_mut().props.insert(
            "length",
            crate::value::Property::data(Value::Num(len as f64), true, false, false),
        );
    }
    Ok(arr)
}

fn array_species_create(i: &mut Interp, original: &Value, len: usize) -> Result<Value, Value> {
    // IsArray pierces proxies (a proxy over an array is an array).
    if !json_is_array(i, original)? {
        return make_sparse_array(i, len);
    }
    let mut c = ab(i.get_member(original, "constructor"))?;
    // A constructor that is another realm's %Array% counts as the default (its @@species is
    // never read).
    if is_constructor_value(&c) {
        if let Value::Obj(co) = &c {
            let foreign_array = i.realms.values().any(|rs| {
                matches!(
                    rs.global.borrow().props.get("Array").map(|p| &p.value),
                    Some(Value::Obj(ac)) if Rc::ptr_eq(ac, co)
                        && !Rc::ptr_eq(&rs.global, &i.global)
                )
            });
            if foreign_array {
                return make_sparse_array(i, len);
            }
        }
    }
    if matches!(&c, Value::Obj(_)) {
        if let Some(key) = well_known_key(i, "species") {
            c = ab(i.get_member(&c, &key))?;
        }
        if matches!(c, Value::Null) {
            c = Value::Undefined;
        }
    }
    if matches!(c, Value::Undefined) {
        return make_sparse_array(i, len);
    }
    let array_ctor = i
        .global
        .borrow()
        .props
        .get("Array")
        .map(|p| p.value.clone());
    if let (Value::Obj(s), Some(Value::Obj(ac))) = (&c, &array_ctor) {
        if Rc::ptr_eq(s, ac) {
            return make_sparse_array(i, len);
        }
    }
    if !is_constructor_value(&c) {
        return Err(i.make_error("TypeError", "Array @@species is not a constructor"));
    }
    ab(i.construct(c, &[Value::Num(len as f64)]))
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
            } else if matches!(b.exotic, Exotic::Arguments) {
                "Arguments"
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
/// Like [`collection_iter`] but brand-checks the exact collection kind ("Set" / "Map").
fn collection_iter_kind(
    i: &mut Interp,
    this: &Value,
    kind: u8,
    want: &str,
) -> Result<Value, Value> {
    coll_ptr_kind(i, this, Some(want))?;
    collection_iter(i, this, kind)
}

/// forEach shared by Map/Set, brand-checking the exact kind.
fn collection_for_each(
    i: &mut Interp,
    this: Value,
    a: &[Value],
    want: Option<&str>,
) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, want)?;
    let cb = arg(a, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "forEach callback is not callable"));
    }
    let cb_this = arg(a, 1);
    // Iterate the LIVE backing list by index (positions are stable — deletes leave tombstones), so
    // entries appended during the callback are visited and deleted entries are skipped.
    let mut idx = 0usize;
    loop {
        let entry = i.map_data.get(&ptr).and_then(|e| e.get(idx).cloned());
        idx += 1;
        let (k, v) = match entry {
            Some(kv) => kv,
            None => break,
        };
        if is_tombstone(i, &k) {
            continue;
        }
        ab(i.call(cb.clone(), cb_this.clone(), &[v, k, this.clone()]))?;
    }
    Ok(Value::Undefined)
}

fn collection_iter(i: &mut Interp, this: &Value, kind: u8) -> Result<Value, Value> {
    coll_ptr(i, this)?; // brand check (a real Map/Set)
    let is_set = this
        .as_obj()
        .and_then(|o| o.borrow().props.get("__ck").map(|p| p.value.clone()))
        .map(|v| matches!(v, Value::Str(ref s) if &**s == "Set"))
        .unwrap_or(false);
    let key = if is_set {
        "%SetIteratorPrototype%"
    } else {
        "%MapIteratorPrototype%"
    };
    let proto = i
        .extra_protos
        .get(key)
        .cloned()
        .or_else(|| i.extra_protos.get("%IteratorPrototype%").cloned());
    let obj = Object::new(proto);
    set_builtin(&obj, "__ci_coll", this.clone());
    set_builtin(&obj, "__ci_index", Value::Num(0.0));
    set_builtin(&obj, "__ci_kind", Value::Num(kind as f64));
    Ok(Value::Obj(obj))
}

/// `next()` for a Map/Set iterator: reads the live backing entries at the current index (so entries
/// appended during iteration are observed). The `__ci_coll` slot is the brand.
fn map_set_iter_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let coll = this
        .as_obj()
        .and_then(|o| o.borrow().props.get("__ci_coll").map(|p| p.value.clone()));
    let coll = match coll {
        Some(c) => c,
        None => return Err(i.make_error("TypeError", "not a Map/Set Iterator")),
    };
    let obj = this.as_obj().unwrap();
    let num = |o: &Gc, k: &str| -> f64 {
        match o.borrow().props.get(k).map(|p| p.value.clone()) {
            Some(Value::Num(n)) => n,
            _ => 0.0,
        }
    };
    // A once-exhausted iterator stays done, even if the collection later grows.
    if matches!(
        obj.borrow().props.get("__ci_done").map(|p| &p.value),
        Some(Value::Bool(true))
    ) {
        return Ok(iter_result(i, Value::Undefined, true));
    }
    let mut idx = num(obj, "__ci_index") as usize;
    let kind = num(obj, "__ci_kind") as u8;
    let coll_ptr = map_ptr(&coll);
    // Skip tombstoned (deleted) slots so the iterator observes a live view.
    loop {
        let entry = coll_ptr
            .and_then(|p| i.map_data.get(&p))
            .and_then(|e| e.get(idx).cloned());
        match entry {
            Some((k, v)) => {
                idx += 1;
                if is_tombstone(i, &k) {
                    continue;
                }
                set_internal(obj, "__ci_index", Value::Num(idx as f64));
                let val = match kind {
                    1 => k,
                    2 => i.make_array(vec![k, v]),
                    _ => v,
                };
                return Ok(iter_result(i, val, false));
            }
            None => {
                set_internal(obj, "__ci_index", Value::Num(idx as f64));
                set_internal(obj, "__ci_done", Value::Bool(true));
                return Ok(iter_result(i, Value::Undefined, true));
            }
        }
    }
}

fn install_collections(it: &mut Interp) {
    // A unique private object used as the deleted-entry tombstone key (see map_tombstone).
    it.extra_protos.insert("%MapTombstone%", Object::new(None));
    // %MapIteratorPrototype% / %SetIteratorPrototype%: distinct iterator prototypes (proto is
    // %IteratorPrototype%) with the right @@toStringTag and a live `next`.
    for (key, tag) in [
        ("%MapIteratorPrototype%", "Map Iterator"),
        ("%SetIteratorPrototype%", "Set Iterator"),
    ] {
        let proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
        set_to_string_tag(it, &proto, tag);
        it.def_method(&proto, "next", 0, map_set_iter_next);
        it.extra_protos.insert(key, proto);
    }
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
        // CoerceKey: -0 is canonicalized to +0 (so the callback and stored key see +0).
        let key = canonicalize_map_key(arg(a, 0));
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
        // The callback may have inserted the key; the computed value overwrites that mutation.
        if let Some(entry) = i
            .map_data
            .get_mut(&ptr)
            .and_then(|d| d.iter_mut().find(|(k, _)| same_value_zero(k, &key)))
        {
            entry.1 = value.clone();
        } else {
            i.map_data
                .entry(ptr)
                .or_default()
                .push((key, value.clone()));
        }
        Ok(value)
    });
}

/// The receiver Set's values (deduped insertion order). Errors if `this` isn't a Set.
fn set_values(i: &mut Interp, this: &Value) -> Result<Vec<Value>, Value> {
    // Requires a real Set [[SetData]] slot — a Map (which shares the map_data table) is rejected.
    let p = coll_ptr_kind(i, this, Some("Set"))?;
    Ok(i.map_data[&p].iter().map(|(k, _)| k.clone()).collect())
}
/// Build a fresh Set from `values` (deduped via SameValueZero).
fn new_set(i: &mut Interp, values: Vec<Value>) -> Value {
    let obj =
        new_from_ctor(i, "Set").unwrap_or_else(|_| Object::new(i.extra_protos.get("Set").cloned()));
    let ptr = Rc::as_ptr(&obj) as usize;
    let mut entries: Vec<(Value, Value)> = Vec::new();
    for v in values {
        // Set records canonicalize -0 to +0.
        let v = canonicalize_map_key(v);
        if !entries.iter().any(|(k, _)| same_value_zero(k, &v)) {
            entries.push((v.clone(), v));
        }
    }
    i.gc_pin(&obj);
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
/// Open a set-like's keys iterator record: `(iterator, nextMethod)`.
fn set_like_open(i: &mut Interp, keys: &Value, other: &Value) -> Result<(Value, Value), Value> {
    let iter = ab(i.call(keys.clone(), other.clone(), &[]))?;
    let next = ab(i.get_member(&iter, "next"))?;
    if !next.is_callable() {
        return Err(i.make_error("TypeError", "set-like keys iterator has no next method"));
    }
    Ok((iter, next))
}

/// Step a set-like keys iterator: `Some(value)` or `None` when done. `-0` is canonicalized to `+0`.
fn set_like_next(i: &mut Interp, iter: &Value, next: &Value) -> Result<Option<Value>, Value> {
    let r = ab(i.call(next.clone(), iter.clone(), &[]))?;
    if !matches!(r, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "iterator result is not an object"));
    }
    let done = ab(i.get_member(&r, "done"))?;
    if i.to_boolean(&done) {
        Ok(None)
    } else {
        Ok(Some(canonicalize_map_key(ab(i.get_member(&r, "value"))?)))
    }
}

/// IteratorClose a set-like keys iterator on early exit (swallowing errors).
fn set_like_close(i: &mut Interp, iter: &Value) {
    if let Ok(ret) = i.get_member(iter, "return") {
        if ret.is_callable() {
            let _ = i.call(ret, iter.clone(), &[]);
        }
    }
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
        // GetSetRecord (which may run `has`/`size`/`keys` getters that mutate this Set) happens
        // BEFORE the result is snapshotted from O.[[SetData]], per spec.
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        let mut vals = set_values(i, &this)?;
        for k in set_like_keys(i, &keys, &arg(a, 0))? {
            if !vals.iter().any(|v| same_value_zero(v, &k)) {
                vals.push(k);
            }
        }
        Ok(new_set(i, vals))
    });
    it.def_method(&sp, "intersection", 1, |i, this, a| {
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let vals = set_values(i, &this)?;
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
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let vals = set_values(i, &this)?;
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
        // GetSetRecord (which may mutate the receiver via getters) precedes the snapshot. The result
        // data keeps stable slots: toggling a present key empties its slot, and adding a key reuses
        // the first empty slot (else appends) — so a removed-then-re-added key keeps its position.
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        // Each slot remembers its key and whether it is present. Toggling a present key clears it;
        // re-adding a key reuses *its own* cleared slot; a brand-new key appends.
        let mut result: Vec<(Value, bool)> = set_values(i, &this)?
            .into_iter()
            .map(|v| (v, true))
            .collect();
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        while let Some(k) = set_like_next(i, &iter, &next)? {
            if let Some(slot) = result
                .iter_mut()
                .find(|(v, present)| *present && same_value_zero(v, &k))
            {
                slot.1 = false;
            } else if let Some(slot) = result
                .iter_mut()
                .find(|(v, present)| !*present && same_value_zero(v, &k))
            {
                slot.1 = true;
            } else {
                result.push((k, true));
            }
        }
        let out: Vec<Value> = result
            .into_iter()
            .filter(|(_, present)| *present)
            .map(|(v, _)| v)
            .collect();
        Ok(new_set(i, out))
    });
    it.def_method(&sp, "isSubsetOf", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, _keys, other_size) = set_record(i, &arg(a, 0))?;
        // A larger set cannot be a subset; otherwise every element must be in the other. The
        // receiver's data is walked LIVE by index (the `has` callback may delete entries).
        if (coll_live_len(i, ptr) as f64) > other_size {
            return Ok(Value::Bool(false));
        }
        let mut idx = 0usize;
        loop {
            let entry = i.map_data.get(&ptr).and_then(|e| e.get(idx).cloned());
            idx += 1;
            let (k, _) = match entry {
                Some(kv) => kv,
                None => break,
            };
            if is_tombstone(i, &k) {
                continue;
            }
            if !set_like_has(i, &has, &arg(a, 0), &k)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "isSupersetOf", 1, |i, this, a| {
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let vals = set_values(i, &this)?;
        // A smaller set cannot be a superset; otherwise every other key must be in this. The other's
        // keys are iterated lazily and the iterator is closed if a missing key exits early.
        if (vals.len() as f64) < other_size {
            return Ok(Value::Bool(false));
        }
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        while let Some(k) = set_like_next(i, &iter, &next)? {
            if !vals.iter().any(|v| same_value_zero(v, &k)) {
                set_like_close(i, &iter);
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "isDisjointFrom", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let vals = set_values(i, &this)?;
        if (coll_live_len(i, ptr) as f64) <= other_size {
            // Walk this Set LIVE by index (the `has` callback may mutate it), probing the other.
            let mut idx = 0usize;
            loop {
                let entry = i.map_data.get(&ptr).and_then(|e| e.get(idx).cloned());
                idx += 1;
                let (k, _) = match entry {
                    Some(kv) => kv,
                    None => break,
                };
                if is_tombstone(i, &k) {
                    continue;
                }
                if set_like_has(i, &has, &arg(a, 0), &k)? {
                    return Ok(Value::Bool(false));
                }
            }
        } else {
            // Iterate the other's keys lazily, probing this Set; close the iterator on early exit.
            let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
            while let Some(k) = set_like_next(i, &iter, &next)? {
                if vals.iter().any(|v| same_value_zero(v, &k)) {
                    set_like_close(i, &iter);
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
    i.gc_pin(&obj);
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

/// The Map/Set tombstone sentinel key: a unique engine-private object placed at a deleted entry's
/// slot so positions stay stable (live iteration observes additions and skips deletions, per spec).
fn map_tombstone(i: &Interp) -> Value {
    match i.extra_protos.get("%MapTombstone%") {
        Some(o) => Value::Obj(o.clone()),
        None => Value::Undefined,
    }
}

fn is_tombstone(i: &Interp, k: &Value) -> bool {
    match (k.as_obj(), i.extra_protos.get("%MapTombstone%")) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

/// Count the live (non-tombstone) entries of a collection.
fn coll_live_len(i: &Interp, ptr: usize) -> usize {
    i.map_data
        .get(&ptr)
        .map(|e| e.iter().filter(|(k, _)| !is_tombstone(i, k)).count())
        .unwrap_or(0)
}

fn map_size(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
    Ok(Value::Num(coll_live_len(i, ptr) as f64))
}

fn set_size(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
    Ok(Value::Num(coll_live_len(i, ptr) as f64))
}

/// CoerceKey for Map/Set: `-0` is canonicalized to `+0` so a stored key (and any key handed to a
/// callback or iterated) is `+0`, per spec.
fn canonicalize_map_key(k: Value) -> Value {
    match k {
        Value::Num(n) if n == 0.0 && n.is_sign_negative() => Value::Num(0.0),
        other => other,
    }
}

/// Map and Set share almost everything; `is_set` flips key/value handling and method names.
fn install_map_like(it: &mut Interp, name: &'static str, is_set: bool, ctor_fn: NativeFn) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());

    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = canonicalize_map_key(arg(a, 0));
            let e = i.map_data.entry(ptr).or_default();
            if !e.iter().any(|(k, _)| same_value_zero(k, &key)) {
                e.push((key.clone(), key));
            }
            Ok(this)
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let (key, val) = (canonicalize_map_key(arg(a, 0)), arg(a, 1));
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
    // Delete marks the matching entry with a tombstone (keeping its slot) so a concurrent forEach /
    // iterator sees stable positions; the entry is otherwise treated as absent everywhere.
    let delete_fn: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = canonicalize_map_key(arg(a, 0));
            let tomb = map_tombstone(i);
            let mut removed = false;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                for slot in e.iter_mut() {
                    if same_value_zero(&slot.0, &key) {
                        slot.0 = tomb.clone();
                        slot.1 = Value::Undefined;
                        removed = true;
                        break;
                    }
                }
            }
            Ok(Value::Bool(removed))
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = canonicalize_map_key(arg(a, 0));
            let tomb = map_tombstone(i);
            let mut removed = false;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                for slot in e.iter_mut() {
                    if same_value_zero(&slot.0, &key) {
                        slot.0 = tomb.clone();
                        slot.1 = Value::Undefined;
                        removed = true;
                        break;
                    }
                }
            }
            Ok(Value::Bool(removed))
        }
    };
    it.def_method(&proto, "delete", 1, delete_fn);
    // clear/forEach/values/keys/entries/size are shared shapes but must brand-check the exact kind
    // (Set.prototype.clear rejects a Map and vice-versa), so select a kind-specific fn pointer.
    let clear_fn: NativeFn = if is_set {
        |i, this, _| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                e.clear();
            }
            Ok(Value::Undefined)
        }
    } else {
        |i, this, _| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                e.clear();
            }
            Ok(Value::Undefined)
        }
    };
    it.def_method(&proto, "clear", 0, clear_fn);
    let for_each_fn: NativeFn = if is_set {
        |i, this, a| collection_for_each(i, this, a, Some("Set"))
    } else {
        |i, this, a| collection_for_each(i, this, a, Some("Map"))
    };
    it.def_method(&proto, "forEach", 1, for_each_fn);
    let (values_fn, keys_fn, entries_fn): (NativeFn, NativeFn, NativeFn) = if is_set {
        (
            |i, this, _| collection_iter_kind(i, &this, 0, "Set"),
            |i, this, _| collection_iter_kind(i, &this, 1, "Set"),
            |i, this, _| collection_iter_kind(i, &this, 2, "Set"),
        )
    } else {
        (
            |i, this, _| collection_iter_kind(i, &this, 0, "Map"),
            |i, this, _| collection_iter_kind(i, &this, 1, "Map"),
            |i, this, _| collection_iter_kind(i, &this, 2, "Map"),
        )
    };
    it.def_method(&proto, "values", 0, values_fn);
    it.def_method(&proto, "entries", 0, entries_fn);
    if is_set {
        // Set.prototype.keys is the *same* function object as Set.prototype.values.
        let _ = keys_fn;
        let values_prop = proto.borrow().props.get("values").cloned();
        if let Some(p) = values_prop {
            proto.borrow_mut().props.insert("keys", p);
        }
    } else {
        it.def_method(&proto, "keys", 0, keys_fn);
    }

    // `size` accessor.
    // Map.prototype.size and Set.prototype.size each brand-check their own kind (a Set passed to
    // Map.prototype.size, or vice versa, is a TypeError — it lacks the right internal slot).
    let size_getter = it.make_native("get size", 0, if is_set { set_size } else { map_size });
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
            i.gc_pin(&m);
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
            // The callback may have inserted the key; the computed value overwrites that mutation.
            if let Some(entry) = i
                .map_data
                .get_mut(&ptr)
                .and_then(|d| d.iter_mut().find(|(k, _)| same_value_zero(k, &key)))
            {
                entry.1 = value.clone();
            } else {
                i.map_data
                    .entry(ptr)
                    .or_default()
                    .push((key, value.clone()));
            }
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
            // Missing/undefined trap: forward to the target's [[Set]] with the original receiver,
            // returning its actual success boolean. [[Set]] reports failure as `false` (never a strict
            // PutValue throw), so evaluate it in a non-strict context.
            let receiver = if a.len() > 3 {
                arg(a, 3)
            } else {
                target.clone()
            };
            let saved = i.strict;
            i.strict = false;
            let r = i.set_member_recv(&ptarget, &key, value, receiver);
            i.strict = saved;
            return Ok(Value::Bool(ab(r)?));
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
        // A mapped arguments index reports the live parameter value.
        if let Some(v) = i.mapped_arg_value(Rc::as_ptr(&o) as usize, &key) {
            if let Some(p) = o.borrow_mut().props.get_mut(&key) {
                p.value = v;
            }
        }
        // A TypedArray canonical numeric index reads from the buffer (out-of-range → undefined).
        if let Some(info) = ta_info(i, &o) {
            if i.canonical_numeric_index(&key).is_some() {
                return Ok(match i.ta_index_kind(&info, &key) {
                    TaIndex::Element(idx) => {
                        let val = i.ta_read(&info, idx);
                        descriptor_from_prop(i, Property::data(val, true, true, true))
                    }
                    _ => Value::Undefined,
                });
            }
        }
        // A proxy's [[GetOwnProperty]] goes through its getOwnPropertyDescriptor trap.
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            return proxy_gopd_value(i, &target, &handler, &key);
        }
        let ptr = Rc::as_ptr(&o) as usize;
        if i.is_namespace(ptr) {
            if let Some(res) = i.namespace_own_property(ptr, &key) {
                return Ok(descriptor_from_prop(i, ab(res)?));
            }
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
    make_bound_len(i, target, bound_args, 1.0)
}

/// Like [`make_bound`] but with an explicit observable `length`. These internal closures are
/// anonymous built-in functions, so they carry own `name` (the empty string) and `length` data
/// properties (both non-writable, non-enumerable, configurable), which test262's verifyProperty checks.
pub(crate) fn make_bound_len(
    i: &Interp,
    target: NativeFn,
    bound_args: Vec<Value>,
    length: f64,
) -> Value {
    let t = i.make_native("", 1, target);
    let obj = Object::new(Some(i.function_proto.clone()));
    {
        let mut b = obj.borrow_mut();
        b.call = Callable::Bound {
            target: t,
            this: Value::Undefined,
            args: bound_args,
        };
        b.props.insert(
            "length",
            Property::data(Value::Num(length), false, false, true),
        );
        b.props
            .insert("name", Property::data(Value::str(""), false, false, true));
    }
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
    new_promise_capability_full(i, ctor).map(|(p, _, _)| p)
}

/// NewPromiseCapability returning `(promise, resolve, reject)`. The resolve/reject are the
/// constructor's own capability functions, so a combinator that resolves through them works with a
/// subclass / foreign Promise constructor (not just the native machinery).
fn new_promise_capability_full(
    i: &mut Interp,
    ctor: &Value,
) -> Result<(Value, Value, Value), Value> {
    if !ctor.is_callable() {
        return Err(i.make_error(
            "TypeError",
            "NewPromiseCapability: receiver is not a constructor",
        ));
    }
    let cap = i.new_object();
    // GetCapabilitiesExecutor is an anonymous built-in function of length 2.
    let executor = make_bound_len(i, capability_executor, vec![Value::Obj(cap.clone())], 2.0);
    let promise = ab(i.construct(ctor.clone(), &[executor]))?;
    let resolve = ab(i.get_member(&Value::Obj(cap.clone()), "__resolve"))?;
    let reject = ab(i.get_member(&Value::Obj(cap.clone()), "__reject"))?;
    if !resolve.is_callable() || !reject.is_callable() {
        return Err(i.make_error("TypeError", "promise capability functions are not callable"));
    }
    Ok((promise, resolve, reject))
}

/// `Promise.all` per-element fulfill reaction. `args = [resultPromise, index, value]`.
fn promise_all_element(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let state = arg(args, 0);
    let idx = ab(i.to_number(&arg(args, 1)))? as usize;
    let already = arg(args, 2);
    let resolve_fn = arg(args, 3);
    let value = arg(args, 4);
    // [[AlreadyCalled]]: a second settlement of the same element is a no-op.
    if let Value::Obj(o) = &already {
        if matches!(
            o.borrow().props.get("__called").map(|p| &p.value),
            Some(Value::Bool(true))
        ) {
            return Ok(Value::Undefined);
        }
        set_internal(o, "__called", Value::Bool(true));
    }
    let results = ab(i.get_member(&state, "__results"))?;
    // CreateDataProperty: a direct own data property, so Array.prototype index setters aren't invoked.
    if let Value::Obj(o) = &results {
        crate::value::set_data(o, &idx.to_string(), value);
    }
    let rem_v = ab(i.get_member(&state, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&state, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        ab(i.call(resolve_fn, Value::Undefined, &[results]))?;
    }
    Ok(Value::Undefined)
}

/// Subscribe a Promise combinator's element handlers via the resolved item's user-visible `.then`
/// (per spec). A throwing `.then` getter/call rejects the combinator's result promise. Returns
/// `false` if the combinator should bail out (already rejected).
fn combinator_then(i: &mut Interp, reject: &Value, next: Value, on_f: Value, on_r: Value) -> bool {
    let then = match i.get_member(&next, "then") {
        Ok(t) => t,
        Err(e) => {
            let r = crate::interpreter::abrupt_value(e);
            let _ = i.call(reject.clone(), Value::Undefined, &[r]);
            return false;
        }
    };
    match i.call(then, next, &[on_f, on_r]) {
        Ok(_) => true,
        Err(e) => {
            let r = crate::interpreter::abrupt_value(e);
            let _ = i.call(reject.clone(), Value::Undefined, &[r]);
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
    let state = arg(args, 0);
    let idx = ab(i.to_number(&arg(args, 1)))? as usize;
    let already = arg(args, 2);
    let resolve_fn = arg(args, 3);
    let value = arg(args, 4);
    // The fulfill and reject functions for one index share this [[AlreadyCalled]] record.
    if let Value::Obj(o) = &already {
        if matches!(
            o.borrow().props.get("__called").map(|p| &p.value),
            Some(Value::Bool(true))
        ) {
            return Ok(Value::Undefined);
        }
        set_internal(o, "__called", Value::Bool(true));
    }
    let status = i.new_object();
    set_data(
        &status,
        "status",
        Value::str(if fulfilled { "fulfilled" } else { "rejected" }),
    );
    set_data(&status, if fulfilled { "value" } else { "reason" }, value);
    let results = ab(i.get_member(&state, "__results"))?;
    if let Value::Obj(o) = &results {
        crate::value::set_data(o, &idx.to_string(), Value::Obj(status));
    }
    let rem_v = ab(i.get_member(&state, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&state, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        ab(i.call(resolve_fn, Value::Undefined, &[results]))?;
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
    let state = arg(a, 0);
    let idx = ab(i.to_number(&arg(a, 1)))? as usize;
    let already = arg(a, 2);
    let reject_fn = arg(a, 3);
    let reason = arg(a, 4);
    if let Value::Obj(o) = &already {
        if matches!(
            o.borrow().props.get("__called").map(|p| &p.value),
            Some(Value::Bool(true))
        ) {
            return Ok(Value::Undefined);
        }
        set_internal(o, "__called", Value::Bool(true));
    }
    let errors = ab(i.get_member(&state, "__errors"))?;
    ab(i.set_member(&errors, &idx.to_string(), reason))?;
    let rem_v = ab(i.get_member(&state, "__remaining"))?;
    let rem = ab(i.to_number(&rem_v))? - 1.0;
    ab(i.set_member(&state, "__remaining", Value::Num(rem)))?;
    if rem == 0.0 {
        let agg = make_aggregate_error(i, errors)?;
        ab(i.call(reject_fn, Value::Undefined, &[agg]))?;
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
        // NewPromiseCapability(C): resolve/reject route through C's own capability so a subclass or
        // foreign Promise constructor works, not just the native machinery.
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        // GetPromiseResolve and the iteration may throw — those reject via the capability.
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        // Iterate lazily so a throwing C.resolve / `.then` closes the iterator (IteratorClose).
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        let results = i.make_array(vec![]);
        let state = i.new_object();
        set_internal(&state, "__results", results.clone());
        // remainingElementsCount starts at 1; each element increments, the loop end decrements once.
        set_internal(&state, "__remaining", Value::Num(1.0));
        let mut idx = 0usize;
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    // A throw from the iterator marks it done — no IteratorClose.
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
            let rem = ab(i.to_number(&rem_v))?;
            set_internal(&state, "__remaining", Value::Num(rem + 1.0));
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let already = i.new_object();
            set_internal(&already, "__called", Value::Bool(false));
            let on_f = make_bound(
                i,
                promise_all_element,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already),
                    resolve_fn.clone(),
                ],
            );
            // Subscribe via the resolved value's `.then`; a throwing getter/call closes the iterator.
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            if let Err(e) = i.call(then, p, &[on_f, reject_fn.clone()]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
            idx += 1;
        }
        // The values array's length is the element count (CreateDataProperty set each index).
        ab(i.set_member(&results, "length", Value::Num(idx as f64)))?;
        let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
        let rem = ab(i.to_number(&rem_v))?;
        set_internal(&state, "__remaining", Value::Num(rem - 1.0));
        if rem - 1.0 == 0.0 {
            let _ = i.call(resolve_fn, Value::Undefined, &[results]);
        }
        Ok(result)
    });
    it.def_method(&ctor, "race", 1, |i, t, a| {
        // Race settles the result promise with the first element to settle, through C's capability.
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            if let Err(e) = i.call(then, p, &[resolve_fn.clone(), reject_fn.clone()]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
        }
        Ok(result)
    });
    it.def_method(&ctor, "allSettled", 1, |i, t, a| {
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        let results = i.make_array(vec![]);
        let state = i.new_object();
        set_internal(&state, "__results", results.clone());
        set_internal(&state, "__remaining", Value::Num(1.0));
        let mut idx = 0usize;
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
            let rem = ab(i.to_number(&rem_v))?;
            set_internal(&state, "__remaining", Value::Num(rem + 1.0));
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            // The fulfill and reject element functions for one index share one [[AlreadyCalled]].
            let already = i.new_object();
            set_internal(&already, "__called", Value::Bool(false));
            let on_f = make_bound(
                i,
                promise_settled_fulfill,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already.clone()),
                    resolve_fn.clone(),
                ],
            );
            let on_r = make_bound(
                i,
                promise_settled_reject,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already),
                    resolve_fn.clone(),
                ],
            );
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            if let Err(e) = i.call(then, p, &[on_f, on_r]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
            idx += 1;
        }
        ab(i.set_member(&results, "length", Value::Num(idx as f64)))?;
        let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
        let rem = ab(i.to_number(&rem_v))?;
        set_internal(&state, "__remaining", Value::Num(rem - 1.0));
        if rem - 1.0 == 0.0 {
            let _ = i.call(resolve_fn, Value::Undefined, &[results]);
        }
        Ok(result)
    });
    it.def_method(&ctor, "any", 1, |i, t, a| {
        let (result, resolve_fn, reject_fn) = match new_promise_capability_full(i, &t) {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        let promise_resolve = match get_promise_resolve(i, &t) {
            Ok(r) => r,
            Err(e) => {
                let _ = i.call(reject_fn, Value::Undefined, &[e]);
                return Ok(result);
            }
        };
        let (iter, next) = match i.get_iterator(&arg(a, 0)) {
            Ok(it) => it,
            Err(e) => {
                let reason = crate::interpreter::abrupt_value(e);
                let _ = i.call(reject_fn, Value::Undefined, &[reason]);
                return Ok(result);
            }
        };
        let errors = i.make_array(vec![]);
        let state = i.new_object();
        set_internal(&state, "__errors", errors.clone());
        set_internal(&state, "__remaining", Value::Num(1.0));
        let mut idx = 0usize;
        loop {
            let item = match i.iterator_step(&iter, &next) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
            let rem = ab(i.to_number(&rem_v))?;
            set_internal(&state, "__remaining", Value::Num(rem + 1.0));
            let p = match i.call(promise_resolve.clone(), t.clone(), &[item]) {
                Ok(p) => p,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            let already = i.new_object();
            set_internal(&already, "__called", Value::Bool(false));
            let on_r = make_bound(
                i,
                promise_any_reject,
                vec![
                    Value::Obj(state.clone()),
                    Value::Num(idx as f64),
                    Value::Obj(already),
                    reject_fn.clone(),
                ],
            );
            let then = match i.get_member(&p, "then") {
                Ok(t) => t,
                Err(e) => {
                    i.iterator_close(&iter);
                    let _ = i.call(
                        reject_fn.clone(),
                        Value::Undefined,
                        &[crate::interpreter::abrupt_value(e)],
                    );
                    return Ok(result);
                }
            };
            // First fulfillment resolves the result (its [[AlreadyResolved]] lives in resolve_fn).
            if let Err(e) = i.call(then, p, &[resolve_fn.clone(), on_r]) {
                i.iterator_close(&iter);
                let _ = i.call(
                    reject_fn.clone(),
                    Value::Undefined,
                    &[crate::interpreter::abrupt_value(e)],
                );
                return Ok(result);
            }
            idx += 1;
        }
        ab(i.set_member(&errors, "length", Value::Num(idx as f64)))?;
        let rem_v = ab(i.get_member(&Value::Obj(state.clone()), "__remaining"))?;
        let rem = ab(i.to_number(&rem_v))?;
        set_internal(&state, "__remaining", Value::Num(rem - 1.0));
        if rem - 1.0 == 0.0 {
            let agg = make_aggregate_error(i, errors)?;
            let _ = i.call(reject_fn, Value::Undefined, &[agg]);
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
            if !combinator_then(i, &on_r.clone(), p, on_f, on_r) {
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
            let overall_reject = i.make_resolver(&result, false);
            if !combinator_then(i, &overall_reject, p, on_f, on_r) {
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
        // A proxy is a constructor exactly when its target is one.
        obj.borrow_mut().is_constructor = is_constructor_value(&target);
    }
    let p = Rc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
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
        // A smuggled pair round-trips as its real character.
        if let Some(real) = chars.peek().and_then(|&n| crate::jstr::paired_char(c, n)) {
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

/// DeletePropertyOrThrow(O, P): [[Delete]] and throw a TypeError if it returns false. An absent
/// property deletes successfully (returns true), matching the spec used by Array mutators.
fn delete_or_throw(i: &mut Interp, holder: &Value, key: &str) -> Result<(), Value> {
    if let Some((target, handler)) = proxy_pair(i, holder) {
        let ok = ab(i.proxy_delete(target, handler, key))?;
        if !ok {
            return Err(i.make_error("TypeError", format!("Cannot delete property '{key}'")));
        }
        return Ok(());
    }
    if let Value::Obj(o) = holder {
        let present = o.borrow().props.contains(key);
        if !present {
            return Ok(());
        }
        let configurable = o
            .borrow()
            .props
            .get(key)
            .map(|p| p.configurable)
            .unwrap_or(true);
        if !configurable {
            return Err(i.make_error("TypeError", format!("Cannot delete property '{key}'")));
        }
        o.borrow_mut().props.remove(key);
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

/// CreateDataPropertyOrThrow: like `json_create_data_prop` but a false [[DefineOwnProperty]]
/// result (e.g. a non-extensible target) throws a TypeError instead of being ignored.
fn json_create_data_prop_or_throw(
    i: &mut Interp,
    holder: &Value,
    key: &str,
    v: Value,
) -> Result<(), Value> {
    let desc = i.new_object();
    set_data(&desc, "value", v);
    set_data(&desc, "writable", Value::Bool(true));
    set_data(&desc, "enumerable", Value::Bool(true));
    set_data(&desc, "configurable", Value::Bool(true));
    let ok = if let Some((target, handler)) = proxy_pair(i, holder) {
        ab(proxy_define_property(
            i,
            &target,
            &handler,
            key,
            &Value::Obj(desc),
        ))?
    } else if let Value::Obj(o) = holder {
        ab(define_own_property(i, o, key, &Value::Obj(desc)))?
    } else {
        false
    };
    if !ok {
        return Err(i.make_error("TypeError", "cannot create data property"));
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
            // A proxy's [[Get]](P, Receiver) threads the explicit receiver.
            return ab(i.get_member_recv(&current, key, receiver.clone()));
        }
        let obj = match &current {
            Value::Obj(o) => o.clone(),
            _ => return Ok(Value::Undefined),
        };
        // A module namespace's [[Get]] reads the export's live value (throwing for a TDZ binding).
        let ptr = Rc::as_ptr(&obj) as usize;
        if i.is_namespace(ptr) {
            if let Some(res) = i.namespace_own_property(ptr, key) {
                return Ok(ab(res)?.value);
            }
        }
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
    // Integer-indexed exotic [[Set]]: a canonical numeric index on a TypedArray is handled by
    // TypedArraySetElement (which coerces the value unconditionally, writing only when the index is
    // in range) and never falls back to creating an ordinary shadowing property.
    // A module namespace exotic object's [[Set]] always returns false.
    if let Value::Obj(o) = target {
        if i.is_namespace(Rc::as_ptr(o) as usize) {
            return Ok(false);
        }
    }
    let mut current = target.clone();
    loop {
        if proxy_pair(i, &current).is_some() {
            // A proxy's [[Set]] returns its own success boolean (via the trap or a forwarded [[Set]]).
            return ab(i.set_member_recv(&current, key, value, receiver.clone()));
        }
        // Integer-indexed exotic [[Set]] — applies to the target or any TypedArray reached along
        // the prototype chain. With the TypedArray itself as receiver, TypedArraySetElement
        // coerces unconditionally and writes only in-range; with a foreign receiver, a valid
        // index acts as a writable data property (created on the receiver) and a canonical
        // non-index is an inert success.
        if let Some(info) = map_ptr(&current).and_then(|p| i.typed_arrays.get(&p).copied()) {
            if i.canonical_numeric_index(key).is_some() {
                let same = matches!((&current, receiver), (Value::Obj(a), Value::Obj(b)) if Rc::ptr_eq(a, b));
                if same {
                    if info.kind.is_bigint() {
                        let n = ab(i.to_bigint(&value))?;
                        if let TaIndex::Element(idx) = i.ta_index_kind(&info, key) {
                            i.ta_write_bigint(&info, idx, n);
                        }
                    } else {
                        let n = ab(i.to_number(&value))?;
                        if let TaIndex::Element(idx) = i.ta_index_kind(&info, key) {
                            i.ta_write(&info, idx, n);
                        }
                    }
                    return Ok(true);
                }
                match i.ta_index_kind(&info, key) {
                    TaIndex::Exotic => return Ok(true),
                    TaIndex::Element(_) => {
                        return reflect_define_on_receiver(i, receiver, key, value)
                    }
                    TaIndex::Ordinary => {}
                }
            }
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
pub(crate) fn reflect_define_on_receiver(
    i: &mut Interp,
    receiver: &Value,
    key: &str,
    value: Value,
) -> Result<bool, Value> {
    if proxy_pair(i, receiver).is_some() {
        let (target, handler) = proxy_pair(i, receiver).unwrap();
        // OrdinarySetWithOwnDescriptor: the receiver's [[GetOwnProperty]] runs first (its trap is
        // observable); an existing accessor or non-writable property rejects, an existing data
        // property is redefined with just {value}, and an absent one gets CreateDataProperty.
        let existing = proxy_gopd_value(i, &target, &handler, key)?;
        let desc = i.new_object();
        if let Value::Obj(d) = &existing {
            let (is_accessor, writable) = {
                let db = d.borrow();
                (
                    db.props.contains("get") || db.props.contains("set"),
                    matches!(
                        db.props.get("writable").map(|p| &p.value),
                        Some(Value::Bool(true))
                    ),
                )
            };
            if is_accessor || !writable {
                return Ok(false);
            }
            set_data(&desc, "value", value);
        } else {
            set_data(&desc, "value", value);
            set_data(&desc, "writable", Value::Bool(true));
            set_data(&desc, "enumerable", Value::Bool(true));
            set_data(&desc, "configurable", Value::Bool(true));
        }
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
    // A TypedArray receiver's integer index is written through its exotic [[DefineOwnProperty]]
    // (CreateDataProperty semantics), not stored as an ordinary property.
    if let Some(info) = ta_info(i, &ro) {
        if i.canonical_numeric_index(key).is_some() {
            return match i.ta_index_kind(&info, key) {
                TaIndex::Element(idx) => {
                    ab(i.ta_store(&info, idx, &value))?;
                    Ok(true)
                }
                _ => Ok(false),
            };
        }
    }
    // An Array receiver's `length` (and index writes) go through the exotic [[Set]] semantics
    // (double coercion, RangeError, truncation) rather than a raw property write.
    if matches!(ro.borrow().exotic, Exotic::Array) {
        let saved = i.strict;
        i.strict = false;
        let r = i.set_member_recv(&Value::Obj(ro.clone()), key, value, Value::Obj(ro.clone()));
        i.strict = saved;
        return ab(r);
    }
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

fn global_fn(it: &Interp, name: &str, len: usize, f: NativeFn) {
    let func = it.make_native(name, len, f);
    set_builtin(&it.global, name, Value::Obj(func));
}

// ---------------------------------------------------------------------------------------------
// Function.prototype
// ---------------------------------------------------------------------------------------------

fn install_function_proto(it: &mut Interp) {
    let fp = it.function_proto.clone();
    // %Function.prototype% is itself a callable function object that accepts any arguments and
    // returns undefined, with length 0 and the empty name.
    {
        let mut b = fp.borrow_mut();
        b.call = crate::value::Callable::Native(|_i, _this, _args| Ok(Value::Undefined));
        b.props.insert(
            "length",
            Property::data(Value::Num(0.0), false, false, true),
        );
        b.props
            .insert("name", Property::data(Value::str(""), false, false, true));
    }
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
        // length: 0 unless the target has an OWN `length` that is a Number; then +Infinity stays,
        // and a finite value becomes max(0, ToInteger(len) - boundArgs). name = "bound " + target.name.
        let has_own_len = matches!(&this, Value::Obj(o) if o.borrow().props.contains("length"));
        let l: f64 = if has_own_len {
            match ab(i.get_member(&this, "length"))? {
                Value::Num(n) if n == f64::INFINITY => f64::INFINITY,
                Value::Num(n) if n.is_finite() => (n.trunc() - bound_args.len() as f64).max(0.0),
                _ => 0.0,
            }
        } else {
            0.0
        };
        let target_name = ab(i.get_member(&this, "name"))?;
        let name = match &target_name {
            Value::Str(s) => format!("bound {s}"),
            _ => "bound ".to_string(),
        };
        let obj = Object::new(Some(i.function_proto.clone()));
        let target_is_ctor = is_constructor_value(&this);
        obj.borrow_mut().call = Callable::Bound {
            target,
            this: bound_this,
            args: bound_args,
        };
        // A bound function is a constructor exactly when its target is.
        obj.borrow_mut().is_constructor = target_is_ctor;
        obj.borrow_mut()
            .props
            .insert("length", Property::data(Value::Num(l), false, false, true));
        obj.borrow_mut().props.insert(
            "name",
            Property::data(Value::from_string(name), false, false, true),
        );
        Ok(Value::Obj(obj))
    });
    it.def_method(&fp, "toString", 0, |i, this, _args| {
        // Function.prototype.toString requires a callable `this` (a function or a callable proxy).
        if !this.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Function.prototype.toString requires that 'this' be a function",
            ));
        }
        // A user function returns the source text it was parsed from; everything else
        // (natives, bound functions, proxies) renders as a native function.
        if let Value::Obj(o) = &this {
            if let Callable::User(f, _) = &o.borrow().call {
                if let Some(src) = &f.source {
                    return Ok(Value::from_string(src.to_string()));
                }
            }
        }
        Ok(Value::str("function () { [native code] }"))
    });

    // The %ThrowTypeError% poison pill: a single frozen function (length 0, name "") reused as the
    // `caller`/`arguments` accessors on Function.prototype and `callee` on strict arguments objects.
    let throw_type_error = it.make_native("", 0, |i, _t, _a| {
        Err(i.make_error(
            "TypeError",
            "'caller', 'callee', and 'arguments' may not be accessed on strict mode functions",
        ))
    });
    {
        let mut b = throw_type_error.borrow_mut();
        // length/name are non-configurable, and the function is frozen + non-extensible.
        b.props.insert(
            "length",
            Property::data(Value::Num(0.0), false, false, false),
        );
        b.props
            .insert("name", Property::data(Value::str(""), false, false, false));
        b.extensible = false;
    }
    it.extra_protos
        .insert("%ThrowTypeError%", throw_type_error.clone());
    // Function.prototype.caller / .arguments: accessor properties whose getter AND setter are
    // the single %ThrowTypeError% intrinsic (the spec requires the same function object).
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
        create_dynamic_function(i, args, "function")
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
    set_builtin(&it.global, "Function", Value::Obj(ctor.clone()));
}

/// The %GeneratorFunction% / %AsyncFunction% / %AsyncGeneratorFunction% intrinsics. Each is a
/// constructor (reachable only as `theFn.constructor`, not a global) whose [[Prototype]] is
/// %Function%, with a `.prototype` object stored in extra_protos so `make_function` installs it as
/// the [[Prototype]] of the corresponding kind of function. Runs after Symbol so @@toStringTag works.
fn install_generator_function_ctors(it: &mut Interp) {
    let fp = it.function_proto.clone();
    let function_ctor = match it
        .global
        .borrow()
        .props
        .get("Function")
        .map(|p| p.value.clone())
    {
        Some(Value::Obj(o)) => o,
        _ => return,
    };
    for (tag, make_fn) in [
        (
            "GeneratorFunction",
            (|i: &mut Interp, _t: Value, a: &[Value]| create_dynamic_function(i, a, "function*"))
                as NativeFn,
        ),
        (
            "AsyncFunction",
            (|i: &mut Interp, _t: Value, a: &[Value]| {
                create_dynamic_function(i, a, "async function")
            }) as NativeFn,
        ),
        (
            "AsyncGeneratorFunction",
            (|i: &mut Interp, _t: Value, a: &[Value]| {
                create_dynamic_function(i, a, "async function*")
            }) as NativeFn,
        ),
    ] {
        // The constructor's `.prototype` object; its [[Prototype]] is %Function.prototype%.
        let kind_proto = Object::new(Some(fp.clone()));
        set_to_string_tag(it, &kind_proto, tag);
        let key: &'static str = Box::leak(format!("%{tag}.prototype%").into_boxed_str());
        it.extra_protos.insert(key, kind_proto.clone());
        let kind_ctor = it.make_native(tag, 1, make_fn);
        kind_ctor.borrow_mut().proto = Some(function_ctor.clone()); // [[Prototype]] is %Function%
        kind_ctor.borrow_mut().props.insert(
            "prototype",
            Property::data(Value::Obj(kind_proto.clone()), false, false, false),
        );
        kind_proto.borrow_mut().props.insert(
            "constructor",
            Property::data(Value::Obj(kind_ctor), false, false, true),
        );
    }
}

/// Shared CreateDynamicFunction for the Function/Generator/Async/AsyncGenerator constructors:
/// synthesize `<prefix> anonymous(<params>) { <body> }`, parse it, and build the function object.
fn create_dynamic_function(i: &mut Interp, args: &[Value], prefix: &str) -> Result<Value, Value> {
    let (params, body) = if args.is_empty() {
        (String::new(), String::new())
    } else {
        // The parameters are stringified left to right BEFORE the body.
        let mut ps = Vec::new();
        for a in &args[..args.len() - 1] {
            ps.push(ab(i.to_string(a))?.to_string());
        }
        let body = ab(i.to_string(args.last().unwrap()))?.to_string();
        (ps.join(","), body)
    };
    let src = format!("{prefix} anonymous({params}\n) {{\n{body}\n}}");
    let program = crate::parser::parse_script(&src, false)
        .map_err(|e| i.make_error("SyntaxError", e.message))?;
    match program.into_iter().next() {
        Some(crate::ast::Stmt::FuncDecl(f)) => {
            let env = i.global_env.clone();
            let func = i.make_function(f, env);
            // GetPrototypeFromConstructor(new.target, ...): a cross-realm `new other.Function()`
            // gets the other realm's prototype as its [[Prototype]] — including the *fallback*
            // when newTarget.prototype isn't an object (resolved in newTarget's realm).
            let nt = i.new_target.clone();
            if matches!(nt, Value::Obj(_)) {
                let kind_key = match prefix {
                    "function*" => "%GeneratorFunction.prototype%",
                    "async function" => "%AsyncFunction.prototype%",
                    "async function*" => "%AsyncGeneratorFunction.prototype%",
                    _ => "Function",
                };
                match ab(i.get_member(&nt, "prototype"))? {
                    Value::Obj(p) => {
                        if let Value::Obj(fo) = &func {
                            fo.borrow_mut().proto = Some(p);
                        }
                    }
                    _ => {
                        if let Some(p) = ctor_realm_proto(i, &nt, kind_key) {
                            if let Value::Obj(fo) = &func {
                                fo.borrow_mut().proto = Some(p);
                            }
                        }
                    }
                }
            }
            Ok(func)
        }
        _ => Err(i.make_error("SyntaxError", "Function constructor: invalid body")),
    }
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
    it.def_method(&proto, "importValue", 2, shadow_import_value);

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
    let subptr = &mut **i.shadow_realms.get_mut(&ptr).unwrap() as *mut Interp;
    // SAFETY: pinned Box target; re-entrant evaluation is sequenced by the JS call stack.
    let sub: &mut Interp = unsafe { &mut *subptr };
    let result = {
        // PerformShadowRealmEval: like eval code — `var`s instantiate in the realm's global,
        // lexicals live in a fresh declarative environment per evaluation.
        sub.strict = matches!(
            body.first(),
            Some(crate::ast::Stmt::Expr(crate::ast::Expr::Str(d))) if &**d == "use strict"
        );
        let genv = sub.global_env.clone();
        let scope = crate::interpreter::new_scope(Some(genv.clone()));
        sub.hoist(&body, &genv, &[]);
        sub.declare_block_lexicals(&body, &scope, false);
        let r = sub.run_stmt_list(&body, &scope).map(|v| match v {
            Value::Empty => Value::Undefined,
            other => other,
        });
        sub.drain_microtasks();
        r
    };
    match result {
        // Primitives cross the realm boundary directly; callables wrap; other objects throw.
        Ok(v) => ab(i.make_shadow_result(ptr, v)),
        // An error thrown inside the shadow realm is re-thrown as a TypeError of the calling realm.
        Err(_) => Err(i.make_error(
            "TypeError",
            "ShadowRealm evaluate: the provided source threw an error",
        )),
    }
}

/// ShadowRealm.prototype.importValue: load `specifier` as a module inside the sub-realm and
/// marshal its `exportName` export out (primitive or wrapped callable).
fn shadow_import_value(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    // Brand check, specifier ToString, and exportName validation all throw synchronously.
    let ptr = map_ptr(&this)
        .filter(|p| i.shadow_realms.contains_key(p))
        .ok_or_else(|| {
            i.make_error(
                "TypeError",
                "ShadowRealm.prototype.importValue called on a non-ShadowRealm",
            )
        })?;
    let spec = ab(i.to_string(&arg(a, 0)))?.to_string();
    let name = match arg(a, 1) {
        Value::Str(s) => s.to_string(),
        _ => return Err(i.make_error("TypeError", "importValue exportName must be a string")),
    };
    let promise = i.new_promise();
    let outcome = (|i: &mut Interp| -> Result<Value, Value> {
        let loader = i
            .module_loader
            .clone()
            .ok_or_else(|| i.make_error("TypeError", "no module loader available"))?;
        let base = i.import_base.clone();
        let (key, src) = loader(&spec, &base)
            .ok_or_else(|| i.make_error("TypeError", format!("module not found: {spec}")))?;
        let subptr = &mut **i.shadow_realms.get_mut(&ptr).unwrap() as *mut Interp;
        // SAFETY: pinned Box target (see shadow_evaluate).
        let sub: &mut Interp = unsafe { &mut *subptr };
        sub.module_loader = Some(loader);
        sub.import_base = base;
        let loaded = {
            let r = sub.load_module(&key, &src);
            sub.drain_microtasks();
            r
        };
        let ns = sub.module_namespace(&key);
        let value = match (&loaded, ns) {
            (Ok(_), Some(ns)) => sub.get_member(&ns, &name),
            _ => {
                return Err(i.make_error("TypeError", "importValue: module evaluation failed"));
            }
        };
        match value {
            Ok(Value::Undefined) => Err(i.make_error(
                "TypeError",
                format!("importValue: no export named '{name}'"),
            )),
            Ok(v) => ab(i.make_shadow_result(ptr, v)),
            Err(_) => Err(i.make_error("TypeError", "importValue: export access failed")),
        }
    })(i);
    match outcome {
        Ok(v) => i.resolve_promise(&promise, v),
        Err(e) => i.reject_promise(&promise, e),
    }
    Ok(promise)
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
        // TypedArray [[PreventExtensions]] refuses unless IsTypedArrayFixedLength: length-tracking
        // views, and any view over a resizable (non-shared) buffer, can change length.
        if let Some(info) = ta_info(i, o) {
            let resizable = Rc::as_ptr(o) as usize;
            let resizable = i
                .ta_buffer
                .get(&resizable)
                .and_then(|b| b.as_obj())
                .is_some_and(|b| {
                    matches!(
                        b.borrow().props.get("__abResizable").map(|p| &p.value),
                        Some(Value::Bool(true))
                    )
                });
            let shared = i.shared_buffers.contains_key(&info.buffer);
            if info.track || (resizable && !shared) {
                return Ok(false);
            }
        }
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
            Value::Obj(t) => {
                // OrdinaryOwnPropertyKeys order: array-index keys ascending, then other string keys in
                // insertion order, then symbol keys (as their Symbol values) in insertion order.
                let keys = t.borrow().props.keys();
                let mut indices: Vec<u32> = Vec::new();
                let mut strings: Vec<Rc<str>> = Vec::new();
                let mut symbols: Vec<Rc<str>> = Vec::new();
                for k in keys {
                    if Interp::is_sym_key(&k) {
                        symbols.push(k);
                    } else if let Ok(n) = k.parse::<u32>() {
                        if n != u32::MAX && n.to_string() == *k {
                            indices.push(n);
                        } else {
                            strings.push(k);
                        }
                    } else {
                        strings.push(k);
                    }
                }
                indices.sort_unstable();
                let mut out: Vec<Value> = indices
                    .into_iter()
                    .map(|n| Value::from_string(n.to_string()))
                    .collect();
                out.extend(strings.into_iter().map(Value::Str));
                out.extend(
                    symbols
                        .into_iter()
                        .map(|k| i.sym_from_key(&k).unwrap_or(Value::Str(k))),
                );
                out
            }
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
/// Trap-aware HasOwnProperty (for CopyNameAndLength across realm boundaries).
pub(crate) fn has_own_property_trapped(
    i: &mut Interp,
    v: &Value,
    key: &str,
) -> Result<bool, Value> {
    if let Some((t, h)) = proxy_pair(i, v) {
        return Ok(!matches!(
            proxy_gopd_value(i, &t, &h, key)?,
            Value::Undefined
        ));
    }
    Ok(matches!(v, Value::Obj(o) if o.borrow().props.contains(key)))
}

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
    // A descriptor may be reported for a property the target lacks only on an extensible target.
    if let Value::Obj(t) = target {
        if !t.borrow().props.contains(key) && !t.borrow().extensible {
            return Err(i.make_error(
                "TypeError",
                "gOPD trap reported a property missing from a non-extensible target",
            ));
        }
    }
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
                // IsCompatiblePropertyDescriptor: a non-configurable target property can't be
                // redefined as configurable.
                if !p.configurable && matches!(pd.configurable, Some(true)) {
                    return Err(i.throw(
                        "TypeError",
                        "proxy 'defineProperty' made a non-configurable target property configurable",
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
                // Step 16.c: a non-configurable *writable* data target can't be reported non-writable.
                if !p.configurable
                    && !p.accessor
                    && p.writable
                    && matches!(pd.writable, Some(false))
                {
                    return Err(i.throw(
                        "TypeError",
                        "proxy 'defineProperty' reported a non-configurable writable property as non-writable",
                    ));
                }
            }
        }
    }
    Ok(true)
}

/// A proxy's own enumerable string keys (for Object.keys/values/entries): the ownKeys trap result
/// filtered by each key's [[GetOwnProperty]] enumerable flag.
pub(crate) fn proxy_enum_string_keys(i: &mut Interp, proxy: &Value) -> Result<Vec<Value>, Value> {
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
    } else if let Some((t2, h2)) = proxy_pair(i, target) {
        // Absent gOPD trap over a proxy target: forward to the target's [[GetOwnProperty]].
        proxy_key_enumerable(i, &t2, &h2, key)
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
        // A proxy's [[GetOwnProperty]] goes through its trap (recursing for a proxy target).
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            let desc = proxy_gopd_value(i, &target, &handler, &key)?;
            return Ok(Value::Bool(!matches!(desc, Value::Undefined)));
        }
        // A module namespace's [[GetOwnProperty]] reads live and throws for an uninitialized export.
        let ptr = Rc::as_ptr(&o) as usize;
        if i.is_namespace(ptr) {
            if let Some(res) = i.namespace_own_property(ptr, &key) {
                ab(res)?;
                return Ok(Value::Bool(true));
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
        if let Some(o) = this_obj(&this) {
            let ptr = Rc::as_ptr(&o) as usize;
            if i.is_namespace(ptr) {
                if let Some(res) = i.namespace_own_property(ptr, &key) {
                    ab(res)?;
                    return Ok(Value::Bool(true));
                }
            }
            // A proxy reads [[GetOwnProperty]]'s enumerable flag through its trap.
            if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
                let desc = proxy_gopd_value(i, &target, &handler, &key)?;
                let e = match desc {
                    Value::Undefined => false,
                    d => {
                        let ev = ab(i.get_member(&d, "enumerable"))?;
                        i.to_boolean(&ev)
                    }
                };
                return Ok(Value::Bool(e));
            }
        }
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
        // IsArray pierces proxies (a revoked proxy throws).
        let builtin = if json_is_array(i, &this)? {
            "Array"
        } else {
            builtin_tag(i, &this)
        };
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
    // Object.prototype.toLocaleString(): the default just invokes `this.toString()`.
    it.def_method(&op, "toLocaleString", 0, |i, this, _args| {
        let to_string = ab(i.get_member(&this, "toString"))?;
        if !to_string.is_callable() {
            return Err(i.make_error("TypeError", "toString is not callable"));
        }
        ab(i.call(to_string, this, &[]))
    });
    // Object.prototype.__proto__ — an accessor (Annex B) over [[GetPrototypeOf]]/[[SetPrototypeOf]].
    {
        let getter = it.make_native("get __proto__", 0, |i, this, _| {
            // RequireObjectCoercible, then ToObject before reading the prototype.
            let o = to_object_arg(i, this, "get __proto__")?;
            js_get_prototype_of(i, &Value::Obj(o))
        });
        let setter = it.make_native("set __proto__", 1, |i, this, args| {
            if matches!(this, Value::Undefined | Value::Null) {
                return Err(i.make_error("TypeError", "__proto__ set on null or undefined"));
            }
            let v = arg(args, 0);
            // Only an Object receiver with an Object/Null value actually changes the prototype;
            // everything else is a silent no-op.
            if matches!(&this, Value::Obj(_))
                && matches!(v, Value::Obj(_) | Value::Null)
                && !js_set_prototype_of(i, &this, &v)?
            {
                return Err(i.make_error("TypeError", "cyclic __proto__ value"));
            }
            Ok(Value::Undefined)
        });
        op.borrow_mut().props.insert(
            "__proto__",
            Property {
                value: Value::Undefined,
                get: Some(Value::Obj(getter)),
                set: Some(Value::Obj(setter)),
                accessor: true,
                writable: false,
                enumerable: false,
                configurable: true,
            },
        );
    }

    let ctor = it.make_native("Object", 1, |i, _this, args| {
        // `new Object()` with a newTarget other than %Object% itself (a subclass or a
        // Reflect.construct target): OrdinaryCreateFromConstructor(newTarget, %Object.prototype%).
        if i.constructing {
            if let Value::Obj(nt) = i.new_target.clone() {
                let is_self = matches!(
                    i.extra_protos.get("%ObjectCtor%"),
                    Some(c) if Rc::ptr_eq(c, &nt)
                );
                if !is_self {
                    let proto = match ab(i.get_member(&Value::Obj(nt.clone()), "prototype"))? {
                        Value::Obj(p) => Some(p),
                        _ => ctor_realm_proto(i, &Value::Obj(nt), "Object")
                            .or_else(|| Some(i.object_proto.clone())),
                    };
                    return Ok(Value::Obj(Object::new(proto)));
                }
            }
        }
        Ok(match arg(args, 0) {
            Value::Undefined | Value::Null => Value::Obj(i.new_object()),
            Value::Obj(o) => Value::Obj(o),
            // ToObject of a primitive yields its wrapper object.
            other => box_primitive(i, other),
        })
    });
    it.extra_protos.insert("%ObjectCtor%", ctor.clone());
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
        let names = ordered_enum_keys(&o);
        // A module namespace's Object.keys reads each binding's [[GetOwnProperty]], throwing for an
        // uninitialized export.
        let ptr = Rc::as_ptr(&o) as usize;
        if i.is_namespace(ptr) {
            for k in &names {
                if let Some(res) = i.namespace_own_property(ptr, k) {
                    ab(res)?;
                }
            }
        }
        let keys: Vec<Value> = names.into_iter().map(Value::Str).collect();
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
            Value::Undefined | Value::Empty | Value::Null => {
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
        // A mapped arguments index reports the live parameter value.
        if let Some(v) = i.mapped_arg_value(Rc::as_ptr(&o) as usize, &key) {
            if let Some(p) = o.borrow_mut().props.get_mut(&key) {
                p.value = v;
            }
        }
        // A TypedArray canonical numeric index is an own data property reading from the buffer; a
        // canonical-but-out-of-range index has no own property (a non-canonical key like "1.0" or
        // "+1" is an ordinary property, handled below).
        if let Some(info) = ta_info(i, &o) {
            if i.canonical_numeric_index(&key).is_some() {
                return Ok(match i.ta_index_kind(&info, &key) {
                    TaIndex::Element(idx) => {
                        let val = i.ta_read(&info, idx);
                        descriptor_from_prop(i, Property::data(val, true, true, true))
                    }
                    _ => Value::Undefined,
                });
            }
        }
        if let Some((target, handler)) = proxy_pair(i, &Value::Obj(o.clone())) {
            return proxy_gopd_value(i, &target, &handler, &key);
        }
        let ptr = Rc::as_ptr(&o) as usize;
        if i.is_namespace(ptr) {
            if let Some(res) = i.namespace_own_property(ptr, &key) {
                return Ok(descriptor_from_prop(i, ab(res)?));
            }
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
#[derive(Default, Clone)]
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
    // ArgumentsExoticObject [[DefineOwnProperty]]: sync the live parameter value into the
    // ordinary property first; after a successful ordinary define, a plain value write goes
    // through the map, and only an accessor or writable:false severs the alias.
    let args_ptr = Rc::as_ptr(o) as usize;
    let mapped = i.mapped_arg_name(args_ptr, key).is_some();
    if mapped {
        if let Some(cur) = i.mapped_arg_value(args_ptr, key) {
            if let Some(p) = o.borrow_mut().props.get_mut(key) {
                p.value = cur;
            }
        }
        let allowed = define_own_property_ordinary(i, o, key, &d)?;
        if !allowed {
            return Ok(false);
        }
        if d.is_accessor() {
            i.unmap_argument(args_ptr, key);
        } else {
            if let Some(v) = d.value.clone() {
                i.mapped_arg_write(args_ptr, key, v);
            }
            if d.writable == Some(false) {
                i.unmap_argument(args_ptr, key);
            }
        }
        return Ok(true);
    }
    define_own_property_ordinary(i, o, key, &d)
}

fn define_own_property_ordinary(
    i: &mut Interp,
    o: &Gc,
    key: &str,
    d: &PartialDesc,
) -> Result<bool, Abrupt> {
    let d = d.clone();
    // Module namespace [[DefineOwnProperty]]: a String key is only redefinable to a descriptor that
    // matches the export's fixed shape (writable, enumerable, non-configurable, same value); adding
    // a new String key fails. Symbol keys fall through to the ordinary algorithm.
    let ptr = Rc::as_ptr(o) as usize;
    if i.is_namespace(ptr) && !Interp::is_sym_key(key) {
        return match i.namespace_own_property(ptr, key) {
            Some(res) => {
                let cur = res?;
                let ok = d.configurable != Some(true)
                    && d.enumerable != Some(false)
                    && !d.is_accessor()
                    && d.writable != Some(false)
                    && match &d.value {
                        Some(v) => same_value(v, &cur.value),
                        None => true,
                    };
                Ok(ok)
            }
            None => Ok(false),
        };
    }
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
    let new_len = match &d.value {
        None => {
            // `length` is a non-configurable, non-enumerable data property.
            if d.is_accessor()
                || matches!(d.configurable, Some(true))
                || matches!(d.enumerable, Some(true))
            {
                return Ok(false);
            }
            let len_writable = o
                .borrow()
                .props
                .get("length")
                .map(|p| p.writable)
                .unwrap_or(true);
            // No value: only a writable change.
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
            // ArraySetLength coerces first — ToUint32(value) then ToNumber(value), both
            // observable — and a mismatch (fraction, negative, ≥ 2^32) is a RangeError before
            // any attribute validation.
            let n1 = i.to_number(v)?;
            let new_len: u32 = if n1.is_nan() || n1.is_infinite() || n1 == 0.0 {
                0
            } else {
                (n1.trunc() as i64 as u64 & 0xFFFF_FFFF) as u32
            };
            let number_len = i.to_number(v)?;
            let same = if number_len == 0.0 {
                new_len == 0
            } else {
                (new_len as f64) == number_len
            };
            if !same {
                return Err(i.throw("RangeError", "Invalid array length"));
            }
            new_len as usize
        }
    };
    // Now the ordinary-define validation against the current `length`.
    if d.is_accessor() || matches!(d.configurable, Some(true)) || matches!(d.enumerable, Some(true))
    {
        return Ok(false);
    }
    let len_writable = o
        .borrow()
        .props
        .get("length")
        .map(|p| p.writable)
        .unwrap_or(true);
    let old_len = i.array_length(o);
    if !len_writable && (new_len != old_len || matches!(d.writable, Some(true))) {
        return Ok(false);
    }
    let writable = d.writable.unwrap_or(len_writable);
    if new_len < old_len {
        // ArraySetLength deletes elements from the top down; a non-configurable element blocks the
        // shrink, so length only drops to just past it and the operation reports failure.
        let mut indices: Vec<usize> = o
            .borrow()
            .props
            .keys()
            .iter()
            .filter_map(|k| k.parse::<usize>().ok())
            .filter(|&idx| idx >= new_len)
            .collect();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        for idx in indices {
            let configurable = o
                .borrow()
                .props
                .get(&idx.to_string())
                .map(|p| p.configurable)
                .unwrap_or(true);
            if configurable {
                o.borrow_mut().props.remove(&idx.to_string());
            } else {
                // Stop here: length settles at idx+1; length stays writable unless explicitly frozen.
                o.borrow_mut().props.insert(
                    "length",
                    Property::data(Value::Num((idx + 1) as f64), writable, false, false),
                );
                return Ok(false);
            }
        }
    }
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
    // %Array.prototype% [@@unscopables]: a null-prototype object naming the `with`-transparent
    // methods; { writable: false, enumerable: false, configurable: true }.
    if let Some(key) = well_known_key(it, "unscopables") {
        let un = Object::new(None);
        for name in [
            "at",
            "copyWithin",
            "entries",
            "fill",
            "find",
            "findIndex",
            "findLast",
            "findLastIndex",
            "flat",
            "flatMap",
            "includes",
            "keys",
            "toReversed",
            "toSorted",
            "toSpliced",
            "values",
        ] {
            un.borrow_mut()
                .props
                .insert(name, Property::data(Value::Bool(true), true, true, true));
        }
        ap.borrow_mut()
            .props
            .insert(key, Property::data(Value::Obj(un), false, false, true));
    }

    it.def_method(&ap, "push", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        // `push` only writes at the tail, so a huge array-like length is fine (use ToLength, not the
        // engine's materialization cap).
        let mut len = ab(i.to_length(&o))? as u64;
        // The resulting length may not exceed 2^53-1.
        if len + args.len() as u64 > 9007199254740991 {
            return Err(i.make_error("TypeError", "push would exceed the maximum array length"));
        }
        for a in args {
            set_throw(i, &ov, &len.to_string(), a.clone())?;
            len += 1;
        }
        // Generic objects don't auto-track length the way arrays do, so set it explicitly.
        set_throw(i, &ov, "length", Value::Num(len as f64))?;
        Ok(Value::Num(len as f64))
    });
    it.def_method(&ap, "pop", 0, |i, this, _args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        // `pop` only touches the last index, so a huge array-like length is fine (use ToLength).
        let len = ab(i.to_length(&o))?;
        if len == 0 {
            set_throw(i, &ov, "length", Value::Num(0.0))?;
            return Ok(Value::Undefined);
        }
        let last = ab(i.get_member(&ov, &(len - 1).to_string()))?;
        delete_or_throw(i, &ov, &(len - 1).to_string())?;
        set_throw(i, &ov, "length", Value::Num((len - 1) as f64))?;
        Ok(last)
    });
    it.def_method(&ap, "shift", 0, |i, this, _args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.checked_array_len(&o))?;
        if len == 0 {
            set_throw(i, &ov, "length", Value::Num(0.0))?;
            return Ok(Value::Undefined);
        }
        let first = ab(i.get_member(&ov, "0"))?;
        for k in 1..len {
            let from = k.to_string();
            let to = (k - 1).to_string();
            if i.has_property(&o, &from) {
                let v = ab(i.get_member(&ov, &from))?;
                set_throw(i, &ov, &to, v)?;
            } else {
                delete_or_throw(i, &ov, &to)?;
            }
        }
        delete_or_throw(i, &ov, &(len - 1).to_string())?;
        set_throw(i, &ov, "length", Value::Num((len - 1) as f64))?;
        Ok(first)
    });
    it.def_method(&ap, "unshift", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.to_length(&o))? as u64;
        let n = args.len() as u64;
        if n > 0 {
            if len + n > 9007199254740991 {
                return Err(i.make_error("TypeError", "unshift result is too long"));
            }
            for k in (0..len).rev() {
                let from = k.to_string();
                let to = (k + n).to_string();
                if ab(i.js_has_property(&ov, &from))? {
                    let v = ab(i.get_member(&ov, &from))?;
                    set_throw(i, &ov, &to, v)?;
                } else {
                    delete_or_throw(i, &ov, &to)?;
                }
            }
            for (idx, a) in args.iter().enumerate() {
                set_throw(i, &ov, &idx.to_string(), a.clone())?;
            }
        }
        set_throw(i, &ov, "length", Value::Num((len + n) as f64))?;
        Ok(Value::Num((len + n) as f64))
    });
    it.def_method(&ap, "slice", 2, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        // The total length may be near 2^53 for an array-like; only the copied span (end-start) is
        // bounded by the engine's materialization cap.
        let len = ab(i.to_length(&o))? as i64;
        let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len);
        let end = match arg(args, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len),
        };
        let count = (end - start).max(0) as usize;
        let result = array_species_create(i, &this, count)?;
        let ov = Value::Obj(o.clone());
        let mut k = start;
        let mut to = 0usize;
        while k < end {
            let key = k.to_string();
            // Preserve holes: only copy indices the source actually has (HasProperty).
            if ab(i.js_has_property(&ov, &key))? {
                let v = ab(i.get_member(&this, &key))?;
                cdp_or_throw(i, &result, &to.to_string(), v)?;
            }
            k += 1;
            to += 1;
        }
        set_length_throw(i, &result, to as f64)?;
        Ok(result)
    });
    it.def_method(&ap, "indexOf", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        if len == 0 {
            // The length check precedes the fromIndex coercion.
            return Ok(Value::Num(-1.0));
        }
        let target = arg(args, 0);
        let from = match arg(args, 1) {
            Value::Undefined => 0usize,
            v => {
                let n = ab(i.to_number(&v))?;
                if n == f64::INFINITY {
                    return Ok(Value::Num(-1.0));
                }
                let n = if n.is_nan() { 0.0 } else { n.trunc() };
                if n >= 0.0 {
                    n as usize
                } else {
                    (len as f64 + n).max(0.0) as usize
                }
            }
        };
        let ov = Value::Obj(o.clone());
        for k in from..len {
            if !ab(i.js_has_property(&ov, &k.to_string()))? {
                continue; // indexOf skips holes
            }
            let v = ab(i.get_member(&ov, &k.to_string()))?;
            if i.strict_equals(&v, &target) {
                return Ok(Value::Num(k as f64));
            }
        }
        Ok(Value::Num(-1.0))
    });
    it.def_method(&ap, "includes", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))? as i64;
        if len == 0 {
            // The length check precedes the fromIndex coercion.
            return Ok(Value::Bool(false));
        }
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
        let ov = Value::Obj(o.clone());
        while k < len {
            let v = ab(i.get_member(&ov, &k.to_string()))?;
            if same_value_zero(&v, &target) {
                return Ok(Value::Bool(true));
            }
            k += 1;
        }
        Ok(Value::Bool(false))
    });
    it.def_method(&ap, "join", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.checked_array_len(&o))?;
        let sep = match arg(args, 0) {
            Value::Undefined => ",".to_string(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        let mut parts = Vec::with_capacity(len);
        for k in 0..len {
            let v = ab(i.get_member(&ov, &k.to_string()))?;
            parts.push(match v {
                Value::Undefined | Value::Null => String::new(),
                other => ab(i.to_string(&other))?.to_string(),
            });
        }
        let out = parts.join(&sep);
        Ok(Value::from_string(
            crate::jstr::canonicalize(&out).unwrap_or(out),
        ))
    });
    it.def_method(&ap, "concat", 1, |i, this, args| {
        // ToObject(this): a primitive receiver is boxed (and spread/appended as its wrapper).
        let recv = Value::Obj(arr_to_object(i, &this)?);
        let result = array_species_create(i, &recv, 0)?;
        let mut n = 0u64;
        let items: Vec<Value> = std::iter::once(recv).chain(args.iter().cloned()).collect();
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
                let len = ab(i.to_length(&match v {
                    Value::Obj(o) => o.clone(),
                    _ => unreachable!(),
                }))? as u64;
                if n + len > 9007199254740991 {
                    return Err(i.make_error("TypeError", "concat result is too long"));
                }
                for k in 0..len {
                    let key = k.to_string();
                    if ab(i.js_has_property(v, &key))? {
                        let elem = ab(i.get_member(v, &key))?;
                        cdp_or_throw(i, &result, &n.to_string(), elem)?;
                    }
                    n += 1; // increment for holes too, preserving their position
                }
            } else {
                if n >= 9007199254740991 {
                    return Err(i.make_error("TypeError", "concat result is too long"));
                }
                cdp_or_throw(i, &result, &n.to_string(), v.clone())?;
                n += 1;
            }
        }
        set_length_throw(i, &result, n as f64)?;
        Ok(result)
    });
    it.def_method(&ap, "forEach", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.forEach callback is not callable",
            ));
        }
        let cb_this = arg(args, 1);
        let ov = Value::Obj(o.clone());
        for k in 0..len {
            if !ab(i.js_has_property(&ov, &k.to_string()))? {
                continue; // skip array holes
            }
            let v = ab(i.get_member(&ov, &k.to_string()))?;
            ab(i.call(
                cb.clone(),
                cb_this.clone(),
                &[v, Value::Num(k as f64), ov.clone()],
            ))?;
        }
        Ok(Value::Undefined)
    });
    it.def_method(&ap, "map", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error("TypeError", "Array.prototype.map callback is not callable"));
        }
        let cb_this = arg(args, 1);
        let ov = Value::Obj(o.clone());
        let result = array_species_create(i, &this, len)?;
        for k in 0..len {
            if !ab(i.js_has_property(&ov, &k.to_string()))? {
                continue; // holes stay holes in the result
            }
            let v = ab(i.get_member(&ov, &k.to_string()))?;
            let mapped = ab(i.call(
                cb.clone(),
                cb_this.clone(),
                &[v, Value::Num(k as f64), ov.clone()],
            ))?;
            json_create_data_prop_or_throw(i, &result, &k.to_string(), mapped)?;
        }
        Ok(result)
    });
    it.def_method(&ap, "filter", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.filter callback is not callable",
            ));
        }
        let cb_this = arg(args, 1);
        let ov = Value::Obj(o.clone());
        let result = array_species_create(i, &this, 0)?;
        let mut to = 0usize;
        for k in 0..len {
            if !ab(i.js_has_property(&ov, &k.to_string()))? {
                continue;
            }
            let v = ab(i.get_member(&ov, &k.to_string()))?;
            let keep = ab(i.call(
                cb.clone(),
                cb_this.clone(),
                &[v.clone(), Value::Num(k as f64), ov.clone()],
            ))?;
            if i.to_boolean(&keep) {
                json_create_data_prop_or_throw(i, &result, &to.to_string(), v)?;
                to += 1;
            }
        }
        Ok(result)
    });
    it.def_method(&ap, "reduce", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.reduce callback is not callable",
            ));
        }
        let ov = Value::Obj(o.clone());
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
                if ab(i.js_has_property(&ov, &k.to_string()))? {
                    acc = ab(i.get_member(&ov, &k.to_string()))?;
                    k += 1;
                    break;
                }
                k += 1;
            }
        }
        while k < len {
            if ab(i.js_has_property(&ov, &k.to_string()))? {
                let v = ab(i.get_member(&ov, &k.to_string()))?;
                acc = ab(i.call(
                    cb.clone(),
                    Value::Undefined,
                    &[acc, v, Value::Num(k as f64), ov.clone()],
                ))?;
            }
            k += 1;
        }
        Ok(acc)
    });
    it.def_method(&ap, "reverse", 0, |i, this, _args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.to_length(&o))?;
        for k in 0..len / 2 {
            let lower = k.to_string();
            let upper = (len - 1 - k).to_string();
            // HasProperty/Get the two ends, then swap — preserving holes (a hole moves as a
            // DeletePropertyOrThrow).
            let lower_val = if ab(i.js_has_property(&ov, &lower))? {
                Some(ab(i.get_member(&ov, &lower))?)
            } else {
                None
            };
            let upper_val = if ab(i.js_has_property(&ov, &upper))? {
                Some(ab(i.get_member(&ov, &upper))?)
            } else {
                None
            };
            match (lower_val, upper_val) {
                (Some(lv), Some(uv)) => {
                    set_throw(i, &ov, &lower, uv)?;
                    set_throw(i, &ov, &upper, lv)?;
                }
                (None, Some(uv)) => {
                    set_throw(i, &ov, &lower, uv)?;
                    delete_or_throw(i, &ov, &upper)?;
                }
                (Some(lv), None) => {
                    delete_or_throw(i, &ov, &lower)?;
                    set_throw(i, &ov, &upper, lv)?;
                }
                (None, None) => {}
            }
        }
        Ok(ov)
    });
    it.def_method(&ap, "toString", 0, |i, this, _args| {
        let ov = Value::Obj(arr_to_object(i, &this)?);
        let join = ab(i.get_member(&ov, "join"))?;
        if join.is_callable() {
            ab(i.call(join, ov, &[]))
        } else {
            // Fall back to the %Object.prototype.toString% INTRINSIC semantics (the live
            // Object.prototype.toString may have been deleted or replaced). IsArray pierces
            // proxies (and throws on a revoked one).
            let builtin = if json_is_array(i, &ov)? {
                "Array"
            } else {
                builtin_tag(i, &ov)
            };
            let tag = match to_string_tag_key(i) {
                Some(key) => match ab(i.get_member(&ov, &key))? {
                    Value::Str(s) => s.to_string(),
                    _ => builtin.to_string(),
                },
                None => builtin.to_string(),
            };
            Ok(Value::str(format!("[object {tag}]")))
        }
    });
    it.def_method(&ap, "toLocaleString", 0, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.checked_array_len(&o))?;
        let mut out = String::new();
        for k in 0..len {
            if k > 0 {
                out.push(',');
            }
            let el = ab(i.get_member(&ov, &k.to_string()))?;
            if !matches!(el, Value::Undefined | Value::Null) {
                // ToString(? Invoke(element, "toLocaleString", « locales, options »)).
                let tls = ab(i.get_member(&el, "toLocaleString"))?;
                if !tls.is_callable() {
                    return Err(i.make_error("TypeError", "toLocaleString is not a function"));
                }
                let s = ab(i.call(tls, el, &[arg(args, 0), arg(args, 1)]))?;
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
        ab(i.get_member(&Value::Obj(o.clone()), &idx.to_string()))
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
        let len = ab(i.to_length(&o))? as i64;
        let v = arg(args, 0);
        let start = norm_index(ab(i.to_number(&arg(args, 1)))?, len);
        let end = match arg(args, 2) {
            Value::Undefined => len,
            x => norm_index(ab(i.to_number(&x))?, len),
        };
        // Only the filled span is bounded by the engine cap; the total length may be near 2^53.
        if (end - start).max(0) as usize > MAX_ARRAY_OP_LEN {
            return Err(i.make_error("RangeError", "array length exceeds engine limit"));
        }
        let ov = Value::Obj(o.clone());
        for k in start..end {
            set_throw(i, &ov, &k.to_string(), v.clone())?;
        }
        Ok(ov)
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
        let ov = Value::Obj(o.clone());
        while k >= 0 {
            if ab(i.js_has_property(&ov, &k.to_string()))? {
                let v = ab(i.get_member(&ov, &k.to_string()))?;
                if i.strict_equals(&v, &target) {
                    return Ok(Value::Num(k as f64));
                }
            }
            k -= 1;
        }
        Ok(Value::Num(-1.0))
    });
    it.def_method(&ap, "flat", 0, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let source_len = ab(i.to_length(&o))?;
        // ToIntegerOrInfinity(depth); undefined defaults to 1.
        let depth = match arg(args, 0) {
            Value::Undefined => 1i64,
            v => {
                let n = ab(i.to_number(&v))?;
                if n.is_nan() || n <= 0.0 {
                    0
                } else if n == f64::INFINITY {
                    i64::MAX
                } else {
                    n as i64
                }
            }
        };
        let a = array_species_create(i, &this, 0)?;
        let ov = Value::Obj(o.clone());
        flatten_into(i, &a, &ov, source_len, 0, depth, None, &Value::Undefined)?;
        Ok(a)
    });
    it.def_method(&ap, "flatMap", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.flatMap mapper is not callable",
            ));
        }
        let cb_this = arg(args, 1);
        let a = array_species_create(i, &this, 0)?;
        let ov = Value::Obj(o.clone());
        flatten_into(i, &a, &ov, len, 0, 1, Some(&cb), &cb_this)?;
        Ok(a)
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
        let ov = Value::Obj(o.clone());
        for (k, v) in items.into_iter().enumerate() {
            ab(i.set_member(&ov, &k.to_string(), v))?;
        }
        // Vacated trailing indices (originally holes, or beyond the present count) are deleted.
        for k in item_count..len {
            o.borrow_mut().props.remove(k.to_string().as_str());
        }
        Ok(ov)
    });
    // ----- change-array-by-copy (return a new Array, leave the receiver untouched) -----
    it.def_method(&ap, "toReversed", 0, |i, this, _| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.checked_array_len(&o))?;
        // Elements are read from the end down (from = len - k - 1) into ascending targets.
        let mut items = Vec::with_capacity(len);
        for k in 0..len {
            items.push(ab(i.get_member(&ov, &(len - k - 1).to_string()))?);
        }
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "toSorted", 1, |i, this, args| {
        // The comparator is validated before the receiver is read.
        let cmp = arg(args, 0);
        if !matches!(cmp, Value::Undefined) && !cmp.is_callable() {
            return Err(i.make_error("TypeError", "comparator is not callable"));
        }
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.checked_array_len(&o))?;
        let mut items = Vec::with_capacity(len);
        for k in 0..len {
            items.push(ab(i.get_member(&ov, &k.to_string()))?);
        }
        merge_sort(i, &mut items, &cmp)?;
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "with", 2, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.checked_array_len(&o))? as i64;
        // ToIntegerOrInfinity truncates first (so -0.5 → 0), then negatives count from the end.
        let rel = ab(i.to_number(&arg(args, 0)))?;
        let rel = if rel.is_nan() { 0.0 } else { rel.trunc() };
        let idx = if rel < 0.0 {
            len + rel as i64
        } else {
            rel as i64
        };
        if idx < 0 || idx >= len {
            return Err(i.make_error("RangeError", "invalid index"));
        }
        // The replaced index is never read.
        let mut items = Vec::with_capacity(len as usize);
        for k in 0..len {
            if k == idx {
                items.push(arg(args, 1));
            } else {
                items.push(ab(i.get_member(&ov, &k.to_string()))?);
            }
        }
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "toSpliced", 2, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.to_length(&o))? as u64;
        let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len as i64) as u64;
        // No start → skip 0; start but no deleteCount → delete through the end.
        let del = if args.is_empty() {
            0
        } else if args.len() < 2 {
            len - start
        } else {
            let d = ab(i.to_number(&arg(args, 1)))?;
            let d = if d.is_nan() { 0.0 } else { d.trunc().max(0.0) };
            (d.min((len - start) as f64)) as u64
        };
        let inserts: Vec<Value> = args.iter().skip(2).cloned().collect();
        let new_len = len - del + inserts.len() as u64;
        if new_len > 9007199254740991 {
            return Err(i.make_error("TypeError", "toSpliced result is too long"));
        }
        if new_len as usize > MAX_ARRAY_OP_LEN {
            return Err(i.make_error("RangeError", "array length exceeds engine limit"));
        }
        // The discarded span is never read.
        let mut items = Vec::with_capacity(new_len as usize);
        for k in 0..start {
            items.push(ab(i.get_member(&ov, &k.to_string()))?);
        }
        items.extend(inserts);
        for k in (start + del)..len {
            items.push(ab(i.get_member(&ov, &k.to_string()))?);
        }
        Ok(i.make_array(items))
    });
    it.def_method(&ap, "reduceRight", 1, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let len = ab(i.to_length(&o))?;
        let cb = arg(args, 0);
        if !cb.is_callable() {
            return Err(i.make_error(
                "TypeError",
                "Array.prototype.reduceRight callback is not callable",
            ));
        }
        let ov = Value::Obj(o.clone());
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
                if ab(i.js_has_property(&ov, &k.to_string()))? {
                    acc = ab(i.get_member(&ov, &k.to_string()))?;
                    k -= 1;
                    break;
                }
                k -= 1;
            }
        }
        while k >= 0 {
            if ab(i.js_has_property(&ov, &k.to_string()))? {
                let v = ab(i.get_member(&ov, &k.to_string()))?;
                acc = ab(i.call(
                    cb.clone(),
                    Value::Undefined,
                    &[acc, v, Value::Num(k as f64), ov.clone()],
                ))?;
            }
            k -= 1;
        }
        Ok(acc)
    });
    it.def_method(&ap, "copyWithin", 2, |i, this, args| {
        let o = arr_to_object(i, &this)?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.to_length(&o))? as i64;
        let target = norm_index(ab(i.to_number(&arg(args, 0)))?, len);
        let start = norm_index(ab(i.to_number(&arg(args, 1)))?, len);
        let end = match arg(args, 2) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len),
        };
        // Copy element by element (backwards when the ranges overlap that way), with live
        // HasProperty/Get/Set and DeletePropertyOrThrow for source holes.
        let mut count = (end - start).min(len - target).max(0);
        let (mut from, mut to, dir) = if start < target && target < start + count {
            (start + count - 1, target + count - 1, -1i64)
        } else {
            (start, target, 1i64)
        };
        while count > 0 {
            let fk = from.to_string();
            let tk = to.to_string();
            if ab(i.js_has_property(&ov, &fk))? {
                let v = ab(i.get_member(&ov, &fk))?;
                set_throw(i, &ov, &tk, v)?;
            } else {
                delete_or_throw(i, &ov, &tk)?;
            }
            from += dir;
            to += dir;
            count -= 1;
        }
        Ok(ov)
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
        let a = if args.len() == 1 && matches!(args[0], Value::Num(_)) {
            // `new Array(len)` sets length without materializing elements; the length setter
            // validates that it is a valid uint32 (else RangeError: Invalid array length).
            let a = i.make_array(Vec::new());
            ab(i.set_member(&a, "length", args[0].clone()))?;
            a
        } else {
            i.make_array(args.to_vec())
        };
        // GetPrototypeFromConstructor: a subclass / cross-realm new.target redirects the
        // instance prototype (falling back to new.target's realm's %Array.prototype%).
        if i.constructing {
            let nt = i.new_target.clone();
            if let Value::Obj(_) = &nt {
                let proto = match ab(i.get_member(&nt, "prototype"))? {
                    Value::Obj(p) => Some(p),
                    _ => ctor_realm_proto(i, &nt, "Array"),
                };
                if let (Value::Obj(o), Some(p)) = (&a, proto) {
                    if !Rc::ptr_eq(&p, &i.array_proto) {
                        o.borrow_mut().proto = Some(p);
                    }
                }
            }
        }
        Ok(a)
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
                    Some((_, Value::Null)) => {
                        return Err(i.make_error("TypeError", "proxy is revoked"))
                    }
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
        // GetMethod(items, @@iterator): a throwing getter propagates; non-callable non-nullish
        // is a TypeError.
        let iter_method = match well_known_key(i, "iterator") {
            Some(k) if !matches!(source, Value::Undefined | Value::Null) => {
                ab(i.get_member(&source, &k))?
            }
            _ => Value::Undefined,
        };
        if !matches!(iter_method, Value::Undefined | Value::Null) && !iter_method.is_callable() {
            return Err(i.make_error("TypeError", "@@iterator is not callable"));
        }
        let array_ctor = i.global.borrow().props.get("Array").map(|p| p.value.clone());
        let is_array_ctor = matches!((&this, &array_ctor), (Value::Obj(a), Some(Value::Obj(b))) if Rc::ptr_eq(a, b));
        let use_ctor = this.is_callable() && !is_array_ctor;
        if iter_method.is_callable() {
            // Iterator path: the target is constructed BEFORE iteration (constructor errors
            // propagate as-is), then elements are defined one at a time; a map/define failure
            // closes the iterator.
            let arr = if use_ctor {
                ab(i.construct(this, &[]))?
            } else {
                i.make_array(Vec::new())
            };
            let iter = ab(i.call(iter_method, source.clone(), &[]))?;
            let next = ab(i.get_member(&iter, "next"))?;
            let mut k = 0u64;
            loop {
                let res = ab(i.call(next.clone(), iter.clone(), &[]))?;
                if !matches!(res, Value::Obj(_)) {
                    return Err(i.make_error("TypeError", "iterator result is not an object"));
                }
                let done = ab(i.get_member(&res, "done"))?;
                if i.to_boolean(&done) {
                    break;
                }
                let raw = ab(i.get_member(&res, "value"))?;
                let step = (|i: &mut Interp| -> Result<(), Value> {
                    let v = if mapfn.is_callable() {
                        ab(i.call(
                            mapfn.clone(),
                            this_arg.clone(),
                            &[raw, Value::Num(k as f64)],
                        ))?
                    } else {
                        raw
                    };
                    cdp_or_throw(i, &arr, &k.to_string(), v)
                })(i);
                if let Err(e) = step {
                    i.iterator_close(&iter);
                    return Err(e);
                }
                k += 1;
            }
            set_length_throw(i, &arr, k as f64)?;
            return Ok(arr);
        }
        // Array-like path: ToObject, read length, construct with «len», then per-index Get
        // (lazily, so mutations during mapping are observed).
        let o = to_object_arg(i, source.clone(), "Array.from")?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.to_length(&o))?;
        let arr = if use_ctor {
            ab(i.construct(this, &[Value::Num(len as f64)]))?
        } else {
            if len > 4294967295 {
                return Err(i.make_error("RangeError", "invalid array length"));
            }
            i.make_array(Vec::new())
        };
        for k in 0..len {
            let raw = ab(i.get_member(&ov, &k.to_string()))?;
            let v = if mapfn.is_callable() {
                ab(i.call(
                    mapfn.clone(),
                    this_arg.clone(),
                    &[raw, Value::Num(k as f64)],
                ))?
            } else {
                raw
            };
            cdp_or_throw(i, &arr, &k.to_string(), v)?;
        }
        set_length_throw(i, &arr, len as f64)?;
        Ok(arr)
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
    let ov = Value::Obj(o.clone());
    let len = ab(i.to_length(&o))?;
    let cb = arg(args, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "predicate is not callable"));
    }
    let cb_this = arg(args, 1);
    for step in 0..len {
        let k = if from_last { len - 1 - step } else { step };
        let v = ab(i.get_member(&ov, &k.to_string()))?;
        let r = ab(i.call(
            cb.clone(),
            cb_this.clone(),
            &[v.clone(), Value::Num(k as f64), ov.clone()],
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
    let ov = Value::Obj(o.clone());
    for k in 0..len {
        if !i.has_property(&o, &k.to_string()) {
            continue; // skip holes
        }
        let v = ab(i.get_member(&ov, &k.to_string()))?;
        let r = ab(i.call(
            cb.clone(),
            cb_this.clone(),
            &[v, Value::Num(k as f64), ov.clone()],
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

/// FlattenIntoArray: write `source`'s (mapped, one-level-per-depth flattened) elements onto
/// `target` starting at `start`, via CreateDataPropertyOrThrow. Returns the next target index.
#[allow(clippy::too_many_arguments)]
fn flatten_into(
    i: &mut Interp,
    target: &Value,
    source: &Value,
    source_len: usize,
    start: usize,
    depth: i64,
    mapper: Option<&Value>,
    mapper_this: &Value,
) -> Result<usize, Value> {
    let mut target_index = start;
    for k in 0..source_len {
        if !ab(i.js_has_property(source, &k.to_string()))? {
            continue; // FlattenIntoArray skips holes
        }
        let mut element = ab(i.get_member(source, &k.to_string()))?;
        if let Some(m) = mapper {
            element = ab(i.call(
                m.clone(),
                mapper_this.clone(),
                &[element, Value::Num(k as f64), source.clone()],
            ))?;
        }
        if depth > 0 && json_is_array(i, &element)? {
            let len_val = ab(i.get_member(&element, "length"))?;
            let el_len = to_length_val(i, &len_val)?;
            target_index = flatten_into(
                i,
                target,
                &element,
                el_len,
                target_index,
                depth - 1,
                None,
                &Value::Undefined,
            )?;
        } else {
            if target_index >= 9_007_199_254_740_991 {
                return Err(i.make_error("TypeError", "flattened array length exceeds 2^53 - 1"));
            }
            json_create_data_prop_or_throw(i, target, &target_index.to_string(), element)?;
            target_index += 1;
        }
    }
    Ok(target_index)
}

fn array_splice(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let ov = Value::Obj(o.clone());
    // The total length may be near 2^53 for an array-like; only the elements actually moved are
    // bounded by the engine's materialization cap.
    let len = ab(i.to_length(&o))? as i64;
    let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len);
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
    // A result length past 2^53-1 is a TypeError (before any mutation or species construction).
    if (len - delete_count + items.len() as i64) as u64 > 9007199254740991 {
        return Err(i.make_error("TypeError", "splice result is too long"));
    }
    // The removed array (ArraySpeciesCreate) preserves holes via HasProperty.
    let removed = array_species_create(i, &this, delete_count.max(0) as usize)?;
    for k in 0..delete_count {
        let from = (start + k).to_string();
        if ab(i.js_has_property(&ov, &from))? {
            let v = ab(i.get_member(&ov, &from))?;
            json_create_data_prop_or_throw(i, &removed, &k.to_string(), v)?;
        }
    }
    ab(i.set_member(&removed, "length", Value::Num(delete_count as f64)))?;
    let item_count = items.len() as i64;
    // Shift the trailing elements (preserving holes) to open or close the gap.
    if item_count < delete_count {
        for k in start..(len - delete_count) {
            let from = (k + delete_count).to_string();
            let to = (k + item_count).to_string();
            if ab(i.js_has_property(&ov, &from))? {
                let v = ab(i.get_member(&ov, &from))?;
                set_throw(i, &ov, &to, v)?;
            } else {
                delete_or_throw(i, &ov, &to)?;
            }
        }
        for k in ((len - delete_count + item_count)..len).rev() {
            delete_or_throw(i, &ov, &k.to_string())?;
        }
    } else if item_count > delete_count {
        for k in ((start + 1)..=(len - delete_count)).rev() {
            let from = (k + delete_count - 1).to_string();
            let to = (k + item_count - 1).to_string();
            if ab(i.js_has_property(&ov, &from))? {
                let v = ab(i.get_member(&ov, &from))?;
                set_throw(i, &ov, &to, v)?;
            } else {
                delete_or_throw(i, &ov, &to)?;
            }
        }
    }
    for (off, v) in items.iter().enumerate() {
        set_throw(i, &ov, &(start + off as i64).to_string(), v.clone())?;
    }
    set_throw(
        i,
        &ov,
        "length",
        Value::Num((len - delete_count + item_count) as f64),
    )?;
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
        require_iterator_object(i, &this)?;
        let mut out = Vec::new();
        while let Some(v) = step_iter(i, &this)? {
            out.push(v);
        }
        Ok(i.make_array(out))
    });
    it.def_method(&proto, "forEach", 1, |i, this, a| {
        require_iterator_object(i, &this)?;
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
        require_iterator_object(i, &this)?;
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
        require_iterator_object(i, &this)?;
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
    // Iterator.prototype.constructor and [@@toStringTag] are accessor pairs: the getter yields
    // the value; the setter throws for the prototype itself as receiver but defines an own data
    // property on any other receiver (SetterThatIgnoresPrototypeProperties).
    {
        it.extra_protos
            .insert("%IteratorProtoMarker%", proto.clone());
        let getter_ctor = it.make_native("get constructor", 0, |i, _t, _a| {
            Ok(i.global
                .borrow()
                .props
                .get("Iterator")
                .map(|p| p.value.clone())
                .unwrap_or(Value::Undefined))
        });
        let getter_tag = it.make_native("get [Symbol.toStringTag]", 0, |_i, _t, _a| {
            Ok(Value::str("Iterator"))
        });
        let set_ctor = it.make_native("set constructor", 1, |i, this, a| {
            iterator_proto_weird_set(i, this, arg(a, 0), "constructor")
        });
        let set_tag = it.make_native("set [Symbol.toStringTag]", 1, |i, this, a| {
            let key = to_string_tag_key(i).unwrap_or_default();
            iterator_proto_weird_set(i, this, arg(a, 0), &key)
        });
        let acc = |get: Gc, set: Gc| Property {
            value: Value::Undefined,
            get: Some(Value::Obj(get)),
            set: Some(Value::Obj(set)),
            accessor: true,
            writable: false,
            enumerable: false,
            configurable: true,
        };
        proto
            .borrow_mut()
            .props
            .insert("constructor", acc(getter_ctor, set_ctor));
        if let Some(tag) = to_string_tag_key(it) {
            proto
                .borrow_mut()
                .props
                .insert(tag, acc(getter_tag, set_tag));
        }
    }
    set_builtin(&it.global, "Iterator", Value::Obj(ctor));

    // %ArrayIteratorPrototype%: the intermediate prototype of Array iterators (its own [[Prototype]]
    // is %IteratorPrototype%), so getPrototypeOf(getPrototypeOf(arrIter)) lands on %IteratorPrototype%.
    let arr_iter_proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
    set_to_string_tag(it, &arr_iter_proto, "Array Iterator");
    it.def_method(&arr_iter_proto, "next", 0, array_iter_next);
    it.extra_protos
        .insert("%ArrayIteratorPrototype%", arr_iter_proto);

    // %StringIteratorPrototype%: the prototype of `String.prototype[@@iterator]()` iterators, which
    // walk the string lazily by code point.
    let str_iter_proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
    set_to_string_tag(it, &str_iter_proto, "String Iterator");
    it.def_method(&str_iter_proto, "next", 0, string_iter_next);
    it.extra_protos
        .insert("%StringIteratorPrototype%", str_iter_proto);

    // %RegExpStringIteratorPrototype%: the prototype of the iterator returned by
    // `RegExp.prototype[@@matchAll]` and `String.prototype.matchAll`.
    let rsi_proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
    set_to_string_tag(it, &rsi_proto, "RegExp String Iterator");
    it.def_method(&rsi_proto, "next", 0, regexp_string_iterator_next);
    it.extra_protos
        .insert("%RegExpStringIteratorPrototype%", rsi_proto);

    // %AsyncIteratorPrototype%: [@@asyncIterator]() returns this, plus [@@asyncDispose] (which calls
    // the iterator's return()). The prototype of every built-in async iterator.
    let async_iter_proto = Object::new(Some(it.object_proto.clone()));
    if let Some(k) = well_known_key(it, "asyncIterator") {
        let f = it.make_native("[Symbol.asyncIterator]", 0, return_this);
        async_iter_proto
            .borrow_mut()
            .props
            .insert(k, Property::builtin(Value::Obj(f)));
    }
    if let Some(k) = well_known_key(it, "asyncDispose") {
        let f = it.make_native("[Symbol.asyncDispose]", 0, async_dispose_via_return);
        async_iter_proto
            .borrow_mut()
            .props
            .insert(k, Property::builtin(Value::Obj(f)));
    }
    it.extra_protos
        .insert("%AsyncIteratorPrototype%", async_iter_proto.clone());

    // %GeneratorPrototype% (proto %IteratorPrototype%) and %AsyncGeneratorPrototype% (proto
    // %AsyncIteratorPrototype%): the [[Prototype]] of a generator function's `.prototype`, carrying
    // next/return/throw (which brand-check the receiver via the generators table).
    let gen_proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
    set_to_string_tag(it, &gen_proto, "Generator");
    it.def_method(&gen_proto, "next", 1, generator_next);
    it.def_method(&gen_proto, "return", 1, generator_return);
    it.def_method(&gen_proto, "throw", 1, generator_throw);
    // %Generator% (= %GeneratorFunction.prototype%) .prototype === %GeneratorPrototype%, and the
    // latter's .constructor points back — both { writable: false, enumerable: false, configurable: true }.
    link_generator_proto(it, "%GeneratorFunction.prototype%", &gen_proto);
    it.extra_protos.insert("%GeneratorPrototype%", gen_proto);
    let async_gen_proto = Object::new(Some(async_iter_proto));
    set_to_string_tag(it, &async_gen_proto, "AsyncGenerator");
    it.def_method(&async_gen_proto, "next", 1, async_generator_next);
    it.def_method(&async_gen_proto, "return", 1, async_generator_return);
    it.def_method(&async_gen_proto, "throw", 1, async_generator_throw);
    link_generator_proto(it, "%AsyncGeneratorFunction.prototype%", &async_gen_proto);
    it.extra_protos
        .insert("%AsyncGeneratorPrototype%", async_gen_proto);
}

/// Wire the `.prototype` ↔ `.constructor` pair between a `%(Async)GeneratorFunction.prototype%`
/// intrinsic (`%Generator%`/`%AsyncGenerator%`) and its instance prototype.
fn link_generator_proto(it: &mut Interp, gen_ctor_key: &str, inst_proto: &Gc) {
    let Some(gen_ctor) = it.extra_protos.get(gen_ctor_key).cloned() else {
        return;
    };
    gen_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(inst_proto.clone()), false, false, true),
    );
    inst_proto.borrow_mut().props.insert(
        "constructor",
        Property::data(Value::Obj(gen_ctor), false, false, true),
    );
}

/// %AsyncIteratorPrototype%[@@asyncDispose]: return a promise that runs the iterator's `return()`
/// (if any) and resolves to undefined.
fn async_dispose_via_return(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    // %AsyncIteratorPrototype%[@@asyncDispose]: every failure — a throwing `return` getter, a
    // throwing call, or a rejected result — surfaces as a rejection of the returned promise, and a
    // fulfilled result is discarded (the promise fulfills with undefined).
    let promise = i.new_promise();
    let ret = match i.get_member(&this, "return") {
        Ok(v) => v,
        Err(e) => {
            let reason = crate::interpreter::abrupt_value(e);
            i.reject_promise(&promise, reason);
            return Ok(promise);
        }
    };
    if !ret.is_callable() {
        i.resolve_promise(&promise, Value::Undefined);
        return Ok(promise);
    }
    match i.call(ret, this, &[Value::Undefined]) {
        Ok(v) => {
            let inner = promise_resolve_value(i, v);
            let on_f = make_bound_len(i, dispose_settle_fulfil, vec![promise.clone()], 1.0);
            let on_r = make_bound_len(i, dispose_settle_reject, vec![promise.clone()], 1.0);
            i.promise_then(&inner, on_f, on_r);
        }
        Err(e) => {
            let reason = crate::interpreter::abrupt_value(e);
            i.reject_promise(&promise, reason);
        }
    }
    Ok(promise)
}

/// Reaction for [@@asyncDispose]: the awaited `return()` result fulfilled — fulfil the dispose
/// promise (`args[0]`) with undefined, dropping the value.
fn dispose_settle_fulfil(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    i.resolve_promise(&arg(args, 0), Value::Undefined);
    Ok(Value::Undefined)
}

/// Reaction for [@@asyncDispose]: the awaited `return()` result rejected — reject the dispose
/// promise (`args[0]`) with the same reason (`args[1]`).
fn dispose_settle_reject(i: &mut Interp, _t: Value, args: &[Value]) -> Result<Value, Value> {
    i.reject_promise(&arg(args, 0), arg(args, 1));
    Ok(Value::Undefined)
}

/// CreateRegExpStringIterator: a lazy iterator over a regex's matches in a string. Its state lives in
/// internal properties; `next` drives it via RegExpExec.
fn create_regexp_string_iterator(
    i: &Interp,
    matcher: Value,
    s: Rc<str>,
    global: bool,
    unicode: bool,
) -> Value {
    let obj = Object::new(
        i.extra_protos
            .get("%RegExpStringIteratorPrototype%")
            .cloned(),
    );
    set_internal(&obj, "__rsi_regexp", matcher);
    set_internal(&obj, "__rsi_string", Value::Str(s));
    set_internal(&obj, "__rsi_global", Value::Bool(global));
    set_internal(&obj, "__rsi_unicode", Value::Bool(unicode));
    set_internal(&obj, "__rsi_done", Value::Bool(false));
    Value::Obj(obj)
}

fn regexp_string_iterator_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = match &this {
        Value::Obj(o) if o.borrow().props.contains("__rsi_regexp") => o.clone(),
        _ => return Err(i.make_error("TypeError", "next called on a non-RegExp-String-Iterator")),
    };
    let done = o
        .borrow()
        .props
        .get("__rsi_done")
        .map(|p| matches!(p.value, Value::Bool(true)))
        .unwrap_or(true);
    if done {
        return Ok(i.iter_result_obj(Value::Undefined, true));
    }
    let r = o.borrow().props.get("__rsi_regexp").unwrap().value.clone();
    let s = match o
        .borrow()
        .props
        .get("__rsi_string")
        .map(|p| p.value.clone())
    {
        Some(Value::Str(s)) => s,
        _ => Rc::from(""),
    };
    let global = matches!(
        o.borrow()
            .props
            .get("__rsi_global")
            .map(|p| p.value.clone()),
        Some(Value::Bool(true))
    );
    let unicode = matches!(
        o.borrow()
            .props
            .get("__rsi_unicode")
            .map(|p| p.value.clone()),
        Some(Value::Bool(true))
    );
    let m = regexp_exec_abstract(i, &r, s.clone())?;
    if matches!(m, Value::Null) {
        set_internal(&o, "__rsi_done", Value::Bool(true));
        return Ok(i.iter_result_obj(Value::Undefined, true));
    }
    if global {
        let m0_v = ab(i.get_member(&m, "0"))?;
        let m0 = ab(i.to_string(&m0_v))?;
        if m0.is_empty() {
            let li_v = ab(i.get_member(&r, "lastIndex"))?;
            let li = to_length_val(i, &li_v)?;
            let next = advance_string_index(li, &s, unicode);
            ab(i.set_member(&r, "lastIndex", Value::Num(next as f64)))?;
        }
    } else {
        set_internal(&o, "__rsi_done", Value::Bool(true));
    }
    Ok(i.iter_result_obj(m, false))
}

/// `Array.fromAsync(source, mapFn?, thisArg?)`: build an array from a sync/async iterable or an
/// array-like, awaiting each element, and return a promise of the result. lumen drains microtasks
/// synchronously, so the whole thing runs eagerly and settles the returned promise.
fn array_from_async(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    // Array.fromAsync runs as a genuine async coroutine: it executes synchronously up to the
    // first await, then parks, so concurrent mutations of the input interleave per spec.
    let promise = i.new_promise();
    let source = arg(a, 0);
    let mapfn = arg(a, 1);
    let this_arg = arg(a, 2);
    let body: Box<dyn FnOnce(&mut Interp) -> crate::coroutine::Suspend> = Box::new(
        move |i: &mut Interp| match from_async_body(i, this, source, mapfn, this_arg) {
            Ok(v) => crate::coroutine::Suspend::Done(v),
            Err(e) => crate::coroutine::Suspend::Throw(e),
        },
    );
    let ptr = i as *mut Interp;
    let coro = crate::coroutine::spawn_coroutine(ptr, crate::coroutine::SendBody(body));
    if let Value::Obj(o) = &promise {
        let key = Rc::as_ptr(o) as usize;
        i.generators.insert(key, coro);
        i.drive_async(
            key,
            promise.clone(),
            crate::coroutine::Resume::Next(Value::Undefined),
        );
    }
    Ok(promise)
}

/// Await inside the fromAsync coroutine: parks until the value settles.
fn fa_await(i: &mut Interp, v: Value) -> Result<Value, Value> {
    match crate::coroutine::coroutine_await(i, v) {
        crate::coroutine::Resume::Next(x) => Ok(x),
        crate::coroutine::Resume::Throw(e) => Err(e),
        crate::coroutine::Resume::Return(x) => Ok(x),
    }
}

fn from_async_body(
    i: &mut Interp,
    this: Value,
    source: Value,
    mapfn: Value,
    this_arg: Value,
) -> Result<Value, Value> {
    if !matches!(mapfn, Value::Undefined) && !mapfn.is_callable() {
        return Err(i.make_error("TypeError", "Array.fromAsync: mapFn is not callable"));
    }
    let use_ctor = is_constructor_value(&this);
    // Prefer @@asyncIterator, then a sync iterator; otherwise treat `source` as array-like.
    // GetMethod: a present-but-non-callable @@asyncIterator/@@iterator is a TypeError.
    let get_method = |i: &mut Interp, key: &str| -> Result<Value, Value> {
        let m = match well_known_key(i, key) {
            Some(k) if !matches!(source, Value::Undefined | Value::Null) => {
                ab(i.get_member(&source, &k))?
            }
            _ => Value::Undefined,
        };
        if matches!(m, Value::Undefined | Value::Null) {
            Ok(Value::Undefined)
        } else if !m.is_callable() {
            Err(i.make_error("TypeError", "iterator method is not callable"))
        } else {
            Ok(m)
        }
    };
    let async_it = get_method(i, "asyncIterator")?;
    let sync_it = if async_it.is_callable() {
        Value::Undefined
    } else {
        get_method(i, "iterator")?
    };
    // AsyncIteratorClose/IteratorClose with a throw completion: errors are swallowed.
    let close_iter = |i: &mut Interp, iter: &Value, is_async: bool| {
        if !is_async {
            i.iterator_close(iter);
            return;
        }
        if let Ok(ret) = i.get_member(iter, "return") {
            if ret.is_callable() {
                if let Ok(r) = i.call(ret, iter.clone(), &[]) {
                    let _ = fa_await(i, r);
                }
            }
        }
    };
    if async_it.is_callable() || sync_it.is_callable() {
        let is_async = async_it.is_callable();
        // A constructor receiver builds the result via `new C()`; else a plain array.
        let arr = if use_ctor {
            ab(i.construct(this.clone(), &[]))?
        } else {
            i.make_array(Vec::new())
        };
        let method = if is_async { async_it } else { sync_it };
        let iter = ab(i.call(method, source.clone(), &[]))?;
        let next = ab(i.get_member(&iter, "next"))?;
        let mut k = 0u64;
        loop {
            // Step errors (next call / result shape / done read) reject without closing.
            let res = ab(i.call(next.clone(), iter.clone(), &[]))?;
            let res = if is_async { fa_await(i, res)? } else { res };
            if !matches!(res, Value::Obj(_)) {
                return Err(i.make_error("TypeError", "iterator result is not an object"));
            }
            let done = ab(i.get_member(&res, "done"))?;
            if i.to_boolean(&done) {
                break;
            }
            let raw = ab(i.get_member(&res, "value"))?;
            // A sync iterator's value is awaited (async-from-sync); an async iterator's value is
            // used as-is. Mapping results are always awaited. Failures close the iterator.
            let step = (|i: &mut Interp| -> Result<Value, Value> {
                let mut v = if is_async { raw } else { fa_await(i, raw)? };
                if mapfn.is_callable() {
                    let mapped =
                        ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k as f64)]))?;
                    v = fa_await(i, mapped)?;
                }
                Ok(v)
            })(i);
            let v = match step {
                Ok(v) => v,
                Err(e) => {
                    close_iter(i, &iter, is_async);
                    return Err(e);
                }
            };
            if let Err(e) = cdp_or_throw(i, &arr, &k.to_string(), v) {
                close_iter(i, &iter, is_async);
                return Err(e);
            }
            k += 1;
        }
        set_length_throw(i, &arr, k as f64)?;
        Ok(arr)
    } else {
        // Not (async/sync) iterable: treat as array-like. ToObject wraps primitives
        // (a number/boolean/symbol/bigint has no `length`, yielding an empty array);
        // null/undefined throw a TypeError.
        let o = to_object_arg(i, source.clone(), "Array.fromAsync")?;
        let ov = Value::Obj(o.clone());
        let len = ab(i.to_length(&o))?;
        let arr = if use_ctor {
            ab(i.construct(this.clone(), &[Value::Num(len as f64)]))?
        } else {
            if len > 4294967295 {
                return Err(i.make_error("RangeError", "invalid array length"));
            }
            i.make_array(Vec::new())
        };
        for k in 0..len {
            let raw = ab(i.get_member(&ov, &k.to_string()))?;
            let mut v = fa_await(i, raw)?;
            if mapfn.is_callable() {
                let mapped =
                    ab(i.call(mapfn.clone(), this_arg.clone(), &[v, Value::Num(k as f64)]))?;
                v = fa_await(i, mapped)?;
            }
            cdp_or_throw(i, &arr, &k.to_string(), v)?;
        }
        set_length_throw(i, &arr, len as f64)?;
        Ok(arr)
    }
}

/// CreateDataPropertyOrThrow (trap-aware for proxy targets).
fn cdp_or_throw(i: &mut Interp, target: &Value, key: &str, v: Value) -> Result<(), Value> {
    let Value::Obj(o) = target else {
        return Err(i.make_error("TypeError", "cannot define a property on a non-object"));
    };
    // Fast path: a fresh element on a plain, extensible Array with writable length skips the
    // descriptor-object round trip (this is the hot loop of from/map/filter/slice/concat).
    if !i.proxies.contains_key(&(Rc::as_ptr(o) as usize)) {
        let fast = {
            let b = o.borrow();
            matches!(b.exotic, Exotic::Array)
                && b.extensible
                && !b.props.contains(key)
                && b.props.get("length").map(|p| p.writable).unwrap_or(true)
        };
        if fast {
            if let Ok(idx) = key.parse::<u64>() {
                if idx < 4294967295 {
                    let len = i.array_length(o);
                    o.borrow_mut()
                        .props
                        .insert(key, Property::data(v, true, true, true));
                    if idx as usize >= len {
                        o.borrow_mut().props.insert(
                            "length",
                            Property::data(Value::Num((idx + 1) as f64), true, false, false),
                        );
                    }
                    return Ok(());
                }
            }
        }
    }
    let desc = i.new_object();
    set_data(&desc, "value", v);
    set_data(&desc, "writable", Value::Bool(true));
    set_data(&desc, "enumerable", Value::Bool(true));
    set_data(&desc, "configurable", Value::Bool(true));
    let r = if let Some((t, h)) = proxy_pair(i, target) {
        proxy_define_property(i, &t, &h, key, &Value::Obj(desc))
    } else {
        define_own_property(i, o, key, &Value::Obj(desc))
    };
    match r {
        Ok(true) => Ok(()),
        Ok(false) => Err(i.make_error("TypeError", format!("cannot define element '{key}'"))),
        Err(a) => Err(crate::interpreter::abrupt_value(a)),
    }
}

/// Set(A, "length", n, true): setter errors propagate, a refused write is a TypeError (via the
/// strict-mode write semantics).
fn set_length_throw(i: &mut Interp, arr: &Value, n: f64) -> Result<(), Value> {
    let saved = i.strict;
    i.strict = true;
    let r = i.set_member(arr, "length", Value::Num(n));
    i.strict = saved;
    r.map_err(crate::interpreter::abrupt_value)
}

fn iter_some_every(i: &mut Interp, this: Value, a: &[Value], want: bool) -> Result<Value, Value> {
    require_iterator_object(i, &this)?;
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
/// GetIteratorDirect's receiver check: an Iterator.prototype helper requires an Object `this`.
fn require_iterator_object(i: &Interp, this: &Value) -> Result<(), Value> {
    if matches!(this, Value::Obj(_)) {
        Ok(())
    } else {
        Err(i.make_error("TypeError", "Iterator method called on a non-object"))
    }
}

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
    // GetIteratorDirect requires an Object receiver (checked before the callable/limit checks).
    if !matches!(source, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "Iterator helper called on a non-object"));
    }
    if matches!(kind, "map" | "filter" | "flatMap") && !f.is_callable() {
        // A non-callable mapper/predicate still closes the underlying iterator.
        i.iterator_close(&source);
        return Err(i.make_error("TypeError", "Iterator helper argument is not callable"));
    }
    // take/drop validate the limit (ToNumber → NaN or negative is a RangeError) before reading the
    // source's `next`; any validation failure closes the underlying iterator (calls its `return`).
    let limit = if matches!(kind, "take" | "drop") {
        let raw = match i.to_number(&f) {
            Ok(n) => n,
            Err(e) => {
                i.iterator_close(&source);
                return Err(crate::interpreter::abrupt_value(e));
            }
        };
        if raw.is_nan() || raw < 0.0 {
            i.iterator_close(&source);
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

/// SetterThatIgnoresPrototypeProperties: assigning through Iterator.prototype's accessor throws
/// when the receiver IS the prototype, else creates an own data property on the receiver.
fn iterator_proto_weird_set(
    i: &mut Interp,
    this: Value,
    v: Value,
    key: &str,
) -> Result<Value, Value> {
    if let (Some(h), Value::Obj(t)) = (i.extra_protos.get("%IteratorProtoMarker%"), &this) {
        if Rc::ptr_eq(h, t) {
            return Err(i.make_error(
                "TypeError",
                "cannot assign to a property of Iterator.prototype",
            ));
        }
    }
    match &this {
        Value::Obj(o) => {
            o.borrow_mut()
                .props
                .insert(key, Property::data(v, true, true, true));
            Ok(Value::Undefined)
        }
        _ => Err(i.make_error("TypeError", "cannot create property on a primitive")),
    }
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
    set_internal(&obj, "__ai_target", target);
    set_internal(&obj, "__ai_index", Value::Num(0.0));
    set_internal(&obj, "__ai_kind", Value::Num(kind as f64));
    Value::Obj(obj)
}

/// `next()` for a String Iterator: advance one code point through the iterated string. The
/// `__si_str` slot doubles as the brand check.
fn string_iter_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = match &this {
        Value::Obj(o) if o.borrow().props.contains("__si_str") => o.clone(),
        _ => {
            return Err(i.make_error(
                "TypeError",
                "String Iterator next called on an incompatible receiver",
            ))
        }
    };
    let s = match o.borrow().props.get("__si_str").map(|p| p.value.clone()) {
        Some(Value::Str(s)) => s,
        _ => return Ok(i.iter_result_obj(Value::Undefined, true)),
    };
    let idx = match o.borrow().props.get("__si_index").map(|p| p.value.clone()) {
        Some(Value::Num(n)) => n as usize,
        _ => 0,
    };
    let mut rest = s[idx.min(s.len())..].chars();
    let Some(ch) = rest.next() else {
        // Exhausted: clear the string so the iterator stays done.
        set_internal(&o, "__si_str", Value::Undefined);
        return Ok(i.iter_result_obj(Value::Undefined, true));
    };
    // A smuggled surrogate pair is one code point: yield both scalars together.
    if let Some(next) = rest.next() {
        if crate::jstr::paired_char(ch, next).is_some() {
            set_internal(
                &o,
                "__si_index",
                Value::Num((idx + ch.len_utf8() + next.len_utf8()) as f64),
            );
            let mut both = String::new();
            both.push(ch);
            both.push(next);
            return Ok(i.iter_result_obj(Value::from_string(both), false));
        }
    }
    set_internal(&o, "__si_index", Value::Num((idx + ch.len_utf8()) as f64));
    Ok(i.iter_result_obj(Value::from_string(ch.to_string()), false))
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
        // Brand check: the receiver must be an actual async generator instance; a mismatch
        // rejects the returned promise rather than throwing.
        Value::Obj(o) if i.async_gens.contains(&(Rc::as_ptr(o) as usize)) => {
            i.drive_async_gen(Rc::as_ptr(o) as usize, r.clone(), signal)
        }
        _ => {
            let e = i.make_error(
                "TypeError",
                "AsyncGenerator method called on an incompatible receiver",
            );
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
    i.drive_async_gen_inner(key, arg(a, 1), crate::coroutine::Resume::Next(arg(a, 2)));
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
    i.drive_async_gen_inner(key, arg(a, 1), crate::coroutine::Resume::Throw(arg(a, 2)));
    Ok(Value::Undefined)
}
/// AsyncGeneratorAwaitReturn settled: complete the generator and settle the request promise.
/// `args` is `[keyMarker, resultPromise, settledValue]`.
pub(crate) fn async_gen_return_fulfil(
    i: &mut Interp,
    _t: Value,
    a: &[Value],
) -> Result<Value, Value> {
    let key = match arg(a, 0) {
        Value::Num(n) => n as usize,
        _ => return Ok(Value::Undefined),
    };
    let (r, x) = (arg(a, 1), arg(a, 2));
    if let Some(mut coro) = i.generators.remove(&key) {
        if !coro.done {
            let _ = coro.resume(i, crate::coroutine::Resume::Return(x.clone()));
        }
        i.generators.insert(key, coro);
    }
    let res = i.iter_result_obj(x, true);
    i.resolve_promise(&r, res);
    i.finish_async_gen_step(key);
    Ok(Value::Undefined)
}
pub(crate) fn async_gen_return_reject(
    i: &mut Interp,
    _t: Value,
    a: &[Value],
) -> Result<Value, Value> {
    let key = match arg(a, 0) {
        Value::Num(n) => n as usize,
        _ => return Ok(Value::Undefined),
    };
    let (r, e) = (arg(a, 1), arg(a, 2));
    if let Some(mut coro) = i.generators.remove(&key) {
        if !coro.done {
            let _ = coro.resume(i, crate::coroutine::Resume::Throw(e.clone()));
        }
        i.generators.insert(key, coro);
    }
    i.reject_promise(&r, e);
    i.finish_async_gen_step(key);
    Ok(Value::Undefined)
}
pub(crate) fn async_iterator_key(i: &Interp) -> Option<String> {
    well_known_key(i, "asyncIterator")
}

fn array_iter_next(i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    // Brand check: the receiver must carry the Array Iterator internal slots.
    if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("__ai_kind")) {
        return Err(i.make_error(
            "TypeError",
            "Array Iterator next called on an incompatible receiver",
        ));
    }
    let target = ab(i.get_member(&this, "__ai_target"))?;
    // An exhausted iterator clears its target so it stays done even if the source later grows.
    if matches!(target, Value::Undefined) {
        let result = i.new_object();
        set_data(&result, "value", Value::Undefined);
        set_data(&result, "done", Value::Bool(true));
        return Ok(Value::Obj(result));
    }
    let idx_v = ab(i.get_member(&this, "__ai_index"))?;
    let idx = ab(i.to_number(&idx_v))? as usize;
    let kind_v = ab(i.get_member(&this, "__ai_kind"))?;
    let kind = ab(i.to_number(&kind_v))? as u8;
    // A TypedArray target re-derives its length each step; an out-of-bounds (detached/shrunk-past)
    // view throws TypeError.
    let len = if let Some(info) = map_ptr(&target).and_then(|p| i.typed_arrays.get(&p).copied()) {
        match i.ta_len(&info) {
            Some(l) => l,
            None => return Err(i.make_error("TypeError", "TypedArray is out of bounds")),
        }
    } else {
        match &target {
            // LengthOfArrayLike through [[Get]], so a proxy target's traps are honored.
            Value::Obj(_) => {
                let lv = ab(i.get_member(&target, "length"))?;
                let n = ab(i.to_number(&lv))?;
                if n.is_nan() || n <= 0.0 {
                    0
                } else {
                    n.min(9007199254740991.0) as usize
                }
            }
            Value::Str(s) => crate::jstr::unit_len(s),
            _ => 0,
        }
    };
    let result = i.new_object();
    if idx >= len {
        ab(i.set_member(&this, "__ai_target", Value::Undefined))?;
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

fn norm_index(n: f64, len: i64) -> i64 {
    // ToIntegerOrInfinity: NaN maps to 0 (undefined args are handled by callers).
    if n.is_nan() {
        return 0;
    }
    let i = if n.is_infinite() {
        if n > 0.0 {
            len
        } else {
            -len - 1
        }
    } else {
        n as i64
    };
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

/// Canonicalize the locale argument to toLocale{Lower,Upper}Case and return its language subtag
/// (lowercased), or `None` when no locale was supplied. Invalid locales throw RangeError.
fn locale_case_lang(i: &mut Interp, locales: &Value) -> Result<Option<String>, Value> {
    let list = crate::intl::canonicalize_locale_list(i, locales)?;
    Ok(list
        .first()
        .and_then(|l| l.split('-').next())
        .map(|s| s.to_ascii_lowercase()))
}

/// Language-sensitive lowercasing. Turkish/Azeri map the dotted/dotless I pair specially; all other
/// languages use the default Unicode mapping.
/// The canonical combining class used by the SpecialCasing conditions (0 = base).
fn ccc(cp: u32) -> u8 {
    crate::unicode_norm_impl::ccc(cp)
}

/// From `chars[k+1..]`, the index of a COMBINING DOT ABOVE reachable across marks of combining
/// class other than 0/230 (the SpecialCasing After_I / After_Soft_Dotted scan), if any.
fn absorbable_dot_above(chars: &[char], k: usize) -> Option<usize> {
    let mut j = k + 1;
    while let Some(&m) = chars.get(j) {
        if m == '\u{0307}' {
            return Some(j);
        }
        match ccc(m as u32) {
            0 | 230 => return None,
            _ => j += 1,
        }
    }
    None
}

/// SpecialCasing More_Above: `chars[k]` is followed by a class-230 mark with no intervening
/// class 0/230 character.
fn more_above(chars: &[char], k: usize) -> bool {
    let mut j = k + 1;
    while let Some(&m) = chars.get(j) {
        match ccc(m as u32) {
            230 => return true,
            0 => return false,
            _ => j += 1,
        }
    }
    false
}

/// Unicode Soft_Dotted (the subset with lowercase dots that interact with case mapping).
fn is_soft_dotted(cp: u32) -> bool {
    matches!(
        cp,
        0x69 | 0x6A
            | 0x12F
            | 0x249
            | 0x268
            | 0x29D
            | 0x2B2
            | 0x3F3
            | 0x456
            | 0x458
            | 0x1D62
            | 0x1D96
            | 0x1DA4
            | 0x1DA8
            | 0x1E2D
            | 0x1ECB
            | 0x2071
            | 0x2148..=0x2149
            | 0x2C7C
            | 0x1D422..=0x1D423
            | 0x1D456..=0x1D457
            | 0x1D48A..=0x1D48B
            | 0x1D4BE..=0x1D4BF
            | 0x1D4F2..=0x1D4F3
            | 0x1D526..=0x1D527
            | 0x1D55A..=0x1D55B
            | 0x1D58E..=0x1D58F
            | 0x1D5C2..=0x1D5C3
            | 0x1D5F6..=0x1D5F7
            | 0x1D62A..=0x1D62B
            | 0x1D65E..=0x1D65F
            | 0x1D692..=0x1D693
            | 0x1DF1A
            | 0x1E04C..=0x1E04D
            | 0x1E068
    )
}

fn locale_lower(s: &str, lang: Option<&str>) -> String {
    match lang {
        Some("tr") | Some("az") => {
            let chars: Vec<char> = s.chars().collect();
            let mut out = String::with_capacity(s.len());
            let mut k = 0;
            while k < chars.len() {
                match chars[k] {
                    '\u{0130}' => out.push('i'), // İ → i
                    'I' => {
                        // After_I: a following combining dot above (reachable across non-0/230
                        // marks) is absorbed and I lowercases dotted; otherwise → dotless ı.
                        if let Some(dj) = absorbable_dot_above(&chars, k) {
                            out.push('i');
                            for &m in &chars[k + 1..dj] {
                                out.push(m);
                            }
                            k = dj;
                        } else {
                            out.push('\u{0131}');
                        }
                    }
                    c => out.extend(c.to_lowercase()),
                }
                k += 1;
            }
            out
        }
        Some("lt") => {
            // Lithuanian retains the dot above a lowercased I/J/Į when further above-marks
            // follow, and bakes it into the dotted-accent capitals.
            let chars: Vec<char> = s.chars().collect();
            let mut out = String::with_capacity(s.len());
            for (k, &c) in chars.iter().enumerate() {
                match c {
                    'I' if more_above(&chars, k) => out.push_str("i\u{0307}"),
                    'J' if more_above(&chars, k) => out.push_str("j\u{0307}"),
                    '\u{012E}' if more_above(&chars, k) => out.push_str("\u{012F}\u{0307}"),
                    '\u{00CC}' => out.push_str("i\u{0307}\u{0300}"),
                    '\u{00CD}' => out.push_str("i\u{0307}\u{0301}"),
                    '\u{0128}' => out.push_str("i\u{0307}\u{0303}"),
                    c => out.extend(c.to_lowercase()),
                }
            }
            out
        }
        _ => s.to_lowercase(),
    }
}

/// Language-sensitive uppercasing (Turkish/Azeri dotted-I, Lithuanian dot-above removal).
fn locale_upper(s: &str, lang: Option<&str>) -> String {
    match lang {
        Some("tr") | Some("az") => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match c {
                    'i' => out.push('\u{0130}'), // i → İ
                    '\u{0131}' => out.push('I'), // ı → I
                    c => out.extend(c.to_uppercase()),
                }
            }
            out
        }
        Some("lt") => {
            // A combining dot above after a soft-dotted base disappears when uppercasing.
            let chars: Vec<char> = s.chars().collect();
            let mut out = String::with_capacity(s.len());
            let mut k = 0;
            while k < chars.len() {
                let c = chars[k];
                out.extend(c.to_uppercase());
                if is_soft_dotted(c as u32) {
                    if let Some(dj) = absorbable_dot_above(&chars, k) {
                        for &m in &chars[k + 1..dj] {
                            out.push(m);
                        }
                        k = dj;
                    }
                }
                k += 1;
            }
            out
        }
        _ => s.to_uppercase(),
    }
}

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
    // String.prototype[@@iterator]: a lazy String Iterator over the receiver, by code point.
    if let Some(sym) = it.iterator_sym.clone() {
        let f = it.make_native("[Symbol.iterator]", 0, |i, this, _| {
            let s = this_string(i, &this)?;
            let obj = Object::new(i.extra_protos.get("%StringIteratorPrototype%").cloned());
            set_internal(&obj, "__si_str", Value::Str(s));
            set_internal(&obj, "__si_index", Value::Num(0.0));
            Ok(Value::Obj(obj))
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
        let n = ab(i.to_number(&arg(args, 0)))?;
        let idx = if n.is_nan() { 0.0 } else { n.trunc() };
        if idx < 0.0 || !idx.is_finite() {
            return Ok(Value::str(""));
        }
        Ok(match crate::jstr::UnitIter::new(&s).nth(idx as usize) {
            Some(u) => Value::from_string(crate::jstr::unit_str(u)),
            None => Value::str(""),
        })
    });
    it.def_method(&sp, "charCodeAt", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let n = ab(i.to_number(&arg(args, 0)))?;
        let idx = if n.is_nan() { 0.0 } else { n.trunc() };
        if idx < 0.0 || !idx.is_finite() {
            return Ok(Value::Num(f64::NAN));
        }
        Ok(match crate::jstr::UnitIter::new(&s).nth(idx as usize) {
            Some(u) => Value::Num(u as f64),
            None => Value::Num(f64::NAN),
        })
    });
    it.def_method(&sp, "indexOf", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let needle = ab(i.to_string(&arg(args, 0)))?;
        let chars = crate::jstr::units(&s);
        let nchars = crate::jstr::units(&needle);
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
        let chars = crate::jstr::units(&s);
        let nchars = crate::jstr::units(&needle);
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
    it.def_method(&sp, "toLocaleLowerCase", 0, |i, this, args| {
        let s = this_string(i, &this)?;
        let lang = locale_case_lang(i, &arg(args, 0))?;
        Ok(Value::from_string(locale_lower(&s, lang.as_deref())))
    });
    it.def_method(&sp, "toLocaleUpperCase", 0, |i, this, args| {
        let s = this_string(i, &this)?;
        let lang = locale_case_lang(i, &arg(args, 0))?;
        Ok(Value::from_string(locale_upper(&s, lang.as_deref())))
    });
    it.def_method(&sp, "normalize", 0, |i, this, args| {
        let s = this_string(i, &this)?;
        let form = match arg(args, 0) {
            Value::Undefined => "NFC".to_string(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        if !matches!(form.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
            return Err(i.make_error(
                "RangeError",
                "The normalization form should be one of NFC, NFD, NFKC, NFKD",
            ));
        }
        let cps = crate::jstr::code_points(&s);
        let out = crate::unicode_norm_impl::normalize(&cps, &form);
        Ok(Value::from_string(crate::jstr::from_code_points(&out)))
    });
    it.def_method(&sp, "includes", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        if arg_is_regexp(i, &arg(args, 0))? {
            return Err(i.make_error("TypeError", "argument must not be a regular expression"));
        }
        let needle = ab(i.to_string(&arg(args, 0)))?;
        let chars = crate::jstr::units(&s);
        let len = chars.len() as i64;
        let pos = str_clamp_pos(i, args.get(1), len)?;
        let nchars = crate::jstr::units(&needle);
        let found = (pos..=chars.len())
            .any(|k| k + nchars.len() <= chars.len() && chars[k..k + nchars.len()] == nchars[..]);
        Ok(Value::Bool(found))
    });
    it.def_method(&sp, "startsWith", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        if arg_is_regexp(i, &arg(args, 0))? {
            return Err(i.make_error("TypeError", "argument must not be a regular expression"));
        }
        let needle = ab(i.to_string(&arg(args, 0)))?;
        let chars = crate::jstr::units(&s);
        let len = chars.len() as i64;
        let pos = str_clamp_pos(i, args.get(1), len)?;
        let nchars = crate::jstr::units(&needle);
        Ok(Value::Bool(
            pos + nchars.len() <= chars.len() && chars[pos..pos + nchars.len()] == nchars[..],
        ))
    });
    it.def_method(&sp, "endsWith", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        if arg_is_regexp(i, &arg(args, 0))? {
            return Err(i.make_error("TypeError", "argument must not be a regular expression"));
        }
        let needle = ab(i.to_string(&arg(args, 0)))?;
        let chars = crate::jstr::units(&s);
        let len = chars.len() as i64;
        // endsWith's optional argument is the END position (default = length).
        let end = match args.get(1) {
            Some(v) if !matches!(v, Value::Undefined) => str_clamp_pos(i, Some(v), len)?,
            _ => len as usize,
        };
        let nchars = crate::jstr::units(&needle);
        Ok(Value::Bool(
            end >= nchars.len() && chars[end - nchars.len()..end] == nchars[..],
        ))
    });
    it.def_method(&sp, "slice", 2, |i, this, args| {
        let s = this_string(i, &this)?;
        let chars = crate::jstr::units(&s);
        let len = chars.len() as i64;
        let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len);
        let end = match arg(args, 1) {
            Value::Undefined => len,
            v => norm_index(ab(i.to_number(&v))?, len),
        };
        let out = if start < end {
            crate::jstr::from_units(&chars[start as usize..end as usize])
        } else {
            String::new()
        };
        Ok(Value::from_string(out))
    });
    it.def_method(&sp, "substring", 2, |i, this, args| {
        let s = this_string(i, &this)?;
        let chars = crate::jstr::units(&s);
        let len = chars.len() as i64;
        let mut a = (ab(i.to_number(&arg(args, 0)))? as i64).clamp(0, len);
        let mut b = match arg(args, 1) {
            Value::Undefined => len,
            v => (ab(i.to_number(&v))? as i64).clamp(0, len),
        };
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        Ok(Value::from_string(crate::jstr::from_units(
            &chars[a as usize..b as usize],
        )))
    });
    // Annex B B.2.3.1 String.prototype.substr(start, length).
    it.def_method(&sp, "substr", 2, |i, this, args| {
        let s = this_string(i, &this)?;
        let chars = crate::jstr::units(&s);
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
        Ok(Value::from_string(crate::jstr::from_units(
            &chars[start as usize..(start + count) as usize],
        )))
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
        let s = this_string(i, &this)?;
        Ok(Value::Bool(!crate::jstr::has_lone_surrogate(&s)))
    });
    it.def_method(&sp, "toWellFormed", 0, |i, this, _| {
        let s = this_string(i, &this)?;
        if !crate::jstr::has_lone_surrogate(&s) {
            return Ok(Value::Str(s));
        }
        let fixed: String = s
            .chars()
            .map(|c| {
                if crate::jstr::smuggled(c).is_some() {
                    '\u{FFFD}'
                } else {
                    c
                }
            })
            .collect();
        Ok(Value::from_string(fixed))
    });
    it.def_method(&sp, "trim", 0, |i, this, _| {
        Ok(Value::from_string(
            this_string(i, &this)?.trim_matches(is_js_ws).to_string(),
        ))
    });
    it.def_method(&sp, "localeCompare", 1, |i, this, args| {
        // RequireObjectCoercible + ToString this, then delegate to Intl.Collator.
        let a = Value::Str(this_string(i, &this)?);
        let b = Value::Str(ab(i.to_string(&arg(args, 0)))?);
        intl_delegate(
            i,
            "Collator",
            arg(args, 1),
            arg(args, 2),
            "compare",
            &[a, b],
        )
    });
    it.def_method(&sp, "toLocaleString", 0, |i, this, _| {
        Ok(Value::Str(this_string(i, &this)?))
    });
    it.def_method(&sp, "concat", 1, |i, this, args| {
        let mut s = this_string(i, &this)?.to_string();
        for a in args {
            s = crate::jstr::concat(&s, &ab(i.to_string(a))?);
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
        let out = s.repeat(count);
        Ok(Value::from_string(
            crate::jstr::canonicalize(&out).unwrap_or(out),
        ))
    });
    it.def_method(&sp, "split", 2, |i, this, args| {
        // RequireObjectCoercible(this), then dispatch to an Object separator's @@split.
        if matches!(this, Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "String.prototype.split called on null or undefined",
            ));
        }
        let separator = arg(args, 0);
        if matches!(separator, Value::Obj(_)) {
            if let Some(key) = well_known_key(i, "split") {
                let splitter = ab(i.get_member(&separator, &key))?;
                if !matches!(splitter, Value::Undefined | Value::Null) {
                    if !splitter.is_callable() {
                        return Err(i.make_error("TypeError", "@@split is not callable"));
                    }
                    return ab(i.call(splitter, separator.clone(), &[this.clone(), arg(args, 1)]));
                }
            }
        }
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
                // Splitting on "" yields one piece per UTF-16 code unit (a surrogate pair
                // becomes its two lone halves).
                let mut parts: Vec<Value> = if sep.is_empty() {
                    crate::jstr::units(&s)
                        .into_iter()
                        .map(|u| Value::from_string(crate::jstr::unit_str(u)))
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
        let chars = crate::jstr::units(&s);
        let len = chars.len() as i64;
        let mut idx = ab(i.to_number(&arg(args, 0)))? as i64;
        if idx < 0 {
            idx += len;
        }
        Ok(if idx < 0 || idx >= len {
            Value::Undefined
        } else {
            Value::from_string(crate::jstr::unit_str(chars[idx as usize]))
        })
    });
    it.def_method(&sp, "codePointAt", 1, |i, this, args| {
        let s = this_string(i, &this)?;
        let n = ab(i.to_number(&arg(args, 0)))?;
        let n = if n.is_nan() { 0.0 } else { n.trunc() };
        if n < 0.0 || !n.is_finite() {
            return Ok(Value::Undefined);
        }
        let idx = n as usize;
        let chars = crate::jstr::units(&s);
        Ok(match chars.get(idx) {
            Some(&u)
                if (0xD800..0xDC00).contains(&u)
                    && idx + 1 < chars.len()
                    && (0xDC00..0xE000).contains(&chars[idx + 1]) =>
            {
                let c = 0x10000 + ((u as u32 - 0xD800) << 10) + (chars[idx + 1] as u32 - 0xDC00);
                Value::Num(c as f64)
            }
            Some(&u) => Value::Num(u as f64),
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
        // RequireObjectCoercible(this), then dispatch to an Object regexp's @@match.
        if matches!(this, Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "String.prototype.match called on null or undefined",
            ));
        }
        let regexp = arg(a, 0);
        if matches!(regexp, Value::Obj(_)) {
            if let Some(key) = well_known_key(i, "match") {
                let matcher = ab(i.get_member(&regexp, &key))?;
                if !matches!(matcher, Value::Undefined | Value::Null) {
                    if !matcher.is_callable() {
                        return Err(i.make_error("TypeError", "@@match is not callable"));
                    }
                    return ab(i.call(matcher, regexp.clone(), std::slice::from_ref(&this)));
                }
            }
        }
        // Otherwise build a RegExp from the argument and invoke its @@match.
        let s = this_string(i, &this)?;
        let pattern = if matches!(regexp, Value::Undefined) {
            String::new()
        } else {
            ab(i.to_string(&regexp))?.to_string()
        };
        let rx = ab(i.make_regexp(&pattern, ""))?;
        let key = well_known_key(i, "match").unwrap();
        let matcher = ab(i.get_member(&rx, &key))?;
        ab(i.call(matcher, rx, &[Value::Str(s)]))
    });
    it.def_method(&sp, "search", 1, |i, this, a| {
        // RequireObjectCoercible(this), then dispatch to an Object regexp's @@search.
        if matches!(this, Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "String.prototype.search called on null or undefined",
            ));
        }
        let regexp = arg(a, 0);
        if matches!(regexp, Value::Obj(_)) {
            if let Some(key) = well_known_key(i, "search") {
                let searcher = ab(i.get_member(&regexp, &key))?;
                if !matches!(searcher, Value::Undefined | Value::Null) {
                    if !searcher.is_callable() {
                        return Err(i.make_error("TypeError", "@@search is not callable"));
                    }
                    return ab(i.call(searcher, regexp.clone(), std::slice::from_ref(&this)));
                }
            }
        }
        // Otherwise build a RegExp from the argument and invoke its @@search.
        let s = this_string(i, &this)?;
        let pattern = if matches!(regexp, Value::Undefined) {
            String::new()
        } else {
            ab(i.to_string(&regexp))?.to_string()
        };
        let rx = ab(i.make_regexp(&pattern, ""))?;
        let key = well_known_key(i, "search").unwrap();
        let searcher = ab(i.get_member(&rx, &key))?;
        ab(i.call(searcher, rx, &[Value::Str(s)]))
    });
    it.def_method(&sp, "matchAll", 1, |i, this, a| {
        // RequireObjectCoercible(this).
        if matches!(this, Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "String.prototype.matchAll called on null or undefined",
            ));
        }
        let regexp = arg(a, 0);
        if !matches!(regexp, Value::Undefined | Value::Null) {
            // A RegExp argument must be global; then dispatch to its @@matchAll if present.
            if arg_is_regexp(i, &regexp)? {
                let flags = ab(i.get_member(&regexp, "flags"))?;
                if matches!(flags, Value::Undefined | Value::Null) {
                    return Err(i.make_error("TypeError", "regexp.flags is null or undefined"));
                }
                let fs = ab(i.to_string(&flags))?;
                if !fs.contains('g') {
                    return Err(i.make_error(
                        "TypeError",
                        "String.prototype.matchAll called with a non-global RegExp",
                    ));
                }
            }
            if let Some(key) = well_known_key(i, "matchAll") {
                let matcher = ab(i.get_member(&regexp, &key))?;
                if !matches!(matcher, Value::Undefined | Value::Null) {
                    if !matcher.is_callable() {
                        return Err(i.make_error("TypeError", "@@matchAll is not callable"));
                    }
                    return ab(i.call(matcher, regexp.clone(), std::slice::from_ref(&this)));
                }
            }
        }
        // Otherwise build a global RegExp from the argument and invoke its @@matchAll.
        let s = this_string(i, &this)?;
        let pattern = if matches!(regexp, Value::Undefined) {
            String::new()
        } else {
            ab(i.to_string(&regexp))?.to_string()
        };
        let rx = ab(i.make_regexp(&pattern, "g"))?;
        let key = well_known_key(i, "matchAll").unwrap();
        let matcher = ab(i.get_member(&rx, &key))?;
        ab(i.call(matcher, rx, &[Value::Str(s)]))
    });
    it.def_method(&sp, "replace", 2, |i, this, args| {
        if matches!(this, Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "String.prototype.replace called on null or undefined",
            ));
        }
        // An *object* search value with a `@@replace` method (any RegExp, subclass, or custom)
        // handles it. A primitive search value never routes here (it has no own `@@replace`, and
        // consulting the prototype's would be observable), so it takes the string path below.
        let search = arg(args, 0);
        if matches!(search, Value::Obj(_)) {
            if let Some(key) = well_known_key(i, "replace") {
                let replacer = ab(i.get_member(&search, &key))?;
                if !matches!(replacer, Value::Undefined | Value::Null) {
                    if !replacer.is_callable() {
                        return Err(i.make_error("TypeError", "@@replace is not callable"));
                    }
                    return ab(i.call(replacer, search.clone(), &[this.clone(), arg(args, 1)]));
                }
            }
        }
        let s = this_string(i, &this)?.to_string();
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
        // RequireObjectCoercible(this) — but keep the raw receiver O for @@replace, which runs
        // BEFORE ToString(this).
        if matches!(this, Value::Undefined | Value::Null) {
            return Err(i.make_error(
                "TypeError",
                "String.prototype.replaceAll called on null or undefined",
            ));
        }
        let search = arg(args, 0);
        // Only an *Object* searchValue is inspected for regexp-ness / @@replace; a primitive's
        // Symbol.replace is never accessed.
        if matches!(search, Value::Obj(_)) {
            // A RegExp search value must be global: read `flags`, require it coercible, and check
            // for "g" — a non-global (or missing-flags) regexp is a TypeError.
            if arg_is_regexp(i, &search)? {
                let flags = ab(i.get_member(&search, "flags"))?;
                if matches!(flags, Value::Undefined | Value::Null) {
                    return Err(i.make_error("TypeError", "regexp flags is null or undefined"));
                }
                let flags_str = ab(i.to_string(&flags))?;
                if !flags_str.contains('g') {
                    return Err(i.make_error(
                        "TypeError",
                        "replaceAll must be called with a global RegExp",
                    ));
                }
            }
            // GetMethod(search, @@replace): if present, delegate with the raw receiver O.
            if let Some(key) = well_known_key(i, "replace") {
                let replacer = ab(i.get_member(&search, &key))?;
                if !matches!(replacer, Value::Undefined | Value::Null) {
                    if !replacer.is_callable() {
                        return Err(i.make_error("TypeError", "@@replace is not callable"));
                    }
                    return ab(i.call(replacer, search.clone(), &[this.clone(), arg(args, 1)]));
                }
            }
        }
        let s = this_string(i, &this)?.to_string();
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
        let mut units: Vec<u16> = Vec::with_capacity(args.len());
        for a in args {
            // Each argument is ToUint16'd (so -1 -> 0xFFFF, 0x10000 -> 0), not truncated.
            let num = ab(i.to_number(a))?;
            units.push(if num.is_finite() {
                num.trunc().rem_euclid(65536.0) as u16
            } else {
                0
            });
        }
        // UTF-16 decode: a surrogate pair combines into one code point; a lone half is smuggled
        // (see `jstr`), so the resulting string round-trips its exact unit sequence.
        Ok(Value::from_string(crate::jstr::from_units(&units)))
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
            let cp = n as u32;
            if (0xD800..0xE000).contains(&cp) {
                // A lone surrogate is a valid argument: smuggle it (see `jstr`).
                s.push(crate::jstr::smuggle(cp as u16));
            } else if cp >= crate::jstr::SMUGGLE_BASE {
                // A smuggle-range character is canonically its smuggled pair.
                let hi = 0xD800 + ((cp - 0x10000) >> 10);
                let lo = 0xDC00 + ((cp - 0x10000) & 0x3FF);
                s.push(crate::jstr::smuggle(hi as u16));
                s.push(crate::jstr::smuggle(lo as u16));
            } else {
                s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            }
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
        let units = crate::jstr::units(s);
        let mut b = obj.borrow_mut();
        for (idx, u) in units.iter().enumerate() {
            b.props.insert(
                idx.to_string().as_str(),
                Property::data(
                    Value::from_string(crate::jstr::unit_str(*u)),
                    false,
                    true,
                    false,
                ),
            );
        }
        b.props.insert(
            "length",
            Property::data(Value::Num(units.len() as f64), false, false, false),
        );
    }
    obj.borrow_mut().exotic = exotic;
    Value::Obj(obj)
}

/// Box only when a wrapper constructor is invoked via `new` (`new Number(x)` boxes, `Number(x)` does
/// not). The instance prototype honors newTarget (OrdinaryCreateFromConstructor), falling back to
/// newTarget's *realm's* intrinsic when its `prototype` isn't an object.
fn maybe_box(i: &mut Interp, v: Value) -> Value {
    if !i.constructing {
        return v;
    }
    let boxed = box_primitive(i, v);
    let nt = i.new_target.clone();
    if let (Value::Obj(o), Value::Obj(_)) = (&boxed, &nt) {
        let key = match o.borrow().exotic {
            Exotic::NumWrap(_) => "Number",
            Exotic::BoolWrap(_) => "Boolean",
            Exotic::StrWrap(_) => "String",
            _ => "",
        };
        if !key.is_empty() {
            match i.get_member(&nt, "prototype") {
                Ok(Value::Obj(p)) => o.borrow_mut().proto = Some(p),
                Ok(_) => {
                    if let Some(p) = ctor_realm_proto(i, &nt, key) {
                        o.borrow_mut().proto = Some(p);
                    }
                }
                Err(_) => {}
            }
        }
    }
    boxed
}

fn string_pad(i: &mut Interp, this: Value, args: &[Value], at_start: bool) -> Result<Value, Value> {
    let s = this_string(i, &this)?.to_string();
    let target = ab(i.to_number(&arg(args, 0)))? as usize;
    let cur = crate::jstr::unit_len(&s);
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
    let pad_units = crate::jstr::units(&pad);
    let fill_units: Vec<u16> = pad_units.iter().copied().cycle().take(need).collect();
    let fill = crate::jstr::from_units(&fill_units);
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

/// Number.prototype.toExponential's digit selection: the exact decimal expansion of the binary
/// value, rounded to `f` fraction digits with ties away from zero (the spec's "larger n").
/// `shortest` (fractionDigits undefined) picks the minimal digits that round-trip.
fn to_exponential(x: f64, f: usize, shortest: bool) -> String {
    let (sign, x) = if x < 0.0 { ("-", -x) } else { ("", x) };
    let fmt_exp = |exp: i32| format!("e{}{}", if exp < 0 { '-' } else { '+' }, exp.abs());
    if shortest {
        let s = format!("{x:e}");
        let (m, e) = s.split_once('e').unwrap();
        let exp: i32 = e.parse().unwrap();
        return format!("{sign}{m}{}", fmt_exp(exp));
    }
    if x == 0.0 {
        let m = if f == 0 {
            "0".to_string()
        } else {
            format!("0.{}", "0".repeat(f))
        };
        return format!("{sign}{m}e+0");
    }
    // 780 fraction digits is past the longest exact decimal expansion of any f64 mantissa, so the
    // digits (including everything after the rounding point) are exact.
    let s = format!("{x:.780e}");
    let (m, e) = s.split_once('e').unwrap();
    let mut exp: i32 = e.parse().unwrap();
    let mut digits: Vec<u8> = m
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| b - b'0')
        .collect();
    if digits.len() > f + 1 && digits[f + 1] >= 5 {
        let mut k = f + 1;
        loop {
            if k == 0 {
                digits.insert(0, 1);
                exp += 1;
                break;
            }
            k -= 1;
            if digits[k] == 9 {
                digits[k] = 0;
            } else {
                digits[k] += 1;
                break;
            }
        }
    }
    digits.truncate(f + 1);
    while digits.len() < f + 1 {
        digits.push(0);
    }
    let ds: String = digits.iter().map(|d| (d + b'0') as char).collect();
    let m = if f == 0 {
        ds
    } else {
        format!("{}.{}", &ds[..1], &ds[1..])
    };
    format!("{sign}{m}{}", fmt_exp(exp))
}

fn install_number(it: &mut Interp) {
    let np = it.number_proto.clone();
    it.def_method(&np, "toLocaleString", 0, |i, this, args| {
        let n = this_number(i, &this)?;
        intl_delegate(
            i,
            "NumberFormat",
            arg(args, 0),
            arg(args, 1),
            "format",
            &[Value::Num(n)],
        )
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
        let x = this_number(i, &this)?;
        let fd = arg(args, 0);
        let f = ab(i.to_number(&fd))?;
        let f = if f.is_nan() { 0.0 } else { f.trunc() };
        if x.is_nan() {
            return Ok(Value::str("NaN"));
        }
        if x.is_infinite() {
            return Ok(Value::str(if x > 0.0 { "Infinity" } else { "-Infinity" }));
        }
        if !(0.0..=100.0).contains(&f) {
            return Err(i.make_error(
                "RangeError",
                "toExponential() argument must be between 0 and 100",
            ));
        }
        let f = f as usize;
        Ok(Value::from_string(to_exponential(
            x,
            f,
            matches!(fd, Value::Undefined),
        )))
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
        ("MIN_VALUE", f64::from_bits(1)), // 5e-324, the smallest subnormal
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

    // The intrinsic %Symbol% is cached so well-known-symbol lookups survive `globalThis.Symbol`
    // being replaced by user code.
    it.extra_protos.insert("%SymbolCtor%", ctor.clone());
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
        if let Some(d) = crate::interpreter::sym_for_get(&key) {
            return Ok(Value::Sym(d));
        }
        let sym = i.new_symbol(Some(Rc::from(key.as_str())));
        if let Value::Sym(d) = &sym {
            crate::interpreter::sym_for_insert(key, d.clone());
        }
        Ok(sym)
    });
    it.def_method(&ctor, "keyFor", 1, |i, _this, args| {
        let Value::Sym(s) = arg(args, 0) else {
            return Err(i.make_error("TypeError", "Symbol.keyFor: argument is not a Symbol"));
        };
        Ok(crate::interpreter::sym_for_key_of(&s)
            .map(Value::from_string)
            .unwrap_or(Value::Undefined))
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
        // Wrap on overflow (mod 2^128), matching the lexer, so BigInt("<huge>") equals the same
        // literal even beyond i128. (Full arbitrary precision is unimplemented.)
        let v = bigint_digits_wrapping(digits, 10);
        return Some(if neg { v.wrapping_neg() } else { v });
    };
    if body.is_empty() || !body.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    Some(bigint_digits_wrapping(body, radix))
}

/// Parse validated `digits` in `radix` to i128, wrapping on overflow (mod 2^128). Mirrors the
/// lexer's BigInt handling so a `BigInt("…")` string and the equivalent literal compare equal.
fn bigint_digits_wrapping(digits: &str, radix: u32) -> i128 {
    let mut acc: i128 = 0;
    for c in digits.chars() {
        if let Some(d) = c.to_digit(radix) {
            acc = acc.wrapping_mul(radix as i128).wrapping_add(d as i128);
        }
    }
    acc
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
    it.def_method(&proto, "toString", 0, |i, this, a| {
        let n = this_bigint(i, &this)?;
        let radix = match arg(a, 0) {
            Value::Undefined => 10,
            v => {
                let r = ab(i.to_number(&v))?.trunc();
                if !(2.0..=36.0).contains(&r) {
                    return Err(
                        i.make_error("RangeError", "toString radix must be between 2 and 36")
                    );
                }
                r as u32
            }
        };
        Ok(Value::from_string(bigint_to_radix(n, radix)))
    });
    it.def_method(&proto, "valueOf", 0, |i, this, _| {
        Ok(Value::BigInt(this_bigint(i, &this)?))
    });
    it.def_method(&proto, "toLocaleString", 0, |i, this, args| {
        let b = this_bigint(i, &this)?;
        intl_delegate(
            i,
            "NumberFormat",
            arg(args, 0),
            arg(args, 1),
            "format",
            &[Value::BigInt(b)],
        )
    });
    let ctor = it.make_native("BigInt", 1, |i, _t, a| {
        if i.constructing {
            return Err(i.make_error("TypeError", "BigInt is not a constructor"));
        }
        // ToPrimitive(value, number) first; a Number primitive then goes through
        // NumberToBigInt (RangeError for NaN/Infinity/non-integral), unlike plain ToBigInt.
        let prim = match arg(a, 0) {
            v @ Value::Obj(_) => ab(i.to_primitive(&v, crate::eval::Hint::Number))?,
            v => v,
        };
        match prim {
            Value::BigInt(n) => Ok(Value::BigInt(n)),
            Value::Num(n) => {
                if n.is_finite() && n.fract() == 0.0 {
                    Ok(Value::BigInt(n as i128))
                } else {
                    Err(i.make_error("RangeError", "The number cannot be converted to a BigInt"))
                }
            }
            Value::Bool(b) => Ok(Value::BigInt(if b { 1 } else { 0 })),
            Value::Str(s) => string_to_bigint(&s)
                .map(Value::BigInt)
                .ok_or_else(|| i.make_error("SyntaxError", "Cannot convert string to a BigInt")),
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
    // Math.round ties toward +Inf and keeps a negative sign for [-0.5, 0). Computed from floor(x)
    // (not floor(x + 0.5), which wrongly rounds up e.g. 0.5 - ε/4 and some large odd integers).
    unary!("round", |x: f64| {
        if x.is_nan() || x.is_infinite() || x == 0.0 {
            x
        } else {
            let f = x.floor();
            let r = if x - f >= 0.5 { f + 1.0 } else { f };
            if r == 0.0 && x < 0.0 {
                -0.0
            } else {
                r
            }
        }
    });
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
        |x: f64| crate::value::f16_to_f32(crate::value::f64_to_f16(x)) as f64
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
            let s = fsum_exact(&finite);
            if s.is_finite() {
                s
            } else {
                // The exact-summation partials transiently overflowed. Retry on values scaled by a
                // power of two (exact, so the correctly-rounded result is unchanged) centred near
                // 2^500, then scale back — a genuine overflow re-materialises as ±Infinity.
                let max_abs = finite.iter().map(|x| x.abs()).fold(0.0_f64, f64::max);
                let scale_exp = max_abs.log2().floor() as i32 - 500;
                let down = 2f64.powi(-scale_exp);
                let up = 2f64.powi(scale_exp);
                let scaled: Vec<f64> = finite.iter().map(|&x| x * down).collect();
                fsum_exact(&scaled) * up
            }
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
        let base = ab(i.to_number(&arg(a, 0)))?;
        let exp = ab(i.to_number(&arg(a, 1)))?;
        // Number::exponentiate special cases Rust's powf doesn't share: a NaN exponent is NaN even
        // for base 1, and a base of ±1 with an infinite exponent is NaN.
        let r = if exp.is_nan() || (base.abs() == 1.0 && exp.is_infinite()) {
            f64::NAN
        } else {
            base.powf(exp)
        };
        Ok(Value::Num(r))
    });
    it.def_method(&math, "atan2", 2, |i, _t, a| {
        Ok(Value::Num(
            ab(i.to_number(&arg(a, 0)))?.atan2(ab(i.to_number(&arg(a, 1)))?),
        ))
    });
    it.def_method(&math, "max", 2, |i, _t, a| {
        // ToNumber every argument first (side effects in order), then reduce. +0 is larger than -0.
        let mut nums = Vec::with_capacity(a.len());
        for v in a {
            nums.push(ab(i.to_number(v))?);
        }
        let mut m = f64::NEG_INFINITY;
        for &n in &nums {
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n > m || (n == 0.0 && m == 0.0 && n.is_sign_positive() && m.is_sign_negative()) {
                m = n;
            }
        }
        Ok(Value::Num(m))
    });
    it.def_method(&math, "min", 2, |i, _t, a| {
        let mut nums = Vec::with_capacity(a.len());
        for v in a {
            nums.push(ab(i.to_number(v))?);
        }
        let mut m = f64::INFINITY;
        for &n in &nums {
            if n.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            if n < m || (n == 0.0 && m == 0.0 && n.is_sign_negative() && m.is_sign_positive()) {
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
        // The radix is ToUint32'd (wrapping), so e.g. 2^32+2 means radix 2 and Infinity means 0.
        let radix = match arg(a, 1) {
            Value::Undefined => 0,
            v => ab(i.to_uint32(&v))?,
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
        for c in crate::jstr::units(&s) {
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
        Ok(Value::from_string(crate::jstr::from_units(&units)))
    });
    global_fn(it, "encodeURIComponent", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_encode(&s, "")
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });
    global_fn(it, "encodeURI", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_encode(&s, ";,/?:@&=+$#")
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });
    global_fn(it, "decodeURIComponent", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_decode(&s, "")
            .map(Value::from_string)
            .ok_or_else(|| i.make_error("URIError", "URI malformed"))
    });
    // decodeURI leaves escapes of the reservedSet (and '#') untouched.
    global_fn(it, "decodeURI", 1, |i, _t, a| {
        let s = ab(i.to_string(&arg(a, 0)))?;
        uri_decode(&s, ";/?:@&=+$,#")
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
        let env = i.global_env.clone();
        ab(i.perform_eval(&code, &env, false))
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
    // StrDecimalLiteral: optional sign, then "Infinity" or digits [. digits] [exponent]. Scan the
    // longest prefix that is itself a valid literal (so "1ex" -> 1, not NaN from "1e").
    let t = s.trim_start_matches(is_js_ws);
    let bytes = t.as_bytes();
    let mut pos = 0;
    let neg = match bytes.first() {
        Some(b'-') => {
            pos += 1;
            true
        }
        Some(b'+') => {
            pos += 1;
            false
        }
        _ => false,
    };
    if t[pos..].starts_with("Infinity") {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let mut digits = 0;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        pos += 1;
        digits += 1;
    }
    if pos < bytes.len() && bytes[pos] == b'.' {
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return f64::NAN;
    }
    // An exponent part only counts if at least one digit follows the marker (and optional sign).
    if pos < bytes.len() && matches!(bytes[pos], b'e' | b'E') {
        let mut ep = pos + 1;
        if ep < bytes.len() && matches!(bytes[ep], b'+' | b'-') {
            ep += 1;
        }
        let exp_start = ep;
        while ep < bytes.len() && bytes[ep].is_ascii_digit() {
            ep += 1;
        }
        if ep > exp_start {
            pos = ep;
        }
    }
    t[..pos].parse::<f64>().unwrap_or(f64::NAN)
}
