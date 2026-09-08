//! Borrow a present own Array element, preserving the deferred-store disjointness proof.
use crate::jit::{asm::Asm, C_EQ, C_HS, C_MI, C_NE};
use crate::value::{JitLayout, PACK_EMPTY};

pub(super) fn supported(layout: &JitLayout) -> bool {
    super::names::object_borrow_supported(layout)
        && crate::jit::get_elem_inlinable(layout)
        && crate::jit::packed_elem_inlinable(layout)
        && layout.gc_data_off.is_multiple_of(8)
        && layout.gc_data_off < 32768
}

/// x0 stored receiver Rc, d16 index -> borrowed packed x13. Scratch x8..17/d17.
/// `length_cache` is an owned live own-property IC for "length". The caller must
/// verify the RefCell borrow-counter representation before emitting this probe.
/// No callback, GC, owner acquisition or JS-state write occurs on either edge.
pub(super) fn emit(a: &mut Asm, layout: &JitLayout, length_cache: usize, fail: usize) {
    a.ldr_imm(14, 0, layout.gc_data_off as u32);
    a.cmp_imm_x(14, 0);
    a.b_cond(C_MI, fail);
    a.add_imm(11, 0, layout.obj_from_rc as u32);
    a.ldrb_imm(14, 11, layout.obj_exotic as u32);
    a.cmp_imm_w(14, layout.exotic_array_tag as u32);
    a.b_cond(C_NE, fail); // Ordinary receivers could alias the pending numeric store.
    a.mov(11, 0);
    crate::jit::property_probe::own_entry(a, layout, length_cache, "length", fail);
    a.ldur(13, 15, layout.entry_value as i32);
    a.fmov_d_x(17, 13);
    a.fcmp(16, 17);
    a.b_cond(C_HS, fail); // index >= length or unordered (all packed tags are NaNs).
    a.fcvtzu_w_d(10, 17);
    a.ucvtf_d_w(17, 10);
    a.fmov_x_d(14, 17);
    a.cmp_reg_x(13, 14);
    a.b_cond(C_NE, fail); // Own length must be an exact, nonnegative u32 Number.
    a.fcvtzu_w_d(9, 16);
    a.ucvtf_d_w(17, 9);
    a.fcmp(16, 17);
    a.b_cond(C_NE, fail);
    a.cmn_imm_w(9, 1);
    a.b_cond(C_EQ, fail); // 2^32-1 is not an array index.
    a.add_imm(11, 0, layout.obj_from_rc as u32);
    a.ldr_imm(12, 11, (layout.obj_props + layout.props_elems) as u32);
    a.cbz(12, true, fail);
    let classic = a.new_label();
    let done = a.new_label();
    crate::jit::packed_element::address(
        a,
        layout,
        classic,
        fail,
        crate::jit::packed_element::Site::Local,
    );
    crate::jit::guard_prop_data(a, 14, 15, layout.property_meta as u32, fail);
    a.ldur(13, 15, layout.property_value as i32);
    a.b(done);
    a.bind(classic);
    classic_element(a, layout, fail);
    a.bind(done);
    a.mov_imm64(14, PACK_EMPTY);
    a.cmp_reg_x(13, 14);
    a.b_cond(C_EQ, fail); // A hole must retain the ordinary prototype lookup.
}

fn classic_element(a: &mut Asm, layout: &JitLayout, fail: usize) {
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
}
