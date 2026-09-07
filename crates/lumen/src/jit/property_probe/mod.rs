//! Borrow an own data-property entry through live ordinary/array cache ways.
mod key;
use crate::bytecode::{
    IcState, IC_ARR_KEYCHK, IC_OFF_DEPTH, IC_OFF_RECV_SHAPE, IC_OFF_SLOT, PROP_IC_WAYS,
};
use crate::jit::{asm::Asm, C_EQ, C_HS, C_NE};
use crate::value::JitLayout;

/// Input x11 is a rooted borrowed stored Rc; output x15 addresses a data-property entry.
/// Clobbers x8..x17 only. No helpers, writes, owner changes, or prototype traversal.
pub(super) fn own_entry(a: &mut Asm, layout: &JitLayout, cache: usize, name: &str, fail: usize) {
    own_entry_with_hint(a, layout, cache, name, None, fail);
}

/// A warmed own state removes cache-cell loads; misses still probe all live ways.
pub(super) fn own_entry_with_hint(
    a: &mut Asm,
    layout: &JitLayout,
    cache: usize,
    name: &str,
    preferred: Option<IcState>,
    fail: usize,
) {
    receiver(a, layout, fail);
    let done = a.new_label();
    if let Some(state) = preferred.filter(|s| matches!(s.depth, 0 | IC_ARR_KEYCHK)) {
        let live = a.new_label();
        a.cmp_imm_w(8, state.depth as u32);
        a.b_cond(C_NE, live);
        a.mov_imm64(9, state.recv_shape as u64);
        a.cmp_reg_w(9, 10);
        a.b_cond(C_NE, live);
        a.mov_imm64(13, state.slot as u64);
        entry(a, layout, name, live);
        #[cfg(test)]
        record_hint(a);
        a.b(done);
        a.bind(live);
    }
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
        entry(a, layout, name, miss);
        a.b(done);
        a.bind(miss);
    }
    a.b(fail);
    a.bind(done);
}

fn receiver(a: &mut Asm, layout: &JitLayout, fail: usize) {
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

fn entry(a: &mut Asm, layout: &JitLayout, name: &str, fail: usize) {
    a.ldr_imm(
        16,
        11,
        (layout.obj_props + layout.props_entries + layout.vec_len_off) as u32,
    );
    a.cmp_reg_x(13, 16);
    a.b_cond(C_HS, fail);
    a.ldr_imm(
        15,
        11,
        (layout.obj_props + layout.props_entries + layout.vec_ptr_off) as u32,
    );
    a.mov_imm64(16, layout.entry_size as u64);
    a.madd(15, 13, 16, 15);
    let ordinary = a.new_label();
    a.cbz(8, false, ordinary);
    key::emit(a, layout, name, fail);
    a.bind(ordinary);
    crate::jit::guard_prop_data(a, 9, 15, layout.entry_accessor as u32, fail);
}

#[cfg(test)]
thread_local! {
    pub(super) static HINT_SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_hint(a: &mut Asm) {
    // Preserve the x15 entry result. These scratch registers are already probe-clobbered.
    a.mov_imm64(9, HINT_SUCCESSES.with(|n| n.as_ptr() as usize) as u64);
    a.ldr_imm(12, 9, 0);
    a.add_imm(12, 12, 1);
    a.str_imm(12, 9, 0);
}
