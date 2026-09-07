//! Owned collection insertion shared by native methods and consuming JIT helpers.
use super::canonicalize_map_key;
use crate::builtins::{arg, collection_data::CollectionData};
use crate::interpreter::Interp;
use crate::value::{NativeFn, Value};
use std::rc::Rc;

pub(crate) const MAP_SET: u8 = 16;
pub(crate) const SET_ADD: u8 = 17;

pub(super) fn intrinsic(native: usize) -> u8 {
    for (method, id) in [(map_set as NativeFn, MAP_SET), (set_add, SET_ADD)] {
        if native == method as *const () as usize {
            return id;
        }
    }
    0
}

fn data<'a>(i: &'a mut Interp, this: &Value, kind: &str) -> Option<&'a mut CollectionData> {
    let object = this.as_obj()?;
    let marker = object.borrow().props.get("__ck").map(|p| p.value());
    if !matches!(marker, Some(Value::Str(ref s)) if &**s == kind) {
        return None;
    }
    i.map_data.get_mut(&(Rc::as_ptr(object) as usize))
}

pub(crate) fn map_set_owned(
    i: &mut Interp,
    this: &Value,
    key: Value,
    value: Value,
) -> Result<(), Value> {
    let Some(data) = data(i, this, "Map") else {
        return Err(i.make_error("TypeError", "method called on an incompatible receiver"));
    };
    data.insert(key, value);
    Ok(())
}

pub(crate) fn set_add_owned(i: &mut Interp, this: &Value, key: Value) -> Result<(), Value> {
    let Some(data) = data(i, this, "Set") else {
        return Err(i.make_error("TypeError", "method called on an incompatible receiver"));
    };
    // Set iteration exposes either side of the pair. Both must contain canonical +0.
    let key = canonicalize_map_key(key);
    data.insert(key.clone(), key);
    Ok(())
}

pub(super) fn map_set(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    map_set_owned(i, &this, arg(args, 0), arg(args, 1))?;
    Ok(this)
}

pub(super) fn set_add(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    set_add_owned(i, &this, arg(args, 0))?;
    Ok(this)
}
