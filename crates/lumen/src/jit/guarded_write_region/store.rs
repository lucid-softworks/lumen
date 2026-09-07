//! Commit one existing ordinary named numeric field after all fallible checks.
use crate::bytecode::Chunk;
use crate::jit::{asm::Asm, property_probe, C_NE, C_VS};
use crate::value::{canonical_index, JitLayout, PACK_CANON_NAN};

/// Reject unsupported layouts and element-key writes before any region code is emitted.
pub(in crate::jit) fn supported(layout: &JitLayout, name: &str) -> bool {
    crate::jit::get_prop_inlinable(layout)
        && layout.entry_accessor == layout.entry_value + 8
        && layout.entry_writable < 4096
        && canonical_index(name).is_none()
}

/// The caller supplies a rooted borrowed receiver in x0..x7 and a number in d16..d31.
/// Clobbers x8..x17, d0 and flags only. Failure precedes every write/owner change.
/// Success performs one packed Number-to-Number store. Any subsequent guard exit must
/// publish state at its exact original PC; it must not replay this committed write.
pub(in crate::jit) fn emit(
    a: &mut Asm,
    layout: &JitLayout,
    chunk: &Chunk,
    receiver_reg: u32,
    number_reg: u32,
    name: u32,
    cache: u32,
    fail: usize,
) {
    debug_assert!(receiver_reg < 8);
    debug_assert!((16..32).contains(&number_reg));
    let name = chunk.jit_name(name);
    // Ordinary objects may have dense numeric mirrors too; preflight excludes index keys.
    debug_assert!(supported(layout, name));
    a.add_imm(11, receiver_reg, layout.obj_from_rc as u32);
    a.ldrb_imm(9, 11, layout.obj_exotic as u32);
    a.cmp_imm_w(9, layout.exotic_none_tag as u32);
    a.b_cond(C_NE, fail);
    a.mov(11, receiver_reg);
    property_probe::own_entry_with_hint(
        a,
        layout,
        chunk.jit_cache_ptr(cache),
        name,
        chunk.jit_cache_preferred(cache),
        fail,
    );
    crate::jit::guard_prop_writable(a, 9, 15, layout.entry_writable as u32, fail);
    // Replacing an old Number cannot release an object edge or call a destructor.
    crate::jit::emit_region_packed_number(a, 15, layout.entry_value as i32, 0, fail);
    a.fmov_x_d(16, number_reg);
    a.fcmp(number_reg, number_reg);
    let encoded = a.new_label();
    a.b_cond(C_VS ^ 1, encoded); // ordered: input is not NaN
    a.mov_imm64(16, PACK_CANON_NAN);
    a.bind(encoded);
    a.stur(16, 15, layout.entry_value as i32);
}
