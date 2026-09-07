//! Borrow a live cached lexical/global Number or Object without changing VM owners.
use crate::bytecode::{Chunk, Op};
use crate::jit::{asm::Asm, C_NE};
use crate::value::{JitLayout, PACK_OBJ};

#[derive(Clone, Copy)]
pub(in crate::jit) enum Target {
    Number(u32),
    Object(u32),
}

/// Preflight the complete plan before emitting any branch or guard labels.
pub(in crate::jit) fn supported(layout: &JitLayout, op: Op, target: Target) -> bool {
    let valid_home = match target {
        Target::Number(register) => (16..32).contains(&register),
        Target::Object(register) => register < 8,
    };
    matches!(op, Op::LoadName(..)) && valid_home && crate::jit::load_name_inlinable(layout)
}

/// Object homes x0..x7 and numeric homes d16..d31 leave the probe scratch free.
/// Other live object homes are preserved, including x7 on all guard failures.
/// Clobbers x8..x17; success additionally defines only the requested destination.
///
/// The active JitCtx must retain its environment/global roots. The surrounding
/// region must not call helpers, collect, mutate bindings, or overwrite object
/// edges while the returned stored Rc is borrowed. TDZ, stale identity/generation,
/// accessor and type failures return to the caller's read-only phase entry.
///
/// Returns false without emitting anything for an unsupported opcode/layout/home.
/// Cache ownership and name association come from this exact chunk's LoadName op;
/// no cached raw pointer is treated as authoritative without the shared guards.
pub(in crate::jit) fn emit(
    a: &mut Asm,
    chunk: &Chunk,
    layout: &JitLayout,
    op: Op,
    target: Target,
    fail: usize,
) -> bool {
    let Op::LoadName(_, cache) = op else {
        return false;
    };
    if !supported(layout, op, target) {
        return false;
    }
    let failed = a.new_label();
    let wide = a.new_label();
    let decoded = a.new_label();
    let done = a.new_label();
    // The shared name probe uses w7 as its packed/wide result flag, but preserves
    // x8. Preserve a possible borrowed owner in x7 even along early probe exits.
    a.mov(8, 7);
    crate::jit::emit_name_ic_value_ptr(a, layout, chunk.jit_name_cache_ptr(cache), failed, true);
    if layout.entry_accessor == layout.entry_value + 8 {
        a.cbz(7, false, wide);
        packed(a, target, failed);
        a.b(decoded);
    }
    a.bind(wide);
    a.ldurb(9, 14, 0);
    a.cmp_imm_w(
        9,
        if matches!(target, Target::Object(_)) {
            8
        } else {
            4
        },
    );
    a.b_cond(C_NE, failed);
    match target {
        Target::Number(register) => a.ldur_d(register, 14, 8),
        Target::Object(register) => a.ldur(register, 14, 8),
    }
    a.bind(decoded);
    if !matches!(target, Target::Object(7)) {
        a.mov(7, 8);
    }
    a.b(done);
    a.bind(failed);
    a.mov(7, 8);
    a.b(fail);
    a.bind(done);
    true
}

fn packed(a: &mut Asm, target: Target, fail: usize) {
    match target {
        Target::Number(register) => {
            crate::jit::emit_region_packed_number(a, 14, 0, register, fail);
        }
        Target::Object(register) => {
            a.ldur(13, 14, 0);
            a.lsr_imm(9, 13, 48);
            a.movz(16, (PACK_OBJ >> 48) as u32, 0);
            a.cmp_reg_x(9, 16);
            a.b_cond(C_NE, fail);
            a.lsl_imm(register, 13, 16);
            a.lsr_imm(register, register, 16);
        }
    }
}
