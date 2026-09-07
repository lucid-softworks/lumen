//! Borrow property/method/element results into a wide shadow frame.
mod element;
mod method;
use crate::bytecode::{Chunk, Op};
use crate::jit::{asm::Asm, property_probe, C_EQ, C_LS, C_NE};
use crate::value::{JitLayout, PACK_OBJ, PACK_SYM, PACK_UNDEFINED};

pub(super) fn supported(layout: &JitLayout, op: Op) -> bool {
    layout.entry_accessor == layout.entry_value + 8
        && crate::jit::get_prop_inlinable(layout)
        && match op {
            Op::GetProp(..) | Op::GetPropThis(..) | Op::GetPropLocal(..) | Op::GetMethod(..) => {
                true
            }
            Op::GetElem | Op::GetElemLocal(_) => element::supported(layout),
            _ => false,
        }
}

/// x0 is a rooted borrowed stored Rc; GetElem additionally reads d16. Writes only
/// the requested wide shadow Value at x23+outoff after all guards succeed. Source
/// roots must survive unchanged until a precise exit publishes shadow owners.
/// Clobbers x7..x17 and d17; preserves x0..x6, x18+, and d16. Output permits only
/// Number/Object for own properties and only Object for methods/dense elements.
/// False means unsupported layout/op/output offset; no instructions emitted.
pub(super) fn emit_read(
    a: &mut Asm,
    layout: &JitLayout,
    chunk: &Chunk,
    op: Op,
    outoff: u32,
    fail: usize,
) -> bool {
    if !supported(layout, op) || !outoff.is_multiple_of(8) || outoff > 32752 {
        return false;
    }
    let object_only = match op {
        Op::GetProp(name, cache)
        | Op::GetPropThis(name, cache)
        | Op::GetPropLocal(_, name, cache) => {
            a.mov(11, 0);
            property_probe::own_entry_with_hint(
                a,
                layout,
                chunk.jit_cache_ptr(cache),
                chunk.jit_name(name),
                chunk.jit_cache_preferred(cache),
                fail,
            );
            a.ldur(13, 15, layout.entry_value as i32);
            false
        }
        Op::GetMethod(_, cache) => {
            method::emit(a, layout, chunk.jit_cache_ptr(cache), fail);
            true
        }
        Op::GetElem | Op::GetElemLocal(_) => {
            element::emit(a, layout, fail);
            true
        }
        _ => unreachable!("preflighted read"),
    };
    decode(a, object_only, fail);
    a.str_imm(12, 23, outoff);
    a.str_imm(13, 23, outoff + 8);
    true
}

/// Packed input x13 becomes wide tag x12/payload x13 without cloning.
fn decode(a: &mut Asm, object_only: bool, fail: usize) {
    let object = a.new_label();
    let done = a.new_label();
    a.lsr_imm(9, 13, 48);
    a.movz(16, (PACK_OBJ >> 48) as u32, 0);
    a.cmp_reg_x(9, 16);
    if object_only {
        a.b_cond(C_NE, fail);
    } else {
        a.b_cond(C_EQ, object);
        let number = a.new_label();
        a.movz(16, (PACK_UNDEFINED >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(crate::jit::C_LO, number);
        a.movz(16, (PACK_SYM >> 48) as u32, 0);
        a.cmp_reg_x(9, 16);
        a.b_cond(C_LS, fail);
        a.bind(number);
        a.movz(12, 4, 0);
        a.b(done);
    }
    a.bind(object);
    a.movz(12, 8, 0);
    a.lsl_imm(13, 13, 16);
    a.lsr_imm(13, 13, 16);
    a.bind(done);
}
