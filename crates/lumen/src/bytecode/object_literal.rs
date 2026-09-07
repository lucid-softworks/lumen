//! Data-object literals with statically known string keys and shared shape templates.
use super::{Bail, CResult, Compiler, Op};
use crate::ast::{Expr, PropDef, PropKey};
use std::rc::Rc;

impl Compiler {
    pub(super) fn object_literal(&mut self, props: &[PropDef]) -> CResult {
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

fn static_key(key: &PropKey) -> Option<String> {
    match key {
        PropKey::Ident(key) => Some(key.clone()),
        PropKey::Str(key) => Some(key.to_string()),
        PropKey::Computed(expr) => {
            let mut expr = expr;
            while let Expr::Paren(inner) = expr {
                expr = inner;
            }
            match expr {
                Expr::Str(key) => Some(key.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::Stmt;
    use crate::bytecode::{compile, Op};

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
