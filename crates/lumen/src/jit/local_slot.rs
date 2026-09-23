//! Ownership-aware property access through a JIT frame local.

use std::cell::Cell;

use crate::{
    bytecode::IcState,
    interpreter::{Abrupt, Interp},
    value::{PackedValue, Value},
};

enum LocalValue<'a> {
    Borrowed(&'a Value),
    Owned(Value),
}

impl<'a> LocalValue<'a> {
    #[inline(always)]
    unsafe fn from_slot(slots: *const Value, slots_packed: bool, slot: usize) -> Self {
        if slots_packed {
            Self::Owned(unsafe { PackedValue::clone_raw(slots.cast::<u64>().add(slot)) })
        } else {
            Self::Borrowed(unsafe { &*slots.add(slot) })
        }
    }

    #[inline(always)]
    fn as_value(&self) -> &Value {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(value) => value,
        }
    }
}

/// Read a property through a local without manufacturing another owner for a wide slot. Packed
/// slots cannot be borrowed as `Value`, so they retain the established owned expansion. The
/// private `LocalValue` guard prevents the borrowed view from escaping into stack publication or
/// side-exit code that requires a distinct owner.
///
/// # Safety
/// `slots` must point to the initialized local storage described by `slots_packed`, and `slot`
/// must be in bounds. The selected slot must remain initialized and unmodified until the property
/// operation returns. Reentrant execution and collection are permitted while the physical frame
/// remains live: the slot itself is the wide value's owner, and the packed branch creates one.
#[inline(always)]
pub(crate) unsafe fn get_property(
    interp: &mut Interp,
    slots: *const Value,
    slots_packed: bool,
    slot: usize,
    slot_name: &str,
    name: &str,
    cache: &Cell<IcState>,
) -> Result<Value, Abrupt> {
    #[cfg(test)]
    HITS.with(|hits| hits.set(hits.get() + 1));
    let local = unsafe { LocalValue::from_slot(slots, slots_packed, slot) };
    let value = local.as_value();
    if matches!(value, Value::Empty) {
        return Err(interp.throw(
            "ReferenceError",
            format!("cannot access '{slot_name}' before initialization"),
        ));
    }
    interp.get_prop_ic(value, name, cache)
}

#[cfg(test)]
thread_local! {
    static HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bytecode::Tier, value::Object, Completion, Engine};
    use std::{panic::AssertUnwindSafe, rc::Rc};

    #[test]
    fn wide_local_is_borrowed_without_an_owner_round_trip() {
        let object = Object::new(None);
        let slots = [Value::Obj(object.clone())];
        let owners = Rc::strong_count(&object);
        let local = unsafe { LocalValue::from_slot(slots.as_ptr(), false, 0) };
        assert!(matches!(local.as_value(), Value::Obj(value) if Rc::ptr_eq(value, &object)));
        assert_eq!(Rc::strong_count(&object), owners);
        drop(local);
        assert_eq!(Rc::strong_count(&object), owners);
    }

    #[test]
    fn packed_local_owner_is_scoped_across_unwinding() {
        let object = Object::new(None);
        let packed = PackedValue::pack(Value::Obj(object.clone()));
        let owners = Rc::strong_count(&object);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| unsafe {
            let local = LocalValue::from_slot((&raw const packed).cast::<Value>(), true, 0);
            assert!(matches!(local.as_value(), Value::Obj(value) if Rc::ptr_eq(value, &object)));
            assert_eq!(Rc::strong_count(&object), owners + 1);
            panic!("exercise callback unwind");
        }));
        assert!(result.is_err());
        assert_eq!(Rc::strong_count(&object), owners);
    }

    #[test]
    fn local_property_get_preserves_reentrant_gc_and_thrown_values() {
        let mut engine = Engine::new();
        engine.set_tier(Tier::Jit);
        engine.set_tier_threshold(0);
        HITS.with(|hits| hits.set(0));
        let source = r#"
            function assert(value){if(!value)throw new Error('assertion failed');}
            function read(receiver){return receiver.value;}
            let calls=0;
            let survivor={token:0};
            const receiver={};
            Object.defineProperty(receiver,'value',{get:function(){
                calls++;
                const held=survivor;
                survivor=null;
                $262.gc();
                survivor={token:held.token+1};
                return held.token;
            }});
            for(let i=0;i<600;i++)assert(read(receiver)===i);
            const thrown={kept:41};
            const thrower={};
            Object.defineProperty(thrower,'value',{get:function(){$262.gc();throw thrown;}});
            try{read(thrower);assert(false);}catch(error){assert(error===thrown&&error.kept===41);}
            assert(calls===600&&survivor.token===600);
            'passed'
        "#;
        match engine.eval(source, false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "passed"),
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
        assert!(HITS.with(|hits| hits.get()) > 0, "helper was not exercised");
    }
}
