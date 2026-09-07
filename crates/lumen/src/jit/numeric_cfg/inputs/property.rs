//! Numeric inputs reuse the shared own-entry probe without cloning property values.
use crate::jit::{asm::Asm, property_probe, C_NE};
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
    if let Some(slot) = slot {
        a.add_imm(14, 22, slot as u32 * 16);
    } else {
        a.ldr_imm(14, 19, 48);
    }
    a.ldrb_imm(9, 14, 0);
    a.cmp_imm_w(9, 8);
    a.b_cond(C_NE, fail);
    a.ldr_imm(11, 14, 8);
    property_probe::own_entry(a, layout, cache, name, fail);
    if layout.entry_accessor == layout.entry_value + 8 {
        crate::jit::emit_region_packed_number(a, 15, layout.entry_value as i32, out, fail);
    } else {
        a.ldrb_imm(9, 15, layout.entry_value as u32);
        a.cmp_imm_w(9, 4);
        a.b_cond(C_NE, fail);
        a.ldur_d(out, 15, layout.entry_value as i32 + 8);
    }
}
