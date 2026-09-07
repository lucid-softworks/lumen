//! Live ordinary prototype-chain lookup, retaining all cache guard dependencies.
use crate::bytecode::{
    IcState, IC_OFF_DEPTH, IC_OFF_HOLDER_SHAPE, IC_OFF_MID2_SHAPE, IC_OFF_MID_OK, IC_OFF_MID_SHAPE,
    IC_OFF_RECV_SHAPE, IC_OFF_SLOT, PROP_IC_WAYS,
};
use crate::jit::{asm::Asm, C_EQ, C_HI, C_HS, C_NE};
use crate::value::JitLayout;

/// Root x0 -> packed property value x13. Exotic receivers/prototypes, absent or
/// key-checked cache modes fall back. Cached methods are data, never getters.
pub(super) fn emit(a: &mut Asm, layout: &JitLayout, cache: usize, fail: usize) {
    let found = a.new_label();
    for way in 0..PROP_IC_WAYS {
        let miss = a.new_label();
        a.mov_imm64(12, (cache + way * std::mem::size_of::<IcState>()) as u64);
        a.ldrb_imm(17, 12, IC_OFF_DEPTH);
        a.cmp_imm_w(17, 3);
        a.b_cond(C_HI, miss);
        a.add_imm(11, 0, layout.obj_from_rc as u32);
        shape(a, layout, IC_OFF_RECV_SHAPE, miss);
        let holder = a.new_label();
        a.cbz(17, false, holder);
        for hop in 1..=3 {
            // Every non-holder intermediate has a recorded absence/shape proof.
            let last = a.new_label();
            a.cmp_imm_w(17, hop);
            a.b_cond(C_EQ, last);
            if hop < 3 {
                a.ldrb_imm(14, 12, IC_OFF_MID_OK);
                a.logic_imm_w(
                    0,
                    14,
                    14,
                    crate::jit::asm::logical_imm_w(1 << (hop - 1)).unwrap(),
                );
                a.cbz(14, false, miss);
                advance(a, layout, miss);
                shape(
                    a,
                    layout,
                    if hop == 1 {
                        IC_OFF_MID_SHAPE
                    } else {
                        IC_OFF_MID2_SHAPE
                    },
                    miss,
                );
                let next = a.new_label();
                a.b(next);
                a.bind(last);
                advance(a, layout, miss);
                shape(a, layout, IC_OFF_HOLDER_SHAPE, miss);
                a.b(holder);
                a.bind(next);
            } else {
                a.bind(last);
                advance(a, layout, miss);
                shape(a, layout, IC_OFF_HOLDER_SHAPE, miss);
            }
        }
        a.bind(holder);
        a.ldr_w_imm(13, 12, IC_OFF_SLOT);
        a.ldr_imm(
            16,
            11,
            (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32,
        );
        a.cmp_reg_x(13, 16);
        a.b_cond(C_HS, miss);
        a.ldr_imm(
            15,
            11,
            (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32,
        );
        a.mov_imm64(16, layout.entry_size as u64);
        a.madd(15, 13, 16, 15);
        crate::jit::guard_prop_data(a, 9, 15, layout.entry_accessor as u32, miss);
        a.ldur(13, 15, layout.entry_value as i32);
        a.b(found);
        a.bind(miss);
    }
    a.b(fail);
    a.bind(found);
}

fn advance(a: &mut Asm, layout: &JitLayout, fail: usize) {
    a.ldr_imm(11, 11, layout.obj_proto as u32);
    a.cbz(11, true, fail);
    a.add_imm(11, 11, layout.obj_from_rc as u32);
}

fn shape(a: &mut Asm, layout: &JitLayout, expected: u32, fail: usize) {
    a.ldrb_imm(14, 11, layout.obj_exotic as u32);
    a.cmp_imm_w(14, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.ldrb_imm(14, 11, layout.obj_ic_plain as u32);
    a.cbz(14, false, fail);
    a.ldr_w_imm(14, 11, (layout.obj_props + layout.props_shape) as u32);
    a.ldr_w_imm(16, 12, expected);
    a.cmp_reg_w(14, 16);
    a.b_cond(C_NE, fail);
}
