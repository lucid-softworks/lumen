//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

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

pub(super) fn install_proxy(it: &mut Interp) {
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
