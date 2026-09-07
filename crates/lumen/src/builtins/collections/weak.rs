//! Weak collection brand checks and prototype methods.

use crate::builtins::{ab, arg, can_be_held_weakly, map_ptr, set_to_string_tag};
use crate::interpreter::Interp;
use crate::value::{set_builtin, NativeFn, Object, Property, Value};

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
        .and_then(|o| o.borrow().props.get("__ck").map(|p| p.value()));
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

pub(super) fn install_weak(it: &mut Interp, name: &'static str, is_set: bool, ctor_fn: NativeFn) {
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
            e.insert(key.clone(), key);
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
            e.insert(key, val);
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
                .and_then(|e| e.lookup(&key).cloned())
                .unwrap_or(Value::Undefined))
        });
        // Upsert proposal: getOrInsert(key, value) / getOrInsertComputed(key, callbackfn).
        it.def_method(&proto, "getOrInsert", 2, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let key = arg(a, 0);
            if !can_be_held_weakly(i, &key) {
                return Err(i.make_error("TypeError", "Invalid value used as weak map key"));
            }
            if let Some(v) = i.map_data[&ptr].lookup(&key) {
                return Ok(v.clone());
            }
            let value = arg(a, 1);
            i.map_data
                .entry(ptr)
                .or_default()
                .insert(key, value.clone());
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
            if let Some(v) = i.map_data[&ptr].lookup(&key) {
                return Ok(v.clone());
            }
            let value = ab(i.call(cb, Value::Undefined, std::slice::from_ref(&key)))?;
            // The callback may have inserted the key; the computed value overwrites that mutation.
            i.map_data
                .entry(ptr)
                .or_default()
                .insert(key, value.clone());
            Ok(value)
        });
    }
    it.def_method(&proto, "has", 1, |i, this, a| {
        let ptr = weak_brand_ptr(i, &this, "Weak")?;
        let key = arg(a, 0);
        Ok(Value::Bool(
            i.map_data
                .get(&ptr)
                .map(|e| e.contains(&key))
                .unwrap_or(false),
        ))
    });
    it.def_method(&proto, "delete", 1, |i, this, a| {
        let ptr = weak_brand_ptr(i, &this, "Weak")?;
        let key = arg(a, 0);
        let mut removed = false;
        if let Some(e) = i.map_data.get_mut(&ptr) {
            removed = e.remove_weak(&key);
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
