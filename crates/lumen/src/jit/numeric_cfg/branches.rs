//! Lay out numeric CFG edges, retaining bounded continuations on every backedge.
use super::{emit::flush, plan::Plan};
use crate::jit::asm::Asm;

pub(super) struct Branches<'a> {
    plan: &'a Plan,
    pc_labels: &'a [usize],
    plain: usize,
    labels: Vec<(usize, usize)>,
    exits: Vec<(usize, usize)>,
    fallthrough: bool,
}

impl<'a> Branches<'a> {
    pub(super) fn new(a: &mut Asm, plan: &'a Plan, pc_labels: &'a [usize], plain: usize) -> Self {
        Self {
            plan,
            pc_labels,
            plain,
            labels: plan
                .blocks
                .iter()
                .map(|b| (b.start, a.new_label()))
                .collect(),
            exits: plan.exits.iter().map(|&pc| (pc, a.new_label())).collect(),
            fallthrough: std::env::var_os("LUMEN_JIT_NO_CFG_FALLTHROUGH").is_none(),
        }
    }

    pub(super) fn label(&self, pc: usize) -> usize {
        self.labels
            .iter()
            .chain(&self.exits)
            .find(|(p, _)| *p == pc)
            .unwrap()
            .1
    }

    pub(super) fn jump(&self, a: &mut Asm, source: usize, target: usize, next: Option<usize>) {
        if target <= source {
            let keep = a.new_label();
            a.sub_imm(17, 17, 1);
            a.cbnz(17, false, keep);
            flush(a, self.plan);
            a.b(if target == self.plan.head {
                self.plain
            } else {
                self.pc_labels[target]
            });
            a.bind(keep);
        }
        if !self.fallthrough || next != Some(target) {
            a.b(self.label(target));
        }
    }

    pub(super) fn compare(
        &self,
        a: &mut Asm,
        source: usize,
        condition: u32,
        yes: usize,
        no: usize,
        next: Option<usize>,
    ) {
        a.fcmp(24, 25);
        if self.fallthrough && yes > source && no > source {
            // Forward edges cannot require a continuation poll. Branch directly to the
            // non-adjacent successor, leaving the physically next block as fallthrough.
            if next == Some(no) {
                a.b_cond(condition ^ 1, self.label(yes));
            } else {
                a.b_cond(condition, self.label(no));
                if next != Some(yes) {
                    a.b(self.label(yes));
                }
            }
        } else {
            let false_edge = a.new_label();
            a.b_cond(condition, false_edge);
            self.jump(a, source, yes, None);
            a.bind(false_edge);
            self.jump(a, source, no, next);
        }
    }

    pub(super) fn exits(&self, a: &mut Asm) {
        for &(pc, label) in &self.exits {
            a.bind(label);
            flush(a, self.plan);
            a.b(self.pc_labels[pc]);
        }
    }
}
