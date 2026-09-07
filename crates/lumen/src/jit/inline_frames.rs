//! Record virtual call frames before a native direct call bypasses the checked helpers.
use super::{asm::Asm, JitCtx};
use crate::interpreter::{InlineFrame, InterpLayout};

pub(super) fn record(a: &mut Asm, layout: &InterpLayout, state: *const InlineFrame) {
    // Direct calls themselves are gated by these probed layout requirements. When they are
    // unavailable the call helper records the location through the ordinary Rust path.
    if !layout.valid
        || [layout.fnf_ptr_word, layout.fnf_len_word]
            .into_iter()
            .any(|word| {
                !(layout.fn_frames + word).is_multiple_of(8)
                    || (layout.fn_frames + word) / 8 >= 4096
            })
    {
        return;
    }
    let done = a.new_label();
    a.ldr_imm(9, 19, std::mem::offset_of!(JitCtx, interp) as u32);
    a.ldr_imm(10, 9, (layout.fn_frames + layout.fnf_len_word) as u32);
    a.cbz(10, true, done);
    a.sub_imm(10, 10, 1);
    a.ldr_imm(11, 9, (layout.fn_frames + layout.fnf_ptr_word) as u32);
    a.add_shifted(11, 11, 10, 5); // FnFrame is 32 bytes, asserted beside the native call ABI.
    a.mov_imm64(12, state as usize as u64);
    a.str_imm(12, 11, 24);
    a.bind(done);
}
