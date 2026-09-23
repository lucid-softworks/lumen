//! Checked numeric element access for side-table-backed TypedArrays.

use super::Interp;
use crate::value::{Gc, Value};
use std::rc::Rc;

impl Interp {
    /// Read a numeric TypedArray key without materializing its property-key string.
    ///
    /// Returning `Some(undefined)` for a non-index numeric key is intentional: canonical
    /// numeric keys that are not valid integer indices are handled by TypedArray's integer-
    /// indexed exotic object and must not fall through to ordinary properties or prototypes.
    /// `ta_read` rechecks the live view length and backing buffer, so detached and resized views
    /// retain their existing behavior.
    pub(super) fn fast_typed_array_get(&self, object: &Gc, key: f64) -> Option<Value> {
        let info = self
            .typed_arrays
            .get(&(Rc::as_ptr(object) as usize))
            .copied()?;
        let index = numeric_index(key).unwrap_or(usize::MAX);
        Some(self.ta_read(&info, index))
    }
}

fn numeric_index(key: f64) -> Option<usize> {
    (key.is_finite() && key >= 0.0 && key.trunc() == key).then_some(key as usize)
}

#[cfg(test)]
mod tests {
    use super::numeric_index;
    use crate::{bytecode::Tier, value::Value, Completion, Engine};

    #[test]
    fn numeric_keys_match_typed_array_index_boundaries() {
        assert_eq!(numeric_index(0.0), Some(0));
        assert_eq!(numeric_index(-0.0), Some(0));
        assert_eq!(numeric_index(1.0), Some(1));
        assert_eq!(numeric_index(1.5), None);
        assert_eq!(numeric_index(-1.0), None);
        assert_eq!(numeric_index(f64::NAN), None);
        assert_eq!(numeric_index(f64::INFINITY), None);
    }

    #[test]
    fn typed_reads_recheck_detachment_and_resizing() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            assert!(matches!(
                engine.eval(
                    "var ta=new Uint8Array(2);ta[0]=7;ta[1]=9;var fixed=new Uint8Array(new ArrayBuffer(4,{maxByteLength:8}),0,4);fixed[0]=3;",
                    false
                ),
                Ok(Completion::Value(_))
            ));
            let ta = engine
                .interp
                .global
                .borrow()
                .props
                .get("ta")
                .expect("ta global")
                .value();
            let fixed = engine
                .interp
                .global
                .borrow()
                .props
                .get("fixed")
                .expect("fixed global")
                .value();
            let ta = ta.as_obj().expect("typed array object");
            let fixed = fixed.as_obj().expect("fixed typed array object");
            assert!(matches!(
                engine.interp.fast_get_elem(&ta, 0.0),
                Some(Value::Num(n)) if n == 7.0
            ));
            assert!(matches!(
                engine.interp.fast_get_elem(&ta, 1.5),
                Some(Value::Undefined)
            ));
            assert!(matches!(
                engine.interp.fast_get_elem(&ta, 2.0),
                Some(Value::Undefined)
            ));
            assert!(matches!(
                engine.eval("$262.detachArrayBuffer(ta.buffer);ta[0]", false),
                Ok(Completion::Value(_))
            ));
            assert!(matches!(
                engine.interp.fast_get_elem(&ta, 0.0),
                Some(Value::Undefined)
            ));
            assert!(matches!(
                engine.eval("fixed.buffer.resize(2);fixed[0]", false),
                Ok(Completion::Value(_))
            ));
            assert!(matches!(
                engine.interp.fast_get_elem(&fixed, 0.0),
                Some(Value::Undefined)
            ));
        }
    }
}
