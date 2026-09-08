//! Lower symbolic reads/arithmetic, then publish the single prepared numeric effect.
use super::{names, values, Cache};
use crate::{
    jit::{asm::Asm, C_GE, C_GT, C_LS, C_MI, C_NE, C_VS},
    jit_ir::iterator_entry::{Binary, Candidate, Expr, Literal, Obligation},
    value::JitLayout,
};
pub(super) struct Context<'a> {
    pub a: &'a mut Asm,
    pub layout: &'a JitLayout,
    pub plan: &'a Candidate,
    pub caches: &'a [Cache],
    pub guards: &'a [Option<names::Guard>],
    pub fail: usize,
    pub prepared: bool,
}
impl Context<'_> {
    fn cache(&self, index: u32) -> Option<usize> {
        Some(self.caches.iter().find(|c| c.index == index)?.ways.as_ptr() as usize)
    }
    fn pending(&self) -> u32 {
        values::offset(self.plan.expressions.len())
    }
    pub(super) fn expression(&mut self, id: usize, expr: &Expr) -> Option<()> {
        let forward = self.plan.obligations.iter().any(
            |o| matches!(o,Obligation::ForwardStoredEntryOrProveDisjoint{read,..} if *read==id),
        );
        if forward && !self.prepared {
            self.prepare()?;
        }
        match expr {
            Expr::Constant(literal) => self.literal(id, literal),
            Expr::This => values::wide(self.a, id, 22, self.fail),
            Expr::Name { .. } => {
                names::emit(self.a, self.guards.get(id)?.as_ref()?, 20, self.fail);
                values::wide(self.a, id, 14, self.fail);
            }
            Expr::Own {
                receiver,
                name,
                cache,
            } => {
                let cache = self.cache(*cache)?;
                self.own(*receiver, name, cache, false);
                if forward {
                    let live = self.a.new_label();
                    let done = self.a.new_label();
                    self.a.ldr_imm(10, 23, self.pending());
                    self.a.cmp_reg_x(15, 10);
                    self.a.b_cond(C_NE, live);
                    self.a.ldr_imm(13, 23, self.pending() + 8);
                    values::store(self.a, id, 4, 13);
                    self.a.b(done);
                    self.a.bind(live);
                    self.a.ldur(13, 15, self.layout.entry_value as i32);
                    values::packed(self.a, id, self.fail);
                    self.a.bind(done);
                } else {
                    self.a.ldur(13, 15, self.layout.entry_value as i32);
                    values::packed(self.a, id, self.fail);
                }
            }
            Expr::Dense { receiver, index } => {
                let length = self.plan.expressions.iter().find_map(|e| {
                    if let Expr::Own { name, cache, .. } = e {
                        (name == "length").then_some(*cache)
                    } else {
                        None
                    }
                })?;
                let cache = self.cache(length)?;
                values::object(self.a, *receiver, 0, self.fail);
                values::number(self.a, *index, 16, self.fail);
                super::super::element::emit(self.a, self.layout, cache, self.fail);
                values::packed(self.a, id, self.fail);
            }
            Expr::Binary { op, left, right } => self.binary(id, *op, *left, *right),
        }
        for branch in self.plan.branches.iter().filter(|b| b.condition == id) {
            // Initial slice accepts comparison Booleans, never guesses object truthiness.
            self.a.ldrb_imm(9, 23, values::offset(id));
            self.a.cmp_imm_w(9, 3);
            self.a.b_cond(C_NE, self.fail);
            self.a.ldrb_imm(9, 23, values::offset(id) + 1);
            self.a.cmp_imm_w(9, u32::from(branch.required_truthy));
            self.a.b_cond(C_NE, self.fail);
        }
        Some(())
    }
    fn literal(&mut self, id: usize, l: &Literal) {
        let (word, payload) = match l {
            Literal::Number(bits) => (4, *bits),
            Literal::Boolean(b) => (3 | ((*b as u64) << 8), 0),
            Literal::Null => (2, 0),
            Literal::Undefined => (0, 0),
        };
        self.a.mov_imm64(12, word);
        self.a.mov_imm64(13, payload);
        self.a.str_imm(12, 23, values::offset(id));
        self.a.str_imm(13, 23, values::offset(id) + 8);
    }
    fn binary(&mut self, id: usize, op: Binary, left: usize, right: usize) {
        values::number(self.a, left, 16, self.fail);
        values::number(self.a, right, 17, self.fail);
        let arithmetic = match op {
            Binary::Add => Some(0),
            Binary::Sub => Some(1),
            Binary::Mul => Some(2),
            Binary::Div => Some(3),
            _ => None,
        };
        if let Some(op) = arithmetic {
            self.a.f_arith(op, 16, 16, 17);
            self.a.fmov_x_d(13, 16);
            values::store(self.a, id, 4, 13);
        } else {
            self.a.fcmp(16, 17);
            let cond = match op {
                Binary::Lt => C_MI,
                Binary::Gt => C_GT,
                Binary::Le => C_LS,
                Binary::Ge => C_GE,
                _ => unreachable!(),
            };
            self.a.cset_w(13, cond);
            self.a.lsl_imm(13, 13, 8);
            self.a.movz(12, 3, 0);
            self.a.logic_x(1, 12, 12, 13);
            self.a.str_imm(12, 23, values::offset(id));
            self.a.str_imm(31, 23, values::offset(id) + 8);
        }
    }
    fn own(&mut self, receiver: usize, name: &str, cache: usize, write: bool) {
        values::object(self.a, receiver, 11, self.fail);
        self.a.ldr_imm(9, 11, self.layout.gc_data_off as u32);
        self.a.cmp_imm_x(9, 0);
        self.a.b_cond(if write { C_NE } else { C_MI }, self.fail);
        if write {
            self.a.add_imm(10, 11, self.layout.obj_from_rc as u32);
            self.a.ldrb_imm(9, 10, self.layout.obj_exotic as u32);
            self.a.cmp_imm_w(9, self.layout.exotic_none_tag as u32);
            self.a.b_cond(C_NE, self.fail);
        }
        crate::jit::property_probe::own_entry(self.a, self.layout, cache, name, self.fail);
    }
    fn prepare(&mut self) -> Option<()> {
        let store = self.plan.store.as_ref()?;
        let cache = self.cache(store.cache)?;
        self.own(store.receiver, &store.name, cache, true);
        self.a.ldrb_imm(9, 15, self.layout.entry_writable as u32);
        self.a.movz(10, crate::value::PROP_WRITABLE as u32, 0);
        self.a.logic_x(0, 9, 9, 10);
        self.a.cbz(9, false, self.fail);
        crate::jit::emit_region_packed_number(
            self.a,
            15,
            self.layout.entry_value as i32,
            16,
            self.fail,
        );
        values::number(self.a, store.value, 16, self.fail);
        self.a.str_imm(15, 23, self.pending());
        self.a.str_d_imm(16, 23, self.pending() + 8);
        self.prepared = true;
        Some(())
    }
    pub(super) fn finish(&mut self) -> Option<()> {
        if !matches!(
            self.plan.expressions.get(self.plan.done),
            Some(Expr::Constant(Literal::Boolean(false)))
        ) {
            return None;
        }
        if self.plan.store.is_some() && !self.prepared {
            self.prepare()?;
        }
        values::output(self.a, self.layout, self.plan.value, self.fail);
        if self.prepared {
            self.a.ldr_imm(15, 23, self.pending());
            self.a.ldr_d_imm(16, 23, self.pending() + 8);
            self.a.fcmp(16, 16);
            let nan = self.a.new_label();
            let commit = self.a.new_label();
            self.a.b_cond(C_VS, nan);
            self.a.fmov_x_d(13, 16);
            self.a.b(commit);
            self.a.bind(nan);
            self.a.mov_imm64(13, f64::NAN.to_bits());
            self.a.bind(commit);
            self.a.stur(13, 15, self.layout.entry_value as i32);
        }
        Some(())
    }
}
