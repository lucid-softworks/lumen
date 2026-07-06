//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_promise(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("Promise", proto.clone());
    if let Some(key) = to_string_tag_key(it) {
        proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("Promise"), false, false, true),
        );
    }
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
        let (result, res_f, rej_f) = new_promise_capability_full(i, &c)?;
        if map_ptr(&result).map(|p| i.promises.contains_key(&p)) == Some(true) {
            i.promise_then_into(&this, arg(a, 0), arg(a, 1), result.clone());
        } else {
            // The constructor produced a promise we don't manage: settle it through the
            // capability's own resolve/reject functions via a native shadow promise.
            let shadow = i.new_promise();
            i.promise_then_into(&this, arg(a, 0), arg(a, 1), shadow.clone());
            let dummy = i.new_promise();
            i.promise_then_into(&shadow, res_f, rej_f, dummy);
        }
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
        // OrdinaryCreateFromConstructor: newTarget's prototype (its getter may throw).
        if let (Value::Obj(p), nt @ Value::Obj(_)) = (&promise, &i.new_target.clone()) {
            match ab(i.get_member(nt, "prototype"))? {
                Value::Obj(proto) => p.borrow_mut().proto = Some(proto),
                _ => {
                    if let Some(proto) = ctor_realm_proto(i, &nt.clone(), "Promise")? {
                        p.borrow_mut().proto = Some(proto);
                    }
                }
            }
        }
        let (res, rej) = i.make_resolver_pair(&promise);
        if let Err(Abrupt::Throw(e)) = i.call(executor, Value::Undefined, &[res, rej.clone()]) {
            // The catch goes through the reject RESOLVER: a no-op once resolve/reject ran.
            let _ = i.call(rej, Value::Undefined, &[e]);
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
        let (resolve, reject) = i.make_resolver_pair(&promise);
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
        // NewPromiseCapability(C): resolution goes through the capability's own resolve.
        let (promise, resolve_fn, _reject_fn) = new_promise_capability_full(i, &t)?;
        ab(i.call(resolve_fn, Value::Undefined, &[v]))?;
        Ok(promise)
    });
    it.def_method(&ctor, "reject", 1, |i, t, a| {
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.reject called on a non-object"));
        }
        // NewPromiseCapability(C): rejection goes through the capability's own reject.
        let (promise, _resolve_fn, reject_fn) = new_promise_capability_full(i, &t)?;
        ab(i.call(reject_fn, Value::Undefined, &[arg(a, 0)]))?;
        Ok(promise)
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
            capability_resolve_or_reject(i, resolve_fn, reject_fn, results);
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
            capability_resolve_or_reject(i, resolve_fn, reject_fn, results);
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
        promise_keyed_combinator(i, t, arg(a, 0), false)
    });
    it.def_method(&ctor, "allSettledKeyed", 1, |i, t, a| {
        promise_keyed_combinator(i, t, arg(a, 0), true)
    });
    it.def_method(&ctor, "try", 1, |i, t, a| {
        // Promise.try(fn, ...args): NewPromiseCapability(this), call fn synchronously, and settle
        // through the capability.
        if !matches!(t, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "Promise.try called on a non-object"));
        }
        let (promise, resolve_fn, reject_fn) = new_promise_capability_full(i, &t)?;
        let func = arg(a, 0);
        let rest: Vec<Value> = a.iter().skip(1).cloned().collect();
        match ab(i.call(func, Value::Undefined, &rest)) {
            Ok(v) => ab(i.call(resolve_fn, Value::Undefined, &[v]))?,
            Err(e) => ab(i.call(reject_fn, Value::Undefined, &[e]))?,
        };
        Ok(promise)
    });
    install_species(it, &ctor);
    set_builtin(&it.global, "Promise", Value::Obj(ctor));
}
