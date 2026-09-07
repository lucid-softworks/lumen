//! Live Map and Set iteration across mutation.

use crate::builtins::{ab, arg, coll_ptr, coll_ptr_kind, iter_result, map_ptr, set_internal};
use crate::interpreter::Interp;
use crate::value::{set_builtin, Gc, Object, Value};

/// Build a live iterator over a Map/Set. `kind`: 0 = values, 1 = keys, 2 = [key,value].
/// Like [`collection_iter`] but brand-checks the exact collection kind ("Set" / "Map").
pub(super) fn collection_iter_kind(
    i: &mut Interp,
    this: &Value,
    kind: u8,
    want: &str,
) -> Result<Value, Value> {
    coll_ptr_kind(i, this, Some(want))?;
    collection_iter(i, this, kind)
}

/// forEach shared by Map/Set, brand-checking the exact kind.
pub(super) fn collection_for_each(
    i: &mut Interp,
    this: Value,
    a: &[Value],
    want: Option<&str>,
) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, want)?;
    let cb = arg(a, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "forEach callback is not callable"));
    }
    let cb_this = arg(a, 1);
    // Iterate the LIVE backing list by index (positions are stable — deletes leave tombstones), so
    // entries appended during the callback are visited and deleted entries are skipped.
    let mut idx = 0usize;
    loop {
        let entry = i.map_data.get(&ptr).and_then(|e| e.next(&mut idx).cloned());
        let (k, v) = match entry {
            Some(kv) => kv,
            None => break,
        };
        ab(i.call(cb.clone(), cb_this.clone(), &[v, k, this.clone()]))?;
    }
    Ok(Value::Undefined)
}

fn collection_iter(i: &mut Interp, this: &Value, kind: u8) -> Result<Value, Value> {
    coll_ptr(i, this)?; // brand check (a real Map/Set)
    let is_set = this
        .as_obj()
        .and_then(|o| o.borrow().props.get("__ck").map(|p| p.value()))
        .map(|v| matches!(v, Value::Str(ref s) if &**s == "Set"))
        .unwrap_or(false);
    let key = if is_set {
        "%SetIteratorPrototype%"
    } else {
        "%MapIteratorPrototype%"
    };
    let proto = i
        .extra_protos
        .get(key)
        .cloned()
        .or_else(|| i.extra_protos.get("%IteratorPrototype%").cloned());
    let obj = Object::new(proto);
    set_builtin(&obj, "__ci_coll", this.clone());
    set_builtin(&obj, "__ci_index", Value::Num(0.0));
    set_builtin(&obj, "__ci_kind", Value::Num(kind as f64));
    Ok(Value::Obj(obj))
}

/// `next()` for a Map/Set iterator: reads the live backing entries at the current index (so entries
/// appended during iteration are observed). The `__ci_coll` slot is the brand.
pub(super) fn map_set_iter_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let coll = this
        .as_obj()
        .and_then(|o| o.borrow().props.get("__ci_coll").map(|p| p.value()));
    let coll = match coll {
        Some(c) => c,
        None => return Err(i.make_error("TypeError", "not a Map/Set Iterator")),
    };
    let obj = this.as_obj().unwrap();
    let num = |o: &Gc, k: &str| -> f64 {
        match o.borrow().props.get(k).map(|p| p.value()) {
            Some(Value::Num(n)) => n,
            _ => 0.0,
        }
    };
    // A once-exhausted iterator stays done, even if the collection later grows.
    if matches!(
        obj.borrow().props.get("__ci_done").map(|p| p.value()),
        Some(Value::Bool(true))
    ) {
        return Ok(iter_result(i, Value::Undefined, true));
    }
    let mut idx = num(obj, "__ci_index") as usize;
    let kind = num(obj, "__ci_kind") as u8;
    let coll_ptr = map_ptr(&coll);
    // Skip tombstoned (deleted) slots so the iterator observes a live view.
    let entry = coll_ptr
        .and_then(|p| i.map_data.get(&p))
        .and_then(|e| e.next(&mut idx).cloned());
    match entry {
        Some((k, v)) => {
            set_internal(obj, "__ci_index", Value::Num(idx as f64));
            let val = match kind {
                1 => k,
                2 => i.make_array(vec![k, v]),
                _ => v,
            };
            Ok(iter_result(i, val, false))
        }
        None => {
            set_internal(obj, "__ci_index", Value::Num(idx as f64));
            set_internal(obj, "__ci_done", Value::Bool(true));
            Ok(iter_result(i, Value::Undefined, true))
        }
    }
}
