//! Data-object literals: shared templates for static keys, own-property writes for dynamic keys.
use super::{Bail, CResult, Compiler, Op};
use crate::ast::{Expr, PropDef, PropKey};
use std::rc::Rc;

impl Compiler {
    pub(super) fn object_literal(&mut self, props: &[PropDef]) -> CResult {
        if props.iter().any(|prop| {
            matches!(prop,
            PropDef::KeyValue { key: PropKey::Computed(expr), .. }
                if !matches!(unparen(expr), Expr::Str(_)))
        }) {
            return self.dynamic_object_literal(props);
        }
        let mut keys = Vec::with_capacity(props.len());
        let count = u16::try_from(props.len()).map_err(|_| Bail)?;
        for prop in props {
            let PropDef::KeyValue { key, value } = prop else {
                return Err(Bail);
            };
            let name = static_key(key).ok_or(Bail)?;
            if name.starts_with('#')
                || (name == "__proto__" && !matches!(key, PropKey::Computed(_)))
            {
                return Err(Bail);
            }
            // Computed constant strings perform no coercion or user code. Their value and
            // anonymous-function naming are exactly the same as a direct string key.
            self.named_expr(value, &name)?;
            keys.push(name);
        }
        // Values may add their own names, so append the contiguous key range afterwards.
        let start = self.names.len() as u32;
        self.names
            .extend(keys.iter().map(|key| Rc::from(key.as_str())));
        let template = self.object_template(&keys);
        self.emit(Op::MakeObject(start, count, template));
        Ok(())
    }

    fn dynamic_object_literal(&mut self, props: &[PropDef]) -> CResult {
        self.emit(Op::MakeObject(0, 0, u32::MAX));
        for prop in props {
            let PropDef::KeyValue { key, value } = prop else {
                return Err(Bail);
            };
            // Keep the literal itself below each temporary property Reference. Key coercion
            // happens before value evaluation; the definition must bypass prototype setters.
            self.emit(Op::Dup);
            if let Some(name) = static_key(key) {
                if name.starts_with('#')
                    || (name == "__proto__" && !matches!(key, PropKey::Computed(_)))
                {
                    return Err(Bail);
                }
                let key = self.const_idx(crate::value::Value::from_string(name));
                self.emit(Op::Const(key));
            } else if let PropKey::Computed(expr) = key {
                self.expr(expr)?;
            } else {
                return Err(Bail);
            }
            self.emit(Op::ToPropKey);
            self.expr(value)?;
            self.emit(Op::DefineLiteralData(crate::eval::is_anonymous_fn(value)));
        }
        Ok(())
    }

    fn object_template(&mut self, keys: &[String]) -> u32 {
        // Duplicate keys need insertion semantics instead of one template slot per property.
        let mut sorted: Vec<_> = keys.iter().collect();
        sorted.sort();
        if !keys.is_empty() && sorted.windows(2).all(|pair| pair[0] != pair[1]) {
            self.obj_maps += 1;
            self.obj_maps - 1
        } else {
            u32::MAX
        }
    }
}

fn unparen(mut expr: &Expr) -> &Expr {
    while let Expr::Paren(inner) = expr {
        expr = inner;
    }
    expr
}

fn static_key(key: &PropKey) -> Option<String> {
    match key {
        PropKey::Ident(key) => Some(key.clone()),
        PropKey::Str(key) => Some(key.to_string()),
        PropKey::Computed(expr) => match unparen(expr) {
            Expr::Str(key) => Some(key.to_string()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::Stmt;
    use crate::bytecode::{compile, Op};

    #[test]
    fn unsupported_literals_in_nested_closures_keep_outer_compilation() {
        for literal in [
            "{...x}",
            "{get value(){return x.value}}",
            "{set value(v){x.value=v}}",
            "{method(){return x.value}}",
            "{__proto__:x}",
        ] {
            for (body, expected) in [
                (format!("return ({literal});"), false),
                (format!("return () => ({literal});"), true),
                (format!("return function(){{return ({literal})}};"), true),
            ] {
                let source = format!("function outer(x){{{body}}}");
                let stmts = crate::parser::parse_script(&source, false)
                    .unwrap_or_else(|e| panic!("{}", e.message));
                let Stmt::FuncDecl(f) = &stmts[0] else {
                    panic!("function expected")
                };
                assert_eq!(compile(f).is_some(), expected, "{source}");
            }
        }
    }

    #[test]
    fn interpreted_literal_closures_preserve_captured_values_and_accessors() {
        for tier in [
            super::super::Tier::Interp,
            super::super::Tier::Bytecode,
            super::super::Tier::Jit,
        ] {
            let mut engine = crate::Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let result = engine
                .eval(
                    r#"
                function outer(x) {
                    return () => ({...x, get doubled(){return x.value*2},
                        set changed(v){x.value=v}, method(){return x.value}});
                }
                const source={value:3}; const closure=outer(source);
                const first=closure(); first.changed=7;
                const second=closure();
                if(first.value!==3 || first.doubled!==14 || first.method()!==7 ||
                   second.value!==7 || second===first) throw 'capture';
                'passed'
            "#,
                    false,
                )
                .unwrap_or_else(|e| panic!("{}", e.message));
            assert!(matches!(result, crate::Completion::Value(v) if v == "passed"));
        }
    }

    #[test]
    fn computed_constant_keys_use_the_object_template_path() {
        let stmts = crate::parser::parse_script(
            "function f(v) { return {['x']:v, [('y')]:v, ['__proto__']:v}; }",
            false,
        )
        .unwrap_or_else(|e| panic!("{}", e.message));
        let Stmt::FuncDecl(f) = &stmts[0] else {
            panic!("function expected")
        };
        let chunk = compile(f).expect("static keys should compile");
        assert!(chunk
            .jit_ops()
            .iter()
            .any(|op| matches!(op, Op::MakeObject(_,3,site) if *site != u32::MAX)));
    }
}
