//! Validate array entry names because array shapes do not fix named slot positions.
use crate::jit::{asm::Asm, C_NE};
use crate::value::JitLayout;

pub(super) fn emit(a: &mut Asm, layout: &JitLayout, name: &str, fail: usize) {
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
