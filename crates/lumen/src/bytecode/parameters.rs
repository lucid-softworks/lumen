//! Default-parameter eligibility and initialization in local or captured bindings.
use super::{CResult, Compiler, Op};
use crate::ast::{ArrayElem, Expr, Function, HoistOp, PropDef, PropKey};

impl Compiler {
    pub(super) fn parameter_default(
        &mut self,
        slot: u16,
        expr: &Expr,
        cap: Option<u32>,
    ) -> CResult {
        self.emit(Op::LoadLocal(slot));
        self.emit(Op::Undef);
        self.emit(Op::StrictEq);
        let skip = self.emit(Op::JumpIfFalse(0));
        self.expr(expr)?;
        self.emit(match cap {
            Some(name) => Op::StoreCap(name),
            None => Op::StoreLocal(slot),
        });
        self.patch(skip);
        Ok(())
    }
}

pub(super) fn captured_default_safe(func: &Function, name: &str, expr: &Expr) -> bool {
    // No parameter-environment closures, reads, coercions or callbacks. Empty containers
    // still allocate freshly through the normal expression emitter on every defaulted call.
    if !literal_default(expr) {
        return false;
    }
    // Hoisted function bindings are seeded into the activation before bytecode runs. Do not
    // overwrite one with a default; supporting that case needs separate parameter/body scopes.
    !crate::interpreter::collect_hoist_ops(&func.body, func.is_strict, &[])
        .iter()
        .any(|op| matches!(op, HoistOp::Fn(n, _) | HoistOp::AnnexB(n, _) if n == name))
}

fn literal_default(expr: &Expr) -> bool {
    match expr {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined => true,
        Expr::Array(items) => items.is_empty(),
        Expr::Object(props) => props.is_empty(),
        Expr::Paren(inner) => literal_default(inner),
        _ => false,
    }
}

/// Whether a parameter default is in the compiler's lowerable subset: no reference to any
/// *banned* name (this parameter itself or a later one — the spec's param-scope TDZ would throw
/// where slots would read a seeded `undefined`), and no nested function/class (whose capture
/// analysis of a *parameter expression* scope the slot model doesn't carry). Whitelist
/// recursion: unknown constructs answer false (the function stays on the tree-walker).
pub(super) fn default_expr_safe(e: &Expr, banned: &std::collections::HashSet<&str>) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::This
        | Expr::Regex { .. } => true,
        Expr::Ident(n) => !banned.contains(n.as_str()),
        Expr::Paren(x) | Expr::ToStr(x) | Expr::Unary { arg: x, .. } => {
            default_expr_safe(x, banned)
        }
        Expr::Update { arg, .. } => default_expr_safe(arg, banned),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            default_expr_safe(left, banned) && default_expr_safe(right, banned)
        }
        Expr::Cond { test, cons, alt } => {
            default_expr_safe(test, banned)
                && default_expr_safe(cons, banned)
                && default_expr_safe(alt, banned)
        }
        Expr::Member { obj, .. } => default_expr_safe(obj, banned),
        Expr::Index { obj, index, .. } => {
            default_expr_safe(obj, banned) && default_expr_safe(index, banned)
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            default_expr_safe(callee, banned)
                && args.iter().all(|a| match a {
                    ArrayElem::Item(x) | ArrayElem::Spread(x) => default_expr_safe(x, banned),
                    ArrayElem::Hole => true,
                })
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            ArrayElem::Item(x) | ArrayElem::Spread(x) => default_expr_safe(x, banned),
            ArrayElem::Hole => true,
        }),
        Expr::Object(props) => props.iter().all(|p| match p {
            PropDef::KeyValue { key, value } => {
                !matches!(key, PropKey::Computed(_)) && default_expr_safe(value, banned)
            }
            _ => false,
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::Stmt;
    use crate::bytecode::{compile, Op};

    fn compiled(source: &str) -> Option<std::rc::Rc<crate::bytecode::Chunk>> {
        let stmts =
            crate::parser::parse_script(source, false).unwrap_or_else(|e| panic!("{}", e.message));
        let Stmt::FuncDecl(f) = &stmts[0] else {
            panic!("function expected")
        };
        compile(f)
    }

    #[test]
    fn captured_literal_default_initializes_its_binding() {
        let chunk = compiled("function f(x={}) { return () => x; }")
            .expect("literal captured default should compile");
        assert!(chunk
            .jit_ops()
            .iter()
            .any(|op| matches!(op, Op::StoreCap(_))));
    }

    #[test]
    fn defaults_with_effects_or_hoist_conflicts_stay_in_the_oracle() {
        assert!(compiled("function f(x=make()) { return () => x; }").is_none());
        assert!(compiled("function f(x={}) { function x() {} return () => x; }").is_none());
        assert!(compiled("function f(x=()=>1) { return () => x; }").is_none());
    }
}
