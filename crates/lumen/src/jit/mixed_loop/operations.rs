//! Helper-free operations over shadow locals and operands.
use super::{numeric, plan::Plan, read_proofs, shadow};
use crate::{
    bytecode::{Chunk, Op},
    jit::{
        asm::Asm,
        guarded_write_region::{name, store},
        C_EQ,
    },
    value::JitLayout,
};

pub(super) fn can_fail(op: Op) -> bool {
    !matches!(
        op,
        Op::Const(_)
            | Op::StoreLocal(_)
            | Op::Undef
            | Op::Pop
            | Op::Dup
            | Op::Dup2
            | Op::Jump(_)
            | Op::InlineGuard(..)
    )
}

pub(super) fn emit(
    a: &mut Asm,
    chunk: &Chunk,
    plan: &Plan,
    layout: &JitLayout,
    (pc, op): (usize, Op),
    depth: usize,
    fail: usize,
) {
    let top = plan.stack(depth);
    match op {
        Op::Const(k) => {
            a.mov_imm64(9, chunk.jit_const_num(k).expect("numeric constant"));
            a.fmov_d_x(16, 9);
            shadow::write_number(a, top, 16);
        }
        Op::Undef => {
            a.str_imm(31, 23, top);
            a.str_imm(31, 23, top + 8);
        }
        Op::LoadLocal(slot) => {
            let at = u32::from(slot) * 16;
            a.ldrb_imm(9, 23, at);
            a.cmp_imm_w(9, 1);
            a.b_cond(C_EQ, fail);
            shadow::copy(a, 23, at, top);
        }
        Op::StoreLocal(slot) => shadow::copy(a, 23, top - 16, u32::from(slot) * 16),
        Op::LoadThis => {
            a.ldr_imm(14, 19, 48);
            shadow::copy(a, 14, 0, top);
        }
        Op::LoadName(..) => load_name(a, chunk, layout, op, top, fail),
        Op::Pop => {}
        Op::Dup => shadow::copy(a, 23, top - 16, top),
        Op::Dup2 => {
            shadow::copy(a, 23, top - 32, top);
            shadow::copy(a, 23, top - 16, top + 16);
        }
        Op::GetProp(..)
        | Op::GetPropLocal(..)
        | Op::GetPropThis(..)
        | Op::GetMethod(..)
        | Op::GetElem
        | Op::GetElemLocal(_) => read(a, chunk, plan, layout, (pc, op), depth, fail),
        Op::SetProp(..)
        | Op::SetPropDrop(..)
        | Op::SetPropThisDrop(..)
        | Op::SetPropLocalDrop(..) => {
            write(a, chunk, plan, layout, op, depth, fail);
        }
        _ => numeric::emit(a, plan, op, depth, fail),
    }
}

fn load_name(a: &mut Asm, chunk: &Chunk, layout: &JitLayout, op: Op, top: u32, fail: usize) {
    let object = a.new_label();
    let done = a.new_label();
    assert!(name::emit(
        a,
        chunk,
        layout,
        op,
        name::Target::Number(16),
        object
    ));
    shadow::write_number(a, top, 16);
    a.b(done);
    a.bind(object);
    assert!(name::emit(
        a,
        chunk,
        layout,
        op,
        name::Target::Object(0),
        fail
    ));
    shadow::write_object(a, top);
    a.bind(done);
}

fn read(
    a: &mut Asm,
    chunk: &Chunk,
    plan: &Plan,
    layout: &JitLayout,
    (pc, op): (usize, Op),
    depth: usize,
    fail: usize,
) {
    let top = plan.stack(depth);
    let out = match op {
        Op::GetPropLocal(slot, ..) | Op::GetElemLocal(slot) => {
            shadow::object(a, 23, u32::from(slot) * 16, fail);
            if matches!(op, Op::GetElemLocal(_)) {
                shadow::number(a, top - 16, 16, fail);
                top - 16
            } else {
                top
            }
        }
        Op::GetPropThis(..) => {
            a.ldr_imm(14, 19, 48);
            shadow::object(a, 14, 0, fail);
            top
        }
        Op::GetElem => {
            shadow::object(a, 23, top - 32, fail);
            shadow::number(a, top - 16, 16, fail);
            top - 32
        }
        Op::GetProp(..) => {
            shadow::object(a, 23, top - 16, fail);
            top - 16
        }
        Op::GetMethod(..) => {
            shadow::object(a, 23, top - 16, fail);
            top
        }
        _ => unreachable!("read opcode"),
    };
    read_proofs::emit(a, layout, chunk, plan, (pc, op), out, fail);
}

fn write(
    a: &mut Asm,
    chunk: &Chunk,
    plan: &Plan,
    layout: &JitLayout,
    op: Op,
    depth: usize,
    fail: usize,
) {
    let top = plan.stack(depth);
    let (name, cache) = match op {
        Op::SetPropThisDrop(n, c) => {
            a.ldr_imm(14, 19, 48);
            shadow::object(a, 14, 0, fail);
            (n, c)
        }
        Op::SetPropLocalDrop(slot, n, c) => {
            shadow::object(a, 23, u32::from(slot) * 16, fail);
            (n, c)
        }
        Op::SetProp(n, c) | Op::SetPropDrop(n, c) => {
            shadow::object(a, 23, top - 32, fail);
            (n, c)
        }
        _ => unreachable!("write opcode"),
    };
    shadow::number(a, top - 16, 16, fail);
    store::emit(a, layout, chunk, 0, 16, name, cache, fail);
    if matches!(op, Op::SetProp(..)) {
        shadow::copy(a, 23, top - 16, top - 32);
    }
    #[cfg(test)]
    {
        a.movz(9, 1, 0);
        a.str_imm(9, 23, plan.control());
    }
}
