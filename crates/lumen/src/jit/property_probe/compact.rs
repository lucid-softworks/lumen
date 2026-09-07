//! A guarded own hint uses its known exotic kind and constant entry offset.
use super::key;
use crate::bytecode::{IcState, IC_ARR_KEYCHK};
use crate::jit::{asm::Asm, C_HS, C_NE};
use crate::value::JitLayout;

/// Input x11 is a stored Rc. Success returns x15 through `done`; failure restores x11
/// for the generic probe. Only x9..x17 are scratch; no values or ownership are changed.
pub(super) fn emit(a: &mut Asm, layout: &JitLayout, state: IcState, name: &str, done: usize) {
    let miss = a.new_label();
    a.add_imm(11, 11, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 11, layout.obj_exotic as u32);
    let exotic = if state.depth == IC_ARR_KEYCHK {
        layout.exotic_array_tag
    } else {
        layout.exotic_none_tag
    };
    a.cmp_imm_w(9, exotic as u32);
    a.b_cond(C_NE, miss);
    a.ldrb_imm(9, 11, layout.obj_ic_plain as u32);
    a.cbz(9, false, miss);
    a.ldr_w_imm(9, 11, (layout.obj_props + layout.props_shape) as u32);
    a.mov_imm64(16, state.recv_shape as u64);
    a.cmp_reg_w(9, 16);
    a.b_cond(C_NE, miss);
    a.ldr_imm(
        16,
        11,
        (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32,
    );
    a.mov_imm64(13, state.slot as u64);
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, miss);
    a.ldr_imm(
        15,
        11,
        (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32,
    );
    // get_prop_inlinable bounds entry_size below 65536; slot is u32, so this fits u64.
    let offset = state.slot as u64 * layout.entry_size as u64;
    if offset != 0 {
        if offset < 4096 {
            a.add_imm(15, 15, offset as u32);
        } else {
            a.mov_imm64(16, offset);
            a.add_shifted(15, 15, 16, 0);
        }
    }
    if state.depth == IC_ARR_KEYCHK {
        key::emit(a, layout, name, miss);
    }
    crate::jit::guard_prop_data(a, 9, 15, layout.entry_accessor as u32, miss);
    #[cfg(test)]
    super::record_hint(a);
    #[cfg(test)]
    if state.depth == IC_ARR_KEYCHK {
        record_array(a);
    }
    a.b(done);
    a.bind(miss);
    a.sub_imm(11, 11, layout.obj_from_rc as u32);
}

#[cfg(test)]
thread_local! {
    pub(super) static ARRAY_SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_array(a: &mut Asm) {
    a.mov_imm64(9, ARRAY_SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
    a.ldr_imm(12, 9, 0);
    a.add_imm(12, 12, 1);
    a.str_imm(12, 9, 0);
}
