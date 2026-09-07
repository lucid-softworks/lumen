//! Borrow numeric local homes and allocate temporaries only for live expression values.
mod fixed;
use super::plan::{Plan, Step};
use crate::jit::{asm::Asm, UpdKind};

pub(super) struct Values {
    stack: Vec<u32>,
    allocated: bool,
}

impl Values {
    pub(super) fn new(allocated: bool) -> Self {
        Self {
            stack: Vec::new(),
            allocated,
        }
    }

    pub(super) fn snapshot(&self) -> Vec<u32> {
        self.stack.clone()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    fn pop(&mut self) -> u32 {
        self.stack.pop().expect("verified numeric operand")
    }

    fn temporary(&self, extra: &[u32]) -> u32 {
        (24..32)
            .find(|r| !self.stack.contains(r) && !extra.contains(r))
            .expect("numeric operand register capacity")
    }

    // Preserve all outstanding reads of a home before assigning a different value to it.
    // Excluded inputs have been popped but are still needed by the instruction being emitted.
    fn preserve(&mut self, a: &mut Asm, home: u32, inputs: &[u32]) {
        if self.stack.contains(&home) {
            let saved = self.temporary(inputs);
            a.fmov_d_d(saved, home);
            for register in &mut self.stack {
                if *register == home {
                    *register = saved;
                }
            }
        }
    }

    fn destination(&mut self, a: &mut Asm, plan: &Plan, next: Option<Step>, inputs: &[u32]) -> u32 {
        if let Some(Step::Store(slot)) = next {
            // Numeric local stores cannot call, throw or drop a reference. No guard/edge exists
            // between this definition and its immediately following store, so coalesce them.
            let home = plan.home(slot);
            self.preserve(a, home, inputs);
            home
        } else {
            // Popped temporary inputs may be reused: A64 reads both operands before writing.
            self.temporary(&[])
        }
    }

    pub(super) fn comparison(&mut self) -> (u32, u32) {
        let rhs = self.pop();
        let lhs = self.pop();
        (lhs, rhs)
    }

    pub(super) fn read(
        &mut self,
        a: &mut Asm,
        plan: &Plan,
        slot: u16,
        fail: usize,
        next: Option<Step>,
    ) {
        let key = self.pop();
        let result = if self.allocated {
            self.destination(a, plan, next, &[key])
        } else {
            key
        };
        // The guards precede the result write, including when it is coalesced into a local.
        super::arrays::read(a, plan, slot, key, result, fail);
        self.stack.push(result);
    }

    pub(super) fn step(&mut self, a: &mut Asm, plan: &Plan, step: Step, next: Option<Step>) {
        if !self.allocated {
            let mut depth = self.stack.len() as u32;
            fixed::step(a, plan, step, &mut depth);
            self.stack = (24..24 + depth).collect();
            return;
        }
        match step {
            Step::Load(slot) => self.stack.push(plan.home(slot)),
            Step::Input(index) => self.stack.push(super::inputs::register(index)),
            Step::Constant(bits) => {
                let result = self.destination(a, plan, next, &[]);
                a.mov_imm64(9, bits);
                a.fmov_d_x(result, 9);
                self.stack.push(result);
            }
            Step::Store(slot) => {
                let value = self.pop();
                let home = plan.home(slot);
                if value != home {
                    self.preserve(a, home, &[value]);
                    a.fmov_d_d(home, value);
                }
            }
            Step::Arithmetic(op) => {
                let rhs = self.pop();
                let lhs = self.pop();
                let result = self.destination(a, plan, next, &[lhs, rhs]);
                a.f_arith(op, result, lhs, rhs);
                self.stack.push(result);
            }
            Step::Negate => {
                let value = self.pop();
                let result = self.destination(a, plan, next, &[value]);
                a.fneg(result, value);
                self.stack.push(result);
            }
            Step::Duplicate => self.stack.push(*self.stack.last().unwrap()),
            Step::Pop => {
                self.pop();
            }
            Step::Update(slot, kind) => self.update(a, plan.home(slot), kind),
            Step::GetElem { .. } | Step::Compare { .. } | Step::Jump(_) => unreachable!(),
        }
    }

    fn update(&mut self, a: &mut Asm, home: u32, kind: UpdKind) {
        if matches!(kind, UpdKind::PostInc | UpdKind::PostDec) {
            self.stack.push(home);
        }
        self.preserve(a, home, &[]);
        a.fmov_one(0);
        let sub = matches!(
            kind,
            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard
        );
        a.f_arith(sub as u32, home, home, 0);
        if matches!(kind, UpdKind::PreInc | UpdKind::PreDec) {
            self.stack.push(home);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, bails: bool) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::super::ENTRIES.with(|n| n.set(0));
            super::super::BAILS.with(|n| n.set(0));
            let source = format!("function assert(v){{if(!v)throw new Error('numeric register ownership');}} {source}; 'passed'");
            match engine.eval(&source, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if matches!(tier, Tier::Jit) {
                assert!(super::super::ENTRIES.with(|n| n.get() > 0));
                assert_eq!(super::super::BAILS.with(|n| n.get() > 0), bails);
            }
        }
    }

    #[test]
    fn assignments_and_updates_preserve_live_old_values() {
        check(
            r#"
            function aliases(n) {
                var sum=0,x=1;
                for(var i=0;i<n;i++) {
                    if(i<2)sum+=x+(x=3);else sum+=x+(x++);
                }
                return sum*100+x;
            }
            assert(aliases(4)===2405);
            function duplicates(n) {
                var x=1,y=0,sum=0;
                for(var i=0;i<n;i++) {
                    if(i<2)sum+=x+(y=x+x)+(x=y);else sum+=x+(--x)+(x--);
                }
                return sum*100+x*10+y;
            }
            assert(duplicates(4)===2904);
        "#,
            false,
        );
    }

    #[test]
    fn coalesced_reads_restore_borrowed_operands_on_guard_failure() {
        check(
            r#"
            function scan(a,n) {
                var sum=0,x=1;
                for(var i=0;i<n;i++) {
                    if(i<10)sum+=x+(x=a[i]);else sum--;
                }
                return sum*100+x;
            }
            var a=[2,3],calls=0,proto=Object.create(Array.prototype);
            Object.defineProperty(proto,'2',{get(){calls++;return 4;}});
            Object.setPrototypeOf(a,proto);
            assert(scan(a,3)===1504 && calls===1);
            function index(a,n) {
                var sum=0,x=0;
                for(var i=0;i<n;i++) {
                    if(i<10)sum+=(++x)*a[i]+x++;else sum--;
                }
                return sum*100+x;
            }
            assert(index(a,3)===4006 && calls===2);
            function coalesced(a,n) {
                var x=1,sum=0;
                for(var i=0;i<n;i++) {if(i<10){x=a[i];sum+=x;}else sum--;}
                return sum*100+x;
            }
            assert(coalesced(a,3)===904 && calls===3);
        "#,
            true,
        );
    }
}
