//! Weak collection brand checks and prototype methods.
//! Weakness is not modeled yet: entries remain strongly stored. Brand and key checks here
//! enforce the distinct WeakMap/WeakSet APIs without changing that storage limitation.

use crate::builtins::collection_data::CollectionKind;
use crate::builtins::{ab, arg, can_be_held_weakly, map_ptr, set_to_string_tag};
use crate::interpreter::Interp;
use crate::value::{set_builtin, NativeFn, Object, Property, Value};

/// Require the exact weak collection slot; ordinary properties cannot supply it.
fn weak_brand_ptr(i: &Interp, this: &Value, want: CollectionKind) -> Result<usize, Value> {
    map_ptr(this)
        .filter(|ptr| i.map_data.get(ptr).is_some_and(|data| data.kind() == want))
        .ok_or_else(|| i.make_error("TypeError", "method called on incompatible receiver"))
}

fn weak_has<const SET: bool>(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let kind = if SET {
        CollectionKind::WeakSet
    } else {
        CollectionKind::WeakMap
    };
    let ptr = weak_brand_ptr(i, &this, kind)?;
    let key = args.first().unwrap_or(&Value::Undefined);
    Ok(Value::Bool(i.map_data[&ptr].contains(key)))
}

fn weak_delete<const SET: bool>(
    i: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let kind = if SET {
        CollectionKind::WeakSet
    } else {
        CollectionKind::WeakMap
    };
    let ptr = weak_brand_ptr(i, &this, kind)?;
    let key = args.first().unwrap_or(&Value::Undefined);
    Ok(Value::Bool(
        i.map_data.get_mut(&ptr).unwrap().remove_weak(key),
    ))
}

pub(super) fn install_weak(it: &mut Interp, name: &'static str, is_set: bool, ctor_fn: NativeFn) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());
    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, CollectionKind::WeakSet)?;
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
            let ptr = weak_brand_ptr(i, &this, CollectionKind::WeakMap)?;
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
            let ptr = weak_brand_ptr(i, &this, CollectionKind::WeakMap)?;
            let key = arg(a, 0);
            Ok(i.map_data
                .get(&ptr)
                .and_then(|e| e.lookup(&key).cloned())
                .unwrap_or(Value::Undefined))
        });
        // Upsert proposal: getOrInsert(key, value) / getOrInsertComputed(key, callbackfn).
        it.def_method(&proto, "getOrInsert", 2, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, CollectionKind::WeakMap)?;
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
            let ptr = weak_brand_ptr(i, &this, CollectionKind::WeakMap)?;
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
    let has: NativeFn = if is_set {
        weak_has::<true>
    } else {
        weak_has::<false>
    };
    let delete: NativeFn = if is_set {
        weak_delete::<true>
    } else {
        weak_delete::<false>
    };
    it.def_method(&proto, "has", 1, has);
    it.def_method(&proto, "delete", 1, delete);
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
