//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

/// WeakRef / FinalizationRegistry. lumen's collector never observably reclaims during a test, so
/// WeakRef holds its target (deref always returns it) and FinalizationRegistry callbacks never fire.
pub(super) fn install_weak_refs(it: &mut Interp) {
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
