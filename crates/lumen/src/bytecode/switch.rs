//! Switch lowering with a single lexical scope shared by all case clauses.
use super::{CResult, Compiler, LoopCtx, Op};
use crate::ast::{Expr, SwitchCase};

impl Compiler {
    pub(super) fn switch_statement(&mut self, disc: &Expr, cases: &[SwitchCase]) -> CResult {
        // The discriminant is evaluated outside the case block's lexical scope.
        self.expr(disc)?;
        let tmp = self.fresh_slot("%switch%");
        self.emit(Op::StoreLocal(tmp));
        self.scopes.push(Vec::new());
        let result = self.switch_scope(tmp, cases);
        self.scopes.pop();
        result
    }

    fn switch_scope(&mut self, tmp: u16, cases: &[SwitchCase]) -> CResult {
        // Every case's lexical binding exists in TDZ before any case expression runs.
        // This also resets slots whenever control re-enters a switch inside a loop.
        for case in cases {
            self.declare_block_lexicals(&case.body)?;
        }
        let mut body_jumps = Vec::new();
        for (ci, case) in cases.iter().enumerate() {
            if let Some(test) = &case.test {
                self.emit(Op::LoadLocal(tmp));
                self.expr(test)?;
                self.emit(Op::StrictEq);
                let jf = self.emit(Op::JumpIfFalse(0));
                let jb = self.emit(Op::Jump(0));
                body_jumps.push((ci, jb));
                self.patch(jf);
            }
        }
        let jdefault = self.emit(Op::Jump(0));
        self.loops.push(LoopCtx {
            labels: std::mem::take(&mut self.pending_labels),
            is_switch: true,
            ..LoopCtx::default()
        });
        let starts = self.switch_bodies(cases);
        let ctx = self.loops.pop().unwrap();
        let starts = starts?;
        for (ci, at) in body_jumps {
            self.ops[at] = Op::Jump(starts[ci] as u32);
        }
        match cases.iter().position(|c| c.test.is_none()) {
            Some(ci) => self.ops[jdefault] = Op::Jump(starts[ci] as u32),
            None => self.patch(jdefault),
        }
        for at in ctx.breaks {
            self.patch(at);
        }
        Ok(())
    }

    fn switch_bodies(&mut self, cases: &[SwitchCase]) -> Result<Vec<usize>, super::Bail> {
        // Contiguous bodies preserve fallthrough, including default in the middle.
        let mut starts = Vec::with_capacity(cases.len());
        for case in cases {
            starts.push(self.ops.len());
            for stmt in &case.body {
                self.stmt(stmt)?;
            }
        }
        Ok(starts)
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::Stmt;
    use crate::bytecode::{compile, Op};

    #[test]
    fn case_lexicals_compile_and_initialize_before_case_tests() {
        let body = crate::parser::parse_script(
            "function f(x) { switch(x) { case 0: let y=3; case 1: return y; default: return 0; } }",
            false,
        )
        .unwrap_or_else(|e| panic!("{}", e.message));
        let Stmt::FuncDecl(func) = &body[0] else {
            panic!("function expected")
        };
        let chunk = compile(func).expect("case lexical should compile");
        let ops = chunk.jit_ops();
        let tdz = ops.iter().position(|op| matches!(op, Op::Tdz(_))).unwrap();
        let test = ops
            .iter()
            .position(|op| matches!(op, Op::StrictEq))
            .unwrap();
        assert!(tdz < test);
    }
}
