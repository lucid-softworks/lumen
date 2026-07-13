//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

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
    let kind = o.borrow().props.get("__ds_kind").map(|p| p.value());
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
                .map(|p| p.value())
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
    bound.borrow_mut().call = Callable::bound(
        target,
        Value::Undefined,
        vec![
            list.clone(),
            Value::Num(idx as f64),
            result.clone(),
            Value::Bool(err.is_some()),
            err.clone().unwrap_or(Value::Undefined),
        ],
    );
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

pub(super) fn install_disposable_stack(it: &mut Interp) {
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
        Property::accessor_prop(Some(Value::Obj(disposed_getter)), None, false, true),
    );
    if let Some(key) = well_known_key(it, "dispose") {
        let dispose = proto.borrow().props.get("dispose").map(|p| p.value());
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
pub(super) fn install_async_disposable_stack(it: &mut Interp) {
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
            bound.borrow_mut().call =
                Callable::bound(target, Value::Undefined, vec![disp, v.clone()]);
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
        Property::accessor_prop(Some(Value::Obj(disposed_getter)), None, false, true),
    );
    if let Some(key) = well_known_key(it, "asyncDispose") {
        let d = proto.borrow().props.get("disposeAsync").map(|p| p.value());
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
