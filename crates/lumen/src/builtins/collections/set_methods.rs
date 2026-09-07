//! Set algebra and set-like protocol helpers.

use super::{canonicalize_map_key, coll_live_len};
use crate::builtins::collection_data::{CollectionData, CollectionKind};
use crate::builtins::{ab, arg, coll_ptr_kind, new_from_ctor, same_value_zero};
use crate::interpreter::Interp;
use crate::value::{Object, Value};
use std::rc::Rc;

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
    let mut entries = CollectionData::new(CollectionKind::Set);
    for v in values {
        // Set records canonicalize -0 to +0.
        let v = canonicalize_map_key(v);
        entries.insert(v.clone(), v);
    }
    i.gc_pin(&obj);
    i.map_data.insert(ptr, entries);
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

/// SetDataHas against the LIVE backing data (skipping tombstones) — set-like callbacks may have
/// mutated the receiver since any snapshot was taken.
fn set_data_has(i: &Interp, ptr: usize, v: &Value) -> bool {
    match i.map_data.get(&ptr) {
        Some(entries) => entries.contains(v),
        None => false,
    }
}

pub(super) fn install_set_methods(it: &mut Interp) {
    let sp = it.extra_protos.get("Set").cloned().unwrap();
    it.def_method(&sp, "union", 1, |i, this, a| {
        // GetSetRecord (which may run `has`/`size`/`keys` getters that mutate this Set) happens
        // BEFORE the result is snapshotted from O.[[SetData]], per spec.
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        // GetKeysIterator (keys() call + `next` get) precedes the [[SetData]] copy, so mutations
        // those getters make to the receiver are visible in the result.
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        let mut vals = set_values(i, &this)?;
        while let Some(k) = set_like_next(i, &iter, &next)? {
            if !vals.iter().any(|v| same_value_zero(v, &k)) {
                vals.push(k);
            }
        }
        Ok(new_set(i, vals))
    });
    it.def_method(&sp, "intersection", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let mut out = Vec::new();
        if (coll_live_len(i, ptr) as f64) <= other_size {
            // Walk this Set LIVE by index, probing the other's `has` — the callback may delete
            // and re-append entries, and the walk observes that (appended entries are visited).
            let mut idx = 0usize;
            loop {
                let entry = i.map_data.get(&ptr).and_then(|e| e.next(&mut idx).cloned());
                let (k, _) = match entry {
                    Some(kv) => kv,
                    None => break,
                };
                if set_like_has(i, &has, &arg(a, 0), &k)?
                    && !out.iter().any(|o| same_value_zero(o, &k))
                {
                    out.push(k);
                }
            }
        } else {
            // Iterate the other's keys, probing this Set's LIVE data (no `has` calls on the other).
            let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
            while let Some(k) = set_like_next(i, &iter, &next)? {
                if set_data_has(i, ptr, &k) && !out.iter().any(|o| same_value_zero(o, &k)) {
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
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        // GetKeysIterator precedes the [[SetData]] copy. For each key of `other`: present in the
        // LIVE receiver → remove it from the result (in both); absent → append if not already in
        // the result (only in other). Removal empties the slot (order is preserved).
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        let mut result: Vec<Option<Value>> = set_values(i, &this)?.into_iter().map(Some).collect();
        while let Some(k) = set_like_next(i, &iter, &next)? {
            let in_result = result.iter().flatten().any(|v| same_value_zero(v, &k));
            if set_data_has(i, ptr, &k) {
                if in_result {
                    for slot in result.iter_mut() {
                        if matches!(&slot, Some(v) if same_value_zero(v, &k)) {
                            *slot = None;
                        }
                    }
                }
            } else if !in_result {
                result.push(Some(k));
            }
        }
        let out: Vec<Value> = result.into_iter().flatten().collect();
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
            let entry = i.map_data.get(&ptr).and_then(|e| e.next(&mut idx).cloned());
            let (k, _) = match entry {
                Some(kv) => kv,
                None => break,
            };
            if !set_like_has(i, &has, &arg(a, 0), &k)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "isSupersetOf", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, other_size) = set_record(i, &arg(a, 0))?;
        // A smaller set cannot be a superset; otherwise every other key must be in this. The
        // other's keys are iterated lazily against the LIVE receiver data (the iterator may add
        // entries), closing the iterator if a missing key exits early.
        if (coll_live_len(i, ptr) as f64) < other_size {
            return Ok(Value::Bool(false));
        }
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        while let Some(k) = set_like_next(i, &iter, &next)? {
            if !set_data_has(i, ptr, &k) {
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
                let entry = i.map_data.get(&ptr).and_then(|e| e.next(&mut idx).cloned());
                let (k, _) = match entry {
                    Some(kv) => kv,
                    None => break,
                };
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
