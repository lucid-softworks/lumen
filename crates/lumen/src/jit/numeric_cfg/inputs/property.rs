//! Own numeric property inputs use live cache ways, including key-checked array slots.
use crate::bytecode::{
    IcState, IC_ARR_KEYCHK, IC_OFF_DEPTH, IC_OFF_RECV_SHAPE, IC_OFF_SLOT, PROP_IC_WAYS,
};
use crate::jit::{asm::Asm, C_EQ, C_HS, C_NE};
use crate::value::JitLayout;

pub(super) fn emit(
    a: &mut Asm,
    layout: &JitLayout,
    slot: Option<u16>,
    cache: usize,
    name: &str,
    out: u32,
    fail: usize,
) {
    receiver(a, layout, slot, fail);
    let done = a.new_label();
    for way in 0..PROP_IC_WAYS {
        let miss = a.new_label();
        a.mov_imm64(12, (cache + way * std::mem::size_of::<IcState>()) as u64);
        a.ldrb_imm(9, 12, IC_OFF_DEPTH);
        a.cmp_reg_w(9, 8); // Only the live receiver's own-property cache mode is admitted.
        a.b_cond(C_NE, miss);
        a.ldr_w_imm(9, 12, IC_OFF_RECV_SHAPE);
        a.cmp_reg_w(9, 10);
        a.b_cond(C_NE, miss);
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
        let ordinary = a.new_label();
        a.cbz(8, false, ordinary);
        key(a, layout, name, miss);
        a.bind(ordinary);
        crate::jit::guard_prop_data(a, 9, 15, layout.entry_accessor as u32, miss);
        if layout.entry_accessor == layout.entry_value + 8 {
            crate::jit::emit_region_packed_number(a, 15, layout.entry_value as i32, out, miss);
        } else {
            a.ldrb_imm(9, 15, layout.entry_value as u32);
            a.cmp_imm_w(9, 4);
            a.b_cond(C_NE, miss);
            a.ldur_d(out, 15, layout.entry_value as i32 + 8);
        }
        a.b(done);
        a.bind(miss);
    }
    a.b(fail);
    a.bind(done);
}

fn receiver(a: &mut Asm, layout: &JitLayout, slot: Option<u16>, fail: usize) {
    if let Some(slot) = slot {
        a.add_imm(14, 22, slot as u32 * 16);
    } else {
        a.ldr_imm(14, 19, 48); // ctx.this_raw
    }
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(11, 14, 8);
    a.add_imm(11, 11, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 11, layout.obj_exotic as u32);
    let ordinary = a.new_label();
    let ready = a.new_label();
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_EQ, ordinary);
    a.cmp_imm_w(9, layout.exotic_array_tag as u32);
    a.b_cond(C_NE, fail);
    a.movz(8, IC_ARR_KEYCHK as u32, 0);
    a.b(ready);
    a.bind(ordinary);
    a.movz(8, 0, 0);
    a.bind(ready);
    a.ldrb_imm(9, 11, layout.obj_ic_plain as u32);
    a.cbz(9, false, fail);
    a.ldr_w_imm(10, 11, (layout.obj_props + layout.props_shape) as u32);
}

fn key(a: &mut Asm, layout: &JitLayout, name: &str, fail: usize) {
    if !layout.key_probe_ok
        || name.len() > 8
        || layout.entry_key + layout.str_len_word >= 256
        || layout.entry_key + layout.str_ptr_word >= 256
        || layout.str_data_off + name.len() >= 4096
    {
        a.b(fail);
        return;
    }
    a.ldur(16, 15, (layout.entry_key + layout.str_len_word) as i32);
    a.cmp_imm_x(16, name.len() as u32);
    a.b_cond(C_NE, fail);
    a.ldur(16, 15, (layout.entry_key + layout.str_ptr_word) as i32);
    for (index, &byte) in name.as_bytes().iter().enumerate() {
        a.ldrb_imm(17, 16, (layout.str_data_off + index) as u32);
        a.cmp_imm_w(17, byte as u32);
        a.b_cond(C_NE, fail);
    }
}
