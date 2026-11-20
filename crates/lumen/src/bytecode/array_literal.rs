//! Spread literals preserve expression order and observable iterator protocol operations.
use super::*;
use crate::value::Property;

impl Compiler {
    pub(super) fn array_literal(&mut self, elems: &[ArrayElem]) -> CResult {
        if elems.len() > u16::MAX as usize || elems.iter().any(|e| matches!(e, ArrayElem::Hole)) {
            return Err(Bail);
        }
        let mut pending = 0u16;
        let mut has_array = false;
        for elem in elems {
            match elem {
                ArrayElem::Item(value) => {
                    self.expr(value)?;
                    pending += 1;
                }
                ArrayElem::Spread(value) => {
                    if !has_array {
                        self.emit(Op::MakeArray(pending));
                        has_array = true;
                    } else if pending != 0 {
                        self.emit(Op::ArrayLiteralItems(pending));
                    }
                    pending = 0;
                    self.expr(value)?;
                    // Finish this iterator before evaluating the next element expression.
                    self.emit(Op::ArrayLiteralAppend(true));
                }
                ArrayElem::Hole => unreachable!(),
            }
        }
        if !has_array {
            self.emit(Op::MakeArray(pending));
        } else if pending != 0 {
            self.emit(Op::ArrayLiteralItems(pending));
        }
        Ok(())
    }
}

pub(super) fn append(
    i: &mut Interp,
    array: Value,
    value: Value,
    spread: bool,
) -> Result<Value, Abrupt> {
    let values = if spread {
        spread_values(i, &value)?
    } else {
        vec![value]
    };
    Ok(append_items(array, values))
}

/// Collect one spread completely before the next literal element is evaluated.
pub(crate) fn spread_values(i: &mut Interp, value: &Value) -> Result<Vec<Value>, Abrupt> {
    if let Some(values) = super::call_spread::try_dense_literal(i, value)? {
        return Ok(values);
    }
    let (iterator, next) = i.get_iterator_protocol(value)?;
    let mut values = Vec::new();
    while let Some(value) = i.iterator_step(&iterator, &next)? {
        values.push(value);
        if values.len() > crate::interpreter::MAX_ARRAY_OP_LEN {
            return Err(i.throw("RangeError", "iterator produced too many values"));
        }
    }
    Ok(values)
}

pub(super) fn append_items(array: Value, values: Vec<Value>) -> Value {
    let Value::Obj(object) = &array else {
        unreachable!("private literal array")
    };
    {
        let mut object = object.borrow_mut();
        let Value::Num(length) = object.props.get("length").unwrap().value() else {
            unreachable!("private literal length")
        };
        let mut index = length as usize;
        for value in values {
            // Create own properties; inherited indexed setters must never run.
            let property = Property::plain(value);
            let fallback = if index < u32::MAX as usize {
                object.props.try_append_element(index as u32, property)
            } else {
                // 2^32 - 1 and larger keys are not canonical array indices.
                Err(property)
            };
            if let Err(property) = fallback {
                object.props.insert(index.to_string(), property);
            }
            index += 1;
        }
        object
            .props
            .get_mut("length")
            .unwrap()
            .set_value(Value::Num(index as f64));
    }
    array
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Completion, Engine};

    #[test]
    fn literal_appends_preserve_dense_and_sparse_storage() {
        let engine = Engine::new();
        for prefix in [0, 8, 32, 128] {
            let array = engine
                .interp
                .make_array((0..prefix).map(|n| Value::Num(n as f64)).collect());
            let array = append_items(array, vec![Value::Num(42.0), Value::Bool(true)]);
            let Value::Obj(object) = array else {
                unreachable!()
            };
            let object = object.borrow();
            for n in 0..prefix {
                assert!(
                    matches!(object.props.get_index(n).unwrap().value(), Value::Num(v) if v == n as f64)
                );
            }
            assert!(matches!(
                object.props.get_index(prefix).unwrap().value(),
                Value::Num(42.0)
            ));
            assert!(matches!(
                object.props.get_index(prefix + 1).unwrap().value(),
                Value::Bool(true)
            ));
            assert!(
                matches!(object.props.get("length").unwrap().value(), Value::Num(v) if v == (prefix + 2) as f64)
            );
        }

        // A non-tail key must take the generic path without losing the value or filling holes.
        let array = engine.interp.make_array(Vec::new());
        let Value::Obj(object) = &array else {
            unreachable!()
        };
        object
            .borrow_mut()
            .props
            .get_mut("length")
            .unwrap()
            .set_value(Value::Num(5.0));
        let array = append_items(array, vec![Value::Num(42.0)]);
        let Value::Obj(object) = array else {
            unreachable!()
        };
        let object = object.borrow();
        assert!(object.props.get("0").is_none());
        assert!(matches!(
            object.props.get("5").unwrap().value(),
            Value::Num(42.0)
        ));
        assert!(matches!(
            object.props.get("length").unwrap().value(),
            Value::Num(6.0)
        ));
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn literal_append_keeps_noncanonical_large_keys() {
        let engine = Engine::new();
        for index in [
            u32::MAX as usize - 1,
            u32::MAX as usize,
            u32::MAX as usize + 1,
        ] {
            let array = engine.interp.make_array(Vec::new());
            let Value::Obj(object) = &array else {
                unreachable!()
            };
            object
                .borrow_mut()
                .props
                .get_mut("length")
                .unwrap()
                .set_value(Value::Num(index as f64));
            let array = append_items(array, vec![Value::Num(42.0)]);
            let Value::Obj(object) = array else {
                unreachable!()
            };
            let object = object.borrow();
            assert!(matches!(
                object.props.get(&index.to_string()).unwrap().value(),
                Value::Num(42.0)
            ));
            assert!(
                matches!(object.props.get("length").unwrap().value(), Value::Num(v) if v == (index + 1) as f64)
            );
        }
    }

    #[test]
    fn literal_spread_observes_iterator_protocol() {
        let source = include_str!("array_literal/iterator_oracle.js");
        let snapshot = crate::compile_snapshot(source).unwrap();
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            for threshold in [0, 8] {
                for cached in [false, true] {
                    let mut engine = Engine::new();
                    engine.set_tier(tier);
                    engine.set_tier_threshold(threshold);
                    let result = if cached {
                        engine.eval_snapshot(&snapshot, false)
                    } else {
                        engine.eval(source, false)
                    }
                    .unwrap_or_else(|e| panic!("{}", e.message));
                    match result {
                        Completion::Value(v) => {
                            assert_eq!(v, "passed", "{tier:?} {threshold} {cached}")
                        }
                        Completion::Throw { name, message } => {
                            panic!("{tier:?} {threshold} {cached}: {name}: {message}")
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn spread_literals_compile_and_preserve_iterator_order() {
        let source = include_str!("../../tests/fixtures/array-literal-spread.js");
        let body =
            crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
        let (mut accepted, mut refused) = (0, 0);
        for stmt in &body {
            if let Stmt::FuncDecl(f) = stmt {
                let name = f.name.as_deref().unwrap();
                if name.starts_with("compiled_") {
                    assert!(compile(f).is_some(), "{name}");
                    accepted += 1;
                } else if name.starts_with("fallback_") {
                    assert!(compile(f).is_none(), "{name}");
                    refused += 1;
                }
            }
        }
        assert_eq!((accepted, refused), (4, 1));
        let snapshot = crate::compile_snapshot(source).unwrap();
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            for threshold in [0, 8] {
                for cached in [false, true] {
                    let mut engine = Engine::new();
                    engine.set_tier(tier);
                    engine.set_tier_threshold(threshold);
                    let result = if cached {
                        engine.eval_snapshot(&snapshot, false)
                    } else {
                        engine.eval(source, false)
                    }
                    .unwrap_or_else(|e| panic!("{}", e.message));
                    match result {
                        Completion::Value(v) => {
                            assert_eq!(v, "passed", "{tier:?} {threshold} {cached}")
                        }
                        Completion::Throw { name, message } => {
                            panic!("{tier:?}: {name}: {message}")
                        }
                    }
                }
            }
        }
    }
}
