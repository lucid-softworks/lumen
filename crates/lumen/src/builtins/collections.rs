//! Collection installation and constructors; storage and method families have separate owners.

use super::collection_data::CollectionData;
use super::{ab, new_from_ctor, set_internal, set_to_string_tag, step_iter_with};
use crate::interpreter::Interp;
use crate::value::{Object, Value};
use std::rc::Rc;

mod iteration;
mod set_methods;
mod strong;
mod weak;
use iteration::map_set_iter_next;
use set_methods::install_set_methods;
use strong::{install_map_like, install_map_methods};
use weak::install_weak;

pub(super) fn install_collections(it: &mut Interp) {
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
    i.map_data.insert(ptr, CollectionData::default());
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

/// Count the live (non-tombstone) entries of a collection.
fn coll_live_len(i: &Interp, ptr: usize) -> usize {
    i.map_data.get(&ptr).map(CollectionData::len).unwrap_or(0)
}

/// CoerceKey for Map/Set: `-0` is canonicalized to `+0` so a stored key (and any key handed to a
/// callback or iterated) is `+0`, per spec.
fn canonicalize_map_key(k: Value) -> Value {
    match k {
        Value::Num(n) if n == 0.0 && n.is_sign_negative() => Value::Num(0.0),
        other => other,
    }
}
