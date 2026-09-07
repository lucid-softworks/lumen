//! Read a present own data Object from classic or packed dense storage.
use crate::jit::{asm::Asm, C_EQ, C_HS, C_NE};
use crate::value::JitLayout;

pub(super) fn supported(layout: &JitLayout) -> bool {
    crate::jit::get_elem_inlinable(layout) && crate::jit::packed_elem_inlinable(layout)
}

/// x0 receiver, d16 exact numeric index -> packed element x13. No owner changes.
pub(super) fn emit(a: &mut Asm, layout: &JitLayout, fail: usize) {
    a.fcvtzu_w_d(9, 16);
    a.ucvtf_d_w(17, 9);
    a.fcmp(16, 17);
    a.b_cond(C_NE, fail);
    a.add_imm(11, 0, layout.obj_from_rc as u32);
    a.ldrb_imm(14, 11, layout.obj_exotic as u32);
    let ordinary = a.new_label();
    a.cmp_imm_w(14, layout.exotic_none_tag as u32);
    a.b_cond(C_EQ, ordinary);
    a.cmp_imm_w(14, layout.exotic_array_tag as u32);
    a.b_cond(C_NE, fail);
    a.bind(ordinary);
    a.ldrb_imm(14, 11, layout.obj_ic_plain as u32);
    a.cbz(14, false, fail);
    a.ldr_imm(12, 11, (layout.obj_props + layout.props_elems) as u32);
    a.cbz(12, true, fail);
    let classic = a.new_label();
    let done = a.new_label();
    a.ldr_imm(15, 12, layout.dense_packed as u32);
    a.cbz(15, true, classic);
    a.ldr_imm(14, 15, layout.vec_len_off as u32);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_HS, fail);
    a.ldr_imm(15, 15, layout.vec_ptr_off as u32);
    a.add_shifted(15, 15, 9, 4);
    crate::jit::guard_prop_data(a, 14, 15, layout.property_meta as u32, fail);
    a.ldur(13, 15, layout.property_value as i32);
    a.b(done); // The Object tag check rejects Empty holes and every other value.
    a.bind(classic);
    a.ldr_imm(14, 12, (layout.dense_elems + layout.vec_len_off) as u32);
    a.cmp_reg_x(9, 14);
    a.b_cond(C_HS, fail);
    a.ldr_imm(12, 12, (layout.dense_elems + layout.vec_ptr_off) as u32);
    a.add_shifted(12, 12, 9, 2);
    a.ldr_w_imm(13, 12, 0);
    a.cmn_imm_w(13, 1);
    a.b_cond(C_EQ, fail);
    a.ldr_imm(
        14,
        11,
        (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32,
    );
    a.cmp_reg_x(13, 14);
    a.b_cond(C_HS, fail);
    a.ldr_imm(
        15,
        11,
        (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32,
    );
    a.mov_imm64(16, layout.entry_size as u64);
    a.madd(15, 13, 16, 15);
    crate::jit::guard_prop_data(a, 9, 15, layout.entry_accessor as u32, fail);
    a.ldur(13, 15, layout.entry_value as i32);
    a.bind(done);
}
