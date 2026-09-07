//! Compiled for-in with uncaptured lexical heads and shared oracle enumeration.
use super::{Bail, CResult, Compiler, LoopCtx, Op};
use crate::ast::{DeclKind, Expr, Stmt};
use crate::interpreter::{Abrupt, Interp};
use crate::value::Value;

impl Compiler {
    pub(super) fn for_in_statement(
        &mut self,
        kind: DeclKind,
        name: &str,
        right: &Expr,
        body: &Stmt,
    ) -> CResult {
        // Captured loop bindings require a fresh environment each iteration.
        if self.env_names.contains_key(name) && !self.homed_lets.contains(name) {
            return Err(Bail);
        }
        let labels = std::mem::take(&mut self.pending_labels);
        self.scopes.push(Vec::new());
        let result = self.for_in_scope(kind, name, right, body, labels);
        self.scopes.pop();
        result
    }

    fn for_in_scope(
        &mut self,
        kind: DeclKind,
        name: &str,
        right: &Expr,
        body: &Stmt,
        labels: Vec<String>,
    ) -> CResult {
        let binding = self.fresh_slot(name);
        self.scope_bind(name, binding, kind == DeclKind::Const);
        self.tdz_slots.insert(binding);
        self.emit(Op::Tdz(binding)); // Head binding shadows outer names while RHS evaluates.
        self.expr(right)?;
        let base = self.fresh_slot("%for-in-base%");
        let keys = self.fresh_slot("%for-in-keys%");
        let cursor = self.fresh_slot("%for-in-cursor%");
        self.emit(Op::Dup);
        self.emit(Op::StoreLocal(base));
        self.emit(Op::ForInKeys);
        self.emit(Op::StoreLocal(keys));
        let zero = self.const_idx(Value::Num(0.0));
        self.emit(Op::Const(zero));
        self.emit(Op::StoreLocal(cursor));
        let head = self.ops.len();
        self.emit(Op::ForInStepL(base, keys, cursor));
        let exit = self.emit(Op::JumpIfFalse(0));
        self.emit(Op::StoreLocal(binding));
        self.loops.push(LoopCtx {
            labels,
            entry_try_depth: self.try_depth,
            ..LoopCtx::default()
        });
        let result = self.stmt(body);
        let ctx = self.loops.pop().unwrap();
        result?;
        for at in ctx.continues {
            self.ops[at] = Op::Jump(head as u32);
        }
        self.emit(Op::Jump(head as u32));
        self.patch(exit);
        self.emit(Op::Pop); // Exhaustion's undefined placeholder.
        for at in ctx.breaks {
            self.patch(at);
        }
        Ok(())
    }
}

/// The snapshot is an internal array, never exposed to JS. Read its own storage directly so
/// array-prototype changes cannot alter enumeration. Release the borrow before proxy callbacks.
pub(super) fn step(
    i: &mut Interp,
    slots: &mut [Value],
    base: u16,
    keys: u16,
    cursor: u16,
) -> Result<Option<Value>, Abrupt> {
    let base = slots[base as usize].clone();
    let keys = slots[keys as usize]
        .as_obj()
        .expect("private key snapshot")
        .clone();
    let Value::Num(mut index) = slots[cursor as usize] else {
        unreachable!("private cursor")
    };
    loop {
        let key = keys
            .borrow()
            .props
            .get_index(index as u32)
            .map(|p| p.value());
        let Some(key) = key else { return Ok(None) };
        index += 1.0;
        slots[cursor as usize] = Value::Num(index);
        let Value::Str(name) = &key else {
            unreachable!("enumeration key is a string")
        };
        if matches!(base, Value::Obj(_)) && !i.js_has_property(&base, name)? {
            continue;
        }
        return Ok(Some(key));
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::Stmt;
    use crate::bytecode::{compile, Op};

    #[test]
    fn lexical_enumeration_compiles_without_iterator_protocol() {
        let body = crate::parser::parse_script(
            "function f(obj) { let s=''; for(const k in obj) s+=k; return s; }",
            false,
        )
        .unwrap_or_else(|e| panic!("{}", e.message));
        let Stmt::FuncDecl(f) = &body[0] else {
            panic!("function expected")
        };
        let chunk = compile(f).expect("for-in should compile");
        assert!(chunk
            .jit_ops()
            .iter()
            .any(|op| matches!(op, Op::ForInStepL(..))));
        let cfg = crate::jit_ir::Cfg::build(&chunk).unwrap();
        let header = cfg.blocks()[cfg.loops()[0].header.0 as usize].start;
        assert!(matches!(
            crate::jit_ir::RegionIr::build_loop(&chunk, &cfg, header),
            Err(crate::jit_ir::IrError::UnmodeledLocalEffect { .. })
        ));
    }
}
