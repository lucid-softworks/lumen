//! Read-only branch evaluation followed by one numeric field commit.
use super::{
    name,
    plan::Node,
    store,
    values::{Expression, Rep, Source},
};
use crate::bytecode::{Chunk, Op};
use crate::jit::{asm::Asm, property_probe, C_EQ, C_GE, C_GT, C_LS, C_MI, C_NE};
use crate::value::{JitLayout, PACK_OBJ};

pub(super) fn supported(node: &Node, chunk: &Chunk, layout: &JitLayout) -> bool {
    let (values, children_ok) = match node {
        Node::Branch {
            values, yes, no, ..
        } => (
            values,
            supported(yes, chunk, layout) && supported(no, chunk, layout),
        ),
        Node::Store { values, name, .. } => {
            (values, store::supported(layout, chunk.jit_name(*name)))
        }
    };
    children_ok
        && values.values.iter().all(|v| match v.source {
            Source::Name(n, c) => name::supported(
                layout,
                Op::LoadName(n, c),
                match v.rep {
                    Some(Rep::Object) => name::Target::Object(v.register),
                    _ => name::Target::Number(v.register),
                },
            ),
            _ => true,
        })
}

pub(super) fn emit(
    a: &mut Asm,
    node: &Node,
    chunk: &Chunk,
    layout: &JitLayout,
    fail: usize,
    join: usize,
    _prefix_depth: usize,
) {
    match node {
        Node::Branch {
            values,
            comparison,
            yes,
            no,
        } => {
            expression(a, values, chunk, layout, fail);
            a.fcmp(
                values.values[values.outputs[0]].register,
                values.values[values.outputs[1]].register,
            );
            let take_yes = a.new_label();
            // MI/LS exclude unordered values for < and <=; NE includes NaN for !=.
            let condition = match comparison {
                Op::EqEq | Op::StrictEq => C_EQ,
                Op::NotEq | Op::StrictNotEq => C_NE,
                Op::Lt => C_MI,
                Op::Le => C_LS,
                Op::Gt => C_GT,
                Op::Ge => C_GE,
                _ => unreachable!("planned numeric comparison"),
            };
            a.b_cond(condition, take_yes);
            emit(a, no, chunk, layout, fail, join, _prefix_depth);
            a.bind(take_yes);
            emit(a, yes, chunk, layout, fail, join, _prefix_depth);
        }
        Node::Store {
            values,
            name,
            cache,
            ..
        } => {
            expression(a, values, chunk, layout, fail);
            store::emit(
                a,
                layout,
                chunk,
                values.values[values.outputs[0]].register,
                values.values[values.outputs[1]].register,
                *name,
                *cache,
                fail,
            );
            #[cfg(test)]
            super::record_success(a, _prefix_depth);
            a.b(join);
        }
    }
}

fn expression(
    a: &mut Asm,
    expression: &Expression,
    chunk: &Chunk,
    layout: &JitLayout,
    fail: usize,
) {
    for at in 0..expression.values.len() {
        value(a, expression, at, chunk, layout, fail);
    }
}

pub(super) fn value(
    a: &mut Asm,
    expression: &Expression,
    at: usize,
    chunk: &Chunk,
    layout: &JitLayout,
    fail: usize,
) {
    let value = &expression.values[at];
    let rep = value.rep.expect("fully constrained region");
    let register = value.register;
    match value.source {
        Source::Local(slot) => {
            a.add_imm(14, 22, slot as u32 * 16);
            root(a, rep, register, fail);
        }
        Source::This => {
            a.ldr_imm(14, 19, 48);
            root(a, rep, register, fail);
        }
        Source::Name(n, c) => {
            let target = match rep {
                Rep::Object => name::Target::Object(register),
                Rep::Number => name::Target::Number(register),
            };
            assert!(name::emit(
                a,
                chunk,
                layout,
                Op::LoadName(n, c),
                target,
                fail
            ));
        }
        Source::Property {
            receiver,
            name,
            cache,
        } => {
            a.mov(11, expression.values[receiver].register);
            property_probe::own_entry_with_hint(
                a,
                layout,
                chunk.jit_cache_ptr(cache),
                chunk.jit_name(name),
                chunk.jit_cache_preferred(cache),
                fail,
            );
            property(a, layout, rep, register, fail);
        }
        Source::Constant(bits) => {
            a.mov_imm64(9, bits);
            a.fmov_d_x(register, 9);
        }
        Source::Arithmetic {
            left,
            right,
            operation,
        } => a.f_arith(
            operation,
            register,
            expression.values[left].register,
            expression.values[right].register,
        ),
        Source::Negate(input) => a.fneg(register, expression.values[input].register),
    }
}

fn root(a: &mut Asm, rep: Rep, register: u32, fail: usize) {
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, if rep == Rep::Object { 8 } else { 4 });
    a.b_cond(C_NE, fail);
    match rep {
        Rep::Object => a.ldr_imm(register, 14, 8),
        Rep::Number => a.ldr_d_imm(register, 14, 8),
    }
}

fn property(a: &mut Asm, layout: &JitLayout, rep: Rep, register: u32, fail: usize) {
    match rep {
        Rep::Object => {
            a.ldur(13, 15, layout.entry_value as i32);
            a.lsr_imm(9, 13, 48);
            a.movz(16, (PACK_OBJ >> 48) as u32, 0);
            a.cmp_reg_x(9, 16);
            a.b_cond(C_NE, fail);
            a.lsl_imm(register, 13, 16);
            a.lsr_imm(register, register, 16);
        }
        Rep::Number => {
            crate::jit::emit_region_packed_number(a, 15, layout.entry_value as i32, register, fail)
        }
    }
}
