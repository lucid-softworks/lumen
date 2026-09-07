//! Lower this reads without confusing lexical arrows with ordinary call receivers.
use super::{Compiler, Op};

impl Compiler {
    pub(super) fn direct_this_allowed(&self) -> bool {
        !self.lexical_this || self.inline_depth > 0
    }

    pub(super) fn emit_this(&mut self) {
        // An inlined ordinary callee reads its receiver, regardless of the caller's kind.
        if let Some(slot) = self.inline_this {
            if self.inline_depth > 0 {
                self.emit(Op::LoadLocal(slot));
                return;
            }
        }
        if self.direct_this_allowed() {
            self.uses_this = true;
            self.emit(Op::LoadThis);
        } else {
            self.emit(Op::LoadLexicalThis);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::{Expr, Stmt};
    use crate::bytecode::{compile, Op};

    #[test]
    fn arrow_this_reads_compile_without_dynamic_receiver_fusions() {
        let body = crate::parser::parse_script("() => { this.x = 2; return this.x; }", false)
            .unwrap_or_else(|e| panic!("{}", e.message));
        let Stmt::Expr(Expr::Func(func)) = &body[0] else {
            panic!("arrow expected")
        };
        let chunk = compile(func).expect("lexical this must compile");
        assert!(chunk
            .jit_ops()
            .iter()
            .any(|op| matches!(op, Op::LoadLexicalThis)));
        assert!(!chunk.uses_this());
        assert!(!chunk.jit_ops().iter().any(|op| matches!(
            op,
            Op::LoadThis | Op::GetPropThis(..) | Op::SetPropThisDrop(..)
        )));
    }
}
