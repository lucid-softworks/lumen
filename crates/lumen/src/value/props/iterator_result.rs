//! Fixed plain-data layout for fresh `{ value, done }` iterator results.
use super::Props;
use crate::value::{Property, Value, GC_STATE};

impl Props {
    pub(crate) fn iterator_result(value: Value, done: bool) -> Self {
        // Primitive placeholders retain neither a realm nor a payload. Keep the Rc keys in
        // the heap context: a pooled worker can later serve an unrelated driver while this
        // heap still owns results created there.
        GC_STATE.with(|current| {
            let state = current.borrow();
            let template = state.iterator_result.get_or_init(|| {
                let mut props = Props::with_capacity(2);
                props.insert("value", Property::plain(Value::Undefined));
                props.insert("done", Property::plain(Value::Bool(false)));
                props
            });
            template.instantiate_plain([value, Value::Bool(done)].into_iter())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Object;
    use std::rc::Rc;

    #[test]
    fn results_preserve_descriptors_order_and_independent_storage() {
        let mut first = Props::iterator_result(Value::Num(7.0), false);
        let second = Props::iterator_result(Value::Undefined, true);
        let mut ordinary = Props::new();
        ordinary.insert("value", Property::plain(Value::Undefined));
        ordinary.insert("done", Property::plain(Value::Bool(true)));
        assert_eq!(second.shape(), ordinary.shape());
        assert_eq!(
            second
                .entries
                .iter()
                .map(|(key, _)| &**key)
                .collect::<Vec<_>>(),
            ["value", "done"]
        );
        for key in ["value", "done"] {
            let property = second.get(key).unwrap();
            assert!(!property.accessor());
            assert!(property.writable() && property.enumerable() && property.configurable());
        }
        first.get_mut("done").unwrap().set_value(Value::Num(3.0));
        first.remove("value");
        assert!(matches!(
            second.get("value").unwrap().value(),
            Value::Undefined
        ));
        assert!(matches!(
            second.get("done").unwrap().value(),
            Value::Bool(true)
        ));
        let third = Props::iterator_result(Value::Num(9.0), false);
        assert_eq!(third.shape(), ordinary.shape());
        assert!(matches!(
            third.get("value").unwrap().value(),
            Value::Num(9.0)
        ));
        assert!(matches!(
            third.get("done").unwrap().value(),
            Value::Bool(false)
        ));
    }

    #[test]
    fn payload_ownership_is_transferred_without_retention_in_template() {
        let child = Object::new(None);
        let initial = Rc::strong_count(&child);
        let result = Props::iterator_result(Value::Obj(child.clone()), false);
        assert_eq!(Rc::strong_count(&child), initial + 1);
        drop(result);
        assert_eq!(Rc::strong_count(&child), initial);
        drop(Props::iterator_result(Value::Undefined, true));
        assert_eq!(Rc::strong_count(&child), initial);
    }

    #[test]
    fn multiple_engines_keep_result_prototypes_in_their_own_realms() {
        let mut first = crate::Engine::new();
        let mut second = crate::Engine::new();
        for round in 0..4 {
            let engine = if round % 2 == 0 {
                &mut first
            } else {
                &mut second
            };
            let result = engine
                .eval(
                    "if (Object.getPrototypeOf([1].values().next()) !== Object.prototype) throw new Error('array result realm');\
                     if (Object.getPrototypeOf((function*(){yield 1;})().next()) !== Object.prototype) throw new Error('generator result realm');\
                     'passed'",
                    false,
                )
                .unwrap();
            match result {
                crate::Completion::Value(value) => assert_eq!(value, "passed"),
                crate::Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
        }
    }
}
