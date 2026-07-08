//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

pub(super) fn install_reflect(it: &mut Interp) {
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
            Value::Obj(o) => {
                let key = ab(i.to_property_key(&arg(a, 1)))?;
                ab(i.defer_trigger(&o, Some(&key)))?;
                o
            }
            _ => {
                return Err(i.make_error(
                    "TypeError",
                    "Reflect.getOwnPropertyDescriptor on non-object",
                ))
            }
        };
        let key = ab(i.to_property_key(&arg(a, 1)))?;
        if Interp::is_private_key(&key) {
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
            ab(i.defer_trigger(&o, Some(&key)))?;
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
        ab(i.defer_trigger(&o, None))?;
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
                out.push(Value::Str(k.into()));
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
        // IsCallable(target) is checked before the argument list is read.
        if !arg(a, 0).is_callable() {
            return Err(i.make_error("TypeError", "Reflect.apply target is not callable"));
        }
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
