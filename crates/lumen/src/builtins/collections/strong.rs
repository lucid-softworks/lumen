//! Map and Set prototype methods.

use super::iteration::{collection_for_each, collection_iter_kind};
use super::{canonicalize_map_key, coll_live_len};
use crate::builtins::collection_data::CollectionData;
use crate::builtins::{
    ab, arg, coll_ptr_kind, install_species, same_value_zero, set_internal, set_to_string_tag,
};
use crate::interpreter::Interp;
use crate::value::{set_builtin, NativeFn, Object, Property, Value};
use std::rc::Rc;

fn map_size(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
    Ok(Value::Num(coll_live_len(i, ptr) as f64))
}

fn set_size(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
    Ok(Value::Num(coll_live_len(i, ptr) as f64))
}

pub(super) fn install_map_methods(it: &mut Interp) {
    let mp = it.extra_protos.get("Map").cloned().unwrap();
    // getOrInsert(key, value): return the existing value, or insert and return `value`.
    it.def_method(&mp, "getOrInsert", 2, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
        let key = arg(a, 0);
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
    it.def_method(&mp, "getOrInsertComputed", 2, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
        // CoerceKey: -0 is canonicalized to +0 (so the callback and stored key see +0).
        let key = canonicalize_map_key(arg(a, 0));
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

/// Map and Set share almost everything; `is_set` flips key/value handling and method names.
pub(super) fn install_map_like(
    it: &mut Interp,
    name: &'static str,
    is_set: bool,
    ctor_fn: NativeFn,
) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());

    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = canonicalize_map_key(arg(a, 0));
            let e = i.map_data.entry(ptr).or_default();
            e.insert(key.clone(), key);
            Ok(this)
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let (key, val) = (canonicalize_map_key(arg(a, 0)), arg(a, 1));
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
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = arg(a, 0);
            Ok(i.map_data
                .get(&ptr)
                .and_then(|e| e.lookup(&key).cloned())
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
                    .map(|e| e.contains(&key))
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
                    .map(|e| e.contains(&key))
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
            let mut removed = false;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                removed = e.remove(&key);
            }
            Ok(Value::Bool(removed))
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = canonicalize_map_key(arg(a, 0));
            let mut removed = false;
            if let Some(e) = i.map_data.get_mut(&ptr) {
                removed = e.remove(&key);
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
        Property::accessor_prop(Some(Value::Obj(size_getter)), None, false, true),
    );
    // @@iterator: Set -> values, Map -> entries.
    if let Some(sym) = it.iterator_sym.clone() {
        let default = if is_set { "values" } else { "entries" };
        let f = proto
            .borrow()
            .props
            .get(default)
            .map(|p| p.value())
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
            let entries: CollectionData = groups
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
